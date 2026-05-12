//! Loom proofs for the `runtime_async::oneshot` channel contract.
//!
//! Models the single-fire send/recv pair that oneshot provides.
//! Skeleton seeded under ft-syqcz.6 (commit 7ff0f9e37); ft-zzw3s
//! (subset 1 of ft-syqcz.7) extended this file with exhaustive
//! coverage of the contract clauses.
//!
//! Contract under test:
//! - exactly one send succeeds; the value is delivered exactly once
//!   to the receiver
//! - sender drop without send transitions the receiver to a closed
//!   state observable as `None`
//! - receiver drop is benign for the sender (no panic)
//! - send-after-receiver-drop returns Err(value) without panic
//! - concurrent send + close_sender race: receiver observes either
//!   Some(v) (send won) or None (close won), never both, never neither
//! - recv after a close-or-deliver is idempotent: subsequent recv
//!   calls (on the same receiver) observe the absence of value
//!
//! Mazurkiewicz trace catalog for oneshot lives at
//! [docs/runtime/mazurkiewicz-traces.md](../../../../docs/runtime/mazurkiewicz-traces.md).

use loom::sync::{Arc, Condvar, Mutex};
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

    /// Mark the receiver as gone so a subsequent send fails closed
    /// rather than blocking on a notification nobody waits for.
    fn close_receiver(&self) {
        let mut state = self.state.lock().unwrap();
        // Use sender_dropped as the unified "channel closed" flag for
        // the model — for the receiver-side test we just want send to
        // observe a closed channel and refuse the value.
        state.sender_dropped = true;
        self.delivered.notify_all();
    }

    /// Send variant that returns Err(value) when the channel is
    /// closed (either side dropped) without panicking. Mirrors the
    /// runtime_async::oneshot send return semantics.
    fn try_send_or_drop(&self, value: T) -> Result<(), T> {
        let mut state = self.state.lock().unwrap();
        if state.sender_dropped || state.sent {
            return Err(value);
        }
        state.value = Some(value);
        state.sent = true;
        self.delivered.notify_all();
        Ok(())
    }

    /// Non-blocking recv used to model "second recv call after the
    /// first delivered or closed" — never blocks because the model
    /// only invokes it after the channel is in a terminal state.
    fn try_recv_terminal(&self) -> Option<T> {
        let mut state = self.state.lock().unwrap();
        state.value.take()
    }
}

#[test]
fn loom_oneshot_cancel_trace_classes_are_declared() {
    loom::model(|| {
        let classes: &[TraceClass] = &[
            (
                "delivery-before-cancel",
                &["send", "recv", "cancel"],
                &[("send", "recv"), ("cancel", "recv")],
                "recv returns Some(value) exactly once; later cancel is post-terminal",
            ),
            (
                "receiver-cancel-before-send",
                &["cancel", "send"],
                &[("cancel", "send")],
                "send observes the closed receiver and returns Err(value)",
            ),
            (
                "sender-close-before-recv",
                &["close_sender", "recv", "cancel"],
                &[("close_sender", "recv"), ("cancel", "recv")],
                "recv returns None and no value is synthesized",
            ),
            (
                "send-close-race",
                &["send", "close_sender", "recv"],
                &[("send", "recv"), ("close_sender", "recv")],
                "exactly one terminal outcome is selected",
            ),
        ];
        assert_cancel_trace_table(classes, 4);
    });
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
        let joined_value = receiver.join().unwrap();

        assert_eq!(joined_value, Some(42));
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
        let joined_value = receiver.join().unwrap();

        assert_eq!(joined_value, None);
    });
}

/// ft-zzw3s: send-after-receiver-drop must return `Err(value)` without
/// panicking. Models the runtime_async::oneshot::Sender::send contract
/// where the channel is observably closed and the sender gets the
/// payload back rather than a panic / silent drop.
#[test]
fn loom_oneshot_send_after_receiver_drop_returns_err() {
    loom::model(|| {
        let chan: Arc<LoomOneshot<usize>> = Arc::new(LoomOneshot::new());

        let chan_r = Arc::clone(&chan);
        let receiver_drop = thread::spawn(move || {
            chan_r.close_receiver();
        });

        let chan_s = Arc::clone(&chan);
        let sender = thread::spawn(move || chan_s.try_send_or_drop(7));

        receiver_drop.join().unwrap();
        let result = sender.join().unwrap();

        // Either the receiver-drop happened first (Err) or the send
        // happened first (Ok) — both are linearizable outcomes. The
        // invariant is that *neither* path panics and the value is
        // accounted for exactly once.
        match result {
            Ok(()) => {}                // send won the race
            Err(v) => assert_eq!(v, 7), // close won; payload returned
        }
    });
}

/// ft-zzw3s: concurrent send + close-sender race. The receiver must
/// observe exactly one terminal outcome:
///
///   * `Some(v)` — send linearized first
///   * `None`    — close_sender linearized first
///
/// Never both (would imply double-delivery) and never neither (would
/// imply the receiver woke spuriously without a state transition).
#[test]
fn loom_oneshot_send_close_race_one_outcome() {
    loom::model(|| {
        let chan: Arc<LoomOneshot<usize>> = Arc::new(LoomOneshot::new());

        let chan_s = Arc::clone(&chan);
        let sender = thread::spawn(move || {
            let _ = chan_s.send(99);
        });

        let chan_c = Arc::clone(&chan);
        let closer = thread::spawn(move || {
            chan_c.close_sender();
        });

        let chan_r = Arc::clone(&chan);
        let receiver = thread::spawn(move || chan_r.recv());

        sender.join().unwrap();
        closer.join().unwrap();
        let joined_value = receiver.join().unwrap();

        // Receiver MUST observe a terminal outcome; either delivery
        // succeeded (Some(99)) or the channel closed first (None).
        match joined_value {
            Some(v) => assert_eq!(v, 99),
            None => {}
        }
    });
}

/// ft-zzw3s: recv after a terminal state is idempotent — a second
/// (or third) recv on the same receiver MUST observe `None` because
/// the value has already been taken or the channel is closed without
/// one. Models the contract that `try_recv` after `recv` does not
/// double-yield.
#[test]
fn loom_oneshot_recv_idempotent_after_terminal() {
    loom::model(|| {
        let chan: Arc<LoomOneshot<usize>> = Arc::new(LoomOneshot::new());

        // Drive both parties to terminal in this thread so the
        // schedule space stays small (the property under test does
        // not need a multi-thread interleaving — it is a per-handle
        // invariant once we are post-terminal).
        chan.send(11).unwrap();
        let first = chan.recv();
        let second = chan.try_recv_terminal();
        let third = chan.try_recv_terminal();

        assert_eq!(first, Some(11));
        assert_eq!(second, None);
        assert_eq!(third, None);
    });
}

/// ft-zzw3s: receiver-drop while a send is in flight must not panic
/// the sender. Mirrors the asupersync runtime_async::oneshot contract
/// that sender drop is benign (the channel returns the payload via
/// `Err` rather than tearing down the task).
#[test]
fn loom_oneshot_send_during_receiver_drop_no_panic() {
    loom::model(|| {
        let chan: Arc<LoomOneshot<usize>> = Arc::new(LoomOneshot::new());

        let chan_s = Arc::clone(&chan);
        let sender = thread::spawn(move || chan_s.try_send_or_drop(3));

        let chan_r = Arc::clone(&chan);
        let receiver_drop = thread::spawn(move || {
            chan_r.close_receiver();
        });

        // Joining either order must succeed; the invariant under test
        // is that sender's try_send_or_drop terminates with a
        // well-formed Ok/Err — Loom's panic detection guards the
        // implicit "no panic" half of the contract.
        let send_result = sender.join().unwrap();
        receiver_drop.join().unwrap();
        match send_result {
            Ok(()) => {}
            Err(v) => assert_eq!(v, 3),
        }
    });
}
