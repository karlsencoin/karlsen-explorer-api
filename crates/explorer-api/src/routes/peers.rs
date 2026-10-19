//! KGI node map: `/info/nodes`, `/info/peers/geo`, `/info/seeder-nodes` and the
//! operator-only `/admin/seed-candidates`. Ported from legacy
//! `get_peers_v2.py` (mounted as `endpoints/get_peers.py` in rest-v2).
//!
//! Everything here is read-only. The scanner (`discover_nodes.py` +
//! `enrich_nodes.py`, cron) writes `discovered_nodes.json`, `node_history.db` and,
//! since this port, the `peer_geo` history table; legacy wrote `peer_geo` itself on
//! each request.
//!
//! Node objects are passed through as the scanner wrote them (key order kept),
//! with the same annotations legacy added: `isAlive`, `lon`/`lng`, `isSeed`,
//! `lastSeenAgoSec`, `acceptsInbound`, `inboundUptimeSec`, a /16-masked `ip` and
//! a stable `peerHandle`. Raw IPs never leave this module except through the
//! token-protected admin endpoint.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

struct Settings {
    nodes_file: PathBuf,
    history_db: PathBuf,
    geo_cache_file: PathBuf,
    seeder_history_file: PathBuf,
    history_window_sec: i64,
    enrich_db: PathBuf,
    inbound_gap_sec: i64,
    admin_token: String,
    seed_min_uptime_sec: i64,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_int(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Same environment variables and defaults as legacy.
fn settings() -> &'static Settings {
    static S: OnceLock<Settings> = OnceLock::new();
    S.get_or_init(|| Settings {
        nodes_file: env_or(
            "DISCOVERED_NODES_FILE",
            "/opt/karlsen/kgi/discovered_nodes.json",
        )
        .into(),
        history_db: env_or("NODE_HISTORY_DB", "/etc/karlsen/peer_geo.db").into(),
        geo_cache_file: env_or("GEO_CACHE_FILE", "/opt/karlsen/kgi/geo_cache.json").into(),
        seeder_history_file: env_or(
            "SEEDER_HISTORY_FILE",
            "/opt/karlsen/kgi/seeder_history.json",
        )
        .into(),
        history_window_sec: env_int("NODE_HISTORY_WINDOW_SEC", 604_800),
        enrich_db: env_or("ENRICH_HISTORY_DB", "/opt/karlsen/kgi/node_history.db").into(),
        inbound_gap_sec: env_int("KGI_INBOUND_GAP_SEC", 90 * 60),
        admin_token: std::env::var("KGI_ADMIN_TOKEN").unwrap_or_default(),
        seed_min_uptime_sec: env_int("KGI_SEED_MIN_UPTIME_SEC", 86_400),
    })
}

fn now_sec() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---- small helpers (same behaviour as legacy) ----

/// `1.2.3.4` -> `1.2.x.x`; anything that is not dotted IPv4 is returned unchanged.
pub fn mask_ip(ip: &str) -> String {
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() == 4 {
        format!("{}.{}.x.x", parts[0], parts[1])
    } else {
        ip.to_string()
    }
}

/// Stable anonymous id: `peer-` + first 8 hex chars of sha256(raw ip).
pub fn peer_handle(raw_ip: &str) -> String {
    let digest = hex::encode(Sha256::digest(raw_ip.as_bytes()));
    format!("peer-{}", &digest[..8])
}

/// Seconds since an ISO `YYYY-MM-DDTHH:MM:SSZ` UTC timestamp (never negative);
/// `None` when the text has another shape.
pub fn ago_sec(iso: &str) -> Option<i64> {
    let ts = parse_iso_utc(iso)?;
    Some(((now_sec() - ts as f64) as i64).max(0))
}

fn parse_iso_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 61 {
        return None;
    }
    // Days from civil (Howard Hinnant), proleptic Gregorian.
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

fn load_json(path: &PathBuf) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Python truthiness for JSON values.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn str_of<'a>(n: &'a Map<String, Value>, k: &str) -> &'a str {
    n.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Longitude whichever name it has ("lon" first, then "lng").
fn lon_of(n: &Map<String, Value>) -> Value {
    match n.get("lon") {
        Some(v) if !v.is_null() => v.clone(),
        _ => n.get("lng").cloned().unwrap_or(Value::Null),
    }
}

fn seed_ips() -> HashSet<String> {
    load_json(&settings().seeder_history_file)
        .and_then(|d| d.get("ips").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// raw ip -> inbound_since for streaks that are still current (longest per ip).
fn inbound_map() -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let s = settings();
    let Ok(conn) = Connection::open_with_flags(&s.enrich_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return out;
    };
    let rows = conn
        .prepare(
            "SELECT ip, inbound_since, last_inbound FROM nodes WHERE inbound_since IS NOT NULL",
        )
        .and_then(|mut st| {
            st.query_map([], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
        });
    let Ok(rows) = rows else {
        tracing::warn!("inbound map unavailable");
        return out;
    };
    for (ip, since, last) in rows {
        let (Some(ip), Some(since)) = (ip, since) else {
            continue;
        };
        match ago_sec(last.as_deref().unwrap_or("")) {
            Some(ago) if ago <= s.inbound_gap_sec => {}
            _ => continue, // streak broken
        }
        match out.get(&ip) {
            Some(prev) if since >= *prev => {}
            _ => {
                out.insert(ip, since);
            }
        }
    }
    out
}

/// One entry per raw IP: prefer a completed handshake, then port 42111, then a
/// live connection; flags are OR-ed and a missing peerId is filled in.
fn dedup_live(nodes: &[Value]) -> Vec<Map<String, Value>> {
    fn score(n: &Map<String, Value>) -> (bool, bool, bool) {
        (
            truthy(n.get("handshakeOK")),
            n.get("port").and_then(Value::as_i64) == Some(42111),
            truthy(n.get("isConnected")),
        )
    }
    let mut best: HashMap<String, Map<String, Value>> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for n in nodes.iter().filter_map(Value::as_object) {
        let ip = str_of(n, "ip").to_string();
        let Some(cur) = best.get(&ip) else {
            order.push(ip.clone());
            best.insert(ip, n.clone());
            continue;
        };
        let new_wins = score(n) > score(cur);
        let (mut merged, other) = if new_wins {
            (n.clone(), cur)
        } else {
            (cur.clone(), n)
        };
        merged.insert(
            "isConnected".into(),
            Value::Bool(truthy(cur.get("isConnected")) || truthy(n.get("isConnected"))),
        );
        merged.insert(
            "handshakeOK".into(),
            Value::Bool(truthy(cur.get("handshakeOK")) || truthy(n.get("handshakeOK"))),
        );
        if !truthy(merged.get("peerId")) && truthy(other.get("peerId")) {
            merged.insert("peerId".into(), other["peerId"].clone());
        }
        best.insert(ip, merged);
    }
    order
        .into_iter()
        .filter_map(|ip| best.remove(&ip))
        .collect()
}

/// Peers seen before (peer_geo) that are not in the current snapshot and were
/// seen within the history window; shown as `isAlive: false`.
fn load_inactive(active: &HashSet<String>, seeds: &HashSet<String>) -> Vec<Map<String, Value>> {
    let s = settings();
    let conn = match Connection::open_with_flags(&s.history_db, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "peer_geo read failed");
            return Vec::new();
        }
    };
    type GeoRow = (
        String,
        Option<i64>,
        Option<f64>,
        Option<f64>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
    );
    let rows: rusqlite::Result<Vec<GeoRow>> = conn
        .prepare(
            "SELECT raw_ip, port, lat, lng, country, city, version, first_seen, last_seen \
             FROM peer_geo",
        )
        .and_then(|mut st| {
            st.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?
            .collect()
        });
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "peer_geo read failed");
            return Vec::new();
        }
    };

    let geo_cache = load_json(&s.geo_cache_file).unwrap_or(Value::Null);
    let mut out = Vec::new();
    for (raw_ip, port, mut lat, mut lng, country, city, version, first_seen, last_seen) in rows {
        if active.contains(&raw_ip) {
            continue;
        }
        let Some(ago) = ago_sec(&last_seen).filter(|a| *a <= s.history_window_sec) else {
            continue;
        };
        if lat.is_none() || lng.is_none() {
            // Rows stored before the lon fix: backfill from the geo cache.
            let loc = geo_cache
                .get(&raw_ip)
                .and_then(|g| g.get("loc"))
                .and_then(Value::as_str);
            if let Some((a, b)) = loc.and_then(|l| l.split_once(',')) {
                if let (Ok(a), Ok(b)) = (a.trim().parse(), b.trim().parse()) {
                    (lat, lng) = (Some(a), Some(b));
                }
            }
        }
        let mut m = Map::new();
        m.insert("lastSeenAgoSec".into(), json!(ago));
        m.insert("ip".into(), json!(raw_ip));
        m.insert("port".into(), json!(port));
        m.insert("lat".into(), json!(lat));
        m.insert("lng".into(), json!(lng));
        m.insert("country".into(), json!(country));
        m.insert("city".into(), json!(city));
        m.insert("version".into(), json!(version));
        m.insert("isSeed".into(), json!(seeds.contains(&raw_ip)));
        m.insert("isConnected".into(), json!(false));
        m.insert("handshakeOK".into(), json!(false));
        m.insert("isAlive".into(), json!(false));
        m.insert("first_seen".into(), json!(first_seen));
        m.insert("last_seen".into(), json!(last_seen));
        out.push(m);
    }
    out
}

/// Adds the legacy annotations, masks the IP and appends `peerHandle`. Existing
/// keys keep their position; new keys are appended in legacy order.
fn annotate(
    mut node: Map<String, Value>,
    alive: bool,
    seeds: Option<&HashSet<String>>,
    inbound: &HashMap<String, String>,
) -> Map<String, Value> {
    let raw_ip = str_of(&node, "ip").to_string();
    let lon = lon_of(&node);
    node.insert("isAlive".into(), json!(alive));
    node.insert("lon".into(), lon.clone());
    node.insert("lng".into(), lon);
    if let Some(seeds) = seeds {
        node.insert("isSeed".into(), json!(seeds.contains(&raw_ip)));
    }
    if alive {
        node.insert("lastSeenAgoSec".into(), json!(0));
    }
    let inbound_since = inbound.get(&raw_ip).filter(|s| !s.is_empty());
    let accepts = alive && (inbound_since.is_some() || truthy(node.get("handshakeOK")));
    let since = if alive {
        ago_sec(inbound_since.map(String::as_str).unwrap_or(""))
    } else {
        None
    };
    node.insert("acceptsInbound".into(), json!(accepts));
    node.insert(
        "inboundUptimeSec".into(),
        if accepts { json!(since) } else { Value::Null },
    );
    node.shift_remove("inboundSince");
    node.insert("ip".into(), json!(mask_ip(&raw_ip)));
    node.insert("peerHandle".into(), json!(peer_handle(&raw_ip)));
    node
}

fn is_live(n: &Map<String, Value>) -> bool {
    truthy(n.get("isConnected")) || truthy(n.get("handshakeOK"))
}

/// The scanner snapshot; `None` when the file is missing, unreadable or `{}`.
fn snapshot() -> Option<Map<String, Value>> {
    match load_json(&settings().nodes_file) {
        Some(Value::Object(m)) if !m.is_empty() => Some(m),
        _ => {
            tracing::error!("failed to load the discovered nodes file");
            None
        }
    }
}

fn raw_nodes(data: &Option<Map<String, Value>>) -> Vec<Value> {
    data.as_ref()
        .and_then(|d| d.get("nodes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .expect("peers task panicked")
}

// ---- GET /info/nodes ----

fn nodes_body() -> Value {
    let Some(data) = snapshot() else {
        return json!({
            "timestamp": null,
            "totalSeeds": 0,
            "totalAlive": 0,
            "totalGeolocated": 0,
            "totalActive": 0,
            "peerCount": 0,
            "nodes": [],
        });
    };
    let data = Some(data);
    let raw = dedup_live(&raw_nodes(&data));
    let seeds = seed_ips();
    let inbound = inbound_map();
    let active: HashSet<String> = raw
        .iter()
        .map(|n| str_of(n, "ip").to_string())
        .filter(|ip| !ip.is_empty())
        .collect();

    let mut live = Vec::new();
    let mut passive = Vec::new();
    for n in raw {
        if is_live(&n) {
            live.push(annotate(n, true, Some(&seeds), &inbound));
        } else {
            passive.push(annotate(n, false, Some(&seeds), &inbound));
        }
    }
    let historical: Vec<_> = load_inactive(&active, &seeds)
        .into_iter()
        .map(|n| annotate(n, false, None, &inbound))
        .collect();

    let (n_live, n_hist) = (live.len(), historical.len());
    let all: Vec<Value> = live
        .into_iter()
        .chain(passive)
        .chain(historical)
        .map(Value::Object)
        .collect();

    // Python dict literal semantics: a repeated key keeps its first position and
    // takes the last value.
    let mut out = Map::new();
    out.insert(
        "historyWindowSec".into(),
        json!(settings().history_window_sec),
    );
    out.insert("totalRecent".into(), json!(n_hist));
    for (k, v) in data.into_iter().flatten() {
        if k != "nodes" {
            out.insert(k, v);
        }
    }
    out.insert("nodes".into(), Value::Array(all));
    out.insert("totalActive".into(), json!(n_live));
    out.insert("totalAlive".into(), json!(n_live));
    out.insert("peerCount".into(), json!(n_live));
    Value::Object(out)
}

pub async fn nodes() -> Json<Value> {
    Json(blocking(nodes_body).await)
}

// ---- GET /info/peers/geo ----

fn peers_geo_body() -> Value {
    let data = snapshot();
    let raw = dedup_live(&raw_nodes(&data));
    let seeds = seed_ips();
    let inbound = inbound_map();
    let active: HashSet<String> = raw
        .iter()
        .map(|n| str_of(n, "ip").to_string())
        .filter(|ip| !ip.is_empty())
        .collect();
    let live: Vec<_> = raw
        .into_iter()
        .filter(is_live)
        .map(|n| annotate(n, true, Some(&seeds), &inbound))
        .collect();
    let historical: Vec<_> = load_inactive(&active, &seeds)
        .into_iter()
        .map(|n| annotate(n, false, None, &inbound))
        .filter(|n| {
            n.get("lat").is_some_and(|v| !v.is_null()) && n.get("lng").is_some_and(|v| !v.is_null())
        })
        .collect();
    let peer_count = live.len();
    let all: Vec<Value> = live
        .into_iter()
        .chain(historical)
        .map(Value::Object)
        .collect();
    json!({ "peerCount": peer_count, "nodes": all })
}

pub async fn peers_geo() -> Json<Value> {
    Json(blocking(peers_geo_body).await)
}

// ---- GET /info/seeder-nodes ----

fn seeder_nodes_body() -> Value {
    let data = snapshot();
    let seeds = seed_ips();
    let inbound = inbound_map();
    let nodes: Vec<Value> = dedup_live(&raw_nodes(&data))
        .into_iter()
        .filter(|n| truthy(n.get("handshakeOK")))
        .map(|n| Value::Object(annotate(n, true, Some(&seeds), &inbound)))
        .collect();
    json!({ "count": nodes.len(), "nodes": nodes })
}

pub async fn seeder_nodes() -> Json<Value> {
    Json(blocking(seeder_nodes_body).await)
}

// ---- GET /admin/seed-candidates (operator only, full IPs) ----

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn seed_candidates_body() -> Value {
    let s = settings();
    let data = snapshot();
    let seeds = seed_ips();
    let inbound = inbound_map();
    let mut out: Vec<Map<String, Value>> = Vec::new();
    for n in dedup_live(&raw_nodes(&data)) {
        let raw_ip = str_of(&n, "ip").to_string();
        if raw_ip.is_empty() || !truthy(n.get("handshakeOK")) {
            continue;
        }
        let uptime = ago_sec(inbound.get(&raw_ip).map(String::as_str).unwrap_or(""));
        let or_default = |k: &str, d: Value| {
            if truthy(n.get(k)) {
                n[k].clone()
            } else {
                d
            }
        };
        let mut m = Map::new();
        m.insert("peerHandle".into(), json!(peer_handle(&raw_ip)));
        m.insert("ip".into(), json!(raw_ip));
        m.insert("port".into(), or_default("port", json!(42111)));
        m.insert("version".into(), or_default("version", json!("")));
        m.insert("city".into(), or_default("city", json!("")));
        m.insert("country".into(), or_default("country", json!("")));
        m.insert("org".into(), or_default("org", json!("")));
        m.insert("inboundUptimeSec".into(), json!(uptime));
        m.insert("inSeeder".into(), json!(seeds.contains(&raw_ip)));
        m.insert(
            "candidate".into(),
            json!(uptime.is_some_and(|u| u >= s.seed_min_uptime_sec)),
        );
        out.push(m);
    }
    let up = |m: &Map<String, Value>| m["inboundUptimeSec"].as_i64().unwrap_or(0);
    out.sort_by_key(|m| std::cmp::Reverse(up(m))); // stable, like Python's sort
    json!({ "minUptimeSec": s.seed_min_uptime_sec, "nodes": out })
}

pub async fn seed_candidates(headers: HeaderMap) -> Response {
    let token = &settings().admin_token;
    if token.is_empty() {
        return (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response();
    }
    let given = headers
        .get("x-kgi-admin")
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    if !constant_time_eq(given, token.as_bytes()) {
        return (StatusCode::FORBIDDEN, Json(json!({"detail": "Forbidden"}))).into_response();
    }
    Json(blocking(seed_candidates_body).await).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_and_handle() {
        assert_eq!(mask_ip("149.50.116.7"), "149.50.x.x");
        assert_eq!(mask_ip("2001:db8::1"), "2001:db8::1");
        assert_eq!(mask_ip(""), "");
        // sha256("1.2.3.4") = 6694f83c...
        assert_eq!(peer_handle("1.2.3.4"), "peer-6694f83c");
    }

    #[test]
    fn iso_parsing() {
        assert_eq!(parse_iso_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_utc("2026-10-19T18:00:36Z"), Some(1_792_432_836));
        assert_eq!(parse_iso_utc("2026-10-19 18:00:36"), None);
        assert_eq!(parse_iso_utc(""), None);
    }

    #[test]
    fn dedup_prefers_handshake_then_standard_port() {
        let nodes = vec![
            json!({"ip": "1.1.1.1", "port": 51000, "isConnected": true, "peerId": "abc"}),
            json!({"ip": "1.1.1.1", "port": 42111, "handshakeOK": true}),
            json!({"ip": "2.2.2.2", "port": 42111}),
        ];
        let out = dedup_live(&nodes);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["port"], 42111);
        assert_eq!(out[0]["isConnected"], true);
        assert_eq!(out[0]["handshakeOK"], true);
        assert_eq!(out[0]["peerId"], "abc");
        assert_eq!(out[1]["ip"], "2.2.2.2");
    }

    #[test]
    fn annotate_keeps_key_order_and_masks() {
        let n = json!({"ip": "1.2.3.4", "port": 42111, "lng": 8.6, "lat": 50.1,
                       "handshakeOK": true, "inboundSince": "x"});
        let mut seeds = HashSet::new();
        seeds.insert("1.2.3.4".to_string());
        let a = annotate(
            n.as_object().unwrap().clone(),
            true,
            Some(&seeds),
            &HashMap::new(),
        );
        let keys: Vec<&str> = a.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "ip",
                "port",
                "lng",
                "lat",
                "handshakeOK",
                "isAlive",
                "lon",
                "isSeed",
                "lastSeenAgoSec",
                "acceptsInbound",
                "inboundUptimeSec",
                "peerHandle"
            ]
        );
        assert_eq!(a["ip"], "1.2.x.x");
        assert_eq!(a["lon"], 8.6);
        assert_eq!(a["acceptsInbound"], true);
        assert_eq!(a["inboundUptimeSec"], Value::Null);
    }
}
