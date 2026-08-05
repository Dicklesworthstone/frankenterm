//! Query client abstraction for TUI data access
//!
//! The `QueryClient` trait provides a clean abstraction over the frankenterm-core
//! query layer, enabling:
//!
//! - Testability: Mock implementations for unit tests
//! - Consistency: Same data access patterns as robot mode
//! - Decoupling: UI doesn't know about SQLite or storage internals
//!
//! # Cx-first migration (ft-xbnl0.2.2)
//!
//! This module is a sync → async bridge: the `QueryClient` trait methods are
//! synchronous and own a dedicated `runtime_async::Runtime` that executes the
//! inner async blocks via `runtime.block_on(async { ... })`. There is no
//! public `async fn` surface here to thread `&Cx` through explicitly — the
//! Cx flows automatically inside each `block_on` because
//! `runtime_async::Runtime::block_on` registers a root Cx under the
//! `asupersync-runtime` feature, and downstream async helpers
//! (`runtime_async::timeout`, `broadcast_recv`, etc.) acquire that Cx via
//! `Cx::current()`.
//!
//! If the TUI ever exposes an `async fn` query entry point, that's when a
//! `_cx` variant should be added, matching the pattern used in `retry.rs`,
//! `caut.rs`, `session_retention.rs`, `telemetry.rs`, `watchdog.rs`,
//! `restore_process.rs`, `survival.rs`, `events.rs`, and `wait.rs`.

use std::path::PathBuf;

use crate::circuit_breaker::CircuitBreakerStatus;
use crate::config::WorkspaceLayout;
use crate::runtime_async::CompatRuntime;
use crate::storage::{EventMuteRecord, StorageHandle};
pub use crate::ui_query::{PaneBookmarkView, RulesetProfileState, SavedSearchView};
use crate::wezterm::{PaneInfo, WeztermHandle, default_wezterm_handle};

/// Errors that can occur during query operations
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("Watcher is not running")]
    WatcherNotRunning,

    #[error("Database not initialized: {0}")]
    DatabaseNotInitialized(String),

    #[error("WezTerm error: {0}")]
    WeztermError(String),

    #[error("Storage error: {0}")]
    StorageError(String),

    #[error("Query failed: {0}")]
    QueryFailed(String),
}

/// Pane information for TUI display
#[derive(Debug, Clone)]
pub struct PaneView {
    pub pane_id: u64,
    pub title: String,
    pub domain: String,
    pub cwd: Option<String>,
    pub is_excluded: bool,
    pub agent_type: Option<String>,
    pub pane_state: String,
    pub last_activity_ts: Option<i64>,
    pub unhandled_event_count: u32,
}

impl From<&PaneInfo> for PaneView {
    fn from(info: &PaneInfo) -> Self {
        Self {
            pane_id: info.pane_id,
            title: info.title.clone().unwrap_or_default(),
            domain: info.effective_domain().to_string(),
            cwd: info.cwd.clone(),
            is_excluded: false,
            agent_type: infer_agent_type(info.title.as_deref(), info.cwd.as_deref()),
            pane_state: infer_pane_state(info),
            last_activity_ts: None,
            unhandled_event_count: 0,
        }
    }
}

fn infer_agent_type(title: Option<&str>, cwd: Option<&str>) -> Option<String> {
    let title_lower = title.unwrap_or("").to_ascii_lowercase();
    let cwd_lower = cwd.unwrap_or("").to_ascii_lowercase();
    if title_lower.contains("codex") || cwd_lower.contains("codex") {
        return Some("codex".to_string());
    }
    if title_lower.contains("claude") || cwd_lower.contains("claude") {
        return Some("claude".to_string());
    }
    if title_lower.contains("gemini") || cwd_lower.contains("gemini") {
        return Some("gemini".to_string());
    }
    None
}

fn infer_pane_state(info: &PaneInfo) -> String {
    let alt_screen = info
        .extra
        .get("is_alt_screen_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if alt_screen {
        return "AltScreen".to_string();
    }
    if info.cursor_visibility == Some(crate::wezterm::CursorVisibility::Hidden) {
        return "CommandRunning".to_string();
    }
    if info.is_active {
        return "PromptActive".to_string();
    }
    "unknown".to_string()
}

fn apply_pane_storage_aggregates(
    panes: &mut [PaneView],
    unhandled_by_pane: &std::collections::HashMap<u64, u32>,
    last_activity_by_pane: &std::collections::HashMap<u64, Option<i64>>,
) {
    for pane in panes {
        pane.unhandled_event_count = *unhandled_by_pane.get(&pane.pane_id).unwrap_or(&0);
        pane.last_activity_ts = last_activity_by_pane
            .get(&pane.pane_id)
            .copied()
            .flatten();
    }
}

/// Event information for TUI display
#[derive(Debug, Clone)]
pub struct EventView {
    pub id: i64,
    pub rule_id: String,
    pub pane_id: u64,
    pub severity: String,
    pub message: String,
    pub timestamp: i64,
    pub handled: bool,
    pub triage_state: Option<String>,
    pub labels: Vec<String>,
    pub note: Option<String>,
}

/// Action associated with a triage item
#[derive(Debug, Clone)]
pub struct TriageAction {
    pub label: String,
    pub command: String,
}

/// Triage item for the TUI
#[derive(Debug, Clone)]
pub struct TriageItemView {
    pub section: String,
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub actions: Vec<TriageAction>,
    pub event_id: Option<i64>,
    pub pane_id: Option<u64>,
    pub workflow_id: Option<String>,
}

/// Search result for TUI display
#[derive(Debug, Clone)]
pub struct SearchResultView {
    pub pane_id: u64,
    pub timestamp: i64,
    pub snippet: String,
    pub rank: f64,
}

/// Active workflow progress for TUI display
#[derive(Debug, Clone)]
pub struct WorkflowProgressView {
    pub id: String,
    pub workflow_name: String,
    pub pane_id: u64,
    pub current_step: usize,
    pub total_steps: usize,
    pub status: String,
    pub error: Option<String>,
    pub started_at: i64,
    pub updated_at: i64,
}

/// Action history entry for TUI display
#[derive(Debug, Clone)]
pub struct HistoryEntryView {
    /// Audit action record ID
    pub audit_id: i64,
    /// Timestamp (epoch ms)
    pub timestamp: i64,
    /// Pane associated with the action, when available
    pub pane_id: Option<u64>,
    /// Workflow associated with the action, when available
    pub workflow_id: Option<String>,
    /// Action kind (send_text, workflow_step, etc.)
    pub action_kind: String,
    /// Result status (success, denied, failed, ...)
    pub result: String,
    /// Actor kind (human/robot/mcp/workflow)
    pub actor_kind: String,
    /// Optional workflow step name
    pub step_name: Option<String>,
    /// Whether action can still be undone
    pub undoable: bool,
    /// Whether undo has already been executed
    pub undone: bool,
    /// Undo strategy label (manual/workflow_abort/...)
    pub undo_strategy: Option<String>,
    /// Redacted undo hint, if present
    pub undo_hint: Option<String>,
    /// Optional policy rule id associated with this action
    pub rule_id: Option<String>,
    /// Best-effort summary for list/detail panels
    pub summary: String,
}

/// Event filters for querying
#[derive(Debug, Default, Clone)]
pub struct EventFilters {
    pub pane_id: Option<u64>,
    pub rule_id: Option<String>,
    pub event_type: Option<String>,
    pub unhandled_only: bool,
    pub limit: usize,
}

/// Health status information
#[derive(Debug, Clone)]
pub struct HealthStatus {
    pub watcher_running: bool,
    pub db_accessible: bool,
    pub wezterm_accessible: bool,
    pub wezterm_circuit: CircuitBreakerStatus,
    pub pane_count: usize,
    pub event_count: usize,
    pub last_capture_ts: Option<i64>,
}

/// Abstraction over frankenterm-core query layer for TUI data access
///
/// This trait allows the TUI to be tested with mock implementations
/// while using the same query patterns as robot mode in production.
pub trait QueryClient: Send + Sync {
    /// List all panes from WezTerm
    fn list_panes(&self) -> Result<Vec<PaneView>, QueryError>;

    /// List recent events with optional filters
    fn list_events(&self, filters: &EventFilters) -> Result<Vec<EventView>, QueryError>;

    /// List triage items for operator attention
    fn list_triage_items(&self) -> Result<Vec<TriageItemView>, QueryError>;

    /// Full-text search across captured output
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResultView>, QueryError>;

    /// Check system health status
    fn health(&self) -> Result<HealthStatus, QueryError>;

    /// Check if the watcher is running
    fn is_watcher_running(&self) -> bool;

    /// Mark an event as muted (handled without workflow)
    fn mark_event_muted(&self, event_id: i64) -> Result<(), QueryError>;

    /// List active (incomplete) workflows with progress info
    fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError>;

    /// List recent action history (audit + undo metadata) for TUI display.
    ///
    /// Implementations may return an empty vector when history storage
    /// is unavailable.
    fn list_action_history(&self, _limit: usize) -> Result<Vec<HistoryEntryView>, QueryError> {
        Ok(Vec::new())
    }

    /// List pane bookmarks for panes/dashboard surfaces.
    fn list_pane_bookmarks(&self) -> Result<Vec<PaneBookmarkView>, QueryError> {
        Ok(Vec::new())
    }

    /// List saved searches for search/dashboard surfaces.
    fn list_saved_searches(&self) -> Result<Vec<SavedSearchView>, QueryError> {
        Ok(Vec::new())
    }

    /// Resolve ruleset profile status for profile-aware UI.
    fn ruleset_profile_state(&self) -> Result<RulesetProfileState, QueryError> {
        Ok(RulesetProfileState::default())
    }

    /// Query the unified timeline of events across panes.
    fn get_timeline(
        &self,
        _last_ms: i64,
        _limit: usize,
    ) -> Result<crate::storage::Timeline, QueryError> {
        Ok(crate::storage::Timeline {
            start: 0,
            end: 0,
            events: Vec::new(),
            correlations: Vec::new(),
            total_count: 0,
            has_more: false,
        })
    }

    /// Get the unified dashboard state snapshot.
    ///
    /// Returns `None` when the dashboard subsystem is not yet initialized
    /// or no data has been collected.
    fn dashboard_state(&self) -> Result<Option<crate::dashboard::DashboardState>, QueryError> {
        Ok(None)
    }
}

/// Production implementation of QueryClient
///
/// Uses the actual frankenterm-core storage and wezterm client to query data.
/// Owns a dedicated runtime_async runtime for async operations, avoiding
/// "cannot start a runtime from within a runtime" panics when the TUI
/// runs in a separate thread from the main async context.
pub struct ProductionQueryClient {
    workspace_layout: WorkspaceLayout,
    config_path: Option<PathBuf>,
    wezterm: WeztermHandle,
    #[allow(dead_code)]
    storage: Option<StorageHandle>,
    /// Shared dashboard manager updated by the runtime, read by TUI.
    dashboard_manager: Option<std::sync::Arc<std::sync::Mutex<crate::dashboard::DashboardManager>>>,
    /// Dedicated runtime for async operations - avoids nested runtime panics
    runtime: crate::runtime_async::Runtime,
}

impl ProductionQueryClient {
    /// Create a new production query client with a dedicated runtime_async runtime.
    ///
    /// The runtime is used to bridge sync TUI code with async operations,
    /// avoiding "cannot start a runtime from within a runtime" panics.
    #[must_use]
    pub fn new(workspace_layout: WorkspaceLayout) -> Self {
        let runtime = crate::runtime_async::RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .thread_name("tui-query-runtime")
            .build()
            .expect("Failed to create TUI query runtime");

        Self {
            workspace_layout,
            config_path: crate::config::resolve_config_path(None),
            wezterm: default_wezterm_handle(),
            storage: None,
            dashboard_manager: None,
            runtime,
        }
    }

    /// Create with an existing storage handle and a dedicated runtime_async runtime.
    ///
    /// The runtime is used to bridge sync TUI code with async operations,
    /// avoiding "cannot start a runtime from within a runtime" panics.
    #[must_use]
    pub fn with_storage(workspace_layout: WorkspaceLayout, storage: StorageHandle) -> Self {
        let runtime = crate::runtime_async::RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .thread_name("tui-query-runtime")
            .build()
            .expect("Failed to create TUI query runtime");

        Self {
            workspace_layout,
            config_path: crate::config::resolve_config_path(None),
            wezterm: default_wezterm_handle(),
            storage: Some(storage),
            dashboard_manager: None,
            runtime,
        }
    }

    /// Create with a custom WezTerm interface (useful for tests/mocks).
    #[must_use]
    pub fn with_wezterm(workspace_layout: WorkspaceLayout, wezterm: WeztermHandle) -> Self {
        let runtime = crate::runtime_async::RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .thread_name("tui-query-runtime")
            .build()
            .expect("Failed to create TUI query runtime");

        Self {
            workspace_layout,
            config_path: crate::config::resolve_config_path(None),
            wezterm,
            storage: None,
            dashboard_manager: None,
            runtime,
        }
    }

    /// Create with storage and a custom WezTerm interface.
    #[must_use]
    pub fn with_storage_and_wezterm(
        workspace_layout: WorkspaceLayout,
        storage: StorageHandle,
        wezterm: WeztermHandle,
    ) -> Self {
        let runtime = crate::runtime_async::RuntimeBuilder::multi_thread()
            .worker_threads(2)
            .thread_name("tui-query-runtime")
            .build()
            .expect("Failed to create TUI query runtime");

        Self {
            workspace_layout,
            config_path: crate::config::resolve_config_path(None),
            wezterm,
            storage: Some(storage),
            dashboard_manager: None,
            runtime,
        }
    }

    /// Set the shared dashboard manager for live subsystem data.
    ///
    /// The dashboard manager should be updated by the runtime observation loop.
    /// The TUI queries it on each refresh cycle via `dashboard_state()`.
    pub fn set_dashboard_manager(
        &mut self,
        mgr: std::sync::Arc<std::sync::Mutex<crate::dashboard::DashboardManager>>,
    ) {
        self.dashboard_manager = Some(mgr);
    }

    /// Get the database path
    fn db_path(&self) -> PathBuf {
        self.workspace_layout.db_path.clone()
    }

    /// Check if the database exists
    fn db_exists(&self) -> bool {
        self.db_path().exists()
    }
}

impl QueryClient for ProductionQueryClient {
    fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
        let wezterm = &self.wezterm;
        let storage = self.storage.clone();

        // Use the dedicated runtime to run async code from sync context.
        // This avoids "cannot start a runtime from within a runtime" panics
        // because this runtime is separate from any parent async context.
        self.runtime.block_on(async {
            let panes = wezterm
                .list_panes()
                .await
                .map_err(|e| QueryError::WeztermError(e.to_string()))?;
            let mut pane_views: Vec<PaneView> = panes.iter().map(PaneView::from).collect();

            if let Some(storage) = storage {
                // ft-xbnl0.2.3 tick 256: cx-first TUI pane-aggregation reads.
                let agg_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                let pane_ids = pane_views
                    .iter()
                    .map(|pane| pane.pane_id)
                    .collect::<Vec<_>>();
                let live_pane_unhandled = async {
                    let mut counts = std::collections::HashMap::with_capacity(pane_ids.len());
                    for pane_id_chunk in
                        pane_ids.chunks(crate::storage::STORAGE_BULK_ID_INPUT_MAX)
                    {
                        counts.extend(
                            storage
                                .count_unhandled_events_by_pane_bulk_with_cx(
                                    &agg_cx,
                                    pane_id_chunk,
                                )
                                .await?,
                        );
                    }
                    Ok::<_, crate::Error>(counts)
                };
                let live_pane_activity = async {
                    let mut activity = std::collections::HashMap::with_capacity(pane_ids.len());
                    for pane_id_chunk in
                        pane_ids.chunks(crate::storage::STORAGE_BULK_ID_INPUT_MAX)
                    {
                        activity.extend(
                            storage
                                .pane_last_output_at_bulk_with_cx(&agg_cx, pane_id_chunk)
                                .await?,
                        );
                    }
                    Ok::<_, crate::Error>(activity)
                };
                let (unhandled_res, last_activity_res) = crate::runtime_async::join!(
                    live_pane_unhandled,
                    live_pane_activity
                );
                let unhandled_by_pane = unhandled_res.unwrap_or_else(|error| {
                    tracing::warn!(
                        error = %error,
                        "TUI pane refresh could not load unhandled-event counts"
                    );
                    std::collections::HashMap::new()
                });
                let last_activity_by_pane = last_activity_res.unwrap_or_else(|error| {
                    tracing::warn!(
                        error = %error,
                        "TUI pane refresh could not load live-pane activity"
                    );
                    std::collections::HashMap::new()
                });
                apply_pane_storage_aggregates(
                    &mut pane_views,
                    &unhandled_by_pane,
                    &last_activity_by_pane,
                );
            }

            Ok(pane_views)
        })
    }

    fn list_events(&self, filters: &EventFilters) -> Result<Vec<EventView>, QueryError> {
        let Some(storage) = &self.storage else {
            return Err(QueryError::DatabaseNotInitialized(
                "Database connection not available".to_string(),
            ));
        };

        let query = crate::storage::EventQuery {
            limit: Some(filters.limit),
            pane_id: filters.pane_id,
            rule_id: filters.rule_id.clone(),
            event_type: filters.event_type.clone(),
            triage_state: None,
            label: None,
            unhandled_only: filters.unhandled_only,
            since: None,
            until: None,
        };

        let rows = self.runtime.block_on(async {
            // ft-interactive-systems-performance-4tenz.15: load annotations in
            // bounded snapshots instead of issuing one storage read per event.
            let query_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let events = storage
                .get_events_with_cx(&query_cx, query)
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))?;

            let event_ids = events.iter().map(|event| event.id).collect::<Vec<_>>();
            let mut annotations_by_event =
                std::collections::HashMap::with_capacity(event_ids.len());
            for (page_index, event_id_chunk) in event_ids
                .chunks(crate::storage::STORAGE_BULK_ID_INPUT_MAX)
                .enumerate()
            {
                match storage
                    .get_event_annotations_bulk_with_cx(&query_cx, event_id_chunk)
                    .await
                {
                    Ok(page) => {
                        // `get_event_annotations_bulk_with_cx` checkpoints at
                        // the start of every call, so each page is also an
                        // explicit cancellation/fairness boundary.
                        for (event_id, annotations) in page {
                            if annotations_by_event
                                .insert(event_id, annotations)
                                .is_some()
                            {
                                return Err(QueryError::StorageError(format!(
                                    "bulk annotation paging returned duplicate event ID {event_id}"
                                )));
                            }
                        }
                    }
                    Err(error) => {
                        // Preserve the prior fail-soft behavior: only this
                        // bounded page defaults, while successful pages remain
                        // visible and output ordering stays event-query order.
                        tracing::warn!(
                            error = %error,
                            page_index,
                            event_count = event_id_chunk.len(),
                            first_event_id = ?event_id_chunk.first().copied(),
                            last_event_id = ?event_id_chunk.last().copied(),
                            "TUI event refresh could not load one annotation page",
                        );
                    }
                }
            }

            let rows = events
                .into_iter()
                .map(|event| {
                    let annotations = annotations_by_event
                        .remove(&event.id)
                        .unwrap_or_default();
                    (event, annotations)
                })
                .collect::<Vec<_>>();
            Ok::<_, QueryError>(rows)
        })?;

        Ok(rows
            .into_iter()
            .map(|(e, annotations)| EventView {
                id: e.id,
                rule_id: e.rule_id,
                pane_id: e.pane_id,
                severity: e.severity,
                message: e
                    .matched_text
                    .unwrap_or_else(|| "Pattern matched".to_string()),
                timestamp: e.detected_at,
                handled: e.handled_at.is_some(),
                triage_state: annotations.triage_state,
                labels: annotations.labels,
                note: annotations.note,
            })
            .collect())
    }

    fn list_triage_items(&self) -> Result<Vec<TriageItemView>, QueryError> {
        use crate::crash::{HealthSnapshot, latest_crash_bundle};
        use crate::output::{HealthDiagnosticStatus, HealthSnapshotRenderer};

        fn action(label: &str, command: String) -> TriageAction {
            TriageAction {
                label: label.to_string(),
                command,
            }
        }

        fn severity_rank(sev: &str) -> u8 {
            match sev {
                "error" => 3,
                "warning" => 2,
                "info" => 1,
                _ => 0,
            }
        }

        let mut items: Vec<TriageItemView> = Vec::new();

        // Health diagnostics (in-process snapshot)
        if let Some(snapshot) = HealthSnapshot::get_global() {
            let checks = HealthSnapshotRenderer::diagnostic_checks(&snapshot);
            for check in &checks {
                let severity = match check.status {
                    HealthDiagnosticStatus::Error => "error",
                    HealthDiagnosticStatus::Warning => "warning",
                    _ => continue,
                };
                items.push(TriageItemView {
                    section: "health".to_string(),
                    severity: severity.to_string(),
                    title: check.name.to_string(),
                    detail: check.detail.to_string(),
                    actions: vec![
                        action("Run diagnostics", "ft doctor".to_string()),
                        action("Machine diagnostics", "ft doctor --json".to_string()),
                    ],
                    event_id: None,
                    pane_id: None,
                    workflow_id: None,
                });
            }
        }

        // Recent crash bundle
        if let Some(bundle) = latest_crash_bundle(&self.workspace_layout.crash_dir) {
            let detail = if let Some(ref report) = bundle.report {
                let message = sanitize_historical_crash_text(&report.message, 100);
                let location = report
                    .location
                    .as_deref()
                    .map(sanitize_historical_crash_location)
                    .unwrap_or_else(|| "unknown".to_string());
                format!("{message} (at {location})")
            } else if let Some(ref manifest) = bundle.manifest {
                format!(
                    "crash at {}",
                    sanitize_historical_crash_text(&manifest.created_at, 40)
                )
            } else {
                "crash bundle found".to_string()
            };
            items.push(TriageItemView {
                section: "crashes".to_string(),
                severity: "warning".to_string(),
                title: "Recent crash".to_string(),
                detail,
                actions: vec![
                    action(
                        "Export crash bundle",
                        "ft reproduce --kind crash".to_string(),
                    ),
                    action("Run diagnostics", "ft doctor".to_string()),
                ],
                event_id: None,
                pane_id: None,
                workflow_id: None,
            });
        }

        // Unhandled events + incomplete workflows (require DB)
        let Some(storage) = &self.storage else {
            items.push(TriageItemView {
                section: "health".to_string(),
                severity: "warning".to_string(),
                title: "Database unavailable".to_string(),
                detail: "Could not open storage".to_string(),
                actions: vec![
                    action("Start watcher", "ft watch".to_string()),
                    action("Run diagnostics", "ft doctor".to_string()),
                ],
                event_id: None,
                pane_id: None,
                workflow_id: None,
            });
            items.sort_by_key(|item| std::cmp::Reverse(severity_rank(&item.severity)));
            return Ok(items);
        };

        // Unhandled events
        let query = crate::storage::EventQuery {
            limit: Some(20),
            pane_id: None,
            rule_id: None,
            event_type: None,
            triage_state: None,
            label: None,
            unhandled_only: true,
            since: None,
            until: None,
        };
        let events = self.runtime.block_on(async {
            storage
                .get_events(query)
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))
        })?;
        for event in events {
            items.push(TriageItemView {
                section: "events".to_string(),
                severity: event.severity,
                title: format!(
                    "[pane {}] {}: {}",
                    event.pane_id, event.event_type, event.rule_id
                ),
                detail: event
                    .matched_text
                    .unwrap_or_default()
                    .chars()
                    .take(120)
                    .collect(),
                actions: vec![
                    action(
                        "List unhandled events",
                        format!("ft events --pane {} --unhandled", event.pane_id),
                    ),
                    action(
                        "Explain detection",
                        format!("ft why --recent --pane {}", event.pane_id),
                    ),
                    action("Show pane details", format!("ft show {}", event.pane_id)),
                ],
                event_id: Some(event.id),
                pane_id: Some(event.pane_id),
                workflow_id: None,
            });
        }

        // Incomplete workflows
        let workflows = self.runtime.block_on(async {
            storage
                .find_incomplete_workflows()
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))
        })?;
        for wf in workflows {
            items.push(TriageItemView {
                section: "workflows".to_string(),
                severity: "info".to_string(),
                title: format!("{} (pane {})", wf.workflow_name, wf.pane_id),
                detail: format!("status={}, step={}", wf.status, wf.current_step),
                actions: vec![
                    action(
                        "Check workflow status",
                        format!("ft workflow status {}", wf.id),
                    ),
                    action(
                        "Explain decisions",
                        format!("ft why --recent --pane {}", wf.pane_id),
                    ),
                    action("Show pane details", format!("ft show {}", wf.pane_id)),
                ],
                event_id: None,
                pane_id: Some(wf.pane_id),
                workflow_id: Some(wf.id.clone()),
            });
        }

        items.sort_by(|a, b| {
            let sa = severity_rank(&a.severity);
            let sb = severity_rank(&b.severity);
            sb.cmp(&sa).then_with(|| a.title.cmp(&b.title))
        });

        Ok(items)
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResultView>, QueryError> {
        let Some(storage) = &self.storage else {
            return Err(QueryError::DatabaseNotInitialized(
                "Database connection not available".to_string(),
            ));
        };

        let options = crate::storage::SearchOptions {
            limit: Some(limit),
            include_snippets: Some(true),
            snippet_max_tokens: Some(30),
            highlight_prefix: Some(">>".to_string()),
            highlight_suffix: Some("<<".to_string()),
            ..Default::default()
        };

        let query = query.to_string();
        // Use the dedicated runtime to run async code from sync context.
        let results = self.runtime.block_on(async {
            storage
                .search_with_results(&query, options)
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))
        })?;

        Ok(results
            .into_iter()
            .map(|r| SearchResultView {
                pane_id: r.segment.pane_id,
                timestamp: r.segment.captured_at,
                snippet: r.snippet.unwrap_or(r.segment.content),
                rank: r.score,
            })
            .collect())
    }

    fn health(&self) -> Result<HealthStatus, QueryError> {
        // Call list_panes() once and reuse the result to avoid duplicate IPC calls
        let panes_result = self.list_panes();
        let wezterm_accessible = panes_result.as_ref().is_ok_and(|p| !p.is_empty());
        let pane_count = panes_result.map_or(0, |p| p.len());

        let db_accessible = self.db_exists();
        let watcher_running = self.is_watcher_running();
        let (event_count, last_capture_ts) = self.storage_health_fields();

        Ok(HealthStatus {
            watcher_running,
            db_accessible,
            wezterm_accessible,
            wezterm_circuit: self.wezterm.circuit_status(),
            pane_count,
            event_count,
            last_capture_ts,
        })
    }

    fn is_watcher_running(&self) -> bool {
        self.workspace_layout.lock_path.exists()
    }

    fn mark_event_muted(&self, event_id: i64) -> Result<(), QueryError> {
        let Some(storage) = &self.storage else {
            return Err(QueryError::DatabaseNotInitialized(
                "Database connection not available".to_string(),
            ));
        };

        self.runtime.block_on(async {
            // ft-xbnl0.2.3 tick 255: cx-first TUI mute + handled writes.
            let mute_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            if let Ok(Some(identity_key)) = storage
                .get_event_identity_key_with_cx(&mute_cx, event_id)
                .await
            {
                let record = EventMuteRecord {
                    identity_key,
                    scope: "workspace".to_string(),
                    created_at: epoch_ms(),
                    expires_at: None,
                    created_by: None,
                    reason: Some("tui mute".to_string()),
                };
                storage
                    .add_event_mute_with_cx(&mute_cx, record)
                    .await
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
            }

            storage
                .mark_event_handled_with_cx(&mute_cx, event_id, None, "muted")
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))
        })
    }

    fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
        let Some(storage) = &self.storage else {
            return Ok(Vec::new());
        };

        let workflows = self.runtime.block_on(async {
            storage
                .find_incomplete_workflows()
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))
        })?;

        Ok(workflows
            .into_iter()
            .map(|wf| {
                // Estimate total steps: at least current_step + 1 for incomplete
                let total_steps = (wf.current_step + 1).max(2);
                WorkflowProgressView {
                    id: wf.id,
                    workflow_name: wf.workflow_name,
                    pane_id: wf.pane_id,
                    current_step: wf.current_step,
                    total_steps,
                    status: wf.status,
                    error: wf.error,
                    started_at: wf.started_at,
                    updated_at: wf.updated_at,
                }
            })
            .collect())
    }

    fn list_action_history(&self, limit: usize) -> Result<Vec<HistoryEntryView>, QueryError> {
        let Some(storage) = &self.storage else {
            return Ok(Vec::new());
        };

        let query = crate::storage::ActionHistoryQuery {
            limit: Some(limit),
            ..Default::default()
        };

        let records = self.runtime.block_on(async {
            storage
                .get_action_history(query)
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))
        })?;

        Ok(records
            .into_iter()
            .map(|row| {
                let summary = row
                    .input_summary
                    .clone()
                    .or_else(|| row.verification_summary.clone())
                    .or_else(|| row.decision_reason.clone())
                    .unwrap_or_default();

                HistoryEntryView {
                    audit_id: row.id,
                    timestamp: row.ts,
                    pane_id: row.pane_id,
                    workflow_id: row.workflow_id,
                    action_kind: row.action_kind,
                    result: row.result,
                    actor_kind: row.actor_kind,
                    step_name: row.step_name,
                    undoable: row.undoable.unwrap_or(false) && row.undone_at.is_none(),
                    undone: row.undone_at.is_some(),
                    undo_strategy: row.undo_strategy,
                    undo_hint: row.undo_hint,
                    rule_id: row.rule_id,
                    summary,
                }
            })
            .collect())
    }

    fn list_pane_bookmarks(&self) -> Result<Vec<PaneBookmarkView>, QueryError> {
        let Some(storage) = &self.storage else {
            return Ok(Vec::new());
        };
        let storage = storage.clone();
        self.runtime.block_on(async {
            crate::ui_query::list_pane_bookmarks(&storage)
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))
        })
    }

    fn list_saved_searches(&self) -> Result<Vec<SavedSearchView>, QueryError> {
        let Some(storage) = &self.storage else {
            return Ok(Vec::new());
        };
        let storage = storage.clone();
        self.runtime.block_on(async {
            crate::ui_query::list_saved_searches(&storage)
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))
        })
    }

    fn ruleset_profile_state(&self) -> Result<RulesetProfileState, QueryError> {
        crate::ui_query::resolve_ruleset_profile_state(self.config_path.as_deref())
            .map_err(|e| QueryError::QueryFailed(e.to_string()))
    }

    fn get_timeline(
        &self,
        last_ms: i64,
        limit: usize,
    ) -> Result<crate::storage::Timeline, QueryError> {
        let storage = match &self.storage {
            Some(s) => s.clone(),
            None => {
                return Ok(crate::storage::Timeline {
                    start: 0,
                    end: 0,
                    events: Vec::new(),
                    correlations: Vec::new(),
                    total_count: 0,
                    has_more: false,
                });
            }
        };
        let now = epoch_ms();
        let start = now - last_ms;
        let query = crate::storage::TimelineQuery::new()
            .with_range(start, now)
            .with_pagination(limit, 0);
        self.runtime
            .block_on(async {
                // ft-xbnl0.2.3 tick 256: cx-first TUI timeline read.
                let timeline_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                storage.get_timeline_with_cx(&timeline_cx, query).await
            })
            .map_err(|e| QueryError::StorageError(e.to_string()))
    }

    fn dashboard_state(&self) -> Result<Option<crate::dashboard::DashboardState>, QueryError> {
        let Some(mgr) = &self.dashboard_manager else {
            return Ok(None);
        };
        let mut guard = mgr
            .lock()
            .map_err(|e| QueryError::QueryFailed(format!("dashboard lock poisoned: {e}")))?;
        Ok(Some(guard.snapshot()))
    }
}

impl ProductionQueryClient {
    fn storage_health_fields(&self) -> (usize, Option<i64>) {
        let Some(storage) = &self.storage else {
            return (0, None);
        };

        let result = self.runtime.block_on(async {
            let health_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let (event_count, segment_range) = crate::runtime_async::join!(
                storage.count_events_with_cx(&health_cx),
                storage.get_segment_time_range_with_cx(&health_cx)
            );
            let event_count = event_count.map_err(|e| QueryError::StorageError(e.to_string()))?;
            let (_, last_capture_ts) =
                segment_range.map_err(|e| QueryError::StorageError(e.to_string()))?;
            Ok::<_, QueryError>((event_count, last_capture_ts))
        });

        match result {
            Ok(fields) => fields,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Failed to load TUI storage health counters",
                );
                (0, None)
            }
        }
    }
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn is_untrusted_display_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// Normalize historical crash-bundle strings before putting them in the TUI.
///
/// Older bundles can contain caller-controlled panic payloads, terminal
/// escape sequences, embedded newlines, and directional controls. Strip ANSI
/// first, replace every remaining display control with a space, then apply the
/// shared Unicode/cell-safe truncator so multibyte text can never panic or
/// overflow the detail column.
fn scrub_historical_crash_text(value: &str) -> String {
    let stripped = crate::output::strip_ansi(value);
    stripped
        .chars()
        .map(|character| {
            if is_untrusted_display_control(character) {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
}

fn sanitize_historical_crash_text(value: &str, max_width: usize) -> String {
    let scrubbed = scrub_historical_crash_text(value);
    crate::output::truncate(scrubbed.trim(), max_width)
}

fn sanitize_historical_crash_location(value: &str) -> String {
    const MAX_LOCATION_WIDTH: usize = 80;
    let scrubbed = scrub_historical_crash_text(value);
    let location = scrubbed.trim();

    if let Some(rest) = location.strip_prefix("line:")
        && let Some((line, column)) = rest.split_once(":column:")
        && line.parse::<u32>().is_ok()
        && column.parse::<u32>().is_ok()
    {
        return format!("line:{line}:column:{column}");
    }

    // Legacy reports used `path:line:column`. Preserve the useful coordinates
    // without reflecting an absolute/local path into the operator display.
    let mut suffixes = location.rsplitn(3, ':');
    let column = suffixes.next();
    let line = suffixes.next();
    if let (Some(column), Some(line)) = (column, line)
        && line.parse::<u32>().is_ok()
        && column.parse::<u32>().is_ok()
    {
        return format!("line:{line}:column:{column}");
    }

    let basename = location
        .rsplit(['/', '\\'])
        .next()
        .filter(|component| !component.is_empty())
        .unwrap_or("unknown");
    crate::output::truncate(basename, MAX_LOCATION_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock query client for testing
    struct MockQueryClient {
        panes: Vec<PaneView>,
        events: Vec<EventView>,
        triage_items: Vec<TriageItemView>,
        watcher_running: bool,
    }

    impl MockQueryClient {
        fn new() -> Self {
            Self {
                panes: vec![PaneView {
                    pane_id: 0,
                    title: "test-pane".to_string(),
                    domain: "local".to_string(),
                    cwd: Some("/home/test".to_string()),
                    is_excluded: false,
                    agent_type: Some("claude-code".to_string()),
                    pane_state: "PromptActive".to_string(),
                    last_activity_ts: Some(1_700_000_000_000),
                    unhandled_event_count: 1,
                }],
                events: Vec::new(),
                triage_items: vec![TriageItemView {
                    section: "events".to_string(),
                    severity: "warning".to_string(),
                    title: "[pane 0] test".to_string(),
                    detail: "detail".to_string(),
                    actions: vec![TriageAction {
                        label: "Explain".to_string(),
                        command: "ft why --recent --pane 0".to_string(),
                    }],
                    event_id: Some(1),
                    pane_id: Some(0),
                    workflow_id: None,
                }],
                watcher_running: true,
            }
        }
    }

    impl QueryClient for MockQueryClient {
        fn list_panes(&self) -> Result<Vec<PaneView>, QueryError> {
            Ok(self.panes.clone())
        }

        fn list_events(&self, _filters: &EventFilters) -> Result<Vec<EventView>, QueryError> {
            Ok(self.events.clone())
        }

        fn list_triage_items(&self) -> Result<Vec<TriageItemView>, QueryError> {
            Ok(self.triage_items.clone())
        }

        fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchResultView>, QueryError> {
            Ok(Vec::new())
        }

        fn health(&self) -> Result<HealthStatus, QueryError> {
            Ok(HealthStatus {
                watcher_running: self.watcher_running,
                db_accessible: true,
                wezterm_accessible: true,
                wezterm_circuit: CircuitBreakerStatus::default(),
                pane_count: self.panes.len(),
                event_count: self.events.len(),
                last_capture_ts: None,
            })
        }

        fn is_watcher_running(&self) -> bool {
            self.watcher_running
        }

        fn mark_event_muted(&self, _event_id: i64) -> Result<(), QueryError> {
            Ok(())
        }

        fn list_active_workflows(&self) -> Result<Vec<WorkflowProgressView>, QueryError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn mock_client_lists_panes() {
        let client = MockQueryClient::new();
        let panes = client.list_panes().unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, 0);
        assert_eq!(panes[0].title, "test-pane");
    }

    #[test]
    fn mock_client_health_status() {
        let client = MockQueryClient::new();
        let health = client.health().unwrap();
        assert!(health.watcher_running);
        assert!(health.db_accessible);
        assert_eq!(health.pane_count, 1);
    }

    #[test]
    fn historical_crash_detail_is_terminal_safe_unicode_safe_and_bounded() {
        let message = format!(
            "\x1b[31m{}\x1b[0m\nspoofed\u{202e}direction",
            "猫".repeat(80)
        );
        let sanitized = sanitize_historical_crash_text(&message, 100);
        assert!(!sanitized.contains('\x1b'));
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\u{202e}'));
        assert!(sanitized.ends_with("..."));
        assert_eq!(
            crate::output::truncate(&sanitized, 100),
            sanitized,
            "sanitized detail must already fit the shared display-width bound"
        );

        assert_eq!(
            sanitize_historical_crash_location(
                "\x1b[31m/Users/private/project/secret.rs:42:7\x1b[0m\n"
            ),
            "line:42:column:7"
        );
        assert_eq!(
            sanitize_historical_crash_location("line:8:column:9"),
            "line:8:column:9"
        );
        assert_eq!(
            sanitize_historical_crash_location(&format!(
                "/{}/secret.rs:123:45",
                "private/".repeat(100)
            )),
            "line:123:column:45",
            "location parsing must retain a bounded coordinate even after a long legacy path"
        );
    }

    #[test]
    fn production_health_reads_storage_counters() {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build query test runtime");

        let temp_dir = tempfile::tempdir().expect("temp workspace");
        let (layout, storage, last_capture_ts, wezterm) = runtime.block_on(async {
            let layout = crate::config::WorkspaceLayout::new(
                temp_dir.path().to_path_buf(),
                &crate::config::StorageConfig::default(),
                &crate::config::IpcConfig::default(),
            );
            std::fs::create_dir_all(&layout.ft_dir).expect("create .ft dir");
            let db_path = layout.db_path.to_string_lossy().to_string();
            let storage = StorageHandle::new(&db_path).await.expect("open storage");
            let now = epoch_ms();
            storage
                .upsert_pane(crate::storage::PaneRecord {
                    pane_id: 1,
                    pane_uuid: None,
                    domain: "local".to_string(),
                    window_id: Some(0),
                    tab_id: Some(0),
                    title: Some("health-pane".to_string()),
                    cwd: None,
                    tty_name: None,
                    first_seen_at: now,
                    last_seen_at: now,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                })
                .await
                .expect("upsert pane");
            let segment = storage
                .append_segment(1, "health segment", None)
                .await
                .expect("append segment");
            storage
                .record_event(crate::storage::StoredEvent {
                    id: 0,
                    pane_id: 1,
                    rule_id: "health.rule".to_string(),
                    agent_type: "codex".to_string(),
                    event_type: "usage".to_string(),
                    severity: "warning".to_string(),
                    confidence: 0.9,
                    extracted: None,
                    matched_text: Some("health segment".to_string()),
                    segment_id: Some(segment.id),
                    detected_at: segment.captured_at,
                    dedupe_key: None,
                    handled_at: None,
                    handled_by_workflow_id: None,
                    handled_status: None,
                })
                .await
                .expect("record event");

            let mock = std::sync::Arc::new(crate::wezterm::MockWezterm::new());
            mock.add_default_pane(1).await;
            let wezterm: crate::wezterm::WeztermHandle = mock;

            (layout, storage, segment.captured_at, wezterm)
        });

        let client =
            ProductionQueryClient::with_storage_and_wezterm(layout, storage.clone(), wezterm);
        let health = client.health().expect("query health");
        assert_eq!(health.event_count, 1);
        assert_eq!(health.last_capture_ts, Some(last_capture_ts));
        assert_eq!(health.pane_count, 1);

        drop(client);
        runtime.block_on(async {
            storage.shutdown().await.expect("shutdown storage");
        });
    }

    #[test]
    fn v35_production_list_panes_pages_live_aggregates_at_the_bulk_limit() {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build query paging test runtime");
        let temp_dir = tempfile::tempdir().expect("temp workspace");
        let last_pane_id =
            u64::try_from(crate::storage::STORAGE_BULK_ID_INPUT_MAX + 1).unwrap();

        let (
            layout,
            storage,
            first_page_pane_id,
            overflow_page_pane_id,
            first_activity,
            overflow_activity,
            wezterm,
        ) = runtime.block_on(async {
            let layout = crate::config::WorkspaceLayout::new(
                temp_dir.path().to_path_buf(),
                &crate::config::StorageConfig::default(),
                &crate::config::IpcConfig::default(),
            );
            std::fs::create_dir_all(&layout.ft_dir).expect("create .ft dir");
            let db_path = layout.db_path.to_string_lossy().to_string();
            let storage = StorageHandle::new(&db_path).await.expect("open storage");
            let mock = std::sync::Arc::new(crate::wezterm::MockWezterm::new());
            for pane_id in 1..=last_pane_id {
                mock.add_default_pane(pane_id).await;
            }
            let pane_order = crate::wezterm::WeztermInterface::list_panes(mock.as_ref())
                .await
                .expect("read stable mock pane order");
            assert_eq!(
                pane_order.len(),
                crate::storage::STORAGE_BULK_ID_INPUT_MAX + 1
            );
            // The map is not mutated between this read and the production
            // read, so its iteration order remains stable. Seed the first ID
            // beyond one full chunk to prove that chunk is not dropped.
            let first_page_pane_id = pane_order.first().expect("first mock pane").pane_id;
            let overflow_page_pane_id = pane_order
                .get(crate::storage::STORAGE_BULK_ID_INPUT_MAX)
                .expect("first pane beyond one full bulk chunk")
                .pane_id;

            let now = epoch_ms();
            for pane_id in [first_page_pane_id, overflow_page_pane_id] {
                storage
                    .upsert_pane(crate::storage::PaneRecord {
                        pane_id,
                        pane_uuid: None,
                        domain: "local".to_string(),
                        window_id: Some(0),
                        tab_id: Some(0),
                        title: Some(format!("paged-pane-{pane_id}")),
                        cwd: None,
                        tty_name: None,
                        first_seen_at: now,
                        last_seen_at: now,
                        observed: true,
                        ignore_reason: None,
                        last_decision_at: None,
                    })
                    .await
                    .expect("upsert paged pane");
            }
            let first_segment = storage
                .append_segment(first_page_pane_id, "first page activity", None)
                .await
                .expect("append first-page segment");
            let overflow_segment = storage
                .append_segment(overflow_page_pane_id, "overflow page activity", None)
                .await
                .expect("append overflow-page segment");
            storage
                .record_event(crate::storage::StoredEvent {
                    id: 0,
                    pane_id: overflow_page_pane_id,
                    rule_id: "paged.overflow-pane".to_string(),
                    agent_type: "test".to_string(),
                    event_type: "paged".to_string(),
                    severity: "info".to_string(),
                    confidence: 1.0,
                    extracted: None,
                    matched_text: None,
                    segment_id: Some(overflow_segment.id),
                    detected_at: overflow_segment.captured_at,
                    dedupe_key: None,
                    handled_at: None,
                    handled_by_workflow_id: None,
                    handled_status: None,
                })
                .await
                .expect("record overflow-page event");

            let wezterm: crate::wezterm::WeztermHandle = mock;
            (
                layout,
                storage,
                first_page_pane_id,
                overflow_page_pane_id,
                first_segment.captured_at,
                overflow_segment.captured_at,
                wezterm,
            )
        });

        let client =
            ProductionQueryClient::with_storage_and_wezterm(layout, storage.clone(), wezterm);
        let panes = client.list_panes().expect("list paged live panes");
        assert_eq!(
            panes.len(),
            crate::storage::STORAGE_BULK_ID_INPUT_MAX + 1
        );
        assert_eq!(
            panes
                .get(crate::storage::STORAGE_BULK_ID_INPUT_MAX)
                .map(|pane| pane.pane_id),
            Some(overflow_page_pane_id),
            "the seeded overflow pane must exercise the second bulk chunk"
        );
        assert_eq!(
            panes
                .iter()
                .find(|pane| pane.pane_id == first_page_pane_id)
                .and_then(|pane| pane.last_activity_ts),
            Some(first_activity)
        );
        assert_eq!(
            panes
                .iter()
                .find(|pane| pane.pane_id == overflow_page_pane_id)
                .and_then(|pane| pane.last_activity_ts),
            Some(overflow_activity)
        );
        assert_eq!(
            panes
                .iter()
                .find(|pane| pane.pane_id == first_page_pane_id)
                .map(|pane| pane.unhandled_event_count),
            Some(0)
        );
        assert_eq!(
            panes
                .iter()
                .find(|pane| pane.pane_id == overflow_page_pane_id)
                .map(|pane| pane.unhandled_event_count),
            Some(1)
        );

        drop(client);
        runtime.block_on(async {
            storage.shutdown().await.expect("shutdown storage");
        });
    }

    #[test]
    fn v35_performance_campaign_tui_annotations_page_past_bulk_cap() {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build annotation paging test runtime");
        let temp_dir = tempfile::tempdir().expect("temp workspace");
        let layout = crate::config::WorkspaceLayout::new(
            temp_dir.path().to_path_buf(),
            &crate::config::StorageConfig::default(),
            &crate::config::IpcConfig::default(),
        );
        std::fs::create_dir_all(&layout.ft_dir).expect("create .ft dir");

        // Seed one row beyond the storage bulk-read cap without spending the
        // test's wall-clock budget on thousands of writer round trips. Event
        // IDs are inserted monotonically so the durable high-water triggers
        // exercise the same invariant as the production event writer.
        let event_count = crate::storage::STORAGE_BULK_ID_INPUT_MAX + 1;
        let event_count_i64 = i64::try_from(event_count).expect("event count fits i64");
        let connection = rusqlite::Connection::open(&layout.db_path).expect("open seed database");
        crate::storage::migrations::initialize_schema(&connection).expect("initialize schema");
        connection
            .execute(
                "INSERT INTO panes (
                     pane_id, domain, first_seen_at, last_seen_at, observed
                 ) VALUES (1, 'local', 1, 1, 1)",
                [],
            )
            .expect("seed pane");
        connection
            .execute(
                "WITH RECURSIVE event_ids(id) AS (
                     VALUES(1)
                     UNION ALL
                     SELECT id + 1 FROM event_ids WHERE id < ?1
                 )
                 INSERT INTO events (
                     id, pane_id, rule_id, agent_type, event_type, severity,
                     confidence, matched_text, detected_at
                 )
                 SELECT id, 1, 'bulk.annotation', 'test', 'bulk', 'info',
                        1.0, 'event-' || id, id
                 FROM event_ids
                 ORDER BY id ASC",
                [event_count_i64],
            )
            .expect("seed ordered events");
        connection
            .execute(
                "UPDATE events
                 SET triage_state = 'overflow-page',
                     triage_updated_at = 10,
                     triage_updated_by = 'test'
                 WHERE id = 1",
                [],
            )
            .expect("seed overflow-page triage state");
        connection
            .execute(
                "INSERT INTO event_notes (event_id, note, updated_at, updated_by)
                 VALUES (1, 'annotation past bulk cap', 11, 'test')",
                [],
            )
            .expect("seed overflow-page note");
        connection
            .execute(
                "INSERT INTO event_labels (event_id, label, created_at, created_by)
                 VALUES (1, 'overflow', 12, 'test'),
                        (?1, 'first-page', 12, 'test')",
                [event_count_i64],
            )
            .expect("seed annotations on both pages");
        drop(connection);

        let storage = runtime.block_on(async {
            StorageHandle::new(layout.db_path.to_string_lossy().as_ref())
                .await
                .expect("open production storage")
        });
        let wezterm: crate::wezterm::WeztermHandle =
            std::sync::Arc::new(crate::wezterm::MockWezterm::new());
        let client =
            ProductionQueryClient::with_storage_and_wezterm(layout, storage.clone(), wezterm);
        let events = client
            .list_events(&EventFilters {
                limit: event_count,
                ..EventFilters::default()
            })
            .expect("list events with paged annotations");

        assert_eq!(events.len(), event_count);
        assert_eq!(events.first().map(|event| event.id), Some(event_count_i64));
        assert_eq!(events.last().map(|event| event.id), Some(1));
        assert!(events.windows(2).all(|pair| pair[0].id > pair[1].id));
        assert_eq!(events[0].labels, vec!["first-page"]);
        let overflow = events.last().expect("overflow-page event");
        assert_eq!(overflow.triage_state.as_deref(), Some("overflow-page"));
        assert_eq!(
            overflow.note.as_deref(),
            Some("annotation past bulk cap")
        );
        assert_eq!(overflow.labels, vec!["overflow"]);
        let unannotated = events.get(1).expect("unannotated event");
        assert!(unannotated.triage_state.is_none());
        assert!(unannotated.note.is_none());
        assert!(unannotated.labels.is_empty());

        drop(client);
        runtime.block_on(async {
            storage.shutdown().await.expect("shutdown storage");
        });
    }

    #[test]
    fn infer_agent_type_detects_known_agents() {
        assert_eq!(
            infer_agent_type(Some("codex terminal"), None),
            Some("codex".to_string())
        );
        assert_eq!(
            infer_agent_type(Some("Claude Code"), None),
            Some("claude".to_string())
        );
        assert_eq!(
            infer_agent_type(None, Some("/tmp/gemini-run")),
            Some("gemini".to_string())
        );
        assert_eq!(infer_agent_type(Some("plain shell"), None), None);
    }

    // =====================================================================
    // infer_agent_type — exhaustive tests
    // =====================================================================

    #[test]
    fn infer_agent_type_none_none() {
        assert_eq!(infer_agent_type(None, None), None);
    }

    #[test]
    fn infer_agent_type_empty_strings() {
        assert_eq!(infer_agent_type(Some(""), Some("")), None);
    }

    #[test]
    fn infer_agent_type_codex_in_title_case_insensitive() {
        assert_eq!(
            infer_agent_type(Some("CODEX SESSION"), None),
            Some("codex".to_string())
        );
        assert_eq!(
            infer_agent_type(Some("Codex"), None),
            Some("codex".to_string())
        );
    }

    #[test]
    fn infer_agent_type_codex_in_cwd() {
        assert_eq!(
            infer_agent_type(None, Some("/home/user/.codex/workspace")),
            Some("codex".to_string())
        );
    }

    #[test]
    fn infer_agent_type_claude_in_title_case_insensitive() {
        assert_eq!(
            infer_agent_type(Some("CLAUDE code"), None),
            Some("claude".to_string())
        );
        assert_eq!(
            infer_agent_type(Some("claude-code"), None),
            Some("claude".to_string())
        );
    }

    #[test]
    fn infer_agent_type_claude_in_cwd() {
        assert_eq!(
            infer_agent_type(None, Some("/tmp/claude-session")),
            Some("claude".to_string())
        );
    }

    #[test]
    fn infer_agent_type_gemini_in_title() {
        assert_eq!(
            infer_agent_type(Some("gemini chat"), None),
            Some("gemini".to_string())
        );
        assert_eq!(
            infer_agent_type(Some("GEMINI"), None),
            Some("gemini".to_string())
        );
    }

    #[test]
    fn infer_agent_type_gemini_in_cwd() {
        assert_eq!(
            infer_agent_type(None, Some("/workspace/gemini-agent")),
            Some("gemini".to_string())
        );
    }

    #[test]
    fn infer_agent_type_priority_codex_over_claude() {
        // codex is checked first
        assert_eq!(
            infer_agent_type(Some("codex claude gemini"), None),
            Some("codex".to_string())
        );
    }

    #[test]
    fn infer_agent_type_priority_claude_over_gemini() {
        // claude is checked before gemini
        assert_eq!(
            infer_agent_type(Some("claude gemini"), None),
            Some("claude".to_string())
        );
    }

    #[test]
    fn infer_agent_type_title_takes_precedence_over_cwd() {
        // If title matches codex, cwd matching claude doesn't matter
        assert_eq!(
            infer_agent_type(Some("codex"), Some("/claude-dir")),
            Some("codex".to_string())
        );
    }

    #[test]
    fn infer_agent_type_unrecognized_returns_none() {
        assert_eq!(infer_agent_type(Some("vim"), Some("/home/user")), None);
        assert_eq!(infer_agent_type(Some("htop"), None), None);
        assert_eq!(infer_agent_type(Some("bash"), Some("/usr/bin")), None);
    }

    // =====================================================================
    // infer_pane_state — exhaustive tests
    // =====================================================================

    fn make_pane_info() -> PaneInfo {
        PaneInfo {
            pane_id: 1,
            tab_id: 1,
            window_id: 1,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn infer_pane_state_unknown_default() {
        let info = make_pane_info();
        assert_eq!(infer_pane_state(&info), "unknown");
    }

    #[test]
    fn infer_pane_state_alt_screen() {
        let mut info = make_pane_info();
        info.extra.insert(
            "is_alt_screen_active".to_string(),
            serde_json::Value::Bool(true),
        );
        assert_eq!(infer_pane_state(&info), "AltScreen");
    }

    #[test]
    fn infer_pane_state_alt_screen_false() {
        let mut info = make_pane_info();
        info.extra.insert(
            "is_alt_screen_active".to_string(),
            serde_json::Value::Bool(false),
        );
        assert_eq!(infer_pane_state(&info), "unknown");
    }

    #[test]
    fn infer_pane_state_alt_screen_non_bool_ignored() {
        let mut info = make_pane_info();
        info.extra.insert(
            "is_alt_screen_active".to_string(),
            serde_json::Value::String("yes".to_string()),
        );
        // Non-bool values default to false via and_then(as_bool)
        assert_eq!(infer_pane_state(&info), "unknown");
    }

    #[test]
    fn infer_pane_state_cursor_hidden() {
        let mut info = make_pane_info();
        info.cursor_visibility = Some(crate::wezterm::CursorVisibility::Hidden);
        assert_eq!(infer_pane_state(&info), "CommandRunning");
    }

    #[test]
    fn infer_pane_state_cursor_visible_not_active() {
        let mut info = make_pane_info();
        info.cursor_visibility = Some(crate::wezterm::CursorVisibility::Visible);
        info.is_active = false;
        assert_eq!(infer_pane_state(&info), "unknown");
    }

    #[test]
    fn infer_pane_state_prompt_active() {
        let mut info = make_pane_info();
        info.is_active = true;
        assert_eq!(infer_pane_state(&info), "PromptActive");
    }

    #[test]
    fn infer_pane_state_alt_screen_takes_priority_over_cursor() {
        let mut info = make_pane_info();
        info.extra.insert(
            "is_alt_screen_active".to_string(),
            serde_json::Value::Bool(true),
        );
        info.cursor_visibility = Some(crate::wezterm::CursorVisibility::Hidden);
        info.is_active = true;
        assert_eq!(infer_pane_state(&info), "AltScreen");
    }

    #[test]
    fn infer_pane_state_cursor_hidden_takes_priority_over_active() {
        let mut info = make_pane_info();
        info.cursor_visibility = Some(crate::wezterm::CursorVisibility::Hidden);
        info.is_active = true;
        assert_eq!(infer_pane_state(&info), "CommandRunning");
    }

    // =====================================================================
    // PaneView::from — conversion tests
    // =====================================================================

    #[test]
    fn pane_view_from_pane_info_basic() {
        let info = make_pane_info();
        let view = PaneView::from(&info);
        assert_eq!(view.pane_id, 1);
        assert_eq!(view.title, "");
        assert_eq!(view.domain, "local");
        assert!(view.cwd.is_none());
        assert!(!view.is_excluded);
        assert!(view.agent_type.is_none());
        assert_eq!(view.pane_state, "unknown");
        assert!(view.last_activity_ts.is_none());
        assert_eq!(view.unhandled_event_count, 0);
    }

    #[test]
    fn v35_pane_storage_aggregates_preserve_missing_and_empty_activity_semantics() {
        let mut first = PaneView::from(&make_pane_info());
        let mut second = first.clone();
        second.pane_id = 2;
        second.last_activity_ts = Some(999);
        second.unhandled_event_count = 999;
        let mut third = first.clone();
        third.pane_id = 3;
        third.last_activity_ts = Some(999);
        third.unhandled_event_count = 999;
        first.last_activity_ts = Some(999);
        first.unhandled_event_count = 999;
        let mut panes = vec![first, second, third];

        let unhandled_by_pane = std::collections::HashMap::from([(1, 7), (2, 0)]);
        let last_activity_by_pane =
            std::collections::HashMap::from([(1, Some(123)), (2, None)]);
        apply_pane_storage_aggregates(
            &mut panes,
            &unhandled_by_pane,
            &last_activity_by_pane,
        );

        assert_eq!(panes[0].unhandled_event_count, 7);
        assert_eq!(panes[0].last_activity_ts, Some(123));
        assert_eq!(panes[1].unhandled_event_count, 0);
        assert_eq!(panes[1].last_activity_ts, None);
        assert_eq!(panes[2].unhandled_event_count, 0);
        assert_eq!(panes[2].last_activity_ts, None);
    }

    #[test]
    fn pane_view_from_with_title_and_cwd() {
        let mut info = make_pane_info();
        info.title = Some("Claude Code".to_string());
        info.cwd = Some("/home/user/project".to_string());
        let view = PaneView::from(&info);
        assert_eq!(view.title, "Claude Code");
        assert_eq!(view.cwd, Some("/home/user/project".to_string()));
        assert_eq!(view.agent_type, Some("claude".to_string()));
    }

    #[test]
    fn pane_view_from_with_domain_name() {
        let mut info = make_pane_info();
        info.domain_name = Some("ssh:remote".to_string());
        let view = PaneView::from(&info);
        assert_eq!(view.domain, "ssh:remote");
    }

    #[test]
    fn pane_view_from_with_active_pane() {
        let mut info = make_pane_info();
        info.is_active = true;
        let view = PaneView::from(&info);
        assert_eq!(view.pane_state, "PromptActive");
    }

    #[test]
    fn pane_view_from_with_alt_screen() {
        let mut info = make_pane_info();
        info.extra.insert(
            "is_alt_screen_active".to_string(),
            serde_json::Value::Bool(true),
        );
        let view = PaneView::from(&info);
        assert_eq!(view.pane_state, "AltScreen");
    }

    // =====================================================================
    // QueryError tests
    // =====================================================================

    #[test]
    fn query_error_display_watcher_not_running() {
        let e = QueryError::WatcherNotRunning;
        assert_eq!(e.to_string(), "Watcher is not running");
    }

    #[test]
    fn query_error_display_database_not_initialized() {
        let e = QueryError::DatabaseNotInitialized("no db file".into());
        let msg = e.to_string();
        assert!(msg.contains("Database not initialized"));
        assert!(msg.contains("no db file"));
    }

    #[test]
    fn query_error_display_wezterm_error() {
        let e = QueryError::WeztermError("connection refused".into());
        let msg = e.to_string();
        assert!(msg.contains("WezTerm error"));
        assert!(msg.contains("connection refused"));
    }

    #[test]
    fn query_error_display_storage_error() {
        let e = QueryError::StorageError("disk full".into());
        let msg = e.to_string();
        assert!(msg.contains("Storage error"));
        assert!(msg.contains("disk full"));
    }

    #[test]
    fn query_error_display_query_failed() {
        let e = QueryError::QueryFailed("syntax error".into());
        let msg = e.to_string();
        assert!(msg.contains("Query failed"));
        assert!(msg.contains("syntax error"));
    }

    #[test]
    fn query_error_debug_contains_variant_name() {
        let e = QueryError::WatcherNotRunning;
        let dbg = format!("{e:?}");
        assert!(dbg.contains("WatcherNotRunning"));
    }

    // =====================================================================
    // EventFilters tests
    // =====================================================================

    #[test]
    fn event_filters_default_values() {
        let f = EventFilters::default();
        assert!(f.pane_id.is_none());
        assert!(f.rule_id.is_none());
        assert!(f.event_type.is_none());
        assert!(!f.unhandled_only);
        assert_eq!(f.limit, 0);
    }

    #[test]
    fn event_filters_clone() {
        let f = EventFilters {
            pane_id: Some(42),
            rule_id: Some("error_pattern".into()),
            event_type: Some("pattern".into()),
            unhandled_only: true,
            limit: 100,
        };
        let f2 = f.clone();
        assert_eq!(f2.pane_id, Some(42));
        assert_eq!(f2.rule_id, Some("error_pattern".into()));
        assert_eq!(f2.event_type, Some("pattern".into()));
        assert!(f2.unhandled_only);
        assert_eq!(f2.limit, 100);
    }

    #[test]
    fn event_filters_debug() {
        let f = EventFilters::default();
        let dbg = format!("{f:?}");
        assert!(dbg.contains("EventFilters"));
    }

    // =====================================================================
    // View struct construction tests
    // =====================================================================

    #[test]
    fn event_view_construction_and_clone() {
        let ev = EventView {
            id: 1,
            rule_id: "test_rule".to_string(),
            pane_id: 42,
            severity: "error".to_string(),
            message: "Something broke".to_string(),
            timestamp: 1_700_000_000_000,
            handled: false,
            triage_state: Some("open".to_string()),
            labels: vec!["critical".to_string()],
            note: Some("investigate".to_string()),
        };
        let ev2 = ev.clone();
        assert_eq!(ev2.id, 1);
        assert_eq!(ev2.rule_id, "test_rule");
        assert_eq!(ev2.pane_id, 42);
        assert_eq!(ev2.severity, "error");
        assert_eq!(ev2.message, "Something broke");
        assert!(!ev2.handled);
        assert_eq!(ev2.triage_state, Some("open".to_string()));
        assert_eq!(ev2.labels.len(), 1);
        assert_eq!(ev2.note, Some("investigate".to_string()));
    }

    #[test]
    fn event_view_debug() {
        let ev = EventView {
            id: 0,
            rule_id: String::new(),
            pane_id: 0,
            severity: String::new(),
            message: String::new(),
            timestamp: 0,
            handled: true,
            triage_state: None,
            labels: Vec::new(),
            note: None,
        };
        let dbg = format!("{ev:?}");
        assert!(dbg.contains("EventView"));
    }

    #[test]
    fn triage_action_construction_and_clone() {
        let a = TriageAction {
            label: "Fix it".to_string(),
            command: "ft fix --auto".to_string(),
        };
        let a2 = a.clone();
        assert_eq!(a2.label, "Fix it");
        assert_eq!(a2.command, "ft fix --auto");
    }

    #[test]
    fn triage_item_view_construction() {
        let item = TriageItemView {
            section: "events".to_string(),
            severity: "warning".to_string(),
            title: "Test item".to_string(),
            detail: "Some detail".to_string(),
            actions: vec![TriageAction {
                label: "Fix".to_string(),
                command: "ft fix".to_string(),
            }],
            event_id: Some(10),
            pane_id: Some(5),
            workflow_id: None,
        };
        let item2 = item.clone();
        assert_eq!(item2.section, "events");
        assert_eq!(item2.actions.len(), 1);
        assert_eq!(item2.event_id, Some(10));
        assert!(item2.workflow_id.is_none());
    }

    #[test]
    fn search_result_view_construction() {
        let sr = SearchResultView {
            pane_id: 7,
            timestamp: 12345,
            snippet: "match here".to_string(),
            rank: 0.95,
        };
        let sr2 = sr.clone();
        assert_eq!(sr2.pane_id, 7);
        assert_eq!(sr2.timestamp, 12345);
        assert_eq!(sr2.snippet, "match here");
        assert!((sr2.rank - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn workflow_progress_view_construction() {
        let wf = WorkflowProgressView {
            id: "wf-1".to_string(),
            workflow_name: "auto-fix".to_string(),
            pane_id: 3,
            current_step: 2,
            total_steps: 5,
            status: "running".to_string(),
            error: None,
            started_at: 1000,
            updated_at: 2000,
        };
        let wf2 = wf.clone();
        assert_eq!(wf2.id, "wf-1");
        assert_eq!(wf2.current_step, 2);
        assert_eq!(wf2.total_steps, 5);
        assert!(wf2.error.is_none());
    }

    #[test]
    fn history_entry_view_construction() {
        let h = HistoryEntryView {
            audit_id: 100,
            timestamp: 5000,
            pane_id: Some(1),
            workflow_id: Some("wf-x".to_string()),
            action_kind: "send_text".to_string(),
            result: "success".to_string(),
            actor_kind: "robot".to_string(),
            step_name: Some("step1".to_string()),
            undoable: true,
            undone: false,
            undo_strategy: Some("workflow_abort".to_string()),
            undo_hint: None,
            rule_id: Some("r1".to_string()),
            summary: "sent text to pane".to_string(),
        };
        let h2 = h.clone();
        assert_eq!(h2.audit_id, 100);
        assert!(h2.undoable);
        assert!(!h2.undone);
        assert_eq!(h2.actor_kind, "robot");
        assert_eq!(h2.summary, "sent text to pane");
    }

    #[test]
    fn health_status_construction() {
        let hs = HealthStatus {
            watcher_running: true,
            db_accessible: true,
            wezterm_accessible: false,
            wezterm_circuit: CircuitBreakerStatus::default(),
            pane_count: 5,
            event_count: 100,
            last_capture_ts: Some(999),
        };
        let hs2 = hs.clone();
        assert!(hs2.watcher_running);
        assert!(!hs2.wezterm_accessible);
        assert_eq!(hs2.pane_count, 5);
        assert_eq!(hs2.last_capture_ts, Some(999));
    }

    // =====================================================================
    // PaneView direct construction and field tests
    // =====================================================================

    #[test]
    fn pane_view_clone_and_debug() {
        let pv = PaneView {
            pane_id: 99,
            title: "my pane".to_string(),
            domain: "local".to_string(),
            cwd: Some("/tmp".to_string()),
            is_excluded: true,
            agent_type: Some("codex".to_string()),
            pane_state: "AltScreen".to_string(),
            last_activity_ts: Some(42),
            unhandled_event_count: 3,
        };
        let pv2 = pv.clone();
        assert_eq!(pv2.pane_id, 99);
        assert!(pv2.is_excluded);
        assert_eq!(pv2.unhandled_event_count, 3);
        let dbg = format!("{pv:?}");
        assert!(dbg.contains("PaneView"));
        assert!(dbg.contains("99"));
    }

    // =====================================================================
    // MockQueryClient — trait method tests
    // =====================================================================

    #[test]
    fn mock_client_list_events_empty() {
        let client = MockQueryClient::new();
        let events = client.list_events(&EventFilters::default()).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn mock_client_triage_items() {
        let client = MockQueryClient::new();
        let items = client.list_triage_items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, "warning");
        assert_eq!(items[0].section, "events");
    }

    #[test]
    fn mock_client_search_empty() {
        let client = MockQueryClient::new();
        let results = client.search("test", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn mock_client_mark_event_muted() {
        let client = MockQueryClient::new();
        assert!(client.mark_event_muted(1).is_ok());
    }

    #[test]
    fn mock_client_list_active_workflows_empty() {
        let client = MockQueryClient::new();
        let workflows = client.list_active_workflows().unwrap();
        assert!(workflows.is_empty());
    }

    #[test]
    fn mock_client_watcher_running() {
        let client = MockQueryClient::new();
        assert!(client.is_watcher_running());
    }

    #[test]
    fn mock_client_watcher_not_running() {
        let mut client = MockQueryClient::new();
        client.watcher_running = false;
        assert!(!client.is_watcher_running());
        let health = client.health().unwrap();
        assert!(!health.watcher_running);
    }

    // =====================================================================
    // QueryClient default method implementations
    // =====================================================================

    #[test]
    fn query_client_default_list_action_history() {
        let client = MockQueryClient::new();
        let history = client.list_action_history(10).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn query_client_default_list_pane_bookmarks() {
        let client = MockQueryClient::new();
        let bookmarks = client.list_pane_bookmarks().unwrap();
        assert!(bookmarks.is_empty());
    }

    #[test]
    fn query_client_default_list_saved_searches() {
        let client = MockQueryClient::new();
        let searches = client.list_saved_searches().unwrap();
        assert!(searches.is_empty());
    }

    #[test]
    fn query_client_default_ruleset_profile_state() {
        let client = MockQueryClient::new();
        let state = client.ruleset_profile_state().unwrap();
        // Default should be the default value
        let dbg = format!("{state:?}");
        assert!(dbg.contains("RulesetProfileState"));
    }

    #[test]
    fn query_client_default_get_timeline() {
        let client = MockQueryClient::new();
        let timeline = client.get_timeline(1000, 50).unwrap();
        assert_eq!(timeline.total_count, 0);
        assert!(!timeline.has_more);
        assert!(timeline.events.is_empty());
    }

    // =====================================================================
    // epoch_ms sanity test
    // =====================================================================

    #[test]
    fn epoch_ms_returns_positive_value() {
        let ms = epoch_ms();
        // Should be after 2024-01-01 in epoch ms
        assert!(ms > 1_704_067_200_000);
    }

    #[test]
    fn epoch_ms_is_monotonic_ish() {
        let ms1 = epoch_ms();
        let ms2 = epoch_ms();
        assert!(ms2 >= ms1);
    }
}
