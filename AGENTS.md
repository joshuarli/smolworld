# smolworld architecture and scope

## Purpose

`smolworld` is a local macOS/Apple-Silicon runner for a small, statically
provisioned group of smolvm machines. A world is described by one `.smolworld`
file and runs on a private userspace Ethernet segment.

It is deliberately not a container orchestrator or general virtual network.
Keep these exclusions unless the user explicitly expands the product contract:

* no host networking, port publishing, smolworld-owned NAT, TAP/vmnet, DHCP,
  or IPv6 on the private world NIC; explicit guest Internet egress is delegated
  to smolvm's existing host-side NAT runtime;
* no service health checks, restart policies, log aggregation, or Compose
  compatibility;
* no registry image pulls from guests—images are host-prepared local archives
  or unpacked rootfs paths; and
* no third-party Rust dependency without explicit user approval.

`depends_on` means creation/start order only. It is not a readiness or health
contract.

## Ownership boundary

```text
.smolworld
    │
    ▼
smolworld
  config + durable allocation state + Unix-stream L2 switch + ARP/DNS gateway
  world lifecycle and namespaced smolvm command delegation
    │
    ▼
patched smolvm
  persistent machine/image lifecycle + guest agent static IPv4 provisioning
  eth0 attached to smolworld's Unix listener + optional eth1 NAT egress NIC
    │
    ▼
libkrun
  VMM and virtio implementation
```

smolworld owns cross-machine identity, Ethernet forwarding, authoritative
local DNS (and upstream forwarding when egress is enabled), socket lifecycle,
and group cleanup. smolvm owns each VM, its guest agent, OCI image handling,
the optional NAT egress relay, and libkrun invocation. The current upstream DNS
forwarder is `1.1.1.1:53`; it is host-side forwarding, not guest access to a
second DNS service. Do not move L2/DNS/world logic into smolvm, and do not
reimplement VMM or virtio behavior here.

The companion smolvm contract is an external virtio-net attachment with a Unix
stream path and a complete static IPv4 tuple: guest address, gateway, DNS, and
MAC. It remains the first guest NIC (`eth0`). When world egress is enabled,
smolvm adds its existing host-side NAT runtime as a second virtio-net NIC
(`eth1`) and owns the default route there; libkrun explicitly supports multiple
`krun_add_net_unixstream` devices. smolworld still owns the private switch and
local DNS, while smolvm owns the egress relay and policy.

## Module map

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | Binary entry point only. |
| `src/cli.rs` | CLI grammar and help text. |
| `src/config.rs` | Strict `.smolworld` parser, semantic validation, dependency ordering. |
| `src/model.rs` | Shared world, machine, network, state, and identity types. |
| `src/state.rs` | Durable allocation state, stable address/MAC assignment, private paths. |
| `src/smolvm.rs` | Preflight and the narrow smolvm subprocess boundary. |
| `src/switch.rs` | Framed Unix-stream ports, MAC learning, Ethernet forwarding, cleanup. |
| `src/gateway.rs` | Synthetic gateway ARP and authoritative DNS A replies. |
| `src/runtime.rs` | `check`, `up`, `ps`, `exec`, `down`, signals, and supervisor cleanup. |

Keep changes in their owning module. `model` contains the cross-module domain
contract; update its users and tests deliberately when changing it.

## Durable invariants

* A world has exactly one IPv4 `/24` subnet. Every guest gets a stable static
  IPv4/MAC assignment persisted under `~/.smolworld`.
* The configured DNS address equals the configured gateway; this process
  implements the authoritative local service. It answers configured short names
  and `<machine>.<domain>` names, and forwards unknown names upstream only when
  egress is enabled. Upstream forwarding uses `1.1.1.1:53` and a bounded
  request timeout; failures return synthetic `SERVFAIL`.
* The gateway address and MAC are reserved: allocation must never give either
  to a machine.
* The host/virtio wire protocol is one big-endian 4-byte Ethernet-frame length
  followed by the raw frame. Accepted streams must be switched to blocking
  mode before frame reads; an idle healthy NIC is not a disconnect.
* Unknown/broadcast/multicast destination MACs flood to other attached ports;
  known unicast targets the learned port. Detach must remove the port and its
  FDB entries.
* `up` owns only deterministic `smw-v2-...` machine names recorded in its v2
  world state. `down` and signal cleanup must never affect unrelated smolvm
  machines or v1 state.
* Images are prepared local material referenced by Smolfiles. Validate all
  configuration and inspectable runtime prerequisites before state, listeners,
  or machines are created; guests never pull images.
* Machine resources belong to the restricted Smolfile profile. smolworld does
  not duplicate or override them in `.smolworld`.

## Runtime requirements

The local source-build workflow needs a patched/signed smolvm binary, matching
`libkrun` and `libkrunfw`, an agent rootfs, and `mkfs.ext4`. On macOS, an ad-hoc
`target/debug/smolvm` must have the checked-in `smolvm.entitlements` applied or
Hypervisor Framework VM creation fails with an opaque EINVAL. `smolworld check`
must remain non-mutating and diagnose these conditions before `up`.

## Verification

Run the narrowest relevant check first, then normally finish a feature change
with:

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
git diff --check
```

The real local Redis foundation integration test is opt-in because it creates
VMs and needs Apple Hypervisor Framework plus prepared artifacts. It must run
without Docker, Compose, OrbStack, `DOCKER_HOST`, or a Docker socket:

```bash
SMOLWORLD_E2E=1 \
SMOLWORLD_SMOLVM=/path/to/smolvm \
SMOLVM_AGENT_ROOTFS=/path/to/agent-rootfs \
SMOLVM_LIB_DIR=/path/to/smolvm/lib \
bash tests/e2e-redis-foundation.sh
```

It proves generic static DNS, Redis TCP through real virtio NICs, and exact
machine/runtime cleanup. Keep it generic: Redis is a workload fixture, never
runtime behavior.

The companion smolvm patch has focused external-network tests. Run them with
the local libkrun build when changing that boundary:

```bash
LIBKRUN_DIR="$HOME/d/smolvm/lib" \
DYLD_LIBRARY_PATH="$HOME/d/smolvm/lib" \
cargo test -p smolvm external --lib
```

Do not run pre-commit hooks or push a remote. Do not add a dependency, expand
the networking/product scope, or make destructive cleanup broader than the
recorded world without user approval.
