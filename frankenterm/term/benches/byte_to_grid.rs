use std::convert::TryFrom;
use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
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
                    "wrap-{idx:04}: abcdefghijklmnopqrstuvwxyz0123456789 \
                     ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 \
                     diagnostics=warning,error,rate-limit,context-window \
                     cwd=/Users/jemanuel/projects/frankenterm/frankenterm/term/src"
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

#[derive(Debug)]
struct ReflowTermConfig;

impl TerminalConfiguration for ReflowTermConfig {
    fn scrollback_size(&self) -> usize {
        // Keep every generated row even at the narrowest measured width.
        131_072
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

const REFLOW_WIDTHS: [usize; 4] = [61, 200, 79, 120];

fn reflow_size(cols: usize) -> TerminalSize {
    TerminalSize {
        rows: 32,
        cols,
        pixel_width: cols * 8,
        pixel_height: 512,
        dpi: 96,
    }
}

fn reflow_payload(logical_lines: usize, unicode: bool) -> (Vec<u8>, String) {
    let body = if unicode {
        "界👩‍💻e\u{301}אב".repeat(32)
    } else {
        "abcdefghijklmnopqrstuvwxyz0123456789".repeat(6)
    };
    let mut input = Vec::new();
    let mut expected = String::new();
    for index in 0..logical_lines {
        let row = format!("row-{index:05}:{body}");
        expected.push_str(&row);
        input.extend_from_slice(row.as_bytes());
        input.extend_from_slice(b"\r\n");
    }
    (input, expected)
}

fn make_reflow_terminal(input: &[u8]) -> Terminal {
    let mut term = Terminal::new(
        reflow_size(120),
        Arc::new(ReflowTermConfig),
        "frankenterm-reflow-cpu",
        env!("CARGO_PKG_VERSION"),
        Box::new(std::io::sink()),
    );
    term.advance_bytes(input);
    term
}

fn assert_reflow_content(term: &Terminal, cols: usize, expected: &str) {
    let mut actual = String::with_capacity(expected.len());
    let screen = term.screen();
    screen.with_phys_lines(0..screen.scrollback_rows(), |lines| {
        for line in lines {
            // The generated corpus contains no spaces. Ignore only terminal
            // padding; preserve combining characters, bidi text and ZWJ bytes.
            actual.extend(line.as_str().chars().filter(|ch| *ch != ' '));
        }
    });
    if actual != expected {
        let first_difference = actual
            .bytes()
            .zip(expected.bytes())
            .position(|(actual, expected)| actual != expected)
            .unwrap_or(actual.len().min(expected.len()));
        panic!(
            "reflow text mismatch at byte {first_difference}: actual_bytes={} expected_bytes={}",
            actual.len(),
            expected.len()
        );
    }
    let cursor = term.cursor_pos();
    // General terminal cursors may use x == cols for soft-wrap affinity.
    // This corpus ends in CRLF, so its stronger independent oracle is the
    // start of the blank final row, even after repeated width reversals.
    assert_eq!(cursor.x, 0, "hard-newline cursor moved at width {cols}");
    assert_eq!(cursor.y, 31, "hard-newline row moved at width {cols}");
}

fn verify_reflow_workload(label: &str, input: &[u8], expected: &str) {
    let mut term = make_reflow_terminal(input);
    assert_reflow_content(&term, 120, expected);
    // Independent input-text oracle runs at every width, outside Criterion's
    // timer. These real CPU milestones are not GUI or presented-frame timing.
    for cycle in 0..2 {
        for cols in REFLOW_WIDTHS {
            let started = Instant::now();
            term.resize(reflow_size(cols));
            let total_us = started.elapsed().as_micros();
            let viewport_near_us = term.screen().last_viewport_first_reflow_us();
            assert!(
                u128::from(viewport_near_us) <= total_us,
                "viewport batch timing must belong to this resize"
            );
            assert_reflow_content(&term, cols, expected);
            println!(
                "REFLOW_CPU_CALIBRATION {}",
                json!({
                    "workload": label,
                    "input_bytes": input.len(),
                    "cycle": cycle,
                    "cols": cols,
                    "resize_return_us": total_us,
                    "viewport_near_cpu_us": viewport_near_us,
                    "retained_physical_rows": term.screen().scrollback_rows(),
                    "content_oracle": "exact_generated_text_excluding_padding",
                    "presentation_proven": false,
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                })
            );
        }
    }
}

fn bench_resize_reflow(c: &mut Criterion) {
    let mut group = c.benchmark_group("reflow_cpu");
    for logical_lines in [1_000, 10_000] {
        for (kind, unicode) in [("ascii", false), ("unicode", true)] {
            let label = format!("{kind}_{logical_lines}");
            let (input, expected) = reflow_payload(logical_lines, unicode);
            verify_reflow_workload(&label, &input, &expected);
            group.throughput(Throughput::Elements(1));
            group.bench_function(format!("{label}/cold_resize"), |b| {
                b.iter_batched_ref(
                    || make_reflow_terminal(&input),
                    |term| {
                        term.resize(reflow_size(61));
                        black_box(term.cursor_pos());
                    },
                    BatchSize::PerIteration,
                );
            });
            group.throughput(Throughput::Elements(
                u64::try_from(REFLOW_WIDTHS.len()).expect("bounded width count"),
            ));
            group.bench_function(format!("{label}/repeated_width_cycle"), |b| {
                let mut term = make_reflow_terminal(&input);
                for cols in REFLOW_WIDTHS {
                    term.resize(reflow_size(cols));
                }
                b.iter(|| {
                    for cols in REFLOW_WIDTHS {
                        term.resize(reflow_size(cols));
                    }
                    black_box(term.cursor_pos());
                });
                assert_reflow_content(&term, 120, &expected);
            });
        }
    }
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
    targets = bench_byte_to_grid, bench_resize_reflow
);
criterion_main!(benches);
