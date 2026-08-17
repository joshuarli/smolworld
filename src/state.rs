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
