//! Property-based tests for `workflows::wait_execution` public carrier types.

use frankenterm_core::workflows::{WaitConditionOptions, WaitConditionResult};
use proptest::prelude::*;
use std::time::Duration;

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
}
