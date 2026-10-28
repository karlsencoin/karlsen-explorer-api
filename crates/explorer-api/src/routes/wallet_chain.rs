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
//!   2. the largest output goes to N and carries at least 75 % of the input
//!      value (NonKYC's change was 85 % on 2026-10-22, after a 15 % withdrawal),
//!      and
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
//! Certain and likely members
//! --------------------------
//! The change link (rules 1-4) is a guess: an exchange paying out a whole
//! input with no change looks the same when one payee gets more than 75 %
//! (2026-10-27, tx 1164ae4053: qqlahash's two deposits paid out in full, the
//! larger output to qzsggnse). The common-input rule, by contrast, is proof of
//! the same keys. So every address carries `confirmed`: true when it is joined
//! to the requested address by common inputs alone (spent in one transaction
//! with it, or with an address that is, and so on), false when at least one
//! change guess is needed to reach it. The common inputs come from the linking
//! transactions and from the newest `FORWARD_CANDIDATES` transactions of every
//! member, so a change address becomes certain once the wallet spends it
//! together with the rest. `wallet-history` reports the balance of the certain
//! members separately (`confirmed_balance_sompi`).
//!
//! The chain is always rebuilt from its newest address (forward to the head,
//! then back from it), so every address of a wallet shows the same chain.
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
    /// Not a change address: spent together with the wallet in
    /// `via_transaction_id` (shown when the requested address is a
    /// collection address).
    co_input: bool,
    /// Joined to the requested address by common inputs alone (same keys,
    /// certain); false when a change guess is needed to reach it.
    confirmed: bool,
}

/// An address spent together with a chain address in a linking transaction.
#[derive(Debug, Clone, Serialize)]
pub struct CoInput {
    address: String,
    via_transaction_id: String,
    via_block_time: i64,
    /// See `ChainLink::confirmed`.
    confirmed: bool,
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
    (pays_someone_else && amount * 4 >= total_in * 3).then(|| best.to_string())
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

/// One address of the chain, oldest first.
struct Node {
    address: String,
    /// Transaction that moved the balance to this address (or, for a
    /// co-input, the one where it was spent together with the wallet).
    via: Option<(Vec<u8>, i64)>,
    co_input: bool,
}

/// Other inputs of a linking transaction, with that transaction.
type CoHit = (String, Vec<u8>, i64);

/// Walks back from `from` and returns the chain oldest first, ending with
/// `from`, plus whether the walk hit the link limit.
async fn walk_back(
    db: &PgPool,
    from: &str,
    co: &mut Vec<CoHit>,
    groups: &mut Vec<Vec<String>>,
) -> Result<(Vec<Node>, bool), ApiError> {
    let mut visited: HashSet<String> = HashSet::from([from.to_string()]);
    // Newest first while walking; reversed at the end.
    let mut nodes = vec![Node {
        address: from.to_string(),
        via: None,
        co_input: false,
    }];
    let mut truncated = false;
    loop {
        if nodes.len() > MAX_LINKS {
            truncated = true;
            break;
        }
        let cur = nodes.last().unwrap().address.clone();
        match step_back(db, &cur).await? {
            Some((parent, others, txid, bt)) if visited.insert(parent.clone()) => {
                groups.push(input_group(&parent, &others));
                for a in others {
                    co.push((a, txid.clone(), bt));
                }
                nodes.last_mut().unwrap().via = Some((txid, bt));
                nodes.push(Node {
                    address: parent,
                    via: None,
                    co_input: false,
                });
            }
            _ => break,
        }
    }
    nodes.reverse();
    Ok((nodes, truncated))
}

/// Builds the wallet chain seen from `address`.
///
/// The chain is always rebuilt from its newest address (walk forward to the
/// head, then back from there), so every address of one wallet gets the same
/// chain and the same merged history. Walking back from the requested address
/// itself gave different answers per entry point: a long-lived collection
/// address (only ever a co-input) has no change ancestry of its own, so its
/// page missed the hot wallet's earlier addresses (2026-10-24: qqlahash showed
/// 3 addresses, the hot wallet qzyvuzk3 showed 18).
async fn compute_chain(db: &PgPool, address: &str) -> Result<WalletChain, ApiError> {
    let start = stored(address);
    let mut co: Vec<CoHit> = Vec::new();
    // Input addresses of every linking transaction (common-input groups).
    let mut groups: Vec<Vec<String>> = Vec::new();

    // Walk forward from the requested address to the newest one.
    let mut later: Vec<(String, Vec<u8>, i64)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::from([start.clone()]);
    let mut cur = start.clone();
    let mut later_truncated = false;
    loop {
        if later.len() >= MAX_LINKS {
            later_truncated = true;
            break;
        }
        match step_forward(db, &cur).await? {
            Some((next, others, txid, bt)) if seen.insert(next.clone()) => {
                groups.push(input_group(&cur, &others));
                for a in others {
                    co.push((a, txid.clone(), bt));
                }
                later.push((next.clone(), txid, bt));
                cur = next;
            }
            _ => break,
        }
    }

    // Walk back from the head.
    let (mut nodes, mut earlier_truncated) = walk_back(db, &cur, &mut co, &mut groups).await?;
    if !nodes.iter().any(|n| n.address == start) {
        let spliced = later.first().and_then(|(first, txid, bt)| {
            nodes
                .iter()
                .position(|n| &n.address == first)
                .map(|j| (j, txid.clone(), *bt))
        });
        match spliced {
            // The requested address is not a change address of this wallet
            // but was spent together with it (a collection address): list it
            // just before the change address its spend created.
            Some((j, txid, bt)) => nodes.insert(
                j,
                Node {
                    address: start.clone(),
                    via: Some((txid, bt)),
                    co_input: true,
                },
            ),
            // The walk from the head did not come back this far (link limit
            // or a different parent): fall back to the requested address's
            // own ancestry followed by the forward walk.
            None => {
                let mut co_back = Vec::new();
                let (back, truncated) = walk_back(db, &start, &mut co_back, &mut groups).await?;
                co.extend(co_back);
                earlier_truncated = truncated;
                nodes = back;
                for (a, txid, bt) in &later {
                    nodes.push(Node {
                        address: a.clone(),
                        via: Some((txid.clone(), *bt)),
                        co_input: false,
                    });
                }
            }
        }
    }

    let at = nodes.iter().position(|n| n.address == start).unwrap_or(0) as i32;
    let members: HashSet<&str> = nodes.iter().map(|n| n.address.as_str()).collect();
    // An address both on the chain and spent alongside it is listed once.
    let mut co_seen: HashSet<String> = HashSet::new();
    let mut co_kept: Vec<CoHit> = Vec::new();
    for (a, txid, bt) in co {
        if co_kept.len() >= MAX_CO_INPUTS {
            break;
        }
        if !members.contains(a.as_str()) && co_seen.insert(a.clone()) {
            co_kept.push((a, txid, bt));
        }
    }

    // Which members share keys with the requested address (common inputs only).
    let all_members: Vec<String> = nodes
        .iter()
        .map(|n| n.address.clone())
        .chain(co_kept.iter().map(|(a, _, _)| a.clone()))
        .collect();
    if all_members.len() > 1 {
        groups.extend(spend_groups(db, &all_members).await?);
    }
    let same_keys = SameKeys::new(&groups);
    let confirmed = |a: &str| same_keys.joined(a, &start);

    let addresses = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| ChainLink {
            address: prefixed(&n.address),
            position: i as i32 - at,
            via_transaction_id: n.via.as_ref().map(|(t, _)| hex::encode(t)),
            via_block_time: n.via.as_ref().map(|(_, b)| *b),
            co_input: n.co_input,
            confirmed: confirmed(&n.address),
        })
        .collect();
    let co_input_addresses = co_kept
        .into_iter()
        .map(|(a, txid, bt)| CoInput {
            confirmed: confirmed(&a),
            address: prefixed(&a),
            via_transaction_id: hex::encode(txid),
            via_block_time: bt,
        })
        .collect();
    Ok(WalletChain {
        address: prefixed(&start),
        addresses,
        co_input_addresses,
        earlier_truncated,
        later_truncated,
    })
}

/// Input addresses of a linking transaction: `spender` plus `others`.
fn input_group(spender: &str, others: &[String]) -> Vec<String> {
    let mut g = Vec::with_capacity(others.len() + 1);
    g.push(spender.to_string());
    g.extend(others.iter().cloned());
    g
}

/// Common-input groups among the newest transactions of `members`: for each
/// transaction with at least two distinct resolved input addresses, one of
/// them a member, the distinct input addresses.
async fn spend_groups(db: &PgPool, members: &[String]) -> Result<Vec<Vec<String>>, ApiError> {
    let rows: Vec<(Vec<String>,)> = sqlx::query_as(
        "WITH cand AS (
             SELECT DISTINCT x.transaction_id
             FROM   unnest($1::text[]) AS m(address)
             CROSS  JOIN LATERAL (
                        SELECT a.transaction_id
                        FROM   addresses_transactions a
                        WHERE  a.address = m.address
                        -- idx_at_address_block_time (address, block_time DESC)
                        ORDER  BY a.block_time DESC
                        LIMIT  $2
                    ) AS x
         ),
         ins AS (
             SELECT c.transaction_id, (p).script_public_key_address AS address
             FROM   cand c
             JOIN   transactions t ON t.transaction_id = c.transaction_id
             CROSS  JOIN LATERAL unnest(t.inputs) AS i
             JOIN   transactions pt ON pt.transaction_id = (i).previous_outpoint_hash
             CROSS  JOIN LATERAL unnest(pt.outputs) AS p
             WHERE  (p).index = (i).previous_outpoint_index
         )
         SELECT array_agg(DISTINCT address ORDER BY address)
         FROM   ins
         WHERE  address IS NOT NULL
         GROUP  BY transaction_id
         HAVING count(DISTINCT address) >= 2 AND bool_or(address = ANY($1))",
    )
    .bind(members)
    .bind(FORWARD_CANDIDATES)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(g,)| g).collect())
}

/// Addresses joined by the common-input rule (union-find over input groups).
struct SameKeys {
    parent: HashMap<String, String>,
}

impl SameKeys {
    fn new(groups: &[Vec<String>]) -> Self {
        let mut s = SameKeys {
            parent: HashMap::new(),
        };
        for g in groups {
            for pair in g.windows(2) {
                s.union(&pair[0], &pair[1]);
            }
        }
        s
    }

    fn find(&mut self, a: &str) -> String {
        let mut root = a.to_string();
        while let Some(p) = self.parent.get(&root) {
            if *p == root {
                break;
            }
            root = p.clone();
        }
        // Path compression.
        let mut cur = a.to_string();
        while cur != root {
            let next = self
                .parent
                .get(&cur)
                .cloned()
                .unwrap_or_else(|| root.clone());
            self.parent.insert(cur, root.clone());
            cur = next;
        }
        root
    }

    fn union(&mut self, a: &str, b: &str) {
        self.parent
            .entry(a.to_string())
            .or_insert_with(|| a.to_string());
        self.parent
            .entry(b.to_string())
            .or_insert_with(|| b.to_string());
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            // Deterministic root: the smaller address.
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent.insert(hi, lo);
        }
    }

    /// Same keys: the same address, or both in one common-input component.
    fn joined(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        let root = |x: &str| {
            let mut r = x.to_string();
            loop {
                match self.parent.get(&r) {
                    Some(p) if *p != r => r = p.clone(),
                    Some(_) => return Some(r),
                    None => return None,
                }
            }
        };
        matches!((root(a), root(b)), (Some(x), Some(y)) if x == y)
    }
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
    /// The wallet address the money went to: the one receiving the largest
    /// amount in this transaction (the new change address on a hop). Falls
    /// back to the largest sender when nothing came back to the wallet.
    main_address: Option<String>,
    /// The other side of the transaction, outside the wallet: the largest
    /// sender when money came in, the largest recipient when it went out.
    counterparty_address: Option<String>,
    /// How many distinct outside addresses are on that side.
    counterparty_count: i64,
    received_sompi: i64,
    sent_sompi: i64,
    /// Net effect on the wallet as a whole: moves between its own addresses
    /// cancel out, so a change hop shows only the amount that left the wallet.
    net_sompi: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct FundedAddress {
    address: String,
    balance_sompi: i64,
    /// Certain member of the wallet (see `ChainLink::confirmed`).
    #[sqlx(skip)]
    confirmed: bool,
}

#[derive(Debug, Serialize)]
pub struct WalletHistory {
    address: String,
    chain_length: usize,
    total: i64,
    /// What the whole wallet holds now: the sum over its addresses, from the
    /// same `balances` table as the rich list (refreshed every 10 minutes).
    balance_sompi: i64,
    /// Part of `balance_sompi` held by certain members (joined to the
    /// requested address by common inputs); the rest rests on a change guess.
    confirmed_balance_sompi: i64,
    /// Wallet addresses with a balance, largest first.
    funded_addresses: Vec<FundedAddress>,
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
received_by AS (
    SELECT x.transaction_id, (o).script_public_key_address AS address, sum((o).amount) AS v
    FROM   txs x CROSS JOIN LATERAL unnest(x.outputs) AS o
    WHERE  (o).script_public_key_address = ANY($1)
    GROUP  BY 1, 2
),
sent_by AS (
    SELECT x.transaction_id, (p).script_public_key_address AS address, sum((p).amount) AS v
    FROM   txs x
    CROSS  JOIN LATERAL unnest(x.inputs) AS i
    JOIN   transactions pt ON pt.transaction_id = (i).previous_outpoint_hash
    CROSS  JOIN LATERAL unnest(pt.outputs) AS p
    WHERE  (p).index = (i).previous_outpoint_index
      AND  (p).script_public_key_address = ANY($1)
    GROUP  BY 1, 2
),
main AS (
    SELECT DISTINCT ON (transaction_id) transaction_id, address
    FROM   (SELECT transaction_id, address, v, 0 AS pref FROM received_by
            UNION ALL
            SELECT transaction_id, address, v, 1 FROM sent_by) m
    ORDER  BY transaction_id, pref, v DESC, address
),
ext_in AS (
    -- Outside senders (resolved inputs not owned by the wallet)
    SELECT x.transaction_id, (p).script_public_key_address AS address, sum((p).amount) AS v
    FROM   txs x
    CROSS  JOIN LATERAL unnest(x.inputs) AS i
    JOIN   transactions pt ON pt.transaction_id = (i).previous_outpoint_hash
    CROSS  JOIN LATERAL unnest(pt.outputs) AS p
    WHERE  (p).index = (i).previous_outpoint_index
      AND  NOT ((p).script_public_key_address = ANY($1))
    GROUP  BY 1, 2
),
ext_out AS (
    -- Outside recipients
    SELECT x.transaction_id, (o).script_public_key_address AS address, sum((o).amount) AS v
    FROM   txs x CROSS JOIN LATERAL unnest(x.outputs) AS o
    WHERE  NOT ((o).script_public_key_address = ANY($1))
    GROUP  BY 1, 2
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
       mn.address                                    AS main_address,
       cp.address                                    AS counterparty_address,
       COALESCE(cp.n, 0)                             AS counterparty_count,
       COALESCE(r.v, 0)::bigint                      AS received_sompi,
       COALESCE(s.v, 0)::bigint                      AS sent_sompi,
       (COALESCE(r.v, 0) - COALESCE(s.v, 0))::bigint AS net_sompi
FROM   txs x
LEFT   JOIN received r ON r.transaction_id = x.transaction_id
LEFT   JOIN sent     s ON s.transaction_id = x.transaction_id
LEFT   JOIN touched tc ON tc.transaction_id = x.transaction_id
LEFT   JOIN main     mn ON mn.transaction_id = x.transaction_id
LEFT   JOIN LATERAL (
           -- Money in: who sent it. Money out (or a hop): who received it.
           SELECT e.address, count(*) OVER () AS n
           FROM   (SELECT address, v FROM ext_in
                   WHERE  transaction_id = x.transaction_id
                     AND  COALESCE(r.v, 0) > COALESCE(s.v, 0)
                   UNION ALL
                   SELECT address, v FROM ext_out
                   WHERE  transaction_id = x.transaction_id
                     AND  COALESCE(r.v, 0) <= COALESCE(s.v, 0)) e
           ORDER  BY e.v DESC, e.address
           LIMIT  1
       ) cp ON true
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

    // The balances table may store addresses with or without the prefix.
    let with_prefix: Vec<String> = members.iter().map(|a| prefixed(a)).collect();
    let mut funded_addresses: Vec<FundedAddress> = sqlx::query_as(
        "SELECT address::text AS address, amount AS balance_sompi
         FROM   balances
         WHERE  (address = ANY($1) OR address = ANY($2)) AND amount > 0
         ORDER  BY amount DESC, address",
    )
    .bind(&members)
    .bind(&with_prefix)
    .fetch_all(&state.db)
    .await?;
    let certain: HashSet<String> = chain
        .addresses
        .iter()
        .filter(|l| l.confirmed)
        .map(|l| l.address.clone())
        .chain(
            chain
                .co_input_addresses
                .iter()
                .filter(|c| c.confirmed)
                .map(|c| c.address.clone()),
        )
        .collect();
    for f in &mut funded_addresses {
        f.address = prefixed(&f.address);
        f.confirmed = certain.contains(&f.address);
    }
    funded_addresses.dedup_by(|a, b| a.address == b.address);
    let balance_sompi = funded_addresses.iter().map(|f| f.balance_sompi).sum();
    let confirmed_balance_sompi = funded_addresses
        .iter()
        .filter(|f| f.confirmed)
        .map(|f| f.balance_sompi)
        .sum();

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
        if let Some(m) = &mut t.main_address {
            *m = prefixed(m);
        }
        if let Some(c) = &mut t.counterparty_address {
            *c = prefixed(c);
        }
    }
    Ok((
        cache_header(60),
        Json(WalletHistory {
            address: chain.address.clone(),
            chain_length: members.len(),
            total,
            balance_sompi,
            confirmed_balance_sompi,
            funded_addresses,
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
    fn change_needs_dominant_output() {
        let v = view(&[("p", 1000)], &[("w", 10), ("n", 989)]);
        assert_eq!(change_target(&v, "p").as_deref(), Some("n"));
        // Mainnet 2026-10-22: 85 % change after a large withdrawal still counts.
        let v = view(&[("p", 1000)], &[("w", 150), ("n", 849)]);
        assert_eq!(change_target(&v, "p").as_deref(), Some("n"));
        // Below 75 %: could be a customer withdrawal, no link.
        let v = view(&[("p", 1000)], &[("w", 260), ("n", 739)]);
        assert_eq!(change_target(&v, "p"), None);
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
    fn same_keys_only_through_common_inputs() {
        let s = SameKeys::new(&[
            vec!["h0".into(), "q".into()],
            vec!["h1".into(), "q".into()],
            vec!["x".into(), "y".into()],
        ]);
        assert!(s.joined("h0", "h1"));
        assert!(s.joined("q", "h0"));
        assert!(!s.joined("h0", "x"));
        assert!(s.joined("x", "y"));
        // Never spent with anything: only itself.
        assert!(s.joined("h2", "h2"));
        assert!(!s.joined("h2", "q"));
        assert!(!s.joined("h2", "z"));
    }

    #[test]
    fn prefix_helpers() {
        assert_eq!(stored("karlsen:qabc"), "qabc");
        assert_eq!(stored("qabc"), "qabc");
        assert_eq!(prefixed("qabc"), "karlsen:qabc");
        assert_eq!(prefixed("karlsen:qabc"), "karlsen:qabc");
    }
}
