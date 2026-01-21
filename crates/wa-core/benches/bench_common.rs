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
    let bench_path = format!("{}/{}", criterion_root, bench);
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
