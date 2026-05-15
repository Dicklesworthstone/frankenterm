//! SSIM parity SLO checks over the retained GPU golden corpus.
//!
//! These tests keep the public comparator, default SLO thresholds, and corpus
//! metadata aligned while the retained oracle-vs-subject release run is still
//! pending.

use frankenterm_gui::gpu_regression::{Thresholds, compare_images};
use frankenterm_gui::renderer_slo::{
    RENDERER_SSIM_PARITY_CURRENT_DEGRADATION,
    RENDERER_SSIM_PARITY_DEFAULT_MAX_CHANGED_PIXEL_FRACTION_PPM,
    RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF, RENDERER_SSIM_PARITY_DEFAULT_MIN_SSIM_PPM,
    RENDERER_SSIM_PARITY_MCP_RESOURCE_URI, RENDERER_SSIM_PARITY_STATUS,
};
use image::{ImageReader, Rgba, RgbaImage};
use proptest::prelude::*;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug)]
struct CorpusFixture {
    name: String,
    golden_png: PathBuf,
    input_json: PathBuf,
    expected_json: PathBuf,
    thresholds: Thresholds,
}

#[derive(Debug, Deserialize)]
struct FixtureMeta {
    fixture: String,
    #[serde(default)]
    thresholds: Option<Thresholds>,
}

#[derive(Debug, Deserialize)]
struct ExpectedStatus {
    status: String,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("frankenterm-gui crate must live under crates/")
        .to_path_buf()
}

fn load_fixture_metadata(path: &Path, fixture_name: &str) -> Thresholds {
    if !path.exists() {
        return Thresholds::default();
    }

    let text = fs::read_to_string(path).expect("read fixture metadata");
    let meta: FixtureMeta = serde_json::from_str(&text).expect("parse fixture metadata");
    assert_eq!(
        meta.fixture.as_str(),
        fixture_name,
        "fixture metadata name must match corpus-relative directory"
    );
    meta.thresholds.unwrap_or_default()
}

fn discover_corpus_fixtures() -> Vec<CorpusFixture> {
    let corpus_root = repo_root().join("tests/golden/gpu");
    let mut fixtures = Vec::new();

    for entry in WalkDir::new(&corpus_root) {
        let entry = entry.expect("walk golden corpus");
        if !entry.file_type().is_file() || entry.file_name() != "golden.png" {
            continue;
        }

        let dir = entry.path().parent().expect("golden fixture has parent");
        let fixture_name = dir
            .strip_prefix(&corpus_root)
            .expect("fixture must be under GPU golden corpus")
            .to_string_lossy()
            .replace('\\', "/");
        let input_json = dir.join("input.json");
        let expected_json = dir.join("expected.json");

        assert!(input_json.exists(), "{fixture_name} is missing input.json");
        assert!(
            expected_json.exists(),
            "{fixture_name} is missing expected.json"
        );

        fixtures.push(CorpusFixture {
            thresholds: load_fixture_metadata(&dir.join("meta.json"), &fixture_name),
            name: fixture_name,
            golden_png: entry.path().to_path_buf(),
            input_json,
            expected_json,
        });
    }

    fixtures.sort_by(|left, right| left.name.cmp(&right.name));
    fixtures
}

#[test]
fn golden_corpus_self_compare_meets_ssim_floor() {
    let fixtures = discover_corpus_fixtures();
    assert!(
        fixtures.len() >= 30,
        "SSIM parity corpus should retain broad GPU fixture coverage"
    );

    for fixture in fixtures {
        let expected_text =
            fs::read_to_string(&fixture.expected_json).expect("read expected fixture status");
        let expected: ExpectedStatus =
            serde_json::from_str(&expected_text).expect("parse expected fixture status");
        assert_eq!(expected.status, "pass", "{} expected status", fixture.name);

        let _input_text = fs::read_to_string(&fixture.input_json).expect("read fixture input");
        let image = ImageReader::open(&fixture.golden_png)
            .expect("open golden PNG")
            .decode()
            .expect("decode golden PNG")
            .into_rgba8();
        let result = compare_images(&image, &image, fixture.thresholds)
            .expect("self-compare dimensions must match");

        assert!(result.passed, "{} self-compare should pass", fixture.name);
        assert!(
            (result.metrics.ssim - 1.0).abs() <= f64::EPSILON,
            "{} self-compare SSIM should be 1.0, got {}",
            fixture.name,
            result.metrics.ssim
        );
        assert_eq!(result.metrics.l_inf, 0, "{} l_inf", fixture.name);
        assert_eq!(
            result.metrics.changed_pixels, 0,
            "{} changed_pixels",
            fixture.name
        );
        assert!(
            result.metrics.changed_pixel_fraction <= f64::EPSILON,
            "{} changed pixel fraction should be zero, got {}",
            fixture.name,
            result.metrics.changed_pixel_fraction
        );
    }
}

#[test]
fn ssim_surface_constants_match_default_thresholds() {
    let thresholds = Thresholds::default();

    assert_eq!(
        RENDERER_SSIM_PARITY_MCP_RESOURCE_URI,
        "wa://perf/renderer-slo/ssim_parity"
    );
    assert_eq!(
        RENDERER_SSIM_PARITY_STATUS,
        "ssim_oracle_corpus_wired_pending_retained_release_run"
    );
    assert_eq!(
        RENDERER_SSIM_PARITY_CURRENT_DEGRADATION,
        "retained-release-run-pending"
    );
    assert!((thresholds.min_ssim - 0.99).abs() <= f64::EPSILON);
    assert_eq!(
        (thresholds.min_ssim * 1_000_000.0).round() as u32,
        RENDERER_SSIM_PARITY_DEFAULT_MIN_SSIM_PPM
    );
    assert_eq!(thresholds.max_l_inf, RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF);
    assert_eq!(
        (thresholds.max_changed_pixel_fraction * 1_000_000.0).round() as u32,
        RENDERER_SSIM_PARITY_DEFAULT_MAX_CHANGED_PIXEL_FRACTION_PPM
    );
}

#[test]
fn metric_threshold_failure_preserves_topology_cross_check_boundary() {
    let expected = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 255]));
    let mut actual = expected.clone();
    for y in 0..8 {
        for x in 0..8 {
            actual.put_pixel(x, y, Rgba([255, 255, 255, 255]));
        }
    }

    let thresholds = Thresholds::default();
    let result =
        compare_images(&actual, &expected, thresholds).expect("synthetic dimensions must match");

    assert!(
        !result.passed,
        "large synthetic divergence must fail the SSIM parity floor"
    );
    assert_eq!(result.metrics.l_inf, 255);
    assert!(
        result.metrics.changed_pixel_fraction > thresholds.max_changed_pixel_fraction
            || result.metrics.ssim < thresholds.min_ssim,
        "failed comparison must preserve metric-threshold evidence for topology cross-check"
    );
}

#[test]
fn topology_cross_check_covers_terminal_conformance_expected_corpus() {
    let root = repo_root();
    let expected_dir = root.join("tests/fixtures/terminal-conformance/expected");
    let mut expected_ids: Vec<String> = fs::read_dir(&expected_dir)
        .expect("read terminal-conformance expected fixture directory")
        .map(|entry| entry.expect("read expected fixture entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .map(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .expect("fixture id must be utf-8")
                .to_string()
        })
        .collect();
    expected_ids.sort();

    assert!(
        expected_ids.len() >= 7,
        "terminal-conformance expected corpus should retain G39 topology coverage"
    );

    let topology_path = root.join("docs/attestations/tui/topology-parity.json");
    let topology: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&topology_path).expect("read topology parity attestation"),
    )
    .expect("parse topology parity attestation");
    assert_eq!(
        topology["scope"]["fixture_expected_dir"],
        "tests/fixtures/terminal-conformance/expected"
    );
    assert_eq!(
        topology["current_status"]["production_evidence_required"],
        true
    );

    let topology_ids = topology["corpus_contract"]["scenario_ids"]
        .as_array()
        .expect("topology attestation must list scenario ids");
    for fixture_id in expected_ids {
        assert!(
            topology_ids
                .iter()
                .any(|id| id.as_str() == Some(&fixture_id)),
            "topology attestation is missing terminal-conformance fixture {fixture_id}"
        );
    }
}

proptest! {
    #[test]
    fn adversarial_small_channel_delta_stays_inside_default_floor(
        x in 0u32..16,
        y in 0u32..16,
        channel in 0usize..4,
        delta in 0u8..=RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF,
    ) {
        let expected = RgbaImage::from_pixel(16, 16, Rgba([64, 96, 128, 255]));
        let mut actual = expected.clone();
        let pixel = actual.get_pixel_mut(x, y);
        pixel.0[channel] = pixel.0[channel].saturating_add(delta);

        let result = compare_images(&actual, &expected, Thresholds::default())
            .expect("synthetic dimensions must match");
        prop_assert!(
            result.passed,
            "single-pixel channel delta inside L-infinity floor must pass: {:?}",
            result.metrics
        );
        prop_assert!(result.metrics.l_inf <= RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF);
    }

    #[test]
    fn adversarial_large_patch_violates_default_floor(
        x0 in 0u32..8,
        y0 in 0u32..8,
        width in 8u32..=16,
        height in 8u32..=16,
    ) {
        let expected = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 255]));
        let mut actual = expected.clone();
        let x1 = (x0 + width).min(16);
        let y1 = (y0 + height).min(16);
        for y in y0..y1 {
            for x in x0..x1 {
                actual.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }

        let result = compare_images(&actual, &expected, Thresholds::default())
            .expect("synthetic dimensions must match");
        prop_assert!(
            !result.passed,
            "large adversarial patch must fail the SSIM parity floor: {:?}",
            result.metrics
        );
        prop_assert!(
            result.metrics.changed_pixel_fraction > Thresholds::default().max_changed_pixel_fraction
                || result.metrics.ssim < Thresholds::default().min_ssim
        );
    }
}
