#![cfg(feature = "asupersync-runtime")]

use frankenterm_core::cx::{
    Cx, CxRuntimeBuilder, RuntimeTuning, for_testing, spawn_with_cx, try_spawn_with_cx, with_cx,
    with_cx_async,
};
use proptest::prelude::*;

fn depth_checkpoint(cx: &Cx, depth: u8) -> u8 {
    if depth == 0 {
        cx.checkpoint().expect("checkpoint should succeed");
        return 0;
    }
    with_cx(cx, |inner| 1 + depth_checkpoint(inner, depth - 1))
}

proptest! {
    #[test]
    fn proptest_with_cx_threads_capability(depth in 0u8..40) {
        let root_cx = for_testing();
        let observed = with_cx(&root_cx, |cx| depth_checkpoint(cx, depth));
        prop_assert_eq!(observed, depth);
    }

    #[test]
    fn proptest_with_cx_async_threads_capability(depth in 0u8..40) {
        let runtime = CxRuntimeBuilder::current_thread()
            .with_tuning(RuntimeTuning {
                worker_threads: 1,
                poll_budget: 64,
                blocking_min_threads: 0,
                blocking_max_threads: 0,
            })
            .build()
            .expect("runtime build should succeed");

        let root_cx = for_testing();
        let observed = runtime.block_on(with_cx_async(&root_cx, |cx| {
            let owned_cx = cx.clone();
            async move { depth_checkpoint(&owned_cx, depth) }
        }));
        prop_assert_eq!(observed, depth);
    }

    #[test]
    fn proptest_runtime_builder_applies_tuning(
        worker_threads in 1usize..=4,
        poll_budget in 1u32..=256,
        blocking_min_threads in 0usize..=4,
        blocking_slack in 0usize..=4
    ) {
        let blocking_max_threads = blocking_min_threads + blocking_slack;
        let runtime = CxRuntimeBuilder::current_thread()
            .with_tuning(RuntimeTuning {
                worker_threads,
                poll_budget,
                blocking_min_threads,
                blocking_max_threads,
            })
            .build()
            .expect("runtime build should succeed");

        let config = runtime.config();
        prop_assert_eq!(config.worker_threads, worker_threads);
        prop_assert_eq!(config.poll_budget, poll_budget);
        prop_assert_eq!(config.blocking.min_threads, blocking_min_threads);
        prop_assert_eq!(config.blocking.max_threads, blocking_max_threads);
    }

    #[test]
    fn proptest_spawn_helpers_preserve_threaded_cx(
        direct_depth in 0u8..20,
        fallible_depth in 0u8..20
    ) {
        let runtime = CxRuntimeBuilder::current_thread()
            .with_tuning(RuntimeTuning {
                worker_threads: 1,
                poll_budget: 64,
                blocking_min_threads: 0,
                blocking_max_threads: 0,
            })
            .build()
            .expect("runtime build should succeed");

        let root_cx = for_testing();
        let handle = runtime.handle();

        let direct = spawn_with_cx(&handle, &root_cx, move |child_cx| async move {
            depth_checkpoint(&child_cx, direct_depth)
        });
        let direct_output = runtime.block_on(direct);

        let fallible = try_spawn_with_cx(&handle, &root_cx, move |child_cx| async move {
            depth_checkpoint(&child_cx, fallible_depth)
        })
        .expect("task admission should succeed");
        let fallible_output = runtime.block_on(fallible);

        prop_assert_eq!(direct_output, direct_depth);
        prop_assert_eq!(fallible_output, fallible_depth);
    }
}
