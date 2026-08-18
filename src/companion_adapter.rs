//! Smolworld's one internal companion-operation adapter.
//!
//! This is intentionally **not** an smolvm protocol. Smolvm and Smolfiles are
//! upstream contracts. `src/smolvm.rs` is the only adapter allowed to translate
//! these typed world operations to its existing CLI flags and versioned TSV
//! replies. Keeping process execution here makes that translation explicit and
//! prevents the rest of smolworld from acquiring another smolvm command shape.

use crate::Result;
use std::process::{Command, ExitStatus, Output};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    Prepare,
    Create,
    Start,
    Stop,
    Delete,
    Status,
    Stats,
    Checkpoint,
    Restore,
    Exec,
    Copy,
}

impl Operation {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Create => "create",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Delete => "delete",
            Self::Status => "status",
            Self::Stats => "stats",
            Self::Checkpoint => "checkpoint",
            Self::Restore => "restore",
            Self::Exec => "exec",
            Self::Copy => "copy",
        }
    }
}

/// Run an already-translated upstream command. The caller owns argument
/// mapping and response decoding; this module owns consistent operation-level
/// errors so no other module needs to know the companion transport.
pub(crate) fn output(operation: Operation, command: &mut Command) -> Result<Output> {
    command
        .output()
        .map_err(|error| format!("run upstream smolvm {}: {error}", operation.name()))
}

pub(crate) fn status(operation: Operation, command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .map_err(|error| format!("run upstream smolvm {}: {error}", operation.name()))?;
    status_result(operation, status)
}

/// Run a control operation whose successful output is not part of the world
/// CLI response. Capturing stderr here preserves the upstream failure detail
/// for checkpoint/restore transaction diagnostics without swallowing the
/// caller-visible stdout of `exec` or `cp`.
pub(crate) fn captured_status(operation: Operation, command: &mut Command) -> Result<()> {
    let output = output(operation, command)?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if detail.is_empty() {
        status_result(operation, output.status)
    } else {
        Err(format!(
            "upstream smolvm {} exited with {}: {detail}",
            operation.name(),
            output.status
        ))
    }
}

pub(crate) fn status_result(operation: Operation, status: ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "upstream smolvm {} exited with {status}",
            operation.name()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_names_are_closed_and_stable() {
        assert_eq!(Operation::Prepare.name(), "prepare");
        assert_eq!(Operation::Copy.name(), "copy");
        assert_eq!(Operation::Stats.name(), "stats");
    }
}
