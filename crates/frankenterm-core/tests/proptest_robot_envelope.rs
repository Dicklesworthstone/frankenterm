// Requires the `vc-export` feature flag.
#![cfg(feature = "vc-export")]
#![allow(clippy::no_effect_underscore_binding)]
//! Property-based tests for robot_envelope::RobotEnvelope<T>.
//!
//! Validates:
//! - Serde roundtrip with various payload types
//! - wrap vs wrap_degraded flag semantics
//! - degraded default (false when omitted from JSON)
//! - Timestamp is RFC-3339 parseable
//! - Structural equality and clone consistency

use frankenterm_core::robot_envelope::RobotEnvelope;
use proptest::prelude::*;
use std::collections::HashMap;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_source() -> impl Strategy<Value = String> {
    "[a-z_]{0,20}".prop_map(String::from)
}

fn arb_timestamp() -> impl Strategy<Value = String> {
    // Generate valid ISO-8601 timestamps
    (
        2020..2030u32,
        1..13u32,
        1..29u32,
        0..24u32,
        0..60u32,
        0..60u32,
    )
        .prop_map(|(y, m, d, h, min, s)| {
            format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}+00:00")
        })
}

fn arb_envelope_i32() -> impl Strategy<Value = RobotEnvelope<i32>> {
    (arb_timestamp(), arb_source(), any::<i32>(), any::<bool>()).prop_map(
        |(timestamp, source, data, degraded)| RobotEnvelope {
            timestamp,
            source,
            data,
            degraded,
        },
    )
}

fn arb_envelope_string() -> impl Strategy<Value = RobotEnvelope<String>> {
    (
        arb_timestamp(),
        arb_source(),
        "[a-zA-Z0-9 ]{0,50}",
        any::<bool>(),
    )
        .prop_map(|(timestamp, source, data, degraded)| RobotEnvelope {
            timestamp,
            source,
            data,
            degraded,
        })
}

fn arb_envelope_vec_u8() -> impl Strategy<Value = RobotEnvelope<Vec<u8>>> {
    (
        arb_timestamp(),
        arb_source(),
        prop::collection::vec(any::<u8>(), 0..32),
        any::<bool>(),
    )
        .prop_map(|(timestamp, source, data, degraded)| RobotEnvelope {
            timestamp,
            source,
            data,
            degraded,
        })
}

fn arb_envelope_option_u32() -> impl Strategy<Value = RobotEnvelope<Option<u32>>> {
    (
        arb_timestamp(),
        arb_source(),
        proptest::option::of(any::<u32>()),
        any::<bool>(),
    )
        .prop_map(|(timestamp, source, data, degraded)| RobotEnvelope {
            timestamp,
            source,
            data,
            degraded,
        })
}

fn arb_envelope_hashmap() -> impl Strategy<Value = RobotEnvelope<HashMap<String, i32>>> {
    (
        arb_timestamp(),
        arb_source(),
        prop::collection::hash_map("[a-z]{1,5}", any::<i32>(), 0..5),
        any::<bool>(),
    )
        .prop_map(|(timestamp, source, data, degraded)| RobotEnvelope {
            timestamp,
            source,
            data,
            degraded,
        })
}

// ── Serde roundtrip properties ──────────────────────────────────────

proptest! {
    /// RobotEnvelope<i32> serde roundtrip.
    #[test]
    fn envelope_i32_serde_roundtrip(envelope in arb_envelope_i32()) {
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<i32> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&envelope, &back);
    }

    /// RobotEnvelope<String> serde roundtrip.
    #[test]
    fn envelope_string_serde_roundtrip(envelope in arb_envelope_string()) {
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<String> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&envelope, &back);
    }

    /// RobotEnvelope<Vec<u8>> serde roundtrip.
    #[test]
    fn envelope_vec_u8_serde_roundtrip(envelope in arb_envelope_vec_u8()) {
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<Vec<u8>> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&envelope, &back);
    }

    /// RobotEnvelope<Option<u32>> serde roundtrip.
    #[test]
    fn envelope_option_u32_serde_roundtrip(envelope in arb_envelope_option_u32()) {
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<Option<u32>> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&envelope, &back);
    }

    /// RobotEnvelope<HashMap<String, i32>> serde roundtrip.
    #[test]
    fn envelope_hashmap_serde_roundtrip(envelope in arb_envelope_hashmap()) {
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<HashMap<String, i32>> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&envelope, &back);
    }

    /// Value-based roundtrip.
    #[test]
    fn envelope_i32_value_roundtrip(envelope in arb_envelope_i32()) {
        let value = serde_json::to_value(&envelope).unwrap();
        let back: RobotEnvelope<i32> = serde_json::from_value(value).unwrap();
        prop_assert_eq!(&envelope, &back);
    }
}

// ── wrap / wrap_degraded properties ─────────────────────────────────

proptest! {
    /// wrap() sets degraded=false.
    #[test]
    fn wrap_sets_degraded_false(source in arb_source(), data in any::<i32>()) {
        let envelope = RobotEnvelope::wrap(&source, data);
        prop_assert!(!envelope.degraded);
        prop_assert_eq!(envelope.source, source);
        prop_assert_eq!(envelope.data, data);
    }

    /// wrap_degraded() sets degraded=true.
    #[test]
    fn wrap_degraded_sets_flag(source in arb_source(), data in any::<i32>()) {
        let envelope = RobotEnvelope::wrap_degraded(&source, data);
        prop_assert!(envelope.degraded);
        prop_assert_eq!(envelope.source, source);
        prop_assert_eq!(envelope.data, data);
    }

    /// wrap() produces parseable RFC-3339 timestamp.
    #[test]
    fn wrap_timestamp_is_rfc3339(source in arb_source()) {
        let envelope = RobotEnvelope::wrap(&source, 42u8);
        let parsed = chrono::DateTime::parse_from_rfc3339(&envelope.timestamp);
        prop_assert!(
            parsed.is_ok(),
            "timestamp should be RFC-3339: {}", envelope.timestamp
        );
    }

    /// wrap_degraded() produces parseable RFC-3339 timestamp.
    #[test]
    fn wrap_degraded_timestamp_is_rfc3339(source in arb_source()) {
        let envelope = RobotEnvelope::wrap_degraded(&source, 42u8);
        let parsed = chrono::DateTime::parse_from_rfc3339(&envelope.timestamp);
        prop_assert!(
            parsed.is_ok(),
            "timestamp should be RFC-3339: {}", envelope.timestamp
        );
    }

    /// wrap() and wrap_degraded() produce different degraded flags.
    #[test]
    fn wrap_vs_degraded_differ(source in arb_source(), data in any::<u32>()) {
        let normal = RobotEnvelope::wrap(&source, data);
        let degraded = RobotEnvelope::wrap_degraded(&source, data);
        prop_assert!(!normal.degraded);
        prop_assert!(degraded.degraded);
    }
}

// ── Structural properties ───────────────────────────────────────────

proptest! {
    /// Clone produces equal envelope.
    #[test]
    fn envelope_clone_eq(envelope in arb_envelope_i32()) {
        let cloned = envelope.clone();
        prop_assert_eq!(&envelope, &cloned);
    }

    /// Debug format is non-empty and contains "RobotEnvelope".
    #[test]
    fn envelope_debug_format(envelope in arb_envelope_i32()) {
        let dbg = format!("{:?}", envelope);
        prop_assert!(!dbg.is_empty());
        prop_assert!(dbg.contains("RobotEnvelope"));
    }

    /// JSON output contains all expected fields.
    #[test]
    fn json_contains_all_fields(envelope in arb_envelope_i32()) {
        let json = serde_json::to_string(&envelope).unwrap();
        prop_assert!(json.contains("\"timestamp\""));
        prop_assert!(json.contains("\"source\""));
        prop_assert!(json.contains("\"data\""));
        prop_assert!(json.contains("\"degraded\""));
    }

    /// Source field is preserved exactly in serde.
    #[test]
    fn source_preserved(source in arb_source()) {
        let envelope = RobotEnvelope {
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            source: source.clone(),
            data: 0u8,
            degraded: false,
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<u8> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.source, source);
    }

    /// Degraded flag preserved in serde.
    #[test]
    fn degraded_flag_preserved(degraded in any::<bool>()) {
        let envelope = RobotEnvelope {
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            source: "test".to_string(),
            data: 42i32,
            degraded,
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<i32> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.degraded, degraded);
    }
}

// ── Default behavior properties ─────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Missing degraded field in JSON defaults to false.
    #[test]
    fn degraded_default_false(data in any::<i32>()) {
        let json = format!(
            r#"{{"timestamp":"2026-01-01T00:00:00+00:00","source":"s","data":{data}}}"#
        );
        let envelope: RobotEnvelope<i32> = serde_json::from_str(&json).unwrap();
        prop_assert!(!envelope.degraded);
    }

    /// Nested envelope roundtrip.
    #[test]
    fn nested_envelope_roundtrip(inner_data in any::<u32>()) {
        let inner = RobotEnvelope {
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            source: "inner".to_string(),
            data: inner_data,
            degraded: false,
        };
        let outer = RobotEnvelope {
            timestamp: "2026-01-02T00:00:00+00:00".to_string(),
            source: "outer".to_string(),
            data: inner.clone(),
            degraded: true,
        };
        let json = serde_json::to_string(&outer).unwrap();
        let back: RobotEnvelope<RobotEnvelope<u32>> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.data.data, inner_data);
        prop_assert_eq!(back.source, "outer");
        prop_assert_eq!(back.data.source, "inner");
        prop_assert!(back.degraded);
        prop_assert!(!back.data.degraded);
    }

    /// Two envelopes with different data are not equal.
    #[test]
    fn different_data_not_equal(a in any::<i32>(), b in any::<i32>()) {
        prop_assume!(a != b);
        let e1 = RobotEnvelope {
            timestamp: "t".to_string(),
            source: "s".to_string(),
            data: a,
            degraded: false,
        };
        let e2 = RobotEnvelope {
            timestamp: "t".to_string(),
            source: "s".to_string(),
            data: b,
            degraded: false,
        };
        prop_assert_ne!(&e1, &e2);
    }

    /// Two envelopes with different degraded are not equal.
    #[test]
    fn different_degraded_not_equal(_dummy in 0..1u8) {
        let e1 = RobotEnvelope {
            timestamp: "t".to_string(),
            source: "s".to_string(),
            data: 1i32,
            degraded: false,
        };
        let e2 = RobotEnvelope {
            timestamp: "t".to_string(),
            source: "s".to_string(),
            data: 1i32,
            degraded: true,
        };
        prop_assert_ne!(&e1, &e2);
    }
}
