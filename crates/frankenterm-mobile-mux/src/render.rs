//! Escape sequences that draw remote lines on the phone's own terminal.
//!
//! The phone's terminal is a separate emulator, so a line is redrawn from
//! its cells: clear the row, then write each run of text under the SGR
//! state that produces its attributes and colors.

use std::fmt::Write as _;
use std::sync::Arc;
use termwiz::cell::{Blink, CellAttributes, Intensity, Underline};
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::hyperlink::Hyperlink;
use termwiz::surface::line::CellRef;
use termwiz::surface::{CursorShape, CursorVisibility, Line};

/// The attributes an SGR sequence can express. Links are written with
/// OSC 8 beside them; images and semantic zones are not drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Style {
    intensity: Intensity,
    underline: Underline,
    blink: Blink,
    italic: bool,
    reverse: bool,
    strikethrough: bool,
    invisible: bool,
    overline: bool,
    foreground: ColorAttribute,
    background: ColorAttribute,
    underline_color: ColorAttribute,
}

impl Style {
    fn of(attrs: &CellAttributes) -> Self {
        Self {
            intensity: attrs.intensity(),
            underline: attrs.underline(),
            blink: attrs.blink(),
            italic: attrs.italic(),
            reverse: attrs.reverse(),
            strikethrough: attrs.strikethrough(),
            invisible: attrs.invisible(),
            overline: attrs.overline(),
            foreground: attrs.foreground(),
            background: attrs.background(),
            underline_color: attrs.underline_color(),
        }
    }

    fn plain() -> Self {
        Self::of(&CellAttributes::default())
    }

    /// A blank in this style looks exactly like an erased cell.
    fn erases_to_blank(&self) -> bool {
        self.background == ColorAttribute::Default
            && !self.reverse
            && self.underline == Underline::None
            && !self.strikethrough
            && !self.overline
    }

    fn write_sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        match self.intensity {
            Intensity::Normal => {}
            Intensity::Bold => out.push_str(";1"),
            Intensity::Half => out.push_str(";2"),
        }
        if self.italic {
            out.push_str(";3");
        }
        match self.underline {
            Underline::None => {}
            Underline::Single => out.push_str(";4"),
            Underline::Double => out.push_str(";21"),
            Underline::Curly => out.push_str(";4:3"),
            Underline::Dotted => out.push_str(";4:4"),
            Underline::Dashed => out.push_str(";4:5"),
        }
        match self.blink {
            Blink::None => {}
            Blink::Slow => out.push_str(";5"),
            Blink::Rapid => out.push_str(";6"),
        }
        if self.reverse {
            out.push_str(";7");
        }
        if self.invisible {
            out.push_str(";8");
        }
        if self.strikethrough {
            out.push_str(";9");
        }
        if self.overline {
            out.push_str(";53");
        }
        write_color(out, self.foreground, 30, 90, 38);
        write_color(out, self.background, 40, 100, 48);
        write_color(out, self.underline_color, 0, 0, 58);
        out.push('m');
    }
}

/// `base` and `bright` are the SGR codes of palette colors 0 and 8; zero
/// means this slot only has the extended form (underline color).
fn write_color(out: &mut String, color: ColorAttribute, base: u8, bright: u8, extended: u8) {
    match color {
        ColorAttribute::Default => {}
        ColorAttribute::PaletteIndex(index) => {
            if base != 0 && index < 8 {
                let _ = write!(out, ";{}", base + index);
            } else if bright != 0 && index < 16 {
                let _ = write!(out, ";{}", bright + index - 8);
            } else {
                let _ = write!(out, ";{extended};5;{index}");
            }
        }
        ColorAttribute::TrueColorWithPaletteFallback(rgb, _)
        | ColorAttribute::TrueColorWithDefaultFallback(rgb) => {
            let (r, g, b) = srgb_bytes(rgb);
            let _ = write!(out, ";{extended};2;{r};{g};{b}");
        }
    }
}

fn srgb_bytes(color: SrgbaTuple) -> (u8, u8, u8) {
    let byte = |channel: f32| {
        let scaled = (channel.clamp(0.0, 1.0) * 255.0).round();
        // `scaled` is a whole number in 0..=255 here.
        (0u8..=255)
            .find(|value| f32::from(*value) >= scaled)
            .unwrap_or(255)
    };
    (byte(color.0), byte(color.1), byte(color.2))
}

/// Characters the phone's terminal would act on rather than draw.
fn is_control(c: char) -> bool {
    c < ' ' || ('\u{7f}'..='\u{9f}').contains(&c)
}

/// A cell's text with anything the phone would execute replaced.
fn drawable(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars()
        .map(|c| if is_control(c) { '\u{fffd}' } else { c })
}

/// The SGR state and link the phone's terminal is writing cells under.
struct Pen {
    style: Style,
    /// SGR does not end a link, so it is opened and closed on its own.
    link: Option<Arc<Hyperlink>>,
}

impl Pen {
    fn new() -> Self {
        Self {
            style: Style::plain(),
            link: None,
        }
    }

    /// Writes `cell`, whose style is `style`, at the cursor.
    fn write(&mut self, out: &mut String, style: Style, cell: &CellRef<'_>) {
        if style != self.style {
            style.write_sgr(out);
            self.style = style;
        }
        let link = cell.attrs().hyperlink();
        if link.map(|l| &**l) != self.link.as_deref() {
            if self.link.is_some() {
                out.push_str("\x1b]8;;\x1b\\");
            }
            if let Some(next) = link {
                open_link(out, next);
            }
            self.link = link.cloned();
        }
        let text = cell.str();
        if text.is_empty() {
            out.push(' ');
        } else {
            out.extend(drawable(text));
        }
    }

    /// Closes the link and resets SGR, if either is open.
    fn finish(self, out: &mut String) {
        if self.link.is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
        if self.style != Style::plain() {
            out.push_str("\x1b[0m");
        }
    }
}

/// Draws `line` on the row the cursor is on, from the first column; cells
/// past `cols` are dropped. Leaves the cursor on that row with SGR reset and
/// no link open. True when the line wraps onto the row below: its last
/// column is then written last, and the cursor waits past it for
/// `continue_onto`.
pub fn paint_line(out: &mut String, line: Option<&Line>, cols: usize) -> bool {
    out.push_str("\r\x1b[0m\x1b[2K");
    paint_cells(out, line, cols)
}

/// `paint_line` for the screen's top row. Erasing that whole row would end
/// the wrap of the history row above it in the phone's terminal, which the
/// remote keeps, so its cells are erased one by one (ECH) instead.
pub fn paint_top_line(out: &mut String, line: Option<&Line>, cols: usize) -> bool {
    let _ = write!(out, "\r\x1b[0m\x1b[{cols}X");
    paint_cells(out, line, cols)
}

/// Writes `line`'s cells on a row just erased, the cursor on its first column.
fn paint_cells(out: &mut String, line: Option<&Line>, cols: usize) -> bool {
    let Some(line) = line else { return false };
    let wraps = line.last_cell_was_wrapped();
    let mut pen = Pen::new();
    // Where the phone's cursor is, when it is known: after a wide or
    // non-ASCII cell the phone's width tables decide, so the next cell is
    // placed with an absolute column instead.
    let mut column = Some(0usize);
    let mut end = 0;
    for cell in line.visible_cells() {
        let index = cell.cell_index();
        let width = cell.width().max(1);
        if index + width > cols {
            break;
        }
        let style = Style::of(cell.attrs());
        let text = cell.str();
        // A wrapping row's last column is written even when blank: the
        // phone's terminal marks the wrap only after a character there.
        if (text == " " || text.is_empty())
            && style.erases_to_blank()
            && cell.attrs().hyperlink().is_none()
            && !(wraps && index + width == cols)
        {
            continue;
        }
        if column != Some(index) {
            let _ = write!(out, "\x1b[{}G", index + 1);
        }
        pen.write(out, style, &cell);
        column = (width == 1 && text.is_ascii()).then_some(index + 1);
        end = index + width;
    }
    pen.finish(out);
    if wraps && end != cols {
        // It wrapped before a wide character that did not fit, or its last
        // column was never stored: blank there, as on the remote.
        let _ = write!(out, "\x1b[{cols}G ");
    }
    wraps
}

/// Writes the first cell of `next`, the row below one `paint_line` left
/// waiting past its last column. The phone's terminal wraps onto that row
/// and marks the one above as continuing there; the row below shows what
/// it did. Leaves the cursor on the row below with SGR reset and no link.
pub fn continue_onto(out: &mut String, next: Option<&Line>) {
    let first = next
        .and_then(|line| line.visible_cells().next())
        .filter(|cell| cell.cell_index() == 0);
    match first {
        Some(cell) => {
            let mut pen = Pen::new();
            pen.write(out, Style::of(cell.attrs()), &cell);
            pen.finish(out);
        }
        None => out.push(' '),
    }
}

/// Opens an OSC 8 link to the remote cell's address, without its
/// parameters. Characters that would end or redirect the sequence go.
fn open_link(out: &mut String, link: &Hyperlink) {
    out.push_str("\x1b]8;;");
    out.extend(link.uri().chars().filter(|c| !is_control(*c)));
    out.push_str("\x1b\\");
}

/// Moves the cursor to a zero-based row and column.
pub fn move_to(out: &mut String, row: usize, column: usize) {
    let _ = write!(out, "\x1b[{};{}H", row + 1, column + 1);
}

pub fn cursor_style(out: &mut String, shape: CursorShape, visibility: CursorVisibility) {
    let shape = match shape {
        CursorShape::Default => 0,
        CursorShape::BlinkingBlock => 1,
        CursorShape::SteadyBlock => 2,
        CursorShape::BlinkingUnderline => 3,
        CursorShape::SteadyUnderline => 4,
        CursorShape::BlinkingBar => 5,
        CursorShape::SteadyBar => 6,
    };
    let _ = write!(out, "\x1b[{shape} q");
    out.push_str(match visibility {
        CursorVisibility::Visible => "\x1b[?25h",
        CursorVisibility::Hidden => "\x1b[?25l",
    });
}

/// Sets the phone terminal's title to the remote pane's.
pub fn title(out: &mut String, title: &str) {
    out.push_str("\x1b]2;");
    out.extend(title.chars().filter(|c| !is_control(*c)));
    out.push_str("\x1b\\");
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::cell::Cell;
    use termwiz::color::AnsiColor;

    fn line_of(cells: &[(&str, CellAttributes)]) -> Line {
        let mut line = Line::with_width(0, 0);
        let mut column = 0;
        for (text, attrs) in cells {
            let cell = Cell::new_grapheme(text, attrs.clone(), None);
            let width = cell.width();
            line.set_cell(column, cell, 0);
            column += width;
        }
        line
    }

    fn painted(line: &Line, cols: usize) -> String {
        let mut out = String::new();
        paint_line(&mut out, Some(line), cols);
        out
    }

    #[test]
    fn plain_text_is_written_after_clearing_the_row() {
        let line = line_of(&[
            ("h", CellAttributes::default()),
            ("i", CellAttributes::default()),
        ]);
        assert_eq!(painted(&line, 80), "\r\x1b[0m\x1b[2Khi");
    }

    #[test]
    fn styles_change_only_where_attributes_do() {
        let mut bold_red = CellAttributes::default();
        bold_red.set_intensity(Intensity::Bold);
        bold_red.set_foreground(AnsiColor::Maroon);
        let mut true_bg = CellAttributes::default();
        true_bg.set_background(ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
            1.0, 0.5, 0.0, 1.0,
        )));
        let line = line_of(&[
            ("a", bold_red.clone()),
            ("b", bold_red),
            ("c", true_bg),
            ("d", CellAttributes::default()),
        ]);
        assert_eq!(
            painted(&line, 80),
            "\r\x1b[0m\x1b[2K\x1b[0;1;31mab\x1b[0;48;2;255;128;0mc\x1b[0md"
        );
    }

    #[test]
    fn blanks_are_skipped_and_text_after_them_is_placed() {
        let line = line_of(&[
            ("a", CellAttributes::default()),
            (" ", CellAttributes::default()),
            (" ", CellAttributes::default()),
            ("b", CellAttributes::default()),
        ]);
        assert_eq!(painted(&line, 80), "\r\x1b[0m\x1b[2Ka\x1b[4Gb");
    }

    #[test]
    fn a_wide_character_is_followed_by_an_absolute_column() {
        let line = line_of(&[
            ("界", CellAttributes::default()),
            ("x", CellAttributes::default()),
        ]);
        assert_eq!(painted(&line, 80), "\r\x1b[0m\x1b[2K界\x1b[3Gx");
    }

    #[test]
    fn cells_past_the_width_are_not_sent() {
        let line = line_of(&[
            ("a", CellAttributes::default()),
            ("b", CellAttributes::default()),
            ("c", CellAttributes::default()),
        ]);
        assert_eq!(painted(&line, 2), "\r\x1b[0m\x1b[2Kab");
    }

    #[test]
    fn control_characters_are_drawn_as_replacements() {
        let text: String = drawable("a\u{1b}[2J\u{7}b\u{9b}c\u{85}é").collect();
        assert_eq!(text, "a\u{fffd}[2J\u{fffd}b\u{fffd}c\u{fffd}é");
    }

    #[test]
    fn titles_lose_their_control_characters() {
        let mut out = String::new();
        title(&mut out, "vim\x07 x\x1b]");
        assert_eq!(out, "\x1b]2;vim x]\x1b\\");
    }

    fn linked(uri: &str, attrs: &CellAttributes) -> CellAttributes {
        let mut attrs = attrs.clone();
        let mut params = std::collections::HashMap::new();
        params.insert("id".to_owned(), "7".to_owned());
        attrs.set_hyperlink(Some(Arc::new(Hyperlink::new_with_params(uri, params))));
        attrs
    }

    #[test]
    fn a_link_is_written_around_its_cells_and_closed_before_the_row_ends() {
        let mut bold = CellAttributes::default();
        bold.set_intensity(Intensity::Bold);
        let pr = linked("https://example.com/pr/1", &CellAttributes::default());
        let line = line_of(&[
            ("a", CellAttributes::default()),
            ("P", pr.clone()),
            ("R", linked("https://example.com/pr/1", &bold)),
            ("b", CellAttributes::default()),
            ("Q", pr),
        ]);
        // One link across a style change; the same address after plain
        // text opens again; the row never ends inside a link.
        assert_eq!(
            painted(&line, 80),
            "\r\x1b[0m\x1b[2Ka\x1b]8;;https://example.com/pr/1\x1b\\P\x1b[0;1mR\
             \x1b[0m\x1b]8;;\x1b\\b\x1b]8;;https://example.com/pr/1\x1b\\Q\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn adjacent_links_to_different_addresses_each_get_their_own() {
        let line = line_of(&[
            (
                "x",
                linked("https://a.example/", &CellAttributes::default()),
            ),
            (
                "y",
                linked("https://b.example/", &CellAttributes::default()),
            ),
        ]);
        assert_eq!(
            painted(&line, 80),
            "\r\x1b[0m\x1b[2K\x1b]8;;https://a.example/\x1b\\x\x1b]8;;\x1b\\\
             \x1b]8;;https://b.example/\x1b\\y\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn a_blank_inside_a_link_is_written_with_it() {
        let pr = linked("https://example.com/pr/1", &CellAttributes::default());
        let line = line_of(&[("P", pr.clone()), (" ", pr.clone()), ("1", pr)]);
        assert_eq!(
            painted(&line, 80),
            "\r\x1b[0m\x1b[2K\x1b]8;;https://example.com/pr/1\x1b\\P 1\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn a_wrapped_row_ends_waiting_on_its_last_column() {
        let mut line = line_of(&[
            ("a", CellAttributes::default()),
            ("b", CellAttributes::default()),
            (" ", CellAttributes::default()),
        ]);
        line.set_last_cell_was_wrapped(true, 0);
        let mut out = String::new();
        assert!(paint_line(&mut out, Some(&line), 3));
        assert_eq!(
            out, "\r\x1b[0m\x1b[2Kab ",
            "its blank last column is written"
        );
        // Wrapped before a wide character that did not fit: the last column
        // was never stored, and is written blank.
        let mut early = line_of(&[("a", CellAttributes::default())]);
        early.set_last_cell_was_wrapped(true, 0);
        let mut out = String::new();
        assert!(paint_line(&mut out, Some(&early), 3));
        assert_eq!(out, "\r\x1b[0m\x1b[2Ka\x1b[3G ");
        let mut out = String::new();
        assert!(!paint_line(
            &mut out,
            Some(&line_of(&[("a", CellAttributes::default())])),
            3
        ));
    }

    #[test]
    fn a_wrap_carries_on_with_the_next_rows_first_cell_as_it_looks() {
        let mut bold = CellAttributes::default();
        bold.set_intensity(Intensity::Bold);
        let next = line_of(&[("x", linked("https://a.example/", &bold)), ("y", bold)]);
        let mut out = String::new();
        continue_onto(&mut out, Some(&next));
        assert_eq!(
            out,
            "\x1b[0;1m\x1b]8;;https://a.example/\x1b\\x\x1b]8;;\x1b\\\x1b[0m"
        );
        let mut out = String::new();
        continue_onto(&mut out, None);
        assert_eq!(out, " ", "a row the server no longer has carries on blank");
    }

    #[test]
    fn a_link_address_cannot_end_or_redirect_its_sequence() {
        let line = line_of(&[(
            "x",
            linked(
                "https://e.example/\x1b\\\x1b]2;pwned\x07\u{9c}a",
                &CellAttributes::default(),
            ),
        )]);
        assert_eq!(
            painted(&line, 80),
            "\r\x1b[0m\x1b[2K\x1b]8;;https://e.example/\\]2;pwneda\x1b\\x\x1b]8;;\x1b\\"
        );
    }
}
