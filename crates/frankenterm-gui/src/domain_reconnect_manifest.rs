//! Crash-consistent attachment intent for configured client domains.
//!
//! Domain names are configuration locators and can reveal host naming.  This
//! authority therefore persists only a domain-separated SHA-256 fingerprint.
//! Absence means "follow configuration"; an explicit attached or detached
//! record overrides the configured auto-connect bit until the operator makes
//! the opposite choice.

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt as _};
use cap_std::fs::{Dir as CapDir, File as CapFile, Metadata as CapMetadata};
use cap_std::fs::{OpenOptions as CapOpenOptions, OpenOptionsExt as _};
#[cfg(unix)]
use cap_std::fs::{MetadataExt as CapUnixMetadataExt, PermissionsExt as _};
#[cfg(windows)]
use cap_std::fs::MetadataExt as CapWindowsMetadataExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
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
const SLOT_NAMES: [&str; 2] = [
    "domain-reconnect-manifest.slot-0",
    "domain-reconnect-manifest.slot-1",
];
const LOCK_NAME: &str = "domain-reconnect-manifest.lock";

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
    #[error("domain reconnect manifest authority identity changed during {operation}")]
    IdentityChanged { operation: &'static str },
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

#[cfg(test)]
fn manifest_paths(directory: &Path) -> [PathBuf; 2] {
    [directory.join(SLOT_NAMES[0]), directory.join(SLOT_NAMES[1])]
}

#[cfg(test)]
fn lock_path(directory: &Path) -> PathBuf {
    directory.join(LOCK_NAME)
}

#[cfg(test)]
fn private_open_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
}

fn open_manifest_directory(directory: &Path) -> Result<CapDir, DomainReconnectManifestError> {
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
    {
        if metadata.permissions().mode() & 0o7777 != 0o700
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err(DomainReconnectManifestError::DirectoryNotPrivate);
        }
    }
    let pinned = CapDir::open_ambient_dir(directory, cap_std::ambient_authority())
        .map_err(|error| DomainReconnectManifestError::io("open manifest directory", error))?;
    validate_pinned_directory(directory, &pinned)?;
    Ok(pinned)
}

fn validate_private_file(
    metadata: &CapMetadata,
    directory: &CapDir,
) -> Result<(), DomainReconnectManifestError> {
    if !metadata.is_file() {
        return Err(DomainReconnectManifestError::UnsafeFile {
            reason: "authority path is not a regular file",
        });
    }
    #[cfg(unix)]
    {
        let directory_metadata = directory
            .dir_metadata()
            .map_err(|error| {
                DomainReconnectManifestError::io("inspect manifest directory", error)
            })?;
        if metadata.permissions().mode() & 0o7777 != 0o600 {
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
    #[cfg(windows)]
    if metadata.number_of_links() != Some(1) {
        return Err(DomainReconnectManifestError::UnsafeFile {
            reason: "authority file has multiple hard links",
        });
    }
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(left: &CapMetadata, right: &CapMetadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &CapMetadata, right: &CapMetadata) -> bool {
    match (
        left.volume_serial_number(),
        left.file_index(),
        right.volume_serial_number(),
        right.file_index(),
    ) {
        (Some(left_volume), Some(left_index), Some(right_volume), Some(right_index)) => {
            left_volume == right_volume && left_index == right_index
        }
        _ => false,
    }
}

#[cfg(all(not(unix), not(windows)))]
fn same_file_identity(_left: &CapMetadata, _right: &CapMetadata) -> bool {
    false
}

fn validate_pinned_directory(
    path: &Path,
    directory: &CapDir,
) -> Result<(), DomainReconnectManifestError> {
    let named = std::fs::symlink_metadata(path)
        .map_err(|error| DomainReconnectManifestError::io("reinspect manifest directory", error))?;
    if named.file_type().is_symlink() || !named.is_dir() {
        return Err(DomainReconnectManifestError::IdentityChanged {
            operation: "directory revalidation",
        });
    }
    #[cfg(unix)]
    if named.permissions().mode() & 0o7777 != 0o700
        || named.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(DomainReconnectManifestError::DirectoryNotPrivate);
    }
    let reopened = CapDir::open_ambient_dir(path, cap_std::ambient_authority())
        .map_err(|error| DomainReconnectManifestError::io("reopen manifest directory", error))?;
    let pinned_metadata = directory
        .dir_metadata()
        .map_err(|error| DomainReconnectManifestError::io("inspect pinned directory", error))?;
    let reopened_metadata = reopened
        .dir_metadata()
        .map_err(|error| DomainReconnectManifestError::io("inspect reopened directory", error))?;
    if !pinned_metadata.is_dir()
        || !reopened_metadata.is_dir()
        || !same_file_identity(&pinned_metadata, &reopened_metadata)
    {
        return Err(DomainReconnectManifestError::IdentityChanged {
            operation: "directory revalidation",
        });
    }
    validate_private_file_owner(&pinned_metadata)?;
    Ok(())
}

fn validate_private_file_owner(
    directory_metadata: &CapMetadata,
) -> Result<(), DomainReconnectManifestError> {
    #[cfg(not(unix))]
    let _ = directory_metadata;
    #[cfg(unix)]
    if directory_metadata.permissions().mode() & 0o7777 != 0o700
        || directory_metadata.uid() != rustix::process::geteuid().as_raw()
    {
        return Err(DomainReconnectManifestError::DirectoryNotPrivate);
    }
    Ok(())
}

fn named_private_file(
    directory: &CapDir,
    name: &OsStr,
) -> Result<CapMetadata, DomainReconnectManifestError> {
    let metadata = directory
        .symlink_metadata(name)
        .map_err(|error| DomainReconnectManifestError::io("inspect authority name", error))?;
    if metadata.file_type().is_symlink() {
        return Err(DomainReconnectManifestError::UnsafeFile {
            reason: "authority path is a symbolic link",
        });
    }
    validate_private_file(&metadata, directory)?;
    Ok(metadata)
}

fn validate_opened_name(
    directory: &CapDir,
    name: &OsStr,
    file: &CapFile,
    operation: &'static str,
) -> Result<CapMetadata, DomainReconnectManifestError> {
    let opened = file
        .metadata()
        .map_err(|error| DomainReconnectManifestError::io("inspect opened authority", error))?;
    validate_private_file(&opened, directory)?;
    let named = named_private_file(directory, name)?;
    if !same_file_identity(&opened, &named) {
        return Err(DomainReconnectManifestError::IdentityChanged { operation });
    }
    Ok(opened)
}

fn sync_directory(directory: &CapDir) -> Result<(), DomainReconnectManifestError> {
    #[cfg(unix)]
    directory
        .open(".")
        .and_then(|file| file.sync_all())
        .map_err(|error| DomainReconnectManifestError::io("sync directory", error))?;
    Ok(())
}

struct ManifestLease {
    path: PathBuf,
    directory: CapDir,
    lock_authority: CapFile,
    lock: File,
}

impl ManifestLease {
    fn acquire(
        path: &Path,
        exclusive: bool,
    ) -> Result<Self, DomainReconnectManifestError> {
        let directory = open_manifest_directory(path)?;
        let name = OsStr::new(LOCK_NAME);
        let before = match directory.symlink_metadata(name) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(DomainReconnectManifestError::UnsafeFile {
                        reason: "lock path is a symbolic link",
                    });
                }
                validate_private_file(&metadata, &directory)?;
                Some(metadata)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(DomainReconnectManifestError::io(
                    "inspect lock",
                    error,
                ));
            }
        };
        let mut options = CapOpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        options.mode(0o600);
        let lock_authority = directory
            .open_with(name, &options)
            .map_err(|error| DomainReconnectManifestError::io("open lock", error))?;
        let opened = validate_opened_name(
            &directory,
            name,
            &lock_authority,
            "lock open",
        )?;
        if opened.len() != 0
            || before
                .as_ref()
                .is_some_and(|before| !same_file_identity(before, &opened))
        {
            return Err(DomainReconnectManifestError::IdentityChanged {
                operation: "lock open",
            });
        }
        lock_authority
            .sync_all()
            .map_err(|error| DomainReconnectManifestError::io("sync lock", error))?;
        sync_directory(&directory)?;
        let lock = lock_authority
            .try_clone()
            .map(CapFile::into_std)
            .map_err(|error| DomainReconnectManifestError::io("clone lock", error))?;
        if exclusive {
            fs2::FileExt::lock_exclusive(&lock)
                .map_err(|error| DomainReconnectManifestError::io("lock for update", error))?;
        } else {
            fs2::FileExt::lock_shared(&lock)
                .map_err(|error| DomainReconnectManifestError::io("lock for reading", error))?;
        }
        let lease = Self {
            path: path.to_path_buf(),
            directory,
            lock_authority,
            lock,
        };
        lease.validate()?;
        Ok(lease)
    }

    fn validate(&self) -> Result<(), DomainReconnectManifestError> {
        validate_pinned_directory(&self.path, &self.directory)?;
        let metadata = validate_opened_name(
            &self.directory,
            OsStr::new(LOCK_NAME),
            &self.lock_authority,
            "locked authority revalidation",
        )?;
        if metadata.len() != 0 {
            return Err(DomainReconnectManifestError::UnsafeFile {
                reason: "lock authority is not empty",
            });
        }
        Ok(())
    }
}

impl Drop for ManifestLease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.lock);
    }
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
    directory: &CapDir,
    name: &OsStr,
) -> Result<SlotRead, DomainReconnectManifestError> {
    let before = match directory.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Ok(SlotRead::Invalid(
                DomainReconnectManifestError::UnsafeFile {
                    reason: "authority slot is a symbolic link",
                },
            ));
        }
        Ok(metadata) => {
            if let Err(error) = validate_private_file(&metadata, directory) {
                return Ok(SlotRead::Invalid(error));
            }
            metadata
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SlotRead::Missing),
        Err(error) => {
            return Err(DomainReconnectManifestError::io(
                "inspect authority slot",
                error,
            ));
        }
    };
    if before.len() > MAX_MANIFEST_BYTES {
        return Ok(SlotRead::Invalid(
            DomainReconnectManifestError::Oversized {
                actual: before.len(),
                maximum: MAX_MANIFEST_BYTES,
            },
        ));
    }
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let mut file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(error) => {
            return Ok(SlotRead::Invalid(DomainReconnectManifestError::io(
                "open authority slot",
                error,
            )));
        }
    };
    let opened = match validate_opened_name(directory, name, &file, "slot read open") {
        Ok(metadata) if same_file_identity(&before, &metadata) => metadata,
        Ok(_) => {
            return Ok(SlotRead::Invalid(
                DomainReconnectManifestError::IdentityChanged {
                    operation: "slot read open",
                },
            ));
        }
        Err(error) => return Ok(SlotRead::Invalid(error)),
    };
    let capacity = usize::try_from(opened.len()).map_err(|_| {
        DomainReconnectManifestError::Oversized {
            actual: opened.len(),
            maximum: MAX_MANIFEST_BYTES,
        }
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    (&mut file)
        .take(MAX_MANIFEST_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| DomainReconnectManifestError::io("read authority slot", error))?;
    let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual > MAX_MANIFEST_BYTES {
        return Ok(SlotRead::Invalid(
            DomainReconnectManifestError::Oversized {
                actual,
                maximum: MAX_MANIFEST_BYTES,
            },
        ));
    }
    let after = match validate_opened_name(directory, name, &file, "slot read completion") {
        Ok(metadata) => metadata,
        Err(error) => return Ok(SlotRead::Invalid(error)),
    };
    if actual != opened.len()
        || actual != after.len()
        || !same_file_identity(&opened, &after)
    {
        return Ok(SlotRead::Invalid(
            DomainReconnectManifestError::Invalid {
                reason: "authority slot length changed while reading",
            },
        ));
    }
    if bytes.is_empty() {
        return Ok(SlotRead::Empty);
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

fn load_locked(directory: &CapDir) -> Result<LoadedManifest, DomainReconnectManifestError> {
    select_manifest(
        read_slot(directory, OsStr::new(SLOT_NAMES[0]))?,
        read_slot(directory, OsStr::new(SLOT_NAMES[1]))?,
    )
}

pub fn load_from(
    directory: &Path,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let lease = ManifestLease::acquire(directory, false)?;
    let loaded = load_locked(&lease.directory)?;
    lease.validate()?;
    Ok(loaded.manifest)
}

pub fn load() -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    load_from(config::DATA_DIR.as_path())
}

fn write_slot(
    directory: &CapDir,
    name: &OsStr,
    manifest: &DomainReconnectManifest,
) -> Result<(), DomainReconnectManifestError> {
    let encoded = encode_manifest(manifest)?;
    let before = match directory.symlink_metadata(name) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(DomainReconnectManifestError::UnsafeFile {
                    reason: "authority slot is a symbolic link",
                });
            }
            validate_private_file(&metadata, directory)?;
            Some(metadata)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(DomainReconnectManifestError::io(
                "inspect authority slot",
                error,
            ));
        }
    };
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = directory
        .open_with(name, &options)
        .map_err(|error| DomainReconnectManifestError::io("open authority slot", error))?;
    let opened = validate_opened_name(directory, name, &file, "slot write open")?;
    if before
        .as_ref()
        .is_some_and(|before| !same_file_identity(before, &opened))
    {
        return Err(DomainReconnectManifestError::IdentityChanged {
            operation: "slot write open",
        });
    }
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
    let after = validate_opened_name(directory, name, &file, "slot write completion")?;
    if after.len() != u64::try_from(encoded.len()).unwrap_or(u64::MAX)
        || !same_file_identity(&opened, &after)
    {
        return Err(DomainReconnectManifestError::IdentityChanged {
            operation: "slot write completion",
        });
    }
    let mut persisted = Vec::with_capacity(encoded.len());
    (&mut file)
        .take(MAX_MANIFEST_BYTES.saturating_add(1))
        .read_to_end(&mut persisted)
        .map_err(|error| DomainReconnectManifestError::io("verify authority slot", error))?;
    if decode_manifest(&persisted)? != *manifest {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "persisted authority does not match the intended generation",
        });
    }
    sync_directory(directory)?;
    let published = validate_opened_name(directory, name, &file, "slot publication")?;
    if published.len() != u64::try_from(encoded.len()).unwrap_or(u64::MAX)
        || !same_file_identity(&opened, &published)
    {
        return Err(DomainReconnectManifestError::IdentityChanged {
            operation: "slot publication",
        });
    }
    Ok(())
}

pub fn set_intent_at(
    directory: &Path,
    domain_name: &str,
    intent: DomainAttachmentIntent,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let lease = ManifestLease::acquire(directory, true)?;
    let loaded = load_locked(&lease.directory)?;
    let fingerprint = fingerprint_domain_name(domain_name);
    let mut next = loaded.manifest;
    if next.intents.get(&fingerprint).copied() == Some(intent) {
        lease.validate()?;
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
        &lease.directory,
        OsStr::new(SLOT_NAMES[target_slot]),
        &next,
    )?;
    lease.validate()?;
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

    #[cfg(unix)]
    #[test]
    fn replacing_the_locked_authority_is_detected_before_success() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let lease = ManifestLease::acquire(temp.path(), true).expect("acquire manifest lease");
        let original_lock = lock_path(temp.path());
        let displaced_lock = temp.path().join("displaced-lock");
        std::fs::rename(&original_lock, displaced_lock).expect("displace locked authority");
        let replacement = private_open_options()
            .open(&original_lock)
            .expect("create replacement lock");
        replacement.sync_all().expect("sync replacement lock");

        assert!(matches!(
            lease.validate(),
            Err(DomainReconnectManifestError::IdentityChanged {
                operation: "locked authority revalidation"
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn replacing_an_open_slot_is_detected_by_descriptor_identity() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let manifest = set_intent_at(
            temp.path(),
            "trj",
            DomainAttachmentIntent::Attached,
        )
        .expect("persist manifest");
        let directory = open_manifest_directory(temp.path()).expect("open manifest directory");
        let name = OsStr::new(SLOT_NAMES[0]);
        let mut options = CapOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let opened = directory
            .open_with(name, &options)
            .expect("open authoritative slot");
        let slot_path = manifest_paths(temp.path())[0].clone();
        std::fs::rename(&slot_path, temp.path().join("displaced-slot"))
            .expect("displace authoritative slot");
        let mut replacement = private_open_options()
            .open(&slot_path)
            .expect("create replacement slot");
        replacement
            .write_all(&encode_manifest(&manifest).expect("encode replacement"))
            .expect("write replacement slot");
        replacement.sync_all().expect("sync replacement slot");

        assert!(matches!(
            validate_opened_name(&directory, name, &opened, "test slot replacement"),
            Err(DomainReconnectManifestError::IdentityChanged {
                operation: "test slot replacement"
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_manifest_directory_is_detected_by_pinned_identity() {
        use std::os::unix::fs::DirBuilderExt as _;

        let parent = tempfile::tempdir().expect("temporary parent directory");
        let authority_path = parent.path().join("authority");
        set_intent_at(
            &authority_path,
            "trj",
            DomainAttachmentIntent::Attached,
        )
        .expect("persist manifest");
        let lease = ManifestLease::acquire(&authority_path, true).expect("acquire manifest lease");
        std::fs::rename(&authority_path, parent.path().join("displaced-authority"))
            .expect("displace manifest directory");
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&authority_path)
            .expect("create replacement manifest directory");

        assert!(matches!(
            lease.validate(),
            Err(DomainReconnectManifestError::IdentityChanged {
                operation: "directory revalidation"
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_authority_slot_is_rejected() {
        let temp = tempfile::tempdir().expect("temporary directory");
        set_intent_at(
            temp.path(),
            "trj",
            DomainAttachmentIntent::Attached,
        )
        .expect("persist manifest");
        std::fs::hard_link(
            &manifest_paths(temp.path())[0],
            temp.path().join("unexpected-hard-link"),
        )
        .expect("create hard-link negative control");

        assert!(matches!(
            load_from(temp.path()),
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
