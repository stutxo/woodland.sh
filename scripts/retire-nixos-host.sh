#!/usr/bin/env bash
# Remove only this host's Woodland NixOS module, retaining deployment data.
set -euo pipefail
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if ! command -v python3 >/dev/null && [[ -f /etc/NIXOS ]]; then
    printf -v invocation '%q ' python3 "$SCRIPT_DIR/host-bundle.py" retire "$@"
    exec nix-shell -p python3 --run "$invocation"
fi
exec python3 "$SCRIPT_DIR/host-bundle.py" retire "$@"
