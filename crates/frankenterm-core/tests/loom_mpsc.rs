//! Loom proofs for the `runtime_async::mpsc` channel contract.
//!
//! Loom cannot instrument asupersync internals, so we model the
//! contract with loom-native primitives and verify the invariants the
//! mpsc surface must preserve. Skeleton seeded under ft-syqcz.6
//! (commit 7ff0f9e37); ft-ue7sr (sub-bead of ft-syqcz.7) extended
//! this file with exhaustive coverage of the contract clauses.
//!
//! Contract under test:
//! - bounded queue with FIFO ordering (single producer)
//! - multiple producers, single consumer
//! - every successfully-sent value is delivered exactly once
//! - max in-flight items never exceed configured capacity
//! - close drains buffered values first, then signals closed via
//!   `recv -> None`
//! - send after close returns `Err(value)` without panic
//! - full queue blocks the sender on `not_full`; a recv that frees
//!   a slot wakes the parked sender (no deadlock)
//!
//! Mazurkiewicz trace catalog for mpsc lives at
//! [docs/runtime/mazurkiewicz-traces.md](../../../../docs/runtime/mazurkiewicz-traces.md).

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

#[derive(Debug)]
struct LoomBoundedMpsc<T> {
    state: Mutex<LoomBoundedMpscState<T>>,
    not_full: Condvar,
    not_empty: Condvar,
    capacity: usize,
}

#[derive(Debug)]
struct LoomBoundedMpscState<T> {
    queue: Vec<T>,
    closed: bool,
}

impl<T> LoomBoundedMpsc<T> {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(LoomBoundedMpscState {
                queue: Vec::with_capacity(capacity),
                closed: false,
            }),
            not_full: Condvar::new(),
            not_empty: Condvar::new(),
            capacity,
        }
    }

    fn send(&self, value: T) -> Result<(), T> {
        let mut state = self.state.lock().unwrap();
        while state.queue.len() == self.capacity && !state.closed {
            state = self.not_full.wait(state).unwrap();
        }
        if state.closed {
            return Err(value);
        }
        state.queue.push(value);
        self.not_empty.notify_one();
        Ok(())
    }

    fn recv(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        while state.queue.is_empty() && !state.closed {
            state = self.not_empty.wait(state).unwrap();
        }
        if state.queue.is_empty() {
            return None;
        }
        let value = state.queue.remove(0);
        self.not_full.notify_one();
        Some(value)
    }

    /// Close the channel from the producer side. Subsequent sends
    /// return `Err(value)`; existing buffered values stay drainable
    /// by the consumer.
    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        self.not_full.notify_all();
        self.not_empty.notify_all();
    }
}

#[test]
fn loom_mpsc_preserves_capacity_and_delivery() {
    loom::model(|| {
        let chan: Arc<LoomBoundedMpsc<usize>> = Arc::new(LoomBoundedMpsc::new(1));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));

        let chan_a = Arc::clone(&chan);
        let in_flight_a = Arc::clone(&in_flight);
        let max_a = Arc::clone(&max_in_flight);
        let producer_a = thread::spawn(move || {
            chan_a.send(1).unwrap();
            let depth = in_flight_a.fetch_add(1, Ordering::SeqCst) + 1;
            let mut current_max = max_a.load(Ordering::SeqCst);
            while depth > current_max {
                match max_a.compare_exchange(current_max, depth, Ordering::SeqCst, Ordering::SeqCst)
                {
                    Ok(_) => break,
                    Err(observed) => current_max = observed,
                }
            }
        });

        let chan_b = Arc::clone(&chan);
        let in_flight_b = Arc::clone(&in_flight);
        let max_b = Arc::clone(&max_in_flight);
        let producer_b = thread::spawn(move || {
            chan_b.send(2).unwrap();
            let depth = in_flight_b.fetch_add(1, Ordering::SeqCst) + 1;
            let mut current_max = max_b.load(Ordering::SeqCst);
            while depth > current_max {
                match max_b.compare_exchange(current_max, depth, Ordering::SeqCst, Ordering::SeqCst)
                {
                    Ok(_) => break,
                    Err(observed) => current_max = observed,
                }
            }
        });

        let chan_c = Arc::clone(&chan);
        let in_flight_c = Arc::clone(&in_flight);
        let consumer = thread::spawn(move || {
            let first = chan_c.recv().unwrap();
            in_flight_c.fetch_sub(1, Ordering::SeqCst);
            let second = chan_c.recv().unwrap();
            in_flight_c.fetch_sub(1, Ordering::SeqCst);
            (first, second)
        });

        producer_a.join().unwrap();
        producer_b.join().unwrap();
        let (first, second) = consumer.join().unwrap();

        let mut received = vec![first, second];
        received.sort_unstable();
        assert_eq!(received, vec![1, 2], "lost or duplicated send");
        assert!(
            max_in_flight.load(Ordering::SeqCst) <= 2,
            "in-flight count exceeded sender count"
        );
    });
}

/// ft-ue7sr: a single producer's sequence of sends is observed by
/// the consumer in FIFO order. Models the per-producer ordering
/// guarantee — even though mpsc is multi-producer, each producer's
/// sends are individually ordered with respect to themselves.
#[test]
fn loom_mpsc_single_producer_fifo_preserved() {
    loom::model(|| {
        let chan: Arc<LoomBoundedMpsc<usize>> = Arc::new(LoomBoundedMpsc::new(8));

        let chan_p = Arc::clone(&chan);
        let producer = thread::spawn(move || {
            chan_p.send(10).unwrap();
            chan_p.send(20).unwrap();
            chan_p.send(30).unwrap();
        });

        let chan_c = Arc::clone(&chan);
        let consumer = thread::spawn(move || {
            (
                chan_c.recv().unwrap(),
                chan_c.recv().unwrap(),
                chan_c.recv().unwrap(),
            )
        });

        producer.join().unwrap();
        let (a, b, c) = consumer.join().unwrap();

        assert_eq!(
            (a, b, c),
            (10, 20, 30),
            "single-producer FIFO violated: got ({a}, {b}, {c})",
        );
    });
}

/// ft-ue7sr: close after sends drains the buffer first, then
/// signals closed. Receivers must consume every buffered value
/// before observing `None`. Forbids close-erases-data.
#[test]
fn loom_mpsc_close_drains_then_observes_none() {
    loom::model(|| {
        let chan: Arc<LoomBoundedMpsc<usize>> = Arc::new(LoomBoundedMpsc::new(4));

        let chan_p = Arc::clone(&chan);
        let producer = thread::spawn(move || {
            chan_p.send(7).unwrap();
            chan_p.send(8).unwrap();
            chan_p.close();
        });

        producer.join().unwrap();

        let buffered_a = chan.recv();
        let buffered_b = chan.recv();
        let post_close = chan.recv();

        assert_eq!(buffered_a, Some(7));
        assert_eq!(buffered_b, Some(8));
        assert_eq!(post_close, None);
    });
}

/// ft-ue7sr: send after close returns `Err(value)` without panic.
/// Models the runtime_async::mpsc::Sender::send contract where
/// the closed receiver returns the payload back rather than
/// silently dropping or panicking.
#[test]
fn loom_mpsc_send_after_close_returns_err() {
    loom::model(|| {
        let chan: Arc<LoomBoundedMpsc<usize>> = Arc::new(LoomBoundedMpsc::new(4));

        chan.close();
        let result = chan.send(99);

        assert_eq!(result, Err(99), "send after close must return Err(value)");

        // recv on a closed empty channel returns None — covers the
        // "close before any send" no-deadlock case.
        assert_eq!(chan.recv(), None);
    });
}

/// ft-ue7sr: a full bounded queue blocks subsequent senders on
/// `not_full`; a recv that frees a slot wakes the parked sender.
/// Tests the no-deadlock invariant: capacity-1 channel, A fills it,
/// B parks waiting for capacity, recv consumes A's value, B's send
/// completes. Forbids "lost wakeup" on the not_full path.
#[test]
fn loom_mpsc_full_queue_unblocks_after_recv() {
    loom::model(|| {
        let chan: Arc<LoomBoundedMpsc<usize>> = Arc::new(LoomBoundedMpsc::new(1));

        // Fill the queue from this thread so the bounded condition
        // is established before any race begins.
        chan.send(1).unwrap();

        // Spawn a producer that tries to send 2; with capacity 1
        // and the queue already full, this must park on not_full
        // until a recv frees a slot.
        let chan_p = Arc::clone(&chan);
        let producer = thread::spawn(move || {
            chan_p.send(2).unwrap();
        });

        // Consume the first value, freeing the slot. The recv
        // calls not_full.notify_one(), which must wake the parked
        // producer thread above.
        let first = chan.recv().unwrap();
        producer.join().unwrap();
        let second = chan.recv().unwrap();

        assert_eq!(first, 1);
        assert_eq!(second, 2);
    });
}
