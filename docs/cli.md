# smolworld CLI

The [world contract](world-contract.md) is authoritative for every command,
lifecycle label, schema, and cleanup guarantee. This page is the operational
reference for the implemented Compose-shaped surface; it does not make
smolworld a Docker Compose configuration parser or runtime.

## Invocation

```text
smolworld <command>
smolworld -f PATH <command>
smolworld <command> -f PATH
```

The default authored file is `.smolworld`. `-f`/`--file` selects one local
world file. `--help` prints generated command help and `<command> --help`
prints its detailed page. Root `--version` prints the package and Git version;
`smolworld version --short` and `smolworld version --format json` are the
Compose-shaped forms.

The normal first-run sequence remains explicit:

```text
smolworld prepare
smolworld check
smolworld up -d
```

`prepare` seals material, `check` verifies it read-only, and `up -d` launches
the world supervisor in the background. The background supervisor owns the
private L2 switch and is the only process allowed to stop, restart, remove, or
delete its recorded machines.

## Configuration and inspection

### `config` / `convert`

```text
smolworld config [-f PATH] [--format yaml|json] [--quiet]
```

Validate the strict `.smolworld` declaration and render its resolved defaults.
`--quiet` validates without output. `check` is different: it validates sealed
material and host prerequisites rather than rendering configuration. `convert`
is the Compose-shaped alias for this same renderer; it does not introduce a
second configuration language.

### `ps`

```text
smolworld ps [-f PATH] [--all] [--status STATE|--filter status=STATE] \
  [--format table|json|TEMPLATE] [--quiet|--services] [SERVICE...]
```

Show only exact allocations from this world. The default table is `SERVICE`,
`IP`, `MAC`, and `STATUS`. `--all` includes stopped and absent declarations;
an explicit service argument also displays that service. `--format json` emits
JSON Lines with `service`, `ip`, `mac`, and `status`; `--json` is retained as
an alias. Templates support `{{.Service}}`, `{{.IP}}`, `{{.MAC}}`, and
`{{.Status}}`.

The status labels are host lifecycle observations only: `created`, `attached`,
`running`, `stopped`, `capturing`, `captured`, and `absent`. They do not imply
health, readiness, a service command, or guest process exit state.

### `stats`

```text
smolworld stats [-f PATH] [--all] [--no-stream] \
  [--format table|json|TEMPLATE] [SERVICE...]
```

Observe exact recorded services. The default streams table snapshots every
second; `--no-stream` prints one. `--format json` and `--json` use the closed
world JSON envelope defined in the contract. Templates support `{{.Service}}`,
`{{.Status}}`, `{{.CPUSeconds}}`, `{{.RSSMb}}`, and `{{.DiskUsedMb}}`.

The command delegates sampling to `smolvm machine stats --format tsv` and
keeps the literal `machine-stats-v1` ABI and closed `schemaVersion: 1` envelope
intact. It never calls `smolvm machine ls` or otherwise discovers another
world's machines.

### `images`

```text
smolworld images [-f PATH] [--format table|json] [SERVICE...]
```

Show the source and digest records already sealed in `.smolworld.lock`. This
is a read-only material summary: it deliberately does not call `smolvm machine
images`, whose implementation may start a stopped machine. JSON is one row per
line with service and sealed source/image identity fields.

### `version`

```text
smolworld version [--short|--format json]
```

Print the smolworld package version and embedded Git revision. It does not
claim or infer the version of a separately selected smolvm checkout.

## Lifecycle

### `up`

```text
smolworld up [-f PATH] [-d|--detach] [SERVICE...]
```

Create and start the selected services and their `depends_on` closure. Without
`-d`, the supervisor remains in the foreground and cleans the exact world on
`Ctrl-C`. With `-d`, it is launched in the background with ordinary output
suppressed; smolworld has no workload log aggregation. No form waits for a
guest readiness/health signal.

### `create`, `start`, `stop`, `restart`, and `rm`

```text
smolworld create [-f PATH] [SERVICE...]
smolworld start [-f PATH] [SERVICE...]
smolworld stop [-f PATH] [SERVICE...]
smolworld restart [-f PATH] [SERVICE...]
smolworld rm [-f PATH] SERVICE...
```

`create` writes exact machine configurations without starting them. `start`
from that created state starts a background supervisor around the same
identities. Once supervised, `start`, `stop`, and `restart` are delivered over
the owner's private control socket so the switch listeners remain consistent.
`stop` retains the VM record and disk; `rm` requires a stopped service and
deletes only its exact recorded machine configuration. A later `start` can
recreate a removed declaration through sealed material.

These are service selections, not replicas: there is exactly one static
machine allocation per declaration. Dependencies control creation/start order
for `up`; they do not add readiness or restart policy semantics.

### `down`

```text
smolworld down [-f PATH]
```

Stop and delete the full exact recorded world and its runtime sockets. Against
a live supervisor, `down` requests that owner to exit and clean up; it never
competes for the world lock. A retained checkpoint remains protected and must
be removed with `release`. When exact companion deletion fails, `down`
returns that failure and preserves the recorded state for a later exact retry
or reconciliation.

## Interaction

### `exec`

```text
smolworld exec [-f PATH] [OPTIONS] SERVICE COMMAND [ARG...]
```

`--` before `COMMAND` is accepted but no longer required. Before `SERVICE`,
the command forwards the companion-supported flags `-e`/`--env`,
`-w`/`--workdir`, `-i`/`--interactive`, `-t`/`--tty`, `--stream`,
`-d`/`--detach`, `--timeout`, `--secret-env`, and `--secret-file`. For example:

```text
smolworld exec -e MODE=check runner /usr/local/bin/run-task --once
smolworld exec -it runner /bin/sh
smolworld exec --secret-env API_KEY=API_KEY runner /usr/local/bin/run-task
```

The service must already be running under the world supervisor. This avoids
the companion implicitly booting a VM without its private switch port.

### `shell`

```text
smolworld shell [-f PATH] SERVICE
```

Run an interactive TTY `exec` of `/bin/sh` in one running service.

### `cp`

```text
smolworld cp [-f PATH] SRC DST
```

Copy one regular file between a host path and exactly one
`SERVICE:/absolute/path` endpoint:

```text
smolworld cp ./input.txt runner:/workspace/input.txt
smolworld cp runner:/workspace/result.txt ./result.txt
```

The selected service must already be running under this world's live
supervisor. smolworld checks that ownership and running state before
delegation, so the companion cannot implicitly boot a VM without its private
switch port.

The selected companion does not support directory recursion, stdin/stdout,
archives, link-following, replica indexes, or copying between two services;
smolworld deliberately does not emulate those missing transport semantics.

## World-specific commands

`prepare`, `check`, `checkpoint`, `restore`, and `release` keep their existing
world-specific forms and semantics. In particular, checkpoint and restore are
whole-world transactions, and `release` is the only deletion path for retained
checkpoint sources.

## Intentionally unavailable Compose commands

`logs`, `events`, `attach`, `top`, `wait`, `pause`, `unpause`, `kill`, `run`,
`scale`, `watch`, `ls`, `port`, `volumes`, `build`, `pull`, `push`, `publish`,
`commit`, and `export` are unavailable. The selected upstream CLI has no safe
primitive for several of them, while the remainder would introduce replicas,
host ports, volumes, image lifecycle, or workload-stream contracts outside the
world model. They are not empty aliases.
