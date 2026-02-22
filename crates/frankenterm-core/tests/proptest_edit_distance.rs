//! Property-based tests for `edit_distance` — sequence comparison algorithms.

use proptest::prelude::*;

use frankenterm_core::edit_distance::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_bytes(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0..=255u8, 0..max_len)
}

fn arb_ascii(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(b'a'..=b'z', 0..max_len)
}

fn arb_string(max_len: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(b'a'..=b'z', 0..max_len)
        .prop_map(|v| String::from_utf8(v).unwrap())
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. Levenshtein identity: d(a, a) == 0
    #[test]
    fn lev_identity(a in arb_bytes(50)) {
        prop_assert_eq!(levenshtein(&a, &a), 0);
    }

    // 2. Levenshtein symmetry: d(a, b) == d(b, a)
    #[test]
    fn lev_symmetry(a in arb_bytes(30), b in arb_bytes(30)) {
        prop_assert_eq!(levenshtein(&a, &b), levenshtein(&b, &a));
    }

    // 3. Triangle inequality: d(a, c) <= d(a, b) + d(b, c)
    #[test]
    fn lev_triangle(a in arb_bytes(20), b in arb_bytes(20), c in arb_bytes(20)) {
        let ab = levenshtein(&a, &b);
        let bc = levenshtein(&b, &c);
        let ac = levenshtein(&a, &c);
        prop_assert!(ac <= ab + bc, "triangle: d(a,c)={} > d(a,b)={} + d(b,c)={}", ac, ab, bc);
    }

    // 4. Distance bounded by max length
    #[test]
    fn lev_bounded_by_max_len(a in arb_bytes(30), b in arb_bytes(30)) {
        let d = levenshtein(&a, &b);
        prop_assert!(d <= a.len().max(b.len()));
    }

    // 5. Distance >= length difference
    #[test]
    fn lev_at_least_length_diff(a in arb_bytes(30), b in arb_bytes(30)) {
        let d = levenshtein(&a, &b);
        prop_assert!(d >= a.len().abs_diff(b.len()));
    }

    // 6. Empty string distance = other's length
    #[test]
    fn lev_empty(a in arb_bytes(30)) {
        prop_assert_eq!(levenshtein(&a, &[]), a.len());
        prop_assert_eq!(levenshtein(&[], &a), a.len());
    }

    // 7. Normalized in [0, 1]
    #[test]
    fn lev_normalized_range(a in arb_bytes(30), b in arb_bytes(30)) {
        let sim = levenshtein_normalized(&a, &b);
        prop_assert!(sim >= 0.0);
        prop_assert!(sim <= 1.0);
    }

    // 8. Normalized identity = 1.0
    #[test]
    fn lev_normalized_identity(a in arb_bytes(30)) {
        let sim = levenshtein_normalized(&a, &a);
        prop_assert!((sim - 1.0).abs() < f64::EPSILON);
    }

    // 9. Damerau-Levenshtein >= 0
    #[test]
    fn dl_non_negative(a in arb_bytes(20), b in arb_bytes(20)) {
        let _ = damerau_levenshtein(&a, &b);
    }

    // 10. DL identity
    #[test]
    fn dl_identity(a in arb_bytes(30)) {
        prop_assert_eq!(damerau_levenshtein(&a, &a), 0);
    }

    // 11. DL symmetry
    #[test]
    fn dl_symmetry(a in arb_bytes(20), b in arb_bytes(20)) {
        prop_assert_eq!(damerau_levenshtein(&a, &b), damerau_levenshtein(&b, &a));
    }

    // 12. DL <= Levenshtein (transpositions can only help)
    #[test]
    fn dl_le_lev(a in arb_bytes(20), b in arb_bytes(20)) {
        let dl = damerau_levenshtein(&a, &b);
        let lev = levenshtein(&a, &b);
        prop_assert!(dl <= lev, "DL={} > Lev={}", dl, lev);
    }

    // 13. Hamming = None for different lengths
    #[test]
    fn hamming_diff_len(a in arb_bytes(30), extra in 1..20usize) {
        let mut b = a.clone();
        b.extend(vec![0u8; extra]);
        prop_assert_eq!(hamming(&a, &b), None);
    }

    // 14. Hamming identity
    #[test]
    fn hamming_identity(a in arb_bytes(30)) {
        prop_assert_eq!(hamming(&a, &a), Some(0));
    }

    // 15. Hamming <= Levenshtein for equal-length
    #[test]
    fn hamming_le_lev(len in 0..20usize, seed_a in arb_ascii(20), seed_b in arb_ascii(20)) {
        // Generate equal-length slices from seeds
        let a: Vec<u8> = seed_a.iter().copied().chain(std::iter::repeat(b'a')).take(len).collect();
        let b: Vec<u8> = seed_b.iter().copied().chain(std::iter::repeat(b'b')).take(len).collect();
        if let Some(h) = hamming(&a, &b) {
            let l = levenshtein(&a, &b);
            prop_assert!(l <= h, "Lev={} > Hamming={}", l, h);
        }
    }

    // 16. LCS length <= min(len(a), len(b))
    #[test]
    fn lcs_bounded(a in arb_bytes(30), b in arb_bytes(30)) {
        let l = lcs_length(&a, &b);
        prop_assert!(l <= a.len().min(b.len()));
    }

    // 17. LCS identity = length
    #[test]
    fn lcs_identity(a in arb_bytes(30)) {
        prop_assert_eq!(lcs_length(&a, &a), a.len());
    }

    // 18. LCS symmetry
    #[test]
    fn lcs_symmetry(a in arb_bytes(20), b in arb_bytes(20)) {
        prop_assert_eq!(lcs_length(&a, &b), lcs_length(&b, &a));
    }

    // 19. LCS + edit distance relationship: lev + lcs >= max(m, n)
    // Each LCS char is a "keep"; remaining chars need edits
    #[test]
    fn lcs_lev_relationship(a in arb_bytes(20), b in arb_bytes(20)) {
        let l = lcs_length(&a, &b);
        let d = levenshtein(&a, &b);
        let max_len = a.len().max(b.len());
        prop_assert!(d + l >= max_len, "lev={} + lcs={} < max_len={}", d, l, max_len);
    }

    // 20. Jaro in [0, 1]
    #[test]
    fn jaro_range(a in arb_ascii(20), b in arb_ascii(20)) {
        let j = jaro(&a, &b);
        prop_assert!(j >= 0.0, "jaro={}", j);
        prop_assert!(j <= 1.0 + f64::EPSILON, "jaro={}", j);
    }

    // 21. Jaro identity = 1.0
    #[test]
    fn jaro_identity(a in arb_ascii(20)) {
        prop_assert!((jaro(&a, &a) - 1.0).abs() < f64::EPSILON || a.is_empty());
    }

    // 22. Jaro symmetry
    #[test]
    fn jaro_symmetry(a in arb_ascii(20), b in arb_ascii(20)) {
        let ab = jaro(&a, &b);
        let ba = jaro(&b, &a);
        prop_assert!((ab - ba).abs() < 1e-10, "jaro(a,b)={} != jaro(b,a)={}", ab, ba);
    }

    // 23. Jaro-Winkler >= Jaro
    #[test]
    fn jw_ge_jaro(a in arb_ascii(20), b in arb_ascii(20)) {
        let j = jaro(&a, &b);
        let jw = jaro_winkler(&a, &b);
        prop_assert!(jw >= j - f64::EPSILON, "JW={} < Jaro={}", jw, j);
    }

    // 24. JW in [0, 1]
    #[test]
    fn jw_range(a in arb_ascii(20), b in arb_ascii(20)) {
        let jw = jaro_winkler(&a, &b);
        prop_assert!(jw >= 0.0);
        prop_assert!(jw <= 1.0 + f64::EPSILON);
    }

    // 25. Bounded agrees with unbounded when within bound
    #[test]
    fn bounded_agrees_when_within(a in arb_bytes(15), b in arb_bytes(15)) {
        let d = levenshtein(&a, &b);
        let bounded = levenshtein_bounded(&a, &b, d);
        prop_assert_eq!(bounded, Some(d));
    }

    // 26. Bounded returns None when exceeded
    #[test]
    fn bounded_none_when_exceeded(a in arb_bytes(15), b in arb_bytes(15)) {
        let d = levenshtein(&a, &b);
        if d > 0 {
            let bounded = levenshtein_bounded(&a, &b, d - 1);
            prop_assert_eq!(bounded, None);
        }
    }

    // 27. Weighted with default costs = Levenshtein
    #[test]
    fn weighted_default_matches(a in arb_bytes(15), b in arb_bytes(15)) {
        let d = levenshtein(&a, &b);
        let w = levenshtein_weighted(&a, &b, &EditCosts::default());
        prop_assert!((w - d as f64).abs() < f64::EPSILON,
            "weighted={}, lev={}", w, d);
    }

    // 28. Edit script length = keeps + edits
    #[test]
    fn script_ops_correct(a in arb_bytes(15), b in arb_bytes(15)) {
        let ops = edit_script(&a, &b);
        let edit_count = ops.iter().filter(|op| !matches!(op, EditOp::Keep(_))).count();
        prop_assert_eq!(edit_count, levenshtein(&a, &b));
    }

    // 29. Generic matches byte version
    #[test]
    fn generic_matches_bytes(a in arb_bytes(20), b in arb_bytes(20)) {
        let byte_dist = levenshtein(&a, &b);
        let gen_dist = levenshtein_generic(&a, &b);
        prop_assert_eq!(byte_dist, gen_dist);
    }

    // 30. Nearest neighbors sorted by distance
    #[test]
    fn nn_sorted(reference in arb_bytes(10),
                 targets in proptest::collection::vec(arb_bytes(10), 1..20)) {
        let target_refs: Vec<&[u8]> = targets.iter().map(|t| t.as_slice()).collect();
        let nn = nearest_neighbors(&reference, &target_refs, 10);
        for w in nn.windows(2) {
            prop_assert!(w[0].1 <= w[1].1);
        }
    }

    // 31. N-gram similarity in [0, 1]
    #[test]
    fn ngram_range(a in arb_bytes(20), b in arb_bytes(20), n in 1..5usize) {
        let sim = ngram_similarity(&a, &b, n);
        prop_assert!(sim >= 0.0);
        prop_assert!(sim <= 1.0 + f64::EPSILON);
    }

    // 32. N-gram identity = 1.0 (when long enough)
    #[test]
    fn ngram_identity(a in arb_bytes(20), n in 1..5usize) {
        prop_assume!(a.len() >= n);
        let sim = ngram_similarity(&a, &a, n);
        prop_assert!((sim - 1.0).abs() < f64::EPSILON);
    }

    // 33. N-gram symmetry
    #[test]
    fn ngram_symmetry(a in arb_bytes(15), b in arb_bytes(15), n in 1..4usize) {
        let ab = ngram_similarity(&a, &b, n);
        let ba = ngram_similarity(&b, &a, n);
        prop_assert!((ab - ba).abs() < f64::EPSILON);
    }

    // 34. Single char substitution => distance 1
    #[test]
    fn single_sub_is_one(a in arb_ascii(30), idx in any::<prop::sample::Index>()) {
        prop_assume!(a.len() >= 5);
        let i = idx.index(a.len());
        let mut b = a.clone();
        b[i] = if a[i] == b'z' { b'a' } else { a[i] + 1 };
        prop_assert_eq!(levenshtein(&a, &b), 1);
    }

    // 35. Single insert => distance 1
    #[test]
    fn single_insert_is_one(a in arb_ascii(30), idx in any::<prop::sample::Index>(), ch in b'a'..=b'z') {
        prop_assume!(!a.is_empty());
        let i = idx.index(a.len() + 1);
        let mut b = a.clone();
        b.insert(i, ch);
        prop_assert_eq!(levenshtein(&a, &b), 1);
    }

    // 36. DL normalized in [0, 1]
    #[test]
    fn dl_normalized_range(a in arb_bytes(20), b in arb_bytes(20)) {
        let sim = damerau_levenshtein_normalized(&a, &b);
        prop_assert!(sim >= 0.0);
        prop_assert!(sim <= 1.0 + f64::EPSILON);
    }

    // 37. LCS empty = 0
    #[test]
    fn lcs_empty(a in arb_bytes(20)) {
        prop_assert_eq!(lcs_length(&a, &[]), 0);
    }

    // 38. Levenshtein_str matches levenshtein on bytes
    #[test]
    fn str_matches_bytes(a in arb_string(20), b in arb_string(20)) {
        prop_assert_eq!(
            levenshtein_str(&a, &b),
            levenshtein(a.as_bytes(), b.as_bytes())
        );
    }

    // 39. NN max_results respected
    #[test]
    fn nn_max_results(reference in arb_bytes(5),
                      targets in proptest::collection::vec(arb_bytes(5), 5..20),
                      max_r in 1..5usize) {
        let target_refs: Vec<&[u8]> = targets.iter().map(|t| t.as_slice()).collect();
        let nn = nearest_neighbors(&reference, &target_refs, max_r);
        prop_assert!(nn.len() <= max_r);
    }

    // 40. Edit script keeps + deletes + inserts + subs covers both lengths
    #[test]
    fn script_covers_lengths(a in arb_bytes(15), b in arb_bytes(15)) {
        let ops = edit_script(&a, &b);
        let keeps = ops.iter().filter(|op| matches!(op, EditOp::Keep(_))).count();
        let subs = ops.iter().filter(|op| matches!(op, EditOp::Substitute(_))).count();
        let inserts = ops.iter().filter(|op| matches!(op, EditOp::Insert(_))).count();
        let deletes = ops.iter().filter(|op| matches!(op, EditOp::Delete(_))).count();
        // a consumed by keeps + subs + deletes
        prop_assert_eq!(keeps + subs + deletes, a.len());
        // b constructed by keeps + subs + inserts
        prop_assert_eq!(keeps + subs + inserts, b.len());
    }
}
