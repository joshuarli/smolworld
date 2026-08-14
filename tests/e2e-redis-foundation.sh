#!/usr/bin/env bash
# Opt-in, real-VM coverage for the two-machine Smolfile foundation gate.
#
# This harness intentionally starts from a temporary copy of the fixture so
# `prepare` can write its lock/material records without changing the checkout.
# The only image input is an already prepared local archive. No Docker, Compose,
# OrbStack, or guest image pull is part of this test.

set -euo pipefail

if [[ "${SMOLWORLD_E2E:-}" != "1" ]]; then
    echo "SKIP: set SMOLWORLD_E2E=1 to run the local Redis foundation integration test"
    exit 0
fi

project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fixture_dir="$project_dir/examples/redis"
binary="$project_dir/target/debug/smolworld"
: "${SMOLWORLD_SMOLVM:?set SMOLWORLD_SMOLVM to the patched smolvm binary}"
: "${SMOLVM_AGENT_ROOTFS:?set SMOLVM_AGENT_ROOTFS to a built agent rootfs}"
: "${SMOLVM_LIB_DIR:?set SMOLVM_LIB_DIR to the matching libkrun/libkrunfw directory}"
redis_archive="${SMOLWORLD_REDIS_ARCHIVE:-$fixture_dir/redis.tar}"

if [[ ! -f "$redis_archive" ]]; then
    echo "missing prepared Redis archive: $redis_archive" >&2
    echo "provide host-prepared local image material with SMOLWORLD_REDIS_ARCHIVE;" >&2
    echo "this foundation harness does not invoke Docker to create it" >&2
    exit 1
fi

# The gate is explicitly Dockerless. Fail early if the caller accidentally
# brings a Docker endpoint into the process environment; no runtime path below
# should inspect or use it.
for forbidden_env in DOCKER_HOST DOCKER_CONTEXT DOCKER_SOCKET ORBCTL_HOST; do
    if [[ -n "${!forbidden_env:-}" ]]; then
        echo "foundation gate must run without $forbidden_env" >&2
        exit 1
    fi
done

cargo build --manifest-path "$project_dir/Cargo.toml" --quiet

temporary_dir=$(mktemp -d "${TMPDIR:-/tmp}/smolworld-redis-foundation.XXXXXX")
isolated_home="$temporary_dir/home"
world_dir="$temporary_dir/world"
export HOME="$isolated_home"
mkdir -p "$isolated_home" "$world_dir/smol"

cp "$fixture_dir/.smolworld" "$world_dir/.smolworld"
cp "$fixture_dir/smol/redis.Smolfile" "$world_dir/smol/redis.Smolfile"
cp "$fixture_dir/smol/runner.Smolfile" "$world_dir/smol/runner.Smolfile"
# Keep Smolfile paths stable while allowing callers to supply another prepared
# archive. The symlink is inside the temporary fixture and is never a guest
# mount or mutable workload input.
ln -s "$redis_archive" "$world_dir/redis.tar"

world_file="$world_dir/.smolworld"
up_log="$temporary_dir/up.log"
up_pid=""
state_file=""

process_running() {
    local pid="$1"
    local process_state
    if ! kill -0 "$pid" 2>/dev/null; then
        return 1
    fi
    process_state=$(ps -p "$pid" -o stat= 2>/dev/null | awk 'NF { print $1; exit }' || true)
    [[ -n "$process_state" && "$process_state" != Z* ]]
}

wait_for_world_up() {
    local pid="$1"
    for _ in $(seq 1 240); do
        if grep -Fq "world is up" "$up_log"; then
            return 0
        fi
        if ! process_running "$pid"; then
            echo "smolworld up exited before the foundation world was ready" >&2
            cat "$up_log" >&2
            return 1
        fi
        sleep 0.25
    done
    echo "timed out waiting for the Redis foundation world" >&2
    cat "$up_log" >&2
    return 1
}

find_state_file() {
    find "$isolated_home/.smolworld" -type f -name state -print -quit 2>/dev/null || true
}

machine_names_for_state() {
    local file="$1"
    awk -F '\t' '$1 == "machine" { print $5 }' "$file"
}

runtime_dir_for_state() {
    local file="$1"
    local state_dir world_hash
    state_dir=${file%/state}
    world_hash=${state_dir##*-}
    # v2 owns a distinct runtime namespace so this gate never adopts or
    # removes a v1 runtime directory with the same canonical config hash.
    printf '/tmp/smw-v2-%s\n' "$world_hash"
}

assert_world_machines_absent() {
    local file="$1"
    local machines machine
    machines=$("$SMOLWORLD_SMOLVM" machine ls --json 2>/dev/null) || {
        echo "could not inspect smolvm machines during foundation cleanup" >&2
        return 1
    }
    while IFS= read -r machine; do
        [[ -z "$machine" ]] && continue
        if grep -Fq "$machine" <<<"$machines"; then
            echo "foundation cleanup left smolvm machine: $machine" >&2
            return 1
        fi
    done < <(machine_names_for_state "$file")
}

stop_world_process() {
    local status=0
    [[ -z "$up_pid" ]] && return 0
    if process_running "$up_pid"; then
        kill -INT "$up_pid" 2>/dev/null || true
    fi
    if wait "$up_pid"; then
        status=0
    else
        status=$?
    fi
    up_pid=""
    if (( status != 0 )); then
        echo "smolworld up did not stop cleanly (status $status)" >&2
        return 1
    fi
}

cleanup() {
    local original_status=$?
    local cleanup_failed=0
    local runtime_dir
    trap - EXIT

    if ! stop_world_process; then
        cleanup_failed=1
    fi
    if [[ -f "$world_file" ]]; then
        if ! "$binary" -f "$world_file" down >/dev/null 2>&1; then
            cleanup_failed=1
        fi
    fi
    if [[ -n "$state_file" && -f "$state_file" ]]; then
        if ! assert_world_machines_absent "$state_file"; then
            cleanup_failed=1
        fi
        runtime_dir=$(runtime_dir_for_state "$state_file")
        if [[ -e "$runtime_dir" ]]; then
            echo "foundation cleanup left runtime directory: $runtime_dir" >&2
            cleanup_failed=1
        fi
    fi
    rm -rf -- "$temporary_dir" || cleanup_failed=1

    if (( original_status == 0 && cleanup_failed != 0 )); then
        exit 1
    fi
    exit "$original_status"
}
trap cleanup EXIT

# `prepare` is the only mutating host-material operation. It must not create
# world allocation state, listeners, or smolvm machines.
"$binary" -f "$world_file" prepare
if [[ -e "$isolated_home/.smolworld" ]]; then
    echo "prepare unexpectedly created a world state directory" >&2
    exit 1
fi

# check is read-only after preparation and must preserve that boundary.
"$binary" -f "$world_file" check
if [[ -e "$isolated_home/.smolworld" ]]; then
    echo "check created a world state directory" >&2
    exit 1
fi

"$binary" -f "$world_file" up >"$up_log" 2>&1 &
up_pid=$!
wait_for_world_up "$up_pid"
state_file=$(find_state_file)
if [[ -z "$state_file" ]]; then
    echo "could not locate foundation world state after up" >&2
    cat "$up_log" >&2
    exit 1
fi

# The runner performs the workload-side checks explicitly. DNS is tested by
# both short name and fully qualified name; Redis TCP is tested with redis-cli.
"$binary" -f "$world_file" exec runner -- getent hosts redis | grep -Fq "10.89.0."
"$binary" -f "$world_file" exec runner -- getent hosts redis.redis-foundation.test | \
    grep -Fq "10.89.0."

redis_ready=false
for _ in $(seq 1 60); do
    if "$binary" -f "$world_file" exec runner -- redis-cli -h redis ping | grep -qx "PONG"; then
        redis_ready=true
        break
    fi
    sleep 0.5
done
if [[ "$redis_ready" != true ]]; then
    cat "$up_log" >&2
    echo "runner could not reach Redis over the private virtio network" >&2
    exit 1
fi

echo "PASS: prepared Smolfiles, read-only check, private DNS, Redis PONG, and world cleanup gate"
