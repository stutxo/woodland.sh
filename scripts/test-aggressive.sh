#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
export WOODLAND_OVERNIGHT_HOURS="${WOODLAND_OVERNIGHT_HOURS:-4}"
export WOODLAND_OVERNIGHT_PLAN="${WOODLAND_OVERNIGHT_PLAN:-full,burst,fanout,reload,regrowth}"
export WOODLAND_OVERNIGHT_COOLDOWN_SECONDS="${WOODLAND_OVERNIGHT_COOLDOWN_SECONDS:-5}"
exec node "$ROOT/scripts/e2e-overnight.mjs"
