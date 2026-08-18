use super::{command_help, option_matches, parse_error, parse_file, unexpected, Cli, CommandSpec, FILE_OPTION, HELP_OPTION, VERSION_OPTION};
use crate::Result;
use lexopt::Parser;
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "prepare",
    about: "Resolve and seal the host inputs referenced by the world.",
    options: &[FILE_OPTION],
    positionals: &[],
    examples: &["smolworld prepare", "smolworld prepare --file ./world.smolworld"],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    while let Some(arg) = parser.next().map_err(|error| parse_error(SPEC.name, error))? {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => parse_file(parser, SPEC.name, &mut config, &mut file_seen)?,
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    Ok(Cli::Prepare { config })
}
