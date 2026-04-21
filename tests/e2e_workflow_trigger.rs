use assert_cmd::Command;
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::policy::{PolicyEngine, PolicyGatedInjector};
use frankenterm_core::runtime_compat::CompatRuntime;
use frankenterm_core::storage::{EventQuery, PaneRecord, StorageHandle, StoredEvent};
use frankenterm_core::wezterm::default_wezterm_handle;
use frankenterm_core::workflows::{
    BoxFuture, CxPolicyInjector, PaneWorkflowLockManager, StepResult, Workflow, WorkflowContext,
    WorkflowEngine, WorkflowRunner, WorkflowRunnerConfig, WorkflowStep,
};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

const RULE_ID: &str = "e2e.workflow_trigger";
const WORKFLOW_NAME: &str = "e2e_workflow_trigger";
const MATCHED_TEXT: &str = "workflow trigger detected";

struct JsonlEvidence {
    path: PathBuf,
}

impl JsonlEvidence {
    fn new(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create evidence dir");
        }
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn log(&self, phase: &str, message: &str, details: Value) {
        let line = json!({
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "component": "tests.e2e_workflow_trigger",
            "phase": phase,
            "message": message,
            "details": details,
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .expect("open evidence log");
        writeln!(
            file,
            "{}",
            serde_json::to_string(&line).expect("serialize evidence")
        )
        .expect("write evidence line");
    }
}

struct Scenario {
    tempdir: TempDir,
    workspace_root: PathBuf,
    evidence: JsonlEvidence,
    execution_id: String,
}

struct ImmediateWorkflow;

impl Workflow for ImmediateWorkflow {
    fn name(&self) -> &'static str {
        WORKFLOW_NAME
    }

    fn description(&self) -> &'static str {
        "Real test workflow that completes immediately for e2e trigger coverage"
    }

    fn handles(&self, detection: &Detection) -> bool {
        detection.rule_id == RULE_ID
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![WorkflowStep::new(
            "complete",
            "Persist a completed workflow execution",
        )]
    }

    fn execute_step(
        &self,
        _ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        Box::pin(async move {
            match step_idx {
                0 => StepResult::done(json!({ "workflow": WORKFLOW_NAME, "completed": true })),
                _ => StepResult::abort("unexpected workflow step"),
            }
        })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis() as i64
}

fn workspace_db_path(root: &Path) -> PathBuf {
    root.join(".ft").join("ft.db")
}

async fn upsert_test_pane(storage: &StorageHandle, pane_id: u64) {
    storage
        .upsert_pane(PaneRecord {
            pane_id,
            pane_uuid: Some(format!("pane-{pane_id}")),
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("e2e test pane".to_string()),
            cwd: Some("/tmp".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        })
        .await
        .expect("upsert pane");
}

fn build_detection() -> Detection {
    Detection {
        rule_id: RULE_ID.to_string(),
        agent_type: AgentType::Codex,
        event_type: "pattern_detected".to_string(),
        severity: Severity::Warning,
        confidence: 1.0,
        extracted: json!({
            "needle": "workflow trigger",
            "source": "e2e",
        }),
        matched_text: MATCHED_TEXT.to_string(),
        span: (0, MATCHED_TEXT.len()),
    }
}

async fn setup_real_trigger_flow() -> Scenario {
    let tempdir = tempfile::tempdir().expect("create temp workspace");
    let workspace_root = tempdir.path().join("workspace");
    fs::create_dir_all(workspace_root.join(".ft")).expect("create .ft directory");

    let evidence = JsonlEvidence::new(
        workspace_root
            .join("artifacts")
            .join("e2e_workflow_trigger.jsonl"),
    );
    evidence.log(
        "setup",
        "initializing real workflow-trigger e2e scenario",
        json!({
            "workspace_root": workspace_root,
        }),
    );

    let db_path = workspace_db_path(&workspace_root);
    let storage = Arc::new(
        StorageHandle::new(db_path.to_str().expect("db path should be valid utf-8"))
            .await
            .expect("open real sqlite storage"),
    );
    let pane_id = 101u64;
    upsert_test_pane(storage.as_ref(), pane_id).await;
    evidence.log(
        "storage",
        "real sqlite storage initialized and pane inserted",
        json!({
            "db_path": db_path,
            "pane_id": pane_id,
        }),
    );

    let injector = CxPolicyInjector::new(PolicyGatedInjector::new(
        PolicyEngine::permissive(),
        default_wezterm_handle(),
    ));
    let runner = WorkflowRunner::new(
        WorkflowEngine::default(),
        Arc::new(PaneWorkflowLockManager::new()),
        Arc::clone(&storage),
        injector,
        WorkflowRunnerConfig::default(),
    );
    runner.register_workflow(Arc::new(ImmediateWorkflow));
    evidence.log(
        "workflow_registration",
        "registered real workflow implementation",
        json!({
            "workflow_name": WORKFLOW_NAME,
        }),
    );

    let detection = build_detection();
    let stored_event = StoredEvent {
        id: 0,
        pane_id,
        rule_id: detection.rule_id.clone(),
        agent_type: detection.agent_type.to_string(),
        event_type: detection.event_type.clone(),
        severity: "warning".to_string(),
        confidence: detection.confidence,
        extracted: Some(detection.extracted.clone()),
        matched_text: Some(detection.matched_text.clone()),
        segment_id: None,
        detected_at: now_ms(),
        dedupe_key: Some(format!("e2e-trigger-{pane_id}-{}", now_ms())),
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    };
    let request_cx = frankenterm_core::cx::for_request();
    let event_id = storage
        .record_event_with_cx(&request_cx, stored_event)
        .await
        .expect("record real pattern-detected event");
    evidence.log(
        "event_recorded",
        "persisted real pattern-detected event",
        json!({
            "event_id": event_id,
            "rule_id": RULE_ID,
        }),
    );

    let start = runner
        .handle_detection_with_cx(&request_cx, pane_id, &detection, Some(event_id))
        .await;
    let execution_id = match start {
        frankenterm_core::workflows::WorkflowStartResult::Started { execution_id, .. } => {
            execution_id
        }
        other => panic!("workflow did not start: {other:?}"),
    };
    evidence.log(
        "workflow_started",
        "workflow runner accepted detection and created execution",
        json!({
            "execution_id": execution_id,
            "event_id": event_id,
        }),
    );

    let workflow = runner
        .find_workflow_by_name(WORKFLOW_NAME)
        .expect("registered workflow should be discoverable");
    let result = runner
        .run_workflow_with_cx(&request_cx, pane_id, workflow, &execution_id, 0)
        .await;
    assert!(
        result.is_completed(),
        "workflow execution should complete successfully: {result:?}"
    );
    evidence.log(
        "workflow_completed",
        "workflow runner completed execution",
        json!({
            "execution_id": execution_id,
            "result": result,
        }),
    );

    let mut handled_event = None;
    for _ in 0..20 {
        let events = storage
            .get_events_with_cx(
                &request_cx,
                EventQuery {
                    limit: Some(5),
                    pane_id: Some(pane_id),
                    rule_id: Some(RULE_ID.to_string()),
                    event_type: None,
                    triage_state: None,
                    label: None,
                    unhandled_only: false,
                    since: None,
                    until: None,
                },
            )
            .await
            .expect("query events");
        handled_event = events
            .into_iter()
            .find(|event| event.id == event_id && event.handled_by_workflow_id.is_some());
        if handled_event.is_some() {
            break;
        }
        frankenterm_core::runtime_compat::sleep(Duration::from_millis(25)).await;
    }
    let handled_event = handled_event.expect("event should be marked handled by the workflow");
    assert_eq!(
        handled_event.handled_by_workflow_id.as_deref(),
        Some(execution_id.as_str())
    );
    assert_eq!(handled_event.handled_status.as_deref(), Some("completed"));
    evidence.log(
        "storage_verified",
        "sqlite event row shows workflow handling",
        json!({
            "event_id": handled_event.id,
            "workflow_id": handled_event.handled_by_workflow_id,
            "handled_status": handled_event.handled_status,
        }),
    );

    storage.shutdown().await.expect("shutdown storage cleanly");
    evidence.log(
        "shutdown",
        "storage shutdown complete before CLI verification",
        json!({
            "db_path": db_path,
        }),
    );

    Scenario {
        tempdir,
        workspace_root,
        evidence,
        execution_id,
    }
}

#[test]
#[allow(deprecated)]
fn workflow_trigger_end_to_end_via_real_services() {
    let runtime = frankenterm_core::runtime_compat::RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build runtime");

    let scenario = runtime.block_on(setup_real_trigger_flow());
    let _keep_tempdir_alive = &scenario.tempdir;

    let output = Command::cargo_bin("ft")
        .expect("ft binary should build for integration test")
        .env("FT_WORKSPACE", &scenario.workspace_root)
        .args([
            "robot",
            "--format",
            "json",
            "events",
            "--rule-id",
            RULE_ID,
            "--limit",
            "5",
        ])
        .output()
        .expect("run ft robot events");

    scenario.evidence.log(
        "robot_events_cli",
        "invoked real ft robot events process",
        json!({
            "status": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }),
    );

    assert!(
        output.status.success(),
        "ft robot events should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: Value = serde_json::from_slice(&output.stdout).expect("parse robot events json");
    assert_eq!(
        parsed["ok"],
        Value::Bool(true),
        "robot response should succeed"
    );

    let events = parsed["data"]["events"]
        .as_array()
        .expect("robot events payload should contain an array");
    assert!(
        !events.is_empty(),
        "robot events should return the persisted event"
    );

    let event = events
        .iter()
        .find(|event| event.get("rule_id").and_then(Value::as_str) == Some(RULE_ID))
        .expect("rule-filtered robot events should include the persisted event");
    assert_eq!(
        event["event_type"],
        Value::String("pattern_detected".to_string())
    );
    assert_eq!(
        event["workflow_id"],
        Value::String(scenario.execution_id.clone())
    );
    assert!(
        !event["handled_at"].is_null(),
        "workflow-triggered event should be marked handled"
    );

    eprintln!(
        "[ARTIFACT][e2e_workflow_trigger] {}",
        scenario.evidence.path().display()
    );
}
