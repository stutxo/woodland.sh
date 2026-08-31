#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [[ ${WOODLAND_RELEASE_SOAK_LOCKED:-0} != 1 ]]; then
  mkdir -p "$ROOT/regtest/_build"
  exec 9>"$ROOT/regtest/_build/release-soak.lock"
  if ! flock -n 9; then
    printf 'error: another woodland.sh release soak or regtest is already running\n' >&2
    exit 73
  fi
fi
export WOODLAND_NETWORK=regtest
export WOODLAND_ARKADE_SERVICE_URL=http://127.0.0.1:7070
export WOODLAND_EMULATOR_URL=http://127.0.0.1:7073
export WOODLAND_DEPLOYER_SECRET=1111111111111111111111111111111111111111111111111111111111111111
export WOODLAND_ROLLOVER_SECRET=4444444444444444444444444444444444444444444444444444444444444444
export WOODLAND_WORLD_MANIFEST="$ROOT/regtest/_build/woodland-world.json"
export WOODLAND_SERVER_URL=http://127.0.0.1:8090
export WOODLAND_SERVER_PUBLIC_URL=http://127.0.0.1:8090
export WOODLAND_SERVER_ORIGIN=http://127.0.0.1:8090
export WOODLAND_SERVER_BIND=127.0.0.1:8090
export WOODLAND_WEB_OUTPUT_DIR="${WOODLAND_WEB_OUTPUT_DIR:-$ROOT/regtest/_build/web}"
export WOODLAND_SERVER_WEB_ROOT="$WOODLAND_WEB_OUTPUT_DIR"
export WOODLAND_SERVER_DB="$ROOT/regtest/_build/server.json"
export WOODLAND_SERVER_FORCE_RENEWAL_ONCE=1
export WOODLAND_E2E_WEB_URL="$WOODLAND_SERVER_URL"
PROFILE=${1:-full}

case "$PROFILE" in
  smoke|full|soak|restock) ;;
  *)
    printf 'usage: %s [smoke|full|soak|restock]\n' "$0" >&2
    exit 2
    ;;
esac

export WOODLAND_E2E_PROFILE=$PROFILE
if [[ "$PROFILE" == restock ]]; then
  export WOODLAND_E2E_INITIAL_TREE_RESERVE=${WOODLAND_E2E_INITIAL_TREE_RESERVE:-5}
fi
SERVER_PID=
WATCHER_PID=
E2E_PID=
ARTIFACT_DIR="${WOODLAND_E2E_ARTIFACT_DIR:-$ROOT/regtest/_build/ci-artifacts}"
SERVER_LOG="$ARTIFACT_DIR/server.log"
WATCHER_LOG="$ARTIFACT_DIR/watcher.log"
ARKD_LOG="$ARTIFACT_DIR/arkd.log"
EMULATOR_LOG="$ARTIFACT_DIR/emulator.log"

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n "$E2E_PID" ]]; then
    kill -- "-$E2E_PID" 2>/dev/null || true
    wait "$E2E_PID" 2>/dev/null || true
  fi
  for pid in "$SERVER_PID" "$WATCHER_PID"; do
    [[ -n "$pid" ]] || continue
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  if [[ -f "$WOODLAND_WORLD_MANIFEST" ]]; then
    cp "$WOODLAND_WORLD_MANIFEST" "$ARTIFACT_DIR/world.json"
  fi
  if [[ -d "$ARTIFACT_DIR" ]]; then
    docker logs --tail 2000 arkd >"$ARKD_LOG" 2>&1 || true
    docker logs --tail 2000 emulator >"$EMULATOR_LOG" 2>&1 || true
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
WOODLAND_RENEWAL_STARTUP=1 "$ROOT/scripts/regtest.sh" renew-world
cargo build --locked --features server --bin woodland-server
rm -f "$WOODLAND_SERVER_DB"
mkdir -p "$ARTIFACT_DIR"
cp "$WOODLAND_WORLD_MANIFEST" "$ARTIFACT_DIR/world.json"
WOODLAND_SERVER_URL=self WOODLAND_WASM_FEATURES=regtest-e2e "$ROOT/scripts/build-web.sh"
"$ROOT/target/debug/woodland-operator" watch "$WOODLAND_WORLD_MANIFEST" >"$WATCHER_LOG" 2>&1 &
WATCHER_PID=$!
if curl --fail --silent --max-time 1 "$WOODLAND_SERVER_URL/health.json" >/dev/null 2>&1; then
  printf 'error: server port is already in use\n' >&2
  exit 1
fi
"$ROOT/target/debug/woodland-server" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
for attempt in {1..60}; do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    cat "$SERVER_LOG" >&2
    printf 'error: server exited before readiness\n' >&2
    exit 1
  fi
  if curl --fail --silent "$WOODLAND_SERVER_URL/health.json" >/dev/null; then
    break
  fi
  if [[ $attempt == 60 ]]; then
    cat "$SERVER_LOG" >&2
    printf 'error: server did not become ready\n' >&2
    exit 1
  fi
  sleep 1
done
if [[ "$PROFILE" == soak ]]; then
  setsid node "$ROOT/scripts/e2e-soak-regtest.mjs" &
elif [[ "$PROFILE" == restock ]]; then
  setsid node "$ROOT/scripts/e2e-restock-regtest.mjs" &
else
  setsid node "$ROOT/scripts/e2e-suite.mjs" &
fi
E2E_PID=$!
set +e
wait "$E2E_PID"
status=$?
kill -- "-$E2E_PID" 2>/dev/null || true
set -e
E2E_PID=
exit "$status"
