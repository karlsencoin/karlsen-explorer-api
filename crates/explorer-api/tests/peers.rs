//! KGI node map endpoints against throwaway scanner files (no Postgres).
//!
//! Set `PEERS_DUMP_DIR` to also write the fixture files and every response
//! there, so they can be diffed against the legacy Python implementation.

use std::{path::PathBuf, sync::OnceLock};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use karlsen_explorer_api::{routes, AppState};
use rusqlite::Connection;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const TOKEN: &str = "test-admin-token";

fn iso(ago_sec: i64) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - ago_sec;
    // Format as YYYY-MM-DDTHH:MM:SSZ (civil from days, Howard Hinnant).
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn fixture_dir() -> &'static PathBuf {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(|| {
        let dir = match std::env::var("PEERS_DUMP_DIR") {
            Ok(d) => PathBuf::from(d),
            Err(_) => std::env::temp_dir().join(format!("kgi-peers-{}", std::process::id())),
        };
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let nodes = json!({
            "timestamp": "2026-10-19T18:00:00Z",
            "totalSeeds": 3,
            "totalAlive": 99,
            "nodes": [
                // Same host twice: ephemeral inbound port + crawler-reached listen port.
                {"ip": "10.0.0.1", "port": 51234, "isConnected": true, "handshakeOK": false,
                 "peerId": "aa11", "lat": 50.11, "lng": 8.68, "country": "DE",
                 "city": "Frankfurt am Main", "version": "v3.3.0", "inboundSince": "stale"},
                {"ip": "10.0.0.1", "port": 42111, "isConnected": false, "handshakeOK": true,
                 "lat": 50.11, "lng": 8.68, "country": "DE", "city": "Frankfurt am Main",
                 "version": "v3.3.0"},
                {"ip": "10.0.0.2", "port": 42111, "isConnected": false, "handshakeOK": true,
                 "lat": 52.23, "lon": 21.01, "country": "PL", "city": "Warsaw",
                 "version": "v3.1.1", "org": "AS1 Example"},
                {"ip": "10.0.0.3", "port": 42111, "isConnected": false, "handshakeOK": false,
                 "lat": 0, "lng": 0, "geoMissing": true},
            ],
            "enriched_at": "2026-10-19T18:01:00Z",
        });
        std::fs::write(dir.join("discovered_nodes.json"), nodes.to_string()).unwrap();
        std::fs::write(
            dir.join("seeder_history.json"),
            json!({"ips": ["10.0.0.2", "10.0.0.5"]}).to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("geo_cache.json"),
            json!({"10.0.0.6": {"loc": "41.01,28.97"}}).to_string(),
        )
        .unwrap();

        let geo = Connection::open(dir.join("peer_geo.db")).unwrap();
        geo.execute_batch(
            "CREATE TABLE peer_geo (raw_ip TEXT PRIMARY KEY, port INTEGER, lat REAL, lng REAL,
                 country TEXT, city TEXT, version TEXT, is_seed INTEGER NOT NULL DEFAULT 0,
                 first_seen TEXT NOT NULL, last_seen TEXT NOT NULL);",
        )
        .unwrap();
        for (ip, lat, lng, last) in [
            ("10.0.0.1", Some(50.11), Some(8.68), iso(10)), // active: skipped
            ("10.0.0.5", Some(43.6), Some(-79.4), iso(3600)), // recent, seed
            ("10.0.0.6", None, None, iso(86_400)),           // geo from cache
            ("10.0.0.7", None, None, iso(2 * 86_400)),       // no geo at all
            ("10.0.0.8", Some(1.0), Some(2.0), iso(8 * 86_400)), // outside the window
        ] {
            geo.execute(
                "INSERT INTO peer_geo VALUES (?1, 42111, ?2, ?3, 'XX', 'City', 'v3.0.0', 0, ?4, ?5)",
                rusqlite::params![ip, lat, lng, iso(30 * 86_400), last],
            )
            .unwrap();
        }

        let hist = Connection::open(dir.join("node_history.db")).unwrap();
        hist.execute_batch(
            "CREATE TABLE nodes (node_key TEXT PRIMARY KEY, ip TEXT NOT NULL, port INTEGER NOT NULL,
                 first_seen TEXT NOT NULL, last_seen TEXT NOT NULL, seen_count INTEGER NOT NULL DEFAULT 1,
                 last_country TEXT, last_city TEXT, last_version TEXT,
                 inbound_since TEXT, last_inbound TEXT);",
        )
        .unwrap();
        for (key, ip, since, last) in [
            ("10.0.0.1:42111", "10.0.0.1", iso(3 * 86_400), iso(120)), // 3-day streak
            ("10.0.0.1:16111", "10.0.0.1", iso(86_400), iso(120)),     // shorter: ignored
            ("10.0.0.2:42111", "10.0.0.2", iso(5 * 86_400), iso(4 * 3600)), // broken streak
        ] {
            hist.execute(
                "INSERT INTO nodes VALUES (?1, ?2, 42111, ?3, ?4, 5, NULL, NULL, NULL, ?3, ?4)",
                rusqlite::params![key, ip, since, last],
            )
            .unwrap();
        }

        // SAFETY: set once, before any request reads them.
        unsafe {
            std::env::set_var("DISCOVERED_NODES_FILE", dir.join("discovered_nodes.json"));
            std::env::set_var("NODE_HISTORY_DB", dir.join("peer_geo.db"));
            std::env::set_var("GEO_CACHE_FILE", dir.join("geo_cache.json"));
            std::env::set_var("SEEDER_HISTORY_FILE", dir.join("seeder_history.json"));
            std::env::set_var("ENRICH_HISTORY_DB", dir.join("node_history.db"));
            std::env::set_var("KGI_ADMIN_TOKEN", TOKEN);
        }
        dir
    })
}

async fn get(path: &str, token: Option<&str>) -> (u16, Value) {
    let dir = fixture_dir();
    let db = PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1:1/unused")
        .unwrap();
    let app = routes::router(AppState::new(db, None));
    let mut req = Request::get(path);
    if let Some(t) = token {
        req = req.header("x-kgi-admin", t);
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    if std::env::var("PEERS_DUMP_DIR").is_ok() && status == 200 {
        let name = path.trim_start_matches('/').replace('/', "_");
        std::fs::write(dir.join(format!("rust-{name}.json")), &body).unwrap();
    }
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn find<'a>(nodes: &'a Value, ip: &str) -> Vec<&'a Value> {
    nodes["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["ip"] == ip)
        .collect()
}

#[tokio::test]
async fn nodes_live_passive_and_history() {
    let (status, v) = get("/info/nodes", None).await;
    assert_eq!(status, 200);
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    // File keys keep their place; counts from the file are overwritten in place.
    assert_eq!(
        keys,
        [
            "historyWindowSec",
            "totalRecent",
            "timestamp",
            "totalSeeds",
            "totalAlive",
            "enriched_at",
            "nodes",
            "totalActive",
            "peerCount"
        ]
    );
    assert_eq!(v["totalAlive"], 2);
    assert_eq!(v["totalRecent"], 3); // .5, .6, .7 (not the active .1 or the old .8)
    assert_eq!(v["historyWindowSec"], 604_800);

    // Deduplicated, masked, never a raw IP.
    let body = v.to_string();
    assert!(!body.contains("10.0.0."), "raw IP leaked: {body}");
    let one = &v["nodes"][0];
    assert_eq!(one["ip"], "10.0.x.x");
    assert_eq!(one["port"], 42111);
    assert_eq!(one["peerId"], "aa11");
    assert_eq!(one["isAlive"], true);
    assert_eq!(one["acceptsInbound"], true);
    let up = one["inboundUptimeSec"].as_i64().unwrap();
    assert!((3 * 86_400..3 * 86_400 + 60).contains(&up), "{up}");
    assert!(one.get("inboundSince").is_none());

    // Seed flag and broken streak for .2: accepts inbound (handshake) but no uptime.
    let two = &v["nodes"][1];
    assert_eq!(two["isSeed"], true);
    assert_eq!(two["lon"], 21.01);
    assert_eq!(two["inboundUptimeSec"], Value::Null);

    // Passive node, then history with the geo-cache backfill.
    assert_eq!(v["nodes"][2]["isAlive"], false);
    let hist: Vec<&Value> = v["nodes"].as_array().unwrap()[3..].iter().collect();
    assert_eq!(hist.len(), 3);
    assert_eq!(hist[0]["isSeed"], true);
    assert_eq!(hist[1]["lat"], 41.01);
    assert_eq!(hist[1]["lon"], 28.97);
    assert_eq!(hist[2]["lat"], Value::Null);
    assert!(find(&v, "10.0.x.x").len() >= 6);
}

#[tokio::test]
async fn peers_geo_skips_history_without_coordinates() {
    let (status, v) = get("/info/peers/geo", None).await;
    assert_eq!(status, 200);
    assert_eq!(v["peerCount"], 2);
    // 2 live + .5 and .6 (the .7 row has no coordinates anywhere).
    assert_eq!(v["nodes"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn seeder_nodes_are_handshake_confirmed() {
    let (status, v) = get("/info/seeder-nodes", None).await;
    assert_eq!(status, 200);
    assert_eq!(v["count"], 2);
}

#[tokio::test]
async fn seed_candidates_need_the_token() {
    assert_eq!(get("/admin/seed-candidates", None).await.0, 403);
    assert_eq!(get("/admin/seed-candidates", Some("wrong")).await.0, 403);
    let (status, v) = get("/admin/seed-candidates", Some(TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(v["minUptimeSec"], 86_400);
    let nodes = v["nodes"].as_array().unwrap();
    assert_eq!(nodes[0]["ip"], "10.0.0.1"); // full IP for the operator
    assert_eq!(nodes[0]["candidate"], true);
    assert_eq!(nodes[1]["org"], "AS1 Example");
    assert_eq!(nodes[1]["inSeeder"], true);
    assert_eq!(nodes[1]["candidate"], false);
}
