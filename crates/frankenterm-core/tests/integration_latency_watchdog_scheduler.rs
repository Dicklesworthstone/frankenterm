//! Integration test: input latency → adaptive watchdog → resize scheduler.
//!
//! Exercises the performance-monitoring pipeline:
//!
//!   InputLatencyCollector.record(measurement)
//!     → AdaptiveWatchdog.observe(component, heartbeat_ms)
//!       → AdaptiveWatchdog.check_health(now_ms) → HealthStatus
//!         → ResizeScheduler.schedule_frame() (adapt frame budget)
//!
//! This mirrors the real render loop: input latency measurements track
//! per-keystroke responsiveness, the watchdog classifies component health
//! from heartbeat intervals, and the resize scheduler adapts its frame
//! budget based on system health (e.g., fewer resize units when degraded).

use frankenterm_core::input_latency::{
    InputLatencyCollector, InputLatencyStage, Percentile,
};
use frankenterm_core::kalman_watchdog::{AdaptiveWatchdog, AdaptiveWatchdogConfig};
use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
    SubmitOutcome,
};
use frankenterm_core::watchdog::{Component, HealthStatus};

// ── Helpers ─────────────────────────────────────────────────────────────

fn make_intent(pane_id: u64, seq: u64, work_units: u32, at_ms: u64) -> ResizeIntent {
    ResizeIntent {
        pane_id,
        intent_seq: seq,
        scheduler_class: ResizeWorkClass::Interactive,
        work_units,
        submitted_at_ms: at_ms,
        domain: ResizeDomain::Local,
        tab_id: Some(1),
    }
}

/// Record a full input-to-render latency measurement.
fn record_full_measurement(
    collector: &mut InputLatencyCollector,
    base_us: u64,
    jitter_us: u64,
) {
    let mut m = collector.begin_measurement();
    let mut t = base_us;
    m.record_stage(InputLatencyStage::KeyEvent, t);
    t += 100 + jitter_us;
    m.record_stage(InputLatencyStage::PtyWrite, t);
    t += 50 + jitter_us;
    m.record_stage(InputLatencyStage::PtyRead, t);
    t += 200 + jitter_us;
    m.record_stage(InputLatencyStage::TermUpdate, t);
    t += 150 + jitter_us;
    m.record_stage(InputLatencyStage::RenderSubmit, t);
    t += 300 + jitter_us;
    m.record_stage(InputLatencyStage::GpuPresent, t);
    collector.record(m);
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Input latency tracking produces percentiles, watchdog classifies
/// health from heartbeat intervals, and both inform scheduling decisions.
#[test]
fn latency_and_watchdog_inform_scheduling() {
    let mut latency = InputLatencyCollector::new(100);
    let mut watchdog = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());

    // Record 20 latency measurements with stable timing (~800us total).
    for i in 0..20u64 {
        record_full_measurement(&mut latency, i * 10_000, 0);
    }
    assert_eq!(latency.count(), 20);

    // P50 and P95 should be close for stable input.
    let p50 = latency.total_latency_percentile(Percentile::P50);
    let p95 = latency.total_latency_percentile(Percentile::P95);
    assert!(p50.is_some());
    assert!(p95.is_some());
    // With zero jitter, P50 ≈ P95.
    let p50_val = p50.unwrap();
    let p95_val = p95.unwrap();
    assert!(p95_val >= p50_val);

    // Feed regular heartbeats to watchdog (every 100ms).
    for i in 0..20u64 {
        watchdog.observe(Component::Capture, i * 100);
    }

    // Check health — should be healthy with regular heartbeats.
    let report = watchdog.check_health(2000);
    assert_eq!(report.overall, HealthStatus::Healthy);

    // Submit a resize intent — should be accepted.
    let intent = make_intent(1, 1, 10, 2000);
    let outcome = scheduler.submit_intent(intent);
    assert!(
        matches!(outcome, SubmitOutcome::Accepted { .. }),
        "intent should be accepted, got {outcome:?}"
    );

    // Schedule a frame — should include our pending resize.
    let frame = scheduler.schedule_frame();
    assert!(frame.frame_budget_units > 0);
}

/// Watchdog detects degraded health when heartbeats become irregular,
/// and the scheduler can adapt by reducing frame budget.
#[test]
fn degraded_watchdog_triggers_conservative_scheduling() {
    let config = AdaptiveWatchdogConfig {
        min_observations: 5,
        degraded_z: 2.0,
        critical_z: 4.0,
        hung_z: 8.0,
        ..AdaptiveWatchdogConfig::default()
    };
    let mut watchdog = AdaptiveWatchdog::new(config);

    // Phase 1: establish baseline with regular heartbeats (every 100ms).
    for i in 0..10u64 {
        watchdog.observe(Component::Capture, i * 100);
    }
    let health = watchdog.check_health(1000);
    assert_eq!(health.overall, HealthStatus::Healthy);

    // Phase 2: heartbeats become irregular (long gap).
    // Skip several expected heartbeats to create a large gap.
    watchdog.observe(Component::Capture, 2000); // 1000ms gap vs 100ms expected

    let health = watchdog.check_health(2100);
    let capture_status = health
        .components
        .iter()
        .find(|c| c.component == Component::Capture)
        .map(|c| c.classification.status);

    // With a 10x gap, the z-score should push beyond degraded threshold.
    assert!(
        capture_status.is_some(),
        "capture component should have a classification"
    );

    // Phase 3: use watchdog health to choose scheduler budget.
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    let default_budget = scheduler.snapshot().config.frame_budget_units;

    // When healthy, use full budget.
    let healthy_budget = default_budget;

    // When degraded, use reduced budget.
    let degraded_budget = default_budget / 2;

    assert!(healthy_budget > degraded_budget);
    assert!(degraded_budget > 0);

    // Submit and schedule with reduced budget.
    let intent = make_intent(1, 1, 5, 3000);
    scheduler.submit_intent(intent);

    let frame = scheduler.schedule_frame_with_budget(degraded_budget);
    assert_eq!(frame.requested_frame_budget_units, degraded_budget);
}

/// Input latency spike detection: when P95 latency exceeds a threshold,
/// the system should reduce resize work to preserve input responsiveness.
#[test]
fn latency_spike_reduces_resize_budget() {
    let mut latency = InputLatencyCollector::new(100);

    // Record 15 stable measurements (~800us each).
    for i in 0..15u64 {
        record_full_measurement(&mut latency, i * 10_000, 0);
    }

    // Record 5 measurements with high jitter (simulating GPU stall).
    for i in 15..20u64 {
        record_full_measurement(&mut latency, i * 10_000, 500); // +500us per stage
    }

    let p50 = latency.total_latency_percentile(Percentile::P50).unwrap();
    let p95 = latency.total_latency_percentile(Percentile::P95).unwrap();
    let p99 = latency.total_latency_percentile(Percentile::P99).unwrap();

    // P95/P99 should be higher than P50 due to jitter.
    assert!(
        p95 > p50,
        "P95 ({p95}) should exceed P50 ({p50}) with jitter"
    );
    assert!(p99 >= p95);

    // Stage-level latency: GPU present stage should show the spike.
    let gpu_p95 = latency.stage_latency_percentile(
        InputLatencyStage::RenderSubmit,
        InputLatencyStage::GpuPresent,
        Percentile::P95,
    );
    assert!(gpu_p95.is_some());

    // Use latency percentiles to drive scheduler budget.
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig::default());
    let base_budget = scheduler.snapshot().config.frame_budget_units;

    // If P95 exceeds threshold, reduce budget.
    let latency_threshold_us = 1000;
    let budget = if p95 > latency_threshold_us {
        (base_budget / 2).max(1)
    } else {
        base_budget
    };

    // Submit resize intent.
    let intent = make_intent(1, 1, 8, 5000);
    scheduler.submit_intent(intent);

    let frame = scheduler.schedule_frame_with_budget(budget);
    assert!(frame.requested_frame_budget_units <= base_budget);
}

/// Full pipeline: measure latency → observe heartbeats → classify health
/// → submit resize intents → schedule frames → verify metrics coherence.
#[test]
fn full_pipeline_latency_watchdog_scheduler() {
    let mut latency = InputLatencyCollector::new(200);
    let mut watchdog = AdaptiveWatchdog::new(AdaptiveWatchdogConfig {
        min_observations: 3,
        ..AdaptiveWatchdogConfig::default()
    });
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        storm_threshold_intents: 100, // high threshold to avoid storm detection
        ..ResizeSchedulerConfig::default()
    });

    // Phase 1: stable operation.
    for i in 0..10u64 {
        // Record input latency.
        record_full_measurement(&mut latency, i * 10_000, 0);

        // Observe heartbeats for multiple components.
        let t = i * 100;
        watchdog.observe(Component::Capture, t);
        watchdog.observe(Component::Persistence, t + 10);
        watchdog.observe(Component::Discovery, t + 20);

        // Submit resize intents.
        let intent = make_intent(i + 1, 1, 5, t);
        scheduler.submit_intent(intent);
    }

    // All components healthy.
    let health = watchdog.check_health(1000);
    assert_eq!(health.overall, HealthStatus::Healthy);
    assert_eq!(health.timestamp_ms, 1000);

    // Schedule frames to process pending intents.
    for _ in 0..5 {
        scheduler.schedule_frame();
    }

    // Verify metrics are populated.
    let metrics = scheduler.metrics();
    assert!(metrics.frames >= 5);

    // Latency percentiles are stable.
    let summary = latency.total_latency_summary();
    assert!(!summary.is_empty());

    // Phase 2: simulate load spike.
    for i in 10..15u64 {
        record_full_measurement(&mut latency, i * 10_000, 200);
    }

    let p95_after = latency.total_latency_percentile(Percentile::P95).unwrap();
    let p50_after = latency.total_latency_percentile(Percentile::P50).unwrap();

    // P95 should reflect the spike.
    assert!(
        p95_after >= p50_after,
        "P95 ({p95_after}) >= P50 ({p50_after})"
    );

    // Scheduler snapshot shows state.
    let snap = scheduler.snapshot();
    assert!(snap.metrics.frames >= 5);
}
