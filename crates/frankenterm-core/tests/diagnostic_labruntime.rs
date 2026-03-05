//! LabRuntime-ported diagnostic tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` tests from `diagnostic.rs` to asupersync-based
//! `RuntimeFixture`, gaining deterministic scheduling for StorageHandle I/O.
//!
//! Covers: generate_bundle (5 async integration tests).
//!
//! Bead: ft-22x4r

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::config::{Config, WorkspaceLayout};
use frankenterm_core::diagnostic::{DiagnosticOptions, generate_bundle};
use frankenterm_core::storage::{AuditActionRecord, PaneRecord, SCHEMA_VERSION, StorageHandle};
use std::sync::atomic::{AtomicU64, Ordering};

// ===========================================================================
// Helpers
// ===========================================================================

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir();
    dir.join(format!(
        "wa_labrt_diag_test_{counter}_{}.db",
        std::process::id()
    ))
    .to_str()
    .unwrap()
    .to_string()
}

// ===========================================================================
// Section 1: generate_bundle integration tests
// ===========================================================================

#[test]
fn generate_bundle_creates_all_files() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let storage = StorageHandle::new(&db_path).await.unwrap();

        // Insert test data
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: 1000,
            last_seen_at: 1000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();
        storage
            .append_segment(1, "test output", None)
            .await
            .unwrap();

        let config = Config::default();
        let layout = WorkspaceLayout::new(
            std::env::temp_dir().join(format!("wa_labrt_diag_ws_all_{}", std::process::id())),
            &config.storage,
            &config.ipc,
        );

        let output_dir =
            std::env::temp_dir().join(format!("wa_labrt_diag_output_all_{}", std::process::id()));
        let opts = DiagnosticOptions {
            output: Some(output_dir.clone()),
            ..Default::default()
        };

        let result = generate_bundle(&config, &layout, &storage, &opts)
            .await
            .unwrap();

        // Verify output
        assert_eq!(result.output_path, output_dir.display().to_string());
        assert!(result.file_count >= 9);
        assert!(result.total_size_bytes > 0);

        // Verify expected files exist
        assert!(output_dir.join("manifest.json").exists());
        assert!(output_dir.join("environment.json").exists());
        assert!(output_dir.join("config_summary.json").exists());
        assert!(output_dir.join("db_health.json").exists());
        assert!(output_dir.join("recent_events.json").exists());
        assert!(output_dir.join("recent_workflows.json").exists());
        assert!(output_dir.join("active_reservations.json").exists());
        assert!(output_dir.join("reservation_history.json").exists());
        assert!(output_dir.join("recent_audit.json").exists());

        // Verify manifest is valid JSON with expected fields
        let manifest_content = std::fs::read_to_string(output_dir.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_content).unwrap();
        assert!(manifest["redacted"].as_bool().unwrap());
        assert!(manifest["file_count"].as_u64().unwrap() >= 8);
        assert!(!manifest["wa_version"].as_str().unwrap().is_empty());

        // Verify environment.json
        let env_content = std::fs::read_to_string(output_dir.join("environment.json")).unwrap();
        let env_info: serde_json::Value = serde_json::from_str(&env_content).unwrap();
        assert!(!env_info["wa_version"].as_str().unwrap().is_empty());
        assert_eq!(env_info["schema_version"], SCHEMA_VERSION);

        // Verify db_health.json
        let health_content = std::fs::read_to_string(output_dir.join("db_health.json")).unwrap();
        let health: serde_json::Value = serde_json::from_str(&health_content).unwrap();
        assert!(health["page_count"].as_i64().unwrap() > 0);
        assert_eq!(health["table_counts"]["panes"], 1);

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&output_dir);
        let _ = std::fs::remove_dir_all(layout.root);
    });
}

#[test]
fn bundle_does_not_contain_secrets() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let storage = StorageHandle::new(&db_path).await.unwrap();

        // Insert data with a secret
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: 1000,
            last_seen_at: 1000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();

        // Record an audit action with a secret in decision_reason
        let action = AuditActionRecord {
            id: 0,
            ts: 1000,
            actor_kind: "workflow".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: None,
            action_kind: "test".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some(
                "API key sk-abc123def456ghi789jkl012mno345pqr678stu901v found".to_string(),
            ),
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "ok".to_string(),
        };
        storage.record_audit_action(action).await.unwrap();

        let config = Config::default();
        let layout = WorkspaceLayout::new(
            std::env::temp_dir().join(format!("wa_labrt_diag_secrets_ws_{}", std::process::id())),
            &config.storage,
            &config.ipc,
        );

        let output_dir = std::env::temp_dir().join(format!(
            "wa_labrt_diag_secrets_output_{}",
            std::process::id()
        ));
        let opts = DiagnosticOptions {
            output: Some(output_dir.clone()),
            ..Default::default()
        };

        generate_bundle(&config, &layout, &storage, &opts)
            .await
            .unwrap();

        // Read all files and verify no secrets leak
        let secret = "sk-abc123def456ghi789jkl012mno345pqr678stu901v";
        for entry in std::fs::read_dir(&output_dir).unwrap() {
            let entry = entry.unwrap();
            let content = std::fs::read_to_string(entry.path()).unwrap();
            assert!(
                !content.contains(secret),
                "Secret found in {}",
                entry.file_name().to_string_lossy()
            );
        }

        // Verify the audit file exists and has [REDACTED]
        let audit_content = std::fs::read_to_string(output_dir.join("recent_audit.json")).unwrap();
        assert!(audit_content.contains("[REDACTED]"));

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&output_dir);
        let _ = std::fs::remove_dir_all(layout.root);
    });
}

#[test]
fn bundle_manifest_has_stable_metadata() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let storage = StorageHandle::new(&db_path).await.unwrap();

        let config = Config::default();
        let layout = WorkspaceLayout::new(
            std::env::temp_dir().join(format!("wa_labrt_diag_meta_ws_{}", std::process::id())),
            &config.storage,
            &config.ipc,
        );

        let output_dir =
            std::env::temp_dir().join(format!("wa_labrt_diag_meta_output_{}", std::process::id()));
        let opts = DiagnosticOptions {
            output: Some(output_dir.clone()),
            ..Default::default()
        };

        generate_bundle(&config, &layout, &storage, &opts)
            .await
            .unwrap();

        // Verify manifest has all required stable metadata fields
        let manifest_content = std::fs::read_to_string(output_dir.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_content).unwrap();

        // Required fields
        assert!(manifest["wa_version"].is_string());
        assert!(!manifest["wa_version"].as_str().unwrap().is_empty());
        assert!(manifest["generated_at_ms"].is_number());
        assert!(manifest["generated_at_ms"].as_u64().unwrap() > 0);
        assert!(manifest["file_count"].is_number());
        assert!(manifest["redacted"].as_bool().unwrap());
        assert!(manifest["files"].is_array());
        let files = manifest["files"].as_array().unwrap();
        assert!(files.len() >= 8);

        // Verify environment.json has stable fields
        let env_content = std::fs::read_to_string(output_dir.join("environment.json")).unwrap();
        let env: serde_json::Value = serde_json::from_str(&env_content).unwrap();
        assert!(env["wa_version"].is_string());
        assert!(env["schema_version"].is_number());
        assert!(env["os"].is_string());
        assert!(env["arch"].is_string());

        // Verify config_summary.json has stable fields
        let config_content =
            std::fs::read_to_string(output_dir.join("config_summary.json")).unwrap();
        let config_json: serde_json::Value = serde_json::from_str(&config_content).unwrap();
        assert!(config_json["general_log_level"].is_string());
        assert!(config_json["ingest_poll_interval_ms"].is_number());
        assert!(config_json["metrics_enabled"].is_boolean());

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&output_dir);
        let _ = std::fs::remove_dir_all(layout.root);
    });
}

#[test]
fn bundle_includes_reservation_snapshot() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let storage = StorageHandle::new(&db_path).await.unwrap();

        // Create a pane
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: 1000,
            last_seen_at: 1000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();

        // Create an active reservation
        let res = storage
            .create_reservation(
                1,
                "workflow",
                "wf-test-123",
                Some("testing bundle"),
                3_600_000,
            )
            .await
            .unwrap();
        assert!(res.id > 0);

        let config = Config::default();
        let layout = WorkspaceLayout::new(
            std::env::temp_dir().join(format!("wa_labrt_diag_res_ws_{}", std::process::id())),
            &config.storage,
            &config.ipc,
        );

        let output_dir =
            std::env::temp_dir().join(format!("wa_labrt_diag_res_output_{}", std::process::id()));
        let opts = DiagnosticOptions {
            output: Some(output_dir.clone()),
            ..Default::default()
        };

        generate_bundle(&config, &layout, &storage, &opts)
            .await
            .unwrap();

        // Verify active_reservations.json contains the reservation
        let res_content =
            std::fs::read_to_string(output_dir.join("active_reservations.json")).unwrap();
        let reservations: serde_json::Value = serde_json::from_str(&res_content).unwrap();
        let arr = reservations.as_array().unwrap();
        assert!(
            !arr.is_empty(),
            "Active reservations should contain at least one entry"
        );

        // Verify reservation fields are present
        let first = &arr[0];
        assert_eq!(first["pane_id"], 1);
        assert_eq!(first["owner_kind"], "workflow");
        assert_eq!(first["status"], "active");
        assert!(first["created_at"].is_number());
        assert!(first["expires_at"].is_number());

        // Verify reservation_history.json also has the reservation
        let hist_content =
            std::fs::read_to_string(output_dir.join("reservation_history.json")).unwrap();
        let history: serde_json::Value = serde_json::from_str(&hist_content).unwrap();
        let hist_arr = history.as_array().unwrap();
        assert!(!hist_arr.is_empty());

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&output_dir);
        let _ = std::fs::remove_dir_all(layout.root);
    });
}

#[test]
fn bundle_output_dir_reuse_generates_fresh_bundle() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let db_path = temp_db_path();
        let storage = StorageHandle::new(&db_path).await.unwrap();

        let config = Config::default();
        let layout = WorkspaceLayout::new(
            std::env::temp_dir().join(format!("wa_labrt_diag_reuse_ws_{}", std::process::id())),
            &config.storage,
            &config.ipc,
        );

        let output_dir =
            std::env::temp_dir().join(format!("wa_labrt_diag_reuse_output_{}", std::process::id()));

        // Generate first bundle
        let opts = DiagnosticOptions {
            output: Some(output_dir.clone()),
            ..Default::default()
        };
        let result1 = generate_bundle(&config, &layout, &storage, &opts)
            .await
            .unwrap();
        assert!(result1.file_count >= 9);

        // Generate second bundle to the same directory (should overwrite)
        let result2 = generate_bundle(&config, &layout, &storage, &opts)
            .await
            .unwrap();
        assert!(result2.file_count >= 9);

        // The manifest should be from the second run (newer timestamp)
        let manifest_content = std::fs::read_to_string(output_dir.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_content).unwrap();
        assert!(manifest["generated_at_ms"].as_u64().unwrap() > 0);

        storage.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&output_dir);
        let _ = std::fs::remove_dir_all(layout.root);
    });
}
