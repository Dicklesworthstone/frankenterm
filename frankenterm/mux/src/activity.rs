//! Keeps track of the number of user-initiated activities
use crate::Mux;
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

static COUNT: AtomicUsize = AtomicUsize::new(0);
static UNOWNED_COUNT: AtomicUsize = AtomicUsize::new(0);

fn try_increment_count(counter: &AtomicUsize) -> bool {
    let mut current = counter.load(Ordering::SeqCst);
    loop {
        let Some(next) = current.checked_add(1) else {
            return false;
        };
        match counter.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

fn try_decrement_count(counter: &AtomicUsize) -> Option<usize> {
    let mut current = counter.load(Ordering::SeqCst);
    loop {
        let next = current.checked_sub(1)?;
        match counter.compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Some(next),
            Err(actual) => current = actual,
        }
    }
}

/// The scheduling phase of the one exact dispatch admitted for this mux.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchPhase {
    /// The claim exists, but no runnable has accepted it yet.
    NeedsSchedule,
    /// A runnable owns the claim and is queued or executing its one pass.
    QueuedOrRunning,
}

/// Exact authority for one logical dispatch and its bounded recovery attempt.
///
/// Pointer identity, rather than a wrapping integer, prevents a late runnable
/// from retiring a newer claim after scheduler rejection or re-entrancy.
struct DispatchClaim {
    /// Request epoch current when this logical dispatch was admitted.
    admitted_epoch: u128,
    /// The first dropped runnable may spend this bit on one recovery attempt.
    retry_remaining: bool,
}

struct ActiveDispatch {
    claim: Arc<DispatchClaim>,
    phase: DispatchPhase,
}

#[derive(Default)]
struct ActivityPruneInner {
    /// Increments for every exact zero-activity transition.
    requested_epoch: u128,
    /// Latest request epoch observed by a completed prune pass.
    completed_epoch: u128,
    /// At most one exact queued, running, or not-yet-scheduled dispatch.
    active: Option<ActiveDispatch>,
    /// Serializes calls into the scheduler, including synchronous rejection.
    driving_scheduler: bool,
}

/// Coalesces exact-owner prune requests without retaining the owning mux.
///
/// This state machine is deliberately mutex-backed. Activity zero transitions
/// are rare, while the failure interleavings are subtle: scheduler callbacks
/// may synchronously drop or run a runnable, may panic, and may race a newer
/// request with an exhausted retry. The mutex makes admission, retirement, and
/// request-epoch comparison one linearizable transition.
#[derive(Default)]
pub(crate) struct ActivityPruneState {
    inner: Mutex<ActivityPruneInner>,
}

impl ActivityPruneState {
    fn lock(&self) -> MutexGuard<'_, ActivityPruneInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn advance_request_epoch(inner: &mut ActivityPruneInner) -> u128 {
        let Some(next) = inner.requested_epoch.checked_add(1) else {
            // Reaching this requires more zero transitions than can be
            // represented during the lifetime of the process. Preserve the
            // existing pending request rather than wrapping into false equality.
            log::error!("activity prune request epoch overflow; preserving pending intent");
            return inner.requested_epoch;
        };
        inner.requested_epoch = next;
        next
    }

    fn admit(inner: &mut ActivityPruneInner, admitted_epoch: u128, retry_remaining: bool) {
        debug_assert!(inner.active.is_none());
        inner.active = Some(ActiveDispatch {
            claim: Arc::new(DispatchClaim {
                admitted_epoch,
                retry_remaining,
            }),
            phase: DispatchPhase::NeedsSchedule,
        });
    }

    fn request(self: &Arc<Self>, owner: Weak<Mux>) {
        if owner.strong_count() == 0 {
            return;
        }

        {
            let mut inner = self.lock();
            let requested_epoch = Self::advance_request_epoch(&mut inner);
            if inner.active.is_none() {
                Self::admit(&mut inner, requested_epoch, true);
            }
        }
        // Always drive, even when this request found an existing claim. A
        // scheduler may have been unavailable for that earlier claim.
        self.drive_scheduler(owner);
    }

    fn begin_pass(&self, claim: &Arc<DispatchClaim>) -> Option<u128> {
        let inner = self.lock();
        let active = inner.active.as_ref()?;
        if active.phase != DispatchPhase::QueuedOrRunning || !Arc::ptr_eq(&active.claim, claim) {
            return None;
        }
        // One pass covers everything requested before it starts. Anything
        // requested during the pass receives a distinct hand-off runnable.
        Some(inner.requested_epoch)
    }

    fn finish_pass(&self, claim: &Arc<DispatchClaim>, covered_epoch: u128) -> bool {
        let mut inner = self.lock();
        let Some(active) = inner.active.as_ref() else {
            return false;
        };
        if !Arc::ptr_eq(&active.claim, claim) {
            return false;
        }

        inner.completed_epoch = inner.completed_epoch.max(covered_epoch);
        inner.active = None;
        if inner.requested_epoch > covered_epoch {
            let requested_epoch = inner.requested_epoch;
            Self::admit(&mut inner, requested_epoch, true);
            true
        } else {
            false
        }
    }

    fn owner_gone(&self, claim: &Arc<DispatchClaim>) {
        let mut inner = self.lock();
        if inner
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(&active.claim, claim))
        {
            inner.active = None;
        }
    }

    /// Retire a dropped or scheduler-rejected claim and decide whether another
    /// exact claim needs scheduling.
    ///
    /// The first drop spends the claim's one recovery bit. If that recovery is
    /// also dropped, only a request newer than the original logical admission
    /// can create a fresh claim. Thus a request racing the second drop cannot
    /// disappear, while a rejecting scheduler cannot recurse forever without
    /// new external work.
    fn dispatch_failed(&self, claim: &Arc<DispatchClaim>, owner: &Weak<Mux>) -> bool {
        let mut inner = self.lock();
        let Some(active) = inner.active.as_ref() else {
            return false;
        };
        if !Arc::ptr_eq(&active.claim, claim) {
            return false;
        }

        inner.active = None;
        if owner.strong_count() == 0 {
            return false;
        }

        if claim.retry_remaining {
            Self::admit(&mut inner, claim.admitted_epoch, false);
            true
        } else if inner.requested_epoch > claim.admitted_epoch {
            let requested_epoch = inner.requested_epoch;
            Self::admit(&mut inner, requested_epoch, true);
            true
        } else {
            false
        }
    }

    fn spawn_claimed_dispatch(self: &Arc<Self>, owner: Weak<Mux>, claim: Arc<DispatchClaim>) {
        let dispatch = ActivityPruneDispatch {
            owner,
            state: Arc::clone(self),
            claim,
            completed: false,
        };
        promise::spawn::spawn_into_main_thread(async move {
            dispatch.execute();
        })
        .detach();
    }

    /// Drive all synchronously-created scheduling work iteratively.
    ///
    /// `driving_scheduler` prevents a runnable dropped synchronously by the
    /// scheduler from recursively scheduling its retry in the destructor call
    /// stack. The outer driver observes the replacement claim and performs the
    /// one bounded retry after that destructor has returned.
    fn drive_scheduler(self: &Arc<Self>, owner: Weak<Mux>) {
        if std::thread::panicking() {
            return;
        }

        {
            let mut inner = self.lock();
            if inner.driving_scheduler {
                return;
            }
            inner.driving_scheduler = true;
        }

        struct DriveReset<'a> {
            state: &'a ActivityPruneState,
            armed: bool,
        }

        impl Drop for DriveReset<'_> {
            fn drop(&mut self) {
                if self.armed {
                    self.state.lock().driving_scheduler = false;
                }
            }
        }

        let mut reset = DriveReset {
            state: self,
            armed: true,
        };
        loop {
            if owner.strong_count() == 0 {
                let mut inner = self.lock();
                inner.active = None;
                inner.driving_scheduler = false;
                reset.armed = false;
                return;
            }
            if !promise::spawn::is_scheduler_configured() {
                // Retain NeedsSchedule. A later request will drive it after the
                // embedding application installs its main-thread scheduler.
                self.lock().driving_scheduler = false;
                reset.armed = false;
                return;
            }

            let claim = {
                let mut inner = self.lock();
                let Some(active) = inner.active.as_mut() else {
                    inner.driving_scheduler = false;
                    reset.armed = false;
                    return;
                };
                if active.phase != DispatchPhase::NeedsSchedule {
                    // Release admission while holding the same mutex used by
                    // dispatch_failed. A concurrent Drop therefore either
                    // installs NeedsSchedule before this observation (and this
                    // loop handles it) or observes driving_scheduler=false and
                    // starts the next driver itself.
                    inner.driving_scheduler = false;
                    reset.armed = false;
                    return;
                }
                active.phase = DispatchPhase::QueuedOrRunning;
                Arc::clone(&active.claim)
            };

            let schedule_result = catch_recoverable(
                RecoverablePanicSite::MuxActivityScheduler,
                std::panic::AssertUnwindSafe({
                    let owner = owner.clone();
                    let state = Arc::clone(self);
                    let claim = Arc::clone(&claim);
                    move || state.spawn_claimed_dispatch(owner, claim)
                }),
            );
            if schedule_result.is_err() {
                log::error!("activity prune scheduler panicked; retaining bounded recovery intent");
                // The future normally drops while the scheduler panic unwinds,
                // and its destructor performs this transition. Keep this
                // idempotent fallback for scheduler implementations that retain
                // the future despite propagating a panic.
                self.dispatch_failed(&claim, &owner);
            }
            // If the scheduler accepted the claim, it remains queued/running
            // and this driver returns. If it synchronously dropped or panicked,
            // Drop/fallback installed a NeedsSchedule replacement and the loop
            // performs that bounded attempt without recursive scheduling.
        }
    }
}

struct ActivityPruneDispatch {
    owner: Weak<Mux>,
    state: Arc<ActivityPruneState>,
    claim: Arc<DispatchClaim>,
    completed: bool,
}

impl ActivityPruneDispatch {
    fn execute(mut self) {
        let Some(covered_epoch) = self.state.begin_pass(&self.claim) else {
            self.completed = true;
            return;
        };
        let Some(owner) = self.owner.upgrade() else {
            self.state.owner_gone(&self.claim);
            self.completed = true;
            return;
        };
        owner.prune_dead_windows();
        drop(owner);
        let needs_handoff = self.state.finish_pass(&self.claim, covered_epoch);
        self.completed = true;
        if needs_handoff {
            self.state.drive_scheduler(self.owner.clone());
        }
    }
}

impl Drop for ActivityPruneDispatch {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let panicking = std::thread::panicking();
        let needs_recovery = self.state.dispatch_failed(&self.claim, &self.owner);
        self.completed = true;
        if needs_recovery && !panicking {
            self.state.drive_scheduler(self.owner.clone());
        }
    }
}

/// Create and hold on to an Activity while you are processing
/// the direct result of a user initiated action, such as preparing
/// to open a window.
/// Once you have opened the window, drop the activity.
/// The activity is used to keep the frontend alive even if there
/// may be no windows present in the mux.
pub struct Activity {
    counted: bool,
    scope_counter: Option<Arc<AtomicUsize>>,
    prune_owner: Option<Weak<Mux>>,
    prune_state: Option<Arc<ActivityPruneState>>,
}

impl Activity {
    /// Create an activity bound to the exact mux that is global now.
    ///
    /// If no mux exists yet, the activity remains explicitly unowned for its
    /// full lifetime: it contributes to [`Activity::count`] but never gates or
    /// later prunes a mux that did not exist when authority was captured.
    pub fn new() -> Self {
        if let Some(owner) = Mux::try_get() {
            Self::new_for_mux(&owner)
        } else {
            Self::new_with_scope(None)
        }
    }

    /// Create an activity bound to one exact mux instance.
    pub fn new_for_mux(owner: &Arc<Mux>) -> Self {
        Self::new_with_scope(Some(owner))
    }

    fn new_with_scope(owner: Option<&Arc<Mux>>) -> Self {
        let scope_counter = owner.map(|owner| Arc::clone(&owner.activity_count));
        let prune_state = owner.map(|owner| Arc::clone(&owner.activity_prune_state));
        let scope = scope_counter.as_deref().unwrap_or(&UNOWNED_COUNT);
        let counted = if try_increment_count(&COUNT) {
            if try_increment_count(scope) {
                true
            } else {
                if try_decrement_count(&COUNT).is_none() {
                    log::error!("activity total counter underflowed while rolling back admission");
                }
                false
            }
        } else {
            false
        };
        Self {
            counted,
            scope_counter,
            prune_owner: owner.map(Arc::downgrade),
            prune_state,
        }
    }

    pub fn count() -> usize {
        COUNT.load(Ordering::SeqCst)
    }

    /// Return the number of activities owned by this exact mux.
    ///
    /// Explicitly unowned pre-mux activities and activities belonging to other
    /// mux instances are excluded. Callers can therefore make lifecycle
    /// decisions without importing mutable process-global authority.
    pub fn count_for_mux(owner: &Mux) -> usize {
        owner.activity_count.load(Ordering::SeqCst)
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if !self.counted {
            return;
        }
        self.counted = false;

        if try_decrement_count(&COUNT).is_none() {
            log::error!("activity total counter underflow; refusing to decrement its scope");
            return;
        }
        let scope = self.scope_counter.as_deref().unwrap_or(&UNOWNED_COUNT);
        let Some(scope_remaining) = try_decrement_count(scope) else {
            // This Activity is gone and its total token was successfully
            // released. Restoring COUNT here would mint a permanent ghost
            // activity with no owner capable of releasing it.
            log::error!("activity scope counter underflow after releasing total counter");
            return;
        };
        if scope_remaining != 0 {
            return;
        }

        let (owner, state) = match (self.prune_owner.take(), self.prune_state.take()) {
            (Some(owner), Some(state)) => (owner, state),
            (None, None) => {
                // An activity created before any mux existed must not acquire a
                // later global mux merely because that singleton changed while
                // the activity was alive.
                return;
            }
            _ => {
                log::error!("activity prune owner/state invariant was violated");
                return;
            }
        };
        state.request(owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MuxNotification;
    use std::sync::atomic::AtomicBool;

    fn capture_scheduler() -> std::sync::mpsc::Receiver<promise::spawn::Runnable> {
        let (sender, receiver) = std::sync::mpsc::channel();
        let low_priority_sender = sender.clone();
        promise::spawn::set_schedulers(
            Box::new(move |runnable| {
                let _ = sender.send(runnable);
            }),
            Box::new(move |runnable| {
                let _ = low_priority_sender.send(runnable);
            }),
        );
        receiver
    }

    fn assert_prune_idle(state: &ActivityPruneState) {
        let inner = state.lock();
        assert_eq!(
            inner.requested_epoch, inner.completed_epoch,
            "an idle state must have observed every request"
        );
        assert!(inner.active.is_none(), "an idle state must have no claim");
        assert!(
            !inner.driving_scheduler,
            "an idle state must not retain scheduler admission"
        );
    }

    fn assert_pending_without_dispatch(state: &ActivityPruneState) {
        let inner = state.lock();
        assert!(
            inner.requested_epoch > inner.completed_epoch,
            "a rejected dispatch must preserve an unobserved request"
        );
        assert!(
            inner.active.is_none(),
            "an exhausted bounded retry must release its dispatch claim"
        );
        assert!(
            !inner.driving_scheduler,
            "an exhausted bounded retry must release scheduler admission"
        );
    }

    #[test]
    fn try_increment_count_refuses_to_wrap() {
        let counter = AtomicUsize::new(usize::MAX);

        assert!(!try_increment_count(&counter));
        assert_eq!(counter.load(Ordering::SeqCst), usize::MAX);
    }

    #[test]
    fn try_decrement_count_refuses_to_underflow() {
        let counter = AtomicUsize::new(0);

        assert_eq!(try_decrement_count(&counter), None);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn scope_underflow_does_not_restore_a_ghost_total_activity() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let baseline = Activity::count();
        assert!(try_increment_count(&COUNT));
        let inconsistent = Activity {
            counted: true,
            scope_counter: Some(Arc::new(AtomicUsize::new(0))),
            prune_owner: None,
            prune_state: None,
        };

        drop(inconsistent);
        assert_eq!(
            Activity::count(),
            baseline,
            "the released total token must not be recreated without an owner"
        );
    }

    #[test]
    fn exact_owner_burst_schedules_only_on_one_to_zero_transition() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let mux = Arc::new(Mux::new(None));
        let mut activities = (0..64)
            .map(|_| Activity::new_for_mux(&mux))
            .collect::<Vec<_>>();
        let last = activities.pop().expect("burst should contain activities");

        drop(activities);
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "non-final drops must not enqueue a prune"
        );

        drop(last);
        let runnable = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the exact counter's one-to-zero transition should enqueue one prune");
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "one activity burst must enqueue only one prune"
        );
        runnable.run();
        assert_prune_idle(&mux.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn repeated_idle_waves_coalesce_behind_one_pending_dispatch() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let mux = Arc::new(Mux::new(None));

        drop(Activity::new_for_mux(&mux));
        for _ in 0..64 {
            drop(Activity::new_for_mux(&mux));
        }

        let runnable = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the first idle transition should enqueue a prune");
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "idle waves queued before dispatch must coalesce"
        );
        runnable.run();
        assert_prune_idle(&mux.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn activity_drop_during_prune_redirties_the_running_dispatch() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let mux = Arc::new(Mux::new(None));
        let empty_events = Arc::new(AtomicUsize::new(0));
        let empty_events_for_subscriber = Arc::clone(&empty_events);
        let reentered = Arc::new(AtomicBool::new(false));
        let reentered_for_subscriber = Arc::clone(&reentered);
        let mux_for_subscriber = Arc::downgrade(&mux);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::Empty) {
                empty_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
                if !reentered_for_subscriber.swap(true, Ordering::SeqCst) {
                    let mux = mux_for_subscriber
                        .upgrade()
                        .expect("test mux should outlive its subscriber");
                    drop(Activity::new_for_mux(&mux));
                }
            }
            true
        })
        .expect("test subscriber identifier");

        drop(Activity::new_for_mux(&mux));
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("initial idle transition should enqueue a prune")
            .run();

        assert_eq!(
            empty_events.load(Ordering::SeqCst),
            1,
            "one runnable must perform at most one prune pass"
        );
        let follow_up = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a transition racing the active scan must receive a hand-off runnable");
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "single-flight hand-off must queue exactly one runnable"
        );
        follow_up.run();
        assert_eq!(
            empty_events.load(Ordering::SeqCst),
            2,
            "the hand-off runnable must observe the re-entrant transition"
        );
        assert_prune_idle(&mux.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn sustained_reentrant_activity_yields_between_prune_passes() {
        const PASSES: usize = 8;

        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let mux = Arc::new(Mux::new(None));
        let empty_events = Arc::new(AtomicUsize::new(0));
        let empty_events_for_subscriber = Arc::clone(&empty_events);
        let mux_for_subscriber = Arc::downgrade(&mux);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::Empty) {
                let pass = empty_events_for_subscriber.fetch_add(1, Ordering::SeqCst) + 1;
                if pass < PASSES {
                    let mux = mux_for_subscriber
                        .upgrade()
                        .expect("test mux should outlive its subscriber");
                    drop(Activity::new_for_mux(&mux));
                }
            }
            true
        })
        .expect("test subscriber identifier");

        drop(Activity::new_for_mux(&mux));
        for expected_pass in 1..=PASSES {
            let runnable = receiver
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("each re-entrant transition should receive one runnable");
            runnable.run();
            assert_eq!(
                empty_events.load(Ordering::SeqCst),
                expected_pass,
                "one runnable must never consume multiple prune passes inline"
            );
        }
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "the final pass must release single-flight admission"
        );
        assert_prune_idle(&mux.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn dropped_prune_runnable_gets_one_bounded_retry() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let mux = Arc::new(Mux::new(None));

        drop(Activity::new_for_mux(&mux));
        let rejected = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("initial idle transition should enqueue a prune");
        drop(rejected);

        let retry = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("dropping the first runnable should enqueue one bounded retry");
        drop(retry);
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "drop recovery must not create an unbounded retry cascade"
        );
        assert_pending_without_dispatch(&mux.activity_prune_state);

        drop(Activity::new_for_mux(&mux));
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a later idle transition should recover preserved prune intent")
            .run();
        assert_prune_idle(&mux.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn request_while_bounded_retry_is_pending_survives_second_drop() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let mux = Arc::new(Mux::new(None));

        drop(Activity::new_for_mux(&mux));
        let first = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("initial zero transition should enqueue a dispatch");
        drop(first);
        let bounded_retry = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("first drop should enqueue its one bounded retry");

        // This request linearizes while the bounded retry owns admission. In
        // the former split-atomic state machine it could observe scheduled=true
        // immediately before the second Drop cleared it, leaving dirty=true
        // with no runnable.
        drop(Activity::new_for_mux(&mux));
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "the pending retry must retain single-flight admission"
        );

        drop(bounded_retry);
        let newer_request = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the newer request must receive a fresh logical dispatch");
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "a newer request receives one claim, not a retry cascade"
        );
        newer_request.run();
        assert_prune_idle(&mux.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn scheduler_panic_is_caught_outside_dispatch_drop_and_retry_is_bounded() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let mux = Arc::new(Mux::new(None));
        let attempts = Arc::new(AtomicUsize::new(0));
        let high_attempts = Arc::clone(&attempts);
        promise::spawn::set_schedulers(
            Box::new(move |_runnable| {
                high_attempts.fetch_add(1, Ordering::SeqCst);
                panic!("intentional activity prune scheduler panic");
            }),
            Box::new(|_runnable| {
                panic!("activity pruning must not use the low-priority scheduler");
            }),
        );

        let drop_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(Activity::new_for_mux(&mux));
        }));
        assert!(
            drop_result.is_ok(),
            "a scheduler panic must not escape Activity::drop"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "one admitted dispatch receives exactly one bounded recovery attempt"
        );
        assert_pending_without_dispatch(&mux.activity_prune_state);

        let receiver = capture_scheduler();
        drop(Activity::new_for_mux(&mux));
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a later transition should recover the retained request")
            .run();
        assert_prune_idle(&mux.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn synchronous_scheduler_drop_recovery_is_iterative_not_recursive() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let mux = Arc::new(Mux::new(None));
        let attempts = Arc::new(AtomicUsize::new(0));
        let depth = Arc::new(AtomicUsize::new(0));
        let max_depth = Arc::new(AtomicUsize::new(0));
        let high_attempts = Arc::clone(&attempts);
        let high_depth = Arc::clone(&depth);
        let high_max_depth = Arc::clone(&max_depth);
        promise::spawn::set_schedulers(
            Box::new(move |runnable| {
                high_attempts.fetch_add(1, Ordering::SeqCst);
                let current_depth = high_depth.fetch_add(1, Ordering::SeqCst) + 1;
                high_max_depth.fetch_max(current_depth, Ordering::SeqCst);
                drop(runnable);
                high_depth.fetch_sub(1, Ordering::SeqCst);
            }),
            Box::new(drop),
        );

        drop(Activity::new_for_mux(&mux));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "a synchronous rejection receives one bounded retry"
        );
        assert_eq!(
            max_depth.load(Ordering::SeqCst),
            1,
            "the retry must begin only after the rejecting callback returns"
        );
        assert_pending_without_dispatch(&mux.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn legacy_new_burst_binds_the_current_exact_mux_once() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let empty_events = Arc::new(AtomicUsize::new(0));
        let empty_events_for_subscriber = Arc::clone(&empty_events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::Empty) {
                empty_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("test subscriber identifier");

        let mut activities = (0..32).map(|_| Activity::new()).collect::<Vec<_>>();
        let last = activities
            .pop()
            .expect("legacy constructor burst should contain activities");
        drop(activities);
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(last);
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("final exact transition should enqueue one prune")
            .run();

        assert_eq!(empty_events.load(Ordering::SeqCst), 1);
        assert!(matches!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert_prune_idle(&mux.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn legacy_activity_keeps_creation_owner_across_global_swap() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let origin = Arc::new(Mux::new(None));
        let replacement = Arc::new(Mux::new(None));
        Mux::set_mux(&origin);

        let origin_empty = Arc::new(AtomicUsize::new(0));
        let origin_empty_for_subscriber = Arc::clone(&origin_empty);
        origin
            .subscribe(move |notification| {
                if matches!(notification, MuxNotification::Empty) {
                    origin_empty_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("origin subscriber identifier");

        let replacement_empty = Arc::new(AtomicUsize::new(0));
        let replacement_empty_for_subscriber = Arc::clone(&replacement_empty);
        replacement
            .subscribe(move |notification| {
                if matches!(notification, MuxNotification::Empty) {
                    replacement_empty_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("replacement subscriber identifier");

        let exact_origin = Activity::new_for_mux(&origin);
        let legacy_origin = Activity::new();
        assert_eq!(Activity::count_for_mux(&origin), 2);
        assert_eq!(Activity::count_for_mux(&replacement), 0);

        drop(exact_origin);
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "the first of two origin activities must not request pruning"
        );

        Mux::set_mux(&replacement);
        drop(legacy_origin);
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("legacy activity must request its creation owner after a global swap")
            .run();

        assert_eq!(origin_empty.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_empty.load(Ordering::SeqCst), 0);
        assert_prune_idle(&origin.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn truly_pre_mux_activity_never_gates_or_adopts_a_later_mux() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let pre_mux = Activity::new();
        let mux = Arc::new(Mux::new(None));
        let empty_events = Arc::new(AtomicUsize::new(0));
        let empty_events_for_subscriber = Arc::clone(&empty_events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::Empty) {
                empty_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("test subscriber identifier");
        Mux::set_mux(&mux);

        assert_eq!(
            Activity::count_for_mux(&mux),
            0,
            "a pre-mux activity has no authority over a later mux"
        );
        mux.prune_dead_windows();
        assert_eq!(
            empty_events.load(Ordering::SeqCst),
            1,
            "the later mux must not be gated by an unowned activity"
        );

        drop(pre_mux);
        assert!(
            matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "dropping a pre-mux activity must not adopt and prune the later singleton"
        );
        Mux::shutdown();
    }

    #[test]
    fn exact_owner_activity_prunes_origin_after_global_mux_swap() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let origin = Arc::new(Mux::new(None));
        let replacement = Arc::new(Mux::new(None));

        let origin_empty = Arc::new(AtomicUsize::new(0));
        let origin_empty_for_subscriber = Arc::clone(&origin_empty);
        origin
            .subscribe(move |notification| {
                if matches!(notification, MuxNotification::Empty) {
                    origin_empty_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("origin subscriber identifier");

        let replacement_empty = Arc::new(AtomicUsize::new(0));
        let replacement_empty_for_subscriber = Arc::clone(&replacement_empty);
        replacement
            .subscribe(move |notification| {
                if matches!(notification, MuxNotification::Empty) {
                    replacement_empty_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("replacement subscriber identifier");

        let activity = Activity::new_for_mux(&origin);
        Mux::set_mux(&replacement);
        drop(activity);
        executor
            .tick()
            .expect("activity drop should schedule exact-owner pruning");

        assert_eq!(origin_empty.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_empty.load(Ordering::SeqCst), 0);
        Mux::shutdown();
    }

    #[test]
    fn exact_owner_activity_does_not_gate_an_unrelated_mux() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let origin = Arc::new(Mux::new(None));
        let unrelated = Arc::new(Mux::new(None));
        let unrelated_empty = Arc::new(AtomicUsize::new(0));
        let unrelated_empty_for_subscriber = Arc::clone(&unrelated_empty);
        unrelated
            .subscribe(move |notification| {
                if matches!(notification, MuxNotification::Empty) {
                    unrelated_empty_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("unrelated subscriber identifier");

        let activity = Activity::new_for_mux(&origin);
        unrelated.prune_dead_windows();

        assert_eq!(
            unrelated_empty.load(Ordering::SeqCst),
            1,
            "an activity scoped to one mux must not suppress another mux's pruning",
        );
        drop(activity);
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("origin activity drop should enqueue exact-owner pruning")
            .run();
        assert_prune_idle(&origin.activity_prune_state);
        Mux::shutdown();
    }

    #[test]
    fn queued_prune_uses_weak_owner_and_is_collectable_after_owner_death() {
        let _guard = crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Mux::shutdown();
        let receiver = capture_scheduler();
        let mux = Arc::new(Mux::new(None));
        let owner = Arc::downgrade(&mux);
        let prune_state = Arc::downgrade(&mux.activity_prune_state);

        drop(Activity::new_for_mux(&mux));
        let runnable = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("zero transition should enqueue a weak-owner dispatch");
        drop(mux);

        assert!(
            owner.upgrade().is_none(),
            "the queued dispatch must not retain its owning mux"
        );
        assert!(
            prune_state.upgrade().is_some(),
            "the queued dispatch retains only its small state until execution"
        );

        runnable.run();
        assert!(
            prune_state.upgrade().is_none(),
            "owner-gone execution must release the final prune-state reference"
        );
        Mux::shutdown();
    }
}
