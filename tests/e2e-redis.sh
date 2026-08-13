#!/usr/bin/env bash
# Opt-in, real-VM integration coverage for macOS/Apple Silicon. It exercises
# the generic static-world contract using Redis only as a convenient workload.
#
# The cases intentionally use separate worlds and an isolated HOME. That keeps
# every generated smolvm name, state file, socket directory, and cleanup action
# scoped to this test process.

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
export HOME="$temporary_dir/home"
mkdir -p "$HOME"

current_config=""
current_up_log=""
current_up_pid=""
current_state_file=""

state_for_marker() {
    local marker="$1"
    local state_file
    state_file=$(find "$HOME/.smolworld" -type f -name state -newer "$marker" -print -quit 2>/dev/null || true)
    if [[ -z "$state_file" ]]; then
        echo "could not locate the world state newer than $marker" >&2
        return 1
    fi
    printf '%s\n' "$state_file"
}

runtime_dir_for_state() {
    local state_file="$1"
    local state_dir world_hash
    state_dir=${state_file%/state}
    world_hash=${state_dir##*-}
    printf '/tmp/smw-%s\n' "$world_hash"
}

machine_names_for_state() {
    local state_file="$1"
    awk -F '\t' '$1 == "machine" { print $5 }' "$state_file"
}

assert_world_machines_absent() {
    local state_file="$1"
    local machines machine
    machines=$("$SMOLWORLD_SMOLVM" machine ls --json 2>/dev/null) || {
        echo "could not inspect smolvm machines during cleanup" >&2
        return 1
    }
    while IFS= read -r machine; do
        [[ -z "$machine" ]] && continue
        if grep -Fq "$machine" <<<"$machines"; then
            echo "world cleanup left smolvm machine: $machine" >&2
            return 1
        fi
    done < <(machine_names_for_state "$state_file")
}

assert_ps_running() {
    local config="$1"
    shift
    local ps_output machine
    ps_output=$("$binary" -f "$config" ps) || {
        echo "smolworld ps failed" >&2
        return 1
    }
    for machine in "$@"; do
        if ! awk -F '\t' -v expected="$machine" \
            '$1 == expected && $4 ~ /running/' <<<"$ps_output"; then
            echo "smolworld ps did not report $machine as running:" >&2
            echo "$ps_output" >&2
            return 1
        fi
    done
}

assert_ps_json_running() {
    local config="$1"
    local ps_output
    ps_output=$("$binary" -f "$config" ps --json) || {
        echo "smolworld ps --json failed" >&2
        return 1
    }
    if ! grep -Fq '"status":"running"' <<<"$ps_output"; then
        echo "smolworld ps --json did not report a running machine:" >&2
        echo "$ps_output" >&2
        return 1
    fi
}

line_number() {
    local pattern="$1"
    local log="$2"
    local line
    line=$(grep -nF "$pattern" "$log" | head -1 | cut -d: -f1 || true)
    printf '%s\n' "$line"
}

assert_log_order() {
    local log="$1"
    local earlier="$2"
    local later="$3"
    local earlier_line later_line
    earlier_line=$(line_number "$earlier" "$log")
    later_line=$(line_number "$later" "$log")
    if [[ -z "$earlier_line" || -z "$later_line" ]]; then
        echo "could not find lifecycle markers in $log:" >&2
        echo "  earlier: $earlier" >&2
        echo "  later:   $later" >&2
        return 1
    fi
    if (( earlier_line >= later_line )); then
        echo "lifecycle order was wrong in $log:" >&2
        echo "  $earlier ($earlier_line)" >&2
        echo "  $later ($later_line)" >&2
        return 1
    fi
}

wait_for_world_up() {
    local pid="$1"
    local log="$2"
    local status
    for _ in $(seq 1 120); do
        if grep -Fq "world is up" "$log"; then
            return 0
        fi
        if ! process_running "$pid"; then
            status=0
            if wait "$pid"; then
                status=0
            else
                status=$?
            fi
            echo "smolworld up exited before the world was ready (status $status)" >&2
            cat "$log" >&2
            return 1
        fi
        sleep 0.25
    done
    echo "timed out waiting for the world to attach all virtio NICs" >&2
    cat "$log" >&2
    return 1
}

process_running() {
    local pid="$1"
    local process_state
    if ! kill -0 "$pid" 2>/dev/null; then
        return 1
    fi
    # A completed child remains as a zombie until wait reaps it. Treat that as
    # exited so a failed-up case cannot block forever on a second wait.
    process_state=$(ps -p "$pid" -o stat= 2>/dev/null | awk 'NF { print $1; exit }' || true)
    [[ -n "$process_state" && "$process_state" != Z* ]]
}

begin_world() {
    local name="$1"
    local case_dir="$temporary_dir/$name"
    mkdir -p "$case_dir"
    current_config="$case_dir/.smolworld"
    current_up_log="$case_dir/up.log"
    current_up_pid=""
    current_state_file=""
}

start_world() {
    local marker
    marker="$temporary_dir/$(basename "$(dirname "$current_config")").state-marker.$RANDOM"
    "$binary" -f "$current_config" check
    touch "$marker"
    "$binary" -f "$current_config" up >"$current_up_log" 2>&1 &
    current_up_pid=$!
    wait_for_world_up "$current_up_pid" "$current_up_log"
    current_state_file=$(state_for_marker "$marker")
}

stop_active_up() {
    local pid="$current_up_pid"
    local status
    [[ -z "$pid" ]] && return 0
    current_up_pid=""
    if kill -0 "$pid" 2>/dev/null; then
        kill -INT "$pid" 2>/dev/null || true
    fi
    status=0
    if wait "$pid"; then
        status=0
    else
        status=$?
    fi
    if (( status != 0 )); then
        echo "smolworld up did not stop cleanly (status $status)" >&2
        return 1
    fi
}

cleanup_world() {
    local failed=0
    local runtime_dir

    if ! stop_active_up; then
        failed=1
    fi

    if [[ -n "$current_config" ]]; then
        # `down` is intentionally the only broad-looking operation here. The
        # Rust runtime derives deterministic smw-* names from this config's
        # state and deletes only those names.
        if ! "$binary" -f "$current_config" down >/dev/null 2>&1; then
            failed=1
        fi
    fi

    if [[ -n "$current_state_file" && -f "$current_state_file" ]]; then
        if ! assert_world_machines_absent "$current_state_file"; then
            failed=1
        fi
        runtime_dir=$(runtime_dir_for_state "$current_state_file")
        if [[ -e "$runtime_dir" ]]; then
            echo "cleanup left runtime directory: $runtime_dir" >&2
            failed=1
        fi
    fi

    if (( failed == 0 )); then
        current_config=""
        current_up_log=""
        current_state_file=""
    fi
    return "$failed"
}

cleanup() {
    local original_status=$?
    local cleanup_failed=0
    trap - EXIT

    if ! cleanup_world; then
        cleanup_failed=1
    fi
    if [[ -e "$temporary_dir" ]]; then
        rm -rf -- "$temporary_dir" || cleanup_failed=1
    fi

    if (( original_status == 0 && cleanup_failed != 0 )); then
        exit 1
    fi
    exit "$original_status"
}
trap cleanup EXIT

run_happy_path() {
    begin_world happy
    cat >"$current_config" <<EOF
world:
  name: e2e-cache
network:
  subnet: 10.94.0.0/24
  gateway: 10.94.0.1
  dns: 10.94.0.1
  domain: e2e.test
machines:
  cache:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    command: [redis-server]
    cpus: 1
    memory_mib: 256
    storage_gib: 1
    overlay_gib: 1
  client:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    command: [sleep, infinity]
    depends_on: [cache]
EOF

    start_world
    "$binary" -f "$current_config" exec client -- getent hosts cache.e2e.test | grep -Fq "10.94.0."

    local pong=false
    for _ in $(seq 1 30); do
        if "$binary" -f "$current_config" exec client -- redis-cli -h cache ping | grep -qx "PONG"; then
            pong=true
            break
        fi
        sleep 0.5
    done
    if [[ "$pong" != true ]]; then
        cat "$current_up_log" >&2
        echo "Redis never accepted a connection through the real virtio network" >&2
        return 1
    fi
    assert_ps_running "$current_config" cache client
    assert_ps_json_running "$current_config"
    echo "PASS: DNS and Redis PONG crossed the real smolworld virtio network"
    cleanup_world
}

run_custom_domain_gateway() {
    begin_world custom-domain-gateway
    cat >"$current_config" <<EOF
world:
  name: e2e-custom
network:
  subnet: 10.95.0.0/24
  gateway: 10.95.0.9
  dns: 10.95.0.9
  domain: custom.e2e.test
machines:
  cache:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    command: [redis-server]
  client:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    command: [sleep, infinity]
    depends_on: [cache]
EOF

    start_world
    if awk -F '\t' '$1 == "machine" && $3 == "10.95.0.9" { exit 1 }' "$current_state_file"; then
        :
    else
        echo "custom gateway address was allocated to a machine" >&2
        return 1
    fi
    "$binary" -f "$current_config" exec client -- cat /etc/resolv.conf | \
        grep -Eq '(^|[[:space:]])nameserver[[:space:]]+10\.95\.0\.9([[:space:]]|$)'
    "$binary" -f "$current_config" exec client -- getent hosts cache.custom.e2e.test | \
        grep -Fq "10.95.0."

    # There is no route-inspection command in the smolworld CLI, so the
    # configured nameserver plus a successful authoritative lookup are the
    # strongest portable guest-side checks of this custom gateway/DNS tuple.
    echo "PASS: custom domain and gateway reached the authoritative DNS service"
    cleanup_world
}

run_dependency_order() {
    local zcache_vm client_vm
    begin_world dependency-order
    cat >"$current_config" <<EOF
world:
  name: e2e-order
network:
  subnet: 10.96.0.0/24
  domain: order.e2e.test
machines:
  # Keep the dependent name first lexically. A successful create/start log order
  # therefore observes topological ordering rather than BTreeMap key ordering.
  client:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    command: [sleep, infinity]
    depends_on: [zcache]
  zcache:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    command: [redis-server]
EOF

    start_world
    zcache_vm=$(awk -F '\t' '$1 == "machine" && $2 == "zcache" { print $5 }' "$current_state_file")
    client_vm=$(awk -F '\t' '$1 == "machine" && $2 == "client" { print $5 }' "$current_state_file")
    assert_log_order "$current_up_log" "Created machine: $zcache_vm" "Created machine: $client_vm"
    assert_log_order "$current_up_log" "Starting machine '$zcache_vm'" "Starting machine '$client_vm'"
    "$binary" -f "$current_config" exec client -- redis-cli -h zcache ping | grep -qx "PONG"
    assert_ps_running "$current_config" zcache client

    # `depends_on` is deliberately tested as creation/start order only. The
    # current contract has no readiness or health semantics to assert here.
    echo "PASS: depends_on created and started the dependency before its client"
    cleanup_world
}

run_startup_failure_cleanup() {
    local marker status runtime_dir ready_vm broken_vm completed
    begin_world startup-failure
    cat >"$current_config" <<EOF
world:
  name: e2e-startup-failure
network:
  subnet: 10.97.0.0/24
  domain: failure.e2e.test
machines:
  ready:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    command: [sleep, infinity]
  zbroken:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    # This command is accepted by smolworld's local-image contract, then makes
    # smolvm's existing machine start boundary fail during workload launch.
    command: [/smolworld-e2e-command-does-not-exist]
EOF

    "$binary" -f "$current_config" check
    marker="$temporary_dir/startup-failure.state-marker"
    touch "$marker"
    "$binary" -f "$current_config" up >"$current_up_log" 2>&1 &
    current_up_pid=$!
    completed=false
    for _ in $(seq 1 240); do
        if ! process_running "$current_up_pid"; then
            completed=true
            break
        fi
        sleep 0.25
    done
    if [[ "$completed" != true ]]; then
        kill -KILL "$current_up_pid" 2>/dev/null || true
        wait "$current_up_pid" 2>/dev/null || true
        current_up_pid=""
        cat "$current_up_log" >&2
        echo "startup-failure world did not fail within 60 seconds" >&2
        return 1
    fi
    status=0
    if wait "$current_up_pid"; then
        status=0
    else
        status=$?
    fi
    current_up_pid=""
    if (( status == 0 )); then
        cat "$current_up_log" >&2
        echo "startup-failure world unexpectedly started successfully" >&2
        return 1
    fi
    current_state_file=$(state_for_marker "$marker")
    ready_vm=$(awk -F '\t' '$1 == "machine" && $2 == "ready" { print $5 }' "$current_state_file")
    broken_vm=$(awk -F '\t' '$1 == "machine" && $2 == "zbroken" { print $5 }' "$current_state_file")
    assert_log_order "$current_up_log" "Created machine: $ready_vm" "Created machine: $broken_vm"
    assert_log_order "$current_up_log" "Starting machine '$ready_vm'" "Starting machine '$broken_vm'"
    if ! assert_world_machines_absent "$current_state_file"; then
        cat "$current_up_log" >&2
        echo "failed up left a world machine behind" >&2
        return 1
    fi
    runtime_dir=$(runtime_dir_for_state "$current_state_file")
    if [[ -e "$runtime_dir" ]]; then
        cat "$current_up_log" >&2
        echo "failed up left the world runtime directory: $runtime_dir" >&2
        return 1
    fi

    # A failed up has already run its own world-scoped cleanup; this final
    # down is an idempotent harness guard for any partial cleanup path.
    echo "PASS: startup failure removed all machines and runtime sockets"
    cleanup_world
}

run_interrupted_rerun() {
    local old_pid old_status runtime_dir
    begin_world interrupted-rerun
    cat >"$current_config" <<EOF
world:
  name: e2e-interrupted
network:
  subnet: 10.98.0.0/24
  domain: interrupted.e2e.test
machines:
  cache:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    command: [redis-server]
  client:
    image: "$SMOLWORLD_REDIS_ARCHIVE"
    command: [sleep, infinity]
    depends_on: [cache]
EOF

    start_world
    "$binary" -f "$current_config" exec client -- getent hosts cache.interrupted.e2e.test | \
        grep -Fq "10.98.0."
    runtime_dir=$(runtime_dir_for_state "$current_state_file")
    if [[ ! -d "$runtime_dir" ]]; then
        echo "world runtime directory was not present before interruption" >&2
        return 1
    fi

    # SIGKILL models a process interruption that cannot run Rust cleanup. The
    # next up must recover its recorded deterministic machines and stale socket
    # directory before it binds fresh listeners.
    old_pid="$current_up_pid"
    current_up_pid=""
    if ! process_running "$old_pid"; then
        wait "$old_pid" 2>/dev/null || true
        echo "foreground up exited before the interruption was injected" >&2
        return 1
    fi
    kill -KILL "$old_pid" 2>/dev/null || true
    old_status=0
    if wait "$old_pid"; then
        old_status=0
    else
        old_status=$?
    fi
    if (( old_status == 0 )); then
        echo "SIGKILL did not interrupt the foreground up process" >&2
        return 1
    fi
    if [[ ! -d "$runtime_dir" ]]; then
        echo "interruption did not leave the expected stale runtime directory" >&2
        return 1
    fi

    start_world
    "$binary" -f "$current_config" exec client -- getent hosts cache.interrupted.e2e.test | \
        grep -Fq "10.98.0."
    "$binary" -f "$current_config" exec client -- redis-cli -h cache ping | grep -qx "PONG"
    assert_ps_running "$current_config" cache client
    echo "PASS: rerunning up recovered the interrupted world and Redis PONG"
    cleanup_world
}

run_happy_path
run_custom_domain_gateway
run_dependency_order
run_startup_failure_cleanup
run_interrupted_rerun

echo "PASS: all real-VM Redis lifecycle cases"
