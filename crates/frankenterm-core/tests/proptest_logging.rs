//! Property-based tests for the logging module.
//!
//! Tests invariants of LogLevel, LogFormat, LogConfig, and LogError
//! using proptest-generated inputs.

use frankenterm_core::config::LogFormat;
use frankenterm_core::logging::{LogConfig, LogLevel};
use proptest::prelude::*;
use std::path::PathBuf;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_log_level() -> impl Strategy<Value = LogLevel> {
    prop_oneof![
        Just(LogLevel::Trace),
        Just(LogLevel::Debug),
        Just(LogLevel::Info),
        Just(LogLevel::Warn),
        Just(LogLevel::Error),
    ]
}

fn arb_log_format() -> impl Strategy<Value = LogFormat> {
    prop_oneof![Just(LogFormat::Pretty), Just(LogFormat::Json),]
}

fn arb_level_string() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("trace".to_string()),
        Just("debug".to_string()),
        Just("info".to_string()),
        Just("warn".to_string()),
        Just("warning".to_string()),
        Just("error".to_string()),
        "[a-z]{1,10}".prop_map(|s| s), // random strings for invalid cases
    ]
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
        arb_log_format(),
        prop::option::of("[a-z/]{1,50}".prop_map(PathBuf::from)),
    )
        .prop_map(|(level, format, file)| LogConfig {
            level,
            format,
            file,
        })
}

// ── LogLevel: total order ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Reflexivity: every LogLevel equals itself.
    #[test]
    fn log_level_reflexive(a in arb_log_level()) {
        prop_assert_eq!(a, a);
    }

    /// Antisymmetry: if a <= b and b <= a then a == b.
    #[test]
    fn log_level_antisymmetric(a in arb_log_level(), b in arb_log_level()) {
        if a <= b && b <= a {
            prop_assert_eq!(a, b);
        }
    }

    /// Transitivity: if a <= b and b <= c then a <= c.
    #[test]
    fn log_level_transitive(
        a in arb_log_level(),
        b in arb_log_level(),
        c in arb_log_level(),
    ) {
        if a <= b && b <= c {
            prop_assert!(a <= c, "transitivity violated: {:?} <= {:?} <= {:?}", a, b, c);
        }
    }

    /// Totality: for any two LogLevels, a <= b or b <= a.
    #[test]
    fn log_level_total(a in arb_log_level(), b in arb_log_level()) {
        prop_assert!(a <= b || b <= a, "totality violated: {:?} vs {:?}", a, b);
    }

    /// The canonical ordering is Trace < Debug < Info < Warn < Error.
    #[test]
    fn log_level_canonical_order(a in arb_log_level(), b in arb_log_level()) {
        let rank = |l: LogLevel| -> u8 {
            match l {
                LogLevel::Trace => 0,
                LogLevel::Debug => 1,
                LogLevel::Info  => 2,
                LogLevel::Warn  => 3,
                LogLevel::Error => 4,
            }
        };
        prop_assert_eq!(a.cmp(&b), rank(a).cmp(&rank(b)),
            "ordering mismatch for {:?} vs {:?}", a, b);
    }
}

// ── LogLevel: FromStr ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Parsing is case-insensitive: any casing of a known level parses.
    #[test]
    fn log_level_parse_case_insensitive(level in arb_log_level()) {
        let lower = match level {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info  => "info",
            LogLevel::Warn  => "warn",
            LogLevel::Error => "error",
        };
        // lowercase always works
        let parsed: LogLevel = lower.parse().unwrap();
        prop_assert_eq!(parsed, level);
        // uppercase should also work
        let upper = lower.to_uppercase();
        let parsed_upper: LogLevel = upper.parse().unwrap();
        prop_assert_eq!(parsed_upper, level);
    }

    /// "warning" is an accepted alias for Warn.
    #[test]
    fn log_level_warning_alias(_dummy in 0..1u8) {
        let parsed: LogLevel = "warning".parse().unwrap();
        prop_assert_eq!(parsed, LogLevel::Warn);
        let parsed_upper: LogLevel = "WARNING".parse().unwrap();
        prop_assert_eq!(parsed_upper, LogLevel::Warn);
    }

    /// Random strings that aren't valid level names produce Err.
    #[test]
    fn log_level_parse_invalid(s in "[a-z]{1,8}") {
        let valid = ["trace", "debug", "info", "warn", "warning", "error"];
        if !valid.contains(&s.as_str()) {
            let result = s.parse::<LogLevel>();
            prop_assert!(result.is_err(), "expected Err for invalid level: {}", s);
            let err_msg = result.unwrap_err();
            prop_assert!(err_msg.contains("unknown log level"),
                "error message should mention 'unknown log level', got: {}", err_msg);
        }
    }

    /// The error message for invalid levels always includes the input.
    #[test]
    fn log_level_error_includes_input(s in "[a-z]{3,8}") {
        let valid = ["trace", "debug", "info", "warn", "warning", "error"];
        if !valid.contains(&s.as_str()) {
            let err_msg = s.parse::<LogLevel>().unwrap_err();
            prop_assert!(err_msg.contains(&s),
                "error should contain input '{}', got: {}", s, err_msg);
        }
    }
}

// ── LogLevel: Into<tracing::Level> ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Every LogLevel variant converts to a distinct tracing::Level.
    #[test]
    fn log_level_into_tracing_level_roundtrip(level in arb_log_level()) {
        let tracing_level: tracing::Level = level.into();
        // Verify the mapping is correct
        let expected = match level {
            LogLevel::Trace => tracing::Level::TRACE,
            LogLevel::Debug => tracing::Level::DEBUG,
            LogLevel::Info  => tracing::Level::INFO,
            LogLevel::Warn  => tracing::Level::WARN,
            LogLevel::Error => tracing::Level::ERROR,
        };
        prop_assert_eq!(tracing_level, expected);
    }
}

// ── LogFormat: Display ↔ FromStr roundtrip ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Display → FromStr roundtrip preserves the variant.
    #[test]
    fn log_format_display_parse_roundtrip(fmt in arb_log_format()) {
        let displayed = fmt.to_string();
        let parsed: LogFormat = displayed.parse().unwrap();
        prop_assert_eq!(parsed, fmt, "roundtrip failed for {:?}", fmt);
    }

    /// Display always produces lowercase output.
    #[test]
    fn log_format_display_is_lowercase(fmt in arb_log_format()) {
        let s = fmt.to_string();
        let lower = s.to_lowercase();
        prop_assert_eq!(s, lower, "Display should be lowercase");
    }

    /// Parsing is case-insensitive.
    #[test]
    fn log_format_parse_case_insensitive(fmt in arb_log_format()) {
        let lower = fmt.to_string();
        let upper = lower.to_uppercase();
        let mixed = {
            let mut chars = lower.chars();
            let mut s = String::new();
            if let Some(c) = chars.next() {
                s.push(c.to_uppercase().next().unwrap());
            }
            for c in chars {
                s.push(c);
            }
            s
        };
        let p1: LogFormat = lower.parse().unwrap();
        let p2: LogFormat = upper.parse().unwrap();
        let p3: LogFormat = mixed.parse().unwrap();
        prop_assert_eq!(p1, fmt);
        prop_assert_eq!(p2, fmt);
        prop_assert_eq!(p3, fmt);
    }

    /// Invalid strings produce parse errors.
    #[test]
    fn log_format_parse_invalid(s in "[a-z]{1,8}") {
        if s != "pretty" && s != "json" {
            let result = s.parse::<LogFormat>();
            prop_assert!(result.is_err(),
                "expected Err for invalid format: {}", s);
        }
    }
}

// ── LogFormat: serde roundtrip ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// JSON serde roundtrip preserves LogFormat.
    #[test]
    fn log_format_serde_roundtrip(fmt in arb_log_format()) {
        let json = serde_json::to_string(&fmt).unwrap();
        let parsed: LogFormat = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, fmt, "serde roundtrip failed for {:?}", fmt);
    }

    /// Serde serializes to lowercase string (rename_all = "lowercase").
    #[test]
    fn log_format_serde_produces_lowercase(fmt in arb_log_format()) {
        let json = serde_json::to_string(&fmt).unwrap();
        // json is like "\"pretty\"" or "\"json\""
        let unquoted = json.trim_matches('"');
        prop_assert_eq!(unquoted, unquoted.to_lowercase(),
            "serde should produce lowercase, got: {}", json);
    }

    /// Display and serde agree on the string representation.
    #[test]
    fn log_format_display_matches_serde(fmt in arb_log_format()) {
        let display_str = fmt.to_string();
        let serde_str = serde_json::to_string(&fmt).unwrap();
        let serde_unquoted = serde_str.trim_matches('"');
        prop_assert_eq!(display_str.as_str(), serde_unquoted,
            "Display != serde for {:?}", fmt);
    }
}

// ── LogConfig: serde roundtrip ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// JSON serde roundtrip preserves all LogConfig fields.
    #[test]
    fn log_config_serde_roundtrip(config in arb_log_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let parsed: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.level, config.level,
            "level mismatch after roundtrip");
        prop_assert_eq!(parsed.format, config.format,
            "format mismatch after roundtrip");
        prop_assert_eq!(parsed.file, config.file,
            "file mismatch after roundtrip");
    }

    /// Serialized LogConfig is valid JSON.
    #[test]
    fn log_config_serializes_to_valid_json(config in arb_log_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object(), "LogConfig should serialize as an object");
    }

    /// LogConfig JSON always has "level" and "format" fields.
    #[test]
    fn log_config_json_has_required_fields(config in arb_log_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("level").is_some(), "missing 'level' field");
        prop_assert!(value.get("format").is_some(), "missing 'format' field");
    }

    /// Pretty-printed JSON also roundtrips correctly.
    #[test]
    fn log_config_pretty_json_roundtrip(config in arb_log_config()) {
        let json = serde_json::to_string_pretty(&config).unwrap();
        let parsed: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.level, config.level);
        prop_assert_eq!(parsed.format, config.format);
        prop_assert_eq!(parsed.file, config.file);
    }
}

// ── LogConfig: Default ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default LogConfig is stable — calling default() N times produces the same config.
    #[test]
    fn log_config_default_is_stable(_i in 0..10u32) {
        let a = LogConfig::default();
        let b = LogConfig::default();
        prop_assert_eq!(a.level, b.level);
        prop_assert_eq!(a.format, b.format);
        prop_assert_eq!(a.file, b.file);
    }

    /// Default LogConfig roundtrips through JSON.
    #[test]
    fn log_config_default_roundtrip(_i in 0..1u8) {
        let config = LogConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: LogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.level, "info");
        prop_assert_eq!(parsed.format, LogFormat::Pretty);
        prop_assert!(parsed.file.is_none());
    }

    /// Empty JSON object deserializes to default values.
    #[test]
    fn log_config_empty_json_gives_defaults(_i in 0..1u8) {
        let config: LogConfig = serde_json::from_str("{}").unwrap();
        let default = LogConfig::default();
        prop_assert_eq!(config.level, default.level);
        prop_assert_eq!(config.format, default.format);
        prop_assert_eq!(config.file, default.file);
    }
}

// ── LogConfig: Clone ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Clone produces an equivalent LogConfig.
    #[test]
    fn log_config_clone_equivalent(config in arb_log_config()) {
        let cloned = config.clone();
        prop_assert_eq!(cloned.level, config.level);
        prop_assert_eq!(cloned.format, config.format);
        prop_assert_eq!(cloned.file, config.file);
    }
}

// ── LogLevel + LogFormat: Debug ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Debug format is non-empty for all LogLevels.
    #[test]
    fn log_level_debug_non_empty(level in arb_log_level()) {
        let debug = format!("{:?}", level);
        prop_assert!(!debug.is_empty(),
            "Debug format should be non-empty for {:?}", level);
    }

    /// Debug format is non-empty for all LogFormats.
    #[test]
    fn log_format_debug_non_empty(fmt in arb_log_format()) {
        let debug = format!("{:?}", fmt);
        prop_assert!(!debug.is_empty(),
            "Debug format should be non-empty for {:?}", fmt);
    }

    /// LogConfig Debug format contains "LogConfig".
    #[test]
    fn log_config_debug_contains_type_name(config in arb_log_config()) {
        let debug = format!("{:?}", config);
        prop_assert!(debug.contains("LogConfig"),
            "Debug should contain 'LogConfig', got: {}", debug);
    }
}

// ── LogLevel: Copy semantics ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Copy leaves original unchanged.
    #[test]
    fn log_level_copy_semantics(level in arb_log_level()) {
        let copied = level;
        prop_assert_eq!(level, copied, "Copy should leave original equal");
    }

    /// LogFormat Copy leaves original unchanged.
    #[test]
    fn log_format_copy_semantics(fmt in arb_log_format()) {
        let copied = fmt;
        prop_assert_eq!(fmt, copied, "Copy should leave original equal");
    }
}

// ── Cross-property: level string parsing ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Valid level strings in LogConfig parse to the expected LogLevel.
    #[test]
    fn log_config_level_is_parseable(config in arb_log_config()) {
        let result = config.level.parse::<LogLevel>();
        // Our strategy only generates valid level strings
        prop_assert!(result.is_ok(),
            "config level '{}' should be parseable", config.level);
    }

    /// Random level strings either parse or fail consistently.
    #[test]
    fn log_level_parse_deterministic(s in arb_level_string()) {
        let r1 = s.parse::<LogLevel>();
        let r2 = s.parse::<LogLevel>();
        match (&r1, &r2) {
            (Ok(a), Ok(b)) => prop_assert_eq!(a, b, "parse is non-deterministic for '{}'", s),
            (Err(_), Err(_)) => {} // both fail — ok
            _ => prop_assert!(false, "parse produced different Ok/Err for '{}'", s),
        }
    }
}

// ── LogFormat: Default ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// LogFormat::default() is Pretty.
    #[test]
    fn log_format_default_is_pretty(_i in 0..1u8) {
        prop_assert_eq!(LogFormat::default(), LogFormat::Pretty);
    }
}
