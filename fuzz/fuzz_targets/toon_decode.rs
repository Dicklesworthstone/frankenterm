#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};

const MAX_STR_LEN: usize = 96;
const MAX_ITEMS: usize = 6;

#[derive(Arbitrary, Debug)]
struct ToonDecodeCase {
    value: RawJsonValue,
    mutation: Mutation,
}

#[derive(Arbitrary, Debug)]
enum Mutation {
    Clean,
    AppendBlankLines(u8),
    DropLastLine,
    DuplicateLastLine,
    AppendGarbage(String),
}

#[derive(Arbitrary, Debug)]
enum RawJsonValue {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
    Array(Vec<RawLeafValue>),
    Object(Vec<RawObjectEntry>),
    NestedObject {
        name: String,
        values: Vec<RawLeafValue>,
        fields: Vec<RawObjectEntry>,
    },
}

#[derive(Arbitrary, Debug)]
enum RawLeafValue {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
}

#[derive(Arbitrary, Debug)]
struct RawObjectEntry {
    key: String,
    value: RawLeafValue,
}

impl ToonDecodeCase {
    fn mutated_toon(self) -> (Value, String, bool) {
        let original = self.value.into_json();
        let base = toon_rust::encode(original.clone(), None);

        match self.mutation {
            Mutation::Clean => (original, base, true),
            Mutation::AppendBlankLines(count) => {
                let mut mutated = base;
                for _ in 0..usize::from(count % 3) {
                    mutated.push('\n');
                }
                (original, mutated, true)
            }
            Mutation::DropLastLine => {
                let mut lines = split_lines(&base);
                if !lines.is_empty() {
                    lines.pop();
                }
                (original, lines.join("\n"), false)
            }
            Mutation::DuplicateLastLine => {
                let mut lines = split_lines(&base);
                if let Some(last) = lines.last().cloned() {
                    lines.push(last);
                }
                (original, lines.join("\n"), false)
            }
            Mutation::AppendGarbage(garbage) => {
                let mut mutated = base;
                if !mutated.ends_with('\n') {
                    mutated.push('\n');
                }
                mutated.push_str(&limited_text(garbage, "x"));
                (original, mutated, false)
            }
        }
    }
}

impl RawJsonValue {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Int(value) => Value::from(value),
            Self::Text(value) => Value::String(limited_text(value, "text")),
            Self::Array(items) => Value::Array(
                items
                    .into_iter()
                    .take(MAX_ITEMS)
                    .map(RawLeafValue::into_json)
                    .collect(),
            ),
            Self::Object(entries) => Value::Object(
                entries
                    .into_iter()
                    .take(MAX_ITEMS)
                    .map(|entry| (entry.key(), entry.value.into_json()))
                    .collect(),
            ),
            Self::NestedObject {
                name,
                values,
                fields,
            } => {
                let mut object = Map::new();
                object.insert(
                    "name".to_string(),
                    Value::String(limited_text(name, "nested")),
                );
                object.insert(
                    "values".to_string(),
                    Value::Array(
                        values
                            .into_iter()
                            .take(MAX_ITEMS)
                            .map(RawLeafValue::into_json)
                            .collect(),
                    ),
                );
                object.insert(
                    "fields".to_string(),
                    Value::Object(
                        fields
                            .into_iter()
                            .take(MAX_ITEMS)
                            .map(|entry| (entry.key(), entry.value.into_json()))
                            .collect(),
                    ),
                );
                Value::Object(object)
            }
        }
    }
}

impl RawLeafValue {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Int(value) => Value::from(value),
            Self::Text(value) => Value::String(limited_text(value, "leaf")),
        }
    }
}

impl RawObjectEntry {
    fn key(&self) -> String {
        limited_text(self.key.clone(), "key")
    }
}

fn limited_text(value: String, fallback: &str) -> String {
    if value.is_empty() {
        return fallback.to_string();
    }

    value.chars().take(MAX_STR_LEN).collect()
}

fn split_lines(input: &str) -> Vec<String> {
    input
        .split('\n')
        .map(std::string::ToString::to_string)
        .collect()
}

fn json_values_equivalent(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(lhs), Value::Bool(rhs)) => lhs == rhs,
        (Value::String(lhs), Value::String(rhs)) => lhs == rhs,
        (Value::Number(lhs), Value::Number(rhs)) => {
            let Some(lhs) = lhs.as_f64() else {
                return false;
            };
            let Some(rhs) = rhs.as_f64() else {
                return false;
            };
            (lhs - rhs).abs() <= lhs.abs().max(rhs.abs()).max(1.0) * 2.0 * f64::EPSILON
        }
        (Value::Array(lhs), Value::Array(rhs)) => {
            lhs.len() == rhs.len()
                && lhs
                    .iter()
                    .zip(rhs.iter())
                    .all(|(lhs, rhs)| json_values_equivalent(lhs, rhs))
        }
        (Value::Object(lhs), Value::Object(rhs)) => {
            lhs.len() == rhs.len()
                && lhs.iter().all(|(key, value)| {
                    rhs.get(key)
                        .is_some_and(|other| json_values_equivalent(value, other))
                })
        }
        _ => false,
    }
}

fuzz_target!(|raw: ToonDecodeCase| {
    let (original, candidate, clean_equivalent) = raw.mutated_toon();
    if candidate.len() > 16_384 {
        return;
    }

    let lines = split_lines(&candidate);
    let decoded = toon_rust::try_decode(&candidate, None);
    let decoded_from_lines = toon_rust::try_decode_from_lines(lines.clone(), None);
    let stream_events = toon_rust::try_decode_stream_sync(lines, None);

    assert_eq!(decoded.is_ok(), decoded_from_lines.is_ok());
    assert_eq!(decoded.is_ok(), stream_events.is_ok());

    if let (Ok(decoded), Ok(decoded_from_lines), Ok(_events)) =
        (decoded, decoded_from_lines, stream_events)
    {
        let decoded_json = Value::from(decoded);
        let decoded_lines_json = Value::from(decoded_from_lines);
        assert!(json_values_equivalent(&decoded_json, &decoded_lines_json));

        if clean_equivalent {
            assert!(json_values_equivalent(&decoded_json, &original));
        }

        let reencoded = toon_rust::encode(decoded_json.clone(), None);
        let redecode = toon_rust::try_decode(&reencoded, None)
            .ok()
            .map(Value::from);
        assert!(
            redecode
                .as_ref()
                .is_some_and(|redecoded| json_values_equivalent(redecoded, &decoded_json))
        );
    }
});
