#![no_main]
//! Fuzz harness for `frankenterm_term::Terminal::advance_bytes` (ft-btg0h).
//!
//! This drives the full terminal state machine, not just the lexer layer:
//! raw bytes -> escape parser -> performer -> screen/cursor mutation.
//! The harness keeps iteration cost bounded, feeds the same payload both
//! single-shot and in fixed chunks, and asserts cheap invariants that should
//! hold for every terminal state after parsing hostile input.

use std::sync::Arc;

use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};
use libfuzzer_sys::fuzz_target;

const ROWS: usize = 24;
const COLS: usize = 80;
const SCROLLBACK: usize = 64;
const DPI: u32 = 96;
const PIXEL_WIDTH: usize = COLS * 8;
const PIXEL_HEIGHT: usize = ROWS * 16;
const MAX_INPUT_BYTES: usize = 256 * 1024;
const CHUNK_BYTES: usize = 17;

#[derive(Debug)]
struct FuzzConfig;

impl TerminalConfiguration for FuzzConfig {
    fn scrollback_size(&self) -> usize {
        SCROLLBACK
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

fn make_term() -> Terminal {
    Terminal::new(
        TerminalSize {
            rows: ROWS,
            cols: COLS,
            pixel_width: PIXEL_WIDTH,
            pixel_height: PIXEL_HEIGHT,
            dpi: DPI,
        },
        Arc::new(FuzzConfig),
        "WezTerm",
        "fuzz",
        Box::new(Vec::new()),
    )
}

fn assert_invariants(term: &Terminal, input_len: usize) {
    let cursor = term.cursor_pos();
    assert!(
        cursor.x <= COLS,
        "cursor column out of bounds: x={} cols={} input_len={input_len}",
        cursor.x,
        COLS,
    );
    assert!(
        cursor.y >= 0 && (cursor.y as usize) < ROWS,
        "cursor row out of bounds: y={} rows={} input_len={input_len}",
        cursor.y,
        ROWS,
    );

    let screen = term.screen();
    assert!(
        screen.scrollback_rows() <= ROWS + SCROLLBACK,
        "screen retained {} rows with {} visible + {} scrollback (input_len={input_len})",
        screen.scrollback_rows(),
        ROWS,
        SCROLLBACK,
    );

    let phys_row = screen.phys_row(cursor.y);
    assert!(
        phys_row < screen.scrollback_rows(),
        "cursor phys_row {} out of screen bounds {} (input_len={input_len})",
        phys_row,
        screen.scrollback_rows(),
    );
}

fn feed_chunked(term: &mut Terminal, data: &[u8]) {
    for chunk in data.chunks(CHUNK_BYTES) {
        term.advance_bytes(chunk);
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }

    let mut whole = make_term();
    whole.advance_bytes(data);
    assert_invariants(&whole, data.len());

    let mut chunked = make_term();
    feed_chunked(&mut chunked, data);
    assert_invariants(&chunked, data.len());

    let whole_cursor = whole.cursor_pos();
    let chunked_cursor = chunked.cursor_pos();
    assert_eq!(
        (whole_cursor.x, whole_cursor.y),
        (chunked_cursor.x, chunked_cursor.y),
        "cursor diverged between single-shot and chunked parsing for input_len={}",
        data.len(),
    );
    assert_eq!(
        whole.screen().scrollback_rows(),
        chunked.screen().scrollback_rows(),
        "scrollback rows diverged between single-shot and chunked parsing for input_len={}",
        data.len(),
    );
});
