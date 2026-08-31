#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$ROOT/regtest/_build"
exec 9>"$ROOT/regtest/_build/release-soak.lock"
if ! flock -n 9; then
  printf 'error: another woodland.sh release soak is already running\n' >&2
  exit 1
fi
export WOODLAND_RELEASE_SOAK_LOCKED=1
exec node "$ROOT/scripts/e2e-overnight.mjs"
