use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

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
pub(crate) struct WorldState {
    pub(crate) seed: u64,
    pub(crate) assignments: BTreeMap<String, Assignment>,
}

/// Durable lifecycle is kept separate from [`WorldState`] for compatibility
/// with the original allocation record. `WorldState` is constructed directly
/// by the gateway and switch tests, while lifecycle transitions are owned by
/// the runtime supervisor through the state APIs.
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
}

impl LifecycleState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Starting => "starting",
            Self::Created => "created",
            Self::Attached => "attached",
            Self::Running => "running",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "absent" => Some(Self::Absent),
            "starting" => Some(Self::Starting),
            "created" => Some(Self::Created),
            "attached" => Some(Self::Attached),
            "running" => Some(Self::Running),
            _ => None,
        }
    }

    /// Every non-absent state can own external smolvm records or sockets.
    /// After the per-world lock is acquired, these states therefore require
    /// recovery before a new start can safely proceed.
    pub(crate) fn needs_recovery(self) -> bool {
        !matches!(self, Self::Absent)
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
