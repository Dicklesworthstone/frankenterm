//! Standardized robot envelope for subprocess bridge outputs.
//!
//! This wrapper normalizes machine-readable payloads produced by bridge modules
//! so downstream robot consumers can rely on consistent metadata.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Normalize a JSON value for deterministic golden-file comparison.
///
/// The transformation is idempotent and applied recursively:
/// - Any object field named `timestamp` whose value is a string is replaced
///   with the constant `"[TIMESTAMP]"` so wall-clock drift does not cause
///   spurious diffs.
/// - Arrays whose parent key is `agents`, `tags`, or `pane_ids` are sorted
///   by their serialized JSON text — these are treated as set-valued
///   fields where order is not semantically meaningful.
/// - Non-integer JSON numbers are rounded to 4 decimal places to absorb
///   floating-point noise across platforms and libc versions. Integers
///   pass through unchanged.
/// - Every other value kind is passed through as-is.
pub fn canonicalize_json(value: &mut Value, parent_key: Option<&str>) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let child = map.get_mut(&key).expect("key exists");
                if key == "timestamp" && child.is_string() {
                    *child = Value::String("[TIMESTAMP]".to_string());
                } else {
                    canonicalize_json(child, Some(&key));
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                canonicalize_json(item, parent_key);
            }
            if matches!(parent_key, Some("agents" | "tags" | "pane_ids")) {
                items.sort_by(|left, right| {
                    serde_json::to_string(left)
                        .expect("serialize left")
                        .cmp(&serde_json::to_string(right).expect("serialize right"))
                });
            }
        }
        Value::Number(number) => {
            if let Some(float) = number.as_f64() {
                // [ft-m9gob] Guard against overflow in the rounding step.
                // `float * 10_000.0` can produce f64::INFINITY for values
                // above ~1.8e304 (still a valid finite f64, still valid
                // JSON). `INFINITY.round() / 10_000 = INFINITY`, and
                // `serde_json::Number::from_f64(INFINITY) = None` —
                // the previous `.expect("rounded float")` then panicked
                // the caller. Skip canonicalization when either the
                // input or the rounded product is non-finite; the
                // caller's original Number stays in place (no rounding
                // applied, but no panic either). Non-finite input is
                // also impossible via the public serde_json API —
                // `Number::from_f64(NaN)` returns None — but the check
                // is cheap defense-in-depth for any future construction
                // path.
                if !float.is_finite() {
                    return;
                }
                if float.fract() != 0.0 {
                    let rounded = (float * 10_000.0).round() / 10_000.0;
                    if let Some(num) = serde_json::Number::from_f64(rounded) {
                        *value = Value::Number(num);
                    }
                    // else: rounded overflowed to ±Infinity (input was
                    // above ~1.8e304). Preserve the caller's original
                    // finite number — a golden-file diff on the
                    // untrimmed precision is a lesser evil than a
                    // panicked test.
                }
            }
        }
        _ => {}
    }
}

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
    use serde_json::{Value, json};
    use std::fs;
    use std::path::PathBuf;

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct DemoPayload {
        id: u32,
        name: String,
    }

    fn golden_fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden_robot_envelope")
            .join(format!("{name}.json"))
    }

    fn assert_golden_json(name: &str, envelope: &RobotEnvelope<Value>) {
        let mut actual = serde_json::to_value(envelope).expect("serialize envelope");
        canonicalize_json(&mut actual, None);
        let actual_text = format!(
            "{}\n",
            serde_json::to_string_pretty(&actual).expect("format envelope")
        );
        let path = golden_fixture_path(name);

        if std::env::var_os("UPDATE_GOLDENS").is_some() {
            fs::create_dir_all(path.parent().expect("fixture dir")).expect("create fixture dir");
            fs::write(&path, &actual_text).expect("write golden");
        }

        let expected = fs::read_to_string(&path).expect("read golden");
        assert_eq!(
            actual_text,
            expected,
            "golden mismatch for {}",
            path.display()
        );
    }

    // ── [ft-m9gob] canonicalize_json huge-f64 panic guard ──────────────

    /// [ft-m9gob] A non-integer f64 above ~1.8e304 overflows the
    /// `float * 10_000.0` rounding step to Infinity, which
    /// `Number::from_f64` cannot represent. Pre-fix, the unconditional
    /// `.expect("rounded float")` panicked. Post-fix, the huge
    /// original Number is preserved (no rounding applied) and the
    /// call returns cleanly.
    #[test]
    fn ft_m9gob_canonicalize_json_preserves_huge_finite_float_without_panicking() {
        let mut value = json!({"metric": 1.5e305});
        canonicalize_json(&mut value, None);
        // Original finite float must be preserved — no panic, no
        // silent mutation to Infinity.
        let metric = value.get("metric").and_then(Value::as_f64).unwrap();
        assert!(metric.is_finite(), "metric must stay finite: {metric}");
        assert_eq!(metric, 1.5e305);
    }

    /// [ft-m9gob] Values that DO round cleanly still get canonicalized.
    /// The fix must only skip the rounding when the overflow occurs,
    /// not for every non-integer input.
    #[test]
    fn ft_m9gob_canonicalize_json_still_rounds_small_floats() {
        let mut value = json!({"metric": 1.234_567_8});
        canonicalize_json(&mut value, None);
        let metric = value.get("metric").and_then(Value::as_f64).unwrap();
        // Rounded to 4 decimal places: 1.2346.
        assert!(
            (metric - 1.2346).abs() < 1e-9,
            "small float must still be rounded to 4dp, got {metric}"
        );
    }

    /// [ft-m9gob] Integer-valued f64 passes through unchanged
    /// (fract() == 0.0 branch) — guards against the fix accidentally
    /// short-circuiting integer values too.
    #[test]
    fn ft_m9gob_canonicalize_json_integers_unchanged() {
        let mut value = json!({"count": 1_000_000.0});
        canonicalize_json(&mut value, None);
        let count = value.get("count").and_then(Value::as_f64).unwrap();
        assert_eq!(count, 1_000_000.0);
    }

    /// [ft-m9gob] Negative huge float — same overflow shape on the
    /// absolute value side. `-1.5e305 * 10_000 = -inf`, same failure
    /// mode pre-fix.
    #[test]
    fn ft_m9gob_canonicalize_json_negative_huge_float_is_safe() {
        let mut value = json!({"metric": -1.5e305});
        canonicalize_json(&mut value, None);
        let metric = value.get("metric").and_then(Value::as_f64).unwrap();
        assert!(
            metric.is_finite(),
            "negative huge must stay finite: {metric}"
        );
        assert_eq!(metric, -1.5e305);
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

    #[test]
    fn golden_robot_envelope_scalar_payload_json() {
        let envelope = RobotEnvelope {
            timestamp: "2026-04-20T12:34:56Z".to_string(),
            source: "bridge.scalar".to_string(),
            data: json!(42),
            degraded: false,
        };
        assert_golden_json("scalar_payload", &envelope);
    }

    #[test]
    fn golden_robot_envelope_degraded_payload_json() {
        let envelope = RobotEnvelope {
            timestamp: "2026-04-20T12:35:01Z".to_string(),
            source: "bridge.degraded".to_string(),
            data: json!({"reason": "fallback", "attempts": 3}),
            degraded: true,
        };
        assert_golden_json("degraded_payload", &envelope);
    }

    #[test]
    fn golden_robot_envelope_float_payload_json() {
        let envelope = RobotEnvelope {
            timestamp: "2026-04-20T12:35:15Z".to_string(),
            source: "bridge.float".to_string(),
            data: json!({
                "score": 0.3333333333,
                "ratio": 1.23456789,
                "nested": { "latency_ms": 12.987654321 }
            }),
            degraded: false,
        };
        assert_golden_json("float_payload", &envelope);
    }

    #[test]
    fn golden_robot_envelope_sorted_set_payload_json() {
        let envelope = RobotEnvelope {
            timestamp: "2026-04-20T12:35:30Z".to_string(),
            source: "bridge.sorted".to_string(),
            data: json!({
                "agents": ["agent-c", "agent-a", "agent-b"],
                "tags": ["zeta", "alpha", "mu"],
            }),
            degraded: false,
        };
        assert_golden_json("sorted_set_payload", &envelope);
    }

    #[test]
    fn golden_robot_envelope_nested_timestamp_payload_json() {
        let inner = RobotEnvelope {
            timestamp: "2026-04-20T12:36:01Z".to_string(),
            source: "bridge.inner".to_string(),
            data: json!({
                "pane_ids": [9, 3, 5],
                "status": "ok"
            }),
            degraded: true,
        };
        let envelope = RobotEnvelope {
            timestamp: "2026-04-20T12:36:05Z".to_string(),
            source: "bridge.outer".to_string(),
            data: serde_json::to_value(inner).expect("serialize nested envelope"),
            degraded: false,
        };
        assert_golden_json("nested_timestamp_payload", &envelope);
    }

    #[test]
    fn golden_robot_envelope_structured_payload_json() {
        let envelope = RobotEnvelope {
            timestamp: "2026-04-20T12:36:20Z".to_string(),
            source: "bridge.structured".to_string(),
            data: json!({
                "pane": {
                    "id": 7,
                    "title": "main",
                    "cwd": "/tmp/demo"
                },
                "metrics": {
                    "cpu": 0.12555555,
                    "memory_mb": 64.4444444
                }
            }),
            degraded: false,
        };
        assert_golden_json("structured_payload", &envelope);
    }
}
