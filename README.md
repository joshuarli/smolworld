# smolworld

`smolworld` is a local macOS proof of concept for a few smolvm machines on one
isolated Ethernet segment. It deliberately has no host networking, port
publishing, NAT, DHCP, IPv6, or guest Internet egress. It is a generic static
world runner; Redis below is only a concrete local example.

It needs the companion `smolvm` change in `~/d/smolvm`: `machine create` must
support `--net-unixstream`, `--net-address`, `--net-gateway`, `--net-dns`, and
`--net-mac`. Point `SMOLWORLD_SMOLVM` at that built binary when it is not on
`PATH`.

The default guest footprint is intentionally small: one vCPU, 256 MiB RAM,
and 1 GiB sparse storage plus overlay per machine. Each machine can override
those values in its world configuration.

## Local prerequisites

The source-build workflow needs a patched smolvm binary, a matching
`libkrun`/`libkrunfw` pair, an agent rootfs, and `mkfs.ext4` on `PATH`. For the
checkouts used here, set the explicit development paths before running a
world:

```bash
export SMOLWORLD_SMOLVM="$HOME/d/smolvm/target/debug/smolvm"
export SMOLVM_AGENT_ROOTFS="$HOME/d/smolvm/target/agent-rootfs"
export SMOLVM_LIB_DIR="$HOME/d/libkrun/target/release"
export PATH="/opt/homebrew/opt/e2fsprogs/sbin:/opt/homebrew/opt/e2fsprogs/bin:$PATH"
```

`SMOLVM_LIB_DIR` must contain both `libkrun.dylib` and `libkrunfw.5.dylib`.
If using smolvm's bundled libraries from a source checkout, hydrate their Git
LFS content first (`git lfs pull --include='lib/*.dylib'`).

A raw `target/debug/smolvm` build also needs the checked-in Hypervisor
Framework entitlement before it can create a VM:

```bash
cd "$HOME/d/smolvm"
codesign --force --sign - --entitlements smolvm.entitlements target/debug/smolvm
```

`smolworld check` verifies this and reports the command before `up` creates
any machines.

Run the non-mutating preflight from the directory holding `.smolworld`:

```bash
cargo run -- check
```

## Redis example

The isolated network cannot pull an OCI image from a registry during first
boot. Prepare a local Docker archive on the host, once:

```bash
cd examples/redis
docker pull redis:8
docker save redis:8 -o redis.tar
```

Run the world in one terminal:

```bash
cargo run -- up
```

Then, from another terminal in the same directory:

```bash
cargo run -- exec client -- redis-cli -h redis ping
# PONG
```

`Ctrl-C` in the `up` terminal stops and deletes only the deterministic
`smw-...` machines created for that world. `cargo run -- down` is idempotent
cleanup for an interrupted session. `cargo run -- ps` lists the stable static
addresses.

## Config

```toml
[world]
name = "demo"

[network]
subnet = "10.89.0.0/24"
gateway = "10.89.0.1" # optional; defaults to .1
dns = "10.89.0.1"     # optional; must match the synthetic DNS gateway
domain = "demo.test"  # optional; defaults to the world name

[machines.api]
image = "./api.tar"
command = ["serve"]
cpus = 1
memory_mib = 256
storage_gib = 1
overlay_gib = 1

[machines.client]
image = "./toolbox.tar"
command = ["sleep", "infinity"]
depends_on = ["api"]
```

Only this small schema is accepted. The subnet must be a `/24`; the gateway
and DNS server default to `.1`, and the DNS address must match the gateway
because smolworld implements that authoritative service itself. Images must be
absolute paths or `./`/`../` paths to a local Docker archive or unpacked
rootfs. The gateway answers ARP and authoritative DNS for configured machine
names, including `<machine>.<domain>`; all other traffic stays inside the L2
segment or is dropped. `depends_on` controls creation/start order only; it is
not a service-health check.

## Opt-in integration test

On an Apple-Silicon macOS host with the prerequisites above, the real-VM test
creates a temporary generic `cache` world, verifies DNS for
`cache.e2e.test`, verifies Redis `PONG` through the two virtio NICs, and checks
that signal cleanup removed only that test's machines and runtime directory:

```bash
SMOLWORLD_E2E=1 bash tests/e2e-redis.sh
```

Set `SMOLWORLD_REDIS_ARCHIVE` if the prepared Docker archive is not at
`examples/redis/redis.tar`. The test is opt-in because it needs Hypervisor
Framework and locally-built smolvm artifacts; a regular `cargo test` remains
VM-free.
