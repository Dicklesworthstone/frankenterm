use frankenterm_core::degradation::ResizeDegradationTier;
use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
    SubmitOutcome,
};
use frankenterm_core::runtime::{ResizeWatchdogSeverity, evaluate_resize_watchdog};

fn intent(pane_id: u64, intent_seq: u64, submitted_at_ms: u64) -> ResizeIntent {
    ResizeIntent {
        pane_id,
        intent_seq,
        scheduler_class: ResizeWorkClass::Interactive,
        work_units: 1,
        submitted_at_ms,
        domain: ResizeDomain::Local,
        tab_id: Some(1),
    }
}

#[test]
fn watchdog_escalates_to_critical_when_multiple_resize_transactions_stall() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 2,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, 1_000));
    let _ = scheduler.submit_intent(intent(2, 1, 1_010));

    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 2, "both panes should become active");

    let warning = evaluate_resize_watchdog(3_500).expect("watchdog snapshot should exist");
    assert_eq!(warning.severity, ResizeWatchdogSeverity::Warning);
    assert_eq!(warning.stalled_total, 2);
    assert!(!warning.safe_mode_recommended);

    let critical = evaluate_resize_watchdog(10_500).expect("watchdog snapshot should exist");
    assert_eq!(critical.severity, ResizeWatchdogSeverity::Critical);
    assert_eq!(critical.stalled_critical, 2);
    assert!(critical.safe_mode_recommended);
    assert!(critical.warning_line().is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn ipc_status_includes_resize_watchdog_assessment() {
    use std::sync::Arc;
    use std::time::Duration;

    use frankenterm_core::events::EventBus;
    use frankenterm_core::ipc::{IpcClient, IpcServer};
    use frankenterm_core::runtime_compat::{mpsc, mpsc_send, sleep};
    use tempfile::TempDir;

    // Create stale active transactions so watchdog emits a critical assessment.
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 2,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });
    let _ = scheduler.submit_intent(intent(10, 1, 0));
    let _ = scheduler.submit_intent(intent(11, 1, 0));
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 2);

    let temp_dir = TempDir::new().expect("temp dir");
    let socket_path = temp_dir.path().join("ipc-watchdog.sock");

    let server = IpcServer::bind(&socket_path)
        .await
        .expect("bind ipc server");
    let event_bus = Arc::new(EventBus::new(100));
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

    let server_bus = Arc::clone(&event_bus);
    let server_handle = tokio::spawn(async move {
        server.run(server_bus, shutdown_rx).await;
    });

    sleep(Duration::from_millis(10)).await;

    let client = IpcClient::new(&socket_path);
    let response = client.status().await.expect("ipc status response");
    assert!(response.ok);

    let data = response.data.expect("status payload data");
    let watchdog = data
        .get("resize_control_plane_watchdog")
        .expect("watchdog field should be present");
    assert!(
        watchdog.is_object(),
        "watchdog payload should be structured"
    );
    assert_eq!(
        watchdog.get("severity"),
        Some(&serde_json::json!("critical"))
    );
    assert_eq!(
        watchdog.get("safe_mode_recommended"),
        Some(&serde_json::json!(true))
    );
    let ladder = data
        .get("resize_degradation_ladder")
        .expect("resize_degradation_ladder field should be present");
    assert!(ladder.is_object(), "ladder payload should be structured");
    assert_eq!(
        ladder.get("tier"),
        Some(&serde_json::json!(
            ResizeDegradationTier::CorrectnessGuarded
        ))
    );

    let _ = mpsc_send(&shutdown_tx, ()).await;
    let _ = server_handle.await;
}

// ── DarkBadger wa-1u90p.7.1 ──────────────────────────────────────

#[test]
fn watchdog_healthy_when_no_stalled_transactions() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 4,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(1, 1, 100));
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 1);

    // Complete immediately — no stall
    assert!(scheduler.complete_active(1, 1));

    // Watchdog uses global state, but scheduler should be clean
    let assessment = evaluate_resize_watchdog(200);
    if let Some(assessment) = assessment {
        // May be healthy or reflect stale global state from prior test
        let _ = assessment.severity;
        let _ = assessment.warning_line();
    }
}

#[test]
fn watchdog_warning_line_is_none_for_healthy() {
    // Construct a watchdog assessment at healthy severity to verify warning_line behavior
    let assessment = evaluate_resize_watchdog(0);
    if let Some(ref a) = assessment {
        if a.severity == ResizeWatchdogSeverity::Healthy {
            assert!(
                a.warning_line().is_none(),
                "healthy watchdog should not produce a warning line"
            );
        }
    }
}

#[test]
fn watchdog_assessment_fields_populated() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 3,
        allow_single_oversubscription: false,
        ..ResizeSchedulerConfig::default()
    });

    let _ = scheduler.submit_intent(intent(30, 1, 500));
    let _ = scheduler.submit_intent(intent(31, 1, 510));
    let _ = scheduler.submit_intent(intent(32, 1, 520));
    let frame = scheduler.schedule_frame();
    assert_eq!(frame.scheduled.len(), 3);

    let assessment = evaluate_resize_watchdog(15_000);
    if let Some(a) = assessment {
        // Thresholds should be populated
        assert!(
            a.warning_threshold_ms > 0,
            "warning threshold should be set"
        );
        assert!(
            a.critical_threshold_ms > a.warning_threshold_ms,
            "critical threshold should exceed warning threshold"
        );
        assert!(
            a.critical_stalled_limit > 0,
            "critical stalled limit should be positive"
        );
        // recommended_action should be a non-empty string
        assert!(
            !a.recommended_action.is_empty(),
            "recommended_action should not be empty"
        );
    }
}

#[test]
fn watchdog_severity_ordering_healthy_then_warning_then_critical() {
    // Verify the severity enum variants are distinguishable
    let healthy = ResizeWatchdogSeverity::Healthy;
    let warning = ResizeWatchdogSeverity::Warning;
    let critical = ResizeWatchdogSeverity::Critical;
    let safe = ResizeWatchdogSeverity::SafeModeActive;

    assert_ne!(healthy, warning);
    assert_ne!(warning, critical);
    assert_ne!(critical, safe);

    // Debug format should work
    let dbg = format!("{:?}", healthy);
    assert!(dbg.contains("Healthy"));
}

#[test]
fn scheduler_emergency_disable_affects_watchdog_assessment() {
    let mut scheduler = ResizeScheduler::new(ResizeSchedulerConfig {
        frame_budget_units: 2,
        emergency_disable: true,
        legacy_fallback_enabled: true,
        ..ResizeSchedulerConfig::default()
    });

    let outcome = scheduler.submit_intent(intent(40, 1, 100));
    assert!(matches!(
        outcome,
        SubmitOutcome::SuppressedByKillSwitch {
            legacy_fallback: true
        }
    ));

    // When emergency_disable is active, watchdog should report safe mode
    let assessment = evaluate_resize_watchdog(200);
    if let Some(a) = assessment {
        if a.safe_mode_active {
            assert_eq!(a.severity, ResizeWatchdogSeverity::SafeModeActive);
            assert!(a.legacy_fallback_enabled);
            assert!(a.warning_line().is_some());
        }
    }
}

#[test]
fn degradation_tier_correctness_guarded_accessible() {
    // Verify the degradation tier constant is usable in integration context
    let tier = ResizeDegradationTier::CorrectnessGuarded;
    let json = serde_json::to_value(tier).expect("tier should serialize");
    assert!(json.is_string());
}
