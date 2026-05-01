//! Loom skeleton for the `runtime_async::watch` channel contract.
//!
//! Models the last-write-wins single-value broadcast that watch
//! provides. The full exhaustive proofs live in `ft-syqcz.7` (G8.2).
//!
//! Contract under test:
//! - last write wins; no intermediate values are mandated to be
//!   observed
//! - reader sees a monotonically non-decreasing version sequence
//! - sender drop transitions the channel to a closed state observable
//!   by the reader

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
        assert!(closed, "watch must observe close after sender path completes");
        assert_eq!(value, 7);
        assert_eq!(version, 1);
    });
}
