use crate::model::{format_mac, MachineLaunch, NetworkConfig, WorldConfig, WorldPaths, WorldState};
use crate::Result;
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

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
pub(crate) fn preflight(config: &WorldConfig, paths: &WorldPaths, smolvm: &Path) -> Result<()> {
    require_smolvm(smolvm)?;
    for machine in config.machines.values() {
        let image = local_image_path(&paths.config_dir, &machine.image)?;
        let metadata = fs::metadata(&image)
            .map_err(|error| format!("inspect local image {}: {error}", image.display()))?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(format!(
                "local image {} must be a Docker archive or unpacked rootfs directory",
                image.display()
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
    let address = format!("{}/24", launch.assignment.ip);
    let cpus = launch.resources.cpus.to_string();
    let memory_mib = launch.resources.memory_mib.to_string();
    let storage_gib = launch.resources.storage_gib.to_string();
    let overlay_gib = launch.resources.overlay_gib.to_string();
    let mut invocation = Command::new(smolvm);
    invocation
        .args(["machine", "create", "--name"])
        .arg(&launch.assignment.smolvm_name)
        .args(["--image"])
        .arg(launch.image)
        // The config validator has already made each resource value positive
        // and retained the intentionally small local-world defaults.
        .args([
            "--cpus",
            &cpus,
            "--mem",
            &memory_mib,
            "--storage",
            &storage_gib,
            "--overlay",
            &overlay_gib,
        ])
        .args(["--net", "--net-backend", "virtio-net", "--net-unixstream"])
        .arg(launch.socket)
        .args(["--net-address", &address, "--net-gateway"])
        .arg(network.gateway.to_string())
        .args(["--net-dns"])
        .arg(network.dns.to_string())
        .args(["--net-mac"])
        .arg(format_mac(launch.assignment.mac));
    if !launch.command.is_empty() {
        invocation.arg("--").args(launch.command);
    }
    status_result(
        &format!("create machine {}", launch.assignment.smolvm_name),
        invocation
            .status()
            .map_err(|error| format!("run smolvm machine create: {error}"))?,
    )
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

pub(crate) fn local_image_path(config_dir: &Path, configured_image: &str) -> Result<PathBuf> {
    let configured = Path::new(configured_image);
    let path = if configured.is_absolute() {
        configured.to_path_buf()
    } else if configured_image.starts_with("./") || configured_image.starts_with("../") {
        config_dir.join(configured)
    } else {
        return Err(format!(
            "image '{configured_image}' is a registry reference. This isolated PoC requires a local docker-save archive or unpacked rootfs path; use an absolute path or ./relative-path"
        ));
    };
    fs::canonicalize(&path)
        .map_err(|error| format!("resolve local image {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_macos_hypervisor_entitlement() {
        assert!(has_hypervisor_entitlement(
            b"<key>com.apple.security.hypervisor</key><true/>"
        ));
        assert!(!has_hypervisor_entitlement(b"<dict></dict>"));
    }
}
