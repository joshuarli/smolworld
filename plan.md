# smolworld implementation plan and execution record

## Status

**Current tranche:** complete — implementation, E2E, module split, and library evaluation

| Tranche | Scope | Status | Evidence |
| --- | --- | --- | --- |
| 0 | Fix contracts, inspect smolvm/libkrun seams, establish repository plan | completed | Existing smolvm persistent lifecycle and built-in per-VM gateway inspected; libkrun Unix-stream frame contract confirmed |
| 1 | Add the narrow external virtio-net attachment contract to smolvm | completed | 8 focused smolvm tests, `cargo check -p smolvm --tests`, a debug build, and the real two-VM smolworld E2E pass against the local libkrun build |
| 2 | Create the dependency-free smolworld CLI, config/state model, and identity allocation | completed | Six std-only unit tests cover strict parsing, graph validation, CLI flags, and stable allocation |
| 3 | Implement the framed Ethernet switch plus ARP/DNS gateway | completed | Unit tests cover known/unknown L2 forwarding plus ARP and DNS A replies |
| 4 | Connect supervisor lifecycle to smolvm and implement `up`, `down`, `ps`, `exec` | completed | CLI construction, `cargo test`, `cargo clippy -- -D warnings`, and help smoke test pass |
| 5 | Add the Redis example and a local Apple-Silicon end-to-end test | completed | Live DNS → Redis PONG proof and `tests/e2e-redis.sh` pass on local macOS/Apple Silicon |
| 6 | Run final focused checks, update README, and record exclusions | completed | README/example, preflight, unit/lint checks, smolvm checks, libkrun smoke tests, and live E2E recorded below |
| 7 | Generalize the world schema and remove example-specific runtime assumptions | completed | Network domain/gateway/DNS, per-machine resources, `check`, neutral docs, and generic config tests pass |
| 8 | Add reproducible local Redis integration coverage and environment diagnostics | completed | Opt-in generic `cache` E2E proves DNS, PONG, SIGINT machine cleanup, and runtime-directory cleanup |
| 9 | Refactor the tested implementation into domain modules | completed | `main.rs` is an entry point; `cli`, `config`, `state`, `smolvm`, `switch`, `gateway`, `runtime`, and `model` have focused tests |
| 10 | Evaluate mature Rust libraries for parser/networking replacements | completed | [`docs/library-evaluation.md`](docs/library-evaluation.md) records a no-adoption decision and bounded future spikes |

Each tranche is marked complete only after its listed evidence passes. Failed checks and deliberate deferrals are recorded in the relevant tranche rather than hidden.

---

## Post-MVP expansion contract

The initial implementation proved the narrow static-L2 shape. The next pass makes
that shape usable as a generic local-world tool without broadening it into a
container orchestrator.

### Tranche 7 — generic and ergonomic world contract

The runtime must not contain Redis-specific behavior. Redis remains an example
and an integration fixture only. The configuration contract will gain explicit,
generic fields where the initial hard-coded defaults were insufficient:

* `[network]` may name a local DNS domain and static gateway/DNS address within
  the `/24`; the default remains the `.1` gateway/DNS identity.
* `[machines.NAME]` will support `cpus`, `memory_mib`, `storage_gib`, and
  `overlay_gib`, all optional and validated at config ingress. The small POC
  defaults remain one vCPU, 256 MiB memory, and 1 GiB sparse disks, but they no
  longer live as an implicit Redis decision in command construction.
* A machine image is still a local archive/rootfs in this isolated PoC, but the
  parser will give an explicit diagnostic that explains how to prepare any OCI
  image as an archive.
* `check` validates the world and the inspectable local runtime prerequisites
  without allocating state, binding sockets, or invoking a mutating smolvm
  command. `up` remains the deliberately simple foreground lifecycle owner.
* Runtime preparation will diagnose all host prerequisites before creating a
  machine: executable, agent rootfs, libkrun/libkrunfw pair, `mkfs.ext4`, and
  readable local image paths. This converts the discovered boot failures into
  actionable preflight output.

The strict parser remains intentionally small. It will reject unsupported TOML
instead of accepting fields that it cannot honor. No networking behavior changes:
there is still one `/24`, static provisioning, no egress/NAT/DHCP/IPv6, and one
external virtio NIC per machine.

### Tranche 8 — real integration coverage

Add an opt-in macOS test harness that:

1. builds or verifies the local `libkrun`/`libkrunfw` and agent-rootfs artifacts;
2. writes a temporary generic two-machine world using a prepared Redis archive;
3. starts `smolworld up` as a child process and waits for every attach;
4. verifies DNS and `redis-cli -h <service> ping` from the client through the
   real virtio NICs; and
5. interrupts `up`, then proves exact runtime-directory and namespaced-machine
   cleanup.

It may skip with a clear reason on non-macOS/non-Apple-Silicon hosts or missing
opt-in artifacts. A failed runtime prerequisite is a failed diagnostic test, not
evidence that the L2 code passed.

The current local investigation established: libkrun's existing Unix-stream API
is present; the standalone minimal `~/d/libkrun` can build with `BLK=1 NET=1`
after making Homebrew LLVM's `libclang.dylib` visible to bindgen; smolvm's
bundled dylibs must be hydrated with Git LFS; and a source checkout needs an
agent rootfs plus `e2fsprogs`. The remaining baseline `krun_start_enter(EINVAL)`
is independent of the external network attachment because it reproduces for a
network-less local-image probe. It must be diagnosed before claiming a passing
real E2E.

### Tranche 9 — module split

Only after tranches 7–8 establish the desired behavior, split `src/main.rs` by
the existing domain boundaries: `cli`, `config`, `state`, `smolvm`, `switch`,
`gateway`, and `runtime`. Preserve public CLI behavior and move each unit test
with its source module. This is a structure-only change with no simultaneous
feature work.

### Tranche 10 — library evaluation (not adoption yet)

Research parser and networking choices against the now-tested contracts:

* TOML: compare the current strict subset parser with `toml`/`serde` for error
  quality, unknown-field rejection, and dependency/maintenance cost.
* Ethernet/IP/DNS: compare the narrow handcrafted packet code with `smoltcp`.
  `smoltcp` is a candidate for packet parsing/checksums, not automatically for
  the Unix-stream framing, switch FDB, lifecycle, or authoritative nameservice.

Write the decision and migration boundary first. Per the working contract, no
third-party dependency is added until the user explicitly approves the selected
proposal.

---

## Goal

Build a macOS/Apple-Silicon-only proof of concept named `smolworld`. A foreground command:

```bash
smolworld up
```

loads `.smolworld`, starts a small set of smolvm machines on one isolated userspace Ethernet segment, and keeps that segment alive until interrupted. Guest traffic remains ordinary Ethernet/IP traffic. No TAP device, vmnet.framework, root privilege, host port publishing, NAT, or Internet egress is part of this PoC.

After preparing a local `redis.tar` archive as documented in the README, the final interactive proof is:

```bash
cd examples/redis
smolworld up
# in a second terminal
smolworld exec client -- redis-cli -h redis ping
# PONG
```

The real-VM E2E test is local-only and requires macOS on Apple Silicon; no CI runner is required to provide HVF.

---

## Resolved product decisions

### Static provisioning, not DHCP

v0 uses deterministic static IPv4 provisioning. `smolworld` assigns each guest's address, gateway, DNS server, and MAC; smolvm's existing early guest agent programs `eth0` and writes the resolver configuration before the workload starts.

Consequences:

* There is no DHCP server, lease state, or guest DHCP client in v0.
* The synthetic gateway implements only ARP and authoritative DNS.
* The plan deliberately does not claim unmodified non-smolvm Linux boot environments will configure themselves. The smolvm guest agent is the provisioning mechanism.
* IPv6 is disabled for this attachment mode. The smolvm patch must not silently apply the current default IPv6 values.

### smolvm remains the single-machine runtime

`smolworld` remains an independent Rust binary/repository. It owns declarative worlds, cross-machine identity, L2 switching, DNS, runtime sockets, and group lifecycle. `smolvm` keeps machine image/lifecycle/agent/libkrun responsibilities.

The PoC uses persistent smolvm machines because they directly support the required split-terminal `exec` flow:

```text
smolworld up
  -> smolvm machine create --name <world-machine> ...
  -> smolvm machine start  --name <world-machine>

smolworld exec client -- command ...
  -> smolvm machine exec --name <world-client> -- command ...
```

On normal shutdown and `down`, smolworld stops and deletes only its namespaced machines. It never deletes unrelated smolvm machines.

### Dependency ordering and readiness

`depends_on` means start order only. It validates that every referenced machine exists and that the graph is acyclic, then starts independent machines in topological order. It has no health checks, service readiness gates, restart policy, or Compose compatibility.

The E2E assertion retries the Redis command briefly after both machines and their Ethernet links are attached. That retry belongs to the test, not to general world orchestration.

### Log behavior

Persistent smolvm starts intentionally detach workload stdout/stderr. Therefore v0 does **not** promise prefixed, retained workload-log streaming. `up` emits prefixed lifecycle, attach, and error messages; `exec` streams the requested command normally. Adding long-lived workload log collection would be a separate smolvm capability and is deferred.

---

## Exact ownership boundary

```text
smolworld
  - parse and validate .smolworld
  - preserve world identity and machine allocations
  - create private Unix listeners, run the switch/gateway, and remove them
  - create/start/stop/delete namespaced smolvm machines
  - delegate exec/status to smolvm

smolvm (small companion patch)
  - create one virtio-net NIC backed by an externally owned Unix stream
  - apply static IPv4/MAC/gateway/DNS to the smolvm guest agent
  - retain this network attachment in persistent machine configuration
  - keep all existing per-VM gateway, TSI, port-forward, and egress paths unchanged

libkrun
  - unchanged unless the installed local checkout lacks the existing
    krun_add_net_unixstream support
```

No multi-machine orchestration or DNS/L2 logic moves into smolvm. No VMM, virtio ring, or libkrun lifecycle machinery is reimplemented by smolworld.

---

## smolvm external attachment contract

The existing smolvm `virtio-net` backend remains named `virtio-net`: it is still a virtio NIC. The new option describes where the host end is attached rather than inventing another network backend.

The smolvm patch exposes an IPv4-only attachment configuration equivalent to:

```text
--net-backend virtio-net
--net-unixstream /tmp/smw-<world-hash>/p-<machine-hash>.sock
--net-address 10.89.0.2/24
--net-gateway 10.89.0.1
--net-dns 10.89.0.1
--net-mac 02:xx:xx:xx:xx:xx
```

The final flag spelling may use one typed `ExternalNetworkConfig` internally, but its persisted contract contains exactly those values. It is valid only when all values are present. It is incompatible with:

* TSI;
* host port mappings;
* host egress allow-lists or DNS filtering;
* IPv6 configuration; and
* any second NIC.

At machine boot, smolvm passes the listener path to `krun_add_net_unixstream`, uses the supplied MAC when creating the virtio NIC, and gives the static IPv4 tuple to the guest agent. smolvm does **not** open a socketpair or start `smolvm-network`'s normal NAT gateway for this mode.

`ExternalNetworkConfig` must be serializable and be persisted in the smolvm machine record so `machine create` followed by `machine start` reconstructs the same attachment. Existing TSI and built-in virtio-net records must deserialize unchanged.

The listener is owned by smolworld before the machine starts. libkrun connects to it exactly once during boot. Its stream protocol is already defined by libkrun:

```text
[4-byte unsigned big-endian frame length][raw Ethernet frame]
```

There is no virtio-net header and all advertised offloads are disabled. smolworld treats a zero-length, over-large, truncated, or malformed frame as a port failure. Socket directories are mode `0700`; socket paths use compact hashes under `/tmp` to stay below Darwin `sun_path` limits.

---

## `.smolworld` configuration contract

v0 accepts only this intentionally small TOML-shaped schema:

```toml
[world]
name = "demo"

[network]
subnet = "10.89.0.0/24"

[machines.redis]
image = "redis:8"
command = ["redis-server"]

[machines.client]
image = "redis:8"
command = ["sleep", "infinity"]
depends_on = ["redis"]
```

Rules:

* `[world]`, `[network]`, and at least one `[machines.<dns-label>]` are required.
* `world.name` and machine names are lowercase RFC-1123-style DNS labels. They are also used in error messages and DNS records.
* `network.subnet` accepts IPv4 CIDR only. v0 requires a `/24`; `.1` is the gateway and `.0`/`.255` are never assigned.
* `image` is required and is an absolute path or a `./`/`../` path relative to the config file. It names a local Docker-save archive or unpacked root filesystem. `command` is optional and otherwise uses the image default command. `depends_on` is optional.
* Duplicate machine names, unknown keys, malformed arrays/strings, missing dependencies, and dependency cycles are errors.
* Registry image references, resource settings, volumes, ports, environment, multiple networks, and multiple NICs are deliberately not parsed.

The first implementation uses a small strict parser for precisely this schema rather than adding a TOML dependency. It must reject unsupported TOML constructs instead of accepting and ignoring them.

The host prepares the local archive (for example `docker save redis:8 -o redis.tar`) before `up`. This is required because smolvm otherwise pulls a registry image from inside the guest, and the PoC intentionally provides no guest egress. Guest egress remains absent.

---

## Identity, persisted state, and names

`smolworld` stores durable allocation state under:

```text
$HOME/.smolworld/world-<hash>/state
```

`<hash>` is a stable, local FNV-1a hash of the canonical configuration path. The state file contains its format version, a world seed, and each machine's assigned IPv4, MAC, and smolvm machine name. It is written atomically through a same-directory temporary file followed by rename.

Allocation policy:

1. Gateway is always `<subnet>.1` with MAC `02:00:00:00:00:01`.
2. Existing state assignments for declared machines are retained unchanged.
3. A new machine receives a candidate address in `.2` through `.254` from a stable hash of the world seed and machine name. Linear probing skips already persisted assignments. Exhaustion is a validation failure.
4. A new guest MAC derives from the same inputs, has locally administered and unicast bits set, and is rejected/retried if it collides with an existing allocation or the gateway MAC.
5. smolvm names are deterministic and namespaced: `smw-<world-hash>-<machine-hash>`. They never use the user-facing machine name directly, preventing collisions with unrelated smolvm machines.

The state file is durable only for identity. Machine disks and process state are not world-persistent: `down`, normal `up` cleanup, and Ctrl-C stop/delete the namespaced smolvm machines and remove transient socket files.

---

## Networking design

### Ports and transport

One Unix-stream listener belongs to every configured machine. A successful accept creates one switch port. The switch runs only for the foreground `up` lifetime.

The minimal implementation uses a bounded thread-per-port design suitable for the PoC:

* listener/reader thread: accepts one connection, decodes length-prefixed frames, and sends switch events;
* switch thread: owns the forwarding database and gateway and decides all deliveries;
* each port writer is protected by one mutex so frame headers and frame bytes cannot interleave;
* an EOF, protocol error, or write failure detaches that port and removes every FDB entry pointing to it.

There is no async runtime and no global mutable switch state. The port count is the parsed machine count; no hot attach API exists beyond the boot listeners.

### L2 behavior

For each valid Ethernet frame from a guest port:

1. Learn `source MAC -> ingress port`, overwriting a stale mapping.
2. For a known unicast destination, send only to that port unless it is the ingress port.
3. For unknown unicast, broadcast, and multicast destinations, flood to every other guest port and offer the frame to the synthetic gateway.
4. For the fixed gateway MAC, offer the frame only to the gateway.
5. Gateway-produced frames are injected as if they originated at the gateway port and follow the same destination/flood rules.

Frames do not include an Ethernet FCS. v0 rejects frames shorter than an Ethernet header and frames larger than 64 KiB; guest MTU is 1500. VLANs and all offloads are unavailable.

### Synthetic gateway

The gateway is not a router. It has the fixed IP/MAC identity above and only:

* replies to ARP requests for its IPv4 address;
* responds to authoritative UDP/IPv4 DNS A queries for world machine names; and
* responds with an empty `NOERROR` answer for known non-A questions and `NXDOMAIN` for names outside the world.

It does not forward arbitrary IP, TCP, UDP, DNS upstream, DHCP, ICMP, IPv6, or traffic to the host. The DNS implementation preserves transaction ID and question bytes, emits a valid IPv4/UDP/Ethernet reply, and calculates the IPv4 checksum. A zero UDP checksum is valid for IPv4 and keeps the implementation small.

Guest-to-guest TCP/UDP works because addresses share one subnet and guests resolve/ARP each other directly through the switch; the gateway sees only ARP/DNS/broadcast traffic.

---

## CLI and lifecycle contract

```text
smolworld up [-f PATH]
smolworld down [-f PATH]
smolworld ps [-f PATH]
smolworld exec [-f PATH] MACHINE -- COMMAND [ARG...]
```

`-f` defaults to `.smolworld` in the current directory. The config's parent directory is the world directory.

`up` performs, in order:

1. Parse and validate configuration, dependency graph, state allocations, and smolvm executable availability.
2. Stop/delete only stale namespaced machines listed in state.
3. Create the private runtime directory and all per-machine listeners.
4. Start the switch/gateway thread.
5. Create persistent smolvm machines with their external attachment config.
6. Start them in topological order.
7. Wait for one socket attachment per machine with a bounded timeout.
8. Print a compact allocation table and run until SIGINT/SIGTERM.
9. On normal signal, stop/delete managed machines, stop the switch, and remove the runtime directory.

`down` reads config and state, stop/deletes only its recorded machine names, and removes the exact runtime directory. It is idempotent. It must not kill a PID merely because the numeric PID has been reused.

`ps` delegates status lookup to smolvm for each recorded machine and presents the user-facing name plus deterministic IPv4/MAC. `exec` resolves a user-facing name to its state record and delegates its remaining arguments to `smolvm machine exec --name ... -- ...`.

`SMOLWORLD_SMOLVM` may override the smolvm executable for local development. It is never persisted. The default is `smolvm` on `PATH`.

For this POC, every created machine passes `--cpus 1 --mem 256 --storage 1 --overlay 1`; it never requests GPU or CUDA. These are the smallest values selected for a reliable Redis guest rather than smolvm's much larger general-purpose defaults.

---

## Test strategy

### smolvm companion patch

Focused tests prove:

* external attachment CLI parsing rejects incomplete configuration;
* a valid attachment persists and reloads in the machine record;
* external attachment selects the Unix-stream path rather than smolvm's built-in gateway;
* its guest environment is IPv4-only and contains the supplied static tuple; and
* existing TSI and built-in virtio-net selection tests still pass.

### smolworld unit tests

Tests run without a VM:

* strict config parsing, unknown-key rejection, `/24` validation, and dependency cycles;
* state preservation, allocation determinism, collision probing, and exhaustion;
* framed stream input rejects invalid lengths; writer serialization prevents headers and payloads from interleaving;
* source learning, known unicast, unknown unicast flooding, broadcast/multicast flooding, and FDB cleanup on detach;
* ARP reply construction and DNS A / no-data / NXDOMAIN responses.

### Local macOS integration test

The README documents an opt-in two-terminal Redis test using the patched local smolvm and a host-prepared `redis.tar`. When the local libkrun build is usable, it should:

1. launches `examples/redis/.smolworld`;
2. verifies `getent hosts redis` from `client`;
3. retries `redis-cli -h redis ping` until `PONG`;
4. interrupts `up` and confirms its sockets and namespaced smolvm machines are cleaned up.

The exact client image must include `getent` and `redis-cli`; `alpine:latest` is not suitable alone.

---

## Tranche detail

### Tranche 0 — contracts and discovery

* Inspect both working trees and their instructions without changing smolvm state.
* Record the existing libkrun frame protocol and smolvm’s current built-in gateway behavior.
* Lock scope decisions above.

**Completion criteria:** the rest of this document contains no unresolved architectural choice that blocks coding.

### Tranche 1 — smolvm external attachment

* Introduce a typed external IPv4 attachment config and persist it in machine state.
* Thread it through run/create/start paths and both static/dynamic launchers as required by existing source conventions.
* Connect libkrun to the supplied Unix listener; skip the internal `smolvm-network` gateway in this mode.
* Reuse the existing guest agent static interface setup but permit IPv4-only configuration.
* Add narrow regression tests before changing behavior.

**Completion criteria:** a manually created local smolvm machine can attach its NIC to a Unix listener with a deterministic IPv4/MAC/DNS tuple.

### Tranche 2 — smolworld configuration and state

* Create the Rust package without third-party dependencies.
* Implement strict minimal configuration parsing, validation, dependency sorting, state read/write, allocation, and CLI argument parsing.
* Add unit tests for observable config and identity behavior.

**Completion criteria:** test-only world planning deterministically produces the expected machine launch specs and survives a state reload.

### Tranche 3 — switch and gateway

* Implement framed stream helpers and a synchronous switch core testable with synthetic ports.
* Implement port threads/runtime and clean detach semantics.
* Implement ARP plus authoritative DNS packet replies.
* Add packet-level unit tests before integrating VMs.

**Completion criteria:** synthetic Ethernet frames prove L2 forwarding and DNS/ARP replies without smolvm.

### Tranche 4 — supervisor

* Implement namespaced smolvm command execution, attach timeout, signals, cleanup, and all four CLI commands.
* Keep `up` foreground and lifecycle-only logging.
* Add command construction and cleanup tests where they do not require HVF.

**Completion criteria:** CLI controls only its recorded smolvm machines and leaves no socket directory after normal shutdown.

### Tranche 5 — example and local E2E

* Add Redis/client example assets with no guest package installation requirement.
* Document the two-terminal local test and concise expected output.
* Run the full demo on the local Apple-Silicon host.

**Completion criteria:** DNS → Redis TCP PONG passes through real virtio NICs.

### Tranche 6 — documentation and final verification

* Add README architecture, prerequisites, build instructions for the companion smolvm checkout, demo commands, and explicit exclusions.
* Run formatting, focused unit tests, smolvm tests, and local E2E if the host prerequisites are available.
* Record every command actually run and any skipped check in this file.

**Completion criteria:** a fresh reader can reproduce the local PoC without discovering hidden behavior from source.

---

## Execution log

| Time | Action | Result |
| --- | --- | --- |
| 2026-08-12 | Began implementation plan and inspected local smolvm checkout | in progress |
| 2026-08-12 | Completed tranche 0 | smolvm's persistent `machine create/start/exec` seam, its private virtio-net gateway, and static guest agent provisioning were confirmed; no libkrun patch is presently required |
| 2026-08-12 | Implemented smolvm external attachment | Added persisted `ExternalNetworkConfig`, complete CLI validation, Unix-stream libkrun attachment, built-in gateway bypass, and IPv4-only guest agent environment. `cargo check -p smolvm` passes. |
| 2026-08-12 | Attempted normal focused smolvm tests | Blocked at link: local smolvm has no linkable `libkrun`; its vendored fallback also fails to build. The separate `~/d/libkrun` checkout lacks Homebrew `virglrenderer`, so no source patch was made. |
| 2026-08-12 | Ran smolvm unit contract tests | Passed: `LIBKRUN_DIR=<temporary empty dylib> DYLD_LIBRARY_PATH=<same> cargo test -p smolvm external --lib` (8 tests). The shim satisfied macOS weak-linking only; no libkrun function or VM was invoked. |
| 2026-08-12 | Completed tranches 2–4 implementation | Added dependency-free `smolworld`, durable allocation state, L2 switch, ARP/DNS gateway, lifecycle CLI, README, and Redis configuration. `cargo test` (6 tests) and `cargo clippy -- -D warnings` pass. |
| 2026-08-12 | Final non-VM compile check | Passed: `cargo check -p smolvm --tests`; `git diff --check` passes in both working trees. |
| 2026-08-13 | Prepared local runtime artifacts | Built minimal `~/d/libkrun` with `BLK=1 NET=1`; hydrated smolvm's LFS dylibs; built `target/agent-rootfs`; installed and exposed Homebrew `e2fsprogs`. |
| 2026-08-13 | Isolated the prior `krun_start_enter(-22)` failure | A network-less smolvm probe failed at `hv_vm_create`, while libkrun's VM-config, block-disk, and TSI tests passed. The locally compiled `target/debug/smolvm` had no Hypervisor Framework entitlement. Ad-hoc signing it with checked-in `smolvm.entitlements` fixed the baseline boot; no libkrun source patch was needed. |
| 2026-08-13 | Proved the smolvm external attachment | A signed smolvm started a network-less Redis machine (`redis-cli ping` → `PONG`), then smolworld attached two real virtio NICs. The first attempt found and fixed accepted Unix-stream sockets inheriting nonblocking mode; the regression test now proves idle ports block rather than detach. |
| 2026-08-13 | Completed generic/ergonomic tranche | Added generic `[network]` gateway/DNS/domain values, per-machine resources, image/runtime preflight, `check`, entitlement diagnostics, and neutral documentation. `cargo test` (10) and `cargo clippy -- -D warnings` pass. |
| 2026-08-13 | Completed real integration coverage | `SMOLWORLD_E2E=1 bash tests/e2e-redis.sh` passed: it created a temporary generic `cache` world, resolved `cache.e2e.test`, received Redis `PONG`, and verified only its exact smolvm machines and `/tmp/smw-<hash>` runtime directory were removed. |
| 2026-08-13 | Completed module split and dependency review | Moved behavior into `cli`, `config`, `state`, `smolvm`, `switch`, `gateway`, `runtime`, and `model`; reran unit/lint/live E2E. Reviewed `toml`/Serde and smoltcp; recorded deferred, no-dependency recommendation in `docs/library-evaluation.md`. |
