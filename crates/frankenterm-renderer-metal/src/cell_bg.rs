//! Cell backgrounds, the platform-independent half (ft-yccm0.4.2.2).
//!
//! Every cell's background is one 4-byte [`CellBg`] entry, `[r, g, b, flags]`,
//! in the frame slot's CellBg buffer: 4 bytes per cell however many colors the
//! screen uses. The flags byte says whether the cell has an explicit color
//! (otherwise it shows the cleared default background) and whether it is
//! selected, a search match or the current search match.
//!
//! Rows form a ring: logical row `r` is stored at ring row
//! `(r + row_offset) % rows`, so a scroll rotates `row_offset` instead of
//! moving every row ([`CellBgGrid::scroll_up`]).
//!
//! The background pass draws one full-screen triangle, so there is no
//! per-cell geometry. Its fragment shader (`src/shaders/background.metal`)
//! finds the cell under each pixel and composites the cell color, the
//! selection and search tints and the cursor. [`shade_background`] is the CPU
//! reference of that shader: the tests' oracle, and the native test compares
//! the GPU's pixels against it.

use crate::frame::{FrameUniforms, GridExtent};
use crate::uploads::RowChanges;

/// The Metal Shading Language source of the background pass.
pub const BACKGROUND_SHADER: &str = include_str!("shaders/background.metal");

/// Flag bits in a [`CellBg`] entry's fourth byte.
pub mod flags {
    /// The cell has an explicit background color; otherwise it shows the
    /// default background.
    pub const COLOR: u8 = 1;
    /// The cell is selected.
    pub const SELECTED: u8 = 2;
    /// The cell is part of a search match.
    pub const SEARCH_MATCH: u8 = 4;
    /// The cell is part of the current search match.
    pub const CURRENT_MATCH: u8 = 8;
}

/// One cell's background entry: straight sRGB red, green, blue and the
/// [`flags`] byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellBg([u8; 4]);

impl CellBg {
    /// The default background: no color, no highlight.
    pub const DEFAULT: Self = Self([0; 4]);

    /// An explicit opaque background color.
    #[must_use]
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self([red, green, blue, flags::COLOR])
    }

    /// This entry with `extra` flag bits set.
    #[must_use]
    pub const fn with_flags(self, extra: u8) -> Self {
        let [red, green, blue, current] = self.0;
        Self([red, green, blue, current | extra])
    }

    #[must_use]
    pub const fn selected(self) -> Self {
        self.with_flags(flags::SELECTED)
    }

    #[must_use]
    pub const fn search_match(self) -> Self {
        self.with_flags(flags::SEARCH_MATCH)
    }

    #[must_use]
    pub const fn current_match(self) -> Self {
        self.with_flags(flags::CURRENT_MATCH)
    }

    /// The bytes the shader reads as a `uchar4`.
    #[must_use]
    pub const fn bytes(self) -> [u8; 4] {
        self.0
    }

    #[must_use]
    pub const fn flags(self) -> u8 {
        self.0[3]
    }

    /// The explicit color, if the cell has one.
    #[must_use]
    pub const fn color(self) -> Option<[u8; 3]> {
        if self.0[3] & flags::COLOR == 0 {
            None
        } else {
            Some([self.0[0], self.0[1], self.0[2]])
        }
    }
}

/// One pane's cell backgrounds, stored as a ring of rows exactly as the
/// shader reads them. [`RowChanges`] tracks which ring rows changed, so a
/// frame slot uploads only those (ft-yccm0.4.2.4); equality compares content.
#[derive(Debug, Clone)]
pub struct CellBgGrid {
    rows: u32,
    cols: u32,
    row_offset: u32,
    bytes: Vec<u8>,
    changes: RowChanges,
}

impl PartialEq for CellBgGrid {
    fn eq(&self, other: &Self) -> bool {
        (self.rows, self.cols, self.row_offset) == (other.rows, other.cols, other.row_offset)
            && self.bytes == other.bytes
    }
}

impl Eq for CellBgGrid {}

impl CellBgGrid {
    /// A grid of default backgrounds.
    #[must_use]
    pub fn new(extent: GridExtent) -> Self {
        let cells = usize::try_from(extent.cells()).unwrap_or(usize::MAX);
        Self {
            rows: extent.rows,
            cols: extent.cols,
            row_offset: 0,
            bytes: vec![0; cells.saturating_mul(4)],
            changes: RowChanges::new(usize::try_from(extent.rows).unwrap_or(usize::MAX)),
        }
    }

    /// Which ring rows changed, and the grid instance's epoch.
    #[must_use]
    pub fn changes(&self) -> &RowChanges {
        &self.changes
    }

    /// Ring row `ring_row`'s bytes, as the CellBg buffer holds them.
    #[must_use]
    pub fn ring_row_bytes(&self, ring_row: usize) -> &[u8] {
        let row = self.cols as usize * 4;
        let start = ring_row.saturating_mul(row).min(self.bytes.len());
        &self.bytes[start..start.saturating_add(row).min(self.bytes.len())]
    }

    #[must_use]
    pub fn extent(&self) -> GridExtent {
        GridExtent {
            rows: self.rows,
            cols: self.cols,
        }
    }

    /// The ring offset the shader adds to every logical row.
    #[must_use]
    pub fn row_offset(&self) -> u32 {
        self.row_offset
    }

    /// Where logical `row` is stored.
    #[must_use]
    pub fn ring_row(&self, row: u32) -> u32 {
        let rows = u64::from(self.rows.max(1));
        let ring = (u64::from(row) + u64::from(self.row_offset)) % rows;
        u32::try_from(ring).expect("a ring row is below the row count")
    }

    fn index(&self, row: u32, col: u32) -> Option<usize> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        let ring = u64::from(self.ring_row(row));
        usize::try_from((ring * u64::from(self.cols) + u64::from(col)) * 4).ok()
    }

    /// The entry of logical `(row, col)`, or `None` outside the grid.
    #[must_use]
    pub fn get(&self, row: u32, col: u32) -> Option<CellBg> {
        let at = self.index(row, col)?;
        let bytes: [u8; 4] = self.bytes[at..at + 4].try_into().ok()?;
        Some(CellBg(bytes))
    }

    /// Sets logical `(row, col)`; returns whether the cell is in the grid.
    pub fn set(&mut self, row: u32, col: u32, background: CellBg) -> bool {
        match self.index(row, col) {
            Some(at) => {
                let bytes = background.bytes();
                if self.bytes[at..at + 4] != bytes {
                    self.bytes[at..at + 4].copy_from_slice(&bytes);
                    self.changes.touch(self.ring_row(row) as usize);
                }
                true
            }
            None => false,
        }
    }

    /// Sets a wide (2-cell) character's background: its cell and the spacer
    /// cell to its right show the same background, so a wide glyph never
    /// sits on a half-default background. A wide character in the last
    /// column has no spacer.
    pub fn set_wide(&mut self, row: u32, col: u32, background: CellBg) -> bool {
        let set = self.set(row, col, background);
        if set && col + 1 < self.cols {
            self.set(row, col + 1, background);
        }
        set
    }

    /// Sets every cell of logical `row`.
    pub fn fill_row(&mut self, row: u32, background: CellBg) {
        for col in 0..self.cols {
            self.set(row, col, background);
        }
    }

    /// Scrolls the content up by `lines`: logical row `r` now shows what row
    /// `r + lines` showed. Only `row_offset` moves; the `lines` rows exposed
    /// at the bottom are reset to the default background.
    pub fn scroll_up(&mut self, lines: u32) {
        if self.rows == 0 {
            return;
        }
        let lines = lines.min(self.rows);
        let rotated = (u64::from(self.row_offset) + u64::from(lines)) % u64::from(self.rows);
        self.row_offset = u32::try_from(rotated).expect("the offset is below the row count");
        for row in self.rows - lines..self.rows {
            self.fill_row(row, CellBg::DEFAULT);
        }
    }

    /// Scrolls the content down by `lines` (scrollback navigation): logical
    /// row `r + lines` now shows what row `r` showed. Only `row_offset`
    /// moves; the `lines` rows exposed at the top are reset to the default
    /// background.
    pub fn scroll_down(&mut self, lines: u32) {
        if self.rows == 0 {
            return;
        }
        let lines = lines.min(self.rows);
        let rows = u64::from(self.rows);
        let rotated = (u64::from(self.row_offset) + rows - u64::from(lines) % rows) % rows;
        self.row_offset = u32::try_from(rotated).expect("the offset is below the row count");
        for row in 0..lines {
            self.fill_row(row, CellBg::DEFAULT);
        }
    }

    /// The ring-ordered bytes for the CellBg buffer.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// How the cursor is drawn in the background pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    /// No cursor (hidden, or blinked off).
    #[default]
    Hidden,
    /// A filled cell.
    Block,
    /// A cell outline `thickness` pixels wide (unfocused windows).
    HollowBlock,
    /// The bottom `thickness` pixels of the cell.
    Underline,
    /// The left `thickness` pixels of the cell.
    Bar,
}

impl CursorShape {
    /// The value the shader switches on.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Hidden => 0,
            Self::Block => 1,
            Self::HollowBlock => 2,
            Self::Underline => 3,
            Self::Bar => 4,
        }
    }
}

/// The cursor as the background pass draws it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CursorUniform {
    pub shape: CursorShape,
    pub col: u32,
    pub row: u32,
    /// 2 over a wide character, else 1.
    pub width_cells: u32,
    /// Outline, underline or bar width in pixels.
    pub thickness: f32,
    /// Premultiplied RGBA.
    pub color: [f32; 4],
}

/// The background pass's part of [`FrameUniforms`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BackgroundUniforms {
    /// Cell size in framebuffer pixels.
    pub cell_size: [f32; 2],
    /// Top-left of the grid in framebuffer pixels: the left and top padding.
    /// Pixels left of or above it, or right of or below the grid (padding,
    /// scrollbar), keep the cleared default background.
    pub grid_origin: [f32; 2],
    /// [`CellBgGrid::row_offset`].
    pub row_offset: u32,
    pub cursor: CursorUniform,
    /// Premultiplied overlays.
    pub selection_tint: [f32; 4],
    pub search_tint: [f32; 4],
    pub current_match_tint: [f32; 4],
}

/// Premultiplied source-over; the text pass's reference blends with it too.
pub(crate) fn over(top: [f32; 4], bottom: [f32; 4]) -> [f32; 4] {
    let keep = 1.0 - top[3];
    [
        top[0] + bottom[0] * keep,
        top[1] + bottom[1] * keep,
        top[2] + bottom[2] * keep,
        top[3] + bottom[3] * keep,
    ]
}

/// The CPU reference of the background fragment shader: the premultiplied
/// linear-light color it writes at framebuffer position `(x, y)` (a pixel
/// center, as Metal's `[[position]]`), or `None` where it discards (outside
/// the grid: padding and scrollbar keep the cleared background).
/// [`to_bgra8`] gives the bytes the sRGB target stores for it.
///
/// `cells` must be the grid the frame uploaded, with
/// `uniforms.grid == cells.extent()` and
/// `uniforms.background.row_offset == cells.row_offset()`.
#[must_use]
// Cell coordinates come from non-negative pixel offsets divided by the cell
// size, truncated exactly as the shader's uint() conversion truncates.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn shade_background(
    uniforms: &FrameUniforms,
    cells: &CellBgGrid,
    x: f32,
    y: f32,
) -> Option<[f32; 4]> {
    let background = &uniforms.background;
    let local = [x - background.grid_origin[0], y - background.grid_origin[1]];
    if local[0] < 0.0 || local[1] < 0.0 {
        return None;
    }
    let col = (local[0] / background.cell_size[0]) as u32;
    let row = (local[1] / background.cell_size[1]) as u32;
    if col >= uniforms.grid.cols || row >= uniforms.grid.rows {
        return None;
    }
    let cell = cells.get(row, col)?;
    // Everything blends in linear light (ft-yccm0.4.7.3): the cell's sRGB
    // bytes are decoded, and the uniform colors are what `to_bytes` writes.
    let linear = crate::color::linear_premultiplied;
    let mut color = match cell.color() {
        Some([red, green, blue]) => [
            crate::color::byte_to_linear(red),
            crate::color::byte_to_linear(green),
            crate::color::byte_to_linear(blue),
            1.0,
        ],
        None => linear(uniforms.clear),
    };
    if cell.flags() & flags::SELECTED != 0 {
        color = over(linear(background.selection_tint), color);
    }
    if cell.flags() & flags::CURRENT_MATCH != 0 {
        color = over(linear(background.current_match_tint), color);
    } else if cell.flags() & flags::SEARCH_MATCH != 0 {
        color = over(linear(background.search_tint), color);
    }
    let cursor = &background.cursor;
    if cursor.shape != CursorShape::Hidden
        && row == cursor.row
        && col >= cursor.col
        && col < cursor.col.saturating_add(cursor.width_cells)
    {
        let cursor_x = local[0] - cursor.col as f32 * background.cell_size[0];
        let cursor_y = local[1] - row as f32 * background.cell_size[1];
        let width = cursor.width_cells as f32 * background.cell_size[0];
        let height = background.cell_size[1];
        let thickness = cursor.thickness;
        let inside = match cursor.shape {
            CursorShape::Hidden => false,
            CursorShape::Block => true,
            CursorShape::HollowBlock => {
                cursor_x < thickness
                    || cursor_y < thickness
                    || cursor_x >= width - thickness
                    || cursor_y >= height - thickness
            }
            CursorShape::Underline => cursor_y >= height - thickness,
            CursorShape::Bar => cursor_x < thickness,
        };
        if inside {
            color = over(linear(cursor.color), color);
        }
    }
    Some(color)
}

/// Inactive-pane dimming (ft-yccm0.4.6): multiplies a premultiplied color's
/// hue, saturation and brightness by `hsb`. It is the CPU reference of the
/// shaders' `apply_hsb`, which is the WebGpu renderer's `apply_hsv` on a
/// premultiplied color; keep the three identical.
#[must_use]
pub fn apply_hsb(color: [f32; 4], hsb: [f32; 3]) -> [f32; 4] {
    let alpha = color[3];
    if alpha <= 0.0 {
        return color;
    }
    let hsv = rgb_to_hsv([color[0] / alpha, color[1] / alpha, color[2] / alpha]);
    let rgb = hsv_to_rgb([hsv[0] * hsb[0], hsv[1] * hsb[1], hsv[2] * hsb[2]]);
    [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha]
}

/// `mix(a, b, t)` for a step `t` of 0 or 1, as the shaders compute it.
fn mix4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

/// The shaders' `step(edge, x)`.
fn step(edge: f32, x: f32) -> f32 {
    if x < edge { 0.0 } else { 1.0 }
}

/// The shaders' `rgb2hsv`.
fn rgb_to_hsv(rgb: [f32; 3]) -> [f32; 3] {
    let konst = [0.0, -1.0 / 3.0, 2.0 / 3.0, -1.0];
    let low = mix4(
        [rgb[2], rgb[1], konst[3], konst[2]],
        [rgb[1], rgb[2], konst[0], konst[1]],
        step(rgb[2], rgb[1]),
    );
    let high = mix4(
        [low[0], low[1], low[3], rgb[0]],
        [rgb[0], low[1], low[2], low[0]],
        step(low[0], rgb[0]),
    );
    let chroma = high[0] - high[3].min(high[1]);
    let epsilon = 1.0e-10;
    [
        (high[2] + (high[3] - high[1]) / (6.0 * chroma + epsilon)).abs(),
        chroma / (high[0] + epsilon),
        high[0],
    ]
}

/// The shaders' `hsv2rgb`; `fract` is `x - floor(x)`, as in Metal.
fn hsv_to_rgb(hsv: [f32; 3]) -> [f32; 3] {
    let konst = [1.0, 2.0 / 3.0, 1.0 / 3.0, 3.0];
    let channel = |offset: f32| {
        let hue = hsv[0] + offset;
        let ramp = ((hue - hue.floor()) * 6.0 - konst[3]).abs();
        hsv[2] * (konst[0] + ((ramp - konst[0]).clamp(0.0, 1.0) - konst[0]) * hsv[1])
    };
    [channel(konst[0]), channel(konst[1]), channel(konst[2])]
}

/// A premultiplied linear-light color, as the shaders write it, in the
/// `BGRA8Unorm_sRGB` bytes the GPU stores (ft-yccm0.4.7.3).
#[must_use]
pub fn to_bgra8(color: [f32; 4]) -> [u8; 4] {
    crate::color::to_srgb_bgra8(color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::SlotBuffer;

    const CELL: [f32; 2] = [8.0, 16.0];
    const ORIGIN: [f32; 2] = [4.0, 2.0];
    const CLEAR: [f32; 4] = [0.1, 0.2, 0.3, 1.0];

    /// What the shader writes for the clear color: linear light.
    fn cleared() -> [f32; 4] {
        crate::color::linear_premultiplied(CLEAR)
    }

    fn uniforms(cells: &CellBgGrid, cursor: CursorUniform) -> FrameUniforms {
        FrameUniforms {
            frame: 7,
            viewport: [96, 103],
            grid: cells.extent(),
            clear: CLEAR,
            background: BackgroundUniforms {
                cell_size: CELL,
                grid_origin: ORIGIN,
                row_offset: cells.row_offset(),
                cursor,
                selection_tint: [0.0, 0.0, 0.25, 0.25],
                search_tint: [0.5, 0.5, 0.0, 0.5],
                current_match_tint: [0.5, 0.25, 0.0, 0.5],
            },
            ..FrameUniforms::default()
        }
    }

    /// The pixel center in the middle of logical cell `(row, col)`.
    #[allow(clippy::cast_precision_loss)]
    fn center(row: u32, col: u32) -> (f32, f32) {
        (
            ORIGIN[0] + (col as f32 + 0.5) * CELL[0],
            ORIGIN[1] + (row as f32 + 0.5) * CELL[1],
        )
    }

    #[test]
    fn entries_pack_color_and_flags_into_four_bytes() {
        assert_eq!(CellBg::DEFAULT.bytes(), [0; 4]);
        assert_eq!(CellBg::DEFAULT.color(), None);
        let red = CellBg::rgb(200, 10, 20);
        assert_eq!(red.bytes(), [200, 10, 20, flags::COLOR]);
        assert_eq!(red.color(), Some([200, 10, 20]));
        let flagged = red.selected().current_match();
        assert_eq!(
            flagged.flags(),
            flags::COLOR | flags::SELECTED | flags::CURRENT_MATCH
        );
        assert_eq!(CellBg::DEFAULT.search_match().color(), None);
        assert_eq!(
            CELL_BG_BYTES,
            crate::frame::CELL_BG_BYTES_PER_CELL,
            "the frame slots size CellBg for this entry"
        );
    }

    const CELL_BG_BYTES: usize = std::mem::size_of::<CellBg>();

    #[test]
    fn the_grid_is_stored_as_a_ring_of_rows() {
        let mut grid = CellBgGrid::new(GridExtent::new(4, 3));
        assert_eq!(grid.as_bytes().len(), 4 * 3 * 4);
        assert!(grid.set(1, 2, CellBg::rgb(1, 2, 3)));
        assert!(!grid.set(4, 0, CellBg::rgb(1, 2, 3)));
        assert!(!grid.set(0, 3, CellBg::rgb(1, 2, 3)));
        assert_eq!(grid.get(1, 2), Some(CellBg::rgb(1, 2, 3)));
        assert_eq!(grid.get(4, 0), None);
        // Row 1, column 2 is byte (1 * 3 + 2) * 4 while the offset is zero.
        assert_eq!(&grid.as_bytes()[20..24], &[1, 2, 3, flags::COLOR]);
    }

    #[test]
    fn scrolling_rotates_the_ring_and_clears_only_the_exposed_rows() {
        let mut grid = CellBgGrid::new(GridExtent::new(4, 2));
        for row in 0..4 {
            grid.fill_row(row, CellBg::rgb(u8::try_from(row).unwrap() + 1, 0, 0));
        }
        let before = grid.as_bytes().to_vec();
        grid.scroll_up(1);
        assert_eq!(grid.row_offset(), 1);
        for row in 0..3 {
            assert_eq!(
                grid.get(row, 0),
                Some(CellBg::rgb(u8::try_from(row).unwrap() + 2, 0, 0)),
                "row {row}"
            );
        }
        assert_eq!(grid.get(3, 1), Some(CellBg::DEFAULT));
        // Only the exposed row's bytes changed: no row moved in memory.
        let changed = before
            .chunks(2 * 4)
            .zip(grid.as_bytes().chunks(2 * 4))
            .filter(|(old, new)| old != new)
            .count();
        assert_eq!(changed, 1);
        grid.scroll_up(3);
        assert_eq!(grid.row_offset(), 0);
        assert!(grid.as_bytes().iter().all(|&byte| byte == 0));
        // Scrolling a whole screen or more clears it.
        grid.fill_row(0, CellBg::rgb(9, 9, 9));
        grid.scroll_up(10);
        assert!(grid.as_bytes().iter().all(|&byte| byte == 0));
        CellBgGrid::new(GridExtent::default()).scroll_up(1);
    }

    #[test]
    fn a_wide_character_gives_its_spacer_the_same_background() {
        let mut grid = CellBgGrid::new(GridExtent::new(1, 4));
        let bg = CellBg::rgb(5, 6, 7);
        assert!(grid.set_wide(0, 1, bg));
        assert_eq!(grid.get(0, 1), Some(bg));
        assert_eq!(grid.get(0, 2), Some(bg), "the spacer cell");
        assert_eq!(grid.get(0, 3), Some(CellBg::DEFAULT));
        // The last column has no spacer.
        assert!(grid.set_wide(0, 3, bg));
        assert!(!grid.set_wide(0, 4, bg));
    }

    #[test]
    fn padding_and_scrollbar_keep_the_cleared_background() {
        let cells = CellBgGrid::new(GridExtent::new(6, 10));
        let u = uniforms(&cells, CursorUniform::default());
        assert_eq!(
            shade_background(&u, &cells, 3.5, 50.0),
            None,
            "left padding"
        );
        assert_eq!(shade_background(&u, &cells, 50.5, 1.5), None, "top padding");
        // The grid ends at 4 + 10 * 8 = 84 px and 2 + 6 * 16 = 98 px.
        assert_eq!(shade_background(&u, &cells, 84.5, 50.5), None, "scrollbar");
        assert_eq!(
            shade_background(&u, &cells, 50.5, 98.5),
            None,
            "bottom padding"
        );
        assert_eq!(shade_background(&u, &cells, 83.5, 97.5), Some(cleared()));
    }

    #[test]
    fn cells_show_their_color_or_the_default_background() {
        let mut cells = CellBgGrid::new(GridExtent::new(6, 10));
        cells.set(2, 3, CellBg::rgb(255, 0, 51));
        let u = uniforms(&cells, CursorUniform::default());
        let (x, y) = center(2, 3);
        // Linear light: sRGB 51 (0.2) is about 3% of the light.
        let blue = crate::color::byte_to_linear(51);
        assert!((blue - 0.033_104_76).abs() < 1e-6, "{blue}");
        assert_eq!(
            shade_background(&u, &cells, x, y),
            Some([1.0, 0.0, blue, 1.0])
        );
        let (x, y) = center(2, 4);
        assert_eq!(shade_background(&u, &cells, x, y), Some(cleared()));
    }

    #[test]
    fn the_ring_offset_maps_logical_rows_like_the_shader() {
        let mut cells = CellBgGrid::new(GridExtent::new(6, 10));
        cells.fill_row(3, CellBg::rgb(0, 255, 0));
        cells.scroll_up(2);
        let u = uniforms(&cells, CursorUniform::default());
        assert_eq!(u.background.row_offset, 2);
        let (x, y) = center(1, 0);
        assert_eq!(
            shade_background(&u, &cells, x, y),
            Some([0.0, 1.0, 0.0, 1.0])
        );
    }

    #[test]
    fn highlights_composite_selection_then_one_search_tint() {
        let mut cells = CellBgGrid::new(GridExtent::new(6, 10));
        cells.set(0, 0, CellBg::DEFAULT.selected());
        cells.set(0, 1, CellBg::rgb(0, 0, 0).search_match().current_match());
        cells.set(0, 2, CellBg::rgb(0, 0, 0).search_match());
        let u = uniforms(&cells, CursorUniform::default());
        let (x, y) = center(0, 0);
        let linear = crate::color::linear_premultiplied;
        let selected = shade_background(&u, &cells, x, y).unwrap();
        assert_eq!(
            selected,
            over(linear(u.background.selection_tint), cleared())
        );
        let (x, y) = center(0, 1);
        assert_eq!(
            shade_background(&u, &cells, x, y),
            Some(over(
                linear(u.background.current_match_tint),
                [0.0, 0.0, 0.0, 1.0]
            )),
            "the current match wins over the plain search tint"
        );
        let (x, y) = center(0, 2);
        assert_eq!(
            shade_background(&u, &cells, x, y),
            Some(over(linear(u.background.search_tint), [0.0, 0.0, 0.0, 1.0]))
        );
    }

    #[test]
    fn every_cursor_shape_covers_its_part_of_the_cell() {
        let cells = CellBgGrid::new(GridExtent::new(6, 10));
        let white = [1.0, 1.0, 1.0, 1.0];
        let cursor = |shape, width_cells| CursorUniform {
            shape,
            col: 3,
            row: 1,
            width_cells,
            thickness: 2.0,
            color: white,
        };
        // Pixel centers inside cell (1, 3): x from 28.5 to 35.5, y from 18.5 to 33.5.
        let shade = |u: &FrameUniforms, x, y| shade_background(u, &cells, x, y).unwrap();
        let block = uniforms(&cells, cursor(CursorShape::Block, 1));
        assert_eq!(shade(&block, 28.5, 18.5), white);
        assert_eq!(shade(&block, 35.5, 33.5), white);
        assert_eq!(shade(&block, 36.5, 25.5), cleared(), "the next cell");
        let wide = uniforms(&cells, cursor(CursorShape::Block, 2));
        assert_eq!(
            shade(&wide, 36.5, 25.5),
            white,
            "a wide cursor covers two cells"
        );
        let underline = uniforms(&cells, cursor(CursorShape::Underline, 1));
        assert_eq!(shade(&underline, 30.5, 33.5), white);
        assert_eq!(shade(&underline, 30.5, 32.5), white);
        assert_eq!(shade(&underline, 30.5, 31.5), cleared());
        let bar = uniforms(&cells, cursor(CursorShape::Bar, 1));
        assert_eq!(shade(&bar, 29.5, 25.5), white);
        assert_eq!(shade(&bar, 30.5, 25.5), cleared());
        let hollow = uniforms(&cells, cursor(CursorShape::HollowBlock, 1));
        assert_eq!(shade(&hollow, 28.5, 25.5), white, "left edge");
        assert_eq!(shade(&hollow, 35.5, 25.5), white, "right edge");
        assert_eq!(shade(&hollow, 31.5, 18.5), white, "top edge");
        assert_eq!(shade(&hollow, 31.5, 33.5), white, "bottom edge");
        assert_eq!(shade(&hollow, 31.5, 25.5), cleared(), "interior");
        let hidden = uniforms(&cells, cursor(CursorShape::Hidden, 1));
        assert_eq!(shade(&hidden, 31.5, 25.5), cleared());
    }

    #[test]
    fn linear_colors_are_stored_as_srgb_bytes_like_the_gpu() {
        // Linear 0.5 and 0.2 encode to sRGB 188 and 124; alpha is stored as is.
        assert_eq!(to_bgra8([1.0, 0.5, 0.2, 1.0]), [124, 188, 255, 255]);
        assert_eq!(to_bgra8([0.0, 0.0, 0.0, 0.5]), [0, 0, 0, 128]);
        assert_eq!(to_bgra8([2.0, -1.0, 0.0, 0.0]), [0, 0, 255, 0]);
    }

    /// ft-yccm0.4.6: `apply_hsb` scales hue, saturation and brightness of
    /// the un-premultiplied color and keeps alpha.
    #[test]
    fn apply_hsb_scales_hsv_of_the_unpremultiplied_color() {
        let close = |a: [f32; 4], b: [f32; 4]| a.iter().zip(&b).all(|(a, b)| (a - b).abs() < 1e-5);
        // Orange at half alpha, premultiplied.
        let color = [0.5, 0.25, 0.0, 0.5];
        assert!(close(apply_hsb(color, [1.0, 1.0, 1.0]), color), "identity");
        // Half the brightness halves every channel.
        assert!(close(
            apply_hsb(color, [1.0, 1.0, 0.5]),
            [0.25, 0.125, 0.0, 0.5]
        ));
        // No saturation is the gray of the brightest channel.
        assert!(close(
            apply_hsb(color, [1.0, 0.0, 1.0]),
            [0.5, 0.5, 0.5, 0.5]
        ));
        // Hue 30 degrees doubled is 60: yellow.
        assert!(close(
            apply_hsb(color, [2.0, 1.0, 1.0]),
            [0.5, 0.5, 0.0, 0.5]
        ));
        let transparent = [0.0, 0.0, 0.0, 0.0];
        assert_eq!(apply_hsb(transparent, [1.0, 0.5, 0.5]), transparent);
        // Planted negative: dimming changes the color.
        assert!(!close(apply_hsb(color, [1.0, 0.8, 0.7]), color));
    }

    #[test]
    fn the_shader_reads_the_buffers_the_frame_slots_bind() {
        // Both paths bind slot buffer i at buffer index i (Metal 3) or
        // argument-table index i (Metal 4).
        let uniforms_binding = format!(
            "constant FrameUniforms &u [[buffer({})]]",
            SlotBuffer::Uniforms.index()
        );
        let cells_binding = format!(
            "device const uchar4 *cells [[buffer({})]]",
            SlotBuffer::CellBg.index()
        );
        assert!(
            BACKGROUND_SHADER.contains(&uniforms_binding),
            "{uniforms_binding}"
        );
        assert!(
            BACKGROUND_SHADER.contains(&cells_binding),
            "{cells_binding}"
        );
        for (name, value) in [
            ("FLAG_COLOR", flags::COLOR),
            ("FLAG_SELECTED", flags::SELECTED),
            ("FLAG_SEARCH_MATCH", flags::SEARCH_MATCH),
            ("FLAG_CURRENT_MATCH", flags::CURRENT_MATCH),
        ] {
            let line = format!("constant uchar {name} = {value};");
            assert!(BACKGROUND_SHADER.contains(&line), "{line}");
        }
        for shape in [
            CursorShape::Block,
            CursorShape::HollowBlock,
            CursorShape::Underline,
            CursorShape::Bar,
        ] {
            let check = format!("u.cursor_shape == {}", shape.code());
            assert!(BACKGROUND_SHADER.contains(&check), "{check}");
        }
    }
}
