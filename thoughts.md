Yes. And there’s a surprisingly strong piece of evidence that our decomposition is right: **libkrun itself is currently acquiring almost exactly the VMM-side primitive smolworld needs.**

As of August 13, 2026, libkrun has an open snapshot/restore PR for macOS/HVF. It already captures and restores vCPU registers, GIC state, RAM, timers, RTC and virtio transport/device state; restored Linux continues from the saved PC rather than rebooting. The prerequisite pause/resume primitive has already merged. What the PR explicitly defers is essentially the **state-logistics layer**: lazy CoW memory, diff chains, clone reseeding, broader device state, etc. ([GitHub][1])

Even better, the author built a separate research VMM, **ignition**, to prove out those deferred pieces. On Apple Silicon/HVF it already has lazy CoW clone-from-snapshot, dirty-page tracking, immutable incremental snapshot chains, fan-out, multi-vCPU restore and fast reset. It reports roughly 130 ms warm restore versus ~7.8 s cold boot for its disposable-browser demo. ([GitHub][2])

So smolworld suddenly has a very concrete path.

## Infinibranch maps cleanly onto our model

Morph's public state model is:

`Image → Snapshot → Instance → Branch`

where a snapshot is immutable bootable state, an instance is live execution, and branching takes an instance, snapshots it, and creates N descendants from that checkpoint. Morph explicitly positions instances + snapshots as its lower-level interface for RL environments and test-time scaling. ([cloud.morph.so][3])

I think that is **mostly the correct algebra**, although I would rename things slightly for smolworld:

```text
Image
  immutable construction artifact

WorldState
  immutable point in the execution DAG

WorldRun
  mutable execution instantiated from a WorldState

checkpoint(WorldRun) -> WorldState

restore(WorldState) -> WorldRun

fork(WorldState, N) -> WorldRun[N]

fork(WorldRun, N):
    s = checkpoint(run)
    return (s, fork(s, N))
```

That distinction is fundamental.

A **state is not a machine**. It is an immutable value.

A **run is not durable state**. It is a mutable execution cursor over one state lineage.

Morph essentially implements this, although its terminology remains VM-product-oriented. Their `instance.branch(count=3)` literally returns the newly created snapshot plus its clones. ([Morph Cloud][4])

For smolworld, I would make the state/run distinction even stronger than they do.

---

## Where Infinibranch is underspecified for our purposes

The user-facing Infinibranch API is good. I would not copy it wholesale as the underlying smolworld kernel API, though.

Morph exposes snapshot/start/branch/pause/resume/delete, but the public model doesn't make several things that matter tremendously for an RL-world substrate explicit: **lineage, diff structure, identity regeneration, external-resource semantics, atomic multi-machine snapshots, placement/locality, and garbage collection.** Its SDK does expose snapshot chains for cached transformations, which hints that their internal representation is richer than the basic API. ([GitHub][5])

Our state should really be a DAG node:

```text
WorldState {
    id
    parent_state
    machine_state[]
    network_state
    provenance
    artifacts
    identity_policy
}
```

And each `machine_state` would itself be conceptually something like:

```text
MachineState {
    vmstate        # registers, timers, GIC, device state
    memory         # base + page deltas
    disk           # base + block/filesystem deltas
    machine_config # topology required to reconstruct VM
}
```

None of those need to correspond to one physical file.

That is precisely where **state logistics** starts.

---

# The remarkable part: the libkrun work already identifies the correct seam

The current upstream libkrun proposal describes almost exactly this distinction.

The author proposes a `Snapshottable`/vstate seam that serializes vCPU and interrupt-controller state separately from the platform-specific memory paging machinery. It explicitly argues that the serialized state format should be portable while the lazy restore mechanism should be platform-specific: `userfaultfd` + reflinks on Linux, native macOS filesystem/memory mechanisms on HVF. ([GitHub][6])

The current implementation performs a stop-the-world capture roughly as:

```text
request snapshot
      │
      ▼
freeze vCPUs
      │
      ▼
quiesce virtio workers
      │
      ▼
capture device queues/state
      │
      ▼
capture GIC
      │
      ▼
capture vCPU registers/timers
      │
      ▼
capture guest RAM
```

Those details aren't hypothetical; that's how PR #762 currently works. Device workers must be quiesced because freezing the CPUs alone doesn't freeze asynchronous virtio I/O. They stop at descriptor boundaries rather than trying to drain arbitrary host I/O, which could deadlock. ([GitHub][1])

That is good systems design.

And importantly, libkrun treats restored host resources as **rebindings**, rather than serializing host FDs. Snapshot state is FD-free; the caller constructs a compatible new VM configuration, and restore hydrates the execution state into it. ([GitHub][1])

That is exactly the rule smolworld wants:

> **Persist logical machine state. Reconstruct host resources.**

Don't try to checkpoint a Darwin file descriptor, vmnet socket, host file handle, etc. Reinstantiate those and bind them to restored logical devices.

---

# What happens at the macOS kernel/HVF level

Ignition is especially useful because it demonstrates what the actual Mac implementation can look like.

For full execution state, HVF lets the VMM operate the VM and vCPUs; guest physical memory is host memory mapped into the VM. Apple's `hv_vm_map` maps a region of the current process's virtual address space into guest physical memory. ([Apple Developer][7])

Ignition's warm-clone algorithm then uses macOS VM/filesystem behavior instead of needing Linux `userfaultfd`.

Conceptually:

```text
immutable memory base
        │
        ├── CoW mapping → VM A
        ├── CoW mapping → VM B
        ├── CoW mapping → VM C
        └── CoW mapping → VM D
```

The OS page cache provides lazy demand paging. Clean memory is shared; a branch only becomes expensive as it dirties pages. Ignition describes this explicitly as the macOS analogue of Firecracker's lazy snapshot restore. ([Vadika][8])

So if we fork a warm 8 GiB development world into 30 trajectories, **we emphatically should not materialize 240 GiB of RAM**.

The semantics should be:

```text
cost(branch) ≈ metadata + new VMM + pages dirtied by branch
```

not:

```text
cost(branch) ≈ total parent state
```

That is the difference between “snapshot support” and an actual world-fork substrate.

---

# Dirty tracking is the really interesting macOS primitive

This is probably the piece I find most relevant to smolworld.

Linux/KVM has facilities designed for dirty-page tracking. Raw HVF does not expose an equivalent KVM dirty bitmap. Ignition therefore implements one through page permissions.

It write-protects guest RAM with `hv_vm_protect`. The first guest write generates a data-abort exit; the VMM records that 16 KiB Apple Silicon host page as dirty, grants it write permission, and retries the instruction. Each page therefore causes at most one tracking exit per snapshot interval. ([Vadika][8])

So:

```text
base S0

     run
      │
   dirty pages
   {1,7,19,81}
      │
      ▼
delta S1
parent = S0
pages = {1,7,19,81}
```

Then:

```text
S0
├── S1
│   ├── S3
│   └── S4
└── S2
    └── S5
```

These are **immutable execution-state commits**.

Ignition already implements exactly this delta-chain model. RAM pages are deltified; the comparatively small CPU/GIC/device state is stored in full per layer. Restore walks the ancestry and overlays the page deltas. ([Vadika][9])

That is extraordinarily close to the **CoW world DAG** we independently arrived at.

---

# And DMA exposes why this can't just be a cute mmap trick

There's a nasty correctness issue that ignition has already confronted.

A guest CPU isn't the only writer to guest RAM. Virtio devices perform DMA-like writes: network receive buffers, block reads, used-ring updates, GPU data, etc.

If dirty tracking only sees trapped CPU writes:

```text
CPU writes       → tracked ✓
virtio writes    → invisible ✗
```

your incremental snapshot eventually corrupts.

Ignition therefore hooks the VMM's device-facing `GuestRam` write path into the **same dirty bitmap**, so CPU and device changes contribute to one coherent dirty set. ([Vadika][10])

That's exactly the sort of obscure state-logistics correctness issue that makes this layer load-bearing.

---

# Disk wants the same algebra

Memory:

```text
immutable memory base
      +
dirty-page delta chain
```

Disk:

```text
immutable disk base
      +
CoW block/file overlays
```

On APFS this is particularly attractive because clone operations can give us cheap CoW backing. Ignition's snapshot store already organizes immutable snapshots separately from per-instance CoW materializations. ([Vadika][8])

And this exposes an important invariant:

> **A world checkpoint must be coherent across RAM, disk and device state.**

Ignition has a warning about in-place RAM rollback while allowing a writable disk to continue forward: the guest's page cache/journal/inode state may then describe a different disk timeline, causing filesystem corruption. ([Vadika][10])

So smolworld should never conceptually support:

```text
restore(memory = S42)
keep(disk = S49)
```

unless the environment explicitly declares the disk outside transactional world state.

---

# Networking is where smolworld becomes more than libkrun

This is another reason the separation is useful.

libkrun should know about **one VM's virtio-net device state**.

smolworld should know about:

```text
world topology
    ├── vm A
    ├── vm B
    ├── vm C
    ├── switch
    ├── NAT boundary
    ├── DNS
    └── link policy
```

For cloned VMs, identity must diverge. Ignition already handles this by constructing a fresh network interface with a new MAC, bouncing the virtual link, and causing the restored guest to reacquire DHCP rather than inheriting the snapshot's network identity. ([Vadika][10])

The libkrun RFC calls out the broader version of this problem too: clones inherit RNG state, machine IDs, SSH keys, etc., and explicitly proposes vmgenid-style reseeding. That work is still deferred. ([GitHub][6])

So a world-state API needs to distinguish:

```text
captured state
    RAM
    disk
    process state
    kernel state
    virtual device state

rebound state
    host file descriptors
    network interfaces
    host sockets
    endpoints

regenerated state
    VM identity
    entropy generation
    MAC address
    ephemeral credentials

external state
    Internet
    external APIs
    wall clock
```

That policy is arguably as important as the snapshot implementation itself.

---

# smolworld's "kernel" should therefore be very small

I would make these the fundamental operations:

```text
checkpoint(run) -> state

spawn(state) -> run

fork(run | state, n) -> (state, run[n])

reset(run, state)

destroy(run)

pin(state)
unpin(state)
gc()
```

Then another introspection surface:

```text
parent(state)
children(state)
diff(a, b)
ancestry(state)
materialization(state)
working_set(run)
```

And crucially, I **would not** initially add `merge()`.

A filesystem has comprehensible merge semantics. Arbitrary running machine state absolutely does not. World DAGs are genealogical, not Git branches in the conventional mergeable sense.

The higher-level agent system chooses a winning descendant by its evaluation and advances its logical pointer:

```text
candidate A ── score .31
candidate B ── score .92  ← select
candidate C ── score .18

current = state(B)
GC(A, C)
```

No machine-state merge required.

---

# State logistics is a separate subsystem

This is where I would now draw the smolworld architecture more sharply:

```text
                    SMOLWORLD
┌─────────────────────────────────────────────────┐
│ World API / DAG                                 │
│ state, fork, checkpoint, reset, lineage, GC     │
├─────────────────────────────────────────────────┤
│ State Logistics                                 │
│                                                 │
│ CAS / manifests                                 │
│ memory bases + deltas                           │
│ disk bases + overlays                           │
│ lazy materialization                            │
│ working-set prefetch                            │
│ deduplication                                   │
│ reference counting / GC                         │
│ locality / placement                            │
│ identity reseeding                              │
├─────────────────────────────────────────────────┤
│ World runtime                                   │
│ networking / topology / lifecycle               │
├─────────────────────────────────────────────────┤
│ SmolVM                                          │
│ VM lifecycle + guest interaction                │
├─────────────────────────────────────────────────┤
│ libkrun                                         │
│ VMM + vCPU/device snapshot seam                 │
├─────────────────────────────────────────────────┤
│ HVF / macOS                                     │
└─────────────────────────────────────────────────┘
```

**libkrun shouldn't become the state-logistics system.** Its maintainer philosophy explicitly says it does not aim to become a generic VMM; the snapshot proposal itself keeps the platform-specific paging mechanism below a stable state seam. ([GitHub][11])

And **SmolVM shouldn't necessarily become it either**. SmolVM currently gives a sandbox abstraction, local SQLite lifecycle metadata and backend-neutral snapshot APIs, but its libkrun backend explicitly still says pause/resume/snapshot aren't supported because the upstream work hasn't landed. ([Celesto AI][12])

smolworld is exactly the right level to own the DAG and logistics.

---

## This changes what I think our next milestone should be

Previously I thought the userspace `libnetwork` analogue was the obvious first milestone.

I still think networking is useful, but **I would now seriously consider world-state fork the more strategically important vertical slice**, because somebody has unexpectedly done a large fraction of the gnarly HVF research for us.

The clean PoC would be:

```text
smolworld run world.smolworld
        │
        ▼
2-3 communicating libkrun VMs
        │
        ▼
smolworld checkpoint world-0
        │
        ▼
smolworld fork world-0 --count 4
        │
        ├── world-0/a
        ├── world-0/b
        ├── world-0/c
        └── world-0/d
```

Each branch resumes **live processes from the checkpoint**, gets independent RAM/disk modifications and network identity, while clean memory and storage blocks remain physically shared.

That would be a qualitatively different system from Docker Compose.

And the timing is almost absurdly good: **the exact missing libkrun primitive is under active upstream review right now, while its author has a working macOS reference implementation that already demonstrates the richer clone/diff machinery we'd need.** ([GitHub][1])

I would study `ignition` extremely closely before writing much state machinery. At this point it may be the single most relevant open-source project to smolworld's world-fork side—not because we should replace libkrun with it, but because it is effectively an **executable design document for the state-logistics substrate libkrun is currently missing**. ([Vadika][8])

[1]: https://github.com/libkrun/libkrun/pull/762 "vmm,devices: VM snapshot/restore (HVF) by vadika · Pull Request #762 · libkrun/libkrun · GitHub"
[2]: https://github.com/vadika/ignition "GitHub - vadika/ignition: A research microVM for macOS on Apple Silicon (Hypervisor.framework) — architecturally modeled on AWS Firecracker, not a port. Boots Linux, virtio, SMP, snapshot/restore. · GitHub"
[3]: https://cloud.morph.so/docs/concepts/mental-model "Mental Model | morphcloud Docs"
[4]: https://cloud.morph.so/docs/documentation/instances/branch "Branch | morphcloud Docs"
[5]: https://github.com/morph-labs/morph-python-sdk?utm_source=chatgpt.com "Morph Cloud Python SDK"
[6]: https://github.com/libkrun/libkrun/issues/748 "Feature propsal/RFC: Snapshot / restore for libkrun · Issue #748 · libkrun/libkrun · GitHub"
[7]: https://developer.apple.com/documentation/hypervisor/hv_vm_map%28_%3A_%3A_%3A_%3A%29?language=objc "hv_vm_map(_:_:_:_:) | Apple Developer Documentation"
[8]: https://vadika.github.io/ignition/concepts/clone-primitive.html "The clone primitive - ignition"
[9]: https://vadika.github.io/ignition/features/diff-snapshots.html "Diff / incremental snapshots - ignition"
[10]: https://vadika.github.io/ignition/features/snapshot-restore.html "Snapshot & restore - ignition"
[11]: https://github.com/containers/libkrun "GitHub - libkrun/libkrun: A dynamic library providing Virtualization-based process isolation capabilities · GitHub"
[12]: https://docs.celesto.ai/smolvm/concepts/backends?utm_source=chatgpt.com "Firecracker, QEMU, and libkrun backends"
