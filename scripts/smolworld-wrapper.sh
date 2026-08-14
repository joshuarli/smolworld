#!/usr/bin/env bash
# Runtime wrapper installed beside the smolworld binary. It supplies the
# bundled smolvm binary, agent rootfs, and dynamic-library search path while
# leaving all of those values overridable for local development.

set -euo pipefail

resolve_symlink() {
    local target="$1"
    while [[ -L "$target" ]]; do
        local link_dir
        link_dir="$(cd "$(dirname "$target")" && pwd -P)"
        target="$(readlink "$target")"
        if [[ "$target" != /* ]]; then
            target="$link_dir/$target"
        fi
    done
    printf '%s\n' "$target"
}

SCRIPT_PATH="$(resolve_symlink "${BASH_SOURCE[0]}")"
SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" && pwd -P)"

if [[ "$(uname -s)" != "Darwin" || "$(uname -m)" != "arm64" ]]; then
    echo "error: smolworld supports only macOS on Apple Silicon" >&2
    exit 1
fi

RUNTIME_DIR="$(cd "$SCRIPT_DIR/../lib/smolworld" && pwd -P)"
SMOLWORLD_BIN="$RUNTIME_DIR/smolworld-bin"

[[ -x "$SMOLWORLD_BIN" ]] || {
    echo "error: smolworld binary not found at $SMOLWORLD_BIN" >&2
    exit 1
}

export SMOLWORLD_SMOLVM="${SMOLWORLD_SMOLVM:-$RUNTIME_DIR/smolvm-bin}"
export SMOLVM_AGENT_ROOTFS="${SMOLVM_AGENT_ROOTFS:-$RUNTIME_DIR/agent-rootfs}"
export SMOLVM_LIB_DIR="${SMOLVM_LIB_DIR:-$RUNTIME_DIR/lib}"

export DYLD_LIBRARY_PATH="$SMOLVM_LIB_DIR${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"

exec "$SMOLWORLD_BIN" "$@"
