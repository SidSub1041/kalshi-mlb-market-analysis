# Kalshi MLB Pipeline

Rust workspace for full-season data collection and live paper trading, plus a Python
model pass. Built from the pilot findings in `../REPORT.md` (markets underreact to
scoring plays; drift is exploitable maker-side only).

**Status note:** this code was written in a sandboxed environment that cannot reach
crates.io, so it has not been compiled yet. Expect `cargo build` to surface small
fixable issues (dep version bumps, minor API drift) before first run.

## Setup

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # if no Rust
cd kalshi-pipeline
cargo build --release
cargo test -p common       # ticker parsing + GUMBO fixture tests
```

## 1. Collect the season

No credentials needed — Kalshi market data is public.

```bash
cargo run --release -p collector -- \
  --start-date 2026-03-25 --end-date 2026-07-02 --out data_full
```

~1–2 h at the default 8 req/s (stay under Kalshi's ~10/s public read limit).
Resumable: re-running skips files that already exist. Add `--no-trades` for a much
faster candles-only pass. Output schema is documented at the top of
`crates/collector/src/main.rs`. Watch stderr for `no MLB schedule match` warnings —
if a team code fails to match, extend `team_aliases()` in `crates/common/src/lib.rs`.

## 2. Train

```bash
pip install pandas numpy scikit-learn
python3 ../train.py --data data_full --horizons 1 3 5 10 --out oos_preds.csv
```

Weekly walk-forward: trains on weeks 1..k, tests on week k+1. Reports out-of-sample
R², directional hit rate, and gross/net cents-per-event at prediction thresholds.
The features use GUMBO `decisive_time` (last pitch), fixing the 30–60 s timing
leakage found in the pilot.

## 3. Paper trade (live week)

Requires a Kalshi API key (websocket auth) — kalshi.com → Account → API keys.
The private key stays in a local file; nothing is ever transmitted except the
signature. **This binary places no orders** — it only reads market data and
simulates fills; there is no order-placement code in it.

```bash
cp config.example.toml config.toml   # edit key_id + private_key_path
cargo run --release -p paper-trader -- --config config.toml
```

Entries: maker bid-join on the batting team's market after configured event types,
skipped when spread > 2¢ or price outside 5–95¢. Fill model tracks displayed queue
ahead and requires that much volume to print at the level (conservative), also
logging first-print time (optimistic bound). Exits: maker ask-join after
`hold_minutes`, crossing the spread (with the quadratic taker fee) after
`exit_timeout_s`. Round trips land in `paper_trades.csv`.

## Suggested week-1 review

Compare `paper_trades.csv` fill-adjusted PnL against the model's predicted drift for
the same events; if conservative-fill PnL is positive after fees, the next step is
real size-1 orders, which would need an order-placement module (deliberately not
included yet).
