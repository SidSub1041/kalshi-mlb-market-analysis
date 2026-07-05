//! Full-season Kalshi MLB data collector.
//!
//! For every settled KXMLBGAME event in [--start-date, --end-date]:
//!   * winner-side moneyline market + highest-volume KXMLBTOTAL market
//!   * 1-minute candlesticks (game start - 30 min -> market close)
//!   * full raw trade tape
//!   * GUMBO play-by-play with pitch-level decisive timestamps + base-out state
//!
//! Output (CSV, resumable — existing files are skipped):
//!   out/games.csv                      manifest
//!   out/candles_{EVENT}_{ML|TOT}.csv  ts,price_close,price_mean,volume,ask_close,bid_close
//!   out/trades_{EVENT}_{ML|TOT}.csv   ts,yes_price,count,taker_side,taker_book_side
//!   out/plays_{EVENT}.csv             at_bat_index,end_time,decisive_time,inning,half,
//!                                     event_type,event,rbi,away_score,home_score,
//!                                     is_scoring,outs_after,on1,on2,on3
//!
//! Run: cargo run --release -p collector -- \
//!        --start-date 2026-03-25 --end-date 2026-07-02 --out data_full
//! Expect ~1-2 h for a full season at 8 req/s (candles + trade pages dominate).

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDur, NaiveDate, Utc};
use clap::Parser;
use common::{dollars, parse_event_teams, team_aliases, KalshiClient, Market, MlbClient};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Parser)]
struct Args {
    /// first game date (YYYY-MM-DD)
    #[arg(long)]
    start_date: NaiveDate,
    /// last game date inclusive (YYYY-MM-DD)
    #[arg(long)]
    end_date: NaiveDate,
    #[arg(long, default_value = "data_full")]
    out: PathBuf,
    /// Kalshi read requests per second (basic tier limit is ~10)
    #[arg(long, default_value_t = 8)]
    rps: u32,
    /// skip the raw trade tape (much faster; candles only)
    #[arg(long, default_value_t = false)]
    no_trades: bool,
}

fn iso_epoch(s: &str) -> i64 {
    DateTime::parse_from_rfc3339(s).map(|d| d.timestamp()).unwrap_or(0)
}

fn write_candles(path: &Path, candles: &[common::Candle]) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    w.write_record(["ts", "price_close", "price_mean", "volume", "ask_close", "bid_close"])?;
    let mut prev_close: Option<f64> = None;
    for c in candles {
        let close = dollars(&c.price.close_dollars)
            .or(dollars(&c.price.previous_dollars))
            .or(prev_close);
        let mean = dollars(&c.price.mean_dollars).or(close);
        let (Some(close), Some(mean)) = (close, mean) else { continue };
        prev_close = Some(close);
        w.write_record([
            c.end_period_ts.to_string(),
            format!("{close:.4}"),
            format!("{mean:.4}"),
            c.volume_fp.clone().unwrap_or_else(|| "0".into()),
            dollars(&c.yes_ask.close_dollars).map(|v| format!("{v:.4}")).unwrap_or_default(),
            dollars(&c.yes_bid.close_dollars).map(|v| format!("{v:.4}")).unwrap_or_default(),
        ])?;
    }
    w.flush()?;
    Ok(())
}

fn write_trades(path: &Path, trades: &[common::Trade]) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    w.write_record(["ts", "yes_price", "count", "taker_side", "taker_book_side"])?;
    for t in trades {
        w.write_record([
            iso_epoch(&t.created_time).to_string(),
            t.yes_price_dollars.clone(),
            t.count_fp.clone(),
            t.taker_side.clone(),
            t.taker_book_side.clone(),
        ])?;
    }
    w.flush()?;
    Ok(())
}

fn write_plays(path: &Path, plays: &[common::PlayRow]) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    w.write_record([
        "at_bat_index", "end_time", "decisive_time", "inning", "half", "event_type",
        "event", "rbi", "away_score", "home_score", "is_scoring", "outs_after",
        "on1", "on2", "on3",
    ])?;
    for p in plays {
        w.write_record([
            p.at_bat_index.to_string(), p.end_time.clone(), p.decisive_time.clone(),
            p.inning.to_string(), p.half.clone(), p.event_type.clone(), p.event.clone(),
            p.rbi.to_string(), p.away_score.to_string(), p.home_score.to_string(),
            p.is_scoring.to_string(), p.outs_after.to_string(),
            p.on1_after.to_string(), p.on2_after.to_string(), p.on3_after.to_string(),
        ])?;
    }
    w.flush()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;

    let kalshi = KalshiClient::new(args.rps)?;
    let mlb = MlbClient::new()?;

    // ---- 1. discover settled markets, chunked weekly to keep queries fast
    let mut ml_markets: Vec<Market> = Vec::new();
    let mut tot_markets: Vec<Market> = Vec::new();
    let mut d = args.start_date;
    while d <= args.end_date {
        let chunk_end = (d + ChronoDur::days(7)).min(args.end_date + ChronoDur::days(1));
        // market close = game end; pad by a day around the date window
        let min_ts = d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
        let max_ts = chunk_end.and_hms_opt(23, 59, 59).unwrap().and_utc().timestamp();
        ml_markets.extend(kalshi.settled_markets("KXMLBGAME", min_ts, max_ts).await?);
        tot_markets.extend(kalshi.settled_markets("KXMLBTOTAL", min_ts, max_ts).await?);
        tracing::info!(week = %d, ml = ml_markets.len(), tot = tot_markets.len(), "discovery");
        d = chunk_end;
    }
    ml_markets.dedup_by(|a, b| a.ticker == b.ticker);
    tot_markets.dedup_by(|a, b| a.ticker == b.ticker);

    // group by event; keep the winner-side ML market (result == "yes"),
    // and the highest-volume totals market per event
    let mut ml_by_event: HashMap<String, Market> = HashMap::new();
    for m in ml_markets {
        if m.result == "yes" {
            ml_by_event.insert(m.event_ticker.clone(), m);
        }
    }
    let mut tot_by_game: HashMap<String, Market> = HashMap::new();
    for m in tot_markets {
        // KXMLBTOTAL event tickers embed the same date/time/teams tail
        let key = m.event_ticker
            .rsplit('-').next().unwrap_or_default().to_string();
        let e = tot_by_game.entry(key).or_insert_with(|| m.clone());
        if m.volume_f64() > e.volume_f64() {
            *e = m;
        }
    }

    // ---- 2. MLB schedule cache: date -> games
    let mut sched: HashMap<String, Vec<(i64, String, String, String)>> = HashMap::new();

    // ---- 3. per game
    let mut manifest = csv::Writer::from_path(args.out.join("games.csv"))?;
    manifest.write_record([
        "event_code", "away", "home", "game_pk", "ml_ticker", "tot_ticker",
        "tot_strike", "game_start_utc", "ml_close_utc",
    ])?;

    let n = ml_by_event.len();
    for (i, (event_ticker, ml)) in ml_by_event.iter().enumerate() {
        let Some((date7, away, home)) = parse_event_teams(event_ticker) else {
            tracing::warn!(%event_ticker, "cannot parse teams; skipping");
            continue;
        };
        let tail = event_ticker.rsplit('-').next().unwrap_or_default().to_string();
        let event_code = format!("{away}{home}_{date7}");

        // MLB date: "26JUN30" -> 2026-06-30
        let date = NaiveDate::parse_from_str(&format!("20{date7}"), "%Y%b%d")
            .with_context(|| format!("bad date in {event_ticker}"))?
            .format("%Y-%m-%d").to_string();
        if !sched.contains_key(&date) {
            sched.insert(date.clone(), mlb.schedule(&date).await?);
        }
        let games = &sched[&date];
        let game = games.iter().find(|(_, a, h, _)| {
            team_aliases(&away).contains(&a.as_str()) && team_aliases(&home).contains(&h.as_str())
        });
        let Some((game_pk, _, _, game_start)) = game else {
            tracing::warn!(%event_ticker, %date, "no MLB schedule match; skipping");
            continue;
        };

        let tot = tot_by_game.get(&tail);
        let start_ts = iso_epoch(game_start) - 1800;
        let close_ts = iso_epoch(&ml.close_time) + 60;

        // candles + trades + plays (skip files that already exist -> resumable)
        let jobs: Vec<(&str, &str, String)> = std::iter::once(("KXMLBGAME", "ML", ml.ticker.clone()))
            .chain(tot.map(|t| ("KXMLBTOTAL", "TOT", t.ticker.clone())))
            .collect();
        for (series, kind, ticker) in &jobs {
            let cpath = args.out.join(format!("candles_{event_code}_{kind}.csv"));
            if !cpath.exists() {
                match kalshi.candles_1m(series, ticker, start_ts, close_ts).await {
                    Ok(c) if !c.is_empty() => write_candles(&cpath, &c)?,
                    Ok(_) => tracing::warn!(%ticker, "no candles"),
                    Err(e) => tracing::error!(%ticker, %e, "candles failed"),
                }
            }
            if !args.no_trades {
                let tpath = args.out.join(format!("trades_{event_code}_{kind}.csv"));
                if !tpath.exists() {
                    match kalshi.all_trades(ticker).await {
                        Ok(t) if !t.is_empty() => write_trades(&tpath, &t)?,
                        Ok(_) => {}
                        Err(e) => tracing::error!(%ticker, %e, "trades failed"),
                    }
                }
            }
        }
        let ppath = args.out.join(format!("plays_{event_code}.csv"));
        if !ppath.exists() {
            match mlb.gumbo_plays(*game_pk).await {
                Ok(p) if !p.is_empty() => write_plays(&ppath, &p)?,
                Ok(_) => tracing::warn!(game_pk, "no plays"),
                Err(e) => tracing::error!(game_pk, %e, "gumbo failed"),
            }
        }

        manifest.write_record([
            event_code.clone(), away.clone(), home.clone(), game_pk.to_string(),
            ml.ticker.clone(),
            tot.map(|t| t.ticker.clone()).unwrap_or_default(),
            tot.and_then(|t| t.cap_strike.or(t.floor_strike))
                .map(|s| s.to_string()).unwrap_or_default(),
            game_start.clone(), ml.close_time.clone(),
        ])?;
        manifest.flush()?;
        if (i + 1) % 25 == 0 {
            tracing::info!("{}/{} games done", i + 1, n);
        }
    }
    tracing::info!("collection complete -> {}", args.out.display());
    Ok(())
}
