//! Deliberately guarded Kalshi execution process.
//!
//! It consumes a desired-position JSON file emitted by a strategy adapter;
//! strategy code never imports this crate or sends orders directly. The binary
//! starts disabled, is intended for Kalshi Demo first, and refuses production
//! unless both `live_enabled` and `confirm_prod` are true.

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use common::{
    auth::Signer,
    orders::{LimitOrder, OrderClient, TradingPermission},
    portfolio::{parse_fill, Fill, PortfolioClient},
    risk::{self, Limits, OrderCheck, RiskBook},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use uuid::Uuid;

#[derive(Parser)]
#[command(about = "Guarded, demo-first Kalshi position executor")]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Reconcile then continuously converge on the desired-position file.
    Run,
    /// Place, query, and cancel one deliberately resting 1-contract DEMO order.
    Smoke {
        #[arg(long)]
        ticker: String,
        #[arg(long)]
        price_cents: i64,
        #[arg(long, default_value = "bid")]
        side: String,
    },
    /// Cancel resting orders and flatten all whole-contract positions at a
    /// fresh displayed price using reduce-only fill-or-kill orders.
    Flatten,
}

#[derive(Deserialize)]
struct Config {
    key_id: String,
    private_key_path: String,
    api_base: String,
    ws_base: String,
    #[serde(default)]
    live_enabled: bool,
    #[serde(default)]
    confirm_prod: bool,
    #[serde(default)]
    adopt_positions_on_boot: bool,
    #[serde(default)]
    balance_floor_cents: i64,
    #[serde(default = "d_max_exposure")]
    max_exposure_cents: i64,
    #[serde(default = "d_market_cap")]
    per_market_cap_cents: i64,
    #[serde(default = "d_daily_stop")]
    daily_loss_stop_cents: i64,
    #[serde(default = "d_order_rate")]
    max_orders_per_min: usize,
    #[serde(default = "d_price_distance")]
    max_price_distance_cents: i64,
    #[serde(default = "d_intents")]
    intent_path: PathBuf,
    #[serde(default = "d_run_dir")]
    run_dir: PathBuf,
    #[serde(default = "d_stale")]
    stale_data_seconds: i64,
    #[serde(default = "d_poll")]
    poll_seconds: u64,
}
fn d_max_exposure() -> i64 {
    2_500
}
fn d_market_cap() -> i64 {
    500
}
fn d_daily_stop() -> i64 {
    500
}
fn d_order_rate() -> usize {
    10
}
fn d_price_distance() -> i64 {
    5
}
fn d_intents() -> PathBuf {
    "desired_positions.json".into()
}
fn d_run_dir() -> PathBuf {
    ".".into()
}
fn d_stale() -> i64 {
    30
}
fn d_poll() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct IntentFile {
    /// Unix milliseconds when both sources were last observed by the strategy.
    book_updated_at_ms: i64,
    game_updated_at_ms: i64,
    intents: Vec<Intent>,
}

#[derive(Debug, Deserialize)]
struct Intent {
    ticker: String,
    /// Stable UUID created by the strategy once per desired order. It MUST NOT
    /// change when retrying an order after a timeout.
    client_order_id: String,
    /// Desired long-YES position. The initial executor intentionally supports
    /// whole contracts only; the strategy must not emit fractional targets.
    target_contracts: i64,
    limit_price_cents: i64,
    mid_price_cents: f64,
    /// The strategy selects a resting maker order or an explicit FOK exit.
    #[serde(default = "d_time_in_force")]
    time_in_force: String,
    #[serde(default = "d_post_only")]
    post_only: bool,
}

fn d_time_in_force() -> String {
    "good_till_canceled".into()
}
fn d_post_only() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default)]
struct Position {
    contracts: i64,
    avg_cost_cents: f64,
}

enum FillEvent {
    Connected,
    Fill(Fill),
    Disconnected,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let cfg: Config = toml::from_str(&std::fs::read_to_string(&args.config)?)
        .context("parsing executor config")?;
    if risk::is_locked_out(&cfg.run_dir, Utc::now()) {
        bail!(
            "today's LOCKOUT file exists in {}; refusing to start",
            cfg.run_dir.display()
        );
    }
    let signer = Signer::from_pem_file(&cfg.key_id, &cfg.private_key_path)?;
    let permission =
        TradingPermission::authorize(&cfg.api_base, cfg.live_enabled, cfg.confirm_prod)?;
    let portfolio = PortfolioClient::new(signer.clone(), &cfg.api_base)?;
    let orders = OrderClient::new(signer.clone(), &cfg.api_base, permission)?;

    match args.command {
        Command::Smoke {
            ticker,
            price_cents,
            side,
        } => smoke(&cfg, &portfolio, &orders, &ticker, price_cents, &side).await,
        Command::Flatten => flatten(&cfg, &portfolio, &orders).await,
        Command::Run => run(cfg, signer, portfolio, orders).await,
    }
}

fn is_production(api_base: &str) -> bool {
    reqwest::Url::parse(api_base)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .is_some_and(|h| {
            matches!(
                h.as_str(),
                "external-api.kalshi.com" | "api.elections.kalshi.com"
            )
        })
}

async fn smoke(
    cfg: &Config,
    portfolio: &PortfolioClient,
    orders: &OrderClient,
    ticker: &str,
    price: i64,
    side: &str,
) -> Result<()> {
    anyhow::ensure!(!is_production(&cfg.api_base), "smoke tests are DEMO-only");
    anyhow::ensure!(matches!(side, "bid" | "ask"), "side must be bid or ask");
    anyhow::ensure!((1..=99).contains(&price), "price_cents must be 1..99");
    let balance = portfolio.balance_cents().await?;
    anyhow::ensure!(
        balance >= cfg.balance_floor_cents,
        "balance below configured floor"
    );
    let id = Uuid::new_v4().to_string();
    let order = LimitOrder {
        ticker,
        client_order_id: &id,
        side,
        count: "1.00",
        price: &dollars(price),
        time_in_force: "good_till_canceled",
        self_trade_prevention_type: "taker_at_cross",
        post_only: Some(true),
        cancel_order_on_pause: Some(true),
        reduce_only: Some(false),
    };
    let created = orders.create_order(&order).await?;
    let order_id = created["order_id"]
        .as_str()
        .context("create response missing order_id")?;
    let queried = match orders.get_order(order_id, ticker).await {
        Ok(queried) => queried,
        Err(get_error) => {
            if let Err(cancel_error) = orders.cancel_order(order_id, ticker).await {
                return Err(get_error).context(format!(
                    "demo smoke lookup failed and cleanup failed: {cancel_error}"
                ));
            }
            return Err(get_error).context("demo smoke lookup failed; test order was cancelled");
        }
    };
    tracing::info!(%ticker, %order_id, response = %queried, "demo smoke order exists; cancelling");
    orders.cancel_order(order_id, ticker).await?;
    wait_for_order_cancelled(portfolio, order_id).await?;
    tracing::info!(%order_id, "demo smoke test passed: created, queried, and cancelled exactly one order");
    Ok(())
}

/// The write response can arrive before the portfolio read model reflects a
/// cancellation. Confirm that a specific smoke order is absent before calling
/// the test successful, without touching any other resting orders.
async fn wait_for_order_cancelled(portfolio: &PortfolioClient, order_id: &str) -> Result<()> {
    for attempt in 0..3 {
        if !portfolio
            .resting_orders()
            .await?
            .iter()
            .any(|(id, _, _)| id == order_id)
        {
            return Ok(());
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    bail!("cancel has not settled after reconciliation wait")
}

async fn run(
    cfg: Config,
    signer: Signer,
    portfolio: PortfolioClient,
    orders: OrderClient,
) -> Result<()> {
    let balance = portfolio.balance_cents().await?;
    anyhow::ensure!(
        balance >= cfg.balance_floor_cents,
        "balance below configured floor"
    );
    cancel_and_verify(&portfolio, &orders).await?;
    let positions = load_positions(&portfolio).await?;
    if !cfg.adopt_positions_on_boot && !positions.is_empty() {
        bail!("positions exist after reconciliation; run `kalshi-executor --config {} flatten` and verify before starting", cfg_path_hint(&cfg));
    }

    let mut actual = positions;
    let mut risk = RiskBook::new(Limits {
        max_exposure_cents: cfg.max_exposure_cents,
        per_market_cap_cents: cfg.per_market_cap_cents,
        daily_loss_stop_cents: cfg.daily_loss_stop_cents,
        max_orders_per_min: cfg.max_orders_per_min,
        max_price_distance_cents: cfg.max_price_distance_cents,
    });
    let (fill_tx, mut fill_rx) = mpsc::channel(256);
    let ws_base = cfg.ws_base.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = fills_task(signer.clone(), ws_base.clone(), fill_tx.clone()).await {
                tracing::warn!(%e, "fill websocket disconnected; reconnecting in 5s");
                let _ = fill_tx.send(FillEvent::Disconnected).await;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    let mut fills_connected = false;
    loop {
        while let Ok(event) = fill_rx.try_recv() {
            match event {
                FillEvent::Connected => fills_connected = true,
                FillEvent::Fill(fill) => apply_fill(&mut actual, &mut risk, &fill)?,
                FillEvent::Disconnected => {
                    orders.cancel_all_orders().await?;
                    bail!("fill stream disconnected; cancelled resting orders and stopped for reconciliation");
                }
            }
        }
        if !fills_connected {
            tracing::warn!("waiting for authenticated fill stream before submitting orders");
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        if risk::kill_requested(&cfg.run_dir) {
            tracing::error!("KILL file found; cancelling then flattening");
            flatten(&cfg, &portfolio, &orders).await?;
            return Ok(());
        }
        if risk.daily_loss_hit() {
            risk::write_lockout(&cfg.run_dir, Utc::now(), "daily loss stop")?;
            tracing::error!("daily loss stop reached; cancelling then flattening");
            flatten(&cfg, &portfolio, &orders).await?;
            return Ok(());
        }
        let file = read_intents(&cfg.intent_path)?;
        if !is_fresh(&file, cfg.stale_data_seconds) {
            tracing::error!("strategy data is stale; cancelling then flattening");
            flatten(&cfg, &portfolio, &orders).await?;
            return Ok(());
        }
        reconcile(&file, &mut actual, &mut risk, &portfolio, &orders).await?;
        tokio::time::sleep(Duration::from_secs(cfg.poll_seconds.max(1))).await;
    }
}

async fn reconcile(
    file: &IntentFile,
    actual: &mut HashMap<String, Position>,
    risk: &mut RiskBook,
    portfolio: &PortfolioClient,
    orders: &OrderClient,
) -> Result<()> {
    let desired_by_id: HashMap<&str, &Intent> = file
        .intents
        .iter()
        .map(|i| (i.client_order_id.as_str(), i))
        .collect();
    let resting = portfolio.resting_orders().await?;
    for (order_id, ticker, raw) in &resting {
        if let Some(id) = raw["client_order_id"].as_str() {
            let current = actual.get(ticker).copied().unwrap_or_default().contracts;
            let still_needed = desired_by_id
                .get(id)
                .is_some_and(|intent| intent.target_contracts != current);
            if !still_needed {
                orders.cancel_order(order_id, ticker).await?;
            }
        }
    }
    for intent in &file.intents {
        validate_intent(intent)?;
        let current = actual
            .get(&intent.ticker)
            .copied()
            .unwrap_or_default()
            .contracts;
        if current == intent.target_contracts {
            continue;
        }
        if resting
            .iter()
            .any(|(_, ticker, _)| ticker == &intent.ticker)
        {
            continue;
        }
        let reducing = intent.target_contracts < current;
        let count = (intent.target_contracts - current).unsigned_abs() as i64;
        let check = OrderCheck {
            ticker: &intent.ticker,
            price_cents: intent.limit_price_cents,
            count,
            mid_cents: Some(intent.mid_price_cents),
            reduce_only: reducing,
        };
        if let Err(veto) = risk.approve(Utc::now(), &check) {
            tracing::warn!(ticker = %intent.ticker, %veto, "risk vetoed intent");
            continue;
        }
        let price = dollars(intent.limit_price_cents);
        let count_s = format!("{count}.00");
        let order = LimitOrder {
            ticker: &intent.ticker,
            client_order_id: &intent.client_order_id,
            side: if reducing { "ask" } else { "bid" },
            count: &count_s,
            price: &price,
            time_in_force: &intent.time_in_force,
            self_trade_prevention_type: "taker_at_cross",
            post_only: Some(intent.post_only),
            cancel_order_on_pause: Some(true),
            reduce_only: Some(reducing),
        };
        if let Err(e) = orders.create_order(&order).await {
            if !reducing {
                risk.release_pending(&intent.ticker, intent.limit_price_cents * count);
            }
            return Err(e).context("order submission failed; reconcile before retrying");
        }
    }
    Ok(())
}

async fn flatten(cfg: &Config, portfolio: &PortfolioClient, orders: &OrderClient) -> Result<()> {
    cancel_and_verify(portfolio, orders).await?;
    let positions = load_positions(portfolio).await?;
    let mut risk = RiskBook::new(Limits {
        max_exposure_cents: cfg.max_exposure_cents,
        per_market_cap_cents: cfg.per_market_cap_cents,
        daily_loss_stop_cents: cfg.daily_loss_stop_cents,
        max_orders_per_min: cfg.max_orders_per_min,
        max_price_distance_cents: cfg.max_price_distance_cents,
    });
    for (ticker, pos) in positions {
        let quote = quote(&cfg.api_base, &ticker).await?;
        let (side, price) = if pos.contracts > 0 {
            ("ask", quote.yes_bid_cents)
        } else {
            ("bid", quote.yes_ask_cents)
        };
        anyhow::ensure!(
            price > 0,
            "no executable displayed price for {ticker}; refusing blind flatten"
        );
        let count = pos.contracts.unsigned_abs() as i64;
        risk.approve(
            Utc::now(),
            &OrderCheck {
                ticker: &ticker,
                price_cents: price,
                count,
                mid_cents: Some((quote.yes_bid_cents + quote.yes_ask_cents) as f64 / 2.0),
                reduce_only: true,
            },
        )
        .map_err(|v| anyhow!("flatten risk veto for {ticker}: {v}"))?;
        let id = Uuid::new_v4().to_string();
        let price_s = dollars(price);
        let count_s = format!("{count}.00");
        let order = LimitOrder {
            ticker: &ticker,
            client_order_id: &id,
            side,
            count: &count_s,
            price: &price_s,
            time_in_force: "fill_or_kill",
            self_trade_prevention_type: "taker_at_cross",
            post_only: Some(false),
            cancel_order_on_pause: Some(true),
            reduce_only: Some(true),
        };
        orders
            .create_order(&order)
            .await
            .context("reduce-only flatten order")?;
    }
    Ok(())
}

async fn cancel_and_verify(portfolio: &PortfolioClient, orders: &OrderClient) -> Result<()> {
    orders.cancel_all_orders().await?;
    for _ in 0..3 {
        if portfolio.resting_orders().await?.is_empty() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    bail!("resting orders remain after cancel-all; refusing to continue")
}

async fn load_positions(portfolio: &PortfolioClient) -> Result<HashMap<String, Position>> {
    let mut out = HashMap::new();
    for (ticker, contracts, raw) in portfolio.positions().await? {
        anyhow::ensure!(
            (contracts.fract()).abs() < 1e-9,
            "fractional position in {ticker}; executor only supports whole contracts"
        );
        let exposure = raw["market_exposure_dollars"]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
            * 100.0;
        out.insert(
            ticker,
            Position {
                contracts: contracts as i64,
                avg_cost_cents: exposure / contracts.abs().max(1.0),
            },
        );
    }
    Ok(out)
}

fn apply_fill(
    actual: &mut HashMap<String, Position>,
    risk: &mut RiskBook,
    fill: &Fill,
) -> Result<()> {
    anyhow::ensure!(
        (fill.count.fract()).abs() < 1e-9,
        "fractional fill unsupported by whole-contract executor"
    );
    let qty = fill.count as i64;
    let pos = actual.entry(fill.ticker.clone()).or_default();
    match fill.action.as_str() {
        "buy" => {
            let old = pos.contracts.max(0) as f64;
            pos.avg_cost_cents = if old == 0.0 {
                fill.yes_price_cents as f64
            } else {
                (pos.avg_cost_cents * old + fill.yes_price_cents as f64 * qty as f64)
                    / (old + qty as f64)
            };
            pos.contracts += qty;
            risk.on_fill_open(&fill.ticker, fill.yes_price_cents * qty);
        }
        "sell" => {
            let pnl =
                ((fill.yes_price_cents as f64 - pos.avg_cost_cents) * qty as f64).round() as i64;
            pos.contracts -= qty;
            risk.on_exposure_change(
                &fill.ticker,
                -(pos.avg_cost_cents * qty as f64).round() as i64,
            );
            risk.on_realized(pnl);
        }
        _ => bail!("unknown fill action {}", fill.action),
    }
    Ok(())
}

async fn fills_task(signer: Signer, ws_base: String, tx: mpsc::Sender<FillEvent>) -> Result<()> {
    let (ts, sig) = signer.headers("GET", "/trade-api/ws/v2")?;
    let mut req = ws_base.as_str().into_client_request()?;
    let headers = req.headers_mut();
    headers.insert("KALSHI-ACCESS-KEY", signer.key_id.parse()?);
    headers.insert("KALSHI-ACCESS-SIGNATURE", sig.parse()?);
    headers.insert("KALSHI-ACCESS-TIMESTAMP", ts.parse()?);
    let (ws, _) = connect_async(req).await.context("fill websocket connect")?;
    let (mut sink, mut stream) = ws.split();
    sink.send(Message::Text(
        serde_json::json!({"id": 1, "cmd": "subscribe", "params": {"channels": ["fill"]}})
            .to_string(),
    ))
    .await?;
    tx.send(FillEvent::Connected).await?;
    while let Some(message) = stream.next().await {
        let Message::Text(text) = message? else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_str(&text)?;
        if value["type"].as_str() == Some("fill") {
            if let Some(fill) = parse_fill(&value["msg"]) {
                tx.send(FillEvent::Fill(fill)).await?;
            }
        }
    }
    bail!("fill websocket closed")
}

struct Quote {
    yes_bid_cents: i64,
    yes_ask_cents: i64,
}
async fn quote(api_base: &str, ticker: &str) -> Result<Quote> {
    let url = format!("{}/markets/{ticker}", api_base.trim_end_matches('/'));
    let value: serde_json::Value = reqwest::get(&url).await?.error_for_status()?.json().await?;
    let cents = |v: &serde_json::Value| {
        v.as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|n| (n * 100.0).round() as i64)
    };
    Ok(Quote {
        yes_bid_cents: cents(&value["market"]["yes_bid_dollars"]).context("yes_bid_dollars")?,
        yes_ask_cents: cents(&value["market"]["yes_ask_dollars"]).context("yes_ask_dollars")?,
    })
}

fn read_intents(path: &Path) -> Result<IntentFile> {
    serde_json::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .context("parsing desired positions")
}
fn is_fresh(file: &IntentFile, limit_s: i64) -> bool {
    let now = Utc::now().timestamp_millis();
    [file.book_updated_at_ms, file.game_updated_at_ms]
        .iter()
        .all(|t| now >= *t && now - *t <= limit_s * 1_000)
}
fn validate_intent(i: &Intent) -> Result<()> {
    anyhow::ensure!(
        !i.ticker.is_empty() && !i.client_order_id.is_empty(),
        "intent ticker/client_order_id missing"
    );
    Uuid::parse_str(&i.client_order_id).context("client_order_id must be a UUID")?;
    anyhow::ensure!(i.target_contracts >= 0, "short targets are unsupported");
    anyhow::ensure!(
        (1..=99).contains(&i.limit_price_cents),
        "limit price must be 1..99 cents"
    );
    anyhow::ensure!(i.mid_price_cents.is_finite(), "mid price must be finite");
    anyhow::ensure!(
        matches!(
            i.time_in_force.as_str(),
            "good_till_canceled" | "fill_or_kill"
        ),
        "unsupported time_in_force"
    );
    anyhow::ensure!(
        !(i.post_only && i.time_in_force == "fill_or_kill"),
        "fill_or_kill must not be post_only"
    );
    Ok(())
}
fn dollars(cents: i64) -> String {
    format!("{:.2}", cents as f64 / 100.0)
}
fn cfg_path_hint(cfg: &Config) -> String {
    cfg.run_dir.join("config.toml").display().to_string()
}
