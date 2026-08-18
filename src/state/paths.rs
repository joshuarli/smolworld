use crate::Result;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Paths owned by the world materializer. These paths are derived only from
/// the canonical configuration, so cleanup never reads, adopts, or removes
/// another world's allocation directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorldPaths {
    pub(crate) canonical_config: PathBuf,
    pub(crate) config_dir: PathBuf,
    pub(crate) hash: u64,
    pub(crate) state_dir: PathBuf,
    pub(crate) state_file: PathBuf,
    pub(crate) runtime_dir: PathBuf,
}

impl WorldPaths {
    pub(crate) fn lock_path(&self) -> PathBuf {
        self.state_dir.join("world.lock")
    }

    /// The generated, sealed preparation record lives beside the authored
    /// `.smolworld`, not under the runtime allocation namespace.
    pub(crate) fn material_lock_path(&self) -> PathBuf {
        self.config_dir.join(".smolworld.lock")
    }

    /// Generated local-only Smolfiles are private world material, distinct
    /// from runtime allocation and safe to retain across `down`/`up` cycles.
    pub(crate) fn material_dir(&self) -> PathBuf {
        self.state_dir.join("material")
    }

    pub(crate) fn lifecycle_path(&self) -> PathBuf {
        self.state_dir.join("lifecycle")
    }
}

/// Return the private allocation namespace for a configuration. This
/// deliberately does not inspect legacy state and has no fallback to it.
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
    let state_file = state_dir.join("state");
    let runtime_dir = PathBuf::from("/tmp").join(format!("smw-{hash:012x}"));
    Ok(WorldPaths {
        canonical_config,
        config_dir,
        hash,
        state_dir,
        state_file,
        runtime_dir,
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

pub(crate) fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("chmod {}: {error}", path.display()))
}
