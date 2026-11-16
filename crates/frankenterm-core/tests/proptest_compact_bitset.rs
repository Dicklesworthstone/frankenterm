//! Property-based tests for `compact_bitset` — fixed-size bitset with set operations.

use proptest::prelude::*;

use frankenterm_core::compact_bitset::CompactBitset;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_capacity() -> impl Strategy<Value = usize> {
    1..500usize
}

fn arb_bitset(max_cap: usize) -> impl Strategy<Value = CompactBitset> {
    (1..=max_cap).prop_flat_map(|cap| {
        proptest::collection::vec(0..cap, 0..cap.min(50))
            .prop_map(move |indices| CompactBitset::from_indices(cap, indices))
    })
}

fn arb_pair(max_cap: usize) -> impl Strategy<Value = (CompactBitset, CompactBitset)> {
    (1..=max_cap).prop_flat_map(|cap| {
        let s1 = proptest::collection::vec(0..cap, 0..cap.min(30))
            .prop_map(move |idx| CompactBitset::from_indices(cap, idx));
        let s2 = proptest::collection::vec(0..cap, 0..cap.min(30))
            .prop_map(move |idx| CompactBitset::from_indices(cap, idx));
        (s1, s2)
    })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. count_ones + count_zeros = capacity
    #[test]
    fn ones_plus_zeros_equals_capacity(bs in arb_bitset(200)) {
        prop_assert_eq!(bs.count_ones() + bs.count_zeros(), bs.capacity());
    }

    // 2. empty bitset has zero ones
    #[test]
    fn new_is_empty(cap in arb_capacity()) {
        let bs = CompactBitset::new(cap);
        prop_assert!(bs.is_empty());
        prop_assert_eq!(bs.count_ones(), 0);
    }

    // 3. full bitset has capacity ones
    #[test]
    fn full_has_capacity_ones(cap in arb_capacity()) {
        let bs = CompactBitset::full(cap);
        prop_assert!(bs.is_full());
        prop_assert_eq!(bs.count_ones(), cap);
    }

    // 4. set then test returns true
    #[test]
    fn set_then_test(cap in 1..500usize, idx_frac in 0.0..1.0f64) {
        let idx = (idx_frac * (cap as f64 - 0.01)) as usize;
        let idx = idx.min(cap - 1);
        let mut bs = CompactBitset::new(cap);
        bs.set(idx);
        prop_assert!(bs.test(idx));
    }

    // 5. clear then test returns false
    #[test]
    fn clear_then_test(cap in 1..500usize, idx_frac in 0.0..1.0f64) {
        let idx = (idx_frac * (cap as f64 - 0.01)) as usize;
        let idx = idx.min(cap - 1);
        let mut bs = CompactBitset::full(cap);
        bs.clear(idx);
        prop_assert!(!bs.test(idx));
    }

    // 6. toggle is self-inverse
    #[test]
    fn toggle_self_inverse(bs in arb_bitset(200), idx_frac in 0.0..1.0f64) {
        let idx = (idx_frac * (bs.capacity() as f64 - 0.01)) as usize;
        let idx = idx.min(bs.capacity() - 1);
        let original = bs.clone();
        let mut toggled = bs;
        toggled.toggle(idx);
        toggled.toggle(idx);
        prop_assert_eq!(original, toggled);
    }

    // 7. complement of complement is identity
    #[test]
    fn complement_involution(bs in arb_bitset(200)) {
        let double_comp = bs.complement().complement();
        prop_assert_eq!(bs, double_comp);
    }

    // 8. union is commutative
    #[test]
    fn union_commutative((a, b) in arb_pair(200)) {
        prop_assert_eq!(a.union(&b), b.union(&a));
    }

    // 9. intersection is commutative
    #[test]
    fn intersection_commutative((a, b) in arb_pair(200)) {
        prop_assert_eq!(a.intersection(&b), b.intersection(&a));
    }

    // 10. symmetric difference is commutative
    #[test]
    fn symmetric_difference_commutative((a, b) in arb_pair(200)) {
        prop_assert_eq!(a.symmetric_difference(&b), b.symmetric_difference(&a));
    }

    // 11. union with self is identity
    #[test]
    fn union_idempotent(bs in arb_bitset(200)) {
        prop_assert_eq!(bs.union(&bs), bs);
    }

    // 12. intersection with self is identity
    #[test]
    fn intersection_idempotent(bs in arb_bitset(200)) {
        prop_assert_eq!(bs.intersection(&bs), bs);
    }

    // 13. difference with self is empty
    #[test]
    fn difference_with_self_empty(bs in arb_bitset(200)) {
        let d = bs.difference(&bs);
        prop_assert!(d.is_empty());
    }

    // 14. symmetric difference with self is empty
    #[test]
    fn symmetric_difference_self_empty(bs in arb_bitset(200)) {
        let sd = bs.symmetric_difference(&bs);
        prop_assert!(sd.is_empty());
    }

    // 15. union with empty is identity
    #[test]
    fn union_with_empty_identity(bs in arb_bitset(200)) {
        let empty = CompactBitset::new(bs.capacity());
        prop_assert_eq!(bs.union(&empty), bs);
    }

    // 16. intersection with empty is empty
    #[test]
    fn intersection_with_empty_is_empty(bs in arb_bitset(200)) {
        let empty = CompactBitset::new(bs.capacity());
        let result = bs.intersection(&empty);
        prop_assert!(result.is_empty());
    }

    // 17. union with full is full
    #[test]
    fn union_with_full(bs in arb_bitset(200)) {
        let full = CompactBitset::full(bs.capacity());
        let result = bs.union(&full);
        prop_assert!(result.is_full());
    }

    // 18. intersection with full is identity
    #[test]
    fn intersection_with_full_identity(bs in arb_bitset(200)) {
        let full = CompactBitset::full(bs.capacity());
        prop_assert_eq!(bs.intersection(&full), bs);
    }

    // 19. |A ∪ B| = |A| + |B| - |A ∩ B|  (inclusion-exclusion)
    #[test]
    fn inclusion_exclusion((a, b) in arb_pair(200)) {
        let union_count = a.union(&b).count_ones();
        let a_count = a.count_ones();
        let b_count = b.count_ones();
        let inter_count = a.intersection(&b).count_ones();
        prop_assert_eq!(union_count, a_count + b_count - inter_count);
    }

    // 20. A \ B is disjoint from B
    #[test]
    fn difference_disjoint_from_rhs((a, b) in arb_pair(200)) {
        let diff = a.difference(&b);
        prop_assert!(diff.is_disjoint(&b));
    }

    // 21. A ∩ B is subset of both A and B
    #[test]
    fn intersection_subset_of_both((a, b) in arb_pair(200)) {
        let inter = a.intersection(&b);
        prop_assert!(inter.is_subset_of(&a));
        prop_assert!(inter.is_subset_of(&b));
    }

    // 22. A is subset of A ∪ B
    #[test]
    fn a_subset_of_union((a, b) in arb_pair(200)) {
        let u = a.union(&b);
        prop_assert!(a.is_subset_of(&u));
        prop_assert!(b.is_subset_of(&u));
    }

    // 23. De Morgan: ¬(A ∪ B) = ¬A ∩ ¬B
    #[test]
    fn de_morgan_union((a, b) in arb_pair(200)) {
        let lhs = a.union(&b).complement();
        let rhs = a.complement().intersection(&b.complement());
        prop_assert_eq!(lhs, rhs);
    }

    // 24. De Morgan: ¬(A ∩ B) = ¬A ∪ ¬B
    #[test]
    fn de_morgan_intersection((a, b) in arb_pair(200)) {
        let lhs = a.intersection(&b).complement();
        let rhs = a.complement().union(&b.complement());
        prop_assert_eq!(lhs, rhs);
    }

    // 25. iter_ones collects all and only set bits
    #[test]
    fn iter_ones_matches_test(bs in arb_bitset(200)) {
        let ones: Vec<usize> = bs.iter_ones().collect();
        prop_assert_eq!(ones.len(), bs.count_ones());
        for &idx in &ones {
            prop_assert!(bs.test(idx));
        }
        // monotonically increasing
        for w in ones.windows(2) {
            prop_assert!(w[0] < w[1]);
        }
    }

    // 26. to_vec/from_indices roundtrip
    #[test]
    fn to_vec_from_indices_roundtrip(bs in arb_bitset(200)) {
        let cap = bs.capacity();
        let indices = bs.to_vec();
        let rebuilt = CompactBitset::from_indices(cap, indices);
        prop_assert_eq!(bs, rebuilt);
    }

    // 27. serde roundtrip
    #[test]
    fn serde_roundtrip(bs in arb_bitset(200)) {
        let json = serde_json::to_string(&bs).unwrap();
        let back: CompactBitset = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(bs, back);
    }

    // 28. in-place operations match functional equivalents
    #[test]
    fn in_place_matches_functional((a, b) in arb_pair(200)) {
        let mut u = a.clone();
        u.union_with(&b);
        prop_assert_eq!(u, a.union(&b));

        let mut i = a.clone();
        i.intersection_with(&b);
        prop_assert_eq!(i, a.intersection(&b));

        let mut d = a.clone();
        d.difference_with(&b);
        prop_assert_eq!(d, a.difference(&b));

        let mut sd = a.clone();
        sd.symmetric_difference_with(&b);
        prop_assert_eq!(sd, a.symmetric_difference(&b));
    }

    // 29. operator overloads match named methods
    #[test]
    fn operators_match_methods((a, b) in arb_pair(200)) {
        prop_assert_eq!(&a | &b, a.union(&b));
        prop_assert_eq!(&a & &b, a.intersection(&b));
        prop_assert_eq!(&a ^ &b, a.symmetric_difference(&b));
        prop_assert_eq!(!&a, a.complement());
    }

    // 30. first_set returns minimum set bit
    #[test]
    fn first_set_is_minimum(bs in arb_bitset(200)) {
        match bs.first_set() {
            None => prop_assert!(bs.is_empty()),
            Some(idx) => {
                prop_assert!(bs.test(idx));
                for i in 0..idx {
                    prop_assert!(!bs.test(i));
                }
            }
        }
    }

    // 31. first_clear returns minimum clear bit
    #[test]
    fn first_clear_is_minimum(bs in arb_bitset(200)) {
        match bs.first_clear() {
            None => prop_assert!(bs.is_full()),
            Some(idx) => {
                prop_assert!(!bs.test(idx));
                for i in 0..idx {
                    prop_assert!(bs.test(i));
                }
            }
        }
    }

    // 32. last_set returns maximum set bit
    #[test]
    fn last_set_is_maximum(bs in arb_bitset(200)) {
        match bs.last_set() {
            None => prop_assert!(bs.is_empty()),
            Some(idx) => {
                prop_assert!(bs.test(idx));
                for i in (idx + 1)..bs.capacity() {
                    prop_assert!(!bs.test(i));
                }
            }
        }
    }

    // 33. set_range sets exactly those bits
    #[test]
    fn set_range_correct(cap in 2..200usize, lo_frac in 0.0..0.5f64, span in 1..50usize) {
        let lo = (lo_frac * (cap as f64 - 1.0)) as usize;
        let lo = lo.min(cap - 2);
        let hi = (lo + span).min(cap - 1);
        let mut bs = CompactBitset::new(cap);
        bs.set_range(lo, hi);
        for i in 0..cap {
            let expected = i >= lo && i <= hi;
            prop_assert_eq!(bs.test(i), expected, "bit {}", i);
        }
    }

    // 34. clear_all produces empty
    #[test]
    fn clear_all_makes_empty(bs in arb_bitset(200)) {
        let mut cleared = bs;
        cleared.clear_all();
        prop_assert!(cleared.is_empty());
    }

    // 35. set_all produces full
    #[test]
    fn set_all_makes_full(cap in arb_capacity()) {
        let mut bs = CompactBitset::new(cap);
        bs.set_all();
        prop_assert!(bs.is_full());
        prop_assert_eq!(bs.count_ones(), cap);
    }

    // 36. A ⊕ B = (A \ B) ∪ (B \ A)
    #[test]
    fn symmetric_diff_equals_union_of_differences((a, b) in arb_pair(200)) {
        let lhs = a.symmetric_difference(&b);
        let rhs = a.difference(&b).union(&b.difference(&a));
        prop_assert_eq!(lhs, rhs);
    }

    // 37. |A ⊕ B| = |A| + |B| - 2|A ∩ B|
    #[test]
    fn symmetric_diff_count((a, b) in arb_pair(200)) {
        let sd_count = a.symmetric_difference(&b).count_ones();
        let expected = a.count_ones() + b.count_ones() - 2 * a.intersection(&b).count_ones();
        prop_assert_eq!(sd_count, expected);
    }

    // 38. subset transitivity: A ⊆ B, B ⊆ C ⟹ A ⊆ C
    #[test]
    fn subset_transitive(cap in 1..100usize,
        a_idx in proptest::collection::vec(0..100usize, 0..10),
        b_idx in proptest::collection::vec(0..100usize, 0..20),
        c_idx in proptest::collection::vec(0..100usize, 0..30),
    ) {
        let cap = cap.min(100);
        let a_idx: Vec<usize> = a_idx.into_iter().filter(|&i| i < cap).collect();
        let b_idx: Vec<usize> = b_idx.into_iter().filter(|&i| i < cap).collect();
        let c_idx: Vec<usize> = c_idx.into_iter().filter(|&i| i < cap).collect();

        let a = CompactBitset::from_indices(cap, a_idx);
        let b_base = CompactBitset::from_indices(cap, b_idx);
        let c_base = CompactBitset::from_indices(cap, c_idx);

        // Force A ⊆ B ⊆ C
        let b = a.union(&b_base);
        let c = b.union(&c_base);

        prop_assert!(a.is_subset_of(&b));
        prop_assert!(b.is_subset_of(&c));
        prop_assert!(a.is_subset_of(&c));
    }

    // 39. clone equality
    #[test]
    fn clone_eq(bs in arb_bitset(200)) {
        let cloned = bs.clone();
        prop_assert_eq!(bs, cloned);
    }

    // 40. complement has count_ones = capacity - original count_ones
    #[test]
    fn complement_count(bs in arb_bitset(200)) {
        let comp = bs.complement();
        prop_assert_eq!(comp.count_ones(), bs.capacity() - bs.count_ones());
    }
}
