//! Loom model-checks for SPSC ring-buffer index/close semantics.
//!
//! This uses a compact atomic model that mirrors the queue-level invariants:
//! bounded depth, no underflow, and close preventing future sends.

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::thread;

type Dependency = (&'static str, &'static str);
type TraceClass = (
    &'static str,
    &'static [&'static str],
    &'static [Dependency],
    &'static str,
);

fn assert_cancel_trace_table(classes: &[TraceClass], min_classes: usize) {
    assert!(classes.len() >= min_classes, "missing cancel trace classes");
    let mut saw_cancel = false;
    for class in classes {
        let (name, events, dependencies, invariant) = *class;
        assert!(!name.is_empty(), "trace class must have a name");
        assert!(!events.is_empty(), "trace class {name} must list events");
        assert!(
            !invariant.is_empty(),
            "trace class {name} must declare an invariant"
        );
        saw_cancel |= events.contains(&"cancel");
        for (left, right) in dependencies {
            assert_ne!(left, right, "dependency relation excludes self edges");
            assert!(
                events.contains(left),
                "{name} dependency left event missing"
            );
            assert!(
                events.contains(right),
                "{name} dependency right event missing"
            );
        }
    }
    assert!(saw_cancel, "at least one class must include cancellation");
}

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
fn loom_spsc_cancel_trace_classes_are_declared() {
    loom::model(|| {
        let classes: &[TraceClass] = &[
            (
                "producer-cancel-before-send",
                &["cancel", "try_send"],
                &[("cancel", "try_send")],
                "head does not advance when producer cancellation wins",
            ),
            (
                "send-before-consumer-cancel",
                &["try_send", "cancel", "try_recv"],
                &[("try_send", "try_recv"), ("cancel", "try_recv")],
                "depth accounts for produced minus consumed",
            ),
            (
                "close-before-future-send",
                &["close", "try_send", "cancel"],
                &[("close", "try_send"), ("cancel", "try_send")],
                "future sends fail once closed is visible",
            ),
            (
                "send-receive-race",
                &["try_send", "try_recv"],
                &[("try_send", "try_recv")],
                "depth stays between zero and capacity",
            ),
        ];
        assert_cancel_trace_table(classes, 4);
    });
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
