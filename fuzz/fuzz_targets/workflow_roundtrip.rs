#![no_main]
//! WorkflowConfig + WorkflowExecutionResult JSON/TOON round-trip fuzzer.
//!
//! This target exercises the *serialization surface* of the workflow runner's
//! public result types — the contract consumed by MCP clients, the robot
//! frame log, and any downstream replay tooling. Running a real
//! `WorkflowEngine::execute` inside a libfuzzer harness is infeasible
//! (requires SQLite, async runtime, live pane capabilities); instead we
//! synthesize structurally valid `WorkflowConfig` and `WorkflowExecutionResult`
//! values via `Arbitrary` and push them through the full serialization
//! matrix: JSON → struct → JSON (fixed point), TOON → struct → TOON
//! (fixed point), and cross-format agreement (JSON-decoded value equivalent
//! to TOON-decoded value).
//!
//! Invariants:
//! - Serialization to JSON and TOON never panics.
//! - Every emitted JSON roundtrips: `json_of(from_json(json_of(v))) == json_of(v)`.
//! - Every emitted TOON roundtrips into a semantically equivalent JSON Value.
//! - Non-determinism: mirror structs use deterministic primitive fields (no
//!   timestamps pulled from `SystemTime::now`), so no scrubbing is needed.
//!   Execution IDs are part of the serialized payload — if they matter for
//!   equality they must round-trip bit-for-bit.
//!
//! The fuzz crate deliberately avoids depending on `frankenterm-core` here;
//! the mirror structs carry the same serde tags/field order as the real
//! types so drift in either direction will surface as a roundtrip failure
//! once the fuzz is cross-checked against core in a dedicated integration
//! test.

use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_STRING_CHARS: usize = 96;
const MAX_JSON_DEPTH: u8 = 3;
const MAX_JSON_WIDTH: usize = 4;

/// Mirror of `frankenterm_core::workflows::context::WorkflowConfig`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct FuzzWorkflowConfig {
    default_wait_timeout_ms: u64,
    max_step_retries: u32,
    retry_delay_ms: u64,
}

impl<'a> Arbitrary<'a> for FuzzWorkflowConfig {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(Self {
            default_wait_timeout_ms: u.arbitrary()?,
            max_step_retries: u.arbitrary()?,
            retry_delay_ms: u.arbitrary()?,
        })
    }
}

/// Mirror of `frankenterm_core::workflows::runner::WorkflowExecutionResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FuzzWorkflowExecutionResult {
    Completed {
        execution_id: String,
        result: Value,
        elapsed_ms: u64,
        steps_executed: usize,
    },
    Aborted {
        execution_id: String,
        reason: String,
        step_index: usize,
        elapsed_ms: u64,
    },
    PolicyDenied {
        execution_id: String,
        step_index: usize,
        reason: String,
    },
    Error {
        execution_id: Option<String>,
        error: String,
    },
}

impl<'a> Arbitrary<'a> for FuzzWorkflowExecutionResult {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=3u8)? {
            0 => Self::Completed {
                execution_id: bounded_string(u, "exec-0000")?,
                result: arbitrary_json_value(u, MAX_JSON_DEPTH)?,
                elapsed_ms: u.arbitrary()?,
                steps_executed: u.int_in_range(0..=1024)?,
            },
            1 => Self::Aborted {
                execution_id: bounded_string(u, "exec-0000")?,
                reason: bounded_string(u, "aborted")?,
                step_index: u.int_in_range(0..=1024)?,
                elapsed_ms: u.arbitrary()?,
            },
            2 => Self::PolicyDenied {
                execution_id: bounded_string(u, "exec-0000")?,
                step_index: u.int_in_range(0..=1024)?,
                reason: bounded_string(u, "denied")?,
            },
            _ => Self::Error {
                execution_id: if bool::arbitrary(u)? {
                    Some(bounded_string(u, "exec-0000")?)
                } else {
                    None
                },
                error: bounded_string(u, "error")?,
            },
        })
    }
}

#[derive(Debug)]
struct FuzzCase {
    config: FuzzWorkflowConfig,
    result: FuzzWorkflowExecutionResult,
}

impl<'a> Arbitrary<'a> for FuzzCase {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(Self {
            config: FuzzWorkflowConfig::arbitrary(u)?,
            result: FuzzWorkflowExecutionResult::arbitrary(u)?,
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

fn arbitrary_json_value(
    u: &mut Unstructured<'_>,
    depth: u8,
) -> libfuzzer_sys::arbitrary::Result<Value> {
    if depth == 0 {
        return arbitrary_json_primitive(u);
    }
    Ok(match u.int_in_range(0..=5u8)? {
        0..=2 => arbitrary_json_primitive(u)?,
        3 => {
            let len = u.int_in_range(0..=MAX_JSON_WIDTH)?;
            let mut v = Vec::with_capacity(len);
            for _ in 0..len {
                v.push(arbitrary_json_value(u, depth - 1)?);
            }
            Value::Array(v)
        }
        _ => {
            let len = u.int_in_range(0..=MAX_JSON_WIDTH)?;
            let mut m = serde_json::Map::with_capacity(len);
            for i in 0..len {
                let key = bounded_string(u, &format!("k{i}"))?;
                m.insert(key, arbitrary_json_value(u, depth - 1)?);
            }
            Value::Object(m)
        }
    })
}

fn arbitrary_json_primitive(u: &mut Unstructured<'_>) -> libfuzzer_sys::arbitrary::Result<Value> {
    Ok(match u.int_in_range(0..=3u8)? {
        0 => Value::Null,
        1 => Value::Bool(u.arbitrary()?),
        2 => Value::Number(serde_json::Number::from(u.arbitrary::<i64>()?)),
        _ => Value::String(bounded_string(u, "text")?),
    })
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
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|other| json_equivalent(v, other)))
        }
        _ => false,
    }
}

fn check_json_roundtrip<T>(label: &str, value: &T)
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    let first_json =
        serde_json::to_value(value).unwrap_or_else(|e| panic!("{label}: to_value failed: {e}"));
    let decoded: T = serde_json::from_value(first_json.clone())
        .unwrap_or_else(|e| panic!("{label}: from_value failed: {e}\npayload={first_json}"));
    let second_json = serde_json::to_value(&decoded)
        .unwrap_or_else(|e| panic!("{label}: second to_value failed: {e}"));
    assert!(
        json_equivalent(&first_json, &second_json),
        "{label}: JSON roundtrip not a fixed point"
    );
}

fn check_toon_bridge<T>(label: &str, value: &T)
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    let as_json = match serde_json::to_value(value) {
        Ok(v) => v,
        Err(_) => return,
    };

    let toon_text = toon_rust::encode(as_json.clone(), None);

    let decoded_tree = match toon_rust::try_decode(&toon_text, None) {
        Ok(v) => v,
        Err(err) => panic!(
            "{label}: TOON decode of self-emitted payload failed:\n{toon_text}\nerror: {err:?}"
        ),
    };
    let via_toon = Value::from(decoded_tree);
    assert!(
        json_equivalent(&as_json, &via_toon),
        "{label}: JSON→TOON→JSON not equivalent"
    );

    let reencoded = toon_rust::encode(via_toon.clone(), None);
    assert_eq!(
        toon_text, reencoded,
        "{label}: TOON encoding is not a fixed point"
    );

    // Final belt-and-suspenders — deserialize back into T via the TOON path.
    if let Err(err) = serde_json::from_value::<T>(via_toon) {
        panic!("{label}: TOON-bridged JSON failed to deserialize into T: {err}");
    }
}

fuzz_target!(|case: FuzzCase| {
    check_json_roundtrip("WorkflowConfig", &case.config);
    check_toon_bridge("WorkflowConfig", &case.config);

    check_json_roundtrip("WorkflowExecutionResult", &case.result);
    check_toon_bridge("WorkflowExecutionResult", &case.result);
});
