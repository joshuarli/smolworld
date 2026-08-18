use super::{
    command_help, missing, option_matches, os_string, parse_error, parse_file, unexpected, Cli,
    CommandSpec, PositionalSpec, FILE_OPTION, HELP_OPTION, VERSION_OPTION,
};
use crate::Result;
use lexopt::{Arg, Parser};
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "shell",
    about: "Open an interactive /bin/sh in one running recorded service.",
    options: &[FILE_OPTION],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: true,
        repeatable: false,
        help: "Declared running service",
    }],
    examples: &["smolworld shell runner"],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut service = None;
    while let Some(arg) = parser
        .next()
        .map_err(|error| parse_error(SPEC.name, error))?
    {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => {
                parse_file(parser, SPEC.name, &mut config, &mut file_seen)?
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            Arg::Value(value) if service.is_none() => {
                service = Some(os_string(value, SPEC.name, "SERVICE")?)
            }
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    Ok(Cli::Shell {
        config,
        service: service.ok_or_else(|| missing(SPEC.name))?,
    })
}
