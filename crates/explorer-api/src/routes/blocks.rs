//! `GET /blocks/{blockId}` — wire-compatible with legacy `rest-v2`.
//!
//! Same strategy as the Kaspa REST server: ask karlsend first, because only the
//! node has every parent level, the real transaction `mass` and the block color.
//! When the node does not have the block (pruned, header only, or the node is
//! down), the response is built from the database and marked with
//! `X-Data-Source: Database`. See docs/VERIFY.md for what differs in that case.

use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Path, Query, State},
    http::{header::CACHE_CONTROL, HeaderMap, HeaderValue},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};

use crate::{
    address::{script_to_address, script_type, MAINNET_PREFIX},
    difficulty::difficulty_from_bits,
    error::ApiError,
    miner::parse_coinbase_payload,
    node::{proto, NodeClient},
    AppState,
};

const COINBASE_SUBNETWORK: &str = "0100000000000000000000000000000000000000";
/// Upper bound on blocks visited when looking for the chain block that merged a
/// block (database color lookup).
const COLOR_SEARCH_LIMIT: usize = 64;

#[derive(Debug, Deserialize)]
pub struct BlockQuery {
    #[serde(rename = "includeTransactions", default = "default_true")]
    include_transactions: bool,
    #[serde(rename = "includeColor", default)]
    include_color: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, FromRow)]
struct BlockRow {
    hash: Vec<u8>,
    accepted_id_merkle_root: Option<Vec<u8>>,
    merge_set_blues_hashes: Option<Vec<Vec<u8>>>,
    merge_set_reds_hashes: Option<Vec<Vec<u8>>>,
    selected_parent_hash: Option<Vec<u8>>,
    bits: Option<i64>,
    blue_score: Option<i64>,
    blue_work: Option<Vec<u8>>,
    daa_score: Option<i64>,
    hash_merkle_root: Option<Vec<u8>>,
    nonce: Option<Vec<u8>>,
    pruning_point: Option<Vec<u8>>,
    timestamp: Option<i64>,
    utxo_commitment: Option<Vec<u8>>,
    version: Option<i16>,
}

#[derive(Debug, FromRow)]
struct TxRow {
    transaction_id: Vec<u8>,
    hash: Option<Vec<u8>>,
    mass: Option<i32>,
    payload: Option<Vec<u8>>,
    block_time: Option<i64>,
    subnetwork_id: Option<String>,
}

#[derive(Debug, FromRow)]
struct InputRow {
    transaction_id: Vec<u8>,
    previous_outpoint_hash: Option<Vec<u8>>,
    previous_outpoint_index: Option<i16>,
    signature_script: Option<Vec<u8>>,
    sig_op_count: Option<i16>,
}

#[derive(Debug, FromRow)]
struct OutputRow {
    transaction_id: Vec<u8>,
    amount: Option<i64>,
    script_public_key: Option<Vec<u8>>,
}

#[derive(Debug, FromRow)]
struct MergeRow {
    hash: Vec<u8>,
    merge_set_blues_hashes: Option<Vec<Vec<u8>>>,
    merge_set_reds_hashes: Option<Vec<Vec<u8>>>,
    is_chain: bool,
}

// ---- response model (field names follow the legacy OpenAPI document) ----

#[derive(Debug, Serialize)]
pub struct BlockModel {
    header: Header,
    /// `null` (not absent) with `includeTransactions=false`, as in legacy responses.
    transactions: Option<Vec<TxModel>>,
    #[serde(rename = "verboseData")]
    verbose_data: VerboseData,
    extra: Extra,
}

/// Always present in legacy responses.
#[derive(Debug, Default, Serialize)]
struct Extra {
    color: Option<&'static str>,
    #[serde(rename = "minerAddress")]
    miner_address: Option<String>,
    #[serde(rename = "minerInfo")]
    miner_info: Option<String>,
}

#[derive(Debug, Serialize)]
struct Header {
    version: i32,
    #[serde(rename = "hashMerkleRoot")]
    hash_merkle_root: String,
    #[serde(rename = "acceptedIdMerkleRoot")]
    accepted_id_merkle_root: String,
    #[serde(rename = "utxoCommitment")]
    utxo_commitment: String,
    timestamp: String,
    bits: i64,
    nonce: String,
    #[serde(rename = "daaScore")]
    daa_score: String,
    #[serde(rename = "blueWork")]
    blue_work: String,
    parents: Vec<ParentHashes>,
    #[serde(rename = "blueScore")]
    blue_score: String,
    #[serde(rename = "pruningPoint")]
    pruning_point: String,
}

#[derive(Debug, Serialize)]
struct ParentHashes {
    #[serde(rename = "parentHashes")]
    parent_hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct VerboseData {
    hash: String,
    difficulty: f64,
    #[serde(rename = "selectedParentHash")]
    selected_parent_hash: String,
    #[serde(rename = "transactionIds")]
    transaction_ids: Vec<String>,
    #[serde(rename = "blueScore")]
    blue_score: String,
    #[serde(rename = "childrenHashes")]
    children_hashes: Vec<String>,
    #[serde(rename = "mergeSetBluesHashes")]
    merge_set_blues_hashes: Vec<String>,
    #[serde(rename = "mergeSetRedsHashes")]
    merge_set_reds_hashes: Vec<String>,
    #[serde(rename = "isChainBlock")]
    is_chain_block: bool,
}

#[derive(Debug, Serialize)]
struct TxModel {
    inputs: Vec<InputModel>,
    outputs: Vec<OutputModel>,
    #[serde(rename = "subnetworkId")]
    subnetwork_id: String,
    payload: String,
    #[serde(rename = "verboseData")]
    verbose_data: TxVerbose,
    #[serde(rename = "lockTime")]
    lock_time: u64,
    gas: u64,
    mass: i64,
    version: i32,
}

#[derive(Debug, Serialize)]
struct InputModel {
    #[serde(rename = "previousOutpoint")]
    previous_outpoint: Outpoint,
    #[serde(rename = "signatureScript")]
    signature_script: String,
    #[serde(rename = "sigOpCount")]
    sig_op_count: i32,
    sequence: u64,
}

#[derive(Debug, Serialize)]
struct Outpoint {
    #[serde(rename = "transactionId")]
    transaction_id: String,
    index: i32,
}

#[derive(Debug, Serialize)]
struct OutputModel {
    amount: i64,
    #[serde(rename = "scriptPublicKey")]
    script_public_key: ScriptPublicKey,
    #[serde(rename = "verboseData")]
    verbose_data: OutputVerbose,
}

#[derive(Debug, Serialize)]
struct ScriptPublicKey {
    #[serde(rename = "scriptPublicKey")]
    script_public_key: String,
    version: i32,
}

#[derive(Debug, Serialize)]
struct OutputVerbose {
    #[serde(rename = "scriptPublicKeyType")]
    script_public_key_type: &'static str,
    #[serde(rename = "scriptPublicKeyAddress")]
    script_public_key_address: String,
}

#[derive(Debug, Serialize)]
struct TxVerbose {
    #[serde(rename = "transactionId")]
    transaction_id: String,
    hash: String,
    #[serde(rename = "computeMass")]
    compute_mass: i64,
    #[serde(rename = "blockHash")]
    block_hash: String,
    #[serde(rename = "blockTime")]
    block_time: i64,
}

// ---- helpers ----

fn hx(b: &Option<Vec<u8>>) -> String {
    b.as_deref().map(hex::encode).unwrap_or_default()
}

fn hx_all(v: &Option<Vec<Vec<u8>>>) -> Vec<String> {
    v.iter().flatten().map(hex::encode).collect()
}

/// Lowercase hex without leading zeros (`"0"` for zero).
fn trim_hex(h: &str) -> String {
    let t = h.trim_start_matches('0');
    if t.is_empty() {
        "0".to_string()
    } else {
        t.to_string()
    }
}

/// Big-endian bytes without leading zero bytes, as lowercase hex.
fn trimmed_hex(b: &Option<Vec<u8>>) -> String {
    trim_hex(&hx(b))
}

fn nonce_decimal(b: &Option<Vec<u8>>) -> String {
    let n = b
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .fold(0u128, |a, &x| (a << 8) | x as u128);
    n.to_string()
}

/// Legacy lists the selected parent first. The rest of the order is not stored, so it
/// is sorted by hash to keep responses stable.
fn order_parents(selected: &str, mut parents: Vec<String>) -> Vec<String> {
    parents.sort_by(|a, b| {
        (a.as_str() != selected)
            .cmp(&(b.as_str() != selected))
            .then_with(|| a.cmp(b))
    });
    parents
}

/// Output model with the script type and address derived from the raw script.
fn output_model(amount: i64, script: &[u8]) -> OutputModel {
    OutputModel {
        amount,
        verbose_data: OutputVerbose {
            script_public_key_type: script_type(script),
            script_public_key_address: script_to_address(MAINNET_PREFIX, script)
                .unwrap_or_default(),
        },
        script_public_key: ScriptPublicKey {
            script_public_key: hex::encode(script),
            version: 0,
        },
    }
}

/// Miner address and tag from the coinbase payload (first coinbase transaction).
fn miner_from_transactions(txs: &[TxModel]) -> (Option<String>, Option<String>) {
    txs.iter()
        .find(|t| t.subnetwork_id == COINBASE_SUBNETWORK)
        .and_then(|t| hex::decode(&t.payload).ok())
        .and_then(|p| parse_coinbase_payload(&p))
        .map_or((None, None), |(a, i)| (Some(a), Some(i)))
}

fn color_name(blue: bool) -> &'static str {
    if blue {
        "blue"
    } else {
        "red"
    }
}

// ---- handler ----

pub async fn get_block(
    State(state): State<AppState>,
    Path(block_id): Path<String>,
    Query(q): Query<BlockQuery>,
) -> Result<(HeaderMap, Json<BlockModel>), ApiError> {
    if block_id.len() != 64
        || !block_id
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ApiError::Validation(
            "blockId must be 64 lowercase hex characters".into(),
        ));
    }

    let mut headers = HeaderMap::new();
    if let Some(node) = &state.node {
        match node.get_block(&block_id, q.include_transactions).await {
            Ok(Some(block)) => {
                let mut model = block_from_node(block, q.include_transactions);
                if q.include_color {
                    model.extra.color = color_from_node(node, &model).await;
                }
                return Ok((headers, Json(model)));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "karlsend unavailable, using the database"),
        }
    }

    let mut model = block_from_db(&state.db, &block_id, q.include_transactions).await?;
    if q.include_color {
        model.extra.color = if model.verbose_data.is_chain_block {
            Some("blue")
        } else {
            color_from_db(&state.db, &block_id).await?
        };
    }
    headers.insert("x-data-source", HeaderValue::from_static("Database"));
    Ok((headers, Json(model)))
}

// ---- GET /blocks ----

#[derive(Debug, Deserialize)]
pub struct BlocksQuery {
    #[serde(rename = "lowHash")]
    low_hash: Option<String>,
    #[serde(rename = "includeBlocks", default)]
    include_blocks: bool,
    #[serde(rename = "includeTransactions", default)]
    include_transactions: bool,
}

/// Legacy validated `lowHash` with an unanchored `[a-f0-9]{64}` search.
fn contains_hex64(s: &str) -> bool {
    s.as_bytes()
        .split(|c| !matches!(c, b'0'..=b'9' | b'a'..=b'f'))
        .any(|run| run.len() >= 64)
}

/// Blocks from `lowHash` towards the tip, straight from karlsend (`getBlocks`).
/// Like legacy, `extra` is `null` here and any node failure answers with empty
/// lists rather than an error.
pub async fn list_blocks(
    State(state): State<AppState>,
    Query(q): Query<BlocksQuery>,
) -> Result<(HeaderMap, Json<serde_json::Value>), ApiError> {
    let low_hash = q.low_hash.unwrap_or_default();
    if !contains_hex64(&low_hash) {
        return Err(ApiError::Validation(
            "lowHash must contain 64 lowercase hex characters".into(),
        ));
    }
    let node = state
        .node
        .as_ref()
        .ok_or(ApiError::Unavailable("karlsend is not configured"))?;
    let mut headers = HeaderMap::new();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("public, max-age=3"));

    let reply = node
        .get_blocks(
            &low_hash,
            q.include_blocks,
            q.include_transactions,
            std::time::Duration::from_secs(60),
        )
        .await;
    let body = match reply {
        Ok(r) => {
            let blocks: Vec<serde_json::Value> = r
                .blocks
                .into_iter()
                .map(|b| {
                    let mut v = serde_json::to_value(block_from_node(b, true))
                        .unwrap_or(serde_json::Value::Null);
                    v["extra"] = serde_json::Value::Null;
                    v
                })
                .collect();
            serde_json::json!({ "blockHashes": r.block_hashes, "blocks": blocks })
        }
        Err(e) => {
            tracing::warn!(error = %e, "getBlocks failed");
            serde_json::json!({ "blockHashes": [], "blocks": [] })
        }
    };
    Ok((headers, Json(body)))
}

// ---- node path ----

async fn color_from_node(node: &NodeClient, model: &BlockModel) -> Option<&'static str> {
    if model.verbose_data.is_chain_block {
        return Some("blue");
    }
    match node.get_current_block_color(&model.verbose_data.hash).await {
        Ok(blue) => blue.map(color_name),
        Err(e) => {
            tracing::warn!(error = %e, "block color lookup failed");
            None
        }
    }
}

fn block_from_node(block: proto::RpcBlock, include_transactions: bool) -> BlockModel {
    let h = block.header.unwrap_or_default();
    let v = block.verbose_data.unwrap_or_default();

    let transactions = include_transactions.then(|| {
        block
            .transactions
            .into_iter()
            .map(tx_from_node)
            .collect::<Vec<_>>()
    });
    let (miner_address, miner_info) = transactions
        .as_deref()
        .map_or((None, None), miner_from_transactions);

    BlockModel {
        header: Header {
            version: h.version as i32,
            hash_merkle_root: h.hash_merkle_root,
            accepted_id_merkle_root: h.accepted_id_merkle_root,
            utxo_commitment: h.utxo_commitment,
            timestamp: h.timestamp.to_string(),
            bits: h.bits as i64,
            nonce: h.nonce.to_string(),
            daa_score: h.daa_score.to_string(),
            blue_work: trim_hex(&h.blue_work),
            parents: h
                .parents
                .into_iter()
                .map(|p| ParentHashes {
                    parent_hashes: p.parent_hashes,
                })
                .collect(),
            blue_score: h.blue_score.to_string(),
            pruning_point: h.pruning_point,
        },
        transactions,
        verbose_data: VerboseData {
            hash: v.hash,
            difficulty: v.difficulty,
            selected_parent_hash: v.selected_parent_hash,
            transaction_ids: v.transaction_ids,
            blue_score: v.blue_score.to_string(),
            children_hashes: v.children_hashes,
            merge_set_blues_hashes: v.merge_set_blues_hashes,
            merge_set_reds_hashes: v.merge_set_reds_hashes,
            is_chain_block: v.is_chain_block,
        },
        extra: Extra {
            color: None,
            miner_address,
            miner_info,
        },
    }
}

fn tx_from_node(tx: proto::RpcTransaction) -> TxModel {
    let v = tx.verbose_data.unwrap_or_default();
    TxModel {
        inputs: tx
            .inputs
            .into_iter()
            .map(|i| {
                let o = i.previous_outpoint.unwrap_or_default();
                InputModel {
                    previous_outpoint: Outpoint {
                        transaction_id: o.transaction_id,
                        index: o.index as i32,
                    },
                    signature_script: i.signature_script,
                    sig_op_count: i.sig_op_count as i32,
                    sequence: i.sequence,
                }
            })
            .collect(),
        outputs: tx
            .outputs
            .into_iter()
            .map(|o| {
                let script_hex = o
                    .script_public_key
                    .map(|s| s.script_public_key)
                    .unwrap_or_default();
                let script = hex::decode(&script_hex).unwrap_or_default();
                output_model(o.amount as i64, &script)
            })
            .collect(),
        subnetwork_id: tx.subnetwork_id,
        payload: tx.payload,
        verbose_data: TxVerbose {
            transaction_id: v.transaction_id,
            hash: v.hash,
            compute_mass: v.compute_mass as i64,
            block_hash: v.block_hash,
            block_time: v.block_time as i64,
        },
        lock_time: tx.lock_time,
        gas: tx.gas,
        mass: tx.mass as i64,
        version: tx.version as i32,
    }
}

// ---- database path ----

async fn block_from_db(
    db: &PgPool,
    block_id: &str,
    include_transactions: bool,
) -> Result<BlockModel, ApiError> {
    let hash = hex::decode(block_id).map_err(|_| ApiError::Validation("invalid blockId".into()))?;

    let block: BlockRow = sqlx::query_as(
        r#"SELECT hash, accepted_id_merkle_root, merge_set_blues_hashes, merge_set_reds_hashes,
                  selected_parent_hash, bits, blue_score, blue_work, daa_score, hash_merkle_root,
                  nonce, pruning_point, "timestamp", utxo_commitment, version
           FROM blocks WHERE hash = $1"#,
    )
    .bind(&hash)
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::NotFound("Block not found"))?;

    let parents: Vec<(Vec<u8>,)> =
        sqlx::query_as("SELECT parent_hash FROM block_parent WHERE block_hash = $1")
            .bind(&hash)
            .fetch_all(db)
            .await?;
    let children: Vec<(Vec<u8>,)> =
        sqlx::query_as("SELECT block_hash FROM block_parent WHERE parent_hash = $1")
            .bind(&hash)
            .fetch_all(db)
            .await?;
    let (is_chain,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM transactions_acceptances WHERE block_hash = $1)",
    )
    .bind(&hash)
    .fetch_one(db)
    .await?;

    // Coinbase first (subnetwork 0100..), then by id: a stable order, because the
    // schema does not store the in-block transaction index.
    let txs: Vec<TxRow> = sqlx::query_as(
        r#"SELECT t.transaction_id, t.hash, t.mass, t.payload, t.block_time, s.subnetwork_id
           FROM blocks_transactions bt
           JOIN transactions t ON t.transaction_id = bt.transaction_id
           LEFT JOIN subnetworks s ON s.id = t.subnetwork_id
           WHERE bt.block_hash = $1
           ORDER BY (s.subnetwork_id LIKE '01%') DESC, t.transaction_id"#,
    )
    .bind(&hash)
    .fetch_all(db)
    .await?;

    let transactions = if include_transactions {
        Some(build_transactions(db, &hash, &txs).await?)
    } else {
        None
    };
    let (miner_address, miner_info) = transactions
        .as_deref()
        .map_or((None, None), miner_from_transactions);

    let bits = block.bits.unwrap_or_default();
    let blue_score = block.blue_score.unwrap_or_default().to_string();
    let selected_parent = hx(&block.selected_parent_hash);
    let parent_hashes = order_parents(
        &selected_parent,
        parents.iter().map(|p| hex::encode(&p.0)).collect(),
    );
    // The stored order is arbitrary; sorting only makes responses deterministic.
    let mut children_hashes: Vec<String> = children.iter().map(|c| hex::encode(&c.0)).collect();
    children_hashes.sort();

    Ok(BlockModel {
        header: Header {
            version: block.version.unwrap_or_default() as i32,
            hash_merkle_root: hx(&block.hash_merkle_root),
            accepted_id_merkle_root: hx(&block.accepted_id_merkle_root),
            utxo_commitment: hx(&block.utxo_commitment),
            timestamp: block.timestamp.unwrap_or_default().to_string(),
            bits,
            nonce: nonce_decimal(&block.nonce),
            daa_score: block.daa_score.unwrap_or_default().to_string(),
            blue_work: trimmed_hex(&block.blue_work),
            // Only level 0 is stored.
            parents: vec![ParentHashes { parent_hashes }],
            blue_score: blue_score.clone(),
            pruning_point: hx(&block.pruning_point),
        },
        transactions,
        verbose_data: VerboseData {
            hash: hex::encode(&block.hash),
            difficulty: difficulty_from_bits(bits as u32),
            selected_parent_hash: selected_parent,
            transaction_ids: txs.iter().map(|t| hex::encode(&t.transaction_id)).collect(),
            blue_score,
            children_hashes,
            merge_set_blues_hashes: hx_all(&block.merge_set_blues_hashes),
            merge_set_reds_hashes: hx_all(&block.merge_set_reds_hashes),
            is_chain_block: is_chain,
        },
        extra: Extra {
            color: None,
            miner_address,
            miner_info,
        },
    })
}

/// Color of a non-chain block from the database: walks the block's future level by
/// level until a chain block lists it in its merge set. Kaspa only checks direct
/// children; a merging chain block can sit deeper, so this searches a bounded
/// number of blocks instead. `None` if not found (not merged yet, or out of range).
async fn color_from_db(db: &PgPool, block_id: &str) -> Result<Option<&'static str>, ApiError> {
    let target =
        hex::decode(block_id).map_err(|_| ApiError::Validation("invalid blockId".into()))?;
    let mut frontier = vec![target.clone()];
    let mut seen: HashSet<Vec<u8>> = HashSet::new();

    while !frontier.is_empty() && seen.len() < COLOR_SEARCH_LIMIT {
        let rows: Vec<MergeRow> = sqlx::query_as(
            r#"SELECT b.hash, b.merge_set_blues_hashes, b.merge_set_reds_hashes,
                      EXISTS (SELECT 1 FROM transactions_acceptances ta
                              WHERE ta.block_hash = b.hash) AS is_chain
               FROM block_parent bp
               JOIN blocks b ON b.hash = bp.block_hash
               WHERE bp.parent_hash = ANY($1)"#,
        )
        .bind(&frontier)
        .fetch_all(db)
        .await?;

        frontier.clear();
        for r in rows {
            if !seen.insert(r.hash.clone()) {
                continue;
            }
            if r.is_chain {
                if r.merge_set_blues_hashes
                    .iter()
                    .flatten()
                    .any(|h| *h == target)
                {
                    return Ok(Some("blue"));
                }
                if r.merge_set_reds_hashes
                    .iter()
                    .flatten()
                    .any(|h| *h == target)
                {
                    return Ok(Some("red"));
                }
            }
            frontier.push(r.hash);
        }
    }
    Ok(None)
}

async fn build_transactions(
    db: &PgPool,
    block_hash: &[u8],
    txs: &[TxRow],
) -> Result<Vec<TxModel>, ApiError> {
    let ids: Vec<Vec<u8>> = txs.iter().map(|t| t.transaction_id.clone()).collect();

    let inputs: Vec<InputRow> = sqlx::query_as(
        r#"SELECT t.transaction_id, i.previous_outpoint_hash, i.previous_outpoint_index,
                  i.signature_script, i.sig_op_count
           FROM transactions t, unnest(t.inputs) WITH ORDINALITY AS i(
                  "index", previous_outpoint_hash, previous_outpoint_index, signature_script,
                  sig_op_count, previous_outpoint_script, previous_outpoint_amount, ord)
           WHERE t.transaction_id = ANY($1)
           ORDER BY t.transaction_id, i."index""#,
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;

    let outputs: Vec<OutputRow> = sqlx::query_as(
        r#"SELECT t.transaction_id, o.amount, o.script_public_key
           FROM transactions t, unnest(t.outputs) WITH ORDINALITY AS o(
                  "index", amount, script_public_key, script_public_key_address, ord)
           WHERE t.transaction_id = ANY($1)
           ORDER BY t.transaction_id, o."index""#,
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;

    let mut ins: HashMap<Vec<u8>, Vec<InputModel>> = HashMap::new();
    for r in inputs {
        ins.entry(r.transaction_id).or_default().push(InputModel {
            previous_outpoint: Outpoint {
                transaction_id: hx(&r.previous_outpoint_hash),
                index: r.previous_outpoint_index.unwrap_or_default() as i32,
            },
            signature_script: hx(&r.signature_script),
            sig_op_count: r.sig_op_count.unwrap_or_default() as i32,
            // Not stored; 0 in every captured legacy response.
            sequence: 0,
        });
    }
    let mut outs: HashMap<Vec<u8>, Vec<OutputModel>> = HashMap::new();
    for r in outputs {
        let script = r.script_public_key.unwrap_or_default();
        outs.entry(r.transaction_id)
            .or_default()
            .push(output_model(r.amount.unwrap_or_default(), &script));
    }

    Ok(txs
        .iter()
        .map(|t| TxModel {
            inputs: ins.remove(&t.transaction_id).unwrap_or_default(),
            outputs: outs.remove(&t.transaction_id).unwrap_or_default(),
            subnetwork_id: t.subnetwork_id.clone().unwrap_or_default(),
            payload: hx(&t.payload),
            verbose_data: TxVerbose {
                transaction_id: hex::encode(&t.transaction_id),
                hash: hx(&t.hash),
                compute_mass: t.mass.unwrap_or_default() as i64,
                block_hash: hex::encode(block_hash),
                block_time: t.block_time.unwrap_or_default(),
            },
            lock_time: 0,
            gas: 0,
            // Only one mass is stored (the compute mass); see docs/VERIFY.md.
            mass: t.mass.unwrap_or_default() as i64,
            version: 0,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // Real legacy responses captured on 2026-10-18 (see docs/VERIFY.md).
    /// Tip block holding only a coinbase (`includeTransactions=true`).
    const COINBASE_ONLY: &str = include_str!("../../tests/fixtures/golden-block.json");
    /// Chain block with a coinbase and one regular transaction.
    const WITH_TXS: &str = include_str!("../../tests/fixtures/golden-block-with-txs.json");
    /// The block above with `includeColor=true`.
    const WITH_COLOR: &str = include_str!("../../tests/fixtures/golden-block-color.json");
    /// The block above with `includeTransactions=false`.
    const WITHOUT_TXS: &str = include_str!("../../tests/fixtures/golden-block-notx.json");

    fn st(v: &Value) -> String {
        v.as_str().expect("string field").to_string()
    }

    fn strings(v: &Value) -> Vec<String> {
        v.as_array().expect("array field").iter().map(st).collect()
    }

    fn input_from_golden(i: &Value) -> InputModel {
        InputModel {
            previous_outpoint: Outpoint {
                transaction_id: st(&i["previousOutpoint"]["transactionId"]),
                index: i["previousOutpoint"]["index"].as_i64().unwrap() as i32,
            },
            signature_script: st(&i["signatureScript"]),
            sig_op_count: i["sigOpCount"].as_i64().unwrap() as i32,
            // Not stored; the handler emits 0 and the golden agrees.
            sequence: 0,
        }
    }

    fn tx_from_golden(tx: &Value) -> TxModel {
        let tv = &tx["verboseData"];
        TxModel {
            inputs: tx["inputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(input_from_golden)
                .collect(),
            outputs: tx["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| {
                    let script = hex::decode(st(&o["scriptPublicKey"]["scriptPublicKey"])).unwrap();
                    output_model(o["amount"].as_i64().unwrap(), &script)
                })
                .collect(),
            subnetwork_id: st(&tx["subnetworkId"]),
            payload: st(&tx["payload"]),
            verbose_data: TxVerbose {
                transaction_id: st(&tv["transactionId"]),
                hash: st(&tv["hash"]),
                compute_mass: tv["computeMass"].as_i64().unwrap(),
                block_hash: st(&tv["blockHash"]),
                block_time: tv["blockTime"].as_i64().unwrap(),
            },
            lock_time: 0,
            gas: 0,
            // Known gap: the database does not store `mass` (the golden has 172 next to
            // a compute mass of 2036), so the golden value is used here.
            mass: tx["mass"].as_i64().unwrap(),
            version: 0,
        }
    }

    /// Builds our response model from the golden values. Everything the handler
    /// derives (difficulty, nonce, blue work, script type, address) goes through the
    /// same functions the handler uses, so a wrong assumption shows up as a diff.
    fn model_from_golden(g: &Value) -> BlockModel {
        let h = &g["header"];
        let vd = &g["verboseData"];
        let bits = h["bits"].as_i64().unwrap();
        let mut transactions: Option<Vec<TxModel>> = g["transactions"]
            .as_array()
            .map(|txs| txs.iter().map(tx_from_golden).collect());
        let (miner_address, miner_info) = transactions
            .as_deref()
            .map_or((None, None), miner_from_transactions);

        // Simulate the stored column values: 8-byte big-endian nonce and blue work as
        // trimmed big-endian bytes.
        let nonce_bytes = st(&h["nonce"])
            .parse::<u64>()
            .unwrap()
            .to_be_bytes()
            .to_vec();
        let work_hex = st(&h["blueWork"]);
        let work_hex = if work_hex.len() % 2 == 1 {
            format!("0{work_hex}")
        } else {
            work_hex
        };
        let work = hex::decode(work_hex).unwrap();

        BlockModel {
            header: Header {
                version: h["version"].as_i64().unwrap() as i32,
                hash_merkle_root: st(&h["hashMerkleRoot"]),
                accepted_id_merkle_root: st(&h["acceptedIdMerkleRoot"]),
                utxo_commitment: st(&h["utxoCommitment"]),
                timestamp: st(&h["timestamp"]),
                bits,
                nonce: nonce_decimal(&Some(nonce_bytes)),
                daa_score: st(&h["daaScore"]),
                blue_work: trimmed_hex(&Some(work)),
                parents: vec![ParentHashes {
                    parent_hashes: strings(&h["parents"][0]["parentHashes"]),
                }],
                blue_score: st(&h["blueScore"]),
                pruning_point: st(&h["pruningPoint"]),
            },
            transactions: transactions.take(),
            verbose_data: VerboseData {
                hash: st(&vd["hash"]),
                difficulty: difficulty_from_bits(bits as u32),
                selected_parent_hash: st(&vd["selectedParentHash"]),
                transaction_ids: strings(&vd["transactionIds"]),
                blue_score: st(&vd["blueScore"]),
                children_hashes: strings(&vd["childrenHashes"]),
                merge_set_blues_hashes: strings(&vd["mergeSetBluesHashes"]),
                merge_set_reds_hashes: strings(&vd["mergeSetRedsHashes"]),
                is_chain_block: vd["isChainBlock"].as_bool().unwrap(),
            },
            extra: Extra {
                color: g["extra"]["color"]
                    .as_str()
                    .map(|c| color_name(c == "blue")),
                miner_address,
                miner_info,
            },
        }
    }

    fn assert_matches_golden(raw: &str) {
        let golden: Value = serde_json::from_str(raw).unwrap();
        let ours = model_from_golden(&golden);

        // Compared as text: serde_json's default float parser is not round-trip exact.
        let literal = raw
            .split("\"difficulty\":")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .expect("difficulty field");
        assert_eq!(
            serde_json::to_string(&ours.verbose_data.difficulty).unwrap(),
            literal
        );

        let mut got = serde_json::to_value(&ours).unwrap();
        let mut want = golden;
        got["verboseData"]["difficulty"] = Value::Null;
        want["verboseData"]["difficulty"] = Value::Null;
        // Known gap: the node reports one `parents` entry per DAG level (41 in the
        // goldens); the database only stores level 0.
        want["header"]["parents"]
            .as_array_mut()
            .unwrap()
            .truncate(1);
        // Legacy rest-v2 leaves these null; we decode them from the coinbase payload
        // (Kaspa does the same). Only checked when the block has transactions.
        if want["transactions"].is_array() {
            assert_eq!(
                got["extra"]["minerAddress"],
                want["transactions"][0]["outputs"][0]["verboseData"]["scriptPublicKeyAddress"]
            );
            want["extra"]["minerAddress"] = got["extra"]["minerAddress"].clone();
            want["extra"]["minerInfo"] = got["extra"]["minerInfo"].clone();
        }

        assert_eq!(got, want);
    }

    #[test]
    fn coinbase_only_block_matches_golden() {
        assert_matches_golden(COINBASE_ONLY);
    }

    #[test]
    fn block_with_regular_transaction_matches_golden() {
        assert_matches_golden(WITH_TXS);
    }

    #[test]
    fn block_with_color_matches_golden() {
        assert_matches_golden(WITH_COLOR);
    }

    #[test]
    fn transactions_are_null_when_not_requested() {
        assert_matches_golden(WITHOUT_TXS);
        let golden: Value = serde_json::from_str(WITHOUT_TXS).unwrap();
        assert!(golden.get("transactions").is_some_and(Value::is_null));
    }

    #[test]
    fn selected_parent_comes_first() {
        let golden: Value = serde_json::from_str(WITH_TXS).unwrap();
        let selected = st(&golden["verboseData"]["selectedParentHash"]);
        let level0 = strings(&golden["header"]["parents"][0]["parentHashes"]);
        // The golden's own order is selected parent first, and it is not hash order.
        assert_eq!(level0[0], selected);
        let mut shuffled = level0.clone();
        shuffled.reverse();
        assert_eq!(order_parents(&selected, shuffled), level0);
    }
}
