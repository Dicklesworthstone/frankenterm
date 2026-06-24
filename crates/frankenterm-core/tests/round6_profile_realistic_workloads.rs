//! Round-6 B0 — the profiling gate (bead `ft-p4vzl.1`).
//!
//! ## What this is
//!
//! A realistic-workload profiling harness that ranks the reachable hot-path
//! frames of `frankenterm-core` by **self-time** under a documented
//! fleet-minute call model, and prints a **scored target list** with a hard
//! **≥0.5% attribution gate**. The gauntlet rule (round-6 marching orders, keep
//! gate rule #1 / #9) is: *no new optimization idea earns a bead unless a
//! profiler attributes ≥0.5% self-time to the frame it targets on a realistic
//! workload.* This harness is the substrate that produces that evidence.
//!
//! It drives the four named realistic workloads through the public API:
//!
//! | Workload            | Frame(s) exercised                                  |
//! |---------------------|-----------------------------------------------------|
//! | high-pane capture   | `ingest::extract_delta`                             |
//! | per-delta detection | `patterns::detect_with_context` (ANSI/trigger stress) |
//! | deep-scroll         | `TieredScrollback::locate_offset` + `::warm_line`   |
//! | search-heavy        | `patterns::detect_with_context` + `Redactor::redact` |
//!
//! ## Method (honest self-time, not a flamegraph artifact)
//!
//! A sampling/dtrace flamegraph on macOS needs root and is host-bound; a
//! deterministic in-process call-site timer is reproducible and host-portable.
//! Each frame is a **leaf** public call, so its call-site wall-clock *is* its
//! self-time (there is no instrumented child to subtract). We:
//!
//! 1. Measure mean ns/call for each frame over a warmed, tight loop on a
//!    representative input — this is the per-op cost a profiler would attribute.
//! 2. Weight each frame by a **documented fleet-minute call model** (a busy
//!    64-pane fleet over 60 s) to get realistic self-time-per-minute.
//! 3. Rank by realistic self-time, compute share %, apply the ≥0.5% gate.
//!
//! Decoupling measurement (tight loops → stable mean) from weighting (the call
//! model → realistic mix) keeps both honest and auditable: change the model
//! constants and the ranking re-derives.
//!
//! ## Honesty
//!
//! The mean-ns numbers are a single-host datapoint and are NOT an attested
//! cross-engine perf claim. The *share ranking* and the ≥0.5% gate verdict are
//! the deliverable — they are robust to host because they are relative. Run
//! under the `release-perf` profile (opt-level 3, thin LTO) for representative
//! codegen:
//!
//! ```text
//! cargo test --profile release-perf -p frankenterm-core \
//!   --test round6_profile_realistic_workloads -- --nocapture
//! ```
//!
//! Built in the default debug `cargo test` it still passes (it is a fail-closed
//! sanity guard on the harness), but the printed ns are debug-inflated — only
//! the release-perf run feeds the scored target list.

// Measurement code: ns→f64 share math and call-count casts are intentional and
// lossless at these magnitudes.
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::hint::black_box;
use std::time::Instant;

// round-9: the dead `scan_pipeline` module was deleted; its `scan_pipeline.process`
// per-delta frame is rewired here to the LIVE per-capture-delta production frame
// `patterns::detect_with_context` (runtime.rs:3748). The corrected B0 is
// `round9_profile_realistic_workloads.rs`; this historical harness keeps measuring
// the real frame so it still compiles and ranks a live target.
use frankenterm_core::ingest::extract_delta;
use frankenterm_core::patterns::{DetectionContext, PatternEngine};
use frankenterm_core::redactor::Redactor;
use frankenterm_core::scrollback_tiers::{
    ScrollbackConfig, ScrollbackLocationHint, TieredScrollback,
};

// ── Fleet-minute call model ─────────────────────────────────────────────────
// A busy 64-pane fleet observed for 60 seconds. These are the *modeling
// assumptions* that turn per-op cost into realistic CPU share. They are
// deliberately explicit so the orchestrator can re-weight: edit a constant,
// re-run, and the ranking re-derives. Rationale per constant inline.

/// Active capture deltas produced fleet-wide each second. 64 panes, ~3
/// output-bearing polls/sec each across the active subset (default poll
/// 200 ms = 5 polls/sec, not all yielding a delta).
const CAPTURE_DELTAS_PER_SEC: u64 = 192;
/// Outbound pane-content reads/sec that pass through redaction — one read per
/// pane per second for the dashboard / robot / watch fanout surfaces.
const REDACT_READS_PER_SEC: u64 = 64;
/// Deep scrollback seeks/sec fleet-wide — an occasional agent/operator
/// search-or-jump into history (rare relative to streaming).
const SCROLL_SEEKS_PER_SEC: u64 = 5;
/// Observation window the model integrates over.
const WINDOW_SECS: u64 = 60;

/// The ≥0.5% self-time attribution gate: below this a frame does not earn a
/// new optimization bead in round 6.
const GATE_SHARE: f64 = 0.005;

// ── Per-frame measurement loop sizing ───────────────────────────────────────
// Modest so the default-`cargo test` debug run stays cheap; large enough that
// the mean is stable. The deep-scroll build dominates wall-clock.
const WARMUP: usize = 1_000;
const ITERS: usize = 8_000;
const DEEP_LINES: usize = 12_000;
const SEEK_COUNT: usize = 1_500;

/// One measured hot frame and its derived realistic contribution.
struct Frame {
    /// Stable frame name (code location identity used in the scored list).
    name: &'static str,
    /// `file:line` anchor a future bead targets.
    location: &'static str,
    /// Named realistic workload that exercises it.
    workload: &'static str,
    /// Iterations measured.
    calls_measured: u64,
    /// Wall-clock self-time of the measured loop, ns.
    total_nanos: u128,
    /// Calls in one fleet-minute per the call model above.
    calls_per_min: u64,
}

impl Frame {
    fn mean_nanos(&self) -> f64 {
        if self.calls_measured == 0 {
            0.0
        } else {
            self.total_nanos as f64 / self.calls_measured as f64
        }
    }

    /// Realistic self-time this frame burns in one fleet-minute (ns).
    fn realistic_self_ns(&self) -> f64 {
        self.mean_nanos() * self.calls_per_min as f64
    }
}

/// Warmed, tight measured loop. Returns `(iters, elapsed_nanos)`.
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

/// Deterministic, allocation-free spreading of seek offsets across `[lo, hi)`
/// via a tiny LCG — reproducible without `rand` so the ranking is stable.
fn spread_offsets(lo: usize, hi: usize, n: usize) -> Vec<usize> {
    debug_assert!(hi > lo);
    let span = (hi - lo) as u64;
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..n)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            lo + ((state >> 33) % span) as usize
        })
        .collect()
}

// ── Representative input builders ───────────────────────────────────────────

/// high-pane capture: an accumulated agent/compiler pane buffer plus the next
/// appended line — the exact shape `extract_delta` sees every observation tick.
fn capture_pair() -> (String, String) {
    let prev = "\
   Compiling frankenterm-core v0.9.0\n\
warning: unused variable `tmp`\n\
    Finished release [optimized] target(s) in 4.21s\n\
running 218 tests\ntest scan_pipeline::tests::ansi ... ok\n"
        .repeat(12);
    let cur = format!("{prev}test ingest::tests::extract_delta_overlap ... ok\n");
    (prev, cur)
}

/// ANSI-dense render: a TUI-style redraw frame — dense SGR colour runs and
/// cursor positioning, the worst case for the escape-aware scan.
fn ansi_saturated_frame() -> Vec<u8> {
    let mut s = String::with_capacity(4096);
    for row in 0..40 {
        s.push_str(&format!("\x1b[{};1H", row + 1)); // cursor position
        for col in 0..16 {
            let fg = 31 + (col % 7);
            s.push_str(&format!("\x1b[1;{fg}m█▓▒░\x1b[0m"));
        }
        s.push('\n');
    }
    s.into_bytes()
}

/// Representative streamed terminal output: mostly text, some colour, occasional
/// markers — the realistic mix `ScanPipeline::process` actually sees per delta.
fn mixed_terminal_frame() -> Vec<u8> {
    "\x1b[32m   Compiling\x1b[0m frankenterm-core v0.9.0\n\
warning: field is never read: `tmp`\n\
\x1b[31merror[E0277]\x1b[0m: the trait bound is not satisfied\n\
    Finished dev [unoptimized] target(s) in 1.04s\n"
        .repeat(8)
        .into_bytes()
}

/// search-heavy: trigger/marker-saturated content (errors, rate limits,
/// completion markers) — maximises the trigger-scanner work in the pipeline.
fn trigger_saturated_frame() -> Vec<u8> {
    "error[E0599]: no method named `foo`\n\
Usage limit reached. Try again at 2026-06-20 12:34 UTC\n\
panic: runtime error: index out of range\n\
Done. Build succeeded.\n\
codex.usage.reached threshold crossed\n"
        .repeat(8)
        .into_bytes()
}

/// search-heavy redaction: outbound text carrying secrets that the redactor
/// must find and mask on every read.
fn secret_dense_text() -> String {
    format!(
        "{}\nleaked sk-proj-abcdefghijklmnopqrstuvwxyz012345 and AKIAIOSFODNN7EXAMPLE token\n",
        "normal agent log output line with no secret ".repeat(20)
    )
}

/// Build a deep scrollback (most lines compressed into warm/cold tiers) and a
/// set of deep seek offsets plus their resolved warm hints.
fn build_deep_scroll() -> (TieredScrollback, Vec<usize>, Vec<ScrollbackLocationHint>) {
    let mut sb = TieredScrollback::new(ScrollbackConfig::default());
    for i in 0..DEEP_LINES {
        sb.push_line(format!(
            "2026-06-20T12:{:02}:{:02}Z line {i} compiling module with some realistic width",
            (i / 60) % 60,
            i % 60
        ));
    }
    let total = sb.total_line_count() as usize;
    let hot = sb.hot_len();
    // Seek into history (offsets from end ≥ hot_len land in warm/cold — the
    // path Q1 prefix-index / EV3 blocked pages optimise).
    let offsets = spread_offsets(hot.saturating_add(1), total.max(hot + 2), SEEK_COUNT);
    let warm_hints: Vec<ScrollbackLocationHint> = offsets
        .iter()
        .filter_map(|&off| sb.locate_offset(off))
        .filter(|h| matches!(h, ScrollbackLocationHint::Warm { .. }))
        .collect();
    (sb, offsets, warm_hints)
}

/// Run all frames, returning the measured set plus the two informational
/// scan stress points.
fn run_profile() -> (Vec<Frame>, f64, f64) {
    // capture
    let (prev, cur) = capture_pair();
    let (c_calls, c_ns) = measure(
        || {
            black_box(extract_delta(black_box(&prev), black_box(&cur), 4096));
        },
        WARMUP,
        ITERS,
    );

    // per-delta detection — the REAL per-capture-delta production frame
    // (round-9: rewired from the deleted ScanPipeline::process to
    // detect_with_context, runtime.rs:3748).
    let engine = PatternEngine::new();
    let _ = engine.detect("warmup");
    let mixed = String::from_utf8_lossy(&mixed_terminal_frame()).into_owned();
    let mut sctx = DetectionContext::new();
    let (s_calls, s_ns) = measure(
        || {
            black_box(engine.detect_with_context(black_box(&mixed), &mut sctx));
        },
        WARMUP,
        ITERS,
    );

    // detection stress — ANSI-saturated and trigger-saturated (informational only)
    let ansi = String::from_utf8_lossy(&ansi_saturated_frame()).into_owned();
    let mut actx = DetectionContext::new();
    let (_, ansi_ns) =
        measure(|| { black_box(engine.detect_with_context(black_box(&ansi), &mut actx)); }, WARMUP, ITERS);
    let triggers = String::from_utf8_lossy(&trigger_saturated_frame()).into_owned();
    let mut tctx = DetectionContext::new();
    let (_, trig_ns) =
        measure(|| { black_box(engine.detect_with_context(black_box(&triggers), &mut tctx)); }, WARMUP, ITERS);
    let ansi_mean = ansi_ns as f64 / ITERS as f64;
    let trig_mean = trig_ns as f64 / ITERS as f64;

    // redact
    let redactor = Redactor::new();
    let secret = secret_dense_text();
    let (r_calls, r_ns) = measure(
        || {
            black_box(redactor.redact(black_box(&secret)));
        },
        WARMUP,
        ITERS,
    );

    // deep-scroll: locate_offset + warm_line
    let (sb, offsets, warm_hints) = build_deep_scroll();
    let mut oi = 0usize;
    let (l_calls, l_ns) = measure(
        || {
            let off = offsets[oi % offsets.len()];
            oi = oi.wrapping_add(1);
            black_box(sb.locate_offset(black_box(off)));
        },
        WARMUP.min(offsets.len()),
        ITERS,
    );
    let (w_calls, w_ns) = if warm_hints.is_empty() {
        (0, 0)
    } else {
        let mut wi = 0usize;
        measure(
            || {
                let h = &warm_hints[wi % warm_hints.len()];
                wi = wi.wrapping_add(1);
                black_box(sb.warm_line(black_box(h)));
            },
            WARMUP.min(warm_hints.len()),
            ITERS,
        )
    };

    let cap_per_min = CAPTURE_DELTAS_PER_SEC * WINDOW_SECS;
    let redact_per_min = REDACT_READS_PER_SEC * WINDOW_SECS;
    let seek_per_min = SCROLL_SEEKS_PER_SEC * WINDOW_SECS;

    let frames = vec![
        Frame {
            name: "ingest.extract_delta",
            location: "ingest.rs:1801",
            workload: "high-pane capture",
            calls_measured: c_calls,
            total_nanos: c_ns,
            calls_per_min: cap_per_min,
        },
        Frame {
            name: "patterns.detect_with_context",
            location: "patterns.rs:4436 (runtime.rs:3748)",
            workload: "per-capture-delta detection (the real per-delta frame)",
            calls_measured: s_calls,
            total_nanos: s_ns,
            calls_per_min: cap_per_min, // every captured delta runs detection
        },
        Frame {
            name: "redactor.redact",
            location: "redactor.rs:690",
            workload: "search-heavy (outbound read redaction)",
            calls_measured: r_calls,
            total_nanos: r_ns,
            calls_per_min: redact_per_min,
        },
        Frame {
            name: "scrollback.locate_offset",
            location: "scrollback_tiers.rs:1014",
            workload: "deep-scroll seek",
            calls_measured: l_calls,
            total_nanos: l_ns,
            calls_per_min: seek_per_min,
        },
        Frame {
            name: "scrollback.warm_line",
            location: "scrollback_tiers.rs:883",
            workload: "deep-scroll line fetch",
            calls_measured: w_calls,
            total_nanos: w_ns,
            calls_per_min: seek_per_min,
        },
    ];

    (frames, ansi_mean, trig_mean)
}

#[test]
fn profile_realistic_workloads_and_emit_scored_targets() {
    let (mut frames, ansi_mean, trig_mean) = run_profile();

    let total_realistic_ns: f64 = frames.iter().map(Frame::realistic_self_ns).sum();
    assert!(
        total_realistic_ns > 0.0,
        "no self-time recorded — harness is dead"
    );

    // Rank by realistic self-time descending.
    frames.sort_by(|a, b| {
        b.realistic_self_ns()
            .partial_cmp(&a.realistic_self_ns())
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // ── Human-readable scored target list (visible with --nocapture) ──
    println!("\n=== ROUND-6 B0 PROFILE — scored hot-frame target list ===");
    println!(
        "fleet-minute model: {CAPTURE_DELTAS_PER_SEC} deltas/s, {REDACT_READS_PER_SEC} reads/s, \
         {SCROLL_SEEKS_PER_SEC} seeks/s over {WINDOW_SECS}s | gate = {:.1}% self-time",
        GATE_SHARE * 100.0
    );
    println!(
        "NOTE: run under --profile release-perf for valid ns; ranking/gate are relative & host-portable.\n"
    );
    println!(
        "{:<28} {:<26} {:>10} {:>10} {:>9} {:>9}  {}",
        "frame", "location", "mean_ns", "calls/min", "share%", "rank_ns", "gate(>=0.5%)"
    );
    for f in &frames {
        let share = f.realistic_self_ns() / total_realistic_ns;
        let gate = if share >= GATE_SHARE { "PASS — eligible" } else { "below — no bead" };
        println!(
            "{:<28} {:<26} {:>10.1} {:>10} {:>8.3}% {:>9.0}  {}",
            f.name,
            f.location,
            f.mean_nanos(),
            f.calls_per_min,
            share * 100.0,
            f.realistic_self_ns(),
            gate
        );
    }
    println!(
        "\nscan stress (informational, not in mix): ansi_saturated mean={ansi_mean:.1}ns  \
         trigger_saturated mean={trig_mean:.1}ns"
    );

    // ── Machine-readable JSON (one line, easy to lift into the artifact) ──
    let mut json = String::from("ROUND6_B0_JSON {\"schema\":\"round6.b0.profile.v1\",\"gate_share\":");
    json.push_str(&format!("{GATE_SHARE},\"frames\":["));
    for (i, f) in frames.iter().enumerate() {
        let share = f.realistic_self_ns() / total_realistic_ns;
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"frame\":\"{}\",\"location\":\"{}\",\"workload\":\"{}\",\"mean_ns\":{:.2},\
             \"calls_per_min\":{},\"realistic_self_ns\":{:.0},\"share\":{:.6},\"gate_pass\":{}}}",
            f.name,
            f.location,
            f.workload,
            f.mean_nanos(),
            f.calls_per_min,
            f.realistic_self_ns(),
            share,
            share >= GATE_SHARE
        ));
    }
    json.push_str("]}");
    println!("{json}");

    // ── Fail-closed sanity assertions on the harness itself ──
    for f in &frames {
        if f.name == "scrollback.warm_line" {
            // warm_line is 0 only if no warm hint resolved (tiny deep build);
            // with DEEP_LINES it must resolve at least one.
            assert!(
                f.calls_measured > 0,
                "deep-scroll produced no warm pages — build too shallow"
            );
        }
        assert!(
            f.mean_nanos() > 0.0,
            "frame {} measured zero mean ns (timer broken)",
            f.name
        );
    }
    // At least one frame must clear the gate — otherwise the gate logic or the
    // measurement is broken and the scored target list would be empty. WHICH
    // frames clear is the empirical result (profile-dependent: regex-heavy
    // redaction dominates in debug but compresses under release-perf), so it is
    // emitted data, not an asserted invariant.
    let cleared: Vec<&str> = frames
        .iter()
        .filter(|f| f.realistic_self_ns() / total_realistic_ns >= GATE_SHARE)
        .map(|f| f.name)
        .collect();
    assert!(
        !cleared.is_empty(),
        "no frame cleared the {:.1}% gate — measurement/gate is broken",
        GATE_SHARE * 100.0
    );

    // Shares sum to ~1.0.
    let sum_share: f64 = frames
        .iter()
        .map(|f| f.realistic_self_ns() / total_realistic_ns)
        .sum();
    assert!((sum_share - 1.0).abs() < 1e-6, "shares must sum to 1, got {sum_share}");
}
