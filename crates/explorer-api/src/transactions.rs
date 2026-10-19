//! The legacy transaction model shared by `/transactions/{id}`,
//! `POST /transactions/search` and `/addresses/{address}/full-transactions`.
//!
//! Wire format follows the legacy `rest-v2` captures (tests/fixtures/golden-*.json):
//! snake_case keys in a fixed order, `mass`, `previous_outpoint_index` and
//! `sig_op_count` as strings, `payload` `null` when empty, `inputs` / `outputs`
//! `null` when empty. One intended difference: addresses always carry the
//! `karlsen:` prefix (legacy drops it in some endpoints, see docs/VERIFY.md).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, PgPool};

use crate::{
    address::{script_to_address, script_type, MAINNET_PREFIX},
    error::ApiError,
    node::NodeClient,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resolve {
    /// Previous outpoint address and amount stay `null`.
    #[default]
    No,
    /// Fill `previous_outpoint_address` and `previous_outpoint_amount`.
    Light,
    /// Also add `previous_outpoint_resolved` with the whole spent output.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcceptanceFilter {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Default)]
pub struct TxOptions {
    /// Keys to keep; empty keeps all (legacy `fields=a,b,c`).
    pub fields: Vec<String>,
    pub resolve: Resolve,
    pub include_inputs: bool,
    pub include_outputs: bool,
    pub acceptance: Option<AcceptanceFilter>,
}

impl TxOptions {
    pub fn new(fields: &str, resolve: Resolve) -> Self {
        Self {
            fields: fields
                .split(',')
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map(str::to_string)
                .collect(),
            resolve,
            include_inputs: true,
            include_outputs: true,
            acceptance: None,
        }
    }

    fn wants(&self, key: &str) -> bool {
        self.fields.is_empty() || self.fields.iter().any(|f| f == key)
    }
}

// ---- wire model ----

#[derive(Debug, Serialize)]
struct TxJson {
    subnetwork_id: Option<String>,
    transaction_id: String,
    hash: Option<String>,
    mass: Option<String>,
    payload: Option<String>,
    block_hash: Vec<String>,
    block_time: Option<i64>,
    is_accepted: bool,
    accepting_block_hash: Option<String>,
    accepting_block_blue_score: Option<i64>,
    accepting_block_time: Option<i64>,
    inputs: Option<Vec<InputJson>>,
    outputs: Option<Vec<OutputJson>>,
}

#[derive(Debug, Serialize)]
struct InputJson {
    transaction_id: String,
    index: i32,
    previous_outpoint_hash: String,
    previous_outpoint_index: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_outpoint_resolved: Option<OutputJson>,
    previous_outpoint_address: Option<String>,
    previous_outpoint_amount: Option<i64>,
    signature_script: Option<String>,
    sig_op_count: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct OutputJson {
    transaction_id: String,
    index: i32,
    amount: i64,
    script_public_key: String,
    script_public_key_address: Option<String>,
    script_public_key_type: &'static str,
}

impl OutputJson {
    fn new(transaction_id: String, index: i32, amount: i64, script: &[u8]) -> Self {
        Self {
            transaction_id,
            index,
            amount,
            script_public_key: hex::encode(script),
            script_public_key_address: script_to_address(MAINNET_PREFIX, script),
            script_public_key_type: script_type(script),
        }
    }
}

// ---- rows ----

#[derive(Debug, FromRow)]
struct TxRow {
    transaction_id: Vec<u8>,
    subnetwork_id: Option<String>,
    hash: Option<Vec<u8>>,
    mass: Option<i32>,
    payload: Option<Vec<u8>>,
    block_time: Option<i64>,
    is_accepted: bool,
    accepting_block_hash: Option<Vec<u8>>,
    accepting_block_blue_score: Option<i64>,
    accepting_block_time: Option<i64>,
}

#[derive(Debug, FromRow)]
struct InputRow {
    transaction_id: Vec<u8>,
    index: Option<i16>,
    previous_outpoint_hash: Option<Vec<u8>>,
    previous_outpoint_index: Option<i16>,
    signature_script: Option<Vec<u8>>,
    sig_op_count: Option<i16>,
    previous_outpoint_script: Option<Vec<u8>>,
    previous_outpoint_amount: Option<i64>,
}

#[derive(Debug, FromRow)]
struct OutputRow {
    transaction_id: Vec<u8>,
    index: Option<i16>,
    amount: Option<i64>,
    script_public_key: Option<Vec<u8>>,
}

fn hx(b: &Option<Vec<u8>>) -> Option<String> {
    b.as_deref().map(hex::encode)
}

/// Loads transactions by id, newest first (legacy order), as JSON objects.
pub async fn load(
    db: &PgPool,
    node: Option<&NodeClient>,
    ids: &[Vec<u8>],
    opts: &TxOptions,
) -> Result<Vec<Value>, ApiError> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let rows: Vec<TxRow> = sqlx::query_as(
        r#"SELECT t.transaction_id, s.subnetwork_id, t.hash, t.mass, t.payload, t.block_time,
                  (ta.transaction_id IS NOT NULL) AS is_accepted,
                  ta.block_hash AS accepting_block_hash,
                  b.blue_score AS accepting_block_blue_score,
                  b."timestamp" AS accepting_block_time
           FROM transactions t
           LEFT JOIN subnetworks s ON s.id = t.subnetwork_id
           LEFT JOIN transactions_acceptances ta ON ta.transaction_id = t.transaction_id
           LEFT JOIN blocks b ON b.hash = ta.block_hash
           WHERE t.transaction_id = ANY($1)
             AND ($2::text IS NULL
                  OR ($2 = 'accepted' AND ta.transaction_id IS NOT NULL)
                  OR ($2 = 'rejected' AND ta.transaction_id IS NULL))
           ORDER BY t.block_time DESC, t.transaction_id"#,
    )
    .bind(ids)
    .bind(opts.acceptance.map(|a| match a {
        AcceptanceFilter::Accepted => "accepted",
        AcceptanceFilter::Rejected => "rejected",
    }))
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok(vec![]);
    }
    let found: Vec<Vec<u8>> = rows.iter().map(|r| r.transaction_id.clone()).collect();

    let mut block_hashes = if opts.wants("block_hash") {
        load_block_hashes(db, &found).await?
    } else {
        HashMap::new()
    };
    let mut inputs = if opts.include_inputs && opts.wants("inputs") {
        load_inputs(db, &found, opts.resolve).await?
    } else {
        HashMap::new()
    };
    let mut outputs = if opts.include_outputs && opts.wants("outputs") {
        load_outputs(db, &found).await?
    } else {
        HashMap::new()
    };

    // The accepting block may not be indexed yet (blocks and acceptances are written
    // independently); ask the node in that case, as legacy does.
    let mut accepting_cache: HashMap<Vec<u8>, Option<(i64, i64)>> = HashMap::new();

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let (mut blue_score, mut time) = (r.accepting_block_blue_score, r.accepting_block_time);
        if let (Some(h), None, Some(node)) = (&r.accepting_block_hash, blue_score, node) {
            if !accepting_cache.contains_key(h) {
                let header = match node.get_block(&hex::encode(h), false).await {
                    Ok(Some(b)) => b.header.map(|hd| (hd.blue_score as i64, hd.timestamp)),
                    _ => None,
                };
                accepting_cache.insert(h.clone(), header);
            }
            if let Some(Some((bs, ts))) = accepting_cache.get(h) {
                (blue_score, time) = (Some(*bs), Some(*ts));
            }
        }

        let tx = TxJson {
            subnetwork_id: r.subnetwork_id,
            transaction_id: hex::encode(&r.transaction_id),
            hash: hx(&r.hash),
            mass: r.mass.map(|m| m.to_string()),
            payload: r.payload.filter(|p| !p.is_empty()).map(hex::encode),
            block_hash: block_hashes.remove(&r.transaction_id).unwrap_or_default(),
            block_time: r.block_time,
            is_accepted: r.is_accepted,
            accepting_block_hash: hx(&r.accepting_block_hash),
            accepting_block_blue_score: blue_score,
            accepting_block_time: time,
            inputs: inputs.remove(&r.transaction_id).filter(|v| !v.is_empty()),
            outputs: outputs.remove(&r.transaction_id).filter(|v| !v.is_empty()),
        };
        let mut v = serde_json::to_value(tx).expect("serializable");
        if !opts.fields.is_empty() {
            if let Value::Object(m) = &mut v {
                m.retain(|k, _| opts.fields.iter().any(|f| f == k));
            }
        }
        out.push(v);
    }
    Ok(out)
}

async fn load_block_hashes(
    db: &PgPool,
    ids: &[Vec<u8>],
) -> Result<HashMap<Vec<u8>, Vec<String>>, ApiError> {
    let rows: Vec<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT transaction_id, block_hash FROM blocks_transactions
         WHERE transaction_id = ANY($1) ORDER BY transaction_id, block_hash",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    let mut m: HashMap<Vec<u8>, Vec<String>> = HashMap::new();
    for (tx, block) in rows {
        m.entry(tx).or_default().push(hex::encode(block));
    }
    Ok(m)
}

async fn load_outputs(
    db: &PgPool,
    ids: &[Vec<u8>],
) -> Result<HashMap<Vec<u8>, Vec<OutputJson>>, ApiError> {
    let rows: Vec<OutputRow> = sqlx::query_as(
        r#"SELECT t.transaction_id, o."index", o.amount, o.script_public_key
           FROM transactions t, unnest(t.outputs) AS o
           WHERE t.transaction_id = ANY($1)
           ORDER BY t.transaction_id, o."index""#,
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    let mut m: HashMap<Vec<u8>, Vec<OutputJson>> = HashMap::new();
    for r in rows {
        let o = OutputJson::new(
            hex::encode(&r.transaction_id),
            r.index.unwrap_or_default() as i32,
            r.amount.unwrap_or_default(),
            r.script_public_key.as_deref().unwrap_or_default(),
        );
        m.entry(r.transaction_id).or_default().push(o);
    }
    Ok(m)
}

async fn load_inputs(
    db: &PgPool,
    ids: &[Vec<u8>],
    resolve: Resolve,
) -> Result<HashMap<Vec<u8>, Vec<InputJson>>, ApiError> {
    let rows: Vec<InputRow> = sqlx::query_as(
        r#"SELECT t.transaction_id, i."index", i.previous_outpoint_hash,
                  i.previous_outpoint_index, i.signature_script, i.sig_op_count,
                  i.previous_outpoint_script, i.previous_outpoint_amount
           FROM transactions t, unnest(t.inputs) AS i
           WHERE t.transaction_id = ANY($1)
           ORDER BY t.transaction_id, i."index""#,
    )
    .bind(ids)
    .fetch_all(db)
    .await?;

    // Spent outputs that the indexer did not store on the input: look them up.
    let spent = if resolve == Resolve::No {
        HashMap::new()
    } else {
        let missing: HashSet<Vec<u8>> = rows
            .iter()
            .filter(|r| {
                r.previous_outpoint_script.is_none() || r.previous_outpoint_amount.is_none()
            })
            .filter_map(|r| r.previous_outpoint_hash.clone())
            .collect();
        load_spent_outputs(db, &missing.into_iter().collect::<Vec<_>>()).await?
    };

    let mut m: HashMap<Vec<u8>, Vec<InputJson>> = HashMap::new();
    for r in rows {
        let prev_hash = r.previous_outpoint_hash.clone().unwrap_or_default();
        let prev_index = r.previous_outpoint_index.unwrap_or_default();
        let resolved: Option<(i64, Vec<u8>)> = match resolve {
            Resolve::No => None,
            _ => match (&r.previous_outpoint_amount, &r.previous_outpoint_script) {
                (Some(a), Some(s)) => Some((*a, s.clone())),
                _ => spent.get(&(prev_hash.clone(), prev_index)).cloned(),
            },
        };
        let (address, amount, full) = match &resolved {
            Some((amount, script)) => (
                script_to_address(MAINNET_PREFIX, script),
                Some(*amount),
                (resolve == Resolve::Full).then(|| {
                    OutputJson::new(hex::encode(&prev_hash), prev_index as i32, *amount, script)
                }),
            ),
            None => (None, None, None),
        };
        m.entry(r.transaction_id.clone())
            .or_default()
            .push(InputJson {
                transaction_id: hex::encode(&r.transaction_id),
                index: r.index.unwrap_or_default() as i32,
                previous_outpoint_hash: hex::encode(&prev_hash),
                previous_outpoint_index: prev_index.to_string(),
                previous_outpoint_resolved: full,
                previous_outpoint_address: address,
                previous_outpoint_amount: amount,
                signature_script: hx(&r.signature_script),
                sig_op_count: r.sig_op_count.map(|c| c.to_string()),
            });
    }
    Ok(m)
}

/// `(transaction_id, index) -> (amount, script)` for the given transactions' outputs.
async fn load_spent_outputs(
    db: &PgPool,
    tx_ids: &[Vec<u8>],
) -> Result<HashMap<(Vec<u8>, i16), (i64, Vec<u8>)>, ApiError> {
    if tx_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<OutputRow> = sqlx::query_as(
        r#"SELECT t.transaction_id, o."index", o.amount, o.script_public_key
           FROM transactions t, unnest(t.outputs) AS o
           WHERE t.transaction_id = ANY($1)"#,
    )
    .bind(tx_ids)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                (r.transaction_id, r.index.unwrap_or_default()),
                (
                    r.amount.unwrap_or_default(),
                    r.script_public_key.unwrap_or_default(),
                ),
            )
        })
        .collect())
}

/// Parses a 64-character lowercase hex id.
pub fn parse_id(id: &str, what: &str) -> Result<Vec<u8>, ApiError> {
    if id.len() == 64 && id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
        Ok(hex::decode(id).expect("validated hex"))
    } else {
        Err(ApiError::Validation(format!(
            "{what} must be 64 lowercase hex characters"
        )))
    }
}
