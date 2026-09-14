# Deploy

launchd agents for fully autonomous daily operation, plus the tooling to keep
the live process isolated from day-to-day development.

## Why a separate clone

The dev checkout is where you `git checkout`, `cargo build`, and iterate. If
launchd's `WorkingDirectory`/binary path point at that same tree, an
in-progress build or branch switch can yank the running binary or config out
from under the live paper-trader.

To avoid that, run the live agents out of their own clone, on a dedicated
`live-execution` branch, outside the dev tree:

```
./deploy/setup_live_clone.sh              # clones to ~/live/kalshi-live on live-execution
./deploy/setup_live_clone.sh <dir> <branch>  # override defaults
```

The script clones (or updates) the repo at `~/live/kalshi-live`, checks out
`live-execution`, and builds the release binaries there. After the first run,
copy a local `config.toml` and your Kalshi private key into
`~/live/kalshi-live/kalshi-pipeline/`. The config and PEM must never be
committed. Start from `kalshi-pipeline/config.live.example.toml` for the
executor-ready production shape.

## Production readiness

Before enabling live order submission, leave `live_enabled = false` and
`confirm_prod = false` in the local config, then run the read-only check:

```
cd ~/live/kalshi-live/kalshi-pipeline
cargo build --release -p kalshi-executor
./target/release/kalshi-executor --config config.toml preflight
```

`preflight` authenticates only to read the balance, positions, and resting
orders. It never constructs an order client and cannot submit, cancel, or
flatten an order. Resolve any existing positions or resting orders before the
first executor start. Also preallocate collateral to the exchange shard shown
by each intended market's `exchange_index`.

Only after reviewing those results and selecting a real-money risk budget
should both production switches be changed to `true`. The template starts at a
$1 maximum exposure and $1 daily loss stop on purpose.

## launchd agents

- `com.sid.kalshi-paper-trader.plist` — runs the `paper-trader` binary.
- `com.sid.kalshi-executor.plist` — runs the guarded executor after its
  configuration gates are deliberately enabled.
- `com.sid.kalshi-tracker.plist` — runs `tracker_server.py` (dashboard on
  `localhost:8787`).
- `deploy_once.sh` — bounces the paper-trader onto a freshly built binary,
  then removes the one-shot launchd job that invoked it.

Both plists point at `~/live/kalshi-live` (adjust the paths if you use a
different `LIVE_DIR`). Install/refresh with:

```
cp deploy/com.sid.kalshi-paper-trader.plist deploy/com.sid.kalshi-tracker.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.sid.kalshi-paper-trader.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.sid.kalshi-tracker.plist
```

Install the executor agent only after the production preflight has passed and
both order gates have been intentionally enabled:

```
cp deploy/com.sid.kalshi-executor.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.sid.kalshi-executor.plist
```

The executor agent does not restart while the local `KILL` file exists. For
an emergency stop, create `~/live/kalshi-live/kalshi-pipeline/KILL`, then use
the executor's logged reconciliation result to verify the account is flat.

To push a new build live, rebuild in `~/live/kalshi-live` (e.g. via
`setup_live_clone.sh` again, or `git pull` + `cargo build --release` inside
that clone) and run `deploy_once.sh`.
