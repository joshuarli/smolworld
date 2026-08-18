use super::{command_help, option_display, option_matches, parse_error, parse_file, missing, unexpected, Cli, CommandSpec, FILE_OPTION, HELP_OPTION, METRICS_JSON_OPTION, VERSION_OPTION};
use crate::Result;
use lexopt::Parser;
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "metrics",
    about: "Collect host-side metrics for the recorded world machines.",
    options: &[FILE_OPTION, METRICS_JSON_OPTION],
    positionals: &[],
    examples: &["smolworld metrics --json"],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut json_seen = false;
    while let Some(arg) = parser.next().map_err(|error| parse_error(SPEC.name, error))? {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => parse_file(parser, SPEC.name, &mut config, &mut file_seen)?,
            arg if option_matches(&arg, &METRICS_JSON_OPTION) && !json_seen => json_seen = true,
            arg if option_matches(&arg, &METRICS_JSON_OPTION) => {
                return Err(format!(
                    "{} accepts {} at most once",
                    SPEC.name,
                    option_display(&METRICS_JSON_OPTION)
                ))
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    if !json_seen {
        return Err(missing(SPEC.name));
    }
    Ok(Cli::Metrics { config })
}
