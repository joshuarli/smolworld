use crate::Result;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

/// The machine lifecycle states exposed by `ps`.
///
/// These labels describe host-visible lifecycle observations only. They do
/// not imply that a guest service is ready, healthy, or accepting traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleState {
    Created,
    Attached,
    Running,
    Absent,
}

impl LifecycleState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Attached => "attached",
            Self::Running => "running",
            Self::Absent => "absent",
        }
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LifecycleState {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "created" => Ok(Self::Created),
            "attached" => Ok(Self::Attached),
            "running" => Ok(Self::Running),
            "absent" => Ok(Self::Absent),
            other => Err(format!("unknown lifecycle state '{other}'")),
        }
    }
}

/// One row in machine visibility output.
///
/// The strings are already presentation-ready so this type stays independent
/// from the world model and can be filled by a future runtime status adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineStatus {
    pub(crate) machine: String,
    pub(crate) ip: String,
    pub(crate) mac: String,
    pub(crate) state: LifecycleState,
}

impl MachineStatus {
    pub(crate) fn new(
        machine: impl Into<String>,
        ip: impl Into<String>,
        mac: impl Into<String>,
        state: LifecycleState,
    ) -> Self {
        Self {
            machine: machine.into(),
            ip: ip.into(),
            mac: mac.into(),
            state,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PsFormat {
    Table,
    Json,
}

/// Parsed options for machine visibility.
///
/// The parser keeps the format choice together with the selected config so
/// `Cli::Ps` and `runtime::ps` cannot silently disagree about output mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PsOptions {
    pub(crate) config: PathBuf,
    pub(crate) format: PsFormat,
}

pub(crate) enum Cli {
    Help,
    Up {
        config: PathBuf,
    },
    Check {
        config: PathBuf,
    },
    Prepare {
        config: PathBuf,
    },
    Down {
        config: PathBuf,
    },
    Ps {
        config: PathBuf,
        format: PsFormat,
    },
    Exec {
        config: PathBuf,
        machine: String,
        command: Vec<String>,
    },
    Cp {
        config: PathBuf,
        source: String,
        destination: String,
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
            "prepare" => {
                return command_config("prepare", config, &args[index + 1..])
                    .map(|config| Cli::Prepare { config })
            }
            "down" => {
                return command_config("down", config, &args[index + 1..])
                    .map(|config| Cli::Down { config })
            }
            "ps" => {
                let options = parse_ps_options(config, &args[index + 1..])?;
                return Ok(Cli::Ps {
                    config: options.config,
                    format: options.format,
                });
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
            "cp" => {
                let (config, source, destination) = parse_cp_options(config, &args[index + 1..])?;
                return Ok(Cli::Cp {
                    config,
                    source,
                    destination,
                });
            }
            "-h" | "--help" => return Ok(Cli::Help),
            other => return Err(format!("unknown command or option '{other}'\n{}", usage())),
        }
    }
    Err(usage().into())
}

/// Parse one explicit file transfer. A `machine:/absolute/path` endpoint is
/// interpreted by the runtime only after the world state has resolved that
/// logical machine to its recorded smolvm name.
fn parse_cp_options(config: PathBuf, rest: &[String]) -> Result<(PathBuf, String, String)> {
    match rest {
        [flag, path, source, destination] if flag == "-f" || flag == "--file" => {
            Ok((PathBuf::from(path), source.clone(), destination.clone()))
        }
        [source, destination] => Ok((config, source.clone(), destination.clone())),
        _ => Err("usage: smolworld cp [-f PATH] SRC DST".into()),
    }
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

/// Parse the options following `ps` while retaining the format choice for the
/// future runtime status adapter.
pub(crate) fn parse_ps_options(config: PathBuf, rest: &[String]) -> Result<PsOptions> {
    let mut config = config;
    let mut format = PsFormat::Table;
    let mut file_seen = false;
    let mut index = 0;

    while index < rest.len() {
        match rest[index].as_str() {
            "-f" | "--file" => {
                if file_seen {
                    return Err("ps accepts -f/--file at most once".into());
                }
                let Some(path) = rest.get(index + 1) else {
                    return Err("ps -f/--file requires a path".into());
                };
                config = PathBuf::from(path);
                file_seen = true;
                index += 2;
            }
            "--json" => {
                if format == PsFormat::Json {
                    return Err("ps accepts --json at most once".into());
                }
                format = PsFormat::Json;
                index += 1;
            }
            other => return Err(format!("unknown ps option '{other}'\n{}", ps_usage())),
        }
    }

    Ok(PsOptions { config, format })
}

pub(crate) fn ps_usage() -> &'static str {
    "usage: smolworld ps [-f PATH] [--json]"
}

/// Format machine rows without adding a trailing newline.
pub(crate) fn format_ps(format: PsFormat, machines: &[MachineStatus]) -> String {
    match format {
        PsFormat::Table => format_ps_table(machines),
        PsFormat::Json => format_ps_json(machines),
    }
}

pub(crate) fn format_ps_table(machines: &[MachineStatus]) -> String {
    let mut output = String::from("MACHINE\tIP\tMAC\tSTATUS");
    for machine in machines {
        output.push('\n');
        output.push_str(&machine.machine);
        output.push('\t');
        output.push_str(&machine.ip);
        output.push('\t');
        output.push_str(&machine.mac);
        output.push('\t');
        output.push_str(machine.state.as_str());
    }
    output
}

pub(crate) fn format_ps_json(machines: &[MachineStatus]) -> String {
    let mut output = String::from("[");
    for (index, machine) in machines.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str("{\"machine\":");
        push_json_string(&mut output, &machine.machine);
        output.push_str(",\"ip\":");
        push_json_string(&mut output, &machine.ip);
        output.push_str(",\"mac\":");
        push_json_string(&mut output, &machine.mac);
        output.push_str(",\"status\":");
        push_json_string(&mut output, machine.state.as_str());
        output.push('}');
    }
    output.push(']');
    output
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

pub(crate) fn usage() -> &'static str {
    "usage: smolworld [-f .smolworld] <check|prepare|up|down|ps>\n       smolworld ps [-f PATH] [--json]\n       smolworld [-f .smolworld] exec MACHINE -- COMMAND [ARG ...]\n       smolworld cp [-f PATH] SRC DST"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_file_flag_before_or_after_command() {
        assert!(matches!(
            parse_cli(vec!["-f".into(), "demo".into(), "ps".into()]).unwrap(),
            Cli::Ps { config, format: PsFormat::Table } if config == PathBuf::from("demo")
        ));
        assert!(matches!(
            parse_cli(vec!["ps".into(), "--file".into(), "demo".into()]).unwrap(),
            Cli::Ps { config, format: PsFormat::Table } if config == PathBuf::from("demo")
        ));
    }

    #[test]
    fn parses_prepare_with_file_flag_before_or_after_command() {
        assert!(matches!(
            parse_cli(vec!["-f".into(), "demo".into(), "prepare".into()]).unwrap(),
            Cli::Prepare { config } if config == PathBuf::from("demo")
        ));
        assert!(matches!(
            parse_cli(vec!["prepare".into(), "--file".into(), "demo".into()]).unwrap(),
            Cli::Prepare { config } if config == PathBuf::from("demo")
        ));
        assert!(matches!(
            parse_cli(vec!["prepare".into()]).unwrap(),
            Cli::Prepare { config } if config == PathBuf::from(".smolworld")
        ));
    }

    #[test]
    fn rejects_invalid_prepare_options() {
        assert_eq!(
            parse_cli(vec!["prepare".into(), "--file".into()])
                .err()
                .unwrap(),
            "usage: smolworld prepare [-f PATH]"
        );
        assert_eq!(
            parse_cli(vec!["prepare".into(), "extra".into()])
                .err()
                .unwrap(),
            "usage: smolworld prepare [-f PATH]"
        );
    }

    #[test]
    fn parses_world_scoped_copy_endpoints() {
        assert!(matches!(
            parse_cli(vec![
                "cp".into(),
                "host-input.tar".into(),
                "runner:/workspace/input.tar".into(),
            ])
            .unwrap(),
            Cli::Cp { config, source, destination }
                if config == PathBuf::from(".smolworld")
                    && source == "host-input.tar"
                    && destination == "runner:/workspace/input.tar"
        ));
        assert!(matches!(
            parse_cli(vec![
                "cp".into(),
                "--file".into(),
                "world.smolworld".into(),
                "runner:/workspace/result.txt".into(),
                "host-result.txt".into(),
            ])
            .unwrap(),
            Cli::Cp { config, source, destination }
                if config == PathBuf::from("world.smolworld")
                    && source == "runner:/workspace/result.txt"
                    && destination == "host-result.txt"
        ));
        assert_eq!(
            parse_cli(vec!["cp".into(), "only-one-operand".into()])
                .err()
                .expect("copy invocation is invalid"),
            "usage: smolworld cp [-f PATH] SRC DST"
        );
    }

    #[test]
    fn parses_ps_json_and_file_in_either_order() {
        let options = parse_ps_options(
            PathBuf::from("default.world"),
            &["--json".into(), "--file".into(), "custom.world".into()],
        )
        .unwrap();
        assert_eq!(options.config, PathBuf::from("custom.world"));
        assert_eq!(options.format, PsFormat::Json);

        let options = parse_ps_options(
            PathBuf::from("default.world"),
            &["-f".into(), "custom.world".into(), "--json".into()],
        )
        .unwrap();
        assert_eq!(options.config, PathBuf::from("custom.world"));
        assert_eq!(options.format, PsFormat::Json);
    }

    #[test]
    fn rejects_invalid_ps_options() {
        assert_eq!(
            parse_ps_options(PathBuf::from("world"), &["--json".into(), "--json".into()])
                .unwrap_err(),
            "ps accepts --json at most once"
        );
        assert_eq!(
            parse_ps_options(PathBuf::from("world"), &["--file".into()]).unwrap_err(),
            "ps -f/--file requires a path"
        );
        assert!(parse_ps_options(PathBuf::from("world"), &["--wat".into()])
            .unwrap_err()
            .contains("unknown ps option '--wat'"));
    }

    #[test]
    fn lifecycle_state_labels_are_closed_and_stable() {
        let states = [
            LifecycleState::Created,
            LifecycleState::Attached,
            LifecycleState::Running,
            LifecycleState::Absent,
        ];
        let labels: Vec<_> = states.iter().map(|state| state.as_str()).collect();
        assert_eq!(labels, ["created", "attached", "running", "absent"]);
        for state in states {
            assert_eq!(state.as_str().parse::<LifecycleState>().unwrap(), state);
        }
        assert_eq!(
            "broken".parse::<LifecycleState>().unwrap_err(),
            "unknown lifecycle state 'broken'"
        );
    }

    fn machines() -> Vec<MachineStatus> {
        vec![
            MachineStatus::new(
                "api",
                "10.77.0.2",
                "02:00:00:00:00:02",
                LifecycleState::Attached,
            ),
            MachineStatus::new(
                "worker",
                "10.77.0.3",
                "02:00:00:00:00:03",
                LifecycleState::Absent,
            ),
        ]
    }

    #[test]
    fn formats_table_with_lifecycle_labels() {
        assert_eq!(
            format_ps_table(&machines()),
            "MACHINE\tIP\tMAC\tSTATUS\napi\t10.77.0.2\t02:00:00:00:00:02\tattached\nworker\t10.77.0.3\t02:00:00:00:00:03\tabsent"
        );
    }

    #[test]
    fn formats_json_as_a_deterministic_array_and_escapes_strings() {
        assert_eq!(
            format_ps_json(&machines()),
            "[{\"machine\":\"api\",\"ip\":\"10.77.0.2\",\"mac\":\"02:00:00:00:00:02\",\"status\":\"attached\"},{\"machine\":\"worker\",\"ip\":\"10.77.0.3\",\"mac\":\"02:00:00:00:00:03\",\"status\":\"absent\"}]"
        );
        let escaped = [MachineStatus::new(
            "a\"b",
            "line\nvalue",
            "slash\\value",
            LifecycleState::Created,
        )];
        assert_eq!(
            format_ps(PsFormat::Json, &escaped),
            "[{\"machine\":\"a\\\"b\",\"ip\":\"line\\nvalue\",\"mac\":\"slash\\\\value\",\"status\":\"created\"}]"
        );
    }
}
