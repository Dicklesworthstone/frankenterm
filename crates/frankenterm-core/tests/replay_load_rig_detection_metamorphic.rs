//! Metamorphic tests for the replay-corpus load rig's REAL detection probe and
//! measured capture metrics (ft-7h5da.10.3.x; bead ft-gqmju).
//!
//! The reality-check (ft-7h5da.10.3.2) found the rig emitted SYNTHETIC per-mode
//! metrics — hardcoded `interval/2`, `window*2`, `event_count/20`, `dropped=0` —
//! and never drove the real detection engine, so it could not catch the scale
//! regressions W9.3 requires. The fix replaced those with metrics MEASURED over
//! the real replay-corpus egress timeline, and added a detection probe that
//! drives every corpus egress frame through the production `PatternEngine`
//! (builtin packs).
//!
//! These tests assert the detection is genuinely corpus-driven (real), not
//! synthetic-over-config, via metamorphic relations: the probe is invariant
//! under capture-mode tuning (same corpus => same probe), scales with the corpus
//! (pane_count), and is deterministic; and the capture metrics are NOT the old
//! synthetic formulas.

use frankenterm_core::chaos_scale_harness::{
    CaptureMode, ChaosScaleHarness, ReplayCorpusDetectionProbe, ReplayCorpusLoadRigConfig,
};

fn detection_probe_for(pane_count: u64) -> ReplayCorpusDetectionProbe {
    let mut config = ReplayCorpusLoadRigConfig::target_200_pane();
    config.pane_count = pane_count;
    ChaosScaleHarness::run_replay_corpus_load_rig(config)
        .expect("replay-corpus rig runs")
        .detection_probe
}

// ===========================================================================
// Detection is corpus-driven (real production engine), not synthetic-over-config.
// ===========================================================================

#[test]
fn detection_probe_is_independent_of_capture_tuning() {
    // Same pane_count => same corpus. The detection probe is run over the corpus
    // egress frames by the production PatternEngine, so it MUST be identical no
    // matter how the CAPTURE-mode knobs are tuned. A synthetic-over-config
    // detector would move with these knobs; the real one cannot.
    let baseline =
        ChaosScaleHarness::run_replay_corpus_load_rig(ReplayCorpusLoadRigConfig::target_200_pane())
            .expect("rig runs")
            .detection_probe;

    let mut tuned = ReplayCorpusLoadRigConfig::target_200_pane();
    tuned.poll_interval_ms = 1234;
    tuned.native_push_dedup_window_ms = 1;
    tuned.queue_depth_limit_events_per_pane = 9_999;
    tuned.memory_limit_bytes = u64::MAX;
    tuned.max_capture_lag_ms = 1.0e9;
    let tuned_probe = ChaosScaleHarness::run_replay_corpus_load_rig(tuned)
        .expect("rig runs")
        .detection_probe;

    assert_eq!(
        baseline, tuned_probe,
        "detection probe must be capture-tuning-independent — it scans the real corpus \
         egress frames via the production PatternEngine, not a synthetic function of config"
    );
    assert!(
        baseline.frames_scanned > 0,
        "the production engine scanned real frames"
    );
    assert!(
        baseline.bytes_scanned > 0,
        "real bytes were read from the corpus"
    );
    assert!(baseline.detections >= baseline.panes_with_detections);
}

#[test]
fn detection_probe_is_deterministic() {
    // The production engine + builtin packs + deterministic corpus reproduce a
    // byte-identical probe (golden-style determinism).
    let a = detection_probe_for(200);
    let b = detection_probe_for(200);
    assert_eq!(
        a, b,
        "detection over the deterministic corpus must be reproducible"
    );
}

#[test]
fn detection_probe_scales_with_pane_count() {
    // Real detection runs over the corpus, so a larger scale point scans strictly
    // more egress frames + bytes — a constant/synthetic count would not.
    let small = detection_probe_for(10);
    let large = detection_probe_for(200);
    assert!(
        large.frames_scanned > small.frames_scanned,
        "more panes -> more real egress frames scanned ({} vs {})",
        large.frames_scanned,
        small.frames_scanned
    );
    assert!(
        large.bytes_scanned > small.bytes_scanned,
        "more panes -> more real bytes scanned"
    );
    assert!(
        large.detections >= small.detections,
        "more corpus content -> at least as many production detections"
    );
}

// ===========================================================================
// Capture metrics are MEASURED, not the old synthetic formulas.
// ===========================================================================

#[test]
fn capture_metrics_are_not_the_old_synthetic_formulas() {
    let report =
        ChaosScaleHarness::run_replay_corpus_load_rig(ReplayCorpusLoadRigConfig::target_200_pane())
            .expect("rig runs");
    let poll = report
        .mode_result(CaptureMode::Poll)
        .expect("poll mode recorded");
    let push = report
        .mode_result(CaptureMode::NativePush)
        .expect("native push mode recorded");

    // OLD synthetic Poll p99 == poll_interval*2 == 600. MEASURED: in (0, interval].
    assert!(
        (poll.capture_lag_p99_ms - 600.0).abs() > f64::EPSILON,
        "poll p99 must not be the synthetic poll_interval*2 (600), got {}",
        poll.capture_lag_p99_ms
    );
    assert!(
        poll.capture_lag_p99_ms > 0.0 && poll.capture_lag_p99_ms <= 300.0,
        "measured poll p99 lag is positive and bounded by the poll interval"
    );

    // OLD synthetic NativePush p99 == dedup_window*2 == 32. MEASURED: ~0 (the
    // tight per-pane egress bursts coalesce inside the dedup window).
    assert!(
        (push.capture_lag_p99_ms - 32.0).abs() > f64::EPSILON,
        "native push p99 must not be the synthetic dedup_window*2 (32), got {}",
        push.capture_lag_p99_ms
    );
    assert!(push.capture_lag_p99_ms.abs() <= f64::EPSILON);

    // OLD synthetic duplicates == event_count/20. MEASURED: the real coalesced
    // bursts — at least one suppression per pane.
    assert!(
        push.duplicate_events_suppressed >= push.pane_count,
        "dedup suppressions are the real coalesced bursts (>= one per pane), not event_count/20"
    );

    // Poll retains every frame, so it holds at least as much memory as the
    // dedup-coalescing push path — a real consequence of the measured timeline.
    assert!(poll.memory_bytes >= push.memory_bytes);
}
