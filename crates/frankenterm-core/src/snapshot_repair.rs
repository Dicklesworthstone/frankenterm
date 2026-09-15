//! Authenticated RaptorQ Snapshot Repair Module.
//!
//! Bead: `ft-interactive-swarm-product-convergence-7xqz4.8.14.2.4`
//!
//! This module implements bounded, authenticated RaptorQ forward-error-correction (FEC)
//! encoding and decoding for FrankenTerm mux snapshot recovery objects.
//!
//! # Non-Claim Boundary
//!
//! Codec and repair proofs in this module establish *representation recovery* only
//! (i.e. bit-for-bit reconstruction of authenticated serialized encrypted envelope bytes).
//! They do NOT claim live PTY continuity, kernel terminal-state restoration, or whole-mux
//! supervisory recovery without the external orchestrator.
//!
//! # Architecture & Doctrine
//!
//! - **Whole-Envelope Protection**: FEC protects the *entire* serialized encrypted
//!   representation (headers, nonce, AAD metadata, ciphertext), not raw ciphertext alone.
//! - **Dual Identity Model**: `representation_id = SHA256(exact envelope bytes)`, which is
//!   cryptographically distinct from the semantic `object_id` and `generation`.
//! - **External Manifest & Tag Authentication**: All decode geometry (original payload length,
//!   symbol size, source block symbol count $K$, SBN) and individual symbols are authenticated
//!   using vetted HMAC-SHA256 (`hmac` crate) with constant-time verification.
//! - **Bounded Single-Block Chunking**: Enforces $SBN = 0$ and $K \le \text{MAX\_CHUNK\_SOURCE\_SYMBOLS}$
//!   ($K \le 2,048$, envelope $\le 2$ MiB per chunk). Callers must chunk large objects into
//!   bounded representations rather than attempting quadratic full-block Gaussian elimination
//!   at the theoretical RFC 6330 maximum of 56,403 symbols.
//! - **Decoder Working-Set Admission**: Pre-charges checked estimates covering
//!   symbol payloads, intermediate symbols, dense inactivation matrices ($L \times L$ in GF(256)),
//!   and equation structures BEFORE constructor initialization or matrix solving.
//! - **Pre-Allocation Buffering Guards**: Enforces `max_symbols_buffered` ceiling using checked
//!   arithmetic BEFORE reserving memory or allocating vectors.
//! - **Rank-Aware Inactivation Decoding**: Checks the exact GF(256) equation rank before
//!   attempting Gaussian elimination, reporting finite diagnostic rank deficits on failure.
//! - **Post-Reconstruction Digest Gate**: Verifies `SHA256(reconstructed) == manifest.representation_id`
//!   before yielding bytes to prevent downstream AEAD/decompression from encountering
//!   corrupted state.
//! - **Admission Control & Cancellation**: Bounded concurrency and memory via
//!   [`RepairAdmissionController`], with periodic `cx.checkpoint()` cancellation gates.
//!   Because RaptorQ systematic matrix inversion and inactivation elimination are
//!   synchronous, compute-bound algorithms that cannot be preempted mid-matrix-solve,
//!   geometry is capped through $K \le \text{MAX\_CHUNK\_SOURCE\_SYMBOLS}$
//!   ($K \le 2,048$). Solve latency requires measurement.
//!   Hard interruption during solve is not claimed; instead, `cx.checkpoint()` is verified
//!   immediately AFTER solve and digest verification before returning success, ensuring that
//!   cancellation during solve fails closed.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::cx::Cx;
use crate::runtime_async::raptorq::{
    InactivationDecoder, ReceivedSymbol, SystematicEncoder, SystematicParams,
};

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// Constants & Configuration
// ---------------------------------------------------------------------------

/// Maximum source block symbol count supported by RFC 6330.
pub const RFC6330_MAX_K: usize = 56_403;

/// Default symbol size in bytes (1 KiB).
pub const DEFAULT_SYMBOL_SIZE: usize = 1024;

/// Minimum allowable symbol size in bytes.
pub const MIN_SYMBOL_SIZE: usize = 16;

/// Maximum allowable symbol size in bytes (64 KiB).
pub const MAX_SYMBOL_SIZE: usize = 64 * 1024;

/// Maximum source symbols per recovery chunk.
///
/// While RFC 6330 permits up to 56,403 symbols, dense Gaussian elimination
/// at that scale requires gigabytes of matrix allocations and seconds of CPU.
/// To enforce strict latency and memory containment, snapshot representations
/// must be partitioned into bounded chunks of at most `MAX_CHUNK_SOURCE_SYMBOLS`.
pub const MAX_CHUNK_SOURCE_SYMBOLS: usize = 2048;

/// Maximum allowable single-chunk envelope size (2 MiB with default 1024-byte symbols).
pub const MAX_CHUNK_ENVELOPE_BYTES: usize = MAX_CHUNK_SOURCE_SYMBOLS * DEFAULT_SYMBOL_SIZE;

/// Persisted objects retain the complete reconstructed output while decoding
/// one block. Keep that block below the standalone codec ceiling so the default
/// 128 MiB object and Maximum protection fit the shared 256 MiB admission budget.
pub const MAX_PERSISTED_SOURCE_SYMBOLS: usize = 512;
pub const MAX_PERSISTED_CHUNK_BYTES: usize = MAX_PERSISTED_SOURCE_SYMBOLS * DEFAULT_SYMBOL_SIZE;

/// Domain separator for symbol HMAC calculation.
const SYMBOL_MAC_DOMAIN: &[u8] = b"frankenterm-snapshot-repair-symbol-v1";

/// Domain separator for manifest HMAC calculation.
const MANIFEST_MAC_DOMAIN: &[u8] = b"frankenterm-snapshot-repair-manifest-v1";

// ---------------------------------------------------------------------------
// Core Types & Enums
// ---------------------------------------------------------------------------

/// Authenticated RaptorQ repair symmetric key wrapper that automatically zeroizes
/// on drop via `zeroize::Zeroizing`.
#[derive(Clone)]
pub struct RepairAuthKey {
    key_bytes: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for RepairAuthKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RepairAuthKey([REDACTED])")
    }
}

impl RepairAuthKey {
    /// Create a new key wrapper from a byte slice, guarding the memory in `Zeroizing`.
    pub fn new(key: &[u8]) -> Result<Self, RepairError> {
        if key.is_empty() {
            return Err(RepairError::EmptyAuthenticationKey);
        }
        Ok(Self {
            key_bytes: Zeroizing::new(key.to_vec()),
        })
    }

    /// Access the key bytes as an immutable slice.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.key_bytes
    }
}

/// Symbol type distinction: systematic source symbol or generated repair symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RepairSymbolKind {
    /// Source symbol containing original envelope slice.
    Source,
    /// Repair symbol generated via RaptorQ LT / inactivation equations.
    Repair,
}

impl RepairSymbolKind {
    /// Return byte tag for HMAC serialization.
    #[must_use]
    pub const fn to_byte(self) -> u8 {
        match self {
            Self::Source => 0,
            Self::Repair => 1,
        }
    }

    /// Check if repair.
    #[must_use]
    pub const fn is_repair(self) -> bool {
        matches!(self, Self::Repair)
    }
}

/// A fully authenticated repair or source symbol with MAC tag and geometry metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthenticatedRepairSymbol {
    /// Symbol classification.
    pub kind: RepairSymbolKind,
    /// Source Block Number (must be 0 in single-block profile).
    pub sbn: u8,
    /// Encoding Symbol ID ($0 \le \text{ESI} < K$ for source; $\text{ESI} \ge K$ for repair).
    pub esi: u32,
    /// Symbol payload bytes (length exactly equals `symbol_size`).
    pub payload: Vec<u8>,
    /// HMAC-SHA256 authentication tag covering metadata + payload.
    pub tag: [u8; 32],
}

/// FEC protection level controlling overhead ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepairProtectionClass {
    /// Standard: +20% repair symbols ($K_{\text{repair}} = \lceil 0.20 \cdot K \rceil$).
    Standard,
    /// High: +50% repair symbols ($K_{\text{repair}} = \lceil 0.50 \cdot K \rceil$).
    High,
    /// Maximum: +100% repair symbols ($K_{\text{repair}} = K$).
    Maximum,
    /// Custom explicit repair symbol count.
    Custom { repair_symbol_count: usize },
}

impl RepairProtectionClass {
    /// Calculate the number of repair symbols for a given source symbol count $K$.
    #[must_use]
    pub fn repair_symbol_count(&self, k: usize) -> usize {
        match self {
            Self::Standard => ((k as f64 * 0.20).ceil() as usize).max(2),
            Self::High => ((k as f64 * 0.50).ceil() as usize).max(4),
            Self::Maximum => k.max(4),
            Self::Custom {
                repair_symbol_count,
            } => *repair_symbol_count,
        }
    }

    /// Return canonical byte representation for HMAC domain authentication.
    /// Format: 1-byte discriminant tag (0=Standard, 1=High, 2=Maximum, 3=Custom),
    /// followed by 8-byte little-endian repair symbol count.
    #[must_use]
    pub fn to_canonical_bytes(&self) -> [u8; 9] {
        let mut buf = [0u8; 9];
        match self {
            Self::Standard => {
                buf[0] = 0;
            }
            Self::High => {
                buf[0] = 1;
            }
            Self::Maximum => {
                buf[0] = 2;
            }
            Self::Custom {
                repair_symbol_count,
            } => {
                buf[0] = 3;
                buf[1..9].copy_from_slice(&(*repair_symbol_count as u64).to_le_bytes());
            }
        }
        buf
    }
}

/// External authenticated manifest describing the repair geometry and representation ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairManifest {
    /// SHA-256 digest of the entire serialized encrypted envelope.
    pub representation_id: [u8; 32],
    /// Semantic recovery object identifier (32-byte hash / UUID).
    pub object_id: [u8; 32],
    /// Monotonic generation counter of the recovery object.
    pub generation: u64,
    /// Source Block Number (must be 0).
    pub sbn: u8,
    /// Source symbol count $K$.
    pub k: u32,
    /// Fixed symbol size in bytes.
    pub symbol_size: u32,
    /// Exact byte length of the original serialized encrypted envelope.
    pub payload_len: u64,
    /// Total symbols generated ($K + K_{\text{repair}}$).
    pub total_symbols_generated: u32,
    /// Protection policy applied.
    pub protection_class: RepairProtectionClass,
    /// HMAC-SHA256 tag authenticating this manifest's geometry.
    pub manifest_mac: [u8; 32],
}

/// Self-contained bundle of manifest and authenticated symbols.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodedRepairBundle {
    /// Geometry and representation manifest.
    pub manifest: RepairManifest,
    /// Complete set of generated source and repair symbols.
    pub symbols: Vec<AuthenticatedRepairSymbol>,
}

/// Mandatory expected identity assertion tuple for recovery operations.
/// Prevents replay of stale generations or substitution of foreign authentic objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedRecoveryIdentity {
    /// Expected SHA-256 digest of the serialized encrypted representation envelope.
    pub representation_id: [u8; 32],
    /// Expected semantic recovery object identifier.
    pub object_id: [u8; 32],
    /// Expected monotonic generation.
    pub generation: u64,
}

impl ExpectedRecoveryIdentity {
    /// Create a new expected identity assertion tuple.
    #[must_use]
    pub const fn new(representation_id: [u8; 32], object_id: [u8; 32], generation: u64) -> Self {
        Self {
            representation_id,
            object_id,
            generation,
        }
    }

    /// Build fixture expectations; production callers supply independent identities.
    #[cfg(test)]
    #[must_use]
    pub const fn from_manifest(manifest: &RepairManifest) -> Self {
        Self {
            representation_id: manifest.representation_id,
            object_id: manifest.object_id,
            generation: manifest.generation,
        }
    }
}

/// Diagnostic equation rank details returned upon decode evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankStatusDiagnostic {
    /// Total equation rank achieved.
    pub rank: usize,
    /// Target column count (intermediate symbols $L$).
    pub columns: usize,
    /// Deficit ($L - \text{rank}$). Zero indicates full rank.
    pub deficit: usize,
}

/// Operational statistics from the repair decode operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairStats {
    /// Number of source symbols utilized.
    pub source_symbols_used: usize,
    /// Number of repair symbols utilized.
    pub repair_symbols_used: usize,
    /// Number of duplicate symbols discarded.
    pub duplicates_discarded: usize,
    /// Damaged or unauthenticated symbols excluded from the equation set.
    pub corrupt_symbols_discarded: usize,
    /// Final rank status.
    pub rank_status: RankStatusDiagnostic,
}

/// Output of a successful repair decode operation.
/// Returns a buffer alongside its admission reservation. Callers that separate
/// these public fields must retain the permit until the buffer is released.
/// Caller-owned inputs and allocator overhead are not a process-wide heap bound.
#[derive(Debug)]
pub struct RepairResult<'a> {
    /// Bit-for-bit recovered serialized encrypted envelope.
    pub reconstructed_envelope: Vec<u8>,
    /// Verified representation digest (matches `SHA256(reconstructed_envelope)`).
    pub representation_id: [u8; 32],
    /// Decode and symbol accounting statistics.
    pub stats: RepairStats,
    /// Active admission permit holding budget reservation for the retained buffer.
    pub permit: RepairPermit<'a>,
}

impl<'a> RepairResult<'a> {
    /// Consume the result, yielding the reconstructed envelope and the active budget permit.
    /// The caller must retain the permit for the lifetime of the returned buffer.
    #[must_use]
    pub fn into_parts(self) -> (Vec<u8>, RepairPermit<'a>) {
        (self.reconstructed_envelope, self.permit)
    }
}

// ---------------------------------------------------------------------------
// Error Types
// ---------------------------------------------------------------------------

/// Exhaustive error enumeration for snapshot repair encoding and decoding.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RepairError {
    #[error("Repair storage failed: {0}")]
    Storage(String),
    #[error("Repair persistence failed: {0}")]
    Persistence(String),
    #[error("Envelope byte payload is empty")]
    EmptyPayload,

    #[error("Envelope byte size {size} exceeds maximum allowable chunk limit {limit}")]
    PayloadTooLarge { size: usize, limit: usize },

    #[error("Invalid symbol size {size}: must be between {min} and {max}")]
    InvalidSymbolSize { size: usize, min: usize, max: usize },

    #[error("Authentication key is empty")]
    EmptyAuthenticationKey,

    #[error(
        "Source symbol count {k} exceeds per-object chunk limit {limit}; callers must chunk large recovery objects"
    )]
    ExceedsMaxSourceSymbols { k: usize, limit: usize },

    #[error("Multi-block SBN {sbn} unsupported: only single-block SBN=0 is supported")]
    MultiBlockUnsupported { sbn: u8 },

    #[error("Invalid manifest geometry: {reason}")]
    InvalidGeometry { reason: String },

    #[error("Manifest HMAC authentication failed")]
    ManifestAuthenticationFailed,

    #[error("Foreign representation ID: expected {expected:?}, got {got:?}")]
    ForeignRepresentationId { got: [u8; 32], expected: [u8; 32] },

    #[error("Foreign object ID: expected {expected:?}, got {got:?}")]
    ForeignObjectId { expected: [u8; 32], got: [u8; 32] },

    #[error("Foreign generation: expected {expected}, got {got}")]
    ForeignGeneration { expected: u64, got: u64 },

    #[error(
        "Conflicting duplicate symbol detected: ESI {esi} received with divergent payload data"
    )]
    ConflictingDuplicateSymbol { esi: u32 },

    #[error("Insufficient equation rank: achieved {rank}/{columns}, deficit {deficit}")]
    InsufficientRank {
        rank: usize,
        columns: usize,
        deficit: usize,
    },

    #[error("RaptorQ decoder failed: {0}")]
    DecoderError(String),

    #[error(
        "Recovered envelope SHA-256 digest mismatch: expected {expected:?}, calculated {actual:?}"
    )]
    RepresentationDigestMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },

    #[error("Admission limit exceeded: {0}")]
    AdmissionExceeded(String),

    #[error("Operation cancelled via context checkpoint")]
    Cancelled,

    #[error("Systematic encoder construction failed: singular constraint matrix")]
    EncoderSingularMatrix,
}

/// Bounds for one complete encrypted representation, independent of block geometry.
#[derive(Clone, Copy, Debug)]
pub struct RepairObjectLimits {
    pub max_envelope_bytes: usize,
    pub max_chunks: usize,
    pub max_descriptor_bytes: usize,
}

impl Default for RepairObjectLimits {
    fn default() -> Self {
        Self {
            max_envelope_bytes: 128 * 1024 * 1024,
            max_chunks: 4096,
            max_descriptor_bytes: 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairObjectChunk {
    pub offset: u64,
    pub manifest: RepairManifest,
    pub records_object_id: String,
}

impl RepairObjectChunk {
    /// Exact fixed-stride file bound, derived only from authenticated geometry.
    pub fn record_bytes_len(&self) -> Result<usize, RepairError> {
        (self.manifest.symbol_size as usize)
            .checked_add(38)
            .and_then(|stride| stride.checked_mul(self.manifest.total_symbols_generated as usize))
            .ok_or_else(|| RepairError::AdmissionExceeded("record file size overflow".into()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairObjectDescriptor {
    pub identity: ExpectedRecoveryIdentity,
    pub payload_len: u64,
    pub chunks: Vec<RepairObjectChunk>,
    #[serde(skip)]
    authenticated_mac: [u8; 32],
}

const REPAIR_DESCRIPTOR_DOMAIN: &[u8] = b"frankenterm-repair-object-descriptor-v1";
const REPAIR_DESCRIPTOR_MAGIC: &[u8; 8] = b"FTREPR01";

fn repair_descriptor_mac(key: &[u8]) -> Result<HmacSha256, RepairError> {
    if key.is_empty() {
        return Err(RepairError::EmptyAuthenticationKey);
    }
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| RepairError::ManifestAuthenticationFailed)?;
    mac.update(REPAIR_DESCRIPTOR_DOMAIN);
    Ok(mac)
}

fn repair_chunk_id(identity: &ExpectedRecoveryIdentity, index: usize) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"frankenterm-repair-object-chunk-v1");
    hash.update(identity.representation_id);
    hash.update(identity.object_id);
    hash.update(identity.generation.to_le_bytes());
    hash.update((index as u64).to_le_bytes());
    hash.finalize().into()
}

impl RepairObjectDescriptor {
    fn validate(
        &self,
        expected: &ExpectedRecoveryIdentity,
        key: &[u8],
        limits: RepairObjectLimits,
    ) -> Result<(), RepairError> {
        if self.identity.representation_id != expected.representation_id {
            return Err(RepairError::ForeignRepresentationId {
                got: self.identity.representation_id,
                expected: expected.representation_id,
            });
        }
        if self.identity.object_id != expected.object_id {
            return Err(RepairError::ForeignObjectId {
                got: self.identity.object_id,
                expected: expected.object_id,
            });
        }
        if self.identity.generation != expected.generation {
            return Err(RepairError::ForeignGeneration {
                got: self.identity.generation,
                expected: expected.generation,
            });
        }
        if self.payload_len == 0
            || self.payload_len > limits.max_envelope_bytes as u64
            || self.chunks.is_empty()
            || self.chunks.len() > limits.max_chunks
        {
            return Err(RepairError::AdmissionExceeded(
                "repair object bounds".into(),
            ));
        }
        let mut offset = 0u64;
        for (index, chunk) in self.chunks.iter().enumerate() {
            let m = &chunk.manifest;
            if !verify_manifest_mac(key, m) {
                return Err(RepairError::ManifestAuthenticationFailed);
            }
            let size = m.symbol_size as usize;
            if chunk.offset != offset
                || m.object_id != repair_chunk_id(expected, index)
                || m.generation != expected.generation
                || m.sbn != 0
                || m.k == 0
                || m.k as usize > MAX_PERSISTED_SOURCE_SYMBOLS
                || !(MIN_SYMBOL_SIZE..=MAX_SYMBOL_SIZE).contains(&size)
                || m.payload_len == 0
                || m.payload_len > MAX_PERSISTED_CHUNK_BYTES as u64
                || m.payload_len.div_ceil(u64::from(m.symbol_size)) != u64::from(m.k)
                || m.total_symbols_generated < m.k
                || chunk.records_object_id.is_empty()
                || chunk.records_object_id.len() > 128
                || !chunk
                    .records_object_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(RepairError::InvalidGeometry {
                    reason: "invalid ordered repair chunk".into(),
                });
            }
            offset = offset
                .checked_add(m.payload_len)
                .ok_or_else(|| RepairError::AdmissionExceeded("chunk offset overflow".into()))?;
        }
        if offset != self.payload_len {
            return Err(RepairError::InvalidGeometry {
                reason: "repair chunk coverage mismatch".into(),
            });
        }
        Ok(())
    }

    pub fn to_authenticated_bytes(
        &self,
        key: &[u8],
        limits: RepairObjectLimits,
    ) -> Result<Vec<u8>, RepairError> {
        self.validate(&self.identity, key, limits)?;
        let payload =
            serde_json::to_vec(self).map_err(|e| RepairError::Persistence(e.to_string()))?;
        if payload.len().saturating_add(40) > limits.max_descriptor_bytes {
            return Err(RepairError::AdmissionExceeded(
                "repair descriptor bytes".into(),
            ));
        }
        let mut mac = repair_descriptor_mac(key)?;
        mac.update(REPAIR_DESCRIPTOR_MAGIC);
        mac.update(&payload);
        let mut bytes = Vec::with_capacity(payload.len() + 40);
        bytes.extend_from_slice(REPAIR_DESCRIPTOR_MAGIC);
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(&mac.finalize().into_bytes());
        Ok(bytes)
    }

    pub fn from_authenticated_bytes(
        bytes: &[u8],
        expected: &ExpectedRecoveryIdentity,
        key: &[u8],
        limits: RepairObjectLimits,
    ) -> Result<Self, RepairError> {
        if bytes.len() < 40
            || bytes.len() > limits.max_descriptor_bytes
            || !bytes.starts_with(REPAIR_DESCRIPTOR_MAGIC)
        {
            return Err(RepairError::ManifestAuthenticationFailed);
        }
        let tag_at = bytes.len() - 32;
        let mut mac = repair_descriptor_mac(key)?;
        mac.update(&bytes[..tag_at]);
        mac.verify_slice(&bytes[tag_at..])
            .map_err(|_| RepairError::ManifestAuthenticationFailed)?;
        let mut descriptor: Self = serde_json::from_slice(&bytes[8..tag_at])
            .map_err(|e| RepairError::Persistence(e.to_string()))?;
        descriptor
            .authenticated_mac
            .copy_from_slice(&bytes[tag_at..]);
        descriptor.validate(expected, key, limits)?;
        Ok(descriptor)
    }
}

/// Encode one bounded block at a time and persist its fixed-stride records.
/// The callback must durably retain the records before returning their immutable ID.
pub fn encode_repair_object(
    cx: &Cx,
    envelope: &[u8],
    object_id: [u8; 32],
    generation: u64,
    key: &[u8],
    symbol_size: usize,
    protection: RepairProtectionClass,
    admission: &RepairAdmissionController,
    limits: RepairObjectLimits,
    mut persist: impl FnMut(usize, &RepairManifest, &[u8]) -> Result<String, RepairError>,
) -> Result<RepairObjectDescriptor, RepairError> {
    if envelope.is_empty() || envelope.len() > limits.max_envelope_bytes {
        return Err(RepairError::AdmissionExceeded("repair object bytes".into()));
    }
    if !(MIN_SYMBOL_SIZE..=MAX_SYMBOL_SIZE).contains(&symbol_size) {
        return Err(RepairError::InvalidSymbolSize {
            size: symbol_size,
            min: MIN_SYMBOL_SIZE,
            max: MAX_SYMBOL_SIZE,
        });
    }
    let chunk_size = MAX_PERSISTED_CHUNK_BYTES.min(symbol_size * MAX_PERSISTED_SOURCE_SYMBOLS);
    let count = envelope.len().div_ceil(chunk_size);
    if count > limits.max_chunks {
        return Err(RepairError::AdmissionExceeded(
            "repair object chunk count".into(),
        ));
    }
    let identity =
        ExpectedRecoveryIdentity::new(Sha256::digest(envelope).into(), object_id, generation);
    let mut chunks = Vec::with_capacity(count);
    for (index, bytes) in envelope.chunks(chunk_size).enumerate() {
        cx.checkpoint().map_err(|_| RepairError::Cancelled)?;
        let k = bytes.len().div_ceil(symbol_size);
        let total = k
            .checked_add(protection.repair_symbol_count(k))
            .ok_or_else(|| RepairError::AdmissionExceeded("symbol count overflow".into()))?;
        if total > admission.max_symbols_buffered {
            return Err(RepairError::AdmissionExceeded("record symbol count".into()));
        }
        let retained = total
            .checked_mul(2 * symbol_size + 38 + std::mem::size_of::<AuthenticatedRepairSymbol>())
            .ok_or_else(|| RepairError::AdmissionExceeded("record bytes overflow".into()))?;
        let _retained = admission.acquire(retained)?;
        let bundle = encode_repair_envelope(
            cx,
            bytes,
            repair_chunk_id(&identity, index),
            generation,
            key,
            symbol_size,
            protection,
            admission,
        )?;
        let mut records = Vec::with_capacity(total * (symbol_size + 38));
        for symbol in &bundle.symbols {
            cx.checkpoint().map_err(|_| RepairError::Cancelled)?;
            records.push(symbol.kind.to_byte());
            records.push(symbol.sbn);
            records.extend_from_slice(&symbol.esi.to_le_bytes());
            records.extend_from_slice(&symbol.tag);
            records.extend_from_slice(&symbol.payload);
        }
        cx.checkpoint().map_err(|_| RepairError::Cancelled)?;
        let records_object_id = persist(index, &bundle.manifest, &records)?;
        chunks.push(RepairObjectChunk {
            offset: (index * chunk_size) as u64,
            manifest: bundle.manifest,
            records_object_id,
        });
    }
    let mut descriptor = RepairObjectDescriptor {
        identity,
        payload_len: envelope.len() as u64,
        chunks,
        authenticated_mac: [0; 32],
    };
    descriptor.validate(&identity, key, limits)?;
    let encoded = descriptor.to_authenticated_bytes(key, limits)?;
    descriptor
        .authenticated_mac
        .copy_from_slice(&encoded[encoded.len() - 32..]);
    Ok(descriptor)
}

/// Recover chunks lazily. The complete output remains admission-reserved until
/// the caller drops the returned permit; input file buffers are per-chunk.
pub fn decode_repair_object<'a>(
    cx: &Cx,
    descriptor: &RepairObjectDescriptor,
    expected: &ExpectedRecoveryIdentity,
    key: &[u8],
    admission: &'a RepairAdmissionController,
    limits: RepairObjectLimits,
    mut load: impl FnMut(&RepairObjectChunk) -> Result<Vec<u8>, RepairError>,
) -> Result<RepairResult<'a>, RepairError> {
    cx.checkpoint().map_err(|_| RepairError::Cancelled)?;
    descriptor.validate(expected, key, limits)?;
    // Reauthenticate the complete ordered descriptor, including object locators,
    // even if a caller has mutated its public projection since decoding it.
    let encoded = descriptor.to_authenticated_bytes(key, limits)?;
    let mut mac = repair_descriptor_mac(key)?;
    mac.update(&encoded[..encoded.len() - 32]);
    mac.verify_slice(&descriptor.authenticated_mac)
        .map_err(|_| RepairError::ManifestAuthenticationFailed)?;
    let output_len = usize::try_from(descriptor.payload_len)
        .map_err(|_| RepairError::AdmissionExceeded("output length overflow".into()))?;
    let permit = admission.acquire(output_len)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(output_len)
        .map_err(|_| RepairError::AdmissionExceeded("output allocation".into()))?;
    let mut stats = RepairStats {
        source_symbols_used: 0,
        repair_symbols_used: 0,
        duplicates_discarded: 0,
        corrupt_symbols_discarded: 0,
        rank_status: RankStatusDiagnostic {
            rank: 0,
            columns: 0,
            deficit: 0,
        },
    };
    for chunk in &descriptor.chunks {
        cx.checkpoint().map_err(|_| RepairError::Cancelled)?;
        let maximum = chunk.record_bytes_len()?;
        let count = chunk.manifest.total_symbols_generated as usize;
        if count > admission.max_symbols_buffered {
            return Err(RepairError::AdmissionExceeded("record symbol count".into()));
        }
        let retained = maximum
            .checked_mul(2)
            .and_then(|n| {
                n.checked_add(count.checked_mul(std::mem::size_of::<AuthenticatedRepairSymbol>())?)
            })
            .ok_or_else(|| RepairError::AdmissionExceeded("record allocation overflow".into()))?;
        let _records_permit = admission.acquire(retained)?;
        let records = load(chunk)?;
        if records.len() > maximum {
            return Err(RepairError::AdmissionExceeded(
                "record file too large".into(),
            ));
        }
        let stride = chunk.manifest.symbol_size as usize + 38;
        let mut symbols = Vec::with_capacity(records.len() / stride);
        for record in records.chunks_exact(stride) {
            cx.checkpoint().map_err(|_| RepairError::Cancelled)?;
            let kind = match record[0] {
                0 => RepairSymbolKind::Source,
                1 => RepairSymbolKind::Repair,
                _ => {
                    stats.corrupt_symbols_discarded += 1;
                    continue;
                }
            };
            if record[1] != 0 {
                stats.corrupt_symbols_discarded += 1;
                continue;
            }
            let mut esi = [0; 4];
            esi.copy_from_slice(&record[2..6]);
            let mut tag = [0; 32];
            tag.copy_from_slice(&record[6..38]);
            let symbol = AuthenticatedRepairSymbol {
                kind,
                sbn: 0,
                esi: u32::from_le_bytes(esi),
                tag,
                payload: record[38..].to_vec(),
            };
            if verify_symbol_mac(key, &chunk.manifest, &symbol) {
                symbols.push(symbol);
            } else {
                stats.corrupt_symbols_discarded += 1;
            }
        }
        if records.len() % stride != 0 {
            stats.corrupt_symbols_discarded += 1;
        }
        let chunk_expected = ExpectedRecoveryIdentity::new(
            chunk.manifest.representation_id,
            chunk.manifest.object_id,
            expected.generation,
        );
        let recovered = decode_repair_symbols(
            cx,
            &chunk.manifest,
            &symbols,
            &chunk_expected,
            key,
            admission,
        )?;
        stats.source_symbols_used += recovered.stats.source_symbols_used;
        stats.repair_symbols_used += recovered.stats.repair_symbols_used;
        stats.duplicates_discarded += recovered.stats.duplicates_discarded;
        stats.corrupt_symbols_discarded += recovered.stats.corrupt_symbols_discarded;
        stats.rank_status.rank += recovered.stats.rank_status.rank;
        stats.rank_status.columns += recovered.stats.rank_status.columns;
        output.extend_from_slice(&recovered.reconstructed_envelope);
    }
    cx.checkpoint().map_err(|_| RepairError::Cancelled)?;
    let actual: [u8; 32] = Sha256::digest(&output).into();
    if output.len() != output_len || actual != expected.representation_id {
        return Err(RepairError::RepresentationDigestMismatch {
            expected: expected.representation_id,
            actual,
        });
    }
    Ok(RepairResult {
        reconstructed_envelope: output,
        representation_id: actual,
        stats,
        permit,
    })
}

// ---------------------------------------------------------------------------
// Admission Control (FCP Pattern)
// ---------------------------------------------------------------------------

static SHARED_ADMISSION_CONTROLLER: OnceLock<RepairAdmissionController> = OnceLock::new();

/// Global shared admission controller enforcing system-wide concurrency and memory limits.
#[must_use]
pub fn shared_admission_controller() -> &'static RepairAdmissionController {
    SHARED_ADMISSION_CONTROLLER.get_or_init(RepairAdmissionController::default_production)
}

/// Bounded resource admission controller for snapshot repair operations.
#[derive(Debug)]
pub struct RepairAdmissionController {
    /// Maximum concurrent active encode/decode operations.
    pub max_concurrent: usize,
    /// Maximum aggregate heap memory bytes permitted across active operations.
    pub max_memory_bytes: usize,
    /// Maximum symbol count buffered per operation to prevent unbounded allocations.
    pub max_symbols_buffered: usize,
    active_operations: AtomicUsize,
    allocated_memory_bytes: AtomicUsize,
}

impl RepairAdmissionController {
    /// Create a new admission controller with explicit operational bounds.
    #[must_use]
    pub fn new(
        max_concurrent: usize,
        max_memory_bytes: usize,
        max_symbols_buffered: usize,
    ) -> Self {
        Self {
            max_concurrent,
            max_memory_bytes,
            max_symbols_buffered,
            active_operations: AtomicUsize::new(0),
            allocated_memory_bytes: AtomicUsize::new(0),
        }
    }

    /// Default production admission profile.
    #[must_use]
    pub fn default_production() -> Self {
        Self::new(8, 256 * 1024 * 1024, 8_192)
    }

    /// Try to acquire an operational permit reserving memory and concurrency.
    /// Employs checked CAS loops to prevent arithmetic wrapping before bounds verification.
    pub fn acquire(&self, estimated_bytes: usize) -> Result<RepairPermit<'_>, RepairError> {
        // Concurrency reservation via checked CAS loop
        let mut current_ops = self.active_operations.load(Ordering::Acquire);
        loop {
            if current_ops >= self.max_concurrent {
                return Err(RepairError::AdmissionExceeded(format!(
                    "concurrency limit reached ({current_ops}/{})",
                    self.max_concurrent
                )));
            }
            let next_ops = current_ops.checked_add(1).ok_or_else(|| {
                RepairError::AdmissionExceeded("concurrency counter overflow".to_string())
            })?;
            match self.active_operations.compare_exchange_weak(
                current_ops,
                next_ops,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current_ops = actual,
            }
        }

        // Memory reservation via checked CAS loop
        let mut current_mem = self.allocated_memory_bytes.load(Ordering::Acquire);
        loop {
            let next_mem = match current_mem.checked_add(estimated_bytes) {
                Some(m) if m <= self.max_memory_bytes => m,
                Some(m) => {
                    self.active_operations.fetch_sub(1, Ordering::Release);
                    return Err(RepairError::AdmissionExceeded(format!(
                        "memory limit exceeded (requested {estimated_bytes} bytes, current {current_mem}, needed {m}, max {})",
                        self.max_memory_bytes
                    )));
                }
                None => {
                    self.active_operations.fetch_sub(1, Ordering::Release);
                    return Err(RepairError::AdmissionExceeded(format!(
                        "memory allocation counter overflow (current {current_mem}, requested {estimated_bytes})"
                    )));
                }
            };

            match self.allocated_memory_bytes.compare_exchange_weak(
                current_mem,
                next_mem,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current_mem = actual,
            }
        }

        Ok(RepairPermit {
            controller: self,
            allocated_bytes: estimated_bytes,
            released: false,
        })
    }

    /// Current number of active operations holding permits.
    #[must_use]
    pub fn active_operations(&self) -> usize {
        self.active_operations.load(Ordering::Acquire)
    }

    /// Current number of aggregate memory bytes allocated across active permits.
    #[must_use]
    pub fn allocated_memory_bytes(&self) -> usize {
        self.allocated_memory_bytes.load(Ordering::Acquire)
    }
}

/// RAII permit releasing reserved memory and concurrency slots on drop.
pub struct RepairPermit<'a> {
    controller: &'a RepairAdmissionController,
    allocated_bytes: usize,
    released: bool,
}

impl RepairPermit<'_> {
    /// Adjust the allocated bytes down to the retained buffer size.
    /// Releases the difference back to the admission controller while retaining
    /// the charge for the surviving buffer.
    pub fn shrink_to(&mut self, retained_bytes: usize) {
        if self.released {
            return;
        }
        if retained_bytes < self.allocated_bytes {
            let diff = self.allocated_bytes - retained_bytes;
            self.controller
                .allocated_memory_bytes
                .fetch_sub(diff, Ordering::Release);
            self.allocated_bytes = retained_bytes;
        }
    }

    /// Explicitly release the permit, returning reserved memory and concurrency to the controller.
    pub fn release(&mut self) {
        if !self.released {
            self.controller
                .allocated_memory_bytes
                .fetch_sub(self.allocated_bytes, Ordering::Release);
            self.controller
                .active_operations
                .fetch_sub(1, Ordering::Release);
            self.released = true;
        }
    }

    /// Number of bytes currently held by this permit.
    #[must_use]
    pub const fn allocated_bytes(&self) -> usize {
        self.allocated_bytes
    }
}

impl Drop for RepairPermit<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

impl std::fmt::Debug for RepairPermit<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepairPermit")
            .field("allocated_bytes", &self.allocated_bytes)
            .field("released", &self.released)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Memory Budget Estimation (Checked Arithmetic)
// ---------------------------------------------------------------------------

/// Estimate the decoder working set for initialization, equation matrix storage,
/// dense inactivation solve, and intermediate symbols. This is an admission
/// estimate for the pinned decoder, not an allocator-level heap measurement.
///
/// Accounts for:
/// 1. Total matrix rows: matrix_rows = received_symbol_count + S + H + (K' - K).
/// 2. Dense rank evaluation & inactivation matrices:
///    - dense_rows in rank profiling: matrix_rows * L in GF(256)
///    - flat dense matrix A in inactivate_and_solve: matrix_rows * L in GF(256)
///    - dense submatrix clone / factor cache during elimination: matrix_rows * L in GF(256)
///    - rank basis vectors in coefficient_rank_profile: L * L in GF(256)
///
///    Charged as: 3 * (matrix_rows * L) + (L * L).
/// 3. HDPC and dense equations having up to L aligned (usize, GF256) terms,
///    vector/struct allocation headers (~128 bytes), and RHS symbol buffers (symbol_size).
///    Solver clones equation and RHS rows while original equation list is held:
///    Charged as: 2 * matrix_rows * (L * size_of::<(usize, u8)>() + 128 + symbol_size).
/// 4. Full decoder state symbol payload buffers:
///    - intermediate symbols (L * symbol_size)
///    - solved array in DecoderState (L * symbol_size)
///    - source symbols (K * symbol_size)
///    - output reconstructed envelope (K * symbol_size)
///    - received symbol data (received_symbol_count * symbol_size)
///
///    Charged as: (2L + 2K + received_symbol_count) * symbol_size.
/// 5. Full state auxiliary vector overhead:
///    - column_states (L * 1)
///    - dense_rows indices, unsolved indices, stats tracking overhead (~64 bytes/row).
fn calculate_decoder_memory_budget(
    params: &SystematicParams,
    received_symbol_count: usize,
    symbol_size: usize,
) -> Result<usize, RepairError> {
    let l = params.l;
    let k = params.k;
    let s = params.s;
    let h = params.h;
    let k_prime = params.k_prime;

    // 1. Symbol payload buffers (checked)
    // Intermediate symbols (L) + solved array (L) + source symbols (K) + output assembly (K) + received symbols (received_symbol_count)
    let total_symbol_slots = l
        .checked_mul(2)
        .and_then(|v| v.checked_add(k.checked_mul(2)?))
        .and_then(|v| v.checked_add(received_symbol_count))
        .ok_or_else(|| {
            RepairError::AdmissionExceeded("symbol buffer count overflow".to_string())
        })?;

    let symbol_payload_bytes = total_symbol_slots.checked_mul(symbol_size).ok_or_else(|| {
        RepairError::AdmissionExceeded("symbol payload bytes overflow".to_string())
    })?;

    // 2. Total matrix rows: received symbols + base constraints (S + H) + padding rows (K' - K)
    let padding_rows = k_prime.saturating_sub(k);
    let matrix_rows = s
        .checked_add(h)
        .and_then(|v| v.checked_add(padding_rows))
        .and_then(|v| v.checked_add(received_symbol_count))
        .ok_or_else(|| RepairError::AdmissionExceeded("equation row count overflow".to_string()))?;

    // 3. Dense matrices and rank evaluation basis:
    // - dense_rows in rank profiling: matrix_rows * L
    // - flat dense matrix A in inactivate_and_solve: matrix_rows * L
    // - solver submatrix clone / factor cache: matrix_rows * L
    // - coefficient_rank_profile basis: L * L
    let matrix_cells = matrix_rows
        .checked_mul(l)
        .ok_or_else(|| RepairError::AdmissionExceeded("dense matrix cells overflow".to_string()))?;

    let dense_matrix_bytes = matrix_cells
        .checked_mul(3)
        .ok_or_else(|| RepairError::AdmissionExceeded("dense matrix bytes overflow".to_string()))?;

    let basis_bytes = l
        .checked_mul(l)
        .ok_or_else(|| RepairError::AdmissionExceeded("rank basis bytes overflow".to_string()))?;

    let total_matrix_bytes = dense_matrix_bytes.checked_add(basis_bytes).ok_or_else(|| {
        RepairError::AdmissionExceeded("total dense matrix bytes overflow".to_string())
    })?;

    // 4. Equation structures and RHS
    // Account for tuple alignment, not just the sum of index and GF256 field widths.
    // Plus vector/struct allocation headers (~128 bytes) + RHS symbol payload (symbol_size).
    let term_bytes = l
        .checked_mul(std::mem::size_of::<(usize, u8)>())
        .ok_or_else(|| {
            RepairError::AdmissionExceeded("equation term bytes overflow".to_string())
        })?;

    const STRUCT_AND_VEC_HEADER_OVERHEAD: usize = 128;
    let single_equation_slot_bytes = term_bytes
        .checked_add(STRUCT_AND_VEC_HEADER_OVERHEAD)
        .and_then(|v| v.checked_add(symbol_size))
        .ok_or_else(|| RepairError::AdmissionExceeded("equation slot size overflow".to_string()))?;

    // The solver clones equation and RHS rows while original equation list is held; charge 2x.
    let equation_bytes = matrix_rows
        .checked_mul(single_equation_slot_bytes)
        .and_then(|v| v.checked_mul(2))
        .ok_or_else(|| RepairError::AdmissionExceeded("equation memory overflow".to_string()))?;

    // 5. Full state overhead: auxiliary vectors (column_states, dense_rows, unsolved, stats)
    let auxiliary_overhead = matrix_rows
        .checked_mul(64)
        .and_then(|v| v.checked_add(l.checked_mul(32)?))
        .ok_or_else(|| {
            RepairError::AdmissionExceeded("auxiliary state overhead overflow".to_string())
        })?;

    // Sum all components safely
    let total_memory = symbol_payload_bytes
        .checked_add(total_matrix_bytes)
        .and_then(|v| v.checked_add(equation_bytes))
        .and_then(|v| v.checked_add(auxiliary_overhead))
        .ok_or_else(|| {
            RepairError::AdmissionExceeded("total decoder memory budget overflow".to_string())
        })?;

    Ok(total_memory)
}

/// Estimate comprehensive memory consumption for encoder intermediate symbol
/// solve and repair symbol generation.
/// Accounts for Systematic solve cloning matrix and RHS while originals are alive.
fn calculate_encoder_memory_budget(
    params: &SystematicParams,
    total_symbols: usize,
    symbol_size: usize,
) -> Result<usize, RepairError> {
    let l = params.l;
    let k = params.k;
    let s = params.s;
    let h = params.h;
    let k_prime = params.k_prime;

    // Intermediate symbols (l) + source symbols (k) + generated symbols (total_symbols)
    let total_symbol_slots = l
        .checked_add(k)
        .and_then(|v| v.checked_add(total_symbols))
        .ok_or_else(|| {
            RepairError::AdmissionExceeded("encoder symbol count overflow".to_string())
        })?;

    let symbol_payload_bytes = total_symbol_slots.checked_mul(symbol_size).ok_or_else(|| {
        RepairError::AdmissionExceeded("encoder symbol payload overflow".to_string())
    })?;

    // Constraint matrix solve: (S + H + K') rows * L columns in GF(256)
    let matrix_rows = s
        .checked_add(h)
        .and_then(|v| v.checked_add(k_prime))
        .ok_or_else(|| {
            RepairError::AdmissionExceeded("encoder matrix rows overflow".to_string())
        })?;

    let single_matrix_bytes = matrix_rows.checked_mul(l).ok_or_else(|| {
        RepairError::AdmissionExceeded("encoder matrix bytes overflow".to_string())
    })?;

    // Solver clones the constraint matrix; charge 2x
    let matrix_bytes = single_matrix_bytes.checked_mul(2).ok_or_else(|| {
        RepairError::AdmissionExceeded("encoder matrix clone bytes overflow".to_string())
    })?;

    // Solver clones the RHS values: matrix_rows * symbol_size; charge 2x
    let single_rhs_bytes = matrix_rows
        .checked_mul(symbol_size)
        .ok_or_else(|| RepairError::AdmissionExceeded("encoder rhs bytes overflow".to_string()))?;

    let rhs_bytes = single_rhs_bytes.checked_mul(2).ok_or_else(|| {
        RepairError::AdmissionExceeded("encoder rhs clone bytes overflow".to_string())
    })?;

    let total_memory = symbol_payload_bytes
        .checked_add(matrix_bytes)
        .and_then(|v| v.checked_add(rhs_bytes))
        .ok_or_else(|| {
            RepairError::AdmissionExceeded("encoder total memory budget overflow".to_string())
        })?;

    Ok(total_memory)
}

// ---------------------------------------------------------------------------
// Cryptographic Authentication (Vetted HMAC-SHA256)
// ---------------------------------------------------------------------------

/// Compute HMAC-SHA256 for a repair manifest using the standard `hmac` crate.
/// Authenticates geometry, representation identity, object identity, and protection policy.
pub fn compute_manifest_mac(
    key: &[u8],
    representation_id: &[u8; 32],
    object_id: &[u8; 32],
    generation: u64,
    protection_class: RepairProtectionClass,
    sbn: u8,
    k: u32,
    symbol_size: u32,
    payload_len: u64,
    total_symbols: u32,
) -> Result<[u8; 32], RepairError> {
    if key.is_empty() {
        return Err(RepairError::EmptyAuthenticationKey);
    }
    let key_guard = Zeroizing::new(key.to_vec());
    let mut mac =
        HmacSha256::new_from_slice(&key_guard).map_err(|e| RepairError::InvalidGeometry {
            reason: format!("HMAC key initialization failed: {e}"),
        })?;

    mac.update(MANIFEST_MAC_DOMAIN);
    mac.update(representation_id);
    mac.update(object_id);
    mac.update(&generation.to_le_bytes());
    mac.update(&protection_class.to_canonical_bytes());
    mac.update(&[sbn]);
    mac.update(&k.to_le_bytes());
    mac.update(&symbol_size.to_le_bytes());
    mac.update(&payload_len.to_le_bytes());
    mac.update(&total_symbols.to_le_bytes());

    let output = mac.finalize().into_bytes();
    let mut result = [0u8; 32];
    result.copy_from_slice(&output);
    Ok(result)
}

/// Verify HMAC-SHA256 for a repair manifest using constant-time `verify_slice`.
#[must_use]
pub fn verify_manifest_mac(key: &[u8], manifest: &RepairManifest) -> bool {
    if key.is_empty() {
        return false;
    }
    let key_guard = Zeroizing::new(key.to_vec());
    let Ok(mut mac) = HmacSha256::new_from_slice(&key_guard) else {
        return false;
    };

    mac.update(MANIFEST_MAC_DOMAIN);
    mac.update(&manifest.representation_id);
    mac.update(&manifest.object_id);
    mac.update(&manifest.generation.to_le_bytes());
    mac.update(&manifest.protection_class.to_canonical_bytes());
    mac.update(&[manifest.sbn]);
    mac.update(&manifest.k.to_le_bytes());
    mac.update(&manifest.symbol_size.to_le_bytes());
    mac.update(&manifest.payload_len.to_le_bytes());
    mac.update(&manifest.total_symbols_generated.to_le_bytes());

    mac.verify_slice(&manifest.manifest_mac).is_ok()
}

/// Compute HMAC-SHA256 for a single symbol using the standard `hmac` crate.
pub fn compute_symbol_mac(
    key: &[u8],
    representation_id: &[u8; 32],
    object_id: &[u8; 32],
    generation: u64,
    sbn: u8,
    kind: RepairSymbolKind,
    esi: u32,
    payload: &[u8],
) -> Result<[u8; 32], RepairError> {
    if key.is_empty() {
        return Err(RepairError::EmptyAuthenticationKey);
    }
    let key_guard = Zeroizing::new(key.to_vec());
    let mut mac =
        HmacSha256::new_from_slice(&key_guard).map_err(|e| RepairError::InvalidGeometry {
            reason: format!("HMAC key initialization failed: {e}"),
        })?;

    mac.update(SYMBOL_MAC_DOMAIN);
    mac.update(representation_id);
    mac.update(object_id);
    mac.update(&generation.to_le_bytes());
    mac.update(&[sbn]);
    mac.update(&[kind.to_byte()]);
    mac.update(&esi.to_le_bytes());
    mac.update(payload);

    let output = mac.finalize().into_bytes();
    let mut result = [0u8; 32];
    result.copy_from_slice(&output);
    Ok(result)
}

/// Verify HMAC-SHA256 for a single symbol against expected manifest metadata using `verify_slice`.
#[must_use]
pub fn verify_symbol_mac(
    key: &[u8],
    manifest: &RepairManifest,
    symbol: &AuthenticatedRepairSymbol,
) -> bool {
    if key.is_empty() {
        return false;
    }
    let key_guard = Zeroizing::new(key.to_vec());
    let Ok(mut mac) = HmacSha256::new_from_slice(&key_guard) else {
        return false;
    };

    mac.update(SYMBOL_MAC_DOMAIN);
    mac.update(&manifest.representation_id);
    mac.update(&manifest.object_id);
    mac.update(&manifest.generation.to_le_bytes());
    mac.update(&[manifest.sbn]);
    mac.update(&[symbol.kind.to_byte()]);
    mac.update(&symbol.esi.to_le_bytes());
    mac.update(&symbol.payload);

    mac.verify_slice(&symbol.tag).is_ok()
}

/// Deterministically derive the RaptorQ pseudo-random generator seed from representation ID.
#[must_use]
pub fn derive_raptorq_seed(representation_id: &[u8; 32], sbn: u8) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"frankenterm-raptorq-seed-derivation-v1");
    hasher.update(representation_id);
    hasher.update([sbn]);
    let digest = hasher.finalize();
    let mut seed_bytes = [0u8; 8];
    seed_bytes.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(seed_bytes)
}

// ---------------------------------------------------------------------------
// Encoding Pipeline
// ---------------------------------------------------------------------------

/// Encode an authenticated serialized encrypted envelope into systematic and repair symbols.
///
/// # Arguments
/// - `cx`: Asupersync capability context for cooperative cancellation.
/// - `envelope_bytes`: The entire serialized encrypted recovery object representation.
/// - `object_id`: Semantic object identifier (32-byte digest/UUID).
/// - `generation`: Monotonic object generation.
/// - `auth_key`: HMAC-SHA256 authentication key.
/// - `symbol_size`: Symbol payload size in bytes (must be within `[MIN_SYMBOL_SIZE, MAX_SYMBOL_SIZE]`).
/// - `protection`: FEC protection policy (Standard, High, Maximum, or Custom).
/// - `admission`: Admission controller managing operational memory and concurrency bounds.
pub fn encode_repair_envelope(
    cx: &Cx,
    envelope_bytes: &[u8],
    object_id: [u8; 32],
    generation: u64,
    auth_key: &[u8],
    symbol_size: usize,
    protection: RepairProtectionClass,
    admission: &RepairAdmissionController,
) -> Result<EncodedRepairBundle, RepairError> {
    if cx.checkpoint().is_err() {
        return Err(RepairError::Cancelled);
    }

    if envelope_bytes.is_empty() {
        return Err(RepairError::EmptyPayload);
    }

    if auth_key.is_empty() {
        return Err(RepairError::EmptyAuthenticationKey);
    }
    let auth_key_guard = Zeroizing::new(auth_key.to_vec());
    let auth_key = auth_key_guard.as_slice();

    if envelope_bytes.len() > MAX_CHUNK_ENVELOPE_BYTES {
        return Err(RepairError::PayloadTooLarge {
            size: envelope_bytes.len(),
            limit: MAX_CHUNK_ENVELOPE_BYTES,
        });
    }

    if !(MIN_SYMBOL_SIZE..=MAX_SYMBOL_SIZE).contains(&symbol_size) {
        return Err(RepairError::InvalidSymbolSize {
            size: symbol_size,
            min: MIN_SYMBOL_SIZE,
            max: MAX_SYMBOL_SIZE,
        });
    }

    // Single-block partition: K = ceil(payload_len / symbol_size)
    let payload_len = envelope_bytes.len();
    let k = payload_len.div_ceil(symbol_size);

    if k > MAX_CHUNK_SOURCE_SYMBOLS {
        return Err(RepairError::ExceedsMaxSourceSymbols {
            k,
            limit: MAX_CHUNK_SOURCE_SYMBOLS,
        });
    }

    let repair_count = protection.repair_symbol_count(k);
    let total_symbols = k.checked_add(repair_count).ok_or_else(|| {
        RepairError::AdmissionExceeded("total symbols calculation overflow".to_string())
    })?;

    // Pre-allocation buffering guard: enforce max_symbols_buffered BEFORE vector allocation
    if total_symbols > admission.max_symbols_buffered {
        return Err(RepairError::AdmissionExceeded(format!(
            "total generated symbol count {total_symbols} exceeds max_symbols_buffered threshold {}",
            admission.max_symbols_buffered
        )));
    }

    // Compute systematic parameters to charge checked memory budget
    let params = SystematicParams::try_for_source_block(k, symbol_size).map_err(|e| {
        RepairError::InvalidGeometry {
            reason: format!("systematic parameter calculation failed: {e:?}"),
        }
    })?;

    let estimated_memory = calculate_encoder_memory_budget(&params, total_symbols, symbol_size)?;
    let _permit = admission.acquire(estimated_memory)?;

    // Calculate exact representation ID = SHA-256(envelope_bytes)
    let representation_id: [u8; 32] = Sha256::digest(envelope_bytes).into();
    let sbn = 0u8; // Single-block invariant

    // Split payload into K source symbols with zero padding for the final symbol
    let mut source_symbols: Vec<Vec<u8>> = Vec::with_capacity(k);
    for chunk in envelope_bytes.chunks(symbol_size) {
        let mut sym = vec![0u8; symbol_size];
        sym[..chunk.len()].copy_from_slice(chunk);
        source_symbols.push(sym);
    }

    // Derive deterministic seed and instantiate RaptorQ SystematicEncoder
    let seed = derive_raptorq_seed(&representation_id, sbn);
    let encoder = SystematicEncoder::new(&source_symbols, symbol_size, seed)
        .ok_or(RepairError::EncoderSingularMatrix)?;

    let mut output_symbols = Vec::with_capacity(total_symbols);

    // 1. Emit authenticated source symbols (ESI 0..K)
    for (esi, payload) in source_symbols.into_iter().enumerate() {
        if esi % 64 == 0 && cx.checkpoint().is_err() {
            return Err(RepairError::Cancelled);
        }
        let esi_u32 = esi as u32;
        let tag = compute_symbol_mac(
            auth_key,
            &representation_id,
            &object_id,
            generation,
            sbn,
            RepairSymbolKind::Source,
            esi_u32,
            &payload,
        )?;
        output_symbols.push(AuthenticatedRepairSymbol {
            kind: RepairSymbolKind::Source,
            sbn,
            esi: esi_u32,
            payload,
            tag,
        });
    }

    // 2. Generate and emit authenticated repair symbols (ESI K..(K + repair_count))
    for i in 0..repair_count {
        if i % 64 == 0 && cx.checkpoint().is_err() {
            return Err(RepairError::Cancelled);
        }
        let esi = (k + i) as u32;
        let payload = encoder.try_repair_symbol(esi).map_err(|e| {
            RepairError::DecoderError(format!("encoder repair symbol failed: {e:?}"))
        })?;

        let tag = compute_symbol_mac(
            auth_key,
            &representation_id,
            &object_id,
            generation,
            sbn,
            RepairSymbolKind::Repair,
            esi,
            &payload,
        )?;
        output_symbols.push(AuthenticatedRepairSymbol {
            kind: RepairSymbolKind::Repair,
            sbn,
            esi,
            payload,
            tag,
        });
    }

    // 3. Construct authenticated manifest
    let manifest_mac = compute_manifest_mac(
        auth_key,
        &representation_id,
        &object_id,
        generation,
        protection,
        sbn,
        k as u32,
        symbol_size as u32,
        payload_len as u64,
        total_symbols as u32,
    )?;

    let manifest = RepairManifest {
        representation_id,
        object_id,
        generation,
        sbn,
        k: k as u32,
        symbol_size: symbol_size as u32,
        payload_len: payload_len as u64,
        total_symbols_generated: total_symbols as u32,
        protection_class: protection,
        manifest_mac,
    };

    // Final cancellation checkpoint before returning encoded bundle
    if cx.checkpoint().is_err() {
        return Err(RepairError::Cancelled);
    }

    Ok(EncodedRepairBundle {
        manifest,
        symbols: output_symbols,
    })
}

/// Convenience encoder using default symbol size (1 KiB) and global shared admission controller.
pub fn encode_repair_envelope_default(
    cx: &Cx,
    envelope_bytes: &[u8],
    object_id: [u8; 32],
    generation: u64,
    auth_key: &[u8],
    protection: RepairProtectionClass,
) -> Result<EncodedRepairBundle, RepairError> {
    encode_repair_envelope(
        cx,
        envelope_bytes,
        object_id,
        generation,
        auth_key,
        DEFAULT_SYMBOL_SIZE,
        protection,
        shared_admission_controller(),
    )
}

// ---------------------------------------------------------------------------
// Decoding Pipeline
// ---------------------------------------------------------------------------

/// Decode and reconstruct the exact serialized encrypted envelope from received symbols,
/// requiring caller assertions on expected recovery identity.
///
/// # Invariants Enforced
/// 1. Manifest HMAC authentication must verify before any symbol inspection.
/// 2. Mandatory `expected` identity tuple (representation_id, object_id, generation) must match manifest,
///    preventing replay of stale generation or substitution of foreign authentic object.
/// 3. Pre-allocation check on `symbols.len() <= admission.max_symbols_buffered`.
/// 4. Comprehensive memory charging (symbol payloads, intermediate symbols, dense matrix, equations, and solver clones).
/// 5. Every accepted symbol must match the manifest's geometry and pass individual HMAC verification.
/// 6. Duplicate symbols with identical payloads are deduplicated without inflating rank.
/// 7. Conflicting duplicate symbols (same ESI, divergent payload) trigger immediate rejection.
/// 8. Equation rank is checked prior to solving; deficits return structured diagnostic error.
/// 9. Reconstructed bytes must match `manifest.representation_id` before returning.
pub fn decode_repair_symbols<'a>(
    cx: &Cx,
    manifest: &RepairManifest,
    symbols: &[AuthenticatedRepairSymbol],
    expected: &ExpectedRecoveryIdentity,
    auth_key: &[u8],
    admission: &'a RepairAdmissionController,
) -> Result<RepairResult<'a>, RepairError> {
    if cx.checkpoint().is_err() {
        return Err(RepairError::Cancelled);
    }

    if auth_key.is_empty() {
        return Err(RepairError::EmptyAuthenticationKey);
    }
    let auth_key_guard = Zeroizing::new(auth_key.to_vec());
    let auth_key = auth_key_guard.as_slice();

    // 1. Verify manifest authentication
    if !verify_manifest_mac(auth_key, manifest) {
        return Err(RepairError::ManifestAuthenticationFailed);
    }

    // 2. Mandatory validation of expected recovery identity:
    // Prevents replay of stale generation or substitution of foreign authentic object.
    if expected.representation_id != manifest.representation_id {
        return Err(RepairError::ForeignRepresentationId {
            expected: expected.representation_id,
            got: manifest.representation_id,
        });
    }

    if expected.object_id != manifest.object_id {
        return Err(RepairError::ForeignObjectId {
            expected: expected.object_id,
            got: manifest.object_id,
        });
    }

    if expected.generation != manifest.generation {
        return Err(RepairError::ForeignGeneration {
            expected: expected.generation,
            got: manifest.generation,
        });
    }

    // 3. Validate geometry bounds
    if manifest.sbn != 0 {
        return Err(RepairError::MultiBlockUnsupported { sbn: manifest.sbn });
    }

    let k = manifest.k as usize;
    if k == 0 || k > MAX_CHUNK_SOURCE_SYMBOLS {
        return Err(RepairError::ExceedsMaxSourceSymbols {
            k,
            limit: MAX_CHUNK_SOURCE_SYMBOLS,
        });
    }

    let symbol_size = manifest.symbol_size as usize;
    if !(MIN_SYMBOL_SIZE..=MAX_SYMBOL_SIZE).contains(&symbol_size) {
        return Err(RepairError::InvalidGeometry {
            reason: format!("symbol size {symbol_size} outside valid range"),
        });
    }

    let payload_len =
        usize::try_from(manifest.payload_len).map_err(|_| RepairError::InvalidGeometry {
            reason: "payload_len does not fit the host address space".to_owned(),
        })?;
    if payload_len == 0 || payload_len > MAX_CHUNK_ENVELOPE_BYTES {
        return Err(RepairError::InvalidGeometry {
            reason: format!("payload_len {payload_len} out of bounds"),
        });
    }

    // Framing geometry invariant: payload_len.div_ceil(symbol_size) must match manifest.k
    let expected_k = payload_len.div_ceil(symbol_size);
    if expected_k != k {
        return Err(RepairError::InvalidGeometry {
            reason: format!(
                "payload_len {payload_len} with symbol_size {symbol_size} requires K {expected_k}, but manifest specifies K {k}"
            ),
        });
    }

    // Manifest geometry invariant: total_symbols_generated must be >= K
    let total_symbols_manifest = manifest.total_symbols_generated as usize;
    if total_symbols_manifest < k {
        return Err(RepairError::InvalidGeometry {
            reason: format!("manifest total_symbols_generated {total_symbols_manifest} < K {k}"),
        });
    }

    // 4. Pre-allocation buffering guard: enforce max_symbols_buffered BEFORE vector allocation
    if symbols.len() > admission.max_symbols_buffered {
        return Err(RepairError::AdmissionExceeded(format!(
            "received symbol count {} exceeds max_symbols_buffered threshold {}",
            symbols.len(),
            admission.max_symbols_buffered
        )));
    }

    // 5. Compute systematic parameters and charge comprehensive checked memory budget
    let params = SystematicParams::try_for_source_block(k, symbol_size).map_err(|e| {
        RepairError::InvalidGeometry {
            reason: format!("systematic parameters calculation failed: {e:?}"),
        }
    })?;

    let estimated_memory = calculate_decoder_memory_budget(&params, symbols.len(), symbol_size)?;
    let mut permit = admission.acquire(estimated_memory)?;

    // 6. Initialize InactivationDecoder
    let seed = derive_raptorq_seed(&manifest.representation_id, manifest.sbn);
    let decoder = InactivationDecoder::try_new(k, symbol_size, seed)
        .map_err(|e| RepairError::DecoderError(format!("decoder initialization failed: {e:?}")))?;

    // 7. Authenticate, validate, and deduplicate received symbols
    let mut seen_esis: HashSet<u32> = HashSet::with_capacity(symbols.len());
    let mut validated_source: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut validated_repair: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut duplicates_discarded = 0usize;
    let mut corrupt_symbols_discarded = 0usize;

    for (idx, sym) in symbols.iter().enumerate() {
        if idx % 64 == 0 && cx.checkpoint().is_err() {
            return Err(RepairError::Cancelled);
        }

        // Corruption turns a symbol into an erasure. Never add unauthenticated
        // bytes to the solver, but allow surviving authenticated redundancy to
        // recover the envelope. Check size before hashing attacker-sized data.
        if sym.payload.len() != symbol_size || !verify_symbol_mac(auth_key, manifest, sym) {
            corrupt_symbols_discarded += 1;
            continue;
        }
        if sym.sbn != manifest.sbn {
            return Err(RepairError::MultiBlockUnsupported { sbn: sym.sbn });
        }

        // Validate even duplicates: authentication does not make a source ESI
        // valid as a repair ESI, or permit noncanonical source padding.
        let esi = sym.esi as usize;
        match sym.kind {
            RepairSymbolKind::Source if esi >= k => {
                return Err(RepairError::InvalidGeometry {
                    reason: format!("source symbol ESI {} >= K {}", sym.esi, k),
                });
            }
            RepairSymbolKind::Repair if esi < k => {
                return Err(RepairError::InvalidGeometry {
                    reason: format!("repair symbol ESI {} < K {}", sym.esi, k),
                });
            }
            RepairSymbolKind::Repair if esi >= total_symbols_manifest => {
                return Err(RepairError::InvalidGeometry {
                    reason: format!(
                        "repair symbol ESI {} >= total_symbols_generated {}",
                        sym.esi, total_symbols_manifest
                    ),
                });
            }
            _ => {}
        }
        if sym.kind == RepairSymbolKind::Source && esi == k - 1 {
            let final_bytes = payload_len - (k - 1) * symbol_size;
            if sym.payload[final_bytes..].iter().any(|&byte| byte != 0) {
                return Err(RepairError::InvalidGeometry {
                    reason: "nonzero source padding".to_owned(),
                });
            }
        }

        // Handle duplicates
        if seen_esis.contains(&sym.esi) {
            // Check for conflicting duplicate payload
            let existing_payload = validated_source
                .iter()
                .chain(validated_repair.iter())
                .find(|(esi, _)| *esi == sym.esi)
                .map(|(_, p)| p);

            if let Some(existing) = existing_payload {
                if existing != &sym.payload {
                    return Err(RepairError::ConflictingDuplicateSymbol { esi: sym.esi });
                }
            }
            duplicates_discarded += 1;
            continue;
        }

        seen_esis.insert(sym.esi);
        match sym.kind {
            RepairSymbolKind::Source => {
                validated_source.push((sym.esi, sym.payload.clone()));
            }
            RepairSymbolKind::Repair => {
                validated_repair.push((sym.esi, sym.payload.clone()));
            }
        }
    }

    // Fast path: all K source symbols present
    if validated_source.len() == k {
        // Sort by ESI to ensure canonical byte order
        validated_source.sort_by_key(|(esi, _)| *esi);
        let mut reconstructed = Vec::with_capacity(payload_len);
        for (_, sym_data) in &validated_source {
            let take = sym_data.len().min(payload_len - reconstructed.len());
            reconstructed.extend_from_slice(&sym_data[..take]);
        }
        reconstructed.truncate(payload_len);

        // Verification gate
        let actual_digest: [u8; 32] = Sha256::digest(&reconstructed).into();
        if actual_digest != manifest.representation_id {
            return Err(RepairError::RepresentationDigestMismatch {
                expected: manifest.representation_id,
                actual: actual_digest,
            });
        }

        // Final cancellation checkpoint before returning fast-path result
        if cx.checkpoint().is_err() {
            return Err(RepairError::Cancelled);
        }

        let l = decoder.params().l;
        // Free temporary allocations before releasing their reservation.
        drop(validated_source);
        drop(validated_repair);
        drop(seen_esis);
        drop(decoder);
        permit.shrink_to(reconstructed.capacity());
        return Ok(RepairResult {
            reconstructed_envelope: reconstructed,
            representation_id: manifest.representation_id,
            stats: RepairStats {
                source_symbols_used: k,
                repair_symbols_used: 0,
                duplicates_discarded,
                corrupt_symbols_discarded,
                rank_status: RankStatusDiagnostic {
                    rank: l,
                    columns: l,
                    deficit: 0,
                },
            },
            permit,
        });
    }

    // General decoding path using RaptorQ InactivationDecoder
    // Populate with LDPC/HDPC constraint symbols
    let mut received_equations = decoder.constraint_symbols();
    if cx.checkpoint().is_err() {
        return Err(RepairError::Cancelled);
    }

    // Add received source symbols
    for (index, (esi, data)) in validated_source.iter().enumerate() {
        if index % 64 == 0 && cx.checkpoint().is_err() {
            return Err(RepairError::Cancelled);
        }
        received_equations.push(ReceivedSymbol::source(*esi, data.clone()));
    }

    // Add received repair symbols with derived repair equations
    for (index, (esi, data)) in validated_repair.iter().enumerate() {
        if index % 64 == 0 && cx.checkpoint().is_err() {
            return Err(RepairError::Cancelled);
        }
        let (columns, coefficients) = decoder.repair_equation(*esi).map_err(|e| {
            RepairError::DecoderError(format!("repair equation failed for ESI {esi}: {e:?}"))
        })?;
        received_equations.push(ReceivedSymbol::repair(
            *esi,
            columns,
            coefficients,
            data.clone(),
        ));
    }

    // Check rank status
    if cx.checkpoint().is_err() {
        return Err(RepairError::Cancelled);
    }
    let rank_profile = decoder
        .rank_status(&received_equations)
        .map_err(|e| RepairError::DecoderError(format!("rank evaluation failed: {e:?}")))?;
    if cx.checkpoint().is_err() {
        return Err(RepairError::Cancelled);
    }

    if rank_profile.deficit > 0 {
        return Err(RepairError::InsufficientRank {
            rank: rank_profile.rank,
            columns: rank_profile.columns,
            deficit: rank_profile.deficit,
        });
    }

    if cx.checkpoint().is_err() {
        return Err(RepairError::Cancelled);
    }

    // Execute full inactivation decode
    let decode_result = decoder
        .decode(&received_equations)
        .map_err(|e| RepairError::DecoderError(format!("RaptorQ solve failed: {e:?}")))?;
    if cx.checkpoint().is_err() {
        return Err(RepairError::Cancelled);
    }

    if decode_result.source.len() < k {
        return Err(RepairError::DecoderError(format!(
            "decoded source count {} < expected {}",
            decode_result.source.len(),
            k
        )));
    }
    let final_bytes = payload_len - (k - 1) * symbol_size;
    if decode_result.source[k - 1].len() != symbol_size
        || decode_result.source[k - 1][final_bytes..]
            .iter()
            .any(|&byte| byte != 0)
    {
        return Err(RepairError::InvalidGeometry {
            reason: "invalid decoded source padding".to_owned(),
        });
    }

    // Assemble reconstructed envelope
    let mut reconstructed = Vec::with_capacity(payload_len);
    for sym in decode_result.source.iter().take(k) {
        let take = sym.len().min(payload_len - reconstructed.len());
        reconstructed.extend_from_slice(&sym[..take]);
    }
    reconstructed.truncate(payload_len);

    // 8. Final digest verification gate
    let actual_digest: [u8; 32] = Sha256::digest(&reconstructed).into();
    if actual_digest != manifest.representation_id {
        return Err(RepairError::RepresentationDigestMismatch {
            expected: manifest.representation_id,
            actual: actual_digest,
        });
    }

    // Cancellation checkpoint immediately AFTER solve and digest verification, before successful return:
    // Ensures that cancellation occurring during the uninterruptible synchronous solve cannot report success.
    if cx.checkpoint().is_err() {
        return Err(RepairError::Cancelled);
    }

    let source_symbols_used = validated_source.len();
    let repair_symbols_used = validated_repair.len();
    drop(decode_result);
    drop(received_equations);
    drop(validated_source);
    drop(validated_repair);
    drop(seen_esis);
    drop(decoder);
    // Release scratch reservations only after their buffers have been freed.
    permit.shrink_to(reconstructed.capacity());

    Ok(RepairResult {
        reconstructed_envelope: reconstructed,
        representation_id: manifest.representation_id,
        stats: RepairStats {
            source_symbols_used,
            repair_symbols_used,
            duplicates_discarded,
            corrupt_symbols_discarded,
            rank_status: RankStatusDiagnostic {
                rank: rank_profile.rank,
                columns: rank_profile.columns,
                deficit: rank_profile.deficit,
            },
        },
        permit,
    })
}

/// Convenience decoder using global shared admission controller and mandatory expected identity.
pub fn decode_repair_symbols_default(
    cx: &Cx,
    manifest: &RepairManifest,
    symbols: &[AuthenticatedRepairSymbol],
    expected: &ExpectedRecoveryIdentity,
    auth_key: &[u8],
) -> Result<RepairResult<'static>, RepairError> {
    decode_repair_symbols(
        cx,
        manifest,
        symbols,
        expected,
        auth_key,
        shared_admission_controller(),
    )
}

/// Backward-compatible decode helper with expected identity tuple.
#[inline]
pub fn decode_repair_symbols_with_expected<'a>(
    cx: &Cx,
    manifest: &RepairManifest,
    symbols: &[AuthenticatedRepairSymbol],
    expected: &ExpectedRecoveryIdentity,
    auth_key: &[u8],
    admission: &'a RepairAdmissionController,
) -> Result<RepairResult<'a>, RepairError> {
    decode_repair_symbols(cx, manifest, symbols, expected, auth_key, admission)
}

/// Test-only convenience decoder asserting identity derived directly from manifest for callers
/// that do not supply an independent expected identity assertion.
/// Strictly gated to test compilation to prevent production bypass of expected identity verification.
#[cfg(test)]
#[must_use = "decoding produces reconstructed envelope bytes"]
pub fn decode_repair_symbols_unasserted<'a>(
    cx: &Cx,
    manifest: &RepairManifest,
    symbols: &[AuthenticatedRepairSymbol],
    auth_key: &[u8],
    admission: &'a RepairAdmissionController,
) -> Result<RepairResult<'a>, RepairError> {
    let expected = ExpectedRecoveryIdentity::from_manifest(manifest);
    decode_repair_symbols(cx, manifest, symbols, &expected, auth_key, admission)
}

/// Test-only convenience decoder using global shared admission controller and identity derived from manifest.
/// Strictly gated to test compilation to prevent production bypass of expected identity verification.
#[cfg(test)]
#[must_use = "decoding produces reconstructed envelope bytes"]
pub fn decode_repair_symbols_unasserted_default(
    cx: &Cx,
    manifest: &RepairManifest,
    symbols: &[AuthenticatedRepairSymbol],
    auth_key: &[u8],
) -> Result<RepairResult<'static>, RepairError> {
    let expected = ExpectedRecoveryIdentity::from_manifest(manifest);
    decode_repair_symbols(
        cx,
        manifest,
        symbols,
        &expected,
        auth_key,
        shared_admission_controller(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_maximum_protection_fits_default_admission_with_maximum_output() {
        let admission = RepairAdmissionController::default_production();
        let output = admission
            .acquire(RepairObjectLimits::default().max_envelope_bytes)
            .unwrap();
        let k = MAX_PERSISTED_SOURCE_SYMBOLS;
        let size = DEFAULT_SYMBOL_SIZE;
        let total = k + RepairProtectionClass::Maximum.repair_symbol_count(k);
        let records_bytes = total * (size + 38);
        let records = admission
            .acquire(2 * records_bytes + total * std::mem::size_of::<AuthenticatedRepairSymbol>())
            .unwrap();
        let params = SystematicParams::try_for_source_block(k, size).unwrap();
        let scratch = admission
            .acquire(calculate_decoder_memory_budget(&params, total, size).unwrap())
            .expect("maximum output, records, and decoder scratch fit simultaneously");
        drop(scratch);
        drop(records);
        drop(output);

        let old_k = MAX_CHUNK_SOURCE_SYMBOLS;
        let old_params = SystematicParams::try_for_source_block(old_k, size).unwrap();
        let old_scratch = calculate_decoder_memory_budget(&old_params, old_k * 2, size).unwrap();
        assert!(admission.acquire(old_scratch).is_err());
    }

    #[test]
    fn persisted_repair_object_chunks_authenticate_and_recover_independent_records() {
        let cx = test_cx();
        let key = [0x61; 32];
        let limits = RepairObjectLimits::default();
        let admission = RepairAdmissionController::default_production();
        let envelope: Vec<u8> = (0..MAX_PERSISTED_CHUNK_BYTES + 17)
            .map(|index| (index % 251) as u8)
            .collect();
        let expected =
            ExpectedRecoveryIdentity::new(Sha256::digest(&envelope).into(), [0x35; 32], 9);
        let mut files = std::collections::HashMap::new();
        let descriptor = encode_repair_object(
            &cx,
            &envelope,
            expected.object_id,
            expected.generation,
            &key,
            MAX_SYMBOL_SIZE,
            RepairProtectionClass::Maximum,
            &admission,
            limits,
            |index, _, records| {
                let name = format!("chunk-{index}");
                files.insert(name.clone(), records.to_vec());
                Ok(name)
            },
        )
        .expect("encode complete object in bounded chunks");
        assert_eq!(descriptor.chunks.len(), 2);
        let wire = descriptor.to_authenticated_bytes(&key, limits).unwrap();
        let decoded =
            RepairObjectDescriptor::from_authenticated_bytes(&wire, &expected, &key, limits)
                .unwrap();
        let repaired = decode_repair_object(
            &cx,
            &decoded,
            &expected,
            &key,
            &admission,
            limits,
            |chunk| {
                let mut records = files[&chunk.records_object_id].clone();
                records[0] ^= 1;
                Ok(records)
            },
        )
        .expect("one damaged fixed record per chunk is an erasure");
        assert_eq!(repaired.reconstructed_envelope, envelope);
        assert_eq!(repaired.stats.corrupt_symbols_discarded, 2);
        drop(repaired);

        let truncated = decode_repair_object(
            &cx,
            &decoded,
            &expected,
            &key,
            &admission,
            limits,
            |chunk| {
                let mut records = files[&chunk.records_object_id].clone();
                records.truncate(records.len() - 1);
                Ok(records)
            },
        )
        .expect("partial final repair record must not discard intact source records");
        assert_eq!(truncated.reconstructed_envelope, envelope);
        assert_eq!(truncated.stats.corrupt_symbols_discarded, 2);
        drop(truncated);

        let mut wrong = expected;
        wrong.object_id[0] ^= 1;
        assert!(matches!(
            RepairObjectDescriptor::from_authenticated_bytes(&wire, &wrong, &key, limits),
            Err(RepairError::ForeignObjectId { .. })
        ));
        wrong = expected;
        wrong.representation_id[0] ^= 1;
        assert!(matches!(
            RepairObjectDescriptor::from_authenticated_bytes(&wire, &wrong, &key, limits),
            Err(RepairError::ForeignRepresentationId { .. })
        ));
        wrong = expected;
        wrong.generation += 1;
        assert!(matches!(
            RepairObjectDescriptor::from_authenticated_bytes(&wire, &wrong, &key, limits),
            Err(RepairError::ForeignGeneration { .. })
        ));
        let mut reordered = decoded.clone();
        reordered.chunks.swap(0, 1);
        assert!(
            decode_repair_object(
                &cx,
                &reordered,
                &expected,
                &key,
                &admission,
                limits,
                |_| panic!("invalid descriptor must fail before reading records"),
            )
            .is_err()
        );
        let mut retargeted = decoded.clone();
        retargeted.chunks[0].records_object_id = "foreign-records".into();
        assert!(matches!(
            decode_repair_object(
                &cx,
                &retargeted,
                &expected,
                &key,
                &admission,
                limits,
                |_| panic!("modified locator must fail before reading records"),
            ),
            Err(RepairError::ManifestAuthenticationFailed)
        ));
        let missing =
            decode_repair_object(&cx, &decoded, &expected, &key, &admission, limits, |_| {
                Ok(Vec::new())
            });
        assert!(matches!(missing, Err(RepairError::InsufficientRank { .. })));
    }

    fn test_cx() -> Cx {
        Cx::for_testing()
    }

    fn sample_object_id(seed: u8) -> [u8; 32] {
        let mut id = [0u8; 32];
        id[0] = seed;
        id[31] = seed.wrapping_mul(7);
        id
    }

    fn sample_envelope(size: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(size);
        for i in 0..size {
            data.push(((i * 37 + 19) % 251) as u8);
        }
        data
    }

    #[test]
    fn test_roundtrip_lossless() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(10_000);
        let obj_id = sample_object_id(1);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let result = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            &expected,
            key,
            &admission,
        )
        .expect("decode failed");

        assert_eq!(result.reconstructed_envelope, payload);
        assert_eq!(result.representation_id, bundle.manifest.representation_id);
    }

    #[test]
    fn test_repair_from_loss_10_percent() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(20_000); // ~20 source symbols
        let obj_id = sample_object_id(2);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::High, // +50% repair symbols
            &admission,
        )
        .expect("encode failed");

        let mut received = Vec::new();

        // Drop 2 source symbols (10% of 20)
        for sym in &bundle.symbols {
            if sym.kind == RepairSymbolKind::Source && (sym.esi == 3 || sym.esi == 7) {
                continue; // drop
            }
            received.push(sym.clone());
        }

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let result =
            decode_repair_symbols(&cx, &bundle.manifest, &received, &expected, key, &admission)
                .expect("decode repair failed");

        assert_eq!(result.reconstructed_envelope, payload);
        assert!(result.stats.repair_symbols_used > 0);
    }

    #[test]
    fn test_repair_from_loss_50_percent() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(15_000); // ~15 source symbols
        let obj_id = sample_object_id(3);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Maximum, // +100% repair symbols
            &admission,
        )
        .expect("encode failed");

        let k = bundle.manifest.k as usize;
        let mut received = Vec::new();

        // Drop half of the source symbols (even ESIs)
        for sym in &bundle.symbols {
            if sym.kind == RepairSymbolKind::Source && sym.esi % 2 == 0 {
                continue; // drop
            }
            received.push(sym.clone());
        }

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let result =
            decode_repair_symbols(&cx, &bundle.manifest, &received, &expected, key, &admission)
                .expect("decode repair 50% failed");

        assert_eq!(result.reconstructed_envelope, payload);
        assert_eq!(result.stats.source_symbols_used, k - (k + 1) / 2);
    }

    #[test]
    fn test_burst_loss_repair() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(25_000); // 25 source symbols
        let obj_id = sample_object_id(4);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::High,
            &admission,
        )
        .expect("encode failed");

        // Drop a burst of 4 contiguous source symbols (ESI 5, 6, 7, 8)
        let received: Vec<_> = bundle
            .symbols
            .iter()
            .filter(|s| !(s.kind == RepairSymbolKind::Source && (5..=8).contains(&s.esi)))
            .cloned()
            .collect();

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let result =
            decode_repair_symbols(&cx, &bundle.manifest, &received, &expected, key, &admission)
                .expect("burst loss repair failed");

        assert_eq!(result.reconstructed_envelope, payload);
    }

    #[test]
    fn test_odd_sized_payload_exact_reconstruction() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        // 1337 bytes: not a multiple of 1024 (1024 + 313 bytes)
        let payload = sample_envelope(1337);
        let obj_id = sample_object_id(5);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        // Drop source symbol 0, use repair symbols
        let received: Vec<_> = bundle
            .symbols
            .iter()
            .filter(|s| !(s.kind == RepairSymbolKind::Source && s.esi == 0))
            .cloned()
            .collect();

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let result =
            decode_repair_symbols(&cx, &bundle.manifest, &received, &expected, key, &admission)
                .expect("odd-sized decode failed");

        assert_eq!(result.reconstructed_envelope.len(), 1337);
        assert_eq!(result.reconstructed_envelope, payload);
    }

    #[test]
    fn test_insufficient_rank_rejection() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(10_000); // 10 source symbols
        let obj_id = sample_object_id(6);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        // Provide only 3 symbols total (out of 10 needed)
        let received: Vec<_> = bundle.symbols.iter().take(3).cloned().collect();

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let err =
            decode_repair_symbols(&cx, &bundle.manifest, &received, &expected, key, &admission)
                .expect_err("insufficient rank must fail");

        match err {
            RepairError::InsufficientRank {
                rank,
                columns,
                deficit,
            } => {
                assert!(deficit > 0);
                assert!(rank < columns);
            }
            other => panic!("expected InsufficientRank, got {other:?}"),
        }
    }

    #[test]
    fn test_corrupt_symbol_is_erased_and_redundancy_recovers_exact_bytes() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(7);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        let mut tampered_symbols = bundle.symbols.clone();
        // Flip one byte in symbol 0 payload
        tampered_symbols[0].payload[10] ^= 0xFF;

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let recovered = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &tampered_symbols,
            &expected,
            key,
            &admission,
        )
        .expect("authenticated redundancy repairs one corrupt source symbol");
        assert_eq!(recovered.reconstructed_envelope, payload);
        assert_eq!(recovered.stats.corrupt_symbols_discarded, 1);
        drop(recovered);

        // Without sufficient authenticated redundancy, fail closed.
        for symbol in &mut tampered_symbols {
            symbol.tag[0] ^= 1;
        }
        assert!(matches!(
            decode_repair_symbols(
                &cx,
                &bundle.manifest,
                &tampered_symbols,
                &expected,
                key,
                &admission
            ),
            Err(RepairError::InsufficientRank { .. })
        ));
    }

    #[test]
    fn test_tampered_manifest_mac_rejected() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(8);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        let mut tampered_manifest = bundle.manifest.clone();
        tampered_manifest.manifest_mac[5] ^= 0xAA; // Corrupt MAC

        let expected = ExpectedRecoveryIdentity::from_manifest(&tampered_manifest);
        let err = decode_repair_symbols(
            &cx,
            &tampered_manifest,
            &bundle.symbols,
            &expected,
            key,
            &admission,
        )
        .expect_err("tampered manifest must fail");

        assert_eq!(err, RepairError::ManifestAuthenticationFailed);
    }

    #[test]
    fn test_wrong_key_rejected() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(9);
        let key = b"test-secret-key-32-bytes-long!!";
        let wrong_key = b"wrong-secret-key-32-bytes-long!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let err = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            &expected,
            wrong_key,
            &admission,
        )
        .expect_err("wrong key must fail manifest MAC check");

        assert_eq!(err, RepairError::ManifestAuthenticationFailed);
    }

    #[test]
    fn test_empty_key_rejected() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(99);

        let err = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            b"",
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect_err("empty key must fail");

        assert_eq!(err, RepairError::EmptyAuthenticationKey);
    }

    #[test]
    fn test_duplicate_symbol_handling() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(10);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        // Duplicate symbol 0 three times
        let mut symbols_with_dupes = bundle.symbols.clone();
        symbols_with_dupes.push(bundle.symbols[0].clone());
        symbols_with_dupes.push(bundle.symbols[0].clone());

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let result = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &symbols_with_dupes,
            &expected,
            key,
            &admission,
        )
        .expect("benign duplicates should be discarded gracefully");

        assert_eq!(result.reconstructed_envelope, payload);
        assert_eq!(result.stats.duplicates_discarded, 2);
    }

    #[test]
    fn test_conflicting_duplicate_rejection() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(11);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        // Create conflicting duplicate symbol (same ESI, altered payload, but re-MAC'd)
        let mut conflicting_sym = bundle.symbols[0].clone();
        conflicting_sym.payload[0] ^= 0x55;
        conflicting_sym.tag = compute_symbol_mac(
            key,
            &bundle.manifest.representation_id,
            &bundle.manifest.object_id,
            bundle.manifest.generation,
            bundle.manifest.sbn,
            conflicting_sym.kind,
            conflicting_sym.esi,
            &conflicting_sym.payload,
        )
        .expect("re-mac");

        let mut symbols_with_conflict = bundle.symbols.clone();
        symbols_with_conflict.push(conflicting_sym);

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let err = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &symbols_with_conflict,
            &expected,
            key,
            &admission,
        )
        .expect_err("conflicting duplicate must fail");

        match err {
            RepairError::ConflictingDuplicateSymbol { esi } => {
                assert_eq!(esi, bundle.symbols[0].esi);
            }
            other => panic!("expected ConflictingDuplicateSymbol, got {other:?}"),
        }
    }

    #[test]
    fn test_authenticated_invalid_duplicate_kind_is_not_discarded() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let key = b"test-secret-key-32-bytes-long!!";
        let bundle = encode_repair_envelope(
            &cx,
            &sample_envelope(1337),
            sample_object_id(77),
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .unwrap();
        let mut symbols = bundle.symbols.clone();
        let mut invalid = symbols[0].clone();
        invalid.kind = RepairSymbolKind::Repair;
        invalid.tag = compute_symbol_mac(
            key,
            &bundle.manifest.representation_id,
            &bundle.manifest.object_id,
            bundle.manifest.generation,
            bundle.manifest.sbn,
            invalid.kind,
            invalid.esi,
            &invalid.payload,
        )
        .unwrap();
        symbols.push(invalid);
        let error = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &symbols,
            &ExpectedRecoveryIdentity::from_manifest(&bundle.manifest),
            key,
            &admission,
        )
        .unwrap_err();
        assert!(matches!(error, RepairError::InvalidGeometry { reason } if reason.contains("< K")));
        assert_eq!(admission.allocated_memory_bytes(), 0);
    }

    #[test]
    fn test_authenticated_nonzero_source_padding_is_rejected() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let key = b"test-secret-key-32-bytes-long!!";
        let bundle = encode_repair_envelope(
            &cx,
            &sample_envelope(1337),
            sample_object_id(78),
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .unwrap();
        let mut symbols = bundle.symbols.clone();
        let last = symbols
            .iter_mut()
            .find(|symbol| {
                symbol.kind == RepairSymbolKind::Source && symbol.esi == bundle.manifest.k - 1
            })
            .unwrap();
        *last.payload.last_mut().unwrap() = 1;
        last.tag = compute_symbol_mac(
            key,
            &bundle.manifest.representation_id,
            &bundle.manifest.object_id,
            bundle.manifest.generation,
            bundle.manifest.sbn,
            last.kind,
            last.esi,
            &last.payload,
        )
        .unwrap();
        let error = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &symbols,
            &ExpectedRecoveryIdentity::from_manifest(&bundle.manifest),
            key,
            &admission,
        )
        .unwrap_err();
        assert!(
            matches!(error, RepairError::InvalidGeometry { reason } if reason == "nonzero source padding")
        );
        assert_eq!(admission.allocated_memory_bytes(), 0);
    }

    #[test]
    fn test_foreign_identity_rejection() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(12);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        // Foreign expected representation ID
        let foreign_rep = [0xEEu8; 32];
        let expected_wrong_rep = ExpectedRecoveryIdentity::new(
            foreign_rep,
            bundle.manifest.object_id,
            bundle.manifest.generation,
        );
        let err = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            &expected_wrong_rep,
            key,
            &admission,
        )
        .expect_err("foreign representation ID must fail");

        assert_eq!(
            err,
            RepairError::ForeignRepresentationId {
                expected: foreign_rep,
                got: bundle.manifest.representation_id
            }
        );

        // Foreign expected object ID
        let foreign_obj = sample_object_id(99);
        let expected_wrong_obj = ExpectedRecoveryIdentity::new(
            bundle.manifest.representation_id,
            foreign_obj,
            bundle.manifest.generation,
        );
        let err2 = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            &expected_wrong_obj,
            key,
            &admission,
        )
        .expect_err("foreign object ID must fail");

        assert_eq!(
            err2,
            RepairError::ForeignObjectId {
                expected: foreign_obj,
                got: bundle.manifest.object_id
            }
        );

        // Foreign expected generation
        let expected_wrong_gen = ExpectedRecoveryIdentity::new(
            bundle.manifest.representation_id,
            bundle.manifest.object_id,
            999,
        );
        let err3 = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            &expected_wrong_gen,
            key,
            &admission,
        )
        .expect_err("foreign generation must fail");

        assert_eq!(
            err3,
            RepairError::ForeignGeneration {
                expected: 999,
                got: 1
            }
        );
    }

    #[test]
    fn test_multiblock_sbn_rejection() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(13);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        let mut multiblock_manifest = bundle.manifest.clone();
        multiblock_manifest.sbn = 1; // SBN > 0
        multiblock_manifest.manifest_mac = compute_manifest_mac(
            key,
            &multiblock_manifest.representation_id,
            &multiblock_manifest.object_id,
            multiblock_manifest.generation,
            multiblock_manifest.protection_class,
            multiblock_manifest.sbn,
            multiblock_manifest.k,
            multiblock_manifest.symbol_size,
            multiblock_manifest.payload_len,
            multiblock_manifest.total_symbols_generated,
        )
        .expect("compute mac");

        let expected = ExpectedRecoveryIdentity::from_manifest(&multiblock_manifest);
        let err = decode_repair_symbols(
            &cx,
            &multiblock_manifest,
            &bundle.symbols,
            &expected,
            key,
            &admission,
        )
        .expect_err("SBN > 0 must fail");

        assert_eq!(err, RepairError::MultiBlockUnsupported { sbn: 1 });
    }

    #[test]
    fn test_exceeds_max_chunk_symbols_rejection() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let obj_id = sample_object_id(15);
        let key = b"test-secret-key-32-bytes-long!!";

        // Create an envelope requiring > MAX_CHUNK_SOURCE_SYMBOLS (e.g. 2049 symbols with size 1024 = 2049 * 1024 bytes)
        let large_payload = sample_envelope((MAX_CHUNK_SOURCE_SYMBOLS + 1) * DEFAULT_SYMBOL_SIZE);

        let err = encode_repair_envelope(
            &cx,
            &large_payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect_err("oversized chunk must be rejected");

        match err {
            RepairError::PayloadTooLarge { .. } | RepairError::ExceedsMaxSourceSymbols { .. } => {}
            other => panic!("expected PayloadTooLarge or ExceedsMaxSourceSymbols, got {other:?}"),
        }
    }

    #[test]
    fn test_max_symbols_buffered_pre_allocation_guard() {
        let cx = test_cx();
        // Admission controller configured with tight max_symbols_buffered = 10
        let admission = RepairAdmissionController::new(8, 256 * 1024 * 1024, 10);
        let payload = sample_envelope(20_000); // requires ~20 source symbols + repair symbols > 10
        let obj_id = sample_object_id(16);
        let key = b"test-secret-key-32-bytes-long!!";

        let err = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect_err("exceeding max_symbols_buffered must fail before allocation");

        match err {
            RepairError::AdmissionExceeded(msg) => {
                assert!(msg.contains("max_symbols_buffered"));
            }
            other => panic!("expected AdmissionExceeded, got {other:?}"),
        }
    }

    #[test]
    fn test_caller_cancellation() {
        let cx = test_cx();
        cx.cancel_fast(crate::outcome::CancelKind::User);
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(14);
        let key = b"test-secret-key-32-bytes-long!!";

        let err = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect_err("pre-cancelled context must fail");

        assert_eq!(err, RepairError::Cancelled);
    }

    #[test]
    fn test_admission_controller_concurrency_limit() {
        let admission = RepairAdmissionController::new(1, 1024 * 1024, 1000);
        let permit1 = admission.acquire(1024).expect("first permit succeeds");
        let permit2_err = admission
            .acquire(1024)
            .expect_err("second permit exceeds concurrency");
        match permit2_err {
            RepairError::AdmissionExceeded(msg) => assert!(msg.contains("concurrency limit")),
            other => panic!("expected AdmissionExceeded, got {other:?}"),
        }
        drop(permit1);
        let _permit3 = admission.acquire(1024).expect("succeeds after drop");
    }

    #[test]
    fn test_admission_controller_memory_limit() {
        let admission = RepairAdmissionController::new(10, 2048, 1000);
        let permit1 = admission.acquire(1500).expect("first permit succeeds");
        let permit2_err = admission.acquire(1000).expect_err("exceeds memory limit");
        match permit2_err {
            RepairError::AdmissionExceeded(msg) => assert!(msg.contains("memory limit")),
            other => panic!("expected AdmissionExceeded, got {other:?}"),
        }
        drop(permit1);
        let _permit3 = admission.acquire(2000).expect("succeeds after drop");
    }

    #[test]
    fn test_tampered_protection_class_rejected() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(5_000);
        let obj_id = sample_object_id(17);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode failed");

        // Tamper with protection_class in manifest without updating manifest_mac
        let mut tampered_manifest = bundle.manifest.clone();
        tampered_manifest.protection_class = RepairProtectionClass::Maximum;

        assert!(!verify_manifest_mac(key, &tampered_manifest));

        let expected = ExpectedRecoveryIdentity::from_manifest(&tampered_manifest);
        let err = decode_repair_symbols(
            &cx,
            &tampered_manifest,
            &bundle.symbols,
            &expected,
            key,
            &admission,
        )
        .expect_err("tampered protection_class must fail manifest authentication");

        assert_eq!(err, RepairError::ManifestAuthenticationFailed);
    }

    #[test]
    fn test_shared_admission_controller_aggregate_limit() {
        let shared = shared_admission_controller();
        // Record starting active operations
        let initial_ops = shared.active_operations();

        // Acquire a permit from shared controller
        let permit = shared
            .acquire(1024)
            .expect("permit from shared controller succeeds");
        assert_eq!(shared.active_operations(), initial_ops + 1);

        drop(permit);
        assert_eq!(shared.active_operations(), initial_ops);
    }

    #[test]
    fn test_checked_cas_concurrency_and_memory_overflow() {
        let admission = RepairAdmissionController::new(2, 5_000, 100);

        // 1. Acquire up to max concurrency
        let p1 = admission.acquire(1_000).expect("permit 1 succeeds");
        let p2 = admission.acquire(1_000).expect("permit 2 succeeds");

        // Concurrency limit exceeded
        let err_conc = admission
            .acquire(1_000)
            .expect_err("permit 3 must exceed concurrency");
        match err_conc {
            RepairError::AdmissionExceeded(msg) => assert!(msg.contains("concurrency limit")),
            other => panic!("expected AdmissionExceeded, got {other:?}"),
        }

        drop(p2);

        // 2. Memory limit exceeded (4001 + 1000 = 5001 > 5000)
        let err_mem = admission.acquire(4001).expect_err("exceeds memory limit");
        match err_mem {
            RepairError::AdmissionExceeded(msg) => assert!(msg.contains("memory limit")),
            other => panic!("expected AdmissionExceeded, got {other:?}"),
        }

        // 3. Counter overflow protection with usize::MAX
        let err_overflow = admission
            .acquire(usize::MAX)
            .expect_err("overflow must be rejected");
        match err_overflow {
            RepairError::AdmissionExceeded(msg) => {
                assert!(msg.contains("limit") || msg.contains("overflow"))
            }
            other => panic!("expected AdmissionExceeded, got {other:?}"),
        }

        drop(p1);
        assert_eq!(admission.active_operations(), 0);
        assert_eq!(admission.allocated_memory_bytes(), 0);
    }

    #[test]
    fn test_invalid_payload_len_k_mismatch_rejected() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let key = b"test-secret-key-32-bytes-long!!";
        let obj_id = sample_object_id(18);
        let rep_id = [42u8; 32];
        let symbol_size = 1024u32;
        let payload_len = 5000u64; // div_ceil(1024) is 5
        let invalid_k = 10u32; // mismatch! 10 != 5
        let total_symbols = 15u32;

        let mac = compute_manifest_mac(
            key,
            &rep_id,
            &obj_id,
            1,
            RepairProtectionClass::Standard,
            0,
            invalid_k,
            symbol_size,
            payload_len,
            total_symbols,
        )
        .expect("compute mac");

        let manifest = RepairManifest {
            representation_id: rep_id,
            object_id: obj_id,
            generation: 1,
            sbn: 0,
            k: invalid_k,
            symbol_size,
            payload_len,
            total_symbols_generated: total_symbols,
            protection_class: RepairProtectionClass::Standard,
            manifest_mac: mac,
        };

        let expected = ExpectedRecoveryIdentity::from_manifest(&manifest);
        let err = decode_repair_symbols(&cx, &manifest, &[], &expected, key, &admission)
            .expect_err("k mismatch must fail");

        match err {
            RepairError::InvalidGeometry { reason } => {
                assert!(reason.contains("requires K 5, but manifest specifies K 10"));
            }
            other => panic!("expected InvalidGeometry, got {other:?}"),
        }
    }

    #[test]
    fn test_manifest_total_symbols_less_than_k_rejected() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let key = b"test-secret-key-32-bytes-long!!";
        let obj_id = sample_object_id(19);
        let rep_id = [43u8; 32];
        let symbol_size = 1024u32;
        let payload_len = 5120u64; // div_ceil(1024) is 5
        let k = 5u32;
        let invalid_total_symbols = 4u32; // < 5

        let mac = compute_manifest_mac(
            key,
            &rep_id,
            &obj_id,
            1,
            RepairProtectionClass::Standard,
            0,
            k,
            symbol_size,
            payload_len,
            invalid_total_symbols,
        )
        .expect("compute mac");

        let manifest = RepairManifest {
            representation_id: rep_id,
            object_id: obj_id,
            generation: 1,
            sbn: 0,
            k,
            symbol_size,
            payload_len,
            total_symbols_generated: invalid_total_symbols,
            protection_class: RepairProtectionClass::Standard,
            manifest_mac: mac,
        };

        let expected = ExpectedRecoveryIdentity::from_manifest(&manifest);
        let err = decode_repair_symbols(&cx, &manifest, &[], &expected, key, &admission)
            .expect_err("total_symbols < k must fail");

        match err {
            RepairError::InvalidGeometry { reason } => {
                assert!(reason.contains("total_symbols_generated 4 < K 5"));
            }
            other => panic!("expected InvalidGeometry, got {other:?}"),
        }
    }

    #[test]
    fn test_repair_symbol_esi_out_of_range_rejected() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(3000);
        let obj_id = sample_object_id(20);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode");

        let total_gen = bundle.manifest.total_symbols_generated;
        let out_of_bounds_esi = total_gen + 10;
        let dummy_payload = vec![0x55u8; DEFAULT_SYMBOL_SIZE];
        let tag = compute_symbol_mac(
            key,
            &bundle.manifest.representation_id,
            &bundle.manifest.object_id,
            bundle.manifest.generation,
            bundle.manifest.sbn,
            RepairSymbolKind::Repair,
            out_of_bounds_esi,
            &dummy_payload,
        )
        .expect("compute tag");

        let bad_symbol = AuthenticatedRepairSymbol {
            kind: RepairSymbolKind::Repair,
            sbn: bundle.manifest.sbn,
            esi: out_of_bounds_esi,
            payload: dummy_payload,
            tag,
        };

        let mut symbols = bundle.symbols.clone();
        symbols.push(bad_symbol);

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let err =
            decode_repair_symbols(&cx, &bundle.manifest, &symbols, &expected, key, &admission)
                .expect_err("out of bounds repair ESI must fail");

        match err {
            RepairError::InvalidGeometry { reason } => {
                assert!(reason.contains("repair symbol ESI"));
                assert!(reason.contains(">= total_symbols_generated"));
            }
            other => panic!("expected InvalidGeometry, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_cancellation_fails_closed() {
        let cx = test_cx();
        cx.cancel_fast(crate::outcome::CancelKind::User);
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(4000);
        let obj_id = sample_object_id(21);
        let key = b"test-secret-key-32-bytes-long!!";

        let live_cx = test_cx();
        let bundle = encode_repair_envelope(
            &live_cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode");

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);

        // Cancelled decode on fast path (all source symbols)
        let err_fast = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            &expected,
            key,
            &admission,
        )
        .expect_err("cancelled decode must fail");
        assert_eq!(err_fast, RepairError::Cancelled);

        // Cancelled decode on general path (drop first source symbol, keep repair symbols)
        let repair_only: Vec<_> = bundle.symbols.iter().skip(1).cloned().collect();
        let err_general = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &repair_only,
            &expected,
            key,
            &admission,
        )
        .expect_err("cancelled general decode must fail");
        assert_eq!(err_general, RepairError::Cancelled);
    }

    #[test]
    fn test_returned_buffer_permit_retention_and_shrink() {
        let cx = test_cx();
        let payload = sample_envelope(3_500);
        let k = payload.len().div_ceil(DEFAULT_SYMBOL_SIZE);
        let total_symbols = k + RepairProtectionClass::Standard.repair_symbol_count(k);
        let params = SystematicParams::try_for_source_block(k, DEFAULT_SYMBOL_SIZE).unwrap();
        // Admit exactly the larger operation peak for this geometry, including
        // the encoder's retained scratch and output buffers.
        let memory_budget =
            calculate_encoder_memory_budget(&params, total_symbols, DEFAULT_SYMBOL_SIZE)
                .unwrap()
                .max(
                    calculate_decoder_memory_budget(&params, total_symbols, DEFAULT_SYMBOL_SIZE)
                        .unwrap(),
                );
        assert!(memory_budget > payload.len());
        let admission = RepairAdmissionController::new(2, memory_budget, total_symbols);
        let obj_id = sample_object_id(22);
        let key = b"test-secret-key-32-bytes-long!!";

        let live_cx = test_cx();
        let bundle = encode_repair_envelope(
            &live_cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode");

        // After encode bundle returned, its temporary permit dropped
        assert_eq!(admission.allocated_memory_bytes(), 0);
        assert_eq!(admission.active_operations(), 0);

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let result = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            &expected,
            key,
            &admission,
        )
        .expect("decode");

        // The returned buffer (3,500 bytes) retains its permit charge, shrunk from scratchpad!
        assert_eq!(result.reconstructed_envelope.len(), 3_500);
        assert_eq!(result.permit.allocated_bytes(), 3_500);
        assert_eq!(admission.allocated_memory_bytes(), 3_500);
        assert_eq!(admission.active_operations(), 1);

        // One byte beyond the remaining budget fails while the result is held.
        let err = admission
            .acquire(memory_budget - result.permit.allocated_bytes() + 1)
            .expect_err("must exceed remaining memory");
        match err {
            RepairError::AdmissionExceeded(msg) => assert!(msg.contains("memory limit")),
            other => panic!("expected AdmissionExceeded, got {other:?}"),
        }

        // Dropping result releases the permit and frees the memory back to the controller
        drop(result);
        assert_eq!(admission.allocated_memory_bytes(), 0);
        assert_eq!(admission.active_operations(), 0);

        // Now the full operation budget can be reserved again.
        let _permit = admission
            .acquire(memory_budget)
            .expect("full reservation succeeds after drop");
    }

    #[test]
    fn test_permit_into_parts_retains_reservation() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(2_000);
        let obj_id = sample_object_id(23);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode");

        let expected = ExpectedRecoveryIdentity::from_manifest(&bundle.manifest);
        let result = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            &expected,
            key,
            &admission,
        )
        .expect("decode");

        assert_eq!(admission.allocated_memory_bytes(), 2_000);
        let (envelope, permit) = result.into_parts();
        assert_eq!(envelope, payload);
        assert_eq!(admission.allocated_memory_bytes(), 2_000);
        assert_eq!(admission.active_operations(), 1);
        drop(envelope);
        drop(permit);
        assert_eq!(admission.allocated_memory_bytes(), 0);
        assert_eq!(admission.active_operations(), 0);
    }

    #[test]
    fn test_unasserted_decode_test_only_wrapper() {
        let cx = test_cx();
        let admission = RepairAdmissionController::default_production();
        let payload = sample_envelope(1_500);
        let obj_id = sample_object_id(24);
        let key = b"test-secret-key-32-bytes-long!!";

        let bundle = encode_repair_envelope(
            &cx,
            &payload,
            obj_id,
            1,
            key,
            DEFAULT_SYMBOL_SIZE,
            RepairProtectionClass::Standard,
            &admission,
        )
        .expect("encode");

        let result = decode_repair_symbols_unasserted(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            key,
            &admission,
        )
        .expect("unasserted decode in test succeeds");

        assert_eq!(result.reconstructed_envelope, payload);
    }
}
