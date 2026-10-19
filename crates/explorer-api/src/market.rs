//! KLS/USDT ticker from NonKYC, shared by `/info/price` and `/info/market-data`.
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

pub const DEFAULT_TICKER_URL: &str = "https://api.nonkyc.io/api/v2/ticker/KLS/USDT";
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

/// NonKYC sends numbers as strings; accept both.
fn num(v: &Value, key: &'static str) -> Result<f64, MarketError> {
    match &v[key] {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .ok_or(MarketError::Field(key))
}

pub fn parse_ticker(v: &Value) -> Result<Ticker, MarketError> {
    Ok(Ticker {
        last_price: num(v, "last_price")?,
        high: num(v, "high")?,
        low: num(v, "low")?,
        bid: num(v, "bid")?,
        ask: num(v, "ask")?,
        base_volume: num(v, "base_volume")?,
        usd_volume: num(v, "usd_volume_est")?,
        change_percent: num(v, "change_percent")?,
        previous_day_price: num(v, "previous_day_price")?,
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

    /// `NONKYC_TICKER_URL` or the public NonKYC endpoint.
    pub fn from_env() -> Self {
        Self::new(&std::env::var("NONKYC_TICKER_URL").unwrap_or_else(|_| DEFAULT_TICKER_URL.into()))
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
                tracing::warn!(error = %e, "NonKYC ticker unavailable");
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
    fn parses_strings_and_numbers() {
        let v = serde_json::json!({
            "last_price": "0.000083549", "high": 0.00015, "low": "0.000069", "bid": "0.000083244",
            "ask": "0.00008355", "base_volume": "13718051.1385", "usd_volume_est": "1390.75",
            "change_percent": "-30.35", "previous_day_price": "0.000119966"
        });
        let t = parse_ticker(&v).unwrap();
        assert_eq!(t.last_price, 8.3549e-05);
        assert_eq!(t.high, 0.00015);
        assert_eq!(t.change_percent, -30.35);
    }

    #[test]
    fn missing_field_is_an_error() {
        let v = serde_json::json!({"last_price": "1"});
        assert!(matches!(parse_ticker(&v), Err(MarketError::Field("high"))));
    }
}
