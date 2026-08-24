//! Private, append-only key authority for guardian output journals.
//!
//! The key directory is supplied as an already pinned capability directory.
//! Every leaf is opened without following symlinks, must be a private regular
//! file owned by the same account as the directory, and is revalidated against
//! its name after open.  Key and activation files are immutable: rotation adds
//! a new key and a monotonically numbered activation record, leaving old keys
//! available for historical segment recovery.

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
#[cfg(unix)]
use cap_fs_ext::MetadataExt as CapMetadataExt;
use cap_std::fs::{Dir as CapDir, File as CapFile, Metadata as CapMetadata};
use cap_std::fs::OpenOptions as CapOpenOptions;
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt as _;
#[cfg(unix)]
use cap_std::fs::{MetadataExt as CapUnixMetadataExt, PermissionsExt as _};
use mux::guardian_output_journal::{
    GuardianOutputCipher, GuardianOutputJournalError, GuardianOutputKey,
};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use thiserror::Error;

const KEY_PREFIX: &str = "key-";
const ACTIVE_PREFIX: &str = "active-";
const FORMAT_SUFFIX: &str = ".v1";
const KEY_ID_HEX_BYTES: usize = 16;
const GENERATION_DECIMAL_BYTES: usize = 20;
const KEY_FILE_BYTES: u64 = GuardianOutputCipher::KEY_BYTES as u64;
const ACTIVATION_MAGIC: [u8; 8] = *b"FTGACT01";
const ACTIVATION_VERSION: u32 = 1;
const ACTIVATION_BYTES: usize = 64;
const ACTIVATION_BYTES_U32: u32 = 64;
const ACTIVATION_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-output-key-activation.v1\0";
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
}

impl std::fmt::Debug for GuardianOutputKeyring {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputKeyring")
            .field("active_generation", &self.active.generation)
            .field("active_key_id", &hex::encode(self.active.key_id))
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
        let path = scrollback_keyring_path(scrollback_base_dir)?;
        match create_private_directory(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        // Synchronize even when another process created the directory. Its
        // successful `mkdir` may have become visible before that creator was
        // able to durably publish the parent-directory entry.
        sync_parent_path(&path)?;
        let directory = open_private_path(&path)?;
        let _authority_lease = AuthorityFileLease::acquire(&directory, true, true)?;
        let mut keyring = Self::open_or_provision(directory)?;
        keyring.use_authority_lock = true;
        Ok(keyring)
    }

    /// Open the existing shared keyring without creating filesystem state.
    /// Read-only transcript export uses this path only after it encounters a
    /// v3 encrypted row.
    pub fn open_existing_scrollback_sibling(
        scrollback_base_dir: &Path,
    ) -> Result<Self, GuardianOutputKeyringError> {
        let path = scrollback_keyring_path(scrollback_base_dir)?;
        let directory = open_private_path(&path)?;
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
        keyring.use_authority_lock = true;
        Ok(keyring)
    }

    /// Open a pinned key directory, provisioning its first key only when the
    /// directory is completely empty.
    pub fn open_or_provision(directory: CapDir) -> Result<Self, GuardianOutputKeyringError> {
        validate_directory(&directory)?;
        let inventory = inventory(&directory)?;
        validate_directory(&directory)?;
        if inventory.entries == 0 {
            return provision_first(directory);
        }
        open_inventory(directory, inventory)
    }

    fn open_existing(directory: CapDir) -> Result<Self, GuardianOutputKeyringError> {
        validate_directory(&directory)?;
        let inventory = inventory(&directory)?;
        validate_directory(&directory)?;
        if inventory.entries == 0 {
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
        let _authority_lease = self.acquire_authority_lease(false)?;
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
        publish_key(&self.directory, &new_key)?;
        let active = Activation { generation, key_id };
        publish_activation(&self.directory, active)?;
        verify_latest_authority(&self.directory, active, &new_key)?;
        self.active = active;
        self.active_key = new_key;
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
        let latest = inventory(&self.directory)?
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

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        builder.mode(0o700);
    }
    builder.create(path)
}

fn sync_parent_path(path: &Path) -> Result<(), GuardianOutputKeyringError> {
    #[cfg(not(windows))]
    {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("guardian keyring path has no parent"))?;
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn open_private_path(path: &Path) -> Result<CapDir, GuardianOutputKeyringError> {
    let before = std::fs::symlink_metadata(path)?;
    if !before.file_type().is_dir() {
        return Err(GuardianOutputKeyringError::DirectoryNotPrivate);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if before.permissions().mode() & 0o7777 != 0o700 {
            return Err(GuardianOutputKeyringError::DirectoryNotPrivate);
        }
    }
    let directory = CapDir::open_ambient_dir(path, cap_std::ambient_authority())?;
    validate_directory(&directory)?;
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
    if before.is_none() {
        // The lock inode is itself part of the authority. Make both its empty
        // contents and its directory entry durable before any process relies
        // on its lease to serialize key publication.
        file.sync_all()?;
        sync_directory(directory)?;
        validate_opened_file(
            directory,
            OsStr::new(AUTHORITY_LOCK_NAME),
            &file,
            0,
        )?;
    }
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
    latest: Option<Activation>,
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
        let Some(named_activation) = parse_activation_name(name) else {
            return Err(GuardianOutputKeyringError::UnrecognizedEntry);
        };
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
    for (key_id, bytes) in &inventory.key_files {
        let referenced = inventory
            .activations
            .iter()
            .any(|activation| activation.key_id == *key_id);
        if *bytes == KEY_FILE_BYTES {
            let _validated_key = load_key(directory, *key_id)?;
        } else if referenced {
            return Err(GuardianOutputKeyringError::UnsafeKeyFile);
        }
    }
    if inventory.latest.is_none() && !inventory.key_files.is_empty() {
        return Err(GuardianOutputKeyringError::OrphanedKeyMaterial);
    }
    Ok(inventory)
}

fn provision_first(directory: CapDir) -> Result<GuardianOutputKeyring, GuardianOutputKeyringError> {
    let active_key = GuardianOutputKey::generate()?;
    let active = Activation {
        generation: 1,
        key_id: active_key.key_id(),
    };
    publish_key(&directory, &active_key)?;
    publish_activation(&directory, active)?;
    verify_latest_authority(&directory, active, &active_key)?;
    Ok(GuardianOutputKeyring {
        directory,
        use_authority_lock: false,
        active,
        active_key,
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

fn publish_key(
    directory: &CapDir,
    key: &GuardianOutputKey,
) -> Result<(), GuardianOutputKeyringError> {
    let name = key_name(key.key_id());
    let mut file = create_private_file(directory, &name).map_err(|error| match error {
        GuardianOutputKeyringError::Io(ref io)
            if io.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            GuardianOutputKeyringError::KeyIdCollision
        }
        other => other,
    })?;
    key.write_exact(&mut file)?;
    file.sync_all()?;
    validate_opened_file(directory, OsStr::new(&name), &file, KEY_FILE_BYTES)?;
    sync_directory(directory)
}

fn publish_activation(
    directory: &CapDir,
    activation: Activation,
) -> Result<(), GuardianOutputKeyringError> {
    let name = activation_name(activation);
    let bytes = encode_activation(activation);
    let publication = (|| {
        let mut file = create_private_file(directory, &name).map_err(|error| match error {
            GuardianOutputKeyringError::Io(ref io)
                if io.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                GuardianOutputKeyringError::AmbiguousGeneration
            }
            other => other,
        })?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        validate_opened_file(
            directory,
            OsStr::new(&name),
            &file,
            ACTIVATION_BYTES as u64,
        )?;
        sync_directory(directory)
    })();
    match publication {
        Ok(()) => Ok(()),
        Err(publication_error) => {
            // Creation, file sync, or directory sync may report an error after
            // the exact activation became visible. Reopen and synchronize that
            // immutable record, then validate the complete contiguous
            // inventory and referenced key before treating this as an
            // acknowledgement-loss retry. A partial or conflicting generation
            // cannot pass this reconciliation.
            match reconcile_activation_publication(directory, activation) {
                Ok(()) => Ok(()),
                Err(_) => Err(publication_error),
            }
        }
    }
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
    let key = load_key(directory, activation.key_id)?;
    verify_latest_authority(directory, activation, &key)
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
    let metadata = directory.dir_metadata()?;
    if !metadata.is_dir() {
        return Err(GuardianOutputKeyringError::DirectoryNotPrivate);
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(GuardianOutputKeyringError::DirectoryNotPrivate);
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
    Ok(metadata)
}

#[cfg(unix)]
fn same_file_identity(left: &CapMetadata, right: &CapMetadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(left: &CapMetadata, right: &CapMetadata) -> bool {
    left.is_file() && right.is_file() && left.len() == right.len()
}

fn sync_directory(directory: &CapDir) -> Result<(), GuardianOutputKeyringError> {
    #[cfg(unix)]
    directory.open(".")?.sync_all()?;
    Ok(())
}

fn key_name(key_id: [u8; 8]) -> String {
    format!("{KEY_PREFIX}{}{FORMAT_SUFFIX}", hex::encode(key_id))
}

fn parse_key_name(name: &str) -> Option<[u8; 8]> {
    let encoded = name.strip_prefix(KEY_PREFIX)?.strip_suffix(FORMAT_SUFFIX)?;
    if encoded.len() != KEY_ID_HEX_BYTES
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    let decoded = hex::decode(encoded).ok()?;
    decoded.try_into().ok()
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
    if generation.len() != GENERATION_DECIMAL_BYTES
        || !generation.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let generation = generation.parse().ok()?;
    if generation == 0 {
        return None;
    }
    Some(Activation {
        generation,
        key_id: parse_key_name(&format!("{KEY_PREFIX}{key_id}{FORMAT_SUFFIX}"))?,
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
        assert_eq!(plaintext, b"historical semantic secret");
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
        create_private_directory(&keyring_path).expect("create empty keyring directory");

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
        assert_eq!(accepted_inventory.entries, 2);
        assert_eq!(
            std::fs::read_dir(accepted_scrollback.join(SCROLLBACK_KEYRING_SIBLING))
                .expect("enumerate accepted authority")
                .count(),
            3,
            "one key, one activation, and only the exempt authority lock are present"
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
    fn interrupted_rotation_key_is_preserved_but_never_becomes_authority() {
        let directory = tempfile::tempdir().expect("create interrupted keyring");
        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("provision keyring");
        let original_id = keyring.active_key_id();
        let unactivated = GuardianOutputKey::generate().expect("generate interrupted key");
        let unactivated_id = unactivated.key_id();
        publish_key(&keyring.directory, &unactivated).expect("publish interrupted key leaf");
        drop(keyring);

        let keyring = GuardianOutputKeyring::open_or_provision(open_private_directory(
            directory.path(),
        ))
        .expect("recover prior activation");
        assert_eq!(keyring.active_generation(), 1);
        assert_eq!(keyring.active_key_id(), original_id);
        assert!(matches!(
            keyring.cipher_for_key_id(unactivated_id),
            Err(GuardianOutputKeyringError::UnactivatedKey)
        ));
        assert!(directory.path().join(key_name(unactivated_id)).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn exact_activation_retry_reconciles_acknowledgement_loss() {
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
        publish_key(&keyring.directory, &next_key).expect("publish retry key");
        publish_activation(&keyring.directory, activation).expect("publish activation");

        publish_activation(&keyring.directory, activation)
            .expect("reconcile exact already-published activation");
        verify_latest_authority(&keyring.directory, activation, &next_key)
            .expect("retain exact reconciled authority");
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
