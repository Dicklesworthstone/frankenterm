//! Loom proofs for the `runtime_async::watch` channel contract.
//!
//! Models the last-write-wins single-value broadcast that watch
//! provides. Skeleton seeded under ft-syqcz.6 (commit 7ff0f9e37);
//! ft-r51h4 (sub-bead of ft-syqcz.7) extended this file with
//! exhaustive coverage of the contract clauses.
//!
//! Contract under test:
//! - last write wins; no intermediate values are mandated to be
//!   observed
//! - reader sees a monotonically non-decreasing version sequence
//! - sender drop transitions the channel to a closed state observable
//!   by the reader
//! - initial value is observable on cursor 0 before any send
//! - concurrent writers serialize through the bus mutex; no torn
//!   reads / no synthesized values
//! - close is sticky: once set, subsequent send must not clear it
//!   (matches the asupersync Sender-drop contract where close
//!   precedes the implicit drop of the send capability)
//!
//! Mazurkiewicz trace catalog for watch lives at
//! [docs/runtime/mazurkiewicz-traces.md](../../../../docs/runtime/mazurkiewicz-traces.md).

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

#[derive(Debug)]
struct LoomWatch<T> {
    state: Mutex<LoomWatchState<T>>,
}

#[derive(Debug)]
struct LoomWatchState<T> {
    value: T,
    version: u64,
    closed: bool,
}

impl<T: Clone> LoomWatch<T> {
    fn new(initial: T) -> Self {
        Self {
            state: Mutex::new(LoomWatchState {
                value: initial,
                version: 0,
                closed: false,
            }),
        }
    }

    fn send(&self, value: T) {
        let mut state = self.state.lock().unwrap();
        state.value = value;
        state.version += 1;
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
    }

    fn snapshot(&self) -> (T, u64, bool) {
        let state = self.state.lock().unwrap();
        (state.value.clone(), state.version, state.closed)
    }
}

#[test]
fn loom_watch_preserves_version_monotonicity() {
    loom::model(|| {
        let watch: Arc<LoomWatch<usize>> = Arc::new(LoomWatch::new(0));
        let last_observed_version = Arc::new(AtomicUsize::new(0));

        let watch_a = Arc::clone(&watch);
        let writer_a = thread::spawn(move || {
            watch_a.send(1);
        });

        let watch_b = Arc::clone(&watch);
        let writer_b = thread::spawn(move || {
            watch_b.send(2);
        });

        let watch_r = Arc::clone(&watch);
        let last = Arc::clone(&last_observed_version);
        let reader = thread::spawn(move || {
            let (_value, version, _closed) = watch_r.snapshot();
            let prev = last.swap(version as usize, Ordering::SeqCst);
            assert!(
                version as usize >= prev,
                "watch reader observed a version regression: prev={} now={}",
                prev,
                version,
            );
            version
        });

        writer_a.join().unwrap();
        writer_b.join().unwrap();
        let _final_version = reader.join().unwrap();

        let (final_value, final_version, _) = watch.snapshot();
        assert!(
            final_version <= 2,
            "version count exceeds writer count: {}",
            final_version,
        );
        assert!(
            (1..=2).contains(&final_value),
            "final value must come from one of the writers, got {}",
            final_value,
        );
    });
}

#[test]
fn loom_watch_close_observed_after_drop() {
    loom::model(|| {
        let watch: Arc<LoomWatch<usize>> = Arc::new(LoomWatch::new(0));

        let watch_w = Arc::clone(&watch);
        let writer = thread::spawn(move || {
            watch_w.send(7);
            watch_w.close();
        });

        writer.join().unwrap();

        let (value, version, closed) = watch.snapshot();
        assert!(
            closed,
            "watch must observe close after sender path completes"
        );
        assert_eq!(value, 7);
        assert_eq!(version, 1);
    });
}

/// ft-r51h4: a reader that snapshots before any writer has run sees
/// the initial value with version 0 and `closed = false`. Models the
/// contract that watch's initial value is part of the observable
/// state from the start, not a special-case "no value yet" sentinel.
#[test]
fn loom_watch_initial_value_visible_before_writes() {
    loom::model(|| {
        let watch: Arc<LoomWatch<&'static str>> = Arc::new(LoomWatch::new("initial"));
        let (value, version, closed) = watch.snapshot();
        assert_eq!(value, "initial");
        assert_eq!(version, 0);
        assert!(!closed);
    });
}

/// ft-r51h4: concurrent writers serialize through the bus mutex.
/// The reader observes one of the sent values (last-write-wins) and
/// the version count never exceeds the writer count. Forbids torn
/// reads and synthesized values. The skeleton's
/// `loom_watch_preserves_version_monotonicity` proof covers
/// monotonicity; this complementary proof pins value-integrity.
#[test]
fn loom_watch_concurrent_writers_no_torn_reads() {
    loom::model(|| {
        let watch: Arc<LoomWatch<usize>> = Arc::new(LoomWatch::new(0));

        let watch_a = Arc::clone(&watch);
        let writer_a = thread::spawn(move || {
            watch_a.send(11);
        });

        let watch_b = Arc::clone(&watch);
        let writer_b = thread::spawn(move || {
            watch_b.send(22);
        });

        writer_a.join().unwrap();
        writer_b.join().unwrap();

        let (value, version, _closed) = watch.snapshot();
        // Must be one of the actually-sent values OR the initial 0
        // (if the reader linearized between writer creation and the
        // first send — for this proof both writers complete before
        // the snapshot, so 0 is impossible). Either way, no
        // synthesized values.
        assert!(
            value == 11 || value == 22,
            "watch produced a torn or synthesized value: {value}"
        );
        // version increments once per send; with two writers, exactly
        // 2 increments must have occurred (mutex serializes; no
        // overlap). Forbids "lost write" (version=1 with both done).
        assert_eq!(version, 2, "exactly 2 sends must have linearized");
    });
}

/// ft-r51h4: a single reader taking multiple snapshots in sequence
/// must observe versions that never go backwards. Even with a
/// concurrent writer, the version sequence (v0, v1, v2) at three
/// snapshot linearization points satisfies v0 ≤ v1 ≤ v2.
#[test]
fn loom_watch_reader_sees_monotonic_versions_across_snapshots() {
    loom::model(|| {
        let watch: Arc<LoomWatch<usize>> = Arc::new(LoomWatch::new(0));

        let watch_w = Arc::clone(&watch);
        let writer = thread::spawn(move || {
            watch_w.send(1);
        });

        let watch_r = Arc::clone(&watch);
        let reader = thread::spawn(move || {
            let (_, v0, _) = watch_r.snapshot();
            let (_, v1, _) = watch_r.snapshot();
            let (_, v2, _) = watch_r.snapshot();
            (v0, v1, v2)
        });

        writer.join().unwrap();
        let (v0, v1, v2) = reader.join().unwrap();

        assert!(
            v0 <= v1 && v1 <= v2,
            "monotonicity violated: v0={v0} v1={v1} v2={v2}"
        );
        assert!(v2 <= 1, "no version > writer count");
    });
}

/// ft-r51h4: close is sticky. Once a close has linearized, a
/// subsequent send (even from a buggy caller that holds onto a
/// Sender after the close path fired) must NOT clear the closed
/// flag. The flag is monotonic: false → true; never true → false.
#[test]
fn loom_watch_close_is_sticky_against_late_send() {
    loom::model(|| {
        let watch: Arc<LoomWatch<usize>> = Arc::new(LoomWatch::new(0));

        // Linearize close FIRST so the test pins the post-close
        // invariant. A multi-thread interleaving where send wins
        // would be a different equivalence class (covered by the
        // skeleton + concurrent-writers proof).
        watch.close();

        let watch_w = Arc::clone(&watch);
        let late_writer = thread::spawn(move || {
            watch_w.send(99);
        });

        late_writer.join().unwrap();

        let (value, _version, closed) = watch.snapshot();
        // The send updated the value (the model permits this; the
        // real Sender drop semantics make this unreachable in
        // production, but the model documents what the type
        // structurally guarantees vs. what only the borrow checker
        // enforces).
        assert_eq!(value, 99);
        // The critical invariant: closed remains true even after
        // the post-close send linearized.
        assert!(closed, "close must be sticky against post-close sends");
    });
}
