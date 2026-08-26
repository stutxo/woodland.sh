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
  printf 'error: another Mutinynet maintenance process owns %s\n' "$LOCK_FILE" >&2
  exit 73
fi

set -a
# shellcheck disable=SC1090
source "$ENV_FILE"
set +a

: "${WOODLAND_DEPLOYER_SECRET:?missing WOODLAND_DEPLOYER_SECRET}"
: "${WOODLAND_TREE_MAINTENANCE_SECRET:?missing WOODLAND_TREE_MAINTENANCE_SECRET}"
: "${WOODLAND_ROLLOVER_SECRET:?missing WOODLAND_ROLLOVER_SECRET}"
: "${WOODLAND_WORLD_MANIFEST:?missing WOODLAND_WORLD_MANIFEST}"

mkdir -p "$(dirname "$WOODLAND_WORLD_MANIFEST")"
cargo build \
  --manifest-path "$ROOT/Cargo.toml" \
  --release \
  --locked \
  --features woodland-app \
  --bin woodland-operator

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
WOODLAND_WASM_FEATURES=woodland-app "$ROOT/scripts/build-web.sh"

printf '\nGitHub Pages bundle ready in %s/dist\n' "$ROOT"
printf 'Push main to deploy through the gated Pages workflow.\n\n'
exec "$OPERATOR" watch "$WOODLAND_WORLD_MANIFEST"
