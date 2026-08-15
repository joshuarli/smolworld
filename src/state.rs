use crate::config::validate_label;
use crate::model::{
    format_mac, gateway_mac, ArtifactState, Assignment, LifecycleMetadata, LifecycleState,
    MachineCheckpointReceipt, RecoveryStatus, SwitchCheckpointReceipt, WorldAllocationState,
    WorldCheckpointReceipt, WorldConfig, WORLD_CHECKPOINT_RECEIPT_VERSION,
};
use crate::Result;
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const V2_STATE_VERSION: u8 = 2;
const V2_LIFECYCLE_VERSION: u8 = 2;
const MATERIAL_LOCK_VERSION: u8 = 5;
const MATERIAL_LOCK_RESOLVER_ABI: &str = "smolvm-external-world/v3";
const WORLD_CHECKPOINT_RECEIPT_NAME: &str = "smolworld-checkpoint";
pub(crate) const MACHINE_CHECKPOINT_RECEIPT_NAME: &str = "smolvm-checkpoint.json";
const MAX_MACHINE_CHECKPOINT_RECEIPT_BYTES: u64 = 1024 * 1024;

/// Paths owned by the v2 materializer.  The explicit `v2` component is an
/// ownership boundary: v2 never reads, adopts, or removes the pre-switch
/// allocation directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V2WorldPaths {
    pub(crate) canonical_config: PathBuf,
    pub(crate) config_dir: PathBuf,
    pub(crate) hash: u64,
    pub(crate) state_dir: PathBuf,
    pub(crate) state_file: PathBuf,
    pub(crate) runtime_dir: PathBuf,
}

impl V2WorldPaths {
    pub(crate) fn lock_path(&self) -> PathBuf {
        self.state_dir.join("world.lock")
    }

    /// The generated, sealed preparation record lives beside the authored
    /// `.smolworld`, not under the runtime allocation namespace.
    pub(crate) fn material_lock_path(&self) -> PathBuf {
        self.config_dir.join(".smolworld.lock")
    }

    pub(crate) fn lifecycle_path(&self) -> PathBuf {
        self.state_dir.join("lifecycle")
    }
}

/// Return the private v2 namespace for a configuration.  This deliberately
/// does not inspect the v1 directory and has no fallback to it.
pub(crate) fn v2_world_paths(config_path: &Path) -> Result<V2WorldPaths> {
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
        .join("v2")
        .join(format!("world-{hash:012x}"));
    let state_file = state_dir.join("state");
    let runtime_dir = PathBuf::from("/tmp").join(format!("smw-v2-{hash:012x}"));
    Ok(V2WorldPaths {
        canonical_config,
        config_dir,
        hash,
        state_dir,
        state_file,
        runtime_dir,
    })
}

/// A content digest observation for one machine's Smolfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V2SmolfileObservation {
    /// Immutable user-authored declaration, relative to the `.smolworld`
    /// directory. This keeps a prepared world valid after Niceforge copies its
    /// sealed source tree into an immutable run snapshot.
    pub(crate) authored_relative_path: PathBuf,
    pub(crate) authored_digest: String,
    /// Local-only Smolfile produced by smolvm's host materializer. It is the
    /// exact machine declaration passed to `smolvm machine create`.
    pub(crate) prepared_path: PathBuf,
    pub(crate) prepared_digest: String,
}

/// A sealed seed-file observation.  The destination and mode are part of the
/// identity, so changing only the guest path or permissions invalidates the
/// material record just as changing the source bytes does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V2SeedObservation {
    pub(crate) machine: String,
    /// Source path relative to the `.smolworld` directory. See
    /// `V2SmolfileObservation::authored_relative_path`.
    pub(crate) source_relative_path: PathBuf,
    pub(crate) destination: String,
    pub(crate) mode: u32,
    pub(crate) digest: String,
}

/// A local image/rootfs material reference resolved by the host-side resolver.
/// Guests consume this local path; they never resolve or pull the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V2ImageMaterial {
    pub(crate) machine: String,
    /// Image kind before preparation: `registry` or `local-archive`.
    pub(crate) source_kind: String,
    /// The original image string in the authored Smolfile.
    pub(crate) source_reference: String,
    /// Immutable OCI source digest or local archive digest.
    pub(crate) source_digest: String,
    pub(crate) local_path: PathBuf,
    pub(crate) image_digest: String,
}

/// Identity of the world declaration captured by a v2 material record. The
/// digest binds the exact declaration bytes without binding a portable lock to
/// a developer-checkout path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V2WorldIdentity {
    pub(crate) config_digest: String,
}

/// Durable host-side inputs for one v2 world materialization.
///
/// The maps are keyed by the machine's declared name and are serialized in
/// sorted order.  Seed observations remain a vector because a machine may
/// have multiple seed files; serialization sorts that vector by all identity
/// fields.  This is a lock/material record, not a cache: every listed local
/// reference and digest is required for `check` to accept the prepared world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V2MaterialLock {
    pub(crate) resolver_abi: String,
    pub(crate) world: V2WorldIdentity,
    pub(crate) smolfiles: BTreeMap<String, V2SmolfileObservation>,
    pub(crate) seeds: Vec<V2SeedObservation>,
    pub(crate) images: BTreeMap<String, V2ImageMaterial>,
}

impl V2MaterialLock {
    /// Build the identity portion from a canonical configuration path. The
    /// caller supplies the resolver ABI so an ABI change cannot reuse old
    /// prepared material accidentally.
    pub(crate) fn from_config(canonical_config: &Path, resolver_abi: &str) -> Result<Self> {
        let canonical_config = fs::canonicalize(canonical_config)
            .map_err(|error| format!("canonicalize {}: {error}", canonical_config.display()))?;
        let content = fs::read(&canonical_config)
            .map_err(|error| format!("read {}: {error}", canonical_config.display()))?;
        if resolver_abi.is_empty() {
            return Err("material lock resolver ABI cannot be empty".into());
        }
        validate_field(resolver_abi, "resolver ABI")?;
        Ok(Self {
            resolver_abi: resolver_abi.to_string(),
            world: V2WorldIdentity {
                config_digest: digest_bytes(&content),
            },
            smolfiles: BTreeMap::new(),
            seeds: Vec::new(),
            images: BTreeMap::new(),
        })
    }

    pub(crate) fn validate(&self) -> Result<()> {
        validate_field(&self.resolver_abi, "resolver ABI")?;
        if self.resolver_abi.is_empty() {
            return Err("material lock resolver ABI cannot be empty".into());
        }
        validate_blake3_digest(&self.world.config_digest, "config digest")?;
        for (machine, observation) in &self.smolfiles {
            validate_label(machine).map_err(|reason| {
                format!("material lock Smolfile machine '{machine}': {reason}")
            })?;
            validate_relative_path(
                &observation.authored_relative_path,
                "authored Smolfile path",
            )?;
            validate_blake3_digest(&observation.authored_digest, "authored Smolfile digest")?;
            validate_path(&observation.prepared_path, "prepared Smolfile path")?;
            ensure_absolute_path(&observation.prepared_path, "prepared Smolfile path")?;
            validate_blake3_digest(&observation.prepared_digest, "prepared Smolfile digest")?;
        }
        let mut seed_keys = HashSet::new();
        for observation in &self.seeds {
            validate_label(&observation.machine).map_err(|reason| {
                format!(
                    "material lock seed machine '{}': {reason}",
                    observation.machine
                )
            })?;
            validate_relative_path(&observation.source_relative_path, "seed source")?;
            validate_field(&observation.destination, "seed destination")?;
            if !observation.destination.starts_with('/') {
                return Err(format!(
                    "material lock seed destination '{}' is not absolute",
                    observation.destination
                ));
            }
            validate_blake3_digest(&observation.digest, "seed digest")?;
            let key = (
                observation.machine.as_str(),
                observation.source_relative_path.as_os_str(),
                observation.destination.as_str(),
                observation.mode,
            );
            if !seed_keys.insert(key) {
                return Err(format!(
                    "material lock repeats seed '{}' for machine '{}'",
                    observation.destination, observation.machine
                ));
            }
        }
        for (machine, material) in &self.images {
            validate_label(machine)
                .map_err(|reason| format!("material lock image machine '{machine}': {reason}"))?;
            validate_label(&material.machine).map_err(|reason| {
                format!(
                    "material lock image machine '{}': {reason}",
                    material.machine
                )
            })?;
            if machine != &material.machine {
                return Err(format!(
                    "material lock image key '{machine}' does not match machine '{}'",
                    material.machine
                ));
            }
            validate_path(&material.local_path, "local image material")?;
            ensure_absolute_path(&material.local_path, "local image material")?;
            if !matches!(material.source_kind.as_str(), "registry" | "local-archive") {
                return Err(format!(
                    "material lock image '{}' has unsupported source kind '{}'",
                    machine, material.source_kind
                ));
            }
            validate_field(&material.source_reference, "image source reference")?;
            match material.source_kind.as_str() {
                "registry" => {
                    validate_sha256_digest(&material.source_digest, "registry image source digest")?
                }
                "local-archive" => {
                    validate_blake3_digest(&material.source_digest, "local archive source digest")?
                }
                _ => unreachable!("source kind was validated above"),
            }
            validate_blake3_digest(&material.image_digest, "image digest")?;
        }
        Ok(())
    }
}

/// The default ABI used by the external-world materializer.
pub(crate) fn material_lock_resolver_abi() -> &'static str {
    MATERIAL_LOCK_RESOLVER_ABI
}

/// Compute the stable BLAKE3 representation used for world declarations and
/// static host inputs. OCI descriptor verification stays SHA-256 at smolvm's
/// registry boundary; it is never conflated with this local identity.
pub(crate) fn digest_bytes(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

pub(crate) fn digest_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(digest_bytes(&bytes))
}

/// Hash the small receipt that smolvm publishes beside each durable machine
/// checkpoint. This is deliberately bounded: the world receipt anchors the
/// opaque machine receipt, while smolvm owns the potentially large RAM/disk
/// file integrity checks described by that receipt.
pub(crate) fn digest_machine_checkpoint_receipt(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "inspect machine checkpoint receipt {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "machine checkpoint receipt is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_MACHINE_CHECKPOINT_RECEIPT_BYTES {
        return Err(format!(
            "machine checkpoint receipt is larger than {} bytes: {}",
            MAX_MACHINE_CHECKPOINT_RECEIPT_BYTES,
            path.display()
        ));
    }
    let mut file = File::open(path).map_err(|error| {
        format!(
            "open machine checkpoint receipt {}: {error}",
            path.display()
        )
    })?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_MACHINE_CHECKPOINT_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "read machine checkpoint receipt {}: {error}",
                path.display()
            )
        })?;
    if bytes.len() as u64 > MAX_MACHINE_CHECKPOINT_RECEIPT_BYTES {
        return Err(format!(
            "machine checkpoint receipt grew beyond {} bytes: {}",
            MAX_MACHINE_CHECKPOINT_RECEIPT_BYTES,
            path.display()
        ));
    }
    Ok(digest_bytes(&bytes))
}

pub(crate) fn load_v2_material_lock(path: &Path) -> Result<Option<V2MaterialLock>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    parse_v2_material_lock(&content).map(Some)
}

pub(crate) fn write_v2_material_lock(paths: &V2WorldPaths, record: &V2MaterialLock) -> Result<()> {
    record.validate()?;
    // Preparation seals this file beside the authored world declaration.  It
    // must not create or inspect the runtime allocation namespace: `check`
    // uses the same path while remaining entirely read-only with respect to
    // ~/.smolworld.
    let temporary = paths
        .config_dir
        .join(format!(".smolworld.lock.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("create {}: {error}", temporary.display()))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod {}: {error}", temporary.display()))?;
    file.write_all(serialize_v2_material_lock(record).as_bytes())
        .map_err(|error| format!("write material lock: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync material lock: {error}"))?;
    fs::rename(&temporary, paths.material_lock_path())
        .map_err(|error| format!("rename {}: {error}", paths.material_lock_path().display()))?;
    Ok(())
}

fn parse_v2_material_lock(content: &str) -> Result<V2MaterialLock> {
    let mut version = None;
    let mut resolver_abi = None;
    let mut config_digest = None;
    let mut smolfiles = BTreeMap::new();
    let mut seeds = Vec::new();
    let mut images = BTreeMap::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["version", value] => {
                if version.is_some() {
                    return Err("material lock repeats version".into());
                }
                version = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| "material lock has invalid version".to_string())?,
                );
            }
            ["resolver_abi", value] => {
                if resolver_abi.is_some() {
                    return Err("material lock repeats resolver ABI".into());
                }
                resolver_abi = Some((*value).to_string());
            }
            ["world", digest] => {
                if config_digest.is_some() {
                    return Err("material lock repeats world identity".into());
                }
                config_digest = Some((*digest).to_string());
            }
            ["smolfile", machine, authored_relative_path, authored_digest, prepared_path, prepared_digest] =>
            {
                validate_label(machine).map_err(|reason| {
                    format!("material lock Smolfile machine '{machine}': {reason}")
                })?;
                if smolfiles
                    .insert(
                        (*machine).to_string(),
                        V2SmolfileObservation {
                            authored_relative_path: normalize_relative_path(
                                Path::new(authored_relative_path),
                                "authored Smolfile path",
                            )?,
                            authored_digest: (*authored_digest).to_string(),
                            prepared_path: PathBuf::from(prepared_path),
                            prepared_digest: (*prepared_digest).to_string(),
                        },
                    )
                    .is_some()
                {
                    return Err(format!(
                        "material lock repeats Smolfile machine '{machine}'"
                    ));
                }
            }
            ["seed", machine, source_relative_path, destination, mode, digest] => {
                let mode = u32::from_str_radix(mode, 8)
                    .map_err(|_| "material lock has invalid seed mode".to_string())?;
                seeds.push(V2SeedObservation {
                    machine: (*machine).to_string(),
                    source_relative_path: normalize_relative_path(
                        Path::new(source_relative_path),
                        "seed source",
                    )?,
                    destination: (*destination).to_string(),
                    mode,
                    digest: (*digest).to_string(),
                });
            }
            ["image", machine, source_kind, source_reference, source_digest, local_path, digest] => {
                if images
                    .insert(
                        (*machine).to_string(),
                        V2ImageMaterial {
                            machine: (*machine).to_string(),
                            source_kind: (*source_kind).to_string(),
                            source_reference: (*source_reference).to_string(),
                            source_digest: (*source_digest).to_string(),
                            local_path: PathBuf::from(local_path),
                            image_digest: (*digest).to_string(),
                        },
                    )
                    .is_some()
                {
                    return Err(format!("material lock repeats image machine '{machine}'"));
                }
            }
            _ => return Err("material lock contains an unknown or malformed line".into()),
        }
    }
    if version != Some(MATERIAL_LOCK_VERSION) {
        return Err(format!(
            "material lock format is not version {MATERIAL_LOCK_VERSION}"
        ));
    }
    let record = V2MaterialLock {
        resolver_abi: resolver_abi
            .ok_or_else(|| "material lock is missing resolver ABI".to_string())?,
        world: V2WorldIdentity {
            config_digest: config_digest
                .ok_or_else(|| "material lock is missing world identity".to_string())?,
        },
        smolfiles,
        seeds,
        images,
    };
    record.validate()?;
    Ok(record)
}

fn serialize_v2_material_lock(record: &V2MaterialLock) -> String {
    let mut output = String::new();
    output.push_str(&format!("version\t{MATERIAL_LOCK_VERSION}\n"));
    output.push_str(&format!("resolver_abi\t{}\n", record.resolver_abi));
    output.push_str(&format!("world\t{}\n", record.world.config_digest));
    for (machine, observation) in &record.smolfiles {
        output.push_str(&format!(
            "smolfile\t{machine}\t{}\t{}\t{}\t{}\n",
            observation.authored_relative_path.display(),
            observation.authored_digest,
            observation.prepared_path.display(),
            observation.prepared_digest,
        ));
    }
    let mut seeds = record.seeds.clone();
    seeds.sort_by(|left, right| {
        (
            &left.machine,
            &left.source_relative_path,
            &left.destination,
            left.mode,
            &left.digest,
        )
            .cmp(&(
                &right.machine,
                &right.source_relative_path,
                &right.destination,
                right.mode,
                &right.digest,
            ))
    });
    for seed in seeds {
        output.push_str(&format!(
            "seed\t{}\t{}\t{}\t{:o}\t{}\n",
            seed.machine,
            seed.source_relative_path.display(),
            seed.destination,
            seed.mode,
            seed.digest
        ));
    }
    for (machine, material) in &record.images {
        output.push_str(&format!(
            "image\t{machine}\t{}\t{}\t{}\t{}\t{}\n",
            material.source_kind,
            material.source_reference,
            material.source_digest,
            material.local_path.display(),
            material.image_digest
        ));
    }
    output
}

fn validate_field(value: &str, label: &str) -> Result<()> {
    if value.is_empty() {
        return Err(format!("material lock {label} cannot be empty"));
    }
    if value.contains(['\t', '\r', '\n']) {
        return Err(format!(
            "material lock {label} contains a control character"
        ));
    }
    Ok(())
}

fn validate_path(path: &Path, label: &str) -> Result<()> {
    let value = path
        .to_str()
        .ok_or_else(|| format!("material lock {label} is not valid UTF-8"))?;
    validate_field(value, label)
}

fn ensure_absolute_path(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() {
        return Err(format!("material lock {label} must be absolute"));
    }
    Ok(())
}

/// Normalize a lock field that denotes a static input relative to the world
/// file. The material lock is deliberately portable, so no absolute path,
/// `..` traversal, or empty path may be serialized into it.
pub(crate) fn normalize_relative_path(path: &Path, label: &str) -> Result<PathBuf> {
    validate_path(path, label)?;
    if path.is_absolute() {
        return Err(format!("material lock {label} must be relative"));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(component) => normalized.push(component),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(format!(
                    "material lock {label} must not contain an escaping path component"
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(format!("material lock {label} must not be empty"));
    }
    Ok(normalized)
}

fn validate_relative_path(path: &Path, label: &str) -> Result<()> {
    let normalized = normalize_relative_path(path, label)?;
    if normalized != path {
        return Err(format!("material lock {label} must be normalized"));
    }
    Ok(())
}

fn validate_blake3_digest(value: &str, label: &str) -> Result<()> {
    validate_algorithm_digest(value, "blake3", label)
}

fn validate_sha256_digest(value: &str, label: &str) -> Result<()> {
    validate_algorithm_digest(value, "sha256", label)
}

fn validate_algorithm_digest(value: &str, algorithm: &str, label: &str) -> Result<()> {
    validate_field(value, label).and_then(|_| {
        let encoded = value.strip_prefix(&format!("{algorithm}:"));
        match encoded {
            Some(encoded)
                if encoded.len() == 64
                    && encoded
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
            {
                Ok(())
            }
            _ => Err(format!(
                "material lock {label} must be a {algorithm} digest"
            )),
        }
    })
}

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
    pub(crate) fn acquire_v2(paths: &V2WorldPaths) -> Result<Self> {
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

pub(crate) fn fnv1a(input: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn load_v2_lifecycle(path: &Path) -> Result<Option<LifecycleMetadata>> {
    load_lifecycle_version(path, V2_LIFECYCLE_VERSION, "v2 lifecycle")
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

pub(crate) fn write_v2_lifecycle(paths: &V2WorldPaths, lifecycle: LifecycleMetadata) -> Result<()> {
    write_lifecycle_at(
        &paths.state_dir,
        paths.lifecycle_path(),
        lifecycle,
        V2_LIFECYCLE_VERSION,
        "v2 lifecycle",
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
pub(crate) fn remove_v2_stale_temporary_files(paths: &V2WorldPaths) -> Result<usize> {
    remove_stale_temporary_files_at(&paths.state_dir, "v2 state")
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

pub(crate) fn mark_v2_starting(paths: &V2WorldPaths) -> Result<LifecycleMetadata> {
    let previous = load_v2_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    let lifecycle = LifecycleMetadata::new(
        LifecycleState::Starting,
        Some(std::process::id()),
        previous.generation.wrapping_add(1),
    )?;
    write_v2_lifecycle(paths, lifecycle)?;
    Ok(lifecycle)
}

pub(crate) fn mark_v2_created(paths: &V2WorldPaths) -> Result<LifecycleMetadata> {
    transition_v2_lifecycle(paths, LifecycleState::Created, &[LifecycleState::Starting])
}

pub(crate) fn mark_v2_attached(paths: &V2WorldPaths) -> Result<LifecycleMetadata> {
    transition_v2_lifecycle(
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

pub(crate) fn mark_v2_running(paths: &V2WorldPaths) -> Result<LifecycleMetadata> {
    transition_v2_lifecycle(
        paths,
        LifecycleState::Running,
        &[LifecycleState::Attached, LifecycleState::Running],
    )
}

/// Record a durable capture intent before the supervisor asks any VM to freeze.
/// A process death in this state is not ordinary stale startup: the operator
/// must explicitly restore or release the exact retained source records.
pub(crate) fn mark_v2_capturing(paths: &V2WorldPaths) -> Result<LifecycleMetadata> {
    transition_v2_lifecycle(
        paths,
        LifecycleState::Capturing,
        &[LifecycleState::Running, LifecycleState::Attached],
    )
}

/// Return a fully rolled-back capture attempt to its live-supervisor state.
/// If this write fails, callers deliberately leave `Capturing` in place so a
/// future ordinary `up` cannot erase uncertain source machines.
pub(crate) fn mark_v2_capture_rolled_back(paths: &V2WorldPaths) -> Result<LifecycleMetadata> {
    transition_v2_lifecycle(paths, LifecycleState::Running, &[LifecycleState::Capturing])
}

/// Publish a committed durable checkpoint after the supervisor has stopped
/// every source VM. Captured state has no live owner process: the allocation
/// and stopped smolvm records are intentionally retained for a later restore.
pub(crate) fn mark_v2_captured(paths: &V2WorldPaths) -> Result<LifecycleMetadata> {
    let previous = load_v2_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    if !matches!(
        previous.state,
        LifecycleState::Starting
            | LifecycleState::Running
            | LifecycleState::Attached
            | LifecycleState::Capturing
    ) {
        return Err(format!(
            "cannot transition v2 lifecycle from {} to captured",
            previous.state.as_str()
        ));
    }
    let lifecycle = LifecycleMetadata::new(
        LifecycleState::Captured,
        None,
        previous.generation.wrapping_add(1),
    )?;
    write_v2_lifecycle(paths, lifecycle)?;
    Ok(lifecycle)
}

pub(crate) fn mark_v2_absent(paths: &V2WorldPaths) -> Result<LifecycleMetadata> {
    let previous = load_v2_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    let lifecycle = LifecycleMetadata::new(LifecycleState::Absent, None, previous.generation)?;
    write_v2_lifecycle(paths, lifecycle)?;
    Ok(lifecycle)
}

fn transition_v2_lifecycle(
    paths: &V2WorldPaths,
    next: LifecycleState,
    allowed_previous: &[LifecycleState],
) -> Result<LifecycleMetadata> {
    let previous = load_v2_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    if !allowed_previous.contains(&previous.state) {
        return Err(format!(
            "cannot transition v2 lifecycle from {} to {}",
            previous.state.as_str(),
            next.as_str()
        ));
    }
    let lifecycle = LifecycleMetadata::new(next, previous.owner_pid, previous.generation)?;
    write_v2_lifecycle(paths, lifecycle)?;
    Ok(lifecycle)
}

pub(crate) fn inspect_v2_recovery(paths: &V2WorldPaths) -> Result<RecoveryStatus> {
    Ok(RecoveryStatus {
        state_file: artifact_state(&paths.state_file)?,
        lifecycle_file: artifact_state(&paths.lifecycle_path())?,
        runtime_dir: artifact_state(&paths.runtime_dir)?,
        lifecycle: load_v2_lifecycle(&paths.lifecycle_path())?.unwrap_or_default(),
    })
}

pub(crate) fn prepare_v2_runtime_dir(paths: &V2WorldPaths) -> Result<()> {
    ensure_private_dir(&paths.runtime_dir)
}

/// Remove only the exact runtime directory derived for this v2 world.  A
/// missing directory is already clean; a non-directory at that path is an
/// error rather than a reason to broaden cleanup.
pub(crate) fn remove_v2_runtime_dir(paths: &V2WorldPaths) -> Result<()> {
    match fs::symlink_metadata(&paths.runtime_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            fs::remove_dir_all(&paths.runtime_dir)
                .map_err(|error| format!("remove {}: {error}", paths.runtime_dir.display()))?;
        }
        Ok(_) => {
            return Err(format!(
                "v2 runtime path is not a directory: {}",
                paths.runtime_dir.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "inspect v2 runtime path {}: {error}",
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

pub(crate) fn load_v2_allocation_state(path: &Path) -> Result<Option<WorldAllocationState>> {
    load_state_version(path, V2_STATE_VERSION, "v2 state")
}

fn load_state_version(
    path: &Path,
    expected_version: u8,
    label: &str,
) -> Result<Option<WorldAllocationState>> {
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
                        .map_err(|_| format!("{label} has invalid version"))?,
                )
            }
            ["seed", value] => {
                seed = Some(
                    u64::from_str_radix(value, 16)
                        .map_err(|_| format!("{label} has invalid seed"))?,
                )
            }
            ["machine", name, ip, mac, smolvm_name] => {
                validate_label(name)
                    .map_err(|reason| format!("{label} machine '{name}': {reason}"))?;
                let previous = assignments.insert(
                    (*name).to_string(),
                    Assignment {
                        ip: ip
                            .parse()
                            .map_err(|_| format!("{label} machine '{name}' has invalid IP"))?,
                        mac: parse_mac(mac)
                            .map_err(|reason| format!("{label} machine '{name}': {reason}"))?,
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

pub(crate) fn write_v2_allocation_state(
    paths: &V2WorldPaths,
    state: &WorldAllocationState,
) -> Result<()> {
    write_state_at(
        &paths.state_dir,
        paths.state_file.clone(),
        state,
        V2_STATE_VERSION,
        "v2 state",
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

/// Receipt filename inside a published world checkpoint directory.
pub(crate) fn world_checkpoint_receipt_path(root: &Path) -> PathBuf {
    root.join(WORLD_CHECKPOINT_RECEIPT_NAME)
}

/// Atomically publish the world-level receipt after every per-machine
/// checkpoint directory is complete. The receipt intentionally records the
/// stable allocation separately from the guest checkpoint files; it is a
/// verifier and ownership record, never a substitute for RAM/device state.
pub(crate) fn write_world_checkpoint_receipt(
    root: &Path,
    receipt: &WorldCheckpointReceipt,
) -> Result<()> {
    validate_world_checkpoint_receipt(receipt)?;
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("inspect checkpoint root {}: {error}", root.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "checkpoint root is not a real directory: {}",
            root.display()
        ));
    }
    let destination = world_checkpoint_receipt_path(root);
    let temporary = root.join(format!(
        ".{WORLD_CHECKPOINT_RECEIPT_NAME}.{}.tmp",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("create {}: {error}", temporary.display()))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod {}: {error}", temporary.display()))?;
    file.write_all(serialize_world_checkpoint_receipt(receipt).as_bytes())
        .map_err(|error| format!("write world checkpoint receipt: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync world checkpoint receipt: {error}"))?;
    fs::rename(&temporary, &destination)
        .map_err(|error| format!("rename {}: {error}", destination.display()))?;
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync checkpoint root {}: {error}", root.display()))?;
    Ok(())
}

/// Read and validate one immutable world checkpoint receipt. Callers still
/// verify every referenced per-machine SmolVM receipt before restoring it.
pub(crate) fn load_world_checkpoint_receipt(root: &Path) -> Result<WorldCheckpointReceipt> {
    let path = world_checkpoint_receipt_path(root);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        format!(
            "inspect world checkpoint receipt {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "world checkpoint receipt is not a regular file: {}",
            path.display()
        ));
    }
    let content = fs::read_to_string(&path)
        .map_err(|error| format!("read world checkpoint receipt {}: {error}", path.display()))?;
    parse_world_checkpoint_receipt(&content)
}

fn validate_world_checkpoint_receipt(receipt: &WorldCheckpointReceipt) -> Result<()> {
    if receipt.schema_version != WORLD_CHECKPOINT_RECEIPT_VERSION {
        return Err(format!(
            "world checkpoint receipt schema {} is not supported; expected {}",
            receipt.schema_version, WORLD_CHECKPOINT_RECEIPT_VERSION
        ));
    }
    validate_label(&receipt.world_name).map_err(|reason| {
        format!(
            "world checkpoint receipt world '{}': {reason}",
            receipt.world_name
        )
    })?;
    validate_blake3_digest(&receipt.config_digest, "world checkpoint config digest")?;
    validate_blake3_digest(
        &receipt.material_lock_digest,
        "world checkpoint material lock digest",
    )?;
    if receipt.allocation.assignments.is_empty() {
        return Err("world checkpoint receipt has no machine allocations".into());
    }
    let mut ips = HashSet::new();
    let mut macs = HashSet::new();
    for (machine, assignment) in &receipt.allocation.assignments {
        validate_label(machine)
            .map_err(|reason| format!("world checkpoint machine '{machine}': {reason}"))?;
        if assignment.smolvm_name.is_empty()
            || assignment.smolvm_name.contains(['\t', '\r', '\n'])
            || !ips.insert(assignment.ip)
            || !macs.insert(assignment.mac)
        {
            return Err(format!(
                "world checkpoint receipt has invalid or repeated allocation for '{machine}'"
            ));
        }
    }
    if receipt
        .machine_receipts
        .keys()
        .ne(receipt.allocation.assignments.keys())
    {
        return Err(
            "world checkpoint receipt machine receipt set does not match allocations".into(),
        );
    }
    for (machine, machine_receipt) in &receipt.machine_receipts {
        validate_label(machine)
            .map_err(|reason| format!("world checkpoint machine receipt '{machine}': {reason}"))?;
        validate_blake3_digest(
            &machine_receipt.digest,
            &format!("world checkpoint machine receipt '{machine}' digest"),
        )?;
    }
    if receipt.switch.queued_frames != 0 {
        return Err("world checkpoint receipt cannot retain switch packet queues".into());
    }
    for (port, connection) in &receipt.switch.active_ports {
        validate_label(port)
            .map_err(|reason| format!("world checkpoint switch port '{port}': {reason}"))?;
        if *connection == 0 {
            return Err(format!(
                "world checkpoint switch port '{port}' has invalid connection"
            ));
        }
    }
    for (mac, port) in &receipt.switch.learned_macs {
        parse_mac(mac)
            .map_err(|reason| format!("world checkpoint switch FDB MAC '{mac}': {reason}"))?;
        validate_label(port)
            .map_err(|reason| format!("world checkpoint switch FDB port '{port}': {reason}"))?;
        if !receipt.switch.active_ports.contains_key(port) {
            return Err(format!(
                "world checkpoint switch FDB references inactive port '{port}'"
            ));
        }
    }
    Ok(())
}

fn serialize_world_checkpoint_receipt(receipt: &WorldCheckpointReceipt) -> String {
    let mut output = String::new();
    output.push_str(&format!("version\t{WORLD_CHECKPOINT_RECEIPT_VERSION}\n"));
    output.push_str(&format!("world\t{}\n", receipt.world_name));
    output.push_str(&format!("config\t{}\n", receipt.config_digest));
    output.push_str(&format!("material\t{}\n", receipt.material_lock_digest));
    output.push_str(&format!("seed\t{:016x}\n", receipt.allocation.seed));
    output.push_str(&format!("switch-epoch\t{}\n", receipt.switch.epoch));
    output.push_str(&format!("switch-queue\t{}\n", receipt.switch.queued_frames));
    for (port, connection) in &receipt.switch.active_ports {
        output.push_str(&format!("switch-port\t{port}\t{connection}\n"));
    }
    for (mac, port) in &receipt.switch.learned_macs {
        output.push_str(&format!("switch-fdb\t{mac}\t{port}\n"));
    }
    for (machine, assignment) in &receipt.allocation.assignments {
        output.push_str(&format!(
            "machine\t{machine}\t{}\t{}\t{}\n",
            assignment.ip,
            format_mac(assignment.mac),
            assignment.smolvm_name
        ));
    }
    for (machine, machine_receipt) in &receipt.machine_receipts {
        output.push_str(&format!(
            "machine-receipt\t{machine}\t{}\n",
            machine_receipt.digest
        ));
    }
    output
}

fn parse_world_checkpoint_receipt(content: &str) -> Result<WorldCheckpointReceipt> {
    let mut version = None;
    let mut world_name = None;
    let mut config_digest = None;
    let mut material_lock_digest = None;
    let mut seed = None;
    let mut switch_epoch = None;
    let mut switch_queue = None;
    let mut active_ports = BTreeMap::new();
    let mut learned_macs = BTreeMap::new();
    let mut assignments = BTreeMap::new();
    let mut machine_receipts = BTreeMap::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        match fields.as_slice() {
            ["version", value] if version.is_none() => {
                version = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| "world checkpoint receipt has invalid version".to_string())?,
                );
            }
            ["world", value] if world_name.is_none() => world_name = Some((*value).to_string()),
            ["config", value] if config_digest.is_none() => {
                config_digest = Some((*value).to_string())
            }
            ["material", value] if material_lock_digest.is_none() => {
                material_lock_digest = Some((*value).to_string())
            }
            ["seed", value] if seed.is_none() => {
                seed = Some(
                    u64::from_str_radix(value, 16)
                        .map_err(|_| "world checkpoint receipt has invalid seed".to_string())?,
                );
            }
            ["switch-epoch", value] if switch_epoch.is_none() => {
                switch_epoch = Some(value.parse::<u64>().map_err(|_| {
                    "world checkpoint receipt has invalid switch epoch".to_string()
                })?);
            }
            ["switch-queue", value] if switch_queue.is_none() => {
                switch_queue = Some(value.parse::<u64>().map_err(|_| {
                    "world checkpoint receipt has invalid switch queue count".to_string()
                })?);
            }
            ["switch-port", port, connection] => {
                validate_label(port)
                    .map_err(|reason| format!("world checkpoint switch port '{port}': {reason}"))?;
                let connection = connection.parse::<u64>().map_err(|_| {
                    format!("world checkpoint switch port '{port}' has invalid connection")
                })?;
                if connection == 0
                    || active_ports
                        .insert((*port).to_string(), connection)
                        .is_some()
                {
                    return Err(format!(
                        "world checkpoint receipt repeats or invalidates switch port '{port}'"
                    ));
                }
            }
            ["switch-fdb", mac, port] => {
                parse_mac(mac).map_err(|reason| {
                    format!("world checkpoint switch FDB MAC '{mac}': {reason}")
                })?;
                validate_label(port).map_err(|reason| {
                    format!("world checkpoint switch FDB port '{port}': {reason}")
                })?;
                if learned_macs
                    .insert((*mac).to_string(), (*port).to_string())
                    .is_some()
                {
                    return Err(format!(
                        "world checkpoint receipt repeats switch FDB MAC '{mac}'"
                    ));
                }
            }
            ["machine", machine, ip, mac, smolvm_name] => {
                validate_label(machine).map_err(|reason| {
                    format!("world checkpoint receipt machine '{machine}': {reason}")
                })?;
                if assignments
                    .insert(
                        (*machine).to_string(),
                        Assignment {
                            ip: ip.parse().map_err(|_| {
                                format!(
                                    "world checkpoint receipt machine '{machine}' has invalid IP"
                                )
                            })?,
                            mac: parse_mac(mac).map_err(|reason| {
                                format!("world checkpoint receipt machine '{machine}': {reason}")
                            })?,
                            smolvm_name: (*smolvm_name).to_string(),
                        },
                    )
                    .is_some()
                {
                    return Err(format!(
                        "world checkpoint receipt repeats machine '{machine}'"
                    ));
                }
            }
            ["machine-receipt", machine, digest] => {
                validate_label(machine).map_err(|reason| {
                    format!("world checkpoint receipt machine '{machine}': {reason}")
                })?;
                if machine_receipts
                    .insert(
                        (*machine).to_string(),
                        MachineCheckpointReceipt {
                            digest: (*digest).to_string(),
                        },
                    )
                    .is_some()
                {
                    return Err(format!(
                        "world checkpoint receipt repeats machine receipt '{machine}'"
                    ));
                }
            }
            _ => {
                return Err("world checkpoint receipt contains an unknown or malformed line".into())
            }
        }
    }
    if version != Some(WORLD_CHECKPOINT_RECEIPT_VERSION) {
        return Err(format!(
            "world checkpoint receipt format is not version {WORLD_CHECKPOINT_RECEIPT_VERSION}"
        ));
    }
    let receipt = WorldCheckpointReceipt {
        schema_version: WORLD_CHECKPOINT_RECEIPT_VERSION,
        world_name: world_name
            .ok_or_else(|| "world checkpoint receipt is missing world".to_string())?,
        config_digest: config_digest
            .ok_or_else(|| "world checkpoint receipt is missing config digest".to_string())?,
        material_lock_digest: material_lock_digest
            .ok_or_else(|| "world checkpoint receipt is missing material digest".to_string())?,
        allocation: WorldAllocationState {
            seed: seed.ok_or_else(|| "world checkpoint receipt is missing seed".to_string())?,
            assignments,
        },
        machine_receipts,
        switch: SwitchCheckpointReceipt {
            epoch: switch_epoch
                .ok_or_else(|| "world checkpoint receipt is missing switch epoch".to_string())?,
            queued_frames: switch_queue.ok_or_else(|| {
                "world checkpoint receipt is missing switch queue count".to_string()
            })?,
            active_ports,
            learned_macs,
        },
    };
    validate_world_checkpoint_receipt(&receipt)?;
    Ok(receipt)
}

pub(crate) fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("chmod {}: {error}", path.display()))
}

/// Allocate v2 identities only from the v2 record and paths.  This mirrors
/// the stable address/MAC invariants of v1 while deliberately avoiding every
/// v1 load/write/allocation entry point.
pub(crate) fn allocate_v2_allocation_state(
    previous: Option<WorldAllocationState>,
    config: &WorldConfig,
    paths: &V2WorldPaths,
) -> Result<WorldAllocationState> {
    let previous = previous.unwrap_or_else(|| WorldAllocationState {
        seed: new_v2_seed(paths),
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
        let assignment = allocate_v2_assignment(
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

pub(crate) fn new_v2_seed(paths: &V2WorldPaths) -> u64 {
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

fn allocate_v2_assignment(
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
        "smw-v2",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MachineConfig, NetworkConfig};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_WORLD_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TemporaryWorld {
        root: PathBuf,
    }

    impl TemporaryWorld {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let serial = TEMP_WORLD_COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "smolworld-state-test-{}-{nonce}-{serial}",
                std::process::id(),
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn config_path(&self) -> PathBuf {
            self.root.join("demo/.smolworld")
        }

        fn v1_state_dir(&self) -> PathBuf {
            self.root.join("v1-state")
        }

        fn v1_state_file(&self) -> PathBuf {
            self.v1_state_dir().join("state")
        }

        fn v1_lifecycle_file(&self) -> PathBuf {
            self.v1_state_dir().join("lifecycle")
        }
    }

    impl Drop for TemporaryWorld {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn v2_paths_for(world: &TemporaryWorld) -> V2WorldPaths {
        let state_dir = world.root.join("home/.smolworld/v2/world-2a");
        V2WorldPaths {
            canonical_config: world.config_path(),
            config_dir: world.root.join("demo"),
            hash: 42,
            state_file: state_dir.join("state"),
            state_dir,
            runtime_dir: world.root.join("runtime-v2"),
        }
    }

    fn material_lock() -> V2MaterialLock {
        V2MaterialLock {
            resolver_abi: material_lock_resolver_abi().to_string(),
            world: V2WorldIdentity {
                config_digest: digest_bytes(b"world: sentry-backend\n"),
            },
            smolfiles: BTreeMap::from([
                (
                    "postgres".to_string(),
                    V2SmolfileObservation {
                        authored_relative_path: PathBuf::from("smol/postgres.Smolfile"),
                        authored_digest: digest_bytes(b"image = \"postgres\"\n"),
                        prepared_path: PathBuf::from(
                            "/tmp/smolworld/prepared/postgres.Smolfile",
                        ),
                        prepared_digest: digest_bytes(b"image = \"/tmp/postgres.tar\"\n"),
                    },
                ),
                (
                    "runner".to_string(),
                    V2SmolfileObservation {
                        authored_relative_path: PathBuf::from("smol/runner.Smolfile"),
                        authored_digest: digest_bytes(b"image = \"runner\"\n"),
                        prepared_path: PathBuf::from("/tmp/smolworld/prepared/runner.Smolfile"),
                        prepared_digest: digest_bytes(b"image = \"/tmp/runner.tar\"\n"),
                    },
                ),
            ]),
            seeds: vec![V2SeedObservation {
                machine: "clickhouse".to_string(),
                source_relative_path: PathBuf::from("assets/clickhouse.xml"),
                destination: "/etc/clickhouse-server/config.d/niceforge.xml".to_string(),
                mode: 0o644,
                digest: digest_bytes(b"<clickhouse/>\n"),
            }],
            images: BTreeMap::from([(
                "postgres".to_string(),
                V2ImageMaterial {
                    machine: "postgres".to_string(),
                    source_kind: "registry".to_string(),
                    source_reference: "docker.io/library/postgres@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                    source_digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                    local_path: PathBuf::from("/tmp/smolworld/material/postgres.ext4"),
                    image_digest: "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                },
            )]),
        }
    }

    #[test]
    fn v2_material_lock_round_trips_all_material_identity() {
        let world = TemporaryWorld::new();
        fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
        fs::write(world.config_path(), b"format: 2\n").unwrap();
        let mut paths = v2_paths_for(&world);
        paths.canonical_config = fs::canonicalize(world.config_path()).unwrap();
        paths.state_file = paths.state_dir.join("state");
        let record = material_lock();
        write_v2_material_lock(&paths, &record).unwrap();

        let serialized = fs::read_to_string(paths.material_lock_path()).unwrap();
        assert!(serialized.starts_with("version\t5\nresolver_abi\tsmolvm-external-world/v3\n"));
        assert!(!serialized.contains(&world.root.display().to_string()));
        assert!(!paths.state_dir.exists());
        assert_eq!(
            load_v2_material_lock(&paths.material_lock_path()).unwrap(),
            Some(record)
        );
    }

    #[test]
    fn world_checkpoint_receipt_round_trips_stable_world_identity() {
        let world = TemporaryWorld::new();
        let checkpoint = world.root.join("checkpoint");
        fs::create_dir(&checkpoint).unwrap();
        let receipt = WorldCheckpointReceipt {
            schema_version: WORLD_CHECKPOINT_RECEIPT_VERSION,
            world_name: "sentry".to_string(),
            config_digest: digest_bytes(b"world config"),
            material_lock_digest: digest_bytes(b"prepared material"),
            allocation: WorldAllocationState {
                seed: 0x1234,
                assignments: BTreeMap::from([(
                    "runner".to_string(),
                    Assignment {
                        ip: "10.89.0.2".parse().unwrap(),
                        mac: [0x02, 0, 0, 0, 0, 2],
                        smolvm_name: "smw-v2-00000000002a-runner".to_string(),
                    },
                )]),
            },
            machine_receipts: BTreeMap::from([(
                "runner".to_string(),
                MachineCheckpointReceipt {
                    digest: digest_bytes(b"smolvm machine receipt"),
                },
            )]),
            switch: SwitchCheckpointReceipt {
                epoch: 7,
                queued_frames: 0,
                active_ports: BTreeMap::from([("runner".to_string(), 3)]),
                learned_macs: BTreeMap::from([(
                    "02:00:00:00:00:02".to_string(),
                    "runner".to_string(),
                )]),
            },
        };

        write_world_checkpoint_receipt(&checkpoint, &receipt).unwrap();

        assert_eq!(load_world_checkpoint_receipt(&checkpoint).unwrap(), receipt);
        let serialized = fs::read_to_string(world_checkpoint_receipt_path(&checkpoint)).unwrap();
        assert!(serialized.starts_with("version\t2\nworld\tsentry\n"));
        assert!(serialized.contains("machine-receipt\trunner\tblake3:"));

        fs::write(
            world_checkpoint_receipt_path(&checkpoint),
            serialized.replacen("version\t2", "version\t1", 1),
        )
        .unwrap();
        assert!(load_world_checkpoint_receipt(&checkpoint)
            .unwrap_err()
            .contains("not version 2"));
    }

    #[test]
    fn machine_checkpoint_receipt_digest_is_bounded_and_rejects_symlinks() {
        let world = TemporaryWorld::new();
        let receipt = world.root.join(MACHINE_CHECKPOINT_RECEIPT_NAME);
        fs::write(&receipt, b"{}\n").unwrap();
        assert_eq!(
            digest_machine_checkpoint_receipt(&receipt).unwrap(),
            digest_bytes(b"{}\n")
        );

        let link = world.root.join("receipt-link");
        std::os::unix::fs::symlink(&receipt, &link).unwrap();
        assert!(digest_machine_checkpoint_receipt(&link)
            .unwrap_err()
            .contains("not a regular file"));

        fs::write(
            &receipt,
            vec![0_u8; (MAX_MACHINE_CHECKPOINT_RECEIPT_BYTES + 1) as usize],
        )
        .unwrap();
        assert!(digest_machine_checkpoint_receipt(&receipt)
            .unwrap_err()
            .contains("larger than"));
    }

    #[test]
    fn v2_paths_are_separate_and_do_not_adopt_v1_state() {
        let world = TemporaryWorld::new();
        fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
        fs::write(world.config_path(), b"format: 2\n").unwrap();
        let canonical_config = fs::canonicalize(world.config_path()).unwrap();
        ensure_private_dir(&world.v1_state_dir()).unwrap();
        fs::write(world.v1_state_file(), b"v1 allocation remains untouched\n").unwrap();
        let mut v2 = v2_paths_for(&world);
        v2.canonical_config = canonical_config;

        assert_ne!(world.v1_state_dir(), v2.state_dir);
        assert_eq!(v2.state_dir.parent().unwrap().file_name().unwrap(), "v2");
        assert_eq!(
            load_v2_material_lock(&v2.material_lock_path()).unwrap(),
            None
        );
        let record = material_lock();
        write_v2_material_lock(&v2, &record).unwrap();
        assert_eq!(
            fs::read(world.v1_state_file()).unwrap(),
            b"v1 allocation remains untouched\n"
        );
        assert!(world.v1_state_file().exists());
        assert!(!v2.state_dir.exists());
    }

    #[test]
    fn v2_state_round_trips_with_an_explicit_v2_version() {
        let world = TemporaryWorld::new();
        let paths = v2_paths_for(&world);
        let state = WorldAllocationState {
            seed: 0xfeed,
            assignments: BTreeMap::from([(
                "redis".to_string(),
                Assignment {
                    ip: Ipv4Addr::new(10, 89, 0, 2),
                    mac: [0x02, 1, 2, 3, 4, 5],
                    smolvm_name: "smw-v2-redis".to_string(),
                },
            )]),
        };

        write_v2_allocation_state(&paths, &state).unwrap();
        assert_eq!(
            load_v2_allocation_state(&paths.state_file).unwrap(),
            Some(state.clone())
        );
        assert_eq!(
            fs::read_to_string(&paths.state_file)
                .unwrap()
                .lines()
                .next(),
            Some("version\t2")
        );
    }

    #[test]
    fn v2_allocation_is_stable_reserved_and_version_namespaced() {
        let world = TemporaryWorld::new();
        let paths = v2_paths_for(&world);
        let config = WorldConfig {
            name: "demo".to_string(),
            network: NetworkConfig {
                subnet: [10, 89, 0, 0],
                gateway: "10.89.0.9".parse().unwrap(),
                dns: "10.89.0.9".parse().unwrap(),
                domain: "demo.test".to_string(),
                egress: false,
            },
            machines: BTreeMap::from([
                (
                    "redis".to_string(),
                    MachineConfig {
                        smolfile: PathBuf::from("redis.Smolfile"),
                        depends_on: Vec::new(),
                        seed_files: Vec::new(),
                    },
                ),
                (
                    "client".to_string(),
                    MachineConfig {
                        smolfile: PathBuf::from("client.Smolfile"),
                        depends_on: vec!["redis".to_string()],
                        seed_files: Vec::new(),
                    },
                ),
            ]),
        };
        let first = allocate_v2_allocation_state(
            Some(WorldAllocationState {
                seed: 7,
                assignments: BTreeMap::new(),
            }),
            &config,
            &paths,
        )
        .unwrap();
        let second = allocate_v2_allocation_state(Some(first.clone()), &config, &paths).unwrap();

        assert_eq!(first, second);
        assert!(first
            .assignments
            .values()
            .all(|assignment| assignment.ip != config.network.gateway));
        assert!(first
            .assignments
            .values()
            .all(|assignment| assignment.smolvm_name.starts_with("smw-v2-")));
        assert_ne!(
            first.assignments["redis"].ip,
            first.assignments["client"].ip
        );
    }

    #[test]
    fn v2_lifecycle_and_recovery_never_adopt_v1_files() {
        let world = TemporaryWorld::new();
        let paths = v2_paths_for(&world);
        ensure_private_dir(&world.v1_state_dir()).unwrap();
        fs::write(
            world.v1_state_file(),
            b"version\t1\nseed\t000000000000000b\n",
        )
        .unwrap();
        fs::write(
            world.v1_lifecycle_file(),
            b"version\t1\nstate\tstarting\nowner_pid\t-\ngeneration\t0000000000000001\n",
        )
        .unwrap();

        assert_eq!(load_v2_allocation_state(&paths.state_file).unwrap(), None);
        assert_eq!(load_v2_lifecycle(&paths.lifecycle_path()).unwrap(), None);
        let absent = inspect_v2_recovery(&paths).unwrap();
        assert_eq!(absent.state_file, ArtifactState::Missing);
        assert_eq!(absent.lifecycle_file, ArtifactState::Missing);
        assert_eq!(absent.runtime_dir, ArtifactState::Missing);
        assert!(!absent.needs_recovery());

        let lifecycle = mark_v2_starting(&paths).unwrap();
        assert_eq!(lifecycle.state, LifecycleState::Starting);
        assert_eq!(
            fs::read_to_string(paths.lifecycle_path())
                .unwrap()
                .lines()
                .next(),
            Some("version\t2")
        );
        write_v2_allocation_state(
            &paths,
            &WorldAllocationState {
                seed: 12,
                assignments: BTreeMap::new(),
            },
        )
        .unwrap();
        assert_eq!(
            load_v2_allocation_state(&paths.state_file)
                .unwrap()
                .unwrap()
                .seed,
            12
        );
        assert!(inspect_v2_recovery(&paths).unwrap().needs_recovery());

        mark_v2_absent(&paths).unwrap();
        assert!(!inspect_v2_recovery(&paths).unwrap().needs_recovery());
        assert!(world.v1_state_file().exists());
        assert!(world.v1_lifecycle_file().exists());
    }

    #[test]
    fn capture_intent_prevents_stale_world_cleanup_until_rollback_or_commit() {
        let world = TemporaryWorld::new();
        let paths = v2_paths_for(&world);

        mark_v2_starting(&paths).unwrap();
        mark_v2_created(&paths).unwrap();
        mark_v2_attached(&paths).unwrap();
        mark_v2_running(&paths).unwrap();
        let capturing = mark_v2_capturing(&paths).unwrap();
        assert_eq!(capturing.state, LifecycleState::Capturing);
        assert!(capturing.state.retains_checkpoint_sources());
        assert!(!capturing.state.needs_recovery());

        let rolled_back = mark_v2_capture_rolled_back(&paths).unwrap();
        assert_eq!(rolled_back.state, LifecycleState::Running);
        mark_v2_capturing(&paths).unwrap();
        let captured = mark_v2_captured(&paths).unwrap();
        assert_eq!(captured.state, LifecycleState::Captured);
        assert!(captured.state.retains_checkpoint_sources());
    }

    #[test]
    fn restored_world_can_attach_without_a_synthetic_create_transition() {
        let world = TemporaryWorld::new();
        let paths = v2_paths_for(&world);

        mark_v2_starting(&paths).unwrap();
        let attached = mark_v2_attached(&paths).unwrap();
        assert_eq!(attached.state, LifecycleState::Attached);
        assert_eq!(
            mark_v2_running(&paths).unwrap().state,
            LifecycleState::Running
        );
    }

    #[test]
    fn v2_cleanup_is_scoped_to_v2_runtime_and_temporary_files() {
        let world = TemporaryWorld::new();
        let paths = v2_paths_for(&world);
        ensure_private_dir(&world.v1_state_dir()).unwrap();
        ensure_private_dir(&paths.state_dir).unwrap();
        fs::write(paths.state_dir.join("state.123.tmp"), b"v2 temporary").unwrap();
        fs::write(world.v1_state_dir().join("state.123.tmp"), b"v1 temporary").unwrap();
        prepare_v2_runtime_dir(&paths).unwrap();
        fs::write(paths.runtime_dir.join("owned"), b"v2").unwrap();

        assert_eq!(remove_v2_stale_temporary_files(&paths).unwrap(), 1);
        assert!(world.v1_state_dir().join("state.123.tmp").exists());
        assert_eq!(remove_v2_runtime_dir(&paths), Ok(()));
        assert!(!paths.runtime_dir.exists());
        assert!(world.v1_state_dir().exists());
    }

    #[test]
    fn v2_material_lock_requires_absolute_seed_destinations_and_matching_image_keys() {
        let mut record = material_lock();
        record.seeds[0].destination = "relative/path".to_string();
        assert!(record.validate().is_err());

        let mut record = material_lock();
        record.images.get_mut("postgres").unwrap().machine = "runner".to_string();
        assert!(record.validate().is_err());
    }

    #[test]
    fn v2_material_lock_keeps_oci_and_local_digest_algorithms_separate() {
        let mut registry = material_lock();
        registry.images.get_mut("postgres").unwrap().source_digest =
            "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        assert!(registry.validate().is_err());

        let mut local = material_lock();
        let material = local.images.get_mut("postgres").unwrap();
        material.source_kind = "local-archive".to_string();
        material.source_reference = "/tmp/postgres.tar".to_string();
        material.source_digest =
            "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        assert!(local.validate().is_ok());

        local.images.get_mut("postgres").unwrap().image_digest =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        assert!(local.validate().is_err());
    }

    #[test]
    fn v2_identity_from_config_records_portable_content_digest() {
        let world = TemporaryWorld::new();
        fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
        fs::write(world.config_path(), b"format: 2\n").unwrap();
        let record =
            V2MaterialLock::from_config(&world.config_path(), material_lock_resolver_abi())
                .unwrap();
        assert_eq!(record.world.config_digest, digest_bytes(b"format: 2\n"));
    }

    #[test]
    fn material_digest_uses_blake3() {
        assert_eq!(
            digest_bytes(b""),
            "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }
}
