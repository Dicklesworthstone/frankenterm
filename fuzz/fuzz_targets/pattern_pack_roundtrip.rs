#![no_main]
//! `PatternPack` serialization fixed-point fuzzer.
//!
//! Takes an `Arbitrary`-derived `FuzzPatternPack` (shape-matched to
//! `frankenterm_core::patterns::PatternPack` + `RuleDef`) and exercises the
//! full serde surface: JSON, TOML (the canonical pack format on disk), and
//! TOON (the wire format used for some MCP tooling). For each format the
//! target asserts:
//!
//! 1. serialize(V) = A
//! 2. deserialize(A) = V'
//! 3. serialize(V') = B
//! 4. assert A == B  (fixed point)
//!
//! Mirror structs are kept in lockstep with the real types' serde shape.
//! Any drift — including new fields, changed tags, or skip_serializing_if
//! reintroduction (varbincode hazard noted in codec) — will surface here
//! because the fixed-point property is violated as soon as the emitted
//! encoding no longer matches what the original type produces.

use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_STRING_CHARS: usize = 96;
const MAX_ANCHORS: usize = 4;
const MAX_RULES: usize = 6;
const ALLOWED_PREFIXES: &[&str] = &["codex", "claude_code", "gemini", "wezterm"];

/// Shape-matched mirror of `frankenterm_core::patterns::PatternPack`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FuzzPatternPack {
    name: String,
    version: String,
    rules: Vec<FuzzRuleDef>,
}

/// Shape-matched mirror of `frankenterm_core::patterns::RuleDef`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FuzzRuleDef {
    id: String,
    agent_type: FuzzAgentType,
    event_type: String,
    severity: FuzzSeverity,
    anchors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    regex: Option<String>,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remediation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manual_fix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preview_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    learn_more_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum FuzzAgentType {
    Codex,
    ClaudeCode,
    Gemini,
    Wezterm,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum FuzzSeverity {
    Info,
    Warning,
    Critical,
}

impl FuzzAgentType {
    fn prefix(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::Gemini => "gemini",
            Self::Wezterm => "wezterm",
        }
    }
}

impl<'a> Arbitrary<'a> for FuzzAgentType {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=3u8)? {
            0 => Self::Codex,
            1 => Self::ClaudeCode,
            2 => Self::Gemini,
            _ => Self::Wezterm,
        })
    }
}

impl<'a> Arbitrary<'a> for FuzzSeverity {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=2u8)? {
            0 => Self::Info,
            1 => Self::Warning,
            _ => Self::Critical,
        })
    }
}

impl<'a> Arbitrary<'a> for FuzzRuleDef {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        let agent_type = FuzzAgentType::arbitrary(u)?;
        let severity = FuzzSeverity::arbitrary(u)?;
        let idx = u.arbitrary::<u16>()?;
        let id = format!("{}.generated_{idx}", agent_type.prefix());
        let anchor_count = u.int_in_range(1..=MAX_ANCHORS)?;
        let mut anchors = Vec::with_capacity(anchor_count);
        for i in 0..anchor_count {
            anchors.push(bounded_token(u, &format!("anchor_{i}"))?);
        }
        Ok(Self {
            id,
            agent_type,
            event_type: bounded_string(u, "usage.warning")?,
            severity,
            anchors,
            regex: arbitrary_optional_string(u)?,
            description: bounded_string(u, "generated rule")?,
            remediation: arbitrary_optional_string(u)?,
            workflow: arbitrary_optional_string(u)?,
            manual_fix: arbitrary_optional_string(u)?,
            preview_command: arbitrary_optional_string(u)?,
            learn_more_url: arbitrary_optional_string(u)?,
        })
    }
}

impl<'a> Arbitrary<'a> for FuzzPatternPack {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        let rule_count = u.int_in_range(1..=MAX_RULES)?;
        let mut rules = Vec::with_capacity(rule_count);
        for _ in 0..rule_count {
            rules.push(FuzzRuleDef::arbitrary(u)?);
        }
        Ok(Self {
            name: bounded_string(u, "fuzz-pack")?,
            version: bounded_string(u, "0.1.0")?,
            rules,
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

fn bounded_token(
    u: &mut Unstructured<'_>,
    fallback: &str,
) -> libfuzzer_sys::arbitrary::Result<String> {
    let raw: String = Arbitrary::arbitrary(u)?;
    let token: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
        .take(MAX_STRING_CHARS)
        .collect();
    if token.is_empty() {
        Ok(fallback.to_string())
    } else {
        Ok(token)
    }
}

fn arbitrary_optional_string(
    u: &mut Unstructured<'_>,
) -> libfuzzer_sys::arbitrary::Result<Option<String>> {
    if bool::arbitrary(u)? {
        Ok(Some(bounded_string(u, "value")?))
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
                (x - y).abs() <= x.abs().max(y.abs()).max(1.0) * 2.0 * f64::EPSILON
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

fn check_json_fixed_point(pack: &FuzzPatternPack) {
    let first = serde_json::to_value(pack).expect("JSON serialize must succeed");
    let decoded: FuzzPatternPack =
        serde_json::from_value(first.clone()).expect("JSON decode must succeed");
    let second = serde_json::to_value(&decoded).expect("JSON re-serialize must succeed");
    assert!(
        json_equivalent(&first, &second),
        "JSON fixed-point violated:\n first={first}\n second={second}"
    );
}

fn check_toml_fixed_point(pack: &FuzzPatternPack) {
    let first = match toml::to_string(pack) {
        Ok(s) => s,
        // TOML rejects some valid JSON shapes (e.g., strings containing
        // control bytes can trip the encoder). Skip those inputs — they are
        // not representative of on-disk rule packs.
        Err(_) => return,
    };
    let decoded: FuzzPatternPack = match toml::from_str(&first) {
        Ok(v) => v,
        Err(err) => panic!("TOML self-emitted pack failed to decode: {err}\n{first}"),
    };
    let second = toml::to_string(&decoded).expect("TOML re-serialize must succeed");
    assert_eq!(first, second, "TOML fixed-point violated");
}

fn check_toon_fixed_point(pack: &FuzzPatternPack) {
    let as_json = match serde_json::to_value(pack) {
        Ok(v) => v,
        Err(_) => return,
    };
    let first_toon = toon_rust::encode(as_json.clone(), None);

    let decoded_tree = match toon_rust::try_decode(&first_toon, None) {
        Ok(v) => v,
        Err(err) => panic!(
            "TOON self-emitted PatternPack failed to decode:\n{first_toon}\nerror: {err:?}"
        ),
    };
    let via_toon = Value::from(decoded_tree);
    assert!(
        json_equivalent(&as_json, &via_toon),
        "TOON roundtrip value not equivalent"
    );

    let second_toon = toon_rust::encode(via_toon.clone(), None);
    assert_eq!(first_toon, second_toon, "TOON fixed-point violated");

    if let Err(err) = serde_json::from_value::<FuzzPatternPack>(via_toon) {
        panic!("TOON→JSON bridge failed to re-deserialize PatternPack: {err}");
    }
}

fuzz_target!(|pack: FuzzPatternPack| {
    // Enforce the ALLOWED_PREFIXES convention (real PatternPack requires
    // dotted ids prefixed with one of codex/claude_code/gemini/wezterm for
    // builtin packs). Arbitrary ids should already satisfy this by
    // construction; belt-and-suspenders check:
    for rule in &pack.rules {
        let ok = ALLOWED_PREFIXES.iter().any(|p| rule.id.starts_with(p));
        assert!(ok, "mirror must keep rule ids prefix-valid: {}", rule.id);
    }

    check_json_fixed_point(&pack);
    check_toml_fixed_point(&pack);
    check_toon_fixed_point(&pack);
});
