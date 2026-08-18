use super::{
    command_help, option_display, option_matches, os_string, parse_error, parse_file, parse_value,
    unexpected, Cli, CommandSpec, PositionalSpec, FILE_OPTION, HELP_OPTION, IMAGES_FORMAT_OPTION,
    VERSION_OPTION,
};
use crate::Result;
use lexopt::{Arg, Parser};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImagesFormat {
    Table,
    Json,
}

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "images",
    about: "Show sealed source material for declared services without starting them.",
    options: &[FILE_OPTION, IMAGES_FORMAT_OPTION],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: false,
        repeatable: true,
        help: "Declared service whose sealed material to show",
    }],
    examples: &["smolworld images", "smolworld images --format json runner"],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut format = ImagesFormat::Table;
    let mut format_seen = false;
    let mut services = Vec::new();
    while let Some(arg) = parser
        .next()
        .map_err(|error| parse_error(SPEC.name, error))?
    {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => {
                parse_file(parser, SPEC.name, &mut config, &mut file_seen)?
            }
            arg if option_matches(&arg, &IMAGES_FORMAT_OPTION) && !format_seen => {
                format = match parse_value(parser, SPEC.name, &IMAGES_FORMAT_OPTION)?
                    .to_string_lossy()
                    .as_ref()
                {
                    "table" => ImagesFormat::Table,
                    "json" => ImagesFormat::Json,
                    other => {
                        return Err(format!(
                            "images --format must be table or json, got '{other}'"
                        ))
                    }
                };
                format_seen = true;
            }
            arg if option_matches(&arg, &IMAGES_FORMAT_OPTION) => {
                return Err(format!(
                    "images accepts {} at most once",
                    option_display(&IMAGES_FORMAT_OPTION)
                ))
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            Arg::Value(value) => services.push(os_string(value, SPEC.name, "SERVICE")?),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    Ok(Cli::Images {
        config,
        services,
        format,
    })
}
