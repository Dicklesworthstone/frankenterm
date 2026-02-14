#![cfg(feature = "asupersync-runtime")]

use frankenterm_core::cx::{
    Cx, CxRuntimeBuilder, RuntimeTuning, for_testing, spawn_bounded_with_cx, spawn_with_cx,
    spawn_with_timeout, try_spawn_with_cx, with_cx,
};
use frankenterm_core::runtime_compat;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn thread_depth(cx: &Cx, depth: usize) -> usize {
    if depth == 0 {
        cx.checkpoint().expect("checkpoint should succeed");
        return 0;
    }

    with_cx(cx, |inner| 1 + thread_depth(inner, depth - 1))
}

#[test]
fn runtime_builder_current_thread_applies_tuning() {
    let runtime = CxRuntimeBuilder::current_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 1,
            poll_budget: 64,
            blocking_min_threads: 0,
            blocking_max_threads: 0,
        })
        .build()
        .expect("build current-thread runtime");

    let config = runtime.config();
    assert_eq!(config.worker_threads, 1);
    assert_eq!(config.poll_budget, 64);
    assert_eq!(config.blocking.min_threads, 0);
    assert_eq!(config.blocking.max_threads, 0);
}

#[test]
fn runtime_builder_multi_thread_applies_tuning() {
    let runtime = CxRuntimeBuilder::multi_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 3,
            poll_budget: 96,
            blocking_min_threads: 2,
            blocking_max_threads: 4,
        })
        .build()
        .expect("build multi-thread runtime");

    let config = runtime.config();
    assert_eq!(config.worker_threads, 3);
    assert_eq!(config.poll_budget, 96);
    assert_eq!(config.blocking.min_threads, 2);
    assert_eq!(config.blocking.max_threads, 4);
}

#[test]
fn spawn_helpers_thread_cx_into_tasks() {
    let runtime = CxRuntimeBuilder::current_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 1,
            poll_budget: 64,
            blocking_min_threads: 0,
            blocking_max_threads: 0,
        })
        .build()
        .expect("build runtime");

    let root_cx = for_testing();
    let handle = runtime.handle();

    let direct = spawn_with_cx(&handle, &root_cx, |child_cx| async move {
        thread_depth(&child_cx, 5)
    });
    assert_eq!(runtime.block_on(direct), 5);

    let fallible = try_spawn_with_cx(&handle, &root_cx, |child_cx| async move {
        with_cx(&child_cx, |inner| thread_depth(inner, 8))
    })
    .expect("task admission should succeed");
    assert_eq!(runtime.block_on(fallible), 8);
}

#[test]
fn spawn_bounded_helper_limits_concurrency_and_preserves_order() {
    let runtime = CxRuntimeBuilder::multi_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 4,
            poll_budget: 64,
            blocking_min_threads: 0,
            blocking_max_threads: 0,
        })
        .build()
        .expect("build runtime");

    let root_cx = for_testing();
    let handle = runtime.handle();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));

    let tasks = (0usize..12)
        .map(|i| {
            let in_flight = Arc::clone(&in_flight);
            let max_seen = Arc::clone(&max_seen);
            move |_child_cx: Cx| async move {
                let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;

                let mut observed = max_seen.load(Ordering::SeqCst);
                while current > observed {
                    match max_seen.compare_exchange(
                        observed,
                        current,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(next) => observed = next,
                    }
                }

                runtime_compat::sleep(Duration::from_millis(10)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                i
            }
        })
        .collect::<Vec<_>>();

    let outputs = runtime.block_on(spawn_bounded_with_cx(&handle, &root_cx, 3, tasks));
    assert_eq!(outputs, (0usize..12).collect::<Vec<_>>());
    assert!(max_seen.load(Ordering::SeqCst) <= 3);
    assert_eq!(in_flight.load(Ordering::SeqCst), 0);
}

#[test]
fn spawn_with_timeout_returns_output_before_deadline() {
    let runtime = CxRuntimeBuilder::current_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 1,
            poll_budget: 64,
            blocking_min_threads: 0,
            blocking_max_threads: 0,
        })
        .build()
        .expect("build runtime");

    let root_cx = for_testing();
    let handle = runtime.handle();

    let output = runtime.block_on(spawn_with_timeout(
        &handle,
        &root_cx,
        Duration::from_millis(100),
        |_child_cx| async move { 42usize },
    ));

    assert_eq!(output.expect("should finish before deadline"), 42);
}

#[test]
fn spawn_with_timeout_errors_when_deadline_expires() {
    let runtime = CxRuntimeBuilder::current_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 1,
            poll_budget: 64,
            blocking_min_threads: 0,
            blocking_max_threads: 0,
        })
        .build()
        .expect("build runtime");

    let root_cx = for_testing();
    let handle = runtime.handle();

    let result = runtime.block_on(spawn_with_timeout(
        &handle,
        &root_cx,
        Duration::from_millis(5),
        |_child_cx| async move {
            runtime_compat::sleep(Duration::from_millis(50)).await;
            7usize
        },
    ));

    assert!(result.is_err());
}
