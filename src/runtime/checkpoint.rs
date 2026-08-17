//! Coordinated checkpoint transactions and same-lineage receipt checks.

use super::*;

pub(super) fn verify_world_checkpoint_receipt(
    config: &WorldConfig,
    paths: &WorldPaths,
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

/// Freeze every machine behind one closed switch epoch, publish the per-machine
/// durable receipts beneath `output`, then publish the world receipt last.
/// Independent machine capture remains parallel; the output is all-or-nothing
/// from the caller's point of view because any pre-publication failure restores
/// every machine that did finish capture before forwarding resumes.
pub(super) fn checkpoint_running_world(
    config: &WorldConfig,
    state: &crate::model::WorldAllocationState,
    paths: &WorldPaths,
    smolvm: &Path,
    switch_tx: &mpsc::Sender<SwitchEvent>,
    attached_rx: &mpsc::Receiver<String>,
    output: &Path,
) -> Result<()> {
    let (parent, staging) = create_world_checkpoint_staging(output)?;
    if let Err(error) = mark_capturing(paths) {
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
    mark_captured(paths)?;
    Ok(())
}

fn abandon_unstarted_world_checkpoint(
    paths: &WorldPaths,
    staging: &Path,
    original_error: String,
) -> Result<()> {
    let remove = fs::remove_dir_all(staging).map_err(|error| {
        format!(
            "remove unstarted checkpoint staging {}: {error}",
            staging.display()
        )
    });
    let lifecycle = mark_capture_rolled_back(paths);
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

pub(super) fn create_world_checkpoint_staging(output: &Path) -> Result<(PathBuf, PathBuf)> {
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
    paths: &'a WorldPaths,
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
            if let Err(error) = mark_capture_rolled_back(rollback.paths) {
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
