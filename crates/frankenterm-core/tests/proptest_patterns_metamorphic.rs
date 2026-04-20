//! Metamorphic relations for `PatternEngine::detect`.
//!
//! Existing `proptest_patterns.rs` covers constructors and serde for
//! the pattern data types (AgentType, Severity, Detection, RuleDef,
//! PatternPack, PatternLibrary, trace types). It does NOT exercise
//! input→output relations on the live `PatternEngine::detect` API,
//! which is the classic oracle-problem surface: for arbitrary text
//! there is no reference answer, but several invariants about how
//! the engine reacts to input transformations must hold.
//!
//! This harness pins two metamorphic relations:
//!
//! 1. **Determinism** — for any `text`, two successive calls to
//!    `engine.detect(text)` must produce multisets of detections
//!    that agree on `(rule_id, matched_text, canonical_extracted)`.
//!    Any drift here indicates shared mutable state in the engine
//!    leaking across calls.
//!
//! 2. **Suffix superset (append-monotonicity)** — appending a
//!    "safe" suffix (a sentinel string that cannot itself combine
//!    with existing text to form a new rule anchor) to the input
//!    must not LOSE any detection that fired on the shorter input.
//!    Formally, the multiset of `(rule_id, canonical_extracted,
//!    matched_text)` for `detect(text)` must be a sub-multiset of
//!    `detect(text ++ suffix)`.
//!
//!    This catches regressions where an internal buffer slicing,
//!    Aho-Corasick recycling, or quick-reject short-circuit starts
//!    silently dropping matches when the input grows.
//!
//! Domain: patterns metamorphic (pane 5).

use frankenterm_core::patterns::{Detection, PatternEngine};
use proptest::prelude::*;
use std::collections::BTreeMap;

// ── Canonicalization ────────────────────────────────────────────────────

/// Collapse a `Detection` down to the fields that are span-independent
/// and multiset-comparable. The live `span` field varies when we move
/// a match forward by prepending text or leave it alone when we append
/// a suffix, so it's excluded — the identity under our MRs is
/// `(rule_id, matched_text, canonical_extracted)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct DetectionFingerprint {
    rule_id: String,
    matched_text: String,
    extracted_canonical: String,
}

fn fingerprint(detection: &Detection) -> DetectionFingerprint {
    DetectionFingerprint {
        rule_id: detection.rule_id.clone(),
        matched_text: detection.matched_text.clone(),
        extracted_canonical: canonicalize_json(&detection.extracted),
    }
}

fn canonicalize_json(value: &serde_json::Value) -> String {
    // Sort object keys recursively so comparisons aren't sensitive to
    // HashMap iteration order in the serializer.
    fn canon(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut sorted: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(b.0));
                let mut out = serde_json::Map::new();
                for (k, v) in sorted {
                    out.insert(k.clone(), canon(v));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.iter().map(canon).collect())
            }
            other => other.clone(),
        }
    }
    serde_json::to_string(&canon(value)).expect("canonicalize extracted JSON")
}

/// Turn a slice of detections into a stable (sorted) multiset keyed
/// by `DetectionFingerprint`.
fn detection_multiset(detections: &[Detection]) -> BTreeMap<DetectionFingerprint, usize> {
    let mut map: BTreeMap<DetectionFingerprint, usize> = BTreeMap::new();
    for detection in detections {
        *map.entry(fingerprint(detection)).or_insert(0) += 1;
    }
    map
}

// ── Strategies ──────────────────────────────────────────────────────────

/// Arbitrary text biased toward anchors that actually appear in the
/// built-in Codex / Claude Code / Gemini packs so many generated
/// cases drive real detections rather than hitting the quick-reject
/// short-circuit.
fn arb_pattern_rich_text() -> impl Strategy<Value = String> {
    let seeds: Vec<&'static str> = vec![
        "less than 25% of your 5h limit remaining",
        "less than 10% of your 5h limit remaining",
        "less than 5% of your 5h limit remaining",
        "You've hit your usage limit, try again at 9:00pm.",
        "You've reached your usage limit, try again at tomorrow 9am.",
        "Token usage: total=1,234 input=1,000 (+ 200 cached) output=34",
        "codex resume 12345678-1234-1234-1234-123456789012",
        "rate limit exceeded",
        "429 Too Many Requests",
        "boring noise",
        "",
        "\n\n\n",
        "some output line",
    ];
    proptest::collection::vec(proptest::sample::select(seeds), 0..6)
        .prop_map(|lines| lines.join("\n"))
}

/// "Safe" suffixes that can be appended to an input without risk of
/// retroactively forming a new rule anchor across the join. The
/// built-in packs' anchors are prose like "less than 5%", "You've hit
/// your usage limit", "Token usage:", etc.; surrounding a message
/// with bracketed delimiters like "\n[END]\n" or many newlines cannot
/// combine with any reasonable prefix to spell one of those anchors.
fn arb_safe_suffix() -> impl Strategy<Value = String> {
    let suffixes: Vec<&'static str> = vec![
        "\n",
        "\n\n",
        "\n[END]\n",
        "\n---\n",
        "\n>>> ",
        "\n!!!\n",
        "\n^^^\n",
    ];
    proptest::collection::vec(proptest::sample::select(suffixes), 0..4)
        .prop_map(|parts| parts.concat())
}

// ── Properties ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// MR1: Determinism.
    ///
    /// Two successive `detect` calls on the same engine + same text
    /// must produce identical detection multisets.
    #[test]
    fn detect_is_deterministic_across_repeated_calls(text in arb_pattern_rich_text()) {
        let engine = PatternEngine::new();
        let first = engine.detect(&text);
        let second = engine.detect(&text);
        prop_assert_eq!(
            detection_multiset(&first),
            detection_multiset(&second),
            "detect(text) must be deterministic across repeated calls"
        );
    }

    /// MR1-cross: determinism across freshly-constructed engines.
    ///
    /// A second, independently constructed engine must produce the
    /// same detection multiset for the same input — the engine must
    /// not depend on any state that differs between constructions
    /// (seed drift, AtomicU64 ordering, etc.).
    #[test]
    fn detect_agrees_across_fresh_engines(text in arb_pattern_rich_text()) {
        let engine_a = PatternEngine::new();
        let engine_b = PatternEngine::new();
        let a = engine_a.detect(&text);
        let b = engine_b.detect(&text);
        prop_assert_eq!(
            detection_multiset(&a),
            detection_multiset(&b),
            "two fresh PatternEngine instances must agree on detections"
        );
    }

    /// MR2: Suffix superset (append-monotonicity).
    ///
    /// For any text T and any safe suffix S, the detection multiset
    /// on T must be a sub-multiset of the detection multiset on
    /// T ++ S. A rule that fired on T cannot silently vanish when
    /// the input grows — at worst, the longer input adds new matches.
    #[test]
    fn detect_on_text_is_submultiset_of_detect_on_text_plus_safe_suffix(
        text in arb_pattern_rich_text(),
        suffix in arb_safe_suffix(),
    ) {
        let engine = PatternEngine::new();
        let base = detection_multiset(&engine.detect(&text));

        let mut extended = text.clone();
        extended.push_str(&suffix);
        let grown = detection_multiset(&engine.detect(&extended));

        for (fp, base_count) in &base {
            let grown_count = grown.get(fp).copied().unwrap_or(0);
            prop_assert!(
                grown_count >= *base_count,
                "detection {fp:?} had {base_count} hits on `{text}` but only \
                 {grown_count} after appending {suffix:?}; append-monotonicity violated"
            );
        }
    }

    /// MR2-symmetric: prepend-monotonicity using the same safe delimiter
    /// corpus on the front instead of the back. Catches the same class
    /// of regressions in any code path that special-cases the start of
    /// the input (e.g., buffer wrap-around, tail-buffer prefix logic).
    #[test]
    fn detect_on_text_is_submultiset_of_detect_on_safe_prefix_plus_text(
        text in arb_pattern_rich_text(),
        prefix in arb_safe_suffix(),
    ) {
        let engine = PatternEngine::new();
        let base = detection_multiset(&engine.detect(&text));

        let mut extended = prefix.clone();
        extended.push_str(&text);
        let grown = detection_multiset(&engine.detect(&extended));

        for (fp, base_count) in &base {
            let grown_count = grown.get(fp).copied().unwrap_or(0);
            prop_assert!(
                grown_count >= *base_count,
                "detection {fp:?} had {base_count} hits on `{text}` but only \
                 {grown_count} after prepending {prefix:?}; prepend-monotonicity violated"
            );
        }
    }
}

// ── Hand-rolled regressions ─────────────────────────────────────────────

#[test]
fn detect_empty_text_is_empty() {
    let engine = PatternEngine::new();
    assert!(engine.detect("").is_empty());
}

#[test]
fn detect_is_deterministic_on_known_rate_limit_anchor() {
    let engine = PatternEngine::new();
    let text = "429 Too Many Requests — rate limit exceeded, try again at 5:00pm.";
    let a = engine.detect(text);
    let b = engine.detect(text);
    assert_eq!(
        detection_multiset(&a),
        detection_multiset(&b),
        "rate-limit-anchor detect must be deterministic"
    );
}

#[test]
fn append_monotonicity_on_known_usage_limit_anchor() {
    let engine = PatternEngine::new();
    let text = "You've hit your usage limit, try again at 9:00pm.";
    let base = detection_multiset(&engine.detect(text));

    let extended = format!("{text}\n[END]\n");
    let grown = detection_multiset(&engine.detect(&extended));

    for (fp, base_count) in &base {
        let grown_count = grown.get(fp).copied().unwrap_or(0);
        assert!(
            grown_count >= *base_count,
            "detection {fp:?} vanished when safe suffix was appended: \
             base={base_count} grown={grown_count}"
        );
    }
}
