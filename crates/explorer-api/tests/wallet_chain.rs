//! Wallet change-chain endpoints against a small seeded chain:
//!
//!   F (funding, inputs not indexed) -> A
//!   tx1: A -> W1 (10) + B (989, change)
//!   tx2: B -> C (979, change) + W2 (10)
//!   tx3: W2 -> C (10)   a deposit into C, not a hop
//!
//! Needs TEST_DATABASE_URL.

#[allow(dead_code)]
mod common;

use common::{get, setup};

const A: &str = "qrnr2c48k5qknd9een407r34wtt3clnr4qw58d6d2fj03sq9844d23pteq43h";
const B: &str = "qzw885rjk49m5rftn6yqyk0853uhuj9dpxauqq855v5ku3agwhjpjagtvtpex";
const C: &str = "qptmu6evslw7mlv76tq5svg45cp8v72nfgd0veldenxqg50yxh757c2tytxwp";
const W1: &str = "qp4m3zzexccwgxvwhkwp3rsmsdw4yhewvt4jqv3kwlclr0t2fu4a2w2v3yh4p";
const W2: &str = "qpz5ttt3kxgpvpjm94e2sm686v20c6zjqhugwggyt75f5l2vhn22s5ddl25lx";

const F: &str = "c0ffee00000000000000000000000000000000000000000000000000000000f0";
const TX1: &str = "c0ffee00000000000000000000000000000000000000000000000000000000f1";
const TX2: &str = "c0ffee00000000000000000000000000000000000000000000000000000000f2";
const TX3: &str = "c0ffee00000000000000000000000000000000000000000000000000000000f3";

fn input(prev: &str, idx: i32) -> String {
    format!("ROW(0, decode('{prev}','hex'), {idx}, NULL, 1, NULL, NULL)::transactions_inputs")
}
fn output(idx: i32, amount: i64, addr: &str) -> String {
    format!("ROW({idx}, {amount}, NULL, '{addr}')::transactions_outputs")
}
fn tx(id: &str, time: i64, inputs: &[String], outputs: &[String]) -> String {
    let ins = if inputs.is_empty() {
        "'{}'::transactions_inputs[]".to_string()
    } else {
        format!("ARRAY[{}]", inputs.join(","))
    };
    format!(
        "INSERT INTO transactions (transaction_id, subnetwork_id, hash, mass, payload, block_time, inputs, outputs)
         VALUES (decode('{id}','hex'), 0, decode('{id}','hex'), 1000, NULL, {time}, {ins}, ARRAY[{}]);",
        outputs.join(",")
    )
}
fn link(addr: &str, id: &str, time: i64) -> String {
    format!("INSERT INTO addresses_transactions VALUES ('{addr}', decode('{id}','hex'), {time});")
}

fn seed() -> String {
    // Funding input points at a transaction that is not indexed.
    let missing = "00000000000000000000000000000000000000000000000000000000000000aa";
    [
        tx(F, 1_000, &[input(missing, 0)], &[output(0, 1000, A)]),
        tx(
            TX1,
            2_000,
            &[input(F, 0)],
            &[output(0, 10, W1), output(1, 989, B)],
        ),
        tx(
            TX2,
            3_000,
            &[input(TX1, 1)],
            &[output(0, 979, C), output(1, 10, W2)],
        ),
        tx(TX3, 4_000, &[input(TX2, 1)], &[output(0, 10, C)]),
        link(A, F, 1_000),
        link(A, TX1, 2_000),
        link(W1, TX1, 2_000),
        link(B, TX1, 2_000),
        link(B, TX2, 3_000),
        link(C, TX2, 3_000),
        link(W2, TX2, 3_000),
        link(W2, TX3, 4_000),
        link(C, TX3, 4_000),
    ]
    .join("\n")
}

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
async fn chain_and_merged_history() {
    let db = db_or_skip!();
    sqlx::raw_sql(&seed()).execute(&db.pool).await.unwrap();
    let app = db.app();

    // From the newest address: walks back to A, nothing later.
    let (s, v) = get(&app, &format!("/addresses/karlsen:{C}/wallet-chain")).await;
    assert_eq!(s, 200, "{v}");
    let addrs: Vec<(String, i64, Option<String>)> = v["addresses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["address"].as_str().unwrap().to_string(),
                l["position"].as_i64().unwrap(),
                l["via_transaction_id"].as_str().map(str::to_string),
            )
        })
        .collect();
    assert_eq!(
        addrs,
        vec![
            (format!("karlsen:{A}"), -2, None),
            (format!("karlsen:{B}"), -1, Some(TX1.to_string())),
            (format!("karlsen:{C}"), 0, Some(TX2.to_string())),
        ]
    );
    assert_eq!(v["earlier_truncated"], false);
    assert_eq!(v["later_truncated"], false);

    // From the oldest address: walks forward to C.
    let (s, v) = get(&app, &format!("/addresses/karlsen:{A}/wallet-chain")).await;
    assert_eq!(s, 200, "{v}");
    let positions: Vec<(String, i64)> = v["addresses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["address"].as_str().unwrap().to_string(),
                l["position"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        positions,
        vec![
            (format!("karlsen:{A}"), 0),
            (format!("karlsen:{B}"), 1),
            (format!("karlsen:{C}"), 2),
        ]
    );

    // A payee is not part of the wallet.
    let (s, v) = get(&app, &format!("/addresses/karlsen:{W1}/wallet-chain")).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["addresses"].as_array().unwrap().len(), 1);

    // Merged history from the middle address: four transactions, newest first,
    // nets relative to the whole wallet (hops only lose what left it).
    let (s, v) = get(
        &app,
        &format!("/addresses/karlsen:{B}/wallet-history?limit=10"),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["chain_length"], 3);
    assert_eq!(v["total"], 4);
    let rows: Vec<(String, i64)> = v["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["transaction_id"].as_str().unwrap().to_string(),
                t["net_sompi"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (TX3.to_string(), 10),
            (TX2.to_string(), -10),
            (TX1.to_string(), -11),
            (F.to_string(), 1000),
        ]
    );

    // Paging.
    let (_, v) = get(
        &app,
        &format!("/addresses/karlsen:{B}/wallet-history?limit=2&offset=2"),
    )
    .await;
    let ids: Vec<&str> = v["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["transaction_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![TX1, F]);

    db.teardown().await;
}

/// Common-input rule, as on mainnet 2026-10-22 (qptmu6 + qrunj7 -> qqzqv2q4):
///
///   FQ -> Q   (500)  long-lived collection address, appears first
///   FH -> H0  (1000) hot wallet change address
///   TXC: H0 + Q -> W (225) + H1 (1274, change: 85 %, as on mainnet)
#[tokio::test]
async fn co_input_keeps_the_chain() {
    let db = db_or_skip!();
    const Q: &str = "qqlahashstjepzax3sa33vdn6a7tul5wvqs8adzgrr6lzt280ctw5my64p427";
    // Distinct from the first test: chains are cached in-process by address.
    const H0: &str = "qrunj7nwjnstv0txm0sq5zca8ccjyhevhxm4pgf5ms66c2eruvel7pvynevep";
    const H1: &str = "qqzqv2q4yku6eev5w3m2f8nn4x5kjy8krwntvy7rraqdr4tfyvg6qh5qlf8qp";
    const W: &str = "qq8prrl9uq3826m5nvvdqucwgdlsz4qhlpm4nuk94l7sprd9kwwwkl9jtgenp";
    const FQ: &str = "c0ffee000000000000000000000000000000000000000000000000000000000a";
    const FH: &str = "c0ffee000000000000000000000000000000000000000000000000000000000b";
    const TXC: &str = "c0ffee000000000000000000000000000000000000000000000000000000000c";
    let missing = "00000000000000000000000000000000000000000000000000000000000000bb";
    let seed = [
        tx(FQ, 1_000, &[input(missing, 0)], &[output(0, 500, Q)]),
        tx(FH, 2_000, &[input(missing, 1)], &[output(0, 1000, H0)]),
        tx(
            TXC,
            3_000,
            &[input(FH, 0), input(FQ, 0)],
            &[output(0, 225, W), output(1, 1274, H1)],
        ),
        link(Q, FQ, 1_000),
        link(H0, FH, 2_000),
        link(H0, TXC, 3_000),
        link(Q, TXC, 3_000),
        link(W, TXC, 3_000),
        link(H1, TXC, 3_000),
    ]
    .join("\n");
    sqlx::raw_sql(&seed).execute(&db.pool).await.unwrap();
    let app = db.app();

    // Back from the new change address: the parent is the hot wallet (the
    // younger input), the collection address is a co-input.
    let (s, v) = get(&app, &format!("/addresses/karlsen:{H1}/wallet-chain")).await;
    assert_eq!(s, 200, "{v}");
    let chain: Vec<&str> = v["addresses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["address"].as_str().unwrap())
        .collect();
    assert_eq!(
        chain,
        vec![format!("karlsen:{H0}"), format!("karlsen:{H1}")]
    );
    let co = &v["co_input_addresses"];
    assert_eq!(co.as_array().unwrap().len(), 1, "{v}");
    assert_eq!(co[0]["address"], format!("karlsen:{Q}"));
    assert_eq!(co[0]["via_transaction_id"], TXC);

    // Forward from the old hot address reaches the same wallet.
    let (_, v) = get(&app, &format!("/addresses/karlsen:{H0}/wallet-chain")).await;
    assert_eq!(v["addresses"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(
        v["co_input_addresses"][0]["address"],
        format!("karlsen:{Q}")
    );

    // Merged history covers all three addresses; the hop only loses the payment.
    let (s, v) = get(
        &app,
        &format!("/addresses/karlsen:{H1}/wallet-history?limit=10"),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["chain_length"], 3);
    assert_eq!(v["total"], 3);
    let nets: Vec<(String, i64)> = v["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["transaction_id"].as_str().unwrap().to_string(),
                t["net_sompi"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        nets,
        vec![
            (TXC.to_string(), -226),
            (FH.to_string(), 1000),
            (FQ.to_string(), 500)
        ]
    );
    // The highlighted address is where the money went: the new change address
    // on the hop, the receiver on a deposit.
    let mains: Vec<&str> = v["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["main_address"].as_str().unwrap())
        .collect();
    let want = [H1, H0, Q].map(|a| format!("karlsen:{a}"));
    assert_eq!(mains, want);
    // The other side: the withdrawal's payee on the hop; the funding inputs
    // of the deposits are not indexed, so they have none.
    let cps: Vec<(Option<&str>, i64)> = v["transactions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["counterparty_address"].as_str(), t["counterparty_count"].as_i64().unwrap()))
        .collect();
    let w = format!("karlsen:{W}");
    assert_eq!(cps, vec![(Some(w.as_str()), 1), (None, 0), (None, 0)]);

    // The payee stays outside the wallet.
    let (_, v) = get(&app, &format!("/addresses/karlsen:{W}/wallet-chain")).await;
    assert_eq!(v["addresses"].as_array().unwrap().len(), 1);
    assert_eq!(v["co_input_addresses"].as_array().unwrap().len(), 0);

    db.teardown().await;
}

/// Every address of one wallet gets the same chain and the same history, as
/// on mainnet 2026-10-24 (qqlahash showed 3 addresses, qzyvuzk3 showed 18):
///
///   FQ  -> Q   (500)  collection address, only ever a co-input
///   FH  -> H0  (2000) hot wallet
///   TXA: H0 -> W1 (20) + H1 (1979, change)
///   TXB: H1 + Q -> W2 (200) + H2 (2278, change)
///
/// Walking back from Q itself finds nothing (FQ's input is not indexed), so
/// before the fix Q's page listed only Q, H1 and H2 and missed H0.
#[tokio::test]
async fn every_wallet_address_sees_the_same_wallet() {
    let db = db_or_skip!();
    // Distinct from the other tests: chains are cached in-process by address.
    const Q: &str = "qzyvuzk39n40wzsxyqvlvtvxlltfs676anlw3hj5pt7h77x45huuxum76m3d6";
    const H0: &str = "qqtesgavw0ugx0yskmqwq27ltu0uyw8z0lk3t624dju7hwjeeqpngd4627kch";
    const H1: &str = "qpmt70zmv7ajtqezsw8e5hpch2z6z2xfwaavcwxngm2c7r0hntw06yft69rup";
    const H2: &str = "qzj3vtrsdq6td7a2dpnlv0l5zh70t3v8mpczv679zhjvjcut94n022hxw06h7";
    const W1: &str = "qzth4yaka5tlyr5de59cjmhgjwt5r4dqqg5h8huzjd0zusltasks2qd30vdve";
    const W2: &str = "qp4p5rve9matgyrgcllwm0dnqeea8mq8yfflgx5gp04zrpuksw57kcuc7cqv2";
    const FQ: &str = "c0ffee0000000000000000000000000000000000000000000000000000000d01";
    const FH: &str = "c0ffee0000000000000000000000000000000000000000000000000000000d02";
    const TXA: &str = "c0ffee0000000000000000000000000000000000000000000000000000000d03";
    const TXB: &str = "c0ffee0000000000000000000000000000000000000000000000000000000d04";
    let missing = "00000000000000000000000000000000000000000000000000000000000000dd";
    let seed = [
        tx(FQ, 1_000, &[input(missing, 0)], &[output(0, 500, Q)]),
        tx(FH, 2_000, &[input(missing, 1)], &[output(0, 2000, H0)]),
        tx(TXA, 3_000, &[input(FH, 0)], &[output(0, 20, W1), output(1, 1979, H1)]),
        tx(
            TXB,
            4_000,
            &[input(TXA, 1), input(FQ, 0)],
            &[output(0, 200, W2), output(1, 2278, H2)],
        ),
        link(Q, FQ, 1_000),
        link(H0, FH, 2_000),
        link(H0, TXA, 3_000),
        link(W1, TXA, 3_000),
        link(H1, TXA, 3_000),
        link(H1, TXB, 4_000),
        link(Q, TXB, 4_000),
        link(W2, TXB, 4_000),
        link(H2, TXB, 4_000),
        // Current balances, one stored with the prefix and one without.
        format!("INSERT INTO balances (id, address, amount) VALUES (9001, 'karlsen:{H2}', 2278), (9002, '{Q}', 50);"),
    ]
    .join("\n");
    sqlx::raw_sql(&seed).execute(&db.pool).await.unwrap();
    let app = db.app();

    let chain_of = |v: &serde_json::Value| -> Vec<(String, i64, bool)> {
        v["addresses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l["address"].as_str().unwrap().trim_start_matches("karlsen:").to_string(),
                    l["position"].as_i64().unwrap(),
                    l["co_input"].as_bool().unwrap(),
                )
            })
            .collect()
    };

    // From the collection address: the hot wallet's whole chain, with Q
    // listed where it was spent together with it.
    let (s, v) = get(&app, &format!("/addresses/karlsen:{Q}/wallet-chain")).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        chain_of(&v),
        vec![
            (H0.to_string(), -2, false),
            (H1.to_string(), -1, false),
            (Q.to_string(), 0, true),
            (H2.to_string(), 1, false),
        ]
    );
    assert_eq!(v["addresses"][2]["via_transaction_id"], TXB);

    // From an old hot wallet address and from the newest one: same chain.
    for (addr, at) in [(H0, 0), (H2, 2)] {
        let (s, v) = get(&app, &format!("/addresses/karlsen:{addr}/wallet-chain")).await;
        assert_eq!(s, 200, "{v}");
        let c = chain_of(&v);
        let names: Vec<&str> = c.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(names, vec![H0, H1, H2], "from {addr}");
        assert_eq!(c[at].1, 0, "from {addr}");
        assert_eq!(v["co_input_addresses"][0]["address"], format!("karlsen:{Q}"));
    }

    // Same members, same history and the same wallet balance from all of them.
    let mut seen = Vec::new();
    for addr in [Q, H0, H1, H2] {
        let (s, v) = get(
            &app,
            &format!("/addresses/karlsen:{addr}/wallet-history?limit=10"),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        let ids: Vec<String> = v["transactions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["transaction_id"].as_str().unwrap().to_string())
            .collect();
        seen.push((
            v["chain_length"].as_i64().unwrap(),
            v["total"].as_i64().unwrap(),
            v["balance_sompi"].as_i64().unwrap(),
            ids,
        ));
        assert_eq!(v["funded_addresses"][0]["address"], format!("karlsen:{H2}"));
        assert_eq!(v["funded_addresses"][1]["address"], format!("karlsen:{Q}"));
    }
    assert_eq!(seen[0].0, 4);
    assert_eq!(seen[0].1, 4);
    assert_eq!(seen[0].2, 2278 + 50);
    assert!(seen.iter().all(|x| x == &seen[0]), "{seen:?}");

    // Payees stay outside.
    let (_, v) = get(&app, &format!("/addresses/karlsen:{W2}/wallet-chain")).await;
    assert_eq!(v["addresses"].as_array().unwrap().len(), 1);

    db.teardown().await;
}
