//! Structure-aware fuzz harness for `Terminal::advance_bytes`.
//!
//! `advance_bytes` is the terminal emulator's main ingest point — pty
//! output bytes come in as an opaque stream and drive the parser ->
//! performer -> screen-state pipeline. The parser handles CSI / OSC /
//! DCS / APC / PM escapes, 7-bit and 8-bit C1 forms, UTF-8, and raw
//! binary, each of which has its own byte-level grammar. A byte-stream
//! that crashes the terminal on any real tty output is a reliability
//! and security hazard — a malicious program could kill the terminal
//! just by `cat`ing a crafted payload.
//!
//! Instead of running `cargo-fuzz` (needs nightly + extra plumbing),
//! this harness uses a structure-aware proptest generator: the strategy
//! composes realistic hostile sequences from the grammar fragments the
//! emulator actually parses (CSI, OSC, SOS/PM/APC, C0/C1 controls,
//! valid and invalid UTF-8, ANSI SGR, cursor-motion), plus pure random
//! bytes to catch the seams between categories. Each generated payload
//! is driven through `advance_bytes` against a fresh Terminal, and
//! several post-conditions the emulator must always uphold are
//! asserted:
//!
//!   (1) no panic, no unwrap cascade — if the call returns, the
//!       terminal is still usable;
//!   (2) cursor column stays in `0..=cols` (trailing-edge cursor is
//!       legal after a grapheme write);
//!   (3) cursor row stays in `0..rows` (bounded to the visible area);
//!   (4) the cursor's physical row indexes an existing screen line
//!       (the stable-row ↔ phys-row ↔ visible-row mapping roundtrips);
//!   (5) chunking is invariant: for the same input, delivering the
//!       bytes as one advance_bytes call or split at arbitrary
//!       boundaries yields the same cursor position + the same number
//!       of screen lines. This catches parser-state corruption across
//!       chunk boundaries (a classic source of bugs for OSC/DCS since
//!       the terminator can straddle).

use std::sync::Arc;

use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};
use proptest::prelude::*;

#[derive(Debug)]
struct FuzzConfig {
    scrollback: usize,
}

impl TerminalConfiguration for FuzzConfig {
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
        Arc::new(FuzzConfig { scrollback: 256 }),
        "WezTerm",
        "fuzz",
        Box::new(Vec::new()),
    )
}

fn assert_terminal_invariants(term: &Terminal, rows: usize, cols: usize, input: &[u8]) {
    let cursor = term.cursor_pos();

    // (2) cursor column in [0, cols].
    assert!(
        cursor.x <= cols,
        "cursor column out of bounds: x={} cols={} input_len={}",
        cursor.x,
        cols,
        input.len(),
    );

    // (3) cursor row in [0, rows).
    assert!(
        cursor.y >= 0 && (cursor.y as usize) < rows,
        "cursor row out of bounds: y={} rows={} input_len={}",
        cursor.y,
        rows,
        input.len(),
    );

    // (4) cursor's physical-row ↔ stable-row ↔ visible-row roundtrip.
    let screen = term.screen();
    let phys_row = screen.phys_row(cursor.y);
    assert!(
        phys_row < screen.scrollback_rows(),
        "cursor phys_row {phys_row} >= scrollback_rows {} input_len={}",
        screen.scrollback_rows(),
        input.len(),
    );
    let stable_row = screen.visible_row_to_stable_row(cursor.y);
    let back_to_phys = screen
        .stable_row_to_phys(stable_row)
        .expect("cursor stable row must map back to a phys row");
    assert_eq!(
        back_to_phys,
        phys_row,
        "cursor phys↔stable mapping must roundtrip (input_len={})",
        input.len(),
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Grammar fragments — structure-aware hostile inputs.
// ─────────────────────────────────────────────────────────────────────────

/// Printable ASCII bytes plus a handful of C0 controls that change cursor
/// state (BS, HT, LF, VT, FF, CR, SO, SI).
fn arb_c0_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        (0x20u8..=0x7E), // printable ASCII
        Just(0x07u8),    // BEL
        Just(0x08u8),    // BS
        Just(0x09u8),    // HT
        Just(0x0Au8),    // LF
        Just(0x0Bu8),    // VT
        Just(0x0Cu8),    // FF
        Just(0x0Du8),    // CR
        Just(0x0Eu8),    // SO
        Just(0x0Fu8),    // SI
    ]
}

/// Bytes that are likely to appear inside a CSI parameter (digits,
/// separators) or as CSI intermediate/final bytes.
fn arb_csi_body() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // Numeric parameters separated by ';'
        proptest::collection::vec(prop_oneof![(0x30u8..=0x39), Just(0x3Bu8)], 0..12,),
        // Private-mode marker '?' + params
        proptest::collection::vec(
            prop_oneof![(0x30u8..=0x39), Just(0x3Bu8), Just(0x3Fu8)],
            0..12
        ),
        // Empty params (final byte only)
        Just(Vec::new()),
    ]
}

fn arb_csi_final_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        Just(b'A'),
        Just(b'B'),
        Just(b'C'),
        Just(b'D'), // cursor motion
        Just(b'H'),
        Just(b'f'),
        Just(b'G'), // cursor position
        Just(b'J'),
        Just(b'K'), // erase
        Just(b'h'),
        Just(b'l'), // mode set/reset
        Just(b'm'), // SGR
        Just(b'r'), // scrolling region
        Just(b's'),
        Just(b'u'), // save/restore cursor
        Just(b'n'),
        Just(b'c'), // reports
        // Uncommon finals — force the parser onto edge paths
        Just(b'@'),
        Just(b'q'),
        Just(b'`'),
        Just(b'~'),
    ]
}

fn arb_csi_sequence() -> impl Strategy<Value = Vec<u8>> {
    (arb_csi_body(), arb_csi_final_byte()).prop_map(|(body, final_byte)| {
        let mut v = vec![0x1B, b'['];
        v.extend(body);
        v.push(final_byte);
        v
    })
}

/// OSC sequences: ESC ] params ST (either BEL or ESC \).
fn arb_osc_sequence() -> impl Strategy<Value = Vec<u8>> {
    (
        (0u32..=2000),
        proptest::collection::vec(prop_oneof![(0x20u8..=0x7E), Just(0x07u8)], 0..32),
        prop_oneof![Just(vec![0x07u8]), Just(vec![0x1B, b'\\'])],
    )
        .prop_map(|(ps, pt, st)| {
            let mut v = vec![0x1B, b']'];
            v.extend(format!("{ps}").into_bytes());
            v.push(b';');
            v.extend(pt);
            v.extend(st);
            v
        })
}

/// Pure random noise — catches the seams where no grammar matches.
fn arb_noise() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..256)
}

/// Valid UTF-8 text including multibyte and combining sequences.
fn arb_utf8_text() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<char>(), 0..64)
        .prop_map(|chars| chars.into_iter().collect::<String>().into_bytes())
}

/// Deliberately malformed UTF-8 — lone continuation bytes, truncated
/// sequences, and overlong starts.
fn arb_bad_utf8() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // Lone continuation byte
        proptest::collection::vec((0x80u8..=0xBF), 1..8),
        // 2-byte start with no continuation
        proptest::collection::vec((0xC2u8..=0xDF), 1..4),
        // 3-byte start truncated
        Just(vec![0xE2u8, 0x82]),
        // 4-byte start truncated
        Just(vec![0xF0u8, 0x9F, 0x98]),
        // BOM + junk
        Just(vec![0xEFu8, 0xBB, 0xBF, 0xC0, 0xC0]),
    ]
}

/// A single fuzz chunk: one of the grammar fragments or raw noise.
fn arb_fuzz_chunk() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        arb_c0_byte().prop_map(|b| vec![b]),
        arb_csi_sequence(),
        arb_osc_sequence(),
        arb_noise(),
        arb_utf8_text(),
        arb_bad_utf8(),
    ]
}

/// A sequence of chunks concatenated into one payload.
fn arb_fuzz_payload() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(arb_fuzz_chunk(), 1..16).prop_map(|chunks| {
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(chunk);
        }
        out
    })
}

// ─────────────────────────────────────────────────────────────────────────
// Proptest harness
// ─────────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Invariant (1) + (2)/(3)/(4): no panic; cursor/screen bounds hold
    /// after every advance_bytes call for every grammar-shaped payload.
    #[test]
    fn advance_bytes_never_panics_and_keeps_cursor_in_bounds(
        payload in arb_fuzz_payload(),
        rows in 2usize..=32,
        cols in 2usize..=120,
    ) {
        let mut term = make_term(rows, cols);
        term.advance_bytes(&payload);
        assert_terminal_invariants(&term, rows, cols, &payload);
    }

    /// Invariant (5): chunking is invariant — feeding the same payload
    /// as one advance call or broken into arbitrary-size chunks yields
    /// the same cursor position (and the same scrollback line count).
    /// Catches parser-state corruption across chunk boundaries (OSC/DCS
    /// terminators that straddle a chunk split are a classic culprit).
    #[test]
    fn chunked_advance_bytes_matches_single_shot(
        payload in arb_fuzz_payload(),
        chunk_sizes in proptest::collection::vec(1usize..=32, 0..16),
        rows in 4usize..=16,
        cols in 8usize..=80,
    ) {
        // Single-shot delivery.
        let mut whole = make_term(rows, cols);
        whole.advance_bytes(&payload);

        // Chunked delivery.
        let mut chunked = make_term(rows, cols);
        let mut offset = 0;
        for size in &chunk_sizes {
            if offset >= payload.len() {
                break;
            }
            let end = (offset + size).min(payload.len());
            chunked.advance_bytes(&payload[offset..end]);
            offset = end;
        }
        // Drain any remainder through a final single call.
        if offset < payload.len() {
            chunked.advance_bytes(&payload[offset..]);
        }

        let cw = whole.cursor_pos();
        let cc = chunked.cursor_pos();
        prop_assert_eq!(
            (cw.x, cw.y),
            (cc.x, cc.y),
            "chunked vs single-shot cursor diverged: single=({}, {}) chunked=({}, {}) payload_len={}",
            cw.x, cw.y, cc.x, cc.y,
            payload.len(),
        );

        // Scrollback row count must match as well — different LF handling
        // across chunk boundaries would manifest here.
        prop_assert_eq!(
            whole.screen().scrollback_rows(),
            chunked.screen().scrollback_rows(),
            "chunked vs single-shot scrollback rows diverged (payload_len={})",
            payload.len(),
        );
    }
}
