#!/usr/bin/env bash
# One-shot bootstrap: toolchain -> build -> test -> (optional) collect + train.
# Run from the repo root:  ./setup.sh            (build + test only)
#                          ./setup.sh --collect  (also pull the season + train)
set -euo pipefail
cd "$(dirname "$0")"

echo "==> 1/4 Rust toolchain"
if ! command -v cargo >/dev/null 2>&1; then
  echo "    installing rustup (one-time)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source "$HOME/.cargo/env"
fi
rustc --version && cargo --version

echo "==> 2/4 Build (release)"
cd kalshi-pipeline
cargo build --release

echo "==> 3/4 Tests"
cargo test -p common

echo "==> 4/4 Python deps"
python3 -m pip install --quiet pandas numpy scikit-learn scipy matplotlib

if [[ "${1:-}" == "--collect" ]]; then
  echo "==> Collecting full season (1-2 h, resumable — safe to Ctrl-C and rerun)"
  cargo run --release -p collector -- \
    --start-date 2026-03-25 --end-date "$(date -v-1d +%Y-%m-%d 2>/dev/null || date -d yesterday +%Y-%m-%d)" \
    --out data_full
  echo "==> Training (walk-forward)"
  cd ..
  python3 train.py --data kalshi-pipeline/data_full --horizons 1 3 5 10 --out oos_preds.csv
fi

echo
echo "Done. Next steps:"
echo "  - full pull + model:  ./setup.sh --collect"
echo "  - paper trading:      cp kalshi-pipeline/config.example.toml kalshi-pipeline/config.toml"
echo "                        (add your Kalshi key id + private key path, then)"
echo "                        cd kalshi-pipeline && cargo run --release -p paper-trader -- --config config.toml"
