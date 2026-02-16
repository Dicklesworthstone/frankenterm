//! Property-based tests for CRDT module.
//!
//! Tests the join-semilattice laws (commutativity, associativity, idempotency)
//! for all CRDT types, plus type-specific invariants.
//!
//! Bead: ft-283h4.24

use frankenterm_core::crdt::*;
use proptest::prelude::*;

// ─── Strategies ──────────────────────────────────────────────────────────

fn replica_id_strategy() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z]{1,8}")
        .unwrap()
        .prop_filter("non-empty", |s| !s.is_empty())
}

fn small_u64() -> impl Strategy<Value = u64> {
    0..10_000u64
}

fn gcounter_strategy() -> impl Strategy<Value = GCounter> {
    (replica_id_strategy(), small_u64()).prop_map(|(id, count)| {
        let mut c = GCounter::new(id);
        c.increment_by(count);
        c
    })
}

fn gcounter_pair_strategy() -> impl Strategy<Value = (GCounter, GCounter)> {
    (
        replica_id_strategy(),
        replica_id_strategy(),
        small_u64(),
        small_u64(),
    )
        .prop_map(|(id1, id2, c1, c2)| {
            let mut g1 = GCounter::new(id1);
            let mut g2 = GCounter::new(id2);
            g1.increment_by(c1);
            g2.increment_by(c2);
            (g1, g2)
        })
}

fn pncounter_strategy() -> impl Strategy<Value = PnCounter> {
    (replica_id_strategy(), small_u64(), small_u64()).prop_map(|(id, inc, dec)| {
        let mut c = PnCounter::new(id);
        c.increment_by(inc);
        c.decrement_by(dec);
        c
    })
}

fn gset_strategy() -> impl Strategy<Value = GSet<String>> {
    prop::collection::vec(replica_id_strategy(), 0..20).prop_map(|items| {
        let mut s = GSet::new();
        for item in items {
            s.insert(item);
        }
        s
    })
}

fn orset_strategy() -> impl Strategy<Value = OrSet<String>> {
    (
        replica_id_strategy(),
        prop::collection::vec(replica_id_strategy(), 0..10),
    )
        .prop_map(|(id, items)| {
            let mut s = OrSet::new(id);
            for item in items {
                s.insert(item);
            }
            s
        })
}

fn lww_strategy() -> impl Strategy<Value = LwwRegister<String>> {
    (replica_id_strategy(), replica_id_strategy(), small_u64())
        .prop_map(|(id, val, ts)| LwwRegister::new(id, val, ts))
}

fn mvregister_strategy() -> impl Strategy<Value = MvRegister<String>> {
    (
        replica_id_strategy(),
        prop::collection::vec(replica_id_strategy(), 0..5),
    )
        .prop_map(|(id, values)| {
            let mut r = MvRegister::new(id);
            for v in values {
                r.set(v);
            }
            r
        })
}

fn version_vector_strategy() -> impl Strategy<Value = VersionVector> {
    prop::collection::vec((replica_id_strategy(), 1..100u64), 0..5).prop_map(|entries| {
        let mut vv = VersionVector::new();
        for (id, count) in entries {
            for _ in 0..count.min(10) {
                vv.increment(&id);
            }
        }
        vv
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// G-Counter Properties
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // --- Semilattice Laws ---

    #[test]
    fn gcounter_merge_commutative((a, b) in gcounter_pair_strategy()) {
        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        prop_assert_eq!(ab.value(), ba.value());
    }

    #[test]
    fn gcounter_merge_associative(
        id1 in replica_id_strategy(),
        id2 in replica_id_strategy(),
        id3 in replica_id_strategy(),
        c1 in small_u64(),
        c2 in small_u64(),
        c3 in small_u64(),
    ) {
        let mut a = GCounter::new(id1);
        let mut b = GCounter::new(id2);
        let mut c = GCounter::new(id3);
        a.increment_by(c1);
        b.increment_by(c2);
        c.increment_by(c3);

        // (a merge b) merge c
        let mut ab_c = a.clone();
        ab_c.merge(&b);
        ab_c.merge(&c);

        // a merge (b merge c)
        let mut bc = b.clone();
        bc.merge(&c);
        let mut a_bc = a.clone();
        a_bc.merge(&bc);

        prop_assert_eq!(ab_c.value(), a_bc.value());
    }

    #[test]
    fn gcounter_merge_idempotent(a in gcounter_strategy()) {
        let mut merged = a.clone();
        merged.merge(&a);
        prop_assert_eq!(merged.value(), a.value());
    }

    // --- Value Invariants ---

    #[test]
    fn gcounter_value_monotone_after_increment(a in gcounter_strategy()) {
        let before = a.value();
        let mut c = a.clone();
        c.increment();
        prop_assert!(c.value() >= before + 1);
    }

    #[test]
    fn gcounter_merge_monotonic((a, b) in gcounter_pair_strategy()) {
        let before = a.value();
        let mut merged = a.clone();
        merged.merge(&b);
        prop_assert!(merged.value() >= before, "merge must not decrease value");
    }

    #[test]
    fn gcounter_serde_roundtrip(a in gcounter_strategy()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: GCounter = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// PN-Counter Properties
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pncounter_merge_commutative(
        id1 in replica_id_strategy(),
        id2 in replica_id_strategy(),
        inc1 in small_u64(),
        dec1 in small_u64(),
        inc2 in small_u64(),
        dec2 in small_u64(),
    ) {
        let mut a = PnCounter::new(id1.clone());
        a.increment_by(inc1);
        a.decrement_by(dec1);

        let mut b = PnCounter::new(id2.clone());
        b.increment_by(inc2);
        b.decrement_by(dec2);

        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        prop_assert_eq!(ab.value(), ba.value());
    }

    #[test]
    fn pncounter_merge_idempotent(a in pncounter_strategy()) {
        let mut merged = a.clone();
        merged.merge(&a);
        prop_assert_eq!(merged.value(), a.value());
    }

    #[test]
    fn pncounter_value_equals_inc_minus_dec(
        id in replica_id_strategy(),
        inc in small_u64(),
        dec in small_u64(),
    ) {
        let mut c = PnCounter::new(id);
        c.increment_by(inc);
        c.decrement_by(dec);
        let expected = i128::from(inc) - i128::from(dec);
        prop_assert_eq!(c.value(), expected);
    }

    #[test]
    fn pncounter_serde_roundtrip(a in pncounter_strategy()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: PnCounter = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// G-Set Properties
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn gset_merge_commutative(a in gset_strategy(), b in gset_strategy()) {
        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        prop_assert_eq!(ab, ba);
    }

    #[test]
    fn gset_merge_associative(
        a in gset_strategy(),
        b in gset_strategy(),
        c in gset_strategy(),
    ) {
        let mut ab_c = a.clone();
        ab_c.merge(&b);
        ab_c.merge(&c);

        let mut bc = b.clone();
        bc.merge(&c);
        let mut a_bc = a.clone();
        a_bc.merge(&bc);

        prop_assert_eq!(ab_c, a_bc);
    }

    #[test]
    fn gset_merge_idempotent(a in gset_strategy()) {
        let mut merged = a.clone();
        merged.merge(&a);
        prop_assert_eq!(merged, a);
    }

    #[test]
    fn gset_merge_superset(a in gset_strategy(), b in gset_strategy()) {
        let mut merged = a.clone();
        merged.merge(&b);
        // merged must contain everything from a
        for item in a.iter() {
            prop_assert!(merged.contains(item));
        }
        // merged must contain everything from b
        for item in b.iter() {
            prop_assert!(merged.contains(item));
        }
    }

    #[test]
    fn gset_merge_monotonic_size(a in gset_strategy(), b in gset_strategy()) {
        let size_a = a.len();
        let mut merged = a.clone();
        merged.merge(&b);
        prop_assert!(merged.len() >= size_a, "merge must not shrink set");
    }

    #[test]
    fn gset_serde_roundtrip(a in gset_strategy()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: GSet<String> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// OR-Set Properties
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    #[test]
    fn orset_insert_always_present(
        id in replica_id_strategy(),
        item in replica_id_strategy(),
    ) {
        let mut s = OrSet::new(id);
        s.insert(item.clone());
        prop_assert!(s.contains(&item));
    }

    #[test]
    fn orset_remove_then_absent(
        id in replica_id_strategy(),
        item in replica_id_strategy(),
    ) {
        let mut s = OrSet::new(id);
        s.insert(item.clone());
        s.remove(&item);
        prop_assert!(!s.contains(&item));
    }

    #[test]
    fn orset_merge_preserves_elements(a in orset_strategy(), b in orset_strategy()) {
        let a_elems: Vec<String> = a.elements().into_iter().cloned().collect();
        let mut merged = a.clone();
        merged.merge(&b);
        // All elements from a should still be in merged
        for item in &a_elems {
            prop_assert!(merged.contains(item), "element from a lost after merge");
        }
    }

    #[test]
    fn orset_serde_roundtrip(a in orset_strategy()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: OrSet<String> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }

    #[test]
    fn orset_len_consistent(a in orset_strategy()) {
        let elems = a.elements();
        prop_assert_eq!(a.len(), elems.len());
        let is_empty = a.is_empty();
        prop_assert_eq!(is_empty, elems.is_empty());
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// LWW-Register Properties
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn lww_merge_commutative(a in lww_strategy(), b in lww_strategy()) {
        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        prop_assert_eq!(ab.get(), ba.get());
        prop_assert_eq!(ab.timestamp(), ba.timestamp());
    }

    #[test]
    fn lww_merge_idempotent(a in lww_strategy()) {
        let before_val = a.get().clone();
        let mut merged = a.clone();
        merged.merge(&a);
        prop_assert_eq!(merged.get(), &before_val);
    }

    #[test]
    fn lww_merge_associative(
        a in lww_strategy(),
        b in lww_strategy(),
        c in lww_strategy(),
    ) {
        let mut ab_c = a.clone();
        ab_c.merge(&b);
        ab_c.merge(&c);

        let mut bc = b.clone();
        bc.merge(&c);
        let mut a_bc = a.clone();
        a_bc.merge(&bc);

        prop_assert_eq!(ab_c.get(), a_bc.get());
    }

    #[test]
    fn lww_higher_timestamp_wins(
        id1 in replica_id_strategy(),
        id2 in replica_id_strategy(),
        val1 in replica_id_strategy(),
        val2 in replica_id_strategy(),
        ts_low in 0..5000u64,
        ts_delta in 1..5000u64,
    ) {
        let ts_high = ts_low + ts_delta;
        let mut low = LwwRegister::new(id1, val1, ts_low);
        let high = LwwRegister::new(id2, val2.clone(), ts_high);
        low.merge(&high);
        prop_assert_eq!(low.get(), &val2);
    }

    #[test]
    fn lww_serde_roundtrip(a in lww_strategy()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: LwwRegister<String> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// MV-Register Properties
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    #[test]
    fn mvregister_single_writer_no_conflict(
        id in replica_id_strategy(),
        values in prop::collection::vec(replica_id_strategy(), 1..5),
    ) {
        let mut r = MvRegister::new(id);
        for v in &values {
            r.set(v.clone());
        }
        // Sequential writes from same replica = no conflict
        prop_assert!(!r.has_conflict());
        prop_assert_eq!(r.conflict_count(), 1);
    }

    #[test]
    fn mvregister_concurrent_creates_conflict(
        id1 in replica_id_strategy(),
        id2 in replica_id_strategy(),
        val1 in replica_id_strategy(),
        val2 in replica_id_strategy(),
    ) {
        // Skip if same replica (can't have concurrent writes from same replica)
        prop_assume!(id1 != id2);

        let mut r1 = MvRegister::new(id1);
        r1.set(val1);

        let mut r2 = MvRegister::new(id2);
        r2.set(val2);

        r1.merge(&r2);
        prop_assert!(r1.has_conflict());
    }

    #[test]
    fn mvregister_causal_write_resolves(
        id1 in replica_id_strategy(),
        val1 in replica_id_strategy(),
        val2 in replica_id_strategy(),
    ) {
        let mut r1 = MvRegister::new(id1);
        r1.set(val1);

        // r2 observes r1's state, then writes
        let mut r2 = r1.clone();
        r2.set(val2.clone());

        // Merge back: r2's write causally dominates r1's
        r1.merge(&r2);
        prop_assert!(!r1.has_conflict());
        prop_assert_eq!(r1.get(), Some(&val2));
    }

    #[test]
    fn mvregister_serde_roundtrip(a in mvregister_strategy()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: MvRegister<String> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Version Vector Properties
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn vv_merge_commutative(a in version_vector_strategy(), b in version_vector_strategy()) {
        let mut ab = a.clone();
        ab.merge(&b);

        let mut ba = b.clone();
        ba.merge(&a);

        prop_assert_eq!(ab, ba);
    }

    #[test]
    fn vv_merge_idempotent(a in version_vector_strategy()) {
        let mut merged = a.clone();
        merged.merge(&a);
        prop_assert_eq!(merged, a);
    }

    #[test]
    fn vv_merge_associative(
        a in version_vector_strategy(),
        b in version_vector_strategy(),
        c in version_vector_strategy(),
    ) {
        let mut ab_c = a.clone();
        ab_c.merge(&b);
        ab_c.merge(&c);

        let mut bc = b.clone();
        bc.merge(&c);
        let mut a_bc = a.clone();
        a_bc.merge(&bc);

        prop_assert_eq!(ab_c, a_bc);
    }

    #[test]
    fn vv_self_not_dominating(a in version_vector_strategy()) {
        prop_assert!(!a.dominates(&a), "a VV must not dominate itself");
    }

    #[test]
    fn vv_increment_increases(id in replica_id_strategy()) {
        let mut vv = VersionVector::new();
        let before = vv.get(&id);
        vv.increment(&id);
        prop_assert_eq!(vv.get(&id), before + 1);
    }

    #[test]
    fn vv_serde_roundtrip(a in version_vector_strategy()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: VersionVector = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Convergence Hash Properties
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn convergence_hash_deterministic_prop(data in prop::collection::vec(any::<u8>(), 0..100)) {
        let h1 = convergence_hash(&data);
        let h2 = convergence_hash(&data);
        prop_assert_eq!(h1, h2);
    }

    #[test]
    fn convergence_hash_empty_deterministic(_dummy in 0..1u32) {
        let h1 = convergence_hash(&[]);
        let h2 = convergence_hash(&[]);
        prop_assert_eq!(h1, h2);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Cross-CRDT Integration Properties
// ═══════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn three_replica_gcounter_convergence_prop(
        c1 in small_u64(),
        c2 in small_u64(),
        c3 in small_u64(),
    ) {
        let mut r1 = GCounter::new("r1");
        let mut r2 = GCounter::new("r2");
        let mut r3 = GCounter::new("r3");
        r1.increment_by(c1);
        r2.increment_by(c2);
        r3.increment_by(c3);

        // All 6 merge orderings should produce same value
        let orders: Vec<Vec<usize>> = vec![
            vec![0, 1, 2],
            vec![0, 2, 1],
            vec![1, 0, 2],
            vec![1, 2, 0],
            vec![2, 0, 1],
            vec![2, 1, 0],
        ];

        let counters = [&r1, &r2, &r3];
        let mut values = Vec::new();

        for order in &orders {
            let mut acc = counters[order[0]].clone();
            acc.merge(counters[order[1]]);
            acc.merge(counters[order[2]]);
            values.push(acc.value());
        }

        let first = values[0];
        for v in &values[1..] {
            prop_assert_eq!(*v, first, "all merge orders must converge");
        }
    }

    #[test]
    fn three_replica_gset_convergence_prop(
        items1 in prop::collection::vec(0..100u32, 0..5),
        items2 in prop::collection::vec(0..100u32, 0..5),
        items3 in prop::collection::vec(0..100u32, 0..5),
    ) {
        let mut s1 = GSet::new();
        let mut s2 = GSet::new();
        let mut s3 = GSet::new();
        for i in items1 { s1.insert(i); }
        for i in items2 { s2.insert(i); }
        for i in items3 { s3.insert(i); }

        // Merge all three in different orders
        let mut a = s1.clone();
        a.merge(&s2);
        a.merge(&s3);

        let mut b = s3.clone();
        b.merge(&s1);
        b.merge(&s2);

        let mut c = s2.clone();
        c.merge(&s3);
        c.merge(&s1);

        prop_assert_eq!(a.len(), b.len());
        prop_assert_eq!(b.len(), c.len());
        // Verify all three converged to same elements
        for item in a.iter() {
            prop_assert!(b.contains(item));
            prop_assert!(c.contains(item));
        }
    }
}
