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

## Companion adapter boundary

Smolworld has one narrow internal boundary for operations against the selected
smolvm binary. The adapter maps typed smolworld operations—preparation,
validation, lifecycle, statistics, command execution, copy, checkpoint, and
restore—to the existing upstream surface, then verifies versioned replies
before they enter world state. This is an implementation boundary, not a new
smolvm CLI protocol, Smolfile format, or parallel lifecycle specification.

The boundary is implemented by `src/companion_adapter.rs` and
`src/smolvm.rs`. Only `src/smolvm.rs` may name upstream command flags, TSV
field positions, or ABI literals. The rest of smolworld speaks in domain
operations and typed records. An upstream ABI change is handled at this
adapter boundary; it is never papered over with a fallback parser or a
Smolfile compatibility layer. Smolfiles and the smolvm command surface remain
upstream contracts. Public labels and schemas affected by an upstream change
remain owned by this world contract.

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

Smolfiles are TOML world material. smolworld validates the restricted
world-facing profile, seals its image input, and passes the resulting
local-only machine declaration to smolvm. The profile permits local or
immutable image material, command fields, environment, working directory, and
positive machine resources:

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
Smolfiles and local image archives, computes BLAKE3 identities, seals a
same-host regular-file identity receipt for every local archive, and writes the
lock beside the authored world. It does not allocate runtime state, bind a
listener, or create a machine. Normal `check` and `up` repeat host/runtime
prerequisite validation and compare the sealed archive receipts without
rereading large immutable archives. `check --deep` is the explicit full
content audit: it recomputes every sealed archive BLAKE3 identity. The receipt
is a bounded same-user-host mutation check, not a portable cryptographic
content audit; changed or incompatible material always requires another
explicit `prepare`.

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
An explicit `down` or `release` keeps those records when a companion deletion
fails; it never reports success or marks the world absent before exact cleanup
has completed.

Machine resources belong to the restricted Smolfile profile; smolworld does
not duplicate or override them in `.smolworld`.

## Commands and observations

The exact command forms and stable observation labels below are normative. The
[CLI guide](cli.md) is the separate operational reference for invocation,
sequencing, and examples; it does not create another contract.

The command surface is:

```text
smolworld config [-f PATH] [--format yaml|json]     Validate and render resolved configuration.
smolworld convert [-f PATH] [--format yaml|json]    Alias for config.
smolworld prepare [-f PATH]                         Resolve and seal local material.
smolworld check [-f PATH] [--deep]                  Validate prepared material read-only.
smolworld up [-f PATH] [-d] [SERVICE...]            Create/start services under the supervisor.
smolworld create [-f PATH] [SERVICE...]             Create recorded service configurations only.
smolworld start [-f PATH] [SERVICE...]              Start recorded services without deleting them.
smolworld stop [-f PATH] [SERVICE...]               Stop services without deleting their records.
smolworld restart [-f PATH] [SERVICE...]            Restart services through the supervisor.
smolworld rm [-f PATH] SERVICE...                   Delete stopped recorded service configurations.
smolworld ps [-f PATH] [OPTIONS] [SERVICE...]       Show host lifecycle observations.
smolworld stats [-f PATH] [OPTIONS] [SERVICE...]    Stream recorded-service host resource observations.
smolworld images [-f PATH] [--format table|json]    Show sealed service image material read-only.
smolworld exec [-f PATH] [OPTIONS] SERVICE CMD...   Delegate one command to a running service.
smolworld shell [-f PATH] SERVICE                    Open /bin/sh in a running service.
smolworld cp [-f PATH] SOURCE DEST                  Copy one regular file through the agent.
smolworld checkpoint [-f PATH] --output DIR         Capture and retain the running world.
smolworld restore [-f PATH] --checkpoint DIR        Restore a retained same-lineage world.
smolworld release [-f PATH] --checkpoint DIR        Delete exactly a retained world.
smolworld down [-f PATH]                            Stop and delete this world's machines.
smolworld version [--short|--format json]           Print smolworld version information.
```

CLI service names are logical keys from `machines`; they do not expose or
broaden the internal recorded `smw-...` machine identity. A service selection
is validated against that declaration before the runtime resolves exact state.
`up SERVICE...` includes each selected service's declared dependencies. It
does not add health checks or readiness waiting. `create` persists only
machine configurations; a later `start` from that stopped state launches the
same selected records under a detached supervisor. `stop`, `restart`, `rm`,
and a live `down` are delivered only through the process that owns the exact
world switch. `stop` retains machine records, `rm` accepts only stopped
services, and `down` still deletes the full world. A restored checkpoint
continues to require `release`, not `down` or `rm`.

`ps` reports host lifecycle observations, not service health or readiness. Its
closed states are `created`, `attached`, `running`, `stopped`, `capturing`,
`captured`, and `absent`. `ps --format table|json|TEMPLATE` accepts declared
service arguments; `--all` includes stopped/absent declarations, `--status`
or `--filter status=STATE` narrows by the closed label, and `--quiet` /
`--services` print names only. `--format json` emits one JSON row per line;
`--json` remains its spelling-compatible alias. Table columns deliberately
describe only world-owned observations: `SERVICE`, `IP`, `MAC`, and `STATUS`.

`SMOLWORLD_STATE_ROOT`, when set, selects an already-created absolute regular
directory for this world's local allocation namespace. Smolworld rejects a
relative, missing, or symlinked value; it never creates or follows a
caller-selected root. Sealed local executors use this boundary to keep their
disposable allocation, lifecycle, and generated-material state with the
private configuration materialization. With the variable unset, the ordinary
per-user `~/.smolworld` namespace remains the local CLI default.

### Stats schema

`stats` is read-only. It reads only world allocation records and never lists
or discovers unrelated smolvm machines. It streams an observation every
second by default; `--no-stream` prints one observation. `--all` includes
declared services with no current machine record. `--format table` is the
human presentation and templates may use `.Service`, `.Status`,
`.CPUSeconds`, `.RSSMb`, and `.DiskUsedMb`.

`stats --format json` (and `stats --json`) preserves the closed world snapshot
schema. Every streaming update is one complete newline-delimited object with
exactly this top-level shape and literal schema value:

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

`images` reads only `.smolworld.lock`; it does not call the upstream image
inspection command because that operation may start a stopped VM. Its table
and JSON Lines presentations identify the service, authored source, source
kind/digest, and sealed image digest.

`exec` accepts companion-supported `-e`/`--env`, `-w`/`--workdir`,
`-i`/`--interactive`, `-t`/`--tty`, `--stream`, `-d`/`--detach`, `--timeout`,
`--secret-env`, and `--secret-file` before `SERVICE`. The `--` separator is
accepted but not required. The service must already be running under this
world's supervisor; `exec` does not let the companion implicitly start a VM
without its switch listener. `shell` is exactly interactive `exec` of
`/bin/sh`. `cp` also requires its selected service to be running under this
world's live supervisor before it calls the companion, so a copy can never
implicitly boot or use an unswitched VM. It uses `SERVICE:/absolute/path`
endpoints and remains limited to one regular host file and one traversal-free
guest path because that is the selected companion's transfer capability.

## Checkpoint, restore, and release

`checkpoint` asks the foreground supervisor to close the switch at a new epoch,
capture every machine concurrently, seal one world receipt, publish it by
rename, and retain exactly those machine sources. `restore` accepts only a
receipt whose configuration, stable material identity, allocation, and topology
match the selected world. The identity binds resolver inputs and content
digests, while excluding regenerated private Smolfile paths and same-host fast
archive metadata; ordinary material verification checks those exact local
records before restore. If the selected world is a blank private materialization,
`restore` first records the receipt's exact allocation as captured state; it
never adopts a partial namespace. It then creates fresh agent and Unix-stream
NIC handles, passes each new listener path explicitly to SmolVM restore, and
rehydrates each restored machine as a new forkable base. A
restored world can therefore capture a later same-lineage checkpoint without
retaining the prior checkpoint source as a live VM. `release` is the normal
deletion path for a retained checkpoint.

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

The opt-in cold-transition harness, `tests/benchmark_world_transitions.py`,
records separate `create`, `start`, agent-exec, and fsynced guest-mutation
samples for archive-backed machines across serial and simultaneous 1/2/4
machine waves. It also uses a real `smolworld up` supervisor to record
material `prepare` and read-only `check`, each machine's supervisor-reported
`created` and `started` boundaries, each switch-reported private-NIC
attachment, and the all-machines-ready barrier. The direct machine matrix deliberately omits a
NIC; a raw Unix listener is not a valid substitute for the authoritative
switch, gateway, and attachment boundary. `start` remains the aggregate
upstream cost of image setup, VM creation, guest boot, agent readiness, and
workload launch until that external interface exposes stable lower-level
events. With `SMOLWORLD_TRANSITION_TRACE=1`, the harness also records nested
smolvm boot diagnostics, including agent-ready and local-layer-materialization
spans; those trace values are not additive substitutes for the external start
or attachment boundaries. A retained-fork reference is reported separately;
requests from one golden are serialized because a fork pauses that golden.

The optional prepared-world attachment profile takes an already sealed
configuration and one declared service through `SMOLWORLD_TRANSITION_PREPARED_WORLD`
and `SMOLWORLD_TRANSITION_ATTACH_SERVICE`. It records `config`, read-only
`check`, per-service `machine_created`, `machine_started`, and their elapsed
intervals to switch attachment, the world-ready barrier, the selected service's
host-lifecycle visibility from `ps --format json`, and successful `exec SERVICE
-- /bin/true` attachment. It first requires every declared
service to be `absent`; it does not invoke `prepare`, remove sealed material,
or adopt a non-idle world. The host-visible `running` observation and command
attachment remain distinct measurements: neither adds a service-health or
application-readiness contract.

APFS clonefile observations distinguish addressed/accounted file blocks from
physical volume deltas. Accounted blocks may double-count shared clones;
volume-used bytes include unrelated host activity; neither is a proportional
guest-RAM measurement. The checkpoint hot path hashes small control files and
uses bounded file-identity receipts for immutable large clonefiles. A deep
full-content audit is an explicit offline operation, not a transition
precondition. Independent preparation and creation remain parallel while
declared dependency waves preserve deterministic order.

## Explicit non-goals and deferrals

smolworld provides a deliberately small Compose-shaped command vocabulary; it
does not implement Docker/Compose configuration, project merging, profiles,
environment interpolation, replica/index/scale semantics, workflow
orchestration, generic service readiness, health checks, restart policies, log
aggregation, primary-workload attachment, event streams, guest process `top`,
wait/exit-event semantics, pause/kill controls, host networking, port
publishing, volumes, smolworld-owned NAT, TAP/vmnet, DHCP, IPv6, guest SSH,
registry pulls from guests, or workflow/executor policy. The unavailable
`logs`, `events`, `attach`, `top`, `wait`, `pause`, `unpause`, `kill`, `run`,
`scale`, `watch`, `ls`, `port`, `volumes`, `build`, `pull`, `push`, `publish`,
`commit`, and `export` commands are intentionally not compatibility shells:
the selected upstream CLI has no safe primitive or the world model has no
matching contract.

It also does not define distributed scheduling, multi-world merge semantics,
GPU fabrics, a universal state codec, persistent agent personalities, or a
social chat system. Those concerns belong above the small, stable runtime and
control-plane boundaries. A replacement kernel or policy must be separately
evaluated and migrated; it must not be silently introduced as compatibility
code.
