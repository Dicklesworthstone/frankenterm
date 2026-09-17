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

#[derive(Debug, Default, Clone, Eq, PartialEq)]
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
    native_anchor: Option<NativeSelectionAnchor>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct NativeSelectionAnchor {
    token: wezterm_term::screen::ScreenSelectionAnchor,
    authority: Option<SelectionAuthority>,
    origin: Option<SelectionCoordinate>,
    range: Option<SelectionRange>,
}

/// One uncommitted native gesture. Contention must neither replace the last
/// anchored selection nor discard the final endpoint when the button lifts.
#[derive(Debug)]
pub(crate) struct PendingNativeSelection {
    pub desired: Selection,
    pub copy: Option<config::keyassignment::ClipboardCopyDestination>,
    pub paint_retries_remaining: u8,
    pub committed: bool,
    pub text_copy: Option<crate::termwindow::SelectionCopy>,
}

pub(crate) enum NativeSelectionCapture {
    Ready(wezterm_term::screen::ScreenSelectionAnchor),
    Unremappable,
    Busy,
    Invalidated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeSelectionCommit {
    Applied { needs_repaint: bool },
    Invalidated,
}

impl
    From<
        Result<
            Option<wezterm_term::screen::ScreenSelectionAnchor>,
            mux::localpane::SelectionAnchorCaptureError,
        >,
    > for NativeSelectionCapture
{
    fn from(
        result: Result<
            Option<wezterm_term::screen::ScreenSelectionAnchor>,
            mux::localpane::SelectionAnchorCaptureError,
        >,
    ) -> Self {
        match result {
            Ok(Some(token)) => Self::Ready(token),
            Ok(None) => Self::Unremappable,
            Err(mux::localpane::SelectionAnchorCaptureError::Busy) => Self::Busy,
            Err(mux::localpane::SelectionAnchorCaptureError::SourceChanged) => Self::Invalidated,
        }
    }
}

impl PendingNativeSelection {
    pub(crate) fn expire_text_copy(pending: &mut Option<Self>, now: std::time::Instant) -> bool {
        if pending
            .as_ref()
            .and_then(|pending| pending.text_copy.as_ref())
            .is_some_and(|copy| now >= copy.deadline())
        {
            *pending = None;
            true
        } else {
            false
        }
    }

    pub fn new(desired: Selection) -> Self {
        Self {
            desired,
            copy: None,
            paint_retries_remaining: 3,
            committed: false,
            text_copy: None,
        }
    }

    /// None retains the intent; Invalidated retires stale coordinates; Applied commits
    /// only after the source has registered an anchor or explicitly established
    /// current coordinates that cannot be remapped (such as alternate screen).
    pub fn try_commit(
        &self,
        committed: &mut Selection,
        capture: NativeSelectionCapture,
    ) -> Option<NativeSelectionCommit> {
        let mut next = self.desired.clone();
        match capture {
            NativeSelectionCapture::Busy => return None,
            NativeSelectionCapture::Invalidated => return Some(NativeSelectionCommit::Invalidated),
            NativeSelectionCapture::Unremappable => {
                next.native_anchor = None;
            }
            NativeSelectionCapture::Ready(token) => {
                next.remember_native_anchor(token);
            }
        }
        let needs_repaint = committed.origin != next.origin
            || committed.range != next.range
            || committed.authority != next.authority
            || committed.rectangular != next.rectangular;
        *committed = next;
        Some(NativeSelectionCommit::Applied { needs_repaint })
    }

    pub fn take_paint_retry(&mut self) -> bool {
        let Some(remaining) = self.paint_retries_remaining.checked_sub(1) else {
            return false;
        };
        self.paint_retries_remaining = remaining;
        true
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct SelectionAuthority {
    source: usize,
    sequence: SequenceNo,
    geometry: (usize, usize, u32, usize, usize),
    alternate: bool,
}

impl SelectionAuthority {
    pub fn from_native_frame(
        pane: &dyn Pane,
        frame: &mux::localpane::NativeRenderFrame,
    ) -> Option<Self> {
        Self::from_native_snapshot(pane, frame.layout_floor, frame.dimensions)
    }

    pub(crate) fn from_native_snapshot(
        pane: &dyn Pane,
        floor: SequenceNo,
        dims: mux::renderable::RenderableDimensions,
    ) -> Option<Self> {
        if floor == SequenceNo::MAX {
            return None;
        }
        Some(Self {
            source: pane as *const dyn Pane as *const () as usize,
            sequence: floor,
            geometry: (
                dims.cols,
                dims.viewport_rows,
                dims.dpi,
                dims.pixel_width,
                dims.pixel_height,
            ),
            alternate: false,
        })
    }

    pub fn capture(pane: &dyn Pane) -> Option<Self> {
        Self::capture_source(pane).map(|(authority, _, _)| authority)
    }

    pub(crate) fn layout_floor(self) -> SequenceNo {
        self.sequence
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
    pub button: window::MousePress,
    pub paint_retries_remaining: u8,
    pub released: bool,
    pub end: Option<(wezterm_term::input::ClickPosition, StableRowIndex)>,
    pub copy: Option<config::keyassignment::ClipboardCopyDestination>,
}

impl PendingSelectionStart {
    pub fn retain_endpoint(
        &mut self,
        frame: SelectionFrameStamp,
        position: wezterm_term::input::ClickPosition,
        row: StableRowIndex,
        release: Option<window::MousePress>,
    ) {
        let final_release = release == Some(self.button);
        if (!self.released || final_release) && frame.same_coordinates(self.frame) {
            self.end = Some((position, row));
        }
        if final_release {
            self.released = true;
        }
    }

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
    /// Return only the complete stamp promoted by this successful submission.
    /// An omitted/incomplete pane must not report its previously displayed stamp.
    pub fn presented(&mut self) -> Option<SelectionFrameStamp> {
        self.presented = self.pending.take();
        self.presented
    }
    pub fn for_mouse(&self, current: Option<SelectionFrameStamp>) -> Option<SelectionFrameStamp> {
        self.presented
            .filter(|presented| current.is_some_and(|current| presented.same_coordinates(current)))
    }
}

pub use config::keyassignment::SelectionMode;

impl Selection {
    pub(crate) fn native_points(
        &self,
    ) -> [Option<wezterm_term::screen::SelectionAnchorCoordinate>; 3] {
        [
            self.origin,
            self.range.map(|r| r.start),
            self.range.map(|r| r.end),
        ]
        .map(|point| {
            point.map(|point| wezterm_term::screen::SelectionAnchorCoordinate {
                row: point.y,
                column: match point.x {
                    SelectionX::Cell(column) => Some(column),
                    SelectionX::BeforeZero => None,
                },
            })
        })
    }

    pub(crate) fn remember_native_anchor(
        &mut self,
        token: wezterm_term::screen::ScreenSelectionAnchor,
    ) {
        self.native_anchor = Some(NativeSelectionAnchor {
            token,
            authority: self.authority,
            origin: self.origin,
            range: self.range,
        });
    }

    pub(crate) fn native_anchor(&self) -> Option<&wezterm_term::screen::ScreenSelectionAnchor> {
        self.native_anchor
            .as_ref()
            .filter(|anchor| {
                !self.rectangular
                    && anchor.authority == self.authority
                    && anchor.origin == self.origin
                    && anchor.range == self.range
            })
            .map(|anchor| &anchor.token)
    }

    /// Only the exact selection that captured a native token can consume its
    /// remap. Later mouse motion must not be overwritten by prepared work.
    pub(crate) fn rebase_native_anchor(
        &mut self,
        points: [Option<wezterm_term::screen::SelectionAnchorCoordinate>; 3],
        authority: SelectionAuthority,
        source_sequence: SequenceNo,
    ) -> bool {
        let Some(token) = self.native_anchor().cloned() else {
            return false;
        };
        if source_sequence == SequenceNo::MAX
            || self.origin.is_some() != points[0].is_some()
            || self.range.is_some() != points[1].is_some()
            || self.range.is_some() != points[2].is_some()
        {
            return false;
        }
        let [origin, start, end] = points.map(|point| {
            point.map(|point| SelectionCoordinate {
                x: point
                    .column
                    .map_or(SelectionX::BeforeZero, SelectionX::Cell),
                y: point.row,
            })
        });
        self.origin = origin;
        self.range = start
            .zip(end)
            .map(|(start, end)| SelectionRange { start, end });
        self.authority = Some(authority);
        self.seqno = source_sequence;
        self.remember_native_anchor(token);
        true
    }

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
        self.native_anchor = None;
    }

    pub fn begin(&mut self, origin: SelectionCoordinate) {
        self.native_anchor = None;
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

    /// Computes the selection range for the line from an already-acquired logical line snapshot.
    pub fn line_around_in_logical_line(
        start: SelectionCoordinate,
        logical: &mux::pane::LogicalLine,
    ) -> Self {
        if logical.contains_y(start.y) {
            let offset = (logical.physical_lines.len().saturating_sub(1)) as StableRowIndex;
            let end_row = logical.first_row.saturating_add(offset);
            Self {
                start: SelectionCoordinate::x_y(0, logical.first_row),
                end: SelectionCoordinate::x_y(usize::MAX, end_row),
            }
        } else {
            Self { start, end: start }
        }
    }

    /// Computes the selection range for the line from already-acquired logical line snapshots.
    pub fn line_around_in_logical_lines(
        start: SelectionCoordinate,
        lines: &[mux::pane::LogicalLine],
    ) -> Self {
        for logical in lines {
            if logical.contains_y(start.y) {
                return Self::line_around_in_logical_line(start, logical);
            }
        }
        // Shouldn't happen, but return a reasonable fallback
        Self { start, end: start }
    }

    /// Computes the selection range for the line around the specified coords
    pub fn line_around(start: SelectionCoordinate, pane: &dyn Pane) -> Self {
        let Some(end_y) = start.y.checked_add(1) else {
            return Self { start, end: start };
        };
        let lines = pane.get_logical_lines(start.y..end_y);
        Self::line_around_in_logical_lines(start, &lines)
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

    /// Computes the selection range for the word from an already-acquired logical line snapshot.
    pub fn word_around_in_logical_line(
        start: SelectionCoordinate,
        logical: &mux::pane::LogicalLine,
    ) -> Self {
        if !logical.contains_y(start.y) {
            return Self { start, end: start };
        }

        if let SelectionX::Cell(start_x) = start.x {
            let start_idx = logical.xy_to_logical_x(start_x, start.y);
            return match logical
                .logical
                .compute_double_click_range(start_idx, is_double_click_word)
            {
                DoubleClickRange::RangeWithWrap(click_range)
                | DoubleClickRange::Range(click_range) => {
                    Self::from_logical_click_range(start, logical, click_range)
                }
            };
        }

        Self { start, end: start }
    }

    /// Computes the selection range for the word from already-acquired logical line snapshots.
    pub fn word_around_in_logical_lines(
        start: SelectionCoordinate,
        lines: &[mux::pane::LogicalLine],
    ) -> Self {
        for logical in lines {
            if logical.contains_y(start.y) {
                return Self::word_around_in_logical_line(start, logical);
            }
        }

        // Shouldn't happen, but return a reasonable fallback
        Self { start, end: start }
    }

    /// Computes the selection range for the word around the specified coords
    pub fn word_around(start: SelectionCoordinate, pane: &dyn Pane) -> Self {
        let Some(end_y) = start.y.checked_add(1) else {
            return Self { start, end: start };
        };
        let lines = pane.get_logical_lines(start.y..end_y);
        Self::word_around_in_logical_lines(start, &lines)
    }

    /// Computes the smart-selection range for the specified coords from an
    /// already-acquired logical line snapshot, falling back to word selection
    /// on the same snapshot when no smart pattern matches.
    pub fn smart_or_word_around_in_logical_line(
        start: SelectionCoordinate,
        logical: &mux::pane::LogicalLine,
    ) -> (Self, Option<SmartSelectionPick>) {
        if !logical.contains_y(start.y) {
            return (Self { start, end: start }, None);
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

        (Self::word_around_in_logical_line(start, logical), None)
    }

    /// Computes the smart-selection range for the specified coords from
    /// already-acquired logical line snapshots, falling back to word selection
    /// on the same snapshot when no smart pattern matches.
    pub fn smart_or_word_around_in_logical_lines(
        start: SelectionCoordinate,
        lines: &[mux::pane::LogicalLine],
    ) -> (Self, Option<SmartSelectionPick>) {
        for logical in lines {
            if logical.contains_y(start.y) {
                return Self::smart_or_word_around_in_logical_line(start, logical);
            }
        }

        (Self { start, end: start }, None)
    }

    /// Computes the smart-selection range for the specified coords,
    /// falling back to the legacy word-boundary selection when the
    /// smart-selection catalog has no match at the click position.
    ///
    /// Reads logical line snapshots from the pane once, ensuring the
    /// same snapshot is used for both smart pattern evaluation and
    /// word-boundary fallback without duplicate I/O or race conditions.
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
        let Some(end_y) = start.y.checked_add(1) else {
            return (Self { start, end: start }, None);
        };
        let lines = pane.get_logical_lines(start.y..end_y);
        Self::smart_or_word_around_in_logical_lines(start, &lines)
    }

    /// Computes the smart-selection range for the line from an already-acquired
    /// logical line snapshot, falling back to line selection on the same
    /// snapshot when no smart pattern matches.
    pub fn smart_or_line_around_in_logical_line(
        start: SelectionCoordinate,
        logical: &mux::pane::LogicalLine,
    ) -> (Self, Option<SmartSelectionPick>) {
        if !logical.contains_y(start.y) {
            return (Self { start, end: start }, None);
        }

        let line_text = logical.logical.as_str();
        // Triple-click resolves to the widest smart-pattern fully
        // contained within the line. `smart_match_in_line` runs
        // find_all → drop_shell_quoted_supersets →
        // classify_triple_click in one call.
        if let Some(selection_match) = smart_match_in_line(&line_text, 0, line_text.len()) {
            let logical_start = logical_x_for_byte_offset(&line_text, selection_match.span_start);
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

        (Self::line_around_in_logical_line(start, logical), None)
    }

    /// Computes the smart-selection range for the line from already-acquired
    /// logical line snapshots, falling back to line selection on the same
    /// snapshot when no smart pattern matches.
    pub fn smart_or_line_around_in_logical_lines(
        start: SelectionCoordinate,
        lines: &[mux::pane::LogicalLine],
    ) -> (Self, Option<SmartSelectionPick>) {
        for logical in lines {
            if logical.contains_y(start.y) {
                return Self::smart_or_line_around_in_logical_line(start, logical);
            }
        }

        (Self { start, end: start }, None)
    }

    /// Computes the smart-selection range for the *line* around the
    /// specified coords (triple-click semantics), falling back to
    /// the legacy full-physical-line selection when the smart-
    /// selection catalog has no widest-pattern fully contained in
    /// the line.
    ///
    /// Reads logical line snapshots from the pane once, ensuring the
    /// same snapshot is used for both smart pattern evaluation and
    /// line fallback without duplicate I/O or race conditions.
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
        let Some(end_y) = start.y.checked_add(1) else {
            return (Self { start, end: start }, None);
        };
        let lines = pane.get_logical_lines(start.y..end_y);
        Self::smart_or_line_around_in_logical_lines(start, &lines)
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

    #[derive(Debug)]
    struct NativeAnchorTestConfig;

    impl wezterm_term::TerminalConfiguration for NativeAnchorTestConfig {
        fn color_palette(&self) -> wezterm_term::color::ColorPalette {
            Default::default()
        }
    }

    fn native_anchor_fixture() -> (wezterm_term::Terminal, Selection, String) {
        let size = wezterm_term::TerminalSize {
            rows: 24,
            cols: 80,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 384,
        };
        let mut term = wezterm_term::Terminal::new(
            size,
            std::sync::Arc::new(NativeAnchorTestConfig),
            "selection-test",
            "test",
            Box::new(Vec::<u8>::new()),
        );
        let text = "café e\u{301} 中文 🙂 end café e\u{301} 中文 🙂 end café e\u{301} 中文 🙂 end TAIL\nsecond line Ω";
        term.advance_bytes(
            format!(
                "FT_HELD_READY\r\nanchor row\r\n{}",
                text.replace('\n', "\r\n")
            )
            .as_bytes(),
        );
        let mut selection = Selection::default();
        selection.begin(SelectionCoordinate::x_y(0, 2));
        selection.range = Some(
            SelectionRange::start(SelectionCoordinate::x_y(0, 2))
                .extend(SelectionCoordinate::x_y(12, 3)),
        );
        selection.seqno = term.current_seqno();
        selection.authority = Some(SelectionAuthority {
            source: 1,
            sequence: selection.seqno,
            geometry: (80, 24, 96, 640, 384),
            alternate: false,
        });
        let token = term
            .screen_mut()
            .capture_selection_anchor(selection.seqno, selection.native_points())
            .unwrap();
        selection.remember_native_anchor(token);
        (term, selection, text.to_string())
    }

    fn live_native_selection_text(term: &wezterm_term::Terminal, selection: &Selection) -> String {
        let lines: Vec<_> = term
            .screen()
            .lines_in_phys_range(0..term.screen().scrollback_rows())
            .into_iter()
            .enumerate()
            .map(|(row, line)| mux::pane::LogicalLine {
                first_row: term.screen().phys_to_stable_row_index(row),
                logical: line.clone(),
                physical_lines: vec![line],
            })
            .collect();
        crate::termwindow::selected_text_from_logical_lines(&lines, selection.range.unwrap(), false)
    }

    #[test]
    fn native_selection_commit_retains_released_intent_through_capture_contention() {
        let (mut term, mut committed, _) = native_anchor_fixture();
        let previous = committed.clone();
        let mut desired = committed.clone();
        desired.range.as_mut().unwrap().end = SelectionCoordinate::x_y(5, 3);
        let mut pending = PendingNativeSelection::new(desired);
        // The LocalPane producer test holds the real terminal lock and checks
        // this typed result. Exercise its actual GUI mapping and transaction.
        let busy = || Err(mux::localpane::SelectionAnchorCaptureError::Busy).into();
        {
            assert_eq!(pending.try_commit(&mut committed, busy()), None);
            // Mouse release binds the copy to this endpoint, not `previous`.
            pending.copy = Some(config::keyassignment::ClipboardCopyDestination::Clipboard);
            for _ in 0..3 {
                assert!(pending.take_paint_retry());
                assert_eq!(pending.try_commit(&mut committed, busy()), None);
                assert_eq!(committed, previous);
            }
            assert!(!pending.take_paint_retry());
            assert!(pending.copy.is_some());
        }
        let captured = term
            .screen_mut()
            .capture_selection_anchor(pending.desired.seqno, pending.desired.native_points());
        assert!(captured.is_some());
        assert_eq!(
            pending.try_commit(&mut committed, Ok(captured).into()),
            Some(NativeSelectionCommit::Applied {
                needs_repaint: true
            })
        );
        assert_eq!(committed.range, pending.desired.range);
        assert!(committed.native_anchor().is_some());
        let captured_again = term
            .screen_mut()
            .capture_selection_anchor(pending.desired.seqno, pending.desired.native_points());
        assert_eq!(
            pending.try_commit(&mut committed, Ok(captured_again).into()),
            Some(NativeSelectionCommit::Applied {
                needs_repaint: false
            }),
            "copy retries with unchanged visible selection must not cause a paint loop"
        );
        let expected = live_native_selection_text(&term, &committed);
        assert_ne!(expected, live_native_selection_text(&term, &previous));
        let token = committed.native_anchor().unwrap().clone();
        term.resize(wezterm_term::TerminalSize {
            rows: 24,
            cols: 37,
            dpi: 96,
            pixel_width: 296,
            pixel_height: 384,
        });
        let sequence = term.current_seqno();
        let points = term
            .screen()
            .resolve_selection_anchor(&token, sequence)
            .unwrap();
        let mut authority = committed.authority.unwrap();
        authority.sequence = sequence;
        authority.geometry = (37, 24, 96, 296, 384);
        assert!(committed.rebase_native_anchor(points, authority, sequence));
        assert_eq!(live_native_selection_text(&term, &committed), expected);
    }

    #[test]
    fn native_selection_commit_rejects_obsolete_intent_without_replacing_anchor() {
        let (_, mut committed, _) = native_anchor_fixture();
        let previous = committed.clone();
        let mut desired = committed.clone();
        desired.range.as_mut().unwrap().end = SelectionCoordinate::x_y(1, 2);
        let pending = PendingNativeSelection::new(desired);
        assert_eq!(
            pending.try_commit(
                &mut committed,
                Err(mux::localpane::SelectionAnchorCaptureError::SourceChanged).into()
            ),
            Some(NativeSelectionCommit::Invalidated)
        );
        assert_eq!(committed, previous);
        assert!(committed.native_anchor().is_some());
    }

    #[test]
    fn native_selection_commit_preserves_current_unremappable_selection() {
        let (_, mut committed, _) = native_anchor_fixture();
        let mut desired = committed.clone();
        desired.range.as_mut().unwrap().end = SelectionCoordinate::x_y(1, 2);
        let pending = PendingNativeSelection::new(desired);
        assert_eq!(
            pending.try_commit(&mut committed, Ok(None).into()),
            Some(NativeSelectionCommit::Applied {
                needs_repaint: true
            })
        );
        assert_eq!(committed.range, pending.desired.range);
        assert!(committed.native_anchor().is_none());
        assert!(committed.is_authorized_by(pending.desired.authority));
        let mut resized = pending.desired.authority.unwrap();
        resized.geometry.0 += 1;
        assert!(committed.is_invalidated_by(Some(resized)));
    }

    #[test]
    fn native_selection_transports_real_parser_endpoints_and_reads_live_unicode_text() {
        let (mut term, mut selection, expected) = native_anchor_fixture();
        assert_eq!(live_native_selection_text(&term, &selection), expected);
        let token = selection.native_anchor().unwrap().clone();
        for cols in [40, 36, 44, 80] {
            let mut size = term.get_size();
            size.cols = cols;
            size.pixel_width = cols * 8;
            term.resize(size);
            let sequence = term.current_seqno();
            let points = term
                .screen()
                .resolve_selection_anchor(&token, sequence)
                .unwrap();
            let authority = SelectionAuthority {
                sequence,
                geometry: (cols, 24, 96, cols * 8, 384),
                ..selection.authority.unwrap()
            };
            assert!(
                selection.is_invalidated_by(Some(authority)),
                "unmapped physical points must remain unauthorized"
            );
            assert!(selection.rebase_native_anchor(points, authority, sequence));
            assert!(selection.is_authorized_by(Some(authority)));
            assert_eq!(live_native_selection_text(&term, &selection), expected);
            if cols == 40 {
                assert_eq!(
                    selection.range.unwrap().end,
                    SelectionCoordinate::x_y(12, 4)
                );
                assert_eq!(term.cursor_pos().x, 13);
                assert_eq!(term.cursor_pos().y, 4);
            }
        }
        // Reads still use the current cells: changing selected content cannot
        // be hidden by a frozen text facade, nor authorize a later transport.
        term.advance_bytes(b"\x1b[3;1HZ");
        assert!(live_native_selection_text(&term, &selection).starts_with("Zafé"));
        let mut size = term.get_size();
        size.cols = 40;
        term.resize(size);
        assert!(
            term.screen()
                .resolve_selection_anchor(&token, term.current_seqno())
                .is_none()
        );
    }

    #[test]
    fn native_selection_transport_rejects_replaced_gesture_and_cleared_selection() {
        let (mut term, mut selection, _) = native_anchor_fixture();
        let token = selection.native_anchor().unwrap().clone();
        let mut size = term.get_size();
        size.cols = 40;
        term.resize(size);
        let sequence = term.current_seqno();
        let points = term
            .screen()
            .resolve_selection_anchor(&token, sequence)
            .unwrap();
        let authority = SelectionAuthority {
            sequence,
            geometry: (40, 24, 96, 640, 384),
            ..selection.authority.unwrap()
        };
        selection.range.as_mut().unwrap().end.x = SelectionX::Cell(5);
        let new_range = selection.range;
        assert!(!selection.rebase_native_anchor(points, authority, sequence));
        assert_eq!(selection.range, new_range);
        selection.clear();
        assert!(!selection.rebase_native_anchor(points, authority, sequence));
        assert!(selection.origin.is_none() && selection.range.is_none());
    }

    #[test]
    fn native_selection_before_zero_preserves_text_for_both_endpoint_roles_and_directions() {
        for starts_before in [false, true] {
            for reverse in [false, true] {
                let (mut term, mut selection, _) = native_anchor_fixture();
                let mut size = term.get_size();
                size.cols = 40;
                term.resize(size);
                let before = SelectionCoordinate {
                    x: SelectionX::BeforeZero,
                    y: 3,
                };
                let (start, end) = if starts_before {
                    (before, SelectionCoordinate::x_y(12, 4))
                } else {
                    (SelectionCoordinate::x_y(0, 2), before)
                };
                selection.origin = Some(SelectionCoordinate::x_y(0, 2));
                selection.range = Some(if reverse {
                    SelectionRange {
                        start: end,
                        end: start,
                    }
                } else {
                    SelectionRange { start, end }
                });
                selection.seqno = term.current_seqno();
                selection.authority = Some(SelectionAuthority {
                    sequence: selection.seqno,
                    geometry: (40, 24, 96, 640, 384),
                    ..selection.authority.unwrap()
                });
                let token = term
                    .screen_mut()
                    .capture_selection_anchor(selection.seqno, selection.native_points())
                    .unwrap();
                selection.remember_native_anchor(token.clone());
                let expected = live_native_selection_text(&term, &selection);
                assert!(!expected.is_empty());
                if !starts_before {
                    let row = term.screen().lines_in_phys_range(2..3).remove(0);
                    assert_eq!(row.len(), 38);
                    assert_eq!(
                        row.as_str(),
                        "café e\u{301} 中文 🙂 end café e\u{301} 中文 🙂 end "
                    );
                    assert!(row.last_cell_was_wrapped());
                    // Endpoint extraction trims trailing blanks regardless of
                    // whether the endpoint lies at a physical soft wrap.
                    assert_eq!(
                        expected,
                        "café e\u{301} 中文 🙂 end café e\u{301} 中文 🙂 end"
                    );
                }
                size.cols = 80;
                term.resize(size);
                let sequence = term.current_seqno();
                let points = term
                    .screen()
                    .resolve_selection_anchor(&token, sequence)
                    .unwrap();
                let authority = SelectionAuthority {
                    sequence,
                    geometry: (80, 24, 96, 640, 384),
                    ..selection.authority.unwrap()
                };
                assert!(selection.rebase_native_anchor(points, authority, sequence));
                if !starts_before {
                    let endpoint = if reverse {
                        selection.range.unwrap().start
                    } else {
                        selection.range.unwrap().end
                    };
                    assert_eq!(endpoint, SelectionCoordinate::x_y(37, 2));
                }
                assert_eq!(
                    live_native_selection_text(&term, &selection),
                    expected,
                    "starts_before={starts_before}, reverse={reverse}"
                );
            }
        }
    }

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
        assert_eq!(state.presented(), Some(a));
        assert_eq!(state.for_mouse(Some(a)), Some(a));
        state.begin_attempt();
        state.stage(Some(b), Some(b), true);
        // Failed swap leaves A displayed; damage/query B cannot authorize B.
        assert_eq!(state.for_mouse(Some(b)), None);
        assert_eq!(state.for_mouse(Some(a)), Some(a));
        state.begin_attempt(); // atlas retry must discard the B candidate
        state.stage(Some(b), Some(b), false); // incomplete cold rows
        assert_eq!(state.presented(), None);
        assert_eq!(state.for_mouse(Some(b)), None);
        state.begin_attempt();
        state.stage(Some(a), Some(b), true); // layout changed during snapshot
        assert_eq!(state.presented(), None);
        assert_eq!(state.for_mouse(Some(b)), None);
        state.begin_attempt();
        state.stage(Some(b), Some(b), true);
        assert_eq!(state.presented(), Some(b));
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
        assert_eq!(state.presented(), Some(b));
        assert_eq!(state.for_mouse(Some(output)), Some(b));
        state.begin_attempt(); // pane omitted by successful next frame
        assert_eq!(state.presented(), None);
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
        assert_eq!(state.presented(), Some(frame));

        // Unlike an established drag, a busy press must retain its ORIGINAL
        // click until a usable frame is available, regardless of later motion.
        let pending = PendingSelectionStart {
            frame,
            coordinate: anchor,
            mode: SelectionMode::Cell,
            button: window::MousePress::Left,
            paint_retries_remaining: 3,
            released: false,
            end: None,
            copy: None,
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
        assert_eq!(unpresented.presented(), Some(frame));
        assert!(matches!(
            pending.resolve(Some(frame), &unpresented),
            PendingSelectionResolution::Ready
        ));
        assert_eq!(pending.coordinate, anchor);
        assert_eq!(pending.mode, SelectionMode::Cell);
        // No motion event arrived while the source was busy. The release
        // position itself must become the final endpoint, even if release was
        // marked before coordinate conversion in the native event handler.
        let mut released = pending;
        released.released = true;
        released.retain_endpoint(
            frame,
            wezterm_term::input::ClickPosition {
                column: 9,
                row: 2,
                x_pixel_offset: 0,
                y_pixel_offset: 0,
            },
            anchor.y + 2,
            Some(window::MousePress::Left),
        );
        assert_eq!(released.end.unwrap().0.column, 9);
        assert_eq!(released.end.unwrap().1, anchor.y + 2);
        assert_eq!(released.coordinate, anchor);
        released.retain_endpoint(
            frame,
            wezterm_term::input::ClickPosition {
                column: 18,
                row: 4,
                x_pixel_offset: 0,
                y_pixel_offset: 0,
            },
            anchor.y + 4,
            None,
        );
        assert_eq!(
            released.end.unwrap().0.column,
            9,
            "later motion cannot alter a released gesture"
        );
        assert!(matches!(
            released.resolve(None, &state),
            PendingSelectionResolution::Wait
        ));
        assert!(matches!(
            released.resolve(Some(frame), &state),
            PendingSelectionResolution::Ready
        ));
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
        assert_eq!(state.presented(), Some(scrolled));
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

    #[test]
    fn smart_or_word_around_wrapped_literal_unicode_smart_match() {
        // Wrapped logical line with a URL containing literal Unicode (Japanese chars)
        // Physical row 10: "visit https://examp" (19 display cols)
        // Physical row 11: "le.com/日本語 path" (17 display cols: 7 ASCII + 6 CJK wide + 4 ASCII)
        // Combined logical line: "visit https://example.com/日本語 path"
        let p0: termwiz::surface::line::Line = "visit https://examp".into();
        let p1: termwiz::surface::line::Line = "le.com/日本語 path".into();
        let full: termwiz::surface::line::Line = "visit https://example.com/日本語 path".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0, p1],
            logical: full,
            first_row: 10,
        };

        // Click on physical row 10, column 8 (within "https://examp")
        let start = SelectionCoordinate::x_y(8, 10);
        let (range, pick) =
            SelectionRange::smart_or_word_around_in_logical_lines(start, &[logical.clone()]);

        let pick = pick.expect("URL match with literal Unicode should succeed");
        assert_eq!(pick.kind, SelectionPatternKind::Url);
        assert_eq!(pick.text, "https://example.com/日本語");

        // "visit " is 6 cols, URL starts at col 6 on row 10
        assert_eq!(range.start, SelectionCoordinate::x_y(6, 10));
        // URL ends at logical x 32 (exclusive). Inclusive end is 31.
        // Physical line 0 len is 19 cols. 31 - 19 = 12 on row 11 (second cell of '語').
        assert_eq!(range.end, SelectionCoordinate::x_y(12, 11));

        // Click directly on row 11 on the wide Unicode character '本'
        // 'le.com/' is 7 cols, '日' is 2 cols (cols 7..9), '本' is cols 9..11
        let start_unicode = SelectionCoordinate::x_y(9, 11);
        let (range_u, pick_u) =
            SelectionRange::smart_or_word_around_in_logical_lines(start_unicode, &[logical]);
        let pick_u = pick_u.expect("Click on wide Unicode inside URL should match");
        assert_eq!(pick_u.text, "https://example.com/日本語");
        assert_eq!(range_u, range);
    }

    #[test]
    fn smart_or_word_around_wrapped_literal_unicode_fallback() {
        // Plain wrapped line with CJK wide characters and no smart pattern.
        // Verifies that fallback uses the same snapshot and respects word boundary across wrap.
        // Row 20: "prefix 日本" (7 ASCII cols + 4 CJK cols = 11 cols)
        // Row 21: "語word suffix" (2 CJK cols + 4 ASCII cols + 7 ASCII cols = 13 cols)
        // Combined logical line: "prefix 日本語word suffix"
        let p0: termwiz::surface::line::Line = "prefix 日本".into();
        let p1: termwiz::surface::line::Line = "語word suffix".into();
        let full: termwiz::surface::line::Line = "prefix 日本語word suffix".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0, p1],
            logical: full,
            first_row: 20,
        };

        // Click on row 21, column 0 ('語')
        let start = SelectionCoordinate::x_y(0, 21);
        let (range, pick) =
            SelectionRange::smart_or_word_around_in_logical_lines(start, &[logical.clone()]);

        // Fallback should fire without smart pick
        assert_eq!(pick, None);

        // Word "日本語word" starts at logical x 7 (col 7 on row 20)
        assert_eq!(range.start, SelectionCoordinate::x_y(7, 20));
        // Word is 6 CJK cols + 4 ASCII cols = 10 cols.
        // In row 20: 4 cols ('日本'). Remaining 6 cols in row 21: '語' (2) + 'word' (4) = 6 cols (cols 0..6, end-1 is col 5).
        assert_eq!(range.end, SelectionCoordinate::x_y(5, 21));

        // Direct word_around_in_logical_lines produces the exact same range
        let direct_range = SelectionRange::word_around_in_logical_lines(start, &[logical]);
        assert_eq!(direct_range, range);
    }

    #[test]
    fn smart_or_word_around_before_zero_and_boundary_cases() {
        let p0: termwiz::surface::line::Line = "hello world".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0],
            logical: "hello world".into(),
            first_row: 5,
        };

        // BeforeZero coordinate: must preserve start == end and return None
        let bz = SelectionCoordinate {
            x: SelectionX::BeforeZero,
            y: 5,
        };
        let (range_bz, pick_bz) =
            SelectionRange::smart_or_word_around_in_logical_lines(bz, &[logical.clone()]);
        assert_eq!(pick_bz, None);
        assert_eq!(range_bz, SelectionRange { start: bz, end: bz });

        // Past-end coordinate (e.g. col 999): must preserve start == end and return None
        let past_end = SelectionCoordinate::x_y(999, 5);
        let (range_pe, pick_pe) =
            SelectionRange::smart_or_word_around_in_logical_lines(past_end, &[logical.clone()]);
        assert_eq!(pick_pe, None);
        assert_eq!(
            range_pe,
            SelectionRange {
                start: past_end,
                end: past_end,
            }
        );

        // Non-matching row coordinate (row 99): must return fallback start == end
        let wrong_row = SelectionCoordinate::x_y(2, 99);
        let (range_wr, pick_wr) =
            SelectionRange::smart_or_word_around_in_logical_lines(wrong_row, &[logical]);
        assert_eq!(pick_wr, None);
        assert_eq!(
            range_wr,
            SelectionRange {
                start: wrong_row,
                end: wrong_row,
            }
        );
    }

    #[test]
    fn smart_or_line_around_wrapped_smart_match() {
        // Wrapped line containing an email address across the wrap
        // Row 30: "please write to " (16 cols)
        // Row 31: "support@example.com for help" (28 cols)
        let p0: termwiz::surface::line::Line = "please write to ".into();
        let p1: termwiz::surface::line::Line = "support@example.com for help".into();
        let full: termwiz::surface::line::Line =
            "please write to support@example.com for help".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0, p1],
            logical: full,
            first_row: 30,
        };

        let start = SelectionCoordinate::x_y(0, 30);
        let (range, pick) =
            SelectionRange::smart_or_line_around_in_logical_lines(start, &[logical]);

        let pick = pick.expect("Email smart pattern in line should be picked");
        assert_eq!(pick.kind, SelectionPatternKind::Email);
        assert_eq!(pick.text, "support@example.com");

        // Email starts at logical col 16 -> on row 31, col 0
        assert_eq!(range.start, SelectionCoordinate::x_y(0, 31));
        // Email length 19 -> on row 31, col 18 (inclusive end)
        assert_eq!(range.end, SelectionCoordinate::x_y(18, 31));
    }

    #[test]
    fn smart_or_line_around_wrapped_plain_fallback() {
        // Plain wrapped line with no smart pattern
        // Row 40: "first physical line of text "
        // Row 41: "second physical line of text"
        let p0: termwiz::surface::line::Line = "first physical line of text ".into();
        let p1: termwiz::surface::line::Line = "second physical line of text".into();
        let full: termwiz::surface::line::Line =
            "first physical line of text second physical line of text".into();
        let logical = mux::pane::LogicalLine {
            physical_lines: vec![p0, p1],
            logical: full,
            first_row: 40,
        };

        // Click within physical row 41
        let start = SelectionCoordinate::x_y(5, 41);
        let (range, pick) =
            SelectionRange::smart_or_line_around_in_logical_lines(start, &[logical.clone()]);

        assert_eq!(pick, None);
        // Line selection spans row 40 col 0 to row 41 col usize::MAX
        assert_eq!(range.start, SelectionCoordinate::x_y(0, 40));
        assert_eq!(range.end, SelectionCoordinate::x_y(usize::MAX, 41));

        // Direct line_around_in_logical_lines produces the exact same range
        let direct = SelectionRange::line_around_in_logical_lines(start, &[logical.clone()]);
        assert_eq!(direct, range);

        // BeforeZero coordinate also selects the entire physical line span
        let bz = SelectionCoordinate {
            x: SelectionX::BeforeZero,
            y: 40,
        };
        let (range_bz, pick_bz) =
            SelectionRange::smart_or_line_around_in_logical_lines(bz, &[logical.clone()]);
        assert_eq!(pick_bz, None);
        assert_eq!(range_bz.start, SelectionCoordinate::x_y(0, 40));
        assert_eq!(range_bz.end, SelectionCoordinate::x_y(usize::MAX, 41));

        // Non-matching row falls back to start == end
        let wrong_row = SelectionCoordinate::x_y(0, 99);
        let (range_wr, pick_wr) =
            SelectionRange::smart_or_line_around_in_logical_lines(wrong_row, &[logical]);
        assert_eq!(pick_wr, None);
        assert_eq!(
            range_wr,
            SelectionRange {
                start: wrong_row,
                end: wrong_row,
            }
        );
    }

    #[test]
    fn extreme_stable_row_index_checked_add_boundary() {
        let _mux = if mux::Mux::try_get().is_none() {
            let m = std::sync::Arc::new(mux::Mux::new(None));
            mux::Mux::set_mux(&m);
            Some(m)
        } else {
            None
        };
        let size = wezterm_term::TerminalSize {
            rows: 24,
            cols: 80,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 384,
        };
        let (_tw, pane) =
            mux::termwiztermtab::allocate(size, std::sync::Arc::new(NativeAnchorTestConfig))
                .unwrap();

        // 1. Literal boundary negative: StableRowIndex::MAX.
        // start.y + 1 cannot be represented in StableRowIndex (isize);
        // checked_add returns None and all methods must return the fallback range
        // without panicking or attempting an invalid range query.
        let max_coord = SelectionCoordinate::x_y(0, StableRowIndex::MAX);
        let expected_fallback = SelectionRange {
            start: max_coord,
            end: max_coord,
        };

        assert_eq!(
            SelectionRange::line_around(max_coord, pane.as_ref()),
            expected_fallback
        );
        assert_eq!(
            SelectionRange::word_around(max_coord, pane.as_ref()),
            expected_fallback
        );
        assert_eq!(
            SelectionRange::smart_or_word_around(max_coord, pane.as_ref()),
            (expected_fallback, None)
        );
        assert_eq!(
            SelectionRange::smart_or_line_around(max_coord, pane.as_ref()),
            (expected_fallback, None)
        );

        // BeforeZero coordinate at StableRowIndex::MAX
        let max_bz = SelectionCoordinate {
            x: SelectionX::BeforeZero,
            y: StableRowIndex::MAX,
        };
        let expected_bz_fallback = SelectionRange {
            start: max_bz,
            end: max_bz,
        };
        assert_eq!(
            SelectionRange::line_around(max_bz, pane.as_ref()),
            expected_bz_fallback
        );
        assert_eq!(
            SelectionRange::word_around(max_bz, pane.as_ref()),
            expected_bz_fallback
        );
        assert_eq!(
            SelectionRange::smart_or_word_around(max_bz, pane.as_ref()),
            (expected_bz_fallback, None)
        );
        assert_eq!(
            SelectionRange::smart_or_line_around(max_bz, pane.as_ref()),
            (expected_bz_fallback, None)
        );

        // 2. Adjacent valid row positive: StableRowIndex::MAX - 1.
        // start.y.checked_add(1) evaluates to Some(StableRowIndex::MAX),
        // querying pane.get_logical_lines((MAX - 1)..MAX) cleanly without
        // integer overflow. Since no lines exist at this offset, all methods
        // return the fallback range.
        let adj_coord = SelectionCoordinate::x_y(0, StableRowIndex::MAX - 1);
        let expected_adj_fallback = SelectionRange {
            start: adj_coord,
            end: adj_coord,
        };

        assert_eq!(
            SelectionRange::line_around(adj_coord, pane.as_ref()),
            expected_adj_fallback
        );
        assert_eq!(
            SelectionRange::word_around(adj_coord, pane.as_ref()),
            expected_adj_fallback
        );
        assert_eq!(
            SelectionRange::smart_or_word_around(adj_coord, pane.as_ref()),
            (expected_adj_fallback, None)
        );
        assert_eq!(
            SelectionRange::smart_or_line_around(adj_coord, pane.as_ref()),
            (expected_adj_fallback, None)
        );

        // 3. Valid resident row positive: row 0 exists in the pane.
        // Confirms that non-extreme coordinates query the pane and produce
        // active selections.
        let valid_coord = SelectionCoordinate::x_y(0, 0);
        let valid_line = SelectionRange::line_around(valid_coord, pane.as_ref());
        assert_eq!(valid_line.start, SelectionCoordinate::x_y(0, 0));
        assert_eq!(valid_line.end, SelectionCoordinate::x_y(usize::MAX, 0));
    }
}
