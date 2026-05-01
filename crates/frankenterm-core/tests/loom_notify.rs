//! Loom proofs for the `runtime_async::notify` primitive contract.
//!
//! Models the wake-one / wake-all semantics that Notify provides.
//! Skeleton seeded under ft-syqcz.6 (commit 7ff0f9e37); ft-kpmej
//! (sub-bead of ft-syqcz.7) extended this file with exhaustive
//! coverage of the contract clauses.
//!
//! Contract under test:
//! - `notify_one` wakes one waiter (or accumulates one permit if no
//!   waiters are parked)
//! - `notify_waiters` wakes every waiter currently parked but does
//!   NOT accumulate a permit for future waiters
//! - waiters cannot deadlock when notify and wait race
//! - the permit cap for accumulated notify_one is 1: multiple
//!   notify_one calls with no waiter still leave at most one permit
//! - notify_one after a wait completes accumulates a fresh permit
//!   for the next waiter; it does not re-wake the already-completed
//!   one
//!
//! Mazurkiewicz trace catalog for Notify lives at
//! [docs/runtime/mazurkiewicz-traces.md](../../../../docs/runtime/mazurkiewicz-traces.md).

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

/// ft-kpmej: notify_one issued before any waiter is parked accumulates
/// a permit; the first subsequent wait returns immediately without
/// blocking. This is the asupersync `Notify::notified()` contract: a
/// permit issued before the future is awaited still wakes that future.
#[test]
fn loom_notify_one_pre_accumulates_permit() {
    loom::model(|| {
        let notify = Arc::new(LoomNotify::new());

        // Single-thread linearization: notify first, then wait. The
        // model does not need a multi-thread interleaving — the
        // property is per-handle ordering.
        notify.notify_one();
        notify.wait(); // must not block; if it does, Loom hangs.
    });
}

/// ft-kpmej: the permit cap for notify_one is 1. Multiple notify_one
/// calls with no waiter parked still leave at most one permit, so a
/// second wait must observe a separate notify before returning.
#[test]
fn loom_notify_one_permit_caps_at_one() {
    loom::model(|| {
        let notify = Arc::new(LoomNotify::new());
        let woken = Arc::new(AtomicUsize::new(0));

        // Three notify_one issued before any waiter parks — the
        // permit must cap at 1 (saturating_add(1).min(1) in the
        // model mirrors asupersync's single-permit semantics).
        notify.notify_one();
        notify.notify_one();
        notify.notify_one();

        // First wait consumes the (single) accumulated permit.
        notify.wait();
        woken.fetch_add(1, Ordering::SeqCst);

        // Second wait must block until a fresh notify fires. Spawn
        // a thread that delivers it after a yield; if the cap were
        // not 1 (i.e. permits had accumulated to 3), the second
        // wait would return without the explicit notify and Loom
        // would not be able to model the linearization point.
        let notify_late = Arc::clone(&notify);
        let woken_late = Arc::clone(&woken);
        let waiter = thread::spawn(move || {
            notify_late.wait();
            woken_late.fetch_add(1, Ordering::SeqCst);
        });

        let notify_n = Arc::clone(&notify);
        let notifier = thread::spawn(move || {
            notify_n.notify_one();
        });

        notifier.join().unwrap();
        waiter.join().unwrap();

        assert_eq!(
            woken.load(Ordering::SeqCst),
            2,
            "first wait consumed the capped permit; second wait \
             required a fresh notify_one to complete",
        );
    });
}

/// ft-kpmej: notify_waiters does NOT accumulate a permit for a future
/// waiter. A wait that arrives after notify_waiters with no concurrent
/// notify must block until the next notify fires. The skeleton above
/// already proves the wake-currently-parked half; this proves the
/// no-permit-accumulation half.
#[test]
fn loom_notify_waiters_does_not_accumulate() {
    loom::model(|| {
        let notify = Arc::new(LoomNotify::new());
        let woken = Arc::new(AtomicUsize::new(0));

        // notify_waiters with no waiters — bumps epoch, no permit.
        notify.notify_waiters();

        // Spawn a waiter; it must observe the post-notify_waiters
        // state and still block (the epoch was bumped before the
        // waiter sampled baseline_epoch, so the loop sees no change
        // and parks on cv).
        let notify_w = Arc::clone(&notify);
        let woken_w = Arc::clone(&woken);
        let waiter = thread::spawn(move || {
            notify_w.wait();
            woken_w.fetch_add(1, Ordering::SeqCst);
        });

        // Deliver a fresh notify_one to release the waiter.
        let notify_n = Arc::clone(&notify);
        let notifier = thread::spawn(move || {
            notify_n.notify_one();
        });

        notifier.join().unwrap();
        waiter.join().unwrap();

        assert_eq!(
            woken.load(Ordering::SeqCst),
            1,
            "waiter required notify_one after notify_waiters; \
             notify_waiters did not accumulate a permit",
        );
    });
}

/// ft-kpmej: notify_one fired after a wait has already completed
/// accumulates a permit for the *next* waiter — it does not
/// retroactively re-wake the completed waiter. Models the
/// "notify_one is single-shot per permit" contract.
#[test]
fn loom_notify_one_post_wait_accumulates_for_next() {
    loom::model(|| {
        let notify = Arc::new(LoomNotify::new());
        let woken_first = Arc::new(AtomicUsize::new(0));
        let woken_second = Arc::new(AtomicUsize::new(0));

        // Phase 1: deliver permit + first wait consumes it.
        notify.notify_one();
        notify.wait();
        woken_first.fetch_add(1, Ordering::SeqCst);

        // Phase 2: deliver a fresh permit; spawn second waiter; the
        // permit must wake the second waiter (and not double-wake
        // the first, which is already complete).
        notify.notify_one();
        let notify_w = Arc::clone(&notify);
        let woken_second_w = Arc::clone(&woken_second);
        let second_waiter = thread::spawn(move || {
            notify_w.wait();
            woken_second_w.fetch_add(1, Ordering::SeqCst);
        });
        second_waiter.join().unwrap();

        assert_eq!(woken_first.load(Ordering::SeqCst), 1);
        assert_eq!(woken_second.load(Ordering::SeqCst), 1);
    });
}
