# smolworld architecture and scope

## Purpose

`smolworld` is a local macOS/Apple-Silicon runner for a small, statically
provisioned group of smolvm machines. A world is described by one `.smolworld`
file and runs on a private userspace Ethernet segment. It is a substrate for
Niceforge's durable workflow/world control plane, not the control plane itself.

The product boundary is deliberately small:

* `.smolworld` owns topology, private-network relationships, stable guest
  allocation, and world lifecycle.
* Each Smolfile owns one machine's image, command, environment, working
  directory, and resources.
* smolvm owns individual VM and guest-agent lifecycle, image handling, and the
  optional existing host-side NAT egress runtime.
* libkrun owns the VMM and virtio implementation.

`depends_on` means creation/start order only. It is not a readiness, health, or
retry contract.

## Ownership boundary

```text
.smolworld
    │
    ▼
smolworld
  config + durable allocation state + Unix-stream L2 switch + ARP/DNS gateway
  world lifecycle + checkpoint coordination + exact recorded-world cleanup
    │
    ▼
patched smolvm
  persistent machine/image lifecycle + guest agent + static IPv4 provisioning
  eth0 attached to smolworld + optional eth1 host-side NAT egress
    │
    ▼
libkrun
  VMM and virtio implementation
```

smolworld owns cross-machine identity, Ethernet forwarding, authoritative
local DNS, upstream DNS forwarding when egress is enabled, socket lifecycle,
checkpoint coordination, and group cleanup. smolvm owns each VM, its guest
agent, OCI image handling, the optional NAT relay, and libkrun invocation. Do
not move L2/DNS/world logic into smolvm or reimplement VMM/virtio behavior here.

The external smolvm ABI supplies one complete static tuple for the private
virtio-net device: guest address, gateway, DNS address, and MAC. That device is
`eth0`. With `network.egress: true`, smolvm adds its existing host-side NAT
device as `eth1` and owns the default route there. The private world remains
on `eth0`; libkrun needs no source patch beyond the existing external
Unix-stream NIC support.

## Module map

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | Binary entry point only. |
| `src/cli.rs` | CLI grammar, help text, and presentation types. |
| `src/config.rs` | Strict `.smolworld` parser, semantic validation, dependency ordering. |
| `src/model.rs` | Shared world, machine, network, state, checkpoint, and identity types. |
| `src/state.rs` | Durable allocation state, stable address/MAC assignment, private paths. |
| `src/smolvm.rs` | Preflight and the narrow smolvm subprocess/capture boundary. |
| `src/switch.rs` | Framed Unix-stream ports, MAC learning, Ethernet forwarding, epochs, cleanup. |
| `src/gateway.rs` | Synthetic gateway ARP and authoritative DNS A replies/forwarding. |
| `src/runtime.rs` | `check`, `up`, `ps`, `metrics`, `exec`, `cp`, checkpoint, restore, release, and cleanup. |

Keep changes in their owning module. `model` contains cross-module domain
contracts; update its users and tests deliberately when changing them.

## Supported platform and inputs

The supported build and runtime target is macOS on Apple Silicon
(`Darwin`/`aarch64`). Linux and Windows are unsupported build/runtime targets;
bundled artifacts in companion repositories are external inputs and must remain
untouched.

The local source workflow is:

```text
~/d/smolworld
└── ../smolvm
    └── libkrun/       initialized pinned submodule
```

The default companion checkout is `../smolvm`. Its initialized `libkrun/`
submodule is the source used when rebuilding libkrun; an independent checkout
is not an implicit input. Keep the selected smolvm checkout and submodule
initialized and pinned:

```bash
git -C "$HOME/d/smolvm" submodule update --init libkrun
git -C "$HOME/d/smolvm" status --short
git -C "$HOME/d/smolvm/libkrun" rev-parse HEAD
```

The supported local source build needs Rust/Cargo, Xcode command-line tools,
`codesign`, `make`, `nm`, and `mkfs.ext4`. Runtime artifacts are a patched and
signed smolvm binary, its signed `smolvm-boot` helper, matching
`libkrun.dylib`/`libkrunfw.5.dylib`, and a prepared agent rootfs containing
`usr/local/bin/smolvm-agent`. On macOS an ad-hoc `target/debug/smolvm` must have
the checked-in `smolvm.entitlements` applied or Hypervisor Framework VM
creation may fail with an opaque `EINVAL`.

The host smolvm binary and guest agent rootfs are separate artifacts. After
changing companion guest-agent networking code, rebuild the rootfs before a
live run:

```bash
cd ~/d/smolvm
./scripts/build-agent-rootfs.sh --arch aarch64 ~/d/smolvm/target/agent-rootfs
```

`smolworld check` verifies that the configured rootfs exists, but cannot know
whether it contains the newest guest-agent build.

The supported local installer consumes prepared artifacts; it does not acquire
images, build a guest rootfs implicitly, or create a world:

```bash
SMOLVM_SOURCE_DIR="$HOME/d/smolvm" \
SMOLVM_AGENT_ROOTFS="/path/to/agent-rootfs" \
SMOLWORLD_BUILD_AGENT_ROOTFS=0 \
./scripts/install-local.sh
```

The durable installer inputs are `SMOLVM_SOURCE_DIR` (default `../smolvm`),
`SMOLVM_LIB_DIR` (default `$SMOLVM_SOURCE_DIR/lib`),
`SMOLVM_AGENT_ROOTFS`, `SMOLWORLD_BUILD_AGENT_ROOTFS`,
`SMOLWORLD_BUILD_LIBKRUN`, `SMOLWORLD_LIBKRUN_DIR`,
`SMOLWORLD_LIBKRUN_BUILD_FLAGS`, `CODESIGN_IDENTITY`, and
`SMOLWORLD_INSTALL_PREFIX` (default `~/.local/smolworld`). The installer may
run `smolworld check` when `SMOLWORLD_CHECK_CONFIG` or `--check PATH` is
provided. It does not use `sudo` or replace an unrelated install directory.

## World configuration contract

The world file is YAML format 2 and contains only topology, private-network
settings, Smolfile references, startup dependencies, and sealed seed-file
declarations:

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

This is a hard Smolfile cutover. Retired world-level `image`, `command`, and
resource fields are rejected rather than treated as aliases or compatibility
fallbacks.

Supported fields are:

| Field | Meaning |
| --- | --- |
| `format` | Must be exactly `2`. |
| `world.name` | Lowercase DNS label identifying the world. |
| `network.subnet` | Required IPv4 `/24` network. |
| `network.gateway` | Optional gateway address; defaults to `.1`. |
| `network.dns` | Optional DNS address; must equal `gateway`. |
| `network.domain` | Optional lowercase DNS suffix; defaults to the world name. |
| `network.egress` | Optional explicit opt-in for smolvm's existing NAT egress on `eth1`. |
| `machines.NAME.smolfile` | Required path to that machine's Smolfile. |
| `machines.NAME.depends_on` | Optional creation/start order only. |
| `machines.NAME.seed_files` | Optional sealed regular-file copies into guest state. |

Seed sources must remain beneath the sealed world root. Destinations are
normalized absolute guest paths, and copies are all-or-nothing into private
machine state. A seed is not a host mount.

Smolfiles are TOML and are interpreted by smolvm. The restricted world-facing
profile permits only local or immutable image material, command fields,
environment, working directory, and positive machine resources:

```toml
image = "../redis.tar"
entrypoint = ["redis-server"]
cpus = 1
memory = 256
storage = 1
overlay = 1
```

Do not put topology or cross-machine addresses in a Smolfile. The profile
rejects `net`, `ports`, `volumes`, Docker socket or SSH-agent forwarding,
egress policy, health checks, restart policy, and other host capabilities.
smolworld injects the private NIC; with egress enabled, smolvm adds the second
NAT NIC and guest-side route policy.

Authored Smolfiles may name a local prepared archive or an immutable OCI
`@sha256:` reference. `prepare` resolves the latter on the host into a
verified local archive and generates local-only material. Guests never pull or
resolve registry images. Unpacked image directories are not accepted because
they lack a sealed tree identity. Every local input is recorded in
`.smolworld.lock`; any change requires another explicit `prepare`.

## Network and lifecycle invariants

* A world has exactly one IPv4 `/24` subnet. Every guest gets a stable static
  IPv4/MAC assignment persisted under `~/.smolworld`.
* The configured DNS address equals the configured gateway. This process is
  authoritative for configured short names and `<machine>.<domain>` names.
* With egress enabled, unknown DNS queries are forwarded by the host gateway
  to the configured upstream resolver (`1.1.1.1:53` today), with bounded
  timeout and synthetic `SERVFAIL` on failure. Without egress, unknown names
  return `NXDOMAIN` and guests have no Internet route.
* The gateway address and MAC are reserved and can never be allocated to a
  machine.
* The host/virtio wire protocol is one big-endian 4-byte Ethernet-frame length
  followed by raw frame bytes. Accepted streams become blocking before frame
  reads; an idle healthy NIC is not a disconnect.
* Unknown, broadcast, and multicast destination MACs flood to other attached
  ports. Known unicast targets the learned port. Detach removes the port and
  its forwarding-database entries.
* `up` owns only deterministic `smw-v2-...` machine names recorded in v2 world
  state. `down`, signal cleanup, restore failure cleanup, and release never
  affect unrelated smolvm machines or v1 state.
* Validate configuration, sealed material, and inspectable runtime
  prerequisites before creating state, listeners, or machines. Cleanup is
  always constrained by exact recorded identities.
* Machine resources belong to the restricted Smolfile profile. smolworld does
  not duplicate or override them in `.smolworld`.

## Commands and preparation

The default authored file is `.smolworld`; `-f`/`--file` selects another path
and may appear before or after the command.

```text
smolworld prepare [-f PATH]                         Resolve and seal local material.
smolworld check [-f PATH]                           Validate prepared material read-only.
smolworld up [-f PATH]                              Start the foreground supervisor.
smolworld ps [-f PATH] [--json]                     Show host lifecycle observations.
smolworld metrics [-f PATH] --json                  Show recorded-machine host metrics.
smolworld exec [-f PATH] MACHINE [--secret-env GUEST=HOST_ENV]... -- CMD
                                                     Delegate one command to a machine.
smolworld cp [-f PATH] SOURCE DEST                  Copy one regular file through the agent.
smolworld checkpoint [-f PATH] --output DIR          Capture and retain the running world.
smolworld restore [-f PATH] --checkpoint DIR        Restore a retained same-lineage world.
smolworld release [-f PATH] --checkpoint DIR        Delete exactly a retained world.
smolworld down [-f PATH]                            Stop and delete this world's machines.
```

`prepare` is the only preparation mutation. It validates all referenced
Smolfiles and local image archives, computes BLAKE3 identities, and writes the
lock beside the authored world. It does not allocate runtime state, bind a
listener, or create a machine. `check` repeats host/runtime and external-NIC
validation and compares all inputs with the lock; it is read-only and must run
after `prepare`. `up` refuses unprepared or changed material. `Ctrl-C` stops
and deletes the exact world; `down` is safe after an interrupted foreground
process.

`metrics --json` is read-only and emits a closed `schemaVersion: 1` object with
one row per configured machine. It reads only v2 allocation records and never
lists or discovers unrelated smolvm machines. CPU/RSS are host VMM
observations, and disk usage is host data-directory usage; none is guest
process or guest-filesystem telemetry.

`ps` reports host lifecycle observations, not service health or readiness. Its
closed states are `created`, `attached`, `running`, `capturing`, `captured`,
and `absent`; `ps --json` emits the same rows as a JSON array.

`exec --secret-env GUEST=HOST_ENV` resolves a caller-owned host environment
variable for that command only. The value is not stored in world state, the
Smolfile, or the material lock. `cp` is likewise scoped to one recorded world
machine and one regular file.

## Checkpoint and restore contract

`checkpoint` asks the foreground supervisor to close the switch at a new epoch,
capture every machine concurrently, seal one world receipt, publish it by
rename, and retain exactly those machine sources. `restore` accepts only a
receipt whose configuration, material lock, allocation, and topology match the
selected world; it creates fresh agent and Unix-stream NIC handles. `release`
is the normal deletion path for a retained checkpoint.

The receipt is a durable same-lineage world artifact, not automatically a
Niceforge workflow fact. Niceforge separately records a lease-fenced
PostgreSQL `WorldState` and exact receipt, and releases that database fact only
after the host releases the checkpoint. smolworld remains independent of
Niceforge workflow semantics.

A checkpoint is one coherent temporal cut across all machines and the switch:

1. Record the capture intent before external capture.
2. Close the switch at a new epoch; reject new actions and `exec` calls, stop
   delivering frames, and record the FDB and bounded queued state.
3. Pause every VM concurrently before capturing any machine.
4. Capture writable disk/overlay, RAM, virtual-device state, workspace,
   topology, and material identity. Captures contain no host file descriptors.
5. Seal machine receipts, switch state, and material receipts into one
   canonical world receipt and publish it atomically.
6. On failure, retain only exact unpublished candidates for reconciliation;
   never guess from broad process or name scans.

Restoring RAM from one point with a disk or network state from another is
invalid. The initial implementation may exit captured VMs and restore a fresh
child; capture-and-continue is an optimization and must not change the receipt
or transaction contract.

The published checkpoint receipt includes a bounded BLAKE3 digest for each
opaque machine checkpoint-control receipt. `restore` and `release` recompute
those small digests before launching or deleting anything; smolvm remains the
owner of the detailed RAM/disk file-identity and control-file checks. This is
an integrity anchor inside the same-user host trust boundary, not a host-wide
content audit or a cryptographic claim about guest state. Receipt schema
changes are intentionally incompatible unless a migration/re-capture contract
is defined.

## Durable world model and state logistics

The world graph keeps these objects distinct:

```text
WorldState       immutable logical state, content-addressed by canonical manifest
WorldTransition  parents + delta + actor/objective/provenance -> child state
Evaluation       state + evaluator + result + evidence + uncertainty
WorldCheckpoint  state + materializer ABI + disposable acceleration artifact
WorldRun         one mutable execution instantiated from a WorldState
```

A state is not a machine, and a run is not durable state. State identity must
not include the transition that produced it, an evaluation that inspected it,
or a host checkpoint path. Independent transitions may produce one
byte-identical state while retaining distinct transition records.

The semantic state is a canonical manifest over named channels such as:

```text
source/checkout       Merkle or equivalent source receipt
source/world          canonical .smolworld + lock identity
materials/images      immutable material receipts
topology/services     machine and network manifest
workspace/runner      workspace state receipt
nondeterminism/input  captured external observations
lineage               parent state references
```

Every semantically relevant input must be immutable, deterministically
derivable, or explicitly captured as nondeterministic evidence. VM disks, RAM
images, package caches, compiler outputs, clonefiles, worktrees, and page
caches are materializations or acceleration inputs. They may be discarded and
regenerated without losing logical state. A checkpoint must never become the
sole source of a semantically required dependency.

State logistics is a separate layer from world lifecycle and from the VMM:

```text
World API / DAG: state, fork, checkpoint, reset, lineage, GC
State logistics: manifests/CAS, memory and disk deltas, lazy materialization,
                 prefetch, deduplication, locality, leases, GC, identity reseed
World runtime:  topology, switch, DNS, lifecycle, checkpoint barrier
smolvm:         VM lifecycle and guest interaction
libkrun:        VMM and virtual-device state seam
```

The reusable index must be content-addressed and host-aware. It should be
implemented as a separate logical-world materialization index; run-scoped
`world_states` remain workflow recovery records, not a cache. A physical
materialization may serve multiple logical states only through explicit
immutable references. Leases protect materializations during use; pins are
durable GC roots; cleanup removes only unreachable, unpinned records.

The first storage implementation may use eager immutable directories and
filesystem CAS. Keep the logical manifest independent so parent-plus-delta
storage, dirty-page tracking, APFS clone acceleration, checkpoint flattening,
and bounded chain compaction remain replaceable implementation details.

The memory, disk, and device timelines must remain coherent. If incremental
dirty tracking is added, both guest CPU writes and virtio/DMA writes must feed
the same dirty set. Host handles are rebound on restore; they are never
serialized into state. Concurrent descendants must reseed static IPv4, MAC,
machine identity, entropy, and guest credentials. New socket paths alone do
not make a valid identity fork.

The core operations are conceptually:

```text
checkpoint(run) -> state
spawn(state) -> run
fork(state, n) -> run[n]
reset(run, state)
destroy(run)
pin(state) / unpin(state) / gc()
parent(state) / children(state) / diff(a, b) / ancestry(state)
```

Do not add privileged machine-state `merge` semantics. A higher-level
evaluator selects a descendant and advances the canonical pointer; reconciliation
is an ordinary explicit N-parent transition if it is ever needed.

## Niceforge integration contract

Niceforge owns constitution/mission, sealed workflow plans, objectives, offices,
leases, step ordering, world lineage, transitions, evaluations, evidence, and
policy. It supplies exact sealed `.smolworld` material to this runtime and must
not consult a mutable worktree or world definition during execution.

The first executor boundary may be host-local, but its operations remain typed:

```text
prepare/check
materialize or restore WorldState
await runner attachment
execute one fixed step action
capture a transition
inspect a retained descendant
stop/pause/resume
release or retain exact world resources
```

The failed-step model is:

```text
failed step
  -> capture post-failure state W1 before cleanup
  -> retain immutable W1 and evidence
  -> inspect a disposable descendant
  -> restore a child run from W1
  -> retry only the failed step
  -> record W2 and its transition/evidence
```

W1 begins only after all semantically relevant channels are captured and the
immutable receipt is committed. A source snapshot, job attempt, or live fork
is not W1. Inspection mutations never rewrite W1 or silently become a retry.
Literal host-reachable SSH and guest SSH daemons are not required; an
SSH-shaped local command may delegate through smolvm `exec`.

Niceforge's broader trajectory model keeps the institutional graph separate
from the world graph. Durable institutional objects are constitution/mission,
office/charter/authority, objective/commitment, hypothesis/question,
experiment/proposal, claim/counterargument, evaluation/evidence,
decision/design principle, and world transition. An office is persistent and
owns jurisdiction, authority, obligations, budget, subscriptions, unresolved
agenda, and institutional memory; an agent is a fungible occupant. Scratch
reasoning may disappear, while consequential deltas, evidence, claims,
questions, and decisions remain typed records. The first vertical slice may
use one root office and explicit objectives; do not make agent process identity
the durable institutional speaker.

The control-plane record shape remains explicit even when storage changes:

```text
logical world state      canonical channel manifest and digest
world state channel      named content digest and derivation receipt
world transition         parent states, delta, objective/step, actor, evidence
world checkpoint         materializer ABI and disposable acceleration receipt
world run                state, job/step lease, generated resource identities
machine/switch receipt   VM/device/material and epoch/FDB/rebind evidence
evaluation               evaluator version, result, evidence, uncertainty
```

State identity is content-addressed and immutable. Transition identity remains
unique even when a child state deduplicates. Evaluation never mutates state
identity. `fork(state, n)` creates branch references before materializing
machines; `materialize(state)` creates a mutable run under exact ownership;
`commit(parents, delta, evidence)` creates or finds the child and records the
transition. Failed worlds and inspection descendants are explicit retention
roots, not accidental cache entries.

Step execution is a durable protocol beneath the executor lease:

```text
pending -> preparing -> running -> capture_requested -> finalizing -> completed
                                      \-> failed/retained
                                      \-> cancelled/lost
```

Every meaningful boundary carries run/job/step/attempt identity, lease/fence,
monotonic executor sequence, before/after state, action/input digests,
outcome/reason, evidence receipts, and idempotency/correlation identity. The
executor may cache expression context for speed, but PostgreSQL events and
world receipts must reconstruct each semantic boundary. A checkpoint capture
is committed only after the lease/fence is revalidated in the same durable
transaction; pre-commit candidates are reconciled by exact receipt, never by
logs or broad host scans.

## Acceptance scenarios and measurements

The Redis foundation is the smallest real private-network fixture: a
Smolfile-composed Redis machine and long-lived runner machine. Its static
fixture check must run without a VM. The real gate proves `prepare -> check ->
up -> explicit DNS/Redis checks -> down`, and proves that preparation/check
create no runtime state and cleanup touches only recorded machines and sockets.
The prepared `redis.tar` archive is an external input; tests never build it or
invoke Docker, Compose, OrbStack, `DOCKER_HOST`, or a Docker socket.

The Sentry backend is the larger workload fixture, not the only test boundary.
Its host-prepared Linux/arm64 `checkout.tar` and `python-site.tar` exercise
parallel independent material preparation, dependency-wave creation, active
services/workspace, restored private DNS/Redis/Snuba traffic, pytest
collection, and the exact model test. Keep generic Smolworld tests and cheap
Niceforge PostgreSQL receipt tests independent of this six-machine workload.

When changing external NIC behavior, the fork gate measures live fork/reconnect
and private traffic while keeping the frozen golden alive. When changing
checkpoint behavior, the durable gate captures a two-machine world, exits the
original supervisor, restores with fresh agent/NIC handles, verifies workspace
and Redis state, and performs exact `release` cleanup.

Measure the phases separately rather than collapsing them into one VM number:

```text
logical fork/reference creation
checkpoint barrier request and switch quiescence
VM pause/capture per machine
manifest/CAS sealing
restore process launch
agent ready and NIC/DNS/private-traffic ready
step resume ready
accounted storage and physical APFS bytes
```

APFS clonefile observations must distinguish addressed/accounted file blocks
from physical volume deltas. Accounted blocks may double-count shared clones;
volume-used bytes include unrelated host activity; neither is a proportional
guest-RAM measurement. The checkpoint hot path must hash small control files
and use bounded file-identity receipts for immutable large clonefiles. A deep
full-content audit is an explicit offline operation, not a transition
precondition. Independent preparation and creation remain parallel while
declared dependency waves preserve deterministic order.

## Verification and working practices

Before editing, read this file and inspect the owning module, callers, tests,
schemas, and nearby documentation. For contract changes:

1. Write the smallest observable regression or acceptance test first.
2. Update domain types, schemas, and callers deliberately.
3. Keep canonical bytes, receipts, errors, and cleanup deterministic.
4. Test crash, duplicate, stale-lease, identity-fork, and exact-cleanup paths.
5. Update this file and nearby comments where durable meaning lives.

The normal local baseline is:

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
git diff --check
```

The real Redis foundation gate is opt-in because it creates VMs and needs
prepared artifacts. It must run without Docker, Compose, OrbStack, `DOCKER_HOST`,
or a Docker socket:

```bash
SMOLWORLD_E2E=1 \
SMOLWORLD_SMOLVM=/path/to/smolvm \
SMOLVM_AGENT_ROOTFS=/path/to/agent-rootfs \
SMOLVM_LIB_DIR=/path/to/smolvm/lib \
bash tests/e2e-redis-foundation.sh
```

The fork and coordinated durable-world gates are opt-in measurements for the
external NIC and checkpoint boundaries:

```bash
SMOLWORLD_FORK_E2E=1 \
SMOLWORLD_SMOLVM=/path/to/smolvm \
SMOLVM_AGENT_ROOTFS=/path/to/agent-rootfs \
SMOLVM_LIB_DIR=/path/to/smolvm/lib \
python3 tests/e2e_fork_world.py

SMOLWORLD_DURABLE_E2E=1 \
SMOLWORLD_SMOLVM=/path/to/smolvm \
SMOLVM_AGENT_ROOTFS=/path/to/agent-rootfs \
SMOLVM_LIB_DIR=/path/to/smolvm/lib \
python3 tests/e2e_fork_world.py
```

The foundation fixture proves generic static DNS, Redis TCP through real
virtio NICs, and exact machine/runtime cleanup. Redis is a workload fixture,
never runtime behavior. The durable fixture proves private DNS/Redis,
workspace state, fresh agent/NIC attachment after the original supervisor
exits, receipt validation, and exact release. These live gates do not replace
cheap config, state, receipt, and control-boundary tests.

Do not run pre-commit hooks or push a remote. Do not add a third-party Rust
dependency, expand networking/product scope, or make destructive cleanup
broader than the recorded world without explicit approval.

## Explicit non-goals and deferrals

smolworld does not provide Docker/Compose compatibility, workflow steps,
generic service readiness, health checks, restart policies, log aggregation,
host networking, port publishing, smolworld-owned NAT, TAP/vmnet, DHCP, IPv6,
guest SSH, registry pulls from guests, or a second executor substrate.

It also does not define distributed scheduling, multi-world merge semantics,
GPU fabrics, a universal state codec, persistent agent personalities, or a
social chat system. Those concerns belong above the small, stable runtime and
control-plane boundaries. A replacement kernel or policy must be separately
evaluated and migrated; it must not be silently introduced as compatibility
code.
