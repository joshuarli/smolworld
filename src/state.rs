use crate::config::validate_label;
use crate::model::{
    format_mac, gateway_mac, ArtifactState, Assignment, LifecycleMetadata, LifecycleState,
    RecoveryStatus, WorldConfig, WorldPaths, WorldState,
};
use crate::Result;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_VERSION: u8 = 1;
const LIFECYCLE_VERSION: u8 = 1;

/// An advisory, kernel-backed per-world lock.
///
/// The lock file itself is persistent, but its exclusive lock is released by
/// the operating system when the process exits. This is deliberately stronger
/// than a PID marker or `create_new` directory: an interrupted `up` cannot
/// leave a lock that blocks recovery, and two cooperating `up` processes cannot
/// both pass acquisition.
#[derive(Debug)]
pub(crate) struct WorldLock {
    _file: File,
}

impl WorldLock {
    pub(crate) fn acquire(paths: &WorldPaths) -> Result<Self> {
        ensure_private_dir(&paths.state_dir)?;
        let path = paths.lock_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("open world lock {}: {error}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("chmod world lock {}: {error}", path.display()))?;
        if let Err(error) = file.try_lock() {
            match error {
                TryLockError::WouldBlock => {
                    return Err(format!(
                        "world is already locked by another lifecycle operation ({})",
                        path.display()
                    ));
                }
                TryLockError::Error(error) => {
                    return Err(format!("lock {}: {error}", path.display()));
                }
            }
        }
        Ok(Self { _file: file })
    }
}

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

pub(crate) fn load_lifecycle(path: &Path) -> Result<Option<LifecycleMetadata>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let mut version = None;
    let mut state = None;
    let mut owner_pid = None;
    let mut owner_pid_seen = false;
    let mut generation = None;
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["version", value] => {
                if version.is_some() {
                    return Err("lifecycle repeats version".into());
                }
                version = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| "lifecycle has invalid version".to_string())?,
                );
            }
            ["state", value] => {
                if state.is_some() {
                    return Err("lifecycle repeats state".into());
                }
                state = Some(
                    LifecycleState::parse(value)
                        .ok_or_else(|| "lifecycle has invalid state".to_string())?,
                );
            }
            ["owner_pid", "-"] => {
                if owner_pid_seen {
                    return Err("lifecycle repeats owner_pid".into());
                }
                owner_pid_seen = true;
                owner_pid = Some(None);
            }
            ["owner_pid", value] => {
                if owner_pid_seen {
                    return Err("lifecycle repeats owner_pid".into());
                }
                let pid = value
                    .parse::<u32>()
                    .map_err(|_| "lifecycle has invalid owner PID".to_string())?;
                if pid == 0 {
                    return Err("lifecycle owner PID must be positive".into());
                }
                owner_pid_seen = true;
                owner_pid = Some(Some(pid));
            }
            ["generation", value] => {
                if generation.is_some() {
                    return Err("lifecycle repeats generation".into());
                }
                generation = Some(
                    u64::from_str_radix(value, 16)
                        .map_err(|_| "lifecycle has invalid generation".to_string())?,
                );
            }
            _ => return Err("lifecycle contains an unknown or malformed line".into()),
        }
    }
    if version != Some(LIFECYCLE_VERSION) {
        return Err(format!(
            "lifecycle format is not version {LIFECYCLE_VERSION}"
        ));
    }
    let state = state.ok_or_else(|| "lifecycle is missing state".to_string())?;
    if !owner_pid_seen {
        return Err("lifecycle is missing owner PID".into());
    }
    let generation = generation.ok_or_else(|| "lifecycle is missing generation".to_string())?;
    let owner_pid = if owner_pid_seen {
        owner_pid.flatten()
    } else {
        None
    };
    LifecycleMetadata::new(state, owner_pid, generation).map(Some)
}

pub(crate) fn write_lifecycle(paths: &WorldPaths, lifecycle: LifecycleMetadata) -> Result<()> {
    ensure_private_dir(&paths.state_dir)?;
    let temporary = paths
        .state_dir
        .join(format!("lifecycle.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("create {}: {error}", temporary.display()))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod {}: {error}", temporary.display()))?;
    writeln!(file, "version\t{LIFECYCLE_VERSION}")
        .map_err(|error| format!("write lifecycle: {error}"))?;
    writeln!(file, "state\t{}", lifecycle.state.as_str())
        .map_err(|error| format!("write lifecycle: {error}"))?;
    match lifecycle.owner_pid {
        Some(pid) => writeln!(file, "owner_pid\t{pid}"),
        None => writeln!(file, "owner_pid\t-"),
    }
    .map_err(|error| format!("write lifecycle: {error}"))?;
    writeln!(file, "generation\t{:016x}", lifecycle.generation)
        .map_err(|error| format!("write lifecycle: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync lifecycle: {error}"))?;
    fs::rename(&temporary, paths.lifecycle_path())
        .map_err(|error| format!("rename {}: {error}", paths.lifecycle_path().display()))?;
    Ok(())
}

/// Remove only abandoned atomic-write temporary files for this world's state
/// directory. The caller must hold [`WorldLock`] before invoking this helper;
/// the lock makes it safe to remove a temporary left by an interrupted writer
/// without racing another lifecycle operation.
pub(crate) fn remove_stale_temporary_files(paths: &WorldPaths) -> Result<usize> {
    match fs::symlink_metadata(&paths.state_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "state path is not a directory: {}",
                paths.state_dir.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "inspect state directory {}: {error}",
                paths.state_dir.display()
            ));
        }
    }

    let entries = fs::read_dir(&paths.state_dir).map_err(|error| {
        format!(
            "read state directory {}: {error}",
            paths.state_dir.display()
        )
    })?;
    let mut removed = 0;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "read state directory {}: {error}",
                paths.state_dir.display()
            )
        })?;
        let name = entry.file_name();
        if !is_state_temporary_file(&name) {
            continue;
        }
        // Inspect without following symlinks. A replacement race can only
        // make remove_file unlink the replacement directory entry; it cannot
        // remove the symlink target.
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!("inspect {}: {error}", entry.path().display()));
            }
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {error}", entry.path().display())),
        }
    }
    Ok(removed)
}

fn is_state_temporary_file(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    ["state.", "lifecycle."]
        .iter()
        .any(|prefix| name.starts_with(prefix) && name.ends_with(".tmp"))
        && name.len() > ".tmp".len() + 1
}

pub(crate) fn mark_starting(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    let previous = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    let lifecycle = LifecycleMetadata::new(
        LifecycleState::Starting,
        Some(std::process::id()),
        previous.generation.wrapping_add(1),
    )?;
    write_lifecycle(paths, lifecycle)?;
    Ok(lifecycle)
}

pub(crate) fn mark_created(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    transition_lifecycle(paths, LifecycleState::Created, &[LifecycleState::Starting])
}

pub(crate) fn mark_attached(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    transition_lifecycle(
        paths,
        LifecycleState::Attached,
        &[LifecycleState::Created, LifecycleState::Attached],
    )
}

pub(crate) fn mark_running(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    transition_lifecycle(
        paths,
        LifecycleState::Running,
        &[LifecycleState::Attached, LifecycleState::Running],
    )
}

pub(crate) fn mark_absent(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    let previous = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    let lifecycle = LifecycleMetadata::new(LifecycleState::Absent, None, previous.generation)?;
    write_lifecycle(paths, lifecycle)?;
    Ok(lifecycle)
}

fn transition_lifecycle(
    paths: &WorldPaths,
    next: LifecycleState,
    allowed_previous: &[LifecycleState],
) -> Result<LifecycleMetadata> {
    let previous = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    if !allowed_previous.contains(&previous.state) {
        return Err(format!(
            "cannot transition lifecycle from {} to {}",
            previous.state.as_str(),
            next.as_str()
        ));
    }
    let lifecycle = LifecycleMetadata::new(next, previous.owner_pid, previous.generation)?;
    write_lifecycle(paths, lifecycle)?;
    Ok(lifecycle)
}

pub(crate) fn inspect_recovery(paths: &WorldPaths) -> Result<RecoveryStatus> {
    Ok(RecoveryStatus {
        state_file: artifact_state(&paths.state_file)?,
        lifecycle_file: artifact_state(&paths.lifecycle_path())?,
        runtime_dir: artifact_state(&paths.runtime_dir)?,
        lifecycle: load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default(),
    })
}

fn artifact_state(path: &Path) -> Result<ArtifactState> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(ArtifactState::Present),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ArtifactState::Missing),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    }
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
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TemporaryWorld {
        root: PathBuf,
        paths: WorldPaths,
    }

    impl TemporaryWorld {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!(
                "smolworld-state-test-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            let state_dir = root.join("state");
            Self {
                paths: WorldPaths {
                    canonical_config: root.join("demo/.smolworld"),
                    config_dir: root.join("demo"),
                    hash: 42,
                    state_file: state_dir.join("state"),
                    state_dir,
                    runtime_dir: root.join("runtime"),
                },
                root,
            }
        }
    }

    impl Drop for TemporaryWorld {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn config() -> WorldConfig {
        parse_config(
            r#"world:
  name: demo
network:
  subnet: 10.89.0.0/24
machines:
  redis:
    image: ./redis.tar
  client:
    image: ./redis.tar
    depends_on: [redis]
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

    #[test]
    fn legacy_state_defaults_to_recorded_but_absent() {
        let world = TemporaryWorld::new();
        let state = WorldState {
            seed: 7,
            assignments: BTreeMap::new(),
        };
        write_state(&world.paths, &state).unwrap();

        assert_eq!(load_state(&world.paths.state_file).unwrap(), Some(state));
        assert_eq!(load_lifecycle(&world.paths.lifecycle_path()).unwrap(), None);

        let recovery = inspect_recovery(&world.paths).unwrap();
        assert!(recovery.is_recorded_but_absent());
        assert!(!recovery.needs_recovery());
    }

    #[test]
    fn lifecycle_transitions_round_trip_and_recover_interrupted_start() {
        let world = TemporaryWorld::new();
        write_state(
            &world.paths,
            &WorldState {
                seed: 7,
                assignments: BTreeMap::new(),
            },
        )
        .unwrap();

        let starting = mark_starting(&world.paths).unwrap();
        assert_eq!(starting.state, LifecycleState::Starting);
        assert_eq!(starting.owner_pid, Some(std::process::id()));
        assert_eq!(starting.generation, 1);
        assert!(inspect_recovery(&world.paths).unwrap().needs_recovery());

        let created = mark_created(&world.paths).unwrap();
        assert_eq!(created.state, LifecycleState::Created);
        let attached = mark_attached(&world.paths).unwrap();
        assert_eq!(attached.state, LifecycleState::Attached);
        let running = mark_running(&world.paths).unwrap();
        assert_eq!(running.state, LifecycleState::Running);
        assert!(inspect_recovery(&world.paths).unwrap().needs_recovery());

        let absent = mark_absent(&world.paths).unwrap();
        assert_eq!(absent.state, LifecycleState::Absent);
        assert_eq!(absent.generation, starting.generation);
        let recovery = inspect_recovery(&world.paths).unwrap();
        assert!(recovery.is_recorded_but_absent());
        assert!(!recovery.needs_recovery());
    }

    #[test]
    fn lifecycle_transitions_reject_skipped_startup_milestones() {
        let world = TemporaryWorld::new();
        assert!(mark_created(&world.paths).is_err());
        mark_starting(&world.paths).unwrap();
        assert!(mark_running(&world.paths).is_err());
    }

    #[test]
    fn world_lock_is_exclusive_and_releases_after_drop() {
        let world = TemporaryWorld::new();
        let first = WorldLock::acquire(&world.paths).unwrap();
        let second = WorldLock::acquire(&world.paths);
        assert!(second.is_err());

        drop(first);
        let _third = WorldLock::acquire(&world.paths).unwrap();
    }

    #[test]
    fn stale_temporary_cleanup_is_narrow_and_does_not_follow_symlinks() {
        let world = TemporaryWorld::new();
        ensure_private_dir(&world.paths.state_dir).unwrap();
        fs::write(world.paths.state_dir.join("state.123.tmp"), b"old state").unwrap();
        fs::write(
            world.paths.state_dir.join("lifecycle.123.tmp"),
            b"old lifecycle",
        )
        .unwrap();
        fs::write(world.paths.state_dir.join("state"), b"keep").unwrap();
        fs::write(world.paths.state_dir.join("state.123"), b"keep").unwrap();
        fs::write(world.paths.state_dir.join("lifecycle.123.tmp.bak"), b"keep").unwrap();
        fs::create_dir(world.paths.state_dir.join("state.456.tmp")).unwrap();

        let sibling = world.root.join("sibling");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("state.789.tmp"), b"keep").unwrap();

        let target = world.root.join("outside-target");
        fs::write(&target, b"do not remove").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, world.paths.state_dir.join("state.link.tmp")).unwrap();

        assert_eq!(remove_stale_temporary_files(&world.paths).unwrap(), 2);
        assert!(!world.paths.state_dir.join("state.123.tmp").exists());
        assert!(!world.paths.state_dir.join("lifecycle.123.tmp").exists());
        assert!(world.paths.state_dir.join("state").exists());
        assert!(world.paths.state_dir.join("state.123").exists());
        assert!(world.paths.state_dir.join("lifecycle.123.tmp.bak").exists());
        assert!(world.paths.state_dir.join("state.456.tmp").is_dir());
        assert!(sibling.join("state.789.tmp").exists());
        assert_eq!(fs::read(&target).unwrap(), b"do not remove");
        #[cfg(unix)]
        assert!(world.paths.state_dir.join("state.link.tmp").exists());
    }
}
