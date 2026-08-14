# NOICEFORGE: durable world control-plane plan

'NOICEFORGE.md' is the durable integration and implementation plan for the
Niceforge control plane over Smolworld. The filename is intentional: this is
the spelling of the plan we are carrying forward.

The downloaded control-plane architecture reinforces the central idea, with
one important sharpening:

> Separate immutable world state, transition causality, and evaluation
> judgment. A VM snapshot is an acceleration artifact, not the identity of a
> world.

This document is written for a coding agent. It names the contracts that must
survive implementation, the owners of each boundary, the order of work, and
the acceptance evidence required before the next layer begins.

## 1. The product

Niceforge is a persistent control plane for an indefinitely evolving digital
world. A user supplies a constitution/mission, a repository, and a sealed
'.smolworld' laboratory. The system creates temporary offices, harnesses, and
agents as needed; performs experiments in isolated worlds; records evidence;
and advances a canonical trajectory when an authorized office selects a
transition.

There is no final world. The product is the durable trajectory:

~~~
constitution + mission + initial world
    -> objectives and experiments
    -> world transitions and evaluations
    -> institutional decisions and propagation
    -> the next canonical world
    -> ...
~~~

The first concrete user-visible slice is the failed-step workflow:

~~~
Sentry job step fails
    -> capture the complete world as immutable W1
    -> retain W1 and its evidence
    -> niceforge ssh sentry-backend W1
    -> inspect a disposable descendant
    -> retry the failed step from W1
    -> produce W2 without replaying the workflow from the beginning
~~~

The initial shell may be host-local SmolVM execution rather than literal TCP
SSH. Smolworld deliberately has no host networking, port publishing, or guest
Internet egress. Literal host-reachable SSH is a separate product expansion
and is not required for the first useful interface.

## 2. The conceptual model

The world graph and the institutional graph are distinct durable structures.

### 2.1 World graph: what exists or could exist

Use separate objects for state, transition, evaluation, and acceleration:

~~~
WorldState S
    immutable logical state; content-addressed by canonical manifest

WorldTransition T
    parents + delta + actor/office + objective + provenance -> child state

Evaluation E
    state + evaluator + result + evidence + uncertainty

WorldCheckpoint C
    state + materializer ABI + disposable acceleration artifact

WorldRun R
    one mutable materialization of a WorldState
~~~

The state identity must not include the transition that produced it or an
evaluation that inspected it. Two independent transitions may produce
byte-identical states and should deduplicate to one state while retaining both
transition records. A later evaluation must not mutate state identity.

The durable semantic object is a logical manifest over named channels:

~~~
WorldState W1
  source/checkout       -> Merkle root
  source/world          -> canonical .smolworld + lock identity
  materials/images      -> immutable material receipts
  topology/services     -> machine and network manifest
  workspace/runner      -> workspace state receipt
  nondeterminism/input  -> captured external observations
  lineage               -> parent state references
~~~

Git patches are the first source-tree 'WorldDelta' codec. They are not the
world abstraction. Future channels may use Merkle deltas, database logical
transactions, overlay deltas, generated-material recipes, or captured
observations.

The semantic closure rule is:

> Every semantically relevant input is immutable, deterministically derivable,
> or explicitly captured as nondeterministic evidence.

VM disks, RAM images, package caches, compiler outputs, clonefiles, worktrees,
and page caches are materializations or acceleration inputs. They may be
discarded and regenerated without losing logical state. A checkpoint must
never become the sole source of a semantically required dependency.

### 2.2 Institutional graph: what the system believes and does

The institutional graph contains durable objects such as:

~~~
Constitution / Mission
Office / Charter / Authority
Objective / Commitment
Hypothesis / Question
Experiment / Proposal
Claim / Counterargument
Evaluation / Evidence
Decision / Design principle
World transition
~~~

An office is persistent; an agent is a fungible occupant. An office owns
jurisdiction, authority, obligations, budget, subscriptions, unresolved
agenda, and institutional memory. A temporary agent receives a bounded
just-in-time harness compiled from the office, objective, world, relevant
institutional state, capabilities, and budget.

The forum is a projection over this typed institutional graph, not a social
network of persistent agent personalities. Scratch reasoning dies;
consequential deltas, evidence, claims, questions, and decisions survive.
Information propagation routes those objects to offices whose future
decisions may be affected. This layer is later than the world substrate; the
first vertical slice can use one root office and explicit objectives.

## 3. Ownership and non-negotiable constraints

~~~
Niceforge control plane
  constitution, mission, sealed workflow, objectives, offices, leases,
  step ordering, world lineage, transitions, evaluations, evidence, policy

smolworld
  .smolworld parsing, durable material/allocation state, private L2 switch,
  authoritative DNS gateway, world lifecycle, checkpoint coordination,
  logical machine identity, exact recorded-world cleanup

smolvm
  Smolfile profile/materialization, OCI image handling, persistent machine
  storage, guest agent, machine lifecycle, external virtio-net attachment,
  VMM/libkrun invocation, per-machine capture/restore primitive

libkrun/libkrunfw
  VMM, vCPU, memory, block, filesystem, and virtual-device mechanisms

Evidence/CAS adapter
  immutable manifests and evidence objects; PostgreSQL stores receipts and
  authority, not unbounded payload bytes
~~~

Keep these constraints in every phase:

- Supported execution is macOS on Apple Silicon ('Darwin', 'aarch64') only.
  Linux and Windows are unsupported build/runtime targets; bundled artifacts in
  companion repositories remain untouched.
- No third-party Rust dependency without explicit approval.
- Do not run pre-commit hooks or push a remote.
- smolworld is not a workflow engine. It must not grow workflow steps,
  generic service readiness, restart policies, Compose compatibility, or
  health-check semantics.
- 'depends_on' means creation/start order only. Readiness is an explicit
  Niceforge runner action and ordinary step evidence.
- No host networking, NAT, TAP/vmnet, port publishing, DHCP, IPv6, or guest
  Internet egress.
- Guests never pull images. Images and static inputs are host-prepared,
  sealed local material.
- Cleanup is scoped to exact generated identities recorded by the world.
  Never scan or prune unrelated SmolVM state.
- Invalid states must not cross module, database, process, or restore
  boundaries. Prefer narrow types, canonical bytes, explicit receipts, and
  lease/fence checks.

## 4. Current baseline and evidence

The current Smolworld implementation has completed the external-world
materialization boundary, macOS-only cutover, private network behavior, and a
real Sentry workload gate. The current code is not yet the durable world
control plane described here.

### Proven today

- Smolfile-composed worlds with sealed '.smolworld.lock' material identity.
- Dockerless host preparation and read-only 'check'.
- Private DNS and Redis traffic over the userspace Ethernet segment.
- External Unix-stream NIC reconnect after a live SmolVM fork.
- Frozen-golden replacement NIC attachment and stale-generation protection.
- Exact machine/runtime cleanup.
- Parallel independent material preparation and dependency-wave create/start,
  committed in Smolworld as '17e2f56'.
- Linux/arm64 Sentry 'checkout.tar' and 'python-site.tar' preparation,
  successful pytest collection, and the exact model test.

The real Redis fork fixture measured on 2026-08-14:

~~~
fork transition                         109.852 ms
clone agent + private NIC ready          44.450 ms
accounted storage delta                 250,454,016 bytes
physical APFS delta                           90,112 bytes
~~~

This is an APFS sharing observation, not a durable checkpoint format. The
fork remains a live 1-to-N acceleration primitive: its frozen golden and
backing RAM must remain alive. It is not a durable 'WorldState'.

### Current gaps

- Niceforge 'snapshot_id' identifies sealed workflow/source material, not VM
  RAM, writable disk, workspace, switch, or world state.
- 'SmolworldExecutor' starts one world and later tears it down. It has no
  capture, retention, restore, world-state lineage, or shell operation.
- Niceforge's executor transport is whole-job and terminal-result oriented.
  It does not stream durable step transitions or checkpoint after each step.
- 'Version2JobRuntime' keeps completed step context in process memory.
- PostgreSQL has a 'job_step_results' schema/projection, but the current V2
  path does not write step-start/step-complete transitions. Current durable
  lifecycle events are job-level.
- Retry derives a fresh queued run attempt from the same sealed plan/source
  snapshot. It does not restore a failed world.
- Failed worlds are cleaned up on the current success-path executor gate.
- There is no 'niceforge ssh ... W1' command or host-local inspection shell.
- There is no coordinated multi-machine capture barrier or atomic world state
  manifest.

Do not describe the existing source snapshot, job attempt, or live fork as W1.
W1 begins only when all semantically relevant world channels have been
captured and the corresponding immutable state receipt has been committed.

## 5. Supported .smolworld laboratory contract

Smolfiles are the source of truth for one machine's image, command,
environment, working directory, and resources. .smolworld owns only topology
and private-network relationships.

~~~yaml
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
~~~

The restricted world-facing Smolfile profile permits only local or immutable
image material, command, environment, working directory, and machine
resources. It rejects 'net', ports, volumes, SSH-agent forwarding, Docker
sockets, egress filters, restart configuration, health checks, and other host
capabilities. smolworld supplies the complete external virtio-net tuple; a
Smolfile cannot add or override a NIC.

Seed files are sealed regular host files copied all-or-nothing into private
machine state before workload release. They are not host mounts. Sources must
remain beneath the sealed world root; destinations are normalized absolute
guest paths.

Each world has a generated lock/material record binding:

- canonical .smolworld bytes and each referenced Smolfile digest;
- immutable OCI source digest and verified local material identity;
- seed source digest, destination, and mode; and
- the smolvm external-world resolver ABI.

The command boundary is:

~~~
smolworld prepare  mutating host materialization and lock creation
smolworld check    read-only prerequisite and lock verification
smolworld up       create/start only from prepared local material
~~~

The resolver belongs to smolvm. smolworld must not parse or reimplement
Smolfile semantics.

## 6. Niceforge runtime contract

The sealed V2 plan carries an explicit world binding:

~~~yaml
runtime:
  world: .smolworld
  runner-machine: runner
  workspace: /workspace
~~~

The plan/snapshot boundary binds the exact world file, referenced Smolfiles,
lockfile, static assets, runner inputs, and source checkout. An executor never
consults a mutable worktree, YAML file, or mutable world definition.

The first executor API may remain host-local, but it must expose typed
operations with explicit ownership:

~~~
prepare/check
materialize or restore WorldState
await runner attachment
execute one fixed step action
capture a transition
inspect a retained descendant
stop/pause/resume
release or retain exact world resources
~~~

Do not preserve a generic Docker/Compose fallback. Historical pre-cutover
plans remain read-only evidence and are never made executable by the new
runtime.

## 7. Durable world-state schema

Do not overload source snapshots or VM files with world semantics. Add a
versioned world-lineage model after the checkpoint substrate has focused test
evidence.

### 7.1 Logical records

The exact SQL names may evolve, but the semantics must remain explicit:

~~~
world_states
  state_id, tenant, canonical_manifest, state_digest, schema, created_at

world_state_channels
  state_id, channel_kind, logical_name, content_digest, derivation_receipt

world_transitions
  transition_id, parent_state_ids, child_state_id, objective/step identity,
  actor invocation, office, delta receipt, evidence roots, outcome, timestamps

world_checkpoints
  checkpoint_id, state_id, materializer ABI, manifest receipt, acceleration
  artifact receipt, retention/root status

world_runs
  world_run_id, state_id, run/job/step lease, generated resource identities,
  lifecycle, current epoch, created/stopped timestamps

world_machine_states
  checkpoint/state, machine identity, Smolfile/material identity, RAM/disk/
  device receipts, guest identity policy, restore metadata

world_switch_states
  checkpoint/state, switch epoch, FDB receipt, bounded queued-frame receipt,
  logical port identities, restore/rebind metadata
~~~

The first implementation may use eager immutable directories and filesystem
CAS. It must still expose the logical manifest so parent-plus-delta storage,
dirty-page tracking, or checkpoint flattening remains an acceleration change.

### 7.2 State, transition, and evaluation rules

- State identity is canonical and content-addressed.
- Transition identity is unique even when the child state deduplicates.
- Evaluation receipts identify evaluator version, input state, result, evidence,
  and uncertainty. Evaluations never mutate state identity.
- 'fork(state, n)' creates branch references before materializing machines.
- 'materialize(state)' creates a mutable 'WorldRun' under exact ownership.
- 'commit(parents, delta, evidence)' creates or finds the child state and
  records the transition.
- There is no privileged world 'merge'; reconciliation is an N-parent
  transition and is deferred until lineage is stable.
- 'GC' removes only unreachable unpinned states, transitions, checkpoints,
  and materializations. Failed worlds and inspection descendants are roots.

### 7.3 Identity policy

A same-lineage restore may preserve guest static IP/MAC only after the source
'WorldRun' is stopped and detached. Concurrent descendants require a reseed
protocol for static IPv4, MAC, machine identity, entropy, and guest
credentials. New socket paths alone are not a valid identity fork.

Host file descriptors, Unix listeners, and other host-local handles never enter
a checkpoint. Restore creates fresh host resources and rebinds captured
logical devices to them.

## 8. Coordinated checkpoint barrier

A world checkpoint is one temporal cut across every machine and the switch.
Restoring RAM from one point while using a disk overlay or network state from
another is invalid.

For each workflow step, Niceforge owns this lease-fenced barrier:

1. Record an idempotent transition-capture intent before external capture.
2. Close the smolworld switch at a new epoch. Reject new runner actions and
   'exec' calls; stop delivering frames; record FDB and bounded queued state.
3. Pause every VM concurrently. Do not capture one machine while another can
   still observe or emit traffic.
4. Capture RAM, writable disk/overlay, virtual-device state, runner workspace,
   and material identity for every machine. Capture is FD-free.
5. Seal machine receipts, switch epoch, workspace, topology, and material
   receipts into one canonical 'WorldState' manifest.
6. Verify receipts and publish the checkpoint artifact as evidence.
7. In one PostgreSQL transaction, revalidate the lease/fence, attach the child
   state, record 'WorldTransition', persist the step outcome, and append the
   matching event.
8. On pre-commit failure, leave only exact unreachable capture candidates for
   reconciliation. On post-commit crash, revalidate the durable receipt before
   rematerialization.

The initial implementation may exit captured VMs and restore a fresh child for
the next action. Capture-and-continue is a later optimization; it must not
change the state or transaction contract.

## 9. Step execution and resume semantics

The current terminal whole-job protocol must evolve into a durable step
protocol without giving the executor database authority.

### 9.1 Step lifecycle

Persist an exact attempt and state transition for every step:

~~~
pending -> preparing -> running -> capture_requested -> finalizing -> completed
                                      \-> failed/retained
                                      \-> cancelled/lost
~~~

Each transition carries run/job/step/attempt identity, lease and executor
fence, monotonic executor sequence, world state before/after, action/input
digests, outcome/reason, evidence receipts, and idempotency/correlation
identity.

The executor may keep expression context and completed steps in memory for
speed, but every semantically relevant boundary must be reconstructable from
PostgreSQL events and world receipts. Step outputs that affect later
expressions must be persisted at the step boundary.

### 9.2 Failed step

On failure, capture the post-failure world before cleanup. The failed state W1
is immutable and retained according to policy. It includes service state,
runner workspace, process state, and private network state required to
understand the failure.

The failed step result points to W1. Logs and test output remain evidence, not
the only reproduction mechanism.

### 9.3 Retry from W1

Retry is a new child 'WorldRun' and 'WorldTransition', not a mutation of W1:

~~~
W1 (retained, immutable)
  -> restore/clone child run
  -> reseed identities if concurrent
  -> execute the failed step again
  -> W2
~~~

Completed prior steps are not replayed. The retry preserves the exact plan,
source snapshot, and step-input semantics while allowing the failed step's
world mutations to differ. A failed-world shell is inspection-only and
cannot resume or rewrite W1.

## 10. Failed-world shell interface

The first interface should be a local, capability-checked command with an
SSH-shaped user experience:

~~~
niceforge ssh <world-or-repository> <state-id> [machine] [-- command]
~~~

The command must resolve the state through durable lineage, authorize the
caller against retained-state/job/office scope, materialize a disposable
inspection descendant, reconnect fresh agent/NIC host handles, open a local
smolvm shell/exec session, and record/clean up the inspection descendant.

Inspection mutations create an unselected descendant or disposable overlay.
They never rewrite the canonical failed state and never silently become the
retry attempt. Literal TCP OpenSSH and guest SSH daemons remain out of scope.

## 11. Implementation workstreams and acceptance gates

Work in this order. A gate is a stop condition, not a documentation claim:
record the command, fixture, commit, timing, receipts, and cleanup result
before proceeding.

### Gate 0: preserve the current baseline

Before changing the checkpoint or Niceforge boundary:

- run Smolworld focused tests and 'cargo fmt --check', 'cargo test',
  'cargo clippy -- -D warnings', and 'git diff --check';
- run the Redis foundation E2E and opt-in fork E2E on the supported host;
- confirm frozen-golden replacement NIC attachment and stale-generation
  protection;
- confirm Linux/Windows bundled artifacts are unchanged.

Acceptance: existing evidence remains reproducible and unrelated SmolVM
machines are untouched after cleanup.

### Gate 1: single-machine durable checkpoint substrate

Owner: smolvm/libkrun, with smolworld as external-NIC contract tester.

Prove pause/capture/restore/resume with active process, TCP, filesystem, and
external Unix-stream virtio-net traffic; fresh host descriptors; full declared
resources including required multi-vCPU cases; reopenable RAM and writable
disk or an equivalent immutable codec; guest agent/NIC reconnect; and explicit
failure for unsupported devices with no partial durable checkpoint.

Known evidence: upstream libkrun PR #762 passed a minimal snapshot-resume test
but rejected Smolworld's explicit vsock and external Unix-stream virtio-net
devices. The canonical SmolVM fork has a separate live fork path, but that is
not evidence of a durable checkpoint. Keep the upstream experiment isolated.

Acceptance artifacts: focused capture/restore tests, an external-NIC real-VM
test with fresh host handles, a reopen-after-process-exit test, canonical
RAM/disk/device receipt vectors, and no unrelated VMM/device changes.

### Gate 2: coordinated two-machine Smolworld checkpoint

Owner: smolworld.

Add a narrow API around the existing world supervisor and switch:

~~~
checkpoint(WorldRun, transition_intent) -> capture_candidate
restore(WorldState, WorldRunSpec) -> WorldRun
retain/release(WorldState)
~~~

Use the Redis + runner foundation. Prove switch epoch closure before VM pause,
both machines paused before capture commit, DNS/Redis resume, FDB/queue
representation, fresh listeners/handles, same-lineage and concurrent identity
rules, exact failure cleanup, and exact recorded-world ownership.

Acceptance: a real two-machine world restores from one state and the runner
observes the same service/process/filesystem state without cold bootstrap.

### Gate 3: Sentry world checkpoint and scaling

Owner: smolworld plus the Niceforge local integration harness.

Run the six-machine Sentry world using host-prepared Linux/arm64
'checkout.tar' and 'python-site.tar'. Prove parallel independent
preparation/creation, capture with active services and workspace, restored
DNS/Redis/Snuba traffic, pytest collection and the exact model test, per-phase
timings, storage, larger aggregate image/workspace behavior, and exact release.

Acceptance: a representative retry is faster than restarting the full world,
or measurements identify the substrate phase that must be optimized before
the path is enabled by policy.

### Gate 4: Niceforge durable world-lineage migration

Owner: Niceforge PostgreSQL control plane.

Add versioned migrations only after Gates 1–2 establish capture receipts.
Preserve plan/source snapshot identities as separate facts. Add typed APIs for
create/verify/pin/release state; register and commit capture candidates;
parent/child transitions; world-run lease/restore; machine/switch receipts;
failed-state retention; exact GC roots; and audit replay.

Use closed world roles, operation codes, manifest digests, and generated
identities. Do not accept open-ended JSON resources.

Acceptance: idempotent lease-fenced commit; crash-before-commit cleanup;
crash-after-commit reopen; duplicate-request safety; audit agreement with the
event stream; and tenant/authority isolation.

### Gate 5: durable step protocol

Owner: Niceforge executor gateway and runner transport.

Replace terminal-only step visibility with a typed, fenced, monotonic protocol:

~~~
step_prepare
step_started
step_output/evidence receipt
transition_capture_requested
step_finalizing
step_completed
step_failed
~~~

The executor receives no database handle or lifecycle authority. Kill it after
each boundary and recover deterministically. Reject duplicate/out-of-order
messages. Show active step timing in the web snapshot. Preserve ordinary
job-level lease/retry behavior for failures outside world capture.

### Gate 6: failed Sentry state W1

Add a deterministic failing Sentry step after database/schema mutation and
before the final test. Prove the failure is recorded, W1 is captured before
cleanup, W1 contains service/workspace/process/switch state, W1 is retained
by an explicit root, ordinary shutdown does not delete it, and unrelated worlds
are untouched.

Acceptance artifact: run ID, step attempt ID, W1 ID, manifest digest, capture
receipt, and a command that restores W1 after the original supervisor exits.

### Gate 7: 'niceforge ssh' inspection

Implement the host-local shell command. Prove authorization, exact state and
machine resolution, fresh agent/NIC handles, observation of failed state,
disposable descendant isolation, idempotent cleanup, and byte-identical
canonical W1 after inspection.

Acceptance: 'niceforge ssh sentry-backend W1 runner' opens the failed
environment without restarting the workflow.

### Gate 8: retry one failed step from W1

Add a typed retry-step operation. It must authorize W1, create a child run,
restore prior service/workspace state, execute only the selected step, capture
W2, preserve transition/evidence lineage, schedule downstream work from the
child result, and retain W1 while GC handles unselected descendants.

Acceptance: a deliberately transient Sentry failure succeeds on retry; retry
wall time excludes world bootstrap and completed steps; W1 remains inspectable;
and the event/evidence graph explains the transition.

### Gate 9: cheap branch references and concurrent descendants

Implement 'fork(W1, N)' as references before materialization. Materialize
concurrent children only after guest reseed and private identity rules pass.
Prove metadata-only unmaterialized forks, distinct generated identities,
immutable parents/siblings, shared immutable material, pinned-root retention,
and reachability GC. Do not add world merge semantics.

### Gate 10: institutional MVP

Do not build a social chat system first. Add the smallest typed institutional
graph that can drive a persistent trajectory:

- constitution/mission;
- durable offices with jurisdiction, authority, subscriptions, and budget;
- objectives and delegated child objectives;
- experiment/proposal records linked to transitions;
- claims, questions, counterarguments, and decisions linked to evidence;
- evaluator receipts with version and uncertainty;
- semantic propagation subscriptions;
- a JIT harness compiler for bounded context/capabilities.

The first office may be the Grand Architect and its first occupant may be one
configured model or human. Do not make an agent process identity the durable
institutional speaker.

Acceptance: an objective delegates an experiment against a world; the
experiment creates a transition/evaluation; a claim routes to a relevant
office; a decision selects a child without rewriting evidence; a replacement
agent can occupy the same office with compiled context; and status can rebuild
the trajectory without private scratch transcripts.

## 12. Sentry acceptance scenario

The Sentry fixture is the first complete vertical proof, not a generic
workflow-engine fixture. It remains Dockerless and Apple-Silicon-only.

The workflow steps are:

~~~
checkout
cache restore
source/dependency preparation
artifact publication
cache save
~~~

The acceptance harness adds a controlled failure mode and runs:

~~~
prepare/check
materialize W0
run preparation
capture W1 after injected failure
inspect W1 through niceforge ssh
retry only the failed step from W1
capture W2
run exact pytest collection/test action
publish evidence
release or pin final state
~~~

Assert no Docker binary/socket, Compose, 'DOCKER_HOST', OrbStack, or guest
image fetch; private service traffic after restore; '1 test collected' for the
focused pytest collection; the exact model test passing; durable/auditable
step/job/world event order; immutable W1 after W2; no rerun of completed
steps; and exact cleanup of only unpinned descendants.

The target-platform artifacts are host-prepared and not committed:

~~~
checkout.tar       Linux/arm64 Sentry source archive
python-site.tar    Linux/arm64 Python dependency closure
~~~

## 13. Performance and scaling plan

Measure separately:

~~~
logical fork/reference creation
checkpoint barrier request
switch quiescence
VM pause/capture per machine
manifest/CAS sealing
restore process launch
agent ready
NIC/DNS/private-traffic ready
step resume ready
accounted storage and physical APFS bytes
~~~

The current 109.852 ms live fork transition is the first process/handshake and
barrier-overhead optimization target. Profile its phases before assigning the
time to any one subsystem. It is not a reason to define W1 as a live golden.
The durable priority is the coordinated multi-machine cut and reopenable
memory/disk representation. After correctness, use parent-plus-delta
checkpoints, lazy materialization, working-set prefetch, and bounded chain
flattening.

Independent machine preparation and creation must remain parallel while
dependency waves preserve declared creation/start order. Use bounded
concurrency if measurements show one-worker-per-machine does not scale; never
trade away deterministic errors or exact cleanup. The Sentry scenario is
approximately 8 GiB aggregate guest memory and should remain the first scaling
fixture.

## 14. Coding-agent execution rules

Before editing, read the nearest 'AGENTS.md' and inspect the owning module,
callers, tests, schema, and documentation. Keep changes in their owning
module:

~~~
smolworld/src/config.rs    topology and dependency contract
smolworld/src/model.rs     shared world/state identity types
smolworld/src/state.rs     durable material/allocation/state receipts
smolworld/src/switch.rs    framing, FDB, epochs, ports, queued frames
smolworld/src/smolvm.rs    narrow smolvm subprocess/capture boundary
smolworld/src/runtime.rs   lifecycle, checkpoint orchestration, cleanup

niceforge/src/workflow_v2_plan.rs  sealed workflow/world binding
niceforge/src/postgres_store.rs    durable records, fences, transitions
niceforge/src/postgres_dispatch.rs plan-to-store projection
niceforge/src/executor_gateway.rs  executor/step protocol
niceforge/src/postgres_transport.rs closed wire ABI
niceforge/src/smolworld_executor.rs local world adapter
niceforge/src/workflow_runtime.rs  executor-local step semantics
~~~

For every contract change:

1. write the smallest observable regression/acceptance test first;
2. update domain types/schema and all callers deliberately;
3. keep canonical bytes and errors deterministic;
4. test crash, duplicate, stale-lease, and exact-cleanup paths;
5. update this plan and nearby comments where durable meaning lives;
6. commit coherent milestones without pre-commit hooks or remote pushes.

Do not add compatibility aliases for retired Compose/Docker paths. Do not
silently turn a checkpoint artifact into world identity. Do not implement the
large institutional graph before a failed step can be retained, inspected,
and resumed.

## 15. Verification contract

### Smolworld baseline

~~~bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
git diff --check
~~~

Run the real local Redis foundation gate without Docker:

~~~bash
SMOLWORLD_E2E=1 \
SMOLWORLD_SMOLVM=/path/to/smolvm \
SMOLVM_AGENT_ROOTFS=/path/to/agent-rootfs \
SMOLVM_LIB_DIR=/path/to/smolvm/lib \
bash tests/e2e-redis-foundation.sh
~~~

Run the opt-in fork gate when changing the external NIC boundary:

~~~bash
SMOLWORLD_FORK_E2E=1 \
SMOLWORLD_SMOLVM=/path/to/smolvm \
SMOLVM_AGENT_ROOTFS=/path/to/agent-rootfs \
SMOLVM_LIB_DIR=/path/to/smolvm/lib \
python3 tests/e2e_fork_world.py
~~~

### Niceforge baseline

For Niceforge changes, run focused contract tests first, then its normal
formatter/test/clippy baseline. PostgreSQL tests use a dedicated empty
database. Add process-boundary fault tests at provisioning, active-step,
capture, commit, failed-world retention, restore, shell, retry, and cleanup
boundaries.

The audit oracle must independently reject a transition/checkpoint projection
that does not match the ordered event and evidence record.

## 16. Explicit non-goals and deferrals

- Docker/Compose execution compatibility, Dockerfiles, registry pulls from
  guests, or a second executor substrate.
- Host networking, NAT, TAP/vmnet, port publishing, DHCP, IPv6, guest Internet
  egress, or unauthenticated guest SSH daemons.
- Generic service health checks, restart policies, log aggregation, or Compose
  readiness semantics in Smolworld.
- Making VM snapshots the semantic world model.
- Multi-world merge semantics. Reconciliation remains an ordinary N-parent
  transition.
- Distributed scheduling, GPU fabrics, multiple repositories, or a universal
  state codec before the single-host world trajectory is correct.
- Persistent agent personalities or agent-to-agent chat as foundational state.
- Sophisticated reputation markets or automatic kernel self-modification before
  offices, evidence, and authority are proven.
- Retaining failed worlds, checkpoint restore, and shell access in the first
  success-path executor cutover; these become mandatory only at Gates 6–8.

The trusted kernel should remain small and stable. Control-plane policies,
office structure, harnesses, evaluators, propagation, and schedulers may be
forked and improved inside worlds, but promotion of a replacement kernel must
be a separately governed shadow-evaluation and migration operation.
