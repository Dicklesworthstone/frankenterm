#![no_main]
//! PatternEngine regex match fuzzer with ReDoS guard + stability oracle.
//!
//! Feeds arbitrary bytes through `PatternEngine::detect` after building a rule
//! pack whose regex set is a mix of user-controlled seeds and a fixed pool of
//! historically-dangerous patterns (nested quantifiers, alternation with
//! overlap, and non-linear cases that the `regex` crate promises to reject).
//!
//! Invariants checked:
//! - `detect()` never panics.
//! - A second `detect()` call on the same text returns byte-identical results
//!   (stability — no leaking state between scans).
//! - `quick_reject()` agrees with `detect()`: if `quick_reject` says reject,
//!   `detect` must return an empty vec.
//! - Wall-clock evaluation time stays under `MAX_EVAL_MS` per call. A breach
//!   is treated as a ReDoS signal and panics so libfuzzer records it.

use std::time::{Duration, Instant};

use frankenterm_core::patterns::{
    AgentType, Detection, PatternEngine, PatternPack, RuleDef, Severity,
};
use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;

const MAX_EVAL_MS: u128 = 100;
const MAX_RULES: usize = 6;
const MAX_ANCHOR_LEN: usize = 24;
const MAX_REGEX_LEN: usize = 96;
const MAX_TEXT_BYTES: usize = 16 * 1024;

/// Known-suspicious patterns drawn from ReDoS case studies. Rust's `regex`
/// crate is linear-time, so these should not blow up — but the pattern engine
/// layers quick-reject, anchoring, and extraction on top, any of which can
/// introduce non-linear behavior. Including them as seeds ensures corpus
/// coverage even when the fuzzer starts cold.
const REDOS_SEEDS: &[&str] = &[
    r"(a+)+$",
    r"(a|a)*$",
    r"(a|aa)+$",
    r"(.*a){10}",
    r"(x+x+)+y",
    r"([a-zA-Z]+)*$",
    r"^(([a-z])+\.)+[A-Z]([a-z])+$",
    r"a?a?a?a?a?aaaaa",
    r"\d{1,1000}",
    r"([0-9]+)*[0-9a-f]",
];

#[derive(Debug, Clone)]
enum RegexSource {
    Seed(u8),
    User(String),
    None,
}

impl<'a> Arbitrary<'a> for RegexSource {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=2u8)? {
            0 => Self::Seed(u.arbitrary::<u8>()?),
            1 => Self::User(bounded_ascii(u, MAX_REGEX_LEN)?),
            _ => Self::None,
        })
    }
}

#[derive(Debug, Clone)]
struct RawRule {
    anchor: String,
    alt_anchor: String,
    regex: RegexSource,
    severity_tag: u8,
}

impl<'a> Arbitrary<'a> for RawRule {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(Self {
            anchor: bounded_ascii(u, MAX_ANCHOR_LEN)?,
            alt_anchor: bounded_ascii(u, MAX_ANCHOR_LEN)?,
            regex: RegexSource::arbitrary(u)?,
            severity_tag: u.arbitrary()?,
        })
    }
}

#[derive(Debug)]
struct FuzzCase {
    rules: Vec<RawRule>,
    text: Vec<u8>,
}

impl<'a> Arbitrary<'a> for FuzzCase {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        let rule_count = u.int_in_range(1..=MAX_RULES)?;
        let mut rules = Vec::with_capacity(rule_count);
        for _ in 0..rule_count {
            rules.push(RawRule::arbitrary(u)?);
        }
        let text_len = u.int_in_range(0..=MAX_TEXT_BYTES)?;
        let text = u.bytes(text_len)?.to_vec();
        Ok(Self { rules, text })
    }
}

fn bounded_ascii(
    u: &mut Unstructured<'_>,
    max: usize,
) -> libfuzzer_sys::arbitrary::Result<String> {
    let len = u.int_in_range(0..=max)?;
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        let byte = u.arbitrary::<u8>()?;
        let ch = match byte % 64 {
            0..=25 => (b'a' + (byte % 26)) as char,
            26..=51 => (b'A' + (byte % 26)) as char,
            52..=61 => (b'0' + (byte % 10)) as char,
            62 => '_',
            _ => '.',
        };
        s.push(ch);
    }
    Ok(s)
}

fn sanitize_anchor(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(MAX_ANCHOR_LEN)
        .collect();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

fn resolve_regex(source: &RegexSource) -> Option<String> {
    match source {
        RegexSource::Seed(idx) => {
            let pick = REDOS_SEEDS[*idx as usize % REDOS_SEEDS.len()];
            Some(pick.to_string())
        }
        RegexSource::User(raw) => {
            if raw.is_empty() {
                None
            } else {
                Some(raw.clone())
            }
        }
        RegexSource::None => None,
    }
}

fn severity_for(tag: u8) -> Severity {
    match tag % 3 {
        0 => Severity::Info,
        1 => Severity::Warning,
        _ => Severity::Critical,
    }
}

fn build_rules(raw_rules: &[RawRule]) -> Vec<RuleDef> {
    raw_rules
        .iter()
        .enumerate()
        .map(|(idx, raw)| {
            let anchor = sanitize_anchor(&raw.anchor, "ANCHOR");
            let alt = sanitize_anchor(&raw.alt_anchor, &anchor);
            RuleDef {
                id: format!("codex.regex_fuzz_{idx}"),
                agent_type: AgentType::Codex,
                event_type: "usage.warning".to_string(),
                severity: severity_for(raw.severity_tag),
                anchors: vec![anchor, alt],
                regex: resolve_regex(&raw.regex),
                description: "regex fuzz rule".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            }
        })
        .collect()
}

/// Detection vectors contain `f64` and raw `serde_json::Value` so we can't
/// derive Eq. Instead we compare a compact fingerprint that captures the
/// fields that must be stable across repeat calls on identical input.
fn detection_fingerprint(detections: &[Detection]) -> Vec<(String, String, String, (usize, usize))> {
    detections
        .iter()
        .map(|d| {
            (
                d.rule_id.clone(),
                d.event_type.clone(),
                d.matched_text.clone(),
                d.span,
            )
        })
        .collect()
}

fn time_detect(engine: &PatternEngine, text: &str) -> (Vec<Detection>, Duration) {
    let start = Instant::now();
    let result = engine.detect(text);
    (result, start.elapsed())
}

fuzz_target!(|case: FuzzCase| {
    let rules = build_rules(&case.rules);
    let pack = PatternPack::new("fuzz.regex", "0.1.0", rules);
    let engine = match PatternEngine::with_packs(vec![pack]) {
        Ok(engine) => engine,
        // Invalid regex / bad rules are expected and not a bug.
        Err(_) => return,
    };

    let text = match std::str::from_utf8(&case.text) {
        Ok(t) => t,
        Err(_) => return,
    };

    // Quick-reject must agree with detect.
    let quick_accepts = !engine.quick_reject(text);

    let (first, first_elapsed) = time_detect(&engine, text);
    assert!(
        first_elapsed.as_millis() <= MAX_EVAL_MS,
        "regex eval ReDoS: first detect took {}ms on {} byte text",
        first_elapsed.as_millis(),
        text.len()
    );

    if !quick_accepts {
        assert!(
            first.is_empty(),
            "quick_reject said reject but detect emitted {} detections",
            first.len()
        );
    }

    let (second, second_elapsed) = time_detect(&engine, text);
    assert!(
        second_elapsed.as_millis() <= MAX_EVAL_MS,
        "regex eval ReDoS: second detect took {}ms",
        second_elapsed.as_millis()
    );

    assert_eq!(
        detection_fingerprint(&first),
        detection_fingerprint(&second),
        "PatternEngine::detect is not stable across repeat calls on identical input"
    );
});
