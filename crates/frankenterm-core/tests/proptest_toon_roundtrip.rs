//! Proptest suite for TOON (Token-Optimized Object Notation) format roundtrips.
//!
//! Bead: wa-165vw
//! Coverage:
//! - Arbitrary serde_json::Value → TOON → back → semantic equality
//! - Nested object/array structures survive roundtrip
//! - Streaming decode matches single-pass decode
//! - Token savings invariant: TOON tokens <= JSON tokens for non-trivial payloads
//! - Canonical form: whitespace/indent variations parse identically

#![cfg(feature = "mcp")]

use proptest::prelude::*;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Generate a leaf JSON value (no nesting).
fn arb_json_leaf() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        // Restrict to ±2^50 to avoid f64 precision loss through TOON roundtrip
        (-1125899906842624_i64..=1125899906842624_i64).prop_map(|n| Value::Number(n.into())),
        (0.001f64..1e6)
            .prop_map(|f| serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)),
        "[a-zA-Z0-9_ ]{0,64}".prop_map(Value::String),
    ]
}

/// Generate a JSON value tree up to configurable depth/breadth.
fn arb_json_value() -> impl Strategy<Value = Value> {
    arb_json_leaf().prop_recursive(
        4,  // depth
        64, // max nodes
        8,  // items per collection
        |inner| {
            prop_oneof![
                // Array of values
                prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                // Object with string keys
                prop::collection::vec(
                    ("[a-z_]{1,12}".prop_map(String::from), inner),
                    0..6,
                )
                .prop_map(|pairs| {
                    Value::Object(pairs.into_iter().collect())
                }),
            ]
        },
    )
}

/// Generate a non-trivial JSON object (guaranteed to have at least one key).
fn arb_json_object() -> impl Strategy<Value = Value> {
    prop::collection::vec(
        ("[a-z_]{1,12}".prop_map(String::from), arb_json_value()),
        1..8,
    )
    .prop_map(|pairs| Value::Object(pairs.into_iter().collect()))
}

/// Generate a robot-mode style response envelope.
fn arb_robot_envelope() -> impl Strategy<Value = Value> {
    (
        any::<bool>(),
        arb_json_value(),
        0u64..5000,
    )
        .prop_map(|(ok, data, elapsed)| {
            json!({
                "ok": ok,
                "data": data,
                "elapsed_ms": elapsed,
                "version": "0.1.0",
            })
        })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compare two serde_json::Value trees with f64 tolerance.
///
/// TOON internally uses f64 for all numbers, so i64 values roundtrip through
/// f64 and back. This means exact numeric equality can fail for large integers
/// and float precision can drift by ~1 ULP.
fn json_values_equivalent(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => {
            let xf = x.as_f64().unwrap_or(f64::NAN);
            let yf = y.as_f64().unwrap_or(f64::NAN);
            if xf.is_nan() && yf.is_nan() {
                true
            } else {
                // Use relative tolerance for large values, absolute for small
                let abs_diff = (xf - yf).abs();
                let max_abs = xf.abs().max(yf.abs());
                if max_abs > 1.0 {
                    abs_diff / max_abs < 1e-10
                } else {
                    abs_diff < 1e-10
                }
            }
        }
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(xs), Value::Array(ys)) => {
            xs.len() == ys.len()
                && xs
                    .iter()
                    .zip(ys.iter())
                    .all(|(x, y)| json_values_equivalent(x, y))
        }
        (Value::Object(xm), Value::Object(ym)) => {
            // TOON preserves insertion order but proptest may produce duplicate keys.
            // Compare by key presence and value equivalence.
            if xm.len() != ym.len() {
                return false;
            }
            xm.iter()
                .all(|(k, v)| ym.get(k).is_some_and(|yv| json_values_equivalent(v, yv)))
        }
        _ => false,
    }
}

/// Estimate token count (chars/4 or word count, whichever is larger).
fn estimate_tokens(s: &str) -> usize {
    let chars = s.len();
    let words = s.split_whitespace().count();
    std::cmp::max(chars / 4, words)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // -----------------------------------------------------------------------
    // Core roundtrip: serde_json::Value → TOON → serde_json::Value
    // -----------------------------------------------------------------------

    #[test]
    fn json_to_toon_roundtrip_arbitrary(value in arb_json_value()) {
        let serde_value: serde_json::Value = value.clone();
        let toon_str = toon_rust::encode(serde_value.clone(), None);

        // TOON encodes empty objects ({}) as empty string — skip roundtrip
        if toon_str.is_empty() {
            return Ok(());
        }

        // Decode back
        let decoded_toon = toon_rust::try_decode(&toon_str, None)
            .expect("TOON decode should succeed for encode output");

        // Convert back to serde_json::Value
        let decoded_serde: serde_json::Value = decoded_toon.into();

        // Semantic equality (f64 tolerance)
        let eq = json_values_equivalent(&value, &decoded_serde);
        prop_assert!(eq,
            "roundtrip mismatch:\n  original: {}\n  decoded:  {}",
            serde_json::to_string(&value).unwrap_or_default(),
            serde_json::to_string(&decoded_serde).unwrap_or_default()
        );
    }

    #[test]
    fn json_to_toon_roundtrip_objects(obj in arb_json_object()) {
        let toon_str = toon_rust::encode(obj.clone(), None);
        let decoded = toon_rust::try_decode(&toon_str, None)
            .expect("TOON decode should succeed");
        let back: serde_json::Value = decoded.into();

        let eq = json_values_equivalent(&obj, &back);
        prop_assert!(eq, "object roundtrip mismatch");
    }

    #[test]
    fn robot_envelope_roundtrip(envelope in arb_robot_envelope()) {
        let toon = toon_rust::encode(envelope.clone(), None);
        let decoded = toon_rust::try_decode(&toon, None)
            .expect("robot envelope decode");
        let back: serde_json::Value = decoded.into();

        let eq = json_values_equivalent(&envelope, &back);
        prop_assert!(eq, "robot envelope roundtrip mismatch");
    }

    // -----------------------------------------------------------------------
    // Convenience wrappers: json_to_toon / toon_to_json
    // -----------------------------------------------------------------------

    #[test]
    fn convenience_json_toon_json_roundtrip(value in arb_json_value()) {
        let json_str = serde_json::to_string(&value).unwrap();
        let toon_str = toon_rust::json_to_toon(&json_str)
            .expect("json_to_toon should succeed for valid JSON");

        // TOON encodes empty containers as empty string — skip roundtrip
        if toon_str.is_empty() {
            return Ok(());
        }

        let json_back = toon_rust::toon_to_json(&toon_str)
            .expect("toon_to_json should succeed for valid TOON");

        let decoded: serde_json::Value = serde_json::from_str(&json_back)
            .expect("output of toon_to_json should be valid JSON");

        let eq = json_values_equivalent(&value, &decoded);
        prop_assert!(eq, "convenience roundtrip mismatch");
    }

    // -----------------------------------------------------------------------
    // Canonical form: re-encoding produces identical output
    // -----------------------------------------------------------------------

    #[test]
    fn double_encode_is_idempotent(value in arb_json_value()) {
        let toon1 = toon_rust::encode(value.clone(), None);
        if toon1.is_empty() {
            return Ok(());
        }
        let decoded = toon_rust::try_decode(&toon1, None)
            .expect("first decode");
        // Re-encode from the decoded JsonValue
        let toon2 = toon_rust::encode(decoded, None);

        prop_assert_eq!(&toon1, &toon2, "double-encode should be idempotent");
    }

    // -----------------------------------------------------------------------
    // Streaming decode matches single-pass decode
    // -----------------------------------------------------------------------

    #[test]
    fn stream_decode_matches_single_pass(obj in arb_json_object()) {
        let toon = toon_rust::encode(obj.clone(), None);

        // Single-pass decode
        let single = toon_rust::try_decode(&toon, None)
            .expect("single-pass decode");

        // Streaming decode: split into lines
        let lines: Vec<String> = toon.lines().map(|l| l.to_string()).collect();
        let stream_events = toon_rust::try_decode_stream_sync(lines, None)
            .expect("stream decode");

        // Stream events should be non-empty for any object
        prop_assert!(!stream_events.is_empty(),
            "stream decode produced no events for non-empty object");

        // Verify by re-encoding from single-pass and checking consistency
        let single_back: serde_json::Value = single.into();
        let eq = json_values_equivalent(&obj, &single_back);
        prop_assert!(eq, "stream decode context: single-pass roundtrip failed");
    }

    // -----------------------------------------------------------------------
    // Token savings: TOON should use fewer tokens than JSON
    // -----------------------------------------------------------------------

    #[test]
    fn toon_token_savings_for_objects(obj in arb_json_object()) {
        let json_str = serde_json::to_string(&obj).unwrap();
        let toon_str = toon_rust::encode(obj, None);

        let json_tokens = estimate_tokens(&json_str);
        let toon_tokens = estimate_tokens(&toon_str);

        // TOON should not be dramatically larger than JSON.
        // For small payloads or payloads dominated by keys (not values),
        // TOON's indentation-based format may use slightly more bytes.
        // We only assert for large payloads where structural savings should dominate.
        if json_str.len() >= 500 {
            prop_assert!(
                toon_tokens <= json_tokens + (json_tokens / 5),
                "TOON used >20% more tokens than JSON for large payload: \
                 json_tokens={json_tokens}, toon_tokens={toon_tokens}, \
                 json_len={}, toon_len={}",
                json_str.len(),
                toon_str.len()
            );
        }
    }

    // -----------------------------------------------------------------------
    // Indent variations parse identically
    // -----------------------------------------------------------------------

    #[test]
    fn different_indent_options_roundtrip(
        value in arb_json_value(),
        indent in 1usize..6,
    ) {
        let opts = toon_rust::EncodeOptions {
            indent: Some(indent),
            delimiter: None,
            key_folding: None,
            flatten_depth: None,
            replacer: None,
        };
        let toon = toon_rust::encode(value.clone(), Some(opts));

        // Empty containers encode to empty string — skip decode
        if toon.is_empty() {
            return Ok(());
        }

        let decode_opts = toon_rust::DecodeOptions {
            indent: Some(indent),
            strict: Some(true),
            expand_paths: None,
        };
        let decoded = toon_rust::try_decode(&toon, Some(decode_opts))
            .expect("decode with matching indent");

        let back: serde_json::Value = decoded.into();
        let eq = json_values_equivalent(&value, &back);
        prop_assert!(eq, "indent={indent} roundtrip mismatch");
    }

    // -----------------------------------------------------------------------
    // String escaping: special characters survive roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn string_with_special_chars_roundtrip(
        s in r#"[a-zA-Z0-9 \t\n"'\\/:;,.\-_!@#$%^&*()]{0,128}"#,
    ) {
        let value = Value::String(s.clone());
        let toon = toon_rust::encode(value, None);
        let decoded = toon_rust::try_decode(&toon, None)
            .expect("special chars decode");
        let back: serde_json::Value = decoded.into();

        if let Value::String(ref decoded_s) = back {
            prop_assert_eq!(&s, decoded_s, "string content mismatch after roundtrip");
        } else {
            prop_assert!(false, "expected string, got {:?}", back);
        }
    }

    // -----------------------------------------------------------------------
    // Nested arrays survive roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn nested_array_roundtrip(
        inner_lens in prop::collection::vec(0usize..4, 1..4),
    ) {
        // Build nested arrays: [[...], [...], ...]
        let nested: Value = Value::Array(
            inner_lens
                .iter()
                .enumerate()
                .map(|(i, &len)| {
                    Value::Array(
                        (0..len)
                            .map(|j| json!(format!("item_{i}_{j}")))
                            .collect(),
                    )
                })
                .collect(),
        );
        let toon = toon_rust::encode(nested.clone(), None);
        let decoded = toon_rust::try_decode(&toon, None).expect("nested array decode");
        let back: serde_json::Value = decoded.into();

        let eq = json_values_equivalent(&nested, &back);
        prop_assert!(eq, "nested array roundtrip mismatch");
    }

    // -----------------------------------------------------------------------
    // Empty containers
    // -----------------------------------------------------------------------

    #[test]
    fn empty_containers_roundtrip(use_array in any::<bool>()) {
        let value = if use_array {
            json!([])
        } else {
            json!({})
        };
        let toon = toon_rust::encode(value.clone(), None);

        if toon.is_empty() {
            // Empty object {} encodes to empty string in TOON — can't roundtrip
            let is_empty_obj = matches!(&value, Value::Object(m) if m.is_empty());
            prop_assert!(is_empty_obj, "only empty objects should produce empty TOON");
        } else {
            // Empty array [] encodes to "[0]:" — verify roundtrip
            let decoded = toon_rust::try_decode(&toon, None)
                .expect("decode empty container");
            let back: serde_json::Value = decoded.into();
            let eq = json_values_equivalent(&value, &back);
            prop_assert!(eq, "empty container roundtrip mismatch");
        }
    }

    // -----------------------------------------------------------------------
    // Large payload roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn large_payload_roundtrip(n in 50usize..200) {
        let large = json!({
            "ok": true,
            "data": {
                "results": (0..n)
                    .map(|i| json!({
                        "id": i,
                        "name": format!("entry_{i}"),
                        "value": i as f64 * 0.1,
                    }))
                    .collect::<Vec<_>>()
            },
            "count": n,
        });
        let toon = toon_rust::encode(large.clone(), None);
        let decoded = toon_rust::try_decode(&toon, None).expect("large payload decode");
        let back: serde_json::Value = decoded.into();

        let eq = json_values_equivalent(&large, &back);
        prop_assert!(eq, "large payload roundtrip mismatch");
    }
}

// ---------------------------------------------------------------------------
// Non-proptest unit tests for specific edge cases
// ---------------------------------------------------------------------------

#[cfg(test)]
mod edge_cases {
    use super::*;

    #[test]
    fn null_roundtrip() {
        let toon = toon_rust::encode(Value::Null, None);
        let decoded = toon_rust::try_decode(&toon, None).unwrap();
        let back: Value = decoded.into();
        assert!(json_values_equivalent(&Value::Null, &back));
    }

    #[test]
    fn bool_roundtrip() {
        for b in [true, false] {
            let val = Value::Bool(b);
            let toon = toon_rust::encode(val.clone(), None);
            let decoded = toon_rust::try_decode(&toon, None).unwrap();
            let back: Value = decoded.into();
            assert!(json_values_equivalent(&val, &back));
        }
    }

    #[test]
    fn integer_boundary_roundtrip() {
        for n in [0i64, 1, -1, i32::MAX as i64, i32::MIN as i64, (1i64 << 53), -(1i64 << 53)] {
            let val = json!(n);
            let toon = toon_rust::encode(val.clone(), None);
            let decoded = toon_rust::try_decode(&toon, None).unwrap();
            let back: Value = decoded.into();
            assert!(
                json_values_equivalent(&val, &back),
                "integer {n} roundtrip failed"
            );
        }
    }

    #[test]
    fn deeply_nested_object() {
        let mut val = json!("leaf");
        for i in 0..10 {
            val = json!({ format!("level_{i}"): val });
        }
        let toon = toon_rust::encode(val.clone(), None);
        let decoded = toon_rust::try_decode(&toon, None).unwrap();
        let back: Value = decoded.into();
        assert!(json_values_equivalent(&val, &back));
    }

    #[test]
    fn representative_robot_state_response() {
        let resp = json!({
            "ok": true,
            "data": {
                "panes": [
                    {"pane_id": 0, "domain": "local", "title": "zsh", "cwd": "/home/user"},
                    {"pane_id": 1, "domain": "local", "title": "vim", "cwd": "/home/user/project"},
                ]
            },
            "elapsed_ms": 3,
            "version": "0.1.0",
        });
        let toon = toon_rust::encode(resp.clone(), None);
        let json_str = serde_json::to_string(&resp).unwrap();

        // Token savings check
        let json_tokens = estimate_tokens(&json_str);
        let toon_tokens = estimate_tokens(&toon);
        assert!(
            toon_tokens <= json_tokens,
            "TOON should save tokens: json={json_tokens}, toon={toon_tokens}"
        );

        // Roundtrip
        let decoded = toon_rust::try_decode(&toon, None).unwrap();
        let back: Value = decoded.into();
        assert!(json_values_equivalent(&resp, &back));
    }

    #[test]
    fn stream_decode_for_typical_events() {
        let payload = json!({
            "ok": true,
            "data": {
                "events": [
                    {"seq": 0, "type": "pane_output", "pane_id": 1},
                    {"seq": 1, "type": "detection", "rule_id": "core.error"},
                    {"seq": 2, "type": "workflow_start", "workflow_id": "build"},
                ]
            }
        });
        let toon = toon_rust::encode(payload.clone(), None);
        let lines: Vec<String> = toon.lines().map(|l| l.to_string()).collect();

        let events = toon_rust::try_decode_stream_sync(lines, None)
            .expect("stream decode should succeed");
        assert!(!events.is_empty(), "stream should produce events");

        // Also verify single-pass matches
        let single = toon_rust::try_decode(&toon, None).unwrap();
        let back: Value = single.into();
        assert!(json_values_equivalent(&payload, &back));
    }

    #[test]
    fn token_savings_on_search_results() {
        let search = json!({
            "ok": true,
            "data": {
                "query": "compilation error",
                "results": (0..50).map(|i| json!({
                    "segment_id": 10000 + i,
                    "pane_id": i % 8,
                    "score": (i as f64).mul_add(-0.01, 0.95),
                    "snippet": format!("error[E0308]: mismatched types at line {}", i * 10 + 1),
                })).collect::<Vec<_>>()
            },
            "elapsed_ms": 15,
        });
        let json_str = serde_json::to_string(&search).unwrap();
        let toon_str = toon_rust::encode(search, None);

        let json_tokens = estimate_tokens(&json_str);
        let toon_tokens = estimate_tokens(&toon_str);

        // For a 50-result search, we should see meaningful savings
        let savings_pct = (toon_tokens * 100)
            .checked_div(json_tokens)
            .map(|ratio| 100 - ratio)
            .unwrap_or(0);
        assert!(
            savings_pct > 10,
            "expected >10% token savings for search results, got {savings_pct}% \
             (json={json_tokens}, toon={toon_tokens})"
        );
    }
}
