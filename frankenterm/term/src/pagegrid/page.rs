//! A page of rows (PageGrid ADR D4 and section 3.3).
//!
//! One `Box<[u64]>` holds `[row headers: R][row seqnos: R][cells: R x
//! (cols + 1)]`. A row header names the slot of its cell block, so rows can
//! be rotated by moving headers alone. The extra cell per row holds legacy's
//! overhang: a wide glyph written at the last column makes `len == cols + 1`.
//! Side tables are keyed by cell offset `slot * (cols + 1) + x`.
//!
//! Every mutation keeps the invariants of ADR section 6 that a page can
//! check (I1-I11 and I14) and debug-asserts the part it touched;
//! [`Page::check_invariants`] checks the whole page.

// The image map keeps legacy's `Vec<Box<ImageCell>>` per cell unchanged
// (ADR section 3.7), so boxes move between the page and legacy cells.
#![allow(clippy::vec_box)]

use super::cell::{
    classify, style_attributes, CellStyle, Glyph, InlineStyle, PackedCell, StyleClass,
};
use super::grapheme::GraphemeArena;
use super::links::{attach_image, ImageMap, LinkTable};
use super::row::RowHeader;
use super::style::{CachedRichId, RichStyle, RichStyleTable};
use frankenterm_cell::image::ImageCell;
use frankenterm_cell::{Cell, CellAttributes, Hyperlink, SemanticType};
use frankenterm_surface::SequenceNo;
use std::ops::Range;
use std::sync::Arc;

/// Cells in a standard page (D4).
pub const STD_PAGE_CELLS: usize = 32_768;

/// Rows in a standard page: `max(1, 32768 / (cols + 1))`, which is 270 at
/// 120 columns and 404 at 80.
pub fn rows_per_page(cols: u16) -> u32 {
    (STD_PAGE_CELLS / (usize::from(cols) + 1)).max(1) as u32
}

/// How a write supplies its style.
#[derive(Debug)]
pub enum StyleSpec<'a> {
    Inline(InlineStyle),
    /// A rich style and the pen's one-entry id cache (the B2.4 hook). A
    /// valid cache makes a repeat O(1) with no hashing; the write refreshes
    /// it.
    Rich {
        style: &'a RichStyle,
        cache: &'a mut Option<CachedRichId>,
    },
}

/// One legacy `set_cell`: a cell's text, width and attributes.
#[derive(Debug)]
pub struct CellWrite<'a> {
    pub glyph: Glyph<'a>,
    /// Two columns wide: the write also stores the spacer after it.
    pub wide: bool,
    pub style: StyleSpec<'a>,
    pub semantic: SemanticType,
    pub wrapped: bool,
    pub hyperlink: Option<&'a Arc<Hyperlink>>,
    pub images: &'a [Box<ImageCell>],
    /// Legacy `set_cell_clearing_image_placements`: drop, rather than carry
    /// over, the overwritten cell's placement images.
    pub clear_image_placements: bool,
}

impl<'a> CellWrite<'a> {
    /// A narrow Output cell with no link and no images.
    pub fn new(glyph: Glyph<'a>, style: StyleSpec<'a>) -> Self {
        Self {
            glyph,
            wide: false,
            style,
            semantic: SemanticType::Output,
            wrapped: false,
            hyperlink: None,
            images: &[],
            clear_image_placements: false,
        }
    }

    fn summary_flags(&self, style: CellStyle, has_image: bool) -> u64 {
        let mut flags = 0;
        if let CellStyle::Rich(_) = style {
            flags |= RowHeader::STYLED;
        }
        if let Glyph::Cluster(_) = self.glyph {
            flags |= RowHeader::GRAPHEME;
        }
        if self.hyperlink.is_some() {
            flags |= RowHeader::HYPERLINK;
        }
        if has_image {
            flags |= RowHeader::IMAGE;
        }
        if !matches!(self.semantic, SemanticType::Output) {
            flags |= RowHeader::SEMANTIC;
        }
        flags
    }
}

pub struct Page {
    buf: Box<[u64]>,
    cols: u16,
    /// Row capacity.
    rows: u32,
    /// Rows handed out by `grow`.
    used: u32,
    /// Unique per terminal among live and pooled pages (D6).
    serial: u64,
    /// At least every row seqno; 0 when some row's seqno is 0, meaning
    /// "always changed".
    max_seqno: u64,
    styles: RichStyleTable,
    graphemes: GraphemeArena,
    links: LinkTable,
    images: ImageMap,
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("used", &self.used)
            .field("serial", &self.serial)
            .field("max_seqno", &self.max_seqno)
            .field("rich_styles", &self.styles.live())
            .field("graphemes", &self.graphemes.len())
            .field("links", &self.links.live())
            .field("image_cells", &self.images.cell_count())
            .finish()
    }
}

impl Page {
    /// A page of `rows` rows of `cols` columns. Standard pages use
    /// [`rows_per_page`]; the alternate screen is one page of exactly its
    /// rows.
    pub fn new(cols: u16, rows: u32, serial: u64) -> Self {
        assert!(rows > 0, "a page holds at least one row");
        let stride = usize::from(cols) + 1;
        let cells = (rows as usize)
            .checked_mul(stride)
            .filter(|&cells| cells <= u32::MAX as usize)
            .expect("page cell offsets fit in u32");
        let mut buf = vec![0_u64; 2 * rows as usize + cells].into_boxed_slice();
        for (slot, word) in buf[..rows as usize].iter_mut().enumerate() {
            *word = RowHeader::with_slot(slot as u32).bits();
        }
        Self {
            buf,
            cols,
            rows,
            used: 0,
            serial,
            max_seqno: 0,
            styles: RichStyleTable::new(),
            graphemes: GraphemeArena::new(),
            links: LinkTable::new(),
            images: ImageMap::new(),
        }
    }

    /// A standard page for `cols` columns.
    pub fn standard(cols: u16, serial: u64) -> Self {
        Self::new(cols, rows_per_page(cols), serial)
    }

    /// A copy under a new serial: two pages never share one (D6, I12).
    pub fn fork(&self, serial: u64) -> Self {
        Self {
            buf: self.buf.clone(),
            cols: self.cols,
            rows: self.rows,
            used: self.used,
            serial,
            max_seqno: self.max_seqno,
            styles: self.styles.clone(),
            graphemes: self.graphemes.clone(),
            links: self.links.clone(),
            images: self.images.clone(),
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Row capacity.
    pub fn capacity(&self) -> u32 {
        self.rows
    }

    /// Words in the page buffer: headers, seqnos and cells.
    pub fn buffer_words(&self) -> usize {
        self.buf.len()
    }

    pub fn used(&self) -> u32 {
        self.used
    }

    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// At least every row seqno; 0 when some row is "always changed".
    pub fn max_seqno(&self) -> SequenceNo {
        self.max_seqno as SequenceNo
    }

    pub fn styles(&self) -> &RichStyleTable {
        &self.styles
    }

    pub fn graphemes(&self) -> &GraphemeArena {
        &self.graphemes
    }

    pub fn links(&self) -> &LinkTable {
        &self.links
    }

    pub fn images(&self) -> &ImageMap {
        &self.images
    }

    fn stride(&self) -> usize {
        usize::from(self.cols) + 1
    }

    fn seqno_index(&self, slot: u32) -> usize {
        self.rows as usize + slot as usize
    }

    fn cell_index(&self, slot: u32, x: usize) -> usize {
        2 * self.rows as usize + slot as usize * self.stride() + x
    }

    fn offset(&self, slot: u32, x: usize) -> u32 {
        (slot as usize * self.stride() + x) as u32
    }

    fn cell_at(&self, slot: u32, x: usize) -> PackedCell {
        PackedCell::from_bits(self.buf[self.cell_index(slot, x)])
    }

    fn set_cell_at(&mut self, slot: u32, x: usize, cell: PackedCell) {
        let index = self.cell_index(slot, x);
        self.buf[index] = cell.bits();
    }

    pub fn header(&self, row: u32) -> RowHeader {
        debug_assert!(row < self.used, "row {} of {} used", row, self.used);
        RowHeader::from_bits(self.buf[row as usize])
    }

    fn set_header(&mut self, row: u32, header: RowHeader) {
        self.buf[row as usize] = header.bits();
    }

    /// Legacy `Line::len()`.
    pub fn row_len(&self, row: u32) -> usize {
        self.header(row).len()
    }

    pub fn row_seqno(&self, row: u32) -> SequenceNo {
        self.buf[self.seqno_index(self.header(row).slot())] as SequenceNo
    }

    /// The stored cell at column `x` (`x <= cols`); zero past the row's
    /// length.
    pub fn cell(&self, row: u32, x: usize) -> PackedCell {
        assert!(
            x < self.stride(),
            "column {} of a {}-column page",
            x,
            self.cols
        );
        self.cell_at(self.header(row).slot(), x)
    }

    /// The cell's text.
    pub fn glyph(&self, row: u32, x: usize) -> Glyph<'_> {
        let cell = self.cell(row, x);
        if cell.has_grapheme() {
            let off = self.offset(self.header(row).slot(), x);
            if let Some(text) = self.graphemes.get(off) {
                return Glyph::Cluster(text);
            }
        }
        match cell.first_char() {
            Some(ch) => Glyph::Char(ch),
            None => Glyph::Blank,
        }
    }

    pub fn hyperlink(&self, row: u32, x: usize) -> Option<&Arc<Hyperlink>> {
        if !self.cell(row, x).has_hyperlink() {
            return None;
        }
        self.links.get(self.offset(self.header(row).slot(), x))
    }

    pub fn cell_images(&self, row: u32, x: usize) -> Option<&[Box<ImageCell>]> {
        if !self.cell(row, x).has_image() {
            return None;
        }
        self.images.get(self.offset(self.header(row).slot(), x))
    }

    /// The cell's legacy attributes. Images are re-attached in stored
    /// order through `attach_image`, which can reorder images of equal
    /// z-index; [`Self::cell_images`] has the exact order.
    pub fn cell_attributes(&self, row: u32, x: usize) -> CellAttributes {
        let cell = self.cell(row, x);
        let mut attrs = style_attributes(cell.style(), &self.styles);
        attrs.set_semantic_type(cell.semantic());
        attrs.set_wrapped(cell.is_wrapped());
        if let Some(link) = self.hyperlink(row, x) {
            attrs.set_hyperlink(Some(Arc::clone(link)));
        }
        for image in self.cell_images(row, x).unwrap_or(&[]) {
            attrs.attach_image(image.clone());
        }
        attrs
    }

    /// The cell as a legacy `Cell`, hidden cells included.
    pub fn legacy_cell(&self, row: u32, x: usize) -> Cell {
        let width = if self.cell(row, x).is_wide() { 2 } else { 1 };
        let attrs = self.cell_attributes(row, x);
        let mut utf8 = [0_u8; 4];
        let text = match self.glyph(row, x) {
            Glyph::Blank if width == 1 => return Cell::blank_with_attrs(attrs),
            Glyph::Blank => " ",
            Glyph::Char(ch) => ch.encode_utf8(&mut utf8),
            Glyph::Cluster(text) => text,
        };
        Cell::new_grapheme_with_width(text, width, attrs)
    }

    /// Hands out the next row, empty, or `None` when the page is full.
    pub fn grow(&mut self, seqno: SequenceNo) -> Option<u32> {
        if self.used == self.rows {
            return None;
        }
        let row = self.used;
        self.used += 1;
        let header = self.header(row);
        debug_assert!(header.is_empty() && header.bits() >> 49 == 0);
        self.set_header(row, header.with_flags(RowHeader::DIRTY, true));
        let index = self.seqno_index(header.slot());
        let seqno = seqno as u64;
        self.buf[index] = seqno;
        if seqno == 0 {
            self.max_seqno = 0;
        } else if row == 0 {
            self.max_seqno = seqno;
        } else if self.max_seqno != 0 {
            self.max_seqno = self.max_seqno.max(seqno);
        }
        self.debug_check_row(row);
        Some(row)
    }

    /// Legacy `Line::update_last_change_seqno`, plus the page maximum.
    pub fn touch_row(&mut self, row: u32, seqno: SequenceNo) {
        let slot = self.header(row).slot();
        self.touch(slot, seqno);
    }

    fn touch(&mut self, slot: u32, seqno: SequenceNo) {
        let index = self.seqno_index(slot);
        let old = self.buf[index];
        let new = old.max(seqno as u64);
        self.buf[index] = new;
        if self.max_seqno != 0 {
            self.max_seqno = self.max_seqno.max(new);
        } else if old == 0 && new != 0 {
            // This row may have been the only one at 0.
            self.max_seqno = self.computed_max_seqno();
        }
    }

    fn computed_max_seqno(&self) -> u64 {
        let mut max = 0;
        for row in 0..self.used {
            let seqno = self.buf[self.seqno_index(self.header(row).slot())];
            if seqno == 0 {
                return 0;
            }
            max = max.max(seqno);
        }
        max
    }

    /// Sets the row's seqno to exactly `seqno`, lower or higher (B3.4: a
    /// row imported from a legacy `Line` keeps that line's seqno).
    /// `max_seqno` stays an upper bound, and 0 makes it 0.
    pub fn set_row_seqno(&mut self, row: u32, seqno: SequenceNo) {
        let slot = self.header(row).slot();
        let index = self.seqno_index(slot);
        let old = self.buf[index];
        let new = seqno as u64;
        self.buf[index] = new;
        if new == 0 {
            self.max_seqno = 0;
        } else if self.max_seqno != 0 {
            self.max_seqno = self.max_seqno.max(new);
        } else if old == 0 {
            // This row may have been the only one at 0.
            self.max_seqno = self.computed_max_seqno();
        }
    }

    /// Stores `cells` as row `row`, replacing what it held (B3.4's import of
    /// a legacy `Line`). Every cell is kept exactly as given, hidden cells
    /// included: legacy vector storage can give a hidden cell attributes of
    /// its own. The row's length becomes `cells.len()` and its seqno exactly
    /// `seqno`. Line flags are cleared for the caller to set.
    ///
    /// Returns false, leaving the row empty, when the cells do not fit: more
    /// than `cols + 1` of them, or a cell wider than two columns.
    pub fn store_legacy_row(&mut self, row: u32, cells: &[Cell], seqno: SequenceNo) -> bool {
        let header = self.header(row);
        let slot = header.slot();
        for x in 0..header.len() {
            self.release_at(slot, x);
        }
        self.set_header(
            row,
            RowHeader::with_slot(slot).with_flags(RowHeader::DIRTY, true),
        );
        if cells.len() > self.stride() || cells.iter().any(|cell| cell.width() > 2) {
            self.set_row_seqno(row, seqno);
            self.debug_check_row(row);
            return false;
        }

        let mut flags = RowHeader::DIRTY;
        let mut cache = None;
        for (x, cell) in cells.iter().enumerate() {
            let attrs = cell.attrs();
            let class = classify(attrs);
            let style = match &class {
                StyleClass::Inline(inline) => self.resolve_style(&mut StyleSpec::Inline(*inline)),
                StyleClass::Rich(rich) => {
                    flags |= RowHeader::STYLED;
                    self.resolve_style(&mut StyleSpec::Rich {
                        style: rich,
                        cache: &mut cache,
                    })
                }
            };
            let glyph = Glyph::from_text(cell.str());
            let mut bits = PackedCell::BLANK
                .with_style(style)
                .with_semantic(attrs.semantic_type())
                .with_wrapped(attrs.wrapped())
                .with_codepoint(glyph.codepoint())
                .with_wide(cell.width() >= 2);
            if !matches!(attrs.semantic_type(), SemanticType::Output) {
                flags |= RowHeader::SEMANTIC;
            }
            let off = self.offset(slot, x);
            if let Glyph::Cluster(text) = glyph {
                self.graphemes.insert(off, text);
                bits = bits.with_grapheme(true);
                flags |= RowHeader::GRAPHEME;
            }
            if let Some(link) = attrs.hyperlink() {
                self.links.attach(off, link);
                bits = bits.with_hyperlink(true);
                flags |= RowHeader::HYPERLINK;
            }
            // Legacy's stored order, as `write_legacy` keeps it.
            let images: Vec<Box<ImageCell>> = attrs
                .images()
                .map(|images| images.into_iter().map(Box::new).collect())
                .unwrap_or_default();
            if !images.is_empty() {
                self.images.insert(off, images);
                bits = bits.with_image(true);
                flags |= RowHeader::IMAGE;
            }
            self.set_cell_at(slot, x, bits);
        }
        let len = cells.len();
        self.set_header(
            row,
            RowHeader::with_slot(slot)
                .with_len(len)
                .with_flags(flags, true),
        );
        // Every hidden bit from the row's first cell (I1).
        self.resync_hidden(slot, 0, len, len);
        self.set_row_seqno(row, seqno);
        self.debug_check_row(row);
        true
    }

    /// Sets or clears legacy line flags ([`RowHeader::LINE_FLAGS`]).
    pub fn set_row_flags(&mut self, row: u32, flags: u64, on: bool) {
        debug_assert_eq!(flags & !RowHeader::LINE_FLAGS, 0);
        let header = self.header(row);
        self.set_header(row, header.with_flags(flags & RowHeader::LINE_FLAGS, on));
    }

    /// Sets or clears the wrapped bit of the cells `from..len`: legacy's
    /// `set_last_cell_was_wrapped`, given the last visible cell (which owns
    /// any hidden cells after it). Bumps the seqno.
    pub fn set_wrapped_from(&mut self, row: u32, from: usize, wrapped: bool, seqno: SequenceNo) {
        let header = self.header(row);
        let slot = header.slot();
        for x in from..header.len() {
            let cell = self.cell_at(slot, x);
            self.set_cell_at(slot, x, cell.with_wrapped(wrapped));
        }
        self.set_header(row, header.with_flags(RowHeader::DIRTY, true));
        self.touch(slot, seqno);
        self.debug_check_row(row);
    }

    /// Legacy vector storage's `prune_trailing_blanks`: trailing default
    /// blank cells are dropped, but a wide cell keeps its placeholder.
    /// Bumps the seqno only when the row shrinks.
    pub fn prune_trailing_blanks(&mut self, row: u32, seqno: SequenceNo) {
        let header = self.header(row);
        let slot = header.slot();
        let len = header.len();
        let new_len = (0..len)
            .rev()
            .find(|&x| self.cell_at(slot, x).with_hidden(false) != PackedCell::BLANK)
            .map_or(0, |x| {
                let width = if self.cell_at(slot, x).is_wide() {
                    2
                } else {
                    1
                };
                (x + width).min(len)
            });
        if new_len < len {
            self.resize_row(row, new_len, seqno);
        }
    }

    /// Renderer consume-and-clear of the row's dirty flag.
    pub fn take_dirty(&mut self, row: u32) -> bool {
        let header = self.header(row);
        self.set_header(row, header.with_flags(RowHeader::DIRTY, false));
        header.has(RowHeader::DIRTY)
    }

    /// Exchanges rows `a` and `b` by swapping their headers (D5). Cells,
    /// seqnos and side entries belong to a slot, so they move with it; both
    /// rows are marked dirty.
    pub fn swap_rows(&mut self, a: u32, b: u32) {
        let first = self.header(a).with_flags(RowHeader::DIRTY, true);
        let second = self.header(b).with_flags(RowHeader::DIRTY, true);
        self.set_header(a, second);
        self.set_header(b, first);
    }

    /// Copies `src_row` of `src`, a page of the same width, into the empty
    /// row `dst_row`: cells, length, row flags and the exact seqno. Side
    /// entries are re-interned here, rich ids included (D5: rows that rotate
    /// across a page boundary are copied).
    pub fn copy_row_from(&mut self, dst_row: u32, src: &Page, src_row: u32) {
        assert_eq!(self.cols, src.cols, "rows copy between pages of one width");
        let dst = self.header(dst_row);
        assert!(
            dst.is_empty(),
            "row {} must be empty to take a copy",
            dst_row
        );
        let source = src.header(src_row);
        let (from_slot, to_slot) = (source.slot(), dst.slot());
        let mut hint = None;
        let mut has_rich = false;
        for x in 0..source.len() {
            let cell = src.cell_at(from_slot, x);
            let mut copy = cell;
            if let CellStyle::Rich(id) = cell.style() {
                let style = src.styles.get(id).expect("I5: a stored rich id is live");
                let (id, cached) = self.styles.acquire(style, self.serial, hint);
                hint = Some(cached);
                has_rich = true;
                copy = copy.with_style(CellStyle::Rich(id));
            }
            let (from, to) = (src.offset(from_slot, x), self.offset(to_slot, x));
            if cell.has_grapheme() {
                let text = src.graphemes.get(from).expect("I6: grapheme bit");
                self.graphemes.insert(to, text);
            }
            if cell.has_hyperlink() {
                let link = src.links.get(from).expect("I7: hyperlink bit");
                self.links.attach(to, link);
            }
            if cell.has_image() {
                let images = src.images.get(from).expect("I7: image bit");
                self.images.insert(to, images.to_vec());
            }
            self.set_cell_at(to_slot, x, copy);
        }
        let flags = source.bits() & (RowHeader::SUMMARY | RowHeader::LINE_FLAGS);
        let header = RowHeader::with_slot(to_slot)
            .with_len(source.len())
            .with_flags(flags | RowHeader::DIRTY, true);
        debug_assert!(!has_rich || header.has(RowHeader::STYLED), "I9");
        self.set_header(dst_row, header);
        let seqno = src.buf[src.seqno_index(from_slot)];
        let index = self.seqno_index(to_slot);
        self.buf[index] = seqno;
        // A lower seqno than the empty row had keeps `max_seqno` an upper
        // bound; a 0 makes the whole page "always changed".
        if seqno == 0 {
            self.max_seqno = 0;
        } else if self.max_seqno != 0 {
            self.max_seqno = self.max_seqno.max(seqno);
        }
        self.debug_check_row(dst_row);
    }

    fn resolve_style(&mut self, spec: &mut StyleSpec<'_>) -> CellStyle {
        match spec {
            StyleSpec::Inline(inline) => {
                debug_assert!(inline.fg() <= 256 && inline.bg() <= 256, "I4");
                CellStyle::Inline(*inline)
            }
            StyleSpec::Rich { style, cache } => {
                let (id, cached) = self.styles.acquire(style, self.serial, **cache);
                **cache = Some(cached);
                CellStyle::Rich(id)
            }
        }
    }

    /// Drops a cell's side entries. With `carry_placements`, returns its
    /// placement images for legacy's overwrite carry-over
    /// (`vecstorage.rs` `set_cell`).
    fn release(
        &mut self,
        off: u32,
        old: PackedCell,
        carry_placements: bool,
    ) -> Vec<Box<ImageCell>> {
        if let CellStyle::Rich(id) = old.style() {
            self.styles.release(id);
        }
        if old.has_grapheme() {
            self.graphemes.remove(off);
        }
        if old.has_hyperlink() {
            self.links.detach(off);
        }
        if !old.has_image() {
            return Vec::new();
        }
        let images = self.images.remove(off).unwrap_or_default();
        if carry_placements {
            images
                .into_iter()
                .filter(|image| image.has_placement_id())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Releases and zeroes one cell.
    fn release_at(&mut self, slot: u32, x: usize) {
        let off = self.offset(slot, x);
        let old = self.cell_at(slot, x);
        self.release(off, old, false);
        self.set_cell_at(slot, x, PackedCell::BLANK);
    }

    /// Takes the side entries of `src` (stored at `from`) again for a copy
    /// at `to`, whose own entries are already released.
    fn duplicate_side_entries(&mut self, from: u32, to: u32, src: PackedCell) {
        if let CellStyle::Rich(id) = src.style() {
            self.styles.add_ref(id);
        }
        if src.has_grapheme() {
            self.graphemes.duplicate(from, to);
        }
        if src.has_hyperlink() {
            self.links.duplicate(from, to);
        }
        if src.has_image() {
            if let Some(images) = self.images.get(from).map(<[_]>::to_vec) {
                self.images.insert(to, images);
            }
        }
    }

    /// Legacy `Cell::blank_with_attrs(attrs of from_x)` stored at `to_x` of
    /// the same row: `from_x`'s attributes, hyperlink and images, but no
    /// text and width 1. The hidden bit is left for the caller.
    fn store_blank_copy(&mut self, slot: u32, from_x: usize, to_x: usize) {
        let to = self.offset(slot, to_x);
        let old = self.cell_at(slot, to_x);
        self.release(to, old, false);
        let src = self.cell_at(slot, from_x).with_grapheme(false);
        let from = self.offset(slot, from_x);
        self.duplicate_side_entries(from, to, src);
        let blank = src
            .with_codepoint(0)
            .with_wide(false)
            .with_hidden(old.is_hidden());
        self.set_cell_at(slot, to_x, blank);
    }

    /// Turns a wide head into a blank that keeps its attributes.
    fn blank_wide_head(&mut self, slot: u32, x: usize) {
        let off = self.offset(slot, x);
        let head = self.cell_at(slot, x);
        if head.has_grapheme() {
            self.graphemes.remove(off);
        }
        let blank = head.with_codepoint(0).with_grapheme(false).with_wide(false);
        self.set_cell_at(slot, x, blank);
    }

    /// Stores a written cell (`head`) or its spacer over the cell at `x`,
    /// whose entries are released first. Returns whether it holds images.
    fn store_written(
        &mut self,
        slot: u32,
        x: usize,
        mut bits: PackedCell,
        cell: &CellWrite<'_>,
        head: bool,
    ) -> bool {
        let off = self.offset(slot, x);
        let old = self.cell_at(slot, x);
        let carried = self.release(off, old, !cell.clear_image_placements);
        if head {
            if let Glyph::Cluster(text) = cell.glyph {
                self.graphemes.insert(off, text);
                bits = bits.with_grapheme(true);
            }
        }
        if let Some(link) = cell.hyperlink {
            self.links.attach(off, link);
            bits = bits.with_hyperlink(true);
        }
        let mut images = cell.images.to_vec();
        for image in carried {
            attach_image(&mut images, image);
        }
        let has_image = !images.is_empty();
        if has_image {
            self.images.insert(off, images);
            bits = bits.with_image(true);
        }
        self.set_cell_at(slot, x, bits);
        has_image
    }

    /// Recomputes `hidden` bits (I1) from `start` up to `len`. Cells in
    /// `start..=settle` changed or have a new left neighbour; past `settle`
    /// each cell moved together with the one before it, so the first stored
    /// bit that is already right proves the rest of the row right (ADR Q1).
    fn resync_hidden(&mut self, slot: u32, start: usize, settle: usize, len: usize) {
        let base = self.cell_index(slot, 0);
        let mut after_visible_wide = start > 0 && {
            let prev = PackedCell::from_bits(self.buf[base + start - 1]);
            !prev.is_hidden() && prev.is_wide()
        };
        for y in start..len {
            let cell = PackedCell::from_bits(self.buf[base + y]);
            let hidden = after_visible_wide;
            if cell.is_hidden() == hidden {
                if y > settle {
                    break;
                }
            } else {
                self.buf[base + y] = cell.with_hidden(hidden).bits();
            }
            after_visible_wide = !hidden && cell.is_wide();
        }
    }

    /// Moves the cells in `range` one column right, or one column left,
    /// re-keying their side entries. The destination column outside `range`
    /// must hold no side entries; the column vacated is left zero.
    fn shift_cells(&mut self, row: u32, range: Range<usize>, right: bool) {
        if range.is_empty() {
            return;
        }
        debug_assert!(right || range.start > 0);
        let header = self.header(row);
        let slot = header.slot();
        let base = self.cell_index(slot, 0);
        let mut moved = Vec::new();
        if header.has(RowHeader::GRAPHEME | RowHeader::HYPERLINK | RowHeader::IMAGE) {
            for x in range.clone() {
                let cell = PackedCell::from_bits(self.buf[base + x]);
                if !(cell.has_grapheme() || cell.has_hyperlink() || cell.has_image()) {
                    continue;
                }
                let off = self.offset(slot, x);
                let to = if right { off + 1 } else { off - 1 };
                let grapheme = if cell.has_grapheme() {
                    self.graphemes.take(off)
                } else {
                    None
                };
                let link = if cell.has_hyperlink() {
                    self.links.take(off)
                } else {
                    None
                };
                let images = if cell.has_image() {
                    self.images.remove(off)
                } else {
                    None
                };
                moved.push((to, grapheme, link, images));
            }
        }
        if right {
            self.buf
                .copy_within(base + range.start..base + range.end, base + range.start + 1);
            self.buf[base + range.start] = 0;
        } else {
            self.buf
                .copy_within(base + range.start..base + range.end, base + range.start - 1);
            self.buf[base + range.end - 1] = 0;
        }
        for (to, grapheme, link, images) in moved {
            if let Some(entry) = grapheme {
                self.graphemes.put(to, entry);
            }
            if let Some(id) = link {
                self.links.put(to, id);
            }
            if let Some(images) = images {
                self.images.insert(to, images);
            }
        }
    }

    /// Legacy `Line::set_cell` (or `set_cell_clearing_image_placements`)
    /// at column `x`. Returns false, writing nothing, when the cell would
    /// run past column `cols` (legacy would grow the row past its stride).
    pub fn write(
        &mut self,
        row: u32,
        x: usize,
        mut cell: CellWrite<'_>,
        seqno: SequenceNo,
    ) -> bool {
        let width = if cell.wide { 2 } else { 1 };
        if x + width > self.stride() {
            debug_assert!(
                false,
                "width-{} write at column {} of a {}-column page",
                width, x, self.cols
            );
            return false;
        }
        if let Glyph::Cluster(text) = cell.glyph {
            cell.glyph = Glyph::from_text(text);
        }
        let header = self.header(row);
        let slot = header.slot();
        let old_len = header.len();

        // Acquire before release (D3): overwriting a cell with its own
        // style must not free the entry in between.
        let style = self.resolve_style(&mut cell.style);
        if cell.wide {
            if let CellStyle::Rich(id) = style {
                self.styles.add_ref(id);
            }
        }

        // Legacy `invalidate_grapheme_at_or_before`: overwriting the cell
        // after a wide head turns both into blanks with the head's
        // attributes.
        let mut first = x;
        if x > 0 && self.cell_at(slot, x - 1).is_wide() {
            self.blank_wide_head(slot, x - 1);
            self.store_blank_copy(slot, x - 1, x);
            first = x - 1;
        }

        let template = PackedCell::BLANK
            .with_style(style)
            .with_semantic(cell.semantic)
            .with_wrapped(cell.wrapped);
        let mut has_image = false;
        if cell.wide {
            has_image |= self.store_written(slot, x + 1, template, &cell, false);
        }
        let head = template
            .with_codepoint(cell.glyph.codepoint())
            .with_wide(cell.wide);
        has_image |= self.store_written(slot, x, head, &cell, true);

        let len = old_len.max(x + width);
        if old_len < first {
            self.resync_hidden(slot, old_len, old_len, len);
        }
        self.resync_hidden(slot, first, x + width - 1, len);
        let flags = RowHeader::DIRTY | cell.summary_flags(style, has_image);
        self.set_header(row, header.with_len(len).with_flags(flags, true));
        self.touch(slot, seqno);
        self.debug_check_cells(
            row,
            first.saturating_sub(1)..(x + width + 1).min(self.stride()),
        );
        true
    }

    /// [`Self::write`] from a legacy cell: the cold-path entry for callers
    /// that hold a `Cell`, such as ingest from the wire or cold storage.
    pub fn write_legacy(
        &mut self,
        row: u32,
        x: usize,
        cell: &Cell,
        clear_image_placements: bool,
        seqno: SequenceNo,
    ) -> bool {
        let attrs = cell.attrs();
        let images: Vec<Box<ImageCell>> = attrs
            .images()
            .map(|images| images.into_iter().map(Box::new).collect())
            .unwrap_or_default();
        let class = classify(attrs);
        let mut cache = None;
        let style = match &class {
            StyleClass::Inline(inline) => StyleSpec::Inline(*inline),
            StyleClass::Rich(rich) => StyleSpec::Rich {
                style: rich,
                cache: &mut cache,
            },
        };
        let write = CellWrite {
            glyph: Glyph::from_text(cell.str()),
            wide: cell.width() >= 2,
            style,
            semantic: attrs.semantic_type(),
            wrapped: attrs.wrapped(),
            hyperlink: attrs.hyperlink(),
            images: &images,
            clear_image_placements,
        };
        self.write(row, x, write, seqno)
    }

    /// Legacy `Screen::insert_cell` (ICH and insert mode): bump the seqno,
    /// `Line::insert_cell(x, Cell::default(), right_margin)`, then truncate
    /// to `limit` (the screen width, at most `cols`) if the row grew past it.
    pub fn insert_blank(
        &mut self,
        row: u32,
        x: usize,
        right_margin: usize,
        limit: usize,
        seqno: SequenceNo,
    ) {
        let header = self.header(row);
        let slot = header.slot();
        self.touch(slot, seqno);
        if right_margin == 0 || x >= self.stride() {
            debug_assert!(right_margin == 0, "insert at column {} past the page", x);
            return;
        }
        let limit = limit.min(usize::from(self.cols));
        let mut len = header.len();
        let mut start = x;
        let mut settle = x + 1;
        if right_margin <= len {
            self.release_at(slot, right_margin - 1);
            self.shift_cells(row, right_margin..len, false);
            len -= 1;
            start = start.min(right_margin - 1);
            settle = settle.max(right_margin);
        }
        if x >= len {
            start = start.min(len);
            len = x;
        }
        if len == self.stride() {
            // The insert pushes the last cell past the stride; legacy then
            // truncates it with the rest of the row past `limit`.
            self.release_at(slot, len - 1);
            len -= 1;
        }
        self.shift_cells(row, x..len, true);
        len += 1;
        if len > limit {
            for col in limit..len {
                self.release_at(slot, col);
            }
            len = limit;
        }
        let header = self.header(row);
        self.set_header(row, header.with_len(len).with_flags(RowHeader::DIRTY, true));
        if start < len {
            self.resync_hidden(slot, start, settle, len);
        }
        self.debug_check_row(row);
    }

    /// Legacy `Line::erase_cell_with_margin` (DCH): delete the cell at `x`,
    /// shifting the rest of the row left, and insert `blank`'s attributes
    /// as a blank at `right_margin - 1`. `blank`'s glyph and width are
    /// ignored.
    pub fn erase_cell_with_margin(
        &mut self,
        row: u32,
        x: usize,
        right_margin: usize,
        mut blank: CellWrite<'_>,
        seqno: SequenceNo,
    ) {
        if right_margin == 0 {
            return;
        }
        let header = self.header(row);
        let slot = header.slot();
        let mut len = header.len();
        let mut start = x;
        let mut settle = x;
        let mut flags = RowHeader::DIRTY;
        if x < len {
            if x > 0 && self.cell_at(slot, x - 1).is_wide() {
                self.blank_wide_head(slot, x - 1);
                start = x - 1;
            }
            self.release_at(slot, x);
            self.shift_cells(row, x + 1..len, false);
            len -= 1;
        }
        let insert_at = right_margin - 1;
        if insert_at <= len && insert_at < self.stride() {
            if len == self.stride() {
                self.release_at(slot, len - 1);
                len -= 1;
            }
            self.shift_cells(row, insert_at..len, true);
            blank.glyph = Glyph::Blank;
            blank.wide = false;
            let style = self.resolve_style(&mut blank.style);
            let bits = PackedCell::BLANK
                .with_style(style)
                .with_semantic(blank.semantic)
                .with_wrapped(blank.wrapped);
            let has_image = self.store_written(slot, insert_at, bits, &blank, false);
            flags |= blank.summary_flags(style, has_image);
            len += 1;
            start = start.min(insert_at);
            settle = settle.max(insert_at + 1);
        }
        let header = self.header(row);
        self.set_header(row, header.with_len(len).with_flags(flags, true));
        self.touch(slot, seqno);
        if start < len {
            self.resync_hidden(slot, start, settle, len);
        }
        self.debug_check_row(row);
    }

    /// Legacy `Line::resize`: truncate to `width` cells, or pad to it with
    /// default blanks.
    pub fn resize_row(&mut self, row: u32, width: usize, seqno: SequenceNo) {
        debug_assert!(width <= self.stride());
        let width = width.min(self.stride());
        let header = self.header(row);
        let slot = header.slot();
        let len = header.len();
        for col in width..len {
            self.release_at(slot, col);
        }
        self.set_header(
            row,
            header.with_len(width).with_flags(RowHeader::DIRTY, true),
        );
        if width > len {
            self.resync_hidden(slot, len, len, width);
        }
        self.touch(slot, seqno);
        self.debug_check_row(row);
    }

    /// The cell copy of legacy's scroll within left and right margins
    /// (`Screen` band scroll): the destination is padded to `x + count`,
    /// then its cells `x..x + count` become clones of the source's, where
    /// `count` is `n` cut to the cells the source row stores. Clones are
    /// raw: no wide-head invalidation and no image carry-over. Returns
    /// `count`.
    pub fn copy_cells(
        &mut self,
        src_row: u32,
        dst_row: u32,
        x: usize,
        n: usize,
        seqno: SequenceNo,
    ) -> usize {
        let src = self.header(src_row);
        let dst = self.header(dst_row);
        let count = n.min(src.len().saturating_sub(x));
        self.touch(dst.slot(), seqno);
        if src_row == dst_row {
            return count;
        }
        let end = x + count;
        let old_len = dst.len();
        let len = old_len.max(end);
        for col in x..end {
            let to = self.offset(dst.slot(), col);
            let old = self.cell_at(dst.slot(), col);
            self.release(to, old, false);
            let from = self.offset(src.slot(), col);
            let cell = self.cell_at(src.slot(), col);
            self.duplicate_side_entries(from, to, cell);
            self.set_cell_at(dst.slot(), col, cell);
        }
        let flags = RowHeader::DIRTY | (src.bits() & RowHeader::SUMMARY);
        self.set_header(dst_row, dst.with_len(len).with_flags(flags, true));
        let start = old_len.min(x);
        if start < len {
            self.resync_hidden(dst.slot(), start, end, len);
        }
        self.debug_check_row(dst_row);
        count
    }

    /// ADR Q3: rewrites each hidden cell the way legacy rebuilds it when it
    /// re-materializes clustered storage, as a blank with its head's
    /// attributes, dropping the hidden cell's own. Like legacy's
    /// `compress_for_scrollback`, this does not bump the seqno. Returns the
    /// number of cells rewritten.
    pub fn rewrite_hidden_cells(&mut self, row: u32) -> usize {
        let header = self.header(row);
        let slot = header.slot();
        let mut rewritten = 0;
        for x in 1..header.len() {
            if self.cell_at(slot, x).is_hidden() {
                self.store_blank_copy(slot, x - 1, x);
                rewritten += 1;
            }
        }
        self.debug_check_row(row);
        rewritten
    }

    /// Empties the row: every cell released and zeroed, `len` 0, row flags
    /// cleared.
    pub fn clear_row(&mut self, row: u32, seqno: SequenceNo) {
        let header = self.header(row);
        let slot = header.slot();
        for x in 0..header.len() {
            self.release_at(slot, x);
        }
        self.set_header(
            row,
            RowHeader::with_slot(slot).with_flags(RowHeader::DIRTY, true),
        );
        self.touch(slot, seqno);
        self.debug_check_row(row);
    }

    /// Recycles the page (I14): zero cells and seqnos, no rows, empty side
    /// tables at standard capacity, and a fresh serial.
    pub fn reset(&mut self, serial: u64) {
        self.buf.fill(0);
        for slot in 0..self.rows {
            self.buf[slot as usize] = RowHeader::with_slot(slot).bits();
        }
        self.used = 0;
        self.serial = serial;
        self.max_seqno = 0;
        self.styles.reset();
        self.graphemes.reset();
        self.links.reset();
        self.images.reset();
        // I14. Not `check_invariants`, which allocates: a recycled page must
        // keep the scroll path allocation-free even in debug builds (B3.3).
        debug_assert!(self.is_clean(), "pagegrid reset left the page unclean");
    }

    /// Whether the page is as [`Self::reset`] leaves it (I14), serial aside.
    pub fn is_clean(&self) -> bool {
        let rows = self.rows as usize;
        self.used == 0
            && self.max_seqno == 0
            && self.buf[..rows]
                .iter()
                .enumerate()
                .all(|(slot, &word)| word == slot as u64)
            && self.buf[rows..].iter().all(|&word| word == 0)
            && self.styles.is_clean()
            && self.graphemes.is_clean()
            && self.links.is_clean()
            && self.images.is_clean()
    }

    fn debug_check_row(&self, row: u32) {
        if cfg!(debug_assertions) {
            if let Err(err) = self.check_cells(row, 0..self.stride()) {
                panic!("pagegrid invariant broken in row {}: {}", row, err);
            }
        }
    }

    fn debug_check_cells(&self, row: u32, cols: Range<usize>) {
        if cfg!(debug_assertions) {
            if let Err(err) = self.check_cells(row, cols) {
                panic!("pagegrid invariant broken in row {}: {}", row, err);
            }
        }
    }

    /// The per-cell invariants for columns `cols` of a row: I1-I4, the
    /// bit-to-map half of I6 and I7, and I9.
    fn check_cells(&self, row: u32, cols: Range<usize>) -> Result<(), String> {
        let header = self.header(row);
        if header.bits() & RowHeader::RESERVED != 0 {
            return Err("reserved header bits set".to_string());
        }
        let len = header.len();
        if len > self.stride() {
            return Err(format!("len {} past the stride", len)); // I2
        }
        let slot = header.slot();
        for x in cols {
            let cell = self.cell_at(slot, x);
            let at = || format!("cell {} of row {}", x, row);
            if x >= len {
                if cell != PackedCell::BLANK {
                    return Err(format!("{} lies past len {} but is not zero", at(), len));
                    // I2
                }
                continue;
            }
            let hidden = x > 0 && {
                let prev = self.cell_at(slot, x - 1);
                !prev.is_hidden() && prev.is_wide()
            };
            if cell.is_hidden() != hidden {
                return Err(format!(
                    "{} has hidden {} but the walk says {}",
                    at(),
                    cell.is_hidden(),
                    hidden
                )); // I1
            }
            if cell.is_blank() && cell.has_grapheme() {
                return Err(format!("{} is blank with a grapheme", at())); // I3
            }
            if cell.bits() & PackedCell::RESERVED != 0 {
                return Err(format!("{} has reserved bits set", at())); // I4
            }
            if let CellStyle::Inline(inline) = cell.style() {
                if inline.fg() > 256 || inline.bg() > 256 {
                    return Err(format!("{} has an invalid inline colour", at()));
                    // I4
                }
            }
            let off = self.offset(slot, x);
            if cell.has_grapheme() != self.graphemes.contains(off) {
                return Err(format!("{} grapheme bit disagrees with the arena", at()));
                // I6
            }
            if cell.has_grapheme() {
                let first = self
                    .graphemes
                    .get(off)
                    .and_then(|text| text.chars().next())
                    .map(u32::from);
                if first != Some(cell.codepoint()) {
                    return Err(format!("{} first scalar disagrees with its grapheme", at()));
                    // I6
                }
            }
            if cell.has_hyperlink() != self.links.contains(off) {
                return Err(format!(
                    "{} hyperlink bit disagrees with the link map",
                    at()
                )); // I7
            }
            if cell.has_image() != self.images.contains(off) {
                return Err(format!("{} image bit disagrees with the image map", at()));
                // I7
            }
            let summary = [
                (cell.is_rich(), RowHeader::STYLED, "styled"),
                (cell.has_grapheme(), RowHeader::GRAPHEME, "grapheme"),
                (cell.has_hyperlink(), RowHeader::HYPERLINK, "hyperlink"),
                (cell.has_image(), RowHeader::IMAGE, "image"),
                (
                    !matches!(cell.semantic(), SemanticType::Output),
                    RowHeader::SEMANTIC,
                    "semantic",
                ),
            ];
            for &(present, flag, name) in &summary {
                if present && !header.has(flag) {
                    return Err(format!("{} needs the row's {} flag", at(), name));
                    // I9
                }
            }
        }
        Ok(())
    }

    /// Checks every page-level invariant of ADR section 6 (I1-I11, I14 for
    /// unused rows): the row checks, refcounts against cell counts, side
    /// maps against cell bits in both directions, distinct slots and the
    /// page seqno maximum. O(page); for tests and debug sweeps.
    pub fn check_invariants(&self) -> Result<(), String> {
        let rows = self.rows as usize;
        if self.used > self.rows {
            return Err(format!("used {} past capacity {}", self.used, self.rows));
        }
        let mut row_of_slot = vec![None; rows];
        for row in 0..self.rows {
            let header = RowHeader::from_bits(self.buf[row as usize]);
            let slot = header.slot() as usize;
            if slot >= rows {
                return Err(format!("row {} names slot {} of {}", row, slot, rows));
            }
            if row_of_slot[slot].replace(row).is_some() {
                return Err(format!("slot {} belongs to two rows", slot)); // I11
            }
        }
        let mut cell_refs = vec![0_u32; self.styles.id_bound()];
        for row in 0..self.rows {
            let header = RowHeader::from_bits(self.buf[row as usize]);
            let slot = header.slot();
            if row >= self.used {
                let cells = self.cell_index(slot, 0)..self.cell_index(slot, self.stride());
                if header.bits() != u64::from(slot)
                    || self.buf[self.seqno_index(slot)] != 0
                    || self.buf[cells].iter().any(|&word| word != 0)
                {
                    return Err(format!("unused row {} is not clean", row));
                }
                continue;
            }
            self.check_cells(row, 0..self.stride())?;
            for x in 0..header.len() {
                if let CellStyle::Rich(id) = self.cell_at(slot, x).style() {
                    match cell_refs.get_mut(id as usize) {
                        Some(count) if id != 0 => *count += 1,
                        _ => {
                            return Err(format!(
                                "cell {} of row {} holds unknown rich id {}",
                                x, row, id
                            ))
                        }
                    }
                }
            }
        }
        self.styles.check(&cell_refs)?; // I5
        self.graphemes.check()?; // I6
        self.links.check()?; // I7

        // I6-I8, map-to-bit half: every key names a used cell with the bit.
        let stride = self.stride();
        let key_cell = |key: u32| -> Option<PackedCell> {
            let slot = key as usize / stride;
            let x = key as usize % stride;
            let row = (*row_of_slot.get(slot)?)?;
            if row >= self.used || x >= self.header(row).len() {
                return None;
            }
            Some(self.cell_at(slot as u32, x))
        };
        for key in self.graphemes.keys() {
            if !key_cell(key).is_some_and(PackedCell::has_grapheme) {
                return Err(format!("grapheme key {} names no grapheme cell", key));
            }
        }
        for key in self.links.keys() {
            if !key_cell(key).is_some_and(PackedCell::has_hyperlink) {
                return Err(format!("link key {} names no hyperlink cell", key));
            }
        }
        for key in self.images.keys() {
            if !key_cell(key).is_some_and(PackedCell::has_image) {
                return Err(format!("image key {} names no image cell", key));
            }
        }

        // I10, page half.
        if self.max_seqno != 0 {
            for row in 0..self.used {
                let seqno = self.buf[self.seqno_index(self.header(row).slot())];
                if seqno == 0 || seqno > self.max_seqno {
                    return Err(format!(
                        "row {} seqno {} not covered by page max {}",
                        row, seqno, self.max_seqno
                    ));
                }
            }
        }
        Ok(())
    }
}
