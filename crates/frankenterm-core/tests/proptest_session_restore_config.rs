//! Property-based tests for `SessionRestoreConfig` and `LogConfig`.
//!
//! Covers serde roundtrips, defaults from empty JSON, and partial
//! deserialization for both `#[serde(default)]` config structs.

use frankenterm_core::logging::LogConfig;
use frankenterm_core::session_restore::SessionRestoreConfig;
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_session_restore_config() -> impl Strategy<Value = SessionRestoreConfig> {
    (any::<bool>(), any::<bool>(), 0_usize..50_000).prop_map(
        |(auto_restore, restore_scrollback, restore_max_lines)| SessionRestoreConfig {
            auto_restore,
            restore_scrollback,
            restore_max_lines,
        },
    )
}

fn arb_log_config() -> impl Strategy<Value = LogConfig> {
    (
        prop_oneof![
            Just("trace".to_string()),
            Just("debug".to_string()),
            Just("info".to_string()),
            Just("warn".to_string()),
            Just("error".to_string()),
        ],
        prop_oneof![
            Just(frankenterm_core::config::LogFormat::Pretty),
            Just(frankenterm_core::config::LogFormat::Json),
        ],
        proptest::option::of("[a-z/]{5,20}\\.log"),
    )
        .prop_map(|(level, format, file)| LogConfig {
            level,
            format,
            file: file.map(std::path::PathBuf::from),
        })
}

// =========================================================================
// SessionRestoreConfig — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_restore_config_serde(config in arb_session_restore_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.auto_restore, config.auto_restore);
        prop_assert_eq!(back.restore_scrollback, config.restore_scrollback);
        prop_assert_eq!(back.restore_max_lines, config.restore_max_lines);
    }

    #[test]
    fn prop_restore_config_deterministic(config in arb_session_restore_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// Empty JSON deserializes to default values.
    #[test]
    fn prop_restore_config_default_from_empty(_dummy in 0..1_u8) {
        let config: SessionRestoreConfig = serde_json::from_str("{}").unwrap();
        prop_assert!(!config.auto_restore);
        prop_assert!(!config.restore_scrollback);
        prop_assert_eq!(config.restore_max_lines, 5000);
    }

    /// Partial JSON fills missing fields with defaults.
    #[test]
    fn prop_restore_config_partial(auto_restore in any::<bool>()) {
        let json = format!("{{\"auto_restore\":{auto_restore}}}");
        let config: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.auto_restore, auto_restore);
        // Missing fields use defaults
        prop_assert!(!config.restore_scrollback);
        prop_assert_eq!(config.restore_max_lines, 5000);
    }

    /// Default() matches serde default from empty JSON.
    #[test]
    fn prop_restore_config_default_consistent(_dummy in 0..1_u8) {
        let from_default = SessionRestoreConfig::default();
        let from_json: SessionRestoreConfig = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(from_default.auto_restore, from_json.auto_restore);
        prop_assert_eq!(from_default.restore_scrollback, from_json.restore_scrollback);
        prop_assert_eq!(from_default.restore_max_lines, from_json.restore_max_lines);
    }
}

// =========================================================================
// LogConfig — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_log_config_serde(config in arb_log_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.level, &config.level);
        prop_assert_eq!(back.format, config.format);
        prop_assert_eq!(back.file, config.file);
    }

    #[test]
    fn prop_log_config_deterministic(config in arb_log_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// Empty JSON deserializes to default values.
    #[test]
    fn prop_log_config_default_from_empty(_dummy in 0..1_u8) {
        let config: LogConfig = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(&config.level, "info");
        prop_assert_eq!(config.format, frankenterm_core::config::LogFormat::Pretty);
        prop_assert!(config.file.is_none());
    }

    /// Partial JSON fills missing fields with defaults.
    #[test]
    fn prop_log_config_partial_level(level in "[a-z]{3,10}") {
        let json = format!("{{\"level\":\"{level}\"}}");
        let config: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&config.level, &level);
        prop_assert_eq!(config.format, frankenterm_core::config::LogFormat::Pretty);
        prop_assert!(config.file.is_none());
    }

    /// Default() matches serde default from empty JSON.
    #[test]
    fn prop_log_config_default_consistent(_dummy in 0..1_u8) {
        let from_default = LogConfig::default();
        let from_json: LogConfig = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(&from_default.level, &from_json.level);
        prop_assert_eq!(from_default.format, from_json.format);
        prop_assert_eq!(from_default.file, from_json.file);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn restore_config_with_null_file_roundtrips() {
    let json = r#"{"auto_restore":true,"restore_scrollback":true,"restore_max_lines":1000}"#;
    let config: SessionRestoreConfig = serde_json::from_str(json).unwrap();
    assert!(config.auto_restore);
    assert!(config.restore_scrollback);
    assert_eq!(config.restore_max_lines, 1000);
}

#[test]
fn log_config_json_format_roundtrips() {
    let json = r#"{"level":"debug","format":"json","file":"/tmp/test.log"}"#;
    let config: LogConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.level, "debug");
    assert_eq!(config.format, frankenterm_core::config::LogFormat::Json);
    assert_eq!(
        config.file.as_deref(),
        Some(std::path::Path::new("/tmp/test.log"))
    );
}
