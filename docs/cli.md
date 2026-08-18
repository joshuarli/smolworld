# smolworld CLI

This page is a practical command reference for the `smolworld` binary. The
[world contract](world-contract.md) is authoritative for configuration,
command literals, lifecycle states, output schemas, checkpoint semantics, and
cleanup guarantees. If this page and the contract ever disagree, follow the
contract.

## Invocation

Run commands from the world directory, or select an authored world explicitly:

```text
smolworld <command>
smolworld -f PATH <command>
smolworld <command> -f PATH
```

The default authored file is `.smolworld`. `-f` and `--file` select another
path and are accepted before or after the command (in the position supported
by that command). `--help` prints a complete, generated command reference;
`<command> --help` prints the detailed page for one command. `--version`
prints the package version together with the embedded Git short SHA.

The normal first-run sequence is:

```text
smolworld prepare
smolworld check
smolworld up
```

`prepare` creates the material lock; `check` validates that prepared material
and the runtime prerequisites still match it; `up` starts the foreground
supervisor. Preparation and checking do not allocate machines, create runtime
sockets, or bind listeners.

## Commands

### `prepare`

```text
smolworld prepare [-f PATH]
```

Resolve and seal the host inputs referenced by the world, validate every
Smolfile and local image archive, and write `.smolworld.lock`. This is the only
preparation mutation. If an authored input changes, run `prepare` again before
`check` or `up`.

### `check`

```text
smolworld check [-f PATH]
```

Perform the read-only preflight for a prepared world. It compares authored and
external inputs with the material lock and validates the configured runtime
artifacts and external NIC prerequisites. It does not create or repair runtime
state.

### `up`

```text
smolworld up [-f PATH]
```

Start the foreground supervisor for the prepared world. Machines are created
and started in dependency waves, then the supervisor owns the switch, gateway,
machine sockets, and cleanup. `up` refuses unprepared or changed material.

The process remains in the foreground. Press `Ctrl-C` to stop the exact world
recorded by this configuration. If the process is interrupted, `down` is the
explicit recovery path.

`depends_on` determines creation/start order only; `up` does not wait for a
guest service to become ready or healthy.

### `ps`

```text
smolworld ps [-f PATH]
smolworld ps [-f PATH] --json
```

Show one row for each configured machine. The default table has `MACHINE`,
`IP`, `MAC`, and `STATUS` columns. `--json` emits the same rows as a JSON
array. Status values are host lifecycle observations—not guest service
health or readiness—and are the closed set documented in the [world
contract](world-contract.md): `created`, `attached`, `running`, `capturing`,
`captured`, and `absent`.

### `metrics`

```text
smolworld metrics [-f PATH] --json
```

Read host-side metrics for the configured machines. `--json` is required so
callers opt into the stable machine-readable presentation. The command reads
only exact allocations recorded for this world; it never discovers unrelated
smolvm machines. The closed JSON envelope, row fields, nullability, and
measurement meanings are defined in the [world contract](world-contract.md).

### `exec`

```text
smolworld exec [-f PATH] MACHINE [--secret-env GUEST=HOST_ENV]... -- COMMAND [ARG ...]
```

Delegate one command to a named, running world machine. The `--` separator is
required. Repeat `--secret-env` to pass selected caller-owned host environment
variables into that command. Secret values are resolved for this invocation
only and are not written to world state, the Smolfile, or the material lock.

For example:

```text
smolworld exec runner --secret-env API_KEY=API_KEY -- \
  /usr/local/bin/run-task --once
```

### `cp`

```text
smolworld cp [-f PATH] SRC DST
```

Copy one regular file between the host and one recorded machine. Exactly one
operand must be a guest endpoint of the form
`MACHINE:/absolute/path`; the other operand is a host path:

```text
smolworld cp ./input.txt runner:/workspace/input.txt
smolworld cp runner:/workspace/result.txt ./result.txt
```

Guest endpoints must name a configured machine and a traversal-free absolute
path. `cp` is a namespaced agent operation, not a host mount or general
filesystem sharing mechanism.

### `checkpoint`

```text
smolworld checkpoint [-f PATH] --output DIR
```

Ask the running foreground supervisor to capture every machine and the switch
as one coherent world checkpoint. `DIR` must be an absolute, unused directory.
The supervisor closes the switch at a new epoch, pauses machines concurrently,
seals one receipt, publishes it atomically, and then exits while retaining the
exact checkpoint sources. A successful command prints the published path.

Checkpointing is coordinated through the running supervisor; starting a second
process does not create a parallel runtime. If capture fails, follow the
receipt and lifecycle state reported by the command before attempting recovery.

### `restore`

```text
smolworld restore [-f PATH] --checkpoint DIR
```

Restore a retained checkpoint for the same world. `DIR` must be an absolute
checkpoint directory whose receipt matches the selected configuration,
material lock, allocation, topology, and machine set. Restore creates fresh
agent and Unix-stream NIC handles; it does not accept an unrelated or
cross-lineage checkpoint.

### `release`

```text
smolworld release [-f PATH] --checkpoint DIR
```

Delete one retained checkpoint and exactly the recorded source machines it
owns. `DIR` must be an absolute, stopped retained checkpoint. The receipt is
verified before deletion. Use `release` to finish a checkpoint lifecycle; do
not use broad process or name scans.

### `down`

```text
smolworld down [-f PATH]
```

Stop and delete the exact machines recorded for this world, then remove its
runtime sockets. It is safe after an interrupted foreground `up`, but it does
not release a retained checkpoint. When a checkpoint retains source machines,
use `release --checkpoint DIR` instead.

## A complete durable-world sequence

The following is the intended shape for a durable capture and restore:

```text
smolworld prepare
smolworld check
smolworld up
smolworld checkpoint --output /absolute/path/checkpoint
smolworld restore --checkpoint /absolute/path/checkpoint
# press Ctrl-C after using the restored world
smolworld release --checkpoint /absolute/path/checkpoint
```

The supervisor exits after a successful checkpoint, so `restore` is a new
foreground invocation. A restored supervisor also retains the checkpoint
sources when it stops, so `release`—rather than `down`—removes the durable
artifact and its exact source machines.
