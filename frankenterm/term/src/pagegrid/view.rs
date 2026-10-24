//! Legacy `Line` views of page rows, and the import of a `Line` into a row
//! (PageGrid ADR section 3.10; B3.4).
//!
//! [`line_view`] rebuilds the legacy row from the page: every stored cell
//! (hidden ones included), the length, the storage form legacy would hold
//! it in, the line flags and the seqno. [`store_line`] is its inverse, for
//! a row that a legacy operation edited through a view.
//!
//! Decoded rows round-trip exactly. A clustered row's attribute runs are
//! rebuilt canonically, so `Line::eq` also holds only where legacy's runs
//! were canonical (ADR Q8). The explicit-hyperlink bit is rebuilt from the
//! cells, so a link that legacy wrote and then overwrote no longer sets it.

use super::page::Page;
use super::row::RowHeader;
use frankenterm_bidi::ParagraphDirectionHint;
use frankenterm_cell::Cell;
use frankenterm_surface::{Line, SequenceNo};

/// Row `row` of `page` as the legacy `Line` it stands for, carrying
/// `seqno`, the row's effective seqno.
pub fn line_view(page: &Page, row: u32, seqno: SequenceNo) -> Line {
    let header = page.header(row);
    let cells: Vec<Cell> = (0..header.len())
        .map(|x| page.legacy_cell(row, x))
        .collect();
    let has_link = header.has(RowHeader::HYPERLINK)
        && cells.iter().any(|cell| {
            cell.attrs()
                .hyperlink()
                .is_some_and(|link| !link.is_implicit())
        });
    let mut line = Line::from_cells(cells, seqno);
    if header.has(RowHeader::LEGACY_FORM_C) {
        line.compress_for_scrollback();
    }

    let bidi_enabled = header.has(RowHeader::BIDI_ENABLED);
    let direction = match (
        header.has(RowHeader::AUTO_DETECT_DIRECTION),
        header.has(RowHeader::RTL),
    ) {
        (true, true) => ParagraphDirectionHint::AutoRightToLeft,
        (true, false) => ParagraphDirectionHint::AutoLeftToRight,
        (false, true) => ParagraphDirectionHint::RightToLeft,
        (false, false) => ParagraphDirectionHint::LeftToRight,
    };
    if bidi_enabled || direction != ParagraphDirectionHint::LeftToRight {
        line.set_bidi_info(bidi_enabled, direction, seqno);
    }
    if header.has(RowHeader::DOUBLE_HEIGHT_TOP) {
        line.set_double_height_top(seqno);
    } else if header.has(RowHeader::DOUBLE_HEIGHT_BOTTOM) {
        line.set_double_height_bottom(seqno);
    } else if header.has(RowHeader::DOUBLE_WIDTH) {
        line.set_double_width(seqno);
    }
    if has_link {
        line.set_has_hyperlink(true);
    }
    line
}

/// Stores `line` as row `row` of `page`: every stored cell, hidden ones
/// included, its length, storage form, line flags and seqno. Returns
/// false, leaving the row empty, when the line does not fit the page (see
/// [`Page::store_legacy_row`]).
pub fn store_line(page: &mut Page, row: u32, line: &Line) -> bool {
    let clustered = line.clustered_storage_owners().is_some();
    let cells = stored_cells(line, clustered);
    if !page.store_legacy_row(row, &cells, line.current_seqno()) {
        return false;
    }
    page.set_row_flags(row, line_flags(line, clustered), true);
    true
}

/// The row flags ([`RowHeader::LINE_FLAGS`]) for `line`: its storage form
/// and its bidi and double-size bits.
pub fn line_flags(line: &Line, clustered: bool) -> u64 {
    let mut flags = 0;
    if clustered {
        flags |= RowHeader::LEGACY_FORM_C;
    }
    let (bidi_enabled, direction) = line.bidi_info();
    if bidi_enabled {
        flags |= RowHeader::BIDI_ENABLED;
    }
    flags |= match direction {
        ParagraphDirectionHint::AutoRightToLeft => {
            RowHeader::AUTO_DETECT_DIRECTION | RowHeader::RTL
        }
        ParagraphDirectionHint::AutoLeftToRight => RowHeader::AUTO_DETECT_DIRECTION,
        ParagraphDirectionHint::RightToLeft => RowHeader::RTL,
        ParagraphDirectionHint::LeftToRight => 0,
    };
    if line.is_double_height_top() {
        flags |= RowHeader::DOUBLE_WIDTH | RowHeader::DOUBLE_HEIGHT_TOP;
    } else if line.is_double_height_bottom() {
        flags |= RowHeader::DOUBLE_WIDTH | RowHeader::DOUBLE_HEIGHT_BOTTOM;
    } else if line.is_double_width() {
        flags |= RowHeader::DOUBLE_WIDTH;
    }
    flags
}

/// Every cell `line` stores, hidden ones included. Clustered storage keeps
/// no hidden cells: legacy materializes each as a blank with its head's
/// attributes, so they are rebuilt that way.
pub fn stored_cells(line: &Line, clustered: bool) -> Vec<Cell> {
    if !clustered {
        return line.clone().cells_mut().to_vec();
    }
    let mut cells = Vec::with_capacity(line.len());
    for cell in line.visible_cells() {
        cells.push(cell.as_cell());
        for _ in 1..cell.width() {
            cells.push(Cell::blank_with_attrs(cell.attrs().clone()));
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{ColorAttribute, ColorPalette, SrgbaTuple};
    use crate::{Terminal, TerminalConfiguration, TerminalSize};
    use frankenterm_cell::{CellAttributes, Hyperlink, Intensity, SemanticType};
    use proptest::prelude::*;
    use std::sync::Arc;

    #[derive(Debug)]
    struct ViewTestConfig;

    impl TerminalConfiguration for ViewTestConfig {
        fn scrollback_size(&self) -> usize {
            40
        }

        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    fn legacy_lines(rows: usize, cols: usize, bytes: &[u8]) -> Vec<Line> {
        let mut terminal = Terminal::new(
            TerminalSize {
                rows,
                cols,
                pixel_width: cols * 8,
                pixel_height: rows * 16,
                dpi: 96,
            },
            Arc::new(ViewTestConfig),
            "frankenterm-pagegrid-view",
            "0",
            Box::new(std::io::sink()),
        );
        terminal.advance_bytes(bytes);
        terminal.screen().all_lines()
    }

    /// Stores `line` in a fresh page and checks that its view is the same
    /// legacy row.
    fn assert_round_trip(line: &Line, cols: u16, what: &str) {
        let mut page = Page::new(cols, 1, 1);
        let row = page.grow(1).expect("an empty page has a row");
        assert!(store_line(&mut page, row, line), "{}: store", what);
        page.check_invariants()
            .unwrap_or_else(|err| panic!("{}: {}", what, err));
        let view = line_view(&page, row, line.current_seqno());
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
        assert_eq!(
            (
                view.is_double_width(),
                view.is_double_height_top(),
                view.is_double_height_bottom()
            ),
            (
                line.is_double_width(),
                line.is_double_height_top(),
                line.is_double_height_bottom()
            ),
            "{}: double size",
            what
        );
        assert_eq!(
            view.current_seqno(),
            line.current_seqno(),
            "{}: seqno",
            what
        );
        assert_eq!(view.as_str(), line.as_str(), "{}: text", what);
    }

    /// ft-yccm0.3.3.4: rows the legacy engine builds round-trip through a
    /// page: clustered and vector storage, wide cells at the margin (the
    /// overhang), combining marks, ZWJ, hyperlinks, rich colours, erased
    /// background, double-size rows, semantic zones and DECALN.
    #[test]
    fn legacy_rows_round_trip_through_a_page() {
        let streams: &[(&str, &[u8])] = &[
            (
                "styled_wrapping",
                b"hello \x1b[1;31mred\x1b[0m world, wrapping past the margin\r\n",
            ),
            (
                "wide_overhang",
                "\u{4e2d}\u{6587}\u{5b57}\u{4e2d}\u{6587}\u{5b57}\u{4e2d}\u{6587}\u{5b57}\u{4e2d}\u{6587}x\r\n"
                    .as_bytes(),
            ),
            (
                "clusters",
                "e\u{301} \u{1f468}\u{200d}\u{1f469} \u{2764}\u{fe0f} \u{1f1fa}\u{1f1f8}\r\n"
                    .as_bytes(),
            ),
            (
                "hyperlink",
                b"\x1b]8;;http://example.com/\x1b\\link\x1b]8;;\x1b\\ plain\r\n",
            ),
            (
                "rich_colours",
                b"\x1b[38;2;1;2;3;48;5;21;58;2;4;5;6;4:3mX\x1b[mY\r\n",
            ),
            ("erased_background", b"text\x1b[44m\x1b[2K\x1b[m\r\nmore\r\n"),
            ("vector_storage", b"abcdef\r\x1b[2@\x1b[1P\x1b[3X\r\n"),
            ("double_size", b"\x1b#3top\r\n\x1b#4bottom\r\n\x1b#6wide\r\n"),
            (
                "semantic_zones",
                b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\x1b]133;C\x07\r\nout\r\n",
            ),
            ("decaln", b"\x1b#8"),
        ];
        for &(rows, cols) in &[(5, 9), (8, 20)] {
            for (name, bytes) in streams {
                for (index, line) in legacy_lines(rows, cols, bytes).iter().enumerate() {
                    let what = format!("{} at {}x{} row {}", name, rows, cols, index);
                    assert_round_trip(line, cols as u16, &what);
                }
            }
        }
    }

    fn arb_attrs() -> impl Strategy<Value = CellAttributes> {
        (
            0u8..4,
            0u16..300,
            any::<bool>(),
            0u8..3,
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(intensity, colour, link, semantic, wrapped, protected)| {
                let mut attrs = CellAttributes::default();
                attrs.set_intensity(match intensity {
                    0 => Intensity::Normal,
                    1 => Intensity::Bold,
                    _ => Intensity::Half,
                });
                match colour {
                    0..=255 => {
                        attrs.set_foreground(ColorAttribute::PaletteIndex(colour as u8));
                    }
                    256 => {}
                    _ => {
                        let channel = f32::from(colour as u8) / 255.0;
                        attrs.set_background(ColorAttribute::TrueColorWithDefaultFallback(
                            SrgbaTuple(channel, 0.5, 0.25, 1.0),
                        ));
                    }
                }
                if link {
                    attrs.set_hyperlink(Some(Arc::new(Hyperlink::new("https://view.example/"))));
                }
                attrs.set_semantic_type(match semantic {
                    0 => SemanticType::Output,
                    1 => SemanticType::Input,
                    _ => SemanticType::Prompt,
                });
                attrs.set_wrapped(wrapped);
                attrs.set_protected(protected);
                attrs
            })
    }

    /// A vector-storage row as legacy can hold one: narrow cells, wide
    /// heads, and hidden cells after them with attributes of their own.
    fn arb_cells() -> impl Strategy<Value = Vec<Cell>> {
        let glyph = prop_oneof![
            Just((" ", 1usize)),
            Just(("a", 1)),
            Just(("\u{e9}", 1)),
            Just(("e\u{301}", 1)),
            Just(("\u{4e2d}", 2)),
            Just(("\u{1f468}\u{200d}\u{1f469}", 2)),
        ];
        prop::collection::vec((glyph, arb_attrs(), arb_attrs()), 0..12).prop_map(|cells| {
            let mut out = Vec::new();
            for ((text, width), attrs, hidden) in cells {
                out.push(Cell::new_grapheme_with_width(text, width, attrs));
                if width == 2 {
                    out.push(Cell::blank_with_attrs(hidden));
                }
            }
            out
        })
    }

    proptest! {
        /// ft-yccm0.3.3.4: arbitrary vector-storage rows, with their line
        /// flags, and the same rows compressed to clustered storage,
        /// round-trip through a page.
        #[test]
        fn arbitrary_rows_round_trip(
            cells in arb_cells(),
            seqno in 0usize..1000,
            bidi in 0u8..5,
            size in 0u8..4,
            compress in any::<bool>(),
        ) {
            let mut line = Line::from_cells(cells, seqno);
            let direction = match bidi {
                0 => ParagraphDirectionHint::LeftToRight,
                1 => ParagraphDirectionHint::RightToLeft,
                2 => ParagraphDirectionHint::AutoLeftToRight,
                _ => ParagraphDirectionHint::AutoRightToLeft,
            };
            if bidi > 0 {
                line.set_bidi_info(bidi % 2 == 0, direction, seqno);
            }
            match size {
                1 => line.set_double_width(seqno),
                2 => line.set_double_height_top(seqno),
                3 => line.set_double_height_bottom(seqno),
                _ => {}
            }
            if compress {
                line.compress_for_scrollback();
            }
            prop_assume!(line.len() <= 31);
            assert_round_trip(&line, 30, "arbitrary row");
        }
    }
}
