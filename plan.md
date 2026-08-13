Build an end-to-end macOS-native PoC called **`smolworld`**: a minimal orchestration/networking layer over **smolvm + libkrun** where multiple smolVM guests share one deterministic userspace virtual network.

## Goal

This command:

```bash
smolworld up
```

run in a directory containing `.smolworld` should:

1. Create one userspace virtual Ethernet network.
2. Start all declared smolVMs.
3. Attach each guest's virtio-net NIC to the shared network.
4. Assign deterministic MAC + IPv4 addresses.
5. Provide internal DNS using machine names.
6. Allow arbitrary TCP/UDP communication directly between guests.

The primary acceptance test is:

```text
client -> DNS "redis" -> redis VM -> TCP -> PONG
```

No host networking privileges should be required.

Target **macOS 26+, Apple Silicon only**. Do not attempt portability.

---

## Architecture

Keep responsibilities sharply separated:

```text
smolvm
  machine lifecycle + libkrun/HVF

smolworld
  declarative world definition
  process orchestration
  network lifecycle
  machine identity

smolworld-net
  userspace Ethernet switch
  DHCP/IPAM
  DNS
```

Do **not** reimplement a VMM or smolvm lifecycle machinery.

If necessary, make the smallest possible patch to smolvm/libkrun integration to expose an external virtio-net backend over a Unix socket.

Prefer upstream-compatible changes.

---

## `.smolworld`

Use a minimal TOML format.

Example:

```toml
[world]
name = "demo"

[network]
subnet = "10.89.0.0/24"

[machines.redis]
image = "redis:8"
command = ["redis-server"]

[machines.client]
image = "alpine:latest"
command = ["sleep", "infinity"]
depends_on = ["redis"]
```

Running:

```bash
smolworld up
```

should load `.smolworld` automatically.

Also support:

```bash
smolworld up -f path/to/file.smolworld
smolworld down
smolworld ps
smolworld exec client -- sh
```

Do not implement Docker Compose compatibility yet.

---

## Networking

Implement a real userspace Ethernet switch.

Each VM's virtio-net device connects to `smolworld` over a Unix-domain socket.

Conceptually:

```text
VM redis eth0 ── UDS ──┐
                       │
VM client eth0 ─ UDS ──┼── L2 switch
                       │
gateway port ──────────┘
```

The switch must support:

* source MAC learning
* known unicast forwarding
* unknown unicast flooding
* broadcast flooding
* basic multicast flooding
* port attach/detach

Do not route VM-to-VM traffic through a TCP proxy.

VM-to-VM packets should remain ordinary Ethernet/IP packets.

---

## Gateway

Create a synthetic gateway port, e.g.:

```text
MAC: 02:00:00:00:00:01
IP:  10.89.0.1
```

The gateway only needs:

* ARP
* DHCP
* DNS

Using `smoltcp` is reasonable.

No Internet egress is required.

---

## Deterministic identity

Addresses must be stable across repeated runs.

Derive or persist identity from:

```text
world
machine name
NIC index
```

Example:

```text
gateway  10.89.0.1
redis    10.89.0.2
client   10.89.0.3
```

MAC addresses must likewise be deterministic and locally administered.

DHCP should communicate the predetermined allocation rather than choosing arbitrary addresses.

---

## DNS

The built-in DNS service should resolve machine names automatically:

```text
redis  -> 10.89.0.2
client -> 10.89.0.3
```

Inside `client`:

```bash
getent hosts redis
```

must succeed.

Then:

```bash
redis-cli -h redis ping
```

must return:

```text
PONG
```

Do not require `/etc/hosts` manipulation.

---

## Scope exclusions

Do **not** implement:

* Internet/NAT egress
* host port publishing
* IPv6
* TAP devices
* `vmnet.framework`
* privileged helpers
* network namespaces
* multiple networks per VM
* multiple NICs
* VLANs
* ACLs
* latency/loss injection
* bandwidth shaping
* PCAP capture
* snapshots/forks
* Docker Compose parsing
* Kubernetes/CNI compatibility
* persistent background daemon

Keep this PoC aggressively small.

---

## CLI behavior

`smolworld up` should run the world as a foreground supervisor.

It should:

1. Parse `.smolworld`.
2. Validate configuration.
3. Build deterministic network assignments.
4. Start the virtual network.
5. Start machines respecting `depends_on`.
6. Stream prefixed machine logs.
7. Handle Ctrl-C.
8. Stop all machines and remove transient sockets/state cleanly.

`smolworld down` may initially only clean up leftover state from an interrupted run.

Runtime state can live under:

```text
~/.smolworld/
```

or an appropriate macOS application-support/runtime directory.

---

## Implementation constraints

Prefer **Rust** for `smolworld`.

Keep dependencies modest.

Prioritize:

* deterministic behavior
* explicit state machines
* structured errors
* strong cleanup semantics
* testable networking components
* minimal hidden global state

Avoid introducing an async framework unless it clearly simplifies the implementation. `smolworld-net` should be independently testable without launching VMs.

Do not build abstractions for hypothetical future portability.

---

## Testing

Implement unit/integration tests for the virtual switch without VMs:

* MAC learning
* unicast
* broadcast
* port removal
* deterministic IP allocation
* DNS lookup

Then provide an end-to-end test using at least two real smolVMs.

Required final demo:

```bash
git clone ...
cd examples/redis
smolworld up
```

with `.smolworld` defining `redis` and `client`.

Then:

```bash
smolworld exec client -- redis-cli -h redis ping
```

returns:

```text
PONG
```

Also demonstrate direct TCP/UDP traffic between guests.

---

## Deliverables

Produce:

* working `smolworld` CLI
* `.smolworld` parser
* userspace L2 switch
* deterministic DHCP/IPAM
* internal DNS
* smolvm/libkrun network attachment
* graceful lifecycle management
* automated tests
* `examples/redis/.smolworld`
* concise README explaining architecture and how to run the demo

If smolvm lacks the exact external-network attachment hook required, inspect its current libkrun integration and implement the **smallest narrowly scoped patch** necessary rather than redesigning smolvm.

The project is successful when two ordinary, unmodified Linux guests launched through smolvm can join one isolated userspace network and communicate by machine name with:

```bash
redis-cli -h redis ping
# PONG
```

Everything beyond that is explicitly deferred.

---

**Rust is the most practical language for this PoC, by a fairly wide margin.**

The main reason is architectural fit. `libkrun` exposes a small C API and already has a Rust-heavy ecosystem around it; `krunvm` itself is ~99% Rust, and `smolvm` sits directly on libkrun/HVF on macOS. ([GitHub][1]) That makes Rust the path of least resistance for FFI, Unix sockets, packet handling, lifecycle supervision, and eventually deeper integration with smolvm.

* **Rust — best choice.** Excellent for the L2 switch and packet path, easy C FFI into libkrun, strong ownership/lifecycle semantics for VM processes and sockets, and no runtime/GC complications in the networking core. It also keeps the project close to the implementation language of adjacent libkrun tooling.

Keep v0 boring:

```text
smolworld
├── cli/
├── config/          # .smolworld TOML
├── world/
│   ├── supervisor
│   ├── machine
│   └── identity
├── net/
│   ├── switch
│   ├── port
│   ├── ethernet
│   ├── ipam
│   ├── dhcp
│   └── dns
└── smolvm/
    └── adapter
```

One Rust binary.

The smolvm integration layer should initially be **process-oriented**, not SDK-oriented. Treat `smolvm` as a CLI/runtime dependency:

```rust
Command::new("smolvm")
    .args([...])
```

and add exactly one small upstream smolvm capability if needed to expose the external virtio-net Unix socket.

That is preferable to binding against an outdated Python/Node SDK because it preserves the abstraction boundary:

```text
smolworld
    │
    │ stable CLI / explicit network socket contract
    ▼
smolvm
    │
    ▼
libkrun
```

Then, **only if process orchestration becomes limiting**, move the adapter down to libkrun's C API directly. libkrun is explicitly designed to be embedded through that small C interface. ([GitHub][3])

For dependencies, I’d keep it similarly restrained

```text
lexopt      CLI
miniserde if aplpicable
toml        .smolworld parsing
thiserror   errors
nix         Unix process/socket primitives
socket2     if needed
smoltcp     gateway packet stack
tracing     observability
```

I would **not start with Tokio** unless the Unix socket multiplexing genuinely becomes painful without it. A simple poll/kqueue-driven event loop is a very natural fit for the first version:

```text
kqueue
  ├── VM A UDS readable
  ├── VM B UDS readable
  ├── VM C UDS readable
  ├── child process exited
  └── signal received
```

That gives you a beautifully deterministic core and aligns with where this project could eventually go: thousands of tiny worlds with extremely explicit resource accounting.

