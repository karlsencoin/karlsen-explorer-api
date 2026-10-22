//! `/info/miners/*` against a throwaway KGI tracker database (SQLite, no Postgres).

use std::{path::PathBuf, sync::OnceLock};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use karlsen_explorer_api::{routes, AppState};
use rusqlite::{params, Connection};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

const POOL_A: &str = "karlsen:qpoola";
const POOL_B: &str = "karlsen:qpoolb";
const SOLO: &str = "karlsen:qsolo";
const LATE: &str = "karlsen:qlate";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Builds the tracker database once per test binary and points the API at it.
fn tracker_db() -> &'static PathBuf {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("kgi-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let c = Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE network_snapshot (ts_ms INTEGER, network_hps REAL, window_blocks INTEGER,
                 active_miners INTEGER, window_minutes INTEGER, min_blocks_gate INTEGER);
             CREATE TABLE miner_snapshot (ts_ms INTEGER, address TEXT, blocks_in_window INTEGER,
                 window_sompi INTEGER, hashrate_hs REAL, winrate REAL, alltime_sompi INTEGER);
             CREATE TABLE miner_blocks (address TEXT, amount_sompi INTEGER, timestamp_ms INTEGER);
             CREATE TABLE miner_software (address TEXT, software TEXT, node_version TEXT,
                 last_seen_ms INTEGER, blocks INTEGER);
             CREATE TABLE miner_alltime (address TEXT, total_sompi INTEGER, blocks INTEGER,
                 first_seen_ms INTEGER, last_seen_ms INTEGER);",
        )
        .unwrap();
        let ts = now_ms() - 10_000;
        c.execute(
            "INSERT INTO network_snapshot VALUES (?1, 1.5e16, 3600, 3, 60, 2)",
            params![ts - 60_000],
        )
        .unwrap();
        c.execute(
            "INSERT INTO network_snapshot VALUES (?1, 2.0e14, 3600, 3, 60, 2)",
            params![ts],
        )
        .unwrap();
        for (addr, blocks, sompi, hps, win) in [
            (POOL_A, 3, 300_000_000_i64, 1.5e14, 0.5),
            (POOL_B, 1, 100_000_000, 2.5e13, 0.125),
            (SOLO, 2, 200_000_000, 5.0e13, 0.25),
        ] {
            c.execute(
                "INSERT INTO miner_snapshot VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![ts, addr, blocks, sompi, hps, win],
            )
            .unwrap();
        }
        // Ledger: in-window findings, plus an address under the gate (LATE).
        for (addr, n) in [(POOL_A, 3), (POOL_B, 1), (SOLO, 2), (LATE, 1)] {
            for i in 0..n {
                c.execute(
                    "INSERT INTO miner_blocks VALUES (?1, 100000000, ?2)",
                    params![addr, ts - 1000 * (i + 1)],
                )
                .unwrap();
            }
        }
        // A POOL_A block older than 24 h: kept in the ledger, but outside day_kls.
        c.execute(
            "INSERT INTO miner_blocks VALUES (?1, 100000000, ?2)",
            params![POOL_A, ts - 25 * 3600 * 1000],
        )
        .unwrap();
        c.execute_batch(&format!(
            "INSERT INTO miner_software VALUES ('{POOL_A}', 'acme (pool)', 'v3.3.0', 10, 5);
             INSERT INTO miner_software VALUES ('{POOL_A}', 'old-miner', 'v3.2.0', 5, 1);
             INSERT INTO miner_software VALUES ('{POOL_B}', 'acme (pool)', NULL, 9, 1);
             INSERT INTO miner_software VALUES ('{SOLO}', 'karlsen-miner', 'v3.3.0', 8, 2);
             INSERT INTO miner_alltime VALUES ('{POOL_A}', 9007199254740993000, 1000, 1, 2);"
        ))
        .unwrap();
        // SAFETY: set once, before any request reads it, in a single-threaded init.
        unsafe { std::env::set_var("KGI_TRACKER_DB", &path) };
        path
    })
}

async fn get(path: &str) -> (u16, String) {
    tracker_db();
    let db = PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1:1/unused")
        .unwrap();
    let app = routes::router(AppState::new(db, None));
    let resp = app
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn top_matches_legacy_shape() {
    let (status, body) = get("/info/miners/top").await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();

    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "ts_ms",
            "stale",
            "recent_hours",
            "recent_miners",
            "pools",
            "network",
            "miners"
        ]
    );
    assert_eq!(v["stale"], false);
    // Latest snapshot only; floats keep Python spelling on the wire.
    assert!(
        body.contains(r#""hashrate_hs":200000000000000.0"#),
        "{body}"
    );

    let miners = v["miners"].as_array().unwrap();
    assert_eq!(miners.len(), 3);
    assert_eq!(miners[0]["address"], POOL_A); // ordered by hashrate
    assert_eq!(miners[0]["software"], "acme (pool)");
    assert_eq!(miners[0]["others"], serde_json::json!(["old-miner"]));
    // day_kls comes from the Postgres-backed cache, which never warms up here.
    assert!(miners[0]["day_kls"].is_null());
    assert!(miners[0].get("alltime_kls").is_none());
    assert_eq!(miners[1]["address"], SOLO);
    assert!(body.contains(r#""window_kls":2.0"#));

    // LATE found a block but is not in the snapshot: listed as a recent miner,
    // without software keys.
    let recent = v["recent_miners"].as_array().unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["address"], LATE);
    let rkeys: Vec<&str> = recent[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        rkeys,
        [
            "address",
            "blocks_recent",
            "recent_kls",
            "last_block_ms",
            "day_kls"
        ]
    );

    // Both pool addresses merge under the pool label; solo miners stay separate.
    let pools = v["pools"].as_array().unwrap();
    assert_eq!(pools[0]["name"], "acme (pool)");
    assert_eq!(pools[0]["is_pool"], true);
    assert_eq!(pools[0]["addresses"], 2);
    assert_eq!(pools[0]["blocks"], 4);
    assert_eq!(pools[0]["share"].as_f64().unwrap(), 4.0 / 7.0);
    assert_eq!(pools[1]["name"], SOLO);
    assert_eq!(pools[1]["is_pool"], false);
}

#[tokio::test]
async fn top_limits_and_validation() {
    let (_, body) = get("/info/miners/top?limit=1&recent_hours=0").await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["miners"].as_array().unwrap().len(), 1);
    assert_eq!(v["recent_miners"], serde_json::json!([]));
    assert_eq!(get("/info/miners/top?limit=0").await.0, 422);
    assert_eq!(get("/info/miners/top?recent_hours=169").await.0, 422);
}

#[tokio::test]
async fn network_snapshot() {
    let (status, body) = get("/info/miners/network").await;
    assert_eq!(status, 200);
    assert!(body.starts_with(r#"{"ts_ms":"#), "{body}");
    assert!(body.contains(r#""stale":false,"hashrate_hs":200000000000000.0,"window_blocks":3600"#));
}

#[tokio::test]
async fn miner_detail() {
    let (status, body) = get(&format!("/info/miners/address/{SOLO}")).await;
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["alltime"]["blocks"], 2);
    assert!(body.contains(r#""total_kls":2.0"#));
    assert_eq!(v["history"].as_array().unwrap().len(), 1);
    assert_eq!(v["history"][0]["winrate"].as_f64(), Some(0.25));

    let (status, _) = get("/info/miners/address/karlsen:nobody").await;
    assert_eq!(status, 200); // legacy answers zeros, not 404
    assert_eq!(get("/info/miners/address/qsolo").await.0, 400);
}
