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
copy `config.toml` and your Kalshi private key into
`~/live/kalshi-live/kalshi-pipeline/`.

## launchd agents

- `com.sid.kalshi-paper-trader.plist` — runs the `paper-trader` binary.
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

To push a new build live, rebuild in `~/live/kalshi-live` (e.g. via
`setup_live_clone.sh` again, or `git pull` + `cargo build --release` inside
that clone) and run `deploy_once.sh`.
