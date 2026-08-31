#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
export WOODLAND_NETWORK=regtest
export WOODLAND_ARKADE_SERVICE_URL=http://127.0.0.1:7070
export WOODLAND_EMULATOR_URL=http://127.0.0.1:7074
export WOODLAND_DEPLOYER_SECRET=1111111111111111111111111111111111111111111111111111111111111111
export WOODLAND_ROLLOVER_SECRET=4444444444444444444444444444444444444444444444444444444444444444
export WOODLAND_WORLD_MANIFEST="$ROOT/regtest/_build/woodland-world.json"
PORT=${WOODLAND_WEB_PORT:-8000}
SERVER_LOCK=${WOODLAND_WEB_LOCK_FILE:-/tmp/woodland-web.lock}
export WOODLAND_SERVER_PUBLIC_URL="http://127.0.0.1:$PORT"
export WOODLAND_SERVER_BIND="127.0.0.1:$PORT"
export WOODLAND_SERVER_DB="$ROOT/regtest/_build/server.json"
export WOODLAND_SERVER_WEB_ROOT="$ROOT/dist"
WATCHER_PID=
SERVER_PID=
exec 9>"$SERVER_LOCK"
if ! flock --exclusive --nonblock 9; then
  printf 'error: another woodland.sh web server already owns %s\n' "$SERVER_LOCK" >&2
  exit 73
fi

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  for pid in "$SERVER_PID" "$WATCHER_PID"; do
    [[ -n "$pid" ]] || continue
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  "$ROOT/scripts/regtest.sh" stop || true
  exit "$status"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

"$ROOT/scripts/regtest.sh" start-tree
WOODLAND_RENEWAL_STARTUP=1 "$ROOT/scripts/regtest.sh" renew-world
# Registration signatures bind the server origin and world genesis; a reused
# database from another port or world fails closed and stops the server.
rm -f "$WOODLAND_SERVER_DB"
cargo build --locked --features server --bin woodland-operator --bin woodland-server
WOODLAND_SERVER_URL=self WOODLAND_WASM_FEATURES=regtest-e2e "$ROOT/scripts/build-web.sh"
unset WOODLAND_DEPLOYER_SECRET

env -u WOODLAND_ROLLOVER_SECRET "$ROOT/target/debug/woodland-operator" watch "$WOODLAND_WORLD_MANIFEST" &
WATCHER_PID=$!
"$ROOT/target/debug/woodland-server" &
SERVER_PID=$!

printf '\nwoodland.sh: http://127.0.0.1:%s/\n' "$PORT"
printf 'Static app, leaderboard, presence, chat, and delegation share this origin.\n'
printf 'Every LOG drop gives 1 XP. LOG chance rises from 20%% to 30%% at woodland levels 10, 20, 30, 40, and 50.\n'
printf "Funded stumps regrow after two Bitcoin tip advances; exhausted trees stay depleted.\n\n"
wait -n "$WATCHER_PID" "$SERVER_PID"
