//! `GET /addresses/{address}/full-transactions` and
//! `GET /addresses/{address}/transactions-count`, from the `addresses_transactions`
//! mapping (indexed on `(address, block_time DESC)`).

use axum::{
    extract::{Path, Query, State},
    http::{header::CACHE_CONTROL, HeaderMap, HeaderValue},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    error::ApiError,
    routes::addresses::validate,
    transactions::{self, Resolve, TxOptions},
    AppState,
};

pub const MAX_LIMIT: i64 = 500;

/// The mapping table stores addresses without the `karlsen:` prefix.
fn stored_address(address: &str) -> &str {
    address.split_once(':').map_or(address, |(_, rest)| rest)
}

fn cache(max_age: u64) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        CACHE_CONTROL,
        HeaderValue::from_str(&format!("public, max-age={max_age}")).expect("ascii"),
    );
    h
}

fn default_limit() -> i64 {
    50
}

#[derive(Debug, Deserialize)]
pub struct FullTxQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    #[serde(default)]
    fields: String,
    #[serde(default)]
    resolve_previous_outpoints: Resolve,
}

/// Newest first; `limit` 1..=500 (default 50), `offset` paging.
pub async fn full_transactions(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(q): Query<FullTxQuery>,
) -> Result<(HeaderMap, Json<Vec<Value>>), ApiError> {
    validate(&address)?;
    if !(1..=MAX_LIMIT).contains(&q.limit) || q.offset < 0 {
        return Err(ApiError::Validation(format!(
            "limit must be 1..={MAX_LIMIT} and offset >= 0"
        )));
    }
    let page: Vec<(Vec<u8>, Option<i64>)> = sqlx::query_as(
        "SELECT transaction_id, block_time FROM addresses_transactions
         WHERE address = $1
         ORDER BY block_time DESC
         LIMIT $2 OFFSET $3",
    )
    .bind(stored_address(&address))
    .bind(q.limit)
    .bind(q.offset)
    .fetch_all(&state.db)
    .await?;

    // Legacy cache policy: first page short, deep pages by age of their newest entry.
    let newest = page.iter().filter_map(|p| p.1).max();
    let ttl = match (q.offset, newest) {
        (0, _) | (_, None) => 8,
        (_, Some(ms)) => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or_default();
            match (now_ms - ms) / 1000 {
                s if s < 600 => 8,
                s if s < 86_400 => 60,
                _ => 600,
            }
        }
    };

    let ids: Vec<Vec<u8>> = page.into_iter().map(|p| p.0).collect();
    let opts = TxOptions::new(&q.fields, q.resolve_previous_outpoints);
    let txs = transactions::load(&state.db, state.node.as_ref(), &ids, &opts).await?;
    Ok((cache(ttl), Json(txs)))
}

#[derive(Debug, Serialize)]
pub struct TransactionCount {
    total: i64,
}

pub async fn transactions_count(
    State(state): State<AppState>,
    Path(address): Path<String>,
) -> Result<(HeaderMap, Json<TransactionCount>), ApiError> {
    validate(&address)?;
    let key = stored_address(&address).to_string();
    let stored = key.clone();
    let db = state.db.clone();
    let (total, ttl) = state
        .tx_counts
        .get_or_load(&key, || async move {
            let (n,): (i64,) =
                sqlx::query_as("SELECT count(*) FROM addresses_transactions WHERE address = $1")
                    .bind(&stored)
                    .fetch_one(&db)
                    .await?;
            Ok::<_, ApiError>(n)
        })
        .await?;
    Ok((cache(ttl), Json(TransactionCount { total })))
}
