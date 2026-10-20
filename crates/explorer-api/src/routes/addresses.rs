//! Address balances and UTXOs, read from karlsend's UTXO index (`--utxoindex`), as in
//! the legacy `rest-v2` and the Kaspa REST server.
//!
//! The database holds no live UTXO set, so there is no fallback: when the node cannot
//! answer, these endpoints return 503. They never answer `{"balance": 0}` for a
//! failure, which hid outages from the deposit watcher in the legacy service.

use axum::{
    extract::{Path, Query, State},
    http::HeaderValue,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    address::{decode, MAINNET_PREFIX},
    error::ApiError,
    node::{proto::RpcUtxosByAddressesEntry, NodeClient},
    AppState,
};

/// Upper bound for the batch endpoints, so one request cannot make the node scan an
/// unbounded address list.
pub const MAX_ADDRESSES_PER_REQUEST: usize = 1000;

fn node(state: &AppState) -> Result<&NodeClient, ApiError> {
    state
        .node
        .as_ref()
        .ok_or(ApiError::Unavailable("karlsend is not configured"))
}

/// Full validation (charset, checksum, `karlsen` prefix), not just a pattern match.
pub(crate) fn validate(address: &str) -> Result<(), ApiError> {
    match decode(address) {
        Some((prefix, _, _)) if prefix == MAINNET_PREFIX => Ok(()),
        _ => Err(ApiError::Validation(format!("Invalid address: {address}"))),
    }
}

fn validate_batch(addresses: &[String]) -> Result<(), ApiError> {
    if addresses.len() > MAX_ADDRESSES_PER_REQUEST {
        return Err(ApiError::Validation(format!(
            "At most {MAX_ADDRESSES_PER_REQUEST} addresses per request"
        )));
    }
    addresses.iter().try_for_each(|a| validate(a))
}

#[derive(Debug, Serialize)]
pub struct BalanceResponse {
    address: String,
    balance: u64,
}

#[derive(Debug, Deserialize)]
pub struct AddressesBody {
    #[serde(default)]
    addresses: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct UtxoResponse {
    address: String,
    outpoint: Outpoint,
    #[serde(rename = "utxoEntry")]
    utxo_entry: UtxoEntry,
}

#[derive(Debug, Serialize)]
struct Outpoint {
    #[serde(rename = "transactionId")]
    transaction_id: String,
    index: u32,
}

/// `amount` and `blockDaaScore` are strings in legacy responses (protobuf JSON
/// encodes 64-bit integers as strings).
#[derive(Debug, Serialize)]
struct UtxoEntry {
    amount: String,
    #[serde(rename = "scriptPublicKey")]
    script_public_key: ScriptPublicKey,
    #[serde(rename = "blockDaaScore")]
    block_daa_score: String,
    #[serde(rename = "isCoinbase")]
    is_coinbase: bool,
}

#[derive(Debug, Serialize)]
struct ScriptPublicKey {
    #[serde(rename = "scriptPublicKey")]
    script_public_key: String,
}

fn utxo_from_node(e: RpcUtxosByAddressesEntry) -> UtxoResponse {
    let o = e.outpoint.unwrap_or_default();
    let u = e.utxo_entry.unwrap_or_default();
    UtxoResponse {
        address: e.address,
        outpoint: Outpoint {
            transaction_id: o.transaction_id,
            index: o.index,
        },
        utxo_entry: UtxoEntry {
            amount: u.amount.to_string(),
            script_public_key: ScriptPublicKey {
                script_public_key: u
                    .script_public_key
                    .map(|s| s.script_public_key)
                    .unwrap_or_default(),
            },
            block_daa_score: u.block_daa_score.to_string(),
            is_coinbase: u.is_coinbase,
        },
    }
}

/// `GET /addresses/{address}/balance`
pub async fn get_balance(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> Result<Json<BalanceResponse>, ApiError> {
    validate(&address)?;
    let balance = node(&state)?.get_balance_by_address(&address).await?;
    Ok(Json(BalanceResponse { address, balance }))
}

/// `POST /addresses/balances`
pub async fn post_balances(
    State(state): State<AppState>,
    Json(body): Json<AddressesBody>,
) -> Result<Json<Vec<BalanceResponse>>, ApiError> {
    if body.addresses.is_empty() {
        return Ok(Json(vec![]));
    }
    validate_batch(&body.addresses)?;
    let entries = node(&state)?
        .get_balances_by_addresses(&body.addresses)
        .await?;
    // A per-address error must not turn into a silent zero balance.
    if let Some(e) = entries.iter().find_map(|e| e.error.as_ref()) {
        return Err(crate::node::NodeError::Rpc(e.message.clone()).into());
    }
    Ok(Json(
        entries
            .into_iter()
            .map(|e| BalanceResponse {
                address: e.address,
                balance: e.balance,
            })
            .collect(),
    ))
}

/// Largest page `GET /addresses/{address}/utxos?limit=` serves.
pub const MAX_UTXO_PAGE: usize = 500;

/// Optional paging. Without `limit` the endpoint answers exactly as legacy did
/// (every UTXO, node order), so existing clients are unaffected.
#[derive(Debug, Default, Deserialize)]
pub struct UtxoPage {
    limit: Option<usize>,
    #[serde(default)]
    offset: usize,
}

/// `GET /addresses/{address}/utxos`
///
/// With `limit` (1..=500) and `offset`: newest first (block DAA score descending,
/// then outpoint), and the response carries `X-Total-Count` (UTXO count) and
/// `X-Total-Amount` (sum in sompi) for the whole set. Large sets are fetched from
/// the node once and kept briefly (see `utxo_cache`), so paging through an address
/// with hundreds of thousands of UTXOs does not rescan the index per page.
pub async fn get_utxos(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(page): Query<UtxoPage>,
) -> Result<Response, ApiError> {
    validate(&address)?;
    let Some(limit) = page.limit else {
        let entries = node(&state)?
            .get_utxos_by_addresses(std::slice::from_ref(&address))
            .await?;
        return Ok(Json(
            entries
                .into_iter()
                .filter(|e| e.address == address)
                .map(utxo_from_node)
                .collect::<Vec<_>>(),
        )
        .into_response());
    };
    if !(1..=MAX_UTXO_PAGE).contains(&limit) {
        return Err(ApiError::Validation(format!(
            "limit must be between 1 and {MAX_UTXO_PAGE}"
        )));
    }

    let set = state
        .utxo_cache
        .get_or_fetch(&address, || async {
            let mut entries: Vec<_> = node(&state)?
                .get_utxos_by_addresses(std::slice::from_ref(&address))
                .await?
                .into_iter()
                .filter(|e| e.address == address)
                .collect();
            entries.sort_by(|a, b| {
                let key = |e: &RpcUtxosByAddressesEntry| {
                    let daa = e.utxo_entry.as_ref().map_or(0, |u| u.block_daa_score);
                    let (tx, idx) = e
                        .outpoint
                        .as_ref()
                        .map_or((String::new(), 0), |o| (o.transaction_id.clone(), o.index));
                    (std::cmp::Reverse(daa), tx, idx)
                };
                key(a).cmp(&key(b))
            });
            Ok::<_, ApiError>(entries)
        })
        .await?;

    let total_amount: u128 = set
        .iter()
        .map(|e| u128::from(e.utxo_entry.as_ref().map_or(0, |u| u.amount)))
        .sum();
    let items: Vec<UtxoResponse> = set
        .iter()
        .skip(page.offset)
        .take(limit)
        .cloned()
        .map(utxo_from_node)
        .collect();

    let mut res = Json(items).into_response();
    let h = res.headers_mut();
    h.insert("x-total-count", HeaderValue::from(set.len()));
    if let Ok(v) = HeaderValue::from_str(&total_amount.to_string()) {
        h.insert("x-total-amount", v);
    }
    Ok(res)
}

/// `POST /addresses/utxos`
pub async fn post_utxos(
    State(state): State<AppState>,
    Json(body): Json<AddressesBody>,
) -> Result<Json<Vec<UtxoResponse>>, ApiError> {
    if body.addresses.is_empty() {
        return Ok(Json(vec![]));
    }
    validate_batch(&body.addresses)?;
    let entries = node(&state)?
        .get_utxos_by_addresses(&body.addresses)
        .await?;
    Ok(Json(entries.into_iter().map(utxo_from_node).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rejects_bad_checksum_and_foreign_prefix() {
        let good = "karlsen:qrc9rgx9x8gyekvrd7533hj3tl0cxd5r9ggxkg5l4a6xgllethrtuvp48fc3n";
        assert!(validate(good).is_ok());
        let bad_checksum = format!("{}x", &good[..good.len() - 1]);
        assert!(validate(&bad_checksum).is_err());
        assert!(validate("karlsen:qqqqinvalid").is_err());
        assert!(
            validate("kaspa:qqkqkzjvr7zwxxmjxjkmxxdwju9kjs6e9u82uh59z07vgaks6gg62v8707g73")
                .is_err()
        );
    }

    #[test]
    fn batch_limit() {
        let good = "karlsen:qrc9rgx9x8gyekvrd7533hj3tl0cxd5r9ggxkg5l4a6xgllethrtuvp48fc3n";
        assert!(validate_batch(&vec![good.to_string(); MAX_ADDRESSES_PER_REQUEST]).is_ok());
        assert!(validate_batch(&vec![good.to_string(); MAX_ADDRESSES_PER_REQUEST + 1]).is_err());
    }
}
