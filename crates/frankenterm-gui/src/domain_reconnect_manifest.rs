//! Crash-consistent attachment intent for configured client domains.
//!
//! Domain names are configuration locators and can reveal host naming.  This
//! authority therefore persists only a domain-separated SHA-256 fingerprint.
//! Absence means "follow configuration"; an explicit attached or detached
//! record overrides the configured auto-connect bit until the operator makes
//! the opposite choice.

use cap_fs_ext::{DirExt as _, FollowSymlinks, OpenOptionsFollowExt as _};
#[cfg(unix)]
use cap_std::fs::DirBuilderExt as _;
#[cfg(windows)]
use cap_std::fs::MetadataExt as CapWindowsMetadataExt;
use cap_std::fs::{
    Dir as CapDir, DirBuilder as CapDirBuilder, File as CapFile, Metadata as CapMetadata,
};
#[cfg(unix)]
use cap_std::fs::{MetadataExt as CapUnixMetadataExt, PermissionsExt as _};
use cap_std::fs::{OpenOptions as CapOpenOptions, OpenOptionsExt as _};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use thiserror::Error;

const MAGIC: &[u8; 8] = b"FTDDOM01";
const LEGACY_SCHEMA_VERSION: u32 = 1;
const SCHEMA_VERSION: u32 = 2;
const HEADER_BYTES: usize = 32;
const ENTRY_BYTES: usize = 33;
const DIGEST_BYTES: usize = 32;
const MAX_DOMAIN_INTENTS: usize = 4_096;
const MAX_MANIFEST_BYTES: u64 =
    (HEADER_BYTES + MAX_DOMAIN_INTENTS * ENTRY_BYTES + DIGEST_BYTES) as u64;
const FINGERPRINT_DOMAIN: &[u8] = b"frankenterm.gui.domain-reconnect-name.v1\0";
const LEGACY_CHECKSUM_DOMAIN: &[u8] = b"frankenterm.gui.domain-reconnect-manifest.v1\0";
const CHECKSUM_DOMAIN: &[u8] = b"frankenterm.gui.domain-reconnect-manifest.v2\0";
const SLOT_NAMES: [&str; 3] = [
    "domain-reconnect-manifest.slot-0",
    "domain-reconnect-manifest.slot-1",
    "domain-reconnect-manifest.slot-2",
];
const LOCK_NAME: &str = "domain-reconnect-manifest.lock";
const PRIVATE_AUTHORITY_DIRECTORY: &str = config::DATA_ARTIFACT_DOMAIN_RECONNECT_PRIVATE;
const NAMESPACE_MIGRATION_INTENT_NAME: &str = "namespace-migration-intent-v1";
const NAMESPACE_MIGRATION_COMPLETE_NAME: &str = "namespace-migration-complete-v1";
static NAMESPACE_MIGRATION_FAILURE: OnceLock<DomainReconnectManifestError> = OnceLock::new();

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

#[derive(Clone, Debug, Error)]
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
    #[error(
        "domain reconnect manifest rolled back to generation {observed} below the retained generation {retained}"
    )]
    AuthorityRollback { observed: u64, retained: u64 },
    #[error(
        "domain reconnect manifest differs from the retained authority at generation {generation}"
    )]
    AuthorityDivergence { generation: u64 },
    #[error("domain reconnect manifest has no authoritative two-replica quorum")]
    NoQuorum,
    #[error("domain reconnect manifest legacy authority is ambiguous")]
    LegacyAmbiguous,
    #[error("domain reconnect manifest namespaces both contain authority evidence")]
    NamespaceDivergence,
    #[error("domain reconnect manifest namespace changed while selecting authority")]
    NamespaceChanged,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DecodedManifest {
    schema_version: u32,
    manifest: DomainReconnectManifest,
}

enum V2Authority {
    Quorum(DomainReconnectManifest),
    Migration {
        manifest: DomainReconnectManifest,
        legacy_anchor: usize,
    },
}

struct LegacySelection {
    manifest: DomainReconnectManifest,
    active_slot: usize,
}

enum LegacyAuthority {
    Pristine,
    Published(LegacySelection),
}

enum SlotRead {
    Missing,
    Empty,
    Valid(DecodedManifest),
    Invalid(DomainReconnectManifestError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NamespaceSlotEvidence {
    None,
    Present,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProductionNamespace {
    LegacyRoot,
    PrivateLeaf,
}

#[must_use]
pub fn fingerprint_domain_name(domain_name: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(FINGERPRINT_DOMAIN);
    digest.update(domain_name.as_bytes());
    digest.finalize().into()
}

#[cfg(test)]
fn manifest_paths(directory: &Path) -> [PathBuf; 3] {
    [
        directory.join(SLOT_NAMES[0]),
        directory.join(SLOT_NAMES[1]),
        directory.join(SLOT_NAMES[2]),
    ]
}

#[cfg(test)]
fn lock_path(directory: &Path) -> PathBuf {
    directory.join(LOCK_NAME)
}

fn private_manifest_directory(data_directory: &Path) -> PathBuf {
    data_directory.join(PRIVATE_AUTHORITY_DIRECTORY)
}

fn open_existing_directory_nofollow(
    directory: &Path,
) -> Result<CapDir, DomainReconnectManifestError> {
    let Some(name) = directory.file_name() else {
        return CapDir::open_ambient_dir(directory, cap_std::ambient_authority())
            .map_err(|error| DomainReconnectManifestError::io("open manifest directory", error));
    };
    let parent_path = directory
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = CapDir::open_ambient_dir(parent_path, cap_std::ambient_authority())
        .map_err(|error| DomainReconnectManifestError::io("open parent directory", error))?;
    parent
        .open_dir_nofollow(name)
        .map_err(|error| DomainReconnectManifestError::io("open manifest directory", error))
}

fn probe_namespace_slot_evidence(
    directory: &Path,
) -> Result<NamespaceSlotEvidence, DomainReconnectManifestError> {
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(NamespaceSlotEvidence::None);
        }
        Err(error) => {
            return Err(DomainReconnectManifestError::io(
                "inspect namespace directory",
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DomainReconnectManifestError::UnsafeFile {
            reason: "namespace path is not a direct directory",
        });
    }

    let pinned = open_existing_directory_nofollow(directory)?;
    validate_pinned_directory_identity(directory, &pinned)?;
    for name in SLOT_NAMES {
        match pinned.symlink_metadata(name) {
            Ok(_) => return Ok(NamespaceSlotEvidence::Present),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(DomainReconnectManifestError::io(
                    "inspect namespace authority slot",
                    error,
                ));
            }
        }
    }
    validate_pinned_directory_identity(directory, &pinned)?;
    Ok(NamespaceSlotEvidence::None)
}

fn select_production_namespace(
    legacy_root: NamespaceSlotEvidence,
    private_leaf: NamespaceSlotEvidence,
) -> Result<ProductionNamespace, DomainReconnectManifestError> {
    match (legacy_root, private_leaf) {
        (NamespaceSlotEvidence::Present, NamespaceSlotEvidence::Present) => {
            Err(DomainReconnectManifestError::NamespaceDivergence)
        }
        (NamespaceSlotEvidence::Present, NamespaceSlotEvidence::None) => {
            Ok(ProductionNamespace::LegacyRoot)
        }
        (NamespaceSlotEvidence::None, _) => Ok(ProductionNamespace::PrivateLeaf),
    }
}

fn legacy_root_can_hold_selection_lease(
    data_directory: &Path,
) -> Result<bool, DomainReconnectManifestError> {
    let metadata = match std::fs::symlink_metadata(data_directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(DomainReconnectManifestError::io(
                "inspect legacy namespace directory",
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DomainReconnectManifestError::UnsafeFile {
            reason: "legacy namespace path is not a direct directory",
        });
    }
    #[cfg(unix)]
    {
        Ok(metadata.permissions().mode() & 0o7777 == 0o700
            && metadata.uid() == rustix::process::geteuid().as_raw())
    }
    #[cfg(not(unix))]
    {
        Ok(true)
    }
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

fn open_or_create_directory_tree_durably(
    directory: &Path,
) -> Result<CapDir, DomainReconnectManifestError> {
    let exists = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(DomainReconnectManifestError::UnsafeFile {
                    reason: "manifest directory is not a direct directory",
                });
            }
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(DomainReconnectManifestError::io(
                "inspect manifest directory",
                error,
            ));
        }
    };

    let Some(name) = directory.file_name() else {
        if !exists {
            return Err(DomainReconnectManifestError::UnsafeFile {
                reason: "manifest directory has no terminal name",
            });
        }
        return CapDir::open_ambient_dir(directory, cap_std::ambient_authority())
            .map_err(|error| DomainReconnectManifestError::io("open manifest directory", error));
    };
    let parent_path = directory
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = if exists {
        CapDir::open_ambient_dir(parent_path, cap_std::ambient_authority())
            .map_err(|error| DomainReconnectManifestError::io("open parent directory", error))?
    } else {
        open_or_create_directory_tree_durably(parent_path)?
    };

    if !exists {
        let mut builder = CapDirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match parent.create_dir_with(name, &builder) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(DomainReconnectManifestError::io(
                    "create manifest directory",
                    error,
                ));
            }
        }
    }
    let pinned = parent
        .open_dir_nofollow(name)
        .map_err(|error| DomainReconnectManifestError::io("open manifest directory", error))?;

    // The child cannot be called durable until the directory entry that names it
    // is itself synchronized. This is required on the existing-directory path
    // too: the winning creator may have reached mkdir and then crashed before
    // making the shared entry durable.
    sync_directory(&parent)?;
    Ok(pinned)
}

fn open_manifest_directory(directory: &Path) -> Result<CapDir, DomainReconnectManifestError> {
    let pinned = open_or_create_directory_tree_durably(directory)?;
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
        let directory_metadata = directory.dir_metadata().map_err(|error| {
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
    let pinned_metadata = validate_pinned_directory_identity(path, directory)?;
    validate_private_file_owner(&pinned_metadata)?;
    Ok(())
}

fn validate_pinned_directory_identity(
    path: &Path,
    directory: &CapDir,
) -> Result<CapMetadata, DomainReconnectManifestError> {
    let named = std::fs::symlink_metadata(path)
        .map_err(|error| DomainReconnectManifestError::io("reinspect manifest directory", error))?;
    if named.file_type().is_symlink() || !named.is_dir() {
        return Err(DomainReconnectManifestError::IdentityChanged {
            operation: "directory revalidation",
        });
    }
    let reopened = open_existing_directory_nofollow(path)?;
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
    Ok(pinned_metadata)
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
    fn acquire(path: &Path, exclusive: bool) -> Result<Self, DomainReconnectManifestError> {
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
                return Err(DomainReconnectManifestError::io("inspect lock", error));
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
        let opened = validate_opened_name(&directory, name, &lock_authority, "lock open")?;
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

fn checksum_domain(schema_version: u32) -> Result<&'static [u8], DomainReconnectManifestError> {
    match schema_version {
        LEGACY_SCHEMA_VERSION => Ok(LEGACY_CHECKSUM_DOMAIN),
        SCHEMA_VERSION => Ok(CHECKSUM_DOMAIN),
        found => Err(DomainReconnectManifestError::UnsupportedVersion { found }),
    }
}

fn encode_manifest_for_schema(
    manifest: &DomainReconnectManifest,
    schema_version: u32,
) -> Result<Vec<u8>, DomainReconnectManifestError> {
    let checksum_domain = checksum_domain(schema_version)?;
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
    encoded.extend_from_slice(&schema_version.to_le_bytes());
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
    checksum.update(checksum_domain);
    checksum.update(&encoded);
    encoded.extend_from_slice(&checksum.finalize());
    Ok(encoded)
}

fn encode_manifest(
    manifest: &DomainReconnectManifest,
) -> Result<Vec<u8>, DomainReconnectManifestError> {
    encode_manifest_for_schema(manifest, SCHEMA_VERSION)
}

fn decode_manifest(bytes: &[u8]) -> Result<DecodedManifest, DomainReconnectManifestError> {
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
    let checksum_domain = checksum_domain(schema)?;
    if bytes[12..16] != [0; 4] || bytes[28..32] != [0; 4] {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "reserved header bytes are nonzero",
        });
    }
    let generation = u64::from_le_bytes(bytes[16..24].try_into().expect("fixed generation slice"));
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
    checksum.update(checksum_domain);
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
    Ok(DecodedManifest {
        schema_version: schema,
        manifest: DomainReconnectManifest {
            generation,
            intents,
        },
    })
}

fn read_slot(directory: &CapDir, name: &OsStr) -> Result<SlotRead, DomainReconnectManifestError> {
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
        return Ok(SlotRead::Invalid(DomainReconnectManifestError::Oversized {
            actual: before.len(),
            maximum: MAX_MANIFEST_BYTES,
        }));
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
    let capacity =
        usize::try_from(opened.len()).map_err(|_| DomainReconnectManifestError::Oversized {
            actual: opened.len(),
            maximum: MAX_MANIFEST_BYTES,
        })?;
    let mut bytes = Vec::with_capacity(capacity);
    (&mut file)
        .take(MAX_MANIFEST_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| DomainReconnectManifestError::io("read authority slot", error))?;
    let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual > MAX_MANIFEST_BYTES {
        return Ok(SlotRead::Invalid(DomainReconnectManifestError::Oversized {
            actual,
            maximum: MAX_MANIFEST_BYTES,
        }));
    }
    let after = match validate_opened_name(directory, name, &file, "slot read completion") {
        Ok(metadata) => metadata,
        Err(error) => return Ok(SlotRead::Invalid(error)),
    };
    if actual != opened.len() || actual != after.len() || !same_file_identity(&opened, &after) {
        return Ok(SlotRead::Invalid(DomainReconnectManifestError::Invalid {
            reason: "authority slot length changed while reading",
        }));
    }
    if bytes.is_empty() {
        return Ok(SlotRead::Empty);
    }
    Ok(match decode_manifest(&bytes) {
        Ok(manifest) => SlotRead::Valid(manifest),
        Err(error) => SlotRead::Invalid(error),
    })
}

fn valid_record(slot: &SlotRead) -> Option<&DecodedManifest> {
    match slot {
        SlotRead::Valid(record) => Some(record),
        SlotRead::Missing | SlotRead::Empty | SlotRead::Invalid(_) => None,
    }
}

fn slot_matches_v2(slot: &SlotRead, manifest: &DomainReconnectManifest) -> bool {
    matches!(
        slot,
        SlotRead::Valid(record)
            if record.schema_version == SCHEMA_VERSION && record.manifest == *manifest
    )
}

fn select_v2_authority(
    slots: &[SlotRead; 3],
) -> Result<Option<V2Authority>, DomainReconnectManifestError> {
    let v2_count = slots
        .iter()
        .filter_map(valid_record)
        .filter(|record| record.schema_version == SCHEMA_VERSION)
        .count();
    if v2_count == 0 {
        return Ok(None);
    }

    // A completed v2 state is identified by content, never merely by the
    // numerically greatest surviving generation.  This is what makes a torn
    // second publication fail closed instead of reviving either side.
    for candidate in slots
        .iter()
        .filter_map(valid_record)
        .filter(|record| record.schema_version == SCHEMA_VERSION)
    {
        let replicas = slots
            .iter()
            .filter_map(valid_record)
            .filter(|record| {
                record.schema_version == SCHEMA_VERSION && record.manifest == candidate.manifest
            })
            .count();
        if replicas >= 2 {
            return Ok(Some(V2Authority::Quorum(candidate.manifest.clone())));
        }
    }

    // The sole singleton exception is the first migration publication.  It is
    // authoritative only together with an exact schema-v1 content replica,
    // and only while no valid record contains a newer or divergent peer state.
    if v2_count == 1 {
        let candidate = slots
            .iter()
            .filter_map(valid_record)
            .find(|record| record.schema_version == SCHEMA_VERSION)
            .expect("a v2 record was counted");
        let legacy_anchor = slots.iter().enumerate().find_map(|(index, slot)| {
            let record = valid_record(slot)?;
            (record.schema_version == LEGACY_SCHEMA_VERSION
                && record.manifest == candidate.manifest)
                .then_some(index)
        });
        let is_newest_unambiguous_state = slots.iter().filter_map(valid_record).all(|record| {
            record.manifest.generation < candidate.manifest.generation
                || record.manifest == candidate.manifest
        });
        if let Some(legacy_anchor) = legacy_anchor.filter(|_| is_newest_unambiguous_state) {
            return Ok(Some(V2Authority::Migration {
                manifest: candidate.manifest.clone(),
                legacy_anchor,
            }));
        }
    }

    Err(DomainReconnectManifestError::NoQuorum)
}

fn select_legacy_authority(
    slots: [SlotRead; 3],
) -> Result<LegacyAuthority, DomainReconnectManifestError> {
    let [first, second, third] = slots;
    let third_is_missing = match third {
        SlotRead::Missing => true,
        SlotRead::Empty | SlotRead::Invalid(DomainReconnectManifestError::Invalid { .. }) => false,
        SlotRead::Invalid(error) => return Err(error),
        SlotRead::Valid(_) => return Err(DomainReconnectManifestError::LegacyAmbiguous),
    };

    match (first, second) {
        (SlotRead::Valid(first), SlotRead::Valid(second)) => {
            if first.schema_version != LEGACY_SCHEMA_VERSION
                || second.schema_version != LEGACY_SCHEMA_VERSION
            {
                return Err(DomainReconnectManifestError::LegacyAmbiguous);
            }
            if first.manifest.generation > second.manifest.generation {
                Ok(LegacyAuthority::Published(LegacySelection {
                    manifest: first.manifest,
                    active_slot: 0,
                }))
            } else if second.manifest.generation > first.manifest.generation {
                Ok(LegacyAuthority::Published(LegacySelection {
                    manifest: second.manifest,
                    active_slot: 1,
                }))
            } else if first.manifest == second.manifest {
                Ok(LegacyAuthority::Published(LegacySelection {
                    manifest: first.manifest,
                    active_slot: 0,
                }))
            } else {
                Err(DomainReconnectManifestError::AmbiguousGeneration {
                    generation: first.manifest.generation,
                })
            }
        }
        (SlotRead::Valid(first), SlotRead::Missing)
            if first.schema_version == LEGACY_SCHEMA_VERSION && first.manifest.generation == 1 =>
        {
            Ok(LegacyAuthority::Published(LegacySelection {
                manifest: first.manifest,
                active_slot: 0,
            }))
        }
        (SlotRead::Invalid(error), _) | (_, SlotRead::Invalid(error)) => Err(error),
        (SlotRead::Missing, SlotRead::Missing) if third_is_missing => Ok(LegacyAuthority::Pristine),
        (
            SlotRead::Missing | SlotRead::Empty | SlotRead::Valid(_),
            SlotRead::Missing | SlotRead::Empty | SlotRead::Valid(_),
        ) => Err(DomainReconnectManifestError::LegacyAmbiguous),
    }
}

fn verify_fully_replicated(
    directory: &CapDir,
    manifest: &DomainReconnectManifest,
) -> Result<(), DomainReconnectManifestError> {
    for name in SLOT_NAMES {
        match read_slot(directory, OsStr::new(name))? {
            slot if slot_matches_v2(&slot, manifest) => {}
            SlotRead::Invalid(error) => return Err(error),
            SlotRead::Missing | SlotRead::Empty | SlotRead::Valid(_) => {
                return Err(DomainReconnectManifestError::NoQuorum);
            }
        }
    }
    Ok(())
}

fn repair_from_v2_quorum(
    directory: &CapDir,
    slots: &[SlotRead; 3],
    manifest: &DomainReconnectManifest,
) -> Result<(), DomainReconnectManifestError> {
    for (index, slot) in slots.iter().enumerate() {
        if !slot_matches_v2(slot, manifest) {
            write_slot(directory, OsStr::new(SLOT_NAMES[index]), manifest)?;
        }
    }
    verify_fully_replicated(directory, manifest)
}

fn complete_cross_schema_migration(
    directory: &CapDir,
    slots: &[SlotRead; 3],
    manifest: &DomainReconnectManifest,
    legacy_anchor: usize,
) -> Result<(), DomainReconnectManifestError> {
    for (index, slot) in slots.iter().enumerate() {
        if index != legacy_anchor && !slot_matches_v2(slot, manifest) {
            write_slot(directory, OsStr::new(SLOT_NAMES[index]), manifest)?;
        }
    }
    write_slot(directory, OsStr::new(SLOT_NAMES[legacy_anchor]), manifest)?;
    verify_fully_replicated(directory, manifest)
}

fn migrate_legacy_authority(
    directory: &CapDir,
    selection: &LegacySelection,
) -> Result<(), DomainReconnectManifestError> {
    let stale_slot = match selection.active_slot {
        0 => 1,
        1 => 0,
        _ => unreachable!("legacy authority has exactly two slots"),
    };
    // Slot 2 creates the cross-schema content quorum.  Converting the stale
    // legacy slot next creates a native v2 quorum before the active legacy
    // replica is touched.
    for index in [2, stale_slot, selection.active_slot] {
        write_slot(
            directory,
            OsStr::new(SLOT_NAMES[index]),
            &selection.manifest,
        )?;
    }
    verify_fully_replicated(directory, &selection.manifest)
}

fn validate_retained_authority(
    manifest: &DomainReconnectManifest,
    retained: Option<&DomainReconnectManifest>,
) -> Result<(), DomainReconnectManifestError> {
    let Some(retained) = retained else {
        return Ok(());
    };
    if manifest.generation < retained.generation {
        return Err(DomainReconnectManifestError::AuthorityRollback {
            observed: manifest.generation,
            retained: retained.generation,
        });
    }
    if manifest.generation == retained.generation && manifest != retained {
        return Err(DomainReconnectManifestError::AuthorityDivergence {
            generation: manifest.generation,
        });
    }
    Ok(())
}

fn validate_locked_authority_without_repair(
    directory: &CapDir,
) -> Result<(), DomainReconnectManifestError> {
    let slots = [
        read_slot(directory, OsStr::new(SLOT_NAMES[0]))?,
        read_slot(directory, OsStr::new(SLOT_NAMES[1]))?,
        read_slot(directory, OsStr::new(SLOT_NAMES[2]))?,
    ];
    if select_v2_authority(&slots)?.is_some() {
        return Ok(());
    }
    match select_legacy_authority(slots)? {
        LegacyAuthority::Published(_) => Ok(()),
        LegacyAuthority::Pristine => Err(DomainReconnectManifestError::NamespaceChanged),
    }
}

struct ProductionManifestLease {
    // Field order makes the inner authority lease drop before the outer legacy
    // selection guard. Acquisition is always legacy root, then private leaf.
    authority: ManifestLease,
    legacy_root_guard: Option<ManifestLease>,
    data_directory: PathBuf,
    private_directory: PathBuf,
    selected: ProductionNamespace,
}

impl ProductionManifestLease {
    fn acquire(data_directory: &Path) -> Result<Self, DomainReconnectManifestError> {
        let private_directory = private_manifest_directory(data_directory);
        let initial_legacy = probe_namespace_slot_evidence(data_directory)?;
        let initial_private = probe_namespace_slot_evidence(&private_directory)?;
        let selected = select_production_namespace(initial_legacy, initial_private)?;

        match selected {
            ProductionNamespace::LegacyRoot => {
                let authority = ManifestLease::acquire(data_directory, true)?;
                let locked_legacy = probe_namespace_slot_evidence(data_directory)?;
                let locked_private = probe_namespace_slot_evidence(&private_directory)?;
                if select_production_namespace(locked_legacy, locked_private)? != selected {
                    return Err(DomainReconnectManifestError::NamespaceChanged);
                }
                // Detect corrupt, unsafe, or ambiguous legacy authority before
                // creating or touching anything in the private namespace.
                validate_locked_authority_without_repair(&authority.directory)?;
                Ok(Self {
                    authority,
                    legacy_root_guard: None,
                    data_directory: data_directory.to_path_buf(),
                    private_directory,
                    selected,
                })
            }
            ProductionNamespace::PrivateLeaf => {
                // A private legacy root is the cross-version selection lock.
                // A broader legacy DATA_DIR cannot be used by the schema-v1
                // writer because that writer enforces the same 0700 contract;
                // it therefore remains unmodified while we create the private
                // leaf. This preserves the global root -> private lock order.
                let legacy_root_guard = if legacy_root_can_hold_selection_lease(data_directory)? {
                    Some(ManifestLease::acquire(data_directory, true)?)
                } else {
                    None
                };
                let authority = ManifestLease::acquire(&private_directory, true)?;
                let locked_legacy = probe_namespace_slot_evidence(data_directory)?;
                let locked_private = probe_namespace_slot_evidence(&private_directory)?;
                if select_production_namespace(locked_legacy, locked_private)? != selected
                    || (initial_private == NamespaceSlotEvidence::Present
                        && locked_private != NamespaceSlotEvidence::Present)
                {
                    return Err(DomainReconnectManifestError::NamespaceChanged);
                }
                Ok(Self {
                    authority,
                    legacy_root_guard,
                    data_directory: data_directory.to_path_buf(),
                    private_directory,
                    selected,
                })
            }
        }
    }

    fn validate(&self) -> Result<(), DomainReconnectManifestError> {
        if let Some(legacy_root_guard) = &self.legacy_root_guard {
            legacy_root_guard.validate()?;
        }
        self.authority.validate()?;
        let legacy = probe_namespace_slot_evidence(&self.data_directory)?;
        let private = probe_namespace_slot_evidence(&self.private_directory)?;
        if select_production_namespace(legacy, private)? != self.selected {
            return Err(DomainReconnectManifestError::NamespaceChanged);
        }
        Ok(())
    }
}

fn load_locked(
    directory: &CapDir,
    retained: Option<&DomainReconnectManifest>,
) -> Result<LoadedManifest, DomainReconnectManifestError> {
    let slots = [
        read_slot(directory, OsStr::new(SLOT_NAMES[0]))?,
        read_slot(directory, OsStr::new(SLOT_NAMES[1]))?,
        read_slot(directory, OsStr::new(SLOT_NAMES[2]))?,
    ];
    match select_v2_authority(&slots)? {
        Some(V2Authority::Quorum(manifest)) => {
            // Reject rollback or same-generation replacement before repairing
            // any replica. This preserves a surviving retained-generation
            // slot for diagnosis/recovery instead of overwriting it from a
            // stale two-replica quorum.
            validate_retained_authority(&manifest, retained)?;
            repair_from_v2_quorum(directory, &slots, &manifest)?;
            Ok(LoadedManifest { manifest })
        }
        Some(V2Authority::Migration {
            manifest,
            legacy_anchor,
        }) => {
            validate_retained_authority(&manifest, retained)?;
            complete_cross_schema_migration(directory, &slots, &manifest, legacy_anchor)?;
            Ok(LoadedManifest { manifest })
        }
        None => match select_legacy_authority(slots)? {
            LegacyAuthority::Pristine => {
                let manifest = DomainReconnectManifest::default();
                validate_retained_authority(&manifest, retained)?;
                Ok(LoadedManifest { manifest })
            }
            LegacyAuthority::Published(selection) => {
                validate_retained_authority(&selection.manifest, retained)?;
                migrate_legacy_authority(directory, &selection)?;
                Ok(LoadedManifest {
                    manifest: selection.manifest,
                })
            }
        },
    }
}

pub fn load_from(
    directory: &Path,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    load_fenced_from(directory, None)
}

fn load_fenced_from(
    directory: &Path,
    retained: Option<&DomainReconnectManifest>,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let lease = ManifestLease::acquire(directory, true)?;
    let loaded = load_locked(&lease.directory, retained)?;
    lease.validate()?;
    Ok(loaded.manifest)
}

fn load_production_from(
    data_directory: &Path,
    retained: Option<&DomainReconnectManifest>,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let lease = ProductionManifestLease::acquire(data_directory)?;
    let loaded = load_locked(&lease.authority.directory, retained)?;
    lease.validate()?;
    Ok(loaded.manifest)
}

fn finish_namespace_migration(
    directory: &CapDir,
    intended: &DomainReconnectManifest,
) -> Result<(), DomainReconnectManifestError> {
    match read_slot(directory, OsStr::new(NAMESPACE_MIGRATION_COMPLETE_NAME))? {
        SlotRead::Valid(decoded)
            if decoded.schema_version == SCHEMA_VERSION && decoded.manifest == *intended =>
        {
            Ok(())
        }
        SlotRead::Valid(_) => Err(DomainReconnectManifestError::NamespaceDivergence),
        // Completion is written only after all three replicas verify against
        // the retained intent. An interrupted empty/invalid completion is
        // therefore safe to reconstruct from those still-held authorities.
        SlotRead::Missing | SlotRead::Empty | SlotRead::Invalid(_) => write_slot(
            directory,
            OsStr::new(NAMESPACE_MIGRATION_COMPLETE_NAME),
            intended,
        ),
    }
}

fn canonical_slots_admit_migration(
    directory: &CapDir,
    intended: &DomainReconnectManifest,
) -> Result<bool, DomainReconnectManifestError> {
    let mut fully_replicated = true;
    for name in SLOT_NAMES {
        match read_slot(directory, OsStr::new(name))? {
            SlotRead::Missing | SlotRead::Empty | SlotRead::Invalid(_) => {
                fully_replicated = false;
            }
            SlotRead::Valid(decoded)
                if decoded.schema_version == SCHEMA_VERSION && decoded.manifest == *intended => {}
            SlotRead::Valid(_) => {
                return Err(DomainReconnectManifestError::NamespaceDivergence);
            }
        }
    }
    Ok(fully_replicated)
}

fn canonical_slots_are_all_missing(
    directory: &CapDir,
) -> Result<bool, DomainReconnectManifestError> {
    for name in SLOT_NAMES {
        if !matches!(read_slot(directory, OsStr::new(name))?, SlotRead::Missing) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn migrate_legacy_data_namespace_at(
    legacy_data_directory: &Path,
    canonical_data_directory: &Path,
) -> Result<bool, DomainReconnectManifestError> {
    if legacy_data_directory == canonical_data_directory {
        return Ok(false);
    }
    for name in SLOT_NAMES {
        let root_relative = Path::new(name);
        let private_relative = Path::new(config::DATA_ARTIFACT_DOMAIN_RECONNECT_PRIVATE).join(name);
        if config::legacy_data_artifact_treatment(root_relative)
            != config::LegacyDataArtifactTreatment::MigrateValidatedState
            || config::legacy_data_artifact_treatment(&private_relative)
                != config::LegacyDataArtifactTreatment::MigrateValidatedState
        {
            return Err(DomainReconnectManifestError::Invalid {
                reason: "domain reconnect migration is absent from the enforced artifact inventory",
            });
        }
    }

    let legacy_root = probe_namespace_slot_evidence(legacy_data_directory)?;
    let legacy_private =
        probe_namespace_slot_evidence(&private_manifest_directory(legacy_data_directory))?;
    if legacy_root == NamespaceSlotEvidence::None && legacy_private == NamespaceSlotEvidence::None {
        return Ok(false);
    }
    // Reject a divergent legacy namespace before creating canonical state.
    select_production_namespace(legacy_root, legacy_private)?;

    // Both roots use the same ProductionManifestLease protocol. Acquire them
    // in lexical order so concurrent old/new launchers cannot deadlock while
    // the validated snapshot crosses namespaces.
    let (first_path, second_path, canonical_is_first) =
        if canonical_data_directory <= legacy_data_directory {
            (canonical_data_directory, legacy_data_directory, true)
        } else {
            (legacy_data_directory, canonical_data_directory, false)
        };
    let first = ProductionManifestLease::acquire(first_path)?;
    let second = ProductionManifestLease::acquire(second_path)?;
    let (canonical, legacy) = if canonical_is_first {
        (&first, &second)
    } else {
        (&second, &first)
    };

    let intent_slot = read_slot(
        &canonical.authority.directory,
        OsStr::new(NAMESPACE_MIGRATION_INTENT_NAME),
    )?;
    let intended = match intent_slot {
        SlotRead::Valid(decoded) if decoded.schema_version == SCHEMA_VERSION => {
            let intent = decoded.manifest;
            let legacy_loaded = load_locked(&legacy.authority.directory, None)?;
            if legacy_loaded.manifest != intent {
                return Err(DomainReconnectManifestError::NamespaceDivergence);
            }
            match read_slot(
                &canonical.authority.directory,
                OsStr::new(NAMESPACE_MIGRATION_COMPLETE_NAME),
            )? {
                SlotRead::Valid(completed)
                    if completed.schema_version == SCHEMA_VERSION
                        && completed.manifest == intent =>
                {
                    let completed = completed.manifest;
                    let canonical_loaded = load_locked(&canonical.authority.directory, None)?;
                    if canonical_loaded.manifest.generation < completed.generation
                        || (canonical_loaded.manifest.generation == completed.generation
                            && canonical_loaded.manifest != completed)
                    {
                        return Err(DomainReconnectManifestError::NamespaceDivergence);
                    }
                    canonical.validate()?;
                    legacy.validate()?;
                    return Ok(false);
                }
                SlotRead::Valid(_) => {
                    return Err(DomainReconnectManifestError::NamespaceDivergence);
                }
                SlotRead::Missing | SlotRead::Empty | SlotRead::Invalid(_) => {}
            }
            // A prior process may have stopped after any individual replica
            // write. Validate every surviving byte against the durable intent
            // before repairing; never ask the ordinary quorum selector to
            // interpret a deliberately incomplete migration publication.
            if canonical_slots_admit_migration(&canonical.authority.directory, &intent)? {
                finish_namespace_migration(&canonical.authority.directory, &intent)?;
                canonical.validate()?;
                legacy.validate()?;
                return Ok(false);
            }
            intent
        }
        SlotRead::Missing => {
            let canonical_loaded = load_locked(&canonical.authority.directory, None)?;
            if canonical_loaded.manifest.generation > 0 {
                canonical.validate()?;
                legacy.validate()?;
                return Ok(false);
            }
            let legacy_loaded = load_locked(&legacy.authority.directory, None)?;
            if legacy_loaded.manifest.generation == 0 {
                canonical.validate()?;
                legacy.validate()?;
                return Ok(false);
            }
            write_slot(
                &canonical.authority.directory,
                OsStr::new(NAMESPACE_MIGRATION_INTENT_NAME),
                &legacy_loaded.manifest,
            )?;
            legacy_loaded.manifest
        }
        SlotRead::Empty | SlotRead::Invalid(_) => {
            // Intent publication precedes every canonical replica. Therefore
            // only an otherwise untouched canonical slot set can prove that
            // an invalid marker is an interrupted intent write rather than a
            // conflict with pre-existing canonical evidence.
            if !canonical_slots_are_all_missing(&canonical.authority.directory)? {
                return Err(DomainReconnectManifestError::NamespaceDivergence);
            }
            let legacy_loaded = load_locked(&legacy.authority.directory, None)?;
            if legacy_loaded.manifest.generation == 0 {
                return Err(DomainReconnectManifestError::Invalid {
                    reason: "interrupted namespace migration has no legacy authority",
                });
            }
            write_slot(
                &canonical.authority.directory,
                OsStr::new(NAMESPACE_MIGRATION_INTENT_NAME),
                &legacy_loaded.manifest,
            )?;
            legacy_loaded.manifest
        }
        SlotRead::Valid(_) => {
            return Err(DomainReconnectManifestError::NamespaceDivergence);
        }
    };

    // The durable intent is recovery authority for interruption after any
    // individual replica write. It is retained permanently; neither a retry
    // nor a successful migration deletes legacy or intent evidence.
    canonical_slots_admit_migration(&canonical.authority.directory, &intended)?;
    for name in SLOT_NAMES {
        write_slot(&canonical.authority.directory, OsStr::new(name), &intended)?;
    }
    verify_fully_replicated(&canonical.authority.directory, &intended)?;
    finish_namespace_migration(&canonical.authority.directory, &intended)?;
    canonical.validate()?;
    legacy.validate()?;
    Ok(true)
}

fn namespace_migration_result_from<F>(
    failure_fence: &OnceLock<DomainReconnectManifestError>,
    migrate: F,
) -> Result<bool, DomainReconnectManifestError>
where
    F: FnOnce() -> Result<bool, DomainReconnectManifestError>,
{
    if let Some(error) = failure_fence.get() {
        return Err(error.clone());
    }
    match migrate() {
        Ok(migrated) => match failure_fence.get() {
            Some(error) => Err(error.clone()),
            None => Ok(migrated),
        },
        Err(error) => {
            let _ = failure_fence.set(error.clone());
            Err(failure_fence.get().cloned().unwrap_or(error))
        }
    }
}

fn migrate_legacy_data_namespace() -> Result<bool, DomainReconnectManifestError> {
    namespace_migration_result_from(&NAMESPACE_MIGRATION_FAILURE, || {
        migrate_legacy_data_namespace_at(&config::legacy_data_dir(), config::DATA_DIR.as_path())
    })
}

pub fn load() -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    if migrate_legacy_data_namespace()? {
        log::info!(
            "domain reconnect: migrated validated legacy authority into {} without removing legacy evidence",
            config::DATA_DIR.display()
        );
    }
    load_production_from(config::DATA_DIR.as_path(), None)
}

/// Load and repair only authority at or beyond the exact in-process retained
/// manifest. Lower or same-generation-divergent evidence is rejected before
/// any on-disk replica is changed.
pub fn load_fenced(
    retained: Option<&DomainReconnectManifest>,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    migrate_legacy_data_namespace()?;
    load_production_from(config::DATA_DIR.as_path(), retained)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotWriteInterruption {
    None,
    #[cfg(test)]
    AfterTruncate,
    #[cfg(test)]
    AfterPartialWrite,
    #[cfg(test)]
    AfterFullWrite,
    #[cfg(test)]
    AfterSync,
    #[cfg(test)]
    AfterDirectorySync,
}

fn write_slot(
    directory: &CapDir,
    name: &OsStr,
    manifest: &DomainReconnectManifest,
) -> Result<(), DomainReconnectManifestError> {
    write_slot_with_interruption(directory, name, manifest, SlotWriteInterruption::None)
}

fn write_slot_with_interruption(
    directory: &CapDir,
    name: &OsStr,
    manifest: &DomainReconnectManifest,
    interruption: SlotWriteInterruption,
) -> Result<(), DomainReconnectManifestError> {
    let _ = interruption;
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
    #[cfg(test)]
    if interruption == SlotWriteInterruption::AfterTruncate {
        return Err(DomainReconnectManifestError::io(
            "injected slot interruption after truncate",
            io::Error::other("injected slot interruption"),
        ));
    }
    #[cfg(test)]
    if interruption == SlotWriteInterruption::AfterPartialWrite {
        file.write_all(&encoded[..encoded.len() / 2])
            .map_err(|error| {
                DomainReconnectManifestError::io("write partial authority slot", error)
            })?;
        return Err(DomainReconnectManifestError::io(
            "injected slot interruption during write",
            io::Error::other("injected slot interruption"),
        ));
    }
    file.write_all(&encoded)
        .map_err(|error| DomainReconnectManifestError::io("write authority slot", error))?;
    #[cfg(test)]
    if interruption == SlotWriteInterruption::AfterFullWrite {
        return Err(DomainReconnectManifestError::io(
            "injected slot interruption before sync",
            io::Error::other("injected slot interruption"),
        ));
    }
    file.sync_all()
        .map_err(|error| DomainReconnectManifestError::io("sync authority slot", error))?;
    #[cfg(test)]
    if interruption == SlotWriteInterruption::AfterSync {
        return Err(DomainReconnectManifestError::io(
            "injected slot interruption after sync",
            io::Error::other("injected slot interruption"),
        ));
    }
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
    let persisted = decode_manifest(&persisted)?;
    if persisted.schema_version != SCHEMA_VERSION || persisted.manifest != *manifest {
        return Err(DomainReconnectManifestError::Invalid {
            reason: "persisted authority does not match the intended generation",
        });
    }
    sync_directory(directory)?;
    #[cfg(test)]
    if interruption == SlotWriteInterruption::AfterDirectorySync {
        return Err(DomainReconnectManifestError::io(
            "injected slot interruption after directory sync",
            io::Error::other("injected slot interruption"),
        ));
    }
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
    set_intent_fenced_at(directory, domain_name, intent, None)
}

fn set_intent_fenced_at(
    directory: &Path,
    domain_name: &str,
    intent: DomainAttachmentIntent,
    retained: Option<&DomainReconnectManifest>,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let lease = ManifestLease::acquire(directory, true)?;
    let next = set_intent_locked(&lease.directory, domain_name, intent, retained)?;
    lease.validate()?;
    Ok(next)
}

fn set_intent_locked(
    directory: &CapDir,
    domain_name: &str,
    intent: DomainAttachmentIntent,
    retained: Option<&DomainReconnectManifest>,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let loaded = load_locked(directory, retained)?;
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
    for name in SLOT_NAMES {
        write_slot(directory, OsStr::new(name), &next)?;
    }
    verify_fully_replicated(directory, &next)?;
    Ok(next)
}

fn set_intent_production_at(
    data_directory: &Path,
    domain_name: &str,
    intent: DomainAttachmentIntent,
    retained: Option<&DomainReconnectManifest>,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    let lease = ProductionManifestLease::acquire(data_directory)?;
    let next = set_intent_locked(&lease.authority.directory, domain_name, intent, retained)?;
    lease.validate()?;
    Ok(next)
}

pub fn set_intent(
    domain_name: &str,
    intent: DomainAttachmentIntent,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    migrate_legacy_data_namespace()?;
    set_intent_production_at(config::DATA_DIR.as_path(), domain_name, intent, None)
}

/// Persist intent only when the selected quorum is at or beyond the exact
/// retained in-process authority. The fence is checked while holding the OS
/// lease and before repair or generation advancement.
pub fn set_intent_fenced(
    domain_name: &str,
    intent: DomainAttachmentIntent,
    retained: Option<&DomainReconnectManifest>,
) -> Result<DomainReconnectManifest, DomainReconnectManifestError> {
    migrate_legacy_data_namespace()?;
    set_intent_production_at(config::DATA_DIR.as_path(), domain_name, intent, retained)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_tempdir(label: &'static str) -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect(label);
        #[cfg(unix)]
        let _ = std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700));
        temp
    }

    #[test]
    fn cross_namespace_migration_preserves_remembered_domains_and_legacy_evidence() {
        let fixture = tempfile::tempdir().expect("domain namespace migration fixture");
        let legacy = fixture.path().join("wezterm");
        let canonical = fixture.path().join("frankenterm");
        let legacy_manifest = set_intent_production_at(
            &legacy,
            "migration-domain",
            DomainAttachmentIntent::Attached,
            None,
        )
        .expect("publish legacy remembered-domain authority");
        let retained_only = [
            (
                "plugins/example/plugin/init.lua",
                b"legacy-plugin".as_slice(),
            ),
            (
                config::DATA_ARTIFACT_REPL_HISTORY,
                b"legacy-repl".as_slice(),
            ),
            (
                config::DATA_ARTIFACT_RECENT_COMMANDS,
                b"legacy-commands".as_slice(),
            ),
            (
                config::DATA_ARTIFACT_RECENT_EMOJI,
                b"legacy-emoji".as_slice(),
            ),
            (
                config::DATA_ARTIFACT_UPDATE_METADATA,
                b"rebuildable-update-metadata".as_slice(),
            ),
        ];
        for (relative, bytes) in retained_only {
            let path = legacy.join(relative);
            std::fs::create_dir_all(path.parent().expect("legacy artifact parent"))
                .expect("create legacy artifact parent");
            std::fs::write(path, bytes).expect("write retained legacy artifact");
        }

        assert!(
            migrate_legacy_data_namespace_at(&legacy, &canonical)
                .expect("migrate remembered-domain authority")
        );
        let canonical_manifest =
            load_production_from(&canonical, None).expect("load canonical migrated authority");
        assert_eq!(canonical_manifest, legacy_manifest);
        assert_eq!(
            canonical_manifest.intent_for_name("migration-domain"),
            Some(DomainAttachmentIntent::Attached)
        );
        assert_eq!(
            load_production_from(&legacy, None).expect("legacy evidence remains readable"),
            legacy_manifest
        );
        assert!(
            private_manifest_directory(&canonical)
                .join(NAMESPACE_MIGRATION_INTENT_NAME)
                .is_file()
        );
        assert!(
            private_manifest_directory(&canonical)
                .join(NAMESPACE_MIGRATION_COMPLETE_NAME)
                .is_file()
        );
        assert!(
            !migrate_legacy_data_namespace_at(&legacy, &canonical)
                .expect("repeated namespace migration is idempotent")
        );
        for (relative, bytes) in retained_only {
            assert_eq!(
                std::fs::read(legacy.join(relative)).expect("legacy artifact remains readable"),
                bytes,
                "legacy artifact bytes changed during authority migration: {relative}"
            );
            assert!(
                !canonical.join(relative).exists(),
                "ambiguous or rebuildable artifact was copied into the canonical namespace: {relative}"
            );
        }
    }

    #[test]
    fn completed_namespace_migration_allows_later_canonical_generations() {
        let fixture = tempfile::tempdir().expect("domain post-migration update fixture");
        let legacy = fixture.path().join("wezterm");
        let canonical = fixture.path().join("frankenterm");
        set_intent_production_at(
            &legacy,
            "legacy-domain",
            DomainAttachmentIntent::Attached,
            None,
        )
        .expect("publish legacy authority");
        assert!(
            migrate_legacy_data_namespace_at(&legacy, &canonical)
                .expect("migrate legacy authority")
        );
        let updated = set_intent_production_at(
            &canonical,
            "post-migration-domain",
            DomainAttachmentIntent::Detached,
            None,
        )
        .expect("publish later canonical generation");

        assert!(
            !migrate_legacy_data_namespace_at(&legacy, &canonical)
                .expect("completed migration admits later canonical generation")
        );
        assert_eq!(
            load_production_from(&canonical, None).expect("load later canonical authority"),
            updated
        );
    }

    #[test]
    fn completed_namespace_migration_rejects_later_legacy_divergence() {
        let fixture = tempfile::tempdir().expect("domain completed divergence fixture");
        let legacy = fixture.path().join("wezterm");
        let canonical = fixture.path().join("frankenterm");
        let migrated = set_intent_production_at(
            &legacy,
            "legacy-domain",
            DomainAttachmentIntent::Attached,
            None,
        )
        .expect("publish legacy authority");
        migrate_legacy_data_namespace_at(&legacy, &canonical)
            .expect("complete namespace migration");
        set_intent_production_at(
            &legacy,
            "late-old-process-domain",
            DomainAttachmentIntent::Detached,
            Some(&migrated),
        )
        .expect("publish later legacy divergence");

        assert!(matches!(
            migrate_legacy_data_namespace_at(&legacy, &canonical)
                .expect_err("completed migration must detect later legacy divergence"),
            DomainReconnectManifestError::NamespaceDivergence
        ));
        assert_eq!(
            load_production_from(&canonical, None).expect("canonical authority remains readable"),
            migrated
        );
    }

    #[test]
    fn canonical_remembered_domain_authority_is_never_overwritten() {
        let fixture = tempfile::tempdir().expect("domain namespace divergence fixture");
        let legacy = fixture.path().join("wezterm");
        let canonical = fixture.path().join("frankenterm");
        set_intent_production_at(
            &legacy,
            "legacy-domain",
            DomainAttachmentIntent::Attached,
            None,
        )
        .expect("publish legacy authority");
        let canonical_before = set_intent_production_at(
            &canonical,
            "canonical-domain",
            DomainAttachmentIntent::Detached,
            None,
        )
        .expect("publish canonical authority");

        assert!(
            !migrate_legacy_data_namespace_at(&legacy, &canonical)
                .expect("canonical authority wins without overwrite")
        );
        assert_eq!(
            load_production_from(&canonical, None).expect("reload canonical authority"),
            canonical_before
        );
    }

    #[test]
    fn namespace_migration_resumes_after_every_replica_publication_cut() {
        for published_replicas in 0..SLOT_NAMES.len() {
            let fixture = tempfile::tempdir().expect("domain namespace resume fixture");
            let legacy = fixture.path().join("wezterm");
            let canonical = fixture.path().join("frankenterm");
            let intended = set_intent_production_at(
                &legacy,
                "resume-domain",
                DomainAttachmentIntent::Attached,
                None,
            )
            .expect("publish legacy resume authority");
            {
                let interrupted =
                    ProductionManifestLease::acquire(&canonical).expect("lock canonical namespace");
                write_slot(
                    &interrupted.authority.directory,
                    OsStr::new(NAMESPACE_MIGRATION_INTENT_NAME),
                    &intended,
                )
                .expect("publish durable migration intent");
                for name in SLOT_NAMES.iter().take(published_replicas) {
                    write_slot(
                        &interrupted.authority.directory,
                        OsStr::new(name),
                        &intended,
                    )
                    .expect("publish interrupted replica");
                }
            }

            assert!(
                migrate_legacy_data_namespace_at(&legacy, &canonical)
                    .expect("resume interrupted namespace migration")
            );
            assert_eq!(
                load_production_from(&canonical, None).expect("load resumed authority"),
                intended
            );
            assert_eq!(
                load_production_from(&legacy, None).expect("legacy recovery evidence remains"),
                intended
            );
        }
    }

    #[test]
    fn namespace_migration_recovers_every_marker_and_replica_write_cut() {
        for interruption in [
            SlotWriteInterruption::AfterTruncate,
            SlotWriteInterruption::AfterPartialWrite,
            SlotWriteInterruption::AfterFullWrite,
            SlotWriteInterruption::AfterSync,
            SlotWriteInterruption::AfterDirectorySync,
        ] {
            for publication in ["intent", "replica", "completion"] {
                let fixture = tempfile::tempdir().expect("domain write-cut fixture");
                let legacy = fixture.path().join("wezterm");
                let canonical = fixture.path().join("frankenterm");
                let intended = set_intent_production_at(
                    &legacy,
                    "write-cut-domain",
                    DomainAttachmentIntent::Attached,
                    None,
                )
                .expect("publish legacy write-cut authority");
                {
                    let interrupted = ProductionManifestLease::acquire(&canonical)
                        .expect("lock canonical write-cut namespace");
                    if publication != "intent" {
                        write_slot(
                            &interrupted.authority.directory,
                            OsStr::new(NAMESPACE_MIGRATION_INTENT_NAME),
                            &intended,
                        )
                        .expect("publish durable write-cut intent");
                    }
                    if publication == "completion" {
                        for name in SLOT_NAMES {
                            write_slot(
                                &interrupted.authority.directory,
                                OsStr::new(name),
                                &intended,
                            )
                            .expect("publish canonical write-cut replica");
                        }
                    }
                    let name = match publication {
                        "intent" => NAMESPACE_MIGRATION_INTENT_NAME,
                        "replica" => SLOT_NAMES[0],
                        "completion" => NAMESPACE_MIGRATION_COMPLETE_NAME,
                        _ => unreachable!("fixed publication cases"),
                    };
                    write_slot_with_interruption(
                        &interrupted.authority.directory,
                        OsStr::new(name),
                        &intended,
                        interruption,
                    )
                    .expect_err("injected write cut must interrupt acknowledgement");
                }

                migrate_legacy_data_namespace_at(&legacy, &canonical)
                    .expect("retry must recover interrupted migration publication");
                assert_eq!(
                    load_production_from(&canonical, None)
                        .expect("load recovered canonical authority"),
                    intended,
                    "recovery failed for {publication} at {interruption:?}"
                );
                assert_eq!(
                    load_production_from(&legacy, None).expect("legacy evidence remains readable"),
                    intended
                );
            }
        }
    }

    #[test]
    fn namespace_migration_failure_is_pinned_for_the_process() {
        use std::cell::Cell;

        let fence = OnceLock::new();
        let calls = Cell::new(0usize);
        for _ in 0..2 {
            assert!(
                namespace_migration_result_from(&fence, || {
                    calls.set(calls.get().saturating_add(1));
                    Err(DomainReconnectManifestError::NamespaceDivergence)
                })
                .is_err()
            );
        }
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn namespace_migration_success_is_rechecked_until_a_failure_is_pinned() {
        use std::cell::Cell;

        let fence = OnceLock::new();
        let calls = Cell::new(0usize);
        for expected in [false, true] {
            assert_eq!(
                namespace_migration_result_from(&fence, || {
                    calls.set(calls.get().saturating_add(1));
                    Ok(expected)
                })
                .expect("successful namespace checks remain live"),
                expected
            );
        }
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn namespace_migration_rejects_divergent_partial_publication() {
        let fixture = tempfile::tempdir().expect("domain namespace conflict fixture");
        let legacy = fixture.path().join("wezterm");
        let canonical = fixture.path().join("frankenterm");
        let intended = set_intent_production_at(
            &legacy,
            "legacy-domain",
            DomainAttachmentIntent::Attached,
            None,
        )
        .expect("publish legacy conflict authority");
        let mut divergent = intended.clone();
        divergent.generation = divergent
            .generation
            .checked_add(1)
            .expect("advance generation");
        {
            let interrupted =
                ProductionManifestLease::acquire(&canonical).expect("lock canonical namespace");
            write_slot(
                &interrupted.authority.directory,
                OsStr::new(NAMESPACE_MIGRATION_INTENT_NAME),
                &intended,
            )
            .expect("publish migration intent");
            write_slot(
                &interrupted.authority.directory,
                OsStr::new(SLOT_NAMES[0]),
                &divergent,
            )
            .expect("publish divergent partial replica");
        }

        assert!(matches!(
            migrate_legacy_data_namespace_at(&legacy, &canonical)
                .expect_err("divergent partial publication must fail closed"),
            DomainReconnectManifestError::NamespaceDivergence
        ));
        assert_eq!(
            load_production_from(&legacy, None).expect("legacy authority remains readable"),
            intended
        );
    }

    #[test]
    fn concurrent_namespace_migrators_converge_on_one_authority() {
        let fixture = tempfile::tempdir().expect("domain concurrent migration fixture");
        let legacy = fixture.path().join("wezterm");
        let canonical = fixture.path().join("frankenterm");
        let intended = set_intent_production_at(
            &legacy,
            "concurrent-domain",
            DomainAttachmentIntent::Attached,
            None,
        )
        .expect("publish concurrent legacy authority");
        let mut workers = Vec::new();
        for _ in 0..2 {
            let legacy = legacy.clone();
            let canonical = canonical.clone();
            workers.push(std::thread::spawn(move || {
                migrate_legacy_data_namespace_at(&legacy, &canonical)
            }));
        }
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().expect("migration worker did not panic"))
            .collect::<Result<Vec<_>, _>>()
            .expect("concurrent migrations converge");
        assert_eq!(outcomes.iter().filter(|migrated| **migrated).count(), 1);
        assert_eq!(
            load_production_from(&canonical, None).expect("load converged authority"),
            intended
        );
    }

    #[cfg(unix)]
    #[test]
    fn namespace_migration_rejects_symlinked_legacy_root() {
        let fixture = tempfile::tempdir().expect("domain symlink migration fixture");
        let outside = fixture.path().join("outside");
        let legacy = fixture.path().join("wezterm");
        let canonical = fixture.path().join("frankenterm");
        set_intent_production_at(
            &outside,
            "foreign-domain",
            DomainAttachmentIntent::Attached,
            None,
        )
        .expect("publish foreign authority");
        std::os::unix::fs::symlink(&outside, &legacy).expect("plant legacy namespace symlink");

        assert!(migrate_legacy_data_namespace_at(&legacy, &canonical).is_err());
        assert!(!canonical.exists());
    }

    #[test]
    fn production_authority_uses_a_dedicated_private_leaf() {
        let data_directory = Path::new("legacy-data-root");
        assert_eq!(
            private_manifest_directory(data_directory),
            data_directory.join(PRIVATE_AUTHORITY_DIRECTORY)
        );
    }

    fn namespace_slot_bytes(directory: &Path) -> [Option<Vec<u8>>; 3] {
        manifest_paths(directory).map(|path| match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                assert_eq!(
                    error.kind(),
                    io::ErrorKind::NotFound,
                    "namespace slot snapshot must be readable or absent"
                );
                None
            }
        })
    }

    fn assert_slots_omit_name(directory: &Path, domain_name: &str) {
        for bytes in namespace_slot_bytes(directory).into_iter().flatten() {
            assert!(
                !bytes
                    .windows(domain_name.len())
                    .any(|window| window == domain_name.as_bytes())
            );
        }
    }

    #[test]
    fn production_v1_root_survives_upgrade_and_explicit_update_stays_in_root() {
        let data_directory = private_tempdir("temporary production data root");
        let legacy_name = "private-legacy-trj-domain";
        let added_name = "private-added-csd-domain";
        let legacy = attached_manifest(legacy_name);
        write_fixture(data_directory.path(), 0, &legacy, LEGACY_SCHEMA_VERSION);

        let migrated = load_production_from(data_directory.path(), None)
            .expect("load and migrate root schema-v1 authority");
        assert_eq!(migrated, legacy);
        assert_fully_replicated_v2(data_directory.path(), &legacy);
        assert!(!private_manifest_directory(data_directory.path()).exists());

        let updated = set_intent_production_at(
            data_directory.path(),
            added_name,
            DomainAttachmentIntent::Detached,
            Some(&migrated),
        )
        .expect("update the selected root authority");
        assert_eq!(updated.generation(), 2);
        assert_eq!(
            updated.intent_for_name(legacy_name),
            Some(DomainAttachmentIntent::Attached)
        );
        assert_eq!(
            updated.intent_for_name(added_name),
            Some(DomainAttachmentIntent::Detached)
        );
        assert_fully_replicated_v2(data_directory.path(), &updated);
        assert!(!private_manifest_directory(data_directory.path()).exists());
        assert_slots_omit_name(data_directory.path(), legacy_name);
        assert_slots_omit_name(data_directory.path(), added_name);
    }

    #[test]
    fn production_fresh_state_uses_only_the_private_leaf() {
        let data_directory = tempfile::tempdir().expect("temporary production data root");
        #[cfg(unix)]
        std::fs::set_permissions(
            data_directory.path(),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("model an existing non-private application data root");
        let private_directory = private_manifest_directory(data_directory.path());
        let domain_name = "private-fresh-trj-domain";

        let persisted = set_intent_production_at(
            data_directory.path(),
            domain_name,
            DomainAttachmentIntent::Attached,
            None,
        )
        .expect("persist fresh production authority");
        assert_eq!(persisted.generation(), 1);
        assert_eq!(
            namespace_slot_bytes(data_directory.path()),
            [None, None, None]
        );
        assert_fully_replicated_v2(&private_directory, &persisted);
        assert_eq!(
            load_production_from(data_directory.path(), Some(&persisted))
                .expect("reload private-only production authority"),
            persisted
        );
        let updated = set_intent_production_at(
            data_directory.path(),
            domain_name,
            DomainAttachmentIntent::Detached,
            Some(&persisted),
        )
        .expect("update private-only production authority");
        assert_eq!(updated.generation(), 2);
        assert_eq!(
            updated.intent_for_name(domain_name),
            Some(DomainAttachmentIntent::Detached)
        );
        assert_eq!(
            namespace_slot_bytes(data_directory.path()),
            [None, None, None]
        );
        assert_fully_replicated_v2(&private_directory, &updated);
        assert_slots_omit_name(&private_directory, domain_name);
        #[cfg(unix)]
        assert_eq!(
            std::fs::symlink_metadata(data_directory.path())
                .expect("inspect application data root")
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
    }

    #[test]
    fn production_corrupt_root_never_defaults_to_a_fresh_private_authority() {
        let data_directory = private_tempdir("temporary production data root");
        write_corrupt_fixture(data_directory.path(), 0);
        let before = namespace_slot_bytes(data_directory.path());

        assert!(matches!(
            set_intent_production_at(
                data_directory.path(),
                "private-corrupt-root-domain",
                DomainAttachmentIntent::Attached,
                None,
            ),
            Err(DomainReconnectManifestError::Invalid {
                reason: "manifest is truncated"
            })
        ));
        assert_eq!(namespace_slot_bytes(data_directory.path()), before);
        assert!(!private_manifest_directory(data_directory.path()).exists());
    }

    #[test]
    fn production_dual_namespace_evidence_fails_before_any_mutation() {
        let data_directory = private_tempdir("temporary production data root");
        let root_manifest = attached_manifest("private-root-trj-domain");
        write_fixture(
            data_directory.path(),
            0,
            &root_manifest,
            LEGACY_SCHEMA_VERSION,
        );
        let private_directory = private_manifest_directory(data_directory.path());
        let private_manifest = set_intent_at(
            &private_directory,
            "private-leaf-csd-domain",
            DomainAttachmentIntent::Detached,
        )
        .expect("seed private namespace evidence");
        let root_before = namespace_slot_bytes(data_directory.path());
        let private_before = namespace_slot_bytes(&private_directory);
        assert!(!lock_path(data_directory.path()).exists());

        let error = set_intent_production_at(
            data_directory.path(),
            "private-third-domain",
            DomainAttachmentIntent::Attached,
            None,
        )
        .expect_err("dual namespace evidence must fail closed");
        assert!(matches!(
            &error,
            DomainReconnectManifestError::NamespaceDivergence
        ));
        assert_eq!(namespace_slot_bytes(data_directory.path()), root_before);
        assert_eq!(namespace_slot_bytes(&private_directory), private_before);
        assert!(!lock_path(data_directory.path()).exists());
        assert_eq!(
            load_from(&private_directory).expect("private evidence remains readable"),
            private_manifest
        );
        let error_text = error.to_string();
        for secret in [
            "private-root-trj-domain",
            "private-leaf-csd-domain",
            "private-third-domain",
        ] {
            assert!(!error_text.contains(secret));
            assert_slots_omit_name(data_directory.path(), secret);
            assert_slots_omit_name(&private_directory, secret);
        }
    }

    #[test]
    fn authority_leaf_syncs_parent_on_creation_and_existing_fast_path() {
        let source = include_str!("domain_reconnect_manifest.rs");
        let helper = source
            .split_once("fn open_or_create_directory_tree_durably(")
            .expect("durable directory helper must exist")
            .1
            .split_once("fn open_manifest_directory(")
            .expect("durable helper must end before the manifest opener")
            .0;
        let create = helper
            .find("parent.create_dir_with(name, &builder)")
            .expect("helper must create the child through its pinned parent");
        let creation_branch_end = helper
            .find("\n    let pinned = parent")
            .expect("child creation branch must end before the common publication path");
        let nofollow_open = helper
            .find(".open_dir_nofollow(name)")
            .expect("helper must pin the created child without following a symlink");
        let parent_sync = helper
            .find("sync_directory(&parent)?")
            .expect("helper must synchronize the parent entry");
        let success = helper
            .find("Ok(pinned)")
            .expect("helper must return the pinned child only after publication");

        assert!(create < nofollow_open);
        assert!(creation_branch_end <= nofollow_open);
        assert!(nofollow_open < parent_sync);
        assert!(parent_sync < success);
        assert_eq!(helper.matches("sync_directory(&parent)?").count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn private_leaf_does_not_require_or_rewrite_legacy_parent_mode() {
        let root = tempfile::tempdir().expect("temporary legacy data root");
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))
            .expect("model an older broadly readable data root");
        let authority = private_manifest_directory(root.path());

        let persisted = set_intent_at(&authority, "trj", DomainAttachmentIntent::Attached)
            .expect("persist inside dedicated private leaf");
        assert_eq!(
            load_from(&authority).expect("reload private-leaf authority"),
            persisted
        );
        assert_eq!(
            std::fs::symlink_metadata(&authority)
                .expect("inspect private leaf")
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(root.path())
                .expect("inspect legacy data root")
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
    }

    fn manifest_with_intent(
        name: &str,
        intent: DomainAttachmentIntent,
        generation: u64,
    ) -> DomainReconnectManifest {
        let mut intents = BTreeMap::new();
        intents.insert(fingerprint_domain_name(name), intent);
        DomainReconnectManifest {
            generation,
            intents,
        }
    }

    fn attached_manifest(name: &str) -> DomainReconnectManifest {
        manifest_with_intent(name, DomainAttachmentIntent::Attached, 1)
    }

    fn detached_manifest(name: &str) -> DomainReconnectManifest {
        manifest_with_intent(name, DomainAttachmentIntent::Detached, 2)
    }

    fn write_fixture(
        directory: &Path,
        slot: usize,
        manifest: &DomainReconnectManifest,
        schema_version: u32,
    ) {
        let paths = manifest_paths(directory);
        let mut file = private_open_options()
            .open(&paths[slot])
            .expect("create private authority fixture");
        file.set_len(0).expect("truncate authority fixture");
        file.write_all(
            &encode_manifest_for_schema(manifest, schema_version).expect("encode fixture"),
        )
        .expect("write authority fixture");
        file.sync_all().expect("sync authority fixture");
    }

    fn write_corrupt_fixture(directory: &Path, slot: usize) {
        let paths = manifest_paths(directory);
        let mut file = private_open_options()
            .open(&paths[slot])
            .expect("create corrupt authority fixture");
        file.set_len(0).expect("truncate corrupt fixture");
        file.write_all(b"torn").expect("write corrupt fixture");
        file.sync_all().expect("sync corrupt fixture");
    }

    fn assert_fully_replicated_v2(directory: &Path, manifest: &DomainReconnectManifest) {
        for path in manifest_paths(directory) {
            let bytes = std::fs::read(path).expect("read replicated fixture");
            let decoded = decode_manifest(&bytes).expect("decode replicated fixture");
            assert_eq!(decoded.schema_version, SCHEMA_VERSION);
            assert_eq!(decoded.manifest, *manifest);
        }
    }

    #[test]
    fn codec_is_canonical_bounded_and_checksum_protected() {
        let manifest = attached_manifest("trj");
        let encoded = encode_manifest(&manifest).expect("encode manifest");
        let decoded = decode_manifest(&encoded).expect("decode manifest");
        assert_eq!(decoded.schema_version, SCHEMA_VERSION);
        assert_eq!(decoded.manifest, manifest);

        let legacy = encode_manifest_for_schema(&manifest, LEGACY_SCHEMA_VERSION)
            .expect("encode legacy fixture");
        let decoded_legacy = decode_manifest(&legacy).expect("decode legacy fixture");
        assert_eq!(decoded_legacy.schema_version, LEGACY_SCHEMA_VERSION);
        assert_eq!(decoded_legacy.manifest, manifest);

        let mut corrupt = encoded;
        corrupt[HEADER_BYTES] ^= 1;
        assert!(matches!(
            decode_manifest(&corrupt),
            Err(DomainReconnectManifestError::Invalid {
                reason: "manifest checksum does not match"
            })
        ));

        let mut unsupported = encode_manifest(&manifest).expect("encode unsupported fixture");
        unsupported[8..12].copy_from_slice(&3_u32.to_le_bytes());
        assert!(matches!(
            decode_manifest(&unsupported),
            Err(DomainReconnectManifestError::UnsupportedVersion { found: 3 })
        ));

        let oversized = vec![0_u8; usize::try_from(MAX_MANIFEST_BYTES).expect("bound fits") + 1];
        assert!(matches!(
            decode_manifest(&oversized),
            Err(DomainReconnectManifestError::Oversized { .. })
        ));
    }

    #[test]
    fn explicit_attach_and_detach_survive_restart_and_override_config() {
        let temp = private_tempdir("temporary directory");
        let missing = load_from(temp.path()).expect("missing manifest");
        assert!(missing.should_connect("trj", true));
        assert!(!missing.should_connect("trj", false));

        set_intent_at(temp.path(), "trj", DomainAttachmentIntent::Attached)
            .expect("persist attach");
        let attached = load_from(temp.path()).expect("reload attached");
        assert!(attached.should_connect("trj", false));

        set_intent_at(temp.path(), "trj", DomainAttachmentIntent::Detached)
            .expect("persist detach");
        let detached = load_from(temp.path()).expect("reload detached");
        assert!(!detached.should_connect("trj", true));
        assert_eq!(detached.generation(), 2);
        assert_fully_replicated_v2(temp.path(), &detached);
    }

    #[test]
    fn v2_crash_cuts_select_only_an_exact_quorum() {
        #[derive(Clone, Copy)]
        enum Fixture<'a> {
            Manifest(&'a DomainReconnectManifest),
            Torn,
        }

        enum Expected<'a> {
            Manifest(&'a DomainReconnectManifest),
            NoQuorum,
        }

        let attached = attached_manifest("trj");
        let detached = detached_manifest("trj");
        let cuts = [
            (
                [
                    Fixture::Torn,
                    Fixture::Manifest(&attached),
                    Fixture::Manifest(&attached),
                ],
                Expected::Manifest(&attached),
            ),
            (
                [
                    Fixture::Manifest(&detached),
                    Fixture::Manifest(&attached),
                    Fixture::Manifest(&attached),
                ],
                Expected::Manifest(&attached),
            ),
            (
                [
                    Fixture::Manifest(&detached),
                    Fixture::Torn,
                    Fixture::Manifest(&attached),
                ],
                Expected::NoQuorum,
            ),
            (
                [
                    Fixture::Manifest(&detached),
                    Fixture::Manifest(&detached),
                    Fixture::Manifest(&attached),
                ],
                Expected::Manifest(&detached),
            ),
            (
                [
                    Fixture::Manifest(&detached),
                    Fixture::Manifest(&detached),
                    Fixture::Torn,
                ],
                Expected::Manifest(&detached),
            ),
            (
                [
                    Fixture::Manifest(&detached),
                    Fixture::Manifest(&detached),
                    Fixture::Manifest(&detached),
                ],
                Expected::Manifest(&detached),
            ),
        ];

        for (fixtures, expected) in cuts {
            let temp = private_tempdir("temporary crash-cut directory");
            for (slot, fixture) in fixtures.into_iter().enumerate() {
                match fixture {
                    Fixture::Manifest(manifest) => {
                        write_fixture(temp.path(), slot, manifest, SCHEMA_VERSION);
                    }
                    Fixture::Torn => write_corrupt_fixture(temp.path(), slot),
                }
            }
            match expected {
                Expected::Manifest(manifest) => {
                    assert_eq!(load_from(temp.path()).expect("load quorum"), *manifest);
                    assert_fully_replicated_v2(temp.path(), manifest);
                }
                Expected::NoQuorum => assert!(matches!(
                    load_from(temp.path()),
                    Err(DomainReconnectManifestError::NoQuorum)
                )),
            }
        }
    }

    #[test]
    fn retained_floor_rejects_rollback_before_repair_or_write() {
        let temp = private_tempdir("temporary rollback-fence directory");
        let retained = manifest_with_intent("trj", DomainAttachmentIntent::Detached, 10);
        let rolled_back = manifest_with_intent("trj", DomainAttachmentIntent::Attached, 8);
        write_fixture(temp.path(), 0, &rolled_back, SCHEMA_VERSION);
        write_fixture(temp.path(), 1, &rolled_back, SCHEMA_VERSION);
        write_fixture(temp.path(), 2, &retained, SCHEMA_VERSION);
        let before = manifest_paths(temp.path())
            .map(|path| std::fs::read(path).expect("read exact pre-fence replica bytes"));

        assert!(matches!(
            load_fenced_from(temp.path(), Some(&retained)),
            Err(DomainReconnectManifestError::AuthorityRollback {
                observed: 8,
                retained: 10,
            })
        ));
        assert_eq!(
            manifest_paths(temp.path())
                .map(|path| { std::fs::read(path).expect("read exact post-load replica bytes") }),
            before,
            "a rejected load must not repair over surviving high-water evidence"
        );

        assert!(matches!(
            set_intent_fenced_at(
                temp.path(),
                "csd",
                DomainAttachmentIntent::Attached,
                Some(&retained),
            ),
            Err(DomainReconnectManifestError::AuthorityRollback {
                observed: 8,
                retained: 10,
            })
        ));
        assert_eq!(
            manifest_paths(temp.path())
                .map(|path| { std::fs::read(path).expect("read exact post-write replica bytes") }),
            before,
            "a rejected write must not repair or advance a rolled-back quorum"
        );
    }

    #[test]
    fn retained_floor_rejects_equal_generation_aba_without_mutation() {
        let temp = private_tempdir("temporary ABA-fence directory");
        let retained = manifest_with_intent("trj", DomainAttachmentIntent::Detached, 10);
        let replacement = manifest_with_intent("trj", DomainAttachmentIntent::Attached, 10);
        write_fixture(temp.path(), 0, &replacement, SCHEMA_VERSION);
        write_fixture(temp.path(), 1, &replacement, SCHEMA_VERSION);
        write_fixture(temp.path(), 2, &retained, SCHEMA_VERSION);
        let before = manifest_paths(temp.path())
            .map(|path| std::fs::read(path).expect("read exact pre-ABA replica bytes"));

        assert!(matches!(
            load_fenced_from(temp.path(), Some(&retained)),
            Err(DomainReconnectManifestError::AuthorityDivergence { generation: 10 })
        ));
        assert!(matches!(
            set_intent_fenced_at(
                temp.path(),
                "csd",
                DomainAttachmentIntent::Attached,
                Some(&retained),
            ),
            Err(DomainReconnectManifestError::AuthorityDivergence { generation: 10 })
        ));
        assert_eq!(
            manifest_paths(temp.path())
                .map(|path| { std::fs::read(path).expect("read exact post-ABA replica bytes") }),
            before,
            "same-generation replacement must be rejected before repair or write"
        );
    }

    #[test]
    fn retained_floor_accepts_and_repairs_a_strictly_newer_quorum() {
        let temp = private_tempdir("temporary forward-fence directory");
        let retained = manifest_with_intent("trj", DomainAttachmentIntent::Detached, 10);
        let advanced = manifest_with_intent("csd", DomainAttachmentIntent::Attached, 11);
        write_fixture(temp.path(), 0, &advanced, SCHEMA_VERSION);
        write_fixture(temp.path(), 1, &advanced, SCHEMA_VERSION);
        write_fixture(temp.path(), 2, &retained, SCHEMA_VERSION);

        assert_eq!(
            load_fenced_from(temp.path(), Some(&retained))
                .expect("strictly newer quorum should advance authority"),
            advanced
        );
        assert_fully_replicated_v2(temp.path(), &advanced);
    }

    #[test]
    fn a_committed_detach_survives_corruption_of_each_single_replica() {
        let detached = detached_manifest("trj");
        for corrupt_slot in 0..SLOT_NAMES.len() {
            let temp = private_tempdir("temporary corruption directory");
            for slot in 0..SLOT_NAMES.len() {
                write_fixture(temp.path(), slot, &detached, SCHEMA_VERSION);
            }
            write_corrupt_fixture(temp.path(), corrupt_slot);

            let loaded = load_from(temp.path()).expect("recover detached quorum");
            assert_eq!(loaded, detached);
            assert!(!loaded.should_connect("trj", true));
            assert_fully_replicated_v2(temp.path(), &detached);
        }
    }

    #[test]
    fn losing_one_new_detach_replica_never_resolves_to_an_older_attach() {
        let attached = attached_manifest("trj");
        let detached = detached_manifest("trj");
        for corrupt_slot in 0..2 {
            let temp = private_tempdir("temporary rollback directory");
            write_fixture(temp.path(), 0, &detached, SCHEMA_VERSION);
            write_fixture(temp.path(), 1, &detached, SCHEMA_VERSION);
            write_fixture(temp.path(), 2, &attached, SCHEMA_VERSION);
            write_corrupt_fixture(temp.path(), corrupt_slot);

            assert!(matches!(
                load_from(temp.path()),
                Err(DomainReconnectManifestError::NoQuorum)
            ));
        }
    }

    #[test]
    fn legacy_authority_migrates_in_place_and_resumes_each_safe_cut() {
        let attached = attached_manifest("trj");
        let detached = detached_manifest("trj");

        let first_publication = private_tempdir("first-publication migration directory");
        write_fixture(
            first_publication.path(),
            0,
            &attached,
            LEGACY_SCHEMA_VERSION,
        );
        assert_eq!(
            load_from(first_publication.path()).expect("migrate canonical legacy singleton"),
            attached
        );
        assert_fully_replicated_v2(first_publication.path(), &attached);

        let ordinary = private_tempdir("ordinary migration directory");
        write_fixture(ordinary.path(), 0, &attached, LEGACY_SCHEMA_VERSION);
        write_fixture(ordinary.path(), 1, &detached, LEGACY_SCHEMA_VERSION);
        assert_eq!(
            load_from(ordinary.path()).expect("migrate legacy authority"),
            detached
        );
        assert_fully_replicated_v2(ordinary.path(), &detached);

        let torn_third = private_tempdir("third-slot-cut migration directory");
        write_fixture(torn_third.path(), 0, &attached, LEGACY_SCHEMA_VERSION);
        write_fixture(torn_third.path(), 1, &detached, LEGACY_SCHEMA_VERSION);
        write_corrupt_fixture(torn_third.path(), 2);
        assert_eq!(
            load_from(torn_third.path()).expect("resume torn third-slot publication"),
            detached
        );
        assert_fully_replicated_v2(torn_third.path(), &detached);

        let after_first = private_tempdir("first-cut migration directory");
        write_fixture(after_first.path(), 0, &attached, LEGACY_SCHEMA_VERSION);
        write_fixture(after_first.path(), 1, &detached, LEGACY_SCHEMA_VERSION);
        write_fixture(after_first.path(), 2, &detached, SCHEMA_VERSION);
        assert_eq!(
            load_from(after_first.path()).expect("resume after first migration publication"),
            detached
        );
        assert_fully_replicated_v2(after_first.path(), &detached);

        let torn_stale = private_tempdir("stale-cut migration directory");
        write_corrupt_fixture(torn_stale.path(), 0);
        write_fixture(torn_stale.path(), 1, &detached, LEGACY_SCHEMA_VERSION);
        write_fixture(torn_stale.path(), 2, &detached, SCHEMA_VERSION);
        assert_eq!(
            load_from(torn_stale.path()).expect("resume torn stale migration publication"),
            detached
        );
        assert_fully_replicated_v2(torn_stale.path(), &detached);

        let after_stale = private_tempdir("post-stale migration directory");
        write_fixture(after_stale.path(), 0, &detached, SCHEMA_VERSION);
        write_fixture(after_stale.path(), 1, &detached, LEGACY_SCHEMA_VERSION);
        write_fixture(after_stale.path(), 2, &detached, SCHEMA_VERSION);
        assert_eq!(
            load_from(after_stale.path()).expect("resume after stale-slot publication"),
            detached
        );
        assert_fully_replicated_v2(after_stale.path(), &detached);
    }

    #[test]
    fn ambiguous_legacy_singleton_fails_closed_without_repair() {
        let temp = private_tempdir("temporary legacy ambiguity directory");
        let attached = attached_manifest("trj");
        write_fixture(temp.path(), 0, &attached, LEGACY_SCHEMA_VERSION);
        write_corrupt_fixture(temp.path(), 1);

        assert!(matches!(
            load_from(temp.path()),
            Err(DomainReconnectManifestError::Invalid {
                reason: "manifest is truncated"
            })
        ));
        assert!(!manifest_paths(temp.path())[2].exists());
    }

    #[test]
    fn a_v2_singleton_never_falls_back_to_an_older_legacy_generation() {
        let temp = private_tempdir("temporary mixed-schema directory");
        let attached = attached_manifest("trj");
        let detached = detached_manifest("trj");
        write_fixture(temp.path(), 0, &attached, LEGACY_SCHEMA_VERSION);
        write_fixture(temp.path(), 1, &attached, LEGACY_SCHEMA_VERSION);
        write_fixture(temp.path(), 2, &detached, SCHEMA_VERSION);

        assert!(matches!(
            load_from(temp.path()),
            Err(DomainReconnectManifestError::NoQuorum)
        ));
    }

    #[test]
    fn divergent_states_at_the_same_generation_fail_closed() {
        let temp = private_tempdir("temporary directory");
        let attached = attached_manifest("trj");
        let mut detached = attached.clone();
        detached.intents.insert(
            fingerprint_domain_name("trj"),
            DomainAttachmentIntent::Detached,
        );
        write_fixture(temp.path(), 0, &attached, LEGACY_SCHEMA_VERSION);
        write_fixture(temp.path(), 1, &detached, LEGACY_SCHEMA_VERSION);

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

        let temp = private_tempdir("temporary directory");
        set_intent_at(temp.path(), "trj", DomainAttachmentIntent::Attached)
            .expect("persist manifest");
        let paths = manifest_paths(temp.path());
        std::fs::set_permissions(&paths[0], std::fs::Permissions::from_mode(0o644))
            .expect("make slot unsafe");
        assert!(matches!(
            load_from(temp.path()),
            Err(DomainReconnectManifestError::UnsafeFile { .. })
        ));

        let other = private_tempdir("second temporary directory");
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
        let temp = private_tempdir("temporary directory");
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
        let temp = private_tempdir("temporary directory");
        let manifest = set_intent_at(temp.path(), "trj", DomainAttachmentIntent::Attached)
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

        let parent = private_tempdir("temporary parent directory");
        let authority_path = parent.path().join("authority");
        set_intent_at(&authority_path, "trj", DomainAttachmentIntent::Attached)
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
        let temp = private_tempdir("temporary directory");
        set_intent_at(temp.path(), "trj", DomainAttachmentIntent::Attached)
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
        let temp = private_tempdir("temporary directory");
        let secret_name = "private-production-host";
        set_intent_at(temp.path(), secret_name, DomainAttachmentIntent::Attached)
            .expect("persist manifest");
        for path in manifest_paths(temp.path()) {
            if let Ok(bytes) = std::fs::read(path) {
                assert!(
                    !bytes
                        .windows(secret_name.len())
                        .any(|window| window == secret_name.as_bytes())
                );
            }
        }
    }
}
