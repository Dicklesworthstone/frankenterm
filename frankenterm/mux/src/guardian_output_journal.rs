//! Durable, bounded raw PTY-output journal substrate for the guardian.
//!
//! The guardian must synchronize each raw output record before it acknowledges
//! that record to a mux.  This module owns that narrow storage contract.  It
//! opens a new segment only by one child name relative to a pinned private
//! directory descriptor.  Existing journals are still opened by the service
//! layer and passed in as exact descriptors.  Keeping arbitrary path traversal
//! outside this primitive also makes it impossible to confuse transcript
//! export with the live append authority.
//! Raw terminal bytes are always sealed with XChaCha20-Poly1305 before they
//! reach the file.  Redaction would destroy exact terminal reconstruction, so
//! this module has no plaintext persistence mode.
//!
//! A complete corrupt record fails closed and is never repaired in place.  An
//! incomplete final frame is reported as an uncommitted tail while preserving
//! every byte for diagnosis.  Appends remain disabled on that descriptor; a
//! later segment manager must seal it and publish a fresh successor segment.
//! This avoids deleting or overwriting crash evidence.

use base64::Engine as _;
use chacha20poly1305::{
    aead::{
        rand_core::{OsRng, RngCore as _},
        Aead, KeyInit, Payload,
    },
    XChaCha20Poly1305, XNonce,
};
use sha2::{Digest as _, Sha256};
use std::convert::{TryFrom, TryInto};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path};
use thiserror::Error;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const FILE_MAGIC: [u8; 8] = *b"FTGOUT01";
const RECORD_MAGIC: [u8; 8] = *b"FTGOR001";
const FORMAT_VERSION: u32 = 3;
const FILE_HEADER_BYTES: usize = 176;
const RECORD_HEADER_BYTES: usize = 96;
const FILE_HEADER_BYTES_U32: u32 = 176;
const RECORD_HEADER_BYTES_U32: u32 = 96;
const FILE_HEADER_BYTES_U64: u64 = 176;
const RECORD_HEADER_BYTES_U64: u64 = 96;
const KEY_ID_BYTES: usize = 8;
const NONCE_BYTES: usize = 24;
const AEAD_TAG_BYTES: u32 = 16;
const AEAD_TAG_BYTES_USIZE: usize = 16;
const FILE_HEADER_AEAD_DOMAIN: &[u8] = b"frankenterm.guardian-output-file-header.v3\0";
const RECORD_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-output-record.v3\0";
const AUTHENTICATED_PREFIX_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-output-authenticated-prefix.v1\0";
const PLAINTEXT_DELIVERY_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.guardian-output-plaintext-delivery.v3\0";
const RECORD_AAD_DOMAIN: &[u8] = b"frankenterm.guardian-output-aead.v3\0";
const REPLAY_STABLE_CATALOG_ADOPTION_NONCE_DOMAIN: &[u8] =
    b"frankenterm.guardian-checkpoint-catalog-adoption-nonce.v1\0";
const REPLAY_STABLE_CATALOG_ADOPTION_NONCE_DERIVATION_VERSION: u32 = 1;
const SCROLLBACK_ROW_AEAD_DOMAIN: &[u8] = b"frankenterm.scrollback-row-aead.v3\0";
const SCROLLBACK_ROW_RECORD_PREFIX: &str = "ftsl3e:";
const SCROLLBACK_ROW_FORMAT_VERSION: u32 = 3;
const SCROLLBACK_ROW_HEADER_BYTES: usize = 96;
const SCROLLBACK_ROW_MAX_PLAINTEXT_BYTES: u32 = 16 * 1024 * 1024;
const SCROLLBACK_ROW_MAX_PLAINTEXT_BYTES_USIZE: usize = 16 * 1024 * 1024;
const SCROLLBACK_MANIFEST_AEAD_DOMAIN: &[u8] =
    b"frankenterm.scrollback-manifest-authentication.v1\0";
const SCROLLBACK_MANIFEST_AUTH_PREFIX: &str = "ftsma1e:";
const SCROLLBACK_MANIFEST_AUTH_VERSION: u32 = 1;
const SCROLLBACK_MANIFEST_AUTH_BYTES: usize = 56;
const SCROLLBACK_MANIFEST_AUTH_ENCODED_BYTES: usize = 75;
const SCROLLBACK_MANIFEST_MAX_CANONICAL_BYTES: u32 = 1024 * 1024;
const SCROLLBACK_APPEND_WAL_AEAD_DOMAIN: &[u8] =
    b"frankenterm.scrollback-append-wal-authentication.v1\0";
const SCROLLBACK_APPEND_WAL_AUTH_PREFIX: &str = "ftswa1e:";
const SCROLLBACK_APPEND_WAL_AUTH_VERSION: u32 = 1;
const SCROLLBACK_APPEND_WAL_AUTH_BYTES: usize = 56;
const SCROLLBACK_APPEND_WAL_AUTH_ENCODED_BYTES: usize = 75;
const SCROLLBACK_APPEND_WAL_MAX_CANONICAL_BYTES: u32 = 1024 * 1024;
const RECOVERY_MAX_RECORDS: u64 = 4_096;
const RECOVERY_MAX_PLAINTEXT_BYTES: u64 = 64 * 1024 * 1024;

/// Opaque guardian authentication seal for one canonical v3 scrollback
/// generation/pointer manifest.
///
/// The seal contains no manifest plaintext. Its AEAD tag authenticates the
/// caller-supplied canonical manifest bytes as associated data under the
/// historical key ID embedded in this record.
pub struct GuardianScrollbackManifestAuthentication {
    key_id: [u8; KEY_ID_BYTES],
    canonical_bytes: u32,
    nonce: [u8; NONCE_BYTES],
    authentication_tag: [u8; AEAD_TAG_BYTES_USIZE],
}

impl GuardianScrollbackManifestAuthentication {
    #[must_use]
    pub fn has_authenticated_prefix(record: &str) -> bool {
        record.starts_with(SCROLLBACK_MANIFEST_AUTH_PREFIX)
    }

    pub fn parse(record: &str) -> Result<Self, GuardianScrollbackManifestError> {
        let encoded = record
            .strip_prefix(SCROLLBACK_MANIFEST_AUTH_PREFIX)
            .ok_or(GuardianScrollbackManifestError::MalformedRecord)?;
        if encoded.len() != SCROLLBACK_MANIFEST_AUTH_ENCODED_BYTES {
            return Err(GuardianScrollbackManifestError::MalformedRecord);
        }
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(encoded)
            .map_err(|_| GuardianScrollbackManifestError::MalformedRecord)?;
        if bytes.len() != SCROLLBACK_MANIFEST_AUTH_BYTES {
            return Err(GuardianScrollbackManifestError::MalformedRecord);
        }
        if base64::engine::general_purpose::STANDARD_NO_PAD.encode(&bytes) != encoded {
            return Err(GuardianScrollbackManifestError::NonCanonicalRecord);
        }
        let version = read_u32(&bytes[0..4]);
        if version != SCROLLBACK_MANIFEST_AUTH_VERSION {
            return Err(GuardianScrollbackManifestError::UnsupportedVersion { observed: version });
        }
        let mut key_id = [0; KEY_ID_BYTES];
        key_id.copy_from_slice(&bytes[4..12]);
        let canonical_bytes = read_u32(&bytes[12..16]);
        if canonical_bytes == 0 || canonical_bytes > SCROLLBACK_MANIFEST_MAX_CANONICAL_BYTES {
            return Err(GuardianScrollbackManifestError::CanonicalByteLimit);
        }
        let mut nonce = [0; NONCE_BYTES];
        nonce.copy_from_slice(&bytes[16..40]);
        let mut authentication_tag = [0; AEAD_TAG_BYTES_USIZE];
        authentication_tag.copy_from_slice(&bytes[40..56]);
        Ok(Self {
            key_id,
            canonical_bytes,
            nonce,
            authentication_tag,
        })
    }

    #[must_use]
    pub const fn key_id(&self) -> [u8; KEY_ID_BYTES] {
        self.key_id
    }

    pub fn encode(&self) -> String {
        let mut bytes = Vec::with_capacity(SCROLLBACK_MANIFEST_AUTH_BYTES);
        bytes.extend_from_slice(&SCROLLBACK_MANIFEST_AUTH_VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.key_id);
        bytes.extend_from_slice(&self.canonical_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.authentication_tag);
        let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes);
        format!("{SCROLLBACK_MANIFEST_AUTH_PREFIX}{encoded}")
    }
}

impl std::fmt::Debug for GuardianScrollbackManifestAuthentication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianScrollbackManifestAuthentication")
            .field("key_id", &"[REDACTED]")
            .field("canonical_bytes", &self.canonical_bytes)
            .field("nonce", &"[REDACTED]")
            .field("authentication_tag", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum GuardianScrollbackManifestError {
    #[error("authenticated scrollback manifest record is malformed")]
    MalformedRecord,
    #[error("authenticated scrollback manifest record is not canonical")]
    NonCanonicalRecord,
    #[error("unsupported authenticated scrollback manifest version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("canonical scrollback manifest exceeds its hard byte limit")]
    CanonicalByteLimit,
    #[error("authenticated scrollback manifest key identity does not match")]
    KeyIdentityMismatch,
    #[error("operating system entropy is unavailable for manifest authentication")]
    EntropyUnavailable,
    #[error("scrollback manifest authentication failed")]
    AuthenticationFailed,
}

/// Opaque guardian authentication seal for one canonical exact-row append
/// WAL transaction. This deliberately uses a distinct type, prefix, and AEAD
/// domain from the v3 pointer manifest so neither authority can be replayed as
/// the other even when their canonical byte lengths happen to match.
pub struct GuardianScrollbackAppendWalAuthentication {
    key_id: [u8; KEY_ID_BYTES],
    canonical_bytes: u32,
    nonce: [u8; NONCE_BYTES],
    authentication_tag: [u8; AEAD_TAG_BYTES_USIZE],
}

impl GuardianScrollbackAppendWalAuthentication {
    pub fn parse(record: &str) -> Result<Self, GuardianScrollbackAppendWalError> {
        let encoded = record
            .strip_prefix(SCROLLBACK_APPEND_WAL_AUTH_PREFIX)
            .ok_or(GuardianScrollbackAppendWalError::MalformedRecord)?;
        if encoded.len() != SCROLLBACK_APPEND_WAL_AUTH_ENCODED_BYTES {
            return Err(GuardianScrollbackAppendWalError::MalformedRecord);
        }
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(encoded)
            .map_err(|_| GuardianScrollbackAppendWalError::MalformedRecord)?;
        if bytes.len() != SCROLLBACK_APPEND_WAL_AUTH_BYTES {
            return Err(GuardianScrollbackAppendWalError::MalformedRecord);
        }
        if base64::engine::general_purpose::STANDARD_NO_PAD.encode(&bytes) != encoded {
            return Err(GuardianScrollbackAppendWalError::NonCanonicalRecord);
        }
        let version = read_u32(&bytes[0..4]);
        if version != SCROLLBACK_APPEND_WAL_AUTH_VERSION {
            return Err(GuardianScrollbackAppendWalError::UnsupportedVersion { observed: version });
        }
        let mut key_id = [0; KEY_ID_BYTES];
        key_id.copy_from_slice(&bytes[4..12]);
        let canonical_bytes = read_u32(&bytes[12..16]);
        if canonical_bytes == 0 || canonical_bytes > SCROLLBACK_APPEND_WAL_MAX_CANONICAL_BYTES {
            return Err(GuardianScrollbackAppendWalError::CanonicalByteLimit);
        }
        let mut nonce = [0; NONCE_BYTES];
        nonce.copy_from_slice(&bytes[16..40]);
        let mut authentication_tag = [0; AEAD_TAG_BYTES_USIZE];
        authentication_tag.copy_from_slice(&bytes[40..56]);
        Ok(Self {
            key_id,
            canonical_bytes,
            nonce,
            authentication_tag,
        })
    }

    #[must_use]
    pub const fn key_id(&self) -> [u8; KEY_ID_BYTES] {
        self.key_id
    }

    pub fn encode(&self) -> String {
        let mut bytes = Vec::with_capacity(SCROLLBACK_APPEND_WAL_AUTH_BYTES);
        bytes.extend_from_slice(&SCROLLBACK_APPEND_WAL_AUTH_VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.key_id);
        bytes.extend_from_slice(&self.canonical_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.authentication_tag);
        let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes);
        format!("{SCROLLBACK_APPEND_WAL_AUTH_PREFIX}{encoded}")
    }
}

impl std::fmt::Debug for GuardianScrollbackAppendWalAuthentication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianScrollbackAppendWalAuthentication")
            .field("key_id", &"[REDACTED]")
            .field("canonical_bytes", &self.canonical_bytes)
            .field("nonce", &"[REDACTED]")
            .field("authentication_tag", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum GuardianScrollbackAppendWalError {
    #[error("authenticated scrollback append WAL record is malformed")]
    MalformedRecord,
    #[error("authenticated scrollback append WAL record is not canonical")]
    NonCanonicalRecord,
    #[error("unsupported authenticated scrollback append WAL version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("canonical scrollback append WAL exceeds its hard byte limit")]
    CanonicalByteLimit,
    #[error("authenticated scrollback append WAL key identity does not match")]
    KeyIdentityMismatch,
    #[error("operating system entropy is unavailable for append WAL authentication")]
    EntropyUnavailable,
    #[error("scrollback append WAL authentication failed")]
    AuthenticationFailed,
}

/// Authenticated storage identity for one exact semantic cold-scrollback row.
///
/// The revision is the spill transaction revision at which the row was
/// admitted.  A clear creates a fresh content epoch, so a ciphertext from a
/// prior logical generation cannot be replayed into the same row/sequence.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianScrollbackRowIdentity {
    durable_pane_id: [u8; 16],
    content_epoch: [u8; 16],
    revision: u64,
    stable_row: i64,
    sequence: u64,
}

impl GuardianScrollbackRowIdentity {
    pub fn new(
        durable_pane_id: [u8; 16],
        content_epoch: [u8; 16],
        revision: u64,
        stable_row: i64,
        sequence: u64,
    ) -> Result<Self, GuardianScrollbackRowError> {
        if durable_pane_id == [0; 16] {
            return Err(GuardianScrollbackRowError::InvalidIdentity(
                "durable pane ID must be nonzero",
            ));
        }
        if content_epoch == [0; 16] {
            return Err(GuardianScrollbackRowError::InvalidIdentity(
                "content epoch must be nonzero",
            ));
        }
        if revision == 0 {
            return Err(GuardianScrollbackRowError::InvalidIdentity(
                "revision must be nonzero",
            ));
        }
        Ok(Self {
            durable_pane_id,
            content_epoch,
            revision,
            stable_row,
            sequence,
        })
    }

    #[must_use]
    pub const fn durable_pane_id(self) -> [u8; 16] {
        self.durable_pane_id
    }

    #[must_use]
    pub const fn content_epoch(self) -> [u8; 16] {
        self.content_epoch
    }

    #[must_use]
    pub const fn revision(self) -> u64 {
        self.revision
    }

    #[must_use]
    pub const fn stable_row(self) -> i64 {
        self.stable_row
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }
}

impl std::fmt::Debug for GuardianScrollbackRowIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianScrollbackRowIdentity")
            .field("durable_pane_id", &"[REDACTED]")
            .field("content_epoch", &"[REDACTED]")
            .field("revision", &self.revision)
            .field("stable_row", &self.stable_row)
            .field("sequence", &self.sequence)
            .finish()
    }
}

/// Parsed, encrypted v3 cold-scrollback record.
///
/// Ciphertext and nonce remain private.  The only exposed field is the
/// nonsecret key fingerprint needed for historical keyring lookup.
pub struct GuardianEncryptedScrollbackRow {
    key_id: [u8; KEY_ID_BYTES],
    identity: GuardianScrollbackRowIdentity,
    plaintext_bytes: u32,
    nonce: [u8; NONCE_BYTES],
    ciphertext: Vec<u8>,
}

impl GuardianEncryptedScrollbackRow {
    #[must_use]
    pub fn has_encrypted_prefix(record: &str) -> bool {
        record.starts_with(SCROLLBACK_ROW_RECORD_PREFIX)
    }

    /// Parse the canonical bounded text envelope stored by the scrollback log.
    pub fn parse(record: &str) -> Result<Self, GuardianScrollbackRowError> {
        let encoded = record
            .strip_prefix(SCROLLBACK_ROW_RECORD_PREFIX)
            .ok_or(GuardianScrollbackRowError::MalformedRecord)?;
        let maximum_binary_bytes = SCROLLBACK_ROW_HEADER_BYTES
            .checked_add(SCROLLBACK_ROW_MAX_PLAINTEXT_BYTES_USIZE)
            .and_then(|bytes| bytes.checked_add(AEAD_TAG_BYTES_USIZE))
            .ok_or(GuardianScrollbackRowError::ArithmeticOverflow)?;
        let maximum_encoded_bytes = maximum_binary_bytes
            .checked_add(2)
            .and_then(|bytes| bytes.checked_div(3))
            .and_then(|bytes| bytes.checked_mul(4))
            .ok_or(GuardianScrollbackRowError::ArithmeticOverflow)?;
        if encoded.len() > maximum_encoded_bytes {
            return Err(GuardianScrollbackRowError::RecordByteLimit);
        }
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(encoded)
            .map_err(|_| GuardianScrollbackRowError::MalformedRecord)?;
        if base64::engine::general_purpose::STANDARD_NO_PAD.encode(&bytes) != encoded {
            return Err(GuardianScrollbackRowError::NonCanonicalRecord);
        }
        if bytes.len() < SCROLLBACK_ROW_HEADER_BYTES + AEAD_TAG_BYTES_USIZE {
            return Err(GuardianScrollbackRowError::MalformedRecord);
        }
        let version = read_u32(&bytes[0..4]);
        if version != SCROLLBACK_ROW_FORMAT_VERSION {
            return Err(GuardianScrollbackRowError::UnsupportedVersion { observed: version });
        }
        let mut key_id = [0; KEY_ID_BYTES];
        key_id.copy_from_slice(&bytes[4..12]);
        let mut durable_pane_id = [0; 16];
        durable_pane_id.copy_from_slice(&bytes[12..28]);
        let mut content_epoch = [0; 16];
        content_epoch.copy_from_slice(&bytes[28..44]);
        let revision = read_u64(&bytes[44..52]);
        let stable_row = read_i64(&bytes[52..60]);
        let sequence = read_u64(&bytes[60..68]);
        let plaintext_bytes = read_u32(&bytes[68..72]);
        if plaintext_bytes == 0 || plaintext_bytes > SCROLLBACK_ROW_MAX_PLAINTEXT_BYTES {
            return Err(GuardianScrollbackRowError::RecordByteLimit);
        }
        let expected_ciphertext_bytes = usize::try_from(plaintext_bytes)
            .map_err(|_| GuardianScrollbackRowError::ArithmeticOverflow)?
            .checked_add(AEAD_TAG_BYTES_USIZE)
            .ok_or(GuardianScrollbackRowError::ArithmeticOverflow)?;
        if bytes.len() != SCROLLBACK_ROW_HEADER_BYTES + expected_ciphertext_bytes {
            return Err(GuardianScrollbackRowError::CiphertextLengthMismatch);
        }
        let identity = GuardianScrollbackRowIdentity::new(
            durable_pane_id,
            content_epoch,
            revision,
            stable_row,
            sequence,
        )?;
        let mut nonce = [0; NONCE_BYTES];
        nonce.copy_from_slice(&bytes[72..SCROLLBACK_ROW_HEADER_BYTES]);
        Ok(Self {
            key_id,
            identity,
            plaintext_bytes,
            nonce,
            ciphertext: bytes[SCROLLBACK_ROW_HEADER_BYTES..].to_vec(),
        })
    }

    #[must_use]
    pub const fn key_id(&self) -> [u8; KEY_ID_BYTES] {
        self.key_id
    }

    #[must_use]
    pub const fn identity(&self) -> GuardianScrollbackRowIdentity {
        self.identity
    }

    #[must_use]
    pub const fn plaintext_bytes(&self) -> u32 {
        self.plaintext_bytes
    }

    /// Encode the canonical opaque storage record.
    pub fn encode(&self) -> Result<String, GuardianScrollbackRowError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(SCROLLBACK_ROW_HEADER_BYTES + self.ciphertext.len())
            .map_err(|_| GuardianScrollbackRowError::AllocationFailed)?;
        bytes.extend_from_slice(&SCROLLBACK_ROW_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.key_id);
        bytes.extend_from_slice(&self.identity.durable_pane_id);
        bytes.extend_from_slice(&self.identity.content_epoch);
        bytes.extend_from_slice(&self.identity.revision.to_le_bytes());
        bytes.extend_from_slice(&self.identity.stable_row.to_le_bytes());
        bytes.extend_from_slice(&self.identity.sequence.to_le_bytes());
        bytes.extend_from_slice(&self.plaintext_bytes.to_le_bytes());
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&self.ciphertext);
        let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes);
        Ok(format!("{SCROLLBACK_ROW_RECORD_PREFIX}{encoded}"))
    }
}

impl std::fmt::Debug for GuardianEncryptedScrollbackRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianEncryptedScrollbackRow")
            .field("key_id", &"[REDACTED]")
            .field("identity", &self.identity)
            .field("plaintext_bytes", &self.plaintext_bytes)
            .field("nonce", &"[REDACTED]")
            .field("ciphertext", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum GuardianScrollbackRowError {
    #[error("invalid encrypted scrollback row identity: {0}")]
    InvalidIdentity(&'static str),
    #[error("encrypted scrollback row is malformed")]
    MalformedRecord,
    #[error("encrypted scrollback row encoding is not canonical")]
    NonCanonicalRecord,
    #[error("unsupported encrypted scrollback row version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("encrypted scrollback row exceeds its hard byte limit")]
    RecordByteLimit,
    #[error("encrypted scrollback row ciphertext length is invalid")]
    CiphertextLengthMismatch,
    #[error("encrypted scrollback row key identity does not match")]
    KeyIdentityMismatch,
    #[error("encrypted scrollback row storage identity does not match")]
    StorageIdentityMismatch,
    #[error("encrypted scrollback row allocation failed")]
    AllocationFailed,
    #[error("encrypted scrollback row arithmetic overflow")]
    ArithmeticOverflow,
    #[error("operating system entropy is unavailable for scrollback encryption")]
    EntropyUnavailable,
    #[error("scrollback row encryption failed")]
    EncryptionFailed,
    #[error("scrollback row authentication or decryption failed")]
    DecryptionFailed,
}

/// Hard admission limits for one immutable guardian output-log segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianOutputJournalLimits {
    pub max_record_bytes: u32,
    pub max_log_bytes: u64,
    pub max_records: u64,
}

impl Default for GuardianOutputJournalLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: 1024 * 1024,
            max_log_bytes: 1024 * 1024 * 1024,
            max_records: 1_000_000,
        }
    }
}

impl GuardianOutputJournalLimits {
    fn validate(self) -> Result<Self, GuardianOutputJournalError> {
        if self.max_record_bytes == 0 {
            return Err(GuardianOutputJournalError::InvalidLimits(
                "max_record_bytes must be nonzero",
            ));
        }
        if self.max_record_bytes > u32::MAX - AEAD_TAG_BYTES {
            return Err(GuardianOutputJournalError::InvalidLimits(
                "max_record_bytes must leave room for the AEAD tag",
            ));
        }
        if self.max_records == 0 {
            return Err(GuardianOutputJournalError::InvalidLimits(
                "max_records must be nonzero",
            ));
        }
        let minimum = FILE_HEADER_BYTES_U64
            .checked_add(RECORD_HEADER_BYTES_U64)
            .and_then(|bytes| bytes.checked_add(u64::from(AEAD_TAG_BYTES) + 1))
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if self.max_log_bytes < minimum {
            return Err(GuardianOutputJournalError::InvalidLimits(
                "max_log_bytes cannot hold a header and one nonempty record",
            ));
        }
        Ok(self)
    }
}

/// Mandatory XChaCha20-Poly1305 encryption authority for raw terminal bytes.
#[derive(Clone)]
pub struct GuardianOutputCipher {
    cipher: XChaCha20Poly1305,
    key_id: [u8; KEY_ID_BYTES],
}

/// In-memory guardian output-journal key material.
///
/// This type is intentionally non-cloneable, zeroizes its owned bytes on drop,
/// and never exposes those bytes through `Debug`.  The service keyring may use
/// `write_exact` only while provisioning a private, securely opened key file;
/// all ordinary consumers should derive a [`GuardianOutputCipher`] instead.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct GuardianOutputKey {
    bytes: [u8; GuardianOutputCipher::KEY_BYTES],
}

impl GuardianOutputKey {
    /// Generate a new key from the operating system random source.
    pub fn generate() -> Result<Self, GuardianOutputJournalError> {
        let mut bytes = [0_u8; GuardianOutputCipher::KEY_BYTES];
        if OsRng.try_fill_bytes(&mut bytes).is_err() {
            bytes.zeroize();
            return Err(GuardianOutputJournalError::EntropyUnavailable);
        }
        if let Err(error) = GuardianOutputCipher::try_from_key_slice(&bytes) {
            bytes.zeroize();
            return Err(error);
        }
        Ok(Self { bytes })
    }

    /// Load exactly one key and reject truncated or trailing bytes.
    pub fn read_exact<R: Read>(reader: &mut R) -> Result<Self, GuardianOutputJournalError> {
        let mut bytes = [0_u8; GuardianOutputCipher::KEY_BYTES];
        if let Err(error) = reader.read_exact(&mut bytes) {
            bytes.zeroize();
            return Err(GuardianOutputJournalError::KeyFileRead(error));
        }
        let mut trailing = [0_u8; 1];
        match reader.read(&mut trailing) {
            Ok(0) => {}
            Ok(_) => {
                bytes.zeroize();
                return Err(GuardianOutputJournalError::InvalidEncryptionKey(
                    "guardian output key file contains trailing bytes",
                ));
            }
            Err(error) => {
                bytes.zeroize();
                return Err(GuardianOutputJournalError::KeyFileRead(error));
            }
        }
        if let Err(error) = GuardianOutputCipher::try_from_key_slice(&bytes) {
            bytes.zeroize();
            return Err(error);
        }
        Ok(Self { bytes })
    }

    /// Persist the exact key bytes to a caller-owned private descriptor.
    pub fn write_exact<W: Write>(&self, writer: &mut W) -> Result<(), GuardianOutputJournalError> {
        writer
            .write_all(&self.bytes)
            .map_err(GuardianOutputJournalError::KeyFileWrite)
    }

    /// Derive the encryption authority without exposing raw key material.
    pub fn cipher(&self) -> Result<GuardianOutputCipher, GuardianOutputJournalError> {
        GuardianOutputCipher::try_from_key_slice(&self.bytes)
    }

    /// Return the nonsecret fingerprint used to bind segments to this key.
    pub fn key_id(&self) -> [u8; KEY_ID_BYTES] {
        let digest = self.material_sha256();
        let mut key_id = [0_u8; KEY_ID_BYTES];
        key_id.copy_from_slice(&digest[..KEY_ID_BYTES]);
        key_id
    }

    /// Return the full SHA-256 commitment to this uniformly random key.
    ///
    /// This commitment is used only by private key-publication records to
    /// distinguish a complete staged key from a conflicting same-prefix key.
    /// It is not key material and must not be used as an encryption key.
    #[must_use]
    pub fn material_sha256(&self) -> [u8; 32] {
        Sha256::digest(self.bytes).into()
    }

    /// Compare two in-memory authorities without exposing either key.
    #[must_use]
    pub fn has_same_material(&self, other: &Self) -> bool {
        self.bytes
            .iter()
            .zip(other.bytes.iter())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

impl std::fmt::Debug for GuardianOutputKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputKey")
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

impl GuardianOutputCipher {
    pub const KEY_BYTES: usize = 32;

    /// Return the nonsecret fingerprint bound into each encrypted segment.
    #[must_use]
    pub const fn key_id(&self) -> [u8; KEY_ID_BYTES] {
        self.key_id
    }

    pub fn try_from_key_slice(key: &[u8]) -> Result<Self, GuardianOutputJournalError> {
        if key.len() != Self::KEY_BYTES {
            return Err(GuardianOutputJournalError::InvalidEncryptionKey(
                "guardian output key must contain exactly 32 bytes",
            ));
        }
        if key.iter().all(|byte| *byte == 0) {
            return Err(GuardianOutputJournalError::InvalidEncryptionKey(
                "guardian output key cannot be all zero",
            ));
        }
        let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| {
            GuardianOutputJournalError::InvalidEncryptionKey(
                "guardian output cipher initialization failed",
            )
        })?;
        let digest = Sha256::digest(key);
        let mut key_id = [0_u8; KEY_ID_BYTES];
        key_id.copy_from_slice(&digest[..KEY_ID_BYTES]);
        Ok(Self { cipher, key_id })
    }

    /// Seal one lossless semantic cold-scrollback row under the guardian key.
    ///
    /// This is deliberately narrower than a generic metadata-encryption API:
    /// the authenticated envelope always carries the complete scrollback
    /// storage identity and the v3 domain/version discriminator.
    pub fn seal_scrollback_row(
        &self,
        identity: GuardianScrollbackRowIdentity,
        plaintext: &[u8],
    ) -> Result<GuardianEncryptedScrollbackRow, GuardianScrollbackRowError> {
        let plaintext_bytes = u32::try_from(plaintext.len())
            .map_err(|_| GuardianScrollbackRowError::RecordByteLimit)?;
        if plaintext_bytes == 0 || plaintext_bytes > SCROLLBACK_ROW_MAX_PLAINTEXT_BYTES {
            return Err(GuardianScrollbackRowError::RecordByteLimit);
        }
        let mut nonce = [0; NONCE_BYTES];
        if OsRng.try_fill_bytes(&mut nonce).is_err() {
            nonce.zeroize();
            return Err(GuardianScrollbackRowError::EntropyUnavailable);
        }
        let aad = scrollback_row_aad(self.key_id, identity, plaintext_bytes);
        let ciphertext = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| GuardianScrollbackRowError::EncryptionFailed)?;
        Ok(GuardianEncryptedScrollbackRow {
            key_id: self.key_id,
            identity,
            plaintext_bytes,
            nonce,
            ciphertext,
        })
    }

    /// Authenticate and open one exact row at its expected durable location.
    ///
    /// The per-record revision is authenticated from the envelope.  The
    /// caller supplies the durable pane, content generation, stable row and
    /// sequence from the surrounding store so cross-pane, cross-clear and
    /// cross-row transplants fail before plaintext is returned.
    pub fn open_scrollback_row(
        &self,
        record: &GuardianEncryptedScrollbackRow,
        expected_durable_pane_id: [u8; 16],
        expected_content_epoch: [u8; 16],
        expected_stable_row: i64,
        expected_sequence: u64,
        max_plaintext_bytes: u32,
    ) -> Result<Zeroizing<Vec<u8>>, GuardianScrollbackRowError> {
        if record.key_id != self.key_id {
            return Err(GuardianScrollbackRowError::KeyIdentityMismatch);
        }
        let identity = record.identity;
        if identity.durable_pane_id != expected_durable_pane_id
            || identity.content_epoch != expected_content_epoch
            || identity.stable_row != expected_stable_row
            || identity.sequence != expected_sequence
        {
            return Err(GuardianScrollbackRowError::StorageIdentityMismatch);
        }
        let maximum = max_plaintext_bytes.min(SCROLLBACK_ROW_MAX_PLAINTEXT_BYTES);
        if record.plaintext_bytes == 0 || record.plaintext_bytes > maximum {
            return Err(GuardianScrollbackRowError::RecordByteLimit);
        }
        let expected_ciphertext_bytes = usize::try_from(record.plaintext_bytes)
            .map_err(|_| GuardianScrollbackRowError::ArithmeticOverflow)?
            .checked_add(AEAD_TAG_BYTES_USIZE)
            .ok_or(GuardianScrollbackRowError::ArithmeticOverflow)?;
        if record.ciphertext.len() != expected_ciphertext_bytes {
            return Err(GuardianScrollbackRowError::CiphertextLengthMismatch);
        }
        let aad = scrollback_row_aad(record.key_id, identity, record.plaintext_bytes);
        let plaintext = Zeroizing::new(
            self.cipher
                .decrypt(
                    XNonce::from_slice(&record.nonce),
                    Payload {
                        msg: &record.ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| GuardianScrollbackRowError::DecryptionFailed)?,
        );
        if plaintext.len()
            != usize::try_from(record.plaintext_bytes)
                .map_err(|_| GuardianScrollbackRowError::ArithmeticOverflow)?
        {
            return Err(GuardianScrollbackRowError::CiphertextLengthMismatch);
        }
        Ok(plaintext)
    }

    /// Authenticate one canonical v3 scrollback manifest without persisting
    /// either generic metadata ciphertext or raw key material.
    pub fn authenticate_scrollback_manifest(
        &self,
        canonical_manifest: &[u8],
    ) -> Result<GuardianScrollbackManifestAuthentication, GuardianScrollbackManifestError> {
        let canonical_bytes = u32::try_from(canonical_manifest.len())
            .map_err(|_| GuardianScrollbackManifestError::CanonicalByteLimit)?;
        if canonical_bytes == 0 || canonical_bytes > SCROLLBACK_MANIFEST_MAX_CANONICAL_BYTES {
            return Err(GuardianScrollbackManifestError::CanonicalByteLimit);
        }
        let mut nonce = [0; NONCE_BYTES];
        if OsRng.try_fill_bytes(&mut nonce).is_err() {
            nonce.zeroize();
            return Err(GuardianScrollbackManifestError::EntropyUnavailable);
        }
        let aad = scrollback_manifest_aad(self.key_id, canonical_bytes, canonical_manifest);
        let authentication_tag = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &[],
                    aad: &aad,
                },
            )
            .map_err(|_| GuardianScrollbackManifestError::AuthenticationFailed)?;
        let authentication_tag: [u8; AEAD_TAG_BYTES_USIZE] = authentication_tag
            .try_into()
            .map_err(|_| GuardianScrollbackManifestError::AuthenticationFailed)?;
        Ok(GuardianScrollbackManifestAuthentication {
            key_id: self.key_id,
            canonical_bytes,
            nonce,
            authentication_tag,
        })
    }

    /// Verify that a canonical v3 scrollback manifest matches its guardian
    /// authentication seal under this historical key.
    pub fn verify_scrollback_manifest(
        &self,
        authentication: &GuardianScrollbackManifestAuthentication,
        canonical_manifest: &[u8],
    ) -> Result<(), GuardianScrollbackManifestError> {
        if authentication.key_id != self.key_id {
            return Err(GuardianScrollbackManifestError::KeyIdentityMismatch);
        }
        let canonical_bytes = u32::try_from(canonical_manifest.len())
            .map_err(|_| GuardianScrollbackManifestError::CanonicalByteLimit)?;
        if canonical_bytes == 0 || canonical_bytes > SCROLLBACK_MANIFEST_MAX_CANONICAL_BYTES {
            return Err(GuardianScrollbackManifestError::CanonicalByteLimit);
        }
        if canonical_bytes != authentication.canonical_bytes {
            return Err(GuardianScrollbackManifestError::AuthenticationFailed);
        }
        let aad = scrollback_manifest_aad(self.key_id, canonical_bytes, canonical_manifest);
        let plaintext = self
            .cipher
            .decrypt(
                XNonce::from_slice(&authentication.nonce),
                Payload {
                    msg: &authentication.authentication_tag,
                    aad: &aad,
                },
            )
            .map_err(|_| GuardianScrollbackManifestError::AuthenticationFailed)?;
        if !plaintext.is_empty() {
            return Err(GuardianScrollbackManifestError::AuthenticationFailed);
        }
        Ok(())
    }

    /// Authenticate one canonical exact-row append WAL transaction. Only the
    /// bounded metadata is associated data; the separately encrypted row is
    /// bound through its length and digest in that canonical metadata.
    pub fn authenticate_scrollback_append_wal(
        &self,
        canonical_wal: &[u8],
    ) -> Result<GuardianScrollbackAppendWalAuthentication, GuardianScrollbackAppendWalError> {
        let canonical_bytes = u32::try_from(canonical_wal.len())
            .map_err(|_| GuardianScrollbackAppendWalError::CanonicalByteLimit)?;
        if canonical_bytes == 0 || canonical_bytes > SCROLLBACK_APPEND_WAL_MAX_CANONICAL_BYTES {
            return Err(GuardianScrollbackAppendWalError::CanonicalByteLimit);
        }
        let mut nonce = [0; NONCE_BYTES];
        if OsRng.try_fill_bytes(&mut nonce).is_err() {
            nonce.zeroize();
            return Err(GuardianScrollbackAppendWalError::EntropyUnavailable);
        }
        let aad = scrollback_append_wal_aad(self.key_id, canonical_bytes, canonical_wal);
        let authentication_tag = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &[],
                    aad: &aad,
                },
            )
            .map_err(|_| GuardianScrollbackAppendWalError::AuthenticationFailed)?;
        let authentication_tag: [u8; AEAD_TAG_BYTES_USIZE] = authentication_tag
            .try_into()
            .map_err(|_| GuardianScrollbackAppendWalError::AuthenticationFailed)?;
        Ok(GuardianScrollbackAppendWalAuthentication {
            key_id: self.key_id,
            canonical_bytes,
            nonce,
            authentication_tag,
        })
    }

    pub fn verify_scrollback_append_wal(
        &self,
        authentication: &GuardianScrollbackAppendWalAuthentication,
        canonical_wal: &[u8],
    ) -> Result<(), GuardianScrollbackAppendWalError> {
        if authentication.key_id != self.key_id {
            return Err(GuardianScrollbackAppendWalError::KeyIdentityMismatch);
        }
        let canonical_bytes = u32::try_from(canonical_wal.len())
            .map_err(|_| GuardianScrollbackAppendWalError::CanonicalByteLimit)?;
        if canonical_bytes == 0 || canonical_bytes > SCROLLBACK_APPEND_WAL_MAX_CANONICAL_BYTES {
            return Err(GuardianScrollbackAppendWalError::CanonicalByteLimit);
        }
        if canonical_bytes != authentication.canonical_bytes {
            return Err(GuardianScrollbackAppendWalError::AuthenticationFailed);
        }
        let aad = scrollback_append_wal_aad(self.key_id, canonical_bytes, canonical_wal);
        let plaintext = self
            .cipher
            .decrypt(
                XNonce::from_slice(&authentication.nonce),
                Payload {
                    msg: &authentication.authentication_tag,
                    aad: &aad,
                },
            )
            .map_err(|_| GuardianScrollbackAppendWalError::AuthenticationFailed)?;
        if !plaintext.is_empty() {
            return Err(GuardianScrollbackAppendWalError::AuthenticationFailed);
        }
        Ok(())
    }

    /// Seal guardian-owned journal metadata under a caller-supplied,
    /// domain-separated associated-data envelope.
    ///
    /// This crate-private surface lets the input-effect journal reuse the
    /// provisioned guardian key without exposing key bytes or reusing the raw
    /// output record identity. Callers must include a unique format domain and
    /// the complete cleartext record header in `aad`.
    pub(crate) fn seal_guardian_metadata(
        &self,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<([u8; NONCE_BYTES], Vec<u8>), GuardianOutputJournalError> {
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        if OsRng.try_fill_bytes(&mut nonce_bytes).is_err() {
            nonce_bytes.zeroize();
            return Err(GuardianOutputJournalError::EntropyUnavailable);
        }
        let ciphertext = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| GuardianOutputJournalError::EncryptionFailed)?;
        Ok((nonce_bytes, ciphertext))
    }

    /// Seal the one replay-stable catalog-adoption evidence envelope.
    ///
    /// Unlike generic guardian metadata, an adoption candidate must reproduce
    /// byte-identical ciphertext after a crash so a durable `.staging` prefix
    /// can be authenticated and completed. The nonce is therefore the first
    /// 192 bits of a frozen SHA-256 transcript over this method's domain and
    /// derivation version, the key ID, the complete length-prefixed AAD, and
    /// the length plus SHA-256 digest of the exact inner plaintext passed to
    /// XChaCha20-Poly1305. The checkpoint AAD itself carries the checkpoint
    /// record format/version and the complete canonical adoption context.
    ///
    /// For one key, a different AAD/plaintext pair can reuse a nonce only by
    /// colliding in that 192-bit SHA-256 prefix (about 2^96 generic birthday
    /// work, far beyond this bounded catalog) or by first colliding the
    /// plaintext SHA-256 digest. Repeating the exact pair intentionally repeats
    /// the complete envelope. This narrow crate-private primitive must not be
    /// generalized to caller-selected records; ordinary metadata and output
    /// records continue to use fresh random nonces.
    pub(crate) fn seal_replay_stable_catalog_adoption_metadata(
        &self,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<([u8; NONCE_BYTES], Vec<u8>), GuardianOutputJournalError> {
        let aad_bytes =
            u64::try_from(aad.len()).map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
        let plaintext_bytes = u64::try_from(plaintext.len())
            .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
        let plaintext_digest: [u8; 32] = Sha256::digest(plaintext).into();
        let mut nonce_hasher = Sha256::new();
        nonce_hasher.update(REPLAY_STABLE_CATALOG_ADOPTION_NONCE_DOMAIN);
        nonce_hasher.update(REPLAY_STABLE_CATALOG_ADOPTION_NONCE_DERIVATION_VERSION.to_le_bytes());
        nonce_hasher.update(self.key_id);
        nonce_hasher.update(aad_bytes.to_le_bytes());
        nonce_hasher.update(aad);
        nonce_hasher.update(plaintext_bytes.to_le_bytes());
        nonce_hasher.update(plaintext_digest);
        let nonce_digest: [u8; 32] = nonce_hasher.finalize().into();
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        nonce_bytes.copy_from_slice(&nonce_digest[..NONCE_BYTES]);
        let ciphertext = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| GuardianOutputJournalError::EncryptionFailed)?;
        Ok((nonce_bytes, ciphertext))
    }

    /// Authenticate and open guardian-owned journal metadata produced by
    /// [`Self::seal_guardian_metadata`].
    pub(crate) fn open_guardian_metadata(
        &self,
        nonce_bytes: &[u8; NONCE_BYTES],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, GuardianOutputJournalError> {
        self.cipher
            .decrypt(
                XNonce::from_slice(nonce_bytes),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| GuardianOutputJournalError::DecryptionFailed)
    }

    fn seal(
        &self,
        identity: GuardianOutputSegmentIdentity,
        sequence: u64,
        plaintext_bytes: u32,
        plaintext: &[u8],
    ) -> Result<([u8; NONCE_BYTES], Vec<u8>), GuardianOutputJournalError> {
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        if OsRng.try_fill_bytes(&mut nonce_bytes).is_err() {
            nonce_bytes.zeroize();
            return Err(GuardianOutputJournalError::EntropyUnavailable);
        }
        let nonce = XNonce::from_slice(&nonce_bytes);
        let aad = record_aad(identity, sequence, plaintext_bytes);
        let ciphertext = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| GuardianOutputJournalError::EncryptionFailed)?;
        Ok((nonce_bytes, ciphertext))
    }

    fn open(
        &self,
        identity: GuardianOutputSegmentIdentity,
        sequence: u64,
        plaintext_bytes: u32,
        nonce_bytes: &[u8; NONCE_BYTES],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, GuardianOutputJournalError> {
        let nonce = XNonce::from_slice(nonce_bytes);
        let aad = record_aad(identity, sequence, plaintext_bytes);
        self.cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| GuardianOutputJournalError::DecryptionFailed)
    }
}

fn scrollback_row_aad(
    key_id: [u8; KEY_ID_BYTES],
    identity: GuardianScrollbackRowIdentity,
    plaintext_bytes: u32,
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(SCROLLBACK_ROW_AEAD_DOMAIN.len() + 72);
    aad.extend_from_slice(SCROLLBACK_ROW_AEAD_DOMAIN);
    aad.extend_from_slice(&SCROLLBACK_ROW_FORMAT_VERSION.to_le_bytes());
    aad.extend_from_slice(&key_id);
    aad.extend_from_slice(&identity.durable_pane_id);
    aad.extend_from_slice(&identity.content_epoch);
    aad.extend_from_slice(&identity.revision.to_le_bytes());
    aad.extend_from_slice(&identity.stable_row.to_le_bytes());
    aad.extend_from_slice(&identity.sequence.to_le_bytes());
    aad.extend_from_slice(&plaintext_bytes.to_le_bytes());
    aad
}

fn scrollback_manifest_aad(
    key_id: [u8; KEY_ID_BYTES],
    canonical_bytes: u32,
    canonical_manifest: &[u8],
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(
        SCROLLBACK_MANIFEST_AEAD_DOMAIN.len() + 4 + KEY_ID_BYTES + 4 + canonical_manifest.len(),
    );
    aad.extend_from_slice(SCROLLBACK_MANIFEST_AEAD_DOMAIN);
    aad.extend_from_slice(&SCROLLBACK_MANIFEST_AUTH_VERSION.to_le_bytes());
    aad.extend_from_slice(&key_id);
    aad.extend_from_slice(&canonical_bytes.to_le_bytes());
    aad.extend_from_slice(canonical_manifest);
    aad
}

fn scrollback_append_wal_aad(
    key_id: [u8; KEY_ID_BYTES],
    canonical_bytes: u32,
    canonical_wal: &[u8],
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(
        SCROLLBACK_APPEND_WAL_AEAD_DOMAIN.len() + 4 + KEY_ID_BYTES + 4 + canonical_wal.len(),
    );
    aad.extend_from_slice(SCROLLBACK_APPEND_WAL_AEAD_DOMAIN);
    aad.extend_from_slice(&SCROLLBACK_APPEND_WAL_AUTH_VERSION.to_le_bytes());
    aad.extend_from_slice(&key_id);
    aad.extend_from_slice(&canonical_bytes.to_le_bytes());
    aad.extend_from_slice(canonical_wal);
    aad
}

impl std::fmt::Debug for GuardianOutputCipher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputCipher")
            .field("key", &"[REDACTED]")
            .field("key_id", &"[REDACTED]")
            .finish()
    }
}

/// Integrity chain from a new segment to the last committed predecessor record.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianOutputPredecessor {
    segment_id: Uuid,
    last_sequence: u64,
    terminal_record_digest: [u8; 32],
    cumulative_plaintext_bytes: u64,
    committed_log_bytes: u64,
}

impl GuardianOutputPredecessor {
    pub fn new(
        segment_id: Uuid,
        last_sequence: u64,
        terminal_record_digest: [u8; 32],
        cumulative_plaintext_bytes: u64,
        committed_log_bytes: u64,
    ) -> Result<Self, GuardianOutputJournalError> {
        if segment_id.is_nil() {
            return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                "predecessor segment UUID must be nonnil",
            ));
        }
        if last_sequence == 0 {
            return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                "predecessor sequence must be nonzero",
            ));
        }
        if cumulative_plaintext_bytes == 0 {
            return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                "predecessor cumulative plaintext endpoint must be nonzero",
            ));
        }
        let minimum_committed_log_bytes = FILE_HEADER_BYTES_U64
            .checked_add(RECORD_HEADER_BYTES_U64)
            .and_then(|bytes| bytes.checked_add(u64::from(AEAD_TAG_BYTES) + 1))
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if committed_log_bytes < minimum_committed_log_bytes {
            return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                "predecessor committed log endpoint cannot contain a record",
            ));
        }
        Ok(Self {
            segment_id,
            last_sequence,
            terminal_record_digest,
            cumulative_plaintext_bytes,
            committed_log_bytes,
        })
    }

    #[must_use]
    pub const fn segment_id(self) -> Uuid {
        self.segment_id
    }

    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    #[must_use]
    pub const fn terminal_record_digest(self) -> [u8; 32] {
        self.terminal_record_digest
    }

    #[must_use]
    pub const fn cumulative_plaintext_bytes(self) -> u64 {
        self.cumulative_plaintext_bytes
    }

    #[must_use]
    pub const fn committed_log_bytes(self) -> u64 {
        self.committed_log_bytes
    }
}

impl std::fmt::Debug for GuardianOutputPredecessor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputPredecessor")
            .field("segment_id", &self.segment_id)
            .field("last_sequence", &self.last_sequence)
            .field("terminal_record_digest", &"[REDACTED]")
            .field(
                "cumulative_plaintext_bytes",
                &self.cumulative_plaintext_bytes,
            )
            .field("committed_log_bytes", &self.committed_log_bytes)
            .finish()
    }
}

/// Exact identity and predecessor fence for one immutable segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianOutputSegmentIdentity {
    durable_pane_id: Uuid,
    segment_id: Uuid,
    first_sequence: u64,
    predecessor: Option<GuardianOutputPredecessor>,
}

impl GuardianOutputSegmentIdentity {
    pub fn new(
        durable_pane_id: Uuid,
        segment_id: Uuid,
        first_sequence: u64,
        predecessor: Option<GuardianOutputPredecessor>,
    ) -> Result<Self, GuardianOutputJournalError> {
        if durable_pane_id.is_nil() {
            return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                "durable pane UUID must be nonnil",
            ));
        }
        if segment_id.is_nil() {
            return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                "segment UUID must be nonnil",
            ));
        }
        if first_sequence == 0 {
            return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                "first output sequence must be nonzero",
            ));
        }
        match predecessor {
            None if first_sequence != 1 => {
                return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                    "the initial segment must begin at output sequence one",
                ));
            }
            Some(previous) => {
                if previous.segment_id == segment_id {
                    return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                        "a segment cannot name itself as its predecessor",
                    ));
                }
                let required_first = previous.last_sequence.checked_add(1).ok_or(
                    GuardianOutputJournalError::InvalidSegmentIdentity(
                        "an exhausted predecessor cannot have a successor",
                    ),
                )?;
                if first_sequence != required_first {
                    return Err(GuardianOutputJournalError::InvalidSegmentIdentity(
                        "successor output sequence is not contiguous",
                    ));
                }
            }
            None => {}
        }
        Ok(Self {
            durable_pane_id,
            segment_id,
            first_sequence,
            predecessor,
        })
    }

    #[must_use]
    pub const fn durable_pane_id(self) -> Uuid {
        self.durable_pane_id
    }

    #[must_use]
    pub const fn segment_id(self) -> Uuid {
        self.segment_id
    }

    #[must_use]
    pub const fn first_sequence(self) -> u64 {
        self.first_sequence
    }

    #[must_use]
    pub const fn predecessor(self) -> Option<GuardianOutputPredecessor> {
        self.predecessor
    }
}

/// Structurally parsed, deliberately unauthenticated key-selection hint.
///
/// This type carries no pane, segment, predecessor, sequence, or delivery
/// authority.  Its sole purpose is to let a caller choose a candidate
/// historical key from a bounded keyring before calling
/// an existing-file journal constructor, which independently authenticates
/// the complete header and record chain.  A successfully parsed hint must never be
/// treated as proof that the file belongs to that key.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianOutputUntrustedHeaderHint {
    key_id: [u8; KEY_ID_BYTES],
}

impl GuardianOutputUntrustedHeaderHint {
    /// Read only the fixed-size current-format header from an already opened
    /// regular-file descriptor.  Magic/version/length checks are structural;
    /// no cryptographic authority is established here.
    pub fn read_from(file: &File) -> Result<Self, GuardianOutputJournalError> {
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(GuardianOutputJournalError::NotRegularFile);
        }
        if metadata.len() < FILE_HEADER_BYTES_U64 {
            return Err(GuardianOutputJournalError::TornFileHeader {
                expected: FILE_HEADER_BYTES,
                actual: metadata.len(),
            });
        }
        let mut header = [0_u8; FILE_HEADER_BYTES];
        read_exact_file_at(file, &mut header, 0)?;
        if header[0..8] != FILE_MAGIC {
            return Err(GuardianOutputJournalError::InvalidFileMagic);
        }
        let version = read_u32(&header[8..12]);
        if version != FORMAT_VERSION {
            return Err(GuardianOutputJournalError::UnsupportedVersion { observed: version });
        }
        let header_bytes = read_u32(&header[12..16]);
        if header_bytes != FILE_HEADER_BYTES_U32 {
            return Err(GuardianOutputJournalError::InvalidFileHeaderLength {
                observed: header_bytes,
            });
        }
        let mut key_id = [0_u8; KEY_ID_BYTES];
        key_id.copy_from_slice(&header[112..120]);
        Ok(Self { key_id })
    }

    /// Unauthenticated key fingerprint used only to select a candidate key.
    #[must_use]
    pub const fn untrusted_key_id(self) -> [u8; KEY_ID_BYTES] {
        self.key_id
    }
}

impl std::fmt::Debug for GuardianOutputUntrustedHeaderHint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputUntrustedHeaderHint")
            .field("key_id", &"[UNTRUSTED REDACTED]")
            .finish()
    }
}

/// Recovery status for bytes after the last verified record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuardianOutputJournalTail {
    Clean,
    Incomplete {
        committed_bytes: u64,
        trailing_bytes: u64,
    },
}

/// Receipt that may be forwarded to a mux only after `sync_all` succeeds.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct GuardianOutputAppendReceipt {
    segment_id: Uuid,
    sequence: u64,
    payload_bytes: u32,
    cumulative_plaintext_bytes: u64,
    committed_log_bytes: u64,
    record_digest: [u8; 32],
    plaintext_delivery_digest: [u8; 32],
}

impl GuardianOutputAppendReceipt {
    /// Reconstitute the delivery-only portion of a receipt from one replay
    /// record whose metadata and plaintext were already authenticated by the
    /// guardian replay protocol.
    ///
    /// This is crate-private so an ordinary mux caller cannot manufacture
    /// durable append authority from public fields. The replay decoder is the
    /// only non-journal producer, and it consumes its nonduplicable plaintext
    /// capability while deriving the private delivery digest here.
    pub(crate) fn from_authenticated_replay(
        segment_id: Uuid,
        sequence: u64,
        cumulative_plaintext_bytes: u64,
        committed_log_bytes: u64,
        record_digest: [u8; 32],
        payload: &[u8],
    ) -> Result<Self, GuardianOutputJournalError> {
        let payload_bytes = u32::try_from(payload.len())
            .map_err(|_| GuardianOutputJournalError::RecoveryPayloadBindingMismatch)?;
        let minimum_committed_log_bytes = FILE_HEADER_BYTES_U64
            .checked_add(RECORD_HEADER_BYTES_U64)
            .and_then(|bytes| bytes.checked_add(u64::from(AEAD_TAG_BYTES)))
            .and_then(|bytes| bytes.checked_add(u64::from(payload_bytes)))
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if segment_id.is_nil()
            || sequence == 0
            || payload_bytes == 0
            || cumulative_plaintext_bytes < u64::from(payload_bytes)
            || committed_log_bytes < minimum_committed_log_bytes
            || record_digest == [0; 32]
        {
            return Err(GuardianOutputJournalError::RecoveryPayloadBindingMismatch);
        }
        Ok(Self {
            segment_id,
            sequence,
            payload_bytes,
            cumulative_plaintext_bytes,
            committed_log_bytes,
            record_digest,
            plaintext_delivery_digest: plaintext_delivery_digest(segment_id, sequence, payload),
        })
    }

    #[must_use]
    pub const fn segment_id(self) -> Uuid {
        self.segment_id
    }

    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn payload_bytes(self) -> u32 {
        self.payload_bytes
    }

    /// Exact pane-lifetime cumulative plaintext stream endpoint through this
    /// authenticated, synchronized record. The endpoint never resets at a
    /// segment rollover.
    #[must_use]
    pub const fn cumulative_plaintext_bytes(self) -> u64 {
        self.cumulative_plaintext_bytes
    }

    #[must_use]
    pub const fn committed_log_bytes(self) -> u64 {
        self.committed_log_bytes
    }

    #[must_use]
    pub const fn record_digest(self) -> [u8; 32] {
        self.record_digest
    }

    /// Prove that `payload` is exactly the plaintext whose synchronized
    /// journal append produced this receipt. The content-derived digest is
    /// private and compared without content-dependent early exit; callers
    /// never receive a reusable digest oracle.
    #[must_use]
    pub(crate) fn matches_payload(&self, payload: &[u8]) -> bool {
        if usize::try_from(self.payload_bytes).ok() != Some(payload.len()) {
            return false;
        }
        let candidate = plaintext_delivery_digest(self.segment_id, self.sequence, payload);
        candidate
            .iter()
            .zip(self.plaintext_delivery_digest.iter())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }

    #[must_use]
    pub const fn into_predecessor(self) -> GuardianOutputPredecessor {
        GuardianOutputPredecessor {
            segment_id: self.segment_id,
            last_sequence: self.sequence,
            terminal_record_digest: self.record_digest,
            cumulative_plaintext_bytes: self.cumulative_plaintext_bytes,
            committed_log_bytes: self.committed_log_bytes,
        }
    }
}

impl std::fmt::Debug for GuardianOutputAppendReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputAppendReceipt")
            .field("segment_id", &self.segment_id)
            .field("sequence", &self.sequence)
            .field("payload_bytes", &self.payload_bytes)
            .field(
                "cumulative_plaintext_bytes",
                &self.cumulative_plaintext_bytes,
            )
            .field("committed_log_bytes", &self.committed_log_bytes)
            .field("record_digest", &"[REDACTED]")
            .field("plaintext_delivery_digest", &"[REDACTED]")
            .finish()
    }
}

/// Hard admission bounds for one authenticated suffix recovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuardianOutputRecoveryLimits {
    pub max_records: u64,
    pub max_plaintext_bytes: u64,
}

impl GuardianOutputRecoveryLimits {
    pub const HARD_MAX_RECORDS: u64 = RECOVERY_MAX_RECORDS;
    pub const HARD_MAX_PLAINTEXT_BYTES: u64 = RECOVERY_MAX_PLAINTEXT_BYTES;

    pub fn new(
        max_records: u64,
        max_plaintext_bytes: u64,
    ) -> Result<Self, GuardianOutputJournalError> {
        Self {
            max_records,
            max_plaintext_bytes,
        }
        .validate()
    }

    fn validate(self) -> Result<Self, GuardianOutputJournalError> {
        if self.max_records == 0 {
            return Err(GuardianOutputJournalError::InvalidRecoveryLimits(
                "max_records must be nonzero",
            ));
        }
        if self.max_records > RECOVERY_MAX_RECORDS {
            return Err(GuardianOutputJournalError::InvalidRecoveryLimits(
                "max_records exceeds the hard recovery cap",
            ));
        }
        if self.max_plaintext_bytes == 0 {
            return Err(GuardianOutputJournalError::InvalidRecoveryLimits(
                "max_plaintext_bytes must be nonzero",
            ));
        }
        if self.max_plaintext_bytes > RECOVERY_MAX_PLAINTEXT_BYTES {
            return Err(GuardianOutputJournalError::InvalidRecoveryLimits(
                "max_plaintext_bytes exceeds the hard recovery cap",
            ));
        }
        Ok(self)
    }
}

/// One exact plaintext record recovered only after digest and AEAD validation.
pub struct GuardianRecoveredOutputRecord {
    receipt: GuardianOutputAppendReceipt,
    plaintext: Zeroizing<Vec<u8>>,
}

impl GuardianRecoveredOutputRecord {
    #[must_use]
    pub const fn receipt(&self) -> GuardianOutputAppendReceipt {
        self.receipt
    }

    /// Content-free confirmation that the opaque recovered plaintext still
    /// matches the constructor-only delivery digest in its authenticated
    /// receipt. This never returns plaintext or accepts caller-chosen bytes,
    /// so it is not a reusable digest oracle.
    #[must_use]
    pub fn authenticated_payload_is_receipt_bound(&self) -> bool {
        self.receipt.matches_payload(self.plaintext.as_slice())
    }

    /// Consume this recovered record and promote it to the only public
    /// plaintext-delivery capability.
    ///
    /// Promotion rechecks the constructor-private receipt binding before any
    /// writer can observe a byte.  The resulting capability is non-cloneable,
    /// writes at most once, and continues to own the plaintext in a zeroizing
    /// allocation until that write completes or unwinds.
    pub fn into_authenticated_delivery(
        self,
    ) -> Result<GuardianAuthenticatedOutputDelivery, GuardianOutputJournalError> {
        if !self.receipt.matches_payload(self.plaintext.as_slice()) {
            return Err(GuardianOutputJournalError::RecoveryPayloadBindingMismatch);
        }
        Ok(GuardianAuthenticatedOutputDelivery {
            receipt: self.receipt,
            plaintext: self.plaintext,
        })
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn plaintext(&self) -> &[u8] {
        self.plaintext.as_slice()
    }
}

impl std::fmt::Debug for GuardianRecoveredOutputRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianRecoveredOutputRecord")
            .field("receipt", &self.receipt)
            .field("plaintext", &"[REDACTED]")
            .finish()
    }
}

/// Single-use authenticated plaintext-delivery capability.
///
/// There is intentionally no payload getter, clone implementation, or public
/// constructor.  Callers can only consume a recovered record into this type
/// and then write its exact receipt-bound payload through
/// [`Self::write_all_bounded`].
pub struct GuardianAuthenticatedOutputDelivery {
    receipt: GuardianOutputAppendReceipt,
    plaintext: Zeroizing<Vec<u8>>,
}

impl GuardianAuthenticatedOutputDelivery {
    #[must_use]
    pub const fn receipt(&self) -> GuardianOutputAppendReceipt {
        self.receipt
    }

    /// Write the complete authenticated payload exactly once.
    ///
    /// The caller's bound is checked before the writer observes any byte.  A
    /// returned receipt therefore means `write_all` accepted the complete
    /// payload; an I/O error is an ambiguous downstream-delivery disposition
    /// and does not return a replayable plaintext capability.
    pub fn write_all_bounded<W: Write>(
        self,
        writer: &mut W,
        max_payload_bytes: u32,
    ) -> Result<GuardianOutputAppendReceipt, GuardianOutputJournalError> {
        let observed = u64::try_from(self.plaintext.len())
            .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
        if max_payload_bytes == 0 || observed > u64::from(max_payload_bytes) {
            return Err(GuardianOutputJournalError::DeliveryPayloadByteLimit {
                observed,
                maximum: max_payload_bytes,
            });
        }
        if !self.receipt.matches_payload(self.plaintext.as_slice()) {
            return Err(GuardianOutputJournalError::RecoveryPayloadBindingMismatch);
        }
        writer.write_all(self.plaintext.as_slice())?;
        Ok(self.receipt)
    }
}

impl std::fmt::Debug for GuardianAuthenticatedOutputDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianAuthenticatedOutputDelivery")
            .field("receipt", &self.receipt)
            .field("plaintext", &"[REDACTED]")
            .finish()
    }
}

/// Exact bounded suffix plus authenticated terminal segment authority.
pub struct GuardianOutputRecoveryBatch {
    segment_identity: GuardianOutputSegmentIdentity,
    requested_first_sequence: u64,
    records: Vec<GuardianRecoveredOutputRecord>,
    next_recovery_sequence: Option<u64>,
    committed_next_sequence: Option<u64>,
    committed_log_bytes: u64,
    cumulative_plaintext_bytes: u64,
    terminal_receipt: Option<GuardianOutputAppendReceipt>,
    tail: GuardianOutputJournalTail,
}

impl GuardianOutputRecoveryBatch {
    #[must_use]
    pub const fn segment_identity(&self) -> GuardianOutputSegmentIdentity {
        self.segment_identity
    }

    #[must_use]
    pub const fn requested_first_sequence(&self) -> u64 {
        self.requested_first_sequence
    }

    #[must_use]
    pub fn records(&self) -> &[GuardianRecoveredOutputRecord] {
        &self.records
    }

    #[must_use]
    /// Sequence at which the next recovery page must begin when this page was
    /// saturated. `None` means that this page reached the authenticated tail;
    /// for a segment ending at `u64::MAX`, it is also the deliberate terminal
    /// cursor because no exclusive successor sequence is representable.
    pub const fn next_recovery_sequence(&self) -> Option<u64> {
        self.next_recovery_sequence
    }

    #[must_use]
    /// Exclusive authenticated record endpoint when representable. `None`
    /// means that a committed record at `u64::MAX` exhausted sequence space;
    /// it never authorizes wrapping or restarting the cursor at zero.
    pub const fn committed_next_sequence(&self) -> Option<u64> {
        self.committed_next_sequence
    }

    #[must_use]
    pub const fn committed_log_bytes(&self) -> u64 {
        self.committed_log_bytes
    }

    #[must_use]
    pub const fn cumulative_plaintext_bytes(&self) -> u64 {
        self.cumulative_plaintext_bytes
    }

    #[must_use]
    pub const fn terminal_receipt(&self) -> Option<GuardianOutputAppendReceipt> {
        self.terminal_receipt
    }

    #[must_use]
    pub fn terminal_predecessor(&self) -> Option<GuardianOutputPredecessor> {
        self.terminal_receipt
            .map(GuardianOutputAppendReceipt::into_predecessor)
            .or(self.segment_identity.predecessor)
    }

    #[must_use]
    pub const fn tail(&self) -> GuardianOutputJournalTail {
        self.tail
    }
}

impl std::fmt::Debug for GuardianOutputRecoveryBatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputRecoveryBatch")
            .field("segment_identity", &self.segment_identity)
            .field("requested_first_sequence", &self.requested_first_sequence)
            .field("record_count", &self.records.len())
            .field("next_recovery_sequence", &self.next_recovery_sequence)
            .field("committed_next_sequence", &self.committed_next_sequence)
            .field("committed_log_bytes", &self.committed_log_bytes)
            .field(
                "cumulative_plaintext_bytes",
                &self.cumulative_plaintext_bytes,
            )
            .field("terminal_receipt", &self.terminal_receipt)
            .field("tail", &self.tail)
            .finish()
    }
}

/// Stateful, bounded, single-pass authenticated recovery cursor.
///
/// The cursor owns a cloned descriptor and a frozen copy of the exact segment
/// authority established by an authenticated journal constructor.  It uses
/// positional reads, so advancing it cannot disturb the append descriptor's
/// shared file offset.  Each physical record is parsed, digest-checked, and
/// AEAD-opened at most once by this cursor.  One bounded zeroizing plaintext
/// allocation is produced per advance, and the cursor retains no emitted
/// plaintext.
pub struct GuardianOutputRecoveryCursor {
    file: File,
    identity: GuardianOutputSegmentIdentity,
    cipher: GuardianOutputCipher,
    journal_limits: GuardianOutputJournalLimits,
    requested_first_sequence: u64,
    max_record_plaintext_bytes: u32,
    committed_bytes: u64,
    expected_record_count: u64,
    expected_cumulative_plaintext_bytes: u64,
    expected_next_sequence: Option<u64>,
    expected_terminal_receipt: Option<GuardianOutputAppendReceipt>,
    expected_authenticated_prefix_digest: [u8; 32],
    tail: GuardianOutputJournalTail,
    offset: u64,
    record_count: u64,
    cumulative_plaintext_bytes: u64,
    next_sequence: Option<u64>,
    terminal_receipt: Option<GuardianOutputAppendReceipt>,
    authenticated_prefix_digest: [u8; 32],
    exhausted: bool,
    failed: bool,
    #[cfg(test)]
    verified_record_count: u64,
}

impl GuardianOutputRecoveryCursor {
    #[must_use]
    pub const fn segment_identity(&self) -> GuardianOutputSegmentIdentity {
        self.identity
    }

    #[must_use]
    pub const fn requested_first_sequence(&self) -> u64 {
        self.requested_first_sequence
    }

    #[must_use]
    pub const fn max_record_plaintext_bytes(&self) -> u32 {
        self.max_record_plaintext_bytes
    }

    #[must_use]
    pub const fn committed_log_bytes(&self) -> u64 {
        self.committed_bytes
    }

    #[must_use]
    pub const fn tail(&self) -> GuardianOutputJournalTail {
        self.tail
    }

    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Authenticate and return the next complete record at or after the
    /// requested first sequence.
    ///
    /// Records before the requested sequence are still authenticated in order
    /// because their chain state is the authority for every later receipt.
    /// The fixed per-record bound is checked from the canonical header before
    /// ciphertext allocation or AEAD opening.  Once `None` is returned, later
    /// calls deterministically return `None` without rereading the file.
    pub fn next_record(
        &mut self,
    ) -> Result<Option<GuardianRecoveredOutputRecord>, GuardianOutputJournalError> {
        if self.failed {
            return Err(GuardianOutputJournalError::RecoveryCursorFailed);
        }
        let result = self.next_record_inner();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn next_record_inner(
        &mut self,
    ) -> Result<Option<GuardianRecoveredOutputRecord>, GuardianOutputJournalError> {
        if self.exhausted {
            return Ok(None);
        }
        let physical_bytes = self.file.metadata()?.len();
        if physical_bytes < self.committed_bytes {
            return Err(GuardianOutputJournalError::ExternalLengthChange {
                expected: self.committed_bytes,
                observed: physical_bytes,
            });
        }

        loop {
            if self.offset == self.committed_bytes {
                if self.record_count != self.expected_record_count
                    || self.cumulative_plaintext_bytes != self.expected_cumulative_plaintext_bytes
                    || self.next_sequence != self.expected_next_sequence
                    || self.terminal_receipt != self.expected_terminal_receipt
                    || self.authenticated_prefix_digest != self.expected_authenticated_prefix_digest
                {
                    return Err(GuardianOutputJournalError::RecoveryAuthorityMismatch);
                }
                self.exhausted = true;
                return Ok(None);
            }
            if self.offset > self.committed_bytes {
                return Err(GuardianOutputJournalError::RecoveryAuthorityMismatch);
            }
            if self.record_count >= self.journal_limits.max_records {
                return Err(GuardianOutputJournalError::RecordLimit {
                    maximum: self.journal_limits.max_records,
                });
            }
            let remaining = self
                .committed_bytes
                .checked_sub(self.offset)
                .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
            if remaining < RECORD_HEADER_BYTES_U64 {
                return Err(GuardianOutputJournalError::RecoveryAuthorityMismatch);
            }

            let record_offset = self.offset;
            let mut record_header = [0_u8; RECORD_HEADER_BYTES];
            read_exact_file_at(&self.file, &mut record_header, record_offset)?;
            if record_header[0..8] != RECORD_MAGIC {
                return Err(GuardianOutputJournalError::InvalidRecordMagic {
                    offset: record_offset,
                });
            }
            let sequence = read_u64(&record_header[8..16]);
            let expected_sequence = self
                .next_sequence
                .ok_or(GuardianOutputJournalError::SequenceExhausted)?;
            if sequence != expected_sequence {
                return Err(GuardianOutputJournalError::SequenceMismatch {
                    offset: record_offset,
                    expected: expected_sequence,
                    observed: sequence,
                });
            }
            let plaintext_bytes = read_u32(&record_header[16..20]);
            let ciphertext_bytes = read_u32(&record_header[20..24]);
            let record_header_bytes = read_u32(&record_header[24..28]);
            if record_header_bytes != RECORD_HEADER_BYTES_U32 {
                return Err(GuardianOutputJournalError::InvalidRecordHeaderLength {
                    offset: record_offset,
                    observed: record_header_bytes,
                });
            }
            if record_header[28..32] != [0_u8; 4] || record_header[88..96] != [0_u8; 8] {
                return Err(GuardianOutputJournalError::NonCanonicalRecordHeader {
                    offset: record_offset,
                });
            }
            if plaintext_bytes == 0 {
                return Err(GuardianOutputJournalError::EmptyRecord {
                    offset: record_offset,
                });
            }
            if plaintext_bytes > self.journal_limits.max_record_bytes {
                return Err(GuardianOutputJournalError::RecordByteLimit {
                    observed: u64::from(plaintext_bytes),
                    maximum: self.journal_limits.max_record_bytes,
                });
            }
            if plaintext_bytes > self.max_record_plaintext_bytes {
                return Err(GuardianOutputJournalError::RecoveryPlaintextByteLimit {
                    observed: u64::from(plaintext_bytes),
                    maximum: u64::from(self.max_record_plaintext_bytes),
                });
            }
            let expected_ciphertext_bytes = plaintext_bytes
                .checked_add(AEAD_TAG_BYTES)
                .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
            if ciphertext_bytes != expected_ciphertext_bytes {
                return Err(GuardianOutputJournalError::CiphertextLengthMismatch {
                    expected: expected_ciphertext_bytes,
                    observed: ciphertext_bytes,
                });
            }
            let frame_bytes = RECORD_HEADER_BYTES_U64
                .checked_add(u64::from(ciphertext_bytes))
                .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
            if remaining < frame_bytes {
                return Err(GuardianOutputJournalError::RecoveryAuthorityMismatch);
            }
            let ciphertext_capacity = usize::try_from(ciphertext_bytes)
                .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
            let mut ciphertext = Vec::new();
            ciphertext
                .try_reserve_exact(ciphertext_capacity)
                .map_err(|_| GuardianOutputJournalError::RecoveryAllocationFailed)?;
            ciphertext.resize(ciphertext_capacity, 0);
            let ciphertext_offset = record_offset
                .checked_add(RECORD_HEADER_BYTES_U64)
                .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
            read_exact_file_at(&self.file, &mut ciphertext, ciphertext_offset)?;
            let mut nonce = [0_u8; NONCE_BYTES];
            nonce.copy_from_slice(&record_header[32..56]);
            let expected_digest = record_digest(
                self.identity,
                sequence,
                plaintext_bytes,
                ciphertext_bytes,
                &nonce,
                &ciphertext,
            );
            if record_header[56..88] != expected_digest {
                return Err(GuardianOutputJournalError::RecordDigestMismatch { sequence });
            }
            let plaintext = Zeroizing::new(self.cipher.open(
                self.identity,
                sequence,
                plaintext_bytes,
                &nonce,
                &ciphertext,
            )?);
            if plaintext.len()
                != usize::try_from(plaintext_bytes)
                    .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?
            {
                return Err(GuardianOutputJournalError::PlaintextLengthMismatch {
                    expected: plaintext_bytes,
                    observed: u32::try_from(plaintext.len())
                        .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?,
                });
            }

            let committed_log_bytes = record_offset
                .checked_add(frame_bytes)
                .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
            let record_count = self
                .record_count
                .checked_add(1)
                .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
            let cumulative_plaintext_bytes = self
                .cumulative_plaintext_bytes
                .checked_add(u64::from(plaintext_bytes))
                .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
            let receipt = GuardianOutputAppendReceipt {
                segment_id: self.identity.segment_id,
                sequence,
                payload_bytes: plaintext_bytes,
                cumulative_plaintext_bytes,
                committed_log_bytes,
                record_digest: expected_digest,
                plaintext_delivery_digest: plaintext_delivery_digest(
                    self.identity.segment_id,
                    sequence,
                    plaintext.as_slice(),
                ),
            };

            self.offset = committed_log_bytes;
            self.record_count = record_count;
            self.cumulative_plaintext_bytes = cumulative_plaintext_bytes;
            self.next_sequence = sequence.checked_add(1);
            self.terminal_receipt = Some(receipt);
            self.authenticated_prefix_digest = extend_authenticated_prefix_digest(
                self.authenticated_prefix_digest,
                &record_header,
                &ciphertext,
            );
            #[cfg(test)]
            {
                self.verified_record_count = self
                    .verified_record_count
                    .checked_add(1)
                    .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
            }

            if sequence >= self.requested_first_sequence {
                return Ok(Some(GuardianRecoveredOutputRecord { receipt, plaintext }));
            }
        }
    }

    #[cfg(test)]
    #[must_use]
    fn verified_record_count(&self) -> u64 {
        self.verified_record_count
    }
}

impl std::fmt::Debug for GuardianOutputRecoveryCursor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputRecoveryCursor")
            .field("identity", &self.identity)
            .field("requested_first_sequence", &self.requested_first_sequence)
            .field(
                "max_record_plaintext_bytes",
                &self.max_record_plaintext_bytes,
            )
            .field("committed_bytes", &self.committed_bytes)
            .field("offset", &self.offset)
            .field("record_count", &self.record_count)
            .field("next_sequence", &self.next_sequence)
            .field("terminal_receipt", &self.terminal_receipt)
            .field("tail", &self.tail)
            .field("exhausted", &self.exhausted)
            .field("failed", &self.failed)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum GuardianOutputJournalError {
    #[error("invalid guardian output journal limits: {0}")]
    InvalidLimits(&'static str),
    #[error("invalid guardian output segment identity: {0}")]
    InvalidSegmentIdentity(&'static str),
    #[error("invalid guardian output recovery limits: {0}")]
    InvalidRecoveryLimits(&'static str),
    #[error("guardian output recovery range is outside the authenticated segment")]
    RecoveryRangeMismatch,
    #[error("guardian output recovery plaintext bound exceeded: {observed} > {maximum}")]
    RecoveryPlaintextByteLimit { observed: u64, maximum: u64 },
    #[error("guardian output recovery allocation failed")]
    RecoveryAllocationFailed,
    #[error("guardian output recovery cursor no longer matches its frozen segment authority")]
    RecoveryAuthorityMismatch,
    #[error("guardian output recovery cursor is terminal after a prior failure")]
    RecoveryCursorFailed,
    #[error("guardian output recovery receipt does not match its plaintext payload")]
    RecoveryPayloadBindingMismatch,
    #[error("guardian output delivery plaintext bound exceeded: {observed} > {maximum}")]
    DeliveryPayloadByteLimit { observed: u64, maximum: u32 },
    #[error("invalid guardian output encryption key: {0}")]
    InvalidEncryptionKey(&'static str),
    #[error("operating system entropy is unavailable for guardian output encryption")]
    EntropyUnavailable,
    #[error("guardian output key file read failed")]
    KeyFileRead(#[source] std::io::Error),
    #[error("guardian output key file write failed")]
    KeyFileWrite(#[source] std::io::Error),
    #[error("guardian output record encryption failed")]
    EncryptionFailed,
    #[error("guardian output record authentication or decryption failed")]
    DecryptionFailed,
    #[error("guardian output journal arithmetic overflow")]
    ArithmeticOverflow,
    #[error("guardian output journal descriptor is not a regular file")]
    NotRegularFile,
    #[error("guardian output recovery descriptor is not opened read-only")]
    RecoveryDescriptorNotReadOnly,
    #[error("guardian output append descriptor is not opened read-write")]
    AppendDescriptorNotReadWrite,
    #[error("guardian output append authority is unavailable because another writer holds it")]
    AppendWriterLeaseUnavailable(#[source] std::io::Error),
    #[error("guardian output append writer leases are unsupported on this target")]
    AppendWriterLeaseUnsupported,
    #[error("guardian output append authority does not hold its exclusive writer lease")]
    AppendWriterLeaseMissing,
    #[error("new guardian output journal child name is not one normalized path component")]
    InvalidNewSegmentName,
    #[error("descriptor-relative guardian output journal creation is unsupported on this target")]
    NewSegmentCreationUnsupported,
    #[error("new guardian output journal parent directory is not private current-user authority")]
    InsecureNewSegmentParent,
    #[error(
        "new guardian output journal file identity, ownership, mode, or link count is invalid"
    )]
    InsecureNewSegmentIdentity,
    #[error("new guardian output journal parent or child identity changed before activation")]
    NewSegmentPublicationIdentityChanged,
    #[error("new guardian output journal has no pending publication authority")]
    NewSegmentPublicationAuthorityMissing,
    #[error("new guardian output journal descriptor is not empty: found {actual} bytes")]
    NewSegmentNotEmpty { actual: u64 },
    #[error("guardian output journal parent descriptor is not a directory")]
    NotDirectory,
    #[error("guardian output journal file header is torn: found {actual} of {expected} bytes")]
    TornFileHeader { expected: usize, actual: u64 },
    #[error("guardian output journal file magic is invalid")]
    InvalidFileMagic,
    #[error("unsupported guardian output journal version {observed}")]
    UnsupportedVersion { observed: u32 },
    #[error("guardian output journal file header length is invalid: {observed}")]
    InvalidFileHeaderLength { observed: u32 },
    #[error("guardian output journal belongs to another durable pane")]
    PaneIdentityMismatch,
    #[error("guardian output journal segment identity or predecessor chain does not match")]
    SegmentIdentityMismatch,
    #[error("guardian output journal encryption key identity does not match")]
    KeyIdentityMismatch,
    #[error("guardian output journal file header authentication seal failed")]
    FileHeaderAuthenticationSealFailed,
    #[error("guardian output journal file header authentication failed")]
    FileHeaderAuthenticationFailed,
    #[error("guardian output journal exceeds its byte limit: {observed} > {maximum}")]
    LogByteLimit { observed: u64, maximum: u64 },
    #[error("guardian output journal record limit {maximum} is exhausted")]
    RecordLimit { maximum: u64 },
    #[error("guardian output journal record at byte {offset} has invalid magic")]
    InvalidRecordMagic { offset: u64 },
    #[error(
        "guardian output journal record at byte {offset} has invalid header length {observed}"
    )]
    InvalidRecordHeaderLength { offset: u64, observed: u32 },
    #[error("guardian output journal record at byte {offset} has nonzero reserved bytes")]
    NonCanonicalRecordHeader { offset: u64 },
    #[error("guardian output journal record at byte {offset} is empty")]
    EmptyRecord { offset: u64 },
    #[error("guardian output record is too large: {observed} > {maximum}")]
    RecordByteLimit { observed: u64, maximum: u32 },
    #[error(
        "guardian output ciphertext length is invalid: expected {expected}, observed {observed}"
    )]
    CiphertextLengthMismatch { expected: u32, observed: u32 },
    #[error(
        "guardian output plaintext length is invalid after authentication: expected {expected}, observed {observed}"
    )]
    PlaintextLengthMismatch { expected: u32, observed: u32 },
    #[error(
        "guardian output sequence mismatch at byte {offset}: expected {expected}, observed {observed}"
    )]
    SequenceMismatch {
        offset: u64,
        expected: u64,
        observed: u64,
    },
    #[error("guardian output record digest mismatch at sequence {sequence}")]
    RecordDigestMismatch { sequence: u64 },
    #[error("guardian output journal sequence space is exhausted")]
    SequenceExhausted,
    #[error(
        "new guardian output segment is not active until its parent directory is synchronized"
    )]
    DirectoryEntryNotDurable,
    #[error("guardian output journal has an incomplete tail and must be sealed")]
    IncompleteTail,
    #[error("guardian output journal is poisoned after an ambiguous write or sync failure")]
    Poisoned,
    #[error(
        "guardian output journal length changed outside its exclusive owner: expected {expected}, observed {observed}"
    )]
    ExternalLengthChange { expected: u64, observed: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JournalScan {
    committed_bytes: u64,
    record_count: u64,
    cumulative_plaintext_bytes: u64,
    next_sequence: Option<u64>,
    terminal_receipt: Option<GuardianOutputAppendReceipt>,
    authenticated_prefix_digest: [u8; 32],
    tail: GuardianOutputJournalTail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GuardianOutputDirectoryIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GuardianOutputFileIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    links: u64,
    bytes: u64,
}

struct GuardianOutputNewSegmentPublication {
    parent_directory: File,
    parent_identity: GuardianOutputDirectoryIdentity,
    child_name: OsString,
    child_identity: GuardianOutputFileIdentity,
}

struct RecoveryCollector {
    first_sequence: u64,
    limits: GuardianOutputRecoveryLimits,
    plaintext_bytes: u64,
    records: Vec<GuardianRecoveredOutputRecord>,
    next_recovery_sequence: Option<u64>,
}

impl RecoveryCollector {
    fn new(
        first_sequence: u64,
        limits: GuardianOutputRecoveryLimits,
    ) -> Result<Self, GuardianOutputJournalError> {
        Ok(Self {
            first_sequence,
            limits: limits.validate()?,
            plaintext_bytes: 0,
            records: Vec::new(),
            next_recovery_sequence: None,
        })
    }

    fn observe(
        &mut self,
        receipt: GuardianOutputAppendReceipt,
        plaintext: Zeroizing<Vec<u8>>,
    ) -> Result<(), GuardianOutputJournalError> {
        // The scanner intentionally authenticates the complete committed
        // prefix even after the requested page saturates. At that point this
        // method drops each just-opened plaintext before the next frame is
        // read, so post-limit temporary allocation remains bounded to one
        // record rather than the unreturned suffix.
        if receipt.sequence < self.first_sequence || self.next_recovery_sequence.is_some() {
            return Ok(());
        }
        let record_count = u64::try_from(self.records.len())
            .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
        if record_count >= self.limits.max_records {
            self.next_recovery_sequence = Some(receipt.sequence);
            return Ok(());
        }
        let plaintext_bytes = u64::try_from(plaintext.len())
            .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
        let projected_plaintext_bytes = self
            .plaintext_bytes
            .checked_add(plaintext_bytes)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if projected_plaintext_bytes > self.limits.max_plaintext_bytes {
            if self.records.is_empty() {
                return Err(GuardianOutputJournalError::RecoveryPlaintextByteLimit {
                    observed: projected_plaintext_bytes,
                    maximum: self.limits.max_plaintext_bytes,
                });
            }
            self.next_recovery_sequence = Some(receipt.sequence);
            return Ok(());
        }
        self.records
            .try_reserve(1)
            .map_err(|_| GuardianOutputJournalError::RecoveryAllocationFailed)?;
        self.records
            .push(GuardianRecoveredOutputRecord { receipt, plaintext });
        self.plaintext_bytes = projected_plaintext_bytes;
        Ok(())
    }
}

/// An exclusively owned append authority for one raw-output segment.
pub struct GuardianOutputJournal {
    file: File,
    identity: GuardianOutputSegmentIdentity,
    cipher: GuardianOutputCipher,
    limits: GuardianOutputJournalLimits,
    committed_bytes: u64,
    record_count: u64,
    cumulative_plaintext_bytes: u64,
    next_sequence: Option<u64>,
    terminal_receipt: Option<GuardianOutputAppendReceipt>,
    authenticated_prefix_digest: [u8; 32],
    tail: GuardianOutputJournalTail,
    new_segment_publication: Option<GuardianOutputNewSegmentPublication>,
    poisoned: bool,
    writer_lease_held: bool,
}

/// Existing-file-only authenticated recovery authority for one raw-output segment.
///
/// On Unix, construction requires an `O_RDONLY` descriptor.  The service layer
/// remains responsible for opening that descriptor relative to its pinned
/// private directory with `O_NOFOLLOW`.  This type deliberately exposes no
/// append, activation, descriptor, dereference, or append-authority conversion
/// seam.  A zero-length or torn existing file is evidence and is rejected
/// without initialization or rewrite.
#[cfg(unix)]
pub struct GuardianOutputJournalReader {
    authenticated: GuardianOutputJournal,
}

#[cfg(unix)]
fn guardian_output_descriptor_access_mode(file: &File) -> Result<i32, GuardianOutputJournalError> {
    let flags = nix::fcntl::fcntl(file, nix::fcntl::F_GETFL)
        .map_err(|error| GuardianOutputJournalError::Io(std::io::Error::from(error)))?;
    Ok(flags & nix::fcntl::OFlag::O_ACCMODE.bits())
}

#[cfg(unix)]
fn require_guardian_output_read_only_descriptor(
    file: &File,
) -> Result<(), GuardianOutputJournalError> {
    if guardian_output_descriptor_access_mode(file)? != nix::fcntl::OFlag::O_RDONLY.bits() {
        return Err(GuardianOutputJournalError::RecoveryDescriptorNotReadOnly);
    }
    Ok(())
}

#[cfg(unix)]
fn require_guardian_output_read_write_descriptor(
    file: &File,
) -> Result<(), GuardianOutputJournalError> {
    if guardian_output_descriptor_access_mode(file)? != nix::fcntl::OFlag::O_RDWR.bits() {
        return Err(GuardianOutputJournalError::AppendDescriptorNotReadWrite);
    }
    Ok(())
}

#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
))]
fn acquire_guardian_output_writer_lease(file: &File) -> Result<(), GuardianOutputJournalError> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        let error = std::io::Error::from(error);
        if error.kind() == std::io::ErrorKind::WouldBlock {
            GuardianOutputJournalError::AppendWriterLeaseUnavailable(error)
        } else {
            GuardianOutputJournalError::Io(error)
        }
    })
}

#[cfg(not(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "ios",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
)))]
fn acquire_guardian_output_writer_lease(_file: &File) -> Result<(), GuardianOutputJournalError> {
    Err(GuardianOutputJournalError::AppendWriterLeaseUnsupported)
}

#[cfg(unix)]
fn validate_new_segment_child_name(name: &OsStr) -> Result<(), GuardianOutputJournalError> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(component)) if component == name)
        || components.next().is_some()
    {
        return Err(GuardianOutputJournalError::InvalidNewSegmentName);
    }
    Ok(())
}

#[cfg(unix)]
fn capture_new_segment_parent_identity(
    parent_directory: &File,
) -> Result<GuardianOutputDirectoryIdentity, GuardianOutputJournalError> {
    let metadata = parent_directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(GuardianOutputJournalError::InsecureNewSegmentParent);
    }
    Ok(GuardianOutputDirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        owner: metadata.uid(),
    })
}

#[cfg(unix)]
fn capture_new_segment_file_identity(
    file: &File,
) -> Result<GuardianOutputFileIdentity, GuardianOutputJournalError> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(GuardianOutputJournalError::InsecureNewSegmentIdentity);
    }
    Ok(GuardianOutputFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        owner: metadata.uid(),
        links: metadata.nlink(),
        bytes: metadata.len(),
    })
}

#[cfg(unix)]
fn capture_new_segment_named_identity(
    parent_directory: &File,
    child_name: &OsStr,
) -> Result<GuardianOutputFileIdentity, GuardianOutputJournalError> {
    let metadata = rustix::fs::statat(
        parent_directory,
        child_name,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|error| GuardianOutputJournalError::Io(std::io::Error::from(error)))?;
    let mode = u32::from(metadata.st_mode);
    let owner = u32::try_from(metadata.st_uid)
        .map_err(|_| GuardianOutputJournalError::InsecureNewSegmentIdentity)?;
    let links = u64::from(metadata.st_nlink);
    let bytes = u64::try_from(metadata.st_size)
        .map_err(|_| GuardianOutputJournalError::InsecureNewSegmentIdentity)?;
    if rustix::fs::FileType::from_raw_mode(metadata.st_mode) != rustix::fs::FileType::RegularFile
        || owner != nix::unistd::geteuid().as_raw()
        || mode & 0o7777 != 0o600
        || links != 1
    {
        return Err(GuardianOutputJournalError::InsecureNewSegmentIdentity);
    }
    Ok(GuardianOutputFileIdentity {
        device: u64::try_from(metadata.st_dev)
            .map_err(|_| GuardianOutputJournalError::InsecureNewSegmentIdentity)?,
        inode: u64::try_from(metadata.st_ino)
            .map_err(|_| GuardianOutputJournalError::InsecureNewSegmentIdentity)?,
        mode,
        owner,
        links,
        bytes,
    })
}

impl GuardianOutputJournal {
    /// Create one new journal segment relative to an exact private directory.
    ///
    /// The child name must be one normalized path component.  Creation uses
    /// `O_CREAT | O_EXCL | O_NOFOLLOW`, and the exact parent, name, file inode,
    /// owner, mode, and link count remain pinned until directory activation.
    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    pub fn create_new_at(
        parent_directory: &File,
        child_name: &OsStr,
        identity: GuardianOutputSegmentIdentity,
        cipher: GuardianOutputCipher,
        limits: GuardianOutputJournalLimits,
    ) -> Result<Self, GuardianOutputJournalError> {
        validate_new_segment_child_name(child_name)?;
        let limits = limits.validate()?;
        let header = encode_file_header(identity, &cipher)?;
        let pinned_parent = parent_directory.try_clone()?;
        let parent_identity = capture_new_segment_parent_identity(&pinned_parent)?;
        if capture_new_segment_parent_identity(parent_directory)? != parent_identity {
            return Err(GuardianOutputJournalError::NewSegmentPublicationIdentityChanged);
        }
        let file = rustix::fs::openat(
            parent_directory,
            child_name,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .map(File::from)
        .map_err(|error| GuardianOutputJournalError::Io(std::io::Error::from(error)))?;
        rustix::fs::fchmod(&file, rustix::fs::Mode::from_raw_mode(0o600))
            .map_err(|error| GuardianOutputJournalError::Io(std::io::Error::from(error)))?;
        if capture_new_segment_parent_identity(&pinned_parent)? != parent_identity {
            return Err(GuardianOutputJournalError::NewSegmentPublicationIdentityChanged);
        }
        let initial_child_identity = capture_new_segment_file_identity(&file)?;
        if initial_child_identity.device != parent_identity.device
            || initial_child_identity.bytes != 0
            || capture_new_segment_named_identity(&pinned_parent, child_name)?
                != initial_child_identity
        {
            return Err(GuardianOutputJournalError::NewSegmentPublicationIdentityChanged);
        }
        let mut journal = Self::initialize_new_file(file, identity, cipher, limits, header)?;
        let child_identity = capture_new_segment_file_identity(&journal.file)?;
        if child_identity.bytes != journal.committed_bytes
            || capture_new_segment_parent_identity(&pinned_parent)? != parent_identity
            || capture_new_segment_named_identity(&pinned_parent, child_name)? != child_identity
        {
            return Err(GuardianOutputJournalError::NewSegmentPublicationIdentityChanged);
        }
        journal.new_segment_publication = Some(GuardianOutputNewSegmentPublication {
            parent_directory: pinned_parent,
            parent_identity,
            child_name: child_name.to_os_string(),
            child_identity,
        });
        Ok(journal)
    }

    #[cfg(not(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    )))]
    pub fn create_new_at(
        _parent_directory: &File,
        _child_name: &OsStr,
        _identity: GuardianOutputSegmentIdentity,
        _cipher: GuardianOutputCipher,
        _limits: GuardianOutputJournalLimits,
    ) -> Result<Self, GuardianOutputJournalError> {
        Err(GuardianOutputJournalError::NewSegmentCreationUnsupported)
    }

    fn initialize_new_file(
        mut file: File,
        identity: GuardianOutputSegmentIdentity,
        cipher: GuardianOutputCipher,
        limits: GuardianOutputJournalLimits,
        header: [u8; FILE_HEADER_BYTES],
    ) -> Result<Self, GuardianOutputJournalError> {
        #[cfg(unix)]
        require_guardian_output_read_write_descriptor(&file)?;
        acquire_guardian_output_writer_lease(&file)?;
        let limits = limits.validate()?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(GuardianOutputJournalError::NotRegularFile);
        }
        if metadata.len() != 0 {
            return Err(GuardianOutputJournalError::NewSegmentNotEmpty {
                actual: metadata.len(),
            });
        }
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.sync_all()?;
        Self::authenticate_existing(file, identity, cipher, limits, true)
    }

    /// Open one existing journal segment relative to an exact private directory
    /// and authenticate it as the sole append authority.
    ///
    /// Each call performs its own descriptor-relative `openat`, so callers
    /// cannot bypass writer exclusion with pre-duplicated file descriptors.
    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    ))]
    pub fn open_existing_for_append_at(
        parent_directory: &File,
        child_name: &OsStr,
        identity: GuardianOutputSegmentIdentity,
        cipher: GuardianOutputCipher,
        limits: GuardianOutputJournalLimits,
    ) -> Result<Self, GuardianOutputJournalError> {
        validate_new_segment_child_name(child_name)?;
        let parent_identity = capture_new_segment_parent_identity(parent_directory)?;
        let pinned_parent = parent_directory.try_clone()?;
        let file = rustix::fs::openat(
            parent_directory,
            child_name,
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map(File::from)
        .map_err(|error| GuardianOutputJournalError::Io(std::io::Error::from(error)))?;
        let file_identity = capture_new_segment_file_identity(&file)?;
        if file_identity.device != parent_identity.device
            || capture_new_segment_parent_identity(&pinned_parent)? != parent_identity
            || capture_new_segment_named_identity(&pinned_parent, child_name)? != file_identity
        {
            return Err(GuardianOutputJournalError::NewSegmentPublicationIdentityChanged);
        }
        let journal = Self::open_existing_append_file(file, identity, cipher, limits)?;
        if capture_new_segment_parent_identity(&pinned_parent)? != parent_identity
            || capture_new_segment_file_identity(&journal.file)? != file_identity
            || capture_new_segment_named_identity(&pinned_parent, child_name)? != file_identity
        {
            return Err(GuardianOutputJournalError::NewSegmentPublicationIdentityChanged);
        }
        Ok(journal)
    }

    #[cfg(not(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "ios",
        target_os = "linux",
        target_os = "macos",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "tvos",
        target_os = "visionos",
        target_os = "watchos",
    )))]
    pub fn open_existing_for_append_at(
        _parent_directory: &File,
        _child_name: &OsStr,
        _identity: GuardianOutputSegmentIdentity,
        _cipher: GuardianOutputCipher,
        _limits: GuardianOutputJournalLimits,
    ) -> Result<Self, GuardianOutputJournalError> {
        Err(GuardianOutputJournalError::NewSegmentCreationUnsupported)
    }

    fn open_existing_append_file(
        file: File,
        identity: GuardianOutputSegmentIdentity,
        cipher: GuardianOutputCipher,
        limits: GuardianOutputJournalLimits,
    ) -> Result<Self, GuardianOutputJournalError> {
        #[cfg(unix)]
        require_guardian_output_read_write_descriptor(&file)?;
        acquire_guardian_output_writer_lease(&file)?;
        Self::authenticate_existing(file, identity, cipher, limits, true)
    }

    /// Authenticate one existing descriptor without ever modifying it.
    fn authenticate_existing(
        mut file: File,
        identity: GuardianOutputSegmentIdentity,
        cipher: GuardianOutputCipher,
        limits: GuardianOutputJournalLimits,
        writer_lease_held: bool,
    ) -> Result<Self, GuardianOutputJournalError> {
        let limits = limits.validate()?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(GuardianOutputJournalError::NotRegularFile);
        }
        let physical_bytes = metadata.len();
        if physical_bytes > limits.max_log_bytes {
            return Err(GuardianOutputJournalError::LogByteLimit {
                observed: physical_bytes,
                maximum: limits.max_log_bytes,
            });
        }
        if physical_bytes < FILE_HEADER_BYTES_U64 {
            // A complete header from an older on-disk format can be shorter
            // than the authenticated v3 header.  Diagnose that condition as
            // an explicit unsupported version instead of misclassifying it
            // as a torn v3 file.  We only trust the version enough to reject
            // it after the fixed magic and version prefix is physically
            // complete; no legacy bytes are admitted or rewritten.
            if physical_bytes >= 16 {
                let mut prefix = [0_u8; 16];
                file.seek(SeekFrom::Start(0))?;
                file.read_exact(&mut prefix)?;
                if prefix[0..8] == FILE_MAGIC {
                    let observed = read_u32(&prefix[8..12]);
                    if observed != FORMAT_VERSION {
                        return Err(GuardianOutputJournalError::UnsupportedVersion { observed });
                    }
                }
            }
            return Err(GuardianOutputJournalError::TornFileHeader {
                expected: FILE_HEADER_BYTES,
                actual: physical_bytes,
            });
        }
        let scan = scan_journal(&mut file, physical_bytes, identity, &cipher, limits)?;
        Ok(Self {
            file,
            identity,
            cipher,
            limits,
            committed_bytes: scan.committed_bytes,
            record_count: scan.record_count,
            cumulative_plaintext_bytes: scan.cumulative_plaintext_bytes,
            next_sequence: scan.next_sequence,
            terminal_receipt: scan.terminal_receipt,
            authenticated_prefix_digest: scan.authenticated_prefix_digest,
            tail: scan.tail,
            new_segment_publication: None,
            poisoned: false,
            writer_lease_held,
        })
    }

    #[must_use]
    pub const fn identity(&self) -> GuardianOutputSegmentIdentity {
        self.identity
    }

    #[must_use]
    pub const fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.record_count
    }

    #[must_use]
    pub const fn cumulative_plaintext_bytes(&self) -> u64 {
        self.cumulative_plaintext_bytes
    }

    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }

    /// Verified terminal record authority reconstructed while opening this
    /// segment. It is sufficient to form the exact successor predecessor.
    #[must_use]
    pub const fn terminal_receipt(&self) -> Option<GuardianOutputAppendReceipt> {
        self.terminal_receipt
    }

    /// Digest of the complete authenticated committed prefix, including the
    /// exact file header and every ordered record frame.
    #[must_use]
    pub const fn authenticated_prefix_digest(&self) -> [u8; 32] {
        self.authenticated_prefix_digest
    }

    /// Verified terminal chain authority after reopen. An empty current
    /// segment has no receipt of its own and therefore preserves the exact
    /// authenticated predecessor authority.
    #[must_use]
    pub fn terminal_predecessor(&self) -> Option<GuardianOutputPredecessor> {
        self.terminal_receipt
            .map(GuardianOutputAppendReceipt::into_predecessor)
            .or(self.identity.predecessor)
    }

    #[must_use]
    pub const fn tail(&self) -> GuardianOutputJournalTail {
        self.tail
    }

    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    #[must_use]
    pub const fn directory_entry_sync_required(&self) -> bool {
        self.new_segment_publication.is_some()
    }

    fn revalidate_authenticated_prefix(&self) -> Result<(), GuardianOutputJournalError> {
        let expected_physical_bytes = match self.tail {
            GuardianOutputJournalTail::Clean => self.committed_bytes,
            GuardianOutputJournalTail::Incomplete {
                committed_bytes,
                trailing_bytes,
            } => {
                if committed_bytes != self.committed_bytes {
                    return Err(GuardianOutputJournalError::RecoveryAuthorityMismatch);
                }
                committed_bytes
                    .checked_add(trailing_bytes)
                    .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?
            }
        };
        let mut file = self.file.try_clone()?;
        let physical_bytes_before = file.metadata()?.len();
        if physical_bytes_before != expected_physical_bytes {
            return Err(GuardianOutputJournalError::ExternalLengthChange {
                expected: expected_physical_bytes,
                observed: physical_bytes_before,
            });
        }
        let scan = scan_journal(
            &mut file,
            physical_bytes_before,
            self.identity,
            &self.cipher,
            self.limits,
        )?;
        let physical_bytes_after = file.metadata()?.len();
        if physical_bytes_after != physical_bytes_before
            || scan.committed_bytes != self.committed_bytes
            || scan.record_count != self.record_count
            || scan.cumulative_plaintext_bytes != self.cumulative_plaintext_bytes
            || scan.next_sequence != self.next_sequence
            || scan.terminal_receipt != self.terminal_receipt
            || scan.authenticated_prefix_digest != self.authenticated_prefix_digest
            || scan.tail != self.tail
        {
            return Err(GuardianOutputJournalError::RecoveryAuthorityMismatch);
        }
        Ok(())
    }

    /// Synchronize the exact parent directory that published this new segment.
    ///
    /// A newly initialized file cannot accept output until this succeeds.  A
    /// file-level sync alone does not guarantee that the directory entry will
    /// survive a crash.  Creation retains the exact descriptor-relative
    /// publication authority, so activation accepts no caller-supplied parent.
    #[cfg(unix)]
    pub fn sync_parent_directory_and_activate(&mut self) -> Result<(), GuardianOutputJournalError> {
        let publication = self
            .new_segment_publication
            .as_ref()
            .ok_or(GuardianOutputJournalError::NewSegmentPublicationAuthorityMissing)?;
        self.revalidate_authenticated_prefix()?;
        if capture_new_segment_parent_identity(&publication.parent_directory)?
            != publication.parent_identity
            || publication.child_identity.bytes != self.committed_bytes
            || capture_new_segment_file_identity(&self.file)? != publication.child_identity
            || capture_new_segment_named_identity(
                &publication.parent_directory,
                &publication.child_name,
            )? != publication.child_identity
        {
            return Err(GuardianOutputJournalError::NewSegmentPublicationIdentityChanged);
        }
        publication.parent_directory.sync_all()?;
        self.revalidate_authenticated_prefix()?;
        if capture_new_segment_parent_identity(&publication.parent_directory)?
            != publication.parent_identity
            || publication.child_identity.bytes != self.committed_bytes
            || capture_new_segment_file_identity(&self.file)? != publication.child_identity
            || capture_new_segment_named_identity(
                &publication.parent_directory,
                &publication.child_name,
            )? != publication.child_identity
        {
            return Err(GuardianOutputJournalError::NewSegmentPublicationIdentityChanged);
        }
        self.new_segment_publication = None;
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn sync_parent_directory_and_activate(&mut self) -> Result<(), GuardianOutputJournalError> {
        Err(GuardianOutputJournalError::NewSegmentCreationUnsupported)
    }

    /// Create a frozen, stateful, single-pass recovery cursor.
    ///
    /// `max_record_plaintext_bytes` is a per-record allocation and delivery
    /// bound, not a page total.  This lets a caller stream the entire committed
    /// segment with constant resident plaintext memory and without rescanning
    /// the prefix between records.  The hard recovery cap still applies even
    /// when the journal was opened with a larger append limit.
    pub fn recovery_cursor(
        &self,
        first_sequence: u64,
        max_record_plaintext_bytes: u32,
    ) -> Result<GuardianOutputRecoveryCursor, GuardianOutputJournalError> {
        if self.poisoned {
            return Err(GuardianOutputJournalError::Poisoned);
        }
        if self.directory_entry_sync_required() {
            return Err(GuardianOutputJournalError::DirectoryEntryNotDurable);
        }
        if max_record_plaintext_bytes == 0
            || u64::from(max_record_plaintext_bytes) > RECOVERY_MAX_PLAINTEXT_BYTES
        {
            return Err(GuardianOutputJournalError::InvalidRecoveryLimits(
                "max_record_plaintext_bytes is zero or exceeds the hard recovery cap",
            ));
        }
        if first_sequence < self.identity.first_sequence {
            return Err(GuardianOutputJournalError::RecoveryRangeMismatch);
        }
        if self
            .next_sequence
            .is_some_and(|next_sequence| first_sequence > next_sequence)
        {
            return Err(GuardianOutputJournalError::RecoveryRangeMismatch);
        }

        let expected_physical_bytes = match self.tail {
            GuardianOutputJournalTail::Clean => self.committed_bytes,
            GuardianOutputJournalTail::Incomplete {
                committed_bytes,
                trailing_bytes,
            } => {
                if committed_bytes != self.committed_bytes {
                    return Err(GuardianOutputJournalError::RecoveryAuthorityMismatch);
                }
                committed_bytes
                    .checked_add(trailing_bytes)
                    .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?
            }
        };
        let file = self.file.try_clone()?;
        let observed_physical_bytes = file.metadata()?.len();
        if observed_physical_bytes != expected_physical_bytes {
            return Err(GuardianOutputJournalError::ExternalLengthChange {
                expected: expected_physical_bytes,
                observed: observed_physical_bytes,
            });
        }
        let mut file_header = [0_u8; FILE_HEADER_BYTES];
        read_exact_file_at(&file, &mut file_header, 0)?;
        validate_file_header(&file_header, self.identity, &self.cipher)?;
        let initial_authenticated_prefix_digest = initial_authenticated_prefix_digest(&file_header);

        Ok(GuardianOutputRecoveryCursor {
            file,
            identity: self.identity,
            cipher: self.cipher.clone(),
            journal_limits: self.limits,
            requested_first_sequence: first_sequence,
            max_record_plaintext_bytes,
            committed_bytes: self.committed_bytes,
            expected_record_count: self.record_count,
            expected_cumulative_plaintext_bytes: self.cumulative_plaintext_bytes,
            expected_next_sequence: self.next_sequence,
            expected_terminal_receipt: self.terminal_receipt,
            expected_authenticated_prefix_digest: self.authenticated_prefix_digest,
            tail: self.tail,
            offset: FILE_HEADER_BYTES_U64,
            record_count: 0,
            cumulative_plaintext_bytes: self
                .identity
                .predecessor
                .map_or(0, |predecessor| predecessor.cumulative_plaintext_bytes),
            next_sequence: Some(self.identity.first_sequence),
            terminal_receipt: None,
            authenticated_prefix_digest: initial_authenticated_prefix_digest,
            exhausted: false,
            failed: false,
            #[cfg(test)]
            verified_record_count: 0,
        })
    }

    /// Re-read and authenticate a bounded contiguous page of the committed
    /// plaintext suffix. Every frame in the committed prefix is verified;
    /// bytes from an incomplete physical tail are reported but never returned.
    pub fn recover_committed_range(
        &self,
        first_sequence: u64,
        limits: GuardianOutputRecoveryLimits,
    ) -> Result<GuardianOutputRecoveryBatch, GuardianOutputJournalError> {
        if self.poisoned {
            return Err(GuardianOutputJournalError::Poisoned);
        }
        if self.directory_entry_sync_required() {
            return Err(GuardianOutputJournalError::DirectoryEntryNotDurable);
        }
        if first_sequence < self.identity.first_sequence {
            return Err(GuardianOutputJournalError::RecoveryRangeMismatch);
        }
        if self
            .next_sequence
            .is_some_and(|next_sequence| first_sequence > next_sequence)
        {
            return Err(GuardianOutputJournalError::RecoveryRangeMismatch);
        }
        let mut collector = RecoveryCollector::new(first_sequence, limits)?;
        let mut file = self.file.try_clone()?;
        let expected_physical_bytes = match self.tail {
            GuardianOutputJournalTail::Clean => self.committed_bytes,
            GuardianOutputJournalTail::Incomplete {
                committed_bytes,
                trailing_bytes,
            } => {
                if committed_bytes != self.committed_bytes {
                    return Err(GuardianOutputJournalError::ExternalLengthChange {
                        expected: self.committed_bytes,
                        observed: committed_bytes,
                    });
                }
                committed_bytes
                    .checked_add(trailing_bytes)
                    .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?
            }
        };
        let physical_bytes_before = file.metadata()?.len();
        if physical_bytes_before != expected_physical_bytes {
            return Err(GuardianOutputJournalError::ExternalLengthChange {
                expected: expected_physical_bytes,
                observed: physical_bytes_before,
            });
        }
        let scan = scan_journal_with_recovery(
            &mut file,
            physical_bytes_before,
            self.identity,
            &self.cipher,
            self.limits,
            Some(&mut collector),
        )?;
        let physical_bytes_after = file.metadata()?.len();
        if physical_bytes_before != physical_bytes_after {
            return Err(GuardianOutputJournalError::ExternalLengthChange {
                expected: physical_bytes_before,
                observed: physical_bytes_after,
            });
        }
        if scan.committed_bytes != self.committed_bytes
            || scan.record_count != self.record_count
            || scan.cumulative_plaintext_bytes != self.cumulative_plaintext_bytes
            || scan.next_sequence != self.next_sequence
            || scan.terminal_receipt != self.terminal_receipt
            || scan.authenticated_prefix_digest != self.authenticated_prefix_digest
            || scan.tail != self.tail
        {
            return Err(GuardianOutputJournalError::ExternalLengthChange {
                expected: self.committed_bytes,
                observed: scan.committed_bytes,
            });
        }
        Ok(GuardianOutputRecoveryBatch {
            segment_identity: self.identity,
            requested_first_sequence: first_sequence,
            records: collector.records,
            next_recovery_sequence: collector.next_recovery_sequence,
            committed_next_sequence: scan.next_sequence,
            committed_log_bytes: scan.committed_bytes,
            cumulative_plaintext_bytes: scan.cumulative_plaintext_bytes,
            terminal_receipt: scan.terminal_receipt,
            tail: scan.tail,
        })
    }

    /// Append and synchronize one nonempty raw PTY output record.
    ///
    /// The returned receipt is the only success signal that permits mux
    /// delivery.  Any write or synchronization error poisons this instance
    /// because the durable disposition is then ambiguous; callers must reopen
    /// and reconcile the segment rather than retrying blindly.
    pub fn append_and_sync(
        &mut self,
        payload: &[u8],
    ) -> Result<GuardianOutputAppendReceipt, GuardianOutputJournalError> {
        if !self.writer_lease_held {
            return Err(GuardianOutputJournalError::AppendWriterLeaseMissing);
        }
        if self.poisoned {
            return Err(GuardianOutputJournalError::Poisoned);
        }
        if self.directory_entry_sync_required() {
            return Err(GuardianOutputJournalError::DirectoryEntryNotDurable);
        }
        if self.tail != GuardianOutputJournalTail::Clean {
            return Err(GuardianOutputJournalError::IncompleteTail);
        }
        if payload.is_empty() {
            return Err(GuardianOutputJournalError::EmptyRecord {
                offset: self.committed_bytes,
            });
        }
        let observed_payload_bytes = u64::try_from(payload.len())
            .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
        if observed_payload_bytes > u64::from(self.limits.max_record_bytes) {
            return Err(GuardianOutputJournalError::RecordByteLimit {
                observed: observed_payload_bytes,
                maximum: self.limits.max_record_bytes,
            });
        }
        if self.record_count >= self.limits.max_records {
            return Err(GuardianOutputJournalError::RecordLimit {
                maximum: self.limits.max_records,
            });
        }
        let sequence = self
            .next_sequence
            .ok_or(GuardianOutputJournalError::SequenceExhausted)?;
        let payload_bytes = u32::try_from(payload.len())
            .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
        let cumulative_plaintext_bytes = self
            .cumulative_plaintext_bytes
            .checked_add(u64::from(payload_bytes))
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        let (nonce, ciphertext) =
            self.cipher
                .seal(self.identity, sequence, payload_bytes, payload)?;
        let ciphertext_bytes = u32::try_from(ciphertext.len())
            .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
        let expected_ciphertext_bytes = payload_bytes
            .checked_add(AEAD_TAG_BYTES)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if ciphertext_bytes != expected_ciphertext_bytes {
            return Err(GuardianOutputJournalError::CiphertextLengthMismatch {
                expected: expected_ciphertext_bytes,
                observed: ciphertext_bytes,
            });
        }
        let frame_bytes = RECORD_HEADER_BYTES_U64
            .checked_add(u64::from(ciphertext_bytes))
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        let projected_bytes = self
            .committed_bytes
            .checked_add(frame_bytes)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if projected_bytes > self.limits.max_log_bytes {
            return Err(GuardianOutputJournalError::LogByteLimit {
                observed: projected_bytes,
                maximum: self.limits.max_log_bytes,
            });
        }
        let physical_bytes = self.file.metadata()?.len();
        if physical_bytes != self.committed_bytes {
            self.poisoned = true;
            return Err(GuardianOutputJournalError::ExternalLengthChange {
                expected: self.committed_bytes,
                observed: physical_bytes,
            });
        }
        let record_digest = record_digest(
            self.identity,
            sequence,
            payload_bytes,
            ciphertext_bytes,
            &nonce,
            &ciphertext,
        );
        let plaintext_delivery_digest =
            plaintext_delivery_digest(self.identity.segment_id, sequence, payload);
        let header = encode_record_header(
            sequence,
            payload_bytes,
            ciphertext_bytes,
            nonce,
            record_digest,
        );
        let result = (|| -> std::io::Result<()> {
            self.file.seek(SeekFrom::Start(self.committed_bytes))?;
            self.file.write_all(&header)?;
            self.file.write_all(&ciphertext)?;
            self.file.sync_all()
        })();
        if let Err(error) = result {
            self.poisoned = true;
            return Err(GuardianOutputJournalError::Io(error));
        }
        self.committed_bytes = projected_bytes;
        self.record_count = self
            .record_count
            .checked_add(1)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        self.cumulative_plaintext_bytes = cumulative_plaintext_bytes;
        self.next_sequence = sequence.checked_add(1);
        self.authenticated_prefix_digest = extend_authenticated_prefix_digest(
            self.authenticated_prefix_digest,
            &header,
            &ciphertext,
        );
        let receipt = GuardianOutputAppendReceipt {
            segment_id: self.identity.segment_id,
            sequence,
            payload_bytes,
            cumulative_plaintext_bytes,
            committed_log_bytes: projected_bytes,
            record_digest,
            plaintext_delivery_digest,
        };
        self.terminal_receipt = Some(receipt);
        Ok(receipt)
    }
}

#[cfg(unix)]
impl GuardianOutputJournalReader {
    /// Authenticate one existing journal through an exact `O_RDONLY` descriptor.
    ///
    /// This constructor shares the append journal's complete header, record
    /// chain, AEAD, digest, sequence, and resource-bound validation, but it has
    /// no initialization branch and mints no mutation authority.
    pub fn open_existing(
        file: File,
        identity: GuardianOutputSegmentIdentity,
        cipher: GuardianOutputCipher,
        limits: GuardianOutputJournalLimits,
    ) -> Result<Self, GuardianOutputJournalError> {
        require_guardian_output_read_only_descriptor(&file)?;
        Ok(Self {
            authenticated: GuardianOutputJournal::authenticate_existing(
                file, identity, cipher, limits, false,
            )?,
        })
    }

    #[must_use]
    pub const fn identity(&self) -> GuardianOutputSegmentIdentity {
        self.authenticated.identity()
    }

    #[must_use]
    pub const fn committed_bytes(&self) -> u64 {
        self.authenticated.committed_bytes()
    }

    #[must_use]
    pub const fn record_count(&self) -> u64 {
        self.authenticated.record_count()
    }

    #[must_use]
    pub const fn cumulative_plaintext_bytes(&self) -> u64 {
        self.authenticated.cumulative_plaintext_bytes()
    }

    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.authenticated.next_sequence()
    }

    #[must_use]
    pub const fn terminal_receipt(&self) -> Option<GuardianOutputAppendReceipt> {
        self.authenticated.terminal_receipt()
    }

    #[must_use]
    pub const fn authenticated_prefix_digest(&self) -> [u8; 32] {
        self.authenticated.authenticated_prefix_digest()
    }

    #[must_use]
    pub fn terminal_predecessor(&self) -> Option<GuardianOutputPredecessor> {
        self.authenticated.terminal_predecessor()
    }

    #[must_use]
    pub const fn tail(&self) -> GuardianOutputJournalTail {
        self.authenticated.tail()
    }

    pub fn recovery_cursor(
        &self,
        first_sequence: u64,
        max_record_plaintext_bytes: u32,
    ) -> Result<GuardianOutputRecoveryCursor, GuardianOutputJournalError> {
        self.authenticated
            .recovery_cursor(first_sequence, max_record_plaintext_bytes)
    }

    pub fn recover_committed_range(
        &self,
        first_sequence: u64,
        limits: GuardianOutputRecoveryLimits,
    ) -> Result<GuardianOutputRecoveryBatch, GuardianOutputJournalError> {
        self.authenticated
            .recover_committed_range(first_sequence, limits)
    }
}

fn encode_file_header(
    identity: GuardianOutputSegmentIdentity,
    cipher: &GuardianOutputCipher,
) -> Result<[u8; FILE_HEADER_BYTES], GuardianOutputJournalError> {
    let mut header = [0_u8; FILE_HEADER_BYTES];
    header[0..8].copy_from_slice(&FILE_MAGIC);
    header[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&FILE_HEADER_BYTES_U32.to_le_bytes());
    header[16..32].copy_from_slice(identity.durable_pane_id.as_bytes());
    header[32..48].copy_from_slice(identity.segment_id.as_bytes());
    header[48..56].copy_from_slice(&identity.first_sequence.to_le_bytes());
    if let Some(previous) = identity.predecessor {
        header[56..72].copy_from_slice(previous.segment_id.as_bytes());
        header[72..80].copy_from_slice(&previous.last_sequence.to_le_bytes());
        header[80..112].copy_from_slice(&previous.terminal_record_digest);
        header[120..128].copy_from_slice(&previous.cumulative_plaintext_bytes.to_le_bytes());
        header[128..136].copy_from_slice(&previous.committed_log_bytes.to_le_bytes());
    }
    header[112..120].copy_from_slice(&cipher.key_id);
    let mut nonce = [0_u8; NONCE_BYTES];
    OsRng
        .try_fill_bytes(&mut nonce)
        .map_err(|_| GuardianOutputJournalError::EntropyUnavailable)?;
    let aad = file_header_aad(&header[0..136]);
    let authentication_tag = cipher
        .cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &[],
                aad: &aad,
            },
        )
        .map_err(|_| GuardianOutputJournalError::FileHeaderAuthenticationSealFailed)?;
    if authentication_tag.len() != AEAD_TAG_BYTES_USIZE {
        return Err(GuardianOutputJournalError::FileHeaderAuthenticationSealFailed);
    }
    header[136..160].copy_from_slice(&nonce);
    header[160..176].copy_from_slice(&authentication_tag);
    Ok(header)
}

fn validate_file_header(
    header: &[u8; FILE_HEADER_BYTES],
    identity: GuardianOutputSegmentIdentity,
    cipher: &GuardianOutputCipher,
) -> Result<(), GuardianOutputJournalError> {
    if header[0..8] != FILE_MAGIC {
        return Err(GuardianOutputJournalError::InvalidFileMagic);
    }
    let version = read_u32(&header[8..12]);
    if version != FORMAT_VERSION {
        return Err(GuardianOutputJournalError::UnsupportedVersion { observed: version });
    }
    let header_bytes = read_u32(&header[12..16]);
    if header_bytes != FILE_HEADER_BYTES_U32 {
        return Err(GuardianOutputJournalError::InvalidFileHeaderLength {
            observed: header_bytes,
        });
    }
    if &header[16..32] != identity.durable_pane_id.as_bytes() {
        return Err(GuardianOutputJournalError::PaneIdentityMismatch);
    }
    if &header[32..48] != identity.segment_id.as_bytes()
        || read_u64(&header[48..56]) != identity.first_sequence
    {
        return Err(GuardianOutputJournalError::SegmentIdentityMismatch);
    }
    let predecessor_matches = match identity.predecessor {
        Some(previous) => {
            &header[56..72] == previous.segment_id.as_bytes()
                && read_u64(&header[72..80]) == previous.last_sequence
                && header[80..112] == previous.terminal_record_digest
                && read_u64(&header[120..128]) == previous.cumulative_plaintext_bytes
                && read_u64(&header[128..136]) == previous.committed_log_bytes
        }
        None => {
            header[56..112].iter().all(|byte| *byte == 0)
                && header[120..136].iter().all(|byte| *byte == 0)
        }
    };
    if !predecessor_matches {
        return Err(GuardianOutputJournalError::SegmentIdentityMismatch);
    }
    if header[112..120] != cipher.key_id {
        return Err(GuardianOutputJournalError::KeyIdentityMismatch);
    }
    let mut nonce = [0_u8; NONCE_BYTES];
    nonce.copy_from_slice(&header[136..160]);
    let aad = file_header_aad(&header[0..136]);
    let plaintext = cipher
        .cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &header[160..176],
                aad: &aad,
            },
        )
        .map_err(|_| GuardianOutputJournalError::FileHeaderAuthenticationFailed)?;
    if !plaintext.is_empty() {
        return Err(GuardianOutputJournalError::FileHeaderAuthenticationFailed);
    }
    Ok(())
}

fn file_header_aad(canonical_header: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(FILE_HEADER_AEAD_DOMAIN.len() + canonical_header.len());
    aad.extend_from_slice(FILE_HEADER_AEAD_DOMAIN);
    aad.extend_from_slice(canonical_header);
    aad
}

fn encode_record_header(
    sequence: u64,
    plaintext_bytes: u32,
    ciphertext_bytes: u32,
    nonce: [u8; NONCE_BYTES],
    record_digest: [u8; 32],
) -> [u8; RECORD_HEADER_BYTES] {
    let mut header = [0_u8; RECORD_HEADER_BYTES];
    header[0..8].copy_from_slice(&RECORD_MAGIC);
    header[8..16].copy_from_slice(&sequence.to_le_bytes());
    header[16..20].copy_from_slice(&plaintext_bytes.to_le_bytes());
    header[20..24].copy_from_slice(&ciphertext_bytes.to_le_bytes());
    header[24..28].copy_from_slice(&RECORD_HEADER_BYTES_U32.to_le_bytes());
    header[32..56].copy_from_slice(&nonce);
    header[56..88].copy_from_slice(&record_digest);
    header
}

fn record_digest(
    identity: GuardianOutputSegmentIdentity,
    sequence: u64,
    plaintext_bytes: u32,
    ciphertext_bytes: u32,
    nonce: &[u8; NONCE_BYTES],
    ciphertext: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DIGEST_DOMAIN);
    hasher.update(FORMAT_VERSION.to_le_bytes());
    hasher.update(identity.durable_pane_id.as_bytes());
    hasher.update(identity.segment_id.as_bytes());
    hasher.update(sequence.to_le_bytes());
    hasher.update(u64::from(plaintext_bytes).to_le_bytes());
    hasher.update(u64::from(ciphertext_bytes).to_le_bytes());
    hasher.update(nonce);
    hasher.update(ciphertext);
    hasher.finalize().into()
}

fn initial_authenticated_prefix_digest(file_header: &[u8; FILE_HEADER_BYTES]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(AUTHENTICATED_PREFIX_DIGEST_DOMAIN);
    hasher.update(FORMAT_VERSION.to_le_bytes());
    hasher.update(file_header);
    hasher.finalize().into()
}

fn extend_authenticated_prefix_digest(
    previous: [u8; 32],
    record_header: &[u8; RECORD_HEADER_BYTES],
    ciphertext: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(AUTHENTICATED_PREFIX_DIGEST_DOMAIN);
    hasher.update(FORMAT_VERSION.to_le_bytes());
    hasher.update(previous);
    hasher.update(record_header);
    hasher.update(ciphertext);
    hasher.finalize().into()
}

fn plaintext_delivery_digest(segment_id: Uuid, sequence: u64, payload: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PLAINTEXT_DELIVERY_DIGEST_DOMAIN);
    hasher.update(FORMAT_VERSION.to_le_bytes());
    hasher.update(segment_id.as_bytes());
    hasher.update(sequence.to_le_bytes());
    hasher.update(
        u64::try_from(payload.len())
            .unwrap_or(u64::MAX)
            .to_le_bytes(),
    );
    hasher.update(payload);
    hasher.finalize().into()
}

fn record_aad(
    identity: GuardianOutputSegmentIdentity,
    sequence: u64,
    plaintext_bytes: u32,
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(RECORD_AAD_DOMAIN.len() + 4 + 16 + 16 + 8 + 4);
    aad.extend_from_slice(RECORD_AAD_DOMAIN);
    aad.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    aad.extend_from_slice(identity.durable_pane_id.as_bytes());
    aad.extend_from_slice(identity.segment_id.as_bytes());
    aad.extend_from_slice(&sequence.to_le_bytes());
    aad.extend_from_slice(&plaintext_bytes.to_le_bytes());
    aad
}

fn scan_journal<R: Read + Seek>(
    reader: &mut R,
    physical_bytes: u64,
    identity: GuardianOutputSegmentIdentity,
    cipher: &GuardianOutputCipher,
    limits: GuardianOutputJournalLimits,
) -> Result<JournalScan, GuardianOutputJournalError> {
    scan_journal_with_recovery(reader, physical_bytes, identity, cipher, limits, None)
}

fn scan_journal_with_recovery<R: Read + Seek>(
    reader: &mut R,
    physical_bytes: u64,
    identity: GuardianOutputSegmentIdentity,
    cipher: &GuardianOutputCipher,
    limits: GuardianOutputJournalLimits,
    mut recovery: Option<&mut RecoveryCollector>,
) -> Result<JournalScan, GuardianOutputJournalError> {
    reader.seek(SeekFrom::Start(0))?;
    let mut file_header = [0_u8; FILE_HEADER_BYTES];
    reader.read_exact(&mut file_header)?;
    validate_file_header(&file_header, identity, cipher)?;
    let mut authenticated_prefix_digest = initial_authenticated_prefix_digest(&file_header);

    let mut committed_bytes = FILE_HEADER_BYTES_U64;
    let mut record_count = 0_u64;
    let mut cumulative_plaintext_bytes = identity
        .predecessor
        .map_or(0, |predecessor| predecessor.cumulative_plaintext_bytes);
    let mut next_sequence = Some(identity.first_sequence);
    let mut terminal_receipt = None;
    while committed_bytes < physical_bytes {
        let remaining = physical_bytes
            .checked_sub(committed_bytes)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if remaining < RECORD_HEADER_BYTES_U64 {
            return Ok(JournalScan {
                committed_bytes,
                record_count,
                cumulative_plaintext_bytes,
                next_sequence,
                terminal_receipt,
                authenticated_prefix_digest,
                tail: GuardianOutputJournalTail::Incomplete {
                    committed_bytes,
                    trailing_bytes: remaining,
                },
            });
        }
        if record_count >= limits.max_records {
            return Err(GuardianOutputJournalError::RecordLimit {
                maximum: limits.max_records,
            });
        }
        reader.seek(SeekFrom::Start(committed_bytes))?;
        let mut record_header = [0_u8; RECORD_HEADER_BYTES];
        reader.read_exact(&mut record_header)?;
        if record_header[0..8] != RECORD_MAGIC {
            return Err(GuardianOutputJournalError::InvalidRecordMagic {
                offset: committed_bytes,
            });
        }
        let sequence = read_u64(&record_header[8..16]);
        let expected = next_sequence.ok_or(GuardianOutputJournalError::SequenceExhausted)?;
        if sequence != expected {
            return Err(GuardianOutputJournalError::SequenceMismatch {
                offset: committed_bytes,
                expected,
                observed: sequence,
            });
        }
        let plaintext_bytes = read_u32(&record_header[16..20]);
        let ciphertext_bytes = read_u32(&record_header[20..24]);
        let record_header_bytes = read_u32(&record_header[24..28]);
        if record_header_bytes != RECORD_HEADER_BYTES_U32 {
            return Err(GuardianOutputJournalError::InvalidRecordHeaderLength {
                offset: committed_bytes,
                observed: record_header_bytes,
            });
        }
        if record_header[28..32] != [0_u8; 4] || record_header[88..96] != [0_u8; 8] {
            return Err(GuardianOutputJournalError::NonCanonicalRecordHeader {
                offset: committed_bytes,
            });
        }
        if plaintext_bytes == 0 {
            return Err(GuardianOutputJournalError::EmptyRecord {
                offset: committed_bytes,
            });
        }
        if plaintext_bytes > limits.max_record_bytes {
            return Err(GuardianOutputJournalError::RecordByteLimit {
                observed: u64::from(plaintext_bytes),
                maximum: limits.max_record_bytes,
            });
        }
        let expected_ciphertext_bytes = plaintext_bytes
            .checked_add(AEAD_TAG_BYTES)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if ciphertext_bytes != expected_ciphertext_bytes {
            return Err(GuardianOutputJournalError::CiphertextLengthMismatch {
                expected: expected_ciphertext_bytes,
                observed: ciphertext_bytes,
            });
        }
        let frame_bytes = RECORD_HEADER_BYTES_U64
            .checked_add(u64::from(ciphertext_bytes))
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if remaining < frame_bytes {
            return Ok(JournalScan {
                committed_bytes,
                record_count,
                cumulative_plaintext_bytes,
                next_sequence,
                terminal_receipt,
                authenticated_prefix_digest,
                tail: GuardianOutputJournalTail::Incomplete {
                    committed_bytes,
                    trailing_bytes: remaining,
                },
            });
        }
        let ciphertext_capacity = usize::try_from(ciphertext_bytes)
            .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?;
        let mut ciphertext = vec![0_u8; ciphertext_capacity];
        reader.read_exact(&mut ciphertext)?;
        let mut nonce = [0_u8; NONCE_BYTES];
        nonce.copy_from_slice(&record_header[32..56]);
        let expected_digest = record_digest(
            identity,
            sequence,
            plaintext_bytes,
            ciphertext_bytes,
            &nonce,
            &ciphertext,
        );
        if record_header[56..88] != expected_digest {
            return Err(GuardianOutputJournalError::RecordDigestMismatch { sequence });
        }
        // Own decrypted terminal content in a zeroizing buffer immediately,
        // before any later validation or arithmetic can return early.
        let plaintext = Zeroizing::new(cipher.open(
            identity,
            sequence,
            plaintext_bytes,
            &nonce,
            &ciphertext,
        )?);
        if plaintext.len()
            != usize::try_from(plaintext_bytes)
                .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?
        {
            return Err(GuardianOutputJournalError::PlaintextLengthMismatch {
                expected: plaintext_bytes,
                observed: u32::try_from(plaintext.len())
                    .map_err(|_| GuardianOutputJournalError::ArithmeticOverflow)?,
            });
        }
        let projected_committed_bytes = committed_bytes
            .checked_add(frame_bytes)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        let projected_record_count = record_count
            .checked_add(1)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        let projected_cumulative_plaintext_bytes = cumulative_plaintext_bytes
            .checked_add(u64::from(plaintext_bytes))
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        let receipt = GuardianOutputAppendReceipt {
            segment_id: identity.segment_id,
            sequence,
            payload_bytes: plaintext_bytes,
            cumulative_plaintext_bytes: projected_cumulative_plaintext_bytes,
            committed_log_bytes: projected_committed_bytes,
            record_digest: expected_digest,
            plaintext_delivery_digest: plaintext_delivery_digest(
                identity.segment_id,
                sequence,
                plaintext.as_slice(),
            ),
        };
        if let Some(collector) = recovery.as_deref_mut() {
            collector.observe(receipt, plaintext)?;
        }
        authenticated_prefix_digest = extend_authenticated_prefix_digest(
            authenticated_prefix_digest,
            &record_header,
            &ciphertext,
        );
        committed_bytes = projected_committed_bytes;
        record_count = projected_record_count;
        cumulative_plaintext_bytes = projected_cumulative_plaintext_bytes;
        next_sequence = sequence.checked_add(1);
        terminal_receipt = Some(receipt);
    }
    Ok(JournalScan {
        committed_bytes,
        record_count,
        cumulative_plaintext_bytes,
        next_sequence,
        terminal_receipt,
        authenticated_prefix_digest,
        tail: GuardianOutputJournalTail::Clean,
    })
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut fixed = [0_u8; 4];
    fixed.copy_from_slice(bytes);
    u32::from_le_bytes(fixed)
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut fixed = [0_u8; 8];
    fixed.copy_from_slice(bytes);
    u64::from_le_bytes(fixed)
}

fn read_i64(bytes: &[u8]) -> i64 {
    let mut fixed = [0_u8; 8];
    fixed.copy_from_slice(bytes);
    i64::from_le_bytes(fixed)
}

fn read_exact_file_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buffer.is_empty() {
        match read_file_at(file, buffer, offset) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "guardian output journal positional read reached EOF",
                ));
            }
            Ok(read) => {
                offset = offset
                    .checked_add(u64::try_from(read).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "guardian output journal positional read length overflow",
                        )
                    })?)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "guardian output journal positional read offset overflow",
                        )
                    })?;
                let (_, remaining) = buffer.split_at_mut(read);
                buffer = remaining;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_file_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt as _;

    file.read_at(buffer, offset)
}

#[cfg(windows)]
fn read_file_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt as _;

    file.seek_read(buffer, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_file_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<usize> {
    let mut clone = file.try_clone()?;
    clone.seek(SeekFrom::Start(offset))?;
    clone.read(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn hex_encode(bytes: &[u8]) -> String {
        use std::fmt::Write as _;

        let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
        for byte in bytes {
            write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
        }
        encoded
    }

    #[cfg(unix)]
    fn create_journal_file(path: &std::path::Path) -> File {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.mode(0o600);
        }
        options.open(path).expect("create private test journal")
    }

    #[cfg(unix)]
    fn create_new_test_journal(
        parent: &File,
        path: &std::path::Path,
        identity: GuardianOutputSegmentIdentity,
        cipher: GuardianOutputCipher,
        limits: GuardianOutputJournalLimits,
    ) -> Result<GuardianOutputJournal, GuardianOutputJournalError> {
        rustix::fs::fchmod(parent, rustix::fs::Mode::from_raw_mode(0o700))
            .expect("make test journal parent private");
        GuardianOutputJournal::create_new_at(
            parent,
            path.file_name()
                .expect("test journal path has a child name"),
            identity,
            cipher,
            limits,
        )
    }

    #[cfg(unix)]
    fn open_test_journal_for_append(
        parent: &File,
        path: &std::path::Path,
        identity: GuardianOutputSegmentIdentity,
        cipher: GuardianOutputCipher,
        limits: GuardianOutputJournalLimits,
    ) -> Result<GuardianOutputJournal, GuardianOutputJournalError> {
        rustix::fs::fchmod(parent, rustix::fs::Mode::from_raw_mode(0o700))
            .expect("keep test journal parent private");
        GuardianOutputJournal::open_existing_for_append_at(
            parent,
            path.file_name()
                .expect("test journal path has a child name"),
            identity,
            cipher,
            limits,
        )
    }

    #[cfg(unix)]
    fn open_read_only_journal_file(path: &std::path::Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .open(path)
            .expect("open read-only test journal")
    }

    fn pane() -> Uuid {
        Uuid::from_bytes([0x42; 16])
    }

    fn identity() -> GuardianOutputSegmentIdentity {
        GuardianOutputSegmentIdentity::new(pane(), Uuid::from_bytes([0x24; 16]), 1, None)
            .expect("fixture segment identity is valid")
    }

    fn cipher() -> GuardianOutputCipher {
        GuardianOutputCipher::try_from_key_slice(&[0x71; 32])
            .expect("fixture encryption key is valid")
    }

    #[test]
    fn replay_stable_catalog_adoption_metadata_repeats_exactly_and_separates_inputs() {
        let cipher = cipher();
        let aad = b"checkpoint-stage-v3:catalog-adoption:complete-context";
        let plaintext = b"catalog-adoption-inner-envelope-v1:complete-binding";
        let (first_nonce, first_ciphertext) = cipher
            .seal_replay_stable_catalog_adoption_metadata(plaintext, aad)
            .expect("seal first replay-stable adoption envelope");
        let (retry_nonce, retry_ciphertext) = cipher
            .seal_replay_stable_catalog_adoption_metadata(plaintext, aad)
            .expect("retry the exact replay-stable adoption envelope");
        assert_eq!(retry_nonce, first_nonce);
        assert_eq!(retry_ciphertext, first_ciphertext);
        assert_eq!(
            cipher
                .open_guardian_metadata(&first_nonce, &first_ciphertext, aad)
                .expect("open replay-stable adoption envelope")
                .as_slice(),
            plaintext
        );

        for index in 0..aad.len() {
            let mut changed_aad = aad.to_vec();
            changed_aad[index] ^= 1;
            let (changed_nonce, _) = cipher
                .seal_replay_stable_catalog_adoption_metadata(plaintext, &changed_aad)
                .expect("seal adoption envelope with one changed AAD byte");
            assert_ne!(
                changed_nonce, first_nonce,
                "AAD byte {index} was not nonce-bound"
            );
        }
        for index in 0..plaintext.len() {
            let mut changed_plaintext = plaintext.to_vec();
            changed_plaintext[index] ^= 1;
            let (changed_nonce, _) = cipher
                .seal_replay_stable_catalog_adoption_metadata(&changed_plaintext, aad)
                .expect("seal adoption envelope with one changed plaintext byte");
            assert_ne!(
                changed_nonce, first_nonce,
                "plaintext byte {index} was not nonce-bound"
            );
        }

        let other_cipher = GuardianOutputCipher::try_from_key_slice(&[0x72; 32])
            .expect("construct distinct adoption key fixture");
        let (other_key_nonce, _) = other_cipher
            .seal_replay_stable_catalog_adoption_metadata(plaintext, aad)
            .expect("seal adoption envelope under distinct key identity");
        assert_ne!(other_key_nonce, first_nonce);
    }

    fn scrollback_identity() -> GuardianScrollbackRowIdentity {
        GuardianScrollbackRowIdentity::new([0x42; 16], [0x24; 16], 7, -3, 11)
            .expect("fixture scrollback identity is valid")
    }

    #[test]
    fn encrypted_scrollback_row_roundtrips_without_plaintext_or_debug_disclosure() {
        let plaintext = b"semantic-row-secret-with-style-width-and-link";
        let cipher = cipher();
        let sealed = cipher
            .seal_scrollback_row(scrollback_identity(), plaintext)
            .expect("seal exact row");
        let encoded = sealed.encode().expect("encode exact row");
        assert!(!encoded.contains("semantic-row-secret"));
        let debug = format!("{sealed:?}");
        assert!(!debug.contains("semantic-row-secret"));
        assert!(!debug.contains(&hex_encode(&cipher.key_id())));

        let parsed = GuardianEncryptedScrollbackRow::parse(&encoded).expect("parse exact row");
        let opened = cipher
            .open_scrollback_row(&parsed, [0x42; 16], [0x24; 16], -3, 11, 1024)
            .expect("authenticate exact row");
        assert_eq!(opened.as_slice(), plaintext);
    }

    #[test]
    fn encrypted_scrollback_row_rejects_tamper_wrong_location_and_wrong_key() {
        let cipher = cipher();
        let sealed = cipher
            .seal_scrollback_row(scrollback_identity(), b"authenticated row")
            .expect("seal exact row");
        let encoded = sealed.encode().expect("encode exact row");
        let parsed = GuardianEncryptedScrollbackRow::parse(&encoded).expect("parse exact row");
        assert!(matches!(
            cipher.open_scrollback_row(&parsed, [0x43; 16], [0x24; 16], -3, 11, 1024),
            Err(GuardianScrollbackRowError::StorageIdentityMismatch)
        ));
        assert!(matches!(
            cipher.open_scrollback_row(&parsed, [0x42; 16], [0x24; 16], -2, 11, 1024),
            Err(GuardianScrollbackRowError::StorageIdentityMismatch)
        ));
        let wrong_cipher = GuardianOutputCipher::try_from_key_slice(&[0x72; 32])
            .expect("wrong-key fixture is structurally valid");
        assert!(matches!(
            wrong_cipher.open_scrollback_row(&parsed, [0x42; 16], [0x24; 16], -3, 11, 1024,),
            Err(GuardianScrollbackRowError::KeyIdentityMismatch)
        ));

        let mut header_tampered =
            GuardianEncryptedScrollbackRow::parse(&encoded).expect("reparse exact row");
        header_tampered.identity.stable_row = -2;
        assert!(matches!(
            cipher.open_scrollback_row(&header_tampered, [0x42; 16], [0x24; 16], -2, 11, 1024,),
            Err(GuardianScrollbackRowError::DecryptionFailed)
        ));

        let mut tampered = parsed;
        tampered.ciphertext[0] ^= 0x80;
        assert!(matches!(
            cipher.open_scrollback_row(&tampered, [0x42; 16], [0x24; 16], -3, 11, 1024),
            Err(GuardianScrollbackRowError::DecryptionFailed)
        ));
    }

    #[test]
    fn encrypted_scrollback_row_enforces_bounded_canonical_framing() {
        let cipher = cipher();
        assert!(matches!(
            cipher.seal_scrollback_row(scrollback_identity(), b""),
            Err(GuardianScrollbackRowError::RecordByteLimit)
        ));
        let sealed = cipher
            .seal_scrollback_row(scrollback_identity(), b"bounded")
            .expect("seal bounded row");
        let encoded = sealed.encode().expect("encode bounded row");
        let parsed = GuardianEncryptedScrollbackRow::parse(&encoded).expect("parse bounded row");
        assert!(matches!(
            cipher.open_scrollback_row(&parsed, [0x42; 16], [0x24; 16], -3, 11, 6),
            Err(GuardianScrollbackRowError::RecordByteLimit)
        ));
        assert!(matches!(
            GuardianEncryptedScrollbackRow::parse(&format!("{encoded}=")),
            Err(GuardianScrollbackRowError::MalformedRecord)
                | Err(GuardianScrollbackRowError::NonCanonicalRecord)
        ));
    }

    #[test]
    fn scrollback_manifest_authentication_roundtrips_without_plaintext_or_debug_disclosure() {
        let canonical = br#"{"schema":"frankenterm.live-scrollback.manifest.v3","secret":"FT-MANIFEST-SECRET"}"#;
        let cipher = cipher();
        let authentication = cipher
            .authenticate_scrollback_manifest(canonical)
            .expect("authenticate canonical manifest");
        let encoded = authentication.encode();
        assert!(!encoded.contains("FT-MANIFEST-SECRET"));
        assert!(!format!("{authentication:?}").contains("FT-MANIFEST-SECRET"));
        assert!(!format!("{authentication:?}").contains(&hex_encode(&cipher.key_id())));

        let parsed = GuardianScrollbackManifestAuthentication::parse(&encoded)
            .expect("parse canonical authentication seal");
        assert_eq!(parsed.encode(), encoded);
        cipher
            .verify_scrollback_manifest(&parsed, canonical)
            .expect("verify canonical manifest");
    }

    #[test]
    fn scrollback_manifest_authentication_rejects_tamper_wrong_manifest_and_wrong_key() {
        let canonical = br#"{"schema":"frankenterm.live-scrollback.manifest.v3","revision":9}"#;
        let cipher = cipher();
        let mut authentication = cipher
            .authenticate_scrollback_manifest(canonical)
            .expect("authenticate canonical manifest");

        assert!(matches!(
            cipher.verify_scrollback_manifest(
                &authentication,
                br#"{"schema":"frankenterm.live-scrollback.manifest.v3","revision":8}"#,
            ),
            Err(GuardianScrollbackManifestError::AuthenticationFailed)
        ));
        assert!(matches!(
            cipher.verify_scrollback_manifest(&authentication, b"different-length-manifest"),
            Err(GuardianScrollbackManifestError::AuthenticationFailed)
        ));
        let wrong_cipher = GuardianOutputCipher::try_from_key_slice(&[0x72; 32])
            .expect("wrong-key fixture is structurally valid");
        assert!(matches!(
            wrong_cipher.verify_scrollback_manifest(&authentication, canonical),
            Err(GuardianScrollbackManifestError::KeyIdentityMismatch)
        ));

        authentication.authentication_tag[0] ^= 0x80;
        assert!(matches!(
            cipher.verify_scrollback_manifest(&authentication, canonical),
            Err(GuardianScrollbackManifestError::AuthenticationFailed)
        ));
    }

    #[test]
    fn scrollback_manifest_authentication_enforces_bounded_canonical_framing() {
        let cipher = cipher();
        assert!(matches!(
            cipher.authenticate_scrollback_manifest(b""),
            Err(GuardianScrollbackManifestError::CanonicalByteLimit)
        ));
        assert!(matches!(
            GuardianScrollbackManifestAuthentication::parse(&format!(
                "{SCROLLBACK_MANIFEST_AUTH_PREFIX}{}",
                "A".repeat(SCROLLBACK_MANIFEST_AUTH_ENCODED_BYTES + 1)
            )),
            Err(GuardianScrollbackManifestError::MalformedRecord)
        ));
        let canonical = b"bounded manifest";
        let encoded = cipher
            .authenticate_scrollback_manifest(canonical)
            .expect("authenticate bounded manifest")
            .encode();
        assert!(matches!(
            GuardianScrollbackManifestAuthentication::parse(&format!("{encoded}=")),
            Err(GuardianScrollbackManifestError::MalformedRecord)
        ));
    }

    #[test]
    fn scrollback_append_wal_authentication_roundtrips_without_content_disclosure() {
        let canonical = br#"{"schema":"frankenterm.live-scrollback-append-wal.v1","record_sha256":"FT-WAL-SECRET"}"#;
        let cipher = cipher();
        let authentication = cipher
            .authenticate_scrollback_append_wal(canonical)
            .expect("authenticate canonical append WAL");
        let encoded = authentication.encode();
        assert!(!encoded.contains("FT-WAL-SECRET"));
        let debug = format!("{authentication:?}");
        assert!(!debug.contains("FT-WAL-SECRET"));
        assert!(!debug.contains(&hex_encode(&cipher.key_id())));

        let parsed = GuardianScrollbackAppendWalAuthentication::parse(&encoded)
            .expect("parse canonical append WAL authentication");
        assert_eq!(parsed.encode(), encoded);
        cipher
            .verify_scrollback_append_wal(&parsed, canonical)
            .expect("verify canonical append WAL");
    }

    #[test]
    fn scrollback_append_wal_authentication_rejects_tamper_wrong_wal_and_wrong_key() {
        let canonical = br#"{"schema":"frankenterm.live-scrollback-append-wal.v1","revision":9}"#;
        let cipher = cipher();
        let mut authentication = cipher
            .authenticate_scrollback_append_wal(canonical)
            .expect("authenticate canonical append WAL");
        assert!(matches!(
            cipher.verify_scrollback_append_wal(
                &authentication,
                br#"{"schema":"frankenterm.live-scrollback-append-wal.v1","revision":8}"#,
            ),
            Err(GuardianScrollbackAppendWalError::AuthenticationFailed)
        ));
        assert!(matches!(
            cipher.verify_scrollback_append_wal(&authentication, b"different-length-append-wal"),
            Err(GuardianScrollbackAppendWalError::AuthenticationFailed)
        ));
        let wrong_cipher = GuardianOutputCipher::try_from_key_slice(&[0x73; 32])
            .expect("wrong append-WAL key is structurally valid");
        assert!(matches!(
            wrong_cipher.verify_scrollback_append_wal(&authentication, canonical),
            Err(GuardianScrollbackAppendWalError::KeyIdentityMismatch)
        ));
        authentication.authentication_tag[0] ^= 0x40;
        assert!(matches!(
            cipher.verify_scrollback_append_wal(&authentication, canonical),
            Err(GuardianScrollbackAppendWalError::AuthenticationFailed)
        ));
    }

    #[test]
    fn scrollback_append_wal_authentication_enforces_bounded_canonical_framing() {
        let cipher = cipher();
        assert!(matches!(
            cipher.authenticate_scrollback_append_wal(b""),
            Err(GuardianScrollbackAppendWalError::CanonicalByteLimit)
        ));
        assert!(matches!(
            GuardianScrollbackAppendWalAuthentication::parse(&format!(
                "{SCROLLBACK_APPEND_WAL_AUTH_PREFIX}{}",
                "A".repeat(SCROLLBACK_APPEND_WAL_AUTH_ENCODED_BYTES + 1)
            )),
            Err(GuardianScrollbackAppendWalError::MalformedRecord)
        ));
        let encoded = cipher
            .authenticate_scrollback_append_wal(b"bounded append WAL")
            .expect("authenticate bounded append WAL")
            .encode();
        assert!(matches!(
            GuardianScrollbackAppendWalAuthentication::parse(&format!("{encoded}=")),
            Err(GuardianScrollbackAppendWalError::MalformedRecord)
        ));
    }

    fn journal_bytes_for(identity: GuardianOutputSegmentIdentity, records: &[&[u8]]) -> Vec<u8> {
        let cipher = cipher();
        let mut bytes = encode_file_header(identity, &cipher)
            .expect("fixture file-header authentication succeeds")
            .to_vec();
        for (index, payload) in records.iter().enumerate() {
            let sequence = identity
                .first_sequence()
                .checked_add(u64::try_from(index).expect("fixture index fits u64"))
                .expect("fixture sequence fits u64");
            let payload_bytes = u32::try_from(payload.len()).expect("fixture payload fits u32");
            let (nonce, ciphertext) = cipher
                .seal(identity, sequence, payload_bytes, payload)
                .expect("fixture encryption succeeds");
            let ciphertext_bytes =
                u32::try_from(ciphertext.len()).expect("fixture ciphertext fits u32");
            let digest = record_digest(
                identity,
                sequence,
                payload_bytes,
                ciphertext_bytes,
                &nonce,
                &ciphertext,
            );
            bytes.extend_from_slice(&encode_record_header(
                sequence,
                payload_bytes,
                ciphertext_bytes,
                nonce,
                digest,
            ));
            bytes.extend_from_slice(&ciphertext);
        }
        bytes
    }

    fn journal_bytes(records: &[&[u8]]) -> Vec<u8> {
        journal_bytes_for(identity(), records)
    }

    #[cfg(unix)]
    fn real_journal_with_records(
        file_name: &str,
        payloads: &[&[u8]],
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        GuardianOutputJournal,
        Vec<GuardianOutputAppendReceipt>,
    ) {
        let directory = tempfile::tempdir().expect("create cursor journal directory");
        let path = directory.path().join(file_name);
        let parent = File::open(directory.path()).expect("open cursor journal parent");
        let mut journal = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize cursor journal");
        journal
            .sync_parent_directory_and_activate()
            .expect("activate cursor journal");
        let mut receipts = Vec::new();
        receipts
            .try_reserve_exact(payloads.len())
            .expect("reserve fixture receipts");
        for payload in payloads {
            receipts.push(
                journal
                    .append_and_sync(*payload)
                    .expect("append cursor fixture record"),
            );
        }
        (directory, path, journal, receipts)
    }

    #[test]
    fn successor_segment_requires_exact_contiguous_predecessor_chain() {
        let previous = GuardianOutputPredecessor::new(
            Uuid::from_bytes([0x11; 16]),
            10,
            [0x55; 32],
            1_024,
            4_096,
        )
        .expect("fixture predecessor is valid");
        let successor = GuardianOutputSegmentIdentity::new(
            pane(),
            Uuid::from_bytes([0x22; 16]),
            11,
            Some(previous),
        )
        .expect("contiguous successor is valid");
        let cipher = cipher();
        let bytes = journal_bytes_for(successor, &[]);
        let mut cursor = Cursor::new(bytes.clone());
        let scan = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            successor,
            &cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect("successor header must scan");
        assert_eq!(scan.next_sequence, Some(11));
        assert_eq!(scan.cumulative_plaintext_bytes, 1_024);

        let gap = GuardianOutputSegmentIdentity::new(
            pane(),
            Uuid::from_bytes([0x23; 16]),
            12,
            Some(previous),
        );
        assert!(matches!(
            gap,
            Err(GuardianOutputJournalError::InvalidSegmentIdentity(_))
        ));
    }

    #[test]
    fn successor_cumulative_plaintext_overflow_fails_closed() {
        let previous = GuardianOutputPredecessor::new(
            Uuid::from_bytes([0x31; 16]),
            7,
            [0x45; 32],
            u64::MAX,
            4_096,
        )
        .expect("maximal cumulative endpoint is a structurally valid predecessor");
        let successor = GuardianOutputSegmentIdentity::new(
            pane(),
            Uuid::from_bytes([0x32; 16]),
            8,
            Some(previous),
        )
        .expect("overflow successor identity is structurally valid");
        let bytes = journal_bytes_for(successor, &[b"cannot overflow pane lifetime endpoint"]);
        let mut cursor = Cursor::new(bytes.clone());
        let error = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            successor,
            &cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect_err("cumulative plaintext overflow must fail closed");
        assert!(matches!(
            error,
            GuardianOutputJournalError::ArithmeticOverflow
        ));
    }

    #[test]
    fn journal_limit_requires_room_for_one_nonempty_authenticated_frame() {
        let one_byte_short =
            FILE_HEADER_BYTES_U64 + RECORD_HEADER_BYTES_U64 + u64::from(AEAD_TAG_BYTES);
        let error = GuardianOutputJournalLimits {
            max_record_bytes: 1,
            max_log_bytes: one_byte_short,
            max_records: 1,
        }
        .validate()
        .expect_err("the byte limit must include plaintext and its AEAD tag");
        assert!(matches!(
            error,
            GuardianOutputJournalError::InvalidLimits(_)
        ));
    }

    #[test]
    fn v3_scan_rejects_ciphertext_sealed_under_a_legacy_record_domain() {
        let identity = identity();
        let cipher = cipher();
        let payload = b"format-bound guardian output";
        let plaintext_bytes =
            u32::try_from(payload.len()).expect("fixture payload length fits u32");
        let nonce = [0x5a; NONCE_BYTES];
        let mut legacy_aad = Vec::new();
        legacy_aad.extend_from_slice(b"frankenterm.guardian-output-aead.v1\0");
        legacy_aad.extend_from_slice(identity.durable_pane_id.as_bytes());
        legacy_aad.extend_from_slice(identity.segment_id.as_bytes());
        legacy_aad.extend_from_slice(&identity.first_sequence.to_le_bytes());
        legacy_aad.extend_from_slice(&plaintext_bytes.to_le_bytes());
        let ciphertext = cipher
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: payload,
                    aad: &legacy_aad,
                },
            )
            .expect("legacy-domain fixture encryption succeeds");
        let ciphertext_bytes =
            u32::try_from(ciphertext.len()).expect("fixture ciphertext length fits u32");
        let digest = record_digest(
            identity,
            identity.first_sequence,
            plaintext_bytes,
            ciphertext_bytes,
            &nonce,
            &ciphertext,
        );
        let record_header = encode_record_header(
            identity.first_sequence,
            plaintext_bytes,
            ciphertext_bytes,
            nonce,
            digest,
        );
        let mut bytes = encode_file_header(identity, &cipher)
            .expect("v3 file header authentication succeeds")
            .to_vec();
        bytes.extend_from_slice(&record_header);
        bytes.extend_from_slice(&ciphertext);

        let mut cursor = Cursor::new(bytes.clone());
        let error = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            identity,
            &cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect_err("v3 must reject a record sealed without its format-version AAD");
        assert!(matches!(
            error,
            GuardianOutputJournalError::DecryptionFailed
        ));
    }

    #[test]
    fn record_digest_rejects_cross_segment_transplant() {
        let source = identity();
        let target =
            GuardianOutputSegmentIdentity::new(pane(), Uuid::from_bytes([0x25; 16]), 1, None)
                .expect("target segment identity is valid");
        let mut bytes = journal_bytes_for(source, &[b"bound to source segment"]);
        let cipher = cipher();
        bytes[0..FILE_HEADER_BYTES].copy_from_slice(
            &encode_file_header(target, &cipher)
                .expect("target file-header authentication succeeds"),
        );
        let mut cursor = Cursor::new(bytes.clone());
        let error = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            target,
            &cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect_err("a record cannot be transplanted into another segment");
        assert!(matches!(
            error,
            GuardianOutputJournalError::RecordDigestMismatch { sequence: 1 }
        ));
    }

    #[test]
    fn complete_prefix_scans_with_exact_sequence_and_bounds() {
        let bytes = journal_bytes(&[b"alpha", b"\x1b[31mred"]);
        let cipher = cipher();
        let mut cursor = Cursor::new(bytes.clone());
        let scan = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            identity(),
            &cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect("complete journal must scan");
        assert_eq!(
            scan.committed_bytes,
            u64::try_from(bytes.len()).expect("fixture length fits u64")
        );
        assert_eq!(scan.record_count, 2);
        assert_eq!(scan.next_sequence, Some(3));
        assert_eq!(scan.tail, GuardianOutputJournalTail::Clean);
    }

    #[test]
    fn raw_terminal_plaintext_never_appears_in_segment_bytes() {
        let plaintext = b"FT-UNIQUE-RAW-TERMINAL-SECRET-7f10c9";
        let bytes = journal_bytes(&[plaintext]);
        assert!(!bytes
            .windows(plaintext.len())
            .any(|window| window == plaintext));
    }

    #[test]
    fn wrong_encryption_key_fails_before_record_recovery() {
        let bytes = journal_bytes(&[b"encrypted output"]);
        let wrong_cipher = GuardianOutputCipher::try_from_key_slice(&[0x72; 32])
            .expect("wrong-key fixture is structurally valid");
        let mut cursor = Cursor::new(bytes.clone());
        let error = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            identity(),
            &wrong_cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect_err("wrong key must fail closed");
        assert!(matches!(
            error,
            GuardianOutputJournalError::KeyIdentityMismatch
        ));
    }

    #[test]
    fn aead_rejects_tamper_even_if_unkeyed_record_digest_is_recomputed() {
        let identity = identity();
        let cipher = cipher();
        let mut bytes = journal_bytes_for(identity, &[b"authenticated output"]);
        let header_offset = FILE_HEADER_BYTES;
        let ciphertext_offset = header_offset + RECORD_HEADER_BYTES;
        bytes[ciphertext_offset] ^= 0x80;
        let sequence = read_u64(&bytes[header_offset + 8..header_offset + 16]);
        let plaintext_bytes = read_u32(&bytes[header_offset + 16..header_offset + 20]);
        let ciphertext_bytes = read_u32(&bytes[header_offset + 20..header_offset + 24]);
        let mut nonce = [0_u8; NONCE_BYTES];
        nonce.copy_from_slice(&bytes[header_offset + 32..header_offset + 56]);
        let digest = record_digest(
            identity,
            sequence,
            plaintext_bytes,
            ciphertext_bytes,
            &nonce,
            &bytes[ciphertext_offset..],
        );
        bytes[header_offset + 56..header_offset + 88].copy_from_slice(&digest);
        let mut cursor = Cursor::new(bytes.clone());
        let mut recovery = RecoveryCollector::new(
            1,
            GuardianOutputRecoveryLimits::new(2, 1024).expect("fixture recovery limits are valid"),
        )
        .expect("fixture recovery collector is valid");
        let error = scan_journal_with_recovery(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            identity,
            &cipher,
            GuardianOutputJournalLimits::default(),
            Some(&mut recovery),
        )
        .expect_err("AEAD authentication must reject recomputed outer digest");
        assert!(matches!(
            error,
            GuardianOutputJournalError::DecryptionFailed
        ));
        assert!(recovery.records.is_empty());
    }

    #[test]
    fn incomplete_final_payload_preserves_verified_prefix() {
        let mut bytes = journal_bytes(&[b"complete", b"torn-tail"]);
        bytes.truncate(bytes.len() - 3);
        let mut cursor = Cursor::new(bytes.clone());
        let cipher = cipher();
        let scan = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            identity(),
            &cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect("an incomplete final frame is uncommitted, not invented corruption");
        assert_eq!(scan.record_count, 1);
        assert_eq!(scan.next_sequence, Some(2));
        assert!(matches!(
            scan.tail,
            GuardianOutputJournalTail::Incomplete {
                trailing_bytes,
                ..
            } if trailing_bytes > RECORD_HEADER_BYTES_U64
        ));
    }

    #[test]
    fn complete_digest_corruption_fails_closed() {
        let mut bytes = journal_bytes(&[b"sensitive output"]);
        let final_byte = bytes.last_mut().expect("record has payload");
        *final_byte ^= 0x80;
        let mut cursor = Cursor::new(bytes.clone());
        let cipher = cipher();
        let error = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            identity(),
            &cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect_err("complete corrupt record must quarantine");
        assert!(matches!(
            error,
            GuardianOutputJournalError::RecordDigestMismatch { sequence: 1 }
        ));
    }

    #[test]
    fn sequence_gap_fails_before_accepting_payload() {
        let mut bytes = journal_bytes(&[b"first", b"second"]);
        let second_header =
            FILE_HEADER_BYTES + RECORD_HEADER_BYTES + b"first".len() + AEAD_TAG_BYTES_USIZE;
        bytes[second_header + 8..second_header + 16].copy_from_slice(&3_u64.to_le_bytes());
        let mut cursor = Cursor::new(bytes.clone());
        let cipher = cipher();
        let error = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            identity(),
            &cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect_err("sequence gaps must fail closed");
        assert!(matches!(
            error,
            GuardianOutputJournalError::SequenceMismatch {
                expected: 2,
                observed: 3,
                ..
            }
        ));
    }

    #[test]
    fn debug_receipt_omits_content_digest() {
        let receipt = GuardianOutputAppendReceipt {
            segment_id: identity().segment_id(),
            sequence: 9,
            payload_bytes: 4,
            cumulative_plaintext_bytes: 44,
            committed_log_bytes: 128,
            record_digest: [0xab; 32],
            plaintext_delivery_digest: [0xcd; 32],
        };
        let rendered = format!("{receipt:?}");
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("ab"));
        assert!(!rendered.contains("cd"));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_reader_rejects_empty_existing_evidence_without_initializing_it() {
        let directory = tempfile::tempdir().expect("create empty evidence directory");
        let path = directory.path().join("empty-evidence.ftgout");
        drop(create_journal_file(&path));

        let error = GuardianOutputJournalReader::open_existing(
            open_read_only_journal_file(&path),
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .err()
        .expect("empty existing evidence must not be initialized");
        assert!(matches!(
            error,
            GuardianOutputJournalError::TornFileHeader {
                expected: FILE_HEADER_BYTES,
                actual: 0,
            }
        ));
        assert!(std::fs::read(&path)
            .expect("read preserved empty evidence")
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn journal_typestates_reject_the_wrong_descriptor_access_modes() {
        let directory = tempfile::tempdir().expect("create descriptor-mode directory");
        let path = directory.path().join("descriptor-mode.ftgout");

        let reader_error = GuardianOutputJournalReader::open_existing(
            create_journal_file(&path),
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .err()
        .expect("a recovery reader must reject an O_RDWR descriptor");
        assert!(matches!(
            reader_error,
            GuardianOutputJournalError::RecoveryDescriptorNotReadOnly
        ));

        let create_error = GuardianOutputJournal::initialize_new_file(
            open_read_only_journal_file(&path),
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
            encode_file_header(identity(), &cipher()).expect("encode rejection-test header"),
        )
        .err()
        .expect("new append authority must reject an O_RDONLY descriptor");
        assert!(matches!(
            create_error,
            GuardianOutputJournalError::AppendDescriptorNotReadWrite
        ));

        let append_error = GuardianOutputJournal::open_existing_append_file(
            open_read_only_journal_file(&path),
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .err()
        .expect("existing append authority must reject an O_RDONLY descriptor");
        assert!(matches!(
            append_error,
            GuardianOutputJournalError::AppendDescriptorNotReadWrite
        ));
        assert!(std::fs::read(&path)
            .expect("read descriptor-mode evidence")
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn append_authority_is_exclusive_for_the_full_journal_lifetime() {
        let directory = tempfile::tempdir().expect("create writer-lease directory");
        let path = directory.path().join("writer-lease.ftgout");
        let parent = File::open(directory.path()).expect("open writer-lease parent");
        let mut first = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("create first exclusive append authority");
        first
            .sync_parent_directory_and_activate()
            .expect("activate first exclusive append authority");
        let first_receipt = first
            .append_and_sync(b"first exclusive record")
            .expect("append through first exclusive authority");
        drop(first);

        let preopened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("preopen a caller-controlled read-write descriptor");
        let precloned = preopened
            .try_clone()
            .expect("preclone the caller-controlled open file description");
        let first_reopened = open_test_journal_for_append(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("descriptor-relative reopen acquires independent append authority");

        let second_error = open_test_journal_for_append(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .err()
        .expect("a second append authority must fail closed");
        assert!(matches!(
            second_error,
            GuardianOutputJournalError::AppendWriterLeaseUnavailable(_)
        ));

        drop(precloned);
        drop(preopened);
        drop(first_reopened);
        let mut successor = open_test_journal_for_append(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("acquire append authority only after predecessor drops");
        let successor_receipt = successor
            .append_and_sync(b"successor exclusive record")
            .expect("append through successor authority");
        assert_eq!(successor_receipt.sequence(), first_receipt.sequence() + 1);
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_prefix_digest_binds_a_nonterminal_valid_fork() {
        let directory = tempfile::tempdir().expect("create prefix-digest directory");
        let original_path = directory.path().join("prefix-original.ftgout");
        let alternate_path = directory.path().join("prefix-alternate.ftgout");
        let fork_path = directory.path().join("prefix-fork.ftgout");
        let parent = File::open(directory.path()).expect("open prefix-digest parent");
        let limits = GuardianOutputJournalLimits::default();
        let segment_identity = identity();
        let segment_cipher = cipher();

        let mut original = create_new_test_journal(
            &parent,
            &original_path,
            segment_identity,
            segment_cipher.clone(),
            limits,
        )
        .expect("create original prefix journal");
        original
            .sync_parent_directory_and_activate()
            .expect("activate original prefix journal");
        let original_first = original
            .append_and_sync(b"primary-prefix")
            .expect("append original nonterminal record");
        let original_terminal = original
            .append_and_sync(b"shared-terminal")
            .expect("append original terminal record");
        let original_prefix_digest = original.authenticated_prefix_digest();
        drop(original);

        let mut alternate = create_new_test_journal(
            &parent,
            &alternate_path,
            segment_identity,
            segment_cipher.clone(),
            limits,
        )
        .expect("create alternate prefix journal");
        alternate
            .sync_parent_directory_and_activate()
            .expect("activate alternate prefix journal");
        let alternate_first = alternate
            .append_and_sync(b"adverse-prefix")
            .expect("append alternate nonterminal record");
        assert_eq!(
            alternate_first.committed_log_bytes(),
            original_first.committed_log_bytes(),
            "fork fixture records must have identical geometry"
        );
        drop(alternate);

        let original_bytes = std::fs::read(&original_path).expect("read original journal bytes");
        let alternate_bytes = std::fs::read(&alternate_path).expect("read alternate journal bytes");
        let first_end = usize::try_from(original_first.committed_log_bytes())
            .expect("first record endpoint fits usize");
        let mut fork_bytes = Vec::with_capacity(original_bytes.len());
        fork_bytes.extend_from_slice(&original_bytes[..FILE_HEADER_BYTES]);
        fork_bytes.extend_from_slice(&alternate_bytes[FILE_HEADER_BYTES..first_end]);
        fork_bytes.extend_from_slice(&original_bytes[first_end..]);
        let mut fork_file = create_journal_file(&fork_path);
        fork_file
            .write_all(&fork_bytes)
            .and_then(|()| fork_file.sync_all())
            .expect("persist valid nonterminal fork fixture");
        drop(fork_file);

        let fork = GuardianOutputJournalReader::open_existing(
            open_read_only_journal_file(&fork_path),
            segment_identity,
            segment_cipher,
            limits,
        )
        .expect("authenticate the fully valid nonterminal fork");
        assert_eq!(fork.record_count(), 2);
        assert_eq!(fork.terminal_receipt(), Some(original_terminal));
        assert_ne!(fork.authenticated_prefix_digest(), original_prefix_digest);
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_creation_rejects_preexisting_files_without_rewrite() {
        let directory = tempfile::tempdir().expect("create preexisting-file directory");
        let parent = File::open(directory.path()).expect("open preexisting-file parent");
        let empty_path = directory.path().join("preexisting-empty.ftgout");
        drop(create_journal_file(&empty_path));
        let empty_error = create_new_test_journal(
            &parent,
            &empty_path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .err()
        .expect("exclusive creation must reject a preexisting empty file");
        assert!(matches!(
            empty_error,
            GuardianOutputJournalError::Io(ref source)
                if source.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert!(std::fs::read(&empty_path)
            .expect("read retained empty evidence")
            .is_empty());

        let path = directory.path().join("preexisting-nonempty.ftgout");
        let retained = b"retained preexisting evidence";
        let mut file = create_journal_file(&path);
        file.write_all(retained)
            .and_then(|()| file.sync_all())
            .expect("persist nonempty new-segment evidence");
        drop(file);

        let error = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .err()
        .expect("exclusive creation must reject a preexisting nonempty file");
        assert!(matches!(
            error,
            GuardianOutputJournalError::Io(ref source)
                if source.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(
            std::fs::read(&path).expect("read retained nonempty evidence"),
            retained
        );
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_creation_rejects_invalid_names_and_limits_before_creation() {
        let directory = tempfile::tempdir().expect("create invalid-creation directory");
        let parent = File::open(directory.path()).expect("open invalid-creation parent");
        rustix::fs::fchmod(&parent, rustix::fs::Mode::from_raw_mode(0o700))
            .expect("make invalid-creation parent private");
        for child_name in ["", ".", "..", "../escape", "nested/child", "/absolute"] {
            let error = GuardianOutputJournal::create_new_at(
                &parent,
                OsStr::new(child_name),
                identity(),
                cipher(),
                GuardianOutputJournalLimits::default(),
            )
            .err()
            .expect("invalid child name must fail before creation");
            assert!(matches!(
                error,
                GuardianOutputJournalError::InvalidNewSegmentName
            ));
        }
        let invalid_limits = GuardianOutputJournalLimits {
            max_record_bytes: 0,
            ..GuardianOutputJournalLimits::default()
        };
        let error = GuardianOutputJournal::create_new_at(
            &parent,
            OsStr::new("invalid-limits.ftgout"),
            identity(),
            cipher(),
            invalid_limits,
        )
        .err()
        .expect("invalid limits must fail before creation");
        assert!(matches!(
            error,
            GuardianOutputJournalError::InvalidLimits(_)
        ));
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("read invalid-creation directory")
                .count(),
            0,
            "a rejected child name or deterministic limit error created an artifact"
        );
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_creation_never_follows_or_rewrites_links() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().expect("create link-creation directory");
        let parent = File::open(directory.path()).expect("open link-creation parent");
        let target_path = directory.path().join("retained-target");
        let symlink_path = directory.path().join("occupied-symlink.ftgout");
        let hardlink_path = directory.path().join("occupied-hardlink.ftgout");
        let retained = b"retained link target evidence";
        let mut target = create_journal_file(&target_path);
        target
            .write_all(retained)
            .and_then(|()| target.sync_all())
            .expect("persist link target evidence");
        drop(target);
        symlink(&target_path, &symlink_path).expect("create occupied symlink child");
        std::fs::hard_link(&target_path, &hardlink_path).expect("create occupied hardlink child");

        for path in [&symlink_path, &hardlink_path] {
            let error = create_new_test_journal(
                &parent,
                path,
                identity(),
                cipher(),
                GuardianOutputJournalLimits::default(),
            )
            .err()
            .expect("exclusive no-follow creation must reject an occupied link");
            assert!(matches!(
                error,
                GuardianOutputJournalError::Io(ref source)
                    if source.kind() == std::io::ErrorKind::AlreadyExists
            ));
        }
        assert_eq!(
            std::fs::read(&target_path).expect("read retained link target"),
            retained
        );
        assert_eq!(
            std::fs::read_link(&symlink_path).expect("read retained symlink target"),
            target_path
        );
        assert_eq!(
            std::fs::read(&hardlink_path).expect("read retained hardlink evidence"),
            retained
        );
    }

    #[cfg(unix)]
    #[test]
    fn new_segment_creation_enforces_private_cloexec_single_link_authority() {
        let directory = tempfile::tempdir().expect("create creation-invariant directory");
        let path = directory.path().join("creation-invariants.ftgout");
        let parent = File::open(directory.path()).expect("open creation-invariant parent");
        let mut journal = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("create invariant-bound journal");
        let metadata = journal
            .file
            .metadata()
            .expect("inspect invariant-bound descriptor");
        assert!(metadata.is_file());
        assert_eq!(metadata.uid(), nix::unistd::geteuid().as_raw());
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.len(), FILE_HEADER_BYTES_U64);
        let descriptor_flags = nix::fcntl::fcntl(&journal.file, nix::fcntl::F_GETFD)
            .expect("inspect invariant-bound descriptor flags");
        assert!(nix::fcntl::FdFlag::from_bits_truncate(descriptor_flags)
            .contains(nix::fcntl::FdFlag::FD_CLOEXEC));
        journal
            .sync_parent_directory_and_activate()
            .expect("activate invariant-bound journal");
        assert!(!journal.directory_entry_sync_required());
    }

    #[cfg(unix)]
    #[test]
    fn activation_rejects_child_replacement_hardlink_and_same_inode_mutation() {
        let replacement_directory =
            tempfile::tempdir().expect("create replacement-activation directory");
        let replacement_path = replacement_directory.path().join("replacement.ftgout");
        let displaced_path = replacement_directory.path().join("displaced.ftgout");
        let replacement_parent =
            File::open(replacement_directory.path()).expect("open replacement parent");
        let mut replaced = create_new_test_journal(
            &replacement_parent,
            &replacement_path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("create replacement-race journal");
        std::fs::rename(&replacement_path, &displaced_path)
            .expect("displace the exact created child");
        drop(create_journal_file(&replacement_path));
        assert!(matches!(
            replaced.sync_parent_directory_and_activate(),
            Err(GuardianOutputJournalError::NewSegmentPublicationIdentityChanged)
        ));
        assert!(matches!(
            replaced.append_and_sync(b"must remain fenced after replacement"),
            Err(GuardianOutputJournalError::DirectoryEntryNotDurable)
        ));

        let hardlink_directory = tempfile::tempdir().expect("create hardlink-activation directory");
        let hardlink_path = hardlink_directory.path().join("hardlink.ftgout");
        let alias_path = hardlink_directory.path().join("hardlink-alias.ftgout");
        let hardlink_parent = File::open(hardlink_directory.path()).expect("open hardlink parent");
        let mut hardlinked = create_new_test_journal(
            &hardlink_parent,
            &hardlink_path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("create hardlink-race journal");
        std::fs::hard_link(&hardlink_path, &alias_path).expect("plant post-create hardlink alias");
        assert!(matches!(
            hardlinked.sync_parent_directory_and_activate(),
            Err(GuardianOutputJournalError::InsecureNewSegmentIdentity)
        ));
        assert!(matches!(
            hardlinked.append_and_sync(b"must remain fenced after hardlink"),
            Err(GuardianOutputJournalError::DirectoryEntryNotDurable)
        ));

        let mutation_directory = tempfile::tempdir().expect("create mutation-activation directory");
        let mutation_path = mutation_directory.path().join("mutation.ftgout");
        let mutation_parent = File::open(mutation_directory.path()).expect("open mutation parent");
        let mut mutated = create_new_test_journal(
            &mutation_parent,
            &mutation_path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("create mutation-race journal");
        let mut external = std::fs::OpenOptions::new()
            .append(true)
            .open(&mutation_path)
            .expect("open same inode for external append");
        external
            .write_all(&[0x5a])
            .and_then(|()| external.sync_all())
            .expect("persist same-inode length mutation");
        drop(external);
        assert!(mutated.sync_parent_directory_and_activate().is_err());
        assert!(matches!(
            mutated.append_and_sync(b"must remain fenced after mutation"),
            Err(GuardianOutputJournalError::DirectoryEntryNotDurable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn activation_rejects_parent_permission_drift_and_same_length_content_mutation() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent_directory = tempfile::tempdir().expect("create parent-drift directory");
        let parent_path = parent_directory.path().join("parent-drift.ftgout");
        let parent = File::open(parent_directory.path()).expect("open parent-drift parent");
        let mut parent_drifted = create_new_test_journal(
            &parent,
            &parent_path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("create parent-drift journal");
        std::fs::set_permissions(
            parent_directory.path(),
            std::fs::Permissions::from_mode(0o750),
        )
        .expect("plant parent permission drift");
        assert!(matches!(
            parent_drifted.sync_parent_directory_and_activate(),
            Err(GuardianOutputJournalError::InsecureNewSegmentParent)
        ));
        std::fs::set_permissions(
            parent_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("restore private parent permission for retained fixture");
        assert!(matches!(
            parent_drifted.append_and_sync(b"must remain fenced after parent drift"),
            Err(GuardianOutputJournalError::DirectoryEntryNotDurable)
        ));

        let content_directory = tempfile::tempdir().expect("create content-drift directory");
        let content_path = content_directory.path().join("content-drift.ftgout");
        let content_parent =
            File::open(content_directory.path()).expect("open content-drift parent");
        let mut content_drifted = create_new_test_journal(
            &content_parent,
            &content_path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("create content-drift journal");
        let mut bytes = std::fs::read(&content_path).expect("read created header");
        bytes[FILE_HEADER_BYTES - 1] ^= 0x80;
        let mut external = std::fs::OpenOptions::new()
            .write(true)
            .open(&content_path)
            .expect("open created header for same-length mutation");
        external
            .write_all(&bytes)
            .and_then(|()| external.sync_all())
            .expect("persist same-length header mutation");
        drop(external);
        assert!(matches!(
            content_drifted.sync_parent_directory_and_activate(),
            Err(GuardianOutputJournalError::FileHeaderAuthenticationFailed)
        ));
        assert!(matches!(
            content_drifted.append_and_sync(b"must remain fenced after content drift"),
            Err(GuardianOutputJournalError::DirectoryEntryNotDurable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_reader_replays_committed_prefix_and_preserves_torn_tail() {
        let directory = tempfile::tempdir().expect("create read-only torn-tail directory");
        let path = directory.path().join("read-only-torn-tail.ftgout");
        let parent = File::open(directory.path()).expect("open read-only torn-tail parent");
        let payload = b"authenticated committed prefix";
        let mut journal = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("create read-only torn-tail journal");
        journal
            .sync_parent_directory_and_activate()
            .expect("activate read-only torn-tail journal");
        let receipt = journal
            .append_and_sync(payload)
            .expect("append committed prefix");
        drop(journal);

        let mut tail_writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open torn-tail fixture writer");
        tail_writer
            .write_all(&RECORD_MAGIC[..3])
            .and_then(|()| tail_writer.sync_all())
            .expect("persist incomplete final frame");
        drop(tail_writer);
        let exact_evidence = std::fs::read(&path).expect("snapshot torn-tail evidence");

        let reader = GuardianOutputJournalReader::open_existing(
            open_read_only_journal_file(&path),
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("authenticate committed prefix through a read-only descriptor");
        assert_eq!(reader.identity(), identity());
        assert_eq!(reader.record_count(), 1);
        assert_eq!(reader.terminal_receipt(), Some(receipt));
        assert!(matches!(
            reader.tail(),
            GuardianOutputJournalTail::Incomplete {
                committed_bytes,
                trailing_bytes: 3,
            } if committed_bytes == receipt.committed_log_bytes()
        ));

        let page = reader
            .recover_committed_range(
                1,
                GuardianOutputRecoveryLimits::new(1, 1024)
                    .expect("valid read-only recovery limits"),
            )
            .expect("recover only the authenticated committed prefix");
        assert_eq!(page.records().len(), 1);
        assert_eq!(page.records()[0].receipt(), receipt);
        assert_eq!(page.records()[0].plaintext(), payload);
        assert!(matches!(
            page.tail(),
            GuardianOutputJournalTail::Incomplete {
                trailing_bytes: 3,
                ..
            }
        ));

        let mut cursor = reader
            .recovery_cursor(1, 1024)
            .expect("create read-only recovery cursor");
        assert_eq!(
            cursor
                .next_record()
                .expect("authenticate committed cursor record")
                .expect("committed cursor record exists")
                .receipt(),
            receipt
        );
        assert!(cursor
            .next_record()
            .expect("cursor excludes incomplete tail")
            .is_none());
        assert_eq!(
            std::fs::read(&path).expect("re-read preserved torn-tail evidence"),
            exact_evidence
        );
    }

    #[cfg(unix)]
    #[test]
    fn truncate_between_reader_and_append_reopen_never_initializes_evidence() {
        let directory = tempfile::tempdir().expect("create reader-append-race directory");
        let path = directory.path().join("reader-append-race.ftgout");
        let parent = File::open(directory.path()).expect("open reader-append-race parent");
        let mut journal = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("create reader-append-race journal");
        journal
            .sync_parent_directory_and_activate()
            .expect("activate reader-append-race journal");
        let receipt = journal
            .append_and_sync(b"committed before append reopen")
            .expect("append reader-append-race record");
        drop(journal);

        let reader = GuardianOutputJournalReader::open_existing(
            open_read_only_journal_file(&path),
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("authenticate the pre-race segment");
        assert_eq!(reader.terminal_receipt(), Some(receipt));

        let truncated = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("truncate the segment between reader and append reopen");
        truncated
            .sync_all()
            .expect("synchronize the simulated crash truncation");
        drop(truncated);

        let error = open_test_journal_for_append(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .err()
        .expect("append recovery must reject truncated existing evidence");
        assert!(matches!(
            error,
            GuardianOutputJournalError::TornFileHeader {
                expected: FILE_HEADER_BYTES,
                actual: 0,
            }
        ));
        assert!(std::fs::read(&path)
            .expect("read crash-truncated evidence after append reopen")
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn complete_legacy_header_is_rejected_as_unsupported_without_rewrite() {
        let directory = tempfile::tempdir().expect("create legacy journal directory");
        let path = directory.path().join("legacy-segment.ftgout");
        let mut legacy_header = [0_u8; 160];
        legacy_header[0..8].copy_from_slice(&FILE_MAGIC);
        legacy_header[8..12].copy_from_slice(&1_u32.to_le_bytes());
        legacy_header[12..16].copy_from_slice(&160_u32.to_le_bytes());
        let mut file = create_journal_file(&path);
        file.write_all(&legacy_header)
            .expect("write complete legacy header fixture");
        file.sync_all().expect("sync legacy header fixture");

        drop(file);
        let parent = File::open(directory.path()).expect("open legacy fixture parent");
        let error = open_test_journal_for_append(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .err()
        .expect("legacy guardian output journals must never be silently upgraded");
        assert!(matches!(
            error,
            GuardianOutputJournalError::UnsupportedVersion { observed: 1 }
        ));
        assert_eq!(
            std::fs::read(&path).expect("read preserved legacy journal"),
            legacy_header
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_file_creation_activation_append_and_reopen_are_contiguous() {
        let directory = tempfile::tempdir().expect("create journal directory");
        let path = directory.path().join("segment.ftgout");
        let parent = File::open(directory.path()).expect("open parent directory");
        let payload = b"FT-REAL-FILE-PLAINTEXT-MUST-NOT-APPEAR";

        let mut journal = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize journal");
        assert!(journal.directory_entry_sync_required());
        assert!(matches!(
            journal.append_and_sync(payload),
            Err(GuardianOutputJournalError::DirectoryEntryNotDurable)
        ));
        journal
            .sync_parent_directory_and_activate()
            .expect("durably activate journal");
        let first = journal
            .append_and_sync(payload)
            .expect("append first synchronized record");
        assert_eq!(first.sequence(), 1);
        assert!(first.matches_payload(payload));
        assert!(!first.matches_payload(b"FT-REAL-FILE-PLAINTEXT-MUST-NOT-APPEAS"));
        assert_eq!(
            first.cumulative_plaintext_bytes(),
            u64::try_from(payload.len()).expect("fixture payload length fits u64")
        );
        let committed_after_first = first.committed_log_bytes();
        drop(journal);

        let bytes = std::fs::read(&path).expect("read encrypted journal bytes");
        assert_eq!(
            u64::try_from(bytes.len()).expect("journal length fits u64"),
            committed_after_first
        );
        assert!(!bytes.windows(payload.len()).any(|window| window == payload));

        let mut reopened = open_test_journal_for_append(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("validate reopened journal");
        assert!(!reopened.directory_entry_sync_required());
        assert_eq!(reopened.record_count(), 1);
        assert_eq!(reopened.next_sequence(), Some(2));
        assert_eq!(reopened.terminal_receipt(), Some(first));
        assert_eq!(
            reopened.cumulative_plaintext_bytes(),
            u64::try_from(payload.len()).expect("fixture payload length fits u64")
        );
        let second = reopened
            .append_and_sync(b"second")
            .expect("append contiguous record after reopen");
        assert_eq!(second.sequence(), 2);
        assert_eq!(
            second.cumulative_plaintext_bytes(),
            u64::try_from(payload.len() + b"second".len())
                .expect("combined fixture payload length fits u64")
        );
        assert!(second.committed_log_bytes() > committed_after_first);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_cursor_is_linear_ordered_and_never_repeats_after_loss_or_eof() {
        let payloads: [&[u8]; 3] = [
            b"FT-CURSOR-FIRST-SECRET",
            b"FT-CURSOR-SECOND-SECRET",
            b"FT-CURSOR-THIRD-SECRET",
        ];
        let (_directory, _path, journal, receipts) =
            real_journal_with_records("linear-cursor.ftgout", &payloads);

        let mut cursor = journal
            .recovery_cursor(1, 1024)
            .expect("create bounded recovery cursor");
        assert_eq!(cursor.verified_record_count(), 0);

        let lost_first = cursor
            .next_record()
            .expect("authenticate first cursor record")
            .expect("first cursor record exists");
        assert_eq!(lost_first.receipt(), receipts[0]);
        assert_eq!(lost_first.plaintext(), payloads[0]);
        assert_eq!(cursor.verified_record_count(), 1);
        drop(lost_first);

        let second = cursor
            .next_record()
            .expect("authenticate second cursor record")
            .expect("second cursor record exists");
        assert_eq!(second.receipt(), receipts[1]);
        assert_eq!(second.plaintext(), payloads[1]);
        assert_eq!(cursor.verified_record_count(), 2);
        let delivery = second
            .into_authenticated_delivery()
            .expect("promote bound second record to delivery capability");
        assert_eq!(delivery.receipt(), receipts[1]);
        let delivery_debug = format!("{delivery:?}");
        assert!(!delivery_debug.contains("FT-CURSOR-SECOND-SECRET"));
        let mut encoded = Vec::new();
        let exact_delivery_bound =
            u32::try_from(payloads[1].len()).expect("fixture payload length fits u32");
        let delivered_receipt = delivery
            .write_all_bounded(&mut encoded, exact_delivery_bound)
            .expect("write the complete bounded authenticated payload");
        assert_eq!(delivered_receipt, receipts[1]);
        assert_eq!(encoded.as_slice(), payloads[1]);

        let third = cursor
            .next_record()
            .expect("authenticate third cursor record")
            .expect("third cursor record exists");
        assert_eq!(third.receipt(), receipts[2]);
        assert_eq!(third.plaintext(), payloads[2]);
        assert_eq!(cursor.verified_record_count(), 3);
        drop(third);

        assert!(cursor
            .next_record()
            .expect("finish exact cursor authority")
            .is_none());
        assert!(cursor.is_exhausted());
        assert_eq!(cursor.verified_record_count(), 3);
        assert!(cursor
            .next_record()
            .expect("repeated EOF is stable")
            .is_none());
        assert_eq!(cursor.verified_record_count(), 3);

        let mut explicit_replay = journal
            .recovery_cursor(2, 1024)
            .expect("create an explicit fresh cursor at sequence two");
        let replayed_second = explicit_replay
            .next_record()
            .expect("authenticate skipped prefix and requested record")
            .expect("explicitly requested record exists");
        assert_eq!(replayed_second.receipt(), receipts[1]);
        assert_eq!(replayed_second.plaintext(), payloads[1]);
        assert_eq!(explicit_replay.verified_record_count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_cursor_is_pinned_to_its_frozen_prefix_while_append_authority_advances() {
        let (_directory, _path, mut journal, receipts) =
            real_journal_with_records("frozen-cursor.ftgout", &[b"frozen-first", b"frozen-second"]);
        let mut cursor = journal
            .recovery_cursor(1, 1024)
            .expect("create frozen-prefix cursor");
        assert_eq!(
            cursor
                .next_record()
                .expect("authenticate frozen first record")
                .expect("frozen first record exists")
                .receipt(),
            receipts[0]
        );

        let appended_after_cursor = journal
            .append_and_sync(b"appended-after-cursor")
            .expect("append authority remains independent of positional cursor reads");
        assert_eq!(appended_after_cursor.sequence(), 3);
        assert_eq!(
            cursor
                .next_record()
                .expect("authenticate frozen second record")
                .expect("frozen second record exists")
                .receipt(),
            receipts[1]
        );
        assert!(cursor
            .next_record()
            .expect("frozen cursor ignores later append")
            .is_none());
        assert_eq!(cursor.verified_record_count(), 2);

        let mut later_cursor = journal
            .recovery_cursor(3, 1024)
            .expect("new cursor sees the advanced append authority");
        assert_eq!(
            later_cursor
                .next_record()
                .expect("authenticate advanced cursor record")
                .expect("advanced cursor record exists")
                .receipt(),
            appended_after_cursor
        );
        assert_eq!(later_cursor.verified_record_count(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_cursor_and_delivery_enforce_bounds_binding_and_zeroization() {
        let payload: &[u8] = b"FT-CURSOR-ZEROIZE-SECRET";
        let (_directory, _path, journal, _receipts) =
            real_journal_with_records("bounded-cursor.ftgout", &[payload]);

        assert!(matches!(
            journal.recovery_cursor(1, 0),
            Err(GuardianOutputJournalError::InvalidRecoveryLimits(_))
        ));
        let above_hard_cap = u32::try_from(RECOVERY_MAX_PLAINTEXT_BYTES + 1)
            .expect("hard recovery cap plus one fits u32");
        assert!(matches!(
            journal.recovery_cursor(1, above_hard_cap),
            Err(GuardianOutputJournalError::InvalidRecoveryLimits(_))
        ));

        let too_small = u32::try_from(payload.len() - 1).expect("fixture bound fits u32");
        let mut bounded_cursor = journal
            .recovery_cursor(1, too_small)
            .expect("create cursor with a deliberately small record bound");
        assert!(matches!(
            bounded_cursor.next_record(),
            Err(GuardianOutputJournalError::RecoveryPlaintextByteLimit {
                observed,
                maximum,
            }) if observed == u64::try_from(payload.len()).expect("fixture length fits u64")
                && maximum == u64::from(too_small)
        ));
        assert_eq!(bounded_cursor.verified_record_count(), 0);
        assert!(matches!(
            bounded_cursor.next_record(),
            Err(GuardianOutputJournalError::RecoveryCursorFailed)
        ));
        assert_eq!(bounded_cursor.verified_record_count(), 0);

        let mut forged_cursor = journal
            .recovery_cursor(1, 1024)
            .expect("create cursor for binding mutation plant");
        let mut forged = forged_cursor
            .next_record()
            .expect("authenticate binding mutation record")
            .expect("binding mutation record exists");
        forged.plaintext[0] ^= 0x01;
        assert!(matches!(
            forged.into_authenticated_delivery(),
            Err(GuardianOutputJournalError::RecoveryPayloadBindingMismatch)
        ));

        let mut zeroize_cursor = journal
            .recovery_cursor(1, 1024)
            .expect("create cursor for zeroization plant");
        let record = zeroize_cursor
            .next_record()
            .expect("authenticate zeroization record")
            .expect("zeroization record exists");
        let mut delivery = record
            .into_authenticated_delivery()
            .expect("promote zeroization record");
        fn require_zeroizing_payload(_: &Zeroizing<Vec<u8>>) {}
        require_zeroizing_payload(&delivery.plaintext);
        assert!(!format!("{delivery:?}").contains("FT-CURSOR-ZEROIZE-SECRET"));
        delivery.plaintext.zeroize();
        assert!(delivery.plaintext.iter().all(|byte| *byte == 0));

        let mut delivery_bound_cursor = journal
            .recovery_cursor(1, 1024)
            .expect("create cursor for delivery bound plant");
        let delivery = delivery_bound_cursor
            .next_record()
            .expect("authenticate delivery bound record")
            .expect("delivery bound record exists")
            .into_authenticated_delivery()
            .expect("promote delivery bound record");
        let mut writer = Vec::new();
        assert!(matches!(
            delivery.write_all_bounded(&mut writer, too_small),
            Err(GuardianOutputJournalError::DeliveryPayloadByteLimit {
                observed,
                maximum,
            }) if observed == u64::try_from(payload.len()).expect("fixture length fits u64")
                && maximum == too_small
        ));
        assert!(writer.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_cursor_rejects_sequence_gap_and_same_length_aead_tamper() {
        let (_gap_directory, gap_path, gap_journal, gap_receipts) =
            real_journal_with_records("cursor-gap.ftgout", &[b"first", b"second"]);
        let second_header = gap_receipts[0]
            .committed_log_bytes()
            .checked_add(8)
            .expect("fixture second sequence offset fits u64");
        let mut gap_writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&gap_path)
            .expect("open cursor gap journal for mutation");
        gap_writer
            .seek(SeekFrom::Start(second_header))
            .and_then(|_| gap_writer.write_all(&3_u64.to_le_bytes()))
            .and_then(|_| gap_writer.sync_all())
            .expect("persist cursor sequence gap");
        drop(gap_writer);

        let mut gap_cursor = gap_journal
            .recovery_cursor(1, 1024)
            .expect("create cursor over same-length sequence gap");
        assert_eq!(
            gap_cursor
                .next_record()
                .expect("first record before gap authenticates")
                .expect("first record before gap exists")
                .receipt(),
            gap_receipts[0]
        );
        assert!(matches!(
            gap_cursor.next_record(),
            Err(GuardianOutputJournalError::SequenceMismatch {
                expected: 2,
                observed: 3,
                ..
            })
        ));

        let (_tamper_directory, tamper_path, tamper_journal, _tamper_receipts) =
            real_journal_with_records("cursor-tamper.ftgout", &[b"authenticated cursor data"]);
        let segment_identity = identity();
        let mut bytes = std::fs::read(&tamper_path).expect("read cursor tamper journal");
        let header_offset = FILE_HEADER_BYTES;
        let ciphertext_offset = header_offset + RECORD_HEADER_BYTES;
        bytes[ciphertext_offset] ^= 0x80;
        let sequence = read_u64(&bytes[header_offset + 8..header_offset + 16]);
        let plaintext_bytes = read_u32(&bytes[header_offset + 16..header_offset + 20]);
        let ciphertext_bytes = read_u32(&bytes[header_offset + 20..header_offset + 24]);
        let mut nonce = [0_u8; NONCE_BYTES];
        nonce.copy_from_slice(&bytes[header_offset + 32..header_offset + 56]);
        let digest = record_digest(
            segment_identity,
            sequence,
            plaintext_bytes,
            ciphertext_bytes,
            &nonce,
            &bytes[ciphertext_offset..],
        );
        bytes[header_offset + 56..header_offset + 88].copy_from_slice(&digest);
        let mut tamper_writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&tamper_path)
            .expect("open cursor tamper journal for mutation");
        tamper_writer
            .seek(SeekFrom::Start(0))
            .and_then(|_| tamper_writer.write_all(&bytes))
            .and_then(|_| tamper_writer.sync_all())
            .expect("persist same-length cursor ciphertext tamper");
        drop(tamper_writer);

        let mut tamper_cursor = tamper_journal
            .recovery_cursor(1, 1024)
            .expect("create cursor over same-length ciphertext tamper");
        assert!(matches!(
            tamper_cursor.next_record(),
            Err(GuardianOutputJournalError::DecryptionFailed)
        ));
        assert_eq!(tamper_cursor.verified_record_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_header_hint_selects_only_a_candidate_key_and_never_authorizes_open() {
        let (directory, path, journal, _receipts) =
            real_journal_with_records("untrusted-hint.ftgout", &[b"hint payload"]);
        let hint = GuardianOutputUntrustedHeaderHint::read_from(&journal.file)
            .expect("read structurally valid untrusted header hint");
        assert_eq!(hint.untrusted_key_id(), cipher().key_id());
        let hint_debug = format!("{hint:?}");
        assert!(hint_debug.contains("UNTRUSTED"));
        assert!(!hint_debug.contains(&hex_encode(&cipher().key_id())));
        drop(journal);

        let mut bytes = std::fs::read(&path).expect("read hint mutation journal");
        bytes[112] ^= 0x80;
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open hint mutation journal");
        writer
            .seek(SeekFrom::Start(0))
            .and_then(|_| writer.write_all(&bytes))
            .and_then(|_| writer.sync_all())
            .expect("persist forged unauthenticated key hint");
        drop(writer);

        let forged_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open forged hint journal");
        let forged_hint = GuardianOutputUntrustedHeaderHint::read_from(&forged_file)
            .expect("structural hint deliberately accepts unauthenticated key bytes");
        assert_ne!(forged_hint.untrusted_key_id(), cipher().key_id());
        drop(forged_file);
        let parent = File::open(directory.path()).expect("open forged-hint parent");
        assert!(matches!(
            open_test_journal_for_append(
                &parent,
                &path,
                identity(),
                cipher(),
                GuardianOutputJournalLimits::default(),
            ),
            Err(GuardianOutputJournalError::KeyIdentityMismatch)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn authenticated_recovery_is_bounded_contiguous_and_reconstructs_terminal_authority() {
        let directory = tempfile::tempdir().expect("create recovery journal directory");
        let path = directory.path().join("recovery.ftgout");
        let parent = File::open(directory.path()).expect("open recovery parent directory");
        let payloads: [&[u8]; 3] = [
            b"FT-RECOVERY-FIRST-SECRET",
            b"FT-RECOVERY-SECOND-SECRET",
            b"FT-RECOVERY-THIRD-SECRET",
        ];

        let mut journal = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize recovery journal");
        journal
            .sync_parent_directory_and_activate()
            .expect("durably activate recovery journal");
        let first = journal
            .append_and_sync(payloads[0])
            .expect("append first recovery record");
        let second = journal
            .append_and_sync(payloads[1])
            .expect("append second recovery record");
        let third = journal
            .append_and_sync(payloads[2])
            .expect("append terminal recovery record");
        drop(journal);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("reopen recovery journal");
        let reopened = GuardianOutputJournalReader::open_existing(
            file,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("authenticate recovery journal");
        assert_eq!(reopened.terminal_receipt(), Some(third));

        let page = reopened
            .recover_committed_range(
                2,
                GuardianOutputRecoveryLimits::new(1, 1024)
                    .expect("fixture recovery limits are valid"),
            )
            .expect("recover one bounded page");
        assert_eq!(page.segment_identity(), identity());
        assert_eq!(page.requested_first_sequence(), 2);
        assert_eq!(page.records().len(), 1);
        assert_eq!(page.records()[0].receipt(), second);
        assert_eq!(page.records()[0].plaintext(), payloads[1]);
        assert!(page.records()[0].authenticated_payload_is_receipt_bound());
        assert!(page.records()[0].receipt().matches_payload(payloads[1]));
        let mut same_length_wrong_payload = payloads[1].to_vec();
        same_length_wrong_payload[0] ^= 0x01;
        assert!(!page.records()[0]
            .receipt()
            .matches_payload(&same_length_wrong_payload));
        assert_eq!(page.next_recovery_sequence(), Some(3));
        assert_eq!(page.committed_next_sequence(), Some(4));
        assert_eq!(page.committed_log_bytes(), third.committed_log_bytes());
        assert_eq!(
            page.cumulative_plaintext_bytes(),
            third.cumulative_plaintext_bytes()
        );
        assert_eq!(page.terminal_receipt(), Some(third));
        assert_eq!(page.terminal_predecessor(), Some(third.into_predecessor()));
        assert_eq!(page.tail(), GuardianOutputJournalTail::Clean);
        let page_debug = format!("{page:?}");
        let record_debug = format!("{:?}", page.records()[0]);
        for payload in payloads {
            let plaintext = std::str::from_utf8(payload).expect("ASCII fixture");
            assert!(!page_debug.contains(plaintext));
            assert!(!record_debug.contains(plaintext));
        }

        let terminal_page = reopened
            .recover_committed_range(
                3,
                GuardianOutputRecoveryLimits::new(2, 1024)
                    .expect("fixture recovery limits are valid"),
            )
            .expect("recover terminal page");
        assert_eq!(terminal_page.records().len(), 1);
        assert_eq!(terminal_page.records()[0].receipt(), third);
        assert_eq!(terminal_page.records()[0].plaintext(), payloads[2]);
        assert_eq!(terminal_page.next_recovery_sequence(), None);

        let end_page = reopened
            .recover_committed_range(
                4,
                GuardianOutputRecoveryLimits::new(1, 1024)
                    .expect("fixture recovery limits are valid"),
            )
            .expect("the committed endpoint is a valid empty recovery page");
        assert!(end_page.records().is_empty());
        assert_eq!(end_page.next_recovery_sequence(), None);
        assert_eq!(end_page.terminal_receipt(), Some(third));

        assert!(matches!(
            reopened.recover_committed_range(
                0,
                GuardianOutputRecoveryLimits::new(1, 1024)
                    .expect("fixture recovery limits are valid"),
            ),
            Err(GuardianOutputJournalError::RecoveryRangeMismatch)
        ));
        assert!(matches!(
            reopened.recover_committed_range(
                5,
                GuardianOutputRecoveryLimits::new(1, 1024)
                    .expect("fixture recovery limits are valid"),
            ),
            Err(GuardianOutputJournalError::RecoveryRangeMismatch)
        ));
        assert!(matches!(
            GuardianOutputRecoveryLimits::new(0, 1),
            Err(GuardianOutputJournalError::InvalidRecoveryLimits(_))
        ));
        assert!(matches!(
            GuardianOutputRecoveryLimits::new(
                GuardianOutputRecoveryLimits::HARD_MAX_RECORDS + 1,
                1,
            ),
            Err(GuardianOutputJournalError::InvalidRecoveryLimits(_))
        ));
        assert!(matches!(
            GuardianOutputRecoveryLimits::new(
                1,
                GuardianOutputRecoveryLimits::HARD_MAX_PLAINTEXT_BYTES + 1,
            ),
            Err(GuardianOutputJournalError::InvalidRecoveryLimits(_))
        ));
        assert!(matches!(
            reopened.recover_committed_range(
                1,
                GuardianOutputRecoveryLimits::new(
                    1,
                    u64::try_from(payloads[0].len() - 1)
                        .expect("fixture recovery byte bound fits u64"),
                )
                .expect("small but nonzero recovery limits are structurally valid"),
            ),
            Err(GuardianOutputJournalError::RecoveryPlaintextByteLimit { .. })
        ));
        assert_eq!(first.sequence(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_method_rejects_same_length_ciphertext_mutation_with_recomputed_outer_digest() {
        let directory = tempfile::tempdir().expect("create mutation recovery directory");
        let path = directory.path().join("mutation-recovery.ftgout");
        let parent = File::open(directory.path()).expect("open mutation recovery parent");
        let segment_identity = identity();
        let mut journal = create_new_test_journal(
            &parent,
            &path,
            segment_identity,
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize mutation recovery journal");
        journal
            .sync_parent_directory_and_activate()
            .expect("activate mutation recovery journal");
        journal
            .append_and_sync(b"same-length authenticated recovery payload")
            .expect("append mutation recovery record");

        let mut bytes = std::fs::read(&path).expect("read mutation recovery journal");
        let header_offset = FILE_HEADER_BYTES;
        let ciphertext_offset = header_offset + RECORD_HEADER_BYTES;
        bytes[ciphertext_offset] ^= 0x80;
        let sequence = read_u64(&bytes[header_offset + 8..header_offset + 16]);
        let plaintext_bytes = read_u32(&bytes[header_offset + 16..header_offset + 20]);
        let ciphertext_bytes = read_u32(&bytes[header_offset + 20..header_offset + 24]);
        let mut nonce = [0_u8; NONCE_BYTES];
        nonce.copy_from_slice(&bytes[header_offset + 32..header_offset + 56]);
        let digest = record_digest(
            segment_identity,
            sequence,
            plaintext_bytes,
            ciphertext_bytes,
            &nonce,
            &bytes[ciphertext_offset..],
        );
        bytes[header_offset + 56..header_offset + 88].copy_from_slice(&digest);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open mutation recovery journal for in-place corruption");
        file.seek(SeekFrom::Start(0))
            .expect("seek mutation recovery journal");
        file.write_all(&bytes)
            .expect("write same-length mutation recovery corruption");
        file.sync_all()
            .expect("synchronize mutation recovery corruption");
        drop(file);

        let error = journal
            .recover_committed_range(
                sequence,
                GuardianOutputRecoveryLimits::new(1, 1024)
                    .expect("fixture recovery limits are valid"),
            )
            .expect_err("the public recovery seam must repeat AEAD authentication");
        assert!(matches!(
            error,
            GuardianOutputJournalError::DecryptionFailed
        ));
    }

    #[cfg(unix)]
    #[test]
    fn maximal_sequence_is_a_terminal_nonwrapping_recovery_cursor_after_reopen() {
        let directory = tempfile::tempdir().expect("create maximal-sequence directory");
        let path = directory.path().join("maximal-sequence.ftgout");
        let parent = File::open(directory.path()).expect("open maximal-sequence parent");
        let predecessor = GuardianOutputPredecessor::new(
            Uuid::from_bytes([0x61; 16]),
            u64::MAX - 1,
            [0x62; 32],
            10,
            4096,
        )
        .expect("maximal-sequence predecessor is valid");
        let maximal_identity = GuardianOutputSegmentIdentity::new(
            pane(),
            Uuid::from_bytes([0x63; 16]),
            u64::MAX,
            Some(predecessor),
        )
        .expect("maximal-sequence segment is valid");
        let mut journal = create_new_test_journal(
            &parent,
            &path,
            maximal_identity,
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize maximal-sequence journal");
        journal
            .sync_parent_directory_and_activate()
            .expect("activate maximal-sequence journal");
        let terminal = journal
            .append_and_sync(b"terminal")
            .expect("commit the maximal representable sequence");
        assert_eq!(terminal.sequence(), u64::MAX);
        assert_eq!(journal.next_sequence(), None);
        assert!(matches!(
            journal.append_and_sync(b"must-not-wrap"),
            Err(GuardianOutputJournalError::SequenceExhausted)
        ));
        drop(journal);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("reopen maximal-sequence journal");
        let reopened = GuardianOutputJournalReader::open_existing(
            file,
            maximal_identity,
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("authenticate maximal-sequence journal after reopen");
        assert_eq!(reopened.next_sequence(), None);
        assert_eq!(reopened.terminal_receipt(), Some(terminal));
        let page = reopened
            .recover_committed_range(
                u64::MAX,
                GuardianOutputRecoveryLimits::new(1, 1024)
                    .expect("fixture recovery limits are valid"),
            )
            .expect("recover the terminal maximal sequence");
        assert_eq!(page.records().len(), 1);
        assert_eq!(page.records()[0].receipt(), terminal);
        assert_eq!(page.records()[0].plaintext(), b"terminal");
        assert_eq!(page.next_recovery_sequence(), None);
        assert_eq!(page.committed_next_sequence(), None);
        assert_eq!(
            page.terminal_predecessor(),
            Some(terminal.into_predecessor())
        );

        let mut cursor = reopened
            .recovery_cursor(u64::MAX, 1024)
            .expect("create maximal nonwrapping stateful cursor");
        let cursor_terminal = cursor
            .next_record()
            .expect("authenticate maximal cursor record")
            .expect("maximal cursor record exists");
        assert_eq!(cursor_terminal.receipt(), terminal);
        assert_eq!(cursor_terminal.plaintext(), b"terminal");
        assert!(cursor
            .next_record()
            .expect("maximal cursor reaches a terminal EOF")
            .is_none());
        assert!(cursor
            .next_record()
            .expect("maximal cursor cannot wrap after EOF")
            .is_none());
        assert_eq!(cursor.verified_record_count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn pane_lifetime_cumulative_endpoint_survives_rollover_reopen_and_predecessor_tamper() {
        let directory = tempfile::tempdir().expect("create rollover journal directory");
        let first_path = directory.path().join("rollover-first.ftgout");
        let successor_path = directory.path().join("rollover-successor.ftgout");
        let parent = File::open(directory.path()).expect("open rollover parent directory");

        let mut first_segment = create_new_test_journal(
            &parent,
            &first_path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize first rollover segment");
        first_segment
            .sync_parent_directory_and_activate()
            .expect("activate first rollover segment");
        let first_receipt = first_segment
            .append_and_sync(b"pane-lifetime-before-rollover")
            .expect("append predecessor record");
        let predecessor = first_receipt.into_predecessor();
        drop(first_segment);

        let successor_identity = GuardianOutputSegmentIdentity::new(
            pane(),
            Uuid::from_bytes([0x26; 16]),
            2,
            Some(predecessor),
        )
        .expect("construct exact successor identity");
        let mut successor = create_new_test_journal(
            &parent,
            &successor_path,
            successor_identity,
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize successor segment");
        assert_eq!(
            successor.cumulative_plaintext_bytes(),
            first_receipt.cumulative_plaintext_bytes()
        );
        assert_eq!(successor.terminal_receipt(), None);
        assert_eq!(successor.terminal_predecessor(), Some(predecessor));
        successor
            .sync_parent_directory_and_activate()
            .expect("activate successor segment");
        let successor_payload = b"pane-lifetime-after-rollover";
        let successor_receipt = successor
            .append_and_sync(successor_payload)
            .expect("append successor record");
        assert_eq!(
            successor_receipt.cumulative_plaintext_bytes(),
            first_receipt
                .cumulative_plaintext_bytes()
                .checked_add(
                    u64::try_from(successor_payload.len())
                        .expect("successor fixture length fits u64"),
                )
                .expect("fixture cumulative endpoint does not overflow")
        );
        drop(successor);

        let reopened = GuardianOutputJournalReader::open_existing(
            std::fs::OpenOptions::new()
                .read(true)
                .open(&successor_path)
                .expect("reopen successor journal"),
            successor_identity,
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("authenticate reopened successor journal");
        assert_eq!(reopened.terminal_receipt(), Some(successor_receipt));
        assert_eq!(
            reopened.cumulative_plaintext_bytes(),
            successor_receipt.cumulative_plaintext_bytes()
        );

        let wrong_cumulative_predecessor = GuardianOutputPredecessor::new(
            predecessor.segment_id(),
            predecessor.last_sequence(),
            predecessor.terminal_record_digest(),
            predecessor
                .cumulative_plaintext_bytes()
                .checked_add(1)
                .expect("fixture cumulative endpoint has headroom"),
            predecessor.committed_log_bytes(),
        )
        .expect("tampered predecessor remains structurally valid");
        let wrong_cumulative_identity = GuardianOutputSegmentIdentity::new(
            pane(),
            successor_identity.segment_id(),
            2,
            Some(wrong_cumulative_predecessor),
        )
        .expect("wrong cumulative identity is structurally valid");
        assert!(matches!(
            GuardianOutputJournalReader::open_existing(
                std::fs::OpenOptions::new()
                    .read(true)
                    .open(&successor_path)
                    .expect("open successor for wrong-cumulative check"),
                wrong_cumulative_identity,
                cipher(),
                GuardianOutputJournalLimits::default(),
            ),
            Err(GuardianOutputJournalError::SegmentIdentityMismatch)
        ));

        let wrong_log_predecessor = GuardianOutputPredecessor::new(
            predecessor.segment_id(),
            predecessor.last_sequence(),
            predecessor.terminal_record_digest(),
            predecessor.cumulative_plaintext_bytes(),
            predecessor
                .committed_log_bytes()
                .checked_add(1)
                .expect("fixture log endpoint has headroom"),
        )
        .expect("wrong log predecessor remains structurally valid");
        let wrong_log_identity = GuardianOutputSegmentIdentity::new(
            pane(),
            successor_identity.segment_id(),
            2,
            Some(wrong_log_predecessor),
        )
        .expect("wrong log identity is structurally valid");
        assert!(matches!(
            GuardianOutputJournalReader::open_existing(
                std::fs::OpenOptions::new()
                    .read(true)
                    .open(&successor_path)
                    .expect("open successor for wrong-log check"),
                wrong_log_identity,
                cipher(),
                GuardianOutputJournalLimits::default(),
            ),
            Err(GuardianOutputJournalError::SegmentIdentityMismatch)
        ));

        let mut tampered_bytes =
            std::fs::read(&successor_path).expect("read successor bytes for header tamper check");
        tampered_bytes[120..128].copy_from_slice(
            &wrong_cumulative_predecessor
                .cumulative_plaintext_bytes()
                .to_le_bytes(),
        );
        let mut tampered_cursor = Cursor::new(tampered_bytes.clone());
        assert!(matches!(
            scan_journal(
                &mut tampered_cursor,
                u64::try_from(tampered_bytes.len()).expect("fixture length fits u64"),
                wrong_cumulative_identity,
                &cipher(),
                GuardianOutputJournalLimits::default(),
            ),
            Err(GuardianOutputJournalError::FileHeaderAuthenticationFailed)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn real_file_torn_tail_is_preserved_and_cannot_be_appended() {
        let directory = tempfile::tempdir().expect("create journal directory");
        let path = directory.path().join("torn.ftgout");
        let parent = File::open(directory.path()).expect("open parent directory");
        let mut journal = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate()
            .expect("activate journal");
        let receipt = journal
            .append_and_sync(b"committed")
            .expect("append committed prefix");
        drop(journal);

        let mut external = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open crash-tail writer");
        external
            .write_all(&RECORD_MAGIC[..3])
            .and_then(|()| external.sync_all())
            .expect("persist simulated torn tail");
        drop(external);
        let physical_bytes = std::fs::metadata(&path)
            .expect("inspect torn journal")
            .len();

        let mut reopened = open_test_journal_for_append(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("recover verified prefix from torn journal");
        assert_eq!(reopened.committed_bytes(), receipt.committed_log_bytes());
        assert_eq!(reopened.record_count(), 1);
        assert!(matches!(
            reopened.tail(),
            GuardianOutputJournalTail::Incomplete {
                committed_bytes,
                trailing_bytes: 3,
            } if committed_bytes == receipt.committed_log_bytes()
        ));
        let recovery = reopened
            .recover_committed_range(
                1,
                GuardianOutputRecoveryLimits::new(2, 1024)
                    .expect("fixture recovery limits are valid"),
            )
            .expect("recover only the authenticated prefix before a torn tail");
        assert_eq!(recovery.records().len(), 1);
        assert_eq!(recovery.records()[0].receipt(), receipt);
        assert_eq!(recovery.records()[0].plaintext(), b"committed");
        assert_eq!(recovery.terminal_receipt(), Some(receipt));
        assert!(matches!(
            recovery.tail(),
            GuardianOutputJournalTail::Incomplete {
                trailing_bytes: 3,
                ..
            }
        ));
        let mut cursor = reopened
            .recovery_cursor(1, 1024)
            .expect("create cursor over authenticated prefix and torn tail");
        assert_eq!(
            cursor
                .next_record()
                .expect("authenticate whole record before torn tail")
                .expect("whole record before torn tail exists")
                .receipt(),
            receipt
        );
        assert!(cursor
            .next_record()
            .expect("cursor excludes the uncommitted torn tail")
            .is_none());
        assert!(matches!(
            cursor.tail(),
            GuardianOutputJournalTail::Incomplete {
                trailing_bytes: 3,
                ..
            }
        ));
        assert!(matches!(
            reopened.append_and_sync(b"must-not-overwrite-tail"),
            Err(GuardianOutputJournalError::IncompleteTail)
        ));
        assert_eq!(
            std::fs::metadata(&path)
                .expect("reinspect preserved torn journal")
                .len(),
            physical_bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_length_change_poisoning_is_sticky() {
        let directory = tempfile::tempdir().expect("create journal directory");
        let path = directory.path().join("poison.ftgout");
        let parent = File::open(directory.path()).expect("open parent directory");
        let mut journal = create_new_test_journal(
            &parent,
            &path,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate()
            .expect("activate journal");
        journal
            .append_and_sync(b"committed")
            .expect("append committed prefix");

        let mut external = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open external writer");
        external
            .write_all(b"unexpected")
            .and_then(|()| external.sync_all())
            .expect("persist external length change");
        drop(external);

        assert!(matches!(
            journal.append_and_sync(b"ambiguous"),
            Err(GuardianOutputJournalError::ExternalLengthChange { .. })
        ));
        assert!(journal.is_poisoned());
        assert!(matches!(
            journal.append_and_sync(b"no-retry"),
            Err(GuardianOutputJournalError::Poisoned)
        ));
    }
}
