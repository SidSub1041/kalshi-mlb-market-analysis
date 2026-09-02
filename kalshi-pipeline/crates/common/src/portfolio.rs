//! Read-only, authenticated views of a Kalshi account: balance, positions,
//! resting orders, and fill-event parsing for the `fills` websocket channel.
//!
//! DELIBERATE SCOPE LIMIT: this module contains no order-transmission code —
//! no POST, no DELETE. It exists so an executor (implemented separately) can
//! reconcile its in-memory state against the exchange's truth, and so
//! monitoring can observe an account without being able to change it.

use crate::auth::Signer;
use anyhow::{Context, Result};
use serde_json::Value;

pub struct PortfolioClient {
    http: reqwest::Client,
    signer: Signer,
    /// e.g. "https://demo-api.kalshi.co/trade-api/v2"
    api_base: String,
}

impl PortfolioClient {
    pub fn new(signer: Signer, api_base: impl Into<String>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            signer,
            api_base: api_base.into(),
        })
    }

    /// Signed GET. The signature covers the URL path only (no query), per
    /// Kalshi's scheme — the same one the websocket handshake already uses.
    async fn get(&self, rel: &str) -> Result<Value> {
        let full = format!("{}{}", self.api_base, rel);
        let url = reqwest::Url::parse(&full).context("portfolio url")?;
        let path = url.path().to_string();
        let (ts, sig) = self.signer.headers("GET", &path)?;
        let resp = self.http.get(url)
            .header("KALSHI-ACCESS-KEY", &self.signer.key_id)
            .header("KALSHI-ACCESS-SIGNATURE", sig)
            .header("KALSHI-ACCESS-TIMESTAMP", ts)
            .send().await?;
        let status = resp.status();
        let text = resp.text().await.context("portfolio body")?;
        if !status.is_success() {
            anyhow::bail!("portfolio GET {rel} -> {status}: {}",
                          text.chars().take(300).collect::<String>());
        }
        serde_json::from_str(&text).context("portfolio json")
    }

    /// Account balance in cents.
    pub async fn balance_cents(&self) -> Result<i64> {
        let v = self.get("/portfolio/balance").await?;
        v["balance"].as_i64().context("balance field")
    }

    /// Open market positions: (ticker, signed contracts, raw record).
    /// Positive contracts = long YES. Follows cursor pagination so a large
    /// account is never silently truncated.
    pub async fn positions(&self) -> Result<Vec<(String, i64, Value)>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let rel = if cursor.is_empty() {
                "/portfolio/positions?limit=200".to_string()
            } else {
                format!("/portfolio/positions?limit=200&cursor={cursor}")
            };
            let v = self.get(&rel).await?;
            for p in v["market_positions"].as_array().unwrap_or(&vec![]) {
                let ticker = p["ticker"].as_str().unwrap_or("").to_string();
                let pos = p["position"].as_i64().unwrap_or(0);
                if !ticker.is_empty() && pos != 0 {
                    out.push((ticker, pos, p.clone()));
                }
            }
            match v["cursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = c.to_string(),
                _ => break,
            }
        }
        Ok(out)
    }

    /// Resting (open) orders: (order_id, ticker, raw record). Follows cursor
    /// pagination; an executor cancel-all should still loop
    /// "cancel, refetch, repeat until empty" to verify cancels landed.
    pub async fn resting_orders(&self) -> Result<Vec<(String, String, Value)>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let rel = if cursor.is_empty() {
                "/portfolio/orders?status=resting&limit=200".to_string()
            } else {
                format!("/portfolio/orders?status=resting&limit=200&cursor={cursor}")
            };
            let v = self.get(&rel).await?;
            for o in v["orders"].as_array().unwrap_or(&vec![]) {
                let id = o["order_id"].as_str().unwrap_or("").to_string();
                let ticker = o["ticker"].as_str().unwrap_or("").to_string();
                if !id.is_empty() {
                    out.push((id, ticker, o.clone()));
                }
            }
            match v["cursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = c.to_string(),
                _ => break,
            }
        }
        Ok(out)
    }
}

/// One execution of YOUR order, from the authenticated `fills` ws channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub trade_id: String,
    pub order_id: String,
    pub ticker: String,
    /// "yes" | "no"
    pub side: String,
    /// "buy" | "sell"
    pub action: String,
    pub count: f64,
    /// Yes-side price in cents.
    pub yes_price_cents: i64,
    pub is_taker: bool,
}

/// Parse a `fill` message body (the `msg` object). Tolerates both integer-cent
/// and dollars_fp string encodings, mirroring the market-data channels.
pub fn parse_fill(m: &Value) -> Option<Fill> {
    let cents = |v: &Value| -> Option<i64> {
        v.as_str().and_then(|s| s.parse::<f64>().ok())
            .map(|d| (d * 100.0).round() as i64)
            .or_else(|| v.as_i64())
    };
    let qty = |v: &Value| -> Option<f64> {
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    };
    Some(Fill {
        trade_id: m["trade_id"].as_str()?.to_string(),
        order_id: m["order_id"].as_str().unwrap_or("").to_string(),
        ticker: m["market_ticker"].as_str().or(m["ticker"].as_str())?.to_string(),
        side: m["side"].as_str().unwrap_or("yes").to_string(),
        action: m["action"].as_str().unwrap_or("").to_string(),
        count: qty(&m["count_fp"]).or_else(|| qty(&m["count"]))?,
        yes_price_cents: cents(&m["yes_price_dollars"])
            .or_else(|| cents(&m["yes_price"]))?,
        is_taker: m["is_taker"].as_bool().unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_fill_cents_format() {
        let m = json!({
            "trade_id": "t1", "order_id": "o1", "market_ticker": "KXMLBGAME-X-NYY",
            "side": "yes", "action": "buy", "count": 3, "yes_price": 42, "is_taker": false
        });
        let f = parse_fill(&m).unwrap();
        assert_eq!(f.yes_price_cents, 42);
        assert_eq!(f.count, 3.0);
        assert!(!f.is_taker);
    }

    #[test]
    fn parses_fill_dollars_fp_format() {
        let m = json!({
            "trade_id": "t2", "order_id": "o2", "market_ticker": "KXMLBGAME-X-SEA",
            "side": "yes", "action": "sell", "count_fp": "10.00",
            "yes_price_dollars": "0.5700", "is_taker": true
        });
        let f = parse_fill(&m).unwrap();
        assert_eq!(f.yes_price_cents, 57);
        assert_eq!(f.count, 10.0);
        assert!(f.is_taker);
    }

    #[test]
    fn rejects_malformed_fill() {
        assert!(parse_fill(&json!({"order_id": "o3"})).is_none());
    }
}
