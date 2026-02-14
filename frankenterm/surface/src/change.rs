use crate::{CursorShape, CursorVisibility, Position};
#[cfg(feature = "use_image")]
use alloc::sync::Arc;
use finl_unicode::grapheme_clusters::Graphemes;
use frankenterm_cell::color::ColorAttribute;
#[cfg(feature = "use_image")]
pub use frankenterm_cell::image::{ImageData, TextureCoordinate};
use frankenterm_cell::{unicode_column_width, AttributeChange, CellAttributes};
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};

extern crate alloc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum LineAttribute {
    DoubleHeightTopHalfLine,
    DoubleHeightBottomHalfLine,
    DoubleWidthLine,
    SingleWidthLine,
}

/// `Change` describes an update operation to be applied to a `Surface`.
/// Changes to the active attributes (color, style), moving the cursor
/// and outputting text are examples of some of the values.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Change {
    /// Change a single attribute
    Attribute(AttributeChange),
    /// Change all possible attributes to the given set of values
    AllAttributes(CellAttributes),
    /// Add printable text.
    /// Control characters are rendered inert by transforming them
    /// to space.  CR and LF characters are interpreted by moving
    /// the cursor position.  CR moves the cursor to the start of
    /// the line and LF moves the cursor down to the next line.
    /// You typically want to use both together when sending in
    /// a line break.
    Text(String),
    /// Clear the screen to the specified color.
    /// Implicitly clears all attributes prior to clearing the screen.
    /// Moves the cursor to the home position (top left).
    ClearScreen(ColorAttribute),
    /// Clear from the current cursor X position to the rightmost
    /// edge of the screen.  The background color is set to the
    /// provided color.  The cursor position remains unchanged.
    ClearToEndOfLine(ColorAttribute),
    /// Clear from the current cursor X position to the rightmost
    /// edge of the screen on the current line.  Clear all of the
    /// lines below the current cursor Y position.  The background
    /// color is set ot the provided color.  The cursor position
    /// remains unchanged.
    ClearToEndOfScreen(ColorAttribute),
    /// Move the cursor to the specified `Position`.
    CursorPosition { x: Position, y: Position },
    /// Change the cursor color.
    CursorColor(ColorAttribute),
    /// Change the cursor shape
    CursorShape(CursorShape),
    /// Change the cursor visibility
    CursorVisibility(CursorVisibility),
    /// Place an image at the current cursor position.
    /// The image defines the dimensions in cells.
    /// TODO: check iterm rendering behavior when the image is larger than the width of the screen.
    /// If the image is taller than the remaining space at the bottom
    /// of the screen, the screen will scroll up.
    /// The cursor Y position is unchanged by rendering the Image.
    /// The cursor X position will be incremented by `Image::width` cells.
    #[cfg(feature = "use_image")]
    Image(Image),
    /// Scroll the `region_size` lines starting at `first_row` upwards
    /// by `scroll_count` lines.  The `scroll_count` lines at the top of
    /// the region are overwritten.  The `scroll_count` lines at the
    /// bottom of the region will become blank.
    ///
    /// After a region is scrolled, the cursor position is undefined,
    /// and the terminal's scroll region is set to the range specified.
    /// To restore scrolling behaviour to the full terminal window, an
    /// additional `Change::ScrollRegionUp { first_row: 0, region_size:
    /// height, scroll_count: 0 }`, where `height` is the height of the
    /// terminal, should be emitted.
    ScrollRegionUp {
        first_row: usize,
        region_size: usize,
        scroll_count: usize,
    },
    /// Scroll the `region_size` lines starting at `first_row` downwards
    /// by `scroll_count` lines.  The `scroll_count` lines at the bottom
    /// the region are overwritten.  The `scroll_count` lines at the top
    /// of the region will become blank.
    ///
    /// After a region is scrolled, the cursor position is undefined,
    /// and the terminal's scroll region is set to the range specified.
    /// To restore scrolling behaviour to the full terminal window, an
    /// additional `Change::ScrollRegionDown { first_row: 0,
    /// region_size: height, scroll_count: 0 }`, where `height` is the
    /// height of the terminal, should be emitted.
    ScrollRegionDown {
        first_row: usize,
        region_size: usize,
        scroll_count: usize,
    },
    /// Change the title of the window in which the surface will be
    /// rendered.
    Title(String),

    /// Adjust the current line attributes, such as double height or width
    LineAttribute(LineAttribute),
}

impl Change {
    pub fn is_text(&self) -> bool {
        matches!(self, Change::Text(_))
    }

    pub fn text(&self) -> &str {
        match self {
            Change::Text(text) => text,
            _ => panic!("you must use Change::is_text() to guard calls to Change::text()"),
        }
    }
}

impl From<String> for Change {
    fn from(s: String) -> Self {
        Change::Text(s)
    }
}

impl From<&str> for Change {
    fn from(s: &str) -> Self {
        Change::Text(s.into())
    }
}

impl From<AttributeChange> for Change {
    fn from(c: AttributeChange) -> Self {
        Change::Attribute(c)
    }
}

impl From<LineAttribute> for Change {
    fn from(attr: LineAttribute) -> Self {
        Change::LineAttribute(attr)
    }
}

/// Keeps track of a run of changes and allows reasoning about the cursor
/// position and the extent of the screen that the sequence will affect.
/// This is useful for example when implementing something like a LineEditor
/// where you don't want to take control over the entire surface but do want
/// to be able to emit a dynamically sized output relative to the cursor
/// position at the time that the editor is invoked.
pub struct ChangeSequence {
    changes: Vec<Change>,
    screen_rows: usize,
    screen_cols: usize,
    pub(crate) cursor_x: usize,
    pub(crate) cursor_y: isize,
    render_y_max: isize,
    render_y_min: isize,
}

impl ChangeSequence {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            changes: vec![],
            screen_rows: rows,
            screen_cols: cols,
            cursor_x: 0,
            cursor_y: 0,
            render_y_max: 0,
            render_y_min: 0,
        }
    }

    pub fn consume(self) -> Vec<Change> {
        self.changes
    }

    /// Returns the cursor position, (x, y).
    pub fn current_cursor_position(&self) -> (usize, isize) {
        (self.cursor_x, self.cursor_y)
    }

    pub fn move_to(&mut self, (cursor_x, cursor_y): (usize, isize)) {
        self.add(Change::CursorPosition {
            x: Position::Relative(cursor_x as isize - self.cursor_x as isize),
            y: Position::Relative(cursor_y - self.cursor_y),
        });
    }

    /// Returns the total number of rows affected
    pub fn render_height(&self) -> usize {
        (self.render_y_max - self.render_y_min).max(0).abs() as usize
    }

    fn update_render_height(&mut self) {
        self.render_y_max = self.render_y_max.max(self.cursor_y);
        self.render_y_min = self.render_y_min.min(self.cursor_y);
    }

    pub fn add_changes(&mut self, changes: Vec<Change>) {
        for change in changes {
            self.add(change);
        }
    }

    pub fn add<C: Into<Change>>(&mut self, change: C) {
        let change = change.into();
        match &change {
            Change::AllAttributes(_)
            | Change::Attribute(_)
            | Change::CursorColor(_)
            | Change::CursorShape(_)
            | Change::CursorVisibility(_)
            | Change::ClearToEndOfLine(_)
            | Change::Title(_)
            | Change::LineAttribute(_)
            | Change::ClearToEndOfScreen(_) => {}
            Change::Text(t) => {
                for g in Graphemes::new(t.as_str()) {
                    if self.cursor_x == self.screen_cols {
                        self.cursor_y += 1;
                        self.cursor_x = 0;
                    }
                    if g == "\n" {
                        self.cursor_y += 1;
                    } else if g == "\r" {
                        self.cursor_x = 0;
                    } else if g == "\r\n" {
                        self.cursor_y += 1;
                        self.cursor_x = 0;
                    } else {
                        let len = unicode_column_width(g, None);
                        self.cursor_x += len;
                    }
                }
                self.update_render_height();
            }
            #[cfg(feature = "use_image")]
            Change::Image(im) => {
                self.cursor_x += im.width;
                self.render_y_max = self.render_y_max.max(self.cursor_y + im.height as isize);
            }
            Change::ClearScreen(_) => {
                self.cursor_x = 0;
                self.cursor_y = 0;
            }
            Change::CursorPosition { x, y } => {
                self.cursor_x = match x {
                    Position::Relative(x) => {
                        ((self.cursor_x as isize + x) % self.screen_cols as isize) as usize
                    }
                    Position::Absolute(x) => x % self.screen_cols,
                    Position::EndRelative(x) => (self.screen_cols - x) % self.screen_cols,
                };

                self.cursor_y = match y {
                    Position::Relative(y) => {
                        (self.cursor_y as isize + y) % self.screen_rows as isize
                    }
                    Position::Absolute(y) => (y % self.screen_rows) as isize,
                    Position::EndRelative(y) => {
                        ((self.screen_rows - y) % self.screen_rows) as isize
                    }
                };
                self.update_render_height();
            }
            Change::ScrollRegionUp { .. } | Change::ScrollRegionDown { .. } => {
                // The resultant cursor position is undefined by
                // the renderer!
                // We just pick something.
                self.cursor_x = 0;
                self.cursor_y = 0;
            }
        }

        self.changes.push(change);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::format;
    use frankenterm_cell::color::AnsiColor;

    // === LineAttribute tests ===

    #[test]
    fn line_attribute_variants_are_distinct() {
        let variants = [
            LineAttribute::DoubleHeightTopHalfLine,
            LineAttribute::DoubleHeightBottomHalfLine,
            LineAttribute::DoubleWidthLine,
            LineAttribute::SingleWidthLine,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn line_attribute_clone_eq() {
        let attr = LineAttribute::DoubleWidthLine;
        let cloned = attr.clone();
        assert_eq!(attr, cloned);
    }

    #[test]
    fn line_attribute_debug() {
        let attr = LineAttribute::SingleWidthLine;
        let dbg = format!("{:?}", attr);
        assert!(dbg.contains("SingleWidthLine"));
    }

    // === Change type tests ===

    #[test]
    fn change_is_text_true_for_text() {
        let c = Change::Text("hello".into());
        assert!(c.is_text());
    }

    #[test]
    fn change_is_text_false_for_non_text() {
        assert!(!Change::ClearScreen(Default::default()).is_text());
        assert!(!Change::Title("t".into()).is_text());
        assert!(!Change::CursorShape(CursorShape::Default).is_text());
        assert!(!Change::CursorVisibility(CursorVisibility::Visible).is_text());
    }

    #[test]
    fn change_text_returns_content() {
        let c = Change::Text("hello world".into());
        assert_eq!(c.text(), "hello world");
    }

    #[test]
    #[should_panic(expected = "you must use Change::is_text()")]
    fn change_text_panics_on_non_text() {
        let c = Change::ClearScreen(Default::default());
        let _ = c.text();
    }

    #[test]
    fn change_from_string() {
        let c: Change = String::from("hello").into();
        assert!(c.is_text());
        assert_eq!(c.text(), "hello");
    }

    #[test]
    fn change_from_str() {
        let c: Change = "world".into();
        assert!(c.is_text());
        assert_eq!(c.text(), "world");
    }

    #[test]
    fn change_from_attribute_change() {
        let attr = AttributeChange::Intensity(frankenterm_cell::Intensity::Bold);
        let c: Change = attr.into();
        assert!(matches!(c, Change::Attribute(_)));
    }

    #[test]
    fn change_from_line_attribute() {
        let la = LineAttribute::DoubleWidthLine;
        let c: Change = la.into();
        assert!(matches!(
            c,
            Change::LineAttribute(LineAttribute::DoubleWidthLine)
        ));
    }

    #[test]
    fn change_clone_eq() {
        let c = Change::Text("abc".into());
        let cloned = c.clone();
        assert_eq!(c, cloned);
    }

    #[test]
    fn change_debug() {
        let c = Change::ClearScreen(Default::default());
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("ClearScreen"));
    }

    #[test]
    fn change_cursor_position_eq() {
        let c1 = Change::CursorPosition {
            x: Position::Absolute(5),
            y: Position::Relative(-1),
        };
        let c2 = c1.clone();
        assert_eq!(c1, c2);
    }

    #[test]
    fn change_scroll_region_up_fields() {
        let c = Change::ScrollRegionUp {
            first_row: 2,
            region_size: 10,
            scroll_count: 3,
        };
        assert!(matches!(
            c,
            Change::ScrollRegionUp {
                first_row: 2,
                region_size: 10,
                scroll_count: 3,
            }
        ));
    }

    #[test]
    fn change_scroll_region_down_fields() {
        let c = Change::ScrollRegionDown {
            first_row: 0,
            region_size: 5,
            scroll_count: 1,
        };
        assert!(matches!(
            c,
            Change::ScrollRegionDown {
                first_row: 0,
                region_size: 5,
                scroll_count: 1,
            }
        ));
    }

    #[test]
    fn change_title() {
        let c = Change::Title("My Terminal".into());
        assert!(matches!(c, Change::Title(ref s) if s == "My Terminal"));
    }

    #[test]
    fn change_cursor_color() {
        let c = Change::CursorColor(AnsiColor::Blue.into());
        assert!(matches!(c, Change::CursorColor(_)));
    }

    // === ChangeSequence tests ===

    #[test]
    fn change_sequence_new_starts_at_origin() {
        let cs = ChangeSequence::new(24, 80);
        assert_eq!(cs.current_cursor_position(), (0, 0));
    }

    #[test]
    fn change_sequence_render_height_initially_zero() {
        let cs = ChangeSequence::new(24, 80);
        assert_eq!(cs.render_height(), 0);
    }

    #[test]
    fn change_sequence_text_advances_cursor() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("Hello");
        assert_eq!(cs.current_cursor_position(), (5, 0));
    }

    #[test]
    fn change_sequence_text_wraps_at_screen_width() {
        let mut cs = ChangeSequence::new(24, 4);
        cs.add("abcde");
        // 'a','b','c','d' fills row, 'e' wraps to next row
        assert_eq!(cs.current_cursor_position(), (1, 1));
    }

    #[test]
    fn change_sequence_newline_advances_y() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("abc\ndef");
        // \n advances y but doesn't reset x, so x = 3 + 3 = 6
        assert_eq!(cs.current_cursor_position(), (6, 1));
    }

    #[test]
    fn change_sequence_cr_resets_x() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("abc\rxy");
        assert_eq!(cs.current_cursor_position(), (2, 0));
    }

    #[test]
    fn change_sequence_crlf_moves_to_next_line_start() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("abc\r\nxy");
        assert_eq!(cs.current_cursor_position(), (2, 1));
    }

    #[test]
    fn change_sequence_clear_screen_resets_to_origin() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("Hello");
        cs.add(Change::ClearScreen(Default::default()));
        assert_eq!(cs.current_cursor_position(), (0, 0));
    }

    #[test]
    fn change_sequence_cursor_position_absolute() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add(Change::CursorPosition {
            x: Position::Absolute(10),
            y: Position::Absolute(5),
        });
        assert_eq!(cs.current_cursor_position(), (10, 5));
    }

    #[test]
    fn change_sequence_cursor_position_relative() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("Hello");
        cs.add(Change::CursorPosition {
            x: Position::Relative(-3),
            y: Position::Relative(2),
        });
        assert_eq!(cs.current_cursor_position(), (2, 2));
    }

    #[test]
    fn change_sequence_cursor_position_end_relative() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add(Change::CursorPosition {
            x: Position::EndRelative(5),
            y: Position::EndRelative(3),
        });
        assert_eq!(cs.current_cursor_position(), (75, 21));
    }

    #[test]
    fn change_sequence_scroll_resets_cursor() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("Hello World");
        cs.add(Change::ScrollRegionUp {
            first_row: 0,
            region_size: 24,
            scroll_count: 1,
        });
        assert_eq!(cs.current_cursor_position(), (0, 0));
    }

    #[test]
    fn change_sequence_scroll_down_resets_cursor() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("Hello World");
        cs.add(Change::ScrollRegionDown {
            first_row: 0,
            region_size: 24,
            scroll_count: 1,
        });
        assert_eq!(cs.current_cursor_position(), (0, 0));
    }

    #[test]
    fn change_sequence_consume_returns_changes() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("Hello");
        cs.add(Change::ClearScreen(Default::default()));
        let changes = cs.consume();
        assert_eq!(changes.len(), 2);
        assert!(changes[0].is_text());
    }

    #[test]
    fn change_sequence_move_to() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("Hello");
        cs.move_to((10, 5));
        assert_eq!(cs.current_cursor_position(), (10, 5));
    }

    #[test]
    fn change_sequence_render_height_tracks_cursor() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("abc\ndef\nghi");
        assert_eq!(cs.render_height(), 2);
    }

    #[test]
    fn change_sequence_add_changes_bulk() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add_changes(vec![
            Change::Text("abc".into()),
            Change::ClearScreen(Default::default()),
            Change::Text("xyz".into()),
        ]);
        let changes = cs.consume();
        assert_eq!(changes.len(), 3);
    }

    #[test]
    fn change_sequence_attribute_changes_dont_move_cursor() {
        let mut cs = ChangeSequence::new(24, 80);
        cs.add("abc");
        let pos = cs.current_cursor_position();
        cs.add(Change::CursorShape(CursorShape::BlinkingBar));
        assert_eq!(cs.current_cursor_position(), pos);
        cs.add(Change::CursorVisibility(CursorVisibility::Hidden));
        assert_eq!(cs.current_cursor_position(), pos);
        cs.add(Change::Title("test".into()));
        assert_eq!(cs.current_cursor_position(), pos);
    }
}

/// The `Image` `Change` needs to support adding an image that spans multiple
/// rows and columns, as well as model the content for just one of those cells.
/// For instance, if some of the cells inside an image are replaced by textual
/// content, and the screen is scrolled, computing the diff change stream needs
/// to be able to express that a single cell holds a slice from a larger image.
/// The `Image` struct expresses its dimensions in cells and references a region
/// in the shared source image data using texture coordinates.
/// A 4x3 cell image would set `width=3`, `height=3`, `top_left=(0,0)`, `bottom_right=(1,1)`.
/// The top left cell from that image, if it were to be included in a diff,
/// would be recorded as `width=1`, `height=1`, `top_left=(0,0)`, `bottom_right=(1/4,1/3)`.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[cfg(feature = "use_image")]
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Image {
    /// measured in cells
    pub width: usize,
    /// measure in cells
    pub height: usize,
    /// Texture coordinate for the top left of this image block.
    /// (0,0) is the top left of the ImageData. (1, 1) is
    /// the bottom right.
    pub top_left: TextureCoordinate,
    /// Texture coordinates for the bottom right of this image block.
    pub bottom_right: TextureCoordinate,
    /// the image data
    pub image: Arc<ImageData>,
}
