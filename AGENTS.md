# smolworld architecture and scope

## Purpose

`smolworld` is a local macOS/Apple-Silicon runner for a small, statically
provisioned group of smolvm machines. A world is described by one `.smolworld`
file and runs on a private userspace Ethernet segment.

It is deliberately not a container orchestrator or general virtual network.
Keep these exclusions unless the user explicitly expands the product contract:

* no host networking, port publishing, NAT, TAP/vmnet, DHCP, IPv6, or guest
  Internet egress;
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
  one virtio-net NIC attached to smolworld's Unix listener
    │
    ▼
libkrun
  VMM and virtio implementation
```

smolworld owns cross-machine identity, Ethernet forwarding, authoritative
local DNS, socket lifecycle, and group cleanup. smolvm owns each VM, its guest
agent, OCI image handling, and libkrun invocation. Do not move L2/DNS/world
logic into smolvm, and do not reimplement VMM or virtio behavior here.

The companion smolvm contract is an external virtio-net attachment with a Unix
stream path and a complete static IPv4 tuple: guest address, gateway, DNS, and
MAC. It is incompatible with smolvm's built-in gateway/TSI, port mappings,
egress policy, DNS filtering, IPv6, and a second NIC. libkrun needs no source
patch while it provides `krun_add_net_unixstream`.

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
  implements the authoritative service. It answers configured short names and
  `<machine>.<domain>` names only.
* The gateway address and MAC are reserved: allocation must never give either
  to a machine.
* The host/virtio wire protocol is one big-endian 4-byte Ethernet-frame length
  followed by the raw frame. Accepted streams must be switched to blocking
  mode before frame reads; an idle healthy NIC is not a disconnect.
* Unknown/broadcast/multicast destination MACs flood to other attached ports;
  known unicast targets the learned port. Detach must remove the port and its
  FDB entries.
* `up` owns only deterministic `smw-...` machine names recorded in its world
  state. `down` and signal cleanup must never affect unrelated smolvm machines.
* Images are local paths. Validate all configuration and inspectable runtime
  prerequisites before state, listeners, or machines are created.
* Default machine resources are intentionally small: 1 vCPU, 256 MiB RAM, and
  1 GiB sparse storage/overlay. Per-machine overrides remain positive; memory
  must be at least 64 MiB.

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

The real local integration test is opt-in because it creates VMs and needs
Apple Hypervisor Framework plus prepared artifacts:

```bash
SMOLWORLD_E2E=1 bash tests/e2e-redis.sh
```

It proves generic static DNS, Redis TCP through real virtio NICs, and exact
machine/runtime cleanup. Keep it generic: Redis is a workload fixture, never
runtime behavior.

The companion smolvm patch has focused external-network tests. Run them with
the local libkrun build when changing that boundary:

```bash
LIBKRUN_DIR="$HOME/d/libkrun/target/release" \
DYLD_LIBRARY_PATH="$HOME/d/libkrun/target/release" \
cargo test -p smolvm external --lib
```

Do not run pre-commit hooks or push a remote. Do not add a dependency, expand
the networking/product scope, or make destructive cleanup broader than the
recorded world without user approval.
