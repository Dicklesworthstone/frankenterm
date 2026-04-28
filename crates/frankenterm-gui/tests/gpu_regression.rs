//! GPU golden-image regression harness scaffold.
//!
//! The default `_smoketest` fixture remains renderer-free so scaffold checks
//! can run without GPU readiness. Fixtures with `kind = "headless_terminal"`
//! call the feature-gated `frankenterm_gui::headless_render` entrypoint.

use frankenterm_gui::gpu_regression::{CompareResult, Thresholds, compare_images};
#[cfg(feature = "headless-render")]
use frankenterm_gui::headless_render::{
    HeadlessCursor, HeadlessFixtureInput, HeadlessRenderError, HeadlessSelection, HeadlessViewport,
    render_headless, smoketest_input,
};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ColorType, ImageEncoder, ImageReader, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

const HARNESS_VERSION: u32 = 1;
const DEFAULT_PERF_THRESHOLD_PER_FIXTURE_PCT: f64 = 20.0;
const DEFAULT_PERF_THRESHOLD_AGGREGATE_PCT: f64 = 10.0;

#[derive(Debug)]
struct Args {
    self_test: bool,
    headless_render_self_test: bool,
    update_goldens: bool,
    perf_self_test: bool,
    perf_report: Option<PathBuf>,
    perf_baseline: Option<PathBuf>,
    perf_threshold_per_fixture_pct: f64,
    perf_threshold_aggregate_pct: f64,
    update_perf_baseline: bool,
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
#[cfg_attr(not(feature = "headless-render"), allow(dead_code))]
struct InputSpec {
    kind: InputKind,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    cursor: Option<HeadlessCursorSpec>,
    #[serde(default)]
    selection: Option<HeadlessSelectionSpec>,
    #[serde(default = "default_true")]
    cursor_blink_disabled: bool,
    #[serde(default = "default_true")]
    ime_disabled: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum InputKind {
    StaticPngRoundtrip,
    HeadlessTerminal,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(not(feature = "headless-render"), allow(dead_code))]
struct HeadlessCursorSpec {
    row: u32,
    col: u32,
    #[serde(default)]
    shape: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(not(feature = "headless-render"), allow(dead_code))]
struct HeadlessSelectionSpec {
    start_row: u32,
    start_col: u32,
    end_row: u32,
    end_col: u32,
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

// `Thresholds`, `CompareMetrics`, and `CompareResult` live in
// `frankenterm_gui::gpu_regression` so the harness binary, the unit-test
// suite (ft-ombfl.11), and any future tooling share a single comparator
// implementation. See `crates/frankenterm-gui/src/gpu_regression.rs`.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfEntry {
    fixture: String,
    render_ms: u128,
    compare_ms: u128,
    elapsed_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    glyphs_cached: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    fonts_loaded: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    texture_format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfAggregate {
    total_render_ms: u128,
    p95_render_ms: u128,
    fixture_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfReport {
    harness_version: u32,
    generated_at_unix_secs: u64,
    runner: Option<String>,
    per_fixture: Vec<PerfEntry>,
    aggregate: PerfAggregate,
}

#[derive(Debug, Clone, Serialize)]
struct PerFixtureRegression {
    fixture: String,
    metric: String,
    baseline_ms: u128,
    current_ms: u128,
    delta_pct: f64,
    threshold_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
struct AggregateRegression {
    metric: String,
    baseline: u128,
    current: u128,
    delta_pct: f64,
    threshold_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
struct PerfComparison {
    per_fixture_regressions: Vec<PerFixtureRegression>,
    aggregate_regressions: Vec<AggregateRegression>,
    baseline_runner: Option<String>,
    baseline_generated_at_unix_secs: u64,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let exit_code = harness_exit_code(err.as_ref());
            emit_json(json!({
                "phase": "summary",
                "status": "error",
                "error": err.to_string(),
                "exit_code": exit_code,
            }));
            eprintln!("gpu_regression: {err}");
            ExitCode::from(exit_code)
        }
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    if args.self_test {
        return run_self_test().map_err(Into::into);
    }
    if args.headless_render_self_test {
        return run_headless_render_self_test().map_err(Into::into);
    }
    if args.perf_self_test {
        return run_perf_self_test().map_err(Into::into);
    }

    if args.update_goldens {
        require_update_goldens_confirmation()?;
    }
    if args.update_perf_baseline {
        require_update_perf_baseline_confirmation()?;
    }

    run_fixtures(&args)?;
    Ok(())
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut args = Args {
        self_test: false,
        headless_render_self_test: false,
        update_goldens: false,
        perf_self_test: false,
        perf_report: None,
        perf_baseline: None,
        perf_threshold_per_fixture_pct: DEFAULT_PERF_THRESHOLD_PER_FIXTURE_PCT,
        perf_threshold_aggregate_pct: DEFAULT_PERF_THRESHOLD_AGGREGATE_PCT,
        update_perf_baseline: false,
    };

    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--self-test" => args.self_test = true,
            "--headless-render-self-test" => args.headless_render_self_test = true,
            "--update-goldens" => args.update_goldens = true,
            "--perf-self-test" => args.perf_self_test = true,
            "--update-perf-baseline" => args.update_perf_baseline = true,
            "--nocapture" | "--ignored" | "--include-ignored" => {}
            other if other.starts_with("--test-threads") => {}
            other => {
                if let Some(value) = other.strip_prefix("--perf-report=") {
                    args.perf_report = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--perf-baseline=") {
                    args.perf_baseline = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--perf-threshold-per-fixture=") {
                    args.perf_threshold_per_fixture_pct = parse_pct_arg(other, value)?;
                } else if let Some(value) = other.strip_prefix("--perf-threshold-aggregate=") {
                    args.perf_threshold_aggregate_pct = parse_pct_arg(other, value)?;
                } else {
                    return Err(format!("unsupported gpu_regression argument: {other}").into());
                }
            }
        }
    }

    Ok(args)
}

fn parse_pct_arg(arg: &str, value: &str) -> Result<f64, Box<dyn std::error::Error>> {
    let parsed: f64 = value
        .parse()
        .map_err(|err| format!("{arg}: invalid percentage `{value}`: {err}"))?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(format!("{arg}: percentage must be a finite, non-negative number").into());
    }
    Ok(parsed)
}

#[cfg(feature = "headless-render")]
fn run_headless_render_self_test() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    emit_json(json!({
        "phase": "render-init",
        "mode": "headless-render-self-test",
        "status": "start",
        "harness_version": HARNESS_VERSION,
    }));
    let input = smoketest_input(64, 64, 96.0);
    let first = render_headless(&input)?;
    emit_json(json!({
        "phase": "render-frame",
        "iteration": 0,
        "ms": first.render_ms,
        "glyphs": first.glyphs_cached,
        "fonts_loaded": first.fonts_loaded,
        "texture_format": first.texture_format,
        "gpu": first.gpu,
    }));
    for iteration in 1..10 {
        let next = render_headless(&input)?;
        emit_json(json!({
            "phase": "render-frame",
            "iteration": iteration,
            "ms": next.render_ms,
            "glyphs": next.glyphs_cached,
            "fonts_loaded": next.fonts_loaded,
            "texture_format": next.texture_format,
            "gpu": next.gpu,
        }));
        assert_eq!(
            first.rgba, next.rgba,
            "headless render output changed at iteration {iteration}"
        );
    }
    emit_json(json!({
        "phase": "render-summary",
        "status": "pass",
        "iterations": 10,
        "elapsed_ms": started.elapsed().as_millis(),
    }));
    Ok(())
}

#[cfg(not(feature = "headless-render"))]
fn run_headless_render_self_test() -> Result<(), Box<dyn std::error::Error>> {
    Err("--headless-render-self-test requires --features headless-render".into())
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

fn run_fixtures(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
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
    let mut perf_entries: Vec<PerfEntry> = Vec::with_capacity(fixtures.len());

    for fixture in fixtures {
        emit_json(json!({
            "phase": "fixture",
            "name": fixture.name,
            "status": "start",
        }));
        let fixture_start = Instant::now();
        let render_start = Instant::now();
        let render_outcome = render_fixture(&fixture)?;
        let render_ms = render_start.elapsed().as_millis();
        let actual = render_outcome.image;

        if args.update_goldens {
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

        let elapsed_ms = fixture_start.elapsed().as_millis();
        emit_json(json!({
            "phase": "fixture",
            "name": fixture.name,
            "render_ms": render_ms,
            "compare_ms": compare_ms,
            "elapsed_ms": elapsed_ms,
            "ssim": comparison.metrics.ssim,
            "linf": comparison.metrics.l_inf,
            "changed_pixels": comparison.metrics.changed_pixels,
            "changed_pixel_fraction": comparison.metrics.changed_pixel_fraction,
            "status": status,
        }));

        perf_entries.push(PerfEntry {
            fixture: fixture.name.clone(),
            render_ms,
            compare_ms,
            elapsed_ms,
            glyphs_cached: render_outcome.glyphs_cached,
            fonts_loaded: render_outcome.fonts_loaded,
            texture_format: render_outcome.texture_format,
        });
    }

    emit_json(json!({
        "phase": "summary",
        "total": passed + failed,
        "passed": passed,
        "failed": failed,
    }));

    let perf_report = build_perf_report(perf_entries);
    let perf_report_path = perf_report_path(args.perf_report.as_deref());
    write_perf_report(&perf_report_path, &perf_report)?;

    let comparison = match args.perf_baseline.as_deref() {
        Some(path) => Some(compare_perf_to_baseline(
            path,
            &perf_report,
            args.perf_threshold_per_fixture_pct,
            args.perf_threshold_aggregate_pct,
        )?),
        None => None,
    };

    emit_perf_summary(&perf_report, &perf_report_path, comparison.as_ref());

    if args.update_perf_baseline {
        let baseline_path = args
            .perf_baseline
            .clone()
            .unwrap_or_else(default_perf_baseline_path);
        write_perf_report(&baseline_path, &perf_report)?;
        emit_json(json!({
            "phase": "perf-baseline",
            "status": "updated",
            "path": baseline_path,
        }));
    }

    if failed == 0 {
        // Perf regressions are warning-only in this iteration (per
        // ft-ombfl.10 risk note). They land in JSON-line output and
        // perf-report.json so reviewers see them without blocking
        // merge.
        Ok(())
    } else {
        Err(format!("{failed} GPU golden fixture(s) failed").into())
    }
}

fn discover_fixtures(root: &Path) -> Result<Vec<Fixture>, Box<dyn std::error::Error>> {
    let filter = fixture_filter();
    let mut fixtures = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if filter
            .as_ref()
            .is_some_and(|allowed| !allowed.iter().any(|fixture| fixture == &name))
        {
            continue;
        }
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
    if filter.is_some() && fixtures.is_empty() {
        return Err("GPU_HARNESS_FIXTURE_FILTER did not match any fixtures".into());
    }
    Ok(fixtures)
}

struct RenderOutcome {
    image: RgbaImage,
    glyphs_cached: Option<u64>,
    fonts_loaded: Option<u64>,
    texture_format: Option<String>,
}

fn render_fixture(fixture: &Fixture) -> Result<RenderOutcome, Box<dyn std::error::Error>> {
    match fixture.input.kind {
        InputKind::StaticPngRoundtrip => {
            let source = fixture
                .input
                .source
                .as_deref()
                .ok_or("static_png_roundtrip requires input.source")?;
            let image = load_png_rgba8(&fixture.dir.join(source))?;
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
            Ok(RenderOutcome {
                image,
                glyphs_cached: None,
                fonts_loaded: None,
                texture_format: None,
            })
        }
        InputKind::HeadlessTerminal => render_headless_fixture(fixture),
    }
}

#[cfg(feature = "headless-render")]
fn render_headless_fixture(fixture: &Fixture) -> Result<RenderOutcome, Box<dyn std::error::Error>> {
    let cursor = match fixture.input.cursor.as_ref() {
        Some(cursor) => Some(HeadlessCursor {
            row: cursor.row,
            col: cursor.col,
            shape: match cursor.shape.as_str() {
                "" | "block" => frankenterm_gui::headless_render::HeadlessCursorShape::Block,
                "underline" => frankenterm_gui::headless_render::HeadlessCursorShape::Underline,
                "beam" => frankenterm_gui::headless_render::HeadlessCursorShape::Beam,
                other => {
                    return Err(format!("unsupported headless cursor shape `{other}`").into());
                }
            },
        }),
        None => None,
    };
    let input = HeadlessFixtureInput {
        viewport: HeadlessViewport {
            width: fixture.meta.viewport.width,
            height: fixture.meta.viewport.height,
            dpi: fixture.meta.viewport.dpi,
        },
        lines: fixture.input.lines.clone(),
        cursor,
        selection: fixture
            .input
            .selection
            .as_ref()
            .map(|selection| HeadlessSelection {
                start_row: selection.start_row,
                start_col: selection.start_col,
                end_row: selection.end_row,
                end_col: selection.end_col,
            }),
        font_set_sha: Some(fixture.meta.font_set_sha.clone()),
        cursor_blink_disabled: fixture.input.cursor_blink_disabled,
        ime_disabled: fixture.input.ime_disabled,
    };
    let frame = render_headless(&input)?;
    let glyphs_cached = u64::try_from(frame.glyphs_cached).ok();
    let fonts_loaded = u64::try_from(frame.fonts_loaded).ok();
    let texture_format = Some(frame.texture_format.clone());
    emit_json(json!({
        "phase": "render-frame",
        "name": fixture.name,
        "ms": frame.render_ms,
        "glyphs": frame.glyphs_cached,
        "fonts_loaded": frame.fonts_loaded,
        "texture_format": frame.texture_format,
        "gpu": frame.gpu,
    }));
    let image = RgbaImage::from_raw(frame.width, frame.height, frame.rgba).ok_or_else(
        || -> Box<dyn std::error::Error> {
            format!(
                "headless renderer returned invalid RGBA frame for `{}`",
                fixture.name
            )
            .into()
        },
    )?;
    Ok(RenderOutcome {
        image,
        glyphs_cached,
        fonts_loaded,
        texture_format,
    })
}

#[cfg(not(feature = "headless-render"))]
fn render_headless_fixture(
    _fixture: &Fixture,
) -> Result<RenderOutcome, Box<dyn std::error::Error>> {
    Err("headless_terminal fixtures require --features headless-render".into())
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

fn default_true() -> bool {
    true
}

fn fixtures_root() -> PathBuf {
    env::var_os("GPU_HARNESS_FIXTURE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("tests")
                .join("golden")
                .join("gpu")
        })
}

fn fixture_filter() -> Option<Vec<String>> {
    let value = env::var("GPU_HARNESS_FIXTURE_FILTER").ok()?;
    let fixtures: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|fixture| !fixture.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if fixtures.is_empty() {
        None
    } else {
        Some(fixtures)
    }
}

fn default_perf_report_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("gpu-regression")
        .join("perf-report.json")
}

fn default_perf_baseline_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("golden")
        .join("gpu")
        .join("perf-baseline.json")
}

fn perf_report_path(override_path: Option<&Path>) -> PathBuf {
    override_path
        .map(PathBuf::from)
        .or_else(|| env::var_os("GPU_HARNESS_PERF_REPORT").map(PathBuf::from))
        .unwrap_or_else(default_perf_report_path)
}

fn unix_secs_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn build_perf_report(per_fixture: Vec<PerfEntry>) -> PerfReport {
    let total_render_ms: u128 = per_fixture.iter().map(|entry| entry.render_ms).sum();
    let p95_render_ms = compute_p95(per_fixture.iter().map(|entry| entry.render_ms));
    let fixture_count = per_fixture.len();
    PerfReport {
        harness_version: HARNESS_VERSION,
        generated_at_unix_secs: unix_secs_now(),
        runner: env::var("GITHUB_RUNNER_OS")
            .ok()
            .or_else(|| env::var("RUNNER_OS").ok()),
        per_fixture,
        aggregate: PerfAggregate {
            total_render_ms,
            p95_render_ms,
            fixture_count,
        },
    }
}

/// 95th percentile of a sequence of u128 millisecond samples using the
/// nearest-rank method. Empty input → 0.
fn compute_p95<I: IntoIterator<Item = u128>>(values: I) -> u128 {
    let mut samples: Vec<u128> = values.into_iter().collect();
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    // Nearest-rank: rank = ceil(0.95 * N), 1-indexed → samples[rank-1].
    let n = samples.len();
    let rank = ((0.95_f64 * n as f64).ceil() as usize).max(1);
    samples[rank - 1]
}

fn write_perf_report(path: &Path, report: &PerfReport) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut json = serde_json::to_string_pretty(report)?;
    json.push('\n');
    fs::write(path, json)?;
    Ok(())
}

fn compare_perf_to_baseline(
    baseline_path: &Path,
    current: &PerfReport,
    threshold_per_fixture_pct: f64,
    threshold_aggregate_pct: f64,
) -> Result<PerfComparison, Box<dyn std::error::Error>> {
    let baseline_bytes = fs::read(baseline_path).map_err(|err| {
        format!(
            "could not read perf baseline `{}`: {err}",
            baseline_path.display()
        )
    })?;
    let baseline: PerfReport = serde_json::from_slice(&baseline_bytes)?;
    Ok(diff_perf_reports(
        &baseline,
        current,
        threshold_per_fixture_pct,
        threshold_aggregate_pct,
    ))
}

fn diff_perf_reports(
    baseline: &PerfReport,
    current: &PerfReport,
    threshold_per_fixture_pct: f64,
    threshold_aggregate_pct: f64,
) -> PerfComparison {
    let mut per_fixture_regressions = Vec::new();
    for current_entry in &current.per_fixture {
        let Some(baseline_entry) = baseline
            .per_fixture
            .iter()
            .find(|entry| entry.fixture == current_entry.fixture)
        else {
            continue;
        };
        if let Some(delta_pct) = pct_increase(baseline_entry.render_ms, current_entry.render_ms) {
            if delta_pct > threshold_per_fixture_pct {
                per_fixture_regressions.push(PerFixtureRegression {
                    fixture: current_entry.fixture.clone(),
                    metric: "render_ms".to_string(),
                    baseline_ms: baseline_entry.render_ms,
                    current_ms: current_entry.render_ms,
                    delta_pct,
                    threshold_pct: threshold_per_fixture_pct,
                });
            }
        }
    }

    let mut aggregate_regressions = Vec::new();
    if let Some(delta_pct) = pct_increase(
        baseline.aggregate.total_render_ms,
        current.aggregate.total_render_ms,
    ) {
        if delta_pct > threshold_aggregate_pct {
            aggregate_regressions.push(AggregateRegression {
                metric: "total_render_ms".to_string(),
                baseline: baseline.aggregate.total_render_ms,
                current: current.aggregate.total_render_ms,
                delta_pct,
                threshold_pct: threshold_aggregate_pct,
            });
        }
    }
    if let Some(delta_pct) = pct_increase(
        baseline.aggregate.p95_render_ms,
        current.aggregate.p95_render_ms,
    ) {
        // P95 carries a slightly looser threshold floor (+50% of the
        // per-fixture threshold) since percentile estimates from small
        // fixture counts are inherently noisier than means.
        let p95_threshold = (threshold_per_fixture_pct * 1.5).max(threshold_aggregate_pct);
        if delta_pct > p95_threshold {
            aggregate_regressions.push(AggregateRegression {
                metric: "p95_render_ms".to_string(),
                baseline: baseline.aggregate.p95_render_ms,
                current: current.aggregate.p95_render_ms,
                delta_pct,
                threshold_pct: p95_threshold,
            });
        }
    }

    PerfComparison {
        per_fixture_regressions,
        aggregate_regressions,
        baseline_runner: baseline.runner.clone(),
        baseline_generated_at_unix_secs: baseline.generated_at_unix_secs,
    }
}

/// Percent increase from `baseline` to `current`. None when baseline is
/// 0 (regression detection on a zero baseline is meaningless — log a
/// neutral event instead). Decreases (current < baseline) yield None
/// because the regression detector only flags slow-downs.
fn pct_increase(baseline: u128, current: u128) -> Option<f64> {
    if baseline == 0 {
        return None;
    }
    if current <= baseline {
        return None;
    }
    let delta = current - baseline;
    Some((delta as f64) / (baseline as f64) * 100.0)
}

fn emit_perf_summary(report: &PerfReport, report_path: &Path, comparison: Option<&PerfComparison>) {
    let mut value = json!({
        "phase": "perf-summary",
        "report_path": report_path,
        "harness_version": report.harness_version,
        "generated_at_unix_secs": report.generated_at_unix_secs,
        "runner": report.runner,
        "fixture_count": report.aggregate.fixture_count,
        "total_render_ms": report.aggregate.total_render_ms,
        "p95_render_ms": report.aggregate.p95_render_ms,
        "per_fixture": report.per_fixture,
    });
    if let Some(cmp) = comparison {
        if let serde_json::Value::Object(ref mut map) = value {
            map.insert(
                "regressions_vs_baseline".into(),
                serde_json::to_value(&cmp.per_fixture_regressions).unwrap_or(json!([])),
            );
            map.insert(
                "aggregate_regressions".into(),
                serde_json::to_value(&cmp.aggregate_regressions).unwrap_or(json!([])),
            );
            map.insert(
                "baseline_runner".into(),
                serde_json::to_value(&cmp.baseline_runner).unwrap_or(json!(null)),
            );
            map.insert(
                "baseline_generated_at_unix_secs".into(),
                json!(cmp.baseline_generated_at_unix_secs),
            );
        }
    }
    emit_json(value);
}

fn require_update_perf_baseline_confirmation() -> Result<(), Box<dyn std::error::Error>> {
    if env::var("SET_PERF_BASELINE").ok().as_deref() == Some("1") {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(
            "--update-perf-baseline requires SET_PERF_BASELINE=1 in non-interactive runs".into(),
        );
    }
    eprint!("Regenerate GPU perf baseline? Type SET_PERF_BASELINE=1 to confirm: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if line.trim() == "SET_PERF_BASELINE=1" {
        Ok(())
    } else {
        Err("perf baseline update declined".into())
    }
}

fn run_perf_self_test() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    emit_json(json!({
        "phase": "perf-self-test",
        "status": "start",
        "harness_version": HARNESS_VERSION,
    }));

    fn entry(fixture: &str, render_ms: u128) -> PerfEntry {
        PerfEntry {
            fixture: fixture.to_string(),
            render_ms,
            compare_ms: 5,
            elapsed_ms: render_ms + 5,
            glyphs_cached: None,
            fonts_loaded: None,
            texture_format: None,
        }
    }
    fn report(per_fixture: Vec<PerfEntry>) -> PerfReport {
        let total_render_ms: u128 = per_fixture.iter().map(|e| e.render_ms).sum();
        let p95_render_ms = compute_p95(per_fixture.iter().map(|e| e.render_ms));
        let fixture_count = per_fixture.len();
        PerfReport {
            harness_version: HARNESS_VERSION,
            generated_at_unix_secs: 1_000_000,
            runner: Some("test-runner".into()),
            per_fixture,
            aggregate: PerfAggregate {
                total_render_ms,
                p95_render_ms,
                fixture_count,
            },
        }
    }

    // ── Scenario A: a real regression ────────────────────────────────
    // baseline render_ms: stable=50, also-stable=50, regressed=100 → total=200, p95=100
    // current  render_ms: stable=52, also-stable=52, regressed=350 → total=454, p95=350
    //   - regressed +250% (≫20% threshold) → per-fixture regression
    //   - total_render_ms +127% (≫10% threshold) → aggregate regression
    //   - p95_render_ms +250% (≫30% p95 threshold) → aggregate regression
    let baseline_a = report(vec![
        entry("stable", 50),
        entry("also-stable", 50),
        entry("regressed", 100),
    ]);
    let current_a = report(vec![
        entry("stable", 52),
        entry("also-stable", 52),
        entry("regressed", 350),
    ]);

    let cmp_a = diff_perf_reports(&baseline_a, &current_a, 20.0, 10.0);
    if cmp_a.per_fixture_regressions.len() != 1 {
        return Err(format!(
            "scenario A: expected exactly 1 per-fixture regression, got {}: {:?}",
            cmp_a.per_fixture_regressions.len(),
            cmp_a.per_fixture_regressions
        )
        .into());
    }
    let only = &cmp_a.per_fixture_regressions[0];
    if only.fixture != "regressed" {
        return Err(format!(
            "scenario A: expected `regressed` flagged, got `{}`",
            only.fixture
        )
        .into());
    }
    if only.delta_pct < 240.0 || only.delta_pct > 260.0 {
        return Err(format!(
            "scenario A: regressed delta_pct expected ~250%, got {:.2}",
            only.delta_pct
        )
        .into());
    }

    let mut got_p95 = false;
    let mut got_total = false;
    for reg in &cmp_a.aggregate_regressions {
        match reg.metric.as_str() {
            "p95_render_ms" => got_p95 = true,
            "total_render_ms" => got_total = true,
            _ => {}
        }
    }
    if !got_p95 {
        return Err(format!(
            "scenario A: expected p95_render_ms aggregate regression, got {:?}",
            cmp_a.aggregate_regressions
        )
        .into());
    }
    if !got_total {
        return Err(format!(
            "scenario A: expected total_render_ms aggregate regression, got {:?}",
            cmp_a.aggregate_regressions
        )
        .into());
    }

    // ── Scenario B: pure improvement, nothing flagged ────────────────
    // Every fixture got faster — no slowdowns at any granularity.
    let baseline_b = report(vec![
        entry("stable", 100),
        entry("also-stable", 100),
        entry("got-faster", 200),
    ]);
    let current_b = report(vec![
        entry("stable", 90),
        entry("also-stable", 90),
        entry("got-faster", 100),
    ]);
    let cmp_b = diff_perf_reports(&baseline_b, &current_b, 20.0, 10.0);
    if !cmp_b.per_fixture_regressions.is_empty() {
        return Err(format!(
            "scenario B: pure improvement must yield 0 per-fixture regressions, got {:?}",
            cmp_b.per_fixture_regressions
        )
        .into());
    }
    if !cmp_b.aggregate_regressions.is_empty() {
        return Err(format!(
            "scenario B: pure improvement must yield 0 aggregate regressions, got {:?}",
            cmp_b.aggregate_regressions
        )
        .into());
    }

    // ── Scenario C: small wobble within thresholds ──────────────────
    // Each fixture +5% → below per-fixture (20%) and aggregate (10%) gates.
    let baseline_c = report(vec![entry("a", 100), entry("b", 100), entry("c", 100)]);
    let current_c = report(vec![entry("a", 105), entry("b", 105), entry("c", 105)]);
    let cmp_c = diff_perf_reports(&baseline_c, &current_c, 20.0, 10.0);
    if !cmp_c.per_fixture_regressions.is_empty() {
        return Err(format!(
            "scenario C: 5% wobble must not exceed 20% per-fixture gate, got {:?}",
            cmp_c.per_fixture_regressions
        )
        .into());
    }
    if !cmp_c.aggregate_regressions.is_empty() {
        return Err(format!(
            "scenario C: 5% wobble must not exceed 10% aggregate gate, got {:?}",
            cmp_c.aggregate_regressions
        )
        .into());
    }

    // pct_increase invariants
    if pct_increase(0, 100).is_some() {
        return Err("pct_increase from zero baseline must be None".into());
    }
    if pct_increase(100, 50).is_some() {
        return Err("pct_increase for a slowdown-only metric must be None on improvement".into());
    }
    let exact = pct_increase(100, 120).ok_or("pct_increase must yield Some on +20%")?;
    if (exact - 20.0).abs() > 0.01 {
        return Err(format!("pct_increase(100,120) expected 20.0, got {exact}").into());
    }

    if compute_p95(std::iter::empty()) != 0 {
        return Err("compute_p95 of empty must be 0".into());
    }
    let single = compute_p95(std::iter::once(42_u128));
    if single != 42 {
        return Err(format!("compute_p95 of [42] expected 42, got {single}").into());
    }
    let many = compute_p95((1u128..=100).collect::<Vec<_>>());
    if many != 95 {
        return Err(format!("compute_p95 of 1..=100 expected 95, got {many}").into());
    }

    emit_json(json!({
        "phase": "perf-self-test",
        "status": "pass",
        "elapsed_ms": started.elapsed().as_millis(),
    }));
    Ok(())
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

fn harness_exit_code(_err: &(dyn std::error::Error + 'static)) -> u8 {
    #[cfg(feature = "headless-render")]
    {
        if _err
            .downcast_ref::<HeadlessRenderError>()
            .is_some_and(HeadlessRenderError::is_gpu_init_failed)
        {
            return 2;
        }
    }
    1
}
