//! Dirty-line telemetry substrate (ft-5ykn9 / BR-TERM-EMULATOR-UPLIFT.1.2.cont2).
//!
//! Pure-logic substrate covering the substrate-shaped pieces of
//! the bead: the dirty-event source taxonomy, frame-aggregate
//! histogram for the RQ-S1/RQ-S8 SLO checks (per-frame p99
//! <0.1 ms), pane-lifecycle dirty-set bookkeeping, and the
//! stable-row → visible-row translation policy.
//!
//! ## What this module ships
//!
//! - `DirtyEventSource` — bead-cited 8-variant enum
//!   (`Pty / CursorMove / SelectionChange / ThemeSwap / FontSwap
//!   / StatusTileUpdate / FocusChange / Resize`) +
//!   `is_whole_screen` predicate distinguishing per-line marks
//!   from full-screen invalidations.
//! - `DirtyMark` per-event payload `{ pane_id, source, lines }`
//!   with cardinality helper.
//! - `RowTranslation` — pure-logic stable-row → visible-row
//!   translation used by the PTY-write dirty source. Defensive
//!   `None` when the stable row is outside the renderable
//!   window (scrolled away).
//! - `DirtyLinesPerFrameHistogram` — bounded-bucket histogram
//!   for the bead's "dirty_lines_per_frame histogram"
//!   telemetry. Buckets log-spaced from 0 to 4096+.
//! - `DirtyMarkClassification` — `Single / Range / WholeScreen`
//!   for per-mark cost classification.
//! - `DirtyTelemetryConfig` — RQ-S1/RQ-S8 acceptance target
//!   (per-frame p99 <0.1 ms = 100 µs default).
//! - `DirtyLineTelemetry` aggregate counters per the bead's
//!   structured-logging schema (`dirty_marks_total`,
//!   `frames_cleared_total`, `clean_lines_skipped`,
//!   `frames_over_budget_p99`).
//! - `should_clear_at_frame_end` — pure predicate gating the
//!   frame-end `bitmap.clear()` (off when a coarse
//!   invalidation force-marks the next frame).
//!
//! ## What is deferred to ft-5ykn9 follow-up
//!
//! - Replacing full visible-row iteration with `iter_dirty()`
//!   in `crates/frankenterm-gui/src/termwindow/render/
//!   pane.rs` and `screen_line.rs`.
//! - Frame-end `bitmap.clear()` in `paint_impl` after Present.
//! - Wiring all 8 event sources into `mark_dirty_line` /
//!   `mark_range`.
//! - `quad_generation` lower-bound migration on top of per-line
//!   dirty (most `+= 1` sites drop in favour of per-line marks).
//! - Per-pane `forget_dirty_lines_for_pane` hook on pane close.
//! - Benches at `crates/frankenterm-core/benches/
//!   dirty_tracking_typing.rs` (200-pane, 1 cell/frame).

#![allow(dead_code)]

// ============================================================================
// Dirty-event source taxonomy
// ============================================================================

/// The bead's 8-variant event source taxonomy. Per the bead:
/// "Six remaining dirty-event sources from ft-mpc9b.1.2 body
/// (PTY writes, cursor moves, selection changes, theme/font
/// swap, status-tile updates) plus the existing two
/// (focus_changed + resize)".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DirtyEventSource {
    /// PTY write — translate `stable_row` to `visible_row` via
    /// `RowTranslation` and mark per-line.
    Pty,
    /// Cursor move — mark old row + new row.
    CursorMove,
    /// Selection change — mark the affected range.
    SelectionChange,
    /// Theme swap — coarse-invalidates all panes (force
    /// repaint on next frame).
    ThemeSwap,
    /// Font swap — coarse-invalidates all panes (font metrics
    /// change cell sizes).
    FontSwap,
    /// Status-tile update — marks the status-bar row only.
    StatusTileUpdate,
    /// Focus change — coarse-invalidates the focused + the
    /// previously-focused pane.
    FocusChange,
    /// Resize — coarse-invalidates affected panes.
    Resize,
}

impl DirtyEventSource {
    /// Whether this event triggers a whole-screen invalidation
    /// rather than per-line marks. Substrate uses this to
    /// classify the mark + decide if `should_clear_at_frame_end`
    /// should defer.
    #[must_use]
    pub const fn is_whole_screen(self) -> bool {
        matches!(
            self,
            Self::ThemeSwap | Self::FontSwap | Self::FocusChange | Self::Resize
        )
    }

    /// Stable string label for telemetry / logging.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pty => "pty",
            Self::CursorMove => "cursor_move",
            Self::SelectionChange => "selection_change",
            Self::ThemeSwap => "theme_swap",
            Self::FontSwap => "font_swap",
            Self::StatusTileUpdate => "status_tile_update",
            Self::FocusChange => "focus_change",
            Self::Resize => "resize",
        }
    }
}

// ============================================================================
// DirtyMark payload + classification
// ============================================================================

/// A single dirty-mark event. The integration's mark sites
/// build one of these and dispatch into the per-pane bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirtyMark {
    pub pane_id: u64,
    pub source: DirtyEventSource,
    /// Range of visible rows touched (start..end, end-exclusive).
    /// For single-row marks, `end == start + 1`.
    pub start_row: u32,
    pub end_row: u32,
}

impl DirtyMark {
    /// Number of rows touched by this mark.
    #[must_use]
    pub fn row_count(&self) -> u32 {
        self.end_row.saturating_sub(self.start_row)
    }

    #[must_use]
    pub fn classify(&self, total_rows: u32) -> DirtyMarkClassification {
        if self.source.is_whole_screen() {
            return DirtyMarkClassification::WholeScreen;
        }
        match self.row_count() {
            0 | 1 => DirtyMarkClassification::Single,
            n if n >= total_rows => DirtyMarkClassification::WholeScreen,
            _ => DirtyMarkClassification::Range,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DirtyMarkClassification {
    /// One row touched (cursor blink, single-cell PTY write).
    Single,
    /// Multiple rows touched (selection drag, multi-line PTY
    /// write).
    Range,
    /// Whole-screen invalidation (theme, font, resize, focus).
    WholeScreen,
}

// ============================================================================
// Stable-row → visible-row translation
// ============================================================================

/// Pure-logic translation from a `stable_row` (in scrollback +
/// viewport coordinates) to a `visible_row` (renderable
/// window). Bead: "PTY writes (translate stable_row to
/// visible_row via RenderableDimensions)".
///
/// `viewport_top_stable_row` is the stable-row index of the
/// topmost visible row; `visible_rows` is the height of the
/// renderable window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowTranslation {
    pub viewport_top_stable_row: i64,
    pub visible_rows: u32,
}

impl RowTranslation {
    /// Translate `stable_row` to `visible_row`. Returns `None`
    /// when the stable row is above the viewport (scrolled out
    /// of view) or below it (off the bottom).
    #[must_use]
    pub fn translate(&self, stable_row: i64) -> Option<u32> {
        if stable_row < self.viewport_top_stable_row {
            return None;
        }
        let offset = stable_row.saturating_sub(self.viewport_top_stable_row);
        // Defensive: u32 conversion only when within visible range.
        if offset >= self.visible_rows as i64 {
            return None;
        }
        // Safe: 0 <= offset < visible_rows ≤ u32::MAX.
        Some(offset as u32)
    }
}

// ============================================================================
// Dirty-lines-per-frame histogram
// ============================================================================

/// Bounded-bucket histogram for the bead's
/// "dirty_lines_per_frame histogram" telemetry. Buckets:
///   0:   [0,    2)
///   1:   [2,    8)
///   2:   [8,    32)
///   3:   [32,  128)
///   4:  [128,  512)
///   5:  [512, 2048)
///   6: [2048, ∞)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirtyLinesPerFrameHistogram {
    pub buckets: [u64; 7],
    pub total: u64,
}

const DIRTY_BUCKET_BOUNDARIES: [u32; 7] = [2, 8, 32, 128, 512, 2_048, u32::MAX];

impl DirtyLinesPerFrameHistogram {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buckets: [0; 7],
            total: 0,
        }
    }

    pub fn record(&mut self, dirty_lines: u32) {
        let bucket = Self::bucket_for(dirty_lines);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.total = self.total.saturating_add(1);
    }

    #[must_use]
    pub fn bucket_for(dirty_lines: u32) -> usize {
        for (i, boundary) in DIRTY_BUCKET_BOUNDARIES.iter().enumerate() {
            if dirty_lines < *boundary {
                return i;
            }
        }
        6
    }

    /// Percentile in `[0..=100]`. Returns the upper bound of
    /// the bucket containing the percentile sample.
    #[must_use]
    pub fn percentile_lines(&self, p: u8) -> Option<u32> {
        if self.total == 0 {
            return None;
        }
        let p = p.min(100) as u64;
        let target = (self.total * p).div_ceil(100).max(1);
        let mut cumulative = 0u64;
        for (i, count) in self.buckets.iter().enumerate() {
            cumulative = cumulative.saturating_add(*count);
            if cumulative >= target {
                return Some(DIRTY_BUCKET_BOUNDARIES[i]);
            }
        }
        Some(u32::MAX)
    }
}

// ============================================================================
// Acceptance target — RQ-S1 / RQ-S8 SLO
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyTelemetryConfig {
    /// Per-frame paint p99 budget in microseconds. RQ-S1/RQ-S8
    /// target: 100 µs (= 0.1 ms) when 1 cell changes per frame.
    pub p99_budget_us: u32,
    /// When true, frame-end `bitmap.clear()` is allowed. The
    /// integration sets this false during a coarse invalidation
    /// to keep the bitmap force-marked across the boundary.
    pub clear_allowed_at_frame_end: bool,
}

pub const RQS1_RQS8_P99_BUDGET_US: u32 = 100;

impl Default for DirtyTelemetryConfig {
    fn default() -> Self {
        Self {
            p99_budget_us: RQS1_RQS8_P99_BUDGET_US,
            clear_allowed_at_frame_end: true,
        }
    }
}

/// Frame-paint duration histogram for SLO checks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FramePaintLatencyHistogram {
    /// Buckets log-spaced from 10 µs to 10 ms.
    pub buckets: [u64; 7],
    pub total: u64,
}

const PAINT_LATENCY_BOUNDARIES_US: [u32; 7] = [10, 50, 100, 500, 1_000, 5_000, u32::MAX];

impl FramePaintLatencyHistogram {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buckets: [0; 7],
            total: 0,
        }
    }

    pub fn record(&mut self, paint_us: u32) {
        let bucket = Self::bucket_for(paint_us);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.total = self.total.saturating_add(1);
    }

    #[must_use]
    pub fn bucket_for(paint_us: u32) -> usize {
        for (i, boundary) in PAINT_LATENCY_BOUNDARIES_US.iter().enumerate() {
            if paint_us < *boundary {
                return i;
            }
        }
        6
    }

    #[must_use]
    pub fn percentile_us(&self, p: u8) -> Option<u32> {
        if self.total == 0 {
            return None;
        }
        let p = p.min(100) as u64;
        let target = (self.total * p).div_ceil(100).max(1);
        let mut cumulative = 0u64;
        for (i, count) in self.buckets.iter().enumerate() {
            cumulative = cumulative.saturating_add(*count);
            if cumulative >= target {
                return Some(PAINT_LATENCY_BOUNDARIES_US[i]);
            }
        }
        Some(u32::MAX)
    }

    /// RQ-S1/RQ-S8 acceptance: p99 frame-paint <100 µs.
    #[must_use]
    pub fn meets_p99_target(&self, config: DirtyTelemetryConfig) -> bool {
        match self.percentile_us(99) {
            None => true, // empty = trivially passes
            Some(us) => us <= config.p99_budget_us,
        }
    }
}

// ============================================================================
// Frame-end clear predicate
// ============================================================================

/// Pure-logic predicate: should the integration call
/// `bitmap.clear()` at frame end?
///
/// Per the bead: most frames yes; coarse-invalidation frames
/// (theme/font/resize/focus) leave the bitmap dirty so the next
/// frame still observes the marks.
#[must_use]
pub fn should_clear_at_frame_end(
    last_event_was_whole_screen: bool,
    config: DirtyTelemetryConfig,
) -> bool {
    config.clear_allowed_at_frame_end && !last_event_was_whole_screen
}

// ============================================================================
// Aggregate telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirtyLineTelemetry {
    /// Total dirty marks emitted (across all sources, all panes).
    pub dirty_marks_total: u64,
    /// Frames where `bitmap.clear()` was called at frame end.
    pub frames_cleared_total: u64,
    /// Lines the renderer skipped because they weren't dirty
    /// (cumulative across all frames). Bead's
    /// "clean_lines_skipped" counter.
    pub clean_lines_skipped: u64,
    /// Frames where paint exceeded the RQ-S1/RQ-S8 budget.
    pub frames_over_budget: u64,
    /// Per-source mark counts for the bead's structured logging.
    pub marks_by_source: MarksBySource,
    /// Per-frame dirty-line distribution.
    pub dirty_lines_per_frame: DirtyLinesPerFrameHistogram,
    /// Per-frame paint-latency distribution.
    pub paint_latency: FramePaintLatencyHistogram,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarksBySource {
    pub pty: u64,
    pub cursor_move: u64,
    pub selection_change: u64,
    pub theme_swap: u64,
    pub font_swap: u64,
    pub status_tile_update: u64,
    pub focus_change: u64,
    pub resize: u64,
}

impl MarksBySource {
    pub fn record(&mut self, source: DirtyEventSource) {
        let slot = match source {
            DirtyEventSource::Pty => &mut self.pty,
            DirtyEventSource::CursorMove => &mut self.cursor_move,
            DirtyEventSource::SelectionChange => &mut self.selection_change,
            DirtyEventSource::ThemeSwap => &mut self.theme_swap,
            DirtyEventSource::FontSwap => &mut self.font_swap,
            DirtyEventSource::StatusTileUpdate => &mut self.status_tile_update,
            DirtyEventSource::FocusChange => &mut self.focus_change,
            DirtyEventSource::Resize => &mut self.resize,
        };
        *slot = slot.saturating_add(1);
    }
}

impl DirtyLineTelemetry {
    pub fn record_mark(&mut self, mark: &DirtyMark) {
        self.dirty_marks_total = self.dirty_marks_total.saturating_add(1);
        self.marks_by_source.record(mark.source);
    }

    pub fn record_frame_end(
        &mut self,
        dirty_lines_in_frame: u32,
        clean_lines_skipped_in_frame: u32,
        paint_us: u32,
        cleared: bool,
        config: DirtyTelemetryConfig,
    ) {
        self.dirty_lines_per_frame.record(dirty_lines_in_frame);
        self.paint_latency.record(paint_us);
        self.clean_lines_skipped = self
            .clean_lines_skipped
            .saturating_add(clean_lines_skipped_in_frame as u64);
        if cleared {
            self.frames_cleared_total = self.frames_cleared_total.saturating_add(1);
        }
        // Self-review fix (br-ft-pmiis): bead's RQ-S1 says
        // 'p99 <100 µs' (strict less-than). A paint at exactly
        // 100 µs violates the strict bound, so >= rather than
        // > makes the per-frame counter consistent with the
        // bucket-aware meets_p99_target predicate.
        if paint_us >= config.p99_budget_us {
            self.frames_over_budget = self.frames_over_budget.saturating_add(1);
        }
    }

    #[must_use]
    pub fn paint_p99_meets_target(&self, config: DirtyTelemetryConfig) -> bool {
        self.paint_latency.meets_p99_target(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(pane_id: u64, source: DirtyEventSource, start: u32, end: u32) -> DirtyMark {
        DirtyMark {
            pane_id,
            source,
            start_row: start,
            end_row: end,
        }
    }

    // ----------------------------------------------------------------
    // DirtyEventSource
    // ----------------------------------------------------------------

    #[test]
    fn source_whole_screen_classification() {
        assert!(DirtyEventSource::ThemeSwap.is_whole_screen());
        assert!(DirtyEventSource::FontSwap.is_whole_screen());
        assert!(DirtyEventSource::FocusChange.is_whole_screen());
        assert!(DirtyEventSource::Resize.is_whole_screen());
        assert!(!DirtyEventSource::Pty.is_whole_screen());
        assert!(!DirtyEventSource::CursorMove.is_whole_screen());
        assert!(!DirtyEventSource::SelectionChange.is_whole_screen());
        assert!(!DirtyEventSource::StatusTileUpdate.is_whole_screen());
    }

    #[test]
    fn source_label_stable() {
        assert_eq!(DirtyEventSource::Pty.label(), "pty");
        assert_eq!(DirtyEventSource::CursorMove.label(), "cursor_move");
        assert_eq!(DirtyEventSource::ThemeSwap.label(), "theme_swap");
        assert_eq!(DirtyEventSource::Resize.label(), "resize");
    }

    // ----------------------------------------------------------------
    // DirtyMark
    // ----------------------------------------------------------------

    #[test]
    fn mark_row_count_simple() {
        let m = mark(1, DirtyEventSource::Pty, 5, 10);
        assert_eq!(m.row_count(), 5);
    }

    #[test]
    fn mark_row_count_zero_for_empty_range() {
        let m = mark(1, DirtyEventSource::Pty, 5, 5);
        assert_eq!(m.row_count(), 0);
    }

    #[test]
    fn mark_classify_single() {
        let m = mark(1, DirtyEventSource::Pty, 5, 6);
        assert_eq!(m.classify(80), DirtyMarkClassification::Single);
    }

    #[test]
    fn mark_classify_range() {
        let m = mark(1, DirtyEventSource::SelectionChange, 5, 15);
        assert_eq!(m.classify(80), DirtyMarkClassification::Range);
    }

    #[test]
    fn mark_classify_whole_screen_by_source() {
        let m = mark(1, DirtyEventSource::ThemeSwap, 5, 6);
        assert_eq!(m.classify(80), DirtyMarkClassification::WholeScreen);
    }

    #[test]
    fn mark_classify_whole_screen_by_size() {
        let m = mark(1, DirtyEventSource::Pty, 0, 80);
        assert_eq!(m.classify(80), DirtyMarkClassification::WholeScreen);
    }

    // ----------------------------------------------------------------
    // RowTranslation
    // ----------------------------------------------------------------

    #[test]
    fn translation_within_viewport() {
        let t = RowTranslation {
            viewport_top_stable_row: 100,
            visible_rows: 24,
        };
        assert_eq!(t.translate(100), Some(0));
        assert_eq!(t.translate(110), Some(10));
        assert_eq!(t.translate(123), Some(23));
    }

    #[test]
    fn translation_above_viewport_none() {
        let t = RowTranslation {
            viewport_top_stable_row: 100,
            visible_rows: 24,
        };
        assert_eq!(t.translate(50), None);
        assert_eq!(t.translate(99), None);
    }

    #[test]
    fn translation_below_viewport_none() {
        let t = RowTranslation {
            viewport_top_stable_row: 100,
            visible_rows: 24,
        };
        assert_eq!(t.translate(124), None);
        assert_eq!(t.translate(200), None);
    }

    #[test]
    fn translation_negative_stable_row_handled() {
        let t = RowTranslation {
            viewport_top_stable_row: 0,
            visible_rows: 24,
        };
        assert_eq!(t.translate(-1), None);
    }

    // ----------------------------------------------------------------
    // DirtyLinesPerFrameHistogram
    // ----------------------------------------------------------------

    #[test]
    fn dirty_hist_bucket_for() {
        assert_eq!(DirtyLinesPerFrameHistogram::bucket_for(0), 0);
        assert_eq!(DirtyLinesPerFrameHistogram::bucket_for(1), 0);
        assert_eq!(DirtyLinesPerFrameHistogram::bucket_for(2), 1);
        assert_eq!(DirtyLinesPerFrameHistogram::bucket_for(7), 1);
        assert_eq!(DirtyLinesPerFrameHistogram::bucket_for(8), 2);
        assert_eq!(DirtyLinesPerFrameHistogram::bucket_for(31), 2);
        assert_eq!(DirtyLinesPerFrameHistogram::bucket_for(2048), 6);
    }

    #[test]
    fn dirty_hist_records_and_percentile() {
        let mut h = DirtyLinesPerFrameHistogram::new();
        for _ in 0..100 {
            h.record(1); // bucket 0 ([0..2))
        }
        assert_eq!(h.total, 100);
        assert_eq!(h.percentile_lines(99), Some(2));
    }

    #[test]
    fn dirty_hist_empty_returns_none() {
        let h = DirtyLinesPerFrameHistogram::new();
        assert_eq!(h.percentile_lines(99), None);
    }

    // ----------------------------------------------------------------
    // FramePaintLatencyHistogram + RQ-S1/RQ-S8 SLO
    // ----------------------------------------------------------------

    #[test]
    fn paint_hist_p99_under_target_passes() {
        let mut h = FramePaintLatencyHistogram::new();
        for _ in 0..200 {
            h.record(20); // bucket 1 ([10..50))
        }
        let config = DirtyTelemetryConfig::default();
        assert!(h.meets_p99_target(config));
    }

    #[test]
    fn paint_hist_p99_at_target_passes() {
        let mut h = FramePaintLatencyHistogram::new();
        for _ in 0..98 {
            h.record(20);
        }
        for _ in 0..2 {
            h.record(80); // bucket 2 ([50..100))
        }
        let config = DirtyTelemetryConfig::default();
        // p99 falls in bucket 1 since cumulative 98 < 99 and
        // bucket 2 closes 100>=99: p99 = 100 = target.
        assert!(h.meets_p99_target(config));
    }

    #[test]
    fn paint_hist_p99_over_target_fails() {
        let mut h = FramePaintLatencyHistogram::new();
        for _ in 0..98 {
            h.record(20);
        }
        for _ in 0..2 {
            h.record(800); // bucket 4 ([500..1000))
        }
        let config = DirtyTelemetryConfig::default();
        assert!(!h.meets_p99_target(config));
    }

    #[test]
    fn paint_hist_empty_passes_trivially() {
        let h = FramePaintLatencyHistogram::new();
        let config = DirtyTelemetryConfig::default();
        assert!(h.meets_p99_target(config));
    }

    // ----------------------------------------------------------------
    // should_clear_at_frame_end
    // ----------------------------------------------------------------

    #[test]
    fn clear_at_frame_end_normal_yes() {
        let config = DirtyTelemetryConfig::default();
        assert!(should_clear_at_frame_end(false, config));
    }

    #[test]
    fn clear_at_frame_end_after_whole_screen_no() {
        let config = DirtyTelemetryConfig::default();
        assert!(!should_clear_at_frame_end(true, config));
    }

    #[test]
    fn clear_at_frame_end_disabled_in_config() {
        let config = DirtyTelemetryConfig {
            clear_allowed_at_frame_end: false,
            ..DirtyTelemetryConfig::default()
        };
        assert!(!should_clear_at_frame_end(false, config));
    }

    // ----------------------------------------------------------------
    // MarksBySource
    // ----------------------------------------------------------------

    #[test]
    fn marks_by_source_routes_each_variant() {
        let mut m = MarksBySource::default();
        m.record(DirtyEventSource::Pty);
        m.record(DirtyEventSource::CursorMove);
        m.record(DirtyEventSource::SelectionChange);
        m.record(DirtyEventSource::ThemeSwap);
        m.record(DirtyEventSource::FontSwap);
        m.record(DirtyEventSource::StatusTileUpdate);
        m.record(DirtyEventSource::FocusChange);
        m.record(DirtyEventSource::Resize);
        assert_eq!(m.pty, 1);
        assert_eq!(m.cursor_move, 1);
        assert_eq!(m.selection_change, 1);
        assert_eq!(m.theme_swap, 1);
        assert_eq!(m.font_swap, 1);
        assert_eq!(m.status_tile_update, 1);
        assert_eq!(m.focus_change, 1);
        assert_eq!(m.resize, 1);
    }

    // ----------------------------------------------------------------
    // DirtyLineTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telem_default_zero() {
        let t = DirtyLineTelemetry::default();
        assert_eq!(t.dirty_marks_total, 0);
        assert_eq!(t.frames_cleared_total, 0);
    }

    #[test]
    fn telem_record_mark_increments_total_and_per_source() {
        let mut t = DirtyLineTelemetry::default();
        t.record_mark(&mark(1, DirtyEventSource::Pty, 5, 6));
        t.record_mark(&mark(1, DirtyEventSource::CursorMove, 3, 4));
        assert_eq!(t.dirty_marks_total, 2);
        assert_eq!(t.marks_by_source.pty, 1);
        assert_eq!(t.marks_by_source.cursor_move, 1);
    }

    #[test]
    fn telem_record_frame_end_increments_clear_when_cleared() {
        let mut t = DirtyLineTelemetry::default();
        let config = DirtyTelemetryConfig::default();
        t.record_frame_end(5, 75, 30, true, config);
        assert_eq!(t.frames_cleared_total, 1);
        assert_eq!(t.clean_lines_skipped, 75);
        assert_eq!(t.frames_over_budget, 0);
    }

    #[test]
    fn telem_record_frame_end_increments_over_budget_when_slow() {
        let mut t = DirtyLineTelemetry::default();
        let config = DirtyTelemetryConfig::default();
        t.record_frame_end(5, 75, 200, true, config);
        assert_eq!(t.frames_over_budget, 1);
    }

    #[test]
    fn telem_record_frame_end_at_exact_budget_counts_as_over() {
        // Self-review fix (br-ft-pmiis): bead's RQ-S1 is strict
        // less-than (paint < 100 µs). A paint at exactly 100 µs
        // violates the bound and substrate must count it as
        // over-budget (>= rather than >).
        let mut t = DirtyLineTelemetry::default();
        let config = DirtyTelemetryConfig::default();
        t.record_frame_end(5, 75, 100, true, config);
        assert_eq!(t.frames_over_budget, 1);
    }

    #[test]
    fn telem_record_frame_end_at_99us_under_budget() {
        // Boundary: 99 µs is strictly under 100 µs budget.
        let mut t = DirtyLineTelemetry::default();
        let config = DirtyTelemetryConfig::default();
        t.record_frame_end(5, 75, 99, true, config);
        assert_eq!(t.frames_over_budget, 0);
    }

    #[test]
    fn telem_does_not_increment_clear_if_not_cleared() {
        let mut t = DirtyLineTelemetry::default();
        let config = DirtyTelemetryConfig::default();
        t.record_frame_end(0, 80, 10, false, config);
        assert_eq!(t.frames_cleared_total, 0);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_typing_1_cell_per_frame_meets_rqs1() {
        // Bead's RQ-S1: 200-pane, 1 cell change per frame, p99
        // <0.1 ms. Substrate sees ~500 frames at 50 µs each.
        let mut t = DirtyLineTelemetry::default();
        let config = DirtyTelemetryConfig::default();
        for _ in 0..500 {
            t.record_mark(&mark(1, DirtyEventSource::Pty, 5, 6));
            t.record_frame_end(1, 23, 50, true, config);
        }
        assert!(t.paint_p99_meets_target(config));
        assert_eq!(t.frames_over_budget, 0);
    }

    #[test]
    fn scenario_font_swap_coarse_invalidate_no_clear() {
        // Theme/font swap leaves bitmap force-marked across
        // the boundary so the next frame paints all panes.
        let mut t = DirtyLineTelemetry::default();
        let config = DirtyTelemetryConfig::default();
        let was_whole_screen = true;
        let cleared = should_clear_at_frame_end(was_whole_screen, config);
        assert!(!cleared);
        t.record_frame_end(80, 0, 250, cleared, config);
        assert_eq!(t.frames_cleared_total, 0);
    }

    #[test]
    fn scenario_full_pty_translation_and_mark() {
        // PTY writes at stable rows; translate to visible rows
        // and mark via the bitmap's range mark API (simulated).
        let translation = RowTranslation {
            viewport_top_stable_row: 1_000,
            visible_rows: 24,
        };
        let stable_writes = [1_000i64, 1_005, 1_023, 999, 1_024];
        let mut translated_count = 0u32;
        for s in stable_writes {
            if translation.translate(s).is_some() {
                translated_count += 1;
            }
        }
        // 1_000, 1_005, 1_023 are in the visible window;
        // 999 is above; 1_024 is at the boundary (excluded).
        assert_eq!(translated_count, 3);
    }

    #[test]
    fn scenario_failure_mode_no_dirty_tracking() {
        // Pre-cont baseline: every frame paints all 80 lines
        // because dirty tracking isn't wired. Paint p99 ~500 µs.
        let mut t = DirtyLineTelemetry::default();
        let config = DirtyTelemetryConfig::default();
        for _ in 0..100 {
            t.record_frame_end(80, 0, 500, true, config);
        }
        assert!(!t.paint_p99_meets_target(config));
        assert_eq!(t.frames_over_budget, 100);
    }
}
