//! Hot-path self-time baseline bench (gauntlet FND-002 / GA-FND-002-bench).
//!
//! Exercises the three hottest loops — `extract_delta`, `ScanPipeline::process`,
//! `Redactor::redact` — under the `hot-path-metrics` feature, then reads
//! `hot_path_metrics::snapshot()` and emits per-frame self-time as JSON. This is
//! the MT8-attribution substrate: it turns the per-frame counters into a
//! comparable artifact a `.bench-history` ratchet can diff over time.
//!
//! Run:
//!   cargo bench -p frankenterm-core --features hot-path-metrics --bench hot_path_self_time
//!
//! HONESTY: the emitted numbers are a single-host datapoint from the bench
//! profile; they are NOT an attested cross-engine perf CLAIM and must not be used
//! as one. The value here is (a) the harness and (b) the first comparable datapoint.

fn main() {
    #[cfg(not(feature = "hot-path-metrics"))]
    {
        eprintln!(
            "hot_path_self_time: rebuild with --features hot-path-metrics to capture self-time"
        );
    }
    #[cfg(feature = "hot-path-metrics")]
    run();
}

#[cfg(feature = "hot-path-metrics")]
fn run() {
    use frankenterm_core::hot_path_metrics::{reset, snapshot};
    use frankenterm_core::ingest::extract_delta;
    use frankenterm_core::redactor::Redactor;
    use frankenterm_core::scan_pipeline::{ScanPipeline, ScanPipelineConfig};

    const WARMUP: usize = 2_000;
    const ITERS: usize = 50_000;

    // Terminal-output-shaped representative workloads.
    let prev = "line one\nline two\nbuilding...\n".repeat(40);
    let cur = format!("{prev}compiling frankenterm-core v0.3.0\n");
    let scan_bytes = cur.as_bytes().to_vec();
    let redact_text = format!(
        "{}\nleaked sk-proj-abcdefghijklmnopqrstuvwxyz012345 and AKIAIOSFODNN7EXAMPLE\n",
        "normal log output ".repeat(30)
    );
    let pipeline = ScanPipeline::new(ScanPipelineConfig::default());
    let redactor = Redactor::new();

    // Warmup (not measured).
    for _ in 0..WARMUP {
        std::hint::black_box(extract_delta(&prev, &cur, 4096));
        std::hint::black_box(pipeline.process(&scan_bytes));
        std::hint::black_box(redactor.redact(&redact_text));
    }

    reset();
    for _ in 0..ITERS {
        std::hint::black_box(extract_delta(&prev, &cur, 4096));
        std::hint::black_box(pipeline.process(&scan_bytes));
        std::hint::black_box(redactor.redact(&redact_text));
    }
    let snap = snapshot();

    let mut frames: Vec<_> = snap.into_iter().collect();
    frames.sort_by(|a, b| a.0.cmp(b.0));

    println!("{{");
    println!("  \"schema\": \"gauntlet.hot_path_self_time.v1\",");
    println!("  \"iters\": {ITERS},");
    println!(
        "  \"note\": \"single-host dev datapoint from per-frame in-process timers; NOT an attested release-perf perf claim\","
    );
    println!("  \"frames\": {{");
    let n = frames.len();
    for (i, (name, st)) in frames.iter().enumerate() {
        let comma = if i + 1 < n { "," } else { "" };
        println!(
            "    \"{name}\": {{ \"count\": {}, \"total_nanos\": {}, \"mean_nanos\": {}, \"max_nanos\": {} }}{comma}",
            st.count,
            st.total_nanos,
            st.mean_nanos(),
            st.max_nanos
        );
    }
    println!("  }}");
    println!("}}");
}
