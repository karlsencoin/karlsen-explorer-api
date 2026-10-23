//! `GET /addresses/{address}/wallet-chain` and
//! `GET /addresses/{address}/wallet-history`.
//!
//! Many wallets (exchange hot wallets above all) send the change of every
//! payment to a freshly derived address, so one wallet's history is spread over
//! a chain of addresses that each live for a single spend. These endpoints
//! rebuild that chain for any address and serve the merged history.
//!
//! A transaction links address P to address N ("N is P's change") when:
//!   1. P is one of its inputs and every input is resolved; inputs from other
//!      addresses are allowed (see below),
//!   2. the largest output goes to N and carries at least 90 % of the input
//!      value, and
//!   3. N has never appeared in any earlier transaction, and
//!   4. the transaction also pays at least one other address, and nothing
//!      returns to any of its input addresses. Moving a whole balance to a
//!      single new address, or paying a new address with the change kept on
//!      an input address, is a transfer between wallets (for example funding a
//!      hot wallet), not change.
//!
//! Addresses spent together in one transaction are controlled by the same keys
//! (common-input rule), so the other inputs of a linking transaction belong to
//! the wallet too: exchanges spend their deposit-collection address together
//! with the hot wallet. They are returned as `co_input_addresses` and included
//! in the merged history. When walking back, the parent is the input address
//! that appeared most recently, because the hot wallet's previous change
//! address is younger than a long-lived collection address.
//!
//! It is a heuristic: it is stated as "likely the same wallet" in the explorer.
//!
//! The chain is computed on request with a few indexed lookups per link and
//! kept in memory for `CHAIN_TTL`; nothing is written to the database. Walks
//! stop after `MAX_LINKS` links in each direction and report truncation.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::{error::ApiError, routes::addresses::validate, AppState};

const MAX_LINKS: usize = 100;
/// Co-input addresses kept per chain (they are listed and merged into history).
const MAX_CO_INPUTS: usize = 200;
/// Transactions of an address inspected when looking for the spend that moved
/// its balance on (newest first). Change addresses have one or two.
const FORWARD_CANDIDATES: i64 = 20;
const CHAIN_TTL: Duration = Duration::from_secs(600);
const CHAIN_CACHE_CAP: usize = 2_000;
const PREFIX: &str = "karlsen:";

fn stored(address: &str) -> String {
    address
        .split_once(':')
        .map_or(address, |(_, rest)| rest)
        .to_string()
}

fn prefixed(address: &str) -> String {
    if address.starts_with(PREFIX) {
        address.to_string()
    } else {
        format!("{PREFIX}{address}")
    }
}

fn cache_header(max_age: u64) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_str(&format!("public, max-age={max_age}")).unwrap(),
    );
    h
}

// ---- chain detection ----

#[derive(Debug, Clone, Serialize)]
pub struct ChainLink {
    address: String,
    /// Negative: earlier addresses of the wallet, 0: the requested one,
    /// positive: later ones.
    position: i32,
    /// Transaction that moved the balance from the previous address to this
    /// one (none for the oldest address found).
    via_transaction_id: Option<String>,
    via_block_time: Option<i64>,
}

/// An address spent together with a chain address in a linking transaction.
#[derive(Debug, Clone, Serialize)]
pub struct CoInput {
    address: String,
    via_transaction_id: String,
    via_block_time: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalletChain {
    address: String,
    addresses: Vec<ChainLink>,
    /// Other addresses of the same wallet found through the common-input
    /// rule (not on the change chain itself).
    co_input_addresses: Vec<CoInput>,
    /// The walk back stopped at the link limit, not at the wallet's start.
    earlier_truncated: bool,
    later_truncated: bool,
}

struct TxView {
    /// Resolved inputs (address, amount); `None` when some input's previous
    /// output is not indexed, which ends the walk.
    inputs: Option<Vec<(String, i64)>>,
    outputs: Vec<(String, i64)>,
}

async fn tx_view(db: &PgPool, txid: &[u8]) -> Result<TxView, ApiError> {
    let (n_inputs,): (Option<i32>,) =
        sqlx::query_as("SELECT cardinality(inputs) FROM transactions WHERE transaction_id = $1")
            .bind(txid)
            .fetch_one(db)
            .await?;
    let inputs: Vec<(Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT (p).script_public_key_address, (p).amount
         FROM   transactions t
         CROSS  JOIN LATERAL unnest(t.inputs) AS i
         JOIN   transactions pt ON pt.transaction_id = (i).previous_outpoint_hash
         CROSS  JOIN LATERAL unnest(pt.outputs) AS p
         WHERE  t.transaction_id = $1 AND (p).index = (i).previous_outpoint_index",
    )
    .bind(txid)
    .fetch_all(db)
    .await?;
    let outputs: Vec<(Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT (o).script_public_key_address, (o).amount
         FROM   transactions t CROSS JOIN LATERAL unnest(t.outputs) AS o
         WHERE  t.transaction_id = $1",
    )
    .bind(txid)
    .fetch_all(db)
    .await?;
    let resolved: Vec<(String, i64)> = inputs
        .into_iter()
        .filter_map(|(a, v)| Some((a?, v?)))
        .collect();
    let complete = n_inputs.unwrap_or(0) > 0 && resolved.len() == n_inputs.unwrap_or(0) as usize;
    Ok(TxView {
        inputs: complete.then_some(resolved),
        outputs: outputs
            .into_iter()
            .filter_map(|(a, v)| Some((a?, v?)))
            .collect(),
    })
}

/// If `from` is one of the (fully resolved) inputs and the dominant output
/// (rule 2) goes to an address that is not an input, returns that address.
fn change_target(view: &TxView, from: &str) -> Option<String> {
    let inputs = view.inputs.as_ref()?;
    if !inputs.iter().any(|(a, _)| a == from) {
        return None;
    }
    let senders: HashSet<&str> = inputs.iter().map(|(a, _)| a.as_str()).collect();
    let total_in: i128 = inputs.iter().map(|(_, v)| *v as i128).sum();
    let mut per_addr: HashMap<&str, i128> = HashMap::new();
    for (a, v) in &view.outputs {
        *per_addr.entry(a.as_str()).or_default() += *v as i128;
    }
    // Change that returns to a sender means the new address was paid, not
    // given the change (e.g. a round 100,000 KLS funding with 14 KLS back).
    if per_addr.keys().any(|a| senders.contains(a)) {
        return None;
    }
    let pays_someone_else = per_addr.len() >= 2;
    // Ties broken by address so the result does not depend on hash order.
    let (best, amount) = per_addr
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))?;
    (pays_someone_else && amount * 10 >= total_in * 9).then(|| best.to_string())
}

/// Input addresses of `view` other than `except` (sorted, deduplicated).
fn other_inputs(view: &TxView, except: &str) -> Vec<String> {
    let mut v: Vec<String> = view
        .inputs
        .iter()
        .flatten()
        .map(|(a, _)| a.clone())
        .filter(|a| a != except)
        .collect();
    v.sort();
    v.dedup();
    v
}

async fn first_seen(db: &PgPool, address: &str) -> Result<i64, ApiError> {
    let (t,): (Option<i64>,) =
        sqlx::query_as("SELECT min(block_time) FROM addresses_transactions WHERE address = $1")
            .bind(address)
            .fetch_one(db)
            .await?;
    Ok(t.unwrap_or(i64::MIN))
}

async fn seen_before(db: &PgPool, address: &str, block_time: i64) -> Result<bool, ApiError> {
    let (seen,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM addresses_transactions
                        WHERE address = $1 AND block_time < $2)",
    )
    .bind(address)
    .bind(block_time)
    .fetch_one(db)
    .await?;
    Ok(seen)
}

/// One link of the walk: the next address, the other inputs of the linking
/// transaction, the transaction and its block time.
type Step = (String, Vec<String>, Vec<u8>, i64);

/// The address whose change created `address`, with the linking transaction.
async fn step_back(db: &PgPool, address: &str) -> Result<Option<Step>, ApiError> {
    let first: Option<(Vec<u8>, Option<i64>)> = sqlx::query_as(
        "SELECT transaction_id, block_time FROM addresses_transactions
         WHERE  address = $1 AND block_time IS NOT NULL
         ORDER  BY block_time ASC LIMIT 1",
    )
    .bind(address)
    .fetch_optional(db)
    .await?;
    let Some((txid, Some(bt))) = first else {
        return Ok(None);
    };
    let view = tx_view(db, &txid).await?;
    let Some(inputs) = view.inputs.as_ref() else {
        return Ok(None);
    };
    let mut sources: Vec<String> = inputs.iter().map(|(a, _)| a.clone()).collect();
    sources.sort();
    sources.dedup();
    // Rule 3 holds by construction: this is `address`'s first transaction.
    if change_target(&view, &sources[0]).as_deref() != Some(address) {
        return Ok(None);
    }
    // Parent: the input address that appeared most recently (the previous
    // change address), not a long-lived address spent along with it.
    let mut parent = sources[0].clone();
    let mut parent_seen = i64::MIN;
    for a in &sources {
        let seen = first_seen(db, a).await?;
        if seen > parent_seen {
            parent_seen = seen;
            parent = a.clone();
        }
    }
    let co = other_inputs(&view, &parent);
    Ok(Some((parent, co, txid, bt)))
}

/// The address `address`'s balance moved on to as change, with the transaction.
async fn step_forward(db: &PgPool, address: &str) -> Result<Option<Step>, ApiError> {
    let recent: Vec<(Vec<u8>, Option<i64>)> = sqlx::query_as(
        "SELECT transaction_id, block_time FROM addresses_transactions
         WHERE  address = $1 AND block_time IS NOT NULL
         ORDER  BY block_time DESC LIMIT $2",
    )
    .bind(address)
    .bind(FORWARD_CANDIDATES)
    .fetch_all(db)
    .await?;
    // Newest first: the latest move is the one that matters.
    for (txid, bt) in recent {
        let bt = bt.unwrap_or_default();
        let view = tx_view(db, &txid).await?;
        if let Some(next) = change_target(&view, address) {
            if !seen_before(db, &next, bt).await? {
                let co = other_inputs(&view, address);
                return Ok(Some((next, co, txid, bt)));
            }
        }
    }
    Ok(None)
}

async fn compute_chain(db: &PgPool, address: &str) -> Result<WalletChain, ApiError> {
    let start = stored(address);
    let mut visited: HashSet<String> = HashSet::from([start.clone()]);

    // Walk back. Each element: (address, tx that created it from the older one).
    let mut earlier: Vec<(String, Option<(Vec<u8>, i64)>)> = Vec::new();
    let mut created_by: Option<(Vec<u8>, i64)> = None;
    let mut cur = start.clone();
    let mut earlier_truncated = false;
    let mut co: Vec<CoInput> = Vec::new();
    let mut co_seen: HashSet<String> = HashSet::new();
    let mut add_co = |list: Vec<String>, txid: &[u8], bt: i64| {
        for a in list {
            if co.len() < MAX_CO_INPUTS && co_seen.insert(a.clone()) {
                co.push(CoInput {
                    address: a,
                    via_transaction_id: hex::encode(txid),
                    via_block_time: bt,
                });
            }
        }
    };
    loop {
        if earlier.len() >= MAX_LINKS {
            earlier_truncated = true;
            break;
        }
        match step_back(db, &cur).await? {
            Some((parent, others, txid, bt)) if visited.insert(parent.clone()) => {
                add_co(others, &txid, bt);
                if earlier.is_empty() {
                    created_by = Some((txid, bt));
                } else {
                    earlier.last_mut().unwrap().1 = Some((txid, bt));
                }
                earlier.push((parent.clone(), None));
                cur = parent;
            }
            _ => break,
        }
    }

    // Walk forward.
    let mut later: Vec<(String, Vec<u8>, i64)> = Vec::new();
    let mut cur = start.clone();
    let mut later_truncated = false;
    loop {
        if later.len() >= MAX_LINKS {
            later_truncated = true;
            break;
        }
        match step_forward(db, &cur).await? {
            Some((next, others, txid, bt)) if visited.insert(next.clone()) => {
                add_co(others, &txid, bt);
                later.push((next.clone(), txid, bt));
                cur = next;
            }
            _ => break,
        }
    }

    let hex_link = |l: &Option<(Vec<u8>, i64)>| {
        (
            l.as_ref().map(|(t, _)| hex::encode(t)),
            l.as_ref().map(|(_, b)| *b),
        )
    };
    let mut addresses = Vec::with_capacity(earlier.len() + 1 + later.len());
    for (i, (a, link)) in earlier.iter().enumerate().rev() {
        let (via, bt) = hex_link(link);
        addresses.push(ChainLink {
            address: prefixed(a),
            position: -(i as i32) - 1,
            via_transaction_id: via,
            via_block_time: bt,
        });
    }
    let (via, bt) = hex_link(&created_by);
    addresses.push(ChainLink {
        address: prefixed(&start),
        position: 0,
        via_transaction_id: via,
        via_block_time: bt,
    });
    for (i, (a, txid, bt)) in later.iter().enumerate() {
        addresses.push(ChainLink {
            address: prefixed(a),
            position: i as i32 + 1,
            via_transaction_id: Some(hex::encode(txid)),
            via_block_time: Some(*bt),
        });
    }
    // An address both on the chain and spent alongside it is listed once.
    let mut co_input_addresses: Vec<CoInput> = co
        .into_iter()
        .filter(|c| !visited.contains(&c.address))
        .collect();
    for c in &mut co_input_addresses {
        c.address = prefixed(&c.address);
    }
    Ok(WalletChain {
        address: prefixed(&start),
        addresses,
        co_input_addresses,
        earlier_truncated,
        later_truncated,
    })
}

// ---- in-memory cache, single flight per address ----

type Slot = Arc<tokio::sync::Mutex<Option<(Instant, Arc<WalletChain>)>>>;

fn slots() -> &'static Mutex<HashMap<String, Slot>> {
    static SLOTS: OnceLock<Mutex<HashMap<String, Slot>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn chain_for(db: &PgPool, address: &str) -> Result<Arc<WalletChain>, ApiError> {
    let key = stored(address);
    let slot = {
        let mut map = slots().lock().unwrap();
        if map.len() >= CHAIN_CACHE_CAP && !map.contains_key(&key) {
            map.retain(|_, s| {
                s.try_lock().map_or(true, |g| {
                    g.as_ref().is_some_and(|(at, _)| at.elapsed() < CHAIN_TTL)
                })
            });
            if map.len() >= CHAIN_CACHE_CAP {
                map.clear();
            }
        }
        map.entry(key.clone()).or_default().clone()
    };
    let mut guard = slot.lock().await;
    if let Some((at, chain)) = guard.as_ref() {
        if at.elapsed() < CHAIN_TTL {
            return Ok(chain.clone());
        }
    }
    let chain = Arc::new(compute_chain(db, &key).await?);
    *guard = Some((Instant::now(), chain.clone()));
    Ok(chain)
}

pub async fn wallet_chain(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> Result<(HeaderMap, Json<WalletChain>), ApiError> {
    validate(&address)?;
    let chain = chain_for(&state.db, &address).await?;
    Ok((cache_header(300), Json((*chain).clone())))
}

// ---- merged history ----

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct WalletHistoryEntry {
    transaction_id: String,
    hash: String,
    block_time: i64,
    /// Chain addresses this transaction touches (prefixed).
    addresses: Vec<String>,
    received_sompi: i64,
    sent_sompi: i64,
    /// Net effect on the wallet as a whole: moves between its own addresses
    /// cancel out, so a change hop shows only the amount that left the wallet.
    net_sompi: i64,
}

#[derive(Debug, Serialize)]
pub struct WalletHistory {
    address: String,
    chain_length: usize,
    total: i64,
    transactions: Vec<WalletHistoryEntry>,
}

const WALLET_HISTORY_SQL: &str = "
WITH newest AS (
    -- Per address, only the newest (offset + limit) rows, read from the
    -- (address, block_time) index. The page is always inside their union, so
    -- a pool address with millions of transactions costs one index range scan.
    SELECT m.address, x.transaction_id, x.block_time
    FROM   unnest($1::text[]) AS m(address)
    CROSS  JOIN LATERAL (
               SELECT a.transaction_id, a.block_time
               FROM   addresses_transactions a
               WHERE  a.address = m.address
               -- Must match idx_at_address_block_time (address, block_time
               -- DESC), whose DESC means NULLS FIRST; NULLS LAST would sort all
               -- 15 M rows of a pool address.
               ORDER  BY a.block_time DESC
               LIMIT  $2 + $3
           ) AS x
),
page AS (
    SELECT transaction_id, max(block_time) AS block_time
    FROM   newest
    GROUP  BY transaction_id
    ORDER  BY max(block_time) DESC NULLS LAST, transaction_id
    LIMIT  $2 OFFSET $3
),
txs AS (
    SELECT t.transaction_id, t.hash, t.block_time, t.inputs, t.outputs
    FROM   page p JOIN transactions t ON t.transaction_id = p.transaction_id
),
received AS (
    SELECT x.transaction_id, sum((o).amount) AS v
    FROM   txs x CROSS JOIN LATERAL unnest(x.outputs) AS o
    WHERE  (o).script_public_key_address = ANY($1)
    GROUP  BY x.transaction_id
),
sent AS (
    SELECT x.transaction_id, sum((p).amount) AS v
    FROM   txs x
    CROSS  JOIN LATERAL unnest(x.inputs) AS i
    JOIN   transactions pt ON pt.transaction_id = (i).previous_outpoint_hash
    CROSS  JOIN LATERAL unnest(pt.outputs) AS p
    WHERE  (p).index = (i).previous_outpoint_index
      AND  (p).script_public_key_address = ANY($1)
    GROUP  BY x.transaction_id
),
touched AS (
    -- Every chain address a page transaction touches is among that address's
    -- newest rows already read above.
    SELECT n.transaction_id, array_agg(DISTINCT n.address) AS addrs
    FROM   newest n JOIN page p ON p.transaction_id = n.transaction_id
    GROUP  BY n.transaction_id
)
SELECT encode(x.transaction_id, 'hex')               AS transaction_id,
       COALESCE(encode(x.hash, 'hex'), '')           AS hash,
       COALESCE(x.block_time, 0)                     AS block_time,
       COALESCE(tc.addrs, '{}')                      AS addresses,
       COALESCE(r.v, 0)::bigint                      AS received_sompi,
       COALESCE(s.v, 0)::bigint                      AS sent_sompi,
       (COALESCE(r.v, 0) - COALESCE(s.v, 0))::bigint AS net_sompi
FROM   txs x
LEFT   JOIN received r ON r.transaction_id = x.transaction_id
LEFT   JOIN sent     s ON s.transaction_id = x.transaction_id
LEFT   JOIN touched tc ON tc.transaction_id = x.transaction_id
ORDER  BY x.block_time DESC NULLS LAST, x.transaction_id";

pub async fn wallet_history(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Result<(HeaderMap, Json<WalletHistory>), ApiError> {
    validate(&address)?;
    let limit = q.limit.unwrap_or(25);
    let offset = q.offset.unwrap_or(0);
    if !(1..=200).contains(&limit) || offset < 0 {
        return Err(ApiError::Validation(
            "limit must be 1..=200 and offset >= 0".into(),
        ));
    }
    let chain = chain_for(&state.db, &address).await?;
    let members: Vec<String> = chain
        .addresses
        .iter()
        .map(|l| stored(&l.address))
        .chain(chain.co_input_addresses.iter().map(|c| stored(&c.address)))
        .collect();

    let total = if members.len() == 1 {
        // A lone address: reuse the per-address count cache.
        let (db, key) = (state.db.clone(), members[0].clone());
        state
            .tx_counts
            .get_or_load(&members[0], || async move {
                let (n,): (i64,) = sqlx::query_as(
                    "SELECT count(*) FROM addresses_transactions WHERE address = $1",
                )
                .bind(&key)
                .fetch_one(&db)
                .await?;
                Ok::<_, ApiError>(n)
            })
            .await?
            .0
    } else {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(DISTINCT transaction_id) FROM addresses_transactions WHERE address = ANY($1)",
        )
        .bind(&members)
        .fetch_one(&state.db)
        .await?;
        n
    };

    let mut transactions: Vec<WalletHistoryEntry> = sqlx::query_as(WALLET_HISTORY_SQL)
        .bind(&members)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
    for t in &mut transactions {
        for a in &mut t.addresses {
            *a = prefixed(a);
        }
    }
    Ok((
        cache_header(60),
        Json(WalletHistory {
            address: chain.address.clone(),
            chain_length: members.len(),
            total,
            transactions,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(inputs: &[(&str, i64)], outputs: &[(&str, i64)]) -> TxView {
        TxView {
            inputs: Some(inputs.iter().map(|(a, v)| (a.to_string(), *v)).collect()),
            outputs: outputs.iter().map(|(a, v)| (a.to_string(), *v)).collect(),
        }
    }

    #[test]
    fn change_needs_single_source_and_90_percent() {
        let v = view(&[("p", 1000)], &[("w", 10), ("n", 989)]);
        assert_eq!(change_target(&v, "p").as_deref(), Some("n"));
        // Even split: no dominant output.
        let v = view(&[("p", 1000)], &[("x", 499), ("y", 500)]);
        assert_eq!(change_target(&v, "p"), None);
        // Mixed sources, single output: a transfer, not change.
        let v = view(&[("p", 500), ("q", 500)], &[("n", 999)]);
        assert_eq!(change_target(&v, "p"), None);
        // Hot wallet spent together with another address of the same wallet
        // (common-input rule): the change hop still counts, from either input.
        let v = view(&[("p", 1000), ("q", 4)], &[("w", 15), ("n", 988)]);
        assert_eq!(change_target(&v, "p").as_deref(), Some("n"));
        assert_eq!(change_target(&v, "q").as_deref(), Some("n"));
        assert_eq!(other_inputs(&v, "p"), vec!["q".to_string()]);
        // Not an input: no link.
        assert_eq!(change_target(&v, "x"), None);
        // Change back to the co-input address is not a hop.
        let v = view(&[("p", 1000), ("q", 4)], &[("q", 990), ("w", 13)]);
        assert_eq!(change_target(&v, "p"), None);
        // Sweeping the whole balance to one new address is a wallet transfer.
        let v = view(&[("p", 1000)], &[("n", 999)]);
        assert_eq!(change_target(&v, "p"), None);
        // Paying a new address while the change stays on the sender.
        let v = view(&[("p", 1000)], &[("n", 985), ("p", 14)]);
        assert_eq!(change_target(&v, "p"), None);
        // Change back to the same address is not a hop.
        let v = view(&[("p", 1000)], &[("p", 990), ("w", 9)]);
        assert_eq!(change_target(&v, "p"), None);
        // Unresolved inputs end the walk.
        let v = TxView {
            inputs: None,
            outputs: vec![("n".into(), 1)],
        };
        assert_eq!(change_target(&v, "p"), None);
    }

    #[test]
    fn prefix_helpers() {
        assert_eq!(stored("karlsen:qabc"), "qabc");
        assert_eq!(stored("qabc"), "qabc");
        assert_eq!(prefixed("qabc"), "karlsen:qabc");
        assert_eq!(prefixed("karlsen:qabc"), "karlsen:qabc");
    }
}
