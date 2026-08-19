//! Strict parsing and preparation of the world-owned Smolfile profile.

use crate::model::ImageSourceKind;
use crate::smolvm::materialize_registry_archive;
use crate::state::{archive_identity, digest_file, ensure_private_dir, ArchiveIdentity, WorldPaths};
use crate::Result;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const WORLD_SMOLFILE_ABI: &str = "smolworld-world-smolfile/v1";
const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// A prepared world machine declaration and its sealed image material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedWorldSmolfile {
    pub(crate) authored_smolfile: PathBuf,
    pub(crate) prepared_smolfile: PathBuf,
    pub(crate) source_kind: ImageSourceKind,
    pub(crate) source_reference: String,
    pub(crate) source_digest: String,
    pub(crate) local_archive: PathBuf,
    pub(crate) image_digest: String,
    pub(crate) archive_identity: ArchiveIdentity,
}

/// The verified local image input named by a prepared world Smolfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedWorldSmolfile {
    pub(crate) local_archive: PathBuf,
    pub(crate) archive_identity: ArchiveIdentity,
    pub(crate) image_digest: Option<String>,
}

#[derive(Debug)]
enum WorldImage {
    LocalArchive {
        reference: String,
        path: PathBuf,
        digest: String,
        identity: ArchiveIdentity,
    },
    Registry {
        reference: String,
        digest: String,
    },
}

#[derive(Debug)]
struct ParsedWorldSmolfile {
    document: toml::Value,
    image: WorldImage,
}

/// The resolver ABI is world-owned because smolworld owns this profile.
pub(crate) fn resolver_abi() -> &'static str {
    WORLD_SMOLFILE_ABI
}

/// Validate and prepare one authored world Smolfile before allocation.
pub(crate) fn prepare_world_smolfile(
    smolvm: &Path,
    paths: &WorldPaths,
    authored_smolfile: &Path,
) -> Result<PreparedWorldSmolfile> {
    let authored_smolfile = canonical_regular_file(authored_smolfile, "authored Smolfile")?;
    let parsed = parse_world_smolfile(&authored_smolfile)?;
    match parsed.image {
        WorldImage::LocalArchive {
            reference,
            path,
            digest,
            identity,
        } => Ok(PreparedWorldSmolfile {
            authored_smolfile: authored_smolfile.clone(),
            prepared_smolfile: authored_smolfile,
            source_kind: ImageSourceKind::LocalArchive,
            source_reference: reference,
            source_digest: digest.clone(),
            local_archive: path,
            image_digest: digest,
            archive_identity: identity,
        }),
        WorldImage::Registry { reference, digest } => {
            let material = materialize_registry_archive(smolvm, &reference)?;
            if material.source_reference != reference || material.source_digest != digest {
                return Err(
                    "smolvm materialized a different immutable image than the world Smolfile declared"
                        .into(),
                );
            }
            let (archive_digest, archive_identity) = audited_archive_digest(&material.archive_path)?;
            if archive_digest != material.archive_digest {
                return Err("smolvm materialized archive does not match its reported digest".into());
            }
            let prepared_smolfile = write_prepared_world_smolfile(
                paths,
                &authored_smolfile,
                parsed.document,
                &material.archive_path,
                &material.archive_digest,
            )?;
            Ok(PreparedWorldSmolfile {
                authored_smolfile,
                prepared_smolfile,
                source_kind: ImageSourceKind::Registry,
                source_reference: reference,
                source_digest: digest,
                local_archive: material.archive_path,
                image_digest: material.archive_digest,
                archive_identity,
            })
        }
    }
}

/// Re-parse a locked prepared Smolfile and prove that it still names a sealed
/// local archive. Registry references cannot appear after preparation.
pub(crate) fn verify_prepared_world_smolfile(
    path: &Path,
    deep: bool,
) -> Result<VerifiedWorldSmolfile> {
    let path = canonical_regular_file(path, "prepared world Smolfile")?;
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("read prepared world Smolfile {}: {error}", path.display()))?;
    let document: toml::Value = text
        .parse()
        .map_err(|error| format!("parse prepared world Smolfile {}: {error}", path.display()))?;
    validate_world_profile(&document)?;
    let reference = document
        .get("image")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| "prepared world Smolfile image must be a string".to_string())?;
    if !looks_local(reference) {
        return Err("prepared world Smolfile still names a registry image; run smolworld prepare again".into());
    }
    let local_archive = resolve_local_archive_path(&path, reference)?;
    let (image_digest, archive_identity) = if deep {
        let (digest, identity) = audited_archive_digest(&local_archive)?;
        (Some(digest), identity)
    } else {
        (None, archive_identity(&local_archive)?)
    };
    Ok(VerifiedWorldSmolfile {
        local_archive,
        archive_identity,
        image_digest,
    })
}

fn parse_world_smolfile(path: &Path) -> Result<ParsedWorldSmolfile> {
    let path = canonical_regular_file(path, "world Smolfile")?;
    let text = fs::read_to_string(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let document: toml::Value = text
        .parse()
        .map_err(|error| format!("parse world Smolfile {}: {error}", path.display()))?;
    validate_world_profile(&document)?;
    let image = document
        .get("image")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| "world Smolfile image must be a string".to_string())?;
    let image = resolve_world_image(&path, image)?;
    Ok(ParsedWorldSmolfile { document, image })
}

fn validate_world_profile(document: &toml::Value) -> Result<()> {
    const ALLOWED: &[&str] = &[
        "image",
        "entrypoint",
        "cmd",
        "env",
        "workdir",
        "cpus",
        "memory",
        "storage",
        "overlay",
    ];
    let table = document
        .as_table()
        .ok_or_else(|| "world Smolfile top level must be a TOML table".to_string())?;
    for key in table.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return Err(format!(
                "world Smolfile field '{key}' is not allowed; worlds permit only image, command, environment, workdir, and resources"
            ));
        }
    }
    require_string(table, "image")?;
    for key in ["entrypoint", "cmd", "env"] {
        if let Some(value) = table.get(key) {
            let values = value
                .as_array()
                .ok_or_else(|| format!("world Smolfile {key} must be an array of strings"))?;
            if values.iter().any(|value| value.as_str().is_none()) {
                return Err(format!("world Smolfile {key} must be an array of strings"));
            }
        }
    }
    if let Some(value) = table.get("workdir") {
        value
            .as_str()
            .ok_or_else(|| "world Smolfile workdir must be a string".to_string())?;
    }
    validate_positive_u8(table, "cpus", 1)?;
    validate_positive_u32(table, "memory", 64)?;
    validate_positive_u64(table, "storage")?;
    validate_positive_u64(table, "overlay")?;
    Ok(())
}

fn require_string<'a>(table: &'a toml::map::Map<String, toml::Value>, key: &str) -> Result<&'a str> {
    let value = table
        .get(key)
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("world Smolfile {key} must be a string"))?;
    if value.is_empty() {
        return Err(format!("world Smolfile {key} must not be empty"));
    }
    Ok(value)
}

fn validate_positive_u8(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    minimum: u8,
) -> Result<()> {
    let Some(value) = table.get(key) else {
        return Ok(());
    };
    let value = value
        .as_integer()
        .and_then(|value| u8::try_from(value).ok())
        .ok_or_else(|| format!("world Smolfile {key} must be an unsigned 8-bit integer"))?;
    if value < minimum {
        return Err(format!("world Smolfile {key} must be at least {minimum}"));
    }
    Ok(())
}

fn validate_positive_u32(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    minimum: u32,
) -> Result<()> {
    let Some(value) = table.get(key) else {
        return Ok(());
    };
    let value = value
        .as_integer()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| format!("world Smolfile {key} must be an unsigned 32-bit integer"))?;
    if value < minimum {
        return Err(format!("world Smolfile {key} must be at least {minimum}"));
    }
    Ok(())
}

fn validate_positive_u64(table: &toml::map::Map<String, toml::Value>, key: &str) -> Result<()> {
    let Some(value) = table.get(key) else {
        return Ok(());
    };
    let value = value
        .as_integer()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| format!("world Smolfile {key} must be an unsigned integer"))?;
    if value == 0 {
        return Err(format!("world Smolfile {key} must be greater than zero"));
    }
    Ok(())
}

fn resolve_world_image(smolfile: &Path, reference: &str) -> Result<WorldImage> {
    if reference == "-" {
        return Err("world Smolfiles cannot use stdin image material; prepare a local archive first".into());
    }
    if looks_local(reference) {
        let path = resolve_local_archive_path(smolfile, reference)?;
        let (digest, identity) = audited_archive_digest(&path)?;
        return Ok(WorldImage::LocalArchive {
            reference: reference.to_string(),
            digest,
            path,
            identity,
        });
    }
    let digest = reference
        .rsplit_once("@sha256:")
        .map(|(_, digest)| digest)
        .filter(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or_else(|| {
            format!(
                "world registry image '{reference}' must include an immutable lowercase sha256 digest"
            )
        })?;
    if reference.starts_with("@sha256:") {
        return Err("world registry image must name a repository before its digest".into());
    }
    Ok(WorldImage::Registry {
        reference: reference.to_string(),
        digest: format!("sha256:{digest}"),
    })
}

fn resolve_local_archive_path(smolfile: &Path, reference: &str) -> Result<PathBuf> {
    let path = Path::new(reference);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        smolfile.parent().expect("canonical file has a parent").join(path)
    };
    let path = canonical_regular_file(&path, "world image archive")?;
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("inspect world image archive {}: {error}", path.display()))?;
    if metadata.len() > max_archive_bytes() {
        return Err(format!(
            "world image archive is {} bytes, over the {}-byte limit",
            metadata.len(),
            max_archive_bytes()
        ));
    }
    if looks_like_dockerfile(&path) {
        return Err(format!(
            "world image archive {} looks like a Dockerfile; build and export an image first",
            path.display()
        ));
    }
    Ok(path)
}

fn audited_archive_digest(path: &Path) -> Result<(String, ArchiveIdentity)> {
    let before = archive_identity(path)?;
    let digest = digest_file(path)?;
    let after = archive_identity(path)?;
    if before != after {
        return Err(format!(
            "world image archive changed while preparing {}",
            path.display()
        ));
    }
    Ok((digest, after))
}

fn looks_local(reference: &str) -> bool {
    reference.starts_with('/')
        || reference.starts_with("./")
        || reference.starts_with("../")
        || [".tar", ".tar.gz", ".tgz"].iter().any(|suffix| reference.ends_with(suffix))
}

fn max_archive_bytes() -> u64 {
    std::env::var("SMOLVM_MAX_IMAGE_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_ARCHIVE_BYTES)
}

fn looks_like_dockerfile(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        let name = name.to_ascii_lowercase();
        if name == "dockerfile"
            || name == "containerfile"
            || name.ends_with(".dockerfile")
            || name.ends_with(".containerfile")
        {
            return true;
        }
    }
    let mut bytes = [0_u8; 4096];
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let Ok(read) = std::io::Read::read(&mut file, &mut bytes) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&bytes[..read]) else {
        return false;
    };
    text.lines().take(50).find_map(|line| {
        let line = line.trim();
        (!line.is_empty() && !line.starts_with('#')).then(|| {
            let word = line.split_whitespace().next().unwrap_or_default();
            word.eq_ignore_ascii_case("from") || word.eq_ignore_ascii_case("arg")
        })
    }) == Some(true)
}

fn canonical_regular_file(path: &Path, label: &str) -> Result<PathBuf> {
    let metadata = fs::metadata(path).map_err(|error| format!("inspect {label} {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{label} {} must be a regular file", path.display()));
    }
    fs::canonicalize(path).map_err(|error| format!("resolve {label} {}: {error}", path.display()))
}

fn write_prepared_world_smolfile(
    paths: &WorldPaths,
    authored: &Path,
    mut document: toml::Value,
    archive: &Path,
    archive_digest: &str,
) -> Result<PathBuf> {
    let archive = archive
        .to_str()
        .ok_or_else(|| format!("prepared archive {} is not valid UTF-8", archive.display()))?;
    let table = document
        .as_table_mut()
        .ok_or_else(|| "world Smolfile top level must be a TOML table".to_string())?;
    table.insert("image".to_string(), toml::Value::String(archive.to_string()));
    let generated = toml::to_string(&document)
        .map_err(|error| format!("serialize prepared world Smolfile: {error}"))?;
    let directory = paths.material_dir();
    ensure_private_dir(&directory)?;
    let authored_digest = digest_file(authored)?;
    let name = format!(
        "{}-{}.Smolfile",
        authored_digest.trim_start_matches("blake3:"),
        archive_digest.trim_start_matches("blake3:")
    );
    let path = directory.join(name);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(generated.as_bytes())
                .map_err(|error| format!("write prepared world Smolfile {}: {error}", path.display()))?;
            file.sync_all()
                .map_err(|error| format!("sync prepared world Smolfile {}: {error}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create prepared world Smolfile {}: {error}", path.display())),
    }
    let prepared = canonical_regular_file(&path, "prepared world Smolfile")?;
    if !prepared.starts_with(&directory) {
        return Err("prepared world Smolfile escaped its private material directory".into());
    }
    if fs::read_to_string(&prepared)
        .map_err(|error| format!("read prepared world Smolfile {}: {error}", prepared.display()))?
        != generated
    {
        return Err("prepared world Smolfile conflicts with different material bytes".into());
    }
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_directory_images_before_any_machine_is_created() {
        let directory = tempfile_dir("directory-image");
        let rootfs = directory.join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let smolfile = directory.join("Smolfile");
        fs::write(&smolfile, "image = \"./rootfs\"\n").unwrap();
        let error = parse_world_smolfile(&smolfile).unwrap_err();
        assert!(error.contains("regular file"), "{error}");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejects_host_capabilities_and_invalid_resources() {
        for text in [
            "image = \"./image.tar\"\nnet = true\n",
            "image = \"./image.tar\"\ncpus = 0\n",
            "image = \"./image.tar\"\nmemory = 63\n",
            "image = \"./image.tar\"\nstorage = 0\n",
        ] {
            let directory = tempfile_dir("invalid-world-smolfile");
            fs::write(directory.join("image.tar"), b"archive").unwrap();
            let smolfile = directory.join("Smolfile");
            fs::write(&smolfile, text).unwrap();
            assert!(parse_world_smolfile(&smolfile).is_err(), "{text}");
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn local_archive_is_a_world_owned_material_observation() {
        let directory = tempfile_dir("local-archive");
        let archive = directory.join("image.tar");
        fs::write(&archive, b"archive").unwrap();
        let smolfile = directory.join("Smolfile");
        fs::write(&smolfile, "image = \"./image.tar\"\ncpus = 2\nmemory = 128\n").unwrap();
        let parsed = parse_world_smolfile(&smolfile).unwrap();
        match parsed.image {
            WorldImage::LocalArchive {
                path,
                digest,
                identity,
                ..
            } => {
                assert_eq!(path, archive.canonicalize().unwrap());
                assert_eq!(digest, digest_file(&archive).unwrap());
                assert_eq!(identity, archive_identity(&archive).unwrap());
            }
            WorldImage::Registry { .. } => panic!("expected local archive"),
        }
        let fast = verify_prepared_world_smolfile(&smolfile, false).unwrap();
        assert_eq!(fast.local_archive, archive.canonicalize().unwrap());
        assert_eq!(fast.archive_identity, archive_identity(&archive).unwrap());
        assert_eq!(fast.image_digest, None);
        let deep = verify_prepared_world_smolfile(&smolfile, true).unwrap();
        assert_eq!(deep.image_digest, Some(digest_file(&archive).unwrap()));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn accepts_an_immutable_registry_profile_with_all_world_fields() {
        let directory = tempfile_dir("registry-profile");
        let smolfile = directory.join("Smolfile");
        fs::write(
            &smolfile,
            concat!(
                "image = \"ghcr.io/astral-sh/uv@sha256:",
                "8802acc1520b49590d75e5823c6586ef2971af0687cc19030be67aabf6b32577\"\n",
                "cpus = 1\nmemory = 256\nstorage = 2\noverlay = 1\n",
                "entrypoint = [\"/bin/sh\", \"-c\"]\n",
                "cmd = [\"exec sleep infinity\"]\n",
                "env = [\"PATH=/usr/bin\"]\nworkdir = \"/workspace\"\n",
            ),
        )
        .unwrap();

        let parsed = parse_world_smolfile(&smolfile).unwrap();
        assert!(matches!(
            parsed.image,
            WorldImage::Registry { ref reference, ref digest }
                if reference.starts_with("ghcr.io/astral-sh/uv@sha256:")
                    && digest.starts_with("sha256:")
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    fn tempfile_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "smolworld-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
}
