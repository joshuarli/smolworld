use super::{ensure_private_dir, WorldPaths};
use crate::model::{ArtifactState, LifecycleMetadata, LifecycleState, RecoveryStatus};
use crate::Result;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const LIFECYCLE_VERSION: u8 = 2;

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
        Self::acquire_at(paths.lock_path())
    }

    fn acquire_at(path: PathBuf) -> Result<Self> {
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

pub(crate) fn load_lifecycle(path: &Path) -> Result<Option<LifecycleMetadata>> {
    load_lifecycle_version(path, LIFECYCLE_VERSION, "world lifecycle")
}

fn load_lifecycle_version(
    path: &Path,
    expected_version: u8,
    label: &str,
) -> Result<Option<LifecycleMetadata>> {
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
                    return Err(format!("{label} repeats version"));
                }
                version = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| format!("{label} has invalid version"))?,
                );
            }
            ["state", value] => {
                if state.is_some() {
                    return Err(format!("{label} repeats state"));
                }
                state = Some(
                    LifecycleState::parse(value)
                        .ok_or_else(|| format!("{label} has invalid state"))?,
                );
            }
            ["owner_pid", "-"] => {
                if owner_pid_seen {
                    return Err(format!("{label} repeats owner_pid"));
                }
                owner_pid_seen = true;
                owner_pid = Some(None);
            }
            ["owner_pid", value] => {
                if owner_pid_seen {
                    return Err(format!("{label} repeats owner_pid"));
                }
                let pid = value
                    .parse::<u32>()
                    .map_err(|_| format!("{label} has invalid owner PID"))?;
                if pid == 0 {
                    return Err(format!("{label} owner PID must be positive"));
                }
                owner_pid_seen = true;
                owner_pid = Some(Some(pid));
            }
            ["generation", value] => {
                if generation.is_some() {
                    return Err(format!("{label} repeats generation"));
                }
                generation = Some(
                    u64::from_str_radix(value, 16)
                        .map_err(|_| format!("{label} has invalid generation"))?,
                );
            }
            _ => return Err(format!("{label} contains an unknown or malformed line")),
        }
    }
    if version != Some(expected_version) {
        return Err(format!("{label} format is not version {expected_version}"));
    }
    let state = state.ok_or_else(|| format!("{label} is missing state"))?;
    if !owner_pid_seen {
        return Err(format!("{label} is missing owner PID"));
    }
    let generation = generation.ok_or_else(|| format!("{label} is missing generation"))?;
    let owner_pid = if owner_pid_seen {
        owner_pid.flatten()
    } else {
        None
    };
    LifecycleMetadata::new(state, owner_pid, generation).map(Some)
}

pub(crate) fn write_lifecycle(paths: &WorldPaths, lifecycle: LifecycleMetadata) -> Result<()> {
    write_lifecycle_at(
        &paths.state_dir,
        paths.lifecycle_path(),
        lifecycle,
        LIFECYCLE_VERSION,
        "world lifecycle",
    )
}

fn write_lifecycle_at(
    state_dir: &Path,
    lifecycle_path: PathBuf,
    lifecycle: LifecycleMetadata,
    version: u8,
    label: &str,
) -> Result<()> {
    ensure_private_dir(state_dir)?;
    let temporary = state_dir.join(format!("lifecycle.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("create {}: {error}", temporary.display()))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod {}: {error}", temporary.display()))?;
    writeln!(file, "version\t{version}").map_err(|error| format!("write {label}: {error}"))?;
    writeln!(file, "state\t{}", lifecycle.state.as_str())
        .map_err(|error| format!("write {label}: {error}"))?;
    match lifecycle.owner_pid {
        Some(pid) => writeln!(file, "owner_pid\t{pid}"),
        None => writeln!(file, "owner_pid\t-"),
    }
    .map_err(|error| format!("write {label}: {error}"))?;
    writeln!(file, "generation\t{:016x}", lifecycle.generation)
        .map_err(|error| format!("write {label}: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync {label}: {error}"))?;
    fs::rename(&temporary, &lifecycle_path)
        .map_err(|error| format!("rename {}: {error}", lifecycle_path.display()))?;
    Ok(())
}

/// Remove only abandoned atomic-write temporary files for this world's state
/// directory. The caller must hold [`WorldLock`] before invoking this helper;
/// the lock makes it safe to remove a temporary left by an interrupted writer
/// without racing another lifecycle operation.
pub(crate) fn remove_stale_temporary_files(paths: &WorldPaths) -> Result<usize> {
    remove_stale_temporary_files_at(&paths.state_dir, "world state")
}

fn remove_stale_temporary_files_at(state_dir: &Path, label: &str) -> Result<usize> {
    match fs::symlink_metadata(state_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "{label} path is not a directory: {}",
                state_dir.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "inspect {label} directory {}: {error}",
                state_dir.display()
            ));
        }
    }

    let entries = fs::read_dir(state_dir)
        .map_err(|error| format!("read {label} directory {}: {error}", state_dir.display()))?;
    let mut removed = 0;
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("read {label} directory {}: {error}", state_dir.display()))?;
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

/// Publish an intentionally stopped set of created machine records. It has no
/// runtime owner because no switch or listener exists until `start`/`up`
/// launches the supervisor; retaining a stale creator PID would misrepresent
/// that boundary to recovery tooling.
pub(crate) fn mark_created_detached(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    let previous = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    if !matches!(
        previous.state,
        LifecycleState::Starting | LifecycleState::Created
    ) {
        return Err(format!(
            "cannot transition world lifecycle from {} to created",
            previous.state.as_str()
        ));
    }
    let lifecycle = LifecycleMetadata::new(LifecycleState::Created, None, previous.generation)?;
    write_lifecycle(paths, lifecycle)?;
    Ok(lifecycle)
}

pub(crate) fn mark_attached(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    transition_lifecycle(
        paths,
        LifecycleState::Attached,
        // A fresh `up` reaches attachment after machine creation. A durable
        // restore rebinds existing stopped records directly from `Starting`,
        // so treating restore as a synthetic create would blur its ownership
        // boundary and made a valid fresh-handle restore fail after every NIC
        // had already attached.
        &[
            LifecycleState::Starting,
            LifecycleState::Created,
            LifecycleState::Attached,
        ],
    )
}

pub(crate) fn mark_running(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    transition_lifecycle(
        paths,
        LifecycleState::Running,
        &[LifecycleState::Attached, LifecycleState::Running],
    )
}

/// Record a durable capture intent before the supervisor asks any VM to freeze.
/// A process death in this state is not ordinary stale startup: the operator
/// must explicitly restore or release the exact retained source records.
pub(crate) fn mark_capturing(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    transition_lifecycle(
        paths,
        LifecycleState::Capturing,
        &[LifecycleState::Running, LifecycleState::Attached],
    )
}

/// Return a fully rolled-back capture attempt to its live-supervisor state.
/// If this write fails, callers deliberately leave `Capturing` in place so a
/// future ordinary `up` cannot erase uncertain source machines.
pub(crate) fn mark_capture_rolled_back(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    transition_lifecycle(paths, LifecycleState::Running, &[LifecycleState::Capturing])
}

/// Publish a committed durable checkpoint after the supervisor has stopped
/// every source VM. Captured state has no live owner process: the allocation
/// and stopped smolvm records are intentionally retained for a later restore.
pub(crate) fn mark_captured(paths: &WorldPaths) -> Result<LifecycleMetadata> {
    let previous = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    if !matches!(
        previous.state,
        LifecycleState::Starting
            | LifecycleState::Running
            | LifecycleState::Attached
            | LifecycleState::Capturing
    ) {
        return Err(format!(
            "cannot transition world lifecycle from {} to captured",
            previous.state.as_str()
        ));
    }
    let lifecycle = LifecycleMetadata::new(
        LifecycleState::Captured,
        None,
        previous.generation.wrapping_add(1),
    )?;
    write_lifecycle(paths, lifecycle)?;
    Ok(lifecycle)
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
            "cannot transition world lifecycle from {} to {}",
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

pub(crate) fn prepare_runtime_dir(paths: &WorldPaths) -> Result<()> {
    ensure_private_dir(&paths.runtime_dir)
}

/// Remove only the exact runtime directory derived for this world. A
/// missing directory is already clean; a non-directory at that path is an
/// error rather than a reason to broaden cleanup.
pub(crate) fn remove_runtime_dir(paths: &WorldPaths) -> Result<()> {
    match fs::symlink_metadata(&paths.runtime_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::remove_dir_all(&paths.runtime_dir)
                .map_err(|error| format!("remove {}: {error}", paths.runtime_dir.display()))?;
        }
        Ok(_) => {
            return Err(format!(
                "world runtime path is not a directory: {}",
                paths.runtime_dir.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "inspect world runtime path {}: {error}",
                paths.runtime_dir.display()
            ));
        }
    }
    Ok(())
}

fn artifact_state(path: &Path) -> Result<ArtifactState> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(ArtifactState::Present),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ArtifactState::Missing),
        Err(error) => Err(format!("inspect {}: {error}", path.display())),
    }
}
