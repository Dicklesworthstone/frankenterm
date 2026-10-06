//! Glyph instances, the platform-independent half (ft-yccm0.4.2.3).
//!
//! Every glyph is one 24-byte [`CellText`] instance: its cell, the atlas
//! texels it samples, where the glyph sits in the cell, its foreground and
//! its decorations. The text pass draws every instance of a frame with one
//! instanced draw. Each instance becomes one quad covering the glyph and its
//! cell span (two cells for a wide glyph), and the fragment shader
//! (`src/shaders/text.metal`) does the rest:
//!
//! - grayscale glyphs sample the `R8` atlas and are tinted with the
//!   foreground;
//! - color glyphs (emoji) sample the `BGRA8` atlas, already premultiplied,
//!   with no tint;
//! - the underline (single, double, curly, dotted, dashed, in its own color
//!   for SGR 58), the overline and the strikethrough are drawn from the
//!   instance's flags, so they need no quads of their own.
//!
//! Rows form the same ring as the cell backgrounds ([`crate::cell_bg`]): each
//! instance stores its ring row, the shader maps it to the screen row with the
//! frame's `row_offset`, and a scroll rotates the ring without touching the
//! rows that stay on screen. A frame lays the ring rows out back to back and
//! the per-row table ([`CellTextGrid::row_table`]) records each row's first
//! instance and count.
//!
//! [`shade_text`] is the CPU reference of the fragment shader: the tests'
//! oracle, and the native tests compare the GPU's pixels against it.

use crate::atlas::{AtlasKind, AtlasSlot};
use crate::cell_bg::over;
use crate::frame::{CELL_TEXT_INSTANCE_BYTES, FrameUniforms, GridExtent, ROW_TABLE_BYTES_PER_ROW};

/// The Metal Shading Language source of the text pass.
pub const TEXT_SHADER: &str = include_str!("shaders/text.metal");

/// Flag bits in a [`CellText`] instance's last byte.
pub mod flags {
    /// Bits 0-1: which atlas the glyph samples, if any.
    pub const ATLAS_MASK: u8 = 3;
    pub const ATLAS_GRAY: u8 = 1;
    pub const ATLAS_COLOR: u8 = 2;
    /// The glyph's cell span is two cells wide.
    pub const WIDE: u8 = 4;
    /// Bits 3-5: the [`super::UnderlineStyle`] code.
    pub const UNDERLINE_SHIFT: u8 = 3;
    pub const UNDERLINE_MASK: u8 = 7;
    pub const STRIKETHROUGH: u8 = 64;
    pub const OVERLINE: u8 = 128;
}

/// How an instance's underline is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnderlineStyle {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl UnderlineStyle {
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Single,
        Self::Double,
        Self::Curly,
        Self::Dotted,
        Self::Dashed,
    ];

    /// The value stored in the flag bits and switched on by the shader.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Single => 1,
            Self::Double => 2,
            Self::Curly => 3,
            Self::Dotted => 4,
            Self::Dashed => 5,
        }
    }

    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Single,
            2 => Self::Double,
            3 => Self::Curly,
            4 => Self::Dotted,
            5 => Self::Dashed,
            _ => Self::None,
        }
    }
}

/// One glyph instance in the shader's byte layout (little-endian):
///
/// | offset | field |
/// |---|---|
/// | 0 | column: `u16` |
/// | 2 | ring row: `u16` |
/// | 4 | glyph's top-left atlas texel: `u16` x, `u16` y |
/// | 8 | glyph size in pixels: `u16` width, `u16` height |
/// | 12 | glyph top-left from the cell's top-left: `i16` x, `i16` y |
/// | 16 | foreground: straight RGBA8 |
/// | 20 | underline color: RGB8, then the [`flags`] byte |
///
/// The ring row is set when the instance is pushed into a [`CellTextGrid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellText([u8; CELL_TEXT_INSTANCE_BYTES]);

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read_i16(bytes: &[u8], at: usize) -> i16 {
    i16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// A texture coordinate or extent as the instance stores it. Atlases are at
/// most `MAX_TEXTURE_EXTENT` (16,384) texels on a side, so every slot fits.
fn texel(value: u32) -> u16 {
    u16::try_from(value).expect("atlas coordinates are below MAX_TEXTURE_EXTENT")
}

impl CellText {
    /// An instance in column `col` with the straight RGBA foreground `fg`:
    /// no glyph and no decoration yet, one cell wide. The underline color
    /// defaults to the foreground's.
    #[must_use]
    pub fn new(col: u16, fg: [u8; 4]) -> Self {
        let mut bytes = [0; CELL_TEXT_INSTANCE_BYTES];
        bytes[0..2].copy_from_slice(&col.to_le_bytes());
        bytes[16..20].copy_from_slice(&fg);
        bytes[20..23].copy_from_slice(&fg[..3]);
        Self(bytes)
    }

    fn with_flags(mut self, extra: u8) -> Self {
        self.0[23] |= extra;
        self
    }

    /// Draws the glyph in `slot`, its top-left `offset` pixels from the
    /// cell's top-left (the bearing, or the centering of a wide glyph).
    #[must_use]
    pub fn with_glyph(mut self, slot: &AtlasSlot, offset: [i16; 2]) -> Self {
        let bytes = &mut self.0;
        bytes[4..6].copy_from_slice(&texel(slot.x).to_le_bytes());
        bytes[6..8].copy_from_slice(&texel(slot.y).to_le_bytes());
        bytes[8..10].copy_from_slice(&texel(slot.width).to_le_bytes());
        bytes[10..12].copy_from_slice(&texel(slot.height).to_le_bytes());
        bytes[12..14].copy_from_slice(&offset[0].to_le_bytes());
        bytes[14..16].copy_from_slice(&offset[1].to_le_bytes());
        bytes[23] &= !flags::ATLAS_MASK;
        let atlas = match slot.kind {
            AtlasKind::Grayscale => flags::ATLAS_GRAY,
            AtlasKind::Color => flags::ATLAS_COLOR,
        };
        self.with_flags(atlas)
    }

    /// The cell span is two cells wide (emoji, CJK).
    #[must_use]
    pub fn wide(self) -> Self {
        self.with_flags(flags::WIDE)
    }

    /// Underlines the span in `style` and straight RGB `color`.
    #[must_use]
    pub fn with_underline(mut self, style: UnderlineStyle, color: [u8; 3]) -> Self {
        self.0[20..23].copy_from_slice(&color);
        self.0[23] &= !(flags::UNDERLINE_MASK << flags::UNDERLINE_SHIFT);
        self.with_flags(style.code() << flags::UNDERLINE_SHIFT)
    }

    /// Strikes the span through in the foreground color.
    #[must_use]
    pub fn with_strikethrough(self) -> Self {
        self.with_flags(flags::STRIKETHROUGH)
    }

    /// Draws a line along the top of the span in the foreground color.
    #[must_use]
    pub fn with_overline(self) -> Self {
        self.with_flags(flags::OVERLINE)
    }

    fn with_ring_row(mut self, ring_row: u16) -> Self {
        self.0[2..4].copy_from_slice(&ring_row.to_le_bytes());
        self
    }

    /// The bytes the shader reads.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; CELL_TEXT_INSTANCE_BYTES] {
        &self.0
    }

    #[must_use]
    pub fn col(&self) -> u16 {
        read_u16(&self.0, 0)
    }

    #[must_use]
    pub fn ring_row(&self) -> u16 {
        read_u16(&self.0, 2)
    }

    #[must_use]
    pub fn flags(&self) -> u8 {
        self.0[23]
    }

    #[must_use]
    pub fn fg(&self) -> [u8; 4] {
        [self.0[16], self.0[17], self.0[18], self.0[19]]
    }

    #[must_use]
    pub fn underline(&self) -> UnderlineStyle {
        UnderlineStyle::from_code((self.flags() >> flags::UNDERLINE_SHIFT) & flags::UNDERLINE_MASK)
    }

    #[must_use]
    pub fn underline_color(&self) -> [u8; 3] {
        [self.0[20], self.0[21], self.0[22]]
    }

    /// The atlas the glyph samples, or `None` for a decoration-only
    /// instance.
    #[must_use]
    pub fn atlas(&self) -> Option<AtlasKind> {
        match self.flags() & flags::ATLAS_MASK {
            flags::ATLAS_GRAY => Some(AtlasKind::Grayscale),
            flags::ATLAS_COLOR => Some(AtlasKind::Color),
            _ => None,
        }
    }

    /// The glyph's top-left atlas texel.
    #[must_use]
    pub fn atlas_origin(&self) -> [u16; 2] {
        [read_u16(&self.0, 4), read_u16(&self.0, 6)]
    }

    /// The glyph's width and height in pixels.
    #[must_use]
    pub fn glyph_size(&self) -> [u16; 2] {
        [read_u16(&self.0, 8), read_u16(&self.0, 10)]
    }

    /// The glyph's top-left from the cell's top-left, in pixels.
    #[must_use]
    pub fn offset(&self) -> [i16; 2] {
        [read_i16(&self.0, 12), read_i16(&self.0, 14)]
    }
}

/// The text pass's part of [`FrameUniforms`]: decoration geometry, in pixels
/// within the cell. The pass reads the cell size, grid origin and ring
/// offset from the background's part.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TextUniforms {
    /// From the cell top to the top of the underline.
    pub underline_position: f32,
    /// Thickness of every decoration line.
    pub line_thickness: f32,
    /// From the cell top to the top of the strikethrough.
    pub strikethrough_position: f32,
}

/// One pane's glyph instances, stored by ring row exactly as the frame lays
/// them out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellTextGrid {
    rows: u32,
    cols: u32,
    row_offset: u32,
    /// Instances of each ring row; cleared rows keep their capacity.
    ring: Vec<Vec<CellText>>,
    len: usize,
}

impl CellTextGrid {
    /// A grid with no instances. Ring rows are stored as `u16`, so rows past
    /// `u16::MAX` accept no instances.
    #[must_use]
    pub fn new(extent: GridExtent) -> Self {
        let rows = usize::try_from(extent.rows).unwrap_or(usize::MAX);
        Self {
            rows: extent.rows,
            cols: extent.cols,
            row_offset: 0,
            ring: (0..rows.min(usize::from(u16::MAX) + 1))
                .map(|_| Vec::new())
                .collect(),
            len: 0,
        }
    }

    #[must_use]
    pub fn extent(&self) -> GridExtent {
        GridExtent {
            rows: self.rows,
            cols: self.cols,
        }
    }

    /// The ring offset the shader subtracts from every ring row; equal to
    /// the background grid's after the same scrolls.
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

    fn ring_index(&self, row: u32) -> Option<usize> {
        if row >= self.rows {
            return None;
        }
        let index = usize::try_from(self.ring_row(row)).ok()?;
        (index < self.ring.len()).then_some(index)
    }

    /// Adds `instance` to logical `row`, drawn after the row's earlier
    /// instances. Returns false (and drops it) outside the grid.
    pub fn push(&mut self, row: u32, instance: CellText) -> bool {
        if u32::from(instance.col()) >= self.cols {
            return false;
        }
        let Some(index) = self.ring_index(row) else {
            return false;
        };
        let ring_row = u16::try_from(index).expect("ring rows past u16::MAX are not stored");
        self.ring[index].push(instance.with_ring_row(ring_row));
        self.len += 1;
        true
    }

    /// Removes every instance of logical `row`.
    pub fn clear_row(&mut self, row: u32) {
        if let Some(index) = self.ring_index(row) {
            self.len -= self.ring[index].len();
            self.ring[index].clear();
        }
    }

    /// The instances of logical `row`, in draw order.
    #[must_use]
    pub fn row(&self, row: u32) -> &[CellText] {
        self.ring_index(row)
            .map_or(&[], |index| self.ring[index].as_slice())
    }

    /// Scrolls the content up by `lines`, like
    /// [`crate::cell_bg::CellBgGrid::scroll_up`]: only `row_offset` moves,
    /// the rows that stay keep their instances in place, and the `lines`
    /// rows exposed at the bottom are emptied.
    pub fn scroll_up(&mut self, lines: u32) {
        if self.rows == 0 {
            return;
        }
        let lines = lines.min(self.rows);
        let rotated = (u64::from(self.row_offset) + u64::from(lines)) % u64::from(self.rows);
        self.row_offset = u32::try_from(rotated).expect("the offset is below the row count");
        for row in self.rows - lines..self.rows {
            self.clear_row(row);
        }
    }

    /// Instances in the grid.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Every instance in draw order: ring row by ring row, each row's in the
    /// order it was pushed. The frame's CellText buffer holds exactly this.
    pub fn instances(&self) -> impl Iterator<Item = &CellText> {
        self.ring.iter().flatten()
    }

    /// Each ring row's instances, in ring order: the CellText buffer is
    /// these slices back to back.
    pub fn ring_rows(&self) -> impl Iterator<Item = &[CellText]> {
        self.ring.iter().map(Vec::as_slice)
    }

    /// The per-row table, one entry per ring row in ring order, as the
    /// RowTable buffer holds it: the row's first instance and its instance
    /// count (`u32` each), then 8 bytes reserved for per-row flags.
    pub fn row_table(&self) -> impl Iterator<Item = [u8; ROW_TABLE_BYTES_PER_ROW]> {
        let mut first = 0_u32;
        self.ring.iter().map(move |row| {
            let count = u32::try_from(row.len()).unwrap_or(u32::MAX);
            let mut entry = [0; ROW_TABLE_BYTES_PER_ROW];
            entry[0..4].copy_from_slice(&first.to_le_bytes());
            entry[4..8].copy_from_slice(&count.to_le_bytes());
            first = first.saturating_add(count);
            entry
        })
    }
}

fn unit(byte: u8) -> f32 {
    f32::from(byte) / 255.0
}

fn in_band(y: f32, top: f32, thickness: f32) -> bool {
    y >= top && y < top + thickness
}

/// Whether the underline of `style` covers `local` (pixels from the span's
/// top-left); `x` is the pixel's distance from the grid's left edge.
// Pattern cells come from a non-negative pixel distance divided by the line
// thickness, truncated exactly as the shader's uint(floor()) truncates.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn underline_covers(style: UnderlineStyle, local: [f32; 2], x: f32, u: &FrameUniforms) -> bool {
    let top = u.text.underline_position;
    let thickness = u.text.line_thickness;
    let unit = thickness.max(1.0);
    let band = in_band(local[1], top, thickness);
    match style {
        UnderlineStyle::None => false,
        UnderlineStyle::Single => band,
        UnderlineStyle::Double => band || in_band(local[1], top - 2.0 * thickness, thickness),
        UnderlineStyle::Curly => {
            let wave = (std::f32::consts::TAU * x / u.background.cell_size[0].max(1.0)).sin();
            let center = top + 0.5 * thickness + thickness * wave;
            (local[1] - center).abs() < 0.5 * thickness + 0.5
        }
        UnderlineStyle::Dotted => band && ((x / unit).floor() as u32).is_multiple_of(2),
        UnderlineStyle::Dashed => band && ((x / unit).floor() as u32) % 5 < 3,
    }
}

/// The premultiplied color one instance's fragment writes at pixel center
/// `(x, y)`, or `None` where it discards.
// Atlas texels come from the non-negative offset inside the glyph,
// truncated exactly as the shader's floor() and nearest sampling truncate.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn shade_instance(
    u: &FrameUniforms,
    cell: &CellText,
    atlas: &impl Fn(AtlasKind, u32, u32) -> [u8; 4],
    x: f32,
    y: f32,
) -> Option<[f32; 4]> {
    let background = &u.background;
    let rows = u64::from(u.grid.rows.max(1));
    let logical_row =
        (u64::from(cell.ring_row()) + rows - u64::from(background.row_offset) % rows) % rows;
    let span_cells = if cell.flags() & flags::WIDE != 0 {
        2.0
    } else {
        1.0
    };
    let span_origin = [
        background.grid_origin[0] + f32::from(cell.col()) * background.cell_size[0],
        background.grid_origin[1] + logical_row as f32 * background.cell_size[1],
    ];
    let span_size = [
        span_cells * background.cell_size[0],
        background.cell_size[1],
    ];
    let fg = cell.fg().map(unit);
    let alpha = fg[3];
    let underline_rgb = cell.underline_color().map(unit);
    let underline_color = [
        underline_rgb[0] * alpha,
        underline_rgb[1] * alpha,
        underline_rgb[2] * alpha,
        alpha,
    ];
    let line_color = [fg[0] * alpha, fg[1] * alpha, fg[2] * alpha, alpha];
    let mut out = [0.0; 4];

    let local = [x - span_origin[0], y - span_origin[1]];
    let in_span =
        local[0] >= 0.0 && local[1] >= 0.0 && local[0] < span_size[0] && local[1] < span_size[1];
    let grid_x = x - background.grid_origin[0];
    if in_span && underline_covers(cell.underline(), local, grid_x, u) {
        out = underline_color;
    }
    if in_span
        && cell.flags() & flags::OVERLINE != 0
        && in_band(local[1], 0.0, u.text.line_thickness)
    {
        out = over(line_color, out);
    }

    let offset = cell.offset();
    let glyph_origin = [
        span_origin[0].floor() + f32::from(offset[0]),
        span_origin[1].floor() + f32::from(offset[1]),
    ];
    let size = cell.glyph_size();
    let inner = [x - glyph_origin[0], y - glyph_origin[1]];
    if let Some(kind) = cell.atlas()
        && inner[0] >= 0.0
        && inner[1] >= 0.0
        && inner[0] < f32::from(size[0])
        && inner[1] < f32::from(size[1])
    {
        let origin = cell.atlas_origin();
        let texel = atlas(
            kind,
            u32::from(origin[0]) + inner[0].floor() as u32,
            u32::from(origin[1]) + inner[1].floor() as u32,
        );
        let glyph = match kind {
            // Stored [B, G, R, A], premultiplied; sampled as RGBA.
            AtlasKind::Color => [
                unit(texel[2]) * alpha,
                unit(texel[1]) * alpha,
                unit(texel[0]) * alpha,
                unit(texel[3]) * alpha,
            ],
            AtlasKind::Grayscale => {
                let coverage = unit(texel[0]) * alpha;
                [
                    fg[0] * coverage,
                    fg[1] * coverage,
                    fg[2] * coverage,
                    coverage,
                ]
            }
        };
        out = over(glyph, out);
    }

    if in_span
        && cell.flags() & flags::STRIKETHROUGH != 0
        && in_band(
            local[1],
            u.text.strikethrough_position,
            u.text.line_thickness,
        )
    {
        out = over(line_color, out);
    }
    (out[3] > 0.0).then_some(out)
}

/// The CPU reference of the text pass: the premultiplied color at pixel
/// center `(x, y)` after every instance of `text` is blended, in draw order,
/// over `under` (what the background pass left there).
///
/// `atlas(kind, x, y)` returns the atlas texel's stored bytes: the coverage
/// in byte 0 for [`AtlasKind::Grayscale`], `[B, G, R, A]` premultiplied for
/// [`AtlasKind::Color`]. `uniforms.grid` must be `text.extent()` and
/// `uniforms.background.row_offset` must be `text.row_offset()`.
#[must_use]
pub fn shade_text(
    uniforms: &FrameUniforms,
    text: &CellTextGrid,
    atlas: impl Fn(AtlasKind, u32, u32) -> [u8; 4],
    under: [f32; 4],
    x: f32,
    y: f32,
) -> [f32; 4] {
    text.instances().fold(under, |color, cell| {
        shade_instance(uniforms, cell, &atlas, x, y).map_or(color, |top| over(top, color))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell_bg::BackgroundUniforms;
    use crate::frame::SlotBuffer;

    const CELL: [f32; 2] = [8.0, 16.0];
    const ORIGIN: [f32; 2] = [4.0, 2.0];
    const UNDER: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    fn uniforms(text: &CellTextGrid) -> FrameUniforms {
        FrameUniforms {
            frame: 3,
            viewport: [96, 103],
            grid: text.extent(),
            clear: UNDER,
            background: BackgroundUniforms {
                cell_size: CELL,
                grid_origin: ORIGIN,
                row_offset: text.row_offset(),
                ..BackgroundUniforms::default()
            },
            text: TextUniforms {
                underline_position: 13.0,
                line_thickness: 1.0,
                strikethrough_position: 8.0,
            },
        }
    }

    fn slot(kind: AtlasKind, x: u32, y: u32, width: u32, height: u32) -> AtlasSlot {
        AtlasSlot {
            kind,
            page: 0,
            epoch: 0,
            x,
            y,
            width,
            height,
        }
    }

    /// A grayscale atlas whose coverage is 255 everywhere, and a color atlas
    /// whose texels are opaque `[B, G, R, A] = [x, y, 7, 255]`.
    fn atlas(kind: AtlasKind, x: u32, y: u32) -> [u8; 4] {
        match kind {
            AtlasKind::Grayscale => [255, 0, 0, 0],
            AtlasKind::Color => [
                u8::try_from(x % 256).unwrap(),
                u8::try_from(y % 256).unwrap(),
                7,
                255,
            ],
        }
    }

    /// The pixel center at `(px, py)` within logical cell `(row, col)`.
    #[allow(clippy::cast_precision_loss)]
    fn at(row: u32, col: u32, px: f32, py: f32) -> (f32, f32) {
        (
            ORIGIN[0] + col as f32 * CELL[0] + px + 0.5,
            ORIGIN[1] + row as f32 * CELL[1] + py + 0.5,
        )
    }

    fn shade(text: &CellTextGrid, (x, y): (f32, f32)) -> [f32; 4] {
        shade_text(&uniforms(text), text, atlas, UNDER, x, y)
    }

    #[test]
    fn instances_pack_into_the_shaders_24_byte_layout() {
        let instance = CellText::new(0x0102, [10, 20, 30, 40])
            .with_glyph(&slot(AtlasKind::Color, 300, 2047, 16, 9), [-2, 3])
            .wide()
            .with_underline(UnderlineStyle::Dashed, [1, 2, 3])
            .with_strikethrough()
            .with_overline();
        let bytes = instance.bytes();
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[0..2], &[0x02, 0x01]);
        assert_eq!(&bytes[4..8], &[44, 1, 0xff, 7]);
        assert_eq!(&bytes[8..12], &[16, 0, 9, 0]);
        assert_eq!(&bytes[12..16], &[0xfe, 0xff, 3, 0]);
        assert_eq!(&bytes[16..20], &[10, 20, 30, 40]);
        assert_eq!(&bytes[20..23], &[1, 2, 3]);
        assert_eq!(
            instance.flags(),
            flags::ATLAS_COLOR
                | flags::WIDE
                | (5 << flags::UNDERLINE_SHIFT)
                | flags::STRIKETHROUGH
                | flags::OVERLINE
        );
        assert_eq!(instance.atlas(), Some(AtlasKind::Color));
        assert_eq!(instance.underline(), UnderlineStyle::Dashed);
        assert_eq!(
            (
                instance.atlas_origin(),
                instance.glyph_size(),
                instance.offset()
            ),
            ([300, 2047], [16, 9], [-2, 3])
        );
        // A plain instance underlines in its foreground color by default.
        let plain = CellText::new(1, [9, 8, 7, 6]);
        assert_eq!(plain.underline_color(), [9, 8, 7]);
        assert_eq!((plain.flags(), plain.atlas()), (0, None));
        // Replacing the glyph or the underline replaces their bits.
        let gray = instance.with_glyph(&slot(AtlasKind::Grayscale, 0, 0, 1, 1), [0, 0]);
        assert_eq!(gray.atlas(), Some(AtlasKind::Grayscale));
        let single = instance.with_underline(UnderlineStyle::Single, [0; 3]);
        assert_eq!(single.underline(), UnderlineStyle::Single);
        for style in UnderlineStyle::ALL {
            assert_eq!(UnderlineStyle::from_code(style.code()), style);
        }
    }

    #[test]
    fn the_shader_declares_the_instance_layout_flags_and_bindings() {
        for (field, offset) in [
            ("grid", 0),
            ("atlas", 4),
            ("size", 8),
            ("offset", 12),
            ("fg", 16),
            ("decoration", 20),
        ] {
            let declaration = format!(" {field};");
            let line = TEXT_SHADER
                .lines()
                .skip_while(|line| !line.starts_with("struct CellText"))
                .find(|line| line.contains(&declaration) && line.contains("//"))
                .unwrap_or_else(|| panic!("the shader's CellText declares {field}"));
            assert!(line.contains(&format!("// {offset}")), "{field}: {line}");
        }
        for (name, value) in [
            ("FLAG_ATLAS_MASK", flags::ATLAS_MASK),
            ("FLAG_ATLAS_GRAY", flags::ATLAS_GRAY),
            ("FLAG_ATLAS_COLOR", flags::ATLAS_COLOR),
            ("FLAG_WIDE", flags::WIDE),
            ("FLAG_UNDERLINE_SHIFT", flags::UNDERLINE_SHIFT),
            ("FLAG_UNDERLINE_MASK", flags::UNDERLINE_MASK),
            ("FLAG_STRIKETHROUGH", flags::STRIKETHROUGH),
            ("FLAG_OVERLINE", flags::OVERLINE),
        ] {
            let line = format!("constant uchar {name} = {value};");
            assert!(TEXT_SHADER.contains(&line), "{line}");
        }
        for (name, style) in [
            ("UNDERLINE_SINGLE", UnderlineStyle::Single),
            ("UNDERLINE_DOUBLE", UnderlineStyle::Double),
            ("UNDERLINE_CURLY", UnderlineStyle::Curly),
            ("UNDERLINE_DOTTED", UnderlineStyle::Dotted),
            ("UNDERLINE_DASHED", UnderlineStyle::Dashed),
        ] {
            let line = format!("constant uint {name} = {};", style.code());
            assert!(TEXT_SHADER.contains(&line), "{line}");
        }
        for binding in [
            format!(
                "constant FrameUniforms &u [[buffer({})]]",
                SlotBuffer::Uniforms.index()
            ),
            format!(
                "device const CellText *instances [[buffer({})]]",
                SlotBuffer::CellText.index()
            ),
        ] {
            assert!(TEXT_SHADER.contains(&binding), "{binding}");
        }
        assert_eq!(
            std::mem::size_of::<CellText>(),
            CELL_TEXT_INSTANCE_BYTES,
            "the frame slots size CellText for this instance"
        );
    }

    #[test]
    fn rows_form_a_ring_and_a_scroll_moves_no_instance() {
        let mut text = CellTextGrid::new(GridExtent::new(4, 3));
        for row in 0..4_u16 {
            assert!(text.push(u32::from(row), CellText::new(row % 3, WHITE)));
        }
        assert!(text.push(1, CellText::new(2, WHITE)));
        assert_eq!(text.len(), 5);
        let before: Vec<CellText> = text.instances().copied().collect();
        text.scroll_up(1);
        assert_eq!(text.row_offset(), 1);
        // Logical row 0 now shows what row 1 showed, stored where it was.
        assert_eq!(text.row(0).len(), 2);
        assert_eq!(text.row(0)[0].ring_row(), 1);
        assert!(text.row(3).is_empty(), "the exposed row is emptied");
        assert_eq!(text.len(), 4);
        let after: Vec<CellText> = text.instances().copied().collect();
        assert_eq!(&after[..], &before[1..], "only ring row 0 was cleared");
        text.scroll_up(10);
        assert!(text.is_empty());
        CellTextGrid::new(GridExtent::default()).scroll_up(1);
    }

    #[test]
    fn the_row_table_gives_each_ring_row_its_range_in_draw_order() {
        let mut text = CellTextGrid::new(GridExtent::new(3, 4));
        for col in 0..3 {
            text.push(2, CellText::new(col, WHITE));
        }
        text.push(0, CellText::new(1, WHITE));
        let table: Vec<[u32; 2]> = text
            .row_table()
            .map(|entry| {
                assert_eq!(&entry[8..], &[0; 8], "reserved");
                [
                    u32::from_le_bytes(entry[0..4].try_into().unwrap()),
                    u32::from_le_bytes(entry[4..8].try_into().unwrap()),
                ]
            })
            .collect();
        assert_eq!(table, [[0, 1], [1, 0], [1, 3]]);
        let rows: Vec<usize> = text.ring_rows().map(<[CellText]>::len).collect();
        assert_eq!(rows, [1, 0, 3]);
        let order: Vec<u16> = text.instances().map(CellText::ring_row).collect();
        assert_eq!(order, [0, 2, 2, 2]);
    }

    #[test]
    fn instances_outside_the_grid_are_refused() {
        let mut text = CellTextGrid::new(GridExtent::new(2, 3));
        assert!(!text.push(2, CellText::new(0, WHITE)));
        assert!(!text.push(0, CellText::new(3, WHITE)));
        assert!(text.push(1, CellText::new(2, WHITE)));
        assert_eq!(text.len(), 1);
        text.clear_row(1);
        text.clear_row(7);
        assert!(text.is_empty());
        assert_eq!(text.row(9), &[] as &[CellText]);
    }

    #[test]
    fn grayscale_glyphs_are_tinted_with_the_foreground_at_their_offset() {
        let mut text = CellTextGrid::new(GridExtent::new(3, 4));
        let fg = [255, 128, 0, 255];
        // A 3x4 glyph 2 px right of and 5 px below cell (1, 2)'s corner.
        text.push(
            1,
            CellText::new(2, fg).with_glyph(&slot(AtlasKind::Grayscale, 10, 20, 3, 4), [2, 5]),
        );
        let tinted = [1.0, 128.0 / 255.0, 0.0, 1.0];
        assert_eq!(shade(&text, at(1, 2, 2.0, 5.0)), tinted);
        assert_eq!(shade(&text, at(1, 2, 4.0, 8.0)), tinted);
        assert_eq!(shade(&text, at(1, 2, 1.0, 5.0)), UNDER, "left of the glyph");
        assert_eq!(
            shade(&text, at(1, 2, 5.0, 5.0)),
            UNDER,
            "right of the glyph"
        );
        assert_eq!(shade(&text, at(1, 2, 2.0, 9.0)), UNDER, "below the glyph");
    }

    #[test]
    fn color_glyphs_keep_their_colors_and_fade_with_the_foreground_alpha() {
        let mut text = CellTextGrid::new(GridExtent::new(2, 4));
        let glyph = slot(AtlasKind::Color, 40, 50, 16, 16);
        text.push(
            0,
            CellText::new(0, [255, 0, 0, 255])
                .with_glyph(&glyph, [0, 0])
                .wide(),
        );
        // The texel under (row 0, x 9, y 3) is (49, 53): [B, G, R] = [49, 53, 7].
        let opaque = shade(&text, at(0, 0, 9.0, 3.0));
        assert_eq!(opaque, [7.0 / 255.0, 53.0 / 255.0, 49.0 / 255.0, 1.0]);
        let mut faded = CellTextGrid::new(GridExtent::new(2, 4));
        faded.push(
            0,
            CellText::new(0, [255, 0, 0, 51])
                .with_glyph(&glyph, [0, 0])
                .wide(),
        );
        let color = shade(&faded, at(0, 0, 9.0, 3.0));
        let alpha = 51.0 / 255.0;
        for (got, want) in color[..3].iter().zip(&opaque[..3]) {
            assert!((got - want * alpha).abs() < 1e-6, "{color:?}");
        }
        assert!(
            (color[3] - (alpha + (1.0 - alpha))).abs() < 1e-6,
            "over opaque black"
        );
    }

    #[test]
    fn a_wide_glyph_spans_two_cells_and_so_does_its_underline() {
        let mut text = CellTextGrid::new(GridExtent::new(2, 4));
        text.push(
            0,
            CellText::new(1, WHITE)
                .with_glyph(&slot(AtlasKind::Grayscale, 0, 0, 16, 10), [0, 0])
                .wide()
                .with_underline(UnderlineStyle::Single, [0, 255, 0]),
        );
        let white = [1.0; 4];
        let green = [0.0, 1.0, 0.0, 1.0];
        assert_eq!(shade(&text, at(0, 2, 7.0, 0.0)), white, "the second cell");
        assert_eq!(shade(&text, at(0, 3, 0.0, 0.0)), UNDER, "past the span");
        assert_eq!(shade(&text, at(0, 2, 7.0, 13.0)), green);
        assert_eq!(shade(&text, at(0, 3, 0.0, 13.0)), UNDER);
    }

    #[test]
    fn the_ring_offset_places_instances_like_the_shader() {
        let mut text = CellTextGrid::new(GridExtent::new(4, 2));
        text.push(3, CellText::new(0, WHITE).with_overline());
        text.scroll_up(2);
        // The instance stored for row 3 now shows on row 1.
        assert_eq!(shade(&text, at(1, 0, 3.0, 0.0)), [1.0; 4]);
        assert_eq!(shade(&text, at(3, 0, 3.0, 0.0)), UNDER);
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn every_underline_style_covers_its_pattern() {
        let row_of = |style: UnderlineStyle| -> Vec<Vec<bool>> {
            let mut text = CellTextGrid::new(GridExtent::new(1, 4));
            for col in 0..4 {
                text.push(0, CellText::new(col, WHITE).with_underline(style, [255; 3]));
            }
            (0..16)
                .map(|py| {
                    (0..32)
                        .map(|px| {
                            let (x, y) = (ORIGIN[0] + px as f32 + 0.5, ORIGIN[1] + py as f32 + 0.5);
                            shade(&text, (x, y))
                                .iter()
                                .zip(UNDER)
                                .any(|(got, under)| (got - under).abs() > 1e-6)
                        })
                        .collect()
                })
                .collect()
        };
        let none = row_of(UnderlineStyle::None);
        assert!(none.iter().flatten().all(|lit| !lit));
        let single = row_of(UnderlineStyle::Single);
        for (py, row) in single.iter().enumerate() {
            assert!(row.iter().all(|&lit| lit == (py == 13)), "single, y {py}");
        }
        let double = row_of(UnderlineStyle::Double);
        for (py, row) in double.iter().enumerate() {
            assert!(
                row.iter().all(|&lit| lit == (py == 13 || py == 11)),
                "double, y {py}"
            );
        }
        let dotted = row_of(UnderlineStyle::Dotted);
        assert_eq!(
            dotted[13].iter().filter(|&&lit| lit).count(),
            16,
            "every other pixel"
        );
        assert!(dotted[13][0] && !dotted[13][1] && dotted[13][30]);
        let dashed = row_of(UnderlineStyle::Dashed);
        let lit: Vec<bool> = dashed[13][..10].to_vec();
        assert_eq!(
            lit,
            [
                true, true, true, false, false, true, true, true, false, false
            ]
        );
        let curly = row_of(UnderlineStyle::Curly);
        let columns = (0..32).map(|px| (0..16).filter(|&py| curly[py][px]).collect::<Vec<usize>>());
        for (px, column) in columns.enumerate() {
            assert!(!column.is_empty(), "the wave is unbroken at x {px}");
            assert!(
                column.iter().all(|&py| (11..=15).contains(&py)),
                "x {px}: {column:?}"
            );
        }
        assert_ne!(curly[12], curly[14], "the wave moves");
    }

    #[test]
    fn decorations_layer_underline_and_overline_below_the_glyph_and_strikethrough_above() {
        let mut text = CellTextGrid::new(GridExtent::new(1, 2));
        let red = [255, 0, 0, 255];
        text.push(
            0,
            CellText::new(0, red)
                .with_glyph(&slot(AtlasKind::Grayscale, 0, 0, 8, 16), [0, 0])
                .with_underline(UnderlineStyle::Single, [0, 0, 255])
                .with_overline()
                .with_strikethrough(),
        );
        let solid_red = [1.0, 0.0, 0.0, 1.0];
        // The opaque glyph covers the underline and the overline...
        assert_eq!(shade(&text, at(0, 0, 3.0, 13.0)), solid_red);
        assert_eq!(shade(&text, at(0, 0, 3.0, 0.0)), solid_red);
        // ...and the strikethrough, in the foreground color, covers it.
        assert_eq!(shade(&text, at(0, 0, 3.0, 8.0)), solid_red);
        let mut bare = CellTextGrid::new(GridExtent::new(1, 2));
        bare.push(
            0,
            CellText::new(0, red)
                .with_underline(UnderlineStyle::Single, [0, 0, 255])
                .with_overline()
                .with_strikethrough(),
        );
        assert_eq!(shade(&bare, at(0, 0, 3.0, 13.0)), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(shade(&bare, at(0, 0, 3.0, 0.0)), solid_red);
        assert_eq!(shade(&bare, at(0, 0, 3.0, 8.0)), solid_red);
        assert_eq!(shade(&bare, at(0, 0, 3.0, 5.0)), UNDER);
    }

    #[test]
    fn instances_blend_in_draw_order() {
        let mut text = CellTextGrid::new(GridExtent::new(1, 2));
        let half_white = [255, 255, 255, 128];
        text.push(0, CellText::new(0, [255, 0, 0, 255]).with_overline());
        text.push(0, CellText::new(0, half_white).with_overline());
        // Half-transparent white over opaque red.
        let alpha = 128.0 / 255.0;
        let got = shade(&text, at(0, 0, 0.0, 0.0));
        for (got, want) in got.iter().zip([1.0, alpha, alpha, 1.0]) {
            assert!((got - want).abs() < 1e-6, "{got} vs {want}");
        }
    }
}
