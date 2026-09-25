#!/usr/bin/env bash
# Snapshot an already-deployed world; never deploy or regenerate credentials.
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if ! command -v python3 >/dev/null && [[ -f /etc/NIXOS ]]; then
    # sudo may discard a caller's nix-shell PATH; acquire Python in this process.
    printf -v invocation '%q ' python3 "$SCRIPT_DIR/host-bundle.py" export "$@"
    exec nix-shell -p python3 --run "$invocation"
fi
exec python3 "$SCRIPT_DIR/host-bundle.py" export "$@"
