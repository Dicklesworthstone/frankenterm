//! Deterministic `LabRuntime` coverage for the scrollback-restoration gate,
//! plus an unwind-safety regression for the shared ordinary-runtime fixture.
//!
//! Feature-gated behind `asupersync-runtime`.
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use common::fixtures::RuntimeFixture;
use frankenterm_core::cx::{Budget, Cx};
use frankenterm_core::error::RuntimeOperationSource;
use frankenterm_core::outcome::CancelKind;
use frankenterm_core::restore_scrollback::{InjectionGuard, ScrollbackData, ScrollbackInjector};
use frankenterm_core::session_topology::MAX_TOPOLOGY_PANES;
use frankenterm_core::test_fixtures::lab_runtime::{assert_ran_to_completion, lab_runtime_test};
use frankenterm_core::wezterm::{MockWezterm, WeztermInterface};

fn run_lab<F, Fut>(test: F)
where
    F: FnOnce(frankenterm_core::cx::Cx) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let report = lab_runtime_test(test);
    assert!(
        report.oracles_passed,
        "LabRuntime invariant oracles must pass: {report:?}"
    );
    assert_ran_to_completion(&report);
}

fn make_injector() -> ScrollbackInjector {
    ScrollbackInjector::new()
}

fn mock_scrollback(lines: Vec<&str>) -> ScrollbackData {
    ScrollbackData::from_terminal_lines(lines.into_iter().map(String::from).collect())
}

// ===========================================================================
// 1. inject_single_pane
// ===========================================================================

#[test]
fn inject_single_pane_fails_closed() {
    run_lab(|cx| async move {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector();

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["line1", "line2", "line3"]));

        injector
            .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
            .await
            .expect_err("mapped replay must report the unsupported safe-output channel");

        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert_eq!(text, "");
    });
}

// ===========================================================================
// 2. inject_multiple_panes
// ===========================================================================

#[test]
fn inject_multiple_panes() {
    run_lab(|cx| async move {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        mock.add_default_pane(11).await;
        let injector = make_injector();

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);
        pane_id_map.insert(2_u64, 11_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["pane1-output"]));
        scrollbacks.insert(2, mock_scrollback(vec!["pane2-output"]));

        injector
            .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
            .await
            .expect_err("mapped replay must report the unsupported safe-output channel");
    });
}

// ===========================================================================
// 3. inject_skips_unmapped_panes
// ===========================================================================

#[test]
fn inject_skips_unmapped_panes() {
    run_lab(|cx| async move {
        let injector = make_injector();

        let pane_id_map = HashMap::new();

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["data"]));

        let report = injector
            .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
            .await
            .unwrap();

        assert_eq!(report.success_count(), 0);
        assert_eq!(report.skipped_count(), 1);
        assert_eq!(report.skipped_sample(), &[1]);
    });
}

// ===========================================================================
// 4. inject_empty_scrollback
// ===========================================================================

#[test]
fn inject_empty_scrollback() {
    run_lab(|cx| async move {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector();

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, ScrollbackData::from_terminal_lines(vec![]));

        injector
            .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
            .await
            .expect_err("mapped empty replay still requires a safe output channel");
    });
}

// ===========================================================================
// 5. large mapped scrollback fails before replay allocation or output
// ===========================================================================

#[test]
fn inject_large_scrollback_does_not_write() {
    run_lab(|cx| async move {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = ScrollbackInjector::new();

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let lines: Vec<String> = (0..100).map(|i| format!("line-{i}")).collect();
        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, ScrollbackData::from_terminal_lines(lines));

        injector
            .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
            .await
            .expect_err("large mapped replay must fail before allocating replay content");

        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert_eq!(text, "");
    });
}

// ===========================================================================
// 6. inject_no_scrollbacks
// ===========================================================================

#[test]
fn inject_no_scrollbacks() {
    run_lab(|cx| async move {
        let injector = make_injector();

        let pane_id_map = HashMap::new();
        let scrollbacks = HashMap::new();

        let report = injector
            .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
            .await
            .unwrap();

        assert_eq!(report.success_count(), 0);
        assert_eq!(report.failure_count(), 0);
        assert_eq!(report.skipped_count(), 0);
        assert!(report.skipped_sample().is_empty());
    });
}

// ===========================================================================
// 7. injection_guard_active_during_inject
// ===========================================================================

#[test]
fn unsupported_injection_does_not_change_suppression_state() {
    run_lab(|cx| async move {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector();
        let suppressed = injector.suppressed_panes().clone();

        assert!(!InjectionGuard::is_suppressed(&suppressed, 10));

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["test"]));

        injector
            .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
            .await
            .expect_err("mapped replay must fail closed");
        assert!(!InjectionGuard::is_suppressed(&suppressed, 10));
    });
}

#[test]
fn inject_preflight_preserves_capability_failure_classes() {
    run_lab(|_lab_cx| async move {
        let injector = make_injector();
        let pane_id_map = HashMap::new();
        let scrollbacks = HashMap::new();

        let cancelled = Cx::for_testing();
        cancelled.cancel_with(CancelKind::User, Some("scrollback pre-cancel proof"));
        let cancelled_error = injector
            .inject_with_cx(&cancelled, &pane_id_map, &scrollbacks)
            .await
            .expect_err("pre-cancelled injection must fail before scanning");
        assert!(matches!(
            cancelled_error,
            frankenterm_core::Error::RuntimeOperation {
                operation: "restore_scrollback.inject.preflight",
                source: RuntimeOperationSource::Cancelled(_),
            }
        ));

        for (budget, expected) in [
            (
                Budget::new().with_deadline(Default::default()),
                RuntimeOperationSource::DeadlineExceeded,
            ),
            (
                Budget::new().with_poll_quota(0),
                RuntimeOperationSource::PollQuotaExhausted,
            ),
            (
                Budget::new().with_cost_quota(0),
                RuntimeOperationSource::CostBudgetExhausted,
            ),
        ] {
            let cx = Cx::for_testing_with_budget(budget);
            let error = injector
                .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
                .await
                .expect_err("exhausted capability must fail before scanning");
            assert!(matches!(
                error,
                frankenterm_core::Error::RuntimeOperation {
                    operation: "restore_scrollback.inject.preflight",
                    source,
                } if source == expected
            ));
        }
    });
}

#[test]
fn injection_scan_checkpoints_at_the_bounded_interval() {
    run_lab(|_lab_cx| async move {
        let injector = make_injector();
        let pane_id_map = HashMap::new();
        let below_interval = (0_u64..256)
            .map(|pane_id| (pane_id, mock_scrollback(vec!["unmapped"])))
            .collect::<HashMap<_, _>>();
        let below_cx = Cx::for_testing_with_budget(Budget::new().with_poll_quota(2));
        let report = injector
            .inject_with_cx(&below_cx, &pane_id_map, &below_interval)
            .await
            .expect("preflight plus final checkpoint must admit 256 unmapped panes");
        assert_eq!(report.skipped_count(), 256);

        let crosses_interval = (0_u64..257)
            .map(|pane_id| (pane_id, mock_scrollback(vec!["unmapped"])))
            .collect::<HashMap<_, _>>();
        let crossing_cx = Cx::for_testing_with_budget(Budget::new().with_poll_quota(2));
        let error = injector
            .inject_with_cx(&crossing_cx, &pane_id_map, &crosses_interval)
            .await
            .expect_err("the 257th scan entry must observe the bounded checkpoint");
        assert!(matches!(
            error,
            frankenterm_core::Error::RuntimeOperation {
                operation: "restore_scrollback.inject.preflight",
                source: RuntimeOperationSource::PollQuotaExhausted,
            }
        ));
    });
}

#[test]
fn injection_preflight_rejects_each_over_limit_map() {
    run_lab(|_lab_cx| async move {
        let injector = make_injector();
        let over_limit = MAX_TOPOLOGY_PANES.saturating_add(1);
        let pane_id_map = (0..over_limit)
            .map(|pane_id| {
                let pane_id = u64::try_from(pane_id).expect("test pane id fits u64");
                (pane_id, pane_id.saturating_add(10_000))
            })
            .collect::<HashMap<_, _>>();
        let empty_scrollback = HashMap::new();
        let error = injector
            .inject_with_cx(&Cx::for_testing(), &pane_id_map, &empty_scrollback)
            .await
            .expect_err("over-limit pane map must fail before scanning");
        assert!(matches!(
            error,
            frankenterm_core::Error::RuntimeOperation {
                operation: "restore_scrollback.inject.resource_limit",
                source: RuntimeOperationSource::Backend(_),
            }
        ));

        let scrollbacks = (0..over_limit)
            .map(|pane_id| {
                (
                    u64::try_from(pane_id).expect("test pane id fits u64"),
                    mock_scrollback(Vec::new()),
                )
            })
            .collect::<HashMap<_, _>>();
        let error = injector
            .inject_with_cx(&Cx::for_testing(), &HashMap::new(), &scrollbacks)
            .await
            .expect_err("over-limit scrollback map must fail before scanning");
        assert!(matches!(
            error,
            frankenterm_core::Error::RuntimeOperation {
                operation: "restore_scrollback.inject.resource_limit",
                source: RuntimeOperationSource::Backend(_),
            }
        ));
    });
}

#[test]
fn mapped_intersection_wins_after_the_entry_checkpoint() {
    run_lab(|_lab_cx| async move {
        let injector = make_injector();
        let pane_id_map = (0..MAX_TOPOLOGY_PANES)
            .map(|pane_id| {
                let pane_id = u64::try_from(pane_id).expect("test pane id fits u64");
                (pane_id, pane_id.saturating_add(10_000))
            })
            .collect::<HashMap<_, _>>();
        let scrollbacks = HashMap::from([(
            u64::try_from(MAX_TOPOLOGY_PANES - 1).expect("test pane id fits u64"),
            mock_scrollback(Vec::new()),
        )]);
        let cx = Cx::for_testing_with_budget(Budget::new().with_poll_quota(1));
        let error = injector
            .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
            .await
            .expect_err("mapped data must identify the unsupported channel first");
        assert!(matches!(
            error,
            frankenterm_core::Error::RuntimeOperation {
                operation: "restore_scrollback.inject.no_safe_output_channel",
                source: RuntimeOperationSource::Backend(_),
            }
        ));
    });
}

#[test]
fn runtime_fixture_clears_installed_handle_when_test_future_panics() {
    frankenterm_core::runtime_async::clear_runtime_handle();
    let runtime = RuntimeFixture::current_thread();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async {
            panic!("runtime-fixture-unwind-regression");
        });
    }));

    assert!(panic.is_err());
    assert!(
        frankenterm_core::runtime_async::current_runtime_handle().is_none(),
        "RuntimeFixture must restore TLS state while unwinding"
    );
}
