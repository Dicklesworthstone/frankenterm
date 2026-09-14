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
//! - **Comprehensive Memory Accounting**: Pre-charges checked memory budgets covering
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

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::cx::Cx;
use crate::runtime_async::raptorq::{
    InactivationDecoder, RankStatus, ReceivedSymbol, SystematicEncoder, SystematicParams,
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
    /// Final rank status.
    pub rank_status: RankStatusDiagnostic,
}

/// Output of a successful repair decode operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairResult {
    /// Bit-for-bit recovered serialized encrypted envelope.
    pub reconstructed_envelope: Vec<u8>,
    /// Verified representation digest (matches `SHA256(reconstructed_envelope)`).
    pub representation_id: [u8; 32],
    /// Decode and symbol accounting statistics.
    pub stats: RepairStats,
}

// ---------------------------------------------------------------------------
// Error Types
// ---------------------------------------------------------------------------

/// Exhaustive error enumeration for snapshot repair encoding and decoding.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RepairError {
    #[error("Envelope byte payload is empty")]
    EmptyPayload,

    #[error("Envelope byte size {size} exceeds maximum allowable chunk limit {limit}")]
    PayloadTooLarge { size: usize, limit: usize },

    #[error("Invalid symbol size {size}: must be between {min} and {max}")]
    InvalidSymbolSize { size: usize, min: usize, max: usize },

    #[error("Authentication key is empty")]
    EmptyAuthenticationKey,

    #[error("Source symbol count {k} exceeds per-object chunk limit {limit}; callers must chunk large recovery objects")]
    ExceedsMaxSourceSymbols { k: usize, limit: usize },

    #[error("Multi-block SBN {sbn} unsupported: only single-block SBN=0 is supported")]
    MultiBlockUnsupported { sbn: u8 },

    #[error("Invalid manifest geometry: {reason}")]
    InvalidGeometry { reason: String },

    #[error("Manifest HMAC authentication failed")]
    ManifestAuthenticationFailed,

    #[error("Symbol HMAC authentication failed for ESI {esi} (kind: {kind:?})")]
    SymbolAuthenticationFailed { esi: u32, kind: RepairSymbolKind },

    #[error("Foreign representation ID: expected {expected:?}, got {got:?}")]
    ForeignRepresentationId { got: [u8; 32], expected: [u8; 32] },

    #[error("Foreign object ID: expected {expected:?}, got {got:?}")]
    ForeignObjectId { expected: [u8; 32], got: [u8; 32] },

    #[error("Foreign generation: expected {expected}, got {got}")]
    ForeignGeneration { expected: u64, got: u64 },

    #[error("Conflicting duplicate symbol detected: ESI {esi} received with divergent payload data")]
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

// ---------------------------------------------------------------------------
// Admission Control (FCP Pattern)
// ---------------------------------------------------------------------------

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
    pub fn new(max_concurrent: usize, max_memory_bytes: usize, max_symbols_buffered: usize) -> Self {
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
    pub fn acquire(&self, estimated_bytes: usize) -> Result<RepairPermit<'_>, RepairError> {
        let current_ops = self.active_operations.fetch_add(1, Ordering::SeqCst);
        if current_ops >= self.max_concurrent {
            self.active_operations.fetch_sub(1, Ordering::SeqCst);
            return Err(RepairError::AdmissionExceeded(format!(
                "concurrency limit reached ({current_ops}/{})",
                self.max_concurrent
            )));
        }

        let current_mem = self
            .allocated_memory_bytes
            .fetch_add(estimated_bytes, Ordering::SeqCst);
        if current_mem.checked_add(estimated_bytes).is_none()
            || current_mem + estimated_bytes > self.max_memory_bytes
        {
            self.allocated_memory_bytes
                .fetch_sub(estimated_bytes, Ordering::SeqCst);
            self.active_operations.fetch_sub(1, Ordering::SeqCst);
            return Err(RepairError::AdmissionExceeded(format!(
                "memory limit exceeded (requested {estimated_bytes} bytes, current {current_mem}, max {})",
                self.max_memory_bytes
            )));
        }

        Ok(RepairPermit {
            controller: self,
            allocated_bytes: estimated_bytes,
        })
    }
}

/// RAII permit releasing reserved memory and concurrency slots on drop.
pub struct RepairPermit<'a> {
    controller: &'a RepairAdmissionController,
    allocated_bytes: usize,
}

impl Drop for RepairPermit<'_> {
    fn drop(&mut self) {
        self.controller
            .allocated_memory_bytes
            .fetch_sub(self.allocated_bytes, Ordering::SeqCst);
        self.controller
            .active_operations
            .fetch_sub(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Memory Budget Estimation (Checked Arithmetic)
// ---------------------------------------------------------------------------

/// Estimate comprehensive memory consumption for decoder initialization,
/// equation matrix storage, dense inactivation solve, and intermediate symbols.
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
    // Intermediate symbols (l) + source symbols (k) + received symbols (received_symbol_count) + output assembly (k)
    let total_symbol_slots = l
        .checked_add(k)
        .and_then(|v| v.checked_add(received_symbol_count))
        .and_then(|v| v.checked_add(k))
        .ok_or_else(|| RepairError::AdmissionExceeded("symbol buffer count overflow".to_string()))?;

    let symbol_payload_bytes = total_symbol_slots
        .checked_mul(symbol_size)
        .ok_or_else(|| RepairError::AdmissionExceeded("symbol payload bytes overflow".to_string()))?;

    // 2. Dense inactivation matrix: L * L in GF(256) (conservative worst-case dense submatrix)
    let dense_matrix_bytes = l
        .checked_mul(l)
        .ok_or_else(|| RepairError::AdmissionExceeded("dense matrix bytes overflow".to_string()))?;

    // 3. Equation structures and RHS
    // Base constraints: S + H
    // Implicit padding rows: K' - K
    // Received symbol equations: received_symbol_count
    let padding_rows = k_prime.saturating_sub(k);
    let total_equations = s
        .checked_add(h)
        .and_then(|v| v.checked_add(padding_rows))
        .and_then(|v| v.checked_add(received_symbol_count))
        .ok_or_else(|| RepairError::AdmissionExceeded("equation count overflow".to_string()))?;

    // Conservative equation structure overhead: 64 bytes struct + up to 64 non-zero terms (9 bytes each)
    const ESTIMATED_EQUATION_STRUCT_OVERHEAD: usize = 640;
    let equation_slot_bytes = ESTIMATED_EQUATION_STRUCT_OVERHEAD
        .checked_add(symbol_size)
        .ok_or_else(|| RepairError::AdmissionExceeded("equation slot size overflow".to_string()))?;

    let equation_bytes = total_equations
        .checked_mul(equation_slot_bytes)
        .ok_or_else(|| RepairError::AdmissionExceeded("equation memory overflow".to_string()))?;

    // Sum all components safely
    let total_memory = symbol_payload_bytes
        .checked_add(dense_matrix_bytes)
        .and_then(|v| v.checked_add(equation_bytes))
        .ok_or_else(|| RepairError::AdmissionExceeded("total decoder memory budget overflow".to_string()))?;

    Ok(total_memory)
}

/// Estimate comprehensive memory consumption for encoder intermediate symbol
/// solve and repair symbol generation.
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
        .ok_or_else(|| RepairError::AdmissionExceeded("encoder symbol count overflow".to_string()))?;

    let symbol_payload_bytes = total_symbol_slots
        .checked_mul(symbol_size)
        .ok_or_else(|| RepairError::AdmissionExceeded("encoder symbol payload overflow".to_string()))?;

    // Constraint matrix solve: (S + H + K') rows * L columns in GF(256)
    let matrix_rows = s
        .checked_add(h)
        .and_then(|v| v.checked_add(k_prime))
        .ok_or_else(|| RepairError::AdmissionExceeded("encoder matrix rows overflow".to_string()))?;

    let matrix_bytes = matrix_rows
        .checked_mul(l)
        .ok_or_else(|| RepairError::AdmissionExceeded("encoder matrix bytes overflow".to_string()))?;

    let total_memory = symbol_payload_bytes
        .checked_add(matrix_bytes)
        .ok_or_else(|| RepairError::AdmissionExceeded("encoder total memory budget overflow".to_string()))?;

    Ok(total_memory)
}

// ---------------------------------------------------------------------------
// Cryptographic Authentication (Vetted HMAC-SHA256)
// ---------------------------------------------------------------------------

/// Compute HMAC-SHA256 for a repair manifest using the standard `hmac` crate.
pub fn compute_manifest_mac(
    key: &[u8],
    representation_id: &[u8; 32],
    object_id: &[u8; 32],
    generation: u64,
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
    let mut mac = HmacSha256::new_from_slice(&key_guard)
        .map_err(|e| RepairError::InvalidGeometry { reason: format!("HMAC key initialization failed: {e}") })?;

    mac.update(MANIFEST_MAC_DOMAIN);
    mac.update(representation_id);
    mac.update(object_id);
    mac.update(&generation.to_le_bytes());
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
    let mut mac = HmacSha256::new_from_slice(&key_guard)
        .map_err(|e| RepairError::InvalidGeometry { reason: format!("HMAC key initialization failed: {e}") })?;

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
        let payload = encoder
            .try_repair_symbol(esi)
            .map_err(|e| RepairError::DecoderError(format!("encoder repair symbol failed: {e:?}")))?;

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

    Ok(EncodedRepairBundle {
        manifest,
        symbols: output_symbols,
    })
}

/// Convenience encoder using default symbol size (1 KiB) and default production admission controller.
pub fn encode_repair_envelope_default(
    cx: &Cx,
    envelope_bytes: &[u8],
    object_id: [u8; 32],
    generation: u64,
    auth_key: &[u8],
    protection: RepairProtectionClass,
) -> Result<EncodedRepairBundle, RepairError> {
    let admission = RepairAdmissionController::default_production();
    encode_repair_envelope(
        cx,
        envelope_bytes,
        object_id,
        generation,
        auth_key,
        DEFAULT_SYMBOL_SIZE,
        protection,
        &admission,
    )
}

// ---------------------------------------------------------------------------
// Decoding Pipeline
// ---------------------------------------------------------------------------

/// Decode and reconstruct the exact serialized encrypted envelope from received symbols,
/// with optional caller assertions on expected identity.
///
/// # Invariants Enforced
/// 1. Manifest HMAC authentication must verify before any symbol inspection.
/// 2. Optional expected representation_id, object_id, and generation must match manifest.
/// 3. Pre-allocation check on `symbols.len() <= admission.max_symbols_buffered`.
/// 4. Comprehensive memory charging (symbol payloads, intermediate symbols, $L \times L$ dense matrix, equations).
/// 5. Every accepted symbol must match the manifest's geometry and pass individual HMAC verification.
/// 6. Duplicate symbols with identical payloads are deduplicated without inflating rank.
/// 7. Conflicting duplicate symbols (same ESI, divergent payload) trigger immediate rejection.
/// 8. Equation rank is checked prior to solving; deficits return structured diagnostic error.
/// 9. Reconstructed bytes must match `manifest.representation_id` before returning.
pub fn decode_repair_symbols_with_expected(
    cx: &Cx,
    manifest: &RepairManifest,
    symbols: &[AuthenticatedRepairSymbol],
    expected_representation_id: Option<&[u8; 32]>,
    expected_object_id: Option<&[u8; 32]>,
    expected_generation: Option<u64>,
    auth_key: &[u8],
    admission: &RepairAdmissionController,
) -> Result<RepairResult, RepairError> {
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

    // 2. Validate expected context if supplied by caller
    if let Some(expected_rep) = expected_representation_id {
        if expected_rep != &manifest.representation_id {
            return Err(RepairError::ForeignRepresentationId {
                expected: *expected_rep,
                got: manifest.representation_id,
            });
        }
    }

    if let Some(expected_obj) = expected_object_id {
        if expected_obj != &manifest.object_id {
            return Err(RepairError::ForeignObjectId {
                expected: *expected_obj,
                got: manifest.object_id,
            });
        }
    }

    if let Some(expected_gen) = expected_generation {
        if expected_gen != manifest.generation {
            return Err(RepairError::ForeignGeneration {
                expected: expected_gen,
                got: manifest.generation,
            });
        }
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

    let payload_len = manifest.payload_len as usize;
    if payload_len == 0 || payload_len > MAX_CHUNK_ENVELOPE_BYTES {
        return Err(RepairError::InvalidGeometry {
            reason: format!("payload_len {payload_len} out of bounds"),
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
    let _permit = admission.acquire(estimated_memory)?;

    // 6. Initialize InactivationDecoder
    let seed = derive_raptorq_seed(&manifest.representation_id, manifest.sbn);
    let decoder = InactivationDecoder::try_new(k, symbol_size, seed)
        .map_err(|e| RepairError::DecoderError(format!("decoder initialization failed: {e:?}")))?;

    // 7. Authenticate, validate, and deduplicate received symbols
    let mut seen_esis: HashSet<u32> = HashSet::with_capacity(symbols.len());
    let mut validated_source: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut validated_repair: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut duplicates_discarded = 0usize;

    for (idx, sym) in symbols.iter().enumerate() {
        if idx % 64 == 0 && cx.checkpoint().is_err() {
            return Err(RepairError::Cancelled);
        }

        // Verify symbol SBN
        if sym.sbn != manifest.sbn {
            return Err(RepairError::MultiBlockUnsupported { sbn: sym.sbn });
        }

        // Verify symbol payload size matches manifest
        if sym.payload.len() != symbol_size {
            return Err(RepairError::InvalidGeometry {
                reason: format!(
                    "symbol payload size {} mismatch manifest {}",
                    sym.payload.len(),
                    symbol_size
                ),
            });
        }

        // Authenticate symbol MAC using vetted constant-time verify_slice
        if !verify_symbol_mac(auth_key, manifest, sym) {
            return Err(RepairError::SymbolAuthenticationFailed {
                esi: sym.esi,
                kind: sym.kind,
            });
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
                if sym.esi as usize >= k {
                    return Err(RepairError::InvalidGeometry {
                        reason: format!("source symbol ESI {} >= K {}", sym.esi, k),
                    });
                }
                validated_source.push((sym.esi, sym.payload.clone()));
            }
            RepairSymbolKind::Repair => {
                if (sym.esi as usize) < k {
                    return Err(RepairError::InvalidGeometry {
                        reason: format!("repair symbol ESI {} < K {}", sym.esi, k),
                    });
                }
                validated_repair.push((sym.esi, sym.payload.clone()));
            }
        }
    }

    // Fast path: all K source symbols present
    if validated_source.len() == k {
        // Sort by ESI to ensure canonical byte order
        validated_source.sort_by_key(|(esi, _)| *esi);
        let mut reconstructed = Vec::with_capacity(k * symbol_size);
        for (_, sym_data) in &validated_source {
            reconstructed.extend_from_slice(sym_data);
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

        let l = decoder.params().l;
        return Ok(RepairResult {
            reconstructed_envelope: reconstructed,
            representation_id: manifest.representation_id,
            stats: RepairStats {
                source_symbols_used: k,
                repair_symbols_used: 0,
                duplicates_discarded,
                rank_status: RankStatusDiagnostic {
                    rank: l,
                    columns: l,
                    deficit: 0,
                },
            },
        });
    }

    // General decoding path using RaptorQ InactivationDecoder
    // Populate with LDPC/HDPC constraint symbols
    let mut received_equations = decoder.constraint_symbols();

    // Add received source symbols
    for (esi, data) in &validated_source {
        received_equations.push(ReceivedSymbol::source(*esi, data.clone()));
    }

    // Add received repair symbols with derived repair equations
    for (esi, data) in &validated_repair {
        let (columns, coefficients) = decoder
            .repair_equation(*esi)
            .map_err(|e| RepairError::DecoderError(format!("repair equation failed for ESI {esi}: {e:?}")))?;
        received_equations.push(ReceivedSymbol::repair(*esi, columns, coefficients, data.clone()));
    }

    // Check rank status
    let rank_profile = decoder
        .rank_status(&received_equations)
        .map_err(|e| RepairError::DecoderError(format!("rank evaluation failed: {e:?}")))?;

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

    if decode_result.source.len() < k {
        return Err(RepairError::DecoderError(format!(
            "decoded source count {} < expected {}",
            decode_result.source.len(),
            k
        )));
    }

    // Assemble reconstructed envelope
    let mut reconstructed = Vec::with_capacity(k * symbol_size);
    for sym in decode_result.source.iter().take(k) {
        reconstructed.extend_from_slice(sym);
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

    Ok(RepairResult {
        reconstructed_envelope: reconstructed,
        representation_id: manifest.representation_id,
        stats: RepairStats {
            source_symbols_used: validated_source.len(),
            repair_symbols_used: validated_repair.len(),
            duplicates_discarded,
            rank_status: RankStatusDiagnostic {
                rank: rank_profile.rank,
                columns: rank_profile.columns,
                deficit: rank_profile.deficit,
            },
        },
    })
}

/// Decode and reconstruct the exact serialized encrypted envelope from received symbols.
pub fn decode_repair_symbols(
    cx: &Cx,
    manifest: &RepairManifest,
    symbols: &[AuthenticatedRepairSymbol],
    auth_key: &[u8],
    admission: &RepairAdmissionController,
) -> Result<RepairResult, RepairError> {
    decode_repair_symbols_with_expected(
        cx, manifest, symbols, None, None, None, auth_key, admission,
    )
}

/// Convenience decoder using default production admission controller.
pub fn decode_repair_symbols_default(
    cx: &Cx,
    manifest: &RepairManifest,
    symbols: &[AuthenticatedRepairSymbol],
    auth_key: &[u8],
) -> Result<RepairResult, RepairError> {
    let admission = RepairAdmissionController::default_production();
    decode_repair_symbols(cx, manifest, symbols, auth_key, &admission)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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

        let result = decode_repair_symbols(&cx, &bundle.manifest, &bundle.symbols, key, &admission)
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

        let result = decode_repair_symbols(&cx, &bundle.manifest, &received, key, &admission)
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

        let result = decode_repair_symbols(&cx, &bundle.manifest, &received, key, &admission)
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

        let result = decode_repair_symbols(&cx, &bundle.manifest, &received, key, &admission)
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

        let result = decode_repair_symbols(&cx, &bundle.manifest, &received, key, &admission)
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

        let err = decode_repair_symbols(&cx, &bundle.manifest, &received, key, &admission)
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
    fn test_tampered_symbol_payload_rejected() {
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

        let err = decode_repair_symbols(&cx, &bundle.manifest, &tampered_symbols, key, &admission)
            .expect_err("tampered symbol must fail");

        match err {
            RepairError::SymbolAuthenticationFailed { esi, .. } => {
                assert_eq!(esi, tampered_symbols[0].esi);
            }
            other => panic!("expected SymbolAuthenticationFailed, got {other:?}"),
        }
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

        let err =
            decode_repair_symbols(&cx, &tampered_manifest, &bundle.symbols, key, &admission)
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

        let err = decode_repair_symbols(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
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

        let result =
            decode_repair_symbols(&cx, &bundle.manifest, &symbols_with_dupes, key, &admission)
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

        let err =
            decode_repair_symbols(&cx, &bundle.manifest, &symbols_with_conflict, key, &admission)
                .expect_err("conflicting duplicate must fail");

        match err {
            RepairError::ConflictingDuplicateSymbol { esi } => {
                assert_eq!(esi, bundle.symbols[0].esi);
            }
            other => panic!("expected ConflictingDuplicateSymbol, got {other:?}"),
        }
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
        let err = decode_repair_symbols_with_expected(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            Some(&foreign_rep),
            None,
            None,
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
        let err2 = decode_repair_symbols_with_expected(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            None,
            Some(&foreign_obj),
            None,
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
        let err3 = decode_repair_symbols_with_expected(
            &cx,
            &bundle.manifest,
            &bundle.symbols,
            None,
            None,
            Some(999),
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
            multiblock_manifest.sbn,
            multiblock_manifest.k,
            multiblock_manifest.symbol_size,
            multiblock_manifest.payload_len,
            multiblock_manifest.total_symbols_generated,
        )
        .expect("compute mac");

        let err = decode_repair_symbols(
            &cx,
            &multiblock_manifest,
            &bundle.symbols,
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
        cx.cancel(); // Pre-cancel context
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
        let permit2_err = admission.acquire(1024).expect_err("second permit exceeds concurrency");
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
}
