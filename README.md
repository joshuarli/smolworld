# smolworld

Run a small group of smolvm machines on one private, static IPv4 network.
`smolworld` starts the world in the foreground, gives each machine a stable
address, and provides local DNS for the configured machine names.

## Requirements

The supported local build runs on macOS Apple Silicon and needs Rust/Cargo,
Xcode command-line tools, `codesign`, `make`, `nm`, and `mkfs.ext4`. Install the
last command with e2fsprogs, for example:

```bash
brew install e2fsprogs
export PATH="/opt/homebrew/opt/e2fsprogs/sbin:/opt/homebrew/opt/e2fsprogs/bin:$PATH"
```

The installer expects a patched smolvm source checkout. By default it looks for
`../smolvm` beside this checkout, uses that checkout's `lib/` directory as the
runtime bundle, and—when rebuilding libkrun—uses its integrated
`libkrun/` source directory. The bundle must contain the patched pair
`libkrun.dylib` and `libkrunfw.5.dylib`; the installer rejects Git LFS pointer
files and checks that `libkrun` exports `krun_add_net_unixstream`.

`SMOLWORLD_LIBKRUN_DIR` defaults to
`$SMOLVM_SOURCE_DIR/libkrun`. A sibling `~/d/libkrun` checkout is not an
implicit input: it may be rebased or used for independent libkrun work, but a
smolworld install uses the libkrun source integrated with the selected smolvm
checkout unless that variable is explicitly overridden.

`libkrunfw` is a guest-kernel artifact, and this repository does not contain a
complete macOS build procedure for its external kernel tree. The installer
therefore requires a prepared, matching `libkrunfw.5.dylib` instead of
pretending to build one. Set `SMOLVM_LIB_DIR` when the pair lives elsewhere.
Set `SMOLWORLD_BUILD_LIBKRUN=1` only when the patched libkrun checkout has its
documented `make smolvm` target; this rebuilds libkrun against the existing
kernel artifact but does not build libkrunfw.

## Reproducible local install

Run one command from this checkout. Supplying a world file also reuses the
normal `smolworld check` diagnostics before anything is installed:

```bash
SMOLWORLD_CHECK_CONFIG=/path/to/.smolworld ./scripts/install-local.sh
```

Without a world file, omit `SMOLWORLD_CHECK_CONFIG`; the installer still checks
the runtime artifacts and runs the built smolvm binary's version command.

The script builds and ad-hoc-signs release smolvm with
`smolvm.entitlements`, builds release smolworld, and stages both together with
the library pair and agent rootfs. If the selected rootfs does not contain
`usr/local/bin/smolvm-agent`, the script invokes smolvm's existing
`scripts/build-agent-rootfs.sh` into a temporary directory. That step needs
Docker or an already usable smolvm bootstrap and downloads the pinned inputs
used by that script. Set `SMOLWORLD_BUILD_AGENT_ROOTFS=0` to require a prepared
rootfs and fail instead.

The default install is the dedicated directory `~/.local/smolworld`; it does
not use `sudo` or overwrite a non-installer directory. A completed install is
replaced only when that directory carries the install marker. The build,
staging, and optional `check` happen before the replacement is committed, so a
failure leaves the previous install unchanged.

Configuration overrides:

```text
SMOLVM_SOURCE_DIR             patched smolvm checkout (default: ../smolvm)
SMOLVM_LIB_DIR                libkrun/libkrunfw bundle (default: $SMOLVM_SOURCE_DIR/lib)
SMOLVM_AGENT_ROOTFS           prepared agent rootfs
SMOLWORLD_BUILD_AGENT_ROOTFS  build a missing rootfs (default: 1)
SMOLWORLD_BUILD_LIBKRUN       rebuild libkrun with make smolvm (default: 0)
SMOLWORLD_LIBKRUN_DIR         integrated smolvm libkrun source (default:
                              $SMOLVM_SOURCE_DIR/libkrun)
SMOLWORLD_LIBKRUN_BUILD_FLAGS make flags (default: BLK=1 NET=1 GPU=1)
CODESIGN_IDENTITY             codesign identity (default: - for ad-hoc signing)
SMOLWORLD_INSTALL_PREFIX      install directory (default: ~/.local/smolworld)
```

Add the installed wrapper to `PATH`:

```bash
export PATH="$HOME/.local/smolworld/bin:$PATH"
smolworld -f /path/to/.smolworld check
```

The wrapper supplies `SMOLWORLD_SMOLVM`, `SMOLVM_AGENT_ROOTFS`,
`SMOLVM_LIB_DIR`, and the macOS dynamic-library path from the installed bundle.
Explicitly set those variables to override the bundle for development.

## Commands

```text
smolworld check [-f PATH]                 Validate the world and local runtime.
smolworld up [-f PATH]                    Start the world; stays in the foreground.
smolworld ps [-f PATH] [--json]           Show configured machines and lifecycle status.
smolworld exec [-f PATH] MACHINE -- CMD   Run CMD in a started machine.
smolworld down [-f PATH]                  Stop and delete this world's machines.
```

`-f` and `--file` select a configuration file; the default is `.smolworld` in
the current directory. Press `Ctrl-C` in `up` to stop and delete this world's
machines. Use `down` if a previous foreground process was interrupted. Each
world has an operating-system lock, so a second `up` fails without touching the
first process. A later `up` recovers the recorded deterministic machines and
stale sockets after an uncatchable interruption.

`ps` reports host lifecycle observations only: `created` means a smolvm machine
record exists but is not running, `attached` means it is running and its world
NIC attachment milestone completed, `running` means the foreground world
supervisor owns it, and `absent` means the recorded machine is not present.
These labels are not health or readiness checks. `ps --json` emits the same
rows as a JSON array.

## `.smolworld` file

```yaml
world:
  name: demo

network:
  subnet: 10.89.0.0/24
  gateway: 10.89.0.1 # optional; defaults to .1
  dns: 10.89.0.1     # optional; must equal gateway
  domain: demo.test  # optional; defaults to the world name

machines:
  api:
    image: ./api.tar
    command: [serve]
    cpus: 1
    memory_mib: 256
    storage_gib: 1
    overlay_gib: 1
  client:
    image: ./toolbox.tar
    command: [sleep, infinity]
    depends_on: [api]
```

The `.smolworld` file is one YAML document with `world`, `network`, and
`machines` mappings. At least one machine is required. Unknown keys and
non-string names/paths are rejected. Machine names, the world name, and domain
labels must be lowercase DNS labels.

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
