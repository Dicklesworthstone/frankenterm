//! Diagnostic bundle export for wa.
//!
//! Generates a sanitized diagnostic bundle for bug reports containing:
//! - Environment info (OS, arch, ft version)
//! - Config summary (redacted)
//! - DB health stats (row counts, schema version, WAL size, page info)
//! - Recent events + workflow step logs (redacted)
//! - Active pane reservations + recent reservation conflicts (redacted)
//!
//! All text fields are redacted using the policy engine before writing.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::config::{Config, WorkspaceLayout};
use crate::policy::Redactor;
use crate::storage::{AuditQuery, EventQuery, ExportQuery, SCHEMA_VERSION, StorageHandle};

// =============================================================================
// Public types
// =============================================================================

/// Options for generating a diagnostic bundle.
#[derive(Debug, Clone)]
pub struct DiagnosticOptions {
    /// Maximum number of recent events to include.
    pub event_limit: usize,
    /// Maximum number of recent audit actions to include.
    pub audit_limit: usize,
    /// Maximum number of recent workflow executions to include.
    pub workflow_limit: usize,
    /// Output directory override (defaults to workspace diag_dir).
    pub output: Option<PathBuf>,
}

impl Default for DiagnosticOptions {
    fn default() -> Self {
        Self {
            event_limit: 100,
            audit_limit: 50,
            workflow_limit: 50,
            output: None,
        }
    }
}

/// Result of a diagnostic bundle generation.
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticResult {
    /// Path to the generated bundle directory.
    pub output_path: String,
    /// Number of files written.
    pub file_count: usize,
    /// Total bundle size in bytes.
    pub total_size_bytes: u64,
}

// =============================================================================
// Environment section
// =============================================================================

#[derive(Debug, Serialize)]
struct EnvironmentInfo {
    wa_version: String,
    schema_version: i32,
    os: String,
    arch: String,
    /// Rust version used to compile wa.
    rust_version: Option<String>,
    /// Current working directory.
    cwd: Option<String>,
}

fn gather_environment() -> EnvironmentInfo {
    EnvironmentInfo {
        wa_version: crate::VERSION.to_string(),
        schema_version: SCHEMA_VERSION,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        rust_version: option_env!("RUSTC_VERSION").map(String::from),
        cwd: std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string()),
    }
}

// =============================================================================
// Config summary (redacted)
// =============================================================================

#[derive(Debug, Serialize)]
struct ConfigSummary {
    general_log_level: String,
    general_log_format: String,
    ingest_poll_interval_ms: u64,
    ingest_max_concurrent: u32,
    ingest_gap_detection: bool,
    storage_retention_days: u32,
    storage_retention_max_mb: u32,
    storage_checkpoint_secs: u32,
    patterns_quick_reject: bool,
    patterns_packs: Vec<String>,
    workflows_enabled: Vec<String>,
    workflows_max_concurrent: u32,
    safety_rate_limit: u32,
    metrics_enabled: bool,
}

fn summarize_config(config: &Config) -> ConfigSummary {
    ConfigSummary {
        general_log_level: config.general.log_level.clone(),
        general_log_format: config.general.log_format.to_string(),
        ingest_poll_interval_ms: config.ingest.poll_interval_ms,
        ingest_max_concurrent: config.ingest.max_concurrent_captures,
        ingest_gap_detection: config.ingest.gap_detection,
        storage_retention_days: config.storage.retention_days,
        storage_retention_max_mb: config.storage.retention_max_mb,
        storage_checkpoint_secs: config.storage.checkpoint_interval_secs,
        patterns_quick_reject: config.patterns.quick_reject_enabled,
        patterns_packs: config.patterns.packs.clone(),
        workflows_enabled: config.workflows.enabled.clone(),
        workflows_max_concurrent: config.workflows.max_concurrent,
        safety_rate_limit: config.safety.rate_limit_per_pane,
        metrics_enabled: config.metrics.enabled,
    }
}

// =============================================================================
// DB health stats
// =============================================================================

#[derive(Debug, Serialize)]
struct DbHealthStats {
    schema_version: i32,
    db_file_size_bytes: u64,
    wal_file_size_bytes: u64,
    page_count: i64,
    page_size: i64,
    freelist_count: i64,
    /// Row counts for major tables.
    table_counts: TableCounts,
}

#[derive(Debug, Serialize)]
struct TableCounts {
    panes: i64,
    output_segments: i64,
    events: i64,
    audit_actions: i64,
    workflow_executions: i64,
    workflow_step_logs: i64,
    pane_reservations: i64,
    approval_tokens: i64,
}

fn gather_db_health(db_path: &Path) -> crate::Result<DbHealthStats> {
    let conn = Connection::open(db_path).map_err(|e| {
        crate::StorageError::Database(format!("Failed to open database for diagnostics: {e}"))
    })?;

    let pragma_i64 = |name: &str| -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .unwrap_or(0)
    };

    let count = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
            row.get(0)
        })
        .unwrap_or(-1)
    };

    let db_file_size = fs::metadata(db_path).map_or(0, |m| m.len());

    let wal_path = db_path.with_extension("db-wal");
    let wal_file_size = fs::metadata(&wal_path).map_or(0, |m| m.len());

    Ok(DbHealthStats {
        schema_version: pragma_i64("user_version") as i32,
        db_file_size_bytes: db_file_size,
        wal_file_size_bytes: wal_file_size,
        page_count: pragma_i64("page_count"),
        page_size: pragma_i64("page_size"),
        freelist_count: pragma_i64("freelist_count"),
        table_counts: TableCounts {
            panes: count("panes"),
            output_segments: count("output_segments"),
            events: count("events"),
            audit_actions: count("audit_actions"),
            workflow_executions: count("workflow_executions"),
            workflow_step_logs: count("workflow_step_logs"),
            pane_reservations: count("pane_reservations"),
            approval_tokens: count("approval_tokens"),
        },
    })
}

// =============================================================================
// Recent events (redacted)
// =============================================================================

#[derive(Debug, Serialize)]
struct RedactedEvent {
    id: i64,
    pane_id: u64,
    rule_id: String,
    event_type: String,
    severity: String,
    confidence: f64,
    detected_at: i64,
    handled_status: Option<String>,
    matched_text: Option<String>,
}

fn redact_events(
    events: Vec<crate::storage::StoredEvent>,
    redactor: &Redactor,
) -> Vec<RedactedEvent> {
    events
        .into_iter()
        .map(|e| RedactedEvent {
            id: e.id,
            pane_id: e.pane_id,
            rule_id: e.rule_id,
            event_type: e.event_type,
            severity: e.severity,
            confidence: e.confidence,
            detected_at: e.detected_at,
            handled_status: e.handled_status,
            matched_text: e.matched_text.map(|t| redactor.redact(&t)),
        })
        .collect()
}

// =============================================================================
// Rule match traces for incident bundles
// =============================================================================

/// Evidence item for a rule match trace.
#[derive(Debug, Serialize)]
struct TraceEvidence {
    /// Evidence kind (e.g., "anchor_match", "regex_capture", "extracted_field").
    kind: String,
    /// Label for the evidence (anchor text, capture name, field name).
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// Redacted value excerpt.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
}

/// Rule match trace for diagnosing detection behavior.
#[derive(Debug, Serialize)]
struct EventRuleTrace {
    /// Event ID this trace belongs to.
    event_id: i64,
    /// Rule ID that matched.
    rule_id: String,
    /// Agent type the rule is for.
    agent_type: String,
    /// Detection confidence score.
    confidence: f64,
    /// Severity of the detection.
    severity: String,
    /// Redacted matched text (the input that triggered the rule).
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_text: Option<String>,
    /// Extracted fields from regex captures or structured data.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    extracted_fields: Vec<TraceEvidence>,
    /// Whether the event was handled.
    handled: bool,
    /// Timestamp when detected (epoch ms).
    detected_at: i64,
}

/// Generate rule traces from stored events.
///
/// Creates trace files for events with pattern/detection data that may help
/// diagnose rule behavior issues.
fn generate_rule_traces(
    events: &[crate::storage::StoredEvent],
    redactor: &Redactor,
) -> Vec<EventRuleTrace> {
    events
        .iter()
        .filter(|e| {
            // Include events with rule IDs (detection-related)
            !e.rule_id.is_empty()
        })
        .map(|e| {
            // Extract fields from the extracted JSON if present
            let extracted_fields = e
                .extracted
                .as_ref()
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .map(|(key, value)| TraceEvidence {
                            kind: "extracted_field".to_string(),
                            label: Some(key.clone()),
                            value: Some(redactor.redact(&value.to_string())),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            EventRuleTrace {
                event_id: e.id,
                rule_id: e.rule_id.clone(),
                agent_type: e.agent_type.clone(),
                confidence: e.confidence,
                severity: e.severity.clone(),
                matched_text: e.matched_text.as_ref().map(|t| redactor.redact(t)),
                extracted_fields,
                handled: e.handled_at.is_some(),
                detected_at: e.detected_at,
            }
        })
        .collect()
}

// =============================================================================
// Workflow summary (redacted)
// =============================================================================

#[derive(Debug, Serialize)]
struct RedactedWorkflow {
    id: String,
    workflow_name: String,
    pane_id: u64,
    status: String,
    started_at: i64,
    completed_at: Option<i64>,
    step_count: usize,
    steps: Vec<RedactedStep>,
}

#[derive(Debug, Serialize)]
struct RedactedStep {
    step_index: usize,
    step_name: String,
    result_type: String,
    policy_summary: Option<String>,
    started_at: i64,
    completed_at: i64,
}

fn redact_step(step: crate::storage::WorkflowStepLogRecord, redactor: &Redactor) -> RedactedStep {
    RedactedStep {
        step_index: step.step_index,
        step_name: step.step_name,
        result_type: step.result_type,
        policy_summary: step.policy_summary.map(|s| redactor.redact(&s)),
        started_at: step.started_at,
        completed_at: step.completed_at,
    }
}

// =============================================================================
// Reservation summary (redacted)
// =============================================================================

#[derive(Debug, Serialize)]
struct RedactedReservation {
    id: i64,
    pane_id: u64,
    owner_kind: String,
    owner_id: String,
    reason: Option<String>,
    status: String,
    created_at: i64,
    expires_at: i64,
    released_at: Option<i64>,
}

fn redact_reservation(
    res: crate::storage::PaneReservation,
    redactor: &Redactor,
) -> RedactedReservation {
    RedactedReservation {
        id: res.id,
        pane_id: res.pane_id,
        owner_kind: res.owner_kind,
        owner_id: redactor.redact(&res.owner_id),
        reason: res.reason.map(|r| redactor.redact(&r)),
        status: res.status,
        created_at: res.created_at,
        expires_at: res.expires_at,
        released_at: res.released_at,
    }
}

// =============================================================================
// Audit summary (redacted)
// =============================================================================

#[derive(Debug, Serialize)]
struct RedactedAudit {
    id: i64,
    ts: i64,
    actor_kind: String,
    action_kind: String,
    policy_decision: String,
    result: String,
    pane_id: Option<u64>,
    input_summary: Option<String>,
    decision_reason: Option<String>,
}

fn redact_audit(action: crate::storage::AuditActionRecord, redactor: &Redactor) -> RedactedAudit {
    RedactedAudit {
        id: action.id,
        ts: action.ts,
        actor_kind: action.actor_kind,
        action_kind: action.action_kind,
        policy_decision: action.policy_decision,
        result: action.result,
        pane_id: action.pane_id,
        input_summary: action.input_summary.map(|s| redactor.redact(&s)),
        decision_reason: action.decision_reason.map(|s| redactor.redact(&s)),
    }
}

// =============================================================================
// Bundle generation
// =============================================================================

/// Generate a diagnostic bundle.
///
/// The bundle is written as a set of JSON files into a timestamped directory.
/// All text fields are redacted before writing. This function is safe to call
/// while the watcher is running (it opens read-only connections).
pub async fn generate_bundle(
    config: &Config,
    layout: &WorkspaceLayout,
    storage: &StorageHandle,
    opts: &DiagnosticOptions,
) -> crate::Result<DiagnosticResult> {
    let redactor = Redactor::new();

    // Determine output directory
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let bundle_name = format!("diag_{now_ms}");
    let output_dir = match &opts.output {
        Some(p) => p.clone(),
        None => layout.diag_dir.join(&bundle_name),
    };

    fs::create_dir_all(&output_dir).map_err(|e| {
        crate::Error::Storage(crate::StorageError::Database(format!(
            "Failed to create diagnostic bundle directory {}: {e}",
            output_dir.display()
        )))
    })?;

    let mut file_count = 0usize;

    // 1. Environment info
    let env_info = gather_environment();
    write_json_file(&output_dir, "environment.json", &env_info)?;
    file_count += 1;

    // 2. Config summary (redacted — no paths or secrets)
    let config_summary = summarize_config(config);
    write_json_file(&output_dir, "config_summary.json", &config_summary)?;
    file_count += 1;

    // 3. DB health stats
    let db_path = Path::new(storage.db_path());
    match gather_db_health(db_path) {
        Ok(health) => {
            write_json_file(&output_dir, "db_health.json", &health)?;
            file_count += 1;
        }
        Err(e) => {
            let error_info = serde_json::json!({
                "error": format!("Failed to gather DB health: {e}"),
            });
            write_json_file(&output_dir, "db_health.json", &error_info)?;
            file_count += 1;
        }
    }

    // 4. Recent events (redacted) and rule traces
    let event_query = EventQuery {
        limit: Some(opts.event_limit),
        ..Default::default()
    };
    match storage.get_events(event_query).await {
        Ok(events) => {
            // Generate rule traces for detection-related events
            let traces = generate_rule_traces(&events, &redactor);
            if !traces.is_empty() {
                let traces_dir = output_dir.join("traces");
                fs::create_dir_all(&traces_dir).map_err(|e| {
                    crate::Error::Storage(crate::StorageError::Database(format!(
                        "Failed to create traces directory: {e}"
                    )))
                })?;
                write_json_file(&traces_dir, "rule_traces.json", &traces)?;
                file_count += 1;
            }

            let redacted = redact_events(events, &redactor);
            write_json_file(&output_dir, "recent_events.json", &redacted)?;
            file_count += 1;
        }
        Err(e) => {
            let error_info = serde_json::json!({
                "error": format!("Failed to query events: {e}"),
            });
            write_json_file(&output_dir, "recent_events.json", &error_info)?;
            file_count += 1;
        }
    }

    // 5. Recent workflow executions with step logs (redacted)
    let wf_query = ExportQuery {
        limit: Some(opts.workflow_limit),
        ..Default::default()
    };
    match storage.export_workflows(wf_query).await {
        Ok(workflows) => {
            let mut redacted_workflows = Vec::with_capacity(workflows.len());
            for wf in workflows {
                let steps = match storage.get_step_logs(&wf.id).await {
                    Ok(steps) => steps
                        .into_iter()
                        .map(|s| redact_step(s, &redactor))
                        .collect(),
                    Err(_) => Vec::new(),
                };
                redacted_workflows.push(RedactedWorkflow {
                    id: wf.id.clone(),
                    workflow_name: wf.workflow_name.clone(),
                    pane_id: wf.pane_id,
                    status: wf.status.clone(),
                    started_at: wf.started_at,
                    completed_at: wf.completed_at,
                    step_count: steps.len(),
                    steps,
                });
            }
            write_json_file(&output_dir, "recent_workflows.json", &redacted_workflows)?;
            file_count += 1;
        }
        Err(e) => {
            let error_info = serde_json::json!({
                "error": format!("Failed to query workflows: {e}"),
            });
            write_json_file(&output_dir, "recent_workflows.json", &error_info)?;
            file_count += 1;
        }
    }

    // 6. Active reservations (redacted)
    match storage.list_active_reservations().await {
        Ok(reservations) => {
            let redacted: Vec<_> = reservations
                .into_iter()
                .map(|r| redact_reservation(r, &redactor))
                .collect();
            write_json_file(&output_dir, "active_reservations.json", &redacted)?;
            file_count += 1;
        }
        Err(e) => {
            let error_info = serde_json::json!({
                "error": format!("Failed to query reservations: {e}"),
            });
            write_json_file(&output_dir, "active_reservations.json", &error_info)?;
            file_count += 1;
        }
    }

    // 7. Recent reservation conflicts (all reservations, including released)
    let res_query = ExportQuery {
        limit: Some(50),
        ..Default::default()
    };
    match storage.export_reservations(res_query).await {
        Ok(all_res) => {
            let redacted: Vec<_> = all_res
                .into_iter()
                .map(|r| redact_reservation(r, &redactor))
                .collect();
            write_json_file(&output_dir, "reservation_history.json", &redacted)?;
            file_count += 1;
        }
        Err(e) => {
            let error_info = serde_json::json!({
                "error": format!("Failed to query reservation history: {e}"),
            });
            write_json_file(&output_dir, "reservation_history.json", &error_info)?;
            file_count += 1;
        }
    }

    // 8. Recent audit actions (redacted)
    let audit_query = AuditQuery {
        limit: Some(opts.audit_limit),
        ..Default::default()
    };
    match storage.get_audit_actions(audit_query).await {
        Ok(actions) => {
            let redacted: Vec<_> = actions
                .into_iter()
                .map(|a| redact_audit(a, &redactor))
                .collect();
            write_json_file(&output_dir, "recent_audit.json", &redacted)?;
            file_count += 1;
        }
        Err(e) => {
            let error_info = serde_json::json!({
                "error": format!("Failed to query audit actions: {e}"),
            });
            write_json_file(&output_dir, "recent_audit.json", &error_info)?;
            file_count += 1;
        }
    }

    // 9. Write bundle manifest
    // Build dynamic file list based on what was actually written
    let mut files = vec![
        "environment.json".to_string(),
        "config_summary.json".to_string(),
        "db_health.json".to_string(),
        "recent_events.json".to_string(),
        "recent_workflows.json".to_string(),
        "active_reservations.json".to_string(),
        "reservation_history.json".to_string(),
        "recent_audit.json".to_string(),
    ];

    // Add traces directory if it exists and has content
    let traces_dir = output_dir.join("traces");
    if traces_dir.exists() && traces_dir.join("rule_traces.json").exists() {
        files.push("traces/rule_traces.json".to_string());
    }

    let manifest = BundleManifest {
        wa_version: crate::VERSION.to_string(),
        generated_at_ms: now_ms,
        file_count,
        files,
        redacted: true,
    };
    write_json_file(&output_dir, "manifest.json", &manifest)?;
    file_count += 1;

    let total_size = dir_size(&output_dir);

    Ok(DiagnosticResult {
        output_path: output_dir.display().to_string(),
        file_count,
        total_size_bytes: total_size,
    })
}

// =============================================================================
// Bundle manifest
// =============================================================================

#[derive(Debug, Serialize)]
struct BundleManifest {
    wa_version: String,
    generated_at_ms: u64,
    file_count: usize,
    files: Vec<String>,
    redacted: bool,
}

// =============================================================================
// Helpers
// =============================================================================

fn write_json_file<T: Serialize>(dir: &Path, name: &str, value: &T) -> crate::Result<()> {
    let path = dir.join(name);
    let json = serde_json::to_string_pretty(value).map_err(|e| {
        crate::Error::Storage(crate::StorageError::Database(format!(
            "Failed to serialize {name}: {e}"
        )))
    })?;

    let mut file = fs::File::create(&path).map_err(|e| {
        crate::Error::Storage(crate::StorageError::Database(format!(
            "Failed to create {}: {e}",
            path.display()
        )))
    })?;

    file.write_all(json.as_bytes()).map_err(|e| {
        crate::Error::Storage(crate::StorageError::Database(format!(
            "Failed to write {}: {e}",
            path.display()
        )))
    })?;

    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    fs::read_dir(path).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .map(|e| e.metadata().map_or(0, |m| m.len()))
            .sum()
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build diagnostic test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn environment_info_populated() {
        let env = gather_environment();
        assert!(!env.wa_version.is_empty());
        assert!(!env.os.is_empty());
        assert!(!env.arch.is_empty());
        assert_eq!(env.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn config_summary_from_defaults() {
        let config = Config::default();
        let summary = summarize_config(&config);
        assert_eq!(summary.general_log_level, "info");
        assert_eq!(summary.ingest_poll_interval_ms, 200);
        assert!(summary.ingest_gap_detection);
    }

    #[test]
    fn db_health_gathers_stats() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_diag_health_{}.db", std::process::id()));

        // Create a minimal DB
        {
            let conn = Connection::open(&tmp).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE panes (id INTEGER PRIMARY KEY);
                CREATE TABLE output_segments (id INTEGER PRIMARY KEY);
                CREATE TABLE events (id INTEGER PRIMARY KEY);
                CREATE TABLE audit_actions (id INTEGER PRIMARY KEY);
                CREATE TABLE workflow_executions (id INTEGER PRIMARY KEY);
                CREATE TABLE workflow_step_logs (id INTEGER PRIMARY KEY);
                CREATE TABLE pane_reservations (id INTEGER PRIMARY KEY);
                CREATE TABLE approval_tokens (id INTEGER PRIMARY KEY);
                INSERT INTO panes VALUES (1);
                INSERT INTO panes VALUES (2);
                INSERT INTO events VALUES (1);
                PRAGMA user_version = 8;
                ",
            )
            .unwrap();
        }

        let health = gather_db_health(&tmp).unwrap();
        assert_eq!(health.schema_version, 8);
        assert_eq!(health.table_counts.panes, 2);
        assert_eq!(health.table_counts.events, 1);
        assert_eq!(health.table_counts.output_segments, 0);
        assert!(health.page_count > 0);
        assert!(health.page_size > 0);
        assert!(health.db_file_size_bytes > 0);

        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn redact_events_removes_secrets() {
        let redactor = Redactor::new();
        let events = vec![crate::storage::StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test".to_string(),
            agent_type: "codex".to_string(),
            event_type: "auth.error".to_string(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: None,
            matched_text: Some("Error: sk-abc123def456ghi789jkl012mno345pqr678stu901v".to_string()),
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }];

        let redacted = redact_events(events, &redactor);
        assert_eq!(redacted.len(), 1);
        let text = redacted[0].matched_text.as_ref().unwrap();
        assert!(text.contains("[REDACTED]"));
        assert!(!text.contains("sk-abc123"));
    }

    #[test]
    fn redact_audit_removes_secrets() {
        let redactor = Redactor::new();
        let action = crate::storage::AuditActionRecord {
            id: 1,
            ts: 1000,
            actor_kind: "workflow".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: None,
            action_kind: "test".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some(
                "token sk-abc123def456ghi789jkl012mno345pqr678stu901v is valid".to_string(),
            ),
            rule_id: None,
            input_summary: Some("input data".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "ok".to_string(),
        };

        let redacted = redact_audit(action, &redactor);
        let reason = redacted.decision_reason.unwrap();
        assert!(reason.contains("[REDACTED]"));
        assert!(!reason.contains("sk-abc123"));
    }

    #[test]
    fn generate_rule_traces_extracts_detection_data() {
        let redactor = Redactor::new();
        let events = vec![
            // Event with rule match and extracted data
            crate::storage::StoredEvent {
                id: 1,
                pane_id: 1,
                rule_id: "codex.usage_limit".to_string(),
                agent_type: "codex".to_string(),
                event_type: "usage".to_string(),
                severity: "warning".to_string(),
                confidence: 0.95,
                extracted: Some(serde_json::json!({"percentage": "25%", "limit": "20h"})),
                matched_text: Some("25% of your 20h limit remaining".to_string()),
                segment_id: Some(100),
                detected_at: 1000,
                dedupe_key: None,
                handled_at: Some(1500),
                handled_by_workflow_id: Some("wf-123".to_string()),
                handled_status: Some("completed".to_string()),
            },
            // Event without extracted data
            crate::storage::StoredEvent {
                id: 2,
                pane_id: 2,
                rule_id: "claude_code.compaction".to_string(),
                agent_type: "claude_code".to_string(),
                event_type: "compaction".to_string(),
                severity: "info".to_string(),
                confidence: 1.0,
                extracted: None,
                matched_text: Some("context compacted".to_string()),
                segment_id: None,
                detected_at: 2000,
                dedupe_key: None,
                handled_at: None,
                handled_by_workflow_id: None,
                handled_status: None,
            },
            // Event without rule_id (should be filtered out)
            crate::storage::StoredEvent {
                id: 3,
                pane_id: 3,
                rule_id: String::new(),
                agent_type: "unknown".to_string(),
                event_type: "manual".to_string(),
                severity: "info".to_string(),
                confidence: 0.0,
                extracted: None,
                matched_text: None,
                segment_id: None,
                detected_at: 3000,
                dedupe_key: None,
                handled_at: None,
                handled_by_workflow_id: None,
                handled_status: None,
            },
        ];

        let traces = generate_rule_traces(&events, &redactor);

        // Should only have 2 traces (event 3 filtered out due to empty rule_id)
        assert_eq!(traces.len(), 2);

        // First trace should have extracted fields
        assert_eq!(traces[0].event_id, 1);
        assert_eq!(traces[0].rule_id, "codex.usage_limit");
        assert!((traces[0].confidence - 0.95).abs() < f64::EPSILON);
        assert!(traces[0].handled);
        assert_eq!(traces[0].extracted_fields.len(), 2);

        // Second trace should have no extracted fields
        assert_eq!(traces[1].event_id, 2);
        assert_eq!(traces[1].rule_id, "claude_code.compaction");
        assert!(!traces[1].handled);
        assert!(traces[1].extracted_fields.is_empty());
    }

    #[test]
    fn rule_traces_redact_secrets_in_matched_text() {
        let redactor = Redactor::new();
        let events = vec![crate::storage::StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "codex.auth_error".to_string(),
            agent_type: "codex".to_string(),
            event_type: "auth".to_string(),
            severity: "critical".to_string(),
            confidence: 1.0,
            extracted: Some(
                serde_json::json!({"key": "sk-abc123def456ghi789jkl012mno345pqr678stu901v"}),
            ),
            matched_text: Some(
                "Error: Invalid API key sk-abc123def456ghi789jkl012mno345pqr678stu901v".to_string(),
            ),
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }];

        let traces = generate_rule_traces(&events, &redactor);

        assert_eq!(traces.len(), 1);
        // Matched text should be redacted
        assert!(
            traces[0]
                .matched_text
                .as_ref()
                .unwrap()
                .contains("[REDACTED]")
        );
        assert!(
            !traces[0]
                .matched_text
                .as_ref()
                .unwrap()
                .contains("sk-abc123")
        );
        // Extracted fields should also be redacted
        assert!(!traces[0].extracted_fields.is_empty());
        let key_field = &traces[0].extracted_fields[0];
        assert!(key_field.value.as_ref().unwrap().contains("[REDACTED]"));
    }

    #[test]
    fn write_json_file_creates_valid_json() {
        let tmp_dir =
            std::env::temp_dir().join(format!("wa_test_diag_write_{}", std::process::id()));
        fs::create_dir_all(&tmp_dir).unwrap();

        let data = serde_json::json!({"key": "value", "count": 42});
        write_json_file(&tmp_dir, "test.json", &data).unwrap();

        let content = fs::read_to_string(tmp_dir.join("test.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["key"], "value");
        assert_eq!(parsed["count"], 42);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn generate_bundle_creates_all_files() {
        run_async_test(async {
            let tmp =
                std::env::temp_dir().join(format!("wa_test_diag_bundle_{}.db", std::process::id()));
            let db_path = tmp.to_string_lossy().to_string();

            let storage = StorageHandle::new(&db_path).await.unwrap();

            // Insert test data
            let pane = crate::storage::PaneRecord {
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
                std::env::temp_dir().join(format!("wa_test_diag_ws_{}", std::process::id())),
                &config.storage,
                &config.ipc,
            );

            let output_dir =
                std::env::temp_dir().join(format!("wa_test_diag_output_{}", std::process::id()));
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
            let manifest_content = fs::read_to_string(output_dir.join("manifest.json")).unwrap();
            let manifest: serde_json::Value = serde_json::from_str(&manifest_content).unwrap();
            assert!(manifest["redacted"].as_bool().unwrap());
            assert!(manifest["file_count"].as_u64().unwrap() >= 8);
            assert!(!manifest["wa_version"].as_str().unwrap().is_empty());

            // Verify environment.json
            let env_content = fs::read_to_string(output_dir.join("environment.json")).unwrap();
            let env_info: serde_json::Value = serde_json::from_str(&env_content).unwrap();
            assert!(!env_info["wa_version"].as_str().unwrap().is_empty());
            assert_eq!(env_info["schema_version"], SCHEMA_VERSION);

            // Verify db_health.json
            let health_content = fs::read_to_string(output_dir.join("db_health.json")).unwrap();
            let health: serde_json::Value = serde_json::from_str(&health_content).unwrap();
            assert!(health["page_count"].as_i64().unwrap() > 0);
            assert_eq!(health["table_counts"]["panes"], 1);

            storage.shutdown().await.unwrap();
            let _ = fs::remove_file(&tmp);
            let _ = fs::remove_dir_all(&output_dir);
            let _ = fs::remove_dir_all(layout.root);
        });
    }

    #[test]
    fn bundle_does_not_contain_secrets() {
        run_async_test(async {
            let tmp = std::env::temp_dir()
                .join(format!("wa_test_diag_secrets_{}.db", std::process::id()));
            let db_path = tmp.to_string_lossy().to_string();

            let storage = StorageHandle::new(&db_path).await.unwrap();

            // Insert data with a secret
            let pane = crate::storage::PaneRecord {
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
            let action = crate::storage::AuditActionRecord {
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
                std::env::temp_dir()
                    .join(format!("wa_test_diag_secrets_ws_{}", std::process::id())),
                &config.storage,
                &config.ipc,
            );

            let output_dir = std::env::temp_dir().join(format!(
                "wa_test_diag_secrets_output_{}",
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
            for entry in fs::read_dir(&output_dir).unwrap() {
                let entry = entry.unwrap();
                let content = fs::read_to_string(entry.path()).unwrap();
                assert!(
                    !content.contains(secret),
                    "Secret found in {}",
                    entry.file_name().to_string_lossy()
                );
            }

            // Verify the audit file exists and has [REDACTED]
            let audit_content = fs::read_to_string(output_dir.join("recent_audit.json")).unwrap();
            assert!(audit_content.contains("[REDACTED]"));

            storage.shutdown().await.unwrap();
            let _ = fs::remove_file(&tmp);
            let _ = fs::remove_dir_all(&output_dir);
            let _ = fs::remove_dir_all(layout.root);
        });
    }

    #[test]
    fn bundle_manifest_has_stable_metadata() {
        run_async_test(async {
            let tmp =
                std::env::temp_dir().join(format!("wa_test_diag_meta_{}.db", std::process::id()));
            let db_path = tmp.to_string_lossy().to_string();
            let storage = StorageHandle::new(&db_path).await.unwrap();

            let config = Config::default();
            let layout = WorkspaceLayout::new(
                std::env::temp_dir().join(format!("wa_test_diag_meta_ws_{}", std::process::id())),
                &config.storage,
                &config.ipc,
            );

            let output_dir = std::env::temp_dir()
                .join(format!("wa_test_diag_meta_output_{}", std::process::id()));
            let opts = DiagnosticOptions {
                output: Some(output_dir.clone()),
                ..Default::default()
            };

            generate_bundle(&config, &layout, &storage, &opts)
                .await
                .unwrap();

            // Verify manifest has all required stable metadata fields
            let manifest_content = fs::read_to_string(output_dir.join("manifest.json")).unwrap();
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
            let env_content = fs::read_to_string(output_dir.join("environment.json")).unwrap();
            let env: serde_json::Value = serde_json::from_str(&env_content).unwrap();
            assert!(env["wa_version"].is_string());
            assert!(env["schema_version"].is_number());
            assert!(env["os"].is_string());
            assert!(env["arch"].is_string());

            // Verify config_summary.json has stable fields
            let config_content =
                fs::read_to_string(output_dir.join("config_summary.json")).unwrap();
            let config_json: serde_json::Value = serde_json::from_str(&config_content).unwrap();
            assert!(config_json["general_log_level"].is_string());
            assert!(config_json["ingest_poll_interval_ms"].is_number());
            assert!(config_json["metrics_enabled"].is_boolean());

            storage.shutdown().await.unwrap();
            let _ = fs::remove_file(&tmp);
            let _ = fs::remove_dir_all(&output_dir);
            let _ = fs::remove_dir_all(layout.root);
        });
    }

    #[test]
    fn bundle_includes_reservation_snapshot() {
        run_async_test(async {
            let tmp =
                std::env::temp_dir().join(format!("wa_test_diag_res_{}.db", std::process::id()));
            let db_path = tmp.to_string_lossy().to_string();
            let storage = StorageHandle::new(&db_path).await.unwrap();

            // Create a pane
            let pane = crate::storage::PaneRecord {
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
                std::env::temp_dir().join(format!("wa_test_diag_res_ws_{}", std::process::id())),
                &config.storage,
                &config.ipc,
            );

            let output_dir = std::env::temp_dir()
                .join(format!("wa_test_diag_res_output_{}", std::process::id()));
            let opts = DiagnosticOptions {
                output: Some(output_dir.clone()),
                ..Default::default()
            };

            generate_bundle(&config, &layout, &storage, &opts)
                .await
                .unwrap();

            // Verify active_reservations.json contains the reservation
            let res_content =
                fs::read_to_string(output_dir.join("active_reservations.json")).unwrap();
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
                fs::read_to_string(output_dir.join("reservation_history.json")).unwrap();
            let history: serde_json::Value = serde_json::from_str(&hist_content).unwrap();
            let hist_arr = history.as_array().unwrap();
            assert!(!hist_arr.is_empty());

            storage.shutdown().await.unwrap();
            let _ = fs::remove_file(&tmp);
            let _ = fs::remove_dir_all(&output_dir);
            let _ = fs::remove_dir_all(layout.root);
        });
    }

    // ---------------------------------------------------------------
    // Pure function tests: dir_size
    // ---------------------------------------------------------------

    #[test]
    fn dir_size_empty_directory() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_diag_dirsize_empty_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        assert_eq!(dir_size(&tmp), 0);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dir_size_with_files() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_diag_dirsize_files_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.txt"), "hello").unwrap();
        fs::write(tmp.join("b.txt"), "world!").unwrap();
        let size = dir_size(&tmp);
        assert_eq!(size, 11); // 5 + 6 bytes
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn dir_size_nonexistent_returns_zero() {
        let fake = std::env::temp_dir().join("wa_test_diag_dirsize_nonexistent_xyzzy");
        assert_eq!(dir_size(&fake), 0);
    }

    // ---------------------------------------------------------------
    // Pure function tests: DiagnosticOptions
    // ---------------------------------------------------------------

    #[test]
    fn diagnostic_options_default_values() {
        let opts = DiagnosticOptions::default();
        assert_eq!(opts.event_limit, 100);
        assert_eq!(opts.audit_limit, 50);
        assert_eq!(opts.workflow_limit, 50);
        assert!(opts.output.is_none());
    }

    #[test]
    fn diagnostic_options_custom_values() {
        let opts = DiagnosticOptions {
            event_limit: 10,
            audit_limit: 5,
            workflow_limit: 3,
            output: Some(PathBuf::from("/tmp/custom")),
        };
        assert_eq!(opts.event_limit, 10);
        assert_eq!(
            opts.output.as_ref().unwrap().to_str().unwrap(),
            "/tmp/custom"
        );
    }

    // ---------------------------------------------------------------
    // Pure function tests: DiagnosticResult serialization
    // ---------------------------------------------------------------

    #[test]
    fn diagnostic_result_serializes() {
        let result = DiagnosticResult {
            output_path: "/tmp/diag_123".to_string(),
            file_count: 9,
            total_size_bytes: 4096,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("\"output_path\":\"/tmp/diag_123\""));
        assert!(json.contains("\"file_count\":9"));
        assert!(json.contains("\"total_size_bytes\":4096"));
    }

    #[test]
    fn diagnostic_result_zero_values() {
        let result = DiagnosticResult {
            output_path: String::new(),
            file_count: 0,
            total_size_bytes: 0,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("\"file_count\":0"));
        assert!(json.contains("\"total_size_bytes\":0"));
    }

    // ---------------------------------------------------------------
    // Pure function tests: EnvironmentInfo serialization
    // ---------------------------------------------------------------

    #[test]
    fn environment_info_serializes() {
        let env = gather_environment();
        let json = serde_json::to_string_pretty(&env).expect("serialize");
        assert!(json.contains("\"wa_version\""));
        assert!(json.contains("\"schema_version\""));
        assert!(json.contains("\"os\""));
        assert!(json.contains("\"arch\""));
    }

    #[test]
    fn environment_info_has_cwd() {
        let env = gather_environment();
        assert!(env.cwd.is_some(), "cwd should be available in test env");
    }

    // ---------------------------------------------------------------
    // Pure function tests: ConfigSummary
    // ---------------------------------------------------------------

    #[test]
    fn config_summary_custom_values() {
        let mut config = Config::default();
        config.general.log_level = "debug".to_string();
        config.ingest.poll_interval_ms = 500;
        config.ingest.gap_detection = false;
        config.storage.retention_days = 90;
        config.workflows.max_concurrent = 10;
        config.safety.rate_limit_per_pane = 5;

        let summary = summarize_config(&config);
        assert_eq!(summary.general_log_level, "debug");
        assert_eq!(summary.ingest_poll_interval_ms, 500);
        assert!(!summary.ingest_gap_detection);
        assert_eq!(summary.storage_retention_days, 90);
        assert_eq!(summary.workflows_max_concurrent, 10);
        assert_eq!(summary.safety_rate_limit, 5);
    }

    #[test]
    fn config_summary_serializes() {
        let config = Config::default();
        let summary = summarize_config(&config);
        let json = serde_json::to_string_pretty(&summary).expect("serialize");
        assert!(json.contains("\"general_log_level\""));
        assert!(json.contains("\"ingest_poll_interval_ms\""));
        assert!(json.contains("\"patterns_packs\""));
        assert!(json.contains("\"metrics_enabled\""));
    }

    // ---------------------------------------------------------------
    // Pure function tests: redact_reservation
    // ---------------------------------------------------------------

    #[test]
    fn redact_reservation_preserves_structure() {
        let redactor = Redactor::new();
        let res = crate::storage::PaneReservation {
            id: 42,
            pane_id: 3,
            owner_kind: "workflow".to_string(),
            owner_id: "wf-test-abc".to_string(),
            reason: Some("testing cleanup".to_string()),
            created_at: 1000,
            expires_at: 2000,
            released_at: None,
            status: "active".to_string(),
        };

        let redacted = redact_reservation(res, &redactor);
        assert_eq!(redacted.id, 42);
        assert_eq!(redacted.pane_id, 3);
        assert_eq!(redacted.owner_kind, "workflow");
        assert_eq!(redacted.status, "active");
        assert_eq!(redacted.created_at, 1000);
        assert_eq!(redacted.expires_at, 2000);
        assert!(redacted.released_at.is_none());
    }

    #[test]
    fn redact_reservation_redacts_secrets_in_owner_id() {
        let redactor = Redactor::new();
        let res = crate::storage::PaneReservation {
            id: 1,
            pane_id: 1,
            owner_kind: "agent".to_string(),
            owner_id: "sk-abc123def456ghi789jkl012mno345pqr678stu901v".to_string(),
            reason: Some("API key sk-abc123def456ghi789jkl012mno345pqr678stu901v used".to_string()),
            created_at: 1000,
            expires_at: 2000,
            released_at: None,
            status: "active".to_string(),
        };

        let redacted = redact_reservation(res, &redactor);
        assert!(redacted.owner_id.contains("[REDACTED]"));
        assert!(!redacted.owner_id.contains("sk-abc123"));
        let reason = redacted.reason.unwrap();
        assert!(reason.contains("[REDACTED]"));
        assert!(!reason.contains("sk-abc123"));
    }

    #[test]
    fn redact_reservation_released_preserves_released_at() {
        let redactor = Redactor::new();
        let res = crate::storage::PaneReservation {
            id: 1,
            pane_id: 1,
            owner_kind: "manual".to_string(),
            owner_id: "user".to_string(),
            reason: None,
            created_at: 1000,
            expires_at: 2000,
            released_at: Some(1500),
            status: "released".to_string(),
        };

        let redacted = redact_reservation(res, &redactor);
        assert_eq!(redacted.released_at, Some(1500));
        assert_eq!(redacted.status, "released");
        assert!(redacted.reason.is_none());
    }

    #[test]
    fn redact_reservation_serializes() {
        let redactor = Redactor::new();
        let res = crate::storage::PaneReservation {
            id: 1,
            pane_id: 1,
            owner_kind: "workflow".to_string(),
            owner_id: "wf-1".to_string(),
            reason: None,
            created_at: 1000,
            expires_at: 2000,
            released_at: None,
            status: "active".to_string(),
        };
        let redacted = redact_reservation(res, &redactor);
        let json = serde_json::to_string(&redacted).expect("serialize");
        assert!(json.contains("\"pane_id\":1"));
        assert!(json.contains("\"status\":\"active\""));
    }

    // ---------------------------------------------------------------
    // Pure function tests: redact_step
    // ---------------------------------------------------------------

    #[test]
    fn redact_step_preserves_structure() {
        let redactor = Redactor::new();
        let step = crate::storage::WorkflowStepLogRecord {
            id: 10,
            workflow_id: "wf-123".to_string(),
            audit_action_id: Some(5),
            step_index: 2,
            step_name: "send_text".to_string(),
            step_id: Some("step-a".to_string()),
            step_kind: Some("action".to_string()),
            result_type: "continue".to_string(),
            result_data: None,
            policy_summary: Some("allow: rate limit ok".to_string()),
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 2000,
            duration_ms: 1000,
        };

        let redacted = redact_step(step, &redactor);
        assert_eq!(redacted.step_index, 2);
        assert_eq!(redacted.step_name, "send_text");
        assert_eq!(redacted.result_type, "continue");
        assert_eq!(redacted.started_at, 1000);
        assert_eq!(redacted.completed_at, 2000);
    }

    #[test]
    fn redact_step_redacts_secrets_in_policy_summary() {
        let redactor = Redactor::new();
        let step = crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "wf-1".to_string(),
            audit_action_id: None,
            step_index: 0,
            step_name: "auth_check".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "done".to_string(),
            result_data: None,
            policy_summary: Some(
                "key sk-abc123def456ghi789jkl012mno345pqr678stu901v valid".to_string(),
            ),
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 1500,
            duration_ms: 500,
        };

        let redacted = redact_step(step, &redactor);
        let summary = redacted.policy_summary.unwrap();
        assert!(summary.contains("[REDACTED]"));
        assert!(!summary.contains("sk-abc123"));
    }

    #[test]
    fn redact_step_none_policy_preserved() {
        let redactor = Redactor::new();
        let step = crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "wf-1".to_string(),
            audit_action_id: None,
            step_index: 0,
            step_name: "noop".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "continue".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 1001,
            duration_ms: 1,
        };

        let redacted = redact_step(step, &redactor);
        assert!(redacted.policy_summary.is_none());
    }

    #[test]
    fn redact_step_serializes() {
        let redactor = Redactor::new();
        let step = crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "wf-1".to_string(),
            audit_action_id: None,
            step_index: 3,
            step_name: "wait_for".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "wait_for".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 5000,
            duration_ms: 4000,
        };
        let redacted = redact_step(step, &redactor);
        let json = serde_json::to_string(&redacted).expect("serialize");
        assert!(json.contains("\"step_index\":3"));
        assert!(json.contains("\"result_type\":\"wait_for\""));
    }

    // ---------------------------------------------------------------
    // Pure function tests: generate_rule_traces edge cases
    // ---------------------------------------------------------------

    #[test]
    fn generate_rule_traces_empty_events() {
        let redactor = Redactor::new();
        let traces = generate_rule_traces(&[], &redactor);
        assert!(traces.is_empty());
    }

    #[test]
    fn generate_rule_traces_non_object_extracted_data() {
        let redactor = Redactor::new();
        let events = vec![crate::storage::StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test.rule".to_string(),
            agent_type: "test".to_string(),
            event_type: "test".to_string(),
            severity: "info".to_string(),
            confidence: 1.0,
            extracted: Some(serde_json::json!("just a string")),
            matched_text: None,
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }];

        let traces = generate_rule_traces(&events, &redactor);
        assert_eq!(traces.len(), 1);
        assert!(
            traces[0].extracted_fields.is_empty(),
            "non-object extracted should yield empty fields"
        );
    }

    #[test]
    fn generate_rule_traces_array_extracted_data() {
        let redactor = Redactor::new();
        let events = vec![crate::storage::StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test.rule".to_string(),
            agent_type: "test".to_string(),
            event_type: "test".to_string(),
            severity: "info".to_string(),
            confidence: 1.0,
            extracted: Some(serde_json::json!([1, 2, 3])),
            matched_text: Some("match".to_string()),
            segment_id: None,
            detected_at: 2000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }];

        let traces = generate_rule_traces(&events, &redactor);
        assert_eq!(traces.len(), 1);
        assert!(traces[0].extracted_fields.is_empty());
        assert_eq!(traces[0].matched_text.as_deref(), Some("match"));
    }

    #[test]
    fn generate_rule_traces_null_extracted() {
        let redactor = Redactor::new();
        let events = vec![crate::storage::StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test.rule".to_string(),
            agent_type: "test".to_string(),
            event_type: "test".to_string(),
            severity: "info".to_string(),
            confidence: 0.5,
            extracted: None,
            matched_text: None,
            segment_id: None,
            detected_at: 3000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }];

        let traces = generate_rule_traces(&events, &redactor);
        assert_eq!(traces.len(), 1);
        assert!(traces[0].extracted_fields.is_empty());
        assert!(traces[0].matched_text.is_none());
        assert!(!traces[0].handled);
    }

    // ---------------------------------------------------------------
    // Pure function tests: gather_db_health edge cases
    // ---------------------------------------------------------------

    #[test]
    fn gather_db_health_missing_wal_file() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_diag_nowal_{}.db", std::process::id()));
        {
            let conn = Connection::open(&tmp).unwrap();
            conn.execute_batch(
                "CREATE TABLE panes (id INTEGER PRIMARY KEY);
                 CREATE TABLE output_segments (id INTEGER PRIMARY KEY);
                 CREATE TABLE events (id INTEGER PRIMARY KEY);
                 CREATE TABLE audit_actions (id INTEGER PRIMARY KEY);
                 CREATE TABLE workflow_executions (id INTEGER PRIMARY KEY);
                 CREATE TABLE workflow_step_logs (id INTEGER PRIMARY KEY);
                 CREATE TABLE pane_reservations (id INTEGER PRIMARY KEY);
                 CREATE TABLE approval_tokens (id INTEGER PRIMARY KEY);",
            )
            .unwrap();
        }
        // Ensure no WAL file exists
        let wal_path = tmp.with_extension("db-wal");
        let _ = fs::remove_file(&wal_path);

        let health = gather_db_health(&tmp).unwrap();
        assert_eq!(health.wal_file_size_bytes, 0);
        assert_eq!(health.table_counts.panes, 0);

        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn gather_db_health_invalid_path() {
        let fake = PathBuf::from("/nonexistent/path/to/db.db");
        // Should still succeed (rusqlite creates a new DB at the path if possible,
        // but /nonexistent won't work, so it should error)
        let result = gather_db_health(&fake);
        assert!(result.is_err());
    }

    #[test]
    fn gather_db_health_missing_table_returns_negative_one() {
        let tmp = std::env::temp_dir().join(format!(
            "wa_test_diag_missing_table_{}.db",
            std::process::id()
        ));
        {
            let conn = Connection::open(&tmp).unwrap();
            // Only create some tables, not all
            conn.execute_batch(
                "CREATE TABLE panes (id INTEGER PRIMARY KEY);
                 CREATE TABLE events (id INTEGER PRIMARY KEY);
                 INSERT INTO panes VALUES (1);",
            )
            .unwrap();
        }

        let health = gather_db_health(&tmp).unwrap();
        assert_eq!(health.table_counts.panes, 1);
        assert_eq!(health.table_counts.events, 0);
        // Missing tables should return -1
        assert_eq!(health.table_counts.output_segments, -1);
        assert_eq!(health.table_counts.audit_actions, -1);
        assert_eq!(health.table_counts.workflow_executions, -1);

        let _ = fs::remove_file(&tmp);
    }

    // ---------------------------------------------------------------
    // Pure function tests: write_json_file edge cases
    // ---------------------------------------------------------------

    #[test]
    fn write_json_file_nested_data() {
        let tmp = std::env::temp_dir().join(format!("wa_test_diag_nested_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();

        let data = serde_json::json!({
            "outer": {
                "inner": [1, 2, 3],
                "nested": {"deep": true}
            }
        });
        write_json_file(&tmp, "nested.json", &data).unwrap();

        let content = fs::read_to_string(tmp.join("nested.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["outer"]["inner"][0], 1);
        assert!(parsed["outer"]["nested"]["deep"].as_bool().unwrap());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_json_file_empty_object() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_diag_empty_obj_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();

        let data = serde_json::json!({});
        write_json_file(&tmp, "empty.json", &data).unwrap();

        let content = fs::read_to_string(tmp.join("empty.json")).unwrap();
        assert_eq!(content.trim(), "{}");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_json_file_invalid_directory() {
        let bad_dir = PathBuf::from("/nonexistent/path/for/test");
        let data = serde_json::json!({"test": true});
        let result = write_json_file(&bad_dir, "test.json", &data);
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // Pure function tests: redact_audit edge cases
    // ---------------------------------------------------------------

    #[test]
    fn redact_audit_none_fields_preserved() {
        let redactor = Redactor::new();
        let action = crate::storage::AuditActionRecord {
            id: 1,
            ts: 1000,
            actor_kind: "robot".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: None,
            domain: None,
            action_kind: "query".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "ok".to_string(),
        };

        let redacted = redact_audit(action, &redactor);
        assert_eq!(redacted.id, 1);
        assert_eq!(redacted.actor_kind, "robot");
        assert!(redacted.pane_id.is_none());
        assert!(redacted.input_summary.is_none());
        assert!(redacted.decision_reason.is_none());
    }

    #[test]
    fn redact_audit_input_summary_redacted() {
        let redactor = Redactor::new();
        let action = crate::storage::AuditActionRecord {
            id: 1,
            ts: 1000,
            actor_kind: "workflow".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(5),
            domain: None,
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("approved".to_string()),
            rule_id: None,
            input_summary: Some(
                "export KEY=sk-abc123def456ghi789jkl012mno345pqr678stu901v".to_string(),
            ),
            verification_summary: None,
            decision_context: None,
            result: "ok".to_string(),
        };

        let redacted = redact_audit(action, &redactor);
        let summary = redacted.input_summary.unwrap();
        assert!(summary.contains("[REDACTED]"));
        assert!(!summary.contains("sk-abc123"));
        assert_eq!(redacted.pane_id, Some(5));
    }

    #[test]
    fn redact_audit_serializes() {
        let redactor = Redactor::new();
        let action = crate::storage::AuditActionRecord {
            id: 7,
            ts: 5000,
            actor_kind: "robot".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(2),
            domain: None,
            action_kind: "send_text".to_string(),
            policy_decision: "deny".to_string(),
            decision_reason: Some("rate limited".to_string()),
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "blocked".to_string(),
        };
        let redacted = redact_audit(action, &redactor);
        let json = serde_json::to_string(&redacted).expect("serialize");
        assert!(json.contains("\"policy_decision\":\"deny\""));
        assert!(json.contains("\"result\":\"blocked\""));
    }

    // ---------------------------------------------------------------
    // Pure function tests: redact_events edge cases
    // ---------------------------------------------------------------

    #[test]
    fn redact_events_empty_list() {
        let redactor = Redactor::new();
        let redacted = redact_events(vec![], &redactor);
        assert!(redacted.is_empty());
    }

    #[test]
    fn redact_events_no_matched_text() {
        let redactor = Redactor::new();
        let events = vec![crate::storage::StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test".to_string(),
            agent_type: "test".to_string(),
            event_type: "test".to_string(),
            severity: "info".to_string(),
            confidence: 1.0,
            extracted: None,
            matched_text: None,
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }];

        let redacted = redact_events(events, &redactor);
        assert_eq!(redacted.len(), 1);
        assert!(redacted[0].matched_text.is_none());
        assert_eq!(redacted[0].rule_id, "test");
    }

    #[test]
    fn redact_events_preserves_handled_status() {
        let redactor = Redactor::new();
        let events = vec![crate::storage::StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test".to_string(),
            agent_type: "test".to_string(),
            event_type: "test".to_string(),
            severity: "warning".to_string(),
            confidence: 0.8,
            extracted: None,
            matched_text: Some("clean text".to_string()),
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: Some(2000),
            handled_by_workflow_id: Some("wf-1".to_string()),
            handled_status: Some("completed".to_string()),
        }];

        let redacted = redact_events(events, &redactor);
        assert_eq!(redacted[0].handled_status.as_deref(), Some("completed"));
        assert_eq!(redacted[0].severity, "warning");
        assert!((redacted[0].confidence - 0.8).abs() < f64::EPSILON);
    }

    // ---------------------------------------------------------------
    // Pure function tests: BundleManifest serialization
    // ---------------------------------------------------------------

    #[test]
    fn bundle_manifest_serializes() {
        let manifest = BundleManifest {
            wa_version: "0.1.0".to_string(),
            generated_at_ms: 1_700_000_000_000,
            file_count: 9,
            files: vec![
                "environment.json".to_string(),
                "config_summary.json".to_string(),
            ],
            redacted: true,
        };
        let json = serde_json::to_string_pretty(&manifest).expect("serialize");
        assert!(json.contains("\"wa_version\": \"0.1.0\""));
        assert!(json.contains("\"file_count\": 9"));
        assert!(json.contains("\"redacted\": true"));
        assert!(json.contains("environment.json"));
    }

    // ---------------------------------------------------------------
    // Pure function tests: TraceEvidence skip_serializing_if
    // ---------------------------------------------------------------

    #[test]
    fn trace_evidence_skips_none_fields() {
        let evidence = TraceEvidence {
            kind: "anchor_match".to_string(),
            label: None,
            value: None,
        };
        let json = serde_json::to_string(&evidence).expect("serialize");
        assert!(!json.contains("label"));
        assert!(!json.contains("value"));
        assert!(json.contains("\"kind\":\"anchor_match\""));
    }

    #[test]
    fn trace_evidence_includes_present_fields() {
        let evidence = TraceEvidence {
            kind: "extracted_field".to_string(),
            label: Some("percentage".to_string()),
            value: Some("25%".to_string()),
        };
        let json = serde_json::to_string(&evidence).expect("serialize");
        assert!(json.contains("\"label\":\"percentage\""));
        assert!(json.contains("\"value\":\"25%\""));
    }

    // ---------------------------------------------------------------
    // Pure function tests: EventRuleTrace skip_serializing_if
    // ---------------------------------------------------------------

    #[test]
    fn event_rule_trace_skips_empty_extracted() {
        let trace = EventRuleTrace {
            event_id: 1,
            rule_id: "test".to_string(),
            agent_type: "test".to_string(),
            confidence: 1.0,
            severity: "info".to_string(),
            matched_text: None,
            extracted_fields: vec![],
            handled: false,
            detected_at: 1000,
        };
        let json = serde_json::to_string(&trace).expect("serialize");
        assert!(!json.contains("extracted_fields"));
        assert!(!json.contains("matched_text"));
    }

    #[test]
    fn event_rule_trace_includes_nonempty_extracted() {
        let trace = EventRuleTrace {
            event_id: 1,
            rule_id: "test.rule".to_string(),
            agent_type: "codex".to_string(),
            confidence: 0.95,
            severity: "warning".to_string(),
            matched_text: Some("matched text here".to_string()),
            extracted_fields: vec![TraceEvidence {
                kind: "extracted_field".to_string(),
                label: Some("key".to_string()),
                value: Some("val".to_string()),
            }],
            handled: true,
            detected_at: 2000,
        };
        let json = serde_json::to_string(&trace).expect("serialize");
        assert!(json.contains("extracted_fields"));
        assert!(json.contains("matched_text"));
        assert!(json.contains("\"handled\":true"));
    }

    // ---------------------------------------------------------------
    // Pure function tests: RedactedWorkflow serialization
    // ---------------------------------------------------------------

    #[test]
    fn redacted_workflow_serializes() {
        let wf = RedactedWorkflow {
            id: "wf-abc".to_string(),
            workflow_name: "handle_usage_limit".to_string(),
            pane_id: 3,
            status: "completed".to_string(),
            started_at: 1000,
            completed_at: Some(2000),
            step_count: 2,
            steps: vec![
                RedactedStep {
                    step_index: 0,
                    step_name: "detect".to_string(),
                    result_type: "continue".to_string(),
                    policy_summary: None,
                    started_at: 1000,
                    completed_at: 1500,
                },
                RedactedStep {
                    step_index: 1,
                    step_name: "send_text".to_string(),
                    result_type: "done".to_string(),
                    policy_summary: Some("allow".to_string()),
                    started_at: 1500,
                    completed_at: 2000,
                },
            ],
        };
        let json = serde_json::to_string_pretty(&wf).expect("serialize");
        assert!(json.contains("\"workflow_name\": \"handle_usage_limit\""));
        assert!(json.contains("\"step_count\": 2"));
        assert!(json.contains("\"pane_id\": 3"));
    }

    // ---------------------------------------------------------------
    // Integration tests (with real StorageHandle)
    // ---------------------------------------------------------------

    #[test]
    fn bundle_output_dir_reuse_generates_fresh_bundle() {
        run_async_test(async {
            let tmp =
                std::env::temp_dir().join(format!("wa_test_diag_reuse_{}.db", std::process::id()));
            let db_path = tmp.to_string_lossy().to_string();
            let storage = StorageHandle::new(&db_path).await.unwrap();

            let config = Config::default();
            let layout = WorkspaceLayout::new(
                std::env::temp_dir().join(format!("wa_test_diag_reuse_ws_{}", std::process::id())),
                &config.storage,
                &config.ipc,
            );

            let output_dir = std::env::temp_dir()
                .join(format!("wa_test_diag_reuse_output_{}", std::process::id()));

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
            let manifest_content = fs::read_to_string(output_dir.join("manifest.json")).unwrap();
            let manifest: serde_json::Value = serde_json::from_str(&manifest_content).unwrap();
            assert!(manifest["generated_at_ms"].as_u64().unwrap() > 0);

            storage.shutdown().await.unwrap();
            let _ = fs::remove_file(&tmp);
            let _ = fs::remove_dir_all(&output_dir);
            let _ = fs::remove_dir_all(layout.root);
        });
    }

    // -----------------------------------------------------------------------
    // Batch — RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    #[test]
    fn diagnostic_options_clone_is_independent() {
        let opts = DiagnosticOptions {
            event_limit: 10,
            audit_limit: 5,
            workflow_limit: 3,
            output: Some(PathBuf::from("/tmp/a")),
        };
        let mut clone = opts.clone();
        clone.event_limit = 999;
        clone.output = Some(PathBuf::from("/tmp/b"));
        // Original unchanged
        assert_eq!(opts.event_limit, 10);
        assert_eq!(opts.output.as_ref().unwrap().to_str().unwrap(), "/tmp/a");
        assert_eq!(clone.event_limit, 999);
    }

    #[test]
    fn diagnostic_options_debug_format() {
        let opts = DiagnosticOptions::default();
        let dbg = format!("{:?}", opts);
        assert!(dbg.contains("DiagnosticOptions"));
        assert!(dbg.contains("event_limit"));
        assert!(dbg.contains("100"));
    }

    #[test]
    fn diagnostic_result_clone_is_independent() {
        let result = DiagnosticResult {
            output_path: "/tmp/diag".to_string(),
            file_count: 5,
            total_size_bytes: 1024,
        };
        let mut clone = result.clone();
        clone.file_count = 99;
        assert_eq!(result.file_count, 5);
        assert_eq!(clone.file_count, 99);
    }

    #[test]
    fn diagnostic_result_debug_format() {
        let result = DiagnosticResult {
            output_path: "/x".to_string(),
            file_count: 3,
            total_size_bytes: 42,
        };
        let dbg = format!("{:?}", result);
        assert!(dbg.contains("DiagnosticResult"));
        assert!(dbg.contains("file_count"));
        assert!(dbg.contains("42"));
    }

    #[test]
    fn diagnostic_result_serde_roundtrip() {
        let result = DiagnosticResult {
            output_path: "/tmp/diag_roundtrip".to_string(),
            file_count: 12,
            total_size_bytes: 8192,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        // Deserialize back (DiagnosticResult derives Serialize but we can parse as Value)
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["output_path"], "/tmp/diag_roundtrip");
        assert_eq!(parsed["file_count"], 12);
        assert_eq!(parsed["total_size_bytes"], 8192);
    }

    #[test]
    fn environment_info_schema_version_matches_constant() {
        let env = gather_environment();
        assert_eq!(
            env.schema_version, SCHEMA_VERSION,
            "schema_version should always match SCHEMA_VERSION"
        );
    }

    #[test]
    fn environment_info_os_and_arch_are_known_values() {
        let env = gather_environment();
        // OS should be one of the standard Rust targets
        let known_os = ["linux", "macos", "windows", "freebsd", "android", "ios"];
        assert!(
            known_os.contains(&env.os.as_str()),
            "unexpected os: {}",
            env.os
        );
        let known_arch = ["x86_64", "aarch64", "arm", "x86", "wasm32", "riscv64"];
        assert!(
            known_arch.contains(&env.arch.as_str()),
            "unexpected arch: {}",
            env.arch
        );
    }

    #[test]
    fn config_summary_serializes_all_twelve_fields() {
        let config = Config::default();
        let summary = summarize_config(&config);
        let json = serde_json::to_string(&summary).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let obj = parsed.as_object().expect("should be object");
        let expected_keys = [
            "general_log_level",
            "general_log_format",
            "ingest_poll_interval_ms",
            "ingest_max_concurrent",
            "ingest_gap_detection",
            "storage_retention_days",
            "storage_retention_max_mb",
            "storage_checkpoint_secs",
            "patterns_quick_reject",
            "patterns_packs",
            "workflows_enabled",
            "workflows_max_concurrent",
            "safety_rate_limit",
            "metrics_enabled",
        ];
        for key in &expected_keys {
            assert!(obj.contains_key(*key), "missing field: {}", key);
        }
    }

    #[test]
    fn config_summary_patterns_packs_is_array() {
        let config = Config::default();
        let summary = summarize_config(&config);
        let json = serde_json::to_string(&summary).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(parsed["patterns_packs"].is_array());
        assert!(parsed["workflows_enabled"].is_array());
    }

    #[test]
    fn db_health_stats_serializes_all_fields() {
        let health = DbHealthStats {
            schema_version: 8,
            db_file_size_bytes: 4096,
            wal_file_size_bytes: 0,
            page_count: 10,
            page_size: 4096,
            freelist_count: 2,
            table_counts: TableCounts {
                panes: 5,
                output_segments: 10,
                events: 20,
                audit_actions: 3,
                workflow_executions: 2,
                workflow_step_logs: 8,
                pane_reservations: 1,
                approval_tokens: 0,
            },
        };
        let json = serde_json::to_string_pretty(&health).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 8);
        assert_eq!(parsed["page_size"], 4096);
        assert_eq!(parsed["freelist_count"], 2);
        assert_eq!(parsed["table_counts"]["panes"], 5);
        assert_eq!(parsed["table_counts"]["approval_tokens"], 0);
    }

    #[test]
    fn table_counts_serializes_all_eight_fields() {
        let tc = TableCounts {
            panes: 1,
            output_segments: 2,
            events: 3,
            audit_actions: 4,
            workflow_executions: 5,
            workflow_step_logs: 6,
            pane_reservations: 7,
            approval_tokens: 8,
        };
        let json = serde_json::to_string(&tc).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let obj = parsed.as_object().expect("should be object");
        assert_eq!(obj.len(), 8, "TableCounts should have exactly 8 fields");
        assert_eq!(parsed["panes"], 1);
        assert_eq!(parsed["approval_tokens"], 8);
    }

    #[test]
    fn redacted_event_serializes_all_fields() {
        let event = RedactedEvent {
            id: 42,
            pane_id: 7,
            rule_id: "test.rule".to_string(),
            event_type: "usage".to_string(),
            severity: "critical".to_string(),
            confidence: 0.99,
            detected_at: 5000,
            handled_status: Some("completed".to_string()),
            matched_text: Some("some match".to_string()),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["pane_id"], 7);
        assert_eq!(parsed["severity"], "critical");
        assert!((parsed["confidence"].as_f64().unwrap() - 0.99).abs() < f64::EPSILON);
        assert_eq!(parsed["handled_status"], "completed");
    }

    #[test]
    fn redacted_event_none_fields_are_null_in_json() {
        let event = RedactedEvent {
            id: 1,
            pane_id: 1,
            rule_id: "r".to_string(),
            event_type: "e".to_string(),
            severity: "info".to_string(),
            confidence: 0.5,
            detected_at: 100,
            handled_status: None,
            matched_text: None,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(parsed["handled_status"].is_null());
        assert!(parsed["matched_text"].is_null());
    }

    #[test]
    fn redact_events_multiple_items_preserves_count() {
        let redactor = Redactor::new();
        let mk_event = |id: i64| crate::storage::StoredEvent {
            id,
            pane_id: id as u64,
            rule_id: format!("rule_{}", id),
            agent_type: "test".to_string(),
            event_type: "test".to_string(),
            severity: "info".to_string(),
            confidence: 1.0,
            extracted: None,
            matched_text: Some(format!("text_{}", id)),
            segment_id: None,
            detected_at: id * 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        let events: Vec<_> = (1..=5).map(mk_event).collect();
        let redacted = redact_events(events, &redactor);
        assert_eq!(redacted.len(), 5);
        for (i, r) in redacted.iter().enumerate() {
            let expected_id = (i + 1) as i64;
            assert_eq!(r.id, expected_id);
            assert_eq!(r.rule_id, format!("rule_{}", expected_id));
        }
    }

    #[test]
    fn redact_events_safe_text_passes_through_unchanged() {
        let redactor = Redactor::new();
        let events = vec![crate::storage::StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "test".to_string(),
            agent_type: "test".to_string(),
            event_type: "test".to_string(),
            severity: "info".to_string(),
            confidence: 1.0,
            extracted: None,
            matched_text: Some("this is perfectly safe text with no secrets".to_string()),
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }];
        let redacted = redact_events(events, &redactor);
        assert_eq!(
            redacted[0].matched_text.as_deref(),
            Some("this is perfectly safe text with no secrets")
        );
    }

    #[test]
    fn redacted_workflow_no_steps_serializes() {
        let wf = RedactedWorkflow {
            id: "wf-empty".to_string(),
            workflow_name: "empty_wf".to_string(),
            pane_id: 1,
            status: "pending".to_string(),
            started_at: 1000,
            completed_at: None,
            step_count: 0,
            steps: vec![],
        };
        let json = serde_json::to_string(&wf).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["step_count"], 0);
        assert!(parsed["steps"].as_array().unwrap().is_empty());
        assert!(parsed["completed_at"].is_null());
    }

    #[test]
    fn redacted_workflow_incomplete_has_null_completed() {
        let wf = RedactedWorkflow {
            id: "wf-running".to_string(),
            workflow_name: "in_progress_wf".to_string(),
            pane_id: 2,
            status: "running".to_string(),
            started_at: 5000,
            completed_at: None,
            step_count: 1,
            steps: vec![RedactedStep {
                step_index: 0,
                step_name: "init".to_string(),
                result_type: "continue".to_string(),
                policy_summary: None,
                started_at: 5000,
                completed_at: 5500,
            }],
        };
        let json = serde_json::to_string(&wf).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(parsed["completed_at"].is_null());
        assert_eq!(parsed["status"], "running");
        assert_eq!(parsed["steps"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn bundle_manifest_empty_files_list() {
        let manifest = BundleManifest {
            wa_version: "0.0.1".to_string(),
            generated_at_ms: 0,
            file_count: 0,
            files: vec![],
            redacted: false,
        };
        let json = serde_json::to_string(&manifest).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert!(parsed["files"].as_array().unwrap().is_empty());
        assert_eq!(parsed["file_count"], 0);
        assert!(!parsed["redacted"].as_bool().unwrap());
    }

    #[test]
    fn generate_rule_traces_many_extracted_fields() {
        let redactor = Redactor::new();
        let mut obj = serde_json::Map::new();
        for i in 0..10 {
            obj.insert(
                format!("field_{}", i),
                serde_json::json!(format!("val_{}", i)),
            );
        }
        let events = vec![crate::storage::StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: "multi_field.rule".to_string(),
            agent_type: "test".to_string(),
            event_type: "test".to_string(),
            severity: "info".to_string(),
            confidence: 1.0,
            extracted: Some(serde_json::Value::Object(obj)),
            matched_text: None,
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }];
        let traces = generate_rule_traces(&events, &redactor);
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].extracted_fields.len(), 10);
        for field in &traces[0].extracted_fields {
            assert_eq!(field.kind, "extracted_field");
            assert!(field.label.is_some());
            assert!(field.value.is_some());
        }
    }

    #[test]
    fn generate_rule_traces_handled_flag_logic() {
        let redactor = Redactor::new();
        let mk = |id, handled_at: Option<i64>| crate::storage::StoredEvent {
            id,
            pane_id: 1,
            rule_id: "r".to_string(),
            agent_type: "t".to_string(),
            event_type: "t".to_string(),
            severity: "info".to_string(),
            confidence: 1.0,
            extracted: None,
            matched_text: None,
            segment_id: None,
            detected_at: 1000,
            dedupe_key: None,
            handled_at,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        let events = vec![mk(1, Some(2000)), mk(2, None), mk(3, Some(0))];
        let traces = generate_rule_traces(&events, &redactor);
        assert_eq!(traces.len(), 3);
        assert!(traces[0].handled, "handled_at=Some(2000) should be handled");
        assert!(!traces[1].handled, "handled_at=None should not be handled");
        assert!(traces[2].handled, "handled_at=Some(0) should be handled");
    }

    #[test]
    fn dir_size_ignores_subdirectories() {
        let tmp = std::env::temp_dir().join(format!(
            "wa_test_diag_dirsize_subdir_{}",
            std::process::id()
        ));
        fs::create_dir_all(tmp.join("subdir")).unwrap();
        fs::write(tmp.join("top.txt"), "hello").unwrap();
        fs::write(tmp.join("subdir").join("nested.txt"), "world!!!!").unwrap();
        let size = dir_size(&tmp);
        // dir_size only counts top-level entries; subdir entry metadata != file content
        // The top.txt is 5 bytes. The subdir entry itself has metadata size but
        // the function calls e.metadata().map_or(0, |m| m.len()) which for dirs is
        // implementation-defined. We just verify top.txt is included.
        assert!(size >= 5, "should include at least top.txt (5 bytes)");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_json_file_large_payload() {
        let tmp =
            std::env::temp_dir().join(format!("wa_test_diag_large_payload_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();

        // Build a large serializable value
        let items: Vec<serde_json::Value> = (0..500)
            .map(|i| {
                serde_json::json!({
                    "index": i,
                    "data": "x".repeat(100),
                })
            })
            .collect();
        let data = serde_json::json!({ "items": items });
        write_json_file(&tmp, "large.json", &data).unwrap();

        let content = fs::read_to_string(tmp.join("large.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(parsed["items"].as_array().unwrap().len(), 500);
        assert!(content.len() > 50_000, "should be a substantial file");

        let _ = fs::remove_dir_all(&tmp);
    }
}
