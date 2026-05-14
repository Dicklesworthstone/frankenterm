//! Differential render oracle for ftui vs ratatui parity
//! ([BR-RC-CUTOVERS.G5.1] / `ft-35yac.1`).
//!
//! The TUI cluster ships **two backends** in parallel during
//! the FTUI migration: the legacy `ratatui` stack
//! (`crates/frankenterm-core/src/tui/views.rs`) and the
//! migration-target `ftui` stack
//! (`crates/frankenterm-core/src/tui/ftui_backend.rs`). Both
//! are compiled in by the rollout feature; runtime selection
//! is via `FT_TUI_BACKEND`. Until ratatui is deleted at
//! Stage 3, every render path needs a parity oracle.
//!
//! This module ships the **contract layer** that the parity
//! harness consumes:
//!
//! - [`RenderCell`] — backend-agnostic single-cell shape:
//!   character + foreground RGBA + background RGBA + the bold
//!   / italic / underline / reverse modifier flags. Both
//!   backends' cell representations project into this type.
//! - [`RenderFrame`] — `width × height` cell grid.
//!   Serializable; used as the on-disk artifact for the
//!   "byte-identical render frames" assertion.
//! - [`FrameDiff`] — per-cell mismatch report with an
//!   insta-style printable summary.
//! - [`KeymapAction`] — closed enum of every input action in
//!   the canonical `keymap.rs` table. Mirrors
//!   `crate::tui::keymap::Action` without depending on the
//!   feature-gated `tui` module (so this oracle compiles
//!   without `--features tui`). The deletion criterion: when
//!   ratatui is deleted, this enum can collapse to whatever
//!   ftui exposes.
//! - [`EventScript`] — sequence of `KeymapAction`s that
//!   becomes one parity-test row (the harness drives both
//!   backends with the same script and compares frames at the
//!   end of each step).
//! - [`OracleHealth`] — `ft doctor` counter snapshot matching
//!   this session's `*Health` shape.
//! - [`synthesized_event_corpus`] — small in-tree corpus.
//!   Recording from real vhs/asciinema sessions (sub-bead
//!   `ft-35yac.1.1`) extends this corpus.
//!
//! ## What this module is NOT
//!
//! - Not the full clean backend-driver harness. This module can
//!   normalize ratatui and ftui frame buffers into `RenderFrame`;
//!   the rollout-gated retained driver now reaches
//!   `views.rs`/`app.rs` and `ftui_backend.rs` with matched
//!   deterministic state, but current evidence still reports a
//!   backend divergence rather than a clean parity pass. Extending
//!   that driver to every `EventScript` and making the diff clean
//!   is the integration follow-on.
//! - Not the vhs/asciinema corpus recording. Sub-bead
//!   `ft-35yac.1.1` records real-session corpora; this module
//!   ships an in-tree synthesized corpus the harness uses
//!   until those land.
//! - Not the GPU-renderer parity test. Sub-bead `ft-35yac.1.2`
//!   covers the headless-GPU comparator; this module is the
//!   buffer-level (CPU-side) oracle.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Cell + Frame
// ============================================================================

/// Backend-agnostic render-cell shape. Both ratatui's `Cell`
/// and ftui's per-cell render output project into this type.
/// Cells use RGBA so backends with named-color palettes
/// resolve into a comparable 32-bit form (alpha is reserved;
/// production renderers emit `0xFF` for opaque).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RenderCell {
    /// The Unicode scalar at this cell. Wide chars occupy a
    /// `Char` slot at the leading column and a `Continuation`
    /// at the trailing column.
    pub ch: char,
    pub fg: Rgba,
    pub bg: Rgba,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    /// True iff this cell is the trailing column of a wide
    /// glyph. Production backends MUST emit a sentinel
    /// (typically `' '`) here so frames stay rectangular.
    pub continuation: bool,
}

impl RenderCell {
    /// Default: space, default fg/bg, no modifiers.
    #[must_use]
    pub const fn space() -> Self {
        Self {
            ch: ' ',
            fg: Rgba::DEFAULT_FG,
            bg: Rgba::DEFAULT_BG,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
            continuation: false,
        }
    }

    /// Whether this cell is visually equivalent to `other`.
    /// Used by the diff to skip diagnostics on cells that
    /// differ only in continuation-flag noise (a backend
    /// quirk).
    #[must_use]
    pub fn structurally_equal(self, other: Self) -> bool {
        self.ch == other.ch
            && self.fg == other.fg
            && self.bg == other.bg
            && self.bold == other.bold
            && self.italic == other.italic
            && self.underline == other.underline
            && self.reverse == other.reverse
    }
}

/// 32-bit color, RGBA. Backends emit alpha = 255 for opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const DEFAULT_FG: Self = Self {
        r: 0xCC,
        g: 0xCC,
        b: 0xCC,
        a: 0xFF,
    };
    pub const DEFAULT_BG: Self = Self {
        r: 0x00,
        g: 0x00,
        b: 0x00,
        a: 0xFF,
    };

    #[cfg(any(feature = "tui", test))]
    const fn opaque(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xFF }
    }
}

/// `width × height` cell grid. Cells are row-major
/// (`cells[row * width + col]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderFrame {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<RenderCell>,
}

impl RenderFrame {
    /// New blank frame.
    #[must_use]
    pub fn blank(width: u16, height: u16) -> Self {
        let total = (width as usize) * (height as usize);
        Self {
            width,
            height,
            cells: vec![RenderCell::space(); total],
        }
    }

    /// Cell at (row, col). Returns `None` if out of bounds.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<RenderCell> {
        if row >= self.height || col >= self.width {
            return None;
        }
        let idx = row as usize * self.width as usize + col as usize;
        self.cells.get(idx).copied()
    }

    /// Set the cell at (row, col).
    pub fn set_cell(&mut self, row: u16, col: u16, cell: RenderCell) -> bool {
        if row >= self.height || col >= self.width {
            return false;
        }
        let idx = row as usize * self.width as usize + col as usize;
        if let Some(slot) = self.cells.get_mut(idx) {
            *slot = cell;
            true
        } else {
            false
        }
    }

    /// Total cell count.
    #[must_use]
    pub fn cell_count(&self) -> usize {
        (self.width as usize) * (self.height as usize)
    }

    /// Whether the cells vector matches the declared
    /// dimensions. Property tests rely on this.
    #[must_use]
    pub fn is_well_shaped(&self) -> bool {
        self.cells.len() == self.cell_count()
    }
}

// ============================================================================
// Backend frame normalizers
// ============================================================================

#[inline]
#[must_use]
#[cfg(any(feature = "tui", feature = "ftui"))]
fn first_scalar(symbol: &str) -> char {
    symbol.chars().next().unwrap_or(' ')
}

#[inline]
#[must_use]
#[cfg(feature = "tui")]
fn xterm_indexed_color(index: u8) -> Rgba {
    const ANSI_16: [Rgba; 16] = [
        Rgba::opaque(0x00, 0x00, 0x00),
        Rgba::opaque(0x80, 0x00, 0x00),
        Rgba::opaque(0x00, 0x80, 0x00),
        Rgba::opaque(0x80, 0x80, 0x00),
        Rgba::opaque(0x00, 0x00, 0x80),
        Rgba::opaque(0x80, 0x00, 0x80),
        Rgba::opaque(0x00, 0x80, 0x80),
        Rgba::opaque(0xC0, 0xC0, 0xC0),
        Rgba::opaque(0x80, 0x80, 0x80),
        Rgba::opaque(0xFF, 0x00, 0x00),
        Rgba::opaque(0x00, 0xFF, 0x00),
        Rgba::opaque(0xFF, 0xFF, 0x00),
        Rgba::opaque(0x00, 0x00, 0xFF),
        Rgba::opaque(0xFF, 0x00, 0xFF),
        Rgba::opaque(0x00, 0xFF, 0xFF),
        Rgba::opaque(0xFF, 0xFF, 0xFF),
    ];

    if index < 16 {
        return ANSI_16[index as usize];
    }

    if index < 232 {
        let n = index - 16;
        let steps = [0, 95, 135, 175, 215, 255];
        return Rgba::opaque(
            steps[(n / 36) as usize],
            steps[((n % 36) / 6) as usize],
            steps[(n % 6) as usize],
        );
    }

    let gray = 8u8.saturating_add((index - 232).saturating_mul(10));
    Rgba::opaque(gray, gray, gray)
}

#[cfg(feature = "tui")]
#[inline]
#[must_use]
fn ratatui_color_to_rgba(color: ratatui::style::Color, default: Rgba) -> Rgba {
    use ratatui::style::Color;

    match color {
        Color::Reset => default,
        Color::Black => xterm_indexed_color(0),
        Color::Red => xterm_indexed_color(1),
        Color::Green => xterm_indexed_color(2),
        Color::Yellow => xterm_indexed_color(3),
        Color::Blue => xterm_indexed_color(4),
        Color::Magenta => xterm_indexed_color(5),
        Color::Cyan => xterm_indexed_color(6),
        Color::Gray => xterm_indexed_color(7),
        Color::DarkGray => xterm_indexed_color(8),
        Color::LightRed => xterm_indexed_color(9),
        Color::LightGreen => xterm_indexed_color(10),
        Color::LightYellow => xterm_indexed_color(11),
        Color::LightBlue => xterm_indexed_color(12),
        Color::LightMagenta => xterm_indexed_color(13),
        Color::LightCyan => xterm_indexed_color(14),
        Color::White => xterm_indexed_color(15),
        Color::Rgb(r, g, b) => Rgba::opaque(r, g, b),
        Color::Indexed(index) => xterm_indexed_color(index),
    }
}

/// Normalize a ratatui [`ratatui::buffer::Buffer`] into the
/// backend-agnostic frame shape consumed by the parity oracle.
#[cfg(feature = "tui")]
#[must_use]
pub fn render_frame_from_ratatui_buffer(buffer: &ratatui::buffer::Buffer) -> RenderFrame {
    use ratatui::style::Modifier;

    let width = buffer.area.width;
    let height = buffer.area.height;
    let mut frame = RenderFrame::blank(width, height);

    for row in 0..height {
        for col in 0..width {
            let Some(cell) = buffer.cell((buffer.area.x + col, buffer.area.y + row)) else {
                continue;
            };
            let modifier = cell.modifier;
            frame.set_cell(
                row,
                col,
                RenderCell {
                    ch: first_scalar(cell.symbol()),
                    fg: ratatui_color_to_rgba(cell.fg, Rgba::DEFAULT_FG),
                    bg: ratatui_color_to_rgba(cell.bg, Rgba::DEFAULT_BG),
                    bold: modifier.contains(Modifier::BOLD),
                    italic: modifier.contains(Modifier::ITALIC),
                    underline: modifier.contains(Modifier::UNDERLINED),
                    reverse: modifier.contains(Modifier::REVERSED),
                    continuation: false,
                },
            );
        }
    }

    frame
}

#[cfg(feature = "ftui")]
#[inline]
#[must_use]
fn ftui_color_to_rgba(color: ftui::PackedRgba, default: Rgba) -> Rgba {
    if color.a() == 0 {
        default
    } else {
        Rgba {
            r: color.r(),
            g: color.g(),
            b: color.b(),
            a: color.a(),
        }
    }
}

#[cfg(feature = "ftui")]
#[inline]
#[must_use]
fn ftui_fg_to_rgba(cell: &ftui::Cell) -> Rgba {
    if cell.fg == ftui::PackedRgba::WHITE && cell.bg.a() == 0 && cell.attrs == ftui::CellAttrs::NONE
    {
        // ftui::Cell::default() and ftui::Cell::from_char()
        // store WHITE as the foreground for otherwise-default
        // cells; ratatui represents the same terminal-default
        // foreground as Color::Reset. The ftui cell shape cannot
        // distinguish an unstyled explicit WHITE fg from the
        // default sentinel, so keep this branch limited to the
        // exact unstyled transparent-bg default-cell shape and
        // preserve styled explicit WHITE cells below.
        Rgba::DEFAULT_FG
    } else {
        ftui_color_to_rgba(cell.fg, Rgba::DEFAULT_FG)
    }
}

#[cfg(feature = "ftui")]
#[must_use]
fn ftui_cell_char(cell: &ftui::Cell, pool: &ftui::GraphemePool) -> (char, bool) {
    if cell.content.is_continuation() {
        return (' ', true);
    }
    if cell.content.is_empty() {
        return (' ', false);
    }
    if let Some(ch) = cell.content.as_char() {
        return (ch, false);
    }
    if let Some(id) = cell.content.grapheme_id() {
        return (pool.get(id).map(first_scalar).unwrap_or(' '), false);
    }
    (' ', false)
}

/// Normalize an ftui [`ftui::Buffer`] plus its grapheme pool into
/// the backend-agnostic frame shape consumed by the parity oracle.
#[cfg(feature = "ftui")]
#[must_use]
pub fn render_frame_from_ftui_buffer(
    buffer: &ftui::Buffer,
    pool: &ftui::GraphemePool,
) -> RenderFrame {
    use ftui::render::cell::StyleFlags;

    let width = buffer.width();
    let height = buffer.height();
    let mut frame = RenderFrame::blank(width, height);

    for row in 0..height {
        for col in 0..width {
            let Some(cell) = buffer.get(col, row) else {
                continue;
            };
            let (ch, continuation) = ftui_cell_char(cell, pool);
            let flags = cell.attrs.flags();
            frame.set_cell(
                row,
                col,
                RenderCell {
                    ch,
                    fg: ftui_fg_to_rgba(cell),
                    bg: ftui_color_to_rgba(cell.bg, Rgba::DEFAULT_BG),
                    bold: flags.contains(StyleFlags::BOLD),
                    italic: flags.contains(StyleFlags::ITALIC),
                    underline: flags.contains(StyleFlags::UNDERLINE),
                    reverse: flags.contains(StyleFlags::REVERSE),
                    continuation,
                },
            );
        }
    }

    frame
}

/// Normalize an ftui [`ftui::Frame`] into the backend-agnostic frame shape.
#[cfg(feature = "ftui")]
#[must_use]
pub fn render_frame_from_ftui_frame(frame: &ftui::Frame<'_>) -> RenderFrame {
    let pool: &ftui::GraphemePool = &*frame.pool;
    render_frame_from_ftui_buffer(&frame.buffer, pool)
}

// ============================================================================
// Frame diff
// ============================================================================

/// One per-cell divergence between two frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellDiff {
    pub row: u16,
    pub col: u16,
    pub left: RenderCell,
    pub right: RenderCell,
}

/// Result of comparing two frames. The comparator is
/// reflexive (`diff(f, f).is_clean()`), symmetric
/// (`diff(a, b).cells.len() == diff(b, a).cells.len()`), and
/// dimension-strict (different widths/heights → returns the
/// `dimension_mismatch` flag with no per-cell records).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameDiff {
    /// True iff `left.width != right.width` or
    /// `left.height != right.height`. When set, `cells` is
    /// empty (the comparator does not attempt cell-wise
    /// alignment across mismatched dimensions).
    pub dimension_mismatch: bool,
    pub left_dim: (u16, u16),
    pub right_dim: (u16, u16),
    pub cells: Vec<CellDiff>,
}

impl FrameDiff {
    /// True iff the frames are byte-identical (no dimension
    /// mismatch, zero per-cell divergences).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        !self.dimension_mismatch && self.cells.is_empty()
    }

    /// Number of divergent cells.
    #[must_use]
    pub fn divergent_cell_count(&self) -> usize {
        self.cells.len()
    }

    /// Insta-style printable summary for triage. The format
    /// is one line per divergence: `(row,col): 'L' fg/bg →
    /// 'R' fg/bg`.
    #[must_use]
    pub fn render_summary(&self, max_lines: usize) -> String {
        if self.dimension_mismatch {
            return format!(
                "frame dimension mismatch: left={:?} vs right={:?}",
                self.left_dim, self.right_dim
            );
        }
        if self.cells.is_empty() {
            return "frames identical".to_string();
        }
        let mut s = String::new();
        for (i, cd) in self.cells.iter().enumerate() {
            if i >= max_lines {
                s.push_str(&format!(
                    "... and {} more divergent cells\n",
                    self.cells.len() - max_lines
                ));
                break;
            }
            s.push_str(&format!(
                "({},{}): {:?} {:?}/{:?} → {:?} {:?}/{:?}\n",
                cd.row,
                cd.col,
                cd.left.ch,
                cd.left.fg,
                cd.left.bg,
                cd.right.ch,
                cd.right.fg,
                cd.right.bg,
            ));
        }
        s
    }
}

/// Compute the per-cell diff between two frames. The cells
/// vector is sorted by `(row, col)` ascending.
#[must_use]
pub fn compute_diff(left: &RenderFrame, right: &RenderFrame) -> FrameDiff {
    if left.width != right.width || left.height != right.height {
        return FrameDiff {
            dimension_mismatch: true,
            left_dim: (left.width, left.height),
            right_dim: (right.width, right.height),
            cells: Vec::new(),
        };
    }

    let mut cells = Vec::new();
    for row in 0..left.height {
        for col in 0..left.width {
            let l = left.cell(row, col).expect("well-shaped frame");
            let r = right.cell(row, col).expect("well-shaped frame");
            if !l.structurally_equal(r) {
                cells.push(CellDiff {
                    row,
                    col,
                    left: l,
                    right: r,
                });
            }
        }
    }

    FrameDiff {
        dimension_mismatch: false,
        left_dim: (left.width, left.height),
        right_dim: (right.width, right.height),
        cells,
    }
}

// ============================================================================
// Keymap action mirror
// ============================================================================

/// Backend-agnostic mirror of `crate::tui::keymap::Action`.
/// This module is NOT feature-gated on `tui`, so it can't
/// import the production `Action` type (which lives behind the
/// feature flag). The mirror MUST stay synchronized with the
/// canonical table; the harness `tests/tui_parity_oracle.rs`
/// asserts coverage parity (via the `KeymapActionKind::ALL`
/// table count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeymapAction {
    Quit,
    ShowHelp,
    Refresh,
    NextTab,
    PrevTab,
    GoToView { view_index: u8 },
    ListNext,
    ListPrev,
    FilterAppendChar { ch: char },
    FilterDeleteChar,
    FilterClear,
    ToggleUnhandledOnly,
    ToggleBookmarkedOnly,
    CycleAgentFilter,
    CycleDomainFilter,
    CycleRulesetProfile,
    ApplyRulesetProfile,
    EventsFilterDigit { digit: char },
    TriagePrimaryAction,
    TriageMute,
    TriageToggleExpand,
    TriageNumberedAction { index: u8 },
    ToggleUndoableOnly,
    SearchNextSaved,
    SearchPrevSaved,
    SearchRunSaved,
    SearchToggleSaved,
    SearchExecute,
    TimelineZoomIn,
    TimelineZoomOut,
    TimelineScrollLeft,
    TimelineScrollRight,
}

/// Variant kinds of `KeymapAction` (without payload). Used by
/// the harness's coverage check: `keymap.rs` action count must
/// equal `KeymapActionKind::ALL.len()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeymapActionKind {
    Quit,
    ShowHelp,
    Refresh,
    NextTab,
    PrevTab,
    GoToView,
    ListNext,
    ListPrev,
    FilterAppendChar,
    FilterDeleteChar,
    FilterClear,
    ToggleUnhandledOnly,
    ToggleBookmarkedOnly,
    CycleAgentFilter,
    CycleDomainFilter,
    CycleRulesetProfile,
    ApplyRulesetProfile,
    EventsFilterDigit,
    TriagePrimaryAction,
    TriageMute,
    TriageToggleExpand,
    TriageNumberedAction,
    ToggleUndoableOnly,
    SearchNextSaved,
    SearchPrevSaved,
    SearchRunSaved,
    SearchToggleSaved,
    SearchExecute,
    TimelineZoomIn,
    TimelineZoomOut,
    TimelineScrollLeft,
    TimelineScrollRight,
}

impl KeymapActionKind {
    /// All variants in declaration order. The count here MUST
    /// equal the canonical `keymap::Action` variant count.
    /// The integration test asserts this.
    pub const ALL: &'static [KeymapActionKind] = &[
        Self::Quit,
        Self::ShowHelp,
        Self::Refresh,
        Self::NextTab,
        Self::PrevTab,
        Self::GoToView,
        Self::ListNext,
        Self::ListPrev,
        Self::FilterAppendChar,
        Self::FilterDeleteChar,
        Self::FilterClear,
        Self::ToggleUnhandledOnly,
        Self::ToggleBookmarkedOnly,
        Self::CycleAgentFilter,
        Self::CycleDomainFilter,
        Self::CycleRulesetProfile,
        Self::ApplyRulesetProfile,
        Self::EventsFilterDigit,
        Self::TriagePrimaryAction,
        Self::TriageMute,
        Self::TriageToggleExpand,
        Self::TriageNumberedAction,
        Self::ToggleUndoableOnly,
        Self::SearchNextSaved,
        Self::SearchPrevSaved,
        Self::SearchRunSaved,
        Self::SearchToggleSaved,
        Self::SearchExecute,
        Self::TimelineZoomIn,
        Self::TimelineZoomOut,
        Self::TimelineScrollLeft,
        Self::TimelineScrollRight,
    ];
}

impl KeymapAction {
    /// Variant kind without payload.
    #[must_use]
    pub const fn kind(&self) -> KeymapActionKind {
        match self {
            Self::Quit => KeymapActionKind::Quit,
            Self::ShowHelp => KeymapActionKind::ShowHelp,
            Self::Refresh => KeymapActionKind::Refresh,
            Self::NextTab => KeymapActionKind::NextTab,
            Self::PrevTab => KeymapActionKind::PrevTab,
            Self::GoToView { .. } => KeymapActionKind::GoToView,
            Self::ListNext => KeymapActionKind::ListNext,
            Self::ListPrev => KeymapActionKind::ListPrev,
            Self::FilterAppendChar { .. } => KeymapActionKind::FilterAppendChar,
            Self::FilterDeleteChar => KeymapActionKind::FilterDeleteChar,
            Self::FilterClear => KeymapActionKind::FilterClear,
            Self::ToggleUnhandledOnly => KeymapActionKind::ToggleUnhandledOnly,
            Self::ToggleBookmarkedOnly => KeymapActionKind::ToggleBookmarkedOnly,
            Self::CycleAgentFilter => KeymapActionKind::CycleAgentFilter,
            Self::CycleDomainFilter => KeymapActionKind::CycleDomainFilter,
            Self::CycleRulesetProfile => KeymapActionKind::CycleRulesetProfile,
            Self::ApplyRulesetProfile => KeymapActionKind::ApplyRulesetProfile,
            Self::EventsFilterDigit { .. } => KeymapActionKind::EventsFilterDigit,
            Self::TriagePrimaryAction => KeymapActionKind::TriagePrimaryAction,
            Self::TriageMute => KeymapActionKind::TriageMute,
            Self::TriageToggleExpand => KeymapActionKind::TriageToggleExpand,
            Self::TriageNumberedAction { .. } => KeymapActionKind::TriageNumberedAction,
            Self::ToggleUndoableOnly => KeymapActionKind::ToggleUndoableOnly,
            Self::SearchNextSaved => KeymapActionKind::SearchNextSaved,
            Self::SearchPrevSaved => KeymapActionKind::SearchPrevSaved,
            Self::SearchRunSaved => KeymapActionKind::SearchRunSaved,
            Self::SearchToggleSaved => KeymapActionKind::SearchToggleSaved,
            Self::SearchExecute => KeymapActionKind::SearchExecute,
            Self::TimelineZoomIn => KeymapActionKind::TimelineZoomIn,
            Self::TimelineZoomOut => KeymapActionKind::TimelineZoomOut,
            Self::TimelineScrollLeft => KeymapActionKind::TimelineScrollLeft,
            Self::TimelineScrollRight => KeymapActionKind::TimelineScrollRight,
        }
    }
}

// ============================================================================
// Event script + corpus
// ============================================================================

/// Sequence of `KeymapAction`s that drive both backends.
/// The harness applies the script step-by-step and compares
/// the resulting frames at every step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventScript {
    /// Stable name (e.g., `"home_to_search_to_results"`).
    pub name: String,
    /// Why this script exists — what backend-divergence shape
    /// it targets.
    pub rationale: String,
    /// Initial view (1..=7).
    pub initial_view: u8,
    /// Frame dimensions to render at.
    pub width: u16,
    pub height: u16,
    /// Action sequence.
    pub actions: Vec<KeymapAction>,
}

/// Synthesized in-tree event-script corpus. Sub-bead
/// `ft-35yac.1.1` extends this with vhs/asciinema-derived
/// real-session corpora.
#[must_use]
pub fn synthesized_event_corpus() -> Vec<EventScript> {
    vec![
        EventScript {
            name: "smoke_quit_from_home".to_string(),
            rationale: "minimal — quit from default view".to_string(),
            initial_view: 1,
            width: 80,
            height: 24,
            actions: vec![KeymapAction::Quit],
        },
        EventScript {
            name: "tab_cycle_all_views".to_string(),
            rationale: "exercises NextTab through every view".to_string(),
            initial_view: 1,
            width: 80,
            height: 24,
            actions: vec![
                KeymapAction::NextTab,
                KeymapAction::NextTab,
                KeymapAction::NextTab,
                KeymapAction::NextTab,
                KeymapAction::NextTab,
                KeymapAction::NextTab,
                KeymapAction::NextTab,
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "goto_view_jumps".to_string(),
            rationale: "GoToView(1..7) — every direct jump".to_string(),
            initial_view: 1,
            width: 80,
            height: 24,
            actions: vec![
                KeymapAction::GoToView { view_index: 1 },
                KeymapAction::GoToView { view_index: 2 },
                KeymapAction::GoToView { view_index: 3 },
                KeymapAction::GoToView { view_index: 4 },
                KeymapAction::GoToView { view_index: 5 },
                KeymapAction::GoToView { view_index: 6 },
                KeymapAction::GoToView { view_index: 7 },
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "panes_filter_toggle".to_string(),
            rationale: "Panes view filter toggles + cycles".to_string(),
            initial_view: 2,
            width: 100,
            height: 30,
            actions: vec![
                KeymapAction::GoToView { view_index: 2 },
                KeymapAction::ToggleUnhandledOnly,
                KeymapAction::ToggleBookmarkedOnly,
                KeymapAction::CycleAgentFilter,
                KeymapAction::CycleDomainFilter,
                KeymapAction::CycleRulesetProfile,
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "search_filter_text_entry".to_string(),
            rationale: "FilterAppendChar / FilterDeleteChar / FilterClear sequence".to_string(),
            initial_view: 5,
            width: 80,
            height: 24,
            actions: vec![
                KeymapAction::GoToView { view_index: 5 },
                KeymapAction::FilterAppendChar { ch: 'h' },
                KeymapAction::FilterAppendChar { ch: 'e' },
                KeymapAction::FilterAppendChar { ch: 'l' },
                KeymapAction::FilterAppendChar { ch: 'l' },
                KeymapAction::FilterAppendChar { ch: 'o' },
                KeymapAction::FilterDeleteChar,
                KeymapAction::FilterAppendChar { ch: 'O' },
                KeymapAction::FilterClear,
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "events_digit_filter".to_string(),
            rationale: "EventsFilterDigit cycle".to_string(),
            initial_view: 3,
            width: 80,
            height: 24,
            actions: vec![
                KeymapAction::GoToView { view_index: 3 },
                KeymapAction::EventsFilterDigit { digit: '1' },
                KeymapAction::EventsFilterDigit { digit: '5' },
                KeymapAction::EventsFilterDigit { digit: '0' },
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "triage_numbered_actions".to_string(),
            rationale: "TriageNumberedAction(1..9) + primary".to_string(),
            initial_view: 4,
            width: 80,
            height: 24,
            actions: vec![
                KeymapAction::GoToView { view_index: 4 },
                KeymapAction::TriagePrimaryAction,
                KeymapAction::TriageNumberedAction { index: 1 },
                KeymapAction::TriageNumberedAction { index: 5 },
                KeymapAction::TriageMute,
                KeymapAction::TriageToggleExpand,
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "history_undoable_filter".to_string(),
            rationale: "ToggleUndoableOnly + list nav".to_string(),
            initial_view: 6,
            width: 80,
            height: 24,
            actions: vec![
                KeymapAction::GoToView { view_index: 6 },
                KeymapAction::ToggleUndoableOnly,
                KeymapAction::ListNext,
                KeymapAction::ListNext,
                KeymapAction::ListPrev,
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "search_saved_cycle".to_string(),
            rationale: "SearchNext/PrevSaved + Run/Toggle/Execute".to_string(),
            initial_view: 5,
            width: 100,
            height: 30,
            actions: vec![
                KeymapAction::GoToView { view_index: 5 },
                KeymapAction::SearchNextSaved,
                KeymapAction::SearchNextSaved,
                KeymapAction::SearchPrevSaved,
                KeymapAction::SearchRunSaved,
                KeymapAction::SearchToggleSaved,
                KeymapAction::SearchExecute,
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "timeline_zoom_scroll".to_string(),
            rationale: "Zoom in/out + horizontal scroll".to_string(),
            initial_view: 7,
            width: 120,
            height: 30,
            actions: vec![
                KeymapAction::GoToView { view_index: 7 },
                KeymapAction::TimelineZoomIn,
                KeymapAction::TimelineZoomIn,
                KeymapAction::TimelineScrollRight,
                KeymapAction::TimelineScrollRight,
                KeymapAction::TimelineZoomOut,
                KeymapAction::TimelineScrollLeft,
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "show_help_overlay_then_dismiss".to_string(),
            rationale:
                "ShowHelp triggers a modal — modal overlay must render identically across backends"
                    .to_string(),
            initial_view: 1,
            width: 80,
            height: 24,
            actions: vec![
                KeymapAction::ShowHelp,
                KeymapAction::Refresh,
                KeymapAction::Quit,
            ],
        },
        EventScript {
            name: "small_terminal_dimensions".to_string(),
            rationale: "40×12 pushes both backends through narrow-frame layout paths".to_string(),
            initial_view: 1,
            width: 40,
            height: 12,
            actions: vec![
                KeymapAction::NextTab,
                KeymapAction::ListNext,
                KeymapAction::Quit,
            ],
        },
    ]
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` counter snapshot for the parity oracle.
/// Mirrors the `*Health` shape used across this session
/// (a11y_tree, color_management, atlas_stability,
/// triple_buffer, live_resize, render_quality,
/// snap_back_fuzz, wayland_frame_pacing, bidi_correctness,
/// tx_killswitch_model, passive_watch_invariant,
/// wire_dedup_model, redactor_coverage_matrix).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OracleHealth {
    pub frames_compared_total: u64,
    pub clean_frames_total: u64,
    pub diverged_frames_total: u64,
    pub dimension_mismatch_total: u64,
    pub max_diverged_cells_in_run: u32,
}

impl OracleHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            frames_compared_total: 0,
            clean_frames_total: 0,
            diverged_frames_total: 0,
            dimension_mismatch_total: 0,
            max_diverged_cells_in_run: 0,
        }
    }

    /// True iff at least one frame has been compared AND every
    /// comparison was clean.
    ///
    /// Per ft-11d5f sweep: previously checked the violation
    /// counters alone, which are zero on cold baseline. The
    /// doctor would surface oracle parity as green for a process
    /// where the parity harness had never been wired.
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        self.frames_compared_total > 0
            && self.diverged_frames_total == 0
            && self.dimension_mismatch_total == 0
    }

    /// Divergence rate per frame compared.
    #[must_use]
    pub fn divergence_rate(&self) -> f64 {
        if self.frames_compared_total == 0 {
            return 0.0;
        }
        self.diverged_frames_total as f64 / self.frames_compared_total as f64
    }
}

/// Fold one diff into a health snapshot.
pub fn fold_diff(health: &mut OracleHealth, diff: &FrameDiff) {
    health.frames_compared_total += 1;
    if diff.dimension_mismatch {
        health.dimension_mismatch_total += 1;
        health.diverged_frames_total += 1;
    } else if diff.cells.is_empty() {
        health.clean_frames_total += 1;
    } else {
        health.diverged_frames_total += 1;
        let dc = diff.cells.len() as u32;
        if dc > health.max_diverged_cells_in_run {
            health.max_diverged_cells_in_run = dc;
        }
    }
}

// ============================================================================
// JSONL render
// ============================================================================

#[must_use]
pub fn render_diffs_jsonl(diffs: &[FrameDiff]) -> String {
    let mut out = String::new();
    for d in diffs {
        let line = serde_json::to_string(d).expect("FrameDiff always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_diffs_jsonl(jsonl: &str) -> Result<Vec<FrameDiff>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

// ============================================================================
// Per-script breakdown
// ============================================================================

/// Aggregate result of a parity run across the corpus. The
/// integration harness calls this; the foundation slice runs
/// degenerate self-comparisons (every frame compared to
/// itself) to validate the comparator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParityRunSnapshot {
    pub scripts_total: u32,
    pub frames_compared: u32,
    pub clean_frames: u32,
    pub diverged_frames: u32,
    pub dimension_mismatches: u32,
    pub by_script: BTreeMap<String, ScriptCounters>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptCounters {
    pub frames_compared: u32,
    pub diverged_frames: u32,
    pub max_diverged_cells: u32,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_cell_defaults() {
        let s = RenderCell::space();
        assert_eq!(s.ch, ' ');
        assert!(!s.bold && !s.italic && !s.underline && !s.reverse);
        assert!(!s.continuation);
    }

    #[test]
    fn blank_frame_has_correct_dimensions() {
        let f = RenderFrame::blank(4, 3);
        assert_eq!(f.width, 4);
        assert_eq!(f.height, 3);
        assert_eq!(f.cell_count(), 12);
        assert_eq!(f.cells.len(), 12);
        assert!(f.is_well_shaped());
    }

    #[test]
    fn cell_get_set_roundtrips() {
        let mut f = RenderFrame::blank(3, 2);
        let c = RenderCell {
            ch: 'X',
            bold: true,
            ..RenderCell::space()
        };
        assert!(f.set_cell(1, 2, c));
        let read = f.cell(1, 2).unwrap();
        assert_eq!(read.ch, 'X');
        assert!(read.bold);
    }

    #[test]
    fn cell_out_of_bounds_is_none() {
        let f = RenderFrame::blank(2, 2);
        assert!(f.cell(2, 0).is_none());
        assert!(f.cell(0, 2).is_none());
        assert!(f.cell(99, 99).is_none());
    }

    #[test]
    fn diff_self_is_clean() {
        let f = RenderFrame::blank(10, 10);
        let d = compute_diff(&f, &f);
        assert!(d.is_clean());
        assert_eq!(d.divergent_cell_count(), 0);
    }

    #[test]
    fn diff_dimension_mismatch_flagged() {
        let a = RenderFrame::blank(10, 10);
        let b = RenderFrame::blank(8, 10);
        let d = compute_diff(&a, &b);
        assert!(d.dimension_mismatch);
        assert!(d.cells.is_empty());
        assert!(d.render_summary(10).contains("dimension mismatch"));
    }

    #[test]
    fn diff_single_cell_change() {
        let a = RenderFrame::blank(3, 2);
        let mut b = a.clone();
        let c = RenderCell {
            ch: 'A',
            ..RenderCell::space()
        };
        b.set_cell(1, 2, c);

        let d = compute_diff(&a, &b);
        assert!(!d.is_clean());
        assert_eq!(d.divergent_cell_count(), 1);
        assert_eq!(d.cells[0].row, 1);
        assert_eq!(d.cells[0].col, 2);
        assert_eq!(d.cells[0].right.ch, 'A');
        assert_eq!(d.cells[0].left.ch, ' ');
    }

    #[test]
    fn diff_is_symmetric_in_count() {
        let a = RenderFrame::blank(4, 3);
        let mut b = a.clone();
        b.set_cell(
            0,
            0,
            RenderCell {
                ch: 'X',
                ..RenderCell::space()
            },
        );
        b.set_cell(
            2,
            3,
            RenderCell {
                ch: 'Y',
                ..RenderCell::space()
            },
        );

        let d_ab = compute_diff(&a, &b);
        let d_ba = compute_diff(&b, &a);
        assert_eq!(d_ab.divergent_cell_count(), d_ba.divergent_cell_count());
        assert_eq!(d_ab.divergent_cell_count(), 2);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn ratatui_buffer_projection_preserves_cells_and_style() {
        use ratatui::{
            buffer::Buffer,
            layout::{Position, Rect},
            style::{Color, Modifier, Style},
        };

        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer
            .cell_mut(Position::new(0, 0))
            .unwrap()
            .set_char('R')
            .set_style(
                Style::default()
                    .fg(Color::Rgb(1, 2, 3))
                    .bg(Color::Rgb(4, 5, 6))
                    .add_modifier(
                        Modifier::BOLD
                            | Modifier::ITALIC
                            | Modifier::UNDERLINED
                            | Modifier::REVERSED,
                    ),
            );

        let frame = render_frame_from_ratatui_buffer(&buffer);
        let cell = frame.cell(0, 0).unwrap();
        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 1);
        assert_eq!(cell.ch, 'R');
        assert_eq!(cell.fg, Rgba::opaque(1, 2, 3));
        assert_eq!(cell.bg, Rgba::opaque(4, 5, 6));
        assert!(cell.bold);
        assert!(cell.italic);
        assert!(cell.underline);
        assert!(cell.reverse);
    }

    #[cfg(feature = "ftui")]
    #[test]
    fn ftui_frame_projection_preserves_cells_and_style() {
        use ftui::render::cell::StyleFlags;

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(2, 1, &mut pool);
        let attrs = ftui::CellAttrs::new(
            StyleFlags::BOLD | StyleFlags::ITALIC | StyleFlags::UNDERLINE | StyleFlags::REVERSE,
            ftui::CellAttrs::LINK_ID_NONE,
        );
        frame.buffer.set(
            0,
            0,
            ftui::Cell::from_char('F')
                .with_fg(ftui::PackedRgba::rgba(1, 2, 3, 255))
                .with_bg(ftui::PackedRgba::rgba(4, 5, 6, 255))
                .with_attrs(attrs),
        );

        let projected = render_frame_from_ftui_frame(&frame);
        let cell = projected.cell(0, 0).unwrap();
        assert_eq!(projected.width, 2);
        assert_eq!(projected.height, 1);
        assert_eq!(cell.ch, 'F');
        assert_eq!(cell.fg, Rgba::opaque(1, 2, 3));
        assert_eq!(cell.bg, Rgba::opaque(4, 5, 6));
        assert!(cell.bold);
        assert!(cell.italic);
        assert!(cell.underline);
        assert!(cell.reverse);
    }

    #[cfg(feature = "ftui")]
    #[test]
    fn ftui_default_white_foreground_projects_to_shared_default_fg() {
        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(2, 1, &mut pool);
        frame.buffer.set(0, 0, ftui::Cell::from_char('D'));
        frame.buffer.set(1, 0, ftui::Cell::default());

        let projected = render_frame_from_ftui_frame(&frame);

        assert_eq!(projected.cell(0, 0).unwrap().fg, Rgba::DEFAULT_FG);
        assert_eq!(projected.cell(0, 1).unwrap().fg, Rgba::DEFAULT_FG);
    }

    #[cfg(feature = "ftui")]
    #[test]
    fn ftui_styled_explicit_white_foreground_stays_opaque_white() {
        use ftui::render::cell::StyleFlags;

        let mut pool = ftui::GraphemePool::new();
        let mut frame = ftui::Frame::new(1, 1, &mut pool);
        frame.buffer.set(
            0,
            0,
            ftui::Cell::from_char('W')
                .with_fg(ftui::PackedRgba::WHITE)
                .with_attrs(ftui::CellAttrs::new(
                    StyleFlags::BOLD,
                    ftui::CellAttrs::LINK_ID_NONE,
                )),
        );

        let projected = render_frame_from_ftui_frame(&frame);

        assert_eq!(
            projected.cell(0, 0).unwrap().fg,
            Rgba::opaque(255, 255, 255)
        );
    }

    #[cfg(all(feature = "tui", feature = "ftui"))]
    #[test]
    fn projected_backend_frames_are_diff_comparable() {
        use ftui::render::cell::StyleFlags;
        use ratatui::{
            buffer::Buffer,
            layout::{Position, Rect},
            style::{Color, Modifier, Style},
        };

        let mut ratatui_buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        ratatui_buffer
            .cell_mut(Position::new(0, 0))
            .unwrap()
            .set_char('O')
            .set_style(
                Style::default()
                    .fg(Color::Rgb(10, 20, 30))
                    .bg(Color::Rgb(40, 50, 60))
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            );
        ratatui_buffer
            .cell_mut(Position::new(1, 0))
            .unwrap()
            .set_char('K');

        let mut pool = ftui::GraphemePool::new();
        let mut ftui_frame = ftui::Frame::new(2, 1, &mut pool);
        let attrs = ftui::CellAttrs::new(
            StyleFlags::BOLD | StyleFlags::UNDERLINE,
            ftui::CellAttrs::LINK_ID_NONE,
        );
        ftui_frame.buffer.set(
            0,
            0,
            ftui::Cell::from_char('O')
                .with_fg(ftui::PackedRgba::rgba(10, 20, 30, 255))
                .with_bg(ftui::PackedRgba::rgba(40, 50, 60, 255))
                .with_attrs(attrs),
        );
        ftui_frame.buffer.set(1, 0, ftui::Cell::from_char('K'));

        let ratatui_frame = render_frame_from_ratatui_buffer(&ratatui_buffer);
        let ftui_frame = render_frame_from_ftui_frame(&ftui_frame);
        let diff = compute_diff(&ratatui_frame, &ftui_frame);

        assert!(diff.is_clean(), "{}", diff.render_summary(10));
    }

    #[test]
    fn structurally_equal_ignores_continuation_flag() {
        let a = RenderCell::space();
        let b = RenderCell {
            continuation: true,
            ..RenderCell::space()
        };
        assert!(a.structurally_equal(b));
    }

    #[test]
    fn corpus_size_meets_minimum() {
        let c = synthesized_event_corpus();
        // Bead requires "every input action covered."
        // Synthesized corpus is the foundation; sub-bead 1.1
        // adds vhs/asciinema rows.
        assert!(
            c.len() >= 10,
            "corpus too small: {} (expected ≥10)",
            c.len()
        );
    }

    #[test]
    fn corpus_script_names_are_unique() {
        use std::collections::HashSet;
        let c = synthesized_event_corpus();
        let mut seen = HashSet::new();
        for s in &c {
            assert!(seen.insert(s.name.clone()), "duplicate script: {}", s.name);
        }
    }

    #[test]
    fn corpus_ends_in_quit() {
        // Every synthesized script terminates in Quit so the
        // backend driver has a clean shutdown signal.
        for s in synthesized_event_corpus() {
            assert!(
                matches!(s.actions.last(), Some(KeymapAction::Quit)),
                "script {} doesn't end in Quit",
                s.name,
            );
        }
    }

    #[test]
    fn keymap_action_kind_count() {
        assert_eq!(KeymapActionKind::ALL.len(), 32);
    }

    #[test]
    fn every_keymap_action_has_a_kind() {
        // Smoke: a few representative variants project to the
        // right kind. The integration test asserts coverage
        // against the canonical keymap::Action.
        assert_eq!(KeymapAction::Quit.kind(), KeymapActionKind::Quit);
        assert_eq!(
            KeymapAction::GoToView { view_index: 3 }.kind(),
            KeymapActionKind::GoToView
        );
        assert_eq!(
            KeymapAction::FilterAppendChar { ch: 'a' }.kind(),
            KeymapActionKind::FilterAppendChar
        );
        assert_eq!(
            KeymapAction::TriageNumberedAction { index: 5 }.kind(),
            KeymapActionKind::TriageNumberedAction
        );
    }

    #[test]
    fn baseline_health_unsafe_until_compared() {
        // Per ft-11d5f sweep fix: cold baseline is unsafe (no
        // frames compared yet). Previously pinned the rubber-
        // stamp behavior.
        let h = OracleHealth::baseline();
        assert!(!h.is_safe(), "cold baseline must be unsafe");
        assert!(h.divergence_rate().abs() <= f64::EPSILON);
    }

    #[test]
    fn fold_diff_clean_frame() {
        let f = RenderFrame::blank(4, 4);
        let d = compute_diff(&f, &f);
        let mut h = OracleHealth::baseline();
        fold_diff(&mut h, &d);
        assert_eq!(h.frames_compared_total, 1);
        assert_eq!(h.clean_frames_total, 1);
        assert_eq!(h.diverged_frames_total, 0);
        assert!(h.is_safe());
    }

    #[test]
    fn fold_diff_diverged_frame_marks_unsafe() {
        let a = RenderFrame::blank(4, 4);
        let mut b = a.clone();
        b.set_cell(
            0,
            0,
            RenderCell {
                ch: 'Z',
                ..RenderCell::space()
            },
        );
        let d = compute_diff(&a, &b);
        let mut h = OracleHealth::baseline();
        fold_diff(&mut h, &d);
        assert_eq!(h.diverged_frames_total, 1);
        assert_eq!(h.max_diverged_cells_in_run, 1);
        assert!(!h.is_safe());
    }

    #[test]
    fn fold_diff_dimension_mismatch_marks_unsafe() {
        let a = RenderFrame::blank(4, 4);
        let b = RenderFrame::blank(5, 4);
        let d = compute_diff(&a, &b);
        let mut h = OracleHealth::baseline();
        fold_diff(&mut h, &d);
        assert_eq!(h.dimension_mismatch_total, 1);
        assert_eq!(h.diverged_frames_total, 1);
        assert!(!h.is_safe());
    }

    #[test]
    fn jsonl_diffs_roundtrip() {
        let a = RenderFrame::blank(3, 2);
        let mut b = a.clone();
        b.set_cell(
            0,
            0,
            RenderCell {
                ch: 'A',
                ..RenderCell::space()
            },
        );
        let diffs = vec![compute_diff(&a, &b), compute_diff(&a, &a)];
        let jsonl = render_diffs_jsonl(&diffs);
        let parsed = parse_diffs_jsonl(&jsonl).unwrap();
        assert_eq!(parsed, diffs);
    }

    #[test]
    fn render_summary_truncates_at_max_lines() {
        let mut a = RenderFrame::blank(10, 10);
        let b = a.clone();
        for r in 0..10 {
            for c in 0..10 {
                a.set_cell(
                    r,
                    c,
                    RenderCell {
                        ch: 'X',
                        ..RenderCell::space()
                    },
                );
            }
        }
        let d = compute_diff(&a, &b);
        let s = d.render_summary(5);
        assert!(s.contains("and "));
        assert!(s.contains(" more divergent cells"));
    }
}
