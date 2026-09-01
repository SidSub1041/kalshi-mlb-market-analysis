# Kalshi MLB Market Analysis

Quantitative research and live paper-trading system for MLB moneyline markets
on Kalshi. Built across the 2026 season.

## What's here

- **`kalshi-pipeline/`** — Rust workspace:
  - `collector` — season-scale market data collection (candles, tick trades,
    play-by-play) via Kalshi REST and MLB GUMBO APIs; 882 games, 66k+ events.
  - `paper-trader` — autonomous live trading simulator: state-based
    win-probability fair values on every play, EV-gated maker entries with
    queue-accurate fill simulation, fee-aware exits, adaptive thresholds, and
    layered risk controls (single-clip sizing, one-side-per-game, salvage
    exits, per-market loss stops, feed-freshness guards).
  - `common` — shared clients, auth signing, and the win-probability engine
    (RE24 base-out run expectancy + season team ratings + logistic
    calibration; unit-tested against the Python reference).
- **`wp_model.py`** — research prototype and season backtest, including the
  survivorship-bias audit that invalidated the project's original edge and the
  fee-aware discrepancy analysis that gates deployment.
- **`tracker_server.py`** — live dashboard (period tabs, per-day performance
  attribution, open-position view) served on `localhost:8787`.
- **`build_workbook.py`** — Excel report generator.
- **`deploy/`** — launchd agents for fully autonomous daily operation, and
  `setup_live_clone.sh` to run them from an isolated clone (see
  `deploy/README.md`).

## Method notes

Paper trading only: the system simulates fills against live order books and
never places real orders. Every strategy change was gated on either the
882-game backtest or live trade-pattern analysis, and several proposed edges
were rejected when the data said no (recency/streak weighting, the original
event-drift strategy).
