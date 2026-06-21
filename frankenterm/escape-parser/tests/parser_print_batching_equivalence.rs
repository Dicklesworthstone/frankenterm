//! Equivalence gate for the Round-5 D1 parser printable-run batching
//! optimization (ft-round5-gauntlet-lw0s7.10).
//!
//! `Parser::set_print_batching(true)` must change the emitted action stream
//! ONLY by coalescing adjacent ground-state `Action::Print(char)` values into
//! `Action::PrintString` — every non-print action stays byte-identical, in the
//! same order, including across arbitrary chunk boundaries. Two action streams
//! are compared after normalizing each maximal run of `Print`/`PrintString`
//! into a single `PrintString`; if batching ever swallowed a control, dropped a
//! codepoint, mis-decoded UTF-8, or desynchronised the state machine, the
//! normalized streams would diverge.

use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::parser::Parser;
use std::path::PathBuf;

/// Coalesce every maximal run of `Print`/`PrintString` into one `PrintString`,
/// leaving all other actions untouched and in order. After normalization the
/// scalar (per-char) and batched parses are equal iff they are print-equivalent.
fn normalize(actions: Vec<Action>) -> Vec<Action> {
    let mut out: Vec<Action> = Vec::with_capacity(actions.len());
    let mut pending = String::new();
    for action in actions {
        match action {
            Action::Print(c) => pending.push(c),
            Action::PrintString(s) => pending.push_str(&s),
            other => {
                if !pending.is_empty() {
                    out.push(Action::PrintString(std::mem::take(&mut pending)));
                }
                out.push(other);
            }
        }
    }
    if !pending.is_empty() {
        out.push(Action::PrintString(pending));
    }
    out
}

fn parse_whole(bytes: &[u8], batching: bool) -> Vec<Action> {
    let mut p = Parser::new();
    p.set_print_batching(batching);
    assert_eq!(p.print_batching(), batching);
    p.parse_as_vec(bytes)
}

fn parse_chunked(bytes: &[u8], split: usize, batching: bool) -> Vec<Action> {
    let mut p = Parser::new();
    p.set_print_batching(batching);
    let mut actions = Vec::new();
    p.parse(&bytes[..split], |a| actions.push(a));
    p.parse(&bytes[split..], |a| actions.push(a));
    actions
}

/// Hand-built battery covering every branch of `scan_printable_run` and the
/// ground-state print path, including the dangerous edges.
fn battery() -> Vec<(&'static str, Vec<u8>)> {
    let mut cases: Vec<(&'static str, Vec<u8>)> = vec![
        ("empty", b"".to_vec()),
        ("single_ascii", b"x".to_vec()),
        ("two_ascii", b"ab".to_vec()),
        (
            "pure_ascii",
            b"the quick brown fox jumps over 0123456789".to_vec(),
        ),
        ("ascii_run_long", vec![b'q'; 300]),
        ("ascii_with_c0", b"abc\r\ndef\tghi\x08jkl\x07mno".to_vec()),
        ("only_controls", b"\r\n\t\x08\x07\x00\x01\x1f".to_vec()),
        ("del_in_run", b"abc\x7fdef".to_vec()),
        ("del_only", b"\x7f".to_vec()),
        // Latin-1 supplement: decodes to U+00A0..U+00FF, all PRINT in ground.
        ("latin1_nbsp", b"a\xc2\xa0b".to_vec()),
        ("latin1_accents", "café déjà ñ über".as_bytes().to_vec()),
        // 2/3/4-byte UTF-8.
        ("utf8_euro", "10€ 20€".as_bytes().to_vec()),
        (
            "utf8_emoji",
            "rocket \u{1f680} done \u{1f4a9}".as_bytes().to_vec(),
        ),
        ("utf8_cjk", "日本語テキスト".as_bytes().to_vec()),
        ("combining_marks", b"a\xcc\x81e\xcc\x80".to_vec()),
        // C1 control encoded as UTF-8: 0xC2 0x9B == U+009B == CSI.
        ("c1_csi_via_utf8", b"ab\xc2\x9b31mcd".to_vec()),
        ("c1_via_utf8_all", {
            let mut v = b"x".to_vec();
            for lo in 0x80u8..=0x9f {
                v.push(0xc2);
                v.push(lo);
            }
            v.extend_from_slice(b"y");
            v
        }),
        // Raw 8-bit C1 byte (not UTF-8): handled by the anywhere_or table.
        ("raw_c1_csi", b"ab\x9b31mcd".to_vec()),
        // Boundary between 0x9F (C1) and 0xA0 (printable) decoded from UTF-8.
        ("c1_boundary", b"a\xc2\x9f\xc2\xa0b".to_vec()),
        // Invalid / truncated UTF-8 — must fall through to scalar (replacement).
        ("invalid_lead_then_ascii", b"a\xc2Zb".to_vec()),
        ("invalid_ff", b"a\xffb".to_vec()),
        ("overlong_nul", b"a\xc0\x80b".to_vec()),
        ("stray_continuation", b"a\x80\x81b".to_vec()),
        ("incomplete_2byte_tail", b"ab\xc3".to_vec()),
        ("incomplete_3byte_tail", b"ab\xe2\x82".to_vec()),
        ("incomplete_4byte_tail", b"ab\xf0\x9f\x9a".to_vec()),
        ("invalid_f5_lead", b"ab\xf5\x80\x80\x80cd".to_vec()),
        // Escape / CSI / OSC heavy (the "neutral" path).
        (
            "sgr_heavy",
            b"\x1b[1m\x1b[31m\x1b[4m\x1b[0m\x1b[2J\x1b[H".to_vec(),
        ),
        (
            "sgr_with_text",
            b"\x1b[1mbold\x1b[0m normal \x1b[31mred\x1b[0m".to_vec(),
        ),
        ("osc_title", b"\x1b]0;my title\x07body text".to_vec()),
        (
            "osc8_hyperlink",
            b"\x1b]8;;http://example.com\x07link\x1b]8;;\x07".to_vec(),
        ),
        ("esc_only", b"\x1bH\x1b%H\x1bc".to_vec()),
        ("dcs_decrqss", b"\x1bP$qm\x1b\\after".to_vec()),
        ("tmux_title_escape", b"\x1bktitle\x1b\\rest".to_vec()),
        // Mixed everything in one stream.
        ("kitchen_sink", {
            let mut v = Vec::new();
            v.extend_from_slice(b"plain ascii ");
            v.extend_from_slice("café €1 \u{1f600} ".as_bytes());
            v.extend_from_slice(b"\x1b[1mbold\x1b[0m\r\n");
            v.extend_from_slice(b"tab\there\x07bell ");
            v.extend_from_slice(b"\x1b]0;title\x07more text");
            v.extend_from_slice(b"\xc2\x9bquasi-csi");
            v.extend_from_slice("日本".as_bytes());
            v.push(0xff); // stray invalid byte
            v.extend_from_slice(b"end");
            v
        }),
    ];

    // Printable runs ending exactly on a control / UTF-8 / SWAR-word boundary.
    for len in [1usize, 2, 7, 8, 9, 15, 16, 17, 31, 32, 64] {
        for (tag, sep) in [
            ("lf", b"\n".as_slice()),
            ("csi", b"\x1b[4m".as_slice()),
            ("utf8", b"\xc3\xa9".as_slice()),
            ("emoji", b"\xf0\x9f\x9a\x80".as_slice()),
            ("c1utf8", b"\xc2\x9b".as_slice()),
            ("del", b"\x7f".as_slice()),
        ] {
            let mut v = vec![b'a'; len];
            v.extend_from_slice(sep);
            v.extend_from_slice(b"tail");
            // Leak the tag into a 'static label via a small fixed table is
            // overkill; use a generic stable name. The index disambiguates.
            let name: &'static str = match tag {
                "lf" => "boundary_lf",
                "csi" => "boundary_csi",
                "utf8" => "boundary_utf8",
                "emoji" => "boundary_emoji",
                "c1utf8" => "boundary_c1utf8",
                _ => "boundary_del",
            };
            cases.push((name, v));
        }
    }

    cases
}

fn corpus_inputs() -> Vec<(String, Vec<u8>)> {
    let root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/terminal-conformance");
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

/// Assert the whole-buffer scalar and batched parses are print-equivalent for
/// a single input.
fn assert_whole_equivalent(name: &str, bytes: &[u8]) {
    let off = normalize(parse_whole(bytes, false));
    let on = normalize(parse_whole(bytes, true));
    assert_eq!(off, on, "whole-buffer parse diverged for {name}");
}

#[test]
fn whole_buffer_equivalence_battery() {
    for (name, bytes) in battery() {
        assert_whole_equivalent(name, &bytes);
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
        assert_whole_equivalent(&name, &bytes);
    }
}

#[test]
fn chunk_boundary_equivalence_battery() {
    for (name, bytes) in battery() {
        // Scalar parse over the whole buffer is the reference oracle; chunking
        // must not change it, and batching (chunked) must match it too.
        let reference = normalize(parse_whole(&bytes, false));
        for split in 0..=bytes.len() {
            let off = normalize(parse_chunked(&bytes, split, false));
            let on = normalize(parse_chunked(&bytes, split, true));
            assert_eq!(
                off, reference,
                "scalar chunked parse changed the stream for {name} at split {split}"
            );
            assert_eq!(
                on, reference,
                "batched chunked parse diverged for {name} at split {split}"
            );
        }
    }
}

#[test]
fn chunk_boundary_equivalence_corpus() {
    for (name, bytes) in corpus_inputs() {
        let reference = normalize(parse_whole(&bytes, false));
        for split in 0..=bytes.len() {
            let on = normalize(parse_chunked(&bytes, split, true));
            assert_eq!(
                on, reference,
                "batched chunked corpus parse diverged for {name} at split {split}"
            );
        }
    }
}

/// Sanity: batching is genuinely off without the toggle (no `PrintString` ever)
/// and genuinely engages with it (printable-heavy input yields a `PrintString`).
#[test]
fn batching_toggle_actually_changes_emission() {
    let bytes = b"hello world, this is a long printable run";

    let off = parse_whole(bytes, false);
    assert!(
        off.iter().all(|a| !matches!(a, Action::PrintString(_))),
        "scalar path must never emit PrintString"
    );
    assert!(off.iter().any(|a| matches!(a, Action::Print(_))));

    let on = parse_whole(bytes, true);
    assert!(
        on.iter().any(|a| matches!(a, Action::PrintString(_))),
        "batched path must coalesce the printable run into a PrintString"
    );
}

/// The C1-via-UTF-8 sequence `0xC2 0x9B` (U+009B == CSI) must still be parsed
/// as a control sequence under batching, never folded into a `PrintString`.
#[test]
fn c1_via_utf8_is_not_swallowed_into_printstring() {
    let bytes = b"ab\xc2\x9b31mcd";
    let on = parse_whole(bytes, true);

    // There must be at least one CSI action (the SGR set-foreground-red).
    assert!(
        on.iter().any(|a| matches!(a, Action::CSI(_))),
        "C1-via-UTF-8 CSI must dispatch a CSI action, got {on:?}"
    );
    // No PrintString may contain the raw C1 codepoint.
    for a in &on {
        if let Action::PrintString(s) = a {
            assert!(
                !s.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c)),
                "PrintString leaked a C1 control: {s:?}"
            );
        }
        if let Action::Print(c) = a {
            assert!(
                !('\u{80}'..='\u{9f}').contains(c),
                "Print leaked a C1 control: {c:?}"
            );
        }
    }
}
