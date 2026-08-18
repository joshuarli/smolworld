use super::{
    command_help, option_matches, os_string, parse_error, parse_file, unexpected, Cli, CommandSpec,
    PositionalSpec, FILE_OPTION, HELP_OPTION, VERSION_OPTION,
};
use crate::Result;
use lexopt::{Arg, Parser};
use std::path::PathBuf;

pub(crate) static CREATE_SPEC: CommandSpec = CommandSpec {
    name: "create",
    about: "Create recorded service machine configurations without starting them.",
    options: &[FILE_OPTION],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: false,
        repeatable: true,
        help: "Declared service to create; dependencies are included",
    }],
    examples: &["smolworld create", "smolworld create redis"],
    subcommands: &[],
};

pub(crate) static START_SPEC: CommandSpec = CommandSpec {
    name: "start",
    about: "Start created or stopped services without deleting their recorded identity.",
    options: &[FILE_OPTION],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: false,
        repeatable: true,
        help: "Declared service to start",
    }],
    examples: &["smolworld start", "smolworld start redis runner"],
    subcommands: &[],
};

pub(crate) static STOP_SPEC: CommandSpec = CommandSpec {
    name: "stop",
    about: "Gracefully stop running services while retaining their machine records.",
    options: &[FILE_OPTION],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: false,
        repeatable: true,
        help: "Declared service to stop",
    }],
    examples: &["smolworld stop", "smolworld stop runner"],
    subcommands: &[],
};

pub(crate) static RESTART_SPEC: CommandSpec = CommandSpec {
    name: "restart",
    about: "Stop and then restart running services through the world supervisor.",
    options: &[FILE_OPTION],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: false,
        repeatable: true,
        help: "Declared service to restart",
    }],
    examples: &["smolworld restart", "smolworld restart redis"],
    subcommands: &[],
};

pub(crate) static RM_SPEC: CommandSpec = CommandSpec {
    name: "rm",
    about: "Delete stopped service records through the owning world supervisor.",
    options: &[FILE_OPTION],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: true,
        repeatable: true,
        help: "Declared stopped service to remove",
    }],
    examples: &["smolworld rm runner"],
    subcommands: &[],
};

pub(crate) fn parse_create(parser: &mut Parser, config: PathBuf) -> Result<Cli> {
    parse_services(parser, config, &CREATE_SPEC, |config, services| {
        Cli::Create { config, services }
    })
}

pub(crate) fn parse_start(parser: &mut Parser, config: PathBuf) -> Result<Cli> {
    parse_services(parser, config, &START_SPEC, |config, services| Cli::Start {
        config,
        services,
    })
}

pub(crate) fn parse_stop(parser: &mut Parser, config: PathBuf) -> Result<Cli> {
    parse_services(parser, config, &STOP_SPEC, |config, services| Cli::Stop {
        config,
        services,
    })
}

pub(crate) fn parse_restart(parser: &mut Parser, config: PathBuf) -> Result<Cli> {
    parse_services(parser, config, &RESTART_SPEC, |config, services| {
        Cli::Restart { config, services }
    })
}

pub(crate) fn parse_rm(parser: &mut Parser, config: PathBuf) -> Result<Cli> {
    let command = parse_services(parser, config, &RM_SPEC, |config, services| Cli::Rm {
        config,
        services,
    })?;
    match &command {
        Cli::Rm { services, .. } if services.is_empty() => {
            Err("rm requires at least one SERVICE".into())
        }
        _ => Ok(command),
    }
}

fn parse_services(
    parser: &mut Parser,
    mut config: PathBuf,
    spec: &'static CommandSpec,
    command: impl FnOnce(PathBuf, Vec<String>) -> Cli,
) -> Result<Cli> {
    let mut file_seen = false;
    let mut services = Vec::new();
    while let Some(arg) = parser
        .next()
        .map_err(|error| parse_error(spec.name, error))?
    {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => {
                parse_file(parser, spec.name, &mut config, &mut file_seen)?
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(spec.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            Arg::Value(value) => services.push(os_string(value, spec.name, "SERVICE")?),
            other => return Err(unexpected(spec.name, other)),
        }
    }
    Ok(command(config, services))
}
