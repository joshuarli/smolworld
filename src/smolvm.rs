use crate::model::{format_mac, Assignment, MachineLaunch, NetworkConfig, WorldConfig, WorldState};
use crate::Result;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// One resolved external-world Smolfile observation returned by the companion
/// smolvm command. It contains only the material identity smolworld must lock;
/// workload fields remain owned by smolvm and are intentionally not duplicated
/// here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalWorldMaterial {
    pub(crate) smolfile: PathBuf,
    pub(crate) local_archive: PathBuf,
    pub(crate) image_digest: String,
}

/// The host-side result of smolvm's explicit external-world preparation
/// boundary. The authored Smolfile remains the sealed user declaration;
/// `prepared_smolfile` is its verified local-only equivalent used at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedExternalWorldSmolfile {
    pub(crate) authored_smolfile: PathBuf,
    pub(crate) prepared_smolfile: PathBuf,
    pub(crate) source_kind: String,
    pub(crate) source_reference: String,
    pub(crate) source_digest: String,
}

const EXTERNAL_WORLD_TSV_ABI: &str = "external-world-v2";
const EXTERNAL_WORLD_PREPARE_TSV_ABI: &str = "external-world-prepare-v2";

/// Invoke the only mutating Smolfile image boundary. smolvm owns registry
/// protocol and archive construction; smolworld consumes only the versioned
/// prepared Smolfile identity and never parses Smolfile syntax itself.
pub(crate) fn materialize_external_world(
    smolvm: &Path,
    smolfile: &Path,
) -> Result<PreparedExternalWorldSmolfile> {
    let output = Command::new(smolvm)
        .args([
            "smolfile",
            "materialize-external",
            "--format",
            "tsv",
            "--smolfile",
        ])
        .arg(smolfile)
        .output()
        .map_err(|error| format!("run smolvm external-world materialization: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "smolvm external-world materialization exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout).map_err(|_| {
        "smolvm external-world materialization emitted non-UTF-8 output".to_string()
    })?;
    parse_external_world_prepare_tsv(stdout, smolfile)
}

fn parse_external_world_prepare_tsv(
    output: &str,
    expected_authored_smolfile: &Path,
) -> Result<PreparedExternalWorldSmolfile> {
    let record = output.strip_suffix('\n').ok_or_else(|| {
        "smolvm external-world materialization must end with one newline".to_string()
    })?;
    if record.contains('\n') || record.contains('\r') {
        return Err("smolvm external-world materialization emitted more than one record".into());
    }
    let fields: Vec<_> = record.split('\t').collect();
    if fields.len() != 6 {
        return Err(format!(
            "smolvm external-world materialization returned {} TSV fields, expected 6",
            fields.len()
        ));
    }
    let [abi, authored, prepared, source_kind, source_reference, source_digest]: [&str; 6] =
        fields.try_into().expect("field count checked above");
    if abi != EXTERNAL_WORLD_PREPARE_TSV_ABI {
        return Err(format!(
            "unsupported smolvm external-world preparation ABI '{abi}'"
        ));
    }
    let expected_authored = fs::canonicalize(expected_authored_smolfile).map_err(|error| {
        format!(
            "resolve authored Smolfile {}: {error}",
            expected_authored_smolfile.display()
        )
    })?;
    if Path::new(authored) != expected_authored {
        return Err(
            "smolvm external-world materialization returned a different authored Smolfile path"
                .into(),
        );
    }
    let prepared_smolfile = PathBuf::from(prepared);
    if !prepared_smolfile.is_absolute() {
        return Err(
            "smolvm external-world materialization returned a relative prepared Smolfile".into(),
        );
    }
    let prepared_smolfile = fs::canonicalize(&prepared_smolfile).map_err(|error| {
        format!(
            "resolve prepared Smolfile {}: {error}",
            prepared_smolfile.display()
        )
    })?;
    if !matches!(source_kind, "registry" | "local-archive") {
        return Err(format!(
            "smolvm external-world materialization returned unknown source kind '{source_kind}'"
        ));
    }
    if source_reference.is_empty() || source_reference.contains(['\t', '\r', '\n']) {
        return Err(
            "smolvm external-world materialization returned an invalid source reference".into(),
        );
    }
    let source_digest_is_valid = match source_kind {
        "registry" => is_algorithm_digest(source_digest, "sha256"),
        "local-archive" => is_algorithm_digest(source_digest, "blake3"),
        _ => false,
    };
    if !source_digest_is_valid {
        return Err(
            "smolvm external-world materialization returned an invalid source digest".into(),
        );
    }
    Ok(PreparedExternalWorldSmolfile {
        authored_smolfile: expected_authored,
        prepared_smolfile,
        source_kind: source_kind.to_string(),
        source_reference: source_reference.to_string(),
        source_digest: source_digest.to_string(),
    })
}

/// Invoke smolvm's read-only external-world resolver and parse its deliberately
/// small versioned TSV record. This must run before smolworld allocates v2
/// state, binds a listener, or creates a machine.
pub(crate) fn validate_external_world(
    smolvm: &Path,
    smolfile: &Path,
    assignment: &Assignment,
    socket: &Path,
    network: &NetworkConfig,
) -> Result<ExternalWorldMaterial> {
    let address = format!("{}/24", assignment.ip);
    let output = Command::new(smolvm)
        .args([
            "smolfile",
            "validate-external",
            "--format",
            "tsv",
            "--smolfile",
        ])
        .arg(smolfile)
        .args(["--net-unixstream"])
        .arg(socket)
        .args(["--net-address", &address, "--net-gateway"])
        .arg(network.gateway.to_string())
        .args(["--net-dns"])
        .arg(network.dns.to_string())
        .args(["--net-mac"])
        .arg(format_mac(assignment.mac))
        .output()
        .map_err(|error| format!("run smolvm external-world validation: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "smolvm external-world validation exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| "smolvm external-world validation emitted non-UTF-8 output".to_string())?;
    parse_external_world_tsv(stdout, smolfile, assignment, socket, network)
}

fn parse_external_world_tsv(
    output: &str,
    expected_smolfile: &Path,
    assignment: &Assignment,
    expected_socket: &Path,
    network: &NetworkConfig,
) -> Result<ExternalWorldMaterial> {
    let record = output
        .strip_suffix('\n')
        .ok_or_else(|| "smolvm external-world validation must end with one newline".to_string())?;
    if record.contains('\n') || record.contains('\r') {
        return Err("smolvm external-world validation emitted more than one record".into());
    }
    let fields: Vec<_> = record.split('\t').collect();
    if fields.len() != 10 {
        return Err(format!(
            "smolvm external-world validation returned {} TSV fields, expected 10",
            fields.len()
        ));
    }
    let [abi, smolfile, image_kind, image_locator, image_digest, socket, guest_cidr, gateway, dns, mac]: [&str; 10] =
        fields.try_into().expect("field count checked above");
    if abi != EXTERNAL_WORLD_TSV_ABI {
        return Err(format!("unsupported smolvm external-world ABI '{abi}'"));
    }
    let canonical_smolfile = fs::canonicalize(expected_smolfile)
        .map_err(|error| format!("resolve Smolfile {}: {error}", expected_smolfile.display()))?;
    if Path::new(smolfile) != canonical_smolfile {
        return Err("smolvm external-world validation returned a different Smolfile path".into());
    }
    if socket != expected_socket.to_string_lossy() {
        return Err("smolvm external-world validation returned a different Unix socket".into());
    }
    let expected_cidr = format!("{}/24", assignment.ip);
    if guest_cidr != expected_cidr
        || gateway != network.gateway.to_string()
        || dns != network.dns.to_string()
        || mac != format_mac(assignment.mac)
    {
        return Err(
            "smolvm external-world validation returned a mismatched static network tuple".into(),
        );
    }
    match image_kind {
        "local-archive" => {
            let local_archive = PathBuf::from(image_locator);
            if !local_archive.is_absolute() {
                return Err("smolvm external-world validation returned a relative local archive".into());
            }
            if is_algorithm_digest(image_digest, "blake3") {
                return Ok(ExternalWorldMaterial {
                    smolfile: canonical_smolfile,
                    local_archive,
                    image_digest: image_digest.to_string(),
                });
            }
            Err("smolvm external-world validation returned an invalid local archive digest".into())
        }
        "local-directory" => Err(
            "external worlds currently require a prepared local archive; directory material has no sealed tree digest"
                .into(),
        ),
        "registry" => Err(format!(
            "external world image '{image_locator}' is an immutable registry reference but no host materializer is available; provide a prepared local archive in the Smolfile"
        )),
        other => Err(format!("smolvm external-world validation returned unknown image kind '{other}'")),
    }
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
    #[cfg(target_os = "macos")]
    {
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
    #[cfg(not(target_os = "macos"))]
    {
        let _ = smolvm;
        Ok(())
    }
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
    let names = if cfg!(target_os = "macos") {
        ["libkrun.dylib", "libkrunfw.5.dylib"]
    } else {
        ["libkrun.so", "libkrunfw.so"]
    };
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
    let name = launch.assignment.smolvm_name.clone();
    let mut invocation = build_machine_create_command(smolvm, &launch, network);
    status_result(
        &format!("create machine {name}"),
        invocation
            .status()
            .map_err(|error| format!("run smolvm machine create: {error}"))?,
    )
}

/// Build the exact, restricted `machine create` invocation without starting
/// it. The Smolfile is the machine declaration; smolworld supplies only its
/// generated identity, complete external NIC tuple, and already-sealed
/// pre-workload seed copies.
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
    for seed in launch.seed_files {
        invocation.args(["--seed-file"]);
        invocation.arg(format!(
            "{}={}:{:04o}",
            seed.source.display(),
            seed.destination.display(),
            seed.mode
        ));
    }
    invocation
}

pub(crate) fn start_machine(smolvm: &Path, name: &str) -> Result<()> {
    status_result(
        &format!("start machine {name}"),
        Command::new(smolvm)
            .args(["machine", "start", "--name", name])
            .status()
            .map_err(|error| format!("run smolvm machine start: {error}"))?,
    )
}

pub(crate) fn status_result(action: &str, status: ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("{action} exited with {status}"))
    }
}

pub(crate) fn cleanup_machines(smolvm: &Path, state: Option<&WorldState>) {
    let Some(state) = state else {
        return;
    };
    for assignment in state.assignments.values() {
        let _ = Command::new(smolvm)
            .args(["machine", "stop", "--name", &assignment.smolvm_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new(smolvm)
            .args(["machine", "delete", "--name", &assignment.smolvm_name, "-f"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
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
    use crate::model::{Assignment, SeedFile};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_launch<'a>(
        assignment: &'a Assignment,
        socket: &'a Path,
        smolfile: &'a Path,
        seed_files: &'a [SeedFile],
    ) -> MachineLaunch<'a> {
        MachineLaunch {
            assignment,
            socket,
            smolfile,
            seed_files,
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
            smolvm_name: "smw-v2-demo-api".into(),
        };
        let seed_files = vec![SeedFile {
            source: PathBuf::from("/tmp/clickhouse-config.xml"),
            destination: PathBuf::from("/etc/clickhouse-server/config.d/world.xml"),
            mode: 0o644,
        }];
        let launch = test_launch(
            &assignment,
            Path::new("/tmp/smw-v2-api.sock"),
            Path::new("/tmp/api.Smolfile"),
            &seed_files,
        );
        let network = NetworkConfig {
            subnet: [10, 89, 0, 0],
            gateway: "10.89.0.1".parse().unwrap(),
            dns: "10.89.0.1".parse().unwrap(),
            domain: "demo.test".into(),
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
                "smw-v2-demo-api",
                "--smolfile",
                "/tmp/api.Smolfile",
                "--net",
                "--net-backend",
                "virtio-net",
                "--net-unixstream",
                "/tmp/smw-v2-api.sock",
                "--net-address",
                "10.89.0.17/24",
                "--net-gateway",
                "10.89.0.1",
                "--net-dns",
                "10.89.0.1",
                "--net-mac",
                "02:00:00:00:00:17",
                "--seed-file",
                "/tmp/clickhouse-config.xml=/etc/clickhouse-server/config.d/world.xml:0644",
            ]
        );
    }

    #[test]
    fn external_world_tsv_binds_the_exact_smolfile_material_and_network_tuple() {
        let root = temporary_test_directory();
        let smolfile = root.join("machine.Smolfile");
        let archive = root.join("image.tar");
        fs::write(&smolfile, "image = \"./image.tar\"\n").unwrap();
        fs::write(&archive, b"prepared archive").unwrap();
        let assignment = Assignment {
            ip: "10.89.0.17".parse().unwrap(),
            mac: [0x02, 0, 0, 0, 0, 0x17],
            smolvm_name: "smw-v2-demo-machine".into(),
        };
        let network = NetworkConfig {
            subnet: [10, 89, 0, 0],
            gateway: "10.89.0.1".parse().unwrap(),
            dns: "10.89.0.1".parse().unwrap(),
            domain: "demo.test".into(),
        };
        let socket = Path::new("/tmp/smw-v2-demo-machine.sock");
        let canonical_smolfile = fs::canonicalize(&smolfile).unwrap();
        let canonical_archive = fs::canonicalize(&archive).unwrap();
        let output = format!(
            "external-world-v2\t{}\tlocal-archive\t{}\tblake3:{}\t{}\t10.89.0.17/24\t10.89.0.1\t10.89.0.1\t02:00:00:00:00:17\n",
            canonical_smolfile.display(),
            canonical_archive.display(),
            "a".repeat(64),
            socket.display(),
        );

        let material =
            parse_external_world_tsv(&output, &smolfile, &assignment, socket, &network).unwrap();

        assert_eq!(material.smolfile, canonical_smolfile);
        assert_eq!(material.local_archive, canonical_archive);
        assert_eq!(material.image_digest, format!("blake3:{}", "a".repeat(64)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_world_tsv_rejects_unsealed_or_mismatched_material() {
        let root = temporary_test_directory();
        let smolfile = root.join("machine.Smolfile");
        fs::write(&smolfile, "image = \"./image.tar\"\n").unwrap();
        let assignment = Assignment {
            ip: "10.89.0.17".parse().unwrap(),
            mac: [0x02, 0, 0, 0, 0, 0x17],
            smolvm_name: "smw-v2-demo-machine".into(),
        };
        let network = NetworkConfig {
            subnet: [10, 89, 0, 0],
            gateway: "10.89.0.1".parse().unwrap(),
            dns: "10.89.0.1".parse().unwrap(),
            domain: "demo.test".into(),
        };
        let canonical_smolfile = fs::canonicalize(&smolfile).unwrap();
        let output = format!(
            "external-world-v2\t{}\tregistry\tdocker.io/library/redis@sha256:{}\tsha256:{}\t/tmp/smw-v2-demo-machine.sock\t10.89.0.17/24\t10.89.0.1\t10.89.0.1\t02:00:00:00:00:17\n",
            canonical_smolfile.display(),
            "a".repeat(64),
            "a".repeat(64),
        );

        let error = parse_external_world_tsv(
            &output,
            &smolfile,
            &assignment,
            Path::new("/tmp/smw-v2-demo-machine.sock"),
            &network,
        )
        .unwrap_err();

        assert!(error.contains("no host materializer is available"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_world_tsv_rejects_sha256_as_local_archive_identity() {
        let root = temporary_test_directory();
        let smolfile = root.join("machine.Smolfile");
        let archive = root.join("image.tar");
        fs::write(&smolfile, "image = \"./image.tar\"\n").unwrap();
        fs::write(&archive, b"prepared archive").unwrap();
        let assignment = Assignment {
            ip: "10.89.0.17".parse().unwrap(),
            mac: [0x02, 0, 0, 0, 0, 0x17],
            smolvm_name: "smw-v2-demo-machine".into(),
        };
        let network = NetworkConfig {
            subnet: [10, 89, 0, 0],
            gateway: "10.89.0.1".parse().unwrap(),
            dns: "10.89.0.1".parse().unwrap(),
            domain: "demo.test".into(),
        };
        let socket = Path::new("/tmp/smw-v2-demo-machine.sock");
        let output = format!(
            "external-world-v2\t{}\tlocal-archive\t{}\tsha256:{}\t{}\t10.89.0.17/24\t10.89.0.1\t10.89.0.1\t02:00:00:00:00:17\n",
            fs::canonicalize(&smolfile).unwrap().display(),
            fs::canonicalize(&archive).unwrap().display(),
            "a".repeat(64),
            socket.display(),
        );

        let error = parse_external_world_tsv(&output, &smolfile, &assignment, socket, &network)
            .unwrap_err();
        assert!(error.contains("invalid local archive digest"));
        fs::remove_dir_all(root).unwrap();
    }

    fn temporary_test_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "smolworld-smolvm-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }
}
