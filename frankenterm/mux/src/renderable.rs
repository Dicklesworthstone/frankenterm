use crate::pane::{ForEachPaneLogicalLine, WithPaneLines};
use frankenterm_dynamic::{FromDynamic, ToDynamic};
use frankenterm_term::{Line, StableRowIndex, Terminal};
#[cfg(feature = "lua")]
use luahelper::impl_lua_conversion_dynamic;
use rangeset::RangeSet;
use serde::{Deserialize, Serialize};
use std::ops::Range;
use termwiz::surface::SequenceNo;

/// Describes the location of the cursor
#[derive(
    Debug, Default, Copy, Clone, Hash, Eq, PartialEq, Deserialize, Serialize, FromDynamic, ToDynamic,
)]
pub struct StableCursorPosition {
    pub x: usize,
    pub y: StableRowIndex,
    pub shape: termwiz::surface::CursorShape,
    pub visibility: termwiz::surface::CursorVisibility,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(StableCursorPosition);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, FromDynamic, ToDynamic,
)]
pub struct RenderableDimensions {
    /// The viewport width
    pub cols: usize,
    /// How many rows fit in the viewport
    pub viewport_rows: usize,
    /// The total number of lines in the scrollback, including the viewport
    pub scrollback_rows: usize,

    /// The top of the physical, non-scrollback, screen expressed
    /// as a stable index.  It is envisioned that this will be used
    /// to compute row/cols for mouse events and to produce a range
    /// for the `get_lines` call when the scroll position is at the
    /// bottom of the screen.
    pub physical_top: StableRowIndex,
    /// The top of the scrollback (the earliest row we remember)
    /// expressed as a stable index.
    pub scrollback_top: StableRowIndex,
    pub dpi: u32,
    pub pixel_width: usize,
    pub pixel_height: usize,
    /// True if the lines should be rendered reversed
    pub reverse_video: bool,
}
#[cfg(feature = "lua")]
impl_lua_conversion_dynamic!(RenderableDimensions);

/// Implements Pane::get_cursor_position for Terminal
pub fn terminal_get_cursor_position(term: &mut Terminal) -> StableCursorPosition {
    let pos = term.cursor_pos();

    StableCursorPosition {
        x: pos.x,
        y: term.screen().visible_row_to_stable_row(pos.y),
        shape: pos.shape,
        visibility: pos.visibility,
    }
}

/// Implements Pane::get_dirty_lines for Terminal
pub fn terminal_get_dirty_lines(
    term: &mut Terminal,
    lines: Range<StableRowIndex>,
    seqno: SequenceNo,
) -> RangeSet<StableRowIndex> {
    let screen = term.screen();
    let lines = screen.get_changed_stable_rows(lines, seqno);
    let mut set = RangeSet::new();
    for line in lines {
        set.add(line);
    }
    set
}

pub fn terminal_for_each_logical_line_in_stable_range_mut(
    term: &mut Terminal,
    lines: Range<StableRowIndex>,
    for_line: &mut dyn ForEachPaneLogicalLine,
) {
    let screen = term.screen_mut();
    screen.for_each_logical_line_in_stable_range_mut(lines, |stable_range, lines| {
        for_line.with_logical_line_mut(stable_range, lines)
    });
}

/// Implements Pane::with_lines for Terminal
pub fn terminal_with_lines<F>(term: &mut Terminal, lines: Range<StableRowIndex>, mut func: F)
where
    F: FnMut(StableRowIndex, &[&Line]),
{
    let screen = term.screen_mut();
    let phys_range = screen.stable_range(&lines);
    let first = screen.phys_to_stable_row_index(phys_range.start);

    screen.with_phys_lines(phys_range, |lines| func(first, lines));
}

/// Implements Pane::with_lines_mut for Terminal
pub fn terminal_with_lines_mut(
    term: &mut Terminal,
    lines: Range<StableRowIndex>,
    with_lines: &mut dyn WithPaneLines,
) {
    let screen = term.screen_mut();
    let phys_range = screen.stable_range(&lines);
    let first = screen.phys_to_stable_row_index(phys_range.start);

    screen.with_phys_lines_mut(phys_range, |lines| with_lines.with_lines_mut(first, lines));
}

/// Implements Pane::get_lines for Terminal
pub fn terminal_get_lines(
    term: &mut Terminal,
    lines: Range<StableRowIndex>,
) -> (StableRowIndex, Vec<Line>) {
    let screen = term.screen_mut();
    let phys_range = screen.stable_range(&lines);

    let first = screen.phys_to_stable_row_index(phys_range.start);
    let lines = screen.lines_in_phys_range(phys_range);

    (first, lines)
}

/// Implements Pane::get_dimensions for Terminal
pub fn terminal_get_dimensions(term: &mut Terminal) -> RenderableDimensions {
    let size = term.get_size();
    let screen = term.screen();
    RenderableDimensions {
        cols: screen.physical_cols,
        viewport_rows: screen.physical_rows,
        scrollback_rows: screen.scrollback_rows(),
        physical_top: screen.visible_row_to_stable_row(0),
        scrollback_top: screen.phys_to_stable_row_index(0),
        dpi: screen.dpi,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
        reverse_video: term.get_reverse_video(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_cursor_position_default() {
        let pos = StableCursorPosition::default();
        assert_eq!(pos.x, 0);
        assert_eq!(pos.y, 0);
    }

    #[test]
    fn stable_cursor_position_equality() {
        let a = StableCursorPosition::default();
        let b = StableCursorPosition::default();
        assert_eq!(a, b);
    }

    #[test]
    fn stable_cursor_position_inequality() {
        let a = StableCursorPosition::default();
        let b = StableCursorPosition {
            x: 5,
            ..Default::default()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn stable_cursor_position_clone_copy() {
        let a = StableCursorPosition {
            x: 10,
            y: 20,
            ..Default::default()
        };
        let b = a; // Copy
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn stable_cursor_position_debug() {
        let pos = StableCursorPosition {
            x: 5,
            y: 10,
            ..Default::default()
        };
        let dbg = format!("{:?}", pos);
        assert!(dbg.contains("StableCursorPosition"));
        assert!(dbg.contains("5"));
        assert!(dbg.contains("10"));
    }

    #[test]
    fn stable_cursor_position_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(StableCursorPosition::default());
        set.insert(StableCursorPosition {
            x: 1,
            ..Default::default()
        });
        set.insert(StableCursorPosition::default()); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn renderable_dimensions_default() {
        let dims = RenderableDimensions::default();
        assert_eq!(dims.cols, 0);
        assert_eq!(dims.viewport_rows, 0);
        assert_eq!(dims.scrollback_rows, 0);
        assert_eq!(dims.physical_top, 0);
        assert_eq!(dims.scrollback_top, 0);
        assert_eq!(dims.dpi, 0);
        assert!(!dims.reverse_video);
    }

    #[test]
    fn renderable_dimensions_equality() {
        let a = RenderableDimensions::default();
        let b = RenderableDimensions::default();
        assert_eq!(a, b);
    }

    #[test]
    fn renderable_dimensions_inequality() {
        let a = RenderableDimensions::default();
        let b = RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            ..Default::default()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn renderable_dimensions_clone_copy() {
        let a = RenderableDimensions {
            cols: 120,
            viewport_rows: 40,
            scrollback_rows: 10000,
            physical_top: 9960,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 960,
            pixel_height: 640,
            reverse_video: false,
        };
        let b = a; // Copy
        let c = a.clone(); // Clone
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn renderable_dimensions_debug() {
        let dims = RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            ..Default::default()
        };
        let dbg = format!("{:?}", dims);
        assert!(dbg.contains("RenderableDimensions"));
        assert!(dbg.contains("80"));
        assert!(dbg.contains("24"));
    }

    #[test]
    fn renderable_dimensions_with_reverse_video() {
        let dims = RenderableDimensions {
            reverse_video: true,
            ..Default::default()
        };
        assert!(dims.reverse_video);
        assert_ne!(dims, RenderableDimensions::default());
    }
}
