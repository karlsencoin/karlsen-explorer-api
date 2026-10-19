//! In-memory cache for per-address transaction counts.
//!
//! `count(*)` over `addresses_transactions` takes seconds for the busiest addresses
//! (a mining pool has 15M+ rows). Each count is kept for a time that grows with its
//! size (the same tiers kaspa-rest-server uses for its `Cache-Control`), and
//! concurrent requests for the same address share a single database query.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// How long a count stays fresh, by size.
pub fn ttl_for(total: i64) -> Duration {
    Duration::from_secs(match total {
        n if n >= 1_000_000 => 600,
        n if n >= 100_000 => 60,
        n if n >= 10_000 => 20,
        _ => 8,
    })
}

/// Cleanup runs once the map holds this many addresses.
const PRUNE_AT: usize = 10_000;

type Slot = Arc<tokio::sync::Mutex<Option<(i64, Instant)>>>;

#[derive(Default)]
pub struct CountCache {
    slots: Mutex<HashMap<String, Slot>>,
}

impl CountCache {
    /// Returns the cached count and the seconds it stays fresh, or runs `load`
    /// (once, even when many callers ask at the same time) and caches its result.
    pub async fn get_or_load<F, Fut, E>(&self, key: &str, load: F) -> Result<(i64, u64), E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<i64, E>>,
    {
        let slot = self.slot(key);
        // Callers for the same address queue here while the first one queries.
        let mut entry = slot.lock().await;
        let now = Instant::now();
        if let Some((total, expires)) = *entry {
            if expires > now {
                return Ok((total, (expires - now).as_secs().max(1)));
            }
        }
        let total = load().await?;
        let ttl = ttl_for(total);
        *entry = Some((total, now + ttl));
        Ok((total, ttl.as_secs()))
    }

    fn slot(&self, key: &str) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots.len() >= PRUNE_AT && !slots.contains_key(key) {
            let now = Instant::now();
            // Drop expired entries nobody is loading right now.
            slots.retain(|_, s| match s.try_lock() {
                Ok(e) => e.is_some_and(|(_, exp)| exp > now),
                Err(_) => true,
            });
        }
        slots.entry(key.to_string()).or_default().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn concurrent_requests_share_one_query() {
        let cache = Arc::new(CountCache::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let (cache, calls) = (cache.clone(), calls.clone());
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_load("pool", || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok::<_, ()>(15_000_000)
                    })
                    .await
                    .unwrap()
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap().0, 15_000_000);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn big_counts_are_kept_longer_and_errors_are_not_cached() {
        let cache = CountCache::default();
        let (n, ttl) = cache
            .get_or_load("pool", || async { Ok::<_, ()>(2_000_000) })
            .await
            .unwrap();
        assert_eq!((n, ttl), (2_000_000, 600));
        // Served from memory: the loader is not called again.
        let (n, _) = cache
            .get_or_load("pool", || async {
                panic!("should not reload") as Result<i64, ()>
            })
            .await
            .unwrap();
        assert_eq!(n, 2_000_000);

        assert!(cache
            .get_or_load("x", || async { Err::<i64, _>("db down") })
            .await
            .is_err());
        let (n, ttl) = cache
            .get_or_load("x", || async { Ok::<_, ()>(5) })
            .await
            .unwrap();
        assert_eq!((n, ttl), (5, 8));
    }
}
