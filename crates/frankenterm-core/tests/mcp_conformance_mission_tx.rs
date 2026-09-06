#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkTestClient, FrameworkTool, framework_create_memory_transport_pair,
};
use frankenterm_core::plan::{
    ApprovalState, Assignment, AssignmentId, CandidateAction, CandidateActionId, Mission,
    MissionActorRole, MissionId, MissionLifecycleState, MissionOwnership, MissionTxContract,
    MissionTxState, Outcome, StepAction, TxCompensation, TxId, TxIntent, TxOutcome, TxPlan,
    TxPlanId, TxPrecondition, TxStep, TxStepId,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

#[cfg(all(unix, feature = "vendored"))]
#[path = "common/wezterm_subprocess.rs"]
mod owned_mux;

struct TestHarness {
    client: FrameworkTestClient,
    #[cfg(all(unix, feature = "vendored"))]
    live: Option<LiveTxFixture>,
    workspace: tempfile::TempDir,
    _workspace_guard: WorkspaceRootGuard,
    // Fields drop in declaration order: restore cwd before another test can
    // acquire this process-wide lock and enter its own workspace.
    _cwd_lock: MutexGuard<'static, ()>,
}

#[derive(Serialize)]
struct ToolGoldenCapture {
    #[serde(skip_serializing)]
    workspace_root: PathBuf,
    tool: String,
    input_schema: Value,
    success_envelope: Value,
    invalid_args_envelope: Value,
}

struct WorkspaceRootGuard {
    previous_cwd: PathBuf,
}

impl WorkspaceRootGuard {
    fn new(workspace_root: &Path) -> Self {
        let previous_cwd = std::env::current_dir().expect("capture current dir");
        std::env::set_current_dir(workspace_root).expect("enter conformance workspace root");
        Self { previous_cwd }
    }
}

impl Drop for WorkspaceRootGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.previous_cwd).expect("restore current dir");
    }
}

fn workspace_root_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_client(config: &Config, db_path: Option<PathBuf>) -> FrameworkTestClient {
    let server = build_server_with_db(config, db_path).expect("build MCP server");
    let (client_transport, server_transport) = framework_create_memory_transport_pair();
    std::thread::spawn(move || {
        server.run_transport_returning(server_transport);
    });

    let mut client = FrameworkTestClient::new(client_transport);
    client
        .initialize()
        .expect("initialize in-memory MCP client");
    client
}

fn new_harness() -> TestHarness {
    let cwd_lock = workspace_root_lock();
    let workspace = tempfile::Builder::new()
        .disable_cleanup(true)
        .tempdir()
        .expect("create retained temp workspace");
    fs::create_dir_all(workspace.path().join(".ft/mission")).expect("create mission dir");
    let workspace_guard = WorkspaceRootGuard::new(workspace.path());
    let mut config = Config::default();
    config.safety.require_prompt_active = false;
    config.general.workspace = Some(workspace.path().display().to_string());
    let client = spawn_client(
        &config,
        Some(config.workspace_layout(None).unwrap().db_path),
    );
    TestHarness {
        #[cfg(all(unix, feature = "vendored"))]
        live: None,
        _cwd_lock: cwd_lock,
        _workspace_guard: workspace_guard,
        workspace,
        client,
    }
}

/// Uses the actual mux, observation runtime, persisted capture and IPC server.
/// No pane records or capabilities are planted. The explicit mux artifact must
/// be built from the same source by the invoking RCH/DSR proof lane.
#[cfg(all(unix, feature = "vendored"))]
struct LiveTxFixture {
    runtime: frankenterm_core::runtime_async::Runtime,
    observer: Option<frankenterm_core::runtime::RuntimeHandle>,
    ipc_task: Option<frankenterm_core::watchdog::WatchdogHandle>,
    ipc_shutdown: frankenterm_core::runtime_async::mpsc::Sender<()>,
    mux: owned_mux::WeztermSubprocessFixture,
    pane_ids: [u64; 2],
}

#[cfg(all(unix, feature = "vendored"))]
impl LiveTxFixture {
    fn start(config: &mut Config) -> Self {
        use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder, RwLock, mpsc};
        use std::sync::Arc;
        use std::time::Duration;

        // Disable terminal echo so readback proves the PTY program consumed
        // each complete command, rather than merely observing input echo.
        let mux = owned_mux::WeztermSubprocessFixture::spawn_with_default_prog(&[
            "/bin/sh",
            "-c",
            "stty -echo; printf 'TX_READY\\n'; while IFS= read -r line; do printf 'TX_ACK:%s\\n' \"$line\"; done",
        ])
        .expect("same-source owned mux artifact required");
        config.vendored.mux_socket_path = Some(mux.socket_path().display().to_string());
        let layout = config.workspace_layout(None).unwrap();
        let runtime = RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build live transaction runtime");
        let (observer, ipc_task, ipc_shutdown, pane_ids) = runtime.block_on(async {
            let cx = frankenterm_core::cx::Cx::current().expect("runtime-owned context");
            let client = mux.client();
            let panes = client.list_panes_with_cx(&cx).await.unwrap();
            assert_eq!(panes.len(), 1, "owned initial pane");
            let second = client.spawn_with_cx(&cx, None, None).await.unwrap();
            let pane_ids = [panes[0].pane_id, second];
            assert_ne!(pane_ids[0], pane_ids[1]);
            let storage = frankenterm_core::storage::StorageHandle::new_with_cx(
                &cx,
                layout.db_path.to_str().unwrap(),
            )
            .await
            .unwrap();
            let event_bus = Arc::new(frankenterm_core::events::EventBus::new(64));
            let server =
                frankenterm_core::ipc::IpcServer::bind_with_cx(&cx, &layout.ipc_socket_path)
                    .await
                    .unwrap();
            let mut observation = frankenterm_core::runtime::ObservationRuntime::new(
                frankenterm_core::runtime::RuntimeConfig {
                    discovery_interval: Duration::from_millis(50),
                    capture_interval: Duration::from_millis(50),
                    min_capture_interval: Duration::from_millis(25),
                    vendored_mux_socket_paths: vec![mux.socket_path().to_path_buf()],
                    ..Default::default()
                },
                storage,
                Arc::new(RwLock::new(frankenterm_core::patterns::PatternEngine::new())),
            )
            .with_wezterm_handle(mux.handle())
            .with_event_bus(Arc::clone(&event_bus));
            let observer = observation.start_with_cx(&cx).await.unwrap();
            let registry = Arc::clone(&observer.registry);
            let (ipc_shutdown, shutdown_rx) = mpsc::channel(1);
            let ipc_task = frankenterm_core::runtime_async::task::spawn(async move {
                server
                    .run_with_registry_with_cx(&cx, event_bus, registry, shutdown_rx)
                    .await;
            });
            let ipc_task = frankenterm_core::watchdog::WatchdogHandle::adopt_shutdown_task(
                ipc_task,
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            );
            (observer, ipc_task, ipc_shutdown, pane_ids)
        });
        let fixture = Self {
            runtime,
            observer: Some(observer),
            ipc_task: Some(ipc_task),
            ipc_shutdown,
            mux,
            pane_ids,
        };
        fixture.wait_for_ready(&layout.ipc_socket_path);
        fixture
    }

    fn wait_for_ready(&self, socket: &Path) {
        use frankenterm_core::runtime_async::{CompatRuntime, sleep_with_cx};
        use std::time::{Duration, Instant};
        self.runtime.block_on(async {
            let cx = frankenterm_core::cx::Cx::current().unwrap();
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut ipc = frankenterm_core::ipc::IpcClient::new(socket);
            ipc.set_token(None);
            let storage = &self.observer.as_ref().unwrap().storage;
            for pane_id in self.pane_ids {
                loop {
                    let state = ipc.pane_state_with_cx(&cx, pane_id).await.unwrap();
                    let data = state.data.as_ref().unwrap();
                    let segments = storage.get_segments(pane_id, 16).await.unwrap();
                    if state.ok
                        && data["known"] == true
                        && data["observed"] == true
                        && data["in_gap"] == false
                        && (data["cursor_alt_screen"] == false
                            || (data["last_status_at"].is_number() && data["alt_screen"] == false))
                        && segments
                            .iter()
                            .any(|segment| segment.content.contains("TX_READY"))
                    {
                        break;
                    }
                    assert!(Instant::now() < deadline, "live pane not ready: {state:?}");
                    sleep_with_cx(&cx, Duration::from_millis(25)).await.unwrap();
                }
            }
        });
    }

    fn assert_consumed(&self, step: usize, text: &str, expected_count: usize) {
        use frankenterm_core::runtime_async::{CompatRuntime, sleep_with_cx};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{Duration, Instant};
        static BARRIER_SEQUENCE: AtomicU64 = AtomicU64::new(0);
        self.runtime.block_on(async {
            let cx = frankenterm_core::cx::Cx::current().unwrap();
            let client = self.mux.client();
            // Read a consumer acknowledgement after all earlier PTY input.
            // This makes absence and exactly-once checks causal rather than
            // assuming a sleeping child had enough time to process its queue.
            let barrier = format!(
                "__TX_BARRIER_{}",
                BARRIER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            client
                .send_text_no_paste_with_cx(&cx, self.pane_ids[step], &format!("{barrier}\n"))
                .await
                .unwrap();
            let expected_ack = format!("TX_ACK:{text}");
            let barrier_ack = format!("TX_ACK:{barrier}");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let output = client
                    .get_text_with_cx(&cx, self.pane_ids[step], false)
                    .await
                    .unwrap();
                if output.lines().any(|line| line.trim_end() == barrier_ack) {
                    assert_eq!(
                        output
                            .lines()
                            .filter(|line| line.trim_end() == expected_ack)
                            .count(),
                        expected_count,
                        "PTY input count for {text}: {output:?}"
                    );
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "PTY did not consume {text}: {output:?}"
                );
                sleep_with_cx(&cx, Duration::from_millis(25)).await.unwrap();
            }
        });
    }

    fn finish(&mut self) {
        use frankenterm_core::runtime_async::CompatRuntime;
        let observer = self.observer.take();
        let ipc_task = self.ipc_task.take();
        if observer.is_none() && ipc_task.is_none() {
            return;
        }
        let panicking = std::thread::panicking();
        self.runtime.block_on(async {
            let cx = frankenterm_core::cx::Cx::current().unwrap();
            let signal = self.ipc_shutdown.try_send(());
            let ipc_result = match ipc_task {
                Some(task) => task.join_with_cx(&cx).await,
                None => Ok(()),
            };
            let summary = match observer {
                Some(observer) => Some(observer.shutdown_with_summary_with_cx(&cx).await),
                None => None,
            };
            eprintln!(
                "LIVE_TX_SETTLEMENT signal={signal:?} ipc={ipc_result:?} runtime={summary:?}"
            );
            if !panicking {
                assert!(ipc_result.is_ok(), "IPC owner did not settle");
                assert!(summary.as_ref().is_some_and(|summary| summary.is_clean()));
            }
        });
        if !panicking {
            self.mux.kill_mux();
        }
    }
}

#[cfg(all(unix, feature = "vendored"))]
impl Drop for LiveTxFixture {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(all(unix, feature = "vendored"))]
fn enable_live_tx(harness: &mut TestHarness) {
    let mut config = Config::default();
    config.general.workspace = Some(harness.workspace.path().display().to_string());
    // The owned program is a line-reading application, not a shell prompt.
    config.safety.require_prompt_active = false;
    let live = LiveTxFixture::start(&mut config);
    harness.client = spawn_client(
        &config,
        Some(config.workspace_layout(None).unwrap().db_path),
    );
    harness.live = Some(live);
}

fn mission_file_path(workspace: &Path) -> PathBuf {
    workspace.join(".ft/mission/active.json")
}

fn tx_file_path(workspace: &Path) -> PathBuf {
    workspace.join(".ft/mission/tx-active.json")
}

fn write_json<T: Serialize>(path: &Path, value: &T) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    let text = serde_json::to_string_pretty(value).expect("serialize fixture");
    fs::write(path, text).expect("write fixture");
}

fn tool_input_schema(client: &mut FrameworkTestClient, tool_name: &str) -> Value {
    client
        .list_tools()
        .expect("list tools")
        .into_iter()
        .find(|tool| tool.name == tool_name)
        .map(|tool: FrameworkTool| tool.input_schema)
        .unwrap_or_else(|| panic!("missing tool {tool_name}"))
}

fn manifest_tool_schema(tool_name: &str) -> Value {
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mcp_manifest.json");
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path).expect("read mcp manifest fixture"),
    )
    .expect("parse mcp manifest fixture");
    manifest["tools"]
        .as_array()
        .expect("manifest tools array")
        .iter()
        .find(|tool| tool["name"] == tool_name)
        .and_then(|tool| tool.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| panic!("missing manifest schema for {tool_name}"))
}

fn first_text_content(contents: &[FrameworkContent]) -> &str {
    contents
        .first()
        .and_then(|content| match content {
            FrameworkContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("expected first MCP content to be text")
}

fn parse_tool_envelope(contents: &[FrameworkContent]) -> Value {
    serde_json::from_str(first_text_content(contents)).expect("parse JSON envelope")
}

fn canonicalize(value: &mut Value, workspace_root: &Path) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    // Schema objects are authority, never response noise.
                    "input_schema" => {}
                    "now" | "elapsed_ms" if child.is_number() => *child = Value::from(0_u64),
                    "mission_file" if child.is_string() => {
                        *child = Value::String("<mission_file>".to_string());
                    }
                    "contract_file" if child.is_string() => {
                        *child = Value::String("<contract_file>".to_string());
                    }
                    "mission_hash" | "content_sha256" if child.is_string() => {
                        *child = Value::String("<verified_content_hash>".to_string())
                    }
                    "checkpoint_id" if child.is_string() => {
                        *child = Value::String("<checkpoint_id>".to_string());
                    }
                    _ if key.ends_with("_ms") && child.is_number() => *child = Value::from(0_i64),
                    _ => canonicalize(child, workspace_root),
                }
            }

            let mut sorted = std::collections::BTreeMap::new();
            for (key, child) in std::mem::take(map) {
                sorted.insert(key, child);
            }
            let mut rebuilt = Map::new();
            for (key, child) in sorted {
                rebuilt.insert(key, child);
            }
            *map = rebuilt;
        }
        Value::Array(items) => {
            for item in items {
                canonicalize(item, workspace_root);
            }
        }
        Value::String(text) => {
            let workspace_root = workspace_root.display().to_string();
            let private_workspace_root = format!("/private{workspace_root}");
            if text.contains(&private_workspace_root) {
                *text = text.replace(&private_workspace_root, "<workspace_root>");
            }
            if text.contains(&workspace_root) {
                *text = text.replace(&workspace_root, "<workspace_root>");
            }
        }
        _ => {}
    }
}

fn pretty_canonical(value: &Value, workspace_root: &Path) -> String {
    let mut cloned = value.clone();
    canonicalize(&mut cloned, workspace_root);
    format!(
        "{}\n",
        serde_json::to_string_pretty(&cloned).expect("serialize canonical JSON")
    )
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden_robot_envelope")
        .join(format!("{name}.json"))
}

fn read_or_update_golden(path: &Path, actual: &str) -> String {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create golden dir");
        }
        fs::write(path, actual).expect("write golden");
        return actual.to_string();
    }

    fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing MCP conformance golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test mcp_conformance_mission_tx \
             --features mcp,asupersync-runtime",
            path.display()
        )
    })
}

fn assert_matches_golden(name: &str, capture: &ToolGoldenCapture) {
    let actual_value = serde_json::to_value(capture).expect("serialize capture");
    let actual_text = pretty_canonical(&actual_value, &capture.workspace_root);
    if std::env::var("DEBUG_GOLDEN_CAPTURE").is_ok() {
        eprintln!("DEBUG_GOLDEN_CAPTURE {name}\n{actual_text}");
    }
    let path = golden_path(name);
    let expected = read_or_update_golden(&path, &actual_text);

    if expected.trim_end_matches('\n') != actual_text.trim_end_matches('\n') {
        let capture_dir = tempfile::Builder::new()
            .prefix("ft-mcp-mission-tx-capture-")
            .tempdir()
            .expect("create retained conformance capture directory")
            .keep();
        let actual_path = capture_dir.join(format!("{name}.actual.json"));
        fs::write(&actual_path, &actual_text).expect("retain actual conformance capture");
        panic!(
            "MCP mission/tx golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test mcp_conformance_mission_tx \
             --features mcp,asupersync-runtime",
            path.display(),
            actual_path.display()
        );
    }
}

fn assert_schema_matches_manifest(tool_name: &str, actual_schema: &Value) {
    let expected_schema = manifest_tool_schema(tool_name);
    assert_eq!(
        actual_schema, &expected_schema,
        "schema drift vs tests/fixtures/mcp_manifest.json for {tool_name}"
    );
}

#[test]
fn mission_tx_canonicalization_preserves_schemas_and_malformed_response_types() {
    let mut schema = manifest_tool_schema("wa.mission_pause");
    schema["properties"]["mission_file"]["maxLength"] = json!(3);
    let mut capture = json!({
        "input_schema": schema.clone(),
        "success_envelope": {"data": {
            "revision": "9007199254740993",
            "mission_file": {"invalid": true},
            "contract_file": ["invalid"],
            "content_sha256": 7,
            "checkpoint_id": false,
            "created_at_ms": "invalid",
            "updated_at_ms": null,
            "elapsed_ms": "invalid"
        }}
    });
    let original = capture.clone();
    canonicalize(&mut capture, Path::new("/owned-workspace"));
    assert_eq!(capture, original);
    assert_ne!(schema, manifest_tool_schema("wa.mission_pause"));
}

fn make_candidate(id: &str, pane_id: u64, text: &str, created_at_ms: i64) -> CandidateAction {
    CandidateAction {
        candidate_id: CandidateActionId(id.to_string()),
        requested_by: MissionActorRole::Planner,
        action: StepAction::SendText {
            pane_id,
            text: text.to_string(),
            paste_mode: Some(false),
        },
        rationale: format!("dispatch {text}"),
        score: Some(0.95),
        created_at_ms,
    }
}

fn make_assignment(
    assignment_id: &str,
    candidate_id: &str,
    assignee: &str,
    approval_state: ApprovalState,
    outcome: Option<Outcome>,
    created_at_ms: i64,
) -> Assignment {
    Assignment {
        assignment_id: AssignmentId(assignment_id.to_string()),
        candidate_id: CandidateActionId(candidate_id.to_string()),
        assigned_by: MissionActorRole::Dispatcher,
        assignee: assignee.to_string(),
        reservation_intent: None,
        approval_state,
        outcome,
        escalation: None,
        created_at_ms,
        updated_at_ms: None,
    }
}

fn make_running_mission() -> Mission {
    let mut mission = Mission::new(
        MissionId("mission:qiba0".to_string()),
        "Mission MCP Conformance",
        "ws-qiba0",
        MissionOwnership {
            planner: "planner-a".to_string(),
            dispatcher: "dispatcher-a".to_string(),
            operator: "operator-a".to_string(),
        },
        1_700_000_000_000,
    );
    mission.generation = "0123456789abcdef0123456789abcdef".to_string();
    mission.lifecycle_state = MissionLifecycleState::Running;
    mission.candidates = vec![
        make_candidate("candidate:alpha", 1, "/approve alpha", 1_700_000_000_010),
        make_candidate("candidate:beta", 2, "/run beta", 1_700_000_000_020),
    ];
    mission.assignments = vec![
        make_assignment(
            "assignment:alpha",
            "candidate:alpha",
            "agent-alpha",
            ApprovalState::Pending {
                requested_by: "dispatcher-a".to_string(),
                requested_at_ms: 1_700_000_000_100,
            },
            None,
            1_700_000_000_110,
        ),
        make_assignment(
            "assignment:beta",
            "candidate:beta",
            "agent-beta",
            ApprovalState::Approved {
                approved_by: "operator-a".to_string(),
                approved_at_ms: 1_700_000_000_120,
                approval_code_hash: "sha256:approved".to_string(),
            },
            Some(Outcome::Success {
                reason_code: "step_completed".to_string(),
                completed_at_ms: 1_700_000_000_130,
            }),
            1_700_000_000_115,
        ),
    ];
    mission
}

fn make_paused_mission() -> Mission {
    let mut mission = make_running_mission();
    mission
        .pause_mission("operator-a", "maintenance_window", 1_700_000_000_200, None)
        .expect("pause seed mission");
    mission
}

fn make_cancelled_mission() -> Mission {
    let mut mission = make_running_mission();
    mission
        .abort_mission(
            "operator-a",
            "operator_abort",
            Some("mission.failure.manual_abort".to_string()),
            1_700_000_000_300,
            None,
        )
        .expect("abort seed mission");
    mission
}

fn make_tx_contract() -> MissionTxContract {
    let tx_id = TxId("tx:qiba0".to_string());
    MissionTxContract {
        tx_version: frankenterm_core::plan::MISSION_TX_SCHEMA_VERSION,
        intent: TxIntent {
            tx_id: tx_id.clone(),
            requested_by: MissionActorRole::Dispatcher,
            summary: "qiba0 tx contract".to_string(),
            correlation_id: "corr-qiba0".to_string(),
            created_at_ms: 1_700_000_001_000,
        },
        plan: TxPlan {
            plan_id: TxPlanId("tx-plan:qiba0".to_string()),
            tx_id,
            steps: vec![
                TxStep {
                    step_id: TxStepId("tx-step:1".to_string()),
                    ordinal: 1,
                    action: StepAction::SendText {
                        pane_id: 11,
                        text: "/do-step-1".to_string(),
                        paste_mode: Some(false),
                    },
                    description: "prepare alpha".to_string(),
                },
                TxStep {
                    step_id: TxStepId("tx-step:2".to_string()),
                    ordinal: 2,
                    action: StepAction::SendText {
                        pane_id: 12,
                        text: "/do-step-2".to_string(),
                        paste_mode: Some(true),
                    },
                    description: "commit beta".to_string(),
                },
            ],
            preconditions: vec![TxPrecondition::PromptActive { pane_id: 11 }],
            compensations: vec![
                TxCompensation {
                    for_step_id: TxStepId("tx-step:1".to_string()),
                    action: StepAction::SendText {
                        pane_id: 11,
                        text: "/undo-step-1".to_string(),
                        paste_mode: Some(false),
                    },
                },
                TxCompensation {
                    for_step_id: TxStepId("tx-step:2".to_string()),
                    action: StepAction::SendText {
                        pane_id: 12,
                        text: "/undo-step-2".to_string(),
                        paste_mode: Some(true),
                    },
                },
            ],
        },
        lifecycle_state: MissionTxState::Planned,
        outcome: TxOutcome::Pending,
        receipts: Vec::new(),
    }
}

fn assert_mission_response_authority(envelope: &Value, before: &Mission, path: &Path) {
    use frankenterm_core::tx_execution::MissionRevisionToken;
    let after: Mission = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    after.validate().unwrap();
    let data = &envelope["data"];
    if let Some(hash) = data.get("mission_hash") {
        assert_eq!(hash, &Value::String(after.compute_hash()));
    }
    let current =
        serde_json::to_value(MissionRevisionToken::from_mission(&after).unwrap()).unwrap();
    if let Some(mutation) = data.get("mutation") {
        assert_eq!(
            mutation["previous"],
            serde_json::to_value(MissionRevisionToken::from_mission(before).unwrap()).unwrap()
        );
        assert_eq!(mutation["current"], current);
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.revision, before.revision + 1);
        assert_eq!(mutation["changed"], true);
        assert_eq!(mutation["durability"], "file_and_directory_synced");
        assert_eq!(
            mutation["owner_acknowledgement"],
            "unavailable_no_mission_driver"
        );
        if let Some(checkpoint_id) = data.get("checkpoint_id").and_then(Value::as_str) {
            let checkpoint = after
                .pause_resume_state
                .current_checkpoint
                .as_ref()
                .or_else(|| after.pause_resume_state.checkpoint_history.last())
                .unwrap();
            assert_eq!(checkpoint_id, checkpoint.checkpoint_id);
        }
    } else {
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        if let Some(token) = data.get("revision_token") {
            assert_eq!(token, &current);
        }
    }
}

fn capture_tool_contract(
    tool_name: &str,
    success_setup: impl FnOnce(&mut TestHarness),
    success_args: impl FnOnce(&TestHarness) -> Value,
    invalid_setup: impl FnOnce(&mut TestHarness),
    invalid_args: impl FnOnce(&TestHarness) -> Value,
) -> ToolGoldenCapture {
    let mut harness = new_harness();
    #[cfg(all(unix, feature = "vendored"))]
    if matches!(tool_name, "wa.tx_run" | "wa.tx_rollback") {
        enable_live_tx(&mut harness);
    }
    success_setup(&mut harness);
    let mission_path = mission_file_path(harness.workspace.path());
    let before = tool_name
        .starts_with("wa.mission_")
        .then(|| serde_json::from_slice::<Mission>(&fs::read(&mission_path).unwrap()).unwrap());
    let input_schema = tool_input_schema(&mut harness.client, tool_name);
    assert_schema_matches_manifest(tool_name, &input_schema);
    let success_args = success_args(&harness);
    let success_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(tool_name, success_args)
            .unwrap_or_else(|err| panic!("call {tool_name} success case: {err}")),
    );
    if let Some(before) = &before {
        assert_mission_response_authority(&success_envelope, before, &mission_path);
    }
    #[cfg(all(unix, feature = "vendored"))]
    if let Some(live) = &harness.live {
        assert_eq!(success_envelope["ok"], true, "{success_envelope}");
        live.assert_consumed(0, "/do-step-1", 1);
        live.assert_consumed(0, "/undo-step-1", 1);
        if tool_name == "wa.tx_rollback" {
            live.assert_consumed(1, "/do-step-2", 1);
            live.assert_consumed(1, "/undo-step-2", 1);
        } else {
            live.assert_consumed(1, "/do-step-2", 0);
            live.assert_consumed(1, "/undo-step-2", 0);
        }
    }

    invalid_setup(&mut harness);
    let invalid_args = invalid_args(&harness);
    let invalid_args_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(tool_name, invalid_args)
            .unwrap_or_else(|err| panic!("call {tool_name} invalid case: {err}")),
    );

    ToolGoldenCapture {
        workspace_root: harness.workspace.path().to_path_buf(),
        tool: tool_name.to_string(),
        input_schema,
        success_envelope,
        invalid_args_envelope,
    }
}

fn seed_running_mission(harness: &TestHarness) {
    write_json(
        &mission_file_path(harness.workspace.path()),
        &make_running_mission(),
    );
}

fn seed_paused_mission(harness: &TestHarness) {
    write_json(
        &mission_file_path(harness.workspace.path()),
        &make_paused_mission(),
    );
}

fn seed_cancelled_mission(harness: &TestHarness) {
    write_json(
        &mission_file_path(harness.workspace.path()),
        &make_cancelled_mission(),
    );
}

fn seed_planned_tx(harness: &TestHarness) {
    let contract = make_tx_contract();
    #[cfg(all(unix, feature = "vendored"))]
    let contract = if let Some(live) = &harness.live {
        let mut contract = contract;
        // App input deliberately has no shell-prompt precondition. Discovery,
        // reservation, capture continuity, alt-screen and policy checks remain
        // the real production preparation path.
        contract.plan.preconditions.clear();
        for (index, step) in contract.plan.steps.iter_mut().enumerate() {
            let StepAction::SendText { pane_id, text, .. } = &mut step.action else {
                panic!("expected send-text step");
            };
            *pane_id = live.pane_ids[index];
            text.push('\n');
        }
        for (index, compensation) in contract.plan.compensations.iter_mut().enumerate() {
            let StepAction::SendText { pane_id, text, .. } = &mut compensation.action else {
                panic!("expected send-text compensation");
            };
            *pane_id = live.pane_ids[index];
            text.push('\n');
        }
        contract
    } else {
        contract
    };
    write_json(&tx_file_path(harness.workspace.path()), &contract);
}

#[cfg(all(unix, feature = "vendored"))]
fn seed_committed_tx(harness: &mut TestHarness) {
    seed_planned_tx(harness);
    let contents = harness
        .client
        .call_tool(
            "wa.tx_run",
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
            }),
        )
        .expect("seed tx_run success");
    let envelope = parse_tool_envelope(&contents);
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(envelope["data"]["final_state"], "committed", "{envelope}");
    let persisted = read_tx_contract(harness.workspace.path());
    assert_eq!(persisted.lifecycle_state, MissionTxState::Committed);
    assert_eq!(persisted.receipts.len(), 2);
    #[cfg(all(unix, feature = "vendored"))]
    if let Some(live) = &harness.live {
        live.assert_consumed(0, "/do-step-1", 1);
        live.assert_consumed(1, "/do-step-2", 1);
    }
}

fn read_tx_contract(workspace: &Path) -> MissionTxContract {
    serde_json::from_str(
        &fs::read_to_string(tx_file_path(workspace)).expect("read persisted tx contract"),
    )
    .expect("parse persisted tx contract")
}

fn log_tx_roundtrip_phase(phase: &str, envelope: &Value, persisted: &MissionTxContract) {
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "test": "mcp_wa_tx_roundtrip_plan_run_rollback_persists_expected_state",
            "phase": phase,
            "envelope": envelope,
            "persisted_state": persisted.lifecycle_state,
            "persisted_outcome": persisted.outcome,
            "receipt_count": persisted.receipts.len(),
        }))
        .expect("serialize tx roundtrip log")
    );
}

#[test]
fn mcp_conformance_wa_mission_state_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_state",
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "limit": 10
            })
        },
        |_| {},
        |_harness| {
            json!({
                "format": "json",
                "mission_file": "missing-mission.json",
                "limit": 10
            })
        },
    );
    assert_matches_golden("wa_mission_state", &capture);
}

#[test]
fn mcp_conformance_wa_mission_explain_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_explain",
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "assignment_id": "assignment:alpha"
            })
        },
        |_| {},
        |_harness| {
            json!({
                "format": "json",
                "mission_file": "missing-mission.json",
                "assignment_id": "assignment:missing"
            })
        },
    );
    assert_matches_golden("wa_mission_explain", &capture);
}

#[test]
fn mcp_conformance_wa_mission_pause_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_pause",
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "reason": "maintenance_window",
                "requested_by": "operator-a"
            })
        },
        |harness| seed_paused_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "reason": "maintenance_window",
                "requested_by": "operator-a"
            })
        },
    );
    assert_matches_golden("wa_mission_pause", &capture);
}

#[test]
fn mcp_conformance_wa_mission_resume_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_resume",
        |harness| seed_paused_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "requested_by": "operator-a"
            })
        },
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "requested_by": "operator-a"
            })
        },
    );
    assert_matches_golden("wa_mission_resume", &capture);
}

#[test]
fn mcp_conformance_wa_mission_abort_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.mission_abort",
        |harness| seed_running_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "reason": "operator_abort",
                "requested_by": "operator-a",
                "error_code": "mission.failure.manual_abort"
            })
        },
        |harness| seed_cancelled_mission(harness),
        |harness| {
            json!({
                "format": "json",
                "mission_file": mission_file_path(harness.workspace.path()).display().to_string(),
                "reason": "operator_abort",
                "requested_by": "operator-a"
            })
        },
    );
    assert_matches_golden("wa_mission_abort", &capture);
}

#[test]
fn mcp_conformance_wa_tx_plan_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.tx_plan",
        |harness| seed_planned_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
            })
        },
        |_| {},
        |_harness| {
            json!({
                "format": "json",
                "contract_file": "missing-tx.json"
            })
        },
    );
    assert_matches_golden("wa_tx_plan", &capture);
}

#[test]
fn mcp_conformance_wa_tx_show_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.tx_show",
        |harness| seed_planned_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                "include_contract": true
            })
        },
        |_| {},
        |_harness| {
            json!({
                "format": "json",
                "contract_file": "missing-tx.json",
                "include_contract": true
            })
        },
    );
    assert_matches_golden("wa_tx_show", &capture);
}

#[test]
#[cfg(all(unix, feature = "vendored"))]
#[ignore = "requires an explicit same-source FT_WEZTERM_MUX_SERVER artifact under RCH/DSR"]
fn mcp_conformance_wa_tx_run_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.tx_run",
        |harness| seed_planned_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                "fail_step": "tx-step:2"
            })
        },
        |harness| seed_planned_tx(harness),
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                "fail_step": "tx-step:missing"
            })
        },
    );
    assert_matches_golden("wa_tx_run", &capture);
}

#[test]
#[cfg(all(unix, feature = "vendored"))]
#[ignore = "requires an explicit same-source FT_WEZTERM_MUX_SERVER artifact under RCH/DSR"]
fn mcp_conformance_wa_tx_rollback_contract_matches_golden() {
    let capture = capture_tool_contract(
        "wa.tx_rollback",
        seed_committed_tx,
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string()
            })
        },
        |_| {},
        |harness| {
            json!({
                "format": "json",
                "contract_file": tx_file_path(harness.workspace.path()).display().to_string(),
                "fail_compensation_for_step": "tx-step:missing"
            })
        },
    );
    assert_matches_golden("wa_tx_rollback", &capture);
}

#[test]
#[cfg(all(unix, feature = "vendored"))]
#[ignore = "requires an explicit same-source FT_WEZTERM_MUX_SERVER artifact under RCH/DSR"]
fn mcp_wa_tx_roundtrip_plan_run_rollback_persists_expected_state() {
    let mut harness = new_harness();
    enable_live_tx(&mut harness);
    seed_planned_tx(&harness);

    let contract_file = tx_file_path(harness.workspace.path());
    let contract_file_text = contract_file.display().to_string();

    let plan_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.tx_plan",
                json!({
                    "format": "json",
                    "contract_file": contract_file_text,
                }),
            )
            .expect("wa.tx_plan roundtrip success"),
    );
    let planned_contract = read_tx_contract(harness.workspace.path());
    log_tx_roundtrip_phase("plan", &plan_envelope, &planned_contract);
    assert_eq!(plan_envelope["ok"], Value::Bool(true));
    assert_eq!(plan_envelope["data"]["lifecycle_state"], "planned");
    assert_eq!(plan_envelope["data"]["step_count"], 2);
    assert_eq!(plan_envelope["data"]["compensation_count"], 2);
    assert_eq!(planned_contract.lifecycle_state, MissionTxState::Planned);
    assert_eq!(planned_contract.outcome, TxOutcome::Pending);
    assert_eq!(planned_contract.receipts.len(), 0);

    // One missing target must prevent even the ready first pane from receiving
    // input. Give this refusal its own durable identity so the success case
    // cannot be satisfied by an idempotency replay from the negative control.
    let mut refused_contract = planned_contract.clone();
    refused_contract.intent.tx_id = TxId("tx:missing-target".to_string());
    refused_contract.plan.tx_id = refused_contract.intent.tx_id.clone();
    refused_contract.plan.plan_id = TxPlanId("tx-plan:missing-target".to_string());
    let StepAction::SendText { pane_id, .. } = &mut refused_contract.plan.steps[1].action else {
        panic!("expected send-text step");
    };
    *pane_id = harness.live.as_ref().unwrap().pane_ids[1] + 10_000;
    let refused_path = harness
        .workspace
        .path()
        .join(".ft/mission/missing-target.json");
    write_json(&refused_path, &refused_contract);
    let refused_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.tx_run",
                json!({"contract_file": refused_path, "format": "json"}),
            )
            .expect("real missing-target refusal"),
    );
    assert_eq!(refused_envelope["ok"], true, "{refused_envelope}");
    assert_eq!(refused_envelope["data"]["final_state"], "failed");
    assert!(refused_envelope["data"]["commit_report"].is_null());
    let refused_persisted: MissionTxContract =
        serde_json::from_slice(&fs::read(&refused_path).unwrap()).unwrap();
    assert!(refused_persisted.receipts.is_empty());
    let live = harness.live.as_ref().unwrap();
    live.assert_consumed(0, "/do-step-1", 0);
    live.assert_consumed(1, "/do-step-2", 0);

    let run_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.tx_run",
                json!({
                    "format": "json",
                    "contract_file": contract_file.display().to_string(),
                }),
            )
            .expect("wa.tx_run roundtrip success"),
    );
    let committed_contract = read_tx_contract(harness.workspace.path());
    log_tx_roundtrip_phase("run", &run_envelope, &committed_contract);
    assert_eq!(run_envelope["ok"], Value::Bool(true));
    assert_eq!(run_envelope["data"]["final_state"], "committed");
    assert_eq!(
        run_envelope["data"]["prepare_report"]["outcome"],
        "all_ready"
    );
    assert_eq!(
        run_envelope["data"]["commit_report"]["outcome"],
        "committed"
    );
    assert!(run_envelope["data"]["compensation_report"].is_null());
    assert_eq!(
        committed_contract.lifecycle_state,
        MissionTxState::Committed
    );
    assert_eq!(committed_contract.outcome, TxOutcome::Committed);
    assert_eq!(committed_contract.receipts.len(), 2);
    let live = harness.live.as_ref().unwrap();
    live.assert_consumed(0, "/do-step-1", 1);
    live.assert_consumed(1, "/do-step-2", 1);

    let rollback_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.tx_rollback",
                json!({
                    "format": "json",
                    "contract_file": contract_file.display().to_string(),
                }),
            )
            .expect("wa.tx_rollback roundtrip success"),
    );
    let rolled_back_contract = read_tx_contract(harness.workspace.path());
    log_tx_roundtrip_phase("rollback", &rollback_envelope, &rolled_back_contract);
    assert_eq!(rollback_envelope["ok"], Value::Bool(true));
    assert_eq!(rollback_envelope["data"]["final_state"], "rolled_back");
    assert_eq!(
        rollback_envelope["data"]["compensation_report"]["outcome"],
        "fully_rolled_back"
    );
    assert_eq!(
        rollback_envelope["data"]["compensation_report"]["compensated_count"],
        2
    );
    assert_eq!(
        rolled_back_contract.lifecycle_state,
        MissionTxState::RolledBack
    );
    assert_eq!(rolled_back_contract.outcome, TxOutcome::Compensated);
    assert_eq!(rolled_back_contract.receipts.len(), 4);
    let live = harness.live.as_mut().unwrap();
    live.assert_consumed(0, "/undo-step-1", 1);
    live.assert_consumed(1, "/undo-step-2", 1);
    live.finish();
}
