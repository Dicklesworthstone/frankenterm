//! A/B benchmark for the Round-5 D2 table-driven CSI/OSC dispatch optimization
//! (ft-round5-gauntlet-lw0s7.12).
//!
//! Gate (per the marching-orders A/B bench contract): the benched code path
//! reads `FT_MOONSHOT_PARSER_TABLE_DISPATCH` at run time and calls
//! `Parser::set_table_dispatch`, so the driver expresses the two arms via
//! `--gate env:FT_MOONSHOT_PARSER_TABLE_DISPATCH=ON/OFF` (build once, run
//! twice). Baseline = unset/off.
//!
//! Workloads target where the optimization should win: SGR-heavy color streams
//! (CSI `m`) and OSC title/prompt/hyperlink traces. A printable-light CSI-cursor
//! stream is included to confirm neutrality where no fast path applies.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::parser::Parser;
use std::hint::black_box;

fn dispatch_from_env() -> bool {
    match std::env::var("FT_MOONSHOT_PARSER_TABLE_DISPATCH") {
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

/// SGR-heavy: colored log lines with bold/reset/fg/bg — the CSI `m` fast path.
fn sgr_heavy() -> Vec<u8> {
    let mut v = Vec::with_capacity(64 * 1024);
    let lines: &[&[u8]] = &[
        b"\x1b[1;31mERROR\x1b[0m \x1b[33mwarn\x1b[0m \x1b[32mok\x1b[0m line of text\r\n",
        b"\x1b[1m\x1b[4m\x1b[7mstyled\x1b[0m \x1b[90;107mdim-on-white\x1b[0m more text\r\n",
        b"\x1b[34mblue\x1b[0m \x1b[35mmagenta\x1b[0m \x1b[36mcyan\x1b[0m \x1b[37mwhite\x1b[0m\r\n",
    ];
    let mut i = 0;
    while v.len() < 64 * 1024 {
        v.extend_from_slice(lines[i % lines.len()]);
        i += 1;
    }
    v
}

/// OSC-heavy: title / CWD / hyperlink / semantic-prompt sequences.
fn osc_heavy() -> Vec<u8> {
    let mut v = Vec::with_capacity(64 * 1024);
    let seqs: &[&[u8]] = &[
        b"\x1b]0;user@host: ~/projects/frankenterm\x07",
        b"\x1b]7;file://host/home/user/projects\x07",
        b"\x1b]8;;https://example.com/path\x07link text\x1b]8;;\x07",
        b"\x1b]133;A\x07prompt\x1b]133;B\x07",
        b"\x1b]2;window title here\x07",
    ];
    let mut i = 0;
    while v.len() < 64 * 1024 {
        v.extend_from_slice(seqs[i % seqs.len()]);
        v.extend_from_slice(b"some body text in between\r\n");
        i += 1;
    }
    v
}

/// Cursor-heavy CSI with no fast handler — confirms neutrality.
fn cursor_heavy() -> Vec<u8> {
    let mut v = Vec::with_capacity(64 * 1024);
    let seq: &[u8] = b"\x1b[2J\x1b[1;1H\x1b[10A\x1b[5B\x1b[3C\x1b[2D\x1b[K\x1b[2;5H";
    while v.len() < 64 * 1024 {
        v.extend_from_slice(seq);
    }
    v
}

fn bench_dispatch(c: &mut Criterion) {
    let on = dispatch_from_env();
    let arm = if on { "on" } else { "off" };

    let workloads: &[(&str, Vec<u8>)] = &[
        ("sgr_heavy", sgr_heavy()),
        ("osc_heavy", osc_heavy()),
        ("cursor_heavy", cursor_heavy()),
    ];

    let mut group = c.benchmark_group("csi_osc_dispatch");
    for (name, bytes) in workloads {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(BenchmarkId::new(*name, arm), bytes, |b, bytes| {
            b.iter(|| {
                let mut parser = Parser::new();
                parser.set_table_dispatch(on);
                let mut count = 0usize;
                parser.parse(black_box(bytes), |action| match action {
                    Action::CSI(_) => count += 1,
                    Action::OperatingSystemCommand(_) => count += 1,
                    Action::Print(_) => count += 1,
                    _ => count += 1,
                });
                black_box(count)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
