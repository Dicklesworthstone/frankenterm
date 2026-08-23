//! Crash-consistent attachment intent for configured client domains.
//!
//! Domain names are configuration locators and can reveal host naming.  This
//! authority therefore persists only a domain-separated SHA-256 fingerprint.
//! Absence means "follow configuration"; an explicit attached or detached
//! record overrides the configured auto-connect bit until the operator makes
//! the opposite choice.

use fs2::FileExt as _;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use thiserror::Error;

const MAGIC: &[u8; 8] = b"FTDDOM01";
const SCHEMA_VERSION: u32 = 1;
const HEADER_BYTES: usize = 32;
const ENTRY_BYTES: usize = 33;
const DIGEST_BYTES: usize = 32;
const MAX_DOMAIN_INTENTS: usize = 4_096;
const MAX_MANIFEST_BYTES: u64 =
    (HEADER_BYTES + MAX_DOMAIN_INTENTS * ENTRY_BYTES + DIGEST_BYTES) as u64;
const FINGERPRINT_DOMAIN: &[u8] = b"frankenterm.gui.domain-reconnect-name.v1\0";
const CHECKSUM_DOMAIN: &[u8] = b"frankenterm.gui.domain-reconnect-manifest.v1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainAttachmentIntent {
    Attached,
    Detached,
}

impl DomainAttachmentIntent {
    const fn encode(self) -> u8 {
        match self {
            Self::Attached => 1,
            Self::Detached => 2,
        }
    }

    fn decode(value: u8) -> Result<Self, DomainReconnectManifestError> {
        match value {
            1 => Ok(Self::Attached),
            2 => Ok(Self::Detached),
            _ => Err(DomainReconnectManifestError::Invalid {
                reason: "unknown attachment-intent discriminant",
            }),
        }
    }
}

#[derive(Debug, Error)]
pub enum DomainReconnectManifestError {
    #[error("domain reconnect manifest {operation} failed ({kind:?})")]
    Io {
        operation: &'static str,
        kind: io::ErrorKind,
    },
    #[error("domain reconnect manifest is oversized: {actual} bytes exceeds {maximum}")]
    Oversized { actual: u64, maximum: u64 },
    #[error("domain reconnect manifest has unsupported schema version {found}")]
    UnsupportedVersion { found: u32 },
    #[error("domain reconnect manifest is invalid: {reason}")]
    Invalid { reason: &'static str },
    #[error("domain reconnect manifest has two different states at generation {generation}")]
    AmbiguousGeneration { generation: u64 },
    #[error("domain reconnect manifest generation namespace is exhausted")]
    GenerationExhausted,
    #[error("domain reconnect manifest private-file contract failed: {reason}")]
    UnsafeFile { reason: &'static str },
    #[error("domain reconnect manifest directory is not private")]
    DirectoryNotPrivate,
}

impl DomainReconnectManifestError {
    fn io(operation: &'static str, error: io::Error) -> Self {
        Self::Io {
            operation,
            kind: error.kind(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DomainReconnectManifest {
    generation: u64,
    intents: BTreeMap<[u8; 32], DomainAttachmentIntent>,
}

impl DomainReconnectManifest {
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn intent_for_name(&self, domain_name: &str) -> Option<DomainAttachmentIntent> {
        self.intents
            .get(&fingerprint_domain_name(domain_name))
            .copied()
    }

    #[must_use]
    pub fn should_connect(&self, domain_name: &str, configured_auto_connect: bool) -> bool {
        match self.intent_for_name(domain_name) {
            Some(DomainAttachmentIntent::Attached) => true,
            Some(DomainAttachmentIntent::Detached) => false,
            None => configured_auto_connect,
        }
    }

    #[must_use]
    pub fn intent_count(&self) -> usize {
        self.intents.len()
    }
}

#[derive(Debug)]
struct LoadedManifest {
    manifest: DomainReconnectManifest,
    active_slot: Option<usize>,
}

enum SlotRead {
    Missing,
    Empty,
    Valid(DomainReconnectManifest),
    Invalid(DomainReconnectManifestError),
}

#[must_use]
pub fn fingerprint_domain_name(domain_name: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(FINGERPRINT_DOMAIN);
    digest.update(domain_name.as_bytes());
    digest.finalize().into()
}

fn manifest_paths(directory: &Path) -> [PathBuf; 2] {
    [
        directory.join("domain-reconnect-manifest.slot-0"),
        directory.join("domain-reconnect-manifest.slot-1"),
    ]
}

fn lock_path(directory: &Path) -> PathBuf {
    directory.join("domain-reconnect-manifest.lock")
}

fn ensure_manifest_directory(directory: &Path) -> Result<(), DomainReconnectManifestError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(directory)
            .map_err(|error| DomainReconnectManifestError::io("create directory", error))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(directory)
        .map_err(|error| DomainReconnectManifestError::io("create directory", error))?;
    let metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| DomainReconnectManifestError::io("inspect directory", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DomainReconnectManifestError::UnsafeFile {
            reason: "manifest directory is not a direct directory",
        });
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(DomainReconnectManifestError::DirectoryNotPrivate);
    }
    Ok(())
}

fn private_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
}

fn read_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
}

fn validate_private_file(
    metadata: &Metadata,
    directory_metadata: &Metadata,
) -> Result<(), DomainReconnectManifestError> {
    if !metadata.is_file() {
        return Err(DomainReconnectManifestError::UnsafeFile {
            reason: "authority path is not a regular file",
        });
    }
    #[cfg(unix)]
    {
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(DomainReconnectManifestError::UnsafeFile {
                reason: "authority file mode is not 0600",
            });
        }
        if metadata.nlink() != 1 {
            return Err(DomainReconnectManifestError::UnsafeFile {
                reason: "authority file has multiple hard links",
            });
        }
        if metadata.uid() != directory_metadata.uid() {
            return Err(DomainReconnectManifestError::UnsafeFile {
                reason: "authority file owner differs from its directory owner",
            });
        }
    }
    Ok(())
}

fn open_lock_file(directory: &Path) -> Result<File, DomainReconnectManifestError> {
    ensure_manifest_directory(directory)?;
    let path = lock_path(directory);
    if std::fs::symlink_metadata(&path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(DomainReconnectManifestError::UnsafeFile {
            reason: "lock path is a symbolic link",
        });
    }
    let existed = path.exists();
    let file = private_open_options()
        .open(&path)
        .map_err(|error| DomainReconnectManifestError::io("open lock", error))?;
    let directory_metadata = std::fs::metadata(directory)
        .map_err(|error| DomainReconnectManifestError::io("inspect directory owner", error))?;
    validate_private_file(
        &file
            .metadata()
            .map_err(|error| DomainReconnectManifestError::io("inspect lock", error))?,
        &directory_metadata,
    )?;
    if !existed {
        file.sync_all()
            .map_err(|error| DomainReconnectManifestError::io("sync lock", error))?;
        sync_directory(directory)?;
    }
    Ok(file)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<(), DomainReconnectManifestError> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| DomainReconnectManifestError::io("sync directory", error))
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<(), DomainReconnectManifestError> {
    Ok(())
}

fn encode_manifest(
    manifest: &DomainReconnectManifest,
) -> Result<Vec<u8>, DomainReconnectManifestError> {
    if manifest.generation == 0 {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "published generation is zero",
        });
    }
    if manifest.intents.len() > MAX_DOMAIN_INTENTS {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "attachment-intent count exceeds its bound",
        });
    }
    let count = u32::try_from(manifest.intents.len()).map_err(|_| {
        DomainReconnectManifestError::Invalid {
            reason: "attachment-intent count cannot be encoded",
        }
    })?;
    let capacity = HEADER_BYTES
        .checked_add(manifest.intents.len().saturating_mul(ENTRY_BYTES))
        .and_then(|value| value.checked_add(DIGEST_BYTES))
        .ok_or(DomainReconnectManifestError::Oversized {
            actual: u64::MAX,
            maximum: MAX_MANIFEST_BYTES,
        })?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(MAGIC);
    encoded.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    encoded.extend_from_slice(&0_u32.to_le_bytes());
    encoded.extend_from_slice(&manifest.generation.to_le_bytes());
    encoded.extend_from_slice(&count.to_le_bytes());
    encoded.extend_from_slice(&0_u32.to_le_bytes());
    for (fingerprint, intent) in &manifest.intents {
        if *fingerprint == [0; 32] {
            return Err(DomainReconnectManifestError::Invalid {
                reason: "zero domain fingerprint is reserved",
            });
        }
        encoded.extend_from_slice(fingerprint);
        encoded.push(intent.encode());
    }
    let mut checksum = Sha256::new();
    checksum.update(CHECKSUM_DOMAIN);
    checksum.update(&encoded);
    encoded.extend_from_slice(&checksum.finalize());
    Ok(encoded)
}

fn decode_manifest(bytes: &[u8]) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual > MAX_MANIFEST_BYTES {
        return Err(DomainReconnectManifestError::Oversized {
            actual,
            maximum: MAX_MANIFEST_BYTES,
        });
    }
    if bytes.len() < HEADER_BYTES + DIGEST_BYTES {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "manifest is truncated",
        });
    }
    if &bytes[..8] != MAGIC {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "manifest magic does not match",
        });
    }
    let schema = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed schema slice"));
    if schema != SCHEMA_VERSION {
        return Err(DomainReconnectManifestError::UnsupportedVersion { found: schema });
    }
    if bytes[12..16] != [0; 4] || bytes[28..32] != [0; 4] {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "reserved header bytes are nonzero",
        });
    }
    let generation = u64::from_le_bytes(
        bytes[16..24]
            .try_into()
            .expect("fixed generation slice"),
    );
    if generation == 0 {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "published generation is zero",
        });
    }
    let count = u32::from_le_bytes(bytes[24..28].try_into().expect("fixed count slice"));
    let count = usize::try_from(count).map_err(|_| DomainReconnectManifestError::Invalid {
        reason: "attachment-intent count does not fit this platform",
    })?;
    if count > MAX_DOMAIN_INTENTS {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "attachment-intent count exceeds its bound",
        });
    }
    let expected = HEADER_BYTES
        .checked_add(count.saturating_mul(ENTRY_BYTES))
        .and_then(|value| value.checked_add(DIGEST_BYTES))
        .ok_or(DomainReconnectManifestError::Oversized {
            actual,
            maximum: MAX_MANIFEST_BYTES,
        })?;
    if bytes.len() != expected {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "manifest length does not match its entry count",
        });
    }
    let payload_end = expected - DIGEST_BYTES;
    let mut checksum = Sha256::new();
    checksum.update(CHECKSUM_DOMAIN);
    checksum.update(&bytes[..payload_end]);
    let expected_checksum: [u8; 32] = checksum.finalize().into();
    if expected_checksum[..] != bytes[payload_end..] {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "manifest checksum does not match",
        });
    }

    let mut intents = BTreeMap::new();
    let mut previous = None;
    for entry in bytes[HEADER_BYTES..payload_end].chunks_exact(ENTRY_BYTES) {
        let fingerprint: [u8; 32] = entry[..32].try_into().expect("fixed fingerprint slice");
        if fingerprint == [0; 32] {
            return Err(DomainReconnectManifestError::Invalid {
                reason: "zero domain fingerprint is reserved",
            });
        }
        if previous.is_some_and(|prior| prior >= fingerprint) {
            return Err(DomainReconnectManifestError::Invalid {
                reason: "domain fingerprints are duplicate or not canonical",
            });
        }
        previous = Some(fingerprint);
        intents.insert(fingerprint, DomainAttachmentIntent::decode(entry[32])?);
    }
    Ok(DomainReconnectManifest {
        generation,
        intents,
    })
}

fn read_slot(
    path: &Path,
    directory_metadata: &Metadata,
) -> Result<SlotRead, DomainReconnectManifestError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Ok(SlotRead::Invalid(
                DomainReconnectManifestError::UnsafeFile {
                    reason: "authority slot is a symbolic link",
                },
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SlotRead::Missing),
        Err(error) => {
            return Err(DomainReconnectManifestError::io(
                "inspect authority slot",
                error,
            ));
        }
    }
    let mut file = match read_open_options().open(path) {
        Ok(file) => file,
        Err(error) => {
            return Ok(SlotRead::Invalid(DomainReconnectManifestError::io(
                "open authority slot",
                error,
            )));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| DomainReconnectManifestError::io("inspect authority slot", error))?;
    if let Err(error) = validate_private_file(&metadata, directory_metadata) {
        return Ok(SlotRead::Invalid(error));
    }
    if metadata.len() == 0 {
        return Ok(SlotRead::Empty);
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Ok(SlotRead::Invalid(
            DomainReconnectManifestError::Oversized {
                actual: metadata.len(),
                maximum: MAX_MANIFEST_BYTES,
            },
        ));
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| {
        DomainReconnectManifestError::Oversized {
            actual: metadata.len(),
            maximum: MAX_MANIFEST_BYTES,
        }
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.read_to_end(&mut bytes)
        .map_err(|error| DomainReconnectManifestError::io("read authority slot", error))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != metadata.len() {
        return Ok(SlotRead::Invalid(
            DomainReconnectManifestError::Invalid {
                reason: "authority slot length changed while reading",
            },
        ));
    }
    Ok(match decode_manifest(&bytes) {
        Ok(manifest) => SlotRead::Valid(manifest),
        Err(error) => SlotRead::Invalid(error),
    })
}

fn select_manifest(
    first: SlotRead,
    second: SlotRead,
) -> Result<LoadedManifest, DomainReconnectManifestError> {
    match (first, second) {
        (SlotRead::Valid(first), SlotRead::Valid(second)) => {
            if first.generation > second.generation {
                Ok(LoadedManifest {
                    manifest: first,
                    active_slot: Some(0),
                })
            } else if second.generation > first.generation {
                Ok(LoadedManifest {
                    manifest: second,
                    active_slot: Some(1),
                })
            } else if first == second {
                Ok(LoadedManifest {
                    manifest: first,
                    active_slot: Some(0),
                })
            } else {
                Err(DomainReconnectManifestError::AmbiguousGeneration {
                    generation: first.generation,
                })
            }
        }
        (SlotRead::Valid(manifest), _) => Ok(LoadedManifest {
            manifest,
            active_slot: Some(0),
        }),
        (_, SlotRead::Valid(manifest)) => Ok(LoadedManifest {
            manifest,
            active_slot: Some(1),
        }),
        (SlotRead::Invalid(error), _) | (_, SlotRead::Invalid(error)) => Err(error),
        (SlotRead::Missing | SlotRead::Empty, SlotRead::Missing | SlotRead::Empty) => {
            Ok(LoadedManifest {
                manifest: DomainReconnectManifest::default(),
                active_slot: None,
            })
        }
    }
}

fn load_locked(directory: &Path) -> Result<LoadedManifest, DomainReconnectManifestError> {
    let directory_metadata = std::fs::metadata(directory)
        .map_err(|error| DomainReconnectManifestError::io("inspect directory owner", error))?;
    let paths = manifest_paths(directory);
    select_manifest(
        read_slot(&paths[0], &directory_metadata)?,
        read_slot(&paths[1], &directory_metadata)?,
    )
}

pub fn load_from(
    directory: &Path,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let lock = open_lock_file(directory)?;
    lock.lock_shared()
        .map_err(|error| DomainReconnectManifestError::io("lock for reading", error))?;
    Ok(load_locked(directory)?.manifest)
}

pub fn load() -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    load_from(config::DATA_DIR.as_path())
}

fn write_slot(
    directory: &Path,
    path: &Path,
    manifest: &DomainReconnectManifest,
) -> Result<(), DomainReconnectManifestError> {
    if std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(DomainReconnectManifestError::UnsafeFile {
            reason: "authority slot is a symbolic link",
        });
    }
    let existed = path.exists();
    let mut file = private_open_options()
        .open(path)
        .map_err(|error| DomainReconnectManifestError::io("open authority slot", error))?;
    let directory_metadata = std::fs::metadata(directory)
        .map_err(|error| DomainReconnectManifestError::io("inspect directory owner", error))?;
    validate_private_file(
        &file
            .metadata()
            .map_err(|error| DomainReconnectManifestError::io("inspect authority slot", error))?,
        &directory_metadata,
    )?;
    let encoded = encode_manifest(manifest)?;
    file.set_len(0)
        .map_err(|error| DomainReconnectManifestError::io("truncate authority slot", error))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| DomainReconnectManifestError::io("seek authority slot", error))?;
    file.write_all(&encoded)
        .map_err(|error| DomainReconnectManifestError::io("write authority slot", error))?;
    file.sync_all()
        .map_err(|error| DomainReconnectManifestError::io("sync authority slot", error))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| DomainReconnectManifestError::io("seek authority slot", error))?;
    let mut persisted = Vec::with_capacity(encoded.len());
    file.read_to_end(&mut persisted)
        .map_err(|error| DomainReconnectManifestError::io("verify authority slot", error))?;
    if decode_manifest(&persisted)? != *manifest {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "persisted authority does not match the intended generation",
        });
    }
    if !existed {
        sync_directory(directory)?;
    }
    Ok(())
}

pub fn set_intent_at(
    directory: &Path,
    domain_name: &str,
    intent: DomainAttachmentIntent,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let lock = open_lock_file(directory)?;
    lock.lock_exclusive()
        .map_err(|error| DomainReconnectManifestError::io("lock for update", error))?;
    let loaded = load_locked(directory)?;
    let fingerprint = fingerprint_domain_name(domain_name);
    let mut next = loaded.manifest;
    if next.intents.get(&fingerprint).copied() == Some(intent) {
        return Ok(next);
    }
    if !next.intents.contains_key(&fingerprint) && next.intents.len() == MAX_DOMAIN_INTENTS {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "attachment-intent count exceeds its bound",
        });
    }
    next.generation = next
        .generation
        .checked_add(1)
        .ok_or(DomainReconnectManifestError::GenerationExhausted)?;
    next.intents.insert(fingerprint, intent);
    let target_slot = match loaded.active_slot {
        Some(0) => 1,
        Some(1) | None => 0,
        Some(_) => unreachable!("manifest has exactly two slots"),
    };
    write_slot(
        directory,
        &manifest_paths(directory)[target_slot],
        &next,
    )?;
    Ok(next)
}

pub fn set_intent(
    domain_name: &str,
    intent: DomainAttachmentIntent,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    set_intent_at(config::DATA_DIR.as_path(), domain_name, intent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    fn attached_manifest(name: &str) -> DomainReconnectManifest {
        let mut intents = BTreeMap::new();
        intents.insert(
            fingerprint_domain_name(name),
            DomainAttachmentIntent::Attached,
        );
        DomainReconnectManifest {
            generation: 1,
            intents,
        }
    }

    #[test]
    fn codec_is_canonical_bounded_and_checksum_protected() {
        let manifest = attached_manifest("trj");
        let encoded = encode_manifest(&manifest).expect("encode manifest");
        assert_eq!(decode_manifest(&encoded).expect("decode manifest"), manifest);

        let mut corrupt = encoded;
        corrupt[HEADER_BYTES] ^= 1;
        assert!(matches!(
            decode_manifest(&corrupt),
            Err(DomainReconnectManifestError::Invalid {
                reason: "manifest checksum does not match"
            })
        ));

        let mut unsupported = encode_manifest(&manifest).expect("encode unsupported fixture");
        unsupported[8..12].copy_from_slice(&2_u32.to_le_bytes());
        assert!(matches!(
            decode_manifest(&unsupported),
            Err(DomainReconnectManifestError::UnsupportedVersion { found: 2 })
        ));

        let oversized = vec![0_u8; usize::try_from(MAX_MANIFEST_BYTES).expect("bound fits") + 1];
        assert!(matches!(
            decode_manifest(&oversized),
            Err(DomainReconnectManifestError::Oversized { .. })
        ));
    }

    #[test]
    fn explicit_attach_and_detach_survive_restart_and_override_config() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let missing = load_from(temp.path()).expect("missing manifest");
        assert!(missing.should_connect("trj", true));
        assert!(!missing.should_connect("trj", false));

        set_intent_at(
            temp.path(),
            "trj",
            DomainAttachmentIntent::Attached,
        )
        .expect("persist attach");
        let attached = load_from(temp.path()).expect("reload attached");
        assert!(attached.should_connect("trj", false));

        set_intent_at(
            temp.path(),
            "trj",
            DomainAttachmentIntent::Detached,
        )
        .expect("persist detach");
        let detached = load_from(temp.path()).expect("reload detached");
        assert!(!detached.should_connect("trj", true));
        assert_eq!(detached.generation(), 2);
    }

    #[test]
    fn torn_inactive_slot_preserves_last_committed_generation() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let first = set_intent_at(
            temp.path(),
            "trj",
            DomainAttachmentIntent::Attached,
        )
        .expect("persist first generation");
        let paths = manifest_paths(temp.path());
        let mut inactive = private_open_options()
            .open(&paths[1])
            .expect("open inactive slot");
        inactive.write_all(b"torn").expect("write torn slot");
        inactive.sync_all().expect("sync torn slot");

        assert_eq!(load_from(temp.path()).expect("recover prior slot"), first);
    }

    #[test]
    fn divergent_states_at_the_same_generation_fail_closed() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let attached = attached_manifest("trj");
        let mut detached = attached.clone();
        detached.intents.insert(
            fingerprint_domain_name("trj"),
            DomainAttachmentIntent::Detached,
        );
        for (path, manifest) in manifest_paths(temp.path())
            .into_iter()
            .zip([attached, detached])
        {
            let mut file = private_open_options()
                .open(path)
                .expect("create private authority slot");
            file.write_all(&encode_manifest(&manifest).expect("encode fixture"))
                .expect("write authority fixture");
            file.sync_all().expect("sync authority fixture");
        }

        assert!(matches!(
            load_from(temp.path()),
            Err(DomainReconnectManifestError::AmbiguousGeneration { generation: 1 })
        ));
    }

    #[test]
    fn duplicate_or_noncanonical_fingerprints_are_rejected() {
        let manifest = attached_manifest("trj");
        let mut encoded = encode_manifest(&manifest).expect("encode manifest");
        encoded[24..28].copy_from_slice(&2_u32.to_le_bytes());
        let first_entry = encoded[HEADER_BYTES..HEADER_BYTES + ENTRY_BYTES].to_vec();
        let payload_end = encoded.len() - DIGEST_BYTES;
        encoded.splice(payload_end..payload_end, first_entry);
        let payload_end = encoded.len() - DIGEST_BYTES;
        let mut checksum = Sha256::new();
        checksum.update(CHECKSUM_DOMAIN);
        checksum.update(&encoded[..payload_end]);
        encoded[payload_end..].copy_from_slice(&checksum.finalize());

        assert!(matches!(
            decode_manifest(&encoded),
            Err(DomainReconnectManifestError::Invalid {
                reason: "domain fingerprints are duplicate or not canonical"
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_directory_permissions_and_symlink_slots_fail_closed() {
        let unsafe_directory = tempfile::tempdir().expect("temporary unsafe directory");
        std::fs::set_permissions(
            unsafe_directory.path(),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("make directory unsafe");
        assert!(matches!(
            load_from(unsafe_directory.path()),
            Err(DomainReconnectManifestError::DirectoryNotPrivate)
        ));

        let temp = tempfile::tempdir().expect("temporary directory");
        set_intent_at(
            temp.path(),
            "trj",
            DomainAttachmentIntent::Attached,
        )
        .expect("persist manifest");
        let paths = manifest_paths(temp.path());
        std::fs::set_permissions(&paths[0], std::fs::Permissions::from_mode(0o644))
            .expect("make slot unsafe");
        assert!(matches!(
            load_from(temp.path()),
            Err(DomainReconnectManifestError::UnsafeFile { .. })
        ));

        let other = tempfile::tempdir().expect("second temporary directory");
        std::os::unix::fs::symlink(other.path(), &manifest_paths(other.path())[0])
            .expect("create slot symlink");
        assert!(matches!(
            load_from(other.path()),
            Err(DomainReconnectManifestError::UnsafeFile { .. })
        ));
    }

    #[test]
    fn diagnostics_and_persisted_bytes_do_not_contain_domain_names() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let secret_name = "private-production-host";
        set_intent_at(
            temp.path(),
            secret_name,
            DomainAttachmentIntent::Attached,
        )
        .expect("persist manifest");
        for path in manifest_paths(temp.path()) {
            if let Ok(bytes) = std::fs::read(path) {
                assert!(!bytes
                    .windows(secret_name.len())
                    .any(|window| window == secret_name.as_bytes()));
            }
        }
    }
}
