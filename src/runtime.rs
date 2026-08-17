use crate::cli::{
    format_metrics_json, format_ps, Cli, LifecycleState as DisplayLifecycleState, MachineMetrics,
    MachineStatus, PsFormat,
};
use crate::config::{load_config, topological_order, topological_waves};
use crate::gateway::Gateway;
use crate::model::{
    format_mac, Assignment, LifecycleState, MachineCheckpointReceipt, MachineLaunch, SeedFile,
    WorldCheckpointReceipt, WorldConfig, WORLD_CHECKPOINT_RECEIPT_VERSION,
};
use crate::smolvm::{
    checkpoint_machine, cleanup_machines, copy_machine, create_machine, exec_machine,
    install_seed_files as install_machine_seed_files, machine_stats,
    machine_status as upstream_machine_status,
    materialize_external_world, preflight, release_machines, restore_machine, smolvm_program,
    start_machine, stop_machines, validate_external_world, CompanionMachineState, MachineStats,
};
use crate::state::{
    allocate_allocation_state, digest_file, digest_machine_checkpoint_receipt,
    inspect_recovery, load_allocation_state, load_lifecycle, load_material_lock,
    load_world_checkpoint_receipt, mark_absent, mark_attached, mark_capture_rolled_back,
    mark_captured, mark_capturing, mark_created, mark_running, mark_starting,
    material_lock_resolver_abi, normalize_relative_path, prepare_runtime_dir,
    remove_runtime_dir, remove_stale_temporary_files, world_paths,
    write_allocation_state, write_material_lock, write_world_checkpoint_receipt,
    ImageMaterial, MaterialLock, SeedObservation, SmolfileObservation, WorldPaths,
    WorldLock, MACHINE_CHECKPOINT_RECEIPT_NAME,
};
use crate::switch::{
    port_socket_path, print_allocations, run_switch, spawn_port_acceptor, wait_for_attachments,
    wait_for_expected_attachments, SwitchEvent,
};
use crate::Result;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod checkpoint;
mod material;

use checkpoint::{
    checkpoint_running_world, verify_world_checkpoint_receipt,
};
use material::{
    prepare_world_material, prepared_seed_files, verify_prepared_world,
};

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
        Cli::Up { config } => up(&config),
        Cli::Check { config } => check(&config),
        Cli::Prepare { config } => prepare(&config),
        Cli::Checkpoint { config, output } => checkpoint(&config, &output),
        Cli::Restore { config, checkpoint } => restore(&config, &checkpoint),
        Cli::Release { config, checkpoint } => release(&config, &checkpoint),
        Cli::Down { config } => down(&config),
        Cli::Ps { config, format } => ps(&config, format),
        Cli::Metrics { config } => metrics(&config),
        Cli::Help => {
            println!("{}", crate::cli::usage());
            Ok(())
        }
        Cli::Exec {
            config,
            machine,
            secret_env,
            command,
        } => exec(&config, &machine, &secret_env, &command),
        Cli::Cp {
            config,
            source,
            destination,
        } => copy(&config, &source, &destination),
    }
}

pub(crate) fn check(config_path: &Path) -> Result<()> {
    let config = load_config(config_path)?;
    topological_order(&config)?;
    let paths = world_paths(config_path)?;
    verify_prepared_world(&config, &paths, &smolvm_program())?;
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
    verify_prepared_world(&config, &paths, &smolvm)?;
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
    Checkpoint { output: PathBuf },
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
    let (verb, argument) = line
        .split_once('\t')
        .ok_or_else(|| "supervisor request is malformed".to_string())?;
    if verb != "checkpoint"
        || argument.is_empty()
        || argument.contains(['\t', '\r', '\n'])
        || !Path::new(argument).is_absolute()
    {
        return Err("supervisor request is malformed".into());
    }
    Ok(RuntimeControlCommand::Checkpoint {
        output: PathBuf::from(argument),
    })
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

pub(crate) fn up(config_path: &Path) -> Result<()> {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    let config = load_config(config_path)?;
    let waves = topological_waves(&config)?;
    let paths = world_paths(config_path)?;
    let smolvm = smolvm_program();
    let _world_lock = WorldLock::acquire(&paths)?;
    let material = verify_prepared_world(&config, &paths, &smolvm)?;

    let recovery = inspect_recovery(&paths)?;
    if recovery.lifecycle.state.retains_checkpoint_sources() {
        return Err(format!(
            "world '{}' has a retained or in-progress durable capture; run `smolworld restore --checkpoint DIR` or explicitly release that checkpoint before a fresh up",
            config.name
        ));
    }
    if recovery.is_recorded_but_absent() {
        eprintln!(
            "smolworld: found recorded allocations for {} but no running machines",
            config.name
        );
    } else if recovery.needs_recovery() {
        eprintln!(
            "smolworld: recovering stale {} state for {}",
            recovery.lifecycle.state.as_str(),
            config.name
        );
    }
    let previous = load_allocation_state(&paths.state_file)?;
    cleanup_machines(&smolvm, previous.as_ref());
    remove_stale_temporary_files(&paths)?;
    remove_runtime_dir(&paths)?;

    let state = allocate_allocation_state(previous, &config, &paths)?;
    write_allocation_state(&paths, &state)?;
    mark_starting(&paths)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let (switch_tx, switch_rx) = mpsc::channel();
    let (attached_tx, attached_rx) = mpsc::channel();
    let gateway = Gateway::new(&config, &state);
    let mut port_handles = Vec::new();
    let mut socket_paths = BTreeMap::new();
    let mut switch_handle = None;
    let mut retain_checkpoint_sources = false;
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
            parallel_machine_operations(wave, "create", |name| {
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
            parallel_machine_operations(wave, "start", |name| {
                start_machine(
                    &smolvm,
                    &state
                        .assignments
                        .get(name)
                        .expect("allocated machine")
                        .smolvm_name,
                )
            })?;
            parallel_machine_operations(wave, "install sealed seed files", |name| {
                let assignment = state.assignments.get(name).expect("allocated machine");
                let seed_files = prepared_seed_files(&paths.config_dir, &material, name)?;
                install_machine_seed_files(&smolvm, &assignment.smolvm_name, &seed_files)
            })?;
        }

        wait_for_attachments(&attached_rx, &config)?;
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

    if !retain_checkpoint_sources {
        cleanup_machines(&smolvm, Some(&state));
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
        let _ = mark_absent(&paths);
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
        cleanup_machines(&smolvm_program(), Some(state));
    }
    remove_stale_temporary_files(&paths)?;
    remove_runtime_dir(&paths)?;
    if state.is_some() {
        mark_absent(&paths)?;
    }
    println!("smolworld: down");
    Ok(())
}

pub(crate) fn ps(config_path: &Path, format: PsFormat) -> Result<()> {
    let config = load_config(config_path)?;
    let paths = world_paths(config_path)?;
    let state = load_allocation_state(&paths.state_file)?;
    let lifecycle = load_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    let smolvm = smolvm_program();
    let mut machines = Vec::new();
    for name in config.machines.keys() {
        let assignment = state.as_ref().and_then(|state| state.assignments.get(name));
        let smolvm_state = match assignment {
            Some(assignment) => machine_status(&smolvm, &assignment.smolvm_name)?,
            None => None,
        };
        let status = display_lifecycle_state(lifecycle.state, smolvm_state);
        machines.push(MachineStatus::new(
            name,
            assignment
                .map(|assignment| assignment.ip.to_string())
                .unwrap_or_else(|| "-".into()),
            assignment
                .map(|assignment| format_mac(assignment.mac))
                .unwrap_or_else(|| "-".into()),
            status,
        ));
    }
    println!("{}", format_ps(format, &machines));
    Ok(())
}

/// Collect read-only host metrics for exactly the configured machines with
/// recorded world allocations. The state file is the identity boundary: this
/// command never lists or discovers unrelated smolvm records.
pub(crate) fn metrics(config_path: &Path) -> Result<()> {
    let config = load_config(config_path)?;
    let paths = world_paths(config_path)?;
    let state = load_allocation_state(&paths.state_file)?;
    let smolvm = smolvm_program();
    let mut machines = Vec::new();

    for machine in config.machines.keys() {
        let Some(assignment) = state
            .as_ref()
            .and_then(|state| state.assignments.get(machine))
        else {
            machines.push(MachineMetrics {
                machine: machine.clone(),
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
            });
            continue;
        };

        require_machine_identity(machine, &assignment.smolvm_name)?;
        let stats = machine_stats(&smolvm, &assignment.smolvm_name)?;
        machines.push(machine_metrics(machine, &stats));
    }

    println!("{}", format_metrics_json(&config.name, &machines));
    Ok(())
}

fn require_machine_identity(machine: &str, smolvm_name: &str) -> Result<()> {
    if smolvm_name.starts_with("smw-")
        && !smolvm_name.contains(['\t', '\r', '\n'])
        && !smolvm_name.contains('/')
    {
        Ok(())
    } else {
        Err(format!(
            "world machine '{machine}' has an unrecognized smolvm identity '{smolvm_name}'"
        ))
    }
}

fn machine_metrics(machine: &str, stats: &MachineStats) -> MachineMetrics {
    MachineMetrics {
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
    machine: &str,
    secret_env: &[String],
    command: &[String],
) -> Result<()> {
    let config = load_config(config_path)?;
    if !config.machines.contains_key(machine) {
        return Err(format!("unknown world machine '{machine}'"));
    }
    let paths = world_paths(config_path)?;
    let state = load_allocation_state(&paths.state_file)?
        .ok_or_else(|| "world has no state; run `smolworld up` first".to_string())?;
    let assignment = state
        .assignments
        .get(machine)
        .ok_or_else(|| format!("machine '{machine}' has no allocation"))?;
    exec_machine(&smolvm_program(), &assignment.smolvm_name, secret_env, command)
}

/// Copy one regular host file to or from exactly one recorded world machine.
/// This is deliberately a namespaced command delegation, not a filesystem
/// sharing mechanism: the smolvm name is resolved only from this world's
/// durable allocation state and is never exposed to callers.
pub(crate) fn copy(config_path: &Path, source: &str, destination: &str) -> Result<()> {
    let config = load_config(config_path)?;
    let paths = world_paths(config_path)?;
    let state = load_allocation_state(&paths.state_file)?
        .ok_or_else(|| "world has no state; run `smolworld up` first".to_string())?;
    let source_remote = parse_copy_remote_endpoint(source)?;
    let destination_remote = parse_copy_remote_endpoint(destination)?;
    let (machine, guest_path, local_path, upload) = match (source_remote, destination_remote) {
        (Some((machine, guest_path)), None) => (machine, guest_path, destination, false),
        (None, Some((machine, guest_path))) => (machine, guest_path, source, true),
        (Some(_), Some(_)) => {
            return Err("smolworld cp accepts exactly one machine:/absolute/path endpoint".into());
        }
        (None, None) => {
            return Err("smolworld cp requires one machine:/absolute/path endpoint".into());
        }
    };
    if !config.machines.contains_key(machine) {
        return Err(format!("unknown world machine '{machine}'"));
    }
    let assignment = state
        .assignments
        .get(machine)
        .ok_or_else(|| format!("machine '{machine}' has no allocation"))?;
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
            "machine copy endpoint must be MACHINE:/absolute/path without traversal".into(),
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
    fn metrics_accepts_only_recorded_machine_identities() {
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
    fn metrics_maps_the_companion_record_without_reinterpreting_it() {
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
        let metrics = machine_metrics("runner", &stats);
        assert_eq!(metrics.machine, "runner");
        assert_eq!(metrics.smolvm_name.as_deref(), Some("smw-demo-runner"));
        assert_eq!(metrics.cpu_millis, Some(2345));
        assert_eq!(metrics.rss_mb, Some(128));
        assert_eq!(metrics.disk_used_mb, Some(64));
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
        }
        for invalid in [
            "checkpoint\trelative\n",
            "checkpoint\t/private/tmp/world\textra\n",
            "restore\t/private/tmp/world\n",
            "checkpoint\t/private/tmp/world\r\n",
        ] {
            assert!(parse(invalid).is_err(), "expected invalid control request {invalid:?}");
        }
    }

    #[test]
    fn checkpoint_staging_never_overwrites_a_visible_artifact() {
        let root = std::env::temp_dir().join(format!(
            "smolworld-checkpoint-staging-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
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
        let names = vec!["first".to_string(), "second".to_string(), "third".to_string()];
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
