//! `ft bench load-rig` reference runner (ft-7h5da.10.3.1 / W9.3a).
//!
//! Exposes the test-gated [`chaos_scale_harness`] 200-pane load rig as a
//! runnable on-demand surface. It drives
//! [`ChaosScaleHarness::run_replay_corpus_load_rig`] over a deterministic
//! large-swarm REPLAY CORPUS (no live mux panes are launched) and reports
//! per-capture-mode metrics plus the threshold verdict.
//!
//! Both capture modes are ALWAYS exercised — the periodic `poll` path and the
//! native push-event bridge (`native_push`, the path the README marks
//! "required" at 200 panes) — because the rig's value is comparing the two on
//! identical replay input. `--mode` only narrows the human-readable display.
//!
//! Usage:
//!   cargo run --example load_rig -p frankenterm-core -- \
//!     [--panes 10|50|200|1000]   (default 200) \
//!     [--mode poll|native-push|both]   (default both; display filter only) \
//!     [--max-capture-lag-ms F]   (default 750) \
//!     [--memory-limit-mb N]      (default 256) \
//!     [--poll-interval-ms N]     (default 300) \
//!     [--dedup-window-ms N]      (default 16) \
//!     [--queue-depth-limit N]    (default 32) \
//!     [--json]                   (emit the full report as JSON on stdout) \
//!     [--help]
//!
//! Exit codes:
//!   0 — rig ran and every exercised mode passed its thresholds.
//!   1 — rig ran but a mode failed its thresholds (a real regression to fix).
//!   2 — argument error or an unsupported scale point (valid: 10/50/200/1000).

use std::process::ExitCode;

use frankenterm_core::chaos_scale_harness::{
    CaptureMode, ChaosScaleHarness, ReplayCorpusCaptureModeResult, ReplayCorpusLoadRigConfig,
    ReplayCorpusLoadRigReport,
};

const HELP: &str = "\
ft load-rig — runnable replay-corpus 200-pane load rig (W9.3a)

OPTIONS:
  --panes N             scale point: 10, 50, 200 (default), or 1000
  --mode M              display filter: poll | native-push | both (default both)
  --max-capture-lag-ms F  capture-lag p99 ceiling per mode (default 750)
  --memory-limit-mb N   whole-run memory ceiling (default 256)
  --poll-interval-ms N  poll-mode interval (default 300)
  --dedup-window-ms N   native-push dedup window (default 16)
  --queue-depth-limit N per-pane queue-depth ceiling (default 32)
  --json                emit the full ReplayCorpusLoadRigReport as JSON
  --help                print this help

Both capture modes are always exercised; --mode only narrows the human display.";

fn next_value<'a>(
    args: &'a [String],
    index: &mut usize,
    flag: &str,
) -> Result<&'a str, String> {
    *index += 1;
    args.get(*index)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_mode(raw: &str) -> Result<Option<CaptureMode>, String> {
    match raw {
        "both" => Ok(None),
        "poll" => Ok(Some(CaptureMode::Poll)),
        "native-push" | "native_push" => Ok(Some(CaptureMode::NativePush)),
        other => Err(format!(
            "unknown --mode {other} (expected poll, native-push, or both)"
        )),
    }
}

const fn mode_token(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Poll => "poll",
        CaptureMode::NativePush => "native_push",
    }
}

struct Options {
    config: ReplayCorpusLoadRigConfig,
    display_mode: Option<CaptureMode>,
    as_json: bool,
}

fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut config = ReplayCorpusLoadRigConfig::target_200_pane();
    let mut display_mode = None;
    let mut as_json = false;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--help" | "-h" => return Ok(None),
            "--json" => as_json = true,
            "--panes" => {
                config.pane_count = next_value(args, &mut index, flag)?
                    .parse()
                    .map_err(|_| format!("{flag} expects an integer"))?;
            }
            "--mode" => {
                display_mode = parse_mode(next_value(args, &mut index, flag)?)?;
            }
            "--max-capture-lag-ms" => {
                config.max_capture_lag_ms = next_value(args, &mut index, flag)?
                    .parse()
                    .map_err(|_| format!("{flag} expects a number"))?;
            }
            "--memory-limit-mb" => {
                let mb: u64 = next_value(args, &mut index, flag)?
                    .parse()
                    .map_err(|_| format!("{flag} expects an integer"))?;
                config.memory_limit_bytes = mb.saturating_mul(1024 * 1024);
            }
            "--poll-interval-ms" => {
                config.poll_interval_ms = next_value(args, &mut index, flag)?
                    .parse()
                    .map_err(|_| format!("{flag} expects an integer"))?;
            }
            "--dedup-window-ms" => {
                config.native_push_dedup_window_ms = next_value(args, &mut index, flag)?
                    .parse()
                    .map_err(|_| format!("{flag} expects an integer"))?;
            }
            "--queue-depth-limit" => {
                config.queue_depth_limit_events_per_pane = next_value(args, &mut index, flag)?
                    .parse()
                    .map_err(|_| format!("{flag} expects an integer"))?;
            }
            other => return Err(format!("unknown argument {other} (try --help)")),
        }
        index += 1;
    }
    Ok(Some(Options {
        config,
        display_mode,
        as_json,
    }))
}

fn render_mode(result: &ReplayCorpusCaptureModeResult) {
    println!(
        "  [{}] {:<12} lag p50/p95/p99 = {:.1}/{:.1}/{:.1} ms | queue_depth_max = {}/pane | mem = {} MiB",
        if result.threshold_passed { "PASS" } else { "FAIL" },
        mode_token(result.mode),
        result.capture_lag_p50_ms,
        result.capture_lag_p95_ms,
        result.capture_lag_p99_ms,
        result.max_queue_depth_events_per_pane,
        result.memory_bytes / (1024 * 1024),
    );
    println!(
        "         events={} frames={} output={} MiB dedup_suppressed={} dropped={} gaps={}",
        result.replay_events,
        result.replay_frames,
        result.output_bytes / (1024 * 1024),
        result.duplicate_events_suppressed,
        result.dropped_events,
        result.capture_gap_count,
    );
}

fn render_report(report: &ReplayCorpusLoadRigReport, display_mode: Option<CaptureMode>) {
    println!(
        "load-rig: {} | scenario {} | {} panes | schema {}",
        if report.overall_pass { "PASS" } else { "FAIL" },
        report.scenario_id,
        report.pane_count,
        report.schema_version,
    );
    for result in &report.capture_modes {
        if display_mode.is_none_or(|mode| mode == result.mode) {
            render_mode(result);
        }
    }
    for limitation in &report.limitations {
        println!("  note: {limitation}");
    }
}

fn run() -> Result<ExitCode, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(options) = parse_args(&args)? else {
        println!("{HELP}");
        return Ok(ExitCode::SUCCESS);
    };

    let report = ChaosScaleHarness::run_replay_corpus_load_rig(options.config)
        .map_err(|error| format!("{error}; valid --panes values are 10, 50, 200, 1000"))?;

    if options.as_json {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|error| format!("failed to serialize report: {error}"))?;
        println!("{json}");
    } else {
        render_report(&report, options.display_mode);
    }

    if report.overall_pass {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}
