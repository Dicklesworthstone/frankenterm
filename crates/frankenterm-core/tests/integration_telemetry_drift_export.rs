//! Integration test: telemetry registry → drift detection → metric export.
//!
//! Exercises the observability pipeline:
//!
//!   MetricRegistry.record_histogram(name, value)
//!     → DriftMonitor.observe(rule_id, rate) → DriftEvent
//!       → MetricPoint.new(name, value).with_tag(k, v) → export
//!
//! The MetricRegistry collects histograms and counters from all subsystems.
//! The DriftMonitor (ADWIN) watches detection rates for pattern rules and
//! fires DriftEvents when rates change significantly. MetricPoints tag
//! the drift events for structured export.

use frankenterm_core::drift::{DriftConfig, DriftMonitor, DriftType};
use frankenterm_core::telemetry::{
    Histogram, MetricPoint, MetricRegistry, TelemetryCollector, TelemetryConfig,
    TelemetrySnapshot,
};

// ── Helpers ─────────────────────────────────────────────────────────────

/// Record a batch of histogram values and return the registry's counter
/// for a given metric name.
fn record_batch(registry: &MetricRegistry, histogram: &str, values: &[f64]) {
    for &v in values {
        registry.record_histogram(histogram, v);
    }
}

/// Build MetricPoints from a drift event for structured export.
fn export_drift_metrics(
    rule_id: &str,
    old_mean: f64,
    new_mean: f64,
    drift_type: DriftType,
) -> Vec<MetricPoint> {
    let direction = match drift_type {
        DriftType::RateDrop => "drop",
        DriftType::RateSpike => "spike",
    };

    vec![
        MetricPoint::new("drift.old_mean", old_mean)
            .with_tag("rule", rule_id.to_string())
            .with_tag("direction", direction.to_string()),
        MetricPoint::new("drift.new_mean", new_mean)
            .with_tag("rule", rule_id.to_string())
            .with_tag("direction", direction.to_string()),
        MetricPoint::new("drift.delta", (new_mean - old_mean).abs())
            .with_tag("rule", rule_id.to_string()),
    ]
}

// ── Tests ───────────────────────────────────────────────────────────────

/// MetricRegistry records histograms, drift monitor detects rate changes,
/// and MetricPoints export the findings with tags.
#[test]
fn registry_histogram_feeds_drift_detection() {
    let registry = MetricRegistry::new();
    registry.register_histogram("rule_match_rate", 200);

    // Phase 1: stable rate (~5.0 matches per period).
    let stable_rates: Vec<f64> = (0..20).map(|_| 5.0).collect();
    record_batch(&registry, "rule_match_rate", &stable_rates);

    let mut drift = DriftMonitor::new(DriftConfig {
        enabled: true,
        confidence: 0.05, // more sensitive for testing
        min_window_size: 5,
        max_window_size: 500,
        min_mean_diff: 0.5,
    });

    // Feed stable observations to drift monitor.
    for &rate in &stable_rates {
        let event = drift.observe("error_pattern", rate);
        assert!(event.is_none(), "no drift expected during stable phase");
    }

    // Phase 2: rate spikes to ~15.0.
    let spike_rates: Vec<f64> = (0..20).map(|_| 15.0).collect();
    record_batch(&registry, "rule_match_rate", &spike_rates);

    let mut drift_detected = false;
    for &rate in &spike_rates {
        if let Some(event) = drift.observe("error_pattern", rate) {
            assert_eq!(event.drift_type, DriftType::RateSpike);
            assert!(event.info.new_mean > event.info.old_mean);
            drift_detected = true;
            break;
        }
    }
    assert!(drift_detected, "drift should be detected after rate spike");

    // Verify histogram captured all values.
    let summaries = registry.histogram_summaries();
    let hist = summaries.iter().find(|h| h.name == "rule_match_rate");
    assert!(hist.is_some());
    assert_eq!(hist.unwrap().count, 40); // 20 stable + 20 spike
}

/// DriftEvents are exported as tagged MetricPoints for structured telemetry.
#[test]
fn drift_events_become_tagged_metric_points() {
    let mut drift = DriftMonitor::new(DriftConfig {
        enabled: true,
        confidence: 0.05,
        min_window_size: 5,
        max_window_size: 500,
        min_mean_diff: 0.5,
    });

    drift.register_rule("compile_error");

    // Establish baseline.
    for _ in 0..20 {
        drift.observe("compile_error", 10.0);
    }

    // Cause a drop.
    let mut event_opt = None;
    for _ in 0..20 {
        if let Some(ev) = drift.observe("compile_error", 1.0) {
            event_opt = Some(ev);
            break;
        }
    }

    let event = event_opt.expect("drift event should fire on rate drop");
    assert_eq!(event.drift_type, DriftType::RateDrop);

    // Export as metric points.
    let points = export_drift_metrics(
        &event.rule_id,
        event.info.old_mean,
        event.info.new_mean,
        event.drift_type,
    );

    assert_eq!(points.len(), 3);
    assert_eq!(points[0].name, "drift.old_mean");
    assert_eq!(points[0].tags.get("rule").unwrap(), "compile_error");
    assert_eq!(points[0].tags.get("direction").unwrap(), "drop");
    assert_eq!(points[1].name, "drift.new_mean");
    assert!(points[1].value < points[0].value); // new < old for drop
    assert_eq!(points[2].name, "drift.delta");
    assert!(points[2].value > 0.0);
}

/// TelemetryCollector snapshot includes histograms and counters populated
/// by the registry, and drift summary remains coherent.
#[test]
fn collector_snapshot_coherent_with_drift_summary() {
    let collector = TelemetryCollector::new(TelemetryConfig::default());
    let registry = collector.registry();

    // Register histograms and counters.
    registry.register_histogram("capture_latency_us", 100);
    registry.register_histogram("render_latency_us", 100);

    // Record values.
    for i in 0..50 {
        registry.record_histogram("capture_latency_us", (i as f64).mul_add(2.0, 100.0));
        registry.record_histogram("render_latency_us", 50.0 + (i as f64));
        registry.increment_counter("frames_rendered");
    }

    // Take snapshot.
    let snap: TelemetrySnapshot = collector.snapshot();
    assert_eq!(snap.histograms.len(), 2);
    assert!(snap.counters.get("frames_rendered").copied().unwrap_or(0) == 50);

    // Drift monitor tracks the same rates.
    let mut drift = DriftMonitor::new(DriftConfig::default());
    for i in 0..50 {
        let rate = (i as f64).mul_add(2.0, 100.0);
        drift.observe("capture_latency", rate);
    }

    let summary = drift.summary();
    assert_eq!(summary.total_rules, 1);
    let rule = &summary.rules[0];
    assert_eq!(rule.total_observations, 50);

    // Drift telemetry observations match.
    let telem = drift.telemetry().snapshot();
    assert_eq!(telem.observations, 50);
}

/// Multiple drift rules track independently, and the summary reports
/// per-rule statistics coherently.
#[test]
fn multi_rule_drift_tracking_with_export() {
    let registry = MetricRegistry::new();
    registry.register_histogram("rule_a_rate", 100);
    registry.register_histogram("rule_b_rate", 100);

    let mut drift = DriftMonitor::new(DriftConfig {
        enabled: true,
        confidence: 0.05,
        min_window_size: 5,
        max_window_size: 500,
        min_mean_diff: 1.0,
    });

    // Rule A: stable at 5.0.
    for _ in 0..30 {
        registry.record_histogram("rule_a_rate", 5.0);
        drift.observe("rule_a", 5.0);
    }

    // Rule B: stable at 20.0, then drops to 2.0.
    for _ in 0..20 {
        registry.record_histogram("rule_b_rate", 20.0);
        drift.observe("rule_b", 20.0);
    }

    let mut rule_b_drifted = false;
    for _ in 0..20 {
        registry.record_histogram("rule_b_rate", 2.0);
        if let Some(ev) = drift.observe("rule_b", 2.0) {
            assert_eq!(ev.drift_type, DriftType::RateDrop);
            rule_b_drifted = true;

            // Export the event.
            let points = export_drift_metrics(
                &ev.rule_id,
                ev.info.old_mean,
                ev.info.new_mean,
                ev.drift_type,
            );
            assert_eq!(points[0].tags.get("rule").unwrap(), "rule_b");
            break;
        }
    }
    assert!(rule_b_drifted, "rule B should detect drift");

    // Rule A should have no drifts.
    let summary = drift.summary();
    assert_eq!(summary.total_rules, 2);
    let rule_a = summary.rules.iter().find(|r| r.rule_id == "rule_a").unwrap();
    assert_eq!(rule_a.total_drifts, 0);

    // Rule B should have at least one drift.
    let rule_b = summary.rules.iter().find(|r| r.rule_id == "rule_b").unwrap();
    assert!(rule_b.total_drifts >= 1);

    // Histogram summaries reflect the recorded data.
    let histograms = registry.histogram_summaries();
    assert_eq!(histograms.len(), 2);
}

/// Histogram p50/p95/p99 quantiles feed into drift detection thresholds,
/// and the pipeline produces coherent export artifacts.
#[test]
fn histogram_quantiles_drive_drift_thresholds() {
    let mut hist = Histogram::new("response_time_ms", 200);

    // Record 100 values: mostly fast (10-20ms).
    for i in 0..100 {
        hist.record(10.0 + (i % 10) as f64);
    }

    let p50 = hist.p50().unwrap();
    let p95 = hist.p95().unwrap();
    let p99 = hist.p99().unwrap();

    assert!((10.0..=20.0).contains(&p50));
    assert!(p95 >= p50);
    assert!(p99 >= p95);

    // Now add some outliers (simulating degradation).
    for _ in 0..20 {
        hist.record(500.0);
    }

    let p95_after = hist.p95().unwrap();
    assert!(
        p95_after > p95,
        "p95 should increase after outliers: before={p95}, after={p95_after}"
    );

    // Use the p95 shift as a drift signal.
    let mut drift = DriftMonitor::new(DriftConfig {
        enabled: true,
        confidence: 0.05,
        min_window_size: 5,
        max_window_size: 500,
        min_mean_diff: 5.0,
    });

    // Feed the pre-outlier p95 as stable baseline.
    for _ in 0..15 {
        drift.observe("response_time_p95", p95);
    }

    // Feed the post-outlier p95 — should trigger spike.
    let mut spike_detected = false;
    for _ in 0..15 {
        if let Some(ev) = drift.observe("response_time_p95", p95_after) {
            assert_eq!(ev.drift_type, DriftType::RateSpike);
            spike_detected = true;

            // Export.
            let points = export_drift_metrics(
                &ev.rule_id,
                ev.info.old_mean,
                ev.info.new_mean,
                ev.drift_type,
            );
            assert_eq!(points[0].tags.get("direction").unwrap(), "spike");
            break;
        }
    }
    assert!(spike_detected, "p95 spike should trigger drift detection");

    // Verify histogram summary.
    let summary = hist.summary();
    assert_eq!(summary.count, 120); // 100 + 20
    assert_eq!(summary.name, "response_time_ms");
}

/// Full pipeline: TelemetryCollector creates registry → register histograms →
/// record values → detect drift → export tagged MetricPoints → verify snapshot.
#[test]
fn full_pipeline_telemetry_drift_export() {
    // Set up telemetry collector.
    let collector = TelemetryCollector::new(TelemetryConfig {
        buffer_capacity: 50,
        histogram_buckets: 256,
        ..TelemetryConfig::default()
    });
    let registry = collector.registry();

    // Register metrics for two subsystems.
    registry.register_histogram("capture_rate", 100);
    registry.register_histogram("render_time_ms", 100);

    // Set up drift monitor.
    let mut drift = DriftMonitor::new(DriftConfig {
        enabled: true,
        confidence: 0.05,
        min_window_size: 5,
        max_window_size: 500,
        min_mean_diff: 2.0,
    });

    // Phase 1: stable operation.
    let mut all_exports: Vec<MetricPoint> = Vec::new();

    for _i in 0..25u64 {
        let capture_rate = 8.0;
        let render_time = 16.0;

        registry.record_histogram("capture_rate", capture_rate);
        registry.record_histogram("render_time_ms", render_time);
        registry.increment_counter("total_frames");

        // No drift expected with constant rates.
        assert!(drift.observe("capture", capture_rate).is_none());
        assert!(drift.observe("render", render_time).is_none());
    }

    // Snapshot should show 25 samples per histogram.
    let snap = collector.snapshot();
    assert_eq!(snap.histograms.len(), 2);
    for h in &snap.histograms {
        assert_eq!(h.count, 25);
    }
    assert_eq!(snap.counters.get("total_frames").copied().unwrap_or(0), 25);

    // Phase 2: capture rate degrades.
    for _ in 0..20 {
        let degraded_rate = 1.0;
        registry.record_histogram("capture_rate", degraded_rate);
        registry.increment_counter("total_frames");

        if let Some(ev) = drift.observe("capture", degraded_rate) {
            let points = export_drift_metrics(
                &ev.rule_id,
                ev.info.old_mean,
                ev.info.new_mean,
                ev.drift_type,
            );
            all_exports.extend(points);
        }
    }

    // Should have drift exports.
    assert!(
        !all_exports.is_empty(),
        "capture rate drop should produce export points"
    );
    assert!(all_exports.iter().any(|p| p.name == "drift.old_mean"));
    assert!(all_exports.iter().any(|p| p.name == "drift.delta"));

    // Final snapshot.
    let final_snap = collector.snapshot();
    let capture_hist = final_snap
        .histograms
        .iter()
        .find(|h| h.name == "capture_rate")
        .unwrap();
    assert_eq!(capture_hist.count, 45); // 25 + 20

    assert_eq!(
        final_snap
            .counters
            .get("total_frames")
            .copied()
            .unwrap_or(0),
        45
    );

    // Drift summary.
    let drift_summary = drift.summary();
    assert_eq!(drift_summary.total_rules, 2);
    let capture_rule = drift_summary
        .rules
        .iter()
        .find(|r| r.rule_id == "capture")
        .unwrap();
    assert!(capture_rule.total_drifts >= 1);

    // Telemetry counters.
    let telem = drift.telemetry().snapshot();
    assert!(telem.drifts_detected >= 1);
    assert!(telem.observations > 0);
}
