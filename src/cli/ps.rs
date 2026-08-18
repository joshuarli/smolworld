use super::{
    command_help, option_display, option_matches, os_string, parse_error, parse_file, parse_value,
    unexpected, Cli, CommandSpec, LifecycleState, PositionalSpec, ALL_OPTION, FILE_OPTION,
    FILTER_OPTION, FORMAT_OPTION, HELP_OPTION, JSON_OPTION, QUIET_OPTION, SERVICES_OPTION,
    STATUS_OPTION, VERSION_OPTION,
};
use crate::Result;
use lexopt::Parser;
#[cfg(test)]
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PsFormat {
    Table,
    Json,
    Template(String),
}

/// Parsed options for machine visibility.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PsOptions {
    pub(crate) config: PathBuf,
    pub(crate) format: PsFormat,
    pub(crate) services: Vec<String>,
    pub(crate) all: bool,
    pub(crate) status: Option<LifecycleState>,
    pub(crate) quiet: bool,
    pub(crate) services_only: bool,
}

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "ps",
    about: "Show host-visible lifecycle observations for declared services.",
    options: &[
        FILE_OPTION,
        ALL_OPTION,
        STATUS_OPTION,
        FILTER_OPTION,
        FORMAT_OPTION,
        JSON_OPTION,
        QUIET_OPTION,
        SERVICES_OPTION,
    ],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: false,
        repeatable: true,
        help: "Declared service to show",
    }],
    examples: &[
        "smolworld ps",
        "smolworld ps --all --format json",
        "smolworld ps redis",
    ],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut format = PsFormat::Table;
    let mut format_seen = false;
    let mut all = false;
    let mut status = None;
    let mut quiet = false;
    let mut services_only = false;
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
                return Err("ps accepts --all at most once".into())
            }
            arg if option_matches(&arg, &STATUS_OPTION) && status.is_none() => {
                status = Some(parse_status(&parse_value(
                    parser,
                    SPEC.name,
                    &STATUS_OPTION,
                )?)?)
            }
            arg if option_matches(&arg, &STATUS_OPTION) => {
                return Err(format!(
                    "{} accepts {} at most once",
                    SPEC.name,
                    option_display(&STATUS_OPTION)
                ))
            }
            arg if option_matches(&arg, &FILTER_OPTION) => {
                let filter = parse_value(parser, SPEC.name, &FILTER_OPTION)?;
                let Some(value) = filter
                    .to_string_lossy()
                    .strip_prefix("status=")
                    .map(str::to_owned)
                else {
                    return Err("ps --filter supports only status=STATE".into());
                };
                let filtered_status = parse_status(&std::ffi::OsString::from(value))?;
                if status.replace(filtered_status).is_some() {
                    return Err("ps accepts one status filter".into());
                }
            }
            arg if option_matches(&arg, &FORMAT_OPTION) && !format_seen => {
                format = parse_format(
                    &parse_value(parser, SPEC.name, &FORMAT_OPTION)?.to_string_lossy(),
                )?;
                format_seen = true;
            }
            arg if option_matches(&arg, &FORMAT_OPTION) => {
                return Err(format!(
                    "{} accepts {} at most once",
                    SPEC.name,
                    option_display(&FORMAT_OPTION)
                ))
            }
            arg if option_matches(&arg, &JSON_OPTION) && !format_seen => {
                format = PsFormat::Json;
                format_seen = true;
            }
            arg if option_matches(&arg, &JSON_OPTION) => {
                return Err("ps --json cannot be combined with --format or repeated".into())
            }
            arg if option_matches(&arg, &QUIET_OPTION) && !quiet => quiet = true,
            arg if option_matches(&arg, &QUIET_OPTION) => {
                return Err("ps accepts --quiet at most once".into())
            }
            arg if option_matches(&arg, &SERVICES_OPTION) && !services_only => services_only = true,
            arg if option_matches(&arg, &SERVICES_OPTION) => {
                return Err("ps accepts --services at most once".into())
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            lexopt::Arg::Value(value) => services.push(os_string(value, SPEC.name, "SERVICE")?),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    if quiet && services_only {
        return Err("ps --quiet and --services are aliases; use one".into());
    }
    Ok(Cli::Ps {
        config,
        services,
        all,
        status,
        quiet,
        services_only,
        format,
    })
}

fn parse_status(value: &std::ffi::OsString) -> Result<LifecycleState> {
    value
        .to_string_lossy()
        .parse()
        .map_err(|error: String| format!("ps --status: {error}"))
}

fn parse_format(value: &str) -> Result<PsFormat> {
    match value {
        "table" => Ok(PsFormat::Table),
        "json" => Ok(PsFormat::Json),
        value if value.contains("{{.") => Ok(PsFormat::Template(value.to_owned())),
        _ => Err("ps --format must be table, json, or a template containing {{.FIELD}}".into()),
    }
}

#[cfg(test)]
pub(crate) fn parse_ps_options(config: PathBuf, rest: &[OsString]) -> Result<PsOptions> {
    let mut args = Vec::with_capacity(rest.len() + 1);
    args.push(OsString::from("smolworld"));
    args.extend(rest.iter().cloned());
    match parse(&mut Parser::from_iter(args), config)? {
        Cli::Ps {
            config,
            format,
            services,
            all,
            status,
            quiet,
            services_only,
        } => Ok(PsOptions {
            config,
            format,
            services,
            all,
            status,
            quiet,
            services_only,
        }),
        _ => Err("ps did not produce a parsed ps command".into()),
    }
}
