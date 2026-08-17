use super::{ensure_private_dir, fnv1a, WorldPaths};
use crate::config::validate_label;
use crate::model::{format_mac, gateway_mac, Assignment, WorldAllocationState, WorldConfig};
use crate::Result;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_VERSION: u8 = 2;

pub(crate) fn load_allocation_state(path: &Path) -> Result<Option<WorldAllocationState>> {
    load_state_version(path, STATE_VERSION, "world state")
}

fn load_state_version(
    path: &Path,
    expected_version: u8,
    label: &str,
) -> Result<Option<WorldAllocationState>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let mut version = None;
    let mut seed = None;
    let mut assignments = BTreeMap::new();
    let mut assigned_ips = HashSet::new();
    let mut assigned_macs = HashSet::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["version", value] => {
                let parsed = value
                    .parse::<u8>()
                    .map_err(|_| format!("{label} has invalid version"))?;
                if version.replace(parsed).is_some() {
                    return Err(format!("{label} repeats version"));
                }
            }
            ["seed", value] => {
                let parsed = u64::from_str_radix(value, 16)
                    .map_err(|_| format!("{label} has invalid seed"))?;
                if seed.replace(parsed).is_some() {
                    return Err(format!("{label} repeats seed"));
                }
            }
            ["machine", name, ip, mac, smolvm_name] => {
                validate_label(name)
                    .map_err(|reason| format!("{label} machine '{name}': {reason}"))?;
                let ip = ip
                    .parse()
                    .map_err(|_| format!("{label} machine '{name}' has invalid IP"))?;
                let mac = parse_mac(mac)
                    .map_err(|reason| format!("{label} machine '{name}': {reason}"))?;
                if smolvm_name.is_empty()
                    || !smolvm_name.starts_with("smw-")
                    || smolvm_name.contains(['\t', '\r', '\n'])
                    || mac[0] & 3 != 2
                    || !assigned_ips.insert(ip)
                    || !assigned_macs.insert(mac)
                {
                    return Err(format!(
                        "{label} machine '{name}' has an unsafe or repeated allocation"
                    ));
                }
                let previous = assignments.insert(
                    (*name).to_string(),
                    Assignment {
                        ip,
                        mac,
                        smolvm_name: (*smolvm_name).to_string(),
                    },
                );
                if previous.is_some() {
                    return Err(format!("{label} repeats machine '{name}'"));
                }
            }
            _ => return Err(format!("{label} contains an unknown or malformed line")),
        }
    }
    if version != Some(expected_version) {
        return Err(format!("{label} format is not version {expected_version}"));
    }
    Ok(Some(WorldAllocationState {
        seed: seed.ok_or_else(|| format!("{label} is missing seed"))?,
        assignments,
    }))
}

pub(crate) fn write_allocation_state(
    paths: &WorldPaths,
    state: &WorldAllocationState,
) -> Result<()> {
    write_state_at(
        &paths.state_dir,
        paths.state_file.clone(),
        state,
        STATE_VERSION,
        "world state",
    )
}

fn write_state_at(
    state_dir: &Path,
    state_file: PathBuf,
    state: &WorldAllocationState,
    version: u8,
    label: &str,
) -> Result<()> {
    ensure_private_dir(state_dir)?;
    let temporary = state_dir.join(format!("state.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("create {}: {error}", temporary.display()))?;
    writeln!(file, "version\t{version}").map_err(|error| format!("write {label}: {error}"))?;
    writeln!(file, "seed\t{:016x}", state.seed)
        .map_err(|error| format!("write {label}: {error}"))?;
    for (name, assignment) in &state.assignments {
        writeln!(
            file,
            "machine\t{name}\t{}\t{}\t{}",
            assignment.ip,
            format_mac(assignment.mac),
            assignment.smolvm_name
        )
        .map_err(|error| format!("write {label}: {error}"))?;
    }
    file.sync_all()
        .map_err(|error| format!("sync {label}: {error}"))?;
    fs::rename(&temporary, &state_file)
        .map_err(|error| format!("rename {}: {error}", state_file.display()))?;
    Ok(())
}

/// Allocate world identities only from the world record and paths. This keeps
/// stable address/MAC invariants while using only the current state boundary.
pub(crate) fn allocate_allocation_state(
    previous: Option<WorldAllocationState>,
    config: &WorldConfig,
    paths: &WorldPaths,
) -> Result<WorldAllocationState> {
    let previous = previous.unwrap_or_else(|| WorldAllocationState {
        seed: new_seed(paths),
        assignments: BTreeMap::new(),
    });
    let mut assigned_ips = HashSet::new();
    let mut assigned_macs = HashSet::new();
    assigned_ips.insert(config.network.gateway);
    assigned_macs.insert(gateway_mac());
    let mut assignments = BTreeMap::new();

    for name in config.machines.keys() {
        if let Some(assignment) = previous.assignments.get(name) {
            if valid_existing_assignment(
                assignment,
                config.network.subnet,
                &assigned_ips,
                &assigned_macs,
            ) {
                assigned_ips.insert(assignment.ip);
                assigned_macs.insert(assignment.mac);
                assignments.insert(name.clone(), assignment.clone());
            }
        }
    }
    for name in config.machines.keys() {
        if assignments.contains_key(name) {
            continue;
        }
        let assignment = allocate_assignment(
            previous.seed,
            paths.hash,
            name,
            config.network.subnet,
            &assigned_ips,
            &assigned_macs,
        )?;
        assigned_ips.insert(assignment.ip);
        assigned_macs.insert(assignment.mac);
        assignments.insert(name.clone(), assignment);
    }
    Ok(WorldAllocationState {
        seed: previous.seed,
        assignments,
    })
}

pub(crate) fn new_seed(paths: &WorldPaths) -> u64 {
    new_seed_for_config(&paths.canonical_config)
}

fn new_seed_for_config(canonical_config: &Path) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_le_bytes();
    let mut input = canonical_config.as_os_str().as_encoded_bytes().to_vec();
    input.extend(now);
    input.extend(std::process::id().to_le_bytes());
    super::fnv1a(&input)
}

pub(crate) fn valid_existing_assignment(
    assignment: &Assignment,
    subnet: [u8; 4],
    assigned_ips: &HashSet<Ipv4Addr>,
    assigned_macs: &HashSet<[u8; 6]>,
) -> bool {
    let octets = assignment.ip.octets();
    octets[..3] == subnet[..3]
        && (2..=254).contains(&octets[3])
        && assignment.mac[0] & 3 == 2
        && !assigned_ips.contains(&assignment.ip)
        && !assigned_macs.contains(&assignment.mac)
}

fn allocate_assignment(
    seed: u64,
    world_hash: u64,
    name: &str,
    subnet: [u8; 4],
    assigned_ips: &HashSet<Ipv4Addr>,
    assigned_macs: &HashSet<[u8; 6]>,
) -> Result<Assignment> {
    allocate_assignment_named(
        seed,
        world_hash,
        name,
        subnet,
        assigned_ips,
        assigned_macs,
        "smw",
    )
}

fn allocate_assignment_named(
    seed: u64,
    world_hash: u64,
    name: &str,
    subnet: [u8; 4],
    assigned_ips: &HashSet<Ipv4Addr>,
    assigned_macs: &HashSet<[u8; 6]>,
    name_prefix: &str,
) -> Result<Assignment> {
    let name_hash = seeded_hash(seed, name.as_bytes());
    let start = (name_hash % 253) as usize;
    let ip = (0..253)
        .map(|offset| {
            Ipv4Addr::new(
                subnet[0],
                subnet[1],
                subnet[2],
                (2 + ((start + offset) % 253)) as u8,
            )
        })
        .find(|candidate| !assigned_ips.contains(candidate))
        .ok_or_else(|| "network address pool is exhausted".to_string())?;
    let mac = (0..256_u64)
        .map(|attempt| mac_for(seeded_hash(seed ^ attempt, name.as_bytes())))
        .find(|candidate| !assigned_macs.contains(candidate))
        .ok_or_else(|| "network MAC pool is exhausted".to_string())?;
    Ok(Assignment {
        ip,
        mac,
        smolvm_name: format!(
            "{name_prefix}-{world_hash:012x}-{:012x}",
            fnv1a(name.as_bytes())
        ),
    })
}

pub(crate) fn seeded_hash(seed: u64, value: &[u8]) -> u64 {
    let mut input = seed.to_le_bytes().to_vec();
    input.extend(value);
    fnv1a(&input)
}

pub(crate) fn mac_for(hash: u64) -> [u8; 6] {
    let bytes = hash.to_be_bytes();
    [0x02, bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]
}

pub(crate) fn parse_mac(value: &str) -> Result<[u8; 6]> {
    let octets: Vec<_> = value.split(':').collect();
    if octets.len() != 6 || octets.iter().any(|octet| octet.len() != 2) {
        return Err("invalid MAC address".into());
    }
    let mut mac = [0; 6];
    for (index, octet) in octets.iter().enumerate() {
        mac[index] =
            u8::from_str_radix(octet, 16).map_err(|_| "invalid MAC address".to_string())?;
    }
    Ok(mac)
}
