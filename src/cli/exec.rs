use super::{command_help, missing, option_matches, os_string, parse_error, parse_file, parse_value, render_help, unexpected, Cli, CommandSpec, FILE_OPTION, HELP_OPTION, PositionalSpec, SECRET_ENV_OPTION, VERSION_OPTION};
use crate::Result;
use lexopt::{Arg, Parser};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "exec",
    about: "Delegate one command to a named, running world machine.",
    options: &[FILE_OPTION, SECRET_ENV_OPTION],
    positionals: &[
        PositionalSpec {
            name: "MACHINE",
            required: true,
            repeatable: false,
            help: "Logical machine name from the authored world",
        },
        PositionalSpec {
            name: "COMMAND [ARG ...]",
            required: true,
            repeatable: false,
            help: "Guest command and its arguments after the required -- separator",
        },
    ],
    examples: &["smolworld exec runner -- /usr/local/bin/run-task --once"],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    // lexopt intentionally consumes `--` as parser state instead of yielding
    // a marker. Capture the remaining span only to locate that boundary; the
    // structured prefix is parsed by a fresh lexopt parser and the tail stays
    // opaque guest argv.
    let raw: Vec<OsString> = parser
        .raw_args()
        .map_err(|error| parse_error(SPEC.name, error))?
        .collect();
    let Some(separator) = raw.iter().position(|value| value == OsStr::new("--")) else {
        return Err(format!("{} requires -- before COMMAND\n\n{}", SPEC.name, render_help(Some(SPEC.name))));
    };

    let mut options = Parser::from_args(raw[..separator].iter().cloned());
    let machine = loop {
        let Some(arg) = options.next().map_err(|error| parse_error(SPEC.name, error))? else {
            return Err(missing(SPEC.name));
        };
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => parse_file(&mut options, SPEC.name, &mut config, &mut file_seen)?,
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            Arg::Value(value) => break os_string(value, SPEC.name, "MACHINE")?,
            other => return Err(unexpected(SPEC.name, other)),
        }
    };

    let mut secret_env = Vec::new();
    while let Some(arg) = options.next().map_err(|error| parse_error(SPEC.name, error))? {
        match arg {
            arg if option_matches(&arg, &SECRET_ENV_OPTION) => {
                secret_env.push(parse_value(&mut options, SPEC.name, &SECRET_ENV_OPTION)?);
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }

    let command = raw[separator + 1..].to_vec();
    if command.is_empty() {
        return Err(format!("{} requires a command after --", SPEC.name));
    }

    Ok(Cli::Exec {
        config,
        machine,
        secret_env,
        command,
    })
}
