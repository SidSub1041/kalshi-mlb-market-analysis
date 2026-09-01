#!/usr/bin/env bash
# Set up an isolated, launchd-safe working copy for live/paper trading.
#
# The dev checkout (wherever you run `cargo build`, `git checkout`, etc.) must
# never be the same directory that launchd points its ProgramArguments /
# WorkingDirectory at: a `git checkout` or in-progress `cargo build` in the
# dev tree can yank the binary or config out from under the running agent.
#
# This script clones a fresh, dedicated copy to LIVE_DIR (default
# ~/live/kalshi-live) on a dedicated branch (default live-execution),
# builds the release binaries there, and leaves it ready for the launchd
# plists in this directory to point at.
#
# Usage:
#   ./deploy/setup_live_clone.sh [live-dir] [branch]
#
# Run from inside the dev clone so the origin remote can be discovered
# automatically; override with $REPO_URL if you need to.
set -euo pipefail

LIVE_DIR="${1:-$HOME/live/kalshi-live}"
BRANCH="${2:-live-execution}"
REPO_URL="${REPO_URL:-$(git -C "$(dirname "$0")/.." remote get-url origin 2>/dev/null || true)}"

if [[ -z "$REPO_URL" ]]; then
  echo "error: could not determine repo URL; set REPO_URL=... and rerun" >&2
  exit 1
fi

echo "==> Repo:   $REPO_URL"
echo "==> Live dir: $LIVE_DIR"
echo "==> Branch: $BRANCH"

if [[ -d "$LIVE_DIR/.git" ]]; then
  echo "==> Existing clone found, fetching + updating"
  git -C "$LIVE_DIR" fetch origin
  if git -C "$LIVE_DIR" show-ref --verify --quiet "refs/heads/$BRANCH"; then
    git -C "$LIVE_DIR" checkout "$BRANCH"
  else
    git -C "$LIVE_DIR" checkout -B "$BRANCH" "origin/$BRANCH" 2>/dev/null \
      || git -C "$LIVE_DIR" checkout -B "$BRANCH"
  fi
  git -C "$LIVE_DIR" pull --ff-only origin "$BRANCH" 2>/dev/null || true
else
  echo "==> Cloning fresh working copy"
  mkdir -p "$(dirname "$LIVE_DIR")"
  git clone "$REPO_URL" "$LIVE_DIR"
  if git -C "$LIVE_DIR" show-ref --verify --quiet "refs/remotes/origin/$BRANCH"; then
    git -C "$LIVE_DIR" checkout -B "$BRANCH" "origin/$BRANCH"
  else
    git -C "$LIVE_DIR" checkout -B "$BRANCH"
  fi
fi

echo "==> Building release binaries in the live clone"
(cd "$LIVE_DIR/kalshi-pipeline" && cargo build --release)

echo
echo "Done. Live working copy ready at: $LIVE_DIR"
echo "Next steps:"
echo "  - copy your config.toml and private key into $LIVE_DIR/kalshi-pipeline/"
echo "  - point the launchd plists' WorkingDirectory/paths at $LIVE_DIR"
echo "  - launchctl bootstrap gui/\$(id -u) ~/Library/LaunchAgents/com.sid.kalshi-paper-trader.plist"
