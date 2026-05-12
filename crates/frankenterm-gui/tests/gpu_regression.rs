//! GPU golden-image regression harness scaffold.
//!
//! The default `_smoketest` fixture remains renderer-free so scaffold checks
//! can run without GPU readiness. Fixtures with `kind = "headless_terminal"`
//! call the feature-gated `frankenterm_gui::headless_render` entrypoint.

use frankenterm_core::gpu_regression_fuzz_report::FuzzCliFlags;
#[cfg(feature = "headless-render")]
use frankenterm_core::gpu_regression_fuzz_report::{
    RunId, RunLayout, RunMeta, ViolationKind, ViolationRecord, render_violations_jsonl,
};
use frankenterm_gui::gpu_regression::{CompareResult, Thresholds, compare_images};
#[cfg(feature = "headless-render")]
use frankenterm_gui::gpu_regression_fuzz::{FuzzConfig, FuzzInputEvent, FuzzStream};
#[cfg(feature = "headless-render")]
use frankenterm_gui::headless_render::{
    HeadlessCursor, HeadlessFixtureInput, HeadlessFrame, HeadlessMonitor, HeadlessRenderError,
    HeadlessSelection, HeadlessViewport, render_headless, smoketest_input,
};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ColorType, ImageEncoder, ImageReader, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(feature = "headless-render")]
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
#[cfg(feature = "headless-render")]
use std::time::Duration;
use std::time::Instant;

const HARNESS_VERSION: u32 = 1;
const DEFAULT_PERF_THRESHOLD_PER_FIXTURE_PCT: f64 = 20.0;
const DEFAULT_PERF_THRESHOLD_AGGREGATE_PCT: f64 = 10.0;
#[cfg(feature = "headless-render")]
const FUZZ_DEFAULT_DURATION_SECS: u32 = 60;
#[cfg(feature = "headless-render")]
const FUZZ_EVENTS_PER_SECOND_BUDGET: u64 = 512;
#[cfg(feature = "headless-render")]
const FUZZ_FRAME_INTERVAL_EVENTS: u64 = 64;
#[cfg(feature = "headless-render")]
const FUZZ_STALE_FRAME_EVENT_DISTANCE: u32 = 200;
#[cfg(feature = "headless-render")]
const FUZZ_MAX_STALE_HASHES: usize = 512;

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
    comparison_report: Option<PathBuf>,
    comparison_frankenterm_frames: Option<PathBuf>,
    comparison_wezterm_frames: Option<PathBuf>,
    fuzz: FuzzCliFlags,
    fixture_filters: Vec<String>,
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
    generated_lines: Option<GeneratedLinesSpec>,
    #[serde(default)]
    resize_sequence: Vec<ResizeFrameSpec>,
    #[serde(default)]
    monitors: Vec<HeadlessMonitorSpec>,
    #[serde(default)]
    cursor: Option<HeadlessCursorSpec>,
    #[serde(default)]
    selection: Option<HeadlessSelectionSpec>,
    #[serde(default = "default_true")]
    cursor_blink_disabled: bool,
    #[serde(default = "default_true")]
    ime_disabled: bool,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(not(feature = "headless-render"), allow(dead_code))]
struct GeneratedLinesSpec {
    count: u32,
    template: String,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(not(feature = "headless-render"), allow(dead_code))]
struct ResizeFrameSpec {
    second: u32,
    width: u32,
    height: u32,
    dpi: f64,
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
#[cfg_attr(not(feature = "headless-render"), allow(dead_code))]
struct HeadlessMonitorSpec {
    id: String,
    x: u32,
    width: u32,
    dpi: f64,
    color_profile: String,
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

#[derive(Debug, Serialize)]
struct WeztermRenderComparisonReport {
    schema_version: &'static str,
    comparisons: Vec<WeztermRenderComparisonRow>,
}

#[derive(Debug, Serialize)]
struct WeztermRenderComparisonRow {
    input_id: String,
    frame_id: String,
    status: &'static str,
    metrics: serde_json::Value,
    thresholds: Thresholds,
    frankenterm_png: String,
    wezterm_png: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
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
        return run_self_test();
    }
    if args.headless_render_self_test {
        return run_headless_render_self_test();
    }
    if args.perf_self_test {
        return run_perf_self_test();
    }
    if args.fuzz.fuzz_mode_active() {
        return run_fuzz(&args.fuzz);
    }
    if args.comparison_report.is_some()
        || args.comparison_frankenterm_frames.is_some()
        || args.comparison_wezterm_frames.is_some()
    {
        return write_wezterm_comparison_report(&args);
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
        comparison_report: None,
        comparison_frankenterm_frames: None,
        comparison_wezterm_frames: None,
        fuzz: FuzzCliFlags::default(),
        fixture_filters: Vec::new(),
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
                } else if let Some(value) = other.strip_prefix("--comparison-report=") {
                    args.comparison_report = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--comparison-frankenterm-frames=") {
                    args.comparison_frankenterm_frames = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--comparison-wezterm-frames=") {
                    args.comparison_wezterm_frames = Some(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--fuzz-seed=") {
                    args.fuzz.seed = Some(parse_u64_arg(other, value)?);
                } else if let Some(value) = other.strip_prefix("--fuzz-duration=") {
                    args.fuzz.duration_secs = Some(parse_positive_u32_arg(other, value)?);
                } else if let Some(value) = other.strip_prefix("--fuzz-start-at=") {
                    args.fuzz.start_at_event_idx = Some(parse_u32_arg(other, value)?);
                } else if let Some(value) = other.strip_prefix("--fuzz-cols=") {
                    args.fuzz.cols = Some(parse_positive_u16_arg(other, value)?);
                } else if let Some(value) = other.strip_prefix("--fuzz-rows=") {
                    args.fuzz.rows = Some(parse_positive_u16_arg(other, value)?);
                } else if let Some(value) = other.strip_prefix("--runs-dir=") {
                    args.fuzz.runs_dir = Some(value.to_string());
                } else if other.starts_with('-') {
                    return Err(format!("unsupported gpu_regression argument: {other}").into());
                } else {
                    args.fixture_filters.push(other.to_string());
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

fn parse_u64_arg(arg: &str, value: &str) -> Result<u64, Box<dyn std::error::Error>> {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16)
            .map_err(|err| format!("{arg}: invalid hex seed `{value}`: {err}").into());
    }
    value.parse::<u64>().or_else(|decimal_err| {
        u64::from_str_radix(value, 16).map_err(|hex_err| {
            format!("{arg}: invalid seed `{value}` as decimal ({decimal_err}) or hex ({hex_err})")
                .into()
        })
    })
}

fn parse_u32_arg(arg: &str, value: &str) -> Result<u32, Box<dyn std::error::Error>> {
    value
        .parse()
        .map_err(|err| format!("{arg}: invalid u32 `{value}`: {err}").into())
}

fn parse_positive_u32_arg(arg: &str, value: &str) -> Result<u32, Box<dyn std::error::Error>> {
    let parsed = parse_u32_arg(arg, value)?;
    if parsed == 0 {
        return Err(format!("{arg}: value must be greater than zero").into());
    }
    Ok(parsed)
}

fn parse_positive_u16_arg(arg: &str, value: &str) -> Result<u16, Box<dyn std::error::Error>> {
    let parsed: u16 = value
        .parse()
        .map_err(|err| format!("{arg}: invalid u16 `{value}`: {err}"))?;
    if parsed == 0 {
        return Err(format!("{arg}: value must be greater than zero").into());
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

#[cfg(feature = "headless-render")]
fn run_fuzz(flags: &FuzzCliFlags) -> Result<(), Box<dyn std::error::Error>> {
    let seed = flags.seed.unwrap_or(0);
    let duration_secs = flags.duration_secs.unwrap_or(FUZZ_DEFAULT_DURATION_SECS);
    let start_at = flags.start_at_event_idx.unwrap_or(0);
    let started_at_ms = unix_millis_now();
    let host = fuzz_host();
    let run_id = RunId::from_parts(seed, started_at_ms, &host);
    let layout = RunLayout::new(run_id.clone());
    let runs_dir = flags
        .runs_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(default_fuzz_runs_dir);
    let run_root = runs_dir.join(&run_id.0);
    fs::create_dir_all(&run_root)?;

    let mut meta = RunMeta {
        run_id: run_id.clone(),
        seed,
        started_at_ms,
        finished_at_ms: None,
        host,
        harness_version: HARNESS_VERSION.to_string(),
        events_processed: 0,
        violations_total: 0,
        critical_count: 0,
    };
    write_run_meta(&run_root, &meta)?;

    let max_cols = flags.cols.unwrap_or(160).max(8);
    let max_rows = flags.rows.unwrap_or(48).max(8);
    let start_cols = flags.cols.unwrap_or(80).min(max_cols).max(8);
    let start_rows = flags.rows.unwrap_or(24).min(max_rows).max(8);
    let event_budget = u64::from(start_at)
        .saturating_add(u64::from(duration_secs).saturating_mul(FUZZ_EVENTS_PER_SECOND_BUDGET))
        .saturating_add(1);
    let config = FuzzConfig {
        max_cols,
        max_rows,
        min_axis: max_cols.min(max_rows).clamp(1, 8),
        event_budget,
        ..FuzzConfig::default()
    };
    let mut stream = FuzzStream::new(seed, config);
    let mut state = FuzzHarnessState::new(start_cols, start_rows);
    let started = Instant::now();
    let duration = Duration::from_secs(u64::from(duration_secs));
    let mut recent_events: VecDeque<String> = VecDeque::new();
    let mut stale_hashes: VecDeque<(u32, u64)> = VecDeque::new();
    let mut seen_nonblank = false;
    let mut violations = Vec::new();
    let mut frame_index = 0u32;
    let mut last_render_event: Option<u64> = None;

    emit_json(json!({
        "phase": "fuzz-start",
        "status": "start",
        "seed": seed,
        "run_id": run_id.0,
        "duration_secs": duration_secs,
        "start_at_event_idx": start_at,
        "runs_dir": runs_dir,
        "layout_root": layout.root_dir(),
    }));

    while let Some(event) = stream.next() {
        let event_index = stream.emitted().saturating_sub(1);
        let event_log = state.apply_event(event_index, &event);
        push_recent_event(&mut recent_events, event_log);
        meta.events_processed = stream.emitted();

        if event_index < u64::from(start_at) {
            continue;
        }

        let elapsed = started.elapsed();
        let stop_after_event = elapsed >= duration && event_index > u64::from(start_at);
        if event_index == u64::from(start_at)
            || event_index % FUZZ_FRAME_INTERVAL_EVENTS == 0
            || stop_after_event
        {
            let mut context = FuzzRunContext {
                run_root: &run_root,
                stale_hashes: &mut stale_hashes,
                seen_nonblank: &mut seen_nonblank,
                violations: &mut violations,
                seed,
                duration_secs,
                runs_dir: &runs_dir,
            };
            render_fuzz_checkpoint(
                &state,
                event_index,
                frame_index,
                &recent_events,
                &mut context,
            )?;
            frame_index = frame_index.saturating_add(1);
            last_render_event = Some(event_index);
        }

        if stop_after_event {
            break;
        }
    }

    if meta.events_processed > u64::from(start_at) {
        let final_event = meta.events_processed.saturating_sub(1);
        if last_render_event != Some(final_event) {
            let mut context = FuzzRunContext {
                run_root: &run_root,
                stale_hashes: &mut stale_hashes,
                seen_nonblank: &mut seen_nonblank,
                violations: &mut violations,
                seed,
                duration_secs,
                runs_dir: &runs_dir,
            };
            render_fuzz_checkpoint(
                &state,
                final_event,
                frame_index,
                &recent_events,
                &mut context,
            )?;
        }
    }

    meta.finished_at_ms = Some(unix_millis_now());
    meta.violations_total = u32::try_from(violations.len()).unwrap_or(u32::MAX);
    meta.critical_count = u32::try_from(
        violations
            .iter()
            .filter(|record| record.is_critical())
            .count(),
    )
    .unwrap_or(u32::MAX);
    write_run_meta(&run_root, &meta)?;
    fs::write(
        run_root.join("violations.jsonl"),
        render_violations_jsonl(&violations),
    )?;

    emit_json(json!({
        "phase": "fuzz-summary",
        "status": if meta.critical_count == 0 { "pass" } else { "fail" },
        "run_id": run_id.0,
        "events_processed": meta.events_processed,
        "violations_total": meta.violations_total,
        "critical_count": meta.critical_count,
        "run_root": run_root,
        "elapsed_ms": started.elapsed().as_millis(),
    }));

    if meta.critical_count == 0 {
        Ok(())
    } else {
        Err(format!(
            "GPU fuzz run {} recorded {} critical violation(s)",
            run_id.0, meta.critical_count
        )
        .into())
    }
}

#[cfg(not(feature = "headless-render"))]
fn run_fuzz(_flags: &FuzzCliFlags) -> Result<(), Box<dyn std::error::Error>> {
    Err("--fuzz-* mode requires --features headless-render".into())
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
    let fixtures = discover_fixtures(&root, &args.fixture_filters)?;

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

fn write_wezterm_comparison_report(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let output = args
        .comparison_report
        .as_deref()
        .ok_or("--comparison-report is required for WezTerm comparison mode")?;
    let frankenterm_root = args
        .comparison_frankenterm_frames
        .as_deref()
        .ok_or("--comparison-frankenterm-frames is required for WezTerm comparison mode")?;
    let wezterm_root = args
        .comparison_wezterm_frames
        .as_deref()
        .ok_or("--comparison-wezterm-frames is required for WezTerm comparison mode")?;

    let frame_paths = discover_png_frames(frankenterm_root)?;
    if frame_paths.is_empty() {
        return Err(format!(
            "no FrankenTerm PNG frames found under {}",
            frankenterm_root.display()
        )
        .into());
    }

    let mut rows = Vec::with_capacity(frame_paths.len());
    let thresholds = Thresholds::default();
    for relative in frame_paths {
        let frankenterm_png = frankenterm_root.join(&relative);
        let wezterm_png = wezterm_root.join(&relative);
        let (input_id, frame_id) = comparison_ids(&relative);
        if !wezterm_png.is_file() {
            rows.push(WeztermRenderComparisonRow {
                input_id,
                frame_id,
                status: "diverged",
                metrics: failed_metrics(thresholds),
                thresholds,
                frankenterm_png: path_string(&frankenterm_png),
                wezterm_png: path_string(&wezterm_png),
                reason: Some("missing_wezterm_png".to_string()),
            });
            continue;
        }

        let frankenterm_image = load_png_rgba8(&frankenterm_png)?;
        let wezterm_image = load_png_rgba8(&wezterm_png)?;
        match compare_images(&frankenterm_image, &wezterm_image, thresholds) {
            Ok(comparison) => {
                rows.push(WeztermRenderComparisonRow {
                    input_id,
                    frame_id,
                    status: if comparison.passed {
                        "pass"
                    } else {
                        "diverged"
                    },
                    metrics: serde_json::to_value(&comparison.metrics)?,
                    thresholds,
                    frankenterm_png: path_string(&frankenterm_png),
                    wezterm_png: path_string(&wezterm_png),
                    reason: (!comparison.passed).then(|| "metric_threshold_exceeded".to_string()),
                });
            }
            Err(err) => {
                rows.push(WeztermRenderComparisonRow {
                    input_id,
                    frame_id,
                    status: "diverged",
                    metrics: failed_metrics(thresholds),
                    thresholds,
                    frankenterm_png: path_string(&frankenterm_png),
                    wezterm_png: path_string(&wezterm_png),
                    reason: Some(err.to_string()),
                });
            }
        }
    }

    let report = WeztermRenderComparisonReport {
        schema_version: "wezterm-render-comparison.v1",
        comparisons: rows,
    };
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output, serde_json::to_string_pretty(&report)? + "\n")?;
    emit_json(json!({
        "phase": "wezterm-comparison-report",
        "status": "written",
        "output": output,
        "frames_compared_total": report.comparisons.len(),
    }));
    Ok(())
}

fn discover_png_frames(root: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    if !root.is_dir() {
        return Err(format!(
            "frame root does not exist or is not a directory: {}",
            root.display()
        )
        .into());
    }
    let mut frames = Vec::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        {
            continue;
        }
        frames.push(path.strip_prefix(root)?.to_path_buf());
    }
    frames.sort();
    Ok(frames)
}

fn comparison_ids(relative: &Path) -> (String, String) {
    let mut components: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    let file_name = components
        .pop()
        .unwrap_or_else(|| "frame-000.png".to_string());
    let frame_id = Path::new(&file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("frame-000")
        .to_string();
    let input_id = if components.is_empty() {
        frame_id.clone()
    } else {
        components.join("/")
    };
    (input_id, frame_id)
}

fn failed_metrics(thresholds: Thresholds) -> serde_json::Value {
    json!({
        "ssim": 0.0,
        "l_inf": 255,
        "changed_pixels": 0,
        "total_pixels": 0,
        "changed_pixel_fraction": 1.0,
        "thresholds": thresholds,
    })
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn discover_fixtures(
    root: &Path,
    arg_filters: &[String],
) -> Result<Vec<Fixture>, Box<dyn std::error::Error>> {
    let filter = fixture_filter(arg_filters);
    let mut fixtures = Vec::new();
    discover_fixtures_in_dir(root, root, filter.as_deref(), &mut fixtures)?;
    fixtures.sort_by(|a, b| a.name.cmp(&b.name));
    if filter.is_some() && fixtures.is_empty() {
        if env_fixture_filter_present() {
            return Err("GPU_HARNESS_FIXTURE_FILTER did not match any fixtures".into());
        }
        return Ok(fixtures);
    }
    Ok(fixtures)
}

fn discover_fixtures_in_dir(
    root: &Path,
    dir: &Path,
    filter: Option<&[String]>,
    fixtures: &mut Vec<Fixture>,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join("input.json").is_file() {
            discover_fixtures_in_dir(root, &path, filter, fixtures)?;
            continue;
        }
        let name = fixture_name(root, &path)?;
        if filter.is_some_and(|allowed| !matches_fixture_filter(allowed, &name)) {
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
    Ok(())
}

fn fixture_name(root: &Path, path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let relative = path.strip_prefix(root)?;
    let parts: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();
    Ok(parts.join("/"))
}

fn matches_fixture_filter(filters: &[String], name: &str) -> bool {
    filters
        .iter()
        .any(|filter| name == filter || name.starts_with(&format!("{filter}/")))
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
    let lines = fixture_lines(&fixture.input)?;
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
    let mut input = HeadlessFixtureInput {
        viewport: viewport_from_meta(&fixture.meta.viewport),
        monitors: fixture
            .input
            .monitors
            .iter()
            .map(|monitor| HeadlessMonitor {
                id: monitor.id.clone(),
                x: monitor.x,
                width: monitor.width,
                dpi: monitor.dpi,
                color_profile: monitor.color_profile.clone(),
            })
            .collect(),
        lines,
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
    let mut frames = if fixture.input.resize_sequence.is_empty() {
        vec![ResizeFrameSpec {
            second: 0,
            width: fixture.meta.viewport.width,
            height: fixture.meta.viewport.height,
            dpi: fixture.meta.viewport.dpi,
        }]
    } else {
        fixture.input.resize_sequence.clone()
    };
    frames.sort_by_key(|frame| frame.second);
    let mut rendered_frame = None;
    for frame_spec in frames {
        input.viewport = HeadlessViewport {
            width: frame_spec.width,
            height: frame_spec.height,
            dpi: frame_spec.dpi,
        };
        let frame = render_headless(&input)?;
        emit_json(json!({
            "phase": "render-frame",
            "name": fixture.name,
            "resize_second": frame_spec.second,
            "ms": frame.render_ms,
            "glyphs": frame.glyphs_cached,
            "fonts_loaded": frame.fonts_loaded,
            "texture_format": frame.texture_format,
            "gpu": frame.gpu,
        }));
        rendered_frame = Some(frame);
    }
    let frame = rendered_frame.ok_or("headless fixture rendered no frames")?;
    if frame.width != fixture.meta.viewport.width || frame.height != fixture.meta.viewport.height {
        return Err(format!(
            "fixture `{}` final frame was {}x{} but meta viewport is {}x{}",
            fixture.name,
            frame.width,
            frame.height,
            fixture.meta.viewport.width,
            fixture.meta.viewport.height
        )
        .into());
    }
    let glyphs_cached = u64::try_from(frame.glyphs_cached).ok();
    let fonts_loaded = u64::try_from(frame.fonts_loaded).ok();
    let texture_format = Some(frame.texture_format.clone());
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

#[cfg(feature = "headless-render")]
fn viewport_from_meta(viewport: &Viewport) -> HeadlessViewport {
    HeadlessViewport {
        width: viewport.width,
        height: viewport.height,
        dpi: viewport.dpi,
    }
}

#[cfg(feature = "headless-render")]
fn fixture_lines(input: &InputSpec) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut lines = input.lines.clone();
    if let Some(generated) = &input.generated_lines {
        if generated.count > 100_000 {
            return Err(format!(
                "generated_lines.count={} exceeds GPU stress harness cap of 100000",
                generated.count
            )
            .into());
        }
        for index in 0..generated.count {
            lines.push(generated.template.replace("{i}", &format!("{index:06}")));
        }
    }
    Ok(lines)
}

#[cfg(not(feature = "headless-render"))]
fn render_headless_fixture(
    _fixture: &Fixture,
) -> Result<RenderOutcome, Box<dyn std::error::Error>> {
    Err("headless_terminal fixtures require --features headless-render".into())
}

#[cfg(feature = "headless-render")]
#[derive(Debug)]
struct FuzzHarnessState {
    cols: u16,
    rows: u16,
    lines: Vec<String>,
    cursor_col: u16,
    cursor_row: u16,
    selection_anchor: Option<(u16, u16)>,
    selection: Option<HeadlessSelection>,
    focused: bool,
}

#[cfg(feature = "headless-render")]
impl FuzzHarnessState {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            lines: vec!["FrankenTerm renderer fuzz lane".to_string()],
            cursor_col: 0,
            cursor_row: 0,
            selection_anchor: None,
            selection: None,
            focused: true,
        }
    }

    fn apply_event(&mut self, event_index: u64, event: &FuzzInputEvent) -> String {
        match event {
            FuzzInputEvent::Resize { cols, rows } => {
                self.cols = (*cols).max(8);
                self.rows = (*rows).max(8);
                self.clamp_cursor_and_selection();
                format!("{event_index}: resize {}x{}", self.cols, self.rows)
            }
            FuzzInputEvent::Write { bytes } => {
                let text = visible_bytes(bytes);
                self.append_text(&text);
                format!("{event_index}: write len={}", bytes.len())
            }
            FuzzInputEvent::EscapeBurst { bytes } => {
                let text = visible_bytes(bytes);
                self.append_text(&format!("[escape:{text}]"));
                format!("{event_index}: escape len={}", bytes.len())
            }
            FuzzInputEvent::Scroll { lines } => {
                self.apply_scroll(*lines);
                format!("{event_index}: scroll {lines}")
            }
            FuzzInputEvent::SelectStart { col, row } => {
                let cell = self.clamp_cell(*col, *row);
                self.selection_anchor = Some(cell);
                self.selection = Some(HeadlessSelection {
                    start_col: u32::from(cell.0),
                    start_row: u32::from(cell.1),
                    end_col: u32::from(cell.0),
                    end_row: u32::from(cell.1),
                });
                format!("{event_index}: select-start {},{}", cell.0, cell.1)
            }
            FuzzInputEvent::SelectExtend { col, row } => {
                let end = self.clamp_cell(*col, *row);
                let start = self.selection_anchor.unwrap_or(end);
                self.selection = Some(HeadlessSelection {
                    start_col: u32::from(start.0),
                    start_row: u32::from(start.1),
                    end_col: u32::from(end.0),
                    end_row: u32::from(end.1),
                });
                format!("{event_index}: select-extend {},{}", end.0, end.1)
            }
            FuzzInputEvent::SelectEnd => {
                self.selection_anchor = None;
                self.selection = None;
                format!("{event_index}: select-end")
            }
            FuzzInputEvent::FocusToggle => {
                self.focused = !self.focused;
                self.append_text(if self.focused {
                    "[focus:on]"
                } else {
                    "[focus:off]"
                });
                format!("{event_index}: focus {}", self.focused)
            }
            FuzzInputEvent::Clear => {
                self.lines.clear();
                self.lines.push(format!("clear at event {event_index}"));
                self.cursor_col = 0;
                self.cursor_row = 0;
                self.selection_anchor = None;
                self.selection = None;
                format!("{event_index}: clear")
            }
        }
    }

    fn append_text(&mut self, text: &str) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        let line_limit = usize::from(self.cols).saturating_mul(4).max(32);
        let tail = self.lines.last_mut().expect("line is present");
        tail.push_str(text);
        while self
            .lines
            .last()
            .is_some_and(|line| line.len() > line_limit)
        {
            let overflow = self
                .lines
                .last_mut()
                .expect("line is present")
                .split_off(line_limit);
            self.lines.push(overflow);
        }
        self.trim_lines();
        let row = self.lines.len().saturating_sub(1);
        let col = self.lines.last().map_or(0, String::len);
        self.cursor_row = u16::try_from(row)
            .unwrap_or(u16::MAX)
            .min(self.rows.saturating_sub(1));
        self.cursor_col = u16::try_from(col % usize::from(self.cols.max(1))).unwrap_or(0);
    }

    fn apply_scroll(&mut self, lines: i32) {
        if lines > 0 {
            for step in 0..lines.min(16) {
                self.lines.push(format!("scroll-forward marker {step}"));
            }
        } else if lines < 0 {
            for step in 0..lines.saturating_abs().min(16) {
                self.lines.insert(0, format!("scrollback marker {step}"));
            }
        }
        self.trim_lines();
        self.clamp_cursor_and_selection();
    }

    fn trim_lines(&mut self) {
        let cap = usize::from(self.rows).saturating_mul(4).max(32);
        if self.lines.len() > cap {
            let drop_count = self.lines.len() - cap;
            self.lines.drain(0..drop_count);
        }
    }

    fn clamp_cursor_and_selection(&mut self) {
        self.cursor_col = self.cursor_col.min(self.cols.saturating_sub(1));
        self.cursor_row = self.cursor_row.min(self.rows.saturating_sub(1));
        if let Some(selection) = self.selection {
            let start = self.clamp_cell(selection.start_col as u16, selection.start_row as u16);
            let end = self.clamp_cell(selection.end_col as u16, selection.end_row as u16);
            self.selection = Some(HeadlessSelection {
                start_col: u32::from(start.0),
                start_row: u32::from(start.1),
                end_col: u32::from(end.0),
                end_row: u32::from(end.1),
            });
        }
    }

    fn clamp_cell(&self, col: u16, row: u16) -> (u16, u16) {
        (
            col.min(self.cols.saturating_sub(1)),
            row.min(self.rows.saturating_sub(1)),
        )
    }

    fn to_input(&self) -> HeadlessFixtureInput {
        HeadlessFixtureInput {
            viewport: HeadlessViewport {
                width: u32::from(self.cols).saturating_mul(8).max(64),
                height: u32::from(self.rows).saturating_mul(14).max(64),
                dpi: 96.0,
            },
            monitors: Vec::new(),
            lines: self.lines.clone(),
            cursor: Some(HeadlessCursor {
                row: u32::from(self.cursor_row),
                col: u32::from(self.cursor_col),
                shape: if self.focused {
                    frankenterm_gui::headless_render::HeadlessCursorShape::Block
                } else {
                    frankenterm_gui::headless_render::HeadlessCursorShape::Beam
                },
            }),
            selection: self.selection,
            font_set_sha: Some("fuzz-no-font-fetch".to_string()),
            cursor_blink_disabled: true,
            ime_disabled: true,
        }
    }
}

#[cfg(feature = "headless-render")]
struct FuzzRunContext<'a> {
    run_root: &'a Path,
    stale_hashes: &'a mut VecDeque<(u32, u64)>,
    seen_nonblank: &'a mut bool,
    violations: &'a mut Vec<ViolationRecord>,
    seed: u64,
    duration_secs: u32,
    runs_dir: &'a Path,
}

#[cfg(feature = "headless-render")]
struct FuzzViolationInput<'a> {
    kind: ViolationKind,
    event_index: u32,
    frame_index: u32,
    before: &'a RgbaImage,
    after: &'a RgbaImage,
    diff: Option<&'a RgbaImage>,
    recent_events: &'a VecDeque<String>,
}

#[cfg(feature = "headless-render")]
fn render_fuzz_checkpoint(
    state: &FuzzHarnessState,
    event_index: u64,
    frame_index: u32,
    recent_events: &VecDeque<String>,
    context: &mut FuzzRunContext<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = state.to_input();
    let first = render_headless(&input)?;
    let second = render_headless(&input)?;
    let first_image = image_from_frame(&first)?;
    let second_image = image_from_frame(&second)?;
    let event_index_u32 = u32::try_from(event_index).unwrap_or(u32::MAX);

    emit_json(json!({
        "phase": "fuzz-frame",
        "event_index": event_index,
        "frame_index": frame_index,
        "render_ms": first.render_ms.saturating_add(second.render_ms),
        "width": first.width,
        "height": first.height,
        "glyphs": first.glyphs_cached,
        "fonts_loaded": first.fonts_loaded,
        "texture_format": first.texture_format,
        "gpu": first.gpu,
    }));

    if is_blank_rgba(&first.rgba) {
        if *context.seen_nonblank {
            record_fuzz_violation(
                context,
                FuzzViolationInput {
                    kind: ViolationKind::BlankFrame,
                    event_index: event_index_u32,
                    frame_index,
                    before: &first_image,
                    after: &second_image,
                    diff: None,
                    recent_events,
                },
            )?;
        }
    } else {
        *context.seen_nonblank = true;
    }

    let frame_hash = hash_bytes(&first.rgba);
    let stale_prior = context
        .stale_hashes
        .iter()
        .find(|(prior_event, prior_hash)| {
            event_index_u32.saturating_sub(*prior_event) >= FUZZ_STALE_FRAME_EVENT_DISTANCE
                && *prior_hash == frame_hash
        })
        .map(|(prior_event, _)| *prior_event);
    if let Some(prior_event) = stale_prior {
        record_fuzz_violation(
            context,
            FuzzViolationInput {
                kind: ViolationKind::StaleFullFrame {
                    stale_distance: event_index_u32.saturating_sub(prior_event),
                },
                event_index: event_index_u32,
                frame_index,
                before: &first_image,
                after: &second_image,
                diff: None,
                recent_events,
            },
        )?;
    }
    context
        .stale_hashes
        .push_back((event_index_u32, frame_hash));
    while context.stale_hashes.len() > FUZZ_MAX_STALE_HASHES {
        context.stale_hashes.pop_front();
    }

    let comparison = compare_images(&second_image, &first_image, Thresholds::default())?;
    if !comparison.passed {
        let kind = if comparison.metrics.l_inf >= 32 {
            ViolationKind::TearBand {
                delta_l_inf: u32::from(comparison.metrics.l_inf),
            }
        } else if comparison.metrics.ssim < comparison.metrics.thresholds.min_ssim {
            ViolationKind::SsimBelowThreshold {
                ssim: comparison.metrics.ssim,
                threshold: comparison.metrics.thresholds.min_ssim,
            }
        } else {
            ViolationKind::ExcessivePixelChange {
                fraction: comparison.metrics.changed_pixel_fraction,
                threshold: comparison.metrics.thresholds.max_changed_pixel_fraction,
            }
        };
        record_fuzz_violation(
            context,
            FuzzViolationInput {
                kind,
                event_index: event_index_u32,
                frame_index,
                before: &first_image,
                after: &second_image,
                diff: Some(&comparison.diff),
                recent_events,
            },
        )?;
    }

    Ok(())
}

#[cfg(feature = "headless-render")]
fn record_fuzz_violation(
    context: &mut FuzzRunContext<'_>,
    input: FuzzViolationInput<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    let start_at_event_idx = input.event_index;
    let record = ViolationRecord {
        event_index: input.event_index,
        frame_index: input.frame_index,
        kind: input.kind,
        reproducer_seed: context.seed,
        start_at_event_idx,
        log_excerpt: Some(recent_event_excerpt(input.recent_events)),
    };
    let artifact_dir = context.run_root.join(record.artifact_subdir());
    fs::create_dir_all(&artifact_dir)?;
    write_png_deterministic(&artifact_dir.join("before.png"), input.before)?;
    write_png_deterministic(&artifact_dir.join("after.png"), input.after)?;
    let generated_diff;
    let diff_image = match input.diff {
        Some(diff) => diff,
        None if input.before.dimensions() == input.after.dimensions() => {
            generated_diff = compare_images(input.after, input.before, Thresholds::default())?.diff;
            &generated_diff
        }
        None => input.after,
    };
    write_png_deterministic(&artifact_dir.join("diff.png"), diff_image)?;
    fs::write(
        artifact_dir.join("log.jsonl"),
        input
            .recent_events
            .iter()
            .map(|event| serde_json::to_string(&json!({ "event": event })))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n")
            + "\n",
    )?;
    write_reproducer(
        &artifact_dir.join("reproducer.sh"),
        context.seed,
        start_at_event_idx,
        context.duration_secs.clamp(1, 60),
        context.runs_dir,
    )?;
    context.violations.push(record);
    Ok(())
}

#[cfg(feature = "headless-render")]
fn image_from_frame(frame: &HeadlessFrame) -> Result<RgbaImage, Box<dyn std::error::Error>> {
    RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone()).ok_or_else(
        || -> Box<dyn std::error::Error> {
            format!(
                "headless renderer returned invalid fuzz RGBA frame {}x{}",
                frame.width, frame.height
            )
            .into()
        },
    )
}

#[cfg(feature = "headless-render")]
fn is_blank_rgba(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4)
        .all(|pixel| pixel[0] <= 4 && pixel[1] <= 4 && pixel[2] <= 4)
}

#[cfg(feature = "headless-render")]
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &byte in bytes {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(feature = "headless-render")]
fn push_recent_event(events: &mut VecDeque<String>, event: String) {
    events.push_back(event);
    while events.len() > 64 {
        events.pop_front();
    }
}

#[cfg(feature = "headless-render")]
fn recent_event_excerpt(events: &VecDeque<String>) -> String {
    let len = events.len();
    let start = len.saturating_sub(8);
    events
        .iter()
        .skip(start)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(feature = "headless-render")]
fn visible_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes {
        match byte {
            b'\x1b' => out.push_str("<ESC>"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out
}

#[cfg(feature = "headless-render")]
fn write_run_meta(run_root: &Path, meta: &RunMeta) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(run_root)?;
    let mut encoded = serde_json::to_string_pretty(meta)?;
    encoded.push('\n');
    fs::write(run_root.join("meta.json"), encoded)?;
    Ok(())
}

#[cfg(feature = "headless-render")]
fn write_reproducer(
    path: &Path,
    seed: u64,
    start_at_event_idx: u32,
    duration_secs: u32,
    runs_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let quoted_runs_dir = shell_quote(&runs_dir.display().to_string());
    let script = format!(
        "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         cargo test -p frankenterm-gui --features headless-render --test gpu_regression -- \\\n\
             --nocapture \\\n\
             --fuzz-seed=0x{seed:016x} \\\n\
             --fuzz-start-at={start_at_event_idx} \\\n\
             --fuzz-duration={duration_secs} \\\n\
             --runs-dir={quoted_runs_dir}\n"
    );
    fs::write(path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(feature = "headless-render")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(feature = "headless-render")]
fn default_fuzz_runs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("gpu-regression")
        .join("runs")
}

#[cfg(feature = "headless-render")]
fn unix_millis_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(feature = "headless-render")]
fn fuzz_host() -> String {
    env::var("RUNNER_NAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| format!("local-{}", env::consts::OS))
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

fn fixture_filter(arg_filters: &[String]) -> Option<Vec<String>> {
    let mut fixtures: Vec<String> = env::var("GPU_HARNESS_FIXTURE_FILTER")
        .ok()
        .into_iter()
        .flat_map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|fixture| !fixture.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    fixtures.extend(
        arg_filters
            .iter()
            .map(String::as_str)
            .map(str::trim)
            .filter(|fixture| !fixture.is_empty())
            .map(ToOwned::to_owned),
    );
    let fixtures: Vec<String> = fixtures
        .into_iter()
        .map(|fixture| fixture.trim_matches('/').to_string())
        .filter(|fixture| !fixture.is_empty())
        .collect();
    if fixtures.is_empty() {
        None
    } else {
        Some(fixtures)
    }
}

fn env_fixture_filter_present() -> bool {
    env::var("GPU_HARNESS_FIXTURE_FILTER")
        .ok()
        .is_some_and(|value| value.split(',').any(|fixture| !fixture.trim().is_empty()))
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
