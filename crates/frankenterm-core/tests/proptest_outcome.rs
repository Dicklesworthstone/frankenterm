//! Property-based tests for `frankenterm_core::outcome`.
//!
//! Covers:
//! 1. `result_to_outcome()` / `outcome_into_result()` roundtrip semantics
//! 2. cancellation helper constructors preserving kind/message invariants
//! 3. `severity_to_log_level()` mapping stability

use frankenterm_core::outcome::{
    CancelKind, Outcome, Severity, cancel_shutdown, cancel_timeout, cancel_user,
    outcome_into_result, result_to_outcome, severity_to_log_level,
};
use proptest::prelude::*;

fn arb_static_message() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("user requested stop"),
        Just("soft timeout"),
        Just("service shutdown"),
        Just("retry later"),
    ]
}

fn arb_severity() -> impl Strategy<Value = Severity> {
    prop_oneof![
        Just(Severity::Ok),
        Just(Severity::Err),
        Just(Severity::Cancelled),
        Just(Severity::Panicked),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn result_outcome_result_roundtrip(
        input in prop_oneof![
            (-10_000i64..10_000).prop_map(Ok::<i64, String>),
            "[-_ a-zA-Z0-9]{1,40}".prop_map(Err::<i64, String>),
        ],
    ) {
        let outcome = result_to_outcome(input.clone());
        let restored = outcome_into_result(
            outcome,
            |reason| format!("cancelled: {}", reason.kind),
            |panic| format!("panicked: {}", panic.message()),
        );
        prop_assert_eq!(restored, input);
    }

    #[test]
    fn cancel_helpers_preserve_kind_and_message(
        message in arb_static_message(),
    ) {
        let user = cancel_user(message);
        prop_assert_eq!(user.kind, CancelKind::User);
        prop_assert_eq!(user.message, Some(message));
        prop_assert!(user.cause.is_none());
        prop_assert!(!user.truncated);

        let timeout = cancel_timeout(message);
        prop_assert_eq!(timeout.kind, CancelKind::Timeout);
        prop_assert_eq!(timeout.message, Some(message));
        prop_assert!(timeout.cause.is_none());
        prop_assert!(!timeout.truncated);

        let shutdown = cancel_shutdown(message);
        prop_assert_eq!(shutdown.kind, CancelKind::Shutdown);
        prop_assert_eq!(shutdown.message, Some(message));
        prop_assert!(shutdown.cause.is_none());
        prop_assert!(!shutdown.truncated);
    }

    #[test]
    fn severity_to_log_level_matches_expected_mapping(
        severity in arb_severity(),
    ) {
        let expected = match severity {
            Severity::Ok => tracing::Level::DEBUG,
            Severity::Err => tracing::Level::WARN,
            Severity::Cancelled => tracing::Level::INFO,
            Severity::Panicked => tracing::Level::ERROR,
        };
        prop_assert_eq!(severity_to_log_level(severity), expected);
    }
}

#[test]
fn outcome_into_result_maps_cancelled_with_callback() {
    let cancelled = Outcome::<u8, String>::Cancelled(cancel_timeout("timed out"));
    let restored = outcome_into_result(
        cancelled,
        |reason| format!("cancelled: {}", reason.kind),
        |panic| format!("panicked: {}", panic.message()),
    );
    assert_eq!(restored, Err("cancelled: timeout".to_string()));
}
