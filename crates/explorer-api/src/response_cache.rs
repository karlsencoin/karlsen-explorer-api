//! In-memory cache for expensive, slowly changing GET responses (the
//! `/analytics/addresses/*` rich list and distribution endpoints).
//!
//! Those endpoints aggregate the whole `balances` table; depending on database
//! load one call takes from about a second to tens of seconds, which is longer
//! than a browser (or nginx) is willing to wait. The data itself changes slowly,
//! so successful responses are kept for `ttl` per path + query:
//!
//! - concurrent misses for the same key share one computation (single flight);
//! - a background warmer (see [`spawn_warmer`]) recomputes the hot keys before
//!   they expire, so visitors of the common pages never wait for the query;
//! - only `200` responses are stored, errors are never cached;
//! - the number of keys is bounded; the oldest entry is evicted first.
//!
//! The database is not modified in any way.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Router,
};
use tower::ServiceExt;

/// Responses larger than this are passed through without caching.
const MAX_BODY: usize = 32 * 1024 * 1024;

#[derive(Clone)]
struct Entry {
    stored: Instant,
    headers: HeaderMap,
    body: Bytes,
}

/// Request extension set only by the warmer: recompute even when a fresh entry
/// exists. Extensions cannot be set by HTTP clients, so the cache cannot be
/// bypassed from outside.
#[derive(Clone, Copy)]
pub struct ForceRefresh;

pub struct ResponseCache {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<HashMap<String, Entry>>,
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ResponseCache {
    pub fn new(ttl: Duration, capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            ttl,
            capacity,
            entries: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
        })
    }

    fn fresh(&self, key: &str) -> Option<Entry> {
        let entries = self.entries.lock().expect("response cache lock");
        entries
            .get(key)
            .filter(|e| e.stored.elapsed() < self.ttl)
            .cloned()
    }

    fn store(&self, key: String, entry: Entry) {
        let mut entries = self.entries.lock().expect("response cache lock");
        if !entries.contains_key(&key) && entries.len() >= self.capacity {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.stored)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(key, entry);
    }

    fn flight(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut flights = self.flights.lock().expect("response cache lock");
        // Drop finished flights so the map does not grow with every distinct query.
        flights.retain(|_, m| Arc::strong_count(m) > 1);
        flights.entry(key.to_string()).or_default().clone()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().expect("response cache lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn respond(entry: &Entry, status: &'static str) -> Response {
    let mut res = Response::new(Body::from(entry.body.clone()));
    *res.headers_mut() = entry.headers.clone();
    res.headers_mut()
        .insert("x-cache", HeaderValue::from_static(status));
    if let Ok(age) = HeaderValue::from_str(&entry.stored.elapsed().as_secs().to_string()) {
        res.headers_mut().insert("age", age);
    }
    res
}

/// Axum middleware; attach with `route_layer(from_fn_with_state(cache, cached))`.
pub async fn cached(State(cache): State<Arc<ResponseCache>>, req: Request, next: Next) -> Response {
    if req.method() != Method::GET {
        return next.run(req).await;
    }
    let key = req
        .uri()
        .path_and_query()
        .map_or_else(|| req.uri().path().to_string(), |p| p.as_str().to_string());
    let force = req.extensions().get::<ForceRefresh>().is_some();

    if !force {
        if let Some(e) = cache.fresh(&key) {
            return respond(&e, "HIT");
        }
    }

    let flight = cache.flight(&key);
    let _guard = flight.lock().await;
    // Another request may have filled the entry while this one waited.
    if !force {
        if let Some(e) = cache.fresh(&key) {
            return respond(&e, "HIT");
        }
    }

    let res = next.run(req).await;
    if res.status() != StatusCode::OK {
        return res;
    }
    let (parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "response too large to cache",
        )
            .into_response();
    };
    let entry = Entry {
        stored: Instant::now(),
        headers: parts.headers,
        body: bytes,
    };
    cache.store(key, entry.clone());
    respond(&entry, "MISS")
}

/// Recomputes `paths` every `every`, starting immediately, through the full
/// router (so the cached bytes are exactly what clients get). Failures are
/// logged and retried on the next round; the previous entry keeps serving
/// until it expires.
pub fn spawn_warmer(router: Router, paths: Vec<String>, every: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for path in &paths {
                let mut req = Request::builder()
                    .uri(path.as_str())
                    .body(Body::empty())
                    .expect("static warm-up request");
                req.extensions_mut().insert(ForceRefresh);
                let started = Instant::now();
                match router.clone().oneshot(req).await {
                    Ok(res) if res.status() == StatusCode::OK => {
                        tracing::debug!(%path, took = ?started.elapsed(), "cache warmed");
                    }
                    Ok(res) => {
                        tracing::warn!(%path, status = %res.status(), "cache warm-up failed")
                    }
                    Err(e) => tracing::warn!(%path, error = %e, "cache warm-up failed"),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{middleware::from_fn_with_state, routing::get};
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn app(calls: Arc<AtomicUsize>, cache: Arc<ResponseCache>) -> Router {
        let ok = {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    format!("v{n}")
                }
            }
        };
        let fail = {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::SERVICE_UNAVAILABLE, "down")
                }
            }
        };
        Router::new()
            .route("/a", get(ok))
            .route("/fail", get(fail))
            .route_layer(from_fn_with_state(cache, cached))
    }

    async fn get_body(app: &Router, uri: &str) -> (StatusCode, String, Option<String>) {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let x = res
            .headers()
            .get("x-cache")
            .map(|v| v.to_str().unwrap().to_string());
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8(body.to_vec()).unwrap(), x)
    }

    #[tokio::test]
    async fn hit_after_miss_keyed_by_query_and_expires() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = ResponseCache::new(Duration::from_millis(300), 8);
        let app = app(calls.clone(), cache);
        assert_eq!(
            get_body(&app, "/a").await,
            (StatusCode::OK, "v0".into(), Some("MISS".into()))
        );
        assert_eq!(
            get_body(&app, "/a").await,
            (StatusCode::OK, "v0".into(), Some("HIT".into()))
        );
        assert_eq!(
            get_body(&app, "/a?x=1").await.1,
            "v1",
            "query is part of the key"
        );
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(
            get_body(&app, "/a").await.1,
            "v2",
            "expired entries are recomputed"
        );
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_computation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(
            calls.clone(),
            ResponseCache::new(Duration::from_secs(60), 8),
        );
        let results = futures_join(&app).await;
        assert!(results.iter().all(|b| b == "v0"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    async fn futures_join(app: &Router) -> Vec<String> {
        let mut handles = Vec::new();
        for _ in 0..8 {
            let app = app.clone();
            handles.push(tokio::spawn(async move { get_body(&app, "/a").await.1 }));
        }
        let mut out = Vec::new();
        for h in handles {
            out.push(h.await.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = app(
            calls.clone(),
            ResponseCache::new(Duration::from_secs(60), 8),
        );
        assert_eq!(
            get_body(&app, "/fail").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            get_body(&app, "/fail").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn warmer_refreshes_before_expiry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = ResponseCache::new(Duration::from_secs(60), 8);
        let app = app(calls.clone(), cache.clone());
        spawn_warmer(app.clone(), vec!["/a".into()], Duration::from_millis(100));
        tokio::time::sleep(Duration::from_millis(80)).await;
        let (_, first, x) = get_body(&app, "/a").await;
        assert_eq!((first.as_str(), x.as_deref()), ("v0", Some("HIT")));
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (_, later, _) = get_body(&app, "/a").await;
        assert_ne!(later, "v0", "the warmer replaced the entry");
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn capacity_is_bounded() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = ResponseCache::new(Duration::from_secs(60), 2);
        let app = app(calls, cache.clone());
        for q in ["/a?1", "/a?2", "/a?3"] {
            get_body(&app, q).await;
        }
        assert_eq!(cache.len(), 2);
    }
}
