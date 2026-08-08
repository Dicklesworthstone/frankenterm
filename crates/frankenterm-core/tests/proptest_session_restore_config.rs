//! Property-based tests for `SessionRestoreConfig` and `LogConfig`.
//!
//! Covers serde roundtrips (JSON + TOML), defaults from empty JSON, partial
//! deserialization, double-roundtrip stability, forward compatibility, and
//! boundary values. `SessionRestoreConfig` uses an explicit wire form so the
//! retired `restore_max_lines` key is rejected even when null; `LogConfig`
//! continues to use ordinary serde defaults.
//!
//! Generated properties (numbered to match the test comments below):
//!  1. SessionRestoreConfig JSON roundtrip
//!  2. SessionRestoreConfig deterministic JSON
//!  3. SessionRestoreConfig partial JSON defaults
//!  4. SessionRestoreConfig TOML roundtrip
//!  5. SessionRestoreConfig stable JSON double roundtrip
//!  6. SessionRestoreConfig unknown-field tolerance
//!  7. LogConfig JSON roundtrip
//!  8. LogConfig deterministic JSON
//!  9. LogConfig partial-level defaults
//! 10. LogConfig TOML roundtrip
//! 11. LogConfig stable JSON double roundtrip
//! 12. LogConfig unknown-field tolerance
//! 13. LogConfig partial-format defaults
//! 14. LogConfig file-path field JSON and TOML roundtrip
//! 15. SessionRestoreConfig stable TOML double roundtrip
//! 16. LogConfig stable TOML double roundtrip
//! 17. Negated SessionRestoreConfig JSON roundtrip
//! 18. SessionRestoreConfig exact serialized field catalog
//! 19. Non-ASCII LogConfig file-path JSON and TOML roundtrip
//!
//! Fixed examples below the generated properties cover defaults, the complete
//! four-value boolean matrix, and retired-key rejection without pretending a
//! one-value generator is a property.

use frankenterm_core::logging::LogConfig;
use frankenterm_core::session_restore::SessionRestoreConfig;
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_session_restore_config() -> impl Strategy<Value = SessionRestoreConfig> {
    (any::<bool>(), any::<bool>()).prop_map(|(auto_restore, restore_scrollback)| {
        SessionRestoreConfig {
            auto_restore,
            restore_scrollback,
        }
    })
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

fn arb_non_ascii_log_path() -> impl Strategy<Value = String> {
    (
        prop_oneof![
            Just("日志"),
            Just("résultats"),
            Just("данные"),
            Just("δοκιμές"),
            Just("🚀"),
        ],
        "[a-z0-9_-]{1,16}",
    )
        .prop_map(|(directory, stem)| format!("{directory}/{stem}.log"))
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
    }

    /// Property 2: Serialization is deterministic.
    #[test]
    fn prop_restore_config_deterministic(config in arb_session_restore_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// Property 3: Partial JSON fills missing fields with defaults.
    #[test]
    fn prop_restore_config_partial(auto_restore in any::<bool>()) {
        let json = format!("{{\"auto_restore\":{}}}", auto_restore);
        let config: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.auto_restore, auto_restore);
        // Missing fields use defaults
        prop_assert!(!config.restore_scrollback);
    }

    /// Property 4: TOML roundtrip preserves all fields.
    #[test]
    fn prop_restore_config_toml_roundtrip(config in arb_session_restore_config()) {
        let toml_str = toml::to_string(&config).unwrap();
        let back: SessionRestoreConfig = toml::from_str(&toml_str).unwrap();
        prop_assert_eq!(back.auto_restore, config.auto_restore,
            "auto_restore mismatch after TOML roundtrip");
        prop_assert_eq!(back.restore_scrollback, config.restore_scrollback,
            "restore_scrollback mismatch after TOML roundtrip");
    }

    /// Property 5: Double roundtrip (serialize→deserialize→serialize) is stable.
    #[test]
    fn prop_restore_config_double_roundtrip(config in arb_session_restore_config()) {
        let json1 = serde_json::to_string(&config).unwrap();
        let mid: SessionRestoreConfig = serde_json::from_str(&json1).unwrap();
        let json2 = serde_json::to_string(&mid).unwrap();
        prop_assert_eq!(&json1, &json2,
            "double roundtrip should produce identical JSON");
    }

    /// Property 6: JSON with extra fields deserializes correctly (forward compat).
    #[test]
    fn prop_restore_config_forward_compat(config in arb_session_restore_config()) {
        let json = format!(
            "{{\"auto_restore\":{},\"restore_scrollback\":{},\"future_flag\":true,\"new_field\":\"hello\"}}",
            config.auto_restore, config.restore_scrollback
        );
        let back: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.auto_restore, config.auto_restore,
            "extra fields should not affect auto_restore");
        prop_assert_eq!(back.restore_scrollback, config.restore_scrollback,
            "extra fields should not affect restore_scrollback");
    }

}

// =========================================================================
// LogConfig properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Property 7: JSON serde roundtrip preserves all fields.
    #[test]
    fn prop_log_config_serde(config in arb_log_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.level, &config.level);
        prop_assert_eq!(back.format, config.format);
        prop_assert_eq!(back.file, config.file);
    }

    /// Property 8: Serialization is deterministic.
    #[test]
    fn prop_log_config_deterministic(config in arb_log_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// Property 9: Partial JSON fills missing fields with defaults.
    #[test]
    fn prop_log_config_partial_level(level in "[a-z]{3,10}") {
        let json = format!("{{\"level\":\"{}\"}}", level);
        let config: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&config.level, &level);
        prop_assert_eq!(config.format, frankenterm_core::config::LogFormat::Pretty);
        prop_assert!(config.file.is_none());
    }

    /// Property 10: TOML roundtrip preserves all fields.
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

    /// Property 11: Double roundtrip (serialize→deserialize→serialize) is stable.
    #[test]
    fn prop_log_config_double_roundtrip(config in arb_log_config()) {
        let json1 = serde_json::to_string(&config).unwrap();
        let mid: LogConfig = serde_json::from_str(&json1).unwrap();
        let json2 = serde_json::to_string(&mid).unwrap();
        prop_assert_eq!(&json1, &json2,
            "double roundtrip should produce identical JSON");
    }

    /// Property 12: JSON with extra fields deserializes correctly (forward compat).
    #[test]
    fn prop_log_config_forward_compat(config in arb_log_config()) {
        let mut value = serde_json::to_value(&config).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("verbose".to_string(), serde_json::json!(true));
        object.insert("rotation".to_string(), serde_json::json!("daily"));
        let back: LogConfig = serde_json::from_value(value).unwrap();
        prop_assert_eq!(&back.level, &config.level,
            "extra fields should not affect level");
        prop_assert_eq!(back.format, config.format,
            "extra fields should not affect format");
        prop_assert_eq!(back.file, config.file,
            "extra fields should not affect file");
    }

    /// Property 13: Partial JSON with only format fills missing with defaults.
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

    /// Property 14: File-path field roundtrips through JSON and TOML.
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
        let json_back: LogConfig = serde_json::from_str(&json).unwrap();
        let toml = toml::to_string(&config).unwrap();
        let toml_back: LogConfig = toml::from_str(&toml).unwrap();
        prop_assert_eq!(
            json_back.file.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some(path.clone()),
            "JSON file path should roundtrip: expected {}", path
        );
        prop_assert_eq!(
            toml_back.file.as_ref().map(|p| p.to_string_lossy().to_string()),
            Some(path.clone()),
            "TOML file path should roundtrip: expected {}", path
        );
    }

    /// Property 15: SessionRestoreConfig TOML double roundtrip is stable.
    #[test]
    fn prop_restore_config_toml_double_roundtrip(config in arb_session_restore_config()) {
        let toml1 = toml::to_string(&config).unwrap();
        let mid: SessionRestoreConfig = toml::from_str(&toml1).unwrap();
        let toml2 = toml::to_string(&mid).unwrap();
        prop_assert_eq!(&toml1, &toml2,
            "TOML double roundtrip should produce identical output");
    }

    /// Property 16: LogConfig TOML double roundtrip is stable.
    #[test]
    fn prop_log_config_toml_double_roundtrip(config in arb_log_config()) {
        let toml1 = toml::to_string(&config).unwrap();
        let mid: LogConfig = toml::from_str(&toml1).unwrap();
        let toml2 = toml::to_string(&mid).unwrap();
        prop_assert_eq!(&toml1, &toml2,
            "TOML double roundtrip should produce identical output");
    }

    /// Property 17: Negation of bool fields roundtrips correctly.
    #[test]
    fn prop_restore_config_negation_roundtrip(config in arb_session_restore_config()) {
        let negated = SessionRestoreConfig {
            auto_restore: !config.auto_restore,
            restore_scrollback: !config.restore_scrollback,
        };
        let json = serde_json::to_string(&negated).unwrap();
        let back: SessionRestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.auto_restore, !config.auto_restore,
            "negated auto_restore mismatch");
        prop_assert_eq!(back.restore_scrollback, !config.restore_scrollback,
            "negated restore_scrollback mismatch");
    }

    /// Property 18: SessionRestoreConfig JSON has its exact field catalog.
    #[test]
    fn prop_restore_config_json_field_catalog(config in arb_session_restore_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        let keys = obj
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        prop_assert_eq!(
            keys,
            std::collections::BTreeSet::from(["auto_restore", "restore_scrollback"])
        );
    }

    /// Property 19: Non-ASCII file paths roundtrip through JSON and TOML.
    #[test]
    fn prop_log_config_non_ascii_file_path_roundtrip(path in arb_non_ascii_log_path()) {
        let config = LogConfig {
            level: "info".to_string(),
            format: frankenterm_core::config::LogFormat::Json,
            file: Some(std::path::PathBuf::from(&path)),
        };
        let json = serde_json::to_string(&config).unwrap();
        let json_back: LogConfig = serde_json::from_str(&json).unwrap();
        let toml = toml::to_string(&config).unwrap();
        let toml_back: LogConfig = toml::from_str(&toml).unwrap();
        prop_assert_eq!(json_back.file.as_deref(), config.file.as_deref());
        prop_assert_eq!(toml_back.file.as_deref(), config.file.as_deref());
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn restore_config_explicit_true_fields_roundtrip() {
    let json = r#"{"auto_restore":true,"restore_scrollback":true}"#;
    let config: SessionRestoreConfig = serde_json::from_str(json).unwrap();
    assert!(config.auto_restore);
    assert!(config.restore_scrollback);
}

#[test]
fn restore_config_defaults_match_empty_json_and_all_boolean_combinations_roundtrip() {
    let from_default = SessionRestoreConfig::default();
    let from_json: SessionRestoreConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(from_default.auto_restore, from_json.auto_restore);
    assert_eq!(
        from_default.restore_scrollback,
        from_json.restore_scrollback
    );

    for auto_restore in [false, true] {
        for restore_scrollback in [false, true] {
            let config = SessionRestoreConfig {
                auto_restore,
                restore_scrollback,
            };
            let encoded = serde_json::to_string(&config).unwrap();
            let decoded: SessionRestoreConfig = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.auto_restore, auto_restore);
            assert_eq!(decoded.restore_scrollback, restore_scrollback);
        }
    }
}

#[test]
fn log_config_defaults_match_empty_json() {
    let from_default = LogConfig::default();
    let from_json: LogConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(from_default.level, from_json.level);
    assert_eq!(from_default.format, from_json.format);
    assert_eq!(from_default.file, from_json.file);
}

#[test]
fn retired_restore_max_lines_presence_rejects_null_scalar_and_map() {
    for value in [
        serde_json::Value::Null,
        serde_json::json!(5_000),
        serde_json::json!({"credential_canary": "must-not-echo"}),
    ] {
        let error = serde_json::from_value::<SessionRestoreConfig>(serde_json::json!({
            "restore_max_lines": value,
        }))
        .expect_err("every explicit retired restore_max_lines value must fail closed")
        .to_string();
        assert!(error.contains("session.restore_max_lines was removed"));
        assert!(!error.contains("must-not-echo"));
    }
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
