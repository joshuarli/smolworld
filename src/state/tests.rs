use super::*;
use crate::model::{
    ArtifactState, Assignment, ImageSourceKind, LifecycleState, MachineCheckpointReceipt,
    MachineConfig, NetworkConfig, SwitchCheckpointReceipt, WorldAllocationState,
    WorldCheckpointReceipt, WorldConfig, WORLD_CHECKPOINT_RECEIPT_VERSION,
};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_WORLD_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TemporaryWorld {
    root: PathBuf,
}

impl TemporaryWorld {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let serial = TEMP_WORLD_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!(
            "smolworld-state-test-{}-{nonce}-{serial}",
            std::process::id(),
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn config_path(&self) -> PathBuf {
        self.root.join("demo/.smolworld")
    }

    fn legacy_state_dir(&self) -> PathBuf {
        self.root.join("home/.smolworld/v2/world-2a")
    }

    fn legacy_state_file(&self) -> PathBuf {
        self.legacy_state_dir().join("state")
    }

    fn legacy_lifecycle_file(&self) -> PathBuf {
        self.legacy_state_dir().join("lifecycle")
    }
}

impl Drop for TemporaryWorld {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn paths_for(world: &TemporaryWorld) -> WorldPaths {
    let state_dir = world.root.join("home/.smolworld/world-2a");
    WorldPaths {
        canonical_config: world.config_path(),
        config_dir: world.root.join("demo"),
        hash: 42,
        state_file: state_dir.join("state"),
        state_dir,
        runtime_dir: world.root.join("runtime"),
    }
}

fn material_lock() -> MaterialLock {
    MaterialLock {
        resolver_abi: material_lock_resolver_abi().to_string(),
        world: WorldIdentity {
            config_digest: digest_bytes(b"world: sentry-backend\n"),
        },
        smolfiles: BTreeMap::from([
            (
                "postgres".to_string(),
                SmolfileObservation {
                    authored_relative_path: PathBuf::from("smol/postgres.Smolfile"),
                    authored_digest: digest_bytes(b"image = \"postgres\"\n"),
                    prepared_path: PathBuf::from("/tmp/smolworld/prepared/postgres.Smolfile"),
                    prepared_digest: digest_bytes(b"image = \"/tmp/postgres.tar\"\n"),
                },
            ),
            (
                "runner".to_string(),
                SmolfileObservation {
                    authored_relative_path: PathBuf::from("smol/runner.Smolfile"),
                    authored_digest: digest_bytes(b"image = \"runner\"\n"),
                    prepared_path: PathBuf::from("/tmp/smolworld/prepared/runner.Smolfile"),
                    prepared_digest: digest_bytes(b"image = \"/tmp/runner.tar\"\n"),
                },
            ),
        ]),
        seeds: vec![SeedObservation {
            machine: "clickhouse".to_string(),
            source_relative_path: PathBuf::from("assets/clickhouse.xml"),
            destination: "/etc/clickhouse-server/config.d/world.xml".to_string(),
            mode: 0o644,
            digest: digest_bytes(b"<clickhouse/>\n"),
        }],
        images: BTreeMap::from([(
            "postgres".to_string(),
            ImageMaterial {
                machine: "postgres".to_string(),
                source_kind: ImageSourceKind::Registry,
                source_reference: "docker.io/library/postgres@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                source_digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                local_path: PathBuf::from("/tmp/smolworld/material/postgres.ext4"),
                image_digest: "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            },
        )]),
    }
}

#[test]
fn material_lock_round_trips_all_material_identity() {
    let world = TemporaryWorld::new();
    fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
    fs::write(world.config_path(), b"format: 2\n").unwrap();
    let mut paths = paths_for(&world);
    paths.canonical_config = fs::canonicalize(world.config_path()).unwrap();
    paths.state_file = paths.state_dir.join("state");
    let record = material_lock();
    write_material_lock(&paths, &record).unwrap();

    let serialized = fs::read_to_string(paths.material_lock_path()).unwrap();
    assert!(serialized.starts_with("version\t5\nresolver_abi\tsmolvm-external-world/v3\n"));
    assert!(!serialized.contains(&world.root.display().to_string()));
    assert!(!paths.state_dir.exists());
    assert_eq!(
        load_material_lock(&paths.material_lock_path()).unwrap(),
        Some(record)
    );
}

#[test]
fn world_checkpoint_receipt_round_trips_stable_world_identity() {
    let world = TemporaryWorld::new();
    let checkpoint = world.root.join("checkpoint");
    fs::create_dir(&checkpoint).unwrap();
    let receipt = WorldCheckpointReceipt {
        schema_version: WORLD_CHECKPOINT_RECEIPT_VERSION,
        world_name: "sentry".to_string(),
        config_digest: digest_bytes(b"world config"),
        material_lock_digest: digest_bytes(b"prepared material"),
        allocation: WorldAllocationState {
            seed: 0x1234,
            assignments: BTreeMap::from([(
                "runner".to_string(),
                Assignment {
                    ip: "10.89.0.2".parse().unwrap(),
                    mac: [0x02, 0, 0, 0, 0, 2],
                    smolvm_name: "smw-00000000002a-runner".to_string(),
                },
            )]),
        },
        machine_receipts: BTreeMap::from([(
            "runner".to_string(),
            MachineCheckpointReceipt {
                digest: digest_bytes(b"smolvm machine receipt"),
            },
        )]),
        switch: SwitchCheckpointReceipt {
            epoch: 7,
            queued_frames: 0,
            active_ports: BTreeMap::from([("runner".to_string(), 3)]),
            learned_macs: BTreeMap::from([("02:00:00:00:00:02".to_string(), "runner".to_string())]),
        },
    };

    write_world_checkpoint_receipt(&checkpoint, &receipt).unwrap();

    assert_eq!(load_world_checkpoint_receipt(&checkpoint).unwrap(), receipt);
    let serialized = fs::read_to_string(world_checkpoint_receipt_path(&checkpoint)).unwrap();
    assert!(serialized.starts_with("version\t2\nworld\tsentry\n"));
    assert!(serialized.contains("machine-receipt\trunner\tblake3:"));

    fs::write(
        world_checkpoint_receipt_path(&checkpoint),
        serialized.replacen("version\t2", "version\t1", 1),
    )
    .unwrap();
    assert!(load_world_checkpoint_receipt(&checkpoint)
        .unwrap_err()
        .contains("not version 2"));
}

#[test]
fn machine_checkpoint_receipt_digest_is_bounded_and_rejects_symlinks() {
    let world = TemporaryWorld::new();
    let receipt = world.root.join(MACHINE_CHECKPOINT_RECEIPT_NAME);
    fs::write(&receipt, b"{}\n").unwrap();
    assert_eq!(
        digest_machine_checkpoint_receipt(&receipt).unwrap(),
        digest_bytes(b"{}\n")
    );

    let link = world.root.join("receipt-link");
    std::os::unix::fs::symlink(&receipt, &link).unwrap();
    assert!(digest_machine_checkpoint_receipt(&link)
        .unwrap_err()
        .contains("not a regular file"));

    fs::write(
        &receipt,
        vec![0_u8; (MAX_MACHINE_CHECKPOINT_RECEIPT_BYTES + 1) as usize],
    )
    .unwrap();
    assert!(digest_machine_checkpoint_receipt(&receipt)
        .unwrap_err()
        .contains("larger than"));
}

#[test]
fn world_paths_do_not_adopt_legacy_state() {
    let world = TemporaryWorld::new();
    fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
    fs::write(world.config_path(), b"format: 2\n").unwrap();
    let canonical_config = fs::canonicalize(world.config_path()).unwrap();
    ensure_private_dir(&world.legacy_state_dir()).unwrap();
    fs::write(
        world.legacy_state_file(),
        b"legacy allocation remains untouched\n",
    )
    .unwrap();
    let mut paths = paths_for(&world);
    paths.canonical_config = canonical_config;

    assert_ne!(world.legacy_state_dir(), paths.state_dir);
    assert_eq!(
        paths.state_dir.parent().unwrap().file_name().unwrap(),
        ".smolworld"
    );
    assert_eq!(
        load_material_lock(&paths.material_lock_path()).unwrap(),
        None
    );
    let record = material_lock();
    write_material_lock(&paths, &record).unwrap();
    assert_eq!(
        fs::read(world.legacy_state_file()).unwrap(),
        b"legacy allocation remains untouched\n"
    );
    assert!(world.legacy_state_file().exists());
    assert!(!paths.state_dir.exists());
}

#[test]
fn state_round_trips_with_explicit_version() {
    let world = TemporaryWorld::new();
    let paths = paths_for(&world);
    let state = WorldAllocationState {
        seed: 0xfeed,
        assignments: BTreeMap::from([(
            "redis".to_string(),
            Assignment {
                ip: Ipv4Addr::new(10, 89, 0, 2),
                mac: [0x02, 1, 2, 3, 4, 5],
                smolvm_name: "smw-redis".to_string(),
            },
        )]),
    };

    write_allocation_state(&paths, &state).unwrap();
    assert_eq!(
        load_allocation_state(&paths.state_file).unwrap(),
        Some(state.clone())
    );
    assert_eq!(
        fs::read_to_string(&paths.state_file)
            .unwrap()
            .lines()
            .next(),
        Some("version\t2")
    );
}

#[test]
fn state_rejects_duplicate_scalars_and_unsafe_allocations() {
    let world = TemporaryWorld::new();
    let paths = paths_for(&world);
    fs::create_dir_all(&paths.state_dir).unwrap();
    let state = |body: &str| {
        fs::write(&paths.state_file, body).unwrap();
        load_allocation_state(&paths.state_file).expect_err("tampered world state must fail closed")
    };

    assert!(state("version\t2\nversion\t2\nseed\t0000000000000001\n").contains("repeats version"));
    assert!(
        state("version\t2\nseed\t0000000000000001\nseed\t0000000000000002\n")
            .contains("repeats seed")
    );
    assert!(state(concat!(
        "version\t2\nseed\t0000000000000001\n",
        "machine\tapi\t10.89.0.2\t02:00:00:00:00:02\tsmw-demo-api\n",
        "machine\tworker\t10.89.0.2\t02:00:00:00:00:03\tsmw-demo-worker\n",
    ))
    .contains("unsafe or repeated allocation"));
    assert!(state(concat!(
        "version\t2\nseed\t0000000000000001\n",
        "machine\tapi\t10.89.0.2\t00:00:00:00:00:02\tsmw-demo-api\n",
    ))
    .contains("unsafe or repeated allocation"));
    assert!(state(concat!(
        "version\t2\nseed\t0000000000000001\n",
        "machine\tapi\t10.89.0.2\t02:00:00:00:00:02\tnot-a-world-machine\n",
    ))
    .contains("unsafe or repeated allocation"));
}

#[test]
fn allocation_is_stable_reserved_and_namespaced() {
    let world = TemporaryWorld::new();
    let paths = paths_for(&world);
    let config = WorldConfig {
        name: "demo".to_string(),
        network: NetworkConfig {
            subnet: [10, 89, 0, 0],
            gateway: "10.89.0.9".parse().unwrap(),
            dns: "10.89.0.9".parse().unwrap(),
            domain: "demo.test".to_string(),
            egress: false,
        },
        machines: BTreeMap::from([
            (
                "redis".to_string(),
                MachineConfig {
                    smolfile: PathBuf::from("redis.Smolfile"),
                    depends_on: Vec::new(),
                    seed_files: Vec::new(),
                },
            ),
            (
                "client".to_string(),
                MachineConfig {
                    smolfile: PathBuf::from("client.Smolfile"),
                    depends_on: vec!["redis".to_string()],
                    seed_files: Vec::new(),
                },
            ),
        ]),
    };
    let first = allocate_allocation_state(
        Some(WorldAllocationState {
            seed: 7,
            assignments: BTreeMap::new(),
        }),
        &config,
        &paths,
    )
    .unwrap();
    let second = allocate_allocation_state(Some(first.clone()), &config, &paths).unwrap();

    assert_eq!(first, second);
    assert!(first
        .assignments
        .values()
        .all(|assignment| assignment.ip != config.network.gateway));
    assert!(first
        .assignments
        .values()
        .all(|assignment| assignment.smolvm_name.starts_with("smw-")));
    assert_ne!(
        first.assignments["redis"].ip,
        first.assignments["client"].ip
    );
}

#[test]
fn lifecycle_and_recovery_never_adopt_legacy_files() {
    let world = TemporaryWorld::new();
    let paths = paths_for(&world);
    ensure_private_dir(&world.legacy_state_dir()).unwrap();
    fs::write(
        world.legacy_state_file(),
        b"version\t1\nseed\t000000000000000b\n",
    )
    .unwrap();
    fs::write(
        world.legacy_lifecycle_file(),
        b"version\t1\nstate\tstarting\nowner_pid\t-\ngeneration\t0000000000000001\n",
    )
    .unwrap();

    assert_eq!(load_allocation_state(&paths.state_file).unwrap(), None);
    assert_eq!(load_lifecycle(&paths.lifecycle_path()).unwrap(), None);
    let absent = inspect_recovery(&paths).unwrap();
    assert_eq!(absent.state_file, ArtifactState::Missing);
    assert_eq!(absent.lifecycle_file, ArtifactState::Missing);
    assert_eq!(absent.runtime_dir, ArtifactState::Missing);
    assert!(!absent.needs_recovery());

    let lifecycle = mark_starting(&paths).unwrap();
    assert_eq!(lifecycle.state, LifecycleState::Starting);
    assert_eq!(
        fs::read_to_string(paths.lifecycle_path())
            .unwrap()
            .lines()
            .next(),
        Some("version\t2")
    );
    write_allocation_state(
        &paths,
        &WorldAllocationState {
            seed: 12,
            assignments: BTreeMap::new(),
        },
    )
    .unwrap();
    assert_eq!(
        load_allocation_state(&paths.state_file)
            .unwrap()
            .unwrap()
            .seed,
        12
    );
    assert!(inspect_recovery(&paths).unwrap().needs_recovery());

    mark_absent(&paths).unwrap();
    assert!(!inspect_recovery(&paths).unwrap().needs_recovery());
    assert!(world.legacy_state_file().exists());
    assert!(world.legacy_lifecycle_file().exists());
}

#[test]
fn capture_intent_prevents_stale_world_cleanup_until_rollback_or_commit() {
    let world = TemporaryWorld::new();
    let paths = paths_for(&world);

    mark_starting(&paths).unwrap();
    mark_created(&paths).unwrap();
    mark_attached(&paths).unwrap();
    mark_running(&paths).unwrap();
    let capturing = mark_capturing(&paths).unwrap();
    assert_eq!(capturing.state, LifecycleState::Capturing);
    assert!(capturing.state.retains_checkpoint_sources());
    assert!(!capturing.state.needs_recovery());

    let rolled_back = mark_capture_rolled_back(&paths).unwrap();
    assert_eq!(rolled_back.state, LifecycleState::Running);
    mark_capturing(&paths).unwrap();
    let captured = mark_captured(&paths).unwrap();
    assert_eq!(captured.state, LifecycleState::Captured);
    assert!(captured.state.retains_checkpoint_sources());
}

#[test]
fn restored_world_can_attach_without_a_synthetic_create_transition() {
    let world = TemporaryWorld::new();
    let paths = paths_for(&world);

    mark_starting(&paths).unwrap();
    let attached = mark_attached(&paths).unwrap();
    assert_eq!(attached.state, LifecycleState::Attached);
    assert_eq!(mark_running(&paths).unwrap().state, LifecycleState::Running);
}

#[test]
fn cleanup_is_scoped_to_world_runtime_and_temporary_files() {
    let world = TemporaryWorld::new();
    let paths = paths_for(&world);
    ensure_private_dir(&world.legacy_state_dir()).unwrap();
    ensure_private_dir(&paths.state_dir).unwrap();
    fs::write(paths.state_dir.join("state.123.tmp"), b"world temporary").unwrap();
    fs::write(
        world.legacy_state_dir().join("state.123.tmp"),
        b"legacy temporary",
    )
    .unwrap();
    prepare_runtime_dir(&paths).unwrap();
    fs::write(paths.runtime_dir.join("owned"), b"world").unwrap();

    assert_eq!(remove_stale_temporary_files(&paths).unwrap(), 1);
    assert!(world.legacy_state_dir().join("state.123.tmp").exists());
    assert_eq!(remove_runtime_dir(&paths), Ok(()));
    assert!(!paths.runtime_dir.exists());
    assert!(world.legacy_state_dir().exists());
}

#[test]
fn material_lock_requires_absolute_seed_destinations_and_matching_image_keys() {
    let mut record = material_lock();
    record.seeds[0].destination = "relative/path".to_string();
    assert!(record.validate().is_err());

    let mut record = material_lock();
    record.images.get_mut("postgres").unwrap().machine = "runner".to_string();
    assert!(record.validate().is_err());
}

#[test]
fn material_lock_keeps_oci_and_local_digest_algorithms_separate() {
    let mut registry = material_lock();
    registry.images.get_mut("postgres").unwrap().source_digest =
        "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    assert!(registry.validate().is_err());

    let mut local = material_lock();
    let material = local.images.get_mut("postgres").unwrap();
    material.source_kind = ImageSourceKind::LocalArchive;
    material.source_reference = "/tmp/postgres.tar".to_string();
    material.source_digest =
        "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    assert!(local.validate().is_ok());

    local.images.get_mut("postgres").unwrap().image_digest =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
    assert!(local.validate().is_err());
}

#[test]
fn identity_from_config_records_portable_content_digest() {
    let world = TemporaryWorld::new();
    fs::create_dir_all(world.config_path().parent().unwrap()).unwrap();
    fs::write(world.config_path(), b"format: 2\n").unwrap();
    let record =
        MaterialLock::from_config(&world.config_path(), material_lock_resolver_abi()).unwrap();
    assert_eq!(record.world.config_digest, digest_bytes(b"format: 2\n"));
}

#[test]
fn material_digest_uses_blake3() {
    assert_eq!(
        digest_bytes(b""),
        "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
}
