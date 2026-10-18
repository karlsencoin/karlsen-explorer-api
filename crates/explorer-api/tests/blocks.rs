mod common;
use common::{get, setup};

const BLOCK: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CB_ADDR: &str = "karlsen:qpjnywfxsdrj3nkuscf4mc0m457wt35sgfgfv379zuuekl6z0ulwkkgayg83r";
const OTHER_ADDR: &str = "karlsen:qqe3p64wpjf5y27kxppxrgks298ge6lhu6ws7ndx4tswzj7c84qkjlrspcuxw";

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

#[tokio::test]
async fn block_with_transactions_matches_legacy_shape() {
    let db = db_or_skip!();
    let app = db.app();
    let (status, v) = get(&app, &format!("/blocks/{BLOCK}")).await;
    assert_eq!(status, 200, "{v}");

    let h = &v["header"];
    assert_eq!(h["blueScore"], "1000");
    assert_eq!(h["daaScore"], "999");
    assert_eq!(h["timestamp"], "1700000000000");
    assert_eq!(h["blueWork"], "abcdef");
    assert_eq!(h["nonce"], "255");
    assert_eq!(h["bits"], 453115903);
    assert_eq!(h["parents"][0]["parentHashes"][0], "bb".repeat(32));

    let vd = &v["verboseData"];
    assert_eq!(vd["hash"], BLOCK);
    assert_eq!(vd["isChainBlock"], true);
    assert_eq!(vd["childrenHashes"][0], "cc".repeat(32));
    assert_eq!(vd["mergeSetBluesHashes"][0], "bb".repeat(32));
    // Coinbase first, then the regular transaction.
    assert_eq!(vd["transactionIds"][0], "01".repeat(32));
    assert_eq!(vd["transactionIds"][1], "02".repeat(32));

    let txs = v["transactions"].as_array().unwrap();
    assert_eq!(txs.len(), 2);
    // The address is derived from the script, not from the stored text column.
    assert_eq!(
        txs[0]["outputs"][0]["verboseData"]["scriptPublicKeyAddress"],
        CB_ADDR
    );
    assert_eq!(
        txs[0]["outputs"][0]["verboseData"]["scriptPublicKeyType"],
        "pubkey"
    );
    assert_eq!(
        txs[1]["outputs"][0]["verboseData"]["scriptPublicKeyAddress"],
        OTHER_ADDR
    );
    assert_eq!(txs[1]["outputs"][1]["amount"], 39990000);
    assert_eq!(
        txs[1]["inputs"][0]["previousOutpoint"]["transactionId"],
        "01".repeat(32)
    );
    assert_eq!(txs[1]["verboseData"]["blockHash"], BLOCK);
    // Always present in legacy responses, all null without includeColor.
    assert_eq!(
        v["extra"],
        serde_json::json!({"color": null, "minerAddress": null, "minerInfo": null})
    );
    db.teardown().await;
}

#[tokio::test]
async fn include_transactions_false_omits_them() {
    let db = db_or_skip!();
    let (status, v) = get(
        &db.app(),
        &format!("/blocks/{BLOCK}?includeTransactions=false"),
    )
    .await;
    assert_eq!(status, 200);
    assert!(v.get("transactions").is_none());
    assert_eq!(
        v["verboseData"]["transactionIds"].as_array().unwrap().len(),
        2
    );
    db.teardown().await;
}

#[tokio::test]
async fn unknown_block_is_404_and_bad_id_is_422() {
    let db = db_or_skip!();
    let app = db.app();
    let (s, v) = get(&app, &format!("/blocks/{}", "ee".repeat(32))).await;
    assert_eq!((s, v["detail"].as_str()), (404, Some("Block not found")));
    let (s, _) = get(&app, "/blocks/not-a-hash").await;
    assert_eq!(s, 422);
    let (s, _) = get(&app, &format!("/blocks/{BLOCK}?includeColor=true")).await;
    assert_eq!(s, 501);
    db.teardown().await;
}

#[tokio::test]
async fn health_ok_and_database_outage_is_a_real_503() {
    let db = db_or_skip!();
    let (s, v) = get(&db.app(), "/info/health").await;
    assert_eq!((s, v["database"].as_str()), (200, Some("ok")));
    db.teardown().await;

    // Unreachable database must surface as 503, never as an empty success.
    let dead = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(300))
        .connect_lazy("postgres://u:p@127.0.0.1:1/none")
        .unwrap();
    let app = karlsen_explorer_api::routes::router(karlsen_explorer_api::AppState { db: dead });
    let (s, v) = get(&app, "/info/health").await;
    assert_eq!(s, 503);
    assert_eq!(v["detail"], "Service temporarily unavailable");
}
