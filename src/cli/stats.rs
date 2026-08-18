use super::{
    command_help, option_display, option_matches, os_string, parse_error, parse_file, parse_value,
    unexpected, Cli, CommandSpec, PositionalSpec, ALL_OPTION, FILE_OPTION, FORMAT_OPTION,
    HELP_OPTION, JSON_OPTION, NO_STREAM_OPTION, VERSION_OPTION,
};
use crate::Result;
use lexopt::{Arg, Parser};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatsFormat {
    Table,
    Json,
    Template(String),
}

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "stats",
    about: "Stream host resource observations for exact recorded world services.",
    options: &[
        FILE_OPTION,
        ALL_OPTION,
        NO_STREAM_OPTION,
        FORMAT_OPTION,
        JSON_OPTION,
    ],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: false,
        repeatable: true,
        help: "Declared service to observe",
    }],
    examples: &[
        "smolworld stats",
        "smolworld stats --no-stream --format json",
        "smolworld stats runner --no-stream",
    ],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut all = false;
    let mut no_stream = false;
    let mut format = StatsFormat::Table;
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
            arg if option_matches(&arg, &ALL_OPTION) && !all => all = true,
            arg if option_matches(&arg, &ALL_OPTION) => {
                return Err("stats accepts --all at most once".into())
            }
            arg if option_matches(&arg, &NO_STREAM_OPTION) && !no_stream => no_stream = true,
            arg if option_matches(&arg, &NO_STREAM_OPTION) => {
                return Err("stats accepts --no-stream at most once".into())
            }
            arg if option_matches(&arg, &FORMAT_OPTION) && !format_seen => {
                let value = parse_value(parser, SPEC.name, &FORMAT_OPTION)?;
                format = parse_format(&value.to_string_lossy())?;
                format_seen = true;
            }
            arg if option_matches(&arg, &FORMAT_OPTION) => {
                return Err(format!(
                    "stats accepts {} at most once",
                    option_display(&FORMAT_OPTION)
                ))
            }
            arg if option_matches(&arg, &JSON_OPTION) && !format_seen => {
                format = StatsFormat::Json;
                format_seen = true;
            }
            arg if option_matches(&arg, &JSON_OPTION) => {
                return Err("stats --json cannot be combined with --format or repeated".into())
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            Arg::Value(value) => services.push(os_string(value, SPEC.name, "SERVICE")?),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    Ok(Cli::Stats {
        config,
        services,
        all,
        no_stream,
        format,
    })
}

fn parse_format(value: &str) -> Result<StatsFormat> {
    match value {
        "table" => Ok(StatsFormat::Table),
        "json" => Ok(StatsFormat::Json),
        value if value.contains("{{.") => Ok(StatsFormat::Template(value.to_owned())),
        _ => Err("stats --format must be table, json, or a template containing {{.FIELD}}".into()),
    }
}
