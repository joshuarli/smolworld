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
# A source-built smolvm is dynamically linked to libkrun. Keep the loaded
# library pair identical to the pair that `check` validated, regardless of a
# caller's existing shell environment.
export DYLD_LIBRARY_PATH="$SMOLVM_LIB_DIR${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
redis_archive="${SMOLWORLD_REDIS_ARCHIVE:-$fixture_dir/redis.tar}"

if [[ ! -f "$redis_archive" ]]; then
    echo "missing prepared Redis archive: $redis_archive" >&2
    echo "provide host-prepared local image material with SMOLWORLD_REDIS_ARCHIVE;" >&2
    echo "this foundation harness does not invoke Docker to create it" >&2
    exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "foundation gate requires python3 for standard-library JSON assertions" >&2
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
mkdir -p "$world_dir/seed"
printf '%s\n' 'smolworld sealed seed' >"$world_dir/seed/runner-message"
# Add one sealed regular file to the temporary fixture. This is a generic
# world capability assertion, not Redis configuration: its content and mode
# must survive material sealing and be present before the runner workload is
# used.
python3 - "$world_dir/.smolworld" <<'PY'
import os
import sys

path = sys.argv[1]
text = open(path, encoding="utf-8").read()
if os.environ.get("SMOLWORLD_E2E_EGRESS") == "1":
    text = text.replace(
        "  domain: redis-foundation.test\n",
        "  domain: redis-foundation.test\n  egress: true\n",
        1,
    )
needle = "  runner:\n    smolfile: ./smol/runner.Smolfile\n    depends_on: [redis]\n"
replacement = needle + "    seed_files:\n      - source: ./seed/runner-message\n        destination: /tmp/smolworld-e2e-seed\n        mode: \"0640\"\n"
if text.count(needle) != 1:
    raise SystemExit("Redis foundation fixture has no unique runner declaration")
with open(path, "w", encoding="utf-8") as output:
    output.write(text.replace(needle, replacement))
PY
# Keep Smolfile paths stable while allowing callers to supply another prepared
# archive. The symlink is inside the temporary fixture and is never a guest
# mount or mutable workload input.
ln -s "$redis_archive" "$world_dir/redis.tar"

world_file="$world_dir/.smolworld"
up_log="$temporary_dir/up.log"
up_pid=""
state_file=""
baseline_machines="$temporary_dir/machines-before.json"
state_assignments="$temporary_dir/assignments.json"

# Keep the runtime namespace calculation in lockstep with `world_paths`.
# It lets the read-only preparation checks prove that no world listener can
# exist, rather than merely proving that no allocation state was written.
world_hash=$(python3 - "$world_file" <<'PY'
import os
import sys

value = 0xcbf29ce484222325
for byte in os.fsencode(os.path.realpath(sys.argv[1])):
    value ^= byte
    value = (value * 0x100000001b3) & 0xffffffffffffffff
print(format(value, "012x"))
PY
)
expected_state_dir="$isolated_home/.smolworld/world-$world_hash"
runtime_dir="/tmp/smw-$world_hash"

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
    local candidate=""
    local count=0
    while IFS= read -r found; do
        candidate="$found"
        count=$((count + 1))
    done < <(find "$isolated_home/.smolworld" -type f -name state -print 2>/dev/null || true)
    if (( count != 1 )); then
        echo "expected exactly one foundation allocation state, found $count" >&2
        return 1
    fi
    printf '%s\n' "$candidate"
}

capture_smolvm_machines() {
    local destination="$1"
    "$SMOLWORLD_SMOLVM" machine ls --json >"$destination"
    python3 - "$destination" <<'PY'
import json
import sys

path = sys.argv[1]
try:
    records = json.load(open(path, encoding="utf-8"))
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"smolvm machine ls did not emit JSON: {error}")
if not isinstance(records, list):
    raise SystemExit("smolvm machine ls JSON must be an array")
names = set()
for record in records:
    if not isinstance(record, dict) or not isinstance(record.get("name"), str):
        raise SystemExit("smolvm machine ls JSON row must contain a string name")
    if record["name"] in names:
        raise SystemExit(f"smolvm machine ls repeats identity {record['name']!r}")
    names.add(record["name"])
PY
}

assert_machine_names_equal() {
    local expected="$1"
    local actual="$2"
    local phase="$3"
    python3 - "$expected" "$actual" "$phase" <<'PY'
import json
import sys

def names(path):
    records = json.load(open(path, encoding="utf-8"))
    return {record["name"] for record in records}

expected, actual = names(sys.argv[1]), names(sys.argv[2])
if actual != expected:
    raise SystemExit(
        f"{sys.argv[3]} changed unrelated smolvm identities: "
        f"expected {sorted(expected)}, got {sorted(actual)}"
    )
PY
}

parse_allocation_state() {
    local source="$1"
    local destination="$2"
    python3 - "$source" "$destination" <<'PY'
import ipaddress
import json
import re
import sys

source, destination = sys.argv[1:]
records = {}
version = seed = None
for raw in open(source, encoding="utf-8"):
    line = raw.rstrip("\n")
    if not line:
        continue
    fields = line.split("\t")
    if fields[0] == "version" and len(fields) == 2 and version is None:
        version = fields[1]
    elif fields[0] == "seed" and len(fields) == 2 and seed is None:
        seed = fields[1]
    elif fields[0] == "machine" and len(fields) == 5 and fields[1] not in records:
        records[fields[1]] = {"ip": fields[2], "mac": fields[3], "smolvmName": fields[4]}
    else:
        raise SystemExit(f"malformed or repeated allocation-state line: {line!r}")
if version != "2" or seed is None or not re.fullmatch(r"[0-9a-f]{16}", seed):
    raise SystemExit("foundation allocation state is not a complete world record")
if set(records) != {"redis", "runner"}:
    raise SystemExit(f"foundation allocation state identities are {sorted(records)}, not redis and runner")
ips, macs = set(), set()
subnet = ipaddress.ip_network("10.89.0.0/24")
for machine, record in records.items():
    ip = ipaddress.ip_address(record["ip"])
    if ip not in subnet or ip in {subnet.network_address, subnet.broadcast_address, ipaddress.ip_address("10.89.0.1")}:
        raise SystemExit(f"{machine} has an invalid reserved/private address {ip}")
    if not re.fullmatch(r"[0-9a-f]{2}(?::[0-9a-f]{2}){5}", record["mac"]):
        raise SystemExit(f"{machine} has an invalid MAC {record['mac']!r}")
    if not record["smolvmName"].startswith("smw-") or any(c in record["smolvmName"] for c in "\t\r\n/"):
        raise SystemExit(f"{machine} has an invalid world smolvm identity {record['smolvmName']!r}")
    ips.add(record["ip"])
    macs.add(record["mac"])
if len(ips) != len(records) or len(macs) != len(records):
    raise SystemExit("foundation allocation state reuses an IP or MAC")
with open(destination, "w", encoding="utf-8") as output:
    json.dump(records, output, sort_keys=True)
PY
}

assert_same_allocations() {
    local previous="$1"
    local current="$2"
    python3 - "$previous" "$current" <<'PY'
import json
import sys

previous = json.load(open(sys.argv[1], encoding="utf-8"))
current = json.load(open(sys.argv[2], encoding="utf-8"))
if current != previous:
    raise SystemExit(f"world restart changed recorded IP/MAC/smolvm assignments: {previous!r} -> {current!r}")
PY
}

assert_baseline_plus_world_machines() {
    local actual="$1"
    python3 - "$baseline_machines" "$state_assignments" "$actual" <<'PY'
import json
import sys

baseline = {record["name"] for record in json.load(open(sys.argv[1], encoding="utf-8"))}
assignments = json.load(open(sys.argv[2], encoding="utf-8"))
world = {record["smolvmName"] for record in assignments.values()}
actual = {record["name"] for record in json.load(open(sys.argv[3], encoding="utf-8"))}
if baseline & world:
    raise SystemExit(f"foundation allocation adopted an unrelated smolvm identity: {sorted(baseline & world)}")
if actual != baseline | world:
    raise SystemExit(
        f"smolvm identities after world up are not exactly baseline plus recorded world: "
        f"expected {sorted(baseline | world)}, got {sorted(actual)}"
    )
PY
}

assert_ps_json() {
    local source="$1"
    python3 - "$state_assignments" "$source" <<'PY'
import json
import sys

assignments = json.load(open(sys.argv[1], encoding="utf-8"))
rows = json.load(open(sys.argv[2], encoding="utf-8"))
if not isinstance(rows, list) or len(rows) != len(assignments):
    raise SystemExit("ps --json must contain exactly one row per configured machine")
seen = set()
for row in rows:
    if set(row) != {"machine", "ip", "mac", "status"}:
        raise SystemExit(f"ps --json has a non-closed row schema: {row!r}")
    machine = row["machine"]
    if machine not in assignments or machine in seen:
        raise SystemExit(f"ps --json has an unexpected or duplicate machine {machine!r}")
    expected = assignments[machine]
    if row["ip"] != expected["ip"] or row["mac"] != expected["mac"] or row["status"] != "running":
        raise SystemExit(f"ps --json does not match the running recorded allocation for {machine}: {row!r}")
    seen.add(machine)
if seen != set(assignments):
    raise SystemExit("ps --json omitted a configured machine")
PY
}

assert_metrics_json() {
    local source="$1"
    python3 - "$state_assignments" "$source" <<'PY'
import json
import sys

assignments = json.load(open(sys.argv[1], encoding="utf-8"))
metrics = json.load(open(sys.argv[2], encoding="utf-8"))
if set(metrics) != {"schemaVersion", "world", "machines"} or metrics["schemaVersion"] != 1 or metrics["world"] != "redis-foundation":
    raise SystemExit(f"metrics --json has an unexpected closed envelope: {metrics!r}")
rows = metrics["machines"]
keys = {"machine", "smolvmName", "state", "pid", "cpus", "memoryMb", "storageGb", "overlayGb", "cpuSeconds", "cpuMillis", "rssMb", "diskUsedMb"}
if not isinstance(rows, list) or len(rows) != len(assignments):
    raise SystemExit("metrics --json must contain exactly one row per configured machine")
seen = set()
for row in rows:
    if not isinstance(row, dict) or set(row) != keys:
        raise SystemExit(f"metrics --json has a non-closed row schema: {row!r}")
    machine = row["machine"]
    if machine not in assignments or machine in seen:
        raise SystemExit(f"metrics --json has an unexpected or duplicate machine {machine!r}")
    if row["smolvmName"] != assignments[machine]["smolvmName"] or row["state"] != "running":
        raise SystemExit(f"metrics --json does not identify the running recorded machine {machine}: {row!r}")
    if (row["cpus"], row["memoryMb"], row["storageGb"], row["overlayGb"]) != (1, 256, 1, 1):
        raise SystemExit(f"metrics --json does not preserve the Smolfile resource envelope for {machine}: {row!r}")
    if not isinstance(row["pid"], int) or row["pid"] <= 0:
        raise SystemExit(f"metrics --json has no running host PID for {machine}: {row!r}")
    for field in ("cpuSeconds", "cpuMillis", "rssMb", "diskUsedMb"):
        if row[field] is not None and (not isinstance(row[field], int) or row[field] < 0):
            raise SystemExit(f"metrics --json has invalid {field} for {machine}: {row!r}")
    seen.add(machine)
if seen != set(assignments):
    raise SystemExit("metrics --json omitted a configured machine")
PY
}

assert_guest_network_tuple() {
    local machine="$1"
    local expected_ip expected_mac
    expected_ip=$(python3 - "$state_assignments" "$machine" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1], encoding="utf-8"))[sys.argv[2]]["ip"])
PY
)
    expected_mac=$(python3 - "$state_assignments" "$machine" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1], encoding="utf-8"))[sys.argv[2]]["mac"])
PY
)
    "$binary" -f "$world_file" exec "$machine" -- /bin/sh -ceu '
        expected_ip=$1
        expected_mac=$2
        expected_dns=$3
        # The workload image deliberately is not a network-tools image. The
        # kernel proc/sys views are present in every supported guest and
        # avoid making this substrate gate depend on an `iproute2` package.
        test -e /sys/class/net/eth0
        grep -Eq "^[[:space:]]*\\|-- $expected_ip$" /proc/net/fib_trie
        test "$(cat /sys/class/net/eth0/address)" = "$expected_mac"
        grep -Fqx "nameserver $expected_dns" /etc/resolv.conf
        awk '\''$1 == "eth0" && $3 == "00000000" { found = 1 } END { exit !found }'\'' /proc/net/route
    ' smolworld-e2e "$expected_ip" "$expected_mac" "10.89.0.1"
}

assert_dns_name() {
    local name="$1"
    local machine="$2"
    local expected_ip
    expected_ip=$(python3 - "$state_assignments" "$machine" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1], encoding="utf-8"))[sys.argv[2]]["ip"])
PY
)
    "$binary" -f "$world_file" exec runner -- getent hosts "$name" | \
        awk -v expected_ip="$expected_ip" '$1 == expected_ip { found = 1 } END { exit !found }'
}

assert_egress_contract() {
    [[ "${SMOLWORLD_E2E_EGRESS:-}" == "1" ]] || return 0
    "$binary" -f "$world_file" exec runner -- /bin/sh -ceu '
        test -e /sys/class/net/eth1
        awk '\''$1 == "eth1" && $2 == "00000000" { found = 1 } END { exit !found }'\'' /proc/net/route
        getent hosts one.one.one.one >/dev/null
    '
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

assert_no_runtime_evidence() {
    if [[ -e "$isolated_home/.smolworld" || -e "$expected_state_dir" || -e "$runtime_dir" ]]; then
        echo "foundation lifecycle left allocation, listener, or runtime evidence" >&2
        echo "state namespace: $expected_state_dir" >&2
        echo "runtime namespace: $runtime_dir" >&2
        return 1
    fi
}

down_world_exactly() {
    local current="$temporary_dir/machines-after-down.json"
    stop_world_process
    "$binary" -f "$world_file" down >/dev/null
    capture_smolvm_machines "$current"
    assert_machine_names_equal "$baseline_machines" "$current" "foundation cleanup"
    if [[ -e "$runtime_dir" ]]; then
        echo "foundation cleanup left runtime directory or listener: $runtime_dir" >&2
        return 1
    fi
}

cleanup() {
    local original_status=$?
    local cleanup_failed=0
    local current="$temporary_dir/machines-cleanup.json"
    trap - EXIT

    if ! stop_world_process; then
        cleanup_failed=1
    fi
    if [[ -f "$world_file" ]] && ! "$binary" -f "$world_file" down >/dev/null 2>&1; then
        cleanup_failed=1
    fi
    if [[ -f "$baseline_machines" ]]; then
        if ! capture_smolvm_machines "$current" || ! assert_machine_names_equal "$baseline_machines" "$current" "foundation cleanup"; then
            cleanup_failed=1
        fi
    fi
    if [[ -e "$runtime_dir" ]]; then
        echo "foundation cleanup left runtime directory or listener: $runtime_dir" >&2
        cleanup_failed=1
    fi
    rm -rf -- "$temporary_dir" || cleanup_failed=1

    if (( original_status == 0 && cleanup_failed != 0 )); then
        exit 1
    fi
    exit "$original_status"
}
trap cleanup EXIT

# `prepare` is the only mutating host-material operation. Snapshot parsed
# smolvm identities first: this is the unrelated-machine sentinel, and every
# later comparison is exact-name JSON comparison rather than substring search.
capture_smolvm_machines "$baseline_machines"
assert_no_runtime_evidence
"$binary" -f "$world_file" prepare
capture_smolvm_machines "$temporary_dir/machines-after-prepare.json"
assert_machine_names_equal "$baseline_machines" "$temporary_dir/machines-after-prepare.json" "prepare"
assert_no_runtime_evidence

# `check` is read-only after preparation and must preserve the same boundary.
"$binary" -f "$world_file" check
capture_smolvm_machines "$temporary_dir/machines-after-check.json"
assert_machine_names_equal "$baseline_machines" "$temporary_dir/machines-after-check.json" "check"
assert_no_runtime_evidence

start_world() {
    "$binary" -f "$world_file" up >"$up_log" 2>&1 &
    up_pid=$!
    wait_for_world_up "$up_pid"
    state_file=$(find_state_file)
    parse_allocation_state "$state_file" "$state_assignments"
    capture_smolvm_machines "$temporary_dir/machines-running.json"
    assert_baseline_plus_world_machines "$temporary_dir/machines-running.json"
}

assert_live_world_contract() {
    "$binary" -f "$world_file" ps --json >"$temporary_dir/ps.json"
    assert_ps_json "$temporary_dir/ps.json"
    "$binary" -f "$world_file" metrics --json >"$temporary_dir/metrics.json"
    assert_metrics_json "$temporary_dir/metrics.json"

    assert_guest_network_tuple redis
    assert_guest_network_tuple runner
    assert_dns_name redis redis
    assert_dns_name redis.redis-foundation.test redis
    assert_egress_contract
    "$binary" -f "$world_file" exec runner -- /bin/sh -ceu '
        test "$(cat /tmp/smolworld-e2e-seed)" = "smolworld sealed seed"
        test "$(stat -c %a /tmp/smolworld-e2e-seed 2>/dev/null || stat -f %Lp /tmp/smolworld-e2e-seed)" = 640
    '

    local redis_ready=false
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
        return 1
    fi

    local host_payload="$temporary_dir/host-payload"
    local returned_payload="$temporary_dir/returned-payload"
    printf '%s\n' 'smolworld foundation cp payload' >"$host_payload"
    "$binary" -f "$world_file" cp "$host_payload" runner:/tmp/smolworld-e2e-payload
    "$binary" -f "$world_file" exec runner -- /bin/sh -ceu 'test "$(cat /tmp/smolworld-e2e-payload)" = "smolworld foundation cp payload"'
    "$binary" -f "$world_file" cp runner:/tmp/smolworld-e2e-payload "$returned_payload"
    cmp "$host_payload" "$returned_payload"

    SMOLWORLD_E2E_SECRET_VALUE='foundation-secret-value' \
        "$binary" -f "$world_file" exec runner \
        --secret-env SMOLWORLD_E2E_SECRET=SMOLWORLD_E2E_SECRET_VALUE -- \
        /bin/sh -ceu 'test "$SMOLWORLD_E2E_SECRET" = "foundation-secret-value"'
    "$binary" -f "$world_file" exec runner -- /bin/sh -ceu 'test "${SMOLWORLD_E2E_SECRET+x}" != x'
}

start_world
assert_live_world_contract
cp "$state_assignments" "$temporary_dir/first-assignments.json"
down_world_exactly

# Allocation is durable world identity, so recreate the exact same world and
# prove the full static tuple remains unchanged after exact cleanup/restart.
start_world
assert_same_allocations "$temporary_dir/first-assignments.json" "$state_assignments"
assert_live_world_contract
down_world_exactly

echo "PASS: Dockerless prepare/check boundary, exact recorded identities, static private DNS, Redis, cp, secret scope, restart stability, and cleanup"
