//! Property-based tests for `workflows::wait_execution` public carrier types
//! and cancellation contracts.

use frankenterm_core::patterns::PatternEngine;
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::wezterm::PaneTextSource;
use frankenterm_core::workflows::{
    ExternalSignalRegistry, TextMatch, WaitCondition, WaitConditionExecutor, WaitConditionOptions,
    WaitConditionResult,
};
use proptest::prelude::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
enum CancellableWaitKind {
    Sleep,
    External,
    Pattern,
    PaneIdle,
    StableTail,
    TextMatch,
}

fn arb_cancellable_wait_kind() -> impl Strategy<Value = CancellableWaitKind> {
    prop_oneof![
        Just(CancellableWaitKind::Sleep),
        Just(CancellableWaitKind::External),
        Just(CancellableWaitKind::Pattern),
        Just(CancellableWaitKind::PaneIdle),
        Just(CancellableWaitKind::StableTail),
        Just(CancellableWaitKind::TextMatch),
    ]
}

struct NeverMatchingPaneSource {
    calls: AtomicUsize,
}

impl NeverMatchingPaneSource {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl PaneTextSource for NeverMatchingPaneSource {
    type Fut<'a> = Pin<Box<dyn Future<Output = frankenterm_core::Result<String>> + Send + 'a>>;

    fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok("command still running\nno target text here".to_string()) })
    }
}

fn test_runtime() -> frankenterm_core::runtime_async::Runtime {
    RuntimeBuilder::current_thread()
        .build()
        .expect("test runtime")
}

fn cancellation_options(poll_initial_ms: u64, poll_max_ms: u64) -> WaitConditionOptions {
    WaitConditionOptions {
        tail_lines: 20,
        poll_initial: Duration::from_millis(poll_initial_ms),
        poll_max: Duration::from_millis(poll_max_ms),
        max_polls: 10_000,
        allow_idle_heuristics: true,
    }
}

fn condition_for(kind: CancellableWaitKind) -> WaitCondition {
    match kind {
        CancellableWaitKind::Sleep => WaitCondition::sleep(60_000),
        CancellableWaitKind::External => WaitCondition::external("property.never_fired"),
        CancellableWaitKind::Pattern => WaitCondition::pattern("property.never_matches"),
        CancellableWaitKind::PaneIdle => WaitCondition::pane_idle(60_000),
        CancellableWaitKind::StableTail => WaitCondition::stable_tail(60_000),
        CancellableWaitKind::TextMatch => {
            WaitCondition::text_match(TextMatch::substring("property text that never appears"))
        }
    }
}

fn assert_cancelled_error(err: &frankenterm_core::Error, kind: CancellableWaitKind) {
    let frankenterm_core::Error::Workflow(frankenterm_core::error::WorkflowError::Aborted(message)) =
        err
    else {
        panic!("{kind:?}: expected workflow-aborted cancellation, got {err:?}");
    };
    assert!(
        message.contains("cancelled"),
        "{kind:?}: cancellation error should explain cancellation, got {message:?}"
    );
}

fn arb_wait_result() -> impl Strategy<Value = WaitConditionResult> {
    prop_oneof![
        (
            0u64..=600_000,
            0usize..=10_000,
            prop::option::of("[A-Za-z0-9 _.,:-]{0,40}"),
        )
            .prop_map(
                |(elapsed_ms, polls, context)| WaitConditionResult::Satisfied {
                    elapsed_ms,
                    polls,
                    context,
                }
            ),
        (
            0u64..=600_000,
            0usize..=10_000,
            prop::option::of("[A-Za-z0-9 _.,:-]{0,40}"),
        )
            .prop_map(
                |(elapsed_ms, polls, last_observed)| WaitConditionResult::TimedOut {
                    elapsed_ms,
                    polls,
                    last_observed,
                }
            ),
        "[A-Za-z0-9 _.,:-]{1,60}".prop_map(|reason| WaitConditionResult::Unsupported { reason }),
    ]
}

fn arb_wait_options() -> impl Strategy<Value = WaitConditionOptions> {
    (
        1usize..=10_000,
        1u64..=10_000,
        1u64..=10_000,
        1usize..=100_000,
        any::<bool>(),
    )
        .prop_filter(
            "poll_initial must be <= poll_max",
            |(_, initial_ms, max_ms, _, _)| initial_ms <= max_ms,
        )
        .prop_map(
            |(tail_lines, initial_ms, max_ms, max_polls, allow_idle_heuristics)| {
                WaitConditionOptions {
                    tail_lines,
                    poll_initial: Duration::from_millis(initial_ms),
                    poll_max: Duration::from_millis(max_ms),
                    max_polls,
                    allow_idle_heuristics,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn wait_condition_result_predicates_match_variant(result in arb_wait_result()) {
        match &result {
            WaitConditionResult::Satisfied { elapsed_ms, .. } => {
                prop_assert!(result.is_satisfied());
                prop_assert!(!result.is_timed_out());
                prop_assert_eq!(result.elapsed_ms(), Some(*elapsed_ms));
            }
            WaitConditionResult::TimedOut { elapsed_ms, .. } => {
                prop_assert!(!result.is_satisfied());
                prop_assert!(result.is_timed_out());
                prop_assert_eq!(result.elapsed_ms(), Some(*elapsed_ms));
            }
            WaitConditionResult::Unsupported { .. } => {
                prop_assert!(!result.is_satisfied());
                prop_assert!(!result.is_timed_out());
                prop_assert_eq!(result.elapsed_ms(), None);
            }
        }
    }

    #[test]
    fn wait_condition_options_clone_preserves_all_fields(options in arb_wait_options()) {
        let cloned = options.clone();

        prop_assert_eq!(cloned.tail_lines, options.tail_lines);
        prop_assert_eq!(cloned.poll_initial, options.poll_initial);
        prop_assert_eq!(cloned.poll_max, options.poll_max);
        prop_assert_eq!(cloned.max_polls, options.max_polls);
        prop_assert_eq!(cloned.allow_idle_heuristics, options.allow_idle_heuristics);
    }

    #[test]
    fn wait_condition_options_debug_mentions_core_fields(options in arb_wait_options()) {
        let debug = format!("{options:?}");

        prop_assert!(debug.contains("WaitConditionOptions"));
        prop_assert!(debug.contains(&options.tail_lines.to_string()));
        prop_assert!(debug.contains(&options.max_polls.to_string()));
        prop_assert!(debug.contains(&options.allow_idle_heuristics.to_string()));
    }

    #[test]
    fn execute_with_cx_precancel_short_circuits_without_polling(kind in arb_cancellable_wait_kind()) {
        let rt = test_runtime();
        let source = NeverMatchingPaneSource::new();
        let engine = PatternEngine::new();
        let registry = ExternalSignalRegistry::new();
        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_external_signals(&registry)
            .with_options(cancellation_options(1, 5));
        let condition = condition_for(kind);
        let cx = frankenterm_core::cx::for_testing();
        cx.cancel_with(
            frankenterm_core::outcome::CancelKind::User,
            Some("wait execution pre-cancel property"),
        );

        let err = rt
            .block_on(executor.execute_with_cx(&cx, &condition, 42, Duration::from_secs(60)))
            .expect_err("pre-cancelled cx must abort wait execution");

        assert_cancelled_error(&err, kind);
        prop_assert_eq!(
            source.calls(),
            0,
            "{:?}: pre-cancelled wait must not poll pane text",
            kind
        );
    }

    #[test]
    fn execute_with_cx_midflight_cancel_aborts_before_timeout(
        kind in arb_cancellable_wait_kind(),
        cancel_after_ms in 1u64..25,
        poll_initial_ms in 1u64..8,
        poll_extra_ms in 0u64..16,
    ) {
        let rt = test_runtime();
        let source = NeverMatchingPaneSource::new();
        let engine = PatternEngine::new();
        let registry = ExternalSignalRegistry::new();
        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_external_signals(&registry)
            .with_options(cancellation_options(
                poll_initial_ms,
                poll_initial_ms + poll_extra_ms,
            ));
        let condition = condition_for(kind);
        let cx = frankenterm_core::cx::for_testing();
        let cancel_cx = cx.clone();
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(cancel_after_ms));
            cancel_cx.cancel_with(
                frankenterm_core::outcome::CancelKind::User,
                Some("wait execution mid-flight property"),
            );
        });

        let started_at = Instant::now();
        let err = rt
            .block_on(executor.execute_with_cx(&cx, &condition, 42, Duration::from_secs(60)))
            .expect_err("mid-flight cancelled cx must abort wait execution");
        cancel_thread.join().expect("cancel thread should finish");

        assert_cancelled_error(&err, kind);
        prop_assert!(
            started_at.elapsed() < Duration::from_secs(1),
            "{:?}: cancellation should abort promptly, elapsed {:?}",
            kind,
            started_at.elapsed()
        );
    }
}
