//! `GET /transactions/{transactionId}` and `POST /transactions/search`.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    error::ApiError,
    transactions::{self, parse_id, AcceptanceFilter, Resolve, TxOptions},
    AppState,
};

/// Legacy limits (`TX_SEARCH_ID_LIMIT`, `TX_SEARCH_BS_LIMIT`).
pub const SEARCH_ID_LIMIT: usize = 1000;
pub const SEARCH_BLUE_SCORE_RANGE: i64 = 100;

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct TransactionQuery {
    #[serde(default = "default_true")]
    inputs: bool,
    #[serde(default = "default_true")]
    outputs: bool,
    #[serde(default)]
    resolve_previous_outpoints: Resolve,
    /// Accepted for compatibility; the database lookup does not need it.
    #[serde(rename = "blockHash")]
    #[allow(dead_code)]
    block_hash: Option<String>,
}

pub async fn get_transaction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<TransactionQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id, "transactionId")?;
    let opts = TxOptions {
        include_inputs: q.inputs,
        include_outputs: q.outputs,
        block_time_from_first_block: true,
        ..TxOptions::new("", q.resolve_previous_outpoints)
    };
    transactions::load(&state.db, state.node.as_ref(), &[id], &opts)
        .await?
        .into_iter()
        .next()
        .map(Json)
        .ok_or(ApiError::NotFound("Transaction not found"))
}

#[derive(Debug, Deserialize)]
pub struct BlueScoreRange {
    gte: i64,
    lt: i64,
}

#[derive(Debug, Deserialize)]
pub struct SearchBody {
    #[serde(rename = "transactionIds")]
    transaction_ids: Option<Vec<String>>,
    #[serde(rename = "acceptingBlueScores")]
    accepting_blue_scores: Option<BlueScoreRange>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    fields: String,
    #[serde(default)]
    resolve_previous_outpoints: Resolve,
    /// Only applies to searches by transaction id (as in legacy).
    acceptance: Option<AcceptanceFilter>,
}

pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
    Json(body): Json<SearchBody>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let ids = body.transaction_ids.unwrap_or_default();
    let mut opts = TxOptions::new(&q.fields, q.resolve_previous_outpoints);

    let ids: Vec<Vec<u8>> = match body.accepting_blue_scores {
        Some(_) if !ids.is_empty() => {
            return Err(ApiError::Validation(
                "Only one of transactionIds and acceptingBlueScores must be non-null".into(),
            ))
        }
        Some(r) => {
            if r.lt - r.gte > SEARCH_BLUE_SCORE_RANGE {
                return Err(ApiError::Validation(format!(
                    "Diff between acceptingBlueScores.gte and lt must be <= {SEARCH_BLUE_SCORE_RANGE}"
                )));
            }
            let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
                "SELECT ta.transaction_id
                 FROM blocks b
                 JOIN transactions_acceptances ta ON ta.block_hash = b.hash
                 WHERE b.blue_score >= $1 AND b.blue_score < $2",
            )
            .bind(r.gte)
            .bind(r.lt)
            .fetch_all(&state.db)
            .await?;
            rows.into_iter().map(|r| r.0).collect()
        }
        None => {
            if ids.len() > SEARCH_ID_LIMIT {
                return Err(ApiError::Validation(format!(
                    "Too many transaction ids. Max {SEARCH_ID_LIMIT}"
                )));
            }
            opts.acceptance = q.acceptance;
            let mut parsed = ids
                .iter()
                .map(|i| parse_id(i, "transactionId"))
                .collect::<Result<Vec<_>, _>>()?;
            parsed.sort();
            parsed.dedup();
            parsed
        }
    };
    Ok(Json(
        transactions::load(&state.db, state.node.as_ref(), &ids, &opts).await?,
    ))
}
