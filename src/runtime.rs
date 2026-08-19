use crate::cli::{
    format_ps, format_stats_json, format_stats_table, format_stats_template, Cli, ConfigFormat,
    ExecOptions, ImagesFormat, LifecycleCommand, LifecycleState as DisplayLifecycleState,
    MachineStatus, PsFormat, ServiceStats, StatsFormat,
};
use crate::config::{load_config, topological_order, topological_waves};
use crate::gateway::Gateway;
use crate::model::{
    format_mac, LifecycleState, MachineCheckpointReceipt, MachineLaunch, SeedFile,
    WorldCheckpointReceipt, WorldConfig, WORLD_CHECKPOINT_RECEIPT_VERSION,
};
use crate::smolvm::{
    checkpoint_machine, cleanup_machines, copy_machine, create_machine, delete_machine,
    delete_recorded_machines,
    exec_machine, install_seed_files as install_machine_seed_files, machine_stats,
    machine_status as upstream_machine_status, preflight, release_machines, restore_machine,
    smolvm_program, start_machine, stop_machine, stop_machines, CompanionMachineState,
    MachineStats,
};
use crate::state::{
    allocate_allocation_state, digest_file, digest_machine_checkpoint_receipt, inspect_recovery,
    load_allocation_state, load_lifecycle, load_material_lock, load_world_checkpoint_receipt,
    mark_absent, mark_attached, mark_capture_rolled_back, mark_captured, mark_capturing,
    mark_created, mark_created_detached, mark_running, mark_starting,
    normalize_relative_path, prepare_runtime_dir, remove_runtime_dir, remove_stale_temporary_files,
    world_paths, write_allocation_state, write_material_lock, write_world_checkpoint_receipt,
    ImageMaterial, MaterialLock, SeedObservation, SmolfileObservation, WorldLock, WorldPaths,
    validate_recorded_smolvm_name, MACHINE_CHECKPOINT_RECEIPT_NAME,
};
use crate::switch::{
    port_socket_path, print_allocations, run_switch, spawn_port_acceptor, wait_for_attachments,
    wait_for_expected_attachments, SwitchEvent,
};
use crate::Result;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod checkpoint;
mod material;

use checkpoint::{checkpoint_running_world, verify_world_checkpoint_receipt};
use material::{prepare_world_material, prepared_seed_files, verify_prepared_world};

#[cfg(test)]
use checkpoint::create_world_checkpoint_staging;
#[cfg(test)]
use material::{validate_seed_destination, validate_seed_source_for_copy};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_stop_handler(_: i32) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

unsafe extern "C" {
    fn signal(signal: i32, handler: extern "C" fn(i32)) -> usize;
}

fn install_signal_handlers() {
    // SIGINT and SIGTERM on Darwin. This PoC is intentionally macOS-only.
    unsafe {
        signal(2, signal_stop_handler);
        signal(15, signal_stop_handler);
    }
}

pub(crate) fn run(cli: Cli) -> Result<()> {
    match cli {
        Cli::Help { .. } | Cli::Version => {
            Err("help and version must be handled by the CLI entrypoint".into())
        }
        Cli::VersionCommand { short, format } => version(short, format.as_deref()),
        Cli::Up {
            config,
            services,
            detach,
        } => up(&config, &services, detach),
        Cli::Create { config, services } => create(&config, &services),
        Cli::Start { config, services } => lifecycle(&config, LifecycleCommand::Start, &services),
        Cli::Stop { config, services } => lifecycle(&config, LifecycleCommand::Stop, &services),
        Cli::Restart { config, services } => {
            lifecycle(&config, LifecycleCommand::Restart, &services)
        }
        Cli::Rm { config, services } => lifecycle(&config, LifecycleCommand::Rm, &services),
        Cli::Check { config, deep } => check(&config, deep),
        Cli::Prepare { config } => prepare(&config),
        Cli::Checkpoint { config, output } => checkpoint(&config, &output),
        Cli::Restore { config, checkpoint } => restore(&config, &checkpoint),
        Cli::Release { config, checkpoint } => release(&config, &checkpoint),
        Cli::Down { config } => down(&config),
        Cli::Ps {
            config,
            services,
            all,
            status,
            quiet,
            services_only,
            format,
        } => ps(
            &config,
            &services,
            all,
            status,
            quiet || services_only,
            &format,
        ),
        Cli::Stats {
            config,
            services,
            all,
            no_stream,
            format,
        } => stats(&config, &services, all, no_stream, &format),
        Cli::Images {
            config,
            services,
            format,
        } => images(&config, &services, format),
        Cli::Config {
            config: config_path,
            format,
            quiet,
        } => config(&config_path, format, quiet),
        Cli::Exec {
            config,
            service,
            options,
            command,
        } => exec(&config, &service, &options, &command),
        Cli::Shell { config, service } => shell(&config, &service),
        Cli::Cp {
            config,
            source,
            destination,
        } => copy(&config, &source, &destination),
    }
}

pub(crate) fn check(config_path: &Path, deep: bool) -> Result<()> {
    let config = load_config(config_path)?;
    topological_order(&config)?;
    let paths = world_paths(config_path)?;
    verify_prepared_world(&config, &paths, &smolvm_program(), deep)?;
    println!("smolworld: {} is ready", config.name);
    Ok(())
}

/// Seal all host-inspectable machine inputs into `.smolworld.lock` without
/// allocating world state, binding a listener, or creating a smolvm machine.
pub(crate) fn prepare(config_path: &Path) -> Result<()> {
    let config = load_config(config_path)?;
    topological_order(&config)?;
    let paths = world_paths(config_path)?;
    let smolvm = smolvm_program();
    preflight(&config, &paths.config_dir, &smolvm)?;
    let material = prepare_world_material(&config, &paths, &smolvm)?;
    write_material_lock(&paths, &material)?;
    println!("smolworld: prepared {}", config.name);
    Ok(())
}

/// Render the resolved strict world declaration. Unlike `check`, this does
/// not inspect sealed material or runtime artifacts; it is configuration
/// validation and presentation only.
pub(crate) fn config(config_path: &Path, format: ConfigFormat, quiet: bool) -> Result<()> {
    let config = load_config(config_path)?;
    topological_order(&config)?;
    if quiet {
        return Ok(());
    }
    let output = match format {
        ConfigFormat::Yaml => format_config_yaml(&config),
        ConfigFormat::Json => format_config_json(&config),
    };
    println!("{output}");
    Ok(())
}

fn format_config_yaml(config: &WorldConfig) -> String {
    let mut output = String::from("format: 2\nworld:\n  name: ");
    push_yaml_string(&mut output, &config.name);
    output.push_str("\nnetwork:\n  subnet: ");
    push_yaml_string(
        &mut output,
        &format!(
            "{}.{}.{}.{}/24",
            config.network.subnet[0],
            config.network.subnet[1],
            config.network.subnet[2],
            config.network.subnet[3]
        ),
    );
    output.push_str("\n  gateway: ");
    push_yaml_string(&mut output, &config.network.gateway.to_string());
    output.push_str("\n  dns: ");
    push_yaml_string(&mut output, &config.network.dns.to_string());
    output.push_str("\n  domain: ");
    push_yaml_string(&mut output, &config.network.domain);
    output.push_str("\n  egress: ");
    output.push_str(if config.network.egress {
        "true"
    } else {
        "false"
    });
    output.push_str("\nmachines:");
    for (name, machine) in &config.machines {
        output.push_str("\n  ");
        output.push_str(name);
        output.push_str(":\n    smolfile: ");
        push_yaml_string(&mut output, &machine.smolfile.to_string_lossy());
        if !machine.depends_on.is_empty() {
            output.push_str("\n    depends_on:");
            for dependency in &machine.depends_on {
                output.push_str("\n      - ");
                push_yaml_string(&mut output, dependency);
            }
        }
        if !machine.seed_files.is_empty() {
            output.push_str("\n    seed_files:");
            for seed in &machine.seed_files {
                output.push_str("\n      - source: ");
                push_yaml_string(&mut output, &seed.source.to_string_lossy());
                output.push_str("\n        destination: ");
                push_yaml_string(&mut output, &seed.destination.to_string_lossy());
                output.push_str("\n        mode: ");
                push_yaml_string(&mut output, &format!("{:04o}", seed.mode));
            }
        }
    }
    output
}

fn push_yaml_string(output: &mut String, value: &str) {
    crate::cli::push_json_string(output, value);
}

fn format_config_json(config: &WorldConfig) -> String {
    let mut output = String::from("{\"format\":2,\"world\":{\"name\":");
    crate::cli::push_json_string(&mut output, &config.name);
    output.push_str("},\"network\":{\"subnet\":");
    crate::cli::push_json_string(
        &mut output,
        &format!(
            "{}.{}.{}.{}/24",
            config.network.subnet[0],
            config.network.subnet[1],
            config.network.subnet[2],
            config.network.subnet[3]
        ),
    );
    output.push_str(",\"gateway\":");
    crate::cli::push_json_string(&mut output, &config.network.gateway.to_string());
    output.push_str(",\"dns\":");
    crate::cli::push_json_string(&mut output, &config.network.dns.to_string());
    output.push_str(",\"domain\":");
    crate::cli::push_json_string(&mut output, &config.network.domain);
    output.push_str(",\"egress\":");
    output.push_str(if config.network.egress {
        "true"
    } else {
        "false"
    });
    output.push_str("},\"machines\":{");
    for (index, (name, machine)) in config.machines.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        crate::cli::push_json_string(&mut output, name);
        output.push_str(":{\"smolfile\":");
        crate::cli::push_json_string(&mut output, &machine.smolfile.to_string_lossy());
        output.push_str(",\"depends_on\":[");
        for (index, dependency) in machine.depends_on.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            crate::cli::push_json_string(&mut output, dependency);
        }
        output.push_str("],\"seed_files\":[");
        for (index, seed) in machine.seed_files.iter().enumerate() {
            if index != 0 {
                output.push(',');
            }
            output.push_str("{\"source\":");
            crate::cli::push_json_string(&mut output, &seed.source.to_string_lossy());
            output.push_str(",\"destination\":");
            crate::cli::push_json_string(&mut output, &seed.destination.to_string_lossy());
            output.push_str(",\"mode\":");
            crate::cli::push_json_string(&mut output, &format!("{:04o}", seed.mode));
            output.push('}');
        }
        output.push_str("]}");
    }
    output.push_str("}}");
    output
}

fn version(short: bool, format: Option<&str>) -> Result<()> {
    if short {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if format == Some("json") {
        println!(
            "{{\"name\":\"smolworld\",\"version\":\"{}\",\"gitCommit\":\"{}\"}}",
            env!("CARGO_PKG_VERSION"),
            env!("SMOLWORLD_GIT_SHA")
        );
        return Ok(());
    }
    println!("{}", crate::cli::version());
    Ok(())
}

/// Create selected exact machine records without launching a switch. `start`
/// later enters the normal supervisor path, binds the deterministic listeners,
/// and starts these same recorded identities.
pub(crate) fn create(config_path: &Path, requested_services: &[String]) -> Result<()> {
    let config = load_config(config_path)?;
    let waves = topological_waves(&config)?;
    let selected = selected_services_with_dependencies(&config, requested_services)?;
    let paths = world_paths(config_path)?;
    let smolvm = smolvm_program();
    let _world_lock = WorldLock::acquire(&paths)?;
    let material = verify_prepared_world(&config, &paths, &smolvm, false)?;
    let recovery = inspect_recovery(&paths)?;
    if recovery.lifecycle.state.retains_checkpoint_sources() {
        return Err("cannot create services for a world with a retained checkpoint".into());
    }
    if recovery.lifecycle.state != LifecycleState::Absent
        || recovery.runtime_dir == crate::model::ArtifactState::Present
    {
        return Err(
            "world already has created or running service records; use start, stop, rm, or down"
                .into(),
        );
    }
    let previous = load_allocation_state(&paths.state_file)?;
    cleanup_machines(&smolvm, previous.as_ref());
    remove_stale_temporary_files(&paths)?;
    remove_runtime_dir(&paths)?;
    let state = allocate_allocation_state(previous, &config, &paths)?;
    write_allocation_state(&paths, &state)?;
    mark_starting(&paths)?;
    let result = (|| {
        for wave in &waves {
            let selected_wave: Vec<_> = wave
                .iter()
                .filter(|name| selected.contains(name.as_str()))
                .cloned()
                .collect();
            parallel_machine_operations(&selected_wave, "create", |name| {
                let assignment = state.assignments.get(name).expect("allocated machine");
                let smolfile = material
                    .smolfiles
                    .get(name)
                    .expect("prepared material has every configured machine");
                create_machine(
                    &smolvm,
                    MachineLaunch {
                        assignment,
                        socket: &port_socket_path(&paths.runtime_dir, name),
                        smolfile: &smolfile.prepared_path,
                    },
                    &config.network,
                )
            })?;
        }
        mark_created_detached(&paths)?;
        println!("smolworld: created {}", config.name);
        Ok(())
    })();
    if result.is_err() {
        cleanup_machines(&smolvm, Some(&state));
        let _ = mark_absent(&paths);
    }
    result
}

/// Dispatch a service transition to the process that currently owns the
/// switch. A stopped supervisor is never reconstructed by guessing at sockets
/// or unrelated smolvm records.
pub(crate) fn lifecycle(
    config_path: &Path,
    action: LifecycleCommand,
    requested_services: &[String],
) -> Result<()> {
    let config = load_config(config_path)?;
    let selected = selected_services(&config, requested_services)?;
    let paths = world_paths(config_path)?;
    let start_requested = matches!(action, LifecycleCommand::Start);
    let verb = action.name();
    let encoded = encode_control_services(&selected)?;
    let Some(reply) = try_send_runtime_control(&paths, &format!("{verb}\t{encoded}\n"))? else {
        if start_requested {
            let lifecycle = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
            if lifecycle.state == LifecycleState::Created {
                return spawn_detached_up(config_path, &selected);
            }
        }
        return Err(format!(
            "world supervisor is not running at {}; use `smolworld up -d` before {verb}",
            runtime_control_socket_path(&paths).display()
        ));
    };
    if reply == "OK" {
        println!("smolworld: {verb}");
        Ok(())
    } else if let Some(error) = reply.strip_prefix("ERR ") {
        Err(format!("world {verb} failed: {error}"))
    } else {
        Err("world supervisor returned a malformed lifecycle reply".into())
    }
}

fn spawn_detached_up(config_path: &Path, services: &[String]) -> Result<()> {
    let config = load_config(config_path)?;
    selected_services_with_dependencies(&config, services)?;
    let paths = world_paths(config_path)?;
    verify_prepared_world(&config, &paths, &smolvm_program(), false)?;
    let executable = std::env::current_exe()
        .map_err(|error| format!("resolve smolworld executable: {error}"))?;
    let mut command = Command::new(executable);
    command
        .arg("--file")
        .arg(config_path)
        .arg("up")
        .args(services)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .map_err(|error| format!("start detached world supervisor: {error}"))?;
    println!("smolworld: starting in the background");
    Ok(())
}

/// Send one control request if and only if the exact world's supervisor is
/// accepting connections. A stale Unix socket after an interrupted supervisor
/// is explicitly non-live; permission, framing, and I/O failures remain hard
/// errors so callers never mistake an uncertain owner for a dead one.
fn try_send_runtime_control(paths: &WorldPaths, request: &str) -> Result<Option<String>> {
    let socket = runtime_control_socket_path(paths);
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            return Ok(None)
        }
        Err(error) => {
            return Err(format!(
                "connect world supervisor {}: {error}",
                socket.display()
            ))
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|error| format!("set supervisor reply timeout: {error}"))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write supervisor request: {error}"))?;
    read_runtime_control_line(&mut stream).map(Some)
}

/// Prove that the current process still owns this world's private control
/// socket before delegating an operation to smolvm. The acknowledgement avoids
/// treating a bound-but-not-serving pathname as a running world.
fn require_live_supervisor(paths: &WorldPaths, operation: &str) -> Result<()> {
    match try_send_runtime_control(paths, "ping\n")? {
        Some(reply) if reply == "OK" => Ok(()),
        Some(_) => Err("world supervisor returned a malformed liveness reply".into()),
        None => Err(format!(
            "world supervisor is not running at {}; use `smolworld up -d` before {operation}",
            runtime_control_socket_path(paths).display()
        )),
    }
}

fn encode_control_services(services: &[String]) -> Result<String> {
    if services.is_empty() {
        return Ok("*".into());
    }
    if services
        .iter()
        .any(|service| service.contains([',', '\t', '\r', '\n']))
    {
        return Err("service name cannot be encoded for supervisor control".into());
    }
    Ok(services.join(","))
}

fn selected_services(config: &WorldConfig, requested: &[String]) -> Result<Vec<String>> {
    let selected: Vec<_> = if requested.is_empty() {
        config.machines.keys().cloned().collect()
    } else {
        requested.to_vec()
    };
    let mut seen = HashSet::new();
    for service in &selected {
        if !config.machines.contains_key(service) {
            return Err(format!("unknown world service '{service}'"));
        }
        if !seen.insert(service) {
            return Err(format!("service '{service}' was selected more than once"));
        }
    }
    Ok(selected)
}

fn selected_services_with_dependencies(
    config: &WorldConfig,
    requested: &[String],
) -> Result<HashSet<String>> {
    let selected = selected_services(config, requested)?;
    let mut result = HashSet::new();
    let mut pending = selected;
    while let Some(service) = pending.pop() {
        if !result.insert(service.clone()) {
            continue;
        }
        pending.extend(
            config
                .machines
                .get(&service)
                .expect("selected service was validated")
                .depends_on
                .iter()
                .cloned(),
        );
    }
    Ok(result)
}

/// Execute an exact service transition while the caller owns the switch. New
/// machine records use the already-bound deterministic listener path; no
/// operation scans the companion for names outside this world's allocation.
fn apply_lifecycle_control(
    config: &WorldConfig,
    state: &crate::model::WorldAllocationState,
    paths: &WorldPaths,
    smolvm: &Path,
    material: &MaterialLock,
    attached_rx: &mpsc::Receiver<String>,
    action: LifecycleCommand,
    requested_services: &[String],
) -> Result<()> {
    let services = selected_services(config, requested_services)?;
    let state_for = |service: &str| {
        state
            .assignments
            .get(service)
            .ok_or_else(|| format!("service '{service}' has no allocation"))
    };
    for service in &services {
        let assignment = state_for(service)?;
        require_machine_identity(service, &assignment.smolvm_name)?;
    }
    match action {
        LifecycleCommand::Start => {
            let mut missing = Vec::new();
            let mut to_start = Vec::new();
            for service in &services {
                let assignment = state_for(service)?;
                match machine_status(smolvm, &assignment.smolvm_name)? {
                    Some(CompanionMachineState::Running) => {}
                    Some(CompanionMachineState::Created | CompanionMachineState::Stopped) => {
                        to_start.push(service.clone())
                    }
                    Some(state) => {
                        return Err(format!(
                            "service '{service}' cannot start from companion state {}",
                            state.as_str()
                        ))
                    }
                    None => {
                        missing.push(service.clone());
                        to_start.push(service.clone());
                    }
                }
            }
            parallel_machine_operations(&missing, "create", |service| {
                let assignment = state_for(service)?;
                let smolfile = material
                    .smolfiles
                    .get(service)
                    .ok_or_else(|| format!("prepared material has no service '{service}'"))?;
                let socket = port_socket_path(&paths.runtime_dir, service);
                create_machine(
                    smolvm,
                    MachineLaunch {
                        assignment,
                        socket: &socket,
                        smolfile: &smolfile.prepared_path,
                    },
                    &config.network,
                )
            })?;
            parallel_machine_operations(&to_start, "start", |service| {
                start_machine(smolvm, &state_for(service)?.smolvm_name)
            })?;
            parallel_machine_operations(&missing, "install sealed seed files", |service| {
                let assignment = state_for(service)?;
                let seed_files = prepared_seed_files(&paths.config_dir, material, service)?;
                install_machine_seed_files(smolvm, &assignment.smolvm_name, &seed_files)
            })?;
            wait_for_expected_attachments(attached_rx, to_start.into_iter().collect())
        }
        LifecycleCommand::Stop => {
            let mut running = Vec::new();
            for service in &services {
                if machine_status(smolvm, &state_for(service)?.smolvm_name)?
                    == Some(CompanionMachineState::Running)
                {
                    running.push(service.clone());
                }
            }
            parallel_machine_operations(&running, "stop", |service| {
                stop_machine(smolvm, &state_for(service)?.smolvm_name)
            })
        }
        LifecycleCommand::Restart => {
            apply_lifecycle_control(
                config,
                state,
                paths,
                smolvm,
                material,
                attached_rx,
                LifecycleCommand::Stop,
                &services,
            )?;
            apply_lifecycle_control(
                config,
                state,
                paths,
                smolvm,
                material,
                attached_rx,
                LifecycleCommand::Start,
                &services,
            )
        }
        LifecycleCommand::Rm => {
            for service in &services {
                match machine_status(smolvm, &state_for(service)?.smolvm_name)? {
                    Some(CompanionMachineState::Created | CompanionMachineState::Stopped) => {}
                    Some(CompanionMachineState::Running) => {
                        return Err(format!("service '{service}' is running; stop it before rm"))
                    }
                    Some(state) => {
                        return Err(format!(
                            "service '{service}' cannot be removed from companion state {}",
                            state.as_str()
                        ))
                    }
                    None => {
                        return Err(format!(
                            "service '{service}' has no machine record to remove"
                        ))
                    }
                }
            }
            parallel_machine_operations(&services, "rm", |service| {
                delete_machine(smolvm, &state_for(service)?.smolvm_name)
            })
        }
    }
}

/// Ask the current supervisor, rather than a second CLI process, to close the
/// switch epoch and capture its live machines. The runtime directory socket is
/// private to this exact world and vanishes when the supervisor exits.
pub(crate) fn checkpoint(config_path: &Path, output: &Path) -> Result<()> {
    if !output.is_absolute() {
        return Err("checkpoint --output must be an absolute directory".into());
    }
    let paths = world_paths(config_path)?;
    let socket = runtime_control_socket_path(&paths);
    let mut stream = UnixStream::connect(&socket)
        .map_err(|error| format!("connect world supervisor {}: {error}", socket.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30 * 60)))
        .map_err(|error| format!("set checkpoint reply timeout: {error}"))?;
    stream
        .write_all(format!("checkpoint\t{}\n", output.display()).as_bytes())
        .map_err(|error| format!("write checkpoint request: {error}"))?;
    let reply = read_runtime_control_line(&mut stream)?;
    if reply == "OK captured" {
        println!("smolworld: captured checkpoint at {}", output.display());
        Ok(())
    } else if let Some(error) = reply.strip_prefix("ERR ") {
        Err(format!("world checkpoint failed: {error}"))
    } else {
        Err("world supervisor returned a malformed checkpoint reply".into())
    }
}

/// Reopen a captured world under the same recorded machine identities. The
/// checkpoint receipt owns RAM/device/disk state; the still-present allocation
/// state owns the static IP/MAC tuple and namespaced SmolVM records. This
/// supervisor recreates only ephemeral listeners and then asks each machine to
/// restore with fresh host vsock/NIC descriptors.
pub(crate) fn restore(config_path: &Path, checkpoint: &Path) -> Result<()> {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    if !checkpoint.is_absolute() {
        return Err("restore --checkpoint must be an absolute directory".into());
    }
    let config = load_config(config_path)?;
    topological_waves(&config)?;
    let paths = world_paths(config_path)?;
    let smolvm = smolvm_program();
    let _world_lock = WorldLock::acquire(&paths)?;
    let material = verify_prepared_world(&config, &paths, &smolvm, false)?;
    let lifecycle = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    if !matches!(
        lifecycle.state,
        LifecycleState::Captured | LifecycleState::Capturing
    ) {
        return Err(format!(
            "world '{}' is not a retained checkpoint (current lifecycle: {})",
            config.name,
            lifecycle.state.as_str()
        ));
    }
    let state = load_allocation_state(&paths.state_file)?
        .ok_or_else(|| "captured world has no allocation state".to_string())?;
    let receipt = load_world_checkpoint_receipt(checkpoint)?;
    verify_world_checkpoint_receipt(&config, &paths, &state, checkpoint, &receipt)?;
    remove_stale_temporary_files(&paths)?;
    remove_runtime_dir(&paths)?;
    mark_starting(&paths)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let (switch_tx, switch_rx) = mpsc::channel();
    let (attached_tx, attached_rx) = mpsc::channel();
    let gateway = Gateway::new(&config, &state);
    let mut port_handles = Vec::new();
    let mut switch_handle = None;
    let retain_checkpoint_sources = true;
    let result = (|| {
        prepare_runtime_dir(&paths)?;
        let control_listener = bind_runtime_control_listener(&paths)?;
        let switch_shutdown = shutdown.clone();
        switch_handle = Some(
            thread::Builder::new()
                .name("smolworld-switch".into())
                .spawn(move || run_switch(switch_rx, gateway, switch_shutdown))
                .map_err(|error| format!("start switch: {error}"))?,
        );
        for name in config.machines.keys() {
            let socket_path = port_socket_path(&paths.runtime_dir, name);
            let listener = UnixListener::bind(&socket_path)
                .map_err(|error| format!("bind {}: {error}", socket_path.display()))?;
            listener
                .set_nonblocking(true)
                .map_err(|error| format!("set {} nonblocking: {error}", socket_path.display()))?;
            port_handles.push(spawn_port_acceptor(
                name.clone(),
                listener,
                switch_tx.clone(),
                attached_tx.clone(),
                shutdown.clone(),
            )?);
        }
        drop(attached_tx);

        let names: Vec<_> = config.machines.keys().cloned().collect();
        parallel_machine_operations(&names, "restore", |name| {
            let assignment = state.assignments.get(name).expect("captured allocation");
            restore_machine(
                &smolvm,
                &assignment.smolvm_name,
                &checkpoint.join("machines").join(name),
            )
        })?;
        wait_for_attachments(&attached_rx, &config)?;
        mark_attached(&paths)?;
        mark_running(&paths)?;
        install_signal_handlers();
        eprintln!("smolworld: restored world is up; press Ctrl-C to stop it");
        while !STOP_REQUESTED.load(Ordering::SeqCst) {
            match control_listener.accept() {
                Ok((mut stream, _)) => {
                    let command = match read_runtime_control_command(&mut stream) {
                        Ok(command) => command,
                        Err(error) => {
                            let _ =
                                write_runtime_control_reply(&mut stream, &format!("ERR {error}\n"));
                            continue;
                        }
                    };
                    match command {
                        RuntimeControlCommand::Ping => {
                            let _ = write_runtime_control_reply(&mut stream, "OK\n");
                        }
                        RuntimeControlCommand::Checkpoint { output } => {
                            match checkpoint_running_world(
                                &config,
                                &state,
                                &paths,
                                &smolvm,
                                &switch_tx,
                                &attached_rx,
                                &output,
                            ) {
                                Ok(()) => {
                                    STOP_REQUESTED.store(true, Ordering::SeqCst);
                                    let _ =
                                        write_runtime_control_reply(&mut stream, "OK captured\n");
                                }
                                Err(error) => {
                                    let _ = write_runtime_control_reply(
                                        &mut stream,
                                        &format!("ERR {error}\n"),
                                    );
                                    if output.exists() {
                                        STOP_REQUESTED.store(true, Ordering::SeqCst);
                                    }
                                }
                            }
                        }
                        RuntimeControlCommand::Lifecycle { action, services } => {
                            if matches!(action, LifecycleCommand::Rm) {
                                let _ = write_runtime_control_reply(
                                    &mut stream,
                                    "ERR rm is unavailable while a restored checkpoint retains source records\n",
                                );
                                continue;
                            }
                            match apply_lifecycle_control(
                                &config,
                                &state,
                                &paths,
                                &smolvm,
                                &material,
                                &attached_rx,
                                action,
                                &services,
                            ) {
                                Ok(()) => {
                                    let _ = write_runtime_control_reply(&mut stream, "OK\n");
                                }
                                Err(error) => {
                                    let _ = write_runtime_control_reply(
                                        &mut stream,
                                        &format!("ERR {error}\n"),
                                    );
                                }
                            }
                        }
                        RuntimeControlCommand::Down => {
                            let _ = write_runtime_control_reply(
                                &mut stream,
                                "ERR down is unavailable while a restored checkpoint retains source records; use release\n",
                            );
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(format!("accept supervisor control: {error}")),
            }
        }
        Ok(())
    })();

    // Restore may fail after one VM has launched. Stop only the recorded world
    // machines and retain their records/disks; the immutable checkpoint remains
    // the recovery source and ordinary `up` cannot delete it accidentally.
    stop_machines(&smolvm, &state);
    shutdown.store(true, Ordering::SeqCst);
    let _ = switch_tx.send(SwitchEvent::Shutdown);
    for handle in port_handles {
        let _ = handle.join();
    }
    if let Some(handle) = switch_handle {
        let _ = handle.join();
    }
    let _ = remove_runtime_dir(&paths);
    if retain_checkpoint_sources {
        let _ = mark_captured(&paths);
    }
    result
}

/// Permanently release one retained state. This is the only durable-world path
/// that deletes source VM records or the checkpoint artifact, and it validates
/// that both still belong to the requested world before touching either.
pub(crate) fn release(config_path: &Path, checkpoint: &Path) -> Result<()> {
    if !checkpoint.is_absolute() {
        return Err("release --checkpoint must be an absolute directory".into());
    }
    let config = load_config(config_path)?;
    let paths = world_paths(config_path)?;
    let _world_lock = WorldLock::acquire(&paths)?;
    let lifecycle = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    if !matches!(
        lifecycle.state,
        LifecycleState::Captured | LifecycleState::Capturing
    ) {
        return Err(format!(
            "world '{}' is not a stopped retained checkpoint (current lifecycle: {})",
            config.name,
            lifecycle.state.as_str()
        ));
    }
    let state = load_allocation_state(&paths.state_file)?
        .ok_or_else(|| "captured world has no allocation state".to_string())?;
    let receipt = load_world_checkpoint_receipt(checkpoint)?;
    verify_world_checkpoint_receipt(&config, &paths, &state, checkpoint, &receipt)?;
    let metadata = fs::symlink_metadata(checkpoint)
        .map_err(|error| format!("inspect checkpoint root {}: {error}", checkpoint.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "checkpoint root is not a real directory: {}",
            checkpoint.display()
        ));
    }
    release_machines(&smolvm_program(), &state)?;
    fs::remove_dir_all(checkpoint).map_err(|error| {
        format!(
            "remove released checkpoint {}: {error}",
            checkpoint.display()
        )
    })?;
    mark_absent(&paths)?;
    println!("smolworld: released checkpoint {}", checkpoint.display());
    Ok(())
}

/// Control messages accepted only by the process that owns the switch and its
/// recorded world lock. Keep this deliberately small and typed so an external
/// world adapter can invoke it without reconstructing lifecycle state.
enum RuntimeControlCommand {
    Ping,
    Checkpoint {
        output: PathBuf,
    },
    Lifecycle {
        action: LifecycleCommand,
        services: Vec<String>,
    },
    Down,
}

fn runtime_control_socket_path(paths: &WorldPaths) -> PathBuf {
    paths.runtime_dir.join("control.sock")
}

fn bind_runtime_control_listener(paths: &WorldPaths) -> Result<UnixListener> {
    let path = runtime_control_socket_path(paths);
    let listener = UnixListener::bind(&path)
        .map_err(|error| format!("bind supervisor control {}: {error}", path.display()))?;
    listener.set_nonblocking(true).map_err(|error| {
        format!(
            "set supervisor control {} nonblocking: {error}",
            path.display()
        )
    })?;
    Ok(listener)
}

fn read_runtime_control_command(stream: &mut UnixStream) -> Result<RuntimeControlCommand> {
    if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
        // macOS rejects this option for an in-process UnixStream pair even
        // though the supervisor's listener accepts it. Keep parser tests and
        // local control callers on the same framing path.
        if error.kind() != std::io::ErrorKind::InvalidInput {
            return Err(format!("set supervisor request timeout: {error}"));
        }
    }
    let line = read_runtime_control_line(stream)?;
    if line == "ping" {
        return Ok(RuntimeControlCommand::Ping);
    }
    if line == "down" {
        return Ok(RuntimeControlCommand::Down);
    }
    let (verb, argument) = line
        .split_once('\t')
        .ok_or_else(|| "supervisor request is malformed".to_string())?;
    if verb == "checkpoint" {
        if argument.is_empty()
            || argument.contains(['\t', '\r', '\n'])
            || !Path::new(argument).is_absolute()
        {
            return Err("supervisor request is malformed".into());
        }
        return Ok(RuntimeControlCommand::Checkpoint {
            output: PathBuf::from(argument),
        });
    }
    let action = match verb {
        "start" => LifecycleCommand::Start,
        "stop" => LifecycleCommand::Stop,
        "restart" => LifecycleCommand::Restart,
        "rm" => LifecycleCommand::Rm,
        _ => return Err("supervisor request is malformed".into()),
    };
    let services = if argument == "*" {
        Vec::new()
    } else {
        let values: Vec<_> = argument.split(',').map(str::to_owned).collect();
        if values.is_empty()
            || values.iter().any(|value| {
                value.is_empty()
                    || value.contains(['\t', '\r', '\n', ',', '/'])
                    || !value.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
        {
            return Err("supervisor request is malformed".into());
        }
        values
    };
    Ok(RuntimeControlCommand::Lifecycle { action, services })
}

fn read_runtime_control_line(stream: &mut UnixStream) -> Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while bytes.len() < 4096 {
        let read = stream
            .read(&mut byte)
            .map_err(|error| format!("read supervisor control: {error}"))?;
        if read == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
    }
    if bytes.len() == 4096 {
        return Err("supervisor control message is too long".into());
    }
    String::from_utf8(bytes).map_err(|_| "supervisor control message is not UTF-8".into())
}

fn write_runtime_control_reply(stream: &mut UnixStream, reply: &str) -> Result<()> {
    if !reply.ends_with('\n') || reply.contains('\r') || reply.len() > 4096 {
        return Err("internal supervisor control reply is invalid".into());
    }
    stream
        .write_all(reply.as_bytes())
        .map_err(|error| format!("write supervisor control reply: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("flush supervisor control reply: {error}"))
}

pub(crate) fn up(config_path: &Path, requested_services: &[String], detach: bool) -> Result<()> {
    if detach {
        return spawn_detached_up(config_path, requested_services);
    }
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    let config = load_config(config_path)?;
    let waves = topological_waves(&config)?;
    let selected = selected_services_with_dependencies(&config, requested_services)?;
    let paths = world_paths(config_path)?;
    let smolvm = smolvm_program();
    let _world_lock = WorldLock::acquire(&paths)?;
    let material = verify_prepared_world(&config, &paths, &smolvm, false)?;

    let recovery = inspect_recovery(&paths)?;
    if recovery.lifecycle.state.retains_checkpoint_sources() {
        return Err(format!(
            "world '{}' has a retained or in-progress durable capture; run `smolworld restore --checkpoint DIR` or explicitly release that checkpoint before a fresh up",
            config.name
        ));
    }
    let reuse_created = recovery.lifecycle.state == LifecycleState::Created
        && recovery.runtime_dir == crate::model::ArtifactState::Missing;
    if recovery.is_recorded_but_absent() {
        eprintln!(
            "smolworld: found recorded allocations for {} but no running machines",
            config.name
        );
    } else if recovery.needs_recovery() && !reuse_created {
        eprintln!(
            "smolworld: recovering stale {} state for {}",
            recovery.lifecycle.state.as_str(),
            config.name
        );
    }
    let previous = load_allocation_state(&paths.state_file)?;
    if reuse_created && previous.is_none() {
        return Err("created world has no recorded allocation state".into());
    }
    if !reuse_created {
        cleanup_machines(&smolvm, previous.as_ref());
    }
    remove_stale_temporary_files(&paths)?;
    remove_runtime_dir(&paths)?;

    let state = if reuse_created {
        previous.expect("created state checked above")
    } else {
        allocate_allocation_state(previous, &config, &paths)?
    };
    write_allocation_state(&paths, &state)?;
    mark_starting(&paths)?;
    let existing_created: HashSet<String> = if reuse_created {
        selected
            .iter()
            .filter_map(|name| {
                let assignment = state.assignments.get(name)?;
                machine_status(&smolvm, &assignment.smolvm_name)
                    .ok()
                    .flatten()
                    .map(|_| name.clone())
            })
            .collect()
    } else {
        HashSet::new()
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let (switch_tx, switch_rx) = mpsc::channel();
    let (attached_tx, attached_rx) = mpsc::channel();
    let gateway = Gateway::new(&config, &state);
    let mut port_handles = Vec::new();
    let mut socket_paths = BTreeMap::new();
    let mut switch_handle = None;
    let mut retain_checkpoint_sources = false;
    let mut deleted_by_explicit_down = false;
    let result = (|| {
        prepare_runtime_dir(&paths)?;
        let control_listener = bind_runtime_control_listener(&paths)?;
        let switch_shutdown = shutdown.clone();
        switch_handle = Some(
            thread::Builder::new()
                .name("smolworld-switch".into())
                .spawn(move || run_switch(switch_rx, gateway, switch_shutdown))
                .map_err(|error| format!("start switch: {error}"))?,
        );

        for name in config.machines.keys() {
            let socket_path = port_socket_path(&paths.runtime_dir, name);
            let listener = UnixListener::bind(&socket_path)
                .map_err(|error| format!("bind {}: {error}", socket_path.display()))?;
            listener
                .set_nonblocking(true)
                .map_err(|error| format!("set {} nonblocking: {error}", socket_path.display()))?;
            socket_paths.insert(name.clone(), socket_path);
            port_handles.push(spawn_port_acceptor(
                name.clone(),
                listener,
                switch_tx.clone(),
                attached_tx.clone(),
                shutdown.clone(),
            )?);
        }
        drop(attached_tx);

        for wave in &waves {
            let selected_wave: Vec<_> = wave
                .iter()
                .filter(|name| {
                    selected.contains(name.as_str()) && !existing_created.contains(name.as_str())
                })
                .cloned()
                .collect();
            parallel_machine_operations(&selected_wave, "create", |name| {
                let assignment = state.assignments.get(name).expect("allocated machine");
                let smolfile = material
                    .smolfiles
                    .get(name)
                    .expect("prepared material has every configured machine");
                create_machine(
                    &smolvm,
                    MachineLaunch {
                        assignment,
                        socket: socket_paths.get(name).expect("socket allocated"),
                        smolfile: &smolfile.prepared_path,
                    },
                    &config.network,
                )
            })?;
        }
        mark_created(&paths)?;
        for wave in &waves {
            let selected_wave: Vec<_> = wave
                .iter()
                .filter(|name| selected.contains(name.as_str()))
                .cloned()
                .collect();
            parallel_machine_operations(&selected_wave, "start", |name| {
                start_machine(
                    &smolvm,
                    &state
                        .assignments
                        .get(name)
                        .expect("allocated machine")
                        .smolvm_name,
                )
            })?;
            parallel_machine_operations(&selected_wave, "install sealed seed files", |name| {
                let assignment = state.assignments.get(name).expect("allocated machine");
                let seed_files = prepared_seed_files(&paths.config_dir, &material, name)?;
                install_machine_seed_files(&smolvm, &assignment.smolvm_name, &seed_files)
            })?;
        }

        wait_for_expected_attachments(&attached_rx, selected.clone())?;
        mark_attached(&paths)?;
        print_allocations(&config, &state);
        mark_running(&paths)?;
        install_signal_handlers();
        eprintln!("smolworld: world is up; press Ctrl-C to stop it");
        while !STOP_REQUESTED.load(Ordering::SeqCst) {
            match control_listener.accept() {
                Ok((mut stream, _)) => {
                    let command = match read_runtime_control_command(&mut stream) {
                        Ok(command) => command,
                        Err(error) => {
                            let _ =
                                write_runtime_control_reply(&mut stream, &format!("ERR {error}\n"));
                            continue;
                        }
                    };
                    match command {
                        RuntimeControlCommand::Ping => {
                            let _ = write_runtime_control_reply(&mut stream, "OK\n");
                        }
                        RuntimeControlCommand::Checkpoint { output } => {
                            match checkpoint_running_world(
                                &config,
                                &state,
                                &paths,
                                &smolvm,
                                &switch_tx,
                                &attached_rx,
                                &output,
                            ) {
                                Ok(()) => {
                                    retain_checkpoint_sources = true;
                                    STOP_REQUESTED.store(true, Ordering::SeqCst);
                                    let _ =
                                        write_runtime_control_reply(&mut stream, "OK captured\n");
                                }
                                Err(error) => {
                                    // A receipt may already have been published when the
                                    // final lifecycle write reports a host I/O error. Its
                                    // stopped machine sources must be retained for explicit
                                    // recovery; deleting them here would turn a surfaced
                                    // commit failure into silent data loss.
                                    let retained_lifecycle =
                                        match load_lifecycle(&paths.lifecycle_path()) {
                                            Ok(Some(lifecycle)) => {
                                                lifecycle.state.retains_checkpoint_sources()
                                            }
                                            Ok(None) => false,
                                            // A corrupt or unreadable lifecycle after a
                                            // checkpoint error is not evidence that the
                                            // stopped source records are disposable.
                                            Err(_) => true,
                                        };
                                    if output.exists() || retained_lifecycle {
                                        retain_checkpoint_sources = true;
                                        STOP_REQUESTED.store(true, Ordering::SeqCst);
                                    }
                                    let _ = write_runtime_control_reply(
                                        &mut stream,
                                        &format!("ERR {error}\n"),
                                    );
                                }
                            }
                        }
                        RuntimeControlCommand::Lifecycle { action, services } => {
                            match apply_lifecycle_control(
                                &config,
                                &state,
                                &paths,
                                &smolvm,
                                &material,
                                &attached_rx,
                                action,
                                &services,
                            ) {
                                Ok(()) => {
                                    let _ = write_runtime_control_reply(&mut stream, "OK\n");
                                }
                                Err(error) => {
                                    let _ = write_runtime_control_reply(
                                        &mut stream,
                                        &format!("ERR {error}\n"),
                                    );
                                }
                            }
                        }
                        RuntimeControlCommand::Down => {
                            match delete_recorded_machines(&smolvm, &state) {
                                Ok(()) => {
                                    deleted_by_explicit_down = true;
                                    STOP_REQUESTED.store(true, Ordering::SeqCst);
                                    let _ = write_runtime_control_reply(&mut stream, "OK\n");
                                }
                                Err(error) => {
                                    let _ = write_runtime_control_reply(
                                        &mut stream,
                                        &format!("ERR {error}\n"),
                                    );
                                }
                            }
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(format!("accept supervisor control: {error}")),
            }
        }
        Ok(())
    })();

    let preserve_created_after_failed_start = reuse_created && result.is_err();
    if !retain_checkpoint_sources {
        if preserve_created_after_failed_start {
            // A failed `start` must not turn a created world's retained
            // identity into deletion. Stop any partial process, remove only
            // ephemeral sockets below, and restore the no-owner Created
            // transition so a corrected later start uses the same record.
            stop_machines(&smolvm, &state);
        } else if !deleted_by_explicit_down {
            cleanup_machines(&smolvm, Some(&state));
        }
    }
    shutdown.store(true, Ordering::SeqCst);
    let _ = switch_tx.send(SwitchEvent::Shutdown);
    for handle in port_handles {
        let _ = handle.join();
    }
    if let Some(handle) = switch_handle {
        let _ = handle.join();
    }
    let _ = remove_runtime_dir(&paths);
    if !retain_checkpoint_sources {
        if preserve_created_after_failed_start {
            let _ = mark_created_detached(&paths);
        } else {
            let _ = mark_absent(&paths);
        }
    }
    result
}

/// Run one lifecycle operation concurrently for a dependency wave. Results
/// are joined in declaration order so failures remain deterministic even when
/// subprocesses finish in a different order. All workers are joined before an
/// error is returned, allowing the outer cleanup path to see every partial
/// machine the wave may have created.
fn parallel_machine_operations<F>(names: &[String], operation: &str, task: F) -> Result<()>
where
    F: Fn(&str) -> Result<()> + Sync,
{
    parallel_machine_map(names, operation, task).map(|_| ())
}

/// Map one host-side operation over a deterministic machine list concurrently.
/// The scoped workers are joined in input order, so a failure remains stable
/// for callers while every worker has finished before cleanup begins.
fn parallel_machine_map<T, F>(names: &[String], operation: &str, task: F) -> Result<Vec<T>>
where
    T: Send,
    F: Fn(&str) -> Result<T> + Sync,
{
    thread::scope(|scope| {
        let handles: Vec<_> = names
            .iter()
            .map(|name| {
                let task = &task;
                scope.spawn(move || task(name))
            })
            .collect();
        handles
            .into_iter()
            .zip(names)
            .map(|(handle, name)| {
                let result = handle
                    .join()
                    .map_err(|_| format!("{operation} machine '{name}' worker panicked"))?;
                result.map_err(|error| format!("{operation} machine '{name}': {error}"))
            })
            .collect()
    })
}

pub(crate) fn down(config_path: &Path) -> Result<()> {
    let paths = world_paths(config_path)?;
    if let Some(reply) = try_send_runtime_control(&paths, "down\n")? {
        if reply == "OK" {
            println!("smolworld: down");
            return Ok(());
        }
        if let Some(error) = reply.strip_prefix("ERR ") {
            return Err(format!("world down failed: {error}"));
        }
        return Err("world supervisor returned a malformed down reply".into());
    }
    let _world_lock = WorldLock::acquire(&paths)?;
    let state = load_allocation_state(&paths.state_file)?;
    let lifecycle = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    if lifecycle.state.retains_checkpoint_sources() {
        return Err(
            "world has a retained durable checkpoint; use `smolworld release --checkpoint DIR` to delete its exact source machines and artifact"
                .into(),
        );
    }
    if let Some(state) = &state {
        delete_recorded_machines(&smolvm_program(), state)?;
    }
    remove_stale_temporary_files(&paths)?;
    remove_runtime_dir(&paths)?;
    if state.is_some() {
        mark_absent(&paths)?;
    }
    println!("smolworld: down");
    Ok(())
}

pub(crate) fn ps(
    config_path: &Path,
    requested_services: &[String],
    all: bool,
    status_filter: Option<DisplayLifecycleState>,
    names_only: bool,
    format: &PsFormat,
) -> Result<()> {
    let config = load_config(config_path)?;
    let requested = selected_services(&config, requested_services)?;
    let paths = world_paths(config_path)?;
    let state = load_allocation_state(&paths.state_file)?;
    let lifecycle = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    let smolvm = smolvm_program();
    let mut machines = Vec::new();
    for name in requested {
        let assignment = state
            .as_ref()
            .and_then(|state| state.assignments.get(&name));
        let smolvm_state = match assignment {
            Some(assignment) => {
                require_machine_identity(&name, &assignment.smolvm_name)?;
                machine_status(&smolvm, &assignment.smolvm_name)?
            }
            None => None,
        };
        let status = display_lifecycle_state(lifecycle.state, smolvm_state);
        let row = MachineStatus::new(
            name,
            assignment
                .map(|assignment| assignment.ip.to_string())
                .unwrap_or_else(|| "-".into()),
            assignment
                .map(|assignment| format_mac(assignment.mac))
                .unwrap_or_else(|| "-".into()),
            status,
        );
        if all
            || !matches!(
                row.state,
                DisplayLifecycleState::Absent | DisplayLifecycleState::Stopped
            )
            || !requested_services.is_empty()
        {
            machines.push(row);
        }
    }
    if let Some(status_filter) = status_filter {
        machines.retain(|machine| machine.state == status_filter);
    }
    if names_only {
        println!(
            "{}",
            machines
                .iter()
                .map(|machine| machine.machine.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
    } else {
        println!("{}", format_ps(format, &machines));
    }
    Ok(())
}

/// Collect one exact-identity resource snapshot. The state file is the
/// identity boundary: this command never lists or discovers unrelated smolvm
/// records.
fn collect_service_stats(
    config_path: &Path,
    requested_services: &[String],
    include_absent: bool,
) -> Result<(String, Vec<ServiceStats>)> {
    let config = load_config(config_path)?;
    let requested = selected_services(&config, requested_services)?;
    let paths = world_paths(config_path)?;
    let state = load_allocation_state(&paths.state_file)?;
    let smolvm = smolvm_program();
    let mut machines = Vec::new();

    for machine in requested {
        let Some(assignment) = state
            .as_ref()
            .and_then(|state| state.assignments.get(&machine))
        else {
            if include_absent || !requested_services.is_empty() {
                machines.push(absent_service_stats(machine));
            }
            continue;
        };

        require_machine_identity(&machine, &assignment.smolvm_name)?;
        let companion_state = machine_status(&smolvm, &assignment.smolvm_name)?;
        let Some(companion_state) = companion_state else {
            if include_absent || !requested_services.is_empty() {
                machines.push(absent_service_stats(machine));
            }
            continue;
        };
        if !include_absent
            && requested_services.is_empty()
            && companion_state != CompanionMachineState::Running
        {
            continue;
        }
        let stats = machine_stats(&smolvm, &assignment.smolvm_name)?;
        machines.push(service_stats_from_machine_stats(&machine, &stats));
    }

    Ok((config.name, machines))
}

fn absent_service_stats(machine: String) -> ServiceStats {
    ServiceStats {
        machine,
        smolvm_name: None,
        state: "absent".into(),
        pid: None,
        cpus: None,
        memory_mb: None,
        storage_gb: None,
        overlay_gb: None,
        cpu_seconds: None,
        cpu_millis: None,
        rss_mb: None,
        disk_used_mb: None,
    }
}

/// Stream Compose-shaped world resource observations. JSON deliberately
/// preserves the contract's closed `schemaVersion: 1` envelope on each update.
pub(crate) fn stats(
    config_path: &Path,
    requested_services: &[String],
    all: bool,
    no_stream: bool,
    format: &StatsFormat,
) -> Result<()> {
    loop {
        let (world, machines) = collect_service_stats(config_path, requested_services, all)?;
        let output = match format {
            StatsFormat::Table => format_stats_table(&machines),
            StatsFormat::Json => format_stats_json(&world, &machines),
            StatsFormat::Template(template) => format_stats_template(template, &machines),
        };
        println!("{output}");
        if no_stream {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Show already sealed material only. This intentionally does not call the
/// upstream `machine images` command because that command may boot a stopped
/// VM; image inspection must remain read-only at the world boundary.
pub(crate) fn images(
    config_path: &Path,
    requested_services: &[String],
    format: ImagesFormat,
) -> Result<()> {
    let config = load_config(config_path)?;
    let services = selected_services(&config, requested_services)?;
    let paths = world_paths(config_path)?;
    let material = load_material_lock(&paths.material_lock_path())?
        .ok_or_else(|| "world has no sealed material; run `smolworld prepare` first".to_string())?;
    let rows: Vec<_> = services
        .iter()
        .map(|service| {
            material
                .images
                .get(service)
                .ok_or_else(|| format!("sealed material has no image for service '{service}'"))
        })
        .collect::<Result<_>>()?;
    match format {
        ImagesFormat::Table => {
            println!("SERVICE\tSOURCE\tKIND\tDIGEST");
            for image in rows {
                println!(
                    "{}\t{}\t{}\t{}",
                    image.machine,
                    image.source_reference,
                    image.source_kind.as_str(),
                    image.image_digest
                );
            }
        }
        ImagesFormat::Json => {
            for image in rows {
                let mut output = String::from("{\"service\":");
                crate::cli::push_json_string(&mut output, &image.machine);
                output.push_str(",\"source\":");
                crate::cli::push_json_string(&mut output, &image.source_reference);
                output.push_str(",\"sourceKind\":");
                crate::cli::push_json_string(&mut output, image.source_kind.as_str());
                output.push_str(",\"sourceDigest\":");
                crate::cli::push_json_string(&mut output, &image.source_digest);
                output.push_str(",\"imageDigest\":");
                crate::cli::push_json_string(&mut output, &image.image_digest);
                output.push('}');
                println!("{output}");
            }
        }
    }
    Ok(())
}

fn require_machine_identity(machine: &str, smolvm_name: &str) -> Result<()> {
    validate_recorded_smolvm_name(smolvm_name).map_err(|reason| {
        format!(
            "world machine '{machine}' has an unrecognized smolvm identity '{smolvm_name}': {reason}"
        )
    })
}

fn service_stats_from_machine_stats(machine: &str, stats: &MachineStats) -> ServiceStats {
    ServiceStats {
        machine: machine.to_string(),
        smolvm_name: Some(stats.name.clone()),
        state: stats.state.as_str().to_string(),
        pid: stats.pid,
        cpus: Some(stats.cpus),
        memory_mb: Some(stats.memory_mb),
        storage_gb: Some(stats.storage_gb),
        overlay_gb: Some(stats.overlay_gb),
        cpu_seconds: stats.cpu_seconds,
        cpu_millis: stats.cpu_millis,
        rss_mb: stats.rss_mb,
        disk_used_mb: stats.disk_used_mb,
    }
}

fn machine_status(smolvm: &Path, name: &str) -> Result<Option<CompanionMachineState>> {
    upstream_machine_status(smolvm, name)
}

fn display_lifecycle_state(
    lifecycle: LifecycleState,
    smolvm_state: Option<CompanionMachineState>,
) -> DisplayLifecycleState {
    let Some(smolvm_state) = smolvm_state else {
        return DisplayLifecycleState::Absent;
    };
    match lifecycle {
        LifecycleState::Capturing => return DisplayLifecycleState::Capturing,
        LifecycleState::Captured => return DisplayLifecycleState::Captured,
        _ => {}
    }
    if smolvm_state == CompanionMachineState::Stopped {
        return DisplayLifecycleState::Stopped;
    }
    if smolvm_state != CompanionMachineState::Running {
        return DisplayLifecycleState::Created;
    }
    match lifecycle {
        LifecycleState::Attached => DisplayLifecycleState::Attached,
        LifecycleState::Running => DisplayLifecycleState::Running,
        _ => DisplayLifecycleState::Created,
    }
}

pub(crate) fn exec(
    config_path: &Path,
    service: &str,
    options: &ExecOptions,
    command: &[std::ffi::OsString],
) -> Result<()> {
    let config = load_config(config_path)?;
    if !config.machines.contains_key(service) {
        return Err(format!("unknown world service '{service}'"));
    }
    let paths = world_paths(config_path)?;
    require_live_supervisor(&paths, "exec")?;
    let state = load_allocation_state(&paths.state_file)?
        .ok_or_else(|| "world has no state; run `smolworld up` first".to_string())?;
    let assignment = state
        .assignments
        .get(service)
        .ok_or_else(|| format!("service '{service}' has no allocation"))?;
    require_machine_identity(service, &assignment.smolvm_name)?;
    if machine_status(&smolvm_program(), &assignment.smolvm_name)?
        != Some(CompanionMachineState::Running)
    {
        return Err(format!(
            "service '{service}' is not running; use `smolworld start {service}` through its world supervisor"
        ));
    }
    exec_machine(&smolvm_program(), &assignment.smolvm_name, options, command)
}

pub(crate) fn shell(config_path: &Path, service: &str) -> Result<()> {
    let options = ExecOptions {
        interactive: true,
        tty: true,
        ..ExecOptions::default()
    };
    exec(
        config_path,
        service,
        &options,
        &[std::ffi::OsString::from("/bin/sh")],
    )
}

/// Copy one regular host file to or from exactly one recorded world machine.
/// This is deliberately a namespaced command delegation, not a filesystem
/// sharing mechanism: the smolvm name is resolved only from this world's
/// durable allocation state and is never exposed to callers.
pub(crate) fn copy(config_path: &Path, source: &str, destination: &str) -> Result<()> {
    let config = load_config(config_path)?;
    let paths = world_paths(config_path)?;
    let source_remote = parse_copy_remote_endpoint(source)?;
    let destination_remote = parse_copy_remote_endpoint(destination)?;
    let (machine, guest_path, local_path, upload) = match (source_remote, destination_remote) {
        (Some((machine, guest_path)), None) => (machine, guest_path, destination, false),
        (None, Some((machine, guest_path))) => (machine, guest_path, source, true),
        (Some(_), Some(_)) => {
            return Err("smolworld cp accepts exactly one SERVICE:/absolute/path endpoint".into());
        }
        (None, None) => {
            return Err("smolworld cp requires one SERVICE:/absolute/path endpoint".into());
        }
    };
    if !config.machines.contains_key(machine) {
        return Err(format!("unknown world service '{machine}'"));
    }
    require_live_supervisor(&paths, "cp")?;
    let state = load_allocation_state(&paths.state_file)?
        .ok_or_else(|| "world has no state; run `smolworld up` first".to_string())?;
    let assignment = state
        .assignments
        .get(machine)
        .ok_or_else(|| format!("service '{machine}' has no allocation"))?;
    require_machine_identity(machine, &assignment.smolvm_name)?;
    if machine_status(&smolvm_program(), &assignment.smolvm_name)?
        != Some(CompanionMachineState::Running)
    {
        return Err(format!(
            "service '{machine}' is not running; use `smolworld start {machine}` through its world supervisor"
        ));
    }
    copy_machine(
        &smolvm_program(),
        &assignment.smolvm_name,
        guest_path,
        local_path,
        upload,
    )
}

fn parse_copy_remote_endpoint(value: &str) -> Result<Option<(&str, &str)>> {
    let Some((machine, guest_path)) = value.split_once(':') else {
        return Ok(None);
    };
    if machine.is_empty() || !safe_copy_guest_path(guest_path) {
        return Err(
            "service copy endpoint must be SERVICE:/absolute/path without traversal".into(),
        );
    }
    Ok(Some((machine, guest_path)))
}

fn safe_copy_guest_path(path: &str) -> bool {
    path.strip_prefix('/').is_some_and(|relative| {
        !relative.is_empty()
            && !path.contains('\0')
            && !path.contains('\\')
            && relative
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != "..")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_destinations_match_the_world_seed_contract() {
        assert!(validate_seed_destination(Path::new("/etc/app/config.toml")).is_ok());
        assert!(validate_seed_destination(Path::new("/etc/app/config:local=1.toml")).is_ok());
        for invalid in [
            "relative/config.toml",
            "/",
            "/etc/app/",
            "/etc//app/config.toml",
            "/etc/app/../config.toml",
        ] {
            assert!(
                validate_seed_destination(Path::new(invalid)).is_err(),
                "expected invalid seed destination {invalid}"
            );
        }
        assert!(validate_seed_source_for_copy(Path::new("/tmp/sealed-input")).is_ok());
        assert!(validate_seed_source_for_copy(Path::new("/tmp/sealed:input")).is_err());
    }

    #[test]
    fn copy_endpoints_are_a_single_safe_guest_path() {
        assert_eq!(
            parse_copy_remote_endpoint("runner:/workspace/input.tar").unwrap(),
            Some(("runner", "/workspace/input.tar"))
        );
        assert_eq!(parse_copy_remote_endpoint("host-input.tar").unwrap(), None);
        for endpoint in [
            ":/workspace/input.tar",
            "runner:workspace/input.tar",
            "runner:/workspace/../input.tar",
            "runner:/workspace//input.tar",
            "runner:/",
        ] {
            assert!(
                parse_copy_remote_endpoint(endpoint).is_err(),
                "expected invalid copy endpoint {endpoint}"
            );
        }
    }

    #[test]
    fn exec_and_copy_require_a_live_supervisor_before_companion_delegation() {
        let root = temporary_runtime_test_directory();
        let config_path = root.join("world.smolworld");
        std::fs::write(
            &config_path,
            "format: 2\nworld:\n  name: boundary-test\nnetwork:\n  subnet: 10.89.0.0/24\nmachines:\n  runner:\n    smolfile: ./runner.Smolfile\n",
        )
        .unwrap();

        let exec_error = exec(
            &config_path,
            "runner",
            &ExecOptions::default(),
            &[std::ffi::OsString::from("/bin/true")],
        )
        .unwrap_err();
        assert!(exec_error.contains("before exec"));

        let copy_error = copy(
            &config_path,
            "host-input",
            "runner:/workspace/input",
        )
        .unwrap_err();
        assert!(copy_error.contains("before cp"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stats_accepts_only_recorded_machine_identities() {
        assert!(require_machine_identity("runner", "smw-abcdef-0123").is_ok());
        for invalid in [
            "runner",
            "smolvm-runner",
            "smw",
            "smw-/runner",
            "smw-runner\tother",
        ] {
            assert!(
                require_machine_identity("runner", invalid).is_err(),
                "expected unrecognized identity to be rejected: {invalid:?}"
            );
        }
    }

    #[test]
    fn stats_maps_the_companion_record_without_reinterpreting_it() {
        let stats = MachineStats {
            name: "smw-demo-runner".into(),
            state: CompanionMachineState::Running,
            pid: Some(42),
            cpus: 4,
            memory_mb: 4096,
            storage_gb: 20,
            overlay_gb: 4,
            cpu_seconds: Some(2),
            cpu_millis: Some(2345),
            rss_mb: Some(128),
            disk_used_mb: Some(64),
        };
        let observation = service_stats_from_machine_stats("runner", &stats);
        assert_eq!(observation.machine, "runner");
        assert_eq!(observation.smolvm_name.as_deref(), Some("smw-demo-runner"));
        assert_eq!(observation.cpu_millis, Some(2345));
        assert_eq!(observation.rss_mb, Some(128));
        assert_eq!(observation.disk_used_mb, Some(64));
    }

    #[test]
    fn supervisor_control_accepts_only_one_absolute_checkpoint_path() {
        let parse = |message: &str| {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(message.as_bytes()).unwrap();
            drop(writer);
            read_runtime_control_command(&mut reader)
        };
        match parse("checkpoint\t/private/tmp/world\n") {
            Ok(RuntimeControlCommand::Checkpoint { output }) => {
                assert_eq!(output, PathBuf::from("/private/tmp/world"));
            }
            Err(error) => panic!("valid supervisor request failed: {error}"),
            Ok(
                RuntimeControlCommand::Ping
                | RuntimeControlCommand::Lifecycle { .. }
                | RuntimeControlCommand::Down
            ) => {
                panic!("checkpoint request parsed as a different control command")
            }
        }
        for invalid in [
            "checkpoint\trelative\n",
            "checkpoint\t/private/tmp/world\textra\n",
            "restore\t/private/tmp/world\n",
            "checkpoint\t/private/tmp/world\r\n",
        ] {
            assert!(
                parse(invalid).is_err(),
                "expected invalid control request {invalid:?}"
            );
        }
    }

    #[test]
    fn supervisor_control_accepts_only_closed_service_transitions() {
        let parse = |message: &str| {
            let (mut reader, mut writer) = UnixStream::pair().unwrap();
            writer.write_all(message.as_bytes()).unwrap();
            drop(writer);
            read_runtime_control_command(&mut reader)
        };
        match parse("restart\tredis,runner\n") {
            Ok(RuntimeControlCommand::Lifecycle { action, services }) => {
                assert_eq!(action.name(), "restart");
                assert_eq!(services, ["redis", "runner"]);
            }
            Ok(_) => panic!("restart request parsed as a different control command"),
            Err(error) => panic!("valid restart request failed: {error}"),
        }
        assert!(matches!(parse("down\n"), Ok(RuntimeControlCommand::Down)));
        for invalid in [
            "create\tredis\n",
            "start\tredis,../other\n",
            "start\tredis,,runner\n",
            "stop\t\n",
            "rm\tredis\tother\n",
        ] {
            assert!(
                parse(invalid).is_err(),
                "expected invalid lifecycle control {invalid:?}"
            );
        }
    }

    #[test]
    fn supervisor_control_treats_missing_and_stale_sockets_as_unavailable() {
        let root = temporary_runtime_test_directory();
        let paths = runtime_test_paths(&root);
        assert_eq!(try_send_runtime_control(&paths, "ping\n").unwrap(), None);

        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let listener = UnixListener::bind(runtime_control_socket_path(&paths)).unwrap();
        drop(listener);
        assert_eq!(try_send_runtime_control(&paths, "ping\n").unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn supervisor_control_ping_requires_an_owner_acknowledgement() {
        let root = temporary_runtime_test_directory();
        let paths = runtime_test_paths(&root);
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let listener = UnixListener::bind(runtime_control_socket_path(&paths)).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(matches!(
                read_runtime_control_command(&mut stream),
                Ok(RuntimeControlCommand::Ping)
            ));
            write_runtime_control_reply(&mut stream, "OK\n").unwrap();
        });
        require_live_supervisor(&paths, "exec").unwrap();
        server.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    fn runtime_test_paths(root: &Path) -> WorldPaths {
        WorldPaths {
            canonical_config: root.join("world.smolworld"),
            config_dir: root.to_path_buf(),
            hash: 0,
            state_dir: root.join("state"),
            state_file: root.join("state/state"),
            runtime_dir: root.join("runtime"),
        }
    }

    fn temporary_runtime_test_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        for sequence in 0..16 {
            let root = PathBuf::from("/tmp").join(format!(
                "smolworld-runtime-test-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            match std::fs::create_dir(&root) {
                Ok(()) => return root,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test directory {}: {error}", root.display()),
            }
        }
        panic!("allocate a unique runtime test directory")
    }

    #[test]
    fn selected_services_include_only_declared_dependencies() {
        let config = WorldConfig {
            name: "demo".into(),
            network: crate::model::NetworkConfig {
                subnet: [10, 89, 0, 0],
                gateway: "10.89.0.1".parse().unwrap(),
                dns: "10.89.0.1".parse().unwrap(),
                domain: "demo.test".into(),
                egress: false,
            },
            machines: BTreeMap::from([
                (
                    "redis".into(),
                    crate::model::MachineConfig {
                        smolfile: PathBuf::from("redis.Smolfile"),
                        depends_on: vec![],
                        seed_files: vec![],
                    },
                ),
                (
                    "runner".into(),
                    crate::model::MachineConfig {
                        smolfile: PathBuf::from("runner.Smolfile"),
                        depends_on: vec!["redis".into()],
                        seed_files: vec![],
                    },
                ),
            ]),
        };
        assert_eq!(
            selected_services_with_dependencies(&config, &["runner".into()]).unwrap(),
            HashSet::from(["redis".into(), "runner".into()])
        );
        assert!(selected_services(&config, &["other".into()]).is_err());
    }

    #[test]
    fn checkpoint_staging_never_overwrites_a_visible_artifact() {
        let root = std::env::temp_dir().join(format!(
            "smolworld-checkpoint-staging-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&root).unwrap();
        let output = root.join("world");
        fs::create_dir(&output).unwrap();
        assert!(create_world_checkpoint_staging(&output)
            .unwrap_err()
            .contains("refusing to overwrite"));
        fs::remove_dir(&output).unwrap();
        let (parent, staging) = create_world_checkpoint_staging(&output).unwrap();
        assert_eq!(parent, root);
        assert!(staging.is_dir());
        assert!(!output.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parallel_operations_wait_for_every_machine_before_returning_the_first_error() {
        let names = vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ];
        let completed = std::sync::atomic::AtomicUsize::new(0);
        let error = parallel_machine_operations(&names, "test", |name| {
            completed.fetch_add(1, Ordering::SeqCst);
            if name == "second" {
                Err("injected failure".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(error.contains("second"));
        assert_eq!(completed.load(Ordering::SeqCst), names.len());
    }
}
