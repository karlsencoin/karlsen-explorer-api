//! Rolling 24 h coinbase payouts per address, for the KGI leaderboard's
//! `day_kls` column.
//!
//! Source of truth is the indexer's Postgres: coinbase outputs of transactions
//! that were actually *accepted*. Every block carries a coinbase paying its
//! merge set, but only the coinbase of a selected-chain block is accepted;
//! counting every block's coinbase (as the reward tracker's ledger does)
//! overstates payouts by the share of off-chain blocks (~26 % measured).
//!
//! Scanning a full day takes about a minute, so the totals are kept in memory
//! as per-minute buckets: one background pass fills the last 24 h in hourly
//! slices, after which every refresh recomputes only the most recent minutes
//! (late acceptances land there) and drops buckets older than a day.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{OnceLock, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use sqlx::PgPool;

use crate::address::{script_to_address, MAINNET_PREFIX};

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 24 * 3600 * 1000;
/// Minutes recomputed on every refresh, so acceptances that arrive after the
/// block's own timestamp are still counted.
const REPLAY_MINUTES: i64 = 10;
const BACKFILL_SLICE_MS: i64 = 3600 * 1000;
const COINBASE_SUBNETWORK: &str = "0100000000000000000000000000000000000000";

/// minute start (ms) -> address -> sompi
type Buckets = BTreeMap<i64, HashMap<String, i64>>;

#[derive(Default)]
struct State {
    buckets: Buckets,
    /// Everything before this instant (ms) is final; refreshes start no later.
    frontier_ms: Option<i64>,
    /// The first full 24 h backfill has completed.
    ready: bool,
}

fn state() -> &'static RwLock<State> {
    static S: OnceLock<RwLock<State>> = OnceLock::new();
    S.get_or_init(|| RwLock::new(State::default()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn floor_minute(ms: i64) -> i64 {
    ms - ms.rem_euclid(MINUTE_MS)
}

/// Per-address sompi received from accepted coinbase outputs in the last 24 h,
/// or `None` until the first backfill has finished.
pub fn day_totals() -> Option<HashMap<String, i64>> {
    let s = state().read().ok()?;
    if !s.ready {
        return None;
    }
    Some(sum_since(&s.buckets, now_ms() - DAY_MS))
}

/// Per-address sompi from accepted coinbase outputs with `block_time` in
/// `[from_ms, to_ms)` (minute granularity), or `None` until the first backfill
/// has finished. Ranges older than 24 h are not covered by the cache.
pub fn range_totals(from_ms: i64, to_ms: i64) -> Option<HashMap<String, i64>> {
    let s = state().read().ok()?;
    if !s.ready {
        return None;
    }
    Some(sum_range(&s.buckets, from_ms, to_ms))
}

fn sum_since(buckets: &Buckets, since_ms: i64) -> HashMap<String, i64> {
    sum_range(buckets, since_ms, i64::MAX)
}

fn sum_range(buckets: &Buckets, from_ms: i64, to_ms: i64) -> HashMap<String, i64> {
    let mut out: HashMap<String, i64> = HashMap::new();
    let (lo, hi) = (floor_minute(from_ms), floor_minute(to_ms));
    if lo >= hi {
        return out;
    }
    for per_addr in buckets.range(lo..hi).map(|(_, v)| v) {
        for (addr, sompi) in per_addr {
            *out.entry(addr.clone()).or_default() += sompi;
        }
    }
    out
}

/// Accepted coinbase payouts with `block_time` in `[from_ms, to_ms)`, bucketed.
async fn fetch(db: &PgPool, from_ms: i64, to_ms: i64) -> Result<Buckets, sqlx::Error> {
    let rows: Vec<(i64, Option<Vec<u8>>, i64)> = sqlx::query_as(
        r#"SELECT (t.block_time / 60000) * 60000 AS minute,
                  o.script_public_key,
                  SUM(o.amount)::bigint AS sompi
           FROM transactions t
           JOIN subnetworks s ON s.id = t.subnetwork_id
           CROSS JOIN unnest(t.outputs) AS o
           WHERE s.subnetwork_id = $1
             AND t.block_time >= $2 AND t.block_time < $3
             AND EXISTS (SELECT 1 FROM transactions_acceptances ta
                         WHERE ta.transaction_id = t.transaction_id)
           GROUP BY 1, 2"#,
    )
    .bind(COINBASE_SUBNETWORK)
    .bind(from_ms)
    .bind(to_ms)
    .fetch_all(db)
    .await?;

    let mut out = Buckets::new();
    for (minute, script, sompi) in rows {
        let Some(addr) = script.and_then(|s| script_to_address(MAINNET_PREFIX, &s)) else {
            continue;
        };
        *out.entry(minute).or_default().entry(addr).or_default() += sompi;
    }
    Ok(out)
}

/// Replace every bucket in `[from_ms, ..)` with `fresh`, then drop buckets
/// that left the 24 h window.
fn merge(buckets: &mut Buckets, from_ms: i64, fresh: Buckets, now: i64) {
    buckets.retain(|&minute, _| minute < from_ms);
    buckets.extend(fresh);
    let oldest = floor_minute(now - DAY_MS);
    buckets.retain(|&minute, _| minute >= oldest);
}

async fn backfill(db: &PgPool) -> Result<(), sqlx::Error> {
    let now = now_ms();
    let end = floor_minute(now - REPLAY_MINUTES * MINUTE_MS);
    let mut from = floor_minute(now - DAY_MS);
    let mut all = Buckets::new();
    while from < end {
        let to = (from + BACKFILL_SLICE_MS).min(end);
        all.extend(fetch(db, from, to).await?);
        from = to;
    }
    let mut s = state().write().expect("day_rewards lock poisoned");
    let start = s.frontier_ms.unwrap_or(end).min(end);
    // Keep anything a refresh already wrote past the backfill's end.
    let newer: Buckets = s.buckets.split_off(&end);
    s.buckets = all;
    s.buckets.extend(newer);
    s.frontier_ms = Some(start);
    s.ready = true;
    Ok(())
}

async fn refresh(db: &PgPool) -> Result<(), sqlx::Error> {
    let now = now_ms();
    let replay = floor_minute(now - REPLAY_MINUTES * MINUTE_MS);
    let from = state()
        .read()
        .ok()
        .and_then(|s| s.frontier_ms)
        .map_or(replay, |f| f.min(replay));
    let fresh = fetch(db, from, now + MINUTE_MS).await?;
    let mut s = state().write().expect("day_rewards lock poisoned");
    merge(&mut s.buckets, from, fresh, now);
    s.frontier_ms = Some(replay);
    Ok(())
}

/// Start the background backfill and the periodic refresh.
pub fn spawn(db: PgPool, every: Duration) {
    tokio::spawn(async move {
        loop {
            let t0 = std::time::Instant::now();
            match backfill(&db).await {
                Ok(()) => {
                    tracing::info!(secs = t0.elapsed().as_secs(), "day_rewards: 24h backfill done");
                    break;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "day_rewards: backfill failed, retrying in 60s");
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            }
        }
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = refresh(&db).await {
                tracing::warn!(error = %e, "day_rewards: refresh failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(a, s)| (a.to_string(), *s)).collect()
    }

    #[test]
    fn merge_replaces_replayed_minutes_and_drops_old_ones() {
        let now = 10 * DAY_MS;
        let mut b = Buckets::new();
        b.insert(now - DAY_MS - 5 * MINUTE_MS, bucket(&[("a", 7)])); // too old
        b.insert(now - 30 * MINUTE_MS, bucket(&[("a", 1)])); // kept
        b.insert(now - 5 * MINUTE_MS, bucket(&[("a", 100)])); // replayed
        let mut fresh = Buckets::new();
        fresh.insert(now - 5 * MINUTE_MS, bucket(&[("a", 2), ("b", 3)]));
        merge(&mut b, now - 10 * MINUTE_MS, fresh, now);

        let sums = sum_since(&b, now - DAY_MS);
        assert_eq!(sums["a"], 3);
        assert_eq!(sums["b"], 3);
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn sum_range_is_half_open_on_minutes() {
        let mut b = Buckets::new();
        b.insert(0, bucket(&[("a", 1)]));
        b.insert(MINUTE_MS, bucket(&[("a", 10)]));
        b.insert(2 * MINUTE_MS, bucket(&[("a", 100)]));
        assert_eq!(sum_range(&b, MINUTE_MS, 2 * MINUTE_MS)["a"], 10);
        assert_eq!(sum_range(&b, 0, 2 * MINUTE_MS + 5)["a"], 11);
        assert!(sum_range(&b, MINUTE_MS, MINUTE_MS).is_empty());
    }

    #[test]
    fn not_ready_until_backfill() {
        assert!(day_totals().is_none());
    }
}
