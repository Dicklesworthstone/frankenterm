//! Property-based tests for stream_hash module.
//!
//! Verifies the homomorphic rolling hash invariants:
//! - Incremental update matches one-shot hash (consistency)
//! - Homomorphic combine: H(A || B) == combine(H(A), H(B))
//! - Associativity: combine(combine(A,B), C) == combine(A, combine(B,C))
//! - Byte-at-a-time matches bulk update
//! - IntegrityChecker matches/divergence detection
//! - Digest serialization roundtrip
//! - Collision resistance (distinct inputs → distinct digests)

use proptest::prelude::*;

use frankenterm_core::stream_hash::{IntegrityChecker, StreamDigest, StreamHash};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..256)
}

fn arb_nonempty_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..256)
}

fn arb_byte_chunks() -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(arb_bytes(), 1..8)
}

// ────────────────────────────────────────────────────────────────────
// Homomorphic property: H(A || B) == combine(H(A), H(B))
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// The core invariant: hashing A||B in one shot equals combining
    /// independent hashes of A and B.
    #[test]
    fn prop_homomorphic_combine(
        a in arb_bytes(),
        b in arb_bytes(),
    ) {
        // One-shot: hash(A || B)
        let mut full = StreamHash::new();
        full.update(&a);
        full.update(&b);

        // Split: combine(hash(A), hash(B))
        let mut ha = StreamHash::new();
        ha.update(&a);
        let mut hb = StreamHash::new();
        hb.update(&b);
        let combined = ha.combine(&hb);

        prop_assert_eq!(
            full.digest(), combined.digest(),
            "H(A||B) != combine(H(A), H(B)) for |A|={}, |B|={}",
            a.len(), b.len()
        );
    }

    /// Three-way homomorphic: H(A || B || C) via any grouping.
    #[test]
    fn prop_homomorphic_three_way(
        a in arb_bytes(),
        b in arb_bytes(),
        c in arb_bytes(),
    ) {
        // One-shot
        let mut full = StreamHash::new();
        full.update(&a);
        full.update(&b);
        full.update(&c);

        // (A, B) then C
        let mut ha = StreamHash::new();
        ha.update(&a);
        let mut hb = StreamHash::new();
        hb.update(&b);
        let mut hc = StreamHash::new();
        hc.update(&c);

        let ab_then_c = ha.combine(&hb).combine(&hc);
        let a_then_bc = ha.combine(&hb.combine(&hc));

        prop_assert_eq!(full.digest(), ab_then_c.digest(), "((A||B)||C) mismatch");
        prop_assert_eq!(full.digest(), a_then_bc.digest(), "(A||(B||C)) mismatch");
    }
}

// ────────────────────────────────────────────────────────────────────
// Associativity: combine is associative
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// combine(combine(A, B), C) == combine(A, combine(B, C))
    #[test]
    fn prop_combine_associative(
        a in arb_bytes(),
        b in arb_bytes(),
        c in arb_bytes(),
    ) {
        let mut ha = StreamHash::new();
        ha.update(&a);
        let mut hb = StreamHash::new();
        hb.update(&b);
        let mut hc = StreamHash::new();
        hc.update(&c);

        let left = ha.combine(&hb).combine(&hc);
        let right = ha.combine(&hb.combine(&hc));

        prop_assert_eq!(left.digest(), right.digest(), "Combine not associative");
    }
}

// ────────────────────────────────────────────────────────────────────
// Incremental consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Chunked updates produce the same digest as a single update.
    #[test]
    fn prop_chunked_matches_oneshot(
        chunks in arb_byte_chunks(),
    ) {
        let all_bytes: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();

        // One-shot
        let mut oneshot = StreamHash::new();
        oneshot.update(&all_bytes);

        // Chunked
        let mut chunked = StreamHash::new();
        for chunk in &chunks {
            chunked.update(chunk);
        }

        prop_assert_eq!(
            oneshot.digest(), chunked.digest(),
            "Chunked update differs from one-shot"
        );
    }

    /// Byte-at-a-time via update_byte matches bulk update.
    #[test]
    fn prop_byte_at_a_time_matches_bulk(
        data in arb_bytes(),
    ) {
        let mut bulk = StreamHash::new();
        bulk.update(&data);

        let mut bytewise = StreamHash::new();
        for &b in &data {
            bytewise.update_byte(b);
        }

        prop_assert_eq!(bulk.digest(), bytewise.digest(), "Byte-at-a-time differs");
    }

    /// bytes_hashed() accurately tracks total bytes fed.
    #[test]
    fn prop_bytes_hashed_accurate(
        chunks in arb_byte_chunks(),
    ) {
        let mut h = StreamHash::new();
        let mut expected_len = 0u64;
        for chunk in &chunks {
            h.update(chunk);
            expected_len += chunk.len() as u64;
        }
        prop_assert_eq!(h.bytes_hashed(), expected_len);
    }
}

// ────────────────────────────────────────────────────────────────────
// Identity element
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Combining with empty hash is identity: combine(H(A), H("")) == H(A).
    #[test]
    fn prop_combine_empty_is_identity(
        data in arb_bytes(),
    ) {
        let mut h = StreamHash::new();
        h.update(&data);
        let empty = StreamHash::new();

        let combined = h.combine(&empty);
        prop_assert_eq!(h.digest(), combined.digest(), "Combine with empty should be identity");
    }

    /// Combining empty with H(A) is also H(A): combine(H(""), H(A)) == H(A).
    #[test]
    fn prop_empty_combine_is_identity(
        data in arb_bytes(),
    ) {
        let empty = StreamHash::new();
        let mut h = StreamHash::new();
        h.update(&data);

        let combined = empty.combine(&h);
        prop_assert_eq!(h.digest(), combined.digest(), "Empty combine should be identity");
    }
}

// ────────────────────────────────────────────────────────────────────
// Reset
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After reset, the hash behaves as if freshly created.
    #[test]
    fn prop_reset_returns_to_initial(
        data1 in arb_bytes(),
        data2 in arb_bytes(),
    ) {
        let mut h = StreamHash::new();
        h.update(&data1);
        h.reset();
        h.update(&data2);

        let mut fresh = StreamHash::new();
        fresh.update(&data2);

        prop_assert_eq!(h.digest(), fresh.digest(), "Reset didn't return to initial state");
    }
}

// ────────────────────────────────────────────────────────────────────
// Collision resistance (weak test — just checks distinct inputs)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Different non-empty inputs should (almost certainly) produce different digests.
    #[test]
    fn prop_distinct_inputs_distinct_digests(
        a in arb_nonempty_bytes(),
        b in arb_nonempty_bytes(),
    ) {
        prop_assume!(a != b);

        let mut ha = StreamHash::new();
        ha.update(&a);
        let mut hb = StreamHash::new();
        hb.update(&b);

        // With 128-bit hash, collision probability is ~2^-64 per pair.
        // In 300 test cases, this is effectively impossible.
        prop_assert_ne!(
            ha.digest(), hb.digest(),
            "Collision for distinct inputs (|a|={}, |b|={})",
            a.len(), b.len()
        );
    }

    /// Appending any non-empty suffix changes the digest.
    #[test]
    fn prop_suffix_changes_digest(
        prefix in arb_bytes(),
        suffix in arb_nonempty_bytes(),
    ) {
        let mut h_prefix = StreamHash::new();
        h_prefix.update(&prefix);

        let mut h_full = StreamHash::new();
        h_full.update(&prefix);
        h_full.update(&suffix);

        prop_assert_ne!(
            h_prefix.digest(), h_full.digest(),
            "Appending bytes didn't change digest"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Digest serialization
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// StreamDigest JSON roundtrip preserves all fields.
    #[test]
    fn prop_digest_serde_roundtrip(
        data in arb_bytes(),
    ) {
        let mut h = StreamHash::new();
        h.update(&data);
        let digest = h.digest();

        let json = serde_json::to_string(&digest).unwrap();
        let back: StreamDigest = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(digest, back, "Serde roundtrip changed digest");
    }

    /// Digest.hex() is 32 hex chars (128 bits).
    #[test]
    fn prop_digest_hex_length(
        data in arb_bytes(),
    ) {
        let mut h = StreamHash::new();
        h.update(&data);
        let hex = h.digest().hex();
        prop_assert_eq!(hex.len(), 32, "Hex should be 32 chars, got {}", hex.len());
        prop_assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "Hex contains non-hex chars: {}",
            hex
        );
    }

    /// Digest.len matches bytes_hashed.
    #[test]
    fn prop_digest_len_matches_bytes_hashed(
        data in arb_bytes(),
    ) {
        let mut h = StreamHash::new();
        h.update(&data);
        prop_assert_eq!(h.digest().len, h.bytes_hashed());
    }

    /// Display format includes the hash and byte count.
    #[test]
    fn prop_digest_display_format(
        data in arb_nonempty_bytes(),
    ) {
        let mut h = StreamHash::new();
        h.update(&data);
        let display = format!("{}", h.digest());
        prop_assert!(display.contains(':'), "Display should contain ':'");
        let parts: Vec<&str> = display.split(':').collect();
        prop_assert_eq!(parts.len(), 2, "Display should be 'hash:len'");
        prop_assert_eq!(parts[0].len(), 32, "Hash part should be 32 hex chars");
    }
}

// ────────────────────────────────────────────────────────────────────
// IntegrityChecker
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// IntegrityChecker passes when producer and consumer see identical bytes.
    #[test]
    fn prop_integrity_check_passes_on_matching_streams(
        data in arb_nonempty_bytes(),
    ) {
        // Producer side
        let mut producer = StreamHash::new();
        producer.update(&data);
        let producer_digest = producer.digest();

        // Consumer side (integrity checker)
        let mut checker = IntegrityChecker::new();
        checker.update(&data);
        checker.set_remote_digest(producer_digest);

        let result = checker.check();
        prop_assert!(result.is_some(), "Check should return a result at matching offset");
        let result = result.unwrap();
        prop_assert!(result.matches, "Identical streams should match");
        prop_assert_eq!(result.byte_offset, data.len() as u64);
        prop_assert_eq!(checker.checks_passed(), 1);
    }

    /// IntegrityChecker fails when streams diverge.
    #[test]
    fn prop_integrity_check_fails_on_divergent_streams(
        data in arb_nonempty_bytes(),
        extra_byte in any::<u8>(),
    ) {
        // Producer sees data + extra_byte
        let mut producer = StreamHash::new();
        producer.update(&data);
        producer.update_byte(extra_byte);

        // Consumer sees only data + different byte
        let different_byte = extra_byte.wrapping_add(1);
        let mut checker = IntegrityChecker::new();
        checker.update(&data);
        checker.update(&[different_byte]);
        checker.set_remote_digest(producer.digest());

        let result = checker.check();
        prop_assert!(result.is_some());
        let result = result.unwrap();
        prop_assert!(!result.matches, "Divergent streams should not match");
        prop_assert_eq!(checker.checks_passed(), 0);
    }

    /// IntegrityChecker returns None when byte offsets differ.
    #[test]
    fn prop_integrity_check_none_on_offset_mismatch(
        data in arb_nonempty_bytes(),
        extra in arb_nonempty_bytes(),
    ) {
        let mut producer = StreamHash::new();
        producer.update(&data);

        let mut checker = IntegrityChecker::new();
        checker.update(&data);
        checker.update(&extra); // Consumer ahead
        checker.set_remote_digest(producer.digest());

        let result = checker.check();
        prop_assert!(result.is_none(), "Different byte counts should return None");
    }

    /// IntegrityChecker returns None when no remote digest has been set.
    #[test]
    fn prop_integrity_check_none_without_remote(
        data in arb_bytes(),
    ) {
        let mut checker = IntegrityChecker::new();
        checker.update(&data);

        let result = checker.check();
        prop_assert!(result.is_none(), "Should be None without remote digest");
        prop_assert_eq!(checker.checks_performed(), 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// Digest.matches() consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// matches() is reflexive: d.matches(&d) is always true.
    #[test]
    fn prop_digest_matches_reflexive(
        data in arb_bytes(),
    ) {
        let mut h = StreamHash::new();
        h.update(&data);
        let d = h.digest();
        prop_assert!(d.matches(&d));
    }

    /// matches() is symmetric.
    #[test]
    fn prop_digest_matches_symmetric(
        a in arb_bytes(),
        b in arb_bytes(),
    ) {
        let mut ha = StreamHash::new();
        ha.update(&a);
        let mut hb = StreamHash::new();
        hb.update(&b);

        let da = ha.digest();
        let db = hb.digest();
        prop_assert_eq!(da.matches(&db), db.matches(&da));
    }
}
