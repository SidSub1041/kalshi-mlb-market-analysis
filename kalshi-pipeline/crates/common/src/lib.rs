//! Shared client code: rate-limited Kalshi REST client, MLB Stats API client,
//! request signing for authenticated Kalshi endpoints (websocket).
//!
//! All Kalshi *market data* endpoints are public; signing is only needed for
//! the websocket feed and (later) order placement.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub const KALSHI_BASE: &str = "https://api.elections.kalshi.com/trade-api/v2";
pub const KALSHI_WS: &str = "wss://api.elections.kalshi.com/trade-api/ws/v2";
pub const MLB_BASE: &str = "https://statsapi.mlb.com";

// ---------------------------------------------------------------- rate limit

/// Simple token-interval limiter: guarantees >= `interval` between requests.
/// Kalshi basic tier allows ~10 read req/s; default to 8/s to stay clear.
pub struct RateLimiter {
    next: Mutex<Instant>,
    interval: Duration,
}

impl RateLimiter {
    pub fn per_second(n: u32) -> Arc<Self> {
        Arc::new(Self {
            next: Mutex::new(Instant::now()),
            interval: Duration::from_micros(1_000_000 / n as u64),
        })
    }
    pub async fn acquire(&self) {
        let mut next = self.next.lock().await;
        let now = Instant::now();
        if *next > now {
            tokio::time::sleep_until(*next).await;
        }
        *next = Instant::now().max(*next) + self.interval;
    }
}

// ---------------------------------------------------------------- kalshi types

#[derive(Debug, Clone, Deserialize)]
pub struct Market {
    pub ticker: String,
    pub event_ticker: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub yes_sub_title: String,
    pub open_time: String,
    pub close_time: String,
    #[serde(default)]
    pub result: String, // "yes" | "no" | "" while open
    /// Present on fp (fractional-contract) markets as decimal string.
    #[serde(default)]
    pub volume_fp: Option<String>,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub floor_strike: Option<f64>,
    #[serde(default)]
    pub cap_strike: Option<f64>,
    #[serde(default)]
    pub status: String,
}

impl Market {
    pub fn volume_f64(&self) -> f64 {
        self.volume_fp
            .as_deref()
            .and_then(|s| s.parse().ok())
            .or(self.volume)
            .unwrap_or(0.0)
    }
}

#[derive(Debug, Deserialize)]
struct MarketsResp {
    markets: Vec<Market>,
    #[serde(default)]
    cursor: String,
}

/// Candlestick price sub-object. Prices arrive as decimal-dollar strings
/// (e.g. "0.4900"); zero-volume candles omit close/mean and only carry
/// `previous_dollars`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CandlePrice {
    #[serde(default)]
    pub open_dollars: Option<String>,
    #[serde(default)]
    pub close_dollars: Option<String>,
    #[serde(default)]
    pub high_dollars: Option<String>,
    #[serde(default)]
    pub low_dollars: Option<String>,
    #[serde(default)]
    pub mean_dollars: Option<String>,
    #[serde(default)]
    pub previous_dollars: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Candle {
    pub end_period_ts: i64,
    #[serde(default)]
    pub price: CandlePrice,
    #[serde(default)]
    pub yes_bid: CandlePrice,
    #[serde(default)]
    pub yes_ask: CandlePrice,
    #[serde(default)]
    pub volume_fp: Option<String>,
    #[serde(default)]
    pub open_interest_fp: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CandlesResp {
    candlesticks: Vec<Candle>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Trade {
    pub trade_id: String,
    pub ticker: String,
    pub created_time: String,
    /// fractional contract count, decimal string e.g. "14.89"
    pub count_fp: String,
    pub yes_price_dollars: String,
    pub no_price_dollars: String,
    pub taker_side: String,      // "yes" | "no"
    #[serde(default)]
    pub taker_book_side: String, // "bid" | "ask"
}

#[derive(Debug, Deserialize)]
struct TradesResp {
    trades: Vec<Trade>,
    #[serde(default)]
    cursor: String,
}

pub fn dollars(s: &Option<String>) -> Option<f64> {
    s.as_deref().and_then(|v| v.parse().ok())
}

// ---------------------------------------------------------------- kalshi client

pub struct KalshiClient {
    http: reqwest::Client,
    limiter: Arc<RateLimiter>,
}

impl KalshiClient {
    pub fn new(reqs_per_sec: u32) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            limiter: RateLimiter::per_second(reqs_per_sec),
        })
    }

    /// GET with retry/backoff on 429 and 5xx.
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let mut delay = Duration::from_millis(500);
        for attempt in 0..6 {
            self.limiter.acquire().await;
            let resp = self.http.get(url).send().await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    return r.json::<T>().await.context("decoding json");
                }
                Ok(r) if r.status().as_u16() == 429 || r.status().is_server_error() => {
                    tracing::warn!(%url, status = %r.status(), attempt, "retrying");
                }
                Ok(r) => return Err(anyhow!("HTTP {} for {url}", r.status())),
                Err(e) if attempt < 5 => tracing::warn!(%url, %e, attempt, "retrying"),
                Err(e) => return Err(e.into()),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(15));
        }
        Err(anyhow!("exhausted retries for {url}"))
    }

    /// All settled markets for a series whose close time falls in [min_ts, max_ts].
    pub async fn settled_markets(
        &self,
        series: &str,
        min_close_ts: i64,
        max_close_ts: i64,
    ) -> Result<Vec<Market>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut url = format!(
                "{KALSHI_BASE}/markets?series_ticker={series}&status=settled\
                 &min_close_ts={min_close_ts}&max_close_ts={max_close_ts}&limit=200"
            );
            if !cursor.is_empty() {
                url.push_str(&format!("&cursor={cursor}"));
            }
            let resp: MarketsResp = self.get_json(&url).await?;
            out.extend(resp.markets);
            if resp.cursor.is_empty() {
                break;
            }
            cursor = resp.cursor;
        }
        Ok(out)
    }

    /// 1-minute candles in [start_ts, end_ts] (epoch secs). The API caps the
    /// number of periods per request, so chunk into <= 4000-minute windows.
    pub async fn candles_1m(
        &self,
        series: &str,
        ticker: &str,
        start_ts: i64,
        end_ts: i64,
    ) -> Result<Vec<Candle>> {
        let mut out: Vec<Candle> = Vec::new();
        let mut s = start_ts;
        while s < end_ts {
            let e = (s + 4000 * 60).min(end_ts);
            let url = format!(
                "{KALSHI_BASE}/series/{series}/markets/{ticker}/candlesticks\
                 ?start_ts={s}&end_ts={e}&period_interval=1"
            );
            let resp: CandlesResp = self.get_json(&url).await?;
            out.extend(resp.candlesticks);
            s = e;
        }
        out.sort_by_key(|c| c.end_period_ts);
        out.dedup_by_key(|c| c.end_period_ts);
        Ok(out)
    }

    /// Full trade tape for a market (cursor-paginated, newest first from API;
    /// returned here oldest-first).
    pub async fn all_trades(&self, ticker: &str) -> Result<Vec<Trade>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        loop {
            let mut url = format!("{KALSHI_BASE}/markets/trades?ticker={ticker}&limit=1000");
            if !cursor.is_empty() {
                url.push_str(&format!("&cursor={cursor}"));
            }
            let resp: TradesResp = self.get_json(&url).await?;
            if resp.trades.is_empty() {
                break;
            }
            out.extend(resp.trades);
            if resp.cursor.is_empty() {
                break;
            }
            cursor = resp.cursor;
        }
        out.reverse();
        Ok(out)
    }
}

// ---------------------------------------------------------------- auth (WS / orders)

/// Kalshi API-key auth: RSA-PSS(SHA256) signature over `{timestamp_ms}{METHOD}{path}`.
/// Returns (timestamp_ms, base64 signature) for the three KALSHI-ACCESS-* headers.
pub mod auth {
    use anyhow::{Context, Result};
    use base64::Engine;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::pss::SigningKey;
    use rsa::sha2::Sha256;
    use rsa::signature::{RandomizedSigner, SignatureEncoding};
    use rsa::RsaPrivateKey;

    #[derive(Clone)]
    pub struct Signer {
        pub key_id: String,
        key: RsaPrivateKey,
    }

    impl Signer {
        pub fn from_pem_file(key_id: &str, path: &str) -> Result<Self> {
            let pem = std::fs::read_to_string(path)
                .with_context(|| format!("reading private key {path}"))?;
            let key = RsaPrivateKey::from_pkcs8_pem(&pem)
                .context("parsing PKCS#8 PEM private key (export the key Kalshi gave you)")?;
            Ok(Self { key_id: key_id.to_string(), key })
        }

        /// `path` must include the API prefix, e.g. "/trade-api/ws/v2".
        pub fn headers(&self, method: &str, path: &str) -> Result<(String, String)> {
            let ts = chrono::Utc::now().timestamp_millis().to_string();
            let msg = format!("{ts}{method}{path}");
            let signing_key = SigningKey::<Sha256>::new(self.key.clone());
            let mut rng = rand_core::OsRng;
            let sig = signing_key.sign_with_rng(&mut rng, msg.as_bytes());
            Ok((ts, base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())))
        }
    }
}

// ---------------------------------------------------------------- MLB client

/// Team-code aliases: Kalshi ticker code -> acceptable MLB statsapi abbreviations.
/// statsapi abbreviations occasionally differ (AZ/ARI, CWS/CHW, WSH/WSN).
pub fn team_aliases(kalshi_code: &str) -> Vec<&'static str> {
    match kalshi_code {
        "AZ" => vec!["AZ", "ARI"],
        "CWS" => vec!["CWS", "CHW"],
        "WSH" => vec!["WSH", "WSN"],
        "ATH" => vec!["ATH", "OAK", "A's"],
        "SF" => vec!["SF", "SFG"],
        "SD" => vec!["SD", "SDP"],
        "TB" => vec!["TB", "TBR"],
        "KC" => vec!["KC", "KCR"],
        "CHC" => vec!["CHC"],
        "NYY" => vec!["NYY"],
        "NYM" => vec!["NYM"],
        "LAD" => vec!["LAD"],
        "LAA" => vec!["LAA"],
        "STL" => vec!["STL"],
        "ATL" => vec!["ATL"],
        "BOS" => vec!["BOS"],
        "BAL" => vec!["BAL"],
        "CIN" => vec!["CIN"],
        "MIL" => vec!["MIL"],
        "DET" => vec!["DET"],
        "TOR" => vec!["TOR"],
        "PIT" => vec!["PIT"],
        "PHI" => vec!["PHI"],
        "TEX" => vec!["TEX"],
        "CLE" => vec!["CLE"],
        "MIN" => vec!["MIN"],
        "HOU" => vec!["HOU"],
        "MIA" => vec!["MIA"],
        "COL" => vec!["COL"],
        "SEA" => vec!["SEA"],
        _ => vec![],
    }
}

/// Parse a KXMLBGAME event ticker like "KXMLBGAME-26JUN301905DETNYY"
/// into (date "26JUN30", away, home). Team codes are 2-3 uppercase chars;
/// the split is ambiguous only in theory — try all splits against known codes.
pub fn parse_event_teams(event_ticker: &str) -> Option<(String, String, String)> {
    let tail = event_ticker.rsplit('-').next()?; // "26JUN301905DETNYY"
    if tail.len() < 4 + 4 + 4 {
        return None;
    }
    let date = tail[..7].to_string(); // "26JUN30"
    let teams = &tail[11..]; // after 7-char date + 4-char time
    for split in 2..=3 {
        if teams.len() > split {
            let (a, h) = teams.split_at(split);
            if !team_aliases(a).is_empty() && !team_aliases(h).is_empty() {
                return Some((date, a.to_string(), h.to_string()));
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct PlayRow {
    pub at_bat_index: i64,
    pub inning: i64,
    pub half: String,
    pub event: String,
    pub event_type: String,
    pub rbi: i64,
    pub away_score: i64,
    pub home_score: i64,
    pub is_scoring: bool,
    /// official play end (lags the decisive moment by 30-60s on HRs)
    pub end_time: String,
    /// timestamp of the last pitch/action of the play — use this for alignment
    pub decisive_time: String,
    pub outs_after: i64,
    pub on1_after: bool,
    pub on2_after: bool,
    pub on3_after: bool,
}

pub struct MlbClient {
    http: reqwest::Client,
}

impl MlbClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?,
        })
    }

    async fn get(&self, url: &str) -> Result<serde_json::Value> {
        let mut delay = Duration::from_millis(500);
        for attempt in 0..5 {
            match self.http.get(url).send().await {
                Ok(r) if r.status().is_success() => return Ok(r.json().await?),
                Ok(r) => tracing::warn!(%url, status = %r.status(), attempt, "mlb retry"),
                Err(e) => tracing::warn!(%url, %e, attempt, "mlb retry"),
            }
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
        Err(anyhow!("exhausted retries for {url}"))
    }

    /// date "YYYY-MM-DD" -> vec of (gamePk, awayAbbrev, homeAbbrev, gameDateIso)
    pub async fn schedule(&self, date: &str) -> Result<Vec<(i64, String, String, String)>> {
        let url = format!(
            "{MLB_BASE}/api/v1/schedule?sportId=1&date={date}&hydrate=team"
        );
        let v = self.get(&url).await?;
        let mut out = Vec::new();
        for d in v["dates"].as_array().unwrap_or(&vec![]) {
            for g in d["games"].as_array().unwrap_or(&vec![]) {
                let pk = g["gamePk"].as_i64().unwrap_or(0);
                let away = g["teams"]["away"]["team"]["abbreviation"]
                    .as_str().unwrap_or("").to_string();
                let home = g["teams"]["home"]["team"]["abbreviation"]
                    .as_str().unwrap_or("").to_string();
                let dt = g["gameDate"].as_str().unwrap_or("").to_string();
                if pk > 0 {
                    out.push((pk, away, home, dt));
                }
            }
        }
        Ok(out)
    }

    /// GUMBO live feed -> per-play rows with pitch-level decisive timestamps.
    pub async fn gumbo_plays(&self, game_pk: i64) -> Result<Vec<PlayRow>> {
        let url = format!("{MLB_BASE}/api/v1.1/game/{game_pk}/feed/live");
        let v = self.get(&url).await?;
        Ok(Self::plays_from_gumbo(&v))
    }

    /// Same as `gumbo_plays` but with a fields filter so the response carries
    /// only what `plays_from_gumbo` reads (~50-100x smaller than the full
    /// feed). Use for latency-sensitive polling.
    pub async fn gumbo_plays_slim(&self, game_pk: i64) -> Result<Vec<PlayRow>> {
        let url = format!(
            "{MLB_BASE}/api/v1.1/game/{game_pk}/feed/live?fields=liveData,plays,allPlays,\
             about,atBatIndex,isComplete,endTime,halfInning,inning,isScoringPlay,\
             result,event,eventType,rbi,awayScore,homeScore,count,outs,\
             matchup,postOnFirst,postOnSecond,postOnThird,playEvents,startTime"
        );
        let v = self.get(&url).await?;
        Ok(Self::plays_from_gumbo(&v))
    }

    /// Plays plus the game's abstract state ("Live", "Final", "Preview").
    pub async fn gumbo_plays_status(&self, game_pk: i64) -> Result<(Vec<PlayRow>, String)> {
        let url = format!(
            "{MLB_BASE}/api/v1.1/game/{game_pk}/feed/live?fields=gameData,status,\
             abstractGameState,liveData,plays,allPlays,\
             about,atBatIndex,isComplete,endTime,halfInning,inning,isScoringPlay,\
             result,event,eventType,rbi,awayScore,homeScore,count,outs,\
             matchup,postOnFirst,postOnSecond,postOnThird,playEvents,startTime"
        );
        let v = self.get(&url).await?;
        let status = v["gameData"]["status"]["abstractGameState"]
            .as_str().unwrap_or("").to_string();
        Ok((Self::plays_from_gumbo(&v), status))
    }

    /// Pure function so it can be unit-tested on a fixture file.
    pub fn plays_from_gumbo(v: &serde_json::Value) -> Vec<PlayRow> {
        let mut out = Vec::new();
        let empty = vec![];
        for p in v["liveData"]["plays"]["allPlays"].as_array().unwrap_or(&empty) {
            if !p["about"]["isComplete"].as_bool().unwrap_or(false) {
                continue;
            }
            let end_time = p["about"]["endTime"].as_str().unwrap_or("").to_string();
            // decisive moment: endTime of the last play event (final pitch or
            // fielding action). Falls back to the official play endTime.
            let decisive_time = p["playEvents"]
                .as_array()
                .and_then(|evs| evs.last())
                .and_then(|e| e["endTime"].as_str().or_else(|| e["startTime"].as_str()))
                .unwrap_or(&end_time)
                .to_string();
            out.push(PlayRow {
                at_bat_index: p["about"]["atBatIndex"].as_i64().unwrap_or(-1),
                inning: p["about"]["inning"].as_i64().unwrap_or(0),
                half: p["about"]["halfInning"].as_str().unwrap_or("").to_string(),
                event: p["result"]["event"].as_str().unwrap_or("").to_string(),
                event_type: p["result"]["eventType"].as_str().unwrap_or("").to_string(),
                rbi: p["result"]["rbi"].as_i64().unwrap_or(0),
                away_score: p["result"]["awayScore"].as_i64().unwrap_or(0),
                home_score: p["result"]["homeScore"].as_i64().unwrap_or(0),
                is_scoring: p["about"]["isScoringPlay"].as_bool().unwrap_or(false),
                end_time,
                decisive_time,
                outs_after: p["count"]["outs"].as_i64().unwrap_or(0),
                on1_after: !p["matchup"]["postOnFirst"].is_null(),
                on2_after: !p["matchup"]["postOnSecond"].is_null(),
                on3_after: !p["matchup"]["postOnThird"].is_null(),
            });
        }
        out
    }
}

/// In-game win-probability model, ported from wp_model.py (backtested on the
/// 2026 season: Brier 0.157 at plays; model-vs-market gaps >=10c predicted
/// ~+3c/contract net of taker fees). Keep in sync with the Python reference.
pub mod wp {
    use super::PlayRow;
    use std::collections::HashMap;

    /// RE24 run-expectancy: (outs, on1, on2, on3) -> expected runs, rest of inning.
    const RE24: [((i64, bool, bool, bool), f64); 24] = [
        ((0,false,false,false),0.481),((0,true,false,false),0.859),
        ((0,false,true,false),1.100),((0,false,false,true),1.350),
        ((0,true,true,false),1.437),((0,true,false,true),1.784),
        ((0,false,true,true),1.964),((0,true,true,true),2.292),
        ((1,false,false,false),0.254),((1,true,false,false),0.509),
        ((1,false,true,false),0.664),((1,false,false,true),0.950),
        ((1,true,true,false),0.884),((1,true,false,true),1.130),
        ((1,false,true,true),1.376),((1,true,true,true),1.541),
        ((2,false,false,false),0.098),((2,true,false,false),0.224),
        ((2,false,true,false),0.319),((2,false,false,true),0.353),
        ((2,true,true,false),0.429),((2,true,false,true),0.478),
        ((2,false,true,true),0.580),((2,true,true,true),0.752),
    ];
    const LEAGUE_HALF_MU: f64 = 0.481;
    const HALF_VAR: f64 = 1.12;
    const HOME_EDGE: f64 = 0.035;
    /// Logistic calibration fit on the full 2026 season (wp_model.py).
    pub const CALIB_A: f64 = 0.0223;
    pub const CALIB_B: f64 = 1.8967;

    fn re24(outs: i64, on1: bool, on2: bool, on3: bool) -> f64 {
        RE24.iter().find(|(k, _)| *k == (outs, on1, on2, on3))
            .map(|(_, v)| *v).unwrap_or(LEAGUE_HALF_MU)
    }

    /// team -> (offense, defense) rating, 1.0 = league average.
    #[derive(Clone)]
    pub struct Ratings(HashMap<String, (f64, f64)>);

    impl Ratings {
        pub fn from_csv(path: &str) -> anyhow::Result<Self> {
            let mut m = HashMap::new();
            let mut rdr = csv::Reader::from_path(path)?;
            for rec in rdr.records() {
                let r = rec?;
                m.insert(r[0].to_string(),
                         (r[1].parse::<f64>()?, r[2].parse::<f64>()?));
            }
            Ok(Self(m))
        }
        pub fn get(&self, team: &str) -> (f64, f64) {
            self.0.get(team).copied().unwrap_or((1.0, 1.0))
        }
    }

    /// P(home team wins) given the state after `play`. Mirrors
    /// wp_model.py::wp_features + logistic calibration.
    pub fn wp_home(play: &PlayRow, ratings: &Ratings, away: &str, home: &str) -> f64 {
        let (o_h, d_h) = ratings.get(home);
        let (o_a, d_a) = ratings.get(away);
        let mu_h = LEAGUE_HALF_MU * o_h * d_a;
        let mu_a = LEAGUE_HALF_MU * o_a * d_h;

        let lead = (play.home_score - play.away_score) as f64;
        let (mut outs, mut on) =
            (play.outs_after, (play.on1_after, play.on2_after, play.on3_after));
        let mid_inning_done = outs >= 3;
        if mid_inning_done { outs = 0; on = (false, false, false); }

        let inn = play.inning.min(9);
        let (rem_a, rem_h, partial, partial_side): (f64, f64, f64, f64) =
            if play.half == "top" {
                let (ra, rh) = ((9 - inn) as f64, (9 - inn + 1) as f64);
                if mid_inning_done { (ra, rh, 0.0, 0.0) }
                else { (ra, rh, re24(outs, on.0, on.1, on.2) * (o_a * d_h), -1.0) }
            } else {
                let (ra, rh) = ((9 - inn) as f64, (9 - inn) as f64);
                if mid_inning_done { (ra, rh, 0.0, 0.0) }
                else { (ra, rh, re24(outs, on.0, on.1, on.2) * (o_h * d_a), 1.0) }
            };

        let exp_diff = lead + rem_h * mu_h - rem_a * mu_a
            + partial_side * partial + HOME_EDGE;
        let n_half = (rem_a + rem_h + if partial_side == 0.0 { 0.0 } else { 1.0 })
            .max(1.0);
        let z = exp_diff / (n_half * HALF_VAR).sqrt();
        1.0 / (1.0 + (-(CALIB_A + CALIB_B * z)).exp())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_teams() {
        let (d, a, h) = parse_event_teams("KXMLBGAME-26JUN301905DETNYY").unwrap();
        assert_eq!((d.as_str(), a.as_str(), h.as_str()), ("26JUN30", "DET", "NYY"));
        let (_, a, h) = parse_event_teams("KXMLBGAME-26JUN302140LAASEA").unwrap();
        assert_eq!((a.as_str(), h.as_str()), ("LAA", "SEA"));
        // 2-then-3 split: AZ vs SEA
        let (_, a, h) = parse_event_teams("KXMLBGAME-26MAY302210AZSEA").unwrap();
        assert_eq!((a.as_str(), h.as_str()), ("AZ", "SEA"));
    }

    #[test]
    fn wp_matches_python_reference() {
        let ratings = wp::Ratings::from_csv("/nonexistent").unwrap_or_else(|_| {
            // neutral ratings when file absent (get() defaults to 1.0)
            wp::Ratings::from_csv("/dev/null").unwrap()
        });
        let mk = |inning, half: &str, outs, on1, on2, on3, a, h| PlayRow {
            at_bat_index: 0, inning, half: half.into(), event: String::new(),
            event_type: String::new(), rbi: 0, away_score: a, home_score: h,
            is_scoring: false, end_time: String::new(), decisive_time: String::new(),
            outs_after: outs, on1_after: on1, on2_after: on2, on3_after: on3,
        };
        // reference values from wp_model.py with neutral ratings
        let cases = [
            (mk(1, "top", 0, false, false, false, 0, 0), 0.50927),
            (mk(9, "bottom", 2, false, false, false, 3, 5), 0.979063),
            (mk(7, "top", 1, true, true, false, 2, 4), 0.771426),
        ];
        for (play, want) in cases {
            let got = wp::wp_home(&play, &ratings, "AWY", "HOM");
            assert!((got - want).abs() < 1e-4, "got {got}, want {want}");
        }
    }

    #[test]
    fn parses_gumbo_fixture() {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/gumbo_fixture.json");
        if let Ok(s) = std::fs::read_to_string(fixture) {
            let v: serde_json::Value = serde_json::from_str(&s).unwrap();
            let plays = MlbClient::plays_from_gumbo(&v);
            assert!(!plays.is_empty());
            assert!(plays.iter().all(|p| !p.decisive_time.is_empty()));
        } // fixture optional; test is a no-op without it
    }
}
