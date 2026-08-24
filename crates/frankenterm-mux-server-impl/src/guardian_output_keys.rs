//! Private, append-only key authority for guardian output journals.
//!
//! The key directory is supplied as an already pinned capability directory.
//! Every leaf is opened without following symlinks, must be a private regular
//! file owned by the same account as the directory, and is revalidated against
//! its name after open.  Key and activation files are immutable: rotation adds
//! a new key and a monotonically numbered activation record, leaving old keys
//! available for historical segment recovery.

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir as CapDir, File as CapFile, Metadata as CapMetadata};
#[cfg(not(target_os = "wasi"))]
use cap_std::fs::DirBuilder as CapDirBuilder;
use cap_std::fs::OpenOptions as CapOpenOptions;
use cap_std::fs::{MetadataExt as _, PermissionsExt as _};
use cap_std::fs::OpenOptionsExt as _;
#[cfg(unix)]
use cap_std::fs::{
    DirBuilderExt as _, MetadataExt as CapUnixMetadataExt, PermissionsExt as _,
};
#[cfg(windows)]
use cap_std::fs::MetadataExt as CapWindowsMetadataExt;
use mux::guardian_output_journal::{
    GuardianOutputCipher, GuardianOutputJournalError, GuardianOutputKey,
};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "redox",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
use std::os::fd::AsFd as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use thiserror::Error;

const KEY_PREFIX: &str = "key-";
const ACTIVE_PREFIX: &str = "active-";
const INTENT_PREFIX: &str = "intent-";
const KEY_STAGE_PREFIX: &str = ".stage-key-";
const ACTIVATION_STAGE_PREFIX: &str = ".stage-active-";
const INTENT_STAGE_PREFIX: &str = ".stage-intent-";
const FORMAT_SUFFIX: &str = ".v1";
const KEY_ID_HEX_BYTES: usize = 16;
const FULL_DIGEST_HEX_BYTES: usize = 64;
const STAGE_NONCE_HEX_BYTES: usize = 32;
const GENERATION_DECIMAL_BYTES: usize = 20;
const KEY_FILE_BYTES: u64 = GuardianOutputCipher::KEY_BYTES as u64;
const ACTIVATION_MAGIC: [u8; 8] = *b"FTGACT01";
const ACTIVATION_VERSION: u32 = 1;
const ACTIVATION_BYTES: usize = 64;
const ACTIVATION_BYTES_U32: u32 = 64;
const ACTIVATION_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-output-key-activation.v1\0";
const INTENT_MAGIC: [u8; 8] = *b"FTGINT01";
const INTENT_VERSION: u32 = 1;
const INTENT_BYTES: usize = 224;
const INTENT_BYTES_U32: u32 = 224;
const INTENT_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-output-key-intent.v1\0";
const MAX_KEYRING_ENTRIES: usize = 4096;
const SCROLLBACK_KEYRING_SIBLING: &str = "guardian-output-keys";
const AUTHORITY_LOCK_NAME: &str = ".authority-lock.v1";
static SCROLLBACK_KEYRING_PROVISION_GATE: Mutex<()> = Mutex::new(());
static SHARED_SCROLLBACK_KEYRINGS: LazyLock<
    Mutex<HashMap<GuardianOutputAuthorityIdentity, Weak<Mutex<GuardianOutputKeyring>>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct GuardianOutputAuthorityIdentity {
    device: u64,
    inode: u64,
}

#[cfg(not(unix))]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct GuardianOutputAuthorityIdentity {
    canonical_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum GuardianOutputKeyringError {
    #[error("guardian output key directory is not private")]
    DirectoryNotPrivate,
    #[error("guardian output key directory contains too many entries")]
    EntryLimit,
    #[error("guardian output key directory contains an unrecognized entry")]
    UnrecognizedEntry,
    #[error("guardian output key entry is a symbolic link")]
    SymlinkRejected,
    #[error("guardian output key entry is not a private regular file")]
    UnsafeKeyFile,
    #[error("guardian output key entry changed identity while opening")]
    IdentityChanged,
    #[error("guardian output key activation record is malformed")]
    InvalidActivation,
    #[error("guardian output key publication intent is malformed")]
    InvalidIntent,
    #[error("guardian output key publication has a pending durable intent")]
    PendingActivation,
    #[error("guardian output key activation generation is ambiguous")]
    AmbiguousGeneration,
    #[error("guardian output key activation exists without its key")]
    MissingActivatedKey,
    #[error("guardian output key material exists without an activation record")]
    OrphanedKeyMaterial,
    #[error("guardian output key was never activated")]
    UnactivatedKey,
    #[error("guardian output key generation space is exhausted")]
    GenerationExhausted,
    #[error("guardian output key identifier collision")]
    KeyIdCollision,
    #[error("guardian output key authority changed after it was pinned")]
    AuthorityChanged,
    #[error("atomic no-replace guardian key publication is unsupported on this platform")]
    UnsupportedAtomicPublication,
    #[error("secure random guardian key publication identity is unavailable")]
    EntropyUnavailable,
    #[error("guardian output key operation failed")]
    Key(#[from] GuardianOutputJournalError),
    #[error("guardian output key filesystem operation failed")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Activation {
    generation: u64,
    key_id: [u8; 8],
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct PublicationIntent {
    authority_id: [u8; 16],
    generation: u64,
    key_id: [u8; 8],
    key_sha256: [u8; 32],
    predecessor_generation: u64,
    predecessor_key_id: [u8; 8],
    predecessor_activation_digest: [u8; 32],
    activation_digest: [u8; 32],
    predecessor_intent_digest: [u8; 32],
    record_digest: [u8; 32],
}

impl std::fmt::Debug for PublicationIntent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublicationIntent")
            .field("generation", &self.generation)
            .field("key_id", &hex::encode(self.key_id))
            .field("authority_id", &hex::encode(self.authority_id))
            .field("digests", &"[REDACTED]")
            .finish()
    }
}

impl std::fmt::Debug for Activation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Activation")
            .field("generation", &self.generation)
            .field("key_id", &hex::encode(self.key_id))
            .finish()
    }
}

/// Pinned active key authority plus append-only historical key lookup.
pub struct GuardianOutputKeyring {
    directory: CapDir,
    use_authority_lock: bool,
    active: Activation,
    active_key: GuardianOutputKey,
    pending_generation: Option<u64>,
}

impl std::fmt::Debug for GuardianOutputKeyring {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputKeyring")
            .field("active_generation", &self.active.generation)
            .field("active_key_id", &hex::encode(self.active.key_id))
            .field("pending_generation", &self.pending_generation)
            .field("active_key", &"[REDACTED]")
            .finish()
    }
}

impl GuardianOutputKeyring {
    /// Return the process-wide guardian output authority for this securely
    /// pinned scrollback store. All live pane sinks sharing the same authority
    /// directory receive the same mutex-protected keyring, so an in-process
    /// rotation becomes visible atomically to every sink.
    pub fn shared_scrollback_sibling(
        scrollback_base_dir: &Path,
    ) -> Result<Arc<Mutex<Self>>, GuardianOutputKeyringError> {
        let keyring = Self::open_or_provision_scrollback_sibling(scrollback_base_dir)?;
        let identity = authority_identity(
            &keyring.directory,
            &scrollback_keyring_path(scrollback_base_dir)?,
        )?;
        let mut registry = SHARED_SCROLLBACK_KEYRINGS
            .lock()
            .map_err(|_| GuardianOutputKeyringError::AuthorityChanged)?;
        registry.retain(|_, weak| weak.strong_count() != 0);
        if let Some(existing) = registry.get(&identity).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        let shared = Arc::new(Mutex::new(keyring));
        registry.insert(identity, Arc::downgrade(&shared));
        Ok(shared)
    }

    /// Open the one guardian output keyring shared by encrypted scrollback and
    /// guardian journals, provisioning it only in the deterministic private
    /// sibling of the durable pane directories inside the scrollback store.
    pub fn open_or_provision_scrollback_sibling(
        scrollback_base_dir: &Path,
    ) -> Result<Self, GuardianOutputKeyringError> {
        let _provision_gate = SCROLLBACK_KEYRING_PROVISION_GATE
            .lock()
            .map_err(|_| GuardianOutputKeyringError::AuthorityChanged)?;
        let _path = scrollback_keyring_path(scrollback_base_dir)?;
        let parent = open_owned_path(scrollback_base_dir, false)?;
        match create_private_directory(&parent, SCROLLBACK_KEYRING_SIBLING) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let directory = open_private_child(&parent, SCROLLBACK_KEYRING_SIBLING)?;
        // Repair both sides of an interrupted mkdir even when another process
        // made the child visible, then prove that the pinned parent and child
        // still name the exact descriptors synchronized here.
        sync_directory(&directory)?;
        sync_directory(&parent)?;
        validate_pinned_child(&parent, SCROLLBACK_KEYRING_SIBLING, &directory)?;
        validate_owned_path(scrollback_base_dir, &parent, false)?;
        let _authority_lease = AuthorityFileLease::acquire(&directory, true, true)?;
        let mut keyring = Self::open_or_provision_under_exclusive_lease(directory)?;
        validate_pinned_child(&parent, SCROLLBACK_KEYRING_SIBLING, &keyring.directory)?;
        validate_owned_path(scrollback_base_dir, &parent, false)?;
        keyring.use_authority_lock = true;
        Ok(keyring)
    }

    /// Open the existing shared keyring without creating filesystem state.
    /// Read-only transcript export uses this path only after it encounters a
    /// v3 encrypted row.
    pub fn open_existing_scrollback_sibling(
        scrollback_base_dir: &Path,
    ) -> Result<Self, GuardianOutputKeyringError> {
        let _path = scrollback_keyring_path(scrollback_base_dir)?;
        let parent = open_owned_path(scrollback_base_dir, false)?;
        let directory = open_private_child(&parent, SCROLLBACK_KEYRING_SIBLING)?;
        validate_owned_path(scrollback_base_dir, &parent, false)?;
        let _authority_lease = match AuthorityFileLease::acquire(&directory, false, false) {
            Ok(lease) => lease,
            Err(GuardianOutputKeyringError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                if inventory(&directory)?.entries == 0 {
                    return Err(GuardianOutputKeyringError::MissingActivatedKey);
                }
                return Err(GuardianOutputKeyringError::AuthorityChanged);
            }
            Err(error) => return Err(error),
        };
        let mut keyring = Self::open_existing(directory)?;
        validate_pinned_child(&parent, SCROLLBACK_KEYRING_SIBLING, &keyring.directory)?;
        validate_owned_path(scrollback_base_dir, &parent, false)?;
        keyring.use_authority_lock = true;
        Ok(keyring)
    }

    /// Open a pinned key directory under an exclusive cross-process lease,
    /// recovering the sole exact pending intent before provisioning or use.
    pub fn open_or_provision(directory: CapDir) -> Result<Self, GuardianOutputKeyringError> {
        validate_directory(&directory)?;
        let _authority_lease = AuthorityFileLease::acquire(&directory, true, true)?;
        let mut keyring = Self::open_or_provision_under_exclusive_lease(directory)?;
        keyring.use_authority_lock = true;
        Ok(keyring)
    }

    fn open_or_provision_under_exclusive_lease(
        directory: CapDir,
    ) -> Result<Self, GuardianOutputKeyringError> {
        validate_directory(&directory)?;
        let mut current = inventory(&directory)?;
        if current.pending.is_some() {
            current = recover_pending_intent(&directory, current)?;
        }
        validate_directory(&directory)?;
        if current.latest.is_none() {
            if !current.key_files.is_empty() || !current.intents.is_empty() {
                return Err(GuardianOutputKeyringError::OrphanedKeyMaterial);
            }
            return provision_first(directory, &current);
        }
        open_inventory(directory, current)
    }

    fn open_existing(directory: CapDir) -> Result<Self, GuardianOutputKeyringError> {
        validate_directory(&directory)?;
        let inventory = inventory(&directory)?;
        validate_directory(&directory)?;
        if inventory.latest.is_none() {
            if inventory.pending.is_some() {
                return Err(GuardianOutputKeyringError::PendingActivation);
            }
            return Err(GuardianOutputKeyringError::MissingActivatedKey);
        }
        open_inventory(directory, inventory)
    }

    #[must_use]
    pub const fn active_generation(&self) -> u64 {
        self.active.generation
    }

    #[must_use]
    pub const fn active_key_id(&self) -> [u8; 8] {
        self.active.key_id
    }

    /// Report a durable next-generation intent that a read-only opener did
    /// not activate. The prior activation remains valid for historical reads;
    /// only a writable exclusive opener may complete this generation.
    #[must_use]
    pub const fn pending_activation_generation(&self) -> Option<u64> {
        self.pending_generation
    }

    pub fn active_cipher(&self) -> Result<GuardianOutputCipher, GuardianOutputKeyringError> {
        let _authority_lease = self.acquire_authority_lease(false)?;
        verify_latest_authority(&self.directory, self.active, &self.active_key)?;
        self.active_key.cipher().map_err(Into::into)
    }

    /// Refresh to the newest authenticated activation and return its cipher.
    /// This is the production sealing path: a rotation published by another
    /// mux process advances this process instead of making all of its live
    /// pane sinks permanently fail with a stale cached activation.
    pub fn latest_active_cipher(
        &mut self,
    ) -> Result<GuardianOutputCipher, GuardianOutputKeyringError> {
        let _authority_lease = self.acquire_authority_lease(true)?;
        self.refresh_latest_authority_unlocked()?;
        self.active_key.cipher().map_err(Into::into)
    }

    /// Load a historical key by the nonsecret ID embedded in a segment header.
    pub fn cipher_for_key_id(
        &self,
        key_id: [u8; 8],
    ) -> Result<GuardianOutputCipher, GuardianOutputKeyringError> {
        let _authority_lease = self.acquire_authority_lease(false)?;
        validate_directory(&self.directory)?;
        let inventory = inventory(&self.directory)?;
        if !inventory
            .activations
            .iter()
            .any(|activation| activation.key_id == key_id)
        {
            return Err(GuardianOutputKeyringError::UnactivatedKey);
        }
        let cipher = load_key(&self.directory, key_id)?.cipher()?;
        validate_directory(&self.directory)?;
        Ok(cipher)
    }

    /// Publish a new active key without altering any existing key or activation
    /// file.  A segment that already holds the prior cipher remains pinned to
    /// that key until rollover.
    pub fn rotate(&mut self) -> Result<[u8; 8], GuardianOutputKeyringError> {
        let _authority_lease = self.acquire_authority_lease(true)?;
        self.refresh_latest_authority_unlocked()?;
        let current = inventory(&self.directory)?;
        let generation = self
            .active
            .generation
            .checked_add(1)
            .ok_or(GuardianOutputKeyringError::GenerationExhausted)?;
        let new_key = GuardianOutputKey::generate()?;
        let key_id = new_key.key_id();
        if key_id == self.active.key_id {
            return Err(GuardianOutputKeyringError::KeyIdCollision);
        }
        let active = Activation { generation, key_id };
        publish_generation(&self.directory, &current, &new_key, active)?;
        verify_latest_authority(&self.directory, active, &new_key)?;
        self.active = active;
        self.active_key = new_key;
        self.pending_generation = None;
        Ok(key_id)
    }

    fn acquire_authority_lease(
        &self,
        exclusive: bool,
    ) -> Result<Option<AuthorityFileLease>, GuardianOutputKeyringError> {
        self.use_authority_lock
            .then(|| AuthorityFileLease::acquire(&self.directory, false, exclusive))
            .transpose()
    }

    fn refresh_latest_authority_unlocked(
        &mut self,
    ) -> Result<(), GuardianOutputKeyringError> {
        validate_directory(&self.directory)?;
        let mut current = inventory(&self.directory)?;
        if current.pending.is_some() {
            current = recover_pending_intent(&self.directory, current)?;
        }
        let latest = current
            .latest
            .ok_or(GuardianOutputKeyringError::AuthorityChanged)?;
        if latest.generation < self.active.generation {
            return Err(GuardianOutputKeyringError::AuthorityChanged);
        }
        let latest_key = load_key(&self.directory, latest.key_id)?;
        if latest == self.active {
            if !self.active_key.has_same_material(&latest_key) {
                return Err(GuardianOutputKeyringError::AuthorityChanged);
            }
        } else {
            self.active = latest;
            self.active_key = latest_key;
        }
        self.pending_generation = current.pending.map(|intent| intent.generation);
        validate_directory(&self.directory)?;
        Ok(())
    }
}

fn open_inventory(
    directory: CapDir,
    inventory: Inventory,
) -> Result<GuardianOutputKeyring, GuardianOutputKeyringError> {
    let active = inventory
        .latest
        .ok_or(GuardianOutputKeyringError::OrphanedKeyMaterial)?;
    let active_key = load_key(&directory, active.key_id).map_err(|error| match error {
        GuardianOutputKeyringError::Io(ref io)
            if io.kind() == std::io::ErrorKind::NotFound =>
        {
            GuardianOutputKeyringError::MissingActivatedKey
        }
        other => other,
    })?;
    Ok(GuardianOutputKeyring {
        directory,
        use_authority_lock: false,
        active,
        active_key,
        pending_generation: inventory.pending.map(|intent| intent.generation),
    })
}

fn scrollback_keyring_path(
    scrollback_base_dir: &Path,
) -> Result<PathBuf, GuardianOutputKeyringError> {
    if scrollback_base_dir.as_os_str().is_empty() {
        return Err(std::io::Error::other("scrollback storage path is empty").into());
    }
    Ok(scrollback_base_dir.join(SCROLLBACK_KEYRING_SIBLING))
}

#[cfg(not(target_os = "wasi"))]
fn create_private_directory(parent: &CapDir, name: &str) -> std::io::Result<()> {
    let mut builder = CapDirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    parent.create_dir_with(name, &builder)
}

#[cfg(target_os = "wasi")]
fn create_private_directory(_parent: &CapDir, _name: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "private guardian key directory creation is unsupported",
    ))
}

fn open_owned_path(
    path: &Path,
    require_private: bool,
) -> Result<CapDir, GuardianOutputKeyringError> {
    let before = std::fs::symlink_metadata(path)?;
    if !before.file_type().is_dir() {
        return Err(GuardianOutputKeyringError::DirectoryNotPrivate);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        if before.uid() != rustix::process::geteuid().as_raw()
            || (require_private && before.permissions().mode() & 0o7777 != 0o700)
        {
            return Err(GuardianOutputKeyringError::DirectoryNotPrivate);
        }
    }
    let directory = CapDir::open_ambient_dir(path, cap_std::ambient_authority())?;
    validate_owned_directory(&directory, require_private)?;
    let after = std::fs::symlink_metadata(path)?;
    if !after.file_type().is_dir() {
        return Err(GuardianOutputKeyringError::IdentityChanged);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let opened = directory.dir_metadata()?;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || opened.dev() != after.dev()
            || opened.ino() != after.ino()
        {
            return Err(GuardianOutputKeyringError::IdentityChanged);
        }
    }
    Ok(directory)
}

fn validate_owned_path(
    path: &Path,
    directory: &CapDir,
    require_private: bool,
) -> Result<(), GuardianOutputKeyringError> {
    validate_owned_directory(directory, require_private)?;
    let named = std::fs::symlink_metadata(path)?;
    if !named.file_type().is_dir() {
        return Err(GuardianOutputKeyringError::IdentityChanged);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let opened = directory.dir_metadata()?;
        if named.uid() != rustix::process::geteuid().as_raw()
            || (require_private && named.permissions().mode() & 0o7777 != 0o700)
            || named.dev() != opened.dev()
            || named.ino() != opened.ino()
        {
            return Err(GuardianOutputKeyringError::IdentityChanged);
        }
    }
    Ok(())
}

fn open_private_child(
    parent: &CapDir,
    name: &str,
) -> Result<CapDir, GuardianOutputKeyringError> {
    let before = parent.symlink_metadata(name)?;
    if !before.file_type().is_dir() {
        return Err(GuardianOutputKeyringError::DirectoryNotPrivate);
    }
    let directory = parent.open_dir(name)?;
    validate_directory(&directory)?;
    let opened = directory.dir_metadata()?;
    let after = parent.symlink_metadata(name)?;
    if !after.file_type().is_dir()
        || !same_file_identity(&before, &opened)
        || !same_file_identity(&opened, &after)
    {
        return Err(GuardianOutputKeyringError::IdentityChanged);
    }
    Ok(directory)
}

fn validate_pinned_child(
    parent: &CapDir,
    name: &str,
    directory: &CapDir,
) -> Result<(), GuardianOutputKeyringError> {
    validate_directory(directory)?;
    let opened = directory.dir_metadata()?;
    let named = parent.symlink_metadata(name)?;
    if !named.file_type().is_dir() || !same_file_identity(&opened, &named) {
        return Err(GuardianOutputKeyringError::IdentityChanged);
    }
    Ok(())
}

fn authority_identity(
    directory: &CapDir,
    _path: &Path,
) -> Result<GuardianOutputAuthorityIdentity, GuardianOutputKeyringError> {
    validate_directory(directory)?;
    #[cfg(unix)]
    {
        let metadata = directory.dir_metadata()?;
        return Ok(GuardianOutputAuthorityIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    #[cfg(not(unix))]
    {
        let canonical_path = std::fs::canonicalize(_path)?;
        Ok(GuardianOutputAuthorityIdentity { canonical_path })
    }
}

struct AuthorityFileLease {
    file: std::fs::File,
}

impl AuthorityFileLease {
    fn acquire(
        directory: &CapDir,
        create: bool,
        exclusive: bool,
    ) -> Result<Self, GuardianOutputKeyringError> {
        let file = open_authority_lock_file(directory, create)?;
        if exclusive {
            fs2::FileExt::lock_exclusive(&file)?;
        } else {
            fs2::FileExt::lock_shared(&file)?;
        }
        if let Err(error) = validate_open_authority_lock_file(directory, &file) {
            let _ = fs2::FileExt::unlock(&file);
            return Err(error);
        }
        Ok(Self { file })
    }
}

impl Drop for AuthorityFileLease {
    fn drop(&mut self) {
        // Closing the freshly opened descriptor also releases the lease. The
        // explicit unlock makes the lifetime obvious and avoids retaining a
        // process-scoped lock if this type is later given another owner.
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn open_authority_lock_file(
    directory: &CapDir,
    create: bool,
) -> Result<std::fs::File, GuardianOutputKeyringError> {
    let before = match directory.symlink_metadata(AUTHORITY_LOCK_NAME) {
        Ok(metadata) => Some(validate_file_metadata(directory, metadata, 0)?),
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.mode(0o600);
    let file = directory.open_with(AUTHORITY_LOCK_NAME, &options)?;
    let opened = validate_opened_metadata(directory, &file, 0)?;
    let named = validate_named_file(directory, OsStr::new(AUTHORITY_LOCK_NAME), 0)?;
    if !same_file_identity(&opened, &named)
        || before
            .as_ref()
            .is_some_and(|before| !same_file_identity(before, &opened))
    {
        return Err(GuardianOutputKeyringError::IdentityChanged);
    }
    // The lock inode is itself part of the authority. Always make both its
    // empty contents and directory entry durable before relying on it: a
    // prior creator can make the name visible and crash before its own
    // directory sync, so observing `before = Some` is not durability proof.
    file.sync_all()?;
    sync_directory(directory)?;
    validate_opened_file(
        directory,
        OsStr::new(AUTHORITY_LOCK_NAME),
        &file,
        0,
    )?;
    Ok(file.into_std())
}

fn validate_open_authority_lock_file(
    directory: &CapDir,
    file: &std::fs::File,
) -> Result<(), GuardianOutputKeyringError> {
    let opened = file.metadata()?;
    if !opened.is_file() || opened.len() != 0 {
        return Err(GuardianOutputKeyringError::UnsafeKeyFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let directory_metadata = directory.dir_metadata()?;
        if opened.permissions().mode() & 0o7777 != 0o600
            || opened.nlink() != 1
            || opened.uid() != directory_metadata.uid()
        {
            return Err(GuardianOutputKeyringError::UnsafeKeyFile);
        }
        let named = validate_named_file(directory, OsStr::new(AUTHORITY_LOCK_NAME), 0)?;
        if opened.dev() != named.dev() || opened.ino() != named.ino() {
            return Err(GuardianOutputKeyringError::IdentityChanged);
        }
    }
    #[cfg(not(unix))]
    {
        let _named = validate_named_file(directory, OsStr::new(AUTHORITY_LOCK_NAME), 0)?;
    }
    Ok(())
}

#[derive(Default)]
struct Inventory {
    entries: usize,
    key_files: Vec<([u8; 8], u64)>,
    activations: Vec<Activation>,
    intents: Vec<PublicationIntent>,
    latest: Option<Activation>,
    pending: Option<PublicationIntent>,
}

fn inventory(directory: &CapDir) -> Result<Inventory, GuardianOutputKeyringError> {
    let mut inventory = Inventory::default();
    for entry in directory.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        if name == OsStr::new(AUTHORITY_LOCK_NAME) {
            let _lock = validate_named_file(directory, &name, 0)?;
            continue;
        }
        inventory.entries = inventory.entries.saturating_add(1);
        if inventory.entries > MAX_KEYRING_ENTRIES {
            return Err(GuardianOutputKeyringError::EntryLimit);
        }
        let name = name
            .to_str()
            .ok_or(GuardianOutputKeyringError::UnrecognizedEntry)?;
        if let Some(key_id) = parse_key_name(name) {
            let metadata = validate_named_key_candidate(directory, OsStr::new(name))?;
            inventory.key_files.push((key_id, metadata.len()));
            continue;
        }
        if let Some((generation, key_id)) = parse_intent_name(name) {
            let file = open_validated_file(directory, OsStr::new(name), INTENT_BYTES as u64)?;
            let decoded = read_intent(file)?;
            if decoded.generation != generation || decoded.key_id != key_id {
                return Err(GuardianOutputKeyringError::InvalidIntent);
            }
            inventory.intents.push(decoded);
            continue;
        }
        if let Some(maximum_bytes) = parse_stage_name(name) {
            let _stage = validate_named_stage_candidate(
                directory,
                OsStr::new(name),
                maximum_bytes,
            )?;
            continue;
        }
        if let Some(named_activation) = parse_activation_name(name) {
            let file = open_validated_file(directory, OsStr::new(name), ACTIVATION_BYTES as u64)?;
            let decoded = read_activation(file)?;
            if decoded != named_activation {
                return Err(GuardianOutputKeyringError::InvalidActivation);
            }
            inventory.activations.push(decoded);
            match inventory.latest {
                Some(current) if decoded.generation == current.generation => {
                    return Err(GuardianOutputKeyringError::AmbiguousGeneration);
                }
                Some(current) if decoded.generation < current.generation => {}
                _ => inventory.latest = Some(decoded),
            }
            continue;
        }
        return Err(GuardianOutputKeyringError::UnrecognizedEntry);
    }
    inventory
        .activations
        .sort_unstable_by_key(|activation| activation.generation);
    let mut activated_keys = HashSet::with_capacity(inventory.activations.len());
    for activation in &inventory.activations {
        if !activated_keys.insert(activation.key_id) {
            return Err(GuardianOutputKeyringError::InvalidActivation);
        }
    }
    if let Some(first) = inventory.activations.first()
        && first.generation != 1
    {
        return Err(GuardianOutputKeyringError::InvalidActivation);
    }
    for adjacent in inventory.activations.windows(2) {
        if adjacent[0].generation.checked_add(1) != Some(adjacent[1].generation) {
            return Err(GuardianOutputKeyringError::InvalidActivation);
        }
    }
    for activation in &inventory.activations {
        let _referenced_key =
            load_key(directory, activation.key_id).map_err(|error| match error {
                GuardianOutputKeyringError::Io(ref io)
                    if io.kind() == std::io::ErrorKind::NotFound =>
                {
                    GuardianOutputKeyringError::MissingActivatedKey
                }
                other => other,
            })?;
    }
    validate_intent_inventory(directory, &mut inventory)?;
    for (key_id, bytes) in &inventory.key_files {
        let referenced = inventory
            .activations
            .iter()
            .any(|activation| activation.key_id == *key_id)
            || inventory
                .pending
                .is_some_and(|intent| intent.key_id == *key_id);
        if *bytes != KEY_FILE_BYTES {
            return Err(GuardianOutputKeyringError::UnsafeKeyFile);
        }
        if !referenced {
            return Err(GuardianOutputKeyringError::OrphanedKeyMaterial);
        }
        let validated_key = load_key(directory, *key_id)?;
        if let Some(intent) = inventory
            .pending
            .filter(|intent| intent.key_id == *key_id)
            && validated_key.material_sha256() != intent.key_sha256
        {
            return Err(GuardianOutputKeyringError::InvalidIntent);
        }
    }
    if inventory.latest.is_none() && !inventory.key_files.is_empty() {
        let pending_key_id = inventory.pending.map(|intent| intent.key_id);
        if inventory.key_files.len() != 1
            || pending_key_id != inventory.key_files.first().map(|(key_id, _)| *key_id)
        {
            return Err(GuardianOutputKeyringError::OrphanedKeyMaterial);
        }
    }
    Ok(inventory)
}

fn validate_intent_inventory(
    directory: &CapDir,
    inventory: &mut Inventory,
) -> Result<(), GuardianOutputKeyringError> {
    inventory
        .intents
        .sort_unstable_by_key(|intent| intent.generation);
    let first_intent_generation = inventory.intents.first().map(|intent| intent.generation);
    let mut previous: Option<PublicationIntent> = None;
    for (index, intent) in inventory.intents.iter().copied().enumerate() {
        if intent.authority_id == [0_u8; 16]
            || intent.generation == 0
            || intent.key_id == [0_u8; 8]
            || intent.activation_digest != activation_digest(Activation {
                generation: intent.generation,
                key_id: intent.key_id,
            })
        {
            return Err(GuardianOutputKeyringError::InvalidIntent);
        }
        if let Some(predecessor) = previous {
            if predecessor.generation.checked_add(1) != Some(intent.generation)
                || predecessor.authority_id != intent.authority_id
                || intent.predecessor_generation != predecessor.generation
                || intent.predecessor_key_id != predecessor.key_id
                || intent.predecessor_activation_digest != predecessor.activation_digest
                || intent.predecessor_intent_digest != predecessor.record_digest
            {
                return Err(GuardianOutputKeyringError::InvalidIntent);
            }
        } else if intent.generation == 1 {
            if intent.predecessor_generation != 0
                || intent.predecessor_key_id != [0_u8; 8]
                || intent.predecessor_activation_digest != [0_u8; 32]
                || intent.predecessor_intent_digest != [0_u8; 32]
            {
                return Err(GuardianOutputKeyringError::InvalidIntent);
            }
        } else {
            let predecessor = inventory
                .activations
                .iter()
                .copied()
                .find(|activation| {
                    activation.generation.checked_add(1) == Some(intent.generation)
                })
                .ok_or(GuardianOutputKeyringError::InvalidIntent)?;
            if intent.predecessor_generation != predecessor.generation
                || intent.predecessor_key_id != predecessor.key_id
                || intent.predecessor_activation_digest != activation_digest(predecessor)
                || intent.predecessor_intent_digest != [0_u8; 32]
            {
                return Err(GuardianOutputKeyringError::InvalidIntent);
            }
        }

        match inventory
            .activations
            .iter()
            .copied()
            .find(|activation| activation.generation == intent.generation)
        {
            Some(activation) => {
                if activation.key_id != intent.key_id {
                    return Err(GuardianOutputKeyringError::InvalidIntent);
                }
                let key = load_key(directory, activation.key_id)?;
                if key.material_sha256() != intent.key_sha256 {
                    return Err(GuardianOutputKeyringError::InvalidIntent);
                }
            }
            None => {
                if index + 1 != inventory.intents.len()
                    || inventory.pending.is_some()
                    || inventory
                        .latest
                        .map_or(intent.generation != 1, |latest| {
                            latest.generation.checked_add(1) != Some(intent.generation)
                        })
                {
                    return Err(GuardianOutputKeyringError::InvalidIntent);
                }
                if inventory
                    .activations
                    .iter()
                    .any(|activation| activation.generation > intent.generation)
                {
                    return Err(GuardianOutputKeyringError::InvalidIntent);
                }
                inventory.pending = Some(intent);
            }
        }
        previous = Some(intent);
    }

    if let Some(first_generation) = first_intent_generation
        && inventory.activations.iter().any(|activation| {
            activation.generation >= first_generation
                && !inventory
                    .intents
                    .iter()
                    .any(|intent| intent.generation == activation.generation)
        })
    {
        return Err(GuardianOutputKeyringError::InvalidIntent);
    }
    Ok(())
}

fn provision_first(
    directory: CapDir,
    inventory: &Inventory,
) -> Result<GuardianOutputKeyring, GuardianOutputKeyringError> {
    let active_key = GuardianOutputKey::generate()?;
    let active = Activation {
        generation: 1,
        key_id: active_key.key_id(),
    };
    publish_generation(&directory, inventory, &active_key, active)?;
    verify_latest_authority(&directory, active, &active_key)?;
    Ok(GuardianOutputKeyring {
        directory,
        use_authority_lock: false,
        active,
        active_key,
        pending_generation: None,
    })
}

fn verify_latest_authority(
    directory: &CapDir,
    expected: Activation,
    expected_key: &GuardianOutputKey,
) -> Result<(), GuardianOutputKeyringError> {
    validate_directory(directory)?;
    let current = inventory(directory)?
        .latest
        .ok_or(GuardianOutputKeyringError::AuthorityChanged)?;
    if current != expected {
        return Err(GuardianOutputKeyringError::AuthorityChanged);
    }
    let current_key = load_key(directory, current.key_id)?;
    if !expected_key.has_same_material(&current_key) {
        return Err(GuardianOutputKeyringError::AuthorityChanged);
    }
    validate_directory(directory)?;
    Ok(())
}

fn publish_generation(
    directory: &CapDir,
    current: &Inventory,
    key: &GuardianOutputKey,
    activation: Activation,
) -> Result<(), GuardianOutputKeyringError> {
    let intent = publication_intent(current, key, activation)?;
    let key_stage = stage_key(directory, activation.generation, key)?;
    publish_intent(directory, intent)?;
    publish_staged_key(directory, &key_stage, key)?;
    publish_activation(directory, activation)?;
    let completed = inventory(directory)?;
    if completed.pending.is_some() || completed.latest != Some(activation) {
        return Err(GuardianOutputKeyringError::AuthorityChanged);
    }
    Ok(())
}

fn publish_activation(
    directory: &CapDir,
    activation: Activation,
) -> Result<(), GuardianOutputKeyringError> {
    let name = activation_name(activation);
    match directory.symlink_metadata(&name) {
        Ok(_) => return reconcile_activation_publication(directory, activation),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let bytes = encode_activation(activation);
    let stage = unique_stage_name(
        ACTIVATION_STAGE_PREFIX,
        activation.generation,
        activation.key_id,
        activation_digest(activation),
    )?;
    stage_exact_bytes(directory, &stage, &bytes)?;
    publish_staged_bytes(directory, &stage, &name, &bytes).map_err(|error| match error {
        GuardianOutputKeyringError::Io(ref io)
            if io.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            GuardianOutputKeyringError::AmbiguousGeneration
        }
        other => other,
    })
}

fn reconcile_activation_publication(
    directory: &CapDir,
    activation: Activation,
) -> Result<(), GuardianOutputKeyringError> {
    let name = activation_name(activation);
    let file = open_validated_file(directory, OsStr::new(&name), ACTIVATION_BYTES as u64)?;
    file.sync_all()?;
    if read_activation(file)? != activation {
        return Err(GuardianOutputKeyringError::InvalidActivation);
    }
    sync_directory(directory)?;
    Ok(())
}

fn publication_intent(
    current: &Inventory,
    key: &GuardianOutputKey,
    activation: Activation,
) -> Result<PublicationIntent, GuardianOutputKeyringError> {
    if current.pending.is_some() {
        return Err(GuardianOutputKeyringError::PendingActivation);
    }
    if key.key_id() != activation.key_id
        || current
            .key_files
            .iter()
            .any(|(key_id, _)| *key_id == activation.key_id)
        || current
            .activations
            .iter()
            .any(|existing| existing.key_id == activation.key_id)
    {
        return Err(GuardianOutputKeyringError::KeyIdCollision);
    }
    let (predecessor_generation, predecessor_key_id, predecessor_activation_digest) =
        match current.latest {
            Some(predecessor)
                if predecessor.generation.checked_add(1) == Some(activation.generation) =>
            {
                (
                    predecessor.generation,
                    predecessor.key_id,
                    activation_digest(predecessor),
                )
            }
            None if activation.generation == 1 => (0, [0_u8; 8], [0_u8; 32]),
            _ => return Err(GuardianOutputKeyringError::InvalidIntent),
        };
    let (authority_id, predecessor_intent_digest) = match current.intents.last().copied() {
        Some(predecessor) if predecessor.generation == predecessor_generation => {
            (predecessor.authority_id, predecessor.record_digest)
        }
        Some(_) => return Err(GuardianOutputKeyringError::InvalidIntent),
        None => (random_publication_identifier()?, [0_u8; 32]),
    };
    PublicationIntent::new(
        authority_id,
        activation,
        key.material_sha256(),
        predecessor_generation,
        predecessor_key_id,
        predecessor_activation_digest,
        predecessor_intent_digest,
    )
}

fn publish_intent(
    directory: &CapDir,
    intent: PublicationIntent,
) -> Result<(), GuardianOutputKeyringError> {
    let final_name = intent_name(intent);
    let bytes = encode_intent(intent);
    match directory.symlink_metadata(&final_name) {
        Ok(_) => {
            reconcile_exact_bytes(directory, &final_name, &bytes)?;
            let current = inventory(directory)?;
            if current.intents.contains(&intent) {
                return Ok(());
            }
            return Err(GuardianOutputKeyringError::InvalidIntent);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let stage = unique_stage_name(
        INTENT_STAGE_PREFIX,
        intent.generation,
        intent.key_id,
        intent.record_digest,
    )?;
    stage_exact_bytes(directory, &stage, &bytes)?;
    publish_staged_bytes(directory, &stage, &final_name, &bytes)?;
    let file = open_validated_file(
        directory,
        OsStr::new(&final_name),
        INTENT_BYTES as u64,
    )?;
    if read_intent(file)? != intent {
        return Err(GuardianOutputKeyringError::InvalidIntent);
    }
    let current = inventory(directory)?;
    if current.pending != Some(intent) {
        return Err(GuardianOutputKeyringError::InvalidIntent);
    }
    Ok(())
}

fn stage_key(
    directory: &CapDir,
    generation: u64,
    key: &GuardianOutputKey,
) -> Result<String, GuardianOutputKeyringError> {
    let name = key_stage_name(generation, key.key_id(), key.material_sha256());
    let publication = (|| {
        let mut file = create_private_file(directory, &name)?;
        key.write_exact(&mut file)?;
        file.sync_all()?;
        validate_opened_file(directory, OsStr::new(&name), &file, KEY_FILE_BYTES)?;
        sync_directory(directory)
    })();
    match publication {
        Ok(()) => Ok(name),
        Err(publication_error) => match load_exact_key_named(
            directory,
            &name,
            key.key_id(),
            key.material_sha256(),
        ) {
            Ok(_) => {
                sync_directory(directory)?;
                Ok(name)
            }
            Err(_) => Err(publication_error),
        },
    }
}

fn publish_staged_key(
    directory: &CapDir,
    stage_name: &str,
    key: &GuardianOutputKey,
) -> Result<(), GuardianOutputKeyringError> {
    let final_name = key_name(key.key_id());
    let publication = atomic_publish_noreplace(directory, stage_name, &final_name)
        .and_then(|()| sync_directory(directory));
    match publication {
        Ok(()) => {
            let _published = load_exact_key_named(
                directory,
                &final_name,
                key.key_id(),
                key.material_sha256(),
            )?;
            sync_directory(directory)
        }
        Err(publication_error) => match load_exact_key_named(
            directory,
            &final_name,
            key.key_id(),
            key.material_sha256(),
        ) {
            Ok(_) => {
                sync_directory(directory)?;
                Ok(())
            }
            Err(_) if matches!(
                publication_error,
                GuardianOutputKeyringError::Io(ref io)
                    if io.kind() == std::io::ErrorKind::AlreadyExists
            ) => Err(GuardianOutputKeyringError::KeyIdCollision),
            Err(_) => Err(publication_error),
        },
    }
}

fn stage_exact_bytes<const N: usize>(
    directory: &CapDir,
    name: &str,
    bytes: &[u8; N],
) -> Result<(), GuardianOutputKeyringError> {
    let publication = (|| {
        let mut file = create_private_file(directory, name)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        validate_opened_file(directory, OsStr::new(name), &file, N as u64)?;
        sync_directory(directory)
    })();
    match publication {
        Ok(()) => Ok(()),
        Err(publication_error) => match reconcile_exact_bytes(directory, name, bytes) {
            Ok(()) => Ok(()),
            Err(_) => Err(publication_error),
        },
    }
}

fn publish_staged_bytes<const N: usize>(
    directory: &CapDir,
    stage_name: &str,
    final_name: &str,
    bytes: &[u8; N],
) -> Result<(), GuardianOutputKeyringError> {
    let publication = atomic_publish_noreplace(directory, stage_name, final_name)
        .and_then(|()| sync_directory(directory));
    match publication {
        Ok(()) => reconcile_exact_bytes(directory, final_name, bytes),
        Err(publication_error) => match reconcile_exact_bytes(directory, final_name, bytes) {
            Ok(()) => Ok(()),
            Err(_) => Err(publication_error),
        },
    }
}

fn reconcile_exact_bytes<const N: usize>(
    directory: &CapDir,
    name: &str,
    expected: &[u8; N],
) -> Result<(), GuardianOutputKeyringError> {
    let mut file = open_validated_file(directory, OsStr::new(name), N as u64)?;
    file.sync_all()?;
    file.seek(SeekFrom::Start(0))?;
    let mut observed = [0_u8; N];
    file.read_exact(&mut observed)?;
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 || observed != *expected {
        return Err(GuardianOutputKeyringError::AuthorityChanged);
    }
    sync_directory(directory)
}

fn recover_pending_intent(
    directory: &CapDir,
    current: Inventory,
) -> Result<Inventory, GuardianOutputKeyringError> {
    let intent = current
        .pending
        .ok_or(GuardianOutputKeyringError::PendingActivation)?;
    let final_key_name = key_name(intent.key_id);
    let key = match directory.symlink_metadata(&final_key_name) {
        Ok(_) => load_exact_key_named(
            directory,
            &final_key_name,
            intent.key_id,
            intent.key_sha256,
        )?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let stage_name = key_stage_name(
                intent.generation,
                intent.key_id,
                intent.key_sha256,
            );
            let staged = load_exact_key_named(
                directory,
                &stage_name,
                intent.key_id,
                intent.key_sha256,
            )?;
            sync_directory(directory)?;
            publish_staged_key(directory, &stage_name, &staged)?;
            staged
        }
        Err(error) => return Err(error.into()),
    };
    if key.material_sha256() != intent.key_sha256 {
        return Err(GuardianOutputKeyringError::InvalidIntent);
    }

    let activation = Activation {
        generation: intent.generation,
        key_id: intent.key_id,
    };
    let activation_name = activation_name(activation);
    match directory.symlink_metadata(&activation_name) {
        Ok(_) => reconcile_activation_publication(directory, activation)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            publish_activation(directory, activation)?;
        }
        Err(error) => return Err(error.into()),
    }
    let recovered = inventory(directory)?;
    if recovered.pending.is_some() || recovered.latest != Some(activation) {
        return Err(GuardianOutputKeyringError::AuthorityChanged);
    }
    Ok(recovered)
}

fn load_exact_key_named(
    directory: &CapDir,
    name: &str,
    expected_key_id: [u8; 8],
    expected_sha256: [u8; 32],
) -> Result<GuardianOutputKey, GuardianOutputKeyringError> {
    let mut file = open_validated_file(directory, OsStr::new(name), KEY_FILE_BYTES)?;
    file.sync_all()?;
    file.seek(SeekFrom::Start(0))?;
    let key = GuardianOutputKey::read_exact(&mut file)?;
    if key.key_id() != expected_key_id || key.material_sha256() != expected_sha256 {
        return Err(GuardianOutputKeyringError::UnsafeKeyFile);
    }
    Ok(key)
}

#[cfg(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "redox",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn atomic_publish_noreplace(
    directory: &CapDir,
    stage_name: &str,
    final_name: &str,
) -> Result<(), GuardianOutputKeyringError> {
    rustix::fs::renameat_with(
        directory.as_fd(),
        stage_name,
        directory.as_fd(),
        final_name,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)
    .map_err(Into::into)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "redox",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn atomic_publish_noreplace(
    _directory: &CapDir,
    _stage_name: &str,
    _final_name: &str,
) -> Result<(), GuardianOutputKeyringError> {
    Err(GuardianOutputKeyringError::UnsupportedAtomicPublication)
}

#[cfg(test)]
fn publish_key(
    directory: &CapDir,
    key: &GuardianOutputKey,
) -> Result<(), GuardianOutputKeyringError> {
    let stage = stage_key(directory, 1, key)?;
    publish_staged_key(directory, &stage, key)
}

fn load_key(
    directory: &CapDir,
    key_id: [u8; 8],
) -> Result<GuardianOutputKey, GuardianOutputKeyringError> {
    let name = key_name(key_id);
    let mut file = open_validated_file(directory, OsStr::new(&name), KEY_FILE_BYTES)?;
    file.seek(SeekFrom::Start(0))?;
    let key = GuardianOutputKey::read_exact(&mut file)?;
    if key.key_id() != key_id {
        return Err(GuardianOutputKeyringError::UnsafeKeyFile);
    }
    Ok(key)
}

fn validate_directory(directory: &CapDir) -> Result<(), GuardianOutputKeyringError> {
    validate_owned_directory(directory, true)
}

fn validate_owned_directory(
    directory: &CapDir,
    require_private: bool,
) -> Result<(), GuardianOutputKeyringError> {
    #[cfg(not(unix))]
    let _ = require_private;
    let metadata = directory.dir_metadata()?;
    if !metadata.is_dir() {
        return Err(GuardianOutputKeyringError::DirectoryNotPrivate);
    }
    #[cfg(unix)]
    {
        if metadata.uid() != rustix::process::geteuid().as_raw()
            || (require_private && metadata.permissions().mode() & 0o7777 != 0o700)
        {
            return Err(GuardianOutputKeyringError::DirectoryNotPrivate);
        }
    }
    Ok(())
}

fn create_private_file(
    directory: &CapDir,
    name: &str,
) -> Result<CapFile, GuardianOutputKeyringError> {
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    options.mode(0o600);
    directory.open_with(name, &options).map_err(Into::into)
}

fn open_validated_file(
    directory: &CapDir,
    name: &OsStr,
    expected_bytes: u64,
) -> Result<CapFile, GuardianOutputKeyringError> {
    let before = validate_named_file(directory, name, expected_bytes)?;
    let mut options = CapOpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options)?;
    let opened = validate_opened_metadata(directory, &file, expected_bytes)?;
    if !same_file_identity(&before, &opened) {
        return Err(GuardianOutputKeyringError::IdentityChanged);
    }
    let after = validate_named_file(directory, name, expected_bytes)?;
    if !same_file_identity(&opened, &after) {
        return Err(GuardianOutputKeyringError::IdentityChanged);
    }
    Ok(file)
}

fn validate_named_file(
    directory: &CapDir,
    name: &OsStr,
    expected_bytes: u64,
) -> Result<CapMetadata, GuardianOutputKeyringError> {
    let metadata = directory.symlink_metadata(name)?;
    if metadata.file_type().is_symlink() {
        return Err(GuardianOutputKeyringError::SymlinkRejected);
    }
    validate_file_metadata(directory, metadata, expected_bytes)
}

fn validate_named_key_candidate(
    directory: &CapDir,
    name: &OsStr,
) -> Result<CapMetadata, GuardianOutputKeyringError> {
    let metadata = directory.symlink_metadata(name)?;
    if metadata.file_type().is_symlink() {
        return Err(GuardianOutputKeyringError::SymlinkRejected);
    }
    let metadata = validate_private_regular_metadata(directory, metadata)?;
    if metadata.len() > KEY_FILE_BYTES {
        return Err(GuardianOutputKeyringError::UnsafeKeyFile);
    }
    Ok(metadata)
}

fn validate_named_stage_candidate(
    directory: &CapDir,
    name: &OsStr,
    maximum_bytes: u64,
) -> Result<CapMetadata, GuardianOutputKeyringError> {
    let metadata = directory.symlink_metadata(name)?;
    if metadata.file_type().is_symlink() {
        return Err(GuardianOutputKeyringError::SymlinkRejected);
    }
    let metadata = validate_private_regular_metadata(directory, metadata)?;
    if metadata.len() > maximum_bytes {
        return Err(GuardianOutputKeyringError::UnsafeKeyFile);
    }
    Ok(metadata)
}

fn validate_opened_file(
    directory: &CapDir,
    name: &OsStr,
    file: &CapFile,
    expected_bytes: u64,
) -> Result<(), GuardianOutputKeyringError> {
    let opened = validate_opened_metadata(directory, file, expected_bytes)?;
    let named = validate_named_file(directory, name, expected_bytes)?;
    if !same_file_identity(&opened, &named) {
        return Err(GuardianOutputKeyringError::IdentityChanged);
    }
    Ok(())
}

fn validate_opened_metadata(
    directory: &CapDir,
    file: &CapFile,
    expected_bytes: u64,
) -> Result<CapMetadata, GuardianOutputKeyringError> {
    let metadata = file.metadata()?;
    validate_file_metadata(directory, metadata, expected_bytes)
}

fn validate_file_metadata(
    directory: &CapDir,
    metadata: CapMetadata,
    expected_bytes: u64,
) -> Result<CapMetadata, GuardianOutputKeyringError> {
    if metadata.len() != expected_bytes {
        return Err(GuardianOutputKeyringError::UnsafeKeyFile);
    }
    validate_private_regular_metadata(directory, metadata)
}

fn validate_private_regular_metadata(
    directory: &CapDir,
    metadata: CapMetadata,
) -> Result<CapMetadata, GuardianOutputKeyringError> {
    if !metadata.is_file() {
        return Err(GuardianOutputKeyringError::UnsafeKeyFile);
    }
    #[cfg(unix)]
    {
        let directory_metadata = directory.dir_metadata()?;
        if metadata.permissions().mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
            || metadata.uid() != directory_metadata.uid()
        {
            return Err(GuardianOutputKeyringError::UnsafeKeyFile);
        }
    }
    #[cfg(windows)]
    if metadata.number_of_links() != Some(1) {
        return Err(GuardianOutputKeyringError::UnsafeKeyFile);
    }
    Ok(metadata)
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

fn sync_directory(directory: &CapDir) -> Result<(), GuardianOutputKeyringError> {
    #[cfg(unix)]
    directory.open(".")?.sync_all()?;
    Ok(())
}

impl PublicationIntent {
    fn new(
        authority_id: [u8; 16],
        activation: Activation,
        key_sha256: [u8; 32],
        predecessor_generation: u64,
        predecessor_key_id: [u8; 8],
        predecessor_activation_digest: [u8; 32],
        predecessor_intent_digest: [u8; 32],
    ) -> Result<Self, GuardianOutputKeyringError> {
        if authority_id == [0_u8; 16]
            || activation.generation == 0
            || activation.key_id == [0_u8; 8]
            || key_sha256[..8] != activation.key_id
        {
            return Err(GuardianOutputKeyringError::InvalidIntent);
        }
        let mut intent = Self {
            authority_id,
            generation: activation.generation,
            key_id: activation.key_id,
            key_sha256,
            predecessor_generation,
            predecessor_key_id,
            predecessor_activation_digest,
            activation_digest: activation_digest(activation),
            predecessor_intent_digest,
            record_digest: [0_u8; 32],
        };
        let encoded = encode_intent(intent);
        let mut digest = Sha256::new();
        digest.update(INTENT_DIGEST_DOMAIN);
        digest.update(&encoded[..192]);
        intent.record_digest = digest.finalize().into();
        Ok(intent)
    }
}

fn activation_digest(activation: Activation) -> [u8; 32] {
    Sha256::digest(encode_activation(activation)).into()
}

fn intent_name(intent: PublicationIntent) -> String {
    format!(
        "{INTENT_PREFIX}{:020}-{}{FORMAT_SUFFIX}",
        intent.generation,
        hex::encode(intent.key_id)
    )
}

fn parse_intent_name(name: &str) -> Option<(u64, [u8; 8])> {
    let body = name.strip_prefix(INTENT_PREFIX)?.strip_suffix(FORMAT_SUFFIX)?;
    let (generation, key_id) = body.split_once('-')?;
    Some((parse_generation(generation)?, parse_key_id_hex(key_id)?))
}

fn key_stage_name(generation: u64, key_id: [u8; 8], digest: [u8; 32]) -> String {
    format!(
        "{KEY_STAGE_PREFIX}{generation:020}-{}-{}{FORMAT_SUFFIX}",
        hex::encode(key_id),
        hex::encode(digest)
    )
}

fn unique_stage_name(
    prefix: &str,
    generation: u64,
    key_id: [u8; 8],
    digest: [u8; 32],
) -> Result<String, GuardianOutputKeyringError> {
    Ok(format!(
        "{prefix}{generation:020}-{}-{}-{}{FORMAT_SUFFIX}",
        hex::encode(key_id),
        hex::encode(digest),
        hex::encode(random_publication_identifier()?)
    ))
}

fn random_publication_identifier() -> Result<[u8; 16], GuardianOutputKeyringError> {
    let mut identifier = [0_u8; 16];
    getrandom::fill(&mut identifier)
        .map_err(|_| GuardianOutputKeyringError::EntropyUnavailable)?;
    if identifier == [0_u8; 16] {
        return Err(GuardianOutputKeyringError::EntropyUnavailable);
    }
    Ok(identifier)
}

fn parse_stage_name(name: &str) -> Option<u64> {
    if let Some(body) = name
        .strip_prefix(KEY_STAGE_PREFIX)
        .and_then(|body| body.strip_suffix(FORMAT_SUFFIX))
    {
        let mut fields = body.split('-');
        parse_generation(fields.next()?)?;
        parse_key_id_hex(fields.next()?)?;
        parse_digest_hex(fields.next()?)?;
        if fields.next().is_some() {
            return None;
        }
        return Some(KEY_FILE_BYTES);
    }
    for (prefix, maximum_bytes) in [
        (ACTIVATION_STAGE_PREFIX, ACTIVATION_BYTES as u64),
        (INTENT_STAGE_PREFIX, INTENT_BYTES as u64),
    ] {
        if let Some(body) = name
            .strip_prefix(prefix)
            .and_then(|body| body.strip_suffix(FORMAT_SUFFIX))
        {
            let mut fields = body.split('-');
            parse_generation(fields.next()?)?;
            parse_key_id_hex(fields.next()?)?;
            parse_digest_hex(fields.next()?)?;
            parse_stage_nonce(fields.next()?)?;
            if fields.next().is_some() {
                return None;
            }
            return Some(maximum_bytes);
        }
    }
    None
}

fn parse_generation(encoded: &str) -> Option<u64> {
    if encoded.len() != GENERATION_DECIMAL_BYTES
        || !encoded.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let generation = encoded.parse().ok()?;
    (generation != 0).then_some(generation)
}

fn parse_key_id_hex(encoded: &str) -> Option<[u8; 8]> {
    if encoded.len() != KEY_ID_HEX_BYTES
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    hex::decode(encoded).ok()?.try_into().ok()
}

fn parse_digest_hex(encoded: &str) -> Option<[u8; 32]> {
    if encoded.len() != FULL_DIGEST_HEX_BYTES
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    hex::decode(encoded).ok()?.try_into().ok()
}

fn parse_stage_nonce(encoded: &str) -> Option<[u8; 16]> {
    if encoded.len() != STAGE_NONCE_HEX_BYTES
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    hex::decode(encoded).ok()?.try_into().ok()
}

fn key_name(key_id: [u8; 8]) -> String {
    format!("{KEY_PREFIX}{}{FORMAT_SUFFIX}", hex::encode(key_id))
}

fn parse_key_name(name: &str) -> Option<[u8; 8]> {
    let encoded = name.strip_prefix(KEY_PREFIX)?.strip_suffix(FORMAT_SUFFIX)?;
    parse_key_id_hex(encoded)
}

fn activation_name(activation: Activation) -> String {
    format!(
        "{ACTIVE_PREFIX}{:020}-{}{FORMAT_SUFFIX}",
        activation.generation,
        hex::encode(activation.key_id)
    )
}

fn parse_activation_name(name: &str) -> Option<Activation> {
    let body = name
        .strip_prefix(ACTIVE_PREFIX)?
        .strip_suffix(FORMAT_SUFFIX)?;
    let (generation, key_id) = body.split_once('-')?;
    Some(Activation {
        generation: parse_generation(generation)?,
        key_id: parse_key_id_hex(key_id)?,
    })
}

fn encode_activation(activation: Activation) -> [u8; ACTIVATION_BYTES] {
    let mut bytes = [0_u8; ACTIVATION_BYTES];
    bytes[..8].copy_from_slice(&ACTIVATION_MAGIC);
    bytes[8..12].copy_from_slice(&ACTIVATION_VERSION.to_le_bytes());
    bytes[12..16].copy_from_slice(&ACTIVATION_BYTES_U32.to_le_bytes());
    bytes[16..24].copy_from_slice(&activation.generation.to_le_bytes());
    bytes[24..32].copy_from_slice(&activation.key_id);
    let mut digest = Sha256::new();
    digest.update(ACTIVATION_DIGEST_DOMAIN);
    digest.update(&bytes[..32]);
    bytes[32..].copy_from_slice(&digest.finalize());
    bytes
}

fn read_activation(mut file: CapFile) -> Result<Activation, GuardianOutputKeyringError> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = [0_u8; ACTIVATION_BYTES];
    file.read_exact(&mut bytes)?;
    if bytes[..8] != ACTIVATION_MAGIC
        || u32::from_le_bytes(bytes[8..12].try_into().expect("fixed version slice"))
            != ACTIVATION_VERSION
        || u32::from_le_bytes(bytes[12..16].try_into().expect("fixed length slice"))
            != ACTIVATION_BYTES_U32
        || bytes[16..24] == [0_u8; 8]
    {
        return Err(GuardianOutputKeyringError::InvalidActivation);
    }
    let mut digest = Sha256::new();
    digest.update(ACTIVATION_DIGEST_DOMAIN);
    digest.update(&bytes[..32]);
    let expected_digest: [u8; 32] = digest.finalize().into();
    if expected_digest[..] != bytes[32..] {
        return Err(GuardianOutputKeyringError::InvalidActivation);
    }
    Ok(Activation {
        generation: u64::from_le_bytes(bytes[16..24].try_into().expect("fixed generation slice")),
        key_id: bytes[24..32].try_into().expect("fixed key ID slice"),
    })
}

fn encode_intent(intent: PublicationIntent) -> [u8; INTENT_BYTES] {
    let mut bytes = [0_u8; INTENT_BYTES];
    bytes[..8].copy_from_slice(&INTENT_MAGIC);
    bytes[8..12].copy_from_slice(&INTENT_VERSION.to_le_bytes());
    bytes[12..16].copy_from_slice(&INTENT_BYTES_U32.to_le_bytes());
    bytes[16..32].copy_from_slice(&intent.authority_id);
    bytes[32..40].copy_from_slice(&intent.generation.to_le_bytes());
    bytes[40..48].copy_from_slice(&intent.key_id);
    bytes[48..80].copy_from_slice(&intent.key_sha256);
    bytes[80..88].copy_from_slice(&intent.predecessor_generation.to_le_bytes());
    bytes[88..96].copy_from_slice(&intent.predecessor_key_id);
    bytes[96..128].copy_from_slice(&intent.predecessor_activation_digest);
    bytes[128..160].copy_from_slice(&intent.activation_digest);
    bytes[160..192].copy_from_slice(&intent.predecessor_intent_digest);
    bytes[192..224].copy_from_slice(&intent.record_digest);
    bytes
}

fn read_intent(mut file: CapFile) -> Result<PublicationIntent, GuardianOutputKeyringError> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = [0_u8; INTENT_BYTES];
    file.read_exact(&mut bytes)?;
    let authority_id: [u8; 16] = bytes[16..32]
        .try_into()
        .expect("fixed authority ID slice");
    let generation = u64::from_le_bytes(
        bytes[32..40]
            .try_into()
            .expect("fixed intent generation slice"),
    );
    let key_id: [u8; 8] = bytes[40..48].try_into().expect("fixed intent key ID slice");
    let key_sha256: [u8; 32] = bytes[48..80]
        .try_into()
        .expect("fixed key digest slice");
    let record_digest: [u8; 32] = bytes[192..224]
        .try_into()
        .expect("fixed intent digest slice");
    let mut digest = Sha256::new();
    digest.update(INTENT_DIGEST_DOMAIN);
    digest.update(&bytes[..192]);
    let expected_digest: [u8; 32] = digest.finalize().into();
    if bytes[..8] != INTENT_MAGIC
        || u32::from_le_bytes(bytes[8..12].try_into().expect("fixed intent version slice"))
            != INTENT_VERSION
        || u32::from_le_bytes(bytes[12..16].try_into().expect("fixed intent length slice"))
            != INTENT_BYTES_U32
        || authority_id == [0_u8; 16]
        || generation == 0
        || key_id == [0_u8; 8]
        || key_sha256[..8] != key_id
        || record_digest != expected_digest
    {
        return Err(GuardianOutputKeyringError::InvalidIntent);
    }
    Ok(PublicationIntent {
        authority_id,
        generation,
        key_id,
        key_sha256,
        predecessor_generation: u64::from_le_bytes(
            bytes[80..88]
                .try_into()
                .expect("fixed predecessor generation slice"),
        ),
        predecessor_key_id: bytes[88..96]
            .try_into()
            .expect("fixed predecessor key ID slice"),
        predecessor_activation_digest: bytes[96..128]
            .try_into()
            .expect("fixed predecessor activation digest slice"),
        activation_digest: bytes[128..160]
            .try_into()
            .expect("fixed activation digest slice"),
        predecessor_intent_digest: bytes[160..192]
            .try_into()
            .expect("fixed predecessor intent digest slice"),
        record_digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux::guardian_output_journal::{
        GuardianEncryptedScrollbackRow, GuardianScrollbackRowIdentity,
    };

    const MULTIPROCESS_KEYRING_MODE: &str = "FT_TEST_GUARDIAN_KEYRING_CHILD_MODE";
    const MULTIPROCESS_KEYRING_BASE: &str = "FT_TEST_GUARDIAN_KEYRING_CHILD_BASE";
    const MULTIPROCESS_KEYRING_START: &str = "FT_TEST_GUARDIAN_KEYRING_CHILD_START";
    const MULTIPROCESS_KEYRING_TEST: &str =
        "guardian_output_keys::tests::scrollback_sibling_interprocess_provision_and_rotation_are_serialized";

    fn wait_for_multiprocess_start(path: &Path) {
        for _ in 0..2_000 {
            if path.is_file() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for guardian keyring child start marker");
    }

    fn spawn_keyring_child(
        base: &Path,
        start: &Path,
        mode: &str,
    ) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().expect("resolve test executable"))
            .arg(MULTIPROCESS_KEYRING_TEST)
            .arg("--exact")
            .arg("--nocapture")
            .env(MULTIPROCESS_KEYRING_MODE, mode)
            .env(MULTIPROCESS_KEYRING_BASE, base)
            .env(MULTIPROCESS_KEYRING_START, start)
            .spawn()
            .expect("spawn guardian keyring child process")
    }

    #[cfg(unix)]
    fn open_private_directory(path: &std::path::Path) -> CapDir {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("make test key directory private");
        CapDir::open_ambient_dir(path, cap_std::ambient_authority())
            .expect("open test key capability directory")
    }

    #[test]
    fn activation_codec_binds_generation_and_key() {
        let expected = Activation {
            generation: 42,
            key_id: [0x5a; 8],
        };
        let bytes = encode_activation(expected);
        assert_eq!(&bytes[..8], &ACTIVATION_MAGIC);
        assert_eq!(bytes.len(), ACTIVATION_BYTES);
        assert_eq!(
            parse_activation_name(&activation_name(expected)),
            Some(expected)
        );
    }

    #[test]
    fn intent_codec_is_exactly_224_bytes_and_binds_full_key_commitment() {
        let key = GuardianOutputKey::generate().expect("generate intent-codec key");
        let activation = Activation {
            generation: 1,
            key_id: key.key_id(),
        };
        let intent = PublicationIntent::new(
            [0x33; 16],
            activation,
            key.material_sha256(),
            0,
            [0_u8; 8],
            [0_u8; 32],
            [0_u8; 32],
        )
        .expect("construct exact intent codec fixture");
        let encoded = encode_intent(intent);
        assert_eq!(encoded.len(), 224);
        assert_eq!(&encoded[..8], &INTENT_MAGIC);
        assert_eq!(&encoded[12..16], &INTENT_BYTES_U32.to_le_bytes());
        assert_eq!(&encoded[40..48], &key.key_id());
        assert_eq!(&encoded[48..80], &key.material_sha256());
        assert_eq!(&encoded[192..224], &intent.record_digest);
    }

    #[test]
    fn scrollback_sibling_reuses_rotated_historical_guardian_authority() {
        let root = tempfile::tempdir().expect("create private storage root");
        let scrollback = root.path().join("scrollback-lines");
        std::fs::create_dir(&scrollback).expect("create scrollback directory");
        let mut keyring = GuardianOutputKeyring::open_or_provision_scrollback_sibling(&scrollback)
            .expect("provision shared sibling keyring");
        let first_cipher = keyring.active_cipher().expect("derive first cipher");
        let first_key_id = first_cipher.key_id();
        let identity = GuardianScrollbackRowIdentity::new([7; 16], [8; 16], 1, 9, 0)
            .expect("construct exact row identity");
        let record = first_cipher
            .seal_scrollback_row(identity, b"historical semantic secret")
            .expect("seal historical row")
            .encode()
            .expect("encode historical row");
        assert!(!record.contains("historical semantic secret"));

        let second_key_id = keyring.rotate().expect("rotate shared keyring");
        assert_ne!(first_key_id, second_key_id);
        drop(keyring);

        let reopened = GuardianOutputKeyring::open_existing_scrollback_sibling(&scrollback)
            .expect("reopen rotated sibling keyring");
        let parsed = GuardianEncryptedScrollbackRow::parse(&record)
            .expect("parse historical encrypted row");
        assert_eq!(parsed.key_id(), first_key_id);
        let historical_cipher = reopened
            .cipher_for_key_id(parsed.key_id())
            .expect("load historical activated key");
        let plaintext = historical_cipher
            .open_scrollback_row(&parsed, [7; 16], [8; 16], 9, 0, 1024)
            .expect("authenticate historical row after rotation");
        assert_eq!(plaintext.as_slice(), b"historical semantic secret");
    }

    #[test]
    fn scrollback_sibling_interprocess_provision_and_rotation_are_serialized() {
        if let Some(mode) = std::env::var_os(MULTIPROCESS_KEYRING_MODE) {
            let mode = mode
                .to_str()
                .expect("guardian keyring child mode is valid UTF-8");
            let base = PathBuf::from(
                std::env::var_os(MULTIPROCESS_KEYRING_BASE)
                    .expect("guardian keyring child base path"),
            );
            let start = PathBuf::from(
                std::env::var_os(MULTIPROCESS_KEYRING_START)
                    .expect("guardian keyring child start path"),
            );
            wait_for_multiprocess_start(&start);
            let mut keyring =
                GuardianOutputKeyring::open_or_provision_scrollback_sibling(&base)
                    .expect("child opens shared guardian keyring");
            if mode == "rotate" {
                keyring.rotate().expect("child rotates guardian keyring");
            } else {
                assert_eq!(mode, "provision");
            }
            return;
        }

        const CHILDREN: usize = 4;
        let root = tempfile::tempdir().expect("create multiprocess storage root");
        let scrollback = root.path().join("scrollback-lines");
        std::fs::create_dir(&scrollback).expect("create multiprocess scrollback directory");

        let provision_start = root.path().join("start-provision");
        let mut provisioners = (0..CHILDREN)
            .map(|_| spawn_keyring_child(&scrollback, &provision_start, "provision"))
            .collect::<Vec<_>>();
        std::fs::File::create(&provision_start).expect("release provisioning children");
        for child in &mut provisioners {
            assert!(
                child.wait().expect("wait for provisioning child").success(),
                "concurrent guardian keyring provisioner failed"
            );
        }
        let provisioned = GuardianOutputKeyring::open_existing_scrollback_sibling(&scrollback)
            .expect("open interprocess-provisioned guardian keyring");
        assert_eq!(provisioned.active_generation(), 1);
        drop(provisioned);

        let rotation_start = root.path().join("start-rotation");
        let mut rotators = (0..CHILDREN)
            .map(|_| spawn_keyring_child(&scrollback, &rotation_start, "rotate"))
            .collect::<Vec<_>>();
        std::fs::File::create(&rotation_start).expect("release rotation children");
        for child in &mut rotators {
            assert!(
                child.wait().expect("wait for rotation child").success(),
                "concurrent guardian keyring rotator failed"
            );
        }
        let rotated = GuardianOutputKeyring::open_existing_scrollback_sibling(&scrollback)
            .expect("open interprocess-rotated guardian keyring");
        assert_eq!(
            rotated.active_generation(),
            1 + u64::try_from(CHILDREN).expect("child count fits u64")
        );
    }

    #[test]
    fn read_only_sibling_open_never_provisions_an_empty_authority() {
        let root = tempfile::tempdir().expect("create private storage root");
        let scrollback = root.path().join("scrollback-lines");
        std::fs::create_dir(&scrollback).expect("create scrollback directory");
        let keyring_path = scrollback.join(SCROLLBACK_KEYRING_SIBLING);
        let parent = open_owned_path(&scrollback, false).expect("pin scrollback parent");
        create_private_directory(&parent, SCROLLBACK_KEYRING_SIBLING)
            .expect("create empty keyring directory");

        assert!(matches!(
            GuardianOutputKeyring::open_existing_scrollback_sibling(&scrollback),
            Err(GuardianOutputKeyringError::MissingActivatedKey)
        ));
        assert_eq!(
            std::fs::read_dir(&keyring_path)
                .expect("enumerate empty keyring")
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_scrollback_parent_rejects_path_replacement() {
        let root = tempfile::tempdir().expect("create parent-replacement root");
        let scrollback = root.path().join("scrollback-lines");
        std::fs::create_dir(&scrollback).expect("create original scrollback parent");
        let parent = open_owned_path(&scrollback, false).expect("pin original scrollback parent");
        let retained = root.path().join("retained-scrollback-lines");
        std::fs::rename(&scrollback, &retained).expect("retain original pinned parent");
        std::fs::create_dir(&scrollback).expect("install replacement scrollback parent");

        assert!(matches!(
            validate_owned_path(&scrollback, &parent, false),
            Err(GuardianOutputKeyringError::IdentityChanged)
        ));
        create_private_directory(&parent, SCROLLBACK_KEYRING_SIBLING)
            .expect("descriptor-relative child remains confined to pinned parent");
        assert!(retained.join(SCROLLBACK_KEYRING_SIBLING).is_dir());
        assert!(!scrollback.join(SCROLLBACK_KEYRING_SIBLING).exists());
    }

    #[test]
    fn authority_lock_is_the_only_safe_inventory_exemption() {
        let accepted_root = tempfile::tempdir().expect("create accepted lock storage root");
        let accepted_scrollback = accepted_root.path().join("scrollback-lines");
        std::fs::create_dir(&accepted_scrollback).expect("create accepted scrollback directory");
        let accepted = GuardianOutputKeyring::open_or_provision_scrollback_sibling(
            &accepted_scrollback,
        )
        .expect("provision authority with durable lock file");
        let accepted_inventory = inventory(&accepted.directory).expect("inventory safe authority");
        assert_eq!(accepted_inventory.entries, 3);
        assert_eq!(
            std::fs::read_dir(accepted_scrollback.join(SCROLLBACK_KEYRING_SIBLING))
                .expect("enumerate accepted authority")
                .count(),
            4,
            "one key, one activation, one retained intent, and only the exempt authority lock are present"
        );

        let unknown_root = tempfile::tempdir().expect("create unknown-entry storage root");
        let unknown_scrollback = unknown_root.path().join("scrollback-lines");
        std::fs::create_dir(&unknown_scrollback).expect("create unknown scrollback directory");
        let unknown =
            GuardianOutputKeyring::open_or_provision_scrollback_sibling(&unknown_scrollback)
                .expect("provision authority for unknown-entry test");
        create_private_file(&unknown.directory, ".another-lock.v1")
            .expect("create private but unrecognized authority entry")
            .sync_all()
            .expect("sync unrecognized authority entry");
        assert!(matches!(
            GuardianOutputKeyring::open_existing_scrollback_sibling(&unknown_scrollback),
            Err(GuardianOutputKeyringError::UnrecognizedEntry)
        ));

        let nonempty_root = tempfile::tempdir().expect("create nonempty-lock storage root");
        let nonempty_scrollback = nonempty_root.path().join("scrollback-lines");
        std::fs::create_dir(&nonempty_scrollback).expect("create nonempty scrollback directory");
        let nonempty =
            GuardianOutputKeyring::open_or_provision_scrollback_sibling(&nonempty_scrollback)
                .expect("provision authority for nonempty-lock test");
        drop(nonempty);
        std::fs::OpenOptions::new()
            .write(true)
            .open(
                nonempty_scrollback
                    .join(SCROLLBACK_KEYRING_SIBLING)
                    .join(AUTHORITY_LOCK_NAME),
            )
            .expect("open authority lock for corruption")
            .write_all(b"unsafe")
            .expect("write unsafe authority lock content");
        assert!(matches!(
            GuardianOutputKeyring::open_existing_scrollback_sibling(&nonempty_scrollback),
            Err(GuardianOutputKeyringError::UnsafeKeyFile)
        ));
    }

    #[test]
    fn key_names_are_strict_lowercase_canonical_hex() {
        let key_id = [0xab; 8];
        assert_eq!(parse_key_name(&key_name(key_id)), Some(key_id));
        assert_eq!(parse_key_name("key-ABABABABABABABAB.v1"), None);
        assert_eq!(parse_key_name("key-abababababababab.v2"), None);
        assert_eq!(parse_key_name("../key-abababababababab.v1"), None);
    }

    #[test]
    fn key_debug_never_contains_key_material() {
        let key = GuardianOutputKey::generate().expect("generate key");
        let debug = format!("{key:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains(&hex::encode(key.key_id())));
    }

    #[cfg(unix)]
    #[test]
    fn provision_restart_rotate_and_historical_lookup_preserve_key_identity() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("create keyring directory");
        let mut keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("provision keyring");
        let original_id = keyring.active_key_id();
        assert_eq!(keyring.active_generation(), 1);
        assert_eq!(
            keyring.active_cipher().expect("derive active cipher").key_id(),
            original_id
        );

        for entry in std::fs::read_dir(directory.path()).expect("enumerate keyring") {
            let metadata = entry.expect("read keyring entry").metadata().expect("metadata");
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        }

        drop(keyring);
        let mut keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("reopen keyring");
        assert_eq!(keyring.active_generation(), 1);
        assert_eq!(keyring.active_key_id(), original_id);

        let rotated_id = keyring.rotate().expect("rotate keyring");
        assert_ne!(rotated_id, original_id);
        assert_eq!(keyring.active_generation(), 2);
        assert_eq!(keyring.active_key_id(), rotated_id);
        assert_eq!(
            keyring
                .cipher_for_key_id(original_id)
                .expect("load historical cipher")
                .key_id(),
            original_id
        );

        drop(keyring);
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("reopen rotated keyring");
        assert_eq!(keyring.active_generation(), 2);
        assert_eq!(keyring.active_key_id(), rotated_id);
        assert_eq!(
            keyring
                .cipher_for_key_id(original_id)
                .expect("reload historical cipher")
                .key_id(),
            original_id
        );
    }

    #[cfg(unix)]
    #[test]
    fn unretained_semantic_key_final_fails_closed_without_authorization() {
        let directory = tempfile::tempdir().expect("create interrupted keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("provision keyring");
        let unactivated = GuardianOutputKey::generate().expect("generate interrupted key");
        publish_key(&keyring.directory, &unactivated).expect("publish interrupted key leaf");
        drop(keyring);

        assert!(matches!(
            GuardianOutputKeyring::open_or_provision(open_private_directory(directory.path())),
            Err(GuardianOutputKeyringError::OrphanedKeyMaterial)
        ));
        assert!(directory.path().join(key_name(unactivated.key_id())).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn exact_publication_retries_reconcile_acknowledgement_loss() {
        let directory = tempfile::tempdir().expect("create retry keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("provision keyring");
        let next_key = GuardianOutputKey::generate().expect("generate retry key");
        let activation = Activation {
            generation: 2,
            key_id: next_key.key_id(),
        };
        let current = inventory(&keyring.directory).expect("inventory predecessor authority");
        let intent = publication_intent(&current, &next_key, activation)
            .expect("construct retry publication intent");
        let key_stage = stage_key(&keyring.directory, activation.generation, &next_key)
            .expect("stage retry key");
        publish_intent(&keyring.directory, intent).expect("publish retry intent");
        publish_intent(&keyring.directory, intent)
            .expect("reconcile exact already-published intent");
        publish_staged_key(&keyring.directory, &key_stage, &next_key)
            .expect("publish retry key");
        publish_staged_key(&keyring.directory, &key_stage, &next_key)
            .expect("reconcile exact already-published key");
        publish_activation(&keyring.directory, activation).expect("publish activation");

        publish_activation(&keyring.directory, activation)
            .expect("reconcile exact already-published activation");
        verify_latest_authority(&keyring.directory, activation, &next_key)
            .expect("retain exact reconciled authority");
    }

    #[derive(Clone, Copy, Debug)]
    enum IntentCrashCut {
        KeyStageCreated,
        KeyStagePartial,
        KeyStageComplete,
        IntentStageCreated,
        IntentStagePartial,
        IntentStageComplete,
        IntentPublished,
        KeyPublished,
        ActivationStageCreated,
        ActivationStagePartial,
        ActivationStageComplete,
        ActivationPublished,
    }

    #[cfg(unix)]
    fn exercise_intent_crash_cut(cut: IntentCrashCut) {
        let directory = tempfile::tempdir().expect("create crash-cut keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("provision crash-cut predecessor");
        let predecessor_id = keyring.active_key_id();
        let current = inventory(&keyring.directory).expect("inventory crash-cut predecessor");
        let key = GuardianOutputKey::generate().expect("generate crash-cut successor key");
        let activation = Activation {
            generation: 2,
            key_id: key.key_id(),
        };
        let intent = publication_intent(&current, &key, activation)
            .expect("construct crash-cut intent");
        let key_stage = key_stage_name(
            activation.generation,
            activation.key_id,
            key.material_sha256(),
        );
        if matches!(cut, IntentCrashCut::KeyStageCreated) {
            create_private_file(&keyring.directory, &key_stage)
                .expect("create empty deterministic key stage")
                .sync_all()
                .expect("sync empty deterministic key stage");
            sync_directory(&keyring.directory).expect("sync empty key-stage name");
        } else if matches!(cut, IntentCrashCut::KeyStagePartial) {
            let mut file = create_private_file(&keyring.directory, &key_stage)
                .expect("create partial deterministic key stage");
            file.write_all(&[0x41; GuardianOutputCipher::KEY_BYTES / 2])
                .expect("write partial deterministic key stage");
            file.sync_all().expect("sync partial deterministic key stage");
            sync_directory(&keyring.directory).expect("sync partial key-stage name");
        } else {
            stage_key(&keyring.directory, activation.generation, &key)
                .expect("synchronize deterministic key stage");
        }

        if matches!(
            cut,
            IntentCrashCut::IntentStageCreated
                | IntentCrashCut::IntentStagePartial
                | IntentCrashCut::IntentStageComplete
        ) {
            let bytes = encode_intent(intent);
            let stage = unique_stage_name(
                INTENT_STAGE_PREFIX,
                intent.generation,
                intent.key_id,
                intent.record_digest,
            )
            .expect("generate intent-stage nonce");
            if matches!(cut, IntentCrashCut::IntentStageComplete) {
                stage_exact_bytes(&keyring.directory, &stage, &bytes)
                    .expect("synchronize complete inert intent stage");
            } else {
                let mut file = create_private_file(&keyring.directory, &stage)
                    .expect("create incomplete inert intent stage");
                if matches!(cut, IntentCrashCut::IntentStagePartial) {
                    file.write_all(&bytes[..INTENT_BYTES / 2])
                        .expect("write partial inert intent stage");
                }
                file.sync_all().expect("sync incomplete intent stage");
                sync_directory(&keyring.directory).expect("sync incomplete intent-stage name");
            }
        }
        if !matches!(
            cut,
            IntentCrashCut::KeyStageCreated
                | IntentCrashCut::KeyStagePartial
                | IntentCrashCut::KeyStageComplete
                | IntentCrashCut::IntentStageCreated
                | IntentCrashCut::IntentStagePartial
                | IntentCrashCut::IntentStageComplete
        ) {
            publish_intent(&keyring.directory, intent).expect("publish durable crash-cut intent");
        }
        if matches!(
            cut,
            IntentCrashCut::KeyPublished
                | IntentCrashCut::ActivationStageCreated
                | IntentCrashCut::ActivationStagePartial
                | IntentCrashCut::ActivationStageComplete
                | IntentCrashCut::ActivationPublished
        ) {
            publish_staged_key(&keyring.directory, &key_stage, &key)
                .expect("publish crash-cut key");
        }
        if matches!(
            cut,
            IntentCrashCut::ActivationStageCreated | IntentCrashCut::ActivationStagePartial
        ) {
            let bytes = encode_activation(activation);
            let stage = unique_stage_name(
                ACTIVATION_STAGE_PREFIX,
                activation.generation,
                activation.key_id,
                activation_digest(activation),
            )
            .expect("generate incomplete activation-stage nonce");
            let mut file = create_private_file(&keyring.directory, &stage)
                .expect("create incomplete activation stage");
            if matches!(cut, IntentCrashCut::ActivationStagePartial) {
                file.write_all(&bytes[..ACTIVATION_BYTES / 2])
                    .expect("write partial activation stage");
            }
            file.sync_all().expect("sync incomplete activation stage");
            sync_directory(&keyring.directory).expect("sync incomplete activation-stage name");
        }
        if matches!(cut, IntentCrashCut::ActivationStageComplete) {
            let bytes = encode_activation(activation);
            let stage = unique_stage_name(
                ACTIVATION_STAGE_PREFIX,
                activation.generation,
                activation.key_id,
                activation_digest(activation),
            )
            .expect("generate complete activation-stage nonce");
            stage_exact_bytes(&keyring.directory, &stage, &bytes)
                .expect("synchronize complete activation stage");
        }
        if matches!(cut, IntentCrashCut::ActivationPublished) {
            publish_activation(&keyring.directory, activation)
                .expect("publish crash-cut activation");
        }
        drop(keyring);

        let reopened = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("recover crash-cut authority");
        if matches!(
            cut,
            IntentCrashCut::KeyStageCreated
                | IntentCrashCut::KeyStagePartial
                | IntentCrashCut::KeyStageComplete
                | IntentCrashCut::IntentStageCreated
                | IntentCrashCut::IntentStagePartial
                | IntentCrashCut::IntentStageComplete
        ) {
            assert_eq!(reopened.active_generation(), 1);
            assert_eq!(reopened.active_key_id(), predecessor_id);
        } else {
            assert_eq!(reopened.active_generation(), 2);
            assert_eq!(reopened.active_key_id(), activation.key_id);
            assert_eq!(reopened.pending_activation_generation(), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn retained_intent_recovers_every_publication_crash_cut_without_replacement() {
        for cut in [
            IntentCrashCut::KeyStageCreated,
            IntentCrashCut::KeyStagePartial,
            IntentCrashCut::KeyStageComplete,
            IntentCrashCut::IntentStageCreated,
            IntentCrashCut::IntentStagePartial,
            IntentCrashCut::IntentStageComplete,
            IntentCrashCut::IntentPublished,
            IntentCrashCut::KeyPublished,
            IntentCrashCut::ActivationStageCreated,
            IntentCrashCut::ActivationStagePartial,
            IntentCrashCut::ActivationStageComplete,
            IntentCrashCut::ActivationPublished,
        ] {
            exercise_intent_crash_cut(cut);
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_only_open_reports_pending_intent_and_never_authorizes_its_key() {
        let directory = tempfile::tempdir().expect("create read-only pending keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("provision read-only predecessor");
        let predecessor = keyring.active_key_id();
        let current = inventory(&keyring.directory).expect("inventory read-only predecessor");
        let key = GuardianOutputKey::generate().expect("generate pending read-only key");
        let activation = Activation {
            generation: 2,
            key_id: key.key_id(),
        };
        let _stage = stage_key(&keyring.directory, 2, &key).expect("stage pending read-only key");
        let intent = publication_intent(&current, &key, activation)
            .expect("construct pending read-only intent");
        publish_intent(&keyring.directory, intent).expect("publish pending read-only intent");
        drop(keyring);

        let read_only = GuardianOutputKeyring::open_existing(open_private_directory(
            directory.path(),
        ))
        .expect("read prior activation while next intent is pending");
        assert_eq!(read_only.active_generation(), 1);
        assert_eq!(read_only.active_key_id(), predecessor);
        assert_eq!(read_only.pending_activation_generation(), Some(2));
        assert!(matches!(
            read_only.cipher_for_key_id(activation.key_id),
            Err(GuardianOutputKeyringError::UnactivatedKey)
        ));

        drop(read_only);
        let recovered = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("writable open completes exact pending intent");
        assert_eq!(recovered.active_generation(), 2);
        assert_eq!(recovered.active_key_id(), activation.key_id);
    }

    #[cfg(unix)]
    #[test]
    fn initial_pending_intent_is_unavailable_read_only_and_recovered_writable() {
        let directory = tempfile::tempdir().expect("create initial pending keyring");
        let capability = open_private_directory(directory.path());
        let _authority_lease = AuthorityFileLease::acquire(&capability, true, true)
            .expect("acquire initial publication lease");
        let current = inventory(&capability).expect("inventory empty initial authority");
        let key = GuardianOutputKey::generate().expect("generate initial pending key");
        let activation = Activation {
            generation: 1,
            key_id: key.key_id(),
        };
        stage_key(&capability, activation.generation, &key)
            .expect("stage initial pending key");
        let intent = publication_intent(&current, &key, activation)
            .expect("construct initial pending intent");
        publish_intent(&capability, intent).expect("publish initial pending intent");

        assert!(matches!(
            GuardianOutputKeyring::open_existing(capability.try_clone().expect("clone capability")),
            Err(GuardianOutputKeyringError::PendingActivation)
        ));
        drop(_authority_lease);
        drop(capability);

        let recovered = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("recover initial pending intent");
        assert_eq!(recovered.active_generation(), 1);
        assert_eq!(recovered.active_key_id(), activation.key_id);
        assert_eq!(recovered.pending_activation_generation(), None);
    }

    #[cfg(unix)]
    #[test]
    fn partial_semantic_final_key_or_activation_is_never_ignored() {
        for partial_key in [true, false] {
            let directory = tempfile::tempdir().expect("create partial-final keyring");
            let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
                directory.path(),
            ))
            .expect("provision partial-final predecessor");
            let current = inventory(&keyring.directory).expect("inventory predecessor");
            let key = GuardianOutputKey::generate().expect("generate partial-final key");
            let activation = Activation {
                generation: 2,
                key_id: key.key_id(),
            };
            let _stage =
                stage_key(&keyring.directory, 2, &key).expect("stage partial-final key");
            let intent = publication_intent(&current, &key, activation)
                .expect("construct partial-final intent");
            publish_intent(&keyring.directory, intent).expect("publish partial-final intent");
            if partial_key {
                let mut file = create_private_file(&keyring.directory, &key_name(key.key_id()))
                    .expect("create partial semantic key");
                file.write_all(&[0x41; 7])
                    .expect("write partial semantic key");
                file.sync_all().expect("sync partial semantic key");
            } else {
                let stage = key_stage_name(2, key.key_id(), key.material_sha256());
                publish_staged_key(&keyring.directory, &stage, &key)
                    .expect("publish complete semantic key");
                let bytes = encode_activation(activation);
                let mut file =
                    create_private_file(&keyring.directory, &activation_name(activation))
                        .expect("create partial semantic activation");
                file.write_all(&bytes[..ACTIVATION_BYTES / 2])
                    .expect("write partial semantic activation");
                file.sync_all()
                    .expect("sync partial semantic activation");
            }
            sync_directory(&keyring.directory).expect("sync partial semantic final");
            drop(keyring);
            assert!(GuardianOutputKeyring::open_or_provision(open_private_directory(
                directory.path()
            ))
            .is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn intent_collision_fork_and_corruption_fail_closed() {
        let collision_directory = tempfile::tempdir().expect("create intent collision keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            collision_directory.path(),
        ))
        .expect("provision collision predecessor");
        let current = inventory(&keyring.directory).expect("inventory collision predecessor");
        let first = GuardianOutputKey::generate().expect("generate first collision candidate");
        let first_activation = Activation {
            generation: 2,
            key_id: first.key_id(),
        };
        let first_intent = publication_intent(&current, &first, first_activation)
            .expect("construct first collision intent");
        publish_intent(&keyring.directory, first_intent).expect("publish first collision intent");
        let conflicting = GuardianOutputKey::generate().expect("generate conflicting candidate");
        let conflicting_activation = Activation {
            generation: 2,
            key_id: conflicting.key_id(),
        };
        let conflicting_intent = publication_intent(&current, &conflicting, conflicting_activation)
            .expect("construct conflicting intent");
        assert!(publish_intent(&keyring.directory, conflicting_intent).is_err());
        drop(keyring);

        let corrupt_directory = tempfile::tempdir().expect("create corrupt intent keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            corrupt_directory.path(),
        ))
        .expect("provision corrupt-intent authority");
        let intent = inventory(&keyring.directory)
            .expect("inventory retained intent")
            .intents[0];
        let mut intent_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(corrupt_directory.path().join(intent_name(intent)))
            .expect("open intent for corruption");
        intent_file.seek(SeekFrom::Start(128)).expect("seek intent");
        intent_file.write_all(&[0x80]).expect("corrupt intent");
        intent_file.sync_all().expect("sync corrupt intent");
        drop(keyring);
        assert!(matches!(
            GuardianOutputKeyring::open_or_provision(open_private_directory(
                corrupt_directory.path()
            )),
            Err(GuardianOutputKeyringError::InvalidIntent)
        ));

        let fork_directory = tempfile::tempdir().expect("create forked intent keyring");
        let mut keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            fork_directory.path(),
        ))
        .expect("provision fork predecessor");
        keyring.rotate().expect("create second chained intent");
        let current = inventory(&keyring.directory).expect("inventory fork predecessor");
        let next_key = GuardianOutputKey::generate().expect("generate fork key");
        let next_activation = Activation {
            generation: 3,
            key_id: next_key.key_id(),
        };
        let latest = current.latest.expect("fork predecessor activation");
        let authority_id = current.intents.last().expect("fork predecessor intent").authority_id;
        let forked = PublicationIntent::new(
            authority_id,
            next_activation,
            next_key.material_sha256(),
            latest.generation,
            latest.key_id,
            activation_digest(latest),
            [0_u8; 32],
        )
        .expect("construct syntactically valid fork intent");
        assert!(matches!(
            publish_intent(&keyring.directory, forked),
            Err(GuardianOutputKeyringError::InvalidIntent)
        ));
        drop(keyring);
        assert!(matches!(
            GuardianOutputKeyring::open_or_provision(open_private_directory(
                fork_directory.path()
            )),
            Err(GuardianOutputKeyringError::InvalidIntent)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn legacy_activation_chain_transitions_once_into_chained_intents() {
        let directory = tempfile::tempdir().expect("create legacy transition keyring");
        let legacy_key = GuardianOutputKey::generate().expect("generate legacy key");
        let capability = open_private_directory(directory.path());
        publish_key(&capability, &legacy_key).expect("publish legacy key");
        publish_activation(
            &capability,
            Activation {
                generation: 1,
                key_id: legacy_key.key_id(),
            },
        )
        .expect("publish legacy activation");
        assert!(inventory(&capability).expect("inventory legacy chain").intents.is_empty());
        drop(capability);

        let mut keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("open legacy authority");
        keyring.rotate().expect("rotate into retained-intent protocol");
        let transitioned = inventory(&keyring.directory).expect("inventory transitioned chain");
        assert_eq!(transitioned.intents.len(), 1);
        assert_eq!(transitioned.intents[0].generation, 2);
        assert_eq!(transitioned.intents[0].predecessor_generation, 1);
        assert_eq!(transitioned.intents[0].predecessor_intent_digest, [0_u8; 32]);

        keyring.rotate().expect("extend transitioned retained-intent chain");
        let extended = inventory(&keyring.directory).expect("inventory extended chain");
        assert_eq!(extended.intents.len(), 2);
        assert_eq!(extended.intents[1].generation, 3);
        assert_eq!(
            extended.intents[1].predecessor_intent_digest,
            extended.intents[0].record_digest
        );
        assert_eq!(
            extended.intents[1].authority_id,
            extended.intents[0].authority_id
        );
    }

    #[cfg(unix)]
    #[test]
    fn stage_residues_are_canonical_private_size_bounded_and_link_safe() {
        use std::os::unix::fs::symlink;

        let oversized_directory = tempfile::tempdir().expect("create oversized-stage keyring");
        let oversized_capability = open_private_directory(oversized_directory.path());
        let key = GuardianOutputKey::generate().expect("generate oversized-stage identity");
        let stage = key_stage_name(1, key.key_id(), key.material_sha256());
        let mut file = create_private_file(&oversized_capability, &stage)
            .expect("create oversized key stage");
        file.write_all(&[0x41; GuardianOutputCipher::KEY_BYTES + 1])
            .expect("write oversized key stage");
        file.sync_all().expect("sync oversized key stage");
        drop(file);
        assert!(matches!(
            inventory(&oversized_capability),
            Err(GuardianOutputKeyringError::UnsafeKeyFile)
        ));

        let linked_directory = tempfile::tempdir().expect("create linked-stage keyring");
        let linked_capability = open_private_directory(linked_directory.path());
        let digest = [0x22; 32];
        let first_stage = unique_stage_name(
            ACTIVATION_STAGE_PREFIX,
            1,
            [0x11; 8],
            digest,
        )
        .expect("generate first linked-stage nonce");
        create_private_file(&linked_capability, &first_stage)
            .expect("create first linked stage")
            .sync_all()
            .expect("sync first linked stage");
        let second_stage = unique_stage_name(
            ACTIVATION_STAGE_PREFIX,
            1,
            [0x11; 8],
            digest,
        )
        .expect("generate second linked-stage nonce");
        std::fs::hard_link(
            linked_directory.path().join(&first_stage),
            linked_directory.path().join(&second_stage),
        )
        .expect("hard-link recognized stage names");
        assert!(matches!(
            inventory(&linked_capability),
            Err(GuardianOutputKeyringError::UnsafeKeyFile)
        ));

        let symlink_directory = tempfile::tempdir().expect("create symlink-stage keyring");
        let symlink_capability = open_private_directory(symlink_directory.path());
        let target = tempfile::NamedTempFile::new().expect("create external stage target");
        let symlink_stage = unique_stage_name(
            INTENT_STAGE_PREFIX,
            1,
            [0x11; 8],
            digest,
        )
        .expect("generate symlink-stage nonce");
        symlink(target.path(), symlink_directory.path().join(symlink_stage))
            .expect("symlink recognized stage name");
        assert!(matches!(
            inventory(&symlink_capability),
            Err(GuardianOutputKeyringError::SymlinkRejected)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn stage_residues_count_against_the_bounded_inventory() {
        let directory = tempfile::tempdir().expect("create bounded-stage keyring");
        let capability = open_private_directory(directory.path());
        for nonce in 0..=MAX_KEYRING_ENTRIES {
            let stage = format!(
                "{ACTIVATION_STAGE_PREFIX}{:020}-{}-{}-{nonce:032x}{FORMAT_SUFFIX}",
                1,
                hex::encode([0x11; 8]),
                hex::encode([0x22; 32]),
            );
            create_private_file(&capability, &stage)
                .expect("create bounded recognized stage residue");
        }
        assert!(matches!(
            inventory(&capability),
            Err(GuardianOutputKeyringError::EntryLimit)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn active_key_mutation_is_detected_before_new_cipher_use() {
        let directory = tempfile::tempdir().expect("create mutation keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("provision keyring");
        let active_path = directory.path().join(key_name(keyring.active_key_id()));
        let replacement = GuardianOutputKey::generate().expect("generate replacement key");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(active_path)
            .expect("open active key for mutation");
        replacement
            .write_exact(&mut file)
            .expect("replace active key bytes");
        file.sync_all().expect("sync mutated key");

        assert!(matches!(
            keyring.active_cipher(),
            Err(GuardianOutputKeyringError::UnsafeKeyFile)
                | Err(GuardianOutputKeyringError::AuthorityChanged)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_directory_and_symlinked_key_fail_closed() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let unsafe_directory = tempfile::tempdir().expect("create unsafe keyring directory");
        std::fs::set_permissions(
            unsafe_directory.path(),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("make directory unsafe");
        let unsafe_cap = CapDir::open_ambient_dir(
            unsafe_directory.path(),
            cap_std::ambient_authority(),
        )
        .expect("open unsafe capability directory");
        assert!(matches!(
            GuardianOutputKeyring::open_or_provision(unsafe_cap),
            Err(GuardianOutputKeyringError::DirectoryNotPrivate)
        ));

        let symlink_directory = tempfile::tempdir().expect("create symlink keyring directory");
        let external_key = tempfile::NamedTempFile::new().expect("create external key target");
        symlink(
            external_key.path(),
            symlink_directory.path().join("key-0000000000000000.v1"),
        )
        .expect("create key symlink");
        assert!(matches!(
            GuardianOutputKeyring::open_or_provision(open_private_directory(
                symlink_directory.path()
            )),
            Err(GuardianOutputKeyringError::SymlinkRejected)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn truncated_or_permission_unsafe_key_fails_closed_after_restart() {
        use std::os::unix::fs::PermissionsExt as _;

        let truncated_directory = tempfile::tempdir().expect("create truncated keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            truncated_directory.path(),
        ))
        .expect("provision truncated keyring");
        let key_path = truncated_directory
            .path()
            .join(key_name(keyring.active_key_id()));
        drop(keyring);
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&key_path)
            .expect("truncate key")
            .write_all(&[0x41; 7])
            .expect("write truncated key");
        assert!(matches!(
            GuardianOutputKeyring::open_or_provision(open_private_directory(
                truncated_directory.path()
            )),
            Err(GuardianOutputKeyringError::UnsafeKeyFile)
        ));

        let unsafe_directory = tempfile::tempdir().expect("create permission keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            unsafe_directory.path(),
        ))
        .expect("provision permission keyring");
        let key_path = unsafe_directory
            .path()
            .join(key_name(keyring.active_key_id()));
        drop(keyring);
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o640))
            .expect("make key permission unsafe");
        assert!(matches!(
            GuardianOutputKeyring::open_or_provision(open_private_directory(
                unsafe_directory.path()
            )),
            Err(GuardianOutputKeyringError::UnsafeKeyFile)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn missing_and_hard_linked_activated_keys_fail_closed() {
        let missing_directory = tempfile::tempdir().expect("create missing-key keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            missing_directory.path(),
        ))
        .expect("provision missing-key keyring");
        let active_path = missing_directory
            .path()
            .join(key_name(keyring.active_key_id()));
        drop(keyring);
        let preserved = tempfile::tempdir().expect("create preserved-key directory");
        std::fs::rename(&active_path, preserved.path().join("preserved-key"))
            .expect("preserve active key outside its authority directory");
        assert!(matches!(
            GuardianOutputKeyring::open_or_provision(open_private_directory(
                missing_directory.path()
            )),
            Err(GuardianOutputKeyringError::MissingActivatedKey)
        ));

        let linked_directory = tempfile::tempdir().expect("create linked-key keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            linked_directory.path(),
        ))
        .expect("provision linked-key keyring");
        let active_id = keyring.active_key_id();
        let mut alias_id = active_id;
        alias_id[0] ^= 0xff;
        std::fs::hard_link(
            linked_directory.path().join(key_name(active_id)),
            linked_directory.path().join(key_name(alias_id)),
        )
        .expect("create hard-linked key alias");
        drop(keyring);
        assert!(matches!(
            GuardianOutputKeyring::open_or_provision(open_private_directory(
                linked_directory.path()
            )),
            Err(GuardianOutputKeyringError::UnsafeKeyFile)
        ));
    }
}
