//! deploy/sql/address_tags_auto.sql run with psql against a seeded database,
//! then served by /analytics/address-tags/auto.
//!
//! Exchange "NonKYC", hand-tagged collection address Q:
//!
//!   TX1: Q + H0      -> W1 (500)  + H1 (9499)   H0 co-input, H1 change
//!   TX2: H1 + Q      -> W2 (400)  + H2 (9198)   H2 change; H1 spent with Q
//!   TX3: Q + Q       -> X1 (1,179,212.69) + X2 (9,658,252.31)
//!                       mainnet 2026-10-27 (tx 1164ae4053): two deposits paid
//!                       out in full, no change; X2 passes the change rules
//!   TX4: H2 + P      -> W3 (500)  + H3 (8997)   P co-input of an unconfirmed tag
//!   TX5: P + Q       -> W4 (10)   + H4 (239)    P spent with Q
//!
//! Expected: H0, H1, P, H2 confirmed (H2 only through P, a second round);
//! H3, H4 and X2 unconfirmed: nothing proves they belong to the exchange.
//!
//! Needs TEST_DATABASE_URL and psql on PATH.

#[allow(dead_code)]
mod common;

use std::{collections::BTreeMap, process::Command};

use common::{get, setup};
use karlsen_explorer_api::address::{encode, Version, MAINNET_PREFIX};

/// A valid address (stored form, without the prefix) from a payload byte.
fn addr(n: u8) -> String {
    encode(MAINNET_PREFIX, Version::PubKey, &[n; 32])
        .trim_start_matches("karlsen:")
        .to_string()
}
fn txid(n: u8) -> String {
    format!("c0ffee{:058x}", 0xa7000u32 + n as u32)
}
fn input(prev: &str, idx: i32) -> String {
    format!("ROW(0, decode('{prev}','hex'), {idx}, NULL, 1, NULL, NULL)::transactions_inputs")
}
fn output(idx: i32, amount: i64, addr: &str) -> String {
    format!("ROW({idx}, {amount}, NULL, '{addr}')::transactions_outputs")
}
/// Inserts the transaction and its addresses_transactions rows (inputs'
/// addresses are passed in `spenders`).
fn tx(
    id: &str,
    time: i64,
    inputs: &[String],
    outputs: &[(i64, &str)],
    spenders: &[&str],
) -> String {
    let outs: Vec<String> = outputs
        .iter()
        .enumerate()
        .map(|(i, (v, a))| output(i as i32, *v, a))
        .collect();
    let mut sql = format!(
        "INSERT INTO transactions (transaction_id, subnetwork_id, hash, mass, payload, block_time, inputs, outputs)
         VALUES (decode('{id}','hex'), 0, decode('{id}','hex'), 1000, NULL, {time}, ARRAY[{}], ARRAY[{}]);\n",
        inputs.join(","),
        outs.join(",")
    );
    let mut seen: Vec<&str> = Vec::new();
    for a in spenders
        .iter()
        .copied()
        .chain(outputs.iter().map(|(_, a)| *a))
    {
        if !seen.contains(&a) {
            seen.push(a);
            sql.push_str(&format!(
                "INSERT INTO addresses_transactions VALUES ('{a}', decode('{id}','hex'), {time});\n"
            ));
        }
    }
    sql
}

fn run_job(url: &str, tags_file: &str) -> String {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/sql/address_tags_auto.sql"
    );
    let out = Command::new("psql")
        .args(["-X", "-q", "-v", "ON_ERROR_STOP=1", "-d", url])
        .args(["-v", &format!("tags_file={tags_file}")])
        // The whole seeded history is inside the scan window.
        .args(["-v", "hours=876000", "-f", script])
        .output()
        .expect("run psql");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "psql failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[tokio::test]
async fn change_guesses_stay_unconfirmed_until_spent_with_the_exchange() {
    let Some(db) = setup().await else {
        eprintln!("TEST_DATABASE_URL not set: skipping");
        return;
    };
    let (q, h0, h1, h2, h3, h4, p) = (
        addr(201),
        addr(202),
        addr(203),
        addr(204),
        addr(205),
        addr(206),
        addr(207),
    );
    let (w1, w2, w3, w4, x1, x2) = (
        addr(211),
        addr(212),
        addr(213),
        addr(214),
        addr(215),
        addr(216),
    );
    let missing = "00000000000000000000000000000000000000000000000000000000000000a7";
    let (fq, fh, fp) = (txid(1), txid(2), txid(3));
    let (tx1, tx2, tx3, tx4, tx5) = (txid(11), txid(12), txid(13), txid(14), txid(15));

    let seed = [
        // Deposits into Q (funding inputs not indexed).
        tx(
            &fq,
            1_000,
            &[input(missing, 0)],
            &[
                (1000, &q),
                (100, &q),
                (574_170_200_000_000, &q),
                (509_576_300_000_000, &q),
                (50, &q),
            ],
            &[],
        ),
        tx(&fh, 2_000, &[input(missing, 1)], &[(9000, &h0)], &[]),
        tx(
            &tx1,
            3_000,
            &[input(&fq, 0), input(&fh, 0)],
            &[(500, &w1), (9499, &h1)],
            &[&q, &h0],
        ),
        tx(
            &tx2,
            4_000,
            &[input(&tx1, 1), input(&fq, 1)],
            &[(400, &w2), (9198, &h2)],
            &[&h1, &q],
        ),
        tx(
            &tx3,
            4_500,
            &[input(&fq, 2), input(&fq, 3)],
            &[(117_921_269_319_520, &x1), (965_825_230_660_480, &x2)],
            &[&q],
        ),
        tx(
            &fp,
            4_600,
            &[input(missing, 2)],
            &[(300, &p), (200, &p)],
            &[],
        ),
        tx(
            &tx4,
            5_000,
            &[input(&tx2, 1), input(&fp, 0)],
            &[(500, &w3), (8997, &h3)],
            &[&h2, &p],
        ),
        tx(
            &tx5,
            6_000,
            &[input(&fp, 1), input(&fq, 4)],
            &[(10, &w4), (239, &h4)],
            &[&p, &q],
        ),
        // A table written by the previous job version (no confirmed column),
        // already holding the X2 guess.
        format!(
            "CREATE TABLE address_tags_auto (
                 address text PRIMARY KEY, label text NOT NULL, parent_address text NOT NULL,
                 transaction_id bytea NOT NULL, tagged_at timestamptz NOT NULL DEFAULT now(),
                 kind text NOT NULL DEFAULT 'change');
             INSERT INTO address_tags_auto (address, label, parent_address, transaction_id)
             VALUES ('{x2}', 'NonKYC', '{q}', decode('{tx3}','hex'));"
        ),
    ]
    .join("\n");
    sqlx::raw_sql(&seed).execute(&db.pool).await.unwrap();
    let app = db.app();
    let k = |a: &str| format!("karlsen:{a}");

    // Before the new job has run: the old table is served, nothing confirmed.
    let (s, v) = get(&app, "/analytics/address-tags/auto").await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v[k(&x2)], "NonKYC (unconfirmed)", "{v}");

    let tags_file = std::env::temp_dir().join(format!("address-tags-{}.json", std::process::id()));
    std::fs::write(
        &tags_file,
        format!(r#"{{"karlsen:{q}": "NonKYC (deposit collector)", "karlsen:{w4}": "Some pool"}}"#),
    )
    .unwrap();
    let tags_file = tags_file.to_string_lossy().to_string();

    let rows_now = || async {
        let rows: Vec<(String, String, bool, Option<String>)> = sqlx::query_as(
            "SELECT address, kind, confirmed, encode(confirmed_by, 'hex')
             FROM address_tags_auto ORDER BY address",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        rows.into_iter()
            .map(|(a, kind, c, by)| (a, (kind, c, by)))
            .collect::<BTreeMap<_, _>>()
    };

    run_job(&db.url, &tags_file);
    let rows = rows_now().await;
    let want: BTreeMap<String, (String, bool, Option<String>)> = [
        (&h0, "co-input", true, Some(&tx1)),
        (&h1, "change", true, Some(&tx2)),
        // Change of a confirmed address, then spent together with P, which is
        // confirmed only by TX5: needs the second confirmation round.
        (&h2, "change", true, Some(&tx4)),
        (&p, "co-input", true, Some(&tx5)),
        (&h3, "change", false, None),
        (&h4, "change", false, None),
        // The full payout of TX3: tagged by the change rules, never proven.
        (&x2, "change", false, None),
    ]
    .into_iter()
    .map(|(a, kind, c, by)| (a.clone(), (kind.to_string(), c, by.cloned())))
    .collect();
    assert_eq!(rows, want);

    // Served labels: plain name only when confirmed. A fresh app: responses
    // are cached per app state.
    let (s, v) = get(&db.app(), "/analytics/address-tags/auto").await;
    assert_eq!(s, 200, "{v}");
    let served: BTreeMap<String, String> = serde_json::from_value(v).unwrap();
    let want_served: BTreeMap<String, String> = [
        (&h0, "NonKYC"),
        (&h1, "NonKYC"),
        (&h2, "NonKYC"),
        (&p, "NonKYC"),
        (&h3, "NonKYC (unconfirmed)"),
        (&h4, "NonKYC (unconfirmed)"),
        (&x2, "NonKYC (unconfirmed)"),
    ]
    .into_iter()
    .map(|(a, l)| (k(a), l.to_string()))
    .collect();
    assert_eq!(served, want_served);
    // Payees and the hand-tagged address are not derived tags.
    for a in [&q, &w1, &w2, &w3, &w4, &x1] {
        assert!(!served.contains_key(&k(a)), "{a} must not be tagged");
    }

    // A second run changes nothing.
    run_job(&db.url, &tags_file);
    assert_eq!(rows_now().await, want);

    // H3 is spent together with a confirmed address later: it confirms.
    let (fx, tx6) = (txid(4), txid(16));
    let w5 = addr(217);
    let later = [
        tx(&fx, 6_500, &[input(missing, 3)], &[(5, &h1)], &[]),
        tx(
            &tx6,
            7_000,
            &[input(&tx4, 1), input(&fx, 0)],
            &[(2, &w5), (9000, &addr(218))],
            &[&h3, &h1],
        ),
    ]
    .join("\n");
    sqlx::raw_sql(&later).execute(&db.pool).await.unwrap();
    run_job(&db.url, &tags_file);
    let rows = rows_now().await;
    assert_eq!(rows[&h3], ("change".to_string(), true, Some(tx6.clone())));
    // Its change is a new guess.
    assert_eq!(rows[&addr(218)], ("change".to_string(), false, None));
    assert!(!rows[&x2].1);

    // A transaction that also spends another exchange's address proves
    // nothing: X2 spent with Q and a Xeggex address stays unconfirmed.
    let xg = addr(219);
    std::fs::write(
        &tags_file,
        format!(
            r#"{{"karlsen:{q}": "NonKYC (deposit collector)", "karlsen:{w4}": "Some pool", "karlsen:{xg}": "Xeggex"}}"#
        ),
    )
    .unwrap();
    let (fy, tx7) = (txid(5), txid(17));
    let mixed = [
        tx(&fy, 7_500, &[input(missing, 4)], &[(7, &q), (8, &xg)], &[]),
        tx(
            &tx7,
            8_000,
            &[input(&tx3, 1), input(&fy, 0), input(&fy, 1)],
            &[(965_825_230_660_495, &addr(220))],
            &[&x2, &q, &xg],
        ),
    ]
    .join("\n");
    sqlx::raw_sql(&mixed).execute(&db.pool).await.unwrap();
    run_job(&db.url, &tags_file);
    let rows = rows_now().await;
    assert_eq!(rows[&x2], ("change".to_string(), false, None));
    assert!(!rows.contains_key(&addr(220)));

    let _ = std::fs::remove_file(&tags_file);
    db.teardown().await;
}
