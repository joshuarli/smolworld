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
    checkpoint_machine, cleanup_machines, create_machine, machine_stats,
    materialize_external_world, preflight, release_machines, restore_machine, smolvm_program,
    start_machine, status_result, stop_machines, validate_external_world, MachineStats,
};
use crate::state::{
    allocate_v2_allocation_state, digest_file, digest_machine_checkpoint_receipt,
    inspect_v2_recovery, load_v2_allocation_state, load_v2_lifecycle, load_v2_material_lock,
    load_world_checkpoint_receipt, mark_v2_absent, mark_v2_attached, mark_v2_capture_rolled_back,
    mark_v2_captured, mark_v2_capturing, mark_v2_created, mark_v2_running, mark_v2_starting,
    material_lock_resolver_abi, normalize_relative_path, prepare_v2_runtime_dir,
    remove_v2_runtime_dir, remove_v2_stale_temporary_files, v2_world_paths,
    write_v2_allocation_state, write_v2_material_lock, write_world_checkpoint_receipt,
    V2ImageMaterial, V2MaterialLock, V2SeedObservation, V2SmolfileObservation, V2WorldPaths,
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
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    let paths = v2_world_paths(config_path)?;
    verify_prepared_world(&config, &paths, &smolvm_program())?;
    println!("smolworld: {} is ready", config.name);
    Ok(())
}

/// Seal all host-inspectable machine inputs into `.smolworld.lock` without
/// allocating world state, binding a listener, or creating a smolvm machine.
pub(crate) fn prepare(config_path: &Path) -> Result<()> {
    let config = load_config(config_path)?;
    topological_order(&config)?;
    let paths = v2_world_paths(config_path)?;
    let smolvm = smolvm_program();
    preflight(&config, &paths.config_dir, &smolvm)?;
    let material = prepare_world_material(&config, &paths, &smolvm)?;
    write_v2_material_lock(&paths, &material)?;
    println!("smolworld: prepared {}", config.name);
    Ok(())
}

/// Ask the current supervisor, rather than a second CLI process, to close the
/// switch epoch and capture its live machines. The runtime directory socket is
/// private to this exact v2 world and vanishes when the supervisor exits.
pub(crate) fn checkpoint(config_path: &Path, output: &Path) -> Result<()> {
    if !output.is_absolute() {
        return Err("checkpoint --output must be an absolute directory".into());
    }
    let paths = v2_world_paths(config_path)?;
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
    let paths = v2_world_paths(config_path)?;
    let smolvm = smolvm_program();
    let _world_lock = WorldLock::acquire_v2(&paths)?;
    verify_prepared_world(&config, &paths, &smolvm)?;
    let lifecycle = load_v2_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
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
    let state = load_v2_allocation_state(&paths.state_file)?
        .ok_or_else(|| "captured world has no allocation state".to_string())?;
    let receipt = load_world_checkpoint_receipt(checkpoint)?;
    verify_world_checkpoint_receipt(&config, &paths, &state, checkpoint, &receipt)?;
    remove_v2_stale_temporary_files(&paths)?;
    remove_v2_runtime_dir(&paths)?;
    mark_v2_starting(&paths)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let (switch_tx, switch_rx) = mpsc::channel();
    let (attached_tx, attached_rx) = mpsc::channel();
    let gateway = Gateway::new(&config, &state);
    let mut port_handles = Vec::new();
    let mut switch_handle = None;
    let retain_checkpoint_sources = true;
    let result = (|| {
        prepare_v2_runtime_dir(&paths)?;
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
        mark_v2_attached(&paths)?;
        mark_v2_running(&paths)?;
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
    let _ = remove_v2_runtime_dir(&paths);
    if retain_checkpoint_sources {
        let _ = mark_v2_captured(&paths);
    }
    result
}

fn verify_world_checkpoint_receipt(
    config: &WorldConfig,
    paths: &V2WorldPaths,
    state: &crate::model::WorldAllocationState,
    checkpoint: &Path,
    receipt: &WorldCheckpointReceipt,
) -> Result<()> {
    if receipt.world_name != config.name {
        return Err(format!(
            "checkpoint belongs to world '{}' rather than '{}",
            receipt.world_name, config.name
        ));
    }
    if receipt.config_digest != digest_file(&paths.canonical_config)? {
        return Err("checkpoint world declaration no longer matches this configuration".into());
    }
    if receipt.material_lock_digest != digest_file(&paths.material_lock_path())? {
        return Err("checkpoint prepared material no longer matches this world".into());
    }
    if receipt.allocation != *state {
        return Err("checkpoint allocation does not match the retained world identity".into());
    }
    if receipt
        .allocation
        .assignments
        .keys()
        .ne(config.machines.keys())
    {
        return Err("checkpoint machine set does not match the configured world".into());
    }
    if receipt.machine_receipts.keys().ne(config.machines.keys()) {
        return Err("checkpoint machine receipt set does not match the configured world".into());
    }
    for name in config.machines.keys() {
        let receipt_path = checkpoint
            .join("machines")
            .join(name)
            .join(MACHINE_CHECKPOINT_RECEIPT_NAME);
        let actual = digest_machine_checkpoint_receipt(&receipt_path)
            .map_err(|error| format!("checkpoint machine '{name}' receipt: {error}"))?;
        let expected = &receipt
            .machine_receipts
            .get(name)
            .expect("validated machine receipt set")
            .digest;
        if actual != *expected {
            return Err(format!(
                "checkpoint machine '{name}' receipt digest does not match the world receipt"
            ));
        }
    }
    Ok(())
}

/// Permanently release one retained state. This is the only durable-world path
/// that deletes source VM records or the checkpoint artifact, and it validates
/// that both still belong to the requested world before touching either.
pub(crate) fn release(config_path: &Path, checkpoint: &Path) -> Result<()> {
    if !checkpoint.is_absolute() {
        return Err("release --checkpoint must be an absolute directory".into());
    }
    let config = load_config(config_path)?;
    let paths = v2_world_paths(config_path)?;
    let _world_lock = WorldLock::acquire_v2(&paths)?;
    let lifecycle = load_v2_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
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
    let state = load_v2_allocation_state(&paths.state_file)?
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
    mark_v2_absent(&paths)?;
    println!("smolworld: released checkpoint {}", checkpoint.display());
    Ok(())
}

/// Control messages accepted only by the process that owns the switch and its
/// recorded world lock. Keep this deliberately small and typed so an external
/// world adapter can invoke it without reconstructing lifecycle state.
enum RuntimeControlCommand {
    Checkpoint { output: PathBuf },
}

fn runtime_control_socket_path(paths: &V2WorldPaths) -> PathBuf {
    paths.runtime_dir.join("control.sock")
}

fn bind_runtime_control_listener(paths: &V2WorldPaths) -> Result<UnixListener> {
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
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("set supervisor request timeout: {error}"))?;
    let line = read_runtime_control_line(stream)?;
    let (verb, argument) = line
        .split_once('\t')
        .ok_or_else(|| "supervisor request is malformed".to_string())?;
    if verb != "checkpoint" || argument.is_empty() || argument.contains(['\t', '\r', '\n']) {
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
    let paths = v2_world_paths(config_path)?;
    let smolvm = smolvm_program();
    let _world_lock = WorldLock::acquire_v2(&paths)?;
    let material = verify_prepared_world(&config, &paths, &smolvm)?;

    let recovery = inspect_v2_recovery(&paths)?;
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
    let previous = load_v2_allocation_state(&paths.state_file)?;
    cleanup_machines(&smolvm, previous.as_ref());
    remove_v2_stale_temporary_files(&paths)?;
    remove_v2_runtime_dir(&paths)?;

    let state = allocate_v2_allocation_state(previous, &config, &paths)?;
    write_v2_allocation_state(&paths, &state)?;
    mark_v2_starting(&paths)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let (switch_tx, switch_rx) = mpsc::channel();
    let (attached_tx, attached_rx) = mpsc::channel();
    let gateway = Gateway::new(&config, &state);
    let mut port_handles = Vec::new();
    let mut socket_paths = BTreeMap::new();
    let mut switch_handle = None;
    let mut retain_checkpoint_sources = false;
    let result = (|| {
        prepare_v2_runtime_dir(&paths)?;
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
                let seed_files = prepared_seed_files(&paths.config_dir, &material, name)?;
                create_machine(
                    &smolvm,
                    MachineLaunch {
                        assignment,
                        socket: socket_paths.get(name).expect("socket allocated"),
                        smolfile: &smolfile.prepared_path,
                        seed_files: &seed_files,
                    },
                    &config.network,
                )
            })?;
        }
        mark_v2_created(&paths)?;
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
        }

        wait_for_attachments(&attached_rx, &config)?;
        mark_v2_attached(&paths)?;
        print_allocations(&config, &state);
        mark_v2_running(&paths)?;
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
                                        match load_v2_lifecycle(&paths.lifecycle_path()) {
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
    let _ = remove_v2_runtime_dir(&paths);
    if !retain_checkpoint_sources {
        let _ = mark_v2_absent(&paths);
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

/// Freeze every machine behind one closed switch epoch, publish the per-machine
/// durable receipts beneath `output`, then publish the world receipt last.
/// Independent machine capture remains parallel; the output is all-or-nothing
/// from the caller's point of view because any pre-publication failure restores
/// every machine that did finish capture before forwarding resumes.
fn checkpoint_running_world(
    config: &WorldConfig,
    state: &crate::model::WorldAllocationState,
    paths: &V2WorldPaths,
    smolvm: &Path,
    switch_tx: &mpsc::Sender<SwitchEvent>,
    attached_rx: &mpsc::Receiver<String>,
    output: &Path,
) -> Result<()> {
    let (parent, staging) = create_world_checkpoint_staging(output)?;
    if let Err(error) = mark_v2_capturing(paths) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    let machines_root = staging.join("machines");
    if let Err(error) = fs::create_dir(&machines_root).map_err(|error| {
        format!(
            "create checkpoint machine root {}: {error}",
            machines_root.display()
        )
    }) {
        return abandon_unstarted_world_checkpoint(paths, &staging, error);
    }
    let switch = match quiesce_switch(switch_tx) {
        Ok(receipt) => receipt,
        Err(error) => return abandon_unstarted_world_checkpoint(paths, &staging, error),
    };
    let rollback = CheckpointRollback {
        paths,
        smolvm,
        state,
        staging: &staging,
        switch_tx,
        attached_rx,
    };

    let names: Vec<_> = config.machines.keys().cloned().collect();
    if switch.queued_frames != 0 || switch.active_ports.keys().ne(config.machines.keys()) {
        return rollback_world_checkpoint(
            &rollback,
            &[],
            "switch checkpoint cut does not match the running world ports".to_string(),
        );
    }
    let captures = parallel_checkpoint_machines(&names, smolvm, state, &machines_root);
    let completed: Vec<_> = captures
        .iter()
        .filter_map(|(name, result)| result.is_ok().then_some(name.clone()))
        .collect();
    if let Some((name, error)) = captures.iter().find_map(|(name, result)| {
        result
            .as_ref()
            .err()
            .map(|error| (name.as_str(), error.as_str()))
    }) {
        return rollback_world_checkpoint(
            &rollback,
            &completed,
            format!("checkpoint machine '{name}': {error}"),
        );
    }

    let machine_receipts = match names
        .iter()
        .map(|name| {
            let path = machines_root
                .join(name)
                .join(MACHINE_CHECKPOINT_RECEIPT_NAME);
            digest_machine_checkpoint_receipt(&path)
                .map(|digest| (name.clone(), MachineCheckpointReceipt { digest }))
        })
        .collect::<Result<BTreeMap<_, _>>>()
    {
        Ok(receipts) => receipts,
        Err(error) => {
            return rollback_world_checkpoint(
                &rollback,
                &completed,
                format!("inspect captured machine receipts: {error}"),
            )
        }
    };

    let receipt = WorldCheckpointReceipt {
        schema_version: WORLD_CHECKPOINT_RECEIPT_VERSION,
        world_name: config.name.clone(),
        config_digest: digest_file(&paths.canonical_config)?,
        material_lock_digest: digest_file(&paths.material_lock_path())?,
        allocation: state.clone(),
        machine_receipts,
        switch,
    };
    if let Err(error) = write_world_checkpoint_receipt(&staging, &receipt) {
        return rollback_world_checkpoint(&rollback, &completed, error);
    }
    if let Err(error) = fs::rename(&staging, output) {
        return rollback_world_checkpoint(
            &rollback,
            &completed,
            format!("publish checkpoint {}: {error}", output.display()),
        );
    }
    if let Err(error) = File::open(&parent).and_then(|directory| directory.sync_all()) {
        return Err(format!(
            "checkpoint is published at {} but parent directory sync failed: {error}",
            output.display()
        ));
    }
    // The artifact is already visible if this final state write fails. Leave
    // the earlier `Capturing` intent in place so `up` cannot clean its stopped
    // sources; `restore`/`release` accept that recoverable state after receipt
    // verification.
    mark_v2_captured(paths)?;
    Ok(())
}

fn abandon_unstarted_world_checkpoint(
    paths: &V2WorldPaths,
    staging: &Path,
    original_error: String,
) -> Result<()> {
    let remove = fs::remove_dir_all(staging).map_err(|error| {
        format!(
            "remove unstarted checkpoint staging {}: {error}",
            staging.display()
        )
    });
    let lifecycle = mark_v2_capture_rolled_back(paths);
    match (remove, lifecycle) {
        (Ok(()), Ok(_)) => Err(original_error),
        (remove, lifecycle) => Err(format!(
            "{original_error}; checkpoint capture intent retained: staging cleanup: {}; lifecycle rollback: {}",
            remove
                .err()
                .unwrap_or_else(|| "ok".to_string()),
            lifecycle
                .err()
                .unwrap_or_else(|| "ok".to_string()),
        )),
    }
}

fn create_world_checkpoint_staging(output: &Path) -> Result<(PathBuf, PathBuf)> {
    if !output.is_absolute() {
        return Err("checkpoint --output must be an absolute directory".into());
    }
    match fs::symlink_metadata(output) {
        Ok(_) => {
            return Err(format!(
                "refusing to overwrite checkpoint output {}",
                output.display()
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "inspect checkpoint output {}: {error}",
                output.display()
            ))
        }
    }
    let parent = output
        .parent()
        .ok_or_else(|| "checkpoint output has no parent directory".to_string())?
        .to_path_buf();
    let metadata = fs::symlink_metadata(&parent).map_err(|error| {
        format!(
            "inspect checkpoint output parent {}: {error}",
            parent.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "checkpoint output parent is not a directory: {}",
            parent.display()
        ));
    }
    let stem = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "checkpoint output name is not valid UTF-8".to_string())?;
    for attempt in 0..128_u64 {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let staging = parent.join(format!(
            ".{stem}.smolworld-capture-{:x}-{:x}-{:x}.partial",
            std::process::id(),
            nonce,
            attempt
        ));
        match fs::create_dir(&staging) {
            Ok(()) => return Ok((parent, staging)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "create checkpoint staging {}: {error}",
                    staging.display()
                ))
            }
        }
    }
    Err("could not allocate a unique checkpoint staging directory".into())
}

fn quiesce_switch(
    switch_tx: &mpsc::Sender<SwitchEvent>,
) -> Result<crate::model::SwitchCheckpointReceipt> {
    let (ack_tx, ack_rx) = mpsc::channel();
    switch_tx
        .send(SwitchEvent::Quiesce {
            acknowledged: ack_tx,
        })
        .map_err(|error| format!("request switch quiescence: {error}"))?;
    ack_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "timed out waiting for switch quiescence".to_string())
}

fn resume_switch(switch_tx: &mpsc::Sender<SwitchEvent>) {
    let _ = switch_tx.send(SwitchEvent::Resume);
}

fn parallel_checkpoint_machines(
    names: &[String],
    smolvm: &Path,
    state: &crate::model::WorldAllocationState,
    machines_root: &Path,
) -> Vec<(String, Result<()>)> {
    thread::scope(|scope| {
        let handles: Vec<_> = names
            .iter()
            .map(|name| {
                let assignment = state.assignments.get(name).expect("allocated machine");
                let checkpoint = machines_root.join(name);
                scope
                    .spawn(move || checkpoint_machine(smolvm, &assignment.smolvm_name, &checkpoint))
            })
            .collect();
        handles
            .into_iter()
            .zip(names)
            .map(|(handle, name)| {
                let result = handle
                    .join()
                    .map_err(|_| format!("checkpoint machine '{name}' worker panicked"))
                    .and_then(|result| result);
                (name.clone(), result)
            })
            .collect()
    })
}

/// All state needed to return a pre-publish capture failure to the same live
/// world. Keeping the rollback boundary explicit makes it harder to resume
/// forwarding before every successfully frozen machine has fresh attachments.
struct CheckpointRollback<'a> {
    paths: &'a V2WorldPaths,
    smolvm: &'a Path,
    state: &'a crate::model::WorldAllocationState,
    staging: &'a Path,
    switch_tx: &'a mpsc::Sender<SwitchEvent>,
    attached_rx: &'a mpsc::Receiver<String>,
}

fn rollback_world_checkpoint(
    rollback: &CheckpointRollback<'_>,
    completed: &[String],
    original_error: String,
) -> Result<()> {
    let restore = parallel_machine_operations(completed, "rollback checkpoint", |name| {
        let assignment = rollback
            .state
            .assignments
            .get(name)
            .expect("allocated machine");
        restore_machine(
            rollback.smolvm,
            &assignment.smolvm_name,
            &rollback.staging.join("machines").join(name),
        )
    });
    let attached = restore.and_then(|()| {
        wait_for_expected_attachments(
            rollback.attached_rx,
            completed.iter().cloned().collect::<HashSet<_>>(),
        )
    });
    resume_switch(rollback.switch_tx);
    match attached {
        Ok(()) => {
            if let Err(error) = mark_v2_capture_rolled_back(rollback.paths) {
                return Err(format!(
                    "{original_error}; checkpoint rollback restored the world but could not clear its capture intent: {error}"
                ));
            }
            fs::remove_dir_all(rollback.staging).map_err(|error| {
                format!(
                    "{original_error}; remove rolled-back checkpoint staging {}: {error}",
                    rollback.staging.display()
                )
            })?;
            Err(original_error)
        }
        Err(rollback_error) => Err(format!(
            "{original_error}; checkpoint rollback failed: {rollback_error}; staging preserved at {}",
            rollback.staging.display()
        )),
    }
}

pub(crate) fn down(config_path: &Path) -> Result<()> {
    let paths = v2_world_paths(config_path)?;
    let _world_lock = WorldLock::acquire_v2(&paths)?;
    let state = load_v2_allocation_state(&paths.state_file)?;
    let lifecycle = load_v2_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
    if lifecycle.state.retains_checkpoint_sources() {
        return Err(
            "world has a retained durable checkpoint; use `smolworld release --checkpoint DIR` to delete its exact source machines and artifact"
                .into(),
        );
    }
    if let Some(state) = &state {
        cleanup_machines(&smolvm_program(), Some(state));
    }
    remove_v2_stale_temporary_files(&paths)?;
    remove_v2_runtime_dir(&paths)?;
    if state.is_some() {
        mark_v2_absent(&paths)?;
    }
    println!("smolworld: down");
    Ok(())
}

pub(crate) fn ps(config_path: &Path, format: PsFormat) -> Result<()> {
    let config = load_config(config_path)?;
    let paths = v2_world_paths(config_path)?;
    let state = load_v2_allocation_state(&paths.state_file)?;
    let lifecycle = load_v2_lifecycle(&paths.lifecycle_path())?.unwrap_or_default();
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
/// recorded v2 allocations. The state file is the identity boundary: this
/// command never lists or discovers unrelated smolvm records.
pub(crate) fn metrics(config_path: &Path) -> Result<()> {
    let config = load_config(config_path)?;
    let paths = v2_world_paths(config_path)?;
    let state = load_v2_allocation_state(&paths.state_file)?;
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

        require_v2_machine_identity(machine, &assignment.smolvm_name)?;
        let stats = machine_stats(&smolvm, &assignment.smolvm_name)?;
        machines.push(machine_metrics(machine, &stats));
    }

    println!("{}", format_metrics_json(&config.name, &machines));
    Ok(())
}

fn require_v2_machine_identity(machine: &str, smolvm_name: &str) -> Result<()> {
    if smolvm_name.starts_with("smw-v2-")
        && !smolvm_name.contains(['\t', '\r', '\n'])
        && !smolvm_name.contains('/')
    {
        Ok(())
    } else {
        Err(format!(
            "world machine '{machine}' has non-v2 smolvm identity '{smolvm_name}'"
        ))
    }
}

fn machine_metrics(machine: &str, stats: &MachineStats) -> MachineMetrics {
    MachineMetrics {
        machine: machine.to_string(),
        smolvm_name: Some(stats.name.clone()),
        state: stats.state.clone(),
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

fn machine_status(smolvm: &Path, name: &str) -> Result<Option<&'static str>> {
    let output = Command::new(smolvm)
        .args(["machine", "status", "--name", name])
        .output()
        .map_err(|error| format!("run smolvm machine status: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(["running", "created", "stopped", "failed", "unreachable"]
        .into_iter()
        .find(|state| text.split_whitespace().any(|word| word == *state)))
}

fn display_lifecycle_state(
    lifecycle: LifecycleState,
    smolvm_state: Option<&str>,
) -> DisplayLifecycleState {
    let Some(smolvm_state) = smolvm_state else {
        return DisplayLifecycleState::Absent;
    };
    match lifecycle {
        LifecycleState::Capturing => return DisplayLifecycleState::Capturing,
        LifecycleState::Captured => return DisplayLifecycleState::Captured,
        _ => {}
    }
    if smolvm_state != "running" {
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
    let paths = v2_world_paths(config_path)?;
    let state = load_v2_allocation_state(&paths.state_file)?
        .ok_or_else(|| "world has no state; run `smolworld up` first".to_string())?;
    let assignment = state
        .assignments
        .get(machine)
        .ok_or_else(|| format!("machine '{machine}' has no allocation"))?;
    let mut invocation = Command::new(smolvm_program());
    invocation
        .arg("machine")
        .arg("exec")
        .arg("--name")
        .arg(&assignment.smolvm_name);
    for value in secret_env {
        invocation.arg("--secret-env").arg(value);
    }
    let status = invocation
        .arg("--")
        .args(command)
        .status()
        .map_err(|error| format!("run smolvm machine exec: {error}"))?;
    status_result("smolvm machine exec", status)
}

/// Copy one regular host file to or from exactly one recorded world machine.
/// This is deliberately a namespaced command delegation, not a filesystem
/// sharing mechanism: the smolvm name is resolved only from this world's
/// durable allocation state and is never exposed to callers.
pub(crate) fn copy(config_path: &Path, source: &str, destination: &str) -> Result<()> {
    let config = load_config(config_path)?;
    let paths = v2_world_paths(config_path)?;
    let state = load_v2_allocation_state(&paths.state_file)?
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
    let remote = format!("{}:{guest_path}", assignment.smolvm_name);
    let status = if upload {
        Command::new(smolvm_program())
            .arg("machine")
            .arg("cp")
            .arg(local_path)
            .arg(remote)
            .status()
    } else {
        Command::new(smolvm_program())
            .arg("machine")
            .arg("cp")
            .arg(remote)
            .arg(local_path)
            .status()
    }
    .map_err(|error| format!("run smolvm machine cp: {error}"))?;
    status_result("smolvm machine cp", status)
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

fn verify_prepared_world(
    config: &WorldConfig,
    paths: &V2WorldPaths,
    smolvm: &Path,
) -> Result<V2MaterialLock> {
    preflight(config, &paths.config_dir, smolvm)?;
    let prepared = load_v2_material_lock(&paths.material_lock_path())?.ok_or_else(|| {
        format!(
            "world material lock is missing at {}; run `smolworld prepare` first",
            paths.material_lock_path().display()
        )
    })?;
    verify_material_lock(config, paths, smolvm, &prepared)?;
    Ok(prepared)
}

/// Resolve, download, and seal every host input that can affect a
/// Smolfile-composed world. This is the explicit mutating `prepare` boundary:
/// immutable registry sources become local archives and local-only prepared
/// Smolfiles before any allocation state or listener exists.
fn prepare_world_material(
    config: &WorldConfig,
    paths: &V2WorldPaths,
    smolvm: &Path,
) -> Result<V2MaterialLock> {
    let mut lock =
        V2MaterialLock::from_config(&paths.canonical_config, material_lock_resolver_abi())?;
    let names: Vec<_> = config.machines.keys().cloned().collect();
    let indices: BTreeMap<_, _> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    let prepared = parallel_machine_map(&names, "prepare material", |name| {
        prepare_one_machine_material(config, paths, smolvm, name, indices[name])
    })?;
    for (name, prepared) in names.into_iter().zip(prepared) {
        if lock
            .smolfiles
            .insert(name.clone(), prepared.smolfile)
            .is_some()
        {
            return Err(format!("material observation repeats machine '{name}'"));
        }
        if lock.images.insert(name.clone(), prepared.image).is_some() {
            return Err(format!(
                "material observation repeats image for machine '{name}'"
            ));
        }
        lock.seeds.extend(prepared.seeds);
    }
    lock.validate()?;
    Ok(lock)
}

struct PreparedMachineMaterial {
    smolfile: V2SmolfileObservation,
    image: V2ImageMaterial,
    seeds: Vec<V2SeedObservation>,
}

fn prepare_one_machine_material(
    config: &WorldConfig,
    paths: &V2WorldPaths,
    smolvm: &Path,
    name: &str,
    index: usize,
) -> Result<PreparedMachineMaterial> {
    let machine = config
        .machines
        .get(name)
        .expect("prepared machine is configured");
    let assignment = validation_assignment(paths, config, name, index)?;
    let socket = validation_socket_path(paths, name);
    let authored_relative_path =
        normalize_relative_path(&machine.smolfile, "configured Smolfile path")?;
    let authored_smolfile =
        sealed_relative_file(&paths.config_dir, &authored_relative_path, "Smolfile")?;
    let preparation = materialize_external_world(smolvm, &authored_smolfile)?;
    let material = validate_external_world(
        smolvm,
        &preparation.prepared_smolfile,
        &assignment,
        &socket,
        &config.network,
    )?;
    let authored_digest = digest_file(&preparation.authored_smolfile)?;
    let prepared_digest = digest_file(&material.smolfile)?;
    let seeds = machine
        .seed_files
        .iter()
        .map(|seed| {
            let source_relative_path =
                normalize_relative_path(&seed.source, "configured seed source")?;
            let source =
                sealed_relative_file(&paths.config_dir, &source_relative_path, "seed source")?;
            validate_seed_destination(&seed.destination)?;
            Ok(V2SeedObservation {
                machine: name.to_string(),
                source_relative_path,
                destination: seed.destination.to_string_lossy().into_owned(),
                mode: seed.mode,
                digest: digest_file(&source)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PreparedMachineMaterial {
        smolfile: V2SmolfileObservation {
            authored_relative_path,
            authored_digest,
            prepared_path: material.smolfile,
            prepared_digest,
        },
        image: V2ImageMaterial {
            machine: name.to_string(),
            source_kind: preparation.source_kind,
            source_reference: preparation.source_reference,
            source_digest: preparation.source_digest,
            local_path: material.local_archive,
            image_digest: material.image_digest,
        },
        seeds,
    })
}

/// Revalidate a material lock without materializing or contacting a registry.
/// `check` and `up` use only the exact local inputs sealed by `prepare`.
fn verify_material_lock(
    config: &WorldConfig,
    paths: &V2WorldPaths,
    smolvm: &Path,
    prepared: &V2MaterialLock,
) -> Result<()> {
    prepared.validate()?;
    if prepared.resolver_abi != material_lock_resolver_abi() {
        return Err(format!(
            "world material uses resolver ABI '{}', but this smolworld requires '{}'; run `smolworld prepare` again",
            prepared.resolver_abi,
            material_lock_resolver_abi()
        ));
    }
    let current =
        V2MaterialLock::from_config(&paths.canonical_config, material_lock_resolver_abi())?;
    if prepared.world != current.world {
        return Err(format!(
            "world declaration no longer matches {}; run `smolworld prepare` again",
            paths.material_lock_path().display()
        ));
    }
    if prepared.smolfiles.len() != config.machines.len()
        || prepared.images.len() != config.machines.len()
    {
        return Err(
            "world material does not contain exactly one Smolfile and image per machine".into(),
        );
    }

    let names: Vec<_> = config.machines.keys().cloned().collect();
    let indices: BTreeMap<_, _> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (name.as_str(), index))
        .collect();
    let mut expected_seeds = parallel_machine_map(&names, "verify material", |name| {
        verify_one_machine_material(config, paths, smolvm, prepared, name, indices[name])
    })?
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    expected_seeds.sort_by(seed_identity);
    let mut locked_seeds = prepared.seeds.clone();
    locked_seeds.sort_by(seed_identity);
    if locked_seeds != expected_seeds {
        return Err(
            "sealed seed inputs no longer match the prepared world; run `smolworld prepare` again"
                .into(),
        );
    }
    Ok(())
}

fn verify_one_machine_material(
    config: &WorldConfig,
    paths: &V2WorldPaths,
    smolvm: &Path,
    prepared: &V2MaterialLock,
    name: &str,
    index: usize,
) -> Result<Vec<V2SeedObservation>> {
    let machine = config
        .machines
        .get(name)
        .expect("verified machine is configured");
    let observation = prepared
        .smolfiles
        .get(name)
        .ok_or_else(|| format!("world material is missing the Smolfile for machine '{name}'"))?;
    let authored_relative_path =
        normalize_relative_path(&machine.smolfile, "configured Smolfile path")?;
    let authored = sealed_relative_file(&paths.config_dir, &authored_relative_path, "Smolfile")?;
    if observation.authored_relative_path != authored_relative_path
        || digest_file(&authored)? != observation.authored_digest
    {
        return Err(format!(
            "authored Smolfile for machine '{name}' no longer matches the prepared world; run smolworld prepare again"
        ));
    }
    let metadata = fs::metadata(&observation.prepared_path).map_err(|error| {
        format!(
            "inspect prepared Smolfile {}: {error}",
            observation.prepared_path.display()
        )
    })?;
    if !metadata.is_file()
        || digest_file(&observation.prepared_path)? != observation.prepared_digest
    {
        return Err(format!(
            "prepared Smolfile for machine '{name}' no longer matches the material lock; run smolworld prepare again"
        ));
    }
    let assignment = validation_assignment(paths, config, name, index)?;
    let socket = validation_socket_path(paths, name);
    let material = validate_external_world(
        smolvm,
        &observation.prepared_path,
        &assignment,
        &socket,
        &config.network,
    )?;
    let image = prepared
        .images
        .get(name)
        .ok_or_else(|| format!("world material is missing the image for machine '{name}'"))?;
    if material.smolfile != observation.prepared_path
        || material.local_archive != image.local_path
        || material.image_digest != image.image_digest
    {
        return Err(format!(
            "prepared image for machine '{name}' no longer matches the material lock; run smolworld prepare again"
        ));
    }
    machine
        .seed_files
        .iter()
        .map(|seed| {
            let source_relative_path =
                normalize_relative_path(&seed.source, "configured seed source")?;
            let source =
                sealed_relative_file(&paths.config_dir, &source_relative_path, "seed source")?;
            validate_seed_destination(&seed.destination)?;
            Ok(V2SeedObservation {
                machine: name.to_string(),
                source_relative_path,
                destination: seed.destination.to_string_lossy().into_owned(),
                mode: seed.mode,
                digest: digest_file(&source)?,
            })
        })
        .collect()
}

fn seed_identity(left: &V2SeedObservation, right: &V2SeedObservation) -> std::cmp::Ordering {
    (
        &left.machine,
        &left.source_relative_path,
        &left.destination,
        left.mode,
        &left.digest,
    )
        .cmp(&(
            &right.machine,
            &right.source_relative_path,
            &right.destination,
            right.mode,
            &right.digest,
        ))
}

/// Create a deterministic, non-persisted NIC identity for smolvm's read-only
/// resolver. Runtime allocation remains separate and is written only by `up`.
fn validation_assignment(
    paths: &V2WorldPaths,
    config: &WorldConfig,
    machine: &str,
    index: usize,
) -> Result<Assignment> {
    let host = u8::try_from(index + 2)
        .ok()
        .filter(|host| *host <= 254)
        .ok_or_else(|| "world has more machines than its /24 can validate".to_string())?;
    let mut mac = [0x02, 0, 0, 0, 0, host];
    let hash = paths.hash.to_be_bytes();
    mac[1..5].copy_from_slice(&hash[4..8]);
    Ok(Assignment {
        ip: std::net::Ipv4Addr::new(
            config.network.subnet[0],
            config.network.subnet[1],
            config.network.subnet[2],
            host,
        ),
        mac,
        smolvm_name: format!("smw-v2-validate-{machine}"),
    })
}

fn validation_socket_path(paths: &V2WorldPaths, machine: &str) -> PathBuf {
    paths.runtime_dir.join(format!("validate-{machine}.sock"))
}

fn sealed_relative_file(config_dir: &Path, relative_path: &Path, label: &str) -> Result<PathBuf> {
    let relative_path = normalize_relative_path(relative_path, label)?;
    let source = config_dir.join(&relative_path);
    let metadata = fs::symlink_metadata(&source)
        .map_err(|error| format!("inspect {label} {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!(
            "{label} {} must be a sealed regular file, not a symlink or directory",
            source.display()
        ));
    }
    let canonical = fs::canonicalize(&source)
        .map_err(|error| format!("resolve {label} {}: {error}", source.display()))?;
    if !canonical.starts_with(config_dir) {
        return Err(format!(
            "{label} {} resolves outside the .smolworld directory",
            source.display()
        ));
    }
    let source_text = canonical
        .to_str()
        .ok_or_else(|| format!("{label} {} is not valid UTF-8", canonical.display()))?;
    if source_text.contains('=') {
        return Err(format!(
            "{label} {} must not contain '=' because the smolvm seed-file ABI uses SOURCE=DESTINATION:MODE", canonical.display()
        ));
    }
    Ok(canonical)
}

fn validate_seed_destination(destination: &Path) -> Result<()> {
    let destination_text = destination.to_str().ok_or_else(|| {
        format!(
            "seed destination {} is not valid UTF-8",
            destination.display()
        )
    })?;
    if !destination.is_absolute()
        || destination.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
        || destination_text == "/"
        || destination_text.ends_with('/')
        || destination_text.contains("//")
        || destination_text.contains([':', '='])
    {
        return Err(format!(
            "seed destination {} must be a non-root normalized absolute guest path without ':' or '='",
            destination.display()
        ));
    }
    Ok(())
}

/// Convert sealed lock observations back into smolvm's pre-workload seed-file
/// input. The lock is re-observed before this is called, so these are canonical
/// regular-file paths whose content digests still match the prepared world.
fn prepared_seed_files(
    config_dir: &Path,
    material: &V2MaterialLock,
    machine: &str,
) -> Result<Vec<SeedFile>> {
    material
        .seeds
        .iter()
        .filter(|seed| seed.machine == machine)
        .map(|seed| {
            let source = sealed_relative_file(config_dir, &seed.source_relative_path, "seed source")?;
            if source.to_string_lossy().contains('=')
                || seed.destination.contains([':', '='])
            {
                return Err(format!(
                    "sealed seed for machine '{machine}' cannot be encoded for smolvm's SOURCE=DESTINATION:MODE ABI"
                ));
            }
            Ok(SeedFile {
                source,
                destination: PathBuf::from(&seed.destination),
                mode: seed.mode,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_destinations_match_the_atomic_smolvm_seed_abi() {
        assert!(validate_seed_destination(Path::new("/etc/app/config.toml")).is_ok());
        for invalid in [
            "relative/config.toml",
            "/",
            "/etc/app/",
            "/etc//app/config.toml",
            "/etc/app/config:old.toml",
            "/etc/app/config=old.toml",
            "/etc/app/../config.toml",
        ] {
            assert!(
                validate_seed_destination(Path::new(invalid)).is_err(),
                "expected invalid seed destination {invalid}"
            );
        }
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
    fn metrics_accepts_only_recorded_v2_machine_identities() {
        assert!(require_v2_machine_identity("runner", "smw-v2-abcdef-0123").is_ok());
        for invalid in [
            "runner",
            "smolvm-runner",
            "smw-v1-abcdef-0123",
            "smw-v2-/runner",
            "smw-v2-runner\tother",
        ] {
            assert!(
                require_v2_machine_identity("runner", invalid).is_err(),
                "expected non-v2 identity to be rejected: {invalid:?}"
            );
        }
    }

    #[test]
    fn metrics_maps_the_companion_record_without_reinterpreting_it() {
        let stats = MachineStats {
            name: "smw-v2-demo-runner".into(),
            state: "running".into(),
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
        assert_eq!(metrics.smolvm_name.as_deref(), Some("smw-v2-demo-runner"));
        assert_eq!(metrics.cpu_millis, Some(2345));
        assert_eq!(metrics.rss_mb, Some(128));
        assert_eq!(metrics.disk_used_mb, Some(64));
    }
}
