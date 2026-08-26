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
PROFILE=${1:-full}

case "$PROFILE" in
  smoke|full) ;;
  *)
    printf 'usage: %s [smoke|full]\n' "$0" >&2
    exit 2
    ;;
esac

export WOODLAND_E2E_PROFILE=$PROFILE

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  "$ROOT/scripts/regtest.sh" stop || true
  exit "$status"
}

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

printf 'Running woodland.sh %s regtest profile\n' "$PROFILE"
"$ROOT/scripts/regtest.sh" clean --force
"$ROOT/scripts/regtest.sh" start-tree
"$ROOT/scripts/build-web.sh"
node "$ROOT/scripts/e2e-suite.mjs"
