//! Equivalence gate for the Round-5 D2 table-driven CSI/OSC dispatch
//! optimization (ft-round5-gauntlet-lw0s7.12).
//!
//! `Parser::set_table_dispatch(true)` must produce a BYTE-IDENTICAL
//! `Action` stream versus the generic parser — the fast CSI (`m`/SGR) and OSC
//! (numeric-code) decoders only short-circuit the dispatch, never change the
//! decoded values, and fall back to the generic parser for any shape they don't
//! handle. This is the bead's named proof: `parse_as_vec` equivalence over a
//! synthetic battery plus the terminal-conformance corpus, including across
//! chunk boundaries.

use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::parser::Parser;
use std::path::PathBuf;

fn parse_whole(bytes: &[u8], table_dispatch: bool) -> Vec<Action> {
    let mut p = Parser::new();
    p.set_table_dispatch(table_dispatch);
    assert_eq!(p.table_dispatch(), table_dispatch);
    p.parse_as_vec(bytes)
}

fn parse_chunked(bytes: &[u8], split: usize, table_dispatch: bool) -> Vec<Action> {
    let mut p = Parser::new();
    p.set_table_dispatch(table_dispatch);
    let mut actions = Vec::new();
    p.parse(&bytes[..split], |a| actions.push(a));
    p.parse(&bytes[split..], |a| actions.push(a));
    actions
}

/// CSI/OSC-focused battery covering the fast paths and the shapes that MUST fall
/// back to the generic parser.
fn battery() -> Vec<(String, Vec<u8>)> {
    let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
    let mut push = |name: &str, bytes: Vec<u8>| cases.push((name.to_string(), bytes));

    // --- SGR: single codes across the whole byte range (incl. unmapped) ---
    for code in 0u16..=130 {
        push(
            &format!("sgr_single_{code}"),
            format!("\x1b[{code}m").into_bytes(),
        );
    }
    // Bright + 256-ish codes and beyond the SgrCode set (force fallback).
    for code in [90, 97, 100, 107, 108, 200, 255, 256, 999, 1000] {
        push(
            &format!("sgr_high_{code}"),
            format!("\x1b[{code}m").into_bytes(),
        );
    }

    // --- SGR: multi-code canonical combos (fast path multi-yield) ---
    for combo in [
        "0", "1;31", "0;1;4;7", "31;42", "1;3;4;5;7;9", "39;49", "53;55",
        "73;74;75", "90;101", "22;23;24;25", "10;11;12;13;14",
    ] {
        push(
            &format!("sgr_combo_{}", combo.replace(';', "_")),
            format!("\x1b[{combo}m").into_bytes(),
        );
    }

    // --- SGR: shapes that MUST fall back to generic ---
    push("sgr_empty", b"\x1b[m".to_vec());
    push("sgr_leading_semi", b"\x1b[;4m".to_vec());
    push("sgr_trailing_semi", b"\x1b[1;m".to_vec());
    push("sgr_double_semi", b"\x1b[1;;4m".to_vec());
    push("sgr_fg_256", b"\x1b[38;5;200m".to_vec());
    push("sgr_fg_rgb", b"\x1b[38;2;10;20;30m".to_vec());
    push("sgr_bg_256", b"\x1b[48;5;12m".to_vec());
    push("sgr_underline_colon", b"\x1b[4:3m".to_vec());
    push("sgr_underline_color", b"\x1b[58;5;9m".to_vec());
    push("sgr_fancy_underline", b"\x1b[4:0;4:1;4:2;4:3;4:4;4:5m".to_vec());
    push("sgr_question", b"\x1b[?4m".to_vec());
    push("sgr_unknown_code", b"\x1b[1;99;31m".to_vec());
    push("sgr_truecolor_then_plain", b"\x1b[38:2::128:64:192mw".to_vec());

    // --- Non-SGR CSI (always generic; confirm gate doesn't disturb them) ---
    for seq in [
        "\x1b[H", "\x1b[2J", "\x1b[1;1H", "\x1b[10A", "\x1b[5B", "\x1b[3C",
        "\x1b[2D", "\x1b[K", "\x1b[2K", "\x1b[?1h", "\x1b[?25l", "\x1b[?1006h",
        "\x1b[6n", "\x1b[!p", "\x1b[1 q", "\x1b[3;4r", "\x1b[>4;2m", "\x1b[<0;1;1M",
        "\x1b[=c", "\x1b[?2026$p", "\x1b[1;2;3;4;5;6*y",
    ] {
        push(&format!("csi_{}", sanitize(seq)), seq.as_bytes().to_vec());
    }

    // --- OSC: fast numeric codes ---
    push("osc_0_title", b"\x1b]0;icon and window\x07".to_vec());
    push("osc_1_icon", b"\x1b]1;icon name\x07".to_vec());
    push("osc_2_window", b"\x1b]2;window title\x07".to_vec());
    push("osc_2_multi", b"\x1b]2;part one;part two\x07".to_vec());
    push("osc_7_cwd", b"\x1b]7;file://host/home/user\x07".to_vec());
    push(
        "osc_8_hyperlink",
        b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07".to_vec(),
    );
    push("osc_9_notify", b"\x1b]9;hello notification\x07".to_vec());
    push("osc_9_progress", b"\x1b]9;4;1;42\x07".to_vec());
    push("osc_52_selection", b"\x1b]52;c;aGVsbG8=\x07".to_vec());
    push("osc_133_prompt", b"\x1b]133;A\x07".to_vec());
    push("osc_1337_iterm", b"\x1b]1337;CurrentDir=/tmp\x07".to_vec());
    push("osc_emoji_title", "\x1b]0;\u{1f915}\x07".as_bytes().to_vec());

    // --- OSC: codes NOT in the fast set (generic path) ---
    push("osc_4_color", b"\x1b]4;0;#000000\x07".to_vec());
    push("osc_10_fg", b"\x1b]10;#ffffff\x07".to_vec());
    push("osc_104_reset", b"\x1b]104\x07".to_vec());
    push("osc_110_reset_fg", b"\x1b]110\x07".to_vec());
    push("osc_22_mouse", b"\x1b]22;pointer\x07".to_vec());
    push("osc_legacy_l", b"\x1b]lwindow title\x07".to_vec());
    push("osc_legacy_L_icon", b"\x1b]Licon name\x07".to_vec());
    push("osc_unknown", b"\x1b]532534523;hello\x07".to_vec());
    push("osc_leading_zero", b"\x1b]00;weird\x07".to_vec()); // "00" != "0" key
    push("osc_st_terminated", b"\x1b]0;via ST\x1b\\".to_vec());

    // --- Mixed kitchen sink ---
    push(
        "kitchen_sink",
        {
            let mut v = Vec::new();
            v.extend_from_slice(b"\x1b]0;title\x07");
            v.extend_from_slice(b"plain \x1b[1;31mred bold\x1b[0m text ");
            v.extend_from_slice("café €1 \u{1f680}".as_bytes());
            v.extend_from_slice(b"\x1b[2J\x1b[H");
            v.extend_from_slice(b"\x1b]8;;http://x.y\x07link\x1b]8;;\x07");
            v.extend_from_slice(b"\x1b[38;5;200mfg256\x1b[0m\r\n");
            v.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07cmd");
            v
        },
    );

    cases
}

fn sanitize(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '_'
            }
        })
        .collect()
}

fn corpus_inputs() -> Vec<(String, Vec<u8>)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/terminal-conformance");
    let mut out = Vec::new();
    for sub in ["transcripts", "minimized"] {
        let dir = root.join(sub);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("hex") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(bytes) = decode_hex(&text) {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string();
                out.push((name, bytes));
            }
        }
    }
    out
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let clean: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(clean.len() / 2);
    for pair in clean.chunks_exact(2) {
        out.push((nibble(pair[0])? << 4) | nibble(pair[1])?);
    }
    Some(out)
}

#[test]
fn whole_buffer_equivalence_battery() {
    for (name, bytes) in battery() {
        let off = parse_whole(&bytes, false);
        let on = parse_whole(&bytes, true);
        assert_eq!(off, on, "table-dispatch parse diverged for {name}");
    }
}

#[test]
fn whole_buffer_equivalence_corpus() {
    let inputs = corpus_inputs();
    assert!(
        inputs.len() >= 6,
        "expected to load the terminal-conformance corpus, got {} inputs",
        inputs.len()
    );
    for (name, bytes) in inputs {
        let off = parse_whole(&bytes, false);
        let on = parse_whole(&bytes, true);
        assert_eq!(off, on, "table-dispatch corpus parse diverged for {name}");
    }
}

#[test]
fn chunk_boundary_equivalence_battery() {
    for (name, bytes) in battery() {
        let reference = parse_whole(&bytes, false);
        for split in 0..=bytes.len() {
            let on = parse_chunked(&bytes, split, true);
            // Chunked + fast must match whole-buffer generic. (Chunking can
            // legitimately split a sequence; the generic chunked parse is the
            // same as whole-buffer here because these inputs don't straddle a
            // partial control sequence in a way that changes actions — the
            // conformance corpus test exercises arbitrary streaming.)
            let off_chunked = parse_chunked(&bytes, split, false);
            assert_eq!(
                on, off_chunked,
                "table-dispatch chunked parse diverged for {name} at split {split}"
            );
            assert_eq!(
                off_chunked, reference,
                "generic chunked parse changed stream for {name} at split {split}"
            );
        }
    }
}

#[test]
fn chunk_boundary_equivalence_corpus() {
    for (name, bytes) in corpus_inputs() {
        for split in 0..=bytes.len() {
            let off = parse_chunked(&bytes, split, false);
            let on = parse_chunked(&bytes, split, true);
            assert_eq!(
                off, on,
                "table-dispatch chunked corpus parse diverged for {name} at split {split}"
            );
        }
    }
}

/// Sanity: the gate is genuinely off without the toggle and the SGR fast path
/// produces the expected actions when on (so the gate isn't vacuous).
#[test]
fn gate_is_off_by_default_and_engages() {
    let mut p = Parser::new();
    assert!(!p.table_dispatch(), "table dispatch must default OFF");
    p.set_table_dispatch(true);
    assert!(p.table_dispatch());

    // A representative SGR + OSC stream must decode identically to generic.
    let bytes = b"\x1b[1;31mhi\x1b[0m\x1b]0;t\x07";
    let off = parse_whole(bytes, false);
    let on = parse_whole(bytes, true);
    assert_eq!(off, on);
    assert!(on.iter().any(|a| matches!(a, Action::CSI(_))));
    assert!(
        on.iter()
            .any(|a| matches!(a, Action::OperatingSystemCommand(_)))
    );
}
