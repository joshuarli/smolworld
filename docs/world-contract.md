# smolworld world contract

This is the sole normative, user-facing contract for `smolworld`. It defines
the authored `.smolworld` file, the world/network boundary, lifecycle
commands, observations, checkpoints, cleanup, and the supported acceptance
scope. If another smolworld document or example conflicts with this document,
this document takes precedence. The implementation and tests are evidence of
this contract; they do not create an alternate public contract.

The external smolvm command surface, its machine and guest-agent behavior, the
Smolfile format, and libkrun are upstream contracts. This document records the
boundary at which smolworld consumes them. It does not redefine or authorize
changes to those upstream projects.

## Purpose and ownership

`smolworld` is a local macOS/Apple-Silicon runner for a small, statically
provisioned group of smolvm machines. One `.smolworld` file describes a world
on a private userspace Ethernet segment. Higher-level tools may use the world
as an independent substrate without changing this contract.

The ownership boundary is:

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

`.smolworld` owns topology, private-network relationships, stable guest
allocation, and world lifecycle. Each Smolfile owns one machine's image,
command, environment, working directory, and resources. smolvm owns each VM,
guest-agent and image lifecycle, the optional existing host-side NAT egress
runtime, and the libkrun invocation. libkrun owns the VMM and virtio
implementation.

smolworld owns cross-machine identity, Ethernet forwarding, authoritative local
DNS, upstream DNS forwarding when egress is enabled, socket lifecycle,
checkpoint coordination, and group cleanup. Do not move L2, DNS, or world
logic into smolvm, or reimplement VMM/virtio behavior here.

`depends_on` controls creation/start order only. It is not a readiness,
health, retry, or service orchestration contract.

## Supported platform and external inputs

The supported build and runtime target is macOS on Apple Silicon
(`Darwin`/`aarch64`). Linux and Windows are unsupported build/runtime targets.
Bundled artifacts in companion repositories are external inputs and remain
untouched by smolworld.

The local source layout is:

```text
~/d/smolworld
└── ../smolvm
    └── libkrun/       initialized pinned submodule
```

The selected smolvm checkout and its initialized, pinned `libkrun/` are the
source inputs when building locally. An independent libkrun checkout is not an
implicit input.

The runtime requires a patched and signed smolvm binary, its signed
`smolvm-boot` helper, matching `libkrun.dylib`/`libkrunfw.5.dylib`, and a
prepared agent rootfs containing `usr/local/bin/smolvm-agent`. On macOS, an
ad-hoc `target/debug/smolvm` must have the checked-in `smolvm.entitlements`
applied or Hypervisor Framework VM creation may fail with `EINVAL`.

The host smolvm binary and guest agent rootfs are separate artifacts. A
`smolworld check` verifies that the configured rootfs exists, but cannot verify
that it contains the newest guest-agent build. The supported installer consumes
prepared artifacts; it does not acquire images, build a guest rootfs
implicitly, or create a world.

## World configuration

The default authored file is `.smolworld`. `-f`/`--file` selects another path
and may appear before or after the command. The file is YAML and must contain
exactly one document with `format: 2`:

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

The parser is strict: unknown fields, missing required fields, multiple YAML
documents, unsupported formats, and retired world-level machine fields are
rejected. This is a hard Smolfile cutover. World-level `image`, `command`, and
resource fields are not aliases or compatibility fallbacks.

The supported fields are:

| Field | Contract |
| --- | --- |
| `format` | Required and exactly the integer `2`. |
| `world.name` | Required lowercase DNS label identifying the world. |
| `network.subnet` | Required IPv4 `/24` network address ending in `.0`. |
| `network.gateway` | Optional usable address in the subnet; defaults to `.1`. |
| `network.dns` | Optional address; defaults to `gateway` and must equal it. |
| `network.domain` | Optional lowercase DNS suffix; defaults to `world.name`. |
| `network.egress` | Optional boolean; defaults to `false` and explicitly opts into smolvm's existing NAT egress on `eth1`. |
| `machines.NAME.smolfile` | Required non-escaping path to that machine's Smolfile. |
| `machines.NAME.depends_on` | Optional list of machine names controlling creation/start order only. Dependencies must exist, be unique, and be acyclic. |
| `machines.NAME.seed_files` | Optional list of sealed regular-file copies into guest state. |

World and machine names are lowercase RFC-1123-style DNS labels. A world has at
least one machine. Authored paths are relative to the `.smolworld` file and
must not escape its world root. A seed declaration has required `source`,
`destination`, and `mode` fields:

```yaml
seed_files:
  - source: ./config/redis.conf
    destination: /etc/redis/redis.conf
    mode: "0644"
```

The source remains beneath the sealed world root and must be a regular file.
The destination is an absolute guest path, each destination is unique, and
`mode` is a four-digit octal mode. Seed copies are sealed and all-or-nothing
into private machine state; a seed is not a host mount.

### Smolfile boundary and material preparation

Smolfiles are TOML interpreted by smolvm. The restricted world-facing profile
permits local or immutable image material, command fields, environment,
working directory, and positive machine resources:

```toml
image = "../redis.tar"
entrypoint = ["redis-server"]
cpus = 1
memory = 256
storage = 1
overlay = 1
```

Topology and cross-machine addresses do not belong in a Smolfile. The profile
rejects `net`, `ports`, `volumes`, Docker socket or SSH-agent forwarding,
egress policy, health checks, restart policy, and other host capabilities.
smolworld injects the private NIC; when egress is enabled, smolvm adds the
second NAT NIC and guest-side route policy.

Authored Smolfiles may name a local prepared archive or an immutable OCI
`@sha256:` reference. Unpacked image directories are not accepted because
they lack a sealed tree identity. Guests never pull or resolve registry
images. `prepare` resolves an immutable reference on the host into a verified
local archive and generates local-only material. Every local input is recorded
in `.smolworld.lock`; changing any input requires another explicit `prepare`.

`prepare` is the only preparation mutation. It validates all referenced
Smolfiles and local image archives, computes BLAKE3 identities, and writes the
lock beside the authored world. It does not allocate runtime state, bind a
listener, or create a machine. `check` repeats host/runtime and external-NIC
validation and compares all inputs with the lock; it is read-only and must run
after `prepare`. `up` refuses unprepared or changed material.

## Network and lifecycle invariants

Each world has exactly one IPv4 `/24` subnet. Every guest receives a stable
static IPv4/MAC assignment persisted beneath `~/.smolworld`. The gateway
address and its MAC are reserved and can never be allocated to a machine.

The configured DNS address equals the configured gateway. smolworld is
authoritative for configured short names and `<machine>.<domain>` names. With
`network.egress: true`, unknown DNS queries are forwarded by the host gateway
to the upstream resolver `1.1.1.1:53`, with a bounded timeout and synthetic
`SERVFAIL` on failure. Without egress, unknown names return `NXDOMAIN` and
guests have no Internet route.

The external smolvm ABI supplies one complete static tuple for the private
virtio-net device: guest address, gateway, DNS address, and MAC. That device is
`eth0`. With `network.egress: true`, smolvm adds its existing host-side NAT
device as `eth1` and owns the default route there. The private world remains on
`eth0`; libkrun needs no source patch beyond the existing external Unix-stream
NIC support.

The host/virtio wire protocol is one big-endian 4-byte Ethernet-frame length
followed by raw frame bytes. Accepted streams become blocking before frame
reads; an idle healthy NIC is not a disconnect. Unknown, broadcast, and
multicast destination MACs flood to other attached ports. A known unicast
destination targets the learned port. Detach removes the port and its
forwarding-database entries.

Validate configuration, sealed material, and inspectable runtime prerequisites
before creating state, listeners, or machines. `up` owns only deterministic
`smw-...` machine names recorded in world state. `down`, signal cleanup,
restore-failure cleanup, and `release` never affect unrelated smolvm machines
or legacy state. Cleanup is always constrained by exact recorded identities.

Machine resources belong to the restricted Smolfile profile; smolworld does
not duplicate or override them in `.smolworld`.

## Commands and observations

The command surface is:

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

`Ctrl-C` stops and deletes the exact foreground world. `down` is safe after an
interrupted foreground process. `exec --secret-env GUEST=HOST_ENV` resolves a
caller-owned host environment variable for that command only. The value is
not stored in world state, the Smolfile, or the material lock. `cp` is scoped
to one recorded world machine and one regular file.

`ps` reports host lifecycle observations, not service health or readiness. Its
closed states are `created`, `attached`, `running`, `capturing`, `captured`,
and `absent`. `ps --json` emits the same rows as a JSON array.

### Metrics schema

`metrics --json` is read-only. It reads only world allocation records and
never lists or discovers unrelated smolvm machines. It emits a closed object
with exactly this top-level shape and literal schema value:

```json
{
  "schemaVersion": 1,
  "world": "world-name",
  "machines": []
}
```

Each machine row has exactly these fields:

```text
machine smolvmName state pid cpus memoryMb storageGb overlayGb
cpuSeconds cpuMillis rssMb diskUsedMb
```

Values are JSON `null` when the machine has no recorded allocation or an
observation is unavailable. CPU counters are cumulative for the current host
VMM process and reset on restart; RSS and disk are instantaneous host gauges.
These are not guest-process or guest-filesystem measurements. The smolvm
subprocess record is the literal upstream ABI label `machine-stats-v1`.
smolworld verifies that record's identity and lifecycle state before rendering
its own JSON. The label and the closed JSON schema are hard boundaries, not
best-effort parser hints or compatibility fallbacks.

## Checkpoint, restore, and release

`checkpoint` asks the foreground supervisor to close the switch at a new epoch,
capture every machine concurrently, seal one world receipt, publish it by
rename, and retain exactly those machine sources. `restore` accepts only a
receipt whose configuration, material lock, allocation, and topology match the
selected world; it creates fresh agent and Unix-stream NIC handles. `release`
is the normal deletion path for a retained checkpoint.

The receipt is a durable same-lineage world artifact. Higher-level systems may
record or reference it, but smolworld does not define their workflow, lease, or
state semantics.

A checkpoint is one coherent temporal cut across all machines and the switch:

1. Record the capture intent before external capture.
2. Close the switch at a new epoch; reject new actions and `exec` calls, stop delivering frames, and record the FDB and bounded queued state.
3. Pause every VM concurrently before capturing any machine.
4. Capture writable disk/overlay, RAM, virtual-device state, workspace, topology, and material identity. Captures contain no host file descriptors.
5. Seal machine receipts, switch state, and material receipts into one canonical world receipt and publish it atomically.
6. On failure, retain only exact unpublished candidates for reconciliation; never guess from broad process or name scans.

Restoring RAM from one point with disk or network state from another is
invalid. The initial implementation may exit captured VMs and restore a fresh
child; capture-and-continue is an optimization and must not change the receipt
or transaction contract.

The published checkpoint receipt includes a bounded BLAKE3 digest for each
opaque machine checkpoint-control receipt. `restore` and `release` recompute
those small digests before launching or deleting anything. smolvm remains the
owner of detailed RAM/disk file-identity and control-file checks. This digest
is an integrity anchor inside the same-user host trust boundary, not a
host-wide content audit or a cryptographic claim about guest state. Receipt
schema changes are intentionally incompatible unless a migration or
re-capture contract is defined.

## External integration boundary

Higher-level systems may treat a prepared world or retained checkpoint as an
immutable input to their own state, workflow, or evaluation models. The
runtime exposes only the typed world lifecycle and machine operations described
here. It does not own external records, policies, leases, workflow retries, or
executor semantics.

## Acceptance scenarios and measurements

The Redis foundation is the smallest real private-network fixture: a
Smolfile-composed Redis machine and long-lived runner machine. Its static
fixture check runs without a VM. The real gate proves
`prepare -> check -> up -> explicit DNS/Redis checks -> down`, and proves that
preparation/check create no runtime state and cleanup touches only recorded
machines and sockets. The prepared `redis.tar` archive is an external input;
tests never build it or invoke Docker, Compose, OrbStack, `DOCKER_HOST`, or a
Docker socket.

When external NIC behavior changes, the fork gate measures live
fork/reconnect and private traffic while keeping the frozen golden alive. When
checkpoint behavior changes, the durable gate captures a two-machine world,
exits the original supervisor, restores with fresh agent/NIC handles, verifies
workspace and Redis state, and performs exact `release` cleanup.

Measure phases separately rather than collapsing them into one VM number:

```text
logical fork/reference creation
checkpoint barrier request and switch quiescence
VM pause/capture per machine
manifest/CAS sealing
restore process launch
agent ready and NIC/DNS/private-traffic ready
restored world ready
accounted storage and physical APFS bytes
```

APFS clonefile observations distinguish addressed/accounted file blocks from
physical volume deltas. Accounted blocks may double-count shared clones;
volume-used bytes include unrelated host activity; neither is a proportional
guest-RAM measurement. The checkpoint hot path hashes small control files and
uses bounded file-identity receipts for immutable large clonefiles. A deep
full-content audit is an explicit offline operation, not a transition
precondition. Independent preparation and creation remain parallel while
declared dependency waves preserve deterministic order.

## Explicit non-goals and deferrals

smolworld does not provide Docker/Compose compatibility, workflow
orchestration, generic service readiness, health checks, restart policies, log
aggregation, host networking, port publishing, smolworld-owned NAT, TAP/vmnet,
DHCP, IPv6, guest SSH, registry pulls from guests, or workflow/executor policy.

It also does not define distributed scheduling, multi-world merge semantics,
GPU fabrics, a universal state codec, persistent agent personalities, or a
social chat system. Those concerns belong above the small, stable runtime and
control-plane boundaries. A replacement kernel or policy must be separately
evaluated and migrated; it must not be silently introduced as compatibility
code.
