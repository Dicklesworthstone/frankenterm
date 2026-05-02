use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use frankenterm_core::runtime_async;
use frankenterm_core::test_fixtures::lab_runtime::{
    AutoAdvanceTermination, DEFAULT_MAX_STEPS, LabConfig, LabRuntimeMultiTask, ManualTimeHarness,
    assert_ran_to_completion, lab_runtime_test_with_config, lab_runtime_test_with_seed,
};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn proptest_lab_runtime_explicit_seed_is_deterministic_for_pure_bodies(
        seed in any::<u64>(),
        increments in 0_usize..=256,
    ) {
        let counter_a = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&counter_a);
        let report_a = lab_runtime_test_with_seed(seed, move |_cx| {
            let counter = a;
            async move {
                for _ in 0..increments {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        let counter_b = Arc::new(AtomicUsize::new(0));
        let b = Arc::clone(&counter_b);
        let report_b = lab_runtime_test_with_seed(seed, move |_cx| {
            let counter = b;
            async move {
                for _ in 0..increments {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
        });

        prop_assert_eq!(counter_a.load(Ordering::SeqCst), increments);
        prop_assert_eq!(counter_b.load(Ordering::SeqCst), increments);
        assert_ran_to_completion(&report_a);
        assert_ran_to_completion(&report_b);
        prop_assert!(report_a.oracles_passed);
        prop_assert!(report_b.oracles_passed);
        prop_assert_eq!(report_a.steps, report_b.steps);
        prop_assert_eq!(report_a.now_nanos, report_b.now_nanos);
    }

    #[test]
    fn proptest_lab_runtime_custom_config_finishes_trivial_body_cleanly(
        seed in any::<u64>(),
        max_steps in 1_000_u64..=DEFAULT_MAX_STEPS,
    ) {
        let config = LabConfig::new(seed)
            .with_auto_advance()
            .worker_count(1)
            .max_steps(max_steps);
        let report = lab_runtime_test_with_config(config, |_cx| async move {});

        assert_ran_to_completion(&report);
        prop_assert_eq!(report.termination, AutoAdvanceTermination::Quiescent);
        prop_assert!(report.steps <= max_steps);
        prop_assert_eq!(report.now_nanos, 0);
        prop_assert!(report.oracles_passed);
    }

    #[test]
    fn proptest_lab_runtime_multi_task_runs_every_spawned_root(
        seed in any::<u64>(),
        task_count in 0_usize..=8,
    ) {
        let observed = Arc::new(AtomicUsize::new(0));
        let mut harness = LabRuntimeMultiTask::with_seed(seed);

        for _ in 0..task_count {
            let observed = Arc::clone(&observed);
            harness.spawn(move |_cx| async move {
                observed.fetch_add(1, Ordering::SeqCst);
            });
        }

        let report = harness.run();
        assert_ran_to_completion(&report);
        prop_assert_eq!(observed.load(Ordering::SeqCst), task_count);
        prop_assert!(report.steps <= DEFAULT_MAX_STEPS);
        prop_assert!(report.oracles_passed);
    }

    #[test]
    fn proptest_manual_time_advance_is_additive_and_reported(
        advances_ms in prop::collection::vec(0_u64..=2_000, 0..=12),
    ) {
        let mut harness = ManualTimeHarness::new();
        let mut expected_nanos = 0_u64;

        for ms in advances_ms {
            harness.advance(Duration::from_millis(ms));
            expected_nanos = expected_nanos.saturating_add(ms.saturating_mul(1_000_000));
            prop_assert_eq!(harness.now_nanos(), expected_nanos);
        }

        prop_assert!(harness.is_quiescent());
        let report = harness.into_report();
        prop_assert_eq!(report.termination, AutoAdvanceTermination::Quiescent);
        prop_assert_eq!(report.now_nanos, expected_nanos);
        prop_assert!(report.oracles_passed);
    }

    #[test]
    fn proptest_manual_time_sleep_wakes_only_after_deadline(
        delay_ms in 1_u64..=2_000,
        before_deadline_ms in 0_u64..2_000,
    ) {
        let before_deadline_ms = before_deadline_ms.min(delay_ms - 1);
        let observed = Arc::new(AtomicBool::new(false));
        let observed_clone = Arc::clone(&observed);
        let mut harness = ManualTimeHarness::new();

        harness.spawn(move |cx| {
            let observed = observed_clone;
            async move {
                runtime_async::sleep_with_cx(&cx, Duration::from_millis(delay_ms))
                    .await
                    .expect("manual LabRuntime-backed sleep should complete after deadline");
                observed.store(true, Ordering::SeqCst);
            }
        });

        let initial_steps = harness.run_until_idle();
        prop_assert!(initial_steps > 0);
        prop_assert!(!observed.load(Ordering::SeqCst));

        harness.advance(Duration::from_millis(before_deadline_ms));
        harness.run_until_idle();
        prop_assert!(!observed.load(Ordering::SeqCst));

        harness.advance(Duration::from_millis(delay_ms - before_deadline_ms));
        harness.run_until_idle();
        prop_assert!(observed.load(Ordering::SeqCst));

        let report = harness.into_report();
        assert_ran_to_completion(&report);
        prop_assert!(report.now_nanos >= delay_ms.saturating_mul(1_000_000));
    }
}
