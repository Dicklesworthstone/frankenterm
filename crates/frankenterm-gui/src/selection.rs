// The range_plus_one lint can't see when the LHS is not compatible with
// and inclusive range
#![allow(clippy::range_plus_one)]
use frankenterm_core::smart_selection_patterns::smart_match_at_click;
use mux::pane::Pane;
use std::cmp::Ordering;
use std::ops::Range;
use termwiz::surface::line::DoubleClickRange;
use termwiz::surface::SequenceNo;
use wezterm_term::unicode_column_width;
use wezterm_term::{SemanticZone, StableRowIndex};

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct Selection {
    /// Remembers the starting coordinate of the selection prior to
    /// dragging.
    pub origin: Option<SelectionCoordinate>,
    /// Holds the not-normalized selection range.
    pub range: Option<SelectionRange>,
    /// When the selection was made wrt. the pane content
    pub seqno: SequenceNo,
    /// Whether the selection is rectangular
    pub rectangular: bool,
}

pub use config::keyassignment::SelectionMode;

impl Selection {
    pub fn clear(&mut self) {
        self.range = None;
        self.origin = None;
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

fn smart_match_logical_x_range(text: &str, click_logical_x: usize) -> Option<Range<usize>> {
    let click_byte_offset = byte_offset_for_logical_x(text, click_logical_x);
    let smart_match = smart_match_at_click(text, click_byte_offset)?;
    let start = logical_x_for_byte_offset(text, smart_match.span_start);
    let end_exclusive = logical_x_for_byte_offset(text, smart_match.span_end);
    if start >= end_exclusive {
        return None;
    }
    Some(start..end_exclusive)
}

impl SelectionRange {
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
                        usize::max_value(),
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
                        let (start_y, start_x) =
                            logical.logical_x_to_physical_coord(click_range.start);
                        let (end_y, end_x) =
                            logical.logical_x_to_physical_coord(click_range.end - 1);
                        Self {
                            start: SelectionCoordinate::x_y(start_x, start_y),
                            end: SelectionCoordinate::x_y(end_x, end_y),
                        }
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
    pub fn smart_or_word_around(start: SelectionCoordinate, pane: &dyn Pane) -> Self {
        for logical in pane.get_logical_lines(start.y..start.y + 1) {
            if !logical.contains_y(start.y) {
                continue;
            }

            if let SelectionX::Cell(start_x) = start.x {
                let click_logical_x = logical.xy_to_logical_x(start_x, start.y);
                let line_text = logical.logical.as_str();
                if let Some(click_range) = smart_match_logical_x_range(&line_text, click_logical_x)
                {
                    let (start_y, start_x) = logical.logical_x_to_physical_coord(click_range.start);
                    let (end_y, end_x) =
                        logical.logical_x_to_physical_coord(click_range.end.saturating_sub(1));
                    return Self {
                        start: SelectionCoordinate::x_y(start_x, start_y),
                        end: SelectionCoordinate::x_y(end_x, end_y),
                    };
                }
            }
        }

        Self::word_around(start, pane)
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
    /// Not that the range may include usize::max_value() for some rows; this
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
                norm.start.x.range(SelectionX::Cell(usize::max_value()))
            } else {
                // some "middle" line of multi-line
                0..usize::max_value()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_match_logical_x_range_selects_url_inside_quotes() {
        let text = "open \"https://example.com/foo?bar=1\" now";
        let click_x = text.find("example").unwrap();

        let range = smart_match_logical_x_range(text, click_x).expect("URL match");

        assert_eq!(&text[range], "https://example.com/foo?bar=1");
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
}
