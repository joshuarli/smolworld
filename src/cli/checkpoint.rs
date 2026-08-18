use super::{
    command_help, missing, option_display, option_matches, parse_error, parse_file, parse_value,
    path_argument, unexpected, Cli, CommandSpec, FILE_OPTION, HELP_OPTION, OUTPUT_OPTION,
    VERSION_OPTION,
};
use crate::Result;
use lexopt::Parser;
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "checkpoint",
    about: "Ask the running supervisor to capture every machine and the switch coherently.",
    options: &[FILE_OPTION, OUTPUT_OPTION],
    positionals: &[],
    examples: &["smolworld checkpoint --output /absolute/path/checkpoint"],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut output = None;
    while let Some(arg) = parser
        .next()
        .map_err(|error| parse_error(SPEC.name, error))?
    {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => {
                parse_file(parser, SPEC.name, &mut config, &mut file_seen)?
            }
            arg if option_matches(&arg, &OUTPUT_OPTION) && output.is_none() => {
                output = Some(path_argument(parse_value(
                    parser,
                    SPEC.name,
                    &OUTPUT_OPTION,
                )?));
            }
            arg if option_matches(&arg, &OUTPUT_OPTION) => {
                return Err(format!(
                    "{} accepts {} at most once",
                    SPEC.name,
                    option_display(&OUTPUT_OPTION)
                ))
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    let Some(output) = output else {
        return Err(missing(SPEC.name));
    };
    Ok(Cli::Checkpoint { config, output })
}
