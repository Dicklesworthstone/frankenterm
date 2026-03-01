// Requires the `subprocess-bridge` feature flag.
#![cfg(feature = "subprocess-bridge")]
//! Property-based tests for subprocess bridge types (ft-3kxe).
//!
//! Validates:
//! 1. BridgeError Display output contains expected substrings
//! 2. BridgeError Clone produces identical values
//! 3. BridgeError PartialEq/Eq reflexivity and discrimination
//! 4. SubprocessBridge builder preserves binary name
//! 5. SubprocessBridge with_timeout overrides default
//! 6. SubprocessBridge with_search_paths overrides default
//! 7. Missing binary consistently returns BinaryNotFound
//! 8. BridgeError Debug format contains variant name

use proptest::prelude::*;
use std::path::PathBuf;
use std::time::Duration;

use frankenterm_core::subprocess_bridge::{BridgeError, SubprocessBridge};

// =============================================================================
// Strategies
// =============================================================================

fn binary_name_strategy() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_-]{0,29}".prop_map(String::from)
}

fn error_message_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _.!:]{0,200}".prop_map(String::from)
}

fn duration_strategy() -> impl Strategy<Value = Duration> {
    (1u64..600_000).prop_map(Duration::from_millis)
}

fn exit_code_strategy() -> impl Strategy<Value = i32> {
    prop::num::i32::ANY
}

fn bridge_error_strategy() -> impl Strategy<Value = BridgeError> {
    prop_oneof![
        binary_name_strategy().prop_map(BridgeError::BinaryNotFound),
        duration_strategy().prop_map(BridgeError::Timeout),
        error_message_strategy().prop_map(BridgeError::ParseError),
        (exit_code_strategy(), error_message_strategy())
            .prop_map(|(code, msg)| BridgeError::ExitCode(code, msg)),
    ]
}

fn search_path_strategy() -> impl Strategy<Value = Vec<PathBuf>> {
    prop::collection::vec("[a-z/]{1,30}".prop_map(PathBuf::from), 0..5)
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn bridge_error_binary_not_found_display() {
    let err = BridgeError::BinaryNotFound("mybin".to_string());
    let display = err.to_string();
    assert!(display.contains("binary not found"));
    assert!(display.contains("mybin"));
}

#[test]
fn bridge_error_timeout_display() {
    let err = BridgeError::Timeout(Duration::from_secs(5));
    let display = err.to_string();
    assert!(display.contains("timed out"));
}

#[test]
fn bridge_error_parse_error_display() {
    let err = BridgeError::ParseError("bad json".to_string());
    let display = err.to_string();
    assert!(display.contains("parse error"));
    assert!(display.contains("bad json"));
}

#[test]
fn bridge_error_exit_code_display() {
    let err = BridgeError::ExitCode(42, "boom".to_string());
    let display = err.to_string();
    assert!(display.contains("exit code 42"));
    assert!(display.contains("boom"));
}

#[test]
fn bridge_error_clone_equality() {
    let err = BridgeError::ExitCode(1, "fail".to_string());
    let cloned = err.clone();
    assert_eq!(err, cloned);
}

#[test]
fn bridge_error_variants_not_equal() {
    let a = BridgeError::BinaryNotFound("x".to_string());
    let b = BridgeError::ParseError("x".to_string());
    assert_ne!(a, b);
}

#[test]
fn bridge_new_binary_name_preserved() {
    let b: SubprocessBridge<serde_json::Value> = SubprocessBridge::new("test-binary");
    assert_eq!(b.binary_name(), "test-binary");
}

#[test]
fn bridge_missing_binary_not_available() {
    let b: SubprocessBridge<serde_json::Value> =
        SubprocessBridge::new("proptest-nonexistent-binary-xyz");
    assert!(!b.is_available());
}

#[test]
fn bridge_missing_binary_invoke_returns_not_found() {
    let b: SubprocessBridge<serde_json::Value> =
        SubprocessBridge::new("proptest-nonexistent-binary-xyz");
    let err = b.invoke(&[]).unwrap_err();
    assert!(matches!(err, BridgeError::BinaryNotFound(_)));
}

#[test]
fn bridge_sh_is_available() {
    let b: SubprocessBridge<serde_json::Value> = SubprocessBridge::new("sh");
    assert!(b.is_available());
}

#[test]
fn bridge_error_debug_format() {
    let err = BridgeError::Timeout(Duration::from_secs(1));
    let dbg = format!("{err:?}");
    assert!(dbg.contains("Timeout"));
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // ── BridgeError Display always produces non-empty output ────────────

    #[test]
    fn bridge_error_display_nonempty(err in bridge_error_strategy()) {
        let display = err.to_string();
        prop_assert!(!display.is_empty(), "Display should produce non-empty output");
    }

    // ── BridgeError Clone produces identical values ─────────────────────

    #[test]
    fn bridge_error_clone_is_equal(err in bridge_error_strategy()) {
        let cloned = err.clone();
        prop_assert_eq!(&err, &cloned);
    }

    // ── BridgeError PartialEq reflexivity ───────────────────────────────

    #[test]
    fn bridge_error_eq_reflexive(err in bridge_error_strategy()) {
        prop_assert_eq!(&err, &err);
    }

    // ── BridgeError Display contains variant-specific substrings ────────

    #[test]
    fn binary_not_found_display_contains_name(name in binary_name_strategy()) {
        let err = BridgeError::BinaryNotFound(name.clone());
        let display = err.to_string();
        prop_assert!(
            display.contains(&name),
            "Display '{}' should contain binary name '{}'",
            display, name
        );
        prop_assert!(
            display.contains("binary not found"),
            "Display should contain 'binary not found'"
        );
    }

    #[test]
    fn timeout_display_contains_timed_out(dur in duration_strategy()) {
        let err = BridgeError::Timeout(dur);
        let display = err.to_string();
        prop_assert!(
            display.contains("timed out"),
            "Display '{}' should contain 'timed out'",
            display
        );
    }

    #[test]
    fn parse_error_display_contains_message(msg in error_message_strategy()) {
        let err = BridgeError::ParseError(msg.clone());
        let display = err.to_string();
        prop_assert!(
            display.contains("parse error"),
            "Display '{}' should contain 'parse error'",
            display
        );
        prop_assert!(
            display.contains(&msg),
            "Display should contain the error message"
        );
    }

    #[test]
    fn exit_code_display_contains_code(
        code in exit_code_strategy(),
        msg in error_message_strategy(),
    ) {
        let err = BridgeError::ExitCode(code, msg);
        let display = err.to_string();
        let code_str = code.to_string();
        prop_assert!(
            display.contains(&code_str),
            "Display '{}' should contain exit code '{}'",
            display, code_str
        );
        prop_assert!(
            display.contains("exit code"),
            "Display should contain 'exit code'"
        );
    }

    // ── BridgeError Debug contains variant name ─────────────────────────

    #[test]
    fn bridge_error_debug_contains_variant(err in bridge_error_strategy()) {
        let dbg = format!("{err:?}");
        let has_variant = dbg.contains("BinaryNotFound")
            || dbg.contains("Timeout")
            || dbg.contains("ParseError")
            || dbg.contains("ExitCode");
        prop_assert!(
            has_variant,
            "Debug '{}' should contain a variant name",
            dbg
        );
    }

    // ── BridgeError variant discrimination ──────────────────────────────

    #[test]
    fn different_variants_not_equal(
        name in binary_name_strategy(),
        dur in duration_strategy(),
    ) {
        let a = BridgeError::BinaryNotFound(name);
        let b = BridgeError::Timeout(dur);
        prop_assert_ne!(&a, &b);
    }

    #[test]
    fn same_variant_different_data_not_equal(
        name1 in "[a-z]{1,10}",
        name2 in "[a-z]{1,10}",
    ) {
        prop_assume!(name1 != name2);
        let a = BridgeError::BinaryNotFound(name1);
        let b = BridgeError::BinaryNotFound(name2);
        prop_assert_ne!(&a, &b);
    }

    // ── SubprocessBridge builder preserves binary name ───────────────────

    #[test]
    fn bridge_binary_name_preserved(name in binary_name_strategy()) {
        let b: SubprocessBridge<serde_json::Value> = SubprocessBridge::new(&name);
        prop_assert_eq!(b.binary_name(), name.as_str());
    }

    // ── SubprocessBridge with_search_paths accepts arbitrary paths ───────

    #[test]
    fn bridge_with_search_paths_accepted(
        name in binary_name_strategy(),
        paths in search_path_strategy(),
    ) {
        let b: SubprocessBridge<serde_json::Value> = SubprocessBridge::new(&name)
            .with_search_paths(paths.clone());
        // Bridge should not panic and binary name should be preserved
        prop_assert_eq!(b.binary_name(), name.as_str());
    }

    // ── SubprocessBridge with_timeout accepts arbitrary durations ────────

    #[test]
    fn bridge_with_timeout_accepted(
        name in binary_name_strategy(),
        dur in duration_strategy(),
    ) {
        let b: SubprocessBridge<serde_json::Value> = SubprocessBridge::new(&name)
            .with_timeout(dur);
        prop_assert_eq!(b.binary_name(), name.as_str());
    }

    // ── Missing binary consistently returns BinaryNotFound ──────────────

    #[test]
    fn missing_binary_returns_not_found(name in "proptest_missing_[a-z]{5,15}") {
        let b: SubprocessBridge<serde_json::Value> = SubprocessBridge::new(&name)
            .with_search_paths(Vec::<PathBuf>::new());
        let err = b.invoke(&[]).unwrap_err();
        let is_not_found = matches!(err, BridgeError::BinaryNotFound(_));
        prop_assert!(
            is_not_found,
            "Expected BinaryNotFound for '{}', got {:?}",
            name, err
        );
    }

    // ── Missing binary is_available returns false ────────────────────────

    #[test]
    fn missing_binary_not_available(name in "proptest_missing_[a-z]{5,15}") {
        let b: SubprocessBridge<serde_json::Value> = SubprocessBridge::new(&name)
            .with_search_paths(Vec::<PathBuf>::new());
        prop_assert!(
            !b.is_available(),
            "Expected is_available=false for '{}'",
            name
        );
    }

    // ── Builder chaining is order-independent ───────────────────────────

    #[test]
    fn builder_chaining_timeout_then_paths(
        name in binary_name_strategy(),
        dur in duration_strategy(),
        paths in search_path_strategy(),
    ) {
        // Both orderings should work without panicking
        let _b1: SubprocessBridge<serde_json::Value> = SubprocessBridge::new(&name)
            .with_timeout(dur)
            .with_search_paths(paths.clone());
        let _b2: SubprocessBridge<serde_json::Value> = SubprocessBridge::new(&name)
            .with_search_paths(paths)
            .with_timeout(dur);
        // If we reach here, neither panicked
    }

    // ── ExitCode error message is preserved in Display ──────────────────

    #[test]
    fn exit_code_message_preserved(
        code in exit_code_strategy(),
        msg in "[a-zA-Z0-9 ]{1,50}",
    ) {
        let err = BridgeError::ExitCode(code, msg.clone());
        let display = err.to_string();
        prop_assert!(
            display.contains(&msg),
            "Display should contain message '{}', got '{}'",
            msg, display
        );
    }
}
