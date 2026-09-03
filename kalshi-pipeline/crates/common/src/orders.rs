//! Signed Kalshi order endpoints.
//!
//! This client deliberately has no strategy logic and never invents or retries
//! a client order ID. The caller creates one ID for an intent and reuses it for
//! every retry, so a timeout cannot turn into a duplicate order.

use crate::auth::Signer;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

const EVENT_ORDERS: &str = "/portfolio/events/orders";

/// Proof that the process was deliberately enabled to submit orders. The
/// token's fields are private: callers must pass the two-key production gate
/// before an `OrderClient` can be constructed.
pub struct TradingPermission(());

impl TradingPermission {
    pub fn authorize(api_base: &str, live_enabled: bool, confirm_prod: bool) -> Result<Self> {
        anyhow::ensure!(
            live_enabled,
            "order submission disabled: set live_enabled = true"
        );
        let host = reqwest::Url::parse(api_base)
            .context("api_base URL")?
            .host_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let production = matches!(
            host.as_str(),
            "external-api.kalshi.com" | "api.elections.kalshi.com"
        );
        anyhow::ensure!(
            !production || confirm_prod,
            "production order submission requires confirm_prod = true"
        );
        Ok(Self(()))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LimitOrder<'a> {
    pub ticker: &'a str,
    pub client_order_id: &'a str,
    /// Kalshi V2 book direction: `bid` buys the selected outcome; `ask` sells it.
    pub side: &'a str,
    /// Fixed-point contracts, for example `"1.00"`.
    pub count: &'a str,
    /// Fixed-point dollars, for example `"0.42"`.
    pub price: &'a str,
    /// Use `good_till_canceled` for a maker order or `fill_or_kill` for a cross.
    pub time_in_force: &'a str,
    /// Kalshi's V2 self-trade policy. The executor always uses
    /// `taker_at_cross` so it cannot take its own resting liquidity.
    pub self_trade_prevention_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_order_on_pause: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reduce_only: Option<bool>,
}

impl<'a> LimitOrder<'a> {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.ticker.is_empty(), "order ticker is empty");
        anyhow::ensure!(!self.client_order_id.is_empty(), "client_order_id is empty");
        anyhow::ensure!(
            matches!(self.side, "bid" | "ask"),
            "side must be bid or ask"
        );
        anyhow::ensure!(
            self.count.parse::<f64>().is_ok_and(|v| v > 0.0),
            "count must be positive"
        );
        anyhow::ensure!(
            self.price.parse::<f64>().is_ok_and(|v| v > 0.0 && v < 1.0),
            "price must be between 0 and 1"
        );
        anyhow::ensure!(
            matches!(self.time_in_force, "good_till_canceled" | "fill_or_kill"),
            "unsupported time_in_force"
        );
        anyhow::ensure!(
            matches!(self.self_trade_prevention_type, "taker_at_cross" | "maker"),
            "unsupported self_trade_prevention_type"
        );
        Ok(())
    }
}

pub struct OrderClient {
    http: reqwest::Client,
    signer: Signer,
    api_base: String,
}

impl OrderClient {
    pub fn new(
        signer: Signer,
        api_base: impl Into<String>,
        _permission: TradingPermission,
    ) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            signer,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
        })
    }

    fn signed(&self, method: &str, rel: &str) -> Result<reqwest::RequestBuilder> {
        let url = reqwest::Url::parse(&format!("{}{}", self.api_base, rel)).context("order url")?;
        let (ts, sig) = self.signer.headers(method, url.path())?;
        Ok(self
            .http
            .request(method.parse()?, url)
            .header("KALSHI-ACCESS-KEY", &self.signer.key_id)
            .header("KALSHI-ACCESS-SIGNATURE", sig)
            .header("KALSHI-ACCESS-TIMESTAMP", ts))
    }

    async fn json(&self, request: reqwest::RequestBuilder, action: &str) -> Result<Value> {
        let response = request.send().await.context(action.to_owned())?;
        let status = response.status();
        let body = response.text().await.context("order response body")?;
        if !status.is_success() {
            anyhow::bail!(
                "{action} -> {status}: {}",
                body.chars().take(500).collect::<String>()
            );
        }
        serde_json::from_str(&body).context("order response json")
    }

    /// Create exactly one V2 event-market order. A network failure is
    /// intentionally returned to the caller; retry only with the identical
    /// `client_order_id` after reconciling REST and fill events.
    pub async fn create_order(&self, order: &LimitOrder<'_>) -> Result<Value> {
        order.validate()?;
        let request = self.signed("POST", EVENT_ORDERS)?.json(order);
        self.json(request, "create order").await
    }

    /// Cancel one V2 event-market order.
    pub async fn cancel_order(&self, order_id: &str) -> Result<Value> {
        valid_order_id(order_id)?;
        let rel = format!("{EVENT_ORDERS}/{order_id}");
        self.json(self.signed("DELETE", &rel)?, "cancel order")
            .await
    }

    /// Cancel every resting event-market order for this API key. This is used
    /// only during controlled reconciliation and emergency shutdown; callers
    /// must refetch `resting_orders` afterwards to verify the exchange state.
    pub async fn cancel_all_orders(&self) -> Result<Value> {
        self.json(self.signed("DELETE", EVENT_ORDERS)?, "cancel all orders")
            .await
    }

    /// Query one order. Kalshi currently serves reads at the portfolio path.
    pub async fn get_order(&self, order_id: &str) -> Result<Value> {
        valid_order_id(order_id)?;
        let rel = format!("/portfolio/orders/{order_id}");
        self.json(self.signed("GET", &rel)?, "get order").await
    }
}

fn valid_order_id(order_id: &str) -> Result<()> {
    anyhow::ensure!(!order_id.is_empty(), "order_id is empty");
    // Kalshi order IDs are UUID-like. Restricting to unreserved path bytes
    // prevents an ID from changing the signed path through URL delimiters.
    anyhow::ensure!(
        order_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "order_id contains unsafe path characters"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_v2_maker_order() {
        assert!(LimitOrder {
            ticker: "KXTEST",
            client_order_id: "intent-1",
            side: "bid",
            count: "1.00",
            price: "0.42",
            time_in_force: "good_till_canceled",
            self_trade_prevention_type: "taker_at_cross",
            post_only: Some(true),
            cancel_order_on_pause: Some(true),
            reduce_only: None
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn rejects_missing_id_and_invalid_price() {
        let mut order = LimitOrder {
            ticker: "KXTEST",
            client_order_id: "",
            side: "bid",
            count: "1.00",
            price: "0.42",
            time_in_force: "good_till_canceled",
            self_trade_prevention_type: "taker_at_cross",
            post_only: None,
            cancel_order_on_pause: None,
            reduce_only: None,
        };
        assert!(order.validate().is_err());
        order.client_order_id = "intent-1";
        order.price = "1.00";
        assert!(order.validate().is_err());
    }

    #[test]
    fn requires_two_keys_for_production() {
        assert!(TradingPermission::authorize(
            "https://external-api.kalshi.com/trade-api/v2",
            false,
            false
        )
        .is_err());
        assert!(TradingPermission::authorize(
            "https://external-api.kalshi.com/trade-api/v2",
            true,
            false
        )
        .is_err());
        assert!(TradingPermission::authorize(
            "https://external-api.kalshi.com/trade-api/v2",
            true,
            true
        )
        .is_ok());
        assert!(TradingPermission::authorize(
            "https://external-api.demo.kalshi.co/trade-api/v2",
            true,
            false
        )
        .is_ok());
    }
}
