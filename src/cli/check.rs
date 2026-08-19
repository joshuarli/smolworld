use super::{
    command_help, option_matches, parse_error, parse_file, unexpected, Cli, CommandSpec,
    DEEP_OPTION, FILE_OPTION, HELP_OPTION, VERSION_OPTION,
};
use crate::Result;
use lexopt::Parser;
use std::path::PathBuf;

pub(crate) static SPEC: CommandSpec = CommandSpec {
    name: "check",
    about: "Perform the read-only preflight for a prepared world.",
    options: &[FILE_OPTION, DEEP_OPTION],
    positionals: &[],
    examples: &[
        "smolworld check",
        "smolworld check --deep",
        "smolworld check --file ./world.smolworld",
    ],
    subcommands: &[],
};

pub(crate) fn parse(parser: &mut Parser, mut config: PathBuf) -> Result<Cli> {
    let mut file_seen = false;
    let mut deep = false;
    while let Some(arg) = parser
        .next()
        .map_err(|error| parse_error(SPEC.name, error))?
    {
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => {
                parse_file(parser, SPEC.name, &mut config, &mut file_seen)?
            }
            arg if option_matches(&arg, &DEEP_OPTION) && !deep => deep = true,
            arg if option_matches(&arg, &DEEP_OPTION) => {
                return Err("check accepts --deep at most once".into())
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(command_help(SPEC.name)),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            other => return Err(unexpected(SPEC.name, other)),
        }
    }
    Ok(Cli::Check { config, deep })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_explicit_deep_archive_audit() {
        let config = PathBuf::from("world/.smolworld");
        let parsed = parse(
            &mut Parser::from_iter([
                std::ffi::OsString::from("smolworld"),
                std::ffi::OsString::from("--deep"),
            ]),
            config.clone(),
        )
        .unwrap();
        assert!(matches!(parsed, Cli::Check { config: value, deep: true } if value == config));
    }

    #[test]
    fn rejects_repeated_deep_archive_audit() {
        let parsed = parse(
            &mut Parser::from_iter([
                std::ffi::OsString::from("smolworld"),
                std::ffi::OsString::from("--deep"),
                std::ffi::OsString::from("--deep"),
            ]),
            PathBuf::from(".smolworld"),
        );
        assert!(matches!(parsed, Err(error) if error.contains("at most once")));
    }
}
