# smolworld contributor guide

The sole normative, user-facing world definition is
[`docs/world-contract.md`](docs/world-contract.md). Keep `format: 2`, the
closed `schemaVersion: 1` metrics envelope, `machine-stats-v1`, and all other
contract literals there. Do not create a second specification in this file or
in a feature document.

## Scope and architecture

`smolworld` is a local macOS/Apple-Silicon runner for a small, statically
provisioned group of smolvm machines. The world runtime and its boundaries are
defined in the canonical contract; this file tells contributors where changes
belong and how to verify them.

Keep world configuration, allocation, the userspace switch, gateway, lifecycle,
checkpoint coordination, and exact recorded-world cleanup in smolworld. Keep
individual VM/image/guest-agent lifecycle, the existing optional NAT relay, and
libkrun invocation in the selected upstream smolvm checkout. Keep VMM and
virtio behavior in libkrun. Do not move L2, DNS, or world logic into smolvm,
reimplement VMM/virtio behavior here, or edit the external smolvm contract.

## Module map

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | Binary entry point only. |
| `src/cli.rs` | CLI grammar, help text, and presentation types. |
| `src/config.rs` | Strict `.smolworld` parser, semantic validation, dependency ordering. |
| `src/model.rs` | Shared world, machine, network, state, checkpoint, and identity types. |
| `src/state.rs` + `src/state/` | Durable paths, material, lifecycle, allocation, and checkpoint codecs. |
| `src/companion_adapter.rs` | Typed operation-level errors for the selected upstream smolvm binary. |
| `src/smolvm.rs` | Preflight plus the narrow upstream smolvm CLI/TSV translation boundary. |
| `src/switch.rs` | Framed Unix-stream ports, MAC learning, Ethernet forwarding, epochs, cleanup. |
| `src/gateway.rs` | Synthetic gateway ARP and authoritative DNS A replies/forwarding. |
| `src/runtime.rs` + `src/runtime/` | Supervisor lifecycle plus material sealing and checkpoint transactions. |

Keep changes in their owning module. `model` contains cross-module domain
contracts; update its users and tests deliberately when changing them.

## Local source workflow

The supported build/runtime target is macOS on Apple Silicon
(`Darwin`/`aarch64`); Linux and Windows are unsupported. The local source
workflow is:

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

The local source build needs Rust/Cargo, Xcode command-line tools, `codesign`,
`make`, `nm`, and `mkfs.ext4`. Runtime artifacts are a patched and signed
smolvm binary, its signed `smolvm-boot` helper, matching
`libkrun.dylib`/`libkrunfw.5.dylib`, and a prepared agent rootfs containing
`usr/local/bin/smolvm-agent`. On macOS an ad-hoc `target/debug/smolvm` must
have the checked-in `smolvm.entitlements` applied or Hypervisor Framework
VM creation may fail with an opaque `EINVAL`.

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

The installer inputs are `SMOLVM_SOURCE_DIR` (default `../smolvm`),
`SMOLVM_LIB_DIR` (default `$SMOLVM_SOURCE_DIR/lib`), `SMOLVM_AGENT_ROOTFS`,
`SMOLWORLD_BUILD_AGENT_ROOTFS`, `SMOLWORLD_BUILD_LIBKRUN`,
`SMOLWORLD_LIBKRUN_DIR`, `SMOLWORLD_LIBKRUN_BUILD_FLAGS`, `CODESIGN_IDENTITY`,
and `SMOLWORLD_INSTALL_PREFIX` (default `~/.local/smolworld`). It may run
`smolworld check` when `SMOLWORLD_CHECK_CONFIG` or `--check PATH` is provided.
It does not use `sudo` or replace an unrelated install directory.

## Contract changes

Read [`docs/world-contract.md`](docs/world-contract.md) before changing any
world behavior. It is the only place for user-facing configuration, lifecycle,
network, metrics, checkpoint, cleanup, acceptance, and non-goal definitions.
When a contract changes, update that document, the owning domain types/schemas,
callers, and observable tests deliberately. Keep canonical bytes, receipts,
errors, cleanup identities, and upstream ABI labels deterministic. Preserve the
hard boundary that Smolfiles and the smolvm external command/ABI are upstream
inputs; do not add compatibility aliases or fallback parsers here.

## Working practices and safety

Before editing, inspect this guide, the canonical contract, the owning module,
callers, tests, schemas, and nearby documentation. Prefer the smallest
observable regression or acceptance test for behavior changes. Keep changes
coherent, dependencies few, and invalid states out of durable schemas and
interfaces. Do not add a third-party Rust dependency without explicit
approval.

Treat existing worktree changes as user-owned. Do not use broad or destructive
cleanup, name/process scans, or operations that can affect unrelated worlds;
cleanup must remain constrained by exact recorded identities. Do not edit the
companion `../smolvm` checkout or its bundled artifacts as part of smolworld
work. Do not run pre-commit hooks or push a remote.

Checkpoint, restore, release, metrics, and lifecycle semantics belong in the
[canonical world contract](docs/world-contract.md). Keep this file focused on
how contributors inspect, implement, and verify those contracts.

## Acceptance and measurement gates

The acceptance scenarios and measurement definitions are maintained in the
[canonical world contract](docs/world-contract.md). The opt-in live gates
below are contributor commands; they require prepared external artifacts and
create VMs. The Redis foundation gate must run without Docker, Compose,
OrbStack, `DOCKER_HOST`, or a Docker socket:

```bash
SMOLWORLD_E2E=1 \
SMOLWORLD_SMOLVM=/path/to/smolvm \
SMOLVM_AGENT_ROOTFS=/path/to/agent-rootfs \
SMOLVM_LIB_DIR=/path/to/smolvm/lib \
bash tests/e2e-redis-foundation.sh
```

Use `SMOLWORLD_E2E_EGRESS=1` with the same inputs for the explicit egress
variant. The fork and coordinated durable-world gates are:

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

## Verification

Start with the nearest hard judge: compiler, type checker, focused test, schema,
search, or runtime check. The normal local baseline is:

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
git diff --check
```

Run the narrowest useful check first and broaden it when warranted. Do not run
formatters, linters, pre-commit hooks, or remote pushes as part of a change
unless the user explicitly requests them. The live acceptance commands above
are opt-in and require prepared external artifacts; they do not replace cheap
config, state, receipt, and control-boundary tests.

The canonical contract owns the complete non-goal list and acceptance meaning.
Keep any contributor note here procedural and link back to that document
instead of restating user-facing semantics.
