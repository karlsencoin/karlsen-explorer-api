//! `GET /blocks/{blockId}` — wire-compatible with legacy `rest-v2`.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::{
    address::{script_to_address, script_type, MAINNET_PREFIX},
    difficulty::difficulty_from_bits,
    error::ApiError,
    AppState,
};

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

// ---- response model (field names follow the legacy OpenAPI document) ----

#[derive(Debug, Serialize)]
pub struct BlockModel {
    header: Header,
    #[serde(skip_serializing_if = "Option::is_none")]
    transactions: Option<Vec<TxModel>>,
    #[serde(rename = "verboseData")]
    verbose_data: VerboseData,
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
    lock_time: i64,
    gas: i64,
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
    sequence: i64,
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

fn hx(b: &Option<Vec<u8>>) -> String {
    b.as_deref().map(hex::encode).unwrap_or_default()
}

fn hx_all(v: &Option<Vec<Vec<u8>>>) -> Vec<String> {
    v.iter().flatten().map(hex::encode).collect()
}

/// Big-endian bytes without leading zero bytes, as lowercase hex.
fn trimmed_hex(b: &Option<Vec<u8>>) -> String {
    let h = hx(b);
    let t = h.trim_start_matches('0');
    if t.is_empty() {
        "0".to_string()
    } else {
        t.to_string()
    }
}

fn nonce_decimal(b: &Option<Vec<u8>>) -> String {
    let n = b
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .fold(0u128, |a, &x| (a << 8) | x as u128);
    n.to_string()
}

pub async fn get_block(
    State(state): State<AppState>,
    Path(block_id): Path<String>,
    Query(q): Query<BlockQuery>,
) -> Result<Json<BlockModel>, ApiError> {
    if block_id.len() != 64
        || !block_id
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ApiError::Validation(
            "blockId must be 64 lowercase hex characters".into(),
        ));
    }
    if q.include_color {
        return Err(ApiError::NotImplemented(
            "includeColor is not implemented yet",
        ));
    }
    let hash =
        hex::decode(&block_id).map_err(|_| ApiError::Validation("invalid blockId".into()))?;
    let db = &state.db;

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

    let transactions = if q.include_transactions {
        Some(build_transactions(db, &hash, &txs).await?)
    } else {
        None
    };

    let bits = block.bits.unwrap_or_default();
    let blue_score = block.blue_score.unwrap_or_default().to_string();
    let parent_hashes: Vec<String> = parents.iter().map(|p| hex::encode(&p.0)).collect();

    Ok(Json(BlockModel {
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
            parents: vec![ParentHashes { parent_hashes }],
            blue_score: blue_score.clone(),
            pruning_point: hx(&block.pruning_point),
        },
        transactions,
        verbose_data: VerboseData {
            hash: hex::encode(&block.hash),
            difficulty: difficulty_from_bits(bits as u32),
            selected_parent_hash: hx(&block.selected_parent_hash),
            transaction_ids: txs.iter().map(|t| hex::encode(&t.transaction_id)).collect(),
            blue_score,
            children_hashes: children.iter().map(|c| hex::encode(&c.0)).collect(),
            merge_set_blues_hashes: hx_all(&block.merge_set_blues_hashes),
            merge_set_reds_hashes: hx_all(&block.merge_set_reds_hashes),
            is_chain_block: is_chain,
        },
    }))
}

async fn build_transactions(
    db: &sqlx::PgPool,
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
            sequence: 0,
        });
    }
    let mut outs: HashMap<Vec<u8>, Vec<OutputModel>> = HashMap::new();
    for r in outputs {
        let script = r.script_public_key.unwrap_or_default();
        outs.entry(r.transaction_id).or_default().push(OutputModel {
            amount: r.amount.unwrap_or_default(),
            verbose_data: OutputVerbose {
                script_public_key_type: script_type(&script),
                script_public_key_address: script_to_address(MAINNET_PREFIX, &script)
                    .unwrap_or_default(),
            },
            script_public_key: ScriptPublicKey {
                script_public_key: hex::encode(&script),
                version: 0,
            },
        });
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
            mass: t.mass.unwrap_or_default() as i64,
            version: 0,
        })
        .collect())
}
