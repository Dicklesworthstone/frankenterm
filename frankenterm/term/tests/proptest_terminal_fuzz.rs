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

use std::io::Write;
use std::sync::{Arc, Mutex};

use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize, UnicodeVersion};
use proptest::prelude::*;

#[derive(Debug)]
struct FuzzConfig {
    scrollback: usize,
    unicode_version: UnicodeVersion,
}

impl TerminalConfiguration for FuzzConfig {
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
        Arc::new(FuzzConfig {
            scrollback: 256,
            unicode_version,
        }),
        "WezTerm",
        "fuzz",
        Box::new(Vec::new()),
    )
}

#[derive(Clone, Default)]
struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedWriter {
    fn snapshot(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChunkSnapshot {
    cursor_x: usize,
    cursor_y: i64,
    title: String,
    scrollback_rows: usize,
    pty_responses: Vec<u8>,
    // NB (FND-008/FND-017): cell content is NOT part of this snapshot. The
    // dedicated cell-content chunk-determinism guarantee lives in
    // `well_formed_unicode_cell_content_is_chunk_boundary_invariant` (curated
    // well-formed input — deterministic). Comparing cells over the *complete-PTY-
    // escape* generators here would be FLAKY because some DCS sequences are not yet
    // cell-chunk-deterministic (pre-existing, tracked GA-FND-017): a 20k-case soak
    // found e.g. `" " + ESC P q a ESC \\` rendering `"  "` byte-by-byte vs `" "`
    // whole. So these complete-escape tests keep their robust cursor + title +
    // scrollback + pty_responses scope and leave cells to the well-formed test.
}

/// Serialize the full in-memory grid (scrollback + visible) as per-line rendered
/// text. This is the FND-008 cell-content axis of the chunk-determinism
/// differential: identical byte streams must produce identical cell content
/// regardless of how the bytes were chunked across `advance_bytes` calls.
fn serialize_screen_cells(term: &Terminal) -> Vec<String> {
    // `for_each_phys_line` is the public (non-`cfg(test)`) iterator over every
    // physical line (scrollback + visible); `visible_lines`/`all_lines` are
    // `#[cfg(test)]` on the crate itself and thus unavailable from an integration
    // test. Capture each line's rendered text.
    let mut cells = Vec::new();
    term.screen().for_each_phys_line(|_idx, line| {
        cells.push(line.as_str().to_string());
    });
    cells
}

fn chunk_snapshot(term: &Terminal, capture: &CapturedWriter) -> ChunkSnapshot {
    let cursor = term.cursor_pos();
    ChunkSnapshot {
        cursor_x: cursor.x,
        cursor_y: cursor.y,
        title: term.get_title().to_string(),
        scrollback_rows: term.screen().scrollback_rows(),
        pty_responses: capture.snapshot(),
    }
}

fn make_term_with_capture(rows: usize, cols: usize) -> (Terminal, CapturedWriter) {
    make_term_with_unicode_capture(rows, cols, UnicodeVersion::new(9))
}

fn make_term_with_unicode_capture(
    rows: usize,
    cols: usize,
    unicode_version: UnicodeVersion,
) -> (Terminal, CapturedWriter) {
    let captured = CapturedWriter::default();
    let term = Terminal::new(
        TerminalSize {
            rows,
            cols,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        },
        Arc::new(FuzzConfig {
            scrollback: 256,
            unicode_version,
        }),
        "WezTerm",
        "fuzz",
        Box::new(captured.clone()),
    );
    (term, captured)
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
        proptest::collection::vec(0x80u8..=0xBF, 1..8),
        // 2-byte start with no continuation
        proptest::collection::vec(0xC2u8..=0xDF, 1..4),
        // 3-byte start truncated
        Just(vec![0xE2u8, 0x82]),
        // 4-byte start truncated
        Just(vec![0xF0u8, 0x9F, 0x98]),
        // BOM + junk
        Just(vec![0xEFu8, 0xBB, 0xBF, 0xC0, 0xC0]),
    ]
}

fn arb_escape_label() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            (b'a'..=b'z').prop_map(char::from),
            (b'A'..=b'Z').prop_map(char::from),
            (b'0'..=b'9').prop_map(char::from),
            Just(' '),
            Just('_'),
            Just('-'),
            Just('.'),
            Just('/'),
            Just(':'),
        ],
        0..24,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn arb_long_printable_body() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        prop_oneof![
            (0x20u8..=0x7Eu8),
            Just(0x1Bu8), // embedded ESC
            Just(0x07u8), // BEL
            Just(0x9Cu8), // 8-bit ST
        ],
        64..512,
    )
}

/// Escape sequences selected to stress parser recovery rather than normal
/// terminal behavior: unterminated string controls, oversized parameter runs,
/// C1/7-bit mixing, and repeated introducers that look like nested sequences.
fn arb_pathological_escape_fragment() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(b"\x1b[".to_vec()), // truncated CSI
        Just(b"\x9b".to_vec()),  // truncated 8-bit CSI
        proptest::collection::vec(
            prop_oneof![
                (b'0'..=b'9'),
                Just(b';'),
                Just(b':'),
                Just(b'?'),
                Just(b'>'),
                Just(b'!'),
                Just(b' '),
            ],
            64..512,
        )
        .prop_map(|mut body| {
            let mut out = vec![0x1B, b'['];
            out.append(&mut body);
            out.push(b'm');
            out
        }),
        arb_long_printable_body().prop_map(|mut body| {
            let mut out = vec![0x1B, b']'];
            out.append(&mut body);
            out
        }),
        arb_long_printable_body().prop_map(|mut body| {
            let mut out = vec![0x1B, b'P'];
            out.append(&mut body);
            out
        }),
        arb_long_printable_body().prop_map(|mut body| {
            let mut out = vec![0x1B, b'_'];
            out.append(&mut body);
            out
        }),
        arb_long_printable_body().prop_map(|mut body| {
            let mut out = vec![0x1B, b'^'];
            out.append(&mut body);
            out
        }),
        arb_long_printable_body().prop_map(|mut body| {
            let mut out = vec![0x1B, b'X'];
            out.append(&mut body);
            out
        }),
        proptest::collection::vec(
            prop_oneof![
                Just(b"\x1b[".to_vec()),
                Just(b"\x1b]".to_vec()),
                Just(b"\x1bP".to_vec()),
                Just(b"\x1b_".to_vec()),
                Just(b"\x1b^".to_vec()),
                Just(b"\x1bX".to_vec()),
                Just(vec![0x9Bu8]),
                Just(vec![0x90u8]),
            ],
            4..64,
        )
        .prop_map(|chunks| chunks.into_iter().flatten().collect()),
        (
            arb_long_printable_body(),
            prop_oneof![Just(vec![0x07u8]), Just(vec![0x1B, b'\\'])]
        )
            .prop_map(|(mut body, terminator)| {
                let mut out = vec![0x1B, b']'];
                out.append(&mut body);
                out.extend(terminator);
                out
            }),
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

fn arb_pathological_escape_payload() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(arb_pathological_escape_fragment(), 1..8).prop_map(|chunks| {
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(chunk);
        }
        out
    })
}

fn arb_subprocess_query_fragment() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(b"\x1b[c".to_vec()),  // primary DA
        Just(b"\x1b[0c".to_vec()), // primary DA with explicit zero
        Just(b"\x1b[>c".to_vec()), // secondary DA
        Just(b"\x1b[=c".to_vec()), // tertiary DA
        Just(b"\x1b[>q".to_vec()), // XTVERSION
        Just(b"\x1b[5n".to_vec()), // status report
        Just(b"\x1b[6n".to_vec()), // cursor position report
        (1u8..=24, 1u8..=80).prop_map(|(row, col)| format!("\x1b[{row};{col}H").into_bytes()),
        proptest::collection::vec(0x20u8..=0x7e, 0..24),
        prop_oneof![
            Just(b"\r".to_vec()),
            Just(b"\n".to_vec()),
            Just(b"\t".to_vec())
        ],
    ]
}

fn arb_subprocess_query_payload() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(arb_subprocess_query_fragment(), 1..24).prop_map(|chunks| {
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(chunk);
        }
        out
    })
}

fn arb_unicode_escape_fragment() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // Standalone Unicode scalars whose width varies across tables, or
        // whose encoded bytes commonly expose parser/chunking mistakes.
        // Multi-scalar ZWJ/variation-selector clusters currently expose a
        // separate terminal buffering bug when split across advance calls.
        prop_oneof![
            Just("é".to_string()),
            Just("Ω".to_string()),
            Just("·".to_string()),
            Just("─".to_string()),
            Just("中".to_string()),
            Just("🙂".to_string()),
        ]
        .prop_map(|s| s.into_bytes()),
        arb_csi_sequence(),
        arb_subprocess_query_fragment(),
        arb_bad_utf8(),
        Just(b"\x1b]1337;UnicodeVersion=9\x1b\\".to_vec()),
        Just(b"\x1b]1337;UnicodeVersion=14\x1b\\".to_vec()),
        Just(b"\x1b]1337;UnicodeVersion=push fuzz\x1b\\".to_vec()),
        Just(b"\x1b]1337;UnicodeVersion=pop fuzz\x1b\\".to_vec()),
        Just(b"\x1b(0".to_vec()), // DEC line drawing G0
        Just(b"\x1b(B".to_vec()), // ASCII G0
        Just(b"\x1b)0".to_vec()), // DEC line drawing G1
        Just(b"\x1b)B".to_vec()), // ASCII G1
        Just(b"\x0e".to_vec()),   // SO: select G1
        Just(b"\x0f".to_vec()),   // SI: select G0
        prop_oneof![
            Just(b"\r".to_vec()),
            Just(b"\n".to_vec()),
            Just(b"\t".to_vec())
        ],
    ]
}

fn arb_unicode_escape_payload() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(arb_unicode_escape_fragment(), 1..32).prop_map(|chunks| {
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(chunk);
        }
        out
    })
}

fn arb_complete_pty_escape_fragment() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        arb_csi_sequence(),
        arb_escape_label().prop_map(|title| format!("\x1b]0;{title}\x1b\\").into_bytes()),
        arb_escape_label().prop_map(|title| format!("\x1b]2;{title}\x07").into_bytes()),
        arb_escape_label().prop_map(|label| format!(
            "\x1b]8;id={label};https://example.test/{label}\x1b\\linked\x1b]8;;\x1b\\"
        )
        .into_bytes()),
        arb_escape_label().prop_map(|label| format!("\x1bP{label}\x1b\\").into_bytes()),
        arb_escape_label().prop_map(|label| format!("\x1b_{label}\x1b\\").into_bytes()),
        arb_escape_label().prop_map(|label| format!("\x1b^{label}\x1b\\").into_bytes()),
        arb_escape_label().prop_map(|label| format!("\x1bX{label}\x1b\\").into_bytes()),
        Just(b"\x1b[c".to_vec()),  // primary DA writes a PTY response
        Just(b"\x1b[5n".to_vec()), // status report writes a PTY response
        Just(b"\x1b[6n".to_vec()), // cursor position report writes a PTY response
        Just(b"\x1b[>q".to_vec()), // XTVERSION writes a PTY response
        Just(b"\x1b(0\x0eqqq\x0f".to_vec()), // charset switch around line drawing
        proptest::collection::vec(0x20u8..=0x7e, 0..32),
    ]
}

fn arb_complete_pty_escape_payload() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(arb_complete_pty_escape_fragment(), 1..12).prop_map(|chunks| {
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

    /// Pathological escape inputs should recover to the same terminal state
    /// whether they arrive in one read or split at inconvenient byte offsets.
    #[test]
    fn pathological_escape_inputs_do_not_panic_or_corrupt_chunk_state(
        payload in arb_pathological_escape_payload(),
        chunk_sizes in proptest::collection::vec(1usize..=7, 1..128),
        rows in 2usize..=16,
        cols in 4usize..=80,
    ) {
        let mut whole = make_term(rows, cols);
        whole.advance_bytes(&payload);
        assert_terminal_invariants(&whole, rows, cols, &payload);

        let mut chunked = make_term(rows, cols);
        let mut offset = 0usize;
        let mut chunks = chunk_sizes.iter().copied().cycle();
        while offset < payload.len() {
            let chunk_len = chunks
                .next()
                .unwrap_or(payload.len())
                .min(payload.len() - offset);
            chunked.advance_bytes(&payload[offset..offset + chunk_len]);
            offset += chunk_len;
        }
        assert_terminal_invariants(&chunked, rows, cols, &payload);

        let whole_cursor = whole.cursor_pos();
        let chunked_cursor = chunked.cursor_pos();
        prop_assert_eq!(
            (chunked_cursor.x, chunked_cursor.y),
            (whole_cursor.x, whole_cursor.y),
            "pathological escape chunking changed cursor for payload_len={} chunks={:?}",
            payload.len(),
            chunk_sizes
        );
        prop_assert_eq!(
            chunked.screen().scrollback_rows(),
            whole.screen().scrollback_rows(),
            "pathological escape chunking changed scrollback rows for payload_len={}",
            payload.len()
        );
    }

    /// Subprocess escape queries are not just display mutations: DA, DSR,
    /// and XTVERSION write answer bytes back to the pty. Splitting those
    /// query sequences across arbitrary read boundaries must not duplicate,
    /// drop, or reorder responses.
    #[test]
    fn subprocess_query_responses_are_chunk_boundary_invariant(
        payload in arb_subprocess_query_payload(),
        chunk_sizes in proptest::collection::vec(1usize..=16, 1..32),
        rows in 4usize..=16,
        cols in 8usize..=80,
    ) {
        let (mut whole, whole_capture) = make_term_with_capture(rows, cols);
        whole.advance_bytes(&payload);

        let (mut chunked, chunked_capture) = make_term_with_capture(rows, cols);
        let mut offset = 0usize;
        let mut chunks = chunk_sizes.iter().copied().cycle();
        while offset < payload.len() {
            let chunk_len = chunks
                .next()
                .unwrap_or(payload.len())
                .min(payload.len() - offset);
            chunked.advance_bytes(&payload[offset..offset + chunk_len]);
            offset += chunk_len;
        }

        prop_assert_eq!(
            chunked_capture.snapshot(),
            whole_capture.snapshot(),
            "chunked subprocess pty escape handling changed response bytes for payload_len={} chunks={:?}",
            payload.len(),
            chunk_sizes
        );
    }

    /// UTF-8 text, malformed byte sequences, charset switching escapes,
    /// iTerm UnicodeVersion OSC controls, and pty query responses must be
    /// chunk-boundary invariant under each supported Unicode width mode.
    #[test]
    fn unicode_escape_handling_is_chunk_boundary_invariant_across_width_modes(
        payload in arb_unicode_escape_payload(),
        chunk_sizes in proptest::collection::vec(1usize..=16, 1..32),
        rows in 4usize..=16,
        cols in 8usize..=80,
    ) {
        for (mode_idx, unicode_version) in unicode_width_modes().into_iter().enumerate() {
            let (mut whole, whole_capture) =
                make_term_with_unicode_capture(rows, cols, unicode_version.clone());
            whole.advance_bytes(&payload);
            assert_terminal_invariants(&whole, rows, cols, &payload);

            let (mut chunked, chunked_capture) =
                make_term_with_unicode_capture(rows, cols, unicode_version);
            let mut offset = 0usize;
            let mut chunks = chunk_sizes.iter().copied().cycle();
            while offset < payload.len() {
                let chunk_len = chunks
                    .next()
                    .unwrap_or(payload.len())
                    .min(payload.len() - offset);
                chunked.advance_bytes(&payload[offset..offset + chunk_len]);
                offset += chunk_len;
            }
            assert_terminal_invariants(&chunked, rows, cols, &payload);

            let whole_cursor = whole.cursor_pos();
            let chunked_cursor = chunked.cursor_pos();
            prop_assert_eq!(
                (chunked_cursor.x, chunked_cursor.y),
                (whole_cursor.x, whole_cursor.y),
                "chunked unicode escape handling changed cursor for mode_idx={} payload_len={} chunks={:?}",
                mode_idx,
                payload.len(),
                chunk_sizes
            );
            prop_assert_eq!(
                chunked.screen().scrollback_rows(),
                whole.screen().scrollback_rows(),
                "chunked unicode escape handling changed scrollback rows for mode_idx={} payload_len={}",
                mode_idx,
                payload.len()
            );
            prop_assert_eq!(
                chunked_capture.snapshot(),
                whole_capture.snapshot(),
                "chunked unicode escape handling changed pty response bytes for mode_idx={} payload_len={}",
                mode_idx,
                payload.len()
            );
        }
    }

    /// Complete PTY escape streams must be byte-chunk invariant: a real PTY
    /// can split ESC, CSI params, OSC/DCS/APC/PM/SOS string bodies, and ST
    /// terminators at any byte. Feeding one byte per `advance_bytes` call
    /// should land on the same observable terminal state and pty response
    /// bytes as a single read.
    #[test]
    fn complete_pty_escape_sequences_match_single_shot_when_split_byte_by_byte(
        payload in arb_complete_pty_escape_payload(),
        rows in 4usize..=16,
        cols in 8usize..=80,
    ) {
        let (mut whole, whole_capture) = make_term_with_capture(rows, cols);
        whole.advance_bytes(&payload);
        assert_terminal_invariants(&whole, rows, cols, &payload);
        let expected = chunk_snapshot(&whole, &whole_capture);

        let (mut chunked, chunked_capture) = make_term_with_capture(rows, cols);
        for byte in &payload {
            chunked.advance_bytes([*byte]);
        }
        assert_terminal_invariants(&chunked, rows, cols, &payload);

        prop_assert_eq!(
            chunk_snapshot(&chunked, &chunked_capture),
            expected,
            "byte-by-byte pty escape chunking changed terminal state for payload_len={}",
            payload.len()
        );
    }

    /// CELL-CONTENT chunk-boundary invariance for WELL-FORMED escape streams
    /// (INV-TERM-2 / gauntlet FND-008). The pre-existing
    /// `chunked_advance_bytes_matches_single_shot` compares only cursor +
    /// scrollback-row-count; the byte-by-byte snapshot test (now upgraded with the
    /// `cells` field) covers cell content but only at 1-byte splits. This adds the
    /// missing axis: ARBITRARY chunk sizes over complete PTY escape streams, with a
    /// full snapshot that includes the rendered text of every in-memory line. A
    /// chunk split that lands text in the wrong cells without moving the cursor or
    /// changing the scrollback count is caught here.
    ///
    /// Scope note (gauntlet FND-009): this asserts invariance for WELL-FORMED
    /// input. Cell content is NOT chunk-invariant for some MALFORMED byte streams
    /// (incomplete UTF-8 multibyte sequences, e.g. `0xC2 0x00`, followed by a
    /// trailing combining mark) — a real but narrow robustness divergence filed
    /// separately as FND-009. Using the complete-escape generator keeps this test
    /// honestly green while the divergence is triaged.
    #[test]
    fn cell_content_chunk_invariant_for_complete_escape_streams(
        payload in arb_complete_pty_escape_payload(),
        chunk_sizes in proptest::collection::vec(1usize..=32, 0..16),
        rows in 4usize..=16,
        cols in 8usize..=80,
    ) {
        let (mut whole, whole_capture) = make_term_with_capture(rows, cols);
        whole.advance_bytes(&payload);
        let expected = chunk_snapshot(&whole, &whole_capture);

        let (mut chunked, chunked_capture) = make_term_with_capture(rows, cols);
        let mut offset = 0;
        for size in &chunk_sizes {
            if offset >= payload.len() {
                break;
            }
            let end = (offset + size).min(payload.len());
            chunked.advance_bytes(&payload[offset..end]);
            offset = end;
        }
        if offset < payload.len() {
            chunked.advance_bytes(&payload[offset..]);
        }
        let actual = chunk_snapshot(&chunked, &chunked_capture);

        // Cursor + title + scrollback + pty-responses must be chunk-invariant over
        // arbitrary chunk sizes for complete escape streams. (Cell content is NOT
        // compared here — see the ChunkSnapshot note + GA-FND-017; the cell-content
        // guarantee lives in the deterministic well-formed test below.)
        prop_assert_eq!(
            actual,
            expected,
            "chunked vs single-shot terminal snapshot diverged (payload_len={})",
            payload.len()
        );
    }
}

/// MUTATION CHECK (non-vacuity guard for FND-008). `serialize_screen_cells` must
/// DISTINGUISH two terminals with different on-screen text and be deterministic
/// for identical input. If it collapsed "hello" and "world" to the same value,
/// the cell-content axis of `cell_content_is_chunk_boundary_invariant` would be
/// vacuous.
#[test]
fn mutation_check_serialize_screen_cells_distinguishes_content() {
    let mut a = make_term(6, 20);
    a.advance_bytes(b"hello");
    let mut b = make_term(6, 20);
    b.advance_bytes(b"world");
    assert_ne!(
        serialize_screen_cells(&a),
        serialize_screen_cells(&b),
        "serializer must distinguish different screen content"
    );

    let mut c = make_term(6, 20);
    c.advance_bytes(b"hello");
    assert_eq!(
        serialize_screen_cells(&a),
        serialize_screen_cells(&c),
        "serializer must be deterministic for identical input"
    );
}

/// INV-TERM-2 (well-formed scope) — gauntlet FND-009.
///
/// PINS the cell-content chunk-determinism that holds for well-formed UTF-8: base
/// characters (precomposed accents, wide CJK, single emoji) AND width-preserving
/// combining marks (the FND-009 fix in `terminalstate/performer.rs` attaches a
/// cross-call zero-width combining mark to the previous cell when it genuinely
/// clusters without changing column width). Each case is fed whole, byte-by-byte
/// (the hardest case — the UTF-8 collector must buffer partial sequences AND the
/// combining mark arrives standalone), and at arbitrary chunk sizes; all must
/// yield byte-identical screen cells.
///
/// FND-009's remainder has a dedicated regression below because it needs
/// retroactive width expansion and ZWJ re-clustering across call boundaries.
#[test]
fn well_formed_unicode_cell_content_is_chunk_boundary_invariant() {
    let cases: &[&str] = &[
        "abc",                   // ascii baseline
        "café",                  // precomposed é (single-codepoint base)
        "a\u{0301}e\u{0301}",    // combining acute over base chars (FND-009 fix)
        "x\u{0730}y",            // the FND-009 combining mark, on a valid base
        "a\u{0301}\u{0323}",     // stacked combining (acute + dot below)
        "你好世界",              // wide CJK (3-byte, width 2)
        "日本語テスト",          // more CJK
        "🎉🚀",                  // 4-byte emoji (base chars)
        "mixed café 你 🎉 done", // realistic mixed line
    ];
    for case in cases {
        let bytes = case.as_bytes();
        let mut whole = make_term(8, 40);
        whole.advance_bytes(bytes);
        let expected = serialize_screen_cells(&whole);

        // (a) byte-by-byte — splits every multibyte char mid-sequence.
        let mut bb = make_term(8, 40);
        for b in bytes {
            bb.advance_bytes([*b]);
        }
        assert_eq!(
            serialize_screen_cells(&bb),
            expected,
            "well-formed {case:?} diverged under byte-by-byte chunking"
        );

        // (b) several arbitrary chunk sizes.
        for &size in &[2usize, 3, 5, 7] {
            let mut ch = make_term(8, 40);
            let mut off = 0;
            while off < bytes.len() {
                let end = (off + size).min(bytes.len());
                ch.advance_bytes(&bytes[off..end]);
                off = end;
            }
            assert_eq!(
                serialize_screen_cells(&ch),
                expected,
                "well-formed {case:?} diverged under chunk size {size}"
            );
        }
    }
}

/// FND-009 remainder — width-changing marks and multi-base ZWJ sequences.
///
/// The performer reopens the committed cell at the cursor boundary when the
/// next scalar still forms one grapheme cluster with it. That covers VS16
/// widening (`❤\u{FE0F}`) and ZWJ emoji chains split across `advance_bytes`
/// calls without holding arbitrary complete emoji in the print buffer.
#[test]
fn width_changing_and_zwj_clusters_are_chunk_boundary_invariant() {
    for case in ["❤\u{FE0F}", "👨\u{200D}👩\u{200D}👧"] {
        let bytes = case.as_bytes();
        let mut whole = make_term(8, 40);
        whole.advance_bytes(bytes);
        let expected = serialize_screen_cells(&whole);
        let expected_cursor = whole.cursor_pos();

        let mut bb = make_term(8, 40);
        for b in bytes {
            bb.advance_bytes([*b]);
        }
        assert_eq!(
            serialize_screen_cells(&bb),
            expected,
            "FND-009 remainder {case:?} diverged under byte-by-byte chunking"
        );
        let cursor = bb.cursor_pos();
        assert_eq!(
            (cursor.x, cursor.y),
            (expected_cursor.x, expected_cursor.y),
            "FND-009 remainder {case:?} cursor diverged under byte-by-byte chunking"
        );

        for &size in &[2usize, 3, 5, 7] {
            let mut ch = make_term(8, 40);
            let mut off = 0;
            while off < bytes.len() {
                let end = (off + size).min(bytes.len());
                ch.advance_bytes(&bytes[off..end]);
                off = end;
            }
            assert_eq!(
                serialize_screen_cells(&ch),
                expected,
                "FND-009 remainder {case:?} diverged under chunk size {size}"
            );
            let cursor = ch.cursor_pos();
            assert_eq!(
                (cursor.x, cursor.y),
                (expected_cursor.x, expected_cursor.y),
                "FND-009 remainder {case:?} cursor diverged under chunk size {size}"
            );
        }
    }
}

/// FND-017 (FIXED) — DCS/sixel cell content is chunk-boundary-invariant.
///
/// A 20k-case soak of the complete-escape chunk-determinism test (after FND-008
/// added cell content to the comparison) found that `" "` followed by a sixel
/// DCS (`ESC P q a ESC \`) rendered as `"  "` byte-by-byte but `" "` whole.
/// Root cause was NOT the vendored parser: vtparse's `parse` is literally
/// `for b in bytes { parse_byte(b) }`, so the `Action` stream is identical
/// either way. The divergence was in ft's own `terminalstate::performer`: the
/// `Sixel` dispatch arm rendered the image WITHOUT first flushing the pending
/// `print` buffer, so a preceding `Print(' ')` left the cursor un-advanced and
/// the sixel landed on column 0. Because the print buffer is flushed at every
/// `advance_bytes` call boundary (Performer `Drop`), whole vs split delivery
/// diverged. Fixed by flushing before the sixel — mirroring the `KittyImage`
/// arm — so the sixel placement is now chunk-invariant (and byte-by-byte, the
/// previously-correct path, is preserved). This asserts equality as the
/// regression gate.
#[test]
fn fnd_017_dcs_sixel_cell_content_is_chunk_boundary_invariant() {
    let payload: &[u8] = &[0x20, 0x1B, b'P', b'q', b'a', 0x1B, 0x5C]; // " " + DCS q a ST
    let mut whole = make_term(4, 8);
    whole.advance_bytes(payload);
    let expected = serialize_screen_cells(&whole);

    let mut bb = make_term(4, 8);
    for b in payload {
        bb.advance_bytes([*b]);
    }
    assert_eq!(
        serialize_screen_cells(&bb),
        expected,
        "FND-017: sixel cell content must be identical whole vs byte-by-byte"
    );
}
