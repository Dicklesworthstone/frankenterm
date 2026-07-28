//! Byte-equivalence proof for the Q3 moonshot
//! ([`frankenterm_core::ingest`] `ingest.delta_linear_overlap`).
//!
//! The Q3 gauntlet experiment replaces the legacy O(n²) overlap search in
//! `extract_delta` — a `for pos in memchr_iter(..) { slice_compare }` loop whose
//! worst case is `search_window.len() * current.len()` when the first byte of
//! `current` repeats across the bounded window — with a single forward-pass
//! Knuth–Morris–Pratt longest-suffix(`search_window`)/prefix(`current`) match.
//!
//! The keep-gate contract (rule #7, "behavior byte-identical") demands the two
//! algorithms be *observably indistinguishable*: identical [`DeltaResult`] for
//! every input, **including the `Gap` `reason` strings**. This harness proves
//! that by driving both arms through the gate-bypassing
//! [`extract_delta_with_overlap_mode`] entry point and comparing canonicalized
//! results on:
//!
//! * arbitrary UTF-8 (1/2/3/4-byte codepoints),
//! * box-drawing glyphs (U+2500–U+257F, 3-byte) at the overlap boundary,
//! * emoji (U+1F300–U+1FAFF, 4-byte) at the overlap boundary,
//! * sliding-window pairs (the real scrollback-capture shape that reaches the
//!   overlap search), and
//! * adversarial repeated-first-byte runs (the legacy path's quadratic worst
//!   case, where every window byte is a memchr hit).
//!
//! It also pins the gate's **default-off** state (keep-gate rule #4): with the
//! `FT_MOONSHOT_DELTA_LINEAR_OVERLAP` env var unset, the public [`extract_delta`]
//! must equal the legacy quadratic arm.
//!
//! Domain: ingest delta extraction — Q3 alien-optimization equivalence.

use frankenterm_core::ingest::{
    DeltaResult, delta_linear_overlap_enabled, extract_delta, extract_delta_with_overlap_mode,
};
use proptest::prelude::*;

/// Collapse a [`DeltaResult`] into a fully-comparable `(tag, a, b)` tuple so
/// proptest can assert exact equality of *every* observable field — variant,
/// payload, and (critically) the `Gap` `reason` string — without adding a
/// `PartialEq` derive to the public enum.
fn canon(r: &DeltaResult) -> (u8, String, String) {
    match r {
        DeltaResult::NoChange => (0, String::new(), String::new()),
        DeltaResult::Content(delta) => (1, delta.clone(), String::new()),
        DeltaResult::Gap { reason, content } => (2, reason.clone(), content.clone()),
    }
}

/// Assert the linear (KMP) and quadratic (legacy memchr) arms agree exactly.
fn assert_arms_agree(
    previous: &str,
    current: &str,
    overlap_size: usize,
) -> Result<(), TestCaseError> {
    let quadratic = extract_delta_with_overlap_mode(previous, current, overlap_size, false);
    let linear = extract_delta_with_overlap_mode(previous, current, overlap_size, true);
    prop_assert_eq!(
        canon(&quadratic),
        canon(&linear),
        "linear != quadratic for previous={:?} current={:?} overlap_size={}: \
         quadratic={:?} linear={:?}",
        previous,
        current,
        overlap_size,
        quadratic,
        linear
    );
    Ok(())
}

// ── Strategies ──────────────────────────────────────────────────────────

/// A char mix spanning all four UTF-8 encoded lengths, weighted toward the
/// multi-byte cases that exercise char-boundary snapping in the overlap window.
fn arb_char() -> impl Strategy<Value = char> {
    prop_oneof![
        10 => (0x20u32..=0x7eu32).prop_filter_map("ascii (1B)", char::from_u32),
        4  => (0x80u32..=0x07ffu32).prop_filter_map("latin/cyrillic (2B)", char::from_u32),
        4  => (0x2500u32..=0x257fu32).prop_filter_map("box-drawing (3B)", char::from_u32),
        3  => (0x1f300u32..=0x1faffu32).prop_filter_map("emoji (4B)", char::from_u32),
    ]
}

fn arb_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(arb_char(), 0..64).prop_map(|chars| chars.into_iter().collect())
}

/// Overlap-size mix: zero, small values that frequently land mid-codepoint
/// (stressing the boundary snap), realistic window sizes, and a huge value.
fn arb_overlap_size() -> impl Strategy<Value = usize> {
    prop_oneof![
        1 => Just(0usize),
        2 => 1usize..=8,
        4 => 1usize..=256,
        1 => Just(usize::MAX / 2),
    ]
}

/// Sliding-window shape: `previous = prefix + shared`, `current = shared + suffix`.
/// This is the scrollback-capture shape that actually reaches the overlap search.
fn arb_sliding_window_pair() -> impl Strategy<Value = (String, String)> {
    (arb_text(), arb_text(), arb_text()).prop_map(|(prefix, shared, suffix)| {
        (format!("{prefix}{shared}"), format!("{shared}{suffix}"))
    })
}

/// Adversarial repeated-first-byte run: `previous = head + run`,
/// `current = run + tail` where `run` is a single char repeated. Every byte of
/// the run is a `memchr` hit for the legacy loop — its quadratic worst case —
/// while the KMP arm stays linear. Equivalence here is the load-bearing case.
fn arb_repeated_run_pair() -> impl Strategy<Value = (String, String)> {
    (arb_char(), 1usize..32, arb_text(), arb_text()).prop_map(|(c, n, head, tail)| {
        let run: String = std::iter::repeat(c).take(n).collect();
        (format!("{head}{run}"), format!("{run}{tail}"))
    })
}

// ── Properties ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Arbitrary UTF-8 pairs: the two arms must produce identical results.
    #[test]
    fn linear_equals_quadratic_on_arbitrary(
        previous in arb_text(),
        current in arb_text(),
        overlap_size in arb_overlap_size(),
    ) {
        assert_arms_agree(&previous, &current, overlap_size)?;
    }

    /// Sliding-window pairs (multi-byte glyphs at the boundary) — the shape that
    /// genuinely hits the overlap-search branch.
    #[test]
    fn linear_equals_quadratic_on_sliding_window(
        (previous, current) in arb_sliding_window_pair(),
        overlap_size in arb_overlap_size(),
    ) {
        assert_arms_agree(&previous, &current, overlap_size)?;
    }

    /// Repeated-first-byte runs — the legacy loop's O(n²) worst case.
    #[test]
    fn linear_equals_quadratic_on_repeated_runs(
        (previous, current) in arb_repeated_run_pair(),
        overlap_size in arb_overlap_size(),
    ) {
        assert_arms_agree(&previous, &current, overlap_size)?;
    }

    /// Default-off contract: with the gate unset, the public `extract_delta`
    /// must equal the legacy quadratic arm (keep-gate rule #4).
    #[test]
    fn public_entry_defaults_to_quadratic(
        previous in arb_text(),
        current in arb_text(),
        overlap_size in arb_overlap_size(),
    ) {
        let public = extract_delta(&previous, &current, overlap_size);
        let quadratic = extract_delta_with_overlap_mode(&previous, &current, overlap_size, false);
        prop_assert_eq!(canon(&public), canon(&quadratic));
    }
}

// ── Hand-rolled regressions on known-hard, semantically-pinned cases ─────

#[test]
fn gate_defaults_off() {
    // No FT_MOONSHOT_DELTA_LINEAR_OVERLAP set in the test process → off.
    assert!(
        !delta_linear_overlap_enabled(),
        "ingest.delta_linear_overlap must default OFF"
    );
}

/// Each case asserts (a) both arms agree and (b) the concrete expected variant,
/// documenting the overlap semantics the equivalence rests on.
#[test]
fn pinned_overlap_cases_agree_and_match_expectation() {
    let cases: &[(&str, &str, usize, DeltaResult)] = &[
        // Sliding-window ASCII: previous ends with "world", current starts with it.
        (
            "hello world",
            "world peace",
            64,
            DeltaResult::Content(" peace".to_string()),
        ),
        // Box-drawing (3-byte) overlap boundary: shared "│─" tail/head.
        ("ab│─", "│─cd", 64, DeltaResult::Content("cd".to_string())),
        // Emoji (4-byte) overlap boundary: shared "🚀" tail/head.
        ("x🚀", "🚀y", 64, DeltaResult::Content("y".to_string())),
        // Full overlap (current is wholly a suffix of previous) → the delta is
        // empty, so the algorithm reports content_changed_without_append.
        (
            "zzabc",
            "abc",
            64,
            DeltaResult::Gap {
                reason: "content_changed_without_append".to_string(),
                content: "abc".to_string(),
            },
        ),
        // No overlap at all → overlap_not_found, content == current.
        (
            "abc",
            "xyz",
            64,
            DeltaResult::Gap {
                reason: "overlap_not_found".to_string(),
                content: "xyz".to_string(),
            },
        ),
        // Repeated-first-byte adversarial run with a genuine 3-char overlap.
        (
            "head aaa",
            "aaa tail",
            64,
            DeltaResult::Content(" tail".to_string()),
        ),
    ];

    for (previous, current, overlap_size, expected) in cases {
        let quadratic = extract_delta_with_overlap_mode(previous, current, *overlap_size, false);
        let linear = extract_delta_with_overlap_mode(previous, current, *overlap_size, true);
        assert_eq!(
            canon(&quadratic),
            canon(&linear),
            "arms disagree for previous={previous:?} current={current:?}"
        );
        assert_eq!(
            canon(&linear),
            canon(expected),
            "linear result mismatch for previous={previous:?} current={current:?}"
        );
    }
}
