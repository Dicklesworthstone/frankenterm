//! Structure-aware fuzz harness for
//! [`frankenterm_core::ingest::extract_delta`].
//!
//! The delta-extraction algorithm is the hot path that turns each
//! WezTerm scrollback snapshot into an append-only `output_segments`
//! row. A regression that panics on multi-byte UTF-8 at the overlap
//! boundary, returns an empty `Content(..)` delta, or emits a delta
//! that isn't a suffix of `current` would corrupt every pane's
//! segment history without a loud failure mode. This harness pins
//! the four correctness contracts the algorithm must satisfy on
//! arbitrary input:
//!
//! 1. **Crash-freedom** — `extract_delta` must return a
//!    `DeltaResult` for any `(previous, current, overlap_size)`
//!    triple, including empty strings, interior multi-byte UTF-8,
//!    mid-codepoint overlap windows, and arbitrary `usize`
//!    overlap sizes up to the input length.
//!
//! 2. **Identity → NoChange** — `extract_delta(x, x, n)` must
//!    return `NoChange` for any `n`. Regressions that stop
//!    short-circuiting on equality would flood storage with
//!    redundant `Content` rows.
//!
//! 3. **Content suffix invariant** — when the algorithm returns
//!    `Content(delta)`, `current.ends_with(delta)` must hold and
//!    `delta` must be non-empty. Pins the published contract that
//!    the extracted payload is always a true suffix of the new
//!    snapshot.
//!
//! 4. **Pure-append recoverability** — when `current.starts_with(
//!    previous)` (a char-aligned append) and `previous != current`,
//!    the algorithm must return `Content(current[previous.len()..])`.
//!    This is the fast-path guarantee the comment on line 1665 of
//!    `ingest.rs` relies on.
//!
//! 5. **Determinism** — identical inputs must produce identical
//!    outputs across repeated calls.
//!
//! Domain: ingest delta extraction fuzzing (pane 5).

use frankenterm_core::ingest::{DeltaResult, extract_delta};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────

/// Arbitrary UTF-8 text of bounded length. Exercises the full
/// char-boundary search logic.
fn arb_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..128).prop_map(|chars| chars.into_iter().collect())
}

/// Narrow strategy: ASCII text with occasional multi-byte codepoints
/// to focus shrinking on the interesting char-boundary cases.
fn arb_ascii_with_multibyte() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            8 => (0x20u32..=0x7eu32).prop_filter_map("ascii", char::from_u32),
            3 => (0x80u32..=0x4ffu32).prop_filter_map("latin-extended", char::from_u32),
            1 => (0x2500u32..=0x257fu32).prop_filter_map("box-drawing", char::from_u32),
        ],
        0..96,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

/// Overlap-size strategy — often close to the text length to stress
/// the bounded-window code, sometimes zero, sometimes huge.
fn arb_overlap_size() -> impl Strategy<Value = usize> {
    prop_oneof![
        1 => Just(0usize),
        4 => 1usize..=256,
        1 => Just(usize::MAX / 2),
    ]
}

/// Structure-aware strategy: build `(previous, current)` where
/// `current == previous + suffix` — the pure-append corpus the
/// fast path is optimized for.
fn arb_append_pair() -> impl Strategy<Value = (String, String)> {
    (arb_ascii_with_multibyte(), arb_ascii_with_multibyte()).prop_map(|(previous, suffix)| {
        let current = format!("{previous}{suffix}");
        (previous, current)
    })
}

/// Structure-aware strategy: build `(previous, current)` where
/// `current` shares a tail window with `previous` — the sliding-
/// window scrollback shape. Formally: previous = A + shared;
/// current = shared + B.
fn arb_sliding_window_pair() -> impl Strategy<Value = (String, String)> {
    (
        arb_ascii_with_multibyte(), // prefix that scrolls off
        arb_ascii_with_multibyte(), // shared overlap window
        arb_ascii_with_multibyte(), // newly-scrolled-in suffix
    )
        .prop_map(|(prefix, shared, suffix)| {
            let previous = format!("{prefix}{shared}");
            let current = format!("{shared}{suffix}");
            (previous, current)
        })
}

// ── Properties ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    // ── MR1: crash-freedom ──────────────────────────────────────────

    /// Arbitrary UTF-8 text must never panic the algorithm.
    #[test]
    fn extract_delta_never_panics_on_arbitrary_text(
        previous in arb_text(),
        current in arb_text(),
        overlap_size in arb_overlap_size(),
    ) {
        let _ = extract_delta(&previous, &current, overlap_size);
    }

    /// Structure-aware append pairs must never panic.
    #[test]
    fn extract_delta_never_panics_on_append_shape(
        (previous, current) in arb_append_pair(),
        overlap_size in arb_overlap_size(),
    ) {
        let _ = extract_delta(&previous, &current, overlap_size);
    }

    /// Sliding-window pairs with multi-byte UTF-8 must never panic —
    /// this exercises the char-boundary logic on the overlap search.
    #[test]
    fn extract_delta_never_panics_on_sliding_window_shape(
        (previous, current) in arb_sliding_window_pair(),
        overlap_size in arb_overlap_size(),
    ) {
        let _ = extract_delta(&previous, &current, overlap_size);
    }

    // ── MR2: identity → NoChange ───────────────────────────────────

    /// `extract_delta(x, x, n)` must always be `NoChange` regardless
    /// of `overlap_size`.
    #[test]
    fn extract_delta_identity_yields_nochange(
        text in arb_text(),
        overlap_size in arb_overlap_size(),
    ) {
        let result = extract_delta(&text, &text, overlap_size);
        prop_assert!(
            matches!(result, DeltaResult::NoChange),
            "identity input must yield NoChange, got {:?}",
            result
        );
    }

    // ── MR3: Content suffix invariant ──────────────────────────────

    /// When `Content(delta)` is returned, `delta` must be a non-empty
    /// suffix of `current`.
    #[test]
    fn extract_delta_content_is_nonempty_suffix_of_current(
        previous in arb_text(),
        current in arb_text(),
        overlap_size in arb_overlap_size(),
    ) {
        if let DeltaResult::Content(delta) = extract_delta(&previous, &current, overlap_size) {
            prop_assert!(
                !delta.is_empty(),
                "Content variant must carry a non-empty delta"
            );
            prop_assert!(
                current.ends_with(&delta),
                "Content delta {:?} must be a suffix of current {:?}",
                delta,
                current
            );
        }
    }

    // ── MR4: pure-append recoverability ────────────────────────────

    /// When `current` starts with `previous` on a char boundary and
    /// `previous != current`, the fast path must return
    /// `Content(current[previous.len()..])`. This is the documented
    /// fast-path guarantee.
    #[test]
    fn extract_delta_pure_append_returns_exact_suffix(
        (previous, current) in arb_append_pair(),
        overlap_size in arb_overlap_size(),
    ) {
        prop_assume!(previous != current); // NoChange branch handled by MR2
        prop_assume!(current.is_char_boundary(previous.len())); // algorithm guard

        let expected_suffix = &current[previous.len()..];
        prop_assume!(!expected_suffix.is_empty()); // empty suffix means prev == curr

        let result = extract_delta(&previous, &current, overlap_size);
        match result {
            DeltaResult::Content(delta) => {
                prop_assert_eq!(
                    delta.as_str(),
                    expected_suffix,
                    "pure-append fast path must return current[previous.len()..]"
                );
                // Reconstruction check: prev + delta == curr.
                let reconstructed = format!("{}{}", previous, delta);
                prop_assert_eq!(
                    reconstructed,
                    current,
                    "prev + delta must reconstruct current on the pure-append path"
                );
            }
            other => prop_assert!(
                false,
                "pure-append pair must yield Content, got {:?}",
                other
            ),
        }
    }

    // ── MR5: determinism ───────────────────────────────────────────

    /// Two successive calls with identical inputs must produce the
    /// same DeltaResult shape and payload.
    #[test]
    fn extract_delta_is_deterministic(
        previous in arb_text(),
        current in arb_text(),
        overlap_size in arb_overlap_size(),
    ) {
        let a = extract_delta(&previous, &current, overlap_size);
        let b = extract_delta(&previous, &current, overlap_size);
        match (&a, &b) {
            (DeltaResult::NoChange, DeltaResult::NoChange) => {}
            (DeltaResult::Content(da), DeltaResult::Content(db)) => {
                prop_assert_eq!(da, db, "Content payload must match across calls");
            }
            (
                DeltaResult::Gap { reason: ra, content: ca },
                DeltaResult::Gap { reason: rb, content: cb },
            ) => {
                prop_assert_eq!(ra, rb, "Gap reason must match across calls");
                prop_assert_eq!(ca, cb, "Gap content must match across calls");
            }
            _ => prop_assert!(
                false,
                "extract_delta variant drift across calls: a={:?} b={:?}",
                a,
                b
            ),
        }
    }

    /// Zero-overlap degrades to Gap when previous != current and
    /// previous is non-empty. Pins the branch contract at line 1675.
    #[test]
    fn extract_delta_zero_overlap_yields_gap_when_mismatched(
        previous in arb_ascii_with_multibyte().prop_filter("non-empty", |s| !s.is_empty()),
        current in arb_text(),
    ) {
        prop_assume!(previous != current);
        // Avoid the fast append path, which is reached before the
        // overlap_size == 0 check.
        prop_assume!(
            !(current.len() > previous.len()
                && current.starts_with(&previous)
                && current.is_char_boundary(previous.len()))
        );

        let result = extract_delta(&previous, &current, 0);
        prop_assert!(
            matches!(result, DeltaResult::Gap { .. }),
            "zero-overlap mismatched pair must yield Gap, got {:?}",
            result
        );
    }
}

// ── Hand-rolled regressions on known-hard cases ─────────────────────────

#[test]
fn extract_delta_empty_previous_with_non_empty_current_returns_full_content() {
    match extract_delta("", "hello", 16) {
        DeltaResult::Content(delta) => assert_eq!(delta, "hello"),
        other => panic!("expected Content, got {other:?}"),
    }
}

#[test]
fn extract_delta_both_empty_returns_nochange() {
    assert!(matches!(extract_delta("", "", 16), DeltaResult::NoChange));
}

#[test]
fn extract_delta_pure_append_returns_suffix() {
    match extract_delta("abc", "abcdef", 32) {
        DeltaResult::Content(delta) => assert_eq!(delta, "def"),
        other => panic!("expected Content, got {other:?}"),
    }
}

#[test]
fn extract_delta_multibyte_boundary_does_not_panic() {
    // Cyrillic char (2B): tests the char_boundary snap on the
    // fast-append branch.
    let previous = "привет";
    let current = "приветмир";
    let result = extract_delta(previous, current, 32);
    match result {
        DeltaResult::Content(delta) => assert_eq!(delta, "мир"),
        other => panic!("expected Content('мир'), got {other:?}"),
    }
}

#[test]
fn extract_delta_sliding_window_mid_codepoint_does_not_panic() {
    // Overlap size in the middle of a multi-byte codepoint must
    // snap forward to the next char boundary, never panic.
    let previous = "αβγδεζηθ";
    let current = "γδεζηθικ";
    // overlap_size chosen so search_start lands mid-codepoint
    let _ = extract_delta(previous, current, 5);
}

#[test]
fn extract_delta_in_place_edit_yields_gap() {
    // Same length, different middle → overlap_not_found or
    // content_changed_without_append.
    let result = extract_delta("aaaaa", "bbbbb", 8);
    assert!(matches!(result, DeltaResult::Gap { .. }));
}
