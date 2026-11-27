//! KLS/USDT ticker from KarlsenX (x.karlsencoin.org), shared by `/info/price` and `/info/market-data`.
//!
//! The ticker is cached for 60 s (legacy cached the price for 60 s and fetched
//! market data on every request). Only one request goes upstream at a time; callers
//! that arrive meanwhile wait for it and share the result.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::sync::Mutex;

pub const DEFAULT_TICKER_URL: &str = "https://x.karlsencoin.org/api/v1/ticker";
const TTL: Duration = Duration::from_secs(60);

/// The ticker fields the API uses, already parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct Ticker {
    pub last_price: f64,
    pub high: f64,
    pub low: f64,
    pub bid: f64,
    pub ask: f64,
    pub base_volume: f64,
    pub usd_volume: f64,
    pub change_percent: f64,
    pub previous_day_price: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum MarketError {
    #[error("ticker request failed: {0}")]
    Http(String),
    #[error("ticker field {0} missing or not a number")]
    Field(&'static str),
}

/// Accept numbers sent as strings or as JSON numbers.
fn opt_num(v: &Value, key: &str) -> Option<f64> {
    match &v[key] {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn num(v: &Value, key: &'static str) -> Result<f64, MarketError> {
    opt_num(v, key).ok_or(MarketError::Field(key))
}

/// Parses the KarlsenX native ticker (`/api/v1/ticker`). Fields without trades in
/// the last 24 h (`high`, `low`, `open_24h`) are `null` and fall back to the last
/// price; an empty book side falls back to the last price as well. The change and
/// USD volume are derived (USDT is taken as USD; change rounded to 2 decimals).
pub fn parse_ticker(v: &Value) -> Result<Ticker, MarketError> {
    let last = num(v, "last_price")?;
    let or_last = |k: &str| opt_num(v, k).unwrap_or(last);
    let open = or_last("open_24h");
    Ok(Ticker {
        last_price: last,
        high: or_last("high").max(last),
        low: or_last("low").min(last),
        bid: or_last("bid"),
        ask: or_last("ask"),
        base_volume: num(v, "base_volume")?,
        usd_volume: num(v, "target_volume")?,
        change_percent: if open > 0.0 {
            ((last - open) / open * 10_000.0).round() / 100.0
        } else {
            0.0
        },
        previous_day_price: open,
    })
}

#[derive(Default)]
struct Cached {
    ticker: Option<(Ticker, Instant)>,
}

#[derive(Clone)]
pub struct MarketFeed {
    url: Arc<str>,
    http: reqwest::Client,
    cache: Arc<Mutex<Cached>>,
}

impl MarketFeed {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent("karlsen-explorer-api")
                .build()
                .expect("static client configuration"),
            cache: Arc::default(),
        }
    }

    /// `KLS_TICKER_URL` or the public KarlsenX endpoint.
    pub fn from_env() -> Self {
        Self::new(&std::env::var("KLS_TICKER_URL").unwrap_or_else(|_| DEFAULT_TICKER_URL.into()))
    }

    async fn fetch(&self) -> Result<Ticker, MarketError> {
        let resp = self
            .http
            .get(&*self.url)
            .send()
            .await
            .map_err(|e| MarketError::Http(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(MarketError::Http(format!("HTTP {}", resp.status())));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| MarketError::Http(e.to_string()))?;
        parse_ticker(&body)
    }

    /// Fresh (≤ 60 s) ticker. If the upstream fails, the last good ticker is
    /// returned with `stale = true`; with no good ticker yet, the error.
    pub async fn ticker(&self) -> Result<(Ticker, bool), MarketError> {
        let mut c = self.cache.lock().await;
        if let Some((t, at)) = &c.ticker {
            if at.elapsed() < TTL {
                return Ok((t.clone(), false));
            }
        }
        match self.fetch().await {
            Ok(t) => {
                c.ticker = Some((t.clone(), Instant::now()));
                Ok((t, false))
            }
            Err(e) => {
                tracing::warn!(error = %e, "KarlsenX ticker unavailable");
                match &c.ticker {
                    Some((t, _)) => Ok((t.clone(), true)),
                    None => Err(e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_karlsenx_ticker() {
        let v = serde_json::json!({
            "ticker_id": "KLS_USDT", "last_price": "0.00003110", "open_24h": "0.00003500",
            "high": "0.00003504", "low": "0.00003100", "bid": "0.00003105", "ask": "0.00003112",
            "base_volume": "206000.5", "target_volume": "6.71"
        });
        let t = parse_ticker(&v).unwrap();
        assert_eq!(t.last_price, 3.11e-05);
        assert_eq!(t.previous_day_price, 3.5e-05);
        assert_eq!(t.change_percent, -11.14);
        assert_eq!(t.usd_volume, 6.71);
    }

    #[test]
    fn quiet_day_falls_back_to_last() {
        let v = serde_json::json!({
            "last_price": 0.00003, "open_24h": null, "high": null, "low": null,
            "bid": null, "ask": "0.000031", "base_volume": "0", "target_volume": "0"
        });
        let t = parse_ticker(&v).unwrap();
        assert_eq!((t.high, t.low, t.bid, t.previous_day_price), (3e-05, 3e-05, 3e-05, 3e-05));
        assert_eq!(t.change_percent, 0.0);
    }

    #[test]
    fn missing_last_price_is_an_error() {
        let v = serde_json::json!({"last_price": null});
        assert!(matches!(parse_ticker(&v), Err(MarketError::Field("last_price"))));
    }
}
