#!/usr/bin/env bash
# Fast, VM-free contract check for the Redis foundation fixture.
#
# Keep this check independent of smolworld's parser while the v2 Smolfile
# boundary is being implemented. It catches accidental reintroduction of the
# legacy image/command fields and of host-capability settings in the fixture.

set -euo pipefail

project_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fixture_dir="$project_dir/examples/redis"
world_file="$fixture_dir/.smolworld"
redis_smolfile="$fixture_dir/smol/redis.Smolfile"
runner_smolfile="$fixture_dir/smol/runner.Smolfile"

fail() {
    echo "redis foundation fixture: $*" >&2
    exit 1
}

[[ -f "$world_file" ]] || fail "missing $world_file"
[[ -f "$redis_smolfile" ]] || fail "missing $redis_smolfile"
[[ -f "$runner_smolfile" ]] || fail "missing $runner_smolfile"

grep -Fqx 'format: 2' "$world_file" || fail "world must declare format: 2"
grep -Fqx '  name: redis-foundation' "$world_file" || fail "unexpected world name"
grep -Fqx '  subnet: 10.89.0.0/24' "$world_file" || fail "unexpected subnet"
grep -Fqx '  domain: redis-foundation.test' "$world_file" || fail "unexpected DNS domain"
grep -Fqx '    smolfile: ./smol/redis.Smolfile' "$world_file" || fail "redis must use its Smolfile"
grep -Fqx '    smolfile: ./smol/runner.Smolfile' "$world_file" || fail "runner must use its Smolfile"
grep -Fqx '    depends_on: [redis]' "$world_file" || fail "runner must depend on redis"

# The old grammar must not survive in this fixture. The image and command
# belong to the Smolfiles, not to the cross-machine topology file.
if grep -Eq '^[[:space:]]*(image|command|cpus|memory_mib|storage_gib|overlay_gib):|^[[:space:]]*\[(machines\.|world|network)' "$world_file"; then
    fail "legacy image/command TOML fields found in .smolworld"
fi

for smolfile in "$redis_smolfile" "$runner_smolfile"; do
    grep -Fqx 'image = "../redis.tar"' "$smolfile" || fail "$smolfile must use local prepared image material"
    grep -Eq '^entrypoint = \[' "$smolfile" || fail "$smolfile must declare an entrypoint"
    grep -Eq '^cpus = [1-9][0-9]*$' "$smolfile" || fail "$smolfile must declare positive CPUs"
    grep -Eq '^memory = [1-9][0-9]*$' "$smolfile" || fail "$smolfile must declare positive memory"
    grep -Eq '^storage = [1-9][0-9]*$' "$smolfile" || fail "$smolfile must declare positive storage"
    grep -Eq '^overlay = [1-9][0-9]*$' "$smolfile" || fail "$smolfile must declare positive overlay"
    if grep -Eq '(^|[[:space:]])(net|ports|volumes|docker_socket|ssh_agent|health|restart)[[:space:]]*=' "$smolfile"; then
        fail "$smolfile declares a forbidden host-capability or lifecycle setting"
    fi
done

grep -Fqx 'entrypoint = ["redis-server"]' "$redis_smolfile" || fail "redis must run redis-server"
grep -Fqx 'entrypoint = ["sleep"]' "$runner_smolfile" || fail "runner must stay available"
grep -Fqx 'cmd = ["infinity"]' "$runner_smolfile" || fail "runner must stay available"

echo "PASS: Redis foundation fixture uses v2 topology plus restricted local Smolfiles"
