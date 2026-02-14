//! Property-based tests for the SPSC ring buffer channel.
//!
//! These tests validate bounded FIFO behavior, close semantics, depth
//! accounting, capacity invariants, idempotency against a simple
//! VecDeque reference model, count conservation, wrap-around correctness,
//! producer-drop semantics, capacity-1 edge cases, depth precision,
//! and long-sequence stress invariants.

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

// =========================================================================
// Count conservation
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// sent - received == depth at every step.
    #[test]
    fn spsc_count_conservation(
        capacity in 1usize..=32,
        ops in arb_ops(200),
    ) {
        let (tx, rx) = channel(capacity);
        let mut total_sent: usize = 0;
        let mut total_received: usize = 0;

        for op in &ops {
            match *op {
                Op::Send(v) => {
                    if tx.try_send(v).is_ok() {
                        total_sent += 1;
                    }
                }
                Op::Recv => {
                    if rx.try_recv().is_some() {
                        total_received += 1;
                    }
                }
                Op::Close => {
                    tx.close();
                }
            }
            let expected_depth = total_sent - total_received;
            prop_assert_eq!(
                tx.depth(), expected_depth,
                "depth {} != sent {} - recv {}",
                tx.depth(), total_sent, total_received
            );
        }
    }

    /// Total received never exceeds total sent.
    #[test]
    fn spsc_recv_never_exceeds_sent(
        capacity in 1usize..=32,
        ops in arb_ops(200),
    ) {
        let (tx, rx) = channel(capacity);
        let mut total_sent: usize = 0;
        let mut total_received: usize = 0;

        for op in &ops {
            match *op {
                Op::Send(v) => {
                    if tx.try_send(v).is_ok() {
                        total_sent += 1;
                    }
                }
                Op::Recv => {
                    if rx.try_recv().is_some() {
                        total_received += 1;
                    }
                }
                Op::Close => { tx.close(); }
            }
            prop_assert!(
                total_received <= total_sent,
                "received {} > sent {}",
                total_received, total_sent
            );
        }
    }
}

// =========================================================================
// Multiple fill-drain cycles (wrap-around)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Multiple fill-drain cycles produce correct FIFO output.
    #[test]
    fn spsc_multi_cycle_fifo(
        capacity in 1usize..=16,
        cycles in 2usize..=10,
    ) {
        let (tx, rx) = channel::<u32>(capacity);

        for cycle in 0..cycles as u32 {
            let base = cycle * capacity as u32;
            for i in 0..capacity as u32 {
                prop_assert!(tx.try_send(base + i).is_ok(),
                    "fill failed at cycle {} item {}", cycle, i);
            }
            prop_assert_eq!(tx.depth(), capacity);

            for i in 0..capacity as u32 {
                let got = rx.try_recv();
                prop_assert_eq!(got, Some(base + i),
                    "FIFO mismatch at cycle {} item {}: got {:?}", cycle, i, got);
            }
            prop_assert_eq!(tx.depth(), 0);
        }
    }

    /// Partial fill-drain cycles maintain correct depth.
    #[test]
    fn spsc_partial_cycles(
        capacity in 2usize..=16,
        fill_amount in 1usize..=16,
        drain_amount in 1usize..=16,
        cycles in 2usize..=8,
    ) {
        let (tx, rx) = channel::<u32>(capacity);
        let mut expected_depth: usize = 0;
        let mut next_val: u32 = 0;

        for _cycle in 0..cycles {
            // Fill phase
            let actual_fill = fill_amount.min(capacity - expected_depth);
            for _ in 0..actual_fill {
                if tx.try_send(next_val).is_ok() {
                    expected_depth += 1;
                    next_val += 1;
                }
            }
            prop_assert_eq!(tx.depth(), expected_depth,
                "depth after fill: {} != {}", tx.depth(), expected_depth);

            // Drain phase
            let actual_drain = drain_amount.min(expected_depth);
            for _ in 0..actual_drain {
                if rx.try_recv().is_some() {
                    expected_depth -= 1;
                }
            }
            prop_assert_eq!(tx.depth(), expected_depth,
                "depth after drain: {} != {}", tx.depth(), expected_depth);
        }
    }
}

// =========================================================================
// Producer drop semantics
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Dropping producer closes the channel.
    #[test]
    fn spsc_drop_producer_closes(capacity in 1usize..=64) {
        let (tx, rx) = channel::<u8>(capacity);
        prop_assert!(!rx.is_closed());
        drop(tx);
        prop_assert!(rx.is_closed());
    }

    /// Items sent before producer drop are still receivable.
    #[test]
    fn spsc_items_survive_producer_drop(
        values in prop::collection::vec(any::<u32>(), 1..50),
    ) {
        let cap = values.len();
        let (tx, rx) = channel(cap);
        for &v in &values {
            tx.try_send(v).unwrap();
        }
        drop(tx);

        let mut received = Vec::new();
        while let Some(v) = rx.try_recv() {
            received.push(v);
        }
        prop_assert_eq!(&received, &values,
            "items should be receivable after producer drop");
    }

    /// After producer drop and full drain, try_recv returns None.
    #[test]
    fn spsc_fully_drained_after_drop(
        values in prop::collection::vec(any::<u8>(), 0..30),
    ) {
        let cap = values.len().max(1);
        let (tx, rx) = channel(cap);
        for &v in &values {
            tx.try_send(v).unwrap();
        }
        drop(tx);

        for _ in 0..values.len() {
            let _ = rx.try_recv();
        }
        prop_assert_eq!(rx.try_recv(), None,
            "should get None after full drain post-drop");
        prop_assert_eq!(rx.depth(), 0);
    }
}

// =========================================================================
// Failed send returns original value
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Failed try_send returns the exact value that was passed in.
    #[test]
    fn spsc_failed_send_returns_value(
        capacity in 1usize..=8,
        fill_values in prop::collection::vec(any::<i32>(), 1..=8),
        extra_value in any::<i32>(),
    ) {
        let cap = capacity.min(fill_values.len());
        let (tx, _rx) = channel::<i32>(cap);

        // Fill to capacity
        for &v in &fill_values[..cap] {
            let _ = tx.try_send(v);
        }

        // Next send should fail and return the value
        let result = tx.try_send(extra_value);
        prop_assert_eq!(result, Err(extra_value),
            "failed send should return the exact value");
    }

    /// Closed channel try_send returns the exact value.
    #[test]
    fn spsc_closed_send_returns_value(value in any::<i64>()) {
        let (tx, _rx) = channel::<i64>(4);
        tx.close();
        let result = tx.try_send(value);
        prop_assert_eq!(result, Err(value),
            "closed channel send should return exact value");
    }
}

// =========================================================================
// Capacity-1 edge cases (minimal buffer)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Capacity-1 channel: each send must be followed by recv before next send.
    #[test]
    fn spsc_capacity_one_alternating(values in prop::collection::vec(any::<i16>(), 1..100)) {
        let (tx, rx) = channel::<i16>(1);

        for &v in &values {
            prop_assert!(tx.try_send(v).is_ok());
            prop_assert_eq!(tx.depth(), 1);
            // Second send must fail
            prop_assert!(tx.try_send(v).is_err());

            let got = rx.try_recv();
            prop_assert_eq!(got, Some(v));
            prop_assert_eq!(tx.depth(), 0);
        }
    }

    /// Capacity-1 channel preserves full FIFO across alternating pattern.
    #[test]
    fn spsc_capacity_one_fifo(values in prop::collection::vec(any::<u32>(), 1..50)) {
        let (tx, rx) = channel::<u32>(1);
        let mut received = Vec::new();

        for &v in &values {
            tx.try_send(v).unwrap();
            received.push(rx.try_recv().unwrap());
        }

        prop_assert_eq!(&received, &values, "capacity-1 FIFO broken");
    }
}

// =========================================================================
// Depth precision (increment/decrement by exactly 1)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Depth increments by exactly 1 on successful send.
    #[test]
    fn spsc_depth_increments_on_send(
        capacity in 2usize..=32,
        count in 1usize..=32,
    ) {
        let count = count.min(capacity);
        let (tx, _rx) = channel::<u32>(capacity);

        for i in 0..count {
            let depth_before = tx.depth();
            tx.try_send(i as u32).unwrap();
            prop_assert_eq!(
                tx.depth(),
                depth_before + 1,
                "depth should increment by 1 after send"
            );
        }
    }

    /// Depth decrements by exactly 1 on successful recv.
    #[test]
    fn spsc_depth_decrements_on_recv(
        capacity in 2usize..=32,
        count in 1usize..=32,
    ) {
        let count = count.min(capacity);
        let (tx, rx) = channel::<u32>(capacity);

        for i in 0..count {
            tx.try_send(i as u32).unwrap();
        }

        for _ in 0..count {
            let depth_before = rx.depth();
            rx.try_recv().unwrap();
            prop_assert_eq!(
                rx.depth(),
                depth_before - 1,
                "depth should decrement by 1 after recv"
            );
        }
    }

    /// Failed send does not change depth.
    #[test]
    fn spsc_failed_send_no_depth_change(capacity in 1usize..=16) {
        let (tx, _rx) = channel::<u32>(capacity);

        // Fill to capacity
        for i in 0..capacity as u32 {
            tx.try_send(i).unwrap();
        }
        let full_depth = tx.depth();
        prop_assert_eq!(full_depth, capacity);

        // Failed sends should not change depth
        for _ in 0..5 {
            let _ = tx.try_send(999);
            prop_assert_eq!(tx.depth(), full_depth, "depth changed after failed send");
        }
    }

    /// Failed recv (empty) does not change depth.
    #[test]
    fn spsc_failed_recv_no_depth_change(capacity in 1usize..=64) {
        let (_tx, rx) = channel::<u32>(capacity);
        prop_assert_eq!(rx.depth(), 0);

        for _ in 0..5 {
            let _ = rx.try_recv();
            prop_assert_eq!(rx.depth(), 0, "depth changed after failed recv");
        }
    }
}

// =========================================================================
// Partial drain then refill across wrap boundary
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// Partial drain then refill preserves FIFO across internal wrap boundary.
    #[test]
    fn spsc_partial_drain_refill_fifo(
        capacity in 2usize..=16,
        drain_fraction_pct in 25_u32..75,
    ) {
        let (tx, rx) = channel::<u32>(capacity);

        // Fill to capacity
        for i in 0..capacity as u32 {
            tx.try_send(i).unwrap();
        }

        // Drain a fraction
        let drain_count = ((capacity as u32 * drain_fraction_pct) / 100).max(1) as usize;
        let drain_count = drain_count.min(capacity);
        let mut received = Vec::new();
        for _ in 0..drain_count {
            if let Some(v) = rx.try_recv() {
                received.push(v);
            }
        }
        prop_assert_eq!(received.len(), drain_count);

        // Refill the drained slots
        let refill_start = capacity as u32;
        for i in 0..drain_count as u32 {
            prop_assert!(tx.try_send(refill_start + i).is_ok());
        }

        // Drain everything and verify FIFO
        let mut all_remaining = Vec::new();
        while let Some(v) = rx.try_recv() {
            all_remaining.push(v);
        }

        // Expected: items [drain_count..capacity], then [capacity..capacity+drain_count]
        let expected: Vec<u32> = (drain_count as u32..capacity as u32)
            .chain(refill_start..refill_start + drain_count as u32)
            .collect();
        prop_assert_eq!(all_remaining, expected, "FIFO broken across wrap boundary");
    }
}

// =========================================================================
// Drop producer: depth preserved and buffer drainable
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// Dropping producer preserves depth and all buffered items are drainable.
    #[test]
    fn spsc_drop_producer_preserves_depth_and_drain(
        capacity in 1usize..=32,
        values in prop::collection::vec(any::<u32>(), 0..32),
    ) {
        let fill_count = values.len().min(capacity);
        let (tx, rx) = channel::<u32>(capacity);

        for &v in values.iter().take(fill_count) {
            let _ = tx.try_send(v);
        }

        let depth_before_drop = rx.depth();
        drop(tx);

        prop_assert!(rx.is_closed(), "consumer should see closed after producer drop");
        prop_assert_eq!(rx.depth(), depth_before_drop, "depth should not change on drop");

        // All buffered items should still be drainable
        let mut drained = 0;
        while rx.try_recv().is_some() {
            drained += 1;
        }
        prop_assert_eq!(drained, depth_before_drop, "should drain all buffered items");
    }
}

// =========================================================================
// Long-sequence stress invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Long random operation sequences preserve all invariants.
    #[test]
    fn spsc_long_sequence_invariants(
        capacity in 1usize..=16,
        ops in arb_ops(1000),
    ) {
        let (tx, rx) = channel(capacity);
        let mut total_sent: u64 = 0;
        let mut total_recv: u64 = 0;

        for op in &ops {
            match *op {
                Op::Send(v) => {
                    if tx.try_send(v).is_ok() {
                        total_sent += 1;
                    }
                }
                Op::Recv => {
                    if rx.try_recv().is_some() {
                        total_recv += 1;
                    }
                }
                Op::Close => {
                    tx.close();
                }
            }

            // Invariant: depth = sent - received (for items still in buffer)
            let expected_depth = (total_sent - total_recv) as usize;
            prop_assert_eq!(
                tx.depth(), expected_depth,
                "depth invariant violated: sent={}, recv={}, depth={}",
                total_sent, total_recv, tx.depth()
            );
            prop_assert!(tx.depth() <= capacity, "depth exceeds capacity");
        }
    }
}
