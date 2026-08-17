use super::WorldPaths;
use crate::config::validate_label;
use crate::model::ImageSourceKind;
use crate::Result;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const MATERIAL_LOCK_VERSION: u8 = 5;
const MATERIAL_LOCK_RESOLVER_ABI: &str = "smolvm-external-world/v3";
pub(crate) const MAX_MACHINE_CHECKPOINT_RECEIPT_BYTES: u64 = 1024 * 1024;

/// A content digest observation for one machine's Smolfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SmolfileObservation {
    /// Immutable user-authored declaration, relative to the `.smolworld`
    /// directory. This keeps a prepared world valid after a caller copies its
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
pub(crate) struct SeedObservation {
    pub(crate) machine: String,
    /// Source path relative to the `.smolworld` directory. See
    /// `SmolfileObservation::authored_relative_path`.
    pub(crate) source_relative_path: PathBuf,
    pub(crate) destination: String,
    pub(crate) mode: u32,
    pub(crate) digest: String,
}

/// A local image/rootfs material reference resolved by the host-side resolver.
/// Guests consume this local path; they never resolve or pull the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageMaterial {
    pub(crate) machine: String,
    /// Image kind before preparation.
    pub(crate) source_kind: ImageSourceKind,
    /// The original image string in the authored Smolfile.
    pub(crate) source_reference: String,
    /// Immutable OCI source digest or local archive digest.
    pub(crate) source_digest: String,
    pub(crate) local_path: PathBuf,
    pub(crate) image_digest: String,
}

/// Identity of the world declaration captured by a material record. The
/// digest binds the exact declaration bytes without binding a portable lock to
/// a developer-checkout path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorldIdentity {
    pub(crate) config_digest: String,
}

/// Durable host-side inputs for one world materialization.
///
/// The maps are keyed by the machine's declared name and are serialized in
/// sorted order.  Seed observations remain a vector because a machine may
/// have multiple seed files; serialization sorts that vector by all identity
/// fields.  This is a lock/material record, not a cache: every listed local
/// reference and digest is required for `check` to accept the prepared world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaterialLock {
    pub(crate) resolver_abi: String,
    pub(crate) world: WorldIdentity,
    pub(crate) smolfiles: BTreeMap<String, SmolfileObservation>,
    pub(crate) seeds: Vec<SeedObservation>,
    pub(crate) images: BTreeMap<String, ImageMaterial>,
}

impl MaterialLock {
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
            world: WorldIdentity {
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
            validate_field(&material.source_reference, "image source reference")?;
            match material.source_kind {
                ImageSourceKind::Registry => {
                    validate_sha256_digest(&material.source_digest, "registry image source digest")?
                }
                ImageSourceKind::LocalArchive => {
                    validate_blake3_digest(&material.source_digest, "local archive source digest")?
                }
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

pub(crate) fn load_material_lock(path: &Path) -> Result<Option<MaterialLock>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    parse_material_lock(&content).map(Some)
}

pub(crate) fn write_material_lock(paths: &WorldPaths, record: &MaterialLock) -> Result<()> {
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
    file.write_all(serialize_material_lock(record).as_bytes())
        .map_err(|error| format!("write material lock: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync material lock: {error}"))?;
    fs::rename(&temporary, paths.material_lock_path())
        .map_err(|error| format!("rename {}: {error}", paths.material_lock_path().display()))?;
    Ok(())
}

fn parse_material_lock(content: &str) -> Result<MaterialLock> {
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
                        SmolfileObservation {
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
                seeds.push(SeedObservation {
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
                        ImageMaterial {
                            machine: (*machine).to_string(),
                            source_kind: ImageSourceKind::parse(source_kind)
                                .map_err(|error| format!("material lock image '{machine}': {error}"))?,
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
    let record = MaterialLock {
        resolver_abi: resolver_abi
            .ok_or_else(|| "material lock is missing resolver ABI".to_string())?,
        world: WorldIdentity {
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

fn serialize_material_lock(record: &MaterialLock) -> String {
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
            material.source_kind.as_str(),
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

pub(crate) fn validate_blake3_digest(value: &str, label: &str) -> Result<()> {
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
