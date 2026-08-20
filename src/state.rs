//! Durable world state is split by persistence boundary. This facade preserves the
//! historical `crate::state` API while keeping paths, material, lifecycle,
//! allocation, and checkpoint records in focused internal modules.

mod allocation;
mod checkpoint;
mod lifecycle;
mod material;
mod paths;

#[cfg(test)]
mod tests;

pub(crate) use allocation::*;
pub(crate) use checkpoint::*;
pub(crate) use lifecycle::*;
pub(crate) use material::*;
pub(crate) use paths::*;

pub(crate) const MACHINE_CHECKPOINT_RECEIPT_NAME: &str = "smolvm-checkpoint.json";
pub(crate) const MACHINE_DELTA_RECEIPT_NAME: &str = "smolvm-delta.json";

/// Return the one supported SmolVM receipt filename in a machine checkpoint
/// directory.  A full base and a changed-block descendant are mutually
/// exclusive; accepting both would make the world receipt bind an ambiguous
/// machine state.
pub(crate) fn machine_checkpoint_receipt_path(root: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let full = root.join(MACHINE_CHECKPOINT_RECEIPT_NAME);
    let delta = root.join(MACHINE_DELTA_RECEIPT_NAME);
    match (full.exists(), delta.exists()) {
        (true, false) => Ok(full),
        (false, true) => Ok(delta),
        (false, false) => Err(format!("machine checkpoint has no SmolVM receipt beneath {}", root.display())),
        (true, true) => Err(format!("machine checkpoint has ambiguous full and delta receipts beneath {}", root.display())),
    }
}
