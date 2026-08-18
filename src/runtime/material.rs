//! Sealing and read-only verification of world-owned host material.

use super::*;
use crate::world_smolfile::{prepare_world_smolfile, resolver_abi, verify_prepared_world_smolfile};

pub(super) fn verify_prepared_world(
    config: &WorldConfig,
    paths: &WorldPaths,
    smolvm: &Path,
) -> Result<MaterialLock> {
    preflight(config, &paths.config_dir, smolvm)?;
    let prepared = load_material_lock(&paths.material_lock_path())?.ok_or_else(|| {
        format!(
            "world material lock is missing at {}; run `smolworld prepare` first",
            paths.material_lock_path().display()
        )
    })?;
    verify_material_lock(config, paths, &prepared)?;
    Ok(prepared)
}

/// Resolve, download, and seal every host input that can affect a
/// Smolfile-composed world. This is the explicit mutating `prepare` boundary:
/// immutable registry sources become local archives and local-only prepared
/// Smolfiles before any allocation state or listener exists.
pub(super) fn prepare_world_material(
    config: &WorldConfig,
    paths: &WorldPaths,
    smolvm: &Path,
) -> Result<MaterialLock> {
    let mut lock = MaterialLock::from_config(&paths.canonical_config, resolver_abi())?;
    let names: Vec<_> = config.machines.keys().cloned().collect();
    let prepared = parallel_machine_map(&names, "prepare material", |name| {
        prepare_one_machine_material(config, paths, smolvm, name)
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
    smolfile: SmolfileObservation,
    image: ImageMaterial,
    seeds: Vec<SeedObservation>,
}

fn prepare_one_machine_material(
    config: &WorldConfig,
    paths: &WorldPaths,
    smolvm: &Path,
    name: &str,
) -> Result<PreparedMachineMaterial> {
    let machine = config
        .machines
        .get(name)
        .expect("prepared machine is configured");
    let authored_relative_path =
        normalize_relative_path(&machine.smolfile, "configured Smolfile path")?;
    let authored_smolfile =
        sealed_relative_file(&paths.config_dir, &authored_relative_path, "Smolfile")?;
    let preparation = prepare_world_smolfile(smolvm, paths, &authored_smolfile)?;
    let authored_digest = digest_file(&preparation.authored_smolfile)?;
    let prepared_digest = digest_file(&preparation.prepared_smolfile)?;
    let seeds = machine
        .seed_files
        .iter()
        .map(|seed| {
            let source_relative_path =
                normalize_relative_path(&seed.source, "configured seed source")?;
            let source =
                sealed_relative_file(&paths.config_dir, &source_relative_path, "seed source")?;
            validate_seed_source_for_copy(&source)?;
            validate_seed_destination(&seed.destination)?;
            Ok(SeedObservation {
                machine: name.to_string(),
                source_relative_path,
                destination: seed.destination.to_string_lossy().into_owned(),
                mode: seed.mode,
                digest: digest_file(&source)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PreparedMachineMaterial {
        smolfile: SmolfileObservation {
            authored_relative_path,
            authored_digest,
            prepared_path: preparation.prepared_smolfile,
            prepared_digest,
        },
        image: ImageMaterial {
            machine: name.to_string(),
            source_kind: preparation.source_kind,
            source_reference: preparation.source_reference,
            source_digest: preparation.source_digest,
            local_path: preparation.local_archive,
            image_digest: preparation.image_digest,
        },
        seeds,
    })
}

/// Revalidate a material lock without materializing or contacting a registry.
/// `check` and `up` use only the exact local inputs sealed by `prepare`.
fn verify_material_lock(
    config: &WorldConfig,
    paths: &WorldPaths,
    prepared: &MaterialLock,
) -> Result<()> {
    prepared.validate()?;
    if prepared.resolver_abi != resolver_abi() {
        return Err(format!(
            "world material uses resolver ABI '{}', but this smolworld requires '{}'; run `smolworld prepare` again",
            prepared.resolver_abi,
            resolver_abi()
        ));
    }
    let current = MaterialLock::from_config(&paths.canonical_config, resolver_abi())?;
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
    let mut expected_seeds = parallel_machine_map(&names, "verify material", |name| {
        verify_one_machine_material(config, paths, prepared, name)
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
    paths: &WorldPaths,
    prepared: &MaterialLock,
    name: &str,
) -> Result<Vec<SeedObservation>> {
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
    let material = verify_prepared_world_smolfile(&observation.prepared_path)?;
    let image = prepared
        .images
        .get(name)
        .ok_or_else(|| format!("world material is missing the image for machine '{name}'"))?;
    if material.local_archive != image.local_path
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
            validate_seed_source_for_copy(&source)?;
            validate_seed_destination(&seed.destination)?;
            Ok(SeedObservation {
                machine: name.to_string(),
                source_relative_path,
                destination: seed.destination.to_string_lossy().into_owned(),
                mode: seed.mode,
                digest: digest_file(&source)?,
            })
        })
        .collect()
}

fn seed_identity(left: &SeedObservation, right: &SeedObservation) -> std::cmp::Ordering {
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
    canonical
        .to_str()
        .ok_or_else(|| format!("{label} {} is not valid UTF-8", canonical.display()))?;
    Ok(canonical)
}

/// The companion `machine cp` uses a namespaced `NAME:/guest/path` endpoint.
/// Keep that delimiter out of sealed host inputs at preparation time instead
/// of allowing a later launch to reinterpret a local source as a guest endpoint.
pub(super) fn validate_seed_source_for_copy(source: &Path) -> Result<()> {
    let source_text = source
        .to_str()
        .ok_or_else(|| format!("seed source {} is not valid UTF-8", source.display()))?;
    if source_text.contains(':') {
        return Err(format!(
            "seed source {} cannot contain ':' because world seed copies use smolvm machine cp endpoints",
            source.display()
        ));
    }
    Ok(())
}

pub(super) fn validate_seed_destination(destination: &Path) -> Result<()> {
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
    {
        return Err(format!(
            "seed destination {} must be a non-root normalized absolute guest path",
            destination.display()
        ));
    }
    Ok(())
}

/// Convert sealed lock observations into world-owned guest-copy inputs. The
/// lock is re-observed before this is called, so these are canonical regular
/// files whose content digests still match the prepared world.
pub(super) fn prepared_seed_files(
    config_dir: &Path,
    material: &MaterialLock,
    machine: &str,
) -> Result<Vec<SeedFile>> {
    material
        .seeds
        .iter()
        .filter(|seed| seed.machine == machine)
        .map(|seed| {
            let source =
                sealed_relative_file(config_dir, &seed.source_relative_path, "seed source")?;
            validate_seed_source_for_copy(&source)?;
            Ok(SeedFile {
                source,
                destination: PathBuf::from(&seed.destination),
                mode: seed.mode,
            })
        })
        .collect()
}
