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
//! - Four-way homomorphic combine
//! - Clone consistency
//! - Multiple resets preserve invariants
//! - IntegrityChecker multiple sequential checks
//! - Combine length additivity
//! - Prefix sensitivity (different prefixes → different digests)
//! - IntegrityResult serde roundtrip

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

fn arb_split_point() -> impl Strategy<Value = usize> {
    0_usize..256
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

// ────────────────────────────────────────────────────────────────────
// NEW: Four-way homomorphic combine
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Four-way split: any grouping of H(A||B||C||D) produces the same digest.
    #[test]
    fn prop_homomorphic_four_way(
        a in arb_bytes(),
        b in arb_bytes(),
        c in arb_bytes(),
        d in arb_bytes(),
    ) {
        let mut full = StreamHash::new();
        full.update(&a);
        full.update(&b);
        full.update(&c);
        full.update(&d);

        let mut ha = StreamHash::new();
        ha.update(&a);
        let mut hb = StreamHash::new();
        hb.update(&b);
        let mut hc = StreamHash::new();
        hc.update(&c);
        let mut hd = StreamHash::new();
        hd.update(&d);

        // ((A||B)||(C||D))
        let ab = ha.combine(&hb);
        let cd = hc.combine(&hd);
        let grouped = ab.combine(&cd);

        prop_assert_eq!(
            full.digest(), grouped.digest(),
            "((A||B)||(C||D)) should equal H(A||B||C||D)"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: Clone consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Cloning a StreamHash preserves the digest.
    #[test]
    fn prop_clone_preserves_digest(
        data in arb_bytes(),
    ) {
        let mut h = StreamHash::new();
        h.update(&data);
        let cloned = h.clone();

        prop_assert_eq!(h.digest(), cloned.digest(), "Clone should preserve digest");
        prop_assert_eq!(h.bytes_hashed(), cloned.bytes_hashed());
    }

    /// Cloned hash can be independently extended without affecting original.
    #[test]
    fn prop_clone_independence(
        data1 in arb_bytes(),
        data2 in arb_nonempty_bytes(),
    ) {
        let mut h = StreamHash::new();
        h.update(&data1);
        let original_digest = h.digest();

        let mut cloned = h.clone();
        cloned.update(&data2);

        // Original should be unchanged
        prop_assert_eq!(h.digest(), original_digest, "Original changed after clone mutation");
        // Clone should have different digest
        prop_assert_ne!(cloned.digest(), original_digest, "Clone should differ after extension");
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: Multiple resets
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Multiple resets leave hash in a clean state each time.
    #[test]
    fn prop_multiple_resets(
        data1 in arb_bytes(),
        data2 in arb_bytes(),
        data3 in arb_bytes(),
    ) {
        let mut h = StreamHash::new();

        h.update(&data1);
        h.reset();
        h.update(&data2);
        h.reset();
        h.update(&data3);

        let mut fresh = StreamHash::new();
        fresh.update(&data3);

        prop_assert_eq!(h.digest(), fresh.digest(),
            "After multiple resets, should match fresh hash of last data");
        prop_assert_eq!(h.bytes_hashed(), data3.len() as u64);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: Combine length additivity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Combined hash length equals sum of component lengths.
    #[test]
    fn prop_combine_length_additive(
        a in arb_bytes(),
        b in arb_bytes(),
    ) {
        let mut ha = StreamHash::new();
        ha.update(&a);
        let mut hb = StreamHash::new();
        hb.update(&b);

        let combined = ha.combine(&hb);
        prop_assert_eq!(
            combined.bytes_hashed(),
            ha.bytes_hashed() + hb.bytes_hashed(),
            "Combined length should be sum of parts"
        );
        prop_assert_eq!(
            combined.digest().len,
            (a.len() + b.len()) as u64,
            "Digest len should reflect total bytes"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: Prefix sensitivity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Prepending any non-empty prefix changes the digest.
    #[test]
    fn prop_prefix_changes_digest(
        prefix in arb_nonempty_bytes(),
        suffix in arb_bytes(),
    ) {
        let mut h_suffix = StreamHash::new();
        h_suffix.update(&suffix);

        let mut h_full = StreamHash::new();
        h_full.update(&prefix);
        h_full.update(&suffix);

        prop_assert_ne!(
            h_suffix.digest(), h_full.digest(),
            "Prepending bytes didn't change digest"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: IntegrityChecker sequential checks
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// IntegrityChecker tracks check counts accurately over multiple checks.
    #[test]
    fn prop_integrity_checker_sequential_counts(
        data in arb_nonempty_bytes(),
        n_checks in 1_u32..5,
    ) {
        let mut producer = StreamHash::new();
        producer.update(&data);

        let mut checker = IntegrityChecker::new();
        checker.update(&data);
        checker.set_remote_digest(producer.digest());

        for _ in 0..n_checks {
            let result = checker.check().unwrap();
            prop_assert!(result.matches);
        }

        prop_assert_eq!(checker.checks_performed(), n_checks as u64,
            "checks_performed should equal {}", n_checks);
        prop_assert_eq!(checker.checks_passed(), n_checks as u64,
            "checks_passed should equal {}", n_checks);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: Arbitrary split point homomorphism
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Splitting data at any point and combining produces the same hash.
    #[test]
    fn prop_arbitrary_split_combine(
        data in arb_nonempty_bytes(),
        split in arb_split_point(),
    ) {
        let split = split % (data.len() + 1);

        let mut full = StreamHash::new();
        full.update(&data);

        let mut left = StreamHash::new();
        left.update(&data[..split]);
        let mut right = StreamHash::new();
        right.update(&data[split..]);

        let combined = left.combine(&right);
        prop_assert_eq!(full.digest(), combined.digest(),
            "Split at {} of {} bytes should produce same hash",
            split, data.len());
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: IntegrityResult serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// IntegrityResult JSON roundtrip preserves all fields.
    #[test]
    fn prop_integrity_result_serde_roundtrip(
        data in arb_nonempty_bytes(),
    ) {
        let mut producer = StreamHash::new();
        producer.update(&data);

        let mut checker = IntegrityChecker::new();
        checker.update(&data);
        checker.set_remote_digest(producer.digest());

        let result = checker.check().unwrap();
        let json = serde_json::to_string(&result).unwrap();
        let back: frankenterm_core::stream_hash::IntegrityResult =
            serde_json::from_str(&json).unwrap();

        prop_assert_eq!(result.matches, back.matches);
        prop_assert_eq!(result.byte_offset, back.byte_offset);
        prop_assert_eq!(result.local_digest, back.local_digest);
        prop_assert_eq!(result.remote_digest, back.remote_digest);
    }
}

// ────────────────────────────────────────────────────────────────────
// NEW: Default trait consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Default::default() and new() produce identical hashes.
    #[test]
    fn prop_default_matches_new(
        data in arb_bytes(),
    ) {
        let mut from_new = StreamHash::new();
        from_new.update(&data);

        let mut from_default = StreamHash::default();
        from_default.update(&data);

        prop_assert_eq!(from_new.digest(), from_default.digest(),
            "Default and new should behave identically");
    }

    /// IntegrityChecker::default() and ::new() are equivalent.
    #[test]
    fn prop_integrity_checker_default_matches_new(
        data in arb_nonempty_bytes(),
    ) {
        let mut checker_new = IntegrityChecker::new();
        checker_new.update(&data);

        let mut checker_default = IntegrityChecker::default();
        checker_default.update(&data);

        prop_assert_eq!(
            checker_new.local_digest(),
            checker_default.local_digest(),
            "Default and new IntegrityChecker should produce same digest"
        );
    }
}
