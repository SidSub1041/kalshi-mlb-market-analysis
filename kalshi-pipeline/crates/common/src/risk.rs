//! Pre-trade risk checks for live execution.
//!
//! This module is deliberately PURE: it contains no networking and no
//! order-transmission code. A live executor must call [`RiskBook::approve`]
//! before every order it sends and honor every veto; the lockout / kill-file
//! helpers make "stop trading" decisions durable across restarts, so a
//! supervisor (launchd KeepAlive) can never resurrect the bot into a day it
//! already lost.
//!
//! Design rule: there is no bypass. If a cap is wrong, change the number and
//! rebuild — friction here is the feature.

use chrono::{DateTime, NaiveDate, Utc};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// Hard limits. Defaults are the Phase-6 starting values from the cutover
/// plan; loosen them only by editing config, never at runtime.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Total cost basis allowed across all open positions, in cents.
    pub max_exposure_cents: i64,
    /// Cost basis allowed in any single market, in cents.
    pub per_market_cap_cents: i64,
    /// Realized daily loss (cents) beyond which trading locks out for the day.
    pub daily_loss_stop_cents: i64,
    /// Order-placement rate cap.
    pub max_orders_per_min: usize,
    /// Reject orders priced further than this from the current mid.
    pub max_price_distance_cents: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_exposure_cents: 2_500,  // $25 total at risk
            per_market_cap_cents: 500,  // $5 per market
            daily_loss_stop_cents: 500, // -$5/day -> lockout
            max_orders_per_min: 10,
            max_price_distance_cents: 5,
        }
    }
}

/// One order the executor wants to send, reduced to what risk cares about.
#[derive(Debug, Clone)]
pub struct OrderCheck<'a> {
    pub ticker: &'a str,
    /// Limit price in cents (yes-side convention, 1-99).
    pub price_cents: i64,
    /// Contracts.
    pub count: i64,
    /// Current market mid in cents, if a fresh book exists. `None` is treated
    /// as "no trustworthy price" and vetoes the order.
    pub mid_cents: Option<f64>,
    /// True only for an exchange-enforced reduce-only exit. It is still
    /// subject to fresh-price and rate checks, but never reserves additional
    /// exposure or gets blocked by an entry cap.
    pub reduce_only: bool,
}

/// Why an order was refused. The executor logs the veto and moves on; it
/// never retries around one.
#[derive(Debug, Clone, PartialEq)]
pub enum Veto {
    ExposureCap {
        open_cents: i64,
        order_cents: i64,
        cap: i64,
    },
    MarketCap {
        ticker: String,
        open_cents: i64,
        order_cents: i64,
        cap: i64,
    },
    DailyLoss {
        realized_cents: i64,
        stop: i64,
    },
    OrderRate {
        in_window: usize,
        cap: usize,
    },
    PriceDistance {
        price: i64,
        mid: f64,
        cap: i64,
    },
    NoMid,
    PriceBounds {
        price: i64,
    },
    BadCount {
        count: i64,
    },
}

impl std::fmt::Display for Veto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Veto::ExposureCap {
                open_cents,
                order_cents,
                cap,
            } => write!(
                f,
                "total exposure cap: open {open_cents}c + order {order_cents}c > {cap}c"
            ),
            Veto::MarketCap {
                ticker,
                open_cents,
                order_cents,
                cap,
            } => {
                write!(
                f, "per-market cap on {ticker}: open {open_cents}c + order {order_cents}c > {cap}c")
            }
            Veto::DailyLoss {
                realized_cents,
                stop,
            } => write!(f, "daily loss stop: realized {realized_cents}c <= -{stop}c"),
            Veto::OrderRate { in_window, cap } => {
                write!(f, "order rate: {in_window} in last 60s >= cap {cap}")
            }
            Veto::PriceDistance { price, mid, cap } => write!(
                f,
                "price sanity: {price}c is more than {cap}c from mid {mid:.1}c"
            ),
            Veto::NoMid => write!(f, "no trustworthy mid for market"),
            Veto::PriceBounds { price } => write!(f, "price {price}c outside 1-99"),
            Veto::BadCount { count } => write!(f, "count {count} not a positive integer"),
        }
    }
}

/// Mutable risk state for one trading day.
#[derive(Debug, Default)]
pub struct RiskBook {
    pub limits: Limits,
    /// Open cost basis per market, cents.
    exposure: HashMap<String, i64>,
    /// Realized P&L today, cents (negative = loss).
    realized_today: i64,
    /// Timestamps of recently sent orders (for the rate cap).
    sent: VecDeque<DateTime<Utc>>,
    /// Approved-but-unfilled cost basis per market, cents. Reserved at
    /// approve() time; released on fill (converted) or cancel/reject.
    pending: HashMap<String, i64>,
}

impl RiskBook {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            ..Default::default()
        }
    }

    /// Approve or veto an order the executor wants to send. On approval the
    /// order is counted against the rate window immediately (call it once per
    /// real send attempt).
    pub fn approve(&mut self, now: DateTime<Utc>, chk: &OrderCheck) -> Result<(), Veto> {
        if chk.price_cents < 1 || chk.price_cents > 99 {
            return Err(Veto::PriceBounds {
                price: chk.price_cents,
            });
        }
        if chk.count <= 0 {
            return Err(Veto::BadCount { count: chk.count });
        }
        if !chk.reduce_only && self.realized_today <= -self.limits.daily_loss_stop_cents {
            return Err(Veto::DailyLoss {
                realized_cents: self.realized_today,
                stop: self.limits.daily_loss_stop_cents,
            });
        }
        // NaN/inf mids are as untrustworthy as no mid at all.
        let Some(mid) = chk.mid_cents.filter(|m| m.is_finite()) else {
            return Err(Veto::NoMid);
        };
        if (chk.price_cents as f64 - mid).abs() > self.limits.max_price_distance_cents as f64 {
            return Err(Veto::PriceDistance {
                price: chk.price_cents,
                mid,
                cap: self.limits.max_price_distance_cents,
            });
        }
        // Caps count filled AND in-flight (approved-but-unfilled) exposure.
        // Without the pending reservation, a burst of approvals could each
        // pass individually and blow through the caps once they all fill.
        let order_cents = chk.price_cents * chk.count;
        if chk.reduce_only {
            let cutoff = now - chrono::Duration::seconds(60);
            while self.sent.front().is_some_and(|t| *t < cutoff) {
                self.sent.pop_front();
            }
            if self.sent.len() >= self.limits.max_orders_per_min {
                return Err(Veto::OrderRate {
                    in_window: self.sent.len(),
                    cap: self.limits.max_orders_per_min,
                });
            }
            self.sent.push_back(now);
            return Ok(());
        }
        let market_open = *self.exposure.get(chk.ticker).unwrap_or(&0)
            + *self.pending.get(chk.ticker).unwrap_or(&0);
        if market_open + order_cents > self.limits.per_market_cap_cents {
            return Err(Veto::MarketCap {
                ticker: chk.ticker.to_string(),
                open_cents: market_open,
                order_cents,
                cap: self.limits.per_market_cap_cents,
            });
        }
        let total_open: i64 =
            self.exposure.values().sum::<i64>() + self.pending.values().sum::<i64>();
        if total_open + order_cents > self.limits.max_exposure_cents {
            return Err(Veto::ExposureCap {
                open_cents: total_open,
                order_cents,
                cap: self.limits.max_exposure_cents,
            });
        }
        let cutoff = now - chrono::Duration::seconds(60);
        while self.sent.front().is_some_and(|t| *t < cutoff) {
            self.sent.pop_front();
        }
        if self.sent.len() >= self.limits.max_orders_per_min {
            return Err(Veto::OrderRate {
                in_window: self.sent.len(),
                cap: self.limits.max_orders_per_min,
            });
        }
        self.sent.push_back(now);
        *self.pending.entry(chk.ticker.to_string()).or_insert(0) += order_cents;
        Ok(())
    }

    /// The order approved for `reserved_cents` was cancelled, rejected, or
    /// definitively failed to send: release its reservation.
    pub fn release_pending(&mut self, ticker: &str, reserved_cents: i64) {
        if let Some(p) = self.pending.get_mut(ticker) {
            *p = (*p - reserved_cents).max(0);
            if *p == 0 {
                self.pending.remove(ticker);
            }
        }
    }

    /// An entry fill converted reservation into real exposure.
    pub fn on_fill_open(&mut self, ticker: &str, cost_cents: i64) {
        self.release_pending(ticker, cost_cents);
        self.on_exposure_change(ticker, cost_cents);
    }

    /// A fill increased (positive cents) or decreased open cost basis.
    pub fn on_exposure_change(&mut self, ticker: &str, delta_cents: i64) {
        let e = self.exposure.entry(ticker.to_string()).or_insert(0);
        if *e + delta_cents < 0 {
            // Clamped in the conservative direction, but drift means the
            // executor's accounting and ours disagree — make it visible.
            tracing::warn!(
                ticker,
                open = *e,
                delta = delta_cents,
                "exposure over-decrement clamped to 0"
            );
        }
        *e = (*e + delta_cents).max(0);
        if *e == 0 {
            self.exposure.remove(ticker);
        }
    }

    /// A round trip / settlement realized P&L (negative = loss).
    pub fn on_realized(&mut self, pnl_cents: i64) {
        self.realized_today += pnl_cents;
    }

    pub fn realized_today(&self) -> i64 {
        self.realized_today
    }
    pub fn daily_loss_hit(&self) -> bool {
        self.realized_today <= -self.limits.daily_loss_stop_cents
    }
    pub fn open_exposure_cents(&self) -> i64 {
        self.exposure.values().sum()
    }
}

// ---------------------------------------------------------------- lockout

/// Path of the lockout marker for `date` inside `dir`.
pub fn lockout_path(dir: &Path, date: NaiveDate) -> PathBuf {
    dir.join(format!("LOCKOUT-{date}"))
}

/// Write today's lockout marker. The executor calls this when the daily loss
/// stop fires, right before flattening and exiting.
pub fn write_lockout(dir: &Path, now: DateTime<Utc>, reason: &str) -> std::io::Result<PathBuf> {
    let p = lockout_path(dir, trading_date(now));
    std::fs::write(&p, format!("{} {reason}\n", now.to_rfc3339()))?;
    Ok(p)
}

/// True when today's lockout marker exists. Check this at startup BEFORE
/// connecting anything, and refuse to run — this is what makes a KeepAlive
/// supervisor safe.
pub fn is_locked_out(dir: &Path, now: DateTime<Utc>) -> bool {
    lockout_path(dir, trading_date(now)).exists()
}

/// The trading "day" flips at a FIXED 08:00 UTC boundary (matching the slate
/// anchor used everywhere else in this codebase). That is 4 AM ET during
/// daylight time and 3 AM ET during standard time — the one-hour winter
/// drift is accepted deliberately to keep the anchor DST-free; it only
/// matters for games running past 3 AM EST (post-season territory).
pub fn trading_date(now: DateTime<Utc>) -> NaiveDate {
    (now - chrono::Duration::hours(8)).date_naive()
}

/// Manual kill switch: `touch KILL` in the run directory. The executor checks
/// every loop; on true it flattens and exits.
pub fn kill_requested(dir: &Path) -> bool {
    dir.join("KILL").exists()
}

// ---------------------------------------------------------------- dead man

/// Tracks data-feed liveness; when a feed has been silent too long, the
/// executor must cancel resting orders and stop quoting.
#[derive(Debug)]
pub struct DeadMan {
    max_age_s: i64,
    last: Option<DateTime<Utc>>,
}

impl DeadMan {
    pub fn new(max_age_s: i64) -> Self {
        Self {
            max_age_s,
            last: None,
        }
    }
    pub fn touch(&mut self, now: DateTime<Utc>) {
        self.last = Some(now);
    }
    /// Stale when never touched, or last touch is older than the limit.
    pub fn is_stale(&self, now: DateTime<Utc>) -> bool {
        match self.last {
            None => true,
            Some(t) => (now - t).num_seconds() > self.max_age_s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 18, 0, 0).unwrap()
    }
    fn chk<'a>(ticker: &'a str, price: i64, count: i64) -> OrderCheck<'a> {
        OrderCheck {
            ticker,
            price_cents: price,
            count,
            mid_cents: Some(price as f64),
            reduce_only: false,
        }
    }

    #[test]
    fn approves_within_all_caps() {
        let mut rb = RiskBook::new(Limits::default());
        assert!(rb.approve(t0(), &chk("A", 40, 10)).is_ok());
    }

    #[test]
    fn vetoes_per_market_cap() {
        let mut rb = RiskBook::new(Limits::default());
        rb.on_exposure_change("A", 450);
        let v = rb.approve(t0(), &chk("A", 40, 10)).unwrap_err();
        assert!(matches!(v, Veto::MarketCap { .. }));
    }

    #[test]
    fn vetoes_total_exposure() {
        let mut rb = RiskBook::new(Limits::default());
        for m in ["A", "B", "C", "D", "E"] {
            rb.on_exposure_change(m, 480);
        }
        let v = rb.approve(t0(), &chk("F", 40, 10)).unwrap_err();
        assert!(matches!(v, Veto::ExposureCap { .. }));
    }

    #[test]
    fn vetoes_after_daily_loss() {
        let mut rb = RiskBook::new(Limits::default());
        rb.on_realized(-500);
        let v = rb.approve(t0(), &chk("A", 40, 1)).unwrap_err();
        assert!(matches!(v, Veto::DailyLoss { .. }));
        assert!(rb.daily_loss_hit());
    }

    #[test]
    fn vetoes_order_rate() {
        let mut rb = RiskBook::new(Limits::default());
        for i in 0..10 {
            assert!(rb
                .approve(t0() + chrono::Duration::seconds(i), &chk("A", 10, 1))
                .is_ok());
        }
        let v = rb
            .approve(t0() + chrono::Duration::seconds(11), &chk("A", 10, 1))
            .unwrap_err();
        assert!(matches!(v, Veto::OrderRate { .. }));
        // window slides: a minute later it approves again
        assert!(rb
            .approve(t0() + chrono::Duration::seconds(130), &chk("A", 10, 1))
            .is_ok());
    }

    #[test]
    fn vetoes_price_distance_and_no_mid() {
        let mut rb = RiskBook::new(Limits::default());
        let mut c = chk("A", 40, 1);
        c.mid_cents = Some(50.0);
        assert!(matches!(
            rb.approve(t0(), &c).unwrap_err(),
            Veto::PriceDistance { .. }
        ));
        c.mid_cents = None;
        assert!(matches!(rb.approve(t0(), &c).unwrap_err(), Veto::NoMid));
    }

    #[test]
    fn lockout_roundtrip_and_day_anchor() {
        let dir = std::env::temp_dir().join(format!("risk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = t0();
        assert!(!is_locked_out(&dir, now));
        write_lockout(&dir, now, "daily loss stop").unwrap();
        assert!(is_locked_out(&dir, now));
        // 4 AM ET boundary: 07:59 UTC next day is still the SAME trading day
        let late = Utc.with_ymd_and_hms(2026, 9, 2, 7, 59, 0).unwrap();
        assert!(is_locked_out(&dir, late));
        // 08:01 UTC = next trading day -> lockout no longer applies
        let next = Utc.with_ymd_and_hms(2026, 9, 2, 8, 1, 0).unwrap();
        assert!(!is_locked_out(&dir, next));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kill_file() {
        let dir = std::env::temp_dir().join(format!("kill-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!kill_requested(&dir));
        std::fs::write(dir.join("KILL"), "").unwrap();
        assert!(kill_requested(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pending_reservation_blocks_burst_approvals() {
        let mut rb = RiskBook::new(Limits::default());
        // First 400c order approved and reserved.
        assert!(rb.approve(t0(), &chk("A", 40, 10)).is_ok());
        // Second identical order must now exceed the 500c per-market cap
        // even though nothing has filled yet.
        let v = rb.approve(t0(), &chk("A", 40, 10)).unwrap_err();
        assert!(matches!(v, Veto::MarketCap { .. }));
        // Cancel releases the reservation; approval works again.
        rb.release_pending("A", 400);
        assert!(rb.approve(t0(), &chk("A", 40, 10)).is_ok());
        // Fill converts reservation to real exposure — still capped.
        rb.on_fill_open("A", 400);
        assert!(matches!(
            rb.approve(t0(), &chk("A", 40, 10)).unwrap_err(),
            Veto::MarketCap { .. }
        ));
    }

    #[test]
    fn vetoes_nan_mid_and_bad_count() {
        let mut rb = RiskBook::new(Limits::default());
        let mut c = chk("A", 40, 1);
        c.mid_cents = Some(f64::NAN);
        assert!(matches!(rb.approve(t0(), &c).unwrap_err(), Veto::NoMid));
        assert!(matches!(
            rb.approve(t0(), &chk("A", 40, 0)).unwrap_err(),
            Veto::BadCount { .. }
        ));
        assert!(matches!(
            rb.approve(t0(), &chk("A", 40, -5)).unwrap_err(),
            Veto::BadCount { .. }
        ));
    }

    #[test]
    fn dead_man() {
        let mut dm = DeadMan::new(30);
        assert!(dm.is_stale(t0()));
        dm.touch(t0());
        assert!(!dm.is_stale(t0() + chrono::Duration::seconds(29)));
        assert!(dm.is_stale(t0() + chrono::Duration::seconds(31)));
    }

    #[test]
    fn allows_reduce_only_exit_after_loss_stop() {
        let mut rb = RiskBook::new(Limits::default());
        rb.on_realized(-500);
        let mut close = chk("A", 40, 1);
        close.reduce_only = true;
        assert!(rb.approve(t0(), &close).is_ok());
    }
}
