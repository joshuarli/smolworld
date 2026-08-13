use crate::config::validate_label;
use crate::model::{format_mac, gateway_mac, Assignment, WorldConfig, WorldPaths, WorldState};
use crate::Result;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_VERSION: u8 = 1;

pub(crate) fn world_paths(config_path: &Path) -> Result<WorldPaths> {
    let canonical_config = fs::canonicalize(config_path)
        .map_err(|error| format!("canonicalize {}: {error}", config_path.display()))?;
    let config_dir = canonical_config
        .parent()
        .ok_or_else(|| "configuration path has no parent directory".to_string())?
        .to_path_buf();
    let hash = fnv1a(canonical_config.as_os_str().as_encoded_bytes());
    let home = env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;
    let state_dir = PathBuf::from(home)
        .join(".smolworld")
        .join(format!("world-{hash:012x}"));
    Ok(WorldPaths {
        canonical_config,
        config_dir,
        hash,
        state_file: state_dir.join("state"),
        state_dir,
        runtime_dir: PathBuf::from("/tmp").join(format!("smw-{hash:012x}")),
    })
}

pub(crate) fn fnv1a(input: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn load_state(path: &Path) -> Result<Option<WorldState>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let mut version = None;
    let mut seed = None;
    let mut assignments = BTreeMap::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["version", value] => {
                version = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| "state has invalid version".to_string())?,
                )
            }
            ["seed", value] => {
                seed = Some(
                    u64::from_str_radix(value, 16)
                        .map_err(|_| "state has invalid seed".to_string())?,
                )
            }
            ["machine", name, ip, mac, smolvm_name] => {
                validate_label(name)
                    .map_err(|reason| format!("state machine '{name}': {reason}"))?;
                let previous = assignments.insert(
                    (*name).to_string(),
                    Assignment {
                        ip: ip
                            .parse()
                            .map_err(|_| format!("state machine '{name}' has invalid IP"))?,
                        mac: parse_mac(mac)
                            .map_err(|reason| format!("state machine '{name}': {reason}"))?,
                        smolvm_name: (*smolvm_name).to_string(),
                    },
                );
                if previous.is_some() {
                    return Err(format!("state repeats machine '{name}'"));
                }
            }
            _ => return Err("state contains an unknown or malformed line".into()),
        }
    }
    if version != Some(STATE_VERSION) {
        return Err(format!("state format is not version {STATE_VERSION}"));
    }
    Ok(Some(WorldState {
        seed: seed.ok_or_else(|| "state is missing seed".to_string())?,
        assignments,
    }))
}

pub(crate) fn write_state(paths: &WorldPaths, state: &WorldState) -> Result<()> {
    ensure_private_dir(&paths.state_dir)?;
    let temporary = paths
        .state_dir
        .join(format!("state.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("create {}: {error}", temporary.display()))?;
    writeln!(file, "version\t{STATE_VERSION}").map_err(|error| format!("write state: {error}"))?;
    writeln!(file, "seed\t{:016x}", state.seed).map_err(|error| format!("write state: {error}"))?;
    for (name, assignment) in &state.assignments {
        writeln!(
            file,
            "machine\t{name}\t{}\t{}\t{}",
            assignment.ip,
            format_mac(assignment.mac),
            assignment.smolvm_name
        )
        .map_err(|error| format!("write state: {error}"))?;
    }
    file.sync_all()
        .map_err(|error| format!("sync state: {error}"))?;
    fs::rename(&temporary, &paths.state_file)
        .map_err(|error| format!("rename {}: {error}", paths.state_file.display()))?;
    Ok(())
}

pub(crate) fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("chmod {}: {error}", path.display()))
}

pub(crate) fn allocate_state(
    previous: Option<WorldState>,
    config: &WorldConfig,
    paths: &WorldPaths,
) -> Result<WorldState> {
    let previous = previous.unwrap_or_else(|| WorldState {
        seed: new_seed(paths),
        assignments: BTreeMap::new(),
    });
    let mut assigned_ips = HashSet::new();
    let mut assigned_macs = HashSet::new();
    let gateway_ip = config.network.gateway;
    let gateway_mac = gateway_mac();
    assigned_ips.insert(gateway_ip);
    assigned_macs.insert(gateway_mac);
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
    Ok(WorldState {
        seed: previous.seed,
        assignments,
    })
}

pub(crate) fn new_seed(paths: &WorldPaths) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_le_bytes();
    let mut input = paths
        .canonical_config
        .as_os_str()
        .as_encoded_bytes()
        .to_vec();
    input.extend(now);
    input.extend(std::process::id().to_le_bytes());
    fnv1a(&input)
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

pub(crate) fn allocate_assignment(
    seed: u64,
    world_hash: u64,
    name: &str,
    subnet: [u8; 4],
    assigned_ips: &HashSet<Ipv4Addr>,
    assigned_macs: &HashSet<[u8; 6]>,
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
        smolvm_name: format!("smw-{world_hash:012x}-{:012x}", fnv1a(name.as_bytes())),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;
    use std::path::PathBuf;

    fn config() -> WorldConfig {
        parse_config(
            r#"
[world]
name = "demo"

[network]
subnet = "10.89.0.0/24"

[machines.redis]
image = "./redis.tar"

[machines.client]
image = "./redis.tar"
depends_on = ["redis"]
"#,
        )
        .unwrap()
    }

    fn paths() -> WorldPaths {
        WorldPaths {
            canonical_config: PathBuf::from("/tmp/demo/.smolworld"),
            config_dir: PathBuf::from("/tmp/demo"),
            hash: 42,
            state_dir: PathBuf::from("/tmp/unused"),
            state_file: PathBuf::from("/tmp/unused/state"),
            runtime_dir: PathBuf::from("/tmp/unused/runtime"),
        }
    }

    #[test]
    fn allocations_are_stable_and_distinct() {
        let config = config();
        let first = allocate_state(
            Some(WorldState {
                seed: 7,
                assignments: BTreeMap::new(),
            }),
            &config,
            &paths(),
        )
        .unwrap();
        let second = allocate_state(Some(first.clone()), &config, &paths()).unwrap();
        assert_eq!(first.assignments, second.assignments);
        assert_ne!(
            first.assignments["redis"].ip,
            first.assignments["client"].ip
        );
        assert_ne!(
            first.assignments["redis"].mac,
            first.assignments["client"].mac
        );
    }

    #[test]
    fn allocation_reserves_the_configured_gateway_address() {
        let mut config = config();
        config.network.gateway = Ipv4Addr::new(10, 89, 0, 9);
        config.network.dns = config.network.gateway;
        let state = allocate_state(
            Some(WorldState {
                seed: 7,
                assignments: BTreeMap::new(),
            }),
            &config,
            &paths(),
        )
        .unwrap();
        assert!(state
            .assignments
            .values()
            .all(|assignment| assignment.ip != config.network.gateway));
    }
}
