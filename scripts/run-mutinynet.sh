#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
ENV_FILE=${WOODLAND_MUTINYNET_ENV:-$ROOT/.cache/mutinynet.env}
LOCK_FILE=${WOODLAND_MUTINYNET_LOCK_FILE:-/tmp/woodland-mutinynet.lock}

if [[ ! -f "$ENV_FILE" ]]; then
  printf 'error: missing Mutinynet configuration %s\n' "$ENV_FILE" >&2
  exit 78
fi

exec 9>"$LOCK_FILE"
if ! flock --exclusive --nonblock 9; then
  printf 'error: another Mutinynet world process owns %s\n' "$LOCK_FILE" >&2
  exit 73
fi

set -a
# shellcheck disable=SC1090
source "$ENV_FILE"
set +a

: "${WOODLAND_DEPLOYER_SECRET:?missing WOODLAND_DEPLOYER_SECRET}"
: "${WOODLAND_ROLLOVER_SECRET:?missing WOODLAND_ROLLOVER_SECRET}"
: "${WOODLAND_WORLD_MANIFEST:?missing WOODLAND_WORLD_MANIFEST}"

PORT=${WOODLAND_SERVER_PORT:-8000}
export WOODLAND_SERVER_PUBLIC_URL=${WOODLAND_SERVER_PUBLIC_URL:-http://127.0.0.1:$PORT}
export WOODLAND_SERVER_BIND=${WOODLAND_SERVER_BIND:-127.0.0.1:$PORT}
export WOODLAND_SERVER_WEB_ROOT=${WOODLAND_SERVER_WEB_ROOT:-$ROOT/dist}
export WOODLAND_SERVER_DB=${WOODLAND_SERVER_DB:-$ROOT/.cache/mutinynet-server.json}
WATCHER_PID=
SERVER_PID=

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  for pid in "$SERVER_PID" "$WATCHER_PID"; do
    [[ -n "$pid" ]] || continue
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  exit "$status"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$(dirname "$WOODLAND_WORLD_MANIFEST")"
cargo build \
  --manifest-path "$ROOT/Cargo.toml" \
  --release \
  --locked \
  --features server \
  --bin woodland-operator \
  --bin woodland-server

OPERATOR="$ROOT/target/release/woodland-operator"
status=$($OPERATOR status "$WOODLAND_WORLD_MANIFEST")
IFS=$'\t' read -r state address sats <<<"$status"
case "$state" in
  needs-funding)
    printf 'Mutinynet deployer needs %s sats at:\n%s\n' "$sats" "$address"
    printf 'Run: mutinynet-cli login && node scripts/fund-mutinynet.mjs %s %s\n' "$address" "$sats"
    exit 75
    ;;
  funded|resume|ready) ;;
  reset-required)
    printf 'error: persisted Mutinynet world is incompatible with this release\n' >&2
    exit 76
    ;;
  *)
    printf 'error: unexpected Mutinynet deployment status: %s\n' "$status" >&2
    exit 1
    ;;
esac

"$OPERATOR" ensure "$WOODLAND_WORLD_MANIFEST"
unset WOODLAND_DEPLOYER_SECRET
WOODLAND_SERVER_URL=self WOODLAND_WASM_FEATURES=woodland-app "$ROOT/scripts/build-web.sh"

env -u WOODLAND_ROLLOVER_SECRET "$OPERATOR" watch "$WOODLAND_WORLD_MANIFEST" &
WATCHER_PID=$!
"$ROOT/target/release/woodland-server" &
SERVER_PID=$!

printf '\nwoodland.sh app and API: %s/\n' "$WOODLAND_SERVER_PUBLIC_URL"
printf 'Static app, leaderboard, presence, chat, and delegation share one origin.\n\n'
wait -n "$WATCHER_PID" "$SERVER_PID"
