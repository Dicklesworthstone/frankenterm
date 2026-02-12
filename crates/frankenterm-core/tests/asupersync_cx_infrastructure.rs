#![cfg(feature = "asupersync-runtime")]

use frankenterm_core::cx::{
    Cx, CxRuntimeBuilder, RuntimeTuning, for_testing, spawn_with_cx, try_spawn_with_cx, with_cx,
};

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
