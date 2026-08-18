use super::{config, Cli, CommandSpec};
use crate::Result;
use lexopt::Parser;
use std::path::PathBuf;

/// Compose calls this spelling `convert`; smolworld has one strict authored
/// configuration and therefore delegates to the same resolved renderer.
pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "convert",
    about: "Alias for config: validate and render the resolved world configuration.",
    options: config::SPEC.options,
    positionals: &[],
    examples: &["smolworld convert --format json"],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, config_path: PathBuf) -> Result<Cli> {
    config::parse_with_name(parser, config_path, SPEC.name)
}
