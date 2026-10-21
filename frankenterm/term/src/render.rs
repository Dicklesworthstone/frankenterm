//! Semantic rows shared by the desktop render mirror and native consumers.
//!
//! This is the canonical cell conversion, extracted from the live mux renderer.
//! It does not parse terminal bytes, shape text, allocate glyphs, or emit effects.
//! [`capture_terminal_frame`] reads the actual terminal and produces one bounded,
//! owned full replacement in the explicit FTSF format. No Rust layout or borrowed
//! terminal pointer crosses the language boundary.

use crate::config::GridEngine;
use crate::{Line, StableRowIndex, Terminal};
use frankenterm_bidi::Direction;
use std::convert::TryFrom;
use std::sync::Arc;
use termwiz::cell::{Blink, CellAttributes, Intensity, SemanticType, Underline, VerticalAlign};
use termwiz::cellcluster::CellCluster;
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::hyperlink::Hyperlink;
use termwiz::image::ImageCell;
use termwiz::surface::line::CellRef;
use termwiz::surface::{CursorShape, CursorVisibility, SequenceNo, SEQ_ZERO};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MirrorColor {
    Default,
    Palette(u8),
    /// Straight-alpha sRGB, with the same 8-bit precision as the desktop mirror.
    Rgba([u8; 4]),
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn unorm8(component: f32) -> u8 {
    (component.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn rgba8(SrgbaTuple(red, green, blue, alpha): SrgbaTuple) -> [u8; 4] {
    [unorm8(red), unorm8(green), unorm8(blue), unorm8(alpha)]
}

impl MirrorColor {
    fn from_attribute(color: ColorAttribute) -> Self {
        match color {
            ColorAttribute::Default => Self::Default,
            ColorAttribute::PaletteIndex(index) => Self::Palette(index),
            ColorAttribute::TrueColorWithPaletteFallback(color, _)
            | ColorAttribute::TrueColorWithDefaultFallback(color) => Self::Rgba(rgba8(color)),
        }
    }

    pub fn to_attribute(self) -> ColorAttribute {
        match self {
            Self::Default => ColorAttribute::Default,
            Self::Palette(index) => ColorAttribute::PaletteIndex(index),
            Self::Rgba([red, green, blue, alpha]) => {
                ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
                    f32::from(red) / 255.0,
                    f32::from(green) / 255.0,
                    f32::from(blue) / 255.0,
                    f32::from(alpha) / 255.0,
                ))
            }
        }
    }
}

const INTENSITY_SHIFT: u32 = 0;
const UNDERLINE_SHIFT: u32 = 2;
const BLINK_SHIFT: u32 = 5;
const ITALIC: u32 = 1 << 7;
const REVERSE: u32 = 1 << 8;
const STRIKETHROUGH: u32 = 1 << 9;
const INVISIBLE: u32 = 1 << 10;
const OVERLINE: u32 = 1 << 11;
const VALIGN_SHIFT: u32 = 12;
const IMAGE: u32 = 1 << 14;
const SEMANTIC_SHIFT: u32 = 15;

/// One visible cell. Wide-character spacers are not separate entries.
/// Host-sized indexes retain the complete grapheme; wire limits are checked by
/// the bounded consumer instead of silently truncating to an eight-bit length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorCell {
    text_start: usize,
    text_len: usize,
    col: usize,
    width: usize,
    flags: u32,
    link: usize,
    fg: MirrorColor,
    bg: MirrorColor,
    underline_color: MirrorColor,
}

impl MirrorCell {
    fn from_cell(cell: CellRef<'_>, text_start: usize, link: usize) -> Self {
        let attrs = cell.attrs();
        Self {
            text_start,
            text_len: cell.str().len(),
            col: cell.cell_index(),
            width: cell.width(),
            flags: Self::flags_of(attrs),
            link,
            fg: MirrorColor::from_attribute(attrs.foreground()),
            bg: MirrorColor::from_attribute(attrs.background()),
            underline_color: MirrorColor::from_attribute(attrs.underline_color()),
        }
    }

    pub fn col(&self) -> usize {
        self.col
    }
    pub fn width(&self) -> usize {
        self.width
    }
    pub fn fg(&self) -> MirrorColor {
        self.fg
    }
    pub fn bg(&self) -> MirrorColor {
        self.bg
    }
    pub fn underline_color(&self) -> MirrorColor {
        self.underline_color
    }
    pub fn intensity(&self) -> Intensity {
        match (self.flags >> INTENSITY_SHIFT) & 3 {
            1 => Intensity::Bold,
            2 => Intensity::Half,
            _ => Intensity::Normal,
        }
    }
    pub fn underline(&self) -> Underline {
        match (self.flags >> UNDERLINE_SHIFT) & 7 {
            1 => Underline::Single,
            2 => Underline::Double,
            3 => Underline::Curly,
            4 => Underline::Dotted,
            5 => Underline::Dashed,
            _ => Underline::None,
        }
    }
    pub fn blink(&self) -> Blink {
        match (self.flags >> BLINK_SHIFT) & 3 {
            1 => Blink::Slow,
            2 => Blink::Rapid,
            _ => Blink::None,
        }
    }
    pub fn italic(&self) -> bool {
        self.flags & ITALIC != 0
    }
    pub fn reverse(&self) -> bool {
        self.flags & REVERSE != 0
    }
    pub fn strikethrough(&self) -> bool {
        self.flags & STRIKETHROUGH != 0
    }
    pub fn invisible(&self) -> bool {
        self.flags & INVISIBLE != 0
    }
    pub fn overline(&self) -> bool {
        self.flags & OVERLINE != 0
    }
    pub fn vertical_align(&self) -> VerticalAlign {
        match (self.flags >> VALIGN_SHIFT) & 3 {
            1 => VerticalAlign::SuperScript,
            2 => VerticalAlign::SubScript,
            _ => VerticalAlign::BaseLine,
        }
    }
    pub fn has_image(&self) -> bool {
        self.flags & IMAGE != 0
    }
    pub fn semantic_type(&self) -> SemanticType {
        match (self.flags >> SEMANTIC_SHIFT) & 3 {
            1 => SemanticType::Input,
            2 => SemanticType::Prompt,
            _ => SemanticType::Output,
        }
    }

    fn flags_of(attrs: &CellAttributes) -> u32 {
        let mut flags = ((attrs.intensity() as u32) << INTENSITY_SHIFT)
            | ((attrs.underline() as u32) << UNDERLINE_SHIFT)
            | ((attrs.blink() as u32) << BLINK_SHIFT)
            | ((attrs.vertical_align() as u32) << VALIGN_SHIFT)
            | ((attrs.semantic_type() as u32) << SEMANTIC_SHIFT);
        for (set, bit) in [
            (attrs.italic(), ITALIC),
            (attrs.reverse(), REVERSE),
            (attrs.strikethrough(), STRIKETHROUGH),
            (attrs.invisible(), INVISIBLE),
            (attrs.overline(), OVERLINE),
            (attrs.has_image_attachments(), IMAGE),
        ] {
            if set {
                flags |= bit;
            }
        }
        flags
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MirrorImage {
    pub col: usize,
    pub image: ImageCell,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MirrorRow {
    stable: StableRowIndex,
    seqno: SequenceNo,
    generation: u64,
    cells: Vec<MirrorCell>,
    text: String,
    links: Vec<Arc<Hyperlink>>,
    has_images: bool,
    images: Vec<MirrorImage>,
}

impl MirrorRow {
    pub fn empty(stable: StableRowIndex, generation: u64) -> Self {
        Self {
            stable,
            seqno: SEQ_ZERO,
            generation,
            cells: Vec::new(),
            text: String::new(),
            links: Vec::new(),
            has_images: false,
            images: Vec::new(),
        }
    }

    /// Desktop capture; the caller owns the terminal's allocation policy. Native
    /// bounded capture preflights borrowed cells and uses the same conversion
    /// directly, so it never allocates this unrestricted intermediate row.
    pub fn from_line(stable: StableRowIndex, line: &Line, generation: u64) -> Self {
        let mut row = Self::empty(stable, generation);
        row.seqno = line.current_seqno();
        row.cells.reserve(line.len());
        row.text.reserve(line.len());
        for cell in line.visible_cells() {
            let link = match cell.attrs().hyperlink() {
                None => 0,
                Some(link) => {
                    let index = row
                        .links
                        .iter()
                        .position(|known| Arc::ptr_eq(known, link) || **known == **link)
                        .unwrap_or_else(|| {
                            row.links.push(Arc::clone(link));
                            row.links.len() - 1
                        });
                    index + 1
                }
            };
            let converted = MirrorCell::from_cell(cell, row.text.len(), link);
            row.text.push_str(cell.str());
            row.has_images |= converted.has_image();
            if let Some(images) = cell.attrs().images() {
                row.images
                    .extend(images.into_iter().map(|image| MirrorImage {
                        col: cell.cell_index(),
                        image,
                    }));
            }
            row.cells.push(converted);
        }
        row
    }

    pub fn stable(&self) -> StableRowIndex {
        self.stable
    }
    pub fn seqno(&self) -> SequenceNo {
        self.seqno
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn cells(&self) -> &[MirrorCell] {
        &self.cells
    }
    pub fn all_text(&self) -> &str {
        &self.text
    }
    pub fn hyperlinks(&self) -> &[Arc<Hyperlink>] {
        &self.links
    }
    pub fn has_images(&self) -> bool {
        self.has_images
    }
    pub fn images(&self) -> &[MirrorImage] {
        &self.images
    }
    pub fn always_recapture(&self) -> bool {
        self.has_images
    }

    pub fn text(&self, cell: &MirrorCell) -> &str {
        cell.text_start
            .checked_add(cell.text_len)
            .and_then(|end| self.text.get(cell.text_start..end))
            .unwrap_or("")
    }

    pub fn hyperlink(&self, cell: &MirrorCell) -> Option<&Arc<Hyperlink>> {
        cell.link
            .checked_sub(1)
            .and_then(|index| self.links.get(index))
    }

    /// Rebuild attributes used by the existing desktop shaping path. Image
    /// payloads remain in the separate image list; wrapped state is not a style.
    pub fn cell_attributes(&self, cell: &MirrorCell) -> CellAttributes {
        let mut attrs = CellAttributes::default();
        attrs
            .set_intensity(cell.intensity())
            .set_underline(cell.underline())
            .set_blink(cell.blink())
            .set_italic(cell.italic())
            .set_reverse(cell.reverse())
            .set_strikethrough(cell.strikethrough())
            .set_invisible(cell.invisible())
            .set_overline(cell.overline())
            .set_semantic_type(cell.semantic_type())
            .set_vertical_align(cell.vertical_align())
            .set_foreground(cell.fg().to_attribute())
            .set_background(cell.bg().to_attribute());
        if cell.underline_color() != MirrorColor::Default {
            attrs.set_underline_color(cell.underline_color().to_attribute());
        }
        if let Some(link) = self.hyperlink(cell) {
            attrs.set_hyperlink(Some(Arc::clone(link)));
        }
        attrs
    }

    pub fn clusters(&self) -> Vec<CellCluster> {
        let attrs: Vec<_> = self
            .cells
            .iter()
            .map(|cell| self.cell_attributes(cell))
            .collect();
        let len = self
            .cells
            .last()
            .map_or(0, |cell| cell.col() + cell.width());
        CellCluster::make_cluster(
            len,
            self.cells
                .iter()
                .zip(&attrs)
                .map(|(cell, attrs)| CellRef::ClusterRef {
                    cell_index: cell.col(),
                    text: self.text(cell),
                    width: cell.width(),
                    attrs,
                }),
            None,
        )
    }

    pub fn to_line(&self) -> Line {
        let mut line = Line::new(self.seqno);
        for cell in &self.cells {
            line.set_cell_grapheme(
                cell.col(),
                self.text(cell),
                cell.width(),
                self.cell_attributes(cell),
                self.seqno,
            );
        }
        line
    }
}

/// Independent from the workspace schema, terminal semantic version and mux
/// codec. FTSF v1 is a direct-terminal, full-replacement render format.
pub const RENDER_FRAME_VERSION: u16 = 1;
pub const RENDER_FRAME_HEADER_BYTES: usize = 256;
pub const RENDER_FRAME_ROW_BYTES: usize = 24;
pub const RENDER_FRAME_CELL_BYTES: usize = 48;
pub const RENDER_FRAME_LINK_BYTES: usize = 8;
pub const RENDER_FRAME_PALETTE_COLORS: usize = 265;
/// Header flag: bytes 176..192 hold the active screen's oldest resident stable
/// row and its live viewport's first stable row, the range a history viewport
/// may select. A decoder that predates it rejects the flag rather than guess.
pub const RENDER_FRAME_FLAG_HISTORY: u32 = 16;

/// These witnesses are supplied by the immutable session owner, not by remote
/// terminal output. UUID bytes use RFC UUID order, not a platform GUID layout.
/// The owner increments frame/geometry/palette/font revisions without wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderFrameWitness {
    pub channel_id: [u8; 16],
    pub transport_id: [u8; 16],
    pub route_revision: u64,
    pub geometry_revision: u64,
    pub palette_revision: u64,
    pub font_revision: u64,
    pub frame_revision: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct RenderFrameLimits {
    pub max_rows: usize,
    pub max_cols: usize,
    /// Bounds both grid occupancy (including omitted blanks) and cell records.
    pub max_cells: usize,
    pub max_text_bytes: usize,
    pub max_grapheme_bytes: usize,
    pub max_links: usize,
    pub max_uri_bytes: usize,
    pub max_frame_bytes: usize,
}

impl Default for RenderFrameLimits {
    fn default() -> Self {
        Self {
            max_rows: 512,
            max_cols: 512,
            max_cells: 65_536,
            max_text_bytes: 1024 * 1024,
            max_grapheme_bytes: 4096,
            max_links: 4096,
            max_uri_bytes: 4096,
            max_frame_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderFrameError {
    InvalidLimits,
    InvalidWitness,
    InvalidGeometry,
    Capacity,
    Allocation,
    UnresidentViewport,
    UnsupportedStorage,
    UnsupportedImage,
    UnsupportedLineMode,
    UnsupportedAttributes,
    InvalidText,
    SequenceExhausted,
}

impl std::fmt::Display for RenderFrameError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "terminal render capture {:?}", self)
    }
}
impl std::error::Error for RenderFrameError {}

impl RenderFrameLimits {
    fn validate(self) -> Result<Self, RenderFrameError> {
        if !(1..=512).contains(&self.max_rows)
            || !(1..=512).contains(&self.max_cols)
            || !(1..=65_536).contains(&self.max_cells)
            || !(1..=1024 * 1024).contains(&self.max_text_bytes)
            || !(1..=16_384).contains(&self.max_grapheme_bytes)
            || self.max_links > 4096
            || !(1..=4096).contains(&self.max_uri_bytes)
            || !(RENDER_FRAME_HEADER_BYTES..=8 * 1024 * 1024).contains(&self.max_frame_bytes)
        {
            return Err(RenderFrameError::InvalidLimits);
        }
        Ok(self)
    }
}

#[derive(Default)]
struct FrameCounts {
    cells: usize,
    text: usize,
    links: usize,
    uris: usize,
}

fn checked_add(left: usize, right: usize) -> Result<usize, RenderFrameError> {
    left.checked_add(right).ok_or(RenderFrameError::Capacity)
}

fn checked_mul(left: usize, right: usize) -> Result<usize, RenderFrameError> {
    left.checked_mul(right).ok_or(RenderFrameError::Capacity)
}

fn safe_text(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(|c| c.is_control())
}

/// Count borrowed semantic cells before copying any text, hyperlink or image
/// metadata. `attrs.images()` allocates; this path deliberately never calls it.
fn count_frame(
    lines: &[&Line],
    cols: usize,
    sequence: SequenceNo,
    limits: RenderFrameLimits,
) -> Result<FrameCounts, RenderFrameError> {
    let mut counts = FrameCounts::default();
    for line in lines {
        if line.current_seqno() > sequence {
            return Err(RenderFrameError::InvalidWitness);
        }
        let (bidi_enabled, direction) = line.bidi_info();
        if !line.is_single_width()
            || bidi_enabled
            || direction.direction() != Direction::LeftToRight
        {
            return Err(RenderFrameError::UnsupportedLineMode);
        }
        if line.has_image_attachments() {
            return Err(RenderFrameError::UnsupportedImage);
        }
        if line.len() > cols {
            return Err(RenderFrameError::InvalidGeometry);
        }
        let mut previous_end = 0;
        for cell in line.visible_cells() {
            let end = checked_add(cell.cell_index(), cell.width())?;
            if !(1..=2).contains(&cell.width()) || cell.cell_index() < previous_end || end > cols {
                return Err(RenderFrameError::InvalidGeometry);
            }
            previous_end = end;
            let attrs = cell.attrs();
            if attrs.has_image_attachments() {
                return Err(RenderFrameError::UnsupportedImage);
            }
            let flags = MirrorCell::flags_of(attrs);
            if flags & !0x1_bfff != 0
                || (flags & 3) > 2
                || ((flags >> 2) & 7) > 5
                || ((flags >> 5) & 3) > 2
                || ((flags >> 12) & 3) > 2
                || ((flags >> 15) & 3) > 2
            {
                return Err(RenderFrameError::UnsupportedAttributes);
            }
            if cell.str().len() > limits.max_grapheme_bytes || !safe_text(cell.str()) {
                return Err(RenderFrameError::InvalidText);
            }
            counts.cells = checked_add(counts.cells, 1)?;
            counts.text = checked_add(counts.text, cell.str().len())?;
            if let Some(link) = attrs.hyperlink() {
                let uri = link.uri();
                if uri.len() > limits.max_uri_bytes || !safe_text(uri) {
                    return Err(RenderFrameError::InvalidText);
                }
                counts.links = checked_add(counts.links, 1)?;
                counts.uris = checked_add(counts.uris, uri.len())?;
            }
            if counts.cells > limits.max_cells
                || counts.links > limits.max_links
                || checked_add(counts.text, counts.uris)? > limits.max_text_bytes
            {
                return Err(RenderFrameError::Capacity);
            }
        }
    }
    Ok(counts)
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
fn put_i64(bytes: &mut [u8], at: usize, value: i64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
fn put_count(bytes: &mut [u8], at: usize, value: usize) -> Result<(), RenderFrameError> {
    put_u32(
        bytes,
        at,
        u32::try_from(value).map_err(|_| RenderFrameError::Capacity)?,
    );
    Ok(())
}
fn put_color(bytes: &mut [u8], at: usize, color: MirrorColor) {
    let (tag, value) = match color {
        MirrorColor::Default => (0, 0),
        MirrorColor::Palette(index) => (1, u32::from(index)),
        MirrorColor::Rgba(rgba) => (2, u32::from_le_bytes(rgba)),
    };
    put_u32(bytes, at, tag);
    put_u32(bytes, at + 4, value);
}

/// Capture a bounded resident viewport from the canonical terminal. This does
/// not mutate parser state, scan hyperlinks, read cold storage, or emit replies.
/// The caller owns the terminal exclusively for this call and associates the
/// supplied witnesses with its exact direct-SSH channel. A source switch must
/// create a new consumer; frame witnesses confer no connection authority.
///
/// The default [`GridEngine::Legacy`] storage is supported. The opt-in Page
/// engine is rejected before reading a row: its lazy legacy views copy glyphs
/// and attributes and may compress text before the cell preflight can bound
/// those allocations. A native Page adapter must provide its own bounded
/// preflight before that storage can be admitted here.
///
/// FTSF v1 uses little-endian integers and RFC-order UUID bytes. The 256-byte
/// header is followed, without padding, by row24 records, cell48 records,
/// link8 records, one UTF-8 pool, and 265 RGBA8 palette entries. Text spans are
/// contiguous: all cell strings first, then one URI per linked cell in order.
/// Links identify frame-local URI entries, not OSC 8 remote authority.
///
/// Header offsets: magic0, version-u16@4, header-size-u16@6, total-u32@8,
/// flags-u32@12 (full=1/reverse=2/alternate=4/synchronized=8/history=16), channelUUID@16,
/// transportUUID@32, route-revision-u64@48, terminal-sequence-u64@56,
/// geometry/palette/font/frame revisions-u64@64/72/80/88, rows/cols-u32@96/100,
/// viewport/cursor stable rows-i64@104/112, cursor-column/shape-u32@120/124,
/// row/cell/text counts-u32@128/132/136, row/cell/text/palette offsets-u32
/// @140/144/148/152, palette-count-u32@156, link-count/offset-u32@160/164,
/// cursor-visible/width-u32@168/172, oldest-resident/live-first stable rows
/// -i64@176/184 (history flag). Bytes192..256 are zero.
///
/// Row24: stable-i64, sequence-u64, first-cell-u32, cell-count-u32.
/// Cell48: column, width, text-start, text-length, flags, foreground-tag/value,
/// background-tag/value, underline-tag/value, link-id; all u32. Color tags are
/// default0(value0), palette1(value0..255), RGBA2(value is bytes r,g,b,a).
/// Link8: UTF-8 start/length-u32. Link0 means none; other IDs are one-based.
/// Palette0..255 are ANSI, followed by foreground, background, cursor foreground,
/// cursor background, cursor border, selection foreground/background, scrollbar,
/// split. Cursor shape0..6 follows Default/BlinkingBlock/SteadyBlock/
/// BlinkingUnderline/SteadyUnderline/BlinkingBar/SteadyBar; visibility0/1.
///
/// Cell flags retain the shared mirror's layout: intensity0..1, underline2..4,
/// blink5..6, italic7, reverse8, strike9, invisible10, overline11, vertical
/// alignment12..13, image14 (rejected), semantic type15..16. Images, bidi and
/// double-size lines fail explicitly until their native representation exists.
pub fn capture_terminal_frame(
    term: &Terminal,
    witness: RenderFrameWitness,
    viewport_top: Option<StableRowIndex>,
    limits: RenderFrameLimits,
) -> Result<Vec<u8>, RenderFrameError> {
    let limits = limits.validate()?;
    if witness.channel_id == [0; 16]
        || witness.transport_id == [0; 16]
        || [
            witness.route_revision,
            witness.geometry_revision,
            witness.palette_revision,
            witness.font_revision,
            witness.frame_revision,
        ]
        .contains(&0)
    {
        return Err(RenderFrameError::InvalidWitness);
    }
    let screen = term.screen();
    if screen.grid_engine() != GridEngine::Legacy {
        return Err(RenderFrameError::UnsupportedStorage);
    }
    let rows = screen.physical_rows;
    let cols = screen.physical_cols;
    if rows == 0
        || cols == 0
        || rows > limits.max_rows
        || cols > limits.max_cols
        || checked_mul(rows, cols)? > limits.max_cells
    {
        return Err(RenderFrameError::InvalidGeometry);
    }
    let sequence = term.current_seqno();
    if sequence == SequenceNo::MAX {
        return Err(RenderFrameError::SequenceExhausted);
    }
    let live_first = screen.visible_row_to_stable_row(0);
    let oldest = screen.phys_to_stable_row_index(0);
    let first = viewport_top.unwrap_or(live_first);
    let end = first
        .checked_add(StableRowIndex::try_from(rows).map_err(|_| RenderFrameError::InvalidGeometry)?)
        .ok_or(RenderFrameError::InvalidGeometry)?;
    if first < oldest || first > live_first {
        return Err(RenderFrameError::UnresidentViewport);
    }
    let physical = screen.stable_range(&(first..end));
    if physical.len() != rows || screen.phys_to_stable_row_index(physical.start) != first {
        return Err(RenderFrameError::UnresidentViewport);
    }
    let cursor = term.cursor_pos();
    if cursor.x >= cols || cursor.y < 0 || usize::try_from(cursor.y).ok().is_none_or(|y| y >= rows)
    {
        return Err(RenderFrameError::InvalidGeometry);
    }
    let cursor_stable = screen.visible_row_to_stable_row(cursor.y);
    let cursor_width = screen
        .phys_line(screen.phys_row(cursor.y))
        .get_cell(cursor.x)
        .map_or(1, |cell| cell.width().clamp(1, 2).min(cols - cursor.x));
    let mut captured = Err(RenderFrameError::UnresidentViewport);
    // For admitted Legacy storage, with_phys_lines allocates only a bounded
    // rows-sized array of borrowed references; all variable cell/text/image
    // copying is after count_frame. Page's lazy views were rejected above.
    screen.with_phys_lines(physical, |lines| {
        captured = encode_frame(
            term,
            witness,
            FrameRows {
                first,
                oldest,
                live_first,
            },
            cursor_stable,
            cursor_width,
            lines,
            limits,
        );
    });
    captured
}

/// Stable rows of the captured viewport's first line and of the history range
/// it may select within: the oldest retained line and the live screen's first.
struct FrameRows {
    first: StableRowIndex,
    oldest: StableRowIndex,
    live_first: StableRowIndex,
}

fn encode_frame(
    term: &Terminal,
    witness: RenderFrameWitness,
    FrameRows {
        first,
        oldest,
        live_first,
    }: FrameRows,
    cursor_stable: StableRowIndex,
    cursor_width: usize,
    lines: &[&Line],
    limits: RenderFrameLimits,
) -> Result<Vec<u8>, RenderFrameError> {
    let rows = term.screen().physical_rows;
    let cols = term.screen().physical_cols;
    if lines.len() != rows {
        return Err(RenderFrameError::UnresidentViewport);
    }
    let counts = count_frame(lines, cols, term.current_seqno(), limits)?;
    let rows_at = RENDER_FRAME_HEADER_BYTES;
    let cells_at = checked_add(rows_at, checked_mul(rows, RENDER_FRAME_ROW_BYTES)?)?;
    let links_at = checked_add(
        cells_at,
        checked_mul(counts.cells, RENDER_FRAME_CELL_BYTES)?,
    )?;
    let text_at = checked_add(
        links_at,
        checked_mul(counts.links, RENDER_FRAME_LINK_BYTES)?,
    )?;
    let text_bytes = checked_add(counts.text, counts.uris)?;
    let palette_at = checked_add(text_at, text_bytes)?;
    let total = checked_add(palette_at, RENDER_FRAME_PALETTE_COLORS * 4)?;
    if total > limits.max_frame_bytes {
        return Err(RenderFrameError::Capacity);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total)
        .map_err(|_| RenderFrameError::Allocation)?;
    bytes.resize(total, 0);
    bytes[0..4].copy_from_slice(b"FTSF");
    bytes[4..6].copy_from_slice(&RENDER_FRAME_VERSION.to_le_bytes());
    bytes[6..8].copy_from_slice(&(RENDER_FRAME_HEADER_BYTES as u16).to_le_bytes());
    put_count(&mut bytes, 8, total)?;
    let flags = 1
        | if term.get_reverse_video() { 2 } else { 0 }
        | if term.is_alt_screen_active() { 4 } else { 0 }
        | if term.synchronized_output() { 8 } else { 0 }
        | RENDER_FRAME_FLAG_HISTORY;
    put_u32(&mut bytes, 12, flags);
    bytes[16..32].copy_from_slice(&witness.channel_id);
    bytes[32..48].copy_from_slice(&witness.transport_id);
    put_u64(&mut bytes, 48, witness.route_revision);
    put_u64(
        &mut bytes,
        56,
        u64::try_from(term.current_seqno()).map_err(|_| RenderFrameError::Capacity)?,
    );
    for (at, value) in [
        (64, witness.geometry_revision),
        (72, witness.palette_revision),
        (80, witness.font_revision),
        (88, witness.frame_revision),
    ] {
        put_u64(&mut bytes, at, value);
    }
    for (at, value) in [
        (96, rows),
        (100, cols),
        (128, rows),
        (132, counts.cells),
        (136, text_bytes),
        (140, rows_at),
        (144, cells_at),
        (148, text_at),
        (152, palette_at),
        (156, RENDER_FRAME_PALETTE_COLORS),
        (160, counts.links),
        (164, links_at),
        (172, cursor_width),
    ] {
        put_count(&mut bytes, at, value)?;
    }
    put_i64(
        &mut bytes,
        104,
        i64::try_from(first).map_err(|_| RenderFrameError::InvalidGeometry)?,
    );
    put_i64(
        &mut bytes,
        112,
        i64::try_from(cursor_stable).map_err(|_| RenderFrameError::InvalidGeometry)?,
    );
    for (at, row) in [(176, oldest), (184, live_first)] {
        put_i64(
            &mut bytes,
            at,
            i64::try_from(row).map_err(|_| RenderFrameError::InvalidGeometry)?,
        );
    }
    let cursor = term.cursor_pos();
    put_count(&mut bytes, 120, cursor.x)?;
    put_u32(
        &mut bytes,
        124,
        match cursor.shape {
            CursorShape::Default => 0,
            CursorShape::BlinkingBlock => 1,
            CursorShape::SteadyBlock => 2,
            CursorShape::BlinkingUnderline => 3,
            CursorShape::SteadyUnderline => 4,
            CursorShape::BlinkingBar => 5,
            CursorShape::SteadyBar => 6,
        },
    );
    put_u32(
        &mut bytes,
        168,
        u32::from(cursor.visibility == CursorVisibility::Visible),
    );

    let mut cell_index = 0;
    let mut text_index = 0;
    let mut link_index = 0;
    let mut uri_index = counts.text;
    for (row, line) in lines.iter().enumerate() {
        let row_at = rows_at + row * RENDER_FRAME_ROW_BYTES;
        let stable = first
            .checked_add(
                StableRowIndex::try_from(row).map_err(|_| RenderFrameError::InvalidGeometry)?,
            )
            .ok_or(RenderFrameError::InvalidGeometry)?;
        put_i64(
            &mut bytes,
            row_at,
            i64::try_from(stable).map_err(|_| RenderFrameError::InvalidGeometry)?,
        );
        put_u64(
            &mut bytes,
            row_at + 8,
            u64::try_from(line.current_seqno()).map_err(|_| RenderFrameError::Capacity)?,
        );
        put_count(&mut bytes, row_at + 16, cell_index)?;
        let first_cell = cell_index;
        for cell in line.visible_cells() {
            let link = if let Some(link) = cell.attrs().hyperlink() {
                let at = links_at + link_index * RENDER_FRAME_LINK_BYTES;
                let uri = link.uri().as_bytes();
                put_count(&mut bytes, at, uri_index)?;
                put_count(&mut bytes, at + 4, uri.len())?;
                bytes[text_at + uri_index..text_at + uri_index + uri.len()].copy_from_slice(uri);
                uri_index += uri.len();
                link_index += 1;
                link_index
            } else {
                0
            };
            // Exactly the same cell conversion as the desktop MirrorRow.
            let converted = MirrorCell::from_cell(cell, text_index, link);
            let at = cells_at + cell_index * RENDER_FRAME_CELL_BYTES;
            for (offset, value) in [
                (0, converted.col),
                (4, converted.width),
                (8, converted.text_start),
                (12, converted.text_len),
                (44, converted.link),
            ] {
                put_count(&mut bytes, at + offset, value)?;
            }
            put_u32(&mut bytes, at + 16, converted.flags);
            put_color(&mut bytes, at + 20, converted.fg);
            put_color(&mut bytes, at + 28, converted.bg);
            put_color(&mut bytes, at + 36, converted.underline_color);
            let text = cell.str().as_bytes();
            bytes[text_at + text_index..text_at + text_index + text.len()].copy_from_slice(text);
            text_index += text.len();
            cell_index += 1;
        }
        put_count(&mut bytes, row_at + 20, cell_index - first_cell)?;
    }
    let palette = term.palette();
    let extras = [
        palette.foreground,
        palette.background,
        palette.cursor_fg,
        palette.cursor_bg,
        palette.cursor_border,
        palette.selection_fg,
        palette.selection_bg,
        palette.scrollbar_thumb,
        palette.split,
    ];
    for (index, color) in palette.colors.0.iter().chain(extras.iter()).enumerate() {
        let at = palette_at + index * 4;
        bytes[at..at + 4].copy_from_slice(&rgba8(*color));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use crate::{BoundedOutputLimits, TerminalConfiguration, TerminalSize};
    use std::convert::TryInto;
    use termwiz::cell::Cell;

    #[derive(Debug)]
    struct Config;
    impl TerminalConfiguration for Config {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
        fn grid_engine(&self) -> GridEngine {
            GridEngine::Legacy
        }
    }

    fn terminal(rows: usize, cols: usize) -> Terminal {
        Terminal::new_bounded_output(
            TerminalSize {
                rows,
                cols,
                pixel_width: cols * 8,
                pixel_height: rows * 16,
                dpi: 96,
            },
            Arc::new(Config),
            "FrankenTerm",
            "render-frame-test",
            BoundedOutputLimits::default(),
        )
        .unwrap()
    }

    fn witness() -> RenderFrameWitness {
        RenderFrameWitness {
            channel_id: [0x11; 16],
            transport_id: [0x22; 16],
            route_revision: 7,
            geometry_revision: 3,
            palette_revision: 5,
            font_revision: 11,
            frame_revision: 13,
        }
    }

    fn frame(term: &Terminal) -> Vec<u8> {
        capture_terminal_frame(term, witness(), None, RenderFrameLimits::default()).unwrap()
    }

    // These readers intentionally use the published numeric offsets, not the
    // writer's constants or helpers. The expected terminal contents are literal
    // ANSI/Unicode examples, not a round trip through a second implementation.
    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }
    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }
    fn i64_at(bytes: &[u8], at: usize) -> i64 {
        i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }
    fn cell_at(bytes: &[u8], index: usize) -> usize {
        u32_at(bytes, 144) as usize + index * 48
    }
    fn cell_text(bytes: &[u8], index: usize) -> &str {
        let at = cell_at(bytes, index);
        let start = u32_at(bytes, 148) as usize + u32_at(bytes, at + 8) as usize;
        let end = start + u32_at(bytes, at + 12) as usize;
        std::str::from_utf8(&bytes[start..end]).unwrap()
    }

    #[test]
    fn real_terminal_ansi_wide_and_combining_cells_have_owned_semantic_frames() {
        let mut term = terminal(3, 10);
        term.advance_bytes("\x1b[1;31mA\x1b[0m界e\u{301}\r\n\x1b[38;2;17;34;51mZ");
        let bytes = frame(&term);
        assert_eq!(&bytes[..8], b"FTSF\x01\x00\x00\x01");
        assert_eq!(u32_at(&bytes, 8) as usize, bytes.len());
        assert_eq!((u32_at(&bytes, 96), u32_at(&bytes, 100)), (3, 10));
        assert_eq!(&bytes[16..32], &[0x11; 16]);
        assert_eq!(&bytes[32..48], &[0x22; 16]);
        assert_eq!((u64_at(&bytes, 48), u64_at(&bytes, 88)), (7, 13));
        assert_eq!(
            u32_at(&bytes, 12) & RENDER_FRAME_FLAG_HISTORY,
            RENDER_FRAME_FLAG_HISTORY
        );
        assert_eq!((i64_at(&bytes, 176), i64_at(&bytes, 184)), (0, 0));
        assert!(bytes[192..256].iter().all(|byte| *byte == 0));
        assert_eq!(u32_at(&bytes, 140), 256);
        assert_eq!(u32_at(&bytes, 144), 256 + 3 * 24);
        assert_eq!(u32_at(&bytes, 156), 265);
        assert_eq!(cell_text(&bytes, 0), "A");
        let first = cell_at(&bytes, 0);
        assert_eq!(u32_at(&bytes, first + 16) & 3, 1, "bold");
        assert_eq!(
            (u32_at(&bytes, first + 20), u32_at(&bytes, first + 24)),
            (1, 1),
            "ANSI red"
        );
        assert_eq!(cell_text(&bytes, 1), "界");
        assert_eq!(
            (
                u32_at(&bytes, cell_at(&bytes, 1)),
                u32_at(&bytes, cell_at(&bytes, 1) + 4)
            ),
            (1, 2)
        );
        assert_eq!(cell_text(&bytes, 2), "e\u{301}");
        assert_eq!(
            u32_at(&bytes, cell_at(&bytes, 2)),
            3,
            "wide spacer is not a cell record"
        );
        let row1 = 256 + 24;
        let second_row_first = u32_at(&bytes, row1 + 16) as usize;
        assert_eq!(cell_text(&bytes, second_row_first), "Z");
        let z = cell_at(&bytes, second_row_first);
        assert_eq!(u32_at(&bytes, z + 20), 2);
        assert_eq!(&bytes[z + 24..z + 28], &[17, 34, 51, 255]);
        assert_eq!((i64_at(&bytes, 112), u32_at(&bytes, 120)), (1, 1));
        term.advance_bytes("\x1b[2Jreplaced");
        assert_eq!(
            cell_text(&bytes, 0),
            "A",
            "an acquired frame owns its old text"
        );
    }

    #[test]
    fn arbitrary_utf8_transport_boundaries_use_the_existing_parser() {
        let mut term = terminal(2, 10);
        let text = "A界🦀e\u{301}";
        for byte in text.as_bytes() {
            term.advance_bytes([*byte]);
        }
        let bytes = frame(&term);
        assert_eq!(cell_text(&bytes, 0), "A");
        assert_eq!(cell_text(&bytes, 1), "界");
        assert_eq!(cell_text(&bytes, 2), "🦀");
        assert_eq!(cell_text(&bytes, 3), "e\u{301}");
    }

    #[test]
    fn canonical_desktop_and_native_conversion_preserve_long_graphemes() {
        let text = format!("a{}", "\u{301}".repeat(300));
        assert_eq!(text.len(), 601);
        let mut line = Line::new(1);
        line.set_cell_grapheme(0, &text, 1, CellAttributes::default(), 1);
        let row = MirrorRow::from_line(0, &line, 1);
        assert_eq!(
            row.text(&row.cells()[0]),
            text,
            "old mirror truncated after 255 bytes"
        );
        assert_eq!(row.to_line().get_cell(0).unwrap().str(), text);
        let mut term = terminal(2, 8);
        term.increment_seqno();
        let sequence = term.current_seqno();
        term.screen_mut()
            .set_cell_grapheme(0, 0, &text, 1, CellAttributes::default(), sequence);
        let bytes = frame(&term);
        assert_eq!(cell_text(&bytes, 0), text);
        assert_eq!(u32_at(&bytes, cell_at(&bytes, 0) + 12), 601);
    }

    #[test]
    fn hyperlinks_are_owned_bounded_and_follow_cell_text_in_the_pool() {
        let mut term = terminal(2, 6);
        term.advance_bytes("\x1b]8;;https://example.test/a\x1b\\AB\x1b]8;;\x1b\\");
        let bytes = frame(&term);
        assert_eq!(
            u32_at(&bytes, 160),
            2,
            "one bounded URI entry per linked cell"
        );
        assert_eq!(u32_at(&bytes, cell_at(&bytes, 0) + 44), 1);
        assert_eq!(u32_at(&bytes, cell_at(&bytes, 1) + 44), 2);
        let links = u32_at(&bytes, 164) as usize;
        let pool = u32_at(&bytes, 148) as usize;
        let uri_start = u32_at(&bytes, links) as usize;
        let uri_len = u32_at(&bytes, links + 4) as usize;
        assert_eq!(
            &bytes[pool + uri_start..pool + uri_start + uri_len],
            b"https://example.test/a"
        );
        let last = cell_at(&bytes, u32_at(&bytes, 132) as usize - 1);
        assert_eq!(
            uri_start,
            (u32_at(&bytes, last + 8) + u32_at(&bytes, last + 12)) as usize
        );
        let limits = RenderFrameLimits {
            max_links: 1,
            ..RenderFrameLimits::default()
        };
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, limits),
            Err(RenderFrameError::Capacity)
        );
    }

    #[test]
    fn admission_checks_geometry_text_and_total_bytes_without_truncating() {
        let mut term = terminal(2, 10);
        term.advance_bytes("abcdefghij");
        let mut limits = RenderFrameLimits {
            max_cells: 19,
            ..RenderFrameLimits::default()
        };
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, limits),
            Err(RenderFrameError::InvalidGeometry)
        );
        limits = RenderFrameLimits::default();
        limits.max_text_bytes = 5;
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, limits),
            Err(RenderFrameError::Capacity)
        );
        limits = RenderFrameLimits::default();
        limits.max_frame_bytes = 256;
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, limits),
            Err(RenderFrameError::Capacity)
        );
        let original = frame(&term);
        assert_eq!(cell_text(&original, 0), "a");
        limits = RenderFrameLimits::default();
        limits.max_rows = usize::MAX;
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, limits),
            Err(RenderFrameError::InvalidLimits)
        );
    }

    #[test]
    fn unsupported_line_modes_and_images_fail_instead_of_changing_semantics() {
        let mut term = terminal(2, 8);
        term.advance_bytes("\x1b#6wide");
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, RenderFrameLimits::default()),
            Err(RenderFrameError::UnsupportedLineMode)
        );
        let mut term = terminal(2, 8);
        term.increment_seqno();
        let sequence = term.current_seqno();
        term.screen_mut()
            .line_mut(0)
            .set_bidi_enabled(true, sequence);
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, RenderFrameLimits::default()),
            Err(RenderFrameError::UnsupportedLineMode)
        );
        let mut term = terminal(2, 8);
        term.increment_seqno();
        let sequence = term.current_seqno();
        term.screen_mut()
            .line_mut(0)
            .set_direction(Direction::RightToLeft, false, sequence);
        assert!(
            !term.screen().phys_line(0).bidi_info().0,
            "layout direction is independent of bidi"
        );
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, RenderFrameLimits::default()),
            Err(RenderFrameError::UnsupportedLineMode)
        );
        use termwiz::image::{ImageData, ImageDataType, TextureCoordinate};
        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![255; 4],
        )));
        let mut attrs = CellAttributes::default();
        attrs.attach_image(Box::new(ImageCell::with_z_index(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            image,
            0,
            0,
            0,
            0,
            0,
            None,
            None,
        )));
        let mut term = terminal(2, 8);
        term.increment_seqno();
        let sequence = term.current_seqno();
        term.screen_mut()
            .set_cell(0, 0, &Cell::new(' ', attrs), sequence);
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, RenderFrameLimits::default()),
            Err(RenderFrameError::UnsupportedImage)
        );
    }

    #[test]
    fn future_row_sequences_are_rejected_before_capture() {
        let mut term = terminal(2, 8);
        let future = term.current_seqno() + 1;
        term.screen_mut()
            .line_mut(0)
            .update_last_change_seqno(future);
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, RenderFrameLimits::default()),
            Err(RenderFrameError::InvalidWitness)
        );
        term.increment_seqno();
        let bytes = frame(&term);
        assert_eq!(u64_at(&bytes, 256 + 8), u64_at(&bytes, 56));
    }

    #[test]
    fn page_storage_is_rejected_before_lazy_legacy_views_are_materialized() {
        #[derive(Debug)]
        struct PageConfig;
        impl TerminalConfiguration for PageConfig {
            fn color_palette(&self) -> ColorPalette {
                ColorPalette::default()
            }
            fn grid_engine(&self) -> GridEngine {
                GridEngine::Page
            }
        }
        let term = Terminal::new_bounded_output(
            TerminalSize {
                rows: 2,
                cols: 8,
                pixel_width: 64,
                pixel_height: 32,
                dpi: 96,
            },
            Arc::new(PageConfig),
            "FrankenTerm",
            "render-page-test",
            BoundedOutputLimits::default(),
        )
        .unwrap();
        assert_eq!(GridEngine::default(), GridEngine::Legacy);
        assert_eq!(term.screen().grid_engine(), GridEngine::Page);
        assert_eq!(
            capture_terminal_frame(&term, witness(), None, RenderFrameLimits::default()),
            Err(RenderFrameError::UnsupportedStorage)
        );
        assert_eq!(
            term.screen().grid_engine(),
            GridEngine::Page,
            "capture must not coerce storage"
        );
        let legacy = terminal(2, 8);
        assert_eq!(legacy.screen().grid_engine(), GridEngine::Legacy);
        assert!(
            !frame(&legacy).is_empty(),
            "the default storage remains supported"
        );
    }

    #[test]
    fn capture_is_inert_and_does_not_reissue_terminal_replies() {
        let mut term = terminal(2, 8);
        term.advance_bytes("ok\x1b[6n");
        let before = term.bounded_output_status().unwrap();
        let first = frame(&term);
        let second = frame(&term);
        assert_eq!(first, second);
        assert_eq!(term.bounded_output_status().unwrap(), before);
        let mut invalid = witness();
        invalid.geometry_revision = 0;
        assert_eq!(
            capture_terminal_frame(&term, invalid, None, RenderFrameLimits::default()),
            Err(RenderFrameError::InvalidWitness)
        );
        assert_eq!(
            capture_terminal_frame(&term, witness(), Some(-1), RenderFrameLimits::default()),
            Err(RenderFrameError::UnresidentViewport)
        );
    }

    #[test]
    fn palette_changes_and_alternate_screen_are_captured_from_the_terminal() {
        let mut term = terminal(2, 8);
        term.advance_bytes("\x1b]4;1;rgb:11/22/33\x1b\\\x1b[?1049h\x1b[?25lalt");
        let bytes = frame(&term);
        assert_ne!(u32_at(&bytes, 12) & 4, 0);
        assert_eq!(u32_at(&bytes, 168), 0);
        let palette = u32_at(&bytes, 152) as usize;
        assert_eq!(&bytes[palette + 4..palette + 8], &[17, 34, 51, 255]);
        assert_eq!(cell_text(&bytes, 0), "a");
    }
}
