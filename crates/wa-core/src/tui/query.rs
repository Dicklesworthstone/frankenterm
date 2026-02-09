//! Query client abstraction for TUI data access
//!
//! The `QueryClient` trait provides a clean abstraction over the wa-core
//! query layer, enabling:
//!
//! - Testability: Mock implementations for unit tests
//! - Consistency: Same data access patterns as robot mode
//! - Decoupling: UI doesn't know about SQLite or storage internals

use std::path::PathBuf;

use crate::circuit_breaker::CircuitBreakerStatus;
use crate::config::WorkspaceLayout;
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

/// Abstraction over wa-core query layer for TUI data access
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
}

/// Production implementation of QueryClient
///
/// Uses the actual wa-core storage and wezterm client to query data.
/// Owns a dedicated tokio runtime for async operations, avoiding
/// "cannot start a runtime from within a runtime" panics when the TUI
/// runs in a separate thread from the main async context.
pub struct ProductionQueryClient {
    workspace_layout: WorkspaceLayout,
    config_path: Option<PathBuf>,
    wezterm: WeztermHandle,
    #[allow(dead_code)]
    storage: Option<StorageHandle>,
    /// Dedicated runtime for async operations - avoids nested runtime panics
    runtime: tokio::runtime::Runtime,
}

impl ProductionQueryClient {
    /// Create a new production query client with a dedicated tokio runtime.
    ///
    /// The runtime is used to bridge sync TUI code with async operations,
    /// avoiding "cannot start a runtime from within a runtime" panics.
    #[must_use]
    pub fn new(workspace_layout: WorkspaceLayout) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("tui-query-runtime")
            .build()
            .expect("Failed to create TUI query runtime");

        Self {
            workspace_layout,
            config_path: crate::config::resolve_config_path(None),
            wezterm: default_wezterm_handle(),
            storage: None,
            runtime,
        }
    }

    /// Create with an existing storage handle and a dedicated tokio runtime.
    ///
    /// The runtime is used to bridge sync TUI code with async operations,
    /// avoiding "cannot start a runtime from within a runtime" panics.
    #[must_use]
    pub fn with_storage(workspace_layout: WorkspaceLayout, storage: StorageHandle) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("tui-query-runtime")
            .build()
            .expect("Failed to create TUI query runtime");

        Self {
            workspace_layout,
            config_path: crate::config::resolve_config_path(None),
            wezterm: default_wezterm_handle(),
            storage: Some(storage),
            runtime,
        }
    }

    /// Create with a custom WezTerm interface (useful for tests/mocks).
    #[must_use]
    pub fn with_wezterm(workspace_layout: WorkspaceLayout, wezterm: WeztermHandle) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("tui-query-runtime")
            .build()
            .expect("Failed to create TUI query runtime");

        Self {
            workspace_layout,
            config_path: crate::config::resolve_config_path(None),
            wezterm,
            storage: None,
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("tui-query-runtime")
            .build()
            .expect("Failed to create TUI query runtime");

        Self {
            workspace_layout,
            config_path: crate::config::resolve_config_path(None),
            wezterm,
            storage: Some(storage),
            runtime,
        }
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
                let (unhandled_res, last_activity_res) = tokio::join!(
                    storage.count_unhandled_events_by_pane(),
                    storage.get_last_activity_by_pane()
                );
                let unhandled_by_pane = unhandled_res.unwrap_or_default();
                let last_activity_by_pane = last_activity_res.unwrap_or_default();

                for pane in &mut pane_views {
                    pane.unhandled_event_count =
                        *unhandled_by_pane.get(&pane.pane_id).unwrap_or(&0_u32);
                    pane.last_activity_ts = last_activity_by_pane.get(&pane.pane_id).copied();
                }
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
            let events = storage
                .get_events(query)
                .await
                .map_err(|e| QueryError::StorageError(e.to_string()))?;

            let mut rows = Vec::with_capacity(events.len());
            for event in events {
                let annotations = match storage.get_event_annotations(event.id).await {
                    Ok(Some(annotations)) => annotations,
                    Ok(None) => crate::storage::EventAnnotations::default(),
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            event_id = event.id,
                            "Failed to load event annotations for TUI",
                        );
                        crate::storage::EventAnnotations::default()
                    }
                };
                rows.push((event, annotations));
            }
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
                        action("Run diagnostics", "wa doctor".to_string()),
                        action("Machine diagnostics", "wa doctor --json".to_string()),
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
                let msg = if report.message.len() > 100 {
                    format!("{}...", &report.message[..97])
                } else {
                    report.message.clone()
                };
                format!(
                    "{msg} (at {})",
                    report.location.as_deref().unwrap_or("unknown")
                )
            } else if let Some(ref manifest) = bundle.manifest {
                format!("crash at {}", manifest.created_at)
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
                        "wa reproduce --kind crash".to_string(),
                    ),
                    action("Run diagnostics", "wa doctor".to_string()),
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
                    action("Start watcher", "wa watch".to_string()),
                    action("Run diagnostics", "wa doctor".to_string()),
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
                        format!("wa events --pane {} --unhandled", event.pane_id),
                    ),
                    action(
                        "Explain detection",
                        format!("wa why --recent --pane {}", event.pane_id),
                    ),
                    action("Show pane details", format!("wa show {}", event.pane_id)),
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
                        format!("wa workflow status {}", wf.id),
                    ),
                    action(
                        "Explain decisions",
                        format!("wa why --recent --pane {}", wf.pane_id),
                    ),
                    action("Show pane details", format!("wa show {}", wf.pane_id)),
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

        Ok(HealthStatus {
            watcher_running,
            db_accessible,
            wezterm_accessible,
            wezterm_circuit: self.wezterm.circuit_status(),
            pane_count,
            event_count: 0,
            last_capture_ts: None,
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
            if let Ok(Some(identity_key)) = storage.get_event_identity_key(event_id).await {
                let record = EventMuteRecord {
                    identity_key,
                    scope: "workspace".to_string(),
                    created_at: epoch_ms(),
                    expires_at: None,
                    created_by: None,
                    reason: Some("tui mute".to_string()),
                };
                storage
                    .add_event_mute(record)
                    .await
                    .map_err(|e| QueryError::StorageError(e.to_string()))?;
            }

            storage
                .mark_event_handled(event_id, None, "muted")
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
            .block_on(storage.get_timeline(query))
            .map_err(|e| QueryError::StorageError(e.to_string()))
    }
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
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
                        command: "wa why --recent --pane 0".to_string(),
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
}
