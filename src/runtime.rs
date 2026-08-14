use crate::cli::{
    format_ps, Cli, LifecycleState as DisplayLifecycleState, MachineStatus, PsFormat,
};
use crate::config::{load_config, topological_order, topological_waves};
use crate::gateway::Gateway;
use crate::model::{format_mac, Assignment, LifecycleState, MachineLaunch, SeedFile, WorldConfig};
use crate::smolvm::{
    cleanup_machines, create_machine, materialize_external_world, preflight, smolvm_program,
    start_machine, status_result, validate_external_world,
};
use crate::state::{
    allocate_v2_state, digest_file, inspect_v2_recovery, load_v2_lifecycle, load_v2_material_lock,
    load_v2_state, mark_v2_absent, mark_v2_attached, mark_v2_created, mark_v2_running,
    mark_v2_starting, material_lock_resolver_abi, normalize_relative_path, prepare_v2_runtime_dir,
    remove_v2_runtime_dir, remove_v2_stale_temporary_files, v2_world_paths, write_v2_material_lock,
    write_v2_state, V2ImageMaterial, V2MaterialLock, V2SeedObservation, V2SmolfileObservation,
    V2WorldPaths, WorldLock,
};
use crate::switch::{
    port_socket_path, print_allocations, run_switch, spawn_port_acceptor, wait_for_attachments,
    SwitchEvent,
};
use crate::Result;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Component, Path, PathBuf};
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
        Cli::Prepare { config } => prepare(&config),
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

pub(crate) fn up(config_path: &Path) -> Result<()> {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    let config = load_config(config_path)?;
    let waves = topological_waves(&config)?;
    let paths = v2_world_paths(config_path)?;
    let smolvm = smolvm_program();
    let _world_lock = WorldLock::acquire_v2(&paths)?;
    let material = verify_prepared_world(&config, &paths, &smolvm)?;

    let recovery = inspect_v2_recovery(&paths)?;
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
    let previous = load_v2_state(&paths.state_file)?;
    cleanup_machines(&smolvm, previous.as_ref());
    remove_v2_stale_temporary_files(&paths)?;
    remove_v2_runtime_dir(&paths)?;

    let state = allocate_v2_state(previous, &config, &paths)?;
    write_v2_state(&paths, &state)?;
    mark_v2_starting(&paths)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let (switch_tx, switch_rx) = mpsc::channel();
    let (attached_tx, attached_rx) = mpsc::channel();
    let gateway = Gateway::new(&config, &state);
    let mut port_handles = Vec::new();
    let mut socket_paths = BTreeMap::new();
    let mut switch_handle = None;
    let result = (|| {
        prepare_v2_runtime_dir(&paths)?;
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
    let _ = remove_v2_runtime_dir(&paths);
    let _ = mark_v2_absent(&paths);
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
    let paths = v2_world_paths(config_path)?;
    let _world_lock = WorldLock::acquire_v2(&paths)?;
    let state = load_v2_state(&paths.state_file)?;
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
    let state = load_v2_state(&paths.state_file)?;
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
    let paths = v2_world_paths(config_path)?;
    let state = load_v2_state(&paths.state_file)?
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

/// Copy one regular host file to or from exactly one recorded world machine.
/// This is deliberately a namespaced command delegation, not a filesystem
/// sharing mechanism: the smolvm name is resolved only from this world's
/// durable allocation state and is never exposed to callers.
pub(crate) fn copy(config_path: &Path, source: &str, destination: &str) -> Result<()> {
    let config = load_config(config_path)?;
    let paths = v2_world_paths(config_path)?;
    let state = load_v2_state(&paths.state_file)?
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
}
