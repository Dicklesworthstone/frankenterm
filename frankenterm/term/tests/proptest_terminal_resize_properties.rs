//! Property tests for terminal-state resize invariants.
//!
//! These cover the terminal state machine through its public
//! `Terminal::advance_bytes` and `Terminal::resize` entry points. The
//! generated resize sequences assert that cursor mappings remain bounded and
//! that known glyph payloads survive screen-buffer reflow.

use std::sync::Arc;

use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Screen, Terminal, TerminalConfiguration, TerminalSize, UnicodeVersion};
use proptest::prelude::*;

#[derive(Debug)]
struct ResizePropertyConfig {
    scrollback: usize,
    unicode_version: UnicodeVersion,
}

impl TerminalConfiguration for ResizePropertyConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn unicode_version(&self) -> UnicodeVersion {
        self.unicode_version.clone()
    }
}

fn make_term(rows: usize, cols: usize) -> Terminal {
    make_term_with_unicode(rows, cols, UnicodeVersion::new(9))
}

fn make_term_with_unicode(rows: usize, cols: usize, unicode_version: UnicodeVersion) -> Terminal {
    Terminal::new(
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        },
        Arc::new(ResizePropertyConfig {
            scrollback: 512,
            unicode_version,
        }),
        "WezTerm",
        "resize-proptest",
        Box::new(Vec::new()),
    )
}

fn resize_to(term: &mut Terminal, rows: usize, cols: usize, dpi: u32) {
    term.resize(TerminalSize {
        rows,
        cols,
        pixel_width: cols * 8,
        pixel_height: rows * 16,
        dpi,
    });
}

fn assert_cursor_mapping_consistent(term: &Terminal, rows: usize, cols: usize) {
    let cursor = term.cursor_pos();
    assert!(
        cursor.x <= cols,
        "cursor column must remain within or at right edge: x={} cols={}",
        cursor.x,
        cols
    );
    assert!(
        cursor.y >= 0 && (cursor.y as usize) < rows,
        "cursor row must remain in visible bounds: y={} rows={}",
        cursor.y,
        rows
    );

    let screen = term.screen();
    let phys_row = screen.phys_row(cursor.y);
    assert!(
        phys_row < screen.scrollback_rows(),
        "cursor physical row must index existing screen lines: phys_row={} line_count={}",
        phys_row,
        screen.scrollback_rows()
    );

    let stable_row = screen.visible_row_to_stable_row(cursor.y);
    let roundtrip = screen
        .stable_row_to_phys(stable_row)
        .expect("stable row for cursor should map back to a physical row");
    assert_eq!(
        roundtrip, phys_row,
        "cursor stable-row mapping should roundtrip through phys-row"
    );
}

fn unicode_width_modes() -> Vec<UnicodeVersion> {
    let mut unicode_9_ambiguous_wide = UnicodeVersion::new(9);
    unicode_9_ambiguous_wide.ambiguous_are_wide = true;

    let mut unicode_14_ambiguous_wide = UnicodeVersion::new(14);
    unicode_14_ambiguous_wide.ambiguous_are_wide = true;

    vec![
        UnicodeVersion::new(9),
        unicode_9_ambiguous_wide,
        UnicodeVersion::new(14),
        unicode_14_ambiguous_wide,
    ]
}

fn assert_visible_cell_grid_consistent(screen: &Screen, cols: usize, context: &str) {
    screen.for_each_phys_line(|phys_row, line| {
        for cell in line.visible_cells() {
            let width = cell.width();
            assert!(
                width >= 1,
                "visible cell width must be non-zero at phys_row={phys_row} cell={} context={context}",
                cell.cell_index()
            );
            assert!(
                cell.cell_index() <= cols,
                "visible cell starts outside grid at phys_row={phys_row} cell={} cols={cols} text={:?} context={context}",
                cell.cell_index(),
                cell.str()
            );
        }
    });

    assert_wrapped_rows_have_continuation(screen, context);
}

fn assert_wrapped_rows_have_continuation(screen: &Screen, context: &str) {
    let total_rows = screen.scrollback_rows();
    screen.for_each_phys_line(|phys_row, line| {
        if line.last_cell_was_wrapped() {
            assert!(
                phys_row + 1 < total_rows,
                "wrapped line must have a continuation row: phys_row={} total_rows={} context={}",
                phys_row,
                total_rows,
                context
            );
        }
    });
}

fn logical_lines_snapshot(screen: &Screen) -> Vec<String> {
    let mut logical_lines = Vec::new();
    let mut current = String::new();

    screen.for_each_phys_line(|_, line| {
        current.push_str(&line.as_str());
        if line.last_cell_was_wrapped() {
            return;
        }
        logical_lines.push(std::mem::take(&mut current));
    });

    if !current.is_empty() {
        logical_lines.push(current);
    }

    while logical_lines
        .last()
        .map(|line| line.is_empty())
        .unwrap_or(false)
    {
        logical_lines.pop();
    }

    logical_lines
}

fn arb_known_glyph() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("A"),
        Just("z"),
        Just("0"),
        Just("界"),
        Just("🧪"),
        Just("👩‍💻"),
        Just("👨‍👩‍👧‍👦"),
        Just("e\u{0301}"),
        Just("🇺🇸"),
    ]
}

fn arb_wrapping_glyph() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("A"),
        Just("z"),
        Just("7"),
        Just("·"),
        Just("Ω"),
        Just("─"),
        Just("界"),
        Just("🧪"),
        Just("👩‍💻"),
        Just("e\u{0301}"),
        Just("🇺🇸"),
    ]
}

#[derive(Debug, Clone)]
enum GridOp {
    Print(&'static str),
    CarriageReturn,
    LineFeed,
    CursorUp(usize),
    CursorDown(usize),
    CursorForward(usize),
    CursorBack(usize),
    CursorPosition { row: usize, col: usize },
}

fn arb_grid_op() -> impl Strategy<Value = GridOp> {
    prop_oneof![
        arb_wrapping_glyph().prop_map(GridOp::Print),
        Just(GridOp::CarriageReturn),
        Just(GridOp::LineFeed),
        (1usize..=12).prop_map(GridOp::CursorUp),
        (1usize..=12).prop_map(GridOp::CursorDown),
        (1usize..=24).prop_map(GridOp::CursorForward),
        (1usize..=24).prop_map(GridOp::CursorBack),
        (1usize..=24, 1usize..=80).prop_map(|(row, col)| GridOp::CursorPosition { row, col }),
    ]
}

fn apply_grid_op(term: &mut Terminal, op: &GridOp) {
    match op {
        GridOp::Print(glyph) => term.advance_bytes(glyph.as_bytes()),
        GridOp::CarriageReturn => term.advance_bytes(b"\r"),
        GridOp::LineFeed => term.advance_bytes(b"\n"),
        GridOp::CursorUp(count) => term.advance_bytes(format!("\x1b[{count}A").as_bytes()),
        GridOp::CursorDown(count) => term.advance_bytes(format!("\x1b[{count}B").as_bytes()),
        GridOp::CursorForward(count) => term.advance_bytes(format!("\x1b[{count}C").as_bytes()),
        GridOp::CursorBack(count) => term.advance_bytes(format!("\x1b[{count}D").as_bytes()),
        GridOp::CursorPosition { row, col } => {
            term.advance_bytes(format!("\x1b[{row};{col}H").as_bytes());
        }
    }
}

fn arb_resize_step() -> impl Strategy<Value = (usize, usize, u32)> {
    (
        2usize..=18,
        4usize..=36,
        prop_oneof![Just(72u32), Just(96), Just(144), Just(192)],
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn resize_churn_keeps_cursor_mapping_in_bounds(
        glyphs in proptest::collection::vec(arb_known_glyph(), 1..40),
        resize_steps in proptest::collection::vec(arb_resize_step(), 1..32),
        initial_rows in 2usize..=18,
        initial_cols in 4usize..=36,
    ) {
        let mut term = make_term(initial_rows, initial_cols);
        for (idx, glyph) in glyphs.iter().enumerate() {
            term.advance_bytes(format!("{idx:02}:{glyph}|{glyph}\r\n").as_bytes());
        }
        assert_cursor_mapping_consistent(&term, initial_rows, initial_cols);

        for (rows, cols, dpi) in resize_steps {
            resize_to(&mut term, rows, cols, dpi);
            assert_cursor_mapping_consistent(&term, rows, cols);
        }
    }

    #[test]
    fn resize_churn_preserves_known_glyph_payloads(
        glyphs in proptest::collection::vec(arb_known_glyph(), 1..40),
        resize_steps in proptest::collection::vec(arb_resize_step(), 1..32),
        initial_rows in 2usize..=18,
        initial_cols in 4usize..=36,
    ) {
        let mut term = make_term(initial_rows, initial_cols);
        let mut expected_fragments = Vec::new();
        for (idx, glyph) in glyphs.iter().enumerate() {
            let fragment = format!("{idx:02}:{glyph}|{glyph}");
            term.advance_bytes(format!("{fragment}\r\n").as_bytes());
            expected_fragments.push(fragment);
        }

        let baseline = logical_lines_snapshot(term.screen()).concat();
        for fragment in &expected_fragments {
            prop_assert!(
                baseline.contains(fragment),
                "baseline should contain known glyph fragment {fragment:?}; baseline={baseline:?}"
            );
        }

        for (rows, cols, dpi) in resize_steps {
            resize_to(&mut term, rows, cols, dpi);
            assert_cursor_mapping_consistent(&term, rows, cols);

            let resized = logical_lines_snapshot(term.screen()).concat();
            prop_assert_eq!(
                &resized,
                &baseline,
                "logical glyph payload changed after resize to {}x{}@{}",
                rows,
                cols,
                dpi
            );
            for fragment in &expected_fragments {
                prop_assert!(
                    resized.contains(fragment),
                    "resized screen should preserve known glyph fragment {fragment:?}; resized={resized:?}"
                );
            }
        }
    }

    #[test]
    fn cell_grid_ops_keep_cursor_and_cells_bounded_for_all_unicode_width_modes(
        ops in proptest::collection::vec(arb_grid_op(), 1..96),
        rows in 2usize..=14,
        cols in 2usize..=24,
    ) {
        for unicode_version in unicode_width_modes() {
            let mut term = make_term_with_unicode(rows, cols, unicode_version.clone());
            assert_cursor_mapping_consistent(&term, rows, cols);
            assert_wrapped_rows_have_continuation(
                term.screen(),
                &format!("initial unicode={unicode_version:?}"),
            );

            for (step, op) in ops.iter().enumerate() {
                apply_grid_op(&mut term, op);
                assert_cursor_mapping_consistent(&term, rows, cols);
                assert_wrapped_rows_have_continuation(
                    term.screen(),
                    &format!("step={step} op={op:?} unicode={unicode_version:?}"),
                );
            }
        }
    }

    #[test]
    fn unicode_runs_wrap_without_losing_logical_text_for_all_width_modes(
        glyphs in proptest::collection::vec(arb_wrapping_glyph(), 1..128),
        rows in 3usize..=14,
        cols in 2usize..=20,
    ) {
        let expected = glyphs.concat();

        for unicode_version in unicode_width_modes() {
            let mut term = make_term_with_unicode(rows, cols, unicode_version.clone());
            for (idx, glyph) in glyphs.iter().enumerate() {
                term.advance_bytes(glyph.as_bytes());
                assert_cursor_mapping_consistent(&term, rows, cols);
                assert_visible_cell_grid_consistent(
                    term.screen(),
                    cols,
                    &format!("glyph_idx={idx} glyph={glyph:?} unicode={unicode_version:?}"),
                );
            }

            let logical = logical_lines_snapshot(term.screen()).concat();
            prop_assert_eq!(
                &logical,
                &expected,
                "wrapped logical text changed under unicode width mode {:?}",
                unicode_version
            );
        }
    }
}
