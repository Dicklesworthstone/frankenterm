# BiDi Rendering-Correctness Contract

**Bead:** [BR-TERM-EMULATOR-UPLIFT.A11Y.4] / `ft-mpc9b.10.4`

Right-to-left scripts (Arabic, Hebrew, Persian) use the Unicode
Bidirectional Algorithm — UAX #9. Mixed-direction text (LTR
English + RTL Arabic in one paragraph) requires careful
handling. The production UBA implementation lives in
`frankenterm/bidi/` (WezTerm-derived). Renderer changes that
bypass that pass break correctness silently — visual screenshots
show "the same letters" but in wrong visual order; only an
Arabic / Hebrew reader notices.

This bead ships the **regression contract** the renderer's GUI
integration consumes. Same pattern as the prior accessibility
beads in this session (`a11y_tree`, `ime_caret`,
`color_management`).

## What this module ships

`crates/frankenterm-core/src/bidi_correctness.rs`:

- `BidiScenario` — closed list of 6 scenarios from the bead's
  "Includes" enumeration: `pure_rtl`, `pure_ltr`,
  `mixed_rtl_ltr`, `numbers_in_rtl`, `bidi_controls`,
  `combining_marks_in_rtl`.
- `BidiTestVector` — one `(input, expected_visual_order)` pair
  with paragraph-direction expectation. Hand-curated corpus
  (8 vectors, ≥1 per scenario). Every vector is hand-verifiable
  by a reviewer without running the UBA implementation.
- `BidiPassObservation` — what the integration's recorder reports
  per render: scenario, vector name, whether the BiDi pass was
  invoked, observed visual order, UCD-test pass/fail.
- `BidiCursorMovement` + `BidiCursorObservation` — cursor /
  selection direction contract for RTL paragraphs.
- `BidiCorrectnessHealth` — `ft doctor` counter snapshot
  (vectors_total, vectors_passed, vectors_failed,
  bidi_pass_invocations_total, bidi_pass_skipped_total,
  cursor_observations_total, cursor_observations_correct).
  `has_skipped_bidi_pass()` is the alert predicate.
- JSONL writer for `tests/bidi/logs/<scenario>.jsonl` per the
  bead's structured-log schema.

## Why this module doesn't call into `frankenterm/bidi/`

The dep graph today is `core ← gui ← bidi`. Calling into
`frankenterm_bidi` from `frankenterm-core` would invert that.
Instead, this module pins the **contract** the GUI integration
enforces — what scenarios MUST fire, what visual order each
produces, how to record observations. The integration bead in
`crates/frankenterm-gui/` is the layer that:
1. Calls `frankenterm_bidi::BidiContext` on each input string.
2. Records a `BidiPassObservation` per scenario.
3. Compares observed vs. expected from the corpus.
4. Bumps `BidiCorrectnessHealth.vectors_passed` /
   `vectors_failed`.

## Hand-curated corpus (vs. full UCD `BidiTest.txt`)

The corpus contains 8 vectors covering all 6 scenarios. Each is
designed so the visual order is computable by hand:

| Scenario | Vector | Logical → Visual rule |
|---|---|---|
| `PureLtr` | `"hello world"` | identity |
| `PureRtl` | `"سلام"` (Arabic) | string-reversed |
| `PureRtl` | `"שלום"` (Hebrew) | string-reversed |
| `MixedRtlLtr` | `"Hi سلام"` (LTR para) | LTR run as-is, RTL run reversed |
| `MixedRtlLtr` | `"سلام Hi"` (RTL para) | RTL run reversed, LTR run on the LEFT |
| `NumbersInRtl` | `"سلام 123"` | digits stay LTR, run on the LEFT in RTL paragraph |
| `BidiControls` | `"سلام\u{200E} Hi"` | LRM preserved (not stripped) |
| `CombiningMarksInRtl` | `"بَ"` (ba + fatha) | mark stays attached to base |

Lib tests assert these structural invariants directly:

- `corpus_pure_ltr_visual_equals_logical` — pure LTR is identity.
- `corpus_pure_rtl_visual_is_reversed_string` — pure RTL with no
  combining marks is exactly the input reversed.
- `corpus_paragraph_direction_matches_first_strong_char_heuristic`
  — UBA P2/P3 sanity check.
- `bidi_controls_vector_preserves_lrm` — control characters are
  NOT stripped during reorder.

The full UCD `BidiTest.txt` (~40k entries) is the integration
bead's CI-lane responsibility; the hand-curated corpus is the
always-on regression net.

## Cursor + selection contract

In an RTL paragraph the **caret moves logically forward** (toward
the next logical character) but **visually leftward**. Selection
grows from the caret toward the anchor in **visual** order.

`BidiCursorMovement::LogicalForward` in an RTL run MUST produce a
visually-leftward step; `VisualRight` MUST always be visually
right regardless of run direction. The
`BidiCursorObservation.correct` field is what the integration
records.

## Bead acceptance status

| Acceptance item | Status |
|---|---|
| Audit BiDi entry points in current renderer | ✓ (frankenterm/bidi/ public API surveyed; `BidiContext` is the load-bearing entry point) |
| Hand-curated test corpus shipped | ✓ (8 vectors covering all 6 scenarios) |
| Per-scenario rendering byte-equal to golden | ⏳ requires GUI integration to call frankenterm_bidi + record observations |
| Includes pure RTL / LTR / mixed / numbers / controls / combining marks | ✓ (corpus covers all 6) |
| Cursor position correctness in RTL | ✓ contract pinned via `BidiCursorMovement` enum + `BidiCursorObservation` + `cursor_correctness_rate()` |
| Selection direction correctness in RTL | ✓ same contract |
| UCD BiDi test corpus passes | ⏳ integration bead's CI-lane (~40k entries) |
| Per-release attestation entry | ⏳ integration follow-on |

## Cross-references

- **Sibling fixtures** (same session pattern):
  `a11y_tree` (ft-mpc9b.10.1), `ime_caret` (ft-mpc9b.10.2),
  `color_management` (ft-mpc9b.10.3),
  `atlas_stability` (ft-mpc9b.1.1), `triple_buffer` (ft-d0ol8),
  `live_resize` (ft-mpc9b.2.1), `grid_reflow` (ft-mpc9b.2.3),
  `render_quality` (ft-mpc9b.2.2), `snap_back_fuzz` (ft-mpc9b.2.4),
  `wayland_frame_pacing` (ft-mpc9b.3.2).
- **Production UBA implementation:** `frankenterm/bidi/` —
  WezTerm-derived; this module DOES NOT replace it.
- **Cross-link in `grid_reflow.rs`:** that module noted
  "BiDi reordering — that's `A11Y.4` cross-link; this module
  handles the LTR wrap algorithm; BiDi is post-processing." —
  this is that A11Y.4 doc.

## What this is NOT

- The full UCD `BidiTest.txt` corpus runner — the integration
  bead's CI lane consumes it; this module is the always-on
  always-fast regression net for the renderer-side contract.
- A glue layer that calls `frankenterm_bidi::BidiContext`. That
  glue is the GUI integration bead's responsibility (which can
  reach into `frankenterm-bidi` directly without inverting the
  dep graph).
- A test that the renderer **actually renders** Arabic glyphs in
  the right pixels — that's the GPU regression harness
  (ft-ombfl). This module pins the BiDi *order* contract; the
  GPU harness pins the visual-pixel contract.
