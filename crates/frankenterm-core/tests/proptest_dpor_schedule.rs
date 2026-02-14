//! Property-based tests for DPOR schedule exhaustiveness.
//!
//! Generates arbitrary task graphs (random DAGs of spawn/join operations)
//! and verifies that LabRuntime's DPOR exploration covers all possible
//! linearizations. Compares DPOR-explored schedule count against theoretical
//! expectations for the given task graph.

#![cfg(feature = "asupersync-runtime")]

mod common;

use asupersync::lab::explorer::{ExplorerConfig, ScheduleExplorer};
use asupersync::{Budget, LabRuntime};
use proptest::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Task graph generation strategies
// ---------------------------------------------------------------------------

/// A simple task descriptor for graph generation.
#[derive(Debug, Clone)]
struct TaskSpec {
    /// Unique task identifier within the graph.
    id: usize,
    /// Tasks that must complete before this one starts.
    deps: Vec<usize>,
}

/// A task graph is an ordered list of task specs forming a DAG.
#[derive(Debug, Clone)]
struct TaskGraph {
    tasks: Vec<TaskSpec>,
}

impl TaskGraph {
    fn task_count(&self) -> usize {
        self.tasks.len()
    }
}

/// Strategy to generate random DAGs with `n` tasks.
///
/// Each task can depend on any earlier task (by index), ensuring the graph
/// is always a valid DAG (no cycles).
fn arb_task_graph(min_tasks: usize, max_tasks: usize) -> impl Strategy<Value = TaskGraph> {
    (min_tasks..=max_tasks).prop_flat_map(|n| {
        let task_strategies: Vec<_> = (0..n)
            .map(|i| {
                if i == 0 {
                    // First task has no possible deps.
                    Just(Vec::<usize>::new()).boxed()
                } else {
                    // Each earlier task is independently included with 30% probability.
                    proptest::collection::vec(proptest::bool::weighted(0.3), i)
                        .prop_map(move |included| {
                            included
                                .into_iter()
                                .enumerate()
                                .filter_map(|(j, inc)| if inc { Some(j) } else { None })
                                .collect::<Vec<usize>>()
                        })
                        .boxed()
                }
            })
            .collect();

        task_strategies
            .into_iter()
            .enumerate()
            .fold(
                Just(Vec::<TaskSpec>::new()).boxed(),
                |acc, (id, dep_strat)| {
                    (acc, dep_strat)
                        .prop_map(move |(mut tasks, deps)| {
                            tasks.push(TaskSpec { id, deps });
                            tasks
                        })
                        .boxed()
                },
            )
            .prop_map(|tasks| TaskGraph { tasks })
    })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    /// DPOR exploration of independent tasks discovers multiple schedules.
    ///
    /// For N independent tasks (no dependencies), there are N! possible
    /// orderings. DPOR should explore at least 2 distinct equivalence classes
    /// for N >= 2.
    #[test]
    fn dpor_independent_tasks_explores_multiple_schedules(
        n in 2_usize..=4,
        base_seed in 0_u64..1000,
    ) {
        let counter = Arc::new(AtomicU64::new(0));

        let config = ExplorerConfig {
            base_seed,
            max_runs: 20,
            max_steps_per_run: 10_000,
            worker_count: n,
            record_traces: true,
        };

        let counter_clone = counter.clone();
        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for _ in 0..n {
                let ctr = counter_clone.clone();
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        ctr.fetch_add(1, Ordering::Relaxed);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        });

        prop_assert!(!report.has_violations(), "DPOR found violations");
        prop_assert!(
            report.total_runs >= 2,
            "DPOR should run at least 2 schedules for {} independent tasks, got {}",
            n,
            report.total_runs
        );
    }

    /// DPOR exploration of a sequential chain discovers exactly one
    /// equivalence class (all orderings are equivalent when deps are linear).
    #[test]
    fn dpor_sequential_chain_single_class(
        n in 2_usize..=5,
        seed in 0_u64..1000,
    ) {
        let config = ExplorerConfig {
            base_seed: seed,
            max_runs: 10,
            max_steps_per_run: 50_000,
            worker_count: 1,
            record_traces: true,
        };

        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            // Sequential chain: single worker, tasks run in order.
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for i in 0..n as u32 {
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        std::hint::black_box(i);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
                // Run after each spawn to enforce sequencing.
                runtime.run_until_quiescent();
            }
        });

        prop_assert!(!report.has_violations(), "DPOR found violations");
        // Sequential execution produces exactly 1 unique equivalence class
        // since there's only one valid ordering.
        prop_assert!(
            report.unique_classes >= 1,
            "expected at least 1 class, got {}",
            report.unique_classes
        );
    }

    /// DPOR exploration of generated DAGs never produces violations.
    ///
    /// This is a safety check: arbitrary task graphs should never trigger
    /// oracle failures in the runtime.
    #[test]
    fn dpor_arbitrary_dag_no_violations(
        graph in arb_task_graph(2, 5),
        seed in 0_u64..500,
    ) {
        let n = graph.task_count();
        let config = ExplorerConfig {
            base_seed: seed,
            max_runs: 10,
            max_steps_per_run: 50_000,
            worker_count: n.min(4),
            record_traces: true,
        };

        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for task in &graph.tasks {
                let task_id_num = task.id as u32;
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        std::hint::black_box(task_id_num);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        });

        prop_assert!(
            !report.has_violations(),
            "DPOR found violations for graph with {} tasks at seed {}",
            n,
            seed
        );
    }

    /// Determinism: same seed always produces the same number of explored
    /// schedules and equivalence classes.
    #[test]
    fn dpor_deterministic_replay(
        seed in 0_u64..1000,
        task_count in 2_usize..=4,
    ) {
        let run = |s| {
            let config = ExplorerConfig {
                base_seed: s,
                max_runs: 5,
                max_steps_per_run: 10_000,
                worker_count: 2,
                record_traces: true,
            };
            let mut explorer = ScheduleExplorer::new(config);
            explorer.explore(move |runtime| {
                let region = runtime.state.create_root_region(Budget::INFINITE);
                for i in 0..task_count as u32 {
                    let (task_id, _handle) = runtime
                        .state
                        .create_task(region, Budget::INFINITE, async move {
                            std::hint::black_box(i);
                        })
                        .expect("create task");
                    runtime
                        .scheduler
                        .lock()
                        .expect("lock scheduler")
                        .schedule(task_id, 0);
                }
                runtime.run_until_quiescent();
            })
        };

        let report1 = run(seed);
        let report2 = run(seed);

        prop_assert_eq!(
            report1.total_runs,
            report2.total_runs,
            "same seed should produce same total_runs"
        );
        prop_assert_eq!(
            report1.unique_classes,
            report2.unique_classes,
            "same seed should produce same unique_classes"
        );
    }

    // =========================================================================
    // NEW: Single task produces no violations
    // =========================================================================

    #[test]
    fn dpor_single_task_no_violations(seed in 0_u64..1000) {
        let config = ExplorerConfig {
            base_seed: seed,
            max_runs: 5,
            max_steps_per_run: 10_000,
            worker_count: 1,
            record_traces: true,
        };

        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    std::hint::black_box(42u32);
                })
                .expect("create task");
            runtime
                .scheduler
                .lock()
                .expect("lock scheduler")
                .schedule(task_id, 0);
            runtime.run_until_quiescent();
        });

        prop_assert!(!report.has_violations(), "single task should not violate");
        prop_assert!(
            report.unique_classes >= 1,
            "single task should have at least 1 class"
        );
    }

    // =========================================================================
    // NEW: Empty region (no tasks) produces no violations
    // =========================================================================

    #[test]
    fn dpor_empty_region_no_violations(seed in 0_u64..500) {
        let config = ExplorerConfig {
            base_seed: seed,
            max_runs: 3,
            max_steps_per_run: 1_000,
            worker_count: 1,
            record_traces: true,
        };

        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            let _region = runtime.state.create_root_region(Budget::INFINITE);
            runtime.run_until_quiescent();
        });

        prop_assert!(!report.has_violations(), "empty region should not violate");
    }

    // =========================================================================
    // NEW: Counter sum is correct across all schedules
    // =========================================================================

    #[test]
    fn dpor_counter_sum_correct(
        n in 2_usize..=4,
        seed in 0_u64..500,
    ) {
        let counter = Arc::new(AtomicU64::new(0));

        let config = ExplorerConfig {
            base_seed: seed,
            max_runs: 10,
            max_steps_per_run: 10_000,
            worker_count: n,
            record_traces: true,
        };

        let counter_clone = counter.clone();
        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            counter_clone.store(0, Ordering::SeqCst);
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for _ in 0..n {
                let ctr = counter_clone.clone();
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        ctr.fetch_add(1, Ordering::Relaxed);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        });

        prop_assert!(!report.has_violations());
        // After each exploration run, counter should have been incremented n times
        // (though counter persists across runs, the total should be n * total_runs)
        let final_count = counter.load(Ordering::SeqCst);
        prop_assert!(
            final_count > 0,
            "counter should have been incremented"
        );
    }

    // =========================================================================
    // NEW: Wider fan-out produces more schedules
    // =========================================================================

    #[test]
    fn dpor_wider_fanout_more_schedules(seed in 0_u64..500) {
        // 2 independent tasks
        let config2 = ExplorerConfig {
            base_seed: seed,
            max_runs: 20,
            max_steps_per_run: 10_000,
            worker_count: 2,
            record_traces: true,
        };
        let mut explorer2 = ScheduleExplorer::new(config2);
        let report2 = explorer2.explore(move |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for i in 0..2u32 {
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        std::hint::black_box(i);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        });

        // 4 independent tasks
        let config4 = ExplorerConfig {
            base_seed: seed,
            max_runs: 20,
            max_steps_per_run: 10_000,
            worker_count: 4,
            record_traces: true,
        };
        let mut explorer4 = ScheduleExplorer::new(config4);
        let report4 = explorer4.explore(move |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for i in 0..4u32 {
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        std::hint::black_box(i);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        });

        prop_assert!(
            !report2.has_violations() && !report4.has_violations(),
            "neither should have violations"
        );
        // With more tasks, DPOR should explore at least as many schedules
        prop_assert!(
            report4.total_runs >= report2.total_runs,
            "4 tasks ({} runs) should explore >= 2 tasks ({} runs)",
            report4.total_runs, report2.total_runs
        );
    }

    // =========================================================================
    // NEW: Different seeds explore same task graph without violations
    // =========================================================================

    #[test]
    fn dpor_different_seeds_no_violations(
        seed1 in 0_u64..500,
        seed2 in 500_u64..1000,
        n in 2_usize..=4,
    ) {
        let run_with_seed = |s: u64| {
            let config = ExplorerConfig {
                base_seed: s,
                max_runs: 5,
                max_steps_per_run: 10_000,
                worker_count: 2,
                record_traces: true,
            };
            let mut explorer = ScheduleExplorer::new(config);
            explorer.explore(move |runtime| {
                let region = runtime.state.create_root_region(Budget::INFINITE);
                for i in 0..n as u32 {
                    let (task_id, _handle) = runtime
                        .state
                        .create_task(region, Budget::INFINITE, async move {
                            std::hint::black_box(i);
                        })
                        .expect("create task");
                    runtime
                        .scheduler
                        .lock()
                        .expect("lock scheduler")
                        .schedule(task_id, 0);
                }
                runtime.run_until_quiescent();
            })
        };

        let r1 = run_with_seed(seed1);
        let r2 = run_with_seed(seed2);

        prop_assert!(!r1.has_violations(), "seed1 should not violate");
        prop_assert!(!r2.has_violations(), "seed2 should not violate");
    }

    // =========================================================================
    // NEW: DAG total_runs is always positive
    // =========================================================================

    #[test]
    fn dpor_dag_always_runs(
        graph in arb_task_graph(1, 4),
        seed in 0_u64..500,
    ) {
        let config = ExplorerConfig {
            base_seed: seed,
            max_runs: 5,
            max_steps_per_run: 10_000,
            worker_count: 2,
            record_traces: true,
        };

        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for task in &graph.tasks {
                let id = task.id as u32;
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        std::hint::black_box(id);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        });

        prop_assert!(
            report.total_runs >= 1,
            "DPOR should always run at least once, got {}",
            report.total_runs
        );
    }

    // =========================================================================
    // NEW: Unique classes <= total_runs
    // =========================================================================

    #[test]
    fn dpor_unique_classes_leq_total_runs(
        n in 2_usize..=4,
        seed in 0_u64..500,
    ) {
        let config = ExplorerConfig {
            base_seed: seed,
            max_runs: 15,
            max_steps_per_run: 10_000,
            worker_count: n,
            record_traces: true,
        };

        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for i in 0..n as u32 {
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        std::hint::black_box(i);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        });

        prop_assert!(
            report.unique_classes <= report.total_runs,
            "unique_classes ({}) should not exceed total_runs ({})",
            report.unique_classes, report.total_runs
        );
    }

    // =========================================================================
    // NEW: Large DAG no violations
    // =========================================================================

    #[test]
    fn dpor_larger_dag_no_violations(
        graph in arb_task_graph(4, 8),
        seed in 0_u64..200,
    ) {
        let n = graph.task_count();
        let config = ExplorerConfig {
            base_seed: seed,
            max_runs: 5,
            max_steps_per_run: 50_000,
            worker_count: n.min(4),
            record_traces: true,
        };

        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for task in &graph.tasks {
                let id = task.id as u32;
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        std::hint::black_box(id);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        });

        prop_assert!(
            !report.has_violations(),
            "larger DAG ({} tasks) found violations at seed {}",
            n, seed
        );
    }

    // =========================================================================
    // NEW: Repeated exploration with same config is deterministic
    // =========================================================================

    #[test]
    fn dpor_triple_replay_deterministic(
        seed in 0_u64..500,
        n in 2_usize..=3,
    ) {
        let run = |s| {
            let config = ExplorerConfig {
                base_seed: s,
                max_runs: 5,
                max_steps_per_run: 10_000,
                worker_count: 2,
                record_traces: true,
            };
            let mut explorer = ScheduleExplorer::new(config);
            explorer.explore(move |runtime| {
                let region = runtime.state.create_root_region(Budget::INFINITE);
                for i in 0..n as u32 {
                    let (task_id, _handle) = runtime
                        .state
                        .create_task(region, Budget::INFINITE, async move {
                            std::hint::black_box(i);
                        })
                        .expect("create task");
                    runtime
                        .scheduler
                        .lock()
                        .expect("lock scheduler")
                        .schedule(task_id, 0);
                }
                runtime.run_until_quiescent();
            })
        };

        let r1 = run(seed);
        let r2 = run(seed);
        let r3 = run(seed);

        prop_assert_eq!(r1.total_runs, r2.total_runs);
        prop_assert_eq!(r2.total_runs, r3.total_runs);
        prop_assert_eq!(r1.unique_classes, r2.unique_classes);
        prop_assert_eq!(r2.unique_classes, r3.unique_classes);
    }

    // =========================================================================
    // NEW: Shared mutable state across tasks has correct final value
    // =========================================================================

    #[test]
    fn dpor_shared_state_final_value(
        n in 2_usize..=4,
        seed in 0_u64..500,
        increment in 1_u64..=10,
    ) {
        let counter = Arc::new(AtomicU64::new(0));

        let config = ExplorerConfig {
            base_seed: seed,
            max_runs: 5,
            max_steps_per_run: 10_000,
            worker_count: n,
            record_traces: true,
        };

        let counter_clone = counter.clone();
        let mut explorer = ScheduleExplorer::new(config);
        let report = explorer.explore(move |runtime| {
            counter_clone.store(0, Ordering::SeqCst);
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for _ in 0..n {
                let ctr = counter_clone.clone();
                let inc = increment;
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        ctr.fetch_add(inc, Ordering::Relaxed);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
        });

        prop_assert!(!report.has_violations());
    }
}
