#![allow(dead_code)] // Shared utility module; not all benchmarks use all functions.

use serde::Serialize;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Serialize)]
pub struct BenchBudget {
    pub name: &'static str,
    pub budget: &'static str,
}

/// Machine-readable budget threshold for CI enforcement.
///
/// Each entry maps a Criterion benchmark group prefix to a coarse
/// median-nanosecond ceiling.  The CI script (`scripts/check_bench_budgets.sh`)
/// reads `target/criterion/wa-budgets.json` and fails the build when any
/// benchmark's median exceeds the corresponding threshold.
///
/// Thresholds are intentionally 10x the design budgets to absorb CI noise
/// while still catching gross regressions (order-of-magnitude slowdowns).
#[derive(Serialize)]
#[allow(dead_code)]
pub struct CiBudgetEntry {
    /// Criterion group prefix (matched via starts-with on the group/bench path).
    pub group_prefix: &'static str,
    /// Maximum allowed median in nanoseconds (~10x observed current perf).
    pub max_median_ns: u64,
    /// Human-readable note.
    pub note: &'static str,
}

/// Canonical CI budget table.  One row per benchmark group.
///
/// Ceilings are set at ~10x the *observed* current performance (not the
/// aspirational design targets) so that CI catches gross regressions without
/// false positives from normal variance or CI machine differences.
#[allow(dead_code)]
pub const CI_BUDGETS: &[CiBudgetEntry] = &[
    // Pattern quick reject: observed ~100µs-2.7ms (varies by input size) → ceiling 5ms
    CiBudgetEntry {
        group_prefix: "pattern_quick_reject",
        max_median_ns: 5_000_000,
        note: "quick reject: observed ~100us-2.7ms, ceiling 5ms",
    },
    // Pattern detection: observed ~300-400µs → ceiling 5ms
    CiBudgetEntry {
        group_prefix: "pattern_detection/",
        max_median_ns: 5_000_000,
        note: "pattern detect: observed ~300-400us, ceiling 5ms",
    },
    // Pattern throughput (64KB can take ~20ms): ceiling 200ms
    CiBudgetEntry {
        group_prefix: "pattern_throughput",
        max_median_ns: 200_000_000,
        note: "pattern throughput: observed ~20ms at 64KB, ceiling 200ms",
    },
    // Pattern detection with context: similar to base detection
    CiBudgetEntry {
        group_prefix: "pattern_detection_context",
        max_median_ns: 5_000_000,
        note: "pattern w/ context: observed ~350us, ceiling 5ms",
    },
    // Lazy init: construction without compilation (pack loading only)
    CiBudgetEntry {
        group_prefix: "pattern_lazy_init/construction_only",
        max_median_ns: 50_000_000,
        note: "lazy construction: observed ~12ms, ceiling 50ms",
    },
    // Lazy init: cold detect (construction + first compilation)
    CiBudgetEntry {
        group_prefix: "pattern_lazy_init/first_detect_cold",
        max_median_ns: 200_000_000,
        note: "cold first detect: observed ~25ms, ceiling 200ms",
    },
    // Lazy init: warm detect (already compiled)
    CiBudgetEntry {
        group_prefix: "pattern_lazy_init/subsequent_detect_warm",
        max_median_ns: 5_000_000,
        note: "warm detect: observed ~40us, ceiling 5ms",
    },
    // Delta extraction: design < 200µs → ceiling 5ms
    CiBudgetEntry {
        group_prefix: "delta_extraction",
        max_median_ns: 5_000_000,
        note: "delta extraction: ceiling 5ms",
    },
    // FTS query: design < 10ms, DB-bound → ceiling 500ms
    CiBudgetEntry {
        group_prefix: "fts_query",
        max_median_ns: 500_000_000,
        note: "FTS query: DB-bound, ceiling 500ms",
    },
    // Storage append: design p95 < 2ms → ceiling 50ms
    CiBudgetEntry {
        group_prefix: "storage_single_append",
        max_median_ns: 50_000_000,
        note: "segment append: ceiling 50ms",
    },
    // Storage batch: DB-bound, can be slow → ceiling 500ms
    CiBudgetEntry {
        group_prefix: "storage_batch_append",
        max_median_ns: 500_000_000,
        note: "batch append: DB-bound, ceiling 500ms",
    },
    // FTS regression: DB-bound → ceiling 500ms
    CiBudgetEntry {
        group_prefix: "storage_fts_regression",
        max_median_ns: 500_000_000,
        note: "FTS search regression: DB-bound, ceiling 500ms",
    },
    // Upsert pane: design p95 < 1ms → ceiling 20ms
    CiBudgetEntry {
        group_prefix: "storage_upsert_pane",
        max_median_ns: 20_000_000,
        note: "upsert pane: ceiling 20ms",
    },
    // Watcher loop: design < 100µs → ceiling 5ms
    CiBudgetEntry {
        group_prefix: "watcher_loop",
        max_median_ns: 5_000_000,
        note: "watcher loop: ceiling 5ms",
    },
    // Backpressure tier classify: design < 100ns → ceiling 10µs
    CiBudgetEntry {
        group_prefix: "backpressure_tier",
        max_median_ns: 10_000,
        note: "bp tier classify: ceiling 10us",
    },
    // Scheduler select: design < 5µs (100 panes) → ceiling 500µs
    CiBudgetEntry {
        group_prefix: "backpressure_scheduler",
        max_median_ns: 500_000,
        note: "bp scheduler: ceiling 500us",
    },
    // Sizing insert: DB-bound, can be slow → ceiling 2s
    CiBudgetEntry {
        group_prefix: "sizing_insert",
        max_median_ns: 2_000_000_000,
        note: "sizing insert: DB-bound, ceiling 2s",
    },
    // Sizing query at scale: DB-bound → ceiling 1s
    CiBudgetEntry {
        group_prefix: "sizing_query",
        max_median_ns: 1_000_000_000,
        note: "sizing query at scale: DB-bound, ceiling 1s",
    },
];

#[derive(Serialize)]
struct BenchEnvironment {
    os: &'static str,
    arch: &'static str,
    rustc: Option<String>,
    cpu: Option<String>,
    features: Vec<String>,
}

#[derive(Serialize)]
struct BenchTestRun<'a> {
    test_type: &'static str,
    name: &'a str,
    status: &'static str,
}

#[derive(Serialize)]
struct BenchMetadata<'a> {
    test_type: &'static str,
    bench: &'a str,
    generated_at_ms: u64,
    wa_version: &'static str,
    budgets: &'a [BenchBudget],
    environment: BenchEnvironment,
}

#[derive(Serialize)]
struct BenchArtifact<'a> {
    #[serde(rename = "type")]
    artifact_type: &'a str,
    path: String,
    format: &'a str,
    description: &'a str,
    redacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

#[derive(Serialize)]
struct BenchManifest<'a> {
    version: &'static str,
    format: &'static str,
    generated_at_ms: u64,
    test_run: BenchTestRun<'a>,
    wa_version: &'static str,
    wa_commit: Option<&'static str>,
    budgets: &'a [BenchBudget],
    environment: BenchEnvironment,
    artifacts: Vec<BenchArtifact<'a>>,
}

pub fn emit_bench_artifacts(bench: &str, budgets: &[BenchBudget]) {
    let environment = build_environment();
    emit_bench_metadata(bench, budgets, &environment);
    emit_bench_manifest(bench, budgets, environment);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or_default()
}

fn build_environment() -> BenchEnvironment {
    BenchEnvironment {
        os: env::consts::OS,
        arch: env::consts::ARCH,
        rustc: rustc_version(),
        cpu: cpu_model(),
        features: cargo_features(),
    }
}

fn emit_bench_metadata(bench: &str, budgets: &[BenchBudget], environment: &BenchEnvironment) {
    let metadata = BenchMetadata {
        test_type: "bench",
        bench,
        generated_at_ms: now_ms(),
        wa_version: env!("CARGO_PKG_VERSION"),
        budgets,
        environment: BenchEnvironment {
            os: environment.os,
            arch: environment.arch,
            rustc: environment.rustc.clone(),
            cpu: environment.cpu.clone(),
            features: environment.features.clone(),
        },
    };

    if let Ok(line) = serde_json::to_string(&metadata) {
        println!("[BENCH] {line}");
        let _ = append_jsonl("target/criterion/wa-bench-meta.jsonl", &line);
    }
}

fn emit_bench_manifest(bench: &str, budgets: &[BenchBudget], environment: BenchEnvironment) {
    let manifest = BenchManifest {
        version: "1",
        format: "wa-bench-manifest",
        generated_at_ms: now_ms(),
        test_run: BenchTestRun {
            test_type: "bench",
            name: bench,
            status: "passed",
        },
        wa_version: env!("CARGO_PKG_VERSION"),
        wa_commit: option_env!("VERGEN_GIT_SHA"),
        budgets,
        environment,
        artifacts: bench_artifacts(bench),
    };

    if let Ok(payload) = serde_json::to_string_pretty(&manifest) {
        let path = format!("target/criterion/wa-bench-manifest-{bench}.json");
        if write_json(&path, &payload).is_ok() {
            println!("[BENCH] manifest={path}");
        }
    }
}

fn rustc_version() -> Option<String> {
    let output = Command::new("rustc").arg("-vV").output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("release: ") {
            return Some(rest.trim().to_string());
        }
    }
    stdout.lines().next().map(|line| line.trim().to_string())
}

fn cpu_model() -> Option<String> {
    if cfg!(target_os = "linux") {
        let contents = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        for line in contents.lines() {
            if line.starts_with("model name") {
                return line
                    .split_once(':')
                    .map(|(_, value)| value.trim().to_string());
            }
        }
        None
    } else if cfg!(target_os = "macos") {
        let output = Command::new("sysctl")
            .args(["-n", "machdep.cpu.brand_string"])
            .output()
            .ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let cpu = stdout.trim();
        if cpu.is_empty() {
            None
        } else {
            Some(cpu.to_string())
        }
    } else {
        env::var("PROCESSOR_IDENTIFIER").ok()
    }
}

fn cargo_features() -> Vec<String> {
    let mut features: Vec<String> = env::vars()
        .filter_map(|(key, _)| key.strip_prefix("CARGO_FEATURE_").map(str::to_string))
        .map(|feature| feature.to_lowercase().replace('_', "-"))
        .collect();
    features.sort();
    features
}

fn bench_artifacts(bench: &str) -> Vec<BenchArtifact<'_>> {
    let criterion_root = "target/criterion".to_string();
    let bench_path = format!("{criterion_root}/{bench}");
    vec![
        BenchArtifact {
            artifact_type: "meta",
            path: "target/criterion/wa-bench-meta.jsonl".to_string(),
            format: "jsonl",
            description: "Bench budgets + environment metadata",
            redacted: false,
            size_bytes: file_size("target/criterion/wa-bench-meta.jsonl"),
        },
        BenchArtifact {
            artifact_type: "criterion",
            path: criterion_root,
            format: "dir",
            description: "Criterion output directory",
            redacted: false,
            size_bytes: None,
        },
        BenchArtifact {
            artifact_type: "criterion_bench",
            path: bench_path,
            format: "dir",
            description: "Criterion output for bench",
            redacted: false,
            size_bytes: None,
        },
    ]
}

fn file_size(path: &str) -> Option<u64> {
    std::fs::metadata(path).map(|meta| meta.len()).ok()
}

fn append_jsonl(path: &str, line: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn write_json(path: &str, payload: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(payload.as_bytes())?;
    Ok(())
}
