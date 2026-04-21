//! Integration test: backpressure → pane tiers → priority classification.
//!
//! Exercises the full cross-module flow that controls adaptive resource
//! allocation under load:
//!
//!   BackpressureManager.evaluate(queue_depths)
//!     → BackpressureTier { Green, Yellow, Red, Black }
//!       → PaneTier::effective_interval(bp)  (scaled polling)
//!         → PriorityClassifier.classify(pane_id)  (resource priority)
//!
//! This mirrors the real watcher loop: as queue depths rise, backpressure
//! escalates → polling intervals stretch → lower-priority panes shed first.

use std::time::{Duration, Instant};

use frankenterm_core::backpressure::{
    BackpressureConfig, BackpressureManager, BackpressureTier, QueueDepths,
};
use frankenterm_core::pane_tiers::{PaneTier, PaneTierClassifier, TierConfig};
use frankenterm_core::priority::{
    PanePriority, PriorityClassifier, PriorityConfig, PrioritySignal,
};

// ── Helpers ─────────────────────────────────────────────────────────────

/// Queue depths at the given fill ratios (capture and write).
fn depths_at(capture_ratio: f64, write_ratio: f64) -> QueueDepths {
    let cap = 1000;
    QueueDepths {
        capture_depth: (capture_ratio * cap as f64) as usize,
        capture_capacity: cap,
        write_depth: (write_ratio * cap as f64) as usize,
        write_capacity: cap,
    }
}

/// BackpressureConfig with zero hysteresis so transitions are instant.
fn instant_bp_config() -> BackpressureConfig {
    BackpressureConfig {
        hysteresis_ms: 0,
        ..BackpressureConfig::default()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

/// End-to-end: rising queue depths escalate backpressure, stretch intervals,
/// and shift priority classification.
#[test]
fn rising_load_escalates_through_all_three_modules() {
    // ── Module 1: Backpressure ──
    let bp = BackpressureManager::new(instant_bp_config());

    // Green at low utilisation.
    assert_eq!(bp.classify(&depths_at(0.10, 0.10)), BackpressureTier::Green);

    // Yellow when capture crosses 50%.
    assert_eq!(
        bp.classify(&depths_at(0.55, 0.10)),
        BackpressureTier::Yellow
    );

    // Red when capture crosses 75%.
    assert_eq!(bp.classify(&depths_at(0.80, 0.10)), BackpressureTier::Red);

    // Black when near saturation.
    assert_eq!(
        bp.classify(&depths_at(0.998, 0.10)),
        BackpressureTier::Black
    );

    // ── Module 2: Pane Tiers → effective intervals ──
    // Active pane's polling interval scales with backpressure.
    let active = PaneTier::Active;
    let base = active.default_interval(); // 200ms
    assert_eq!(base, Duration::from_millis(200));

    // Green: 1x → 200ms
    assert_eq!(active.effective_interval(BackpressureTier::Green), base);

    // Yellow: 1.5x → 300ms
    assert_eq!(
        active.effective_interval(BackpressureTier::Yellow),
        Duration::from_millis(300)
    );

    // Red: 3x → 600ms
    assert_eq!(
        active.effective_interval(BackpressureTier::Red),
        Duration::from_millis(600)
    );

    // Black: 10x → 2000ms
    assert_eq!(
        active.effective_interval(BackpressureTier::Black),
        Duration::from_secs(2)
    );

    // Dormant pane under Red is 3x × 30s = 90s.
    let dormant = PaneTier::Dormant;
    assert_eq!(
        dormant.effective_interval(BackpressureTier::Red),
        Duration::from_secs(90)
    );

    // ── Module 3: Priority Classifier ──
    let pc = PriorityClassifier::new(PriorityConfig::default());
    let pane_a = 1;
    let pane_b = 2;
    pc.register_pane(pane_a);
    pc.register_pane(pane_b);

    // Both start at Medium (default).
    assert_eq!(pc.classify(pane_a), PanePriority::Medium);
    assert_eq!(pc.classify(pane_b), PanePriority::Medium);

    // Feed an error signal to pane_a → should promote to Critical.
    let now = Instant::now();
    let error_signal = PrioritySignal {
        event_type: "error".to_string(),
        severity: 2,
        observed_at: now,
    };
    pc.observe_signal(pane_a, &error_signal);
    assert_eq!(pc.classify(pane_a), PanePriority::Critical);

    // pane_b is unaffected.
    assert_eq!(pc.classify(pane_b), PanePriority::Medium);
}

/// Verify that the tier classifier correctly reclassifies panes based on
/// activity signals, and those tiers flow through to priority.
#[test]
fn tier_classifier_output_flows_into_priority() {
    let tier_classifier = PaneTierClassifier::new(TierConfig::default());
    let priority_classifier = PriorityClassifier::new(PriorityConfig::default());

    let pane_id = 42;
    tier_classifier.register_pane(pane_id);
    priority_classifier.register_pane(pane_id);

    // Fresh pane starts Active.
    let tier = tier_classifier.classify(pane_id);
    assert_eq!(tier, PaneTier::Active);

    // Feed that tier into priority.
    priority_classifier.update_tier(pane_id, tier);
    let priority = priority_classifier.classify(pane_id);
    // Active pane with no output → Medium (default).
    assert_eq!(priority, PanePriority::Medium);

    // Mark pane as rate-limited → classifier should eventually downgrade
    // to Dormant when reclassified.
    tier_classifier.set_rate_limited(pane_id, true);
    let tier = tier_classifier.classify(pane_id);
    assert_eq!(tier, PaneTier::Dormant);

    // Feed the Dormant tier into priority.
    priority_classifier.update_tier(pane_id, PaneTier::Dormant);

    // Also send rate_limited signal.
    let signal = PrioritySignal {
        event_type: "rate_limited".to_string(),
        severity: 1,
        observed_at: Instant::now(),
    };
    priority_classifier.observe_signal(pane_id, &signal);

    let priority = priority_classifier.classify(pane_id);
    // Rate-limited dormant pane → Background (lowest).
    assert_eq!(priority, PanePriority::Background);
}

/// Backpressure manager pause/resume integrates with tier classification.
#[test]
fn backpressure_pause_resume_affects_tier_and_priority() {
    let bp = BackpressureManager::new(instant_bp_config());
    let tier_classifier = PaneTierClassifier::new(TierConfig::default());
    let priority_classifier = PriorityClassifier::new(PriorityConfig::default());

    let pane_ids: Vec<u64> = (1..=5).collect();
    for &id in &pane_ids {
        tier_classifier.register_pane(id);
        priority_classifier.register_pane(id);
    }

    // Simulate Red backpressure.
    let transition = bp.evaluate(&depths_at(0.80, 0.10));
    assert!(transition.is_some());
    let (old, new) = transition.unwrap();
    assert_eq!(old, BackpressureTier::Green);
    assert_eq!(new, BackpressureTier::Red);

    // Under Red, pause the lowest-priority panes (panes 4 & 5).
    bp.pause_pane(4);
    bp.pause_pane(5);
    assert_eq!(bp.paused_pane_ids().len(), 2);
    assert!(bp.is_pane_paused(4));
    assert!(bp.is_pane_paused(5));
    assert!(!bp.is_pane_paused(1));

    // Paused panes should be rate-limited → Dormant tier.
    tier_classifier.set_rate_limited(4, true);
    tier_classifier.set_rate_limited(5, true);
    assert_eq!(tier_classifier.classify(4), PaneTier::Dormant);
    assert_eq!(tier_classifier.classify(5), PaneTier::Dormant);

    // Non-paused panes stay Active.
    assert_eq!(tier_classifier.classify(1), PaneTier::Active);

    // Dormant polling at Red: 30s × 3 = 90s.
    let dormant_red = PaneTier::Dormant.effective_interval(BackpressureTier::Red);
    assert_eq!(dormant_red, Duration::from_secs(90));

    // Active polling at Red: 200ms × 3 = 600ms.
    let active_red = PaneTier::Active.effective_interval(BackpressureTier::Red);
    assert_eq!(active_red, Duration::from_millis(600));

    // Update priority with new tier info.
    priority_classifier.update_tier(4, PaneTier::Dormant);
    priority_classifier.update_tier(5, PaneTier::Dormant);

    // Feed rate_limited signal.
    let signal = PrioritySignal {
        event_type: "rate_limited".to_string(),
        severity: 1,
        observed_at: Instant::now(),
    };
    priority_classifier.observe_signal(4, &signal);
    priority_classifier.observe_signal(5, &signal);

    // Paused panes should be Background priority.
    assert_eq!(priority_classifier.classify(4), PanePriority::Background);
    assert_eq!(priority_classifier.classify(5), PanePriority::Background);

    // Non-paused pane remains Medium.
    assert_eq!(priority_classifier.classify(1), PanePriority::Medium);

    // ── Recovery: queues drain → Green ──
    // Need to wait past hysteresis (0ms in our config).
    let transition = bp.evaluate(&depths_at(0.10, 0.10));
    assert!(transition.is_some());
    let (old, new) = transition.unwrap();
    assert_eq!(old, BackpressureTier::Red);
    assert_eq!(new, BackpressureTier::Green);

    // Resume paused panes.
    bp.resume_pane(4);
    bp.resume_pane(5);
    assert_eq!(bp.paused_pane_ids().len(), 0);

    // Re-activate.
    tier_classifier.set_rate_limited(4, false);
    tier_classifier.set_rate_limited(5, false);
    tier_classifier.on_pane_output(4);
    tier_classifier.on_pane_output(5);
    assert_eq!(tier_classifier.classify(4), PaneTier::Active);
    assert_eq!(tier_classifier.classify(5), PaneTier::Active);
}

/// Output rate from priority classifier integrates with tier and
/// backpressure to produce a coherent resource allocation picture.
#[test]
fn high_output_rate_elevates_priority_while_backpressure_stretches_interval() {
    let bp = BackpressureManager::new(instant_bp_config());
    let priority_classifier = PriorityClassifier::new(PriorityConfig {
        high_rate_threshold: 10.0,
        medium_rate_threshold: 1.0,
        ..PriorityConfig::default()
    });

    let pane_id = 99;
    priority_classifier.register_pane(pane_id);

    // Simulate high output rate: 50 lines every 100ms = 500 lines/sec.
    let start = Instant::now();
    for i in 0..10 {
        let t = start + Duration::from_millis(100 * (i + 1));
        priority_classifier.record_output_at(pane_id, 50, t);
    }

    // Classify after output burst.
    let now = start + Duration::from_millis(1100);
    let priority = priority_classifier.classify_at(pane_id, now);
    assert_eq!(
        priority,
        PanePriority::High,
        "high output rate should produce High priority"
    );

    // Meanwhile, system is under Yellow backpressure.
    bp.evaluate(&depths_at(0.55, 0.10));
    assert_eq!(bp.current_tier(), BackpressureTier::Yellow);

    // High-priority pane's interval at Yellow: 200ms × 1.5 = 300ms.
    // This is the key integration: priority says "important" but
    // backpressure says "slow down".
    let interval = PaneTier::Active.effective_interval(BackpressureTier::Yellow);
    assert_eq!(interval, Duration::from_millis(300));

    // Verify the priority system recorded output rate.
    let rate = priority_classifier.output_rate_at(pane_id, now);
    assert!(
        rate > 10.0,
        "output rate should exceed high_rate_threshold; got {rate}"
    );
}

/// Telemetry from all three modules is coherent after the integration flow.
#[test]
fn telemetry_snapshots_are_coherent_after_flow() {
    let bp = BackpressureManager::new(instant_bp_config());
    let priority_classifier = PriorityClassifier::new(PriorityConfig::default());

    // Register panes.
    for id in 1..=3 {
        priority_classifier.register_pane(id);
    }

    // Drive some evaluations.
    bp.evaluate(&depths_at(0.10, 0.10)); // stays Green (no transition)
    bp.evaluate(&depths_at(0.55, 0.10)); // → Yellow
    bp.evaluate(&depths_at(0.80, 0.10)); // → Red

    let bp_telem = bp.telemetry().snapshot();
    assert_eq!(bp_telem.evaluations, 3);
    assert_eq!(bp_telem.transitions, 2); // Green→Yellow, Yellow→Red

    // Drive some classifications.
    for id in 1..=3 {
        priority_classifier.classify(id);
    }

    let pc_metrics = priority_classifier.metrics();
    assert_eq!(pc_metrics.tracked_panes, 3);
    // 3 registrations + 3 classifies = at least that many operations.
    assert!(pc_metrics.total_classifications >= 3);

    // Drive a signal.
    let signal = PrioritySignal {
        event_type: "error".to_string(),
        severity: 2,
        observed_at: Instant::now(),
    };
    priority_classifier.observe_signal(1, &signal);

    // Reclassify: pane 1 should be Critical now.
    let priorities: Vec<_> = (1..=3).map(|id| priority_classifier.classify(id)).collect();
    assert_eq!(priorities[0], PanePriority::Critical);
    assert_eq!(priorities[1], PanePriority::Medium);
    assert_eq!(priorities[2], PanePriority::Medium);
}

/// Manual overrides in priority bypass the tier → priority flow.
#[test]
fn manual_override_bypasses_automatic_classification() {
    let priority_classifier = PriorityClassifier::new(PriorityConfig::default());
    let pane_id = 7;
    priority_classifier.register_pane(pane_id);

    // Force to Background regardless of other signals.
    priority_classifier.set_override(pane_id, PanePriority::Background);
    assert_eq!(
        priority_classifier.classify(pane_id),
        PanePriority::Background
    );

    // Even with an error signal, override holds.
    priority_classifier.observe_signal(
        pane_id,
        &PrioritySignal {
            event_type: "error".to_string(),
            severity: 2,
            observed_at: Instant::now(),
        },
    );
    assert_eq!(
        priority_classifier.classify(pane_id),
        PanePriority::Background
    );
    assert!(priority_classifier.has_override(pane_id));

    // Clear override → automatic classification kicks in → Critical from
    // the error signal we just delivered.
    priority_classifier.clear_override(pane_id);
    assert!(!priority_classifier.has_override(pane_id));
    assert_eq!(
        priority_classifier.classify(pane_id),
        PanePriority::Critical
    );
}

/// All tiers' effective intervals form a monotonically increasing sequence
/// at every backpressure level.
#[test]
fn effective_intervals_are_monotonic_across_tiers_and_bp_levels() {
    let tiers = PaneTier::all();
    let bp_levels = [
        BackpressureTier::Green,
        BackpressureTier::Yellow,
        BackpressureTier::Red,
        BackpressureTier::Black,
    ];

    for bp in &bp_levels {
        let intervals: Vec<Duration> = tiers.iter().map(|t| t.effective_interval(*bp)).collect();

        for window in intervals.windows(2) {
            assert!(
                window[0] <= window[1],
                "intervals should be non-decreasing across tiers at {bp:?}: {:?}",
                intervals
            );
        }
    }

    // Also verify that for any given tier, higher bp → longer interval.
    for tier in tiers {
        let intervals: Vec<Duration> = bp_levels
            .iter()
            .map(|bp| tier.effective_interval(*bp))
            .collect();

        for window in intervals.windows(2) {
            assert!(
                window[0] <= window[1],
                "intervals should be non-decreasing across bp levels for {tier:?}: {:?}",
                intervals
            );
        }
    }
}
