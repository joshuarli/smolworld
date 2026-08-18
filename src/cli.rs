use crate::Result;
use lexopt::{Arg, Parser};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

mod check;
mod checkpoint;
mod cp;
mod down;
mod exec;
mod metrics;
mod prepare;
mod ps;
mod release;
mod restore;
mod up;

#[cfg(test)]
pub(crate) use ps::parse_ps_options;
pub(crate) use ps::PsFormat;

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

pub(crate) enum Cli {
    Help {
        command: Option<String>,
    },
    Version,
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
    /// Collect host-side metrics for the recorded world machines.
    Metrics {
        config: PathBuf,
    },
    Exec {
        config: PathBuf,
        machine: String,
        secret_env: Vec<OsString>,
        command: Vec<OsString>,
    },
    Cp {
        config: PathBuf,
        source: String,
        destination: String,
    },
}

/// Metadata shared by the parser and every help view. Command modules expose
/// one `SPEC`; the renderer walks those values recursively, so the top-level
/// help cannot accidentally omit a command's options or positionals.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OptionSpec {
    pub(crate) short: Option<char>,
    pub(crate) long: &'static str,
    pub(crate) value_name: Option<&'static str>,
    pub(crate) required: bool,
    pub(crate) repeatable: bool,
    pub(crate) default: Option<&'static str>,
    pub(crate) help: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PositionalSpec {
    pub(crate) name: &'static str,
    pub(crate) required: bool,
    pub(crate) repeatable: bool,
    pub(crate) help: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    pub(crate) about: &'static str,
    pub(crate) options: &'static [OptionSpec],
    pub(crate) positionals: &'static [PositionalSpec],
    pub(crate) examples: &'static [&'static str],
    pub(crate) subcommands: &'static [&'static CommandSpec],
}

pub(crate) const FILE_OPTION: OptionSpec = OptionSpec {
    short: Some('f'),
    long: "file",
    value_name: Some("PATH"),
    required: false,
    repeatable: false,
    default: Some(".smolworld"),
    help: "Select the authored .smolworld file",
};

pub(crate) const JSON_OPTION: OptionSpec = OptionSpec {
    short: None,
    long: "json",
    value_name: None,
    required: false,
    repeatable: false,
    default: None,
    help: "Use the stable JSON presentation",
};

pub(crate) const METRICS_JSON_OPTION: OptionSpec = OptionSpec {
    short: None,
    long: "json",
    value_name: None,
    required: true,
    repeatable: false,
    default: None,
    help: "Use the stable JSON presentation",
};

pub(crate) const OUTPUT_OPTION: OptionSpec = OptionSpec {
    short: None,
    long: "output",
    value_name: Some("DIR"),
    required: true,
    repeatable: false,
    default: None,
    help: "Write the new checkpoint into this directory",
};

pub(crate) const CHECKPOINT_OPTION: OptionSpec = OptionSpec {
    short: None,
    long: "checkpoint",
    value_name: Some("DIR"),
    required: true,
    repeatable: false,
    default: None,
    help: "Use this retained checkpoint directory",
};

pub(crate) const SECRET_ENV_OPTION: OptionSpec = OptionSpec {
    short: None,
    long: "secret-env",
    value_name: Some("GUEST=HOST_ENV"),
    required: false,
    repeatable: true,
    default: None,
    help: "Pass one selected host environment variable to the guest command",
};

pub(crate) const HELP_OPTION: OptionSpec = OptionSpec {
    short: Some('h'),
    long: "help",
    value_name: None,
    required: false,
    repeatable: false,
    default: None,
    help: "Print this complete command help",
};

pub(crate) const VERSION_OPTION: OptionSpec = OptionSpec {
    short: Some('v'),
    long: "version",
    value_name: None,
    required: false,
    repeatable: false,
    default: None,
    help: "Print the package version and embedded Git commit",
};

pub(crate) static COMMANDS: &[&CommandSpec] = &[
    &check::SPEC,
    &prepare::SPEC,
    &up::SPEC,
    &checkpoint::SPEC,
    &restore::SPEC,
    &release::SPEC,
    &down::SPEC,
    &ps::SPEC,
    &metrics::SPEC,
    &exec::SPEC,
    &cp::SPEC,
];

pub(crate) static ROOT_SPEC: CommandSpec = CommandSpec {
    name: "smolworld",
    about: "Run small, statically provisioned local worlds for smolvm.",
    options: &[FILE_OPTION, HELP_OPTION, VERSION_OPTION],
    positionals: &[],
    examples: &[],
    subcommands: COMMANDS,
};

#[cfg(test)]
pub(crate) fn parse_cli(args: Vec<OsString>) -> Result<Cli> {
    parse_cli_os(args)
}

pub(crate) fn parse_cli_os<I, T>(args: I) -> Result<Cli>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut parser = Parser::from_args(args);
    let mut config = PathBuf::from(".smolworld");
    let mut file_seen = false;

    loop {
        let Some(arg) = parser
            .next()
            .map_err(|error| format!("smolworld: {error}"))?
        else {
            return Err(format!("missing command\n\n{}", render_help(None)));
        };
        match arg {
            arg if option_matches(&arg, &FILE_OPTION) => {
                if file_seen {
                    return Err(format!(
                        "smolworld: {} may be specified only once before the command",
                        option_display(&FILE_OPTION)
                    ));
                }
                config = PathBuf::from(
                    parser
                        .value()
                        .map_err(|error| format!("smolworld: {error}"))?,
                );
                file_seen = true;
            }
            arg if option_matches(&arg, &HELP_OPTION) => return Ok(Cli::Help { command: None }),
            arg if option_matches(&arg, &VERSION_OPTION) => return Ok(Cli::Version),
            Arg::Value(command) => {
                let command_name = command.to_string_lossy();
                if command_name == check::SPEC.name {
                    return check::parse(&mut parser, config);
                }
                if command_name == prepare::SPEC.name {
                    return prepare::parse(&mut parser, config);
                }
                if command_name == up::SPEC.name {
                    return up::parse(&mut parser, config);
                }
                if command_name == checkpoint::SPEC.name {
                    return checkpoint::parse(&mut parser, config);
                }
                if command_name == restore::SPEC.name {
                    return restore::parse(&mut parser, config);
                }
                if command_name == release::SPEC.name {
                    return release::parse(&mut parser, config);
                }
                if command_name == down::SPEC.name {
                    return down::parse(&mut parser, config);
                }
                if command_name == ps::SPEC.name {
                    return ps::parse(&mut parser, config);
                }
                if command_name == metrics::SPEC.name {
                    return metrics::parse(&mut parser, config);
                }
                if command_name == exec::SPEC.name {
                    return exec::parse(&mut parser, config);
                }
                if command_name == cp::SPEC.name {
                    return cp::parse(&mut parser, config);
                }
                return Err(format!(
                    "unknown command '{}'\n\n{}",
                    command.to_string_lossy(),
                    render_help(None)
                ));
            }
            other => {
                return Err(format!(
                    "unexpected top-level argument {:?}\n\n{}",
                    other,
                    render_help(None)
                ));
            }
        }
    }
}

pub(crate) fn option_matches(arg: &Arg<'_>, option: &OptionSpec) -> bool {
    match arg {
        Arg::Short(short) => option.short == Some(*short),
        Arg::Long(long) => *long == option.long,
        Arg::Value(_) => false,
    }
}

pub(crate) fn option_display(option: &OptionSpec) -> String {
    let mut display = String::new();
    if let Some(short) = option.short {
        display.push('-');
        display.push(short);
        display.push('/');
    }
    display.push_str("--");
    display.push_str(option.long);
    display
}

pub(crate) fn version() -> &'static str {
    concat!(
        "smolworld ",
        env!("CARGO_PKG_VERSION"),
        " (git ",
        env!("SMOLWORLD_GIT_SHA"),
        ")"
    )
}

pub(crate) fn render_help(command: Option<&str>) -> String {
    let mut output = String::new();
    match command {
        None => {
            write_help_header(
                &mut output,
                ROOT_SPEC.name,
                ROOT_SPEC.about,
                &command_usage(&ROOT_SPEC, ROOT_SPEC.name),
            );
            output.push_str("Common options (shown once; command pages show placement):\n");
            write_options_at(&mut output, ROOT_SPEC.options, 2);
            output.push_str("\nEvery command also accepts -h/--help and -v/--version; command-specific options are shown below.\n");
            output.push_str("\nCommand reference:\n");
            for spec in ROOT_SPEC.subcommands {
                write_compact_command(&mut output, spec, ROOT_SPEC.name, 2);
            }
        }
        Some(name) => {
            let Some(spec) = find_spec(ROOT_SPEC.subcommands, name) else {
                return render_help(None);
            };
            write_command_details(&mut output, spec, ROOT_SPEC.name);
        }
    }
    output
}

fn find_spec(specs: &'static [&'static CommandSpec], name: &str) -> Option<&'static CommandSpec> {
    for spec in specs {
        if spec.name == name {
            return Some(spec);
        }
        if let Some(found) = find_spec(spec.subcommands, name) {
            return Some(found);
        }
    }
    None
}

fn write_help_header(output: &mut String, name: &str, about: &str, usage: &str) {
    output.push_str(&format!("{name}: {about}\n\nUsage: {usage}\n\n"));
}

fn write_options(output: &mut String, options: &[OptionSpec]) {
    output.push_str("Options:\n");
    write_options_at(output, options, 2);
}

fn write_options_at(output: &mut String, options: &[OptionSpec], indent: usize) {
    let padding = " ".repeat(indent);
    for option in options {
        let mut spelling = String::new();
        if let Some(short) = option.short {
            spelling.push('-');
            spelling.push(short);
            if option.long.is_empty() {
                spelling.push(' ');
            } else {
                spelling.push_str(", ");
            }
        }
        if !option.long.is_empty() {
            spelling.push_str("--");
            spelling.push_str(option.long);
        }
        if let Some(value_name) = option.value_name {
            spelling.push(' ');
            spelling.push_str(value_name);
        }
        let mut qualifiers = Vec::new();
        if option.required {
            qualifiers.push("required");
        }
        if option.repeatable {
            qualifiers.push("repeatable");
        }
        let mut description = option.help.to_owned();
        if let Some(default) = option.default {
            description.push_str(" (default: ");
            description.push_str(default);
            description.push(')');
        }
        if qualifiers.is_empty() {
            output.push_str(&format!("{padding}{:<28} {description}\n", spelling));
        } else {
            output.push_str(&format!(
                "{padding}{:<28} {description} ({})\n",
                spelling,
                qualifiers.join(", ")
            ));
        }
    }
}

fn write_compact_command(output: &mut String, spec: &CommandSpec, parent: &str, indent: usize) {
    let padding = " ".repeat(indent);
    let command_name = format!("{parent} {}", spec.name);
    let usage = command_usage(spec, &command_name);
    output.push_str(&format!("\n{padding}{usage}\n{padding}  {}\n", spec.about));
    let options: Vec<_> = spec
        .options
        .iter()
        .copied()
        .filter(|option| !common_option(option))
        .collect();
    if !options.is_empty() {
        output.push_str(&format!("{padding}  Options:\n"));
        write_options_at(output, &options, indent + 4);
    }
    if !spec.positionals.is_empty() {
        output.push_str(&format!("{padding}  Arguments:\n"));
        for positional in spec.positionals {
            output.push_str(&format!(
                "{padding}    {:<20} {}\n",
                positional.name, positional.help
            ));
        }
    }
    for child in spec.subcommands {
        write_compact_command(output, child, &command_name, indent + 2);
    }
}

fn common_option(option: &OptionSpec) -> bool {
    option.long == FILE_OPTION.long
}

fn command_usage(spec: &CommandSpec, command_name: &str) -> String {
    let mut usage = format!("{command_name} [OPTIONS]");
    if !spec.subcommands.is_empty() {
        usage.push_str(" <COMMAND>");
    }
    for positional in spec.positionals {
        usage.push(' ');
        if !positional.required {
            usage.push('[');
        }
        usage.push_str(positional.name);
        if positional.repeatable {
            usage.push_str("...");
        }
        if !positional.required {
            usage.push(']');
        }
    }
    usage
}

fn write_command_details(output: &mut String, spec: &CommandSpec, parent: &str) {
    let command_name = format!("{parent} {}", spec.name);
    let usage = command_usage(spec, &command_name);
    output.push_str(&format!(
        "{command_name}\n{about}\n\nUsage: {usage}\n\n",
        about = spec.about
    ));
    let mut options = spec.options.to_vec();
    options.push(HELP_OPTION);
    options.push(VERSION_OPTION);
    write_options(output, &options);
    if !spec.positionals.is_empty() {
        output.push_str("\nArguments:\n");
        for positional in spec.positionals {
            output.push_str(&format!("  {:<20} {}\n", positional.name, positional.help));
        }
    }
    if !spec.examples.is_empty() {
        output.push_str("\nExamples:\n");
        for example in spec.examples {
            output.push_str("  ");
            output.push_str(example);
            output.push('\n');
        }
    }
    if !spec.subcommands.is_empty() {
        output.push_str("\nCommands:\n");
        for child in spec.subcommands {
            output.push_str(&format!("  {:<14} {}\n", child.name, child.about));
        }
    }
}

pub(crate) fn command_help(command: &'static str) -> Cli {
    Cli::Help {
        command: Some(command.to_owned()),
    }
}

pub(crate) fn parse_value(
    parser: &mut Parser,
    command: &str,
    option: &OptionSpec,
) -> Result<OsString> {
    parser.value().map_err(|error| {
        format!(
            "{command}: --{} requires {} ({error})\n\n{}",
            option.long,
            option.value_name.unwrap_or("a value"),
            render_help(Some(command)),
        )
    })
}

pub(crate) fn parse_file(
    parser: &mut Parser,
    command: &str,
    config: &mut PathBuf,
    seen: &mut bool,
) -> Result<()> {
    if *seen {
        return Err(format!(
            "{command} accepts {} at most once",
            option_display(&FILE_OPTION)
        ));
    }
    *config = PathBuf::from(parse_value(parser, command, &FILE_OPTION)?);
    *seen = true;
    Ok(())
}

pub(crate) fn unexpected(command: &str, arg: Arg<'_>) -> String {
    let label = match arg {
        Arg::Short(short) => format!("-{short}"),
        Arg::Long(long) => format!("--{long}"),
        Arg::Value(value) => format!("positional {:?}", value),
    };
    format!(
        "unknown {command} argument {label}\n\n{}",
        render_help(Some(command))
    )
}

pub(crate) fn missing(command: &str) -> String {
    format!(
        "missing arguments for {command}\n\n{}",
        render_help(Some(command))
    )
}

pub(crate) fn parse_error(command: &str, error: lexopt::Error) -> String {
    format!("{command}: {error}\n\n{}", render_help(Some(command)))
}

pub(crate) fn os_string(value: OsString, command: &str, positional: &str) -> Result<String> {
    value
        .into_string()
        .map_err(|_| format!("{command}: {positional} must be valid UTF-8"))
}

pub(crate) fn path_argument(value: OsString) -> PathBuf {
    PathBuf::from(value)
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

#[cfg(test)]
mod tests {
    use super::{
        format_metrics_json, format_ps, format_ps_json, format_ps_table, parse_cli,
        parse_ps_options, render_help, version, Cli, CommandSpec, LifecycleState, MachineMetrics,
        MachineStatus, PsFormat, COMMANDS, ROOT_SPEC,
    };
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn assert_spec_is_described(spec: &CommandSpec) {
        assert!(!spec.name.is_empty());
        assert!(!spec.about.is_empty());
        for option in spec.options {
            assert!(!option.long.is_empty());
            assert!(
                !option.help.is_empty(),
                "--{} has no explanation",
                option.long
            );
        }
        for positional in spec.positionals {
            assert!(!positional.name.is_empty());
            assert!(
                !positional.help.is_empty(),
                "{} has no explanation",
                positional.name
            );
        }
        for child in spec.subcommands {
            assert_spec_is_described(child);
        }
    }

    #[test]
    fn command_schema_is_complete_and_top_level_help_is_a_readable_reference() {
        assert_spec_is_described(&ROOT_SPEC);
        let help = render_help(None);
        assert!(help.contains("Common options"));
        assert!(help.contains("Command reference:"));
        assert!(!help.contains("Command details:"));
        assert!(!help.contains("\nCommands:\n"));
        assert_eq!(help.matches("--file").count(), 1);
        for spec in COMMANDS {
            assert!(help.contains(&format!("smolworld {}", spec.name)));
            for option in spec.options {
                assert!(help.contains(&format!("--{}", option.long)));
            }
            for positional in spec.positionals {
                assert!(help.contains(positional.name));
            }
        }
    }

    #[test]
    fn version_contains_the_compile_time_git_sha() {
        assert!(version().contains(env!("SMOLWORLD_GIT_SHA")));
        assert!(version().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn accepts_file_flag_before_or_after_command() {
        assert!(matches!(
            parse_cli(vec!["-f".into(), "demo".into(), "ps".into()]).unwrap(),
            Cli::Ps { config, format: PsFormat::Table } if config == *"demo"
        ));
        assert!(matches!(
            parse_cli(vec!["ps".into(), "--file".into(), "demo".into()]).unwrap(),
            Cli::Ps { config, format: PsFormat::Table } if config == *"demo"
        ));
    }

    #[test]
    fn parses_prepare_with_file_flag_before_or_after_command() {
        assert!(matches!(
            parse_cli(vec!["-f".into(), "demo".into(), "prepare".into()]).unwrap(),
            Cli::Prepare { config } if config == *"demo"
        ));
        assert!(matches!(
            parse_cli(vec!["prepare".into(), "--file".into(), "demo".into()]).unwrap(),
            Cli::Prepare { config } if config == *"demo"
        ));
        assert!(matches!(
            parse_cli(vec!["prepare".into()]).unwrap(),
            Cli::Prepare { config } if config == *".smolworld"
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
            } if config == *".smolworld"
                && machine == "agent"
                && secret_env == vec![OsString::from("OPENROUTER_API_KEY=OPENROUTER_API_KEY")]
                && command == vec![
                    OsString::from("/usr/local/bin/runebench-pi-agent"),
                    OsString::from("--model"),
                    OsString::from("openrouter/example"),
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
                if config == *"world.smolworld"
                    && output == *"/private/tmp/w1"
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
                if config == *"world.smolworld"
                    && checkpoint == *"/private/tmp/w1"
        ));
        assert!(matches!(
            parse_cli(vec![
                "release".into(),
                "--checkpoint".into(),
                "/private/tmp/w1".into(),
            ])
            .unwrap(),
            Cli::Release { config, checkpoint }
                if config == *".smolworld"
                    && checkpoint == *"/private/tmp/w1"
        ));
        assert!(parse_cli(vec!["checkpoint".into(), "--output".into()]).is_err());
        assert!(parse_cli(vec!["restore".into(), "--checkpoint".into()]).is_err());
    }

    #[test]
    fn rejects_invalid_prepare_options() {
        let missing_file = parse_cli(vec!["prepare".into(), "--file".into()])
            .err()
            .unwrap();
        assert!(missing_file.contains("prepare: --file requires PATH"));
        let extra = parse_cli(vec!["prepare".into(), "extra".into()])
            .err()
            .unwrap();
        assert!(extra.contains("Usage: smolworld prepare [OPTIONS]"));
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
                if config == *".smolworld"
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
                if config == *"world.smolworld"
                    && source == "runner:/workspace/result.txt"
                    && destination == "host-result.txt"
        ));
        let error = parse_cli(vec!["cp".into(), "only-one-operand".into()])
            .err()
            .expect("copy invocation is invalid");
        assert!(error.contains("Usage: smolworld cp [OPTIONS] SRC DST"));
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
            Cli::Metrics { config } if config == *"world.smolworld"
        ));
        assert!(matches!(
            parse_cli(vec![
                "-f".into(),
                "world.smolworld".into(),
                "metrics".into(),
                "--json".into(),
            ])
            .unwrap(),
            Cli::Metrics { config } if config == *"world.smolworld"
        ));
        assert!(parse_cli(vec!["metrics".into()]).is_err());
        assert!(parse_cli(vec!["metrics".into(), "--json".into(), "--json".into(),]).is_err());
    }

    #[test]
    fn rejects_invalid_ps_options() {
        assert_eq!(
            parse_ps_options(PathBuf::from("world"), &["--json".into(), "--json".into()])
                .unwrap_err(),
            "ps accepts --json at most once"
        );
        assert!(parse_ps_options(PathBuf::from("world"), &["--file".into()])
            .unwrap_err()
            .contains("--file requires PATH"));
        assert!(parse_ps_options(PathBuf::from("world"), &["--wat".into()])
            .unwrap_err()
            .contains("unknown ps argument"));
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
            smolvm_name: Some("smw-demo-runner".into()),
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
            "{\"schemaVersion\":1,\"world\":\"demo\",\"machines\":[{\"machine\":\"runner\",\"smolvmName\":\"smw-demo-runner\",\"state\":\"running\",\"pid\":42,\"cpus\":4,\"memoryMb\":4096,\"storageGb\":20,\"overlayGb\":4,\"cpuSeconds\":2,\"cpuMillis\":2345,\"rssMb\":128,\"diskUsedMb\":null}]}"
        );
    }
}
