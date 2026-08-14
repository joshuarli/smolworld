# Niceforge hard-switch plan

## Objective

Prove that smolworld can replace Docker Compose and OrbStack for Niceforge workloads, beginning with the archived Sentry backend integration fixture. The end state is a Niceforge executor backed by sealed, Smolfile-composed worlds: one isolated multi-machine world per job attempt, with a durable world transition after every workflow step.

This is a hard switch. New Niceforge dispatches must have no Compose YAML, Docker daemon, Docker socket, Docker resource ledger, or Docker executor path. Historical pre-cutover records remain read-only evidence; they are never made executable by the new runtime.

## Scope and ownership

smolworld owns the private L2 network, DNS, stable addresses, deterministic machine identity, world lifecycle, exact cleanup, and later coordinated world checkpointing. smolvm owns each machine's Smolfile, persistent machine storage, guest agent, image handling, and VMM invocation. Niceforge owns the sealed workflow, workspace, step ordering, leases, evidence, durable transition records, and policy.

The migration does not turn smolworld into a workflow engine. It must not grow workflow steps, generic service readiness, restart policy, host networking, port publishing, guest Internet egress, or Docker/Compose compatibility.

No new Rust dependency is in scope without explicit approval.

## Target world format

Smolfiles are the single source of truth for one machine's image, command, environment, working directory, and resources. The .smolworld file is only the cross-machine topology and private-network contract.

    # .smolworld
    format: 2

    world:
      name: sentry-backend

    network:
      subnet: 10.96.0.0/24
      domain: sentry.test

    machines:
      postgres:
        smolfile: ./smol/postgres.Smolfile
      redis:
        smolfile: ./smol/redis.Smolfile
      kafka:
        smolfile: ./smol/kafka.Smolfile
      clickhouse:
        smolfile: ./smol/clickhouse.Smolfile
        seed_files:
          - source: ./assets/sentry-backend/clickhouse/config.xml
            destination: /etc/clickhouse-server/config.d/niceforge.xml
            mode: "0644"
          - source: ./assets/sentry-backend/clickhouse/users.xml
            destination: /etc/clickhouse-server/users.d/niceforge.xml
            mode: "0644"
      snuba:
        smolfile: ./smol/snuba.Smolfile
        depends_on: [postgres, redis, kafka, clickhouse]
      runner:
        smolfile: ./smol/runner.Smolfile
        depends_on: [postgres, redis, kafka, clickhouse, snuba]

    # smol/postgres.Smolfile
    image = "docker.io/library/postgres@sha256:7958605b474b3d264a969cb3a123d6aa00ad1e1fe9da8a69984dabb704d93317"
    cpus = 1
    memory = 512
    storage = 4
    overlay = 2
    env = [
      "POSTGRES_HOST_AUTH_METHOD=trust",
      "POSTGRES_DB=sentry",
    ]

The world-facing Smolfile profile is intentionally narrower than standalone smolvm. It permits only a local or immutable-image source, command, environment, work directory, and machine resources. It rejects net, ports, volumes, SSH-agent forwarding, Docker sockets, egress filters, restart configuration, health checks, and other host-capability or lifecycle settings. smolworld supplies the complete external virtio-net tuple; a Smolfile cannot override or supplement it.

Seed files are copied into the target machine's private persistent state before its workload is released. They are not host mounts. Sources must be sealed regular files, destinations must be absolute guest paths, and the copy operation must be all-or-nothing before that machine starts.

depends_on remains creation/start order only. Any service-specific waiting is an explicit Niceforge runner bootstrap action and produces ordinary step logs; it is never a hidden smolworld readiness contract.

## Material identity and image preparation

A Smolfile is a machine declaration, not by itself an immutable prepared image. Each world must therefore have a generated, sealed .smolworld.lock that binds:

- canonical .smolworld bytes and every referenced Smolfile digest;
- every selected image's immutable OCI digest and verified local material;
- every seed-file source digest and destination/mode; and
- the smolvm external-world resolver ABI used to materialize it.

The resolver is host-side. Guests never pull images and have no Internet egress. The preparation path must not require a Docker daemon, Docker socket, or OrbStack. Its exact implementation is a focused smolvm/smolworld design spike: it must produce a verified local material suitable for an externally networked persistent machine without exposing that material detail in the user-authored .smolworld grammar.

Image acquisition is an explicit, mutating preparation boundary. The proposed command contract is:

    smolworld prepare  -> resolve immutable image sources and write the local material and lockfile
    smolworld check    -> read-only validation of the Smolfiles, lockfile, local material, and runtime
    smolworld up       -> create a world only from prepared local material

smolworld must not parse or reimplement Smolfile semantics. The companion smolvm boundary needs a non-mutating external-world check/resolve operation. It validates the Smolfile against the restricted profile and local material, and is used before smolworld allocates state, creates a listener, or creates a machine.

## Foundation gate: two-machine Smolfile world

Before Sentry, convert the existing Redis example into a Smolfile-composed Redis and runner world. This small gate proves the new boundaries in isolation:

1. host-side Dockerless preparation produces a lockfile and verified local material;
2. check is entirely read-only after preparation;
3. smolworld launches each machine from its Smolfile with only the external private NIC injected;
4. private DNS and Redis communication work between the two machines; and
5. exact cleanup leaves no recorded machine, switch socket, or runtime directory behind.

The foundation gate must run without docker, docker compose, DOCKER_HOST, orbctl, or a Docker socket. It is an implementation prerequisite, not a replacement for the Sentry acceptance gate.

## First workload acceptance gate: Sentry backend world

The first workload deliverable lives at /Users/josh/dev/niceforge/showcase/sentry-backend/. It replaces the archived tests/fixtures/sentry-backend-v0/workflow.yml service graph with:

- postgres, redis, kafka, clickhouse, and snuba Smolfiles;
- a runner Smolfile that stays available for ordered Niceforge commands;
- ClickHouse configuration delivered through sealed seed files, replacing the old file mounts;
- the Sentry runner environment (DATABASE_URL, SNUBA, cache paths, and skip flags) delivered as sealed step inputs, not service configuration; and
- the exact Sentry test tests/sentry/models/test_base.py::AvailableOnTest::test_available_on_same_mode.

The Redis localhost bridge remains an explicit test/workload concern until the Sentry upstream test stops requiring it. It must not become smolworld behavior.

The exact service resource values are acceptance-test inputs, not guessed defaults. Establish them by measuring successful startup and test execution on the supported Apple-Silicon host, then record the values in each Smolfile.

The Sentry end-to-end test must prove all of the following:

1. smolworld check validates all world and host material prerequisites without creating a state directory, listener, or smolvm machine.
2. The five service machines start in declared order and the runner resolves every service by short name and machine.sentry.test.
3. The explicit runner bootstrap establishes service availability and the exact Sentry test passes with no guest Internet access.
4. The execution and materialization paths run without docker, docker compose, DOCKER_HOST, orbctl, or a Docker socket.
5. down removes only the recorded Sentry machines and runtime sockets; unrelated smolvm machines are untouched.

Before relying on this gate, re-run the normal smolworld baseline in both its ordinary and serial forms. The initial exploration observed one parallel test failure in state::tests::legacy_state_defaults_to_recorded_but_absent that did not reproduce in focused or serial runs; test isolation must be made reliable if it recurs.

This gate is deliberately success-path only. It does not retain failed worlds, create checkpoints, restore a world, expose a Niceforge CLI command, or change the Niceforge database. A successful run performs immediate exact cleanup.

## smolworld implementation plan

The current implementation can evolve in place. Keep the switch, gateway, framing, allocation, and exact-recorded-machine cleanup invariants. Replace the parts that duplicate smolvm's machine configuration domain.

1. Add the external-world Smolfile profile and non-mutating smolvm check/resolve contract, plus explicit host-side prepare behavior.
2. Replace MachineConfig image, command, and resource fields in src/model.rs with a Smolfile reference and explicit seed-file model.
3. Change src/config.rs to parse only world topology, Smolfile paths, dependency order, and sealed file-copy declarations. Reject old image and command keys rather than retaining aliases.
4. Replace src/smolvm.rs preflight and creation calls with the external-world Smolfile check/resolve and launch contract. smolworld passes only its generated name and static external-network tuple.
5. Extend src/state.rs with a format-versioned lock/material record and keep v2 state under a new HOME/.smolworld/v2 namespace. v2 must never infer ownership of v1 state or delete it.
6. Adapt src/runtime.rs and src/cli.rs around the resolved machine model. Retain exact ownership and recovery behavior; add no generic daemon or unscoped cleanup mechanism.
7. Convert the Redis example first, then add the opt-in Sentry integration test.
8. Update README.md, AGENTS.md, and module comments so search results name Smolfile composition as the supported configuration model.

## Niceforge executor cutover

Niceforge is not a clean-room rewrite. Preserve its PostgreSQL lifecycle, sealed snapshots and plans, lease fencing, event receipts, audit oracle, evidence store, scheduler, and executor transport. Replace only the execution substrate.

The new workflow ABI has an explicit world binding, for example:

    runtime:
      world: .smolworld
      runner_machine: runner
      workspace: /workspace

The sealed Niceforge plan copies and binds the world file, referenced Smolfiles, lockfile, declared static assets, runner profile/action inputs, and the checkout. Niceforge materializes the sealed world, copies the sealed workspace into the runner's private workspace, and invokes each fixed runner action there. It never allows an executor to consult a mutable checkout or a mutable world file.

Replace the Docker/Compose execution surfaces—including ServiceSpec, Compose rendering, Docker CLI/Engine resource handling, Docker runner-image builds, and Docker fixtures—with a narrow SmolworldExecutor adapter. Its operations are closed and exact: check/resolve, start, await explicit runner bootstrap, copy sealed input, execute a runner action, inspect, capture, shell, stop, and delete.

The executor records generated world and machine identities before external effects. Reconciliation acts only on those exact identities and never scans or prunes smolvm state.

## Niceforge schema and cutover

Use two deliberate, versioned PostgreSQL migrations. The first begins only after the standalone Sentry gate stabilizes the Smolfile and materialization contract. The second begins only after a coordinated multi-machine checkpoint spike establishes the durable capture and restore boundary.

The execution-substrate migration introduces only the facts necessary to materialize, run, and clean up a world:

| Record | Durable meaning |
| --- | --- |
| world_definitions | The sealed topology, Smolfile, and lockfile inputs for a plan. |
| world_materials | Exact verified image/static-input observations. |
| world_instances | One generated, lease-bound physical materialization of a job attempt. |

run_resources and resource_operations gain closed smolworld roles and operations with the exact world/machine identity required for a retry or reconciler. No open JSON resource descriptions are introduced.

The later lineage migration adds world_states, world_transitions, and world_checkpoints only after their state channels, capture consistency, restore behavior, and garbage-collection rules have focused test evidence. This avoids freezing an unproven checkpoint design into the primary executor cutover.

The cutover policy is strict:

- new dispatches seal only the new world-backed plan ABI;
- new code does not schedule or execute pre-cutover Compose plans;
- existing immutable plans, attempts, events, and evidence remain queryable historical records, but no Docker compatibility adapter survives; and
- Docker/Compose grammar, executor code, docs, fixtures, and operational commands are removed after the world-backed acceptance suite passes.

Keeping historical facts is evidence retention, not runtime compatibility.

## Step-level world transitions

This phase begins after the world-backed executor is stable and the second migration has been reviewed. It is not required for the Redis foundation gate, standalone Sentry gate, or the first Niceforge world-executor cutover.

The logical objects are deliberately separate:

    WorldState       = sealed topology/materials + workspace state + machine-state manifest
    WorldTransition  = parent state + exact step attempt + outcome/evidence + child state
    WorldCheckpoint  = (WorldState, smolvm materializer ABI, acceleration artifact)

A VM disk snapshot, RAM image, or smolvm fork is a checkpoint implementation; it is not the identity of a world state. Caches are similarly acceleration inputs only. A cache must not be the sole source of a semantically required dependency: the sealed material set must suffice for reproducible execution.

For each step, Niceforge uses a commit barrier:

1. Record a lease-fenced, idempotent transition-capture intent before capture.
2. Quiesce the world at the declared step boundary and capture every relevant machine and sealed workspace state.
3. Verify and seal the resulting state manifest and checkpoint receipts as evidence.
4. In one PostgreSQL transaction, validate the current lease/fence, attach the child world state, record the world transition, and append the matching step-terminal event.
5. A crash before the transaction leaves only unreachable capture candidates; reconciliation may validate or delete those exact candidates. A crash after the transaction leaves a durable state receipt that must be revalidated before materialization.

The same barrier runs for failed steps. Therefore a failure has a sealed state reference rather than only logs and a process exit code.

The required smolvm/smolworld spike is coordinated, multi-machine capture and restore. Current single-machine smolvm machine fork freezes one forkable golden and cannot itself establish an atomic world checkpoint. The new contract must prove consistent multi-machine capture, distinct generated machine names and switch sockets on restore, and safe handling of the private network.

## Failed-world inspection

After the Sentry world and executor are working, add local failed-world inspection.

1. A failed step first receives the sealed state/checkpoint described above.
2. niceforge world shell with a run attempt, job attempt, and machine materializes a disposable inspection descendant from that exact state.
3. It opens an interactive smolvm machine shell session for the selected machine. The descendant has exact generated ownership and a bounded retention/explicit-cleanup policy.
4. Shell mutations never rewrite the canonical failed state or resume the failed job. They are inspection-only descendant changes.

The initial local interface is intentionally a shell, not literal TCP OpenSSH. It requires no added guest authentication because it is controlled by the calling host user and smolvm's local machine boundary. Literal ssh from the host would require a host-facing transport or port publication and therefore a separate approved expansion of smolworld's no-host-networking contract.

## Delivery order

1. Specify and test the restricted Smolfile external-world profile and its non-mutating smolvm validation/material-resolution command.
2. Implement explicit Dockerless preparation, then pass the two-machine Redis foundation gate.
3. Convert smolworld to Smolfile-composed topology and preserve all existing L2/DNS/ownership guarantees.
4. Add the prepared Sentry world and pass the standalone success-path Sentry gate without Docker or OrbStack, followed by immediate exact cleanup.
5. Design and test Niceforge's new sealed world plan, SmolworldExecutor, and exact resource reconciliation behind the existing control-plane interfaces.
6. Apply the execution-substrate migration and make world-backed plans the only executable plan ABI.
7. Delete Compose/Docker execution paths and their compatibility tests/docs; retain only read-only historical evidence data.
8. Prove coordinated capture and restore independently, then apply the lineage migration for world states, transitions, and checkpoints.
9. Add disposable failed-world shell access and its retention/cleanup tests.

## Required verification

For smolworld changes, start with focused unit tests and finish with:

    cargo fmt --check
    cargo test
    cargo clippy -- -D warnings
    git diff --check

The opt-in integration sequence first proves the two-machine Redis foundation world, then the Sentry world. The Sentry test validates DNS, all service communication, the exact Sentry workload, Dockerless execution, and recorded-world-only immediate cleanup on a prepared Apple-Silicon host.

For Niceforge changes, follow its active PostgreSQL contract: focused transition/lease/evidence tests first, then its normal formatter and test baseline. Add process-boundary fault tests at provisioning, active-step, capture, commit-barrier, failed-world retention, and cleanup boundaries. The audit oracle must independently reject a transition/checkpoint projection that does not match the ordered event and evidence record.

## Explicitly deferred

- literal host-reachable SSH and unauthenticated guest SSH daemons;
- any host networking, NAT, TAP/vmnet, port publishing, DHCP, IPv6, or guest Internet egress;
- general service health checks or restart policies in smolworld;
- generic Compose, Dockerfile, or Docker resource compatibility;
- retained failure worlds, checkpoint restore, and failed-world shell access through the first Niceforge executor cutover; and
- speculative multi-world branching before one lineage has correct, coordinated capture and restore semantics.
