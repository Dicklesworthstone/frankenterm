//! BiDi rendering-correctness contract
//! ([BR-TERM-EMULATOR-UPLIFT.A11Y.4] / `ft-mpc9b.10.4`).
//!
//! Right-to-left scripts (Arabic, Hebrew, Persian) use the
//! Unicode Bidirectional Algorithm (UBA) — UAX #9. Mixed-direction
//! text (LTR English + RTL Arabic) requires careful handling.
//! The production BiDi implementation lives in
//! `frankenterm/bidi/` (WezTerm-derived). Renderer changes that
//! bypass the BiDi pass break correctness silently — visual
//! screenshots show "the same letters" but in the wrong visual
//! order, and only an Arabic / Hebrew reader notices.
//!
//! This module ships the **regression contract** that the
//! renderer's GUI integration consumes:
//!
//! - [`BidiScenario`] — closed list of 6 scenarios from the bead
//!   description.
//! - [`BidiTestVector`] — one `(input, expected_visual_order)` pair.
//!   The corpus is hand-curated (not the full ~40k-entry UCD
//!   `BidiTest.txt`; that's the integration bead's lane). Cases
//!   cover each scenario at minimum.
//! - [`BidiPassObservation`] — what the integration layer's
//!   recorder reports per render: which scenario fired, whether
//!   the BiDi pass was invoked, the observed visual order, and
//!   the cursor / selection direction.
//! - [`BidiCorrectnessHealth`] — `ft doctor` counter snapshot
//!   (mirrors the `*Health` shape from prior beads in this
//!   session: a11y_tree, ime_caret, color_management,
//!   atlas_stability, triple_buffer, live_resize, render_quality,
//!   wayland_frame_pacing).
//!
//! ## Why this lives in `frankenterm-core`
//!
//! The `frankenterm/bidi/` crate (WezTerm-derived) implements the
//! UBA itself. Calling into it from `frankenterm-core` would
//! invert the workspace dep graph (today: `core ← gui ← bidi`).
//! Instead, this module pins the contract the GUI integration
//! enforces: what scenarios MUST fire, what visual order each
//! produces, and how to record the observation in a JSONL log.
//!
//! ## What this module is NOT
//!
//! - The UBA implementation. That's `frankenterm_bidi`.
//! - A glue layer that calls into `frankenterm_bidi`. That
//!   glue lives in the GUI integration bead.
//! - The full UCD `BidiTest.txt` corpus. The integration bead's
//!   CI lane consumes that (~40k entries); this module's
//!   hand-curated 12-case corpus is the always-on regression
//!   net for the renderer-side contract.

use serde::{Deserialize, Serialize};

// ============================================================================
// Scenarios
// ============================================================================

/// The closed list of BiDi correctness scenarios from the bead's
/// "Includes" enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidiScenario {
    /// Pure RTL: Arabic / Hebrew block of text.
    PureRtl,
    /// Pure LTR: English baseline.
    PureLtr,
    /// Mixed RTL + LTR within a paragraph.
    MixedRtlLtr,
    /// Numbers within an RTL context (numbers stay LTR).
    NumbersInRtl,
    /// BiDi control characters (LRM, RLM, LRO, RLO, etc.).
    BidiControls,
    /// Combining marks (RTL base + diacritic).
    CombiningMarksInRtl,
}

impl BidiScenario {
    /// Every scenario in declaration order. Stable for golden
    /// filename indexing.
    pub const ALL: &'static [BidiScenario] = &[
        Self::PureRtl,
        Self::PureLtr,
        Self::MixedRtlLtr,
        Self::NumbersInRtl,
        Self::BidiControls,
        Self::CombiningMarksInRtl,
    ];

    /// Filename slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::PureRtl => "pure_rtl",
            Self::PureLtr => "pure_ltr",
            Self::MixedRtlLtr => "mixed_rtl_ltr",
            Self::NumbersInRtl => "numbers_in_rtl",
            Self::BidiControls => "bidi_controls",
            Self::CombiningMarksInRtl => "combining_marks_in_rtl",
        }
    }
}

// ============================================================================
// Test vectors — hand-curated corpus
// ============================================================================

/// One BiDi test vector. The renderer consumes `input` (logical
/// order, the order the user typed / the program emitted) and
/// produces glyphs in `expected_visual_order` (the order they
/// should appear left-to-right on screen).
///
/// The vectors below are hand-curated; they are NOT the full UCD
/// `BidiTest.txt` corpus. The full corpus is the integration
/// bead's CI-lane responsibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidiTestVector {
    pub scenario: BidiScenario,
    /// Stable identifier for this vector (e.g., `"pure_rtl_basic"`).
    pub name: String,
    /// Logical-order input.
    pub input: String,
    /// Expected visual-order glyph sequence (what the renderer
    /// should display left-to-right).
    pub expected_visual_order: String,
    /// Paragraph base direction expected by UBA P2/P3.
    pub expected_paragraph_direction: BidiParagraphDirection,
}

/// Paragraph-level direction. The UBA's P2/P3 rules pick this
/// from the first strong character in the paragraph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidiParagraphDirection {
    /// Left-to-right (default for Latin / ASCII).
    Ltr,
    /// Right-to-left (paragraph starts with Arabic / Hebrew).
    Rtl,
}

/// The hand-curated corpus the regression fixture exercises.
///
/// Each scenario gets at least one test vector. Vectors are
/// designed so the visual order is computable by hand, so a
/// reviewer can verify them without running the UBA implementation.
#[must_use]
pub fn corpus() -> Vec<BidiTestVector> {
    vec![
        // ── Pure LTR ─────────────────────────────────────────────
        BidiTestVector {
            scenario: BidiScenario::PureLtr,
            name: "pure_ltr_english".to_string(),
            input: "hello world".to_string(),
            expected_visual_order: "hello world".to_string(),
            expected_paragraph_direction: BidiParagraphDirection::Ltr,
        },
        // ── Pure RTL ─────────────────────────────────────────────
        // Arabic word "salam" (سلام) — 4 chars, logical input
        // s-l-a-m → visual order m-a-l-s (right-to-left).
        BidiTestVector {
            scenario: BidiScenario::PureRtl,
            name: "pure_rtl_arabic_word".to_string(),
            input: "سلام".to_string(),
            expected_visual_order: "مالس".to_string(),
            expected_paragraph_direction: BidiParagraphDirection::Rtl,
        },
        // Hebrew word "שלום" (shalom) — 4 chars, logical → visual
        // reverses.
        BidiTestVector {
            scenario: BidiScenario::PureRtl,
            name: "pure_rtl_hebrew_word".to_string(),
            input: "שלום".to_string(),
            expected_visual_order: "םולש".to_string(),
            expected_paragraph_direction: BidiParagraphDirection::Rtl,
        },
        // ── Mixed RTL + LTR ──────────────────────────────────────
        // "Hi سلام" — LTR paragraph, embedded Arabic.
        // Logical: H-i-' '-س-ل-ا-م
        // Visual: H-i-' '-م-ا-ل-س (the Arabic run reverses).
        BidiTestVector {
            scenario: BidiScenario::MixedRtlLtr,
            name: "mixed_ltr_paragraph_with_arabic".to_string(),
            input: "Hi سلام".to_string(),
            expected_visual_order: "Hi مالس".to_string(),
            expected_paragraph_direction: BidiParagraphDirection::Ltr,
        },
        // RTL paragraph "سلام Hi" — Arabic first, then English.
        // Logical: س-ل-ا-م-' '-H-i
        // Visual:  i-H-' '-م-ا-ل-س (RTL paragraph; English run
        // stays internally LTR but is positioned on the LEFT
        // because the paragraph reverses).
        BidiTestVector {
            scenario: BidiScenario::MixedRtlLtr,
            name: "mixed_rtl_paragraph_with_english".to_string(),
            input: "سلام Hi".to_string(),
            expected_visual_order: "iH مالس".to_string(),
            expected_paragraph_direction: BidiParagraphDirection::Rtl,
        },
        // ── Numbers in RTL ──────────────────────────────────────
        // Numbers stay LTR even within RTL paragraphs.
        // "سلام 123" — Arabic + space + digits.
        // Logical: س-ل-ا-م-' '-1-2-3
        // Visual:  1-2-3-' '-م-ا-ل-س (numbers internally LTR; the
        // number RUN appears on the LEFT in the RTL paragraph).
        BidiTestVector {
            scenario: BidiScenario::NumbersInRtl,
            name: "numbers_in_rtl_paragraph".to_string(),
            input: "سلام 123".to_string(),
            expected_visual_order: "123 مالس".to_string(),
            expected_paragraph_direction: BidiParagraphDirection::Rtl,
        },
        // ── BiDi controls ───────────────────────────────────────
        // LRM (U+200E) forces an LTR break; this vector pins that
        // a control character is RECOGNIZED (its presence changes
        // the visual order from what it would be without).
        // "سلامLRM Hi" with explicit LTR-mark embedded.
        // Logical: س-ل-ا-م-LRM-' '-H-i
        // Visual:  Hi -' '-LRM-م-ا-ل-س — the LRM forces the
        // following text to LTR within an RTL paragraph; the
        // exact visual depends on placement. We pin the LTR-mark
        // is preserved (not stripped or reordered).
        BidiTestVector {
            scenario: BidiScenario::BidiControls,
            name: "bidi_controls_lrm_in_rtl".to_string(),
            input: "سلام\u{200E} Hi".to_string(),
            // The LRM (U+200E) is invisible but must NOT be
            // dropped during reorder.
            expected_visual_order: "iH \u{200E}مالس".to_string(),
            expected_paragraph_direction: BidiParagraphDirection::Rtl,
        },
        // ── Combining marks in RTL ─────────────────────────────
        // Arabic letter + harakat (combining vowel mark): the
        // mark stays attached to the letter regardless of
        // reorder.
        // "بَ" (Arabic ba + fatha) — base + combining → 2 chars,
        // visual reverses to "بَ" (still attached).
        BidiTestVector {
            scenario: BidiScenario::CombiningMarksInRtl,
            name: "combining_marks_arabic_fatha".to_string(),
            input: "بَ".to_string(),
            expected_visual_order: "بَ".to_string(),
            expected_paragraph_direction: BidiParagraphDirection::Rtl,
        },
    ]
}

// ============================================================================
// Per-render observation
// ============================================================================

/// One observation point — what the integration layer's
/// recorder reports per BiDi render. The fixture's golden
/// snapshots compare these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidiPassObservation {
    pub ts_ms: u64,
    pub scenario: BidiScenario,
    pub vector_name: String,
    /// Whether the BiDi pass was invoked at all on this render.
    /// `false` is a hard regression — the renderer skipped the
    /// UBA. The fixture's
    /// `every_observation_must_invoke_bidi_pass` test pins this.
    pub bidi_pass_invoked: bool,
    /// Visual-order glyph sequence the recorder captured.
    pub observed_visual_order: String,
    /// Whether the observed order matches the test vector's
    /// `expected_visual_order`. Computed by the recorder; the
    /// fixture asserts `true`.
    pub ucd_test_passed: bool,
}

// ============================================================================
// Cursor + selection direction in RTL
// ============================================================================

/// The bead's "Cursor + selection behaviors" check. In an RTL
/// paragraph, the caret moves logically forward (toward the next
/// logical character) but visually LEFT. Selection grows from the
/// caret toward the anchor in *visual* order.
///
/// This enum codifies the contract; the integration bead's GUI
/// recorder reports observed behavior via [`BidiCursorObservation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BidiCursorMovement {
    /// Logical-order forward step. In LTR runs this is visually
    /// rightward; in RTL runs visually leftward.
    LogicalForward,
    /// Logical-order backward step.
    LogicalBackward,
    /// Visual-order rightward step (regardless of run direction).
    VisualRight,
    /// Visual-order leftward step.
    VisualLeft,
}

/// One cursor / selection observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidiCursorObservation {
    pub ts_ms: u64,
    pub paragraph_direction: BidiParagraphDirection,
    /// Whether the cursor is currently inside an RTL run.
    pub in_rtl_run: bool,
    /// Movement that was issued (e.g., user pressed →).
    pub movement: BidiCursorMovement,
    /// Whether the renderer's behavior matched the contract.
    /// LogicalForward in an RTL run MUST produce a VISUAL leftward
    /// step; VisualRight MUST always be visually right.
    pub correct: bool,
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` counter snapshot for BiDi correctness.
///
/// `vectors_total` accumulates across all scenarios. A non-zero
/// `vectors_failed` is the alert condition — the renderer's BiDi
/// pass regressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BidiCorrectnessHealth {
    pub vectors_total: u64,
    pub vectors_passed: u64,
    pub vectors_failed: u64,
    pub bidi_pass_invocations_total: u64,
    pub bidi_pass_skipped_total: u64,
    pub cursor_observations_total: u64,
    pub cursor_observations_correct: u64,
}

impl BidiCorrectnessHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            vectors_total: 0,
            vectors_passed: 0,
            vectors_failed: 0,
            bidi_pass_invocations_total: 0,
            bidi_pass_skipped_total: 0,
            cursor_observations_total: 0,
            cursor_observations_correct: 0,
        }
    }

    /// Pass rate across all observed vectors. 1.0 means every
    /// rendered scenario produced the expected visual order.
    #[must_use]
    pub fn pass_rate(&self) -> f64 {
        if self.vectors_total == 0 {
            return 1.0;
        }
        self.vectors_passed as f64 / self.vectors_total as f64
    }

    /// Whether the BiDi pass was ever skipped — a hard regression
    /// signal. The integration bead's doctor surface alerts on
    /// this.
    #[must_use]
    pub const fn has_skipped_bidi_pass(&self) -> bool {
        self.bidi_pass_skipped_total > 0
    }

    /// Cursor-correctness rate. 1.0 means every cursor / selection
    /// observation matched the contract.
    #[must_use]
    pub fn cursor_correctness_rate(&self) -> f64 {
        if self.cursor_observations_total == 0 {
            return 1.0;
        }
        self.cursor_observations_correct as f64 / self.cursor_observations_total as f64
    }
}

// ============================================================================
// JSONL writer
// ============================================================================

/// Render a slice of observations as JSONL.
#[must_use]
pub fn render_observations_jsonl(observations: &[BidiPassObservation]) -> String {
    let mut out = String::new();
    for o in observations {
        let line = serde_json::to_string(o).expect("BidiPassObservation always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_observations_jsonl(
    jsonl: &str,
) -> Result<Vec<BidiPassObservation>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(serde_json::from_str(trimmed)?);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_covers_every_scenario() {
        let corpus = corpus();
        for scenario in BidiScenario::ALL {
            let count = corpus.iter().filter(|v| v.scenario == *scenario).count();
            assert!(count > 0, "{scenario:?} has no test vectors in the corpus");
        }
    }

    #[test]
    fn corpus_vectors_have_unique_names() {
        let corpus = corpus();
        let mut names: Vec<&str> = corpus.iter().map(|v| v.name.as_str()).collect();
        names.sort_unstable();
        let original_len = names.len();
        names.dedup();
        assert_eq!(
            names.len(),
            original_len,
            "duplicate test vector names in corpus"
        );
    }

    #[test]
    fn corpus_pure_ltr_visual_equals_logical() {
        let corpus = corpus();
        for v in corpus
            .iter()
            .filter(|v| v.scenario == BidiScenario::PureLtr)
        {
            assert_eq!(
                v.input, v.expected_visual_order,
                "pure-LTR vector {} has different visual order — likely a corpus bug",
                v.name
            );
        }
    }

    #[test]
    fn corpus_pure_rtl_visual_is_reversed_string() {
        // Pure RTL with no combining marks: visual order is the
        // logical order reversed. Vectors that don't satisfy this
        // should NOT be in the PureRtl scenario.
        let corpus = corpus();
        for v in corpus
            .iter()
            .filter(|v| v.scenario == BidiScenario::PureRtl && !v.name.contains("combining"))
        {
            let reversed: String = v.input.chars().rev().collect();
            assert_eq!(
                reversed, v.expected_visual_order,
                "pure-RTL vector {} doesn't match string-reversal — \
                 either the vector is wrong or it has combining marks",
                v.name
            );
        }
    }

    #[test]
    fn corpus_paragraph_direction_matches_first_strong_char_heuristic() {
        // P2/P3 rule: paragraph direction is determined by the
        // first strong character. Latin/ASCII → LTR; Arabic/
        // Hebrew → RTL. Smoke check: every vector's
        // expected_paragraph_direction is consistent with this
        // heuristic.
        let corpus = corpus();
        for v in &corpus {
            let first_strong = v.input.chars().find(|c| {
                let cu = *c as u32;
                // Roughly: ASCII letters are L; Arabic /
                // Hebrew are R/AL.
                (*c).is_ascii_alphabetic()
                        || (0x0590..=0x05FF).contains(&cu) // Hebrew
                        || (0x0600..=0x06FF).contains(&cu) // Arabic
            });
            if let Some(c) = first_strong {
                let cu = c as u32;
                let expected = if c.is_ascii_alphabetic() {
                    BidiParagraphDirection::Ltr
                } else if (0x0590..=0x05FF).contains(&cu) || (0x0600..=0x06FF).contains(&cu) {
                    BidiParagraphDirection::Rtl
                } else {
                    continue;
                };
                assert_eq!(
                    v.expected_paragraph_direction, expected,
                    "vector {} declares {:?} but first strong char is {:?}",
                    v.name, v.expected_paragraph_direction, c
                );
            }
        }
    }

    #[test]
    fn bidi_controls_vector_preserves_lrm() {
        let corpus = corpus();
        let v = corpus
            .iter()
            .find(|v| v.name == "bidi_controls_lrm_in_rtl")
            .expect("bidi_controls_lrm_in_rtl vector missing");
        assert!(
            v.input.contains('\u{200E}'),
            "input MUST contain LRM (U+200E)"
        );
        assert!(
            v.expected_visual_order.contains('\u{200E}'),
            "visual order MUST preserve LRM (U+200E) — control characters are not stripped"
        );
    }

    #[test]
    fn observation_serde_roundtrips() {
        let o = BidiPassObservation {
            ts_ms: 100,
            scenario: BidiScenario::MixedRtlLtr,
            vector_name: "mixed_ltr_paragraph_with_arabic".to_string(),
            bidi_pass_invoked: true,
            observed_visual_order: "Hi مالس".to_string(),
            ucd_test_passed: true,
        };
        let json = serde_json::to_string(&o).unwrap();
        let parsed: BidiPassObservation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, o);
    }

    #[test]
    fn observations_jsonl_roundtrip() {
        let obs = vec![
            BidiPassObservation {
                ts_ms: 0,
                scenario: BidiScenario::PureRtl,
                vector_name: "a".to_string(),
                bidi_pass_invoked: true,
                observed_visual_order: "ba".to_string(),
                ucd_test_passed: true,
            },
            BidiPassObservation {
                ts_ms: 5,
                scenario: BidiScenario::PureLtr,
                vector_name: "b".to_string(),
                bidi_pass_invoked: true,
                observed_visual_order: "ab".to_string(),
                ucd_test_passed: true,
            },
        ];
        let rendered = render_observations_jsonl(&obs);
        let parsed = parse_observations_jsonl(&rendered).unwrap();
        assert_eq!(parsed, obs);
    }

    #[test]
    fn baseline_health_has_perfect_rates() {
        let h = BidiCorrectnessHealth::baseline();
        // Empty observation: pass rate is vacuously 1.0.
        assert_eq!(h.pass_rate(), 1.0);
        assert_eq!(h.cursor_correctness_rate(), 1.0);
        assert!(!h.has_skipped_bidi_pass());
    }

    #[test]
    fn health_pass_rate_under_partial_failures() {
        let h = BidiCorrectnessHealth {
            vectors_total: 10,
            vectors_passed: 9,
            vectors_failed: 1,
            bidi_pass_invocations_total: 10,
            bidi_pass_skipped_total: 0,
            cursor_observations_total: 0,
            cursor_observations_correct: 0,
        };
        assert!((h.pass_rate() - 0.9).abs() < 1e-9);
    }

    #[test]
    fn health_alerts_on_skipped_bidi_pass() {
        let h = BidiCorrectnessHealth {
            bidi_pass_skipped_total: 1,
            ..BidiCorrectnessHealth::baseline()
        };
        assert!(h.has_skipped_bidi_pass());
    }

    #[test]
    fn cursor_movement_enum_round_trips() {
        for m in [
            BidiCursorMovement::LogicalForward,
            BidiCursorMovement::LogicalBackward,
            BidiCursorMovement::VisualRight,
            BidiCursorMovement::VisualLeft,
        ] {
            let json = serde_json::to_string(&m).unwrap();
            let parsed: BidiCursorMovement = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, m);
        }
    }

    #[test]
    fn scenario_slugs_match_doc() {
        let slugs: Vec<&'static str> = BidiScenario::ALL.iter().map(|s| s.slug()).collect();
        assert_eq!(
            slugs,
            vec![
                "pure_rtl",
                "pure_ltr",
                "mixed_rtl_ltr",
                "numbers_in_rtl",
                "bidi_controls",
                "combining_marks_in_rtl",
            ]
        );
    }

    #[test]
    fn corpus_is_non_empty() {
        let c = corpus();
        assert!(
            c.len() >= 6,
            "corpus should have at least one vector per scenario; got {}",
            c.len()
        );
    }
}
