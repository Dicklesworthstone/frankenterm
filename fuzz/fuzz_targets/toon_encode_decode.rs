#![no_main]
//! Structure-aware TOON encode→decode fixed-point fuzzer.
//!
//! Takes an `Arbitrary`-driven `FuzzPaneState` (a shape-matched mirror of
//! `frankenterm_core::robot_types::PaneStateData`), serializes it through
//! `serde_json::Value`, encodes to TOON, decodes back, and re-encodes.
//!
//! Invariants:
//! - Decoding never panics on TOON emitted by `encode`.
//! - The second encoding equals the first (fixed-point reached after one
//!   roundtrip — any drift indicates non-canonical encoding).
//! - The decoded JSON value is semantically equivalent to the input JSON.
//!
//! We deliberately avoid taking a direct dependency on `frankenterm-core` so
//! this target compiles without the optional `core-fuzz-targets` feature.
//! The mirror struct is kept in lockstep with `PaneStateData`'s serde shape.

use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_STRING_CHARS: usize = 96;

/// Shape-matched mirror of `frankenterm_core::robot_types::PaneStateData`.
/// The serde layout must stay identical — if you add/remove a field here,
/// update it in both places.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FuzzPaneState {
    pane_id: u64,
    #[serde(default)]
    pane_uuid: Option<String>,
    tab_id: u64,
    window_id: u64,
    domain: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    observed: bool,
    #[serde(default)]
    ignore_reason: Option<String>,
}

impl<'a> Arbitrary<'a> for FuzzPaneState {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(Self {
            pane_id: u.arbitrary()?,
            pane_uuid: arbitrary_optional_string(u)?,
            tab_id: u.arbitrary()?,
            window_id: u.arbitrary()?,
            domain: bounded_string(u, "local")?,
            title: arbitrary_optional_string(u)?,
            cwd: arbitrary_optional_string(u)?,
            observed: u.arbitrary()?,
            ignore_reason: arbitrary_optional_string(u)?,
        })
    }
}

fn bounded_string(
    u: &mut Unstructured<'_>,
    fallback: &str,
) -> libfuzzer_sys::arbitrary::Result<String> {
    let raw: String = Arbitrary::arbitrary(u)?;
    let trimmed: String = raw.chars().take(MAX_STRING_CHARS).collect();
    if trimmed.is_empty() {
        Ok(fallback.to_string())
    } else {
        Ok(trimmed)
    }
}

fn arbitrary_optional_string(
    u: &mut Unstructured<'_>,
) -> libfuzzer_sys::arbitrary::Result<Option<String>> {
    if bool::arbitrary(u)? {
        let s: String = Arbitrary::arbitrary(u)?;
        Ok(Some(s.chars().take(MAX_STRING_CHARS).collect()))
    } else {
        Ok(None)
    }
}

fn json_equivalent(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => {
                if x.is_nan() && y.is_nan() {
                    true
                } else {
                    (x - y).abs() <= x.abs().max(y.abs()).max(1.0) * 2.0 * f64::EPSILON
                }
            }
            _ => a.to_string() == b.to_string(),
        },
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(l, r)| json_equivalent(l, r))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(k, v)| {
                    b.get(k)
                        .is_some_and(|other| json_equivalent(v, other))
                })
        }
        _ => false,
    }
}

fuzz_target!(|state: FuzzPaneState| {
    // 1. PaneState → JSON Value.
    let Ok(as_json) = serde_json::to_value(&state) else {
        return;
    };

    // 2. JSON → TOON (first encoding).
    let first_encoded = toon_rust::encode(as_json.clone(), None);

    // 3. TOON → JsonValue → serde_json::Value.
    let decoded = match toon_rust::try_decode(&first_encoded, None) {
        Ok(v) => v,
        Err(err) => {
            panic!(
                "TOON failed to decode its own emission for PaneState:\n{first_encoded}\nerror: {err:?}"
            );
        }
    };
    let roundtrip_json = Value::from(decoded);

    // 4. Semantic equivalence — decoded JSON matches what we encoded.
    assert!(
        json_equivalent(&as_json, &roundtrip_json),
        "toon roundtrip produced non-equivalent JSON:\n  original = {as_json}\n  roundtrip = {roundtrip_json}"
    );

    // 5. Fixed-point check — second encoding equals the first byte-for-byte.
    let second_encoded = toon_rust::encode(roundtrip_json.clone(), None);
    assert_eq!(
        first_encoded, second_encoded,
        "TOON encoding is not a fixed point: first != second after one roundtrip"
    );

    // 6. Deserialize back to FuzzPaneState — serde layout must be stable
    // through TOON.
    let deserialized: FuzzPaneState = match serde_json::from_value(roundtrip_json) {
        Ok(v) => v,
        Err(err) => panic!(
            "TOON roundtrip broke serde deserialization of PaneStateData: {err}"
        ),
    };

    // 7. Field-level spot checks — confirm primitive fields survive.
    assert_eq!(deserialized.pane_id, state.pane_id);
    assert_eq!(deserialized.tab_id, state.tab_id);
    assert_eq!(deserialized.window_id, state.window_id);
    assert_eq!(deserialized.observed, state.observed);
    assert_eq!(deserialized.domain, state.domain);
});
