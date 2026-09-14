//! Signed Kalshi order endpoints.
//!
//! This client deliberately has no strategy logic and never invents or retries
//! a client order ID. The caller creates one ID for an intent and reuses it for
//! every retry, so a timeout cannot turn into a duplicate order.

use crate::auth::Signer;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

const EVENT_ORDERS: &str = "/portfolio/events/orders";
const ORDER_LOOKUP_ATTEMPTS: usize = 6;
const ORDER_LOOKUP_RETRY_DELAY: Duration = Duration::from_millis(500);

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
        self.signed_url(method, url)
    }

    /// Sign a request after its query parameters have been added. Kalshi signs
    /// the path, not the query string, so this preserves the standard signing
    /// behavior while allowing shard auto-routing parameters.
    fn signed_url(&self, method: &str, url: reqwest::Url) -> Result<reqwest::RequestBuilder> {
        let (ts, sig) = self.signer.headers(method, url.path())?;
        Ok(self
            .http
            .request(method.parse()?, url)
            .header("KALSHI-ACCESS-KEY", &self.signer.key_id)
            .header("KALSHI-ACCESS-SIGNATURE", sig)
            .header("KALSHI-ACCESS-TIMESTAMP", ts))
    }

    /// Route an order-specific request to its matching-engine shard using the
    /// market ticker. An order ID by itself cannot identify that shard.
    fn signed_for_market_ticker(
        &self,
        method: &str,
        rel: &str,
        market_ticker: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let mut url = reqwest::Url::parse(&format!("{}{}", self.api_base, rel))
            .context("routed order url")?;
        add_market_ticker(&mut url, market_ticker)?;
        self.signed_url(method, url)
    }

    async fn response_body(
        &self,
        request: reqwest::RequestBuilder,
        action: &str,
    ) -> Result<(reqwest::StatusCode, String)> {
        let response = request.send().await.context(action.to_owned())?;
        let status = response.status();
        let body = response.text().await.context("order response body")?;
        Ok((status, body))
    }

    fn ensure_success(status: reqwest::StatusCode, body: &str, action: &str) -> Result<()> {
        if !status.is_success() {
            anyhow::bail!(
                "{action} -> {status}: {}",
                body.chars().take(500).collect::<String>()
            );
        }
        Ok(())
    }

    async fn json(&self, request: reqwest::RequestBuilder, action: &str) -> Result<Value> {
        let (status, body) = self.response_body(request, action).await?;
        Self::ensure_success(status, &body, action)?;
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

    /// Cancel one V2 event-market order. Supplying its market ticker makes
    /// Kalshi auto-route this request to the market's exchange shard.
    pub async fn cancel_order(&self, order_id: &str, market_ticker: &str) -> Result<Value> {
        valid_order_id(order_id)?;
        let rel = format!("{EVENT_ORDERS}/{order_id}");
        self.json(
            self.signed_for_market_ticker("DELETE", &rel, market_ticker)?,
            "cancel order",
        )
        .await
    }

    /// Cancel every resting event-market order for this API key. This is used
    /// only during controlled reconciliation and emergency shutdown; callers
    /// must refetch `resting_orders` afterwards to verify the exchange state.
    pub async fn cancel_all_orders(&self) -> Result<()> {
        let (status, body) = self
            .response_body(self.signed("DELETE", EVENT_ORDERS)?, "cancel all orders")
            .await?;
        Self::ensure_success(status, &body, "cancel all orders")
    }

    /// Query one order. Its market ticker enables shard auto-routing. A newly
    /// created order can take a moment to become visible on the read path, so
    /// retry a short, bounded number of shard-aware 404 responses.
    pub async fn get_order(&self, order_id: &str, market_ticker: &str) -> Result<Value> {
        valid_order_id(order_id)?;
        let rel = format!("/portfolio/orders/{order_id}");
        for attempt in 0..ORDER_LOOKUP_ATTEMPTS {
            let response = self
                .signed_for_market_ticker("GET", &rel, market_ticker)?
                .send()
                .await
                .context("get order")?;
            let status = response.status();
            let body = response.text().await.context("order response body")?;
            if status == reqwest::StatusCode::NOT_FOUND && attempt + 1 < ORDER_LOOKUP_ATTEMPTS {
                tokio::time::sleep(ORDER_LOOKUP_RETRY_DELAY).await;
                continue;
            }
            if !status.is_success() {
                anyhow::bail!(
                    "get order -> {status}: {}",
                    body.chars().take(500).collect::<String>()
                );
            }
            return serde_json::from_str(&body).context("order response json");
        }
        unreachable!("order lookup loop always returns or errors")
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

fn add_market_ticker(url: &mut reqwest::Url, market_ticker: &str) -> Result<()> {
    anyhow::ensure!(!market_ticker.is_empty(), "market_ticker is empty");
    url.query_pairs_mut()
        .append_pair("market_ticker", market_ticker);
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
    fn encodes_market_ticker_for_shard_auto_routing() {
        let mut url = reqwest::Url::parse(
            "https://external-api.demo.kalshi.co/trade-api/v2/portfolio/events/orders/order-1",
        )
        .unwrap();
        add_market_ticker(&mut url, "KXTEST-X/Y").unwrap();
        assert_eq!(
            url.as_str(),
            "https://external-api.demo.kalshi.co/trade-api/v2/portfolio/events/orders/order-1?market_ticker=KXTEST-X%2FY"
        );
        assert!(add_market_ticker(&mut url, "").is_err());
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

    #[test]
    fn accepts_empty_no_content_cancel_response() {
        assert!(OrderClient::ensure_success(
            reqwest::StatusCode::NO_CONTENT,
            "",
            "cancel all orders"
        )
        .is_ok());
    }

    #[test]
    fn retains_cancel_error_body() {
        let error = OrderClient::ensure_success(
            reqwest::StatusCode::UNAUTHORIZED,
            "signature rejected",
            "cancel all orders",
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("401 Unauthorized: signature rejected"));
    }
}
