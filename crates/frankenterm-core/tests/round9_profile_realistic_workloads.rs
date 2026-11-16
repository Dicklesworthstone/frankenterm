//! Round-9 B0-CORRECTION profile sweep — beads `ft-zhj63` + `ft-ui1xn`.
//!
//! ## Why this file exists (ft-zhj63)
//!
//! Round-6/round-7 B0 ranked `scan_pipeline::ScanPipeline::process` as the #1
//! per-delta hot frame (72% / 55% of fleet self-time). That frame is **dead
//! code**: `ScanPipeline` / `ChunkedPipelineState` / `TriggerScanner` have ZERO
//! non-test/non-bench production callers (round-6 grep proof `df794dca3`,
//! re-verified for round-9). The real per-capture-delta production frame is
//! [`PatternEngine::detect_with_context`], driven per pane segment from
//! `runtime.rs:3748`. So the round-6/7 192-deltas/s slot was attributed to a
//! phantom; this harness puts the LIVE `detect_with_context` frame in that slot
//! and re-derives the denominator shares against the other live frames
//! (`redactor.redact`, `bocpd.observe_text_chunk`, `events.publish`, WAL
//! recovery). `scan_pipeline.process` is intentionally NOT measured here — it is
//! not on any production path.
//!
//! ## Why it also adjudicates ft-ui1xn (the quick_reject A/B)
//!
//! `quick_reject` (the default-on Bloom prefilter) is the first op inside
//! `detect_with_context` on the no-match-dominant per-delta path. Round-8's A/B
//! (synthetic no-match text on a noisy RCH remote) showed `ac_direct` (Bloom
//! disabled) faster at 1–16 KB and tied at 64 KB, but could not reach cv ≤ 5%
//! and did not cover the match-present case. This harness runs the A/B through
//! the **production `detect_with_context` entry** over a **realistic
//! count-weighted segment distribution that includes match-present segments**,
//! on the local Mac (deterministic warmed sweeps), and reports median ns/call +
//! cv per arm so the ≤5% keep-gate can be adjudicated.
//!
//! Disabling `quick_reject` is byte-equivalent (a Bloom filter has no false
//! negatives, so it never rejects text the exact Aho-Corasick matcher would
//! match); the `quick_reject_disabled_is_byte_equivalent` test proves it on this
//! exact corpus and is the RCH-remote / fail-closed correctness lane.
//!
//! Run (local Mac, deterministic timing):
//! ```text
//! cargo test --profile release-perf -p frankenterm-core \
//!   --test round9_profile_realistic_workloads -- --nocapture
//! ```

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use frankenterm_core::bocpd::{BocpdConfig, BocpdManager};
use frankenterm_core::events::{Event, EventBus};
use frankenterm_core::patterns::{AgentType, Detection, DetectionContext, PatternEngine, Severity};
use frankenterm_core::redactor::Redactor;
use frankenterm_core::storage::check_and_recover_wal;
use frankenterm_core::storage_backend_trait::RusqliteBackend;
use rusqlite::{Connection, params};

// ── fleet-minute call model (identical to round-6/7 so shares stay comparable) ──
const CAPTURE_DELTAS_PER_SEC: u64 = 192;
const REDACT_READS_PER_SEC: u64 = 64;
const DETECTION_BURST_PUBLISHES_PER_SEC: u64 = 16;
const STARTUP_RECOVERIES_PER_MIN: u64 = 1;
const WINDOW_SECS: u64 = 60;
const GATE_SHARE: f64 = 0.005;

const WARMUP: usize = 1_000;
const ITERS: usize = 8_000;
const WAL_CASES: usize = 6;
const DIRTY_WAL_ROWS: usize = 8_000;

// ── A/B sweep config (cv ≤ 5% adjudication) ──
const AB_WARMUP_SWEEPS: usize = 12;
const AB_BLOCKS: usize = 120;

struct Frame {
    name: &'static str,
    location: &'static str,
    workload: &'static str,
    candidate: bool,
    calls_measured: u64,
    total_nanos: u128,
    calls_per_min: u64,
    notes: &'static str,
}

impl Frame {
    fn mean_nanos(&self) -> f64 {
        if self.calls_measured == 0 {
            0.0
        } else {
            self.total_nanos as f64 / self.calls_measured as f64
        }
    }

    fn realistic_self_ns(&self) -> f64 {
        self.mean_nanos() * self.calls_per_min as f64
    }
}

struct WalCase {
    _dir: tempfile::TempDir,
    path: PathBuf,
    _writer: Option<Connection>,
    wal_bytes: u64,
}

fn measure(mut body: impl FnMut(), warmup: usize, iters: usize) -> (u64, u128) {
    for _ in 0..warmup {
        body();
    }
    let start = Instant::now();
    for _ in 0..iters {
        body();
    }
    (iters as u64, start.elapsed().as_nanos())
}

// ───────────────────────── realistic capture-delta corpus ─────────────────────
//
// Count-weighted to mirror real AI-agent pane output: most deltas are small
// no-match chatter (a ~200ms poll of compiler/log lines), a meaningful minority
// are medium dumps (test output, diffs, listings), a few are large (stack
// traces near the 64 KiB persist cap), and a small fraction carry a real
// rate-limit / compaction banner. Deltas run `detect_with_context` once each
// regardless of size, so the COUNT distribution — not the byte distribution —
// sets the realistic per-call mean.

const NO_MATCH_LINES: &[&str] = &[
    "   Compiling frankenterm-core v0.10.1 (/work/frankenterm)\n",
    "warning: unused variable: `tmp` (#[warn(unused_variables)])\n",
    "heartbeat ok; queue depth 3; tokens sampled; latency 12ms\n",
    "    Finished dev [unoptimized + debuginfo] target(s) in 1.04s\n",
    "-rw-r--r--  1 user staff  4321 Jun 22 10:00 module_handler.rs\n",
    "test storage::roundtrip::case_07 ... ok\n",
    "  --> crates/frankenterm-core/src/runtime.rs:3748:21\n",
    "INFO ingest: persisted segment seq=58213 bytes=4096 pane=11\n",
    "+    let combined = format!(\"{}{content}\", state.lookback);\n",
    "DEBUG scheduler: 64 panes active, poll cadence 200ms, no backpressure\n",
];

// Known-matching banners (verified at corpus-build time to produce >=1 detection).
const MATCH_BANNERS: &[&str] = &[
    "\nWarning: You have less than 25% of your 20h limit remaining.\n\
     Your current usage: 15% of your 20h limit remaining.\n",
    "\n[Claude Code] Auto-compact: Conversation compacted 150,000 tokens to 50,000 tokens.\n",
    "\nWarning: less than 10% of your 20h limit remaining. 8% of your 20h limit remaining.\n",
];

/// Tiny deterministic LCG so the corpus is byte-identical every run / arm.
struct Lcg(u64);
impl Lcg {
    fn next_u32(&mut self) -> u32 {
        // Numerical Recipes LCG constants.
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 16) as u32
    }
    fn in_range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next_u32() as usize) % (hi - lo + 1)
    }
}

fn no_match_segment(rng: &mut Lcg, target_bytes: usize) -> String {
    let mut seg = String::with_capacity(target_bytes + 64);
    while seg.len() < target_bytes {
        let line = NO_MATCH_LINES[rng.in_range(0, NO_MATCH_LINES.len() - 1)];
        seg.push_str(line);
    }
    seg.truncate(target_bytes.max(1)); // ASCII-only → byte truncation is char-safe
    seg.push('\n');
    seg
}

/// Build the realistic corpus as `(segment, is_match_present)` pairs.
fn realistic_capture_corpus() -> Vec<(String, bool)> {
    let mut rng = Lcg(0x9E37_79B9);
    let mut corpus = Vec::with_capacity(256);
    // 256 deltas: 70% small, 22% medium, 5% large, 3% match-present.
    for i in 0..256usize {
        let bucket = i % 100;
        if bucket < 70 {
            let n = rng.in_range(128, 1_024);
            corpus.push((no_match_segment(&mut rng, n), false));
        } else if bucket < 92 {
            let n = rng.in_range(1_024, 8_192);
            corpus.push((no_match_segment(&mut rng, n), false));
        } else if bucket < 97 {
            let n = rng.in_range(8_192, 64_512);
            corpus.push((no_match_segment(&mut rng, n), false));
        } else {
            let banner = MATCH_BANNERS[rng.in_range(0, MATCH_BANNERS.len() - 1)];
            // Embed the banner in a little surrounding chatter (still small).
            let n = rng.in_range(128, 384);
            let mut seg = no_match_segment(&mut rng, n);
            seg.push_str(banner);
            corpus.push((seg, true));
        }
    }
    corpus
}

/// One warmed sweep over the corpus with a fresh context (the realistic
/// cross-segment tail-overlap behavior is preserved within a sweep). Returns
/// total detections (kept live via black_box) and the call count.
fn sweep(engine: &PatternEngine, corpus: &[(String, bool)]) -> (usize, usize) {
    let mut ctx = DetectionContext::new();
    ctx.pane_id = Some(1);
    let mut hits = 0usize;
    for (seg, _) in corpus {
        hits += black_box(engine.detect_with_context(black_box(seg), &mut ctx)).len();
    }
    (hits, corpus.len())
}

/// Run `AB_BLOCKS` warmed sweeps, return per-sweep mean ns/call.
fn measure_detect_blocks(engine: &PatternEngine, corpus: &[(String, bool)]) -> Vec<f64> {
    for _ in 0..AB_WARMUP_SWEEPS {
        black_box(sweep(engine, corpus));
    }
    let mut means = Vec::with_capacity(AB_BLOCKS);
    for _ in 0..AB_BLOCKS {
        let start = Instant::now();
        let (_hits, calls) = sweep(engine, corpus);
        let ns = start.elapsed().as_nanos() as f64;
        means.push(ns / calls as f64);
    }
    means
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        xs[n / 2]
    } else {
        f64::midpoint(xs[n / 2 - 1], xs[n / 2])
    }
}

fn mean_and_cv(xs: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    if n == 0.0 {
        return (0.0, 0.0);
    }
    let mean = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let cv = if mean == 0.0 { 0.0 } else { var.sqrt() / mean };
    (mean, cv)
}

// ───────────────────────── denominator (live) frames ─────────────────────────

fn secret_dense_text() -> String {
    format!(
        "{}\nleaked sk-proj-abcdefghijklmnopqrstuvwxyz012345 and AKIAIOSFODNN7EXAMPLE token\n",
        "normal agent log output line with no secret ".repeat(20)
    )
}

fn detection(seq: u64) -> Detection {
    Detection {
        rule_id: "core.codex:usage_reached".to_string(),
        agent_type: AgentType::Codex,
        event_type: "usage_reached".to_string(),
        severity: Severity::Warning,
        confidence: 0.875,
        extracted: serde_json::json!({ "seq": seq, "reset_at": "2026-06-21T12:00:00Z" }),
        matched_text: "Usage limit reached".to_string(),
        span: (0, 19),
    }
}

fn pattern_event(seq: u64) -> Event {
    Event::PatternDetected {
        pane_id: seq % 64,
        pane_uuid: Some(format!("pane-{seq:08x}")),
        detection: detection(seq),
        event_id: Some(i64::try_from(seq).unwrap_or(i64::MAX)),
    }
}

fn bocpd_chunks() -> [&'static str; 4] {
    [
        "   Compiling frankenterm-core v0.10.1\n",
        "\x1b[32mtest storage::wal_recovery ... ok\x1b[0m\n",
        "warning: unused import in round9 profile harness\n",
        "Usage limit reached. Try again at 2026-06-22 12:00 UTC\n",
    ]
}

fn recover_wal_once(path: &Path) {
    let conn = Connection::open(path).expect("open WAL recovery profile DB");
    let backend = RusqliteBackend::new(conn).expect("install WAL recovery profile authorizer");
    let db_path = path.to_string_lossy();
    check_and_recover_wal(&backend, &db_path).expect("WAL recovery must succeed");
    black_box(
        backend
            .into_connection()
            .expect("reclaim reusable WAL recovery profile connection"),
    );
}

fn clean_wal_case(idx: usize) -> WalCase {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("round9_clean_{idx}.db"));
    {
        let conn = Connection::open(&path).expect("open clean DB");
        conn.execute_batch(
            "CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);
             INSERT INTO sample(body) VALUES ('clean startup row');",
        )
        .expect("seed clean DB");
    }
    WalCase {
        _dir: dir,
        path,
        _writer: None,
        wal_bytes: 0,
    }
}

fn dirty_wal_case(idx: usize) -> WalCase {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("round9_dirty_{idx}.db"));
    let mut conn = Connection::open(&path).expect("open dirty DB");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("enable WAL");
    conn.pragma_update(None, "wal_autocheckpoint", 0)
        .expect("disable auto-checkpoint");
    conn.execute_batch("CREATE TABLE sample(id INTEGER PRIMARY KEY, body TEXT NOT NULL);")
        .expect("create dirty DB schema");
    {
        let tx = conn.transaction().expect("dirty WAL transaction");
        let payload = "dirty startup WAL frame payload ".repeat(16);
        for row in 0..DIRTY_WAL_ROWS {
            tx.execute(
                "INSERT INTO sample(id, body) VALUES (?1, ?2)",
                params![row as i64, payload],
            )
            .expect("insert dirty WAL row");
        }
        tx.commit().expect("commit dirty WAL rows");
    }
    let wal_path = format!("{}-wal", path.to_string_lossy());
    let wal_bytes = std::fs::metadata(&wal_path).map_or(0, |meta| meta.len());
    assert!(wal_bytes > 0, "dirty WAL profile case produced no WAL file");
    WalCase {
        _dir: dir,
        path,
        _writer: Some(conn),
        wal_bytes,
    }
}

fn measure_wal_cases(mut make_case: impl FnMut(usize) -> WalCase) -> (u64, u128, u64) {
    let cases: Vec<WalCase> = (0..WAL_CASES).map(&mut make_case).collect();
    let avg_wal_bytes = cases.iter().map(|case| case.wal_bytes).sum::<u64>() / WAL_CASES as u64;
    let start = Instant::now();
    for case in &cases {
        recover_wal_once(&case.path);
    }
    (WAL_CASES as u64, start.elapsed().as_nanos(), avg_wal_bytes)
}

struct AbResult {
    on_median_ns: f64,
    on_cv: f64,
    off_median_ns: f64,
    off_cv: f64,
    corpus_len: usize,
    match_segments: usize,
}

impl AbResult {
    /// ac_direct speedup as a fraction (>0 = ac_direct faster).
    fn ac_direct_speedup(&self) -> f64 {
        if self.off_median_ns == 0.0 {
            0.0
        } else {
            (self.on_median_ns - self.off_median_ns) / self.on_median_ns
        }
    }
}

fn run_ab(corpus: &[(String, bool)]) -> AbResult {
    let on = PatternEngine::new(); // test-only ctor keeps quick_reject ON (prod default is now OFF)
    let _ = on.detect("warmup");
    let mut off = PatternEngine::new();
    off.set_quick_reject_enabled(false);
    let _ = off.detect("warmup");

    let on_blocks = measure_detect_blocks(&on, corpus);
    let off_blocks = measure_detect_blocks(&off, corpus);
    let (_on_mean, on_cv) = mean_and_cv(&on_blocks);
    let (_off_mean, off_cv) = mean_and_cv(&off_blocks);
    AbResult {
        on_median_ns: median(on_blocks),
        on_cv,
        off_median_ns: median(off_blocks),
        off_cv,
        corpus_len: corpus.len(),
        match_segments: corpus.iter().filter(|(_, m)| *m).count(),
    }
}

fn run_profile(detect_mean_ns: f64) -> (Vec<Frame>, u64) {
    let redactor = Redactor::new();
    let secret = secret_dense_text();
    let (redact_calls, redact_ns) = measure(
        || {
            black_box(redactor.redact(black_box(&secret)));
        },
        WARMUP,
        ITERS,
    );

    let (clean_calls, clean_ns, _) = measure_wal_cases(clean_wal_case);
    let (dirty_calls, dirty_ns, avg_dirty_wal_bytes) = measure_wal_cases(dirty_wal_case);

    let bus = EventBus::new(4_096);
    let _all_sub = bus.subscribe();
    let _detection_sub = bus.subscribe_detections();
    let mut seq = 0_u64;
    let (event_calls, event_ns) = measure(
        || {
            let event = pattern_event(seq);
            seq = seq.wrapping_add(1);
            black_box(bus.publish(black_box(event)));
        },
        WARMUP,
        ITERS,
    );

    let mut manager = BocpdManager::new(BocpdConfig::default());
    for pane_id in 0..64 {
        manager.register_pane(pane_id);
    }
    let chunks = bocpd_chunks();
    let mut idx = 0usize;
    let (bocpd_calls, bocpd_ns) = measure(
        || {
            let pane_id = (idx % 64) as u64;
            let chunk = chunks[idx % chunks.len()];
            idx = idx.wrapping_add(1);
            black_box(manager.observe_text_chunk(
                pane_id,
                black_box(chunk),
                Duration::from_millis(250),
            ));
        },
        WARMUP,
        ITERS,
    );

    let capture_per_min = CAPTURE_DELTAS_PER_SEC * WINDOW_SECS;
    let redact_per_min = REDACT_READS_PER_SEC * WINDOW_SECS;
    let event_per_min = DETECTION_BURST_PUBLISHES_PER_SEC * WINDOW_SECS;

    // detect_with_context is fed its realistic per-call mean (from the A/B ON
    // arm) as a synthetic single-call measurement so it occupies the corrected
    // per-delta slot the dead scan_pipeline.process used to phantom-fill.
    let detect_total_ns = (detect_mean_ns * ITERS as f64) as u128;

    let frames = vec![
        Frame {
            name: "patterns.detect_with_context",
            location: "patterns.rs:4441 (runtime.rs:3748)",
            workload: "per-capture-delta detection (realistic corpus, quick_reject ON)",
            candidate: true,
            calls_measured: ITERS as u64,
            total_nanos: detect_total_ns,
            calls_per_min: capture_per_min,
            notes: "CORRECTED per-delta frame (was phantom scan_pipeline.process, dead code)",
        },
        Frame {
            name: "redactor.redact",
            location: "redactor.rs:690",
            workload: "denominator: outbound read redaction",
            candidate: false,
            calls_measured: redact_calls,
            total_nanos: redact_ns,
            calls_per_min: redact_per_min,
            notes: "denominator (live read/export/audit surfaces)",
        },
        Frame {
            name: "bocpd.observe_text_chunk",
            location: "runtime.rs:3758 -> bocpd.rs:844",
            workload: "per-capture BOCPD segment observation",
            candidate: true,
            calls_measured: bocpd_calls,
            total_nanos: bocpd_ns,
            calls_per_min: capture_per_min,
            notes: "live per-delta (quality ARL metric deferred)",
        },
        Frame {
            name: "storage.wal_recovery_clean",
            location: "storage.rs:1647",
            workload: "startup clean DB",
            candidate: true,
            calls_measured: clean_calls,
            total_nanos: clean_ns,
            calls_per_min: STARTUP_RECOVERIES_PER_MIN,
            notes: "startup clean contrast",
        },
        Frame {
            name: "storage.wal_recovery_dirty",
            location: "storage.rs:1647",
            workload: "startup WAL-dirty DB",
            candidate: true,
            calls_measured: dirty_calls,
            total_nanos: dirty_ns,
            calls_per_min: STARTUP_RECOVERIES_PER_MIN,
            notes: "startup dirty contrast (WAL skip-checkpoint lever ft-yjihu.1)",
        },
        Frame {
            name: "events.event_bus_publish",
            location: "events.rs:1280",
            workload: "burst pattern detection fanout",
            candidate: true,
            calls_measured: event_calls,
            total_nanos: event_ns,
            calls_per_min: event_per_min,
            notes: "live via runtime/ipc/bridge publishers",
        },
    ];

    (frames, avg_dirty_wal_bytes)
}

#[test]
fn profile_round9_detect_with_context_corrected() {
    let corpus = realistic_capture_corpus();
    let ab = run_ab(&corpus);

    let (mut frames, avg_dirty_wal_bytes) = run_profile(ab.on_median_ns);
    let total_realistic_ns: f64 = frames.iter().map(Frame::realistic_self_ns).sum();
    assert!(
        total_realistic_ns > 0.0,
        "no self-time recorded; harness is dead"
    );

    frames.sort_by(|a, b| {
        b.realistic_self_ns()
            .partial_cmp(&a.realistic_self_ns())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    println!("\n=== ROUND-9 B0-CORRECTION PROFILE (ft-zhj63) — corrected per-delta frame ===");
    println!(
        "model: {CAPTURE_DELTAS_PER_SEC} captures/s, {REDACT_READS_PER_SEC} redacted reads/s, \
         {DETECTION_BURST_PUBLISHES_PER_SEC} burst detections/s, \
         {STARTUP_RECOVERIES_PER_MIN} startup recovery/min over {WINDOW_SECS}s | \
         gate = {:.1}% self-time | avg dirty WAL bytes = {avg_dirty_wal_bytes}",
        GATE_SHARE * 100.0
    );
    println!(
        "{:<30} {:<34} {:>10} {:>10} {:>9} {:>11} {}",
        "frame", "location", "mean_ns", "calls/min", "share%", "rank_ns", "gate"
    );
    let mut detect_share = 0.0;
    for frame in &frames {
        let share = frame.realistic_self_ns() / total_realistic_ns;
        if frame.name == "patterns.detect_with_context" {
            detect_share = share;
        }
        let gate = if share >= GATE_SHARE { "PASS" } else { "below" };
        println!(
            "{:<30} {:<34} {:>10.1} {:>10} {:>8.3}% {:>11.0} {}",
            frame.name,
            frame.location,
            frame.mean_nanos(),
            frame.calls_per_min,
            share * 100.0,
            frame.realistic_self_ns(),
            gate
        );
    }

    // ── ft-ui1xn A/B verdict ──
    let speedup = ab.ac_direct_speedup();
    // Fraction of detect_with_context self-time attributable to the quick_reject
    // prefilter on the realistic (no-match-dominant) corpus.
    let quick_reject_share_of_detect = speedup.max(0.0);
    let quick_reject_realistic_share = detect_share * quick_reject_share_of_detect;
    let cv_ok = ab.on_cv <= 0.05 && ab.off_cv <= 0.05;

    println!(
        "\n=== ROUND-9 quick_reject A/B (ft-ui1xn) — production detect_with_context entry ==="
    );
    println!(
        "corpus: {} deltas ({} match-present), count-weighted realistic size distribution",
        ab.corpus_len, ab.match_segments
    );
    println!(
        "quick_reject_on : median {:.1} ns/call  cv {:.2}%",
        ab.on_median_ns,
        ab.on_cv * 100.0
    );
    println!(
        "ac_direct       : median {:.1} ns/call  cv {:.2}%",
        ab.off_median_ns,
        ab.off_cv * 100.0
    );
    println!(
        "ac_direct speedup vs quick_reject_on: {:+.2}%  (cv<=5% both arms: {})",
        speedup * 100.0,
        cv_ok
    );
    println!(
        "detect_with_context realistic share: {:.3}%  (gate {:.1}%: {})",
        detect_share * 100.0,
        GATE_SHARE * 100.0,
        if detect_share >= GATE_SHARE {
            "PASS"
        } else {
            "below"
        }
    );
    println!(
        "quick_reject prefilter realistic share (= detect_share x speedup): {:.3}%",
        quick_reject_realistic_share * 100.0
    );

    let promote = detect_share >= GATE_SHARE && cv_ok && speedup > 0.0;
    println!(
        "VERDICT: detect_with_context_gate={} cv_ok={} ac_direct_faster={} => promote_ac_direct={}",
        detect_share >= GATE_SHARE,
        cv_ok,
        speedup > 0.0,
        promote
    );

    let mut frames_json = String::new();
    for (i, frame) in frames.iter().enumerate() {
        if i > 0 {
            frames_json.push(',');
        }
        let share = frame.realistic_self_ns() / total_realistic_ns;
        frames_json.push_str(&format!(
            "{{\"frame\":\"{}\",\"location\":\"{}\",\"workload\":\"{}\",\
             \"candidate\":{},\"mean_ns\":{:.2},\"calls_per_min\":{},\
             \"realistic_self_ns\":{:.0},\"share\":{:.6},\"gate_pass\":{},\
             \"notes\":\"{}\"}}",
            frame.name,
            frame.location,
            frame.workload,
            frame.candidate,
            frame.mean_nanos(),
            frame.calls_per_min,
            frame.realistic_self_ns(),
            share,
            share >= GATE_SHARE,
            frame.notes
        ));
    }

    println!(
        "ROUND9_PROFILE_JSON {{\"schema\":\"round9.b0_correction.v1\",\
         \"detect_with_context_share\":{detect_share:.6},\
         \"detect_gate_pass\":{},\
         \"quick_reject_on_median_ns\":{:.2},\"quick_reject_on_cv\":{:.4},\
         \"ac_direct_median_ns\":{:.2},\"ac_direct_cv\":{:.4},\
         \"ac_direct_speedup\":{speedup:.6},\"cv_ok\":{cv_ok},\
         \"quick_reject_realistic_share\":{quick_reject_realistic_share:.6},\
         \"corpus_len\":{},\"match_segments\":{},\"promote_ac_direct\":{promote},\
         \"frames\":[{frames_json}]}}",
        detect_share >= GATE_SHARE,
        ab.on_median_ns,
        ab.on_cv,
        ab.off_median_ns,
        ab.off_cv,
        ab.corpus_len,
        ab.match_segments,
    );

    // Sanity (not a perf assertion): every frame measured non-zero, shares sum to 1.
    for frame in &frames {
        assert!(
            frame.mean_nanos() > 0.0,
            "frame {} measured zero mean ns",
            frame.name
        );
    }
    let share_sum: f64 = frames
        .iter()
        .map(|frame| frame.realistic_self_ns() / total_realistic_ns)
        .sum();
    assert!(
        (share_sum - 1.0).abs() < 1e-6,
        "shares must sum to 1, got {share_sum}"
    );
    assert!(
        frames
            .iter()
            .any(|f| f.name == "patterns.detect_with_context"),
        "corrected per-delta frame missing"
    );
    assert!(
        !frames.iter().any(|f| f.name.contains("scan_pipeline")),
        "dead scan_pipeline.process must NOT be in the corrected denominator"
    );
}

/// RCH-remote / fail-closed correctness lane: disabling quick_reject is
/// byte-equivalent on the realistic corpus (a Bloom prefilter has no false
/// negatives). This is the ft-ui1xn promotion safety proof.
#[test]
fn quick_reject_disabled_is_byte_equivalent() {
    let corpus = realistic_capture_corpus();

    let on = PatternEngine::new();
    let _ = on.detect("warmup");
    let mut off = PatternEngine::new();
    off.set_quick_reject_enabled(false);
    let _ = off.detect("warmup");

    let mut ctx_on = DetectionContext::new();
    ctx_on.pane_id = Some(1);
    let mut ctx_off = DetectionContext::new();
    ctx_off.pane_id = Some(1);

    let mut total_match_segments = 0usize;
    let mut total_detections = 0usize;
    for (i, (seg, is_match)) in corpus.iter().enumerate() {
        let r_on = on.detect_with_context(seg, &mut ctx_on);
        let r_off = off.detect_with_context(seg, &mut ctx_off);
        let j_on = serde_json::to_string(&r_on).expect("serialize on");
        let j_off = serde_json::to_string(&r_off).expect("serialize off");
        assert_eq!(
            j_on, j_off,
            "quick_reject on/off diverged at segment {i} (is_match={is_match}): \
             disabling the Bloom prefilter must be byte-equivalent"
        );
        if *is_match {
            total_match_segments += 1;
        }
        total_detections += r_on.len();
    }
    // The corpus must actually exercise the match path, or the equivalence proof
    // is vacuous on the interesting (banner-present) case.
    assert!(
        total_match_segments > 0,
        "corpus has no match-present segments; equivalence proof would be vacuous"
    );
    assert!(
        total_detections > 0,
        "no detections fired across the corpus; match banners are not matching — \
         fix MATCH_BANNERS before trusting the byte-equivalence proof"
    );
    println!(
        "byte-equivalence holds across {} deltas ({} match-present, {} total detections)",
        corpus.len(),
        total_match_segments,
        total_detections
    );
}
