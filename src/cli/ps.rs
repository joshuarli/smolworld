use super::{command_help, option_display, option_matches, parse_error, parse_file, unexpected, Cli, CommandSpec, FILE_OPTION, HELP_OPTION, JSON_OPTION, VERSION_OPTION};
use crate::Result;
use lexopt::Parser;
#[cfg(test)]
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PsFormat {
    Table,
    Json,
}

/// Parsed options for machine visibility.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PsOptions {
    pub(crate) config: PathBuf,
    pub(crate) format: PsFormat,
}

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "ps",
    about: "Show host-visible lifecycle observations for the recorded machines.",
    options: &[FILE_OPTION, JSON_OPTION],
    positionals: &[],
    examples: &["smolworld ps", "smolworld ps --json"],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut format = PsFormat::Table;
    while let Some(arg) = parser.next().map_err(|error| parse_error(SPEC.name, error))? {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => parse_file(parser, SPEC.name, &mut config, &mut file_seen)?,
            arg if option_matches(&arg, &JSON_OPTION) && format == PsFormat::Table => format = PsFormat::Json,
            arg if option_matches(&arg, &JSON_OPTION) => {
                return Err(format!("{} accepts {} at most once", SPEC.name, option_display(&JSON_OPTION)))
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    Ok(Cli::Ps { config, format })
}

#[cfg(test)]
pub(crate) fn parse_ps_options(config: PathBuf, rest: &[OsString]) -> Result<PsOptions> {
    let mut args = Vec::with_capacity(rest.len() + 1);
    args.push(OsString::from("smolworld"));
    args.extend(rest.iter().cloned());
    match parse(&mut Parser::from_iter(args), config)? {
        Cli::Ps { config, format } => Ok(PsOptions { config, format }),
        _ => Err("ps did not produce a parsed ps command".into()),
    }
}
