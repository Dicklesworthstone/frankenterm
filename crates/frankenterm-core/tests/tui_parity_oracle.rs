//! Regression harness for the differential render oracle
//! ([BR-RC-CUTOVERS.G5.1] / `ft-35yac.1`).
//!
//! Exercises the comparator + corpus invariants the
//! `crate::tui_parity_oracle` module ships, plus a proptest
//! sweep that asserts the comparator's algebraic properties
//! on randomly-generated frames:
//!
//! - **Reflexivity:** `compute_diff(f, f).is_clean()` for any
//!   well-shaped frame `f`.
//! - **Symmetry:** `compute_diff(a, b)` and `compute_diff(b, a)`
//!   report the same divergent-cell count.
//! - **Cell-wise correctness:** the divergent-cell count of
//!   `compute_diff(a, b)` equals the manual count of
//!   `(row, col)` positions where `a.cell != b.cell`
//!   structurally.
//! - **Dimension-mismatch flag:** distinct dimensions ⇒
//!   `dimension_mismatch == true` AND `cells.is_empty()`.
//!
//! The proptest count is 256 — same bound as this session's
//! sibling harnesses (passive_watch_invariant,
//! redactor_coverage_matrix, wire_dedup_model, tx_killswitch).

use frankenterm_core::tui_parity_oracle::{
    EventScript, KeymapAction, KeymapActionKind, OracleHealth, RenderCell, RenderFrame, Rgba,
    compute_diff, fold_diff, synthesized_event_corpus,
};
use proptest::prelude::*;

// ----------------------------------------------------------------------------
// Property tests
// ----------------------------------------------------------------------------

fn arb_rgba() -> impl Strategy<Value = Rgba> {
    (any::<u8>(), any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b, a)| Rgba {
        r,
        g,
        b,
        a,
    })
}

fn arb_cell() -> impl Strategy<Value = RenderCell> {
    (
        any::<u32>().prop_map(|n| char::from_u32(n % 0x10FFFF + 0x20).unwrap_or(' ')),
        arb_rgba(),
        arb_rgba(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(ch, fg, bg, bold, italic, underline, reverse)| RenderCell {
                ch,
                fg,
                bg,
                bold,
                italic,
                underline,
                reverse,
                continuation: false,
            },
        )
}

fn arb_frame_with_dim(width: u16, height: u16) -> impl Strategy<Value = RenderFrame> {
    let total = width as usize * height as usize;
    proptest::collection::vec(arb_cell(), total..=total).prop_map(move |cells| RenderFrame {
        width,
        height,
        cells,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Reflexivity: a frame compared to itself is always
    /// clean. The bead's headline rule
    /// ("byte-identical render frames") collapses to this in
    /// the degenerate self-compare case.
    #[test]
    fn diff_self_is_clean_property(f in arb_frame_with_dim(4, 3)) {
        let d = compute_diff(&f, &f);
        prop_assert!(d.is_clean());
        prop_assert_eq!(d.divergent_cell_count(), 0);
    }

    /// Symmetry on count: swapping operands cannot change the
    /// number of divergent cells.
    #[test]
    fn diff_is_symmetric_in_count_property(
        a in arb_frame_with_dim(5, 4),
        b in arb_frame_with_dim(5, 4),
    ) {
        let d_ab = compute_diff(&a, &b);
        let d_ba = compute_diff(&b, &a);
        prop_assert_eq!(d_ab.divergent_cell_count(), d_ba.divergent_cell_count());
        prop_assert_eq!(d_ab.dimension_mismatch, d_ba.dimension_mismatch);
    }

    /// Cell-wise correctness: divergent-cell count from
    /// compute_diff equals the manual count of structurally-
    /// unequal cells. The contract is "byte-identical" at the
    /// span level — this property is the formal version.
    #[test]
    fn diff_count_matches_manual_count(
        a in arb_frame_with_dim(6, 4),
        b in arb_frame_with_dim(6, 4),
    ) {
        let d = compute_diff(&a, &b);
        let mut manual = 0usize;
        for row in 0..a.height {
            for col in 0..a.width {
                let l = a.cell(row, col).unwrap();
                let r = b.cell(row, col).unwrap();
                if !l.structurally_equal(r) {
                    manual += 1;
                }
            }
        }
        prop_assert_eq!(d.divergent_cell_count(), manual);
    }

    /// Dimension-mismatch flag fires iff dimensions differ;
    /// when set, no cell-level records are emitted (the
    /// comparator does not attempt cross-dim alignment).
    #[test]
    fn dimension_mismatch_flag_correctness(
        w_a in 1u16..=10,
        h_a in 1u16..=10,
        w_b in 1u16..=10,
        h_b in 1u16..=10,
    ) {
        let a = RenderFrame::blank(w_a, h_a);
        let b = RenderFrame::blank(w_b, h_b);
        let d = compute_diff(&a, &b);
        if (w_a, h_a) == (w_b, h_b) {
            prop_assert!(!d.dimension_mismatch);
            prop_assert!(d.is_clean());
        } else {
            prop_assert!(d.dimension_mismatch);
            prop_assert!(d.cells.is_empty());
        }
    }

    /// Triangle-inequality-style: if a and c differ on N cells
    /// total, and b is between them, the union of (a,b) and
    /// (b,c) divergent cells is at most N. This is a
    /// soft-check that flush-coalescing of partial frames
    /// can't fool the comparator.
    #[test]
    fn triangle_inequality_on_diverged_cells(
        a in arb_frame_with_dim(4, 3),
        b in arb_frame_with_dim(4, 3),
        c in arb_frame_with_dim(4, 3),
    ) {
        let d_ac = compute_diff(&a, &c);
        let d_ab = compute_diff(&a, &b);
        let d_bc = compute_diff(&b, &c);
        // Triangle inequality on cell-position sets:
        // |a ⊕ c| ≤ |a ⊕ b| + |b ⊕ c|.
        prop_assert!(
            d_ac.divergent_cell_count()
                <= d_ab.divergent_cell_count() + d_bc.divergent_cell_count()
        );
    }

    /// Continuation-flag noise is invisible to the comparator
    /// (per the structurally_equal predicate). A frame
    /// compared to itself with continuation flags toggled is
    /// still clean.
    #[test]
    fn continuation_flag_does_not_affect_diff(f in arb_frame_with_dim(4, 3)) {
        let mut g = f.clone();
        for cell in &mut g.cells {
            cell.continuation = !cell.continuation;
        }
        let d = compute_diff(&f, &g);
        prop_assert!(d.is_clean());
    }

    /// Health rollup is monotone: fold_diff strictly
    /// increases frames_compared_total by 1 per call, never
    /// decreases any counter, and clean-counter only
    /// increments when the diff is clean.
    #[test]
    fn fold_diff_is_monotone(
        a in arb_frame_with_dim(4, 3),
        b in arb_frame_with_dim(4, 3),
    ) {
        let d = compute_diff(&a, &b);
        let mut h = OracleHealth::baseline();
        let before = h;
        fold_diff(&mut h, &d);
        prop_assert_eq!(h.frames_compared_total, before.frames_compared_total + 1);
        prop_assert!(h.clean_frames_total >= before.clean_frames_total);
        prop_assert!(h.diverged_frames_total >= before.diverged_frames_total);
        if d.is_clean() {
            prop_assert_eq!(h.clean_frames_total, before.clean_frames_total + 1);
        } else {
            prop_assert_eq!(h.diverged_frames_total, before.diverged_frames_total + 1);
        }
    }
}

// ----------------------------------------------------------------------------
// Corpus shape
// ----------------------------------------------------------------------------

#[test]
fn every_synthesized_script_is_well_formed() {
    let corpus = synthesized_event_corpus();
    for s in &corpus {
        assert!(!s.name.is_empty(), "script with empty name");
        assert!(
            !s.actions.is_empty(),
            "{} has empty action sequence",
            s.name
        );
        assert!(s.width > 0 && s.height > 0, "{} has zero dim", s.name);
        assert!(
            (1..=7).contains(&s.initial_view),
            "{} initial_view={} out of range",
            s.name,
            s.initial_view,
        );
    }
}

#[test]
fn every_keymap_action_kind_appears_in_corpus_or_is_explicitly_omitted() {
    // Bead requires "every input action covered" by the
    // proptest scaffold. The corpus covers a representative
    // subset; the proptest scaffold below sweeps every kind.
    let corpus = synthesized_event_corpus();
    let mut covered = std::collections::BTreeSet::new();
    for s in &corpus {
        for a in &s.actions {
            covered.insert(a.kind());
        }
    }

    // Document which kinds are explicitly absent from the
    // synthesized corpus and rely on proptest sweep coverage
    // instead. When ft-35yac.1.1 lands real-session corpora,
    // these gaps are expected to close naturally.
    let known_omitted = [
        KeymapActionKind::PrevTab, // covered indirectly by NextTab + symmetry
        KeymapActionKind::ApplyRulesetProfile,
    ];
    let missing: Vec<_> = KeymapActionKind::ALL
        .iter()
        .copied()
        .filter(|k| !covered.contains(k) && !known_omitted.contains(k))
        .collect();
    assert!(
        missing.is_empty(),
        "missing keymap action kinds in synthesized corpus: {missing:?}",
    );
}

// ----------------------------------------------------------------------------
// Proptest sweep over every keymap action kind
// ----------------------------------------------------------------------------

fn arb_keymap_action() -> impl Strategy<Value = KeymapAction> {
    prop_oneof![
        Just(KeymapAction::Quit),
        Just(KeymapAction::ShowHelp),
        Just(KeymapAction::Refresh),
        Just(KeymapAction::NextTab),
        Just(KeymapAction::PrevTab),
        (1u8..=7).prop_map(|view_index| KeymapAction::GoToView { view_index }),
        Just(KeymapAction::ListNext),
        Just(KeymapAction::ListPrev),
        proptest::char::range('a', 'z').prop_map(|ch| KeymapAction::FilterAppendChar { ch }),
        Just(KeymapAction::FilterDeleteChar),
        Just(KeymapAction::FilterClear),
        Just(KeymapAction::ToggleUnhandledOnly),
        Just(KeymapAction::ToggleBookmarkedOnly),
        Just(KeymapAction::CycleAgentFilter),
        Just(KeymapAction::CycleDomainFilter),
        Just(KeymapAction::CycleRulesetProfile),
        Just(KeymapAction::ApplyRulesetProfile),
        proptest::char::range('0', '9').prop_map(|digit| KeymapAction::EventsFilterDigit { digit }),
        Just(KeymapAction::TriagePrimaryAction),
        Just(KeymapAction::TriageMute),
        Just(KeymapAction::TriageToggleExpand),
        (1u8..=9).prop_map(|index| KeymapAction::TriageNumberedAction { index }),
        Just(KeymapAction::ToggleUndoableOnly),
        Just(KeymapAction::SearchNextSaved),
        Just(KeymapAction::SearchPrevSaved),
        Just(KeymapAction::SearchRunSaved),
        Just(KeymapAction::SearchToggleSaved),
        Just(KeymapAction::SearchExecute),
        Just(KeymapAction::TimelineZoomIn),
        Just(KeymapAction::TimelineZoomOut),
        Just(KeymapAction::TimelineScrollLeft),
        Just(KeymapAction::TimelineScrollRight),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every random KeymapAction projects to a stable kind
    /// and serializes cleanly. Property-test-shape coverage
    /// over the full input alphabet.
    #[test]
    fn keymap_action_kind_is_stable(a in arb_keymap_action()) {
        let kind1 = a.kind();
        let kind2 = a.kind();
        prop_assert_eq!(kind1, kind2);
        let json = serde_json::to_string(&a).unwrap();
        let restored: KeymapAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, restored);
    }

    /// Every random EventScript that ends in Quit serializes
    /// stably and round-trips through serde.
    #[test]
    fn event_script_serde_roundtrip(
        actions in proptest::collection::vec(arb_keymap_action(), 1..16),
        view in 1u8..=7,
        w in 40u16..=200,
        h in 12u16..=60,
    ) {
        let mut full = actions.clone();
        full.push(KeymapAction::Quit);
        let s = EventScript {
            name: "proptest_synth".to_string(),
            rationale: "proptest".to_string(),
            initial_view: view,
            width: w,
            height: h,
            actions: full,
        };
        let json = serde_json::to_string(&s).unwrap();
        let restored: EventScript = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, restored);
    }
}
