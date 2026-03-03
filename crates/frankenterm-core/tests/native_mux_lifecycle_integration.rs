use std::collections::HashMap;

use frankenterm_core::session_topology::{
    LifecycleDecision, LifecycleEntityKind, LifecycleEvent, LifecycleIdentity, LifecycleRegistry,
    LifecycleState, LifecycleTransitionContext, LifecycleTransitionRequest, MuxPaneLifecycleState,
    TopologySnapshot,
};
use frankenterm_core::wezterm::{PaneInfo, PaneSize};

fn make_pane(
    pane_id: u64,
    tab_id: u64,
    window_id: u64,
    rows: u32,
    cols: u32,
    cwd: Option<&str>,
    title: Option<&str>,
    is_active: bool,
) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id,
        window_id,
        domain_id: None,
        domain_name: None,
        workspace: Some("swarm".to_string()),
        size: Some(PaneSize {
            rows,
            cols,
            pixel_width: None,
            pixel_height: None,
            dpi: None,
        }),
        rows: None,
        cols: None,
        title: title.map(str::to_string),
        cwd: cwd.map(str::to_string),
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active,
        is_zoomed: false,
        extra: HashMap::new(),
    }
}

fn context(ts: u64, scenario: &str, reason: &str) -> LifecycleTransitionContext {
    LifecycleTransitionContext::new(
        ts,
        "native_mux.lifecycle.integration",
        format!("corr-{ts}"),
        scenario,
        reason,
    )
}

#[test]
fn topology_capture_bootstrap_and_recovery_flow() {
    let panes = vec![
        make_pane(
            1001,
            500,
            300,
            24,
            80,
            Some("/work/a"),
            Some("agent-a"),
            true,
        ),
        make_pane(
            1002,
            500,
            300,
            24,
            80,
            Some("/work/b"),
            Some("agent-b"),
            false,
        ),
    ];

    let (snapshot, report) = TopologySnapshot::from_panes(&panes, 1_000_000);
    assert_eq!(report.window_count, 1);
    assert_eq!(report.tab_count, 1);
    assert_eq!(report.pane_count, 2);
    assert_eq!(snapshot.pane_count(), 2);

    let mut registry = LifecycleRegistry::bootstrap_from_panes(&panes, 7, 1_000_100).unwrap();
    assert_eq!(
        registry.entity_count_by_kind(LifecycleEntityKind::Session),
        1
    );
    assert_eq!(
        registry.entity_count_by_kind(LifecycleEntityKind::Window),
        1
    );
    assert_eq!(registry.entity_count_by_kind(LifecycleEntityKind::Pane), 2);

    let pane_identity = LifecycleIdentity::from_pane_info(&panes[0], 7);
    let before = registry.get(&pane_identity).unwrap();
    assert_eq!(
        before.state,
        LifecycleState::Pane(MuxPaneLifecycleState::Running)
    );
    assert_eq!(before.version, 0);

    let disconnected = registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_identity.clone(),
            event: LifecycleEvent::PeerDisconnected,
            expected_version: Some(0),
            context: context(
                1_000_200,
                "native-mux-recovery",
                "native_mux.lifecycle.disconnect",
            ),
        })
        .unwrap();
    assert_eq!(
        disconnected.record.state,
        LifecycleState::Pane(MuxPaneLifecycleState::Orphaned)
    );
    assert_eq!(disconnected.record.version, 1);

    let recovered = registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_identity,
            event: LifecycleEvent::Recover,
            expected_version: Some(1),
            context: context(
                1_000_300,
                "native-mux-recovery",
                "native_mux.lifecycle.recover",
            ),
        })
        .unwrap();
    assert_eq!(
        recovered.record.state,
        LifecycleState::Pane(MuxPaneLifecycleState::Ready)
    );
    assert_eq!(recovered.record.version, 2);

    let logs = registry.transition_log();
    assert_eq!(logs.len(), 2);
    let last = logs.last().unwrap();
    assert_eq!(last.decision, LifecycleDecision::Applied);
    assert_eq!(last.scenario_id, "native-mux-recovery");
    assert_eq!(last.correlation_id, "corr-1000300");
    assert_eq!(last.reason_code, "native_mux.lifecycle.recover");
    assert!(last.error_code.is_none());
}

#[test]
fn degraded_path_stale_writer_conflict_is_recorded_with_reason_codes() {
    let panes = vec![make_pane(
        2001,
        700,
        400,
        24,
        80,
        Some("/work/c"),
        Some("agent-c"),
        true,
    )];
    let mut registry = LifecycleRegistry::bootstrap_from_panes(&panes, 5, 2_000_000).unwrap();

    let pane_identity = LifecycleIdentity::from_pane_info(&panes[0], 5);

    let _ = registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_identity.clone(),
            event: LifecycleEvent::DrainRequested,
            expected_version: Some(0),
            context: context(
                2_000_100,
                "native-mux-stale-writer",
                "native_mux.lifecycle.drain_requested",
            ),
        })
        .unwrap();

    let err = registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_identity,
            event: LifecycleEvent::DrainCompleted,
            expected_version: Some(0),
            context: context(
                2_000_200,
                "native-mux-stale-writer",
                "native_mux.lifecycle.drain_completed",
            ),
        })
        .unwrap_err();
    assert!(err.to_string().contains("lifecycle concurrency conflict"));

    let rejected = registry.transition_log().last().unwrap();
    assert_eq!(rejected.decision, LifecycleDecision::Rejected);
    assert_eq!(
        rejected.error_code.as_deref(),
        Some("native_mux.lifecycle.version_conflict")
    );
    assert_eq!(rejected.scenario_id, "native-mux-stale-writer");
    assert_eq!(rejected.reason_code, "native_mux.lifecycle.drain_completed");
}

#[test]
fn deterministic_e2e_failure_injection_then_recovery_emits_artifacts() {
    let panes = vec![make_pane(
        3001,
        800,
        500,
        24,
        80,
        Some("/work/d"),
        Some("agent-d"),
        true,
    )];
    let mut registry = LifecycleRegistry::bootstrap_from_panes(&panes, 9, 3_000_000).unwrap();
    let pane_identity = LifecycleIdentity::from_pane_info(&panes[0], 9);

    let disconnected = registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_identity.clone(),
            event: LifecycleEvent::PeerDisconnected,
            expected_version: Some(0),
            context: context(
                3_000_100,
                "native-mux-e2e-failure-recovery",
                "native_mux.lifecycle.disconnect",
            ),
        })
        .unwrap();
    assert_eq!(
        disconnected.record.state,
        LifecycleState::Pane(MuxPaneLifecycleState::Orphaned)
    );
    assert_eq!(disconnected.record.version, 1);

    // Failure injection: stale writer tries to recover using pre-disconnect version.
    let stale_err = registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_identity.clone(),
            event: LifecycleEvent::Recover,
            expected_version: Some(0),
            context: context(
                3_000_200,
                "native-mux-e2e-failure-recovery",
                "native_mux.lifecycle.recover_stale_writer",
            ),
        })
        .unwrap_err();
    assert!(
        stale_err
            .to_string()
            .contains("lifecycle concurrency conflict")
    );

    let recovered = registry
        .apply_transition(LifecycleTransitionRequest {
            identity: pane_identity,
            event: LifecycleEvent::Recover,
            expected_version: Some(1),
            context: context(
                3_000_300,
                "native-mux-e2e-failure-recovery",
                "native_mux.lifecycle.recover",
            ),
        })
        .unwrap();
    assert_eq!(
        recovered.record.state,
        LifecycleState::Pane(MuxPaneLifecycleState::Ready)
    );
    assert_eq!(recovered.record.version, 2);

    let log_entries = registry.transition_log();
    assert_eq!(log_entries.len(), 3);
    assert_eq!(log_entries[0].decision, LifecycleDecision::Applied);
    assert_eq!(log_entries[1].decision, LifecycleDecision::Rejected);
    assert_eq!(log_entries[2].decision, LifecycleDecision::Applied);
    assert_eq!(
        log_entries[1].error_code.as_deref(),
        Some("native_mux.lifecycle.version_conflict")
    );

    // Deterministic JSON evidence that can be persisted by CI/triage tooling.
    let snapshot_json = registry.snapshot_json().unwrap();
    let transition_json = registry.transition_log_json().unwrap();
    assert!(snapshot_json.contains("pane"));
    assert!(transition_json.contains("native-mux-e2e-failure-recovery"));
    assert!(transition_json.contains("correlation_id"));
    assert!(transition_json.contains("reason_code"));
}
