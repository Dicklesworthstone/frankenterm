//! Durable, bounded raw PTY-output journal substrate for the guardian.
//!
//! The guardian must synchronize each raw output record before it acknowledges
//! that record to a mux.  This module owns that narrow storage contract.  It
//! deliberately does not open paths: the service layer must supply an
//! exclusively owned, securely opened regular file descriptor from its private
//! state directory.  Keeping path traversal and service policy outside this
//! primitive also makes it impossible to confuse transcript export with the
//! live append authority.
//! Raw terminal bytes are always sealed with XChaCha20-Poly1305 before they
//! reach the file.  Redaction would destroy exact terminal reconstruction, so
//! this module has no plaintext persistence mode.
//!
//! A complete corrupt record fails closed and is never repaired in place.  An
//! incomplete final frame is reported as an uncommitted tail while preserving
//! every byte for diagnosis.  Appends remain disabled on that descriptor; a
//! later segment manager must seal it and publish a fresh successor segment.
//! This avoids deleting or overwriting crash evidence.

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{
        Aead, KeyInit, Payload,
        rand_core::{OsRng, RngCore as _},
    },
};
use sha2::{Digest as _, Sha256};
use std::convert::TryFrom;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use thiserror::Error;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

const FILE_MAGIC: [u8; 8] = *b"FTGOUT01";
const RECORD_MAGIC: [u8; 8] = *b"FTGOR001";
const FORMAT_VERSION: u32 = 1;
const FILE_HEADER_BYTES: usize = 160;
const RECORD_HEADER_BYTES: usize = 96;
const FILE_HEADER_BYTES_U32: u32 = 160;
const RECORD_HEADER_BYTES_U32: u32 = 96;
const FILE_HEADER_BYTES_U64: u64 = 160;
const RECORD_HEADER_BYTES_U64: u64 = 96;
const KEY_ID_BYTES: usize = 8;
const NONCE_BYTES: usize = 24;
const AEAD_TAG_BYTES: u32 = 16;
const AEAD_TAG_BYTES_USIZE: usize = 16;
const FILE_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-output-file.v1\0";
const RECORD_DIGEST_DOMAIN: &[u8] = b"frankenterm.guardian-output-record.v1\0";
const RECORD_AAD_DOMAIN: &[u8] = b"frankenterm.guardian-output-aead.v1\0";

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
            .and_then(|bytes| bytes.checked_add(1))
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
    pub fn write_exact<W: Write>(
        &self,
        writer: &mut W,
    ) -> Result<(), GuardianOutputJournalError> {
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
        let digest = Sha256::digest(&self.bytes);
        let mut key_id = [0_u8; KEY_ID_BYTES];
        key_id.copy_from_slice(&digest[..KEY_ID_BYTES]);
        key_id
    }

    /// Compare two in-memory authorities without exposing either key.
    #[must_use]
    pub fn has_same_material(&self, other: &Self) -> bool {
        self.bytes == other.bytes
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

    /// Authenticate and open guardian-owned journal metadata produced by
    /// [`Self::seal_guardian_metadata`].
    pub(crate) fn open_guardian_metadata(
        &self,
        nonce_bytes: &[u8; NONCE_BYTES],
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, GuardianOutputJournalError> {
        self.cipher
            .decrypt(
                XNonce::from_slice(nonce_bytes),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
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
}

impl GuardianOutputPredecessor {
    pub fn new(
        segment_id: Uuid,
        last_sequence: u64,
        terminal_record_digest: [u8; 32],
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
        Ok(Self {
            segment_id,
            last_sequence,
            terminal_record_digest,
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
}

impl std::fmt::Debug for GuardianOutputPredecessor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuardianOutputPredecessor")
            .field("segment_id", &self.segment_id)
            .field("last_sequence", &self.last_sequence)
            .field("terminal_record_digest", &"[REDACTED]")
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
    committed_log_bytes: u64,
    record_digest: [u8; 32],
}

impl GuardianOutputAppendReceipt {
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

    #[must_use]
    pub const fn committed_log_bytes(self) -> u64 {
        self.committed_log_bytes
    }

    #[must_use]
    pub const fn record_digest(self) -> [u8; 32] {
        self.record_digest
    }

    #[must_use]
    pub const fn into_predecessor(self) -> GuardianOutputPredecessor {
        GuardianOutputPredecessor {
            segment_id: self.segment_id,
            last_sequence: self.sequence,
            terminal_record_digest: self.record_digest,
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
            .field("committed_log_bytes", &self.committed_log_bytes)
            .field("record_digest", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum GuardianOutputJournalError {
    #[error("invalid guardian output journal limits: {0}")]
    InvalidLimits(&'static str),
    #[error("invalid guardian output segment identity: {0}")]
    InvalidSegmentIdentity(&'static str),
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
    #[error("guardian output journal file header digest mismatch")]
    FileHeaderDigestMismatch,
    #[error("guardian output journal exceeds its byte limit: {observed} > {maximum}")]
    LogByteLimit { observed: u64, maximum: u64 },
    #[error("guardian output journal record limit {maximum} is exhausted")]
    RecordLimit { maximum: u64 },
    #[error("guardian output journal record at byte {offset} has invalid magic")]
    InvalidRecordMagic { offset: u64 },
    #[error("guardian output journal record at byte {offset} has invalid header length {observed}")]
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
    #[error("new guardian output segment is not active until its parent directory is synchronized")]
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
    next_sequence: Option<u64>,
    tail: GuardianOutputJournalTail,
}

/// An exclusively owned append authority for one raw-output segment.
pub struct GuardianOutputJournal {
    file: File,
    identity: GuardianOutputSegmentIdentity,
    cipher: GuardianOutputCipher,
    limits: GuardianOutputJournalLimits,
    committed_bytes: u64,
    record_count: u64,
    next_sequence: Option<u64>,
    tail: GuardianOutputJournalTail,
    directory_entry_sync_required: bool,
    poisoned: bool,
}

impl GuardianOutputJournal {
    /// Open or initialize one journal segment from a securely opened descriptor.
    ///
    /// The caller must guarantee exclusive ownership and must have rejected
    /// symlinks while opening the path.  This method independently rejects
    /// non-regular descriptors and validates the complete committed prefix.
    pub fn open(
        mut file: File,
        identity: GuardianOutputSegmentIdentity,
        cipher: GuardianOutputCipher,
        limits: GuardianOutputJournalLimits,
    ) -> Result<Self, GuardianOutputJournalError> {
        let limits = limits.validate()?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(GuardianOutputJournalError::NotRegularFile);
        }
        let mut physical_bytes = metadata.len();
        if physical_bytes > limits.max_log_bytes {
            return Err(GuardianOutputJournalError::LogByteLimit {
                observed: physical_bytes,
                maximum: limits.max_log_bytes,
            });
        }
        let initialized_new_segment = physical_bytes == 0;
        if initialized_new_segment {
            let header = encode_file_header(identity, cipher.key_id);
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&header)?;
            file.sync_all()?;
            physical_bytes = FILE_HEADER_BYTES_U64;
        }
        if physical_bytes < FILE_HEADER_BYTES_U64 {
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
            next_sequence: scan.next_sequence,
            tail: scan.tail,
            directory_entry_sync_required: initialized_new_segment,
            poisoned: false,
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
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
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
        self.directory_entry_sync_required
    }

    /// Synchronize the exact parent directory that published a new segment.
    ///
    /// A newly initialized file cannot accept output until this succeeds.  A
    /// file-level sync alone does not guarantee that the directory entry will
    /// survive a crash.  The service layer must pass the securely opened parent
    /// directory descriptor corresponding to this segment.
    pub fn sync_parent_directory_and_activate(
        &mut self,
        parent_directory: &File,
    ) -> Result<(), GuardianOutputJournalError> {
        if !self.directory_entry_sync_required {
            return Ok(());
        }
        let metadata = parent_directory.metadata()?;
        if !metadata.file_type().is_dir() {
            return Err(GuardianOutputJournalError::NotDirectory);
        }
        parent_directory.sync_all()?;
        self.directory_entry_sync_required = false;
        Ok(())
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
        if self.poisoned {
            return Err(GuardianOutputJournalError::Poisoned);
        }
        if self.directory_entry_sync_required {
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
        let (nonce, ciphertext) = self
            .cipher
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
        self.next_sequence = sequence.checked_add(1);
        Ok(GuardianOutputAppendReceipt {
            segment_id: self.identity.segment_id,
            sequence,
            payload_bytes,
            committed_log_bytes: projected_bytes,
            record_digest,
        })
    }
}

fn encode_file_header(
    identity: GuardianOutputSegmentIdentity,
    key_id: [u8; KEY_ID_BYTES],
) -> [u8; FILE_HEADER_BYTES] {
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
    }
    header[112..120].copy_from_slice(&key_id);
    let mut hasher = Sha256::new();
    hasher.update(FILE_DIGEST_DOMAIN);
    hasher.update(&header[0..128]);
    header[128..160].copy_from_slice(&hasher.finalize());
    header
}

fn validate_file_header(
    header: &[u8; FILE_HEADER_BYTES],
    identity: GuardianOutputSegmentIdentity,
    key_id: [u8; KEY_ID_BYTES],
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
        }
        None => header[56..112].iter().all(|byte| *byte == 0),
    };
    if !predecessor_matches || header[120..128].iter().any(|byte| *byte != 0) {
        return Err(GuardianOutputJournalError::SegmentIdentityMismatch);
    }
    if header[112..120] != key_id {
        return Err(GuardianOutputJournalError::KeyIdentityMismatch);
    }
    let mut hasher = Sha256::new();
    hasher.update(FILE_DIGEST_DOMAIN);
    hasher.update(&header[0..128]);
    if header[128..160] != hasher.finalize()[..] {
        return Err(GuardianOutputJournalError::FileHeaderDigestMismatch);
    }
    Ok(())
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
    hasher.update(identity.durable_pane_id.as_bytes());
    hasher.update(identity.segment_id.as_bytes());
    hasher.update(sequence.to_le_bytes());
    hasher.update(u64::from(plaintext_bytes).to_le_bytes());
    hasher.update(u64::from(ciphertext_bytes).to_le_bytes());
    hasher.update(nonce);
    hasher.update(ciphertext);
    hasher.finalize().into()
}

fn record_aad(
    identity: GuardianOutputSegmentIdentity,
    sequence: u64,
    plaintext_bytes: u32,
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(RECORD_AAD_DOMAIN.len() + 16 + 16 + 8 + 4);
    aad.extend_from_slice(RECORD_AAD_DOMAIN);
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
    reader.seek(SeekFrom::Start(0))?;
    let mut file_header = [0_u8; FILE_HEADER_BYTES];
    reader.read_exact(&mut file_header)?;
    validate_file_header(&file_header, identity, cipher.key_id)?;

    let mut committed_bytes = FILE_HEADER_BYTES_U64;
    let mut record_count = 0_u64;
    let mut next_sequence = Some(identity.first_sequence);
    while committed_bytes < physical_bytes {
        let remaining = physical_bytes
            .checked_sub(committed_bytes)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        if remaining < RECORD_HEADER_BYTES_U64 {
            return Ok(JournalScan {
                committed_bytes,
                record_count,
                next_sequence,
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
                next_sequence,
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
        let plaintext = cipher.open(
            identity,
            sequence,
            plaintext_bytes,
            &nonce,
            &ciphertext,
        )?;
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
        committed_bytes = committed_bytes
            .checked_add(frame_bytes)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        record_count = record_count
            .checked_add(1)
            .ok_or(GuardianOutputJournalError::ArithmeticOverflow)?;
        next_sequence = sequence.checked_add(1);
    }
    Ok(JournalScan {
        committed_bytes,
        record_count,
        next_sequence,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write as _};

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

    fn pane() -> Uuid {
        Uuid::from_bytes([0x42; 16])
    }

    fn identity() -> GuardianOutputSegmentIdentity {
        GuardianOutputSegmentIdentity::new(
            pane(),
            Uuid::from_bytes([0x24; 16]),
            1,
            None,
        )
        .expect("fixture segment identity is valid")
    }

    fn cipher() -> GuardianOutputCipher {
        GuardianOutputCipher::try_from_key_slice(&[0x71; 32])
            .expect("fixture encryption key is valid")
    }

    fn journal_bytes_for(
        identity: GuardianOutputSegmentIdentity,
        records: &[&[u8]],
    ) -> Vec<u8> {
        let cipher = cipher();
        let mut bytes = encode_file_header(identity, cipher.key_id).to_vec();
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

    #[test]
    fn successor_segment_requires_exact_contiguous_predecessor_chain() {
        let previous = GuardianOutputPredecessor::new(
            Uuid::from_bytes([0x11; 16]),
            10,
            [0x55; 32],
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
    fn record_digest_rejects_cross_segment_transplant() {
        let source = identity();
        let target = GuardianOutputSegmentIdentity::new(
            pane(),
            Uuid::from_bytes([0x25; 16]),
            1,
            None,
        )
        .expect("target segment identity is valid");
        let mut bytes = journal_bytes_for(source, &[b"bound to source segment"]);
        let cipher = cipher();
        bytes[0..FILE_HEADER_BYTES]
            .copy_from_slice(&encode_file_header(target, cipher.key_id));
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
        let error = scan_journal(
            &mut cursor,
            u64::try_from(bytes.len()).expect("fixture length fits u64"),
            identity,
            &cipher,
            GuardianOutputJournalLimits::default(),
        )
        .expect_err("AEAD authentication must reject recomputed outer digest");
        assert!(matches!(
            error,
            GuardianOutputJournalError::DecryptionFailed
        ));
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
        let second_header = FILE_HEADER_BYTES
            + RECORD_HEADER_BYTES
            + b"first".len()
            + AEAD_TAG_BYTES_USIZE;
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
            committed_log_bytes: 128,
            record_digest: [0xab; 32],
        };
        let rendered = format!("{receipt:?}");
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("ab"));
    }

    #[cfg(unix)]
    #[test]
    fn real_file_creation_activation_append_and_reopen_are_contiguous() {
        let directory = tempfile::tempdir().expect("create journal directory");
        let path = directory.path().join("segment.ftgout");
        let parent = File::open(directory.path()).expect("open parent directory");
        let payload = b"FT-REAL-FILE-PLAINTEXT-MUST-NOT-APPEAR";

        let mut journal = GuardianOutputJournal::open(
            create_journal_file(&path),
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
            .sync_parent_directory_and_activate(&parent)
            .expect("durably activate journal");
        let first = journal
            .append_and_sync(payload)
            .expect("append first synchronized record");
        assert_eq!(first.sequence(), 1);
        let committed_after_first = first.committed_log_bytes();
        drop(journal);

        let bytes = std::fs::read(&path).expect("read encrypted journal bytes");
        assert_eq!(
            u64::try_from(bytes.len()).expect("journal length fits u64"),
            committed_after_first
        );
        assert!(!bytes
            .windows(payload.len())
            .any(|window| window == payload));

        let reopened_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("reopen journal");
        let mut reopened = GuardianOutputJournal::open(
            reopened_file,
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("validate reopened journal");
        assert!(!reopened.directory_entry_sync_required());
        assert_eq!(reopened.record_count(), 1);
        assert_eq!(reopened.next_sequence(), Some(2));
        let second = reopened
            .append_and_sync(b"second")
            .expect("append contiguous record after reopen");
        assert_eq!(second.sequence(), 2);
        assert!(second.committed_log_bytes() > committed_after_first);
    }

    #[cfg(unix)]
    #[test]
    fn real_file_torn_tail_is_preserved_and_cannot_be_appended() {
        let directory = tempfile::tempdir().expect("create journal directory");
        let path = directory.path().join("torn.ftgout");
        let parent = File::open(directory.path()).expect("open parent directory");
        let mut journal = GuardianOutputJournal::open(
            create_journal_file(&path),
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&parent)
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

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("reopen torn journal");
        let mut reopened = GuardianOutputJournal::open(
            file,
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
        let mut journal = GuardianOutputJournal::open(
            create_journal_file(&path),
            identity(),
            cipher(),
            GuardianOutputJournalLimits::default(),
        )
        .expect("initialize journal");
        journal
            .sync_parent_directory_and_activate(&parent)
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
