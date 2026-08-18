I compared the current CLI with the current Docker Compose surface. Docker Compose has substantially more than lifecycle commands: its root options include file merging, project naming, profiles, progress controls, dry-run, parallelism, and environment-file handling; its command set includes `attach`, `build`, `config`, `create`, `events`, `logs`, `run`, `start`, `stop`, `restart`, `stats`, `top`, `wait`, and more. [Compose CLI reference](https://docs.docker.com/reference/cli/docker/compose/)

This is an implementation inventory and planning note, not a second
user-facing contract. The normative behavior remains in
[`docs/world-contract.md`](docs/world-contract.md).

## Immediate renames

| Current | Recommended | Reason |
|---|---|---|
| `metrics` | `stats` | `docker compose stats` is the native resource-observation command. It live-streams by default and supports `--no-stream`, `--all`, and `--format`. [stats reference](https://docs.docker.com/reference/cli/docker/compose/stats/) |
| `MACHINE` | `SERVICE` in CLI syntax | Compose users address services, not containers. We can retain “machine” internally and in the world contract. |
| `--json` | `--format json` | Compose uses `--format table/json/template` for `ps` and `stats`. `--json` could remain as a compatibility alias. [ps reference](https://docs.docker.com/reference/cli/docker/compose/ps/) |
| `smolworld cp SRC DST` | Compose-style endpoint placeholders | Use `SERVICE:SRC_PATH DEST_PATH` and `SRC_PATH SERVICE:DEST_PATH`, even though our current endpoint shape is already close. |

`prepare`, `check`, `checkpoint`, `restore`, and `release` should remain smolworld-specific commands. They do not have faithful Compose equivalents.

## Delegation inventory: reuse smolvm commands

The selected smolvm checkout already owns most single-machine lifecycle and
agent operations. A Compose-shaped smolworld command should be a thin adapter
where possible: validate the logical service against this world's declaration,
resolve its exact recorded `smw-*` identity, forward the supported options, and
translate the result. It must not rediscover machines with an unrestricted
`smolvm machine ls` call; that would cross the world's identity and cleanup
boundary.

The existing adapter in [`src/smolvm.rs`](src/smolvm.rs) already demonstrates
this pattern for `machine create`, `start`, `status`, `stats`, `exec`, `cp`,
`checkpoint`, `restore`, and exact `delete`/`stop` cleanup. The companion CLI
also provides `machine run`, `machine update`, `machine images`, `machine
monitor`, `machine shell`, `machine fork`, and `machine delete`/`rm`.

### Thin forwarding layers

These should reuse the upstream operation rather than reimplement VM, agent,
file-transfer, or resource-sampling behavior:

| Compose-facing command | Existing upstream call | Smolworld-owned work |
| --- | --- | --- |
| `stats` | `smolvm machine stats --name NAME --format tsv` | Select recorded machines, implement the Compose stream/format presentation, and retain identity checks. The measurement itself already comes from smolvm. |
| `ps` | `smolvm machine status --name NAME` (or `--json`) | Query only declared identities and format Compose-shaped rows. Do not replace this with an unrestricted `machine ls`. |
| `exec` | `smolvm machine exec --name NAME ...` | Forward native `-e`, `-w`, `-i`, `-t`, `--stream`, `-d`, `--timeout`, `--secret-env`, and `--secret-file` options; smolvm already owns guest execution and exit status. |
| `cp` | `smolvm machine cp SRC DST` | Keep world endpoint validation and identity resolution; reuse smolvm's streaming transfer and progress behavior. Directory/stdin/stdout support is not present upstream, so do not recreate it in smolworld without an upstream capability. |
| `version` | `smolvm --version` | Add a small presentation wrapper if a Compose-style version command is desired. |
| `shell` (optional convenience) | `smolvm machine shell --name NAME` | Forward to smolvm's interactive `exec -it /bin/sh` behavior. This is closer to a shell convenience command than Compose `attach`. |

`prepare` and `check` are also mostly existing upstream adapters: they call
`smolvm smolfile materialize-external` and `smolvm smolfile validate-external`
for each declaration. The world-level lock, dependency validation, and
all-or-nothing material transaction remain smolworld responsibilities.

### Upstream primitive exists, but world coordination is required

These are still thin at the VM boundary, but cannot safely become a direct
subprocess passthrough:

| Proposed command | Existing primitive | Why coordination remains |
| --- | --- | --- |
| `start` | `smolvm machine start --name NAME` | The world must keep or recreate the external NIC listener, switch port, gateway, and lifecycle record consistently. |
| `stop` | `smolvm machine stop --name NAME` | The world must detach the port and update state without deleting the recorded allocation or affecting another world. |
| `restart` | `stop` followed by `start` | smolvm has no `machine restart`; ordering, NIC reconnect, and failure rollback belong to the world supervisor. |
| `create` | `smolvm machine create --name NAME ...` | smolworld already supplies the exact name, Smolfile, static network tuple, Unix-stream socket, material lock, dependency wave, and allocation record. |
| `rm` / selected `down` | `smolvm machine delete --name NAME -f` | Reuse the upstream stop-then-delete implementation. Keep exact recorded identities, state transitions, and checkpoint guards in smolworld; do not hand-roll stop plus delete. |
| `checkpoint` | `smolvm machine checkpoint --name NAME --output DIR` | The primitive is already used, but a world checkpoint must quiesce the switch and capture every machine into one receipt/transaction. |
| `restore` | `smolvm machine restore --name NAME --checkpoint DIR` | The primitive is already used, but smolworld verifies same-lineage configuration/material/allocation identity and recreates the multi-machine switch. |
| `images` | `smolvm machine images --name NAME` | The upstream command owns image/storage inspection, but it may start a stopped machine to query the agent; that conflicts with a strictly read-only world observation contract and needs an explicit policy. |
| `monitor` / health behavior | `smolvm machine monitor --name NAME` | The upstream monitor owns health checks/restarts, but health and restart policy are currently explicit smolworld non-goals. |

The current `down` cleanup already follows the important upstream rule:
`machine delete -f` owns stop-then-remove. Calling `machine stop` separately
and ignoring its result can race deletion and leave an orphaned VM process.

### No direct CLI primitive in smolvm

These cannot currently be thin wrappers over `smolvm machine ...`:

```text
logs       # smolvm's machine CLI has no logs command
events     # no machine event-stream CLI
attach     # shell/exec is not primary workload-stream attachment
top        # no structured machine top command; `exec ps` is only an approximation
wait       # no machine wait command or stable exit-event CLI
kill       # no machine kill command; stop is graceful
pause      # no machine pause command
unpause    # no machine unpause command
```

The smolvm HTTP API has a log-stream endpoint, but using it would be a new
transport boundary rather than a thin wrapper over the selected CLI. If these
commands become requirements, the cleanest path is to add a versioned upstream
CLI/API operation first and keep smolworld as the identity/format adapter.

### Existing upstream commands that do not map directly

`smolvm machine run` supports one-shot and detached ephemeral machines, but it
does not represent a declared world service. It bypasses the world's durable
allocation, dependency waves, and coordinated switch lifecycle; use it only if
we deliberately define a separate `run` contract. Similarly, smolvm's
`machine fork` is a powerful CoW clone primitive, but it is not Compose
replica/scale semantics without world-level allocation, naming, networking,
and cleanup rules.

The companion also has image/artifact commands (`pack pull`, `pack push`,
`pack create`, `pack prune`) and machine resource commands (`machine update`,
`machine images`, `machine prune`). They should not be exposed as ad-hoc
Compose aliases that bypass the `.smolworld.lock`: material preparation and
world identity are the controlling boundaries. `build`, `pull`, `push`,
`publish`, `commit`, and `export` need an explicit artifact contract before
they can be added safely.

Finally, upstream `machine create`/`update` support volumes and host port
publishing, but the current smolworld contract intentionally rejects those
capabilities for external-world machines. `port` and `volumes` therefore are
not thin wrappers until that contract changes.

## Existing commands with major Compose deviations

### `up`

Current `up` is much narrower than Compose `up`.

Missing:

- `[SERVICE...]` selection
- `-d, --detach`
- attached service output/log aggregation
- `--wait`, `--wait-timeout`
- `--build`, `--no-build`, `--pull`
- `--no-deps`
- `--no-recreate`, `--force-recreate`
- `--no-start`
- `--abort-on-container-exit`
- `--abort-on-container-failure`
- `--exit-code-from`
- `--attach`, `--no-attach`, `--attach-dependencies`
- `--timestamps`, `--no-color`, `--no-log-prefix`
- `--remove-orphans`
- `--scale`
- `--watch`
- `-t, --timeout`
- `-y, --yes`

Compose `up` defaults to attached output, while `-d` leaves services running in the background. [up reference](https://docs.docker.com/reference/cli/docker/compose/up/)

The biggest semantic gap is that our `up` requires an explicit `prepare`/`check` cycle and has no service-output or health/readiness model.

### `down`

Current `down` always operates on the whole recorded world.

Missing:

- `[SERVICE...]`
- `-t, --timeout`
- `--remove-orphans`
- `-v, --volumes`
- `--rmi`

More importantly, we have no Compose-like `stop`. Our `down` is effectively “stop and delete.” Compose distinguishes `stop` from `down`; `down` removes the project resources, while `stop` leaves them available for `start`. [down reference](https://docs.docker.com/reference/cli/docker/compose/down/)

### `ps`

Current output is:

```text
MACHINE IP MAC STATUS
```

Compose exposes service/container identity, image, command, creation time, status, ports, health, and exit information.

Missing:

- `[SERVICE...]`
- `-a, --all`
- `--status`
- `--filter`
- `--format table|json|template`
- `-q, --quiet`
- `--services`
- `--orphans`
- `--no-trunc`

Our JSON is one array; Compose’s `--format json` is JSON Lines. [ps reference](https://docs.docker.com/reference/cli/docker/compose/ps/)

### `stats`

This should replace `metrics`, not merely alias it.

Current behavior is one required `--json` snapshot with a world envelope. Compose behavior is:

```text
docker compose stats [OPTIONS] [SERVICE]
```

with:

- live streaming by default
- `--no-stream`
- `-a, --all`
- `--format table|json|template`
- service selection
- template output
- non-truncated output controls

[stats reference](https://docs.docker.com/reference/cli/docker/compose/stats/)

We can preserve a stable smolworld-specific JSON schema under `--format json`, but it should probably emit one row per update rather than the current closed top-level `{"schemaVersion":...}` envelope.

### `exec`

Current syntax requires:

```text
smolworld exec SERVICE [--secret-env ...] -- COMMAND [ARG...]
```

Compose syntax is:

```text
docker compose exec [OPTIONS] SERVICE COMMAND [ARGS...]
```

Missing:

- native command syntax without mandatory `--`
- default interactive/TTY behavior
- `-d, --detach`
- `-e, --env`
- `-T, --no-tty`
- `-u, --user`
- `-w, --workdir`
- `--privileged`
- `--index`
- explicit interactive/stdin controls

`--secret-env` is useful as an additive feature, but it should not replace native `-e/--env` semantics. [exec reference](https://docs.docker.com/reference/cli/docker/compose/exec/)

### `cp`

Current `cp` only supports one regular file and one `MACHINE:/absolute/path` endpoint.

Missing:

- directory copies
- `-` for stdin/stdout
- `--all`
- `-a, --archive`
- `-L, --follow-link`
- `--index`
- replicas/service selection
- native endpoint naming (`SERVICE:SRC_PATH`)

Compose’s syntax is already structurally similar, so this is a relatively easy compatibility win. [cp reference](https://docs.docker.com/reference/cli/docker/compose/cp/)

## Entire Compose commands currently missing

### Core lifecycle and interaction

These are the most valuable additions:

```text
attach
create
events
kill
logs
pause
restart
rm
start
stop
top
unpause
wait
```

In particular:

- `start` / `stop` are needed to distinguish lifecycle suspension from deletion.
- `restart` is a natural machine-level operation.
- `logs` is required for a Compose-like attached experience.
- `attach` is different from `exec`: it connects to the primary workload streams.
- `events` gives scripts a lifecycle/event interface.
- `top` exposes guest process listings.
- `wait` provides a blocking lifecycle primitive.

Compose documents `attach` as stream attachment to a running service. [attach reference](https://docs.docker.com/reference/cli/docker/compose/attach/)

### Configuration and inspection

```text
config
convert       # Compose alias for config
ls
version
```

`config` is especially important. It should render the resolved world configuration, support `--format yaml|json`, and offer `--quiet` validation. Our `check` is not a substitute: it validates sealed runtime material rather than rendering the effective configuration. [config reference](https://docs.docker.com/reference/cli/docker/compose/config/)

`version` is a cheap compatibility improvement and should support `--short` or `--format`.

### Image/material operations

```text
build
pull
push
publish
images
export
commit
```

Some of these should probably remain intentionally unsupported:

- `build`, `pull`, `push`, and `publish` conflict with the current smolvm-owned image/material boundary.
- `commit` and `export` assume mutable container filesystems and image lifecycle that smolworld does not own.
- `images` could still be useful as a read-only material summary.

`prepare` currently covers part of what Compose users expect from `pull`/`build`, but it is explicit, host-sealing work rather than an `up` option.

### One-off and scaling workflows

```text
run
scale
watch
```

These are major semantic decisions:

- `run` requires an ephemeral machine/clone model.
- `scale` conflicts with one statically allocated machine per world declaration.
- `watch` requires host source watching and rebuild/restart behavior.

Compose’s `run` creates a one-off service execution with options such as `--rm`, `--detach`, `--env`, `--user`, `--workdir`, and `--no-deps`. [run reference](https://docs.docker.com/reference/cli/docker/compose/run/)

### Network and storage discovery

```text
port
volumes
```

These are currently incompatible with explicit non-goals:

- `port` requires host port publishing.
- `volumes` requires a volume model.

They should not be added as empty compatibility shells.

## Global CLI gaps

Current global support is essentially only `-f/--file` and help.

Compose also supports:

```text
--all-resources
--ansi
--compatibility
--dry-run
--env-file
--parallel
--profile
--progress
--project-directory
-p, --project-name
```

Compose also:

- searches parent directories for a default compose file;
- accepts multiple `-f` files and merges them;
- supports `COMPOSE_FILE`, `COMPOSE_PROJECT_NAME`, `COMPOSE_PROFILES`, and `COMPOSE_PARALLEL_LIMIT`;
- supports stdin and remote/OCI/Git configuration sources.

Our CLI currently accepts one local `.smolworld` file and has no per-command help. [Compose global options](https://docs.docker.com/reference/cli/docker/compose/)

## Non-CLI differences that affect “native” UX

Even with command renames, these remain visible deviations:

- `machines` rather than Compose `services`
- strict `.smolworld` schema rather than `compose.yaml` with `services:`
- no environment interpolation or `--env-file`
- no profiles
- no replicas/indexes/scaling
- no service healthchecks or readiness conditions
- `depends_on` controls start order only
- no ports, volumes, host networking, logs, or service-level secrets/configs
- static one-machine-per-declaration allocation
- mandatory material sealing before startup
- no project-wide multi-world `ls`

## Recommended compatibility order

1. Rename/remove `metrics`; implement `stats`.
2. Add `stop`, `start`, and `restart`.
3. Add `up -d`, service selection, `logs`, and attached-output behavior.
4. Make `ps` Compose-shaped: service arguments, `--format`, `--all`, `--status`, `--quiet`.
5. Make `exec` and `cp` syntax/options Compose-shaped.
6. Add `config`, `version`, `wait`, `events`, `top`, `attach`, `create`, and `rm`.
7. Decide explicitly whether image operations, `run`, `scale`, `port`, `volumes`, and `watch` are supported or permanent non-goals.

The important distinction is that `stats`, `start/stop/restart`, `logs`, `ps`, `exec`, and `cp` can become Compose-like without changing the world model. `run`, `scale`, `port`, `volumes`, and image publishing require contract-level architectural decisions.
