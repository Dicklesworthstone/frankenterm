//! Property-based tests for the `build_coord` module.
//!
//! Covers `detect_cargo_command` parsing invariants (alias resolution,
//! whitespace handling, non-cargo rejection), `BuildCoordConfig` serde
//! roundtrips and default values, and `BuildLockMetadata` serde roundtrips.

use std::path::PathBuf;
use std::time::Duration;

use frankenterm_core::build_coord::{BuildCoordConfig, BuildLockMetadata, detect_cargo_command};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_build_coord_config() -> impl Strategy<Value = BuildCoordConfig> {
    (
        any::<bool>(),                               // enabled
        0_u64..3600,                                 // wait_timeout secs
        1_u64..5000,                                 // poll_interval ms
        any::<bool>(),                               // shared_target_dir
        proptest::option::of("[a-z/]{3,20}"),        // target_dir_override
        any::<bool>(),                               // auto_sccache
        proptest::option::of("[a-z/]{3,20}"),        // lock_dir_override
    )
        .prop_map(
            |(enabled, timeout_s, poll_ms, shared, target_override, auto_sccache, lock_override)| {
                BuildCoordConfig {
                    enabled,
                    wait_timeout: Duration::from_secs(timeout_s),
                    poll_interval: Duration::from_millis(poll_ms),
                    shared_target_dir: shared,
                    target_dir_override: target_override.map(PathBuf::from),
                    auto_sccache,
                    lock_dir_override: lock_override.map(PathBuf::from),
                }
            },
        )
}

fn arb_build_lock_metadata() -> impl Strategy<Value = BuildLockMetadata> {
    (
        1_u32..100_000,                            // pid
        "[a-z]{3,10}",                             // cargo_command
        "[a-z/]{5,30}",                            // project_root
        0_u64..10_000_000_000,                     // started_at
        "[a-z:0-9 ]{5,20}",                       // started_at_human
        "[0-9.]{3,10}",                            // ft_version
        proptest::option::of("[A-Za-z]{3,15}"),    // agent_name
        proptest::option::of(0_u64..1000),         // pane_id
    )
        .prop_map(
            |(pid, cmd, root, started, human, version, agent, pane)| BuildLockMetadata {
                pid,
                cargo_command: cmd,
                project_root: root,
                started_at: started,
                started_at_human: human,
                ft_version: version,
                agent_name: agent,
                pane_id: pane,
            },
        )
}

// =========================================================================
// detect_cargo_command — known subcommands
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Known cargo subcommands always resolve to the canonical name.
    #[test]
    fn prop_known_subcommand_resolved(
        suffix in "[a-z -]{0,20}",
    ) {
        let cases = [
            ("cargo build", "build"),
            ("cargo b", "build"),
            ("cargo check", "check"),
            ("cargo c", "check"),
            ("cargo test", "test"),
            ("cargo t", "test"),
            ("cargo bench", "bench"),
            ("cargo clippy", "clippy"),
            ("cargo run", "run"),
            ("cargo r", "run"),
            ("cargo doc", "doc"),
        ];
        for (input, expected) in &cases {
            let cmd = format!("{input} {suffix}");
            let result = detect_cargo_command(&cmd);
            prop_assert_eq!(result, Some(*expected), "input: {}", cmd);
        }
    }

    /// Nextest invocations always resolve to "test".
    #[test]
    fn prop_nextest_is_test(args in "[a-z -]{0,20}") {
        let cmd = format!("cargo nextest {args}");
        let result = detect_cargo_command(&cmd);
        prop_assert_eq!(result, Some("test"), "nextest should be 'test': {}", cmd);
    }
}

// =========================================================================
// detect_cargo_command — rejection
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// Non-cargo commands always return None.
    #[test]
    fn prop_non_cargo_rejected(cmd in "[a-z]{1,5} [a-z -]{0,20}") {
        // Skip if cmd accidentally starts with "cargo "
        if !cmd.starts_with("cargo ") && !cmd.contains("cargo nextest") {
            prop_assert_eq!(detect_cargo_command(&cmd), None, "should reject: {}", cmd);
        }
    }

    /// Unknown cargo subcommands return None.
    #[test]
    fn prop_unknown_subcommand_rejected(sub in "fmt|update|publish|add|remove|init|new|search") {
        let cmd = format!("cargo {sub}");
        prop_assert_eq!(detect_cargo_command(&cmd), None, "should reject: {}", cmd);
    }

    /// Empty and whitespace-only strings return None.
    #[test]
    fn prop_empty_returns_none(spaces in " {0,5}") {
        prop_assert_eq!(detect_cargo_command(&spaces), None);
    }
}

// =========================================================================
// detect_cargo_command — idempotence and consistency
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// detect_cargo_command is deterministic.
    #[test]
    fn prop_detect_deterministic(cmd in "[a-z ]{1,30}") {
        let r1 = detect_cargo_command(&cmd);
        let r2 = detect_cargo_command(&cmd);
        prop_assert_eq!(r1, r2);
    }

    /// Leading whitespace is tolerated (trimmed).
    #[test]
    fn prop_leading_whitespace_trimmed(
        spaces in " {1,5}",
        subcmd in "build|check|test|bench|clippy|run|doc",
    ) {
        let padded = format!("{spaces}cargo {subcmd}");
        let unpadded = format!("cargo {subcmd}");
        prop_assert_eq!(
            detect_cargo_command(&padded),
            detect_cargo_command(&unpadded),
            "leading whitespace should be trimmed"
        );
    }

    /// Result is always a static known string (from a fixed set).
    #[test]
    fn prop_result_is_known_string(cmd in "[a-z ]{1,30}") {
        if let Some(result) = detect_cargo_command(&cmd) {
            let known = ["build", "check", "test", "bench", "clippy", "run", "doc"];
            prop_assert!(
                known.contains(&result),
                "result '{result}' should be in known set"
            );
        }
    }
}

// =========================================================================
// BuildCoordConfig — serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// BuildCoordConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(config in arb_build_coord_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: BuildCoordConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.enabled, config.enabled);
        prop_assert_eq!(back.wait_timeout, config.wait_timeout);
        prop_assert_eq!(back.poll_interval, config.poll_interval);
        prop_assert_eq!(back.shared_target_dir, config.shared_target_dir);
        prop_assert_eq!(back.target_dir_override, config.target_dir_override);
        prop_assert_eq!(back.auto_sccache, config.auto_sccache);
        prop_assert_eq!(back.lock_dir_override, config.lock_dir_override);
    }
}

// =========================================================================
// BuildLockMetadata — serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// BuildLockMetadata serde roundtrip preserves all fields.
    #[test]
    fn prop_lock_metadata_serde_roundtrip(meta in arb_build_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let back: BuildLockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pid, meta.pid);
        prop_assert_eq!(&back.cargo_command, &meta.cargo_command);
        prop_assert_eq!(&back.project_root, &meta.project_root);
        prop_assert_eq!(back.started_at, meta.started_at);
        prop_assert_eq!(&back.started_at_human, &meta.started_at_human);
        prop_assert_eq!(&back.ft_version, &meta.ft_version);
        prop_assert_eq!(back.agent_name, meta.agent_name);
        prop_assert_eq!(back.pane_id, meta.pane_id);
    }
}

// =========================================================================
// Default config values
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Default config always has the documented values.
    #[test]
    fn prop_default_config_values(_dummy in 0..1_u8) {
        let config = BuildCoordConfig::default();
        prop_assert!(config.enabled);
        prop_assert_eq!(config.wait_timeout, Duration::from_secs(600));
        prop_assert_eq!(config.poll_interval, Duration::from_millis(500));
        prop_assert!(config.shared_target_dir);
        prop_assert!(config.auto_sccache);
        prop_assert!(config.target_dir_override.is_none());
        prop_assert!(config.lock_dir_override.is_none());
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn detect_cargo_nextest_standalone() {
    assert_eq!(detect_cargo_command("cargo-nextest run"), Some("test"));
}

#[test]
fn detect_cargo_with_flags() {
    assert_eq!(
        detect_cargo_command("cargo build --release -j4"),
        Some("build")
    );
    assert_eq!(
        detect_cargo_command("cargo test -- --nocapture --test-threads=1"),
        Some("test")
    );
}

#[test]
fn config_serde_default_roundtrips() {
    let config = BuildCoordConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let back: BuildCoordConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.enabled, config.enabled);
    assert_eq!(back.wait_timeout, config.wait_timeout);
}

#[test]
fn metadata_optional_fields_serialize_correctly() {
    let meta = BuildLockMetadata {
        pid: 1234,
        cargo_command: "build".to_string(),
        project_root: "/home/user/project".to_string(),
        started_at: 1700000000,
        started_at_human: "unix:1700000000".to_string(),
        ft_version: "0.1.0".to_string(),
        agent_name: None,
        pane_id: None,
    };
    let json = serde_json::to_string(&meta).unwrap();
    let back: BuildLockMetadata = serde_json::from_str(&json).unwrap();
    assert!(back.agent_name.is_none());
    assert!(back.pane_id.is_none());
}
