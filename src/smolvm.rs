use crate::cli::ExecOptions;
use crate::companion_adapter::{self, Operation};
use crate::model::{
    format_mac, MachineLaunch, NetworkConfig, SeedFile, WorldAllocationState, WorldConfig,
};
use crate::state::validate_recorded_smolvm_name;
use crate::Result;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// A generic immutable registry image rendered by smolvm into a local archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaterializedRegistryArchive {
    pub(crate) source_reference: String,
    pub(crate) source_digest: String,
    pub(crate) archive_path: PathBuf,
    pub(crate) archive_digest: String,
}

/// Closed resource-observation record returned by the companion smolvm subprocess.
///
/// smolworld consumes the versioned TSV form directly, so this crate does not
/// need a general-purpose JSON dependency or parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineStats {
    pub(crate) name: String,
    pub(crate) state: CompanionMachineState,
    pub(crate) pid: Option<i32>,
    pub(crate) cpus: u8,
    pub(crate) memory_mb: u32,
    pub(crate) storage_gb: u64,
    pub(crate) overlay_gb: u64,
    pub(crate) cpu_seconds: Option<u64>,
    pub(crate) cpu_millis: Option<u64>,
    pub(crate) rss_mb: Option<u64>,
    pub(crate) disk_used_mb: Option<u64>,
}

const IMAGE_MATERIAL_TSV_ABI: &str = "image-material-v1";
const MACHINE_STATS_TSV_ABI: &str = "machine-stats-v1";

/// Closed lifecycle observations returned by smolvm's machine inspection
/// commands. This adapter parses upstream text immediately, so no
/// unrecognized status crosses into world runtime logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompanionMachineState {
    Created,
    Running,
    Stopped,
    Failed,
    Unreachable,
    Frozen,
}

impl CompanionMachineState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Unreachable => "unreachable",
            Self::Frozen => "frozen",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "created" => Ok(Self::Created),
            "running" => Ok(Self::Running),
            "stopped" => Ok(Self::Stopped),
            "failed" => Ok(Self::Failed),
            "unreachable" => Ok(Self::Unreachable),
            "frozen" => Ok(Self::Frozen),
            _ => Err(format!("unknown smolvm machine state '{value}'")),
        }
    }
}

/// Collect one exact recorded smolvm machine through the read-only stats
/// subprocess boundary. The caller is responsible for proving that `name` is
/// a world identity before invoking this function.
pub(crate) fn machine_stats(smolvm: &Path, name: &str) -> Result<MachineStats> {
    let mut command = Command::new(smolvm);
    command.args(["machine", "stats", "--name", name, "--format", "tsv"]);
    let output = companion_adapter::output(Operation::Stats, &mut command)?;
    if !output.status.success() {
        return Err(format!(
            "smolvm machine stats {name} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| "smolvm machine stats emitted non-UTF-8 output".to_string())?;
    parse_machine_stats_tsv(stdout, name)
}

/// Query an exact recorded identity through the upstream status command. This
/// keeps its human-oriented response parsing contained in the adapter.
pub(crate) fn machine_status(smolvm: &Path, name: &str) -> Result<Option<CompanionMachineState>> {
    let mut command = Command::new(smolvm);
    command.args(["machine", "status", "--name", name]);
    let output = companion_adapter::output(Operation::Status, &mut command)?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .split_whitespace()
        .find_map(|word| CompanionMachineState::parse(word).ok()))
}

pub(crate) fn exec_machine(
    smolvm: &Path,
    name: &str,
    options: &ExecOptions,
    command: &[std::ffi::OsString],
) -> Result<()> {
    let mut invocation = Command::new(smolvm);
    invocation.args(["machine", "exec", "--name", name]);
    for value in &options.env {
        invocation.arg("--env").arg(value);
    }
    if let Some(workdir) = &options.workdir {
        invocation.arg("--workdir").arg(workdir);
    }
    if options.interactive {
        invocation.arg("--interactive");
    }
    if options.tty {
        invocation.arg("--tty");
    }
    if options.stream {
        invocation.arg("--stream");
    }
    if options.detach {
        invocation.arg("--detach");
    }
    if let Some(timeout) = &options.timeout {
        invocation.arg("--timeout").arg(timeout);
    }
    for value in &options.secret_env {
        invocation.arg("--secret-env").arg(value);
    }
    for value in &options.secret_file {
        invocation.arg("--secret-file").arg(value);
    }
    invocation.arg("--").args(command);
    companion_adapter::status(Operation::Exec, &mut invocation)
}

pub(crate) fn copy_machine(
    smolvm: &Path,
    name: &str,
    guest_path: &str,
    local_path: &str,
    upload: bool,
) -> Result<()> {
    let remote = format!("{name}:{guest_path}");
    let mut invocation = Command::new(smolvm);
    invocation.args(["machine", "cp"]);
    if upload {
        invocation.args([local_path, &remote]);
    } else {
        invocation.args([&remote, local_path]);
    }
    companion_adapter::status(Operation::Copy, &mut invocation)
}

/// Place one sealed world input through smolvm's generic running-machine
/// operations. Keeping this step here makes the world contract observable
/// without extending Smolfiles or adding an upstream-specific seed protocol.
pub(crate) fn install_seed_files(smolvm: &Path, name: &str, seeds: &[SeedFile]) -> Result<()> {
    for seed in seeds {
        let source = seed
            .source
            .to_str()
            .ok_or_else(|| format!("seed source {} is not valid UTF-8", seed.source.display()))?;
        if source.contains(':') {
            return Err(format!(
                "seed source {} cannot contain ':' because smolvm machine cp uses NAME:/path endpoints",
                seed.source.display()
            ));
        }
        let destination = seed.destination.to_str().ok_or_else(|| {
            format!(
                "seed destination {} is not valid UTF-8",
                seed.destination.display()
            )
        })?;
        copy_machine(smolvm, name, destination, source, true)?;

        let mode = format!("{:04o}", seed.mode);
        let mut chmod = Command::new(smolvm);
        chmod
            .args(["machine", "exec", "--name", name, "--", "/bin/chmod"])
            .arg(mode)
            .arg(destination);
        companion_adapter::status(Operation::Exec, &mut chmod)?;
    }
    Ok(())
}

fn parse_machine_stats_tsv(output: &str, expected_name: &str) -> Result<MachineStats> {
    let record = output.strip_suffix('\n').ok_or_else(|| {
        "smolvm machine stats must emit exactly one newline-terminated record".to_string()
    })?;
    if record.contains(['\n', '\r']) {
        return Err("smolvm machine stats emitted more than one record".into());
    }
    let fields: Vec<_> = record.split('\t').collect();
    if fields.len() != 12 {
        return Err(format!(
            "smolvm machine stats returned {} TSV fields, expected 12",
            fields.len()
        ));
    }
    let [abi, name, state, pid, cpus, memory_mb, storage_gb, overlay_gb, cpu_seconds, cpu_millis, rss_mb, disk_used_mb]: [&str; 12] =
        fields.try_into().expect("field count checked above");
    if abi != MACHINE_STATS_TSV_ABI {
        return Err(format!("unsupported smolvm machine stats ABI '{abi}'"));
    }
    if name != expected_name {
        return Err(format!(
            "smolvm machine stats returned machine '{name}', expected '{expected_name}'"
        ));
    }
    let state = CompanionMachineState::parse(state)
        .map_err(|_| format!("smolvm machine stats returned unknown state '{state}'"))?;
    Ok(MachineStats {
        name: name.to_string(),
        state,
        pid: parse_optional_i32(pid, "pid")?,
        cpus: cpus
            .parse()
            .map_err(|_| "smolvm machine stats returned invalid cpus".to_string())?,
        memory_mb: memory_mb
            .parse()
            .map_err(|_| "smolvm machine stats returned invalid memory_mb".to_string())?,
        storage_gb: storage_gb
            .parse()
            .map_err(|_| "smolvm machine stats returned invalid storage_gb".to_string())?,
        overlay_gb: overlay_gb
            .parse()
            .map_err(|_| "smolvm machine stats returned invalid overlay_gb".to_string())?,
        cpu_seconds: parse_optional_u64(cpu_seconds, "cpu_seconds")?,
        cpu_millis: parse_optional_u64(cpu_millis, "cpu_millis")?,
        rss_mb: parse_optional_u64(rss_mb, "rss_mb")?,
        disk_used_mb: parse_optional_u64(disk_used_mb, "disk_used_mb")?,
    })
}

fn parse_optional_i32(value: &str, field: &str) -> Result<Option<i32>> {
    if value.is_empty() {
        Ok(None)
    } else {
        value
            .parse()
            .map(Some)
            .map_err(|_| format!("smolvm machine stats returned invalid {field}"))
    }
}

fn parse_optional_u64(value: &str, field: &str) -> Result<Option<u64>> {
    if value.is_empty() {
        Ok(None)
    } else {
        value
            .parse()
            .map(Some)
            .map_err(|_| format!("smolvm machine stats returned invalid {field}"))
    }
}

/// Materialize one immutable registry image through smolvm's generic image
/// boundary. smolworld owns the policy that chooses when this is allowed.
pub(crate) fn materialize_registry_archive(
    smolvm: &Path,
    reference: &str,
) -> Result<MaterializedRegistryArchive> {
    let mut command = Command::new(smolvm);
    command
        .args([
            "image",
            "materialize",
            "--format",
            "tsv",
            "--reference",
        ])
        .arg(reference);
    let output = companion_adapter::output(Operation::Prepare, &mut command)?;
    if !output.status.success() {
        return Err(format!(
            "smolvm image materialization exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout).map_err(|_| {
        "smolvm image materialization emitted non-UTF-8 output".to_string()
    })?;
    parse_materialized_registry_archive_tsv(stdout, reference)
}

fn parse_materialized_registry_archive_tsv(
    output: &str,
    expected_reference: &str,
) -> Result<MaterializedRegistryArchive> {
    let record = output.strip_suffix('\n').ok_or_else(|| {
        "smolvm image materialization must end with one newline".to_string()
    })?;
    if record.contains('\n') || record.contains('\r') {
        return Err("smolvm image materialization emitted more than one record".into());
    }
    let fields: Vec<_> = record.split('\t').collect();
    if fields.len() != 5 {
        return Err(format!(
            "smolvm image materialization returned {} TSV fields, expected 5",
            fields.len()
        ));
    }
    let [abi, source_reference, source_digest, archive, archive_digest]: [&str; 5] =
        fields.try_into().expect("field count checked above");
    if abi != IMAGE_MATERIAL_TSV_ABI {
        return Err(format!("unsupported smolvm image materialization ABI '{abi}'"));
    }
    if source_reference != expected_reference {
        return Err("smolvm image materialization returned a different source reference".into());
    }
    if !is_algorithm_digest(source_digest, "sha256") {
        return Err("smolvm image materialization returned an invalid source digest".into());
    }
    let archive_path = PathBuf::from(archive);
    if !archive_path.is_absolute() {
        return Err("smolvm image materialization returned a relative archive path".into());
    }
    let archive_path = fs::canonicalize(&archive_path)
        .map_err(|error| format!("resolve materialized archive {}: {error}", archive_path.display()))?;
    if !archive_path.is_file() {
        return Err("smolvm image materialization returned a non-file archive path".into());
    }
    if !is_algorithm_digest(archive_digest, "blake3") {
        return Err("smolvm image materialization returned an invalid archive digest".into());
    }
    Ok(MaterializedRegistryArchive {
        source_reference: source_reference.to_string(),
        source_digest: source_digest.to_string(),
        archive_path,
        archive_digest: archive_digest.to_string(),
    })
}

fn is_algorithm_digest(value: &str, algorithm: &str) -> bool {
    value
        .strip_prefix(&format!("{algorithm}:"))
        .is_some_and(|encoded| {
            encoded.len() == 64
                && encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

pub(crate) fn smolvm_program() -> PathBuf {
    env::var_os("SMOLWORLD_SMOLVM")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("smolvm"))
}

pub(crate) fn require_smolvm(program: &Path) -> Result<()> {
    let status = Command::new(program)
        .arg("--version")
        .status()
        .map_err(|error| format!("run {} --version: {error}", program.display()))?;
    status_result("smolvm --version", status)
}

/// Check the host-side files that `smolvm machine start` needs before `up`
/// creates any state or machine. smolvm still owns its own discovery rules;
/// these checks deliberately mirror only the local development paths that can
/// be inspected without starting a VM.
pub(crate) fn preflight(config: &WorldConfig, config_dir: &Path, smolvm: &Path) -> Result<()> {
    require_smolvm(smolvm)?;
    for machine in config.machines.values() {
        let smolfile = local_smolfile_path(config_dir, &machine.smolfile)?;
        let metadata = fs::metadata(&smolfile)
            .map_err(|error| format!("inspect Smolfile {}: {error}", smolfile.display()))?;
        if !metadata.is_file() {
            return Err(format!(
                "Smolfile {} must be a regular file",
                smolfile.display()
            ));
        }
    }
    require_program_on_path("mkfs.ext4").map_err(|_| {
        "mkfs.ext4 is required by this source-build smolvm workflow; install e2fsprogs and add its bin directory to PATH".to_string()
    })?;
    require_hypervisor_entitlement(smolvm)?;
    require_agent_rootfs(smolvm)?;
    require_libkrun_pair(smolvm)?;
    Ok(())
}

pub(crate) fn require_program_on_path(program: &str) -> Result<PathBuf> {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 {
        return program_path
            .is_file()
            .then(|| program_path.to_path_buf())
            .ok_or_else(|| format!("required program {program} does not exist"));
    }
    let path = env::var_os("PATH").ok_or_else(|| "PATH is not set".to_string())?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!("required program {program} is not on PATH"))
}

pub(crate) fn smolvm_binary_path(smolvm: &Path) -> Option<PathBuf> {
    if smolvm.components().count() > 1 {
        return fs::canonicalize(smolvm).ok();
    }
    require_program_on_path(smolvm.to_str()?).ok()
}

pub(crate) fn require_hypervisor_entitlement(smolvm: &Path) -> Result<()> {
    let binary = smolvm_binary_path(smolvm).ok_or_else(|| {
        format!(
            "resolve smolvm binary {} for Hypervisor Framework entitlement check",
            smolvm.display()
        )
    })?;
    let output = Command::new("codesign")
        .args(["-d", "--entitlements", ":-"])
        .arg(&binary)
        .output()
        .map_err(|error| format!("run codesign for {}: {error}", binary.display()))?;
    let mut entitlements = output.stdout;
    entitlements.extend_from_slice(&output.stderr);
    if output.status.success() && has_hypervisor_entitlement(&entitlements) {
        return Ok(());
    }
    Err(format!(
        "{} lacks the macOS Hypervisor Framework entitlement; for a local debug build run `codesign --force --sign - --entitlements smolvm.entitlements {}` from the smolvm checkout",
        binary.display(),
        binary.display(),
    ))
}

pub(crate) fn has_hypervisor_entitlement(entitlements: &[u8]) -> bool {
    String::from_utf8_lossy(entitlements).contains("com.apple.security.hypervisor")
}

pub(crate) fn require_agent_rootfs(smolvm: &Path) -> Result<()> {
    let explicit = env::var_os("SMOLVM_AGENT_ROOTFS").map(PathBuf::from);
    let mut candidates = explicit.clone().into_iter().collect::<Vec<_>>();
    if explicit.is_none() {
        if let Some(binary) = smolvm_binary_path(smolvm) {
            if let Some(directory) = binary.parent() {
                candidates.push(directory.join("agent-rootfs"));
            }
        }
        if let Some(home) = env::var_os("HOME") {
            candidates
                .push(PathBuf::from(home).join("Library/Application Support/smolvm/agent-rootfs"));
        }
    }
    for rootfs in &candidates {
        if rootfs.join("usr/local/bin/smolvm-agent").is_file() {
            return Ok(());
        }
    }
    let expected = explicit
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| {
            "a bundled agent-rootfs or ~/Library/Application Support/smolvm/agent-rootfs"
                .to_string()
        });
    Err(format!(
        "smolvm agent rootfs is unavailable at {expected}; set SMOLVM_AGENT_ROOTFS to a rootfs containing usr/local/bin/smolvm-agent"
    ))
}

pub(crate) fn require_libkrun_pair(smolvm: &Path) -> Result<()> {
    let names = ["libkrun.dylib", "libkrunfw.5.dylib"];
    let explicit = env::var_os("SMOLVM_LIB_DIR").map(PathBuf::from);
    let mut candidates = explicit.clone().into_iter().collect::<Vec<_>>();
    if explicit.is_none() {
        if let Some(binary) = smolvm_binary_path(smolvm) {
            if let Some(directory) = binary.parent() {
                candidates.extend([
                    directory.join("lib"),
                    directory.join("../lib"),
                    directory.join("../../lib"),
                ]);
            }
        }
    }
    for directory in candidates {
        let files = [directory.join(names[0]), directory.join(names[1])];
        if files.iter().all(|path| path.is_file()) {
            for file in &files {
                if is_git_lfs_pointer(file)? {
                    return Err(format!(
                        "{} is a Git LFS pointer, not a loadable library; run git lfs pull in the smolvm checkout",
                        file.display()
                    ));
                }
            }
            return Ok(());
        }
    }
    let expected = explicit
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "a bundled lib/ directory beside smolvm".to_string());
    Err(format!(
        "libkrun and libkrunfw are unavailable in {expected}; set SMOLVM_LIB_DIR to a directory containing both libraries"
    ))
}

pub(crate) fn is_git_lfs_pointer(path: &Path) -> Result<bool> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut prefix = [0; 64];
    let read = file
        .read(&mut prefix)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(prefix[..read].starts_with(b"version https://git-lfs.github.com/spec/v1"))
}

pub(crate) fn create_machine(
    smolvm: &Path,
    launch: MachineLaunch<'_>,
    network: &NetworkConfig,
) -> Result<()> {
    let mut invocation = build_machine_create_command(smolvm, &launch, network);
    companion_adapter::status(Operation::Create, &mut invocation)
}

/// Build the exact, restricted `machine create` invocation without starting
/// it. The Smolfile is the machine declaration; smolworld supplies only its
/// generated identity and complete external NIC tuple. Sealed world seed
/// copies use the generic running-machine boundary after the agent is ready;
/// they are deliberately not an smolvm Smolfile concern.
fn build_machine_create_command(
    smolvm: &Path,
    launch: &MachineLaunch<'_>,
    network: &NetworkConfig,
) -> Command {
    let address = format!("{}/24", launch.assignment.ip);
    let mut invocation = Command::new(smolvm);
    invocation
        .args(["machine", "create", "--name"])
        .arg(&launch.assignment.smolvm_name)
        .args(["--smolfile"])
        .arg(launch.smolfile)
        .args(["--net", "--net-backend", "virtio-net", "--net-unixstream"])
        .arg(launch.socket)
        .args(["--net-address", &address, "--net-gateway"])
        .arg(network.gateway.to_string())
        .args(["--net-dns"])
        .arg(network.dns.to_string())
        .args(["--net-mac"])
        .arg(format_mac(launch.assignment.mac));
    if network.egress {
        invocation.arg("--net-egress");
    }
    invocation
}

/// Start every Smolworld machine as a forkable base. The external-network
/// declaration already provides the stable identity; enabling the fork control
/// socket up front makes the supervisor's later durable checkpoint barrier a
/// capture operation rather than a cold restart of the machine.
pub(crate) fn start_machine(smolvm: &Path, name: &str) -> Result<()> {
    let mut command = Command::new(smolvm);
    command.args(["machine", "start", "--name", name, "--forkable"]);
    companion_adapter::status(Operation::Start, &mut command)
}

/// Capture one forkable world machine into a checkpoint-owned subdirectory.
pub(crate) fn checkpoint_machine(smolvm: &Path, name: &str, output: &Path) -> Result<()> {
    let mut command = Command::new(smolvm);
    command
        .args(["machine", "checkpoint", "--name", name, "--output"])
        .arg(output);
    companion_adapter::captured_status(Operation::Checkpoint, &mut command)
}

/// Restore one stopped world machine from its receipt with fresh host handles.
pub(crate) fn restore_machine(smolvm: &Path, name: &str, checkpoint: &Path) -> Result<()> {
    let mut command = Command::new(smolvm);
    command
        .args(["machine", "restore", "--name", name, "--checkpoint"])
        .arg(checkpoint);
    companion_adapter::captured_status(Operation::Restore, &mut command)
}

pub(crate) fn status_result(action: &str, status: ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("{action} exited with {status}"))
    }
}

pub(crate) fn cleanup_machines(smolvm: &Path, state: Option<&WorldAllocationState>) {
    let Some(state) = state else {
        return;
    };
    // `machine delete -f` owns the stop-then-remove sequence. Calling
    // `machine stop` separately and ignoring its result can race deletion
    // and leave an orphaned boot process. This is signal/failure cleanup, so
    // retain best-effort behavior; explicit `down` calls the checked helper.
    let _ = delete_recorded_machines(smolvm, state);
}

/// Delete only exact, validated, currently recorded companion machines.
///
/// It never lists or discovers machines beyond the durable world allocation;
/// an explicit caller receives every upstream delete failure for reconciliation.
pub(crate) fn delete_recorded_machines(
    smolvm: &Path,
    state: &WorldAllocationState,
) -> Result<()> {
    for (service, assignment) in &state.assignments {
        validate_recorded_smolvm_name(&assignment.smolvm_name).map_err(|reason| {
            format!(
                "world machine '{service}' has an unsafe recorded smolvm identity '{}': {reason}",
                assignment.smolvm_name
            )
        })?;
    }
    for (service, assignment) in &state.assignments {
        delete_machine(smolvm, &assignment.smolvm_name)
            .map_err(|error| format!("delete recorded service '{service}': {error}"))?;
    }
    Ok(())
}

/// Stop exactly the recorded world machines while retaining their names and
/// disks for a durable checkpoint restore. Unlike [`cleanup_machines`], this
/// never deletes any smolvm configuration or another world's machine.
pub(crate) fn stop_machines(smolvm: &Path, state: &WorldAllocationState) {
    for assignment in state.assignments.values() {
        let mut command = Command::new(smolvm);
        command
            .args(["machine", "stop", "--name", &assignment.smolvm_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = companion_adapter::status(Operation::Stop, &mut command);
    }
}

/// Stop one exact recorded machine and preserve its configuration/disks for a
/// later world-supervised start. Unlike the checkpoint best-effort helper,
/// this user-requested transition reports the upstream failure to the caller.
pub(crate) fn stop_machine(smolvm: &Path, name: &str) -> Result<()> {
    let mut command = Command::new(smolvm);
    command.args(["machine", "stop", "--name", name]);
    companion_adapter::status(Operation::Stop, &mut command)
}

/// Delete one exact recorded stopped machine. The caller must prove lifecycle
/// eligibility before this operation so the upstream force flag cannot widen
/// cleanup beyond the world identity boundary.
pub(crate) fn delete_machine(smolvm: &Path, name: &str) -> Result<()> {
    validate_recorded_smolvm_name(name)
        .map_err(|reason| format!("unsafe recorded smolvm identity '{name}': {reason}"))?;
    let mut command = Command::new(smolvm);
    command.args(["machine", "delete", "--name", name, "-f"]);
    companion_adapter::status(Operation::Delete, &mut command)
}

/// Release only the exact machine records named by a retained world receipt.
/// Unlike best-effort signal cleanup, this is an explicit user-facing durable
/// state transition and therefore returns the first subprocess failure instead
/// of silently broadening or abandoning the requested cleanup.
pub(crate) fn release_machines(smolvm: &Path, state: &WorldAllocationState) -> Result<()> {
    for (service, assignment) in &state.assignments {
        validate_recorded_smolvm_name(&assignment.smolvm_name).map_err(|reason| {
            format!(
                "world machine '{service}' has an unsafe recorded smolvm identity '{}': {reason}",
                assignment.smolvm_name
            )
        })?;
    }
    for assignment in state.assignments.values() {
        let mut stop = Command::new(smolvm);
        stop.args(["machine", "stop", "--name", &assignment.smolvm_name]);
        companion_adapter::status(Operation::Stop, &mut stop)?;
        let mut delete = Command::new(smolvm);
        delete.args(["machine", "delete", "--name", &assignment.smolvm_name, "-f"]);
        companion_adapter::status(Operation::Delete, &mut delete)?;
    }
    Ok(())
}

pub(crate) fn local_smolfile_path(
    config_dir: &Path,
    configured_smolfile: &Path,
) -> Result<PathBuf> {
    let configured = configured_smolfile;
    let path = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        config_dir.join(configured)
    };
    fs::canonicalize(&path).map_err(|error| format!("resolve Smolfile {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Assignment;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_launch<'a>(
        assignment: &'a Assignment,
        socket: &'a Path,
        smolfile: &'a Path,
    ) -> MachineLaunch<'a> {
        MachineLaunch {
            assignment,
            socket,
            smolfile,
        }
    }

    #[test]
    fn detects_the_macos_hypervisor_entitlement() {
        assert!(has_hypervisor_entitlement(
            b"<key>com.apple.security.hypervisor</key><true/>"
        ));
        assert!(!has_hypervisor_entitlement(b"<dict></dict>"));
    }

    #[test]
    fn machine_create_builder_keeps_the_complete_external_network_tuple() {
        let assignment = Assignment {
            ip: "10.89.0.17".parse().unwrap(),
            mac: [0x02, 0, 0, 0, 0, 0x17],
            smolvm_name: "smw-demo-api".into(),
        };
        let launch = test_launch(
            &assignment,
            Path::new("/tmp/smw-api.sock"),
            Path::new("/tmp/api.Smolfile"),
        );
        let network = NetworkConfig {
            subnet: [10, 89, 0, 0],
            gateway: "10.89.0.1".parse().unwrap(),
            dns: "10.89.0.1".parse().unwrap(),
            domain: "demo.test".into(),
            egress: false,
        };

        let invocation = build_machine_create_command(Path::new("smolvm"), &launch, &network);
        let args = invocation
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(invocation.get_program(), Path::new("smolvm"));
        assert_eq!(
            args,
            vec![
                "machine",
                "create",
                "--name",
                "smw-demo-api",
                "--smolfile",
                "/tmp/api.Smolfile",
                "--net",
                "--net-backend",
                "virtio-net",
                "--net-unixstream",
                "/tmp/smw-api.sock",
                "--net-address",
                "10.89.0.17/24",
                "--net-gateway",
                "10.89.0.1",
                "--net-dns",
                "10.89.0.1",
                "--net-mac",
                "02:00:00:00:00:17",
            ]
        );
    }

    #[test]
    fn image_material_tsv_binds_the_requested_immutable_reference() {
        let root = temporary_test_directory();
        let archive = root.join("image.tar");
        fs::write(&archive, b"prepared archive").unwrap();
        let reference = format!(
            "docker.io/library/redis@sha256:{}",
            "a".repeat(64)
        );
        let output = format!(
            "image-material-v1\t{reference}\tsha256:{}\t{}\tblake3:{}\n",
            "a".repeat(64),
            archive.display(),
            "b".repeat(64),
        );
        let material = parse_materialized_registry_archive_tsv(&output, &reference).unwrap();
        assert_eq!(material.source_reference, reference);
        assert_eq!(material.archive_path, archive.canonicalize().unwrap());
        assert_eq!(material.archive_digest, format!("blake3:{}", "b".repeat(64)));

        let mismatched = output.replacen("redis@", "other@", 1);
        assert!(parse_materialized_registry_archive_tsv(&mismatched, &material.source_reference)
            .unwrap_err()
            .contains("different source reference"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn machine_stats_tsv_accepts_the_closed_versioned_record() {
        let output =
            "machine-stats-v1\tsmw-demo-runner\trunning\t42\t4\t4096\t20\t4\t2\t2345\t128\t64\n";
        let stats = parse_machine_stats_tsv(output, "smw-demo-runner").unwrap();
        assert_eq!(stats.name, "smw-demo-runner");
        assert_eq!(stats.state, CompanionMachineState::Running);
        assert_eq!(stats.pid, Some(42));
        assert_eq!(stats.cpus, 4);
        assert_eq!(stats.memory_mb, 4096);
        assert_eq!(stats.cpu_millis, Some(2345));
        assert_eq!(stats.rss_mb, Some(128));
        assert_eq!(stats.disk_used_mb, Some(64));
    }

    #[test]
    fn machine_stats_tsv_rejects_wrong_identity_and_shape() {
        let wrong_name =
            "machine-stats-v1\tsmw-other\trunning\t42\t4\t4096\t20\t4\t2\t2345\t128\t64\n";
        assert!(parse_machine_stats_tsv(wrong_name, "smw-demo-runner")
            .unwrap_err()
            .contains("expected 'smw-demo-runner'"));

        let unknown_state =
            "machine-stats-v1\tsmw-demo-runner\tunknown\t42\t4\t4096\t20\t4\t2\t2345\t128\t64\n";
        assert!(parse_machine_stats_tsv(unknown_state, "smw-demo-runner")
            .unwrap_err()
            .contains("unknown state"));

        assert!(
            parse_machine_stats_tsv("machine-stats-v1\ttoo-short\n", "too-short")
                .unwrap_err()
                .contains("expected 12")
        );
    }

    #[test]
    fn fake_upstream_exercises_world_adapter_and_fault_boundaries() {
        let root = temporary_test_directory();
        let fake = root.join("smolvm");
        let log = root.join("calls");
        let archive = root.join("prepared.tar");
        fs::write(&archive, b"prepared archive").unwrap();
        let script = format!(
            "#!/bin/sh\n\
             printf '%s ' \"$@\" >> '{log}'\n\
             printf '\\n' >> '{log}'\n\
             if [ \"$1\" = image ] && [ \"$2\" = materialize ]; then\n\
               printf 'image-material-v1\\t%s\\tsha256:{source_digest}\\t{archive}\\tblake3:{archive_digest}\\n' \"$6\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = machine ] && [ \"$2\" = stats ]; then\n\
               printf 'machine-stats-v1\\t%s\\trunning\\t42\\t1\\t256\\t1\\t1\\t2\\t2000\\t32\\t4\\n' \"$4\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = machine ] && [ \"$2\" = status ]; then\n\
               printf '%s running\\n' \"$4\"\n\
               exit 0\n\
             fi\n\
             if [ \"$1\" = machine ] && [ \"$2\" = start ]; then exit 23; fi\n\
             if [ \"$1\" = machine ] && [ \"$2\" = stop ] && [ \"$4\" = smw-fail ]; then exit 24; fi\n\
             if [ \"$1\" = machine ] && [ \"$2\" = delete ] && [ \"$4\" = smw-delete-fail ]; then exit 25; fi\n\
            exit 0\n",
            log = log.display(),
            source_digest = "a".repeat(64),
            archive = archive.display(),
            archive_digest = "b".repeat(64),
        );
        fs::write(&fake, script).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();

        let stats = machine_stats(&fake, "smw-runner").unwrap();
        assert_eq!(stats.name, "smw-runner");
        assert_eq!(stats.pid, Some(42));
        assert_eq!(
            machine_status(&fake, "smw-runner").unwrap(),
            Some(CompanionMachineState::Running)
        );
        let reference = format!(
            "docker.io/library/redis@sha256:{}",
            "a".repeat(64)
        );
        let material = materialize_registry_archive(&fake, &reference).unwrap();
        assert_eq!(material.source_reference, reference);
        assert_eq!(material.archive_path, archive.canonicalize().unwrap());

        let assignment = Assignment {
            ip: "10.89.0.17".parse().unwrap(),
            mac: [0x02, 0, 0, 0, 0, 0x17],
            smolvm_name: "smw-runner".into(),
        };
        let network = NetworkConfig {
            subnet: [10, 89, 0, 0],
            gateway: "10.89.0.1".parse().unwrap(),
            dns: "10.89.0.1".parse().unwrap(),
            domain: "demo.test".into(),
            egress: true,
        };
        let seeds = [SeedFile {
            source: root.join("seed"),
            destination: PathBuf::from("/etc/demo/seed"),
            mode: 0o640,
        }];
        create_machine(
            &fake,
            test_launch(
                &assignment,
                &root.join("runner.sock"),
                &root.join("runner.Smolfile"),
            ),
            &network,
        )
        .unwrap();
        assert!(start_machine(&fake, "smw-runner")
            .unwrap_err()
            .contains("start"));
        checkpoint_machine(&fake, "smw-runner", &root.join("checkpoint")).unwrap();
        restore_machine(&fake, "smw-runner", &root.join("checkpoint")).unwrap();
        exec_machine(
            &fake,
            "smw-runner",
            &ExecOptions {
                secret_env: vec!["TOKEN=HOST_TOKEN".into()],
                ..ExecOptions::default()
            },
            &["/bin/sh".into(), "-c".into(), "true".into()],
        )
        .unwrap();
        copy_machine(
            &fake,
            "smw-runner",
            "/tmp/guest-file",
            &root.join("host-file").display().to_string(),
            true,
        )
        .unwrap();
        install_seed_files(&fake, "smw-runner", &seeds).unwrap();

        let failing_state = WorldAllocationState {
            seed: 1,
            assignments: BTreeMap::from([(
                "runner".into(),
                Assignment {
                    ip: "10.89.0.18".parse().unwrap(),
                    mac: [0x02, 0, 0, 0, 0, 0x18],
                    smolvm_name: "smw-fail".into(),
                },
            )]),
        };
        assert!(release_machines(&fake, &failing_state)
            .unwrap_err()
            .contains("stop"));
        let delete_failing_state = WorldAllocationState {
            seed: 2,
            assignments: BTreeMap::from([(
                "runner".into(),
                Assignment {
                    ip: "10.89.0.19".parse().unwrap(),
                    mac: [0x02, 0, 0, 0, 0, 0x19],
                    smolvm_name: "smw-delete-fail".into(),
                },
            )]),
        };
        assert!(delete_recorded_machines(&fake, &delete_failing_state)
            .unwrap_err()
            .contains("delete"));

        let calls = fs::read_to_string(&log).unwrap();
        assert!(calls.contains("machine create --name smw-runner"));
        assert!(calls.contains("image materialize --format tsv --reference docker.io/library/redis@sha256:"));
        assert!(calls.contains("--net-unixstream"));
        assert!(calls.contains("--net-egress"));
        assert!(!calls.contains("--seed-file"));
        assert!(calls.contains("machine cp "));
        assert!(calls.contains("/bin/chmod 0640 /etc/demo/seed"));
        assert!(calls.contains("machine checkpoint --name smw-runner"));
        assert!(calls.contains("machine restore --name smw-runner"));
        assert!(calls.contains(
            "machine exec --name smw-runner --secret-env TOKEN=HOST_TOKEN -- /bin/sh -c true"
        ));
        assert!(calls.contains("machine stop --name smw-fail"));
        assert!(!calls.contains("machine delete --name smw-fail -f"));
        assert!(calls.contains("machine delete --name smw-delete-fail -f"));
        fs::remove_dir_all(root).unwrap();
    }

    fn temporary_test_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        for _ in 0..16 {
            let sequence = TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "smolworld-smolvm-test-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&root) {
                Ok(()) => return root,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory {}: {error}", root.display()),
            }
        }
        panic!("allocate a unique smolvm test directory")
    }
}
