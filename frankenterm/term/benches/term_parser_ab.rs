//! Round-6 term/parser A/B throughput harness for ft-p4vzl.7.
//!
//! The Round-7 recommended term-render gates default on together. The campaign
//! runner can disable the whole recommended set with `FT_MOONSHOT_RECOMMENDED=0`
//! or disable individual recommended gates with their own falsey env value:
//! - recommended set: `FT_MOONSHOT_RECOMMENDED`
//! - EV1: `FT_MOONSHOT_TERM_BULK_ASCII_ROW_WRITE`
//! - B5: `FT_MOONSHOT_TERM_ASCII_CLUSTER_RUN_APPEND`
//! - D1: `FT_MOONSHOT_PARSER_PRINT_BATCHING`
//!
//! D2 remains opt-in and is toggled independently:
//! - D2: `FT_MOONSHOT_PARSER_TABLE_DISPATCH`
//!
//! Dense ASCII rows exercise D1 and the full terminal EV1 row writer. CSI/OSC
//! heavy traces exercise D2 in both parser-only and terminal paths.

use std::convert::TryFrom;
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::parser::Parser;
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};
use serde_json::json;

const SAMPLE_COUNT: usize = 51;
const WARMUP_SAMPLES: usize = 8;

const MOONSHOT_RECOMMENDED_ENV: &str = "FT_MOONSHOT_RECOMMENDED";
const BULK_ASCII_ROW_WRITE_ENV: &str = "FT_MOONSHOT_TERM_BULK_ASCII_ROW_WRITE";
const ASCII_CLUSTER_RUN_APPEND_ENV: &str = "FT_MOONSHOT_TERM_ASCII_CLUSTER_RUN_APPEND";
const PARSER_PRINT_BATCHING_ENV: &str = "FT_MOONSHOT_PARSER_PRINT_BATCHING";
const PARSER_TABLE_DISPATCH_ENV: &str = "FT_MOONSHOT_PARSER_TABLE_DISPATCH";
const MOONSHOT_ALL_ENV: &str = "FT_MOONSHOT_ALL";

#[derive(Debug)]
struct BenchTermConfig;

impl TerminalConfiguration for BenchTermConfig {
    fn scrollback_size(&self) -> usize {
        4096
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

fn make_terminal() -> Terminal {
    Terminal::new(
        TerminalSize {
            rows: 40,
            cols: 120,
            pixel_width: 960,
            pixel_height: 640,
            dpi: 96,
        },
        Arc::new(BenchTermConfig),
        "frankenterm-term-parser-ab",
        env!("CARGO_PKG_VERSION"),
        Box::new(Vec::new()),
    )
}

fn env_flag_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            value.eq_ignore_ascii_case("1")
                || value.eq_ignore_ascii_case("on")
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

fn env_flag_falsey(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            value.is_empty()
                || value == "0"
                || value.eq_ignore_ascii_case("off")
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("no")
        }
        Err(_) => false,
    }
}

fn moonshot_recommended_enabled() -> bool {
    !env_flag_falsey(MOONSHOT_RECOMMENDED_ENV)
}

fn term_bulk_ascii_row_write_enabled() -> bool {
    moonshot_recommended_enabled() && !env_flag_falsey(BULK_ASCII_ROW_WRITE_ENV)
}

fn term_ascii_cluster_run_append_enabled() -> bool {
    moonshot_recommended_enabled() && !env_flag_falsey(ASCII_CLUSTER_RUN_APPEND_ENV)
}

fn parser_print_batching_enabled() -> bool {
    moonshot_recommended_enabled() && !env_flag_falsey(PARSER_PRINT_BATCHING_ENV)
}

fn dense_ascii_rows_payload() -> &'static [u8] {
    static PAYLOAD: OnceLock<Vec<u8>> = OnceLock::new();
    PAYLOAD
        .get_or_init(|| {
            let mut payload = Vec::with_capacity(640 * 98);
            for row in 0..640 {
                for col in 0..96 {
                    let letter = b'a' + u8::try_from((row + col) % 26).unwrap_or(0);
                    payload.push(letter);
                }
                payload.extend_from_slice(b"\r\n");
            }
            payload
        })
        .as_slice()
}

fn csi_osc_heavy_payload() -> &'static [u8] {
    static PAYLOAD: OnceLock<Vec<u8>> = OnceLock::new();
    PAYLOAD
        .get_or_init(|| {
            let mut payload = Vec::with_capacity(64 * 1024);
            for idx in 0..1024 {
                let result = write!(
                    payload,
                    "\x1b[{};{}H\x1b[1;3{};4{}mX\x1b[0m\x1b[2K\x1b]0;round6-{idx}\x07\x1b]133;A\x07",
                    (idx % 40) + 1,
                    (idx % 80) + 1,
                    idx % 8,
                    idx % 8
                );
                if result.is_err() {
                    break;
                }
            }
            payload
        })
        .as_slice()
}

fn drive_parser(payload: &[u8]) -> usize {
    let mut parser = Parser::new();
    let mut count = 0usize;
    parser.parse(payload, |action| {
        count = count.wrapping_add(match action {
            Action::PrintString(text) => text.len(),
            Action::Print(_) => 1,
            Action::CSI(_) => 1,
            Action::OperatingSystemCommand(_) => 1,
            _ => 1,
        });
    });
    black_box(count)
}

fn drive_terminal(payload: &[u8]) -> usize {
    let mut term = make_terminal();
    term.advance_bytes(payload);
    let cursor = term.cursor_pos();
    black_box(cursor.x ^ usize::try_from(cursor.y).unwrap_or_default())
}

fn percentile_us(samples: &mut [u64], percentile: u8) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let percentile = u64::from(percentile.min(100));
    let len = u64::try_from(samples.len()).unwrap_or(u64::MAX);
    let target = len.saturating_mul(percentile).div_ceil(100).max(1);
    let index = usize::try_from(target - 1).unwrap_or(samples.len() - 1);
    samples
        .get(index.min(samples.len().saturating_sub(1)))
        .copied()
        .unwrap_or_default()
}

fn measure_payload(label: &str, lane: &str, payload: &[u8], drive: fn(&[u8]) -> usize) {
    for _ in 0..WARMUP_SAMPLES {
        black_box(drive(payload));
    }

    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        let started = Instant::now();
        black_box(drive(payload));
        samples.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    }

    let mut p50_samples = samples.clone();
    let p50_us = percentile_us(&mut p50_samples, 50);
    let mut p95_samples = samples.clone();
    let p95_us = percentile_us(&mut p95_samples, 95);
    let p99_us = percentile_us(&mut samples, 99);

    let row = json!({
        "schema_version": "ft.perf.evidence-sample.v1",
        "ts_ms": now_ms(),
        "claim_id": "ft-p4vzl.7",
        "metric_name": "term_parser_ab_latency",
        "metric_unit": "us",
        "sample_size": SAMPLE_COUNT,
        "workload_class": label,
        "lane": lane,
        "payload_bytes": payload.len(),
        "p50_us": p50_us,
        "p95_us": p95_us,
        "p99_us": p99_us,
        "source_bench": "frankenterm/term/benches/term_parser_ab.rs",
        "runner": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "cargo_target_dir": std::env::var("CARGO_TARGET_DIR").ok()
        },
        "tags": {
            "bead": "ft-p4vzl.7",
            "ev1_bulk_ascii_row_write": term_bulk_ascii_row_write_enabled(),
            "b5_ascii_cluster_run_append": term_ascii_cluster_run_append_enabled(),
            "d1_parser_print_batching": parser_print_batching_enabled(),
            "d2_parser_table_dispatch": env_flag_truthy(PARSER_TABLE_DISPATCH_ENV),
            "ft_moonshot_recommended": moonshot_recommended_enabled(),
            "ft_moonshot_all": std::env::var_os(MOONSHOT_ALL_ENV).is_some()
        }
    });

    let path = evidence_path();
    if let Err(err) = write_evidence_row(&path, &row) {
        eprintln!(
            "[BENCH] term_parser_ab evidence_write_error={err} evidence={}",
            path.display()
        );
    }
    println!(
        "[BENCH] term_parser_ab/{lane}/{label} p50_us={p50_us} p95_us={p95_us} p99_us={p99_us} evidence={}",
        path.display()
    );
}

fn write_evidence_row(path: &Path, row: &serde_json::Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{row}")?;
    Ok(())
}

fn evidence_path() -> PathBuf {
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    PathBuf::from(target_dir).join("criterion/term_parser_ab.jsonl")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

fn bench_term_parser_ab(c: &mut Criterion) {
    type ParserDriver = fn(&[u8]) -> usize;
    type ParserLane = (&'static str, ParserDriver);

    let workloads: &[(&str, &[u8])] = &[
        ("dense_ascii_rows", dense_ascii_rows_payload()),
        ("csi_osc_heavy", csi_osc_heavy_payload()),
    ];
    let lanes: &[ParserLane] = &[("parser", drive_parser), ("terminal", drive_terminal)];

    let mut group = c.benchmark_group("term_parser_ab");
    for (workload, payload) in workloads {
        group.throughput(Throughput::Bytes(
            u64::try_from(payload.len()).unwrap_or(u64::MAX),
        ));
        for (lane, drive) in lanes {
            group.bench_with_input(BenchmarkId::new(*lane, *workload), payload, |b, payload| {
                b.iter(|| black_box(drive(black_box(*payload))));
            });
        }
    }
    group.finish();
}

fn bench_config() -> Criterion {
    measure_payload(
        "dense_ascii_rows",
        "parser",
        dense_ascii_rows_payload(),
        drive_parser,
    );
    measure_payload(
        "dense_ascii_rows",
        "terminal",
        dense_ascii_rows_payload(),
        drive_terminal,
    );
    measure_payload(
        "csi_osc_heavy",
        "parser",
        csi_osc_heavy_payload(),
        drive_parser,
    );
    measure_payload(
        "csi_osc_heavy",
        "terminal",
        csi_osc_heavy_payload(),
        drive_terminal,
    );
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_term_parser_ab
);
criterion_main!(benches);
