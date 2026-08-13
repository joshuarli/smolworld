#!/usr/bin/env bash
# Opt-in, real-VM integration coverage for macOS/Apple Silicon. It exercises
# the generic static-world contract using Redis only as a convenient workload.

set -euo pipefail

if [[ "${SMOLWORLD_E2E:-}" != "1" ]]; then
    echo "SKIP: set SMOLWORLD_E2E=1 to run the local macOS Redis integration test"
    exit 0
fi

project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
binary="$project_dir/target/debug/smolworld"
: "${SMOLWORLD_SMOLVM:?set SMOLWORLD_SMOLVM to the patched smolvm binary}"
: "${SMOLVM_AGENT_ROOTFS:?set SMOLVM_AGENT_ROOTFS to a built agent rootfs}"
: "${SMOLVM_LIB_DIR:?set SMOLVM_LIB_DIR to a libkrun/libkrunfw directory}"
: "${SMOLWORLD_REDIS_ARCHIVE:=$project_dir/examples/redis/redis.tar}"

if [[ ! -f "$SMOLWORLD_REDIS_ARCHIVE" ]]; then
    echo "missing Redis archive: $SMOLWORLD_REDIS_ARCHIVE" >&2
    echo "prepare it with: docker pull redis:8 && docker save redis:8 -o examples/redis/redis.tar" >&2
    exit 1
fi

cargo build --manifest-path "$project_dir/Cargo.toml" --quiet

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/smolworld-e2e.XXXXXX")
config="$temporary_dir/.smolworld"
up_log="$temporary_dir/up.log"
started_marker="$temporary_dir/started"
up_pid=""
state_file=""
machine_names=""

cleanup() {
    local original_status=$?
    local cleanup_failed=0
    trap - EXIT

    if [[ -n "$up_pid" ]] && kill -0 "$up_pid" 2>/dev/null; then
        kill -INT "$up_pid" 2>/dev/null || cleanup_failed=1
    fi
    if [[ -n "$up_pid" ]]; then
        wait "$up_pid" || cleanup_failed=1
    fi

    if [[ -n "$state_file" && -f "$state_file" ]]; then
        machine_names=$(awk -F '\t' '$1 == "machine" { print $5 }' "$state_file")
        local machines
        machines=$("$SMOLWORLD_SMOLVM" machine ls --json) || cleanup_failed=1
        while IFS= read -r machine; do
            [[ -z "$machine" ]] && continue
            if grep -Fq "$machine" <<<"$machines"; then
                echo "E2E cleanup left smolvm machine: $machine" >&2
                cleanup_failed=1
            fi
        done <<<"$machine_names"

        local world_hash runtime_dir
        world_hash=${state_file%/state}
        world_hash=${world_hash##*-}
        runtime_dir="/tmp/smw-$world_hash"
        if [[ -e "$runtime_dir" ]]; then
            echo "E2E cleanup left runtime directory: $runtime_dir" >&2
            cleanup_failed=1
        fi
    fi

    # `up` normally owns all cleanup. `down` makes an interrupted harness
    # recoverable without touching machines outside this world state.
    "$binary" -f "$config" down >/dev/null 2>&1 || cleanup_failed=1
    rm -rf -- "$temporary_dir"

    if (( original_status == 0 && cleanup_failed != 0 )); then
        exit 1
    fi
    exit "$original_status"
}
trap cleanup EXIT

cat >"$config" <<EOF
[world]
name = "e2e-cache"

[network]
subnet = "10.94.0.0/24"
gateway = "10.94.0.1"
dns = "10.94.0.1"
domain = "e2e.test"

[machines.cache]
image = "$SMOLWORLD_REDIS_ARCHIVE"
command = ["redis-server"]
cpus = 1
memory_mib = 256
storage_gib = 1
overlay_gib = 1

[machines.client]
image = "$SMOLWORLD_REDIS_ARCHIVE"
command = ["sleep", "infinity"]
depends_on = ["cache"]
EOF

"$binary" -f "$config" check
touch "$started_marker"
"$binary" -f "$config" up >"$up_log" 2>&1 &
up_pid=$!

ready=false
for _ in $(seq 1 120); do
    if grep -Fq "world is up" "$up_log"; then
        ready=true
        break
    fi
    if ! kill -0 "$up_pid" 2>/dev/null; then
        cat "$up_log" >&2
        exit 1
    fi
    sleep 0.25
done
if [[ "$ready" != true ]]; then
    cat "$up_log" >&2
    echo "timed out waiting for the world to attach both virtio NICs" >&2
    exit 1
fi

state_file=$(find "$HOME/.smolworld" -type f -name state -newer "$started_marker" -print -quit)
if [[ -z "$state_file" ]]; then
    echo "could not locate the E2E world state" >&2
    exit 1
fi

"$binary" -f "$config" exec client -- getent hosts cache.e2e.test | grep -Fq "10.94.0."

pong=false
for _ in $(seq 1 30); do
    if "$binary" -f "$config" exec client -- redis-cli -h cache ping | grep -qx "PONG"; then
        pong=true
        break
    fi
    sleep 0.5
done
if [[ "$pong" != true ]]; then
    cat "$up_log" >&2
    echo "Redis never accepted a connection through the real virtio network" >&2
    exit 1
fi

echo "PASS: DNS and Redis PONG crossed the real smolworld virtio network"
