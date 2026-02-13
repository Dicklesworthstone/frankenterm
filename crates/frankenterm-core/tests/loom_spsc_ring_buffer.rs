//! Loom model-checks for SPSC ring-buffer index/close semantics.
//!
//! This uses a compact atomic model that mirrors the queue-level invariants:
//! bounded depth, no underflow, and close preventing future sends.

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::thread;

struct LoomSpscIndexModel {
    capacity: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
    closed: AtomicBool,
}

impl LoomSpscIndexModel {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        }
    }

    fn try_send(&self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }

        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.capacity {
            return false;
        }

        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    fn try_recv(&self) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return false;
        }

        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn snapshot(&self) -> (usize, usize, bool) {
        (
            self.head.load(Ordering::Acquire),
            self.tail.load(Ordering::Acquire),
            self.closed.load(Ordering::Acquire),
        )
    }
}

#[test]
fn loom_spsc_never_exceeds_capacity() {
    loom::model(|| {
        let q = Arc::new(LoomSpscIndexModel::new(2));

        let qp = Arc::clone(&q);
        let producer = thread::spawn(move || {
            let _ = qp.try_send();
            let _ = qp.try_send();
            let _ = qp.try_send(); // may fail when full
        });

        let qc = Arc::clone(&q);
        let consumer = thread::spawn(move || {
            let _ = qc.try_recv();
            let _ = qc.try_recv();
        });

        producer.join().unwrap();
        consumer.join().unwrap();

        let (head, tail, _) = q.snapshot();
        assert!(head >= tail, "tail advanced past head");
        assert!(head - tail <= 2, "depth exceeded capacity");
    });
}

#[test]
fn loom_spsc_close_prevents_future_sends() {
    loom::model(|| {
        let q = Arc::new(LoomSpscIndexModel::new(1));

        let qp = Arc::clone(&q);
        let producer = thread::spawn(move || {
            let _ = qp.try_send();
            qp.close();
            let sent_after_close = qp.try_send();
            assert!(!sent_after_close, "send after close must fail");
        });

        let qc = Arc::clone(&q);
        let consumer = thread::spawn(move || {
            let _ = qc.try_recv();
            let _ = qc.try_recv();
        });

        producer.join().unwrap();
        consumer.join().unwrap();

        let (head, tail, closed) = q.snapshot();
        assert!(closed, "queue should be closed");
        assert!(head >= tail, "tail advanced past head");
        assert!(head - tail <= 1, "depth exceeded capacity");
    });
}

#[test]
fn loom_spsc_produced_equals_consumed_plus_depth() {
    loom::model(|| {
        let q = Arc::new(LoomSpscIndexModel::new(4));

        let qp = Arc::clone(&q);
        let producer = thread::spawn(move || {
            let mut produced = 0usize;
            for _ in 0..3 {
                if qp.try_send() {
                    produced += 1;
                }
                thread::yield_now();
            }
            produced
        });

        let qc = Arc::clone(&q);
        let consumer = thread::spawn(move || {
            let mut consumed = 0usize;
            for _ in 0..6 {
                if qc.try_recv() {
                    consumed += 1;
                }
                thread::yield_now();
            }
            consumed
        });

        let result_produced = producer.join().unwrap();
        let result_consumed = consumer.join().unwrap();

        let (head, tail, _) = q.snapshot();
        let depth = head - tail;
        assert_eq!(
            result_produced,
            result_consumed + depth,
            "lost or duplicated elements"
        );
    });
}
