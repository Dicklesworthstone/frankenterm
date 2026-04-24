#![no_main]
//! TOON parser robustness fuzzer.
//!
//! Complements the existing `toon_decode` target (which exercises TOON
//! emitted from arbitrary JSON) by feeding **raw bytes** — including
//! adversarial prefixes that exercise deep nesting and large inputs — to
//! `toon_rust::try_decode` and the line/stream variants.
//!
//! Invariants:
//! - No panic, no stack overflow, no OOM on any input up to `MAX_INPUT_BYTES`.
//!   Inputs larger than the cap are rejected cleanly by this target (libfuzzer
//!   still surfaces crashes on the underlying code if any).
//! - When `try_decode` succeeds, re-encoding the decoded tree produces a
//!   string that re-decodes to a value equivalent to the first decode
//!   (semantic round-trip).
//! - `try_decode`, `try_decode_from_lines`, and `try_decode_stream_sync`
//!   agree on success/failure for the same input.
//! - Depth guard: synthetically nested inputs (beyond 64 levels) must not
//!   crash — they either decode cleanly or return an error.

use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use serde_json::Value;
use toon_rust::{JsonValue, try_decode, try_decode_from_lines, try_decode_stream_sync};

const MAX_INPUT_BYTES: usize = 1024 * 1024; // 1 MiB hard cap.
const MAX_SYNTH_DEPTH: usize = 96;

#[derive(Debug)]
enum InputShape {
    Raw(Vec<u8>),
    DeepArray(usize),
    DeepObject(usize),
    DeepMixed(usize),
}

impl<'a> Arbitrary<'a> for InputShape {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=9u8)? {
            0 => {
                // Synthesize an array nested `depth` deep — exceeds the
                // intended 64-level threshold roughly half the time.
                let depth = u.int_in_range(1..=MAX_SYNTH_DEPTH)?;
                Self::DeepArray(depth)
            }
            1 => {
                let depth = u.int_in_range(1..=MAX_SYNTH_DEPTH)?;
                Self::DeepObject(depth)
            }
            2 => {
                let depth = u.int_in_range(1..=MAX_SYNTH_DEPTH)?;
                Self::DeepMixed(depth)
            }
            _ => {
                let len = u.int_in_range(0..=MAX_INPUT_BYTES)?;
                Self::Raw(u.bytes(len)?.to_vec())
            }
        })
    }
}

fn synth_deep_array(depth: usize) -> String {
    // TOON encoding of a nested-array structure. `encode` on the equivalent
    // serde_json::Value is the safer route, because it always emits a
    // syntactically valid string.
    let mut v = Value::Null;
    for _ in 0..depth {
        v = Value::Array(vec![v]);
    }
    toon_rust::encode(v, None)
}

fn synth_deep_object(depth: usize) -> String {
    let mut v = Value::Null;
    for i in 0..depth {
        let mut map = serde_json::Map::new();
        map.insert(format!("k{i}"), v);
        v = Value::Object(map);
    }
    toon_rust::encode(v, None)
}

fn synth_deep_mixed(depth: usize) -> String {
    let mut v = Value::Null;
    for i in 0..depth {
        if i.is_multiple_of(2) {
            v = Value::Array(vec![v]);
        } else {
            let mut map = serde_json::Map::new();
            map.insert(format!("n{i}"), v);
            v = Value::Object(map);
        }
    }
    toon_rust::encode(v, None)
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

fn exercise(input: &str) {
    let decoded = try_decode(input, None);
    let from_lines =
        try_decode_from_lines(input.split('\n').map(str::to_string).collect(), None);
    let stream = try_decode_stream_sync(
        input.split('\n').map(str::to_string).collect(),
        None,
    );

    // All three entry points must agree on success/failure.
    assert_eq!(decoded.is_ok(), from_lines.is_ok(), "try_decode vs try_decode_from_lines disagree");
    assert_eq!(decoded.is_ok(), stream.is_ok(), "try_decode vs stream_sync disagree");

    let Ok(decoded) = decoded else {
        return;
    };

    // Re-encode and re-decode; the second decode must be semantically
    // equivalent to the first.
    let as_json = Value::from(decoded.clone());
    let reencoded = toon_rust::encode(as_json.clone(), None);
    if reencoded.len() > MAX_INPUT_BYTES {
        // Re-encoded output may exceed the input budget for extreme inputs;
        // do not re-drive the decoder with oversized payloads.
        return;
    }

    match try_decode(&reencoded, None) {
        Ok(redecoded) => {
            let roundtrip_json = Value::from(redecoded);
            assert!(
                json_equivalent(&as_json, &roundtrip_json),
                "toon encode→decode round-trip produced non-equivalent value"
            );
        }
        Err(_) => {
            // A successful first decode whose re-encoded form fails to
            // decode is a genuine bug in toon_rust — surface it.
            panic!(
                "re-encoded TOON failed to parse:\n{reencoded}\noriginal JsonValue: {:?}",
                JsonValue::from(as_json)
            );
        }
    }
}

fuzz_target!(|shape: InputShape| {
    let input = match shape {
        InputShape::Raw(bytes) => {
            if bytes.len() > MAX_INPUT_BYTES {
                return;
            }
            match std::str::from_utf8(&bytes) {
                Ok(s) => s.to_string(),
                Err(_) => return,
            }
        }
        InputShape::DeepArray(d) => synth_deep_array(d),
        InputShape::DeepObject(d) => synth_deep_object(d),
        InputShape::DeepMixed(d) => synth_deep_mixed(d),
    };

    if input.len() > MAX_INPUT_BYTES {
        return;
    }

    exercise(&input);
});
