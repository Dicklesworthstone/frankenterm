//! Kitty image decode pipeline — state machine + budget
//! envelope (br-ft-jwst6 sub-task 3 substrate slice).
//!
//! The `image` crate's PNG/zlib decode call lives at the
//! integration layer (frankenterm-gui or escape-parser); this
//! substrate ships the typed state machine + per-stage budget
//! decisions the integration drives. Splitting this out from
//! the actual decode call lets the pipeline flow be unit-
//! tested without bytes-on-wire and lets the decode-timeout
//! doctrine be enforced from one place.
//!
//! ## Pipeline shape
//!
//! ```text
//!     ┌──────────────┐
//!     │ SniffFormat  │  PNG magic check / format hint check
//!     └──────┬───────┘
//!            │
//!            v
//!     ┌──────────────┐  (only when the format is ZlibCompressed)
//!     │ ZlibInflate  │
//!     └──────┬───────┘
//!            │
//!            v
//!     ┌──────────────┐  PNG/zlib/raw → packed-pixel buffer
//!     │ DecodeBytes  │
//!     └──────┬───────┘
//!            │
//!            v
//!     ┌──────────────────┐  RGB → RGBA premultiply, etc.
//!     │ ConvertColorSpace│
//!     └──────┬───────────┘
//!            │
//!            v
//!     ┌──────────────┐
//!     │  AtlasReady  │  bytes ready for sparse_texture_atlas upload
//!     └──────────────┘
//! ```
//!
//! ## Bead invariants
//!
//! - **Total budget**: 500 ms wall-clock from sniff to
//!   atlas-ready. The integration wraps the whole pipeline
//!   in `runtime_async::timeout_with_cx(500ms, ...)`; on
//!   timeout the substrate emits
//!   [`DecodeOutcome::Timeout`].
//! - **Privacy**: bytes stay in-memory; the substrate's
//!   types deliberately don't include any disk-spool stage.
//! - **Format integrity**: the format-sniff stage is
//!   short-circuit. PNG magic mismatch → fail before any
//!   inflate/decode work. The substrate ensures that a
//!   raw-RGB stream with an attacker-supplied PNG-like
//!   prefix doesn't accidentally route through the PNG
//!   decoder.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::kitty_graphics::KittyImageFormat;

// ============================================================================
// PNG magic
// ============================================================================

/// PNG file signature: `\x89PNG\r\n\x1a\n` per the PNG spec.
pub const PNG_MAGIC: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// True iff the first 8 bytes of `bytes` match the PNG file
/// signature. Returns false for any shorter slice.
#[must_use]
pub fn sniff_png_header(bytes: &[u8]) -> bool {
    bytes.len() >= PNG_MAGIC.len() && bytes[..PNG_MAGIC.len()] == PNG_MAGIC
}

// ============================================================================
// Pipeline stages
// ============================================================================

/// Ordered stages the integration's decoder walks.
///
/// `SniffFormat` always runs first (cheap header check).
/// `ZlibInflate` runs only for `ZlibCompressed` payloads.
/// `DecodeBytes` is the heavyweight call (`image::load_from_memory`).
/// `ConvertColorSpace` exists so RGB → RGBA and premultiplied-alpha
/// transforms route through one canonical step. `AtlasReady`
/// is a sentinel — no work, just the terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DecodeStage {
    SniffFormat,
    ZlibInflate,
    DecodeBytes,
    ConvertColorSpace,
    AtlasReady,
}

impl DecodeStage {
    /// Stable string identifier for logging.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::SniffFormat => "sniff_format",
            Self::ZlibInflate => "zlib_inflate",
            Self::DecodeBytes => "decode_bytes",
            Self::ConvertColorSpace => "convert_color_space",
            Self::AtlasReady => "atlas_ready",
        }
    }

    /// True iff this stage runs the decode-thread budget — the
    /// substrate's [`crate::kitty_graphics_compositor::KittyFrameBudgetOp::DecodeAsync`]
    /// rule says these MUST run off the render thread.
    /// `AtlasReady` is a sentinel so it doesn't count.
    #[must_use]
    pub const fn runs_off_thread(self) -> bool {
        !matches!(self, Self::AtlasReady)
    }
}

// ============================================================================
// Decode plan — the ordered list of stages a given format walks
// ============================================================================

/// Ordered stages for a given input format. The integration
/// walks the plan in order, calling `evaluate_stage_step` at
/// each transition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DecodePlan {
    pub stages: Vec<DecodeStage>,
}

impl DecodePlan {
    #[must_use]
    pub fn first(&self) -> Option<DecodeStage> {
        self.stages.first().copied()
    }

    #[must_use]
    pub fn next_after(&self, current: DecodeStage) -> Option<DecodeStage> {
        let idx = self.stages.iter().position(|&s| s == current)?;
        self.stages.get(idx + 1).copied()
    }
}

/// Build the ordered stage list for a given input format.
///
/// - PNG: SniffFormat → DecodeBytes → ConvertColorSpace → AtlasReady
///   (color-space step exists because PNG's bit depths can yield
///   non-RGBA outputs).
/// - Rgb24: SniffFormat → ConvertColorSpace → AtlasReady
///   (no decode pass; bytes ARE the pixels).
/// - Rgba32: SniffFormat → AtlasReady (already in target format).
/// - ZlibCompressed: SniffFormat → ZlibInflate → ConvertColorSpace
///   → AtlasReady (the underlying format after inflate is
///   raw RGB or RGBA per the bead; the integration tells
///   ZlibInflate which target to produce).
#[must_use]
pub fn plan_decode_for_format(format: KittyImageFormat) -> DecodePlan {
    let stages = match format {
        KittyImageFormat::Png => vec![
            DecodeStage::SniffFormat,
            DecodeStage::DecodeBytes,
            DecodeStage::ConvertColorSpace,
            DecodeStage::AtlasReady,
        ],
        KittyImageFormat::Rgb24 => vec![
            DecodeStage::SniffFormat,
            DecodeStage::ConvertColorSpace,
            DecodeStage::AtlasReady,
        ],
        KittyImageFormat::Rgba32 => {
            vec![DecodeStage::SniffFormat, DecodeStage::AtlasReady]
        }
        KittyImageFormat::ZlibCompressed => vec![
            DecodeStage::SniffFormat,
            DecodeStage::ZlibInflate,
            DecodeStage::ConvertColorSpace,
            DecodeStage::AtlasReady,
        ],
    };
    DecodePlan { stages }
}

// ============================================================================
// Decode budget
// ============================================================================

/// Per-decode timeout envelope. The bead pins the total budget
/// at 500 ms from sniff to atlas-ready; the substrate also
/// caps any single stage at `per_stage_max_ms` so a hung zlib
/// stream can't burn the whole budget on inflate before the
/// decode call even starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeBudget {
    pub total_max_ms: u64,
    pub per_stage_max_ms: u64,
}

pub const DEFAULT_TOTAL_MAX_MS: u64 = 500;
pub const DEFAULT_PER_STAGE_MAX_MS: u64 = 100;

impl Default for DecodeBudget {
    fn default() -> Self {
        Self {
            total_max_ms: DEFAULT_TOTAL_MAX_MS,
            per_stage_max_ms: DEFAULT_PER_STAGE_MAX_MS,
        }
    }
}

impl DecodeBudget {
    /// Repair invariants on adversarially-supplied budgets.
    /// Total budget capped at 5 seconds (no legitimate kitty
    /// payload needs longer); per-stage capped at total. Zero
    /// budgets bumped to 1 ms so the pipeline doesn't trivially
    /// short-circuit on any operation.
    #[must_use]
    pub fn with_repaired_invariants(mut self) -> Self {
        if self.total_max_ms == 0 {
            self.total_max_ms = 1;
        }
        if self.total_max_ms > 5_000 {
            self.total_max_ms = 5_000;
        }
        if self.per_stage_max_ms == 0 {
            self.per_stage_max_ms = 1;
        }
        if self.per_stage_max_ms > self.total_max_ms {
            self.per_stage_max_ms = self.total_max_ms;
        }
        self
    }

    /// Convenience: total budget as `Duration` for
    /// [`runtime_async::timeout_with_cx`].
    #[must_use]
    pub const fn total_max_duration(&self) -> Duration {
        Duration::from_millis(self.total_max_ms)
    }
}

// ============================================================================
// Decode outcomes
// ============================================================================

/// Terminal-state outcomes the integration emits when the
/// pipeline finishes (either successfully or via a guard).
/// Routed into the substrate's
/// [`crate::kitty_graphics::ImageRejectionReason`] taxonomy
/// at the integration's reject site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DecodeOutcome {
    /// Pipeline ran every stage cleanly; bytes uploaded to atlas.
    Success,
    /// Format-sniff failed: PNG magic missing for `f=100`, or
    /// raw-format byte length doesn't match width × height ×
    /// channels.
    MalformedHeader,
    /// Format hint named an unrecognized variant. Should never
    /// fire if the parser already filtered against
    /// `KittyImageFormat`.
    UnsupportedFormat,
    /// Decode produced fewer bytes than the geometry implied —
    /// the input was truncated mid-stream.
    Truncated,
    /// Total or per-stage budget exhausted.
    Timeout { stage: DecodeStage },
    /// Zlib stream corrupt (CRC mismatch, premature EOF inside
    /// inflate). Surfaced by the underlying decoder.
    ZlibCorrupted,
}

impl DecodeOutcome {
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Success)
    }

    /// True iff this outcome should bump the integration's
    /// rejection counter (any non-Success result).
    #[must_use]
    pub const fn is_rejection(self) -> bool {
        !matches!(self, Self::Success)
    }
}

// ============================================================================
// Per-stage decisions
// ============================================================================

/// Decision the integration applies after running one stage's
/// work and observing how long it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StageDecision {
    /// Stage completed under budget; advance to the next stage
    /// in the plan.
    Advance,
    /// Stage completed but the plan has no more stages —
    /// pipeline done.
    Done,
    /// Stage exceeded its per-stage budget; abort.
    AbortPerStage,
    /// Cumulative elapsed exceeded the total budget; abort.
    AbortTotal,
}

/// Decide what to do after a stage call returned.
///
/// `stage_elapsed_ms` is the wall-clock the stage itself took.
/// `total_elapsed_ms` is the cumulative time since pipeline
/// start. Both budgets are checked; per-stage overrun yields
/// `AbortPerStage`, total overrun yields `AbortTotal`, and a
/// stage with no successor yields `Done`.
#[must_use]
pub fn evaluate_stage_step(
    plan: &DecodePlan,
    current: DecodeStage,
    stage_elapsed_ms: u64,
    total_elapsed_ms: u64,
    budget: DecodeBudget,
) -> StageDecision {
    let budget = budget.with_repaired_invariants();
    if stage_elapsed_ms > budget.per_stage_max_ms {
        return StageDecision::AbortPerStage;
    }
    if total_elapsed_ms > budget.total_max_ms {
        return StageDecision::AbortTotal;
    }
    match plan.next_after(current) {
        Some(_) => StageDecision::Advance,
        None => StageDecision::Done,
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodePipelineTelemetry {
    pub successes: u64,
    pub malformed_header: u64,
    pub unsupported_format: u64,
    pub truncated: u64,
    pub zlib_corrupted: u64,
    pub timeouts_sniff_format: u64,
    pub timeouts_zlib_inflate: u64,
    pub timeouts_decode_bytes: u64,
    pub timeouts_convert_color_space: u64,
    pub abort_per_stage_total: u64,
    pub abort_total_total: u64,
}

impl DecodePipelineTelemetry {
    pub fn record_outcome(&mut self, outcome: DecodeOutcome) {
        match outcome {
            DecodeOutcome::Success => {
                self.successes = self.successes.saturating_add(1);
            }
            DecodeOutcome::MalformedHeader => {
                self.malformed_header = self.malformed_header.saturating_add(1);
            }
            DecodeOutcome::UnsupportedFormat => {
                self.unsupported_format = self.unsupported_format.saturating_add(1);
            }
            DecodeOutcome::Truncated => {
                self.truncated = self.truncated.saturating_add(1);
            }
            DecodeOutcome::ZlibCorrupted => {
                self.zlib_corrupted = self.zlib_corrupted.saturating_add(1);
            }
            DecodeOutcome::Timeout { stage } => {
                let slot = match stage {
                    DecodeStage::SniffFormat => &mut self.timeouts_sniff_format,
                    DecodeStage::ZlibInflate => &mut self.timeouts_zlib_inflate,
                    DecodeStage::DecodeBytes => &mut self.timeouts_decode_bytes,
                    DecodeStage::ConvertColorSpace => &mut self.timeouts_convert_color_space,
                    // AtlasReady is a sentinel; a Timeout there
                    // would be a logic bug. Route to total-abort
                    // counter so it shows up in telemetry instead
                    // of being silently dropped.
                    DecodeStage::AtlasReady => &mut self.abort_total_total,
                };
                *slot = slot.saturating_add(1);
            }
        }
    }

    pub fn record_decision(&mut self, decision: StageDecision) {
        match decision {
            StageDecision::AbortPerStage => {
                self.abort_per_stage_total = self.abort_per_stage_total.saturating_add(1);
            }
            StageDecision::AbortTotal => {
                self.abort_total_total = self.abort_total_total.saturating_add(1);
            }
            StageDecision::Advance | StageDecision::Done => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // sniff_png_header
    // ----------------------------------------------------------------

    #[test]
    fn sniff_recognises_png_magic() {
        let mut bytes = PNG_MAGIC.to_vec();
        bytes.extend_from_slice(b"...IHDR data...");
        assert!(sniff_png_header(&bytes));
    }

    #[test]
    fn sniff_rejects_short_buffer() {
        assert!(!sniff_png_header(&PNG_MAGIC[..7]));
        assert!(!sniff_png_header(&[]));
    }

    #[test]
    fn sniff_rejects_almost_png() {
        // First byte off by one (0x88 instead of 0x89).
        let mut bad = PNG_MAGIC;
        bad[0] = 0x88;
        assert!(!sniff_png_header(&bad));
    }

    #[test]
    fn sniff_rejects_jpeg_header() {
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F'];
        assert!(!sniff_png_header(&jpeg));
    }

    // ----------------------------------------------------------------
    // DecodePlan
    // ----------------------------------------------------------------

    #[test]
    fn plan_for_png_includes_sniff_decode_convert_atlas() {
        let plan = plan_decode_for_format(KittyImageFormat::Png);
        assert_eq!(
            plan.stages,
            vec![
                DecodeStage::SniffFormat,
                DecodeStage::DecodeBytes,
                DecodeStage::ConvertColorSpace,
                DecodeStage::AtlasReady,
            ]
        );
    }

    #[test]
    fn plan_for_rgba32_skips_decode_and_convert() {
        let plan = plan_decode_for_format(KittyImageFormat::Rgba32);
        assert_eq!(
            plan.stages,
            vec![DecodeStage::SniffFormat, DecodeStage::AtlasReady]
        );
    }

    #[test]
    fn plan_for_rgb24_includes_convert_step() {
        let plan = plan_decode_for_format(KittyImageFormat::Rgb24);
        assert!(plan.stages.contains(&DecodeStage::ConvertColorSpace));
        assert!(!plan.stages.contains(&DecodeStage::DecodeBytes));
    }

    #[test]
    fn plan_for_zlib_routes_through_inflate_step() {
        let plan = plan_decode_for_format(KittyImageFormat::ZlibCompressed);
        assert_eq!(plan.stages.first(), Some(&DecodeStage::SniffFormat));
        assert!(plan.stages.contains(&DecodeStage::ZlibInflate));
        assert_eq!(plan.stages.last(), Some(&DecodeStage::AtlasReady));
    }

    #[test]
    fn plan_next_after_walks_in_order() {
        let plan = plan_decode_for_format(KittyImageFormat::Png);
        assert_eq!(
            plan.next_after(DecodeStage::SniffFormat),
            Some(DecodeStage::DecodeBytes)
        );
        assert_eq!(
            plan.next_after(DecodeStage::DecodeBytes),
            Some(DecodeStage::ConvertColorSpace)
        );
        assert_eq!(
            plan.next_after(DecodeStage::ConvertColorSpace),
            Some(DecodeStage::AtlasReady)
        );
        assert_eq!(plan.next_after(DecodeStage::AtlasReady), None);
    }

    #[test]
    fn plan_next_after_returns_none_for_stage_not_in_plan() {
        let plan = plan_decode_for_format(KittyImageFormat::Rgba32);
        // Rgba32 plan doesn't include DecodeBytes.
        assert_eq!(plan.next_after(DecodeStage::DecodeBytes), None);
    }

    // ----------------------------------------------------------------
    // DecodeStage helpers
    // ----------------------------------------------------------------

    #[test]
    fn stage_label_is_stable_and_unique() {
        let labels = [
            DecodeStage::SniffFormat.label(),
            DecodeStage::ZlibInflate.label(),
            DecodeStage::DecodeBytes.label(),
            DecodeStage::ConvertColorSpace.label(),
            DecodeStage::AtlasReady.label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }

    #[test]
    fn atlas_ready_is_sentinel_and_doesnt_run_off_thread() {
        assert!(!DecodeStage::AtlasReady.runs_off_thread());
        assert!(DecodeStage::SniffFormat.runs_off_thread());
        assert!(DecodeStage::ZlibInflate.runs_off_thread());
        assert!(DecodeStage::DecodeBytes.runs_off_thread());
        assert!(DecodeStage::ConvertColorSpace.runs_off_thread());
    }

    // ----------------------------------------------------------------
    // DecodeBudget
    // ----------------------------------------------------------------

    #[test]
    fn budget_default_matches_bead() {
        let b = DecodeBudget::default();
        assert_eq!(b.total_max_ms, 500);
        assert_eq!(b.per_stage_max_ms, 100);
    }

    #[test]
    fn budget_repair_clamps_zero_total() {
        let b = DecodeBudget {
            total_max_ms: 0,
            per_stage_max_ms: 100,
        }
        .with_repaired_invariants();
        assert_eq!(b.total_max_ms, 1);
        // per_stage was 100 but total was 1 after clamp; per_stage
        // is then clamped DOWN to total.
        assert_eq!(b.per_stage_max_ms, 1);
    }

    #[test]
    fn budget_repair_caps_runaway_total() {
        let b = DecodeBudget {
            total_max_ms: u64::MAX,
            per_stage_max_ms: 100,
        }
        .with_repaired_invariants();
        assert_eq!(b.total_max_ms, 5_000);
    }

    #[test]
    fn budget_repair_clamps_per_stage_above_total() {
        let b = DecodeBudget {
            total_max_ms: 200,
            per_stage_max_ms: 1_000,
        }
        .with_repaired_invariants();
        assert_eq!(b.total_max_ms, 200);
        assert_eq!(b.per_stage_max_ms, 200);
    }

    #[test]
    fn budget_repair_clamps_zero_per_stage() {
        let b = DecodeBudget {
            total_max_ms: 500,
            per_stage_max_ms: 0,
        }
        .with_repaired_invariants();
        assert_eq!(b.per_stage_max_ms, 1);
    }

    #[test]
    fn budget_total_max_duration_matches_ms() {
        let b = DecodeBudget {
            total_max_ms: 250,
            per_stage_max_ms: 50,
        };
        assert_eq!(b.total_max_duration(), Duration::from_millis(250));
    }

    // ----------------------------------------------------------------
    // evaluate_stage_step
    // ----------------------------------------------------------------

    #[test]
    fn step_advances_when_under_both_budgets_and_more_stages_remain() {
        let plan = plan_decode_for_format(KittyImageFormat::Png);
        let budget = DecodeBudget::default();
        let dec = evaluate_stage_step(&plan, DecodeStage::DecodeBytes, 50, 200, budget);
        assert_eq!(dec, StageDecision::Advance);
    }

    #[test]
    fn step_done_at_terminal_stage() {
        let plan = plan_decode_for_format(KittyImageFormat::Png);
        let budget = DecodeBudget::default();
        let dec = evaluate_stage_step(&plan, DecodeStage::AtlasReady, 1, 400, budget);
        assert_eq!(dec, StageDecision::Done);
    }

    #[test]
    fn step_aborts_on_per_stage_overrun() {
        let plan = plan_decode_for_format(KittyImageFormat::Png);
        let budget = DecodeBudget::default();
        let dec = evaluate_stage_step(&plan, DecodeStage::DecodeBytes, 150, 200, budget);
        assert_eq!(dec, StageDecision::AbortPerStage);
    }

    #[test]
    fn step_aborts_on_total_overrun() {
        let plan = plan_decode_for_format(KittyImageFormat::Png);
        let budget = DecodeBudget::default();
        let dec = evaluate_stage_step(&plan, DecodeStage::DecodeBytes, 50, 600, budget);
        assert_eq!(dec, StageDecision::AbortTotal);
    }

    #[test]
    fn step_per_stage_takes_priority_when_both_overrun() {
        // If both budgets are blown, per-stage is the more
        // specific signal — operator wants to know which stage
        // hung.
        let plan = plan_decode_for_format(KittyImageFormat::Png);
        let budget = DecodeBudget::default();
        let dec = evaluate_stage_step(&plan, DecodeStage::DecodeBytes, 200, 600, budget);
        assert_eq!(dec, StageDecision::AbortPerStage);
    }

    #[test]
    fn step_repairs_adversarial_budget_before_evaluating() {
        // Adversarial total=0 would auto-abort everything; the
        // repair raises it to 1 so a 0-elapsed stage still
        // advances.
        let plan = plan_decode_for_format(KittyImageFormat::Png);
        let bad = DecodeBudget {
            total_max_ms: 0,
            per_stage_max_ms: 0,
        };
        let dec = evaluate_stage_step(&plan, DecodeStage::DecodeBytes, 0, 0, bad);
        assert_eq!(dec, StageDecision::Advance);
    }

    // ----------------------------------------------------------------
    // DecodeOutcome
    // ----------------------------------------------------------------

    #[test]
    fn outcome_predicates() {
        assert!(DecodeOutcome::Success.is_success());
        assert!(!DecodeOutcome::Success.is_rejection());
        assert!(DecodeOutcome::MalformedHeader.is_rejection());
        assert!(DecodeOutcome::Truncated.is_rejection());
        assert!(DecodeOutcome::ZlibCorrupted.is_rejection());
        assert!(
            DecodeOutcome::Timeout {
                stage: DecodeStage::DecodeBytes
            }
            .is_rejection()
        );
    }

    // ----------------------------------------------------------------
    // Telemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_records_each_outcome_in_its_slot() {
        let mut t = DecodePipelineTelemetry::default();
        t.record_outcome(DecodeOutcome::Success);
        t.record_outcome(DecodeOutcome::MalformedHeader);
        t.record_outcome(DecodeOutcome::UnsupportedFormat);
        t.record_outcome(DecodeOutcome::Truncated);
        t.record_outcome(DecodeOutcome::ZlibCorrupted);
        t.record_outcome(DecodeOutcome::Timeout {
            stage: DecodeStage::DecodeBytes,
        });
        t.record_outcome(DecodeOutcome::Timeout {
            stage: DecodeStage::ZlibInflate,
        });
        assert_eq!(t.successes, 1);
        assert_eq!(t.malformed_header, 1);
        assert_eq!(t.unsupported_format, 1);
        assert_eq!(t.truncated, 1);
        assert_eq!(t.zlib_corrupted, 1);
        assert_eq!(t.timeouts_decode_bytes, 1);
        assert_eq!(t.timeouts_zlib_inflate, 1);
    }

    #[test]
    fn telemetry_records_decisions() {
        let mut t = DecodePipelineTelemetry::default();
        t.record_decision(StageDecision::Advance);
        t.record_decision(StageDecision::Done);
        t.record_decision(StageDecision::AbortPerStage);
        t.record_decision(StageDecision::AbortTotal);
        // Advance + Done are normal operations, not counted.
        assert_eq!(t.abort_per_stage_total, 1);
        assert_eq!(t.abort_total_total, 1);
    }

    #[test]
    fn telemetry_serde_roundtrip() {
        let mut t = DecodePipelineTelemetry::default();
        t.record_outcome(DecodeOutcome::Success);
        t.record_outcome(DecodeOutcome::Timeout {
            stage: DecodeStage::ConvertColorSpace,
        });
        let json = serde_json::to_string(&t).unwrap();
        let back: DecodePipelineTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t);
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic integration walk
    // ----------------------------------------------------------------

    #[test]
    fn scenario_clean_png_walk_completes() {
        // Simulate the integration walking a PNG image through
        // every stage well under budget.
        let plan = plan_decode_for_format(KittyImageFormat::Png);
        let budget = DecodeBudget::default();
        let mut tlm = DecodePipelineTelemetry::default();

        let mut current = plan.first().expect("plan must have a first stage");
        let mut total = 0_u64;
        loop {
            let stage_elapsed = 10; // Each stage 10 ms.
            total += stage_elapsed;
            let dec = evaluate_stage_step(&plan, current, stage_elapsed, total, budget);
            tlm.record_decision(dec);
            match dec {
                StageDecision::Advance => {
                    current = plan
                        .next_after(current)
                        .expect("Advance implies a next stage exists");
                }
                StageDecision::Done => break,
                StageDecision::AbortPerStage | StageDecision::AbortTotal => {
                    panic!("clean PNG walk must not abort");
                }
            }
        }
        tlm.record_outcome(DecodeOutcome::Success);
        assert_eq!(tlm.successes, 1);
        assert_eq!(tlm.abort_per_stage_total, 0);
        assert_eq!(tlm.abort_total_total, 0);
    }

    #[test]
    fn scenario_hung_zlib_inflate_aborts_with_per_stage() {
        // Adversarial zlib stream: SniffFormat clean, but the
        // ZlibInflate stage exceeds its 100 ms per-stage cap.
        let plan = plan_decode_for_format(KittyImageFormat::ZlibCompressed);
        let budget = DecodeBudget::default();
        let mut tlm = DecodePipelineTelemetry::default();

        // Stage 1: SniffFormat — 5 ms, under budget.
        let dec1 = evaluate_stage_step(&plan, DecodeStage::SniffFormat, 5, 5, budget);
        assert_eq!(dec1, StageDecision::Advance);
        tlm.record_decision(dec1);

        // Stage 2: ZlibInflate — 250 ms hangs the per-stage cap.
        let dec2 = evaluate_stage_step(&plan, DecodeStage::ZlibInflate, 250, 255, budget);
        assert_eq!(dec2, StageDecision::AbortPerStage);
        tlm.record_decision(dec2);
        tlm.record_outcome(DecodeOutcome::Timeout {
            stage: DecodeStage::ZlibInflate,
        });

        assert_eq!(tlm.timeouts_zlib_inflate, 1);
        assert_eq!(tlm.abort_per_stage_total, 1);
    }
}
