//! Live paper trader — places NO real orders. Simulates maker-side entries on
//! Kalshi MLB moneyline markets after scoring plays, using the live websocket
//! order book + trade tape for fill simulation, and MLB GUMBO polling for signals.
//!
//! Strategy (from the pilot event study): after a scoring play, the batting
//! team's price keeps drifting up for ~5 min. We join the best bid (maker, no
//! taker fee), hold `hold_minutes`, then exit.
//!
//! Fill model (honest bounds):
//!   queue-ahead = displayed size at our level when we "place"; we fill after
//!   that much volume prints at our price or better (sells into the bid).
//!   `fills_optimistic` additionally logs the first-print fill time.
//!   Exit: maker ask-join with the same queue model; if unfilled after
//!   `exit_timeout_s`, cross the spread at the bid and pay the taker fee.
//!
//! Run: cargo run --release -p paper-trader -- --config config.toml
//! Needs Kalshi API credentials (websocket requires auth). Market data only —
//! this binary cannot place orders even by accident: it never calls POST.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use clap::Parser;
use common::{auth::Signer, KalshiClient, MlbClient, PlayRow, KALSHI_WS};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
}

#[derive(Deserialize)]
struct Config {
    key_id: String,
    private_key_path: String,
    /// contracts per signal
    #[serde(default = "d_size")]
    size: f64,
    #[serde(default = "d_hold")]
    hold_minutes: i64,
    #[serde(default = "d_exit_timeout")]
    exit_timeout_s: i64,
    /// only trade when spread (cents) is at most this
    #[serde(default = "d_spread")]
    max_spread_cents: i64,
    /// which event types trigger entries
    #[serde(default = "d_events")]
    entry_events: Vec<String>,
    #[serde(default = "d_log")]
    log_path: String,
}
fn d_size() -> f64 { 10.0 }
fn d_hold() -> i64 { 5 }
fn d_exit_timeout() -> i64 { 120 }
fn d_spread() -> i64 { 2 }
fn d_events() -> Vec<String> {
    ["home_run", "single", "double", "triple", "walk", "hit_by_pitch"]
        .iter().map(|s| s.to_string()).collect()
}
fn d_log() -> String { "paper_trades.csv".into() }

// ---------------------------------------------------------------- order book

/// price (cents 1-99) -> resting size, per side. Kalshi ws sends deltas on the
/// yes book; "no" side deltas are mirrored to yes-complement internally.
#[derive(Default)]
struct Book {
    yes_bids: BTreeMap<i64, f64>, // buy YES orders
    yes_asks: BTreeMap<i64, f64>, // from NO-side liquidity: ask = 100 - no_bid
}

impl Book {
    fn best_bid(&self) -> Option<(i64, f64)> {
        self.yes_bids.iter().next_back().map(|(p, s)| (*p, *s))
    }
    fn best_ask(&self) -> Option<(i64, f64)> {
        self.yes_asks.iter().next().map(|(p, s)| (*p, *s))
    }
    fn apply(&mut self, side: &str, price: i64, delta: f64) {
        let (map, key) = match side {
            "yes" => (&mut self.yes_bids, price),
            _ => (&mut self.yes_asks, 100 - price),
        };
        let e = map.entry(key).or_insert(0.0);
        *e += delta;
        if *e <= 1e-9 {
            map.remove(&key);
        }
    }
    fn set_snapshot(&mut self, yes: &[(i64, f64)], no: &[(i64, f64)]) {
        self.yes_bids = yes.iter().copied().collect();
        self.yes_asks = no.iter().map(|(p, s)| (100 - p, *s)).collect();
    }
}

// ---------------------------------------------------------------- sim orders

#[derive(Debug, Clone, Copy, PartialEq)]
enum OrderState { Resting, Filled, TimedOut }

#[derive(Debug)]
struct SimOrder {
    ticker: String,
    /// true = buying yes (entry), false = selling yes (exit)
    is_buy: bool,
    price: i64, // cents
    size: f64,
    queue_ahead: f64,
    filled_volume: f64,
    placed_at: chrono::DateTime<Utc>,
    first_print_at: Option<chrono::DateTime<Utc>>, // optimistic fill marker
    state: OrderState,
}

/// Kalshi taker fee (dollars) for `count` contracts at price p (cents):
/// ceil_cents(0.07 * count * p/100 * (1 - p/100))
fn taker_fee(count: f64, price_cents: i64) -> f64 {
    let p = price_cents as f64 / 100.0;
    (0.07 * count * p * (1.0 - p) * 100.0).ceil() / 100.0
}

// ---------------------------------------------------------------- events

enum Tick {
    /// trade print: ticker, yes price cents, count, taker_side
    Trade(String, i64, f64, String),
    Delta(String, String, i64, f64),
    Snapshot(String, Vec<(i64, f64)>, Vec<(i64, f64)>),
    Signal { ticker: String, event: String, detail: String },
}

// ---------------------------------------------------------------- ws task

async fn ws_task(signer: Signer, tickers: Vec<String>, tx: mpsc::Sender<Tick>) -> Result<()> {
    let (ts, sig) = signer.headers("GET", "/trade-api/ws/v2")?;
    let mut req = KALSHI_WS.into_client_request()?;
    let h = req.headers_mut();
    h.insert("KALSHI-ACCESS-KEY", signer.key_id.parse()?);
    h.insert("KALSHI-ACCESS-SIGNATURE", sig.parse()?);
    h.insert("KALSHI-ACCESS-TIMESTAMP", ts.parse()?);

    let (ws, _) = tokio_tungstenite::connect_async(req).await.context("ws connect")?;
    let (mut sink, mut stream) = ws.split();
    let sub = serde_json::json!({
        "id": 1, "cmd": "subscribe",
        "params": {"channels": ["orderbook_delta", "trade"], "market_tickers": tickers}
    });
    sink.send(Message::Text(sub.to_string())).await?;
    tracing::info!("ws subscribed");

    while let Some(msg) = stream.next().await {
        let Message::Text(txt) = msg? else { continue };
        let v: serde_json::Value = serde_json::from_str(&txt)?;
        match v["type"].as_str().unwrap_or("") {
            "orderbook_snapshot" => {
                let m = &v["msg"];
                let parse = |k: &str| -> Vec<(i64, f64)> {
                    m[k].as_array().unwrap_or(&vec![]).iter()
                        .filter_map(|lv| {
                            Some((lv[0].as_i64()?,
                                  lv[1].as_f64().or_else(|| lv[1].as_str()?.parse().ok())?))
                        }).collect()
                };
                tx.send(Tick::Snapshot(
                    m["market_ticker"].as_str().unwrap_or("").into(),
                    parse("yes"), parse("no"),
                )).await.ok();
            }
            "orderbook_delta" => {
                let m = &v["msg"];
                tx.send(Tick::Delta(
                    m["market_ticker"].as_str().unwrap_or("").into(),
                    m["side"].as_str().unwrap_or("yes").into(),
                    m["price"].as_i64().unwrap_or(0),
                    m["delta"].as_f64()
                        .or_else(|| m["delta"].as_str().and_then(|s| s.parse().ok()))
                        .unwrap_or(0.0),
                )).await.ok();
            }
            "trade" => {
                let m = &v["msg"];
                let price = m["yes_price"].as_i64().unwrap_or(0);
                let count = m["count"].as_f64()
                    .or_else(|| m["count_fp"].as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0.0);
                tx.send(Tick::Trade(
                    m["market_ticker"].as_str().unwrap_or("").into(),
                    price, count,
                    m["taker_side"].as_str().unwrap_or("").into(),
                )).await.ok();
            }
            _ => {}
        }
    }
    Err(anyhow!("websocket closed"))
}

// ---------------------------------------------------------------- gumbo poll

/// Polls each live game's GUMBO feed every ~3 s; emits a Signal for each new
/// completed play whose event_type is in `entry_events`. The signal names the
/// market of the BATTING team.
async fn gumbo_task(
    games: Vec<(i64, String, String)>, // (game_pk, away_ticker, home_ticker)
    entry_events: Vec<String>,
    tx: mpsc::Sender<Tick>,
) -> Result<()> {
    let mlb = MlbClient::new()?;
    let mut seen: HashMap<i64, i64> = HashMap::new(); // game_pk -> last atBatIndex
    loop {
        for (pk, away_t, home_t) in &games {
            let plays: Vec<PlayRow> = match mlb.gumbo_plays(*pk).await {
                Ok(p) => p,
                Err(e) => { tracing::warn!(pk, %e, "gumbo poll failed"); continue; }
            };
            let last = seen.entry(*pk).or_insert(-1);
            for p in plays.iter().filter(|p| p.at_bat_index > *last) {
                if entry_events.iter().any(|e| e == &p.event_type) {
                    let ticker = if p.half == "top" { away_t } else { home_t };
                    if !ticker.is_empty() {
                        tx.send(Tick::Signal {
                            ticker: ticker.clone(),
                            event: p.event_type.clone(),
                            detail: format!("inn{} {} {}-{}", p.inning, p.half,
                                            p.away_score, p.home_score),
                        }).await.ok();
                    }
                }
                *last = p.at_bat_index;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
}

// ---------------------------------------------------------------- main loop

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let cfg: Config = toml::from_str(&std::fs::read_to_string(&args.config)?)
        .context("parsing config.toml")?;
    let signer = Signer::from_pem_file(&cfg.key_id, &cfg.private_key_path)?;

    // ---- discover today's games & their ML market tickers (public REST)
    let kalshi = KalshiClient::new(8)?;
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let mlb = MlbClient::new()?;
    let sched = mlb.schedule(&today).await?;

    #[derive(Deserialize)]
    struct EventsResp { events: Vec<serde_json::Value>, #[serde(default)] cursor: String }
    let mut markets_by_teams: HashMap<String, String> = HashMap::new(); // "AWY|HOM" per side
    let mut cursor = String::new();
    loop {
        let mut url = format!("{}/events?series_ticker=KXMLBGAME&status=open&limit=200&with_nested_markets=true",
                              common::KALSHI_BASE);
        if !cursor.is_empty() { url.push_str(&format!("&cursor={cursor}")); }
        let resp: EventsResp = kalshi.get_json(&url).await?;
        for ev in &resp.events {
            let et = ev["event_ticker"].as_str().unwrap_or("");
            if let Some((_, away, home)) = common::parse_event_teams(et) {
                for m in ev["markets"].as_array().unwrap_or(&vec![]) {
                    let ticker = m["ticker"].as_str().unwrap_or("");
                    if let Some(side) = ticker.rsplit('-').next() {
                        markets_by_teams.insert(format!("{away}|{home}|{side}"), ticker.into());
                    }
                }
            }
        }
        if resp.cursor.is_empty() { break; }
        cursor = resp.cursor;
    }

    // join schedule to markets
    let mut games = Vec::new();
    let mut tickers = Vec::new();
    for (pk, away, home, _) in &sched {
        // find kalshi codes whose alias sets contain these MLB abbrevs
        let find = |mlb_ab: &str, other: &str, want_side: bool| -> String {
            markets_by_teams.iter()
                .find(|(k, _)| {
                    let p: Vec<&str> = k.split('|').collect();
                    let (a, h, side) = (p[0], p[1], p[2]);
                    common::team_aliases(a).contains(&if want_side { mlb_ab } else { other })
                        && common::team_aliases(h).contains(&if want_side { other } else { mlb_ab })
                        && common::team_aliases(side).contains(&mlb_ab)
                })
                .map(|(_, v)| v.clone()).unwrap_or_default()
        };
        let away_ticker = find(away, home, true);
        let home_ticker = find(home, away, false);
        if !away_ticker.is_empty() || !home_ticker.is_empty() {
            for t in [&away_ticker, &home_ticker] {
                if !t.is_empty() { tickers.push(t.clone()); }
            }
            games.push((*pk, away_ticker, home_ticker));
        }
    }
    tracing::info!(n_games = games.len(), n_markets = tickers.len(), "watching");
    if games.is_empty() {
        return Err(anyhow!("no live MLB markets found for {today}"));
    }

    // ---- log file
    let mut log = csv::Writer::from_path(&cfg.log_path)?;
    log.write_record([
        "signal_time", "ticker", "event", "detail", "entry_price", "entry_filled_at",
        "entry_filled_at_optimistic", "exit_price", "exit_mode", "pnl_cents_conservative",
        "pnl_cents_optimistic", "fees",
    ])?;

    // ---- spin up tasks
    let (tx, mut rx) = mpsc::channel::<Tick>(4096);
    tokio::spawn({
        let tx = tx.clone();
        let tickers = tickers.clone();
        async move {
            if let Err(e) = ws_task(signer, tickers, tx).await {
                tracing::error!(%e, "ws task died");
            }
        }
    });
    tokio::spawn(gumbo_task(games, cfg.entry_events.clone(), tx.clone()));

    let mut books: HashMap<String, Book> = HashMap::new();
    let mut open_entries: Vec<SimOrder> = Vec::new();
    let mut positions: Vec<(SimOrder, chrono::DateTime<Utc>)> = Vec::new(); // filled entry, fill time
    let mut open_exits: Vec<(SimOrder, SimOrder)> = Vec::new(); // (entry, exit order)

    while let Some(tick) = rx.recv().await {
        let now = Utc::now();
        match tick {
            Tick::Snapshot(t, yes, no) => {
                books.entry(t).or_default().set_snapshot(&yes, &no);
            }
            Tick::Delta(t, side, price, delta) => {
                books.entry(t).or_default().apply(&side, price, delta);
            }
            Tick::Trade(t, price, count, _taker) => {
                // advance queues: sells print at/below our bid; buys at/above our ask
                for o in open_entries.iter_mut().chain(open_exits.iter_mut().map(|(_, e)| e)) {
                    if o.ticker != t || o.state != OrderState::Resting { continue; }
                    let crosses = if o.is_buy { price <= o.price } else { price >= o.price };
                    if crosses {
                        o.first_print_at.get_or_insert(now);
                        o.filled_volume += count;
                        if o.filled_volume >= o.queue_ahead + o.size {
                            o.state = OrderState::Filled;
                        }
                    }
                }
            }
            Tick::Signal { ticker, event, detail } => {
                let Some(book) = books.get(&ticker) else { continue };
                let (Some((bid, bid_sz)), Some((ask, _))) = (book.best_bid(), book.best_ask())
                    else { continue };
                if ask - bid > cfg.max_spread_cents || bid < 5 || bid > 95 {
                    continue; // too wide or too close to settled
                }
                tracing::info!(%ticker, %event, %detail, bid, ask, "SIGNAL -> join bid");
                open_entries.push(SimOrder {
                    ticker, is_buy: true, price: bid, size: cfg.size,
                    queue_ahead: bid_sz, filled_volume: 0.0, placed_at: now,
                    first_print_at: None, state: OrderState::Resting,
                });
                // stash signal context in the log row when the round trip closes
                let _ = (&event, &detail);
            }
        }

        // ---- lifecycle: entries that filled -> positions; hold; exits
        let mut i = 0;
        while i < open_entries.len() {
            match open_entries[i].state {
                OrderState::Filled => {
                    let o = open_entries.remove(i);
                    tracing::info!(ticker = %o.ticker, price = o.price, "entry filled");
                    positions.push((o, now));
                }
                OrderState::Resting
                    if (now - open_entries[i].placed_at).num_seconds() > 90 =>
                {
                    // never filled inside the reaction window -> cancel
                    open_entries.remove(i);
                }
                _ => i += 1,
            }
        }
        let mut i = 0;
        while i < positions.len() {
            if (now - positions[i].1).num_minutes() >= cfg.hold_minutes {
                let (entry, _) = positions.remove(i);
                let book = books.get(&entry.ticker);
                let (ask, ask_sz) = book.and_then(|b| b.best_ask()).unwrap_or((99, 0.0));
                open_exits.push((entry, SimOrder {
                    ticker: String::new(), // filled from entry on log
                    is_buy: false, price: ask, size: cfg.size, queue_ahead: ask_sz,
                    filled_volume: 0.0, placed_at: now, first_print_at: None,
                    state: OrderState::Resting,
                }));
                let n = open_exits.len() - 1;
                open_exits[n].1.ticker = open_exits[n].0.ticker.clone();
            } else { i += 1; }
        }
        let mut i = 0;
        while i < open_exits.len() {
            let done = {
                let (entry, exit) = &mut open_exits[i];
                let timed_out = (now - exit.placed_at).num_seconds() > cfg.exit_timeout_s;
                if exit.state == OrderState::Filled || timed_out {
                    // conservative: maker exit at exit.price if filled, else cross at bid + fee
                    let (exit_px, mode, fee) = if exit.state == OrderState::Filled {
                        (exit.price, "maker", 0.0)
                    } else {
                        let bid = books.get(&entry.ticker)
                            .and_then(|b| b.best_bid()).map(|(p, _)| p).unwrap_or(1);
                        (bid, "taker", taker_fee(exit.size, bid))
                    };
                    let pnl_c = (exit_px - entry.price) as f64 * entry.size
                        - fee * 100.0;
                    let pnl_opt = pnl_c; // same round trip; optimistic differs on entry time only
                    log.write_record([
                        entry.placed_at.to_rfc3339(), entry.ticker.clone(), "", "",
                        entry.price.to_string(),
                        entry.first_print_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                        entry.first_print_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                        exit_px.to_string(), mode.into(),
                        format!("{pnl_c:.1}"), format!("{pnl_opt:.1}"), format!("{fee:.2}"),
                    ])?;
                    log.flush()?;
                    tracing::info!(ticker = %entry.ticker, entry = entry.price, exit = exit_px,
                                   mode, pnl_cents = pnl_c, "round trip closed");
                    true
                } else { false }
            };
            if done { open_exits.remove(i); } else { i += 1; }
        }
    }
    Ok(())
}
