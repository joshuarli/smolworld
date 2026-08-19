use super::{parse_mac, validate_blake3_digest};
use crate::config::validate_label;
use crate::model::{
    format_mac, Assignment, MachineCheckpointReceipt, SwitchCheckpointReceipt,
    WorldAllocationState, WorldCheckpointReceipt, WORLD_CHECKPOINT_RECEIPT_VERSION,
};
use crate::Result;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const WORLD_CHECKPOINT_RECEIPT_NAME: &str = "smolworld-checkpoint";

/// Receipt filename inside a published world checkpoint directory.
pub(crate) fn world_checkpoint_receipt_path(root: &Path) -> std::path::PathBuf {
    root.join(WORLD_CHECKPOINT_RECEIPT_NAME)
}

/// Atomically publish the world-level receipt after every per-machine
/// checkpoint directory is complete. The receipt intentionally records the
/// stable allocation separately from the guest checkpoint files; it is a
/// verifier and ownership record, never a substitute for RAM/device state.
pub(crate) fn write_world_checkpoint_receipt(
    root: &Path,
    receipt: &WorldCheckpointReceipt,
) -> Result<()> {
    validate_world_checkpoint_receipt(receipt)?;
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("inspect checkpoint root {}: {error}", root.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "checkpoint root is not a real directory: {}",
            root.display()
        ));
    }
    let destination = world_checkpoint_receipt_path(root);
    let temporary = root.join(format!(
        ".{WORLD_CHECKPOINT_RECEIPT_NAME}.{}.tmp",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("create {}: {error}", temporary.display()))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod {}: {error}", temporary.display()))?;
    file.write_all(serialize_world_checkpoint_receipt(receipt).as_bytes())
        .map_err(|error| format!("write world checkpoint receipt: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync world checkpoint receipt: {error}"))?;
    fs::rename(&temporary, &destination)
        .map_err(|error| format!("rename {}: {error}", destination.display()))?;
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync checkpoint root {}: {error}", root.display()))?;
    Ok(())
}

/// Read and validate one immutable world checkpoint receipt. Callers still
/// verify every referenced per-machine SmolVM receipt before restoring it.
pub(crate) fn load_world_checkpoint_receipt(root: &Path) -> Result<WorldCheckpointReceipt> {
    let path = world_checkpoint_receipt_path(root);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        format!(
            "inspect world checkpoint receipt {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "world checkpoint receipt is not a regular file: {}",
            path.display()
        ));
    }
    let content = fs::read_to_string(&path)
        .map_err(|error| format!("read world checkpoint receipt {}: {error}", path.display()))?;
    parse_world_checkpoint_receipt(&content)
}

fn validate_world_checkpoint_receipt(receipt: &WorldCheckpointReceipt) -> Result<()> {
    if receipt.schema_version != WORLD_CHECKPOINT_RECEIPT_VERSION {
        return Err(format!(
            "world checkpoint receipt schema {} is not supported; expected {}",
            receipt.schema_version, WORLD_CHECKPOINT_RECEIPT_VERSION
        ));
    }
    validate_label(&receipt.world_name).map_err(|reason| {
        format!(
            "world checkpoint receipt world '{}': {reason}",
            receipt.world_name
        )
    })?;
    validate_blake3_digest(&receipt.config_digest, "world checkpoint config digest")?;
    validate_blake3_digest(
        &receipt.material_identity_digest,
        "world checkpoint material identity digest",
    )?;
    if receipt.allocation.assignments.is_empty() {
        return Err("world checkpoint receipt has no machine allocations".into());
    }
    let mut ips = HashSet::new();
    let mut macs = HashSet::new();
    for (machine, assignment) in &receipt.allocation.assignments {
        validate_label(machine)
            .map_err(|reason| format!("world checkpoint machine '{machine}': {reason}"))?;
        if assignment.smolvm_name.is_empty()
            || assignment.smolvm_name.contains(['\t', '\r', '\n'])
            || !ips.insert(assignment.ip)
            || !macs.insert(assignment.mac)
        {
            return Err(format!(
                "world checkpoint receipt has invalid or repeated allocation for '{machine}'"
            ));
        }
    }
    if receipt
        .machine_receipts
        .keys()
        .ne(receipt.allocation.assignments.keys())
    {
        return Err(
            "world checkpoint receipt machine receipt set does not match allocations".into(),
        );
    }
    for (machine, machine_receipt) in &receipt.machine_receipts {
        validate_label(machine)
            .map_err(|reason| format!("world checkpoint machine receipt '{machine}': {reason}"))?;
        validate_blake3_digest(
            &machine_receipt.digest,
            &format!("world checkpoint machine receipt '{machine}' digest"),
        )?;
    }
    if receipt.switch.queued_frames != 0 {
        return Err("world checkpoint receipt cannot retain switch packet queues".into());
    }
    for (port, connection) in &receipt.switch.active_ports {
        validate_label(port)
            .map_err(|reason| format!("world checkpoint switch port '{port}': {reason}"))?;
        if *connection == 0 {
            return Err(format!(
                "world checkpoint switch port '{port}' has invalid connection"
            ));
        }
    }
    for (mac, port) in &receipt.switch.learned_macs {
        parse_mac(mac)
            .map_err(|reason| format!("world checkpoint switch FDB MAC '{mac}': {reason}"))?;
        validate_label(port)
            .map_err(|reason| format!("world checkpoint switch FDB port '{port}': {reason}"))?;
        if !receipt.switch.active_ports.contains_key(port) {
            return Err(format!(
                "world checkpoint receipt switch FDB references inactive port '{port}'"
            ));
        }
    }
    Ok(())
}

fn serialize_world_checkpoint_receipt(receipt: &WorldCheckpointReceipt) -> String {
    let mut output = String::new();
    output.push_str(&format!("version\t{WORLD_CHECKPOINT_RECEIPT_VERSION}\n"));
    output.push_str(&format!("world\t{}\n", receipt.world_name));
    output.push_str(&format!("config\t{}\n", receipt.config_digest));
    output.push_str(&format!(
        "material-identity\t{}\n",
        receipt.material_identity_digest
    ));
    output.push_str(&format!("seed\t{:016x}\n", receipt.allocation.seed));
    output.push_str(&format!("switch-epoch\t{}\n", receipt.switch.epoch));
    output.push_str(&format!("switch-queue\t{}\n", receipt.switch.queued_frames));
    for (port, connection) in &receipt.switch.active_ports {
        output.push_str(&format!("switch-port\t{port}\t{connection}\n"));
    }
    for (mac, port) in &receipt.switch.learned_macs {
        output.push_str(&format!("switch-fdb\t{mac}\t{port}\n"));
    }
    for (machine, assignment) in &receipt.allocation.assignments {
        output.push_str(&format!(
            "machine\t{machine}\t{}\t{}\t{}\n",
            assignment.ip,
            format_mac(assignment.mac),
            assignment.smolvm_name
        ));
    }
    for (machine, machine_receipt) in &receipt.machine_receipts {
        output.push_str(&format!(
            "machine-receipt\t{machine}\t{}\n",
            machine_receipt.digest
        ));
    }
    output
}

fn parse_world_checkpoint_receipt(content: &str) -> Result<WorldCheckpointReceipt> {
    let mut version = None;
    let mut world_name = None;
    let mut config_digest = None;
    let mut material_identity_digest = None;
    let mut seed = None;
    let mut switch_epoch = None;
    let mut switch_queue = None;
    let mut active_ports = BTreeMap::new();
    let mut learned_macs = BTreeMap::new();
    let mut assignments = BTreeMap::new();
    let mut machine_receipts = BTreeMap::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<_> = line.split('\t').collect();
        match fields.as_slice() {
            ["version", value] if version.is_none() => {
                version = Some(
                    value
                        .parse::<u8>()
                        .map_err(|_| "world checkpoint receipt has invalid version".to_string())?,
                );
            }
            ["world", value] if world_name.is_none() => world_name = Some((*value).to_string()),
            ["config", value] if config_digest.is_none() => {
                config_digest = Some((*value).to_string())
            }
            ["material-identity", value] if material_identity_digest.is_none() => {
                material_identity_digest = Some((*value).to_string())
            }
            ["seed", value] if seed.is_none() => {
                seed = Some(
                    u64::from_str_radix(value, 16)
                        .map_err(|_| "world checkpoint receipt has invalid seed".to_string())?,
                );
            }
            ["switch-epoch", value] if switch_epoch.is_none() => {
                switch_epoch = Some(value.parse::<u64>().map_err(|_| {
                    "world checkpoint receipt has invalid switch epoch".to_string()
                })?);
            }
            ["switch-queue", value] if switch_queue.is_none() => {
                switch_queue = Some(value.parse::<u64>().map_err(|_| {
                    "world checkpoint receipt has invalid switch queue count".to_string()
                })?);
            }
            ["switch-port", port, connection] => {
                validate_label(port)
                    .map_err(|reason| format!("world checkpoint switch port '{port}': {reason}"))?;
                let connection = connection.parse::<u64>().map_err(|_| {
                    format!("world checkpoint switch port '{port}' has invalid connection")
                })?;
                if connection == 0
                    || active_ports
                        .insert((*port).to_string(), connection)
                        .is_some()
                {
                    return Err(format!(
                        "world checkpoint receipt repeats or invalidates switch port '{port}'"
                    ));
                }
            }
            ["switch-fdb", mac, port] => {
                parse_mac(mac).map_err(|reason| {
                    format!("world checkpoint switch FDB MAC '{mac}': {reason}")
                })?;
                validate_label(port).map_err(|reason| {
                    format!("world checkpoint switch FDB port '{port}': {reason}")
                })?;
                if learned_macs
                    .insert((*mac).to_string(), (*port).to_string())
                    .is_some()
                {
                    return Err(format!(
                        "world checkpoint receipt repeats switch FDB MAC '{mac}'"
                    ));
                }
            }
            ["machine", machine, ip, mac, smolvm_name] => {
                validate_label(machine).map_err(|reason| {
                    format!("world checkpoint receipt machine '{machine}': {reason}")
                })?;
                if assignments
                    .insert(
                        (*machine).to_string(),
                        Assignment {
                            ip: ip.parse().map_err(|_| {
                                format!(
                                    "world checkpoint receipt machine '{machine}' has invalid IP"
                                )
                            })?,
                            mac: parse_mac(mac).map_err(|reason| {
                                format!("world checkpoint receipt machine '{machine}': {reason}")
                            })?,
                            smolvm_name: (*smolvm_name).to_string(),
                        },
                    )
                    .is_some()
                {
                    return Err(format!(
                        "world checkpoint receipt repeats machine '{machine}'"
                    ));
                }
            }
            ["machine-receipt", machine, digest] => {
                validate_label(machine).map_err(|reason| {
                    format!("world checkpoint receipt machine '{machine}': {reason}")
                })?;
                if machine_receipts
                    .insert(
                        (*machine).to_string(),
                        MachineCheckpointReceipt {
                            digest: (*digest).to_string(),
                        },
                    )
                    .is_some()
                {
                    return Err(format!(
                        "world checkpoint receipt repeats machine receipt '{machine}'"
                    ));
                }
            }
            _ => {
                return Err("world checkpoint receipt contains an unknown or malformed line".into())
            }
        }
    }
    if version != Some(WORLD_CHECKPOINT_RECEIPT_VERSION) {
        return Err(format!(
            "world checkpoint receipt format is not version {WORLD_CHECKPOINT_RECEIPT_VERSION}"
        ));
    }
    let receipt = WorldCheckpointReceipt {
        schema_version: WORLD_CHECKPOINT_RECEIPT_VERSION,
        world_name: world_name
            .ok_or_else(|| "world checkpoint receipt is missing world".to_string())?,
        config_digest: config_digest
            .ok_or_else(|| "world checkpoint receipt is missing config digest".to_string())?,
        material_identity_digest: material_identity_digest.ok_or_else(|| {
            "world checkpoint receipt is missing material identity digest".to_string()
        })?,
        allocation: WorldAllocationState {
            seed: seed.ok_or_else(|| "world checkpoint receipt is missing seed".to_string())?,
            assignments,
        },
        machine_receipts,
        switch: SwitchCheckpointReceipt {
            epoch: switch_epoch
                .ok_or_else(|| "world checkpoint receipt is missing switch epoch".to_string())?,
            queued_frames: switch_queue.ok_or_else(|| {
                "world checkpoint receipt is missing switch queue count".to_string()
            })?,
            active_ports,
            learned_macs,
        },
    };
    validate_world_checkpoint_receipt(&receipt)?;
    Ok(receipt)
}
