//! Lindley-bounds diagnostic producer (br-ft-43x69 substrate-pass).
//!
//! Computes a model comparison; it does not run a benchmark or establish
//! measured provenance. Absent model/empirical inputs use HISTORICAL
//! documentation defaults, never a current release measurement.
//!
//! ## Telemetry input
//!
//! `FT_LINDLEY_STAGE_TELEMETRY_JSON` may contain a serialized
//! `LindleyTelemetryModel`. `FT_LINDLEY_STAGE_TELEMETRY_PATH` may point
//! at a file with the same JSON. If neither is set, the example uses
//! `LindleyTelemetryModel::documented_default()`. Explicit input must be
//! UTF-8 without NUL bytes and at most 64 KiB, including JSON whitespace.
//! File reads consume at most 64 KiB plus one byte to detect oversize input;
//! oversized input is rejected, never silently truncated.
//!
//! `FT_LINDLEY_EMPIRICAL_P99_MS` accepts only finite nonnegative numbers.
//! Only absence selects the historical 8.5ms reference. Empty, malformed,
//! negative, non-finite and non-Unicode values fail with exit 2.
//!
//! `FT_RELEASE_VERSION` defaults to `0.0.0-substrate`. Any other version
//! requires explicit model and empirical inputs plus matching
//! `FT_LINDLEY_INPUT_SHA256=sha256:<64 lowercase hex digits>`.
//! The digest covers the exact UTF-8 `input_provenance.payload_json` bytes:
//! compact serde_json serialization of `InputPayload`, in field order
//! `telemetry_model`, `empirical_p99_ms`, without a trailing newline. Model
//! fields use their declared serde order and parsed numeric values. The
//! encoding is `serde-json-lindley-inputs-v1`; it is not general canonical JSON.
//! Optional `FT_LINDLEY_INPUT_ORIGIN` records a caller-declared external origin.
//! A matching digest proves input binding only, not measurement authenticity,
//! source identity, performance coverage or release readiness.
//!
//! `FT_LINDLEY_BOUNDS_EMIT_JSON_MARKERS=1` opts into fixed stdout boundary
//! markers for the RCH wrapper. No caller-selected marker text is accepted.
//!
//! ## Usage
//!
//! ```text
//! cargo run --locked -j 1 --example lindley_bounds_build \
//!     -p frankenterm-core --no-default-features
//! ```
//!
//! The default development profile runs a diagnostic calculation; it provides
//! no performance measurement.
//!
//! `--measure-live` is a separate, opt-in real mux capture experiment. Build
//! with `--features vendored --profile release-interactive` on the same host as
//! an isolated DSR-built mux. Launch one owned pane at >=80 columns with
//! `sh -c 'stty -echo; exec <this-example-binary> --pane-producer'`, 24 visible
//! rows and 64 lines of scrollback. The bounded snapshot must retain the complete
//! 4KiB frame while keeping inter-snapshot overlap <=4KiB. Supply
//! `FT_LINDLEY_MUX_SOCKET`, `FT_LINDLEY_PANE_ID`, a NEW
//! `FT_LINDLEY_DB_PATH`, `FT_RELEASE_VERSION`, `FT_LINDLEY_SOURCE_SHA`, and
//! `FT_LINDLEY_ARRIVAL_RATE_EVENTS_PER_MS` (for example 0.1, declared before
//! measuring). `FT_WEZTERM_CLI` must name the same candidate CLI if the normal
//! read-only fallback is needed. The example never creates or discovers panes.
//! Invoke through `scripts/lindley-bounds-build.sh --measure-live-executable
//! <this-example-binary>`: its external process watchdog bounds initialization,
//! synchronous file writes and shutdown, which cooperative async timers cannot.
//! Live mode requires this declared watchdog contract and regular-file stdout.
//! Retain stdout, stderr, database, source/build/host/filesystem receipts and
//! exact mux/example executable hashes. It emits 100 calibration bursts of ten
//! 4096-byte frames, freezes its model, then measures 100 held-out bursts.
//! A failed delay/service/arrival check OR failed 20% agreement exits 1 while
//! retaining the JSON. Exit 2 is an execution/input/integrity failure. Per-burst
//! trace lines survive partial failures. Neither exit 0 nor a declared source
//! SHA authenticates the build or proves future, renderer or full-watch SLOs.
//!
//! Or via the wrapper:
//!
//! ```text
//! bash scripts/lindley-bounds-build.sh
//! ```
//!
//! ## Exit codes
//!
//! Exits 0 for a comparison within tolerance, 1 for a failed or undefined
//! comparison (with diagnostic JSON), and 2 for invalid input/encoding.
//! Exit 0 alone is not release evidence or proof of an upper bound: the
//! separate `exceeds_analytical_bound` field reports empirical exceedance.

use frankenterm_core::latency_stages::LindleyTelemetryModel;
use frankenterm_core::network_calculus_bound::{LindleyBoundsArtifact, pipeline_delay_bound};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::Read;
use std::process::ExitCode;

const HISTORICAL_VERSION: &str = "0.0.0-substrate";
const JSON_BEGIN: &str = "__FT_LINDLEY_BOUNDS_JSON_BEGIN__";
const JSON_END: &str = "__FT_LINDLEY_BOUNDS_JSON_END__";
const MAX_TELEMETRY_BYTES: usize = 64 * 1024;

#[derive(Serialize)]
struct InputPayload<'a> {
    telemetry_model: &'a LindleyTelemetryModel,
    empirical_p99_ms: f64,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args == ["--pane-producer"] {
        return match live_measurement::pane_producer() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lindley pane producer: {error}");
                ExitCode::from(2)
            }
        };
    }
    if args == ["--measure-live"] {
        return match live_measurement::run() {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::from(1),
            Err(error) => {
                eprintln!("lindley live measurement: {error}");
                ExitCode::from(2)
            }
        };
    }
    if !args.is_empty() {
        eprintln!("expected no arguments, --pane-producer or --measure-live");
        return ExitCode::from(2);
    }
    match build_diagnostic() {
        Ok(within_tolerance) => {
            if within_tolerance {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("lindley_bounds_build: {error}");
            ExitCode::from(2)
        }
    }
}

/// Actual mux/overlap/storage workload, separate from supplied-input diagnostics.
/// The operator owns the dedicated mux, pane, database path and retained stdout.
/// This producer never discovers or creates panes, launches a mux, or publishes
/// an attestation. Final source/build identity requires the external DSR receipt.
mod live_measurement {
    use std::io::{BufRead, Read, Write};

    pub fn frame(sequence: u32) -> String {
        let mut text = format!("\nFT LINDLEY BEGIN {sequence:08}\n");
        let end = format!("FT LINDLEY END {sequence:08}");
        while text.len() + end.len() + 65 <= 4096 {
            text.push_str("bounded terminal capture workload with ordinary public text only\n");
        }
        text.push_str(&"x".repeat(4096 - text.len() - end.len() - 1));
        text.push('\n');
        text.push_str(&end);
        text
    }

    pub fn pane_producer() -> Result<(), String> {
        let mut output = std::io::stdout().lock();
        // Fill the initial screen before baselining. Leave the cursor on the
        // ready marker, so the next leading newline is a true append rather
        // than replacement of the terminal's trailing blank screen rows.
        for _ in 0..96 {
            writeln!(output, "bounded warmup line").map_err(|error| error.to_string())?;
        }
        write!(output, "FT LINDLEY READY V1").map_err(|error| error.to_string())?;
        output.flush().map_err(|error| error.to_string())?;
        let mut expected = 0_u32;
        // read_line is bounded by the trusted harness's numeric protocol; use
        // take to reject oversized or unterminated input without growing a line.
        let mut input = std::io::stdin().lock();
        loop {
            let mut line = Vec::new();
            let count = std::io::Read::by_ref(&mut input)
                .take(32)
                .read_until(b'\n', &mut line)
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Ok(());
            }
            let request = std::str::from_utf8(&line).map_err(|error| error.to_string())?;
            if !request.ends_with('\n') || request.trim() != expected.to_string() {
                return Err("expected the next numeric sequence followed by newline".into());
            }
            output
                .write_all(frame(expected).as_bytes())
                .and_then(|()| output.flush())
                .map_err(|error| error.to_string())?;
            expected = expected.checked_add(1).ok_or("sequence exhausted")?;
        }
    }

    #[cfg(not(all(unix, feature = "vendored")))]
    pub fn run() -> Result<bool, String> {
        Err("--measure-live requires a Unix host and --features vendored".into())
    }

    #[cfg(all(unix, feature = "vendored"))]
    pub fn run() -> Result<bool, String> {
        measured::run()
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn frame_is_bounded_distinguishable_and_terminal_append_safe() {
            for sequence in [0, 999, u32::MAX] {
                let frame = super::frame(sequence);
                assert_eq!(frame.len(), 4096);
                assert!(frame.starts_with('\n'));
                assert!(!frame.ends_with('\n'));
                assert!(frame.lines().all(|line| line.len() < 80));
                assert!(frame.contains(&format!("FT LINDLEY BEGIN {sequence:08}")));
                assert!(frame.ends_with(&format!("FT LINDLEY END {sequence:08}")));
            }
            assert_ne!(super::frame(0), super::frame(1));
        }
    }

    #[cfg(all(unix, feature = "vendored"))]
    mod measured {
        use super::super::*;
        use frankenterm_core::cx::Cx;
        use frankenterm_core::ingest::{CapturedSegmentKind, PaneCursor};
        use frankenterm_core::latency_stages::{LatencyStage, LindleyStageTelemetry};
        use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder, sleep_with_cx};
        use frankenterm_core::storage::{PaneRecord, StorageHandle};
        use frankenterm_core::vendored::{DirectMuxClientConfig, MuxPool, MuxPoolConfig};
        use frankenterm_core::wezterm::WeztermClient;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        const BURST: usize = 10;
        const BURSTS_PER_PHASE: usize = 100;

        #[derive(Clone, Serialize)]
        struct Observation {
            sequence: u32,
            transient_read_rejections: u32,
            // Monotonic nanoseconds relative to the measurement epoch. Stage
            // boundaries include batching wait before storage admission.
            stages_ns: [[u64; 2]; 3],
            content_sha256: String,
        }

        fn retryable_capture_read(error: &frankenterm_core::Error) -> bool {
            use frankenterm_core::error::{
                MuxEffectCertainty, MuxOperation, MuxRejectionCode, MuxRetryAuthority, WeztermError,
            };
            matches!(error, frankenterm_core::Error::Wezterm(WeztermError::MuxRejection(rejection))
                if rejection.has_consistent_authority()
                    && rejection.operation == MuxOperation::ReadPaneText
                    && rejection.code == MuxRejectionCode::BackendFailure
                    && rejection.effect == MuxEffectCertainty::NotApplied
                    && rejection.retry == MuxRetryAuthority::SafeAfterBackoff)
        }

        async fn poll_frame<F, Fut>(
            cx: &Cx,
            marker: &str,
            deadline: Instant,
            mut read: F,
        ) -> Result<(String, u32), String>
        where
            F: FnMut() -> Fut,
            Fut: std::future::Future<Output = frankenterm_core::Result<String>>,
        {
            let mut rejections = 0_u32;
            loop {
                cx.checkpoint().map_err(|error| error.to_string())?;
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(format!(
                        "producer frame deadline expired; transient_read_rejections={rejections}"
                    ));
                }
                let result = frankenterm_core::runtime_async::timeout_with_cx(
                    cx,
                    remaining,
                    read(),
                )
                .await
                .map_err(|error| {
                    format!(
                        "capture read deadline: {error}; transient_read_rejections={rejections}"
                    )
                })?;
                cx.checkpoint().map_err(|error| error.to_string())?;
                if Instant::now() >= deadline {
                    return Err(format!(
                        "producer frame deadline expired; transient_read_rejections={rejections}"
                    ));
                }
                match result {
                    Ok(text) => {
                        if text.len() > 8192 {
                            return Err("dedicated pane snapshot exceeds 8KiB".into());
                        }
                        if text.contains(marker) {
                            return Ok((text, rejections));
                        }
                    }
                    Err(error) if retryable_capture_read(&error) => {
                        rejections = rejections.checked_add(1).ok_or("retry count overflow")?;
                    }
                    Err(error) => return Err(error.to_string()),
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(format!(
                        "producer frame deadline expired; transient_read_rejections={rejections}"
                    ));
                }
                sleep_with_cx(cx, remaining.min(Duration::from_millis(1)))
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }

        fn elapsed(epoch: Instant) -> Result<u64, String> {
            u64::try_from(epoch.elapsed().as_nanos()).map_err(|error| error.to_string())
        }

        fn required(name: &str) -> Result<String, String> {
            optional_env(name)?
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{name} is required"))
        }

        pub fn run() -> Result<bool, String> {
            #[cfg(unix)]
            use std::os::fd::AsFd;
            let watchdog_seconds = required("FT_LINDLEY_EXTERNAL_WATCHDOG_SECS")?
                .parse::<u32>()
                .map_err(|error| error.to_string())?;
            if !(1..=2400).contains(&watchdog_seconds) {
                return Err("external watchdog must be between 1 and 2400 seconds".into());
            }
            let output = fs::File::from(
                std::io::stdout()
                    .as_fd()
                    .try_clone_to_owned()
                    .map_err(|error| error.to_string())?,
            );
            if !output
                .metadata()
                .map_err(|error| error.to_string())?
                .is_file()
            {
                return Err("live measurement stdout must be an ordinary retained file".into());
            }
            let socket = required("FT_LINDLEY_MUX_SOCKET")?;
            let pane_id: u64 = required("FT_LINDLEY_PANE_ID")?
                .parse::<u64>()
                .map_err(|error| error.to_string())?;
            let db_path = required("FT_LINDLEY_DB_PATH")?;
            let version = required("FT_RELEASE_VERSION")?;
            let source = required("FT_LINDLEY_SOURCE_SHA")?;
            if source.len() != 40 || !source.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("FT_LINDLEY_SOURCE_SHA must be a full commit SHA".into());
            }
            let arrival_rate = required("FT_LINDLEY_ARRIVAL_RATE_EVENTS_PER_MS")?
                .parse::<f64>()
                .map_err(|error| error.to_string())?;
            if !arrival_rate.is_finite() || !(0.001..=100.0).contains(&arrival_rate) {
                return Err("arrival rate must be finite in [0.001, 100] events/ms".into());
            }
            // Reserve a new file atomically; never reuse or overwrite an existing
            // database. Keep it for independent row verification after shutdown.
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&db_path)
                .map_err(|error| format!("reserve new database: {error}"))?;
            let runtime = RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            let result = runtime.block_on(async {
                let cx = Cx::for_request();
                let storage = frankenterm_core::runtime_async::timeout_with_cx(
                    &cx,
                    Duration::from_secs(60),
                    StorageHandle::new_with_cx(&cx, &db_path),
                )
                .await
                .map_err(|error| format!("storage initialization timeout: {error}"))?
                .map_err(|error| error.to_string())?;
                let result = frankenterm_core::runtime_async::timeout_with_cx(
                    &cx,
                    Duration::from_secs(2400),
                    measure(&cx, &storage, &socket, pane_id, arrival_rate),
                )
                .await
                .map_err(|error| format!("overall workload timeout: {error}"))
                .and_then(std::convert::identity);
                // Cleanup must run even after capture/integrity errors and must
                // use an independent context if the workload was cancelled.
                let shutdown = storage
                    .shutdown_with_cx(&Cx::for_request())
                    .await
                    .map_err(|error| error.to_string());
                match (result, shutdown) {
                    (Ok(value), Ok(())) => Ok(value),
                    (Err(error), Ok(())) => Err(error),
                    (result, Err(error)) => Err(format!(
                        "writer shutdown failed: {error}; workload failed={}",
                        result.is_err()
                    )),
                }
            })?;
            let (model, observations, initial_read_rejections) = result;
            let (arrival, stages) = model.to_network_calculus_inputs()?;
            let bound = pipeline_delay_bound(arrival, &stages);
            let held_out = &observations[BURST * BURSTS_PER_PHASE..];
            let mut latencies: Vec<f64> = held_out
                .iter()
                .map(|row| (row.stages_ns[2][1] - row.stages_ns[0][0]) as f64 / 1e6)
                .collect();
            latencies.sort_by(f64::total_cmp);
            let empirical = latencies[(99 * latencies.len()).div_ceil(100) - 1];
            let artifact = LindleyBoundsArtifact {
                release_version: version,
                arrival,
                stages,
                analytical_bound_ms: bound.unwrap_or(f64::INFINITY),
                empirical_p99_ms: empirical,
            };
            let mut output: serde_json::Value =
                serde_json::from_str(&artifact.render_attestation_json())
                    .map_err(|error| error.to_string())?;
            let maximum = latencies.last().copied().ok_or("missing held-out rows")?;
            let observed_bound_holds = bound.is_some_and(|value| maximum <= value);
            let arrival_holds = arrival_conforms(&observations, arrival_rate);
            let service_holds: Vec<bool> = (0..3)
                .map(|index| service_conforms(held_out, index, &model.stages[index]))
                .collect();
            let trace_json =
                serde_json::to_string(&observations).map_err(|error| error.to_string())?;
            let trace_hash = hex::encode(Sha256::digest(trace_json.as_bytes()));
            output["measurement"] = serde_json::json!({
                "schema": "frankenterm.lindley-live-capture.v1",
                "scope": "dedicated_mux_capture_delta_grouped_storage_finite_workload",
                "declared_source_sha": source,
                "declared_source_verified": false,
                "build_profile_verified": false,
                "declared_external_watchdog_seconds": watchdog_seconds,
                "release_ready": false,
                "mux_socket": socket,
                "pane_id": pane_id,
                "database_path": db_path,
                "platform": std::env::consts::OS,
                "architecture": std::env::consts::ARCH,
                "payload_bytes": 4096,
                "transient_read_rejections": observations.iter().map(|row| u64::from(row.transient_read_rejections)).sum::<u64>(),
                "initial_read_rejections": initial_read_rejections,
                "overlap_bytes": 4096,
                "burst_events": BURST,
                "calibration_rows": BURST * BURSTS_PER_PHASE,
                "held_out_rows": BURST * BURSTS_PER_PHASE,
                "maximum_held_out_latency_ms": maximum,
                "observed_delay_bound_holds": observed_bound_holds,
                "arrival_envelope_holds": arrival_holds,
                "held_out_service_curves_hold": service_holds,
                "calibration_method": "per-stage minimum burst throughput; maximum per-request latency, including batch wait; frozen before held-out requests",
                "latency_field_semantics": "model p99_latency_ms fields contain calibration maximums, not quantile guarantees",
                "capture_timing": "numeric stimulus dispatch through complete mux snapshot receipt; includes producer response and any bounded snapshot polling",
                "trace_encoding": "serde-json-observations-v1",
                "trace_json": trace_json,
                "trace_sha256": trace_hash,
                "telemetry_model": model,
                "observations": observations,
                "excluded": ["production_watch_scheduler", "pattern_detection", "event_dispatch", "renderer", "future_workload_guarantee", "power_loss_durability"],
                "grouping": "concurrent append_segment requests through production writer; physical transaction group size not observed",
            });
            println!("{JSON_BEGIN}");
            println!(
                "{}",
                serde_json::to_string_pretty(&output).map_err(|error| error.to_string())?
            );
            println!("{JSON_END}");
            Ok(observed_bound_holds
                && arrival_holds
                && service_holds.iter().all(|value| *value)
                && artifact.comparison().within_tolerance())
        }

        async fn measure(
            cx: &Cx,
            storage: &StorageHandle,
            socket: &str,
            pane_id: u64,
            arrival_rate: f64,
        ) -> Result<(LindleyTelemetryModel, Vec<Observation>, u32), String> {
            let pool = Arc::new(MuxPool::new(MuxPoolConfig {
                mux: DirectMuxClientConfig::default().with_socket_path(socket),
                ..MuxPoolConfig::default()
            }));
            let client = WeztermClient::with_socket(socket)
                .with_mux_pool(pool)
                .with_timeout(5)
                .with_retries(1);
            // Socket/pane readiness does not prove the producer has finished
            // its warmup output. Use the same bounded, typed capture polling
            // before establishing the baseline; do not add a startup sleep.
            let (initial, initial_read_rejections) = poll_frame(
                cx,
                "FT LINDLEY READY V1",
                Instant::now() + Duration::from_secs(5),
                || client.get_text_with_cx(cx, pane_id, false),
            )
            .await?;
            storage
                .upsert_pane_with_cx(
                    cx,
                    PaneRecord {
                        pane_id,
                        pane_uuid: None,
                        domain: "lindley-dedicated-mux".into(),
                        window_id: None,
                        tab_id: None,
                        title: None,
                        cwd: None,
                        tty_name: None,
                        first_seen_at: 0,
                        last_seen_at: 0,
                        observed: true,
                        ignore_reason: None,
                        last_decision_at: None,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            let mut cursor = PaneCursor::new(pane_id);
            cursor.capture_snapshot(&initial, 4096, None);
            let epoch = Instant::now();
            let mut observations = Vec::with_capacity(2 * BURST * BURSTS_PER_PHASE);
            let interval = Duration::from_secs_f64(BURST as f64 / arrival_rate / 1000.0);
            let mut previous_burst = None;
            let mut expected_content = Vec::new();
            let mut calibration_model = None;
            for burst in 0..2 * BURSTS_PER_PHASE {
                if let Some(started) = previous_burst {
                    let wait = interval.saturating_sub(Instant::now().duration_since(started));
                    sleep_with_cx(cx, wait)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                previous_burst = Some(Instant::now());
                let mut pending = Vec::with_capacity(BURST);
                for offset in 0..BURST {
                    let sequence =
                        u32::try_from(burst * BURST + offset).map_err(|error| error.to_string())?;
                    let started = elapsed(epoch)?;
                    client
                        .send_text_no_paste_with_cx(cx, pane_id, &sequence.to_string())
                        .await
                        .map_err(|error| error.to_string())?;
                    let marker = format!("FT LINDLEY END {sequence:08}");
                    let capture_deadline = Instant::now() + Duration::from_secs(5);
                    let (snapshot, transient_read_rejections) =
                        poll_frame(cx, &marker, capture_deadline, || {
                            client.get_text_with_cx(cx, pane_id, false)
                        })
                        .await?;
                    let captured = elapsed(epoch)?;
                    let segment = cursor
                        .capture_snapshot(&snapshot, 4096, None)
                        .ok_or("new frame produced no delta")?;
                    if !matches!(segment.kind, CapturedSegmentKind::Delta)
                        || segment.content.trim_matches('\n')
                            != super::frame(sequence).trim_matches('\n')
                    {
                        return Err(format!("gap or missing frame in delta {sequence}"));
                    }
                    let extracted = elapsed(epoch)?;
                    let hash = hex::encode(Sha256::digest(segment.content.as_bytes()));
                    pending.push((
                        sequence,
                        started,
                        captured,
                        extracted,
                        hash,
                        segment.content,
                        transient_read_rejections,
                    ));
                }
                let results =
                    futures::future::join_all(
                        pending.iter().map(
                            |(
                                sequence,
                                started,
                                captured,
                                extracted,
                                hash,
                                content,
                                rejections,
                            )| async move {
                                let stored = storage
                                    .append_segment_with_cx(cx, pane_id, content, None)
                                    .await
                                    .map_err(|error| error.to_string())?;
                                let completed = elapsed(epoch)?;
                                if stored.content != *content || stored.seq != u64::from(*sequence)
                                {
                                    return Err(
                                        "committed segment content or sequence differs".to_string()
                                    );
                                }
                                Ok(Observation {
                                    sequence: *sequence,
                                    transient_read_rejections: *rejections,
                                    stages_ns: [
                                        [*started, *captured],
                                        [*captured, *extracted],
                                        [*extracted, completed],
                                    ],
                                    content_sha256: hash.clone(),
                                })
                            },
                        ),
                    )
                    .await;
                for result in results {
                    observations.push(result?);
                }
                println!(
                    "__FT_LINDLEY_TRACE_BURST__ {}",
                    serde_json::to_string(&observations[observations.len() - BURST..])
                        .map_err(|error| error.to_string())?
                );
                expected_content.extend(pending.into_iter().map(|row| row.5));
                if burst + 1 == BURSTS_PER_PHASE {
                    calibration_model = Some(calibrate(&observations, arrival_rate)?);
                }
            }
            let mut stored = storage
                .get_segments_with_cx(cx, pane_id, expected_content.len() + 1)
                .await
                .map_err(|error| error.to_string())?;
            stored.sort_by_key(|row| row.seq);
            if stored.len() != expected_content.len()
                || stored.iter().zip(&expected_content).enumerate().any(
                    |(index, (row, content))| row.seq != index as u64 || row.content != *content,
                )
            {
                return Err("persisted corpus differs from captured deltas".into());
            }
            Ok((
                calibration_model.ok_or("missing calibration model")?,
                observations,
                initial_read_rejections,
            ))
        }

        fn calibrate(
            rows: &[Observation],
            arrival_rate: f64,
        ) -> Result<LindleyTelemetryModel, String> {
            let mut stages = Vec::new();
            for (index, stage) in [
                LatencyStage::PtyCapture,
                LatencyStage::DeltaExtraction,
                LatencyStage::StorageWrite,
            ]
            .into_iter()
            .enumerate()
            {
                let latency_ns = rows
                    .iter()
                    .map(|row| row.stages_ns[index][1] - row.stages_ns[index][0])
                    .max()
                    .ok_or("empty calibration")?;
                let rate = rows
                    .chunks_exact(BURST)
                    .map(|batch| {
                        let first = batch
                            .iter()
                            .map(|row| row.stages_ns[index][0])
                            .min()
                            .unwrap();
                        let last = batch
                            .iter()
                            .map(|row| row.stages_ns[index][1])
                            .max()
                            .unwrap();
                        BURST as f64 * 1e6 / (last - first).max(1) as f64
                    })
                    .fold(f64::INFINITY, f64::min);
                stages.push(LindleyStageTelemetry::try_new(
                    stage,
                    rate,
                    latency_ns as f64 / 1e6,
                )?);
            }
            LindleyTelemetryModel::try_new(BURST as f64, arrival_rate, stages)
        }

        fn arrival_conforms(rows: &[Observation], rate: f64) -> bool {
            rows.iter().enumerate().all(|(first, start)| {
                rows[first..].iter().enumerate().all(|(offset, end)| {
                    (offset + 1) as f64
                        <= BURST as f64
                            + rate * (end.stages_ns[0][0] - start.stages_ns[0][0]) as f64 / 1e6
                })
            })
        }

        // Check D(t) >= (A * beta)(t) immediately before each departure, where
        // the continuous lower bound is largest before D jumps. Stage arrivals
        // and completions are counted independently, so async acknowledgment
        // order is not assumed to be FIFO. This proves only this finite trace.
        fn service_conforms(
            rows: &[Observation],
            stage: usize,
            model: &LindleyStageTelemetry,
        ) -> bool {
            let mut arrivals: Vec<u64> = rows.iter().map(|row| row.stages_ns[stage][0]).collect();
            let mut departures: Vec<u64> = rows.iter().map(|row| row.stages_ns[stage][1]).collect();
            arrivals.sort_unstable();
            departures.sort_unstable();
            departures.iter().all(|departure| {
                let time = departure.saturating_sub(1);
                let completed = departures.partition_point(|value| *value <= time);
                let lower = arrivals
                    .iter()
                    .enumerate()
                    .take_while(|(_, start)| **start <= time)
                    .map(|(count, start)| {
                        count as f64
                            + model.service_rate_events_per_ms
                                * (((time - start) as f64 / 1e6) - model.p99_latency_ms).max(0.0)
                    })
                    .fold(f64::INFINITY, f64::min);
                // s=t is also a candidate in the min-plus convolution.
                let arrived = arrivals.partition_point(|value| *value <= time);
                lower.min(arrived as f64) <= completed as f64 + 1e-9
            })
        }

        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn frame_poll_retries_only_typed_read_authority_and_preserves_deadline() {
                use frankenterm_core::error::{MuxOperation, MuxRejection, WeztermError};
                let runtime = RuntimeBuilder::current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    let cx = Cx::for_testing();
                    let rejection = || {
                        frankenterm_core::Error::Wezterm(WeztermError::MuxRejection(
                            MuxRejection::backend_failure(MuxOperation::ReadPaneText),
                        ))
                    };
                    let mut attempts = 0;
                    let (text, count) = poll_frame(
                        &cx,
                        "ready",
                        Instant::now() + Duration::from_secs(1),
                        || {
                            attempts += 1;
                            std::future::ready(if attempts == 1 {
                                Err(rejection())
                            } else {
                                Ok("ready".into())
                            })
                        },
                    )
                    .await
                    .expect("safe rejection followed by complete frame");
                    assert_eq!((text.as_str(), count, attempts), ("ready", 1, 2));

                    for authority in [
                        MuxRejection::backend_failure(MuxOperation::SendText),
                        MuxRejection::pane_not_found(MuxOperation::ReadPaneText, 0),
                        MuxRejection::deadline_exceeded(MuxOperation::ReadPaneText),
                    ] {
                        let mut attempts = 0;
                        let result = poll_frame(
                            &cx,
                            "ready",
                            Instant::now() + Duration::from_secs(1),
                            || {
                                attempts += 1;
                                std::future::ready(Err(frankenterm_core::Error::Wezterm(
                                    WeztermError::MuxRejection(authority),
                                )))
                            },
                        )
                        .await;
                        assert!(result.is_err());
                        assert_eq!(attempts, 1);
                    }
                    let mut attempts = 0;
                    let result = poll_frame(&cx, "ready", Instant::now(), || {
                        attempts += 1;
                        std::future::ready(Ok("ready".into()))
                    })
                    .await;
                    assert!(result.is_err());
                    assert_eq!(attempts, 0, "expired deadline must not invoke read");

                    let result = poll_frame(
                        &cx,
                        "ready",
                        Instant::now() + Duration::from_millis(1),
                        || {
                            std::thread::sleep(Duration::from_millis(5));
                            std::future::ready(Ok("ready".into()))
                        },
                    )
                    .await;
                    assert!(
                        result.is_err(),
                        "late ready result must not outrun deadline"
                    );

                    let result = poll_frame(
                        &cx,
                        "ready",
                        Instant::now() + Duration::from_secs(1),
                        || {
                            cx.cancel_with(
                                frankenterm_core::outcome::CancelKind::User,
                                Some("poll control"),
                            );
                            std::future::ready(Ok("ready".into()))
                        },
                    )
                    .await;
                    assert!(result.is_err(), "cancelled read cannot publish a frame");
                });
            }

            fn row(sequence: u32, start: u64, end: u64) -> Observation {
                Observation {
                    sequence,
                    transient_read_rejections: 0,
                    stages_ns: [[start, end]; 3],
                    content_sha256: String::new(),
                }
            }

            #[test]
            fn held_out_service_check_rejects_delayed_departure() {
                let model =
                    LindleyStageTelemetry::try_new(LatencyStage::PtyCapture, 1.0, 1.0).unwrap();
                assert!(service_conforms(&[row(0, 0, 1_000_000)], 0, &model));
                assert!(!service_conforms(&[row(0, 0, 3_000_000)], 0, &model));
            }

            #[test]
            fn arrival_check_rejects_excess_burst() {
                let rows: Vec<_> = (0..11).map(|seq| row(seq, 0, 1)).collect();
                assert!(arrival_conforms(&rows[..10], 1.0));
                assert!(!arrival_conforms(&rows, 1.0));
            }

            #[test]
            fn calibration_excludes_later_observations() {
                let mut rows: Vec<_> = (0..10).map(|seq| row(seq, 0, 1_000_000)).collect();
                let model = calibrate(&rows, 0.1).unwrap();
                rows.push(row(10, 0, 100_000_000));
                assert_eq!(model, calibrate(&rows[..10], 0.1).unwrap());
                assert!(!service_conforms(&rows[10..], 0, &model.stages[0]));
            }
        }
    }
}

fn build_diagnostic() -> Result<bool, String> {
    let emit_markers = match optional_env("FT_LINDLEY_BOUNDS_EMIT_JSON_MARKERS")?.as_deref() {
        None => false,
        Some("1") => true,
        Some(_) => return Err("FT_LINDLEY_BOUNDS_EMIT_JSON_MARKERS must be absent or 1".into()),
    };
    let (model, model_source) = load_lindley_model()?;
    let empirical_input = optional_env("FT_LINDLEY_EMPIRICAL_P99_MS")?;
    let empirical_p99_ms = parse_empirical(empirical_input.as_deref())?;
    let release_version =
        optional_env("FT_RELEASE_VERSION")?.unwrap_or_else(|| HISTORICAL_VERSION.to_string());
    if release_version.is_empty() || release_version.trim() != release_version {
        return Err("FT_RELEASE_VERSION must be nonempty without surrounding whitespace".into());
    }
    let declared_digest = optional_env("FT_LINDLEY_INPUT_SHA256")?;
    let declared_origin = optional_env("FT_LINDLEY_INPUT_ORIGIN")?;
    if declared_origin
        .as_deref()
        .is_some_and(|origin| origin.trim().is_empty())
    {
        return Err("FT_LINDLEY_INPUT_ORIGIN must be nonempty when supplied".into());
    }
    let (arrival, stages) = model
        .to_network_calculus_inputs()
        .map_err(|error| format!("invalid Lindley telemetry: {error}"))?;
    let payload_json = serde_json::to_string(&InputPayload {
        telemetry_model: &model,
        empirical_p99_ms,
    })
    .map_err(|error| format!("failed to serialize input payload: {error}"))?;
    let actual_digest = format!(
        "sha256:{}",
        hex::encode(Sha256::digest(payload_json.as_bytes()))
    );
    validate_input_binding(
        &release_version,
        model_source != "historical_documented_default",
        empirical_input.is_some(),
        declared_digest.as_deref(),
        &actual_digest,
    )?;
    let historical = model_source == "historical_documented_default" || empirical_input.is_none();
    if historical {
        eprintln!("lindley_bounds_build: HISTORICAL diagnostic inputs; not a release measurement");
    }

    // Compute the analytical bound from the substrate's
    // `pipeline_delay_bound` (Pay-Bursts-Only-Once composition + Lindley
    // delay bound). The historical default yields about 8.067ms;
    // supplied models can yield a different or unrepresentable bound.
    let analytical_bound_ms = pipeline_delay_bound(arrival, &stages).unwrap_or_else(|| {
        eprintln!(
            "lindley_bounds_build: pipeline_delay_bound returned None — \
             arrival/stages combination is unstable or its bound is unrepresentable"
        );
        f64::INFINITY
    });

    let artifact = LindleyBoundsArtifact {
        release_version,
        arrival,
        stages,
        analytical_bound_ms,
        empirical_p99_ms,
    };

    let mut diagnostic: serde_json::Value =
        serde_json::from_str(&artifact.render_attestation_json())
            .map_err(|error| format!("substrate emitted invalid diagnostic JSON: {error}"))?;
    diagnostic["exceeds_analytical_bound"] =
        serde_json::json!(artifact.comparison().exceeds_bound());
    diagnostic["input_provenance"] = serde_json::json!({
        "status": if historical { "historical_diagnostic" } else { "supplied_inputs_unverified" },
        "model_source": model_source,
        "empirical_source": if empirical_input.is_some() { "caller_supplied" } else { "historical_8_5_ms_reference" },
        "payload_encoding": "serde-json-lindley-inputs-v1",
        "payload_json": payload_json,
        "input_sha256": actual_digest,
        "declared_input_sha256": declared_digest,
        "input_binding_verified": declared_digest.is_some(),
        "declared_external_origin": declared_origin,
        "measurement_provenance_verified": false,
        "release_ready": false,
    });
    let json = serde_json::to_string_pretty(&diagnostic)
        .map_err(|error| format!("failed to encode diagnostic: {error}"))?;
    if emit_markers {
        println!("{JSON_BEGIN}");
    }
    println!("{json}");
    if emit_markers {
        println!("{JSON_END}");
    }

    let comparison = artifact.comparison();
    if comparison.within_tolerance() {
        Ok(true)
    } else {
        eprintln!(
            "lindley_bounds_build: tolerance check FAILED. \
             analytical_bound_ms={analytical} empirical_p99_ms={empirical} \
             deviation_pct={dev:.2} (substrate's TOLERANCE_PCT=20.0)",
            analytical = artifact.analytical_bound_ms,
            empirical = artifact.empirical_p99_ms,
            dev = comparison.deviation_pct().unwrap_or(f64::NAN),
        );
        Ok(false)
    }
}

fn optional_env(name: &str) -> Result<Option<String>, String> {
    decode_env(name, env::var(name))
}

fn decode_env(name: &str, value: Result<String, env::VarError>) -> Result<Option<String>, String> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} must be valid Unicode")),
    }
}

fn parse_empirical(input: Option<&str>) -> Result<f64, String> {
    let Some(input) = input else {
        return Ok(8.5);
    };
    input
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| "FT_LINDLEY_EMPIRICAL_P99_MS must be a finite nonnegative number".into())
}

fn validate_input_binding(
    release_version: &str,
    explicit_model: bool,
    explicit_empirical: bool,
    declared_digest: Option<&str>,
    actual_digest: &str,
) -> Result<(), String> {
    if declared_digest.is_some_and(|digest| digest != actual_digest) {
        return Err(
            "FT_LINDLEY_INPUT_SHA256 does not match the serialized model and empirical input"
                .into(),
        );
    }
    if release_version != HISTORICAL_VERSION
        && (!explicit_model || !explicit_empirical || declared_digest.is_none())
    {
        return Err("a release version requires explicit telemetry, empirical p99 and matching FT_LINDLEY_INPUT_SHA256; defaults are HISTORICAL diagnostics".into());
    }
    Ok(())
}

fn load_lindley_model() -> Result<(LindleyTelemetryModel, &'static str), String> {
    let json = optional_env("FT_LINDLEY_STAGE_TELEMETRY_JSON")?;
    let path = optional_env("FT_LINDLEY_STAGE_TELEMETRY_PATH")?;
    if json.is_some() && path.is_some() {
        return Err("supply one telemetry input: JSON or PATH, not both".into());
    }
    if let Some(json) = json {
        return parse_lindley_model(json.as_bytes(), "FT_LINDLEY_STAGE_TELEMETRY_JSON")
            .map(|model| (model, "caller_supplied_json"));
    }
    if let Some(path) = path {
        let file = fs::File::open(&path)
            .map_err(|error| format!("failed to open telemetry file {path}: {error}"))?;
        return read_lindley_model(file, &format!("Lindley telemetry file {path}"))
            .map(|model| (model, "caller_supplied_file"));
    }

    Ok((
        LindleyTelemetryModel::documented_default(),
        "historical_documented_default",
    ))
}

fn read_lindley_model(reader: impl Read, source: &str) -> Result<LindleyTelemetryModel, String> {
    let mut bytes = Vec::with_capacity(MAX_TELEMETRY_BYTES + 1);
    reader
        .take((MAX_TELEMETRY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read {source}: {error}"))?;
    parse_lindley_model(&bytes, source)
}

fn parse_lindley_model(bytes: &[u8], source: &str) -> Result<LindleyTelemetryModel, String> {
    if bytes.len() > MAX_TELEMETRY_BYTES {
        return Err(format!("{source}: telemetry exceeds 65536-byte limit"));
    }
    if bytes.contains(&0) {
        return Err(format!("{source}: telemetry must not contain NUL bytes"));
    }
    let json = std::str::from_utf8(bytes)
        .map_err(|_| format!("{source}: telemetry must be valid UTF-8"))?;
    serde_json::from_str(json).map_err(|error| format!("invalid {source}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absence_is_distinct_from_invalid_empirical_input() {
        assert_eq!(parse_empirical(None).unwrap().to_bits(), 8.5_f64.to_bits());
        assert_eq!(
            parse_empirical(Some("0")).unwrap().to_bits(),
            0.0_f64.to_bits()
        );
        for input in ["", "garbage", "-1", "NaN", "inf", "-inf", "1e999", " 8.5"] {
            assert!(parse_empirical(Some(input)).is_err(), "input={input:?}");
        }
        assert!(
            decode_env("EMPIRICAL", Err(env::VarError::NotPresent))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            decode_env("EMPIRICAL", Ok(String::new())).unwrap(),
            Some(String::new())
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_environment_is_not_an_absent_default() {
        use std::os::unix::ffi::OsStringExt;
        let invalid = std::ffi::OsString::from_vec(vec![0xff]);
        assert!(decode_env("EMPIRICAL", Err(env::VarError::NotUnicode(invalid))).is_err());
    }

    #[test]
    fn telemetry_limit_applies_before_parsing_without_truncation() {
        let model = LindleyTelemetryModel::documented_default();
        let mut bytes = serde_json::to_vec(&model).unwrap();
        bytes.resize(MAX_TELEMETRY_BYTES, b' ');
        assert_eq!(parse_lindley_model(&bytes, "inline").unwrap(), model);
        assert_eq!(
            read_lindley_model(bytes.as_slice(), "reader").unwrap(),
            model
        );

        // A valid JSON prefix plus whitespace must still fail above the cap.
        bytes.resize(MAX_TELEMETRY_BYTES * 2, b' ');
        assert!(
            parse_lindley_model(&bytes, "inline")
                .unwrap_err()
                .contains("exceeds 65536-byte limit")
        );
        let mut reader = std::io::Cursor::new(bytes);
        assert!(
            read_lindley_model(&mut reader, "reader")
                .unwrap_err()
                .contains("exceeds 65536-byte limit")
        );
        assert_eq!(reader.position(), (MAX_TELEMETRY_BYTES + 1) as u64);
    }

    #[test]
    fn telemetry_encoding_is_validated_before_deserialization() {
        for (bytes, expected) in [
            (b"\xff".as_slice(), "must be valid UTF-8"),
            (b"{}\0".as_slice(), "must not contain NUL bytes"),
        ] {
            assert!(
                parse_lindley_model(bytes, "inline")
                    .unwrap_err()
                    .contains(expected)
            );
            assert!(
                read_lindley_model(bytes, "reader")
                    .unwrap_err()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn release_binding_checks_both_input_values() {
        let model = LindleyTelemetryModel::documented_default();
        let encode = |model: &LindleyTelemetryModel, empirical_p99_ms| {
            serde_json::to_string(&InputPayload {
                telemetry_model: model,
                empirical_p99_ms,
            })
            .unwrap()
        };
        let original = encode(&model, 8.5);
        let digest = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(original.as_bytes()))
        );
        assert!(validate_input_binding("1.2.3", true, true, Some(&digest), &digest).is_ok());
        for (model_present, empirical_present, declared) in [
            (false, true, Some(digest.as_str())),
            (true, false, Some(digest.as_str())),
            (true, true, None),
        ] {
            assert!(
                validate_input_binding(
                    "1.2.3",
                    model_present,
                    empirical_present,
                    declared,
                    &digest
                )
                .is_err()
            );
        }
        let mut altered = model.clone();
        altered.arrival_burst_events += 1.0;
        for changed in [encode(&altered, 8.5), encode(&model, 8.6)] {
            let changed_digest =
                format!("sha256:{}", hex::encode(Sha256::digest(changed.as_bytes())));
            assert!(
                validate_input_binding("1.2.3", true, true, Some(&digest), &changed_digest)
                    .is_err()
            );
        }
    }
}
