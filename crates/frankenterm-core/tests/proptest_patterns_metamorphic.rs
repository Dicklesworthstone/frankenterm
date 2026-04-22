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

use frankenterm_core::pattern_trigger::TriggerScanner;
use frankenterm_core::patterns::{
    AgentType, Detection, PatternEngine, PatternPack, RuleDef, Severity,
};
use frankenterm_core::scan_pipeline::{ChunkedPipelineState, ScanPipeline, ScanPipelineConfig};
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

// ══════════════════════════════════════════════════════════════════════════
// Additional MRs — scan pipeline chunk-vs-batch + trigger-scanner level
// append monotonicity + rule reordering invariance
// ══════════════════════════════════════════════════════════════════════════
//
// The MRs above target the high-level `PatternEngine::detect` surface.
// The block below covers the layers beneath:
//
//   * `TriggerScanner` (used by `scan_pipeline` for byte-level Aho-Corasick
//     matching): adds the byte-level append-monotonicity MR that the
//     existing string-level `arb_safe_suffix` does not exercise.
//
//   * `ScanPipeline::process` vs `process_chunk + flush`: the chunked API
//     is what the live capture pipeline actually runs in production; the
//     batch API is what most tests exercise. A drift between them is the
//     classic source of "it works in unit tests but breaks in prod" bugs
//     and has bitten this module before (memory: Aho-Corasick LeftmostFirst
//     non-overlapping matching is context-dependent across chunk boundaries
//     — flush() does a batch rescan to close that gap, so the MR pins
//     that rescan as load-bearing).
//
//   * `PatternEngine::detect` under rule permutation: reordering rules
//     whose anchors don't overlap must be a no-op, because Aho-Corasick
//     output for disjoint anchors is independent of registration order.
//     A bug that lets rule order leak into the output (e.g., a HashMap
//     iteration leaking into the detection vec) fails this MR.

// ── Shared helpers for the new MRs ─────────────────────────────────────

/// Benign suffix: only ASCII digits. No default `TriggerScanner` pattern
/// contains a digit, so under `TriggerScanner::default()` this suffix is
/// guaranteed to produce zero matches on its own. Using a narrow,
/// provably-benign alphabet avoids the circular oracle where "benign"
/// is verified by the very scanner under test. (The MR body still
/// double-checks via `prop_assume!` to future-proof against pattern-set
/// changes that might introduce a digit-bearing trigger.)
fn benign_digit_suffix(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(b'0'..=b'9', 0..max_len)
}

fn any_text_bytes(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..max_len)
}

fn split_points(len: usize) -> BoxedStrategy<Vec<usize>> {
    if len < 2 {
        return Just(Vec::new()).boxed();
    }
    prop::collection::vec(1..len, 0..8)
        .prop_map(|mut v| {
            v.sort_unstable();
            v.dedup();
            v
        })
        .boxed()
}

fn slice_at_splits<'a>(bytes: &'a [u8], splits: &[usize]) -> Vec<&'a [u8]> {
    let mut chunks = Vec::with_capacity(splits.len() + 1);
    let mut prev = 0usize;
    for &cut in splits {
        chunks.push(&bytes[prev..cut]);
        prev = cut;
    }
    chunks.push(&bytes[prev..]);
    chunks
}

fn chunked_trigger_counts(
    pipeline: &ScanPipeline,
    bytes: &[u8],
    chunks: &[&[u8]],
) -> std::collections::HashMap<frankenterm_core::pattern_trigger::TriggerCategory, u64> {
    // Sanity on helper input. The test body only constructs chunk slices
    // via `slice_at_splits`, but this debug-only check catches future
    // refactors that might break the concat invariant.
    debug_assert_eq!(
        chunks
            .iter()
            .flat_map(|c| c.iter().copied())
            .collect::<Vec<u8>>(),
        bytes,
    );

    // max_buffer_bytes set to usize::MAX so `should_flush()` never fires
    // mid-stream — the MR tests the "within one buffer" equivalence.
    // Cross-flush behavior (where state.reset() drops the trigger
    // overlap) is a separate property worth covering later.
    let mut state = ChunkedPipelineState::new(usize::MAX);
    for chunk in chunks {
        let _ = pipeline.process_chunk(chunk, &mut state);
    }
    let flushed = pipeline.flush(&mut state);
    flushed.triggers.map(|t| t.counts).unwrap_or_default()
}

/// Pairwise-disjoint anchors: no anchor is a substring of another and
/// the character sets are distinct enough that Aho-Corasick cannot
/// match one rule's anchor inside another rule's anchor region. This
/// is the precondition under which MR3 (rule reordering invariance)
/// holds unconditionally; rules whose anchors overlap have
/// order-dependent matching under `LeftmostFirst`.
fn disjoint_rules() -> Vec<RuleDef> {
    vec![
        RuleDef {
            id: "codex.usage_reached".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            anchors: vec!["USAGE_LIMIT_REACHED".to_string()],
            regex: None,
            description: "usage quota hit".to_string(),
            remediation: None,
            workflow: None,
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
        },
        RuleDef {
            id: "claude_code.compaction_done".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "compaction.done".to_string(),
            severity: Severity::Info,
            anchors: vec!["COMPACTION_COMPLETE_TAG".to_string()],
            regex: None,
            description: "compaction banner".to_string(),
            remediation: None,
            workflow: None,
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
        },
        RuleDef {
            id: "gemini.rate_limited".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "rate.limited".to_string(),
            severity: Severity::Warning,
            anchors: vec!["RATELIMIT_BLOCKED_SIG".to_string()],
            regex: None,
            description: "rate limit".to_string(),
            remediation: None,
            workflow: None,
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
        },
    ]
}

fn engine_from(rules: Vec<RuleDef>) -> PatternEngine {
    let pack = PatternPack::new("mr_test".to_string(), "0.1.0".to_string(), rules);
    PatternEngine::with_packs(vec![pack]).expect("fixture rules must validate")
}

fn detection_id_set(engine: &PatternEngine, text: &str) -> Vec<String> {
    let mut ids: Vec<String> = engine.detect(text).into_iter().map(|d| d.rule_id).collect();
    ids.sort();
    ids.dedup();
    ids
}

// ── TriggerScanner MR: byte-level append-benign monotonicity ──────────

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        ..ProptestConfig::default()
    })]

    /// For the default `TriggerScanner`, appending a digits-only suffix
    /// (no default pattern contains digits) must not DECREASE the
    /// match count for any category. This is the byte-level sibling of
    /// the string-level `detect_on_text_is_submultiset...` property
    /// above and exercises the Aho-Corasick path directly, bypassing
    /// the pattern-engine's regex + quick-reject layer.
    #[test]
    fn mr_trigger_scanner_append_benign_digits_preserves_counts(
        base in any_text_bytes(2048),
        suffix in benign_digit_suffix(256),
    ) {
        let scanner = TriggerScanner::default();

        // Oracle for "benign" is the scanner itself. If the default
        // pattern set ever grows to include a digit-bearing trigger
        // this prop_assume turns the test into a no-op for that case
        // rather than a false positive.
        let suffix_result = scanner.scan_counts(&suffix);
        prop_assume!(suffix_result.total_matches == 0);

        let base_result = scanner.scan_counts(&base);
        let mut concat = base.clone();
        concat.extend_from_slice(&suffix);
        let concat_result = scanner.scan_counts(&concat);

        for (category, base_count) in &base_result.counts {
            let concat_count = concat_result.counts.get(category).copied().unwrap_or(0);
            prop_assert!(
                concat_count >= *base_count,
                "TriggerScanner append-benign violated: category {category:?} \
                 went from {base_count} to {concat_count}; benign suffix \
                 cannot erase matches. base_len={}, suffix_len={}",
                base.len(),
                suffix.len(),
            );
        }
        prop_assert!(
            concat_result.total_matches >= base_result.total_matches,
            "TriggerScanner total went from {} to {} under benign append",
            base_result.total_matches,
            concat_result.total_matches,
        );
    }
}

// ── ScanPipeline MR: chunked vs batch trigger equivalence ─────────────

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    /// Chunked `process_chunk` + `flush` must yield the same per-category
    /// trigger counts as a single `process(bytes)` on the concatenation,
    /// for both compression modes. The chunked path runs an approximate
    /// incremental scan during chunks and a DEFINITIVE batch rescan at
    /// flush() — this MR pins that rescan as the correctness anchor.
    /// If the flush rescan regresses (e.g. someone replaces the
    /// accumulated-buffer scan with a per-chunk-sum), this MR fails
    /// on any input where Aho-Corasick's LeftmostFirst behaviour
    /// differs between chunked and batch scanning.
    #[test]
    fn mr_chunked_equals_batch_compression_on(
        bytes in any_text_bytes(4096),
        splits in prop::collection::vec(0usize..4096, 0..8),
    ) {
        // Defensively keep only in-range splits and normalize.
        let mut splits: Vec<usize> =
            splits.into_iter().filter(|s| *s > 0 && *s < bytes.len()).collect();
        splits.sort_unstable();
        splits.dedup();

        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_triggers: true,
            enable_compression: true,
            ..ScanPipelineConfig::default()
        });

        let batch_counts = pipeline
            .process(&bytes)
            .triggers
            .map(|t| t.counts)
            .unwrap_or_default();

        let chunks = slice_at_splits(&bytes, &splits);
        let chunked_counts = chunked_trigger_counts(&pipeline, &bytes, &chunks);

        prop_assert_eq!(
            &batch_counts,
            &chunked_counts,
            "MR chunk-vs-batch (compression=on) violated: \
             batch {batch_counts:?} != chunked {chunked_counts:?}, \
             bytes_len={}, n_chunks={}",
            bytes.len(),
            chunks.len(),
        );
    }

    /// Same MR for the `enable_compression = false` code path, which
    /// uses `trigger_data_buffer` (as opposed to `uncompressed_buffer`)
    /// to accumulate bytes for the flush-time rescan. These are two
    /// structurally separate accumulators and both must agree with
    /// the batch result.
    #[test]
    fn mr_chunked_equals_batch_compression_off(
        bytes in any_text_bytes(4096),
        splits in prop::collection::vec(0usize..4096, 0..8),
    ) {
        let mut splits: Vec<usize> =
            splits.into_iter().filter(|s| *s > 0 && *s < bytes.len()).collect();
        splits.sort_unstable();
        splits.dedup();

        let pipeline = ScanPipeline::new(ScanPipelineConfig {
            enable_triggers: true,
            enable_compression: false,
            ..ScanPipelineConfig::default()
        });

        let batch_counts = pipeline
            .process(&bytes)
            .triggers
            .map(|t| t.counts)
            .unwrap_or_default();

        let chunks = slice_at_splits(&bytes, &splits);
        let chunked_counts = chunked_trigger_counts(&pipeline, &bytes, &chunks);

        prop_assert_eq!(
            &batch_counts,
            &chunked_counts,
            "MR chunk-vs-batch (compression=off) violated: \
             batch {batch_counts:?} != chunked {chunked_counts:?}, \
             bytes_len={}, n_chunks={}",
            bytes.len(),
            chunks.len(),
        );
    }
}

// ── PatternEngine MR: rule reordering invariance (disjoint anchors) ──

// Silence the unused-import warning produced by proptest's macro when
// split_points is defined but no proptest! block in this file calls it
// directly — kept as a helper for future MRs that want in-range splits.
#[allow(dead_code)]
fn _keep_split_points_alive() -> BoxedStrategy<Vec<usize>> {
    split_points(16)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    /// Permuting rules whose anchors are pairwise disjoint must not
    /// change the set of rule_ids that fire. Disjointness is the
    /// precondition that makes Aho-Corasick's output rule-order
    /// independent — any bug that lets registration order leak into
    /// the detection path (e.g. HashMap iteration, unsorted-vec
    /// output) fails this MR.
    #[test]
    fn mr_rule_reordering_invariance_disjoint_anchors(
        include_a in any::<bool>(),
        include_b in any::<bool>(),
        include_c in any::<bool>(),
        seg0 in "[A-Za-z0-9 ]{0,32}",
        seg1 in "[A-Za-z0-9 ]{0,32}",
        seg2 in "[A-Za-z0-9 ]{0,32}",
        seg3 in "[A-Za-z0-9 ]{0,32}",
        perm_idx in 0usize..6,
    ) {
        let anchor_a = "USAGE_LIMIT_REACHED";
        let anchor_b = "COMPACTION_COMPLETE_TAG";
        let anchor_c = "RATELIMIT_BLOCKED_SIG";

        // Disjointness precondition. If this ever trips, the MR's
        // fixture is broken and the whole test is meaningless.
        prop_assert!(!anchor_a.contains(anchor_b) && !anchor_b.contains(anchor_a));
        prop_assert!(!anchor_a.contains(anchor_c) && !anchor_c.contains(anchor_a));
        prop_assert!(!anchor_b.contains(anchor_c) && !anchor_c.contains(anchor_b));

        let mut text = String::new();
        text.push_str(&seg0);
        if include_a { text.push_str(anchor_a); }
        text.push_str(&seg1);
        if include_b { text.push_str(anchor_b); }
        text.push_str(&seg2);
        if include_c { text.push_str(anchor_c); }
        text.push_str(&seg3);

        let rules = disjoint_rules();

        let permutations: [[usize; 3]; 6] = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let perm = permutations[perm_idx];

        let canonical = engine_from(rules.clone());
        let permuted_rules: Vec<RuleDef> = perm.iter().map(|&i| rules[i].clone()).collect();
        let permuted = engine_from(permuted_rules);

        let canonical_ids = detection_id_set(&canonical, &text);
        let permuted_ids = detection_id_set(&permuted, &text);

        prop_assert_eq!(
            &canonical_ids,
            &permuted_ids,
            "MR3 violated: rule permutation {perm:?} changed detection set \
             from {canonical_ids:?} to {permuted_ids:?} on text of length {}",
            text.len(),
        );
    }
}
