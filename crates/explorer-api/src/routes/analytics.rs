//! `/analytics/addresses/*`: rich list, address distribution and per-address
//! balance history. Ported from the legacy files that win route registration in
//! rest-v2: `get_analytics_addresses.py` (top, total, distribution, range),
//! `get_analytics_distribution_summary.py` and, for `/{address}/history`,
//! `get_analytics.py`.
//!
//! Numbers are computed the way Python did so the wire values match exactly:
//! `int / int` is a correctly rounded division, `round(x, n)` rounds the exact
//! binary value half-to-even, and the rich-list percentage went through
//! `decimal.Decimal` because Postgres `SUM(bigint)` arrives as `numeric`.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::{header::CACHE_CONTROL, HeaderMap, HeaderValue},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{error::ApiError, pyfloat::PyFloat, routes::addresses::validate, AppState};

const SOMPI_PER_KLS: i128 = 100_000_000;

fn cache(max_age: u64) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        CACHE_CONTROL,
        HeaderValue::from_str(&format!("public, max-age={max_age}")).expect("ascii"),
    );
    h
}

// ---- Python arithmetic ----

/// Python `a / b` for integers: the exact quotient, correctly rounded to f64.
pub fn int_div(a: i128, b: i128) -> f64 {
    if b == 0 {
        return f64::NAN;
    }
    let neg = (a < 0) != (b < 0);
    let (a, b) = (a.unsigned_abs(), b.unsigned_abs());
    if a == 0 {
        return if neg { -0.0 } else { 0.0 };
    }
    // Scale so the integer quotient has well over 53 significant bits, then
    // fold the remainder into a sticky bit: `as f64` rounds half-to-even.
    let bits = |x: u128| 128 - x.leading_zeros() as i32;
    let shift = (64 + bits(b) - bits(a)).clamp(0, 126 - bits(a));
    let num = a << shift;
    let mut q = num / b;
    if num % b != 0 {
        q |= 1;
    }
    let v = q as f64 * 2f64.powi(-shift);
    if neg {
        -v
    } else {
        v
    }
}

/// Python `round(x, n)` for floats.
pub fn py_round(x: f64, n: usize) -> f64 {
    format!("{x:.n$}").parse().unwrap_or(x)
}

/// `round(Decimal(a) / Decimal(b) * 100, 6)` converted to float: the share as a
/// percentage, rounded half-to-even at six decimals on the exact value.
pub fn decimal_percent6(a: i128, b: i128) -> f64 {
    if b == 0 {
        return 0.0;
    }
    let num = a * 100 * 1_000_000;
    let (mut q, r) = (num / b, num % b);
    let twice = 2 * r.abs();
    if twice > b.abs() || (twice == b.abs() && q % 2 != 0) {
        q += if (num < 0) != (b < 0) { -1 } else { 1 };
    }
    let sign = if q < 0 { "-" } else { "" };
    let q = q.unsigned_abs();
    format!("{sign}{}.{:06}", q / 1_000_000, q % 1_000_000)
        .parse()
        .unwrap_or(0.0)
}

fn kls8(sompi: i128) -> PyFloat {
    PyFloat(py_round(int_div(sompi, SOMPI_PER_KLS), 8))
}

// ---- rich list: /top and /range ----

#[derive(Debug, Serialize)]
pub struct AddressEntry {
    delta_24h: Option<PyFloat>,
    is_new: Option<bool>,
    address: String,
    amount: PyFloat,
    percent: PyFloat,
}

#[derive(Debug, Serialize)]
pub struct TopAddresses {
    top_addresses: Vec<AddressEntry>,
}

#[derive(Debug, Deserialize)]
pub struct TopQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct TopRow {
    address: Option<String>,
    balance_sompi: i64,
    total_sompi: i64,
    prev_sompi: Option<i64>,
    has_base: bool,
}

/// Addresses ranked at or above this position get the 24h trend marker.
const TREND_TOP_N: i64 = 1000;

pub async fn top(
    State(state): State<AppState>,
    Query(q): Query<TopQuery>,
) -> Result<(HeaderMap, Json<TopAddresses>), ApiError> {
    let limit = q.limit.unwrap_or(100);
    let offset = q.offset.unwrap_or(0);
    if !(1..=100_000).contains(&limit) {
        return Err(ApiError::Validation(
            "limit must be between 1 and 100000".into(),
        ));
    }
    if offset < 0 {
        return Err(ApiError::Validation("offset must be >= 0".into()));
    }
    // Ties on balance are ordered by address so pages are stable (legacy left
    // equal balances in Postgres' order).
    let rows: Vec<TopRow> = sqlx::query_as(
        "WITH ranked AS (
             SELECT address, amount AS balance_sompi
             FROM   balances
             WHERE  amount > 0
             ORDER  BY balance_sompi DESC, address
             LIMIT  $1 OFFSET $2
         ),
         total_supply AS (
             SELECT COALESCE(SUM(amount), 1)::bigint AS total_sompi
             FROM   balances WHERE amount > 0
         ),
         base AS (
             SELECT MAX(snapshot_date) AS d
             FROM   top_balance_snapshots
             WHERE  snapshot_date < CURRENT_DATE
         )
         SELECT r.address, r.balance_sompi, ts.total_sompi,
                b.amount AS prev_sompi, (base.d IS NOT NULL) AS has_base
         FROM   ranked r
         CROSS  JOIN total_supply ts
         CROSS  JOIN base
         LEFT   JOIN top_balance_snapshots b
                ON b.snapshot_date = base.d AND b.address = r.address
         ORDER  BY r.balance_sompi DESC, r.address",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?;

    let entries = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let (mut delta, mut is_new) = (None, None);
            if r.has_base && offset + (i as i64) < TREND_TOP_N {
                match r.prev_sompi {
                    None => is_new = Some(true),
                    Some(prev) => {
                        delta = Some(kls8(r.balance_sompi as i128 - prev as i128));
                    }
                }
            }
            AddressEntry {
                delta_24h: delta,
                is_new,
                address: r.address.unwrap_or_default(),
                amount: kls8(r.balance_sompi as i128),
                percent: PyFloat(decimal_percent6(
                    r.balance_sompi as i128,
                    r.total_sompi as i128,
                )),
            }
        })
        .collect();
    Ok((
        cache(300),
        Json(TopAddresses {
            top_addresses: entries,
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct RangeQuery {
    min_amount: Option<f64>,
    max_amount: Option<f64>,
}

/// `[min_amount, max_amount)` in sompi; `max_amount < 0` means no upper bound.
/// Python `int(x * 100_000_000)` truncates toward zero.
fn sompi_bounds(q: &RangeQuery) -> Result<(i64, Option<i64>), ApiError> {
    let min = q
        .min_amount
        .ok_or_else(|| ApiError::Validation("min_amount is required".into()))?;
    if min.is_nan() || min < 0.0 {
        return Err(ApiError::Validation("min_amount must be >= 0".into()));
    }
    let max = q.max_amount.unwrap_or(-1.0);
    let to_sompi = |x: f64| (x * SOMPI_PER_KLS as f64).trunc() as i64;
    Ok((to_sompi(min), (max >= 0.0).then(|| to_sompi(max))))
}

#[derive(sqlx::FromRow)]
struct RangeRow {
    address: Option<String>,
    balance_sompi: i64,
    total_sompi: i64,
}

pub async fn range(
    State(state): State<AppState>,
    Query(q): Query<RangeQuery>,
) -> Result<(HeaderMap, Json<Vec<AddressEntry>>), ApiError> {
    let (min, max) = sompi_bounds(&q)?;
    let rows: Vec<RangeRow> = sqlx::query_as(
        "WITH address_totals AS (
             SELECT address, amount AS balance_sompi
             FROM   balances
             WHERE  amount >= $1 AND ($2::bigint IS NULL OR amount < $2)
         ),
         total_supply AS (
             SELECT COALESCE(SUM(amount), 1)::bigint AS total_sompi
             FROM   balances WHERE amount > 0
         )
         SELECT at.address, at.balance_sompi, ts.total_sompi
         FROM   address_totals at, total_supply ts
         ORDER  BY at.balance_sompi DESC, at.address",
    )
    .bind(min)
    .bind(max)
    .fetch_all(&state.db)
    .await?;
    let entries = rows
        .into_iter()
        .map(|r| AddressEntry {
            delta_24h: None,
            is_new: None,
            address: r.address.unwrap_or_default(),
            amount: kls8(r.balance_sompi as i128),
            percent: PyFloat(decimal_percent6(
                r.balance_sompi as i128,
                r.total_sompi as i128,
            )),
        })
        .collect();
    Ok((cache(120), Json(entries)))
}

// ---- counts: /total and /distribution ----

#[derive(Debug, Serialize)]
pub struct Total {
    total_addresses: i64,
}

pub async fn total(State(state): State<AppState>) -> Result<(HeaderMap, Json<Total>), ApiError> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM balances WHERE amount > 0")
        .fetch_one(&state.db)
        .await?;
    Ok((cache(120), Json(Total { total_addresses: n })))
}

#[derive(Debug, Serialize)]
pub struct Distribution {
    from_addresses_total: i64,
}

pub async fn distribution(
    State(state): State<AppState>,
    Query(q): Query<RangeQuery>,
) -> Result<(HeaderMap, Json<Distribution>), ApiError> {
    let (min, max) = sompi_bounds(&q)?;
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM balances
         WHERE amount >= $1 AND ($2::bigint IS NULL OR amount < $2)",
    )
    .bind(min)
    .bind(max)
    .fetch_one(&state.db)
    .await?;
    Ok((
        cache(120),
        Json(Distribution {
            from_addresses_total: n,
        }),
    ))
}

// ---- /distribution-summary ----

const KLS: i64 = SOMPI_PER_KLS as i64;
const TIERS: [(&str, i64, Option<i64>); 8] = [
    ("10M+", 10_000_000 * KLS, None),
    ("1M-10M", 1_000_000 * KLS, Some(10_000_000 * KLS)),
    ("500K-1M", 500_000 * KLS, Some(1_000_000 * KLS)),
    ("100K-500K", 100_000 * KLS, Some(500_000 * KLS)),
    ("10K-100K", 10_000 * KLS, Some(100_000 * KLS)),
    ("1K-10K", 1_000 * KLS, Some(10_000 * KLS)),
    ("100-1K", 100 * KLS, Some(1_000 * KLS)),
    ("1-100", KLS, Some(100 * KLS)),
];

fn live_tiers_sql() -> String {
    let cases: Vec<String> = TIERS
        .iter()
        .map(|(id, lo, hi)| match hi {
            None => format!("WHEN amount >= {lo} THEN '{id}'"),
            Some(hi) => format!("WHEN amount >= {lo} AND amount < {hi} THEN '{id}'"),
        })
        .collect();
    format!(
        "WITH bucketed AS (
             SELECT CASE {} END AS tier, amount
             FROM   balances
             WHERE  amount >= $1
         )
         SELECT tier, COUNT(*) AS addresses, SUM(amount)::BIGINT AS total_sompi
         FROM   bucketed
         WHERE  tier IS NOT NULL
         GROUP  BY tier",
        cases.join(" ")
    )
}

#[derive(Debug, Serialize)]
pub struct TierEntry {
    tier: &'static str,
    addresses: i64,
    balance_kls: PyFloat,
    balance_percent: PyFloat,
    delta_1d: Option<i64>,
    delta_7d: Option<i64>,
    delta_1d_kls: Option<PyFloat>,
    delta_7d_kls: Option<PyFloat>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    total_addresses: i64,
    total_balance_kls: PyFloat,
    total_delta_1d: Option<i64>,
    total_delta_7d: Option<i64>,
    total_delta_1d_kls: Option<PyFloat>,
    total_delta_7d_kls: Option<PyFloat>,
    tiers: Vec<TierEntry>,
}

/// A daily snapshot: address count and (for rows written after the column was
/// added) KLS balance per tier, in row order.
struct Snapshot {
    counts: HashMap<String, i64>,
    sompi: HashMap<String, Option<i64>>,
}

async fn snapshot(db: &sqlx::PgPool, days: i32) -> Result<Option<Snapshot>, ApiError> {
    let rows: Vec<(String, i32, Option<i64>)> = sqlx::query_as(
        "SELECT tier::text, address_count, tier_sompi FROM distribution_snapshots
         WHERE snapshot_date = CURRENT_DATE - make_interval(days => $1)",
    )
    .bind(days)
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut s = Snapshot {
        counts: HashMap::new(),
        sompi: HashMap::new(),
    };
    for (tier, count, sompi) in rows {
        s.counts.insert(tier.clone(), count as i64);
        s.sompi.insert(tier, sompi);
    }
    Ok(Some(s))
}

/// Sum, or `None` when any baseline value is missing.
fn sum_or_none(snap: &Snapshot) -> Option<i64> {
    snap.sompi.values().copied().sum::<Option<i64>>()
}

fn delta_kls(now: i64, base: Option<i64>) -> Option<PyFloat> {
    base.map(|b| kls8(now as i128 - b as i128))
}

pub async fn distribution_summary(
    State(state): State<AppState>,
) -> Result<(HeaderMap, Json<Summary>), ApiError> {
    let live: Vec<(Option<String>, i64, Option<i64>)> = sqlx::query_as(&live_tiers_sql())
        .bind(KLS)
        .fetch_all(&state.db)
        .await?;
    let live: HashMap<String, (i64, i64)> = live
        .into_iter()
        .filter_map(|(t, n, s)| t.map(|t| (t, (n, s.unwrap_or(0)))))
        .collect();
    let d1 = snapshot(&state.db, 1).await?;
    let d7 = snapshot(&state.db, 7).await?;

    let per_tier: Vec<(&'static str, i64, i64)> = TIERS
        .iter()
        .map(|(id, _, _)| {
            let (n, s) = live.get(*id).copied().unwrap_or((0, 0));
            (*id, n, s)
        })
        .collect();
    let total_sompi: i64 = per_tier.iter().map(|t| t.2).sum();

    let tiers: Vec<TierEntry> = per_tier
        .iter()
        .map(|&(id, cnt, sompi)| {
            let pct = if total_sompi != 0 {
                int_div(sompi as i128, total_sompi as i128) * 100.0
            } else {
                0.0
            };
            let base = |snap: &Option<Snapshot>| {
                snap.as_ref()
                    .and_then(|s| s.sompi.get(id).copied().flatten())
            };
            TierEntry {
                tier: id,
                addresses: cnt,
                balance_kls: kls8(sompi as i128),
                balance_percent: PyFloat(py_round(pct, 4)),
                delta_1d: d1.as_ref().and_then(|s| s.counts.get(id)).map(|b| cnt - b),
                delta_7d: d7.as_ref().and_then(|s| s.counts.get(id)).map(|b| cnt - b),
                delta_1d_kls: delta_kls(sompi, base(&d1)),
                delta_7d_kls: delta_kls(sompi, base(&d7)),
            }
        })
        .collect();

    let total_addr: i64 = tiers.iter().map(|t| t.addresses).sum();
    let prev_addr = |s: &Option<Snapshot>| s.as_ref().map(|s| s.counts.values().sum::<i64>());
    let prev_sompi = |s: &Option<Snapshot>| s.as_ref().and_then(sum_or_none);

    Ok((
        cache(300),
        Json(Summary {
            total_addresses: total_addr,
            total_balance_kls: kls8(total_sompi as i128),
            total_delta_1d: prev_addr(&d1).map(|p| total_addr - p),
            total_delta_7d: prev_addr(&d7).map(|p| total_addr - p),
            total_delta_1d_kls: delta_kls(total_sompi, prev_sompi(&d1)),
            total_delta_7d_kls: delta_kls(total_sompi, prev_sompi(&d7)),
            tiers,
        }),
    ))
}

// ---- /{address}/history ----

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct HistoryEntry {
    transaction_id: String,
    hash: String,
    block_time: i64,
    received_sompi: i64,
    sent_sompi: i64,
    net_sompi: i64,
}

#[derive(Debug, Serialize)]
pub struct History {
    address: String,
    total: i64,
    transactions: Vec<HistoryEntry>,
}

/// Same query as legacy: received = outputs to the address, sent = previous
/// outputs of the address spent by the transaction, newest first.
const HISTORY_SQL: &str = "
WITH addr_txs AS (
    SELECT t.transaction_id, t.hash, t.block_time, t.outputs, t.inputs
    FROM addresses_transactions ata
    JOIN transactions t ON t.transaction_id = ata.transaction_id
    WHERE ata.address = $1
    ORDER BY ata.block_time DESC
    LIMIT $2 OFFSET $3
),
received_agg AS (
    SELECT atx.transaction_id, COALESCE(SUM((o).amount), 0)::BIGINT AS received_sompi
    FROM addr_txs atx
    CROSS JOIN LATERAL unnest(atx.outputs) AS o
    WHERE (o).script_public_key_address = $1
    GROUP BY atx.transaction_id
),
sent_agg AS (
    SELECT atx.transaction_id, COALESCE(SUM((prev_o).amount), 0)::BIGINT AS sent_sompi
    FROM addr_txs atx
    CROSS JOIN LATERAL unnest(atx.inputs) AS inp
    JOIN transactions prev_t ON prev_t.transaction_id = (inp).previous_outpoint_hash
    CROSS JOIN LATERAL unnest(prev_t.outputs) AS prev_o
    WHERE (prev_o).index = (inp).previous_outpoint_index
      AND (prev_o).script_public_key_address = $1
    GROUP BY atx.transaction_id
)
SELECT
    encode(atx.transaction_id, 'hex')                            AS transaction_id,
    COALESCE(encode(atx.hash, 'hex'), '')                        AS hash,
    COALESCE(atx.block_time, 0)                                  AS block_time,
    COALESCE(r.received_sompi, 0)::BIGINT                        AS received_sompi,
    COALESCE(s.sent_sompi, 0)::BIGINT                            AS sent_sompi,
    (COALESCE(r.received_sompi, 0) - COALESCE(s.sent_sompi, 0))::BIGINT AS net_sompi
FROM addr_txs atx
LEFT JOIN received_agg r ON r.transaction_id = atx.transaction_id
LEFT JOIN sent_agg     s ON s.transaction_id = atx.transaction_id
ORDER BY atx.block_time DESC";

pub async fn history(
    State(state): State<AppState>,
    Path(address): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<History>, ApiError> {
    validate(&address)?;
    let limit = q.limit.unwrap_or(20);
    let offset = q.offset.unwrap_or(0);
    if !(1..=200).contains(&limit) {
        return Err(ApiError::Validation(
            "limit must be between 1 and 200".into(),
        ));
    }
    if offset < 0 {
        return Err(ApiError::Validation("offset must be >= 0".into()));
    }
    let stored = address
        .split_once(':')
        .map_or(address.as_str(), |(_, rest)| rest)
        .to_string();

    // Same count as /transactions-count, so it shares that cache.
    let (db, key) = (state.db.clone(), stored.clone());
    let (total, _) = state
        .tx_counts
        .get_or_load(&stored, || async move {
            let (n,): (i64,) =
                sqlx::query_as("SELECT count(*) FROM addresses_transactions WHERE address = $1")
                    .bind(&key)
                    .fetch_one(&db)
                    .await?;
            Ok::<_, ApiError>(n)
        })
        .await?;
    if total == 0 {
        return Ok(Json(History {
            address,
            total: 0,
            transactions: Vec::new(),
        }));
    }
    let transactions: Vec<HistoryEntry> = sqlx::query_as(HISTORY_SQL)
        .bind(&stored)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(History {
        address,
        total,
        transactions,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_div_is_correctly_rounded() {
        // Values Python computes with exact integer division (checked in CPython).
        assert_eq!(int_div(1, 3), 1.0 / 3.0);
        assert_eq!(int_div(431_780_366_403_077, 100_000_000), 4317803.66403077);
        // Above 2^53 the naive `a as f64 / b as f64` can be off by one ulp.
        assert_eq!(
            int_div(493_123_456_789_012_345, 496_084_881_700_000_000),
            0.9940304068512643
        );
        assert_eq!(int_div(-5, 2), -2.5);
    }

    #[test]
    fn python_round_and_decimal_percent() {
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(2.675, 2), 2.67);
        assert_eq!(decimal_percent6(1, 3), 33.333333);
        assert_eq!(decimal_percent6(1, 16_000_000), 0.000006); // 0.00000625 -> even
        assert_eq!(decimal_percent6(5, 1), 500.0);
    }

    #[test]
    fn range_bounds_truncate_like_python() {
        let q = RangeQuery {
            min_amount: Some(0.1),
            max_amount: None,
        };
        assert_eq!(sompi_bounds(&q).unwrap(), (10_000_000, None));
        let q = RangeQuery {
            min_amount: Some(1.0),
            max_amount: Some(100.0),
        };
        assert_eq!(
            sompi_bounds(&q).unwrap(),
            (100_000_000, Some(10_000_000_000))
        );
        assert!(sompi_bounds(&RangeQuery {
            min_amount: None,
            max_amount: None
        })
        .is_err());
    }
}
