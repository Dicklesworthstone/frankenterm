//! Property-based tests for the SPSC ring buffer channel.
//!
//! These tests validate bounded FIFO behavior, close semantics, and depth
//! accounting against a simple `VecDeque` reference model.

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
}
