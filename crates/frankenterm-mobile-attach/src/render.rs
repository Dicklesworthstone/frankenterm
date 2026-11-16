//! Render one mux [`Line`] as an ordinary terminal byte stream: its visible
//! text plus the minimal SGR (and OSC 8 hyperlink) changes between cells.
//!
//! Every rendered line starts and ends in the default SGR state with no open
//! hyperlink, so a caller may place lines anywhere (after a CUP, or as the
//! next line of a scrolling stream) without tracking attributes itself.

use std::sync::Arc;

use termwiz::cell::{Blink, CellAttributes, Hyperlink, Intensity, Underline};
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::surface::Line;

/// The SGR-visible subset of a cell's attributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sgr {
    intensity: Intensity,
    italic: bool,
    underline: Underline,
    blink: Blink,
    reverse: bool,
    invisible: bool,
    strikethrough: bool,
    overline: bool,
    foreground: ColorAttribute,
    background: ColorAttribute,
    underline_color: ColorAttribute,
}

impl Sgr {
    const DEFAULT: Self = Self {
        intensity: Intensity::Normal,
        italic: false,
        underline: Underline::None,
        blink: Blink::None,
        reverse: false,
        invisible: false,
        strikethrough: false,
        overline: false,
        foreground: ColorAttribute::Default,
        background: ColorAttribute::Default,
        underline_color: ColorAttribute::Default,
    };

    fn of(attrs: &CellAttributes) -> Self {
        Self {
            intensity: attrs.intensity(),
            italic: attrs.italic(),
            underline: attrs.underline(),
            blink: attrs.blink(),
            reverse: attrs.reverse(),
            invisible: attrs.invisible(),
            strikethrough: attrs.strikethrough(),
            overline: attrs.overline(),
            foreground: attrs.foreground(),
            background: attrs.background(),
            underline_color: attrs.underline_color(),
        }
    }

    /// Whether a space drawn with these attributes looks like an erased
    /// cell, so it may be left to the caller's trailing erase-in-line.
    fn blank_space_is_invisible(&self) -> bool {
        self.background == ColorAttribute::Default
            && !self.reverse
            && self.underline == Underline::None
            && !self.strikethrough
            && !self.overline
    }
}

/// Builds one CSI ... m sequence from `;`-separated parameter groups.
struct SgrParams<'a> {
    out: &'a mut Vec<u8>,
    started: bool,
}

impl<'a> SgrParams<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self {
            out,
            started: false,
        }
    }

    fn separator(&mut self) {
        if self.started {
            self.out.push(b';');
        } else {
            self.out.extend_from_slice(b"\x1b[");
            self.started = true;
        }
    }

    fn push(&mut self, param: &str) {
        self.separator();
        self.out.extend_from_slice(param.as_bytes());
    }

    /// `base;5;index` for a palette color, `base;2;r;g;b` for true color.
    fn push_color(&mut self, color: ColorAttribute, base: u8, default: &str) {
        match color {
            ColorAttribute::Default => self.push(default),
            ColorAttribute::PaletteIndex(index) => {
                self.separator();
                push_decimal(self.out, u32::from(base));
                self.out.extend_from_slice(b";5;");
                push_decimal(self.out, u32::from(index));
            }
            ColorAttribute::TrueColorWithPaletteFallback(rgb, _)
            | ColorAttribute::TrueColorWithDefaultFallback(rgb) => {
                let (r, g, b) = rgb_u8(rgb);
                self.separator();
                push_decimal(self.out, u32::from(base));
                self.out.extend_from_slice(b";2;");
                push_decimal(self.out, u32::from(r));
                self.out.push(b';');
                push_decimal(self.out, u32::from(g));
                self.out.push(b';');
                push_decimal(self.out, u32::from(b));
            }
        }
    }

    fn finish(self) {
        if self.started {
            self.out.push(b'm');
        }
    }
}

fn push_decimal(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(value.to_string().as_bytes());
}

/// Rounded, clamped 8-bit sRGB components.  Truncating `x * 255` maps many
/// exact 8-bit inputs one step down (200/255 * 255 is 199.99998 in f32).
fn rgb_u8(rgb: SrgbaTuple) -> (u8, u8, u8) {
    let component = |value: f32| -> u8 {
        if value.is_nan() {
            0
        } else {
            (value.clamp(0.0, 1.0) * 255.0).round() as u8
        }
    };
    (component(rgb.0), component(rgb.1), component(rgb.2))
}

/// Emit the minimal SGR change from `from` to `to`.
fn write_sgr_transition(from: &Sgr, to: &Sgr, out: &mut Vec<u8>) {
    if from == to {
        return;
    }
    if *to == Sgr::DEFAULT {
        out.extend_from_slice(b"\x1b[0m");
        return;
    }
    let mut params = SgrParams::new(out);
    if from.intensity != to.intensity {
        match (from.intensity, to.intensity) {
            (_, Intensity::Normal) => params.push("22"),
            (Intensity::Half, Intensity::Bold) => params.push("22;1"),
            (Intensity::Bold, Intensity::Half) => params.push("22;2"),
            (_, Intensity::Bold) => params.push("1"),
            (_, Intensity::Half) => params.push("2"),
        }
    }
    if from.italic != to.italic {
        params.push(if to.italic { "3" } else { "23" });
    }
    if from.underline != to.underline {
        params.push(match to.underline {
            Underline::None => "24",
            Underline::Single => "4",
            Underline::Double => "4:2",
            Underline::Curly => "4:3",
            Underline::Dotted => "4:4",
            Underline::Dashed => "4:5",
        });
    }
    if from.blink != to.blink {
        params.push(match to.blink {
            Blink::None => "25",
            Blink::Slow => "5",
            Blink::Rapid => "6",
        });
    }
    if from.reverse != to.reverse {
        params.push(if to.reverse { "7" } else { "27" });
    }
    if from.invisible != to.invisible {
        params.push(if to.invisible { "8" } else { "28" });
    }
    if from.strikethrough != to.strikethrough {
        params.push(if to.strikethrough { "9" } else { "29" });
    }
    if from.overline != to.overline {
        params.push(if to.overline { "53" } else { "55" });
    }
    if from.foreground != to.foreground {
        params.push_color(to.foreground, 38, "39");
    }
    if from.background != to.background {
        params.push_color(to.background, 48, "49");
    }
    if from.underline_color != to.underline_color {
        params.push_color(to.underline_color, 58, "59");
    }
    params.finish();
}

/// An explicit (OSC 8) hyperlink in the form this renderer emits it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct OpenLink {
    id: Option<String>,
    uri: String,
}

impl OpenLink {
    /// Explicit links only: implicit (rule-matched) links are a GUI
    /// decoration the phone's own terminal applies or not by itself.
    fn of(link: Option<&Arc<Hyperlink>>) -> Option<Self> {
        let link = link?;
        if link.is_implicit() || link.uri().is_empty() {
            return None;
        }
        let mut uri = String::with_capacity(link.uri().len());
        for byte in link.uri().bytes() {
            if byte.is_ascii_graphic() {
                uri.push(char::from(byte));
            } else {
                // Percent-encode anything that could end or corrupt the OSC.
                uri.push_str(&format!("%{byte:02X}"));
            }
        }
        let id = link.params().get("id").filter(|id| {
            !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_graphic() && b != b';' && b != b':')
        });
        Some(Self {
            id: id.cloned(),
            uri,
        })
    }

    fn write_open(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b]8;");
        if let Some(id) = &self.id {
            out.extend_from_slice(b"id=");
            out.extend_from_slice(id.as_bytes());
        }
        out.push(b';');
        out.extend_from_slice(self.uri.as_bytes());
        out.extend_from_slice(b"\x1b\\");
    }

    fn write_close(out: &mut Vec<u8>) {
        out.extend_from_slice(b"\x1b]8;;\x1b\\");
    }
}

/// Cell text with every control character removed; a cell that renders to
/// nothing becomes one space so column accounting stays exact.
fn write_cell_text(text: &str, out: &mut Vec<u8>) {
    let start = out.len();
    for ch in text.chars() {
        if !ch.is_control() {
            let mut utf8 = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
        }
    }
    if out.len() == start {
        out.push(b' ');
    }
}

/// The column just past the last cell that is not an erased-looking space.
fn significant_end(line: &Line) -> usize {
    let mut end = 0;
    for cell in line.visible_cells() {
        let blank = cell.str() == " "
            && cell.attrs().hyperlink().is_none()
            && Sgr::of(cell.attrs()).blank_space_is_invisible();
        if !blank {
            end = cell.cell_index() + cell.width().max(1);
        }
    }
    end
}

/// Tracks the renderer's emitted state across one line.
struct LineWriter<'a> {
    out: &'a mut Vec<u8>,
    sgr: Sgr,
    link: Option<OpenLink>,
}

impl LineWriter<'_> {
    fn set_link(&mut self, link: Option<OpenLink>) {
        if self.link == link {
            return;
        }
        if self.link.is_some() {
            OpenLink::write_close(self.out);
        }
        if let Some(link) = &link {
            link.write_open(self.out);
        }
        self.link = link;
    }

    fn set_sgr(&mut self, sgr: Sgr) {
        write_sgr_transition(&self.sgr, &sgr, self.out);
        self.sgr = sgr;
    }

    fn plain_spaces(&mut self, count: usize) {
        self.set_link(None);
        self.set_sgr(Sgr::DEFAULT);
        for _ in 0..count {
            self.out.push(b' ');
        }
    }

    fn finish(mut self) {
        self.set_link(None);
        self.set_sgr(Sgr::DEFAULT);
    }
}

/// Append `line` to `out`, clipped to `max_cols` columns, and return the
/// number of columns written.  Trailing erased-looking cells are omitted: a
/// caller that needs the rest of the row cleared follows a short result with
/// an erase-in-line (never after a full-width row, where EL would erase the
/// pending-wrap last column).
pub fn render_line(line: &Line, max_cols: usize, out: &mut Vec<u8>) -> usize {
    let end = significant_end(line).min(max_cols);
    let mut writer = LineWriter {
        out,
        sgr: Sgr::DEFAULT,
        link: None,
    };
    let mut col = 0;
    for cell in line.visible_cells() {
        let index = cell.cell_index();
        if index >= end {
            break;
        }
        if index < col {
            // Overlaps the previous wide cell: malformed, skip it.
            continue;
        }
        if index > col {
            writer.plain_spaces(index - col);
            col = index;
        }
        let width = cell.width().max(1);
        if col + width > max_cols {
            // A wide character that does not fit: pad the remaining columns.
            writer.plain_spaces(max_cols - col);
            col = max_cols;
            break;
        }
        writer.set_link(OpenLink::of(cell.attrs().hyperlink()));
        writer.set_sgr(Sgr::of(cell.attrs()));
        write_cell_text(cell.str(), writer.out);
        col += width;
    }
    writer.finish();
    col
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::cell::Cell;

    fn plain() -> CellAttributes {
        CellAttributes::default()
    }

    fn rendered(line: &Line, max_cols: usize) -> (String, usize) {
        let mut out = Vec::new();
        let cols = render_line(line, max_cols, &mut out);
        (String::from_utf8(out).expect("utf8 output"), cols)
    }

    fn line_of(cells: &[(&str, CellAttributes)]) -> Line {
        let mut line = Line::new(0);
        let mut col = 0;
        for (text, attrs) in cells {
            let cell = Cell::new_grapheme(text, attrs.clone(), None);
            let width = cell.width();
            line.set_cell(col, cell, 0);
            col += width;
        }
        line
    }

    #[test]
    fn plain_text_renders_verbatim_without_sgr() {
        let line = Line::from_text("hello world", &plain(), 0, None);
        assert_eq!(rendered(&line, 80), ("hello world".to_string(), 11));
    }

    #[test]
    fn trailing_erased_cells_are_left_to_the_callers_erase() {
        let line = Line::from_text("hi      ", &plain(), 0, None);
        assert_eq!(rendered(&line, 80), ("hi".to_string(), 2));
        assert_eq!(rendered(&Line::new(0), 80), (String::new(), 0));
    }

    #[test]
    fn styled_cells_emit_one_csi_and_reset_at_line_end() {
        let mut attrs = plain();
        attrs
            .set_intensity(Intensity::Bold)
            .set_italic(true)
            .set_foreground(ColorAttribute::PaletteIndex(196));
        let line = line_of(&[("X", attrs), ("y", plain())]);
        assert_eq!(
            rendered(&line, 80).0,
            "\x1b[1;3;38;5;196mX\x1b[0my",
            "the plain cell after a styled one resets with SGR 0"
        );
    }

    #[test]
    fn transitions_change_only_what_differs() {
        let mut bold = plain();
        bold.set_intensity(Intensity::Bold);
        let mut bold_underlined = bold.clone();
        bold_underlined.set_underline(Underline::Single);
        let mut half_underlined = bold_underlined.clone();
        half_underlined.set_intensity(Intensity::Half);
        let line = line_of(&[("a", bold), ("b", bold_underlined), ("c", half_underlined)]);
        assert_eq!(
            rendered(&line, 80).0,
            "\x1b[1ma\x1b[4mb\x1b[22;2mc\x1b[0m",
            "bold to half-bright clears bold first (SGR 22) so no terminal keeps both"
        );
    }

    #[test]
    fn every_attribute_has_its_own_set_and_reset_code() {
        let mut all = plain();
        all.set_blink(Blink::Rapid)
            .set_reverse(true)
            .set_invisible(true)
            .set_strikethrough(true)
            .set_overline(true);
        let mut slow = plain();
        slow.set_blink(Blink::Slow);
        let line = line_of(&[("a", all), ("b", slow), ("c", plain())]);
        assert_eq!(
            rendered(&line, 80).0,
            "\x1b[6;7;8;9;53ma\x1b[5;27;28;29;55mb\x1b[0mc"
        );
    }

    #[test]
    fn underline_styles_use_colon_subparameters() {
        let mut double = plain();
        double.set_underline(Underline::Double);
        let mut curly = plain();
        curly.set_underline(Underline::Curly);
        let mut dotted = plain();
        dotted.set_underline(Underline::Dotted);
        let mut dashed = plain();
        dashed.set_underline(Underline::Dashed);
        let line = line_of(&[("a", double), ("b", curly), ("c", dotted), ("d", dashed)]);
        assert_eq!(
            rendered(&line, 80).0,
            "\x1b[4:2ma\x1b[4:3mb\x1b[4:4mc\x1b[4:5md\x1b[0m"
        );
    }

    #[test]
    fn colors_cover_default_palette_and_true_color() {
        let rgb = |r: u8, g: u8, b: u8| {
            SrgbaTuple(
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0,
                1.0,
            )
        };
        let mut true_fg = plain();
        true_fg
            .set_foreground(ColorAttribute::TrueColorWithDefaultFallback(rgb(
                200, 100, 7,
            )))
            .set_background(ColorAttribute::PaletteIndex(4));
        let mut fallback_bg = plain();
        fallback_bg
            .set_background(ColorAttribute::TrueColorWithPaletteFallback(
                rgb(1, 255, 128),
                2,
            ))
            .set_underline(Underline::Single)
            .set_underline_color(ColorAttribute::PaletteIndex(9));
        let line = line_of(&[("a", true_fg), ("b", fallback_bg), ("c", plain())]);
        assert_eq!(
            rendered(&line, 80).0,
            "\x1b[38;2;200;100;7;48;5;4ma\x1b[4;39;48;2;1;255;128;58;5;9mb\x1b[0mc",
            "exact 8-bit components survive the f32 round trip"
        );
    }

    #[test]
    fn wide_characters_occupy_two_columns_and_skip_their_continuation() {
        let line = Line::from_text("a\u{6f22}b", &plain(), 0, None);
        assert_eq!(rendered(&line, 80), ("a\u{6f22}b".to_string(), 4));
    }

    #[test]
    fn a_wide_character_that_does_not_fit_is_padded_not_split() {
        let line = Line::from_text("a\u{6f22}b", &plain(), 0, None);
        assert_eq!(rendered(&line, 2), ("a ".to_string(), 2));
        assert_eq!(rendered(&line, 3), ("a\u{6f22}".to_string(), 3));
    }

    #[test]
    fn lines_are_clipped_to_the_screen_width() {
        let line = Line::from_text("abcdefgh", &plain(), 0, None);
        assert_eq!(rendered(&line, 3), ("abc".to_string(), 3));
        assert_eq!(rendered(&line, 0), (String::new(), 0));
    }

    #[test]
    fn styled_blanks_are_significant_and_plain_gaps_stay() {
        let mut reverse = plain();
        reverse.set_reverse(true);
        let line = line_of(&[("a", plain()), (" ", plain()), (" ", reverse)]);
        assert_eq!(rendered(&line, 80), ("a \x1b[7m \x1b[0m".to_string(), 3));
    }

    #[test]
    fn control_characters_in_cells_never_reach_the_output() {
        let mut line = Line::new(0);
        line.set_cell(0, Cell::new('\u{1b}', plain()), 0);
        line.set_cell(1, Cell::new('\u{9b}', plain()), 0);
        line.set_cell(2, Cell::new('z', plain()), 0);
        let (text, cols) = rendered(&line, 80);
        assert_eq!(cols, 3);
        assert_eq!(text, "  z");
        assert!(!text.bytes().any(|b| b == 0x1b));
    }

    #[test]
    fn explicit_hyperlinks_become_osc8_and_close_at_their_end() {
        let mut linked = plain();
        linked.set_hyperlink(Some(Arc::new(Hyperlink::new_with_id(
            "https://x.test/a b\u{1b}",
            "L1",
        ))));
        let line = line_of(&[("a", linked.clone()), ("b", linked), ("c", plain())]);
        assert_eq!(
            rendered(&line, 80).0,
            "\x1b]8;id=L1;https://x.test/a%20b%1B\x1b\\ab\x1b]8;;\x1b\\c"
        );
    }

    #[test]
    fn implicit_hyperlinks_are_not_forwarded() {
        let mut linked = plain();
        linked.set_hyperlink(Some(Arc::new(Hyperlink::new_implicit("https://x.test/"))));
        let line = line_of(&[("a", linked)]);
        assert_eq!(rendered(&line, 80).0, "a");
    }
}
