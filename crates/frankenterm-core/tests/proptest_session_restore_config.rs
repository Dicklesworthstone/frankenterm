//! Property-based tests for `SessionRestoreConfig` and `LogConfig`.
//!
//! Covers serde roundtrips (JSON + TOML), defaults from empty JSON, partial
//! deserialization, double-roundtrip stability, forward compatibility, and
//! boundary values for both `#[serde(default)]` config structs.
//!
//! Properties:
//!  1. SessionRestoreConfig JSON serde roundtrip preserves all fields
//!  2. SessionRestoreConfig serialization is deterministic
//!  3. SessionRestoreConfig default from empty JSON
//!  4. SessionRestoreConfig partial JSON fills missing with defaults
//!  5. SessionRestoreConfig Default() matches serde default
//!  6. SessionRestoreConfig TOML roundtrip preserves all fields
//!  7. SessionRestoreConfig double roundtrip (JSON→struct→JSON) is stable
//!  8. SessionRestoreConfig JSON with extra fields deserializes (forward compat)
//!  9. SessionRestoreConfig all 4 bool combos produce valid configs
//! 10. SessionRestoreConfig max_lines 0 roundtrips correctly
//! 11. SessionRestoreConfig max_lines large values roundtrip
//! 12. SessionRestoreConfig partial with only max_lines preserves bools as default
//! 13. LogConfig JSON serde roundtrip preserves all fields
//! 14. LogConfig serialization is deterministic
//! 15. LogConfig default from empty JSON
//! 16. LogConfig partial level fills missing with defaults
//! 17. LogConfig Default() matches serde default
//! 18. LogConfig TOML roundtrip preserves all fields
//! 19. LogConfig double roundtrip (JSON→struct→JSON) is stable
//! 20. LogConfig JSON with extra fields deserializes (forward compat)
//! 21. LogConfig partial with only format fills missing with defaults
//! 22. LogConfig file path roundtrips through JSON
//! 23. SessionRestoreConfig TOML double roundtrip is stable
//! 24. LogConfig TOML double roundtrip is stable
//! 25. SessionRestoreConfig negation of bool fields roundtrips

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
// SessionRestoreConfig properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Property 1: JSON serde roundtrip preserves all fields.
    #[test]
    fn prop_restore_config_serde(config in arb_session_restore_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.auto_restore, config.auto_restore);
        prop_assert_eq!(back.restore_scrollback, config.restore_scrollback);
        prop_assert_eq!(back.restore_max_lines, config.restore_max_lines);
    }

    /// Property 2: Serialization is deterministic.
    #[test]
    fn prop_restore_config_deterministic(config in arb_session_restore_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// Property 3: Empty JSON deserializes to default values.
    #[test]
    fn prop_restore_config_default_from_empty(_dummy in 0..1_u8) {
        let config: SessionRestoreConfig = serde_json::from_str("{}").unwrap();
        prop_assert!(!config.auto_restore);
        prop_assert!(!config.restore_scrollback);
        prop_assert_eq!(config.restore_max_lines, 5000);
    }

    /// Property 4: Partial JSON fills missing fields with defaults.
    #[test]
    fn prop_restore_config_partial(auto_restore in any::<bool>()) {
        let json = format!("{{\"auto_restore\":{}}}", auto_restore);
        let config: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.auto_restore, auto_restore);
        // Missing fields use defaults
        prop_assert!(!config.restore_scrollback);
        prop_assert_eq!(config.restore_max_lines, 5000);
    }

    /// Property 5: Default() matches serde default from empty JSON.
    #[test]
    fn prop_restore_config_default_consistent(_dummy in 0..1_u8) {
        let from_default = SessionRestoreConfig::default();
        let from_json: SessionRestoreConfig = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(from_default.auto_restore, from_json.auto_restore);
        prop_assert_eq!(from_default.restore_scrollback, from_json.restore_scrollback);
        prop_assert_eq!(from_default.restore_max_lines, from_json.restore_max_lines);
    }

    /// Property 6: TOML roundtrip preserves all fields.
    #[test]
    fn prop_restore_config_toml_roundtrip(config in arb_session_restore_config()) {
        let toml_str = toml::to_string(&config).unwrap();
        let back: SessionRestoreConfig = toml::from_str(&toml_str).unwrap();
        prop_assert_eq!(back.auto_restore, config.auto_restore,
            "auto_restore mismatch after TOML roundtrip");
        prop_assert_eq!(back.restore_scrollback, config.restore_scrollback,
            "restore_scrollback mismatch after TOML roundtrip");
        prop_assert_eq!(back.restore_max_lines, config.restore_max_lines,
            "restore_max_lines mismatch after TOML roundtrip");
    }

    /// Property 7: Double roundtrip (serialize→deserialize→serialize) is stable.
    #[test]
    fn prop_restore_config_double_roundtrip(config in arb_session_restore_config()) {
        let json1 = serde_json::to_string(&config).unwrap();
        let mid: SessionRestoreConfig = serde_json::from_str(&json1).unwrap();
        let json2 = serde_json::to_string(&mid).unwrap();
        prop_assert_eq!(&json1, &json2,
            "double roundtrip should produce identical JSON");
    }

    /// Property 8: JSON with extra fields deserializes correctly (forward compat).
    #[test]
    fn prop_restore_config_forward_compat(config in arb_session_restore_config()) {
        let json = format!(
            "{{\"auto_restore\":{},\"restore_scrollback\":{},\"restore_max_lines\":{},\"future_flag\":true,\"new_field\":\"hello\"}}",
            config.auto_restore, config.restore_scrollback, config.restore_max_lines
        );
        let back: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.auto_restore, config.auto_restore,
            "extra fields should not affect auto_restore");
        prop_assert_eq!(back.restore_scrollback, config.restore_scrollback,
            "extra fields should not affect restore_scrollback");
        prop_assert_eq!(back.restore_max_lines, config.restore_max_lines,
            "extra fields should not affect restore_max_lines");
    }

    /// Property 9: All 4 boolean combinations produce valid configs.
    #[test]
    fn prop_restore_config_all_bool_combos(_dummy in 0..1_u8) {
        for auto in [false, true] {
            for scrollback in [false, true] {
                let config = SessionRestoreConfig {
                    auto_restore: auto,
                    restore_scrollback: scrollback,
                    restore_max_lines: 1000,
                };
                let json = serde_json::to_string(&config).unwrap();
                let back: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(back.auto_restore, auto);
                prop_assert_eq!(back.restore_scrollback, scrollback);
            }
        }
    }

    /// Property 10: max_lines = 0 roundtrips correctly.
    #[test]
    fn prop_restore_config_zero_max_lines(
        auto in any::<bool>(),
        scrollback in any::<bool>(),
    ) {
        let config = SessionRestoreConfig {
            auto_restore: auto,
            restore_scrollback: scrollback,
            restore_max_lines: 0,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.restore_max_lines, 0,
            "max_lines=0 should roundtrip correctly");
    }

    /// Property 11: Large max_lines values roundtrip correctly.
    #[test]
    fn prop_restore_config_large_max_lines(
        max_lines in 100_000_usize..10_000_000,
    ) {
        let config = SessionRestoreConfig {
            auto_restore: true,
            restore_scrollback: true,
            restore_max_lines: max_lines,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.restore_max_lines, max_lines,
            "large max_lines {} should roundtrip correctly", max_lines);
    }

    /// Property 12: Partial JSON with only max_lines preserves bool defaults.
    #[test]
    fn prop_restore_config_partial_max_lines_only(max_lines in 0_usize..50_000) {
        let json = format!("{{\"restore_max_lines\":{}}}", max_lines);
        let config: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert!(!config.auto_restore,
            "auto_restore should default to false");
        prop_assert!(!config.restore_scrollback,
            "restore_scrollback should default to false");
        prop_assert_eq!(config.restore_max_lines, max_lines,
            "restore_max_lines should match the provided value");
    }
}

// =========================================================================
// LogConfig properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Property 13: JSON serde roundtrip preserves all fields.
    #[test]
    fn prop_log_config_serde(config in arb_log_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.level, &config.level);
        prop_assert_eq!(back.format, config.format);
        prop_assert_eq!(back.file, config.file);
    }

    /// Property 14: Serialization is deterministic.
    #[test]
    fn prop_log_config_deterministic(config in arb_log_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// Property 15: Empty JSON deserializes to default values.
    #[test]
    fn prop_log_config_default_from_empty(_dummy in 0..1_u8) {
        let config: LogConfig = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(&config.level, "info");
        prop_assert_eq!(config.format, frankenterm_core::config::LogFormat::Pretty);
        prop_assert!(config.file.is_none());
    }

    /// Property 16: Partial JSON fills missing fields with defaults.
    #[test]
    fn prop_log_config_partial_level(level in "[a-z]{3,10}") {
        let json = format!("{{\"level\":\"{}\"}}", level);
        let config: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&config.level, &level);
        prop_assert_eq!(config.format, frankenterm_core::config::LogFormat::Pretty);
        prop_assert!(config.file.is_none());
    }

    /// Property 17: Default() matches serde default from empty JSON.
    #[test]
    fn prop_log_config_default_consistent(_dummy in 0..1_u8) {
        let from_default = LogConfig::default();
        let from_json: LogConfig = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(&from_default.level, &from_json.level);
        prop_assert_eq!(from_default.format, from_json.format);
        prop_assert_eq!(from_default.file, from_json.file);
    }

    /// Property 18: TOML roundtrip preserves all fields.
    #[test]
    fn prop_log_config_toml_roundtrip(config in arb_log_config()) {
        let toml_str = toml::to_string(&config).unwrap();
        let back: LogConfig = toml::from_str(&toml_str).unwrap();
        prop_assert_eq!(&back.level, &config.level,
            "level mismatch after TOML roundtrip");
        prop_assert_eq!(back.format, config.format,
            "format mismatch after TOML roundtrip");
        prop_assert_eq!(back.file, config.file,
            "file mismatch after TOML roundtrip");
    }

    /// Property 19: Double roundtrip (serialize→deserialize→serialize) is stable.
    #[test]
    fn prop_log_config_double_roundtrip(config in arb_log_config()) {
        let json1 = serde_json::to_string(&config).unwrap();
        let mid: LogConfig = serde_json::from_str(&json1).unwrap();
        let json2 = serde_json::to_string(&mid).unwrap();
        prop_assert_eq!(&json1, &json2,
            "double roundtrip should produce identical JSON");
    }

    /// Property 20: JSON with extra fields deserializes correctly (forward compat).
    #[test]
    fn prop_log_config_forward_compat(config in arb_log_config()) {
        let file_json = match &config.file {
            Some(p) => format!("\"{}\"", p.display()),
            None => "null".to_string(),
        };
        let format_str = match config.format {
            frankenterm_core::config::LogFormat::Pretty => "\"pretty\"",
            frankenterm_core::config::LogFormat::Json => "\"json\"",
        };
        let json = format!(
            "{{\"level\":\"{}\",\"format\":{},\"file\":{},\"verbose\":true,\"rotation\":\"daily\"}}",
            config.level, format_str, file_json
        );
        let back: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.level, &config.level,
            "extra fields should not affect level");
        prop_assert_eq!(back.format, config.format,
            "extra fields should not affect format");
    }

    /// Property 21: Partial JSON with only format fills missing with defaults.
    #[test]
    fn prop_log_config_partial_format_only(
        format in prop_oneof![
            Just(frankenterm_core::config::LogFormat::Pretty),
            Just(frankenterm_core::config::LogFormat::Json),
        ]
    ) {
        let format_str = match format {
            frankenterm_core::config::LogFormat::Pretty => "\"pretty\"",
            frankenterm_core::config::LogFormat::Json => "\"json\"",
        };
        let json = format!("{{\"format\":{}}}", format_str);
        let config: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&config.level, "info",
            "level should default to 'info'");
        prop_assert_eq!(config.format, format,
            "format should match the provided value");
        prop_assert!(config.file.is_none(),
            "file should default to None");
    }

    /// Property 22: File path roundtrips through JSON.
    #[test]
    fn prop_log_config_file_path_roundtrip(
        path in "[a-z]{3,10}/[a-z]{3,10}\\.log"
    ) {
        let config = LogConfig {
            level: "info".to_string(),
            format: frankenterm_core::config::LogFormat::Pretty,
            file: Some(std::path::PathBuf::from(&path)),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            back.file.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some(path.clone()),
            "file path should roundtrip: expected {}", path
        );
    }

    /// Property 23: SessionRestoreConfig TOML double roundtrip is stable.
    #[test]
    fn prop_restore_config_toml_double_roundtrip(config in arb_session_restore_config()) {
        let toml1 = toml::to_string(&config).unwrap();
        let mid: SessionRestoreConfig = toml::from_str(&toml1).unwrap();
        let toml2 = toml::to_string(&mid).unwrap();
        prop_assert_eq!(&toml1, &toml2,
            "TOML double roundtrip should produce identical output");
    }

    /// Property 24: LogConfig TOML double roundtrip is stable.
    #[test]
    fn prop_log_config_toml_double_roundtrip(config in arb_log_config()) {
        let toml1 = toml::to_string(&config).unwrap();
        let mid: LogConfig = toml::from_str(&toml1).unwrap();
        let toml2 = toml::to_string(&mid).unwrap();
        prop_assert_eq!(&toml1, &toml2,
            "TOML double roundtrip should produce identical output");
    }

    /// Property 25: Negation of bool fields roundtrips correctly.
    #[test]
    fn prop_restore_config_negation_roundtrip(config in arb_session_restore_config()) {
        let negated = SessionRestoreConfig {
            auto_restore: !config.auto_restore,
            restore_scrollback: !config.restore_scrollback,
            restore_max_lines: config.restore_max_lines,
        };
        let json = serde_json::to_string(&negated).unwrap();
        let back: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.auto_restore, !config.auto_restore,
            "negated auto_restore mismatch");
        prop_assert_eq!(back.restore_scrollback, !config.restore_scrollback,
            "negated restore_scrollback mismatch");
        prop_assert_eq!(back.restore_max_lines, config.restore_max_lines,
            "restore_max_lines should be unchanged");
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
