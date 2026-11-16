#![recursion_limit = "256"]

pub mod client;
pub mod discovery;
pub mod domain;
pub mod pane;

#[cfg(test)]
use mux::Mux;
#[cfg(test)]
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(test)]
static MUX_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
fn lock_or_recover_and_clear<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            let guard = poisoned.into_inner();
            mutex.clear_poison();
            guard
        }
    }
}

#[cfg(test)]
fn install_mux_test_scheduler() {
    // These synchronous unit tests do not own an event loop. Reject work
    // immediately, on the thread that scheduled it, rather than claiming that
    // an unpumped queue is live. Dropping the runnable cancels detached client
    // work and exercises the mux dispatch guards' inline liveness fallback
    // without moving `spawn_local` futures across threads.
    //
    // A future test that needs scheduled work to execute must extend this
    // serialized harness with a same-thread, explicitly ticked executor. A
    // background pump is invalid for `spawn_local`, and must not replace this
    // fixture.
    promise::spawn::set_schedulers(Box::new(drop), Box::new(drop));
}

#[cfg(test)]
struct RestoreMux(Option<Arc<Mux>>);

#[cfg(test)]
impl RestoreMux {
    fn capture() -> Self {
        Self(Mux::try_get())
    }
}

#[cfg(test)]
impl Drop for RestoreMux {
    fn drop(&mut self) {
        if let Some(prior) = self.0.take() {
            Mux::set_mux(&prior);
        } else {
            Mux::shutdown();
        }
    }
}

/// Process-wide test scope for code that reads or replaces the global mux.
///
/// Acquisition order is deliberately fixed: serialize first, install the
/// process-lifetime scheduler second, and only then capture the ambient mux.
/// On drop, the exact ambient mux is restored before the serialization guard
/// is released.
#[cfg(test)]
pub(crate) struct MuxTestScope {
    restore_mux: Option<RestoreMux>,
    parked_scheduler: Option<promise::spawn::SimpleExecutor>,
    _lock: MutexGuard<'static, ()>,
}

#[cfg(test)]
impl MuxTestScope {
    pub(crate) fn enter() -> Self {
        let lock = lock_or_recover_and_clear(&MUX_TEST_LOCK);
        install_mux_test_scheduler();
        let restore_mux = RestoreMux::capture();
        Self {
            restore_mux: Some(restore_mux),
            parked_scheduler: None,
            _lock: lock,
        }
    }

    /// Enter a mux scope whose bounded main-thread scheduler admits work but
    /// deliberately leaves it parked for the lifetime of the test.
    ///
    /// This is the healthy-transport fixture for tests that register a
    /// `ClientPane`: the production pane-bind hook must see real admission
    /// authority, while its palette RPC must not run without a test peer. The
    /// executor is dropped on this same test thread before the ambient mux is
    /// restored, cancelling every parked future and releasing its admission
    /// permit without leaking work into a later test.
    pub(crate) fn enter_with_parked_main_thread_scheduler() -> Self {
        let lock = lock_or_recover_and_clear(&MUX_TEST_LOCK);
        let parked_scheduler = promise::spawn::SimpleExecutor::new();
        let restore_mux = RestoreMux::capture();
        Self {
            restore_mux: Some(restore_mux),
            parked_scheduler: Some(parked_scheduler),
            _lock: lock,
        }
    }

    pub(crate) fn set_mux(&self, mux: &Arc<Mux>) {
        debug_assert!(
            self.restore_mux.is_some(),
            "an active mux test scope must retain its restore guard"
        );
        Mux::set_mux(mux);
    }
}

#[cfg(test)]
impl Drop for MuxTestScope {
    fn drop(&mut self) {
        // Cancel parked thread-affine futures while their exact mux is still
        // ambient, then restore the prior mux while `_lock` remains held. The
        // explicit takes make this ordering robust to future field reordering.
        drop(self.parked_scheduler.take());
        install_mux_test_scheduler();
        drop(self.restore_mux.take());
    }
}

#[cfg(test)]
mod mux_test_scope_tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn same_mux(left: &Option<Arc<Mux>>, right: &Option<Arc<Mux>>) -> bool {
        match (left, right) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }

    #[test]
    fn poisoned_mutex_is_recovered_and_cleared() {
        let mutex = Mutex::new(0_u8);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _guard = mutex.lock().expect("fresh mutex should lock");
            panic!("poison the private test mutex");
        }));
        assert!(panic.is_err());
        assert!(mutex.is_poisoned());

        {
            let mut guard = lock_or_recover_and_clear(&mutex);
            *guard = 1;
        }

        assert!(!mutex.is_poisoned());
        assert_eq!(*mutex.lock().expect("poison should be cleared"), 1);
    }

    #[test]
    fn rejecting_scheduler_cancels_work_instead_of_stranding_it() {
        let _scope = MuxTestScope::enter();
        let ran = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let probe = DropProbe(Arc::clone(&dropped));

        let outcome = promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Input,
            4 * 1024,
        );
        assert!(matches!(
            outcome,
            promise::spawn::MainThreadReservationOutcome::SchedulerUnavailable
        ));

        assert!(!ran.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst));
        drop(probe);
        assert!(
            dropped.load(Ordering::SeqCst),
            "rejected admission must leave future construction and disposal with the producer"
        );
    }

    #[test]
    fn parked_scheduler_admits_then_cancels_owned_work_on_scope_drop() {
        let accounting_errors_before = promise::spawn::main_thread_admission_accounting_errors();
        let scope = MuxTestScope::enter_with_parked_main_thread_scheduler();
        let ran = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let ran_in_task = Arc::clone(&ran);
        let probe = DropProbe(Arc::clone(&dropped));

        let outcome = promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Topology,
            4 * 1024,
        );
        let reservation = match outcome {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            rejected => {
                panic!(
                    "parked scheduler must provide real bounded admission: {:?}",
                    rejected
                )
            }
        };
        reservation
            .spawn(async move {
                ran_in_task.store(true, Ordering::SeqCst);
                drop(probe);
            })
            .detach();

        assert!(!ran.load(Ordering::SeqCst));
        assert!(!dropped.load(Ordering::SeqCst));
        drop(scope);
        assert!(
            !ran.load(Ordering::SeqCst),
            "parked work must never execute without an explicitly ticked owner"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "scope teardown must cancel and drop every parked future on its owner thread"
        );
        assert!(matches!(
            promise::spawn::try_reserve_main_thread(
                promise::spawn::MainThreadServiceClass::Topology,
                4 * 1024,
            ),
            promise::spawn::MainThreadReservationOutcome::SchedulerUnavailable
        ));
        assert_eq!(
            promise::spawn::main_thread_admission_accounting_errors(),
            accounting_errors_before,
            "parked task cancellation must release its exact permit without accounting drift"
        );
    }

    #[test]
    fn scope_teardown_restores_rejecting_scheduler_after_nested_replacement() {
        let scope = MuxTestScope::enter();
        let nested_scheduler = promise::spawn::SimpleExecutor::new();
        let outcome = promise::spawn::try_reserve_main_thread(
            promise::spawn::MainThreadServiceClass::Interactive,
            4 * 1024,
        );
        let reservation = match outcome {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            rejected => panic!("nested scheduler must admit the probe: {:?}", rejected),
        };
        drop(reservation);
        drop(nested_scheduler);
        drop(scope);

        assert!(matches!(
            promise::spawn::try_reserve_main_thread(
                promise::spawn::MainThreadServiceClass::Interactive,
                4 * 1024,
            ),
            promise::spawn::MainThreadReservationOutcome::SchedulerUnavailable
        ));
    }

    #[test]
    fn restore_mux_preserves_empty_and_exact_ambient_state_through_unwind() {
        let _lock = lock_or_recover_and_clear(&MUX_TEST_LOCK);
        install_mux_test_scheduler();
        let _restore_outer = RestoreMux::capture();

        Mux::shutdown();
        let temporary = Arc::new(Mux::new(None));
        let empty_unwind = catch_unwind(AssertUnwindSafe(|| {
            let _restore = RestoreMux::capture();
            Mux::set_mux(&temporary);
            panic!("exercise empty ambient restoration");
        }));
        assert!(empty_unwind.is_err());
        assert!(Mux::try_get().is_none());

        let ambient = Arc::new(Mux::new(None));
        Mux::set_mux(&ambient);
        let replacement = Arc::new(Mux::new(None));
        let occupied_unwind = catch_unwind(AssertUnwindSafe(|| {
            let _restore = RestoreMux::capture();
            Mux::set_mux(&replacement);
            panic!("exercise occupied ambient restoration");
        }));
        assert!(occupied_unwind.is_err());
        let restored = Mux::try_get().expect("occupied ambient mux should be restored");
        assert!(Arc::ptr_eq(&restored, &ambient));
    }

    #[test]
    fn scope_unwind_restores_mux_and_allows_reentry() {
        let captured = std::sync::Mutex::new(None);
        let panic_mux = Arc::new(Mux::new(None));
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let scope = MuxTestScope::enter();
            *captured.lock().expect("capture mutex should lock") = Some(
                scope
                    .restore_mux
                    .as_ref()
                    .expect("active scope must own a restore guard")
                    .0
                    .clone(),
            );
            scope.set_mux(&panic_mux);
            panic!("exercise full-scope unwind");
        }));
        assert!(unwind.is_err());

        let expected = captured
            .into_inner()
            .expect("capture mutex should not be poisoned")
            .expect("the scope should capture its ambient mux state");

        {
            let scope = MuxTestScope::enter();
            assert!(!MUX_TEST_LOCK.is_poisoned());
            assert!(promise::spawn::is_scheduler_configured());
            assert!(same_mux(&Mux::try_get(), &expected));

            let replacement = Arc::new(Mux::new(None));
            scope.set_mux(&replacement);
            let installed = Mux::try_get().expect("replacement mux should be installed");
            assert!(Arc::ptr_eq(&installed, &replacement));
        }

        // Reacquire before observing the process global. An assertion after
        // releasing the scope would race another default-parallel mux test.
        {
            let _scope = MuxTestScope::enter();
            assert!(!MUX_TEST_LOCK.is_poisoned());
            assert!(same_mux(&Mux::try_get(), &expected));
        }
    }
}
