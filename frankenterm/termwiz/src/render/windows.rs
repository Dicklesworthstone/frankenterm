//! A Renderer for windows consoles

use crate::Result;
use crate::caps::{Capabilities, ColorLevel};
use crate::cell::{AttributeChange, CellAttributes, Underline};
use crate::color::{AnsiColor, ColorAttribute};
use crate::surface::{Change, Position};
use crate::terminal::windows::ConsoleOutputHandle;
use num_traits::FromPrimitive;
use std::io::Write;
use winapi::shared::minwindef::WORD;
use winapi::um::wincon::{
    BACKGROUND_BLUE, BACKGROUND_GREEN, BACKGROUND_INTENSITY, BACKGROUND_RED, CHAR_INFO,
    COMMON_LVB_REVERSE_VIDEO, COMMON_LVB_UNDERSCORE, FOREGROUND_BLUE, FOREGROUND_GREEN,
    FOREGROUND_INTENSITY, FOREGROUND_RED,
};

pub struct WindowsConsoleRenderer {
    pending_attr: CellAttributes,
    capabilities: Capabilities,
}

fn relative_coordinate(current: usize, delta: isize) -> usize {
    if delta >= 0 {
        current.saturating_add(delta as usize)
    } else {
        current.saturating_sub(delta.unsigned_abs())
    }
}

fn end_relative_coordinate(extent: usize, offset: usize) -> usize {
    extent.saturating_sub(offset.saturating_add(1))
}

fn negative_scroll_count(value: usize) -> isize {
    if value > isize::MAX as usize {
        isize::MIN
    } else {
        -(value as isize)
    }
}

fn screen_cell_count(cols: usize, rows: usize) -> usize {
    cols.saturating_mul(rows)
}

fn coord_extent_to_usize(value: i16) -> usize {
    usize::try_from(i32::from(value).max(0)).unwrap_or(0)
}

fn visible_window_rows(top: i16, bottom: i16) -> usize {
    let top = i32::from(top);
    let bottom = i32::from(bottom);
    if bottom < top {
        0
    } else {
        usize::try_from(bottom - top + 1).unwrap_or(usize::MAX)
    }
}

fn relative_window_coord(position: i16, origin: i16) -> usize {
    usize::try_from(i32::from(position).saturating_sub(i32::from(origin))).unwrap_or(0)
}

impl WindowsConsoleRenderer {
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            pending_attr: CellAttributes::default(),
            capabilities,
        }
    }
}

fn to_attr_word(capabilities: &Capabilities, attr: &CellAttributes) -> u16 {
    macro_rules! ansi_colors_impl {
        ($idx:expr, $default:ident,
                $red:ident, $green:ident, $blue:ident,
                $bright:ident, $( ($variant:ident, $bits:expr) ),*) =>{
            match FromPrimitive::from_u8($idx).unwrap_or(AnsiColor::$default) {
                $(
                    AnsiColor::$variant => $bits,
                )*
            }
        }
    }

    macro_rules! ansi_colors {
        ($idx:expr, $default:ident, $red:ident, $green:ident, $blue:ident, $bright:ident) => {
            ansi_colors_impl!(
                $idx,
                $default,
                $red,
                $green,
                $blue,
                $bright,
                (Black, 0),
                (Maroon, $red),
                (Green, $green),
                (Olive, $red | $green),
                (Navy, $blue),
                (Purple, $red | $blue),
                (Teal, $green | $blue),
                (Silver, $red | $green | $blue),
                (Grey, $bright),
                (Red, $bright | $red),
                (Lime, $bright | $green),
                (Yellow, $bright | $red | $green),
                (Blue, $bright | $blue),
                (Fuchsia, $bright | $red | $blue),
                (Aqua, $bright | $green | $blue),
                (White, $bright | $red | $green | $blue)
            )
        };
    }

    let reverse = if attr.reverse() {
        COMMON_LVB_REVERSE_VIDEO
    } else {
        0
    };
    let underline = if attr.underline() != Underline::None {
        COMMON_LVB_UNDERSCORE
    } else {
        0
    };

    if capabilities.color_level() == ColorLevel::MonoChrome {
        return reverse | underline;
    }

    let fg = match attr.foreground() {
        ColorAttribute::TrueColorWithDefaultFallback(_) | ColorAttribute::Default => {
            FOREGROUND_BLUE | FOREGROUND_RED | FOREGROUND_GREEN
        }

        ColorAttribute::TrueColorWithPaletteFallback(_, idx)
        | ColorAttribute::PaletteIndex(idx) => ansi_colors!(
            idx,
            White,
            FOREGROUND_RED,
            FOREGROUND_GREEN,
            FOREGROUND_BLUE,
            FOREGROUND_INTENSITY
        ),
    };

    let bg = match attr.background() {
        ColorAttribute::TrueColorWithDefaultFallback(_) | ColorAttribute::Default => 0,
        ColorAttribute::TrueColorWithPaletteFallback(_, idx)
        | ColorAttribute::PaletteIndex(idx) => ansi_colors!(
            idx,
            Black,
            BACKGROUND_RED,
            BACKGROUND_GREEN,
            BACKGROUND_BLUE,
            BACKGROUND_INTENSITY
        ),
    };

    bg | fg | reverse | underline
}

struct ScreenBuffer {
    buf: Vec<CHAR_INFO>,
    dirty: bool,
    rows: usize,
    cols: usize,
    cursor_x: usize,
    cursor_y: usize,
    pending_attr: WORD,
}

impl ScreenBuffer {
    fn cursor_idx(&self) -> usize {
        let idx = self
            .cursor_y
            .checked_mul(self.cols)
            .and_then(|base| base.checked_add(self.cursor_x))
            .expect("cursor cell index overflow");
        assert!(
            idx < self.rows.saturating_mul(self.cols),
            "idx={}, cursor:({},{}) rows={}, cols={}.",
            idx,
            self.cursor_x,
            self.cursor_y,
            self.rows,
            self.cols
        );
        idx
    }

    fn fill(&mut self, c: char, attr: WORD, x: usize, y: usize, num_elements: usize) -> usize {
        let idx = y
            .saturating_mul(self.cols)
            .saturating_add(x)
            .min(self.buf.len());
        let max = self.rows.saturating_mul(self.cols).min(self.buf.len());

        let end = idx.saturating_add(num_elements).min(max);
        let c = c as u16;
        for cell in &mut self.buf[idx..end] {
            cell.Attributes = attr;
            unsafe {
                *cell.Char.UnicodeChar_mut() = c;
            }
        }
        self.dirty = true;
        end
    }

    fn do_cursor_y_scroll<B: ConsoleOutputHandle + Write>(&mut self, out: &mut B) -> Result<()> {
        if self.rows == 0 {
            self.cursor_y = 0;
            return Ok(());
        }
        if self.cursor_y >= self.rows {
            self.dirty = true;
            let lines_to_scroll = self.cursor_y.saturating_sub(self.rows).saturating_add(1);
            self.scroll(0, self.rows, negative_scroll_count(lines_to_scroll), out)?;
            self.dirty = true;
            self.cursor_y = self.cursor_y.saturating_sub(lines_to_scroll);
            assert!(self.cursor_y < self.rows);
        }
        Ok(())
    }

    fn set_cursor<B: ConsoleOutputHandle + Write>(
        &mut self,
        x: usize,
        y: usize,
        out: &mut B,
    ) -> Result<()> {
        self.cursor_x = x;
        self.cursor_y = y;

        self.do_cursor_y_scroll(out)?;

        // Make sure we mark dirty after we've scrolled!
        self.dirty = true;
        assert!(self.cursor_x < self.cols);
        assert!(self.cursor_y < self.rows);
        Ok(())
    }

    fn write_text<B: ConsoleOutputHandle + Write>(
        &mut self,
        t: &str,
        attr: WORD,
        out: &mut B,
    ) -> Result<()> {
        for c in t.chars() {
            match c {
                '\r' => {
                    self.cursor_x = 0;
                }
                '\n' => {
                    self.cursor_y = self.cursor_y.saturating_add(1);
                    self.do_cursor_y_scroll(out)?;
                }
                c => {
                    if self.cursor_x == self.cols {
                        self.cursor_y = self.cursor_y.saturating_add(1);
                        self.cursor_x = 0;
                    }
                    self.do_cursor_y_scroll(out)?;

                    let idx = self.cursor_idx();

                    let cell = &mut self.buf[idx];
                    cell.Attributes = attr;
                    unsafe {
                        *cell.Char.UnicodeChar_mut() = c as u16;
                    }
                    self.cursor_x = self.cursor_x.saturating_add(1);
                    self.dirty = true;
                }
            }
        }
        Ok(())
    }

    fn flush<B: ConsoleOutputHandle + Write>(&mut self, out: &mut B) -> Result<()> {
        self.flush_screen(out)?;
        let info = out.get_buffer_info()?;
        out.set_cursor_position(
            self.cursor_x.min(i16::MAX as usize) as i16,
            (self.cursor_y.min(i16::MAX as usize) as i16).saturating_add(info.srWindow.Top),
        )?;
        out.set_attr(self.pending_attr)?;
        out.flush()?;
        Ok(())
    }

    fn flush_screen<B: ConsoleOutputHandle + Write>(&mut self, out: &mut B) -> Result<()> {
        if self.dirty {
            out.flush()?;
            out.set_buffer_contents(&self.buf)?;
            out.flush()?;
            self.dirty = false;
        }
        Ok(())
    }

    fn reread_buffer<B: ConsoleOutputHandle + Write>(&mut self, out: &mut B) -> Result<()> {
        self.buf = out.get_buffer_contents()?;
        self.dirty = false;
        Ok(())
    }

    fn scroll<B: ConsoleOutputHandle + Write>(
        &mut self,
        first_row: usize,
        region_size: usize,
        scroll_count: isize,
        out: &mut B,
    ) -> Result<()> {
        if region_size > 0 && scroll_count != 0 {
            self.flush_screen(out)?;
            let info = out.get_buffer_info()?;

            // Scroll the full width of the window, always.
            let left = 0;
            let right = info.dwSize.X.saturating_sub(1);

            // We're only doing vertical scrolling
            let dx = 0;
            let dy = scroll_count.clamp(i16::MIN as isize, i16::MAX as isize) as i16;

            if first_row == 0 && region_size == self.rows {
                // We're scrolling the whole screen, so let it scroll
                // up into the scrollback
                out.set_viewport(
                    info.srWindow.Left,
                    info.srWindow.Top.saturating_sub(dy),
                    info.srWindow.Right,
                    info.srWindow.Bottom.saturating_sub(dy),
                )?;
            } else {
                // We're just scrolling a region within the window
                let top = info
                    .srWindow
                    .Top
                    .saturating_add(first_row.min(i16::MAX as usize) as i16);
                let bottom =
                    top.saturating_add(region_size.saturating_sub(1).min(i16::MAX as usize) as i16);
                out.scroll_region(left, top, right, bottom, dx, dy, self.pending_attr)?;
            }

            self.reread_buffer(out)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_helpers_handle_extreme_relative_offsets() {
        assert_eq!(relative_coordinate(5, 3), 8);
        assert_eq!(relative_coordinate(5, -3), 2);
        assert_eq!(relative_coordinate(5, isize::MIN), 0);
        assert_eq!(end_relative_coordinate(10, 0), 9);
        assert_eq!(end_relative_coordinate(10, usize::MAX), 0);
    }

    #[test]
    fn negative_scroll_count_saturates_extreme_values() {
        assert_eq!(negative_scroll_count(3), -3);
        assert_eq!(negative_scroll_count(isize::MAX as usize + 1), isize::MIN);
    }

    #[test]
    fn screen_cell_count_saturates_extreme_dimensions() {
        assert_eq!(screen_cell_count(80, 24), 1_920);
        assert_eq!(screen_cell_count(usize::MAX, 2), usize::MAX);
    }

    #[test]
    fn windows_coord_helpers_do_not_wrap_negative_values() {
        assert_eq!(coord_extent_to_usize(120), 120);
        assert_eq!(coord_extent_to_usize(-1), 0);
        assert_eq!(visible_window_rows(3, 5), 3);
        assert_eq!(visible_window_rows(5, 3), 0);
        assert_eq!(relative_window_coord(10, 3), 7);
        assert_eq!(relative_window_coord(-1, 3), 0);
    }
}

impl WindowsConsoleRenderer {
    pub fn render_to<B: ConsoleOutputHandle + Write>(
        &mut self,
        changes: &[Change],
        out: &mut B,
    ) -> Result<()> {
        out.flush()?;
        let info = out.get_buffer_info()?;

        let cols = coord_extent_to_usize(info.dwSize.X);
        let rows = visible_window_rows(info.srWindow.Top, info.srWindow.Bottom);

        let mut buffer = ScreenBuffer {
            buf: out.get_buffer_contents()?,
            cursor_x: coord_extent_to_usize(info.dwCursorPosition.X),
            cursor_y: relative_window_coord(info.dwCursorPosition.Y, info.srWindow.Top),
            dirty: false,
            rows,
            cols,
            pending_attr: to_attr_word(&self.capabilities, &CellAttributes::default()),
        };

        for change in changes {
            match change {
                Change::ClearScreen(color) => {
                    let attr = CellAttributes::default()
                        .set_background(color.clone())
                        .clone();

                    buffer.fill(
                        ' ',
                        to_attr_word(&self.capabilities, &attr),
                        0,
                        0,
                        screen_cell_count(cols, rows),
                    );
                    buffer.set_cursor(0, 0, out)?;
                }
                Change::ClearToEndOfLine(color) => {
                    let attr = CellAttributes::default()
                        .set_background(color.clone())
                        .clone();

                    buffer.fill(
                        ' ',
                        to_attr_word(&self.capabilities, &attr),
                        buffer.cursor_x,
                        buffer.cursor_y,
                        cols.saturating_sub(buffer.cursor_x),
                    );
                }
                Change::ClearToEndOfScreen(color) => {
                    let attr = CellAttributes::default()
                        .set_background(color.clone())
                        .clone();

                    buffer.fill(
                        ' ',
                        to_attr_word(&self.capabilities, &attr),
                        buffer.cursor_x,
                        buffer.cursor_y,
                        screen_cell_count(cols, rows),
                    );
                }
                Change::Text(text) => {
                    buffer.write_text(
                        &text,
                        to_attr_word(&self.capabilities, &self.pending_attr),
                        out,
                    )?;
                }
                Change::CursorPosition { x, y } => {
                    let x = match x {
                        Position::Absolute(x) => *x as usize,
                        Position::Relative(delta) => relative_coordinate(buffer.cursor_x, *delta),
                        Position::EndRelative(delta) => end_relative_coordinate(cols, *delta),
                    };

                    // For vertical cursor movement, we constrain the movement to
                    // the viewport.
                    let y = match y {
                        Position::Absolute(y) => *y as usize,
                        Position::Relative(delta) => relative_coordinate(buffer.cursor_y, *delta),
                        Position::EndRelative(delta) => end_relative_coordinate(rows, *delta),
                    };

                    buffer.set_cursor(x, y, out)?;
                }
                Change::Attribute(AttributeChange::Intensity(value)) => {
                    self.pending_attr.set_intensity(*value);
                }
                Change::Attribute(AttributeChange::Italic(value)) => {
                    self.pending_attr.set_italic(*value);
                }
                Change::Attribute(AttributeChange::Reverse(value)) => {
                    self.pending_attr.set_reverse(*value);
                }
                Change::Attribute(AttributeChange::StrikeThrough(value)) => {
                    self.pending_attr.set_strikethrough(*value);
                }
                Change::Attribute(AttributeChange::Blink(value)) => {
                    self.pending_attr.set_blink(*value);
                }
                Change::Attribute(AttributeChange::Invisible(value)) => {
                    self.pending_attr.set_invisible(*value);
                }
                Change::Attribute(AttributeChange::Underline(value)) => {
                    self.pending_attr.set_underline(*value);
                }
                Change::Attribute(AttributeChange::Foreground(col)) => {
                    self.pending_attr.set_foreground(*col);
                }
                Change::Attribute(AttributeChange::Background(col)) => {
                    self.pending_attr.set_background(*col);
                }
                Change::Attribute(AttributeChange::Hyperlink(link)) => {
                    self.pending_attr.set_hyperlink(link.clone());
                }
                Change::AllAttributes(all) => {
                    self.pending_attr = all.clone();
                }
                Change::CursorColor(_color) => {}
                Change::CursorShape(_shape) => {}
                Change::CursorVisibility(_visibility) => {}
                #[cfg(feature = "use_image")]
                Change::Image(image) => {
                    // Images are not supported, so just blank out the cells and
                    // move the cursor to the right spot

                    for y in 0..image.height {
                        buffer.fill(
                            ' ',
                            0,
                            buffer.cursor_x,
                            y.saturating_add(buffer.cursor_y),
                            image.width as usize,
                        );
                    }
                    buffer.set_cursor(
                        buffer.cursor_x.saturating_add(image.width),
                        buffer.cursor_y,
                        out,
                    )?;
                }
                Change::ScrollRegionUp {
                    first_row,
                    region_size,
                    scroll_count,
                } => {
                    buffer.scroll(
                        *first_row,
                        *region_size,
                        negative_scroll_count(*scroll_count),
                        out,
                    )?;
                }
                Change::ScrollRegionDown {
                    first_row,
                    region_size,
                    scroll_count,
                } => {
                    buffer.scroll(*first_row, *region_size, *scroll_count as isize, out)?;
                }
                Change::Title(_text) => {
                    // Don't actually render this for now.
                    // The primary purpose of Change::Title at the time of
                    // writing is to transfer tab titles across domains
                    // in the wezterm multiplexer model.  It's not clear
                    // that it would be a good idea to unilaterally output
                    // eg: a title change escape sequence here in the
                    // renderer because we might be composing multiple widgets
                    // together, each with its own title.
                }
                Change::LineAttribute(_) => {
                    // Ignore line attributes
                }
            }
        }

        buffer.flush(out)?;
        Ok(())
    }
}
