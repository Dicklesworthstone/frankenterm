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

/// Normalize only the representation of contiguous printable output.
///
/// A parser call must flush a printable run at the end of its input slice, so
/// splitting the same byte stream can legitimately turn one `PrintString`
/// into a `Print` plus a shorter `PrintString`. Table dispatch must remain
/// byte-identical for the same chunking, while chunked-versus-whole parsing is
/// compared after this representation-only normalization.
fn normalize_print_runs(actions: Vec<Action>) -> Vec<Action> {
    let mut normalized = Vec::with_capacity(actions.len());
    let mut pending = String::new();
    for action in actions {
        match action {
            Action::Print(character) => pending.push(character),
            Action::PrintString(string) => pending.push_str(&string),
            other => {
                if !pending.is_empty() {
                    normalized.push(Action::PrintString(std::mem::take(&mut pending)));
                }
                normalized.push(other);
            }
        }
    }
    if !pending.is_empty() {
        normalized.push(Action::PrintString(pending));
    }
    normalized
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
        "0",
        "1;31",
        "0;1;4;7",
        "31;42",
        "1;3;4;5;7;9",
        "39;49",
        "53;55",
        "73;74;75",
        "90;101",
        "22;23;24;25",
        "10;11;12;13;14",
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
    push(
        "sgr_fancy_underline",
        b"\x1b[4:0;4:1;4:2;4:3;4:4;4:5m".to_vec(),
    );
    push("sgr_question", b"\x1b[?4m".to_vec());
    push("sgr_unknown_code", b"\x1b[1;99;31m".to_vec());
    push(
        "sgr_truecolor_then_plain",
        b"\x1b[38:2::128:64:192mw".to_vec(),
    );

    // --- Non-SGR CSI (always generic; confirm gate doesn't disturb them) ---
    for seq in [
        "\x1b[H",
        "\x1b[2J",
        "\x1b[1;1H",
        "\x1b[10A",
        "\x1b[5B",
        "\x1b[3C",
        "\x1b[2D",
        "\x1b[K",
        "\x1b[2K",
        "\x1b[?1h",
        "\x1b[?25l",
        "\x1b[?1006h",
        "\x1b[6n",
        "\x1b[!p",
        "\x1b[1 q",
        "\x1b[3;4r",
        "\x1b[>4;2m",
        "\x1b[<0;1;1M",
        "\x1b[=c",
        "\x1b[?2026$p",
        "\x1b[1;2;3;4;5;6*y",
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
    push(
        "osc_emoji_title",
        "\x1b]0;\u{1f915}\x07".as_bytes().to_vec(),
    );

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
    push("kitchen_sink", {
        let mut v = Vec::new();
        v.extend_from_slice(b"\x1b]0;title\x07");
        v.extend_from_slice(b"plain \x1b[1;31mred bold\x1b[0m text ");
        v.extend_from_slice("café €1 \u{1f680}".as_bytes());
        v.extend_from_slice(b"\x1b[2J\x1b[H");
        v.extend_from_slice(b"\x1b]8;;http://x.y\x07link\x1b]8;;\x07");
        v.extend_from_slice(b"\x1b[38;5;200mfg256\x1b[0m\r\n");
        v.extend_from_slice(b"\x1b]133;A\x07$ \x1b]133;B\x07cmd");
        v
    });

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
    let (pairs, remainder) = clean.as_chunks::<2>();
    debug_assert!(remainder.is_empty());
    for (pair_index, pair) in pairs.iter().enumerate() {
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

#[test]
fn whole_buffer_equivalence_battery() {
    for (name, bytes) in battery() {
        let off = parse_whole(&bytes, false);
        let on = parse_whole(&bytes, true);
        assert_eq!(off, on, "table-dispatch parse diverged for {name}");
    }
}

#[test]
fn whole_buffer_equivalence_corpus() -> TestResult {
    let inputs = corpus_inputs()?;
    for (name, bytes) in inputs {
        let off = parse_whole(&bytes, false);
        let on = parse_whole(&bytes, true);
        assert_eq!(off, on, "table-dispatch corpus parse diverged for {name}");
    }
    Ok(())
}

#[test]
fn chunk_boundary_equivalence_battery() {
    for (name, bytes) in battery() {
        let reference = normalize_print_runs(parse_whole(&bytes, false));
        for split in 0..=bytes.len() {
            let on = parse_chunked(&bytes, split, true);
            // The fast and generic paths must remain byte-identical under the
            // same chunking. Chunked versus whole-buffer parsing may differ
            // only in `Print`/`PrintString` segmentation at the call boundary.
            let off_chunked = parse_chunked(&bytes, split, false);
            assert_eq!(
                on, off_chunked,
                "table-dispatch chunked parse diverged for {name} at split {split}"
            );
            assert_eq!(
                normalize_print_runs(off_chunked),
                reference,
                "generic chunked parse changed stream for {name} at split {split}"
            );
        }
    }
}

#[test]
fn chunk_boundary_equivalence_corpus() -> TestResult {
    for (name, bytes) in corpus_inputs()? {
        for split in 0..=bytes.len() {
            let off = parse_chunked(&bytes, split, false);
            let on = parse_chunked(&bytes, split, true);
            assert_eq!(
                off, on,
                "table-dispatch chunked corpus parse diverged for {name} at split {split}"
            );
        }
    }
    Ok(())
}

/// Sanity: the gate can be forced off and the SGR fast path engages when it is
/// forced on, independent of compile-time features or process environment.
#[test]
fn gate_toggle_is_environment_independent_and_engages() {
    let mut p = Parser::new();
    p.set_table_dispatch(false);
    assert!(!p.table_dispatch());
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
