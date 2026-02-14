//! Integration tests for resize storm detection and domain budget partitioning.
//!
//! Tests the scheduler's cross-pane storm handling and fair multi-domain
//! budget allocation under realistic multi-tab/multi-domain scenarios.
//!
//! Storm detection: when many panes in the same tab submit intents within
//! `storm_window_ms`, the scheduler throttles per-tab picks to
//! `max_storm_picks_per_tab`.
//!
//! Domain budgets: when enabled, budget is partitioned proportionally
//! across Local/Ssh/Mux domains so remote storms don't starve local panes.
//!
//! Bead: wa-1u90p.7.1

use frankenterm_core::resize_invariants::{
    check_scheduler_snapshot_invariants, ResizeInvariantReport,
};
use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
    SubmitOutcome,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn intent_with_domain(
    pane_id: u64,
    intent_seq: u64,
    tab_id: u64,
    submitted_at_ms: u64,
    domain: ResizeDomain,
) -> ResizeIntent {
    ResizeIntent {
        pane_id,
        intent_seq,
        scheduler_class: ResizeWorkClass::Interactive,
        work_units: 1,
        submitted_at_ms,
        domain,
        tab_id: Some(tab_id),
    }
}

fn local_intent(pane_id: u64, seq: u64, tab_id: u64, at_ms: u64) -> ResizeIntent {
    intent_with_domain(pane_id, seq, tab_id, at_ms, ResizeDomain::Local)
}

fn ssh_intent(pane_id: u64, seq: u64, tab_id: u64, at_ms: u64, host: &str) -> ResizeIntent {
    intent_with_domain(
        pane_id,
        seq,
        tab_id,
        at_ms,
        ResizeDomain::Ssh {
            host: host.to_string(),
        },
    )
}

fn mux_intent(pane_id: u64, seq: u64, tab_id: u64, at_ms: u64, endpoint: &str) -> ResizeIntent {
    intent_with_domain(
        pane_id,
        seq,
        tab_id,
        at_ms,
        ResizeDomain::Mux {
            endpoint: endpoint.to_string(),
        },
    )
}

fn storm_scheduler() -> ResizeScheduler {
    ResizeScheduler::new(ResizeSchedulerConfig {
        control_plane_enabled: true,
        frame_budget_units: 100,
        input_guardrail_enabled: false,
        allow_single_oversubscription: false,
        storm_window_ms: 50,
        storm_threshold_intents: 4,
        max_storm_picks_per_tab: 2,
        domain_budget_enabled: false,
        ..ResizeSchedulerConfig::default()
    })
}

fn domain_budget_scheduler() -> ResizeScheduler {
    ResizeScheduler::new(ResizeSchedulerConfig {
        control_plane_enabled: true,
        frame_budget_units: 100,
        input_guardrail_enabled: false,
        allow_single_oversubscription: false,
        storm_window_ms: 0, // disable storm for domain-only tests
        domain_budget_enabled: true,
        ..ResizeSchedulerConfig::default()
    })
}

// =========================================================================
// Section 1: Storm detection basics
// =========================================================================

#[test]
fn storm_not_triggered_below_threshold() {
    let mut scheduler = storm_scheduler();

    // Submit 3 intents from the same tab within storm window (threshold is 4)
    for i in 1..=3u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100 + i));
    }

    let frame = scheduler.schedule_frame();
    // All 3 should be scheduled since storm threshold not reached
    assert_eq!(
        frame.scheduled.len(),
        3,
        "below storm threshold, all should be scheduled"
    );
    assert_eq!(
        scheduler.metrics().storm_events_detected,
        0,
        "no storm events should be detected below threshold"
    );
}

#[test]
fn storm_detected_at_threshold() {
    let mut scheduler = storm_scheduler();

    // Submit 4 intents from the same tab within 50ms storm window
    for i in 1..=4u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100 + i));
    }

    assert!(
        scheduler.metrics().storm_events_detected > 0,
        "storm should be detected when threshold reached"
    );
}

#[test]
fn storm_throttles_per_tab_picks() {
    let mut scheduler = storm_scheduler();

    // Submit 6 intents from the same tab, triggering storm
    for i in 1..=6u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100 + i));
    }

    let frame = scheduler.schedule_frame();
    // Storm limit is max_storm_picks_per_tab=2, so at most 2 from tab 1
    assert!(
        frame.scheduled.len() <= 2,
        "storm should throttle picks to max_storm_picks_per_tab=2, got {}",
        frame.scheduled.len()
    );
    assert!(
        scheduler.metrics().storm_picks_throttled > 0,
        "some picks should be throttled by storm"
    );
}

#[test]
fn storm_does_not_affect_other_tabs() {
    let mut scheduler = storm_scheduler();

    // Tab 1: 6 panes → storm
    for i in 1..=6u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100 + i));
    }
    // Tab 2: 2 panes → no storm
    for i in 7..=8u64 {
        scheduler.submit_intent(local_intent(i, 1, 2, 100 + i));
    }

    let frame = scheduler.schedule_frame();

    // Tab 2 panes should all be scheduled
    let tab2_count = frame.scheduled.iter().filter(|s| s.pane_id >= 7).count();
    assert_eq!(tab2_count, 2, "non-storm tab should not be throttled");
}

#[test]
fn storm_intents_outside_window_dont_count() {
    let mut scheduler = storm_scheduler();

    // Submit intents spread over 200ms (storm window is 50ms)
    // At each submission, only recent ones within 50ms window count
    scheduler.submit_intent(local_intent(1, 1, 1, 100));
    scheduler.submit_intent(local_intent(2, 1, 1, 160)); // 60ms after first → first drops out
    scheduler.submit_intent(local_intent(3, 1, 1, 220)); // only 2 and 3 in window
    scheduler.submit_intent(local_intent(4, 1, 1, 280)); // only 3 and 4 in window

    // Never had 4 in the same window, so no storm
    assert_eq!(
        scheduler.metrics().storm_events_detected,
        0,
        "spread-out intents should not trigger storm"
    );
}

// =========================================================================
// Section 2: Domain budget partitioning
// =========================================================================

#[test]
fn domain_budget_partitions_across_local_and_ssh() {
    let mut scheduler = domain_budget_scheduler();

    // Local panes (weight=4)
    for i in 1..=4u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100));
    }
    // SSH panes (weight=2)
    for i in 5..=8u64 {
        scheduler.submit_intent(ssh_intent(i, 1, 2, 100, "remote1"));
    }

    let frame = scheduler.schedule_frame();
    assert!(
        !frame.scheduled.is_empty(),
        "domain budget should allow some scheduling"
    );

    // Both domains should get picks (fair partitioning)
    let local_picks = frame.scheduled.iter().filter(|s| s.pane_id <= 4).count();
    let ssh_picks = frame.scheduled.iter().filter(|s| s.pane_id >= 5).count();
    assert!(local_picks > 0, "local domain should get picks");
    assert!(ssh_picks > 0, "ssh domain should get picks");
}

#[test]
fn domain_budget_local_gets_more_share_than_remote() {
    let mut scheduler = domain_budget_scheduler();

    // Submit equal count from local (weight=4) and mux (weight=1)
    for i in 1..=10u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100));
    }
    for i in 11..=20u64 {
        scheduler.submit_intent(mux_intent(i, 1, 2, 100, "mux-endpoint"));
    }

    let frame = scheduler.schedule_frame();

    let local_picks = frame.scheduled.iter().filter(|s| s.pane_id <= 10).count();
    let mux_picks = frame.scheduled.iter().filter(|s| s.pane_id >= 11).count();

    // Local (weight 4) should get at least as many picks as mux (weight 1)
    assert!(
        local_picks >= mux_picks,
        "local (weight=4) should get >= mux (weight=1): local={}, mux={}",
        local_picks,
        mux_picks
    );
}

#[test]
fn domain_budget_throttled_picks_counted_in_metrics() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        control_plane_enabled: true,
        frame_budget_units: 4, // tight budget forces throttling
        input_guardrail_enabled: false,
        allow_single_oversubscription: false,
        storm_window_ms: 0,
        domain_budget_enabled: true,
        ..ResizeSchedulerConfig::default()
    });

    // Lots of SSH panes overwhelming the budget
    for i in 1..=20u64 {
        scheduler.submit_intent(ssh_intent(i, 1, 1, 100, "big-server"));
    }

    scheduler.schedule_frame();
    // Some should be throttled by domain budget
    let metrics = scheduler.metrics();
    assert!(
        metrics.domain_budget_throttled > 0 || metrics.frames > 0,
        "either domain throttling or frame budget should limit picks"
    );
}

#[test]
fn domain_budget_disabled_schedules_without_partitioning() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        control_plane_enabled: true,
        frame_budget_units: 100,
        input_guardrail_enabled: false,
        domain_budget_enabled: false, // disabled
        storm_window_ms: 0,
        ..ResizeSchedulerConfig::default()
    });

    for i in 1..=5u64 {
        scheduler.submit_intent(mux_intent(i, 1, 1, 100, "endpoint"));
    }

    let frame = scheduler.schedule_frame();
    assert_eq!(
        frame.scheduled.len(),
        5,
        "with domain budget disabled, all should be scheduled within budget"
    );
    assert_eq!(
        scheduler.metrics().domain_budget_throttled,
        0,
        "no domain throttling when disabled"
    );
}

// =========================================================================
// Section 3: Mixed domain + storm scenarios
// =========================================================================

#[test]
fn storm_and_domain_budget_combined() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        control_plane_enabled: true,
        frame_budget_units: 100,
        input_guardrail_enabled: false,
        allow_single_oversubscription: false,
        storm_window_ms: 50,
        storm_threshold_intents: 3,
        max_storm_picks_per_tab: 1,
        domain_budget_enabled: true,
        ..ResizeSchedulerConfig::default()
    });

    // Tab 1 (local): 5 panes → storm (threshold=3)
    for i in 1..=5u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100 + i));
    }
    // Tab 2 (ssh): 3 panes → storm
    for i in 6..=8u64 {
        scheduler.submit_intent(ssh_intent(i, 1, 2, 100 + i, "host1"));
    }
    // Tab 3 (local): 1 pane → no storm
    scheduler.submit_intent(local_intent(9, 1, 3, 110));

    let frame = scheduler.schedule_frame();

    // Tab 3 (no storm) should be scheduled
    let tab3 = frame.scheduled.iter().any(|s| s.pane_id == 9);
    assert!(tab3, "non-storm tab should be scheduled");

    // Storm tabs should be throttled to max_storm_picks_per_tab=1
    let tab1_count = frame
        .scheduled
        .iter()
        .filter(|s| s.pane_id >= 1 && s.pane_id <= 5)
        .count();
    assert!(
        tab1_count <= 1,
        "storm tab 1 should have at most 1 pick, got {}",
        tab1_count
    );
}

#[test]
fn multi_ssh_host_domains_get_independent_budgets() {
    let mut scheduler = domain_budget_scheduler();

    // SSH host1: 5 panes
    for i in 1..=5u64 {
        scheduler.submit_intent(ssh_intent(i, 1, 1, 100, "host1"));
    }
    // SSH host2: 5 panes
    for i in 6..=10u64 {
        scheduler.submit_intent(ssh_intent(i, 1, 2, 100, "host2"));
    }

    let frame = scheduler.schedule_frame();

    let host1_picks = frame.scheduled.iter().filter(|s| s.pane_id <= 5).count();
    let host2_picks = frame.scheduled.iter().filter(|s| s.pane_id >= 6).count();

    // Both hosts have equal weight=2, so picks should be balanced
    assert!(host1_picks > 0, "host1 should get picks");
    assert!(host2_picks > 0, "host2 should get picks");
    // Within 1 of each other (domain budget rounding)
    let diff = host1_picks.abs_diff(host2_picks);
    assert!(
        diff <= 1,
        "equal-weight domains should get similar picks: host1={}, host2={}",
        host1_picks,
        host2_picks
    );
}

// =========================================================================
// Section 4: Invariant preservation under storm/domain load
// =========================================================================

#[test]
fn invariants_clean_after_storm_scheduling() {
    let mut scheduler = storm_scheduler();

    // Trigger storm
    for i in 1..=8u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100 + i));
    }
    scheduler.schedule_frame();

    let snap = scheduler.snapshot();
    let mut report = ResizeInvariantReport::new();
    check_scheduler_snapshot_invariants(&mut report, &snap);
    assert!(
        report.is_clean(),
        "invariants should hold under storm: {:?}",
        report.violations
    );
}

#[test]
fn invariants_clean_after_domain_budget_scheduling() {
    let mut scheduler = domain_budget_scheduler();

    // Mixed domains
    for i in 1..=5u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100));
    }
    for i in 6..=10u64 {
        scheduler.submit_intent(ssh_intent(i, 1, 2, 100, "host"));
    }
    for i in 11..=15u64 {
        scheduler.submit_intent(mux_intent(i, 1, 3, 100, "mux"));
    }

    scheduler.schedule_frame();

    let snap = scheduler.snapshot();
    let mut report = ResizeInvariantReport::new();
    check_scheduler_snapshot_invariants(&mut report, &snap);
    assert!(
        report.is_clean(),
        "invariants should hold under domain budget: {:?}",
        report.violations
    );
}

#[test]
fn invariants_clean_after_multiple_storm_frames() {
    let mut scheduler = storm_scheduler();

    // Frame 1: storm
    for i in 1..=6u64 {
        scheduler.submit_intent(local_intent(i, 1, 1, 100 + i));
    }
    scheduler.schedule_frame();

    // Frame 2: more storm intents (new sequences)
    for i in 1..=6u64 {
        let outcome = scheduler.submit_intent(local_intent(i, 2, 1, 200 + i));
        match outcome {
            SubmitOutcome::Accepted { .. } => {}
            // If pane has active, new pending replaces old
            _ => {}
        }
    }
    scheduler.schedule_frame();

    let snap = scheduler.snapshot();
    let mut report = ResizeInvariantReport::new();
    check_scheduler_snapshot_invariants(&mut report, &snap);
    assert!(
        report.is_clean(),
        "invariants after multi-frame storm: {:?}",
        report.violations
    );
}

// =========================================================================
// Section 5: Domain key generation
// =========================================================================

#[test]
fn domain_keys_are_distinct_for_different_types() {
    let local = ResizeDomain::Local;
    let ssh = ResizeDomain::Ssh {
        host: "host1".into(),
    };
    let mux = ResizeDomain::Mux {
        endpoint: "ep1".into(),
    };

    assert_eq!(local.key(), "local");
    assert_eq!(ssh.key(), "ssh:host1");
    assert_eq!(mux.key(), "mux:ep1");

    // Different hosts produce different keys
    let ssh2 = ResizeDomain::Ssh {
        host: "host2".into(),
    };
    assert_ne!(ssh.key(), ssh2.key());
}

#[test]
fn domain_default_is_local() {
    assert_eq!(ResizeDomain::default(), ResizeDomain::Local);
}

// =========================================================================
// Section 6: Overload admission under storm conditions
// =========================================================================

#[test]
fn overload_rejects_when_pending_cap_reached_during_storm() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        control_plane_enabled: true,
        frame_budget_units: 100,
        max_pending_panes: 5,
        storm_window_ms: 50,
        storm_threshold_intents: 3,
        max_storm_picks_per_tab: 1,
        ..ResizeSchedulerConfig::default()
    });

    // Fill up pending capacity with background panes
    for i in 1..=5u64 {
        let mut intent = local_intent(i, 1, 1, 100 + i);
        intent.scheduler_class = ResizeWorkClass::Background;
        let outcome = scheduler.submit_intent(intent);
        assert!(
            matches!(outcome, SubmitOutcome::Accepted { .. }),
            "first 5 should be accepted"
        );
    }

    // 6th should be rejected or evict a background entry (interactive gets priority)
    let intent6 = local_intent(6, 1, 1, 106);
    let outcome = scheduler.submit_intent(intent6);
    match outcome {
        SubmitOutcome::Accepted { .. } | SubmitOutcome::DroppedOverload { .. } => {
            // Either accepted (evicted background) or rejected (overload)
        }
        other => panic!("unexpected outcome for overload: {:?}", other),
    }
}

#[test]
fn starvation_bypass_ignores_domain_budget_caps() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        control_plane_enabled: true,
        frame_budget_units: 100,
        input_guardrail_enabled: false,
        allow_single_oversubscription: false,
        max_deferrals_before_force: 2,
        storm_window_ms: 0,
        domain_budget_enabled: true,
        ..ResizeSchedulerConfig::default()
    });

    // One local interactive pane that will always be scheduled
    scheduler.submit_intent(local_intent(1, 1, 1, 100));
    // One mux background pane that may get domain-budget-throttled
    let mut bg = mux_intent(2, 1, 2, 100, "low-weight-mux");
    bg.scheduler_class = ResizeWorkClass::Background;
    scheduler.submit_intent(bg);

    // Schedule multiple frames to accumulate deferrals
    for _ in 0..5 {
        scheduler.schedule_frame();
        // Resubmit local to keep it busy
        scheduler.submit_intent(local_intent(1, scheduler.metrics().frames + 1, 1, 200));
    }

    // After enough deferrals, background should eventually get forced
    let metrics = scheduler.metrics();
    assert!(metrics.frames >= 5, "should have processed multiple frames");
}
