//! Legacy `Line` edits applied natively to a page row (B3.4).
//!
//! The page-engine `Screen` writes its hot paths here instead of through a
//! `Line` view. Each function reproduces one legacy `Line` method on the row
//! a [`store_line`](super::view::store_line) of that line would give,
//! including legacy's storage-form rules (ADR section 9), which the row
//! carries as [`RowHeader::LEGACY_FORM_C`]:
//! - a clustered row only appends: a cell at or past the end whose first
//!   character cannot cluster with the row's last one;
//! - any other write turns it into a vector row;
//! - past its end, a clustered row leaves a default blank implicit.
//!
//! A function that returns `false` has written nothing. The caller then
//! applies the legacy method to the row's `Line` view instead.

use super::cell::{classify, Glyph, PackedCell, StyleClass};
use super::page::{CellWrite, Page, StyleSpec};
use super::row::RowHeader;
use crate::config::BidiMode;
use frankenterm_bidi::ParagraphDirectionHint;
use frankenterm_cell::image::ImageCell;
use frankenterm_cell::{Cell, CellAttributes, SemanticType};
use frankenterm_surface::line::clustered_append_breaks;
use finl_unicode::grapheme_clusters::Graphemes;
use frankenterm_surface::{Line, SequenceNo};
use std::ops::Range;

/// The row flags `BidiMode::apply_to_line` gives a line.
pub fn bidi_flags(mode: &BidiMode) -> u64 {
    let mut flags = if mode.enabled {
        RowHeader::BIDI_ENABLED
    } else {
        0
    };
    flags |= match mode.hint {
        ParagraphDirectionHint::AutoRightToLeft => {
            RowHeader::AUTO_DETECT_DIRECTION | RowHeader::RTL
        }
        ParagraphDirectionHint::AutoLeftToRight => RowHeader::AUTO_DETECT_DIRECTION,
        ParagraphDirectionHint::RightToLeft => RowHeader::RTL,
        ParagraphDirectionHint::LeftToRight => 0,
    };
    flags
}

const BIDI_FLAGS: u64 = RowHeader::BIDI_ENABLED | RowHeader::AUTO_DETECT_DIRECTION | RowHeader::RTL;

/// `BidiMode::apply_to_line`: the row's bidi flags become the mode's, and
/// the seqno moves.
pub fn apply_bidi(page: &mut Page, row: u32, mode: &BidiMode, seqno: SequenceNo) {
    page.set_row_flags(row, BIDI_FLAGS, false);
    let flags = bidi_flags(mode);
    if flags != 0 {
        page.set_row_flags(row, flags, true);
    }
    page.touch_row(row, seqno);
}

fn is_clustered(page: &Page, row: u32) -> bool {
    page.header(row).has(RowHeader::LEGACY_FORM_C)
}

fn set_vector(page: &mut Page, row: u32) {
    page.set_row_flags(row, RowHeader::LEGACY_FORM_C, false);
}

/// The storage effect of legacy `Line::cells_mut`: the row becomes a vector
/// row, and nothing else changes, its seqno included.
pub fn make_vector(page: &mut Page, row: u32) {
    set_vector(page, row);
}

/// The last character of the row's text as clustered storage holds it: the
/// last visible cell's, `' '` for a blank. `None` for an empty row.
fn last_char(page: &Page, row: u32) -> Option<char> {
    let mut x = page.row_len(row).checked_sub(1)?;
    while x > 0 && page.cell(row, x).is_hidden() {
        x -= 1;
    }
    Some(match page.glyph(row, x) {
        Glyph::Blank => ' ',
        Glyph::Char(ch) => ch,
        Glyph::Cluster(text) => text.chars().next_back().unwrap_or(' '),
    })
}

/// `ClusteredLine::can_append_cell_at` for a cell whose text starts with
/// `first`.
fn clustered_can_append(page: &Page, row: u32, x: usize, first: char) -> bool {
    let len = page.row_len(row);
    if x < len {
        return false;
    }
    let mut prev = last_char(page, row);
    if x > len {
        if !clustered_append_breaks(prev, ' ') {
            return false;
        }
        prev = Some(' ');
    }
    clustered_append_breaks(prev, first)
}

/// Whether a `width`-wide cell at `x` fits the page's stride.
fn fits(page: &Page, x: usize, width: usize) -> bool {
    x + width <= usize::from(page.cols()) + 1
}

/// Legacy `Line::set_cell`, or with `clear`,
/// `set_cell_clearing_image_placements`.
pub fn set_cell(
    page: &mut Page,
    row: u32,
    x: usize,
    cell: &Cell,
    clear: bool,
    seqno: SequenceNo,
) -> bool {
    if !fits(page, x, cell.width().clamp(1, 2)) {
        return false;
    }
    // `set_cell_impl` moves the seqno first, whatever becomes of the cell.
    page.touch_row(row, seqno);
    if is_clustered(page, row) {
        if x > page.row_len(row) && *cell == Cell::blank() {
            return true;
        }
        let first = cell.str().chars().next().unwrap_or(' ');
        if !clustered_can_append(page, row, x, first) {
            set_vector(page, row);
        }
    }
    page.write_legacy(row, x, cell, clear, seqno)
}

/// Legacy `Line::set_cell_grapheme`.
pub fn set_cell_grapheme(
    page: &mut Page,
    row: u32,
    x: usize,
    text: &str,
    width: usize,
    attr: &CellAttributes,
    seqno: SequenceNo,
) -> bool {
    let width = width.clamp(1, 2);
    if !fits(page, x, width) {
        return false;
    }
    // Legacy builds `Cell::new_grapheme_with_width(text, width, attr)` and
    // writes it; the page writes its parts directly (T0 prints one such cell
    // per emoji).
    let stored = stored_text(text);
    if is_clustered(page, row) {
        // Unlike `set_cell`, this returns before the seqno moves.
        if x > page.row_len(row) && text == " " && *attr == CellAttributes::blank() {
            return true;
        }
        // `ClusteredLine::can_append_cell_at`: text with no first character
        // never appends. Text that does append starts with an inert
        // character, so the cell keeps it as it is.
        if let Some(first) = text.chars().next() {
            if clustered_can_append(page, row, x, first) {
                return write_grapheme(page, row, x, stored, width, attr, seqno);
            }
        }
    }
    // `set_cell` of that cell.
    page.touch_row(row, seqno);
    if is_clustered(page, row) {
        if x > page.row_len(row)
            && stored == " "
            && *attr == CellAttributes::blank()
            && Cell::new_grapheme_with_width(text, width, attr.clone()) == Cell::blank()
        {
            return true;
        }
        let first = stored.chars().next().unwrap_or(' ');
        if !clustered_can_append(page, row, x, first) {
            set_vector(page, row);
        }
    }
    write_grapheme(page, row, x, stored, width, attr, seqno)
}

/// The text `Cell::new_grapheme_with_width` keeps (`TeenyString::from_str`):
/// empty text, CR LF and a lone control byte become a space.
fn stored_text(text: &str) -> &str {
    match text.as_bytes() {
        [] | b"\r\n" => " ",
        [byte] if *byte < 0x20 || *byte == 0x7f => " ",
        _ => text,
    }
}

/// `Page::write_legacy` of `Cell::new_grapheme_with_width(.., width, attr)`
/// whose kept text is `stored`, without building the cell. `width` is
/// already in 1..=2, as the cell normalizes it.
fn write_grapheme(
    page: &mut Page,
    row: u32,
    x: usize,
    stored: &str,
    width: usize,
    attr: &CellAttributes,
    seqno: SequenceNo,
) -> bool {
    let images: Vec<Box<ImageCell>> = attr
        .images()
        .map(|images| images.into_iter().map(Box::new).collect())
        .unwrap_or_default();
    let class = classify(attr);
    let mut cache = None;
    let style = match &class {
        StyleClass::Inline(inline) => StyleSpec::Inline(*inline),
        StyleClass::Rich(rich) => StyleSpec::Rich {
            style: rich,
            cache: &mut cache,
        },
    };
    let write = CellWrite {
        glyph: Glyph::from_text(stored),
        wide: width >= 2,
        style,
        semantic: attr.semantic_type(),
        wrapped: attr.wrapped(),
        hyperlink: attr.hyperlink(),
        images: &images,
        clear_image_placements: false,
    };
    page.write(row, x, write, seqno)
}

/// Legacy `Screen::set_ascii_cell_run`: each byte of the printable-ASCII
/// `text` as `set_cell` writes it, starting at `x`, with the style
/// classified once for the run.
pub fn set_ascii_run(
    page: &mut Page,
    row: u32,
    x: usize,
    text: &str,
    attr: &CellAttributes,
    seqno: SequenceNo,
) -> bool {
    if !fits(page, x, text.len()) {
        return false;
    }
    let class = classify(attr);
    let mut cache = None;
    let images: Vec<Box<ImageCell>> = attr
        .images()
        .map(|images| images.into_iter().map(Box::new).collect())
        .unwrap_or_default();
    let Some(&first) = text.as_bytes().first() else {
        return true;
    };

    // The common case appends the run at or past the row's end: a vector
    // row, or a clustered row whose last character lets it append (printable
    // ASCII then always breaks from ASCII). Past a clustered row's end,
    // legacy leaves leading default blanks implicit and pads the gap with
    // default blanks when a cell follows, which storing the run gives too;
    // a run of only such blanks there changes nothing but the seqno, which
    // the per-byte path below applies.
    //
    // A run that starts inside the row overwrites it. Legacy's `set_cell`
    // makes a clustered row a vector row there, and then stores each cell
    // raw, which `Page::put_ascii` does in bulk unless a cell needs
    // `Page::write`'s handling.
    let len = page.row_len(row);
    let clustered = is_clustered(page, row);
    let appends = if clustered {
        x >= len
            && clustered_can_append(page, row, x, char::from(first))
            && !(x > len
                && text.bytes().all(|byte| byte == b' ')
                && *attr == CellAttributes::blank())
    } else {
        x >= len
    };
    if images.is_empty() && (appends || x < len) {
        let style = match &class {
            StyleClass::Inline(inline) => StyleSpec::Inline(*inline),
            StyleClass::Rich(rich) => StyleSpec::Rich {
                style: rich,
                cache: &mut cache,
            },
        };
        let mut write = CellWrite::new(Glyph::Blank, style);
        write.semantic = attr.semantic_type();
        write.wrapped = attr.wrapped();
        write.hyperlink = attr.hyperlink();
        let stored = if appends {
            page.append_ascii(row, x, text.as_bytes(), write, seqno)
        } else {
            if clustered {
                set_vector(page, row);
            }
            page.put_ascii(row, x, text.as_bytes(), write, seqno)
        };
        if stored {
            return true;
        }
    }

    let blank_is_default = Cell::new(' ', attr.clone()) == Cell::blank();
    for (offset, byte) in text.bytes().enumerate() {
        let at = x + offset;
        page.touch_row(row, seqno);
        if is_clustered(page, row) {
            if at > page.row_len(row) && byte == b' ' && blank_is_default {
                continue;
            }
            if !clustered_can_append(page, row, at, char::from(byte)) {
                set_vector(page, row);
            }
        }
        let style = match &class {
            StyleClass::Inline(inline) => StyleSpec::Inline(*inline),
            StyleClass::Rich(rich) => StyleSpec::Rich {
                style: rich,
                cache: &mut cache,
            },
        };
        let write = CellWrite {
            glyph: if byte == b' ' {
                Glyph::Blank
            } else {
                Glyph::Char(char::from(byte))
            },
            wide: false,
            style,
            semantic: attr.semantic_type(),
            wrapped: attr.wrapped(),
            hyperlink: attr.hyperlink(),
            images: &images,
            clear_image_placements: false,
        };
        page.write(row, at, write, seqno);
    }
    true
}

/// Legacy `Line::set_last_cell_was_wrapped`.
pub fn set_last_cell_was_wrapped(page: &mut Page, row: u32, wrapped: bool, seqno: SequenceNo) {
    page.touch_row(row, seqno);
    if is_clustered(page, row) && page.row_len(row) == 0 {
        if !wrapped {
            return;
        }
        // Clustered storage marks the implicit space by appending it.
        page.write_legacy(row, 0, &Cell::blank(), false, seqno);
    }
    let Some(mut last) = page.row_len(row).checked_sub(1) else {
        return;
    };
    while last > 0 && page.cell(row, last).is_hidden() {
        last -= 1;
    }
    page.set_wrapped_from(row, last, wrapped, seqno);
}

/// Legacy `Line::fill_range(cols, &Cell::blank_with_attrs(attr), seqno)`,
/// as erasing (EL, ED, ECH) and the scroll within left and right margins
/// call it: the cells written as blanks, then trailing default blanks
/// pruned. Returns false, writing nothing, only for a range past the page.
pub fn fill_blank(
    page: &mut Page,
    row: u32,
    cols: Range<usize>,
    attr: &CellAttributes,
    seqno: SequenceNo,
) -> bool {
    if cols.start >= cols.end {
        return true;
    }
    let cell = Cell::blank_with_attrs(attr.clone());
    let len = page.row_len(row);
    let is_default = cell == Cell::blank();
    if is_default && (len == 0 || cols.start >= len) {
        // A default fill past the content is a no-op, except over the
        // implicit placeholder of a final visible wide cell: legacy then
        // blanks that cell with its own attributes, and prunes.
        let last_is_wide = len > 0 && {
            let last = page.cell(row, len - 1);
            !last.is_hidden() && last.is_wide()
        };
        if cols.start == len && last_is_wide {
            let head = Cell::blank_with_attrs(page.cell_attributes(row, len - 1));
            set_vector(page, row);
            page.put_legacy_cells(row, len - 1, &[head], seqno);
            page.prune_trailing_blanks(row, seqno);
        }
        return true;
    }
    let end = if is_default {
        cols.end.min(len)
    } else {
        cols.end
    };
    if end <= cols.start {
        return true;
    }
    if !fits(page, end, 0) {
        return false;
    }
    if is_default && cols.start == 0 && end == len {
        // Every stored cell becomes a default blank, and pruning then empties
        // the row; the fill turned it into a vector row.
        let kept = page.header(row).bits() & RowHeader::LINE_FLAGS & !RowHeader::LEGACY_FORM_C;
        page.clear_row(row, seqno);
        if kept != 0 {
            page.set_row_flags(row, kept, true);
        }
        return true;
    }
    page.touch_row(row, seqno);
    set_vector(page, row);
    for x in cols.start..end {
        // A fill replaces cells outright: no placement carries over.
        page.write_legacy(row, x, &cell, true, seqno);
    }
    page.prune_trailing_blanks(row, seqno);
    true
}

/// Legacy `Line::compress_for_scrollback`: a vector row becomes clustered
/// (its hidden cells rebuilt, ADR Q3, and the seqno unmoved) when clustered
/// storage reproduces its cells: when every boundary is inert, or else
/// when segmenting the cells' text gives back exactly the cells
/// (`ClusteredLine::reproduces`). Otherwise it stays a vector row. Returns
/// false only for a final wide cell whose placeholder was truncated, which
/// the `Line` handles.
pub fn compress_for_scrollback(page: &mut Page, row: u32) -> bool {
    if is_clustered(page, row) {
        return true;
    }
    let len = page.row_len(row);
    // A final wide cell whose placeholder was truncated gains it back in
    // clustered storage, growing the row: the `Line` handles that.
    if len > 0 && page.cell(row, len - 1).is_wide() {
        return false;
    }
    let mut prev = None;
    for x in 0..len {
        if page.cell(row, x).is_hidden() {
            continue;
        }
        let (first, last) = match page.glyph(row, x) {
            Glyph::Blank => (' ', ' '),
            Glyph::Char(ch) => (ch, ch),
            Glyph::Cluster(text) => (
                text.chars().next().unwrap_or(' '),
                text.chars().next_back().unwrap_or(' '),
            ),
        };
        if !clustered_append_breaks(prev, first) {
            // A boundary that might cluster: legacy re-segments.
            if !segmentation_reproduces(page, row) {
                return true;
            }
            break;
        }
        prev = Some(last);
    }
    page.rewrite_hidden_cells(row);
    page.set_row_flags(row, RowHeader::LEGACY_FORM_C, true);
    true
}

/// The text of the cell at `x`, as legacy's cell holds it.
fn cell_str<'a>(page: &'a Page, row: u32, x: usize, utf8: &'a mut [u8; 4]) -> &'a str {
    match page.glyph(row, x) {
        Glyph::Blank => " ",
        Glyph::Char(ch) => ch.encode_utf8(utf8),
        Glyph::Cluster(text) => text,
    }
}

/// `ClusteredLine::reproduces` past its inert-boundary shortcut: clustered
/// storage keeps the row's text and re-segments it, so it stands for the
/// row only when segmenting the visible cells' concatenated text gives back
/// exactly those cells. (The widths then agree too: both sides advance
/// column by column through the same wide cells.)
fn segmentation_reproduces(page: &Page, row: u32) -> bool {
    let len = page.row_len(row);
    let mut text = String::with_capacity(len * 4);
    let mut utf8 = [0_u8; 4];
    for x in 0..len {
        if !page.cell(row, x).is_hidden() {
            text.push_str(cell_str(page, row, x, &mut utf8));
        }
    }
    let mut graphemes = Graphemes::new(&text);
    for x in 0..len {
        if page.cell(row, x).is_hidden() {
            continue;
        }
        if graphemes.next() != Some(cell_str(page, row, x, &mut utf8)) {
            return false;
        }
    }
    graphemes.next().is_none()
}

/// Legacy `Screen::insert_cell` (ICH, and printing in insert mode): the
/// seqno moves, `Line::insert_cell(x, Cell::default(), right_margin)` makes
/// it a vector row, and a row grown past `limit` (the screen width) is cut
/// back to it. Returns false for no right margin, or a column past the
/// page.
pub fn insert_blank(
    page: &mut Page,
    row: u32,
    x: usize,
    right_margin: usize,
    limit: usize,
    seqno: SequenceNo,
) -> bool {
    if right_margin == 0 || x >= usize::from(page.cols()) {
        return false;
    }
    set_vector(page, row);
    page.insert_blank(row, x, right_margin, limit, seqno);
    true
}

/// Legacy `Line::erase_cell_with_margin` (DCH): the cell at `x` deleted,
/// the cells after it moved left, and a blank with `blank_attr` inserted
/// at `right_margin - 1`. The row becomes a vector row when a cell moves.
/// Returns false for a margin past the page.
pub fn erase_cell(
    page: &mut Page,
    row: u32,
    x: usize,
    right_margin: usize,
    blank_attr: &CellAttributes,
    seqno: SequenceNo,
) -> bool {
    if right_margin == 0 {
        // Legacy returns first, seqno included.
        return true;
    }
    if right_margin > usize::from(page.cols()) {
        return false;
    }
    let len = page.row_len(row);
    let removes = x < len;
    let kept = if removes { len - 1 } else { len };
    if removes || right_margin - 1 <= kept {
        set_vector(page, row);
    }
    let images: Vec<Box<ImageCell>> = blank_attr
        .images()
        .map(|images| images.into_iter().map(Box::new).collect())
        .unwrap_or_default();
    let class = classify(blank_attr);
    let mut cache = None;
    let style = match &class {
        StyleClass::Inline(inline) => StyleSpec::Inline(*inline),
        StyleClass::Rich(rich) => StyleSpec::Rich {
            style: rich,
            cache: &mut cache,
        },
    };
    let mut blank = CellWrite::new(Glyph::Blank, style);
    blank.semantic = blank_attr.semantic_type();
    blank.wrapped = blank_attr.wrapped();
    blank.hyperlink = blank_attr.hyperlink();
    blank.images = &images;
    page.erase_cell_with_margin(row, x, right_margin, blank, seqno);
    true
}

/// A row's size, as DECSWL, DECDWL and DECDHL (ESC # 5, 6, 3, 4) set it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineSize {
    Single,
    DoubleWidth,
    DoubleHeightTop,
    DoubleHeightBottom,
}

const SIZE_FLAGS: u64 =
    RowHeader::DOUBLE_WIDTH | RowHeader::DOUBLE_HEIGHT_TOP | RowHeader::DOUBLE_HEIGHT_BOTTOM;

impl LineSize {
    /// The legacy `Line` method: `set_single_width`, `set_double_width`,
    /// `set_double_height_top` or `set_double_height_bottom`.
    pub fn apply_to_line(self, line: &mut Line, seqno: SequenceNo) {
        match self {
            Self::Single => line.set_single_width(seqno),
            Self::DoubleWidth => line.set_double_width(seqno),
            Self::DoubleHeightTop => line.set_double_height_top(seqno),
            Self::DoubleHeightBottom => line.set_double_height_bottom(seqno),
        }
    }

    /// The double-size row flags the legacy method leaves.
    fn row_flags(self) -> u64 {
        match self {
            Self::Single => 0,
            Self::DoubleWidth => RowHeader::DOUBLE_WIDTH,
            Self::DoubleHeightTop => RowHeader::DOUBLE_WIDTH | RowHeader::DOUBLE_HEIGHT_TOP,
            Self::DoubleHeightBottom => RowHeader::DOUBLE_WIDTH | RowHeader::DOUBLE_HEIGHT_BOTTOM,
        }
    }
}

/// [`LineSize::apply_to_line`] on a page row: the size flags replaced, and
/// the seqno moves.
pub fn set_line_size(page: &mut Page, row: u32, size: LineSize, seqno: SequenceNo) {
    page.set_row_flags(row, SIZE_FLAGS, false);
    let flags = size.row_flags();
    if flags != 0 {
        page.set_row_flags(row, flags, true);
    }
    page.touch_row(row, seqno);
}

/// Legacy DECALN on one row: `Line::resize(cols)`, then
/// `fill_range(0..cols, &Cell::new('E', CellAttributes::default()))`,
/// which leave a vector row of `cols` plain `E` cells, line flags kept.
/// Returns false for a width past the page.
pub fn fill_alignment(page: &mut Page, row: u32, cols: usize, seqno: SequenceNo) -> bool {
    if !fits(page, cols, 0) {
        return false;
    }
    let kept = page.header(row).bits() & RowHeader::LINE_FLAGS & !RowHeader::LEGACY_FORM_C;
    page.clear_row(row, seqno);
    if kept != 0 {
        page.set_row_flags(row, kept, true);
    }
    let cell = Cell::new('E', CellAttributes::default());
    for x in 0..cols {
        page.write_legacy(row, x, &cell, false, seqno);
    }
    true
}

/// The source side of legacy's scroll within left and right margins: the
/// row's stored cells in `band`, which legacy reads through `cells_mut`,
/// making it a vector row without moving its seqno.
pub fn take_band(page: &mut Page, row: u32, band: Range<usize>) -> Vec<Cell> {
    set_vector(page, row);
    let end = band.end.min(page.row_len(row));
    (band.start..end)
        .map(|x| page.legacy_cell(row, x))
        .collect()
}

/// The destination side of legacy's scroll within left and right margins:
/// the seqno moves, the row is padded to `start + cells.len()`, `cells` are
/// stored there raw, and the rest of the band, up to `band_end`, is filled
/// with blanks with `blank_attr` (legacy `fill_range`). The row becomes a
/// vector row. Returns false for cells past the page.
pub fn put_band(
    page: &mut Page,
    row: u32,
    start: usize,
    cells: &[Cell],
    band_end: usize,
    blank_attr: &CellAttributes,
    seqno: SequenceNo,
) -> bool {
    let end = start + cells.len();
    if !fits(page, end.max(band_end), 0) || cells.iter().any(|cell| cell.width() > 2) {
        return false;
    }
    page.touch_row(row, seqno);
    set_vector(page, row);
    if page.row_len(row) < end {
        page.resize_row(row, end, seqno);
    }
    page.put_legacy_cells(row, start, cells, seqno);
    let filled = fill_blank(page, row, end..band_end, blank_attr, seqno);
    debug_assert!(filled, "the band fits the page");
    true
}

/// A row legacy's scroll within left and right margins vacates: the seqno
/// moves, and the row's stored cells in `band` become blanks with
/// `blank_attr`, raw, with nothing padded or pruned. The row becomes a
/// vector row.
pub fn blank_band(
    page: &mut Page,
    row: u32,
    band: Range<usize>,
    blank_attr: &CellAttributes,
    seqno: SequenceNo,
) {
    page.touch_row(row, seqno);
    set_vector(page, row);
    let end = band.end.min(page.row_len(row));
    if band.start < end {
        let cells = vec![Cell::blank_with_attrs(blank_attr.clone()); end - band.start];
        page.put_legacy_cells(row, band.start, &cells, seqno);
    }
}

/// Legacy `Line::semantic_zone_ranges` (`compute_zones`) on a page row: the
/// runs of visible cells by semantic type, as `(type, columns)`, with each
/// run ending one past its last cell's column. Visible cells after the last
/// non-blank one are left out, unless the row is all blanks, in which case
/// legacy keeps every cell.
pub fn semantic_zone_ranges(page: &Page, row: u32) -> Vec<(SemanticType, Range<u16>)> {
    let len = page.row_len(row);
    let is_blank = |x: usize| page.cell(row, x).with_hidden(false).with_wide(false) == PackedCell::BLANK;
    let mut last_non_blank = len;
    for x in 0..len {
        if !page.cell(row, x).is_hidden() && !is_blank(x) {
            last_non_blank = x;
        }
    }
    let mut zones: Vec<(SemanticType, Range<u16>)> = Vec::new();
    let mut last = None;
    for x in 0..len {
        let cell = page.cell(row, x);
        if cell.is_hidden() {
            continue;
        }
        if x > last_non_blank {
            break;
        }
        // Legacy's `cell_index() as u16`.
        let start = x as u16;
        let end = start.saturating_add(1);
        let semantic = cell.semantic();
        if last != Some(semantic) {
            zones.push((semantic, start..end));
        } else if let Some(zone) = zones.last_mut() {
            zone.1.end = end;
        }
        last = Some(semantic);
    }
    zones
}

/// The visible cell a grapheme printed at `cursor_x` may continue, as the
/// performer's merge looks for it (ft-yccm0.2.14): the one at the cursor
/// when a wrap is pending, otherwise the last one before the cursor.
pub fn merge_candidate(page: &Page, row: u32, cursor_x: usize, pending_wrap: bool) -> Option<usize> {
    let len = page.row_len(row);
    if pending_wrap && cursor_x < len && !page.cell(row, cursor_x).is_hidden() {
        return Some(cursor_x);
    }
    let mut x = cursor_x.min(len).checked_sub(1)?;
    while page.cell(row, x).is_hidden() {
        x = x.checked_sub(1)?;
    }
    Some(x)
}

/// `cols` blanks with `attr` appended to the empty row, in bulk: what
/// `write_legacy` of each would store. Returns false, writing nothing, when
/// `attr` carries images or the row is not empty.
fn append_blanks(
    page: &mut Page,
    row: u32,
    cols: usize,
    attr: &CellAttributes,
    seqno: SequenceNo,
) -> bool {
    const SPACES: [u8; 64] = [b' '; 64];
    if attr.images().is_some() || page.row_len(row) != 0 || cols > usize::from(page.cols()) {
        return false;
    }
    let class = classify(attr);
    let mut cache = None;
    let mut x = 0;
    while x < cols {
        let n = (cols - x).min(SPACES.len());
        let style = match &class {
            StyleClass::Inline(inline) => StyleSpec::Inline(*inline),
            StyleClass::Rich(rich) => StyleSpec::Rich {
                style: rich,
                cache: &mut cache,
            },
        };
        let mut write = CellWrite::new(Glyph::Blank, style);
        write.semantic = attr.semantic_type();
        write.wrapped = attr.wrapped();
        write.hyperlink = attr.hyperlink();
        let appended = page.append_ascii(row, x, &SPACES[..n], write, seqno);
        debug_assert!(appended, "an empty row takes its blanks");
        x += n;
    }
    true
}

/// A row new to the screen, as legacy's scroll makes one in the empty row
/// `row`: clustered and empty for a default `blank_attr`, otherwise `cols`
/// blanks with `blank_attr` in vector storage. With `bidi`, the screen's
/// bidi mode is applied, as legacy does for every new row except those its
/// eviction path reuses (ADR section 9).
pub fn new_row(
    page: &mut Page,
    row: u32,
    cols: usize,
    blank_attr: &CellAttributes,
    bidi: Option<&BidiMode>,
    seqno: SequenceNo,
) {
    if *blank_attr == CellAttributes::blank() {
        page.set_row_flags(row, RowHeader::LEGACY_FORM_C, true);
    } else {
        let cols = cols.min(usize::from(page.cols()));
        if !append_blanks(page, row, cols, blank_attr, seqno) {
            let cell = Cell::blank_with_attrs(blank_attr.clone());
            for x in 0..cols {
                page.write_legacy(row, x, &cell, false, seqno);
            }
        }
    }
    if let Some(mode) = bidi {
        apply_bidi(page, row, mode, seqno);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{ColorAttribute, SrgbaTuple};
    use crate::pagegrid::view::{line_view, store_line, stored_cells};
    use frankenterm_cell::{Hyperlink, Intensity};
    use proptest::prelude::*;
    use std::sync::Arc;

    const COLS: u16 = 12;

    /// The page row and the legacy line as the same decoded row: stored
    /// cells, length, storage form and seqno.
    fn assert_same_row(page: &Page, row: u32, line: &Line, what: &str) {
        page.check_invariants()
            .unwrap_or_else(|err| panic!("{}: {}", what, err));
        let view = line_view(page, row, page.row_seqno(row));
        let clustered = line.clustered_storage_owners().is_some();
        assert_eq!(
            view.clustered_storage_owners().is_some(),
            clustered,
            "{}: storage form",
            what
        );
        assert_eq!(view.len(), line.len(), "{}: len", what);
        assert_eq!(
            stored_cells(&view, clustered),
            stored_cells(line, clustered),
            "{}: cells",
            what
        );
        assert_eq!(view.bidi_info(), line.bidi_info(), "{}: bidi", what);
        let size = |line: &Line| {
            (
                line.is_double_width(),
                line.is_double_height_top(),
                line.is_double_height_bottom(),
            )
        };
        assert_eq!(size(&view), size(line), "{}: size", what);
        assert_eq!(page.row_seqno(row), line.current_seqno(), "{}: seqno", what);
    }

    /// Eight pens: plain, bold, palette background, a hyperlink, palette
    /// foreground with the wrapped bit, a true-colour foreground (which
    /// pages store as a rich style), and the Prompt and Input semantic
    /// types (OSC 133).
    fn attrs(n: u8) -> CellAttributes {
        let mut attrs = CellAttributes::default();
        match n % 8 {
            0 => {}
            1 => {
                attrs.set_intensity(Intensity::Bold);
            }
            2 => {
                attrs.set_background(ColorAttribute::PaletteIndex(4));
            }
            3 => {
                attrs.set_hyperlink(Some(Arc::new(Hyperlink::new("https://native.example/"))));
            }
            4 => {
                attrs.set_foreground(ColorAttribute::PaletteIndex(196));
                attrs.set_wrapped(true);
            }
            5 => {
                attrs.set_foreground(ColorAttribute::TrueColorWithDefaultFallback(
                    SrgbaTuple(0.25, 0.5, 0.75, 1.0),
                ));
            }
            6 => {
                attrs.set_semantic_type(SemanticType::Prompt);
            }
            _ => {
                attrs.set_semantic_type(SemanticType::Input);
            }
        }
        attrs
    }

    /// Legacy's zone ranges for `line`, as the native function gives them.
    fn legacy_zones(line: &Line) -> Vec<(SemanticType, std::ops::Range<u16>)> {
        line.clone()
            .semantic_zone_ranges()
            .iter()
            .map(|zone| (zone.semantic_type, zone.range.clone()))
            .collect()
    }

    /// The performer's legacy row walk for the merge candidate: its index,
    /// width, text and attributes.
    fn legacy_candidate(
        line: &Line,
        cursor_x: usize,
        pending_wrap: bool,
    ) -> Option<(usize, usize, String, CellAttributes)> {
        let mut candidate = None;
        for cell in line.visible_cells() {
            let cell_index = cell.cell_index();
            if pending_wrap && cell_index == cursor_x {
                candidate = Some(cell);
                break;
            }
            if cell_index < cursor_x {
                candidate = Some(cell);
            } else {
                break;
            }
        }
        candidate.map(|cell| {
            (
                cell.cell_index(),
                cell.width(),
                cell.str().to_string(),
                cell.attrs().clone(),
            )
        })
    }

    /// What the native merge-candidate read gives for the page row.
    fn native_candidate(
        page: &Page,
        row: u32,
        cursor_x: usize,
        pending_wrap: bool,
    ) -> Option<(usize, usize, String, CellAttributes)> {
        let idx = merge_candidate(page, row, cursor_x, pending_wrap)?;
        let text = match page.glyph(row, idx) {
            Glyph::Blank => " ".to_string(),
            Glyph::Char(ch) => ch.to_string(),
            Glyph::Cluster(text) => text.to_string(),
        };
        let width = if page.cell(row, idx).is_wide() { 2 } else { 1 };
        Some((idx, width, text, page.cell_attributes(row, idx)))
    }

    #[derive(Clone, Debug)]
    enum Op {
        SetCell { x: usize, glyph: usize, attrs: u8 },
        Grapheme { x: usize, glyph: usize, attrs: u8 },
        Ascii { x: usize, text: usize, attrs: u8 },
        Wrapped { wrapped: bool },
        Fill { start: usize, end: usize, attrs: u8 },
        Compress,
        Insert { x: usize, margin: usize },
        Delete { x: usize, margin: usize, attrs: u8 },
        Size { size: usize },
        Align,
        TakeBand { start: usize, end: usize },
        PutBand { start: usize, cells: Vec<(usize, u8)>, end: usize, attrs: u8 },
        BlankBand { start: usize, end: usize, attrs: u8 },
    }

    const SIZES: [LineSize; 4] = [
        LineSize::Single,
        LineSize::DoubleWidth,
        LineSize::DoubleHeightTop,
        LineSize::DoubleHeightBottom,
    ];

    /// Cells including the ones that defeat clustered storage's cheap
    /// boundary test (combining marks, VS16, ZWJ sequences, a lone regional
    /// indicator that pairs with its neighbour), plus text that `Cell`
    /// de-fangs (empty, a control byte).
    const GLYPHS: [(&str, usize); 12] = [
        (" ", 1),
        ("a", 1),
        ("\u{e9}", 1),
        ("e\u{301}", 1),
        ("\u{301}", 1),
        ("\u{4e2d}", 2),
        ("\u{1f600}", 2),
        ("\u{2764}\u{fe0f}", 2),
        ("\u{1f468}\u{200d}\u{1f469}", 2),
        ("\u{1f1e6}", 2),
        ("", 1),
        ("\u{7}", 1),
    ];
    const TEXTS: [&str; 4] = ["abc", "  x", "   ", "hello"];

    fn op() -> impl Strategy<Value = Op> {
        let x = 0usize..11;
        prop_oneof![
            (x.clone(), 0..GLYPHS.len(), 0u8..8).prop_map(|(x, glyph, attrs)| Op::SetCell {
                x,
                glyph,
                attrs
            }),
            (x.clone(), 0..GLYPHS.len(), 0u8..8).prop_map(|(x, glyph, attrs)| Op::Grapheme {
                x,
                glyph,
                attrs
            }),
            (0usize..8, 0..TEXTS.len(), 0u8..8).prop_map(|(x, text, attrs)| Op::Ascii {
                x,
                text,
                attrs
            }),
            any::<bool>().prop_map(|wrapped| Op::Wrapped { wrapped }),
            (0usize..13, 0usize..14, 0u8..8).prop_map(|(start, end, attrs)| Op::Fill {
                start,
                end,
                attrs
            }),
            Just(Op::Compress),
            (0usize..12, 0usize..13).prop_map(|(x, margin)| Op::Insert { x, margin }),
            (0usize..13, 0usize..13, 0u8..8).prop_map(|(x, margin, attrs)| Op::Delete {
                x,
                margin,
                attrs
            }),
            (0..SIZES.len()).prop_map(|size| Op::Size { size }),
            Just(Op::Align),
            (0usize..12, 0usize..13).prop_map(|(start, end)| Op::TakeBand { start, end }),
            (
                0usize..10,
                prop::collection::vec((0..GLYPHS.len(), 0u8..8), 0..4),
                0usize..13,
                0u8..8
            )
                .prop_map(|(start, cells, end, attrs)| Op::PutBand {
                    start,
                    cells,
                    end,
                    attrs
                }),
            (0usize..12, 0usize..13, 0u8..8).prop_map(|(start, end, attrs)| Op::BlankBand {
                start,
                end,
                attrs
            }),
        ]
    }

    /// ft-yccm0.3.3.4: a new row is what legacy's scroll builds, `Line::new`
    /// for the default pen and otherwise `cols` blanks with the pen, whose
    /// bulk store must match a `write_legacy` of each. The page row also
    /// stays dirty.
    #[test]
    fn new_rows_match_legacy_new_lines() {
        for pen in 0..8 {
            let attr = attrs(pen);
            for cols in [0, 1, 11, usize::from(COLS)] {
                // Scroll hands over the row empty and carrying the seqno.
                let mut page = Page::new(COLS, 1, 1);
                let row = page.grow(7).expect("a row");
                page.take_dirty(row);
                new_row(&mut page, row, cols, &attr, None, 7);
                let line = if attr == CellAttributes::blank() {
                    Line::new(7)
                } else {
                    Line::with_width_and_cell(cols, Cell::blank_with_attrs(attr.clone()), 7)
                };
                assert_same_row(&page, row, &line, &format!("pen {} cols {}", pen, cols));
                assert!(page.take_dirty(row) || cols == 0 || attr == CellAttributes::blank());
            }
        }
    }

    proptest! {
        /// ft-yccm0.3.3.4: each native edit leaves the page row equal to the
        /// legacy line after the same `Line` method, whatever the storage
        /// form; an edit the native code declines is applied to a view and
        /// stored back, as the screen does.
        #[test]
        fn native_edits_match_legacy_line_methods(
            ops in prop::collection::vec(op(), 1..24),
        ) {
            let mut page = Page::new(COLS, 1, 1);
            let row = page.grow(1).expect("a row");
            let mut line = Line::new(1);
            assert!(store_line(&mut page, row, &line));
            for (step, op) in ops.iter().enumerate() {
                let seqno = step + 2;
                let legacy_seqno = line.current_seqno();
                page.take_dirty(row);
                let native = match op {
                    Op::SetCell { x, glyph, attrs: a } => {
                        let (text, width) = GLYPHS[*glyph];
                        let cell = Cell::new_grapheme_with_width(text, width, attrs(*a));
                        line.set_cell(*x, cell.clone(), seqno);
                        set_cell(&mut page, row, *x, &cell, false, seqno)
                    }
                    Op::Grapheme { x, glyph, attrs: a } => {
                        let (text, width) = GLYPHS[*glyph];
                        line.set_cell_grapheme(*x, text, width, attrs(*a), seqno);
                        set_cell_grapheme(&mut page, row, *x, text, width, &attrs(*a), seqno)
                    }
                    Op::Ascii { x, text, attrs: a } => {
                        let text = TEXTS[*text];
                        for (offset, byte) in text.bytes().enumerate() {
                            line.set_cell(*x + offset, Cell::new(char::from(byte), attrs(*a)), seqno);
                        }
                        set_ascii_run(&mut page, row, *x, text, &attrs(*a), seqno)
                    }
                    Op::Wrapped { wrapped } => {
                        line.set_last_cell_was_wrapped(*wrapped, seqno);
                        set_last_cell_was_wrapped(&mut page, row, *wrapped, seqno);
                        true
                    }
                    Op::Fill { start, end, attrs: a } => {
                        let attr = attrs(*a);
                        let before = line.clone();
                        line.fill_range(*start..*end, &Cell::blank_with_attrs(attr.clone()), seqno);
                        if fill_blank(&mut page, row, *start..*end, &attr, seqno) {
                            true
                        } else {
                            // Declined: the row is unchanged, as the screen
                            // then edits its view.
                            assert_same_row(&page, row, &before, "declined fill");
                            false
                        }
                    }
                    Op::Compress => {
                        let before = line.clone();
                        line.compress_for_scrollback();
                        if compress_for_scrollback(&mut page, row) {
                            true
                        } else {
                            assert_same_row(&page, row, &before, "declined compress");
                            false
                        }
                    }
                    Op::Insert { x, margin } => {
                        // `Screen::insert_cell`.
                        let before = line.clone();
                        line.update_last_change_seqno(seqno);
                        line.insert_cell(*x, Cell::default(), *margin, seqno);
                        if line.len() > usize::from(COLS) {
                            line.resize(usize::from(COLS), seqno);
                        }
                        if insert_blank(&mut page, row, *x, *margin, usize::from(COLS), seqno) {
                            true
                        } else {
                            assert_same_row(&page, row, &before, "declined insert");
                            false
                        }
                    }
                    Op::Delete { x, margin, attrs: a } => {
                        let before = line.clone();
                        line.erase_cell_with_margin(*x, *margin, seqno, attrs(*a));
                        if erase_cell(&mut page, row, *x, *margin, &attrs(*a), seqno) {
                            true
                        } else {
                            assert_same_row(&page, row, &before, "declined delete");
                            false
                        }
                    }
                    Op::Size { size } => {
                        SIZES[*size].apply_to_line(&mut line, seqno);
                        set_line_size(&mut page, row, SIZES[*size], seqno);
                        true
                    }
                    Op::Align => {
                        let cols = usize::from(COLS);
                        line.resize(cols, seqno);
                        line.fill_range(0..cols, &Cell::new('E', CellAttributes::default()), seqno);
                        assert!(fill_alignment(&mut page, row, cols, seqno));
                        true
                    }
                    Op::TakeBand { start, end } => {
                        let legacy: Vec<Cell> = line
                            .cells_mut()
                            .iter()
                            .skip(*start)
                            .take(end.saturating_sub(*start))
                            .cloned()
                            .collect();
                        let taken = take_band(&mut page, row, *start..*end);
                        assert_eq!(taken, legacy, "step {}: band cells", step);
                        true
                    }
                    Op::PutBand { start, cells, end, attrs: a } => {
                        let cells: Vec<Cell> = cells
                            .iter()
                            .map(|(glyph, a)| {
                                let (text, width) = GLYPHS[*glyph];
                                Cell::new_grapheme_with_width(text, width, attrs(*a))
                            })
                            .collect();
                        let blank = attrs(*a);
                        // `Screen::copy_margin_band`'s destination.
                        let before = line.clone();
                        line.update_last_change_seqno(seqno);
                        let dest = *start..*start + cells.len();
                        if line.len() < dest.end {
                            line.resize(dest.end, seqno);
                        }
                        for (cell, slot) in cells.iter().zip(&mut line.cells_mut()[dest.clone()]) {
                            *slot = cell.clone();
                        }
                        line.fill_range(dest.end..*end, &Cell::blank_with_attrs(blank.clone()), seqno);
                        if put_band(&mut page, row, *start, &cells, *end, &blank, seqno) {
                            true
                        } else {
                            assert_same_row(&page, row, &before, "declined band");
                            false
                        }
                    }
                    Op::BlankBand { start, end, attrs: a } => {
                        let blank = attrs(*a);
                        line.update_last_change_seqno(seqno);
                        for cell in line
                            .cells_mut()
                            .iter_mut()
                            .skip(*start)
                            .take(end.saturating_sub(*start))
                        {
                            *cell = Cell::blank_with_attrs(blank.clone());
                        }
                        blank_band(&mut page, row, *start..*end, &blank, seqno);
                        true
                    }
                };
                if !native {
                    assert!(store_line(&mut page, row, &line));
                }
                let what = format!("step {} {:?}", step, op);
                assert_same_row(&page, row, &line, &what);
                assert_eq!(
                    semantic_zone_ranges(&page, row),
                    legacy_zones(&line),
                    "{}: zones",
                    what
                );
                for cursor_x in 0..=usize::from(COLS) + 1 {
                    for pending_wrap in [false, true] {
                        assert_eq!(
                            native_candidate(&page, row, cursor_x, pending_wrap),
                            legacy_candidate(&line, cursor_x, pending_wrap),
                            "{}: merge candidate at {} (pending wrap {})",
                            what,
                            cursor_x,
                            pending_wrap
                        );
                    }
                }
                if line.current_seqno() != legacy_seqno {
                    // Every mutation legacy's seqno sees also dirties the row.
                    assert!(page.take_dirty(row), "{}: not dirty", what);
                }
            }
        }
    }
}
