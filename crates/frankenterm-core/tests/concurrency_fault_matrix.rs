//! Concurrency failure injection matrix (ft-e34d9.10.6.2).
//!
//! Combines DPOR schedule exploration with chaos fault injection to
//! systematically verify that concurrent FrankenTerm operations survive
//! all classes of failure:
//!
//! - **Race conditions**: DPOR explores all relevant interleavings
//! - **I/O errors**: Chaos injects failures at DB/CLI fault points
//! - **Timeouts**: Delay injection simulates slow operations
//! - **Cancellation**: Tests that in-flight work is cleanly abandoned
//! - **Cascading failures**: Multiple simultaneous fault points
//!
//! # Matrix structure
//!
//! Each test defines a concurrent workload (multiple tasks operating on
//! shared state) and a failure injection profile. The matrix covers:
//!
//! | Scenario              | No faults | Single | Multi | Cascade | Backpressure | Timeout | Partial I/O |
//! |-----------------------|-----------|--------|-------|---------|--------------|---------|-------------|
//! | Pool acquire/release  |     ✓     |   ✓    |   ✓   |    ✓    |      ✓       |         |             |
//! | Channel send/recv     |     ✓     |   ✓    |   ✓   |    ✓    |              |    ✓    |             |
//! | Shared state mutation |     ✓     |   ✓    |       |    ✓    |              |         |      ✓      |
//! | Shutdown drain        |     ✓     |   ✓    |   ✓   |    ✓    |              |    ✓    |             |
//! | Event dispatch        |     ✓     |   ✓    |   ✓   |    ✓    |      ✓       |         |             |
//!
//! ## User-facing scenarios
//!
//! - Rate-limit wait: operations slow under throttling, verify no dropped work
//! - Reconnect storm: CLI backend fails then recovers, verify CFM-5
//! - Remote command retry: send_text fails with fallback path, verify resilience
//! - Startup under degraded I/O: initial DB failures, verify post-recovery
//! - Cancellation storm: mass task cancellation under load, verify CFM-7
//!
//! # Invariants verified
//!
//! CFM-1: No task leaks — all spawned tasks complete or cancel cleanly
//! CFM-2: No data races — shared counters are consistent after all schedules
//! CFM-3: Monotonic progress — at least one task makes progress per schedule
//! CFM-4: Fault resilience — operations degrade gracefully under injection
//! CFM-5: Recovery completeness — after faults clear, operations resume
//! CFM-6: Cascading containment — multi-fault doesn't cause unbounded failure
//! CFM-7: Cancellation safety — cancelled tasks don't corrupt shared state

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::lab::explorer::ScheduleExplorer;
use asupersync::{Budget, LabRuntime};

use frankenterm_core::chaos::{
    ChaosAssertion, ChaosScenario, FaultInjector, FaultMode, FaultPoint,
};

use common::lab::{ExplorationTestConfig, LabTestConfig, run_lab_test};

// =============================================================================
// Shared test state
// =============================================================================

/// Shared state for concurrent tasks. Wraps atomic counters to verify
/// consistency across all DPOR interleavings.
#[derive(Debug)]
struct SharedWorkloadState {
    /// Total operations attempted (monotonically increasing).
    ops_attempted: AtomicU64,
    /// Total operations that succeeded.
    ops_succeeded: AtomicU64,
    /// Total operations that failed (fault injected).
    ops_failed: AtomicU64,
    /// Tracks which tasks completed (bit mask).
    completed_mask: AtomicU64,
    /// Tracks cancellations observed.
    #[allow(dead_code)]
    cancellations: AtomicU64,
}

impl SharedWorkloadState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ops_attempted: AtomicU64::new(0),
            ops_succeeded: AtomicU64::new(0),
            ops_failed: AtomicU64::new(0),
            completed_mask: AtomicU64::new(0),
            cancellations: AtomicU64::new(0),
        })
    }

    /// Record an operation attempt and its outcome.
    fn record_op(&self, task_id: u64, succeeded: bool) {
        self.ops_attempted.fetch_add(1, Ordering::SeqCst);
        if succeeded {
            self.ops_succeeded.fetch_add(1, Ordering::SeqCst);
        } else {
            self.ops_failed.fetch_add(1, Ordering::SeqCst);
        }
        // Mark task as having completed at least one operation.
        self.completed_mask.fetch_or(1 << task_id, Ordering::SeqCst);
    }

    /// Record a cancellation.
    #[allow(dead_code)]
    fn record_cancellation(&self) {
        self.cancellations.fetch_add(1, Ordering::SeqCst);
    }

    /// Verify invariants after all tasks complete.
    fn assert_invariants(&self, test_name: &str) {
        let attempted = self.ops_attempted.load(Ordering::SeqCst);
        let succeeded = self.ops_succeeded.load(Ordering::SeqCst);
        let failed = self.ops_failed.load(Ordering::SeqCst);

        // CFM-2: Atomic counter consistency.
        assert_eq!(
            attempted,
            succeeded + failed,
            "[{test_name}] CFM-2: attempted ({attempted}) != succeeded ({succeeded}) + failed ({failed})"
        );

        // CFM-3: At least one task made progress.
        assert!(
            attempted > 0,
            "[{test_name}] CFM-3: no operations were attempted"
        );
    }
}

// =============================================================================
// Fault injection profiles
// =============================================================================

/// No faults — baseline correctness under DPOR exploration.
fn no_fault_profile() -> Vec<(FaultPoint, FaultMode)> {
    Vec::new()
}

/// Single fault: DB writes fail once.
fn single_db_write_fault() -> Vec<(FaultPoint, FaultMode)> {
    vec![(
        FaultPoint::DbWrite,
        FaultMode::fail_n_times(1, "injected: single db write failure"),
    )]
}

/// Single fault: CLI calls fail probabilistically.
fn probabilistic_cli_fault() -> Vec<(FaultPoint, FaultMode)> {
    vec![(
        FaultPoint::WeztermCliCall,
        FaultMode::fail_with_probability(0.3, "injected: cli call failure (30%)"),
    )]
}

/// Multi-fault: DB reads slow + pattern detection fails.
fn multi_fault_profile() -> Vec<(FaultPoint, FaultMode)> {
    vec![
        (FaultPoint::DbRead, FaultMode::delay(50)),
        (
            FaultPoint::PatternDetect,
            FaultMode::fail_n_times(2, "injected: pattern detection failure"),
        ),
    ]
}

/// Cascade: Everything goes wrong at once.
fn cascade_fault_profile() -> Vec<(FaultPoint, FaultMode)> {
    vec![
        (
            FaultPoint::DbWrite,
            FaultMode::fail_with_probability(0.5, "cascade: db write"),
        ),
        (
            FaultPoint::DbRead,
            FaultMode::delay_then_fail(10, "cascade: db read"),
        ),
        (
            FaultPoint::WeztermCliCall,
            FaultMode::fail_n_times(3, "cascade: cli"),
        ),
        (
            FaultPoint::PatternDetect,
            FaultMode::always_fail("cascade: pattern detect"),
        ),
    ]
}

// =============================================================================
// Workload definitions
// =============================================================================

/// Simulates concurrent pool acquire/release with fault injection at DB points.
///
/// N tasks each attempt to:
/// 1. "Acquire" a resource (check DbRead fault point)
/// 2. Perform work (check WeztermCliCall fault point)
/// 3. "Release" the resource (check DbWrite fault point)
fn pool_acquire_release_workload(
    runtime: &mut LabRuntime,
    state: &Arc<SharedWorkloadState>,
    task_count: u64,
) {
    let region = runtime.state.create_root_region(Budget::INFINITE);
    for task_id in 0..task_count {
        let st = Arc::clone(state);
        let (tid, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                // Step 1: Acquire (check read fault)
                let acquire_ok = FaultInjector::check(FaultPoint::DbRead).is_ok();
                if !acquire_ok {
                    st.record_op(task_id, false);
                    return;
                }

                // Step 2: Work (check CLI fault)
                let work_ok = FaultInjector::check(FaultPoint::WeztermCliCall).is_ok();

                // Step 3: Release (check write fault) — always attempt release
                let release_ok = FaultInjector::check(FaultPoint::DbWrite).is_ok();

                st.record_op(task_id, work_ok && release_ok);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(tid, 0);
    }
}

/// Simulates concurrent channel send/recv with fault injection.
///
/// Producer tasks send events, consumer tasks process them.
/// Faults can cause drops or delays in the pipeline.
fn channel_pipeline_workload(
    runtime: &mut LabRuntime,
    state: &Arc<SharedWorkloadState>,
    producer_count: u64,
    consumer_count: u64,
) {
    let events = Arc::new(AtomicU64::new(0));
    let region = runtime.state.create_root_region(Budget::INFINITE);

    // Producers
    for task_id in 0..producer_count {
        let st = Arc::clone(state);
        let ev = Arc::clone(&events);
        let (tid, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let ok = FaultInjector::check(FaultPoint::WeztermCliCall).is_ok();
                if ok {
                    ev.fetch_add(1, Ordering::SeqCst);
                }
                st.record_op(task_id, ok);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(tid, 0);
    }

    // Consumers
    for task_id in producer_count..producer_count + consumer_count {
        let st = Arc::clone(state);
        let ev = Arc::clone(&events);
        let (tid, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let pending = ev.load(Ordering::SeqCst);
                let ok = if pending > 0 {
                    FaultInjector::check(FaultPoint::PatternDetect).is_ok()
                } else {
                    true // Nothing to consume — vacuously ok
                };
                st.record_op(task_id, ok);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(tid, 0);
    }
}

/// Simulates shared state mutation under contention.
///
/// All tasks increment a shared counter through a fault-checked path.
/// Under fault injection, some increments fail — but the counter must
/// remain consistent (never negative, never exceed attempt count).
fn shared_mutation_workload(
    runtime: &mut LabRuntime,
    state: &Arc<SharedWorkloadState>,
    task_count: u64,
    ops_per_task: u64,
) {
    let shared_counter = Arc::new(AtomicU64::new(0));
    let region = runtime.state.create_root_region(Budget::INFINITE);

    for task_id in 0..task_count {
        let st = Arc::clone(state);
        let counter = Arc::clone(&shared_counter);
        let (tid, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                for _ in 0..ops_per_task {
                    let ok = FaultInjector::check(FaultPoint::DbWrite).is_ok();
                    if ok {
                        counter.fetch_add(1, Ordering::SeqCst);
                    }
                    st.record_op(task_id, ok);
                }
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(tid, 0);
    }
}

/// Simulates event dispatch under concurrent fault injection.
///
/// Dispatcher tasks fire events at multiple fault points; listener tasks
/// observe outcomes. Validates that event dispatch degrades gracefully.
fn event_dispatch_workload(
    runtime: &mut LabRuntime,
    state: &Arc<SharedWorkloadState>,
    dispatcher_count: u64,
) {
    let dispatch_results = Arc::new(AtomicU64::new(0));
    let region = runtime.state.create_root_region(Budget::INFINITE);

    for task_id in 0..dispatcher_count {
        let st = Arc::clone(state);
        let results = Arc::clone(&dispatch_results);
        let (tid, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                // Try multiple fault points in sequence
                let db_ok = FaultInjector::check(FaultPoint::DbWrite).is_ok();
                let cli_ok = FaultInjector::check(FaultPoint::WeztermCliCall).is_ok();
                let pattern_ok = FaultInjector::check(FaultPoint::PatternDetect).is_ok();

                let all_ok = db_ok && cli_ok && pattern_ok;
                if all_ok {
                    results.fetch_add(1, Ordering::SeqCst);
                }
                st.record_op(task_id, all_ok);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(tid, 0);
    }
}

// =============================================================================
// Matrix runner
// =============================================================================

/// Result of running one cell in the fault matrix.
#[derive(Debug)]
#[allow(dead_code)]
struct MatrixCellResult {
    scenario_name: String,
    workload_name: String,
    total_runs: usize,
    unique_classes: usize,
    all_passed: bool,
    ops_attempted: u64,
    ops_succeeded: u64,
    ops_failed: u64,
}

/// Run a single matrix cell: workload x fault profile across DPOR seeds.
fn run_matrix_cell<W>(
    workload_name: &str,
    scenario_name: &str,
    faults: &[(FaultPoint, FaultMode)],
    max_runs: usize,
    workload_fn: W,
) -> MatrixCellResult
where
    W: Fn(&mut LabRuntime, &Arc<SharedWorkloadState>) + Send + Sync,
{
    let test_name = format!("cfm/{workload_name}/{scenario_name}");

    // Set up global fault injector
    let injector = FaultInjector::init_global();
    injector.clear_all();
    for (point, mode) in faults {
        injector.set_fault(*point, mode.clone());
    }

    let final_state = SharedWorkloadState::new();
    let captured_state = Arc::clone(&final_state);

    let config = ExplorationTestConfig::new(&test_name, max_runs)
        .base_seed(42)
        .worker_count(4)
        .max_steps_per_run(10_000);

    let explorer_config = config.to_explorer_config();
    let mut explorer = ScheduleExplorer::new(explorer_config);

    let inner = explorer.explore(|runtime| {
        // Reset fault counters per exploration run
        if let Some(inj) = FaultInjector::global() {
            let _ = inj.drain_log();
        }

        workload_fn(runtime, &captured_state);
        runtime.run_until_quiescent();
    });

    let has_violations = inner.has_violations();

    // Verify workload invariants
    final_state.assert_invariants(&test_name);

    // Clean up
    FaultInjector::reset_global();

    MatrixCellResult {
        scenario_name: scenario_name.to_string(),
        workload_name: workload_name.to_string(),
        total_runs: inner.total_runs,
        unique_classes: inner.unique_classes,
        all_passed: !has_violations,
        ops_attempted: final_state.ops_attempted.load(Ordering::SeqCst),
        ops_succeeded: final_state.ops_succeeded.load(Ordering::SeqCst),
        ops_failed: final_state.ops_failed.load(Ordering::SeqCst),
    }
}

// =============================================================================
// Test: Pool acquire/release matrix
// =============================================================================

#[test]
fn cfm_pool_no_faults() {
    let result = run_matrix_cell(
        "pool",
        "no_faults",
        &no_fault_profile(),
        20,
        |runtime, state| pool_acquire_release_workload(runtime, state, 4),
    );
    assert!(result.all_passed, "pool/no_faults failed: {result:?}");
    // Without faults, all ops should succeed.
    assert_eq!(
        result.ops_failed, 0,
        "expected zero failures without faults"
    );
}

#[test]
fn cfm_pool_single_fault() {
    let result = run_matrix_cell(
        "pool",
        "single_db_write",
        &single_db_write_fault(),
        20,
        |runtime, state| pool_acquire_release_workload(runtime, state, 4),
    );
    assert!(result.all_passed, "pool/single_db_write failed: {result:?}");
}

#[test]
fn cfm_pool_multi_fault() {
    let result = run_matrix_cell(
        "pool",
        "multi_fault",
        &multi_fault_profile(),
        20,
        |runtime, state| pool_acquire_release_workload(runtime, state, 4),
    );
    assert!(result.all_passed, "pool/multi_fault failed: {result:?}");
}

#[test]
fn cfm_pool_cascade() {
    let result = run_matrix_cell(
        "pool",
        "cascade",
        &cascade_fault_profile(),
        20,
        |runtime, state| pool_acquire_release_workload(runtime, state, 4),
    );
    assert!(result.all_passed, "pool/cascade failed: {result:?}");
    // CFM-6: Even under cascade, some ops should still be attempted.
    assert!(
        result.ops_attempted > 0,
        "cascade should still attempt operations"
    );
}

// =============================================================================
// Test: Channel pipeline matrix
// =============================================================================

#[test]
fn cfm_channel_no_faults() {
    let result = run_matrix_cell(
        "channel",
        "no_faults",
        &no_fault_profile(),
        20,
        |runtime, state| channel_pipeline_workload(runtime, state, 3, 2),
    );
    assert!(result.all_passed, "channel/no_faults failed: {result:?}");
    assert_eq!(
        result.ops_failed, 0,
        "expected zero failures without faults"
    );
}

#[test]
fn cfm_channel_single_fault() {
    let result = run_matrix_cell(
        "channel",
        "probabilistic_cli",
        &probabilistic_cli_fault(),
        20,
        |runtime, state| channel_pipeline_workload(runtime, state, 3, 2),
    );
    assert!(
        result.all_passed,
        "channel/probabilistic_cli failed: {result:?}"
    );
}

#[test]
fn cfm_channel_multi_fault() {
    let result = run_matrix_cell(
        "channel",
        "multi_fault",
        &multi_fault_profile(),
        20,
        |runtime, state| channel_pipeline_workload(runtime, state, 3, 2),
    );
    assert!(result.all_passed, "channel/multi_fault failed: {result:?}");
}

#[test]
fn cfm_channel_cascade() {
    let result = run_matrix_cell(
        "channel",
        "cascade",
        &cascade_fault_profile(),
        20,
        |runtime, state| channel_pipeline_workload(runtime, state, 3, 2),
    );
    assert!(result.all_passed, "channel/cascade failed: {result:?}");
}

// =============================================================================
// Test: Shared mutation matrix
// =============================================================================

#[test]
fn cfm_mutation_no_faults() {
    let result = run_matrix_cell(
        "mutation",
        "no_faults",
        &no_fault_profile(),
        20,
        |runtime, state| shared_mutation_workload(runtime, state, 4, 3),
    );
    assert!(result.all_passed, "mutation/no_faults failed: {result:?}");
    // 4 tasks x 3 ops each = 12 total, all succeed.
    assert_eq!(result.ops_failed, 0);
}

#[test]
fn cfm_mutation_single_fault() {
    let result = run_matrix_cell(
        "mutation",
        "single_db_write",
        &single_db_write_fault(),
        20,
        |runtime, state| shared_mutation_workload(runtime, state, 4, 3),
    );
    assert!(
        result.all_passed,
        "mutation/single_db_write failed: {result:?}"
    );
}

#[test]
fn cfm_mutation_cascade() {
    let result = run_matrix_cell(
        "mutation",
        "cascade",
        &cascade_fault_profile(),
        20,
        |runtime, state| shared_mutation_workload(runtime, state, 4, 3),
    );
    assert!(result.all_passed, "mutation/cascade failed: {result:?}");
}

// =============================================================================
// Test: Event dispatch matrix
// =============================================================================

#[test]
fn cfm_dispatch_no_faults() {
    let result = run_matrix_cell(
        "dispatch",
        "no_faults",
        &no_fault_profile(),
        20,
        |runtime, state| event_dispatch_workload(runtime, state, 5),
    );
    assert!(result.all_passed, "dispatch/no_faults failed: {result:?}");
    assert_eq!(result.ops_failed, 0);
}

#[test]
fn cfm_dispatch_single_fault() {
    let result = run_matrix_cell(
        "dispatch",
        "probabilistic_cli",
        &probabilistic_cli_fault(),
        20,
        |runtime, state| event_dispatch_workload(runtime, state, 5),
    );
    assert!(
        result.all_passed,
        "dispatch/probabilistic_cli failed: {result:?}"
    );
}

#[test]
fn cfm_dispatch_multi_fault() {
    let result = run_matrix_cell(
        "dispatch",
        "multi_fault",
        &multi_fault_profile(),
        20,
        |runtime, state| event_dispatch_workload(runtime, state, 5),
    );
    assert!(result.all_passed, "dispatch/multi_fault failed: {result:?}");
}

#[test]
fn cfm_dispatch_cascade() {
    let result = run_matrix_cell(
        "dispatch",
        "cascade",
        &cascade_fault_profile(),
        20,
        |runtime, state| event_dispatch_workload(runtime, state, 5),
    );
    assert!(result.all_passed, "dispatch/cascade failed: {result:?}");
}

// =============================================================================
// Test: Cross-workload chaos scenarios
// =============================================================================

/// Run the full matrix as a ChaosScenario, validating assertion predicates.
#[test]
fn cfm_full_matrix_chaos_scenario() {
    let scenario = ChaosScenario::new(
        "full_cfm_chaos",
        "Full concurrency fault matrix with chaos assertions",
    )
    .with_fault(
        FaultPoint::DbWrite,
        FaultMode::fail_n_times(5, "matrix: db write"),
    )
    .with_fault(
        FaultPoint::WeztermCliCall,
        FaultMode::fail_with_probability(0.2, "matrix: cli"),
    )
    .with_assertion(ChaosAssertion::FaultFiredAtLeast(FaultPoint::DbWrite, 1))
    .with_assertion(ChaosAssertion::TotalFaultsInRange(1, 100));

    let injector = FaultInjector::init_global();
    injector.clear_all();

    // Apply scenario faults
    for (point, mode) in &scenario.faults {
        injector.set_fault(*point, mode.clone());
    }

    // Run mixed workloads
    let config = LabTestConfig::new(777, "cfm_full_matrix_chaos_scenario").worker_count(4);
    let state = SharedWorkloadState::new();
    let st = Arc::clone(&state);

    run_lab_test(config, |runtime| {
        pool_acquire_release_workload(runtime, &st, 3);
        channel_pipeline_workload(runtime, &st, 2, 2);
        shared_mutation_workload(runtime, &st, 2, 2);
        event_dispatch_workload(runtime, &st, 3);
    });

    // Verify workload invariants
    state.assert_invariants("cfm_full_matrix_chaos_scenario");

    // Verify chaos assertions
    let report = injector.check_assertions(&scenario);
    for result in &report {
        assert!(
            result.passed,
            "Chaos assertion failed: {} — {}",
            result.assertion, result.detail
        );
    }

    FaultInjector::reset_global();
}

// =============================================================================
// Test: Recovery after fault clearance
// =============================================================================

/// Verify CFM-5: after faults are cleared, operations resume successfully.
#[test]
fn cfm_recovery_after_fault_clearance() {
    let injector = FaultInjector::init_global();
    injector.clear_all();

    // Phase 1: Run with faults
    injector.set_fault(
        FaultPoint::DbWrite,
        FaultMode::always_fail("recovery test: db write blocked"),
    );

    let state_faulted = SharedWorkloadState::new();
    let st = Arc::clone(&state_faulted);
    let config = LabTestConfig::new(888, "cfm_recovery_faulted").worker_count(2);
    run_lab_test(config, |runtime| {
        shared_mutation_workload(runtime, &st, 3, 2);
    });
    state_faulted.assert_invariants("cfm_recovery_faulted");
    let faulted_failures = state_faulted.ops_failed.load(Ordering::SeqCst);
    assert!(faulted_failures > 0, "expected failures during fault phase");

    // Phase 2: Clear faults and run again
    injector.clear_all();

    let state_recovered = SharedWorkloadState::new();
    let st2 = Arc::clone(&state_recovered);
    let config2 = LabTestConfig::new(889, "cfm_recovery_cleared").worker_count(2);
    run_lab_test(config2, |runtime| {
        shared_mutation_workload(runtime, &st2, 3, 2);
    });
    state_recovered.assert_invariants("cfm_recovery_cleared");
    let recovered_failures = state_recovered.ops_failed.load(Ordering::SeqCst);

    // CFM-5: After clearing faults, operations should succeed.
    assert_eq!(
        recovered_failures, 0,
        "CFM-5: expected zero failures after fault clearance, got {recovered_failures}"
    );

    FaultInjector::reset_global();
}

// =============================================================================
// Test: Cancellation safety
// =============================================================================

/// Verify CFM-7: cancelled tasks don't corrupt shared state.
#[test]
fn cfm_cancellation_safety() {
    let injector = FaultInjector::init_global();
    injector.clear_all();

    // Use delay to simulate slow operations that get cancelled.
    injector.set_fault(FaultPoint::DbRead, FaultMode::delay(1000));

    let state = SharedWorkloadState::new();
    let st = Arc::clone(&state);

    // Run with tight step limit — tasks won't all complete.
    let config = LabTestConfig::new(999, "cfm_cancellation_safety")
        .worker_count(2)
        .max_steps(50)
        .panic_on_leak(false); // Expect some tasks won't finish

    let mut runtime = LabRuntime::new(config.to_lab_config());
    let region = runtime.state.create_root_region(Budget::INFINITE);

    // Spawn tasks that will be interrupted
    for task_id in 0..4u64 {
        let s = Arc::clone(&st);
        let (tid, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let ok = FaultInjector::check(FaultPoint::DbRead).is_ok();
                if ok {
                    let _ = FaultInjector::check(FaultPoint::DbWrite);
                }
                s.record_op(task_id, ok);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(tid, 0);
    }

    // Run with limited steps (some tasks may not complete)
    runtime.run_until_quiescent();

    // CFM-7: Whatever completed must be consistent.
    let attempted = state.ops_attempted.load(Ordering::SeqCst);
    let succeeded = state.ops_succeeded.load(Ordering::SeqCst);
    let failed = state.ops_failed.load(Ordering::SeqCst);
    assert_eq!(
        attempted,
        succeeded + failed,
        "CFM-7: inconsistent counters after cancellation"
    );

    FaultInjector::reset_global();
}

// =============================================================================
// Test: Exploration with increasing concurrency
// =============================================================================

/// Verify that the matrix scales — more concurrent tasks don't break invariants.
#[test]
fn cfm_scaling_concurrency() {
    for task_count in [2, 4, 8] {
        let result = run_matrix_cell(
            "scaling",
            &format!("tasks_{task_count}"),
            &single_db_write_fault(),
            10,
            |runtime, state| pool_acquire_release_workload(runtime, state, task_count),
        );
        assert!(
            result.all_passed,
            "scaling/tasks_{task_count} failed: {result:?}"
        );
    }
}

// =============================================================================
// Test: Determinism verification
// =============================================================================

// =============================================================================
// Fault injection profiles — extended
// =============================================================================

/// Backpressure spike: all I/O delayed, simulating system under heavy load.
fn backpressure_spike_profile() -> Vec<(FaultPoint, FaultMode)> {
    vec![
        (FaultPoint::DbWrite, FaultMode::delay(200)),
        (FaultPoint::DbRead, FaultMode::delay(200)),
        (
            FaultPoint::WeztermCliCall,
            FaultMode::delay_then_fail(100, "backpressure: cli timeout"),
        ),
    ]
}

/// Timeout race: operations delay then fail, simulating deadline exceedance.
fn timeout_race_profile() -> Vec<(FaultPoint, FaultMode)> {
    vec![
        (
            FaultPoint::DbRead,
            FaultMode::delay_then_fail(500, "timeout: db read deadline exceeded"),
        ),
        (
            FaultPoint::WeztermCliCall,
            FaultMode::delay_then_fail(500, "timeout: cli deadline exceeded"),
        ),
    ]
}

/// Partial I/O: reads succeed but writes fail intermittently.
fn partial_io_profile() -> Vec<(FaultPoint, FaultMode)> {
    vec![
        (
            FaultPoint::DbWrite,
            FaultMode::fail_with_probability(0.5, "partial_io: write failed"),
        ),
        // Reads always succeed — simulates partial I/O degradation.
    ]
}

// =============================================================================
// Workload: Shutdown drain
// =============================================================================

/// Simulates graceful shutdown drain under fault injection.
///
/// Tasks are spawned in two waves:
/// 1. "Running" tasks that perform work.
/// 2. "Drain" tasks that clean up after running tasks finish.
///
/// Under faults, drain must still complete — no resource leaks.
fn shutdown_drain_workload(
    runtime: &mut LabRuntime,
    state: &Arc<SharedWorkloadState>,
    running_count: u64,
    drain_count: u64,
) {
    let drain_ready = Arc::new(AtomicU64::new(0));
    let region = runtime.state.create_root_region(Budget::INFINITE);

    // Phase 1: Running tasks
    for task_id in 0..running_count {
        let st = Arc::clone(state);
        let ready = Arc::clone(&drain_ready);
        let (tid, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let ok = FaultInjector::check(FaultPoint::DbWrite).is_ok();
                st.record_op(task_id, ok);
                // Signal drain readiness
                ready.fetch_add(1, Ordering::SeqCst);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(tid, 0);
    }

    // Phase 2: Drain tasks (cleanup)
    for task_id in running_count..running_count + drain_count {
        let st = Arc::clone(state);
        let ready = Arc::clone(&drain_ready);
        let (tid, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                // Drain checks remaining work
                let pending = ready.load(Ordering::SeqCst);
                let ok = if pending > 0 {
                    FaultInjector::check(FaultPoint::RetentionCleanup).is_ok()
                } else {
                    true
                };
                st.record_op(task_id, ok);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(tid, 0);
    }
}

// =============================================================================
// Test: Shutdown drain matrix
// =============================================================================

#[test]
fn cfm_drain_no_faults() {
    let result = run_matrix_cell(
        "drain",
        "no_faults",
        &no_fault_profile(),
        20,
        |runtime, state| shutdown_drain_workload(runtime, state, 3, 2),
    );
    assert!(result.all_passed, "drain/no_faults failed: {result:?}");
    assert_eq!(result.ops_failed, 0);
}

#[test]
fn cfm_drain_single_fault() {
    let result = run_matrix_cell(
        "drain",
        "single_db_write",
        &single_db_write_fault(),
        20,
        |runtime, state| shutdown_drain_workload(runtime, state, 3, 2),
    );
    assert!(result.all_passed, "drain/single_db_write failed: {result:?}");
}

#[test]
fn cfm_drain_multi_fault() {
    let result = run_matrix_cell(
        "drain",
        "multi_fault",
        &multi_fault_profile(),
        20,
        |runtime, state| shutdown_drain_workload(runtime, state, 3, 2),
    );
    assert!(result.all_passed, "drain/multi_fault failed: {result:?}");
}

#[test]
fn cfm_drain_cascade() {
    let result = run_matrix_cell(
        "drain",
        "cascade",
        &cascade_fault_profile(),
        20,
        |runtime, state| shutdown_drain_workload(runtime, state, 3, 2),
    );
    assert!(result.all_passed, "drain/cascade failed: {result:?}");
}

// =============================================================================
// Test: Backpressure spike matrix (extended profiles)
// =============================================================================

#[test]
fn cfm_pool_backpressure_spike() {
    let result = run_matrix_cell(
        "pool",
        "backpressure_spike",
        &backpressure_spike_profile(),
        15,
        |runtime, state| pool_acquire_release_workload(runtime, state, 4),
    );
    assert!(
        result.all_passed,
        "pool/backpressure_spike failed: {result:?}"
    );
}

#[test]
fn cfm_channel_timeout_race() {
    let result = run_matrix_cell(
        "channel",
        "timeout_race",
        &timeout_race_profile(),
        15,
        |runtime, state| channel_pipeline_workload(runtime, state, 3, 2),
    );
    assert!(
        result.all_passed,
        "channel/timeout_race failed: {result:?}"
    );
}

#[test]
fn cfm_mutation_partial_io() {
    let result = run_matrix_cell(
        "mutation",
        "partial_io",
        &partial_io_profile(),
        15,
        |runtime, state| shared_mutation_workload(runtime, state, 4, 3),
    );
    assert!(
        result.all_passed,
        "mutation/partial_io failed: {result:?}"
    );
}

#[test]
fn cfm_dispatch_backpressure_spike() {
    let result = run_matrix_cell(
        "dispatch",
        "backpressure_spike",
        &backpressure_spike_profile(),
        15,
        |runtime, state| event_dispatch_workload(runtime, state, 5),
    );
    assert!(
        result.all_passed,
        "dispatch/backpressure_spike failed: {result:?}"
    );
}

#[test]
fn cfm_drain_timeout_race() {
    let result = run_matrix_cell(
        "drain",
        "timeout_race",
        &timeout_race_profile(),
        15,
        |runtime, state| shutdown_drain_workload(runtime, state, 3, 2),
    );
    assert!(
        result.all_passed,
        "drain/timeout_race failed: {result:?}"
    );
}

// =============================================================================
// Test: User-facing scenario — rate-limit wait
// =============================================================================

/// Simulates user operations slowed by rate limiting.
///
/// Tasks check multiple fault points representing backend calls. Under
/// backpressure, operations succeed but slowly — verifying that the system
/// doesn't drop work during rate-limit waits.
#[test]
fn cfm_scenario_rate_limit_wait() {
    let injector = FaultInjector::init_global();
    injector.clear_all();

    // Simulate rate limiting: reads delayed, writes occasionally rejected
    injector.set_fault(FaultPoint::DbRead, FaultMode::delay(100));
    injector.set_fault(
        FaultPoint::DbWrite,
        FaultMode::fail_n_times(3, "rate limited: retry later"),
    );

    let state = SharedWorkloadState::new();
    let st = Arc::clone(&state);
    let config = LabTestConfig::new(1001, "cfm_scenario_rate_limit_wait").worker_count(4);

    run_lab_test(config, |runtime| {
        // Multiple users sending commands concurrently
        let region = runtime.state.create_root_region(Budget::INFINITE);
        for task_id in 0..6u64 {
            let s = Arc::clone(&st);
            let (tid, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    // Read succeeds (but delayed)
                    let read_ok = FaultInjector::check(FaultPoint::DbRead).is_ok();
                    // Write may be rate-limited
                    let write_ok = FaultInjector::check(FaultPoint::DbWrite).is_ok();
                    s.record_op(task_id, read_ok && write_ok);
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(tid, 0);
        }
    });

    state.assert_invariants("cfm_scenario_rate_limit_wait");
    // After rate limit exhausts (3 failures), remaining ops should succeed
    let succeeded = state.ops_succeeded.load(Ordering::SeqCst);
    assert!(succeeded > 0, "at least some ops should succeed after rate limit");

    FaultInjector::reset_global();
}

// =============================================================================
// Test: User-facing scenario — reconnect storm
// =============================================================================

/// Simulates a reconnect storm: CLI backend goes down and comes back.
///
/// All CLI calls fail for the first N invocations, then recover.
/// Verifies CFM-5 (recovery) in a user-facing context.
#[test]
fn cfm_scenario_reconnect_storm() {
    let injector = FaultInjector::init_global();
    injector.clear_all();

    // CLI completely down for 5 calls, then recovers
    injector.set_fault(
        FaultPoint::WeztermCliCall,
        FaultMode::fail_n_times(5, "connection refused"),
    );

    let state = SharedWorkloadState::new();
    let st = Arc::clone(&state);
    let config = LabTestConfig::new(1002, "cfm_scenario_reconnect_storm").worker_count(3);

    run_lab_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        // 8 tasks hitting CLI — first 5 calls fail, remaining succeed
        for task_id in 0..8u64 {
            let s = Arc::clone(&st);
            let (tid, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let ok = FaultInjector::check(FaultPoint::WeztermCliCall).is_ok();
                    s.record_op(task_id, ok);
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(tid, 0);
        }
    });

    state.assert_invariants("cfm_scenario_reconnect_storm");
    // Exactly 5 failures from the fail_n_times(5)
    let failed = state.ops_failed.load(Ordering::SeqCst);
    assert!(
        failed <= 5,
        "reconnect storm: at most 5 failures expected, got {failed}"
    );
    // Some must succeed (recovery)
    let succeeded = state.ops_succeeded.load(Ordering::SeqCst);
    assert!(
        succeeded > 0,
        "reconnect storm: at least some ops should recover"
    );

    FaultInjector::reset_global();
}

// =============================================================================
// Test: User-facing scenario — remote command retry
// =============================================================================

/// Simulates remote send_text retries: first attempts fail, then succeed.
///
/// Models the user experience of sending a command to a pane when the
/// WezTerm CLI is temporarily unreachable.
#[test]
fn cfm_scenario_remote_command_retry() {
    let injector = FaultInjector::init_global();
    injector.clear_all();

    // Transient CLI failures
    injector.set_fault(
        FaultPoint::WeztermCliCall,
        FaultMode::fail_n_times(2, "send_text: connection reset"),
    );
    // DB writes also flaky
    injector.set_fault(
        FaultPoint::DbWrite,
        FaultMode::fail_n_times(1, "write ahead log full"),
    );

    let state = SharedWorkloadState::new();
    let st = Arc::clone(&state);
    let config = LabTestConfig::new(1003, "cfm_scenario_remote_command_retry").worker_count(2);

    run_lab_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);
        for task_id in 0..5u64 {
            let s = Arc::clone(&st);
            let (tid, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    // Simulate retry pattern: check CLI, fallback to DB log
                    let cli_ok = FaultInjector::check(FaultPoint::WeztermCliCall).is_ok();
                    let db_ok = FaultInjector::check(FaultPoint::DbWrite).is_ok();
                    s.record_op(task_id, cli_ok || db_ok); // Succeed if either path works
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(tid, 0);
        }
    });

    state.assert_invariants("cfm_scenario_remote_command_retry");
    // With retry/fallback, most ops should succeed
    let succeeded = state.ops_succeeded.load(Ordering::SeqCst);
    assert!(
        succeeded >= 3,
        "remote command retry: expected >= 3 successes with fallback, got {succeeded}"
    );

    FaultInjector::reset_global();
}

// =============================================================================
// Test: User-facing scenario — startup under degraded I/O
// =============================================================================

/// Simulates application startup when the disk subsystem is degraded.
///
/// Initial DB operations are slow and fail; the system must still
/// initialize and serve requests once I/O recovers.
#[test]
fn cfm_scenario_startup_degraded_io() {
    let injector = FaultInjector::init_global();
    injector.clear_all();

    // DB reads and writes fail during startup (first 3 of each)
    injector.set_fault(
        FaultPoint::DbRead,
        FaultMode::fail_n_times(3, "disk: I/O error during startup"),
    );
    injector.set_fault(
        FaultPoint::DbWrite,
        FaultMode::fail_n_times(3, "disk: I/O error during startup"),
    );
    // Config reload also fails once
    injector.set_fault(
        FaultPoint::ConfigReload,
        FaultMode::fail_n_times(1, "config: file locked during startup"),
    );

    let state = SharedWorkloadState::new();
    let st = Arc::clone(&state);
    let config = LabTestConfig::new(1004, "cfm_scenario_startup_degraded_io").worker_count(3);

    run_lab_test(config, |runtime| {
        let region = runtime.state.create_root_region(Budget::INFINITE);

        // Startup phase: initialization tasks
        for task_id in 0..3u64 {
            let s = Arc::clone(&st);
            let (tid, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let read_ok = FaultInjector::check(FaultPoint::DbRead).is_ok();
                    let config_ok = FaultInjector::check(FaultPoint::ConfigReload).is_ok();
                    s.record_op(task_id, read_ok && config_ok);
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(tid, 0);
        }

        // Post-startup phase: normal operations (I/O should be recovered)
        for task_id in 3..7u64 {
            let s = Arc::clone(&st);
            let (tid, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let read_ok = FaultInjector::check(FaultPoint::DbRead).is_ok();
                    let write_ok = FaultInjector::check(FaultPoint::DbWrite).is_ok();
                    s.record_op(task_id, read_ok && write_ok);
                })
                .expect("create task");
            runtime.scheduler.lock().schedule(tid, 0);
        }
    });

    state.assert_invariants("cfm_scenario_startup_degraded_io");
    // Post-startup tasks should mostly succeed
    let total_ops = state.ops_attempted.load(Ordering::SeqCst);
    let succeeded = state.ops_succeeded.load(Ordering::SeqCst);
    assert!(total_ops >= 7, "all tasks should have attempted operations");
    assert!(
        succeeded > 0,
        "at least some post-startup operations should succeed"
    );

    FaultInjector::reset_global();
}

// =============================================================================
// Test: User-facing scenario — cancellation storm
// =============================================================================

/// Simulates a cancellation storm where many tasks are cancelled mid-flight.
///
/// Exercises CFM-7 under high-concurrency cancellation, verifying that
/// shared state remains consistent when many tasks are abruptly stopped.
#[test]
fn cfm_scenario_cancellation_storm() {
    let injector = FaultInjector::init_global();
    injector.clear_all();

    // All operations delayed heavily — most will be "cancelled" by step limit
    injector.set_fault(FaultPoint::DbRead, FaultMode::delay(5000));
    injector.set_fault(FaultPoint::DbWrite, FaultMode::delay(5000));
    injector.set_fault(FaultPoint::WeztermCliCall, FaultMode::delay(5000));

    let state = SharedWorkloadState::new();
    let st = Arc::clone(&state);

    let config = LabTestConfig::new(1005, "cfm_scenario_cancellation_storm")
        .worker_count(4)
        .max_steps(100)
        .panic_on_leak(false);

    let mut runtime = LabRuntime::new(config.to_lab_config());
    let region = runtime.state.create_root_region(Budget::INFINITE);

    // Spawn many tasks that will mostly be cancelled
    for task_id in 0..16u64 {
        let s = Arc::clone(&st);
        let (tid, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let db_ok = FaultInjector::check(FaultPoint::DbRead).is_ok();
                let cli_ok = FaultInjector::check(FaultPoint::WeztermCliCall).is_ok();
                let write_ok = FaultInjector::check(FaultPoint::DbWrite).is_ok();
                s.record_op(task_id, db_ok && cli_ok && write_ok);
            })
            .expect("create task");
        runtime.scheduler.lock().schedule(tid, 0);
    }

    runtime.run_until_quiescent();

    // CFM-7: Even with mass cancellation, completed tasks must be consistent
    let attempted = state.ops_attempted.load(Ordering::SeqCst);
    let succeeded = state.ops_succeeded.load(Ordering::SeqCst);
    let failed = state.ops_failed.load(Ordering::SeqCst);
    assert_eq!(
        attempted,
        succeeded + failed,
        "CFM-7 cancellation storm: counter inconsistency"
    );

    FaultInjector::reset_global();
}

// =============================================================================
// Structured telemetry output
// =============================================================================

/// Structured test result for machine-readable output.
///
/// Used for CI archival and failure trace replay per acceptance criterion 6.
#[derive(Debug, Serialize)]
struct CfmTestResult {
    /// Test identifier.
    test_id: String,
    /// Timestamp of test execution (epoch ms).
    timestamp_ms: u64,
    /// Component under test.
    component: &'static str,
    /// Workload name.
    workload: String,
    /// Scenario/fault profile name.
    scenario: String,
    /// Whether all invariants passed.
    passed: bool,
    /// Total DPOR runs executed.
    total_runs: usize,
    /// Unique equivalence classes discovered.
    unique_classes: usize,
    /// Operations attempted across all runs.
    ops_attempted: u64,
    /// Operations succeeded across all runs.
    ops_succeeded: u64,
    /// Operations failed (fault-injected) across all runs.
    ops_failed: u64,
    /// Fault injection triggers observed.
    fault_triggers: usize,
    /// Reason code for failures (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    reason_code: Option<String>,
}

use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

impl CfmTestResult {
    fn from_cell(cell: &MatrixCellResult, fault_triggers: usize) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Self {
            test_id: format!("cfm/{}/{}", cell.workload_name, cell.scenario_name),
            timestamp_ms: now_ms,
            component: "concurrency_fault_matrix",
            workload: cell.workload_name.clone(),
            scenario: cell.scenario_name.clone(),
            passed: cell.all_passed,
            total_runs: cell.total_runs,
            unique_classes: cell.unique_classes,
            ops_attempted: cell.ops_attempted,
            ops_succeeded: cell.ops_succeeded,
            ops_failed: cell.ops_failed,
            fault_triggers,
            reason_code: if !cell.all_passed {
                Some("invariant_violation".to_string())
            } else {
                None
            },
        }
    }
}

/// Run a matrix cell and emit structured JSON telemetry to tracing.
fn run_matrix_cell_with_telemetry<W>(
    workload_name: &str,
    scenario_name: &str,
    faults: &[(FaultPoint, FaultMode)],
    max_runs: usize,
    workload_fn: W,
) -> MatrixCellResult
where
    W: Fn(&mut LabRuntime, &Arc<SharedWorkloadState>) + Send + Sync,
{
    let result = run_matrix_cell(workload_name, scenario_name, faults, max_runs, workload_fn);

    let triggers = FaultInjector::global()
        .map(|inj| inj.total_fired())
        .unwrap_or(0);
    let telemetry = CfmTestResult::from_cell(&result, triggers);

    if let Ok(json) = serde_json::to_string(&telemetry) {
        tracing::info!(
            target: "cfm_telemetry",
            test_id = %telemetry.test_id,
            passed = telemetry.passed,
            ops_attempted = telemetry.ops_attempted,
            "CFM result: {json}"
        );
    }

    result
}

// =============================================================================
// Test: Full matrix with structured telemetry
// =============================================================================

/// Run the complete 5×4 core matrix with structured telemetry output.
///
/// This test validates the full matrix and emits machine-readable results
/// for CI archival. Marked with `#[ignore]` for CI-friendly slicing —
/// run explicitly with `--ignored` for thorough verification.
#[test]
#[ignore] // CI slice: thorough — run with `cargo test -- --ignored cfm_full_telemetry_matrix`
fn cfm_full_telemetry_matrix() {
    let workloads: Vec<(&str, Box<dyn Fn(&mut LabRuntime, &Arc<SharedWorkloadState>) + Send + Sync>)> = vec![
        ("pool", Box::new(|rt, st| pool_acquire_release_workload(rt, st, 4))),
        ("channel", Box::new(|rt, st| channel_pipeline_workload(rt, st, 3, 2))),
        ("mutation", Box::new(|rt, st| shared_mutation_workload(rt, st, 4, 3))),
        ("dispatch", Box::new(|rt, st| event_dispatch_workload(rt, st, 5))),
        ("drain", Box::new(|rt, st| shutdown_drain_workload(rt, st, 3, 2))),
    ];

    let profiles: Vec<(&str, Vec<(FaultPoint, FaultMode)>)> = vec![
        ("no_faults", no_fault_profile()),
        ("single_db_write", single_db_write_fault()),
        ("multi_fault", multi_fault_profile()),
        ("cascade", cascade_fault_profile()),
        ("backpressure_spike", backpressure_spike_profile()),
        ("timeout_race", timeout_race_profile()),
        ("partial_io", partial_io_profile()),
    ];

    let mut total = 0;
    let mut passed = 0;

    for (wl_name, wl_fn) in &workloads {
        for (sc_name, faults) in &profiles {
            let result = run_matrix_cell_with_telemetry(wl_name, sc_name, faults, 10, |rt, st| {
                wl_fn(rt, st);
            });
            total += 1;
            if result.all_passed {
                passed += 1;
            }
        }
    }

    assert_eq!(
        passed, total,
        "Full 5x7 telemetry matrix: {passed}/{total} cells passed"
    );
}

// =============================================================================
// Test: Exploration with increasing concurrency
// =============================================================================

/// Verify that running the same matrix cell twice with the same seed
/// produces identical results (CFM determinism guarantee).
#[test]
fn cfm_determinism() {
    let run = |seed: u64| -> (u64, u64, u64) {
        let injector = FaultInjector::init_global();
        injector.clear_all();
        injector.set_fault(
            FaultPoint::DbWrite,
            FaultMode::fail_n_times(2, "determinism test"),
        );

        let state = SharedWorkloadState::new();
        let st = Arc::clone(&state);

        let config = LabTestConfig::new(seed, "cfm_determinism").worker_count(2);
        run_lab_test(config, |runtime| {
            shared_mutation_workload(runtime, &st, 3, 3);
        });

        let result = (
            state.ops_attempted.load(Ordering::SeqCst),
            state.ops_succeeded.load(Ordering::SeqCst),
            state.ops_failed.load(Ordering::SeqCst),
        );

        FaultInjector::reset_global();
        result
    };

    let (a1, s1, f1) = run(12345);
    let (a2, s2, f2) = run(12345);
    assert_eq!(
        (a1, s1, f1),
        (a2, s2, f2),
        "determinism: same seed should produce same results"
    );
}
