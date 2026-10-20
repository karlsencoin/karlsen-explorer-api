//! `GET /info/dag-window`: the newest blocks (or the blocks after a DAA score) with
//! their parent links, for the explorer's live DAG view. Ported from legacy
//! `endpoints/get_dag_window.py`; served from the database only, as in legacy.

use axum::{
    extract::{Query, State},
    http::{header::CACHE_CONTROL, HeaderName, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, AppState};

const DEFAULT_LIMIT: i64 = 200;
const MAX_LIMIT: i64 = 1000;

#[derive(Debug, Deserialize)]
pub struct DagWindowQuery {
    limit: Option<i64>,
    since_daa: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct DagWindowBlock {
    hash: String,
    #[serde(rename = "daaScore")]
    daa_score: i64,
    #[serde(rename = "blueScore")]
    blue_score: i64,
    parents: Vec<String>,
    /// Always `null`: legacy never filled it in.
    #[serde(rename = "isChainBlock")]
    is_chain_block: Option<bool>,
    #[serde(rename = "selectedParentHash")]
    selected_parent_hash: Option<String>,
    #[serde(rename = "mergeSetBluesHashes")]
    merge_set_blues_hashes: Vec<String>,
    #[serde(rename = "mergeSetRedsHashes")]
    merge_set_reds_hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DagWindow {
    count: usize,
    #[serde(rename = "newestDaa")]
    newest_daa: Option<i64>,
    #[serde(rename = "oldestDaa")]
    oldest_daa: Option<i64>,
    blocks: Vec<DagWindowBlock>,
}

#[derive(sqlx::FromRow)]
struct Row {
    hash: Vec<u8>,
    daa_score: Option<i64>,
    blue_score: Option<i64>,
    selected_parent_hash: Option<Vec<u8>>,
    merge_set_blues_hashes: Option<Vec<Vec<u8>>>,
    merge_set_reds_hashes: Option<Vec<Vec<u8>>>,
}

fn hexes(v: Option<Vec<Vec<u8>>>) -> Vec<String> {
    v.into_iter().flatten().map(hex::encode).collect()
}

/// Selected parent first, then merge-set blues and reds, without duplicates
/// (the legacy order).
fn parents(selected: &Option<String>, blues: &[String], reds: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(1 + blues.len() + reds.len());
    for h in selected.iter().chain(blues).chain(reds) {
        if !h.is_empty() && !out.contains(h) {
            out.push(h.clone());
        }
    }
    out
}

fn block_model(r: Row) -> DagWindowBlock {
    let selected = r.selected_parent_hash.as_deref().map(hex::encode);
    let blues = hexes(r.merge_set_blues_hashes);
    let reds = hexes(r.merge_set_reds_hashes);
    DagWindowBlock {
        hash: hex::encode(&r.hash),
        daa_score: r.daa_score.unwrap_or(0),
        blue_score: r.blue_score.unwrap_or(0),
        parents: parents(&selected, &blues, &reds),
        is_chain_block: None,
        selected_parent_hash: selected,
        merge_set_blues_hashes: blues,
        merge_set_reds_hashes: reds,
    }
}

pub async fn dag_window(
    State(state): State<AppState>,
    Query(q): Query<DagWindowQuery>,
) -> Result<Response, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::Validation(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    if q.since_daa.is_some_and(|d| d < 0) {
        return Err(ApiError::Validation("since_daa must be >= 0".into()));
    }

    // Ties on daa_score are broken by hash so repeated calls return the same list
    // (legacy left the order of equal scores to Postgres).
    //
    // Both orders must stay walkable from `idx_blocks_daa_score (daa_score DESC)`
    // plus an incremental sort on hash. Do not add `NULLS LAST` here: it no longer
    // matches the index, and Postgres falls back to sorting all ~18M blocks
    // (110 s per call, which took the VPS down on 2026-10-19). See docs/INDEXES.md.
    let rows: Vec<Row> = match q.since_daa {
        Some(since) => {
            sqlx::query_as(
                "SELECT hash, daa_score, blue_score, selected_parent_hash,
                        merge_set_blues_hashes, merge_set_reds_hashes
                 FROM blocks WHERE daa_score > $1
                 ORDER BY daa_score ASC, hash ASC LIMIT $2",
            )
            .bind(since)
            .bind(limit)
            .fetch_all(&state.db)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT hash, daa_score, blue_score, selected_parent_hash,
                        merge_set_blues_hashes, merge_set_reds_hashes
                 FROM blocks WHERE daa_score IS NOT NULL
                 ORDER BY daa_score DESC, hash DESC LIMIT $1",
            )
            .bind(limit)
            .fetch_all(&state.db)
            .await?
        }
    };

    let blocks: Vec<DagWindowBlock> = rows.into_iter().map(block_model).collect();
    let body = DagWindow {
        count: blocks.len(),
        newest_daa: blocks.iter().map(|b| b.daa_score).max(),
        oldest_daa: blocks.iter().map(|b| b.daa_score).min(),
        blocks,
    };

    let cache = if q.since_daa.is_some() {
        "no-store"
    } else {
        "public, max-age=2"
    };
    let mut resp = Json(body).into_response();
    let h = resp.headers_mut();
    h.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        HeaderName::from_static("x-data-source"),
        HeaderValue::from_static("Database"),
    );
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::parents;

    #[test]
    fn selected_parent_first_without_duplicates() {
        let sel = Some("aa".to_string());
        let blues = vec!["aa".to_string(), "bb".to_string()];
        let reds = vec!["cc".to_string(), "bb".to_string()];
        assert_eq!(parents(&sel, &blues, &reds), ["aa", "bb", "cc"]);
    }

    #[test]
    fn missing_selected_parent() {
        assert_eq!(parents(&None, &["bb".into()], &[]), ["bb"]);
    }
}
