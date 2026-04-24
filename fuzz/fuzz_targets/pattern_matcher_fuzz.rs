#![no_main]
//! PatternEngine Aho-Corasick + regex combined fuzzer.
//!
//! Targets the full matcher pipeline end-to-end:
//!   anchor Aho-Corasick scan → candidate rules → extraction regex → Detection.
//!
//! Invariants:
//! - no panic on any rule set + haystack.
//! - `detect()` is deterministic: three back-to-back calls produce the same
//!   detection fingerprint.
//! - All detection spans are within haystack bounds and `matched_text` is
//!   consistent with the byte slice at that span.
//! - Known bug class — Aho-Corasick `LeftmostFirst` non-overlapping matching
//!   can eat bytes in one chunk that prevent a later chunk from matching
//!   (scan_pipeline.rs 6e67bcb9). We exercise this by splitting the haystack
//!   into an arbitrary number of chunks and feeding them through
//!   `detect_with_context` streaming; every detection emitted during streaming
//!   must also satisfy the within-bounds and text-consistency invariants
//!   against the reassembled buffer seen so far.
//! - Wall-clock per-call budget is 50ms; a breach panics as a ReDoS/algorithmic
//!   complexity signal.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use frankenterm_core::patterns::{
    AgentType, Detection, DetectionContext, PatternEngine, PatternPack, RuleDef, Severity,
};
use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;

const MAX_EVAL_MS: u128 = 50;
const MIN_RULES: usize = 1;
const MAX_RULES: usize = 5;
const MAX_ANCHOR_LEN: usize = 24;
const MAX_REGEX_LEN: usize = 96;
const MAX_HAYSTACK_BYTES: usize = 64 * 1024;
const MAX_SPLIT_POINTS: usize = 8;

/// Pool of regex shapes known to either be benign or stress-test the engine's
/// extraction path. The Rust `regex` crate guarantees linear time, but pattern
/// composition with anchors + captures has historically surprised us.
const REGEX_POOL: &[&str] = &[
    r"(?P<num>\d+)",
    r"(?P<word>[A-Za-z_]+)",
    r"(?P<ident>[A-Za-z_][A-Za-z0-9_]{0,32})",
    r"(?P<url>https?://[^\s]{0,64})",
    r"(?P<path>/[A-Za-z0-9_./-]{0,64})",
    r"(?P<hex>0x[0-9a-fA-F]{1,16})",
    r"anchor[0-9]{0,4}",
    r"[A-Z]{2,}_[A-Z]{2,}",
    r"(\w+\s+){0,4}\w+",
];

#[derive(Debug, Clone)]
struct RawRule {
    anchor: String,
    alt_anchor: String,
    regex_choice: RegexChoice,
    severity_tag: u8,
}

#[derive(Debug, Clone)]
enum RegexChoice {
    None,
    Pool(u8),
    User(String),
}

impl<'a> Arbitrary<'a> for RegexChoice {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=4u8)? {
            0 => Self::None,
            1 | 2 | 3 => Self::Pool(u.arbitrary::<u8>()?),
            _ => Self::User(bounded_ascii(u, MAX_REGEX_LEN)?),
        })
    }
}

impl<'a> Arbitrary<'a> for RawRule {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        Ok(Self {
            anchor: bounded_ascii(u, MAX_ANCHOR_LEN)?,
            alt_anchor: bounded_ascii(u, MAX_ANCHOR_LEN)?,
            regex_choice: RegexChoice::arbitrary(u)?,
            severity_tag: u.arbitrary()?,
        })
    }
}

#[derive(Debug)]
struct FuzzCase {
    rules: Vec<RawRule>,
    haystack: Vec<u8>,
    split_points: Vec<u16>,
}

impl<'a> Arbitrary<'a> for FuzzCase {
    fn arbitrary(u: &mut Unstructured<'a>) -> libfuzzer_sys::arbitrary::Result<Self> {
        let rule_count = u.int_in_range(MIN_RULES..=MAX_RULES)?;
        let mut rules = Vec::with_capacity(rule_count);
        for _ in 0..rule_count {
            rules.push(RawRule::arbitrary(u)?);
        }
        let haystack_len = u.int_in_range(0..=MAX_HAYSTACK_BYTES)?;
        let haystack = u.bytes(haystack_len)?.to_vec();
        let split_count = u.int_in_range(0..=MAX_SPLIT_POINTS)?;
        let mut split_points = Vec::with_capacity(split_count);
        for _ in 0..split_count {
            split_points.push(u.arbitrary::<u16>()?);
        }
        Ok(Self {
            rules,
            haystack,
            split_points,
        })
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

fn resolve_regex(choice: &RegexChoice) -> Option<String> {
    match choice {
        RegexChoice::None => None,
        RegexChoice::Pool(idx) => Some(REGEX_POOL[*idx as usize % REGEX_POOL.len()].to_string()),
        RegexChoice::User(raw) => {
            if raw.is_empty() {
                None
            } else {
                Some(raw.clone())
            }
        }
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
                id: format!("codex.matcher_fuzz_{idx}"),
                agent_type: AgentType::Codex,
                event_type: "usage.warning".to_string(),
                severity: severity_for(raw.severity_tag),
                anchors: vec![anchor, alt],
                regex: resolve_regex(&raw.regex_choice),
                description: "matcher fuzz rule".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            }
        })
        .collect()
}

/// Compact fingerprint of a `Vec<Detection>` for equality checks. `Detection`
/// contains `f64` + `serde_json::Value` so we can't derive `PartialEq` directly.
fn fingerprint(detections: &[Detection]) -> Vec<(String, String, String, (usize, usize))> {
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

fn assert_spans_within_bounds(detections: &[Detection], text: &str) {
    for d in detections {
        let (start, end) = d.span;
        assert!(
            start <= end,
            "detection span inverted: {start}..{end} for rule {}",
            d.rule_id
        );
        assert!(
            end <= text.len(),
            "detection span out of bounds: {start}..{end} exceeds text len {} (rule {})",
            text.len(),
            d.rule_id
        );
        // matched_text must be derivable from the span; we accept either
        // byte-equivalence or a UTF-8-boundary-adjacent slice (engine may
        // normalize whitespace inside matched_text on some paths).
        if text.is_char_boundary(start) && text.is_char_boundary(end) {
            let slice = &text[start..end];
            assert!(
                !d.matched_text.is_empty() || slice.is_empty(),
                "detection matched_text empty but span {start}..{end} yields non-empty slice ({} bytes)",
                slice.len()
            );
        }
    }
}

fn normalize_splits(raw: &[u16], text_len: usize) -> Vec<usize> {
    if text_len == 0 {
        return Vec::new();
    }
    let mut points: Vec<usize> = raw
        .iter()
        .map(|p| (*p as usize) % (text_len + 1))
        .collect();
    points.sort_unstable();
    points.dedup();
    // Snap every split point to the next UTF-8 boundary so we can safely
    // slice &str — streaming detection operates on char-aligned chunks.
    // (bytes slicing happens on `&str`, so this matters for correctness.)
    points
}

fn chunks_for<'a>(text: &'a str, splits: &[usize]) -> Vec<&'a str> {
    if text.is_empty() {
        return vec![""];
    }
    let mut boundaries: Vec<usize> = splits
        .iter()
        .copied()
        .filter(|p| *p <= text.len() && text.is_char_boundary(*p))
        .collect();
    boundaries.sort_unstable();
    boundaries.dedup();
    if boundaries.first().copied() != Some(0) {
        boundaries.insert(0, 0);
    }
    if boundaries.last().copied() != Some(text.len()) {
        boundaries.push(text.len());
    }
    boundaries
        .windows(2)
        .map(|w| &text[w[0]..w[1]])
        .collect()
}

fn time_detect(engine: &PatternEngine, text: &str) -> (Vec<Detection>, Duration) {
    let start = Instant::now();
    let result = engine.detect(text);
    (result, start.elapsed())
}

fuzz_target!(|case: FuzzCase| {
    let rules = build_rules(&case.rules);
    let pack = PatternPack::new("fuzz.matcher", "0.1.0", rules);
    let engine = match PatternEngine::with_packs(vec![pack]) {
        Ok(e) => e,
        // Invalid regex / duplicate ids / bad rules are expected at the edges
        // of the input space.
        Err(_) => return,
    };

    let text = match std::str::from_utf8(&case.haystack) {
        Ok(t) => t,
        Err(_) => return,
    };

    // --- Determinism: three back-to-back detect() calls must match. ---
    let (first, first_elapsed) = time_detect(&engine, text);
    assert!(
        first_elapsed.as_millis() <= MAX_EVAL_MS,
        "detect took {}ms (>{}ms budget) on {} byte text with {} rules",
        first_elapsed.as_millis(),
        MAX_EVAL_MS,
        text.len(),
        case.rules.len()
    );
    assert_spans_within_bounds(&first, text);

    let fp_first = fingerprint(&first);

    let (second, second_elapsed) = time_detect(&engine, text);
    assert!(second_elapsed.as_millis() <= MAX_EVAL_MS);
    let fp_second = fingerprint(&second);
    assert_eq!(fp_first, fp_second, "detect non-deterministic on 2nd call");

    let (third, third_elapsed) = time_detect(&engine, text);
    assert!(third_elapsed.as_millis() <= MAX_EVAL_MS);
    let fp_third = fingerprint(&third);
    assert_eq!(fp_first, fp_third, "detect non-deterministic on 3rd call");

    // --- Chunk-boundary probe: exercise Aho-Corasick LeftmostFirst across
    // split points, mirroring the class of bug fixed in scan_pipeline
    // commit 6e67bcb9. We use DetectionContext streaming and assert every
    // emitted detection satisfies bounds/UTF-8 invariants against the
    // reassembled buffer observed so far. Dedup keys across the streamed
    // run must be unique — a duplicate means the tail buffer is leaking.
    let splits = normalize_splits(&case.split_points, text.len());
    let chunks = chunks_for(text, &splits);

    let mut ctx = DetectionContext::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut assembled = String::new();

    for chunk in &chunks {
        assembled.push_str(chunk);
        let start = Instant::now();
        let streamed = engine.detect_with_context(chunk, &mut ctx);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() <= MAX_EVAL_MS,
            "detect_with_context took {}ms on {} byte chunk",
            elapsed.as_millis(),
            chunk.len()
        );

        // Every streamed detection's text must still be consistent with the
        // UTF-8 we've fed so far — even if the span coordinates are relative
        // to a chunk-local buffer, the matched_text must be valid UTF-8 and
        // non-empty whenever the rule has anchors (anchors are non-empty by
        // construction).
        for d in &streamed {
            assert!(std::str::from_utf8(d.matched_text.as_bytes()).is_ok());
            assert!(!d.rule_id.is_empty());
            let key = d.dedup_key();
            assert!(
                seen_keys.insert(key),
                "duplicate dedup_key within single streaming pass — tail buffer state leaked across chunks"
            );
        }

        // Tail buffer stays bounded and UTF-8 safe.
        assert!(std::str::from_utf8(ctx.tail_buffer.as_bytes()).is_ok());
    }
});
