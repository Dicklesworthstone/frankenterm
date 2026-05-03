//! Property tests for terminal-state resize invariants.
//!
//! These cover the terminal state machine through its public
//! `Terminal::advance_bytes` and `Terminal::resize` entry points. The
//! generated resize sequences assert that cursor mappings remain bounded and
//! that known glyph payloads survive screen-buffer reflow.

use std::sync::Arc;

use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Screen, Terminal, TerminalConfiguration, TerminalSize};
use proptest::prelude::*;

#[derive(Debug)]
struct ResizePropertyConfig {
    scrollback: usize,
}

impl TerminalConfiguration for ResizePropertyConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

fn make_term(rows: usize, cols: usize) -> Terminal {
    Terminal::new(
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        },
        Arc::new(ResizePropertyConfig { scrollback: 512 }),
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
}
