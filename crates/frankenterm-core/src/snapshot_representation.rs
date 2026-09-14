//! Snapshot representation: bounded zstd compression and AEAD encrypted recovery objects.
//!
//! # Bead: `ft-interactive-swarm-product-convergence-7xqz4.8.14.2.3`
//!
//! Provides defense-in-depth representation encoding for terminal/mux snapshot recovery:
//!
//! 1. **Bounded zstd compression**: semantic bytes are compressed using zstd with explicit
//!    pre-compression and post-compression limits to protect against memory exhaustion.
//! 2. **AEAD Encryption (XChaCha20-Poly1305)**: compressed bytes are encrypted using pure-Rust
//!    XChaCha20-Poly1305 with random 192-bit nonces, ensuring collision resistance across
//!    restarts and concurrent workers without state coordination.
//! 3. **Versioned Object/Generation Identity AAD**: Associated Authenticated Data cryptographically
//!    binds logical object identity, representation version, generation, predecessor generation,
//!    timestamp, key ID, chunk bounds (`chunk_index` / `chunk_count`), and digests. Tampering
//!    with any contextual field fails AEAD verification.
//! 4. **Digest Verification**: SHA-256 digests verify plaintext integrity before compression,
//!    compressed bytes before encryption, and ciphertext after encryption.
//! 5. **Bounded Decompression (Bomb Defense)**: during decode, AEAD authentication runs first;
//!    decompression enforces strict uncompressed size ceilings and expansion ratio bounds.
//! 6. **Zero Secret Debug & Memory Zeroization**: symmetric key material is zeroized on drop,
//!    and `Debug` implementations redact all secret bytes.
//! 7. **Whole-Representation Wire Envelope for FEC & Publication**: the canonical binary format
//!    `to_bytes()` packages magic, version, header metadata (with context/nonce), and ciphertext.
//!    Peers `%570` (RaptorQ FEC) and `%572` (Snapshot Publication) protect and persist this complete
//!    envelope so that repairing or reading an object preserves all authenticated contextual parameters.

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use rand::{TryRng, rngs::SysRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

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

/// Domain separator for representation-id derivation.
pub const RECOVERY_REPRESENTATION_ID_DOMAIN: &[u8] =
    b"frankenterm.snapshot-recovery.representation-id.v1\0";

/// Required symmetric key length in bytes (256 bits).
pub const KEY_BYTES: usize = 32;

/// Required nonce length for XChaCha20-Poly1305 in bytes (192 bits).
pub const NONCE_BYTES: usize = 24;

/// Default maximum uncompressed object size (64 MiB).
pub const DEFAULT_MAX_UNCOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum compressed object size (64 MiB).
pub const DEFAULT_MAX_COMPRESSED_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum allowable expansion ratio during decompression (200x).
pub const DEFAULT_MAX_EXPANSION_RATIO: usize = 200;

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
/// Automatically zeroizes memory on drop.
/// `Debug` implementation redacts all key material.
pub struct RecoveryKey {
    bytes: [u8; KEY_BYTES],
    key_id: [u8; 8],
}

impl RecoveryKey {
    /// Construct a recovery key from raw 32 bytes.
    ///
    /// Rejects all-zero keys to prevent uninitialized key use.
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Result<Self, RepresentationError> {
        if bytes.iter().all(|b| *b == 0) {
            return Err(RepresentationError::KeyError {
                reason: "all-zero recovery key is rejected".into(),
            });
        }
        let mut hasher = Sha256::new();
        hasher.update(RECOVERY_KEY_ID_DOMAIN);
        hasher.update(&bytes);
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
        Self::try_generate()
    }

    /// Explicit try-variant for generating a cryptographically secure key.
    pub fn try_generate() -> Result<Self, RepresentationError> {
        let mut bytes = [0u8; KEY_BYTES];
        let mut rng = SysRng;
        rng.try_fill_bytes(&mut bytes)
            .map_err(|e| RepresentationError::EntropyUnavailable {
                reason: format!("failed to obtain {} bytes of entropy: {e}", KEY_BYTES),
            })?;
        Self::from_bytes(bytes)
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
}

impl Drop for RecoveryKey {
    fn drop(&mut self) {
        for b in self.bytes.iter_mut() {
            *b = 0;
        }
        std::hint::black_box(&mut self.bytes);
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
    /// Compute deterministic, collision-resistant Associated Authenticated Data (AAD).
    #[must_use]
    pub fn compute_canonical_aad(&self) -> Vec<u8> {
        let mut aad = Vec::with_capacity(160);
        aad.extend_from_slice(RECOVERY_AAD_DOMAIN);
        aad.extend_from_slice(&self.representation_version.to_be_bytes());
        aad.extend_from_slice(&self.object_id);
        aad.push(self.object_kind as u8);
        aad.extend_from_slice(&self.generation.to_be_bytes());
        aad.extend_from_slice(&self.predecessor_generation.unwrap_or(0).to_be_bytes());
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

    /// 32-byte representation identity uniquely binding the representation format,
    /// object ID, generation, and chunk coordinates.
    ///
    /// Consumed by `%570` (`snapshot_repair.rs`) as `representation_id`.
    #[must_use]
    pub fn representation_id(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(RECOVERY_REPRESENTATION_ID_DOMAIN);
        hasher.update(&self.representation_version.to_be_bytes());
        hasher.update(&self.object_id);
        hasher.update(&self.generation.to_be_bytes());
        hasher.update(&self.chunk_index.to_be_bytes());
        hasher.update(&self.chunk_count.to_be_bytes());
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    /// Hex-encoded representation identity.
    #[must_use]
    pub fn representation_id_hex(&self) -> String {
        hex::encode(self.representation_id())
    }

    /// Canonical immutable chunk filename embedding representation and chunk identity.
    #[must_use]
    pub fn chunk_filename(&self) -> String {
        let rep_short = &self.representation_id_hex()[..16];
        if self.chunk_count > 1 {
            format!(
                "obj-gen{:016x}-{}-rep{}-chunk{:04x}-of-{:04x}.ftrec",
                self.generation,
                hex::encode(self.object_id),
                rep_short,
                self.chunk_index,
                self.chunk_count,
            )
        } else {
            format!(
                "obj-gen{:016x}-{}-rep{}.ftrec",
                self.generation,
                hex::encode(self.object_id),
                rep_short,
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
    pub max_expansion_ratio: usize,
}

impl Default for RepresentationConfig {
    fn default() -> Self {
        Self {
            zstd_level: DEFAULT_ZSTD_LEVEL,
            max_uncompressed_bytes: DEFAULT_MAX_UNCOMPRESSED_BYTES,
            max_compressed_bytes: DEFAULT_MAX_COMPRESSED_BYTES,
            max_expansion_ratio: DEFAULT_MAX_EXPANSION_RATIO,
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
    /// The entire byte slice returned by this method is what `%570` (`snapshot_repair.rs`) ingests
    /// to generate systematic RaptorQ repair symbols.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RepresentationError> {
        let header_json = serde_json::to_vec(&self.header)
            .map_err(|e| RepresentationError::SerializationError { reason: e.to_string() })?;
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

        let mut bytes =
            Vec::with_capacity(8 + 4 + 4 + header_json.len() + 8 + self.ciphertext.len());
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
        let header_len =
            u32::from_be_bytes([slice[12], slice[13], slice[14], slice[15]]) as usize;
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
        let header: RecoveryObjectHeader = serde_json::from_slice(header_bytes)
            .map_err(|e| RepresentationError::MalformedRepresentation {
                reason: format!("header JSON deserialization failed: {e}"),
            })?;

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

    /// SHA-256 digest of the entire canonical serialized binary envelope (`to_bytes()`).
    /// Matches `representation_id` in `%570` (`snapshot_repair.rs`).
    pub fn envelope_digest(&self) -> Result<[u8; 32], RepresentationError> {
        self.representation_id()
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRepresentation {
    pub plaintext: Vec<u8>,
    pub context: RepresentationContext,
}

impl DecodedRepresentation {
    /// Consume the wrapper and return the recovered plaintext.
    #[must_use]
    pub fn into_plaintext(self) -> Vec<u8> {
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

// =============================================================================
// Encode & Decode Algorithms
// =============================================================================

/// Compress semantic bytes using bounded zstd, then AEAD-encrypt with authenticated object identity AAD.
pub fn encode_recovery_object(
    plaintext: &[u8],
    metadata: ObjectMetadata,
    key: &RecoveryKey,
    config_opt: Option<&RepresentationConfig>,
) -> Result<EncryptedRecoveryObject, RepresentationError> {
    let default_config = RepresentationConfig::default();
    let config = config_opt.unwrap_or(&default_config);

    // 1. Guard uncompressed input length
    if plaintext.len() > config.max_uncompressed_bytes {
        return Err(RepresentationError::UncompressedLimitExceeded {
            size: plaintext.len(),
            limit: config.max_uncompressed_bytes,
        });
    }
    let uncompressed_bytes = plaintext.len() as u64;
    let uncompressed_digest: [u8; 32] = Sha256::digest(plaintext).into();

    // 2. Compress via zstd
    let compressed = zstd::bulk::compress(plaintext, config.zstd_level)
        .map_err(|e| RepresentationError::CompressionFailed { reason: e.to_string() })?;

    if compressed.len() > config.max_compressed_bytes {
        return Err(RepresentationError::CompressedLimitExceeded {
            size: compressed.len(),
            limit: config.max_compressed_bytes,
        });
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
            reason: format!("failed to obtain {} bytes of entropy for nonce: {e}", NONCE_BYTES),
        })?;

    // 5. Compute AAD and encrypt via XChaCha20-Poly1305
    let aad = context.compute_canonical_aad();
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_bytes())
        .map_err(|e| RepresentationError::KeyError { reason: e.to_string() })?;
    let xnonce = XNonce::from_slice(&nonce);
    let payload = Payload {
        msg: &compressed,
        aad: &aad,
    };
    let ciphertext = cipher
        .encrypt(xnonce, payload)
        .map_err(|e| RepresentationError::EncryptionFailed { reason: e.to_string() })?;

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
/// Returns `DecodedRepresentation` containing both the recovered semantic plaintext and
/// the authenticated context.
pub fn decode_recovery_object(
    object: &EncryptedRecoveryObject,
    expected: &ExpectedContext,
    key: &RecoveryKey,
    config_opt: Option<&RepresentationConfig>,
) -> Result<DecodedRepresentation, RepresentationError> {
    let default_config = RepresentationConfig::default();
    let config = config_opt.unwrap_or(&default_config);

    // 1. Validate envelope magic & format version
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

    // 2. Validate expected context matches representation header
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

    // 3. Validate key ID matches
    if object.header.context.key_id != key.key_id() {
        return Err(RepresentationError::KeyError {
            reason: format!(
                "key ID mismatch: object requires {}, provided key is {}",
                hex::encode(object.header.context.key_id),
                key.key_id_hex()
            ),
        });
    }

    // 4. Verify ciphertext digest
    let actual_ciphertext_digest: [u8; 32] = Sha256::digest(&object.ciphertext).into();
    if actual_ciphertext_digest != object.header.ciphertext_digest {
        return Err(RepresentationError::DigestMismatch {
            target: "ciphertext digest",
            expected: hex::encode(object.header.ciphertext_digest),
            actual: hex::encode(actual_ciphertext_digest),
        });
    }

    // 5. Decrypt via XChaCha20-Poly1305 with authenticated AAD
    let aad = object.header.context.compute_canonical_aad();
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_bytes())
        .map_err(|e| RepresentationError::KeyError { reason: e.to_string() })?;
    let xnonce = XNonce::from_slice(&object.header.nonce);
    let payload = Payload {
        msg: &object.ciphertext,
        aad: &aad,
    };
    let compressed = cipher
        .decrypt(xnonce, payload)
        .map_err(|e| RepresentationError::DecryptionFailed { reason: e.to_string() })?;

    // 6. Verify compressed bytes digest
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

    // 7. Bounded decompression guards (decompression bomb protection)
    let expected_uncompressed = usize::try_from(object.header.context.uncompressed_bytes)
        .map_err(|_| RepresentationError::UncompressedLimitExceeded {
            size: usize::MAX,
            limit: config.max_uncompressed_bytes,
        })?;

    if expected_uncompressed > config.max_uncompressed_bytes {
        return Err(RepresentationError::UncompressedLimitExceeded {
            size: expected_uncompressed,
            limit: config.max_uncompressed_bytes,
        });
    }

    let max_allowed = (compressed.len() as u64).saturating_mul(config.max_expansion_ratio as u64);
    if object.header.context.uncompressed_bytes > max_allowed {
        return Err(RepresentationError::ExpansionRatioExceeded {
            uncompressed: expected_uncompressed,
            compressed: compressed.len(),
            limit: config.max_expansion_ratio,
        });
    }

    // 8. Decompress up to exact expected uncompressed length
    let decompressed = zstd::bulk::decompress(&compressed, expected_uncompressed)
        .map_err(|e| RepresentationError::DecompressionFailed { reason: e.to_string() })?;

    if decompressed.len() != expected_uncompressed {
        return Err(RepresentationError::DecompressedLengthMismatch {
            expected: expected_uncompressed,
            actual: decompressed.len(),
        });
    }

    // 9. Verify uncompressed digest against recovered plaintext
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

/// Convenience helper to decode directly into raw plaintext bytes.
pub fn decode_recovery_object_bytes(
    object: &EncryptedRecoveryObject,
    expected: &ExpectedContext,
    key: &RecoveryKey,
    config_opt: Option<&RepresentationConfig>,
) -> Result<Vec<u8>, RepresentationError> {
    decode_recovery_object(object, expected, key, config_opt).map(|res| res.plaintext)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metadata(object_id_byte: u8, generation: u64) -> ObjectMetadata {
        let mut object_id = [0u8; 32];
        object_id[0] = object_id_byte;
        ObjectMetadata::single(
            object_id,
            RecoveryObjectKind::WholeMuxImage,
            generation,
            if generation > 0 { Some(generation - 1) } else { None },
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
            if generation > 0 { Some(generation - 1) } else { None },
            1747371642000,
            chunk_index,
            chunk_count,
        )
    }

    #[test]
    fn positive_exact_semantic_bytes_encrypt_decrypt_roundtrip() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"{\"version\":1,\"panes\":[{\"pane_id\":1,\"title\":\"test-agent\"}]}";
        let metadata = sample_metadata(42, 10);
        let expected = ExpectedContext::from_metadata(&metadata);

        let encrypted = encode_recovery_object(payload, metadata, &key, None)
            .expect("encoding must succeed");

        assert_eq!(encrypted.header.magic, RECOVERY_OBJECT_MAGIC);
        assert_eq!(encrypted.header.format_version, RECOVERY_FORMAT_VERSION);
        assert_ne!(encrypted.ciphertext, payload);

        let decoded = decode_recovery_object(&encrypted, &expected, &key, None)
            .expect("decoding must succeed");

        assert_eq!(decoded.plaintext(), payload);
        assert_eq!(decoded.context().generation, 10);
    }

    #[test]
    fn positive_wire_serialization_roundtrip() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let payload = b"critical terminal state checkpoint";
        let metadata = sample_metadata(7, 1);
        let expected = ExpectedContext::from_metadata(&metadata);

        let encrypted = encode_recovery_object(payload, metadata, &key, None)
            .expect("encoding must succeed");

        let wire_bytes = encrypted.to_bytes().expect("serialization must succeed");
        let parsed = EncryptedRecoveryObject::from_bytes(&wire_bytes)
            .expect("deserialization must succeed");

        assert_eq!(parsed.header, encrypted.header);
        assert_eq!(parsed.ciphertext, encrypted.ciphertext);

        let decoded = decode_recovery_object(&parsed, &expected, &key, None)
            .expect("decoding from parsed wire bytes must succeed");
        assert_eq!(decoded.into_plaintext(), payload);
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

        assert_ne!(obj0.representation_id().unwrap(), obj1.representation_id().unwrap());
        assert_ne!(obj0.canonical_filename().unwrap(), obj1.canonical_filename().unwrap());
        assert!(obj0.canonical_filename().unwrap().contains("chunk0000-of-0002"));
        assert!(obj1.canonical_filename().unwrap().contains("chunk0001-of-0002"));
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
            RepresentationError::ContextMismatch { field: "chunk_index", .. }
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
            RepresentationError::ContextMismatch { field: "object_id", .. }
        ));

        // 2. Wrong expected generation
        let mut wrong_gen = ExpectedContext::from_metadata(&metadata);
        wrong_gen.generation = 999;
        let err2 = decode_recovery_object(&encrypted, &wrong_gen, &key, None).unwrap_err();
        assert!(matches!(
            err2,
            RepresentationError::ContextMismatch { field: "generation", .. }
        ));

        // 3. Wrong expected predecessor generation
        let mut wrong_pred = ExpectedContext::from_metadata(&metadata);
        wrong_pred.predecessor_generation = Some(42);
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

        let err4 = decode_recovery_object(&tampered_header, &tampered_expected, &key, None)
            .unwrap_err();
        assert!(matches!(err4, RepresentationError::DecryptionFailed { .. }));
    }

    #[test]
    fn negative_decompression_bomb_expansion_ratio_rejected() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        // 10,000 zeros compress to ~30 bytes (ratio > 300x)
        let zeros = vec![0u8; 10_000];
        let metadata = sample_metadata(4, 1);
        let expected = ExpectedContext::from_metadata(&metadata);

        let encrypted = encode_recovery_object(&zeros, metadata, &key, None).unwrap();

        // Decode with strict expansion ratio limit (e.g. 50x)
        let strict_config = RepresentationConfig {
            max_expansion_ratio: 50,
            ..Default::default()
        };

        let err = decode_recovery_object(&encrypted, &expected, &key, Some(&strict_config))
            .unwrap_err();
        assert!(matches!(
            err,
            RepresentationError::ExpansionRatioExceeded { .. }
        ));
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
    fn security_zero_secret_debug() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let debug_str = format!("{:?}", key);
        assert!(debug_str.contains("[REDACTED]"));
        assert!(!debug_str.contains(&hex::encode(key.as_bytes())));

        let payload = b"super secret auth token bearer xyz";
        let metadata = sample_metadata(9, 1);
        let encrypted = encode_recovery_object(payload, metadata, &key, None).unwrap();
        let enc_debug = format!("{:?}", encrypted);
        assert!(enc_debug.contains("[ENCRYPTED_PAYLOAD]"));
        assert!(!enc_debug.contains("secret"));
    }

    #[test]
    fn negative_wire_bytes_corruptions() {
        // 1. Slice too short
        let short = vec![0u8; 10];
        let err = EncryptedRecoveryObject::from_bytes(&short).unwrap_err();
        assert!(matches!(err, RepresentationError::MalformedRepresentation { .. }));

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
        assert!(matches!(err3, RepresentationError::UnsupportedVersion { version: 999 }));

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

            let decoded = decode_recovery_object(&enc, &expected, &key, None)
                .expect("decode must succeed");
            assert_eq!(decoded.plaintext(), payload.as_slice());
        }
    }

    #[test]
    fn positive_large_payload_compression_efficiency() {
        let key = RecoveryKey::generate().expect("key generation succeeds");
        let mut large_text = Vec::new();
        for i in 0..1000 {
            large_text.extend_from_slice(format!("\x1b[32m[2026-09-14T22:00:00Z] INFO [worker-{}]: line data and repetitive terminal scrollback buffer content\x1b[0m\n", i).as_bytes());
        }
        let metadata = sample_metadata(99, 10);
        let expected = ExpectedContext::from_metadata(&metadata);

        let enc = encode_recovery_object(&large_text, metadata, &key, None)
            .expect("large payload encode must succeed");

        // zstd should achieve significant compression on repetitive text
        assert!(enc.ciphertext.len() < large_text.len() / 2);

        let decoded = decode_recovery_object(&enc, &expected, &key, None)
            .expect("large payload decode must succeed");
        assert_eq!(decoded.into_plaintext(), large_text);
    }
}
