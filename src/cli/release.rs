use super::{command_help, missing, option_display, option_matches, parse_error, parse_file, parse_value, path_argument, unexpected, Cli, CommandSpec, CHECKPOINT_OPTION, FILE_OPTION, HELP_OPTION, VERSION_OPTION};
use crate::Result;
use lexopt::Parser;
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "release",
    about: "Irreversibly release one retained checkpoint and its recorded source VMs.",
    options: &[FILE_OPTION, CHECKPOINT_OPTION],
    positionals: &[],
    examples: &["smolworld release --checkpoint /absolute/path/checkpoint"],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut checkpoint = None;
    while let Some(arg) = parser.next().map_err(|error| parse_error(SPEC.name, error))? {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => parse_file(parser, SPEC.name, &mut config, &mut file_seen)?,
            arg if option_matches(&arg, &CHECKPOINT_OPTION) && checkpoint.is_none() => {
                checkpoint = Some(path_argument(parse_value(parser, SPEC.name, &CHECKPOINT_OPTION)?));
            }
            arg if option_matches(&arg, &CHECKPOINT_OPTION) => {
                return Err(format!("{} accepts {} at most once", SPEC.name, option_display(&CHECKPOINT_OPTION)))
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    let Some(checkpoint) = checkpoint else {
        return Err(missing(SPEC.name));
    };
    Ok(Cli::Release { config, checkpoint })
}
