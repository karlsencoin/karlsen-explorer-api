//! Node-backed `/info/*` endpoints, ported from the live legacy service
//! (`rest-v2` image `v2.0.1-karlsen.13` plus its mounted files). Answers match the
//! production responses captured on 2026-10-19 (tests/fixtures/golden-info-*.json).

use std::sync::OnceLock;

use axum::{
    extract::{Query, State},
    http::header::CONTENT_TYPE,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    emission::{block_reward, SOMPI_PER_KLS},
    error::ApiError,
    node::{proto::GetBlockDagInfoResponseMessage, NodeClient},
    pyfloat::{repr, PyFloat},
    AppState,
};

/// Settings that legacy took from its environment.
struct InfoSettings {
    /// Blocks per second used by the hashrate and halving formulas (legacy env `BPS`;
    /// production runs with 1).
    bps: f64,
    /// Maximum supply in KLS reported by `/info/coinsupply`.
    max_supply_kls: u64,
}

fn settings() -> &'static InfoSettings {
    static S: OnceLock<InfoSettings> = OnceLock::new();
    S.get_or_init(|| InfoSettings {
        bps: std::env::var("BPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0),
        max_supply_kls: std::env::var("MAX_SUPPLY_KLS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4_960_848_817),
    })
}

fn node(state: &AppState) -> Result<&NodeClient, ApiError> {
    state
        .node
        .as_ref()
        .ok_or(ApiError::Unavailable("karlsend is not configured"))
}

/// Legacy answers `?stringOnly=true` and `field=` with a bare value.
fn plain_text(body: String) -> Response {
    ([(CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

#[derive(Debug, Default, Deserialize)]
pub struct StringOnly {
    #[serde(rename = "stringOnly", default)]
    string_only: bool,
}

// ---- /info/blockdag, /info/network ----

#[derive(Debug, Serialize)]
pub struct BlockDag {
    #[serde(rename = "networkName")]
    network_name: String,
    #[serde(rename = "blockCount")]
    block_count: String,
    #[serde(rename = "headerCount")]
    header_count: String,
    #[serde(rename = "tipHashes")]
    tip_hashes: Vec<String>,
    difficulty: PyFloat,
    #[serde(rename = "pastMedianTime")]
    past_median_time: String,
    #[serde(rename = "virtualParentHashes")]
    virtual_parent_hashes: Vec<String>,
    #[serde(rename = "pruningPointHash")]
    pruning_point_hash: String,
    #[serde(rename = "virtualDaaScore")]
    virtual_daa_score: String,
    sink: String,
}

fn blockdag_model(i: GetBlockDagInfoResponseMessage) -> BlockDag {
    BlockDag {
        // Legacy prefixes the bare network name ("mainnet") with "karlsen-".
        network_name: if i.network_name.starts_with("karlsen-") {
            i.network_name
        } else {
            format!("karlsen-{}", i.network_name)
        },
        block_count: i.block_count.to_string(),
        header_count: i.header_count.to_string(),
        tip_hashes: i.tip_hashes,
        difficulty: PyFloat(i.difficulty),
        past_median_time: i.past_median_time.to_string(),
        virtual_parent_hashes: i.virtual_parent_hashes,
        pruning_point_hash: i.pruning_point_hash,
        virtual_daa_score: i.virtual_daa_score.to_string(),
        sink: i.sink,
    }
}

pub async fn blockdag(State(state): State<AppState>) -> Result<Json<BlockDag>, ApiError> {
    Ok(Json(blockdag_model(
        node(&state)?.get_block_dag_info().await?,
    )))
}

// ---- /info/hashrate ----

#[derive(Debug, Serialize)]
struct Hashrate {
    hashrate: PyFloat,
}

/// Network hashrate in TH/s: `difficulty * 2 * BPS / 1e12` (legacy formula).
pub async fn hashrate(
    State(state): State<AppState>,
    Query(q): Query<StringOnly>,
) -> Result<Response, ApiError> {
    let dag = node(&state)?.get_block_dag_info().await?;
    let th = dag.difficulty * 2.0 * settings().bps / 1_000_000_000_000.0;
    Ok(if q.string_only {
        // A JSON string, as legacy returns it ("0.0").
        Json(format!("{th:.1}")).into_response()
    } else {
        Json(Hashrate {
            hashrate: PyFloat(th),
        })
        .into_response()
    })
}

// ---- /info/coinsupply ----

#[derive(Debug, Serialize)]
struct CoinSupply {
    #[serde(rename = "circulatingSupply")]
    circulating_supply: String,
    #[serde(rename = "maxSupply")]
    max_supply: String,
}

pub async fn coinsupply(State(state): State<AppState>) -> Result<Response, ApiError> {
    let s = node(&state)?.get_coin_supply().await?;
    Ok(Json(CoinSupply {
        circulating_supply: s.circulating_sompi.to_string(),
        max_supply: (settings().max_supply_kls * SOMPI_PER_KLS).to_string(),
    })
    .into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct InBillion {
    #[serde(default)]
    in_billion: bool,
}

/// Plain-text KLS amount; `/info/coinsupply/total` is the same value in legacy.
pub async fn circulating(
    State(state): State<AppState>,
    Query(q): Query<InBillion>,
) -> Result<Response, ApiError> {
    let s = node(&state)?.get_coin_supply().await?;
    let kls = s.circulating_sompi as f64 / SOMPI_PER_KLS as f64;
    Ok(plain_text(if q.in_billion {
        repr((kls / 1_000_000_000.0 * 100.0).round() / 100.0)
    } else {
        repr(kls)
    }))
}

// ---- /info/blockreward, /info/halving ----

#[derive(Debug, Serialize)]
struct BlockReward {
    blockreward: PyFloat,
}

pub async fn blockreward(
    State(state): State<AppState>,
    Query(q): Query<StringOnly>,
) -> Result<Response, ApiError> {
    let dag = node(&state)?.get_block_dag_info().await?;
    let r = block_reward(dag.virtual_daa_score);
    Ok(if q.string_only {
        Json(format!("{:.2}", r.current)).into_response()
    } else {
        Json(BlockReward {
            blockreward: PyFloat(r.current),
        })
        .into_response()
    })
}

#[derive(Debug, Default, Deserialize)]
pub struct HalvingQuery {
    field: Option<String>,
}

#[derive(Debug, Serialize)]
struct Halving {
    #[serde(rename = "nextHalvingTimestamp")]
    next_halving_timestamp: i64,
    #[serde(rename = "nextHalvingDate")]
    next_halving_date: String,
    #[serde(rename = "nextHalvingAmount")]
    next_halving_amount: PyFloat,
}

/// `YYYY-MM-DD HH:MM:SS UTC` for a Unix timestamp (proleptic Gregorian, UTC).
fn utc_date(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

pub async fn halving(
    State(state): State<AppState>,
    Query(q): Query<HalvingQuery>,
) -> Result<Response, ApiError> {
    let dag = node(&state)?.get_block_dag_info().await?;
    let daa = dag.virtual_daa_score;
    let r = block_reward(daa);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default();
    // Legacy: now + int((daa_next_halving - daa) / BPS), seconds.
    let ts = now + ((r.daa_next_halving as f64 - daa as f64) / settings().bps).trunc();
    let date = utc_date(ts.floor() as i64);
    Ok(match q.field.as_deref() {
        Some("nextHalvingTimestamp") => plain_text(repr(ts)),
        Some("nextHalvingDate") => plain_text(date),
        Some("nextHalvingAmount") => plain_text(repr(r.next)),
        _ => Json(Halving {
            next_halving_timestamp: ts as i64,
            next_halving_date: date,
            next_halving_amount: PyFloat(r.next),
        })
        .into_response(),
    })
}

// ---- /info/karlsend ----

#[derive(Debug, Serialize)]
pub struct KarlsendInfo {
    #[serde(rename = "mempoolSize")]
    mempool_size: String,
    #[serde(rename = "serverVersion")]
    server_version: String,
    #[serde(rename = "isUtxoIndexed")]
    is_utxo_indexed: bool,
    #[serde(rename = "isSynced")]
    is_synced: bool,
    /// The node's P2P id is never exposed, only its SHA-256.
    #[serde(rename = "p2pIdHashed")]
    p2p_id_hashed: String,
}

pub async fn karlsend(State(state): State<AppState>) -> Result<Json<KarlsendInfo>, ApiError> {
    let i = node(&state)?.get_info().await?;
    Ok(Json(KarlsendInfo {
        mempool_size: i.mempool_size.to_string(),
        server_version: i.server_version,
        is_utxo_indexed: i.is_utxo_indexed,
        is_synced: i.is_synced,
        p2p_id_hashed: hex::encode(Sha256::digest(i.p2p_id.as_bytes())),
    }))
}

// ---- /info/price, /info/market-data (NonKYC) ----

async fn ticker(state: &AppState) -> Result<crate::market::Ticker, ApiError> {
    match state.market.ticker().await {
        Ok((t, _stale)) => Ok(t),
        Err(e) => {
            tracing::error!(error = %e, "no KLS price available");
            Err(ApiError::Unavailable("price source unavailable"))
        }
    }
}

#[derive(Debug, Serialize)]
struct Price {
    price: PyFloat,
}

/// Last KLS/USDT price. Legacy answered `0` when NonKYC failed; this API serves the
/// last good price, or 503 if it never had one.
pub async fn price(
    State(state): State<AppState>,
    Query(q): Query<StringOnly>,
) -> Result<Response, ApiError> {
    let t = ticker(&state).await?;
    Ok(if q.string_only {
        plain_text(repr(t.last_price))
    } else {
        Json(Price {
            price: PyFloat(t.last_price),
        })
        .into_response()
    })
}

#[derive(Debug, Serialize)]
struct Usd {
    usd: PyFloat,
}

#[derive(Debug, Serialize)]
pub struct MarketData {
    current_price: Usd,
    price_change_percentage_24h: PyFloat,
    high_24h: Usd,
    low_24h: Usd,
    total_volume: Usd,
    market_cap: Usd,
    circulating_supply: PyFloat,
    best_bid: PyFloat,
    best_ask: PyFloat,
    volume_kls: PyFloat,
    previous_day_price: PyFloat,
    price_direction: &'static str,
    spread_percent: PyFloat,
}

/// Python `round(x, n)` for the magnitudes used here.
fn round_to(x: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (x * f).round() / f
}

pub async fn market_data(State(state): State<AppState>) -> Result<Json<MarketData>, ApiError> {
    let (t, supply) = tokio::join!(ticker(&state), async {
        node(&state)?
            .get_coin_supply()
            .await
            .map_err(ApiError::from)
    });
    let t = t?;
    let circulating = supply?.circulating_sompi as f64 / SOMPI_PER_KLS as f64;
    let spread = if t.bid > 0.0 {
        round_to((t.ask - t.bid) / t.bid * 100.0, 3)
    } else {
        0.0
    };
    Ok(Json(MarketData {
        current_price: Usd {
            usd: PyFloat(t.last_price),
        },
        price_change_percentage_24h: PyFloat(t.change_percent),
        high_24h: Usd {
            usd: PyFloat(t.high),
        },
        low_24h: Usd {
            usd: PyFloat(t.low),
        },
        total_volume: Usd {
            usd: PyFloat(t.usd_volume),
        },
        market_cap: Usd {
            usd: PyFloat(round_to(t.last_price * circulating, 2)),
        },
        circulating_supply: PyFloat(circulating),
        best_bid: PyFloat(t.bid),
        best_ask: PyFloat(t.ask),
        volume_kls: PyFloat(t.base_volume),
        previous_day_price: PyFloat(t.previous_day_price),
        price_direction: if t.last_price >= t.previous_day_price {
            "up"
        } else {
            "down"
        },
        spread_percent: PyFloat(spread),
    }))
}

#[cfg(test)]
mod tests {
    use super::utc_date;

    #[test]
    fn date_matches_legacy() {
        // /info/halving capture: 1793569755 -> "2026-11-01 21:49:15 UTC".
        assert_eq!(utc_date(1_793_569_755), "2026-11-01 21:49:15 UTC");
        assert_eq!(utc_date(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_date(951_782_400), "2000-02-29 00:00:00 UTC");
    }
}
