//! INV-INGEST-1 — overlap byte-conservation (no double-count) property tests.
//!
//! Gap closed: `extract_delta` dedups the 4 KB sliding-window overlap *by
//! construction*, and suffix-invariants are already fuzzed
//! (`proptest_ingest_extract_delta_fuzz.rs` MR3/MR3b/MR8). What was missing — and
//! what this file adds — is a **multi-step byte-conservation** property: feeding
//! a *sequence* of successive captures through `extract_delta` and asserting that
//! reconstructing from `capture_0 + Σ delta_i` never double-counts the overlapping
//! bytes (and, for pure appends, reproduces the stream exactly).
//!
//! Provenance: gauntlet FND-007 / EXP-005 / CH-004 (frankenterm__gauntlet_workspace,
//! Round 1 CONFIRMED_GAP). Verifier-confirmed: only example-based unit tests existed
//! (ingest.rs:3308,3385); no property fed N overlapping captures.
//!
//! The `mutation_check_*` test is a non-vacuity guard: it shows a deliberately
//! double-counting reconstruction is *detected* by the conservation metric, so the
//! properties below are not trivially true.

use frankenterm_core::ingest::{DeltaResult, extract_delta};
use proptest::prelude::*;

/// Mirror the real sliding-window overlap budget (4 KB). `extract_delta` clamps
/// this to `min(prev.len(), cur.len())`, so it is always "large enough" for the
/// small strings these properties generate.
const OVERLAP: usize = 4096;

/// Apply one capture transition to a running reconstruction.
///
/// Returns the (possibly grown) reconstruction. On `Gap` we resync to the gap's
/// full content (the documented capture-discontinuity behavior) — modeling what
/// the real cursor does when overlap matching fails.
fn apply_step(recon: String, prev: &str, cur: &str) -> (String, DeltaResult) {
    let result = extract_delta(prev, cur, OVERLAP);
    let next = match &result {
        DeltaResult::Content(delta) => {
            let mut r = recon;
            r.push_str(delta);
            r
        }
        DeltaResult::NoChange => recon,
        DeltaResult::Gap { content, .. } => content.clone(),
    };
    (next, result)
}

proptest! {
    /// PURE-APPEND CONSERVATION (exact). A stream that only ever appends (the
    /// dominant terminal-output shape) must reconstruct byte-for-byte from the
    /// first capture plus the concatenation of deltas, with *each* delta equal to
    /// exactly the appended chunk — no overlap byte is ever re-emitted.
    #[test]
    fn prop_append_sequence_conserves_bytes(
        chunks in proptest::collection::vec("[a-z ]{1,40}", 2..8usize)
    ) {
        // Build successive append captures: captures[i] = chunks[0..=i] concatenated.
        let mut captures: Vec<String> = Vec::with_capacity(chunks.len());
        let mut acc = String::new();
        for c in &chunks {
            acc.push_str(c);
            captures.push(acc.clone());
        }
        let full = captures.last().unwrap().clone();

        let mut recon = captures[0].clone();
        for i in 1..captures.len() {
            let (next, result) = apply_step(recon, &captures[i - 1], &captures[i]);
            recon = next;
            // Each append step must yield exactly the new chunk (no overlap re-emit).
            match result {
                DeltaResult::Content(delta) => {
                    prop_assert_eq!(&delta, &chunks[i], "append delta must equal the new chunk only");
                }
                other => prop_assert!(false, "expected Content on pure append, got {:?}", other),
            }
        }
        // No bytes lost, none duplicated.
        prop_assert_eq!(&recon, &full, "reconstruction must equal the final capture exactly");
    }

    /// SLIDING-WINDOW NO-DOUBLE-COUNT. A window of width `w` slides forward by
    /// `step` over `full`. For arbitrary content (including repeats) the
    /// reconstruction may *lose* bytes (over-match on repeats → shorter delta →
    /// `Gap` resync) but must NEVER *inflate*: the running reconstruction length
    /// can never exceed the high-water content length. Additionally, every
    /// `Content` delta must be a genuine non-overlapping tail — the bytes that
    /// precede it in `cur` were already present as a suffix of `prev`.
    #[test]
    fn prop_sliding_window_never_double_counts(
        full in "[a-z]{20,300}",
        w in 5usize..=20,
        step in 1usize..=20,
    ) {
        let step = step.min(w); // step never exceeds the window
        let len = full.len();
        // Build sliding windows: [i*step .. min(i*step + w, len)].
        let mut starts: Vec<usize> = Vec::new();
        let mut s = 0usize;
        while s < len {
            starts.push(s);
            if s + w >= len { break; }
            s += step;
        }
        let captures: Vec<&str> = starts.iter().map(|&st| &full[st..(st + w).min(len)]).collect();
        prop_assume!(captures.len() >= 2);

        let high_water = (starts.last().unwrap() + w).min(len); // max real content end

        let mut recon = captures[0].to_string();
        for i in 1..captures.len() {
            let prev = captures[i - 1];
            let cur = captures[i];
            let (next, result) = apply_step(recon, prev, cur);
            recon = next;

            // Per-step structural no-double-count: a Content delta is exactly the
            // tail of `cur`, and the bytes BEFORE it in `cur` (the claimed overlap)
            // really were a suffix of `prev` — so they are not re-counted in delta.
            if let DeltaResult::Content(delta) = &result {
                prop_assert!(cur.ends_with(delta.as_str()), "delta must be a suffix of current");
                let overlap_len = cur.len() - delta.len();
                prop_assert!(
                    prev.ends_with(&cur[..overlap_len]),
                    "the non-delta prefix of current must be a genuine suffix of previous"
                );
            }
            // Sequence-level: reconstruction never inflates past real content.
            prop_assert!(
                recon.len() <= high_water,
                "reconstruction length {} exceeded high-water content {} — double count",
                recon.len(), high_water
            );
        }
    }
}

/// MUTATION CHECK (non-vacuity guard). A deliberately *wrong* reconstruction that
/// double-counts — pushing the FULL `cur` each step instead of only the delta —
/// must produce a length that strictly exceeds the true content. If this failed,
/// the conservation properties above would be vacuous.
#[test]
fn mutation_check_naive_double_count_is_detectable() {
    // Three pure-append captures of a 6-char stream.
    let captures = ["abc", "abcde", "abcdef"];
    let full_len = captures.last().unwrap().len(); // 6

    // Correct reconstruction (delta-only) == full, length 6.
    let mut correct = captures[0].to_string();
    for i in 1..captures.len() {
        if let DeltaResult::Content(d) = extract_delta(captures[i - 1], captures[i], OVERLAP) {
            correct.push_str(&d);
        }
    }
    assert_eq!(correct, "abcdef");
    assert_eq!(correct.len(), full_len);

    // Naive double-counting reconstruction: push the whole `cur` each step.
    let mut naive = captures[0].to_string();
    for c in &captures[1..] {
        naive.push_str(c);
    }
    // "abc" + "abcde" + "abcdef" = 3 + 5 + 6 = 14 bytes >> 6.
    assert!(
        naive.len() > full_len,
        "negative control must inflate; got {} vs full {}",
        naive.len(),
        full_len
    );
    // The metric used by the property (recon.len() <= true content) DISTINGUISHES
    // correct from double-counting: correct passes, naive fails.
    assert!(correct.len() <= full_len);
    assert!(naive.len() > full_len);
}
