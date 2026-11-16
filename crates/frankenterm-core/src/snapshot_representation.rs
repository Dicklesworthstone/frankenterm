//! Snapshot representation: bounded zstd compression and AEAD encrypted recovery objects.
//!
//! # Bead: `ft-interactive-swarm-product-convergence-7xqz4.8.14.2.3`
//!
//! Provides defense-in-depth representation encoding for terminal/mux snapshot recovery:
//!
//! 1. **Bounded streaming zstd compression**: semantic bytes are compressed with explicit output
//!    budget limits enforced *during production* via a bounded writer, preventing unbounded allocation.
//! 2. **AEAD Encryption (XChaCha20-Poly1305)**: compressed bytes are encrypted using pure-Rust
//!    XChaCha20-Poly1305 with random 192-bit nonces, ensuring collision resistance across
//!    restarts and concurrent workers without state coordination.
//! 3. **Versioned Object/Generation Identity AAD**: Associated Authenticated Data cryptographically
//!    binds logical object identity, representation version, generation, predecessor generation
//!    (with explicit Option discriminant tagging to prevent None/Some(0) aliasing), timestamp,
//!    key ID, chunk bounds (`chunk_index` / `chunk_count`), and digests. Tampering with any
//!    contextual field fails AEAD verification.
//! 4. **Digest Verification**: SHA-256 digests verify plaintext integrity before compression,
//!    compressed bytes before encryption, and ciphertext after encryption.
//! 5. **Bounded Decompression & Symmetric Policy**: mandatory exact byte ceilings (`max_uncompressed_bytes`)
//!    strictly protect against memory exhaustion. Default policy cleanly supports ordinary sparse terminal
//!    snapshots (e.g. large repeated spaces/zeros) without arbitrary low expansion ratio rejections.
//!    Optional expansion ratio limits are enforced symmetrically by both encoder and decoder.
//! 6. **Zero Secret Debug & Zeroization**: symmetric key material, compressed plaintext, and decoded
//!    representation plaintext are guarded by `zeroize::Zeroizing`, with `Debug` redacting all secret bytes.
//! 7. **Whole-Representation Wire Envelope for FEC & Publication**: the canonical binary format
//!    `to_bytes()` packages magic, version, canonical header metadata (with context/nonce), and ciphertext.
//!    Non-canonical header JSON is strictly rejected during parsing to guarantee deterministic representation
//!    identity (`SHA-256(exact envelope bytes)`). Peers `%570` (RaptorQ FEC) and `%572` (Snapshot Publication)
//!    protect and persist this complete envelope.

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use hmac::{Hmac, Mac};
use rand::{TryRng, rngs::SysRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use thiserror::Error;
use zeroize::Zeroizing;

// =============================================================================
// Constants & Domains
// =============================================================================

/// Envelope magic bytes identifying a frankenterm encrypted recovery object.
pub const RECOVERY_OBJECT_MAGIC: [u8; 8] = *b"FTRECOV1";

/// Current recovery representation format version.
pub const RECOVERY_FORMAT_VERSION: u32 = 1;

/// Domain separator for AAD computation.
pub const RECOVERY_AAD_DOMAIN: &[u8] = b"frankenterm.snapshot-recovery.aad.v1\0";

/// Domain separator for key-id derivation.
pub const RECOVERY_KEY_ID_DOMAIN: &[u8] = b"frankenterm.snapshot-recovery.key-id.v1\0";

/// Separate authentication-key domain for persisted repair and discovery metadata.
pub const RECOVERY_REPAIR_KEY_DOMAIN: &[u8] = b"frankenterm.snapshot-recovery.repair-auth-key.v1\0";

/// Domain separator for chunk coordinate identity derivation.
pub const RECOVERY_CHUNK_COORDINATE_ID_DOMAIN: &[u8] =
    b"frankenterm.snapshot-recovery.chunk-coordinate-id.v1\0";

/// Required symmetric key length in bytes (256 bits).
pub const KEY_BYTES: usize = 32;

/// Required nonce length for XChaCha20-Poly1305 in bytes (192 bits).
pub const NONCE_BYTES: usize = 24;

/// Poly1305 authentication tag size in bytes.
pub const AEAD_TAG_BYTES: usize = 16;

/// Default maximum uncompressed object size (64 MiB).
pub const DEFAULT_MAX_UNCOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum compressed object size (64 MiB).
pub const DEFAULT_MAX_COMPRESSED_BYTES: usize = 64 * 1024 * 1024;

/// Default zstd compression level.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// Maximum allowable binary envelope size (65 MiB, matching single-block RFC 6330 limit + header overhead).
pub const MAX_ENVELOPE_BYTES: usize = 65 * 1024 * 1024;

/// Maximum allowable header JSON size (64 KiB) before deserialization.
pub const MAX_HEADER_JSON_BYTES: usize = 64 * 1024;

// =============================================================================
// Error Types
// =============================================================================

/// Errors emitted during recovery representation encoding, decoding, and verification.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RepresentationError {
    #[error("envelope exceeds maximum allowable size ({size} bytes > limit {limit} bytes)")]
    EnvelopeTooLarge { size: usize, limit: usize },

    #[error("header JSON exceeds maximum allowable size ({size} bytes > limit {limit} bytes)")]
    HeaderTooLarge { size: usize, limit: usize },

    #[error("ciphertext exceeds maximum allowable size ({size} bytes > limit {limit} bytes)")]
    CiphertextTooLarge { size: usize, limit: usize },

    #[error("uncompressed payload exceeds budget ({size} bytes > limit {limit} bytes)")]
    UncompressedLimitExceeded { size: usize, limit: usize },

    #[error("compressed payload exceeds budget ({size} bytes > limit {limit} bytes)")]
    CompressedLimitExceeded { size: usize, limit: usize },

    #[error(
        "decompression expansion ratio exceeds budget ({uncompressed} / {compressed} > max {limit}x)"
    )]
    ExpansionRatioExceeded {
        uncompressed: usize,
        compressed: usize,
        limit: usize,
    },

    #[error("compression failed: {reason}")]
    CompressionFailed { reason: String },

    #[error("unable to allocate bounded {operation} buffer")]
    AllocationFailed { operation: &'static str },

    #[error("decompression failed: {reason}")]
    DecompressionFailed { reason: String },

    #[error("decompressed length mismatch: expected {expected} bytes, got {actual} bytes")]
    DecompressedLengthMismatch { expected: usize, actual: usize },

    #[error("digest mismatch for {target}: expected {expected}, got {actual}")]
    DigestMismatch {
        target: &'static str,
        expected: String,
        actual: String,
    },

    #[error("encryption failed: {reason}")]
    EncryptionFailed { reason: String },

    #[error("decryption failed (wrong key, tampered ciphertext, or mismatched AAD): {reason}")]
    DecryptionFailed { reason: String },

    #[error("invalid recovery object magic: expected {expected:?}, got {actual:?}")]
    InvalidMagic { expected: [u8; 8], actual: [u8; 8] },

    #[error("unsupported recovery representation version {version}")]
    UnsupportedVersion { version: u32 },

    #[error("malformed recovery object representation: {reason}")]
    MalformedRepresentation { reason: String },

    #[error("invalid metadata: {reason}")]
    InvalidMetadata { reason: String },

    #[error("serialization failed: {reason}")]
    SerializationError { reason: String },

    #[error("key error: {reason}")]
    KeyError { reason: String },

    #[error("entropy source unavailable for secure generation: {reason}")]
    EntropyUnavailable { reason: String },

    #[error("expected context mismatch for {field}: expected {expected}, got {actual}")]
    ContextMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
}

// =============================================================================
// Recovery Key Custody
// =============================================================================

/// Symmetric 256-bit key for recovery representation encryption.
///
/// Automatically zeroizes memory on drop via `zeroize::Zeroizing`.
/// `Debug` implementation redacts all key material.
#[derive(Clone)]
pub struct RecoveryKey {
    bytes: Zeroizing<[u8; KEY_BYTES]>,
    key_id: [u8; 8],
}

impl RecoveryKey {
    /// Construct a recovery key from raw 32 bytes.
    ///
    /// Rejects all-zero keys to prevent uninitialized key use.
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Result<Self, RepresentationError> {
        let bytes = Zeroizing::new(bytes);
        if bytes.iter().all(|b| *b == 0) {
            return Err(RepresentationError::KeyError {
                reason: "all-zero recovery key is rejected".into(),
            });
        }
        let mut hasher = Sha256::new();
        hasher.update(RECOVERY_KEY_ID_DOMAIN);
        hasher.update(bytes.as_slice());
        let digest = hasher.finalize();
        let mut key_id = [0u8; 8];
        key_id.copy_from_slice(&digest[..8]);
        Ok(Self { bytes, key_id })
    }

    /// Generate a cryptographically random recovery key using system entropy.
    ///
    /// Returns an error if the system entropy source fails; never panics or uses
    /// fixed fallback constants.
    pub fn generate() -> Result<Self, RepresentationError> {
        let mut bytes = Zeroizing::new([0u8; KEY_BYTES]);
        let mut rng = SysRng;
        rng.try_fill_bytes(bytes.as_mut_slice()).map_err(|e| {
            RepresentationError::EntropyUnavailable {
                reason: format!("failed to obtain {} bytes of entropy: {e}", KEY_BYTES),
            }
        })?;
        Self::from_bytes(*bytes)
    }

    /// 8-byte non-secret key identifier for key lookup and rotation.
    #[must_use]
    pub fn key_id(&self) -> [u8; 8] {
        self.key_id
    }

    /// Hex representation of the key identifier.
    #[must_use]
    pub fn key_id_hex(&self) -> String {
        hex::encode(self.key_id)
    }

    /// Raw key bytes for cipher initialization.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.bytes
    }

    /// Deterministically derives a distinct MAC key without exposing encryption
    /// key bytes to repair metadata. The caller retains this zeroizing owner.
    pub fn derive_repair_authentication_key(
        &self,
    ) -> Result<Zeroizing<[u8; KEY_BYTES]>, RepresentationError> {
        let mut mac =
            <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(self.as_bytes()).map_err(|_| {
                RepresentationError::KeyError {
                    reason: "repair authentication key derivation failed".into(),
                }
            })?;
        mac.update(RECOVERY_REPAIR_KEY_DOMAIN);
        Ok(Zeroizing::new(mac.finalize().into_bytes().into()))
    }
}

impl std::fmt::Debug for RecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryKey")
            .field("key_id", &self.key_id_hex())
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

// =============================================================================
// Wrapped recovery keys: cryptographic envelope only, not durable KEK custody.
// =============================================================================

const RECOVERY_WRAP_MAGIC: [u8; 8] = *b"FTWRAP01";
const RECOVERY_WRAP_VERSION: u32 = 1;
const RECOVERY_WRAP_DOMAIN: &[u8] = b"frankenterm.snapshot-recovery.key-wrap.v1\0";
const RECOVERY_WRAPPING_KEY_ID_DOMAIN: &[u8] =
    b"frankenterm.snapshot-recovery.wrapping-authority-id.v1\0";
pub const WRAPPED_RECOVERY_KEY_BYTES: usize = 188;

/// Explicit caller-owned wrapping authority. This type neither persists keys
/// nor establishes enrollment, independent recovery, rotation, or revocation.
pub struct RecoveryWrappingKey {
    bytes: Zeroizing<[u8; KEY_BYTES]>,
    authority_id: [u8; 32],
}

impl RecoveryWrappingKey {
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Result<Self, RecoveryWrapError> {
        let bytes = Zeroizing::new(bytes);
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(RecoveryWrapError::InvalidKey);
        }
        let mut digest = Sha256::new();
        digest.update(RECOVERY_WRAPPING_KEY_ID_DOMAIN);
        digest.update(bytes.as_slice());
        Ok(Self {
            bytes,
            authority_id: digest.finalize().into(),
        })
    }

    #[must_use]
    pub const fn authority_id(&self) -> [u8; 32] {
        self.authority_id
    }
}

impl std::fmt::Debug for RecoveryWrappingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryWrappingKey")
            .field("authority_id", &hex::encode(self.authority_id))
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryWrapContext {
    pub namespace_id: [u8; 32],
    pub policy_id: [u8; 32],
}

/// Caller-provided expectations, independent of the artifact being opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpectedRecoveryWrapContext {
    pub namespace_id: [u8; 32],
    pub policy_id: [u8; 32],
    pub recovery_key_id: [u8; 8],
    pub authority_id: [u8; 32],
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RecoveryWrapError {
    #[error("wrapped recovery key has invalid fixed framing")]
    InvalidEnvelope,
    #[error("unsupported wrapped recovery key version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid recovery wrapping key")]
    InvalidKey,
    #[error("recovery key wrapper identity mismatch: {field}")]
    KeyMismatch { field: &'static str },
    #[error("recovery key wrapper context mismatch: {field}")]
    ContextMismatch { field: &'static str },
    #[error("recovery key wrapper authentication failed")]
    AuthenticationFailed,
    #[error("recovery key wrapper entropy unavailable")]
    EntropyUnavailable,
}

/// One independently authenticated wrapper for a DEK. Multiple authorities
/// produce separate records sharing a recovery-key ID, not shared KEK material.
#[derive(Clone, PartialEq, Eq)]
pub struct WrappedRecoveryKey {
    expected: ExpectedRecoveryWrapContext,
    nonce: [u8; NONCE_BYTES],
    ciphertext: [u8; KEY_BYTES + AEAD_TAG_BYTES],
}

impl std::fmt::Debug for WrappedRecoveryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WrappedRecoveryKey")
            .field("identity", &self.expected)
            .field("ciphertext", &"[REDACTED]")
            .finish()
    }
}

impl WrappedRecoveryKey {
    #[must_use]
    pub const fn identity(&self) -> ExpectedRecoveryWrapContext {
        self.expected
    }

    #[must_use]
    pub fn to_bytes(&self) -> [u8; WRAPPED_RECOVERY_KEY_BYTES] {
        let mut bytes = [0; WRAPPED_RECOVERY_KEY_BYTES];
        bytes[..8].copy_from_slice(&RECOVERY_WRAP_MAGIC);
        bytes[8..12].copy_from_slice(&RECOVERY_WRAP_VERSION.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.expected.recovery_key_id);
        bytes[20..52].copy_from_slice(&self.expected.authority_id);
        bytes[52..84].copy_from_slice(&self.expected.namespace_id);
        bytes[84..116].copy_from_slice(&self.expected.policy_id);
        bytes[116..140].copy_from_slice(&self.nonce);
        bytes[140..].copy_from_slice(&self.ciphertext);
        bytes
    }

    /// Decode fixed framing only. This does not authenticate or authorize use;
    /// the caller must supply independent expectations to `unwrap_recovery_key`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RecoveryWrapError> {
        if bytes.len() != WRAPPED_RECOVERY_KEY_BYTES || bytes[..8] != RECOVERY_WRAP_MAGIC {
            return Err(RecoveryWrapError::InvalidEnvelope);
        }
        let mut version = [0; 4];
        version.copy_from_slice(&bytes[8..12]);
        let version = u32::from_le_bytes(version);
        if version != RECOVERY_WRAP_VERSION {
            return Err(RecoveryWrapError::UnsupportedVersion(version));
        }
        let mut wrapped = Self {
            expected: ExpectedRecoveryWrapContext {
                namespace_id: [0; 32],
                policy_id: [0; 32],
                recovery_key_id: [0; 8],
                authority_id: [0; 32],
            },
            nonce: [0; NONCE_BYTES],
            ciphertext: [0; KEY_BYTES + AEAD_TAG_BYTES],
        };
        wrapped
            .expected
            .recovery_key_id
            .copy_from_slice(&bytes[12..20]);
        wrapped
            .expected
            .authority_id
            .copy_from_slice(&bytes[20..52]);
        wrapped
            .expected
            .namespace_id
            .copy_from_slice(&bytes[52..84]);
        wrapped.expected.policy_id.copy_from_slice(&bytes[84..116]);
        wrapped.nonce.copy_from_slice(&bytes[116..140]);
        wrapped.ciphertext.copy_from_slice(&bytes[140..]);
        Ok(wrapped)
    }

    fn aad(&self) -> Vec<u8> {
        let mut aad = Vec::with_capacity(RECOVERY_WRAP_DOMAIN.len() + 116);
        aad.extend_from_slice(RECOVERY_WRAP_DOMAIN);
        aad.extend_from_slice(&self.to_bytes()[..116]);
        aad
    }
}

pub fn wrap_recovery_key(
    key: &RecoveryKey,
    authority: &RecoveryWrappingKey,
    context: &RecoveryWrapContext,
) -> Result<WrappedRecoveryKey, RecoveryWrapError> {
    let mut wrapped = WrappedRecoveryKey {
        expected: ExpectedRecoveryWrapContext {
            namespace_id: context.namespace_id,
            policy_id: context.policy_id,
            recovery_key_id: key.key_id(),
            authority_id: authority.authority_id(),
        },
        nonce: [0; NONCE_BYTES],
        ciphertext: [0; KEY_BYTES + AEAD_TAG_BYTES],
    };
    SysRng
        .try_fill_bytes(&mut wrapped.nonce)
        .map_err(|_| RecoveryWrapError::EntropyUnavailable)?;
    let cipher = XChaCha20Poly1305::new_from_slice(authority.bytes.as_slice())
        .map_err(|_| RecoveryWrapError::InvalidKey)?;
    let plaintext = Zeroizing::new(*key.as_bytes());
    let aad = wrapped.aad();
    let ciphertext = cipher
        .encrypt(
            &XNonce::from(wrapped.nonce),
            Payload {
                msg: plaintext.as_slice(),
                aad: &aad,
            },
        )
        .map_err(|_| RecoveryWrapError::AuthenticationFailed)?;
    if ciphertext.len() != KEY_BYTES + AEAD_TAG_BYTES {
        return Err(RecoveryWrapError::InvalidEnvelope);
    }
    wrapped.ciphertext.copy_from_slice(&ciphertext);
    Ok(wrapped)
}

pub fn unwrap_recovery_key(
    wrapped: &WrappedRecoveryKey,
    authority: &RecoveryWrappingKey,
    expected: &ExpectedRecoveryWrapContext,
) -> Result<RecoveryKey, RecoveryWrapError> {
    if wrapped.expected.authority_id != expected.authority_id
        || authority.authority_id() != expected.authority_id
    {
        return Err(RecoveryWrapError::KeyMismatch {
            field: "authority_id",
        });
    }
    if wrapped.expected.recovery_key_id != expected.recovery_key_id {
        return Err(RecoveryWrapError::KeyMismatch {
            field: "recovery_key_id",
        });
    }
    if wrapped.expected.namespace_id != expected.namespace_id {
        return Err(RecoveryWrapError::ContextMismatch {
            field: "namespace_id",
        });
    }
    if wrapped.expected.policy_id != expected.policy_id {
        return Err(RecoveryWrapError::ContextMismatch { field: "policy_id" });
    }
    let cipher = XChaCha20Poly1305::new_from_slice(authority.bytes.as_slice())
        .map_err(|_| RecoveryWrapError::InvalidKey)?;
    let aad = wrapped.aad();
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &XNonce::from(wrapped.nonce),
                Payload {
                    msg: &wrapped.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| RecoveryWrapError::AuthenticationFailed)?,
    );
    if plaintext.len() != KEY_BYTES {
        return Err(RecoveryWrapError::InvalidEnvelope);
    }
    let mut bytes = Zeroizing::new([0; KEY_BYTES]);
    bytes.copy_from_slice(&plaintext);
    let key = RecoveryKey::from_bytes(*bytes).map_err(|_| RecoveryWrapError::InvalidKey)?;
    if key.key_id() != expected.recovery_key_id {
        return Err(RecoveryWrapError::KeyMismatch {
            field: "recovery_key_id",
        });
    }
    Ok(key)
}

// =============================================================================
// Context & AAD Data Structures
// =============================================================================

/// Domain taxonomy for recovery objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum RecoveryObjectKind {
    WholeMuxImage = 1,
    TerminalCheckpoint = 2,
    ScrollbackSegment = 3,
    TopologyTree = 4,
    Custom = 255,
}

/// Metadata supplied by caller when encoding a recovery object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub object_id: [u8; 32],
    pub object_kind: RecoveryObjectKind,
    pub generation: u64,
    pub predecessor_generation: Option<u64>,
    pub epoch_timestamp_ms: u64,
    pub chunk_index: u32,
    pub chunk_count: u32,
}

impl ObjectMetadata {
    /// Create metadata for a single unchunked object.
    #[must_use]
    pub fn single(
        object_id: [u8; 32],
        object_kind: RecoveryObjectKind,
        generation: u64,
        predecessor_generation: Option<u64>,
        epoch_timestamp_ms: u64,
    ) -> Self {
        Self {
            object_id,
            object_kind,
            generation,
            predecessor_generation,
            epoch_timestamp_ms,
            chunk_index: 0,
            chunk_count: 1,
        }
    }

    /// Create metadata for an explicit chunk in a multi-chunk object.
    #[must_use]
    pub fn chunk(
        object_id: [u8; 32],
        object_kind: RecoveryObjectKind,
        generation: u64,
        predecessor_generation: Option<u64>,
        epoch_timestamp_ms: u64,
        chunk_index: u32,
        chunk_count: u32,
    ) -> Self {
        Self {
            object_id,
            object_kind,
            generation,
            predecessor_generation,
            epoch_timestamp_ms,
            chunk_index,
            chunk_count,
        }
    }

    /// Validate structural constraints on object metadata.
    pub fn validate(&self) -> Result<(), RepresentationError> {
        if self.chunk_count == 0 {
            return Err(RepresentationError::InvalidMetadata {
                reason: "chunk_count must be greater than 0".into(),
            });
        }
        if self.chunk_index >= self.chunk_count {
            return Err(RepresentationError::InvalidMetadata {
                reason: format!(
                    "chunk_index {} must be less than chunk_count {}",
                    self.chunk_index, self.chunk_count
                ),
            });
        }
        if let Some(pred) = self.predecessor_generation {
            if pred >= self.generation {
                return Err(RepresentationError::InvalidMetadata {
                    reason: format!(
                        "predecessor_generation {} must be strictly less than generation {}",
                        pred, self.generation
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Expected verification context supplied by caller when decoding.
///
/// Cryptographically binds object identity, generation, kind, and chunk bounds
/// to ensure that chunks from different objects or different positions cannot be swapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedContext {
    pub object_id: [u8; 32],
    pub object_kind: RecoveryObjectKind,
    pub generation: u64,
    pub predecessor_generation: Option<u64>,
    pub chunk_index: u32,
    pub chunk_count: u32,
}

impl ExpectedContext {
    /// Create expected context for a single unchunked object (chunk 0 of 1).
    #[must_use]
    pub fn single(
        object_id: [u8; 32],
        object_kind: RecoveryObjectKind,
        generation: u64,
        predecessor_generation: Option<u64>,
    ) -> Self {
        Self {
            object_id,
            object_kind,
            generation,
            predecessor_generation,
            chunk_index: 0,
            chunk_count: 1,
        }
    }

    /// Create expected context for an explicit chunk.
    #[must_use]
    pub fn chunk(
        object_id: [u8; 32],
        object_kind: RecoveryObjectKind,
        generation: u64,
        predecessor_generation: Option<u64>,
        chunk_index: u32,
        chunk_count: u32,
    ) -> Self {
        Self {
            object_id,
            object_kind,
            generation,
            predecessor_generation,
            chunk_index,
            chunk_count,
        }
    }

    /// Derive expected context directly from object metadata.
    #[must_use]
    pub fn from_metadata(metadata: &ObjectMetadata) -> Self {
        Self {
            object_id: metadata.object_id,
            object_kind: metadata.object_kind,
            generation: metadata.generation,
            predecessor_generation: metadata.predecessor_generation,
            chunk_index: metadata.chunk_index,
            chunk_count: metadata.chunk_count,
        }
    }

    /// Validate structural constraints on expected context.
    pub fn validate(&self) -> Result<(), RepresentationError> {
        if self.chunk_count == 0 {
            return Err(RepresentationError::InvalidMetadata {
                reason: "chunk_count must be greater than 0".into(),
            });
        }
        if self.chunk_index >= self.chunk_count {
            return Err(RepresentationError::InvalidMetadata {
                reason: format!(
                    "chunk_index {} must be less than chunk_count {}",
                    self.chunk_index, self.chunk_count
                ),
            });
        }
        if let Some(pred) = self.predecessor_generation {
            if pred >= self.generation {
                return Err(RepresentationError::InvalidMetadata {
                    reason: format!(
                        "predecessor_generation {} must be strictly less than generation {}",
                        pred, self.generation
                    ),
                });
            }
        }
        Ok(())
    }
}

/// Representation context containing all identifiers, generation hierarchy,
/// chunk bounds, and digests that form the authenticated Associated Data (AAD).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepresentationContext {
    pub representation_version: u32,
    pub object_id: [u8; 32],
    pub object_kind: RecoveryObjectKind,
    pub generation: u64,
    pub predecessor_generation: Option<u64>,
    pub epoch_timestamp_ms: u64,
    pub key_id: [u8; 8],
    pub chunk_index: u32,
    pub chunk_count: u32,
    pub uncompressed_bytes: u64,
    pub uncompressed_digest: [u8; 32],
    pub compressed_bytes: u64,
    pub compressed_digest: [u8; 32],
}

impl RepresentationContext {
    /// Validate structural constraints on representation context.
    pub fn validate(&self) -> Result<(), RepresentationError> {
        if self.representation_version != RECOVERY_FORMAT_VERSION {
            return Err(RepresentationError::UnsupportedVersion {
                version: self.representation_version,
            });
        }
        if self.chunk_count == 0 {
            return Err(RepresentationError::InvalidMetadata {
                reason: "chunk_count must be greater than 0".into(),
            });
        }
        if self.chunk_index >= self.chunk_count {
            return Err(RepresentationError::InvalidMetadata {
                reason: format!(
                    "chunk_index {} must be less than chunk_count {}",
                    self.chunk_index, self.chunk_count
                ),
            });
        }
        if let Some(pred) = self.predecessor_generation {
            if pred >= self.generation {
                return Err(RepresentationError::InvalidMetadata {
                    reason: format!(
                        "predecessor_generation {} must be strictly less than generation {}",
                        pred, self.generation
                    ),
                });
            }
        }
        Ok(())
    }

    /// Compute deterministic, collision-resistant Associated Authenticated Data (AAD).
    ///
    /// Encodes an explicit Option discriminant tag (0 for None, 1 for Some) for
    /// `predecessor_generation` to prevent aliasing between `None` and `Some(0)`.
    #[must_use]
    pub fn compute_canonical_aad(&self) -> Vec<u8> {
        let mut aad = Vec::with_capacity(161);
        aad.extend_from_slice(RECOVERY_AAD_DOMAIN);
        aad.extend_from_slice(&self.representation_version.to_be_bytes());
        aad.extend_from_slice(&self.object_id);
        aad.push(self.object_kind as u8);
        aad.extend_from_slice(&self.generation.to_be_bytes());
        match self.predecessor_generation {
            None => {
                aad.push(0u8);
            }
            Some(pred) => {
                aad.push(1u8);
                aad.extend_from_slice(&pred.to_be_bytes());
            }
        }
        aad.extend_from_slice(&self.epoch_timestamp_ms.to_be_bytes());
        aad.extend_from_slice(&self.key_id);
        aad.extend_from_slice(&self.chunk_index.to_be_bytes());
        aad.extend_from_slice(&self.chunk_count.to_be_bytes());
        aad.extend_from_slice(&self.uncompressed_bytes.to_be_bytes());
        aad.extend_from_slice(&self.uncompressed_digest);
        aad.extend_from_slice(&self.compressed_bytes.to_be_bytes());
        aad.extend_from_slice(&self.compressed_digest);
        aad
    }

    /// 32-byte chunk coordinate identity uniquely binding the representation format,
    /// object ID, generation, and chunk coordinates.
    ///
    /// Distinct from the whole-envelope `EncryptedRecoveryObject::representation_id`.
    #[must_use]
    pub fn chunk_coordinate_id(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(RECOVERY_CHUNK_COORDINATE_ID_DOMAIN);
        hasher.update(self.representation_version.to_be_bytes());
        hasher.update(self.object_id);
        hasher.update(self.generation.to_be_bytes());
        hasher.update(self.chunk_index.to_be_bytes());
        hasher.update(self.chunk_count.to_be_bytes());
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    /// Hex-encoded chunk coordinate identity.
    #[must_use]
    pub fn chunk_coordinate_id_hex(&self) -> String {
        hex::encode(self.chunk_coordinate_id())
    }

    /// Canonical immutable chunk filename embedding coordinate and chunk identity.
    #[must_use]
    pub fn chunk_filename(&self) -> String {
        let coord_short = &self.chunk_coordinate_id_hex()[..16];
        if self.chunk_count > 1 {
            format!(
                "obj-gen{:016x}-{}-coord{}-chunk{:04x}-of-{:04x}.ftrec",
                self.generation,
                hex::encode(self.object_id),
                coord_short,
                self.chunk_index,
                self.chunk_count,
            )
        } else {
            format!(
                "obj-gen{:016x}-{}-coord{}.ftrec",
                self.generation,
                hex::encode(self.object_id),
                coord_short,
            )
        }
    }
}

/// Configuration tuning for compression and safety limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepresentationConfig {
    pub zstd_level: i32,
    pub max_uncompressed_bytes: usize,
    pub max_compressed_bytes: usize,
    /// Optional expansion ratio ceiling. If `None` (the default), protection relies
    /// on the mandatory exact byte budget `max_uncompressed_bytes` (which is authenticated in AEAD AAD),
    /// avoiding arbitrary rejections of ordinary sparse terminal snapshots (e.g. repeated spaces/zeros).
    /// If `Some(ratio)`, both encoder and decoder enforce the ratio symmetrically.
    pub max_expansion_ratio: Option<usize>,
}

impl Default for RepresentationConfig {
    fn default() -> Self {
        Self {
            zstd_level: DEFAULT_ZSTD_LEVEL,
            max_uncompressed_bytes: DEFAULT_MAX_UNCOMPRESSED_BYTES,
            max_compressed_bytes: DEFAULT_MAX_COMPRESSED_BYTES,
            max_expansion_ratio: None,
        }
    }
}

// =============================================================================
// Representation Envelope
// =============================================================================

/// Header metadata for an encrypted recovery object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryObjectHeader {
    pub magic: [u8; 8],
    pub format_version: u32,
    pub nonce: [u8; NONCE_BYTES],
    pub ciphertext_digest: [u8; 32],
    pub context: RepresentationContext,
}

/// Authenticated, encrypted recovery object.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedRecoveryObject {
    pub header: RecoveryObjectHeader,
    pub ciphertext: Vec<u8>,
}

impl std::fmt::Debug for EncryptedRecoveryObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedRecoveryObject")
            .field("header", &self.header)
            .field("ciphertext_len", &self.ciphertext.len())
            .field("ciphertext", &"[ENCRYPTED_PAYLOAD]")
            .finish()
    }
}

impl EncryptedRecoveryObject {
    /// Canonical binary serialization for wire transmission, FEC protection, and disk publication.
    ///
    /// Framing:
    /// `MAGIC(8) || FORMAT_VERSION(4) || HEADER_JSON_LEN(4) || HEADER_JSON(N) || CIPHERTEXT_LEN(8) || CIPHERTEXT(M)`
    ///
    /// The entire byte slice returned by this method is what `snapshot_repair` ingests
    /// to generate systematic RaptorQ repair symbols.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RepresentationError> {
        if self.ciphertext.len() > MAX_ENVELOPE_BYTES.saturating_sub(24) {
            return Err(RepresentationError::EnvelopeTooLarge {
                size: self.ciphertext.len().saturating_add(24),
                limit: MAX_ENVELOPE_BYTES,
            });
        }
        let header_json = serde_json::to_vec(&self.header).map_err(|e| {
            RepresentationError::SerializationError {
                reason: e.to_string(),
            }
        })?;
        if header_json.len() > MAX_HEADER_JSON_BYTES {
            return Err(RepresentationError::HeaderTooLarge {
                size: header_json.len(),
                limit: MAX_HEADER_JSON_BYTES,
            });
        }
        let header_len = u32::try_from(header_json.len()).map_err(|_| {
            RepresentationError::SerializationError {
                reason: "header JSON exceeds u32::MAX".into(),
            }
        })?;
        let ciphertext_len = u64::try_from(self.ciphertext.len()).map_err(|_| {
            RepresentationError::SerializationError {
                reason: "ciphertext exceeds u64::MAX".into(),
            }
        })?;

        let total_len = self
            .ciphertext
            .len()
            .checked_add(header_json.len())
            .and_then(|len| len.checked_add(24))
            .filter(|len| *len <= MAX_ENVELOPE_BYTES)
            .ok_or_else(|| RepresentationError::EnvelopeTooLarge {
                size: self
                    .ciphertext
                    .len()
                    .saturating_add(header_json.len())
                    .saturating_add(24),
                limit: MAX_ENVELOPE_BYTES,
            })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total_len)
            .map_err(|_| RepresentationError::AllocationFailed {
                operation: "serialize encrypted recovery envelope",
            })?;
        bytes.extend_from_slice(&self.header.magic);
        bytes.extend_from_slice(&self.header.format_version.to_be_bytes());
        bytes.extend_from_slice(&header_len.to_be_bytes());
        bytes.extend_from_slice(&header_json);
        bytes.extend_from_slice(&ciphertext_len.to_be_bytes());
        bytes.extend_from_slice(&self.ciphertext);
        Ok(bytes)
    }

    /// Parse and validate canonical binary bytes.
    ///
    /// Explicitly bounds untrusted envelope, header JSON, and ciphertext lengths
    /// before performing any memory allocation or JSON deserialization.
    /// Strictly rejects non-canonical header JSON to guarantee deterministic representation identity.
    pub fn from_bytes(slice: &[u8]) -> Result<Self, RepresentationError> {
        if slice.len() < 24 {
            return Err(RepresentationError::MalformedRepresentation {
                reason: format!(
                    "slice too short for recovery object envelope: {} bytes",
                    slice.len()
                ),
            });
        }
        if slice.len() > MAX_ENVELOPE_BYTES {
            return Err(RepresentationError::EnvelopeTooLarge {
                size: slice.len(),
                limit: MAX_ENVELOPE_BYTES,
            });
        }
        let mut magic = [0u8; 8];
        magic.copy_from_slice(&slice[0..8]);
        if magic != RECOVERY_OBJECT_MAGIC {
            return Err(RepresentationError::InvalidMagic {
                expected: RECOVERY_OBJECT_MAGIC,
                actual: magic,
            });
        }
        let format_version = u32::from_be_bytes([slice[8], slice[9], slice[10], slice[11]]);
        if format_version != RECOVERY_FORMAT_VERSION {
            return Err(RepresentationError::UnsupportedVersion {
                version: format_version,
            });
        }
        let header_len = u32::from_be_bytes([slice[12], slice[13], slice[14], slice[15]]) as usize;
        if header_len > MAX_HEADER_JSON_BYTES {
            return Err(RepresentationError::HeaderTooLarge {
                size: header_len,
                limit: MAX_HEADER_JSON_BYTES,
            });
        }
        let header_end = 16 + header_len;
        if slice.len() < header_end + 8 {
            return Err(RepresentationError::MalformedRepresentation {
                reason: "slice truncated before ciphertext header".into(),
            });
        }
        let header_bytes = &slice[16..header_end];
        let header: RecoveryObjectHeader = serde_json::from_slice(header_bytes).map_err(|e| {
            RepresentationError::MalformedRepresentation {
                reason: format!("header JSON deserialization failed: {e}"),
            }
        })?;

        // Reject non-canonical envelope JSON to guarantee representation identity uniqueness
        let canonical_header_json =
            serde_json::to_vec(&header).map_err(|e| RepresentationError::SerializationError {
                reason: e.to_string(),
            })?;
        if header_bytes != canonical_header_json.as_slice() {
            return Err(RepresentationError::MalformedRepresentation {
                reason: "non-canonical header JSON encoding in recovery object envelope".into(),
            });
        }

        // Validate outer vs inner magic & version consistency
        if header.magic != magic {
            return Err(RepresentationError::InvalidMagic {
                expected: magic,
                actual: header.magic,
            });
        }
        if header.format_version != format_version {
            return Err(RepresentationError::UnsupportedVersion {
                version: header.format_version,
            });
        }
        if header.context.representation_version != format_version {
            return Err(RepresentationError::MalformedRepresentation {
                reason: format!(
                    "inner context representation_version ({}) does not match wire format_version ({})",
                    header.context.representation_version, format_version
                ),
            });
        }
        header.context.validate()?;

        let ciphertext_len = u64::from_be_bytes([
            slice[header_end],
            slice[header_end + 1],
            slice[header_end + 2],
            slice[header_end + 3],
            slice[header_end + 4],
            slice[header_end + 5],
            slice[header_end + 6],
            slice[header_end + 7],
        ]) as usize;

        if ciphertext_len > MAX_ENVELOPE_BYTES {
            return Err(RepresentationError::CiphertextTooLarge {
                size: ciphertext_len,
                limit: MAX_ENVELOPE_BYTES,
            });
        }

        let ciphertext_start = header_end + 8;
        let ciphertext_end = ciphertext_start
            .checked_add(ciphertext_len)
            .ok_or_else(|| RepresentationError::MalformedRepresentation {
                reason: "ciphertext length overflow".into(),
            })?;

        if slice.len() != ciphertext_end {
            return Err(RepresentationError::MalformedRepresentation {
                reason: format!(
                    "ciphertext length mismatch: header specifies {ciphertext_len} bytes, slice has {}",
                    slice.len().saturating_sub(ciphertext_start)
                ),
            });
        }

        let ciphertext = slice[ciphertext_start..ciphertext_end].to_vec();

        // Validate ciphertext digest
        let computed_digest: [u8; 32] = Sha256::digest(&ciphertext).into();
        if computed_digest != header.ciphertext_digest {
            return Err(RepresentationError::DigestMismatch {
                target: "ciphertext digest",
                expected: hex::encode(header.ciphertext_digest),
                actual: hex::encode(computed_digest),
            });
        }

        Ok(Self { header, ciphertext })
    }

    /// Protected representation ID defined as `SHA-256(exact envelope bytes)`.
    ///
    /// Distinct from the semantic `object_id`. This is the exact identifier authenticated
    /// and reconstructed by %570 (`snapshot_repair.rs`).
    pub fn representation_id(&self) -> Result<[u8; 32], RepresentationError> {
        let envelope_bytes = self.to_bytes()?;
        Ok(representation_id_from_envelope_bytes(&envelope_bytes))
    }

    /// Hex-encoded protected representation ID.
    pub fn representation_id_hex(&self) -> Result<String, RepresentationError> {
        self.representation_id().map(hex::encode)
    }

    /// Canonical immutable filename for disk publication:
    /// `obj-gen<gen>-<object_id>-rep<rep_id_16>-chunk<index>-of-<count>.ftrec`
    pub fn canonical_filename(&self) -> Result<String, RepresentationError> {
        let rep_id = self.representation_id_hex()?;
        let rep_short = &rep_id[..16];
        if self.header.context.chunk_count > 1 {
            Ok(format!(
                "obj-gen{:016x}-{}-rep{}-chunk{:04x}-of-{:04x}.ftrec",
                self.header.context.generation,
                hex::encode(self.header.context.object_id),
                rep_short,
                self.header.context.chunk_index,
                self.header.context.chunk_count,
            ))
        } else {
            Ok(format!(
                "obj-gen{:016x}-{}-rep{}.ftrec",
                self.header.context.generation,
                hex::encode(self.header.context.object_id),
                rep_short,
            ))
        }
    }
}

/// Compute the protected representation ID directly from serialized envelope bytes.
///
/// Under the RFC 6330 FEC model and whole-mux recovery architecture, the representation ID
/// is defined strictly as `SHA-256(exact envelope bytes)`, distinct from the semantic object ID.
#[must_use]
pub fn representation_id_from_envelope_bytes(envelope_bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(envelope_bytes).into()
}

/// Result of a verified decoding operation, containing both the recovered semantic
/// plaintext and the fully authenticated representation context.
///
/// Secret plaintext is held in `zeroize::Zeroizing` to guarantee zeroization on drop.
/// `Debug` implementation redacts all plaintext bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct DecodedRepresentation {
    pub plaintext: Zeroizing<Vec<u8>>,
    pub context: RepresentationContext,
}

impl DecodedRepresentation {
    /// Consume the wrapper and return the recovered plaintext with memory zeroization on drop.
    #[must_use]
    pub fn into_plaintext(self) -> Zeroizing<Vec<u8>> {
        self.plaintext
    }

    /// Reference to the recovered plaintext.
    #[must_use]
    pub fn plaintext(&self) -> &[u8] {
        &self.plaintext
    }

    /// Reference to the verified representation context.
    #[must_use]
    pub fn context(&self) -> &RepresentationContext {
        &self.context
    }
}

impl std::fmt::Debug for DecodedRepresentation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedRepresentation")
            .field("context", &self.context)
            .field("plaintext_len", &self.plaintext.len())
            .field("plaintext", &"[REDACTED]")
            .finish()
    }
}

// =============================================================================
// Bounded Streaming Compression (Output Budget Enforced During Production)
// =============================================================================

/// Internal bounded streaming writer that halts and errors the moment
/// output exceeds the allocated budget during compression.
struct BoundedWriter {
    buffer: Zeroizing<Vec<u8>>,
    limit: usize,
    exceeded: bool,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            buffer: Zeroizing::new(Vec::new()),
            limit,
            exceeded: false,
        }
    }
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let new_len = self.buffer.len().checked_add(buf.len());
        if new_len.is_none_or(|len| len > self.limit) {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "compressed output exceeded budget limit while producing",
            ));
        }
        // Avoid Vec's geometric capacity growth beyond the admitted output size.
        self.buffer
            .try_reserve_exact(buf.len())
            .map_err(std::io::Error::other)?;
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Compress plaintext while strictly enforcing output budget during production
/// rather than posthoc allocation.
fn compress_bounded(
    plaintext: &[u8],
    level: i32,
    limit: usize,
) -> Result<Zeroizing<Vec<u8>>, RepresentationError> {
    let writer = BoundedWriter::new(limit);
    let mut encoder =
        zstd::Encoder::new(writer, level).map_err(|e| RepresentationError::CompressionFailed {
            reason: e.to_string(),
        })?;

    if let Err(e) = encoder.write_all(plaintext) {
        if encoder.get_ref().exceeded {
            return Err(RepresentationError::CompressedLimitExceeded {
                size: limit.saturating_add(1),
                limit,
            });
        }
        return Err(RepresentationError::CompressionFailed {
            reason: e.to_string(),
        });
    }

    // Keep ownership of the writer when final frame emission fails: otherwise a
    // limit hit in finish() becomes indistinguishable from a codec failure.
    let writer = match encoder.try_finish() {
        Ok(writer) => writer,
        Err((encoder, error)) => {
            return Err(if encoder.get_ref().exceeded {
                RepresentationError::CompressedLimitExceeded {
                    size: limit.saturating_add(1),
                    limit,
                }
            } else {
                RepresentationError::CompressionFailed {
                    reason: error.to_string(),
                }
            });
        }
    };

    if writer.exceeded || writer.buffer.len() > limit {
        return Err(RepresentationError::CompressedLimitExceeded {
            size: writer.buffer.len(),
            limit,
        });
    }

    Ok(writer.buffer)
}

// =============================================================================
// Encode & Decode Algorithms
// =============================================================================

/// Compress semantic bytes with bounded streaming zstd, then AEAD-encrypt with authenticated object identity AAD.
pub fn encode_recovery_object(
    plaintext: &[u8],
    metadata: ObjectMetadata,
    key: &RecoveryKey,
    config_opt: Option<&RepresentationConfig>,
) -> Result<EncryptedRecoveryObject, RepresentationError> {
    let default_config = RepresentationConfig::default();
    let config = config_opt.unwrap_or(&default_config);

    // 0. Validate input metadata
    metadata.validate()?;

    // 1. Guard uncompressed input length
    if plaintext.len() > config.max_uncompressed_bytes {
        return Err(RepresentationError::UncompressedLimitExceeded {
            size: plaintext.len(),
            limit: config.max_uncompressed_bytes,
        });
    }
    let uncompressed_bytes = plaintext.len() as u64;
    let uncompressed_digest: [u8; 32] = Sha256::digest(plaintext).into();

    // 2. Compress via streaming zstd with strict budget enforcement during production
    let compressed: Zeroizing<Vec<u8>> =
        compress_bounded(plaintext, config.zstd_level, config.max_compressed_bytes)?;

    // Symmetric expansion ratio check on encode (if configured)
    if let Some(ratio) = config.max_expansion_ratio {
        let max_allowed = (compressed.len() as u64).saturating_mul(ratio as u64);
        if uncompressed_bytes > max_allowed {
            return Err(RepresentationError::ExpansionRatioExceeded {
                uncompressed: plaintext.len(),
                compressed: compressed.len(),
                limit: ratio,
            });
        }
    }

    let compressed_bytes = compressed.len() as u64;
    let compressed_digest: [u8; 32] = Sha256::digest(&compressed).into();

    // 3. Assemble representation context
    let context = RepresentationContext {
        representation_version: RECOVERY_FORMAT_VERSION,
        object_id: metadata.object_id,
        object_kind: metadata.object_kind,
        generation: metadata.generation,
        predecessor_generation: metadata.predecessor_generation,
        epoch_timestamp_ms: metadata.epoch_timestamp_ms,
        key_id: key.key_id(),
        chunk_index: metadata.chunk_index,
        chunk_count: metadata.chunk_count,
        uncompressed_bytes,
        uncompressed_digest,
        compressed_bytes,
        compressed_digest,
    };

    // 4. Generate random 24-byte nonce using system entropy (fails closed on entropy error)
    let mut nonce = [0u8; NONCE_BYTES];
    let mut rng = SysRng;
    rng.try_fill_bytes(&mut nonce)
        .map_err(|e| RepresentationError::EntropyUnavailable {
            reason: format!(
                "failed to obtain {} bytes of entropy for nonce: {e}",
                NONCE_BYTES
            ),
        })?;

    // 5. Compute AAD and encrypt via XChaCha20-Poly1305
    let aad = context.compute_canonical_aad();
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_bytes()).map_err(|e| {
        RepresentationError::KeyError {
            reason: e.to_string(),
        }
    })?;
    let xnonce = XNonce::from(nonce);
    let payload = Payload {
        msg: &compressed,
        aad: &aad,
    };
    let ciphertext =
        cipher
            .encrypt(&xnonce, payload)
            .map_err(|e| RepresentationError::EncryptionFailed {
                reason: e.to_string(),
            })?;

    let ciphertext_digest: [u8; 32] = Sha256::digest(&ciphertext).into();

    let header = RecoveryObjectHeader {
        magic: RECOVERY_OBJECT_MAGIC,
        format_version: RECOVERY_FORMAT_VERSION,
        nonce,
        ciphertext_digest,
        context,
    };

    Ok(EncryptedRecoveryObject { header, ciphertext })
}

/// Decrypt an encrypted recovery object, verifying expected context, AAD, and digest bounds,
/// then decompress with bounded expansion to recover original semantic bytes.
///
/// Returns `DecodedRepresentation` containing both the recovered semantic plaintext
/// (held in zeroizing memory) and the authenticated context.
pub fn decode_recovery_object(
    object: &EncryptedRecoveryObject,
    expected: &ExpectedContext,
    key: &RecoveryKey,
    config_opt: Option<&RepresentationConfig>,
) -> Result<DecodedRepresentation, RepresentationError> {
    let default_config = RepresentationConfig::default();
    let config = config_opt.unwrap_or(&default_config);

    // 0. Validate expected context constraints
    expected.validate()?;

    // 1. Guard against oversized ciphertext/envelope and budget limits BEFORE hashing or decrypt allocation
    // (critical defense against Direct struct / Serde paths bypassing from_bytes)
    if object.ciphertext.len() > MAX_ENVELOPE_BYTES {
        return Err(RepresentationError::CiphertextTooLarge {
            size: object.ciphertext.len(),
            limit: MAX_ENVELOPE_BYTES,
        });
    }

    if object.ciphertext.len() < AEAD_TAG_BYTES {
        return Err(RepresentationError::MalformedRepresentation {
            reason: format!(
                "ciphertext length {} is smaller than minimum AEAD tag size {}",
                object.ciphertext.len(),
                AEAD_TAG_BYTES
            ),
        });
    }

    let max_allowed_ciphertext = config.max_compressed_bytes.saturating_add(AEAD_TAG_BYTES);
    if object.ciphertext.len() > max_allowed_ciphertext {
        return Err(RepresentationError::CiphertextTooLarge {
            size: object.ciphertext.len(),
            limit: max_allowed_ciphertext,
        });
    }

    if object.header.context.compressed_bytes > config.max_compressed_bytes as u64 {
        return Err(RepresentationError::CompressedLimitExceeded {
            size: object.header.context.compressed_bytes as usize,
            limit: config.max_compressed_bytes,
        });
    }

    if object.header.context.uncompressed_bytes > config.max_uncompressed_bytes as u64 {
        return Err(RepresentationError::UncompressedLimitExceeded {
            size: object.header.context.uncompressed_bytes as usize,
            limit: config.max_uncompressed_bytes,
        });
    }

    // 2. Validate envelope magic & format version consistency
    if object.header.magic != RECOVERY_OBJECT_MAGIC {
        return Err(RepresentationError::InvalidMagic {
            expected: RECOVERY_OBJECT_MAGIC,
            actual: object.header.magic,
        });
    }
    if object.header.format_version != RECOVERY_FORMAT_VERSION {
        return Err(RepresentationError::UnsupportedVersion {
            version: object.header.format_version,
        });
    }
    if object.header.context.representation_version != object.header.format_version {
        return Err(RepresentationError::MalformedRepresentation {
            reason: format!(
                "inner context representation_version ({}) does not match header format_version ({})",
                object.header.context.representation_version, object.header.format_version
            ),
        });
    }
    object.header.context.validate()?;

    // 3. Validate expected context matches representation header
    if object.header.context.object_id != expected.object_id {
        return Err(RepresentationError::ContextMismatch {
            field: "object_id",
            expected: hex::encode(expected.object_id),
            actual: hex::encode(object.header.context.object_id),
        });
    }
    if object.header.context.object_kind != expected.object_kind {
        return Err(RepresentationError::ContextMismatch {
            field: "object_kind",
            expected: format!("{:?}", expected.object_kind),
            actual: format!("{:?}", object.header.context.object_kind),
        });
    }
    if object.header.context.generation != expected.generation {
        return Err(RepresentationError::ContextMismatch {
            field: "generation",
            expected: expected.generation.to_string(),
            actual: object.header.context.generation.to_string(),
        });
    }
    if object.header.context.predecessor_generation != expected.predecessor_generation {
        return Err(RepresentationError::ContextMismatch {
            field: "predecessor_generation",
            expected: format!("{:?}", expected.predecessor_generation),
            actual: format!("{:?}", object.header.context.predecessor_generation),
        });
    }
    if object.header.context.chunk_index != expected.chunk_index {
        return Err(RepresentationError::ContextMismatch {
            field: "chunk_index",
            expected: expected.chunk_index.to_string(),
            actual: object.header.context.chunk_index.to_string(),
        });
    }
    if object.header.context.chunk_count != expected.chunk_count {
        return Err(RepresentationError::ContextMismatch {
            field: "chunk_count",
            expected: expected.chunk_count.to_string(),
            actual: object.header.context.chunk_count.to_string(),
        });
    }

    // 4. Validate key ID matches
    if object.header.context.key_id != key.key_id() {
        return Err(RepresentationError::KeyError {
            reason: format!(
                "key ID mismatch: object requires {}, provided key is {}",
                hex::encode(object.header.context.key_id),
                key.key_id_hex()
            ),
        });
    }

    // 5. Verify ciphertext digest
    let actual_ciphertext_digest: [u8; 32] = Sha256::digest(&object.ciphertext).into();
    if actual_ciphertext_digest != object.header.ciphertext_digest {
        return Err(RepresentationError::DigestMismatch {
            target: "ciphertext digest",
            expected: hex::encode(object.header.ciphertext_digest),
            actual: hex::encode(actual_ciphertext_digest),
        });
    }

    // 6. Decrypt via XChaCha20-Poly1305 with authenticated AAD
    let aad = object.header.context.compute_canonical_aad();
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_bytes()).map_err(|e| {
        RepresentationError::KeyError {
            reason: e.to_string(),
        }
    })?;
    let xnonce = XNonce::from(object.header.nonce);
    let payload = Payload {
        msg: &object.ciphertext,
        aad: &aad,
    };
    let compressed_bytes =
        cipher
            .decrypt(&xnonce, payload)
            .map_err(|e| RepresentationError::DecryptionFailed {
                reason: e.to_string(),
            })?;
    let compressed: Zeroizing<Vec<u8>> = Zeroizing::new(compressed_bytes);

    // 7. Verify compressed bytes digest
    let actual_compressed_digest: [u8; 32] = Sha256::digest(&compressed).into();
    if actual_compressed_digest != object.header.context.compressed_digest {
        return Err(RepresentationError::DigestMismatch {
            target: "compressed payload digest",
            expected: hex::encode(object.header.context.compressed_digest),
            actual: hex::encode(actual_compressed_digest),
        });
    }
    if compressed.len() as u64 != object.header.context.compressed_bytes {
        return Err(RepresentationError::MalformedRepresentation {
            reason: format!(
                "compressed length mismatch: header claims {} bytes, decrypted {} bytes",
                object.header.context.compressed_bytes,
                compressed.len()
            ),
        });
    }

    // 8. Bounded decompression guards (mandatory exact byte ceilings enforce strict limits)
    let expected_uncompressed =
        usize::try_from(object.header.context.uncompressed_bytes).map_err(|_| {
            RepresentationError::UncompressedLimitExceeded {
                size: usize::MAX,
                limit: config.max_uncompressed_bytes,
            }
        })?;

    if expected_uncompressed > config.max_uncompressed_bytes {
        return Err(RepresentationError::UncompressedLimitExceeded {
            size: expected_uncompressed,
            limit: config.max_uncompressed_bytes,
        });
    }

    // Symmetric expansion ratio check on decode (if configured)
    if let Some(ratio) = config.max_expansion_ratio {
        let max_allowed = (compressed.len() as u64).saturating_mul(ratio as u64);
        if object.header.context.uncompressed_bytes > max_allowed {
            return Err(RepresentationError::ExpansionRatioExceeded {
                uncompressed: expected_uncompressed,
                compressed: compressed.len(),
                limit: ratio,
            });
        }
    }

    // 9. Decompress up to exact expected uncompressed length
    // Decode directly into owned, zeroizing storage. The convenience bulk API
    // allocates an ordinary Vec internally and can drop partial plaintext on an
    // error before the caller has a chance to wrap it.
    let mut decompressed = Zeroizing::new(Vec::new());
    decompressed
        .try_reserve_exact(expected_uncompressed)
        .map_err(|_| RepresentationError::AllocationFailed {
            operation: "decompression",
        })?;
    decompressed.resize(expected_uncompressed, 0);
    let mut decoder =
        zstd::bulk::Decompressor::new().map_err(|e| RepresentationError::DecompressionFailed {
            reason: e.to_string(),
        })?;
    let decoded_len = decoder
        .decompress_to_buffer(&compressed, decompressed.as_mut_slice())
        .map_err(|e| RepresentationError::DecompressionFailed {
            reason: e.to_string(),
        })?;
    decompressed.truncate(decoded_len);

    if decompressed.len() != expected_uncompressed {
        return Err(RepresentationError::DecompressedLengthMismatch {
            expected: expected_uncompressed,
            actual: decompressed.len(),
        });
    }

    // 10. Verify uncompressed digest against recovered plaintext
    let actual_uncompressed_digest: [u8; 32] = Sha256::digest(&decompressed).into();
    if actual_uncompressed_digest != object.header.context.uncompressed_digest {
        return Err(RepresentationError::DigestMismatch {
            target: "uncompressed plaintext digest",
            expected: hex::encode(object.header.context.uncompressed_digest),
            actual: hex::encode(actual_uncompressed_digest),
        });
    }

    Ok(DecodedRepresentation {
        plaintext: decompressed,
        context: object.header.context.clone(),
    })
}

/// Convenience helper to decode directly into raw plaintext bytes wrapped in `zeroize::Zeroizing`.
pub fn decode_recovery_object_bytes(
    object: &EncryptedRecoveryObject,
    expected: &ExpectedContext,
    key: &RecoveryKey,
    config_opt: Option<&RepresentationConfig>,
) -> Result<Zeroizing<Vec<u8>>, RepresentationError> {
    decode_recovery_object(object, expected, key, config_opt).map(|res| res.plaintext)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cipher_key_schedule_requires_zeroization_feature() {
        fn require_zeroizing_drop<T: zeroize::ZeroizeOnDrop>() {}
        require_zeroizing_drop::<XChaCha20Poly1305>();
        require_zeroizing_drop::<Sha256>();
    }

    #[test]
    fn recovery_key_wrappers_roundtrip_with_independent_authorities() {
        let key = RecoveryKey::from_bytes([0x63; KEY_BYTES]).unwrap();
        let first = RecoveryWrappingKey::from_bytes([0x17; KEY_BYTES]).unwrap();
        let second = RecoveryWrappingKey::from_bytes([0x28; KEY_BYTES]).unwrap();
        let context = RecoveryWrapContext {
            namespace_id: [0x41; 32],
            policy_id: [0x52; 32],
        };
        for authority in [&first, &second] {
            let expected = ExpectedRecoveryWrapContext {
                namespace_id: context.namespace_id,
                policy_id: context.policy_id,
                recovery_key_id: key.key_id(),
                authority_id: authority.authority_id(),
            };
            let wrapped = wrap_recovery_key(&key, authority, &context).unwrap();
            let bytes = wrapped.to_bytes();
            assert_eq!(bytes.len(), WRAPPED_RECOVERY_KEY_BYTES);
            assert!(!bytes.windows(KEY_BYTES).any(|part| part == key.as_bytes()));
            let decoded = WrappedRecoveryKey::from_bytes(&bytes).unwrap();
            assert_eq!(decoded, wrapped);
            let reopened = unwrap_recovery_key(&decoded, authority, &expected).unwrap();
            assert_eq!(reopened.as_bytes(), key.as_bytes());
            assert_eq!(reopened.key_id(), key.key_id());
        }
        assert_ne!(first.authority_id(), second.authority_id());
        assert!(!format!("{first:?}").contains(&hex::encode([0x17; KEY_BYTES])));
    }

    #[test]
    fn recovery_key_wrapper_rejects_tampering_and_wrong_expected_identity() {
        let key = RecoveryKey::from_bytes([0x63; KEY_BYTES]).unwrap();
        let authority = RecoveryWrappingKey::from_bytes([0x17; KEY_BYTES]).unwrap();
        let foreign = RecoveryWrappingKey::from_bytes([0x28; KEY_BYTES]).unwrap();
        let context = RecoveryWrapContext {
            namespace_id: [0x41; 32],
            policy_id: [0x52; 32],
        };
        let expected = ExpectedRecoveryWrapContext {
            namespace_id: context.namespace_id,
            policy_id: context.policy_id,
            recovery_key_id: key.key_id(),
            authority_id: authority.authority_id(),
        };
        let wrapped = wrap_recovery_key(&key, &authority, &context).unwrap();
        assert!(matches!(
            unwrap_recovery_key(&wrapped, &foreign, &expected),
            Err(RecoveryWrapError::KeyMismatch {
                field: "authority_id"
            })
        ));
        for field in [
            "namespace_id",
            "policy_id",
            "recovery_key_id",
            "authority_id",
        ] {
            let mut wrong = expected;
            match field {
                "namespace_id" => wrong.namespace_id[0] ^= 1,
                "policy_id" => wrong.policy_id[0] ^= 1,
                "recovery_key_id" => wrong.recovery_key_id[0] ^= 1,
                _ => wrong.authority_id[0] ^= 1,
            }
            assert!(unwrap_recovery_key(&wrapped, &authority, &wrong).is_err());
        }
        for offset in [116, 140, WRAPPED_RECOVERY_KEY_BYTES - 1] {
            let mut corrupted = wrapped.to_bytes();
            corrupted[offset] ^= 1;
            let decoded = WrappedRecoveryKey::from_bytes(&corrupted).unwrap();
            assert!(matches!(
                unwrap_recovery_key(&decoded, &authority, &expected),
                Err(RecoveryWrapError::AuthenticationFailed)
            ));
        }
        // Even if an attacker changes both claimed and expected scope, the
        // original AEAD tag still binds the producer's namespace and policy.
        let mut changed = wrapped.to_bytes();
        changed[52] ^= 1;
        let changed = WrappedRecoveryKey::from_bytes(&changed).unwrap();
        let mut changed_expected = expected;
        changed_expected.namespace_id[0] ^= 1;
        assert!(matches!(
            unwrap_recovery_key(&changed, &authority, &changed_expected),
            Err(RecoveryWrapError::AuthenticationFailed)
        ));
    }

    #[test]
    fn recovery_key_wrapper_fixed_framing_rejects_size_version_and_zero_authority() {
        assert!(matches!(
            RecoveryWrappingKey::from_bytes([0; KEY_BYTES]),
            Err(RecoveryWrapError::InvalidKey)
        ));
        for size in [
            0,
            WRAPPED_RECOVERY_KEY_BYTES - 1,
            WRAPPED_RECOVERY_KEY_BYTES + 1,
        ] {
            assert!(matches!(
                WrappedRecoveryKey::from_bytes(&vec![0; size]),
                Err(RecoveryWrapError::InvalidEnvelope)
            ));
        }
        let mut bytes = [0; WRAPPED_RECOVERY_KEY_BYTES];
        bytes[..8].copy_from_slice(&RECOVERY_WRAP_MAGIC);
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(
            WrappedRecoveryKey::from_bytes(&bytes),
            Err(RecoveryWrapError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn repair_authentication_key_is_stable_and_separate_from_encryption_key() {
        let first = RecoveryKey::from_bytes([7; KEY_BYTES]).unwrap();
        let reopened = RecoveryKey::from_bytes([7; KEY_BYTES]).unwrap();
        let other = RecoveryKey::from_bytes([8; KEY_BYTES]).unwrap();
        let derived = first.derive_repair_authentication_key().unwrap();
        assert_eq!(
            *derived,
            *reopened.derive_repair_authentication_key().unwrap()
        );
        assert_ne!(*derived, *first.as_bytes());
        assert_ne!(*derived, *other.derive_repair_authentication_key().unwrap());
    }

    #[test]
    fn final_frame_emission_preserves_compression_limit_error() {
        assert!(matches!(
            compress_bounded(b"x", DEFAULT_ZSTD_LEVEL, 0),
            Err(RepresentationError::CompressedLimitExceeded { limit: 0, .. })
        ));
    }

    #[test]
    fn bounded_writer_rejects_growth_without_appending_partial_output() {
        let mut writer = BoundedWriter::new(3);
        writer.write_all(b"ab").expect("first write fits");
        assert!(writer.write_all(b"cd").is_err());
        assert!(writer.exceeded);
        assert_eq!(writer.buffer.as_slice(), b"ab");
        assert!(writer.buffer.capacity() <= 3);
    }

    fn sample_metadata(object_id_byte: u8, generation: u64) -> ObjectMetadata {
        let mut object_id = [0u8; 32];
        object_id[0] = object_id_byte;
        ObjectMetadata::single(
            object_id,
            RecoveryObjectKind::WholeMuxImage,
            generation,
            if generation > 0 {
                Some(generation - 1)
            } else {
                None
            },
            1747371642000,
        )
    }

    fn sample_chunk_metadata(
        object_id_byte: u8,
        generation: u64,
        chunk_index: u32,
        chunk_count: u32,
    ) -> ObjectMetadata {
        let mut object_id = [0u8; 32];
        object_id[0] = object_id_byte;
        ObjectMetadata::chunk(
            object_id,
            RecoveryObjectKind::ScrollbackSegment,
            generation,
            if generation > 0 {
                Some(generation - 1)
            } else {
                None
            },
            1747371642000,
            chunk_index,
            chunk_count,
        )
    }

    #[test]
    fn serialized_envelope_limit_includes_header_and_framing() {
        let key = RecoveryKey::from_bytes([7; KEY_BYTES]).unwrap();
        let mut object =
            encode_recovery_object(b"small", sample_metadata(1, 1), &key, None).unwrap();
        object.ciphertext.resize(MAX_ENVELOPE_BYTES - 24, 0);
        assert!(matches!(
            object.to_bytes(),
            Err(RepresentationError::EnvelopeTooLarge {
                limit: MAX_ENVELOPE_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn positive_encode_decode_roundtrip() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"{\"pane_id\": 1, \"scrollback\": \"echo hello world\\n\"}";
        let metadata = sample_metadata(42, 1);
        let expected = ExpectedContext::from_metadata(&metadata);

        let encrypted = encode_recovery_object(payload, metadata.clone(), &key, None)
            .expect("encoding must succeed");

        assert_eq!(encrypted.header.magic, RECOVERY_OBJECT_MAGIC);
        assert_eq!(encrypted.header.format_version, RECOVERY_FORMAT_VERSION);
        assert_eq!(encrypted.header.context.object_id, metadata.object_id);
        assert_eq!(
            encrypted.header.context.uncompressed_bytes,
            payload.len() as u64
        );

        let decoded = decode_recovery_object(&encrypted, &expected, &key, None)
            .expect("decoding must succeed");

        assert_eq!(decoded.plaintext(), payload);
        assert_eq!(decoded.context().object_id, metadata.object_id);
        assert_eq!(decoded.context().generation, 1);
    }

    #[test]
    fn positive_wire_serialization_roundtrip() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"critical terminal state checkpoint";
        let metadata = sample_metadata(7, 1);
        let expected = ExpectedContext::from_metadata(&metadata);

        let encrypted =
            encode_recovery_object(payload, metadata, &key, None).expect("encoding must succeed");

        let wire_bytes = encrypted.to_bytes().expect("serialization must succeed");
        let parsed =
            EncryptedRecoveryObject::from_bytes(&wire_bytes).expect("deserialization must succeed");

        assert_eq!(parsed.header, encrypted.header);
        assert_eq!(parsed.ciphertext, encrypted.ciphertext);

        let decoded = decode_recovery_object(&parsed, &expected, &key, None)
            .expect("decoding from parsed wire bytes must succeed");
        assert_eq!(decoded.into_plaintext().as_slice(), payload);
    }

    #[test]
    fn positive_default_policy_large_repetitive_sparse_terminal_roundtrip() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        // 2 MiB of sparse spaces (typical terminal screen / scrollback with blank areas)
        let sparse_spaces = vec![b' '; 2 * 1024 * 1024];
        let metadata = sample_metadata(77, 20);
        let expected = ExpectedContext::from_metadata(&metadata);

        // Default policy must encode and decode cleanly without arbitrary low expansion ratio rejection
        let enc = encode_recovery_object(&sparse_spaces, metadata, &key, None)
            .expect("default policy encode must succeed on sparse terminal data");

        // Verify high compressibility (> 500x ratio)
        let ratio = sparse_spaces.len() / enc.ciphertext.len();
        assert!(ratio > 500, "expected >500x ratio, got {}x", ratio);

        let decoded = decode_recovery_object(&enc, &expected, &key, None)
            .expect("default policy decode must succeed on sparse terminal data");
        assert_eq!(decoded.plaintext(), sparse_spaces.as_slice());
    }

    #[test]
    fn negative_symmetric_expansion_ratio_rejected_by_both_encode_and_decode() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let zeros = vec![0u8; 100_000];
        let metadata = sample_metadata(4, 1);
        let expected = ExpectedContext::from_metadata(&metadata);

        let strict_config = RepresentationConfig {
            max_expansion_ratio: Some(50),
            ..Default::default()
        };

        // 1. Encoder rejects under strict expansion ratio
        let enc_err = encode_recovery_object(&zeros, metadata.clone(), &key, Some(&strict_config))
            .unwrap_err();
        assert!(matches!(
            enc_err,
            RepresentationError::ExpansionRatioExceeded { .. }
        ));

        // 2. Decoder also rejects under strict expansion ratio
        let enc = encode_recovery_object(&zeros, metadata, &key, None).unwrap();
        let dec_err =
            decode_recovery_object(&enc, &expected, &key, Some(&strict_config)).unwrap_err();
        assert!(matches!(
            dec_err,
            RepresentationError::ExpansionRatioExceeded { .. }
        ));
    }

    #[test]
    fn negative_compressed_limit_enforced_during_production() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let mut rng = SysRng;
        let mut noise = vec![0u8; 10_000];
        rng.try_fill_bytes(&mut noise).unwrap();
        let metadata = sample_metadata(5, 1);

        // Budget smaller than compressed output: must fail during production
        let strict_config = RepresentationConfig {
            max_compressed_bytes: 50,
            ..Default::default()
        };

        let err = encode_recovery_object(&noise, metadata, &key, Some(&strict_config)).unwrap_err();
        assert!(matches!(
            err,
            RepresentationError::CompressedLimitExceeded { limit: 50, .. }
        ));
    }

    #[test]
    fn negative_decode_bypassing_from_bytes_checks_ciphertext_cap_before_decrypt() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let metadata = sample_metadata(10, 1);
        let expected = ExpectedContext::from_metadata(&metadata);

        // 1. Direct struct with ciphertext > MAX_ENVELOPE_BYTES
        let huge_obj = EncryptedRecoveryObject {
            header: RecoveryObjectHeader {
                magic: RECOVERY_OBJECT_MAGIC,
                format_version: RECOVERY_FORMAT_VERSION,
                nonce: [0u8; 24],
                ciphertext_digest: [0u8; 32],
                context: RepresentationContext {
                    representation_version: RECOVERY_FORMAT_VERSION,
                    object_id: metadata.object_id,
                    object_kind: metadata.object_kind,
                    generation: metadata.generation,
                    predecessor_generation: metadata.predecessor_generation,
                    epoch_timestamp_ms: metadata.epoch_timestamp_ms,
                    key_id: key.key_id(),
                    chunk_index: 0,
                    chunk_count: 1,
                    uncompressed_bytes: 10,
                    uncompressed_digest: [0u8; 32],
                    compressed_bytes: 10,
                    compressed_digest: [0u8; 32],
                },
            },
            ciphertext: vec![0u8; MAX_ENVELOPE_BYTES + 1],
        };
        let err1 = decode_recovery_object(&huge_obj, &expected, &key, None).unwrap_err();
        assert!(matches!(
            err1,
            RepresentationError::CiphertextTooLarge {
                size: s,
                limit: MAX_ENVELOPE_BYTES,
            } if s == MAX_ENVELOPE_BYTES + 1
        ));

        // 2. Direct struct with ciphertext > config.max_compressed_bytes + AEAD_TAG_BYTES
        let config = RepresentationConfig {
            max_compressed_bytes: 64,
            ..Default::default()
        };
        let over_budget_obj = EncryptedRecoveryObject {
            header: huge_obj.header.clone(),
            ciphertext: vec![0u8; 64 + AEAD_TAG_BYTES + 1],
        };
        let err2 =
            decode_recovery_object(&over_budget_obj, &expected, &key, Some(&config)).unwrap_err();
        assert!(matches!(
            err2,
            RepresentationError::CiphertextTooLarge { .. }
        ));

        // 3. Direct struct with ciphertext smaller than AEAD tag size
        let tiny_obj = EncryptedRecoveryObject {
            header: huge_obj.header.clone(),
            ciphertext: vec![0u8; 5],
        };
        let err3 = decode_recovery_object(&tiny_obj, &expected, &key, None).unwrap_err();
        assert!(matches!(
            err3,
            RepresentationError::MalformedRepresentation { .. }
        ));

        // 4. Header claims compressed_bytes > config.max_compressed_bytes
        let mut over_compressed_meta_obj = huge_obj.clone();
        over_compressed_meta_obj.ciphertext = vec![0u8; 32];
        over_compressed_meta_obj.header.context.compressed_bytes = 200;
        let err4 =
            decode_recovery_object(&over_compressed_meta_obj, &expected, &key, Some(&config))
                .unwrap_err();
        assert!(matches!(
            err4,
            RepresentationError::CompressedLimitExceeded { limit: 64, .. }
        ));
    }

    #[test]
    fn negative_aad_predecessor_none_vs_some_zero_tag_substitution_fails() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"state with none vs some zero predecessor";

        // Case A: Encoded with predecessor_generation: None
        let mut meta_none = sample_metadata(12, 1);
        meta_none.predecessor_generation = None;
        let expected_none = ExpectedContext::from_metadata(&meta_none);

        let enc_none = encode_recovery_object(payload, meta_none.clone(), &key, None)
            .expect("encode with None predecessor succeeds");

        // Verify clean decode with matching None context
        let dec_ok = decode_recovery_object(&enc_none, &expected_none, &key, None).unwrap();
        assert_eq!(dec_ok.plaintext(), payload);

        // Substitute predecessor_generation: Some(0) into header and expected context
        let mut tampered_none_to_some = enc_none.clone();
        tampered_none_to_some.header.context.predecessor_generation = Some(0);
        let mut expected_some_zero = expected_none.clone();
        expected_some_zero.predecessor_generation = Some(0);

        // Because AAD encodes discriminant 0 for None vs 1 for Some, Poly1305 MUST reject!
        let err_sub =
            decode_recovery_object(&tampered_none_to_some, &expected_some_zero, &key, None)
                .unwrap_err();
        assert!(
            matches!(err_sub, RepresentationError::DecryptionFailed { .. }),
            "expected DecryptionFailed on None -> Some(0) tag substitution, got: {:?}",
            err_sub
        );

        // Case B: Encoded with predecessor_generation: Some(0)
        let mut meta_some = sample_metadata(13, 1);
        meta_some.predecessor_generation = Some(0);
        let expected_some = ExpectedContext::from_metadata(&meta_some);

        let enc_some = encode_recovery_object(payload, meta_some.clone(), &key, None)
            .expect("encode with Some(0) predecessor succeeds");

        // Substitute predecessor_generation: None into header and expected context
        let mut tampered_some_to_none = enc_some.clone();
        tampered_some_to_none.header.context.predecessor_generation = None;
        let mut expected_none_sub = expected_some.clone();
        expected_none_sub.predecessor_generation = None;

        let err_sub_b =
            decode_recovery_object(&tampered_some_to_none, &expected_none_sub, &key, None)
                .unwrap_err();
        assert!(
            matches!(err_sub_b, RepresentationError::DecryptionFailed { .. }),
            "expected DecryptionFailed on Some(0) -> None tag substitution, got: {:?}",
            err_sub_b
        );
    }

    #[test]
    fn negative_invalid_metadata_and_context_bounds_rejected() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"test payload for bounds validation";

        // 1. chunk_count == 0 in ObjectMetadata
        let mut bad_meta1 = sample_metadata(14, 1);
        bad_meta1.chunk_count = 0;
        let err1 = encode_recovery_object(payload, bad_meta1, &key, None).unwrap_err();
        assert!(matches!(err1, RepresentationError::InvalidMetadata { .. }));

        // 2. chunk_index >= chunk_count in ObjectMetadata
        let mut bad_meta2 = sample_metadata(14, 1);
        bad_meta2.chunk_index = 2;
        bad_meta2.chunk_count = 2;
        let err2 = encode_recovery_object(payload, bad_meta2, &key, None).unwrap_err();
        assert!(matches!(err2, RepresentationError::InvalidMetadata { .. }));

        // 3. predecessor_generation >= generation in ObjectMetadata
        let mut bad_meta3 = sample_metadata(14, 5);
        bad_meta3.predecessor_generation = Some(5);
        let err3 = encode_recovery_object(payload, bad_meta3, &key, None).unwrap_err();
        assert!(matches!(err3, RepresentationError::InvalidMetadata { .. }));

        let mut bad_meta4 = sample_metadata(14, 5);
        bad_meta4.predecessor_generation = Some(6);
        let err4 = encode_recovery_object(payload, bad_meta4, &key, None).unwrap_err();
        assert!(matches!(err4, RepresentationError::InvalidMetadata { .. }));

        // 4. Bad bounds on ExpectedContext in decode_recovery_object
        let valid_meta = sample_metadata(14, 2);
        let valid_enc = encode_recovery_object(payload, valid_meta, &key, None).unwrap();

        let bad_exp1 = ExpectedContext {
            object_id: [0u8; 32],
            object_kind: RecoveryObjectKind::WholeMuxImage,
            generation: 2,
            predecessor_generation: Some(1),
            chunk_index: 0,
            chunk_count: 0,
        };
        let err_exp1 = decode_recovery_object(&valid_enc, &bad_exp1, &key, None).unwrap_err();
        assert!(matches!(
            err_exp1,
            RepresentationError::InvalidMetadata { .. }
        ));

        let bad_exp2 = ExpectedContext {
            object_id: [0u8; 32],
            object_kind: RecoveryObjectKind::WholeMuxImage,
            generation: 2,
            predecessor_generation: Some(2),
            chunk_index: 0,
            chunk_count: 1,
        };
        let err_exp2 = decode_recovery_object(&valid_enc, &bad_exp2, &key, None).unwrap_err();
        assert!(matches!(
            err_exp2,
            RepresentationError::InvalidMetadata { .. }
        ));
    }

    #[test]
    fn negative_non_canonical_header_json_in_envelope_rejected() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"canonical json representation test";
        let metadata = sample_metadata(15, 1);
        let enc = encode_recovery_object(payload, metadata, &key, None).unwrap();

        let canonical_wire = enc.to_bytes().unwrap();
        let header_len = u32::from_be_bytes([
            canonical_wire[12],
            canonical_wire[13],
            canonical_wire[14],
            canonical_wire[15],
        ]) as usize;

        // Insert an extra space into the header JSON to create non-canonical formatting
        let header_bytes = &canonical_wire[16..16 + header_len];
        let header_str = std::str::from_utf8(header_bytes).unwrap();
        // {"magic": -> {"magic":
        let non_canonical_str = header_str.replace("{\"magic\":", "{\"magic\": ");
        assert_ne!(header_str, non_canonical_str);

        let non_canonical_header_bytes = non_canonical_str.as_bytes();
        let new_header_len = non_canonical_header_bytes.len() as u32;

        let mut non_canonical_wire = Vec::new();
        non_canonical_wire.extend_from_slice(&canonical_wire[..12]);
        non_canonical_wire.extend_from_slice(&new_header_len.to_be_bytes());
        non_canonical_wire.extend_from_slice(non_canonical_header_bytes);
        non_canonical_wire.extend_from_slice(&canonical_wire[16 + header_len..]);

        let err = EncryptedRecoveryObject::from_bytes(&non_canonical_wire).unwrap_err();
        assert!(
            matches!(err, RepresentationError::MalformedRepresentation { ref reason } if reason.contains("non-canonical header JSON")),
            "expected non-canonical header JSON rejection, got {:?}",
            err
        );
    }

    #[test]
    fn negative_magic_and_version_inconsistency_rejected() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"version and magic consistency test";
        let metadata = sample_metadata(16, 1);
        let enc = encode_recovery_object(payload, metadata.clone(), &key, None).unwrap();
        let expected = ExpectedContext::from_metadata(&metadata);

        // 1. Header magic mismatch
        let mut bad_magic = enc.clone();
        bad_magic.header.magic = *b"BADMAGIC";
        let err1 = decode_recovery_object(&bad_magic, &expected, &key, None).unwrap_err();
        assert!(matches!(err1, RepresentationError::InvalidMagic { .. }));

        // 2. Header format_version mismatch
        let mut bad_ver = enc.clone();
        bad_ver.header.format_version = 999;
        let err2 = decode_recovery_object(&bad_ver, &expected, &key, None).unwrap_err();
        assert!(matches!(
            err2,
            RepresentationError::UnsupportedVersion { version: 999 }
        ));

        // 3. Inner context representation_version mismatch with header
        let mut bad_context_ver = enc.clone();
        bad_context_ver.header.context.representation_version = 999;
        let err3 = decode_recovery_object(&bad_context_ver, &expected, &key, None).unwrap_err();
        assert!(matches!(
            err3,
            RepresentationError::MalformedRepresentation { .. }
        ));
    }

    #[test]
    fn positive_chunked_object_representation_id_uniqueness() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let chunk0_meta = sample_chunk_metadata(55, 3, 0, 2);
        let chunk1_meta = sample_chunk_metadata(55, 3, 1, 2);

        let payload0 = b"scrollback lines 0..500";
        let payload1 = b"scrollback lines 501..1000";

        let obj0 = encode_recovery_object(payload0, chunk0_meta, &key, None).unwrap();
        let obj1 = encode_recovery_object(payload1, chunk1_meta, &key, None).unwrap();

        assert_ne!(
            obj0.representation_id().unwrap(),
            obj1.representation_id().unwrap()
        );
        assert_ne!(
            obj0.canonical_filename().unwrap(),
            obj1.canonical_filename().unwrap()
        );
        assert!(
            obj0.canonical_filename()
                .unwrap()
                .contains("chunk0000-of-0002")
        );
        assert!(
            obj1.canonical_filename()
                .unwrap()
                .contains("chunk0001-of-0002")
        );
    }

    #[test]
    fn negative_chunk_index_swap_fails_closed() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let chunk0_meta = sample_chunk_metadata(55, 3, 0, 2);
        let chunk1_meta = sample_chunk_metadata(55, 3, 1, 2);

        let payload0 = b"scrollback chunk 0";
        let payload1 = b"scrollback chunk 1";

        let obj0 = encode_recovery_object(payload0, chunk0_meta, &key, None).unwrap();
        let _obj1 = encode_recovery_object(payload1, chunk1_meta.clone(), &key, None).unwrap();

        // Attempt to supply expected context for chunk 1 to chunk 0
        let expected1 = ExpectedContext::from_metadata(&chunk1_meta);
        let err = decode_recovery_object(&obj0, &expected1, &key, None).unwrap_err();
        assert!(matches!(
            err,
            RepresentationError::ContextMismatch {
                field: "chunk_index",
                ..
            }
        ));
    }

    #[test]
    fn negative_wrong_key_fails_decryption() {
        let key1 = RecoveryKey::generate().expect("key generation succeeds");
        let key2 = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"secret agent session recovery stream";
        let metadata = sample_metadata(1, 100);
        let expected = ExpectedContext::from_metadata(&metadata);

        let encrypted = encode_recovery_object(payload, metadata, &key1, None).unwrap();

        // Key ID mismatch caught before AEAD
        let err = decode_recovery_object(&encrypted, &expected, &key2, None).unwrap_err();
        assert!(matches!(err, RepresentationError::KeyError { .. }));

        // Now test identical key_id but corrupted key bytes
        let key2_spoofed = RecoveryKey {
            bytes: key2.bytes,
            key_id: key1.key_id(),
        };
        let err2 = decode_recovery_object(&encrypted, &expected, &key2_spoofed, None).unwrap_err();
        assert!(matches!(err2, RepresentationError::DecryptionFailed { .. }));
    }

    #[test]
    fn negative_ciphertext_tamper_fails_integrity_or_aead() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"important terminal modes and styled cells";
        let metadata = sample_metadata(2, 5);
        let expected = ExpectedContext::from_metadata(&metadata);

        let mut encrypted = encode_recovery_object(payload, metadata, &key, None).unwrap();

        // Flip one byte in the ciphertext
        let last_idx = encrypted.ciphertext.len() - 1;
        encrypted.ciphertext[last_idx] ^= 0x01;

        // Ciphertext digest fails first
        let err = decode_recovery_object(&encrypted, &expected, &key, None).unwrap_err();
        assert!(matches!(err, RepresentationError::DigestMismatch { .. }));

        // If digest is updated to match tampered ciphertext, AEAD authentication tag must reject!
        let forged_digest: [u8; 32] = Sha256::digest(&encrypted.ciphertext).into();
        encrypted.header.ciphertext_digest = forged_digest;
        let err2 = decode_recovery_object(&encrypted, &expected, &key, None).unwrap_err();
        assert!(matches!(err2, RepresentationError::DecryptionFailed { .. }));
    }

    #[test]
    fn negative_nonce_tamper_fails_aead() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"critical terminal stream";
        let metadata = sample_metadata(2, 6);
        let expected = ExpectedContext::from_metadata(&metadata);

        let mut encrypted = encode_recovery_object(payload, metadata, &key, None).unwrap();
        encrypted.header.nonce[0] ^= 0x01;

        let err = decode_recovery_object(&encrypted, &expected, &key, None).unwrap_err();
        assert!(matches!(err, RepresentationError::DecryptionFailed { .. }));
    }

    #[test]
    fn negative_aad_identity_tamper_fails_closed() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"window and tab layout tree";
        let metadata = sample_metadata(3, 8);

        let encrypted = encode_recovery_object(payload, metadata.clone(), &key, None).unwrap();

        // 1. Wrong expected object_id
        let mut wrong_expected = ExpectedContext::from_metadata(&metadata);
        wrong_expected.object_id[0] ^= 0xFF;
        let err = decode_recovery_object(&encrypted, &wrong_expected, &key, None).unwrap_err();
        assert!(matches!(
            err,
            RepresentationError::ContextMismatch {
                field: "object_id",
                ..
            }
        ));

        // 2. Wrong expected generation
        let mut wrong_gen = ExpectedContext::from_metadata(&metadata);
        wrong_gen.generation = 999;
        let err2 = decode_recovery_object(&encrypted, &wrong_gen, &key, None).unwrap_err();
        assert!(matches!(
            err2,
            RepresentationError::ContextMismatch {
                field: "generation",
                ..
            }
        ));

        // 3. Wrong expected predecessor generation
        let mut wrong_pred = ExpectedContext::from_metadata(&metadata);
        wrong_pred.predecessor_generation = Some(42);
        assert!(matches!(
            decode_recovery_object(&encrypted, &wrong_pred, &key, None),
            Err(RepresentationError::InvalidMetadata { .. })
        ));
        wrong_pred.predecessor_generation = Some(metadata.generation - 2);
        wrong_pred.validate().unwrap();
        let err3 = decode_recovery_object(&encrypted, &wrong_pred, &key, None).unwrap_err();
        assert!(matches!(
            err3,
            RepresentationError::ContextMismatch {
                field: "predecessor_generation",
                ..
            }
        ));

        // 4. In-header AAD tamper: modify generation in header and decode with forged expected context
        let mut tampered_header = encrypted.clone();
        tampered_header.header.context.generation = 999;
        let mut tampered_expected = ExpectedContext::from_metadata(&metadata);
        tampered_expected.generation = 999;

        let err4 =
            decode_recovery_object(&tampered_header, &tampered_expected, &key, None).unwrap_err();
        assert!(matches!(err4, RepresentationError::DecryptionFailed { .. }));
    }

    #[test]
    fn negative_uncompressed_limit_exceeded_rejected() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = vec![1u8; 1000];
        let metadata = sample_metadata(5, 1);

        let strict_config = RepresentationConfig {
            max_uncompressed_bytes: 500,
            ..Default::default()
        };

        let err =
            encode_recovery_object(&payload, metadata, &key, Some(&strict_config)).unwrap_err();
        assert!(matches!(
            err,
            RepresentationError::UncompressedLimitExceeded {
                size: 1000,
                limit: 500
            }
        ));
    }

    #[test]
    fn negative_zero_key_rejected() {
        let zero_bytes = [0u8; 32];
        let err = RecoveryKey::from_bytes(zero_bytes).unwrap_err();
        assert!(matches!(err, RepresentationError::KeyError { .. }));
    }

    #[test]
    fn security_zero_secret_debug_and_redaction() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let debug_str = format!("{:?}", key);
        assert!(debug_str.contains("[REDACTED]"));
        assert!(!debug_str.contains(&hex::encode(key.as_bytes())));

        let payload = b"super secret auth token bearer xyz";
        let metadata = sample_metadata(9, 1);
        let expected = ExpectedContext::from_metadata(&metadata);

        let encrypted = encode_recovery_object(payload, metadata, &key, None).unwrap();
        let enc_debug = format!("{:?}", encrypted);
        assert!(enc_debug.contains("[ENCRYPTED_PAYLOAD]"));
        assert!(!enc_debug.contains("secret"));

        let decoded = decode_recovery_object(&encrypted, &expected, &key, None).unwrap();
        let dec_debug = format!("{:?}", decoded);
        assert!(dec_debug.contains("[REDACTED]"));
        assert!(!dec_debug.contains("secret"));
    }

    #[test]
    fn negative_wire_bytes_corruptions() {
        // 1. Slice too short
        let short = vec![0u8; 10];
        let err = EncryptedRecoveryObject::from_bytes(&short).unwrap_err();
        assert!(matches!(
            err,
            RepresentationError::MalformedRepresentation { .. }
        ));

        // 2. Invalid magic
        let mut bad_magic = vec![0u8; 32];
        bad_magic[0..8].copy_from_slice(b"BADMAGIC");
        let err2 = EncryptedRecoveryObject::from_bytes(&bad_magic).unwrap_err();
        assert!(matches!(err2, RepresentationError::InvalidMagic { .. }));

        // 3. Unsupported format version
        let mut bad_version = vec![0u8; 32];
        bad_version[0..8].copy_from_slice(&RECOVERY_OBJECT_MAGIC);
        bad_version[8..12].copy_from_slice(&999u32.to_be_bytes());
        let err3 = EncryptedRecoveryObject::from_bytes(&bad_version).unwrap_err();
        assert!(matches!(
            err3,
            RepresentationError::UnsupportedVersion { version: 999 }
        ));

        // 4. Header JSON length exceeds bound
        let mut huge_header = vec![0u8; 32];
        huge_header[0..8].copy_from_slice(&RECOVERY_OBJECT_MAGIC);
        huge_header[8..12].copy_from_slice(&RECOVERY_FORMAT_VERSION.to_be_bytes());
        huge_header[12..16].copy_from_slice(&(MAX_HEADER_JSON_BYTES as u32 + 1).to_be_bytes());
        let err4 = EncryptedRecoveryObject::from_bytes(&huge_header).unwrap_err();
        assert!(matches!(err4, RepresentationError::HeaderTooLarge { .. }));
    }

    #[test]
    fn positive_all_object_kinds_supported() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let kinds = [
            RecoveryObjectKind::WholeMuxImage,
            RecoveryObjectKind::TerminalCheckpoint,
            RecoveryObjectKind::ScrollbackSegment,
            RecoveryObjectKind::TopologyTree,
            RecoveryObjectKind::Custom,
        ];

        for (idx, kind) in kinds.into_iter().enumerate() {
            let mut obj_id = [0u8; 32];
            obj_id[0] = idx as u8;
            let metadata = ObjectMetadata::single(obj_id, kind, 1, None, 1747371642000);
            let expected = ExpectedContext::from_metadata(&metadata);

            let payload = format!("state for kind {:?}", kind).into_bytes();
            let enc = encode_recovery_object(&payload, metadata, &key, None)
                .expect("encode must succeed");
            assert_eq!(enc.header.context.object_kind, kind);

            let decoded =
                decode_recovery_object(&enc, &expected, &key, None).expect("decode must succeed");
            assert_eq!(decoded.plaintext(), payload.as_slice());
        }
    }

    #[test]
    fn positive_large_payload_compression_efficiency() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let mut large_text = Vec::new();
        for i in 0..1000 {
            large_text.extend_from_slice(
                format!(
                    "\x1b[32m[2026-09-14T22:00:00Z] INFO [worker-{}]: line data and repetitive terminal scrollback buffer content\x1b[0m\n",
                    i
                )
                .as_bytes(),
            );
        }
        let metadata = sample_metadata(99, 10);
        let expected = ExpectedContext::from_metadata(&metadata);

        let enc = encode_recovery_object(&large_text, metadata, &key, None)
            .expect("large payload encode must succeed");

        // zstd should achieve significant compression on repetitive text
        assert!(enc.ciphertext.len() < large_text.len() / 2);

        let decoded = decode_recovery_object(&enc, &expected, &key, None)
            .expect("large payload decode must succeed");
        assert_eq!(decoded.into_plaintext().as_slice(), large_text.as_slice());
    }
}
