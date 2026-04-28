//! GPU golden-image regression harness scaffold.
//!
//! Renderer integration lands in a later ft-ombfl bead. This scaffold
//! deliberately exercises fixture loading, PNG decode/encode, comparator
//! metrics, diff artifact generation, and JSON-line logging without requiring a
//! live GPU.

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ColorType, GenericImageView, ImageEncoder, ImageReader, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cmp;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

const HARNESS_VERSION: u32 = 1;

#[derive(Debug)]
struct Args {
    self_test: bool,
    update_goldens: bool,
}

#[derive(Debug)]
struct Fixture {
    name: String,
    dir: PathBuf,
    input: InputSpec,
    meta: FixtureMeta,
    expected: ExpectedResult,
}

#[derive(Debug, Deserialize)]
struct InputSpec {
    kind: InputKind,
    source: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum InputKind {
    StaticPngRoundtrip,
}

#[derive(Debug, Deserialize)]
struct ExpectedResult {
    status: ExpectedStatus,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ExpectedStatus {
    Pass,
}

#[derive(Debug, Deserialize)]
struct FixtureMeta {
    fixture: String,
    viewport: Viewport,
    texture_format: String,
    font_set_sha: String,
    harness_version: u32,
    generated_at_runner: String,
    #[serde(default)]
    thresholds: Thresholds,
}

#[derive(Debug, Deserialize)]
struct Viewport {
    width: u32,
    height: u32,
    dpi: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
struct Thresholds {
    min_ssim: f64,
    max_l_inf: u8,
    max_changed_pixel_fraction: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_ssim: 0.99,
            max_l_inf: 8,
            max_changed_pixel_fraction: 0.001,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct CompareMetrics {
    ssim: f64,
    l_inf: u8,
    changed_pixels: u64,
    total_pixels: u64,
    changed_pixel_fraction: f64,
    thresholds: Thresholds,
}

#[derive(Debug)]
struct CompareResult {
    passed: bool,
    metrics: CompareMetrics,
    diff: RgbaImage,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            emit_json(json!({
                "phase": "summary",
                "status": "error",
                "error": err.to_string(),
            }));
            eprintln!("gpu_regression: {err}");
            ExitCode::from(1)
        }
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    if args.self_test {
        return run_self_test().map_err(Into::into);
    }

    if args.update_goldens {
        require_update_goldens_confirmation()?;
    }

    run_fixtures(args.update_goldens)?;
    Ok(())
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut args = Args {
        self_test: false,
        update_goldens: false,
    };

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--self-test" => args.self_test = true,
            "--update-goldens" => args.update_goldens = true,
            "--nocapture" | "--ignored" | "--include-ignored" => {}
            arg if arg.starts_with("--test-threads") => {}
            other => return Err(format!("unsupported gpu_regression argument: {other}").into()),
        }
    }

    Ok(args)
}

fn run_self_test() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    emit_json(json!({
        "phase": "self-test",
        "status": "start",
        "harness_version": HARNESS_VERSION,
    }));

    let base = synthetic_smoketest_image();
    let identical = compare_images(&base, &base, Thresholds::default())?;
    assert!(
        identical.passed,
        "identical image comparison must pass: {:?}",
        identical.metrics
    );

    let mut near = base.clone();
    near.get_pixel_mut(0, 0).0[0] = near.get_pixel(0, 0).0[0].saturating_add(4);
    let near_result = compare_images(&near, &base, Thresholds::default())?;
    assert!(
        near_result.passed,
        "near-identical image comparison must pass within threshold: {:?}",
        near_result.metrics
    );

    let mut changed = base.clone();
    for y in 0..8 {
        for x in 0..8 {
            changed.put_pixel(x, y, Rgba([0, 255, 0, 255]));
        }
    }
    let changed_result = compare_images(&changed, &base, Thresholds::default())?;
    assert!(
        !changed_result.passed,
        "visibly changed image comparison must fail: {:?}",
        changed_result.metrics
    );
    assert_eq!(changed_result.metrics.total_pixels, 64 * 64);
    assert!(changed_result.metrics.changed_pixels > 0);

    emit_json(json!({
        "phase": "self-test",
        "status": "pass",
        "elapsed_ms": started.elapsed().as_millis(),
    }));
    Ok(())
}

fn run_fixtures(update_goldens: bool) -> Result<(), Box<dyn std::error::Error>> {
    let root = fixtures_root();
    let artifact_root = artifact_root();
    let fixtures = discover_fixtures(&root)?;

    emit_json(json!({
        "phase": "discover",
        "root": root,
        "count": fixtures.len(),
    }));

    let mut passed = 0usize;
    let mut failed = 0usize;

    for fixture in fixtures {
        emit_json(json!({
            "phase": "fixture",
            "name": fixture.name,
            "status": "start",
        }));
        let fixture_start = Instant::now();
        let render_start = Instant::now();
        let actual = render_fixture(&fixture)?;
        let render_ms = render_start.elapsed().as_millis();

        if update_goldens {
            write_png_deterministic(&fixture.dir.join("golden.png"), &actual)?;
        }

        let golden = load_png_rgba8(&fixture.dir.join("golden.png"))?;
        let compare_start = Instant::now();
        let comparison = compare_images(&actual, &golden, fixture.meta.thresholds)?;
        let compare_ms = compare_start.elapsed().as_millis();
        let status = if comparison.passed { "pass" } else { "fail" };

        if comparison.passed && fixture.expected.status == ExpectedStatus::Pass {
            passed += 1;
        } else {
            failed += 1;
            write_failure_artifacts(&artifact_root, &fixture, &actual, &comparison)?;
        }

        emit_json(json!({
            "phase": "fixture",
            "name": fixture.name,
            "render_ms": render_ms,
            "compare_ms": compare_ms,
            "elapsed_ms": fixture_start.elapsed().as_millis(),
            "ssim": comparison.metrics.ssim,
            "linf": comparison.metrics.l_inf,
            "changed_pixels": comparison.metrics.changed_pixels,
            "changed_pixel_fraction": comparison.metrics.changed_pixel_fraction,
            "status": status,
        }));
    }

    emit_json(json!({
        "phase": "summary",
        "total": passed + failed,
        "passed": passed,
        "failed": failed,
    }));

    if failed == 0 {
        Ok(())
    } else {
        Err(format!("{failed} GPU golden fixture(s) failed").into())
    }
}

fn discover_fixtures(root: &Path) -> Result<Vec<Fixture>, Box<dyn std::error::Error>> {
    let mut fixtures = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let input = read_json(&path.join("input.json"))?;
        let meta: FixtureMeta = read_json(&path.join("meta.json"))?;
        let expected = read_json(&path.join("expected.json"))?;
        if meta.fixture != name {
            return Err(format!(
                "fixture `{name}` has mismatched meta.fixture `{}`",
                meta.fixture
            )
            .into());
        }
        if meta.harness_version != HARNESS_VERSION {
            return Err(format!(
                "fixture `{name}` harness_version={} but harness is {}",
                meta.harness_version, HARNESS_VERSION
            )
            .into());
        }
        fixtures.push(Fixture {
            name,
            dir: path,
            input,
            meta,
            expected,
        });
    }
    fixtures.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(fixtures)
}

fn render_fixture(fixture: &Fixture) -> Result<RgbaImage, Box<dyn std::error::Error>> {
    match fixture.input.kind {
        InputKind::StaticPngRoundtrip => {
            let image = load_png_rgba8(&fixture.dir.join(&fixture.input.source))?;
            let (width, height) = image.dimensions();
            if width != fixture.meta.viewport.width || height != fixture.meta.viewport.height {
                return Err(format!(
                    "fixture `{}` rendered {}x{} but meta viewport is {}x{}",
                    fixture.name,
                    width,
                    height,
                    fixture.meta.viewport.width,
                    fixture.meta.viewport.height
                )
                .into());
            }
            let _ = (
                &fixture.meta.texture_format,
                &fixture.meta.font_set_sha,
                &fixture.meta.generated_at_runner,
            );
            let _ = fixture.meta.viewport.dpi;
            Ok(image)
        }
    }
}

fn compare_images(
    actual: &RgbaImage,
    expected: &RgbaImage,
    thresholds: Thresholds,
) -> Result<CompareResult, Box<dyn std::error::Error>> {
    let (actual_width, actual_height) = actual.dimensions();
    let (expected_width, expected_height) = expected.dimensions();
    if (actual_width, actual_height) != (expected_width, expected_height) {
        return Err(format!(
            "image dimensions differ: actual={}x{}, expected={}x{}",
            actual_width, actual_height, expected_width, expected_height
        )
        .into());
    }

    let total_pixels = u64::from(actual_width) * u64::from(actual_height);
    let mut changed_pixels = 0u64;
    let mut l_inf = 0u8;
    let mut diff = RgbaImage::new(actual_width, actual_height);

    for y in 0..actual_height {
        for x in 0..actual_width {
            let a = actual.get_pixel(x, y).0;
            let e = expected.get_pixel(x, y).0;
            let pixel_delta = a
                .iter()
                .zip(e.iter())
                .map(|(left, right)| left.abs_diff(*right))
                .max()
                .unwrap_or(0);
            l_inf = cmp::max(l_inf, pixel_delta);
            if pixel_delta > thresholds.max_l_inf {
                changed_pixels += 1;
                diff.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            } else {
                let shade = ((u16::from(a[0]) + u16::from(a[1]) + u16::from(a[2])) / 3) as u8;
                diff.put_pixel(x, y, Rgba([shade, shade, shade, 96]));
            }
        }
    }

    let changed_pixel_fraction = if total_pixels == 0 {
        0.0
    } else {
        changed_pixels as f64 / total_pixels as f64
    };
    let ssim = ssim_luma(actual, expected);
    let passed = ssim >= thresholds.min_ssim
        && l_inf <= thresholds.max_l_inf
        && changed_pixel_fraction <= thresholds.max_changed_pixel_fraction;

    Ok(CompareResult {
        passed,
        metrics: CompareMetrics {
            ssim,
            l_inf,
            changed_pixels,
            total_pixels,
            changed_pixel_fraction,
            thresholds,
        },
        diff,
    })
}

fn ssim_luma(actual: &RgbaImage, expected: &RgbaImage) -> f64 {
    let n = f64::from(actual.width()) * f64::from(actual.height());
    if n == 0.0 {
        return 1.0;
    }

    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    for (actual, expected) in actual.pixels().zip(expected.pixels()) {
        sum_x += luma(actual);
        sum_y += luma(expected);
    }
    let mean_x = sum_x / n;
    let mean_y = sum_y / n;

    let mut var_x = 0.0;
    let mut var_y = 0.0;
    let mut cov_xy = 0.0;
    for (actual, expected) in actual.pixels().zip(expected.pixels()) {
        let dx = luma(actual) - mean_x;
        let dy = luma(expected) - mean_y;
        var_x += dx * dx;
        var_y += dy * dy;
        cov_xy += dx * dy;
    }
    let denom = (n - 1.0).max(1.0);
    var_x /= denom;
    var_y /= denom;
    cov_xy /= denom;

    let c1 = (0.01_f64 * 255.0).powi(2);
    let c2 = (0.03_f64 * 255.0).powi(2);
    ((2.0 * mean_x * mean_y + c1) * (2.0 * cov_xy + c2))
        / ((mean_x.powi(2) + mean_y.powi(2) + c1) * (var_x + var_y + c2))
}

fn luma(pixel: &Rgba<u8>) -> f64 {
    let [r, g, b, _a] = pixel.0;
    0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b)
}

fn write_failure_artifacts(
    artifact_root: &Path,
    fixture: &Fixture,
    actual: &RgbaImage,
    comparison: &CompareResult,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(artifact_root)?;
    write_png_deterministic(
        &artifact_root.join(format!("{}.actual.png", fixture.name)),
        actual,
    )?;
    write_png_deterministic(
        &artifact_root.join(format!("{}.diff.png", fixture.name)),
        &comparison.diff,
    )?;
    let report = json!({
        "fixture": fixture.name,
        "status": "fail",
        "metrics": comparison.metrics,
    });
    fs::write(
        artifact_root.join(format!("{}.report.json", fixture.name)),
        format!("{}\n", serde_json::to_string_pretty(&report)?),
    )?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn load_png_rgba8(path: &Path) -> Result<RgbaImage, Box<dyn std::error::Error>> {
    Ok(ImageReader::open(path)?.decode()?.to_rgba8())
}

fn write_png_deterministic(
    path: &Path,
    image: &RgbaImage,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(path)?;
    let encoder = PngEncoder::new_with_quality(file, CompressionType::Fast, FilterType::Paeth);
    encoder.write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ColorType::Rgba8.into(),
    )?;
    Ok(())
}

fn synthetic_smoketest_image() -> RgbaImage {
    let mut image = RgbaImage::new(64, 64);
    for y in 0..64 {
        for x in 0..64 {
            let pixel = if y < 32 && x < 32 {
                if ((x / 8) + (y / 8)) % 2 == 0 {
                    Rgba([24, 24, 24, 255])
                } else {
                    Rgba([232, 232, 232, 255])
                }
            } else if y < 32 {
                let local_x = x - 32;
                if y >= local_x / 2 && y <= 31 - (local_x / 2) {
                    Rgba([224, 40, 40, 255])
                } else {
                    Rgba([18, 20, 28, 255])
                }
            } else {
                let band = ((y - 32) / 3).min(9) as u8;
                let r = 20u8.saturating_add(band.saturating_mul(18));
                let g = 80u8.saturating_add((x as u8).saturating_mul(2));
                let b = 180u8.saturating_sub(band.saturating_mul(12));
                Rgba([r, g, b, 255])
            };
            image.put_pixel(x, y, pixel);
        }
    }
    image
}

fn require_update_goldens_confirmation() -> Result<(), Box<dyn std::error::Error>> {
    if env::var("SET_GOLDEN").ok().as_deref() == Some("1") {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err("--update-goldens requires SET_GOLDEN=1 in non-interactive runs".into());
    }
    eprint!("Regenerate GPU goldens? Type SET_GOLDEN=1 to confirm: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if line.trim() == "SET_GOLDEN=1" {
        Ok(())
    } else {
        Err("golden update declined".into())
    }
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("golden")
        .join("gpu")
}

fn artifact_root() -> PathBuf {
    env::var_os("GPU_HARNESS_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
                .join("gpu-regression")
        })
}

fn emit_json(value: serde_json::Value) {
    eprintln!("{value}");
}
