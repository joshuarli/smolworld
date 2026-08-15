use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

/// On-disk schema for the published world checkpoint receipt. A version bump
/// is an intentional compatibility boundary: restore and release reject
/// receipts whose integrity contract they cannot prove.
pub(crate) const WORLD_CHECKPOINT_RECEIPT_VERSION: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorldConfig {
    pub(crate) name: String,
    pub(crate) network: NetworkConfig,
    pub(crate) machines: BTreeMap<String, MachineConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetworkConfig {
    pub(crate) subnet: [u8; 4],
    pub(crate) gateway: Ipv4Addr,
    pub(crate) dns: Ipv4Addr,
    pub(crate) domain: String,
    /// Attach smolvm's existing host-side NAT as a second guest NIC.
    pub(crate) egress: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineConfig {
    /// Path to the machine's Smolfile. The contents belong to smolvm; the
    /// world parser deliberately treats this as an opaque reference.
    pub(crate) smolfile: PathBuf,
    pub(crate) depends_on: Vec<String>,
    pub(crate) seed_files: Vec<SeedFile>,
}

/// A sealed host-file to guest-file copy declaration. The source is a host
/// path that is checked and materialized by the host-side preparation path;
/// smolworld never turns it into a live guest mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeedFile {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
    /// Unix permission bits, represented as the numeric value of the exact
    /// four-digit octal mode in the `.smolworld` file.
    pub(crate) mode: u32,
}

/// The host inputs for one smolvm external-world launch. Keeping this
/// separate from `MachineConfig` makes the configuration-to-host boundary
/// explicit: static network identity and the opaque Smolfile reference are
/// passed to smolvm only after configuration validation.
pub(crate) struct MachineLaunch<'a> {
    pub(crate) assignment: &'a Assignment,
    pub(crate) socket: &'a Path,
    pub(crate) smolfile: &'a Path,
    pub(crate) seed_files: &'a [SeedFile],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assignment {
    pub(crate) ip: Ipv4Addr,
    pub(crate) mac: [u8; 6],
    pub(crate) smolvm_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Stable address/MAC allocation for one configured world. This intentionally
/// does not describe a checkpointable workload state: a future `WorldState`
/// will be an immutable cross-machine state manifest.
pub(crate) struct WorldAllocationState {
    pub(crate) seed: u64,
    pub(crate) assignments: BTreeMap<String, Assignment>,
}

/// Immutable receipt for one coordinated world checkpoint. It records the
/// world declaration/material identities separately from the stable allocation
/// tuple so restore can reject a checkpoint whose source configuration drifted
/// without mistaking allocation state for the guest workload state itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorldCheckpointReceipt {
    /// Receipt schema. The version changes when the world-to-machine
    /// integrity contract changes; older receipts are intentionally not
    /// accepted by restore or release.
    pub(crate) schema_version: u8,
    pub(crate) world_name: String,
    pub(crate) config_digest: String,
    pub(crate) material_lock_digest: String,
    pub(crate) allocation: WorldAllocationState,
    /// One BLAKE3 digest for each opaque smolvm machine receipt. Smolworld
    /// does not duplicate smolvm's RAM/disk verifier; it binds the exact
    /// machine receipt that smolvm will verify during restore.
    pub(crate) machine_receipts: BTreeMap<String, MachineCheckpointReceipt>,
    /// The forwarding cut that preceded the concurrent VM captures. The
    /// switch has no durable packet queue: everything before this epoch was
    /// applied, and frames arriving after it were deliberately dropped while
    /// guest writers were paused. The receipt preserves that fact rather than
    /// pretending host Unix-stream handles can be restored.
    pub(crate) switch: SwitchCheckpointReceipt,
}

/// Bounded integrity evidence for one smolvm durable machine checkpoint.
///
/// The referenced `smolvm-checkpoint.json` is intentionally opaque to
/// smolworld. Its digest is the narrow ownership bridge: smolworld proves
/// that the published world still contains the same machine receipt, while
/// smolvm remains responsible for interpreting and verifying the receipt's
/// control files, RAM, and disks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineCheckpointReceipt {
    pub(crate) digest: String,
}

/// A canonical description of the ephemeral L2 state at one checkpoint cut.
/// `active_ports` and `learned_macs` are diagnostic/rebind evidence only; a
/// restore always creates fresh listeners and lets the FDB relearn from fresh
/// guest NIC connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwitchCheckpointReceipt {
    pub(crate) epoch: u64,
    pub(crate) queued_frames: u64,
    pub(crate) active_ports: BTreeMap<String, u64>,
    pub(crate) learned_macs: BTreeMap<String, String>,
}

/// Durable lifecycle is kept separate from [`WorldAllocationState`] for
/// compatibility with the original allocation record. Allocation state is
/// constructed directly by the gateway and switch tests, while lifecycle
/// transitions are owned by the runtime supervisor through the state APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleState {
    /// No machine records or runtime sockets are expected to be present.
    Absent,
    /// An `up` operation has claimed the world and may have created nothing,
    /// some machines, or all machines when it is interrupted.
    Starting,
    /// Machine records have been created but the world has not attached all
    /// guest NICs yet.
    Created,
    /// All expected guest NICs have attached to the world switch. This is an
    /// attachment milestone, not a health/readiness claim.
    Attached,
    /// Startup completed and the supervisor owns the world lifecycle.
    Running,
    /// A supervisor has durably declared a checkpoint attempt before freezing
    /// any machine. A crash here is intentionally retained rather than being
    /// treated as stale startup state: the source may be stopped with a
    /// recoverable candidate on disk.
    Capturing,
    /// Every configured machine has a committed durable checkpoint receipt.
    /// The source machine records/disks remain retained for same-lineage
    /// restore, but no switch runtime is expected to be live.
    Captured,
}

impl LifecycleState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Starting => "starting",
            Self::Created => "created",
            Self::Attached => "attached",
            Self::Running => "running",
            Self::Capturing => "capturing",
            Self::Captured => "captured",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "absent" => Some(Self::Absent),
            "starting" => Some(Self::Starting),
            "created" => Some(Self::Created),
            "attached" => Some(Self::Attached),
            "running" => Some(Self::Running),
            "capturing" => Some(Self::Capturing),
            "captured" => Some(Self::Captured),
            _ => None,
        }
    }

    /// Every non-absent state can own external smolvm records or sockets.
    /// After the per-world lock is acquired, these states therefore require
    /// recovery before a new start can safely proceed.
    pub(crate) fn needs_recovery(self) -> bool {
        !matches!(self, Self::Absent | Self::Capturing | Self::Captured)
    }

    /// A captured state intentionally retains its stopped machine records and
    /// allocation receipt. Treating it as stale startup state would destroy the
    /// only same-lineage restore source for the durable checkpoint.
    pub(crate) fn retains_checkpoint_sources(self) -> bool {
        matches!(self, Self::Capturing | Self::Captured)
    }
}

/// The durable lifecycle sidecar for one allocation state. `owner_pid` is
/// diagnostic and identifies the process that last advanced an active world;
/// the lock itself, rather than the PID, is the concurrency authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LifecycleMetadata {
    pub(crate) state: LifecycleState,
    pub(crate) owner_pid: Option<u32>,
    pub(crate) generation: u64,
}

impl Default for LifecycleMetadata {
    fn default() -> Self {
        Self {
            state: LifecycleState::Absent,
            owner_pid: None,
            generation: 0,
        }
    }
}

impl LifecycleMetadata {
    pub(crate) fn new(
        state: LifecycleState,
        owner_pid: Option<u32>,
        generation: u64,
    ) -> std::result::Result<Self, String> {
        if state == LifecycleState::Absent && owner_pid.is_some() {
            return Err("absent lifecycle cannot have an owner PID".into());
        }
        if state != LifecycleState::Absent && owner_pid == Some(0) {
            return Err("lifecycle owner PID must be positive".into());
        }
        Ok(Self {
            state,
            owner_pid,
            generation,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactState {
    Missing,
    Present,
}

/// Read-only evidence used by `up` before it mutates external smolvm state.
/// Callers should acquire [`WorldLock`](crate::state::WorldLock) first. With
/// the lock held, a present runtime directory is leftover state from an
/// earlier owner and must be cleaned before creating a new switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryStatus {
    pub(crate) state_file: ArtifactState,
    pub(crate) lifecycle_file: ArtifactState,
    pub(crate) runtime_dir: ArtifactState,
    pub(crate) lifecycle: LifecycleMetadata,
}

impl RecoveryStatus {
    pub(crate) fn is_recorded_but_absent(&self) -> bool {
        self.state_file == ArtifactState::Present
            && self.lifecycle.state == LifecycleState::Absent
            && self.runtime_dir == ArtifactState::Missing
    }

    pub(crate) fn needs_recovery(&self) -> bool {
        self.lifecycle.state.needs_recovery() || self.runtime_dir == ArtifactState::Present
    }
}

pub(crate) fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

pub(crate) fn gateway_ip(subnet: [u8; 4]) -> Ipv4Addr {
    Ipv4Addr::new(subnet[0], subnet[1], subnet[2], 1)
}

pub(crate) fn gateway_mac() -> [u8; 6] {
    [0x02, 0, 0, 0, 0, 1]
}
