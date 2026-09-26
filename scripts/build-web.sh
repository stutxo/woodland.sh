#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
WASM_FEATURES=${WOODLAND_WASM_FEATURES:-woodland-app}
: "${WOODLAND_WORLD_MANIFEST:?set WOODLAND_WORLD_MANIFEST to the verified world manifest to publish}"
WORLD_MANIFEST=$WOODLAND_WORLD_MANIFEST
OUTPUT_DIR=${WOODLAND_WEB_OUTPUT_DIR:-$ROOT/dist}
cd "$ROOT"

# Reject a stale test-network manifest before building a mainnet artifact.
node --input-type=module - "$WORLD_MANIFEST" <<'JS'
import { readFileSync } from 'node:fs';
const manifest = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const network = (process.env.WOODLAND_NETWORK || '').toLowerCase();
if (['bitcoin', 'mainnet'].includes(network) && manifest.network !== 'bitcoin') {
  throw new Error('mainnet web build requires a bitcoin world manifest');
}
JS

if [[ -z "${CC_wasm32_unknown_unknown:-}" ]]; then
  # Reuse the repository LLVM when present, otherwise fall back to PATH.
  for toolchain_root in "$ROOT/.toolchain/llvm" "$ROOT/../arkade_game/.toolchain/llvm"; do
    [[ -d "$toolchain_root/usr" ]] || continue
    repo_clang=$(find "$toolchain_root/usr" -type f \( -name clang -o -name 'clang-[0-9]*' \) -path '*/bin/*' 2>/dev/null | sort -V | tail -1 || true)
    if [[ -n "$repo_clang" && -x "$repo_clang" ]]; then
      export CC_wasm32_unknown_unknown=$(realpath "$repo_clang")
      break
    fi
  done
fi

if [[ -z "${CC_wasm32_unknown_unknown:-}" ]]; then
  for compiler in clang clang-21 clang-20 clang-19 clang-18 clang-17 clang-16 clang-15; do
    if command -v "$compiler" >/dev/null 2>&1; then
      export CC_wasm32_unknown_unknown=$(command -v "$compiler")
      break
    fi
  done
fi

if [[ -z "${CC_wasm32_unknown_unknown:-}" ]]; then
  printf 'error: install clang or set CC_wasm32_unknown_unknown\n' >&2
  exit 1
fi

printf 'using CC_wasm32_unknown_unknown=%s\n' "$CC_wasm32_unknown_unknown"
rustup target add wasm32-unknown-unknown >/dev/null 2>&1 || true
mkdir -p "$OUTPUT_DIR/pkg"
wasm-pack build \
  --target web \
  --release \
  --no-pack \
  --out-dir "$OUTPUT_DIR/pkg" \
  -- \
  --features "$WASM_FEATURES"
rm -f "$OUTPUT_DIR/pkg/.gitignore"
node "$ROOT/scripts/assemble-web.mjs" "$WORLD_MANIFEST" "$OUTPUT_DIR"
printf 'built woodland.sh web bundle in %s with %s\n' "$OUTPUT_DIR" "$WASM_FEATURES"
