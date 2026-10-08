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
//! - rows with blinking text when the blink levels move;
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
//! - underline, strikethrough and overline are one line sprite per cell
//!   ([`LineSprite`]), drawn under the row's glyphs in the underline color
//!   (SGR 58, or else the cell's foreground before selection and cursor
//!   colors), as the WebGpu renderer draws them;
//! - the hovered hyperlink is underlined ([`hovered_underline`]);
//! - blinking text (SGR 5 and 6), and its decorations in its color, fade
//!   toward its background by the window's blink level ([`BlinkLevels`]),
//!   mixed in linear light ([`mix_linear`]).
//!
//! The background pass draws the selection tint and the cursor itself. The
//! caller builds its uniforms.
//!
//! Glyphs come from a [`GlyphSource`]: the GUI's fonts placing them in the
//! renderer's atlases, or a synthetic source in tests.

use frankenterm_renderer_metal::{
    AtlasSlot, CellBg, CellBgGrid, CellText, CellTextGrid, GridExtent, apply_hsb,
};
use mux::render_mirror::{MirrorCell, MirrorColor, MirrorRow, RenderMirror};
use std::ops::Range;
use std::sync::Arc;
use termwiz::cell::{Blink, CellAttributes, Intensity, Underline, unicode_column_width};
use termwiz::cellcluster::CellCluster;
use termwiz::hyperlink::Hyperlink;
use wezterm_term::StableRowIndex;
use wezterm_term::color::{ColorAttribute, ColorPalette, SrgbaTuple};

/// A glyph in an atlas, its top-left `offset` pixels from the cell's
/// top-left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedGlyph {
    pub slot: AtlasSlot,
    pub offset: [i16; 2],
}

/// One glyph of a shaped row (ft-yccm0.4.7.3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    /// The column the glyph starts in: the shaper's cell counts of every
    /// glyph before it in the row, as the WebGpu renderer advances along a
    /// line.
    pub cell: usize,
    /// Placed from the top-left of that cell.
    pub glyph: PlacedGlyph,
    /// The font's brightness adjustment, 1.0 for none.
    pub brightness: f32,
}

/// The lines one cell's decorations draw. Like the WebGpu renderer, the
/// scene draws them as one sprite per cell (ft-yccm0.4.7.3), so curly,
/// dotted and dashed patterns come from the same pixels on both paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineSprite {
    pub underline: Underline,
    pub strikethrough: bool,
    pub overline: bool,
}

impl LineSprite {
    /// Whether the sprite draws anything.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.underline == Underline::None && !self.strikethrough && !self.overline
    }
}

/// The underline of a cell in the hovered hyperlink: a plain cell gets a
/// single underline, a single underline becomes double, and any other style
/// becomes single. The WebGpu renderer applies the same rule.
#[must_use]
pub fn hovered_underline(underline: Underline) -> Underline {
    match underline {
        Underline::Single => Underline::Double,
        _ => Underline::Single,
    }
}

/// Where the glyphs of a frame come from.
pub trait GlyphSource {
    /// The glyphs that draw a row's `clusters` (clustered as the WebGpu
    /// renderer clusters its lines), shaped as it shapes a line
    /// ([`crate::line_shaping`]): each cluster or shaping run with one
    /// shaper call, so ligatures, combining marks and emoji sequences come
    /// out as they do there. Inkless glyphs (spaces, ligature carriers) are
    /// left out; they only advance the cells of the glyphs after them.
    fn shape_row(&mut self, clusters: &[CellCluster]) -> Vec<ShapedGlyph>;

    /// The grayscale sprite that draws `lines` over one cell, placed from
    /// the cell's top-left; `None` when the source has none.
    fn line_sprite(&mut self, lines: LineSprite) -> Option<PlacedGlyph>;

    /// The grayscale sprite of a `shape` cursor `width_cells` cells wide,
    /// placed from its first cell's top-left; `None` when the source has
    /// none.
    fn cursor_sprite(&mut self, shape: CursorSprite, width_cells: u8) -> Option<PlacedGlyph>;
}

/// A cursor the scene draws as the WebGpu renderer's cursor sprite
/// (ft-yccm0.4.7.3): every cursor but a focused block, which the background
/// pass fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorSprite {
    /// An unfocused window's or inactive pane's cursor, whatever its shape.
    HollowBlock,
    /// A focused bar, drawn over the glyphs as on the WebGpu path.
    Bar,
    /// A focused underline.
    Underline,
    /// A compose cursor ([`Compose`]): the WebGpu renderer's sprite for its
    /// default cursor shape, a solid fill across the cursor's cells, under
    /// the glyphs.
    Solid,
}

/// A dead key, an IME composition or the leader key active in the pane,
/// which the WebGpu renderer draws as a compose cursor
/// (`compute_cell_fg_bg` with `dead_key_or_leader` on the active pane): a
/// solid block over the composition, or else over the cursor's cell, shown
/// even where the terminal hides its cursor, with the text in it in the
/// cursor foreground. The visual bell's cursor flash
/// (`VisualBellTarget::CursorColor`) is drawn as the same solid block, in
/// its fading color.
#[derive(Debug, Clone, Copy)]
pub struct Compose<'a> {
    /// The composition (IME preedit) text, overlaid at the cursor in blank
    /// attributes as the WebGpu renderer overlays it on the cursor's line;
    /// `None` while a dead key is held or the leader key is active.
    pub text: Option<&'a str>,
    /// The block's color: `compose_cursor`, or else the cursor color.
    pub color: SrgbaTuple,
    /// The color of the text in the block (the cursor foreground).
    pub fg: SrgbaTuple,
    /// The color that text is not drawn in (the cursor color, which WebGpu
    /// gives as the text's background there).
    pub under: SrgbaTuple,
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
    /// Any other cursor, drawn as a sprite in its color (the cursor color
    /// when focused, the cursor border otherwise); `None` for a focused block
    /// or no cursor.
    pub cursor_sprite: Option<(CursorSprite, SrgbaTuple)>,
    /// The hyperlink under the mouse, underlined wherever it appears.
    pub hover: Option<&'a Arc<Hyperlink>>,
    /// Where blinking text is in its blink. Only the rows with blinking
    /// text are rebuilt when it moves.
    pub blink: BlinkLevels,
    /// A compose cursor in place of the cursor above; `None` for none.
    pub compose: Option<Compose<'a>>,
    /// `text_min_contrast_ratio`: text is moved to at least this contrast
    /// with the color it sits on, as WebGpu's `ensure_min_contrast` moves
    /// it; `None` for no minimum.
    pub min_contrast: Option<f32>,
    /// `reverse_video_cursor_min_contrast`, when `force_reverse_video_cursor`
    /// is set and the pane keeps the window's cursor colors: text under a
    /// focused block or compose cursor whose colors have at least this
    /// contrast is drawn in its own background, as WebGpu's
    /// `use_reverse_video_cursor` draws it. The cursor's own color is the
    /// caller's ([`cursor_attr_colors`]).
    pub reverse_video_cursor: Option<f32>,
    /// A blinking focused cursor's blink level (WebGpu's cursor
    /// `intensity_continuous`). Text under a block is drawn that far from
    /// the cursor foreground toward its own, as WebGpu's shader mixes it;
    /// the caller mixes the cursor's own color.
    pub cursor_blink: Option<f32>,
}

/// The foreground and background of the cell at `col` of `row` as the
/// WebGpu renderer resolves them for its cursor: from the attributes alone
/// (bold brightening the eight ANSI colors when configured, but no reverse
/// or blink), or the palette's own where there is no cell.
#[must_use]
pub fn cursor_attr_colors(
    row: Option<&MirrorRow>,
    col: usize,
    palette: &ColorPalette,
    bold_brightens: bool,
) -> (SrgbaTuple, SrgbaTuple) {
    let cell = row.and_then(|row| row.cells().iter().find(|cell| cell.col() == col));
    match cell {
        Some(cell) => (
            resolved_fg(cell, palette, bold_brightens),
            palette.resolve_bg(cell.bg().to_attribute()),
        ),
        None => (palette.foreground, palette.background),
    }
}

/// Whether WebGpu's reverse-video cursor applies to colors `fg` on `bg`:
/// their contrast reaches `min_contrast`.
#[must_use]
pub fn reverse_video_cursor_applies(fg: SrgbaTuple, bg: SrgbaTuple, min_contrast: f32) -> bool {
    fg.contrast_ratio(&bg) >= min_contrast
}

/// How visible blinking text is (ft-yccm0.4.7.3): the WebGpu renderer's
/// `intensity_continuous` of the window's slow (SGR 5) and rapid (SGR 6)
/// blink states, from 1.0 (shown) to 0.0 (in its background color). `None`
/// where that blink rate is 0, which draws the text as if it did not blink.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BlinkLevels {
    pub slow: Option<f32>,
    pub rapid: Option<f32>,
}

impl BlinkLevels {
    fn level(self, blink: Blink) -> Option<f32> {
        match blink {
            Blink::None => None,
            Blink::Slow => self.slow,
            Blink::Rapid => self.rapid,
        }
    }
}

/// The blinking text of a built row: which blinks it has, and the levels
/// of those it was drawn at (as bits, so rows compare exactly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowBlink {
    slow: bool,
    rapid: bool,
    levels: [Option<u32>; 2],
}

impl RowBlink {
    /// The blinks `row`'s text has, `None` for none.
    fn of(row: &MirrorRow) -> Option<Self> {
        let has = |blink| row.cells().iter().any(|cell| cell.blink() == blink);
        let (slow, rapid) = (has(Blink::Slow), has(Blink::Rapid));
        (slow || rapid).then_some(Self {
            slow,
            rapid,
            levels: [None, None],
        })
    }

    /// This row's blinks at `levels`.
    fn at(self, levels: BlinkLevels) -> Self {
        let bits = |has: bool, level: Option<f32>| level.filter(|_| has).map(f32::to_bits);
        Self {
            levels: [bits(self.slow, levels.slow), bits(self.rapid, levels.rapid)],
            ..self
        }
    }
}

/// `from` moved `amount` of the way to `to`, mixed in linear light as the
/// WebGpu renderer mixes its linear colors, with `from`'s alpha.
/// At `amount` 0 it is `from` exactly, as WebGpu's sum is, so text faded
/// all the way stays equal to its background (which the minimum contrast
/// leaves alone).
#[must_use]
pub fn mix_linear(from: SrgbaTuple, to: SrgbaTuple, amount: f32) -> SrgbaTuple {
    use frankenterm_renderer_metal::color::{linear_to_srgb, srgb_to_linear};
    if amount == 0.0 {
        return from;
    }
    let mix = |from: f32, to: f32| {
        let from = srgb_to_linear(from);
        linear_to_srgb(from + (srgb_to_linear(to) - from) * amount)
    };
    SrgbaTuple(
        mix(from.0, to.0),
        mix(from.1, to.1),
        mix(from.2, to.2),
        from.3,
    )
}

/// How the cursor draws on its row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RowCursor {
    col: usize,
    /// The columns from `col` the cursor covers: one, or a compose cursor's
    /// width.
    cols: usize,
    /// A focused block's (or compose cursor's) text color override.
    fg: Option<SrgbaTuple>,
    /// A focused block's (or compose cursor's) color, which the text under
    /// it sits on.
    block: Option<SrgbaTuple>,
    /// Any other cursor's sprite and color.
    sprite: Option<(CursorSprite, [u8; 4])>,
    /// A compose cursor's composition text, overlaid at `col`.
    composing: Option<String>,
    /// The reverse-video cursor's minimum contrast (as bits), for a
    /// focused block or compose cursor ([`SceneStyle::reverse_video_cursor`]).
    reverse: Option<u32>,
    /// A blinking focused block's blink level (as bits)
    /// ([`SceneStyle::cursor_blink`]).
    blink: Option<u32>,
}

impl RowCursor {
    /// The compose cursor at `col` of `row`, which covers the composition's
    /// columns or, without one (or with one of no width), the cell at `col`,
    /// as the WebGpu renderer's cursor range does.
    fn compose(compose: &Compose<'_>, col: usize, row: &MirrorRow, reverse: Option<f32>) -> Self {
        let width = compose
            .text
            .map_or(0, |text| unicode_column_width(text, None));
        let cols = if width > 0 {
            width
        } else {
            row.cells()
                .iter()
                .find(|cell| cell.col() == col)
                .map_or(1, |cell| cell.width().max(1))
        };
        Self {
            col,
            cols,
            fg: Some(compose.fg),
            block: Some(compose.under),
            sprite: Some((CursorSprite::Solid, rgba8(compose.color))),
            composing: compose.text.map(str::to_string),
            reverse: reverse.map(f32::to_bits),
            blink: None,
        }
    }

    fn covers(&self, col: usize) -> bool {
        (self.col..self.col + self.cols).contains(&col)
    }

    /// The text color, and the color under it, for text in `fg` on `bg`
    /// under this cursor: swapped when WebGpu's reverse-video cursor applies
    /// to them, or else the cursor's overrides (`None` keeps the text's
    /// own).
    fn text_colors(
        &self,
        fg: SrgbaTuple,
        bg: SrgbaTuple,
    ) -> (Option<SrgbaTuple>, Option<SrgbaTuple>) {
        let reversed = self
            .reverse
            .map(f32::from_bits)
            .is_some_and(|min| reverse_video_cursor_applies(fg, bg, min));
        if reversed {
            (Some(bg), Some(fg))
        } else {
            (self.fg, self.block)
        }
    }

    /// For text in `fg` under a blinking block: the color its drawn color
    /// moves toward, and how far (WebGpu's `fg_color_alt` and
    /// `fg_color_mix`).
    fn blink_mix(&self, fg: SrgbaTuple) -> Option<(SrgbaTuple, f32)> {
        self.blink.map(|level| (fg, f32::from_bits(level)))
    }

    /// The columns the composition text took when overlaid.
    fn composed(&self) -> Range<usize> {
        let width = self
            .composing
            .as_deref()
            .map_or(0, |text| unicode_column_width(text, None));
        self.col..self.col + width
    }
}

/// What one rebuilt row was built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BuiltRow {
    stable: Option<StableRowIndex>,
    generation: u64,
    selection: Range<usize>,
    /// The cursor, when it is on this row.
    cursor: Option<RowCursor>,
    /// The row's blinking text, and the levels it was drawn at; `None` when
    /// the row has none.
    blink: Option<RowBlink>,
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

/// `cell`'s foreground from its attribute, with bold brightening the eight
/// ANSI colors when configured, as WebGpu's `resolve_fg_color_attr` does.
fn resolved_fg(cell: &MirrorCell, palette: &ColorPalette, bold_brightens: bool) -> SrgbaTuple {
    match cell.fg() {
        MirrorColor::Palette(index)
            if index < 8 && bold_brightens && cell.intensity() == Intensity::Bold =>
        {
            palette.resolve_fg(ColorAttribute::PaletteIndex(index + 8))
        }
        color => palette.resolve_fg(color.to_attribute()),
    }
}

fn cell_colors(cell: &MirrorCell, style: &SceneStyle<'_>, reverse_video: bool) -> CellColors {
    let palette = style.palette;
    let fg = resolved_fg(cell, palette, style.bold_brightens);
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

/// A line sprite's color under the selection's tint (`selection`, opaque,
/// at alpha `tint`). The WebGpu renderer blends the tint over the sprite
/// and the cell; tinting the sprite's color, over a cell the background
/// pass tinted, gives the same blend in linear light:
/// `(bg (1 - c) + u c)(1 - a) + s a` is `bg' (1 - c) + u' c` with
/// `bg' = bg (1 - a) + s a` and `u' = u (1 - a) + s a`, for coverage `c`.
fn selection_tinted(color: SrgbaTuple, selection: SrgbaTuple, tint: f32) -> [u8; 4] {
    rgba8(mix_linear(color, selection, tint))
}

/// `cell`'s foreground as the WebGpu renderer draws blinking text: moved
/// from its background toward its foreground by its blink level
/// ([`mix_linear`]), before selection and cursor colors apply. Its
/// decorations in the foreground color blink with it.
fn blinked_fg(cell: &MirrorCell, colors: &CellColors, style: &SceneStyle<'_>) -> SrgbaTuple {
    match style.blink.level(cell.blink()) {
        Some(level) => mix_linear(colors.bg, colors.fg, level),
        None => colors.fg,
    }
}

/// The color `cell`'s text is drawn in, or `None` where it draws no glyph:
/// invisible text, or text in the color it sits on. As in the WebGpu
/// renderer (compute_cell_fg_bg), that is the selection's background for
/// selected text and a focused block cursor's color under it, not the
/// cell's own background.
fn text_color(
    cell: &MirrorCell,
    style: &SceneStyle<'_>,
    built: &BuiltRow,
    reverse_video: bool,
) -> Option<[u8; 4]> {
    let colors = cell_colors(cell, style, reverse_video);
    let col = cell.col();
    let text_fg = blinked_fg(cell, &colors, style);
    let mut fg = text_fg;
    let mut under = colors.bg;
    if built.selection.contains(&col) {
        if let Some(selection_fg) = style.selection_fg {
            fg = selection_fg;
        }
        under = style.selection_bg;
    }
    let mut blink = None;
    if let Some(cursor) = &built.cursor {
        if cursor.covers(col) {
            let (cursor_fg, block) = cursor.text_colors(text_fg, colors.bg);
            if let Some(cursor_fg) = cursor_fg {
                fg = cursor_fg;
            }
            if let Some(block) = block {
                under = block;
            }
            blink = cursor.blink_mix(text_fg);
        }
    }
    if cell.invisible() {
        return None;
    }
    drawn_fg(style, fg, under, blink)
}

/// Text color `fg` on `under` as drawn: moved to the minimum contrast with
/// `under` (WebGpu's `ensure_min_contrast`, which leaves a color equal to
/// `under` alone), and `None` when it then is the color it sits on. Under a
/// blinking block, the color that survives the check is then mixed toward
/// `blink`'s color, as WebGpu's shader mixes it after that check.
fn drawn_fg(
    style: &SceneStyle<'_>,
    fg: SrgbaTuple,
    under: SrgbaTuple,
    blink: Option<(SrgbaTuple, f32)>,
) -> Option<[u8; 4]> {
    let fg = style
        .min_contrast
        .and_then(|ratio| fg.ensure_contrast_ratio(&under, ratio))
        .unwrap_or(fg);
    if rgba8(fg) == rgba8(under) {
        return None;
    }
    Some(rgba8(match blink {
        Some((toward, level)) => mix_linear(fg, toward, level),
        None => fg,
    }))
}

/// `color` with its brightness scaled by `brightness`, as the WebGpu renderer
/// draws a glyph from a font with a brightness adjustment: the HSB
/// brightness of the color in linear light.
fn adjust_brightness(color: [u8; 4], brightness: f32) -> [u8; 4] {
    if (brightness - 1.0).abs() < f32::EPSILON {
        return color;
    }
    let [red, green, blue, alpha_byte] = color;
    let alpha = f32::from(alpha_byte) / 255.0;
    let decode = frankenterm_renderer_metal::color::byte_to_linear;
    let premultiplied = [
        decode(red) * alpha,
        decode(green) * alpha,
        decode(blue) * alpha,
        alpha,
    ];
    let [red, green, blue, alpha] = apply_hsb(premultiplied, [1.0, 1.0, brightness]);
    let encode = |component: f32| {
        let straight = if alpha > 0.0 { component / alpha } else { 0.0 };
        unorm8(frankenterm_renderer_metal::color::linear_to_srgb(straight))
    };
    [encode(red), encode(green), encode(blue), alpha_byte]
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

    /// Whether the built rows have slow (SGR 5) and rapid (SGR 6) blinking
    /// text, which needs frames as its blink level moves.
    #[must_use]
    pub fn blinking(&self) -> (bool, bool) {
        self.built
            .iter()
            .filter_map(|built| built.blink)
            .fold((false, false), |(slow, rapid), blink| {
                (slow || blink.slow, rapid || blink.rapid)
            })
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
        // A compose cursor shows even where the terminal hides its cursor.
        let cursor_row = (cursor.visibility == termwiz::surface::CursorVisibility::Visible
            || style.compose.is_some())
        .then(|| cursor.y - mirror.first())
        .and_then(|row| usize::try_from(row).ok());
        for (row, mirror_row) in rows.iter().enumerate() {
            let previous = &self.built[row];
            let same_cells = previous.stable == Some(mirror_row.stable())
                && previous.generation == mirror_row.generation();
            let wanted = BuiltRow {
                stable: Some(mirror_row.stable()),
                generation: mirror_row.generation(),
                selection: selection(mirror_row.stable()),
                cursor: (cursor_row == Some(row)).then(|| match &style.compose {
                    Some(compose) => RowCursor::compose(
                        compose,
                        cursor.x,
                        mirror_row,
                        style.reverse_video_cursor,
                    ),
                    None => RowCursor {
                        col: cursor.x,
                        cols: 1,
                        fg: style.cursor_fg,
                        block: style.cursor_bg,
                        sprite: style
                            .cursor_sprite
                            .map(|(shape, color)| (shape, rgba8(color))),
                        composing: None,
                        // Only a focused block (which has a block color)
                        // reverses, or blinks, the text under it.
                        reverse: style
                            .cursor_bg
                            .and(style.reverse_video_cursor)
                            .map(f32::to_bits),
                        blink: style.cursor_bg.and(style.cursor_blink).map(f32::to_bits),
                    },
                }),
                // Unchanged cells with blinking text want the current levels;
                // changed cells are rebuilt anyway.
                blink: previous
                    .blink
                    .filter(|_| same_cells)
                    .map(|blink| blink.at(style.blink)),
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
                self.built[row] = BuiltRow {
                    blink: RowBlink::of(mirror_row).map(|blink| blink.at(style.blink)),
                    ..wanted
                };
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
        // Decorations first: the WebGpu renderer draws its line sprites, one
        // per cell, in the layer under every glyph, in the underline color
        // whatever the selection or cursor do to the text, and also under
        // invisible text. It draws the selection's tint over them, where the
        // background pass draws it under them here, so a selected column's
        // sprite takes the tint in its color: the same blend in linear light
        // ([`selection_tinted`]).
        let SrgbaTuple(red, green, blue, tint) = style.selection_bg;
        let selection_opaque = SrgbaTuple(red, green, blue, 1.0);
        for cell in mirror_row.cells() {
            let hovered = style
                .hover
                .is_some_and(|hover| same_link(Some(hover), mirror_row.hyperlink(cell)));
            let lines = LineSprite {
                underline: if hovered {
                    hovered_underline(cell.underline())
                } else {
                    cell.underline()
                },
                strikethrough: cell.strikethrough(),
                overline: cell.overline(),
            };
            if lines.is_empty() {
                continue;
            }
            let Some(sprite) = glyphs.line_sprite(lines) else {
                continue;
            };
            let color = match cell.underline_color() {
                MirrorColor::Default => {
                    let colors = cell_colors(cell, style, reverse_video);
                    blinked_fg(cell, &colors, style)
                }
                color => style.palette.resolve_fg(color.to_attribute()),
            };
            let end = (cell.col() + cell.width().max(1)).min(cols as usize);
            for col in cell.col()..end {
                let color = if built.selection.contains(&col) {
                    selection_tinted(color, selection_opaque, tint)
                } else {
                    rgba8(color)
                };
                let instance =
                    CellText::new(col as u16, color).with_glyph(&sprite.slot, sprite.offset);
                self.text.push(grid_row, instance);
            }
        }
        // A cursor sprite: a hollow block or an underline goes after the
        // decorations (the WebGpu renderer's layer 0), a bar after the glyphs
        // (its layer 2).
        let cursor_sprite = built.cursor.as_ref().and_then(|cursor| {
            let (shape, color) = cursor.sprite?;
            let width = if shape == CursorSprite::Solid {
                cursor.cols.clamp(1, usize::from(u8::MAX))
            } else {
                mirror_row
                    .cells()
                    .iter()
                    .find(|cell| {
                        (cell.col()..cell.col() + cell.width().max(1)).contains(&cursor.col)
                    })
                    .map_or(1, |cell| cell.width().clamp(1, 2))
            };
            let sprite = glyphs.cursor_sprite(shape, width as u8)?;
            let instance =
                CellText::new(cursor.col as u16, color).with_glyph(&sprite.slot, sprite.offset);
            Some((shape, instance))
        });
        if let Some((shape, instance)) = cursor_sprite {
            if shape != CursorSprite::Bar {
                self.text.push(grid_row, instance);
            }
        }
        for cell in mirror_row.cells() {
            let colors = cell_colors(cell, style, reverse_video);
            if let Some(SrgbaTuple(red, green, blue, _)) = colors.explicit_bg {
                let background = CellBg::rgb(unorm8(red), unorm8(green), unorm8(blue));
                if cell.width() > 1 {
                    self.cells.set_wide(grid_row, cell.col() as u32, background);
                } else {
                    self.cells.set(grid_row, cell.col() as u32, background);
                }
            }
        }
        // Glyphs: the row's clusters, made and shaped as the WebGpu renderer
        // makes and shapes them (ft-yccm0.4.7.3). Each glyph takes the
        // colors of the cell it starts in. A composition is overlaid on the
        // cursor's line before clustering, as WebGpu overlays it, and its
        // glyphs take the compose cursor's text color.
        let cells = mirror_row.cells();
        let composing = built
            .cursor
            .as_ref()
            .and_then(|cursor| Some((cursor, cursor.composing.as_deref()?)));
        let clusters = match composing {
            Some((cursor, text)) => {
                let mut line = mirror_row.to_line();
                line.overlay_text_with_attribute(
                    cursor.col,
                    text,
                    CellAttributes::blank(),
                    mirror_row.seqno(),
                );
                line.cluster(None)
            }
            None => mirror_row.clusters(),
        };
        let composed = composing.map_or(0..0, |(cursor, _)| cursor.composed());
        for shaped in glyphs.shape_row(&clusters) {
            let col = shaped.cell;
            if col >= cols as usize {
                break;
            }
            let fg = match composing {
                Some((cursor, _)) if composed.contains(&col) => {
                    // The composition's blank attributes.
                    let palette = style.palette;
                    let (fg, bg) = if reverse_video {
                        (palette.background, palette.foreground)
                    } else {
                        (palette.foreground, palette.background)
                    };
                    let (cursor_fg, block) = cursor.text_colors(fg, bg);
                    drawn_fg(style, cursor_fg.unwrap_or(fg), block.unwrap_or(bg), None)
                }
                _ => {
                    let index =
                        cells.partition_point(|cell| cell.col() + cell.width().max(1) <= col);
                    cells
                        .get(index)
                        .and_then(|cell| text_color(cell, style, built, reverse_video))
                }
            };
            let Some(fg) = fg else {
                continue;
            };
            let fg = adjust_brightness(fg, shaped.brightness);
            let instance =
                CellText::new(col as u16, fg).with_glyph(&shaped.glyph.slot, shaped.glyph.offset);
            self.text.push(grid_row, instance);
        }
        if let Some((CursorSprite::Bar, instance)) = cursor_sprite {
            self.text.push(grid_row, instance);
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

    /// A glyph's synthetic style key: intensity and italic.
    type SyntheticStyle = (u8, bool);

    /// Glyphs with made-up atlas slots, one per (text, style), so the
    /// instance bytes say which glyph each instance draws. Shaping is one
    /// glyph per non-blank cell, except that "->" is one glyph across its
    /// two cells, a synthetic ligature.
    #[derive(Default)]
    struct SyntheticGlyphs {
        shaped: HashMap<(String, SyntheticStyle), Vec<ShapedGlyph>>,
        slots: HashMap<(String, SyntheticStyle), PlacedGlyph>,
        lines: HashMap<LineSprite, PlacedGlyph>,
        cursors: HashMap<(CursorSprite, u8), PlacedGlyph>,
    }

    fn synthetic_style(cluster: &CellCluster) -> SyntheticStyle {
        (cluster.attrs.intensity() as u8, cluster.attrs.italic())
    }

    /// The text of each cell of `cluster`, with its cell counted from the
    /// cluster's first, from the cluster's own byte-to-cell map.
    fn cluster_cells(cluster: &CellCluster) -> Vec<(usize, &str)> {
        let text = cluster.text.as_str();
        let mut cells = Vec::new();
        let mut start = 0;
        for byte in 1..=text.len() {
            if byte == text.len()
                || (text.is_char_boundary(byte)
                    && cluster.byte_to_cell_idx(byte) != cluster.byte_to_cell_idx(start))
            {
                let cell = cluster.byte_to_cell_idx(start) - cluster.first_cell_idx;
                cells.push((cell, &text[start..byte]));
                start = byte;
            }
        }
        cells
    }

    /// Shapes `cluster` the synthetic way, calling `place` once per glyph
    /// text the first time it is seen.
    fn synthetic_shape(
        cluster: &CellCluster,
        mut place: impl FnMut(&str) -> PlacedGlyph,
    ) -> Vec<ShapedGlyph> {
        let cells = cluster_cells(cluster);
        let mut shaped = Vec::new();
        let mut index = 0;
        while index < cells.len() {
            let (cell, text) = cells[index];
            let ligature =
                text == "-" && cells.get(index + 1).is_some_and(|(_, next)| *next == ">");
            let (text, step) = if ligature { ("->", 2) } else { (text, 1) };
            if !text.trim().is_empty() {
                shaped.push(ShapedGlyph {
                    cell,
                    glyph: place(text),
                    brightness: 1.0,
                });
            }
            index += step;
        }
        shaped
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

    /// A row shaped one cluster at a time by `shape_cluster` (its glyphs'
    /// cells counted from the cluster's first), each cluster advancing the
    /// row by its width, which its synthetic cell counts add up to.
    fn synthetic_row(
        clusters: &[CellCluster],
        mut shape_cluster: impl FnMut(&CellCluster) -> Vec<ShapedGlyph>,
    ) -> Vec<ShapedGlyph> {
        let mut row = Vec::new();
        let mut first_cell = 0;
        for cluster in clusters {
            row.extend(shape_cluster(cluster).into_iter().map(|glyph| ShapedGlyph {
                cell: first_cell + glyph.cell,
                ..glyph
            }));
            first_cell += cluster.width;
        }
        row
    }

    impl SyntheticGlyphs {
        fn shape_cluster(&mut self, cluster: &CellCluster) -> Vec<ShapedGlyph> {
            let style = synthetic_style(cluster);
            let key = (cluster.text.clone(), style);
            if !self.shaped.contains_key(&key) {
                let slots = &mut self.slots;
                let shaped = synthetic_shape(cluster, |text| {
                    let index = u32::try_from(slots.len()).unwrap_or(u32::MAX);
                    *slots
                        .entry((text.to_string(), style))
                        .or_insert_with(|| PlacedGlyph {
                            slot: synthetic_slot(index, text),
                            offset: [1, 2 + (index % 3) as i16],
                        })
                });
                self.shaped.insert(key.clone(), shaped);
            }
            self.shaped[&key].clone()
        }
    }

    impl GlyphSource for SyntheticGlyphs {
        fn shape_row(&mut self, clusters: &[CellCluster]) -> Vec<ShapedGlyph> {
            synthetic_row(clusters, |cluster| self.shape_cluster(cluster))
        }

        /// One made-up grayscale slot per line sprite, far from the glyphs'.
        fn line_sprite(&mut self, lines: LineSprite) -> Option<PlacedGlyph> {
            let index = 4096 + u32::try_from(self.lines.len()).unwrap_or(u32::MAX - 4096);
            Some(*self.lines.entry(lines).or_insert_with(|| PlacedGlyph {
                slot: synthetic_slot(index, "line"),
                offset: [0, 0],
            }))
        }

        /// One made-up grayscale slot per cursor shape and width.
        fn cursor_sprite(&mut self, shape: CursorSprite, width_cells: u8) -> Option<PlacedGlyph> {
            let index = 8192 + u32::try_from(self.cursors.len()).unwrap_or(u32::MAX - 8192);
            Some(
                *self
                    .cursors
                    .entry((shape, width_cells))
                    .or_insert_with(|| PlacedGlyph {
                        slot: synthetic_slot(index, "cursor"),
                        offset: [0, 0],
                    }),
            )
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
        // A synthetic ligature: one glyph across two cells.
        "a->b",
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
        "5",
        "6",
        "25",
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
            // The selection's text color is a style color, so it changes
            // with the generation, as SceneStyle requires.
            let selection_fgs = [
                None,
                Some(SrgbaTuple(1.0, 1.0, 1.0, 1.0)),
                Some(palette.background),
                Some(palette.selection_bg),
            ];
            let style = SceneStyle {
                palette: &palette,
                generation,
                bold_brightens: true,
                selection_fg: selection_fgs[generation as usize % selection_fgs.len()],
                selection_bg: palette.selection_bg,
                cursor_fg: (rng.below(2) == 0).then_some(SrgbaTuple(0.0, 0.0, 0.0, 1.0)),
                cursor_bg: (rng.below(2) == 0).then_some(palette.cursor_bg),
                cursor_sprite: *rng.pick(&[
                    None,
                    Some((CursorSprite::HollowBlock, palette.cursor_border)),
                    Some((CursorSprite::Bar, palette.cursor_bg)),
                    Some((CursorSprite::Underline, palette.cursor_bg)),
                ]),
                hover,
                // Blink levels moving between frames, so rows of blinking
                // text are rebuilt exactly when theirs changes.
                blink: BlinkLevels {
                    slow: *rng.pick(&[None, Some(1.0), Some(0.5), Some(0.0)]),
                    rapid: *rng.pick(&[None, Some(1.0), Some(0.25)]),
                },
                // Compositions coming, changing and going, wide and narrow,
                // and the leader key's cursor without one.
                compose: (rng.below(4) == 0).then(|| Compose {
                    text: *rng.pick(&[None, Some("x"), Some("日本"), Some("e\u{301}")]),
                    color: palette.cursor_bg,
                    fg: *rng.pick(&[palette.cursor_fg, palette.cursor_bg]),
                    under: palette.cursor_bg,
                }),
                // A configuration change (a new generation) may set a minimum
                // contrast.
                min_contrast: (generation % 2 == 1).then_some(4.5),
                // And the reverse-video cursor.
                reverse_video_cursor: (generation % 3 == 1).then_some(2.5),
                // The cursor blinking between frames.
                cursor_blink: *rng.pick(&[None, Some(0.0), Some(0.5), Some(1.0)]),
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
            cursor_sprite: None,
            hover: None,
            blink: BlinkLevels::default(),
            compose: None,
            min_contrast: None,
            reverse_video_cursor: None,
            cursor_blink: None,
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

    #[test]
    fn a_hovered_link_promotes_its_underline_as_webgpu_does() {
        assert_eq!(hovered_underline(Underline::None), Underline::Single);
        assert_eq!(hovered_underline(Underline::Single), Underline::Double);
        for other in [
            Underline::Double,
            Underline::Curly,
            Underline::Dotted,
            Underline::Dashed,
        ] {
            assert_eq!(hovered_underline(other), Underline::Single, "{other:?}");
        }
        let plain = LineSprite {
            underline: Underline::None,
            strikethrough: false,
            overline: false,
        };
        assert!(plain.is_empty());
        assert!(
            !LineSprite {
                overline: true,
                ..plain
            }
            .is_empty()
        );
    }

    /// ft-yccm0.4.7.3: decorations encode as the WebGpu renderer draws them,
    /// one line sprite per cell (two for a wide cell) pushed before every
    /// glyph of the row. The sprite is in the underline color: SGR 58, or else
    /// the cell's foreground after reverse. Invisible text keeps its
    /// underline.
    #[test]
    fn decorations_are_one_line_sprite_per_cell_under_the_glyphs() {
        let palette = ColorPalette::default();
        let style = plain_style(&palette);
        let mut glyphs = SyntheticGlyphs::default();
        let mut term = terminal(2, 12);
        // a b: red curly; c: strikethrough and overline; d: reversed and
        // underlined; a wide underlined character; h: invisible, underlined.
        term.advance_bytes(
            "\x1b[4:3;58;5;196mab\x1b[0m \x1b[9;53mc\x1b[0m\x1b[7;4md\x1b[0m\x1b[4m你\x1b[0m\x1b[8;4mh\x1b[0m",
        );
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        step(
            &mut term,
            &mut mirror,
            &mut scene,
            &mut glyphs,
            &no_selection,
            &style,
        );
        let row: Vec<CellText> = scene.text().row(0).collect();
        let lines = |underline, strikethrough, overline| LineSprite {
            underline,
            strikethrough,
            overline,
        };
        let origin = |lines: LineSprite| {
            let slot = glyphs.lines[&lines].slot;
            [
                u16::try_from(slot.x).unwrap(),
                u16::try_from(slot.y).unwrap(),
            ]
        };
        let red = rgba8(palette.resolve_fg(ColorAttribute::PaletteIndex(196)));
        let fg = rgba8(palette.foreground);
        let reversed = rgba8(palette.background);
        let curly = origin(lines(Underline::Curly, false, false));
        let struck = origin(lines(Underline::None, true, true));
        let single = origin(lines(Underline::Single, false, false));
        let sprites: Vec<(u16, [u16; 2], [u8; 4])> = row[..7]
            .iter()
            .map(|instance| (instance.col(), instance.atlas_origin(), instance.fg()))
            .collect();
        assert_eq!(
            sprites,
            [
                (0, curly, red),
                (1, curly, red),
                (3, struck, fg),
                (4, single, reversed),
                (5, single, fg),
                (6, single, fg),
                (7, single, fg),
            ]
        );
        assert!(row[..7].iter().all(|instance| instance.offset() == [0, 0]));
        // Then the glyphs: a, b, c, d and the wide character; none for the
        // blank or the invisible cell.
        let glyph_cols: Vec<u16> = row[7..].iter().map(CellText::col).collect();
        assert_eq!(glyph_cols, [0, 1, 3, 4, 5]);
        assert!(
            row.iter().all(|instance| instance.flags()
                & !frankenterm_renderer_metal::cell_text::flags::ATLAS_MASK
                == 0),
            "no procedural decoration flags"
        );
    }

    /// ft-yccm0.4.7.3: glyphs come from shaping whole clusters, as on the
    /// WebGpu path. A ligature across two cells is one glyph at its first
    /// cell, the glyphs after it start where the shaper's cell counts put
    /// them, and an attribute change starts a new cluster, so no ligature
    /// forms across it.
    #[test]
    fn clusters_are_shaped_whole_and_glyphs_start_at_their_cells() {
        let palette = ColorPalette::default();
        let style = plain_style(&palette);
        let mut glyphs = SyntheticGlyphs::default();
        let mut term = terminal(2, 12);
        // "a->b " is one cluster; the "-" and the bold ">" after it are two.
        term.advance_bytes("a->b -\x1b[1m>\x1b[0m");
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        step(
            &mut term,
            &mut mirror,
            &mut scene,
            &mut glyphs,
            &no_selection,
            &style,
        );
        let texts: Vec<String> = mirror.rows()[0]
            .clusters()
            .iter()
            .map(|cluster| cluster.text.clone())
            .collect();
        assert_eq!(texts, ["a->b ", "-", ">"]);
        let row: Vec<(u16, [u16; 2])> = scene
            .text()
            .row(0)
            .map(|instance| (instance.col(), instance.atlas_origin()))
            .collect();
        let origin = |text: &str, style: SyntheticStyle| {
            let slot = glyphs.slots[&(text.to_string(), style)].slot;
            [
                u16::try_from(slot.x).unwrap(),
                u16::try_from(slot.y).unwrap(),
            ]
        };
        let plain = (Intensity::Normal as u8, false);
        let bold = (Intensity::Bold as u8, false);
        assert_eq!(
            row,
            [
                (0, origin("a", plain)),
                (1, origin("->", plain)),
                (3, origin("b", plain)),
                (5, origin("-", plain)),
                (6, origin(">", bold)),
            ]
        );
    }

    /// ft-yccm0.4.7.3: every cursor but a focused block is the WebGpu
    /// renderer's cursor sprite in its color, as wide as the cell under it.
    /// A hollow block or an underline sits under the row's glyphs, a bar
    /// over them. A new cursor shape rebuilds the cursor's row only.
    #[test]
    fn cursor_sprites_draw_in_webgpu_layer_order() {
        let palette = ColorPalette::default();
        let mut glyphs = SyntheticGlyphs::default();
        let mut term = terminal(2, 10);
        // The cursor goes to the wide character in columns 2 and 3.
        term.advance_bytes("ab你\x1b[1;3H");
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        let border = rgba8(palette.cursor_border);
        let mut frame = |sprite: Option<(CursorSprite, SrgbaTuple)>,
                         glyphs: &mut SyntheticGlyphs| {
            let style = SceneStyle {
                cursor_sprite: sprite,
                ..plain_style(&palette)
            };
            let update = step(
                &mut term,
                &mut mirror,
                &mut scene,
                glyphs,
                &no_selection,
                &style,
            );
            let row: Vec<(u16, [u16; 2], [u8; 4])> = scene
                .text()
                .row(0)
                .map(|instance| (instance.col(), instance.atlas_origin(), instance.fg()))
                .collect();
            (update, row)
        };
        let (_, plain) = frame(None, &mut glyphs);
        assert_eq!(plain.len(), 3, "a, b and the wide character");
        for (shape, first) in [
            (CursorSprite::HollowBlock, true),
            (CursorSprite::Underline, true),
            (CursorSprite::Bar, false),
        ] {
            let (update, row) = frame(Some((shape, palette.cursor_border)), &mut glyphs);
            assert_eq!(
                (update.full, update.rows_rebuilt),
                (false, 1),
                "{shape:?}: only the cursor's row"
            );
            let slot = glyphs.cursors[&(shape, 2)].slot;
            let sprite = (
                2,
                [
                    u16::try_from(slot.x).unwrap(),
                    u16::try_from(slot.y).unwrap(),
                ],
                border,
            );
            assert_eq!(row.len(), 4, "{shape:?}");
            if first {
                assert_eq!(row[0], sprite, "{shape:?} under the glyphs");
                assert_eq!(row[1..], plain[..], "{shape:?}");
            } else {
                assert_eq!(row[3], sprite, "{shape:?} over the glyphs");
                assert_eq!(row[..3], plain[..], "{shape:?}");
            }
        }
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

    /// ft-yccm0.4.7.3: the WebGpu renderer draws the selection's tint over
    /// decorations, so a selected column's line sprite carries the tint in
    /// its color; an unselected one keeps the underline color.
    #[test]
    fn selected_decorations_carry_the_selection_tint() {
        let palette = ColorPalette::default();
        let mut term = terminal(2, 10);
        term.advance_bytes(b"\x1b[4mabcd\x1b[24m\x1b[?25l");
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        let mut glyphs = SyntheticGlyphs::default();
        let request = CaptureRequest {
            viewport_top: None,
            rules: &[],
            rules_generation: 0,
        };
        capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        let first = mirror.first();
        let selection = move |stable: StableRowIndex| {
            if stable == first { 1..3 } else { 0..0 }
        };
        let style = plain_style(&palette);
        scene.update(&mirror, &style, &selection, &mut glyphs);
        let lines: Vec<(u16, [u8; 4])> = scene
            .text()
            .row(0)
            .filter(|instance| instance.atlas_origin()[1] >= 1536)
            .map(|instance| (instance.col(), instance.fg()))
            .collect();
        let SrgbaTuple(red, green, blue, alpha) = palette.selection_bg;
        assert!(alpha > 0.0 && alpha < 1.0);
        let tinted = rgba8(mix_linear(
            palette.foreground,
            SrgbaTuple(red, green, blue, 1.0),
            alpha,
        ));
        let fg = rgba8(palette.foreground);
        assert_ne!(tinted, fg);
        assert_eq!(lines, [(0, fg), (1, tinted), (2, tinted), (3, fg)]);
    }

    /// ft-yccm0.4.7.3: with `text_min_contrast_ratio`, low-contrast text is
    /// moved toward the minimum contrast with what it sits on, by the same
    /// `ensure_contrast_ratio` WebGpu applies (light gray on white, which it
    /// darkens). Text faded all the way by its blink is its background
    /// exactly, which the minimum leaves alone, so it stays hidden.
    #[test]
    fn a_minimum_contrast_lifts_dim_text_but_not_faded_blink() {
        let palette = ColorPalette::default();
        let mut term = terminal(2, 10);
        term.advance_bytes(
            b"\x1b[38;2;150;150;150;48;2;255;255;255mab\x1b[0m \x1b[5mcd\x1b[0m\x1b[?25l",
        );
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        let mut glyphs = SyntheticGlyphs::default();
        let style = SceneStyle {
            min_contrast: Some(4.5),
            blink: BlinkLevels {
                slow: Some(0.0),
                rapid: None,
            },
            ..plain_style(&palette)
        };
        step(
            &mut term,
            &mut mirror,
            &mut scene,
            &mut glyphs,
            &no_selection,
            &style,
        );
        let cell = &mirror.rows()[0].cells()[0];
        let dim = palette.resolve_fg(cell.fg().to_attribute());
        let white = palette.resolve_bg(cell.bg().to_attribute());
        let lifted = dim
            .ensure_contrast_ratio(&white, 4.5)
            .expect("light gray on white is below 4.5");
        assert_ne!(rgba8(lifted), rgba8(dim));
        let row: Vec<(u16, [u8; 4])> = scene
            .text()
            .row(0)
            .map(|instance| (instance.col(), instance.fg()))
            .collect();
        assert_eq!(row, [(0, rgba8(lifted)), (1, rgba8(lifted))]);

        // Without the minimum the dim text keeps its color.
        let mut plain = MetalScene::new();
        plain.update(
            &mirror,
            &SceneStyle {
                min_contrast: None,
                ..style
            },
            &no_selection,
            &mut glyphs,
        );
        assert!(
            plain
                .text()
                .row(0)
                .all(|instance| instance.fg() == rgba8(dim))
        );
    }

    /// ft-yccm0.4.7.3: text under a blinking block moves from the cursor
    /// foreground toward its own by the cursor's blink level, as WebGpu's
    /// shader mixes it, while whether it is drawn at all is decided on the
    /// cursor foreground. A new level rebuilds only the cursor's row.
    #[test]
    fn text_under_a_blinking_block_moves_toward_its_own_color() {
        let palette = ColorPalette::default();
        let cursor_fg = SrgbaTuple(0.2, 0.4, 0.6, 1.0);
        let mut term = terminal(2, 10);
        term.advance_bytes(b"ab\r\ncd\x1b[1;1H");
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        let mut glyphs = SyntheticGlyphs::default();
        let blinking = |level| SceneStyle {
            cursor_fg: Some(cursor_fg),
            cursor_bg: Some(palette.cursor_bg),
            cursor_blink: level,
            ..plain_style(&palette)
        };
        let at_cursor = |scene: &MetalScene| {
            scene
                .text()
                .row(0)
                .find(|instance| instance.col() == 0)
                .map(|instance| instance.fg())
        };
        step(
            &mut term,
            &mut mirror,
            &mut scene,
            &mut glyphs,
            &no_selection,
            &blinking(Some(0.0)),
        );
        assert_eq!(at_cursor(&scene), Some(rgba8(cursor_fg)));
        let update = scene.update(&mirror, &blinking(Some(0.5)), &no_selection, &mut glyphs);
        assert_eq!((update.full, update.rows_rebuilt), (false, 1));
        assert_eq!(
            at_cursor(&scene),
            Some(rgba8(mix_linear(cursor_fg, palette.foreground, 0.5)))
        );
        scene.update(&mirror, &blinking(Some(1.0)), &no_selection, &mut glyphs);
        assert_eq!(at_cursor(&scene), Some(rgba8(palette.foreground)));

        // Text in the block's color stays hidden whatever the level.
        let hidden = SceneStyle {
            cursor_fg: Some(palette.cursor_bg),
            ..blinking(Some(0.5))
        };
        scene.update(&mirror, &hidden, &no_selection, &mut glyphs);
        assert_eq!(at_cursor(&scene), None);
    }

    /// ft-yccm0.4.7.3: a scene says which blinks its rows have, so the
    /// window draws frames for them only while there are some.
    #[test]
    fn a_scene_reports_which_blinks_its_rows_have() {
        let palette = ColorPalette::default();
        let blinks_of = |bytes: &[u8]| {
            let mut term = terminal(3, 10);
            term.advance_bytes(bytes);
            let mut mirror = RenderMirror::new();
            let mut scene = MetalScene::new();
            step(
                &mut term,
                &mut mirror,
                &mut scene,
                &mut SyntheticGlyphs::default(),
                &no_selection,
                &plain_style(&palette),
            );
            scene.blinking()
        };
        assert_eq!(blinks_of(b"plain"), (false, false));
        assert_eq!(blinks_of(b"\x1b[5mslow"), (true, false));
        assert_eq!(blinks_of(b"\x1b[6mrapid"), (false, true));
        assert_eq!(blinks_of(b"\x1b[5ma\r\n\x1b[6mb"), (true, true));
    }

    /// ft-yccm0.4.7.3: WebGpu's reverse-video cursor. Under a focused block,
    /// text whose colors have the minimum contrast is drawn in its own
    /// background; text below it keeps the cursor foreground. The cursor's
    /// own color comes from the cell's attribute colors, unreversed.
    #[test]
    fn a_reverse_video_cursor_draws_contrasting_text_in_its_background() {
        let palette = ColorPalette::default();
        let cursor_fg = SrgbaTuple(0.2, 0.4, 0.6, 1.0);
        let block = SceneStyle {
            cursor_fg: Some(cursor_fg),
            cursor_bg: Some(palette.cursor_bg),
            reverse_video_cursor: Some(2.5),
            ..plain_style(&palette)
        };
        let drawn_at_cursor = |bytes: &[u8], style: &SceneStyle<'_>| {
            let mut term = terminal(2, 10);
            term.advance_bytes(bytes);
            let mut mirror = RenderMirror::new();
            let mut scene = MetalScene::new();
            step(
                &mut term,
                &mut mirror,
                &mut scene,
                &mut SyntheticGlyphs::default(),
                &no_selection,
                style,
            );
            scene
                .text()
                .row(0)
                .find(|instance| instance.col() == 0)
                .map(|instance| instance.fg())
        };
        // Light text on black: reversed, so drawn in black.
        assert_eq!(
            drawn_at_cursor(b"ab\x1b[1;1H", &block),
            Some(rgba8(palette.background))
        );
        // Without the option, the cursor foreground.
        let plain_block = SceneStyle {
            reverse_video_cursor: None,
            ..block
        };
        assert_eq!(
            drawn_at_cursor(b"ab\x1b[1;1H", &plain_block),
            Some(rgba8(cursor_fg))
        );
        // Dark gray on black is below the contrast: the cursor foreground.
        assert_eq!(
            drawn_at_cursor(b"\x1b[38;2;20;20;20mab\x1b[1;1H", &block),
            Some(rgba8(cursor_fg))
        );

        // The cursor's color: the attribute colors, before SGR 7 swaps them.
        let mut term = terminal(2, 10);
        term.advance_bytes(b"\x1b[7;31ma\x1b[0m");
        let mut mirror = RenderMirror::new();
        let request = CaptureRequest {
            viewport_top: None,
            rules: &[],
            rules_generation: 0,
        };
        capture_terminal_rows(&mut term, &mut mirror, &request).expect("resident");
        let red = palette.resolve_fg(ColorAttribute::PaletteIndex(1));
        assert_eq!(
            cursor_attr_colors(mirror.rows().first(), 0, &palette, true),
            (red, palette.background)
        );
        assert_eq!(
            cursor_attr_colors(None, 0, &palette, true),
            (palette.foreground, palette.background)
        );
        assert!(reverse_video_cursor_applies(
            palette.foreground,
            palette.background,
            2.5
        ));
        assert!(!reverse_video_cursor_applies(
            palette.background,
            palette.background,
            2.5
        ));
    }

    /// The blink mix is in linear light, as on the WebGpu path: halfway from
    /// black to white is sRGB 188, not 128. The alpha is the background's.
    #[test]
    fn blink_mixes_in_linear_light() {
        let black = SrgbaTuple(0.0, 0.0, 0.0, 1.0);
        let white = SrgbaTuple(1.0, 1.0, 1.0, 0.5);
        assert_eq!(rgba8(mix_linear(black, white, 0.5)), [188, 188, 188, 255]);
        assert_eq!(rgba8(mix_linear(black, white, 0.0)), [0, 0, 0, 255]);
        assert_eq!(rgba8(mix_linear(black, white, 1.0)), [255, 255, 255, 255]);
    }

    /// ft-yccm0.4.7.3: slow (SGR 5) and rapid (SGR 6) blinking text, and its
    /// underline, are drawn at their blink level's mix of foreground and
    /// background; at level 0 the glyphs are hidden like any text in the
    /// color it sits on. A moving level rebuilds only the rows with that
    /// kind of blinking text.
    #[test]
    fn blinking_text_fades_by_its_level_and_rebuilds_only_its_rows() {
        let palette = ColorPalette::default();
        let mut term = terminal(3, 10);
        term.advance_bytes(b"\x1b[5;4mab\x1b[25;24m cd\r\n\x1b[6mxy\x1b[25m\r\nplain");
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        let mut glyphs = SyntheticGlyphs::default();
        let levels = |slow, rapid| SceneStyle {
            blink: BlinkLevels { slow, rapid },
            ..plain_style(&palette)
        };
        // Line sprites have the made-up slots from 4096 on, far below the
        // glyphs' in the synthetic atlas.
        let split = |scene: &MetalScene, row: u32| {
            let (lines, glyphs): (Vec<CellText>, Vec<CellText>) = scene
                .text()
                .row(row)
                .partition(|instance| instance.atlas_origin()[1] >= 1536);
            let colors = |instances: Vec<CellText>| -> Vec<(u16, [u8; 4])> {
                instances
                    .iter()
                    .map(|instance| (instance.col(), instance.fg()))
                    .collect()
            };
            (colors(glyphs), colors(lines))
        };
        let fg = rgba8(palette.foreground);
        let at = |level: f32| rgba8(mix_linear(palette.background, palette.foreground, level));

        // Blinking off: drawn as plain text.
        step(
            &mut term,
            &mut mirror,
            &mut scene,
            &mut glyphs,
            &no_selection,
            &levels(None, None),
        );
        assert_eq!(
            split(&scene, 0),
            (
                vec![(0, fg), (1, fg), (3, fg), (4, fg)],
                vec![(0, fg), (1, fg)]
            )
        );

        // Halfway through a slow blink: only row 0 changes.
        let update = scene.update(
            &mirror,
            &levels(Some(0.5), None),
            &no_selection,
            &mut glyphs,
        );
        assert_eq!((update.full, update.rows_rebuilt), (false, 1));
        assert_ne!(at(0.5), fg);
        assert_eq!(
            split(&scene, 0),
            (
                vec![(0, at(0.5)), (1, at(0.5)), (3, fg), (4, fg)],
                vec![(0, at(0.5)), (1, at(0.5))]
            )
        );

        // Fully faded: the glyphs are hidden; the underline is in the
        // background color.
        scene.update(
            &mirror,
            &levels(Some(0.0), None),
            &no_selection,
            &mut glyphs,
        );
        assert_eq!(at(0.0), rgba8(palette.background));
        assert_eq!(
            split(&scene, 0),
            (vec![(3, fg), (4, fg)], vec![(0, at(0.0)), (1, at(0.0))])
        );

        // The rapid level moves row 1 alone; the same levels rebuild nothing.
        let update = scene.update(
            &mirror,
            &levels(Some(0.0), Some(0.25)),
            &no_selection,
            &mut glyphs,
        );
        assert_eq!((update.full, update.rows_rebuilt), (false, 1));
        assert_eq!(split(&scene, 1).0, vec![(0, at(0.25)), (1, at(0.25))]);
        let update = scene.update(
            &mirror,
            &levels(Some(0.0), Some(0.25)),
            &no_selection,
            &mut glyphs,
        );
        assert_eq!(update.rows_rebuilt, 0);
        assert_eq!(split(&scene, 2).0.len(), 5);
    }

    /// ft-yccm0.4.7.3: an IME composition is overlaid at the cursor in blank
    /// attributes before the row is clustered, as on the WebGpu path, under
    /// a solid compose cursor as wide as the composition, with its text in
    /// the cursor foreground. The terminal hiding its cursor does not hide
    /// a compose cursor. Without a composition (the leader key) the cursor
    /// covers its cell. Only the cursor's row is rebuilt as it changes.
    #[test]
    fn a_composition_is_overlaid_at_the_cursor_under_a_solid_compose_cursor() {
        let palette = ColorPalette::default();
        let mut term = terminal(2, 12);
        // The cursor is on "c", and hidden.
        term.advance_bytes(b"ab\x1b[4mcdefgh\x1b[24m\r\nxyz\x1b[1;3H\x1b[?25l");
        let mut mirror = RenderMirror::new();
        let mut scene = MetalScene::new();
        let mut glyphs = SyntheticGlyphs::default();
        let (fg, compose_fg) = (rgba8(palette.foreground), [10, 20, 30, 255]);
        let compose_color = SrgbaTuple(0.9, 0.3, 0.6, 1.0);
        let composing = |text| SceneStyle {
            compose: Some(Compose {
                text,
                color: compose_color,
                fg: SrgbaTuple(10.0 / 255.0, 20.0 / 255.0, 30.0 / 255.0, 1.0),
                under: palette.cursor_bg,
            }),
            ..plain_style(&palette)
        };
        // Cursor sprites have the made-up slots from 8192 on, and line
        // sprites those from 4096.
        let kinds = |scene: &MetalScene| -> Vec<(char, u16, [u8; 4])> {
            scene
                .text()
                .row(0)
                .map(|instance| {
                    let kind = match instance.atlas_origin()[1] {
                        3072.. => 'c',
                        1536.. => 'l',
                        _ => 'g',
                    };
                    (kind, instance.col(), instance.fg())
                })
                .collect()
        };

        step(
            &mut term,
            &mut mirror,
            &mut scene,
            &mut glyphs,
            &no_selection,
            &composing(Some("日本")),
        );
        let underline: Vec<_> = (2..8).map(|col| ('l', col, fg)).collect();
        let mut expected = underline.clone();
        expected.push(('c', 2, rgba8(compose_color)));
        expected.extend([
            ('g', 0, fg),
            ('g', 1, fg),
            ('g', 2, compose_fg),
            ('g', 4, compose_fg),
            ('g', 6, fg),
            ('g', 7, fg),
        ]);
        assert_eq!(kinds(&scene), expected);
        // The compose cursor is the solid sprite four cells wide.
        assert!(glyphs.cursors.contains_key(&(CursorSprite::Solid, 4)));
        assert!(glyphs.slots.contains_key(&("日".to_string(), (0, false))));

        // A new composition rebuilds the cursor's row alone.
        let update = scene.update(&mirror, &composing(Some("x")), &no_selection, &mut glyphs);
        assert_eq!((update.full, update.rows_rebuilt), (false, 1));
        let mut expected = underline.clone();
        expected.push(('c', 2, rgba8(compose_color)));
        expected.extend([
            ('g', 0, fg),
            ('g', 1, fg),
            ('g', 2, compose_fg),
            ('g', 3, fg),
            ('g', 4, fg),
            ('g', 5, fg),
            ('g', 6, fg),
            ('g', 7, fg),
        ]);
        assert_eq!(kinds(&scene), expected);

        // The leader key: no composition, the cursor covers "c", which is
        // drawn in the cursor foreground.
        scene.update(&mirror, &composing(None), &no_selection, &mut glyphs);
        let row = kinds(&scene);
        assert!(row.contains(&('c', 2, rgba8(compose_color))));
        assert!(row.contains(&('g', 2, compose_fg)));
        assert!(row.contains(&('g', 3, fg)));
        assert!(glyphs.cursors.contains_key(&(CursorSprite::Solid, 1)));

        // No compose state: the hidden cursor draws nothing, and the row is
        // as the terminal has it.
        let update = scene.update(&mirror, &plain_style(&palette), &no_selection, &mut glyphs);
        assert_eq!(update.rows_rebuilt, 1);
        assert!(
            kinds(&scene)
                .iter()
                .all(|(kind, _, color)| *kind != 'c' && *color == fg)
        );
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
            shaped: HashMap<(String, SyntheticStyle), Vec<ShapedGlyph>>,
            slots: HashMap<(String, SyntheticStyle), PlacedGlyph>,
            lines: HashMap<LineSprite, PlacedGlyph>,
            cursors: HashMap<(CursorSprite, u8), PlacedGlyph>,
        }

        impl AtlasGlyphs<'_> {
            /// Synthetic shaping with synthetic bitmaps in the real atlases.
            fn shape_cluster(&mut self, cluster: &CellCluster) -> Vec<ShapedGlyph> {
                let renderer = self.renderer;
                let style = synthetic_style(cluster);
                let key = (cluster.text.clone(), style);
                if !self.shaped.contains_key(&key) {
                    let slots = &mut self.slots;
                    let shaped = synthetic_shape(cluster, |text| {
                        let index = u32::try_from(slots.len()).unwrap_or(u32::MAX);
                        *slots.entry((text.to_string(), style)).or_insert_with(|| {
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
                            PlacedGlyph {
                                slot,
                                offset: [1, 2 + (index % 3) as i16],
                            }
                        })
                    });
                    self.shaped.insert(key.clone(), shaped);
                }
                self.shaped[&key].clone()
            }
        }

        impl GlyphSource for AtlasGlyphs<'_> {
            fn shape_row(&mut self, clusters: &[CellCluster]) -> Vec<ShapedGlyph> {
                synthetic_row(clusters, |cluster| self.shape_cluster(cluster))
            }

            /// A cell-sized (8x16) coverage pattern per line sprite.
            fn line_sprite(&mut self, lines: LineSprite) -> Option<PlacedGlyph> {
                let renderer = self.renderer;
                let index = u32::try_from(self.lines.len()).unwrap_or(u32::MAX);
                Some(*self.lines.entry(lines).or_insert_with(|| {
                    let pixels: Vec<u8> = (0..8 * 16_u32)
                        .map(|i| if (i / 8 + index) % 5 == 0 { 255 } else { 0 })
                        .collect();
                    let slot = renderer
                        .insert_glyph(AtlasKind::Grayscale, 8, 16, &pixels)
                        .expect("the atlas takes a line sprite");
                    PlacedGlyph {
                        slot,
                        offset: [0, 0],
                    }
                }))
            }

            /// A hollow box `width_cells` cells wide, with a half-covered
            /// inner ring, so anti-aliased edges go through the readback.
            fn cursor_sprite(
                &mut self,
                shape: CursorSprite,
                width_cells: u8,
            ) -> Option<PlacedGlyph> {
                let renderer = self.renderer;
                Some(
                    *self.cursors.entry((shape, width_cells)).or_insert_with(|| {
                        let width = 8 * u32::from(width_cells);
                        let pixels: Vec<u8> = (0..width * 16)
                            .map(|i| {
                                let (x, y) = (i % width, i / width);
                                let edge = x.min(width - 1 - x).min(y.min(15 - y));
                                match edge {
                                    0 => 255,
                                    1 => 128,
                                    _ => 0,
                                }
                            })
                            .collect();
                        let slot = renderer
                            .insert_glyph(AtlasKind::Grayscale, width, 16, &pixels)
                            .expect("the atlas takes a cursor sprite");
                        PlacedGlyph {
                            slot,
                            offset: [0, 0],
                        }
                    }),
                )
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
                shaped: HashMap::new(),
                slots: HashMap::new(),
                lines: HashMap::new(),
                cursors: HashMap::new(),
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
                // A cursor sprite through the real pipeline (ft-yccm0.4.7.3).
                cursor_sprite: Some((CursorSprite::HollowBlock, SrgbaTuple(0.9, 0.5, 0.1, 1.0))),
                hover: None,
                blink: BlinkLevels::default(),
                compose: None,
                min_contrast: None,
                reverse_video_cursor: None,
                cursor_blink: None,
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
