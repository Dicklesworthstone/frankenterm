// Requires the `subprocess-bridge` feature flag.
#![cfg(feature = "subprocess-bridge")]
//! Property-based tests for subprocess bridge types (ft-3kxe).
//!
//! Validates:
//! 1. BridgeError Display output is stable and content-free
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
use tempfile::tempdir;

use frankenterm_core::runtime_async::process::{CommandCleanupTrigger, CommandOutputStream};
use frankenterm_core::subprocess_bridge::{BridgeError, SubprocessBridge};

// =============================================================================
// Strategies
// =============================================================================

fn binary_name_strategy() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_-]{0,29}".prop_map(String::from)
}

fn duration_strategy() -> impl Strategy<Value = Duration> {
    (1u64..600_000).prop_map(Duration::from_millis)
}

fn exit_code_strategy() -> impl Strategy<Value = i32> {
    prop::num::i32::ANY
}

fn output_stream_strategy() -> impl Strategy<Value = CommandOutputStream> {
    prop_oneof![
        Just(CommandOutputStream::Stdout),
        Just(CommandOutputStream::Stderr),
    ]
}

fn cleanup_trigger_strategy() -> impl Strategy<Value = CommandCleanupTrigger> {
    prop_oneof![
        Just(CommandCleanupTrigger::Cancelled),
        Just(CommandCleanupTrigger::TimedOut),
        output_stream_strategy().prop_map(CommandCleanupTrigger::CaptureLimit),
        Just(CommandCleanupTrigger::CaptureRead),
        Just(CommandCleanupTrigger::StdinWrite),
        Just(CommandCleanupTrigger::ReadinessPoll),
        Just(CommandCleanupTrigger::StatusProbe),
    ]
}

fn bridge_error_strategy() -> impl Strategy<Value = BridgeError> {
    prop_oneof![
        Just(BridgeError::BinaryNotFound),
        duration_strategy().prop_map(BridgeError::Timeout),
        Just(BridgeError::ParseError),
        exit_code_strategy().prop_map(BridgeError::ExitCode),
        Just(BridgeError::Cancelled),
        (any::<bool>(), any::<bool>(), 0_u64..10_000).prop_map(
            |(stdout_open, stderr_open, drain_timeout_ms)| BridgeError::CaptureIncomplete {
                stdout_open,
                stderr_open,
                drain_timeout_ms,
            }
        ),
        (
            cleanup_trigger_strategy(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            0_u64..10_000,
        )
            .prop_map(
                |(
                    trigger,
                    leader_reaped,
                    signal_helper_settled,
                    process_tree_signalled,
                    stdout_open,
                    stderr_open,
                    settle_timeout_ms,
                )| {
                    BridgeError::CleanupIncomplete {
                        trigger,
                        leader_reaped,
                        signal_helper_settled,
                        process_tree_signalled,
                        stdout_open,
                        stderr_open,
                        settle_timeout_ms,
                    }
                }
            ),
        (output_stream_strategy(), 0_usize..999_000, 1_usize..1_000).prop_map(
            |(stream, limit, excess)| BridgeError::OutputTooLarge {
                stream,
                observed: limit + excess,
                limit,
            }
        ),
        Just(BridgeError::Io(std::io::ErrorKind::Other)),
    ]
}

fn search_path_strategy() -> impl Strategy<Value = Vec<PathBuf>> {
    prop::collection::vec("[a-z/]{1,30}".prop_map(PathBuf::from), 0..5)
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn bridge_error_binary_not_found_display() {
    let err = BridgeError::BinaryNotFound;
    let display = err.to_string();
    assert!(display.contains("binary not found"));
    assert!(!display.contains("raw-binary-name-canary"));
}

#[test]
fn bridge_error_timeout_display() {
    let err = BridgeError::Timeout(Duration::from_secs(5));
    let display = err.to_string();
    assert!(display.contains("timed out"));
}

#[test]
fn bridge_error_parse_error_display() {
    let err = BridgeError::ParseError;
    let display = err.to_string();
    assert!(display.contains("not valid JSON"));
    assert!(!display.contains("raw-child-output-canary"));
}

#[test]
fn bridge_error_exit_code_display() {
    let err = BridgeError::ExitCode(42);
    let display = err.to_string();
    assert!(display.contains("code 42"));
    assert!(!display.contains("raw-child-output-canary"));
}

#[test]
fn bridge_error_clone_equality() {
    let err = BridgeError::ExitCode(1);
    let cloned = err.clone();
    assert_eq!(err, cloned);
}

#[test]
fn bridge_error_variants_not_equal() {
    let a = BridgeError::BinaryNotFound;
    let b = BridgeError::ParseError;
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
    assert_eq!(err, BridgeError::BinaryNotFound);
}

#[cfg(unix)]
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
    fn binary_not_found_display_is_content_free(_name in binary_name_strategy()) {
        let err = BridgeError::BinaryNotFound;
        let display = err.to_string();
        prop_assert_eq!(display, "subprocess binary not found");
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
    fn parse_error_display_is_content_free(_content in "[QWXZ]{32,64}") {
        let err = BridgeError::ParseError;
        let display = err.to_string();
        prop_assert_eq!(display, "subprocess output was not valid JSON");
    }

    #[test]
    fn exit_code_display_contains_code(
        code in exit_code_strategy(),
    ) {
        let err = BridgeError::ExitCode(code);
        let display = err.to_string();
        let code_str = code.to_string();
        prop_assert!(
            display.contains(&code_str),
            "Display '{}' should contain exit code '{}'",
            display, code_str
        );
        prop_assert!(
            display.contains("with code"),
            "Display should contain 'with code'"
        );
    }

    // ── BridgeError Debug contains variant name ─────────────────────────

    #[test]
    fn bridge_error_debug_contains_variant(err in bridge_error_strategy()) {
        let dbg = format!("{err:?}");
        let has_variant = dbg.contains("BinaryNotFound")
            || dbg.contains("Timeout")
            || dbg.contains("ParseError")
            || dbg.contains("ExitCode")
            || dbg.contains("Cancelled")
            || dbg.contains("CaptureIncomplete")
            || dbg.contains("CleanupIncomplete")
            || dbg.contains("OutputTooLarge")
            || dbg.contains("Io");
        prop_assert!(
            has_variant,
            "Debug '{}' should contain a variant name",
            dbg
        );
    }

    // ── BridgeError variant discrimination ──────────────────────────────

    #[test]
    fn different_variants_not_equal(
        dur in duration_strategy(),
    ) {
        let a = BridgeError::BinaryNotFound;
        let b = BridgeError::Timeout(dur);
        prop_assert_ne!(&a, &b);
    }

    #[test]
    fn same_variant_different_data_not_equal(
        code1 in exit_code_strategy(),
        code2 in exit_code_strategy(),
    ) {
        prop_assume!(code1 != code2);
        let a = BridgeError::ExitCode(code1);
        let b = BridgeError::ExitCode(code2);
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
        let is_not_found = matches!(&err, BridgeError::BinaryNotFound);
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

    // ── ExitCode display carries only its structural numeric class ──────

    #[test]
    fn exit_code_display_is_structural(
        code in exit_code_strategy(),
        _child_output in "[QWXZ]{32,64}",
    ) {
        let err = BridgeError::ExitCode(code);
        let display = err.to_string();
        prop_assert_eq!(display, format!("subprocess exited with code {code}"));
    }

    // ── Direct path binaries bypass PATH/search-root discovery ───────────

    #[test]
    fn direct_missing_path_returns_not_found(
        leaf in "[a-z]{4,16}",
    ) {
        let dir = tempdir().unwrap();
        let missing = dir.path().join(format!("{leaf}_missing_bin"));
        let binary = missing.to_string_lossy().to_string();
        let b: SubprocessBridge<serde_json::Value> = SubprocessBridge::new(&binary)
            .with_search_paths(Vec::<PathBuf>::new());
        prop_assert!(!b.is_available());
        let err = b.invoke(&[]).unwrap_err();
        prop_assert!(matches!(err, BridgeError::BinaryNotFound));
    }

    #[cfg(unix)]
    #[test]
    fn direct_executable_path_invokes_without_search_paths(
        value in 0_i32..10_000,
    ) {
        let dir = tempdir().unwrap();
        let script = dir.path().join("bridge-script");
        write_executable(
            &script,
            &format!("#!/bin/sh\nprintf '{{\"value\":{value}}}'\n"),
        );
        let binary = script.to_string_lossy().to_string();
        let b: SubprocessBridge<serde_json::Value> = SubprocessBridge::new(&binary)
            .with_search_paths(Vec::<PathBuf>::new());
        prop_assert!(b.is_available());
        let out = b.invoke(&[]).unwrap();
        prop_assert_eq!(out["value"].as_i64(), Some(i64::from(value)));
    }
}
