//! M6 evidence harness — concurrent search-while-streaming at high pane count.
//!
//! Bead: ft-round5-gauntlet-lw0s7.11 (R5-E1).
//!
//! ## What this measures and why it exists
//!
//! M6 ("persistent COW scrollback grid") was *deferred, not attempted* in
//! round 4. Its retry-condition predicate (Form 1, see
//! `docs/perf-ledger/round4-negative-results.md`) is:
//!
//! > retry only if a profiler attributes a clearly-above-noise share to
//! > scrollback **read/render lock contention (or clone cost)** on a
//! > **concurrent search-while-streaming** workload at **high pane count**.
//!
//! No such evidence harness existed. This is it. It models the *baseline
//! comparator* named in that ledger entry — a `VecDeque` hot tier behind a
//! per-pane `Mutex`, with concurrent readers either (a) scanning **under the
//! lock** or (b) **cloning under the lock then scanning** the snapshot — and
//! drives it at 100–200 panes while background writer threads stream captured
//! output into the same per-pane scrollbacks (the real `Arc<Mutex<…>>`
//! sharing pattern). It then reports whether the reader-side lock-wait tail is
//! above the no-contention noise floor.
//!
//! M6's whole pitch is that a copy-on-write rope makes the search snapshot
//! O(1) and lock-free, eliminating *both* the reader's lock-wait *and* the
//! clone cost. So this harness instruments three quantities per strategy and
//! pane count, contended vs. quiescent:
//!
//! * **`reader_lock_wait_ns`** — time the search/render reader blocks acquiring
//!   each per-pane `Mutex`. This is the literal "read/render lock contention"
//!   in the predicate. The contended-vs-baseline delta is the signal.
//! * **`reader_lock_hold_ns`** — how long the reader holds the lock (the scan
//!   for `scan_under_lock`, the deep clone for `clone_then_scan`). For
//!   `clone_then_scan` this *is* the "clone cost" in the predicate, and it also
//!   proxies the capture-side stall a writer eats while the reader holds.
//! * the Criterion wall-clock of a full-fleet search pass, contended vs.
//!   quiescent (the gross A/B the orchestrator reads from Criterion directly).
//!
//! ## How to adjudicate (E2)
//!
//! Like M9 / S3-FIFO, the *win metric here is not nanoseconds of the bench
//! itself* — it is the lock-wait/clone distribution. Even if the Criterion
//! wall-clock A/B is flat, this harness emits, to stdout and to
//! `target/criterion/m6-lock-wait-evidence.jsonl`:
//!
//! * one `[M6-EVIDENCE]` row per (pane count, strategy, contended?) with the
//!   full `Distribution` (p50/p95/p99/p99.9/max) of lock-wait and lock-hold;
//! * one `[M6-VERDICT]` row per (pane count, strategy) pairing the contended
//!   run against its quiescent baseline, with an *advisory* `above_noise`
//!   boolean computed from a documented, conservative threshold.
//!
//! The boolean is advisory only — the orchestrator owns the M6 keep/kill call.
//! The harness's job is to make the evidence legible.
//!
//! Build/proof: `cargo bench --no-run -p frankenterm-core --bench
//! m6_search_while_streaming` (no feature flags required).

use std::collections::VecDeque;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde::Serialize;

use frankenterm_core::bench_stats::Distribution;

mod bench_common;

// ── Workload parameters ──────────────────────────────────────────────

/// High pane counts — the "high pane count" half of the retry predicate.
const PANE_COUNTS: &[usize] = &[100, 200];

/// Lines in each pane's hot tier. Sized so a full-fleet scan does real work
/// (200 panes × 1500 lines = 300k `contains()` per pass) while keeping the
/// resident set bounded (~300k short Strings ≈ tens of MB).
const HOT_TIER_LINES: usize = 1_500;

/// Background streaming writers. Enough to keep real append pressure on the
/// per-pane mutexes at high pane count without pegging an entire machine.
const WRITER_THREADS: usize = 6;

/// Search needle. Embedded in ~1/97 of generated lines (see [`gen_line`]) so
/// the scan exercises the match path, not just rejection.
const QUERY: &str = "M6_NEEDLE";

/// Soft cap on accumulated per-strategy lock-wait/-hold samples so memory
/// stays bounded even if Criterion runs many iterations.
const SAMPLE_CAP: usize = 300_000;

// ── Verdict thresholds (documented, conservative) ────────────────────

/// Contended p95 lock-wait must exceed the quiescent p95 by at least this
/// factor before we call the contention "above noise". 3× separates a genuine
/// blocking tail from ordinary uncontended-mutex jitter.
const ABOVE_NOISE_RATIO: f64 = 3.0;

/// …AND it must exceed this absolute floor. 50µs is ~0.3% of a 16ms (60fps)
/// frame budget; below it, even a large *ratio* is operationally irrelevant
/// to render/search latency, so we do not flag it.
const ABOVE_NOISE_ABS_NS: f64 = 50_000.0;

const EVIDENCE_PATH: &str = "target/criterion/m6-lock-wait-evidence.jsonl";

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "search_pass/scan_under_lock",
        budget: "full-fleet substring scan under per-pane lock (gross A/B comparator)",
    },
    bench_common::BenchBudget {
        name: "search_pass/clone_then_scan",
        budget: "snapshot-clone under lock + scan outside lock (clone-cost A/B comparator)",
    },
    bench_common::BenchBudget {
        name: "reader_lock_wait_ns",
        budget: "M6 signal: contended reader lock-wait p95 vs quiescent noise floor",
    },
];

// ── Scrollback model — the M6 baseline comparator ────────────────────
//
// `VecDeque` hot tier behind a `Mutex`, exactly as named in the negative-
// results ledger. The fleet is `Vec<Arc<Mutex<PaneScrollback>>>`, mirroring
// the per-pane `Arc<Mutex<Terminal>>` sharing the real mux uses.

#[derive(Clone)]
struct ScrollLine {
    seq: u64,
    text: String,
}

struct PaneScrollback {
    lines: VecDeque<ScrollLine>,
    cap: usize,
    next_seq: u64,
}

impl PaneScrollback {
    fn with_capacity(cap: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(cap),
            cap,
            next_seq: 0,
        }
    }

    /// Append one captured line, evicting the oldest when over the hot-tier
    /// capacity — the ring behaviour of the real hot scrollback tier.
    fn append(&mut self, text: String) {
        if self.lines.len() >= self.cap {
            self.lines.pop_front();
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.lines.push_back(ScrollLine { seq, text });
    }

    /// Count matching lines (the "search read" workload), scanned in place.
    fn search(&self, query: &str) -> usize {
        self.lines.iter().filter(|l| l.text.contains(query)).count()
    }

    /// Deep snapshot of the hot tier — clones every line `String`. This is the
    /// O(n) clone cost M6's copy-on-write rope would collapse to O(1).
    fn snapshot(&self) -> VecDeque<ScrollLine> {
        self.lines.clone()
    }
}

/// Deterministic synthetic captured line. Varied content so the scan can't be
/// constant-folded; `QUERY` embedded in ~1/97 of lines for a realistic
/// low-but-nonzero match rate.
fn gen_line(seq: u64) -> String {
    let body = match seq % 8 {
        0 => format!("error: compile failed in module_{seq}"),
        1 => format!("[####------] {}% complete seq={seq}", seq % 100),
        2 => format!("Using tool: Bash step {seq}"),
        3 => format!("test result: {} passed; {} failed", seq % 50, seq % 4),
        4 => format!("Compiling crate_{} v0.1.0", seq % 200),
        5 => format!("\u{1b}[2K\u{1b}[1A repaint seq={seq}"),
        6 => format!("INFO worker {} heartbeat ts={seq}", seq % 32),
        _ => format!("regular streamed output line number {seq} with filler text"),
    };
    if seq % 97 == 0 {
        format!("{QUERY} {body}")
    } else {
        body
    }
}

/// Spread writes across the fleet pseudo-randomly (Knuth multiplicative hash)
/// so a writer's target pane is decorrelated from the reader's in-order sweep
/// — maximising realistic collision spread without `rand`.
#[inline]
fn pane_for_seq(seq: u64, panes: usize) -> usize {
    (seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % panes
}

type Fleet = Vec<Arc<Mutex<PaneScrollback>>>;

/// Build a fleet of `panes` panes, each pre-filled to a full hot tier so the
/// very first search pass scans a warm scrollback.
fn build_fleet(panes: usize) -> Fleet {
    (0..panes)
        .map(|p| {
            let mut sb = PaneScrollback::with_capacity(HOT_TIER_LINES);
            let base = (p as u64).wrapping_mul(HOT_TIER_LINES as u64);
            for i in 0..HOT_TIER_LINES {
                sb.append(gen_line(base + i as u64));
            }
            Arc::new(Mutex::new(sb))
        })
        .collect()
}

/// Spawn the streaming-capture writers. Each thread tight-loops appending
/// generated lines into hash-selected panes until `stop` flips, modelling many
/// panes streaming output concurrently with the search reads.
fn spawn_writers(fleet: &Fleet, stop: Arc<AtomicBool>, threads: usize) -> Vec<JoinHandle<()>> {
    let panes = fleet.len();
    (0..threads)
        .map(|w| {
            let fleet = fleet.clone();
            let stop = Arc::clone(&stop);
            // Disjoint starting seq per writer so content (and pane targeting)
            // does not lock-step across writer threads.
            let mut seq = (HOT_TIER_LINES as u64)
                .wrapping_mul(panes as u64)
                .wrapping_add((w as u64).wrapping_mul(0x1_0000_0000));
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let idx = pane_for_seq(seq, panes);
                    let text = gen_line(seq);
                    if let Ok(mut guard) = fleet[idx].lock() {
                        guard.append(text);
                    }
                    seq = seq.wrapping_add(1);
                }
            })
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Strategy {
    /// Hold the per-pane lock for the whole scan.
    ScanUnderLock,
    /// Clone the hot tier under the lock, scan the snapshot lock-free.
    CloneThenScan,
}

impl Strategy {
    fn as_str(self) -> &'static str {
        match self {
            Strategy::ScanUnderLock => "scan_under_lock",
            Strategy::CloneThenScan => "clone_then_scan",
        }
    }
}

/// Clean (uninstrumented) full-fleet search pass — used for the Criterion
/// wall-clock A/B so per-lock timing calls don't perturb the headline number.
fn search_pass_clean(fleet: &Fleet, strat: Strategy) -> usize {
    let mut matches = 0usize;
    for pane in fleet {
        match strat {
            Strategy::ScanUnderLock => {
                let guard = pane.lock().unwrap();
                matches += guard.search(QUERY);
            }
            Strategy::CloneThenScan => {
                let snap = {
                    let guard = pane.lock().unwrap();
                    guard.snapshot()
                };
                matches += snap.iter().filter(|l| l.text.contains(QUERY)).count();
            }
        }
    }
    matches
}

struct PassResult {
    duration: Duration,
    waits_ns: Vec<f64>,
    holds_ns: Vec<f64>,
}

/// Instrumented full-fleet search pass — records per-pane lock-wait and
/// lock-hold (= scan time, or clone time for `clone_then_scan`). The two
/// `Instant::now()` calls per pane are the deliberate measurement cost of this
/// evidence group (the clean A/B above does not pay them).
fn search_pass_instrumented(fleet: &Fleet, strat: Strategy) -> PassResult {
    let mut waits = Vec::with_capacity(fleet.len());
    let mut holds = Vec::with_capacity(fleet.len());
    let mut matches = 0usize;
    let pass_start = Instant::now();
    for pane in fleet {
        match strat {
            Strategy::ScanUnderLock => {
                let t = Instant::now();
                let guard = pane.lock().unwrap();
                let wait = t.elapsed();
                let h = Instant::now();
                matches += guard.search(QUERY);
                let hold = h.elapsed();
                drop(guard);
                waits.push(wait.as_nanos() as f64);
                holds.push(hold.as_nanos() as f64);
            }
            Strategy::CloneThenScan => {
                let t = Instant::now();
                let guard = pane.lock().unwrap();
                let wait = t.elapsed();
                let h = Instant::now();
                let snap = guard.snapshot();
                let hold = h.elapsed(); // clone cost
                drop(guard);
                matches += snap.iter().filter(|l| l.text.contains(QUERY)).count();
                waits.push(wait.as_nanos() as f64);
                holds.push(hold.as_nanos() as f64);
            }
        }
    }
    let duration = pass_start.elapsed();
    black_box(matches);
    PassResult {
        duration,
        waits_ns: waits,
        holds_ns: holds,
    }
}

// ── Criterion group A: gross wall-clock A/B (contended vs quiescent) ──

fn bench_search_pass(c: &mut Criterion) {
    let mut group = c.benchmark_group("m6_search_while_streaming");
    group.sample_size(15);
    group.measurement_time(Duration::from_secs(3));

    for &panes in PANE_COUNTS {
        group.throughput(Throughput::Elements((panes * HOT_TIER_LINES) as u64));
        for &strat in &[Strategy::ScanUnderLock, Strategy::CloneThenScan] {
            for &contended in &[false, true] {
                let arm = if contended { "contended" } else { "quiescent" };
                let id = BenchmarkId::new(strat.as_str(), format!("{arm}/{panes}p"));
                group.bench_function(id, |b| {
                    let fleet = build_fleet(panes);
                    let stop = Arc::new(AtomicBool::new(false));
                    let writers = if contended {
                        spawn_writers(&fleet, Arc::clone(&stop), WRITER_THREADS)
                    } else {
                        Vec::new()
                    };
                    b.iter(|| black_box(search_pass_clean(&fleet, strat)));
                    stop.store(true, Ordering::Relaxed);
                    for h in writers {
                        let _ = h.join();
                    }
                });
            }
        }
    }
    group.finish();
}

// ── Criterion group B: instrumented lock-wait / clone-cost evidence ───

fn bench_lock_wait_evidence(c: &mut Criterion) {
    let mut group = c.benchmark_group("m6_lock_wait_evidence");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(3));

    let mut rows: Vec<EvidenceRow> = Vec::new();

    for &panes in PANE_COUNTS {
        for &strat in &[Strategy::ScanUnderLock, Strategy::CloneThenScan] {
            for &contended in &[false, true] {
                let arm = if contended { "contended" } else { "quiescent" };
                let id = BenchmarkId::new(strat.as_str(), format!("{arm}/{panes}p"));

                let mut waits_acc: Vec<f64> = Vec::new();
                let mut holds_acc: Vec<f64> = Vec::new();

                group.bench_function(id, |b| {
                    let fleet = build_fleet(panes);
                    let stop = Arc::new(AtomicBool::new(false));
                    let writers = if contended {
                        spawn_writers(&fleet, Arc::clone(&stop), WRITER_THREADS)
                    } else {
                        Vec::new()
                    };
                    b.iter_custom(|iters| {
                        let mut elapsed = Duration::ZERO;
                        for _ in 0..iters {
                            let pass = search_pass_instrumented(&fleet, strat);
                            elapsed += pass.duration;
                            if waits_acc.len() < SAMPLE_CAP {
                                waits_acc.extend(pass.waits_ns);
                            }
                            if holds_acc.len() < SAMPLE_CAP {
                                holds_acc.extend(pass.holds_ns);
                            }
                        }
                        elapsed
                    });
                    stop.store(true, Ordering::Relaxed);
                    for h in writers {
                        let _ = h.join();
                    }
                });

                rows.push(EvidenceRow::summarize(
                    panes, strat, contended, &waits_acc, &holds_acc,
                ));
            }
        }
    }
    group.finish();

    emit_evidence(&rows);
}

// ── Evidence rows + emission ─────────────────────────────────────────

#[derive(Serialize)]
struct EvidenceRow {
    test_type: &'static str,
    schema: &'static str,
    panes: usize,
    strategy: &'static str,
    contended: bool,
    writer_threads: usize,
    hot_tier_lines: usize,
    query: &'static str,
    reader_lock_wait_ns: Option<Distribution>,
    reader_lock_hold_ns: Option<Distribution>,
}

impl EvidenceRow {
    fn summarize(
        panes: usize,
        strat: Strategy,
        contended: bool,
        waits: &[f64],
        holds: &[f64],
    ) -> Self {
        Self {
            test_type: "m6-lock-wait-evidence",
            schema: "1",
            panes,
            strategy: strat.as_str(),
            contended,
            writer_threads: if contended { WRITER_THREADS } else { 0 },
            hot_tier_lines: HOT_TIER_LINES,
            query: QUERY,
            reader_lock_wait_ns: Distribution::from_samples(waits),
            reader_lock_hold_ns: Distribution::from_samples(holds),
        }
    }
}

#[derive(Serialize)]
struct VerdictRow {
    test_type: &'static str,
    schema: &'static str,
    panes: usize,
    strategy: &'static str,
    baseline_wait_p95_ns: f64,
    contended_wait_p95_ns: f64,
    wait_p95_ratio: f64,
    contended_wait_max_ns: f64,
    baseline_hold_p95_ns: f64,
    contended_hold_p95_ns: f64,
    /// Advisory only. The orchestrator owns the M6 keep/kill call.
    above_noise: bool,
    threshold_note: &'static str,
}

fn pct(dist: &Option<Distribution>, q: f64) -> f64 {
    dist.as_ref()
        .and_then(|d| {
            d.percentiles
                .iter()
                .find(|p| (p.q - q).abs() < 1e-9)
                .map(|p| p.value)
        })
        .unwrap_or(f64::NAN)
}

fn max_of(dist: &Option<Distribution>) -> f64 {
    dist.as_ref().map(|d| d.max).unwrap_or(f64::NAN)
}

fn emit_evidence(rows: &[EvidenceRow]) {
    for row in rows {
        if let Ok(json) = serde_json::to_string(row) {
            println!("[M6-EVIDENCE] {json}");
            append_jsonl(EVIDENCE_PATH, &json);
        }
    }

    // Pair each contended run with its quiescent baseline and spell out the
    // above-noise verdict so the E2 decision can be read off directly.
    for &panes in PANE_COUNTS {
        for &strat in &[Strategy::ScanUnderLock, Strategy::CloneThenScan] {
            let s = strat.as_str();
            let base = rows
                .iter()
                .find(|r| r.panes == panes && r.strategy == s && !r.contended);
            let cont = rows
                .iter()
                .find(|r| r.panes == panes && r.strategy == s && r.contended);
            let (Some(base), Some(cont)) = (base, cont) else {
                continue;
            };

            let base_wait_p95 = pct(&base.reader_lock_wait_ns, 0.95);
            let cont_wait_p95 = pct(&cont.reader_lock_wait_ns, 0.95);
            let ratio = if base_wait_p95 > 0.0 {
                cont_wait_p95 / base_wait_p95
            } else {
                f64::INFINITY
            };
            let above_noise =
                ratio >= ABOVE_NOISE_RATIO && cont_wait_p95 >= ABOVE_NOISE_ABS_NS;

            let verdict = VerdictRow {
                test_type: "m6-lock-wait-verdict",
                schema: "1",
                panes,
                strategy: s,
                baseline_wait_p95_ns: base_wait_p95,
                contended_wait_p95_ns: cont_wait_p95,
                wait_p95_ratio: ratio,
                contended_wait_max_ns: max_of(&cont.reader_lock_wait_ns),
                baseline_hold_p95_ns: pct(&base.reader_lock_hold_ns, 0.95),
                contended_hold_p95_ns: pct(&cont.reader_lock_hold_ns, 0.95),
                above_noise,
                threshold_note: "above_noise = contended p95 wait >= 3x baseline p95 AND >= 50us (advisory)",
            };
            if let Ok(json) = serde_json::to_string(&verdict) {
                println!("[M6-VERDICT] {json}");
                append_jsonl(EVIDENCE_PATH, &json);
            }
        }
    }
}

fn append_jsonl(path: &str, line: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::Path;
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("m6_search_while_streaming", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_search_pass, bench_lock_wait_evidence
);
criterion_main!(benches);
