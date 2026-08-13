use crate::cli::{
    format_ps, Cli, LifecycleState as DisplayLifecycleState, MachineStatus, PsFormat,
};
use crate::config::{load_config, topological_order};
use crate::gateway::Gateway;
use crate::model::{format_mac, LifecycleState, MachineLaunch};
use crate::smolvm::{
    cleanup_machines, create_machine, local_image_path, preflight, smolvm_program, start_machine,
    status_result,
};
use crate::state::{
    allocate_state, inspect_recovery, load_lifecycle, load_state, mark_absent, mark_attached,
    mark_created, mark_running, mark_starting, remove_stale_temporary_files, world_paths,
    write_state, WorldLock,
};
use crate::switch::{
    port_socket_path, prepare_runtime_dir, print_allocations, remove_runtime_dir, run_switch,
    spawn_port_acceptor, wait_for_attachments, SwitchEvent,
};
use crate::Result;
use std::collections::BTreeMap;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

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
        Cli::Down { config } => down(&config),
        Cli::Ps { config, format } => ps(&config, format),
        Cli::Help => {
            println!("{}", crate::cli::usage());
            Ok(())
        }
        Cli::Exec {
            config,
            machine,
            command,
        } => exec(&config, &machine, &command),
    }
}

pub(crate) fn check(config_path: &Path) -> Result<()> {
    let config = load_config(config_path)?;
    topological_order(&config)?;
    let paths = world_paths(config_path)?;
    preflight(&config, &paths, &smolvm_program())?;
    println!("smolworld: {} is ready", config.name);
    Ok(())
}

pub(crate) fn up(config_path: &Path) -> Result<()> {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    let config = load_config(config_path)?;
    let order = topological_order(&config)?;
    let paths = world_paths(config_path)?;
    let _world_lock = WorldLock::acquire(&paths)?;
    let smolvm = smolvm_program();
    preflight(&config, &paths, &smolvm)?;

    let recovery = inspect_recovery(&paths)?;
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
    let previous = load_state(&paths.state_file)?;
    cleanup_machines(&smolvm, previous.as_ref());
    remove_stale_temporary_files(&paths)?;
    remove_runtime_dir(&paths.runtime_dir)?;

    let state = allocate_state(previous, &config, &paths)?;
    write_state(&paths, &state)?;
    mark_starting(&paths)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let (switch_tx, switch_rx) = mpsc::channel();
    let (attached_tx, attached_rx) = mpsc::channel();
    let gateway = Gateway::new(&config, &state);
    let mut port_handles = Vec::new();
    let mut socket_paths = BTreeMap::new();
    let mut switch_handle = None;
    let result = (|| {
        prepare_runtime_dir(&paths.runtime_dir)?;
        let switch_shutdown = shutdown.clone();
        switch_handle = Some(
            thread::Builder::new()
                .name("smolworld-switch".into())
                .spawn(move || run_switch(switch_rx, gateway, switch_shutdown))
                .map_err(|error| format!("start switch: {error}"))?,
        );

        for name in config.machines.keys() {
            let socket_path = port_socket_path(&paths, name);
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

        for name in &order {
            let machine = config
                .machines
                .get(name)
                .expect("topological order only contains configured machines");
            let assignment = state.assignments.get(name).expect("allocated machine");
            let image = local_image_path(&paths.config_dir, &machine.image)?;
            create_machine(
                &smolvm,
                MachineLaunch {
                    assignment,
                    socket: socket_paths.get(name).expect("socket allocated"),
                    image: &image,
                    command: &machine.command,
                    resources: machine.resources,
                },
                &config.network,
            )?;
        }
        mark_created(&paths)?;
        for name in &order {
            start_machine(
                &smolvm,
                &state
                    .assignments
                    .get(name)
                    .expect("allocated machine")
                    .smolvm_name,
            )?;
        }

        wait_for_attachments(&attached_rx, &config)?;
        mark_attached(&paths)?;
        print_allocations(&config, &state);
        mark_running(&paths)?;
        install_signal_handlers();
        eprintln!("smolworld: world is up; press Ctrl-C to stop it");
        while !STOP_REQUESTED.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    })();

    cleanup_machines(&smolvm, Some(&state));
    shutdown.store(true, Ordering::SeqCst);
    let _ = switch_tx.send(SwitchEvent::Shutdown);
    for handle in port_handles {
        let _ = handle.join();
    }
    if let Some(handle) = switch_handle {
        let _ = handle.join();
    }
    let _ = remove_runtime_dir(&paths.runtime_dir);
    let _ = mark_absent(&paths);
    result
}

pub(crate) fn down(config_path: &Path) -> Result<()> {
    let paths = world_paths(config_path)?;
    let _world_lock = WorldLock::acquire(&paths)?;
    let state = load_state(&paths.state_file)?;
    if let Some(state) = &state {
        cleanup_machines(&smolvm_program(), Some(state));
    }
    remove_stale_temporary_files(&paths)?;
    remove_runtime_dir(&paths.runtime_dir)?;
    if state.is_some() {
        mark_absent(&paths)?;
    }
    println!("smolworld: down");
    Ok(())
}

pub(crate) fn ps(config_path: &Path, format: PsFormat) -> Result<()> {
    let config = load_config(config_path)?;
    let paths = world_paths(config_path)?;
    let state = load_state(&paths.state_file)?;
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
    if smolvm_state != "running" {
        return DisplayLifecycleState::Created;
    }
    match lifecycle {
        LifecycleState::Attached => DisplayLifecycleState::Attached,
        LifecycleState::Running => DisplayLifecycleState::Running,
        _ => DisplayLifecycleState::Created,
    }
}

pub(crate) fn exec(config_path: &Path, machine: &str, command: &[String]) -> Result<()> {
    let config = load_config(config_path)?;
    if !config.machines.contains_key(machine) {
        return Err(format!("unknown world machine '{machine}'"));
    }
    let paths = world_paths(config_path)?;
    let state = load_state(&paths.state_file)?
        .ok_or_else(|| "world has no state; run `smolworld up` first".to_string())?;
    let assignment = state
        .assignments
        .get(machine)
        .ok_or_else(|| format!("machine '{machine}' has no allocation"))?;
    let status = Command::new(smolvm_program())
        .arg("machine")
        .arg("exec")
        .arg("--name")
        .arg(&assignment.smolvm_name)
        .arg("--")
        .args(command)
        .status()
        .map_err(|error| format!("run smolvm machine exec: {error}"))?;
    status_result("smolvm machine exec", status)
}
