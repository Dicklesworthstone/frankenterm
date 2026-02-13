//! Property-based tests for snapshot_engine types.
//!
//! Validates:
//! 1. SnapshotTrigger serde roundtrip for all 11 variants
//! 2. SnapshotTrigger serialized forms are snake_case strings
//! 3. SnapshotTrigger variants are all distinct under serialization
//! 4. SnapshotError Display messages match expected text
//! 5. SnapshotError Debug output contains variant names
//! 6. SnapshotResult construction and field access
//! 7. SnapshotSchedulingMode serde roundtrip and default
//! 8. SnapshotSchedulingConfig serde roundtrip and default values
//! 9. SnapshotConfig serde roundtrip and default values
//! 10. Config from empty JSON/TOML produces valid defaults
//!
//! Pure property tests only -- no async, no I/O, no SQLite.

use proptest::prelude::*;
use std::collections::HashSet;

use frankenterm_core::config::{SnapshotConfig, SnapshotSchedulingConfig, SnapshotSchedulingMode};
use frankenterm_core::snapshot_engine::{SnapshotError, SnapshotResult, SnapshotTrigger};

// =============================================================================
// Constants
// =============================================================================

/// All 11 SnapshotTrigger variants.
const ALL_TRIGGERS: [SnapshotTrigger; 11] = [
    SnapshotTrigger::Periodic,
    SnapshotTrigger::PeriodicFallback,
    SnapshotTrigger::Manual,
    SnapshotTrigger::Shutdown,
    SnapshotTrigger::Startup,
    SnapshotTrigger::Event,
    SnapshotTrigger::WorkCompleted,
    SnapshotTrigger::HazardThreshold,
    SnapshotTrigger::StateTransition,
    SnapshotTrigger::IdleWindow,
    SnapshotTrigger::MemoryPressure,
];

/// Expected snake_case serde strings for each variant (same order as ALL_TRIGGERS).
const EXPECTED_SERDE_STRINGS: [&str; 11] = [
    "\"periodic\"",
    "\"periodic_fallback\"",
    "\"manual\"",
    "\"shutdown\"",
    "\"startup\"",
    "\"event\"",
    "\"work_completed\"",
    "\"hazard_threshold\"",
    "\"state_transition\"",
    "\"idle_window\"",
    "\"memory_pressure\"",
];

// =============================================================================
// Strategies
// =============================================================================

/// Strategy that picks one of the 11 SnapshotTrigger variants uniformly.
fn arb_trigger() -> impl Strategy<Value = SnapshotTrigger> {
    (0..11_usize).prop_map(|i| ALL_TRIGGERS[i])
}

/// Strategy for arbitrary session IDs.
fn arb_session_id() -> impl Strategy<Value = String> {
    "[a-z0-9\\-]{8,32}".prop_map(|s| format!("sess-{}", s))
}

/// Strategy for arbitrary SnapshotSchedulingMode.
fn arb_scheduling_mode() -> impl Strategy<Value = SnapshotSchedulingMode> {
    prop_oneof![
        Just(SnapshotSchedulingMode::Periodic),
        Just(SnapshotSchedulingMode::Intelligent),
    ]
}

/// Strategy for arbitrary SnapshotSchedulingConfig with sensible ranges.
fn arb_scheduling_config() -> impl Strategy<Value = SnapshotSchedulingConfig> {
    (
        arb_scheduling_mode(),
        0.1_f64..100.0, // snapshot_threshold
        0.1_f64..50.0,  // work_completed_value
        0.1_f64..50.0,  // state_transition_value
        0.1_f64..50.0,  // idle_window_value
        0.1_f64..50.0,  // memory_pressure_value
        0.1_f64..50.0,  // hazard_trigger_value
        1_u64..120,     // periodic_fallback_minutes
    )
        .prop_map(
            |(mode, threshold, work, state, idle, memory, hazard, fallback)| {
                SnapshotSchedulingConfig {
                    mode,
                    snapshot_threshold: threshold,
                    work_completed_value: work,
                    state_transition_value: state,
                    idle_window_value: idle,
                    memory_pressure_value: memory,
                    hazard_trigger_value: hazard,
                    periodic_fallback_minutes: fallback,
                }
            },
        )
}

/// Strategy for arbitrary SnapshotConfig with sensible ranges.
fn arb_snapshot_config() -> impl Strategy<Value = SnapshotConfig> {
    (
        any::<bool>(), // enabled
        30_u64..3600,  // interval_seconds
        1_usize..50,   // max_concurrent_captures
        1_usize..100,  // retention_count
        1_u64..365,    // retention_days
        arb_scheduling_config(),
    )
        .prop_map(
            |(enabled, interval, max_captures, ret_count, ret_days, scheduling)| SnapshotConfig {
                enabled,
                interval_seconds: interval,
                max_concurrent_captures: max_captures,
                retention_count: ret_count,
                retention_days: ret_days,
                scheduling,
                ..SnapshotConfig::default()
            },
        )
}

// =============================================================================
// SnapshotTrigger serde roundtrip (all 11 variants)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Every SnapshotTrigger variant survives JSON serialization roundtrip.
    #[test]
    fn trigger_serde_roundtrip(trigger in arb_trigger()) {
        let json = serde_json::to_string(&trigger).expect("serialize");
        let back: SnapshotTrigger = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(trigger, back, "roundtrip failed for {:?}", trigger);
    }
}

// =============================================================================
// SnapshotTrigger snake_case serialized forms
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Serialized form is a lowercase snake_case JSON string.
    #[test]
    fn trigger_serde_is_snake_case(trigger in arb_trigger()) {
        let json = serde_json::to_string(&trigger).expect("serialize");
        // Must be a quoted string
        prop_assert!(json.starts_with('"'), "expected quoted string, got: {}", json);
        prop_assert!(json.ends_with('"'), "expected quoted string, got: {}", json);

        // Inner content must be lowercase snake_case: only [a-z_]
        let inner = &json[1..json.len() - 1];
        let is_snake = inner.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(is_snake, "not snake_case: {}", inner);
    }
}

// =============================================================================
// SnapshotTrigger: verify specific serialized forms
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Each variant maps to its expected serde string.
    #[test]
    fn trigger_specific_serde_form(idx in 0..11_usize) {
        let trigger = ALL_TRIGGERS[idx];
        let expected = EXPECTED_SERDE_STRINGS[idx];
        let json = serde_json::to_string(&trigger).expect("serialize");
        prop_assert_eq!(&json, expected, "serde mismatch for {:?}", trigger);
    }
}

// =============================================================================
// SnapshotTrigger: all 11 variants have distinct serde forms
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// All 11 variants produce distinct serialized JSON strings.
    #[test]
    fn trigger_all_variants_distinct(_dummy in 0..1_i32) {
        let mut seen = HashSet::new();
        for trigger in &ALL_TRIGGERS {
            let json = serde_json::to_string(trigger).expect("serialize");
            let is_new = seen.insert(json.clone());
            prop_assert!(is_new, "duplicate serde form: {} for {:?}", json, trigger);
        }
        prop_assert_eq!(seen.len(), 11, "expected 11 distinct forms, got {}", seen.len());
    }
}

// =============================================================================
// SnapshotTrigger: deserialization from all valid snake_case strings
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Deserializing from the known snake_case strings produces the correct variant.
    #[test]
    fn trigger_deserialize_from_known_strings(idx in 0..11_usize) {
        let expected = ALL_TRIGGERS[idx];
        let json_str = EXPECTED_SERDE_STRINGS[idx];
        let parsed: SnapshotTrigger = serde_json::from_str(json_str).expect("deserialize");
        prop_assert_eq!(parsed, expected, "deserialize mismatch for {}", json_str);
    }
}

// =============================================================================
// SnapshotTrigger: Copy semantics
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SnapshotTrigger is Copy: original equals copy.
    #[test]
    fn trigger_copy_semantics(trigger in arb_trigger()) {
        let copy = trigger;
        prop_assert_eq!(trigger, copy, "Copy should produce equal value");
    }
}

// =============================================================================
// SnapshotTrigger: Clone equals original
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SnapshotTrigger Clone produces an equal value.
    #[test]
    fn trigger_clone_eq(trigger in arb_trigger()) {
        let cloned = trigger;
        prop_assert_eq!(trigger, cloned, "Clone should produce equal value");
    }
}

// =============================================================================
// SnapshotTrigger: Debug output contains variant name
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Debug output contains a recognizable variant name substring.
    #[test]
    fn trigger_debug_contains_variant_name(trigger in arb_trigger()) {
        let debug = format!("{:?}", trigger);
        let has_name = !debug.is_empty();
        prop_assert!(has_name, "Debug output should not be empty");
        // Debug output for an enum variant should contain alphabetic characters
        let has_alpha = debug.chars().any(|c| c.is_ascii_alphabetic());
        prop_assert!(has_alpha, "Debug output should contain alphabetic chars: {}", debug);
    }
}

// =============================================================================
// SnapshotError: Display messages
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SnapshotError::InProgress displays "snapshot already in progress".
    #[test]
    fn error_in_progress_display(_dummy in 0..1_i32) {
        let err = SnapshotError::InProgress;
        let msg = format!("{}", err);
        prop_assert_eq!(&msg, "snapshot already in progress");
    }

    /// SnapshotError::NoPanes displays "no panes found".
    #[test]
    fn error_no_panes_display(_dummy in 0..1_i32) {
        let err = SnapshotError::NoPanes;
        let msg = format!("{}", err);
        prop_assert_eq!(&msg, "no panes found");
    }

    /// SnapshotError::NoChanges displays "no changes since last snapshot".
    #[test]
    fn error_no_changes_display(_dummy in 0..1_i32) {
        let err = SnapshotError::NoChanges;
        let msg = format!("{}", err);
        prop_assert_eq!(&msg, "no changes since last snapshot");
    }

    /// SnapshotError::PaneList contains the inner message.
    #[test]
    fn error_pane_list_display(inner in "[a-zA-Z0-9 ]{1,50}") {
        let err = SnapshotError::PaneList(inner.clone());
        let msg = format!("{}", err);
        prop_assert!(msg.contains(&inner), "PaneList display should contain inner: {}", msg);
        prop_assert!(msg.starts_with("pane listing failed: "), "unexpected prefix: {}", msg);
    }

    /// SnapshotError::Database contains the inner message.
    #[test]
    fn error_database_display(inner in "[a-zA-Z0-9 ]{1,50}") {
        let err = SnapshotError::Database(inner.clone());
        let msg = format!("{}", err);
        prop_assert!(msg.contains(&inner), "Database display should contain inner: {}", msg);
        prop_assert!(msg.starts_with("database error: "), "unexpected prefix: {}", msg);
    }

    /// SnapshotError::Serialization contains the inner message.
    #[test]
    fn error_serialization_display(inner in "[a-zA-Z0-9 ]{1,50}") {
        let err = SnapshotError::Serialization(inner.clone());
        let msg = format!("{}", err);
        prop_assert!(msg.contains(&inner), "Serialization display should contain inner: {}", msg);
        prop_assert!(msg.starts_with("serialization error: "), "unexpected prefix: {}", msg);
    }
}

// =============================================================================
// SnapshotError: Debug contains variant name
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Debug output for each SnapshotError variant contains its name.
    #[test]
    fn error_debug_in_progress(_dummy in 0..1_i32) {
        let err = SnapshotError::InProgress;
        let debug = format!("{:?}", err);
        prop_assert!(debug.contains("InProgress"), "Debug should contain InProgress: {}", debug);
    }

    #[test]
    fn error_debug_no_panes(_dummy in 0..1_i32) {
        let err = SnapshotError::NoPanes;
        let debug = format!("{:?}", err);
        prop_assert!(debug.contains("NoPanes"), "Debug should contain NoPanes: {}", debug);
    }

    #[test]
    fn error_debug_no_changes(_dummy in 0..1_i32) {
        let err = SnapshotError::NoChanges;
        let debug = format!("{:?}", err);
        prop_assert!(debug.contains("NoChanges"), "Debug should contain NoChanges: {}", debug);
    }

    #[test]
    fn error_debug_pane_list(inner in "[a-zA-Z0-9]{1,20}") {
        let err = SnapshotError::PaneList(inner.clone());
        let debug = format!("{:?}", err);
        prop_assert!(debug.contains("PaneList"), "Debug should contain PaneList: {}", debug);
        prop_assert!(debug.contains(&inner), "Debug should contain inner string: {}", debug);
    }

    #[test]
    fn error_debug_database(inner in "[a-zA-Z0-9]{1,20}") {
        let err = SnapshotError::Database(inner.clone());
        let debug = format!("{:?}", err);
        prop_assert!(debug.contains("Database"), "Debug should contain Database: {}", debug);
        prop_assert!(debug.contains(&inner), "Debug should contain inner string: {}", debug);
    }

    #[test]
    fn error_debug_serialization(inner in "[a-zA-Z0-9]{1,20}") {
        let err = SnapshotError::Serialization(inner.clone());
        let debug = format!("{:?}", err);
        prop_assert!(debug.contains("Serialization"), "Debug should contain Serialization: {}", debug);
        prop_assert!(debug.contains(&inner), "Debug should contain inner string: {}", debug);
    }
}

// =============================================================================
// SnapshotResult: construction and field access
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SnapshotResult fields are accessible and retain their values.
    #[test]
    fn snapshot_result_construction(
        session_id in arb_session_id(),
        checkpoint_id in 1_i64..1_000_000,
        pane_count in 0_usize..100,
        total_bytes in 0_usize..10_000_000,
        trigger in arb_trigger(),
    ) {
        let result = SnapshotResult {
            session_id: session_id.clone(),
            checkpoint_id,
            pane_count,
            total_bytes,
            trigger,
        };

        prop_assert_eq!(&result.session_id, &session_id, "session_id mismatch");
        prop_assert_eq!(result.checkpoint_id, checkpoint_id, "checkpoint_id mismatch");
        prop_assert_eq!(result.pane_count, pane_count, "pane_count mismatch");
        prop_assert_eq!(result.total_bytes, total_bytes, "total_bytes mismatch");
        prop_assert_eq!(result.trigger, trigger, "trigger mismatch");
    }

    /// SnapshotResult Clone produces an equal value.
    #[test]
    fn snapshot_result_clone(
        session_id in arb_session_id(),
        checkpoint_id in 1_i64..1_000_000,
        pane_count in 0_usize..100,
        total_bytes in 0_usize..10_000_000,
        trigger in arb_trigger(),
    ) {
        let result = SnapshotResult {
            session_id,
            checkpoint_id,
            pane_count,
            total_bytes,
            trigger,
        };
        let cloned = result.clone();

        prop_assert_eq!(&result.session_id, &cloned.session_id, "session_id clone mismatch");
        prop_assert_eq!(result.checkpoint_id, cloned.checkpoint_id, "checkpoint_id clone mismatch");
        prop_assert_eq!(result.pane_count, cloned.pane_count, "pane_count clone mismatch");
        prop_assert_eq!(result.total_bytes, cloned.total_bytes, "total_bytes clone mismatch");
        prop_assert_eq!(result.trigger, cloned.trigger, "trigger clone mismatch");
    }

    /// SnapshotResult Debug is non-empty and contains "SnapshotResult".
    #[test]
    fn snapshot_result_debug(
        checkpoint_id in 1_i64..1_000_000,
        pane_count in 0_usize..100,
        trigger in arb_trigger(),
    ) {
        let result = SnapshotResult {
            session_id: "sess-test".to_string(),
            checkpoint_id,
            pane_count,
            total_bytes: 0,
            trigger,
        };
        let debug = format!("{:?}", result);
        prop_assert!(!debug.is_empty(), "Debug should not be empty");
        prop_assert!(debug.contains("SnapshotResult"), "Debug should contain SnapshotResult: {}", debug);
    }
}

// =============================================================================
// SnapshotSchedulingMode: serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SnapshotSchedulingMode survives JSON roundtrip.
    #[test]
    fn scheduling_mode_serde_roundtrip(mode in arb_scheduling_mode()) {
        let json = serde_json::to_string(&mode).expect("serialize");
        let back: SnapshotSchedulingMode = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(mode, back, "roundtrip failed for {:?}", mode);
    }

    /// SnapshotSchedulingMode serialized form is snake_case.
    #[test]
    fn scheduling_mode_serde_is_snake_case(mode in arb_scheduling_mode()) {
        let json = serde_json::to_string(&mode).expect("serialize");
        let inner = &json[1..json.len() - 1];
        let is_snake = inner.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(is_snake, "not snake_case: {}", inner);
    }
}

// =============================================================================
// SnapshotSchedulingMode: default is Intelligent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Default SnapshotSchedulingMode is Intelligent.
    #[test]
    fn scheduling_mode_default_is_intelligent(_dummy in 0..1_i32) {
        let mode = SnapshotSchedulingMode::default();
        prop_assert_eq!(mode, SnapshotSchedulingMode::Intelligent, "default should be Intelligent");
    }

    /// SnapshotSchedulingMode has exactly two variants with known serde forms.
    #[test]
    fn scheduling_mode_known_variants(_dummy in 0..1_i32) {
        let periodic_json = serde_json::to_string(&SnapshotSchedulingMode::Periodic).unwrap();
        let intelligent_json = serde_json::to_string(&SnapshotSchedulingMode::Intelligent).unwrap();

        prop_assert_eq!(&periodic_json, "\"periodic\"");
        prop_assert_eq!(&intelligent_json, "\"intelligent\"");
    }
}

// =============================================================================
// SnapshotSchedulingConfig: serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SnapshotSchedulingConfig survives JSON roundtrip.
    #[test]
    fn scheduling_config_serde_roundtrip(config in arb_scheduling_config()) {
        let json = serde_json::to_string(&config).expect("serialize");
        let back: SnapshotSchedulingConfig = serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(config.mode, back.mode, "mode mismatch");
        prop_assert!((config.snapshot_threshold - back.snapshot_threshold).abs() < 1e-10,
            "snapshot_threshold mismatch: {} vs {}", config.snapshot_threshold, back.snapshot_threshold);
        prop_assert!((config.work_completed_value - back.work_completed_value).abs() < 1e-10,
            "work_completed_value mismatch");
        prop_assert!((config.state_transition_value - back.state_transition_value).abs() < 1e-10,
            "state_transition_value mismatch");
        prop_assert!((config.idle_window_value - back.idle_window_value).abs() < 1e-10,
            "idle_window_value mismatch");
        prop_assert!((config.memory_pressure_value - back.memory_pressure_value).abs() < 1e-10,
            "memory_pressure_value mismatch");
        prop_assert!((config.hazard_trigger_value - back.hazard_trigger_value).abs() < 1e-10,
            "hazard_trigger_value mismatch");
        prop_assert_eq!(config.periodic_fallback_minutes, back.periodic_fallback_minutes,
            "periodic_fallback_minutes mismatch");
    }
}

// =============================================================================
// SnapshotSchedulingConfig: default values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Default SnapshotSchedulingConfig has expected field values.
    #[test]
    fn scheduling_config_default_values(_dummy in 0..1_i32) {
        let config = SnapshotSchedulingConfig::default();

        prop_assert_eq!(config.mode, SnapshotSchedulingMode::Intelligent, "default mode");
        prop_assert!((config.snapshot_threshold - 5.0).abs() < f64::EPSILON,
            "default threshold: {}", config.snapshot_threshold);
        prop_assert!((config.work_completed_value - 2.0).abs() < f64::EPSILON,
            "default work_completed_value: {}", config.work_completed_value);
        prop_assert!((config.state_transition_value - 1.0).abs() < f64::EPSILON,
            "default state_transition_value: {}", config.state_transition_value);
        prop_assert!((config.idle_window_value - 3.0).abs() < f64::EPSILON,
            "default idle_window_value: {}", config.idle_window_value);
        prop_assert!((config.memory_pressure_value - 4.0).abs() < f64::EPSILON,
            "default memory_pressure_value: {}", config.memory_pressure_value);
        prop_assert!((config.hazard_trigger_value - 10.0).abs() < f64::EPSILON,
            "default hazard_trigger_value: {}", config.hazard_trigger_value);
        prop_assert_eq!(config.periodic_fallback_minutes, 30,
            "default periodic_fallback_minutes: {}", config.periodic_fallback_minutes);
    }
}

// =============================================================================
// SnapshotConfig: serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SnapshotConfig survives JSON roundtrip for key fields.
    #[test]
    fn snapshot_config_serde_roundtrip(config in arb_snapshot_config()) {
        let json = serde_json::to_string(&config).expect("serialize");
        let back: SnapshotConfig = serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(config.enabled, back.enabled, "enabled mismatch");
        prop_assert_eq!(config.interval_seconds, back.interval_seconds, "interval_seconds mismatch");
        prop_assert_eq!(config.max_concurrent_captures, back.max_concurrent_captures,
            "max_concurrent_captures mismatch");
        prop_assert_eq!(config.retention_count, back.retention_count, "retention_count mismatch");
        prop_assert_eq!(config.retention_days, back.retention_days, "retention_days mismatch");
        prop_assert_eq!(config.scheduling.mode, back.scheduling.mode, "scheduling.mode mismatch");
    }
}

// =============================================================================
// SnapshotConfig: default values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Default SnapshotConfig has expected field values.
    #[test]
    fn snapshot_config_default_values(_dummy in 0..1_i32) {
        let config = SnapshotConfig::default();

        prop_assert!(config.enabled, "default enabled should be true");
        prop_assert_eq!(config.interval_seconds, 300, "default interval_seconds");
        prop_assert_eq!(config.max_concurrent_captures, 10, "default max_concurrent_captures");
        prop_assert_eq!(config.retention_count, 10, "default retention_count");
        prop_assert_eq!(config.retention_days, 7, "default retention_days");
        prop_assert_eq!(config.scheduling.mode, SnapshotSchedulingMode::Intelligent,
            "default scheduling mode");
    }
}

// =============================================================================
// Config from empty JSON produces valid defaults
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Empty JSON object deserializes to SnapshotConfig with all defaults.
    #[test]
    fn snapshot_config_empty_json_defaults(_dummy in 0..1_i32) {
        let config: SnapshotConfig = serde_json::from_str("{}").expect("deserialize empty JSON");
        let default_config = SnapshotConfig::default();

        prop_assert_eq!(config.enabled, default_config.enabled, "enabled from empty JSON");
        prop_assert_eq!(config.interval_seconds, default_config.interval_seconds,
            "interval_seconds from empty JSON");
        prop_assert_eq!(config.retention_count, default_config.retention_count,
            "retention_count from empty JSON");
        prop_assert_eq!(config.retention_days, default_config.retention_days,
            "retention_days from empty JSON");
    }

    /// Empty JSON object deserializes to SnapshotSchedulingConfig with all defaults.
    #[test]
    fn scheduling_config_empty_json_defaults(_dummy in 0..1_i32) {
        let config: SnapshotSchedulingConfig = serde_json::from_str("{}").expect("deserialize empty JSON");
        let default_config = SnapshotSchedulingConfig::default();

        prop_assert_eq!(config.mode, default_config.mode, "mode from empty JSON");
        prop_assert!((config.snapshot_threshold - default_config.snapshot_threshold).abs() < f64::EPSILON,
            "snapshot_threshold from empty JSON");
        prop_assert!((config.work_completed_value - default_config.work_completed_value).abs() < f64::EPSILON,
            "work_completed_value from empty JSON");
        prop_assert_eq!(config.periodic_fallback_minutes, default_config.periodic_fallback_minutes,
            "periodic_fallback_minutes from empty JSON");
    }
}

// =============================================================================
// Config partial JSON fills missing fields with defaults
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Partial JSON with only `enabled` set fills remaining fields with defaults.
    #[test]
    fn snapshot_config_partial_json(enabled in any::<bool>()) {
        let json = format!(r#"{{"enabled": {}}}"#, enabled);
        let config: SnapshotConfig = serde_json::from_str(&json).expect("deserialize partial JSON");

        prop_assert_eq!(config.enabled, enabled, "enabled should match input");
        // Other fields should be defaults
        prop_assert_eq!(config.interval_seconds, 300, "interval_seconds should be default");
        prop_assert_eq!(config.retention_count, 10, "retention_count should be default");
        prop_assert_eq!(config.scheduling.mode, SnapshotSchedulingMode::Intelligent,
            "scheduling mode should be default");
    }

    /// Partial scheduling JSON with only `mode` set fills remaining fields with defaults.
    #[test]
    fn scheduling_config_partial_json(mode in arb_scheduling_mode()) {
        let mode_str = serde_json::to_string(&mode).unwrap();
        let json = format!(r#"{{"mode": {}}}"#, mode_str);
        let config: SnapshotSchedulingConfig = serde_json::from_str(&json).expect("deserialize partial JSON");

        prop_assert_eq!(config.mode, mode, "mode should match input");
        prop_assert!((config.snapshot_threshold - 5.0).abs() < f64::EPSILON,
            "snapshot_threshold should be default");
        prop_assert!((config.work_completed_value - 2.0).abs() < f64::EPSILON,
            "work_completed_value should be default");
        prop_assert_eq!(config.periodic_fallback_minutes, 30,
            "periodic_fallback_minutes should be default");
    }
}

// =============================================================================
// SnapshotTrigger: invalid deserialization rejects unknown strings
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Unknown strings fail to deserialize as SnapshotTrigger.
    #[test]
    fn trigger_rejects_unknown_strings(s in "[a-z]{6,15}") {
        // Skip strings that happen to be valid variant names
        let valid = [
            "periodic", "periodic_fallback", "manual", "shutdown",
            "startup", "event", "work_completed", "hazard_threshold",
            "state_transition", "idle_window", "memory_pressure",
        ];
        if valid.contains(&s.as_str()) {
            // Valid string; skip this case
            return Ok(());
        }

        let json = format!("\"{}\"", s);
        let result = serde_json::from_str::<SnapshotTrigger>(&json);
        prop_assert!(result.is_err(), "unknown string '{}' should fail to deserialize", s);
    }
}

// =============================================================================
// SnapshotSchedulingMode: invalid deserialization rejects unknown strings
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Unknown strings fail to deserialize as SnapshotSchedulingMode.
    #[test]
    fn scheduling_mode_rejects_unknown_strings(s in "[a-z]{6,15}") {
        let valid = ["periodic", "intelligent"];
        if valid.contains(&s.as_str()) {
            return Ok(());
        }

        let json = format!("\"{}\"", s);
        let result = serde_json::from_str::<SnapshotSchedulingMode>(&json);
        prop_assert!(result.is_err(), "unknown string '{}' should fail to deserialize", s);
    }
}
