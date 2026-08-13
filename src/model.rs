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
    pub(crate) image: String,
    pub(crate) command: Vec<String>,
    pub(crate) depends_on: Vec<String>,
    pub(crate) resources: MachineResources,
}

/// The deliberately small default footprint for a local service VM. These are
/// world defaults, not application-specific values: an individual machine can
/// override any field in `.smolworld`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MachineResources {
    pub(crate) cpus: u8,
    pub(crate) memory_mib: u32,
    pub(crate) storage_gib: u64,
    pub(crate) overlay_gib: u64,
}

impl Default for MachineResources {
    fn default() -> Self {
        Self {
            cpus: 1,
            memory_mib: 256,
            storage_gib: 1,
            overlay_gib: 1,
        }
    }
}

/// The already-resolved host inputs for one smolvm `machine create` call.
/// Keeping this separate from `MachineConfig` makes the configuration-to-host
/// boundary explicit: paths and static network identity have been validated
/// before a subprocess is started.
pub(crate) struct MachineLaunch<'a> {
    pub(crate) assignment: &'a Assignment,
    pub(crate) socket: &'a Path,
    pub(crate) image: &'a Path,
    pub(crate) command: &'a [String],
    pub(crate) resources: MachineResources,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assignment {
    pub(crate) ip: Ipv4Addr,
    pub(crate) mac: [u8; 6],
    pub(crate) smolvm_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct WorldState {
    pub(crate) seed: u64,
    pub(crate) assignments: BTreeMap<String, Assignment>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorldPaths {
    pub(crate) canonical_config: PathBuf,
    pub(crate) config_dir: PathBuf,
    pub(crate) hash: u64,
    pub(crate) state_dir: PathBuf,
    pub(crate) state_file: PathBuf,
    pub(crate) runtime_dir: PathBuf,
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
