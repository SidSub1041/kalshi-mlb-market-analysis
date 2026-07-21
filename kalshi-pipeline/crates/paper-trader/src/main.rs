//! Live paper trader — places NO real orders. Simulates maker-side entries on
//! Kalshi MLB moneyline markets, using the live websocket order book + trade
//! tape for fill simulation, and MLB GUMBO polling for game state.
//!
//! Strategy (v2, fair-value): a state-based win-probability model (common::wp,
//! backtested on the full 2026 season) prices every market continuously from
//! score/inning/outs/runners + season team strength. When the model's fair
//! value diverges from the book by >= `entry_edge_cents`, join the bid
//! (maker). Add clips if the edge widens (averaging down), up to `max_clips`.
//! Exit maker when the ask rises above fair value, dump taker if the model
//! flips hard against the position, and settle at 0/100 when the game ends.
//! Entry/exit thresholds are net of the Kalshi taker fee where a taker exit
//! is the realistic outcome.
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
use chrono::{Duration as ChronoDur, Utc};
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
    /// which event types trigger entries (v1 strategy; unused by fair-value v2)
    #[serde(default = "d_events")]
    #[allow(dead_code)]
    entry_events: Vec<String>,
    #[serde(default = "d_log")]
    log_path: String,
    /// model-vs-bid divergence (cents) required to enter / add a clip.
    /// Backtest: only >=10c gaps stayed profitable net of fees.
    #[serde(default = "d_entry_edge")]
    entry_edge_cents: f64,
    /// remaining edge (cents) below which resting orders are cancelled and
    /// maker exits are placed.
    #[serde(default = "d_exit_edge")]
    exit_edge_cents: f64,
    /// max clips (size-sized buys) per market — caps averaging down
    #[serde(default = "d_max_clips")]
    max_clips: usize,
    #[serde(default = "d_ratings")]
    ratings_path: String,
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
fn d_entry_edge() -> f64 { 10.0 }
fn d_exit_edge() -> f64 { 2.0 }
fn d_max_clips() -> usize { 2 }
fn d_ratings() -> String { "ratings.csv".into() }

/// Weight of the model in the fair-value blend; the rest is market mid.
/// 0.5 was the best Brier in the season backtest (beats model and market).
const BLEND_W: f64 = 0.5;
/// Stop opening new positions on a market after this much realized loss (cents).
const MARKET_LOSS_STOP_C: f64 = -300.0;
/// Adaptive entry threshold bounds (cents) and P&L window (closed positions).
const THR_MIN: f64 = 8.0;
const THR_MAX: f64 = 15.0;
const THR_WINDOW: usize = 5;

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
    /// model fair value for a market (cents), refreshed on every new play
    Fair { ticker: String, fair_c: f64, detail: String },
    /// game ended; `won` = this market's team won
    Settle { ticker: String, won: bool },
    /// every game on the slate is final — the day is done
    AllFinal,
}

// ---------------------------------------------------------------- ws task

/// Kalshi ws now sends prices as fixed-point dollar strings ("0.5300");
/// older payloads used integer cents. Accept both, normalized to cents.
fn dollars_to_cents(v: &serde_json::Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|d| (d * 100.0).round() as i64)
        .or_else(|| v.as_i64())
}

/// Quantities arrive as fp strings ("181.00") or plain numbers.
fn fp_qty(v: &serde_json::Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

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

    // A socket can go "zombie" (open but silent) across Kalshi's overnight
    // maintenance; treat prolonged silence as death so the caller reconnects.
    loop {
        let msg = match tokio::time::timeout(
            std::time::Duration::from_secs(300), stream.next()).await
        {
            Err(_) => return Err(anyhow!("ws silent for 5 min; assuming zombie")),
            Ok(None) => break,
            Ok(Some(m)) => m,
        };
        let Message::Text(txt) = msg? else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
        match v["type"].as_str().unwrap_or("") {
            "orderbook_snapshot" => {
                let m = &v["msg"];
                let parse = |fp_key: &str, legacy_key: &str| -> Vec<(i64, f64)> {
                    let levels = m[fp_key].as_array()
                        .or_else(|| m[legacy_key].as_array());
                    levels.map(|a| a.iter()
                        .filter_map(|lv| Some((dollars_to_cents(&lv[0])?, fp_qty(&lv[1])?)))
                        .collect()).unwrap_or_default()
                };
                tx.send(Tick::Snapshot(
                    m["market_ticker"].as_str().unwrap_or("").into(),
                    parse("yes_dollars_fp", "yes"), parse("no_dollars_fp", "no"),
                )).await.ok();
            }
            "orderbook_delta" => {
                let m = &v["msg"];
                let price = dollars_to_cents(&m["price_dollars"])
                    .or_else(|| m["price"].as_i64()).unwrap_or(0);
                let delta = fp_qty(&m["delta_fp"])
                    .or_else(|| fp_qty(&m["delta"])).unwrap_or(0.0);
                tx.send(Tick::Delta(
                    m["market_ticker"].as_str().unwrap_or("").into(),
                    m["side"].as_str().unwrap_or("yes").into(),
                    price, delta,
                )).await.ok();
            }
            "trade" => {
                let m = &v["msg"];
                let price = dollars_to_cents(&m["yes_price_dollars"])
                    .or_else(|| m["yes_price"].as_i64()).unwrap_or(0);
                let count = fp_qty(&m["count_fp"])
                    .or_else(|| fp_qty(&m["count"])).unwrap_or(0.0);
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

/// Game handle for the poller: (game_pk, away_ab, home_ab, away_ticker, home_ticker)
type Game = (i64, String, String, String, String);

/// Polls each live game's GUMBO feed every ~3 s; recomputes model fair value
/// after every new completed play and emits it for both team markets. Emits
/// Settle when a game goes Final.
async fn gumbo_task(
    games: Vec<Game>,
    ratings: common::wp::Ratings,
    tx: mpsc::Sender<Tick>,
) -> Result<()> {
    let mlb = MlbClient::new()?;
    let mut seen: HashMap<i64, i64> = HashMap::new(); // game_pk -> last atBatIndex
    let mut done: HashMap<i64, bool> = HashMap::new();

    let send_fair = |p: &PlayRow, g: &Game, tx: mpsc::Sender<Tick>| {
        let (_, away, home, away_t, home_t) = g.clone();
        let wp_h = common::wp::wp_home(p, &ratings, &away, &home);
        let detail = format!("inn{} {} {}-{} outs{}", p.inning, p.half,
                             p.away_score, p.home_score, p.outs_after.min(3));
        async move {
            if !home_t.is_empty() {
                tx.send(Tick::Fair { ticker: home_t, fair_c: wp_h * 100.0,
                                     detail: detail.clone() }).await.ok();
            }
            if !away_t.is_empty() {
                tx.send(Tick::Fair { ticker: away_t, fair_c: (1.0 - wp_h) * 100.0,
                                     detail }).await.ok();
            }
        }
    };

    // Prime: current state -> initial fair values, no replay of old plays.
    for g in &games {
        if let Ok(plays) = mlb.gumbo_plays_slim(g.0).await {
            let max_idx = plays.iter().map(|p| p.at_bat_index).max().unwrap_or(-1);
            seen.insert(g.0, max_idx);
            if let Some(p) = plays.last() {
                send_fair(p, g, tx.clone()).await;
            }
        }
    }
    tracing::info!(n_games = games.len(), "gumbo primed; pricing every new play");
    loop {
        let polls = games.iter()
            .filter(|g| !done.get(&g.0).copied().unwrap_or(false))
            .map(|g| mlb.gumbo_plays_status(g.0));
        let live: Vec<&Game> = games.iter()
            .filter(|g| !done.get(&g.0).copied().unwrap_or(false)).collect();
        if live.is_empty() {
            tracing::info!("all games final; ending day");
            tx.send(Tick::AllFinal).await.ok();
            return Ok(());
        }
        let results = futures_util::future::join_all(polls).await;
        for (g, res) in live.iter().zip(results) {
            let (plays, status) = match res {
                Ok(r) => r,
                Err(e) => { tracing::warn!(pk = g.0, %e, "gumbo poll failed"); continue; }
            };
            let last = seen.entry(g.0).or_insert(-1);
            if let Some(p) = plays.iter().filter(|p| p.at_bat_index > *last).last() {
                *last = p.at_bat_index;
                send_fair(p, g, tx.clone()).await;
            }
            if status == "Final" {
                done.insert(g.0, true);
                if let Some(p) = plays.last() {
                    let home_won = p.home_score > p.away_score;
                    for (t, won) in [(&g.4, home_won), (&g.3, !home_won)] {
                        if !t.is_empty() {
                            tx.send(Tick::Settle { ticker: t.clone(), won }).await.ok();
                        }
                    }
                    tracing::info!(pk = g.0, home_won, "game final -> settle");
                }
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
    let kalshi = KalshiClient::new(8)?;
    let mlb = MlbClient::new()?;
    let ratings = common::wp::Ratings::from_csv(&cfg.ratings_path)
        .context("loading ratings.csv (regenerate with wp_model.py)")?;

    // Self-rolling day loop: trade a slate until every game is final, then
    // rediscover. Off days (e.g. the All-Star break) just retry until the
    // next slate appears — no external daily restart needed.
    loop {
        match run_day(&cfg, &signer, &kalshi, &mlb, &ratings).await {
            Ok(true) => {
                // Kalshi keeps settled markets "open" for a few minutes after
                // the last out; rolling instantly re-discovers the finished
                // slate in a tight loop. The next slate is hours away — cool
                // off before looking again.
                tracing::info!("slate complete; next discovery in 30 min");
                tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
            }
            Ok(false) => {
                tracing::info!("no open MLB markets; retrying in 30 min");
                tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
            }
            Err(e) => {
                tracing::warn!(%e, "day loop error; retrying in 5 min");
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            }
        }
    }
}

/// One slate: discover -> trade -> return Ok(true) when all games settle.
/// Ok(false) = nothing to trade today (off day / slate already settled).
async fn run_day(
    cfg: &Config, signer: &Signer, kalshi: &KalshiClient, mlb: &MlbClient,
    ratings: &common::wp::Ratings,
) -> Result<bool> {
    // "Today" in US/Eastern terms: MLB slates run past midnight UTC, so anchor
    // the date 8h behind UTC (flips ~4 AM ET, after the last West-coast final).
    let today = (Utc::now() - ChronoDur::hours(8)).format("%Y-%m-%d").to_string();
    // Kalshi event tickers embed the date as e.g. "26JUL09" — used to keep the
    // join from matching a leftover open market from a previous day's game.
    let date_code = (Utc::now() - ChronoDur::hours(8)).format("%y%b%d")
        .to_string().to_uppercase();
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
            if let Some((date, away, home)) = common::parse_event_teams(et) {
                if date != date_code { continue; }
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
            games.push((*pk, away.clone(), home.clone(), away_ticker, home_ticker));
        }
    }
    tracing::info!(n_games = games.len(), n_markets = tickers.len(), "watching");
    if games.is_empty() {
        return Ok(false);
    }

    // ---- log file (archive any previous run so daily history accumulates)
    if let Ok(meta) = std::fs::metadata(&cfg.log_path) {
        if meta.len() > 0 {
            let stamp: chrono::DateTime<Utc> = meta.modified()?.into();
            let archived = format!("{}.{}.csv",
                cfg.log_path.trim_end_matches(".csv"), stamp.format("%Y-%m-%d_%H%M%S"));
            std::fs::rename(&cfg.log_path, &archived)?;
            tracing::info!(%archived, "archived previous trade log");
        }
    }
    let mut log = csv::Writer::from_path(&cfg.log_path)?;
    log.write_record([
        "ts", "ticker", "action", "price_cents", "size", "cost_cents",
        "fair_model_cents", "fair_blend_cents", "detail", "pnl_cents", "fees",
    ])?;
    log.flush()?;
    let mut adapt = Adapt::load("adapt_state.json", cfg.entry_edge_cents);
    tracing::info!(threshold = adapt.threshold, "adaptive entry threshold loaded");

    // ---- spin up tasks (aborted when the slate completes)
    let (tx, mut rx) = mpsc::channel::<Tick>(4096);
    let ws_handle = tokio::spawn({
        let tx = tx.clone();
        let tickers = tickers.clone();
        let signer = signer.clone();
        async move {
            loop {
                if let Err(e) = ws_task(signer.clone(), tickers.clone(), tx.clone()).await {
                    tracing::warn!(%e, "ws task died; reconnecting in 5s");
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    });
    // map each market to its opposite side so a game is never held both ways
    let other_side: HashMap<String, String> = games.iter()
        .flat_map(|g| [(g.3.clone(), g.4.clone()), (g.4.clone(), g.3.clone())])
        .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        .collect();
    let gumbo_handle = tokio::spawn(gumbo_task(games, ratings.clone(), tx.clone()));

    let mut books: HashMap<String, Book> = HashMap::new();
    let mut positions: HashMap<String, Pos> = HashMap::new();
    // feed-health telemetry: what the loop actually receives, logged each minute
    let (mut n_snap, mut n_delta, mut n_trade) = (0u64, 0u64, 0u64);
    let mut last_stats = Utc::now();

    while let Some(tick) = rx.recv().await {
        let now = Utc::now();
        if (now - last_stats).num_seconds() >= 60 {
            let sample = books.iter()
                .find_map(|(t, b)| Some((t, b.best_bid()?, b.best_ask()?)));
            tracing::info!(n_snap, n_delta, n_trade, ?sample, "feed stats (last 60s)");
            (n_snap, n_delta, n_trade) = (0, 0, 0);
            last_stats = now;
        }
        // which market to re-evaluate after applying this tick
        let touched: Option<String> = match tick {
            Tick::Snapshot(t, yes, no) => {
                n_snap += 1;
                books.entry(t.clone()).or_default().set_snapshot(&yes, &no);
                Some(t)
            }
            Tick::Delta(t, side, price, delta) => {
                n_delta += 1;
                books.entry(t.clone()).or_default().apply(&side, price, delta);
                Some(t)
            }
            Tick::Trade(t, price, count, _taker) => {
                n_trade += 1;
                // advance queues: sells print at/below our bid; buys at/above our ask
                if let Some(pos) = positions.get_mut(&t) {
                    for o in [pos.entry.as_mut(), pos.exit.as_mut()].into_iter().flatten() {
                        if o.state != OrderState::Resting { continue; }
                        let crosses = if o.is_buy { price <= o.price } else { price >= o.price };
                        if crosses {
                            o.first_print_at.get_or_insert(now);
                            o.filled_volume += count;
                            let through = if o.is_buy { price < o.price } else { price > o.price };
                            if through || o.filled_volume >= o.queue_ahead + o.size {
                                o.state = OrderState::Filled;
                            }
                        }
                    }
                }
                Some(t)
            }
            Tick::Fair { ticker, fair_c, detail } => {
                let pos = positions.entry(ticker.clone()).or_default();
                pos.fair_c = Some(fair_c);
                // detail leads with "inn{N} ..." — track the inning for the
                // late-game clip cap
                pos.inning = detail.strip_prefix("inn")
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(pos.inning);
                pos.detail = detail;
                pos.fair_seq += 1;
                Some(ticker)
            }
            Tick::Settle { ticker, won } => {
                if let Some(pos) = positions.get_mut(&ticker) {
                    let qty: f64 = pos.clips.iter().map(|c| c.1).sum();
                    if qty > 0.0 {
                        let px = if won { 100i64 } else { 0i64 };
                        let cost: f64 = pos.clips.iter()
                            .map(|(p, s)| *p as f64 * s).sum();
                        let pnl = px as f64 * qty - cost;
                        pos.realized += pnl;
                        adapt.record(pnl);
                        log.write_record([
                            now.to_rfc3339(), ticker.clone(), "settle".into(),
                            px.to_string(), format!("{qty}"), format!("{cost:.0}"),
                            format!("{:.1}", pos.fair_c.unwrap_or_default()),
                            String::new(),
                            pos.detail.clone(), format!("{pnl:.1}"), "0.00".into(),
                        ])?;
                        log.flush()?;
                        tracing::info!(%ticker, won, pnl_cents = pnl,
                                       invested_cents = cost,
                                       ret_pct = format!("{:+.0}", pnl / cost * 100.0),
                                       "settled at game end");
                    }
                    pos.clips.clear();
                    pos.entry = None;
                    pos.exit = None;
                    pos.fair_c = None; // no more trading this market
                }
                None
            }
            Tick::AllFinal => break,
        };

        // ---- evaluate the touched market: fills, then entry/exit decisions
        let Some(t) = touched else { continue };
        // one side per game: block entries while the opposite market has any
        // position or resting entry (holding both sides, then exiting one, is
        // how the Jul 17 SD/KC incoherence happened)
        let other_blocked = other_side.get(&t)
            .and_then(|o| positions.get(o))
            .is_some_and(|op| !op.clips.is_empty() || op.entry.is_some());
        let Some(pos) = positions.get_mut(&t) else { continue };

        let Some(book) = books.get(&t) else { continue };
        let (Some((bid, bid_sz)), Some((ask, ask_sz))) = (book.best_bid(), book.best_ask())
            else { continue };
        let fair_model = pos.fair_c;
        // Decisions use a model/market blend: the model only updates on
        // completed plays, so raw model-vs-book gaps can be the market knowing
        // something first. The blend (best Brier in backtest) halves phantom
        // edges while keeping real ones tradeable.
        let mid = (bid + ask) as f64 / 2.0;
        let fair_blend = fair_model.map(|f| BLEND_W * f + (1.0 - BLEND_W) * mid);

        // realize fills
        if pos.entry.as_ref().is_some_and(|o| o.state == OrderState::Filled) {
            let o = pos.entry.take().unwrap();
            pos.clips.push((o.price, o.size));
            pos.last_fill_seq = pos.fair_seq;
            let clip_cost = o.price as f64 * o.size;
            let pos_cost: f64 = pos.clips.iter().map(|(p, s)| *p as f64 * s).sum();
            log.write_record([
                now.to_rfc3339(), t.clone(),
                if pos.clips.len() > 1 { "add".into() } else { "entry".to_string() },
                o.price.to_string(), format!("{}", o.size), format!("{clip_cost:.0}"),
                format!("{:.1}", fair_model.unwrap_or_default()),
                format!("{:.1}", fair_blend.unwrap_or_default()),
                pos.detail.clone(), String::new(), "0.00".into(),
            ])?;
            log.flush()?;
            tracing::info!(ticker = %t, price = o.price, clips = pos.clips.len(),
                           invested_cents = pos_cost,
                           fair = fair_model.unwrap_or_default(), "entry filled");
        }
        if pos.exit.as_ref().is_some_and(|o| o.state == OrderState::Filled) {
            let o = pos.exit.take().unwrap();
            let cost: f64 = pos.clips.iter().map(|(p, s)| *p as f64 * s).sum();
            let qty: f64 = pos.clips.iter().map(|c| c.1).sum();
            let pnl = o.price as f64 * qty - cost;
            pos.clips.clear();
            pos.realized += pnl;
            adapt.record(pnl);
            log.write_record([
                now.to_rfc3339(), t.clone(), "exit_maker".into(),
                o.price.to_string(), format!("{qty}"), format!("{cost:.0}"),
                format!("{:.1}", fair_model.unwrap_or_default()),
                format!("{:.1}", fair_blend.unwrap_or_default()),
                pos.detail.clone(), format!("{pnl:.1}"), "0.00".into(),
            ])?;
            log.flush()?;
            tracing::info!(ticker = %t, exit = o.price, pnl_cents = pnl,
                           invested_cents = cost,
                           ret_pct = format!("{:+.0}", pnl / cost.max(1.0) * 100.0),
                           "position closed (maker)");
        }

        let (Some(_), Some(fair)) = (fair_model, fair_blend) else { continue };
        let qty: f64 = pos.clips.iter().map(|c| c.1).sum();

        // cancel stale resting orders when the blended view has moved
        if pos.entry.as_ref().is_some_and(|o| fair - o.price as f64 <= cfg.exit_edge_cents) {
            pos.entry = None;
        }
        if pos.exit.as_ref().is_some_and(|o| (o.price as f64) < fair) {
            pos.exit = None;
        }

        // entry / averaging down, gated on:
        //  - blended edge >= adaptive threshold
        //  - adds only after a NEW play re-confirmed the edge (fair_seq moved)
        //  - per-market daily loss stop
        let fresh_confirmation = pos.clips.is_empty() || pos.fair_seq > pos.last_fill_seq;
        // from inning 7 on, cap the market at a single clip: late-game edges
        // backtest best but settle binary — bound the tail
        let late_capped = pos.inning >= 7 && !pos.clips.is_empty();
        if pos.entry.is_none() && pos.exit.is_none()
            && !other_blocked
            && !late_capped
            && pos.clips.len() < cfg.max_clips
            && fresh_confirmation
            && pos.realized > MARKET_LOSS_STOP_C
            && fair - bid as f64 >= adapt.threshold
            && ask - bid <= cfg.max_spread_cents
            && bid >= 5 && ask <= 95
        {
            tracing::info!(ticker = %t, bid, ask, fair = format!("{fair:.1}"),
                           model = format!("{:.1}", fair_model.unwrap_or_default()),
                           thr = adapt.threshold, detail = %pos.detail,
                           clips = pos.clips.len(), "EDGE -> join bid");
            pos.entry = Some(SimOrder {
                ticker: t.clone(), is_buy: true, price: bid, size: cfg.size,
                queue_ahead: bid_sz, filled_volume: 0.0, placed_at: now,
                first_print_at: None, state: OrderState::Resting,
            });
        }

        if qty > 0.0 {
            // taker dump: blended view flipped hard against us; pay the fee
            let fee = taker_fee(qty, bid);
            if (bid as f64 - fair) - fee * 100.0 / qty.max(1.0) >= adapt.threshold {
                let cost: f64 = pos.clips.iter().map(|(p, s)| *p as f64 * s).sum();
                let pnl = bid as f64 * qty - cost - fee * 100.0;
                pos.clips.clear();
                pos.exit = None;
                pos.realized += pnl;
                adapt.record(pnl);
                log.write_record([
                    now.to_rfc3339(), t.clone(), "exit_taker".into(),
                    bid.to_string(), format!("{qty}"), format!("{cost:.0}"),
                    format!("{:.1}", fair_model.unwrap_or_default()),
                    format!("{fair:.1}"), pos.detail.clone(),
                    format!("{pnl:.1}"), format!("{fee:.2}"),
                ])?;
                log.flush()?;
                tracing::info!(ticker = %t, exit = bid, pnl_cents = pnl,
                               invested_cents = cost,
                               ret_pct = format!("{:+.0}", pnl / cost.max(1.0) * 100.0),
                               "position dumped (taker, model flipped)");
            } else if pos.exit.is_none() && ask as f64 >= fair + cfg.exit_edge_cents {
                // maker exit: the ask is now above blended fair -> sell into it
                pos.exit = Some(SimOrder {
                    ticker: t.clone(), is_buy: false, price: ask, size: qty,
                    queue_ahead: ask_sz, filled_volume: 0.0, placed_at: now,
                    first_print_at: None, state: OrderState::Resting,
                });
            }
        }
    }
    ws_handle.abort();
    gumbo_handle.abort();
    Ok(true)
}

/// Per-market strategy state.
#[derive(Default)]
struct Pos {
    /// latest model fair value (cents); None once settled
    fair_c: Option<f64>,
    detail: String,
    /// filled clips: (price cents, contracts)
    clips: Vec<(i64, f64)>,
    entry: Option<SimOrder>,
    exit: Option<SimOrder>,
    /// current inning per the last Fair tick (for the late-game clip cap)
    inning: i64,
    /// bumped on every Fair tick (new completed play)
    fair_seq: u64,
    /// fair_seq at the last clip fill; adds require a newer play to confirm
    last_fill_seq: u64,
    /// realized P&L today (cents) — entries stop at MARKET_LOSS_STOP_C
    realized: f64,
}

/// Self-tuning entry threshold: tightens after losses, relaxes after wins.
struct Adapt {
    threshold: f64,
    recent: std::collections::VecDeque<f64>,
    path: String,
}

impl Adapt {
    fn load(path: &str, default_thr: f64) -> Self {
        let threshold = std::fs::read_to_string(path).ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v["threshold"].as_f64())
            .unwrap_or(default_thr)
            .clamp(THR_MIN, THR_MAX);
        Self { threshold, recent: Default::default(), path: path.into() }
    }
    fn record(&mut self, pnl_cents: f64) {
        self.recent.push_back(pnl_cents);
        if self.recent.len() > THR_WINDOW { self.recent.pop_front(); }
        if self.recent.len() == THR_WINDOW {
            let net: f64 = self.recent.iter().sum();
            let old = self.threshold;
            self.threshold = (self.threshold + if net < 0.0 { 1.0 } else { -1.0 })
                .clamp(THR_MIN, THR_MAX);
            if (self.threshold - old).abs() > f64::EPSILON {
                tracing::info!(net_last5 = net, threshold = self.threshold,
                               "adaptive entry threshold updated");
                std::fs::write(&self.path, format!(
                    "{{\"threshold\": {:.1}}}", self.threshold)).ok();
            }
        }
    }
}
