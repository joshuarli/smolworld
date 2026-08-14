# smolworld

Run a small, statically declared group of smolvm machines on one private
Ethernet segment. A `.smolworld` file owns the world topology and network;
each machine's Smolfile owns its image, command, environment, and resources.

This is a hard switch to the Smolfile model. The old world-level `image`,
`command`, and resource fields are not aliases and are rejected. smolworld is
not a container orchestrator: it has no Compose compatibility, host networking,
port publishing, NAT, TAP/vmnet, DHCP, IPv6, guest Internet egress, health
checks, or restart policies.

## Requirements

The supported local build runs on macOS Apple Silicon and needs Rust/Cargo,
Xcode command-line tools, `codesign`, `make`, `nm`, and `mkfs.ext4`. Install the
last command with e2fsprogs, for example:

```bash
brew install e2fsprogs
export PATH="/opt/homebrew/opt/e2fsprogs/sbin:/opt/homebrew/opt/e2fsprogs/bin:$PATH"
```

The runtime requires a patched, signed smolvm checkout, a matching
`libkrun.dylib`/`libkrunfw.5.dylib` bundle, and a prepared agent rootfs
containing `usr/local/bin/smolvm-agent`. The local source workflow is:

```text
~/d/smolworld
└── ../smolvm
    └── libkrun/       initialized pinned submodule
```

The default source checkout is `../smolvm`. Its `libkrun/` submodule is the
source used when rebuilding libkrun; an independent `~/d/libkrun` checkout is
not an implicit input. Keep the smolvm submodule initialized and pinned:

```bash
git -C "$HOME/d/smolvm" submodule update --init libkrun
git -C "$HOME/d/smolvm" status --short
git -C "$HOME/d/smolvm/libkrun" rev-parse HEAD
```

The selected smolvm checkout must provide `krun_add_net_unixstream`. The
external-world launch passes one complete static tuple—guest address, gateway,
DNS address, and MAC—over one Unix-stream virtio NIC. smolworld owns the L2
switch, gateway, DNS, socket lifecycle, and exact world cleanup; smolvm owns the
individual VM and Smolfile interpretation.

## Local install

Provide the prepared runtime artifacts, then build and install from this
checkout. This path does not acquire images, build a guest rootfs, or create a
world implicitly:

```bash
SMOLVM_SOURCE_DIR="$HOME/d/smolvm" \
SMOLVM_AGENT_ROOTFS="/path/to/agent-rootfs" \
SMOLWORLD_BUILD_AGENT_ROOTFS=0 \
./scripts/install-local.sh
```

To validate a world during installation, set `SMOLWORLD_CHECK_CONFIG` or pass
`--check PATH`. The default install directory is `~/.local/smolworld` and it
does not use `sudo` or replace an unrelated directory.

Important configuration variables:

```text
SMOLVM_SOURCE_DIR             patched smolvm checkout (default: ../smolvm)
SMOLVM_LIB_DIR                matching libkrun/libkrunfw bundle (default: $SMOLVM_SOURCE_DIR/lib)
SMOLVM_AGENT_ROOTFS           prepared agent rootfs
SMOLWORLD_BUILD_AGENT_ROOTFS  build a missing rootfs (use 0 for the prepared-artifact workflow)
SMOLWORLD_BUILD_LIBKRUN       rebuild from $SMOLVM_SOURCE_DIR/libkrun (default: 0)
SMOLWORLD_LIBKRUN_DIR         libkrun source override (default: $SMOLVM_SOURCE_DIR/libkrun)
SMOLWORLD_LIBKRUN_BUILD_FLAGS make flags (default: BLK=1 NET=1 GPU=1)
CODESIGN_IDENTITY             codesign identity (default: - for ad-hoc signing)
SMOLWORLD_INSTALL_PREFIX      install directory (default: ~/.local/smolworld)
```

If the installed wrapper is used, add it to `PATH`:

```bash
export PATH="$HOME/.local/smolworld/bin:$PATH"
smolworld -f /path/to/.smolworld check
```

## Commands

```text
smolworld prepare [-f PATH]                Resolve and seal local material.
smolworld check [-f PATH]                  Validate the prepared world read-only.
smolworld up [-f PATH]                     Start the world in the foreground.
smolworld ps [-f PATH] [--json]            Show machine lifecycle observations.
smolworld exec [-f PATH] MACHINE -- CMD    Run CMD in a started machine.
smolworld down [-f PATH]                   Stop and delete this world's machines.
```

The default configuration is `.smolworld` in the current directory. `-f` and
`--file` select another path and may appear before or after the command.

`prepare` is the only preparation mutation. It validates every referenced
Smolfile and local image archive, computes BLAKE3 identities for every local
input, and writes `.smolworld.lock` beside the authored world. OCI registry
descriptors retain their standard SHA-256 identity. It does not
allocate world state, bind a listener, or create a machine.

`check` repeats the host/runtime and external-NIC validation and compares all
inputs with `.smolworld.lock`; it is read-only and must run after `prepare`.
`up` refuses unprepared or changed material and starts only from the verified
lock. Press `Ctrl-C` in `up` to stop and delete this world's machines. `down`
is safe to use after an interrupted foreground process and acts only on
machine identities recorded for this world.

`ps` reports host lifecycle observations, not service health or readiness:
`created`, `attached`, `running`, and `absent`. `ps --json` emits the same
rows as a JSON array.

## `.smolworld` format: version 2

The world file is YAML and contains only topology, private-network settings,
Smolfile references, startup dependencies, and sealed seed-file declarations.
For example:

```yaml
format: 2

world:
  name: redis-foundation

network:
  subnet: 10.89.0.0/24
  domain: redis-foundation.test

machines:
  redis:
    smolfile: ./smol/redis.Smolfile
  runner:
    smolfile: ./smol/runner.Smolfile
    depends_on: [redis]
```

An optional seed declaration copies a sealed regular host file into the
machine's private persistent state before its workload starts. It is not a
host mount:

```yaml
  clickhouse:
    smolfile: ./smol/clickhouse.Smolfile
    seed_files:
      - source: ./assets/clickhouse/config.xml
        destination: /etc/clickhouse-server/config.d/smolworld.xml
        mode: "0644"
```

Supported world fields are:

| Field | Meaning |
| --- | --- |
| `format` | Must be exactly `2`. |
| `world.name` | Lowercase DNS label identifying the world. |
| `network.subnet` | Required IPv4 `/24` network. |
| `network.gateway` | Optional gateway address; defaults to `.1`. |
| `network.dns` | Optional DNS address; must equal `gateway`. |
| `network.domain` | Optional lowercase DNS suffix; defaults to the world name. |
| `machines.NAME.smolfile` | Required path to that machine's Smolfile. |
| `machines.NAME.depends_on` | Optional creation/start order only; not readiness. |
| `machines.NAME.seed_files` | Optional sealed file copies into guest paths. |

Every machine receives a stable address and MAC from the world's persisted
allocation state. Other machines resolve it by short name (`redis`) and by
fully qualified name (`redis.redis-foundation.test`). The gateway and DNS
service are synthetic and private to this world. Guests have no route to the
host or Internet, and smolworld does not publish a guest port to the host.

## Smolfile profile

Smolfiles are TOML and are interpreted by smolvm. In the smolworld profile,
use only a local prepared image archive or immutable local material, workload
command fields, environment, working directory, and positive resources:

```toml
# examples/redis/smol/redis.Smolfile
image = "../redis.tar"
entrypoint = ["redis-server"]

cpus = 1
memory = 256
storage = 1
overlay = 1
```

`entrypoint` and `cmd` follow smolvm's OCI command model. `env` and `workdir`
are machine-local settings. Do not put topology or cross-machine addresses in
a Smolfile; use `.smolworld` for those.

The external-world profile rejects `net`, `ports`, `volumes`, Docker socket or
SSH-agent forwarding, egress policy, health checks, restart policy, and other
host-capability or lifecycle settings. smolworld injects the complete external
virtio-net tuple and does not merge a second NIC or guest networking policy.

An authored Smolfile may name an immutable OCI `@sha256:` reference or a local
archive. `prepare` resolves the former on the host into a verified local Docker
archive and generates a local-only Smolfile; every guest always launches from
that archive and never pulls or resolves an image. Unpacked directory material
is not accepted because it has no sealed tree identity. `prepare` records a
BLAKE3 archive identity in `.smolworld.lock`, and any change requires another
explicit `prepare`.

## Redis foundation example

[`examples/redis/.smolworld`](examples/redis/.smolworld) is the first
Smolfile-composed foundation world. It starts a Redis machine and a long-lived
runner machine on a private network. The runner performs DNS and Redis TCP
checks explicitly; smolworld provides no readiness or health contract.

Supply a host-prepared OCI archive at `examples/redis/redis.tar`, or set
`SMOLWORLD_REDIS_ARCHIVE` for the opt-in integration harness. The archive is
deliberately an external input and is never created by the test:

```bash
bash tests/check-redis-foundation-fixture.sh
```

Run the real Apple-Silicon/Hypervisor gate only with prepared local artifacts:

```bash
SMOLWORLD_E2E=1 \
SMOLWORLD_SMOLVM="$HOME/d/smolvm/target/debug/smolvm" \
SMOLVM_AGENT_ROOTFS="$HOME/d/smolvm/target/agent-rootfs" \
SMOLVM_LIB_DIR="$HOME/d/smolvm/lib" \
bash tests/e2e-redis-foundation.sh
```

The gate executes `prepare -> check -> up -> DNS/Redis checks -> down`, proves
that preparation/check create no world runtime state, and verifies cleanup is
limited to the recorded machines and sockets. It requires no guest Internet
access and no container/VM orchestrator beyond the selected smolvm checkout.

## Transition substrate benchmark

[`tests/benchmark_world_transitions.py`](tests/benchmark_world_transitions.py)
measures the currently available single-machine SmolVM fork substrate against
a cold local-archive machine. It is an opt-in Apple-Silicon integration
measurement, not a smolworld checkpoint or a durable `WorldState` benchmark:
the golden stays frozen, each child is non-forkable, and no private L2,
multi-machine cut, state manifest, or restore-after-host-exit contract is
involved.

It uses one vCPU, 256 MiB of configured RAM, a prepared local archive, and a
4 MiB fsynced guest mutation. It creates an isolated, short
`SMOLVM_RUNTIME_ROOT` under `/tmp` and removes only the exact machines and root
it created.

```bash
SMOLWORLD_TRANSITION_BENCH=1 \
SMOLVM_BIN="$HOME/d/smolvm/target/debug/smolvm" \
SMOLVM_AGENT_ROOTFS="$HOME/d/smolvm/target/agent-rootfs" \
SMOLWORLD_TRANSITION_ARCHIVE=/absolute/path/to/prepared/archive.tar \
SMOLVM_LIB_DIR="$HOME/d/smolvm/lib" \
DYLD_LIBRARY_PATH="$HOME/d/smolvm/lib" \
python3 tests/benchmark_world_transitions.py
```

The TSV reports wall time, `accounted_file_blocks_delta_bytes`, and
`volume_used_delta_bytes`. The first counts blocks addressed by each benchmark
file, so APFS clonefile makes it deliberately double-count shared disk blocks.
The latter observes physical space on the enclosing volume and captures CoW
sharing, but sees unrelated host activity; use it as a noisy range rather than
a quota. Neither metric measures guest-RAM sharing, because macOS RSS would
double-count CoW pages and has no stable proportional-set-size interface.
