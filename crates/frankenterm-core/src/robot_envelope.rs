//! Standardized robot envelope for subprocess bridge outputs.
//!
//! This wrapper normalizes machine-readable payloads produced by bridge modules
//! so downstream robot consumers can rely on consistent metadata.

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Standardized JSON envelope for subprocess bridge outputs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RobotEnvelope<T> {
    /// ISO-8601 timestamp of when the data was captured.
    pub timestamp: String,
    /// Source bridge identifier (e.g. "vibe_cockpit").
    pub source: String,
    /// The actual payload.
    pub data: T,
    /// Whether the payload is degraded (fallback/default data path).
    #[serde(default)]
    pub degraded: bool,
}

impl<T> RobotEnvelope<T> {
    /// Wrap payload as a normal (non-degraded) envelope.
    #[must_use]
    pub fn wrap(source: &str, data: T) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            source: source.to_string(),
            data,
            degraded: false,
        }
    }

    /// Wrap payload as degraded/fallback output.
    #[must_use]
    pub fn wrap_degraded(source: &str, data: T) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            source: source.to_string(),
            data,
            degraded: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct DemoPayload {
        id: u32,
        name: String,
    }

    #[test]
    fn test_robot_envelope_wraps_output() {
        let envelope = RobotEnvelope::wrap("demo", 42);
        assert_eq!(envelope.source, "demo");
        assert_eq!(envelope.data, 42);
        assert!(!envelope.degraded);
        assert!(!envelope.timestamp.is_empty());
    }

    #[test]
    fn test_robot_envelope_includes_timestamp() {
        let envelope = RobotEnvelope::wrap("demo", "value");
        assert!(envelope.timestamp.contains('T'));
    }

    #[test]
    fn test_robot_envelope_wrap_degraded_sets_flag() {
        let envelope = RobotEnvelope::wrap_degraded("demo", "fallback");
        assert!(envelope.degraded);
    }

    #[test]
    fn test_robot_envelope_source_preserved() {
        let envelope = RobotEnvelope::wrap("vibe_cockpit", 1usize);
        assert_eq!(envelope.source, "vibe_cockpit");
    }

    #[test]
    fn test_robot_envelope_serde_roundtrip_scalar() {
        let envelope = RobotEnvelope::wrap("src", 99i32);
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
    }

    #[test]
    fn test_robot_envelope_serde_roundtrip_struct() {
        let payload = DemoPayload {
            id: 7,
            name: "alpha".to_string(),
        };
        let envelope = RobotEnvelope::wrap("src", payload.clone());
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<DemoPayload> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data, payload);
        assert!(!back.degraded);
    }

    #[test]
    fn test_robot_envelope_option_payload() {
        let envelope: RobotEnvelope<Option<i32>> = RobotEnvelope::wrap("src", Some(3));
        assert_eq!(envelope.data, Some(3));
    }

    #[test]
    fn test_robot_envelope_bool_payload() {
        let envelope = RobotEnvelope::wrap("src", true);
        assert!(envelope.data);
    }

    #[test]
    fn test_robot_envelope_timestamp_is_parseable_rfc3339() {
        let envelope = RobotEnvelope::wrap("src", ());
        let parsed = chrono::DateTime::parse_from_rfc3339(&envelope.timestamp);
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_robot_envelope_clone() {
        let envelope = RobotEnvelope::wrap("src", 3u8);
        let cloned = envelope.clone();
        assert_eq!(cloned, envelope);
    }

    #[test]
    fn test_robot_envelope_debug_format() {
        let envelope = RobotEnvelope::wrap("src", 5u8);
        let dbg = format!("{envelope:?}");
        assert!(dbg.contains("RobotEnvelope"));
    }

    #[test]
    fn test_robot_envelope_degraded_roundtrip() {
        let envelope = RobotEnvelope::wrap_degraded("src", "fallback");
        let json = serde_json::to_string(&envelope).unwrap();
        let back: RobotEnvelope<String> = serde_json::from_str(&json).unwrap();
        assert!(back.degraded);
        assert_eq!(back.data, "fallback");
    }

    #[test]
    fn test_wrap_non_degraded_flag() {
        let e = RobotEnvelope::wrap("bridge", 0u8);
        assert!(!e.degraded);
    }

    #[test]
    fn test_wrap_degraded_flag() {
        let e = RobotEnvelope::wrap_degraded("bridge", 0u8);
        assert!(e.degraded);
    }

    #[test]
    fn test_serde_degraded_default_false() {
        // JSON without "degraded" field should deserialize as false
        let json = r#"{"timestamp":"2026-01-01T00:00:00+00:00","source":"s","data":42}"#;
        let e: RobotEnvelope<i32> = serde_json::from_str(json).unwrap();
        assert!(!e.degraded);
    }

    #[test]
    fn test_serde_string_payload_roundtrip() {
        let e = RobotEnvelope::wrap("src", "hello world".to_string());
        let json = serde_json::to_string(&e).unwrap();
        let back: RobotEnvelope<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data, "hello world");
        assert_eq!(back.source, "src");
    }

    #[test]
    fn test_serde_vec_payload_roundtrip() {
        let e = RobotEnvelope::wrap("src", vec![1, 2, 3]);
        let json = serde_json::to_string(&e).unwrap();
        let back: RobotEnvelope<Vec<i32>> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data, vec![1, 2, 3]);
    }

    #[test]
    fn test_serde_nested_envelope_roundtrip() {
        let inner = RobotEnvelope::wrap("inner", 42u32);
        let outer = RobotEnvelope::wrap("outer", inner.clone());
        let json = serde_json::to_string(&outer).unwrap();
        let back: RobotEnvelope<RobotEnvelope<u32>> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data.data, 42);
        assert_eq!(back.data.source, "inner");
        assert_eq!(back.source, "outer");
    }

    #[test]
    fn test_json_contains_all_fields() {
        let e = RobotEnvelope::wrap("mybridge", 99i32);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"timestamp\""));
        assert!(json.contains("\"source\""));
        assert!(json.contains("\"data\""));
        assert!(json.contains("\"mybridge\""));
        assert!(json.contains("99"));
    }

    #[test]
    fn test_degraded_json_contains_flag() {
        let e = RobotEnvelope::wrap_degraded("src", 1u8);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"degraded\":true"));
    }

    #[test]
    fn test_wrap_empty_source() {
        let e = RobotEnvelope::wrap("", 42);
        assert_eq!(e.source, "");
    }

    #[test]
    fn test_wrap_unicode_source() {
        let e = RobotEnvelope::wrap("日本語ブリッジ", 1u8);
        assert_eq!(e.source, "日本語ブリッジ");
        let json = serde_json::to_string(&e).unwrap();
        let back: RobotEnvelope<u8> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, "日本語ブリッジ");
    }

    #[test]
    fn test_wrap_none_payload() {
        let e: RobotEnvelope<Option<u32>> = RobotEnvelope::wrap("src", None);
        assert_eq!(e.data, None);
        let json = serde_json::to_string(&e).unwrap();
        let back: RobotEnvelope<Option<u32>> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data, None);
    }

    #[test]
    fn test_equality_same_payload() {
        // Two envelopes with same fields are equal
        let e1 = RobotEnvelope {
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            source: "s".to_string(),
            data: 42,
            degraded: false,
        };
        let e2 = e1.clone();
        assert_eq!(e1, e2);
    }

    #[test]
    fn test_inequality_different_data() {
        let e1 = RobotEnvelope {
            timestamp: "t".to_string(),
            source: "s".to_string(),
            data: 1,
            degraded: false,
        };
        let e2 = RobotEnvelope {
            timestamp: "t".to_string(),
            source: "s".to_string(),
            data: 2,
            degraded: false,
        };
        assert_ne!(e1, e2);
    }

    #[test]
    fn test_inequality_different_degraded() {
        let e1 = RobotEnvelope {
            timestamp: "t".to_string(),
            source: "s".to_string(),
            data: 1,
            degraded: false,
        };
        let e2 = RobotEnvelope {
            timestamp: "t".to_string(),
            source: "s".to_string(),
            data: 1,
            degraded: true,
        };
        assert_ne!(e1, e2);
    }

    #[test]
    fn test_debug_contains_fields() {
        let e = RobotEnvelope::wrap("my_bridge", "payload");
        let dbg = format!("{e:?}");
        assert!(dbg.contains("my_bridge"));
        assert!(dbg.contains("payload"));
    }

    #[test]
    fn test_hashmap_payload() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert("key".to_string(), 42);
        let e = RobotEnvelope::wrap("src", map.clone());
        let json = serde_json::to_string(&e).unwrap();
        let back: RobotEnvelope<HashMap<String, i32>> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data, map);
    }

    #[test]
    fn test_tuple_payload() {
        let e = RobotEnvelope::wrap("src", (1, "two", 3.0));
        let json = serde_json::to_string(&e).unwrap();
        let back: RobotEnvelope<(i32, String, f64)> = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data.0, 1);
        assert_eq!(back.data.1, "two");
        assert!((back.data.2 - 3.0).abs() < f64::EPSILON);
    }
}
