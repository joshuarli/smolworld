use super::{
    command_help, option_display, option_matches, parse_error, parse_file, parse_value, unexpected,
    Cli, CommandSpec, CONFIG_FORMAT_OPTION, CONFIG_QUIET_OPTION, FILE_OPTION, HELP_OPTION,
    VERSION_OPTION,
};
use crate::Result;
use lexopt::Parser;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigFormat {
    Yaml,
    Json,
}

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "config",
    about: "Validate and render the resolved authored world configuration.",
    options: &[FILE_OPTION, CONFIG_FORMAT_OPTION, CONFIG_QUIET_OPTION],
    positionals: &[],
    examples: &[
        "smolworld config",
        "smolworld config --format json",
        "smolworld config --quiet",
    ],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, config: PathBuf) -> Result<Cli> {
    parse_with_name(parser, config, SPEC.name)
}

pub(crate) fn parse_with_name(
    parser: &mut Parser,
    mut config: PathBuf,
    command: &'static str,
) -> Result<Cli> {
    let mut file_seen = false;
    let mut format_seen = false;
    let mut format = ConfigFormat::Yaml;
    let mut quiet = false;
    while let Some(arg) = parser.next().map_err(|error| parse_error(command, error))? {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => {
                parse_file(parser, command, &mut config, &mut file_seen)?
            }
            arg if option_matches(&arg, &CONFIG_FORMAT_OPTION) && !format_seen => {
                let value = parse_value(parser, command, &CONFIG_FORMAT_OPTION)?;
                format = match value.to_string_lossy().as_ref() {
                    "yaml" => ConfigFormat::Yaml,
                    "json" => ConfigFormat::Json,
                    other => {
                        return Err(format!(
                            "{command} --format must be yaml or json, got '{other}'"
                        ))
                    }
                };
                format_seen = true;
            }
            arg if option_matches(&arg, &CONFIG_FORMAT_OPTION) => {
                return Err(format!(
                    "{command} accepts {} at most once",
                    option_display(&CONFIG_FORMAT_OPTION)
                ))
            }
            arg if option_matches(&arg, &CONFIG_QUIET_OPTION) && !quiet => quiet = true,
            arg if option_matches(&arg, &CONFIG_QUIET_OPTION) => {
                return Err(format!("{command} accepts --quiet at most once"))
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(command)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            other => return Err(unexpected(command, other)),
        }
    }
    Ok(Cli::Config {
        config,
        format,
        quiet,
    })
}
