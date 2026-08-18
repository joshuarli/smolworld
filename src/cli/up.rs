use super::{
    command_help, option_matches, os_string, parse_error, parse_file, unexpected, Cli, CommandSpec,
    PositionalSpec, DETACH_OPTION, FILE_OPTION, HELP_OPTION, VERSION_OPTION,
};
use crate::Result;
use lexopt::{Arg, Parser};
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "up",
    about: "Create and start declared services under the prepared world supervisor.",
    options: &[FILE_OPTION, DETACH_OPTION],
    positionals: &[PositionalSpec {
        name: "SERVICE",
        required: false,
        repeatable: true,
        help: "Declared service to start; dependencies are included",
    }],
    examples: &[
        "smolworld up",
        "smolworld up -d redis",
        "smolworld up --file ./world.smolworld",
    ],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut detach = false;
    let mut services = Vec::new();
    while let Some(arg) = parser
        .next()
        .map_err(|error| parse_error(SPEC.name, error))?
    {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => {
                parse_file(parser, SPEC.name, &mut config, &mut file_seen)?
            }
            arg if option_matches(&arg, &DETACH_OPTION) && !detach => detach = true,
            arg if option_matches(&arg, &DETACH_OPTION) => {
                return Err("up accepts --detach at most once".into())
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            Arg::Value(value) => services.push(os_string(value, SPEC.name, "SERVICE")?),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    Ok(Cli::Up {
        config,
        services,
        detach,
    })
}
