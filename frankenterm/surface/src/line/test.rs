#![cfg(test)]

use super::*;
use crate::hyperlink::{Hyperlink, Rule};
use crate::line::clusterline::ClusteredLine;
use crate::line::storage::CellStorage;
use crate::SEQ_ZERO;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use frankenterm_cell::{Cell, CellAttributes};
use k9::assert_equal as assert_eq;

/// There are 4 double-wide graphemes that occupy 2 cells each.
/// When we join the lines, we must preserve the invisible blank
/// that is part of the grapheme otherwise our metrics will be
/// wrong.
/// <https://github.com/wezterm/wezterm/issues/2568>
#[test]
fn append_line() {
    let mut line1: Line = "0123456789".into();
    let line2: Line = "グループaa".into();

    line1.append_line(line2, SEQ_ZERO);

    assert_eq!(line1.len(), 20);
}

#[test]
fn hyperlinks() {
    let text = "❤ 😍🤢 http://example.com \u{1f468}\u{1f3fe}\u{200d}\u{1f9b0} http://example.com";

    let rules = hyperlink_rules();

    let hyperlink = Arc::new(Hyperlink::new_implicit("http://example.com"));
    let hyperlink_attr = CellAttributes::default()
        .set_hyperlink(Some(hyperlink.clone()))
        .clone();

    let mut line: Line = text.into();
    line.scan_and_create_hyperlinks(&rules);
    assert!(line.has_hyperlink());
    assert_eq!(
        line.coerce_vec_storage().to_vec(),
        vec![
            Cell::new_grapheme("❤", CellAttributes::default(), None),
            Cell::new(' ', CellAttributes::default()), // double width spacer
            Cell::new_grapheme("😍", CellAttributes::default(), None),
            Cell::new(' ', CellAttributes::default()), // double width spacer
            Cell::new_grapheme("🤢", CellAttributes::default(), None),
            Cell::new(' ', CellAttributes::default()), // double width spacer
            Cell::new(' ', CellAttributes::default()),
            Cell::new('h', hyperlink_attr.clone()),
            Cell::new('t', hyperlink_attr.clone()),
            Cell::new('t', hyperlink_attr.clone()),
            Cell::new('p', hyperlink_attr.clone()),
            Cell::new(':', hyperlink_attr.clone()),
            Cell::new('/', hyperlink_attr.clone()),
            Cell::new('/', hyperlink_attr.clone()),
            Cell::new('e', hyperlink_attr.clone()),
            Cell::new('x', hyperlink_attr.clone()),
            Cell::new('a', hyperlink_attr.clone()),
            Cell::new('m', hyperlink_attr.clone()),
            Cell::new('p', hyperlink_attr.clone()),
            Cell::new('l', hyperlink_attr.clone()),
            Cell::new('e', hyperlink_attr.clone()),
            Cell::new('.', hyperlink_attr.clone()),
            Cell::new('c', hyperlink_attr.clone()),
            Cell::new('o', hyperlink_attr.clone()),
            Cell::new('m', hyperlink_attr.clone()),
            Cell::new(' ', CellAttributes::default()),
            Cell::new_grapheme(
                // man: dark skin tone, red hair ZWJ emoji grapheme
                "\u{1f468}\u{1f3fe}\u{200d}\u{1f9b0}",
                CellAttributes::default(),
                None,
            ),
            Cell::new(' ', CellAttributes::default()), // double width spacer
            Cell::new(' ', CellAttributes::default()),
            Cell::new('h', hyperlink_attr.clone()),
            Cell::new('t', hyperlink_attr.clone()),
            Cell::new('t', hyperlink_attr.clone()),
            Cell::new('p', hyperlink_attr.clone()),
            Cell::new(':', hyperlink_attr.clone()),
            Cell::new('/', hyperlink_attr.clone()),
            Cell::new('/', hyperlink_attr.clone()),
            Cell::new('e', hyperlink_attr.clone()),
            Cell::new('x', hyperlink_attr.clone()),
            Cell::new('a', hyperlink_attr.clone()),
            Cell::new('m', hyperlink_attr.clone()),
            Cell::new('p', hyperlink_attr.clone()),
            Cell::new('l', hyperlink_attr.clone()),
            Cell::new('e', hyperlink_attr.clone()),
            Cell::new('.', hyperlink_attr.clone()),
            Cell::new('c', hyperlink_attr.clone()),
            Cell::new('o', hyperlink_attr.clone()),
            Cell::new('m', hyperlink_attr.clone()),
        ]
    );
}

fn hyperlink_rules() -> Vec<Rule> {
    vec![
        Rule::new(r"\b\w+://(?:[\w.-]+)\.[a-z]{2,15}\S*\b", "$0").unwrap(),
        Rule::new(r"\b\w+@[\w-]+(\.[\w-]+)+\b", "mailto:$0").unwrap(),
    ]
}

fn linked_uri(cell: &Cell) -> Option<&str> {
    cell.attrs().hyperlink().map(|link| link.uri())
}

#[test]
fn implicit_hyperlink_after_wide_cell_uses_visible_cell_map() {
    let mut line: Line = "中http://example.com".into();

    line.scan_and_create_hyperlinks(&hyperlink_rules());

    let cells = line.coerce_vec_storage().to_vec();
    assert_eq!(cells[0].str(), "中");
    assert_eq!(cells[1].str(), " ");
    assert_eq!(linked_uri(&cells[0]), None);
    assert_eq!(linked_uri(&cells[1]), None);
    assert_eq!(linked_uri(&cells[2]), Some("http://example.com"));
    assert_eq!(
        linked_uri(cells.last().unwrap()),
        Some("http://example.com")
    );
}

#[test]
fn implicit_hyperlink_with_wide_cell_inside_match_skips_spacer_cell() {
    let wide_link = Arc::new(Hyperlink::new_implicit("https://wide.example/"));
    let wide_link_attr = CellAttributes::default()
        .set_hyperlink(Some(wide_link))
        .clone();
    let mut line = Line::from_cells(
        vec![
            Cell::new('x', CellAttributes::default()),
            Cell::new_grapheme("❤", CellAttributes::default(), None),
            Cell::new(' ', CellAttributes::default()),
            Cell::new('y', CellAttributes::default()),
        ],
        SEQ_ZERO,
    );
    let rules = vec![Rule::new(r"\S+", "https://wide.example/").unwrap()];

    line.scan_and_create_hyperlinks(&rules);

    let cells = line.coerce_vec_storage().to_vec();
    assert_eq!(cells[0].attrs(), &wide_link_attr);
    assert_eq!(cells[1].attrs(), &wide_link_attr);
    assert_eq!(linked_uri(&cells[2]), None);
    assert_eq!(cells[3].attrs(), &wide_link_attr);
}

#[test]
fn implicit_hyperlink_scan_splits_at_zero_width_cells() {
    let zero_width = Cell::new_grapheme(
        "\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}\u{0301}",
        CellAttributes::default(),
        None,
    );
    assert_eq!(zero_width.width(), 0);
    let cells: Vec<Cell> = vec![
        Cell::new('a', CellAttributes::default()),
        Cell::new('b', CellAttributes::default()),
        zero_width,
        Cell::new('c', CellAttributes::default()),
        Cell::new('d', CellAttributes::default()),
    ];
    let mut line = Line::from_cells(cells, SEQ_ZERO);
    let rules = vec![Rule::new(r"ab.*cd", "https://zero-width.example/").unwrap()];

    line.scan_and_create_hyperlinks(&rules);

    assert!(!line.has_hyperlink());
    assert!(line
        .coerce_vec_storage()
        .iter()
        .all(|cell| cell.attrs().hyperlink().is_none()));
}

#[test]
fn implicit_hyperlink_scan_excludes_explicit_link_spans() {
    let explicit_link = Arc::new(Hyperlink::new("https://explicit.example/"));
    let explicit_attr = CellAttributes::default()
        .set_hyperlink(Some(explicit_link))
        .clone();
    let text = "http://blocked.test http://ok.test";
    let cells: Vec<Cell> = text
        .chars()
        .enumerate()
        .map(|(idx, c)| {
            let attrs = if idx == "http://blocked".len() {
                explicit_attr.clone()
            } else {
                CellAttributes::default()
            };
            Cell::new(c, attrs)
        })
        .collect();
    let explicit_idx = "http://blocked".len();
    let ok_start = "http://blocked.test ".len();
    let mut line = Line::from_cells(cells, SEQ_ZERO);

    line.scan_and_create_hyperlinks(&hyperlink_rules());

    let cells = line.coerce_vec_storage().to_vec();
    assert!(line.has_hyperlink());
    assert_eq!(linked_uri(&cells[0]), None);
    assert_eq!(
        linked_uri(&cells[explicit_idx]),
        Some("https://explicit.example/")
    );
    assert_eq!(linked_uri(&cells[explicit_idx + 1]), None);
    assert_eq!(linked_uri(&cells[ok_start]), Some("http://ok.test"));
    assert_eq!(linked_uri(cells.last().unwrap()), Some("http://ok.test"));
}

#[test]
fn double_click_range_bounds() {
    let line: Line = "hello".into();
    let r = line.compute_double_click_range(200, |_| true);
    assert_eq!(r, DoubleClickRange::Range(200..200));
}

#[test]
fn cluster_representation_basic() {
    let line: Line = "hello".into();
    let mut compressed = line.clone();
    compressed.compress_for_scrollback();
    k9::snapshot!(
        &compressed.cells,
        r#"
C(
    ClusteredLine {
        text_bytes: 5,
        cell_len: 5,
        cluster_count: 1,
        text: "[REDACTED]",
    },
)
"#
    );
    compressed.coerce_vec_storage();
    assert_eq!(line, compressed);
}

#[test]
fn cluster_representation_double_width() {
    let line: Line = "❤ 😍🤢he❤ 😍🤢llo❤ 😍🤢".into();
    let expected_visible_cells = line
        .visible_cells()
        .map(|cell| cell.as_cell())
        .collect::<Vec<_>>();
    let expected_cell_geometry = line
        .visible_cells()
        .map(|cell| (cell.cell_index(), cell.width()))
        .collect::<Vec<_>>();
    let mut compressed = line.clone();
    compressed.compress_for_scrollback();
    assert!(matches!(&compressed.cells, CellStorage::C(_)));
    assert_eq!(
        compressed
            .visible_cells()
            .map(|cell| cell.as_cell())
            .collect::<Vec<_>>(),
        expected_visible_cells
    );
    assert_eq!(
        compressed
            .visible_cells()
            .map(|cell| (cell.cell_index(), cell.width()))
            .collect::<Vec<_>>(),
        expected_cell_geometry,
        "cluster compression must preserve the exact double-width bit positions"
    );
    compressed.coerce_vec_storage();
    assert_eq!(line, compressed);
}

#[test]
fn cluster_representation_empty() {
    let line = Line::from_cells(vec![], SEQ_ZERO);

    let mut compressed = line.clone();
    compressed.compress_for_scrollback();
    k9::snapshot!(
        &compressed.cells,
        r#"
C(
    ClusteredLine {
        text_bytes: 0,
        cell_len: 0,
        cluster_count: 0,
        text: "[REDACTED]",
    },
)
"#
    );
    compressed.coerce_vec_storage();
    assert_eq!(line, compressed);
}

#[test]
fn cluster_wrap_last() {
    let mut line: Line = "hello".into();
    line.compress_for_scrollback();
    line.set_last_cell_was_wrapped(true, 1);
    k9::snapshot!(
        &line,
        r#"
Line {
    cells: C(
        ClusteredLine {
            text_bytes: 5,
            cell_len: 5,
            cluster_count: 2,
            text: "[REDACTED]",
        },
    ),
    zones: [],
    seqno: 1,
    bits: LineBits(
        0x0,
    ),
}
"#
    );
    let cells = line
        .visible_cells()
        .map(|cell| cell.as_cell())
        .collect::<Vec<_>>();
    assert_eq!(cells.len(), 5);
    assert!(cells[..4].iter().all(|cell| !cell.attrs().wrapped()));
    assert!(cells[4].attrs().wrapped());
}

fn bold() -> CellAttributes {
    use frankenterm_cell::Intensity;
    let mut attr = CellAttributes::default();
    attr.set_intensity(Intensity::Bold);
    attr
}

#[test]
fn cluster_representation_attributes() {
    let line = Line::from_cells(
        vec![
            Cell::new_grapheme("a", CellAttributes::default(), None),
            Cell::new_grapheme("b", bold(), None),
            Cell::new_grapheme("c", CellAttributes::default(), None),
            Cell::new_grapheme("d", bold(), None),
        ],
        SEQ_ZERO,
    );

    let mut compressed = line.clone();
    compressed.compress_for_scrollback();
    k9::snapshot!(
        &compressed.cells,
        r#"
C(
    ClusteredLine {
        text_bytes: 4,
        cell_len: 4,
        cluster_count: 4,
        text: "[REDACTED]",
    },
)
"#
    );
    compressed.coerce_vec_storage();
    assert_eq!(line, compressed);
}

#[test]
fn cluster_append() {
    let mut cl = ClusteredLine::new();
    cl.append(Cell::new_grapheme("h", CellAttributes::default(), None));
    cl.append(Cell::new_grapheme("e", CellAttributes::default(), None));
    cl.append(Cell::new_grapheme("l", bold(), None));
    cl.append(Cell::new_grapheme("l", CellAttributes::default(), None));
    cl.append(Cell::new_grapheme("o", CellAttributes::default(), None));
    k9::snapshot!(
        &cl,
        r#"
ClusteredLine {
    text_bytes: 5,
    cell_len: 5,
    cluster_count: 3,
    text: "[REDACTED]",
}
"#
    );
    assert_eq!(
        cl.to_cell_vec(),
        vec![
            Cell::new_grapheme("h", CellAttributes::default(), None),
            Cell::new_grapheme("e", CellAttributes::default(), None),
            Cell::new_grapheme("l", bold(), None),
            Cell::new_grapheme("l", CellAttributes::default(), None),
            Cell::new_grapheme("o", CellAttributes::default(), None),
        ],
        "redacted Debug output must not replace cluster behavior coverage"
    );
}

#[test]
fn cluster_line_new() {
    let mut line = Line::new(1);
    line.set_cell(
        0,
        Cell::new_grapheme("h", CellAttributes::default(), None),
        1,
    );
    line.set_cell(
        1,
        Cell::new_grapheme("e", CellAttributes::default(), None),
        2,
    );
    line.set_cell(2, Cell::new_grapheme("l", bold(), None), 3);
    line.set_cell(
        3,
        Cell::new_grapheme("l", CellAttributes::default(), None),
        4,
    );
    line.set_cell(
        4,
        Cell::new_grapheme("o", CellAttributes::default(), None),
        5,
    );
    k9::snapshot!(
        &line,
        r#"
Line {
    cells: C(
        ClusteredLine {
            text_bytes: 5,
            cell_len: 5,
            cluster_count: 3,
            text: "[REDACTED]",
        },
    ),
    zones: [],
    seqno: 5,
    bits: LineBits(
        0x0,
    ),
}
"#
    );
    assert_eq!(
        line.visible_cells()
            .map(|cell| cell.as_cell())
            .collect::<Vec<_>>(),
        vec![
            Cell::new_grapheme("h", CellAttributes::default(), None),
            Cell::new_grapheme("e", CellAttributes::default(), None),
            Cell::new_grapheme("l", bold(), None),
            Cell::new_grapheme("l", CellAttributes::default(), None),
            Cell::new_grapheme("o", CellAttributes::default(), None),
        ],
        "redacted Debug output must not replace line cell coverage"
    );
}

#[test]
fn fill_range_past_end_of_empty_line_does_not_panic() {
    // Regression for the mux-server abort in #85 (index-out-of-bounds with
    // "len 0 / index 0"): filling a range that starts beyond the
    // materialized width performed the wide-grapheme look-back on storage
    // that had not been grown yet.
    let mut line = Line::from_text("", &CellAttributes::default(), SEQ_ZERO, None);
    line.fill_range(
        1..2,
        &Cell::new_grapheme("x", CellAttributes::default(), None),
        SEQ_ZERO,
    );
    assert_eq!(line.as_str(), " x");
}

#[test]
fn fill_range_after_wide_grapheme_truncated_to_head_does_not_panic() {
    // resize() truncates storage at an arbitrary column, so a double-width
    // grapheme's head can be the final stored cell.  The look-back then
    // blanked one cell past the end of storage.
    let mut line = Line::from_text("a\u{4f60}", &CellAttributes::default(), SEQ_ZERO, None);
    assert_eq!(line.len(), 3);
    line.resize(2, SEQ_ZERO);
    line.fill_range(
        2..3,
        &Cell::new_grapheme("x", CellAttributes::default(), None),
        SEQ_ZERO,
    );
    // The truncated wide head is invalidated to a blank rather than left
    // half-rendered.
    assert_eq!(line.as_str(), "a x");
}

#[test]
fn fill_range_blank_erases_wide_grapheme_truncated_to_head() {
    let mut line = Line::from_text("a\u{4f60}", &CellAttributes::default(), SEQ_ZERO, None);
    assert_eq!(line.len(), 3);
    line.resize(2, SEQ_ZERO);

    line.fill_range(2..3, &Cell::blank(), SEQ_ZERO);

    // Column 2 is the implicit placeholder owned by the wide grapheme at
    // column 1.  Erasing that placeholder must not leave an orphaned head.
    assert_eq!(line.as_str(), "a");
}
