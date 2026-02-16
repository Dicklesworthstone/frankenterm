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

// ── DarkBadger wa-1u90p.7.1 ──────────────────────────────────────

#[test]
#[allow(clippy::similar_names)]
fn multi_ssh_domain_fair_budget_partitioning() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 8,
        domain_budget_enabled: true,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    // SSH host A: 2 panes, each 3 units
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        1,
        1,
        ResizeWorkClass::Interactive,
        3,
        100,
        ResizeDomain::Ssh {
            host: "hostA".into(),
        },
        1,
    ));
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        2,
        1,
        ResizeWorkClass::Interactive,
        3,
        101,
        ResizeDomain::Ssh {
            host: "hostA".into(),
        },
        1,
    ));
    // SSH host B: 1 pane, 3 units
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        3,
        1,
        ResizeWorkClass::Interactive,
        3,
        102,
        ResizeDomain::Ssh {
            host: "hostB".into(),
        },
        2,
    ));

    let frame = scheduler.schedule_frame();
    // Domain budgets should be proportional to domain weights
    // Both SSH domains have weight=2, total_weight=4, budget_each=4
    // hostA: 3+3=6 > 4 budget → at most 1 pick from hostA
    // hostB: 3 <= 4 budget → 1 pick from hostB
    let host_a_picks = frame.scheduled.iter().filter(|w| w.pane_id <= 2).count();
    let host_b_picks = frame.scheduled.iter().filter(|w| w.pane_id == 3).count();
    assert!(host_a_picks <= 2, "hostA should be budget-constrained");
    assert_eq!(host_b_picks, 1, "hostB should get its pane scheduled");
}

#[test]
fn local_domain_gets_higher_share_than_remote() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 12,
        domain_budget_enabled: true,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    // Local: 3 panes, weight=4
    for i in 1..=3 {
        let _ = scheduler.submit_intent(intent_with_domain_and_tab(
            i,
            1,
            ResizeWorkClass::Interactive,
            2,
            100 + i,
            ResizeDomain::Local,
            1,
        ));
    }
    // SSH: 3 panes, weight=2
    for i in 10..=12 {
        let _ = scheduler.submit_intent(intent_with_domain_and_tab(
            i,
            1,
            ResizeWorkClass::Interactive,
            2,
            100 + i,
            ResizeDomain::Ssh {
                host: "remote".into(),
            },
            2,
        ));
    }

    let frame = scheduler.schedule_frame();
    let local_picks = frame.scheduled.iter().filter(|w| w.pane_id < 10).count();
    let ssh_picks = frame.scheduled.iter().filter(|w| w.pane_id >= 10).count();

    // Local weight=4, SSH weight=2, total=6
    // Local share = 12*4/6 = 8, SSH share = 12*2/6 = 4
    // Local: 3*2=6 fits in 8 → all 3 scheduled
    // SSH: 3*2=6 > 4 → at most 2 scheduled
    assert_eq!(
        local_picks, 3,
        "all local panes should fit in local budget share"
    );
    assert!(
        ssh_picks <= 2,
        "SSH panes should be limited by domain budget share, got {}",
        ssh_picks
    );
}

#[test]
fn storm_tab_rotation_allows_different_tabs_per_frame() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        storm_window_ms: 50,
        storm_threshold_intents: 3,
        max_storm_picks_per_tab: 1,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    // Tab 1: 4 panes (triggers storm)
    for i in 1..=4 {
        let _ = scheduler.submit_intent(intent_with_domain_and_tab(
            i,
            1,
            ResizeWorkClass::Interactive,
            2,
            100 + i,
            ResizeDomain::Local,
            1,
        ));
    }
    // Tab 2: 2 panes (no storm)
    for i in 10..=11 {
        let _ = scheduler.submit_intent(intent_with_domain_and_tab(
            i,
            1,
            ResizeWorkClass::Interactive,
            2,
            100 + i,
            ResizeDomain::Local,
            2,
        ));
    }

    let frame = scheduler.schedule_frame();
    let tab1_picks = frame.scheduled.iter().filter(|w| w.pane_id < 10).count();
    let tab2_picks = frame.scheduled.iter().filter(|w| w.pane_id >= 10).count();

    // Tab 1 is in storm → max 1 pick per tab per frame
    assert_eq!(tab1_picks, 1, "stormed tab should be limited to 1 pick");
    // Tab 2 is not stormed → both panes can schedule
    assert_eq!(tab2_picks, 2, "non-stormed tab should schedule freely");
    assert!(scheduler.metrics().storm_events_detected > 0);
    assert!(scheduler.metrics().storm_picks_throttled > 0);
}

#[test]
fn mux_domain_panes_respect_endpoint_grouping() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 8,
        domain_budget_enabled: true,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    // Two different mux endpoints
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        1,
        1,
        ResizeWorkClass::Interactive,
        3,
        100,
        ResizeDomain::Mux {
            endpoint: "east".into(),
        },
        1,
    ));
    let _ = scheduler.submit_intent(intent_with_domain_and_tab(
        2,
        1,
        ResizeWorkClass::Interactive,
        3,
        101,
        ResizeDomain::Mux {
            endpoint: "west".into(),
        },
        2,
    ));

    let frame = scheduler.schedule_frame();
    // Mux weight=1 each, total_weight=2, each gets 4 units
    // 3 units fits in 4 budget → both should schedule
    assert_eq!(
        frame.scheduled.len(),
        2,
        "different mux endpoints should get independent budget shares"
    );
}

#[test]
fn domain_budget_disabled_allows_single_domain_to_consume_all() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 10,
        domain_budget_enabled: false,
        allow_single_oversubscription: false,
        // Disable storm throttling — this test focuses on domain budget behavior
        storm_threshold_intents: 100,
        ..ResizeSchedulerConfig::default()
    });

    // 5 SSH panes from same host, each 2 units
    for i in 1..=5 {
        let _ = scheduler.submit_intent(intent_with_domain_and_tab(
            i,
            1,
            ResizeWorkClass::Interactive,
            2,
            100 + i,
            ResizeDomain::Ssh {
                host: "single".into(),
            },
            1,
        ));
    }

    let frame = scheduler.schedule_frame();
    // All 5*2=10 fits budget=10
    assert_eq!(
        frame.scheduled.len(),
        5,
        "without domain budgets, single domain can use full budget"
    );
    assert_eq!(scheduler.metrics().domain_budget_throttled, 0);
}
