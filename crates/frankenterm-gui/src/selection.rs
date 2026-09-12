// The range_plus_one lint can't see when the LHS is not compatible with
// and inclusive range
#![allow(clippy::range_plus_one)]
use frankenterm_core::smart_selection::SelectionPatternKind;
use frankenterm_core::smart_selection_patterns::{smart_match_at_click, smart_match_in_line};
use mux::pane::Pane;
use std::cmp::Ordering;
use std::ops::Range;
use termwiz::surface::SequenceNo;
use termwiz::surface::line::DoubleClickRange;
use wezterm_term::unicode_column_width;
use wezterm_term::{SemanticZone, StableRowIndex};

/// Result of a successful smart-selection pick. Carries the pattern
/// kind plus the selected text so the GUI mouse handler can emit
/// the matching `SmartSelectionA11yMessage` to the AT-tree without
/// re-borrowing the line text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmartSelectionPick {
    pub kind: SelectionPatternKind,
    pub text: String,
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct Selection {
    /// Remembers the starting coordinate of the selection prior to
    /// dragging.
    pub origin: Option<SelectionCoordinate>,
    /// Holds the not-normalized selection range.
    pub range: Option<SelectionRange>,
    /// When the selection was made wrt. the pane content
    pub seqno: SequenceNo,
    /// Authority of the coordinates, independent of the drag's damage seqno.
    pub authority: Option<SelectionAuthority>,
    /// Whether the selection is rectangular
    pub rectangular: bool,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct SelectionAuthority {
    source: usize,
    sequence: SequenceNo,
    geometry: (usize, usize, u32, usize, usize),
    alternate: bool,
}

impl SelectionAuthority {
    pub fn capture(pane: &dyn Pane) -> Option<Self> {
        Self::capture_source(pane).map(|(authority, _, _)| authority)
    }

    pub fn capture_source(
        pane: &dyn Pane,
    ) -> Option<(Self, SequenceNo, mux::renderable::RenderableDimensions)> {
        // Native layout capture is atomic and nonblocking. Busy must never
        // fall back to a fabricated stamp from separately sampled metadata.
        let (sequence, source_sequence, dims, alternate) = if let Some(local) =
            pane.downcast_ref::<mux::localpane::LocalPane>()
        {
            let (floor, source_sequence, dims) = local.selection_source_snapshot()?;
            (floor, source_sequence, dims, false) // The native floor includes Screen identity.
        } else if let Some(client) = pane.downcast_ref::<frankenterm_client::pane::ClientPane>() {
            client.selection_source_snapshot()?
        } else {
            // Other backends do not expose a layout floor yet. Conservatively
            // bind to their content sequence rather than disabling selection.
            let before = pane.get_current_seqno();
            let dims = pane.get_dimensions();
            let alternate = pane.is_alt_screen_active();
            if pane.get_current_seqno() != before {
                return None;
            }
            (before, before, dims, alternate)
        };
        if sequence == SequenceNo::MAX {
            return None;
        }
        Some((
            Self {
                source: pane as *const dyn Pane as *const () as usize,
                sequence,
                geometry: (
                    dims.cols,
                    dims.viewport_rows,
                    dims.dpi,
                    dims.pixel_width,
                    dims.pixel_height,
                ),
                alternate,
            },
            source_sequence,
            dims,
        ))
    }
}

/// Coordinates of a fully populated pane in a submitted frame. This is a
/// synchronous presentation boundary, not a GPU-completion/scanout receipt.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct SelectionFrameStamp {
    pub authority: SelectionAuthority,
    pub source_sequence: SequenceNo,
    pub viewport: StableRowIndex,
    pub geometry: [usize; 12],
}

impl SelectionFrameStamp {
    pub fn same_coordinates(self, other: Self) -> bool {
        self.authority == other.authority
            && self.viewport == other.viewport
            && self.geometry == other.geometry
    }
}

/// A press whose displayed coordinates are known, but whose source is busy.
#[derive(Debug, Copy, Clone)]
pub struct PendingSelectionStart {
    pub frame: SelectionFrameStamp,
    pub coordinate: SelectionCoordinate,
    pub mode: SelectionMode,
    pub paint_retries_remaining: u8,
}

impl PendingSelectionStart {
    pub fn take_paint_retry(&mut self) -> bool {
        let Some(remaining) = self.paint_retries_remaining.checked_sub(1) else {
            return false;
        };
        self.paint_retries_remaining = remaining;
        true
    }

    /// Defer a busy/unpresented frame and retire obsolete pixel coordinates.
    pub fn resolve(
        self,
        current: Option<SelectionFrameStamp>,
        frames: &SelectionFrameState,
    ) -> PendingSelectionResolution {
        let Some(current) = current else {
            return PendingSelectionResolution::Wait;
        };
        if !self.frame.same_coordinates(current) {
            return PendingSelectionResolution::Invalidated;
        }
        if frames.for_mouse(Some(current)).is_some() {
            PendingSelectionResolution::Ready
        } else {
            PendingSelectionResolution::Wait
        }
    }
}

#[derive(Debug)]
pub enum PendingSelectionResolution {
    Wait,
    Ready,
    Invalidated,
}

#[derive(Debug, Default)]
pub struct SelectionFrameState {
    pending: Option<SelectionFrameStamp>,
    presented: Option<SelectionFrameStamp>,
}

impl SelectionFrameState {
    pub fn displayed_for_geometry(&self, geometry: [usize; 12]) -> Option<SelectionFrameStamp> {
        self.presented.filter(|frame| frame.geometry == geometry)
    }
    pub fn begin_attempt(&mut self) {
        self.pending = None;
    }
    pub fn stage(
        &mut self,
        before: Option<SelectionFrameStamp>,
        after: Option<SelectionFrameStamp>,
        complete: bool,
    ) {
        self.pending = before
            .filter(|before| complete && after.is_some_and(|after| before.same_coordinates(after)));
    }
    pub fn presented(&mut self) {
        self.presented = self.pending.take();
    }
    pub fn for_mouse(&self, current: Option<SelectionFrameStamp>) -> Option<SelectionFrameStamp> {
        self.presented
            .filter(|presented| current.is_some_and(|current| presented.same_coordinates(current)))
    }
}

pub use config::keyassignment::SelectionMode;

impl Selection {
    /// An unavailable nonblocking snapshot is not evidence of a new layout.
    /// Keep the anchor until it can be checked; reads still require authority.
    pub fn is_invalidated_by(&self, current: Option<SelectionAuthority>) -> bool {
        current.is_some() && self.authority.is_some() && self.authority != current
    }

    pub fn is_authorized_by(&self, current: Option<SelectionAuthority>) -> bool {
        self.authority.is_some() && self.authority == current
    }
    pub fn clear(&mut self) {
        self.range = None;
        self.origin = None;
        self.authority = None;
    }

    pub fn begin(&mut self, origin: SelectionCoordinate) {
        self.range = None;
        self.origin = Some(origin);
    }

    pub fn is_empty(&self) -> bool {
        self.range.is_none()
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SelectionX {
    /// Zero-based cell index
    Cell(usize),
    /// Exactly before the 0th cell
    BeforeZero,
}

impl SelectionX {
    pub const fn saturating_add(self, rhs: usize) -> Self {
        match self {
            Self::Cell(x) => Self::Cell(x.saturating_add(rhs)),
            Self::BeforeZero => {
                if rhs == 0 {
                    Self::BeforeZero
                } else {
                    Self::Cell(rhs - 1)
                }
            }
        }
    }

    pub const fn saturating_sub(self, rhs: usize) -> Self {
        match self {
            Self::Cell(x) => match x.checked_sub(rhs) {
                Some(x) => Self::Cell(x),
                None => Self::BeforeZero,
            },
            Self::BeforeZero => Self::BeforeZero,
        }
    }

    pub const fn range(self, rhs: Self) -> Range<usize> {
        match self {
            Self::Cell(left) => match rhs {
                Self::Cell(right) => left..right,
                Self::BeforeZero => 0..0,
            },
            Self::BeforeZero => match rhs {
                Self::Cell(right) => 0..right,
                Self::BeforeZero => 0..0,
            },
        }
    }
}

impl Default for SelectionX {
    // Default is 0th cell
    fn default() -> Self {
        Self::Cell(0)
    }
}

impl PartialEq<usize> for SelectionX {
    fn eq(&self, other: &usize) -> bool {
        match self {
            Self::Cell(x) => x == other,
            _ => false,
        }
    }
}

impl PartialEq<SelectionX> for usize {
    fn eq(&self, other: &SelectionX) -> bool {
        other == self
    }
}

impl Ord for SelectionX {
    fn cmp(&self, other: &Self) -> Ordering {
        match self {
            Self::Cell(x1) => match other {
                Self::Cell(x2) => x1.cmp(x2),
                Self::BeforeZero => Ordering::Greater,
            },
            Self::BeforeZero => match other {
                Self::Cell(_) => Ordering::Less,
                Self::BeforeZero => Ordering::Equal,
            },
        }
    }
}

impl PartialOrd for SelectionX {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialOrd<usize> for SelectionX {
    fn partial_cmp(&self, other: &usize) -> Option<Ordering> {
        self.partial_cmp(&Self::Cell(*other))
    }
}

impl PartialOrd<SelectionX> for usize {
    fn partial_cmp(&self, other: &SelectionX) -> Option<Ordering> {
        SelectionX::Cell(*self).partial_cmp(other)
    }
}

/// The x,y coordinates of either the start or end of a selection region
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct SelectionCoordinate {
    pub x: SelectionX,
    pub y: StableRowIndex,
}

impl SelectionCoordinate {
    pub const fn x_y(x: usize, y: StableRowIndex) -> Self {
        Self {
            x: SelectionX::Cell(x),
            y,
        }
    }
}

/// Represents the selected text range.
/// The end coordinates are inclusive.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct SelectionRange {
    pub start: SelectionCoordinate,
    pub end: SelectionCoordinate,
}

fn is_double_click_word(s: &str) -> bool {
    match s.chars().count() {
        1 => !config::configuration().selection_word_boundary.contains(s),
        0 => false,
        _ => true,
    }
}

fn byte_offset_for_logical_x(text: &str, logical_x: usize) -> usize {
    let mut display_x: usize = 0;
    for (byte_offset, ch) in text.char_indices() {
        let next_byte_offset = byte_offset + ch.len_utf8();
        let width = unicode_column_width(&text[byte_offset..next_byte_offset], None);
        let next_display_x = display_x.saturating_add(width);
        if logical_x < next_display_x {
            return byte_offset;
        }
        display_x = next_display_x;
    }
    text.len()
}

fn logical_x_for_byte_offset(text: &str, byte_offset: usize) -> usize {
    if byte_offset >= text.len() {
        return unicode_column_width(text, None);
    }

    let Some(prefix) = text.get(..byte_offset) else {
        return unicode_column_width(text, None);
    };
    unicode_column_width(prefix, None)
}

/// Smart-match result that carries both the GUI-side display range
/// (logical-x columns, post-unicode-width translation) and the
/// pattern kind + selected text needed to emit a
/// `SmartSelectionA11yMessage`.
struct SmartLogicalMatch {
    range: Range<usize>,
    kind: SelectionPatternKind,
    text: String,
}

fn smart_match_logical_x_range(text: &str, click_logical_x: usize) -> Option<SmartLogicalMatch> {
    let click_byte_offset = byte_offset_for_logical_x(text, click_logical_x);
    let smart_match = smart_match_at_click(text, click_byte_offset)?;
    let start = logical_x_for_byte_offset(text, smart_match.span_start);
    let end_exclusive = logical_x_for_byte_offset(text, smart_match.span_end);
    if start >= end_exclusive {
        return None;
    }
    let selected_text = text
        .get(smart_match.span_start..smart_match.span_end)?
        .to_string();
    Some(SmartLogicalMatch {
        range: start..end_exclusive,
        kind: smart_match.kind,
        text: selected_text,
    })
}

impl SelectionRange {
    fn from_logical_click_range(
        start: SelectionCoordinate,
        logical: &mux::pane::LogicalLine,
        click_range: Range<usize>,
    ) -> Self {
        if click_range.is_empty() {
            return Self { start, end: start };
        }

        let (start_y, start_x) = logical.logical_x_to_physical_coord(click_range.start);
        let (end_y, end_x) = logical.logical_x_to_physical_coord(click_range.end - 1);
        Self {
            start: SelectionCoordinate::x_y(start_x, start_y),
            end: SelectionCoordinate::x_y(end_x, end_y),
        }
    }

    /// Create a new range that starts at the specified location
    pub fn start(start: SelectionCoordinate) -> Self {
        let end = start;
        Self { start, end }
    }

    /// Computes the selection range for the line around the specified coords
    pub fn line_around(start: SelectionCoordinate, pane: &dyn Pane) -> Self {
        for logical in pane.get_logical_lines(start.y..start.y + 1) {
            if logical.contains_y(start.y) {
                return Self {
                    start: SelectionCoordinate::x_y(0, logical.first_row),
                    end: SelectionCoordinate::x_y(
                        usize::MAX,
                        logical.first_row + (logical.physical_lines.len() - 1) as StableRowIndex,
                    ),
                };
            }
        }
        // Shouldn't happen, but return a reasonable fallback
        Self { start, end: start }
    }

    pub fn zone_around(start: SelectionCoordinate, pane: &dyn mux::pane::Pane) -> Self {
        let zones = match pane.get_semantic_zones() {
            Ok(z) => z,
            Err(_) => return Self { start, end: start },
        };

        fn find_zone(start: &SelectionCoordinate, zone: &SemanticZone) -> Ordering {
            match zone.start_y.cmp(&start.y) {
                Ordering::Greater => return Ordering::Greater,
                // If the zone starts on the same line then check that the
                // x position is within bounds
                Ordering::Equal => match SelectionX::Cell(zone.start_x).cmp(&start.x) {
                    Ordering::Greater => return Ordering::Greater,
                    Ordering::Equal | Ordering::Less => {}
                },
                Ordering::Less => {}
            }
            match zone.end_y.cmp(&start.y) {
                Ordering::Less => Ordering::Less,
                // If the zone ends on the same line then check that the
                // x position is within bounds
                Ordering::Equal => match SelectionX::Cell(zone.end_x).cmp(&start.x) {
                    Ordering::Less => Ordering::Less,
                    Ordering::Equal | Ordering::Greater => Ordering::Equal,
                },
                Ordering::Greater => Ordering::Equal,
            }
        }

        if let Ok(idx) = zones.binary_search_by(|zone| find_zone(&start, zone)) {
            let zone = &zones[idx];
            Self {
                start: SelectionCoordinate::x_y(zone.start_x, zone.start_y),
                end: SelectionCoordinate::x_y(zone.end_x, zone.end_y),
            }
        } else {
            Self { start, end: start }
        }
    }

    /// Computes the selection range for the word around the specified coords
    pub fn word_around(start: SelectionCoordinate, pane: &dyn Pane) -> Self {
        for logical in pane.get_logical_lines(start.y..start.y + 1) {
            if !logical.contains_y(start.y) {
                continue;
            }

            if let SelectionX::Cell(start_x) = start.x {
                let start_idx = logical.xy_to_logical_x(start_x, start.y);
                return match logical
                    .logical
                    .compute_double_click_range(start_idx, is_double_click_word)
                {
                    DoubleClickRange::RangeWithWrap(click_range)
                    | DoubleClickRange::Range(click_range) => {
                        Self::from_logical_click_range(start, &logical, click_range)
                    }
                };
            }
        }

        // Shouldn't happen, but return a reasonable fallback
        Self { start, end: start }
    }

    /// Computes the smart-selection range for the specified coords,
    /// falling back to the legacy word-boundary selection when the
    /// smart-selection catalog has no match at the click position.
    ///
    /// Returns the resolved range plus a `Some(SmartSelectionPick)`
    /// when a smart pattern was matched (so the caller can emit the
    /// AT-tree announcement) or `None` when the word-boundary
    /// fallback fired (avoids screen-reader noise on plain word
    /// picks per ft-cnil8.4 acceptance).
    pub fn smart_or_word_around(
        start: SelectionCoordinate,
        pane: &dyn Pane,
    ) -> (Self, Option<SmartSelectionPick>) {
        for logical in pane.get_logical_lines(start.y..start.y + 1) {
            if !logical.contains_y(start.y) {
                continue;
            }

            if let SelectionX::Cell(start_x) = start.x {
                let click_logical_x = logical.xy_to_logical_x(start_x, start.y);
                let line_text = logical.logical.as_str();
                if let Some(smart) = smart_match_logical_x_range(&line_text, click_logical_x) {
                    let (start_y, start_x) = logical.logical_x_to_physical_coord(smart.range.start);
                    let (end_y, end_x) =
                        logical.logical_x_to_physical_coord(smart.range.end.saturating_sub(1));
                    let range = Self {
                        start: SelectionCoordinate::x_y(start_x, start_y),
                        end: SelectionCoordinate::x_y(end_x, end_y),
                    };
                    return (
                        range,
                        Some(SmartSelectionPick {
                            kind: smart.kind,
                            text: smart.text,
                        }),
                    );
                }
            }
        }

        (Self::word_around(start, pane), None)
    }

    /// Computes the smart-selection range for the *line* around the
    /// specified coords (triple-click semantics), falling back to
    /// the legacy full-physical-line selection when the smart-
    /// selection catalog has no widest-pattern fully contained in
    /// the line.
    ///
    /// Returns the resolved range plus a `Some(SmartSelectionPick)`
    /// when a smart pattern was matched in the line — so the caller
    /// can emit the AT-tree announcement — or `None` when the
    /// legacy line fallback fired (avoids screen-reader noise on
    /// plain line picks, per ft-cnil8.4 acceptance).
    ///
    /// br-ft-t5j0a: closes the ghostwiring gap left by ft-cnil8.2's
    /// substrate-only closure. The substrate
    /// (`smart_match_in_line` at c1cfbb435 + `classify_triple_click`
    /// at 668e8d662) was unreachable from the GUI mouse handler
    /// until this function landed.
    pub fn smart_or_line_around(
        start: SelectionCoordinate,
        pane: &dyn Pane,
    ) -> (Self, Option<SmartSelectionPick>) {
        for logical in pane.get_logical_lines(start.y..start.y + 1) {
            if !logical.contains_y(start.y) {
                continue;
            }

            let line_text = logical.logical.as_str();
            // Triple-click resolves to the widest smart-pattern fully
            // contained within the line. `smart_match_in_line` runs
            // find_all → drop_shell_quoted_supersets →
            // classify_triple_click in one call.
            if let Some(selection_match) = smart_match_in_line(&line_text, 0, line_text.len()) {
                let logical_start =
                    logical_x_for_byte_offset(&line_text, selection_match.span_start);
                let logical_end = logical_x_for_byte_offset(&line_text, selection_match.span_end);
                if logical_start < logical_end {
                    let (start_y, start_x) = logical.logical_x_to_physical_coord(logical_start);
                    let (end_y, end_x) =
                        logical.logical_x_to_physical_coord(logical_end.saturating_sub(1));
                    if let Some(text) =
                        line_text.get(selection_match.span_start..selection_match.span_end)
                    {
                        let range = Self {
                            start: SelectionCoordinate::x_y(start_x, start_y),
                            end: SelectionCoordinate::x_y(end_x, end_y),
                        };
                        return (
                            range,
                            Some(SmartSelectionPick {
                                kind: selection_match.kind,
                                text: text.to_string(),
                            }),
                        );
                    }
                }
            }
        }

        (Self::line_around(start, pane), None)
    }

    /// Extends the current selection by unioning it with another selection range
    pub fn extend_with(&self, other: Self) -> Self {
        let norm = self.normalize();
        let other = other.normalize();
        let (start, end) = if (norm.start.y < other.start.y)
            || (norm.start.y == other.start.y && norm.start.x <= other.start.x)
        {
            (norm, other)
        } else {
            (other, norm)
        };
        Self {
            start: start.start,
            end: end.end,
        }
    }

    /// Returns an extended selection that it ends at the specified location
    pub fn extend(&self, end: SelectionCoordinate) -> Self {
        Self {
            start: self.start,
            end,
        }
    }

    /// Return a normalized selection such that the starting y coord
    /// is <= the ending y coord.
    pub fn normalize(&self) -> Self {
        if self.start.y <= self.end.y {
            *self
        } else {
            Self {
                start: self.end,
                end: self.start,
            }
        }
    }

    /// Yields a range representing the row indices.
    /// Make sure that you invoke this on a normalized range!
    pub fn rows(&self) -> Range<StableRowIndex> {
        let norm = self.normalize();
        norm.start.y..norm.end.y + 1
    }

    /// Yields a range representing the selected columns for the specified row.
    /// Note that the range may include `usize::MAX` for some rows; this
    /// indicates that the selection extends to the end of that row.
    /// Since this struct has no knowledge of line length, it cannot be
    /// more precise than that.
    /// Must be called on a normalized range!
    pub fn cols_for_row(&self, row: StableRowIndex, rectangular: bool) -> Range<usize> {
        let norm = self.normalize();

        if rectangular {
            if row < norm.start.y || row > norm.end.y {
                0..0
            } else {
                if norm.start.x <= norm.end.x {
                    norm.start.x.range(norm.end.x.saturating_add(1))
                } else {
                    norm.end.x.range(norm.start.x.saturating_add(1))
                }
            }
        } else {
            if row < norm.start.y || row > norm.end.y {
                0..0
            } else if norm.start.y == norm.end.y {
                // A single line selection
                if norm.start.x <= norm.end.x {
                    norm.start.x.range(norm.end.x.saturating_add(1))
                } else {
                    norm.end.x.range(norm.start.x.saturating_add(1))
                }
            } else if row == norm.end.y {
                // last line of multi-line
                SelectionX::Cell(0).range(norm.end.x.saturating_add(1))
            } else if row == norm.start.y {
                // first line of multi-line
                norm.start.x.range(SelectionX::Cell(usize::MAX))
            } else {
                // some "middle" line of multi-line
                0..usize::MAX
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_coordinates_cannot_be_reauthorized_by_drag_damage_sequence() {
        let authority = SelectionAuthority {
            source: 1,
            sequence: 10,
            geometry: (80, 24, 96, 800, 480),
            alternate: false,
        };
        let mut selection = Selection::default();
        selection.begin(SelectionCoordinate::x_y(0, -100));
        selection.range = Some(SelectionRange::start(SelectionCoordinate::x_y(0, -100)));
        selection.authority = Some(authority);
        selection.seqno = 10;
        assert!(selection.is_authorized_by(Some(authority)));
        assert!(!selection.is_authorized_by(None));
        assert!(!selection.is_invalidated_by(None));
        assert!(!selection.is_invalidated_by(Some(authority)));
        let changed = SelectionAuthority {
            sequence: 11,
            ..authority
        };
        selection.seqno = 100; // dragging/dirty updates do not move the anchor epoch
        assert!(!selection.is_authorized_by(Some(changed)));
        assert!(selection.is_invalidated_by(Some(changed)));
        assert!(!selection.is_authorized_by(Some(SelectionAuthority {
            source: 2,
            ..authority
        })));
        assert!(!selection.is_authorized_by(Some(SelectionAuthority {
            alternate: true,
            ..authority
        })));
        assert!(!selection.is_authorized_by(Some(SelectionAuthority {
            geometry: (79, 24, 96, 790, 480),
            ..authority
        })));
        selection.clear();
        assert!(!selection.is_authorized_by(Some(authority)));
        assert!(selection.origin.is_none() && selection.range.is_none());
    }

    #[test]
    fn selection_frame_authority_requires_complete_successful_presentation() {
        let a = SelectionFrameStamp {
            authority: SelectionAuthority {
                source: 1,
                sequence: 10,
                geometry: (80, 24, 96, 800, 480),
                alternate: false,
            },
            source_sequence: 10,
            viewport: -100,
            geometry: [0; 12],
        };
        let mut b = a;
        b.authority.sequence = 11;
        b.source_sequence = 11;
        let mut state = SelectionFrameState::default();
        state.begin_attempt();
        state.stage(Some(a), Some(a), true);
        assert_eq!(state.for_mouse(Some(a)), None); // geometry is not presentation
        state.presented();
        assert_eq!(state.for_mouse(Some(a)), Some(a));
        state.begin_attempt();
        state.stage(Some(b), Some(b), true);
        // Failed swap leaves A displayed; damage/query B cannot authorize B.
        assert_eq!(state.for_mouse(Some(b)), None);
        assert_eq!(state.for_mouse(Some(a)), Some(a));
        state.begin_attempt(); // atlas retry must discard the B candidate
        state.stage(Some(b), Some(b), false); // incomplete cold rows
        state.presented();
        assert_eq!(state.for_mouse(Some(b)), None);
        state.begin_attempt();
        state.stage(Some(a), Some(b), true); // layout changed during snapshot
        state.presented();
        assert_eq!(state.for_mouse(Some(b)), None);
        state.begin_attempt();
        state.stage(Some(b), Some(b), true);
        state.presented();
        assert_eq!(state.for_mouse(Some(b)), Some(b));
        let mut scrolled = b;
        scrolled.viewport += 1;
        assert_eq!(state.for_mouse(Some(scrolled)), None);
        let mut rescaled = b;
        rescaled.geometry[0] += 1;
        assert_eq!(state.for_mouse(Some(rescaled)), None);
        let mut output = b;
        output.source_sequence += 1;
        assert_eq!(state.for_mouse(Some(output)), Some(b));
        state.begin_attempt();
        state.stage(Some(b), Some(output), true);
        state.presented();
        assert_eq!(state.for_mouse(Some(output)), Some(b));
        state.begin_attempt(); // pane omitted by successful next frame
        state.presented();
        assert_eq!(state.for_mouse(Some(b)), None);
    }

    #[test]
    fn selection_drag_survives_unavailable_frame_and_autoscroll_until_layout_changes() {
        let frame = SelectionFrameStamp {
            authority: SelectionAuthority {
                source: 1,
                sequence: 10,
                geometry: (80, 24, 96, 800, 480),
                alternate: false,
            },
            source_sequence: 10,
            viewport: 100,
            geometry: [0; 12],
        };
        let mut selection = Selection::default();
        let anchor = SelectionCoordinate::x_y(3, 110);
        selection.begin(anchor);
        selection.authority = Some(frame.authority);
        let mut state = SelectionFrameState::default();
        state.stage(Some(frame), Some(frame), true);
        state.presented();

        // Unlike an established drag, a busy press must retain its ORIGINAL
        // click until a usable frame is available, regardless of later motion.
        let pending = PendingSelectionStart {
            frame,
            coordinate: anchor,
            mode: SelectionMode::Cell,
            paint_retries_remaining: 3,
        };
        let mut retries = pending;
        for _ in 0..3 {
            assert!(retries.take_paint_retry());
        }
        assert!(!retries.take_paint_retry());
        assert!(matches!(
            pending.resolve(None, &state),
            PendingSelectionResolution::Wait
        ));
        let mut unpresented = SelectionFrameState::default();
        assert!(matches!(
            pending.resolve(Some(frame), &unpresented),
            PendingSelectionResolution::Wait
        ));
        unpresented.stage(Some(frame), Some(frame), true);
        unpresented.presented();
        assert!(matches!(
            pending.resolve(Some(frame), &unpresented),
            PendingSelectionResolution::Ready
        ));
        assert_eq!(pending.coordinate, anchor);
        assert_eq!(pending.mode, SelectionMode::Cell);
        let mut different_geometry = frame.geometry;
        different_geometry[0] += 1;
        assert!(state.displayed_for_geometry(different_geometry).is_none());

        // The parser owns the lock: neither extend nor destroy the anchor.
        assert_eq!(state.for_mouse(None), None);
        assert!(!selection.is_invalidated_by(None));
        assert_eq!(selection.origin, Some(anchor));
        let scrolled = SelectionFrameStamp {
            viewport: 101,
            source_sequence: 12,
            ..frame
        };
        assert!(matches!(
            pending.resolve(Some(scrolled), &state),
            PendingSelectionResolution::Invalidated
        ));
        assert_eq!(state.for_mouse(Some(scrolled)), None);
        assert!(!selection.is_invalidated_by(Some(scrolled.authority)));
        state.begin_attempt();
        state.stage(Some(scrolled), Some(scrolled), true);
        state.presented();
        assert_eq!(state.for_mouse(Some(scrolled)), Some(scrolled));
        assert_eq!(selection.origin, Some(anchor));

        let replaced = SelectionAuthority {
            sequence: 13,
            ..frame.authority
        };
        assert!(matches!(
            pending.resolve(
                Some(SelectionFrameStamp {
                    authority: replaced,
                    ..frame
                }),
                &state
            ),
            PendingSelectionResolution::Invalidated
        ));
        assert!(selection.is_invalidated_by(Some(replaced)));
        assert_eq!(
            state.for_mouse(Some(SelectionFrameStamp {
                authority: replaced,
                ..scrolled
            })),
            None
        );
    }

    #[test]
    fn smart_match_logical_x_range_selects_url_inside_quotes() {
        let text = "open \"https://example.com/foo?bar=1\" now";
        let click_x = text.find("example").unwrap();

        let smart = smart_match_logical_x_range(text, click_x).expect("URL match");

        assert_eq!(&text[smart.range.clone()], "https://example.com/foo?bar=1");
        assert_eq!(smart.kind, SelectionPatternKind::Url);
        assert_eq!(smart.text, "https://example.com/foo?bar=1");
    }

    #[test]
    fn smart_match_logical_x_range_returns_none_on_whitespace() {
        let text = "alpha beta";
        let click_x = text.find(' ').unwrap();

        assert!(smart_match_logical_x_range(text, click_x).is_none());
    }

    #[test]
    fn byte_offset_for_logical_x_handles_wide_prefix() {
        let text = "表 https://example.com";
        let click_x = 3;

        assert_eq!(byte_offset_for_logical_x(text, click_x), "表 ".len());
    }

    #[test]
    fn word_click_range_empty_stays_at_clicked_point() {
        let line: termwiz::surface::line::Line = "abc".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![line.clone()],
            logical: line,
            first_row: 0,
        };
        let start = SelectionCoordinate::x_y(1, 0);

        let range = SelectionRange::from_logical_click_range(start, &logical, 0..0);

        assert_eq!(range, SelectionRange { start, end: start });
    }

    // ----------------------------------------------------------------
    // br-ft-t5j0a: smart_match_in_line wiring proof at the GUI-helper
    // layer. The Pane-driven `smart_or_line_around` integration is
    // tested via the bin-level selection tests; these unit tests
    // pin the substrate the new function consumes so a future
    // refactor of `smart_match_in_line` semantics surfaces here
    // immediately.
    // ----------------------------------------------------------------

    #[test]
    fn smart_match_in_line_picks_url_in_full_line_span() {
        // Triple-click selects the widest pattern fully contained in
        // the line. URL inside text → URL wins.
        let text = "see https://example.com/long-path?param=value for details";
        let m = smart_match_in_line(text, 0, text.len()).expect("URL match");
        assert_eq!(m.kind, SelectionPatternKind::Url);
        assert_eq!(
            &text[m.span_start..m.span_end],
            "https://example.com/long-path?param=value"
        );
    }

    #[test]
    fn smart_match_in_line_returns_none_on_plain_text_only_line() {
        // No smart pattern → fallback to legacy line_around must fire
        // (the Pane-driver test verifies that path; here we just
        // confirm the substrate signals "no match").
        let text = "alpha beta gamma delta";
        assert!(smart_match_in_line(text, 0, text.len()).is_none());
    }

    #[test]
    fn smart_match_in_line_picks_email_when_present() {
        let text = "ping ops@example.com on incident";
        let m = smart_match_in_line(text, 0, text.len()).expect("Email match");
        assert_eq!(m.kind, SelectionPatternKind::Email);
        assert_eq!(&text[m.span_start..m.span_end], "ops@example.com");
    }

    #[test]
    fn smart_match_in_line_skips_partial_pattern_when_constrained() {
        // span constrained to before the URL starts → no match.
        let text = "echo https://example.com/x";
        let cutoff = text.find("https").unwrap();
        assert!(smart_match_in_line(text, 0, cutoff).is_none());
    }

    #[test]
    fn smart_match_in_line_url_inside_quotes_wins_over_shell_quoted() {
        // Same drop_shell_quoted_supersets pre-filter the double-
        // click path uses applies here: URL inside 'quotes' beats
        // the surrounding ShellQuoted span.
        let text = r"echo 'https://example.com/foo'";
        let m = smart_match_in_line(text, 0, text.len()).expect("URL match");
        assert_eq!(m.kind, SelectionPatternKind::Url);
        assert_eq!(&text[m.span_start..m.span_end], "https://example.com/foo");
    }
}
