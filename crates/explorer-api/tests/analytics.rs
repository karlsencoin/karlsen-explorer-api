//! `/analytics/addresses/*` on the v20 schema plus `seed-analytics.sql`.
//! Expected values were produced by the legacy Python handlers on the same data.
#[allow(dead_code)]
mod common;

use common::{request, setup};
use serde_json::json;

macro_rules! db_or_skip {
    () => {
        match setup().await {
            Some(db) => {
                sqlx::raw_sql(include_str!("fixtures/seed-analytics.sql"))
                    .execute(&db.pool)
                    .await
                    .expect("seed-analytics");
                db
            }
            None => {
                eprintln!("TEST_DATABASE_URL not set: skipping");
                return;
            }
        }
    };
}

const ADDR: &str = "karlsen:qrc9rgx9x8gyekvrd7533hj3tl0cxd5r9ggxkg5l4a6xgllethrtuvp48fc3n";

#[tokio::test]
async fn rich_list_matches_legacy() {
    let db = db_or_skip!();
    let app = db.app();

    let r = request(&app, "/analytics/addresses/top?limit=3").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("public, max-age=300"));
    let body = String::from_utf8(r.body.clone()).unwrap();
    assert_eq!(
        body,
        r#"{"top_addresses":[{"delta_24h":42000.0,"is_new":null,"address":"karlsen:qwhale1","amount":123456789.01234567,"percent":93.995829},{"delta_24h":0.0,"is_new":null,"address":"karlsen:qorca01","amount":3456789.01234567,"percent":2.631882},{"delta_24h":-1.23456789,"is_new":null,"address":"karlsen:qorca02","amount":3456789.01234567,"percent":2.631882}]}"#
    );

    // Not in yesterday's snapshot: flagged new.
    let v = request(&app, "/analytics/addresses/top").await.json();
    let newbie = v["top_addresses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["address"] == "karlsen:qnewbie")
        .unwrap();
    assert_eq!(newbie["is_new"], true);
    assert_eq!(newbie["delta_24h"], serde_json::Value::Null);

    let v = request(&app, "/analytics/addresses/total").await.json();
    assert_eq!(v, json!({"total_addresses": 11}));

    let v = request(
        &app,
        "/analytics/addresses/distribution?min_amount=1000&max_amount=10000",
    )
    .await
    .json();
    assert_eq!(v, json!({"from_addresses_total": 1}));

    let v = request(
        &app,
        "/analytics/addresses/range?min_amount=1&max_amount=200",
    )
    .await
    .json();
    assert_eq!(
        v,
        json!([
            {"delta_24h": null, "is_new": null, "address": "karlsen:qcrab01",
             "amount": 123.45678901, "percent": 9.4e-05},
            {"delta_24h": null, "is_new": null, "address": "karlsen:qplnk01",
             "amount": 1.0, "percent": 1e-06},
        ])
    );

    assert_eq!(
        request(&app, "/analytics/addresses/range").await.status,
        422
    );
    assert_eq!(
        request(&app, "/analytics/addresses/top?limit=0")
            .await
            .status,
        422
    );
    db.teardown().await;
}

#[tokio::test]
async fn distribution_summary_matches_legacy() {
    let db = db_or_skip!();
    let v = request(&db.app(), "/analytics/addresses/distribution-summary")
        .await
        .json();
    assert_eq!(v["total_addresses"], 10);
    assert_eq!(v["total_delta_1d"], 1);
    assert_eq!(v["total_delta_7d"], 3);
    // One 7-day baseline row has no balance: no total balance delta for 7d.
    assert_eq!(v["total_delta_7d_kls"], serde_json::Value::Null);
    let t = &v["tiers"];
    assert_eq!(t[0]["tier"], "10M+");
    assert_eq!(t[0]["delta_1d_kls"], 42000.0);
    assert_eq!(t[0]["delta_7d_kls"], 23456789.01234567);
    assert_eq!(t[1]["delta_7d_kls"], serde_json::Value::Null);
    assert_eq!(t[7]["tier"], "1-100");
    assert_eq!(t[7]["addresses"], 1);
    assert_eq!(t[7]["delta_7d"], -4);
    db.teardown().await;
}

#[tokio::test]
async fn address_history() {
    let db = db_or_skip!();
    let app = db.app();
    let v = request(&app, &format!("/analytics/addresses/{ADDR}/history"))
        .await
        .json();
    assert_eq!(v["address"], ADDR);
    assert_eq!(v["total"], 2);
    let txs = v["transactions"].as_array().unwrap();
    assert_eq!(txs.len(), 2);
    assert!(txs[0]["block_time"].as_i64() > txs[1]["block_time"].as_i64());
    for t in txs {
        assert_eq!(
            t["net_sompi"].as_i64().unwrap(),
            t["received_sompi"].as_i64().unwrap() - t["sent_sompi"].as_i64().unwrap()
        );
    }
    let v = request(
        &app,
        &format!("/analytics/addresses/{ADDR}/history?limit=1&offset=1"),
    )
    .await
    .json();
    assert_eq!(v["transactions"][0], txs[1].clone());
    assert_eq!(
        request(&app, "/analytics/addresses/karlsen:qbad/history")
            .await
            .status,
        422
    );
    db.teardown().await;
}
