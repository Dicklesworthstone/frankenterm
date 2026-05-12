#![allow(dead_code)]

use jsonschema::{Draft, Validator};
use serde::Serialize;
use serde_json::Value;

pub const MAX_ITEMS: usize = 8;
pub const MAX_TEXT_CHARS: usize = 96;

pub fn compile_schema(bytes: &'static [u8]) -> Validator {
    let schema: Value = serde_json::from_slice(bytes).expect("contract schema must be valid JSON");
    Validator::options()
        .with_draft(Draft::Draft202012)
        .build(&schema)
        .expect("contract schema must compile under Draft 2020-12")
}

pub fn roundtrip_value<T>(value: &T) -> Value
where
    T: Serialize + ?Sized,
{
    let bytes = serde_json::to_vec(value).expect("contract output must serialize");
    serde_json::from_slice(&bytes).expect("serialized contract output must parse as JSON")
}

pub fn assert_schema_valid(schema: &Validator, value: &Value) {
    if let Err(errors) = schema.validate(value) {
        let failures = errors
            .take(4)
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        panic!(
            "contract schema validation failed: {}",
            failures.join(" | ")
        );
    }
}

pub fn assert_no_raw_content_flags(value: &Value) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key.starts_with("raw_")
                    && (key.ends_with("_stored") || key.ends_with("_allowed"))
                {
                    assert_eq!(
                        child,
                        &Value::Bool(false),
                        "privacy flag {key} must be false"
                    );
                }
                assert_no_raw_content_flags(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_no_raw_content_flags(item);
            }
        }
        _ => {}
    }
}

pub fn limited_text(input: String, fallback: &str) -> String {
    let value = input
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_TEXT_CHARS)
        .collect::<String>();
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

pub fn stable_fragment(input: String, fallback: &str) -> String {
    let value = input
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/'))
        .take(MAX_TEXT_CHARS)
        .collect::<String>();
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

pub fn bounded_ms(value: u64) -> u64 {
    value % 4_102_444_800_000
}
