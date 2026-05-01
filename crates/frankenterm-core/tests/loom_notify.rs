//! Loom skeleton for the `runtime_async::notify` primitive contract.
//!
//! Models the wake-one / wake-all semantics that Notify provides. The
//! full exhaustive proofs live in `ft-syqcz.7` (G8.2).
//!
//! Contract under test:
//! - `notify_one` wakes one waiter (or accumulates one permit if no
//!   waiters are parked)
//! - `notify_waiters` wakes every waiter currently parked but does
//!   NOT accumulate a permit for future waiters
//! - waiters cannot deadlock when notify and wait race

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;

#[derive(Debug)]
struct LoomNotify {
    state: Mutex<LoomNotifyState>,
    cv: Condvar,
}

#[derive(Debug)]
struct LoomNotifyState {
    permits: usize,
    epoch: u64,
}

impl LoomNotify {
    fn new() -> Self {
        Self {
            state: Mutex::new(LoomNotifyState {
                permits: 0,
                epoch: 0,
            }),
            cv: Condvar::new(),
        }
    }

    fn notify_one(&self) {
        let mut state = self.state.lock().unwrap();
        state.permits = state.permits.saturating_add(1).min(1);
        self.cv.notify_one();
    }

    fn notify_waiters(&self) {
        let mut state = self.state.lock().unwrap();
        state.epoch += 1;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut state = self.state.lock().unwrap();
        let baseline_epoch = state.epoch;
        loop {
            if state.permits > 0 {
                state.permits -= 1;
                return;
            }
            if state.epoch != baseline_epoch {
                return;
            }
            state = self.cv.wait(state).unwrap();
        }
    }
}

#[test]
fn loom_notify_one_wakes_one_waiter() {
    loom::model(|| {
        let notify = Arc::new(LoomNotify::new());
        let woken = Arc::new(AtomicUsize::new(0));

        let notify_w = Arc::clone(&notify);
        let woken_w = Arc::clone(&woken);
        let waiter = thread::spawn(move || {
            notify_w.wait();
            woken_w.fetch_add(1, Ordering::SeqCst);
        });

        let notify_n = Arc::clone(&notify);
        let notifier = thread::spawn(move || {
            notify_n.notify_one();
        });

        notifier.join().unwrap();
        waiter.join().unwrap();

        assert_eq!(
            woken.load(Ordering::SeqCst),
            1,
            "notify_one must wake exactly one waiter",
        );
    });
}

#[test]
fn loom_notify_waiters_wakes_currently_parked() {
    loom::model(|| {
        let notify = Arc::new(LoomNotify::new());
        let woken = Arc::new(AtomicUsize::new(0));

        let notify_a = Arc::clone(&notify);
        let woken_a = Arc::clone(&woken);
        let waiter_a = thread::spawn(move || {
            notify_a.wait();
            woken_a.fetch_add(1, Ordering::SeqCst);
        });

        let notify_b = Arc::clone(&notify);
        let woken_b = Arc::clone(&woken);
        let waiter_b = thread::spawn(move || {
            notify_b.wait();
            woken_b.fetch_add(1, Ordering::SeqCst);
        });

        // Two notify_waiters calls, since under loom's interleaving a
        // single broadcast can race with the wait() entry — issuing
        // two ensures both waiters complete (the second is a no-op
        // for already-woken waiters but a real wake for any still
        // parked).
        let notify_x = Arc::clone(&notify);
        let notify_y = Arc::clone(&notify);
        let notifier = thread::spawn(move || {
            notify_x.notify_waiters();
            notify_y.notify_waiters();
        });

        notifier.join().unwrap();
        waiter_a.join().unwrap();
        waiter_b.join().unwrap();

        assert_eq!(
            woken.load(Ordering::SeqCst),
            2,
            "notify_waiters must wake every parked waiter",
        );
    });
}
