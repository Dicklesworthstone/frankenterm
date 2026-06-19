//! A/B benchmark for the Round-5 D1 parser printable-run batching optimization
//! (ft-round5-gauntlet-lw0s7.10).
//!
//! Gate (per the marching-orders A/B bench contract): the benched code path
//! reads `FT_MOONSHOT_PARSER_PRINT_BATCHING` at run time and calls
//! `Parser::set_print_batching`, so the driver expresses the two arms via
//! `--gate env:FT_MOONSHOT_PARSER_PRINT_BATCHING=ON/OFF` (build once, run
//! twice). Baseline = unset/off.
//!
//! Workloads are chosen so the optimization SHOULD win on printable-heavy
//! streams, win modestly on mixed logs, and be neutral on CSI-heavy streams.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::parser::Parser;

fn batching_from_env() -> bool {
    match std::env::var("FT_MOONSHOT_PARSER_PRINT_BATCHING") {
        Ok(v) => {
            let v = v.trim();
            v.eq_ignore_ascii_case("on")
                || v.eq_ignore_ascii_case("1")
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

/// Printable-heavy: long lines of plain ASCII with the occasional newline —
/// the case the optimization targets (most bytes are ground-state prints).
fn printable_heavy() -> Vec<u8> {
    let mut v = Vec::with_capacity(64 * 1024);
    let line = b"the quick brown fox jumps over the lazy dog 0123456789 abcdefghij\n";
    while v.len() < 64 * 1024 {
        v.extend_from_slice(line);
    }
    v
}

/// Mixed log: printable runs interleaved with SGR color changes and CR/LF, like
/// a chatty build log with colored severity tags.
fn mixed_log() -> Vec<u8> {
    let mut v = Vec::with_capacity(64 * 1024);
    let lines: &[&[u8]] = &[
        b"\x1b[32mINFO\x1b[0m  compiling crate frankenterm-core v0.8.0\r\n",
        b"\x1b[33mWARN\x1b[0m  unused variable `tmp` in src/lib.rs:1284\r\n",
        b"\x1b[31mERROR\x1b[0m mismatched types: expected u32, found i64\r\n",
        b"    finished in 12.34s with 0 errors and 2 warnings\r\n",
    ];
    let mut i = 0;
    while v.len() < 64 * 1024 {
        v.extend_from_slice(lines[i % lines.len()]);
        i += 1;
    }
    v
}

/// UTF-8 heavy: multibyte text (accents, currency, CJK, emoji), exercising the
/// from_utf8 path in the run scanner.
fn utf8_heavy() -> Vec<u8> {
    let mut v = Vec::with_capacity(64 * 1024);
    let line = "café déjà — 10€ 20€ — 日本語のテキスト — rocket \u{1f680} ok\n";
    while v.len() < 64 * 1024 {
        v.extend_from_slice(line.as_bytes());
    }
    v
}

/// CSI heavy: cursor moves and SGR with little printable text — the
/// neutral/worst case (batching should not regress here).
fn csi_heavy() -> Vec<u8> {
    let mut v = Vec::with_capacity(64 * 1024);
    let seq: &[u8] = b"\x1b[H\x1b[2J\x1b[1;1H\x1b[31m\x1b[1m\x1b[4m\x1b[0m\x1b[2;5H\x1b[K";
    while v.len() < 64 * 1024 {
        v.extend_from_slice(seq);
    }
    v
}

fn bench_parse(c: &mut Criterion) {
    let on = batching_from_env();
    let arm = if on { "on" } else { "off" };

    let workloads: &[(&str, Vec<u8>)] = &[
        ("printable_heavy", printable_heavy()),
        ("mixed_log", mixed_log()),
        ("utf8_heavy", utf8_heavy()),
        ("csi_heavy", csi_heavy()),
    ];

    let mut group = c.benchmark_group("parser_print_batching");
    for (name, bytes) in workloads {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(BenchmarkId::new(*name, arm), bytes, |b, bytes| {
            b.iter(|| {
                let mut parser = Parser::new();
                parser.set_print_batching(on);
                let mut count = 0usize;
                parser.parse(black_box(bytes), |action| {
                    // Touch the action so the optimizer can't elide the parse.
                    match action {
                        Action::Print(_) => count += 1,
                        Action::PrintString(s) => count += s.len(),
                        _ => count += 1,
                    }
                });
                black_box(count)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
