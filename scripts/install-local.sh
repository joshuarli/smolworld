#!/usr/bin/env bash
# Build and install a local smolworld runtime from a patched smolvm checkout.
#
# The script deliberately builds only artifacts for which the adjacent smolvm
# checkout has an unambiguous local build entry point. In particular, the
# macOS libkrunfw kernel blob must already be present in the selected library
# bundle; smolworld does not guess how an external kernel tree should be built.

set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"

usage() {
    cat <<'EOF'
Build and install smolworld with a local patched smolvm runtime.

Usage:
  scripts/install-local.sh [--check PATH]

Options:
  --check PATH   Run the installed `smolworld check` against PATH before commit.
  -h, --help     Show this help text.

Configuration environment:
  SMOLVM_SOURCE_DIR             Patched smolvm checkout (default: ../smolvm).
  SMOLVM_LIB_DIR                Matching libkrun/libkrunfw directory
                                (default: $SMOLVM_SOURCE_DIR/lib).
  SMOLVM_AGENT_ROOTFS           Existing agent rootfs to reuse.
  SMOLWORLD_BUILD_AGENT_ROOTFS  Build a missing rootfs with smolvm's script
                                (default: 1; set to 0 to require one).
  SMOLWORLD_BUILD_LIBKRUN       Rebuild libkrun with `make smolvm` (default: 0).
  SMOLWORLD_LIBKRUN_DIR         Pinned smolvm libkrun submodule for that
                                build (default: $SMOLVM_SOURCE_DIR/libkrun).
  SMOLWORLD_LIBKRUN_BUILD_FLAGS Make flags (default: BLK=1 NET=1 GPU=1).
  CODESIGN_IDENTITY             macOS signing identity (default: -).
  SMOLWORLD_INSTALL_PREFIX      Dedicated install directory
                                (default: ~/.local/smolworld).
  SMOLWORLD_CHECK_CONFIG        Same as --check PATH.
EOF
}

fail() {
    echo "error: $*" >&2
    exit 1
}

info() {
    echo "info: $*"
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command '$1' is not on PATH"
}

resolve_existing_dir() {
    local path="$1"
    [[ -d "$path" ]] || return 1
    (cd "$path" && pwd -P)
}

resolve_existing_file() {
    local path="$1"
    [[ -f "$path" ]] || return 1
    if [[ "$path" == /* ]]; then
        printf '%s\n' "$path"
    else
        (cd "$(dirname "$path")" && printf '%s/%s\n' "$(pwd -P)" "$(basename "$path")")
    fi
}

is_lfs_pointer() {
    local path="$1"
    head -c 64 "$path" 2>/dev/null | grep -Fq 'version https://git-lfs.github.com/spec/v1'
}

valid_rootfs() {
    local rootfs="$1"
    [[ -d "$rootfs/usr/local/bin" && -x "$rootfs/usr/local/bin/smolvm-agent" ]]
}

require_library() {
    local path="$1"
    [[ -f "$path" ]] || fail "missing required library: $path"
    [[ -s "$path" ]] || fail "required library is empty: $path"
    if is_lfs_pointer "$path"; then
        fail "$path is a Git LFS pointer; run 'git lfs pull' in the smolvm checkout"
    fi
    local description
    description="$(file -b "$path")"
    [[ "$description" == *"Mach-O"* ]] || fail "$path is not a macOS Mach-O library ($description)"
}

CHECK_CONFIG="${SMOLWORLD_CHECK_CONFIG:-}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)
            [[ -n "${2:-}" ]] || fail "--check requires a configuration path"
            CHECK_CONFIG="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            fail "unknown argument '$1'"
            ;;
    esac
done

[[ "$(uname -s)" == "Darwin" ]] || fail "this local installer targets macOS"
[[ "$(uname -m)" == "arm64" ]] || fail "this local installer targets Apple Silicon (arm64)"

require_command cargo
require_command codesign
require_command file
require_command grep
require_command head
require_command install
require_command make
require_command mktemp
require_command mkfs.ext4
require_command nm

HOME_DIR="${HOME:-}"
[[ -n "$HOME_DIR" ]] || fail "HOME is not set"

SMOLVM_SOURCE_DIR="${SMOLVM_SOURCE_DIR:-$PROJECT_ROOT/../smolvm}"
SMOLVM_SOURCE_DIR="$(resolve_existing_dir "$SMOLVM_SOURCE_DIR")" \
    || fail "patched smolvm checkout not found; set SMOLVM_SOURCE_DIR"

SMOLVM_LIB_DIR="${SMOLVM_LIB_DIR:-$SMOLVM_SOURCE_DIR/lib}"
if [[ "$SMOLVM_LIB_DIR" == /* ]]; then
    :
elif [[ -d "$SMOLVM_LIB_DIR" ]]; then
    SMOLVM_LIB_DIR="$(cd "$SMOLVM_LIB_DIR" && pwd -P)"
else
    SMOLVM_LIB_DIR="$PWD/$SMOLVM_LIB_DIR"
fi

SMOLVM_AGENT_ROOTFS="${SMOLVM_AGENT_ROOTFS:-$SMOLVM_SOURCE_DIR/target/agent-rootfs}"
if [[ "$SMOLVM_AGENT_ROOTFS" != /* && -d "$SMOLVM_AGENT_ROOTFS" ]]; then
    SMOLVM_AGENT_ROOTFS="$(cd "$SMOLVM_AGENT_ROOTFS" && pwd -P)"
elif [[ "$SMOLVM_AGENT_ROOTFS" != /* ]]; then
    SMOLVM_AGENT_ROOTFS="$PWD/$SMOLVM_AGENT_ROOTFS"
fi

SMOLWORLD_BUILD_AGENT_ROOTFS="${SMOLWORLD_BUILD_AGENT_ROOTFS:-1}"
SMOLWORLD_BUILD_LIBKRUN="${SMOLWORLD_BUILD_LIBKRUN:-0}"
SMOLWORLD_LIBKRUN_DIR="${SMOLWORLD_LIBKRUN_DIR:-$SMOLVM_SOURCE_DIR/libkrun}"
SMOLWORLD_LIBKRUN_BUILD_FLAGS="${SMOLWORLD_LIBKRUN_BUILD_FLAGS:-BLK=1 NET=1 GPU=1}"
CODESIGN_IDENTITY="${CODESIGN_IDENTITY:--}"
SMOLWORLD_INSTALL_PREFIX="${SMOLWORLD_INSTALL_PREFIX:-$HOME_DIR/.local/smolworld}"

case "$SMOLWORLD_BUILD_AGENT_ROOTFS" in 0|1) ;; *) fail "SMOLWORLD_BUILD_AGENT_ROOTFS must be 0 or 1" ;; esac
case "$SMOLWORLD_BUILD_LIBKRUN" in 0|1) ;; *) fail "SMOLWORLD_BUILD_LIBKRUN must be 0 or 1" ;; esac

if [[ "$SMOLWORLD_INSTALL_PREFIX" != /* ]]; then
    SMOLWORLD_INSTALL_PREFIX="$PWD/$SMOLWORLD_INSTALL_PREFIX"
fi
INSTALL_PARENT="$(dirname "$SMOLWORLD_INSTALL_PREFIX")"
INSTALL_MARKER="$SMOLWORLD_INSTALL_PREFIX/.smolworld-local-install-v1"

[[ -f "$SMOLVM_SOURCE_DIR/Cargo.toml" ]] || fail "$SMOLVM_SOURCE_DIR is not a smolvm source checkout"
[[ -f "$SMOLVM_SOURCE_DIR/smolvm.entitlements" ]] \
    || fail "smolvm entitlements file is missing: $SMOLVM_SOURCE_DIR/smolvm.entitlements"
[[ -f "$SMOLVM_SOURCE_DIR/scripts/smolvm-wrapper.sh" ]] \
    || fail "smolvm runtime wrapper is missing: $SMOLVM_SOURCE_DIR/scripts/smolvm-wrapper.sh"
[[ -f "$PROJECT_ROOT/scripts/smolworld-wrapper.sh" ]] \
    || fail "smolworld runtime wrapper is missing: $PROJECT_ROOT/scripts/smolworld-wrapper.sh"

AGENT_BUILD_DIR=""
STAGE_DIR=""
PREVIOUS_DIR=""

cleanup() {
    local status=$?
    trap - EXIT
    if [[ -n "$STAGE_DIR" && -d "$STAGE_DIR" ]]; then
        rm -rf -- "$STAGE_DIR"
    fi
    if [[ -n "$AGENT_BUILD_DIR" && -d "$AGENT_BUILD_DIR" ]]; then
        rm -rf -- "$AGENT_BUILD_DIR"
    fi
    if [[ -n "$PREVIOUS_DIR" && -d "$PREVIOUS_DIR" ]]; then
        rm -rf -- "$PREVIOUS_DIR"
    fi
    exit "$status"
}
trap cleanup EXIT

if ! valid_rootfs "$SMOLVM_AGENT_ROOTFS"; then
    if [[ "$SMOLWORLD_BUILD_AGENT_ROOTFS" != 1 ]]; then
        fail "agent rootfs is unavailable at $SMOLVM_AGENT_ROOTFS; set SMOLVM_AGENT_ROOTFS or enable SMOLWORLD_BUILD_AGENT_ROOTFS=1"
    fi
    [[ -x "$SMOLVM_SOURCE_DIR/scripts/build-agent-rootfs.sh" ]] \
        || fail "smolvm agent-rootfs build script is unavailable; provide SMOLVM_AGENT_ROOTFS"
    mkdir -p "$SMOLVM_SOURCE_DIR/target"
    AGENT_BUILD_DIR="$(mktemp -d "$SMOLVM_SOURCE_DIR/target/.smolworld-agent-rootfs.XXXXXX")"
    info "agent rootfs not found; building into a private temporary directory"
    "$SMOLVM_SOURCE_DIR/scripts/build-agent-rootfs.sh" "$AGENT_BUILD_DIR"
    valid_rootfs "$AGENT_BUILD_DIR" \
        || fail "agent rootfs build completed without usr/local/bin/smolvm-agent"
    SMOLVM_AGENT_ROOTFS="$AGENT_BUILD_DIR"
else
    info "using agent rootfs: $SMOLVM_AGENT_ROOTFS"
fi

LIBKRUN="$SMOLVM_LIB_DIR/libkrun.dylib"
LIBKRUNFW="$SMOLVM_LIB_DIR/libkrunfw.5.dylib"

if [[ "$SMOLWORLD_BUILD_LIBKRUN" == 1 ]]; then
    require_library "$LIBKRUNFW"
    [[ -f "$SMOLWORLD_LIBKRUN_DIR/Makefile" ]] \
        || fail "pinned smolvm libkrun submodule is unavailable at $SMOLWORLD_LIBKRUN_DIR"
    grep -q '^smolvm:' "$SMOLWORLD_LIBKRUN_DIR/Makefile" \
        || fail "$SMOLWORLD_LIBKRUN_DIR/Makefile has no supported 'smolvm' build target"
    mkdir -p "$SMOLVM_LIB_DIR"
    IFS=' ' read -r -a LIBKRUN_BUILD_ARGS <<< "$SMOLWORLD_LIBKRUN_BUILD_FLAGS"
    info "building patched libkrun with $SMOLWORLD_LIBKRUN_BUILD_FLAGS"
    (
        cd "$SMOLWORLD_LIBKRUN_DIR"
        make smolvm "SMOLVM_DEST=$LIBKRUN" "${LIBKRUN_BUILD_ARGS[@]}"
    )
fi

require_library "$LIBKRUN"
require_library "$LIBKRUNFW"
if ! nm -gU "$LIBKRUN" 2>/dev/null | grep -q 'krun_add_net_unixstream'; then
    fail "$LIBKRUN does not export krun_add_net_unixstream; use the patched NET=1 libkrun bundle"
fi

SMOLVM_BINARY="$SMOLVM_SOURCE_DIR/target/release/smolvm"
info "building patched smolvm"
(
    cd "$SMOLVM_SOURCE_DIR"
    LIBKRUN_BUNDLE="$SMOLVM_LIB_DIR" cargo build --release --bin smolvm
)
[[ -x "$SMOLVM_BINARY" ]] || fail "smolvm build did not produce $SMOLVM_BINARY"

info "signing smolvm with identity '$CODESIGN_IDENTITY'"
codesign --force --sign "$CODESIGN_IDENTITY" \
    --entitlements "$SMOLVM_SOURCE_DIR/smolvm.entitlements" "$SMOLVM_BINARY"
codesign --verify --strict "$SMOLVM_BINARY"

SMOLWORLD_BINARY="$PROJECT_ROOT/target/release/smolworld"
info "building smolworld"
cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" --bin smolworld
[[ -x "$SMOLWORLD_BINARY" ]] || fail "smolworld build did not produce $SMOLWORLD_BINARY"

if [[ -n "$CHECK_CONFIG" ]]; then
    CHECK_CONFIG="$(resolve_existing_file "$CHECK_CONFIG")" \
        || fail "check configuration not found: $CHECK_CONFIG"
fi

if [[ -e "$SMOLWORLD_INSTALL_PREFIX" || -L "$SMOLWORLD_INSTALL_PREFIX" ]]; then
    [[ -d "$SMOLWORLD_INSTALL_PREFIX" ]] \
        || fail "install prefix exists but is not a directory: $SMOLWORLD_INSTALL_PREFIX"
    if [[ ! -f "$INSTALL_MARKER" ]]; then
        if find "$SMOLWORLD_INSTALL_PREFIX" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
            fail "install prefix is not owned by this installer: $SMOLWORLD_INSTALL_PREFIX (choose another prefix)"
        fi
    fi
fi

mkdir -p "$INSTALL_PARENT"
STAGE_DIR="$(mktemp -d "$INSTALL_PARENT/.smolworld-stage.XXXXXX")"
STAGE_RUNTIME="$STAGE_DIR/lib/smolworld"
mkdir -p "$STAGE_DIR/bin" "$STAGE_RUNTIME/lib" "$STAGE_RUNTIME/agent-rootfs"

install -m 0755 "$SMOLWORLD_BINARY" "$STAGE_RUNTIME/smolworld-bin"
install -m 0755 "$SMOLVM_BINARY" "$STAGE_RUNTIME/smolvm-bin"
install -m 0755 "$SMOLVM_SOURCE_DIR/scripts/smolvm-wrapper.sh" "$STAGE_RUNTIME/smolvm"
install -m 0755 "$PROJECT_ROOT/scripts/smolworld-wrapper.sh" "$STAGE_DIR/bin/smolworld"
cp -a "$SMOLVM_LIB_DIR"/. "$STAGE_RUNTIME/lib/"
cp -a "$SMOLVM_AGENT_ROOTFS"/. "$STAGE_RUNTIME/agent-rootfs/"
printf 'smolworld-local-install-v1\nsource=%s\n' "$SMOLVM_SOURCE_DIR" > "$STAGE_DIR/.smolworld-local-install-v1"

"$STAGE_RUNTIME/smolvm" --version >/dev/null
codesign --verify --strict "$STAGE_RUNTIME/smolvm-bin"

if [[ -n "$CHECK_CONFIG" ]]; then
    info "running installed smolworld check before commit"
    env -u SMOLWORLD_SMOLVM -u SMOLVM_AGENT_ROOTFS -u SMOLVM_LIB_DIR \
        "$STAGE_DIR/bin/smolworld" -f "$CHECK_CONFIG" check
fi

if [[ -e "$SMOLWORLD_INSTALL_PREFIX" || -L "$SMOLWORLD_INSTALL_PREFIX" ]]; then
    PREVIOUS_DIR="$(mktemp -d "$INSTALL_PARENT/.smolworld-previous.XXXXXX")"
    rmdir "$PREVIOUS_DIR"
    mv "$SMOLWORLD_INSTALL_PREFIX" "$PREVIOUS_DIR/installation"
fi

if ! mv "$STAGE_DIR" "$SMOLWORLD_INSTALL_PREFIX"; then
    if [[ -n "$PREVIOUS_DIR" && -d "$PREVIOUS_DIR/installation" ]]; then
        mv "$PREVIOUS_DIR/installation" "$SMOLWORLD_INSTALL_PREFIX"
    fi
    fail "could not commit installation to $SMOLWORLD_INSTALL_PREFIX"
fi
STAGE_DIR=""

if [[ -n "$PREVIOUS_DIR" && -d "$PREVIOUS_DIR" ]]; then
    rm -rf -- "$PREVIOUS_DIR"
    PREVIOUS_DIR=""
fi

info "installed smolworld at $SMOLWORLD_INSTALL_PREFIX/bin/smolworld"
info "add it to PATH with: export PATH=$SMOLWORLD_INSTALL_PREFIX/bin:\$PATH"
info "the wrapper supplies SMOLWORLD_SMOLVM, SMOLVM_AGENT_ROOTFS, SMOLVM_LIB_DIR, and the library search path"
