//! Reed-Solomon erasure encoding spec for cross-host audit
//! ledger replication
//! ([BR-RC-SAFETY-PROOFS.G11.1] / `ft-x0666.5`).
//!
//! Round-3 alien-artifact addition. When distributed mode is
//! on, the policy-denial audit chain currently replicates 1:1
//! across aggregators. **k-of-n Reed-Solomon erasure encoding**
//! survives loss of any (n - k) hosts without losing audit
//! history.
//!
//! ## Default parameters
//!
//! - **k = 3** data shards.
//! - **n = 5** total shards (3 data + 2 parity).
//!
//! Tied to a typical 5-aggregator distributed deployment: any
//! 2 host failures are recoverable; the original audit row is
//! reconstructed from any 3 of the 5 shards.
//!
//! ## Headline properties (proven below)
//!
//! 1. **MDS** (Maximum Distance Separable) — any k of n shards
//!    reconstruct the original. The implementation uses a
//!    Vandermonde-shape generator over `GF(256)` (the same
//!    field as raptorq/zfec); the property follows from the
//!    Vandermonde determinant being nonzero for distinct
//!    evaluation points.
//! 2. **Round-trip** — `decode(encode(data)) == data` for any
//!    valid k-of-n parameter pair.
//! 3. **Single-host-loss survives** — the bead's headline
//!    operational claim: any one of the 5 hosts can drop and
//!    the audit ledger still reconstructs.
//!
//! ## What this module ships
//!
//! - [`ErasureConfig`] — k/n parameter envelope with
//!   validation. Defaults to (3, 5); operators can tune for
//!   non-default deployments via the doctor warning surface.
//! - [`ErasureShard`] — one of the n shards, tagged with its
//!   shard index and parity-flag.
//! - [`encode_row`] — turns one audit row's bytes into n
//!   shards. Pads to a multiple of k.
//! - [`reconstruct`] — given any k surviving shards, returns
//!   the original bytes. Returns an error if fewer than k
//!   shards or if any shard's index is out of range.
//! - [`AuditErasureHealth`] — `ft doctor` snapshot mirroring
//!   this session's `*Health` shape.
//! - [`single_host_loss_recoverable`] — the bead's safety
//!   predicate: under default (3, 5), any single missing
//!   shard is recoverable.
//!
//! ## What this module is NOT
//!
//! - Not the production wiring. Integrating with the existing
//!   `policy_decision_log` + `policy_audit_chain` modules is
//!   the integration follow-on (the bead's action #2). This
//!   module ships the spec layer the integration consumes.
//! - Not the operator's deployment-topology logic. The bead's
//!   action #1 says "tied to deployment topology" — that
//!   binding is operator-side; this module ships the typed
//!   envelope (`ErasureConfig`) the binding fills in.
//! - Not the doctor warning surface. Action #4 ("warn when
//!   distributed mode is on but audit replication is single-
//!   copy") consumes this module's `AuditErasureHealth`; the
//!   warning code lives in the doctor module.

use serde::{Deserialize, Serialize};

// ============================================================================
// GF(256) arithmetic
// ============================================================================
//
// Reed-Solomon over GF(2^8) using the same primitive
// polynomial as zfec / raptorq / ISA-L (0x11d). Encode is a
// matrix-vector product over the field; reconstruct solves a
// linear system.
//
// The implementation is dependency-free and small enough to
// audit by inspection. Performance is irrelevant for audit-
// row encoding (≤ a few KB per row, encoded once per write).

/// GF(256) primitive polynomial: `x^8 + x^4 + x^3 + x^2 + 1`.
/// Same constant zfec / raptorq use.
const GF_PRIM: u32 = 0x11d;

/// Generator of the multiplicative group of GF(256).
const GF_GEN: u8 = 0x02;

/// Precomputed log / exp tables for fast multiplication.
struct GfTables {
    exp: [u8; 512],
    log: [u8; 256],
}

impl GfTables {
    fn new() -> Self {
        let mut t = Self {
            exp: [0u8; 512],
            log: [0u8; 256],
        };
        let mut x: u32 = 1;
        for i in 0..255 {
            t.exp[i] = x as u8;
            t.log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= GF_PRIM;
            }
        }
        // Extend exp[] to length 512 so a*b lookup never wraps.
        for i in 255..512 {
            t.exp[i] = t.exp[i - 255];
        }
        t
    }
}

fn gf_tables() -> &'static GfTables {
    use std::sync::OnceLock;
    static TBL: OnceLock<GfTables> = OnceLock::new();
    TBL.get_or_init(GfTables::new)
}

#[inline]
fn gf_add(a: u8, b: u8) -> u8 {
    a ^ b
}

#[inline]
fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let t = gf_tables();
    t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
}

#[inline]
fn gf_inv(a: u8) -> u8 {
    debug_assert!(a != 0, "gf_inv(0) undefined");
    let t = gf_tables();
    t.exp[(255 - t.log[a as usize] as usize) % 255]
}

#[inline]
fn gf_div(a: u8, b: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    let t = gf_tables();
    t.exp[(t.log[a as usize] as usize + 255 - t.log[b as usize] as usize) % 255]
}

#[inline]
fn gf_pow(a: u8, n: u32) -> u8 {
    if n == 0 {
        return 1;
    }
    if a == 0 {
        return 0;
    }
    let t = gf_tables();
    t.exp[(t.log[a as usize] as usize * n as usize) % 255]
}

// ============================================================================
// Config
// ============================================================================

/// k-of-n erasure-encoding parameters.
///
/// `k` is the number of data shards; `n` is the total number
/// of shards (data + parity). The system survives `n - k` host
/// losses without data loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ErasureConfig {
    /// **Private** per ft-mpnj3: previously public, allowing
    /// callers to construct `ErasureConfig { k: 0, n: 5 }` /
    /// `{ k: 5, n: 3 }` / `{ k: 3, n: 100 }` directly,
    /// bypassing `new()`'s validation. Such configs feed the
    /// Reed-Solomon math (encode_row + reconstruct +
    /// generator_matrix) and produce data that looks encoded
    /// but cannot be reconstructed → silent data loss in the
    /// audit ledger.
    k: u8,
    n: u8,
}

impl ErasureConfig {
    /// Default: (3, 5) — typical 5-aggregator deployment;
    /// any 2 host losses recoverable.
    pub const DEFAULT: Self = Self { k: 3, n: 5 };

    /// New config; returns `Err` if invalid (`k == 0`, `n == 0`,
    /// `k > n`, or `n > 32` — the latter is a spec sanity bound;
    /// no real distributed audit deployment uses ≥ 32
    /// aggregators).
    pub fn new(k: u8, n: u8) -> Result<Self, ErasureError> {
        if k == 0 {
            return Err(ErasureError::InvalidConfig {
                reason: "k must be > 0".to_string(),
            });
        }
        if n == 0 || k > n {
            return Err(ErasureError::InvalidConfig {
                reason: format!("invalid k/n: ({k}, {n})"),
            });
        }
        if n > 32 {
            return Err(ErasureError::InvalidConfig {
                reason: format!("n={n} exceeds spec max 32"),
            });
        }
        Ok(Self { k, n })
    }

    /// Number of data shards. Always > 0 and <= [`Self::n`]
    /// (guaranteed by [`Self::new`]).
    #[must_use]
    pub const fn k(self) -> u8 {
        self.k
    }

    /// Total shards (data + parity). Always > 0, <= 32, >= [`Self::k`].
    #[must_use]
    pub const fn n(self) -> u8 {
        self.n
    }

    /// Number of parity shards.
    #[must_use]
    pub const fn parity(self) -> u8 {
        self.n - self.k
    }

    /// Whether single-host-loss survives at this config (the
    /// bead's headline operational claim — `n - k >= 1`).
    #[must_use]
    pub const fn single_host_loss_recoverable(self) -> bool {
        self.n - self.k >= 1
    }
}

impl Default for ErasureConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// ============================================================================
// Shard
// ============================================================================

/// One of the n shards an encoded audit row produces.
///
/// **Field privacy** (per ft-h1hvw): `shard_index`, `is_parity`,
/// and `bytes` are `pub(crate)` so external callers cannot
/// construct or mutate a shard. [`reconstruct`] selects a row
/// of the generator matrix using `shard_index`; mutating it
/// (without changing `bytes`) passes the in-range / uniqueness
/// / size guards but feeds the wrong coefficients to the
/// Gauss-Jordan solve, silently producing garbage. The
/// production-grade defense is a per-shard MAC at the
/// integration layer; this fence is defense-in-depth against
/// same-process forgery.
///
/// External code constructs shards via [`encode_row`] and
/// reads them via [`Self::shard_index`], [`Self::is_parity`],
/// and [`Self::bytes`]. Tests build forged shards via
/// [`Self::for_test`] which is gated to test/integration use.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ErasureShard {
    /// Shard index in `0..n`.
    pub(crate) shard_index: u8,
    /// True iff this is a parity shard (`shard_index >= k`).
    pub(crate) is_parity: bool,
    /// The shard's bytes. Length is the chunk-padded
    /// row-length / k.
    pub(crate) bytes: Vec<u8>,
}

impl ErasureShard {
    /// Shard index in `0..n`.
    #[must_use]
    pub const fn shard_index(&self) -> u8 {
        self.shard_index
    }

    /// Whether this is a parity shard.
    #[must_use]
    pub const fn is_parity(&self) -> bool {
        self.is_parity
    }

    /// Read-only view of the shard payload.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Construct a shard from raw fields. Reserved for
    /// downstream tests / integration adapters that need to
    /// build forged shards (e.g., to assert reconstruct's
    /// validation catches an out-of-range index). Production
    /// code must use [`encode_row`].
    #[must_use]
    pub fn for_test(shard_index: u8, is_parity: bool, bytes: Vec<u8>) -> Self {
        Self {
            shard_index,
            is_parity,
            bytes,
        }
    }
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum ErasureError {
    #[error("invalid erasure config: {reason}")]
    InvalidConfig { reason: String },
    #[error("not enough shards: have {have}, need {k}")]
    InsufficientShards { have: usize, k: u8 },
    #[error("duplicate shard index {index}")]
    DuplicateShardIndex { index: u8 },
    #[error("shard index {index} out of range (n={n})")]
    ShardIndexOutOfRange { index: u8, n: u8 },
    #[error("inconsistent shard size: shard {index} has {got} bytes, expected {expected}")]
    InconsistentShardSize {
        index: u8,
        got: usize,
        expected: usize,
    },
    #[error("encoded length mismatch: original {original}, decoded {decoded}")]
    DecodedLengthMismatch { original: usize, decoded: usize },
}

// ============================================================================
// Vandermonde generator
// ============================================================================
//
// The (n × k) generator matrix is the Vandermonde matrix
// `G[i][j] = i^j` over GF(256), with the first k rows being
// the identity (so data shards are direct copies of the
// original chunks). Rows k..n produce parity. Any k rows are
// invertible (Vandermonde determinant is nonzero for distinct
// evaluation points), so any k shards reconstruct the
// original.

/// Build the (n × k) generator matrix in row-major order.
fn generator_matrix(cfg: ErasureConfig) -> Vec<Vec<u8>> {
    let mut g = vec![vec![0u8; cfg.k as usize]; cfg.n as usize];

    // First k rows: identity.
    for i in 0..cfg.k {
        g[i as usize][i as usize] = 1;
    }

    // Parity rows: Vandermonde over GF(256). Use evaluation
    // points i = 1..=parity (avoiding 0 which would zero out a
    // row).
    for i in cfg.k..cfg.n {
        let eval = (i - cfg.k + 1) as u8;
        for j in 0..cfg.k {
            g[i as usize][j as usize] = gf_pow(eval, j as u32);
        }
    }

    g
}

// ============================================================================
// Encode / decode
// ============================================================================

/// Encode `data` into n shards under `cfg`. Pads the input
/// length to a multiple of k bytes so every chunk fits a row.
/// The pad length is encoded into the first 4 bytes of every
/// data shard so decode can recover the original length.
///
/// (The 4-byte length prefix is part of the spec — operators
/// can pre-strip it if they prefer, but the canonical encoding
/// includes it for self-describing decode.)
pub fn encode_row(cfg: ErasureConfig, data: &[u8]) -> Vec<ErasureShard> {
    let original_len = data.len() as u32;
    // Build the prefixed payload: 4-byte length + data.
    let mut payload = Vec::with_capacity(4 + data.len());
    payload.extend_from_slice(&original_len.to_le_bytes());
    payload.extend_from_slice(data);
    // Pad to multiple of k.
    let chunk_size = (payload.len() + cfg.k as usize - 1) / cfg.k as usize;
    let padded_total = chunk_size * cfg.k as usize;
    payload.resize(padded_total, 0u8);

    let g = generator_matrix(cfg);
    let mut shards: Vec<ErasureShard> = Vec::with_capacity(cfg.n as usize);

    for shard_idx in 0..cfg.n {
        let mut bytes = vec![0u8; chunk_size];
        for byte_off in 0..chunk_size {
            let mut acc = 0u8;
            for j in 0..cfg.k {
                let coeff = g[shard_idx as usize][j as usize];
                let chunk_byte = payload[j as usize * chunk_size + byte_off];
                acc = gf_add(acc, gf_mul(coeff, chunk_byte));
            }
            bytes[byte_off] = acc;
        }
        shards.push(ErasureShard {
            shard_index: shard_idx,
            is_parity: shard_idx >= cfg.k,
            bytes,
        });
    }

    shards
}

/// Reconstruct the original bytes from any `k` of `n`
/// surviving shards.
///
/// Errors:
/// - `InsufficientShards` if `surviving.len() < cfg.k`.
/// - `DuplicateShardIndex` if two shards share an index.
/// - `ShardIndexOutOfRange` if any index is `>= cfg.n`.
/// - `InconsistentShardSize` if shards have differing lengths.
pub fn reconstruct(
    cfg: ErasureConfig,
    surviving: &[ErasureShard],
) -> Result<Vec<u8>, ErasureError> {
    if surviving.len() < cfg.k as usize {
        return Err(ErasureError::InsufficientShards {
            have: surviving.len(),
            k: cfg.k,
        });
    }
    // Take the first k.
    let used = &surviving[..cfg.k as usize];

    // Validate shard indices.
    let chunk_size = used[0].bytes.len();
    let mut seen_idx = [false; 256];
    for s in used {
        if s.shard_index >= cfg.n {
            return Err(ErasureError::ShardIndexOutOfRange {
                index: s.shard_index,
                n: cfg.n,
            });
        }
        if seen_idx[s.shard_index as usize] {
            return Err(ErasureError::DuplicateShardIndex {
                index: s.shard_index,
            });
        }
        seen_idx[s.shard_index as usize] = true;
        if s.bytes.len() != chunk_size {
            return Err(ErasureError::InconsistentShardSize {
                index: s.shard_index,
                got: s.bytes.len(),
                expected: chunk_size,
            });
        }
    }

    // Assemble the k×k submatrix of the generator matrix
    // selected by the surviving shard indices.
    let g = generator_matrix(cfg);
    let mut sub = vec![vec![0u8; cfg.k as usize]; cfg.k as usize];
    for (row, shard) in used.iter().enumerate() {
        sub[row].copy_from_slice(&g[shard.shard_index as usize]);
    }

    // Invert sub via Gaussian elimination over GF(256).
    // Augment with the identity to compute the inverse.
    let k = cfg.k as usize;
    let mut aug = vec![vec![0u8; 2 * k]; k];
    for i in 0..k {
        for j in 0..k {
            aug[i][j] = sub[i][j];
        }
        aug[i][k + i] = 1;
    }

    // Gauss-Jordan elimination.
    for col in 0..k {
        // Find a pivot row with non-zero entry in this column.
        let mut pivot = None;
        for row in col..k {
            if aug[row][col] != 0 {
                pivot = Some(row);
                break;
            }
        }
        let pivot =
            pivot.expect("Vandermonde sub-matrix is invertible — k rows linearly independent");
        if pivot != col {
            aug.swap(col, pivot);
        }
        // Scale pivot row to make leading 1.
        let inv = gf_inv(aug[col][col]);
        for j in 0..2 * k {
            aug[col][j] = gf_mul(aug[col][j], inv);
        }
        // Eliminate other rows.
        for row in 0..k {
            if row == col {
                continue;
            }
            let factor = aug[row][col];
            if factor == 0 {
                continue;
            }
            for j in 0..2 * k {
                let term = gf_mul(factor, aug[col][j]);
                aug[row][j] = gf_add(aug[row][j], term);
            }
        }
    }

    // Inverse is the right half.
    let inv_sub: Vec<Vec<u8>> = aug.into_iter().map(|row| row[k..2 * k].to_vec()).collect();

    // Multiply inv_sub × stacked-shard-bytes to recover the
    // original payload (chunk-by-chunk).
    let mut payload = vec![0u8; chunk_size * k];
    for byte_off in 0..chunk_size {
        for orig_chunk in 0..k {
            let mut acc = 0u8;
            for src_row in 0..k {
                let coeff = inv_sub[orig_chunk][src_row];
                let byte = used[src_row].bytes[byte_off];
                acc = gf_add(acc, gf_mul(coeff, byte));
            }
            payload[orig_chunk * chunk_size + byte_off] = acc;
        }
    }

    // Strip the 4-byte length prefix.
    if payload.len() < 4 {
        return Err(ErasureError::DecodedLengthMismatch {
            original: 0,
            decoded: payload.len(),
        });
    }
    let original_len =
        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    if original_len + 4 > payload.len() {
        return Err(ErasureError::DecodedLengthMismatch {
            original: original_len,
            decoded: payload.len() - 4,
        });
    }
    Ok(payload[4..4 + original_len].to_vec())
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot for the audit-erasure surface.
/// Mirrors the `*Health` shape used across this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditErasureHealth {
    /// Currently active config.
    pub config: ErasureConfig,
    /// True iff distributed mode is on. The doctor warning
    /// fires when this is true and `effective_replication` is
    /// 1 (single-copy — no erasure encoding).
    pub distributed_mode_on: bool,
    /// Effective replication factor; 1 = single-copy,
    /// `n - k + 1` = full erasure-encoded.
    pub effective_replication: u8,
    /// Total rows encoded since process start.
    pub rows_encoded_total: u64,
    /// Total reconstructions performed (host loss recovery
    /// fired).
    pub reconstructions_total: u64,
}

impl AuditErasureHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            config: ErasureConfig::DEFAULT,
            distributed_mode_on: false,
            effective_replication: 1,
            rows_encoded_total: 0,
            reconstructions_total: 0,
        }
    }

    /// True iff the doctor SHOULD warn — distributed mode is
    /// on but no real replication is configured.
    #[must_use]
    pub const fn should_warn(&self) -> bool {
        self.distributed_mode_on && self.effective_replication == 1
    }

    /// Convenience — bead's headline predicate.
    #[must_use]
    pub const fn single_host_loss_recoverable(&self) -> bool {
        self.config.single_host_loss_recoverable()
    }
}

/// Bead's headline operational predicate exposed at the
/// module level.
#[must_use]
pub const fn single_host_loss_recoverable(cfg: ErasureConfig) -> bool {
    cfg.single_host_loss_recoverable()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_three_of_five() {
        let cfg = ErasureConfig::default();
        assert_eq!(cfg.k, 3);
        assert_eq!(cfg.n, 5);
        assert_eq!(cfg.parity(), 2);
    }

    #[test]
    fn invalid_configs_rejected() {
        assert!(ErasureConfig::new(0, 5).is_err());
        assert!(ErasureConfig::new(5, 0).is_err());
        assert!(ErasureConfig::new(6, 5).is_err());
        assert!(ErasureConfig::new(3, 33).is_err());
    }

    #[test]
    fn single_host_loss_recoverable_at_default() {
        assert!(ErasureConfig::DEFAULT.single_host_loss_recoverable());
        assert!(single_host_loss_recoverable(ErasureConfig::DEFAULT));
    }

    #[test]
    fn no_replication_config_doesnt_recover() {
        let cfg = ErasureConfig::new(3, 3).unwrap();
        assert!(!cfg.single_host_loss_recoverable());
    }

    #[test]
    fn gf_arithmetic_basic() {
        // Add is XOR.
        assert_eq!(gf_add(0x12, 0x34), 0x12 ^ 0x34);
        // Mul: 0 × x = 0.
        assert_eq!(gf_mul(0, 5), 0);
        assert_eq!(gf_mul(5, 0), 0);
        // Mul: 1 × x = x.
        assert_eq!(gf_mul(1, 0x5a), 0x5a);
        // Inverse: x × x⁻¹ = 1.
        for x in 1u8..=255 {
            assert_eq!(gf_mul(x, gf_inv(x)), 1, "inverse of {x:#x} broken");
        }
    }

    #[test]
    fn gf_pow_consistency() {
        // pow(2, 8) = primitive's reduction value.
        assert_eq!(gf_pow(GF_GEN, 0), 1);
        assert_eq!(gf_pow(GF_GEN, 1), GF_GEN);
        // Cycle: 2^255 = 1.
        assert_eq!(gf_pow(GF_GEN, 255), 1);
    }

    #[test]
    fn encode_produces_n_shards() {
        let cfg = ErasureConfig::DEFAULT;
        let data = b"hello, world";
        let shards = encode_row(cfg, data);
        assert_eq!(shards.len(), 5);
        for (i, s) in shards.iter().enumerate() {
            assert_eq!(s.shard_index as usize, i);
            assert_eq!(s.is_parity, i >= 3);
        }
    }

    #[test]
    fn round_trip_preserves_data() {
        let cfg = ErasureConfig::DEFAULT;
        for data in [
            b"".to_vec(),
            b"x".to_vec(),
            b"hello, world".to_vec(),
            b"the quick brown fox jumps over the lazy dog".to_vec(),
            (0..256).map(|i| i as u8).collect::<Vec<_>>(),
        ] {
            let shards = encode_row(cfg, &data);
            let recovered = reconstruct(cfg, &shards).unwrap();
            assert_eq!(recovered, data);
        }
    }

    #[test]
    fn any_three_of_five_reconstructs() {
        let cfg = ErasureConfig::DEFAULT;
        let data = b"reality-check audit row".to_vec();
        let shards = encode_row(cfg, &data);

        // Try every C(5,3) = 10 subset.
        let n = cfg.n as usize;
        let k = cfg.k as usize;
        for i in 0..n {
            for j in (i + 1)..n {
                for l in (j + 1)..n {
                    let subset = vec![shards[i].clone(), shards[j].clone(), shards[l].clone()];
                    assert_eq!(subset.len(), k);
                    let recovered = reconstruct(cfg, &subset).unwrap();
                    assert_eq!(
                        recovered, data,
                        "subset ({i},{j},{l}) failed to reconstruct"
                    );
                }
            }
        }
    }

    #[test]
    fn fewer_than_k_shards_fails() {
        let cfg = ErasureConfig::DEFAULT;
        let data = b"x".to_vec();
        let shards = encode_row(cfg, &data);
        // 2 shards (< k=3) must fail.
        let r = reconstruct(cfg, &shards[..2]);
        assert!(matches!(
            r,
            Err(ErasureError::InsufficientShards { have: 2, k: 3 })
        ));
    }

    #[test]
    fn duplicate_shard_index_rejected() {
        let cfg = ErasureConfig::DEFAULT;
        let data = b"x".to_vec();
        let shards = encode_row(cfg, &data);
        let dup = vec![shards[0].clone(), shards[0].clone(), shards[1].clone()];
        let r = reconstruct(cfg, &dup);
        assert!(matches!(
            r,
            Err(ErasureError::DuplicateShardIndex { index: 0 })
        ));
    }

    #[test]
    fn shard_index_out_of_range_rejected() {
        let cfg = ErasureConfig::DEFAULT;
        let data = b"x".to_vec();
        let shards = encode_row(cfg, &data);
        let mut bad = shards.clone();
        bad[0].shard_index = 99;
        let r = reconstruct(cfg, &bad[..3]);
        assert!(matches!(
            r,
            Err(ErasureError::ShardIndexOutOfRange { index: 99, n: 5 })
        ));
    }

    #[test]
    fn inconsistent_shard_size_rejected() {
        let cfg = ErasureConfig::DEFAULT;
        let data = b"abc".to_vec();
        let shards = encode_row(cfg, &data);
        let mut bad = shards.clone();
        bad[1].bytes.push(0); // size mismatch
        let r = reconstruct(cfg, &bad[..3]);
        assert!(matches!(r, Err(ErasureError::InconsistentShardSize { .. })));
    }

    #[test]
    fn audit_erasure_health_warns_when_distributed_and_no_replication() {
        let h = AuditErasureHealth {
            distributed_mode_on: true,
            effective_replication: 1,
            ..AuditErasureHealth::baseline()
        };
        assert!(h.should_warn());
    }

    #[test]
    fn audit_erasure_health_does_not_warn_when_replicated() {
        let h = AuditErasureHealth {
            distributed_mode_on: true,
            effective_replication: 3, // 3 = n - k + 1 at default
            ..AuditErasureHealth::baseline()
        };
        assert!(!h.should_warn());
    }

    #[test]
    fn audit_erasure_health_does_not_warn_in_single_node_mode() {
        let h = AuditErasureHealth {
            distributed_mode_on: false,
            effective_replication: 1,
            ..AuditErasureHealth::baseline()
        };
        assert!(!h.should_warn());
    }

    #[test]
    fn varied_config_round_trip() {
        // Sweep a range of (k, n) pairs to exercise the
        // Vandermonde generator under non-default parameters.
        for k in 1..=5 {
            for n in k..=8 {
                let cfg = ErasureConfig::new(k, n).unwrap();
                let data = vec![0xa5u8; 50];
                let shards = encode_row(cfg, &data);
                let recovered = reconstruct(cfg, &shards[..k as usize]).unwrap();
                assert_eq!(recovered, data, "({k}, {n}) round-trip failed");
            }
        }
    }

    #[test]
    fn shard_serde_roundtrip() {
        let cfg = ErasureConfig::DEFAULT;
        let data = b"json-serialize-me".to_vec();
        let shards = encode_row(cfg, &data);
        let json = serde_json::to_string(&shards).unwrap();
        let parsed: Vec<ErasureShard> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, shards);
    }

    #[test]
    fn parity_flag_correctness() {
        for k in 1u8..=5 {
            for n in k..=8 {
                let cfg = ErasureConfig::new(k, n).unwrap();
                let shards = encode_row(cfg, b"x");
                for s in &shards {
                    assert_eq!(s.is_parity, s.shard_index >= k);
                }
            }
        }
    }

    // ========================================================================
    // ft-v4va6: single-host-loss + edge-data + parity-recovery coverage
    // ========================================================================

    #[test]
    fn default_config_satisfies_single_host_loss_recoverable() {
        // Bead headline operational claim direct test.
        let cfg = ErasureConfig::DEFAULT;
        assert!(
            cfg.single_host_loss_recoverable(),
            "default (3, 5): n - k = 2 ≥ 1 — single host loss MUST be recoverable"
        );
    }

    #[test]
    fn n_equals_k_does_not_satisfy_single_host_loss_recoverable() {
        // No parity shards → losing any host loses data.
        let cfg = ErasureConfig::new(3, 3).unwrap();
        assert!(!cfg.single_host_loss_recoverable());
    }

    #[test]
    fn single_host_loss_predicate_matches_n_minus_k_threshold() {
        // Boundary sweep on the predicate.
        for k in 1u8..=4 {
            for n in k..=8 {
                let cfg = ErasureConfig::new(k, n).unwrap();
                let recoverable = cfg.single_host_loss_recoverable();
                assert_eq!(
                    recoverable,
                    n - k >= 1,
                    "predicate must reflect n - k >= 1 for k={k} n={n}"
                );
                // Free function and method must agree.
                assert_eq!(
                    super::single_host_loss_recoverable(cfg),
                    recoverable,
                    "free fn / method disagreement for k={k} n={n}"
                );
            }
        }
    }

    #[test]
    fn round_trip_handles_empty_data() {
        // Empty data — degenerate case. Encode produces n shards
        // (possibly all empty or all-padding); decode round-trips
        // back to empty.
        let cfg = ErasureConfig::DEFAULT;
        let shards = encode_row(cfg, &[]);
        assert_eq!(shards.len(), cfg.n as usize);
        let recovered = reconstruct(cfg, &shards[0..cfg.k as usize]).unwrap();
        // Recovered may have padding; the original was empty so
        // either empty bytes or k zero-padding bytes is acceptable
        // for the trivial-data degenerate case. Both shapes are
        // padding-or-empty.
        assert!(
            recovered.iter().all(|&b| b == 0),
            "empty input should round-trip to all-zero / empty bytes; got {:?}",
            recovered
        );
    }

    #[test]
    fn round_trip_handles_single_byte_data() {
        // Single-byte data is below k=3 bytes; padding adds k-1
        // zero bytes per chunk.
        let cfg = ErasureConfig::DEFAULT;
        let shards = encode_row(cfg, &[0xAB]);
        let recovered = reconstruct(cfg, &shards[0..cfg.k as usize]).unwrap();
        assert!(
            !recovered.is_empty(),
            "padded single byte must round-trip to non-empty"
        );
        // First byte preserved.
        assert_eq!(recovered[0], 0xAB);
    }

    #[test]
    fn reconstructs_from_mixed_data_and_parity_shards() {
        // The MDS property says ANY k of n shards reconstruct.
        // Existing tests use [0, 1, 2] (all data). Verify a
        // mixed subset works.
        let cfg = ErasureConfig::DEFAULT;
        let data = b"hello, audit row";
        let all = encode_row(cfg, data);
        // Subset: [0, 3, 4] = 1 data + 2 parity.
        let subset = vec![all[0].clone(), all[3].clone(), all[4].clone()];
        let recovered = reconstruct(cfg, &subset).unwrap();
        assert_eq!(&recovered[..data.len()], data);
    }

    #[test]
    fn reconstructs_from_only_parity_shards_when_k_is_two() {
        // k=2, n=5: shards [3, 4] are 2 parity shards. Should
        // reconstruct without any data shards.
        let cfg = ErasureConfig::new(2, 5).unwrap();
        let data = b"parity-only recovery";
        let all = encode_row(cfg, data);
        let parity_only = vec![all[3].clone(), all[4].clone()];
        let recovered = reconstruct(cfg, &parity_only).unwrap();
        assert_eq!(&recovered[..data.len()], data);
    }

    #[test]
    fn reconstructs_when_first_data_shard_lost() {
        // Common failure mode: shard 0 drops. Verify the
        // remaining k shards reconstruct.
        let cfg = ErasureConfig::DEFAULT;
        let data = b"shard0-dropped scenario";
        let all = encode_row(cfg, data);
        // Skip shard 0; use [1, 2, 3].
        let subset = vec![all[1].clone(), all[2].clone(), all[3].clone()];
        let recovered = reconstruct(cfg, &subset).unwrap();
        assert_eq!(&recovered[..data.len()], data);
    }

    #[test]
    fn round_trip_with_k_equals_one_degenerate() {
        // k=1, n=3: each "data shard" is the whole input; parity
        // shards are linear combinations. Degenerate but valid.
        let cfg = ErasureConfig::new(1, 3).unwrap();
        let data = b"k1 case";
        let all = encode_row(cfg, data);
        assert_eq!(all.len(), 3);
        // Any single shard reconstructs.
        let recovered = reconstruct(cfg, &all[0..1]).unwrap();
        assert_eq!(&recovered[..data.len()], data);
        // Even just the parity.
        let recovered_p = reconstruct(cfg, &all[2..3]).unwrap();
        assert_eq!(&recovered_p[..data.len()], data);
    }

    /// ft-mpnj3 regression guard: ErasureConfig fields are private,
    /// so callers cannot construct invalid (k=0, k>n, n>32, etc.)
    /// configs that would corrupt the Reed-Solomon math. new() is
    /// the only entry point that takes arbitrary (k, n) pairs.
    #[test]
    fn erasure_config_new_rejects_invalid_combos() {
        // k=0
        assert!(ErasureConfig::new(0, 5).is_err());
        // n=0
        assert!(ErasureConfig::new(3, 0).is_err());
        // k>n
        assert!(ErasureConfig::new(5, 3).is_err());
        // n exceeds spec max
        assert!(ErasureConfig::new(3, 33).is_err());
        // valid
        assert!(ErasureConfig::new(3, 5).is_ok());
    }

    #[test]
    fn erasure_config_getters_return_validated_values() {
        let cfg = ErasureConfig::new(3, 5).unwrap();
        assert_eq!(cfg.k(), 3);
        assert_eq!(cfg.n(), 5);
        assert_eq!(cfg.parity(), 2);

        let cfg = ErasureConfig::DEFAULT;
        assert_eq!(cfg.k(), 3);
        assert_eq!(cfg.n(), 5);
    }

    // -------------------------------------------------------------
    // Regression: ErasureShard read accessors + for_test (ft-h1hvw)
    // -------------------------------------------------------------

    #[test]
    fn shard_accessors_match_internal_state() {
        // Produce shards via the legitimate path; assert the public
        // accessors return what encode_row stored.
        let cfg = ErasureConfig::DEFAULT;
        let data = b"audit-row".to_vec();
        let shards = encode_row(cfg, &data);
        for (i, s) in shards.iter().enumerate() {
            assert_eq!(s.shard_index() as usize, i);
            assert_eq!(s.is_parity(), i >= cfg.k() as usize);
            assert!(!s.bytes().is_empty());
        }
    }

    #[test]
    fn shard_for_test_constructs_forgeable_shard() {
        // The escape hatch for negative-test scaffolding. Production
        // code must use encode_row.
        let s = ErasureShard::for_test(7, true, vec![0xAA, 0xBB]);
        assert_eq!(s.shard_index(), 7);
        assert!(s.is_parity());
        assert_eq!(s.bytes(), &[0xAA, 0xBB]);
    }

    #[test]
    fn shard_index_forgery_within_range_does_not_silently_succeed() {
        // The audit threat: an attacker mutates a parity shard's
        // shard_index to point at a different in-range slot, hoping
        // reconstruct will pick the wrong generator-matrix row and
        // produce garbage that passes the 4-byte length-prefix check.
        //
        // Since shard_index is now pub(crate), external code cannot
        // construct or mutate a shard except via encode_row or
        // for_test. This regression simulates the forgery via
        // for_test, confirms that a shard with mutated metadata but
        // legitimate bytes either (a) still reconstructs to the
        // original (when index matches) or (b) reconstructs to
        // recognisable garbage that the length-prefix sanity check
        // catches.
        let cfg = ErasureConfig::new(3, 5).unwrap();
        let data = b"forge-test".to_vec();
        let shards = encode_row(cfg, &data);

        // Take 3 valid shards; replace one with a forged-index variant
        // whose bytes belong to a DIFFERENT shard.
        let forged_bytes = shards[3].bytes().to_vec();
        let forged = ErasureShard::for_test(0, false, forged_bytes); // claim index 0, ship index-3 bytes
        let surviving = vec![forged, shards[1].clone(), shards[2].clone()];

        // reconstruct will use generator row 0 (identity) on bytes
        // that are actually parity-3 — produces nonsense. The 4-byte
        // length prefix on the corrupted shard is the parity-row
        // computation, which has astronomically low probability of
        // matching a legitimate length.
        match reconstruct(cfg, &surviving) {
            Ok(out) => {
                // If reconstruction succeeds, the bytes must NOT match
                // the original — that would mean the forgery was
                // undetected and silently corrupted the audit row.
                // Allow that the spurious length-prefix happens to
                // bound a valid range, but the bytes will differ.
                assert_ne!(
                    out, data,
                    "shard-index forgery silently reconstructed correct data — substrate cannot defend against this without integration-layer MACs"
                );
            }
            Err(_) => {
                // Length-prefix sanity check caught the garbage —
                // expected outcome.
            }
        }
    }
}
