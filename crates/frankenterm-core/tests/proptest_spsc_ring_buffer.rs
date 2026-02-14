//! Property-based tests for the SPSC ring buffer channel.
//!
//! These tests validate bounded FIFO behavior, close semantics, depth
//! accounting, capacity invariants, and idempotency against a simple
//! `VecDeque` reference model.

use std::collections::VecDeque;

use proptest::prelude::*;

use frankenterm_core::spsc_ring_buffer::channel;

#[derive(Debug, Clone)]
enum Op {
    Send(i16),
    Recv,
    Close,
}

fn arb_ops(max_len: usize) -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            any::<i16>().prop_map(Op::Send),
            Just(Op::Recv),
            Just(Op::Close),
        ],
        1..max_len,
    )
}

struct RefModel {
    capacity: usize,
    closed: bool,
    buf: VecDeque<i16>,
}

impl RefModel {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            closed: false,
            buf: VecDeque::with_capacity(capacity),
        }
    }

    fn try_send(&mut self, value: i16) -> Result<(), i16> {
        if self.closed || self.buf.len() == self.capacity {
            Err(value)
        } else {
            self.buf.push_back(value);
            Ok(())
        }
    }

    fn try_recv(&mut self) -> Option<i16> {
        self.buf.pop_front()
    }

    fn close(&mut self) {
        self.closed = true;
    }

    fn len(&self) -> usize {
        self.buf.len()
    }
}

// =========================================================================
// Reference model linearizability
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn spsc_try_send_try_recv_matches_reference_model(
        capacity in 1usize..=32,
        ops in arb_ops(300),
    ) {
        let (tx, rx) = channel(capacity);
        let mut model = RefModel::new(capacity);

        for (idx, op) in ops.iter().enumerate() {
            match *op {
                Op::Send(v) => {
                    let expected = model.try_send(v);
                    let actual = tx.try_send(v);
                    prop_assert_eq!(
                        actual, expected,
                        "send mismatch at step {}",
                        idx
                    );
                }
                Op::Recv => {
                    let expected = model.try_recv();
                    let actual = rx.try_recv();
                    prop_assert_eq!(
                        actual, expected,
                        "recv mismatch at step {}",
                        idx
                    );
                }
                Op::Close => {
                    model.close();
                    tx.close();
                }
            }

            prop_assert_eq!(
                tx.is_closed(),
                model.closed,
                "closed flag mismatch at step {}",
                idx
            );
            prop_assert_eq!(tx.depth(), model.len(), "tx depth mismatch at step {}", idx);
            prop_assert_eq!(rx.depth(), model.len(), "rx depth mismatch at step {}", idx);
        }
    }
}

// =========================================================================
// FIFO ordering
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn spsc_close_and_drain_preserves_fifo(values in prop::collection::vec(any::<i16>(), 1..200)) {
        let capacity = values.len().max(1);
        let (tx, rx) = channel(capacity);

        for &v in &values {
            let sent = tx.try_send(v);
            prop_assert!(sent.is_ok(), "send unexpectedly failed while under capacity");
        }

        tx.close();
        prop_assert!(tx.is_closed());

        let mut drained = Vec::with_capacity(values.len());
        while let Some(v) = rx.try_recv() {
            drained.push(v);
        }

        prop_assert_eq!(drained, values, "drain order mismatch");
        prop_assert_eq!(rx.try_recv(), None, "expected drained channel to be empty");
    }

    /// Interleaved send/recv maintains FIFO order.
    #[test]
    fn spsc_interleaved_fifo(
        values in prop::collection::vec(any::<i16>(), 2..100),
        recv_every in 2usize..10,
    ) {
        let capacity = values.len();
        let (tx, rx) = channel(capacity);
        let mut expected_order = VecDeque::new();
        let mut received = Vec::new();

        for (i, &v) in values.iter().enumerate() {
            tx.try_send(v).unwrap();
            expected_order.push_back(v);

            if (i + 1) % recv_every == 0 {
                if let Some(got) = rx.try_recv() {
                    let want = expected_order.pop_front().unwrap();
                    received.push(got);
                    prop_assert_eq!(got, want, "FIFO mismatch at interleaved recv");
                }
            }
        }

        // Drain remainder
        while let Some(got) = rx.try_recv() {
            let want = expected_order.pop_front().unwrap();
            received.push(got);
            prop_assert_eq!(got, want, "FIFO mismatch during drain");
        }

        prop_assert!(expected_order.is_empty(), "not all items drained");
    }
}

// =========================================================================
// Capacity and depth invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Capacity accessor matches construction parameter.
    #[test]
    fn spsc_capacity_matches_construction(capacity in 1usize..=256) {
        let (tx, rx) = channel::<u8>(capacity);
        prop_assert_eq!(tx.capacity(), capacity);
        prop_assert_eq!(rx.capacity(), capacity);
    }

    /// Depth never exceeds capacity during random operations.
    #[test]
    fn spsc_depth_never_exceeds_capacity(
        capacity in 1usize..=32,
        ops in arb_ops(200),
    ) {
        let (tx, rx) = channel(capacity);
        for op in &ops {
            match *op {
                Op::Send(v) => { let _ = tx.try_send(v); }
                Op::Recv => { let _ = rx.try_recv(); }
                Op::Close => { tx.close(); }
            }
            prop_assert!(
                tx.depth() <= capacity,
                "tx depth {} > capacity {}", tx.depth(), capacity
            );
            prop_assert!(
                rx.depth() <= capacity,
                "rx depth {} > capacity {}", rx.depth(), capacity
            );
        }
    }

    /// Filling to exact capacity succeeds, one more fails.
    #[test]
    fn spsc_fill_to_capacity(capacity in 1usize..=64) {
        let (tx, rx) = channel::<u32>(capacity);

        for i in 0..capacity as u32 {
            prop_assert!(tx.try_send(i).is_ok(), "send {} should succeed", i);
        }
        prop_assert_eq!(tx.depth(), capacity);

        // One more should fail
        prop_assert!(tx.try_send(999).is_err(), "send beyond capacity should fail");
        prop_assert_eq!(tx.depth(), capacity, "depth should not change after failed send");

        // Verify rx sees same depth
        prop_assert_eq!(rx.depth(), capacity);
    }

    /// After draining all items, depth is 0.
    #[test]
    fn spsc_drain_to_zero(values in prop::collection::vec(any::<u32>(), 1..100)) {
        let cap = values.len();
        let (tx, rx) = channel(cap);
        for &v in &values {
            tx.try_send(v).unwrap();
        }
        prop_assert_eq!(tx.depth(), cap);

        for _ in 0..values.len() {
            let _ = rx.try_recv();
        }
        prop_assert_eq!(tx.depth(), 0);
        prop_assert_eq!(rx.depth(), 0);
    }

    /// tx.depth() and rx.depth() always agree.
    #[test]
    fn spsc_tx_rx_depth_agree(
        capacity in 1usize..=32,
        ops in arb_ops(200),
    ) {
        let (tx, rx) = channel(capacity);
        for op in &ops {
            match *op {
                Op::Send(v) => { let _ = tx.try_send(v); }
                Op::Recv => { let _ = rx.try_recv(); }
                Op::Close => { tx.close(); }
            }
            prop_assert_eq!(
                tx.depth(), rx.depth(),
                "tx.depth() != rx.depth()"
            );
        }
    }
}

// =========================================================================
// Close semantics
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// Fresh channel is not closed.
    #[test]
    fn spsc_fresh_not_closed(capacity in 1usize..=128) {
        let (tx, rx) = channel::<u8>(capacity);
        prop_assert!(!tx.is_closed());
        prop_assert!(!rx.is_closed());
    }

    /// After close, all try_send calls fail and return the value.
    #[test]
    fn spsc_send_after_close_fails(
        capacity in 1usize..=32,
        values in prop::collection::vec(any::<i32>(), 1..50),
    ) {
        let (tx, _rx) = channel::<i32>(capacity);
        tx.close();

        for &v in &values {
            let result = tx.try_send(v);
            prop_assert_eq!(result, Err(v), "try_send on closed channel should return Err(value)");
        }
    }

    /// Close is idempotent: multiple closes don't panic or change state.
    #[test]
    fn spsc_close_idempotent(
        capacity in 1usize..=32,
        close_count in 2usize..10,
    ) {
        let (tx, rx) = channel::<u8>(capacity);
        for _ in 0..close_count {
            tx.close();
        }
        prop_assert!(tx.is_closed());
        prop_assert!(rx.is_closed());
    }

    /// After close, existing items can still be received.
    #[test]
    fn spsc_close_preserves_buffered(values in prop::collection::vec(any::<u32>(), 1..50)) {
        let cap = values.len();
        let (tx, rx) = channel(cap);
        for &v in &values {
            tx.try_send(v).unwrap();
        }
        tx.close();

        let mut received = Vec::new();
        while let Some(v) = rx.try_recv() {
            received.push(v);
        }
        prop_assert_eq!(&received, &values, "buffered items should survive close");
    }

    /// is_closed is visible from both producer and consumer after close.
    #[test]
    fn spsc_is_closed_symmetric(capacity in 1usize..=64) {
        let (tx, rx) = channel::<u8>(capacity);
        prop_assert!(!tx.is_closed());
        prop_assert!(!rx.is_closed());
        tx.close();
        prop_assert!(tx.is_closed());
        prop_assert!(rx.is_closed());
    }
}

// =========================================================================
// Empty channel behavior
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// try_recv on empty channel returns None.
    #[test]
    fn spsc_recv_empty_returns_none(capacity in 1usize..=128) {
        let (_tx, rx) = channel::<u8>(capacity);
        prop_assert_eq!(rx.try_recv(), None);
        prop_assert_eq!(rx.depth(), 0);
    }

    /// Fresh channel has depth 0.
    #[test]
    fn spsc_fresh_depth_zero(capacity in 1usize..=128) {
        let (tx, rx) = channel::<u8>(capacity);
        prop_assert_eq!(tx.depth(), 0);
        prop_assert_eq!(rx.depth(), 0);
    }
}
