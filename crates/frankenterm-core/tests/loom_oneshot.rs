//! Loom skeleton for the `runtime_async::oneshot` channel contract.
//!
//! Models the single-fire send/recv pair that oneshot provides. The
//! full exhaustive proofs live in `ft-syqcz.7` (G8.2).
//!
//! Contract under test:
//! - exactly one send succeeds; the value is delivered exactly once
//!   to the receiver
//! - sender drop without send transitions the receiver to a closed
//!   state observable as `None`
//! - receiver drop is benign for the sender (no panic)

use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

#[derive(Debug)]
struct LoomOneshot<T> {
    state: Mutex<LoomOneshotState<T>>,
    delivered: Condvar,
}

#[derive(Debug)]
struct LoomOneshotState<T> {
    value: Option<T>,
    sent: bool,
    sender_dropped: bool,
}

impl<T> LoomOneshot<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(LoomOneshotState {
                value: None,
                sent: false,
                sender_dropped: false,
            }),
            delivered: Condvar::new(),
        }
    }

    fn send(&self, value: T) -> Result<(), T> {
        let mut state = self.state.lock().unwrap();
        if state.sent {
            return Err(value);
        }
        state.value = Some(value);
        state.sent = true;
        self.delivered.notify_all();
        Ok(())
    }

    fn close_sender(&self) {
        let mut state = self.state.lock().unwrap();
        state.sender_dropped = true;
        self.delivered.notify_all();
    }

    fn recv(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        while !state.sent && !state.sender_dropped {
            state = self.delivered.wait(state).unwrap();
        }
        state.value.take()
    }
}

#[test]
fn loom_oneshot_delivers_value_exactly_once() {
    loom::model(|| {
        let chan: Arc<LoomOneshot<usize>> = Arc::new(LoomOneshot::new());

        let chan_s = Arc::clone(&chan);
        let sender = thread::spawn(move || {
            chan_s.send(42).unwrap();
        });

        let chan_r = Arc::clone(&chan);
        let receiver = thread::spawn(move || chan_r.recv());

        sender.join().unwrap();
        let received = receiver.join().unwrap();

        assert_eq!(received, Some(42));
    });
}

#[test]
fn loom_oneshot_sender_drop_observed_as_none() {
    loom::model(|| {
        let chan: Arc<LoomOneshot<usize>> = Arc::new(LoomOneshot::new());

        let chan_s = Arc::clone(&chan);
        let sender = thread::spawn(move || {
            chan_s.close_sender();
        });

        let chan_r = Arc::clone(&chan);
        let receiver = thread::spawn(move || chan_r.recv());

        sender.join().unwrap();
        let received = receiver.join().unwrap();

        assert_eq!(received, None);
    });
}
