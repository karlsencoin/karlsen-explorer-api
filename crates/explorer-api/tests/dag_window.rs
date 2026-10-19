#[allow(dead_code)]
mod common;

use common::{request, setup};

const BB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const D1: &str = "d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1";
const E1: &str = "e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1";

macro_rules! db_or_skip {
    () => {
        match setup().await {
            Some(db) => db,
            None => {
                eprintln!("TEST_DATABASE_URL not set: skipping");
                return;
            }
        }
    };
}

fn daa(b: &serde_json::Value) -> i64 {
    b["daaScore"].as_i64().unwrap()
}

#[tokio::test]
async fn newest_blocks_with_parents() {
    let db = db_or_skip!();
    let app = db.app();

    let r = request(&app, "/info/dag-window?limit=1000").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("public, max-age=2"));
    assert_eq!(r.header("x-data-source"), Some("Database"));
    let v = r.json();
    let blocks = v["blocks"].as_array().unwrap();
    assert!(blocks.len() >= 6);
    assert_eq!(v["count"].as_u64().unwrap() as usize, blocks.len());
    // Newest first; equal scores in a fixed (hash) order.
    for w in blocks.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        assert!(daa(a) > daa(b) || (daa(a) == daa(b) && a["hash"].as_str() > b["hash"].as_str()));
    }
    assert_eq!(v["newestDaa"].as_i64(), Some(daa(&blocks[0])));
    assert_eq!(v["oldestDaa"].as_i64(), Some(daa(blocks.last().unwrap())));

    let e1 = blocks.iter().find(|b| b["hash"] == E1).unwrap();
    assert_eq!(
        e1,
        &serde_json::json!({
            "hash": E1,
            "daaScore": 1000,
            "blueScore": 1001,
            "parents": [BB, D1],
            "isChainBlock": null,
            "selectedParentHash": BB,
            "mergeSetBluesHashes": [],
            "mergeSetRedsHashes": [D1],
        })
    );
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["count", "newestDaa", "oldestDaa", "blocks"]);

    let r = request(&app, "/info/dag-window?limit=2").await;
    assert_eq!(r.json()["blocks"].as_array().unwrap().len(), 2);
    db.teardown().await;
}

#[tokio::test]
async fn delta_since_daa_is_ascending_and_uncached() {
    let db = db_or_skip!();
    let app = db.app();

    let r = request(&app, "/info/dag-window?since_daa=999").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("no-store"));
    let v = r.json();
    let blocks = v["blocks"].as_array().unwrap();
    assert!(!blocks.is_empty());
    assert!(blocks.iter().all(|b| daa(b) > 999));
    assert!(blocks.windows(2).all(|w| daa(&w[0]) <= daa(&w[1])));

    // Nothing newer: legacy shape with null bounds.
    let v = request(&app, "/info/dag-window?since_daa=999999999")
        .await
        .json();
    assert_eq!(
        v,
        serde_json::json!({"count": 0, "newestDaa": null, "oldestDaa": null, "blocks": []})
    );

    for bad in ["limit=0", "limit=1001", "since_daa=-1"] {
        let r = request(&app, &format!("/info/dag-window?{bad}")).await;
        assert_eq!(r.status, 422, "{bad}");
    }
    db.teardown().await;
}
