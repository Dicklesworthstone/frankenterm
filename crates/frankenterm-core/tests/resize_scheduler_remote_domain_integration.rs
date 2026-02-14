use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
    SubmitOutcome,
};

fn intent_with_domain_and_tab(
    pane_id: u64,
    intent_seq: u64,
    scheduler_class: ResizeWorkClass,
    work_units: u32,
    submitted_at_ms: u64,
    domain: ResizeDomain,
    tab_id: u64,
) -> ResizeIntent {
    ResizeIntent {
        pane_id,
        intent_seq,
        scheduler_class,
        work_units,
        submitted_at_ms,
        domain,
        tab_id: Some(tab_id),
    }
}

#[test]
fn remote_jitter_burst_obeys_storm_and_domain_throttles() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 8,
        domain_budget_enabled: true,
        storm_window_ms: 30,
        storm_threshold_intents: 3,
        max_storm_picks_per_tab: 1,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    // Local panes should keep service even when remote tabs burst with jitter.
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        1,
        1,
        ResizeWorkClass::Interactive,
        2,
        100,
        ResizeDomain::Local,
        1,
    ));
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        2,
        1,
        ResizeWorkClass::Interactive,
        2,
        103,
        ResizeDomain::Local,
        1,
    ));

    // SSH burst (same tab, jittered arrival times) should trigger storm throttling.
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        10,
        1,
        ResizeWorkClass::Interactive,
        2,
        104,
        ResizeDomain::Ssh {
            host: "edge-a".into(),
        },
        7,
    ));
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        11,
        1,
        ResizeWorkClass::Interactive,
        2,
        109,
        ResizeDomain::Ssh {
            host: "edge-a".into(),
        },
        7,
    ));
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        12,
        1,
        ResizeWorkClass::Interactive,
        2,
        113,
        ResizeDomain::Ssh {
            host: "edge-a".into(),
        },
        7,
    ));

    // Mux burst in a separate remote tab/domain.
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        20,
        1,
        ResizeWorkClass::Interactive,
        2,
        105,
        ResizeDomain::Mux {
            endpoint: "mux-east".into(),
        },
        8,
    ));
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        21,
        1,
        ResizeWorkClass::Interactive,
        2,
        111,
        ResizeDomain::Mux {
            endpoint: "mux-east".into(),
        },
        8,
    ));

    let frame = scheduler.schedule_frame();

    let local_picks = frame
        .scheduled
        .iter()
        .filter(|item| matches!(item.pane_id, 1 | 2))
        .count();
    let ssh_picks = frame
        .scheduled
        .iter()
        .filter(|item| matches!(item.pane_id, 10..=12))
        .count();
    let mux_picks = frame
        .scheduled
        .iter()
        .filter(|item| matches!(item.pane_id, 20 | 21))
        .count();

    assert_eq!(
        local_picks, 2,
        "local panes should retain their budget share"
    );
    assert_eq!(ssh_picks, 1, "stormed SSH tab should be capped to one pick");
    assert_eq!(mux_picks, 0, "mux domain should be throttled this frame");
    assert!(scheduler.metrics().storm_events_detected > 0);
    assert!(scheduler.metrics().storm_picks_throttled > 0);
    assert!(scheduler.metrics().domain_budget_throttled > 0);

    let debug = scheduler.debug_snapshot(16);
    let mux_row = debug
        .scheduler
        .panes
        .iter()
        .find(|row| row.pane_id == 20)
        .expect("mux pane should remain tracked");
    assert_eq!(mux_row.pending_seq, Some(1));
    assert!(mux_row.deferrals >= 1);
}

#[test]
fn remote_starvation_force_bypasses_domain_caps_under_sustained_local_pressure() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 4,
        domain_budget_enabled: true,
        max_deferrals_before_force: 1,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    // Two remote background panes on different domains.
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        50,
        1,
        ResizeWorkClass::Background,
        2,
        100,
        ResizeDomain::Ssh {
            host: "slow-ssh".into(),
        },
        50,
    ));
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        60,
        1,
        ResizeWorkClass::Background,
        2,
        107,
        ResizeDomain::Mux {
            endpoint: "slow-mux".into(),
        },
        60,
    ));

    // Local interactive pressure keeps first frame focused on local work.
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        1,
        1,
        ResizeWorkClass::Interactive,
        2,
        109,
        ResizeDomain::Local,
        1,
    ));

    let frame1 = scheduler.schedule_frame();
    assert_eq!(frame1.scheduled.len(), 1);
    assert_eq!(frame1.scheduled[0].pane_id, 1);
    assert!(scheduler.complete_active(1, 1));

    // Reintroduce local pressure with jittered timing.
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        1,
        2,
        ResizeWorkClass::Interactive,
        2,
        141,
        ResizeDomain::Local,
        1,
    ));

    let frame2 = scheduler.schedule_frame();
    let remote_forced_count = frame2
        .scheduled
        .iter()
        .filter(|work| matches!(work.pane_id, 50 | 60) && work.forced_by_starvation)
        .count();

    assert_eq!(
        remote_forced_count, 2,
        "both remote panes should be force-served after starvation"
    );
    assert_eq!(scheduler.metrics().forced_background_runs, 2);

    assert!(scheduler.complete_active(50, 1));
    assert!(scheduler.complete_active(60, 1));

    // Local work should still remain pending and eventually run (graceful degradation).
    let frame3 = scheduler.schedule_frame();
    assert!(
        frame3.scheduled.iter().any(|work| work.pane_id == 1),
        "local pane should recover after forced remote service"
    );
}

#[test]
fn emergency_disable_returns_legacy_fallback_hint_for_remote_submissions() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        emergency_disable: true,
        legacy_fallback_enabled: true,
        ..ResizeSchedulerConfig::default()
    });

    let outcome = scheduler.submit_intent(intent_with_domain_and_tab(
        90,
        1,
        ResizeWorkClass::Interactive,
        1,
        200,
        ResizeDomain::Mux {
            endpoint: "degraded-path".into(),
        },
        90,
    ));

    assert!(matches!(
        outcome,
        SubmitOutcome::SuppressedByKillSwitch {
            legacy_fallback: true
        }
    ));

    let frame = scheduler.schedule_frame();
    assert!(frame.scheduled.is_empty());

    let debug = scheduler.debug_snapshot(8);
    assert!(!debug.gate.active);
    assert!(debug.gate.emergency_disable);
    assert!(debug.gate.legacy_fallback_enabled);
    assert_eq!(debug.scheduler.metrics.suppressed_by_gate, 1);
    assert_eq!(debug.scheduler.metrics.suppressed_frames, 1);
}
