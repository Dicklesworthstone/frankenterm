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
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
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
    /// Open a pinned key directory, provisioning its first key only when the
    /// directory is completely empty.
    pub fn open_or_provision(directory: CapDir) -> Result<Self, GuardianOutputKeyringError> {
        validate_directory(&directory)?;
        let inventory = inventory(&directory)?;
        validate_directory(&directory)?;
        if inventory.entries == 0 {
            return provision_first(directory);
        }
        let active = inventory
            .latest
            .ok_or(GuardianOutputKeyringError::OrphanedKeyMaterial)?;
        let active_key = load_key(&directory, active.key_id)
            .map_err(|error| match error {
                GuardianOutputKeyringError::Io(ref io)
                    if io.kind() == std::io::ErrorKind::NotFound =>
                {
                    GuardianOutputKeyringError::MissingActivatedKey
                }
                other => other,
            })?;
        Ok(Self {
            directory,
            active,
            active_key,
        })
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
        verify_latest_authority(&self.directory, self.active, &self.active_key)?;
        self.active_key.cipher().map_err(Into::into)
    }

    /// Load a historical key by the nonsecret ID embedded in a segment header.
    pub fn cipher_for_key_id(
        &self,
        key_id: [u8; 8],
    ) -> Result<GuardianOutputCipher, GuardianOutputKeyringError> {
        if key_id == self.active.key_id {
            return self.active_cipher();
        }
        verify_latest_authority(&self.directory, self.active, &self.active_key)?;
        let inventory = inventory(&self.directory)?;
        if !inventory
            .activations
            .iter()
            .any(|activation| activation.key_id == key_id)
        {
            return Err(GuardianOutputKeyringError::UnactivatedKey);
        }
        load_key(&self.directory, key_id)?.cipher().map_err(Into::into)
    }

    /// Publish a new active key without altering any existing key or activation
    /// file.  A segment that already holds the prior cipher remains pinned to
    /// that key until rollover.
    pub fn rotate(&mut self) -> Result<[u8; 8], GuardianOutputKeyringError> {
        verify_latest_authority(&self.directory, self.active, &self.active_key)?;
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
        inventory.entries = inventory.entries.saturating_add(1);
        if inventory.entries > MAX_KEYRING_ENTRIES {
            return Err(GuardianOutputKeyringError::EntryLimit);
        }
        let name = entry.file_name();
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
