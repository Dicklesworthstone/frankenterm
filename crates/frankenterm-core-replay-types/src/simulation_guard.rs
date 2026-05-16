//! Shared simulation scope guard for replay and live side-effect barriers.
//!
//! This module lives in the replay-types leaf crate so both `frankenterm-core`
//! live paths and `frankenterm-core-replay` simulation code can depend on the
//! same guard without introducing a Cargo cycle.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! {
    static THREAD_SIMULATION_DEPTH: Cell<u32> = const { Cell::new(0) };
}

static PROCESS_SIMULATION_DEPTH: AtomicU64 = AtomicU64::new(0);

/// RAII guard that marks the current process as running replay simulation.
///
/// The per-thread depth keeps accidental nested enters on the same execution
/// path fail-closed. The process-wide depth is what live side-effect choke
/// points check, because async work may migrate across threads before a leak
/// reaches storage, event fanout, or pane mutation.
pub struct SimulationGuard {
    _private: (),
}

impl SimulationGuard {
    /// Enter simulation mode. Panics if this thread is already simulating.
    #[must_use]
    pub fn enter() -> Self {
        THREAD_SIMULATION_DEPTH.with(|depth| {
            assert_eq!(
                depth.get(),
                0,
                "SimulationGuard::enter called while already in simulation mode"
            );
            depth.set(1);
        });
        PROCESS_SIMULATION_DEPTH.fetch_add(1, Ordering::SeqCst);
        Self { _private: () }
    }

    /// Check whether any replay simulation is active in this process.
    #[must_use]
    pub fn is_active() -> bool {
        Self::active_count() > 0
    }

    /// Number of active simulation guards in this process.
    #[must_use]
    pub fn active_count() -> u64 {
        PROCESS_SIMULATION_DEPTH.load(Ordering::SeqCst)
    }

    /// Check whether the current thread owns an active simulation guard.
    #[must_use]
    pub fn is_thread_active() -> bool {
        THREAD_SIMULATION_DEPTH.with(|depth| depth.get() > 0)
    }

    /// Assert that no replay simulation is active.
    ///
    /// Live side-effect code should call this before mutating panes, writing
    /// durable state, or publishing process-visible events.
    pub fn assert_not_simulating(operation: &str) {
        assert!(
            !Self::is_active(),
            "SIMULATION SAFETY VIOLATION: attempted live operation '{}' \
             during counterfactual simulation. This indicates a barrier \
             leak: all side effects must go through SideEffectBarrier.",
            operation
        );
    }
}

impl Drop for SimulationGuard {
    fn drop(&mut self) {
        THREAD_SIMULATION_DEPTH.with(|depth| {
            depth.set(depth.get().saturating_sub(1));
        });
        PROCESS_SIMULATION_DEPTH.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::SimulationGuard;
    use std::sync::{Mutex, MutexGuard};

    static SIMULATION_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn simulation_test_lock() -> MutexGuard<'static, ()> {
        SIMULATION_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn guard_sets_process_and_thread_scope() {
        let _lock = simulation_test_lock();
        assert!(!SimulationGuard::is_thread_active());
        {
            let _guard = SimulationGuard::enter();
            assert!(SimulationGuard::is_active());
            assert!(SimulationGuard::is_thread_active());
            assert!(SimulationGuard::active_count() > 0);
        }
        assert!(!SimulationGuard::is_thread_active());
    }

    #[test]
    fn guard_is_visible_from_other_threads() {
        let _lock = simulation_test_lock();
        let _guard = SimulationGuard::enter();
        let visible = std::thread::spawn(SimulationGuard::is_active)
            .join()
            .expect("simulation visibility probe thread should finish");
        assert!(visible);
    }

    #[test]
    #[should_panic(expected = "already in simulation mode")]
    fn guard_double_enter_same_thread_panics() {
        let _lock = simulation_test_lock();
        let _first = SimulationGuard::enter();
        let _second = SimulationGuard::enter();
    }

    #[test]
    #[should_panic(expected = "SIMULATION SAFETY VIOLATION")]
    fn guard_panics_on_live_operation() {
        let _lock = simulation_test_lock();
        let _guard = SimulationGuard::enter();
        SimulationGuard::assert_not_simulating("test live operation");
    }
}
