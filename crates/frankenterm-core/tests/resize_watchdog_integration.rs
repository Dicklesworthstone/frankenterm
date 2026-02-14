use frankenterm_core::degradation::ResizeDegradationTier;
use frankenterm_core::resize_scheduler::{
    ResizeDomain, ResizeIntent, ResizeScheduler, ResizeSchedulerConfig, ResizeWorkClass,
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
    use frankenterm_core::runtime_compat::{mpsc, sleep};
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

    let _ = shutdown_tx.send(()).await;
    let _ = server_handle.await;
}
