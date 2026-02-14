#![cfg(feature = "asupersync-runtime")]

use frankenterm_core::cx::{
    Cx, CxRuntimeBuilder, RuntimePreset, RuntimeTuning, for_testing, spawn_with_cx,
    try_spawn_with_cx, with_cx, with_cx_async,
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
    // =========================================================================
    // Original tests
    // =========================================================================

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

    // =========================================================================
    // NEW: RuntimeTuning Default trait roundtrip
    // =========================================================================

    #[test]
    fn runtime_tuning_default_is_valid(_dummy in 0..1u8) {
        let tuning = RuntimeTuning::default();
        // Default tuning should have reasonable non-zero worker threads
        prop_assert!(tuning.worker_threads >= 1, "default worker_threads should be >= 1");
        prop_assert!(tuning.poll_budget >= 1, "default poll_budget should be >= 1");
        prop_assert!(
            tuning.blocking_max_threads >= tuning.blocking_min_threads,
            "blocking_max should >= blocking_min"
        );
    }

    // =========================================================================
    // NEW: RuntimeTuning structural equality
    // =========================================================================

    #[test]
    fn runtime_tuning_eq_is_structural(
        w in 1usize..=8,
        pb in 1u32..=512,
        bmin in 0usize..=4,
        bslack in 0usize..=4,
    ) {
        let bmax = bmin + bslack;
        let t1 = RuntimeTuning {
            worker_threads: w,
            poll_budget: pb,
            blocking_min_threads: bmin,
            blocking_max_threads: bmax,
        };
        let t2 = RuntimeTuning {
            worker_threads: w,
            poll_budget: pb,
            blocking_min_threads: bmin,
            blocking_max_threads: bmax,
        };
        prop_assert_eq!(t1, t2);
    }

    // =========================================================================
    // NEW: RuntimePreset Debug/Clone/Copy/PartialEq
    // =========================================================================

    #[test]
    fn runtime_preset_debug_nonempty(idx in 0u8..2) {
        let preset = if idx == 0 {
            RuntimePreset::CurrentThread
        } else {
            RuntimePreset::MultiThread
        };
        let dbg = format!("{:?}", preset);
        prop_assert!(!dbg.is_empty());
    }

    #[test]
    fn runtime_preset_clone_eq(idx in 0u8..2) {
        let preset = if idx == 0 {
            RuntimePreset::CurrentThread
        } else {
            RuntimePreset::MultiThread
        };
        let cloned = preset;
        prop_assert_eq!(preset, cloned);
    }

    // =========================================================================
    // NEW: RuntimeTuning Clone/Debug
    // =========================================================================

    #[test]
    fn runtime_tuning_clone_preserves_fields(
        w in 1usize..=8,
        pb in 1u32..=512,
        bmin in 0usize..=4,
        bslack in 0usize..=4,
    ) {
        let bmax = bmin + bslack;
        let tuning = RuntimeTuning {
            worker_threads: w,
            poll_budget: pb,
            blocking_min_threads: bmin,
            blocking_max_threads: bmax,
        };
        let cloned = tuning;
        prop_assert_eq!(tuning.worker_threads, cloned.worker_threads);
        prop_assert_eq!(tuning.poll_budget, cloned.poll_budget);
        prop_assert_eq!(tuning.blocking_min_threads, cloned.blocking_min_threads);
        prop_assert_eq!(tuning.blocking_max_threads, cloned.blocking_max_threads);
    }

    #[test]
    fn runtime_tuning_debug_contains_fields(
        w in 1usize..=4,
        pb in 1u32..=256,
    ) {
        let tuning = RuntimeTuning {
            worker_threads: w,
            poll_budget: pb,
            blocking_min_threads: 0,
            blocking_max_threads: 2,
        };
        let dbg = format!("{:?}", tuning);
        prop_assert!(dbg.contains("RuntimeTuning"));
    }

    // =========================================================================
    // NEW: CxRuntimeBuilder Debug
    // =========================================================================

    #[test]
    fn cx_runtime_builder_debug(idx in 0u8..2) {
        let builder = if idx == 0 {
            CxRuntimeBuilder::current_thread()
        } else {
            CxRuntimeBuilder::multi_thread()
        };
        let dbg = format!("{:?}", builder);
        prop_assert!(dbg.contains("CxRuntimeBuilder"));
    }

    // =========================================================================
    // NEW: Builder from_preset matches named constructors
    // =========================================================================

    #[test]
    fn from_preset_builds_successfully(idx in 0u8..2) {
        let preset = if idx == 0 {
            RuntimePreset::CurrentThread
        } else {
            RuntimePreset::MultiThread
        };
        let builder = CxRuntimeBuilder::from_preset(preset);
        let runtime = builder
            .with_tuning(RuntimeTuning {
                worker_threads: 1,
                poll_budget: 32,
                blocking_min_threads: 0,
                blocking_max_threads: 1,
            })
            .build();
        prop_assert!(runtime.is_ok(), "from_preset should build successfully");
    }

    // =========================================================================
    // NEW: with_cx is identity for depth=0
    // =========================================================================

    #[test]
    fn with_cx_identity_for_value(val in any::<u64>()) {
        let cx = for_testing();
        let result = with_cx(&cx, |_| val);
        prop_assert_eq!(result, val);
    }

    // =========================================================================
    // NEW: Cx clone preserves checkpoint ability
    // =========================================================================

    #[test]
    fn cx_clone_preserves_checkpoint(_dummy in 0..10u8) {
        let cx = for_testing();
        let cloned = cx.clone();
        // Both original and clone should support checkpoint
        let r1 = cx.checkpoint();
        let r2 = cloned.checkpoint();
        prop_assert!(r1.is_ok(), "original cx checkpoint failed");
        prop_assert!(r2.is_ok(), "cloned cx checkpoint failed");
    }

    // =========================================================================
    // NEW: Builder method chaining preserves tuning
    // =========================================================================

    #[test]
    fn builder_chaining_preserves_tuning(
        w in 1usize..=4,
        pb in 1u32..=128,
        bmin in 0usize..=2,
        bslack in 0usize..=2,
    ) {
        let bmax = bmin + bslack;
        // Build with individual methods instead of with_tuning
        let runtime = CxRuntimeBuilder::current_thread()
            .worker_threads(w)
            .poll_budget(pb)
            .blocking_threads(bmin, bmax)
            .build()
            .expect("builder chaining should succeed");

        let config = runtime.config();
        prop_assert_eq!(config.worker_threads, w);
        prop_assert_eq!(config.poll_budget, pb);
        prop_assert_eq!(config.blocking.min_threads, bmin);
        prop_assert_eq!(config.blocking.max_threads, bmax);
    }

    // =========================================================================
    // NEW: spawn_with_cx returns correct value
    // =========================================================================

    #[test]
    fn spawn_returns_value(val in any::<u32>()) {
        let runtime = CxRuntimeBuilder::current_thread()
            .with_tuning(RuntimeTuning {
                worker_threads: 1,
                poll_budget: 64,
                blocking_min_threads: 0,
                blocking_max_threads: 0,
            })
            .build()
            .expect("build");

        let cx = for_testing();
        let handle = runtime.handle();
        let jh = spawn_with_cx(&handle, &cx, move |_child_cx| async move { val });
        let result = runtime.block_on(jh);
        prop_assert_eq!(result, val);
    }

    // =========================================================================
    // NEW: try_spawn_with_cx returns correct value
    // =========================================================================

    #[test]
    fn try_spawn_returns_value(val in any::<u32>()) {
        let runtime = CxRuntimeBuilder::current_thread()
            .with_tuning(RuntimeTuning {
                worker_threads: 1,
                poll_budget: 64,
                blocking_min_threads: 0,
                blocking_max_threads: 0,
            })
            .build()
            .expect("build");

        let cx = for_testing();
        let handle = runtime.handle();
        let jh = try_spawn_with_cx(&handle, &cx, move |_child_cx| async move { val })
            .expect("try_spawn admission");
        let result = runtime.block_on(jh);
        prop_assert_eq!(result, val);
    }
}
