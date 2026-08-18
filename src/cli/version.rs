use super::{
    command_help, option_display, option_matches, parse_error, unexpected, Cli, CommandSpec,
    HELP_OPTION, SHORT_OPTION, VERSION_FORMAT_OPTION, VERSION_OPTION,
};
use crate::Result;
use lexopt::Parser;
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "version",
    about: "Print smolworld version information.",
    options: &[SHORT_OPTION, VERSION_FORMAT_OPTION],
    positionals: &[],
    examples: &[
        "smolworld version",
        "smolworld version --short",
        "smolworld version --format json",
    ],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, _config: PathBuf) -> Result<Cli> {
    let mut short = false;
    let mut format = None;
    while let Some(arg) = parser
        .next()
        .map_err(|error| parse_error(SPEC.name, error))?
    {
        match arg {
            arg if option_matches(&arg, &SHORT_OPTION) && !short => short = true,
            arg if option_matches(&arg, &SHORT_OPTION) => {
                return Err("version accepts --short at most once".into())
            }
            arg if option_matches(&arg, &VERSION_FORMAT_OPTION) && format.is_none() => {
                let value = parser
                    .value()
                    .map_err(|error| parse_error(SPEC.name, error))?;
                let value = value
                    .into_string()
                    .map_err(|_| "version format must be valid UTF-8")?;
                if value != "json" {
                    return Err(format!(
                        "version {} accepts only json",
                        option_display(&VERSION_FORMAT_OPTION)
                    ));
                }
                format = Some(value);
            }
            arg if option_matches(&arg, &VERSION_FORMAT_OPTION) => {
                return Err(format!(
                    "version accepts {} at most once",
                    option_display(&VERSION_FORMAT_OPTION)
                ))
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    if short && format.is_some() {
        return Err("version --short cannot be combined with --format".into());
    }
    Ok(Cli::VersionCommand { short, format })
}
