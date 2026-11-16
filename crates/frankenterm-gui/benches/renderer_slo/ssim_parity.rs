//! SSIM parity renderer SLO Criterion substrate.
//!
//! The retained release-gate test owns oracle-vs-subject correctness. This
//! bench keeps the GUI SLO suite measurable by timing the public comparator over
//! the retained golden corpus and emitting the same structured evidence row
//! shape used by the other renderer SLO benches.

use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_gui::gpu_regression::{Thresholds, compare_images};
use frankenterm_gui::renderer_slo::{
    RENDERER_SSIM_PARITY_CURRENT_DEGRADATION,
    RENDERER_SSIM_PARITY_DEFAULT_MAX_CHANGED_PIXEL_FRACTION_PPM,
    RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF, RENDERER_SSIM_PARITY_DEFAULT_MIN_SSIM_PPM,
    RENDERER_SSIM_PARITY_STATUS,
};
use image::{ImageReader, RgbaImage};
use serde::Deserialize;
use serde_json::json;
use walkdir::WalkDir;

const CLAIM_ID: &str = "renderer.ssim_parity_floor";
const STRUCTURED_LOG: &str = "target/criterion/slo-ssim_parity.jsonl";
const SOURCE_BENCH: &str = "crates/frankenterm-gui/benches/renderer_slo/ssim_parity.rs";

#[derive(Debug, Clone)]
struct CorpusFixture {
    name: String,
    golden_png: PathBuf,
    thresholds: Thresholds,
}

#[derive(Debug)]
struct LoadedFixture {
    name: String,
    image: RgbaImage,
    thresholds: Thresholds,
}

#[derive(Debug, Deserialize)]
struct FixtureMeta {
    fixture: String,
    #[serde(default)]
    thresholds: Option<Thresholds>,
}

#[derive(Debug)]
enum FixtureDiscoveryError {
    FixtureMetadata(String),
    GoldenFixtureMissingParent,
    FixturePathOutsideCorpus,
    RepoRootUnavailable(String),
    WalkGoldenCorpus,
}

#[derive(Debug)]
struct SsimEvidence {
    sample_count: usize,
    min_ssim: Option<f64>,
    max_l_inf: Option<u8>,
    max_changed_pixel_fraction: Option<f64>,
    within_target: Option<bool>,
    state: &'static str,
    degradation_reason: Option<String>,
}

fn bench_golden_corpus_self_compare(c: &mut Criterion) {
    let fixtures = match load_fixtures() {
        Ok(fixtures) if !fixtures.is_empty() => fixtures,
        Ok(_) | Err(_) => {
            c.bench_function("ssim_parity/corpus_unavailable_noop", |b| {
                b.iter(|| black_box(()));
            });
            return;
        }
    };

    c.bench_function("ssim_parity/golden_corpus_self_compare", |b| {
        b.iter(|| {
            let mut all_passed = true;
            for fixture in &fixtures {
                match compare_images(
                    black_box(&fixture.image),
                    black_box(&fixture.image),
                    fixture.thresholds,
                ) {
                    Ok(result) => {
                        all_passed &= result.passed;
                        black_box(result.metrics.ssim);
                        black_box(result.metrics.l_inf);
                        black_box(result.metrics.changed_pixel_fraction);
                    }
                    Err(error) => {
                        black_box(error);
                        all_passed = false;
                    }
                }
            }
            black_box(all_passed);
        });
    });
}

fn bench_config() -> Criterion {
    emit_evidence_row();
    Criterion::default().configure_from_args()
}

fn emit_evidence_row() {
    let evidence = measure_corpus();
    let row = json!({
        "schema_version": "ft.perf.evidence-sample.v1",
        "ts_ms": now_ms(),
        "claim_id": CLAIM_ID,
        "metric_name": "min_ssim",
        "metric_value": evidence.min_ssim.unwrap_or(0.0),
        "metric_unit": "ratio",
        "sample_size": evidence.sample_count.max(1),
        "target": "SSIM >= 0.99, L_inf <= 8, changed-pixel fraction <= 0.001",
        "target_min_ssim_ppm": RENDERER_SSIM_PARITY_DEFAULT_MIN_SSIM_PPM,
        "target_max_l_inf": RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF,
        "target_max_changed_pixel_fraction_ppm": RENDERER_SSIM_PARITY_DEFAULT_MAX_CHANGED_PIXEL_FRACTION_PPM,
        "within_target": evidence.within_target,
        "max_l_inf": evidence.max_l_inf,
        "max_changed_pixel_fraction": evidence.max_changed_pixel_fraction,
        "source_bench": SOURCE_BENCH,
        "structured_log": STRUCTURED_LOG,
        "commit_sha": option_env!("VERGEN_GIT_SHA"),
        "hardware_fingerprint": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "runner_sku": std::env::var("RUNNER_OS").unwrap_or_else(|_| std::env::consts::OS.to_string()),
        "workload_class": "retained-gpu-golden-corpus-self-compare",
        "tags": {
            "frankenterm_version": env!("CARGO_PKG_VERSION"),
            "renderer_slo_state": evidence.state,
            "renderer_slo_status": RENDERER_SSIM_PARITY_STATUS,
            "current_degradation": RENDERER_SSIM_PARITY_CURRENT_DEGRADATION,
            "degradation_reason": evidence.degradation_reason.unwrap_or_else(|| "none".to_string()),
            "retained_release_artifact_required": "true"
        }
    });

    let evidence_path = PathBuf::from(STRUCTURED_LOG);
    if let Some(parent) = evidence_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&evidence_path)
    {
        let _ = writeln!(file, "{row}");
    }
    println!("[BENCH] ssim_parity_evidence={}", evidence_path.display());
}

fn measure_corpus() -> SsimEvidence {
    let fixtures = match load_fixtures() {
        Ok(fixtures) => fixtures,
        Err(reason) => {
            return SsimEvidence {
                sample_count: 0,
                min_ssim: None,
                max_l_inf: None,
                max_changed_pixel_fraction: None,
                within_target: None,
                state: "corpus_unavailable",
                degradation_reason: Some(reason),
            };
        }
    };
    if fixtures.is_empty() {
        return SsimEvidence {
            sample_count: 0,
            min_ssim: None,
            max_l_inf: None,
            max_changed_pixel_fraction: None,
            within_target: None,
            state: "corpus_unavailable",
            degradation_reason: Some("tests/golden/gpu has no retained golden PNG fixtures".into()),
        };
    }

    let mut min_ssim = f64::INFINITY;
    let mut max_l_inf = 0u8;
    let mut max_changed_pixel_fraction = 0.0_f64;
    let mut all_passed = true;
    let sample_count = fixtures.len();
    for fixture in fixtures {
        let result = match compare_images(&fixture.image, &fixture.image, fixture.thresholds) {
            Ok(result) => result,
            Err(_) => {
                return SsimEvidence {
                    sample_count,
                    min_ssim: None,
                    max_l_inf: None,
                    max_changed_pixel_fraction: None,
                    within_target: None,
                    state: "dimension_mismatch",
                    degradation_reason: Some(fixture.name),
                };
            }
        };
        all_passed &= result.passed;
        min_ssim = min_ssim.min(result.metrics.ssim);
        max_l_inf = max_l_inf.max(result.metrics.l_inf);
        max_changed_pixel_fraction =
            max_changed_pixel_fraction.max(result.metrics.changed_pixel_fraction);
    }

    SsimEvidence {
        sample_count,
        min_ssim: Some(min_ssim),
        max_l_inf: Some(max_l_inf),
        max_changed_pixel_fraction: Some(max_changed_pixel_fraction),
        within_target: Some(all_passed),
        state: if all_passed {
            "measured"
        } else {
            "metric_threshold_exceeded"
        },
        degradation_reason: if all_passed {
            None
        } else {
            Some("one or more retained golden fixtures violated the SSIM floor".into())
        },
    }
}

fn load_fixtures() -> Result<Vec<LoadedFixture>, String> {
    discover_corpus_fixtures()
        .map_err(fixture_discovery_error_message)?
        .into_iter()
        .map(|fixture| {
            let image = ImageReader::open(&fixture.golden_png)
                .map_err(|error| format!("open {}: {error}", fixture.golden_png.display()))?
                .decode()
                .map_err(|error| format!("decode {}: {error}", fixture.golden_png.display()))?
                .into_rgba8();
            Ok(LoadedFixture {
                name: fixture.name,
                image,
                thresholds: fixture.thresholds,
            })
        })
        .collect()
}

fn discover_corpus_fixtures() -> Result<Vec<CorpusFixture>, FixtureDiscoveryError> {
    let corpus_root = repo_root()
        .map_err(FixtureDiscoveryError::RepoRootUnavailable)?
        .join("tests/golden/gpu");
    let mut fixtures = Vec::new();

    for entry in WalkDir::new(&corpus_root) {
        let entry = entry.map_err(|_| FixtureDiscoveryError::WalkGoldenCorpus)?;
        if !entry.file_type().is_file() || entry.file_name() != "golden.png" {
            continue;
        }

        let dir = entry
            .path()
            .parent()
            .ok_or(FixtureDiscoveryError::GoldenFixtureMissingParent)?;
        let fixture_name = dir
            .strip_prefix(&corpus_root)
            .map_err(|_| FixtureDiscoveryError::FixturePathOutsideCorpus)?
            .to_string_lossy()
            .replace('\\', "/");

        fixtures.push(CorpusFixture {
            thresholds: load_fixture_metadata(&dir.join("meta.json"), &fixture_name)
                .map_err(FixtureDiscoveryError::FixtureMetadata)?,
            name: fixture_name,
            golden_png: entry.path().to_path_buf(),
        });
    }

    fixtures.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(fixtures)
}

fn fixture_discovery_error_message(error: FixtureDiscoveryError) -> String {
    match error {
        FixtureDiscoveryError::FixtureMetadata(reason)
        | FixtureDiscoveryError::RepoRootUnavailable(reason) => reason,
        FixtureDiscoveryError::GoldenFixtureMissingParent => {
            "golden fixture has no parent".to_string()
        }
        FixtureDiscoveryError::FixturePathOutsideCorpus => {
            "fixture path outside GPU corpus".to_string()
        }
        FixtureDiscoveryError::WalkGoldenCorpus => "walk golden corpus failed".to_string(),
    }
}

fn load_fixture_metadata(path: &Path, fixture_name: &str) -> Result<Thresholds, String> {
    if !path.exists() {
        return Ok(Thresholds::default());
    }

    let text =
        fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let meta: FixtureMeta = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if meta.fixture.as_str() != fixture_name {
        return Err(format!(
            "fixture metadata name mismatch: expected {fixture_name}, got {}",
            meta.fixture
        ));
    }
    Ok(meta.thresholds.unwrap_or_default())
}

fn repo_root() -> Result<PathBuf, String> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| "frankenterm-gui crate must live under crates/".to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_golden_corpus_self_compare
);
criterion_main!(benches);
