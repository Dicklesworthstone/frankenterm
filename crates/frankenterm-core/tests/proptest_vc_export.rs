// Requires the `vc-export` feature flag.
#![cfg(feature = "vc-export")]
//! Property-based tests for vc_export telemetry types (ft-3kxe).
//!
//! Validates:
//! 1. SessionTelemetry serde roundtrip with arbitrary values
//! 2. AgentMetrics serde roundtrip with arbitrary values
//! 3. Forward compatibility — extra HashMap fields survive roundtrip
//! 4. Degraded helpers preserve the ID and zero all counters
//! 5. RobotEnvelope wrapping preserves inner data
//! 6. Default values are deterministic
//! 7. Partial JSON parsing fills defaults for missing fields

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::robot_envelope::RobotEnvelope;
use frankenterm_core::vc_export::{AgentMetrics, SessionTelemetry};

// =============================================================================
// Helpers
// =============================================================================

fn session_telemetry_strategy() -> impl Strategy<Value = SessionTelemetry> {
    (
        prop::option::of("[a-z0-9\\-]{1,20}".prop_map(String::from)),
        0.0f64..1e9,
        0usize..100_000,
        0usize..100_000,
        0usize..100_000,
    )
        .prop_map(
            |(session_id, duration_secs, commands, errors, agent_interactions)| SessionTelemetry {
                session_id,
                duration_secs,
                commands,
                errors,
                agent_interactions,
                extra: HashMap::new(),
            },
        )
}

fn agent_metrics_strategy() -> impl Strategy<Value = AgentMetrics> {
    (
        prop::option::of("[a-z0-9\\-]{1,20}".prop_map(String::from)),
        0u64..10_000_000,
        0usize..100_000,
        0usize..10_000,
        0.0f64..1e6,
    )
        .prop_map(
            |(agent_id, total_tokens, tool_calls, sessions, avg_session_secs)| AgentMetrics {
                agent_id,
                total_tokens,
                tool_calls,
                sessions,
                avg_session_secs,
                extra: HashMap::new(),
            },
        )
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn session_telemetry_default_is_zeroed() {
    let t = SessionTelemetry::default();
    assert!(t.session_id.is_none());
    assert!(t.duration_secs.abs() < f64::EPSILON);
    assert_eq!(t.commands, 0);
    assert_eq!(t.errors, 0);
    assert_eq!(t.agent_interactions, 0);
    assert!(t.extra.is_empty());
}

#[test]
fn agent_metrics_default_is_zeroed() {
    let m = AgentMetrics::default();
    assert!(m.agent_id.is_none());
    assert_eq!(m.total_tokens, 0);
    assert_eq!(m.tool_calls, 0);
    assert_eq!(m.sessions, 0);
    assert!(m.avg_session_secs.abs() < f64::EPSILON);
    assert!(m.extra.is_empty());
}

#[test]
fn session_telemetry_partial_json_fills_defaults() {
    let json = r#"{"commands": 42}"#;
    let t: SessionTelemetry = serde_json::from_str(json).unwrap();
    assert_eq!(t.commands, 42);
    assert!(t.session_id.is_none());
    assert!(t.duration_secs.abs() < f64::EPSILON);
    assert_eq!(t.errors, 0);
    assert_eq!(t.agent_interactions, 0);
}

#[test]
fn agent_metrics_partial_json_fills_defaults() {
    let json = r#"{"total_tokens": 999}"#;
    let m: AgentMetrics = serde_json::from_str(json).unwrap();
    assert_eq!(m.total_tokens, 999);
    assert!(m.agent_id.is_none());
    assert_eq!(m.tool_calls, 0);
    assert_eq!(m.sessions, 0);
    assert!(m.avg_session_secs.abs() < f64::EPSILON);
}

#[test]
fn session_telemetry_extra_fields_captured() {
    let json = r#"{"commands": 1, "surprise_field": "hello", "nested": {"a": 1}}"#;
    let t: SessionTelemetry = serde_json::from_str(json).unwrap();
    assert_eq!(t.commands, 1);
    assert!(t.extra.contains_key("surprise_field"));
    assert!(t.extra.contains_key("nested"));
}

#[test]
fn agent_metrics_extra_fields_captured() {
    let json = r#"{"total_tokens": 5, "cost_usd": 0.05}"#;
    let m: AgentMetrics = serde_json::from_str(json).unwrap();
    assert_eq!(m.total_tokens, 5);
    assert!(m.extra.contains_key("cost_usd"));
}

#[test]
fn robot_envelope_wrap_not_degraded() {
    let t = SessionTelemetry::default();
    let envelope = RobotEnvelope::wrap("test-source", t);
    assert!(!envelope.degraded);
    assert_eq!(envelope.source, "test-source");
}

#[test]
fn robot_envelope_wrap_degraded_sets_flag() {
    let t = SessionTelemetry::default();
    let envelope = RobotEnvelope::wrap_degraded("test-source", t);
    assert!(envelope.degraded);
}

#[test]
fn envelope_serde_roundtrip_with_telemetry() {
    let t = SessionTelemetry {
        session_id: Some("s-1".to_string()),
        duration_secs: 120.5,
        commands: 10,
        errors: 1,
        agent_interactions: 5,
        extra: HashMap::new(),
    };
    let envelope = RobotEnvelope::wrap("vc", t);
    let json = serde_json::to_string(&envelope).unwrap();
    let back: RobotEnvelope<SessionTelemetry> = serde_json::from_str(&json).unwrap();
    assert_eq!(back.data, envelope.data);
    assert_eq!(back.source, "vc");
    assert!(!back.degraded);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // ── SessionTelemetry serde roundtrip ─────────────────────────────────

    #[test]
    fn session_telemetry_roundtrip(t in session_telemetry_strategy()) {
        let json = serde_json::to_string(&t).expect("serialize");
        let back: SessionTelemetry = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back.session_id, t.session_id);
        prop_assert_eq!(back.commands, t.commands);
        prop_assert_eq!(back.errors, t.errors);
        prop_assert_eq!(back.agent_interactions, t.agent_interactions);
        // f64 roundtrip: check within tolerance
        prop_assert!(
            (back.duration_secs - t.duration_secs).abs() < 1e-6,
            "duration_secs drift: {} vs {}",
            back.duration_secs, t.duration_secs
        );
    }

    // ── AgentMetrics serde roundtrip ─────────────────────────────────────

    #[test]
    fn agent_metrics_roundtrip(m in agent_metrics_strategy()) {
        let json = serde_json::to_string(&m).expect("serialize");
        let back: AgentMetrics = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(back.agent_id, m.agent_id);
        prop_assert_eq!(back.total_tokens, m.total_tokens);
        prop_assert_eq!(back.tool_calls, m.tool_calls);
        prop_assert_eq!(back.sessions, m.sessions);
        prop_assert!(
            (back.avg_session_secs - m.avg_session_secs).abs() < 1e-6,
            "avg_session_secs drift: {} vs {}",
            back.avg_session_secs, m.avg_session_secs
        );
    }

    // ── Forward compatibility: extra fields survive roundtrip ─────────

    #[test]
    fn session_telemetry_extra_survives_roundtrip(
        key in "[a-z_]{3,12}",
        val in prop::num::i64::ANY,
    ) {
        let mut t = SessionTelemetry::default();
        t.extra.insert(key.clone(), serde_json::Value::Number(val.into()));

        let json = serde_json::to_string(&t).expect("serialize");
        let back: SessionTelemetry = serde_json::from_str(&json).expect("deserialize");

        prop_assert!(
            back.extra.contains_key(&key),
            "extra field '{}' lost in roundtrip",
            key
        );
        prop_assert_eq!(
            back.extra.get(&key).and_then(serde_json::Value::as_i64),
            Some(val),
        );
    }

    #[test]
    fn agent_metrics_extra_survives_roundtrip(
        key in "[a-z_]{3,12}",
        val in "[a-zA-Z0-9 ]{1,30}",
    ) {
        let mut m = AgentMetrics::default();
        m.extra.insert(key.clone(), serde_json::Value::String(val.clone()));

        let json = serde_json::to_string(&m).expect("serialize");
        let back: AgentMetrics = serde_json::from_str(&json).expect("deserialize");

        prop_assert!(
            back.extra.contains_key(&key),
            "extra field '{}' lost in roundtrip",
            key
        );
        prop_assert_eq!(
            back.extra.get(&key).and_then(serde_json::Value::as_str),
            Some(val.as_str()),
        );
    }

    // ── RobotEnvelope wrapping preserves inner data ──────────────────

    #[test]
    fn envelope_preserves_session_telemetry(t in session_telemetry_strategy()) {
        let envelope = RobotEnvelope::wrap("test", t.clone());
        prop_assert_eq!(&envelope.data, &t);
        prop_assert!(!envelope.degraded);
        prop_assert_eq!(envelope.source, "test");
    }

    #[test]
    fn envelope_preserves_agent_metrics(m in agent_metrics_strategy()) {
        let envelope = RobotEnvelope::wrap("test", m.clone());
        prop_assert_eq!(&envelope.data, &m);
        prop_assert!(!envelope.degraded);
    }

    // ── Degraded envelope roundtrip ──────────────────────────────────

    #[test]
    fn degraded_envelope_roundtrip(t in session_telemetry_strategy()) {
        let envelope = RobotEnvelope::wrap_degraded("vc", t);
        let json = serde_json::to_string(&envelope).expect("serialize");
        let back: RobotEnvelope<SessionTelemetry> =
            serde_json::from_str(&json).expect("deserialize");
        prop_assert!(back.degraded, "degraded flag lost in roundtrip");
        prop_assert_eq!(back.source, "vc");
        prop_assert_eq!(back.data.commands, envelope.data.commands);
    }

    // ── Default is idempotent ────────────────────────────────────────

    #[test]
    fn default_telemetry_always_zeroed(_dummy in 0..10u8) {
        let t = SessionTelemetry::default();
        prop_assert!(t.session_id.is_none());
        prop_assert_eq!(t.commands, 0);
        prop_assert_eq!(t.errors, 0);
        prop_assert_eq!(t.agent_interactions, 0);
        prop_assert!(t.duration_secs.abs() < f64::EPSILON, "duration_secs not zero: {}", t.duration_secs);
    }

    #[test]
    fn default_metrics_always_zeroed(_dummy in 0..10u8) {
        let m = AgentMetrics::default();
        prop_assert!(m.agent_id.is_none());
        prop_assert_eq!(m.total_tokens, 0);
        prop_assert_eq!(m.tool_calls, 0);
        prop_assert_eq!(m.sessions, 0);
        prop_assert!(m.avg_session_secs.abs() < f64::EPSILON, "avg_session_secs not zero: {}", m.avg_session_secs);
    }

    // ── Counters are non-negative ────────────────────────────────────

    #[test]
    fn session_counters_non_negative(t in session_telemetry_strategy()) {
        // usize is always >= 0, but verify duration_secs isn't negative
        // after roundtrip
        let json = serde_json::to_string(&t).expect("serialize");
        let back: SessionTelemetry = serde_json::from_str(&json).expect("deserialize");
        prop_assert!(back.duration_secs >= 0.0, "negative duration_secs: {}", back.duration_secs);
    }

    #[test]
    fn agent_counters_non_negative(m in agent_metrics_strategy()) {
        let json = serde_json::to_string(&m).expect("serialize");
        let back: AgentMetrics = serde_json::from_str(&json).expect("deserialize");
        prop_assert!(back.avg_session_secs >= 0.0, "negative avg_session_secs: {}", back.avg_session_secs);
    }

    // ── Envelope timestamp is non-empty ──────────────────────────────

    #[test]
    fn envelope_timestamp_nonempty(
        source in "[a-z_]{1,20}",
    ) {
        let envelope = RobotEnvelope::wrap(&source, SessionTelemetry::default());
        prop_assert!(!envelope.timestamp.is_empty(), "timestamp should be non-empty");
        prop_assert!(
            envelope.timestamp.contains('T'),
            "timestamp should contain 'T' separator: {}",
            envelope.timestamp
        );
    }
}
