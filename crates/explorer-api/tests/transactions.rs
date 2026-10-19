//! Transaction endpoints against rows that reproduce the legacy captures
//! (tests/fixtures/seed-transactions.sql). Needs TEST_DATABASE_URL.

#[allow(dead_code)]
mod common;

use common::{request, setup};
use serde_json::Value;

const ADDR: &str = "karlsen:qrc9rgx9x8gyekvrd7533hj3tl0cxd5r9ggxkg5l4a6xgllethrtuvp48fc3n";
const TX: &str = "f596f45c4feaa72728783d914eef424955bb33086957caddaf90ddfd0d283d37";

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

fn golden(name: &str) -> Value {
    let raw = match name {
        "full" => include_str!("fixtures/golden-address-full-txs.json"),
        "light" => include_str!("fixtures/golden-address-full-txs-light.json"),
        "fields" => include_str!("fixtures/golden-address-full-txs-fields.json"),
        "one" => include_str!("fixtures/golden-transaction.json"),
        "search" => include_str!("fixtures/golden-transactions-search.json"),
        "count" => include_str!("fixtures/golden-address-tx-count.json"),
        _ => unreachable!(),
    };
    serde_json::from_str(raw).unwrap()
}

/// Legacy drops the `karlsen:` prefix in some endpoints; we always send it.
fn with_prefix(mut v: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    if k == "script_public_key_address" || k == "previous_outpoint_address" {
                        if let Value::String(s) = x {
                            if !s.starts_with("karlsen:") {
                                *s = format!("karlsen:{s}");
                            }
                        }
                    } else {
                        walk(x);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    v
}

/// Compares key order too, not just content.
fn assert_same(got: &[u8], want: &Value) {
    let got: Value = serde_json::from_slice(got).unwrap();
    assert_eq!(got, *want);
    assert_eq!(
        serde_json::to_string(&got).unwrap(),
        serde_json::to_string(want).unwrap(),
        "key order"
    );
}

#[tokio::test]
async fn full_transactions_match_capture() {
    let db = db_or_skip!();
    let app = db.app();

    let r = request(
        &app,
        &format!("/addresses/{ADDR}/full-transactions?limit=2&offset=0"),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.json());
    assert_eq!(r.headers["cache-control"], "public, max-age=8");
    assert_same(&r.body, &with_prefix(golden("full")));

    let r = request(
        &app,
        &format!("/addresses/{ADDR}/full-transactions?limit=1&resolve_previous_outpoints=light"),
    )
    .await;
    assert_same(&r.body, &with_prefix(golden("light")));

    let r = request(
        &app,
        &format!("/addresses/{ADDR}/full-transactions?limit=1&fields=transaction_id,block_time,is_accepted"),
    )
    .await;
    assert_same(&r.body, &golden("fields"));
    db.teardown().await;
}

#[tokio::test]
async fn full_resolution_adds_the_spent_output() {
    let db = db_or_skip!();
    let r = request(
        &db.app(),
        &format!("/addresses/{ADDR}/full-transactions?limit=1&resolve_previous_outpoints=full"),
    )
    .await;
    let input = &r.json()[0]["inputs"][0];
    assert_eq!(
        input["previous_outpoint_resolved"],
        serde_json::json!({
            "transaction_id": "2cf0b2ef570fff4550a123f6664548b0bbde366f6f4483339dd7c96da5c7ace4",
            "index": 1,
            "amount": 1902023716675i64,
            "script_public_key": "20a09fa15567edcb804896d4c89476dde483e62072fa4fe30130385f4fca619f0fac",
            "script_public_key_address": "karlsen:qzsflg24vlkuhqzgjm2v39rkmhjg8e3qwtaylccpxqu97n72vx0s7faphh8a7",
            "script_public_key_type": "pubkey"
        })
    );
    // Without resolution the field is absent, not null (legacy exclude_unset).
    let r = request(
        &db.app(),
        &format!("/addresses/{ADDR}/full-transactions?limit=1"),
    )
    .await;
    assert!(r.json()[0]["inputs"][0]
        .get("previous_outpoint_resolved")
        .is_none());
    db.teardown().await;
}

#[tokio::test]
async fn transactions_count() {
    let db = db_or_skip!();
    let r = request(&db.app(), &format!("/addresses/{ADDR}/transactions-count")).await;
    assert_eq!(r.status, 200);
    // Same shape as the capture; the seed holds 2 of the address's 1294 transactions.
    assert_eq!(golden("count")["total"], 1294);
    assert_eq!(r.json(), serde_json::json!({"total": 2}));
    db.teardown().await;
}

#[tokio::test]
async fn transaction_and_search_match_capture() {
    let db = db_or_skip!();
    let app = db.app();

    let r = request(&app, &format!("/transactions/{TX}")).await;
    assert_eq!(r.status, 200, "{}", r.json());
    let mut want = golden("one");
    // Legacy reports two different block_time values for this transaction (…244 here,
    // …298 in search); the stored value is the search one. See docs/VERIFY.md.
    want["block_time"] = golden("search")[0]["block_time"].clone();
    assert_same(&r.body, &want);

    let body = format!(r#"{{"transactionIds":["{TX}"]}}"#);
    let r = common_post(&app, "/transactions/search", &body).await;
    assert_eq!(r.status, 200, "{}", r.json());
    assert_same(&r.body, &with_prefix(golden("search")));
    db.teardown().await;
}

#[tokio::test]
async fn search_rules() {
    let db = db_or_skip!();
    let app = db.app();
    let unaccepted = "ab".repeat(32);
    let both = format!(r#"{{"transactionIds":["{TX}","{unaccepted}"]}}"#);

    let ids = |r: common::Reply| -> Vec<String> {
        r.json()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["transaction_id"].as_str().unwrap().to_string())
            .collect()
    };
    let r = common_post(&app, "/transactions/search?acceptance=accepted", &both).await;
    assert_eq!(ids(r), vec![TX.to_string()]);
    let r = common_post(&app, "/transactions/search?acceptance=rejected", &both).await;
    assert_eq!(ids(r), vec![unaccepted.clone()]);

    // By accepting blue score (the block that accepted TX has blue score 93023465).
    let r = common_post(
        &app,
        "/transactions/search",
        r#"{"acceptingBlueScores":{"gte":93023465,"lt":93023466}}"#,
    )
    .await;
    assert_eq!(ids(r), vec![TX.to_string()]);

    // Limits and conflicts.
    let r = common_post(
        &app,
        "/transactions/search",
        r#"{"acceptingBlueScores":{"gte":1,"lt":1000}}"#,
    )
    .await;
    assert_eq!(r.status, 422);
    let conflict =
        format!(r#"{{"transactionIds":["{TX}"],"acceptingBlueScores":{{"gte":1,"lt":2}}}}"#);
    assert_eq!(
        common_post(&app, "/transactions/search", &conflict)
            .await
            .status,
        422
    );
    let r = common_post(&app, "/transactions/search", r#"{"transactionIds":[]}"#).await;
    assert_eq!((r.status, r.json()), (200, serde_json::json!([])));

    // Unknown transaction.
    let r = request(&app, &format!("/transactions/{}", "cd".repeat(32))).await;
    assert_eq!(
        (r.status, r.json()["detail"].as_str()),
        (404, Some("Transaction not found"))
    );
    db.teardown().await;
}

async fn common_post(app: &axum::Router, uri: &str, body: &str) -> common::Reply {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let resp = app
        .clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    common::Reply {
        status: resp.status().as_u16(),
        headers: resp.headers().clone(),
        body: resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    }
}
