//! W9.T — Target-Class Scale Campaign tests & verification (ft-7h5da.10.5).
//!
//! CAMPAIGN-LEVEL, zero-RCH, public-API-only verification that ties the W9
//! sub-results together at their CANONICAL target-class configs. The individual
//! sub-results already have their own unit/metamorphic coverage; this file does
//! NOT re-test their internals — it asserts the campaign acceptance that spans
//! them:
//!
//!   - W9.1 storage group-commit (ft-7h5da.10.1, CLOSED): higher write
//!     throughput. Verified here only via its analytical consequence (§C):
//!     raising the write-stage service rate strictly tightens the latency bound.
//!   - W9.2 re-derived Lindley/min-plus bound (`network_calculus_bound`): the
//!     formula-level cases live in that module's `#[cfg(test)]`. §C asserts the
//!     CAMPAIGN property — the target-class capture→extract→write pipeline has a
//!     finite (stable) bound that monotonically tightens with added capacity.
//!   - W9.3 200-pane replay-corpus load rig (`chaos_scale_harness`): the
//!     detection-probe / measured-metrics cases live in
//!     `tests/replay_load_rig_detection_metamorphic.rs` and the module's own
//!     tests. §A/§B assert the acceptance at the canonical `target_200_pane()`
//!     config: the rig is REPRODUCIBLE, drives both capture paths, has NO capture
//!     gaps, and stays within the tiered-scrollback memory budget — across every
//!     required scale point.
//!   - W9.4 signing→flip gate (ft-7h5da.10.4): verified end-to-end by
//!     `tests/target_class_signing_flip_e2e.rs` (ft-d7fow) +
//!     `tests/target_class_signing_flip_conformance.rs` (ft-1t0za). NOT
//!     duplicated here.
//!
//! DEFERRED (binary / rented-hardware / RCH-lane gated, out of this zero-RCH
//! slice — see the bead comment): `tests/e2e/target_class.sh` driving the rig at
//! increasing pane counts on the rented 64-core/256-GiB host; the bench-lane +
//! cockpit conformance producing a SIGNED non-skipped artifact; and the live
//! post-sign README/count update. Those need the paid run and the remote lane.

use frankenterm_core::chaos_scale_harness::{
    CaptureMode, ChaosScaleHarness, ReplayCorpusLoadRigConfig, ReplayCorpusLoadRigReport,
};
use frankenterm_core::network_calculus_bound::{
    ArrivalCurve, ServiceCurve, StageModel, pipeline_delay_bound,
};

// ── helpers ───────────────────────────────────────────────────────────────

fn run_target_200() -> ReplayCorpusLoadRigReport {
    ChaosScaleHarness::run_replay_corpus_load_rig(ReplayCorpusLoadRigConfig::target_200_pane())
        .expect("the 200-pane replay-corpus load rig runs")
}

/// A config for one of the campaign's required scale points, reusing the
/// canonical target-class thresholds so only the pane count varies.
fn config_for_scale_point(pane_count: u64) -> ReplayCorpusLoadRigConfig {
    ReplayCorpusLoadRigConfig {
        pane_count,
        ..ReplayCorpusLoadRigConfig::target_200_pane()
    }
}

// ── §A — W9.3 acceptance at the canonical 200-pane target ───────────────────

/// Acceptance: "the 200-pane rig is reproducible." Determinism is the whole
/// point of a replay-corpus rig (no live mux panes) — the same config must yield
/// a byte-identical report so a scale regression is a diff, not noise.
#[test]
fn target_200_pane_rig_is_reproducible() {
    let first = run_target_200();
    let second = run_target_200();
    assert_eq!(
        first, second,
        "the 200-pane replay-corpus load rig must be deterministic across runs"
    );
}

/// Acceptance: "assert no capture gaps." A capture gap means the rig dropped a
/// window of pane output — exactly the scale failure W9.3 must catch. Both the
/// poll and native-push paths must be gap-free at the target config.
#[test]
fn target_200_pane_rig_has_no_capture_gaps() {
    let report = run_target_200();
    for mode in [CaptureMode::Poll, CaptureMode::NativePush] {
        let result = report
            .mode_result(mode)
            .expect("capture mode result present");
        assert_eq!(
            result.capture_gap_count, 0,
            "{mode:?} capture path must have zero capture gaps at 200 panes: {result:?}"
        );
        assert_eq!(
            result.dropped_events, 0,
            "{mode:?} capture path must not drop events at 200 panes: {result:?}"
        );
    }
}

/// Acceptance: "bounded memory per the tiered-scrollback budgets." Each capture
/// path must stay within the config's memory ceiling (the 256 MiB target-class
/// budget) and the rig must report overall_pass at its own canonical thresholds.
#[test]
fn target_200_pane_rig_stays_within_memory_budget() {
    let config = ReplayCorpusLoadRigConfig::target_200_pane();
    let report = run_target_200();
    for mode in [CaptureMode::Poll, CaptureMode::NativePush] {
        let result = report
            .mode_result(mode)
            .expect("capture mode result present");
        assert!(
            result.memory_bytes <= config.memory_limit_bytes,
            "{mode:?} memory {} exceeds the tiered-scrollback budget {}",
            result.memory_bytes,
            config.memory_limit_bytes
        );
        assert!(
            result.capture_lag_p99_ms <= config.max_capture_lag_ms,
            "{mode:?} p99 capture lag {}ms exceeds the SLO ceiling {}ms",
            result.capture_lag_p99_ms,
            config.max_capture_lag_ms
        );
    }
    assert!(
        report.overall_pass,
        "the canonical 200-pane target config must pass its own thresholds: {:?}",
        report
            .capture_modes
            .iter()
            .map(|m| (m.mode, m.threshold_passed))
            .collect::<Vec<_>>()
    );
}

/// Acceptance: "the load rig drives ~200 simulated panes from replay corpora"
/// over BOTH capture paths, with detection driven by the production engine over
/// real corpus content (not a synthetic count).
#[test]
fn target_200_pane_rig_drives_both_capture_paths_over_real_corpus() {
    let report = run_target_200();
    assert_eq!(report.pane_count, 200, "report drives the 200-pane target");
    assert_eq!(
        report.capture_modes.len(),
        2,
        "both poll and native-push paths are exercised"
    );
    for mode in [CaptureMode::Poll, CaptureMode::NativePush] {
        let result = report
            .mode_result(mode)
            .expect("capture mode result present");
        assert_eq!(result.pane_count, 200, "{mode:?} drives all 200 panes");
        assert!(
            result.replay_events > 0 && result.replay_frames > 0,
            "{mode:?} replays real corpus events/frames: {result:?}"
        );
    }
    assert!(
        report.detection_probe.frames_scanned > 0 && report.detection_probe.bytes_scanned > 0,
        "the production PatternEngine scans real corpus egress, not a synthetic count: {:?}",
        report.detection_probe
    );
}

// ── §B — W9.T scale-matrix reproducibility + fail-closed ────────────────────

/// Acceptance: the rig is reproducible across EVERY required scale point
/// (10/50/200/1000), not just the headline 200. Each point runs without error
/// and is deterministic, and reports the pane count it was asked for.
#[test]
fn all_required_scale_points_run_deterministically() {
    for pane_count in [10_u64, 50, 200, 1_000] {
        let config = config_for_scale_point(pane_count);
        let first = ChaosScaleHarness::run_replay_corpus_load_rig(config.clone())
            .expect("required scale point runs");
        let second = ChaosScaleHarness::run_replay_corpus_load_rig(config)
            .expect("required scale point re-runs");
        assert_eq!(
            first, second,
            "scale point {pane_count} must be reproducible"
        );
        assert_eq!(
            first.pane_count, pane_count,
            "scale point {pane_count} reports its pane count"
        );
    }
}

/// Negative/canary: an unsupported scale point fails closed with an error rather
/// than silently fabricating a corpus for an unmodeled pane count.
#[test]
fn unsupported_scale_point_fails_closed() {
    let config = config_for_scale_point(7); // not in {10, 50, 200, 1000}
    let result = ChaosScaleHarness::run_replay_corpus_load_rig(config);
    assert!(
        result.is_err(),
        "an unsupported pane_count must fail closed, got: {result:?}"
    );
}

// ── §C — W9.1 → W9.2 campaign coherence (capacity tightens the bound) ────────

/// Build the campaign's headline pipeline: capture → delta-extract →
/// storage-write, as rate-latency service curves over a token-bucket arrival.
/// Returns (baseline_bound, group_commit_bound) where the only change is a
/// higher-throughput / lower-latency write stage (the W9.1 group-commit win).
fn pipeline_bounds_baseline_vs_group_commit() -> (f64, f64) {
    // Representative target-class arrival: a 200-pane burst at ~2 events/ms.
    let arrival = ArrivalCurve::new(200.0, 2.0);
    let capture = ServiceCurve::new(10.0, 1.0);
    let extract = ServiceCurve::new(8.0, 2.0);
    // Baseline single-writer storage path is the bottleneck.
    let write_baseline = ServiceCurve::new(4.0, 5.0);
    // Group commit batches the flush window: higher rate, lower per-flush latency.
    let write_group_commit = ServiceCurve::new(6.0, 3.0);

    let baseline = pipeline_delay_bound(
        arrival,
        &[
            StageModel::new("capture", capture),
            StageModel::new("delta_extract", extract),
            StageModel::new("storage_write", write_baseline),
        ],
    )
    .expect("baseline target-class pipeline is stable");
    let group_commit = pipeline_delay_bound(
        arrival,
        &[
            StageModel::new("capture", capture),
            StageModel::new("delta_extract", extract),
            StageModel::new("storage_write", write_group_commit),
        ],
    )
    .expect("group-commit target-class pipeline is stable");
    (baseline, group_commit)
}

/// Campaign coherence: the re-derived min-plus bound yields a FINITE worst-case
/// delay for the stable target-class pipeline, and raising the storage-write
/// stage's throughput (the W9.1 group-commit improvement) STRICTLY tightens that
/// bound — the analytical model and the storage win point the same direction.
#[test]
fn group_commit_capacity_strictly_tightens_the_analytical_bound() {
    let (baseline, group_commit) = pipeline_bounds_baseline_vs_group_commit();
    assert!(
        baseline.is_finite() && baseline > 0.0,
        "stable target-class pipeline has a finite positive delay bound: {baseline}"
    );
    assert!(
        group_commit < baseline,
        "added write throughput (group commit) must tighten the bound: \
         group_commit={group_commit} !< baseline={baseline}"
    );
    // Coherence with the rig: the analytical worst-case delay sits under the
    // load rig's enforced capture-lag SLO ceiling (the two W9 results agree).
    let lag_ceiling = ReplayCorpusLoadRigConfig::target_200_pane().max_capture_lag_ms;
    assert!(
        baseline <= lag_ceiling,
        "analytical pipeline bound {baseline}ms must not exceed the rig SLO ceiling {lag_ceiling}ms"
    );
}

/// The bound GATES over-subscription: when the bottleneck write stage cannot
/// keep up with arrival, the min-plus delay bound is undefined (`None`) — the
/// model refuses to certify an unstable target-class pipeline.
#[test]
fn oversubscribed_pipeline_bound_refuses_to_certify() {
    // Arrival rate 5 ev/ms exceeds the bottleneck write rate of 4 ev/ms.
    let arrival = ArrivalCurve::new(200.0, 5.0);
    let bound = pipeline_delay_bound(
        arrival,
        &[
            StageModel::new("capture", ServiceCurve::new(10.0, 1.0)),
            StageModel::new("storage_write", ServiceCurve::new(4.0, 5.0)),
        ],
    );
    assert!(
        bound.is_none(),
        "an over-subscribed pipeline must not yield a finite certified bound: {bound:?}"
    );
}
