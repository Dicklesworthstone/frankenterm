//! LabRuntime coverage for tailer concurrency and channel semantics.
//!
//! These tests target `ft-124z4` requirements with deterministic schedule
//! exploration and avoid wall-clock sleeps.

#![cfg(feature = "asupersync-runtime")]

mod common;

use asupersync::{Budget, LabRuntime, TaskId};
use common::lab::{ExplorationTestConfig, LabTestConfig, run_exploration_test, run_lab_test};
use frankenterm_core::ingest::{PaneCursor, PaneRegistry};
use frankenterm_core::runtime_compat::{self, RwLock, mpsc};
use frankenterm_core::tailer::{TailerConfig, TailerPollTaskSet, TailerSupervisor};
use frankenterm_core::wezterm::{PaneInfo, PaneTextSource};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

type PaneFuture<'a> = Pin<Box<dyn Future<Output = frankenterm_core::Result<String>> + Send + 'a>>;

#[derive(Clone)]
struct CountingYieldSource {
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    yield_steps: usize,
}

impl CountingYieldSource {
    fn new(
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
        yield_steps: usize,
    ) -> Self {
        Self {
            active,
            max_active,
            calls,
            yield_steps,
        }
    }
}

impl PaneTextSource for CountingYieldSource {
    type Fut<'a> = PaneFuture<'a>;

    fn get_text(&self, pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
        let active = Arc::clone(&self.active);
        let max_active = Arc::clone(&self.max_active);
        let calls = Arc::clone(&self.calls);
        let yield_steps = self.yield_steps;

        Box::pin(async move {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            max_active.fetch_max(current, Ordering::SeqCst);
            calls.fetch_add(1, Ordering::SeqCst);

            for _ in 0..yield_steps {
                asupersync::runtime::yield_now().await;
            }

            active.fetch_sub(1, Ordering::SeqCst);
            Ok(format!("pane-{pane_id}"))
        })
    }
}

fn make_pane(pane_id: u64) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id: 0,
        window_id: 0,
        domain_id: None,
        domain_name: None,
        workspace: None,
        size: None,
        rows: Some(24),
        cols: Some(80),
        title: None,
        cwd: None,
        tty_name: None,
        cursor_x: Some(0),
        cursor_y: Some(0),
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: pane_id == 1,
        is_zoomed: false,
        extra: std::collections::HashMap::new(),
    }
}

fn schedule_task(runtime: &mut LabRuntime, task_id: TaskId) {
    runtime
        .scheduler
        .lock()
        .expect("lock scheduler")
        .schedule(task_id, 0);
}

#[test]
fn dpor_tailer_max_concurrent_limit_respected() {
    let config = ExplorationTestConfig::new("tailer_max_concurrent_limit", 10)
        .base_seed(401)
        .worker_count(3)
        .max_steps_per_run(120_000);

    let report = run_exploration_test(config, |runtime| {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingYieldSource::new(
            Arc::clone(&active),
            Arc::clone(&max_active),
            Arc::clone(&calls),
            8,
        ));

        let (tx, rx) = mpsc::channel(64);
        let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut panes = HashMap::new();
        for pane_id in 1..=6_u64 {
            panes.insert(pane_id, make_pane(pane_id));
        }

        let task_source = Arc::clone(&source);
        let task_cursors = Arc::clone(&cursors);
        let task_registry = Arc::clone(&registry);
        let task_shutdown = Arc::clone(&shutdown);

        let region = runtime.state.create_root_region(Budget::INFINITE);
        let (task_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let _keep_rx_alive = rx;
                let tailer_config = TailerConfig {
                    min_interval: Duration::ZERO,
                    max_interval: Duration::from_millis(1),
                    max_concurrent: 2,
                    send_timeout: Duration::from_millis(250),
                    capture_timeout: Duration::from_millis(250),
                    ..Default::default()
                };

                {
                    let mut guard = task_cursors.write().await;
                    for pane_id in 1..=6_u64 {
                        guard.insert(pane_id, PaneCursor::new(pane_id));
                    }
                }

                let mut supervisor = TailerSupervisor::new(
                    tailer_config,
                    tx,
                    task_cursors,
                    task_registry,
                    task_shutdown,
                    task_source,
                );
                supervisor.sync_tailers(&panes);

                let mut poll_tasks = TailerPollTaskSet::new();
                supervisor.spawn_ready(&mut poll_tasks);
                tracing::info!(spawned = poll_tasks.len(), "tailer dpor concurrency spawn");
                assert_eq!(poll_tasks.len(), 2);

                while let Some((pane_id, outcome)) = poll_tasks.join_next().await {
                    tracing::debug!(pane_id, ?outcome, "tailer dpor task completed");
                    supervisor.handle_poll_result(pane_id, outcome);
                }
                assert_eq!(supervisor.metrics().events_sent, 2);
            })
            .expect("create tailer task");

        schedule_task(runtime, task_id);
        runtime.run_until_quiescent();

        assert!(
            max_active.load(Ordering::SeqCst) <= 2,
            "semaphore limit exceeded"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "expected two captures");
    });

    assert!(report.passed());
    assert!(report.total_runs >= 8);
}

#[test]
fn dpor_tailer_event_delivery_under_concurrent_load() {
    let config = ExplorationTestConfig::new("tailer_event_delivery_under_load", 8)
        .base_seed(733)
        .worker_count(4)
        .max_steps_per_run(120_000);

    let report = run_exploration_test(config, |runtime| {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(CountingYieldSource::new(
            Arc::clone(&active),
            Arc::clone(&max_active),
            Arc::clone(&calls),
            4,
        ));

        let (tx, mut rx) = mpsc::channel(128);
        let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let pane_count = 8_u64;
        let mut panes = HashMap::new();
        for pane_id in 1..=pane_count {
            panes.insert(pane_id, make_pane(pane_id));
        }

        let task_source = Arc::clone(&source);
        let task_cursors = Arc::clone(&cursors);
        let task_registry = Arc::clone(&registry);
        let task_shutdown = Arc::clone(&shutdown);

        let region = runtime.state.create_root_region(Budget::INFINITE);
        let (task_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let tailer_config = TailerConfig {
                    min_interval: Duration::ZERO,
                    max_interval: Duration::from_millis(1),
                    max_concurrent: pane_count as usize,
                    send_timeout: Duration::from_millis(250),
                    capture_timeout: Duration::from_millis(250),
                    ..Default::default()
                };

                {
                    let mut guard = task_cursors.write().await;
                    for pane_id in 1..=pane_count {
                        guard.insert(pane_id, PaneCursor::new(pane_id));
                    }
                }

                let mut supervisor = TailerSupervisor::new(
                    tailer_config,
                    tx,
                    task_cursors,
                    task_registry,
                    task_shutdown,
                    task_source,
                );
                supervisor.sync_tailers(&panes);

                let mut poll_tasks = TailerPollTaskSet::new();
                supervisor.spawn_ready(&mut poll_tasks);
                tracing::info!(spawned = poll_tasks.len(), "tailer dpor load spawn");
                assert_eq!(poll_tasks.len(), pane_count as usize);

                while let Some((pane_id, outcome)) = poll_tasks.join_next().await {
                    tracing::debug!(pane_id, ?outcome, "tailer dpor load completed");
                    supervisor.handle_poll_result(pane_id, outcome);
                }

                let mut seen = HashSet::new();
                for _ in 0..pane_count {
                    let event = runtime_compat::mpsc_recv_option(&mut rx)
                        .await
                        .expect("expected capture event");
                    seen.insert(event.segment.pane_id);
                }
                assert_eq!(seen.len(), pane_count as usize);
                assert_eq!(supervisor.metrics().events_sent, pane_count);
            })
            .expect("create tailer task");

        schedule_task(runtime, task_id);
        runtime.run_until_quiescent();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            pane_count as usize,
            "every pane should capture once"
        );
        assert!(
            max_active.load(Ordering::SeqCst) >= 2,
            "expected interleaving under concurrent load"
        );
    });

    assert!(report.passed());
}

#[test]
fn lab_tailer_shutdown_prevents_follow_up_spawns() {
    let report = run_lab_test(
        LabTestConfig::new(909, "tailer_shutdown_prevents_follow_up_spawns")
            .worker_count(2)
            .max_steps(120_000),
        |runtime| {
            let active = Arc::new(AtomicUsize::new(0));
            let max_active = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let source = Arc::new(CountingYieldSource::new(
                Arc::clone(&active),
                Arc::clone(&max_active),
                Arc::clone(&calls),
                6,
            ));

            let (tx, rx) = mpsc::channel(64);
            let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
            let registry = Arc::new(RwLock::new(PaneRegistry::new()));
            let shutdown = Arc::new(AtomicBool::new(false));

            let mut panes = HashMap::new();
            for pane_id in 1..=5_u64 {
                panes.insert(pane_id, make_pane(pane_id));
            }

            let task_source = Arc::clone(&source);
            let task_cursors = Arc::clone(&cursors);
            let task_registry = Arc::clone(&registry);
            let task_shutdown = Arc::clone(&shutdown);

            let region = runtime.state.create_root_region(Budget::INFINITE);
            let (task_id, _) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let _keep_rx_alive = rx;
                    let tailer_config = TailerConfig {
                        min_interval: Duration::ZERO,
                        max_interval: Duration::from_millis(1),
                        max_concurrent: 3,
                        send_timeout: Duration::from_millis(250),
                        capture_timeout: Duration::from_millis(250),
                        ..Default::default()
                    };

                    {
                        let mut guard = task_cursors.write().await;
                        for pane_id in 1..=5_u64 {
                            guard.insert(pane_id, PaneCursor::new(pane_id));
                        }
                    }

                    let mut supervisor = TailerSupervisor::new(
                        tailer_config,
                        tx,
                        task_cursors,
                        task_registry,
                        Arc::clone(&task_shutdown),
                        task_source,
                    );
                    supervisor.sync_tailers(&panes);

                    let mut initial_tasks = TailerPollTaskSet::new();
                    supervisor.spawn_ready(&mut initial_tasks);
                    tracing::info!(
                        spawned = initial_tasks.len(),
                        "tailer shutdown initial spawn"
                    );
                    assert_eq!(initial_tasks.len(), 3);

                    task_shutdown.store(true, Ordering::SeqCst);

                    while let Some((pane_id, outcome)) = initial_tasks.join_next().await {
                        supervisor.handle_poll_result(pane_id, outcome);
                    }

                    let mut follow_up_tasks = TailerPollTaskSet::new();
                    supervisor.spawn_ready(&mut follow_up_tasks);
                    assert!(
                        follow_up_tasks.is_empty(),
                        "shutdown should prevent new poll task spawns"
                    );
                })
                .expect("create tailer task");

            schedule_task(runtime, task_id);
            runtime.run_until_quiescent();

            assert_eq!(calls.load(Ordering::SeqCst), 3);
            assert_eq!(active.load(Ordering::SeqCst), 0);
            assert!(max_active.load(Ordering::SeqCst) <= 3);
        },
    );

    assert!(report.passed());
}
