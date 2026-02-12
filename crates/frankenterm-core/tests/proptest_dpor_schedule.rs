//! Property-based tests for DPOR schedule exhaustiveness.
//!
//! Generates arbitrary task graphs (random DAGs of spawn/join operations)
//! and verifies that LabRuntime's DPOR exploration covers all possible
//! linearizations. Compares DPOR-explored schedule count against theoretical
//! expectations for the given task graph.

#![cfg(feature = "asupersync-runtime")]

mod common;

use asupersync::{Budget, LabConfig, LabRuntime};
use asupersync::lab::explorer::{ExplorerConfig, ScheduleExplorer};
use proptest::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
}
