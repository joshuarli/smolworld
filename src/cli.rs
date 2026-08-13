use crate::Result;
use std::path::PathBuf;

pub(crate) enum Cli {
    Help,
    Up {
        config: PathBuf,
    },
    Check {
        config: PathBuf,
    },
    Down {
        config: PathBuf,
    },
    Ps {
        config: PathBuf,
    },
    Exec {
        config: PathBuf,
        machine: String,
        command: Vec<String>,
    },
}

pub(crate) fn parse_cli(args: Vec<String>) -> Result<Cli> {
    let mut config = PathBuf::from(".smolworld");
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-f" | "--file" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("-f/--file requires a path".into());
                };
                config = PathBuf::from(value);
                index += 2;
            }
            "up" => {
                return command_config("up", config, &args[index + 1..])
                    .map(|config| Cli::Up { config })
            }
            "check" => {
                return command_config("check", config, &args[index + 1..])
                    .map(|config| Cli::Check { config })
            }
            "down" => {
                return command_config("down", config, &args[index + 1..])
                    .map(|config| Cli::Down { config })
            }
            "ps" => {
                return command_config("ps", config, &args[index + 1..])
                    .map(|config| Cli::Ps { config })
            }
            "exec" => {
                let mut rest = &args[index + 1..];
                if rest.first().map(String::as_str) == Some("-f")
                    || rest.first().map(String::as_str) == Some("--file")
                {
                    let Some(path) = rest.get(1) else {
                        return Err("-f/--file requires a path".into());
                    };
                    config = PathBuf::from(path);
                    rest = &rest[2..];
                }
                let Some(machine) = rest.first() else {
                    return Err(
                        "usage: smolworld exec [-f PATH] MACHINE -- COMMAND [ARG ...]".into(),
                    );
                };
                if rest.get(1).map(String::as_str) != Some("--") {
                    return Err("smolworld exec requires -- before COMMAND".into());
                }
                let command = rest[2..].to_vec();
                if command.is_empty() {
                    return Err("smolworld exec requires a command".into());
                }
                return Ok(Cli::Exec {
                    config,
                    machine: machine.clone(),
                    command,
                });
            }
            "-h" | "--help" => return Ok(Cli::Help),
            other => return Err(format!("unknown command or option '{other}'\n{}", usage())),
        }
    }
    Err(usage().into())
}

pub(crate) fn command_config(command: &str, config: PathBuf, rest: &[String]) -> Result<PathBuf> {
    if rest.is_empty() {
        return Ok(config);
    } else {
        let file = rest
            .first()
            .is_some_and(|value| value == "-f" || value == "--file");
        if file && rest.len() == 2 {
            return Ok(PathBuf::from(&rest[1]));
        }
    }
    Err(format!("usage: smolworld {command} [-f PATH]"))
}

pub(crate) fn usage() -> &'static str {
    "usage: smolworld [-f .smolworld] <check|up|down|ps>\n       smolworld [-f .smolworld] exec MACHINE -- COMMAND [ARG ...]"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_file_flag_before_or_after_command() {
        assert!(matches!(
            parse_cli(vec!["-f".into(), "demo".into(), "ps".into()]).unwrap(),
            Cli::Ps { config } if config == PathBuf::from("demo")
        ));
        assert!(matches!(
            parse_cli(vec!["ps".into(), "--file".into(), "demo".into()]).unwrap(),
            Cli::Ps { config } if config == PathBuf::from("demo")
        ));
    }
}
