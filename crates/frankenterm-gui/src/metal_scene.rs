//! Metal frame scenes from a pane's render mirror (ft-yccm0.4.4).
//!
//! [`MetalScene`] keeps the Metal renderer's per-pane frame inputs: the
//! [`CellBgGrid`] of cell backgrounds and the [`CellTextGrid`] of glyph
//! instances. It builds them from a [`RenderMirror`], after the terminal lock
//! is released, and rebuilds only the rows that need it:
//! - rows the last capture copied (their mirror generation is newer than the
//!   one they were built from);
//! - rows whose selection span or cursor changed;
//! - rows with hyperlinks when the hovered hyperlink changes;
//! - every row when the style generation (palette, colors, configuration),
//!   reverse video or the grid size changes.
//!
//! A viewport that moved down by fewer rows than it has (output scrolling)
//! rotates both grids' rings, so only the exposed rows and the changed ones
//! are rebuilt. Any other move rebuilds every row.
//!
//! Colors resolve the way the WebGpu renderer resolves them:
//! - bold brightens the eight ANSI colors when configured;
//! - reverse (and reverse video) swaps foreground and background;
//! - a selected cell takes the selection foreground, and the focused block
//!   cursor's cell takes the cursor foreground;
//! - invisible text, or text in the color it sits on (its background, the
//!   selection's, or a focused block cursor's), draws no glyph;
//! - an underline takes its own color (SGR 58), or else the foreground;
//! - the hovered hyperlink is underlined.
//!
//! The background pass draws the selection tint and the cursor itself. The
//! caller builds its uniforms.
//!
//! Glyphs come from a [`GlyphSource`]: the GUI's fonts placing them in the
//! renderer's atlases, or a synthetic source in tests.

use frankenterm_renderer_metal::{
    AtlasSlot, CellBg, CellBgGrid, CellText, CellTextGrid, GridExtent, UnderlineStyle,
};
use mux::render_mirror::{MirrorCell, MirrorColor, MirrorRow, RenderMirror};
use std::ops::Range;
use std::sync::Arc;
use termwiz::cell::{Intensity, Underline};
use termwiz::hyperlink::Hyperlink;
use wezterm_term::StableRowIndex;
use wezterm_term::color::{ColorAttribute, ColorPalette, SrgbaTuple};

/// The font style a glyph is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct GlyphStyle {
    pub bold: bool,
    pub half: bool,
    pub italic: bool,
}

/// A glyph in an atlas, its top-left `offset` pixels from the cell's
/// top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedGlyph {
    pub slot: AtlasSlot,
    pub offset: [i16; 2],
}

/// Where the glyphs of a frame come from.
pub trait GlyphSource {
    /// The glyphs that draw `text` (one grapheme) in `style` across `width`
    /// cells. Empty when the fonts have nothing to draw.
    fn glyphs(&mut self, text: &str, style: GlyphStyle, width: usize) -> &[PlacedGlyph];
}

/// What a frame's colors depend on besides the cells.
#[derive(Debug, Clone, Copy)]
pub struct SceneStyle<'a> {
    pub palette: &'a ColorPalette,
    /// Changes whenever anything else here changes except `hover` and the
    /// cursor (a new palette, configuration or color scheme): every row is
    /// rebuilt.
    pub generation: u64,
    /// `bold_brightens_ansi_colors` is enabled.
    pub bold_brightens: bool,
    /// The selection's text color; `None` keeps each cell's foreground.
    pub selection_fg: Option<SrgbaTuple>,
    /// The selection's background, which selected text sits on.
    pub selection_bg: SrgbaTuple,
    /// The text color under a focused block cursor; `None` when the cursor
    /// is not a focused block (the background pass draws other shapes) or
    /// keeps the cell's foreground.
    pub cursor_fg: Option<SrgbaTuple>,
    /// A focused block cursor's color, which the text under it sits on;
    /// `None` when the cursor is not a focused block.
    pub cursor_bg: Option<SrgbaTuple>,
    /// The hyperlink under the mouse, underlined wherever it appears.
    pub hover: Option<&'a Arc<Hyperlink>>,
}

/// What one rebuilt row was built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BuiltRow {
    stable: Option<StableRowIndex>,
    generation: u64,
    selection: Range<usize>,
    /// The cursor's column on this row, with a focused block's foreground
    /// override and its color.
    cursor: Option<(usize, Option<[u8; 4]>, Option<[u8; 4]>)>,
}

/// What [`MetalScene::update`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SceneUpdate {
    pub rows_rebuilt: usize,
    /// The grids were rebuilt from scratch.
    pub full: bool,
    /// The rings rotated up by this many rows (output scrolling, or the
    /// viewport moving down).
    pub scrolled: u32,
    /// The rings rotated down by this many rows (the viewport moving back
    /// through scrollback).
    pub scrolled_back: u32,
}

/// One pane's Metal frame inputs, rebuilt incrementally from its mirror.
#[derive(Debug, Clone)]
pub struct MetalScene {
    cells: CellBgGrid,
    text: CellTextGrid,
    built: Vec<BuiltRow>,
    first: Option<StableRowIndex>,
    hover: Option<Arc<Hyperlink>>,
    style_generation: Option<u64>,
    reverse_video: bool,
}

impl Default for MetalScene {
    fn default() -> Self {
        Self::new()
    }
}

/// A color component as the 8-bit value the GPU stores.
// Clamped to 0.0..=255.0 before the cast.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn unorm8(component: f32) -> u8 {
    (component.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn rgba8(SrgbaTuple(red, green, blue, alpha): SrgbaTuple) -> [u8; 4] {
    [unorm8(red), unorm8(green), unorm8(blue), unorm8(alpha)]
}

fn underline_style(underline: Underline) -> UnderlineStyle {
    match underline {
        Underline::None => UnderlineStyle::None,
        Underline::Single => UnderlineStyle::Single,
        Underline::Double => UnderlineStyle::Double,
        Underline::Curly => UnderlineStyle::Curly,
        Underline::Dotted => UnderlineStyle::Dotted,
        Underline::Dashed => UnderlineStyle::Dashed,
    }
}

fn same_link(a: Option<&Arc<Hyperlink>>, b: Option<&Arc<Hyperlink>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b) || **a == **b,
        _ => false,
    }
}

/// A cell's foreground, its explicit background (a default background is
/// left to the cleared frame), and the background its text sits on.
struct CellColors {
    fg: SrgbaTuple,
    explicit_bg: Option<SrgbaTuple>,
    bg: SrgbaTuple,
}

fn cell_colors(cell: &MirrorCell, style: &SceneStyle<'_>, reverse_video: bool) -> CellColors {
    let palette = style.palette;
    let fg = match cell.fg() {
        MirrorColor::Palette(index)
            if index < 8 && style.bold_brightens && cell.intensity() == Intensity::Bold =>
        {
            palette.resolve_fg(ColorAttribute::PaletteIndex(index + 8))
        }
        color => palette.resolve_fg(color.to_attribute()),
    };
    let bg = palette.resolve_bg(cell.bg().to_attribute());
    if cell.reverse() != reverse_video {
        CellColors {
            fg: bg,
            explicit_bg: Some(fg),
            bg: fg,
        }
    } else {
        CellColors {
            fg,
            explicit_bg: (cell.bg() != MirrorColor::Default).then_some(bg),
            bg,
        }
    }
}

impl MetalScene {
    pub fn new() -> Self {
        let empty = GridExtent::new(0, 0);
        Self {
            cells: CellBgGrid::new(empty),
            text: CellTextGrid::new(empty),
            built: Vec::new(),
            first: None,
            hover: None,
            style_generation: None,
            reverse_video: false,
        }
    }

    /// The cell backgrounds the background pass draws.
    pub fn cells(&self) -> &CellBgGrid {
        &self.cells
    }

    /// The glyph instances the text pass draws.
    pub fn text(&self) -> &CellTextGrid {
        &self.text
    }

    /// Forgets every built row (the glyphs' atlas slots died, say), so the
    /// next update rebuilds the frame.
    pub fn invalidate(&mut self) {
        self.style_generation = None;
    }

    /// Brings the grids up to date with `mirror`. `selection` gives the
    /// selected columns of a stable row (an empty range for none).
    pub fn update(
        &mut self,
        mirror: &RenderMirror,
        style: &SceneStyle<'_>,
        selection: &dyn Fn(StableRowIndex) -> Range<usize>,
        glyphs: &mut dyn GlyphSource,
    ) -> SceneUpdate {
        self.update_rows(mirror, style, selection, glyphs, None)
    }

    /// [`Self::update`], optionally failing to rebuild one row that needs
    /// it: the planted fault that proves the frame equality tests can fail.
    fn update_rows(
        &mut self,
        mirror: &RenderMirror,
        style: &SceneStyle<'_>,
        selection: &dyn Fn(StableRowIndex) -> Range<usize>,
        glyphs: &mut dyn GlyphSource,
        planted_skip: Option<usize>,
    ) -> SceneUpdate {
        let Some(dimensions) = mirror.dimensions() else {
            return SceneUpdate::default();
        };
        let rows = mirror.rows();
        let extent = GridExtent::new(rows.len(), dimensions.cols);
        let mut update = SceneUpdate {
            full: self.cells.extent() != extent
                || self.style_generation != Some(style.generation)
                || self.reverse_video != dimensions.reverse_video,
            ..SceneUpdate::default()
        };
        if update.full {
            self.cells = CellBgGrid::new(extent);
            self.text = CellTextGrid::new(extent);
            self.built = vec![BuiltRow::default(); rows.len()];
        } else if let Some(previous) = self.first {
            let delta = mirror.first() - previous;
            if delta > 0 && (delta as usize) < rows.len() {
                let lines = u32::try_from(delta).unwrap_or(u32::MAX);
                self.cells.scroll_up(lines);
                self.text.scroll_up(lines);
                self.built.rotate_left(delta as usize);
                let exposed = rows.len() - delta as usize;
                for built in &mut self.built[exposed..] {
                    *built = BuiltRow::default();
                }
                update.scrolled = lines;
            } else if delta < 0 && (delta.unsigned_abs()) < rows.len() {
                let back = delta.unsigned_abs();
                let lines = u32::try_from(back).unwrap_or(u32::MAX);
                self.cells.scroll_down(lines);
                self.text.scroll_down(lines);
                self.built.rotate_right(back);
                for built in &mut self.built[..back] {
                    *built = BuiltRow::default();
                }
                update.scrolled_back = lines;
            }
        }
        let hover_changed = !same_link(self.hover.as_ref(), style.hover);
        let cursor = mirror.cursor();
        let cursor_row = (cursor.visibility == termwiz::surface::CursorVisibility::Visible)
            .then(|| cursor.y - mirror.first())
            .and_then(|row| usize::try_from(row).ok());
        for (row, mirror_row) in rows.iter().enumerate() {
            let wanted = BuiltRow {
                stable: Some(mirror_row.stable()),
                generation: mirror_row.generation(),
                selection: selection(mirror_row.stable()),
                cursor: (cursor_row == Some(row)).then(|| {
                    (
                        cursor.x,
                        style.cursor_fg.map(rgba8),
                        style.cursor_bg.map(rgba8),
                    )
                }),
            };
            let hovered = hover_changed
                && mirror_row
                    .cells()
                    .iter()
                    .any(|cell| mirror_row.hyperlink(cell).is_some());
            if update.full || hovered || self.built[row] != wanted {
                if planted_skip == Some(row) && !update.full {
                    self.built[row] = wanted;
                    continue;
                }
                self.build_row(
                    row,
                    mirror_row,
                    style,
                    &wanted,
                    dimensions.reverse_video,
                    glyphs,
                );
                self.built[row] = wanted;
                update.rows_rebuilt += 1;
            }
        }
        self.first = Some(mirror.first());
        self.hover = style.hover.cloned();
        self.style_generation = Some(style.generation);
        self.reverse_video = dimensions.reverse_video;
        update
    }

    // Rows and columns are bounded by the grid extent, which fits u16.
    #[allow(clippy::cast_possible_truncation)]
    fn build_row(
        &mut self,
        row: usize,
        mirror_row: &MirrorRow,
        style: &SceneStyle<'_>,
        built: &BuiltRow,
        reverse_video: bool,
        glyphs: &mut dyn GlyphSource,
    ) {
        let grid_row = row as u32;
        let cols = self.cells.extent().cols;
        self.cells.fill_row(grid_row, CellBg::DEFAULT);
        self.text.clear_row(grid_row);
        for cell in mirror_row.cells() {
            let col = cell.col();
            let colors = cell_colors(cell, style, reverse_video);
            if let Some(SrgbaTuple(red, green, blue, _)) = colors.explicit_bg {
                let background = CellBg::rgb(unorm8(red), unorm8(green), unorm8(blue));
                if cell.width() > 1 {
                    self.cells.set_wide(grid_row, col as u32, background);
                } else {
                    self.cells.set(grid_row, col as u32, background);
                }
            }

            // Text in the color it sits on draws no glyph. As in the WebGpu
            // renderer (compute_cell_fg_bg), that is the selection's
            // background for selected text and a focused block cursor's
            // color under it, not the cell's own background.
            let mut fg = colors.fg;
            let mut under = colors.bg;
            if built.selection.contains(&col) {
                if let Some(selection_fg) = style.selection_fg {
                    fg = selection_fg;
                }
                under = style.selection_bg;
            }
            let (mut fg, mut under) = (rgba8(fg), rgba8(under));
            if let Some((cursor_col, cursor_fg, cursor_bg)) = built.cursor {
                if cursor_col == col {
                    if let Some(cursor_fg) = cursor_fg {
                        fg = cursor_fg;
                    }
                    if let Some(cursor_bg) = cursor_bg {
                        under = cursor_bg;
                    }
                }
            }
            if cell.invisible() || fg == under {
                continue;
            }
            let mut underline = underline_style(cell.underline());
            if underline == UnderlineStyle::None
                && style
                    .hover
                    .is_some_and(|hover| same_link(Some(hover), mirror_row.hyperlink(cell)))
            {
                underline = UnderlineStyle::Single;
            }
            let underline_color = match cell.underline_color() {
                MirrorColor::Default => fg,
                color => rgba8(style.palette.resolve_fg(color.to_attribute())),
            };
            let decorate = |mut instance: CellText| {
                if cell.width() > 1 {
                    instance = instance.wide();
                }
                if underline != UnderlineStyle::None {
                    let [red, green, blue, _] = underline_color;
                    instance = instance.with_underline(underline, [red, green, blue]);
                }
                if cell.strikethrough() {
                    instance = instance.with_strikethrough();
                }
                if cell.overline() {
                    instance = instance.with_overline();
                }
                instance
            };
            let text = mirror_row.text(cell);
            let glyph_style = GlyphStyle {
                bold: cell.intensity() == Intensity::Bold,
                half: cell.intensity() == Intensity::Half,
                italic: cell.italic(),
            };
            let placed = if text.trim().is_empty() {
                &[][..]
            } else {
                glyphs.glyphs(text, glyph_style, cell.width())
            };
            if placed.is_empty() {
                // A blank cell can still be underlined, struck or overlined.
                if underline != UnderlineStyle::None || cell.strikethrough() || cell.overline() {
                    self.text
                        .push(grid_row, decorate(CellText::new(col as u16, fg)));
                }
                continue;
            }
            for glyph in placed {
                let instance = CellText::new(col as u16, fg).with_glyph(&glyph.slot, glyph.offset);
                self.text.push(grid_row, decorate(instance));
            }
        }
        // The selection tints its whole span, blank columns included.
        for col in built.selection.start..built.selection.end.min(cols as usize) {
            let col = col as u32;
            if let Some(background) = self.cells.get(grid_row, col) {
                self.cells.set(grid_row, col, background.selected());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_renderer_metal::AtlasKind;
    use mux::render_mirror::{CaptureRequest, capture_terminal_rows};
    use mux::renderable::{
        terminal_get_cursor_position, terminal_get_dimensions, terminal_get_lines,
    };
    use std::collections::HashMap;
    use wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

    #[derive(Debug)]
    struct TestConfig;

    impl TerminalConfiguration for TestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    fn size(rows: usize, cols: usize) -> TerminalSize {
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        }
    }

    fn terminal(rows: usize, cols: usize) -> Terminal {
        Terminal::new(
            size(rows, cols),
            Arc::new(TestConfig),
            "FrankenTerm",
            "metal-scene-test",
            Box::new(Vec::<u8>::new()),
        )
    }

    /// Glyphs with made-up atlas slots, one set per (text, style, width), so
    /// the instance bytes say which glyph each instance draws.
    #[derive(Default)]
    struct SyntheticGlyphs {
        placed: HashMap<(String, GlyphStyle, usize), Vec<PlacedGlyph>>,
    }

    fn synthetic_slot(index: u32, text: &str) -> AtlasSlot {
        let color = text.chars().any(|c| u32::from(c) >= 0x1F000);
        AtlasSlot {
            kind: if color {
                AtlasKind::Color
            } else {
                AtlasKind::Grayscale
            },
            page: 0,
            epoch: 0,
            x: (index % 64) * 16,
            y: (index / 64) * 24,
            width: 6 + index % 5,
            height: 12,
        }
    }

    impl GlyphSource for SyntheticGlyphs {
        fn glyphs(&mut self, text: &str, style: GlyphStyle, width: usize) -> &[PlacedGlyph] {
            let index = u32::try_from(self.placed.len()).unwrap_or(u32::MAX);
            self.placed
                .entry((text.to_string(), style, width))
                .or_insert_with(|| {
                    vec![PlacedGlyph {
                        slot: synthetic_slot(index, text),
                        offset: [1, 2 + (index % 3) as i16],
                    }]
                })
        }
    }

    /// The Line-based reference: full copies of every viewport row.
    fn reference_mirror(term: &mut Terminal, viewport_top: Option<StableRowIndex>) -> RenderMirror {
        let dims = terminal_get_dimensions(term);
        let first = viewport_top
            .unwrap_or(dims.physical_top)
            .max(dims.scrollback_top)
            .min(dims.physical_top);
        let end = first + dims.viewport_rows as StableRowIndex;
        let (lines_first, lines) = terminal_get_lines(term, first..end);
        assert_eq!(lines_first, first);
        RenderMirror::from_lines(dims, terminal_get_cursor_position(term), first, &lines)
    }

    #[test]
    fn colors_become_clamped_and_rounded_unorm_bytes() {
        assert_eq!(unorm8(0.2), 51);
        assert_eq!(unorm8(1.5), 255);
        assert_eq!(unorm8(-1.0), 0);
        assert_eq!(rgba8(SrgbaTuple(0.2, 1.5, -1.0, 0.5)), [51, 255, 0, 128]);
    }

    /// An instance's bytes without its ring row, which depends on how far
    /// the ring has rotated, not on what the row shows.
    fn normalized(instance: CellText) -> [u8; 24] {
        let mut bytes = *instance.bytes();
        bytes[2] = 0;
        bytes[3] = 0;
        bytes
    }

    /// The first difference between what two scenes draw, row by row.
    fn difference(scene: &MetalScene, reference: &MetalScene) -> Option<String> {
        let extent = scene.cells().extent();
        if extent != reference.cells().extent() || extent != scene.text().extent() {
            return Some(format!(
                "extent {:?} vs {:?}",
                extent,
                reference.cells().extent()
            ));
        }
        for row in 0..extent.rows {
            for col in 0..extent.cols {
                let (ours, theirs) = (scene.cells().get(row, col), reference.cells().get(row, col));
                if ours != theirs {
                    return Some(format!(
                        "background row {row} col {col}: {ours:?} vs {theirs:?}"
                    ));
                }
            }
            let ours: Vec<_> = scene.text().row(row).map(normalized).collect();
            let theirs: Vec<_> = reference.text().row(row).map(normalized).collect();
            if ours != theirs {
                return Some(format!("glyphs of row {row}: {ours:?}\n vs {theirs:?}"));
            }
        }
        None
    }

    /// xorshift64*: deterministic, so a failing seed replays.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }

        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len() as u64) as usize]
        }
    }

    const WORDS: &[&str] = &[
        "alpha",
        "beta",
        "你好",
        "e\u{301}",
        "👍",
        "👩\u{200d}💻",
        "x",
        "  ",
    ];
    const SGR: &[&str] = &[
        "0",
        "1",
        "3",
        "4",
        "4:3",
        "7",
        "8",
        "9",
        "53",
        "22",
        "27",
        "38;5;3",
        "38;5;196",
        "48;5;21",
        "38;2;10;200;30",
        "48;2;250;128;7",
        "58;5;46",
        "39",
        "49",
    ];

    fn random_output(rng: &mut Rng, rows: usize, cols: usize) -> Vec<u8> {
        let (rows, cols) = (rows as u64, cols as u64);
        let mut out = String::new();
        match rng.below(14) {
            0..=3 => {
                for _ in 0..1 + rng.below(6) {
                    out.push_str(rng.pick(WORDS));
                    out.push(' ');
                }
            }
            4 | 5 => out.push_str(&format!("\x1b[{}m", rng.pick(SGR))),
            6 => out.push_str(&format!(
                "\x1b[{};{}H",
                1 + rng.below(rows + 1),
                1 + rng.below(cols + 1)
            )),
            7 | 8 => {
                for _ in 0..1 + rng.below(3) {
                    out.push_str("\r\n");
                }
            }
            9 => {
                let top = 1 + rng.below(rows);
                let bottom = top + rng.below(rows + 1 - top);
                out.push_str(&format!("\x1b[{top};{bottom}r\x1b[{top};1H"));
                out.push_str(rng.pick(&["\x1b[2L", "\x1b[M", "\x1b[S", "\x1b[T"]));
                out.push_str("\x1b[r");
            }
            10 => out.push_str(rng.pick(&["\x1b[J", "\x1b[2J", "\x1b[K", "\x1b[1K"])),
            11 => out.push_str(rng.pick(&["\x1b[?1049h", "\x1b[?1049l", "\x1b[?47h", "\x1b[?47l"])),
            12 => out.push_str("\x1b]8;;https://example.com/a\x1b\\link\x1b]8;;\x1b\\"),
            _ => out.push_str(rng.pick(&["\x1b[2@", "\x1b[P", "\x1b[3X"])),
        }
        out.into_bytes()
    }

    /// Feeds random output, capturing and updating the scene incrementally,
    /// and compares each frame with one built from scratch out of full line
    /// copies. With `plant`, each update fails to rebuild the cursor's row.
    /// Returns how many frames differed.
    fn scene_session(seed: u64, plant: bool, steps: usize) -> usize {
        let mut rng = Rng::new(seed);
        let (mut rows, mut cols) = (8, 24);
        let mut term = terminal(rows, cols);
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        let mut glyphs = SyntheticGlyphs::default();
        let palette = ColorPalette::default();
        let link = Arc::new(Hyperlink::new("https://example.com/a"));
        let mut generation = 0;
        let mut differences = 0;
        for step in 0..steps {
            term.advance_bytes(random_output(&mut rng, rows, cols));
            if rng.below(50) == 0 {
                rows = 4 + rng.below(9) as usize;
                cols = 10 + rng.below(31) as usize;
                term.resize(size(rows, cols));
            }
            if rng.below(2) == 0 {
                continue;
            }
            let dims = terminal_get_dimensions(&mut term);
            let viewport_top = (rng.below(5) == 0).then(|| {
                let back = dims.physical_top - dims.scrollback_top;
                dims.physical_top - rng.below(back as u64 + 1) as StableRowIndex
            });
            let request = CaptureRequest {
                viewport_top,
                rules: &[],
                rules_generation: 0,
            };
            capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident viewport");
            if rng.below(25) == 0 {
                generation += 1;
            }
            let hover = (rng.below(3) == 0).then_some(&link);
            // Selection and cursor text sometimes in the default background
            // or in the color it sits on, and the block cursor coming and
            // going, so the glyph-hiding rule is compared with full builds.
            let style = SceneStyle {
                palette: &palette,
                generation,
                bold_brightens: true,
                selection_fg: *rng.pick(&[
                    None,
                    Some(SrgbaTuple(1.0, 1.0, 1.0, 1.0)),
                    Some(palette.background),
                    Some(palette.selection_bg),
                ]),
                selection_bg: palette.selection_bg,
                cursor_fg: (rng.below(2) == 0).then_some(SrgbaTuple(0.0, 0.0, 0.0, 1.0)),
                cursor_bg: (rng.below(2) == 0).then_some(palette.cursor_bg),
                hover,
            };
            let top = mirror.first() + rng.below(rows as u64) as StableRowIndex;
            let bottom = top + rng.below(3) as StableRowIndex;
            let span = rng.below(cols as u64) as usize..rng.below(cols as u64 + 4) as usize;
            let selection = move |stable: StableRowIndex| {
                if (top..=bottom).contains(&stable) {
                    span.clone()
                } else {
                    0..0
                }
            };
            let planted = plant
                .then(|| usize::try_from(mirror.cursor().y - mirror.first()).ok())
                .flatten();
            let update = scene.update_rows(&mirror, &style, &selection, &mut glyphs, planted);
            assert!(update.rows_rebuilt <= rows);
            let reference = reference_mirror(&mut term, viewport_top);
            let mut full = MetalScene::new();
            full.update(&reference, &style, &selection, &mut glyphs);
            if let Some(difference) = difference(&scene, &full) {
                assert!(
                    plant,
                    "seed {seed} step {step}: incremental frame differs from a full one: {difference}"
                );
                differences += 1;
                scene.invalidate();
            }
        }
        differences
    }

    #[test]
    fn incremental_scenes_equal_scenes_built_from_full_line_copies() {
        for seed in 0..10 {
            assert_eq!(scene_session(seed, false, 300), 0);
        }
    }

    #[test]
    fn the_scene_check_catches_an_update_that_misses_a_changed_row() {
        let caught: usize = (0..4).map(|seed| scene_session(seed, true, 200)).sum();
        assert!(caught > 0, "a missed row rebuild went unnoticed");
    }

    fn plain_style(palette: &ColorPalette) -> SceneStyle<'_> {
        SceneStyle {
            palette,
            generation: 0,
            bold_brightens: true,
            selection_fg: None,
            selection_bg: palette.selection_bg,
            cursor_fg: None,
            cursor_bg: None,
            hover: None,
        }
    }

    fn no_selection(_: StableRowIndex) -> Range<usize> {
        0..0
    }

    /// Captures `term` into `mirror` and updates `scene` from it.
    fn step(
        term: &mut Terminal,
        mirror: &mut RenderMirror,
        scene: &mut MetalScene,
        glyphs: &mut SyntheticGlyphs,
        selection: &dyn Fn(StableRowIndex) -> Range<usize>,
        style: &SceneStyle<'_>,
    ) -> SceneUpdate {
        let request = CaptureRequest {
            viewport_top: None,
            rules: &[],
            rules_generation: 0,
        };
        capture_terminal_rows(term, mirror, &request).expect("resident");
        scene.update(mirror, style, selection, glyphs)
    }

    #[test]
    fn an_update_rebuilds_only_the_rows_that_changed() {
        let palette = ColorPalette::default();
        let style = plain_style(&palette);
        let mut glyphs = SyntheticGlyphs::default();
        let mut term = terminal(10, 30);
        term.advance_bytes(b"one\r\ntwo\r\nthree");
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        let (t, m, s, g) = (&mut term, &mut mirror, &mut scene, &mut glyphs);
        let update = step(t, m, s, g, &no_selection, &style);
        assert_eq!((update.full, update.rows_rebuilt), (true, 10));
        assert_eq!(step(t, m, s, g, &no_selection, &style).rows_rebuilt, 0);

        // One written cell: its row and the row the cursor left (the end of
        // "three"); the cursor ends on the written row.
        t.advance_bytes(b"\x1b[6;3Hx");
        let update = step(t, m, s, g, &no_selection, &style);
        assert_eq!((update.full, update.rows_rebuilt), (false, 2));

        // A scroll by one line at the bottom rotates the rings: at most the
        // written row, the exposed row and the row the cursor left.
        t.advance_bytes(b"\x1b[10;1Hlast\r\n");
        let update = step(t, m, s, g, &no_selection, &style);
        assert_eq!(update.scrolled, 1);
        assert!(update.rows_rebuilt <= 3, "{update:?}");

        // A selection on one row: that row only.
        let first = m.first();
        let selection = move |stable: StableRowIndex| {
            if stable == first + 2 { 0..3 } else { 0..0 }
        };
        assert_eq!(step(t, m, s, g, &selection, &style).rows_rebuilt, 1);

        // A new style generation: everything.
        let restyled = SceneStyle {
            generation: 1,
            ..style
        };
        let update = step(t, m, s, g, &selection, &restyled);
        assert_eq!((update.full, update.rows_rebuilt), (true, 10));
    }

    /// Scrollback navigation (ft-yccm0.4.2.4): moving the viewport back or
    /// forward by a few rows rotates the rings and rebuilds only the exposed
    /// rows (and the row the cursor left), never the whole frame.
    #[test]
    fn scrollback_navigation_rotates_the_rings_and_rebuilds_the_exposed_rows() {
        let palette = ColorPalette::default();
        let style = plain_style(&palette);
        let mut glyphs = SyntheticGlyphs::default();
        let mut term = terminal(6, 20);
        for row in 0..30 {
            term.advance_bytes(format!("row {row}\r\n"));
        }
        let top = terminal_get_dimensions(&mut term).physical_top;
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        let mut at = |viewport_top: StableRowIndex,
                      scene: &mut MetalScene,
                      mirror: &mut RenderMirror,
                      term: &mut Terminal| {
            let request = CaptureRequest {
                viewport_top: Some(viewport_top),
                rules: &[],
                rules_generation: 0,
            };
            capture_terminal_rows(term, mirror, &request).expect("resident");
            let update = scene.update(mirror, &style, &no_selection, &mut glyphs);
            let mut full = MetalScene::new();
            full.update(
                &reference_mirror(term, Some(viewport_top)),
                &style,
                &no_selection,
                &mut glyphs,
            );
            assert_eq!(difference(scene, &full), None, "at {viewport_top}");
            update
        };
        let (s, m, t) = (&mut scene, &mut mirror, &mut term);
        assert!(at(top, s, m, t).full);
        let back = at(top - 2, s, m, t);
        assert_eq!(
            (back.full, back.scrolled_back, back.scrolled),
            (false, 2, 0)
        );
        assert!(back.rows_rebuilt <= 3, "{back:?}");
        let back = at(top - 5, s, m, t);
        assert_eq!((back.scrolled_back, back.rows_rebuilt), (3, 3));
        let forward = at(top - 4, s, m, t);
        assert_eq!((forward.scrolled, forward.rows_rebuilt), (1, 1));
    }

    /// Text is hidden only in the color it actually sits on, as the WebGpu
    /// renderer decides: the selection's background under selected text, a
    /// focused block cursor's color under the cursor, else the cell's own
    /// background. The Metal focus-selection scene (ft-yccm0.4.4) lost its
    /// default-colored selected text: dark selection text was compared with
    /// the dark default background it does not sit on.
    #[test]
    fn text_is_hidden_only_in_the_color_it_sits_on() {
        let palette = ColorPalette::default();
        let (dark, light, gray) = (
            palette.background,
            SrgbaTuple(0.7, 0.84, 1.0, 1.0),
            SrgbaTuple(0.75, 0.75, 0.75, 1.0),
        );
        let mut term = terminal(2, 10);
        // "ab" is selected, the cursor is on "c", "d" is plain and "ef" is
        // black on black.
        term.advance_bytes(b"abcd\x1b[30;40mef\x1b[0m\x1b[1;3H");
        let mut mirror = RenderMirror::new();
        let request = CaptureRequest {
            viewport_top: None,
            rules: &[],
            rules_generation: 0,
        };
        capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        let first = mirror.first();
        let selection = move |stable: StableRowIndex| {
            if stable == first { 0..2 } else { 0..0 }
        };
        let drawn_cols = |selection_fg: SrgbaTuple, cursor_fg: SrgbaTuple| {
            let style = SceneStyle {
                selection_fg: Some(selection_fg),
                selection_bg: light,
                cursor_fg: Some(cursor_fg),
                cursor_bg: Some(gray),
                ..plain_style(&palette)
            };
            let mut scene = MetalScene::new();
            scene.update(&mirror, &style, &selection, &mut SyntheticGlyphs::default());
            let mut cols: Vec<u16> = scene.text().row(0).map(|instance| instance.col()).collect();
            cols.dedup();
            cols
        };
        // Dark text on the light selection and on the gray block is drawn,
        // although the cells' own background is dark too.
        assert_eq!(drawn_cols(dark, dark), [0, 1, 2, 3]);
        // Text in the selection's or the block's own color is not.
        assert_eq!(drawn_cols(light, gray), [3]);

        // The block going away (focus lost) brings back the text it hid,
        // and the incremental update rebuilds that row for it.
        let block = SceneStyle {
            cursor_bg: Some(palette.foreground),
            ..plain_style(&palette)
        };
        let mut scene = MetalScene::new();
        let mut glyphs = SyntheticGlyphs::default();
        scene.update(&mirror, &block, &no_selection, &mut glyphs);
        assert!(!scene.text().row(0).any(|instance| instance.col() == 2));
        let update = scene.update(&mirror, &plain_style(&palette), &no_selection, &mut glyphs);
        assert_eq!((update.full, update.rows_rebuilt), (false, 1));
        assert!(scene.text().row(0).any(|instance| instance.col() == 2));
    }

    /// Exact readback (ft-yccm0.4.4): incremental frames and frames built
    /// from full line copies render to identical pixels on the real Metal
    /// pipeline, and a planted missed row renders differently.
    #[cfg(target_os = "macos")]
    mod readback {
        use super::*;
        use frankenterm_renderer_metal::{
            BackgroundUniforms, ClearColor, FrameScene, MetalRenderer, TextUniforms,
        };

        /// Synthetic glyph bitmaps placed in the renderer's real atlases.
        struct AtlasGlyphs<'a> {
            renderer: &'a MetalRenderer,
            placed: HashMap<(String, GlyphStyle, usize), Vec<PlacedGlyph>>,
        }

        impl GlyphSource for AtlasGlyphs<'_> {
            fn glyphs(&mut self, text: &str, style: GlyphStyle, width: usize) -> &[PlacedGlyph] {
                let renderer = self.renderer;
                let index = u32::try_from(self.placed.len()).unwrap_or(u32::MAX);
                self.placed
                    .entry((text.to_string(), style, width))
                    .or_insert_with(|| {
                        let wanted = synthetic_slot(index, text);
                        let (w, h) = (wanted.width, wanted.height);
                        let pixels: Vec<u8> = match wanted.kind {
                            AtlasKind::Grayscale => (0..w * h)
                                .map(|i| ((i * 37 + index * 11) % 251) as u8)
                                .collect(),
                            AtlasKind::Color => (0..w * h)
                                .flat_map(|i| {
                                    let level = ((i * 13 + index * 7) % 256) as u8;
                                    [level / 2, level, level / 3, 255]
                                })
                                .collect(),
                        };
                        let slot = renderer
                            .insert_glyph(wanted.kind, w, h, &pixels)
                            .expect("the atlas takes a small glyph");
                        vec![PlacedGlyph {
                            slot,
                            offset: [1, 2 + (index % 3) as i16],
                        }]
                    })
            }
        }

        fn read_back(renderer: &MetalRenderer, scene: &MetalScene) -> Vec<u8> {
            let extent = scene.cells().extent();
            let frame = FrameScene {
                cells: scene.cells(),
                background: BackgroundUniforms {
                    cell_size: [8.0, 16.0],
                    grid_origin: [0.0, 0.0],
                    row_offset: scene.cells().row_offset(),
                    selection_tint: [0.2, 0.2, 0.5, 0.5],
                    ..BackgroundUniforms::default()
                },
                text: scene.text(),
                text_uniforms: TextUniforms {
                    underline_position: 13.0,
                    line_thickness: 1.0,
                    strikethrough_position: 8.0,
                },
                clear: ClearColor::from_srgba(0.05, 0.05, 0.05, 1.0),
            };
            renderer
                .snapshot_frame(extent.cols * 8, extent.rows * 16, &frame)
                .expect("offscreen frame")
        }

        fn readback_session(seed: u64, plant: bool) -> usize {
            let renderer = MetalRenderer::offscreen().expect("an offscreen Metal renderer");
            let mut glyphs = AtlasGlyphs {
                renderer: &renderer,
                placed: HashMap::new(),
            };
            let mut rng = Rng::new(seed);
            let (rows, cols) = (8, 24);
            let mut term = terminal(rows, cols);
            let mut mirror = RenderMirror::new();
            let mut scene = MetalScene::new();
            let palette = ColorPalette::default();
            let style = SceneStyle {
                palette: &palette,
                generation: 0,
                bold_brightens: true,
                selection_fg: Some(SrgbaTuple(1.0, 1.0, 1.0, 1.0)),
                selection_bg: palette.selection_bg,
                cursor_fg: None,
                cursor_bg: None,
                hover: None,
            };
            let request = CaptureRequest {
                viewport_top: None,
                rules: &[],
                rules_generation: 0,
            };
            let mut differences = 0;
            for step in 0..60 {
                term.advance_bytes(random_output(&mut rng, rows, cols));
                if step % 3 != 0 {
                    continue;
                }
                capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
                let first = mirror.first();
                let selection = move |stable: StableRowIndex| {
                    if stable == first + 1 { 2..9 } else { 0..0 }
                };
                let planted = plant
                    .then(|| usize::try_from(mirror.cursor().y - mirror.first()).ok())
                    .flatten();
                scene.update_rows(&mirror, &style, &selection, &mut glyphs, planted);
                let reference = reference_mirror(&mut term, None);
                let mut full = MetalScene::new();
                full.update(&reference, &style, &selection, &mut glyphs);
                if read_back(&renderer, &scene) != read_back(&renderer, &full) {
                    assert!(
                        plant,
                        "seed {seed} step {step}: incremental frame reads back differently: {:?}",
                        difference(&scene, &full)
                    );
                    differences += 1;
                    scene.invalidate();
                }
            }
            differences
        }

        #[test]
        fn incremental_frames_read_back_identical_to_frames_from_full_line_copies() {
            for seed in 0..3 {
                assert_eq!(readback_session(seed, false), 0);
            }
        }

        #[test]
        fn a_missed_row_rebuild_reads_back_differently() {
            let caught: usize = (0..3).map(|seed| readback_session(seed, true)).sum();
            assert!(caught > 0, "a missed row rebuild read back identically");
        }
    }
}
