//! Property-based tests for cancellation_safe_channel.rs.
//!
//! Covers serde roundtrips for TxChannelMetrics and ChannelMetadata,
//! error Display consistency, Reservation seq/commit/rollback semantics,
//! FIFO ordering, capacity invariants, no-loss guarantees with mixed
//! commit/rollback, TxChannelRegistry operations, and ReserveGuard
//! commit/rollback/drop semantics.

use std::collections::BTreeSet;

use frankenterm_core::cancellation_safe_channel::{
    tx_channel, ChannelMetadata, ReserveGuard, TxChannelError, TxChannelMetrics,
    TxChannelRegistry,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_metrics() -> impl Strategy<Value = TxChannelMetrics> {
    (
        1..=1000u64,
        1..=256usize,
        0..=256usize,
        0..=64u64,
        0..=256usize,
        any::<bool>(),
    )
        .prop_map(
            |(channel_id, capacity, queued, active_reservations, available, closed)| {
                TxChannelMetrics {
                    channel_id,
                    capacity,
                    queued,
                    active_reservations,
                    available,
                    closed,
                }
            },
        )
}

fn arb_channel_metadata() -> impl Strategy<Value = ChannelMetadata> {
    (
        1..=1000u64,
        "[a-z_]{3,12}",
        1..=1024usize,
        0..=1_000_000i64,
        "[a-z ]{5,30}",
    )
        .prop_map(|(channel_id, name, capacity, created_at_ms, purpose)| ChannelMetadata {
            channel_id,
            name,
            capacity,
            created_at_ms,
            purpose,
        })
}

fn arb_error() -> impl Strategy<Value = TxChannelError> {
    prop_oneof![
        Just(TxChannelError::Closed),
        Just(TxChannelError::Full),
        (1..=100u64, 1..=100u64)
            .prop_map(|(expected, actual)| TxChannelError::WrongChannel { expected, actual }),
        (1..=1000u64).prop_map(|seq| TxChannelError::AlreadyCommitted { seq }),
    ]
}

// ── TxChannelMetrics serde ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 1. TxChannelMetrics serde roundtrip
    #[test]
    fn metrics_serde_roundtrip(m in arb_metrics()) {
        let json = serde_json::to_string(&m).unwrap();
        let restored: TxChannelMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, m);
    }

    // 2. Metrics debug output contains key fields
    #[test]
    fn metrics_debug_contains_fields(m in arb_metrics()) {
        let debug = format!("{:?}", m);
        prop_assert!(debug.contains("channel_id"));
        prop_assert!(debug.contains("capacity"));
        prop_assert!(debug.contains("queued"));
    }
}

// ── ChannelMetadata serde ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 3. ChannelMetadata serde roundtrip
    #[test]
    fn channel_metadata_serde_roundtrip(meta in arb_channel_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let restored: ChannelMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.channel_id, meta.channel_id);
        prop_assert_eq!(&restored.name, &meta.name);
        prop_assert_eq!(restored.capacity, meta.capacity);
        prop_assert_eq!(restored.created_at_ms, meta.created_at_ms);
        prop_assert_eq!(&restored.purpose, &meta.purpose);
    }
}

// ── TxChannelError ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 4. Error Display is non-empty
    #[test]
    fn error_display_non_empty(err in arb_error()) {
        let display = err.to_string();
        prop_assert!(!display.is_empty());
    }

    // 5. Error equality is reflexive
    #[test]
    fn error_equality_reflexive(err in arb_error()) {
        prop_assert_eq!(&err, &err);
    }

    // 6. WrongChannel display contains both channel IDs
    #[test]
    fn wrong_channel_display_contains_ids(expected in 1..=100u64, actual in 1..=100u64) {
        let err = TxChannelError::WrongChannel { expected, actual };
        let display = err.to_string();
        prop_assert!(
            display.contains(&actual.to_string()),
            "display '{}' should contain actual '{}'", display, actual
        );
        prop_assert!(
            display.contains(&expected.to_string()),
            "display '{}' should contain expected '{}'", display, expected
        );
    }

    // 7. AlreadyCommitted display contains the seq
    #[test]
    fn already_committed_display_contains_seq(seq in 1..=1000u64) {
        let err = TxChannelError::AlreadyCommitted { seq };
        let display = err.to_string();
        prop_assert!(
            display.contains(&seq.to_string()),
            "display '{}' should contain seq '{}'", display, seq
        );
    }
}

// ── Channel FIFO ordering ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 8. Items are received in FIFO order via try_send/try_recv
    #[test]
    fn channel_fifo_ordering(items in prop::collection::vec(any::<u32>(), 1..=64)) {
        let (tx, rx) = tx_channel::<u32>(128);
        for &item in &items {
            tx.try_send(item).unwrap();
        }
        let mut received = Vec::with_capacity(items.len());
        for _ in 0..items.len() {
            received.push(rx.try_recv().unwrap().value);
        }
        prop_assert_eq!(received, items);
    }

    // 9. Sequence numbers are strictly monotonic
    #[test]
    fn sequence_numbers_monotonic(n in 2..=32usize) {
        let (tx, rx) = tx_channel::<u32>(64);
        for i in 0..(n as u32) {
            tx.try_send(i).unwrap();
        }
        let mut prev_seq = 0u64;
        for _ in 0..n {
            let rv = rx.try_recv().unwrap();
            prop_assert!(rv.seq > prev_seq, "seq {} should be > {}", rv.seq, prev_seq);
            prev_seq = rv.seq;
        }
    }
}

// ── Capacity invariants ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 10. Cannot exceed capacity with try_send
    #[test]
    fn capacity_enforcement_try_send(cap in 1..=32usize) {
        let (tx, _rx) = tx_channel::<u32>(cap);
        for i in 0..cap {
            prop_assert!(tx.try_send(i as u32).is_ok());
        }
        let result = tx.try_send(999);
        prop_assert!(matches!(result, Err(TxChannelError::Full)));
    }

    // 11. Cannot exceed capacity with try_reserve
    #[test]
    fn capacity_enforcement_try_reserve(cap in 1..=16usize) {
        let (tx, _rx) = tx_channel::<u32>(cap);
        let mut reservations = Vec::new();
        for _ in 0..cap {
            let r = tx.try_reserve();
            prop_assert!(r.is_ok());
            reservations.push(r.unwrap());
        }
        // Next should fail
        prop_assert!(matches!(tx.try_reserve(), Err(TxChannelError::Full)));
    }

    // 12. available() = capacity - queued - active_reservations
    #[test]
    fn available_capacity_formula(cap in 2..=16usize, n_send in 0..=4usize, n_reserve in 0..=4usize) {
        let total_use = n_send + n_reserve;
        if total_use > cap {
            return Ok(()); // skip invalid combos
        }
        let (tx, _rx) = tx_channel::<u32>(cap);
        for i in 0..(n_send as u32) {
            tx.try_send(i).unwrap();
        }
        let mut _reservations = Vec::new();
        for _ in 0..n_reserve {
            _reservations.push(tx.try_reserve().unwrap());
        }
        let m = tx.metrics();
        prop_assert_eq!(
            m.available,
            cap - m.queued - m.active_reservations as usize,
            "available should be capacity - queued - active_reservations"
        );
    }
}

// ── Reservation semantics ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 13. Reserve then commit delivers the value
    #[test]
    fn reserve_commit_delivers(val in any::<u32>()) {
        let (tx, rx) = tx_channel::<u32>(8);
        let r = tx.try_reserve().unwrap();
        prop_assert!(!r.is_committed());
        prop_assert!(r.seq() >= 1);
        tx.commit(&r, val).unwrap();
        prop_assert!(r.is_committed());
        let rv = rx.try_recv().unwrap();
        prop_assert_eq!(rv.value, val);
    }

    // 14. Reserve then drop frees capacity without delivering
    #[test]
    fn reserve_drop_frees_capacity(cap in 1..=8usize) {
        let (tx, rx) = tx_channel::<u32>(cap);
        {
            let _r = tx.try_reserve().unwrap();
            prop_assert_eq!(tx.active_reservations(), 1);
            // drops here
        }
        prop_assert_eq!(tx.active_reservations(), 0);
        prop_assert_eq!(tx.queued(), 0);
        prop_assert!(rx.try_recv().is_none());
    }

    // 15. Double commit returns AlreadyCommitted
    #[test]
    fn double_commit_rejected(val in any::<u32>()) {
        let (tx, _rx) = tx_channel::<u32>(8);
        let r = tx.try_reserve().unwrap();
        tx.commit(&r, val).unwrap();
        let result = tx.commit(&r, val + 1);
        let is_already_committed = matches!(result, Err(TxChannelError::AlreadyCommitted { .. }));
        prop_assert!(is_already_committed);
    }

    // 16. Cross-channel commit returns WrongChannel
    #[test]
    fn cross_channel_commit_rejected(val in any::<u32>()) {
        let (tx1, _rx1) = tx_channel::<u32>(8);
        let (tx2, _rx2) = tx_channel::<u32>(8);
        let r = tx2.try_reserve().unwrap();
        let result = tx1.commit(&r, val);
        let is_wrong_channel = matches!(result, Err(TxChannelError::WrongChannel { .. }));
        prop_assert!(is_wrong_channel);
    }
}

// ── Close behavior ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 17. Close allows draining existing items, then returns None
    #[test]
    fn close_drains_then_none(items in prop::collection::vec(any::<u32>(), 1..=16)) {
        let (tx, rx) = tx_channel::<u32>(32);
        for &item in &items {
            tx.try_send(item).unwrap();
        }
        tx.close();
        prop_assert!(tx.is_closed());
        prop_assert!(rx.is_closed());

        let mut received = Vec::new();
        while let Some(rv) = rx.try_recv() {
            received.push(rv.value);
        }
        prop_assert_eq!(received, items);
    }

    // 18. Send to closed channel returns Closed
    #[test]
    fn send_to_closed_returns_closed(val in any::<u32>()) {
        let (tx, _rx) = tx_channel::<u32>(8);
        tx.close();
        let result = tx.try_send(val);
        prop_assert!(matches!(result, Err(TxChannelError::Closed)));
    }

    // 19. Reserve on closed channel returns Closed
    #[test]
    fn reserve_on_closed_returns_closed(_seed in 0..10u32) {
        let (tx, _rx) = tx_channel::<u32>(8);
        tx.close();
        let result = tx.try_reserve();
        prop_assert!(matches!(result, Err(TxChannelError::Closed)));
    }
}

// ── No-loss with mixed operations ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 20. All committed items are received, no duplicates, no loss
    #[test]
    fn no_loss_no_duplicates(n in 4..=16usize, drop_indices in prop::collection::btree_set(0..16usize, 0..=4)) {
        let (tx, rx) = tx_channel::<u32>(32);

        let mut committed_values = Vec::new();
        for i in 0..(n as u32) {
            let r = tx.try_reserve().unwrap();
            if drop_indices.contains(&(i as usize)) {
                drop(r); // rollback, value never sent
            } else {
                tx.commit(&r, i).unwrap();
                committed_values.push(i);
            }
        }

        let mut received = Vec::new();
        while let Some(rv) = rx.try_recv() {
            received.push(rv.value);
        }
        prop_assert_eq!(received, committed_values, "only committed values should be received");
    }

    // 21. Unique seq numbers across all committed items
    #[test]
    fn unique_seq_numbers(n in 2..=16usize) {
        let (tx, rx) = tx_channel::<u32>(32);
        for i in 0..(n as u32) {
            tx.try_send(i).unwrap();
        }
        let mut seqs = BTreeSet::new();
        while let Some(rv) = rx.try_recv() {
            let is_new = seqs.insert(rv.seq);
            prop_assert!(is_new, "duplicate seq {} found", rv.seq);
        }
        prop_assert_eq!(seqs.len(), n, "should have {} unique seqs", n);
    }
}

// ── ReserveGuard ────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 22. ReserveGuard commit delivers value and returns seq
    #[test]
    fn guard_commit_delivers(val in any::<u32>()) {
        let (tx, rx) = tx_channel::<u32>(8);
        let r = tx.try_reserve().unwrap();
        let guard = ReserveGuard::new(&tx, r, val);
        prop_assert!(!guard.is_committed());
        let seq = guard.commit().unwrap();
        prop_assert!(seq >= 1);

        let rv = rx.try_recv().unwrap();
        prop_assert_eq!(rv.value, val);
        prop_assert_eq!(rv.seq, seq);
    }

    // 23. ReserveGuard rollback returns value, nothing in channel
    #[test]
    fn guard_rollback_returns_value(val in any::<u32>()) {
        let (tx, rx) = tx_channel::<u32>(8);
        let r = tx.try_reserve().unwrap();
        let guard = ReserveGuard::new(&tx, r, val);
        let returned = guard.rollback();
        prop_assert_eq!(returned, val);
        prop_assert!(rx.try_recv().is_none());
        prop_assert_eq!(tx.active_reservations(), 0);
    }

    // 24. ReserveGuard drop without commit → auto-rollback, capacity freed
    #[test]
    fn guard_drop_frees_capacity(val in any::<u32>()) {
        let (tx, rx) = tx_channel::<u32>(8);
        {
            let r = tx.try_reserve().unwrap();
            let _guard = ReserveGuard::new(&tx, r, val);
            prop_assert_eq!(tx.active_reservations(), 1);
        }
        prop_assert_eq!(tx.active_reservations(), 0);
        prop_assert!(rx.try_recv().is_none());
    }
}

// ── TxChannelRegistry ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 25. Registry by_name and by_id lookups are consistent
    #[test]
    fn registry_lookup_consistency(
        id in 1..=1000u64,
        name in "[a-z_]{3,12}",
        cap in 1..=1024usize,
        ts in 0..=1_000_000i64,
        purpose in "[a-z ]{5,20}",
    ) {
        let mut reg = TxChannelRegistry::new();
        reg.register(id, &name, cap, ts, &purpose);

        let by_name = reg.by_name(&name);
        prop_assert!(by_name.is_some());
        prop_assert_eq!(by_name.unwrap().channel_id, id);

        let by_id = reg.by_id(id);
        prop_assert!(by_id.is_some());
        prop_assert_eq!(&by_id.unwrap().name, &name);
    }

    // 26. Registry len tracks registrations
    #[test]
    fn registry_len_tracks(
        entries in prop::collection::vec((1..=1000u64, "[a-z]{3,8}"), 1..=8),
    ) {
        let mut reg = TxChannelRegistry::new();
        prop_assert!(reg.is_empty());

        // Deduplicate by both id and name
        let mut seen_ids = BTreeSet::new();
        let mut seen_names = BTreeSet::new();
        let mut unique_count = 0;
        for (id, name) in &entries {
            if seen_ids.insert(*id) && seen_names.insert(name.clone()) {
                reg.register(*id, name.as_str(), 8, 0, "test");
                unique_count += 1;
            }
        }
        prop_assert_eq!(reg.len(), unique_count);
    }

    // 27. Registry canonical_string is deterministic
    #[test]
    fn registry_canonical_deterministic(
        entries in prop::collection::vec((1..=100u64, "[a-z]{3,6}"), 1..=4),
    ) {
        let mut reg = TxChannelRegistry::new();
        let mut seen_ids = BTreeSet::new();
        let mut seen_names = BTreeSet::new();
        for (id, name) in &entries {
            if seen_ids.insert(*id) && seen_names.insert(name.clone()) {
                reg.register(*id, name.as_str(), 8, 0, "test");
            }
        }
        let s1 = reg.canonical_string();
        let s2 = reg.canonical_string();
        prop_assert_eq!(s1, s2, "canonical_string must be deterministic");
    }

    // 28. Registry canonical_string names are sorted
    #[test]
    fn registry_canonical_sorted(_seed in 0..10u32) {
        let mut reg = TxChannelRegistry::new();
        reg.register(2, "beta", 8, 0, "");
        reg.register(1, "alpha", 8, 0, "");
        reg.register(3, "gamma", 8, 0, "");
        let s = reg.canonical_string();
        prop_assert!(
            s.contains("alpha,beta,gamma"),
            "canonical string should have sorted names: {}", s
        );
    }
}

// ── Producer/Consumer len/empty/closed ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 29. queued() and is_empty()/len() are consistent
    #[test]
    fn queued_consistency(n in 0..=16usize) {
        let (tx, rx) = tx_channel::<u32>(32);
        for i in 0..(n as u32) {
            tx.try_send(i).unwrap();
        }
        prop_assert_eq!(tx.queued(), n);
        prop_assert_eq!(rx.len(), n);
        prop_assert_eq!(rx.is_empty(), n == 0);
    }

    // 30. channel_id matches between producer, consumer, and reservations
    #[test]
    fn channel_id_consistent(_seed in 0..10u32) {
        let (tx, rx) = tx_channel::<u32>(8);
        let tx_id = tx.channel_id();
        let rx_id = rx.channel_id();
        prop_assert_eq!(tx_id, rx_id);

        let r = tx.try_reserve().unwrap();
        prop_assert_eq!(r.channel_id(), tx_id);
    }
}
