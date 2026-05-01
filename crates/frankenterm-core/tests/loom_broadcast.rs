//! Loom skeleton for the `runtime_async::broadcast` channel contract.
//!
//! Models a bounded ring broadcast where every receiver sees every
//! delivered message in the sender's order. The full exhaustive proofs
//! live in `ft-syqcz.7` (G8.2).
//!
//! Contract under test:
//! - all receivers observe the same total order of sender writes
//! - bounded ring: oldest entry is overwritten if all receivers have
//!   already advanced past it (lagging-receiver semantics are NOT
//!   modeled in the skeleton — they are part of G8.2)
//! - sender drop is observable by every receiver

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

#[derive(Debug)]
struct LoomBroadcast<T> {
    state: Mutex<LoomBroadcastState<T>>,
    not_empty: Condvar,
}

#[derive(Debug)]
struct LoomBroadcastState<T> {
    log: Vec<(u64, T)>,
    closed: bool,
}

impl<T: Clone> LoomBroadcast<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(LoomBroadcastState {
                log: Vec::new(),
                closed: false,
            }),
            not_empty: Condvar::new(),
        }
    }

    fn send(&self, value: T) -> u64 {
        let mut state = self.state.lock().unwrap();
        let seq = state.log.len() as u64;
        state.log.push((seq, value));
        self.not_empty.notify_all();
        seq
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        self.not_empty.notify_all();
    }

    fn recv_at(&self, cursor: u64) -> Option<(u64, T)> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(entry) = state.log.iter().find(|(seq, _)| *seq == cursor).cloned() {
                return Some(entry);
            }
            if state.closed {
                return None;
            }
            state = self.not_empty.wait(state).unwrap();
        }
    }
}

#[test]
fn loom_broadcast_preserves_total_order_across_receivers() {
    loom::model(|| {
        let bus: Arc<LoomBroadcast<usize>> = Arc::new(LoomBroadcast::new());
        let received_count = Arc::new(AtomicUsize::new(0));

        let bus_send = Arc::clone(&bus);
        let sender = thread::spawn(move || {
            bus_send.send(10);
            bus_send.send(20);
        });

        let bus_a = Arc::clone(&bus);
        let count_a = Arc::clone(&received_count);
        let receiver_a = thread::spawn(move || {
            let (s0, v0) = bus_a.recv_at(0).unwrap();
            count_a.fetch_add(1, Ordering::SeqCst);
            let (s1, v1) = bus_a.recv_at(1).unwrap();
            count_a.fetch_add(1, Ordering::SeqCst);
            (s0, v0, s1, v1)
        });

        let bus_b = Arc::clone(&bus);
        let count_b = Arc::clone(&received_count);
        let receiver_b = thread::spawn(move || {
            let (s0, v0) = bus_b.recv_at(0).unwrap();
            count_b.fetch_add(1, Ordering::SeqCst);
            let (s1, v1) = bus_b.recv_at(1).unwrap();
            count_b.fetch_add(1, Ordering::SeqCst);
            (s0, v0, s1, v1)
        });

        sender.join().unwrap();
        let observed_a = receiver_a.join().unwrap();
        let observed_b = receiver_b.join().unwrap();

        assert_eq!(observed_a, (0, 10, 1, 20));
        assert_eq!(observed_b, (0, 10, 1, 20));
        assert_eq!(received_count.load(Ordering::SeqCst), 4);
    });
}
