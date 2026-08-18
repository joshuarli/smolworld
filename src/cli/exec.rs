use super::{
    command_help, missing, option_matches, os_string, parse_error, parse_file, parse_value,
    render_help, unexpected, Cli, CommandSpec, PositionalSpec, ENV_OPTION, EXEC_DETACH_OPTION,
    FILE_OPTION, HELP_OPTION, INTERACTIVE_OPTION, SECRET_ENV_OPTION, SECRET_FILE_OPTION,
    STREAM_OPTION, TIMEOUT_OPTION, TTY_OPTION, VERSION_OPTION, WORKDIR_OPTION,
};
use crate::Result;
use lexopt::{Arg, Parser};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// Options passed verbatim through the narrow `smolvm machine exec` adapter.
/// They remain invocation-local and are never written into world state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ExecOptions {
    pub(crate) env: Vec<OsString>,
    pub(crate) secret_env: Vec<OsString>,
    pub(crate) secret_file: Vec<OsString>,
    pub(crate) workdir: Option<OsString>,
    pub(crate) interactive: bool,
    pub(crate) tty: bool,
    pub(crate) stream: bool,
    pub(crate) detach: bool,
    pub(crate) timeout: Option<OsString>,
}

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "exec",
    about: "Delegate one command to a named, running world service.",
    options: &[
        FILE_OPTION,
        ENV_OPTION,
        WORKDIR_OPTION,
        INTERACTIVE_OPTION,
        TTY_OPTION,
        STREAM_OPTION,
        EXEC_DETACH_OPTION,
        TIMEOUT_OPTION,
        SECRET_ENV_OPTION,
        SECRET_FILE_OPTION,
    ],
    positionals: &[
        PositionalSpec {
            name: "SERVICE",
            required: true,
            repeatable: false,
            help: "Logical service name from the authored world",
        },
        PositionalSpec {
            name: "COMMAND [ARG ...]",
            required: true,
            repeatable: true,
            help: "Guest command and its arguments; -- remains accepted before COMMAND",
        },
    ],
    examples: &[
        "smolworld exec runner /usr/local/bin/run-task --once",
        "smolworld exec -it runner /bin/sh",
        "smolworld exec -e MODE=check runner /usr/local/bin/run-task",
    ],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    // Command argv starts after SERVICE. Parse the option prefix separately so
    // guest flags are opaque and cannot be mistaken for smolworld flags.
    let raw: Vec<OsString> = parser
        .raw_args()
        .map_err(|error| parse_error(SPEC.name, error))?
        .collect();
    let mut prefix = Parser::from_args(raw.iter().cloned());
    let mut file_seen = false;
    let mut options = ExecOptions::default();
    let (service, mut command) = loop {
        let Some(arg) = prefix
            .next()
            .map_err(|error| parse_error(SPEC.name, error))?
        else {
            return Err(missing(SPEC.name));
        };
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => {
                parse_file(&mut prefix, SPEC.name, &mut config, &mut file_seen)?
            }
            arg if option_matches(&arg, &ENV_OPTION) => {
                options
                    .env
                    .push(parse_value(&mut prefix, SPEC.name, &ENV_OPTION)?)
            }
            arg if option_matches(&arg, &WORKDIR_OPTION) && options.workdir.is_none() => {
                options.workdir = Some(parse_value(&mut prefix, SPEC.name, &WORKDIR_OPTION)?)
            }
            arg if option_matches(&arg, &WORKDIR_OPTION) => {
                return Err("exec accepts --workdir at most once".into())
            }
            arg if option_matches(&arg, &INTERACTIVE_OPTION) && !options.interactive => {
                options.interactive = true
            }
            arg if option_matches(&arg, &INTERACTIVE_OPTION) => {
                return Err("exec accepts --interactive at most once".into())
            }
            arg if option_matches(&arg, &TTY_OPTION) && !options.tty => options.tty = true,
            arg if option_matches(&arg, &TTY_OPTION) => {
                return Err("exec accepts --tty at most once".into())
            }
            arg if option_matches(&arg, &STREAM_OPTION) && !options.stream => options.stream = true,
            arg if option_matches(&arg, &STREAM_OPTION) => {
                return Err("exec accepts --stream at most once".into())
            }
            arg if option_matches(&arg, &EXEC_DETACH_OPTION) && !options.detach => {
                options.detach = true
            }
            arg if option_matches(&arg, &EXEC_DETACH_OPTION) => {
                return Err("exec accepts --detach at most once".into())
            }
            arg if option_matches(&arg, &TIMEOUT_OPTION) && options.timeout.is_none() => {
                options.timeout = Some(parse_value(&mut prefix, SPEC.name, &TIMEOUT_OPTION)?)
            }
            arg if option_matches(&arg, &TIMEOUT_OPTION) => {
                return Err("exec accepts --timeout at most once".into())
            }
            arg if option_matches(&arg, &SECRET_ENV_OPTION) => options
                .secret_env
                .push(parse_value(&mut prefix, SPEC.name, &SECRET_ENV_OPTION)?),
            arg if option_matches(&arg, &SECRET_FILE_OPTION) => options
                .secret_file
                .push(parse_value(&mut prefix, SPEC.name, &SECRET_FILE_OPTION)?),
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            Arg::Value(value) => {
                let service = os_string(value, SPEC.name, "SERVICE")?;
                let command: Vec<OsString> = prefix
                    .raw_args()
                    .map_err(|error| parse_error(SPEC.name, error))?
                    .collect();
                break (service, command);
            }
            other => return Err(unexpected(SPEC.name, other)),
        }
    };

    // Every argument following SERVICE is opaque guest argv. This prevents a
    // guest flag such as `--timeout` from being silently consumed as a host
    // option. `--` remains an optional visual separator for shell scripts.
    if command
        .first()
        .is_some_and(|value| value == OsStr::new("--"))
    {
        command.remove(0);
    }
    if command.is_empty() {
        return Err(format!(
            "exec requires COMMAND after SERVICE\n\n{}",
            render_help(Some(SPEC.name))
        ));
    }
    if options.detach && (options.interactive || options.tty || options.stream) {
        return Err(
            "exec --detach cannot be combined with --interactive, --tty, or --stream".into(),
        );
    }
    Ok(Cli::Exec {
        config,
        service,
        options,
        command,
    })
}
