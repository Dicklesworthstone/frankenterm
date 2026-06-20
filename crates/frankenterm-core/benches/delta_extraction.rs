//! Benchmarks for delta extraction (overlap matching).
//!
//! This is the hot ingest path - every capture runs through delta extraction.
//!
//! Performance budgets:
//! - Delta extraction should complete in microseconds, not milliseconds
//! - Should scale reasonably with content size up to typical pane buffers (~100KB)

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::ingest::{extract_delta, extract_delta_with_overlap_mode};
use std::fmt::Write;
use std::hint::black_box;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "delta_extraction",
        budget: "p50 < 200µs, p99 < 1ms (typical overlap sizes)",
    },
    bench_common::BenchBudget {
        name: "delta_adversarial_overlap",
        budget: "Q3 forced A/B: KMP (linear) overlap should beat legacy memchr (quadratic) \
                 by >=2x on repeated-first-byte input where the O(n^2) scan dominates",
    },
];

/// Default overlap window size from RuntimeConfig.
const DEFAULT_OVERLAP_SIZE: usize = 4096;

/// Generate terminal-like content of specified approximate size.
fn generate_content(lines: usize) -> String {
    let mut content = String::with_capacity(lines * 80);
    for i in 0..lines {
        let _ = writeln!(
            &mut content,
            "[{}] Processing item {} - status: OK - elapsed: {}ms",
            i % 1000,
            i,
            (i * 7) % 100
        );
    }
    content
}

/// Scenario: Append-only (typical shell output).
/// Previous content is a prefix of current content.
fn bench_append_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_append_only");

    for lines in [10, 100, 500, 1000] {
        let prev = generate_content(lines);
        // Current is previous + 10 more lines
        let curr = format!("{}{}", prev, generate_content(10));

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("lines", lines),
            &(prev.clone(), curr.clone()),
            |b, (prev, curr)| b.iter(|| extract_delta(prev, curr, DEFAULT_OVERLAP_SIZE)),
        );
    }

    group.finish();
}

/// Scenario: No change (content identical).
fn bench_no_change(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_no_change");

    for lines in [10, 100, 500, 1000] {
        let content = generate_content(lines);

        group.throughput(Throughput::Bytes(content.len() as u64));
        group.bench_with_input(BenchmarkId::new("lines", lines), &content, |b, content| {
            b.iter(|| extract_delta(content, content, DEFAULT_OVERLAP_SIZE));
        });
    }

    group.finish();
}

/// Scenario: Small edit (middle of content changes).
/// This should fail overlap matching and produce a Gap.
fn bench_edit_middle(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_edit_middle");

    for lines in [100, 500, 1000] {
        let prev = generate_content(lines);
        // Modify a line in the middle
        let curr = prev.replacen("status: OK", "status: CHANGED", 1);

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("lines", lines),
            &(prev.clone(), curr.clone()),
            |b, (prev, curr)| b.iter(|| extract_delta(prev, curr, DEFAULT_OVERLAP_SIZE)),
        );
    }

    group.finish();
}

/// Scenario: Scrollback truncation (common in terminal buffers).
/// Previous content is longer, current content is a suffix.
fn bench_truncation(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_truncation");

    for prev_lines in [500, 1000, 2000] {
        let prev = generate_content(prev_lines);
        // Current keeps only last 100 lines (simulating scrollback)
        let lines: Vec<&str> = prev.lines().collect();
        let curr = lines[lines.len().saturating_sub(100)..].join("\n") + "\n";

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("prev_lines", prev_lines),
            &(prev.clone(), curr.clone()),
            |b, (prev, curr)| b.iter(|| extract_delta(prev, curr, DEFAULT_OVERLAP_SIZE)),
        );
    }

    group.finish();
}

/// Scenario: Varying overlap window sizes.
fn bench_overlap_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_overlap_sizes");

    let lines = 500;
    let prev = generate_content(lines);
    let curr = format!("{}{}", prev, generate_content(5));

    for overlap_size in [512, 1024, 2048, 4096, 8192] {
        group.bench_with_input(
            BenchmarkId::new("overlap_size", overlap_size),
            &(prev.clone(), curr.clone(), overlap_size),
            |b, (prev, curr, overlap)| b.iter(|| extract_delta(prev, curr, *overlap)),
        );
    }

    group.finish();
}

/// Scenario: First capture (empty previous).
fn bench_first_capture(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_first_capture");

    for lines in [10, 100, 500] {
        let curr = generate_content(lines);

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(BenchmarkId::new("lines", lines), &curr, |b, curr| {
            b.iter(|| extract_delta("", curr, DEFAULT_OVERLAP_SIZE));
        });
    }

    group.finish();
}

/// Scenario: Large content (stress test).
fn bench_large_content(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_large_content");
    group.sample_size(20); // Fewer samples for large content

    for lines in [5000, 10000] {
        let prev = generate_content(lines);
        let curr = format!("{}{}", prev, generate_content(10));

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("lines", lines),
            &(prev.clone(), curr.clone()),
            |b, (prev, curr)| b.iter(|| extract_delta(prev, curr, DEFAULT_OVERLAP_SIZE)),
        );
    }

    group.finish();
}

/// Build the Q3 adversarial repeated-first-byte pair for a given overlap window
/// `w`, returning `(previous, current, overlap_size)`.
///
/// Shape (analysis pinned by `proptest_ingest_delta_linear_overlap_equivalence`'s
/// `arb_repeated_run_pair`): `previous` is `w` copies of a single byte — the
/// realistic terminal case of a run of padding spaces / box-drawing / separator
/// dashes filling the overlap window. `current` is the *same* run with its
/// divergence char planted at index `w/2`, so it shares `current[0]` with the
/// window (every window byte is a `memchr` hit) but only overlaps at the back
/// half:
///
/// * Legacy arm: `memchr` yields all `w` positions; for each of the `~w/2`
///   positions whose candidate overlap still spans the divergence char, the
///   slice compare agrees for `~w/2` bytes before failing — `O(w^2/4)` work
///   before the first real match at `overlap_len = w/2`.
/// * KMP arm: a single forward pass, `O(w)`.
///
/// Both arms return the identical `DeltaResult::Content(current[w/2..])`; the
/// bench asserts that equality at setup so the two IDs can never silently
/// measure different work. `current.len() == previous.len() == w` keeps the
/// pure-append fast path (`current.len() > previous.len()`) out of the picture
/// so the overlap search actually runs.
fn adversarial_repeated_run(w: usize) -> (String, String, usize) {
    const RUN_BYTE: char = ' '; // padding spaces are the most common real-world run
    const DIVERGE: char = 'X';

    let previous: String = std::iter::repeat(RUN_BYTE).take(w).collect();
    let split = w / 2;
    let mut current = String::with_capacity(w);
    current.extend(std::iter::repeat(RUN_BYTE).take(split));
    current.push(DIVERGE);
    current.extend(std::iter::repeat(RUN_BYTE).take(w - split - 1));
    debug_assert_eq!(current.len(), w);

    // Setup-time guard: both arms must agree, else the A/B is comparing apples
    // to oranges. (Proven for all inputs by the equivalence proptest; re-checked
    // here on the exact benched payload.)
    let quad = extract_delta_with_overlap_mode(&previous, &current, w, false);
    let kmp = extract_delta_with_overlap_mode(&previous, &current, w, true);
    assert!(
        matches!(
            (&quad, &kmp),
            (
                frankenterm_core::ingest::DeltaResult::Content(a),
                frankenterm_core::ingest::DeltaResult::Content(b),
            ) if a == b
        ),
        "Q3 adversarial arms disagree at w={w}: quad={quad:?} kmp={kmp:?}"
    );

    (previous, current, w)
}

/// Build a *benign* sliding-window overlap pair for window `w` — the common
/// scrollback-capture shape the gate must NOT regress. `current` begins with a
/// genuine suffix of `previous` made of *varied* terminal lines, so `current[0]`
/// is a rare byte in the window and the legacy loop's first `memchr` hit
/// (pos 0 = largest overlap) is the real match: a single SIMD `memcmp`. KMP, by
/// contrast, always pays its `O(pattern)` prefix-function build, so it is
/// expected to *lose* here — the measurement that turns "KMP wins" into the
/// correct nuanced recommendation (gate/adaptive, not static default-on).
fn benign_sliding_window(w: usize) -> (String, String, usize) {
    // Varied realistic lines, comfortably longer than the window.
    let previous = generate_content(w / 25 + 200);
    let mut start = previous.len().saturating_sub(w);
    while start < previous.len() && !previous.is_char_boundary(start) {
        start += 1;
    }
    // current = a real tail of `previous` (the still-visible region) + new
    // lines. Not a pure append of all of `previous`, so the append fast path is
    // skipped and the overlap search actually runs.
    let shared = &previous[start..];
    let current = format!("{shared}{}", generate_content(10));

    let quad = extract_delta_with_overlap_mode(&previous, &current, w, false);
    let kmp = extract_delta_with_overlap_mode(&previous, &current, w, true);
    assert!(
        matches!(
            (&quad, &kmp),
            (
                frankenterm_core::ingest::DeltaResult::Content(a),
                frankenterm_core::ingest::DeltaResult::Content(b),
            ) if a == b
        ),
        "benign arms disagree at w={w}: quad={quad:?} kmp={kmp:?}"
    );

    (previous, current, w)
}

/// Q3 forced A/B: legacy quadratic (gate OFF) vs KMP linear (gate ON) overlap
/// search on adversarial repeated-first-byte input, driven through the
/// `#[doc(hidden)]` [`extract_delta_with_overlap_mode`] entry point.
///
/// This bypasses the `FT_MOONSHOT_DELTA_LINEAR_OVERLAP` env gate — whose
/// `env::var_os().is_some()` parse makes the build-once env A/B unable to express
/// the OFF arm (empty-but-set = ON) — so both arms are reachable in a single run.
/// Stable IDs `legacy_quadratic`/`kmp_linear` per window let the orchestrator
/// read the ratio directly; the ratio should grow ~linearly with `w` (the
/// O(n^2) vs O(n) signature).
fn bench_adversarial_overlap_ab(c: &mut Criterion) {
    let mut group = c.benchmark_group("delta_adversarial_overlap");

    // 4096 is the shipping DEFAULT_OVERLAP_SIZE; 1024/2048 expose the quadratic
    // scaling (legacy time should ~4x per doubling, KMP ~2x).
    for w in [1024usize, 2048, 4096] {
        let (prev, curr, overlap) = adversarial_repeated_run(w);

        group.throughput(Throughput::Bytes(curr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("legacy_quadratic", w),
            &(prev.clone(), curr.clone(), overlap),
            |b, (prev, curr, overlap)| {
                b.iter(|| {
                    black_box(extract_delta_with_overlap_mode(
                        black_box(prev),
                        black_box(curr),
                        *overlap,
                        false,
                    ))
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("kmp_linear", w),
            &(prev, curr, overlap),
            |b, (prev, curr, overlap)| {
                b.iter(|| {
                    black_box(extract_delta_with_overlap_mode(
                        black_box(prev),
                        black_box(curr),
                        *overlap,
                        true,
                    ))
                });
            },
        );

        // Common-case guard rail: a benign sliding-window overlap, where legacy's
        // single SIMD memcmp at pos 0 is expected to beat KMP's fixed
        // preprocessing. If KMP also wins (or ties) here, a static default-on is
        // safe; if legacy wins, the gate should stay off / go adaptive.
        let (bprev, bcurr, boverlap) = benign_sliding_window(w);
        group.throughput(Throughput::Bytes(bcurr.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("benign_legacy", w),
            &(bprev.clone(), bcurr.clone(), boverlap),
            |b, (prev, curr, overlap)| {
                b.iter(|| {
                    black_box(extract_delta_with_overlap_mode(
                        black_box(prev),
                        black_box(curr),
                        *overlap,
                        false,
                    ))
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("benign_kmp", w),
            &(bprev, bcurr, boverlap),
            |b, (prev, curr, overlap)| {
                b.iter(|| {
                    black_box(extract_delta_with_overlap_mode(
                        black_box(prev),
                        black_box(curr),
                        *overlap,
                        true,
                    ))
                });
            },
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("delta_extraction", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_append_only,
        bench_no_change,
        bench_edit_middle,
        bench_truncation,
        bench_overlap_sizes,
        bench_first_capture,
        bench_large_content,
        bench_adversarial_overlap_ab
);
criterion_main!(benches);
