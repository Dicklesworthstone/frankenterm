//! Homomorphic stream hashing for output integrity verification.
//!
//! Terminal output passes through multiple layers: PTY → mux → codec → client.
//! Any layer could silently drop or corrupt bytes. This module provides O(1)
//! space integrity verification using polynomial rolling hashes.
//!
//! # Homomorphic property
//!
//! A hash function H is homomorphic over concatenation if:
//!
//! > H(A ‖ B) = combine(H(A), H(B), |B|)
//!
//! This means both sides of a stream can independently compute running hashes
//! and compare periodically — if they diverge, bytes were lost or corrupted.
//! No buffering required.
//!
//! # Algorithm
//!
//! Uses polynomial rolling hash (Rabin fingerprint variant):
//!
//! > H(b₁ b₂ ... bₙ) = Σᵢ bᵢ × baseⁿ⁻ⁱ mod p
//!
//! where `p` is a large prime and `base` is a random multiplier.
//! Combining two hashes: `H(A ‖ B) = H(A) × base^|B| + H(B) mod p`.
//!
//! The 128-bit variant uses two independent polynomial hashes with different
//! primes for negligible collision probability (< 2⁻⁶⁴).

use serde::{Deserialize, Serialize};

// =============================================================================
// Constants
// =============================================================================

/// First prime for the dual-hash scheme.
const P1: u128 = 0xFFFF_FFFF_FFFF_FFC5; // 2^64 - 59 (a Mersenne-adjacent prime)
/// Second prime for the dual-hash scheme.
const P2: u128 = 0xFFFF_FFFF_FFFF_FF43; // 2^64 - 189
/// Base for the first hash.
const BASE1: u128 = 257;
/// Base for the second hash.
const BASE2: u128 = 263;

// =============================================================================
// StreamHash (128-bit, dual polynomial)
// =============================================================================

/// A 128-bit stream hash using dual polynomial rolling hashes.
///
/// Supports:
/// - O(1) per-byte incremental update.
/// - Homomorphic combination: `H(A ‖ B) = combine(H(A), H(B), len(B))`.
/// - Constant-space integrity checking between producer and consumer.
///
/// # Example
///
/// ```ignore
/// let mut producer = StreamHash::new();
/// producer.update(b"hello ");
/// producer.update(b"world");
///
/// let mut consumer = StreamHash::new();
/// consumer.update(b"hello world");
///
/// assert_eq!(producer.digest(), consumer.digest());
/// ```
#[derive(Debug, Clone)]
pub struct StreamHash {
    h1: u128,
    h2: u128,
    len: u64,
    /// Running power: base^len mod p (for efficient combine).
    pow1: u128,
    pow2: u128,
}

impl StreamHash {
    /// Create a new empty stream hash.
    #[must_use]
    pub fn new() -> Self {
        Self {
            h1: 0,
            h2: 0,
            len: 0,
            pow1: 1,
            pow2: 1,
        }
    }

    /// Feed bytes into the hash.
    pub fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.h1 = (self.h1.wrapping_mul(BASE1).wrapping_add(b as u128)) % P1;
            self.h2 = (self.h2.wrapping_mul(BASE2).wrapping_add(b as u128)) % P2;
            self.pow1 = self.pow1.wrapping_mul(BASE1) % P1;
            self.pow2 = self.pow2.wrapping_mul(BASE2) % P2;
        }
        self.len += data.len() as u64;
    }

    /// Feed a single byte into the hash.
    pub fn update_byte(&mut self, b: u8) {
        self.h1 = (self.h1.wrapping_mul(BASE1).wrapping_add(b as u128)) % P1;
        self.h2 = (self.h2.wrapping_mul(BASE2).wrapping_add(b as u128)) % P2;
        self.pow1 = self.pow1.wrapping_mul(BASE1) % P1;
        self.pow2 = self.pow2.wrapping_mul(BASE2) % P2;
        self.len += 1;
    }

    /// Get the current 128-bit digest.
    #[must_use]
    pub fn digest(&self) -> StreamDigest {
        StreamDigest {
            h1: self.h1 as u64,
            h2: self.h2 as u64,
            len: self.len,
        }
    }

    /// Number of bytes hashed so far.
    #[must_use]
    pub fn bytes_hashed(&self) -> u64 {
        self.len
    }

    /// Combine this hash with another (homomorphic concatenation).
    ///
    /// Computes `H(self_data ‖ other_data)` from `H(self_data)` and
    /// `H(other_data)` without access to the original data.
    #[must_use]
    pub fn combine(&self, other: &StreamHash) -> StreamHash {
        // H(A || B) = H(A) * base^|B| + H(B)
        let h1 = (self.h1.wrapping_mul(other.pow1).wrapping_add(other.h1)) % P1;
        let h2 = (self.h2.wrapping_mul(other.pow2).wrapping_add(other.h2)) % P2;
        let pow1 = self.pow1.wrapping_mul(other.pow1) % P1;
        let pow2 = self.pow2.wrapping_mul(other.pow2) % P2;
        StreamHash {
            h1,
            h2,
            len: self.len + other.len,
            pow1,
            pow2,
        }
    }

    /// Reset to empty state.
    pub fn reset(&mut self) {
        self.h1 = 0;
        self.h2 = 0;
        self.len = 0;
        self.pow1 = 1;
        self.pow2 = 1;
    }
}

impl Default for StreamHash {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// StreamDigest (serializable snapshot)
// =============================================================================

/// A serializable snapshot of a stream hash state.
///
/// Contains the 128-bit hash (as two u64s) and the byte count.
/// Two digests are equal iff the streams that produced them contained
/// the same bytes in the same order (with overwhelming probability).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamDigest {
    /// First 64 bits of the hash.
    pub h1: u64,
    /// Second 64 bits of the hash.
    pub h2: u64,
    /// Number of bytes that were hashed.
    pub len: u64,
}

impl StreamDigest {
    /// Whether two digests indicate the same stream content.
    #[must_use]
    pub fn matches(&self, other: &StreamDigest) -> bool {
        self == other
    }

    /// Hex string representation of the 128-bit hash.
    #[must_use]
    pub fn hex(&self) -> String {
        format!("{:016x}{:016x}", self.h1, self.h2)
    }
}

impl std::fmt::Display for StreamDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}{:016x}:{}", self.h1, self.h2, self.len)
    }
}

// =============================================================================
// IntegrityChecker (producer/consumer verification)
// =============================================================================

/// Compares two stream hashes for integrity verification.
///
/// Typically one side is the producer (source) and the other is the
/// consumer (destination). Periodic comparison detects byte loss or
/// corruption.
pub struct IntegrityChecker {
    local: StreamHash,
    /// Last known remote digest (received periodically).
    remote_digest: Option<StreamDigest>,
    /// Number of checks performed.
    checks_performed: u64,
    /// Number of checks that passed.
    checks_passed: u64,
}

impl IntegrityChecker {
    /// Create a new integrity checker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            local: StreamHash::new(),
            remote_digest: None,
            checks_performed: 0,
            checks_passed: 0,
        }
    }

    /// Feed bytes into the local hash.
    pub fn update(&mut self, data: &[u8]) {
        self.local.update(data);
    }

    /// Update the remote digest (received from the other end).
    pub fn set_remote_digest(&mut self, digest: StreamDigest) {
        self.remote_digest = Some(digest);
    }

    /// Check integrity: compare local hash against the last known remote digest.
    ///
    /// Returns `None` if no remote digest has been set yet, or if the
    /// byte counts don't match (comparison only valid at identical offsets).
    #[must_use]
    pub fn check(&mut self) -> Option<IntegrityResult> {
        let remote = self.remote_digest?;
        let local = self.local.digest();

        // Only compare at the same byte offset.
        if local.len != remote.len {
            return None;
        }

        self.checks_performed += 1;
        let matches = local.matches(&remote);
        if matches {
            self.checks_passed += 1;
        }

        Some(IntegrityResult {
            matches,
            local_digest: local,
            remote_digest: remote,
            byte_offset: local.len,
        })
    }

    /// Local digest at the current position.
    #[must_use]
    pub fn local_digest(&self) -> StreamDigest {
        self.local.digest()
    }

    /// Total integrity checks performed.
    #[must_use]
    pub fn checks_performed(&self) -> u64 {
        self.checks_performed
    }

    /// Total integrity checks that passed.
    #[must_use]
    pub fn checks_passed(&self) -> u64 {
        self.checks_passed
    }
}

impl Default for IntegrityChecker {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of an integrity check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrityResult {
    /// Whether the streams match.
    pub matches: bool,
    /// Local digest at the comparison point.
    pub local_digest: StreamDigest,
    /// Remote digest at the comparison point.
    pub remote_digest: StreamDigest,
    /// Byte offset at which comparison was made.
    pub byte_offset: u64,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- StreamHash basic -------------------------------------------------------

    #[test]
    fn empty_hash_is_zero() {
        let h = StreamHash::new();
        assert_eq!(h.bytes_hashed(), 0);
        let d = h.digest();
        assert_eq!(d.h1, 0);
        assert_eq!(d.h2, 0);
        assert_eq!(d.len, 0);
    }

    #[test]
    fn single_byte_hash() {
        let mut h = StreamHash::new();
        h.update_byte(42);
        assert_eq!(h.bytes_hashed(), 1);
        let d = h.digest();
        assert_ne!(d.h1, 0);
        assert_ne!(d.h2, 0);
    }

    #[test]
    fn same_content_same_hash() {
        let mut h1 = StreamHash::new();
        let mut h2 = StreamHash::new();
        h1.update(b"hello world");
        h2.update(b"hello world");
        assert_eq!(h1.digest(), h2.digest());
    }

    #[test]
    fn different_content_different_hash() {
        let mut h1 = StreamHash::new();
        let mut h2 = StreamHash::new();
        h1.update(b"hello");
        h2.update(b"world");
        assert_ne!(h1.digest(), h2.digest());
    }

    #[test]
    fn incremental_equals_batch() {
        let data = b"the quick brown fox";

        let mut batch = StreamHash::new();
        batch.update(data);

        let mut inc = StreamHash::new();
        for &b in data {
            inc.update_byte(b);
        }

        assert_eq!(batch.digest(), inc.digest());
    }

    #[test]
    fn chunked_equals_batch() {
        let data = b"abcdefghijklmnopqrstuvwxyz0123456789";

        let mut batch = StreamHash::new();
        batch.update(data);

        let mut chunked = StreamHash::new();
        chunked.update(&data[..10]);
        chunked.update(&data[10..20]);
        chunked.update(&data[20..]);

        assert_eq!(batch.digest(), chunked.digest());
    }

    // -- Homomorphic combine ---------------------------------------------------

    #[test]
    fn combine_produces_correct_hash() {
        let data_a = b"hello ";
        let data_b = b"world";
        let combined_data = b"hello world";

        let mut ha = StreamHash::new();
        ha.update(data_a);

        let mut hb = StreamHash::new();
        hb.update(data_b);

        let combined = ha.combine(&hb);

        let mut expected = StreamHash::new();
        expected.update(combined_data);

        assert_eq!(combined.digest(), expected.digest());
    }

    #[test]
    fn combine_three_parts() {
        let mut h1 = StreamHash::new();
        h1.update(b"aaa");

        let mut h2 = StreamHash::new();
        h2.update(b"bbb");

        let mut h3 = StreamHash::new();
        h3.update(b"ccc");

        let combined = h1.combine(&h2).combine(&h3);

        let mut expected = StreamHash::new();
        expected.update(b"aaabbbccc");

        assert_eq!(combined.digest(), expected.digest());
    }

    #[test]
    fn combine_with_empty() {
        let mut h = StreamHash::new();
        h.update(b"data");

        let empty = StreamHash::new();

        // Combining with empty should not change the hash.
        let r1 = h.combine(&empty);
        assert_eq!(r1.digest(), h.digest());

        // Empty combining with data should equal data.
        let r2 = empty.combine(&h);
        assert_eq!(r2.digest(), h.digest());
    }

    #[test]
    fn combine_is_not_commutative() {
        let mut ha = StreamHash::new();
        ha.update(b"AB");

        let mut hb = StreamHash::new();
        hb.update(b"CD");

        let ab = ha.combine(&hb);
        let ba = hb.combine(&ha);

        // "ABCD" != "CDAB" so hashes should differ.
        assert_ne!(ab.digest(), ba.digest());
    }

    // -- StreamDigest -----------------------------------------------------------

    #[test]
    fn digest_display() {
        let d = StreamDigest {
            h1: 0x1234_5678_9ABC_DEF0,
            h2: 0xFEDC_BA98_7654_3210,
            len: 42,
        };
        let s = format!("{d}");
        assert!(s.contains("123456789abcdef0"));
        assert!(s.contains("fedcba9876543210"));
        assert!(s.contains(":42"));
    }

    #[test]
    fn digest_hex() {
        let d = StreamDigest {
            h1: 1,
            h2: 2,
            len: 0,
        };
        let hex = d.hex();
        assert_eq!(hex.len(), 32);
        assert_eq!(hex, "00000000000000010000000000000002");
    }

    #[test]
    fn digest_serde_roundtrip() {
        let d = StreamDigest {
            h1: 123456789,
            h2: 987654321,
            len: 100,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: StreamDigest = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn digest_matches() {
        let d1 = StreamDigest {
            h1: 10,
            h2: 20,
            len: 5,
        };
        let d2 = StreamDigest {
            h1: 10,
            h2: 20,
            len: 5,
        };
        let d3 = StreamDigest {
            h1: 10,
            h2: 21,
            len: 5,
        };
        assert!(d1.matches(&d2));
        assert!(!d1.matches(&d3));
    }

    // -- IntegrityChecker -------------------------------------------------------

    #[test]
    fn integrity_check_matching_streams() {
        let data = b"test data for integrity";

        let mut producer = StreamHash::new();
        producer.update(data);

        let mut checker = IntegrityChecker::new();
        checker.update(data);
        checker.set_remote_digest(producer.digest());

        let result = checker.check().unwrap();
        assert!(result.matches);
        assert_eq!(checker.checks_performed(), 1);
        assert_eq!(checker.checks_passed(), 1);
    }

    #[test]
    fn integrity_check_mismatched_streams() {
        let mut producer = StreamHash::new();
        producer.update(b"hello world");

        let mut checker = IntegrityChecker::new();
        checker.update(b"hello worlx"); // one byte different
        checker.set_remote_digest(producer.digest());

        let result = checker.check().unwrap();
        assert!(!result.matches);
        assert_eq!(checker.checks_performed(), 1);
        assert_eq!(checker.checks_passed(), 0);
    }

    #[test]
    fn integrity_check_no_remote() {
        let mut checker = IntegrityChecker::new();
        checker.update(b"data");
        assert!(checker.check().is_none());
    }

    #[test]
    fn integrity_check_different_lengths() {
        let mut producer = StreamHash::new();
        producer.update(b"short");

        let mut checker = IntegrityChecker::new();
        checker.update(b"longer data");
        checker.set_remote_digest(producer.digest());

        // Different lengths → comparison not valid.
        assert!(checker.check().is_none());
    }

    // -- Reset ------------------------------------------------------------------

    #[test]
    fn hash_reset() {
        let mut h = StreamHash::new();
        h.update(b"some data");
        assert_ne!(h.bytes_hashed(), 0);

        h.reset();
        assert_eq!(h.bytes_hashed(), 0);
        assert_eq!(h.digest(), StreamHash::new().digest());
    }

    // -- Large data -------------------------------------------------------------

    #[test]
    fn large_data_consistency() {
        let data: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();

        let mut h1 = StreamHash::new();
        h1.update(&data);

        // Hash in 1000-byte chunks.
        let mut h2 = StreamHash::new();
        for chunk in data.chunks(1000) {
            h2.update(chunk);
        }

        assert_eq!(h1.digest(), h2.digest());
    }

    #[test]
    fn large_data_combine() {
        let data: Vec<u8> = (0..50_000).map(|i| (i % 256) as u8).collect();
        let mid = data.len() / 2;

        let mut full = StreamHash::new();
        full.update(&data);

        let mut part1 = StreamHash::new();
        part1.update(&data[..mid]);

        let mut part2 = StreamHash::new();
        part2.update(&data[mid..]);

        let combined = part1.combine(&part2);
        assert_eq!(combined.digest(), full.digest());
    }

    // -- Byte order sensitivity -------------------------------------------------

    #[test]
    fn order_matters() {
        let mut h1 = StreamHash::new();
        h1.update(&[1, 2, 3]);

        let mut h2 = StreamHash::new();
        h2.update(&[3, 2, 1]);

        assert_ne!(h1.digest(), h2.digest());
    }

    #[test]
    fn repeated_bytes_distinguishable() {
        let mut h1 = StreamHash::new();
        h1.update(&[0, 0, 0, 1]);

        let mut h2 = StreamHash::new();
        h2.update(&[0, 0, 1, 0]);

        assert_ne!(h1.digest(), h2.digest());
    }

    // -- IntegrityResult serde --------------------------------------------------

    #[test]
    fn integrity_result_serde() {
        let result = IntegrityResult {
            matches: true,
            local_digest: StreamDigest {
                h1: 1,
                h2: 2,
                len: 10,
            },
            remote_digest: StreamDigest {
                h1: 1,
                h2: 2,
                len: 10,
            },
            byte_offset: 10,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: IntegrityResult = serde_json::from_str(&json).unwrap();
        assert!(back.matches);
        assert_eq!(back.byte_offset, 10);
    }
}
