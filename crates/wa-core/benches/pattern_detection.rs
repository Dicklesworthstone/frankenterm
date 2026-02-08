//! Benchmarks for pattern detection engine.
//!
//! Performance budgets (from PLAN §13.4 + Appendix G.7):
//! - Quick reject no-match: **< 1µs** for typical non-matching text
//! - Pattern detection (typical corpus): **p50 < 1ms**, **p99 < 5ms**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use wa_core::patterns::{DetectionContext, PatternEngine};

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
   Compiling wa-core v0.1.0 (/path/to/project)
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
        bench_lazy_init
);
criterion_main!(benches);
