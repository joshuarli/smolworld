use super::{command_help, missing, option_matches, os_string, parse_error, parse_file, unexpected, Cli, CommandSpec, FILE_OPTION, HELP_OPTION, PositionalSpec, VERSION_OPTION};
use crate::Result;
use lexopt::{Arg, Parser};
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "cp",
    about: "Copy one regular file between the host and one recorded machine.",
    options: &[FILE_OPTION],
    positionals: &[
        PositionalSpec {
            name: "SRC",
            required: true,
            repeatable: false,
            help: "Host path or MACHINE:/absolute/path source endpoint",
        },
        PositionalSpec {
            name: "DST",
            required: true,
            repeatable: false,
            help: "Host path or MACHINE:/absolute/path destination endpoint",
        },
    ],
    examples: &[
        "smolworld cp ./input.txt runner:/workspace/input.txt",
        "smolworld cp runner:/workspace/result.txt ./result.txt",
    ],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut operands = Vec::new();
    while let Some(arg) = parser.next().map_err(|error| parse_error(SPEC.name, error))? {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => parse_file(parser, SPEC.name, &mut config, &mut file_seen)?,
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            Arg::Value(value) => operands.push(os_string(value, SPEC.name, "SRC/DST")?),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    if operands.len() != 2 {
        return Err(missing(SPEC.name));
    }
    Ok(Cli::Cp {
        config,
        source: operands.remove(0),
        destination: operands.remove(0),
    })
}
