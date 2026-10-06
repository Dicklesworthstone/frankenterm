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

use super::cell::{classify, Glyph, StyleClass};
use super::page::{CellWrite, Page, StyleSpec};
use super::row::RowHeader;
use crate::config::BidiMode;
use frankenterm_bidi::ParagraphDirectionHint;
use frankenterm_cell::image::ImageCell;
use frankenterm_cell::{Cell, CellAttributes};
use frankenterm_surface::line::clustered_append_breaks;
use frankenterm_surface::SequenceNo;
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
    if is_clustered(page, row) {
        // Unlike `set_cell`, this returns before the seqno moves.
        if x > page.row_len(row) && text == " " && *attr == CellAttributes::blank() {
            return true;
        }
        let first = text.chars().next().unwrap_or(' ');
        if clustered_can_append(page, row, x, first) {
            let cell = Cell::new_grapheme_with_width(text, width, attr.clone());
            return page.write_legacy(row, x, &cell, false, seqno);
        }
    }
    let cell = Cell::new_grapheme_with_width(text, width, attr.clone());
    set_cell(page, row, x, &cell, false, seqno)
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

/// Legacy `Line::fill_range(cols, &Cell::blank_with_attrs(attr), seqno)`
/// for the shapes erasing takes: nothing to erase, the whole row erased to
/// default blanks (EL 2, ED), and erasing with a styled blank (background
/// colour erase). Returns false for the rest, partial default erases whose
/// pruning the `Line` applies.
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
        // implicit placeholder of a final wide cell.
        return !(cols.start == len && len > 0 && page.cell(row, len - 1).is_wide());
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
    if is_default {
        if cols.start != 0 || end != len {
            return false;
        }
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

/// Legacy `Line::compress_for_scrollback`, which turns a vector row whose
/// cells cannot cluster with each other into a clustered one (rebuilding
/// its hidden cells, ADR Q3) without moving the seqno. Returns false for a
/// vector row with a boundary that might cluster; the `Line` decides that
/// one.
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
            return false;
        }
        prev = Some(last);
    }
    page.rewrite_hidden_cells(row);
    page.set_row_flags(row, RowHeader::LEGACY_FORM_C, true);
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
        let cell = Cell::blank_with_attrs(blank_attr.clone());
        for x in 0..cols.min(usize::from(page.cols())) {
            page.write_legacy(row, x, &cell, false, seqno);
        }
    }
    if let Some(mode) = bidi {
        apply_bidi(page, row, mode, seqno);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorAttribute;
    use crate::pagegrid::view::{line_view, store_line, stored_cells};
    use frankenterm_cell::{Hyperlink, Intensity};
    use frankenterm_surface::Line;
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
        assert_eq!(page.row_seqno(row), line.current_seqno(), "{}: seqno", what);
    }

    fn attrs(n: u8) -> CellAttributes {
        let mut attrs = CellAttributes::default();
        match n % 5 {
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
            _ => {
                attrs.set_foreground(ColorAttribute::PaletteIndex(196));
                attrs.set_wrapped(true);
            }
        }
        attrs
    }

    #[derive(Clone, Debug)]
    enum Op {
        SetCell { x: usize, glyph: usize, attrs: u8 },
        Grapheme { x: usize, glyph: usize, attrs: u8 },
        Ascii { x: usize, text: usize, attrs: u8 },
        Wrapped { wrapped: bool },
        Fill { start: usize, end: usize, attrs: u8 },
        Compress,
    }

    const GLYPHS: [(&str, usize); 7] = [
        (" ", 1),
        ("a", 1),
        ("\u{e9}", 1),
        ("e\u{301}", 1),
        ("\u{301}", 1),
        ("\u{4e2d}", 2),
        ("\u{1f600}", 2),
    ];
    const TEXTS: [&str; 4] = ["abc", "  x", "   ", "hello"];

    fn op() -> impl Strategy<Value = Op> {
        let x = 0usize..11;
        prop_oneof![
            (x.clone(), 0..GLYPHS.len(), 0u8..5).prop_map(|(x, glyph, attrs)| Op::SetCell {
                x,
                glyph,
                attrs
            }),
            (x.clone(), 0..GLYPHS.len(), 0u8..5).prop_map(|(x, glyph, attrs)| Op::Grapheme {
                x,
                glyph,
                attrs
            }),
            (0usize..8, 0..TEXTS.len(), 0u8..5).prop_map(|(x, text, attrs)| Op::Ascii {
                x,
                text,
                attrs
            }),
            any::<bool>().prop_map(|wrapped| Op::Wrapped { wrapped }),
            (0usize..13, 0usize..14, 0u8..5).prop_map(|(start, end, attrs)| Op::Fill {
                start,
                end,
                attrs
            }),
            Just(Op::Compress),
        ]
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
                };
                if !native {
                    assert!(store_line(&mut page, row, &line));
                }
                assert_same_row(&page, row, &line, &format!("step {} {:?}", step, op));
            }
        }
    }
}
