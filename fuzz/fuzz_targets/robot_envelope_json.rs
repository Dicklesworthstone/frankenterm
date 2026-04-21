#![no_main]

use arbitrary::Arbitrary;
use frankenterm_core::robot_envelope::RobotEnvelope;
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};

const MAX_STR_LEN: usize = 96;
const MAX_ITEMS: usize = 6;

#[derive(Arbitrary, Debug)]
struct RawEnvelopeDocument {
    mode: DecodeMode,
    timestamp: String,
    source: String,
    data: RawPayload,
    degraded: bool,
    include_degraded: bool,
    wrong_value: WrongValue,
    extra_fields: Vec<RawObjectEntry>,
}

#[derive(Arbitrary, Debug)]
enum DecodeMode {
    Valid,
    Missing(MissingField),
    Mistyped(WrongField),
}

#[derive(Arbitrary, Debug)]
enum MissingField {
    Timestamp,
    Source,
    Data,
}

#[derive(Arbitrary, Debug)]
enum WrongField {
    Timestamp,
    Source,
    Data,
    Degraded,
}

#[derive(Arbitrary, Debug)]
enum WrongValue {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
    Array(Vec<RawScalar>),
}

#[derive(Arbitrary, Debug)]
enum RawPayload {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
    Array(Vec<RawScalar>),
    Object(Vec<RawObjectEntry>),
    EnvelopeLike(RawNestedEnvelope),
}

#[derive(Arbitrary, Debug)]
enum RawScalar {
    Null,
    Bool(bool),
    Int(i64),
    Text(String),
}

#[derive(Arbitrary, Debug)]
struct RawObjectEntry {
    key: String,
    value: RawScalar,
}

#[derive(Arbitrary, Debug)]
struct RawNestedEnvelope {
    timestamp: String,
    source: String,
    data: RawScalar,
    degraded: bool,
    include_degraded: bool,
}

impl RawEnvelopeDocument {
    fn into_json(self) -> Value {
        let mut object = Map::new();
        object.insert(
            "timestamp".to_string(),
            Value::String(limited_text(self.timestamp, "2026-01-01T00:00:00Z")),
        );
        object.insert(
            "source".to_string(),
            Value::String(limited_text(self.source, "bridge")),
        );
        object.insert("data".to_string(), self.data.into_json());

        if self.include_degraded {
            object.insert("degraded".to_string(), Value::Bool(self.degraded));
        }

        for extra in self.extra_fields.into_iter().take(MAX_ITEMS) {
            object.insert(extra.key(), extra.value.into_json());
        }

        match self.mode {
            DecodeMode::Valid => {}
            DecodeMode::Missing(field) => match field {
                MissingField::Timestamp => {
                    object.remove("timestamp");
                }
                MissingField::Source => {
                    object.remove("source");
                }
                MissingField::Data => {
                    object.remove("data");
                }
            },
            DecodeMode::Mistyped(field) => {
                let key = match field {
                    WrongField::Timestamp => "timestamp",
                    WrongField::Source => "source",
                    WrongField::Data => "data",
                    WrongField::Degraded => "degraded",
                };
                object.insert(key.to_string(), self.wrong_value.into_json());
            }
        }

        Value::Object(object)
    }
}

impl WrongValue {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Int(value) => Value::from(value),
            Self::Text(value) => Value::String(limited_text(value, "wrong")),
            Self::Array(items) => Value::Array(
                items
                    .into_iter()
                    .take(MAX_ITEMS)
                    .map(RawScalar::into_json)
                    .collect(),
            ),
        }
    }
}

impl RawPayload {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Int(value) => Value::from(value),
            Self::Text(value) => Value::String(limited_text(value, "payload")),
            Self::Array(items) => Value::Array(
                items
                    .into_iter()
                    .take(MAX_ITEMS)
                    .map(RawScalar::into_json)
                    .collect(),
            ),
            Self::Object(entries) => Value::Object(
                entries
                    .into_iter()
                    .take(MAX_ITEMS)
                    .map(|entry| (entry.key(), entry.value.into_json()))
                    .collect(),
            ),
            Self::EnvelopeLike(nested) => nested.into_json(),
        }
    }
}

impl RawScalar {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Int(value) => Value::from(value),
            Self::Text(value) => Value::String(limited_text(value, "text")),
        }
    }
}

impl RawObjectEntry {
    fn key(&self) -> String {
        limited_text(self.key.clone(), "key")
    }
}

impl RawNestedEnvelope {
    fn into_json(self) -> Value {
        let mut object = Map::new();
        object.insert(
            "timestamp".to_string(),
            Value::String(limited_text(self.timestamp, "2026-01-01T00:00:00Z")),
        );
        object.insert(
            "source".to_string(),
            Value::String(limited_text(self.source, "nested")),
        );
        object.insert("data".to_string(), self.data.into_json());
        if self.include_degraded {
            object.insert("degraded".to_string(), Value::Bool(self.degraded));
        }
        Value::Object(object)
    }
}

fn limited_text(value: String, fallback: &str) -> String {
    if value.is_empty() {
        return fallback.to_string();
    }

    value.chars().take(MAX_STR_LEN).collect()
}

fuzz_target!(|raw: RawEnvelopeDocument| {
    let input = raw.into_json();
    let encoded = match serde_json::to_vec(&input) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };

    let decoded = serde_json::from_slice::<RobotEnvelope<Value>>(&encoded);

    if let Ok(envelope) = decoded {
        let Some(input_object) = input.as_object() else {
            return;
        };

        let Some(timestamp) = input_object.get("timestamp").and_then(Value::as_str) else {
            return;
        };
        let Some(source) = input_object.get("source").and_then(Value::as_str) else {
            return;
        };
        let Some(data) = input_object.get("data") else {
            return;
        };

        assert_eq!(envelope.timestamp, timestamp);
        assert_eq!(envelope.source, source);
        assert_eq!(&envelope.data, data);

        match input_object.get("degraded").and_then(Value::as_bool) {
            Some(expected) => assert_eq!(envelope.degraded, expected),
            None => assert!(!envelope.degraded),
        }

        let Ok(reencoded) = serde_json::to_vec(&envelope) else {
            return;
        };
        let reparsed = serde_json::from_slice::<RobotEnvelope<Value>>(&reencoded).ok();
        assert_eq!(reparsed.as_ref(), Some(&envelope));
    }
});
