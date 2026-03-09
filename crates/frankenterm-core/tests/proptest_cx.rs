#![cfg(feature = "asupersync-runtime")]
//! Property-based tests for the `cx` module (asupersync Cx capability-context adapters).
//!
//! Covers: RuntimePreset, RuntimeTuning, CxRuntimeBuilder, with_cx, with_cx_async,
//! for_testing, spawn_with_cx, try_spawn_with_cx, spawn_bounded_with_cx, spawn_with_timeout.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::cx::{
    Cx, CxRuntimeBuilder, RuntimePreset, RuntimeTuning, for_testing, spawn_bounded_with_cx,
    spawn_with_cx, spawn_with_timeout, try_spawn_with_cx, with_cx, with_cx_async,
};

// ── Strategies ──────────────────────────────────────────────────────────

fn arb_preset() -> impl Strategy<Value = RuntimePreset> {
    prop_oneof![
        Just(RuntimePreset::CurrentThread),
        Just(RuntimePreset::MultiThread),
    ]
}

fn arb_tuning() -> impl Strategy<Value = RuntimeTuning> {
    (1..=8usize, 1..=256u32, 1..=4usize, 4..=16usize).prop_map(|(workers, budget, bmin, bmax)| {
        let actual_max = bmin.max(bmax);
        RuntimeTuning {
            worker_threads: workers,
            poll_budget: budget,
            blocking_min_threads: bmin,
            blocking_max_threads: actual_max,
        }
    })
}

fn make_runtime() -> frankenterm_core::cx::Runtime {
    CxRuntimeBuilder::current_thread()
        .build()
        .expect("test runtime")
}

// ── RuntimePreset tests ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 1
    #[test]
    fn preset_eq_reflexive(p in arb_preset()) {
        prop_assert_eq!(p, p);
    }

    // 2
    #[test]
    fn preset_eq_symmetric(a in arb_preset(), b in arb_preset()) {
        prop_assert_eq!(a == b, b == a);
    }

    // 3
    #[test]
    fn preset_clone_preserves_value(p in arb_preset()) {
        let cloned = p;
        prop_assert_eq!(p, cloned);
    }

    // 4
    #[test]
    fn preset_copy_preserves_value(p in arb_preset()) {
        let copied = p;
        let again = p;
        prop_assert_eq!(copied, again);
    }

    // 5
    #[test]
    fn preset_debug_is_nonempty(p in arb_preset()) {
        let dbg = format!("{:?}", p);
        prop_assert!(!dbg.is_empty());
    }

    // 6
    #[test]
    fn preset_debug_contains_variant_name(p in arb_preset()) {
        let dbg = format!("{:?}", p);
        match p {
            RuntimePreset::CurrentThread => prop_assert!(dbg.contains("CurrentThread")),
            RuntimePreset::MultiThread => prop_assert!(dbg.contains("MultiThread")),
        }
    }
}

// ── RuntimeTuning tests ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    // 7
    #[test]
    fn tuning_eq_reflexive(t in arb_tuning()) {
        prop_assert_eq!(t, t);
    }

    // 8
    #[test]
    fn tuning_eq_symmetric(a in arb_tuning(), b in arb_tuning()) {
        prop_assert_eq!(a == b, b == a);
    }

    // 9
    #[test]
    fn tuning_clone_preserves_value(t in arb_tuning()) {
        let cloned = t;
        prop_assert_eq!(t, cloned);
    }

    // 10
    #[test]
    fn tuning_copy_preserves_value(t in arb_tuning()) {
        let copied = t;
        let again = t;
        prop_assert_eq!(copied, again);
    }

    // 11
    #[test]
    fn tuning_debug_contains_field_names(t in arb_tuning()) {
        let dbg = format!("{:?}", t);
        prop_assert!(dbg.contains("worker_threads"));
        prop_assert!(dbg.contains("poll_budget"));
        prop_assert!(dbg.contains("blocking_min_threads"));
        prop_assert!(dbg.contains("blocking_max_threads"));
    }

    // 12
    #[test]
    fn tuning_max_gte_min(t in arb_tuning()) {
        prop_assert!(
            t.blocking_max_threads >= t.blocking_min_threads,
            "max {} < min {}",
            t.blocking_max_threads,
            t.blocking_min_threads
        );
    }

    // 13
    #[test]
    fn tuning_worker_threads_positive(t in arb_tuning()) {
        prop_assert!(t.worker_threads > 0);
    }

    // 14
    #[test]
    fn tuning_poll_budget_positive(t in arb_tuning()) {
        prop_assert!(t.poll_budget > 0);
    }

    // 15
    #[test]
    fn tuning_ne_when_workers_differ(
        base in arb_tuning(),
        delta in 1..=4usize,
    ) {
        let mut modified = base;
        modified.worker_threads = base.worker_threads + delta;
        prop_assert_ne!(base, modified);
    }

    // 16
    #[test]
    fn tuning_ne_when_budget_differs(
        base in arb_tuning(),
        delta in 1..=100u32,
    ) {
        let mut modified = base;
        modified.poll_budget = base.poll_budget + delta;
        prop_assert_ne!(base, modified);
    }
}

// ── RuntimeTuning default ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 17
    #[test]
    fn tuning_default_is_stable(_seed in 0..100u32) {
        let d1 = RuntimeTuning::default();
        let d2 = RuntimeTuning::default();
        prop_assert_eq!(d1, d2);
    }

    // 18
    #[test]
    fn tuning_default_has_positive_fields(_seed in 0..100u32) {
        let t = RuntimeTuning::default();
        prop_assert!(t.worker_threads > 0);
        prop_assert!(t.poll_budget > 0);
        prop_assert!(t.blocking_max_threads >= t.blocking_min_threads);
    }
}

// ── CxRuntimeBuilder tests ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 19
    #[test]
    fn builder_from_preset_debug_nonempty(p in arb_preset()) {
        let builder = CxRuntimeBuilder::from_preset(p);
        let dbg = format!("{:?}", builder);
        prop_assert!(dbg.contains("CxRuntimeBuilder"));
    }

    // 20
    #[test]
    fn builder_current_thread_builds(workers in 1..=4usize) {
        let rt = CxRuntimeBuilder::current_thread()
            .worker_threads(workers)
            .build();
        prop_assert!(rt.is_ok(), "current_thread build failed");
    }

    // 21
    #[test]
    fn builder_with_poll_budget_builds(budget in 1..=256u32) {
        let rt = CxRuntimeBuilder::current_thread()
            .poll_budget(budget)
            .build();
        prop_assert!(rt.is_ok(), "poll_budget build failed");
    }

    // 22
    #[test]
    fn builder_with_tuning_builds(t in arb_tuning()) {
        let rt = CxRuntimeBuilder::current_thread()
            .with_tuning(t)
            .build();
        prop_assert!(rt.is_ok(), "with_tuning build failed");
    }

    // 23
    #[test]
    fn builder_blocking_threads_builds(
        min_t in 1..=4usize,
        max_t in 4..=16usize,
    ) {
        let actual_max = min_t.max(max_t);
        let rt = CxRuntimeBuilder::current_thread()
            .blocking_threads(min_t, actual_max)
            .build();
        prop_assert!(rt.is_ok(), "blocking_threads build failed");
    }

    // 24
    #[test]
    fn builder_chain_all_methods_builds(
        workers in 1..=4usize,
        budget in 1..=128u32,
        bmin in 1..=2usize,
        bmax in 2..=8usize,
    ) {
        let rt = CxRuntimeBuilder::current_thread()
            .worker_threads(workers)
            .poll_budget(budget)
            .blocking_threads(bmin, bmin.max(bmax))
            .build();
        prop_assert!(rt.is_ok(), "chained build failed");
    }
}

// ── with_cx tests ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    // 25
    #[test]
    fn with_cx_preserves_int(val in any::<i64>()) {
        let cx = for_testing();
        let result = with_cx(&cx, |_| val);
        prop_assert_eq!(result, val);
    }

    // 26
    #[test]
    fn with_cx_preserves_string(val in "[a-zA-Z0-9]{0,50}") {
        let cx = for_testing();
        let result = with_cx(&cx, |_| val.clone());
        prop_assert_eq!(result, val);
    }

    // 27
    #[test]
    fn with_cx_preserves_vec(vals in prop::collection::vec(any::<u32>(), 0..20)) {
        let cx = for_testing();
        let result = with_cx(&cx, |_| vals.clone());
        prop_assert_eq!(result, vals);
    }

    // 28
    #[test]
    fn with_cx_nested_depth(depth in 1..=10usize, val in any::<u32>()) {
        let cx = for_testing();
        fn nest(cx: &Cx, depth: usize, val: u32) -> u32 {
            if depth == 0 {
                val
            } else {
                with_cx(cx, |inner| nest(inner, depth - 1, val))
            }
        }
        let result = nest(&cx, depth, val);
        prop_assert_eq!(result, val);
    }

    // 29
    #[test]
    fn with_cx_maps_value(val in 0..1000u32) {
        let cx = for_testing();
        let result = with_cx(&cx, |_| val * 2);
        prop_assert_eq!(result, val * 2);
    }
}

// ── for_testing tests ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 30
    #[test]
    fn for_testing_always_valid(_seed in 0..1000u32) {
        let cx = for_testing();
        prop_assert!(cx.checkpoint().is_ok());
    }

    // 31
    #[test]
    fn for_testing_multiple_checkpoints(count in 1..=20usize) {
        let cx = for_testing();
        for _ in 0..count {
            prop_assert!(cx.checkpoint().is_ok());
        }
    }
}

// ── spawn_with_cx tests ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 32
    #[test]
    fn spawn_preserves_value(val in any::<i32>()) {
        let rt = make_runtime();
        let cx = for_testing();
        let handle = rt.handle();
        let result = rt.block_on(async {
            spawn_with_cx(&handle, &cx, move |_cx: Cx| async move { val }).await
        });
        prop_assert_eq!(result, val);
    }

    // 33
    #[test]
    fn try_spawn_preserves_value(val in any::<u64>()) {
        let rt = make_runtime();
        let cx = for_testing();
        let handle = rt.handle();
        let result = rt.block_on(async {
            let jh = try_spawn_with_cx(&handle, &cx, move |_cx: Cx| async move { val })
                .expect("try_spawn should succeed");
            jh.await
        });
        prop_assert_eq!(result, val);
    }

    // 34
    #[test]
    fn spawn_multiple_preserves_all(vals in prop::collection::vec(0..1000u32, 1..=8)) {
        let rt = make_runtime();
        let cx = for_testing();
        let handle = rt.handle();
        let expected = vals.clone();
        let results = rt.block_on(async {
            let mut joins = Vec::new();
            for v in vals {
                joins.push(spawn_with_cx(&handle, &cx, move |_cx: Cx| async move { v }));
            }
            let mut out = Vec::new();
            for jh in joins {
                out.push(jh.await);
            }
            out
        });
        prop_assert_eq!(results, expected);
    }

    // 35
    #[test]
    fn spawn_with_cx_child_can_checkpoint(_seed in 0..100u32) {
        let rt = make_runtime();
        let cx = for_testing();
        let handle = rt.handle();
        let ok = rt.block_on(async {
            spawn_with_cx(&handle, &cx, |child_cx: Cx| async move {
                child_cx.checkpoint().is_ok()
            })
            .await
        });
        prop_assert!(ok);
    }
}

// ── spawn_bounded_with_cx tests ─────────────────────────────────────────

type CxTask<T> = Box<dyn FnOnce(Cx) -> Pin<Box<dyn Future<Output = T> + Send>> + Send>;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 36
    #[test]
    fn spawn_bounded_preserves_count(n in 0..=8usize, concurrency in 1..=4usize) {
        let rt = make_runtime();
        let cx = for_testing();
        let handle = rt.handle();
        let tasks: Vec<CxTask<u32>> =
            (0..n as u32)
                .map(|i| {
                    let closure: CxTask<u32> =
                        Box::new(move |_cx| Box::pin(async move { i }));
                    closure
                })
                .collect();
        let results = rt.block_on(async {
            spawn_bounded_with_cx(&handle, &cx, concurrency, tasks).await
        });
        prop_assert_eq!(results.len(), n);
    }

    // 37
    #[test]
    fn spawn_bounded_preserves_order(n in 1..=6usize) {
        let rt = make_runtime();
        let cx = for_testing();
        let handle = rt.handle();
        let expected: Vec<u32> = (0..n as u32).collect();
        let tasks: Vec<CxTask<u32>> =
            (0..n as u32)
                .map(|i| {
                    let closure: CxTask<u32> =
                        Box::new(move |_cx| Box::pin(async move { i }));
                    closure
                })
                .collect();
        let results = rt.block_on(async {
            spawn_bounded_with_cx(&handle, &cx, 2, tasks).await
        });
        prop_assert_eq!(results, expected);
    }

    // 38
    #[test]
    fn spawn_bounded_empty_returns_empty(concurrency in 1..=4usize) {
        let rt = make_runtime();
        let cx = for_testing();
        let handle = rt.handle();
        let tasks: Vec<CxTask<()>> = Vec::new();
        let results =
            rt.block_on(async { spawn_bounded_with_cx(&handle, &cx, concurrency, tasks).await });
        prop_assert!(results.is_empty());
    }
}

// ── spawn_with_timeout tests ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 39
    #[test]
    fn timeout_fast_task_succeeds(val in any::<i32>()) {
        let rt = make_runtime();
        let cx = for_testing();
        let handle = rt.handle();
        let result = rt.block_on(async {
            spawn_with_timeout(&handle, &cx, Duration::from_secs(5), move |_cx| async move {
                val
            })
            .await
        });
        prop_assert!(result.is_ok());
        prop_assert_eq!(result.unwrap(), val);
    }

    // 40
    #[test]
    fn timeout_preserves_return_type(s in "[a-z]{1,10}") {
        let rt = make_runtime();
        let cx = for_testing();
        let handle = rt.handle();
        let expected = s.clone();
        let result = rt.block_on(async {
            spawn_with_timeout(&handle, &cx, Duration::from_secs(5), move |_cx| async move {
                s
            })
            .await
        });
        prop_assert!(result.is_ok());
        prop_assert_eq!(result.unwrap(), expected);
    }
}

// ── with_cx_async tests ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 41
    #[test]
    fn with_cx_async_preserves_value(val in any::<u64>()) {
        let rt = make_runtime();
        let cx = for_testing();
        let result = rt.block_on(async {
            with_cx_async(&cx, |_| async move { val }).await
        });
        prop_assert_eq!(result, val);
    }

    // 42
    #[test]
    fn with_cx_async_maps_value(val in 0..10000u32) {
        let rt = make_runtime();
        let cx = for_testing();
        let result = rt.block_on(async {
            with_cx_async(&cx, |_| async move { val + 1 }).await
        });
        prop_assert_eq!(result, val + 1);
    }
}

// ── Builder block_on integration tests ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 43
    #[test]
    fn runtime_block_on_returns_value(val in any::<i64>()) {
        let rt = make_runtime();
        let result = rt.block_on(async { val });
        prop_assert_eq!(result, val);
    }

    // 44
    #[test]
    fn runtime_with_tuning_block_on(t in arb_tuning(), val in any::<u32>()) {
        let rt = CxRuntimeBuilder::current_thread()
            .with_tuning(t)
            .build()
            .expect("tuned runtime");
        let result = rt.block_on(async { val });
        prop_assert_eq!(result, val);
    }

    // 45
    #[test]
    fn runtime_handle_spawn_returns_value(val in any::<u32>()) {
        let rt = make_runtime();
        let handle = rt.handle();
        let result = rt.block_on(async {
            handle.spawn(async move { val }).await
        });
        prop_assert_eq!(result, val);
    }
}
