#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REGTEST="$ROOT/regtest/regtest.mjs"
ARKD_COMMIT=c7c3184f5cd416e231023f717489a5b0550960cc
ARKD_IMAGE=arkd-local:c7c3184-forest-v1-stock
ARKD_WALLET_IMAGE=arkd-wallet-local:c7c3184-forest-v1-stock
export ARKD_IMAGE ARKD_WALLET_IMAGE
OWNER_VOLUME=woodland-regtest-owner
LOCK_FILE=/tmp/woodland-regtest.lock
WORLD_MANIFEST="$ROOT/regtest/_build/woodland-world.json"
WORLD_PLAN="$ROOT/regtest/_build/woodland-world-plan.json"
DEPLOYER_SECRET=${WOODLAND_DEPLOYER_SECRET:-1111111111111111111111111111111111111111111111111111111111111111}
ROLLOVER_SECRET=${WOODLAND_ROLLOVER_SECRET:-4444444444444444444444444444444444444444444444444444444444444444}
GATE_BIN="$ROOT/target/debug/woodland-emulator-gate"
GATE_PID_FILE="$ROOT/regtest/_build/emulator-gate.pid"
GATE_LOG="$ROOT/regtest/_build/emulator-gate.log"

usage() {
  cat <<'EOF'
usage: ./scripts/regtest.sh <command> [args]

  start                         build missing images and start minimal base + ark
  start-tree                    start minimal base + ark + script emulator
  renew-world                   run the wrapper's pre-game renewal pass
  stop                          stop containers, preserving data
  clean --force                 remove containers and volumes
  build-images [arkd-source]    build stock pinned arkd and arkd-wallet images
  fund <ark-address> <sats>     send seeded offchain sats to a browser wallet
  balance                       show the seeded Ark CLI wallet balance
  vtxos                         show the seeded Ark CLI wallet VTXOs
  info                          print local /v1/info
  mine [blocks]                 mine regtest blocks
  rpc <args...>                 bitcoin-cli passthrough
  ark <args...>                 Ark CLI passthrough
  arkd <args...>                arkd CLI passthrough
EOF
}

require_regtest() {
  if [[ ! -f "$REGTEST" ]]; then
    echo "error: vendored regtest harness is missing" >&2
    exit 1
  fi
}

prepare_arkd_source() {
  local source=${1:-${ARKD_SOURCE:-$ROOT/.cache/arkd-stock}}
  if [[ ! -d "$source/.git" ]]; then
    mkdir -p "$(dirname "$source")"
    git clone --filter=blob:none "https://github.com/arkade-os/arkd.git" "$source"
  fi
  git -C "$source" fetch --depth 1 origin "$ARKD_COMMIT"
  git -C "$source" checkout --detach "$ARKD_COMMIT"
  if ! git -C "$source" diff --quiet || [[ -n $(git -C "$source" status --short) ]]; then
    echo "error: stock arkd source has local changes: $source" >&2
    exit 1
  fi
  PREPARED_ARKD_SOURCE=$source
}


build_images() {
  prepare_arkd_source "${1:-}"
  local source=$PREPARED_ARKD_SOURCE
  docker build \
    --build-arg "VERSION=$ARKD_COMMIT" \
    --file "$source/Dockerfile" \
    --tag "$ARKD_IMAGE" \
    "$source"
  docker build \
    --build-arg "VERSION=$ARKD_COMMIT" \
    --file "$source/arkdwallet.Dockerfile" \
    --tag "$ARKD_WALLET_IMAGE" \
    "$source"
}

ensure_images() {
  if ! docker image inspect "$ARKD_IMAGE" >/dev/null 2>&1 \
    || ! docker image inspect "$ARKD_WALLET_IMAGE" >/dev/null 2>&1; then
    build_images
  fi
}

fund_address() {
  local address=$1
  local sats=$2
  # A preserved faucet wallet may hold only near-expiry VTXOs. Redeem a new
  # server note first so coin selection gives the recipient fresh lifetime.
  local note
  note=$(node "$REGTEST" arkd note --amount "$sats")
  if [[ ! "$note" =~ ^arknote[[:alnum:]]+$ ]]; then
    echo "error: failed to create a fresh Ark credit note" >&2
    exit 1
  fi
  node "$REGTEST" ark redeem-notes -n "$note" --password secret >/dev/null
  node "$REGTEST" ark send --to "$address" --amount "$sats" --password secret
}

run_world_bootstrap() {
  WOODLAND_NETWORK=regtest \
  WOODLAND_ARKADE_SERVICE_URL="${WOODLAND_ARKADE_SERVICE_URL:-http://127.0.0.1:7070}" \
  WOODLAND_EMULATOR_URL="${WOODLAND_EMULATOR_URL:-http://127.0.0.1:7074}" \
  WOODLAND_DEPLOYER_SECRET="$DEPLOYER_SECRET" \
  WOODLAND_ROLLOVER_SECRET="$ROLLOVER_SECRET" cargo run \
    --manifest-path "$ROOT/Cargo.toml" \
    --locked \
    --quiet \
    --features regtest-e2e \
    --bin woodland-operator \
    -- "$@"
}

stop_emulator_gate() {
  if [[ ! -f "$GATE_PID_FILE" ]]; then
    return
  fi
  local pid executable gate_executable
  pid=$(cat "$GATE_PID_FILE")
  if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
    executable=$(readlink -f "/proc/$pid/exe" 2>/dev/null || true)
    gate_executable=$(readlink -f "$GATE_BIN" 2>/dev/null || true)
    if [[ "$executable" != "$gate_executable" && "$executable" != "$gate_executable (deleted)" ]]; then
      echo "error: refuse to stop unrelated PID $pid from $GATE_PID_FILE" >&2
      exit 1
    fi
    kill "$pid"
    for _ in {1..50}; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.1
    done
    if kill -0 "$pid" 2>/dev/null; then
      echo "error: emulator gate PID $pid did not stop" >&2
      exit 1
    fi
  fi
  rm -f "$GATE_PID_FILE"
}

start_emulator_gate() {
  stop_emulator_gate
  mkdir -p "$(dirname "$GATE_LOG")"
  local allowed_origin="${WOODLAND_EMULATOR_GATE_ORIGIN:-${WOODLAND_SERVER_ORIGIN:-${WOODLAND_SERVER_PUBLIC_URL:-http://127.0.0.1:8000}}}"
  cargo build \
    --manifest-path "$ROOT/Cargo.toml" \
    --locked \
    --quiet \
    --features server \
    --bin woodland-emulator-gate
  WOODLAND_EMULATOR_GATE_BIND=127.0.0.1:7074 \
  WOODLAND_EMULATOR_UPSTREAM_URL=http://127.0.0.1:7073 \
  WOODLAND_BITCOIN_RPC_URL=http://127.0.0.1:18443 \
  WOODLAND_BITCOIN_RPC_USER=admin1 \
  WOODLAND_BITCOIN_RPC_PASSWORD=123 \
  WOODLAND_EMULATOR_GATE_ORIGIN="$allowed_origin" \
    nohup "$GATE_BIN" >"$GATE_LOG" 2>&1 &
  local pid=$!
  printf '%s\n' "$pid" >"$GATE_PID_FILE"
  for attempt in {1..60}; do
    if ! kill -0 "$pid" 2>/dev/null; then
      cat "$GATE_LOG" >&2
      echo "error: emulator gate exited before readiness" >&2
      exit 1
    fi
    if curl --fail --silent http://127.0.0.1:7074/health.json >/dev/null; then
      return
    fi
    if [[ $attempt == 60 ]]; then
      cat "$GATE_LOG" >&2
      echo "error: emulator gate did not become ready" >&2
      exit 1
    fi
    sleep 0.25
  done
}

ensure_world() {
  local output status address sats
  output=$(run_world_bootstrap status "$WORLD_MANIFEST")
  IFS=$'\t' read -r status address sats <<<"$output"
  case "$status" in
    needs-funding)
      printf 'Funding woodland.sh at %s with %s sats...\n' "$address" "$sats"
      fund_address "$address" "$sats"
      ;;
    ready|resume|funded)
      ;;
    reset-required)
      echo "error: the persisted woodland.sh world uses an incompatible protocol generation" >&2
      echo "reset it with: ./scripts/regtest.sh clean --force" >&2
      exit 1
      ;;
    *)
      echo "error: invalid woodland.sh bootstrap status: $output" >&2
      exit 1
      ;;
  esac
  run_world_bootstrap ensure "$WORLD_MANIFEST"
}

assert_stack_ownership() {
  local expected="$ROOT/regtest/docker/compose.base.yml,$ROOT/regtest/docker/compose.ark.yml"
  local containers config_files owner volumes
  containers=$(docker container ls -a \
    --filter label=com.docker.compose.project=arkade-regtest \
    --format '{{.ID}}')
  while IFS= read -r container; do
    [[ -z "$container" ]] && continue
    config_files=$(docker container inspect "$container" \
      --format '{{ index .Config.Labels "com.docker.compose.project.config_files" }}')
    if [[ "$config_files" != "$expected" ]]; then
      echo "error: the global arkade-regtest Docker project belongs to another checkout: $config_files" >&2
      exit 1
    fi
  done <<<"$containers"

  if docker volume inspect "$OWNER_VOLUME" >/dev/null 2>&1; then
    owner=$(docker volume inspect "$OWNER_VOLUME" \
      --format '{{ index .Labels "woodland.owner" }}')
    if [[ "$owner" != "$ROOT" ]]; then
      echo "error: the global arkade-regtest volumes belong to another checkout: $owner" >&2
      exit 1
    fi
    return
  fi

  volumes=$(docker volume ls \
    --filter label=com.docker.compose.project=arkade-regtest \
    --format '{{.Name}}')
  if [[ -n "$volumes" && -z "$containers" ]]; then
    echo "error: found unowned arkade-regtest volumes; refuse to reuse or delete them" >&2
    exit 1
  fi
  docker volume create --label "woodland.owner=$ROOT" "$OWNER_VOLUME" >/dev/null
}

assert_stack_stopped() {
  local running
  running=$(docker container ls \
    --filter label=com.docker.compose.project=arkade-regtest \
    --format '{{.Names}}')
  if [[ -n "$running" ]]; then
    echo "error: regtest containers are still running: ${running//$'\n'/, }" >&2
    exit 1
  fi
}

assert_pinned_images() {
  local actual
  actual=$(docker container inspect arkd --format '{{.Config.Image}}')
  if [[ "$actual" != "$ARKD_IMAGE" ]]; then
    echo "error: arkd is using $actual instead of required image $ARKD_IMAGE" >&2
    exit 1
  fi
  actual=$(docker container inspect arkd-wallet --format '{{.Config.Image}}')
  if [[ "$actual" != "$ARKD_WALLET_IMAGE" ]]; then
    echo "error: arkd-wallet is using $actual instead of required image $ARKD_WALLET_IMAGE" >&2
    exit 1
  fi
}

assert_stack_removed() {
  local containers volumes
  containers=$(docker container ls -a \
    --filter label=com.docker.compose.project=arkade-regtest \
    --format '{{.Names}}')
  volumes=$(docker volume ls \
    --filter label=com.docker.compose.project=arkade-regtest \
    --format '{{.Name}}')
  if [[ -n "$containers" || -n "$volumes" ]]; then
    echo "error: regtest cleanup left containers or volumes behind" >&2
    exit 1
  fi
}

load_existing_bitcoin_wallet() {
  if ! docker container inspect bitcoin >/dev/null 2>&1; then
    return
  fi
  if [[ $(docker container inspect bitcoin --format '{{ index .Config.Labels "com.docker.compose.project" }}') != "arkade-regtest" ]]; then
    return
  fi
  docker start bitcoin >/dev/null
  for _ in {1..30}; do
    if docker exec bitcoin bitcoin-cli -regtest -rpcuser=admin1 -rpcpassword=123 getblockchaininfo >/dev/null 2>&1; then
      docker exec bitcoin bitcoin-cli -regtest -rpcuser=admin1 -rpcpassword=123 loadwallet default >/dev/null 2>&1 || true
      return
    fi
    sleep 1
  done
}

command=${1:-}
shift || true

case "$command" in
  start|start-tree|renew-world|stop|clean|build-images|fund|balance|vtxos|info|mine|rpc|ark|arkd)
    if [[ ${ARKADE_REGTEST_LOCKED:-} != 1 ]]; then
      export ARKADE_REGTEST_LOCKED=1
      exec flock --exclusive --close "$LOCK_FILE" "$0" "$command" "$@"
    fi
    ;;
esac

if [[ "$command" == "clean" ]]; then
  if [[ ${1:-} != "--force" || $# -ne 1 ]]; then
    echo "error: clean permanently removes the globally named regtest volumes; rerun with clean --force" >&2
    exit 1
  fi
fi

case "$command" in
  start|start-tree|renew-world|stop|clean|fund|balance|vtxos|info|mine|rpc|ark|arkd)
    assert_stack_ownership
    ;;
esac

case "$command" in
  start)
    require_regtest
    stop_emulator_gate
    ensure_images
    load_existing_bitcoin_wallet
    node "$REGTEST" start --profile ark
    assert_pinned_images
    ;;
  start-tree)
    require_regtest
    ensure_images
    stop_emulator_gate
    # A prior full-profile run can leave optional containers alive because
    # Compose does not stop services omitted from a later profile selection.
    node "$REGTEST" stop
    load_existing_bitcoin_wallet
    node "$REGTEST" start --profile emulator
    assert_pinned_images
    start_emulator_gate
    ensure_world
    ;;
  renew-world)
    if [[ ${WOODLAND_RENEWAL_STARTUP:-} != 1 ]]; then
      echo "error: renew-world is pre-game only; start it through ./scripts/run-web.sh" >&2
      exit 1
    fi
    require_regtest
    run_world_bootstrap renew-once "$WORLD_MANIFEST"
    ;;
  stop)
    require_regtest
    stop_emulator_gate
    node "$REGTEST" stop
    assert_stack_stopped
    ;;
  clean)
    require_regtest
    stop_emulator_gate
    node "$REGTEST" clean
    assert_stack_removed
    docker volume rm "$OWNER_VOLUME" >/dev/null
    rm -f \
      "$WORLD_MANIFEST" \
      "$WORLD_PLAN" \
      "$GATE_LOG"
    ;;
  build-images)
    build_images "${1:-}"
    ;;
  fund)
    require_regtest
    address=${1:-}
    sats=${2:-}
    if [[ -z "$address" || ! "$sats" =~ ^[0-9]+$ || "$sats" == 0 ]]; then
      echo "usage: ./scripts/regtest.sh fund <ark-address> <positive-sats>" >&2
      exit 1
    fi
    fund_address "$address" "$sats"
    ;;
  balance|vtxos)
    require_regtest
    node "$REGTEST" ark "$command"
    ;;
  info)
    curl --fail --silent --show-error http://127.0.0.1:7070/v1/info
    printf '\n'
    ;;
  mine)
    require_regtest
    node "$REGTEST" mine "${1:-1}"
    ;;
  rpc|ark|arkd)
    require_regtest
    node "$REGTEST" "$command" "$@"
    ;;
  *)
    usage >&2
    exit 1
    ;;
esac
