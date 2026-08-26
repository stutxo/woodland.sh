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
PORT=${WOODLAND_WEB_PORT:-8000}
SERVER_LOCK=${WOODLAND_WEB_LOCK_FILE:-/tmp/woodland-web.lock}
exec 9>"$SERVER_LOCK"
if ! flock --exclusive --nonblock 9; then
  printf 'error: another woodland.sh web server already owns %s\n' "$SERVER_LOCK" >&2
  exit 73
fi

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  "$ROOT/scripts/regtest.sh" stop || true
  exit "$status"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

"$ROOT/scripts/regtest.sh" start-tree
WOODLAND_MAINTENANCE_STARTUP=1 "$ROOT/scripts/regtest.sh" maintain
"$ROOT/scripts/build-web.sh"

printf '\nwoodland.sh: http://127.0.0.1:%s/\n' "$PORT"
printf 'Every LOG drop gives 1 XP. LOG chance rises from 10%% to 15%% at woodland levels 10, 20, 30, 40, and 50.\n'
printf 'Renewable stumps return after 20-40 seconds; exhausted trees stay depleted.\n\n'
node "$ROOT/scripts/dev-server.mjs" --port "$PORT"
