//! Benchmarks for pattern detection engine.
//!
//! Performance budgets (from PLAN §13.4 + Appendix G.7):
//! - Quick reject no-match: **< 1µs** for typical non-matching text
//! - Pattern detection (typical corpus): **p50 < 1ms**, **p99 < 5ms**

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::patterns::{DetectionContext, PatternEngine};
use std::fmt::Write as _;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "quick_reject_no_match",
        budget: "p50 < 1µs (typical non-matching text)",
    },
    bench_common::BenchBudget {
        name: "pattern_detection_typical",
        budget: "p50 < 1ms, p99 < 5ms (typical corpus)",
    },
    bench_common::BenchBudget {
        name: "lazy_init_construction",
        budget: "< 50ms (no compilation, pack loading only)",
    },
    bench_common::BenchBudget {
        name: "lazy_init_first_detect",
        budget: "< 200ms (includes one-time compilation)",
    },
    bench_common::BenchBudget {
        name: "lazy_init_warm_detect",
        budget: "< 5ms (index already compiled)",
    },
    bench_common::BenchBudget {
        name: "b1_cross_chunk_rescan",
        budget: "tail-overlap re-scan overhead far below the round-6 >=2x bar \
                 (production cross-chunk path is detect_with_context's bounded \
                 2048-byte tail, prefilter-gated; the named trigger_data_buffer \
                 whole-window re-scan is dead code — ft-p4vzl.2)",
    },
    bench_common::BenchBudget {
        name: "quick_reject_vs_ac_direct",
        budget: "ac_direct (quick_reject disabled) should beat quick_reject_on \
                 on realistic no-match text — the Bloom prefilter does ~15 SipHash \
                 window-hashes/byte + 32 memchr sweeps to avoid one exact AC pass \
                 that is already built and does zero hashing (ft-p4vzl B5 candidate)",
    },
];

/// Typical shell output that shouldn't match any patterns.
const TYPICAL_NO_MATCH: &str = r"$ ls -la
total 64
drwxr-xr-x  10 user  staff    320 Jan 18 12:00 .
drwxr-xr-x   8 user  staff    256 Jan 17 10:00 ..
-rw-r--r--   1 user  staff   1234 Jan 18 11:30 Cargo.toml
-rw-r--r--   1 user  staff   5678 Jan 18 11:30 README.md
drwxr-xr-x   5 user  staff    160 Jan 18 10:00 src

$ git status
On branch main
Your branch is up to date with 'origin/main'.

nothing to commit, working tree clean

$ cargo build
   Compiling frankenterm-core v0.1.0 (/path/to/project)
    Finished dev [unoptimized + debuginfo] target(s) in 2.34s
";

/// Short shell command with no patterns.
const SHORT_NO_MATCH: &str = "$ echo hello\nhello\n";

/// Content that triggers Codex usage warning pattern.
const CODEX_USAGE_WARNING: &str = r"
Warning: You have less than 25% of your 20h limit remaining.

Your current usage: 15% of your 20h limit remaining.
Consider wrapping up your current session soon.

To check your remaining time, run: codex usage
";

/// Content that triggers Claude Code compaction pattern.
const CLAUDE_COMPACTION: &str = r"
[Claude Code] Auto-compact: Conversation compacted 150,000 tokens to 50,000 tokens.

Your conversation has been summarized to fit within the context window.
Some earlier messages may no longer be available in full detail.
";

/// Content with multiple potential pattern matches.
const MULTI_MATCH: &str = r"
[Session Info]
Token usage: total=50,000 input=30,000 (+ 10,000 cached) output=10,000

Warning: less than 10% of your 20h limit remaining. 8% of your 20h limit remaining.

Note: If you need to resume this session later, use:
  codex resume 12345678-1234-1234-1234-123456789012

[Auto-compact] context compacted 200,000 tokens to 75,000 tokens.
";

/// Large terminal output (simulating scrollback buffer).
fn large_output(size_kb: usize) -> String {
    let base = "$ echo 'Processing item'\nProcessing item\nStatus: OK\n";
    base.repeat(size_kb * 1024 / base.len())
}

fn bench_quick_reject(c: &mut Criterion) {
    let engine = PatternEngine::new();
    let _ = engine.detect("warmup");

    let mut group = c.benchmark_group("pattern_quick_reject");

    // Budget: < 1µs for typical non-matching text
    group.bench_function("typical_shell_output", |b| {
        b.iter(|| engine.detect(TYPICAL_NO_MATCH));
    });

    group.bench_function("short_no_match", |b| {
        b.iter(|| engine.detect(SHORT_NO_MATCH));
    });

    // Test with various sizes
    for size_kb in [1, 4, 16] {
        let large = large_output(size_kb);
        group.throughput(Throughput::Bytes(large.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("large_no_match", format!("{size_kb}KB")),
            &large,
            |b, content| b.iter(|| engine.detect(content)),
        );
    }

    group.finish();
}

fn bench_pattern_detection(c: &mut Criterion) {
    let engine = PatternEngine::new();
    let _ = engine.detect("warmup");

    let mut group = c.benchmark_group("pattern_detection");

    // Budget: p50 < 1ms, p99 < 5ms
    group.bench_function("codex_usage_warning", |b| {
        b.iter(|| engine.detect(CODEX_USAGE_WARNING));
    });

    group.bench_function("claude_compaction", |b| {
        b.iter(|| engine.detect(CLAUDE_COMPACTION));
    });

    group.bench_function("multi_match", |b| {
        b.iter(|| engine.detect(MULTI_MATCH));
    });

    group.finish();
}

fn bench_detection_with_context(c: &mut Criterion) {
    let engine = PatternEngine::new();

    let mut group = c.benchmark_group("pattern_detection_context");

    // Test detection with deduplication context
    group.bench_function("with_context_no_match", |b| {
        let mut ctx = DetectionContext::new();
        ctx.pane_id = Some(1);
        b.iter(|| engine.detect_with_context(TYPICAL_NO_MATCH, &mut ctx));
    });

    group.bench_function("with_context_match", |b| {
        let mut ctx = DetectionContext::new();
        ctx.pane_id = Some(1);
        b.iter(|| engine.detect_with_context(CODEX_USAGE_WARNING, &mut ctx));
    });

    // Context dedup after first detection should be faster
    group.bench_function("with_context_dedup", |b| {
        let mut ctx = DetectionContext::new();
        ctx.pane_id = Some(1);
        // Prime the context with a detection
        let _ = engine.detect_with_context(CODEX_USAGE_WARNING, &mut ctx);
        b.iter(|| engine.detect_with_context(CODEX_USAGE_WARNING, &mut ctx));
    });

    group.finish();
}

fn bench_throughput(c: &mut Criterion) {
    let engine = PatternEngine::new();

    let mut group = c.benchmark_group("pattern_throughput");

    // Test throughput with various content sizes
    for size_kb in [1, 4, 16, 64] {
        let content = large_output(size_kb);
        group.throughput(Throughput::Bytes(content.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("throughput", format!("{size_kb}KB")),
            &content,
            |b, content| b.iter(|| engine.detect(content)),
        );
    }

    group.finish();
}

/// B1 (ft-p4vzl.2) evidence bench — quantify the *production* cross-chunk
/// Aho-Corasick re-scan overhead.
///
/// FINDING (the load-bearing reason B1 does not proceed to implementation):
/// the flagship target named in the round-6 marching orders —
/// `scan_pipeline::ChunkedPipelineState::flush` re-scanning the accumulated
/// `trigger_data_buffer` (README §"Cross-chunk subtlety") — was **dead code**
/// (ZERO production callers repo-wide), and the whole `scan_pipeline` module was
/// DELETED in round-9. That whole-window re-scan never had non-test self-time
/// and could not clear the >=0.5% profile-first gate.
///
/// The *real* production cross-chunk detection path is
/// [`PatternEngine::detect_with_context`] (driven per pane segment from
/// `runtime.rs`). Its cross-segment handling is NOT a whole-window re-scan:
/// it prepends a bounded `DetectionContext::tail_buffer` (<= 2048 B; segments
/// are capped at 64 KiB) to each new segment, re-scanning only that tail, and
/// the common no-match case is rejected by `quick_reject` before Aho-Corasick
/// even runs. Carrying a streaming LeftmostFirst automaton across chunks is
/// also infeasible with the `aho-corasick` crate (no resumable LeftmostFirst
/// stream API — which is precisely why the overlap-re-scan design exists).
///
/// This bench isolates that tail-overlap re-scan cost so a quiet-host A/B can
/// confirm it is far below the round-6 >=2x certifiable bar. Two arms over an
/// identical non-matching segment stream (steady-state pane chatter — the
/// dominant case):
///   - `tail_overlap` — `detect_with_context` (tail re-scan active, prod path)
///   - `no_tail`      — `detect` (no cross-segment tail; counterfactual floor)
/// across a small (128 B, worst-case *relative* overhead since the 2048 B tail
/// dwarfs the segment) and large (8 KiB, typical bulk) segment regime. The
/// wall-clock ratio between the arms is the tail-overlap overhead.
fn chatty_no_match_stream(seg_bytes: usize, count: usize) -> Vec<String> {
    // Realistic non-matching pane chatter — contains no anchor strings, so
    // every scan is handled by quick_reject (the steady-state common case).
    let filler = "heartbeat ok; compile queue steady; tokens sampled; no banner here. ";
    let mut segments = Vec::with_capacity(count);
    for idx in 0..count {
        let mut seg = String::with_capacity(seg_bytes + 24);
        while seg.len() < seg_bytes {
            let _ = write!(seg, "seg{idx} {filler}");
        }
        seg.truncate(seg_bytes.max(1)); // ASCII-only filler → byte truncation is char-safe
        seg.push('\n');
        segments.push(seg);
    }
    segments
}

fn bench_b1_cross_chunk_rescan(c: &mut Criterion) {
    let engine = PatternEngine::new();
    let _ = engine.detect("warmup");

    let mut group = c.benchmark_group("b1_cross_chunk_rescan");
    for &seg_bytes in &[128usize, 8192usize] {
        // ~512 KiB of streamed segments per regime.
        let count = (512 * 1024 / seg_bytes).max(8);
        let segments = chatty_no_match_stream(seg_bytes, count);
        let total_bytes: usize = segments.iter().map(String::len).sum();
        group.throughput(Throughput::Bytes(total_bytes as u64));

        // Arm A — production cross-chunk path (bounded tail-overlap re-scan).
        group.bench_with_input(
            BenchmarkId::new("tail_overlap", format!("{seg_bytes}B")),
            &segments,
            |b, segments| {
                b.iter_batched(
                    || {
                        let mut ctx = DetectionContext::new();
                        ctx.pane_id = Some(1);
                        ctx
                    },
                    |mut ctx| {
                        let mut hits = 0usize;
                        for seg in segments {
                            hits += engine.detect_with_context(seg, &mut ctx).len();
                        }
                        hits
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        // Arm B — no cross-segment tail (counterfactual lower bound).
        group.bench_with_input(
            BenchmarkId::new("no_tail", format!("{seg_bytes}B")),
            &segments,
            |b, segments| {
                b.iter(|| {
                    let mut hits = 0usize;
                    for seg in segments {
                        hits += engine.detect(seg).len();
                    }
                    hits
                });
            },
        );
    }
    group.finish();
}

/// B5 candidate (ft-p4vzl, found during ft-p4vzl.2) — the default-ON Bloom
/// `quick_reject` prefilter vs Aho-Corasick-direct on realistic no-match text.
///
/// EVIDENCE (`patterns.rs::quick_reject_with_index`): the prefilter sweeps the
/// whole text once per distinct anchor first-byte (builtin packs: 32 distinct
/// first-bytes → 32 memchr sweeps) and, for every byte-hit, Bloom-checks every
/// distinct *global* anchor length (25 lengths) even though only ~2.59 lengths
/// belong to that first-byte (9.6x inner-loop waste). The 32 first-bytes
/// include nearly every common English letter, so realistic no-match output
/// hits the inner loop on ~67% of byte positions → ~14.8 SipHash window-hashes
/// PER INPUT BYTE (~970k hashes for a 64 KiB segment), on top of 32 full memchr
/// sweeps — all to avoid a single exact Aho-Corasick pass that is ALREADY built
/// (`index.anchor_matcher`) and does zero hashing.
///
/// `set_quick_reject_enabled(false)` makes `detect()` skip the prefilter and go
/// straight to the AC matcher. Byte-equivalent: a Bloom filter has no false
/// negatives, so `quick_reject` never rejects a text the AC matcher would match;
/// disabling it only runs the exact matcher on more inputs → identical output.
/// Arms (one run, compare IDs):
///   - `quick_reject_on` — Bloom prefilter then AC (the pre-9137b11ab default)
///   - `ac_direct`       — `quick_reject` disabled (AC matcher only)
fn bench_quick_reject_vs_ac_direct(c: &mut Criterion) {
    let on = PatternEngine::new();
    let _ = on.detect("warmup");
    let mut off = PatternEngine::new();
    off.set_quick_reject_enabled(false);
    let _ = off.detect("warmup");

    let mut group = c.benchmark_group("quick_reject_vs_ac_direct");
    for size_kb in [1usize, 4, 16, 64] {
        let content = large_output(size_kb);
        group.throughput(Throughput::Bytes(content.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("quick_reject_on", format!("{size_kb}KB")),
            &content,
            |b, content| b.iter(|| on.detect(content)),
        );
        group.bench_with_input(
            BenchmarkId::new("ac_direct", format!("{size_kb}KB")),
            &content,
            |b, content| b.iter(|| off.detect(content)),
        );
    }
    group.finish();
}

fn bench_lazy_init(c: &mut Criterion) {
    let mut group = c.benchmark_group("pattern_lazy_init");

    // Budget: construction must be fast (no compilation)
    group.bench_function("construction_only", |b| {
        b.iter(|| {
            let engine = PatternEngine::new();
            assert!(!engine.is_initialized());
            engine
        });
    });

    // First detect() triggers compilation — measure the one-time cost
    group.bench_function("first_detect_cold", |b| {
        b.iter(|| {
            let engine = PatternEngine::new();
            engine.detect(TYPICAL_NO_MATCH)
        });
    });

    // Subsequent detect() should be fast (index already compiled)
    group.bench_function("subsequent_detect_warm", |b| {
        let engine = PatternEngine::new();
        let _ = engine.detect("warmup");
        assert!(engine.is_initialized());
        b.iter(|| engine.detect(TYPICAL_NO_MATCH));
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("pattern_detection", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_quick_reject,
        bench_pattern_detection,
        bench_detection_with_context,
        bench_throughput,
        bench_b1_cross_chunk_rescan,
        bench_quick_reject_vs_ac_direct,
        bench_lazy_init
);
criterion_main!(benches);
