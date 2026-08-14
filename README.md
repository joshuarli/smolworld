# smolworld

Run a small, statically declared group of smolvm machines on one private
Ethernet segment. A `.smolworld` file owns the world topology and network;
each machine's Smolfile owns its image, command, environment, and resources.

This is a hard switch to the Smolfile model. The old world-level `image`,
`command`, and resource fields are not aliases and are rejected. smolworld is
not a container orchestrator: it has no Compose compatibility, host networking,
port publishing, host-side NAT implementation, TAP/vmnet, DHCP, or IPv6 on the
private world NIC. Explicit `network.egress` delegates outbound Internet
traffic to smolvm's existing NAT runtime; smolworld does not implement that
data path itself. Health checks and restart policies also remain out of scope.

## Platform support

smolworld supports only macOS on Apple Silicon (`Darwin`/`aarch64`). Linux and
Windows are unsupported build and runtime targets; this repository does not
build, test, or release them. Any Linux or Windows artifacts in a companion
smolvm checkout are external inputs and remain intentionally untouched.

## Requirements

The supported local build runs on macOS Apple Silicon and needs Rust/Cargo,
Xcode command-line tools, `codesign`, `make`, `nm`, and `mkfs.ext4`. Install the
last command with e2fsprogs, for example:

```bash
brew install e2fsprogs
export PATH="/opt/homebrew/opt/e2fsprogs/sbin:/opt/homebrew/opt/e2fsprogs/bin:$PATH"
```

The runtime requires a patched, signed smolvm checkout, its signed
`smolvm-boot` launch helper, a matching `libkrun.dylib`/`libkrunfw.5.dylib`
bundle, and a prepared agent rootfs containing `usr/local/bin/smolvm-agent`.
The local installer builds and signs both launch binaries. When the helper is
next to `smolvm`, smolvm selects it automatically for fresh VM subprocesses;
otherwise it falls back to the full binary.

The host `smolvm` binary and guest agent rootfs are separate artifacts. After
changing companion `smolvm-agent` networking code, rebuild the rootfs before a
live run, for example:

```bash
cd ~/d/smolvm
./scripts/build-agent-rootfs.sh --arch aarch64 ~/d/smolvm/target/agent-rootfs
```

`smolworld check` verifies that the configured rootfs exists, but cannot
determine whether it contains the newest guest-agent build.

The local source workflow is:

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
DNS address, and MAC—over the first Unix-stream virtio NIC. With
`network.egress: true`, smolvm also attaches its existing host-side NAT runtime
as a second virtio NIC: the private smolworld NIC remains `eth0`, while smolvm's
NAT NIC is `eth1` and owns the default route. smolworld owns the private L2
switch, local DNS, socket lifecycle, and exact world cleanup; smolvm owns the
individual VM, guest NIC setup, NAT, and Smolfile interpretation.

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
smolworld exec [-f PATH] MACHINE [--secret-env GUEST=HOST_ENV]... -- CMD
                                            Run CMD in a started machine.
smolworld checkpoint [-f PATH] --output DIR
                                            Capture this running world, retain
                                            its exact machine sources, and exit.
smolworld restore [-f PATH] --checkpoint DIR
                                            Restore a retained same-lineage world.
smolworld release [-f PATH] --checkpoint DIR
                                            Delete exactly a retained world.
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

`checkpoint` asks the foreground supervisor to close its switch at a new epoch,
capture every machine concurrently, seal a world receipt, publish it by rename,
then retain exactly those machine sources.  `restore` accepts only a receipt
whose sealed configuration, material lock, allocation, and topology match the
selected world; it always creates fresh agent and Unix-stream NIC handles.
`release` is the only normal deletion path for a retained checkpoint.  These
commands implement a durable same-lineage world artifact, not a Niceforge
workflow `WorldState`: Niceforge has not yet supplied the lease-fenced lineage
transaction that makes a captured world a workflow fact.

`ps` reports host lifecycle observations, not service health or readiness:
`created`, `attached`, `running`, `capturing`, `captured`, and `absent`.
`ps --json` emits the same rows as a JSON array.

`exec --secret-env GUEST=HOST_ENV` resolves a caller-owned host environment
variable for one delegated command. The value is passed to smolvm for that
exec only; it is not stored in world state, the Smolfile, or the material lock.

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
  egress: true

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
| `network.egress` | Optional; when true, adds smolvm's existing NAT as guest `eth1` and puts the default route there. |
| `machines.NAME.smolfile` | Required path to that machine's Smolfile. |
| `machines.NAME.depends_on` | Optional creation/start order only; not readiness. |
| `machines.NAME.seed_files` | Optional sealed file copies into guest paths. |

### Guest Internet egress

`network.egress` is an explicit opt-in for outbound guest traffic. It changes
the guest's two-NIC topology as follows:

| Interface | Owner | Addressing | Route |
| --- | --- | --- | --- |
| `eth0` | smolworld | Static world address from the allocation state | Private world subnet and local DNS only |
| `eth1` | smolvm | Existing smolvm host-side NAT runtime | Default route for Internet traffic |

smolworld continues to own the private Ethernet switch and the gateway/DNS
address on `eth0`; it does not perform NAT or publish ports. Known world names
are answered locally. When egress is enabled, unknown DNS queries are forwarded
by the smolworld host gateway to its configured upstream resolver, currently
`1.1.1.1:53`, so the guest can resolve public names without bypassing local
world discovery. With egress disabled, unknown names return `NXDOMAIN` and the
guest has no Internet route.

The guest's default route is deliberately moved to `eth1`; this keeps
machine-to-machine traffic on the private `eth0` segment while allowing the
existing smolvm NAT relay to handle outbound TCP/UDP traffic. The feature
requires the companion smolvm external-world ABI that supports
`--net-egress`; `smolworld check` validates that boundary before creating any
world resources.

Every machine receives a stable address and MAC from the world's persisted
allocation state. Other machines resolve it by short name (`redis`) and by
fully qualified name (`redis.redis-foundation.test`). The gateway and DNS
service are synthetic and private to this world. Without `network.egress`,
guests have no route to the host or Internet. With it, only smolvm's second
NIC provides outbound Internet access; the private world NIC remains isolated,
and smolworld does not publish a guest port to the host.

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
virtio-net tuple; with `network.egress`, smolvm adds its own second NAT NIC and
guest-side routing policy.

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

### External-NIC fork reconnect E2E

[`tests/e2e_fork_world.py`](tests/e2e_fork_world.py) extends that fixture with
the current live SmolVM fork substrate. It verifies ordinary private DNS/Redis
traffic, restarts the runner as a forkable golden, then proves that its restored
clone reconnects both the agent and the same Unix-stream NIC while the frozen
golden still holds its old connection. The clone must resolve and reach Redis
through Smolworld's switch before the test passes.

```bash
PATH="/opt/homebrew/opt/e2fsprogs/sbin:/opt/homebrew/opt/e2fsprogs/bin:$PATH" \
SMOLWORLD_FORK_E2E=1 \
SMOLWORLD_SMOLVM="$HOME/d/smolvm/target/debug/smolvm" \
SMOLVM_AGENT_ROOTFS="$HOME/d/smolvm/target/agent-rootfs" \
SMOLVM_LIB_DIR="$HOME/d/smolvm/lib" \
python3 tests/e2e_fork_world.py
```

Its TSV reports fork wall time and two storage-sharing proxies. Allocated file
blocks deliberately double-count APFS clonefile sharing; volume-used bytes see
physical CoW sharing but include unrelated host writes. It does not establish a
durable world checkpoint or measure proportional guest-RAM sharing.

The first performance target was the fork transition itself: the recorded
109.852 ms transition was roughly 71% of the 154.302 ms measured path to a
clone with private DNS/Redis traffic, while the physical APFS delta was only
90,112 bytes. Stage tracing identified fresh clone-process startup as the
largest controllable slice. The minimal signed `smolvm-boot` helper reduced
clone agent readiness from about 53 ms to 27 ms in the real-VM gate; the next
performance seam is now the guest identity/release handshakes and the outer
fork-command launch.  Coordinated durable capture is now available as a
separate, slower correctness path; its integrity sealing remains the next
performance seam.

### Coordinated durable-world E2E

The same fixture also exercises a real two-machine durable checkpoint.  It
writes a workspace marker and a Redis key, checkpoints the active world,
waits for the original supervisor to exit, restores from the published receipt,
then proves private DNS, Redis, the workspace, fresh agent/NIC attachment, and
exact release.  The artifact retains an APFS-backed RAM/disk copy and validates
its receipt before restore; it does not create a concurrent child or reseed
guest identity.

```bash
PATH="/opt/homebrew/opt/e2fsprogs/sbin:/opt/homebrew/opt/e2fsprogs/bin:$PATH" \
SMOLWORLD_DURABLE_E2E=1 \
SMOLWORLD_SMOLVM="$HOME/d/smolvm/target/debug/smolvm" \
SMOLVM_AGENT_ROOTFS="$HOME/d/smolvm/target/agent-rootfs" \
SMOLVM_LIB_DIR="$HOME/d/smolvm/lib" \
python3 tests/e2e_fork_world.py
```

On 2026-08-14, the real two-machine run captured in 5,680.518 ms and the
restored runner reached a private NIC in 98.493 ms. The hot-path receipt uses
BLAKE3 for the small VMM control files and a versioned APFS file-identity,
size, and modification-time receipt for immutable RAM/disk clonefiles. A deep
full-content audit is deliberately deferred; it must not turn a checkpoint
transition into a host-wide hash workload.

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
