//! Property-based tests for cancellation-safe channel with reserve/commit semantics.
//!
//! Covers:
//! - Reserve/commit/rollback invariants
//! - Capacity accounting consistency
//! - Sequence number monotonicity
//! - No message loss on commit path
//! - Auto-rollback on drop guarantees
//! - Serde roundtrip for metrics
//! - Registry operations
//! - Stress patterns (many reserves, selective commits)

use frankenterm_core::cancellation_safe_channel::{
    ReserveGuard, TxChannelError, TxChannelMetrics, TxChannelRegistry, tx_channel,
};
use proptest::prelude::*;

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn capacity_invariant_maintained(
        capacity in 1usize..32,
        n_ops in 1usize..100,
        ops in proptest::collection::vec(prop_oneof![Just(0u8), Just(1), Just(2)], 1..100)
    ) {
        let (tx, rx) = tx_channel::<u32>(capacity);
        let mut reservations = Vec::new();
        let mut committed_count = 0u32;
        let mut received_count = 0u32;

        for op in ops.iter().take(n_ops) {
            match op {
                0 => {
                    // Try reserve
                    if let Ok(r) = tx.try_reserve() {
                        reservations.push(r);
                    }
                }
                1 => {
                    // Try commit first reservation
                    if let Some(r) = reservations.first() {
                        if !r.is_committed() {
                            let val = committed_count;
                            if tx.commit(r, val).is_ok() {
                                committed_count += 1;
                            }
                        }
                        // Remove from our tracking (committed or will auto-rollback)
                        reservations.remove(0);
                    }
                }
                2 => {
                    // Try receive
                    if let Some(_rv) = rx.try_recv() {
                        received_count += 1;
                    }
                }
                _ => {}
            }

            // Invariant: queued + active_reservations <= capacity
            let queued = tx.queued();
            let active = tx.active_reservations() as usize;
            prop_assert!(
                queued + active <= capacity,
                "queued {} + active {} > capacity {}",
                queued,
                active,
                capacity,
            );
        }

        // After dropping all remaining reservations
        drop(reservations);
        prop_assert_eq!(tx.active_reservations(), 0);

        // Drain remaining
        while rx.try_recv().is_some() {
            received_count += 1;
        }

        // Everything committed was eventually received
        prop_assert_eq!(
            received_count, committed_count,
            "received {} != committed {}",
            received_count, committed_count,
        );
    }

    #[test]
    fn sequence_numbers_always_monotonic(n_sends in 1usize..50) {
        let (tx, rx) = tx_channel::<u32>(64);

        for i in 0..n_sends {
            tx.try_send(i as u32).unwrap();
        }

        let mut prev_seq = 0u64;
        let mut count = 0;
        while let Some(rv) = rx.try_recv() {
            prop_assert!(rv.seq > prev_seq, "seq {} should be > {}", rv.seq, prev_seq);
            prev_seq = rv.seq;
            count += 1;
        }
        prop_assert_eq!(count, n_sends);
    }

    #[test]
    fn rollback_frees_capacity(
        capacity in 1usize..16,
        n_reserve in 0usize..32,
    ) {
        let (tx, _rx) = tx_channel::<u32>(capacity);
        let mut reservations = Vec::new();
        let mut reserved = 0;

        for _ in 0..n_reserve {
            match tx.try_reserve() {
                Ok(r) => {
                    reservations.push(r);
                    reserved += 1;
                }
                Err(TxChannelError::Full) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }

        prop_assert!(reserved <= capacity);
        prop_assert_eq!(tx.active_reservations() as usize, reserved);

        // Drop all reservations
        drop(reservations);
        prop_assert_eq!(tx.active_reservations(), 0);
        prop_assert_eq!(tx.available(), capacity);
    }

    #[test]
    fn selective_commit_rollback(
        capacity in 2usize..16,
        commit_mask in proptest::collection::vec(any::<bool>(), 1..16),
    ) {
        let (tx, rx) = tx_channel::<u32>(capacity);
        let mut reservations = Vec::new();

        // Reserve as many as we can
        for _ in 0..commit_mask.len() {
            match tx.try_reserve() {
                Ok(r) => reservations.push(r),
                Err(_) => break,
            }
        }

        let n_reserved = reservations.len();
        let mut committed_values = Vec::new();

        // Selectively commit based on mask
        for (i, r) in reservations.iter().enumerate() {
            if i < commit_mask.len() && commit_mask[i] {
                if tx.commit(r, i as u32).is_ok() {
                    committed_values.push(i as u32);
                }
            }
        }

        // Drop remaining (uncommitted) reservations
        drop(reservations);
        prop_assert_eq!(tx.active_reservations(), 0);

        // Receive should get exactly the committed values in order
        let mut received = Vec::new();
        while let Some(rv) = rx.try_recv() {
            received.push(rv.value);
        }

        let recv_dbg = format!("{:?}", received);
        let commit_dbg = format!("{:?}", committed_values);
        prop_assert_eq!(
            received, committed_values,
            "received {} != committed {} (reserved {})",
            recv_dbg, commit_dbg, n_reserved,
        );
    }

    #[test]
    fn metrics_capacity_consistent(
        capacity in 1usize..32,
        n_sends in 0usize..16,
    ) {
        let (tx, rx) = tx_channel::<u32>(capacity);

        for i in 0..n_sends.min(capacity) {
            tx.try_send(i as u32).unwrap();
        }

        let m = tx.metrics();
        prop_assert_eq!(m.capacity, capacity);
        prop_assert!(m.queued <= capacity);
        prop_assert_eq!(m.available + m.queued + m.active_reservations as usize, capacity);
        prop_assert!(!m.closed);

        // Consume some
        let consumed = n_sends.min(capacity) / 2;
        for _ in 0..consumed {
            rx.try_recv();
        }

        let m2 = tx.metrics();
        prop_assert_eq!(m2.available + m2.queued + m2.active_reservations as usize, capacity);
    }

    #[test]
    fn close_prevents_reserve(capacity in 1usize..16) {
        let (tx, _rx) = tx_channel::<u32>(capacity);
        tx.close();

        let result = tx.try_reserve();
        prop_assert!(matches!(result, Err(TxChannelError::Closed)));
        prop_assert!(tx.is_closed());
    }

    #[test]
    fn consumer_drain_after_close(n_sends in 0usize..20) {
        let capacity = n_sends.max(1);
        let (tx, rx) = tx_channel::<u32>(capacity);

        let mut sent = 0;
        for i in 0..n_sends {
            if tx.try_send(i as u32).is_ok() {
                sent += 1;
            }
        }

        tx.close();

        let mut received = 0;
        while rx.try_recv().is_some() {
            received += 1;
        }

        prop_assert_eq!(received, sent);
    }

    #[test]
    fn reserve_guard_commit_delivers(n in 1usize..10) {
        let capacity = n.max(1);
        let (tx, rx) = tx_channel::<String>(capacity);

        for i in 0..n.min(capacity) {
            let r = tx.try_reserve().unwrap();
            let guard = ReserveGuard::new(&tx, r, format!("val_{i}"));
            guard.commit().unwrap();
        }

        let mut received = Vec::new();
        while let Some(rv) = rx.try_recv() {
            received.push(rv.value);
        }

        let expected_count = n.min(capacity);
        prop_assert_eq!(received.len(), expected_count);
    }

    #[test]
    fn reserve_guard_drop_is_rollback(n in 1usize..10) {
        let capacity = n.max(1);
        let (tx, rx) = tx_channel::<String>(capacity);

        for i in 0..n.min(capacity) {
            let r = tx.try_reserve().unwrap();
            let _guard = ReserveGuard::new(&tx, r, format!("val_{i}"));
            // guard drops without commit
        }

        prop_assert_eq!(tx.active_reservations(), 0);
        prop_assert!(rx.try_recv().is_none());
    }

    #[test]
    fn serde_roundtrip_metrics(
        capacity in 1usize..1000,
        queued in 0usize..100,
        active in 0u64..50,
    ) {
        let available = capacity.saturating_sub(queued + active as usize);
        let m = TxChannelMetrics {
            channel_id: 42,
            capacity,
            queued,
            active_reservations: active,
            available,
            closed: false,
        };
        let json = serde_json::to_string(&m).unwrap();
        let restored: TxChannelMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(m, restored);
    }
}

// ── Non-proptest structural tests ──────────────────────────────────────────

#[test]
fn no_message_loss_reserve_commit_cycle() {
    let (tx, rx) = tx_channel::<u32>(16);

    // Reserve 10 slots
    let mut reservations = Vec::new();
    for _ in 0..10 {
        reservations.push(tx.try_reserve().unwrap());
    }

    // Commit all with known values
    for (i, r) in reservations.iter().enumerate() {
        tx.commit(r, (i * 10) as u32).unwrap();
    }
    drop(reservations);

    // Verify all 10 received in order
    let mut values = Vec::new();
    while let Some(rv) = rx.try_recv() {
        values.push(rv.value);
    }
    assert_eq!(values, vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 90]);
}

#[test]
fn interleaved_reserve_commit_recv() {
    let (tx, rx) = tx_channel::<&str>(4);

    let r1 = tx.try_reserve().unwrap();
    let r2 = tx.try_reserve().unwrap();

    tx.commit(&r1, "first").unwrap();
    let v1 = rx.try_recv().unwrap();
    assert_eq!(v1.value, "first");

    tx.commit(&r2, "second").unwrap();
    let v2 = rx.try_recv().unwrap();
    assert_eq!(v2.value, "second");
}

#[test]
fn registry_deterministic_ordering() {
    let mut reg = TxChannelRegistry::new();
    reg.register(3, "charlie", 8, 1000, "c");
    reg.register(1, "alpha", 8, 1000, "a");
    reg.register(2, "bravo", 8, 1000, "b");

    let s = reg.canonical_string();
    assert!(s.contains("alpha,bravo,charlie"));
}

#[test]
fn capacity_one_channel() {
    let (tx, rx) = tx_channel::<u32>(1);

    let r = tx.try_reserve().unwrap();
    assert!(matches!(tx.try_reserve(), Err(TxChannelError::Full)));

    tx.commit(&r, 42).unwrap();
    assert!(matches!(tx.try_reserve(), Err(TxChannelError::Full)));

    rx.try_recv().unwrap();
    assert!(tx.try_reserve().is_ok());
}

#[test]
fn mixed_reserve_send_doesnt_corrupt() {
    let (tx, rx) = tx_channel::<u32>(8);

    // Mix try_send and reserve/commit
    tx.try_send(1).unwrap();
    let r = tx.try_reserve().unwrap();
    tx.try_send(3).unwrap();
    tx.commit(&r, 2).unwrap();
    tx.try_send(4).unwrap();

    let mut values = Vec::new();
    while let Some(rv) = rx.try_recv() {
        values.push(rv.value);
    }
    // Order: 1 (try_send), 2 (commit of reservation), 3 (try_send), 4 (try_send)
    // Actually the order depends on when commit happens vs try_send
    // try_send(1) → queue: [1]
    // reserve → active_res: 1
    // try_send(3) → queue: [1, 3]
    // commit(r, 2) → queue: [1, 3, 2]
    // try_send(4) → queue: [1, 3, 2, 4]
    assert_eq!(values.len(), 4);
    assert_eq!(values[0], 1);
    assert_eq!(values[1], 3);
    assert_eq!(values[2], 2);
    assert_eq!(values[3], 4);
}

#[test]
fn producer_debug_format() {
    let (tx, _rx) = tx_channel::<u32>(8);
    let debug = format!("{:?}", tx);
    assert!(debug.contains("TxProducer"));
    assert!(debug.contains("capacity: 8"));
}

#[test]
fn consumer_debug_format() {
    let (_tx, rx) = tx_channel::<u32>(8);
    let debug = format!("{:?}", rx);
    assert!(debug.contains("TxConsumer"));
}
