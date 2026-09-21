#!/usr/bin/env bash
set -euo pipefail

WOODLAND_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
WOODLAND_VECTOR_DIR=$(mktemp -d "${TMPDIR:-/tmp}/woodland-covenants.XXXXXXXX")
trap 'rm -rf "$WOODLAND_VECTOR_DIR"' EXIT

cd "$WOODLAND_ROOT"
WOODLAND_COVENANT_VECTORS="$WOODLAND_VECTOR_DIR" \
  cargo test --locked --lib --all-features covenant_vm_vectors

# A typo in a Rust test filter otherwise succeeds with zero tests. Require both
# independent exporters before invoking the unmodified stock interpreter.
for filename in template.json chop.json; do
  if [[ ! -s "$WOODLAND_VECTOR_DIR/$filename" ]]; then
    printf 'Missing covenant VM fixture export: %s\n' "$filename" >&2
    exit 1
  fi
done

cd "$WOODLAND_ROOT/scripts/covenant-vm"
"${WOODLAND_GO:-go}" run -mod=readonly . \
  "$WOODLAND_VECTOR_DIR/template.json" "$WOODLAND_VECTOR_DIR/chop.json"
