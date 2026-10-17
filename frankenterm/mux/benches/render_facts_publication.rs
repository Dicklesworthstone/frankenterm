//! What publishing `PaneRenderFacts` after every applied batch costs a local
//! pane's output path (ft-yccm0.2.2.1 acceptance: under 1%).
//!
//! ```text
//! cargo bench -p mux --features bench-hooks --bench render_facts_publication
//! ```
//!
//! Workload: the M.1 T0 corpus (`color_emoji_random`, the operator's primary
//! test), produced by the M.1 generator itself
//! (`frankenterm/term/benches/ingest/corpus.rs`, default seed), cut into reads
//! of 4 KiB (one publication per small PTY read: the worst case) and 128 KiB
//! (the M.1 chunk), and applied to a fresh 80x120 pane with 3500 rows of
//! scrollback (the M.1 geometry) through two paths:
//! - `actions`: batches parsed outside the timed region, through
//!   `LocalPane::perform_actions`;
//! - `fused`: raw reads through the fused live path the mux parse thread takes
//!   for bulk output such as T0 (`feed_fused`, ft-yccm0.3.2.1), with one parser
//!   for the whole corpus. Absent with `disruptor-pane-io`.
//!
//! Arms: `publish` is the production path. `skip` sets the bench-only
//! `LocalPane::bench_skip_render_facts_publication` hook, which returns from
//! the publication before it captures anything. Before anything is timed, both
//! arms must leave identical terminal state on each path (seqno, cursor,
//! dimensions, title and every scrollback and screen line); the bench panics
//! otherwise.
//!
//! Output:
//! - Criterion times each arm as
//!   `render_facts_publication/{publish,skip}/{actions,fused}_batch_{4,128}KiB`.
//! - Because criterion runs the arms one after the other, a paired ABBA run
//!   also prints one `[BENCH]` JSON line per path and read size: the median and
//!   CV of each arm, the overhead percentage and a verdict (`pass` under 1%,
//!   `fail`, or `NO_ADMISSIBLE_RATIO` when either arm's CV exceeds 5%).
//!
//! Environment:
//! - `FT_RENDER_FACTS_PAIRS`: ABBA pairs (default 10).
//! - `FT_RENDER_FACTS_CORPUS_BYTES`: corpus bytes per run (default 4 MiB).
//!
//! Under `cargo test --benches` (no `--bench` argument) it checks state
//! equality on a 256 KiB corpus and skips the paired run.

#[path = "../../term/benches/ingest/corpus.rs"]
#[allow(dead_code)]
mod corpus;

use anyhow::Error;
use criterion::{criterion_group, BatchSize, Criterion, Throughput};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};
use mux::domain::DomainId;
use mux::localpane::LocalPane;
use mux::pane::{Pane, PaneId};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use std::hint::black_box;
use std::io::{Cursor, Read, Result as IoResult, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::escape::parser::Parser;
use termwiz::escape::Action;
use termwiz::surface::{Line, SequenceNo};

const BATCH_SIZES: [usize; 2] = [4 * 1024, 128 * 1024];
const BENCH_CORPUS_BYTES: usize = 4 * 1024 * 1024;
const TEST_CORPUS_BYTES: usize = 256 * 1024;
const DEFAULT_PAIRS: usize = 10;
/// The acceptance threshold, and the noise level above which a paired
/// comparison cannot resolve it.
const MAX_OVERHEAD_PCT: f64 = 1.0;
const MAX_CV_PCT: f64 = 5.0;

#[derive(Debug)]
struct BenchTermConfig;

impl TerminalConfiguration for BenchTermConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }

    fn scrollback_size(&self) -> usize {
        3500
    }
}

#[derive(Debug, Clone)]
struct BenchChild;

impl ChildKiller for BenchChild {
    fn kill(&mut self) -> IoResult<()> {
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}

impl Child for BenchChild {
    fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
        Ok(None)
    }

    fn wait(&mut self) -> IoResult<ExitStatus> {
        Ok(ExitStatus::with_exit_code(0))
    }

    fn process_id(&self) -> Option<u32> {
        None
    }
}

struct BenchMasterPty;

impl MasterPty for BenchMasterPty {
    fn resize(&self, _size: PtySize) -> Result<(), Error> {
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, Error> {
        Ok(PtySize::default())
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, Error> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, Error> {
        Ok(Box::new(Vec::<u8>::new()))
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        None
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// A fresh pane in the M.1 geometry; `publish` false sets the skip hook.
fn make_pane(publish: bool) -> LocalPane {
    let terminal = Terminal::new(
        TerminalSize {
            rows: 80,
            cols: 120,
            pixel_width: 960,
            pixel_height: 1280,
            dpi: 96,
        },
        Arc::new(BenchTermConfig),
        "frankenterm-mux-render-facts-bench",
        env!("CARGO_PKG_VERSION"),
        Box::new(Vec::<u8>::new()),
    );
    let pane = LocalPane::new(
        9002 as PaneId,
        terminal,
        Box::new(BenchChild),
        Box::new(BenchMasterPty),
        Box::new(Vec::<u8>::new()),
        1 as DomainId,
        [7; 16],
        "bench-render-facts".to_string(),
    );
    pane.bench_skip_render_facts_publication(!publish);
    pane
}

/// The corpus cut into `batch_bytes` reads, parsed by one parser so escapes
/// torn across a boundary complete in the next batch, as on the parse thread.
fn parse_batches(corpus: &[u8], batch_bytes: usize) -> Vec<Vec<Action>> {
    let mut parser = Parser::new();
    corpus
        .chunks(batch_bytes)
        .map(|chunk| parser.parse_as_vec(chunk))
        .filter(|actions| !actions.is_empty())
        .collect()
}

/// How a run hands the corpus to the pane.
#[derive(Clone, Copy, Debug)]
enum Path {
    /// Pre-parsed batches through `LocalPane::perform_actions`.
    Actions,
    /// Raw reads through the fused live path (`feed_fused`).
    #[cfg(not(feature = "disruptor-pane-io"))]
    Fused,
}

impl Path {
    #[cfg(not(feature = "disruptor-pane-io"))]
    const ALL: &'static [Path] = &[Path::Actions, Path::Fused];
    #[cfg(feature = "disruptor-pane-io")]
    const ALL: &'static [Path] = &[Path::Actions];

    fn name(self) -> &'static str {
        match self {
            Path::Actions => "actions",
            #[cfg(not(feature = "disruptor-pane-io"))]
            Path::Fused => "fused",
        }
    }
}

/// One path over the corpus at one read size.
struct Workload<'a> {
    path: Path,
    corpus: &'a [u8],
    batch_bytes: usize,
    /// The `Actions` path's input, parsed once.
    batches: Vec<Vec<Action>>,
}

impl<'a> Workload<'a> {
    fn new(path: Path, corpus: &'a [u8], batch_bytes: usize) -> Self {
        let batches = match path {
            Path::Actions => parse_batches(corpus, batch_bytes),
            #[cfg(not(feature = "disruptor-pane-io"))]
            Path::Fused => Vec::new(),
        };
        Self {
            path,
            corpus,
            batch_bytes,
            batches,
        }
    }

    fn label(&self) -> String {
        format!("{}_batch_{}KiB", self.path.name(), self.batch_bytes / 1024)
    }

    /// The untimed per-run input: a copy of the batches `perform_actions`
    /// consumes (the fused path reads the corpus in place).
    fn input(&self) -> Vec<Vec<Action>> {
        self.batches.clone()
    }

    fn reads(&self) -> usize {
        self.corpus.chunks(self.batch_bytes).count()
    }

    fn run(&self, pane: &LocalPane, input: Vec<Vec<Action>>) {
        match self.path {
            Path::Actions => {
                for actions in input {
                    pane.perform_actions(actions)
                        .expect("bench pane admits every batch");
                }
            }
            #[cfg(not(feature = "disruptor-pane-io"))]
            Path::Fused => {
                // As the parse thread does: one parser for the stream, and any
                // diverted action goes through the admitted path (T0 has none).
                let mut parser = Parser::new();
                let mut diverted = Vec::new();
                for chunk in self.corpus.chunks(self.batch_bytes) {
                    pane.bench_feed_fused(&mut parser, chunk, &mut diverted)
                        .expect("bench pane admits every read");
                    if !diverted.is_empty() {
                        pane.perform_actions(std::mem::take(&mut diverted))
                            .expect("bench pane admits diverted actions");
                    }
                }
            }
        }
    }
}

#[derive(Debug, PartialEq)]
struct PaneState {
    seqno: SequenceNo,
    cursor: StableCursorPosition,
    dimensions: RenderableDimensions,
    title: String,
    lines: Vec<Line>,
}

fn capture_state(pane: &LocalPane) -> PaneState {
    let dimensions = pane.get_dimensions();
    let end = dimensions.physical_top + dimensions.viewport_rows as isize;
    let (_, lines) = pane.get_lines(dimensions.scrollback_top..end);
    PaneState {
        seqno: pane.get_current_seqno(),
        cursor: pane.get_cursor_position(),
        dimensions,
        title: pane.get_title(),
        lines,
    }
}

/// Both arms must leave the same terminal behind: publication only reads.
fn assert_arms_leave_identical_state(workload: &Workload<'_>) {
    let publish = make_pane(true);
    workload.run(&publish, workload.input());
    let skip = make_pane(false);
    workload.run(&skip, workload.input());
    let published = capture_state(&publish);
    let skipped = capture_state(&skip);
    assert!(
        !published.lines.is_empty(),
        "the corpus must leave rows behind"
    );
    assert!(
        published == skipped,
        "{}: publishing render facts changed terminal state",
        workload.label()
    );
    // The publish arm really published; the skip arm kept its initial facts.
    assert_eq!(publish.render_facts().seqno, published.seqno);
    assert!(skip.render_facts().seqno < skipped.seqno);
}

fn timed_run(workload: &Workload<'_>, publish: bool) -> Duration {
    let pane = make_pane(publish);
    let input = workload.input();
    let started = Instant::now();
    workload.run(&pane, black_box(input));
    let elapsed = started.elapsed();
    black_box(pane.get_current_seqno());
    elapsed
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

fn cv_pct(values: &[f64]) -> f64 {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    if values.len() < 2 || mean == 0.0 {
        return 0.0;
    }
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
    variance.sqrt() / mean * 100.0
}

fn load_average() -> String {
    let text = std::fs::read_to_string("/proc/loadavg").ok().or_else(|| {
        std::process::Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
    });
    text.map_or_else(|| "unknown".to_string(), |text| text.trim().to_string())
}

/// ABBA-paired runs of both arms; prints one `[BENCH]` JSON line.
fn paired_overhead(workload: &Workload<'_>, pairs: usize) {
    let load_before = load_average();
    let (mut on, mut off) = (Vec::new(), Vec::new());
    // Warm both arms once, untimed.
    timed_run(workload, true);
    timed_run(workload, false);
    for pair in 0..pairs {
        let order = if pair % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        };
        for publish in order {
            let seconds = timed_run(workload, publish).as_secs_f64();
            if publish {
                on.push(seconds);
            } else {
                off.push(seconds);
            }
        }
    }
    let (on_cv, off_cv) = (cv_pct(&on), cv_pct(&off));
    let mut paired: Vec<f64> = on
        .iter()
        .zip(&off)
        .map(|(on, off)| (on / off - 1.0) * 100.0)
        .collect();
    let paired_median_pct = median(&mut paired);
    let (on_median, off_median) = (median(&mut on), median(&mut off));
    let overhead_pct = (on_median / off_median - 1.0) * 100.0;
    let verdict = if on_cv > MAX_CV_PCT || off_cv > MAX_CV_PCT {
        format!(
            "NO_ADMISSIBLE_RATIO (CV publish {:.2}% / skip {:.2}% > {}%)",
            on_cv, off_cv, MAX_CV_PCT
        )
    } else if overhead_pct < MAX_OVERHEAD_PCT {
        "pass".to_string()
    } else {
        "fail".to_string()
    };
    println!(
        "[BENCH] {{\"bench\":\"render_facts_publication\",\"bead\":\"ft-yccm0.2.2.1\",\
         \"path\":\"{}\",\"corpus\":\"color_emoji_random\",\"corpus_bytes\":{},\"seed\":{},\
         \"batch_bytes\":{},\"reads\":{},\"pairs\":{},\"publish_median_ms\":{:.3},\
         \"skip_median_ms\":{:.3},\"publish_cv_pct\":{:.2},\"skip_cv_pct\":{:.2},\
         \"overhead_pct\":{:.3},\"paired_median_overhead_pct\":{:.3},\"threshold_pct\":{},\
         \"verdict\":\"{}\",\"load_avg_before\":\"{}\",\"load_avg_after\":\"{}\"}}",
        workload.path.name(),
        workload.corpus.len(),
        corpus::DEFAULT_SEED,
        workload.batch_bytes,
        workload.reads(),
        pairs,
        on_median * 1e3,
        off_median * 1e3,
        on_cv,
        off_cv,
        overhead_pct,
        paired_median_pct,
        MAX_OVERHEAD_PCT,
        verdict,
        load_before,
        load_average()
    );
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn bench_mode() -> bool {
    std::env::args().any(|arg| arg == "--bench")
}

fn corpus_bytes() -> usize {
    if bench_mode() {
        env_usize("FT_RENDER_FACTS_CORPUS_BYTES", BENCH_CORPUS_BYTES)
    } else {
        TEST_CORPUS_BYTES
    }
}

fn t0_corpus() -> Vec<u8> {
    corpus::Corpus::ColorEmojiRandom.generate(corpus_bytes(), corpus::DEFAULT_SEED)
}

fn bench_render_facts_publication(c: &mut Criterion) {
    let corpus = t0_corpus();
    let mut group = c.benchmark_group("render_facts_publication");
    group.throughput(Throughput::Bytes(corpus.len() as u64));
    for &path in Path::ALL {
        for batch_bytes in BATCH_SIZES {
            let workload = Workload::new(path, &corpus, batch_bytes);
            for (arm, publish) in [("publish", true), ("skip", false)] {
                let name = format!("{}/{}", arm, workload.label());
                group.bench_function(name, |b| {
                    b.iter_batched(
                        || (make_pane(publish), workload.input()),
                        |(pane, input)| {
                            workload.run(&pane, black_box(input));
                            pane
                        },
                        BatchSize::PerIteration,
                    );
                });
            }
        }
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_render_facts_publication
}

fn main() {
    let corpus = t0_corpus();
    let pairs = env_usize("FT_RENDER_FACTS_PAIRS", DEFAULT_PAIRS);
    for &path in Path::ALL {
        for batch_bytes in BATCH_SIZES {
            let workload = Workload::new(path, &corpus, batch_bytes);
            assert_arms_leave_identical_state(&workload);
            if bench_mode() {
                paired_overhead(&workload, pairs);
            }
        }
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
}
