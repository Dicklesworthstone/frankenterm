//! LabRuntime-ported workflow tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` tests from `workflows/mod.rs` to asupersync-based
//! `RuntimeFixture`, gaining deterministic scheduling for StorageHandle I/O.
//!
//! Covers: descriptor workflow execution (9), policy gated injector (1),
//! HandleCompaction (5), HandleProcessTriageLifecycle (3),
//! HandleSessionEnd (1), persist_caut_refresh_accounts (1),
//! HandleAuthRequired (2) — 22 async integration tests.
//!
//! Bead: ft-22x4r

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::storage::StorageHandle;
use frankenterm_core::policy::PaneCapabilities;
use frankenterm_core::workflows::{
    DescriptorWorkflow, HandleCompaction, HandleProcessTriageLifecycle, HandleSessionEnd,
    StepResult, Workflow, WorkflowContext, WorkflowDescriptor, AUTH_COOLDOWN_MS,
};
use std::sync::Arc;

// ===========================================================================
// Helpers
// ===========================================================================

/// Replacement for `pub(super) fn now_ms()` in workflows/engine.rs.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

// ===========================================================================
// Section 1: Descriptor workflow execution tests
// ===========================================================================

#[test]
fn descriptor_send_ctrl_requires_injector() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
workflow_schema_version: 1
name: "ctrl_only"
steps:
  - type: send_ctrl
    id: interrupt
    key: ctrl_c
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let workflow = DescriptorWorkflow::new(descriptor);

        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("descriptor_send_ctrl.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());

        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-ctrl");
        let result = workflow.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => {
                assert!(reason.contains("No injector configured"));
            }
            other => panic!("Expected abort, got: {other:?}"),
        }
    });
}

#[test]
fn descriptor_notify_step_returns_continue() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
workflow_schema_version: 1
name: "notify_exec"
steps:
  - type: notify
    id: alert
    message: "test notification"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("notify.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-notify");
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
    });
}

#[test]
fn descriptor_log_step_returns_continue() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
workflow_schema_version: 1
name: "log_exec"
steps:
  - type: log
    id: entry
    message: "audit trail entry"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("log.db").to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-log");
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
    });
}

#[test]
fn descriptor_abort_step_returns_abort() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
workflow_schema_version: 1
name: "abort_exec"
steps:
  - type: abort
    id: bail
    reason: "cannot proceed"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("abort.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-abort");
        let result = workflow.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => assert_eq!(reason, "cannot proceed"),
            other => panic!("Expected Abort, got: {other:?}"),
        }
    });
}

#[test]
fn descriptor_conditional_then_branch() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_then"
steps:
  - type: conditional
    id: check
    test_text: "error detected in output"
    matcher:
      kind: substring
      value: "error"
    then_steps:
      - type: notify
        id: alert
        message: "error found"
    else_steps:
      - type: log
        id: ok
        message: "all clear"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("cond.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-cond");
        // test_text contains "error" so then_steps should run (notify returns Continue)
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
    });
}

#[test]
fn descriptor_conditional_else_branch() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_else"
steps:
  - type: conditional
    id: check
    test_text: "all systems nominal"
    matcher:
      kind: substring
      value: "error"
    then_steps:
      - type: abort
        id: bail
        reason: "error found"
    else_steps:
      - type: notify
        id: ok
        message: "all clear"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("cond_else.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
        let mut ctx =
            WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-cond-else");
        // test_text does NOT contain "error" so else branch runs (notify returns Continue).
        let num_steps = workflow.step_count();
        let mut step = 0usize;
        let result = loop {
            let r = workflow.execute_step(&mut ctx, step).await;
            match r {
                StepResult::JumpTo { step: target } => step = target,
                StepResult::Continue if step + 1 < num_steps => step += 1,
                other => break other,
            }
        };
        assert!(result.is_continue());
    });
}

#[test]
fn descriptor_conditional_then_with_abort() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_abort"
steps:
  - type: conditional
    id: check
    test_text: "FATAL error occurred"
    matcher:
      kind: regex
      pattern: "FATAL"
    then_steps:
      - type: abort
        id: bail
        reason: "fatal error"
    else_steps: []
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("cond_abort.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
        let mut ctx =
            WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-cond-abort");
        let num_steps = workflow.step_count();
        let mut step = 0usize;
        let result = loop {
            let r = workflow.execute_step(&mut ctx, step).await;
            match r {
                StepResult::JumpTo { step: target } => step = target,
                StepResult::Continue if step + 1 < num_steps => step += 1,
                other => break other,
            }
        };
        match result {
            StepResult::Abort { reason } => assert_eq!(reason, "fatal error"),
            other => panic!("Expected Abort, got: {other:?}"),
        }
    });
}

#[test]
fn descriptor_loop_repeats_steps() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
workflow_schema_version: 1
name: "loop_test"
steps:
  - type: loop
    id: repeat
    count: 3
    body:
      - type: log
        id: tick
        message: "iteration"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("loop.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-loop");
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
    });
}

#[test]
fn descriptor_loop_aborts_on_abort_step() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let yaml = r#"
workflow_schema_version: 1
name: "loop_abort"
steps:
  - type: loop
    id: repeat
    count: 10
    body:
      - type: abort
        id: bail
        reason: "stop early"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("loop_abort.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
        let mut ctx =
            WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-loop-abort");
        let result = workflow.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => assert_eq!(reason, "stop early"),
            other => panic!("Expected Abort, got: {other:?}"),
        }
    });
}

// ===========================================================================
// Section 2: Policy gated injector test
// ===========================================================================

#[test]
fn policy_gated_injector_returns_denied_for_running_command() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use frankenterm_core::policy::{
            ActorKind, InjectionResult, PaneCapabilities as PolicyPaneCaps, PolicyEngine,
            PolicyGatedInjector,
        };

        let engine = PolicyEngine::strict();
        let client = frankenterm_core::wezterm::default_wezterm_handle();
        let mut injector = PolicyGatedInjector::new(engine, client);

        let caps = PolicyPaneCaps::running();

        let result = injector
            .send_text(
                42,
                "echo test",
                ActorKind::Workflow,
                &caps,
                Some("wf-test-002"),
            )
            .await;

        assert!(
            result.is_denied(),
            "Expected denied result, got: {result:?}"
        );

        if let InjectionResult::Denied { decision, .. } = result {
            assert_eq!(
                decision.rule_id(),
                Some("policy.prompt_required"),
                "Expected policy.prompt_required rule, got: {:?}",
                decision.rule_id()
            );
        }
    });
}

// ===========================================================================
// Section 3: HandleCompaction tests
// ===========================================================================

#[test]
fn handle_compaction_execute_step0_prompt_active_continues() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step0.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step0-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 0).await;

        match result {
            StepResult::Continue => {}
            StepResult::Abort { reason } => {
                panic!("Step 0 should not abort for PromptActive state: {}", reason);
            }
            other => {
                panic!("Unexpected step result for step 0: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    });
}

#[test]
fn handle_compaction_execute_step0_alt_screen_aborts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step0_alt.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());

        let alt_caps = PaneCapabilities {
            alt_screen: Some(true),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, alt_caps, "test-step0-alt-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 0).await;

        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("alt-screen"),
                    "Abort reason should mention 'alt-screen': {}",
                    reason
                );
            }
            StepResult::Continue => {
                panic!("Step 0 should abort for AltScreen state, got Continue");
            }
            other => {
                panic!(
                    "Unexpected step result for step 0 with AltScreen: {:?}",
                    other
                );
            }
        }

        storage.shutdown().await.unwrap();
    });
}

#[test]
fn handle_compaction_execute_step1_returns_continue() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step1.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step1-001");

        let workflow = HandleCompaction::new()
            .with_stabilization_ms(0)
            .with_idle_timeout_ms(50);
        let result = workflow.execute_step(&mut ctx, 1).await;

        match result {
            StepResult::Continue => {}
            StepResult::Abort { reason } => {
                panic!("Step 1 should not abort when stabilization is zero: {reason}");
            }
            other => panic!("Step 1 should return Continue, got: {:?}", other),
        }

        storage.shutdown().await.unwrap();
    });
}

#[test]
fn handle_compaction_execute_step2_no_injector_aborts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step2_no_inj.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx =
            WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step2-no-inj-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 2).await;

        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.to_lowercase().contains("injector"),
                    "Abort reason should mention missing injector: {}",
                    reason
                );
            }
            other => {
                panic!("Step 2 should abort without injector, got: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    });
}

#[test]
fn handle_compaction_execute_invalid_step_aborts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_invalid_step.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities::default();

        let mut ctx =
            WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-invalid-step-001");

        let workflow = HandleCompaction::new();

        let invalid_step = workflow.step_count() + 1;
        let result = workflow.execute_step(&mut ctx, invalid_step).await;

        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("step") || reason.contains("index"),
                    "Abort reason should mention invalid step: {}",
                    reason
                );
            }
            other => {
                panic!("Invalid step should abort, got: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    });
}

// ===========================================================================
// Section 4: HandleProcessTriageLifecycle tests
// ===========================================================================

#[test]
fn handle_process_triage_lifecycle_step0_aborts_on_alt_screen() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("triage_lifecycle_alt_screen.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let mut caps = PaneCapabilities::default();
        caps.alt_screen = Some(true);
        let mut ctx = WorkflowContext::new(storage, 7, caps, "exec-triage-alt");

        let wf = HandleProcessTriageLifecycle::new();
        let result = wf.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => {
                assert!(reason.contains("alt-screen"), "unexpected reason: {reason}");
            }
            other => panic!("expected abort, got {other:?}"),
        }
    });
}

#[test]
fn handle_process_triage_lifecycle_step2_aborts_on_protected_destructive_action() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("triage_lifecycle_protected_abort.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let trigger = serde_json::json!({
            "process_triage": {
                "plan": {
                    "entries": [
                        {
                            "category": "system_process",
                            "action": { "action": "force_kill" }
                        }
                    ],
                    "auto_safe_count": 0,
                    "review_count": 0,
                    "protected_count": 1
                }
            }
        });

        let mut ctx = WorkflowContext::new(
            storage,
            9,
            PaneCapabilities::default(),
            "exec-triage-protected",
        )
        .with_trigger(trigger);

        let wf = HandleProcessTriageLifecycle::new();
        let result = wf.execute_step(&mut ctx, 2).await;
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("protected category includes destructive action"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected abort, got {other:?}"),
        }
    });
}

#[test]
fn handle_process_triage_lifecycle_session_step_emits_all_artifacts() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("triage_lifecycle_session.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let trigger = serde_json::json!({
            "process_triage": {
                "ft_session_id": "ft-abc",
                "pt_session_id": "pt-xyz",
                "provider": "pt_cli",
                "plan": {
                    "entries": [
                        {
                            "category": "stuck_cli",
                            "action": { "action": "graceful_kill" }
                        },
                        {
                            "category": "active_agent",
                            "action": { "action": "protect" }
                        }
                    ],
                    "auto_safe_count": 1,
                    "review_count": 0,
                    "protected_count": 1
                }
            }
        });

        let mut ctx = WorkflowContext::new(
            storage,
            42,
            PaneCapabilities::default(),
            "exec-triage-session",
        )
        .with_trigger(trigger);
        let wf = HandleProcessTriageLifecycle::new();
        let result = wf.execute_step(&mut ctx, 5).await;

        match result {
            StepResult::Done { result } => {
                assert_eq!(result["status"], "completed");
                assert_eq!(result["workflow"], "handle_process_triage_lifecycle");
                assert!(result["snapshot"].is_object());
                assert!(result["plan"].is_object());
                assert!(result["apply"].is_object());
                assert!(result["verify"].is_object());
                assert!(result["diff"].is_object());
                assert_eq!(result["session"]["ft_session_id"], "ft-abc");
                assert_eq!(result["session"]["pt_session_id"], "pt-xyz");
                assert_eq!(result["session"]["provider"], "pt_cli");
            }
            other => panic!("expected done, got {other:?}"),
        }
    });
}

// ===========================================================================
// Section 5: HandleSessionEnd test
// ===========================================================================

#[test]
fn handle_session_end_persist_roundtrip() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_labrt_session_end_{}_{n}.db",
            std::process::id()
        ));
        let db = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        let pane = frankenterm_core::storage::PaneRecord {
            pane_id: 77,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        db.upsert_pane(pane).await.expect("insert pane");

        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": "5000",
                "input": "3000",
                "output": "2000",
                "session_id": "abc-def-123",
            }
        });
        let record = HandleSessionEnd::record_from_detection(77, &trigger);
        let db_id = db.upsert_agent_session(record).await.expect("upsert");
        assert!(db_id > 0);

        let session = db
            .get_agent_session(db_id)
            .await
            .expect("query")
            .expect("session should exist");
        assert_eq!(session.agent_type, "codex");
        assert_eq!(session.session_id.as_deref(), Some("abc-def-123"));
        assert_eq!(session.total_tokens, Some(5000));
        assert_eq!(session.input_tokens, Some(3000));
        assert_eq!(session.output_tokens, Some(2000));
        assert!(session.ended_at.is_some());
        assert_eq!(session.end_reason.as_deref(), Some("completed"));

        let _ = std::fs::remove_file(&db_path);
    });
}

// ===========================================================================
// Section 6: persist_caut_refresh_accounts test
// ===========================================================================

#[test]
fn persist_caut_refresh_accounts_records_metrics() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_labrt_caut_metrics_{}_{n}.db",
            std::process::id()
        ));
        let storage = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        let refresh = frankenterm_core::caut::CautRefresh {
            service: Some("openai".to_string()),
            refreshed_at: Some("2026-02-06T00:00:00Z".to_string()),
            accounts: vec![
                frankenterm_core::caut::CautAccountUsage {
                    id: Some("acct-1".to_string()),
                    name: Some("Account 1".to_string()),
                    percent_remaining: Some(42.0),
                    limit_hours: None,
                    reset_at: Some("2026-02-06T01:00:00Z".to_string()),
                    tokens_used: Some(1000),
                    tokens_remaining: Some(2000),
                    tokens_limit: Some(3000),
                    extra: HashMap::new(),
                },
                frankenterm_core::caut::CautAccountUsage {
                    id: Some("acct-2".to_string()),
                    name: Some("Account 2".to_string()),
                    percent_remaining: Some(7.0),
                    limit_hours: None,
                    reset_at: None,
                    tokens_used: Some(10),
                    tokens_remaining: Some(20),
                    tokens_limit: Some(30),
                    extra: HashMap::new(),
                },
            ],
            extra: HashMap::new(),
        };

        let now = 10_000_i64;
        let refreshed = frankenterm_core::workflows::persist_caut_refresh_accounts(
            &storage,
            frankenterm_core::caut::CautService::OpenAI,
            &refresh,
            now,
        )
        .await
        .expect("persist refresh");
        assert_eq!(refreshed, 2);

        let acct1 = storage
            .query_usage_metrics(frankenterm_core::storage::MetricQuery {
                metric_type: Some(frankenterm_core::storage::MetricType::TokenUsage),
                agent_type: None,
                account_id: Some("acct-1".to_string()),
                since: Some(0),
                until: None,
                limit: Some(10),
            })
            .await
            .expect("query metrics");
        assert_eq!(acct1.len(), 1);
        assert_eq!(acct1[0].tokens, Some(1000));
        assert_eq!(acct1[0].amount, None);

        storage.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_file(&db_path);
    });
}

// ===========================================================================
// Section 7: HandleAuthRequired tests
// ===========================================================================

#[test]
fn handle_auth_required_audit_roundtrip() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_labrt_auth_req_{}_{n}.db",
            std::process::id()
        ));
        let db = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        let pane = frankenterm_core::storage::PaneRecord {
            pane_id: 88,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        db.upsert_pane(pane).await.expect("insert pane");

        let audit = frankenterm_core::storage::AuditActionRecord {
            id: 0,
            ts: now_ms(),
            actor_kind: "workflow".to_string(),
            actor_id: Some("test-exec-1".to_string()),
            correlation_id: None,
            pane_id: Some(88),
            domain: None,
            action_kind: "auth_required".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: Some("codex.auth.device_code_prompt".to_string()),
            input_summary: Some("Auth required for codex: device_code".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "recorded".to_string(),
        };
        let audit_id = db.record_audit_action(audit).await.expect("record");
        assert!(audit_id > 0);

        let query = frankenterm_core::storage::AuditQuery {
            pane_id: Some(88),
            action_kind: Some("auth_required".to_string()),
            limit: Some(10),
            ..Default::default()
        };
        let results = db.get_audit_actions(query).await.expect("query");
        assert!(!results.is_empty());
        assert_eq!(results[0].action_kind, "auth_required");
        assert_eq!(results[0].pane_id, Some(88));

        let _ = std::fs::remove_file(&db_path);
    });
}

#[test]
fn handle_auth_required_cooldown_blocks_repeat() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR2: AtomicU64 = AtomicU64::new(0);
        let n = CTR2.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_labrt_auth_cooldown_{}_{n}.db",
            std::process::id()
        ));
        let db = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        let pane = frankenterm_core::storage::PaneRecord {
            pane_id: 89,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        db.upsert_pane(pane).await.expect("insert pane");

        let audit = frankenterm_core::storage::AuditActionRecord {
            id: 0,
            ts: now_ms(),
            actor_kind: "workflow".to_string(),
            actor_id: Some("test-exec-2".to_string()),
            correlation_id: None,
            pane_id: Some(89),
            domain: None,
            action_kind: "auth_required".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "recorded".to_string(),
        };
        db.record_audit_action(audit).await.expect("record");

        let since = now_ms() - AUTH_COOLDOWN_MS;
        let query = frankenterm_core::storage::AuditQuery {
            pane_id: Some(89),
            action_kind: Some("auth_required".to_string()),
            since: Some(since),
            limit: Some(1),
            ..Default::default()
        };
        let results = db.get_audit_actions(query).await.expect("query");
        assert!(
            !results.is_empty(),
            "Should find recent auth event within cooldown window"
        );

        let _ = std::fs::remove_file(&db_path);
    });
}
