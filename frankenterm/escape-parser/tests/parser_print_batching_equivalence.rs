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
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

type TestResult<T = ()> = Result<T, String>;

/// Keep this exact path set synchronized with the input artifacts declared by
/// `terminal-conformance/manifest.json` and its minimized-case metadata. A
/// floor such as "at least six inputs" can silently pass after a fixture is
/// omitted, unreadable, or malformed; this closed manifest cannot.
const EXPECTED_CORPUS_PATHS: &[&str] = &[
    "minimized/tc-minimized-synthetic-failure-001.hex",
    "transcripts/tc-alt-screen-001.hex",
    "transcripts/tc-bracketed-paste-focus-001.hex",
    "transcripts/tc-cursor-mode-001.hex",
    "transcripts/tc-graphics-negative-001.hex",
    "transcripts/tc-osc8-hyperlink-001.hex",
    "transcripts/tc-resize-wrap-001.hex",
    "transcripts/tc-utf8-grapheme-001.hex",
];

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/terminal-conformance")
}

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

fn collect_hex_paths(root: &Path, dir: &Path, paths: &mut Vec<(String, PathBuf)>) -> TestResult {
    let entries = std::fs::read_dir(dir).map_err(|err| {
        format!(
            "failed to read required corpus directory {}: {err}",
            dir.display()
        )
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("failed to read an entry in {}: {err}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("failed to inspect corpus entry {}: {err}", path.display()))?;
        if file_type.is_dir() {
            collect_hex_paths(root, &path, paths)?;
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("hex") {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|err| {
                format!(
                    "corpus path {} escaped fixture root {}: {err}",
                    path.display(),
                    root.display()
                )
            })?
            .to_str()
            .ok_or_else(|| format!("corpus path is not valid UTF-8: {}", path.display()))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        paths.push((relative, path));
    }
    Ok(())
}

fn validate_corpus_paths(mut actual: Vec<String>) -> TestResult<Vec<String>> {
    actual.sort_unstable();
    let actual_set = actual.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let expected_set = EXPECTED_CORPUS_PATHS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let missing = expected_set
        .difference(&actual_set)
        .copied()
        .collect::<Vec<_>>();
    let unexpected = actual_set
        .difference(&expected_set)
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() || !unexpected.is_empty() || actual.len() != actual_set.len() {
        return Err(format!(
            "terminal-conformance hex manifest mismatch: missing={missing:?}, unexpected={unexpected:?}, duplicate_paths={}",
            actual.len().saturating_sub(actual_set.len())
        ));
    }
    Ok(actual)
}

fn corpus_inputs_from_root(root: &Path) -> TestResult<Vec<(String, Vec<u8>)>> {
    let mut paths = Vec::new();
    for required_subdir in ["transcripts", "minimized"] {
        collect_hex_paths(root, &root.join(required_subdir), &mut paths)?;
    }
    paths.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let names = paths.iter().map(|(relative, _)| relative.clone()).collect();
    let names = validate_corpus_paths(names)?;

    paths
        .into_iter()
        .zip(names)
        .map(|((relative, path), expected_relative)| {
            if relative != expected_relative {
                return Err(format!(
                    "internal corpus ordering mismatch: discovered={relative}, validated={expected_relative}"
                ));
            }
            let bytes = read_corpus_hex(&path, &relative)?;
            Ok((relative, bytes))
        })
        .collect()
}

fn corpus_inputs() -> TestResult<Vec<(String, Vec<u8>)>> {
    corpus_inputs_from_root(&fixture_root())
}

fn read_corpus_hex(path: &Path, relative: &str) -> TestResult<Vec<u8>> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read corpus input {}: {err}", path.display()))?;
    decode_hex(&text, relative)
}

// `slice::as_chunks` is newer than the workspace's Rust 1.85 MSRV. Retain the
// exact two-byte iterator until the compiler floor makes Clippy's replacement
// available.
#[allow(clippy::chunks_exact_to_as_chunks)]
fn decode_hex(text: &str, label: &str) -> TestResult<Vec<u8>> {
    let clean: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if clean.is_empty() {
        return Err(format!("empty hex input in {label}"));
    }
    if !clean.len().is_multiple_of(2) {
        return Err(format!("odd-length hex in {label}"));
    }
    let nibble = |byte: u8, pair_index: usize, half: &str| -> TestResult<u8> {
        let value = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => {
                return Err(format!(
                    "invalid {half} hex nibble {:?} at byte pair {pair_index} in {label}",
                    byte as char
                ));
            }
        };
        Ok(value)
    };
    let mut out = Vec::with_capacity(clean.len() / 2);
    for (pair_index, pair) in clean.chunks_exact(2).enumerate() {
        out.push((nibble(pair[0], pair_index, "high")? << 4) | nibble(pair[1], pair_index, "low")?);
    }
    Ok(out)
}

#[test]
fn corpus_path_contract_is_exact_and_deterministic() -> TestResult {
    let reversed = EXPECTED_CORPUS_PATHS
        .iter()
        .rev()
        .map(ToString::to_string)
        .collect();
    let ordered = validate_corpus_paths(reversed)?;
    assert_eq!(
        ordered.iter().map(String::as_str).collect::<Vec<_>>(),
        EXPECTED_CORPUS_PATHS
    );

    let mut missing = EXPECTED_CORPUS_PATHS
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let missing_path = missing.pop().expect("the pinned corpus is non-empty");
    let error = validate_corpus_paths(missing).expect_err("a missing fixture must fail closed");
    assert!(
        error.contains(missing_path.as_str()),
        "unexpected error: {error}"
    );

    let mut unexpected = EXPECTED_CORPUS_PATHS
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    unexpected.push("transcripts/unexpected.hex".to_owned());
    let error =
        validate_corpus_paths(unexpected).expect_err("an unexpected fixture must fail closed");
    assert!(
        error.contains("unexpected.hex"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn corpus_loader_rejects_missing_required_directories() {
    let missing_root = fixture_root().join("definitely-not-a-corpus-root");
    let error = corpus_inputs_from_root(&missing_root)
        .expect_err("missing required corpus directories must fail closed");
    assert!(
        error.contains("failed to read required corpus directory"),
        "unexpected error: {error}"
    );
}

#[test]
fn corpus_reader_propagates_input_read_errors() {
    let error = read_corpus_hex(&fixture_root(), "fixture-root")
        .expect_err("attempting to read a directory as hex must fail closed");
    assert!(
        error.contains("failed to read corpus input"),
        "unexpected error: {error}"
    );
}

#[test]
fn corpus_hex_decoder_rejects_odd_and_invalid_text() {
    let odd = decode_hex("0a f", "odd.hex").expect_err("odd-length hex must fail closed");
    assert!(odd.contains("odd-length hex"), "unexpected error: {odd}");

    let invalid = decode_hex("0g", "invalid.hex").expect_err("invalid hex must fail closed");
    assert!(
        invalid.contains("invalid low hex nibble"),
        "unexpected error: {invalid}"
    );

    let empty = decode_hex(" \n\t", "empty.hex").expect_err("empty hex must fail closed");
    assert!(
        empty.contains("empty hex input"),
        "unexpected error: {empty}"
    );
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
fn whole_buffer_equivalence_corpus() -> TestResult {
    let inputs = corpus_inputs()?;
    for (name, bytes) in inputs {
        assert_whole_equivalent(&name, &bytes);
    }
    Ok(())
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
fn chunk_boundary_equivalence_corpus() -> TestResult {
    for (name, bytes) in corpus_inputs()? {
        let reference = normalize(parse_whole(&bytes, false));
        for split in 0..=bytes.len() {
            let on = normalize(parse_chunked(&bytes, split, true));
            assert_eq!(
                on, reference,
                "batched chunked corpus parse diverged for {name} at split {split}"
            );
        }
    }
    Ok(())
}

/// Sanity: the explicit false override uses the scalar emission path and the
/// explicit true override engages batching, independent of the default-on
/// environment policy for newly constructed parsers.
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
