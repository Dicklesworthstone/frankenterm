//! Semantic rows shared by the desktop render mirror and native consumers.
//!
//! This is the canonical cell conversion, extracted from the live mux renderer.
//! It does not parse terminal bytes, shape text, allocate glyphs, or emit effects.
//! [`capture_terminal_frame`] reads the actual terminal and produces one bounded,
//! owned full replacement in the explicit FTSF format. No Rust layout or borrowed
//! terminal pointer crosses the language boundary.

use crate::config::GridEngine;
use crate::pagegrid::{CellStyle, Glyph, PackedCell, Page, RichStyle, RowHeader};
use crate::{Line, StableRowIndex, Terminal};
use frankenterm_bidi::Direction;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;
use termwiz::cell::{
    Blink, Cell, CellAttributes, Intensity, SemanticType, Underline, VerticalAlign,
};
use termwiz::cellcluster::CellCluster;
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::hyperlink::{Hyperlink, Rule};
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

/// DEC line sizing, independent of cell widths and terminal column count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MirrorLineMode {
    #[default]
    SingleWidth,
    DoubleWidth,
    DoubleHeightTop,
    DoubleHeightBottom,
}

impl MirrorLineMode {
    fn from_line(line: &Line) -> Self {
        if line.is_double_height_top() {
            Self::DoubleHeightTop
        } else if line.is_double_height_bottom() {
            Self::DoubleHeightBottom
        } else if line.is_double_width() {
            Self::DoubleWidth
        } else {
            Self::SingleWidth
        }
    }

    fn from_header(header: RowHeader) -> Self {
        if header.has(RowHeader::DOUBLE_HEIGHT_TOP) {
            Self::DoubleHeightTop
        } else if header.has(RowHeader::DOUBLE_HEIGHT_BOTTOM) {
            Self::DoubleHeightBottom
        } else if header.has(RowHeader::DOUBLE_WIDTH) {
            Self::DoubleWidth
        } else {
            Self::SingleWidth
        }
    }

    fn apply(self, line: &mut Line, seqno: SequenceNo) {
        match self {
            Self::SingleWidth => {}
            Self::DoubleWidth => line.set_double_width(seqno),
            Self::DoubleHeightTop => line.set_double_height_top(seqno),
            Self::DoubleHeightBottom => line.set_double_height_bottom(seqno),
        }
    }
}

/// A terminal row borrowed only while its terminal lock is held. Looking up
/// a native row or its damage facts does not construct a legacy `Line` view.
pub enum RenderRowRef<'a> {
    Line(&'a Line),
    Page {
        page: &'a Page,
        row: u32,
        seqno: SequenceNo,
        /// A live legacy view would additionally compress this scrollback row.
        compress: bool,
    },
}

impl RenderRowRef<'_> {
    /// Line geometry without constructing a native row's legacy view.
    pub fn line_mode(&self) -> MirrorLineMode {
        match self {
            Self::Line(line) => MirrorLineMode::from_line(line),
            Self::Page { page, row, .. } => MirrorLineMode::from_header(page.header(*row)),
        }
    }

    /// The cursor's logical width at an exact visible-cell start. A cursor
    /// explicitly placed on a wide character's spacer covers one cell, as
    /// does a cursor past the row's stored content.
    pub fn cursor_cell_width(&self, col: usize) -> u8 {
        match self {
            Self::Line(line) => line
                .get_cell(col)
                .map_or(1, |cell| cell.width().clamp(1, 2) as u8),
            Self::Page { page, row, .. } => {
                if col < page.row_len(*row) {
                    let cell = page.cell(*row, col);
                    if !cell.is_hidden() && cell.is_wide() {
                        return 2;
                    }
                }
                1
            }
        }
    }

    pub fn seqno(&self) -> SequenceNo {
        match self {
            Self::Line(line) => line.current_seqno(),
            Self::Page { seqno, .. } => *seqno,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Line(line) => line.len(),
            Self::Page { page, row, .. } => page.row_len(*row),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn last_cell_was_wrapped(&self) -> bool {
        match self {
            Self::Line(line) => line.last_cell_was_wrapped(),
            Self::Page { page, row, .. } => page
                .row_words(*row)
                .iter()
                .rev()
                .map(|word| PackedCell::from_bits(*word))
                .find(|cell| !cell.is_hidden())
                .is_some_and(PackedCell::is_wrapped),
        }
    }

    pub fn implicit_hyperlinks_are_scanned(&self) -> bool {
        match self {
            Self::Line(line) => line.implicit_hyperlinks_are_scanned(),
            // A hyperlink scan edits a Line, making that view authoritative.
            // A native write invalidates that scan when it returns to pages.
            Self::Page { .. } => false,
        }
    }
}

/// An owned snapshot of one native page row. Copying retains raw packed
/// words and only their referenced side entries. No page reference escapes
/// the terminal lock; style/color conversion happens in `update_from_packed`
/// after the caller releases it. All scratch capacities survive recapture.
#[derive(Debug, Clone, Default)]
pub struct PackedMirrorRow {
    seqno: SequenceNo,
    header: RowHeader,
    compress: bool,
    cells: Vec<u64>,
    styles: HashMap<u32, RichStyle>,
    graphemes: Vec<(usize, std::ops::Range<usize>)>,
    text: String,
    links: Vec<(usize, Arc<Hyperlink>)>,
    images: Vec<MirrorImage>,
}

impl PackedMirrorRow {
    /// Copies one row and returns its visible cell count. The effective
    /// sequence includes PageList damage floors, not just the page word.
    pub fn capture(&mut self, page: &Page, row: u32, seqno: SequenceNo) -> usize {
        self.clear();
        self.seqno = seqno;
        self.header = page.header(row);
        self.cells.extend_from_slice(page.row_words(row));
        let mut visible = 0;
        let mut last_rich_id = 0;
        for (col, word) in self.cells.iter().enumerate() {
            let cell = PackedCell::from_bits(*word);
            if !cell.is_hidden() {
                visible += 1;
            }
            if let CellStyle::Rich(id) = cell.style() {
                if id != last_rich_id {
                    self.styles
                        .entry(id)
                        .or_insert_with(|| *page.styles().get(id).expect("a live rich style"));
                    last_rich_id = id;
                }
            }
            if cell.has_grapheme() {
                if let Glyph::Cluster(text) = page.glyph(row, col) {
                    let start = self.text.len();
                    self.text.push_str(text);
                    self.graphemes.push((col, start..self.text.len()));
                }
            }
            if cell.has_hyperlink() {
                if let Some(link) = page.hyperlink(row, col) {
                    self.links.push((col, Arc::clone(link)));
                }
            }
            if cell.has_image() {
                if let Some(images) = page.cell_images(row, col) {
                    self.images.extend(images.iter().map(|image| MirrorImage {
                        col,
                        image: (**image).clone(),
                    }));
                }
            }
        }
        visible
    }

    /// Releases captured side-data owners after resolution, retaining only
    /// their reusable allocations. A subsequent legacy-row capture must not
    /// leave an old native image or hyperlink payload retained here.
    pub fn clear(&mut self) {
        self.header = RowHeader::default();
        self.compress = false;
        self.cells.clear();
        self.styles.clear();
        self.graphemes.clear();
        self.text.clear();
        self.links.clear();
        self.images.clear();
    }

    /// Exact legacy representation, made solely from owned bytes after the
    /// terminal lock is released. Hidden cells retain their own attributes;
    /// they can become visible when a truncated wide cell is concatenated.
    fn to_line(&self, clear_implicit: bool) -> Line {
        let mut grapheme = 0;
        let mut hyperlink = 0;
        let mut image = 0;
        let cells = self
            .cells
            .iter()
            .enumerate()
            .map(|(col, word)| {
                let cell = PackedCell::from_bits(*word);
                let mut attrs = crate::pagegrid::cell::style_attributes_with(cell.style(), |id| {
                    *self.styles.get(&id).expect("a copied rich style")
                });
                attrs
                    .set_semantic_type(cell.semantic())
                    .set_wrapped(cell.is_wrapped())
                    .set_protected(cell.is_protected());
                if let Some((_, link)) = self.links.get(hyperlink).filter(|(at, _)| *at == col) {
                    if !clear_implicit || !link.is_implicit() {
                        attrs.set_hyperlink(Some(Arc::clone(link)));
                    }
                    hyperlink += 1;
                }
                while let Some(attachment) = self.images.get(image).filter(|item| item.col == col) {
                    attrs.attach_image(Box::new(attachment.image.clone()));
                    image += 1;
                }
                let mut utf8 = [0_u8; 4];
                let text = if let Some((_, range)) =
                    self.graphemes.get(grapheme).filter(|(at, _)| *at == col)
                {
                    grapheme += 1;
                    &self.text[range.clone()]
                } else {
                    cell.first_char().unwrap_or(' ').encode_utf8(&mut utf8)
                };
                Cell::new_grapheme_with_width(text, if cell.is_wide() { 2 } else { 1 }, attrs)
            })
            .collect();
        let mut line = crate::pagegrid::view::line_from_cells(cells, self.header, self.seqno);
        if self.compress {
            line.compress_for_scrollback();
        }
        line
    }
}

/// One immutable source row owned by a pending render capture. Native rows
/// retain packed bytes; only an already-authoritative legacy row is cloned.
#[derive(Debug, Clone, Default)]
pub struct RenderRowSnapshot {
    packed: PackedMirrorRow,
    line: Option<Line>,
}

impl RenderRowSnapshot {
    pub fn capture(&mut self, source: RenderRowRef<'_>) -> usize {
        match source {
            RenderRowRef::Line(line) => {
                self.packed.clear();
                self.line = Some(line.clone());
                line.visible_cells().count()
            }
            RenderRowRef::Page {
                page,
                row,
                seqno,
                compress,
            } => {
                self.line = None;
                let cells = self.packed.capture(page, row, seqno);
                self.packed.compress = compress;
                cells
            }
        }
    }

    pub fn is_packed(&self) -> bool {
        self.line.is_none()
    }

    pub fn resolve(&self, row: &mut MirrorRow, stable: StableRowIndex, generation: u64) {
        match &self.line {
            Some(line) => row.update_from_line(stable, line, generation),
            None => row.update_from_packed(stable, &self.packed, generation),
        }
    }

    /// Consumes a captured legacy owner without cloning it a second time.
    /// Native reconstruction borrows only this snapshot's own copied bytes.
    /// It removes old implicit side entries before rebuilding storage: an
    /// imported page can carry them without the legacy scan-state bits that
    /// `Line::invalidate_implicit_hyperlinks` requires.
    pub fn take_line(&mut self) -> Line {
        match self.line.take() {
            Some(line) => line,
            None => self.packed.to_line(true),
        }
    }

    pub fn clear(&mut self) {
        self.packed.clear();
        self.line = None;
    }
}

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
    line_mode: MirrorLineMode,
    physical_len: usize,
    wrapped: bool,
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
            line_mode: MirrorLineMode::SingleWidth,
            physical_len: 0,
            wrapped: false,
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
        row.update_from_line(stable, line, generation);
        row
    }

    /// Replaces this row's contents while retaining its cell and text storage.
    /// The desktop mirror calls this under the terminal lock, so steady output
    /// reuses the previous row's allocations instead of freeing and allocating
    /// them on every frame. Shared image and hyperlink handles are refreshed.
    pub fn update_from_line(&mut self, stable: StableRowIndex, line: &Line, generation: u64) {
        self.clear(stable, generation);
        self.seqno = line.current_seqno();
        self.line_mode = MirrorLineMode::from_line(line);
        self.physical_len = line.len();
        self.wrapped = line.last_cell_was_wrapped();
        self.cells.reserve(line.len());
        self.text.reserve(line.len());
        for cell in line.visible_cells() {
            let link = match cell.attrs().hyperlink() {
                None => 0,
                Some(link) => {
                    let index = self
                        .links
                        .iter()
                        .position(|known| Arc::ptr_eq(known, link) || **known == **link)
                        .unwrap_or_else(|| {
                            self.links.push(Arc::clone(link));
                            self.links.len() - 1
                        });
                    index + 1
                }
            };
            let converted = MirrorCell::from_cell(cell, self.text.len(), link);
            self.text.push_str(cell.str());
            self.has_images |= converted.has_image();
            if let Some(images) = cell.attrs().images() {
                self.images
                    .extend(images.into_iter().map(|image| MirrorImage {
                        col: cell.cell_index(),
                        image,
                    }));
            }
            self.cells.push(converted);
        }
    }

    /// Resolves a copied native row after releasing the terminal lock. The
    /// conversion writes the same semantic cells as `update_from_line`,
    /// without constructing temporary `Cell`, `CellAttributes` or `Line`
    /// objects for packed text. Only the snapshot's own side entries are read.
    pub fn update_from_packed(
        &mut self,
        stable: StableRowIndex,
        packed: &PackedMirrorRow,
        generation: u64,
    ) {
        self.clear(stable, generation);
        self.seqno = packed.seqno;
        self.line_mode = MirrorLineMode::from_header(packed.header);
        self.physical_len = packed.cells.len();
        self.wrapped = packed
            .cells
            .iter()
            .rev()
            .map(|word| PackedCell::from_bits(*word))
            .find(|cell| !cell.is_hidden())
            .is_some_and(PackedCell::is_wrapped);
        self.cells.reserve(packed.cells.len());
        self.text.reserve(packed.cells.len());
        let mut grapheme = 0;
        let mut hyperlink = 0;
        let mut image = 0;
        let mut last_rich_id = 0;
        let mut rich_colors = (
            0,
            MirrorColor::Default,
            MirrorColor::Default,
            MirrorColor::Default,
        );
        for (col, word) in packed.cells.iter().enumerate() {
            let cell = PackedCell::from_bits(*word);
            if cell.is_hidden() {
                continue;
            }
            while packed
                .graphemes
                .get(grapheme)
                .is_some_and(|(at, _)| *at < col)
            {
                grapheme += 1;
            }
            while packed.links.get(hyperlink).is_some_and(|(at, _)| *at < col) {
                hyperlink += 1;
            }
            while packed.images.get(image).is_some_and(|item| item.col < col) {
                image += 1;
            }
            let (flags, fg, bg, underline_color) = match cell.style() {
                CellStyle::Inline(style) => {
                    let color = |code: u16| match code {
                        0 => MirrorColor::Default,
                        code => MirrorColor::Palette((code - 1).min(255) as u8),
                    };
                    (
                        u32::from(style.attrs()),
                        color(style.fg()),
                        color(style.bg()),
                        MirrorColor::Default,
                    )
                }
                CellStyle::Rich(id) => {
                    // True-color runs commonly repeat one style for most
                    // of a row. Decode its colors once and avoid a hash
                    // lookup and float conversion for every text cell.
                    if id != last_rich_id {
                        let style = packed.styles.get(&id).expect("a copied rich style");
                        rich_colors = (
                            u32::from(style.attrs),
                            MirrorColor::from_attribute(style.fg),
                            MirrorColor::from_attribute(style.bg),
                            MirrorColor::from_attribute(style.underline_color),
                        );
                        last_rich_id = id;
                    }
                    rich_colors
                }
            };
            let text_start = self.text.len();
            if let Some((_, range)) = packed.graphemes.get(grapheme).filter(|(at, _)| *at == col) {
                self.text.push_str(&packed.text[range.clone()]);
                grapheme += 1;
            } else {
                self.text.push(cell.first_char().unwrap_or(' '));
            }
            let link =
                if let Some((_, link)) = packed.links.get(hyperlink).filter(|(at, _)| *at == col) {
                    hyperlink += 1;
                    self.links
                        .iter()
                        .position(|known| Arc::ptr_eq(known, link) || **known == **link)
                        .unwrap_or_else(|| {
                            self.links.push(Arc::clone(link));
                            self.links.len() - 1
                        })
                        + 1
                } else {
                    0
                };
            let image_start = self.images.len();
            while let Some(attachment) = packed.images.get(image).filter(|item| item.col == col) {
                // Match Page::cell_attributes -> attach_image exactly, even
                // for equal z-indices whose insertion order may be reversed.
                let at = self.images[image_start..]
                    .binary_search_by(|known| {
                        known.image.z_index().cmp(&attachment.image.z_index())
                    })
                    .unwrap_or_else(|at| at);
                self.images.insert(image_start + at, attachment.clone());
                image += 1;
            }
            let has_images = self.images.len() != image_start;
            self.has_images |= has_images;
            self.cells.push(MirrorCell {
                text_start,
                text_len: self.text.len() - text_start,
                col,
                width: if cell.is_wide() { 2 } else { 1 },
                flags: flags
                    | ((cell.semantic() as u32) << SEMANTIC_SHIFT)
                    | if has_images { IMAGE } else { 0 },
                link,
                fg,
                bg,
                underline_color,
            });
        }
    }

    /// Replaces the contents with an empty row without releasing reusable
    /// backing storage. Clearing still drops all image and hyperlink handles.
    pub fn clear(&mut self, stable: StableRowIndex, generation: u64) {
        self.stable = stable;
        self.seqno = SEQ_ZERO;
        self.generation = generation;
        self.line_mode = MirrorLineMode::SingleWidth;
        self.physical_len = 0;
        self.wrapped = false;
        self.cells.clear();
        self.text.clear();
        self.links.clear();
        self.has_images = false;
        self.images.clear();
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
    pub fn line_mode(&self) -> MirrorLineMode {
        self.line_mode
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

    /// Removes render-generated links while preserving OSC 8 links and all
    /// cell geometry. Rule changes belong to this mirror, never to the source
    /// terminal's global "already scanned" bit.
    pub fn clear_implicit_hyperlinks(&mut self) {
        let mut kept = 0;
        for old in 0..self.links.len() {
            let replacement = if self.links[old].is_implicit() {
                0
            } else {
                self.links.swap(old, kept);
                kept += 1;
                kept
            };
            if replacement != old + 1 {
                for cell in &mut self.cells {
                    if cell.link == old + 1 {
                        cell.link = replacement;
                    }
                }
            }
        }
        self.links.truncate(kept);
    }

    fn set_cell_hyperlink(&mut self, cell: usize, link: &Arc<Hyperlink>) {
        // Rule matches arrive longest first. As VecStorage writeback does,
        // preserve the first accepted match when later rules overlap it.
        if self.cells[cell].link != 0 {
            return;
        }
        let index = self
            .links
            .iter()
            .position(|known| Arc::ptr_eq(known, link) || **known == **link)
            .unwrap_or_else(|| {
                self.links.push(Arc::clone(link));
                self.links.len() - 1
            });
        self.cells[cell].link = index + 1;
    }

    fn canonicalize_hyperlinks(&mut self) {
        let mut kept = 0;
        for index in 0..self.cells.len() {
            let link = self.cells[index].link;
            if link == 0 || link <= kept {
                continue;
            }
            let next = kept + 1;
            self.links.swap(link - 1, kept);
            if link != next {
                for cell in &mut self.cells {
                    if cell.link == link {
                        cell.link = next;
                    } else if cell.link == next {
                        cell.link = link;
                    }
                }
            }
            kept = next;
        }
        self.links.truncate(kept);
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
        self.line_mode.apply(&mut line, self.seqno);
        line.set_last_cell_was_wrapped(self.wrapped, self.seqno);
        line
    }
}

#[derive(Debug, Clone)]
struct MirrorScanCell {
    bytes: std::ops::Range<usize>,
    row: usize,
    cell: usize,
}

/// Reusable rule resolution over owned semantic rows. It mirrors the Line
/// scanner's byte-to-cell alignment, wide padding and explicit-link barriers.
/// Ambiguous concatenation uses the exact owned-Line fallback instead.
#[derive(Debug, Clone, Default)]
pub struct MirrorHyperlinkScanner {
    text: String,
    cells: Vec<MirrorScanCell>,
}

impl MirrorHyperlinkScanner {
    /// Resolves one complete, already bounded logical group. A refusal leaves
    /// rows untouched so the caller can run Line::apply_hyperlink_rules on
    /// its original owned snapshots. No live terminal reference is accepted.
    pub fn apply(&mut self, rows: &mut [MirrorRow], rules: &[Rule]) -> bool {
        if !rules.is_empty() && !Self::can_scan(rows) {
            return false;
        }
        for row in rows.iter_mut() {
            row.clear_implicit_hyperlinks();
        }
        if rules.is_empty() {
            return true;
        }

        self.text.clear();
        self.cells.clear();
        for row in 0..rows.len() {
            for index in 0..rows[row].cells.len() {
                let cell = rows[row].cells[index];
                if rows[row].hyperlink(&cell).is_some() || cell.width == 0 {
                    self.finish_run(rows, rules);
                    continue;
                }
                let start = self.text.len();
                self.text.push_str(rows[row].text(&cell));
                let end = self.text.len();
                if start != end {
                    self.cells.push(MirrorScanCell {
                        bytes: start..end,
                        row,
                        cell: index,
                    });
                    for _ in 1..cell.width {
                        self.text.push(' ');
                    }
                }
            }
        }
        self.finish_run(rows, rules);
        for row in rows.iter_mut() {
            row.canonicalize_hyperlinks();
        }
        // Line's split-back path carries the logical maximum when any link,
        // including an explicit link, survives. Source damage is tracked
        // independently by the capture's exact per-row sequence certificate.
        if rows.iter().any(|row| !row.links.is_empty()) {
            let seqno = rows.iter().map(|row| row.seqno).max().unwrap_or(SEQ_ZERO);
            for row in rows {
                row.seqno = seqno;
            }
        }
        true
    }

    fn can_scan(rows: &[MirrorRow]) -> bool {
        const MAX_SCAN_BYTES: usize = 64 * 1024;
        const MAX_SCAN_CELLS: usize = 1024;
        let mut bytes = 0usize;
        let mut cells = 0usize;
        let mut previous = None;
        let mode = rows.first().map(|row| row.line_mode);
        for row in rows {
            // The legacy concatenation/split-back path preserves its first
            // row's line flags. Let that path handle mixed DEC line modes.
            if Some(row.line_mode) != mode {
                return false;
            }
            let mut column = 0usize;
            for cell in &row.cells {
                cells += 1;
                if cells > MAX_SCAN_CELLS {
                    return false;
                }
                if cell.col != column || !(1..=2).contains(&cell.width) {
                    return false;
                }
                column = column.saturating_add(cell.width);
                let glyph = row.text(cell);
                let Some(next_bytes) = bytes
                    .checked_add(glyph.len())
                    .and_then(|len| len.checked_add(cell.width - 1))
                    .filter(|len| *len <= MAX_SCAN_BYTES)
                else {
                    return false;
                };
                bytes = next_bytes;
                let Some(first) = glyph.chars().next() else {
                    return false;
                };
                if first.len_utf8() != glyph.len() {
                    let mut graphemes = finl_unicode::grapheme_clusters::Graphemes::new(glyph);
                    if graphemes.next() != Some(glyph) || graphemes.next().is_some() {
                        return false;
                    }
                }
                if !frankenterm_surface::line::clustered_append_breaks(previous, first) {
                    return false;
                }
                // Artificial wide padding is not part of clustered storage;
                // carry the true scalar across cells and physical rows.
                previous = if first.len_utf8() == glyph.len() {
                    Some(first)
                } else {
                    glyph.chars().next_back()
                };
            }
            if column != row.physical_len {
                // A missing wide spacer can consume the next physical row's
                // first cell when legacy Lines are appended.
                return false;
            }
        }
        true
    }

    fn finish_run(&mut self, rows: &mut [MirrorRow], rules: &[Rule]) {
        if !self.text.is_empty() {
            for matched in Rule::match_hyperlinks(&self.text, rules) {
                let Some(first) = self
                    .cells
                    .iter()
                    .position(|cell| cell.bytes.start == matched.range.start)
                else {
                    continue;
                };
                let mut end = first;
                let mut aligned = false;
                for cell in &self.cells[first..] {
                    if cell.bytes.start >= matched.range.end || cell.bytes.end > matched.range.end {
                        break;
                    }
                    end += 1;
                    if cell.bytes.end == matched.range.end {
                        aligned = true;
                        break;
                    }
                }
                if aligned {
                    for cell in &self.cells[first..end] {
                        rows[cell.row].set_cell_hyperlink(cell.cell, &matched.link);
                    }
                }
            }
        }
        self.text.clear();
        self.cells.clear();
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
/// Header flag: bytes 192..256 are a least-significant-bit-first bitmap of
/// canonical soft-wrap boundaries, one bit per captured row. A set bit means
/// that row continues into the next physical row; unused bits are zero. The
/// viewport may end before the continuation, so consumers must also validate
/// adjacent row/source/geometry witnesses before joining copied text.
pub const RENDER_FRAME_FLAG_SOFT_WRAPS: u32 = 32;
/// The canonical terminal currently requests mouse reports. This is a display
/// hint for gesture arbitration, never input authority or an encoding mode.
pub const RENDER_FRAME_FLAG_MOUSE_GRABBED: u32 = 64;

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
/// flags-u32@12 (full=1/reverse=2/alternate=4/synchronized=8/history=16/soft-wraps=32/mouse-grabbed=64), channelUUID@16,
/// transportUUID@32, route-revision-u64@48, terminal-sequence-u64@56,
/// geometry/palette/font/frame revisions-u64@64/72/80/88, rows/cols-u32@96/100,
/// viewport/cursor stable rows-i64@104/112, cursor-column/shape-u32@120/124,
/// row/cell/text counts-u32@128/132/136, row/cell/text/palette offsets-u32
/// @140/144/148/152, palette-count-u32@156, link-count/offset-u32@160/164,
/// cursor-visible/width-u32@168/172, oldest-resident/live-first stable rows
/// -i64@176/184 (history flag). Bytes192..256 are the soft-wrap bitmap when
/// flag32 is set: row N uses bit N%8 of byte 192+N/8. Bits for uncaptured rows
/// are zero. Without flag32 all 64 bytes are zero. Table sizes and offsets are
/// unchanged; pre-extension decoders reject the new flag rather than guess.
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
        | if term.is_mouse_grabbed() { RENDER_FRAME_FLAG_MOUSE_GRABBED } else { 0 }
        | RENDER_FRAME_FLAG_HISTORY
        | RENDER_FRAME_FLAG_SOFT_WRAPS;
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
        if line.last_cell_was_wrapped() {
            bytes[192 + row / 8] |= 1 << (row % 8);
        }
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
    struct Config(GridEngine);
    impl TerminalConfiguration for Config {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
        fn grid_engine(&self) -> GridEngine {
            self.0
        }
    }

    fn terminal(rows: usize, cols: usize) -> Terminal {
        terminal_with_engine(rows, cols, GridEngine::Legacy)
    }

    fn terminal_with_engine(rows: usize, cols: usize, engine: GridEngine) -> Terminal {
        Terminal::new_bounded_output(
            TerminalSize {
                rows,
                cols,
                pixel_width: cols * 8,
                pixel_height: rows * 16,
                dpi: 96,
            },
            Arc::new(Config(engine)),
            "FrankenTerm",
            "render-frame-test",
            BoundedOutputLimits::default(),
        )
        .unwrap()
    }

    #[test]
    fn native_snapshot_and_row_facts_construct_no_legacy_line_views() {
        let mut term = terminal_with_engine(2, 24, GridEngine::Page);
        term.advance_bytes("\x1b[1;31mfirst row\r\n\x1b[38;2;17;34;51msecond row");
        assert_eq!(term.screen().cached_page_views(), 0);
        let mut rows = Vec::new();
        for physical in 0..2 {
            let source = term.screen().render_row(physical).unwrap();
            assert!(!source.is_empty());
            assert!(!source.last_cell_was_wrapped());
            assert!(!source.implicit_hyperlinks_are_scanned());
            let RenderRowRef::Page {
                page, row, seqno, ..
            } = source
            else {
                panic!("ordinary native output should remain packed");
            };
            let mut packed = PackedMirrorRow::default();
            let count = packed.capture(page, row, seqno);
            let mut converted = MirrorRow::empty(physical as StableRowIndex, 9);
            converted.update_from_packed(physical as StableRowIndex, &packed, 9);
            assert_eq!(converted.cells().len(), count);
            rows.push(converted);
        }
        assert_eq!(
            term.screen().cached_page_views(),
            0,
            "both metadata reads and packed capture must avoid OnceLock Line views"
        );
        for (physical, row) in rows.iter().enumerate() {
            let reference = MirrorRow::from_line(
                physical as StableRowIndex,
                term.screen().phys_line(physical),
                9,
            );
            assert_eq!(*row, reference);
        }
        assert_eq!(
            term.screen().cached_page_views(),
            2,
            "the oracle builds views"
        );

        term.increment_seqno();
        let seqno = term.current_seqno();
        term.screen_mut()
            .line_mut(0)
            .set_cell(0, Cell::new('!', CellAttributes::blank()), seqno);
        assert!(matches!(
            term.screen().render_row(0),
            Some(RenderRowRef::Line(_))
        ));
        assert!(matches!(
            term.screen().render_row(1),
            Some(RenderRowRef::Page { .. })
        ));
    }

    #[test]
    fn cursor_geometry_reads_native_headers_and_exact_cell_starts_without_views() {
        for (escape, mode) in [
            ("\x1b#5", MirrorLineMode::SingleWidth),
            ("\x1b#6", MirrorLineMode::DoubleWidth),
            ("\x1b#3", MirrorLineMode::DoubleHeightTop),
            ("\x1b#4", MirrorLineMode::DoubleHeightBottom),
        ] {
            let mut term = terminal_with_engine(2, 8, GridEngine::Page);
            term.advance_bytes(format!("\x1b[2;1H{escape}你x\x1b[2;1H"));
            let source = term.screen().render_row(1).unwrap();
            assert!(matches!(source, RenderRowRef::Page { .. }));
            assert_eq!(source.line_mode(), mode);
            for (col, width) in [(0, 2), (1, 1), (2, 1), (7, 1), (usize::MAX, 1)] {
                assert_eq!(source.cursor_cell_width(col), width, "{mode:?} at {col}");
            }
            assert_eq!(term.screen().cached_page_views(), 0);
            let legacy = RenderRowRef::Line(term.screen().phys_line(1));
            assert_eq!(legacy.line_mode(), mode);
            for (col, width) in [(0, 2), (1, 1), (2, 1), (7, 1), (usize::MAX, 1)] {
                assert_eq!(legacy.cursor_cell_width(col), width, "{mode:?} at {col}");
            }

            term.increment_seqno();
            let seqno = term.current_seqno();
            let line = term.screen_mut().line_mut(1);
            line.set_single_width(seqno);
            line.set_cell(0, Cell::new('n', CellAttributes::blank()), seqno);
            let source = term.screen().render_row(1).unwrap();
            assert!(matches!(source, RenderRowRef::Line(_)));
            assert_eq!(source.line_mode(), MirrorLineMode::SingleWidth);
            assert_eq!(source.cursor_cell_width(0), 1);
        }
    }

    #[test]
    fn packed_rows_match_legacy_views_with_wide_cells_clusters_and_rich_styles() {
        use crate::pagegrid::view::{line_view, store_line};

        let streams = [
            "\x1b[1;3;4:3;5;7;9;53mstyled\x1b[0m\r\n\x1b[2;8;74mhalf\x1b[0m",
            "\x1b[38;5;200;48;5;21mindexed\r\n\x1b[38;2;17;34;51;48;2;91;81;71;58;2;4;5;6mtrue color",
            "界界界界界界界e\u{301} 👩\u{200d}💻 ❤\u{fe0f} 🇯🇵",
            "\x1b]8;;https://example.test/explicit\x1b\\wide界link\x1b]8;;\x1b\\ tail",
            "\x1b]133;A\x1b\\prompt\x1b]133;B\x1b\\input\x1b]133;C\x1b\\output",
        ];
        for stream in streams {
            let mut term = terminal(4, 12);
            term.advance_bytes(stream);
            for line in term.screen().all_lines() {
                for compressed in [false, true] {
                    let mut line = line.clone();
                    if compressed {
                        line.compress_for_scrollback();
                    }
                    let mut page = Page::new(12, 1, 1);
                    let row = page.grow(1).unwrap();
                    assert!(store_line(&mut page, row, &line));
                    let mut packed = PackedMirrorRow::default();
                    packed.capture(&page, row, 77);
                    let view = line_view(&page, row, 77);
                    let facts = RenderRowRef::Page {
                        page: &page,
                        row,
                        seqno: 77,
                        compress: false,
                    };
                    assert_eq!(facts.len(), view.len());
                    assert_eq!(facts.last_cell_was_wrapped(), view.last_cell_was_wrapped());
                    let expected = MirrorRow::from_line(19, &view, 8);

                    // No page pointer, style id, or grapheme-arena borrow may
                    // outlive capture, including when storage is recycled.
                    drop(page);
                    assert_eq!(packed.to_line(false), view, "owned legacy fallback");
                    let mut converted = MirrorRow::empty(0, 0);
                    converted.update_from_packed(19, &packed, 8);
                    assert_eq!(
                        converted, expected,
                        "stream {stream:?}, compressed={compressed}"
                    );
                }
            }
        }
    }

    #[test]
    fn owned_hyperlink_scan_keeps_wide_padding_and_rejects_ambiguous_cells() {
        use crate::pagegrid::view::store_line;

        let mut page = Page::new(32, 4, 1);
        let wide = page.grow(1).unwrap();
        assert!(store_line(
            &mut page,
            wide,
            &Line::from_text("界https://example.test", &CellAttributes::blank(), 1, None)
        ));
        let convert = |page: &Page, row| {
            let mut packed = PackedMirrorRow::default();
            packed.capture(page, row, 1);
            let mut converted = MirrorRow::empty(0, 0);
            converted.update_from_packed(0, &packed, 0);
            converted
        };
        let rules = [Rule::new(r"https?://\S+", "$0").unwrap()];
        let mut scanner = MirrorHyperlinkScanner::default();
        let mut rows = [convert(&page, wide)];
        assert!(scanner.apply(&mut rows, &rules));
        assert!(rows[0].hyperlink(&rows[0].cells()[0]).is_none());
        assert_eq!(
            rows[0].hyperlink(&rows[0].cells()[1]).unwrap().uri(),
            "https://example.test"
        );

        // A cell may legally hold more than one grapheme. Legacy clustered
        // append can split it, so a negative scan must not guess its padding.
        let multiple = page.grow(1).unwrap();
        let line = Line::from_cells(
            vec![
                Cell::new_grapheme_with_width("AB", 2, CellAttributes::blank()),
                Cell::blank(),
            ],
            1,
        );
        assert!(store_line(&mut page, multiple, &line));
        let mut rows = [convert(&page, multiple)];
        let original = rows.clone();
        assert!(!scanner.apply(&mut rows, &rules));
        assert_eq!(rows, original, "a refusal leaves the owned fallback intact");

        let truncated = page.grow(1).unwrap();
        let line = Line::from_cells(vec![Cell::new('界', CellAttributes::blank())], 1);
        assert!(store_line(&mut page, truncated, &line));
        assert!(!scanner.apply(&mut [convert(&page, truncated)], &rules));
    }

    #[test]
    fn owned_scan_bounds_bytes_and_rejects_joining_unicode_boundaries() {
        use crate::pagegrid::view::store_line;

        let mut page = Page::new(16, 2, 1);
        let first = page.grow(1).unwrap();
        let next = page.grow(1).unwrap();
        assert!(store_line(
            &mut page,
            first,
            &Line::from_cells(
                vec![Cell::new_grapheme(
                    "e\u{301}",
                    CellAttributes::blank(),
                    None
                )],
                1,
            ),
        ));
        assert!(store_line(
            &mut page,
            next,
            &Line::from_text("next", &CellAttributes::blank(), 1, None),
        ));
        let source = |row| {
            let mut packed = PackedMirrorRow::default();
            packed.capture(&page, row, 1);
            let mut converted = MirrorRow::empty(0, 0);
            converted.update_from_packed(0, &packed, 0);
            converted
        };
        assert!(MirrorHyperlinkScanner::can_scan(&[source(first)]));
        assert!(!MirrorHyperlinkScanner::can_scan(&[
            source(first),
            source(next)
        ]));

        let huge = format!("e{}", "\u{301}".repeat(40_000));
        let line = Line::from_cells(
            vec![Cell::new_grapheme(&huge, CellAttributes::blank(), None)],
            1,
        );
        let mut scanner = MirrorHyperlinkScanner::default();
        let rules = [Rule::new("e", "$0").unwrap()];
        assert!(!scanner.apply(&mut [MirrorRow::from_line(0, &line, 0)], &rules));
        assert_eq!(
            scanner.text.capacity(),
            0,
            "refusal cannot grow the scan buffer"
        );
    }

    #[test]
    fn owned_hyperlink_rules_match_legacy_overlap_barriers_and_byte_alignment() {
        let rules = [
            Rule::new("abc LINK abc", "https://crossing.test").unwrap(),
            Rule::new("abc", "https://long.test").unwrap(),
            Rule::new("b", "https://short.test").unwrap(),
            Rule::new("界 ", "https://padding.test").unwrap(),
            Rule::new("e", "https://partial-grapheme.test").unwrap(),
            Rule::new("^", "https://empty.test").unwrap(),
        ];
        let mut explicit = Line::from_text("abc LINK abc", &CellAttributes::blank(), 7, None);
        let link = Arc::new(Hyperlink::new("https://explicit.test"));
        for (offset, ch) in "LINK".chars().enumerate() {
            let mut attrs = CellAttributes::blank();
            attrs.set_hyperlink(Some(Arc::clone(&link)));
            explicit.set_cell(4 + offset, Cell::new(ch, attrs), 7);
        }
        explicit.set_has_hyperlink(true);
        for line in [
            explicit,
            Line::from_text("界abc", &CellAttributes::blank(), 7, None),
            Line::from_text("e\u{301}", &CellAttributes::blank(), 7, None),
        ] {
            let mut expected = line.clone();
            Line::apply_hyperlink_rules(&rules, &mut [&mut expected]);
            let mut rows = [MirrorRow::from_line(0, &line, 1)];
            assert!(MirrorHyperlinkScanner::default().apply(&mut rows, &rules));
            assert_eq!(rows[0], MirrorRow::from_line(0, &expected, 1));
            assert!(rows[0].hyperlinks().iter().all(|link| {
                !matches!(
                    link.uri(),
                    "https://short.test"
                        | "https://padding.test"
                        | "https://partial-grapheme.test"
                        | "https://empty.test"
                        | "https://crossing.test"
                )
            }));
        }
    }

    #[test]
    fn packed_fallback_keeps_hidden_side_entries_without_exposing_them() {
        use crate::pagegrid::view::{line_view, store_line};
        let mut hidden_attrs = CellAttributes::blank();
        hidden_attrs
            .set_foreground(ColorAttribute::TrueColorWithPaletteFallback(
                SrgbaTuple(0.2, 0.4, 0.6, 0.5),
                19,
            ))
            .set_hyperlink(Some(Arc::new(Hyperlink::new("https://hidden.test"))));
        let mut visible_attrs = CellAttributes::blank();
        visible_attrs.set_hyperlink(Some(Arc::new(Hyperlink::new("https://visible.test"))));
        let line = Line::from_cells(
            vec![
                Cell::new('界', CellAttributes::blank()),
                Cell::new_grapheme_with_width("hidden", 1, hidden_attrs),
                Cell::new_grapheme_with_width("e\u{301}", 1, visible_attrs),
            ],
            9,
        );
        let mut page = Page::new(8, 1, 1);
        let native = page.grow(9).unwrap();
        assert!(store_line(&mut page, native, &line));
        let expected = line_view(&page, native, 9);
        let mut packed = PackedMirrorRow::default();
        packed.capture(&page, native, 9);
        drop(page);
        assert_eq!(packed.to_line(false), expected);
        let mut row = MirrorRow::empty(0, 0);
        row.update_from_packed(0, &packed, 1);
        assert_eq!(row, MirrorRow::from_line(0, &expected, 1));
        assert_eq!(row.all_text(), "界e\u{301}");
        assert_eq!(row.hyperlinks().len(), 1);
        assert_eq!(row.hyperlinks()[0].uri(), "https://visible.test");
    }

    #[test]
    fn packed_fallback_discards_imported_implicit_links_without_scan_state() {
        use crate::pagegrid::view::store_line;

        let old = Arc::new(Hyperlink::new_implicit("https://old.test/AB"));
        let explicit = Arc::new(Hyperlink::new("https://explicit.test"));
        let mut implicit_attrs = CellAttributes::blank();
        implicit_attrs.set_hyperlink(Some(Arc::clone(&old)));
        let mut explicit_attrs = CellAttributes::blank();
        explicit_attrs.set_hyperlink(Some(Arc::clone(&explicit)));
        let source = Line::from_cells(
            vec![
                Cell::new_grapheme_with_width("AB", 2, implicit_attrs.clone()),
                Cell::blank_with_attrs(implicit_attrs),
                Cell::new('E', explicit_attrs),
            ],
            9,
        );
        assert!(!source.implicit_hyperlinks_are_scanned());
        let mut page = Page::new(8, 1, 1);
        let native = page.grow(9).unwrap();
        assert!(store_line(&mut page, native, &source));
        let mut snapshot = RenderRowSnapshot::default();
        snapshot.capture(RenderRowRef::Page {
            page: &page,
            row: native,
            seqno: 9,
            compress: false,
        });
        drop(page);
        let rules = [Rule::new("AB", "https://new.test/$0").unwrap()];
        let mut rows = [MirrorRow::empty(0, 0)];
        snapshot.resolve(&mut rows[0], 0, 1);
        assert!(!MirrorHyperlinkScanner::default().apply(&mut rows, &rules));

        let mut owned = snapshot.take_line();
        assert!(owned.get_cell(0).unwrap().attrs().hyperlink().is_none());
        assert_eq!(
            owned.get_cell(2).unwrap().attrs().hyperlink(),
            Some(&explicit)
        );
        Line::apply_hyperlink_rules(&rules, &mut [&mut owned]);
        assert_eq!(
            owned
                .get_cell(0)
                .unwrap()
                .attrs()
                .hyperlink()
                .unwrap()
                .uri(),
            "https://new.test/AB"
        );
        assert_eq!(
            owned.get_cell(2).unwrap().attrs().hyperlink(),
            Some(&explicit)
        );
        assert_eq!(source.get_cell(0).unwrap().attrs().hyperlink(), Some(&old));
    }

    #[test]
    fn dec_line_modes_survive_native_snapshot_clear_and_line_roundtrip() {
        use crate::pagegrid::view::store_line;
        for mode in [
            MirrorLineMode::SingleWidth,
            MirrorLineMode::DoubleWidth,
            MirrorLineMode::DoubleHeightTop,
            MirrorLineMode::DoubleHeightBottom,
        ] {
            let mut line = Line::from_text("wide", &CellAttributes::blank(), 7, None);
            mode.apply(&mut line, 7);
            let mut page = Page::new(12, 1, 1);
            let row = page.grow(7).unwrap();
            assert!(store_line(&mut page, row, &line));
            let mut snapshot = RenderRowSnapshot::default();
            snapshot.capture(RenderRowRef::Page {
                page: &page,
                row,
                seqno: 7,
                compress: false,
            });
            drop(page);
            let mut converted = MirrorRow::empty(0, 0);
            snapshot.resolve(&mut converted, 0, 1);
            assert_eq!(converted.line_mode(), mode);
            assert_eq!(converted, MirrorRow::from_line(0, &line, 1));
            assert_eq!(MirrorLineMode::from_line(&converted.to_line()), mode);
            assert_eq!(MirrorLineMode::from_line(&snapshot.take_line()), mode);
            converted.clear(0, 2);
            assert_eq!(converted.line_mode(), MirrorLineMode::SingleWidth);
        }
    }

    #[test]
    fn packed_snapshot_owns_images_and_links_and_matches_equal_z_order() {
        use crate::pagegrid::view::{line_view, store_line};
        use termwiz::image::{ImageData, ImageDataType, TextureCoordinate};

        let data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            2,
            1,
            vec![255, 0, 0, 255, 0, 0, 255, 255],
        )));
        let released = Arc::downgrade(&data);
        let mut attrs = CellAttributes::blank();
        attrs
            .set_foreground(ColorAttribute::TrueColorWithPaletteFallback(
                SrgbaTuple(0.2, 0.4, 0.6, 0.5),
                9,
            ))
            .set_underline_color(ColorAttribute::PaletteIndex(17))
            .set_semantic_type(SemanticType::Prompt)
            .set_hyperlink(Some(Arc::new(Hyperlink::new("https://example.test/image"))));
        for (left, z) in [(0.0, 2), (0.5, -1), (0.5, 2)] {
            attrs.attach_image(Box::new(ImageCell::with_z_index(
                TextureCoordinate::new_f32(left, 0.0),
                TextureCoordinate::new_f32(left + 0.5, 1.0),
                Arc::clone(&data),
                z,
                1,
                2,
                3,
                4,
                Some(7),
                None,
            )));
        }
        let mut line = Line::from_text("x", &CellAttributes::blank(), 4, None);
        line.set_cell(0, Cell::new_grapheme("e\u{301}", attrs, None), 4);
        let mut page = Page::new(8, 1, 1);
        let native = page.grow(1).unwrap();
        assert!(store_line(&mut page, native, &line));
        let expected = MirrorRow::from_line(0, &line_view(&page, native, 4), 7);
        let mut packed = PackedMirrorRow::default();
        packed.capture(&page, native, 4);
        let allocation = packed.cells.as_ptr();
        drop(page);
        drop(line);
        drop(data);

        let mut row = MirrorRow::empty(0, 0);
        row.update_from_packed(0, &packed, 7);
        assert_eq!(row, expected);
        assert_eq!(row.images().len(), 3);
        assert_eq!(row.images()[0].image.padding(), (1, 2, 3, 4));
        assert_eq!(row.hyperlinks()[0].uri(), "https://example.test/image");
        packed.clear();
        assert_eq!(
            packed.cells.as_ptr(),
            allocation,
            "scratch capacity survives"
        );
        assert!(packed.images.is_empty() && packed.links.is_empty());
        drop(expected);
        row.clear(1, 8);
        assert!(
            released.upgrade().is_none(),
            "cleared scratch cannot pin old image data"
        );
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
        assert_eq!(
            u32_at(&bytes, 12) & 32,
            32,
            "soft-wrap metadata is declared"
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
    fn soft_wrap_bitmap_distinguishes_commands_and_urls_from_real_newlines() {
        let mut term = terminal(5, 12);
        term.advance_bytes("curl https://example.test/a?b=1\r\nprintf done");
        let bytes = frame(&term);
        assert_eq!(u32_at(&bytes, 12) & 32, 32);
        assert_eq!(
            bytes[192], 0b0000_0011,
            "only the two URL continuations wrap"
        );
        assert!(bytes[193..256].iter().all(|byte| *byte == 0));
        assert_eq!(u32_at(&bytes, 140), 256, "v1 table offsets are unchanged");
        assert_eq!(u32_at(&bytes, 144), 256 + 5 * 24);

        let mut exact_width = terminal(3, 4);
        exact_width.advance_bytes("abcd");
        assert_eq!(
            frame(&exact_width)[192],
            0,
            "reaching the margin is not a wrap"
        );
        exact_width.advance_bytes("\r\nefgh");
        assert_eq!(frame(&exact_width)[192], 0, "explicit newline stays hard");
        exact_width.advance_bytes("i");
        assert_eq!(
            frame(&exact_width)[192],
            2,
            "only a printed continuation wraps"
        );
    }

    #[test]
    fn soft_wrap_bitmap_uses_canonical_wide_and_combining_cell_columns() {
        let mut term = terminal(3, 4);
        term.advance_bytes("A界e\u{301}Z");
        let bytes = frame(&term);
        assert_eq!(bytes[192], 1);
        assert_eq!(cell_text(&bytes, 0), "A");
        assert_eq!(cell_text(&bytes, 1), "界");
        assert_eq!(u32_at(&bytes, cell_at(&bytes, 1) + 4), 2);
        assert_eq!(cell_text(&bytes, 2), "e\u{301}");
        assert_eq!(u32_at(&bytes, cell_at(&bytes, 2)), 3);
        let next_row_first = u32_at(&bytes, 256 + 24 + 16) as usize;
        assert_eq!(cell_text(&bytes, next_row_first), "Z");
        assert_eq!(u32_at(&bytes, cell_at(&bytes, next_row_first)), 0);

        // A width-two grapheme that cannot fit in the final column wraps
        // without retaining a textual spacer at the previous row's end.
        let mut wide = terminal(3, 4);
        wide.advance_bytes("abc界B");
        let bytes = frame(&wide);
        assert_eq!(bytes[192], 1);
        assert_eq!(u32_at(&bytes, 256 + 20), 3);
        assert_eq!(cell_text(&bytes, 2), "c");
        let next_row_first = u32_at(&bytes, 256 + 24 + 16) as usize;
        assert_eq!(cell_text(&bytes, next_row_first), "界");
        assert_eq!(u32_at(&bytes, cell_at(&bytes, next_row_first)), 0);
        assert_eq!(u32_at(&bytes, cell_at(&bytes, next_row_first) + 4), 2);
        assert_eq!(cell_text(&bytes, next_row_first + 1), "B");
    }

    #[test]
    fn soft_wrap_bitmap_is_relative_to_the_captured_history_viewport() {
        let mut term = terminal(3, 4);
        term.advance_bytes("abcdefghijklmnop");
        let live = frame(&term);
        assert_eq!(i64_at(&live, 104), 1);
        assert_eq!(live[192], 0b0000_0011, "live rows 1 and 2 continue");
        let history =
            capture_terminal_frame(&term, witness(), Some(0), RenderFrameLimits::default())
                .unwrap();
        assert_eq!(i64_at(&history, 104), 0);
        assert_eq!(
            history[192], 0b0000_0111,
            "row 2 continues outside this viewport"
        );
        assert!(history[193..256].iter().all(|byte| *byte == 0));
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
