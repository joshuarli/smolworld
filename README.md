# smolworld

Run a small group of smolvm machines on one private, static IPv4 network.
`smolworld` starts the world in the foreground, gives each machine a stable
address, and provides local DNS for the configured machine names.

## Requirements

This local proof of concept runs on macOS on Apple Silicon and needs a smolvm
binary with external virtio-net support. `smolworld check` validates the local
requirements before it creates a machine.

For the adjacent source checkouts, configure the development artifacts:

```bash
export SMOLWORLD_SMOLVM="$HOME/d/smolvm/target/debug/smolvm"
export SMOLVM_AGENT_ROOTFS="$HOME/d/smolvm/target/agent-rootfs"
export SMOLVM_LIB_DIR="$HOME/d/libkrun/target/release"
export PATH="/opt/homebrew/opt/e2fsprogs/sbin:/opt/homebrew/opt/e2fsprogs/bin:$PATH"
```

If the smolvm binary is a raw debug build, sign it once:

```bash
cd "$HOME/d/smolvm"
codesign --force --sign - --entitlements smolvm.entitlements target/debug/smolvm
```

Build smolworld, then run the resulting binary from the directory that contains
the `.smolworld` file:

```bash
cargo build --release
cd /path/to/world
/path/to/smolworld/target/release/smolworld check
```

Or use `cargo run -- check` while developing from this checkout.

## Commands

```text
smolworld check [-f PATH]                 Validate the world and local runtime.
smolworld up [-f PATH]                    Start the world; stays in the foreground.
smolworld ps [-f PATH]                    Show configured machines and their status.
smolworld exec [-f PATH] MACHINE -- CMD   Run CMD in a started machine.
smolworld down [-f PATH]                  Stop and delete this world's machines.
```

`-f` and `--file` select a configuration file; the default is `.smolworld` in
the current directory. Press `Ctrl-C` in `up` to stop and delete this world's
machines. Use `down` if a previous foreground process was interrupted.

## `.smolworld` file

```toml
[world]
name = "demo"

[network]
subnet = "10.89.0.0/24"
gateway = "10.89.0.1" # optional; defaults to .1
dns = "10.89.0.1"     # optional; must equal gateway
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

`[world]`, `[network]`, and at least one `[machines.NAME]` table are required.
Machine names, the world name, and domain labels must be lowercase DNS labels.

| Field | Meaning |
| --- | --- |
| `world.name` | Name of the world. |
| `network.subnet` | Required IPv4 `/24` network address. |
| `network.gateway` | Gateway address inside the subnet; defaults to `.1`. |
| `network.dns` | DNS server; must equal `gateway`. |
| `network.domain` | Local DNS suffix; defaults to `world.name`. |
| `machines.NAME.image` | Required local Docker archive or unpacked rootfs path. Use an absolute path, `./...`, or `../...`. |
| `machines.NAME.command` | Optional command and arguments for the workload. |
| `machines.NAME.depends_on` | Optional machine names to start first. This is ordering only. |
| `machines.NAME.cpus`, `memory_mib`, `storage_gib`, `overlay_gib` | Optional machine resources. Defaults: 1, 256, 1, and 1. |

Guests can resolve another configured machine by its short name (for example
`api`) or fully qualified name (`api.demo.test`). The network is isolated:
images must already be local, and guests have no Internet access.

## Redis example

The included example uses Redis for both the server and client image. Prepare
the local archive once:

```bash
cd examples/redis
docker pull redis:8
docker save redis:8 -o redis.tar
```

In that directory, start the world:

```bash
cargo run --manifest-path ../../Cargo.toml -- up
```

In another terminal, verify name resolution and Redis:

```bash
cargo run --manifest-path ../../Cargo.toml -- exec client -- getent hosts redis
cargo run --manifest-path ../../Cargo.toml -- exec client -- redis-cli -h redis ping
# PONG
```

Press `Ctrl-C` in the first terminal when finished.
