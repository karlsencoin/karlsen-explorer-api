//! `/info/miners/*`: miner leaderboard and network stats from the KGI reward
//! tracker. Ported from legacy `endpoints/get_miners.py`.
//!
//! The data lives in the tracker's SQLite file (`KGI_TRACKER_DB`, default
//! `/opt/karlsen/kgi-backend/data/kgi.db`), opened read-only per request; Postgres is not used.
//! SQLite columns are dynamically typed, so values are passed through with their
//! stored type (an INTEGER stays an integer, a REAL is spelled like Python's
//! `repr`), exactly as legacy's `sqlite3` rows were.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Path as UrlPath, Query},
    http::header::CONTENT_TYPE,
    response::{IntoResponse, Response},
};
use rusqlite::{
    params_from_iter,
    types::{Value, ValueRef},
    Connection, OpenFlags,
};
use serde::{Deserialize, Serialize, Serializer};

use crate::{error::ApiError, pyfloat::PyFloat};

const STALE_AFTER_MS: i64 = 90_000;
const CHUNK: usize = 500;
const SOMPI_PER_KLS: f64 = 1e8;

fn db_path() -> &'static Path {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| {
        std::env::var("KGI_TRACKER_DB")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/opt/karlsen/kgi-backend/data/kgi.db".into())
            .into()
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn is_stale(ts_ms: Option<i64>) -> bool {
    ts_ms.is_none_or(|ts| now_ms() - ts > STALE_AFTER_MS)
}

fn open(path: &Path) -> Result<Connection, ApiError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(ApiError::from)
}

/// Runs blocking SQLite work off the async runtime.
async fn with_db<T, F>(f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let conn = open(db_path())?;
        f(&conn)
    })
    .await
    .map_err(|_| ApiError::Unavailable("tracker task failed"))?
}

/// Serializes straight to text: going through `serde_json::Value` would respell
/// Python-style floats (`1e+16` -> `1e16`).
fn to_json<T: Serialize>(v: &T) -> Result<String, ApiError> {
    serde_json::to_string(v).map_err(|_| ApiError::Unavailable("serialization failed"))
}

fn json_response(body: String) -> Response {
    ([(CONTENT_TYPE, "application/json")], body).into_response()
}

// ---- dynamically typed cells ----

/// One SQLite value, serialized the way Python's `json` writes the `sqlite3` value.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

impl From<ValueRef<'_>> for Cell {
    fn from(v: ValueRef<'_>) -> Self {
        match v {
            ValueRef::Null => Cell::Null,
            ValueRef::Integer(i) => Cell::Int(i),
            ValueRef::Real(f) => Cell::Real(f),
            ValueRef::Text(t) => Cell::Text(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => Cell::Text(hex::encode(b)),
        }
    }
}

impl Cell {
    fn as_i64(&self) -> Option<i64> {
        match self {
            Cell::Int(i) => Some(*i),
            Cell::Real(f) => Some(*f as i64),
            _ => None,
        }
    }
    /// Python `x / 1e8` (an int is converted to float first).
    fn kls(&self) -> f64 {
        match self {
            Cell::Int(i) => *i as f64 / SOMPI_PER_KLS,
            Cell::Real(f) => *f / SOMPI_PER_KLS,
            _ => 0.0,
        }
    }
    fn text(&self) -> Option<&str> {
        match self {
            Cell::Text(s) => Some(s),
            _ => None,
        }
    }
}

impl Serialize for Cell {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Cell::Null => s.serialize_none(),
            Cell::Int(i) => s.serialize_i64(*i),
            Cell::Real(f) => PyFloat(*f).serialize(s),
            Cell::Text(t) => s.serialize_str(t),
        }
    }
}

type Row = HashMap<String, Cell>;

fn query(conn: &Connection, sql: &str, params: &[Value]) -> rusqlite::Result<Vec<Row>> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let rows = stmt.query_map(params_from_iter(params.iter()), |r| {
        let mut m = Row::with_capacity(names.len());
        for (i, n) in names.iter().enumerate() {
            m.insert(n.clone(), Cell::from(r.get_ref(i)?));
        }
        Ok(m)
    })?;
    rows.collect()
}

fn get<'a>(r: &'a Row, k: &str) -> &'a Cell {
    r.get(k).unwrap_or(&Cell::Null)
}

fn marks(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn text_params(v: &[String]) -> Vec<Value> {
    v.iter().map(|s| Value::Text(s.clone())).collect()
}

// ---- tracker helpers (same logic as legacy) ----

fn latest_ts(conn: &Connection) -> Result<Option<i64>, ApiError> {
    let rows = query(
        conn,
        "SELECT ts_ms FROM network_snapshot ORDER BY ts_ms DESC LIMIT 1",
        &[],
    )?;
    Ok(rows.first().and_then(|r| get(r, "ts_ms").as_i64()))
}

/// Software reported for a miner address; the most recently seen row wins.
#[derive(Debug, Clone, Serialize)]
pub struct Software {
    software: Cell,
    node_version: Cell,
    others: Vec<Cell>,
}

fn software_map(conn: &Connection, addresses: &[String]) -> HashMap<String, Software> {
    let mut out: HashMap<String, Software> = HashMap::new();
    // Legacy swallows every error here (table missing on an old tracker) and keeps
    // what it read so far.
    let _ = (|| -> rusqlite::Result<()> {
        for chunk in addresses.chunks(CHUNK) {
            let sql = format!(
                "SELECT address, software, node_version, last_seen_ms, blocks \
                 FROM miner_software WHERE address IN ({}) ORDER BY last_seen_ms DESC",
                marks(chunk.len())
            );
            for r in query(conn, &sql, &text_params(chunk))? {
                let Some(addr) = get(&r, "address").text() else {
                    continue;
                };
                let software = get(&r, "software").clone();
                match out.get_mut(addr) {
                    None => {
                        out.insert(
                            addr.to_string(),
                            Software {
                                software,
                                node_version: get(&r, "node_version").clone(),
                                others: Vec::new(),
                            },
                        );
                    }
                    Some(e) => {
                        if !e.others.contains(&software) {
                            e.others.push(software);
                        }
                    }
                }
            }
        }
        Ok(())
    })();
    out
}

#[derive(Debug, Clone, Default)]
struct AllTime {
    sompi: i64,
    blocks: i64,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
}

impl AllTime {
    fn add(&mut self, sompi: &Cell, blocks: &Cell, first: &Cell, last: &Cell) {
        self.sompi += sompi.as_i64().unwrap_or(0);
        self.blocks += blocks.as_i64().unwrap_or(0);
        if let Some(f) = first.as_i64() {
            self.first_ms = Some(self.first_ms.map_or(f, |x| x.min(f)));
        }
        if let Some(l) = last.as_i64() {
            self.last_ms = Some(self.last_ms.map_or(l, |x| x.max(l)));
        }
    }
}

/// All-time totals: the daily roll-up (`miner_alltime`) plus the short per-block
/// ledger that has not been rolled up yet (`miner_blocks`).
fn alltime_map(
    conn: &Connection,
    addresses: &[String],
) -> Result<HashMap<String, AllTime>, ApiError> {
    let mut uniq: Vec<String> = Vec::with_capacity(addresses.len());
    for a in addresses {
        if !uniq.contains(a) {
            uniq.push(a.clone());
        }
    }
    let mut out: HashMap<String, AllTime> = HashMap::new();
    for chunk in uniq.chunks(CHUNK) {
        let m = marks(chunk.len());
        let params = text_params(chunk);
        // Older trackers have no roll-up table: ledger-only totals, as in legacy.
        if let Ok(rows) = query(
            conn,
            &format!(
                "SELECT address, total_sompi, blocks, first_seen_ms, last_seen_ms \
                 FROM miner_alltime WHERE address IN ({m})"
            ),
            &params,
        ) {
            for r in rows {
                if let Some(a) = get(&r, "address").text() {
                    out.entry(a.to_string()).or_default().add(
                        get(&r, "total_sompi"),
                        get(&r, "blocks"),
                        get(&r, "first_seen_ms"),
                        get(&r, "last_seen_ms"),
                    );
                }
            }
        }
        for r in query(
            conn,
            &format!(
                "SELECT address, SUM(amount_sompi) AS s, COUNT(*) AS n, \
                        MIN(timestamp_ms) AS f, MAX(timestamp_ms) AS l \
                 FROM miner_blocks WHERE address IN ({m}) GROUP BY address"
            ),
            &params,
        )? {
            if let Some(a) = get(&r, "address").text() {
                out.entry(a.to_string()).or_default().add(
                    get(&r, "s"),
                    get(&r, "n"),
                    get(&r, "f"),
                    get(&r, "l"),
                );
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
struct PoolShare {
    name: String,
    is_pool: bool,
    addresses: i64,
    blocks: i64,
    share: PyFloat,
}

/// Block findings in the current window grouped by operator: addresses whose
/// software label ends in "(pool)" merge under that pool; everything else stays
/// one group per address.
fn pool_shares(
    conn: &Connection,
    ts_ms: i64,
    window_minutes: i64,
    limit: usize,
) -> Result<Vec<PoolShare>, ApiError> {
    let cutoff = ts_ms - window_minutes.max(1) * 60 * 1000;
    let rows = query(
        conn,
        "SELECT address, COUNT(*) AS n FROM miner_blocks \
         WHERE timestamp_ms >= ? GROUP BY address",
        &[Value::Integer(cutoff)],
    )?;
    let total: i64 = rows.iter().filter_map(|r| get(r, "n").as_i64()).sum();
    if total == 0 {
        return Ok(Vec::new());
    }
    let addrs: Vec<String> = rows
        .iter()
        .filter_map(|r| get(r, "address").text().map(str::to_string))
        .collect();
    let sw = software_map(conn, &addrs);

    // Insertion order matters: ties keep the order groups were first seen.
    let mut groups: Vec<PoolShare> = Vec::new();
    for r in &rows {
        let addr = get(r, "address").text().unwrap_or_default();
        let label = sw
            .get(addr)
            .and_then(|s| s.software.text())
            .unwrap_or_default();
        let is_pool = label.ends_with("(pool)");
        let key = if is_pool { label } else { addr };
        let n = get(r, "n").as_i64().unwrap_or(0);
        match groups.iter_mut().find(|g| g.name == key) {
            Some(g) => {
                g.addresses += 1;
                g.blocks += n;
            }
            None => groups.push(PoolShare {
                name: key.to_string(),
                is_pool,
                addresses: 1,
                blocks: n,
                share: PyFloat(0.0),
            }),
        }
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g.blocks)); // stable, like Python's sorted
    for g in &mut groups {
        g.share = PyFloat(g.blocks as f64 / total as f64);
    }
    groups.truncate(limit);
    Ok(groups)
}

// ---- GET /info/miners/top ----

#[derive(Debug, Deserialize)]
pub struct TopQuery {
    limit: Option<i64>,
    recent_hours: Option<i64>,
}

#[derive(Debug, Serialize)]
struct Network {
    hashrate_hs: Cell,
    window_blocks: Cell,
    active_miners: Cell,
    window_minutes: Cell,
    min_blocks_gate: Cell,
}

impl Network {
    fn from_row(r: &Row, hps_column: &str) -> Self {
        Self {
            hashrate_hs: get(r, hps_column).clone(),
            window_blocks: get(r, "window_blocks").clone(),
            active_miners: get(r, "active_miners").clone(),
            window_minutes: get(r, "window_minutes").clone(),
            min_blocks_gate: get(r, "min_blocks_gate").clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Miner {
    address: Cell,
    blocks: Cell,
    window_kls: PyFloat,
    hashrate_hs: Cell,
    winrate: Cell,
    /// KLS this address earned in the last 24 hours (rolling, from the ledger).
    day_kls: PyFloat,
    #[serde(flatten)]
    software: Option<Software>,
}

#[derive(Debug, Serialize)]
struct RecentMiner {
    address: String,
    blocks_recent: Cell,
    recent_kls: PyFloat,
    last_block_ms: Cell,
    /// KLS this address earned in the last 24 hours (rolling, from the ledger).
    day_kls: PyFloat,
    #[serde(flatten)]
    software: Option<Software>,
}

#[derive(Debug, Serialize)]
struct Top {
    ts_ms: i64,
    stale: bool,
    recent_hours: i64,
    recent_miners: Vec<RecentMiner>,
    pools: Vec<PoolShare>,
    network: Option<Network>,
    miners: Vec<Miner>,
}

/// Rolling window behind `day_kls`: the leaderboard shows the daily reward
/// distribution, so only the last 24 hours of the ledger are ever needed.
const DAY_MS: i64 = 24 * 3600 * 1000;

/// Per-address KLS earned in the last 24 hours, summed from `miner_blocks`.
fn day_map(conn: &Connection) -> Result<HashMap<String, i64>, ApiError> {
    let cutoff = now_ms() - DAY_MS;
    let mut out = HashMap::new();
    for r in query(
        conn,
        "SELECT address, COALESCE(SUM(amount_sompi), 0) AS s \
         FROM miner_blocks WHERE timestamp_ms >= ? GROUP BY address",
        &[Value::Integer(cutoff)],
    )? {
        if let Some(a) = get(&r, "address").text() {
            out.insert(a.to_string(), get(&r, "s").as_i64().unwrap_or(0));
        }
    }
    Ok(out)
}

fn day_kls(day: &HashMap<String, i64>, addr: &str) -> PyFloat {
    PyFloat(day.get(addr).copied().unwrap_or(0) as f64 / SOMPI_PER_KLS)
}

fn top(conn: &Connection, limit: i64, recent_hours: i64) -> Result<String, ApiError> {
    let Some(ts_ms) = latest_ts(conn)? else {
        return Ok(r#"{"ts_ms":null,"stale":true,"network":null,"miners":[]}"#.into());
    };

    let net = query(
        conn,
        "SELECT network_hps, window_blocks, active_miners, window_minutes, min_blocks_gate \
         FROM network_snapshot WHERE ts_ms = ?",
        &[Value::Integer(ts_ms)],
    )?
    .into_iter()
    .next();

    let miners = query(
        conn,
        "SELECT address, blocks_in_window, window_sompi, hashrate_hs, winrate \
         FROM miner_snapshot WHERE ts_ms = ? ORDER BY hashrate_hs DESC LIMIT ?",
        &[Value::Integer(ts_ms), Value::Integer(limit)],
    )?;

    let mut recent_rows = Vec::new();
    if recent_hours > 0 {
        let gated: Vec<&str> = miners
            .iter()
            .filter_map(|m| get(m, "address").text())
            .collect();
        let cutoff = now_ms() - recent_hours * 3600 * 1000;
        for r in query(
            conn,
            "SELECT address, COUNT(*) AS n, COALESCE(SUM(amount_sompi), 0) AS sompi, \
                    MAX(timestamp_ms) AS last_ms \
             FROM miner_blocks WHERE timestamp_ms >= ? \
             GROUP BY address ORDER BY n DESC, last_ms DESC",
            &[Value::Integer(cutoff)],
        )? {
            let addr = get(&r, "address").text().unwrap_or_default();
            if !gated.contains(&addr) {
                recent_rows.push(r);
            }
        }
    }

    let window_minutes = net
        .as_ref()
        .and_then(|n| get(n, "window_minutes").as_i64())
        .unwrap_or(60);
    let pools = pool_shares(conn, ts_ms, window_minutes, 10)?;

    let all_addrs: Vec<String> = miners
        .iter()
        .chain(&recent_rows)
        .filter_map(|r| get(r, "address").text().map(str::to_string))
        .collect();
    let sw = software_map(conn, &all_addrs);
    let day = day_map(conn)?;

    let recent_miners = recent_rows
        .iter()
        .map(|r| {
            let addr = get(r, "address").text().unwrap_or_default().to_string();
            RecentMiner {
                blocks_recent: get(r, "n").clone(),
                recent_kls: PyFloat(get(r, "sompi").kls()),
                last_block_ms: get(r, "last_ms").clone(),
                day_kls: day_kls(&day, &addr),
                software: sw.get(&addr).cloned(),
                address: addr,
            }
        })
        .collect();

    let miners = miners
        .iter()
        .map(|m| {
            let addr = get(m, "address").text().unwrap_or_default();
            Miner {
                address: get(m, "address").clone(),
                blocks: get(m, "blocks_in_window").clone(),
                window_kls: PyFloat(get(m, "window_sompi").kls()),
                hashrate_hs: get(m, "hashrate_hs").clone(),
                winrate: get(m, "winrate").clone(),
                day_kls: day_kls(&day, addr),
                software: sw.get(addr).cloned(),
            }
        })
        .collect();

    let body = Top {
        ts_ms,
        stale: is_stale(Some(ts_ms)),
        recent_hours,
        recent_miners,
        pools,
        network: net.as_ref().map(|n| Network::from_row(n, "network_hps")),
        miners,
    };
    to_json(&body)
}

pub async fn miners_top(Query(q): Query<TopQuery>) -> Result<Response, ApiError> {
    let limit = q.limit.unwrap_or(300);
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::Validation(
            "limit must be between 1 and 1000".into(),
        ));
    }
    let recent_hours = q.recent_hours.unwrap_or(24);
    if !(0..=168).contains(&recent_hours) {
        return Err(ApiError::Validation(
            "recent_hours must be between 0 and 168".into(),
        ));
    }
    Ok(json_response(
        with_db(move |c| top(c, limit, recent_hours)).await?,
    ))
}

// ---- GET /info/miners/network ----

#[derive(Debug, Serialize)]
struct NetworkOnly {
    ts_ms: Cell,
    stale: bool,
    #[serde(flatten)]
    network: Network,
}

pub async fn miners_network() -> Result<Response, ApiError> {
    Ok(json_response(
        with_db(|conn| {
            let row = query(
                conn,
                "SELECT ts_ms, network_hps, window_blocks, active_miners, \
                        window_minutes, min_blocks_gate \
                 FROM network_snapshot ORDER BY ts_ms DESC LIMIT 1",
                &[],
            )?
            .into_iter()
            .next();
            let Some(r) = row else {
                return Ok(r#"{"ts_ms":null,"stale":true}"#.into());
            };
            to_json(&NetworkOnly {
                ts_ms: get(&r, "ts_ms").clone(),
                stale: is_stale(get(&r, "ts_ms").as_i64()),
                network: Network::from_row(&r, "network_hps"),
            })
        })
        .await?,
    ))
}

// ---- GET /info/miners/address/{address} ----

#[derive(Debug, Deserialize)]
pub struct DetailQuery {
    hours: Option<i64>,
}

#[derive(Debug, Serialize)]
struct DetailAllTime {
    blocks: i64,
    total_kls: PyFloat,
    first_seen_ms: Option<i64>,
    last_seen_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
struct HistoryPoint {
    ts_ms: Cell,
    blocks: Cell,
    hashrate_hs: Cell,
    winrate: Cell,
}

#[derive(Debug, Serialize)]
struct Detail {
    address: String,
    alltime: DetailAllTime,
    history: Vec<HistoryPoint>,
}

pub async fn miner_detail(
    UrlPath(address): UrlPath<String>,
    Query(q): Query<DetailQuery>,
) -> Result<Response, ApiError> {
    if !address.starts_with("karlsen:") {
        return Err(ApiError::BadRequest("address must start with 'karlsen:'"));
    }
    let hours = q.hours.unwrap_or(24);
    if !(1..=168).contains(&hours) {
        return Err(ApiError::Validation(
            "hours must be between 1 and 168".into(),
        ));
    }
    Ok(json_response(
        with_db(move |conn| {
            let tot = alltime_map(conn, std::slice::from_ref(&address))?
                .remove(&address)
                .unwrap_or_default();
            let cutoff = now_ms() - hours * 3600 * 1000;
            let history = query(
                conn,
                "SELECT ts_ms, blocks_in_window, hashrate_hs, winrate \
                 FROM miner_snapshot WHERE address = ? AND ts_ms >= ? ORDER BY ts_ms ASC",
                &[Value::Text(address.clone()), Value::Integer(cutoff)],
            )?
            .iter()
            .map(|h| HistoryPoint {
                ts_ms: get(h, "ts_ms").clone(),
                blocks: get(h, "blocks_in_window").clone(),
                hashrate_hs: get(h, "hashrate_hs").clone(),
                winrate: get(h, "winrate").clone(),
            })
            .collect();
            to_json(&Detail {
                address,
                alltime: DetailAllTime {
                    blocks: tot.blocks,
                    total_kls: PyFloat(tot.sompi as f64 / SOMPI_PER_KLS),
                    first_seen_ms: tot.first_ms,
                    last_seen_ms: tot.last_ms,
                },
                history,
            })
        })
        .await?,
    ))
}
