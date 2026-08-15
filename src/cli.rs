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
    Capturing,
    Captured,
    Absent,
}

impl LifecycleState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Attached => "attached",
            Self::Running => "running",
            Self::Capturing => "capturing",
            Self::Captured => "captured",
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
            "capturing" => Ok(Self::Capturing),
            "captured" => Ok(Self::Captured),
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
    /// Ask the running supervisor to capture every world machine into one
    /// checkpoint root. This command never guesses at a switch/runtime from a
    /// second process; it talks to the owner through its private control socket.
    Checkpoint {
        config: PathBuf,
        output: PathBuf,
    },
    /// Start a new supervisor around a previously captured world receipt.
    Restore {
        config: PathBuf,
        checkpoint: PathBuf,
    },
    /// Irreversibly release one retained checkpoint and its exact source VMs.
    Release {
        config: PathBuf,
        checkpoint: PathBuf,
    },
    Down {
        config: PathBuf,
    },
    Ps {
        config: PathBuf,
        format: PsFormat,
    },
    /// Collect host-side metrics for the recorded v2 world machines.
    Metrics {
        config: PathBuf,
    },
    Exec {
        config: PathBuf,
        machine: String,
        secret_env: Vec<String>,
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
            "checkpoint" => {
                let (config, output) = parse_checkpoint_options(config, &args[index + 1..])?;
                return Ok(Cli::Checkpoint { config, output });
            }
            "restore" => {
                let (config, checkpoint) = parse_restore_options(config, &args[index + 1..])?;
                return Ok(Cli::Restore { config, checkpoint });
            }
            "release" => {
                let (config, checkpoint) = parse_release_options(config, &args[index + 1..])?;
                return Ok(Cli::Release { config, checkpoint });
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
            "metrics" => {
                let config = parse_metrics_options(config, &args[index + 1..])?;
                return Ok(Cli::Metrics { config });
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
                        "usage: smolworld exec [-f PATH] MACHINE [--secret-env GUEST=HOST_ENV]... -- COMMAND [ARG ...]".into(),
                    );
                };
                rest = &rest[1..];
                let mut secret_env = Vec::new();
                while rest.first().map(String::as_str) != Some("--") {
                    if rest.first().map(String::as_str) != Some("--secret-env") {
                        return Err(
                            "smolworld exec accepts only --secret-env before -- COMMAND".into()
                        );
                    }
                    let Some(value) = rest.get(1) else {
                        return Err("smolworld exec --secret-env requires GUEST=HOST_ENV".into());
                    };
                    secret_env.push(value.clone());
                    rest = &rest[2..];
                    if rest.is_empty() {
                        return Err("smolworld exec requires -- before COMMAND".into());
                    }
                }
                let command = rest[1..].to_vec();
                if command.is_empty() {
                    return Err("smolworld exec requires a command".into());
                }
                return Ok(Cli::Exec {
                    config,
                    machine: machine.clone(),
                    secret_env,
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

fn parse_checkpoint_options(config: PathBuf, rest: &[String]) -> Result<(PathBuf, PathBuf)> {
    parse_world_path_option("checkpoint", config, rest, "--output")
}

fn parse_restore_options(config: PathBuf, rest: &[String]) -> Result<(PathBuf, PathBuf)> {
    parse_world_path_option("restore", config, rest, "--checkpoint")
}

fn parse_release_options(config: PathBuf, rest: &[String]) -> Result<(PathBuf, PathBuf)> {
    parse_world_path_option("release", config, rest, "--checkpoint")
}

fn parse_world_path_option(
    command: &str,
    mut config: PathBuf,
    rest: &[String],
    path_flag: &str,
) -> Result<(PathBuf, PathBuf)> {
    let mut path = None;
    let mut file_seen = false;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "-f" | "--file" if !file_seen => {
                let Some(value) = rest.get(index + 1) else {
                    return Err(format!("{command} -f/--file requires a path"));
                };
                config = PathBuf::from(value);
                file_seen = true;
                index += 2;
            }
            flag if flag == path_flag && path.is_none() => {
                let Some(value) = rest.get(index + 1) else {
                    return Err(format!("{command} {path_flag} requires a directory"));
                };
                path = Some(PathBuf::from(value));
                index += 2;
            }
            other => {
                return Err(format!(
                    "unknown {command} option '{other}'\n{}",
                    world_path_usage(command, path_flag)
                ));
            }
        }
    }
    path.map(|path| (config, path))
        .ok_or_else(|| world_path_usage(command, path_flag))
}

fn world_path_usage(command: &str, path_flag: &str) -> String {
    format!("usage: smolworld {command} [-f PATH] {path_flag} DIR")
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

/// Parse the closed metrics command. The JSON flag is explicit so callers do
/// not accidentally consume a future human-readable presentation as a stable
/// machine contract.
pub(crate) fn parse_metrics_options(config: PathBuf, rest: &[String]) -> Result<PathBuf> {
    let mut config = config;
    let mut file_seen = false;
    let mut json_seen = false;
    let mut index = 0;

    while index < rest.len() {
        match rest[index].as_str() {
            "-f" | "--file" => {
                if file_seen {
                    return Err("metrics accepts -f/--file at most once".into());
                }
                let Some(path) = rest.get(index + 1) else {
                    return Err("metrics -f/--file requires a path".into());
                };
                config = PathBuf::from(path);
                file_seen = true;
                index += 2;
            }
            "--json" => {
                if json_seen {
                    return Err("metrics accepts --json at most once".into());
                }
                json_seen = true;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown metrics option '{other}'\n{}",
                    metrics_usage()
                ))
            }
        }
    }

    if json_seen {
        Ok(config)
    } else {
        Err(metrics_usage().into())
    }
}

pub(crate) fn metrics_usage() -> &'static str {
    "usage: smolworld metrics [-f PATH] --json"
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

/// One row in the closed `metrics --json` world schema.
///
/// `None` is rendered as JSON `null`; the field set is intentionally fixed so
/// consumers can distinguish an absent/unallocated machine from a machine
/// whose observation is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineMetrics {
    pub(crate) machine: String,
    pub(crate) smolvm_name: Option<String>,
    pub(crate) state: String,
    pub(crate) pid: Option<i32>,
    pub(crate) cpus: Option<u8>,
    pub(crate) memory_mb: Option<u32>,
    pub(crate) storage_gb: Option<u64>,
    pub(crate) overlay_gb: Option<u64>,
    pub(crate) cpu_seconds: Option<u64>,
    pub(crate) cpu_millis: Option<u64>,
    pub(crate) rss_mb: Option<u64>,
    pub(crate) disk_used_mb: Option<u64>,
}

pub(crate) fn format_metrics_json(world: &str, machines: &[MachineMetrics]) -> String {
    let mut output = String::from("{\"schemaVersion\":1,\"world\":");
    push_json_string(&mut output, world);
    output.push_str(",\"machines\":[");
    for (index, machine) in machines.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        output.push_str("{\"machine\":");
        push_json_string(&mut output, &machine.machine);
        output.push_str(",\"smolvmName\":");
        push_json_optional_string(&mut output, machine.smolvm_name.as_deref());
        output.push_str(",\"state\":");
        push_json_string(&mut output, &machine.state);
        output.push_str(",\"pid\":");
        push_json_optional_i32(&mut output, machine.pid);
        output.push_str(",\"cpus\":");
        push_json_optional_u64(&mut output, machine.cpus.map(u64::from));
        output.push_str(",\"memoryMb\":");
        push_json_optional_u64(&mut output, machine.memory_mb.map(u64::from));
        output.push_str(",\"storageGb\":");
        push_json_optional_u64(&mut output, machine.storage_gb);
        output.push_str(",\"overlayGb\":");
        push_json_optional_u64(&mut output, machine.overlay_gb);
        output.push_str(",\"cpuSeconds\":");
        push_json_optional_u64(&mut output, machine.cpu_seconds);
        output.push_str(",\"cpuMillis\":");
        push_json_optional_u64(&mut output, machine.cpu_millis);
        output.push_str(",\"rssMb\":");
        push_json_optional_u64(&mut output, machine.rss_mb);
        output.push_str(",\"diskUsedMb\":");
        push_json_optional_u64(&mut output, machine.disk_used_mb);
        output.push('}');
    }
    output.push_str("]}");
    output
}

fn push_json_optional_string(output: &mut String, value: Option<&str>) {
    match value {
        Some(value) => push_json_string(output, value),
        None => output.push_str("null"),
    }
}

fn push_json_optional_i32(output: &mut String, value: Option<i32>) {
    match value {
        Some(value) => output.push_str(&value.to_string()),
        None => output.push_str("null"),
    }
}

fn push_json_optional_u64(output: &mut String, value: Option<u64>) {
    match value {
        Some(value) => output.push_str(&value.to_string()),
        None => output.push_str("null"),
    }
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
    "usage: smolworld [-f .smolworld] <check|prepare|up|checkpoint|restore|release|down|ps|metrics>\n       smolworld checkpoint [-f PATH] --output DIR\n       smolworld restore [-f PATH] --checkpoint DIR\n       smolworld release [-f PATH] --checkpoint DIR\n       smolworld ps [-f PATH] [--json]\n       smolworld metrics [-f PATH] --json\n       smolworld [-f .smolworld] exec MACHINE [--secret-env GUEST=HOST_ENV]... -- COMMAND [ARG ...]\n       smolworld cp [-f PATH] SRC DST"
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
    fn parses_exec_secret_env_before_command_separator() {
        assert!(matches!(
            parse_cli(vec![
                "exec".into(),
                "agent".into(),
                "--secret-env".into(),
                "OPENROUTER_API_KEY=OPENROUTER_API_KEY".into(),
                "--".into(),
                "/usr/local/bin/runebench-pi-agent".into(),
                "--model".into(),
                "openrouter/example".into(),
            ])
            .unwrap(),
            Cli::Exec {
                config,
                machine,
                secret_env,
                command,
            } if config == PathBuf::from(".smolworld")
                && machine == "agent"
                && secret_env == vec!["OPENROUTER_API_KEY=OPENROUTER_API_KEY"]
                && command == vec![
                    "/usr/local/bin/runebench-pi-agent",
                    "--model",
                    "openrouter/example",
                ]
        ));
    }

    #[test]
    fn parses_checkpoint_and_restore_with_explicit_artifact_paths() {
        assert!(matches!(
            parse_cli(vec![
                "checkpoint".into(),
                "--output".into(),
                "/private/tmp/w1".into(),
                "--file".into(),
                "world.smolworld".into(),
            ])
            .unwrap(),
            Cli::Checkpoint { config, output }
                if config == PathBuf::from("world.smolworld")
                    && output == PathBuf::from("/private/tmp/w1")
        ));
        assert!(matches!(
            parse_cli(vec![
                "-f".into(),
                "world.smolworld".into(),
                "restore".into(),
                "--checkpoint".into(),
                "/private/tmp/w1".into(),
            ])
            .unwrap(),
            Cli::Restore { config, checkpoint }
                if config == PathBuf::from("world.smolworld")
                    && checkpoint == PathBuf::from("/private/tmp/w1")
        ));
        assert!(matches!(
            parse_cli(vec![
                "release".into(),
                "--checkpoint".into(),
                "/private/tmp/w1".into(),
            ])
            .unwrap(),
            Cli::Release { config, checkpoint }
                if config == PathBuf::from(".smolworld")
                    && checkpoint == PathBuf::from("/private/tmp/w1")
        ));
        assert!(parse_cli(vec!["checkpoint".into(), "--output".into()]).is_err());
        assert!(parse_cli(vec!["restore".into(), "--checkpoint".into()]).is_err());
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
    fn parses_metrics_json_and_file_in_either_order() {
        assert!(matches!(
            parse_cli(vec![
                "metrics".into(),
                "--json".into(),
                "--file".into(),
                "world.smolworld".into(),
            ])
            .unwrap(),
            Cli::Metrics { config } if config == PathBuf::from("world.smolworld")
        ));
        assert!(matches!(
            parse_cli(vec![
                "-f".into(),
                "world.smolworld".into(),
                "metrics".into(),
                "--json".into(),
            ])
            .unwrap(),
            Cli::Metrics { config } if config == PathBuf::from("world.smolworld")
        ));
        assert_eq!(
            parse_cli(vec!["metrics".into()]).err().as_deref(),
            Some(metrics_usage())
        );
        assert!(parse_cli(vec!["metrics".into(), "--json".into(), "--json".into(),]).is_err());
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
            LifecycleState::Capturing,
            LifecycleState::Captured,
            LifecycleState::Absent,
        ];
        let labels: Vec<_> = states.iter().map(|state| state.as_str()).collect();
        assert_eq!(
            labels,
            [
                "created",
                "attached",
                "running",
                "capturing",
                "captured",
                "absent"
            ]
        );
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

    #[test]
    fn formats_metrics_as_a_closed_schema_with_nulls() {
        let machines = vec![MachineMetrics {
            machine: "runner".into(),
            smolvm_name: Some("smw-v2-demo-runner".into()),
            state: "running".into(),
            pid: Some(42),
            cpus: Some(4),
            memory_mb: Some(4096),
            storage_gb: Some(20),
            overlay_gb: Some(4),
            cpu_seconds: Some(2),
            cpu_millis: Some(2345),
            rss_mb: Some(128),
            disk_used_mb: None,
        }];
        assert_eq!(
            format_metrics_json("demo", &machines),
            "{\"schemaVersion\":1,\"world\":\"demo\",\"machines\":[{\"machine\":\"runner\",\"smolvmName\":\"smw-v2-demo-runner\",\"state\":\"running\",\"pid\":42,\"cpus\":4,\"memoryMb\":4096,\"storageGb\":20,\"overlayGb\":4,\"cpuSeconds\":2,\"cpuMillis\":2345,\"rssMb\":128,\"diskUsedMb\":null}]}"
        );
    }
}
