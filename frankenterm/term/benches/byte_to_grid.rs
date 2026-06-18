use std::convert::TryFrom;
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};
use serde_json::json;

const SAMPLE_COUNT: usize = 101;
const WARMUP_SAMPLES: usize = 12;

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
            rows: 32,
            cols: 120,
            pixel_width: 960,
            pixel_height: 512,
            dpi: 96,
        },
        Arc::new(BenchTermConfig),
        "frankenterm-byte-to-grid",
        env!("CARGO_PKG_VERSION"),
        Box::new(Vec::new()),
    )
}

fn ascii_agent_payload() -> &'static [u8] {
    static PAYLOAD: OnceLock<Vec<u8>> = OnceLock::new();
    PAYLOAD
        .get_or_init(|| {
            let mut payload = Vec::with_capacity(128 * 96);
            for idx in 0..128 {
                writeln!(
                    payload,
                    "agent-{idx:04} compiling crate_{:02} status=ok tokens={} path=/tmp/frankenterm/workspace/src/lib.rs",
                    idx % 37,
                    100_000 + idx
                )
                .expect("write ascii agent payload");
            }
            payload
        })
        .as_slice()
}

fn wrapped_agent_payload() -> &'static [u8] {
    static PAYLOAD: OnceLock<Vec<u8>> = OnceLock::new();
    PAYLOAD
        .get_or_init(|| {
            let mut payload = Vec::with_capacity(64 * 240);
            for idx in 0..64 {
                writeln!(
                    payload,
                    "wrap-{idx:04}: {} {} {} {}",
                    "abcdefghijklmnopqrstuvwxyz0123456789",
                    "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
                    "diagnostics=warning,error,rate-limit,context-window",
                    "cwd=/Users/jemanuel/projects/frankenterm/frankenterm/term/src"
                )
                .expect("write wrapped agent payload");
            }
            payload
        })
        .as_slice()
}

fn mixed_control_payload() -> &'static [u8] {
    static PAYLOAD: OnceLock<Vec<u8>> = OnceLock::new();
    PAYLOAD
        .get_or_init(|| {
            let mut payload = Vec::with_capacity(96 * 96);
            for idx in 0..96 {
                write!(
                    payload,
                    "\x1b[3{}mphase-{idx:04}\x1b[0m\r\n\x1b[2Kcursor={} status=running\r\n",
                    idx % 8,
                    idx % 120
                )
                .expect("write mixed control payload");
            }
            payload
        })
        .as_slice()
}

fn drive_terminal(payload: &[u8]) -> usize {
    let mut term = make_terminal();
    term.advance_bytes(payload);
    black_box(term.cursor_pos().x)
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
    samples[index.min(samples.len() - 1)]
}

fn measure_payload(label: &str, payload: &[u8]) {
    for _ in 0..WARMUP_SAMPLES {
        black_box(drive_terminal(payload));
    }

    let mut samples = Vec::with_capacity(SAMPLE_COUNT);
    for _ in 0..SAMPLE_COUNT {
        let started = Instant::now();
        black_box(drive_terminal(payload));
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
        "claim_id": "ft-p8vls.swar_vte_scan",
        "metric_name": "advance_bytes_payload_latency",
        "metric_unit": "us",
        "sample_size": SAMPLE_COUNT,
        "workload_class": label,
        "payload_bytes": payload.len(),
        "p50_us": p50_us,
        "p95_us": p95_us,
        "p99_us": p99_us,
        "source_bench": "frankenterm/term/benches/byte_to_grid.rs",
        "runner": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "cargo_target_dir": std::env::var("CARGO_TARGET_DIR").ok()
        },
        "tags": {
            "bead": "ft-p8vls",
            "swar_printable_ascii_scan": !cfg!(feature = "bench-scalar-vte-scan"),
            "scalar_vte_scan_forced": cfg!(feature = "bench-scalar-vte-scan")
        }
    });

    let path = evidence_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create byte_to_grid evidence directory");
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open byte_to_grid evidence file");
    writeln!(file, "{row}").expect("write byte_to_grid evidence row");
    println!(
        "[BENCH] byte_to_grid/{label} p50_us={p50_us} p95_us={p95_us} p99_us={p99_us} evidence={}",
        path.display()
    );
}

fn evidence_path() -> PathBuf {
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    PathBuf::from(target_dir).join("criterion/byte_to_grid.jsonl")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

fn bench_byte_to_grid(c: &mut Criterion) {
    let mut group = c.benchmark_group("byte_to_grid");

    group.bench_function("ascii_agent_output", |b| {
        b.iter_batched(
            ascii_agent_payload,
            |payload| black_box(drive_terminal(payload)),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("wrapped_agent_output", |b| {
        b.iter_batched(
            wrapped_agent_payload,
            |payload| black_box(drive_terminal(payload)),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("mixed_control_output", |b| {
        b.iter_batched(
            mixed_control_payload,
            |payload| black_box(drive_terminal(payload)),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_config() -> Criterion {
    measure_payload("ascii_agent_output", ascii_agent_payload());
    measure_payload("wrapped_agent_output", wrapped_agent_payload());
    measure_payload("mixed_control_output", mixed_control_payload());
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_byte_to_grid
);
criterion_main!(benches);
