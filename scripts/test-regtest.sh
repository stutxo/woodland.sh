#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
export WOODLAND_NETWORK=regtest
export WOODLAND_ARKADE_SERVICE_URL=http://127.0.0.1:7070
export WOODLAND_EMULATOR_URL=http://127.0.0.1:7073
export WOODLAND_DEPLOYER_SECRET=1111111111111111111111111111111111111111111111111111111111111111
export WOODLAND_TREE_MAINTENANCE_SECRET=2222222222222222222222222222222222222222222222222222222222222222
export WOODLAND_ROLLOVER_SECRET=4444444444444444444444444444444444444444444444444444444444444444
export WOODLAND_WORLD_MANIFEST="$ROOT/regtest/_build/woodland-world.json"
export WOODLAND_LEADERBOARD_URL=http://127.0.0.1:8090
export WOODLAND_LEADERBOARD_PUBLIC_URL=http://127.0.0.1:8090
export WOODLAND_LEADERBOARD_ORIGIN=http://127.0.0.1:18776
export WOODLAND_LEADERBOARD_BIND=127.0.0.1:8090
export WOODLAND_LEADERBOARD_DB="$ROOT/regtest/_build/leaderboard.json"
PROFILE=${1:-full}

case "$PROFILE" in
  smoke|full) ;;
  *)
    printf 'usage: %s [smoke|full]\n' "$0" >&2
    exit 2
    ;;
esac

export WOODLAND_E2E_PROFILE=$PROFILE
LEADERBOARD_PID=
LEADERBOARD_LOG="${WOODLAND_E2E_ARTIFACT_DIR:-$ROOT/regtest/_build/ci-artifacts}/leaderboard.log"

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n "$LEADERBOARD_PID" ]]; then
    kill "$LEADERBOARD_PID" 2>/dev/null || true
    wait "$LEADERBOARD_PID" 2>/dev/null || true
  fi
  "$ROOT/scripts/regtest.sh" stop || true
  exit "$status"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

printf 'Running woodland.sh %s regtest profile\n' "$PROFILE"
"$ROOT/scripts/regtest.sh" clean --force
"$ROOT/scripts/regtest.sh" start-tree
cargo build --locked --features leaderboard --bin woodland-leaderboard
rm -f "$WOODLAND_LEADERBOARD_DB"
mkdir -p "$(dirname "$LEADERBOARD_LOG")"
"$ROOT/target/debug/woodland-leaderboard" >"$LEADERBOARD_LOG" 2>&1 &
LEADERBOARD_PID=$!
for attempt in {1..60}; do
  if curl --fail --silent "$WOODLAND_LEADERBOARD_URL/health.json" >/dev/null; then
    break
  fi
  if [[ $attempt == 60 ]]; then
    cat "$LEADERBOARD_LOG" >&2
    printf 'error: leaderboard did not become ready\n' >&2
    exit 1
  fi
  sleep 1
done
"$ROOT/scripts/build-web.sh"
node "$ROOT/scripts/e2e-suite.mjs"
