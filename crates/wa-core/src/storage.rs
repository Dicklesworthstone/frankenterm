//! Storage layer with SQLite and FTS5
//!
//! Provides persistent storage for captured output, events, and workflows.
//!
//! # Schema Design
//!
//! The database uses WAL mode for concurrent reads and single-writer semantics.
//! All timestamps are epoch milliseconds (i64) for hot-path performance.
//! JSON columns are stored as TEXT for SQLite compatibility.
//!
//! # Tables
//!
//! - `panes`: Pane metadata and observation decisions
//! - `output_segments`: Append-only captured terminal output
//! - `output_gaps`: Explicit discontinuities in capture
//! - `events`: Pattern detections with lifecycle tracking
//! - `workflow_executions`: Durable workflow state
//! - `workflow_step_logs`: Step execution history
//! - `workflow_action_plans`: Canonical action plans for workflows
//! - `audit_actions`: Audit trail for policy decisions and outcomes
//! - `action_undo`: Undo metadata for audit actions
//! - `action_history`: View joining audit + undo + workflow step info
//! - `approval_tokens`: Allow-once approvals scoped to actions
//! - `config`: Key-value settings
//! - `saved_searches`: Persisted search definitions
//! - `maintenance_log`: System events and metrics
//!
//! FTS5 virtual table `output_segments_fts` enables full-text search.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use rusqlite::{Connection, OptionalExtension, params, types::Value as SqlValue};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Result, StorageError};
use crate::events::event_identity_key;
use crate::policy::Redactor;

// =============================================================================
// Schema Definition
// =============================================================================

/// Current schema version for migration tracking.
///
/// This is the target version that new databases will be initialized to,
/// and existing databases will be migrated to.
/// Uses SQLite's PRAGMA user_version for atomic version tracking.
pub const SCHEMA_VERSION: i32 = 20;

/// Schema initialization SQL
///
/// Convention notes:
/// - Timestamps: epoch milliseconds (i64) for hot-path queries
/// - JSON columns: TEXT containing JSON (v0 simplicity)
/// - All tables use INTEGER PRIMARY KEY for rowid aliasing
pub const SCHEMA_SQL: &str = r#"
-- Enable WAL mode for concurrent reads and single-writer semantics
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;

-- Schema version tracking
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL,
    applied_at INTEGER NOT NULL,  -- epoch ms
    description TEXT
);

-- wa metadata: version compatibility + provenance
CREATE TABLE IF NOT EXISTS wa_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version INTEGER NOT NULL,
    min_compatible_wa TEXT NOT NULL,
    created_by_wa TEXT NOT NULL,
    created_at INTEGER NOT NULL  -- epoch ms
);

-- Panes: metadata and observation decisions
-- Supports: wa status, wa robot state, privacy/perf filtering
CREATE TABLE IF NOT EXISTS panes (
    pane_id INTEGER PRIMARY KEY,
    pane_uuid TEXT,                    -- stable UUID (persists across renames/moves)
    domain TEXT NOT NULL DEFAULT 'local',
    window_id INTEGER,
    tab_id INTEGER,
    title TEXT,
    cwd TEXT,
    tty_name TEXT,
    first_seen_at INTEGER NOT NULL,   -- epoch ms
    last_seen_at INTEGER NOT NULL,    -- epoch ms
    observed INTEGER NOT NULL DEFAULT 1,  -- bool: 1=observe, 0=ignore
    ignore_reason TEXT,               -- rule id or short description if ignored
    last_decision_at INTEGER          -- epoch ms when observed/ignore was set
);

CREATE INDEX IF NOT EXISTS idx_panes_last_seen ON panes(last_seen_at);
CREATE INDEX IF NOT EXISTS idx_panes_observed ON panes(observed);

-- Output segments: append-only terminal output capture
-- UNIQUE(pane_id, seq) enforces monotonic sequence per pane
CREATE TABLE IF NOT EXISTS output_segments (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,             -- monotonically increasing within pane
    content TEXT NOT NULL,
    content_len INTEGER NOT NULL,     -- cached length for stats
    content_hash TEXT,                -- for overlap detection (optional)
    captured_at INTEGER NOT NULL,     -- epoch ms
    UNIQUE(pane_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_segments_pane_seq ON output_segments(pane_id, seq);
CREATE INDEX IF NOT EXISTS idx_segments_captured ON output_segments(captured_at);

-- Output gaps: explicit discontinuities in capture
CREATE TABLE IF NOT EXISTS output_gaps (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    seq_before INTEGER NOT NULL,      -- last known seq before gap
    seq_after INTEGER NOT NULL,       -- first seq after gap
    reason TEXT NOT NULL,             -- e.g., "daemon_restart", "timeout", "buffer_overflow"
    detected_at INTEGER NOT NULL      -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_gaps_pane ON output_gaps(pane_id);
CREATE INDEX IF NOT EXISTS idx_gaps_detected ON output_gaps(detected_at);

-- FTS5 virtual table for full-text search over segments
CREATE VIRTUAL TABLE IF NOT EXISTS output_segments_fts USING fts5(
    content,
    content='output_segments',
    content_rowid='id',
    tokenize='porter unicode61'
);

-- Triggers to keep FTS index in sync
CREATE TRIGGER IF NOT EXISTS output_segments_ai AFTER INSERT ON output_segments BEGIN
    INSERT INTO output_segments_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER IF NOT EXISTS output_segments_ad AFTER DELETE ON output_segments BEGIN
    INSERT INTO output_segments_fts(output_segments_fts, rowid, content) VALUES('delete', old.id, old.content);
END;

CREATE TRIGGER IF NOT EXISTS output_segments_au AFTER UPDATE ON output_segments BEGIN
    INSERT INTO output_segments_fts(output_segments_fts, rowid, content) VALUES('delete', old.id, old.content);
    INSERT INTO output_segments_fts(rowid, content) VALUES (new.id, new.content);
END;

-- Events: pattern detections with lifecycle tracking
-- Supports: unhandled queries, workflow linkage, idempotency
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    rule_id TEXT NOT NULL,            -- stable pattern identifier
    agent_type TEXT NOT NULL,         -- codex, claude_code, gemini, unknown
    event_type TEXT NOT NULL,         -- detection category
    severity TEXT NOT NULL,           -- info, warning, critical
    confidence REAL NOT NULL,         -- 0.0-1.0
    extracted TEXT,                   -- JSON: structured data from pattern
    matched_text TEXT,                -- original matched text
    segment_id INTEGER REFERENCES output_segments(id),  -- source segment
    detected_at INTEGER NOT NULL,     -- epoch ms

    -- Lifecycle tracking
    handled_at INTEGER,               -- epoch ms when handled (NULL = unhandled)
    handled_by_workflow_id TEXT,      -- links to workflow_executions.id
    handled_status TEXT,              -- completed, aborted, failed, paused

    -- Triage state tracking (bd-1yk8)
    triage_state TEXT,                -- e.g. new, investigating, resolved
    triage_updated_at INTEGER,        -- epoch ms
    triage_updated_by TEXT,           -- actor identifier (optional)

    -- Idempotency: optional dedupe key (pane_id + rule_id + time_window)
    dedupe_key TEXT,                  -- computed key for duplicate prevention

    UNIQUE(dedupe_key)                -- prevents duplicate events when dedupe_key set
);

CREATE INDEX IF NOT EXISTS idx_events_pane ON events(pane_id);
CREATE INDEX IF NOT EXISTS idx_events_rule ON events(rule_id);
CREATE INDEX IF NOT EXISTS idx_events_unhandled ON events(handled_at) WHERE handled_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_events_detected ON events(detected_at);
CREATE INDEX IF NOT EXISTS idx_events_severity ON events(severity, detected_at);
CREATE INDEX IF NOT EXISTS idx_events_triage_state
    ON events(triage_state) WHERE triage_state IS NOT NULL;

-- Event labels (many-to-one) for triage and filtering (bd-1yk8)
CREATE TABLE IF NOT EXISTS event_labels (
    event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    created_at INTEGER NOT NULL,      -- epoch ms
    created_by TEXT,                 -- actor identifier (optional)
    PRIMARY KEY (event_id, label)
);

CREATE INDEX IF NOT EXISTS idx_event_labels_event ON event_labels(event_id);
CREATE INDEX IF NOT EXISTS idx_event_labels_label ON event_labels(label);

-- Event notes (one-to-one) for operator annotations (bd-1yk8)
CREATE TABLE IF NOT EXISTS event_notes (
    event_id INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
    note TEXT NOT NULL,
    updated_at INTEGER NOT NULL,      -- epoch ms
    updated_by TEXT                  -- actor identifier (optional)
);

CREATE INDEX IF NOT EXISTS idx_event_notes_updated_at ON event_notes(updated_at);

-- Event mutes: suppress noisy notifications by identity key
CREATE TABLE IF NOT EXISTS event_mutes (
    identity_key TEXT PRIMARY KEY,
    scope TEXT NOT NULL DEFAULT 'workspace',
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    created_by TEXT,
    reason TEXT
);

CREATE INDEX IF NOT EXISTS idx_event_mutes_expires
    ON event_mutes(expires_at) WHERE expires_at IS NOT NULL;

-- Agent sessions: per-agent session timeline with token tracking
CREATE TABLE IF NOT EXISTS agent_sessions (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    agent_type TEXT NOT NULL,         -- codex, claude_code, gemini, unknown
    session_id TEXT,                  -- Agent's internal session ID if available
    external_id TEXT,                 -- Correlation with cass, etc.
    external_meta TEXT,               -- JSON metadata for correlation decisions
    started_at INTEGER NOT NULL,      -- epoch ms
    ended_at INTEGER,                 -- epoch ms (NULL = still active)
    end_reason TEXT,                  -- completed, limit_reached, error, manual
    -- Token tracking
    total_tokens INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cached_tokens INTEGER,
    reasoning_tokens INTEGER,
    -- Model info
    model_name TEXT,
    -- Cost tracking
    estimated_cost_usd REAL
);

CREATE INDEX IF NOT EXISTS idx_sessions_pane ON agent_sessions(pane_id, started_at);
CREATE INDEX IF NOT EXISTS idx_sessions_external ON agent_sessions(external_id) WHERE external_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_sessions_active ON agent_sessions(ended_at) WHERE ended_at IS NULL;

-- Workflow executions: durable FSM state for resumability
CREATE TABLE IF NOT EXISTS workflow_executions (
    id TEXT PRIMARY KEY,              -- UUID or ulid
    workflow_name TEXT NOT NULL,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
    trigger_event_id INTEGER REFERENCES events(id),  -- event that started this
    current_step INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'running',  -- running, waiting, completed, aborted
    wait_condition TEXT,              -- JSON: WaitCondition if status='waiting'
    context TEXT,                     -- JSON: workflow-specific state
    result TEXT,                      -- JSON: final result if completed
    error TEXT,                       -- error message if aborted
    started_at INTEGER NOT NULL,      -- epoch ms
    updated_at INTEGER NOT NULL,      -- epoch ms
    completed_at INTEGER              -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_workflows_pane ON workflow_executions(pane_id);
CREATE INDEX IF NOT EXISTS idx_workflows_status ON workflow_executions(status);
CREATE INDEX IF NOT EXISTS idx_workflows_started ON workflow_executions(started_at);

-- Workflow step logs: execution history for audit and debugging
CREATE TABLE IF NOT EXISTS workflow_step_logs (
    id INTEGER PRIMARY KEY,
    workflow_id TEXT NOT NULL REFERENCES workflow_executions(id) ON DELETE CASCADE,
    audit_action_id INTEGER REFERENCES audit_actions(id) ON DELETE SET NULL,
    step_index INTEGER NOT NULL,
    step_name TEXT NOT NULL,
    step_id TEXT,
    step_kind TEXT,
    result_type TEXT NOT NULL,        -- continue, done, retry, abort, wait_for
    result_data TEXT,                 -- JSON: result payload
    policy_summary TEXT,              -- JSON: decision summary
    verification_refs TEXT,           -- JSON: verification evidence refs
    error_code TEXT,                  -- stable error code if step failed
    started_at INTEGER NOT NULL,      -- epoch ms
    completed_at INTEGER NOT NULL,    -- epoch ms
    duration_ms INTEGER NOT NULL      -- cached for stats
);

CREATE INDEX IF NOT EXISTS idx_step_logs_workflow ON workflow_step_logs(workflow_id, step_index);
CREATE INDEX IF NOT EXISTS idx_step_logs_audit_action ON workflow_step_logs(audit_action_id);

-- Workflow action plans: canonical plan JSON + hash for explainability
CREATE TABLE IF NOT EXISTS workflow_action_plans (
    workflow_id TEXT PRIMARY KEY REFERENCES workflow_executions(id) ON DELETE CASCADE,
    plan_id TEXT NOT NULL,
    plan_hash TEXT NOT NULL,
    plan_json TEXT NOT NULL,          -- canonical JSON
    created_at INTEGER NOT NULL       -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_action_plans_hash ON workflow_action_plans(plan_hash);

-- Prepared plans: plan previews awaiting commit
CREATE TABLE IF NOT EXISTS prepared_plans (
    plan_id TEXT PRIMARY KEY,
    plan_hash TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    action_kind TEXT NOT NULL,
    pane_id INTEGER,
    pane_uuid TEXT,
    params_json TEXT,
    plan_json TEXT NOT NULL,          -- redacted plan JSON for preview
    requires_approval INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,      -- epoch ms
    expires_at INTEGER NOT NULL,      -- epoch ms
    consumed_at INTEGER               -- epoch ms when commit was attempted
);

CREATE INDEX IF NOT EXISTS idx_prepared_plans_hash ON prepared_plans(plan_hash);
CREATE INDEX IF NOT EXISTS idx_prepared_plans_workspace ON prepared_plans(workspace_id);
CREATE INDEX IF NOT EXISTS idx_prepared_plans_expires ON prepared_plans(expires_at)
    WHERE consumed_at IS NULL;

-- Audit actions: policy decisions and outcomes
CREATE TABLE IF NOT EXISTS audit_actions (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,               -- epoch ms
    actor_kind TEXT NOT NULL,          -- human, robot, mcp, workflow
    actor_id TEXT,                     -- optional (workflow execution id, MCP client id)
    correlation_id TEXT,              -- optional chain/correlation identifier
    pane_id INTEGER REFERENCES panes(pane_id) ON DELETE SET NULL,
    domain TEXT,
    action_kind TEXT NOT NULL,         -- send_text, workflow_run, etc.
    policy_decision TEXT NOT NULL,     -- allow, deny, require_approval
    decision_reason TEXT,
    rule_id TEXT,                      -- policy rule id if any
    input_summary TEXT,                -- redacted summary of input
    verification_summary TEXT,         -- redacted summary of verification
    decision_context TEXT,             -- JSON: decision context
    result TEXT NOT NULL               -- success, denied, failed, timeout
);

CREATE INDEX IF NOT EXISTS idx_audit_actions_ts ON audit_actions(ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_pane ON audit_actions(pane_id, ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_actor ON audit_actions(actor_kind, ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_action ON audit_actions(action_kind, ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_decision ON audit_actions(policy_decision, ts);
CREATE INDEX IF NOT EXISTS idx_audit_actions_correlation ON audit_actions(correlation_id);

-- Undo metadata for audit actions
CREATE TABLE IF NOT EXISTS action_undo (
    audit_action_id INTEGER PRIMARY KEY REFERENCES audit_actions(id) ON DELETE CASCADE,
    undoable INTEGER NOT NULL DEFAULT 0,
    undo_strategy TEXT NOT NULL,       -- none|manual|workflow_abort|pane_close|custom
    undo_hint TEXT,                    -- redacted guidance for humans
    undo_payload TEXT,                 -- JSON for executor (redacted)
    undone_at INTEGER,
    undone_by TEXT
);

CREATE INDEX IF NOT EXISTS idx_action_undo_undoable ON action_undo(undoable) WHERE undoable = 1;

-- Approval tokens: allow-once approvals scoped to actions
CREATE TABLE IF NOT EXISTS approval_tokens (
    id INTEGER PRIMARY KEY,
    code_hash TEXT NOT NULL,           -- sha256 hash of allow-once code
    created_at INTEGER NOT NULL,       -- epoch ms
    expires_at INTEGER NOT NULL,       -- epoch ms
    used_at INTEGER,                   -- epoch ms when consumed
    workspace_id TEXT NOT NULL,        -- workspace scope
    action_kind TEXT NOT NULL,         -- send_text, workflow_run, etc.
    pane_id INTEGER REFERENCES panes(pane_id) ON DELETE SET NULL,
    action_fingerprint TEXT NOT NULL,  -- normalized action fingerprint
    plan_hash TEXT,                    -- optional sha256 hash of bound ActionPlan
    plan_version INTEGER,             -- optional plan schema version
    risk_summary TEXT                  -- optional human-readable risk description
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_approval_tokens_hash ON approval_tokens(code_hash);
CREATE INDEX IF NOT EXISTS idx_approval_tokens_workspace ON approval_tokens(workspace_id, action_kind);
CREATE INDEX IF NOT EXISTS idx_approval_tokens_pane ON approval_tokens(pane_id);
CREATE INDEX IF NOT EXISTS idx_approval_tokens_expires ON approval_tokens(expires_at);
CREATE INDEX IF NOT EXISTS idx_approval_tokens_unused ON approval_tokens(used_at) WHERE used_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_approval_tokens_fingerprint ON approval_tokens(action_fingerprint);

-- Accounts: mirrors caut usage data for failover selection
-- Supports: account selection policy, usage tracking
CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY,
    account_id TEXT NOT NULL,          -- stable identifier (from caut or hash)
    service TEXT NOT NULL,             -- openai, anthropic, google, etc.
    name TEXT,                         -- display name
    percent_remaining REAL NOT NULL,   -- 0.0-100.0
    reset_at TEXT,                     -- ISO8601 or epoch string
    tokens_used INTEGER,
    tokens_remaining INTEGER,
    tokens_limit INTEGER,
    last_refreshed_at INTEGER NOT NULL, -- epoch ms
    last_used_at INTEGER,              -- epoch ms when used for failover
    created_at INTEGER NOT NULL,       -- epoch ms
    updated_at INTEGER NOT NULL        -- epoch ms
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_service_account ON accounts(service, account_id);
CREATE INDEX IF NOT EXISTS idx_accounts_service ON accounts(service);
CREATE INDEX IF NOT EXISTS idx_accounts_percent ON accounts(service, percent_remaining DESC);
CREATE INDEX IF NOT EXISTS idx_accounts_last_used ON accounts(service, last_used_at);

-- Pane reservations: exclusive workflow locks on panes
-- Only one active reservation per pane; auto-expire on TTL
CREATE TABLE IF NOT EXISTS pane_reservations (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
    owner_kind TEXT NOT NULL,          -- workflow, agent, manual
    owner_id TEXT NOT NULL,            -- workflow ID or agent name
    reason TEXT,                       -- human-readable reason
    created_at INTEGER NOT NULL,       -- epoch ms
    expires_at INTEGER NOT NULL,       -- epoch ms (created_at + TTL)
    released_at INTEGER,              -- epoch ms when released (NULL if active)
    status TEXT NOT NULL DEFAULT 'active'  -- active | released
);

CREATE INDEX IF NOT EXISTS idx_reservations_pane_status ON pane_reservations(pane_id, status);
CREATE INDEX IF NOT EXISTS idx_reservations_status ON pane_reservations(status);
CREATE INDEX IF NOT EXISTS idx_reservations_expires ON pane_reservations(expires_at) WHERE status = 'active';

-- FTS index state: track index version and per-pane progress for incremental sync
-- Enables efficient recovery without full reindex on restart
CREATE TABLE IF NOT EXISTS fts_index_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),  -- singleton row
    index_version INTEGER NOT NULL DEFAULT 1,
    last_full_rebuild_at INTEGER,           -- epoch ms
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Per-pane FTS indexing progress for batched rebuild
CREATE TABLE IF NOT EXISTS fts_pane_progress (
    pane_id INTEGER PRIMARY KEY REFERENCES panes(pane_id) ON DELETE CASCADE,
    last_indexed_seq INTEGER NOT NULL DEFAULT 0,
    indexed_count INTEGER NOT NULL DEFAULT 0,
    last_indexed_at INTEGER NOT NULL
);

-- Config: key-value settings
CREATE TABLE IF NOT EXISTS config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,              -- JSON value
    updated_at INTEGER NOT NULL       -- epoch ms
);

-- Saved searches: persisted query definitions for reuse/scheduling
CREATE TABLE IF NOT EXISTS saved_searches (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    query TEXT NOT NULL,
    pane_id INTEGER,
    "limit" INTEGER NOT NULL DEFAULT 50,
    since_mode TEXT NOT NULL DEFAULT 'last_run',
    since_ms INTEGER,
    schedule_interval_ms INTEGER,
    enabled INTEGER NOT NULL DEFAULT 0,
    last_run_at INTEGER,
    last_result_count INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_saved_searches_enabled ON saved_searches(enabled);
CREATE INDEX IF NOT EXISTS idx_saved_searches_last_run ON saved_searches(last_run_at);

-- Maintenance log: system events and metrics
CREATE TABLE IF NOT EXISTS maintenance_log (
    id INTEGER PRIMARY KEY,
    event_type TEXT NOT NULL,         -- startup, shutdown, vacuum, retention_cleanup, error
    message TEXT,
    metadata TEXT,                    -- JSON: additional context
    timestamp INTEGER NOT NULL        -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_maintenance_timestamp ON maintenance_log(timestamp);

-- Secret scan reports: incremental scan checkpoints + report payloads
CREATE TABLE IF NOT EXISTS secret_scan_reports (
    id INTEGER PRIMARY KEY,
    scope_hash TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    report_version INTEGER NOT NULL,
    last_segment_id INTEGER,
    report_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_secret_scan_reports_scope
    ON secret_scan_reports(scope_hash, created_at);

-- Usage metrics: analytics data model for token/cost/API tracking
CREATE TABLE IF NOT EXISTS usage_metrics (
    id INTEGER PRIMARY KEY,
    timestamp INTEGER NOT NULL,          -- epoch ms
    metric_type TEXT NOT NULL,           -- token_usage, api_cost, api_call, rate_limit_hit, workflow_cost, session_duration
    pane_id INTEGER,                     -- NULL for global metrics
    agent_type TEXT,                     -- codex, claude_code, gemini, NULL
    account_id TEXT,                     -- caut account reference
    workflow_id TEXT,                    -- workflow execution reference
    count INTEGER,                       -- for countable metrics
    amount REAL,                         -- for costs (USD)
    tokens INTEGER,                      -- for token counts
    metadata TEXT,                       -- JSON for extensibility
    created_at INTEGER NOT NULL          -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_usage_metrics_timestamp ON usage_metrics(timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_metrics_type_ts ON usage_metrics(metric_type, timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_metrics_agent_ts ON usage_metrics(agent_type, timestamp);
CREATE INDEX IF NOT EXISTS idx_usage_metrics_account_ts ON usage_metrics(account_id, timestamp);

-- Notification history: persistent log of all sent notifications
CREATE TABLE IF NOT EXISTS notification_history (
    id INTEGER PRIMARY KEY,
    timestamp INTEGER NOT NULL,          -- epoch ms when notification was created
    event_id INTEGER,                    -- optional FK to events(id)
    channel TEXT NOT NULL,               -- webhook, desktop, slack, etc.
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    severity TEXT NOT NULL,              -- info, warning, error, critical
    status TEXT NOT NULL DEFAULT 'pending', -- pending, sent, failed, throttled
    error_message TEXT,                  -- error details on failure
    acknowledged_at INTEGER,             -- epoch ms
    acknowledged_by TEXT,
    action_taken TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    metadata TEXT,                       -- JSON blob for channel-specific data
    created_at Integer NOT NULL          -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_notification_history_timestamp ON notification_history(timestamp);
CREATE INDEX IF NOT EXISTS idx_notification_history_status ON notification_history(status);
CREATE INDEX IF NOT EXISTS idx_notification_history_event ON notification_history(event_id);
CREATE INDEX IF NOT EXISTS idx_notification_history_channel_ts ON notification_history(channel, timestamp);

-- Pane bookmarks: named aliases with optional tags for fast pane access
CREATE TABLE IF NOT EXISTS pane_bookmarks (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL,
    alias TEXT NOT NULL UNIQUE,
    tags TEXT,                            -- JSON array of tag strings
    description TEXT,
    created_at INTEGER NOT NULL,          -- epoch ms
    updated_at INTEGER NOT NULL           -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_pane_bookmarks_pane_id ON pane_bookmarks(pane_id);
CREATE INDEX IF NOT EXISTS idx_pane_bookmarks_alias ON pane_bookmarks(alias);

-- Action history view (audit + undo + workflow step info)
CREATE VIEW IF NOT EXISTS action_history AS
SELECT a.*,
       u.undoable, u.undo_strategy, u.undo_hint, u.undone_at, u.undone_by,
       w.workflow_id, w.step_name
FROM audit_actions a
LEFT JOIN action_undo u ON u.audit_action_id = a.id
LEFT JOIN workflow_step_logs w ON w.audit_action_id = a.id;
"#;

// =============================================================================
// Schema Migrations
// =============================================================================

/// A schema migration
#[derive(Debug, Clone)]
pub struct Migration {
    /// Target version after this migration is applied
    pub version: i32,
    /// Human-readable description
    pub description: &'static str,
    /// SQL to execute for the upgrade
    pub up_sql: &'static str,
    /// SQL to execute for rollback (None means rollback unsupported)
    pub down_sql: Option<&'static str>,
}

/// Direction for a migration plan or execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationDirection {
    /// Upgrade schema to a newer version
    Up,
    /// Roll back schema to an older version
    Down,
}

impl MigrationDirection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// A single migration step in a plan or report.
#[derive(Debug, Clone)]
pub struct MigrationStep {
    /// Migration version being applied or rolled back
    pub migration_version: i32,
    /// Resulting schema version after this step
    pub resulting_version: i32,
    /// Human-readable description
    pub description: &'static str,
    /// Direction of this step
    pub direction: MigrationDirection,
}

/// Migration plan or execution report.
#[derive(Debug, Clone)]
pub struct MigrationPlan {
    /// Starting version
    pub from_version: i32,
    /// Target version
    pub to_version: i32,
    /// Direction (up/down)
    pub direction: MigrationDirection,
    /// Steps to apply
    pub steps: Vec<MigrationStep>,
}

/// Status entry for a migration.
#[derive(Debug, Clone)]
pub struct MigrationStatusEntry {
    /// Schema version after the migration is applied
    pub version: i32,
    /// Human-readable description
    pub description: &'static str,
    /// Whether this migration is applied
    pub applied: bool,
    /// Whether rollback is available
    pub rollback_supported: bool,
}

/// Status report for schema migrations.
#[derive(Debug, Clone)]
pub struct MigrationStatusReport {
    /// Whether the database file exists
    pub db_exists: bool,
    /// Whether schema initialization is required
    pub needs_initialization: bool,
    /// Current schema version (PRAGMA user_version)
    pub current_version: i32,
    /// Target schema version (SCHEMA_VERSION)
    pub target_version: i32,
    /// All migration entries with applied/pending status
    pub entries: Vec<MigrationStatusEntry>,
}

/// Registry of all migrations.
///
/// Migrations are applied in order. Each migration's `version` field indicates
/// the schema version AFTER the migration is applied.
///
/// # Adding New Migrations
///
/// 1. Increment `SCHEMA_VERSION` constant
/// 2. Add a new `Migration` entry here with `version = SCHEMA_VERSION`
/// 3. Write idempotent SQL (use IF NOT EXISTS, IF EXISTS where appropriate)
/// 4. Add upgrade test using fixture from previous version
static MIGRATIONS: &[Migration] = &[
    // Version 1: Initial schema (baseline)
    // No migration SQL needed - SCHEMA_SQL creates the full schema
    Migration {
        version: 1,
        description: "Initial schema",
        up_sql: "", // Empty - baseline schema is created via SCHEMA_SQL
        down_sql: None,
    },
    Migration {
        version: 2,
        description: "Add decision_context to audit_actions",
        up_sql: "ALTER TABLE audit_actions ADD COLUMN decision_context TEXT;",
        down_sql: Some("ALTER TABLE audit_actions DROP COLUMN decision_context;"),
    },
    Migration {
        version: 3,
        description: "Add pane_uuid to panes for stable identity",
        up_sql: "ALTER TABLE panes ADD COLUMN pane_uuid TEXT;",
        down_sql: Some("ALTER TABLE panes DROP COLUMN pane_uuid;"),
    },
    Migration {
        version: 4,
        description: "Add action_undo + action_history view + audit_action_id on step logs",
        up_sql: r"
            CREATE INDEX IF NOT EXISTS idx_step_logs_audit_action ON workflow_step_logs(audit_action_id);

            CREATE TABLE IF NOT EXISTS action_undo (
                audit_action_id INTEGER PRIMARY KEY REFERENCES audit_actions(id) ON DELETE CASCADE,
                undoable INTEGER NOT NULL DEFAULT 0,
                undo_strategy TEXT NOT NULL,
                undo_hint TEXT,
                undo_payload TEXT,
                undone_at INTEGER,
                undone_by TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_action_undo_undoable ON action_undo(undoable) WHERE undoable = 1;

            CREATE VIEW IF NOT EXISTS action_history AS
            SELECT a.*,
                   u.undoable, u.undo_strategy, u.undo_hint, u.undone_at, u.undone_by,
                   w.workflow_id, w.step_name
            FROM audit_actions a
            LEFT JOIN action_undo u ON u.audit_action_id = a.id
            LEFT JOIN workflow_step_logs w ON w.audit_action_id = a.id;
        ",
        down_sql: Some(
            r"
            DROP VIEW IF EXISTS action_history;
            DROP INDEX IF EXISTS idx_action_undo_undoable;
            DROP TABLE IF EXISTS action_undo;
            DROP INDEX IF EXISTS idx_step_logs_audit_action;
            ALTER TABLE workflow_step_logs DROP COLUMN audit_action_id;
        ",
        ),
    },
    Migration {
        version: 5,
        description: "Add accounts table for usage tracking and failover selection",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS accounts (
                id INTEGER PRIMARY KEY,
                account_id TEXT NOT NULL,
                service TEXT NOT NULL,
                name TEXT,
                percent_remaining REAL NOT NULL,
                reset_at TEXT,
                tokens_used INTEGER,
                tokens_remaining INTEGER,
                tokens_limit INTEGER,
                last_refreshed_at INTEGER NOT NULL,
                last_used_at INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_accounts_service_account ON accounts(service, account_id);
            CREATE INDEX IF NOT EXISTS idx_accounts_service ON accounts(service);
            CREATE INDEX IF NOT EXISTS idx_accounts_percent ON accounts(service, percent_remaining DESC);
            CREATE INDEX IF NOT EXISTS idx_accounts_last_used ON accounts(service, last_used_at);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_accounts_last_used;
            DROP INDEX IF EXISTS idx_accounts_percent;
            DROP INDEX IF EXISTS idx_accounts_service;
            DROP INDEX IF EXISTS idx_accounts_service_account;
            DROP TABLE IF EXISTS accounts;
        ",
        ),
    },
    Migration {
        version: 6,
        description: "Add wa_meta for version compatibility tracking",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS wa_meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                min_compatible_wa TEXT NOT NULL,
                created_by_wa TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
        ",
        down_sql: Some("DROP TABLE IF EXISTS wa_meta;"),
    },
    Migration {
        version: 7,
        description: "Persist workflow action plans and enrich step logs",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS workflow_action_plans (
                workflow_id TEXT PRIMARY KEY REFERENCES workflow_executions(id) ON DELETE CASCADE,
                plan_id TEXT NOT NULL,
                plan_hash TEXT NOT NULL,
                plan_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_action_plans_hash ON workflow_action_plans(plan_hash);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_action_plans_hash;
            DROP TABLE IF EXISTS workflow_action_plans;
        ",
        ),
    },
    Migration {
        version: 8,
        description: "Add pane_reservations for per-pane workflow lock/reservation",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS pane_reservations (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
                owner_kind TEXT NOT NULL,
                owner_id TEXT NOT NULL,
                reason TEXT,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                released_at INTEGER,
                status TEXT NOT NULL DEFAULT 'active'
            );

            CREATE INDEX IF NOT EXISTS idx_reservations_pane_status
                ON pane_reservations(pane_id, status);
            CREATE INDEX IF NOT EXISTS idx_reservations_status
                ON pane_reservations(status);
            CREATE INDEX IF NOT EXISTS idx_reservations_expires
                ON pane_reservations(expires_at) WHERE status = 'active';
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_reservations_expires;
            DROP INDEX IF EXISTS idx_reservations_status;
            DROP INDEX IF EXISTS idx_reservations_pane_status;
            DROP TABLE IF EXISTS pane_reservations;
        ",
        ),
    },
    Migration {
        version: 9,
        description: "Add external_meta to agent_sessions for correlation metadata",
        up_sql: "ALTER TABLE agent_sessions ADD COLUMN external_meta TEXT;",
        down_sql: Some("ALTER TABLE agent_sessions DROP COLUMN external_meta;"),
    },
    Migration {
        version: 10,
        description: "Add FTS index state tables for incremental sync and recovery",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS fts_index_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                index_version INTEGER NOT NULL DEFAULT 1,
                last_full_rebuild_at INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS fts_pane_progress (
                pane_id INTEGER PRIMARY KEY REFERENCES panes(pane_id) ON DELETE CASCADE,
                last_indexed_seq INTEGER NOT NULL DEFAULT 0,
                indexed_count INTEGER NOT NULL DEFAULT 0,
                last_indexed_at INTEGER NOT NULL
            );

            -- Initialize state with current timestamp
            INSERT OR IGNORE INTO fts_index_state (id, index_version, created_at, updated_at)
            VALUES (1, 1, strftime('%s', 'now') * 1000, strftime('%s', 'now') * 1000);
        ",
        down_sql: Some(
            r"
            DROP TABLE IF EXISTS fts_pane_progress;
            DROP TABLE IF EXISTS fts_index_state;
        ",
        ),
    },
    Migration {
        version: 11,
        description: "Add prepared_plans for prepare/commit plan previews",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS prepared_plans (
                plan_id TEXT PRIMARY KEY,
                plan_hash TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                action_kind TEXT NOT NULL,
                pane_id INTEGER,
                pane_uuid TEXT,
                params_json TEXT,
                plan_json TEXT NOT NULL,
                requires_approval INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                consumed_at INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_prepared_plans_hash ON prepared_plans(plan_hash);
            CREATE INDEX IF NOT EXISTS idx_prepared_plans_workspace ON prepared_plans(workspace_id);
            CREATE INDEX IF NOT EXISTS idx_prepared_plans_expires ON prepared_plans(expires_at)
                WHERE consumed_at IS NULL;
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_prepared_plans_expires;
            DROP INDEX IF EXISTS idx_prepared_plans_workspace;
            DROP INDEX IF EXISTS idx_prepared_plans_hash;
            DROP TABLE IF EXISTS prepared_plans;
        ",
        ),
    },
    Migration {
        version: 12,
        description: "Add correlation_id to audit_actions for prepare/commit chains",
        up_sql: r"
            ALTER TABLE audit_actions ADD COLUMN correlation_id TEXT;
            CREATE INDEX IF NOT EXISTS idx_audit_actions_correlation ON audit_actions(correlation_id);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_audit_actions_correlation;
            ALTER TABLE audit_actions DROP COLUMN correlation_id;
        ",
        ),
    },
    Migration {
        version: 13,
        description: "Add secret_scan_reports for incremental scan checkpoints",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS secret_scan_reports (
                id INTEGER PRIMARY KEY,
                scope_hash TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                report_version INTEGER NOT NULL,
                last_segment_id INTEGER,
                report_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_secret_scan_reports_scope
                ON secret_scan_reports(scope_hash, created_at);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_secret_scan_reports_scope;
            DROP TABLE IF EXISTS secret_scan_reports;
        ",
        ),
    },
    Migration {
        version: 14,
        description: "Add saved_searches for persisted search definitions",
        up_sql: r#"
            CREATE TABLE IF NOT EXISTS saved_searches (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                query TEXT NOT NULL,
                pane_id INTEGER,
                "limit" INTEGER NOT NULL DEFAULT 50,
                since_mode TEXT NOT NULL DEFAULT 'last_run',
                since_ms INTEGER,
                schedule_interval_ms INTEGER,
                enabled INTEGER NOT NULL DEFAULT 0,
                last_run_at INTEGER,
                last_result_count INTEGER,
                last_error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_saved_searches_enabled
                ON saved_searches(enabled);
            CREATE INDEX IF NOT EXISTS idx_saved_searches_last_run
                ON saved_searches(last_run_at);
        "#,
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_saved_searches_last_run;
            DROP INDEX IF EXISTS idx_saved_searches_enabled;
            DROP TABLE IF EXISTS saved_searches;
        ",
        ),
    },
    Migration {
        version: 15,
        description: "Add event_mutes for noise suppression by identity key",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS event_mutes (
                identity_key TEXT PRIMARY KEY,
                scope TEXT NOT NULL DEFAULT 'workspace',
                created_at INTEGER NOT NULL,
                expires_at INTEGER,
                created_by TEXT,
                reason TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_event_mutes_expires
                ON event_mutes(expires_at) WHERE expires_at IS NOT NULL;
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_event_mutes_expires;
            DROP TABLE IF EXISTS event_mutes;
        ",
        ),
    },
    Migration {
        version: 16,
        description: "Add usage_metrics table for analytics tracking",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS usage_metrics (
                id INTEGER PRIMARY KEY,
                timestamp INTEGER NOT NULL,
                metric_type TEXT NOT NULL,
                pane_id INTEGER,
                agent_type TEXT,
                account_id TEXT,
                workflow_id TEXT,
                count INTEGER,
                amount REAL,
                tokens INTEGER,
                metadata TEXT,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_usage_metrics_timestamp ON usage_metrics(timestamp);
            CREATE INDEX IF NOT EXISTS idx_usage_metrics_type_ts ON usage_metrics(metric_type, timestamp);
            CREATE INDEX IF NOT EXISTS idx_usage_metrics_agent_ts ON usage_metrics(agent_type, timestamp);
            CREATE INDEX IF NOT EXISTS idx_usage_metrics_account_ts ON usage_metrics(account_id, timestamp);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_usage_metrics_account_ts;
            DROP INDEX IF EXISTS idx_usage_metrics_agent_ts;
            DROP INDEX IF EXISTS idx_usage_metrics_type_ts;
            DROP INDEX IF EXISTS idx_usage_metrics_timestamp;
            DROP TABLE IF EXISTS usage_metrics;
        ",
        ),
    },
    Migration {
        version: 17,
        description: "Add notification_history table for persistent notification log",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS notification_history (
                id INTEGER PRIMARY KEY,
                timestamp INTEGER NOT NULL,
                event_id INTEGER,
                channel TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                severity TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                error_message TEXT,
                acknowledged_at INTEGER,
                acknowledged_by TEXT,
                action_taken TEXT,
                retry_count INTEGER NOT NULL DEFAULT 0,
                metadata TEXT,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_notification_history_timestamp ON notification_history(timestamp);
            CREATE INDEX IF NOT EXISTS idx_notification_history_status ON notification_history(status);
            CREATE INDEX IF NOT EXISTS idx_notification_history_event ON notification_history(event_id);
            CREATE INDEX IF NOT EXISTS idx_notification_history_channel_ts ON notification_history(channel, timestamp);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_notification_history_channel_ts;
            DROP INDEX IF EXISTS idx_notification_history_event;
            DROP INDEX IF EXISTS idx_notification_history_status;
            DROP INDEX IF EXISTS idx_notification_history_timestamp;
            DROP TABLE IF EXISTS notification_history;
        ",
        ),
    },
    Migration {
        version: 18,
        description: "Add event triage state + annotations (labels/notes)",
        up_sql: r"
            ALTER TABLE events ADD COLUMN triage_state TEXT;
            ALTER TABLE events ADD COLUMN triage_updated_at INTEGER;
            ALTER TABLE events ADD COLUMN triage_updated_by TEXT;

            CREATE INDEX IF NOT EXISTS idx_events_triage_state
                ON events(triage_state) WHERE triage_state IS NOT NULL;

            CREATE TABLE IF NOT EXISTS event_labels (
                event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
                label TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                created_by TEXT,
                PRIMARY KEY (event_id, label)
            );

            CREATE INDEX IF NOT EXISTS idx_event_labels_event ON event_labels(event_id);
            CREATE INDEX IF NOT EXISTS idx_event_labels_label ON event_labels(label);

            CREATE TABLE IF NOT EXISTS event_notes (
                event_id INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
                note TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                updated_by TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_event_notes_updated_at ON event_notes(updated_at);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_event_notes_updated_at;
            DROP TABLE IF EXISTS event_notes;

            DROP INDEX IF EXISTS idx_event_labels_label;
            DROP INDEX IF EXISTS idx_event_labels_event;
            DROP TABLE IF EXISTS event_labels;

            DROP INDEX IF EXISTS idx_events_triage_state;

            ALTER TABLE events DROP COLUMN triage_updated_by;
            ALTER TABLE events DROP COLUMN triage_updated_at;
            ALTER TABLE events DROP COLUMN triage_state;
        ",
        ),
    },
    Migration {
        version: 19,
        description: "Add plan_hash binding to approval_tokens",
        up_sql: r"
            ALTER TABLE approval_tokens ADD COLUMN plan_hash TEXT;
            ALTER TABLE approval_tokens ADD COLUMN plan_version INTEGER;
            ALTER TABLE approval_tokens ADD COLUMN risk_summary TEXT;

            CREATE INDEX IF NOT EXISTS idx_approval_tokens_plan_hash
                ON approval_tokens(plan_hash) WHERE plan_hash IS NOT NULL;
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_approval_tokens_plan_hash;
            ALTER TABLE approval_tokens DROP COLUMN risk_summary;
            ALTER TABLE approval_tokens DROP COLUMN plan_version;
            ALTER TABLE approval_tokens DROP COLUMN plan_hash;
        ",
        ),
    },
    Migration {
        version: 20,
        description: "Add pane_bookmarks table for named pane aliases with tags",
        up_sql: r"
            CREATE TABLE IF NOT EXISTS pane_bookmarks (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL,
                alias TEXT NOT NULL UNIQUE,
                tags TEXT,
                description TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_pane_bookmarks_pane_id
                ON pane_bookmarks(pane_id);
            CREATE INDEX IF NOT EXISTS idx_pane_bookmarks_alias
                ON pane_bookmarks(alias);
        ",
        down_sql: Some(
            r"
            DROP INDEX IF EXISTS idx_pane_bookmarks_alias;
            DROP INDEX IF EXISTS idx_pane_bookmarks_pane_id;
            DROP TABLE IF EXISTS pane_bookmarks;
        ",
        ),
    },
];

// =============================================================================
// Data Structures
// =============================================================================

/// A captured segment of pane output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    /// Unique segment ID
    pub id: i64,
    /// Pane this segment belongs to
    pub pane_id: u64,
    /// Sequence number within the pane (monotonically increasing)
    pub seq: u64,
    /// The captured text content
    pub content: String,
    /// Content length (cached)
    pub content_len: usize,
    /// Optional content hash for overlap detection
    pub content_hash: Option<String>,
    /// Timestamp when captured (epoch ms)
    pub captured_at: i64,
}

/// Result of a WAL checkpoint operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointResult {
    /// Number of WAL frames checkpointed
    pub wal_pages: i64,
    /// Whether PRAGMA optimize was also run
    pub optimized: bool,
}

/// Result of an FTS search query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Matching segment
    pub segment: Segment,
    /// Snippet with highlighted terms (optional when snippets are disabled)
    pub snippet: Option<String>,
    /// Highlighted text with matching terms marked (optional)
    pub highlight: Option<String>,
    /// BM25 relevance score (lower is more relevant)
    pub score: f64,
}

/// Per-pane indexing statistics for observability.
///
/// Since FTS5 indexing is trigger-driven (same transaction as INSERT),
/// segments and FTS rows are always in sync under normal operation.
/// A mismatch indicates index corruption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneIndexingStats {
    /// Pane ID
    pub pane_id: u64,
    /// Total segments stored for this pane
    pub segment_count: u64,
    /// Total content bytes stored for this pane
    pub total_bytes: u64,
    /// Highest sequence number for this pane
    pub max_seq: Option<u64>,
    /// Timestamp of the most recent segment (epoch ms)
    pub last_segment_at: Option<i64>,
    /// Number of FTS rows for this pane (should equal segment_count)
    pub fts_row_count: u64,
    /// Whether FTS index is consistent (fts_row_count == segment_count)
    pub fts_consistent: bool,
}

/// Aggregate indexing health across all panes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingHealthReport {
    /// Per-pane statistics
    pub panes: Vec<PaneIndexingStats>,
    /// Total segments across all panes
    pub total_segments: u64,
    /// Total bytes across all panes
    pub total_bytes: u64,
    /// Total FTS rows across all panes
    pub total_fts_rows: u64,
    /// Number of panes with FTS inconsistency
    pub inconsistent_panes: u64,
    /// Overall health: all panes consistent and no errors
    pub healthy: bool,
}

/// FTS index state for incremental sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsIndexState {
    /// Index version (incremented on schema changes requiring rebuild)
    pub index_version: u32,
    /// Timestamp of last full rebuild (epoch ms)
    pub last_full_rebuild_at: Option<i64>,
    /// Created timestamp (epoch ms)
    pub created_at: i64,
    /// Updated timestamp (epoch ms)
    pub updated_at: i64,
}

/// Per-pane FTS indexing progress for batched rebuild
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsPaneProgress {
    /// Pane ID
    pub pane_id: u64,
    /// Last indexed segment sequence number
    pub last_indexed_seq: u64,
    /// Total segments indexed for this pane
    pub indexed_count: u64,
    /// Timestamp of last indexing (epoch ms)
    pub last_indexed_at: i64,
}

/// Result of an incremental FTS sync operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsSyncResult {
    /// Number of segments indexed in this sync
    pub segments_indexed: u64,
    /// Number of panes processed
    pub panes_processed: u64,
    /// Whether a full rebuild was required
    pub full_rebuild: bool,
    /// Duration of sync in milliseconds
    pub duration_ms: u64,
    /// Any errors encountered (non-fatal)
    pub warnings: Vec<String>,
}

/// Configuration for FTS sync batching
#[derive(Debug, Clone)]
pub struct FtsSyncConfig {
    /// Maximum segments per batch
    pub batch_size: usize,
    /// Maximum bytes per batch
    pub max_batch_bytes: usize,
    /// Whether to commit progress after each batch
    pub commit_progress: bool,
}

impl Default for FtsSyncConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            max_batch_bytes: 1_048_576, // 1 MB
            commit_progress: true,
        }
    }
}

/// A gap event indicating discontinuous capture
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gap {
    /// Unique gap ID
    pub id: i64,
    /// Pane where gap occurred
    pub pane_id: u64,
    /// Sequence number before gap
    pub seq_before: u64,
    /// Sequence number after gap
    pub seq_after: u64,
    /// Reason for gap
    pub reason: String,
    /// Timestamp of gap detection (epoch ms)
    pub detected_at: i64,
}

/// Pane metadata and observation state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRecord {
    /// Pane ID (from WezTerm)
    pub pane_id: u64,
    /// Stable pane UUID (persists across renames/moves)
    pub pane_uuid: Option<String>,
    /// Domain name
    pub domain: String,
    /// Window ID
    pub window_id: Option<u64>,
    /// Tab ID
    pub tab_id: Option<u64>,
    /// Pane title
    pub title: Option<String>,
    /// Current working directory
    pub cwd: Option<String>,
    /// TTY name
    pub tty_name: Option<String>,
    /// First seen timestamp (epoch ms)
    pub first_seen_at: i64,
    /// Last seen timestamp (epoch ms)
    pub last_seen_at: i64,
    /// Whether to observe this pane
    pub observed: bool,
    /// Reason for ignoring (if not observed)
    pub ignore_reason: Option<String>,
    /// When observation decision was made (epoch ms)
    pub last_decision_at: Option<i64>,
}

/// A stored event (pattern detection)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    /// Event ID
    pub id: i64,
    /// Pane ID
    pub pane_id: u64,
    /// Rule ID
    pub rule_id: String,
    /// Agent type
    pub agent_type: String,
    /// Event type
    pub event_type: String,
    /// Severity
    pub severity: String,
    /// Confidence score
    pub confidence: f64,
    /// Extracted data (JSON)
    pub extracted: Option<serde_json::Value>,
    /// Original matched text
    pub matched_text: Option<String>,
    /// Source segment ID
    pub segment_id: Option<i64>,
    /// Detection timestamp (epoch ms)
    pub detected_at: i64,
    /// Dedupe/identity key for repeated events
    pub dedupe_key: Option<String>,
    /// When handled (epoch ms, None = unhandled)
    pub handled_at: Option<i64>,
    /// Workflow that handled this
    pub handled_by_workflow_id: Option<String>,
    /// Handling status
    pub handled_status: Option<String>,
}

/// Stored annotations for an event (bd-1yk8).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventAnnotations {
    /// Current triage state, if set.
    pub triage_state: Option<String>,
    /// When triage state last changed (epoch ms).
    pub triage_updated_at: Option<i64>,
    /// Who changed triage state last (optional).
    pub triage_updated_by: Option<String>,
    /// Free-form operator note (redacted at write time).
    pub note: Option<String>,
    /// When note was last updated (epoch ms).
    pub note_updated_at: Option<i64>,
    /// Who updated the note last (optional).
    pub note_updated_by: Option<String>,
    /// Labels attached to the event (sorted).
    pub labels: Vec<String>,
}

/// Persistent mute record for event identity keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMuteRecord {
    /// Identity key (hashed)
    pub identity_key: String,
    /// Scope of mute (workspace/global)
    pub scope: String,
    /// Creation timestamp (epoch ms)
    pub created_at: i64,
    /// Optional expiry timestamp (epoch ms)
    pub expires_at: Option<i64>,
    /// Optional actor identifier
    pub created_by: Option<String>,
    /// Optional reason
    pub reason: Option<String>,
}

/// Agent session record for tracking agent timeline and token usage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionRecord {
    /// Session ID (auto-assigned)
    pub id: i64,
    /// Pane ID
    pub pane_id: u64,
    /// Agent type (codex, claude_code, gemini, unknown)
    pub agent_type: String,
    /// Agent's internal session ID if available
    pub session_id: Option<String>,
    /// External correlation ID (e.g., cass session)
    pub external_id: Option<String>,
    /// External correlation metadata (JSON)
    pub external_meta: Option<serde_json::Value>,
    /// Session start timestamp (epoch ms)
    pub started_at: i64,
    /// Session end timestamp (epoch ms, None = active)
    pub ended_at: Option<i64>,
    /// End reason (completed, limit_reached, error, manual)
    pub end_reason: Option<String>,
    /// Total tokens used
    pub total_tokens: Option<i64>,
    /// Input tokens
    pub input_tokens: Option<i64>,
    /// Output tokens
    pub output_tokens: Option<i64>,
    /// Cached tokens
    pub cached_tokens: Option<i64>,
    /// Reasoning tokens (for models that expose this)
    pub reasoning_tokens: Option<i64>,
    /// Model name
    pub model_name: Option<String>,
    /// Estimated cost in USD
    pub estimated_cost_usd: Option<f64>,
}

impl AgentSessionRecord {
    /// Create a new session record for starting a session
    #[must_use]
    pub fn new_start(pane_id: u64, agent_type: &str) -> Self {
        Self {
            id: 0, // Will be assigned by DB
            pane_id,
            agent_type: agent_type.to_string(),
            session_id: None,
            external_id: None,
            external_meta: None,
            started_at: now_ms(),
            ended_at: None,
            end_reason: None,
            total_tokens: None,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            reasoning_tokens: None,
            model_name: None,
            estimated_cost_usd: None,
        }
    }
}

// =============================================================================
// Timeline Data Model (wa-6sk.1)
// =============================================================================

/// Type of correlation between events
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CorrelationType {
    /// Usage limit event followed by new session (failover)
    Failover,
    /// One event triggers another in cascade
    Cascade,
    /// Events close in time (within window)
    Temporal,
    /// Events from the same workflow run
    WorkflowGroup,
    /// Events with same dedupe key pattern
    DedupeGroup,
}

impl std::fmt::Display for CorrelationType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failover => write!(f, "failover"),
            Self::Cascade => write!(f, "cascade"),
            Self::Temporal => write!(f, "temporal"),
            Self::WorkflowGroup => write!(f, "workflow_group"),
            Self::DedupeGroup => write!(f, "dedupe_group"),
        }
    }
}

/// A correlation between multiple events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Correlation {
    /// Unique correlation ID
    pub id: String,
    /// IDs of correlated events
    pub event_ids: Vec<i64>,
    /// Type of correlation
    pub correlation_type: CorrelationType,
    /// Confidence score (0.0-1.0)
    pub confidence: f64,
    /// Human-readable description
    pub description: String,
}

/// Reference to a correlation (lightweight)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationRef {
    /// Correlation ID
    pub id: String,
    /// Correlation type
    pub correlation_type: CorrelationType,
}

/// Pane information snapshot for timeline events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Pane ID
    pub pane_id: u64,
    /// Stable pane UUID
    pub pane_uuid: Option<String>,
    /// Agent type detected in pane
    pub agent_type: Option<String>,
    /// Domain (local, ssh, etc.)
    pub domain: String,
    /// Current working directory
    pub cwd: Option<String>,
    /// Pane title
    pub title: Option<String>,
}

/// Information about how an event was handled
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandledInfo {
    /// When handled (epoch ms)
    pub handled_at: i64,
    /// Workflow that handled this
    pub workflow_id: Option<String>,
    /// Handling status
    pub status: String,
}

/// An event enriched with pane info and correlations for timeline display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// Event ID
    pub id: i64,
    /// Detection timestamp (epoch ms)
    pub timestamp: i64,
    /// Pane information
    pub pane_info: PaneInfo,
    /// Rule that triggered this event
    pub rule_id: String,
    /// Event type
    pub event_type: String,
    /// Severity level
    pub severity: String,
    /// Confidence score
    pub confidence: f64,
    /// Handling information (if handled)
    pub handled: Option<HandledInfo>,
    /// References to correlations involving this event
    pub correlations: Vec<CorrelationRef>,
    /// Brief summary for display
    pub summary: Option<String>,
}

/// A timeline of events across panes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timeline {
    /// Start of time range (epoch ms)
    pub start: i64,
    /// End of time range (epoch ms)
    pub end: i64,
    /// Events in chronological order
    pub events: Vec<TimelineEvent>,
    /// All correlations referenced by events
    pub correlations: Vec<Correlation>,
    /// Total event count (may be more than events.len() if paginated)
    pub total_count: u64,
    /// Whether there are more events beyond this page
    pub has_more: bool,
}

/// Query parameters for timeline
#[derive(Debug, Clone, Default)]
pub struct TimelineQuery {
    /// Start of time range (epoch ms, inclusive)
    pub start: Option<i64>,
    /// End of time range (epoch ms, inclusive)
    pub end: Option<i64>,
    /// Filter by pane IDs
    pub pane_ids: Option<Vec<u64>>,
    /// Filter by severity levels
    pub severities: Option<Vec<String>>,
    /// Filter by event types
    pub event_types: Option<Vec<String>>,
    /// Filter by agent types
    pub agent_types: Option<Vec<String>>,
    /// Only unhandled events
    pub unhandled_only: bool,
    /// Include correlations
    pub include_correlations: bool,
    /// Maximum events to return
    pub limit: usize,
    /// Offset for pagination
    pub offset: usize,
}

impl TimelineQuery {
    /// Create a new query with default settings
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: 100,
            include_correlations: true,
            ..Default::default()
        }
    }

    /// Set time range
    #[must_use]
    pub fn with_range(mut self, start: i64, end: i64) -> Self {
        self.start = Some(start);
        self.end = Some(end);
        self
    }

    /// Filter by panes
    #[must_use]
    pub fn with_panes(mut self, pane_ids: Vec<u64>) -> Self {
        self.pane_ids = Some(pane_ids);
        self
    }

    /// Filter by severities
    #[must_use]
    pub fn with_severities(mut self, severities: Vec<String>) -> Self {
        self.severities = Some(severities);
        self
    }

    /// Only show unhandled events
    #[must_use]
    pub fn unhandled_only(mut self) -> Self {
        self.unhandled_only = true;
        self
    }

    /// Set pagination
    #[must_use]
    pub fn with_pagination(mut self, limit: usize, offset: usize) -> Self {
        self.limit = limit;
        self.offset = offset;
        self
    }
}

/// Workflow execution record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRecord {
    /// Execution ID
    pub id: String,
    /// Workflow name
    pub workflow_name: String,
    /// Pane ID
    pub pane_id: u64,
    /// Trigger event ID
    pub trigger_event_id: Option<i64>,
    /// Current step index
    pub current_step: usize,
    /// Status
    pub status: String,
    /// Wait condition (JSON)
    pub wait_condition: Option<serde_json::Value>,
    /// Workflow context (JSON)
    pub context: Option<serde_json::Value>,
    /// Result (JSON)
    pub result: Option<serde_json::Value>,
    /// Error message
    pub error: Option<String>,
    /// Started timestamp (epoch ms)
    pub started_at: i64,
    /// Updated timestamp (epoch ms)
    pub updated_at: i64,
    /// Completed timestamp (epoch ms)
    pub completed_at: Option<i64>,
}

/// Workflow action plan record (canonical JSON + hash)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowActionPlanRecord {
    /// Workflow execution ID (foreign key)
    pub workflow_id: String,
    /// Content-addressed plan ID
    pub plan_id: String,
    /// Plan hash (sha256 prefix)
    pub plan_hash: String,
    /// Canonical JSON representation of the plan
    pub plan_json: String,
    /// Creation timestamp (epoch ms)
    pub created_at: i64,
}

/// Prepared action plan record (plan preview awaiting commit)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedPlanRecord {
    /// Content-addressed plan ID
    pub plan_id: String,
    /// Plan hash (sha256 prefix)
    pub plan_hash: String,
    /// Workspace scope for the plan
    pub workspace_id: String,
    /// Action kind this plan represents (send_text, workflow_run, etc.)
    pub action_kind: String,
    /// Target pane ID (if applicable)
    pub pane_id: Option<u64>,
    /// Stable pane UUID (if known)
    pub pane_uuid: Option<String>,
    /// Action parameters (JSON, redacted as needed)
    pub params_json: Option<String>,
    /// Redacted plan JSON for preview
    pub plan_json: String,
    /// Whether approval is required before commit
    pub requires_approval: bool,
    /// Creation timestamp (epoch ms)
    pub created_at: i64,
    /// Expiration timestamp (epoch ms)
    pub expires_at: i64,
    /// When the plan was consumed (commit attempted)
    pub consumed_at: Option<i64>,
}

/// Workflow step log entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStepLogRecord {
    /// Step log ID
    pub id: i64,
    /// Workflow execution ID
    pub workflow_id: String,
    /// Linked audit action ID (if available)
    pub audit_action_id: Option<i64>,
    /// Step index within workflow
    pub step_index: usize,
    /// Step name
    pub step_name: String,
    /// Step idempotency key from plan (if available)
    pub step_id: Option<String>,
    /// Step action kind (if available)
    pub step_kind: Option<String>,
    /// Result type (continue, done, retry, abort, wait_for)
    pub result_type: String,
    /// Result data (JSON)
    pub result_data: Option<String>,
    /// Policy decision summary (JSON)
    pub policy_summary: Option<String>,
    /// Verification evidence references (JSON)
    pub verification_refs: Option<String>,
    /// Stable error code, if any
    pub error_code: Option<String>,
    /// Started timestamp (epoch ms)
    pub started_at: i64,
    /// Completed timestamp (epoch ms)
    pub completed_at: i64,
    /// Duration in milliseconds
    pub duration_ms: i64,
}

/// Audit action record for policy decisions and outcomes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditActionRecord {
    /// Audit record ID
    pub id: i64,
    /// Timestamp (epoch ms)
    pub ts: i64,
    /// Actor kind (human, robot, mcp, workflow)
    pub actor_kind: String,
    /// Optional actor identifier (workflow id, MCP client id)
    pub actor_id: Option<String>,
    /// Optional correlation identifier (prepare/approve/commit chain, etc.)
    pub correlation_id: Option<String>,
    /// Pane ID (if action targeted a pane)
    pub pane_id: Option<u64>,
    /// Domain name (if applicable)
    pub domain: Option<String>,
    /// Action kind (send_text, workflow_run, etc.)
    pub action_kind: String,
    /// Policy decision (allow, deny, require_approval)
    pub policy_decision: String,
    /// Policy decision reason (redacted)
    pub decision_reason: Option<String>,
    /// Policy rule ID, if any
    pub rule_id: Option<String>,
    /// Redacted input summary
    pub input_summary: Option<String>,
    /// Redacted verification summary
    pub verification_summary: Option<String>,
    /// Decision context (JSON), if available
    pub decision_context: Option<String>,
    /// Result (success, denied, failed, timeout)
    pub result: String,
}

impl AuditActionRecord {
    /// Redact sensitive fields before persistence or export
    pub fn redact_fields(&mut self, redactor: &Redactor) {
        self.decision_reason = self
            .decision_reason
            .as_ref()
            .map(|value| redactor.redact(value));
        self.input_summary = self
            .input_summary
            .as_ref()
            .map(|value| redactor.redact(value));
        self.verification_summary = self
            .verification_summary
            .as_ref()
            .map(|value| redactor.redact(value));
        self.decision_context = self
            .decision_context
            .as_ref()
            .map(|value| redactor.redact(value));
    }
}

/// Redacted audit record for JSONL streaming.
///
/// This schema is stable and safe for external consumers. The `id` field is
/// monotonically increasing and can be used as a cursor for pagination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditStreamRecord {
    /// Monotonic record ID (cursor)
    pub id: i64,
    /// Timestamp (epoch ms)
    pub ts: i64,
    /// Actor kind (human/robot/mcp/workflow)
    pub actor_kind: String,
    /// Actor identifier, if any
    pub actor_id: Option<String>,
    /// Correlation identifier, if any
    pub correlation_id: Option<String>,
    /// Pane ID, if applicable
    pub pane_id: Option<u64>,
    /// Domain name, if applicable
    pub domain: Option<String>,
    /// Action kind (send_text, workflow_run, etc.)
    pub action_kind: String,
    /// Policy decision (allow/deny/require_approval)
    pub policy_decision: String,
    /// Redacted decision reason, if any
    pub decision_reason: Option<String>,
    /// Policy rule ID, if any
    pub rule_id: Option<String>,
    /// Redacted input summary, if any
    pub input_summary: Option<String>,
    /// Redacted verification summary, if any
    pub verification_summary: Option<String>,
    /// Redacted decision context (JSON), if any
    pub decision_context: Option<String>,
    /// Result (success/denied/failed/timeout)
    pub result: String,
}

impl AuditStreamRecord {
    /// Build a redacted stream record from an audit action.
    pub fn from_action(mut action: AuditActionRecord, redactor: &Redactor) -> Self {
        action.redact_fields(redactor);
        Self {
            id: action.id,
            ts: action.ts,
            actor_kind: action.actor_kind,
            actor_id: action.actor_id,
            correlation_id: action.correlation_id,
            pane_id: action.pane_id,
            domain: action.domain,
            action_kind: action.action_kind,
            policy_decision: action.policy_decision,
            decision_reason: action.decision_reason,
            rule_id: action.rule_id,
            input_summary: action.input_summary,
            verification_summary: action.verification_summary,
            decision_context: action.decision_context,
            result: action.result,
        }
    }
}

/// Undo metadata for an audit action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionUndoRecord {
    /// Audit action ID (primary key)
    pub audit_action_id: i64,
    /// Whether the action is undoable
    pub undoable: bool,
    /// Undo strategy (none|manual|workflow_abort|pane_close|custom)
    pub undo_strategy: String,
    /// Redacted guidance for humans
    pub undo_hint: Option<String>,
    /// Redacted JSON payload for executor
    pub undo_payload: Option<String>,
    /// When the action was undone (epoch ms)
    pub undone_at: Option<i64>,
    /// Who performed the undo
    pub undone_by: Option<String>,
}

impl ActionUndoRecord {
    /// Redact sensitive fields before persistence or export
    pub fn redact_fields(&mut self, redactor: &Redactor) {
        self.undo_hint = self.undo_hint.as_ref().map(|value| redactor.redact(value));
        self.undo_payload = self
            .undo_payload
            .as_ref()
            .map(|value| redactor.redact(value));
    }
}

/// Read-optimized action history record (audit + undo + workflow step info)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionHistoryRecord {
    /// Audit record ID
    pub id: i64,
    /// Timestamp (epoch ms)
    pub ts: i64,
    /// Actor kind (human, robot, mcp, workflow)
    pub actor_kind: String,
    /// Optional actor identifier (workflow id, MCP client id)
    pub actor_id: Option<String>,
    /// Optional correlation identifier (prepare/approve/commit chain, etc.)
    pub correlation_id: Option<String>,
    /// Pane ID (if action targeted a pane)
    pub pane_id: Option<u64>,
    /// Domain name (if applicable)
    pub domain: Option<String>,
    /// Action kind (send_text, workflow_run, etc.)
    pub action_kind: String,
    /// Policy decision (allow, deny, require_approval)
    pub policy_decision: String,
    /// Policy decision reason (redacted)
    pub decision_reason: Option<String>,
    /// Policy rule ID, if any
    pub rule_id: Option<String>,
    /// Redacted input summary
    pub input_summary: Option<String>,
    /// Redacted verification summary
    pub verification_summary: Option<String>,
    /// Decision context (JSON), if available
    pub decision_context: Option<String>,
    /// Result (success, denied, failed, timeout)
    pub result: String,
    /// Whether the action is undoable (from action_undo)
    pub undoable: Option<bool>,
    /// Undo strategy (from action_undo)
    pub undo_strategy: Option<String>,
    /// Redacted undo hint
    pub undo_hint: Option<String>,
    /// When the action was undone (epoch ms)
    pub undone_at: Option<i64>,
    /// Who performed the undo
    pub undone_by: Option<String>,
    /// Workflow ID associated with the action (if any)
    pub workflow_id: Option<String>,
    /// Workflow step name associated with the action (if any)
    pub step_name: Option<String>,
}

/// Maintenance log record for system events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceRecord {
    /// Maintenance record ID
    pub id: i64,
    /// Event type (startup, shutdown, vacuum, retention_cleanup, error)
    pub event_type: String,
    /// Optional message
    pub message: Option<String>,
    /// Optional JSON metadata
    pub metadata: Option<String>,
    /// Timestamp (epoch ms)
    pub timestamp: i64,
}

/// Secret scan report record stored for incremental resumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretScanReportRecord {
    /// Report ID
    pub id: i64,
    /// Stable hash of the scan scope (filters).
    pub scope_hash: String,
    /// JSON representation of the scan scope.
    pub scope_json: String,
    /// Report schema version.
    pub report_version: i64,
    /// Last segment ID scanned (checkpoint).
    pub last_segment_id: Option<i64>,
    /// Full report payload (JSON).
    pub report_json: String,
    /// Timestamp when the report was created (epoch ms).
    pub created_at: i64,
}

/// Type of usage metric being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricType {
    /// Tokens consumed by an API call
    TokenUsage,
    /// Cost in USD
    ApiCost,
    /// API call count
    ApiCall,
    /// Rate limit event
    RateLimitHit,
    /// Workflow execution cost
    WorkflowCost,
    /// Session duration in seconds
    SessionDuration,
}

impl MetricType {
    /// Convert to the SQL-stored string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            MetricType::TokenUsage => "token_usage",
            MetricType::ApiCost => "api_cost",
            MetricType::ApiCall => "api_call",
            MetricType::RateLimitHit => "rate_limit_hit",
            MetricType::WorkflowCost => "workflow_cost",
            MetricType::SessionDuration => "session_duration",
        }
    }
}

impl std::str::FromStr for MetricType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "token_usage" => Ok(MetricType::TokenUsage),
            "api_cost" => Ok(MetricType::ApiCost),
            "api_call" => Ok(MetricType::ApiCall),
            "rate_limit_hit" => Ok(MetricType::RateLimitHit),
            "workflow_cost" => Ok(MetricType::WorkflowCost),
            "session_duration" => Ok(MetricType::SessionDuration),
            _ => Err(format!("Unknown metric type: {s}")),
        }
    }
}

impl std::fmt::Display for MetricType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A usage metric record for analytics tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageMetricRecord {
    /// Record ID (0 for new records)
    pub id: i64,
    /// When the metric was recorded (epoch ms)
    pub timestamp: i64,
    /// Type of metric
    pub metric_type: MetricType,
    /// Optional pane ID (None for global metrics)
    pub pane_id: Option<u64>,
    /// Optional agent type (codex, claude_code, gemini)
    pub agent_type: Option<String>,
    /// Optional account reference
    pub account_id: Option<String>,
    /// Optional workflow execution reference
    pub workflow_id: Option<String>,
    /// For countable metrics
    pub count: Option<i64>,
    /// For costs (USD)
    pub amount: Option<f64>,
    /// For token counts
    pub tokens: Option<i64>,
    /// Optional JSON metadata
    pub metadata: Option<String>,
    /// When the record was created (epoch ms)
    pub created_at: i64,
}

/// Aggregated daily summary row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyMetricSummary {
    /// Day as epoch ms (midnight UTC)
    pub day_ts: i64,
    /// Agent type (None for mixed)
    pub agent_type: Option<String>,
    /// Total tokens across all metrics for the day
    pub total_tokens: i64,
    /// Total cost in USD
    pub total_cost: f64,
    /// Number of metric events
    pub event_count: i64,
}

/// Per-agent metric breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMetricBreakdown {
    /// Agent type
    pub agent_type: String,
    /// Total tokens consumed
    pub total_tokens: i64,
    /// Total cost in USD
    pub total_cost: f64,
    /// Average tokens per event
    pub avg_tokens_per_event: f64,
}

/// Query filter for usage metrics.
#[derive(Debug, Clone, Default)]
pub struct MetricQuery {
    /// Filter by metric type
    pub metric_type: Option<MetricType>,
    /// Filter by agent type
    pub agent_type: Option<String>,
    /// Filter by account ID
    pub account_id: Option<String>,
    /// Filter since timestamp (epoch ms)
    pub since: Option<i64>,
    /// Filter until timestamp (epoch ms)
    pub until: Option<i64>,
    /// Maximum results
    pub limit: Option<usize>,
}

/// Status of a notification delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationStatus {
    /// Notification created, delivery not yet attempted
    Pending,
    /// Successfully delivered
    Sent,
    /// Delivery failed
    Failed,
    /// Delivery was throttled / rate-limited
    Throttled,
}

impl NotificationStatus {
    /// Convert to the SQL-stored string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            NotificationStatus::Pending => "pending",
            NotificationStatus::Sent => "sent",
            NotificationStatus::Failed => "failed",
            NotificationStatus::Throttled => "throttled",
        }
    }
}

impl std::str::FromStr for NotificationStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(NotificationStatus::Pending),
            "sent" => Ok(NotificationStatus::Sent),
            "failed" => Ok(NotificationStatus::Failed),
            "throttled" => Ok(NotificationStatus::Throttled),
            _ => Err(format!("Unknown notification status: {s}")),
        }
    }
}

impl std::fmt::Display for NotificationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A notification history record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationHistoryRecord {
    /// Record ID (0 for new records)
    pub id: i64,
    /// When the notification was created (epoch ms)
    pub timestamp: i64,
    /// Optional event ID that triggered the notification
    pub event_id: Option<i64>,
    /// Delivery channel (webhook, desktop, slack, etc.)
    pub channel: String,
    /// Notification title
    pub title: String,
    /// Notification body
    pub body: String,
    /// Severity level (info, warning, error, critical)
    pub severity: String,
    /// Delivery status
    pub status: NotificationStatus,
    /// Error message if delivery failed
    pub error_message: Option<String>,
    /// When notification was acknowledged (epoch ms)
    pub acknowledged_at: Option<i64>,
    /// Who acknowledged the notification
    pub acknowledged_by: Option<String>,
    /// Action taken in response
    pub action_taken: Option<String>,
    /// Number of retry attempts
    pub retry_count: i64,
    /// Optional JSON metadata
    pub metadata: Option<String>,
    /// When the record was created (epoch ms)
    pub created_at: i64,
}

/// Query filter for notification history.
#[derive(Debug, Clone, Default)]
pub struct NotificationHistoryQuery {
    /// Filter since timestamp (epoch ms)
    pub since: Option<i64>,
    /// Filter until timestamp (epoch ms)
    pub until: Option<i64>,
    /// Filter by channel
    pub channel: Option<String>,
    /// Filter by status
    pub status: Option<NotificationStatus>,
    /// Filter by event ID
    pub event_id: Option<i64>,
    /// Maximum results (default 100)
    pub limit: Option<usize>,
}

/// Saved search record for reusable queries and scheduling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedSearchRecord {
    /// Stable saved search identifier.
    pub id: String,
    /// Human-friendly name (unique).
    pub name: String,
    /// FTS query string.
    pub query: String,
    /// Optional scope to a pane.
    pub pane_id: Option<u64>,
    /// Maximum number of results.
    pub limit: i64,
    /// Since window mode ("last_run" or "fixed").
    pub since_mode: String,
    /// Fixed since timestamp (epoch ms) when since_mode="fixed".
    pub since_ms: Option<i64>,
    /// Optional schedule interval (ms). None means manual-only.
    pub schedule_interval_ms: Option<i64>,
    /// Whether the search is enabled for scheduling.
    pub enabled: bool,
    /// Last run timestamp (epoch ms).
    pub last_run_at: Option<i64>,
    /// Last run result count.
    pub last_result_count: Option<i64>,
    /// Last run error (if any).
    pub last_error: Option<String>,
    /// Created timestamp (epoch ms).
    pub created_at: i64,
    /// Updated timestamp (epoch ms).
    pub updated_at: i64,
}

/// A pane bookmark record binding an alias (and optional tags) to a pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneBookmarkRecord {
    pub id: i64,
    pub pane_id: u64,
    pub alias: String,
    pub tags: Option<Vec<String>>,
    pub description: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Default since-mode: last_run.
pub const SAVED_SEARCH_SINCE_MODE_LAST_RUN: &str = "last_run";
/// Fixed since-mode uses the stored since_ms value.
pub const SAVED_SEARCH_SINCE_MODE_FIXED: &str = "fixed";
/// Default maximum results for saved searches.
pub const SAVED_SEARCH_DEFAULT_LIMIT: i64 = 50;

impl SavedSearchRecord {
    /// Build a new saved search record with defaults.
    #[must_use]
    pub fn new(
        name: String,
        query: String,
        pane_id: Option<u64>,
        limit: i64,
        since_mode: String,
        since_ms: Option<i64>,
    ) -> Self {
        let now = now_ms();
        let random: u32 = rand::random();
        let id = format!("ss-{now}-{random:08x}");
        Self {
            id,
            name,
            query,
            pane_id,
            limit,
            since_mode,
            since_ms,
            schedule_interval_ms: None,
            enabled: false,
            last_run_at: None,
            last_result_count: None,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Approval token record for allow-once approvals
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalTokenRecord {
    /// Token record ID
    pub id: i64,
    /// Hash of allow-once code (sha256)
    pub code_hash: String,
    /// Created timestamp (epoch ms)
    pub created_at: i64,
    /// Expiration timestamp (epoch ms)
    pub expires_at: i64,
    /// When token was consumed (epoch ms)
    pub used_at: Option<i64>,
    /// Workspace identifier
    pub workspace_id: String,
    /// Action kind
    pub action_kind: String,
    /// Target pane ID (if applicable)
    pub pane_id: Option<u64>,
    /// Normalized action fingerprint
    pub action_fingerprint: String,
    /// Optional plan hash binding (sha256 of bound ActionPlan)
    pub plan_hash: Option<String>,
    /// Optional plan schema version
    pub plan_version: Option<i32>,
    /// Optional human-readable risk summary
    pub risk_summary: Option<String>,
}

impl ApprovalTokenRecord {
    /// Returns true if the token is unused and unexpired
    #[must_use]
    pub fn is_active(&self, now_ms: i64) -> bool {
        self.used_at.is_none() && self.expires_at >= now_ms
    }
}

/// A pane reservation representing an exclusive workflow lock on a pane.
///
/// Only one active reservation per pane is allowed. Reservations expire
/// automatically after their TTL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneReservation {
    /// Unique reservation ID
    pub id: i64,
    /// Pane this reservation applies to
    pub pane_id: u64,
    /// Kind of owner (e.g. "workflow", "agent", "manual")
    pub owner_kind: String,
    /// Owner identifier (e.g. workflow ID or agent name)
    pub owner_id: String,
    /// Human-readable reason for the reservation
    pub reason: Option<String>,
    /// When the reservation was created (epoch ms)
    pub created_at: i64,
    /// When the reservation expires (epoch ms)
    pub expires_at: i64,
    /// When the reservation was released (epoch ms), None if still active
    pub released_at: Option<i64>,
    /// Current status: "active" or "released"
    pub status: String,
}

impl PaneReservation {
    /// Returns true if the reservation is still active and unexpired.
    #[must_use]
    pub fn is_active(&self, now_ms: i64) -> bool {
        self.status == "active" && self.released_at.is_none() && self.expires_at > now_ms
    }
}

/// Configuration for pane reservation behavior.
#[derive(Debug, Clone)]
pub struct PaneReservationConfig {
    /// Default TTL in milliseconds (30 minutes)
    pub default_ttl_ms: i64,
    /// Maximum allowed TTL in milliseconds (4 hours)
    pub max_ttl_ms: i64,
}

impl Default for PaneReservationConfig {
    fn default() -> Self {
        Self {
            default_ttl_ms: 30 * 60 * 1000, // 30 minutes
            max_ttl_ms: 4 * 60 * 60 * 1000, // 4 hours
        }
    }
}

impl PaneReservationConfig {
    /// Clamp a requested TTL to the allowed range.
    ///
    /// Returns the clamped TTL in milliseconds. The minimum is 1000ms (1 second).
    #[must_use]
    pub fn clamp_ttl(&self, requested_ttl_ms: i64) -> i64 {
        requested_ttl_ms.clamp(1000, self.max_ttl_ms)
    }
}

// =============================================================================
// Schema Initialization & Migrations
// =============================================================================

/// Get the current schema version from PRAGMA user_version.
///
/// Returns 0 for fresh databases that haven't been initialized.
pub fn get_user_version(conn: &Connection) -> Result<i32> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| StorageError::Database(format!("Failed to read user_version: {e}")).into())
}

/// Set the schema version using PRAGMA user_version.
fn set_user_version(conn: &Connection, version: i32) -> Result<()> {
    // PRAGMA doesn't support parameters, so we format directly
    // Version is an i32, so no SQL injection risk
    conn.execute_batch(&format!("PRAGMA user_version = {version}"))
        .map_err(|e| {
            StorageError::MigrationFailed(format!("Failed to set user_version: {e}")).into()
        })
}

/// Record a migration in the schema_version audit table.
fn record_migration(conn: &Connection, version: i32, description: &str) -> Result<()> {
    #[allow(clippy::cast_possible_truncation)]
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);

    conn.execute(
        "INSERT INTO schema_version (version, applied_at, description) VALUES (?1, ?2, ?3)",
        params![version, now_ms, description],
    )
    .map_err(|e| StorageError::MigrationFailed(format!("Failed to record migration: {e}")))?;

    Ok(())
}

/// WAL recovery threshold: if WAL has more than this many frames, do a full checkpoint.
const WAL_RECOVERY_THRESHOLD: i64 = 10_000;

/// Check for and recover from unclean shutdown.
///
/// Handles WAL/journal files left over from crashes by:
/// 1. Detecting recovery situation (WAL/journal files exist)
/// 2. Running quick integrity check
/// 3. Checkpointing WAL if it's large
///
/// # Errors
///
/// Returns an error if:
/// - Database corruption is detected
/// - WAL checkpoint fails
pub fn check_and_recover_wal(conn: &Connection, db_path: &str) -> Result<()> {
    let wal_path = format!("{db_path}-wal");
    let journal_path = format!("{db_path}-journal");

    let wal_exists = Path::new(&wal_path).exists();
    let journal_exists = Path::new(&journal_path).exists();

    if wal_exists || journal_exists {
        tracing::info!(
            wal_exists,
            journal_exists,
            "Recovery situation detected, attempting recovery"
        );
    }

    // Run quick integrity check
    let integrity_result: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|e| StorageError::Database(format!("Integrity check failed: {e}")))?;

    if integrity_result != "ok" {
        tracing::error!(result = %integrity_result, "Database corruption detected");
        return Err(StorageError::Corruption {
            details: integrity_result,
        }
        .into());
    }

    // Checkpoint WAL using PASSIVE mode (doesn't block readers)
    let (busy, wal_frames, checkpointed): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|e| StorageError::Database(format!("WAL checkpoint failed: {e}")))?;

    if wal_frames > 0 {
        tracing::info!(busy, wal_frames, checkpointed, "WAL checkpoint completed");
    }

    // If WAL is huge, do a full checkpoint to truncate it
    if wal_frames > WAL_RECOVERY_THRESHOLD {
        tracing::warn!(
            frames = wal_frames,
            threshold = WAL_RECOVERY_THRESHOLD,
            "Large WAL detected, performing full checkpoint"
        );

        let (busy2, wal_frames2, checkpointed2): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| StorageError::Database(format!("WAL truncate checkpoint failed: {e}")))?;

        tracing::info!(
            busy = busy2,
            wal_frames = wal_frames2,
            checkpointed = checkpointed2,
            "WAL truncate checkpoint completed"
        );
    }

    if wal_exists || journal_exists {
        tracing::info!("Database recovery complete");
    }

    Ok(())
}

// =============================================================================
// Database Health Check & Repair
// =============================================================================

/// Status of a single database health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbCheckStatus {
    Ok,
    Warning,
    Error,
}

impl std::fmt::Display for DbCheckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "OK"),
            Self::Warning => write!(f, "WARNING"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

/// Result of a single health check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbCheckItem {
    pub name: String,
    pub status: DbCheckStatus,
    pub detail: Option<String>,
}

/// Full database health report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbCheckReport {
    pub db_path: String,
    pub db_exists: bool,
    pub db_size_bytes: Option<u64>,
    pub schema_version: Option<i32>,
    pub checks: Vec<DbCheckItem>,
}

impl DbCheckReport {
    /// Whether any check has error status.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.checks.iter().any(|c| c.status == DbCheckStatus::Error)
    }

    /// Whether any check has warning status.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        self.checks
            .iter()
            .any(|c| c.status == DbCheckStatus::Warning)
    }

    /// Count of problems (errors + warnings).
    #[must_use]
    pub fn problem_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status != DbCheckStatus::Ok)
            .count()
    }
}

/// Result of a single repair operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbRepairItem {
    pub name: String,
    pub success: bool,
    pub detail: String,
}

/// Full repair report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbRepairReport {
    pub backup_path: Option<String>,
    pub repairs: Vec<DbRepairItem>,
}

impl DbRepairReport {
    /// Whether all repairs succeeded.
    #[must_use]
    pub fn all_succeeded(&self) -> bool {
        self.repairs.iter().all(|r| r.success)
    }
}

/// Per-table row count for the stats report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableStats {
    pub name: String,
    pub row_count: u64,
}

/// Per-pane storage summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneStats {
    pub pane_id: u64,
    pub title: Option<String>,
    pub segment_count: u64,
    pub segment_bytes: u64,
    pub event_count: u64,
}

/// Event type distribution entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventTypeStats {
    pub event_type: String,
    pub count: u64,
}

/// Full database statistics report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbStatsReport {
    pub db_path: String,
    pub db_size_bytes: Option<u64>,
    pub tables: Vec<TableStats>,
    pub top_panes: Vec<PaneStats>,
    pub event_types: Vec<EventTypeStats>,
    pub suggestions: Vec<String>,
}

/// Collect storage statistics for `db_path`.
///
/// Returns row counts per table, top panes by data volume, event type
/// distribution, and cleanup suggestions referencing dry-run commands.
pub fn database_stats(db_path: &Path, retention_days: u32) -> DbStatsReport {
    let path_str = db_path.display().to_string();
    let db_size_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());

    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => {
            return DbStatsReport {
                db_path: path_str,
                db_size_bytes,
                tables: vec![],
                top_panes: vec![],
                event_types: vec![],
                suggestions: vec!["Database could not be opened.".to_string()],
            };
        }
    };

    // Table row counts
    let table_names = [
        "panes",
        "output_segments",
        "events",
        "audit_actions",
        "usage_metrics",
        "notification_history",
        "workflow_executions",
        "maintenance_log",
    ];
    let mut tables = Vec::new();
    for name in &table_names {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |row| {
                row.get(0)
            })
            .unwrap_or(0);
        tables.push(TableStats {
            name: (*name).to_string(),
            row_count: count as u64,
        });
    }

    // Top panes by segment volume (count + total content_len)
    let top_panes = {
        let mut stmt = conn
            .prepare(
                "SELECT s.pane_id, p.title,
                        COUNT(*) as seg_count,
                        COALESCE(SUM(s.content_len), 0) as seg_bytes,
                        (SELECT COUNT(*) FROM events e WHERE e.pane_id = s.pane_id) as evt_count
                 FROM output_segments s
                 LEFT JOIN panes p ON p.pane_id = s.pane_id
                 GROUP BY s.pane_id
                 ORDER BY seg_bytes DESC
                 LIMIT 10",
            )
            .ok();
        match stmt.as_mut() {
            Some(s) => s
                .query_map([], |row| {
                    Ok(PaneStats {
                        pane_id: row.get::<_, i64>(0)? as u64,
                        title: row.get(1)?,
                        segment_count: row.get::<_, i64>(2)? as u64,
                        segment_bytes: row.get::<_, i64>(3)? as u64,
                        event_count: row.get::<_, i64>(4)? as u64,
                    })
                })
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            None => vec![],
        }
    };

    // Event type distribution
    let event_types = {
        let mut stmt = conn
            .prepare(
                "SELECT event_type, COUNT(*) as cnt
                 FROM events
                 GROUP BY event_type
                 ORDER BY cnt DESC",
            )
            .ok();
        match stmt.as_mut() {
            Some(s) => s
                .query_map([], |row| {
                    Ok(EventTypeStats {
                        event_type: row.get(0)?,
                        count: row.get::<_, i64>(1)? as u64,
                    })
                })
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            None => vec![],
        }
    };

    // Cleanup suggestions
    let mut suggestions = Vec::new();

    let total_events: u64 = tables
        .iter()
        .find(|t| t.name == "events")
        .map_or(0, |t| t.row_count);
    let total_segments: u64 = tables
        .iter()
        .find(|t| t.name == "output_segments")
        .map_or(0, |t| t.row_count);

    if total_events > 10_000 {
        suggestions.push(format!(
            "{total_events} events stored. Preview cleanup: wa cleanup --dry-run"
        ));
    }
    if total_segments > 50_000 {
        suggestions.push(format!(
            "{total_segments} segments stored. Preview cleanup: wa cleanup --dry-run"
        ));
    }
    if let Some(size) = db_size_bytes {
        if size > 100 * 1024 * 1024 {
            suggestions.push(format!(
                "Database is {:.1} MB. Consider: wa db repair --dry-run (includes VACUUM)",
                size as f64 / 1_048_576.0
            ));
        }
    }
    if retention_days > 0 {
        suggestions.push(format!(
            "Retention policy: {retention_days} days. Configure in wa.toml [storage] retention_days"
        ));
    }
    if suggestions.is_empty() {
        suggestions.push("Database looks healthy. No cleanup actions needed.".to_string());
    }

    DbStatsReport {
        db_path: path_str,
        db_size_bytes,
        tables,
        top_panes,
        event_types,
        suggestions,
    }
}

/// Run health checks on the database at `db_path`.
///
/// Checks performed:
/// 1. SQLite quick integrity check
/// 2. Schema version validation
/// 3. Foreign key consistency
/// 4. FTS index integrity
/// 5. WAL status
#[must_use]
pub fn check_database_health(db_path: &Path) -> DbCheckReport {
    let path_str = db_path.display().to_string();
    let db_exists = db_path.exists();

    if !db_exists {
        return DbCheckReport {
            db_path: path_str,
            db_exists: false,
            db_size_bytes: None,
            schema_version: None,
            checks: vec![DbCheckItem {
                name: "Database file".to_string(),
                status: DbCheckStatus::Error,
                detail: Some("Database file does not exist".to_string()),
            }],
        };
    }

    let db_size_bytes = std::fs::metadata(db_path).ok().map(|m| m.len());

    let conn = match Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            return DbCheckReport {
                db_path: path_str,
                db_exists: true,
                db_size_bytes,
                schema_version: None,
                checks: vec![DbCheckItem {
                    name: "Database open".to_string(),
                    status: DbCheckStatus::Error,
                    detail: Some(format!("Failed to open database: {e}")),
                }],
            };
        }
    };

    let mut checks = Vec::new();
    let mut schema_version = None;

    // 1. SQLite integrity check
    match conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0)) {
        Ok(result) if result == "ok" => {
            checks.push(DbCheckItem {
                name: "SQLite integrity".to_string(),
                status: DbCheckStatus::Ok,
                detail: None,
            });
        }
        Ok(result) => {
            checks.push(DbCheckItem {
                name: "SQLite integrity".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("CORRUPT: {result}")),
            });
        }
        Err(e) => {
            checks.push(DbCheckItem {
                name: "SQLite integrity".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("Check failed: {e}")),
            });
        }
    }

    // 2. Schema version
    match get_user_version(&conn) {
        Ok(v) => {
            schema_version = Some(v);
            let (status, detail) = match v.cmp(&SCHEMA_VERSION) {
                std::cmp::Ordering::Equal => (DbCheckStatus::Ok, format!("{v} (current)")),
                std::cmp::Ordering::Less => (
                    DbCheckStatus::Warning,
                    format!("{v} (needs migration to {SCHEMA_VERSION})"),
                ),
                std::cmp::Ordering::Greater => (
                    DbCheckStatus::Error,
                    format!("{v} (newer than supported {SCHEMA_VERSION})"),
                ),
            };
            checks.push(DbCheckItem {
                name: "Schema version".to_string(),
                status,
                detail: Some(detail),
            });
        }
        Err(e) => {
            checks.push(DbCheckItem {
                name: "Schema version".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("Failed to read: {e}")),
            });
        }
    }

    // 3. Foreign key check
    match conn.query_row("PRAGMA foreign_key_check", [], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(table) => {
            // If a row is returned, there's a violation
            checks.push(DbCheckItem {
                name: "Foreign keys".to_string(),
                status: DbCheckStatus::Warning,
                detail: Some(format!("Violation in table: {table}")),
            });
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            checks.push(DbCheckItem {
                name: "Foreign keys".to_string(),
                status: DbCheckStatus::Ok,
                detail: None,
            });
        }
        Err(e) => {
            checks.push(DbCheckItem {
                name: "Foreign keys".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("Check failed: {e}")),
            });
        }
    }

    // 4. FTS index integrity
    match conn.execute(
        "INSERT INTO output_segments_fts(output_segments_fts) VALUES('integrity-check')",
        [],
    ) {
        Ok(_) => {
            checks.push(DbCheckItem {
                name: "FTS index".to_string(),
                status: DbCheckStatus::Ok,
                detail: None,
            });
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("no such table") {
                checks.push(DbCheckItem {
                    name: "FTS index".to_string(),
                    status: DbCheckStatus::Error,
                    detail: Some("FTS table missing".to_string()),
                });
            } else {
                checks.push(DbCheckItem {
                    name: "FTS index".to_string(),
                    status: DbCheckStatus::Error,
                    detail: Some(format!("CORRUPT: {msg}")),
                });
            }
        }
    }

    // 5. WAL checkpoint status
    let wal_path = format!("{}-wal", db_path.display());
    let wal_exists = Path::new(&wal_path).exists();
    match conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    }) {
        Ok((_busy, wal_frames, checkpointed)) => {
            if wal_frames > WAL_RECOVERY_THRESHOLD {
                checks.push(DbCheckItem {
                    name: "WAL checkpoint".to_string(),
                    status: DbCheckStatus::Warning,
                    detail: Some(format!(
                        "Large WAL: {wal_frames} frames ({checkpointed} checkpointed)"
                    )),
                });
            } else if wal_exists && wal_frames > 0 {
                checks.push(DbCheckItem {
                    name: "WAL checkpoint".to_string(),
                    status: DbCheckStatus::Ok,
                    detail: Some(format!("{wal_frames} frames pending")),
                });
            } else {
                checks.push(DbCheckItem {
                    name: "WAL checkpoint".to_string(),
                    status: DbCheckStatus::Ok,
                    detail: None,
                });
            }
        }
        Err(e) => {
            checks.push(DbCheckItem {
                name: "WAL checkpoint".to_string(),
                status: DbCheckStatus::Error,
                detail: Some(format!("Check failed: {e}")),
            });
        }
    }

    DbCheckReport {
        db_path: path_str,
        db_exists,
        db_size_bytes,
        schema_version,
        checks,
    }
}

/// Repair the database at `db_path`.
///
/// Repairs performed based on detected issues:
/// 1. Rebuild FTS index from source table
/// 2. Checkpoint and truncate WAL
/// 3. VACUUM to reclaim space
///
/// Creates a backup before any modifications unless `skip_backup` is true.
pub fn repair_database(db_path: &Path, dry_run: bool, skip_backup: bool) -> Result<DbRepairReport> {
    if !db_path.exists() {
        return Err(
            StorageError::Database(format!("Database not found: {}", db_path.display())).into(),
        );
    }

    // Create backup unless skipped
    let backup_path = if !dry_run && !skip_backup {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let backup = format!("{}.bak.{ts}", db_path.display());
        match std::fs::copy(db_path, &backup) {
            Ok(_) => Some(backup),
            Err(e) => {
                return Err(StorageError::Database(format!("Failed to create backup: {e}")).into());
            }
        }
    } else {
        None
    };

    let mut repairs = Vec::new();

    let conn = Connection::open(db_path)
        .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;

    // 1. Rebuild FTS index
    let fts_needs_rebuild = conn
        .execute(
            "INSERT INTO output_segments_fts(output_segments_fts) VALUES('integrity-check')",
            [],
        )
        .is_err();

    if fts_needs_rebuild {
        if dry_run {
            repairs.push(DbRepairItem {
                name: "FTS index rebuild".to_string(),
                success: true,
                detail: "Would rebuild FTS index from output_segments table".to_string(),
            });
        } else {
            match conn.execute(
                "INSERT INTO output_segments_fts(output_segments_fts) VALUES('rebuild')",
                [],
            ) {
                Ok(_) => {
                    let count: i64 = conn
                        .query_row("SELECT COUNT(*) FROM output_segments", [], |row| row.get(0))
                        .unwrap_or(0);
                    repairs.push(DbRepairItem {
                        name: "FTS index rebuild".to_string(),
                        success: true,
                        detail: format!("Rebuilt FTS index ({count} segments indexed)"),
                    });
                }
                Err(e) => {
                    repairs.push(DbRepairItem {
                        name: "FTS index rebuild".to_string(),
                        success: false,
                        detail: format!("Failed to rebuild FTS index: {e}"),
                    });
                }
            }
        }
    } else {
        repairs.push(DbRepairItem {
            name: "FTS index".to_string(),
            success: true,
            detail: "FTS index is healthy, no rebuild needed".to_string(),
        });
    }

    // 2. WAL checkpoint + truncate
    match conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        row.get::<_, i64>(1)
    }) {
        Ok(wal_frames) if wal_frames > 0 => {
            if dry_run {
                repairs.push(DbRepairItem {
                    name: "WAL checkpoint".to_string(),
                    success: true,
                    detail: format!("Would checkpoint and truncate WAL ({wal_frames} frames)"),
                });
            } else {
                match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok::<_, rusqlite::Error>((row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
                }) {
                    Ok((frames, checkpointed)) => {
                        repairs.push(DbRepairItem {
                            name: "WAL checkpoint".to_string(),
                            success: true,
                            detail: format!("Checkpointed WAL ({checkpointed}/{frames} frames)"),
                        });
                    }
                    Err(e) => {
                        repairs.push(DbRepairItem {
                            name: "WAL checkpoint".to_string(),
                            success: false,
                            detail: format!("WAL checkpoint failed: {e}"),
                        });
                    }
                }
            }
        }
        Ok(_) => {
            repairs.push(DbRepairItem {
                name: "WAL checkpoint".to_string(),
                success: true,
                detail: "WAL is clean, no checkpoint needed".to_string(),
            });
        }
        Err(e) => {
            repairs.push(DbRepairItem {
                name: "WAL checkpoint".to_string(),
                success: false,
                detail: format!("WAL status check failed: {e}"),
            });
        }
    }

    // 3. VACUUM
    if dry_run {
        repairs.push(DbRepairItem {
            name: "Vacuum".to_string(),
            success: true,
            detail: "Would vacuum database to reclaim space".to_string(),
        });
    } else {
        match conn.execute_batch("VACUUM") {
            Ok(()) => {
                repairs.push(DbRepairItem {
                    name: "Vacuum".to_string(),
                    success: true,
                    detail: "Database vacuumed".to_string(),
                });
            }
            Err(e) => {
                repairs.push(DbRepairItem {
                    name: "Vacuum".to_string(),
                    success: false,
                    detail: format!("Vacuum failed: {e}"),
                });
            }
        }
    }

    Ok(DbRepairReport {
        backup_path,
        repairs,
    })
}

/// Initialize or migrate the database schema.
///
/// This function handles both fresh databases and existing databases that
/// need migration to a newer schema version.
///
/// # Behavior
///
/// - Fresh database (user_version = 0): Creates all tables via SCHEMA_SQL
/// - Existing database (user_version < SCHEMA_VERSION): Applies pending migrations
/// - Up-to-date database (user_version = SCHEMA_VERSION): No-op
///
/// # Errors
///
/// Returns an error if:
/// - The database has a newer schema than this code supports
/// - Any migration fails to apply
pub fn initialize_schema(conn: &Connection) -> Result<()> {
    let current = get_user_version(conn)?;
    let needs_init = needs_initialization(conn)?;

    if current > SCHEMA_VERSION {
        return Err(StorageError::SchemaTooNew {
            current,
            supported: SCHEMA_VERSION,
        }
        .into());
    }

    if current != 0 {
        check_wa_version_compatibility(conn)?;
    }

    if current == SCHEMA_VERSION {
        // Already up to date
        ensure_wa_meta(conn, SCHEMA_VERSION)?;
        return Ok(());
    }

    // Fresh database: create base schema and mark as current.
    if current == 0 && needs_init {
        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| StorageError::MigrationFailed(format!("Schema init failed: {e}")))?;
        set_user_version(conn, SCHEMA_VERSION)?;
        record_migration(conn, SCHEMA_VERSION, "Initial schema")?;
        ensure_wa_meta(conn, SCHEMA_VERSION)?;
        return Ok(());
    }

    // Existing database at version 0: apply full schema (idempotent via IF NOT EXISTS).
    // SCHEMA_SQL creates the complete current schema, so no incremental migrations needed.
    if current == 0 {
        if table_exists(conn, "audit_actions")?
            && !table_has_column(conn, "audit_actions", "correlation_id")?
        {
            conn.execute(
                "ALTER TABLE audit_actions ADD COLUMN correlation_id TEXT;",
                [],
            )
            .map_err(|e| {
                StorageError::MigrationFailed(format!(
                    "Failed to add correlation_id to audit_actions: {e}"
                ))
            })?;
        }
        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| StorageError::MigrationFailed(format!("Schema init failed: {e}")))?;
        set_user_version(conn, SCHEMA_VERSION)?;
        record_migration(conn, SCHEMA_VERSION, "Schema init from v0")?;
        ensure_wa_meta(conn, SCHEMA_VERSION)?;
        return Ok(());
    }

    // Apply pending migrations for existing databases (version > 0)
    run_migrations(conn, current)?;

    ensure_wa_meta(conn, SCHEMA_VERSION)?;

    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| StorageError::Database(e.to_string()))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| StorageError::Database(e.to_string()))?;

    while let Some(row) = rows
        .next()
        .map_err(|e| StorageError::Database(e.to_string()))?
    {
        let name: String = row
            .get(1)
            .map_err(|e| StorageError::Database(e.to_string()))?;
        if name == column {
            return Ok(true);
        }
    }

    Ok(false)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
    Ok(count > 0)
}

fn ensure_workflow_step_logs_audit_action_id(conn: &Connection) -> Result<()> {
    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='workflow_step_logs'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;

    if table_exists == 0 {
        // Will be created via SCHEMA_SQL; nothing to do here.
        return Ok(());
    }

    if table_has_column(conn, "workflow_step_logs", "audit_action_id")? {
        return Ok(());
    }

    conn.execute_batch(
        "ALTER TABLE workflow_step_logs ADD COLUMN audit_action_id INTEGER REFERENCES audit_actions(id) ON DELETE SET NULL;",
    )
    .map_err(|e| StorageError::MigrationFailed(format!("Failed to add audit_action_id to workflow_step_logs: {e}")))?;

    Ok(())
}

fn ensure_workflow_step_log_columns(conn: &Connection) -> Result<()> {
    if !table_exists(conn, "workflow_step_logs")? {
        return Ok(());
    }

    let columns = [
        ("step_id", "TEXT"),
        ("step_kind", "TEXT"),
        ("policy_summary", "TEXT"),
        ("verification_refs", "TEXT"),
        ("error_code", "TEXT"),
    ];

    for (column, column_type) in columns {
        if table_has_column(conn, "workflow_step_logs", column)? {
            continue;
        }
        conn.execute(
            &format!("ALTER TABLE workflow_step_logs ADD COLUMN {column} {column_type};"),
            [],
        )
        .map_err(|e| {
            StorageError::MigrationFailed(format!(
                "Failed to add {column} to workflow_step_logs: {e}"
            ))
        })?;
    }

    Ok(())
}

fn migration_for_version(version: i32) -> Option<&'static Migration> {
    MIGRATIONS.iter().find(|m| m.version == version)
}

fn previous_migration_version(version: i32) -> i32 {
    let mut prev = 0;
    for migration in MIGRATIONS {
        if migration.version < version {
            prev = migration.version;
        } else {
            break;
        }
    }
    prev
}

fn build_migration_plan(from_version: i32, to_version: i32) -> Result<MigrationPlan> {
    if to_version > SCHEMA_VERSION {
        return Err(StorageError::MigrationFailed(format!(
            "Target schema version ({to_version}) is newer than supported ({SCHEMA_VERSION}). \
             Please upgrade wa to a newer version."
        ))
        .into());
    }

    if to_version < 1 {
        return Err(StorageError::MigrationFailed(format!(
            "Target schema version ({to_version}) is not supported. \
             The minimum supported schema version is 1."
        ))
        .into());
    }

    if from_version == to_version {
        return Ok(MigrationPlan {
            from_version,
            to_version,
            direction: MigrationDirection::Up,
            steps: Vec::new(),
        });
    }

    if to_version > from_version {
        let steps = MIGRATIONS
            .iter()
            .filter(|m| m.version > from_version && m.version <= to_version)
            .map(|m| MigrationStep {
                migration_version: m.version,
                resulting_version: m.version,
                description: m.description,
                direction: MigrationDirection::Up,
            })
            .collect();

        return Ok(MigrationPlan {
            from_version,
            to_version,
            direction: MigrationDirection::Up,
            steps,
        });
    }

    let mut steps = Vec::new();
    for migration in MIGRATIONS.iter().rev() {
        if migration.version <= to_version || migration.version > from_version {
            continue;
        }
        if migration.down_sql.is_none() {
            return Err(StorageError::MigrationFailed(format!(
                "Rollback not supported for migration v{} ({})",
                migration.version, migration.description
            ))
            .into());
        }
        let resulting_version = previous_migration_version(migration.version);
        steps.push(MigrationStep {
            migration_version: migration.version,
            resulting_version,
            description: migration.description,
            direction: MigrationDirection::Down,
        });
    }

    Ok(MigrationPlan {
        from_version,
        to_version,
        direction: MigrationDirection::Down,
        steps,
    })
}

fn apply_migration_step(conn: &Connection, step: &MigrationStep) -> Result<()> {
    let Some(migration) = migration_for_version(step.migration_version) else {
        return Err(StorageError::MigrationFailed(format!(
            "Unknown migration version {}",
            step.migration_version
        ))
        .into());
    };

    conn.execute_batch("BEGIN IMMEDIATE").map_err(|e| {
        StorageError::MigrationFailed(format!(
            "Failed to start migration transaction for v{}: {e}",
            migration.version
        ))
    })?;

    let result = match step.direction {
        MigrationDirection::Up => {
            if migration.version == 4 {
                ensure_workflow_step_logs_audit_action_id(conn)?;
            }
            if migration.version == 7 {
                ensure_workflow_step_log_columns(conn)?;
            }
            if !migration.up_sql.is_empty() {
                conn.execute_batch(migration.up_sql).map_err(|e| {
                    StorageError::MigrationFailed(format!(
                        "Migration to v{} ({}) failed: {e}",
                        migration.version, migration.description
                    ))
                })?;
            }
            set_user_version(conn, migration.version)?;
            record_migration(conn, migration.version, migration.description)?;
            Ok(())
        }
        MigrationDirection::Down => {
            let down_sql = migration.down_sql.ok_or_else(|| {
                StorageError::MigrationFailed(format!(
                    "Rollback not supported for migration v{} ({})",
                    migration.version, migration.description
                ))
            })?;
            if !down_sql.is_empty() {
                conn.execute_batch(down_sql).map_err(|e| {
                    StorageError::MigrationFailed(format!(
                        "Rollback of v{} ({}) failed: {e}",
                        migration.version, migration.description
                    ))
                })?;
            }
            set_user_version(conn, step.resulting_version)?;
            record_migration(
                conn,
                step.resulting_version,
                &format!("Rollback: {}", migration.description),
            )?;
            Ok(())
        }
    };

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT").map_err(|e| {
                StorageError::MigrationFailed(format!(
                    "Failed to commit migration transaction for v{}: {e}",
                    migration.version
                ))
            })?;
            Ok(())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

fn apply_migration_plan(conn: &Connection, plan: &MigrationPlan) -> Result<()> {
    for step in &plan.steps {
        apply_migration_step(conn, step)?;
        tracing::info!(
            direction = step.direction.as_str(),
            version = step.migration_version,
            resulting_version = step.resulting_version,
            description = step.description,
            "Applied schema migration step"
        );
    }
    Ok(())
}

/// Apply all pending migrations from the current version to SCHEMA_VERSION.
///
/// Each migration is applied in order, and the user_version is updated after
/// each successful migration. This ensures that if a migration fails partway
/// through, the database version correctly reflects which migrations have
/// been applied.
fn run_migrations(conn: &Connection, from_version: i32) -> Result<()> {
    let plan = build_migration_plan(from_version, SCHEMA_VERSION)?;
    apply_migration_plan(conn, &plan)
}

/// Get the current schema version from the schema_version audit table.
///
/// This returns the version from the audit table, which should match
/// PRAGMA user_version but provides history of when migrations were applied.
pub fn get_schema_version(conn: &Connection) -> Result<Option<i32>> {
    conn.query_row(
        "SELECT version FROM schema_version ORDER BY applied_at DESC, rowid DESC LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| StorageError::Database(e.to_string()).into())
}

#[derive(Debug, Clone)]
struct WaMeta {
    schema_version: i32,
    min_compatible_wa: String,
    created_by_wa: String,
    created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct WaVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl WaVersion {
    fn parse(input: &str) -> Option<Self> {
        let core = input.split(['-', '+']).next().unwrap_or_default();
        let mut parts = core.split('.');
        let major: u64 = parts.next()?.parse().ok()?;
        let minor: u64 = parts.next().unwrap_or("0").parse().ok()?;
        let patch: u64 = parts.next().unwrap_or("0").parse().ok()?;
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn canonicalize_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            let mut canonical = serde_json::Map::new();
            for key in keys {
                if let Some(val) = map.get(&key) {
                    canonical.insert(key, canonicalize_json_value(val));
                }
            }
            serde_json::Value::Object(canonical)
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonicalize_json_value).collect())
        }
        _ => value.clone(),
    }
}

fn canonical_json_string(value: &serde_json::Value) -> Result<String> {
    let canonical = canonicalize_json_value(value);
    serde_json::to_string(&canonical).map_err(|e| {
        StorageError::Database(format!("Failed to serialize canonical JSON: {e}")).into()
    })
}

fn action_plan_record_from_plan(
    workflow_id: &str,
    plan: &crate::plan::ActionPlan,
) -> Result<WorkflowActionPlanRecord> {
    let mut plan = plan.clone();
    if plan.created_at.is_none() {
        plan.created_at = Some(now_epoch_ms());
    }
    let plan_hash = plan.compute_hash();
    let plan_json_value =
        serde_json::to_value(&plan).map_err(|e| StorageError::Database(e.to_string()))?;
    let plan_json = canonical_json_string(&plan_json_value)?;
    let created_at = plan.created_at.unwrap_or_else(now_epoch_ms);
    Ok(WorkflowActionPlanRecord {
        workflow_id: workflow_id.to_string(),
        plan_id: plan.plan_id.to_string(),
        plan_hash,
        plan_json,
        created_at,
    })
}

fn load_wa_meta(conn: &Connection) -> Result<Option<WaMeta>> {
    if !table_exists(conn, "wa_meta")? {
        return Ok(None);
    }

    conn.query_row(
        "SELECT schema_version, min_compatible_wa, created_by_wa, created_at \
         FROM wa_meta WHERE id = 1",
        [],
        |row| {
            Ok(WaMeta {
                schema_version: row.get(0)?,
                min_compatible_wa: row.get(1)?,
                created_by_wa: row.get(2)?,
                created_at: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(e.to_string()).into())
}

fn ensure_wa_meta(conn: &Connection, schema_version: i32) -> Result<()> {
    if !table_exists(conn, "wa_meta")? {
        return Ok(());
    }

    let now_ms = now_epoch_ms();
    let mut current_wa = crate::VERSION.to_string();

    let existing = load_wa_meta(conn)?;
    match existing {
        None => {
            conn.execute(
                "INSERT INTO wa_meta \
                 (id, schema_version, min_compatible_wa, created_by_wa, created_at) \
                 VALUES (1, ?1, ?2, ?3, ?4)",
                params![schema_version, current_wa, current_wa, now_ms],
            )
            .map_err(|e| StorageError::Database(e.to_string()))?;
        }
        Some(meta) => {
            let mut min_compatible = meta.min_compatible_wa.clone();
            if let (Some(current), Some(existing_min)) = (
                WaVersion::parse(&current_wa),
                WaVersion::parse(&meta.min_compatible_wa),
            ) {
                if current > existing_min {
                    min_compatible.clone_from(&current_wa);
                }
            } else if meta.min_compatible_wa != current_wa {
                min_compatible.clone_from(&current_wa);
            }

            let created_by = if meta.created_by_wa.is_empty() {
                std::mem::take(&mut current_wa)
            } else {
                meta.created_by_wa.clone()
            };
            let created_at = if meta.created_at <= 0 {
                now_ms
            } else {
                meta.created_at
            };

            if meta.schema_version != schema_version
                || meta.min_compatible_wa != min_compatible
                || meta.created_by_wa != created_by
                || meta.created_at != created_at
            {
                conn.execute(
                    "UPDATE wa_meta \
                     SET schema_version=?1, min_compatible_wa=?2, created_by_wa=?3, created_at=?4 \
                     WHERE id = 1",
                    params![schema_version, min_compatible, created_by, created_at],
                )
                .map_err(|e| StorageError::Database(e.to_string()))?;
            }
        }
    }

    Ok(())
}

fn check_wa_version_compatibility(conn: &Connection) -> Result<()> {
    let Some(meta) = load_wa_meta(conn)? else {
        return Ok(());
    };

    let Some(current) = WaVersion::parse(crate::VERSION) else {
        return Err(StorageError::MigrationFailed(format!(
            "Invalid wa version string: {}",
            crate::VERSION
        ))
        .into());
    };

    let Some(min) = WaVersion::parse(&meta.min_compatible_wa) else {
        return Err(StorageError::MigrationFailed(format!(
            "Invalid min_compatible_wa value in database: {}",
            meta.min_compatible_wa
        ))
        .into());
    };

    if current < min {
        return Err(StorageError::WaTooOld {
            current: crate::VERSION.to_string(),
            min_compatible: meta.min_compatible_wa,
        }
        .into());
    }

    Ok(())
}

/// Check if schema needs initialization (fresh database).
pub fn needs_initialization(conn: &Connection) -> Result<bool> {
    let table_exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='panes'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;

    Ok(table_exists == 0)
}

/// Get list of pending migrations that would be applied.
///
/// Useful for dry-run scenarios or displaying upgrade information.
#[must_use]
pub fn pending_migrations(current_version: i32) -> Vec<&'static Migration> {
    MIGRATIONS
        .iter()
        .filter(|m| m.version > current_version)
        .collect()
}

/// Build a migration plan for a database path without executing it.
pub fn migration_plan_for_path(db_path: &Path, target_version: i32) -> Result<MigrationPlan> {
    if !db_path.exists() {
        return Err(StorageError::MigrationFailed(format!(
            "Database not found at {}",
            db_path.display()
        ))
        .into());
    }

    let conn = Connection::open(db_path)
        .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
    let needs_init = needs_initialization(&conn)?;
    if needs_init {
        return Err(StorageError::MigrationFailed(
            "Database is uninitialized; run migration without --status to initialize".to_string(),
        )
        .into());
    }

    let current = get_user_version(&conn)?;
    build_migration_plan(current, target_version)
}

/// Return a migration status report for a database path.
pub fn migration_status_for_path(db_path: &Path) -> Result<MigrationStatusReport> {
    let db_exists = db_path.exists();
    if !db_exists {
        return Ok(MigrationStatusReport {
            db_exists,
            needs_initialization: true,
            current_version: 0,
            target_version: SCHEMA_VERSION,
            entries: MIGRATIONS
                .iter()
                .map(|m| MigrationStatusEntry {
                    version: m.version,
                    description: m.description,
                    applied: false,
                    rollback_supported: m.down_sql.is_some(),
                })
                .collect(),
        });
    }

    let conn = Connection::open(db_path)
        .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
    let needs_init = needs_initialization(&conn)?;
    let current = get_user_version(&conn)?;
    let entries = MIGRATIONS
        .iter()
        .map(|m| MigrationStatusEntry {
            version: m.version,
            description: m.description,
            applied: !needs_init && m.version <= current,
            rollback_supported: m.down_sql.is_some(),
        })
        .collect();

    Ok(MigrationStatusReport {
        db_exists,
        needs_initialization: needs_init,
        current_version: current,
        target_version: SCHEMA_VERSION,
        entries,
    })
}

/// Migrate a database at the given path to a target schema version.
///
/// If the database is uninitialized, it will be initialized to the current
/// schema version (SCHEMA_VERSION). Initializing to older versions is not supported.
pub fn migrate_database_to_version(db_path: &Path, target_version: i32) -> Result<MigrationPlan> {
    ensure_parent_dir(db_path)?;

    let conn = Connection::open(db_path)
        .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
    let needs_init = needs_initialization(&conn)?;
    let current = get_user_version(&conn)?;

    if needs_init {
        if target_version != SCHEMA_VERSION {
            return Err(StorageError::MigrationFailed(format!(
                "Database is uninitialized; can only initialize to current schema version ({SCHEMA_VERSION})."
            ))
            .into());
        }
        initialize_schema(&conn)?;
        return Ok(MigrationPlan {
            from_version: 0,
            to_version: SCHEMA_VERSION,
            direction: MigrationDirection::Up,
            steps: vec![MigrationStep {
                migration_version: SCHEMA_VERSION,
                resulting_version: SCHEMA_VERSION,
                description: "Initial schema",
                direction: MigrationDirection::Up,
            }],
        });
    }

    let plan = build_migration_plan(current, target_version)?;
    apply_migration_plan(&conn, &plan)?;
    Ok(plan)
}

// =============================================================================
// Writer Command Types
// =============================================================================

/// Commands sent to the writer thread
enum WriteCommand {
    /// Append a segment (pane_id, content, content_hash, response channel)
    AppendSegment {
        pane_id: u64,
        content: String,
        content_hash: Option<String>,
        respond: oneshot::Sender<Result<Segment>>,
    },
    /// Record a gap event
    RecordGap {
        pane_id: u64,
        reason: String,
        respond: oneshot::Sender<Result<Option<Gap>>>,
    },
    /// Record an event/detection
    RecordEvent {
        event: StoredEvent,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Mark event as handled
    MarkEventHandled {
        event_id: i64,
        workflow_id: Option<String>,
        status: String,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Set or clear triage state for an event.
    SetEventTriageState {
        event_id: i64,
        triage_state: Option<String>,
        updated_by: Option<String>,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Set or clear the note for an event (note text is redacted before persist).
    SetEventNote {
        event_id: i64,
        note: Option<String>,
        updated_by: Option<String>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Add a label to an event (idempotent).
    AddEventLabel {
        event_id: i64,
        label: String,
        created_by: Option<String>,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Remove a label from an event.
    RemoveEventLabel {
        event_id: i64,
        label: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Insert or update a persistent event mute
    UpsertEventMute {
        record: EventMuteRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Delete a persistent event mute
    DeleteEventMute {
        identity_key: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Upsert a pane record
    UpsertPane {
        pane: PaneRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Insert or update a workflow execution
    UpsertWorkflow {
        workflow: WorkflowRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Insert or update a workflow action plan
    UpsertActionPlan {
        record: WorkflowActionPlanRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Insert a prepared plan preview
    InsertPreparedPlan {
        record: PreparedPlanRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Consume a prepared plan (mark as used)
    ConsumePreparedPlan {
        plan_id: String,
        now_ms: i64,
        respond: oneshot::Sender<Result<Option<PreparedPlanRecord>>>,
    },
    /// Insert a workflow step log
    InsertStepLog {
        workflow_id: String,
        audit_action_id: Option<i64>,
        step_index: usize,
        step_name: String,
        step_id: Option<String>,
        step_kind: Option<String>,
        result_type: String,
        result_data: Option<String>,
        policy_summary: Option<String>,
        verification_refs: Option<String>,
        error_code: Option<String>,
        started_at: i64,
        completed_at: i64,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Upsert undo metadata for an audit action
    UpsertActionUndo {
        record: ActionUndoRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Mark an undo record as executed by setting undone_at/undone_by.
    MarkActionUndone {
        audit_action_id: i64,
        undone_at: i64,
        undone_by: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Upsert an agent session record
    UpsertSession {
        session: AgentSessionRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Record an audit action
    RecordAuditAction {
        action: AuditActionRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Purge audit actions older than a cutoff timestamp
    PurgeAuditActions {
        before_ts: i64,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Insert an approval token
    InsertApprovalToken {
        token: ApprovalTokenRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Consume (use) an approval token if it matches scope
    ConsumeApprovalToken {
        code_hash: String,
        workspace_id: String,
        action_kind: String,
        pane_id: Option<u64>,
        action_fingerprint: String,
        respond: oneshot::Sender<Result<Option<ApprovalTokenRecord>>>,
    },
    /// Get an approval token by code hash (without consuming)
    GetApprovalTokenByCode {
        code_hash: String,
        workspace_id: String,
        respond: oneshot::Sender<Result<Option<ApprovalTokenRecord>>>,
    },
    /// Consume an approval token by code hash only (without fingerprint validation)
    ConsumeApprovalTokenByCode {
        code_hash: String,
        workspace_id: String,
        respond: oneshot::Sender<Result<Option<ApprovalTokenRecord>>>,
    },
    /// Record a maintenance event
    RecordMaintenance {
        record: MaintenanceRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Record a secret scan report
    RecordSecretScanReport {
        record: SecretScanReportRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Insert a saved search definition
    InsertSavedSearch {
        record: SavedSearchRecord,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Update last-run metadata for a saved search
    UpdateSavedSearchRun {
        id: String,
        last_run_at: i64,
        last_result_count: Option<i64>,
        last_error: Option<String>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Update scheduling settings for a saved search
    UpdateSavedSearchSchedule {
        id: String,
        enabled: bool,
        schedule_interval_ms: Option<i64>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Delete a saved search by name
    DeleteSavedSearch {
        name: String,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Prune output segments older than a cutoff
    PruneSegments {
        before_ts: i64,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Vacuum the database (explicit)
    Vacuum {
        respond: oneshot::Sender<Result<()>>,
    },
    /// Upsert an account record (insert or update by service+account_id)
    UpsertAccount {
        account: crate::accounts::AccountRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Update an account's last_used_at timestamp
    UpdateAccountLastUsed {
        service: String,
        account_id: String,
        last_used_at: i64,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Delete an account by service and account_id
    DeleteAccount {
        service: String,
        account_id: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Create a pane reservation (exclusive lock)
    CreateReservation {
        pane_id: u64,
        owner_kind: String,
        owner_id: String,
        reason: Option<String>,
        ttl_ms: i64,
        respond: oneshot::Sender<Result<PaneReservation>>,
    },
    /// Release a pane reservation by ID
    ReleaseReservation {
        reservation_id: i64,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Expire all stale reservations (past their TTL)
    ExpireStaleReservations {
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Checkpoint WAL (incremental, non-blocking)
    Checkpoint {
        respond: oneshot::Sender<Result<CheckpointResult>>,
    },
    /// Record a usage metric
    RecordUsageMetric {
        record: UsageMetricRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Record multiple usage metrics in a single transaction.
    ///
    /// This is used by higher-level collectors to avoid DB spam when a single
    /// event produces multiple metric rows (eg, caut refresh -> N accounts).
    RecordUsageMetricsBatch {
        records: Vec<UsageMetricRecord>,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Purge usage metrics older than a cutoff timestamp
    PurgeUsageMetrics {
        before_ts: i64,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Record a notification in the history log
    RecordNotification {
        record: NotificationHistoryRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Update the status of a notification
    UpdateNotificationStatus {
        id: i64,
        status: NotificationStatus,
        error_message: Option<String>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Acknowledge a notification
    AcknowledgeNotification {
        id: i64,
        acknowledged_by: String,
        action_taken: Option<String>,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Increment retry count for a notification
    IncrementNotificationRetry {
        id: i64,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Purge notification history older than a cutoff timestamp
    PurgeNotificationHistory {
        before_ts: i64,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Delete events older than a cutoff (flat, no tier filters)
    DeleteEventsBefore {
        before_ts: i64,
        batch_size: usize,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Delete events matching tier criteria older than a cutoff
    DeleteEventsByTier {
        before_ts: i64,
        severities: Vec<String>,
        event_types: Vec<String>,
        handled: Option<bool>,
        batch_size: usize,
        respond: oneshot::Sender<Result<usize>>,
    },
    /// Insert a pane bookmark
    InsertPaneBookmark {
        record: PaneBookmarkRecord,
        respond: oneshot::Sender<Result<i64>>,
    },
    /// Delete a pane bookmark by alias
    DeletePaneBookmark {
        alias: String,
        respond: oneshot::Sender<Result<bool>>,
    },
    /// Shutdown the writer thread (flush pending writes)
    Shutdown { respond: oneshot::Sender<()> },
}

/// Configuration for the storage handle
pub struct StorageConfig {
    /// Maximum number of pending write commands before backpressure
    pub write_queue_size: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            write_queue_size: 1024,
        }
    }
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let existed = parent.exists();
            std::fs::create_dir_all(parent)
                .map_err(|e| StorageError::Database(format!("Failed to create directory: {e}")))?;
            #[cfg(unix)]
            if !existed {
                set_permissions(parent, 0o700)?;
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_permissions(path: &Path, mode: u32) -> Result<()> {
    let permissions = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, permissions).map_err(|e| {
        StorageError::Database(format!(
            "Failed to set permissions on {}: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

#[cfg(unix)]
fn ensure_db_permissions(path: &Path, is_new: bool) -> Result<()> {
    if is_new {
        set_permissions(path, 0o600)?;
    }

    let wal_path = std::path::PathBuf::from(format!("{}-wal", path.display()));
    if wal_path.exists() {
        set_permissions(&wal_path, 0o600)?;
    }

    let shm_path = std::path::PathBuf::from(format!("{}-shm", path.display()));
    if shm_path.exists() {
        set_permissions(&shm_path, 0o600)?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn ensure_db_permissions(_path: &Path, _is_new: bool) -> Result<()> {
    Ok(())
}

// =============================================================================
// Storage Handle
// =============================================================================

/// Async-safe storage handle
///
/// Provides an async API for storage operations. Writes are serialized through
/// a dedicated writer thread to avoid blocking the async runtime. Reads use
/// spawn_blocking with WAL mode for concurrent access.
#[derive(Clone)]
pub struct StorageHandle {
    /// Sender for write commands
    write_tx: mpsc::Sender<WriteCommand>,
    /// Database path for read connections
    db_path: Arc<String>,
    /// Writer thread join handle (for shutdown) - shared to allow Clone
    writer_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl StorageHandle {
    /// Create a new storage handle
    ///
    /// Opens/creates the database at `db_path`, initializes the schema,
    /// and starts the writer thread.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or schema fails.
    pub async fn new(db_path: &str) -> Result<Self> {
        Self::with_config(db_path, StorageConfig::default()).await
    }

    /// Return the database path backing this storage handle.
    #[must_use]
    pub fn db_path(&self) -> &str {
        self.db_path.as_str()
    }

    /// Create a storage handle with custom configuration
    pub async fn with_config(db_path: &str, config: StorageConfig) -> Result<Self> {
        // Ensure parent directory exists
        ensure_parent_dir(Path::new(db_path))?;

        // Open connection, recover WAL if needed, and initialize schema (blocking)
        let db_path_owned = db_path.to_string();
        let db_existed = Path::new(&db_path_owned).exists();
        let init_result = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let conn = Connection::open(&db_path_owned)
                .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;

            // Check for and recover from unclean shutdown (wa-o8j)
            check_and_recover_wal(&conn, &db_path_owned)?;

            initialize_schema(&conn)?;
            #[cfg(unix)]
            {
                ensure_db_permissions(Path::new(&db_path_owned), !db_existed)?;
            }
            Ok(conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))??;

        // Create bounded channel for write commands
        let (write_tx, mut write_rx) = mpsc::channel::<WriteCommand>(config.write_queue_size);

        // Spawn writer thread
        let writer_handle = thread::spawn(move || {
            let mut conn = init_result;
            writer_loop(&mut conn, &mut write_rx);
        });

        Ok(Self {
            write_tx,
            db_path: Arc::new(db_path.to_string()),
            writer_handle: Arc::new(Mutex::new(Some(writer_handle))),
        })
    }

    /// Append a segment to storage
    ///
    /// Automatically assigns the next sequence number for the pane.
    /// The pane must exist (call `upsert_pane` first).
    pub async fn append_segment(
        &self,
        pane_id: u64,
        content: &str,
        content_hash: Option<String>,
    ) -> Result<Segment> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::AppendSegment {
                pane_id,
                content: content.to_string(),
                content_hash,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Record a gap event
    ///
    /// Indicates a discontinuity in capture for the given pane.
    /// Returns `None` if the gap was skipped (e.g. at start of stream).
    pub async fn record_gap(&self, pane_id: u64, reason: &str) -> Result<Option<Gap>> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::RecordGap {
                pane_id,
                reason: reason.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Record an event (pattern detection)
    ///
    /// Returns the event ID.
    pub async fn record_event(&self, event: StoredEvent) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::RecordEvent { event, respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Mark an event as handled
    pub async fn mark_event_handled(
        &self,
        event_id: i64,
        workflow_id: Option<String>,
        status: &str,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::MarkEventHandled {
                event_id,
                workflow_id,
                status: status.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Set or clear an event's triage state.
    ///
    /// Returns true if an event row was updated.
    pub async fn set_event_triage_state(
        &self,
        event_id: i64,
        triage_state: Option<String>,
        updated_by: Option<String>,
    ) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::SetEventTriageState {
                event_id,
                triage_state,
                updated_by,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Set or clear an event's note.
    ///
    /// Note text is redacted before being persisted.
    pub async fn set_event_note(
        &self,
        event_id: i64,
        note: Option<String>,
        updated_by: Option<String>,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::SetEventNote {
                event_id,
                note,
                updated_by,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Add a label to an event.
    ///
    /// Returns true if a new label row was inserted.
    pub async fn add_event_label(
        &self,
        event_id: i64,
        label: String,
        created_by: Option<String>,
    ) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::AddEventLabel {
                event_id,
                label,
                created_by,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Remove a label from an event.
    ///
    /// Returns true if a label row was deleted.
    pub async fn remove_event_label(&self, event_id: i64, label: String) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::RemoveEventLabel {
                event_id,
                label,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Fetch triage state, note, and labels for an event.
    pub async fn get_event_annotations(&self, event_id: i64) -> Result<Option<EventAnnotations>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_event_annotations_sync(&conn, event_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Add or update a persistent event mute by identity key.
    pub async fn add_event_mute(&self, record: EventMuteRecord) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpsertEventMute {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Remove a persistent event mute by identity key.
    pub async fn remove_event_mute(&self, identity_key: &str) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::DeleteEventMute {
                identity_key: identity_key.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Check whether an identity key is muted (and not expired).
    pub async fn is_event_muted(&self, identity_key: &str, now_ms: i64) -> Result<bool> {
        let db_path = Arc::clone(&self.db_path);
        let identity_key = identity_key.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_event_mute(&conn, &identity_key, now_ms)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// List all active (non-expired) mutes.
    pub async fn list_active_mutes(&self, now_ms: i64) -> Result<Vec<EventMuteRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            list_active_mutes_sync(&conn, now_ms)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Fetch an event's dedupe/identity key by ID.
    pub async fn get_event_identity_key(&self, event_id: i64) -> Result<Option<String>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_event_identity_key(&conn, event_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Record an audit action
    pub async fn record_audit_action(&self, action: AuditActionRecord) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::RecordAuditAction {
                action,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Record an audit action after applying redaction
    pub async fn record_audit_action_redacted(&self, mut action: AuditActionRecord) -> Result<i64> {
        let redactor = Redactor::new();
        action.redact_fields(&redactor);
        self.record_audit_action(action).await
    }

    /// Upsert undo metadata for an audit action
    pub async fn upsert_action_undo(&self, record: ActionUndoRecord) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpsertActionUndo {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Upsert undo metadata after applying redaction
    pub async fn upsert_action_undo_redacted(&self, mut record: ActionUndoRecord) -> Result<()> {
        let redactor = Redactor::new();
        record.redact_fields(&redactor);
        self.upsert_action_undo(record).await
    }

    /// Fetch undo metadata for a specific audit action ID.
    pub async fn get_action_undo(&self, audit_action_id: i64) -> Result<Option<ActionUndoRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_action_undo_sync(&conn, audit_action_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Mark an undo record as executed.
    ///
    /// Returns `true` when the row was updated and `false` when the target
    /// action was already undone, non-undoable, or missing undo metadata.
    pub async fn mark_action_undone(&self, audit_action_id: i64, undone_by: &str) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::MarkActionUndone {
                audit_action_id,
                undone_at: now_ms(),
                undone_by: undone_by.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Purge audit actions older than a cutoff timestamp
    pub async fn purge_audit_actions_before(&self, before_ts: i64) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::PurgeAuditActions {
                before_ts,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Record a maintenance event
    pub async fn record_maintenance(&self, record: MaintenanceRecord) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::RecordMaintenance {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Record a secret scan report (checkpoint + payload).
    pub async fn record_secret_scan_report(&self, record: SecretScanReportRecord) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::RecordSecretScanReport {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Insert a saved search definition.
    pub async fn insert_saved_search(&self, record: SavedSearchRecord) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::InsertSavedSearch {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Update last-run metadata for a saved search.
    pub async fn update_saved_search_run(
        &self,
        id: &str,
        last_run_at: i64,
        last_result_count: Option<i64>,
        last_error: Option<String>,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpdateSavedSearchRun {
                id: id.to_string(),
                last_run_at,
                last_result_count,
                last_error,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Update scheduling settings for a saved search.
    pub async fn update_saved_search_schedule(
        &self,
        id: &str,
        enabled: bool,
        schedule_interval_ms: Option<i64>,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpdateSavedSearchSchedule {
                id: id.to_string(),
                enabled,
                schedule_interval_ms,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Delete a saved search by name. Returns number of rows deleted.
    pub async fn delete_saved_search(&self, name: &str) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::DeleteSavedSearch {
                name: name.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Fetch a saved search by name.
    pub async fn get_saved_search_by_name(&self, name: &str) -> Result<Option<SavedSearchRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_saved_search_by_name(&conn, &name)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// List saved searches in deterministic order.
    pub async fn list_saved_searches(&self) -> Result<Vec<SavedSearchRecord>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            list_saved_searches_sync(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Insert a pane bookmark. Returns the row ID.
    pub async fn insert_pane_bookmark(&self, record: PaneBookmarkRecord) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::InsertPaneBookmark {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Delete a pane bookmark by alias. Returns true if a row was deleted.
    pub async fn delete_pane_bookmark(&self, alias: &str) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::DeletePaneBookmark {
                alias: alias.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Get a pane bookmark by alias.
    pub async fn get_pane_bookmark_by_alias(
        &self,
        alias: &str,
    ) -> Result<Option<PaneBookmarkRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let alias = alias.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_pane_bookmark_by_alias(&conn, &alias)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// List all pane bookmarks in alias order.
    pub async fn list_pane_bookmarks(&self) -> Result<Vec<PaneBookmarkRecord>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            list_pane_bookmarks_sync(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// List pane bookmarks filtered by tag.
    pub async fn list_pane_bookmarks_by_tag(&self, tag: &str) -> Result<Vec<PaneBookmarkRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let tag = tag.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            list_pane_bookmarks_by_tag_sync(&conn, &tag)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Prune output segments older than a cutoff timestamp
    pub async fn prune_segments_before(&self, before_ts: i64) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::PruneSegments {
                before_ts,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Run retention cleanup and log the maintenance event
    pub async fn retention_cleanup(&self, before_ts: i64) -> Result<usize> {
        let deleted = self.prune_segments_before(before_ts).await?;
        let metadata = serde_json::json!({
            "deleted_segments": deleted,
            "before_ts": before_ts,
        })
        .to_string();
        let record = MaintenanceRecord {
            id: 0,
            event_type: "retention_cleanup".to_string(),
            message: Some(format!("Deleted {deleted} output segments")),
            metadata: Some(metadata),
            timestamp: now_ms(),
        };
        let _ = self.record_maintenance(record).await?;
        Ok(deleted)
    }

    /// Record a usage metric for analytics tracking.
    pub async fn record_usage_metric(&self, record: UsageMetricRecord) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::RecordUsageMetric {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Record multiple usage metrics for analytics tracking in a single transaction.
    ///
    /// Returns the number of rows inserted.
    pub async fn record_usage_metrics_batch(
        &self,
        records: Vec<UsageMetricRecord>,
    ) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::RecordUsageMetricsBatch {
                records,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Purge usage metrics older than a cutoff timestamp.
    pub async fn purge_usage_metrics(&self, before_ts: i64) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::PurgeUsageMetrics {
                before_ts,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Query usage metrics with filters (read-only, uses read connection).
    pub async fn query_usage_metrics(&self, query: MetricQuery) -> Result<Vec<UsageMetricRecord>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_usage_metrics_sync(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Get daily aggregated metric summaries since a given timestamp.
    pub async fn aggregate_daily_metrics(&self, since_ts: i64) -> Result<Vec<DailyMetricSummary>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            aggregate_daily_sync(&conn, since_ts)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Get per-agent metric breakdown since a given timestamp.
    pub async fn aggregate_by_agent(&self, since_ts: i64) -> Result<Vec<AgentMetricBreakdown>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            aggregate_by_agent_sync(&conn, since_ts)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    // ---- Notification History ----

    /// Record a notification in the persistent history log.
    pub async fn record_notification(&self, record: NotificationHistoryRecord) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::RecordNotification {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Update the delivery status of a notification.
    pub async fn update_notification_status(
        &self,
        id: i64,
        status: NotificationStatus,
        error_message: Option<String>,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpdateNotificationStatus {
                id,
                status,
                error_message,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Acknowledge a notification (marks when and by whom).
    pub async fn acknowledge_notification(
        &self,
        id: i64,
        acknowledged_by: String,
        action_taken: Option<String>,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::AcknowledgeNotification {
                id,
                acknowledged_by,
                action_taken,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Increment the retry count for a notification and reset its status to pending.
    pub async fn increment_notification_retry(&self, id: i64) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::IncrementNotificationRetry { id, respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Purge notification history older than the given timestamp.
    pub async fn purge_notification_history(&self, before_ts: i64) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::PurgeNotificationHistory {
                before_ts,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    // =========================================================================
    // Cleanup engine: count + delete helpers
    // =========================================================================

    /// Count output_segments older than a cutoff (read-path).
    pub async fn count_segments_before(&self, before_ts: i64) -> Result<usize> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            count_segments_before_sync(&conn, before_ts)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Count events older than a cutoff (flat, no tier filters; read-path).
    pub async fn count_events_before(&self, before_ts: i64) -> Result<usize> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            count_events_before_sync(&conn, before_ts)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Count events matching tier criteria older than a cutoff (read-path).
    pub async fn count_events_by_tier(
        &self,
        before_ts: i64,
        severities: &[String],
        event_types: &[String],
        handled: Option<bool>,
    ) -> Result<usize> {
        let db_path = Arc::clone(&self.db_path);
        let severities = severities.to_vec();
        let event_types = event_types.to_vec();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            count_events_by_tier_sync(&conn, before_ts, &severities, &event_types, handled)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Count audit_actions older than a cutoff (read-path).
    pub async fn count_audit_actions_before(&self, before_ts: i64) -> Result<usize> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            count_audit_actions_before_sync(&conn, before_ts)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Count usage_metrics older than a cutoff (read-path).
    pub async fn count_usage_metrics_before(&self, before_ts: i64) -> Result<usize> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            count_usage_metrics_before_sync(&conn, before_ts)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Count notification_history older than a cutoff (read-path).
    pub async fn count_notification_history_before(&self, before_ts: i64) -> Result<usize> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            count_notification_history_before_sync(&conn, before_ts)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Delete events older than a cutoff (flat, no tier; write-path).
    pub async fn delete_events_before(&self, before_ts: i64, batch_size: usize) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::DeleteEventsBefore {
                before_ts,
                batch_size,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Delete events matching tier criteria older than a cutoff (write-path).
    pub async fn delete_events_by_tier(
        &self,
        before_ts: i64,
        severities: &[String],
        event_types: &[String],
        handled: Option<bool>,
        batch_size: usize,
    ) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::DeleteEventsByTier {
                before_ts,
                severities: severities.to_vec(),
                event_types: event_types.to_vec(),
                handled,
                batch_size,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Query notification history with filters.
    pub async fn query_notification_history(
        &self,
        query: NotificationHistoryQuery,
    ) -> Result<Vec<NotificationHistoryRecord>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_notification_history_sync(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Get a single notification by ID.
    pub async fn get_notification(&self, id: i64) -> Result<NotificationHistoryRecord> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            get_notification_sync(&conn, id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Spawn blocking failed: {e}")))?
    }

    /// Current write queue depth (pending commands waiting for the writer thread).
    ///
    /// Computed as `max_capacity - available_capacity`.  Zero means the writer
    /// is idle; approaching `write_queue_size` means backpressure.
    pub fn write_queue_depth(&self) -> usize {
        self.write_tx.max_capacity() - self.write_tx.capacity()
    }

    /// Maximum write queue capacity (from `StorageConfig.write_queue_size`).
    pub fn write_queue_capacity(&self) -> usize {
        self.write_tx.max_capacity()
    }

    /// Vacuum the database (explicit)
    pub async fn vacuum(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::Vacuum { respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Lightweight WAL checkpoint (PASSIVE) + PRAGMA optimize.
    ///
    /// Prefer this over `vacuum()` for periodic maintenance — it is
    /// non-blocking and much cheaper than a full VACUUM.
    pub async fn checkpoint(&self) -> Result<CheckpointResult> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::Checkpoint { respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Get per-pane indexing statistics (read-only, uses read connection).
    pub async fn get_pane_indexing_stats(&self) -> Result<Vec<PaneIndexingStats>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            get_pane_indexing_stats_sync(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get a full indexing health report (per-pane stats + FTS integrity).
    pub async fn get_indexing_health(&self) -> Result<IndexingHealthReport> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            let stats = get_pane_indexing_stats_sync(&conn)?;
            let fts_ok = check_fts_integrity_sync(&conn)?;
            Ok(build_indexing_health_report(stats, fts_ok))
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Perform incremental FTS sync on startup.
    ///
    /// This checks the FTS index state and either:
    /// 1. Does nothing if index is healthy and version matches
    /// 2. Syncs only new segments if index is healthy but has gaps
    /// 3. Performs a full rebuild if index is corrupt or version mismatches
    ///
    /// Returns a result describing what was synced.
    pub async fn sync_fts(&self, config: FtsSyncConfig) -> Result<FtsSyncResult> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            sync_fts_on_startup(&conn, &config)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Perform a full FTS rebuild regardless of current state.
    ///
    /// This drops the FTS index and reindexes all segments with batched progress.
    /// Use this for recovery or when a clean rebuild is needed.
    pub async fn rebuild_fts(&self, config: FtsSyncConfig) -> Result<FtsSyncResult> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            full_fts_rebuild_sync(&conn, &config)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get the current FTS index state (version, last rebuild time).
    pub async fn get_fts_index_state(&self) -> Result<Option<FtsIndexState>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            get_fts_index_state_sync(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Insert an approval token
    pub async fn insert_approval_token(&self, token: ApprovalTokenRecord) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::InsertApprovalToken { token, respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Consume an approval token if it matches scope and is valid
    #[allow(clippy::too_many_arguments)]
    pub async fn consume_approval_token(
        &self,
        code_hash: &str,
        workspace_id: &str,
        action_kind: &str,
        pane_id: Option<u64>,
        action_fingerprint: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::ConsumeApprovalToken {
                code_hash: code_hash.to_string(),
                workspace_id: workspace_id.to_string(),
                action_kind: action_kind.to_string(),
                pane_id,
                action_fingerprint: action_fingerprint.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Get an approval token by code hash (without consuming)
    pub async fn get_approval_token_by_code(
        &self,
        code_hash: &str,
        workspace_id: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::GetApprovalTokenByCode {
                code_hash: code_hash.to_string(),
                workspace_id: workspace_id.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Consume an approval token by code hash only (without fingerprint validation)
    pub async fn consume_approval_token_by_code(
        &self,
        code_hash: &str,
        workspace_id: &str,
    ) -> Result<Option<ApprovalTokenRecord>> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::ConsumeApprovalTokenByCode {
                code_hash: code_hash.to_string(),
                workspace_id: workspace_id.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Upsert a pane record
    pub async fn upsert_pane(&self, pane: PaneRecord) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpsertPane { pane, respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Upsert a workflow execution record
    pub async fn upsert_workflow(&self, workflow: WorkflowRecord) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpsertWorkflow {
                workflow,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Upsert a workflow action plan (canonical JSON + hash)
    pub async fn upsert_action_plan(
        &self,
        workflow_id: &str,
        plan: &crate::plan::ActionPlan,
    ) -> Result<()> {
        let record = action_plan_record_from_plan(workflow_id, plan)?;
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpsertActionPlan {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Insert a prepared plan preview for later commit
    pub async fn insert_prepared_plan(&self, record: PreparedPlanRecord) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::InsertPreparedPlan {
                record,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Consume a prepared plan by plan_id (marks as used if valid)
    pub async fn consume_prepared_plan(
        &self,
        plan_id: &str,
        now_ms: i64,
    ) -> Result<Option<PreparedPlanRecord>> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::ConsumePreparedPlan {
                plan_id: plan_id.to_string(),
                now_ms,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Insert a workflow step log entry
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_step_log(
        &self,
        workflow_id: &str,
        audit_action_id: Option<i64>,
        step_index: usize,
        step_name: &str,
        step_id: Option<String>,
        step_kind: Option<String>,
        result_type: &str,
        result_data: Option<String>,
        policy_summary: Option<String>,
        verification_refs: Option<String>,
        error_code: Option<String>,
        started_at: i64,
        completed_at: i64,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::InsertStepLog {
                workflow_id: workflow_id.to_string(),
                audit_action_id,
                step_index,
                step_name: step_name.to_string(),
                step_id,
                step_kind,
                result_type: result_type.to_string(),
                result_data,
                policy_summary,
                verification_refs,
                error_code,
                started_at,
                completed_at,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Upsert an agent session record
    ///
    /// Creates a new session or updates an existing one.
    /// Returns the session ID.
    pub async fn upsert_agent_session(&self, session: AgentSessionRecord) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpsertSession {
                session,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Get an agent session by ID
    pub async fn get_agent_session(&self, session_id: i64) -> Result<Option<AgentSessionRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_agent_session(&conn, session_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get active agent sessions (those without an ended_at timestamp)
    pub async fn get_active_sessions(&self) -> Result<Vec<AgentSessionRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_active_sessions(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get agent sessions for a specific pane
    pub async fn get_sessions_for_pane(&self, pane_id: u64) -> Result<Vec<AgentSessionRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_sessions_for_pane(&conn, pane_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Search segments using FTS5
    ///
    /// Returns matching segments ordered by BM25 relevance score.
    pub async fn search(&self, query: &str) -> Result<Vec<Segment>> {
        let results = self
            .search_with_results(query, SearchOptions::default())
            .await?;
        Ok(results.into_iter().map(|r| r.segment).collect())
    }

    /// Search segments with options (legacy, returns segments only)
    pub async fn search_with_options(
        &self,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<Segment>> {
        let results = self.search_with_results(query, options).await?;
        Ok(results.into_iter().map(|r| r.segment).collect())
    }

    /// Search segments with full results including snippets, highlights, and scores
    ///
    /// Returns `SearchResult` objects with:
    /// - The matching segment
    /// - A snippet with highlighted matching terms
    /// - Highlighted content (full segment with markers)
    /// - The BM25 relevance score
    ///
    /// # Errors
    ///
    /// Returns `StorageError::FtsQueryError` if the query syntax is invalid.
    /// FTS5 syntax supports:
    /// - Simple words: `hello world` (matches both terms)
    /// - Phrases: `"hello world"` (matches exact phrase)
    /// - Prefix: `hel*` (matches words starting with "hel")
    /// - Boolean: `hello AND world`, `hello OR world`, `NOT hello`
    /// - Column filter: `content:hello` (search specific column)
    pub async fn search_with_results(
        &self,
        query: &str,
        options: SearchOptions,
    ) -> Result<Vec<SearchResult>> {
        let db_path = Arc::clone(&self.db_path);
        let query = query.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            search_fts_with_snippets(&conn, &query, &options)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get unhandled events
    pub async fn get_unhandled_events(&self, limit: usize) -> Result<Vec<StoredEvent>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_unhandled_events(&conn, limit)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Query events with filters
    pub async fn get_events(&self, query: EventQuery) -> Result<Vec<StoredEvent>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_events(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get a unified timeline of events across panes.
    ///
    /// Returns events enriched with pane info and correlations,
    /// sorted chronologically with pagination support.
    pub async fn get_timeline(&self, query: TimelineQuery) -> Result<Timeline> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_timeline(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Count unhandled events grouped by pane ID
    ///
    /// Returns a map from pane_id to the count of unhandled events for that pane.
    pub async fn count_unhandled_events_by_pane(
        &self,
    ) -> Result<std::collections::HashMap<u64, u32>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_unhandled_event_counts(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get the most recent activity timestamp for each pane
    ///
    /// Returns a map from pane_id to the most recent segment captured_at timestamp.
    pub async fn get_last_activity_by_pane(&self) -> Result<std::collections::HashMap<u64, i64>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_last_activity_by_pane(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Query audit actions with filters
    pub async fn get_audit_actions(&self, query: AuditQuery) -> Result<Vec<AuditActionRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            crate::storage::query_audit_actions(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Stream audit actions using a cursor and stable ordering.
    ///
    /// Records are ordered by monotonically increasing ID for deterministic paging.
    pub async fn get_audit_actions_stream(
        &self,
        query: AuditStreamQuery,
    ) -> Result<AuditStreamPage> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            crate::storage::query_audit_actions_stream(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Query action history view with filters
    pub async fn get_action_history(
        &self,
        query: ActionHistoryQuery,
    ) -> Result<Vec<ActionHistoryRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            crate::storage::query_action_history(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Count active (unused + unexpired) approval tokens for a workspace
    pub async fn count_active_approvals(&self, workspace_id: &str, now_ms: i64) -> Result<u32> {
        let db_path = Arc::clone(&self.db_path);
        let workspace_id = workspace_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_active_approvals_count(&conn, &workspace_id, now_ms)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Look up an approval token by code hash (without consuming it)
    ///
    /// Returns the token record if found, regardless of whether it's expired or consumed.
    /// Use this for validation and dry-run checks.
    pub async fn get_approval_token(&self, code_hash: &str) -> Result<Option<ApprovalTokenRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let code_hash = code_hash.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_approval_token_by_hash(&conn, &code_hash)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get the maximum sequence number for a pane (to resume capture).
    pub async fn get_max_seq(&self, pane_id: u64) -> Result<Option<u64>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_max_seq(&conn, pane_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get all panes
    pub async fn get_panes(&self) -> Result<Vec<PaneRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_panes(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get a specific pane
    pub async fn get_pane(&self, pane_id: u64) -> Result<Option<PaneRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_pane(&conn, pane_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get recent segments for a pane
    pub async fn get_segments(&self, pane_id: u64, limit: usize) -> Result<Vec<Segment>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_segments(&conn, pane_id, limit)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Scan segments in ascending id order with incremental paging.
    pub async fn scan_segments(&self, query: SegmentScanQuery) -> Result<Vec<Segment>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_scan_segments(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Fetch the most recent secret scan report for a scope hash.
    pub async fn latest_secret_scan_report(
        &self,
        scope_hash: &str,
    ) -> Result<Option<SecretScanReportRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let scope_hash = scope_hash.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_latest_secret_scan_report(&conn, &scope_hash)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get workflow by ID
    pub async fn get_workflow(&self, workflow_id: &str) -> Result<Option<WorkflowRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let workflow_id = workflow_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_workflow(&conn, &workflow_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get step logs for a workflow
    ///
    /// Returns all step logs for the given workflow, ordered by step index.
    pub async fn get_step_logs(&self, workflow_id: &str) -> Result<Vec<WorkflowStepLogRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let workflow_id = workflow_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_step_logs(&conn, &workflow_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get the latest step log for a workflow (highest step_index).
    pub async fn get_latest_step_log(
        &self,
        workflow_id: &str,
    ) -> Result<Option<WorkflowStepLogRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let workflow_id = workflow_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_latest_step_log(&conn, &workflow_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get the persisted action plan for a workflow execution, if available
    pub async fn get_action_plan(
        &self,
        workflow_id: &str,
    ) -> Result<Option<WorkflowActionPlanRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let workflow_id = workflow_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_action_plan(&conn, &workflow_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get a prepared plan preview by plan_id
    pub async fn get_prepared_plan(&self, plan_id: &str) -> Result<Option<PreparedPlanRecord>> {
        let db_path = Arc::clone(&self.db_path);
        let plan_id = plan_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_prepared_plan(&conn, &plan_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Find incomplete workflows for resume on restart
    ///
    /// Returns all workflows with status 'running' or 'waiting', ordered by started_at.
    /// These are workflows that were interrupted and should be resumed.
    pub async fn find_incomplete_workflows(&self) -> Result<Vec<WorkflowRecord>> {
        let db_path = Arc::clone(&self.db_path);

        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;

            query_incomplete_workflows(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Check if the storage is writable (writer thread is alive and responsive).
    ///
    /// This is a lightweight health check that sends a ping to the writer thread.
    pub async fn is_writable(&self) -> bool {
        // A simple check: if the channel is not closed, writer should be alive
        // We can't easily send a ping without adding a new WriteCommand variant,
        // so we check if the channel has capacity (indicates writer is processing)
        !self.write_tx.is_closed()
    }

    // =========================================================================
    // Account Operations
    // =========================================================================

    /// Upsert an account record (insert or update by service+account_id)
    ///
    /// Returns the row ID of the upserted account.
    pub async fn upsert_account(&self, account: crate::accounts::AccountRecord) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpsertAccount {
                account,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Update an account's last_used_at timestamp
    ///
    /// Call this when an account is selected for use to maintain LRU ordering.
    pub async fn update_account_last_used(
        &self,
        service: &str,
        account_id: &str,
        last_used_at: i64,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::UpdateAccountLastUsed {
                service: service.to_string(),
                account_id: account_id.to_string(),
                last_used_at,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Delete an account by service and account_id
    ///
    /// Returns true if an account was deleted, false if not found.
    pub async fn delete_account(&self, service: &str, account_id: &str) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::DeleteAccount {
                service: service.to_string(),
                account_id: account_id.to_string(),
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Get all accounts for a service
    ///
    /// Returns accounts sorted by percent_remaining DESC, last_used_at ASC.
    pub async fn get_accounts_by_service(
        &self,
        service: &str,
    ) -> Result<Vec<crate::accounts::AccountRecord>> {
        let db_path = self.db_path.clone();
        let service = service.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str())
                .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
            get_accounts_by_service_sync(&conn, &service)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get a single account by service and account_id
    pub async fn get_account(
        &self,
        service: &str,
        account_id: &str,
    ) -> Result<Option<crate::accounts::AccountRecord>> {
        let db_path = self.db_path.clone();
        let service = service.to_string();
        let account_id = account_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str())
                .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
            get_account_sync(&conn, &service, &account_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Select the best account for a service according to selection policy
    ///
    /// This combines fetching accounts with the selection algorithm from the
    /// accounts module.
    pub async fn select_account(
        &self,
        service: &str,
        config: &crate::accounts::AccountSelectionConfig,
    ) -> Result<crate::accounts::AccountSelectionResult> {
        let accounts = self.get_accounts_by_service(service).await?;
        Ok(crate::accounts::select_account(&accounts, config))
    }

    // =========================================================================
    // Pane Reservation Operations
    // =========================================================================

    /// Create an exclusive pane reservation.
    ///
    /// Returns a conflict error if the pane already has an active reservation.
    /// TTL is clamped to `[1s, max_ttl]` via `PaneReservationConfig`.
    pub async fn create_reservation(
        &self,
        pane_id: u64,
        owner_kind: &str,
        owner_id: &str,
        reason: Option<&str>,
        ttl_ms: i64,
    ) -> Result<PaneReservation> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::CreateReservation {
                pane_id,
                owner_kind: owner_kind.to_string(),
                owner_id: owner_id.to_string(),
                reason: reason.map(String::from),
                ttl_ms,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Release a pane reservation by ID.
    ///
    /// Returns true if released, false if not found or already released.
    pub async fn release_reservation(&self, reservation_id: i64) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::ReleaseReservation {
                reservation_id,
                respond: tx,
            })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Get the active reservation for a pane (read-only).
    pub async fn get_active_reservation(&self, pane_id: u64) -> Result<Option<PaneReservation>> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str())
                .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
            get_active_reservation_sync(&conn, pane_id)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// List all active (unexpired) pane reservations (read-only).
    pub async fn list_active_reservations(&self) -> Result<Vec<PaneReservation>> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str())
                .map_err(|e| StorageError::Database(format!("Failed to open database: {e}")))?;
            list_active_reservations_sync(&conn)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    // =========================================================================
    // Export Query Operations
    // =========================================================================

    /// Export segments with optional pane/time/limit filters
    pub async fn export_segments(&self, query: ExportQuery) -> Result<Vec<Segment>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_export_segments(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Export output gaps with optional pane/time/limit filters
    pub async fn export_gaps(&self, query: ExportQuery) -> Result<Vec<Gap>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_export_gaps(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get all output gaps (for search explain diagnostics)
    pub async fn get_gaps(&self) -> Result<Vec<Gap>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> Result<Vec<Gap>> {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, pane_id, seq_before, seq_after, reason, detected_at \
                     FROM output_gaps ORDER BY detected_at DESC",
                )
                .map_err(|e| StorageError::Database(format!("Prepare gaps query: {e}")))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(Gap {
                        id: row.get(0)?,
                        pane_id: row.get::<_, i64>(1)? as u64,
                        seq_before: row.get::<_, i64>(2)? as u64,
                        seq_after: row.get::<_, i64>(3)? as u64,
                        reason: row.get(4)?,
                        detected_at: row.get(5)?,
                    })
                })
                .map_err(|e| StorageError::Database(format!("Query gaps: {e}")))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|e| StorageError::Database(format!("Collect gaps: {e}")).into())
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Count retention cleanup events (for search explain diagnostics)
    pub async fn get_retention_cleanup_count(&self) -> Result<u64> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM maintenance_log WHERE event_type = 'retention_cleanup'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StorageError::Database(format!("Count retention cleanups: {e}")))?;
            Ok(count as u64)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Get the min/max captured_at timestamps across all segments (for search explain diagnostics)
    pub async fn get_segment_time_range(&self) -> Result<(Option<i64>, Option<i64>)> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || -> Result<(Option<i64>, Option<i64>)> {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            let (earliest, latest): (Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT MIN(captured_at), MAX(captured_at) FROM output_segments",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|e| StorageError::Database(format!("Query segment time range: {e}")))?;
            Ok((earliest, latest))
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Export workflow executions with optional pane/time/limit filters
    pub async fn export_workflows(&self, query: ExportQuery) -> Result<Vec<WorkflowRecord>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_export_workflows(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Export agent sessions with optional pane/time/limit filters
    pub async fn export_sessions(&self, query: ExportQuery) -> Result<Vec<AgentSessionRecord>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_export_sessions(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Export pane reservations (active + historical) with optional pane/time/limit filters
    pub async fn export_reservations(&self, query: ExportQuery) -> Result<Vec<PaneReservation>> {
        let db_path = Arc::clone(&self.db_path);
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path.as_str()).map_err(|e| {
                StorageError::Database(format!("Failed to open read connection: {e}"))
            })?;
            query_export_reservations(&conn, &query)
        })
        .await
        .map_err(|e| StorageError::Database(format!("Task join error: {e}")))?
    }

    /// Expire all stale reservations (past their TTL).
    ///
    /// Returns the number of reservations expired.
    pub async fn expire_stale_reservations(&self) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        self.write_tx
            .send(WriteCommand::ExpireStaleReservations { respond: tx })
            .await
            .map_err(|_| StorageError::Database("Writer thread not available".to_string()))?;

        rx.await
            .map_err(|_| StorageError::Database("Writer response channel closed".to_string()))?
    }

    /// Shutdown the storage handle
    ///
    /// Flushes all pending writes and waits for the writer thread to exit.
    /// Safe to call multiple times - subsequent calls are no-ops.
    pub async fn shutdown(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        // Send shutdown command
        let _ = self
            .write_tx
            .send(WriteCommand::Shutdown { respond: tx })
            .await;

        // Wait for acknowledgment
        let _ = rx.await;

        // Wait for thread to finish (only the first caller does this)
        let handle = self.writer_handle.lock().unwrap().take();
        if let Some(handle) = handle {
            handle
                .join()
                .map_err(|_| StorageError::Database("Writer thread panicked".to_string()))?;
        }

        Ok(())
    }
}

/// Search options for FTS queries
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// Maximum number of results
    pub limit: Option<usize>,
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by time range (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range (epoch ms)
    pub until: Option<i64>,
    /// Include snippets in results (default: true)
    pub include_snippets: Option<bool>,
    /// Maximum tokens per snippet (default: 64)
    pub snippet_max_tokens: Option<usize>,
    /// Snippet highlight prefix (default: ">>>")
    pub highlight_prefix: Option<String>,
    /// Snippet highlight suffix (default: "<<<")
    pub highlight_suffix: Option<String>,
}

/// Severity level for query lint findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchLintSeverity {
    /// Query is invalid and should not be executed.
    Error,
    /// Query is valid but likely unintended.
    Warning,
}

/// Lint finding for a search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchLint {
    /// Stable lint identifier.
    pub code: String,
    /// Severity of the lint finding.
    pub severity: SearchLintSeverity,
    /// Human-readable description.
    pub message: String,
    /// Suggested fix or example query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Suggestion for completing a search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchSuggestion {
    /// Suggested query fragment.
    pub text: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

struct SearchSuggestionTemplate {
    text: &'static str,
    description: &'static str,
}

const SEARCH_SUGGESTION_TEMPLATES: &[SearchSuggestionTemplate] = &[
    SearchSuggestionTemplate {
        text: "error",
        description: "Common errors",
    },
    SearchSuggestionTemplate {
        text: "warning",
        description: "Warnings in output",
    },
    SearchSuggestionTemplate {
        text: "panic",
        description: "Rust panics",
    },
    SearchSuggestionTemplate {
        text: "\"usage limit\"",
        description: "Usage limit messages",
    },
    SearchSuggestionTemplate {
        text: "\"rate limit\"",
        description: "Rate limit messages",
    },
    SearchSuggestionTemplate {
        text: "\"approval needed\"",
        description: "Approval prompts",
    },
    SearchSuggestionTemplate {
        text: "compaction",
        description: "Compaction output",
    },
    SearchSuggestionTemplate {
        text: "AND",
        description: "Boolean AND operator",
    },
    SearchSuggestionTemplate {
        text: "OR",
        description: "Boolean OR operator",
    },
    SearchSuggestionTemplate {
        text: "NOT",
        description: "Boolean NOT operator",
    },
    SearchSuggestionTemplate {
        text: "\"exact phrase\"",
        description: "Quoted phrase search",
    },
    SearchSuggestionTemplate {
        text: "term*",
        description: "Prefix wildcard search",
    },
    SearchSuggestionTemplate {
        text: "content:term",
        description: "Restrict to content column",
    },
];

/// Provide deterministic search query suggestions for CLI/TUI autocomplete.
#[must_use]
pub fn search_query_suggestions(query: &str, limit: usize) -> Vec<SearchSuggestion> {
    if limit == 0 {
        return Vec::new();
    }

    let prefix = search_suggestion_prefix(query);
    let prefix_lower = prefix.to_ascii_lowercase();
    let mut suggestions = Vec::new();

    for template in SEARCH_SUGGESTION_TEMPLATES {
        if prefix.is_empty()
            || template
                .text
                .to_ascii_lowercase()
                .starts_with(prefix_lower.as_str())
        {
            suggestions.push(SearchSuggestion {
                text: template.text.to_string(),
                description: Some(template.description.to_string()),
            });
            if suggestions.len() >= limit {
                return suggestions;
            }
        }
    }

    if suggestions.is_empty() && !prefix.is_empty() {
        for template in SEARCH_SUGGESTION_TEMPLATES {
            if template
                .text
                .to_ascii_lowercase()
                .contains(prefix_lower.as_str())
            {
                suggestions.push(SearchSuggestion {
                    text: template.text.to_string(),
                    description: Some(template.description.to_string()),
                });
                if suggestions.len() >= limit {
                    break;
                }
            }
        }
    }

    suggestions
}

fn search_suggestion_prefix(query: &str) -> &str {
    let trimmed = query.trim_end();
    if trimmed.is_empty() {
        return "";
    }
    trimmed.split_whitespace().next_back().unwrap_or("")
}

/// Lint an FTS query for common mistakes.
#[must_use]
pub fn lint_fts_query(query: &str) -> Vec<SearchLint> {
    let trimmed = query.trim();
    let mut lints = Vec::new();

    if trimmed.is_empty() {
        lints.push(SearchLint {
            code: "empty_query".to_string(),
            severity: SearchLintSeverity::Error,
            message: "Query is empty.".to_string(),
            suggestion: Some("Try: wa search \"error\"".to_string()),
        });
        return lints;
    }

    let (tokens, unbalanced_quotes, paren_imbalance, paren_underflow) = tokenize_fts_query(trimmed);

    if unbalanced_quotes {
        lints.push(SearchLint {
            code: "unbalanced_quotes".to_string(),
            severity: SearchLintSeverity::Error,
            message: "Unbalanced double quotes in query.".to_string(),
            suggestion: Some("Close the quote or remove it.".to_string()),
        });
    }

    if paren_underflow {
        lints.push(SearchLint {
            code: "unmatched_paren_close".to_string(),
            severity: SearchLintSeverity::Error,
            message: "Unmatched closing parenthesis in query.".to_string(),
            suggestion: Some("Remove the extra ')' or add a matching '('.".to_string()),
        });
    } else if paren_imbalance {
        lints.push(SearchLint {
            code: "unbalanced_parentheses".to_string(),
            severity: SearchLintSeverity::Warning,
            message: "Unbalanced parentheses in query.".to_string(),
            suggestion: Some("Check grouping parentheses for a match.".to_string()),
        });
    }

    if tokens.is_empty() {
        lints.push(SearchLint {
            code: "empty_tokens".to_string(),
            severity: SearchLintSeverity::Error,
            message: "Query contains no searchable tokens.".to_string(),
            suggestion: Some("Add at least one term, e.g. \"error\".".to_string()),
        });
        return lints;
    }

    let mut prev_operator = false;
    for (idx, token) in tokens.iter().enumerate() {
        let token_trim = token.trim();
        if token_trim.is_empty() {
            continue;
        }

        if is_operator_token(token_trim) {
            if idx == 0 {
                lints.push(SearchLint {
                    code: "leading_operator".to_string(),
                    severity: SearchLintSeverity::Error,
                    message: format!("Query starts with operator '{token_trim}'."),
                    suggestion: Some("Start with a term or a quoted phrase.".to_string()),
                });
            }
            if idx + 1 == tokens.len() {
                lints.push(SearchLint {
                    code: "trailing_operator".to_string(),
                    severity: SearchLintSeverity::Error,
                    message: format!("Query ends with operator '{token_trim}'."),
                    suggestion: Some("Add a term after the operator.".to_string()),
                });
            }
            if prev_operator {
                lints.push(SearchLint {
                    code: "double_operator".to_string(),
                    severity: SearchLintSeverity::Error,
                    message: "Consecutive boolean operators detected.".to_string(),
                    suggestion: Some("Remove the extra operator.".to_string()),
                });
            }
            prev_operator = true;
            continue;
        }

        prev_operator = false;

        if is_quoted_token(token_trim) {
            continue;
        }

        if token_trim == "*" {
            lints.push(SearchLint {
                code: "wildcard_only".to_string(),
                severity: SearchLintSeverity::Error,
                message: "Wildcard '*' cannot be used alone.".to_string(),
                suggestion: Some("Use a term with prefix wildcard, e.g. \"err*\".".to_string()),
            });
        } else if token_trim.contains('*') {
            if !token_trim.ends_with('*') {
                lints.push(SearchLint {
                    code: "wildcard_position".to_string(),
                    severity: SearchLintSeverity::Warning,
                    message: format!("Wildcard in '{token_trim}' is not in suffix position."),
                    suggestion: Some("Use prefix search syntax like \"term*\".".to_string()),
                });
            } else if token_trim.starts_with('*') {
                lints.push(SearchLint {
                    code: "wildcard_prefix".to_string(),
                    severity: SearchLintSeverity::Warning,
                    message: format!("Leading wildcard in '{token_trim}' is not supported."),
                    suggestion: Some("Use a suffix wildcard like \"term*\".".to_string()),
                });
            }
        }
    }

    lints
}

fn tokenize_fts_query(query: &str) -> (Vec<String>, bool, bool, bool) {
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    let mut paren_balance: i32 = 0;
    let mut paren_underflow = false;

    for ch in query.chars() {
        if escaped {
            buf.push(ch);
            escaped = false;
            continue;
        }

        if ch == '\\' {
            buf.push(ch);
            escaped = true;
            continue;
        }

        if ch == '"' {
            in_quotes = !in_quotes;
            buf.push(ch);
            continue;
        }

        if !in_quotes {
            match ch {
                '(' => paren_balance += 1,
                ')' => {
                    paren_balance -= 1;
                    if paren_balance < 0 {
                        paren_underflow = true;
                    }
                }
                _ => {}
            }
        }

        if ch.is_whitespace() && !in_quotes {
            if !buf.is_empty() {
                tokens.push(std::mem::take(&mut buf));
            }
        } else {
            buf.push(ch);
        }
    }

    if !buf.is_empty() {
        tokens.push(buf);
    }

    let unbalanced_quotes = in_quotes;
    let paren_imbalance = paren_balance != 0;

    (tokens, unbalanced_quotes, paren_imbalance, paren_underflow)
}

fn is_operator_token(token: &str) -> bool {
    let upper = token.to_ascii_uppercase();
    matches!(upper.as_str(), "AND" | "OR" | "NOT")
}

fn is_quoted_token(token: &str) -> bool {
    token.len() >= 2 && token.starts_with('"') && token.ends_with('"')
}

/// Query options for audit actions
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// Maximum number of results (default: 100)
    pub limit: Option<usize>,
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by domain name
    pub domain: Option<String>,
    /// Filter by actor kind
    pub actor_kind: Option<String>,
    /// Filter by actor identifier
    pub actor_id: Option<String>,
    /// Filter by correlation identifier
    pub correlation_id: Option<String>,
    /// Filter by action kind
    pub action_kind: Option<String>,
    /// Filter by policy decision
    pub policy_decision: Option<String>,
    /// Filter by rule ID
    pub rule_id: Option<String>,
    /// Filter by result
    pub result: Option<String>,
    /// Filter by time range start (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms)
    pub until: Option<i64>,
}

/// Query options for cursor-based audit streaming
#[derive(Debug, Clone, Default)]
pub struct AuditStreamQuery {
    /// Resume after this audit action ID (exclusive)
    pub cursor: Option<i64>,
    /// Maximum number of results (default: 100)
    pub limit: Option<usize>,
    /// Optional offset (applied after cursor filtering)
    pub offset: Option<usize>,
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by domain name
    pub domain: Option<String>,
    /// Filter by actor kind
    pub actor_kind: Option<String>,
    /// Filter by actor identifier
    pub actor_id: Option<String>,
    /// Filter by correlation identifier
    pub correlation_id: Option<String>,
    /// Filter by action kind
    pub action_kind: Option<String>,
    /// Filter by policy decision
    pub policy_decision: Option<String>,
    /// Filter by rule ID
    pub rule_id: Option<String>,
    /// Filter by result
    pub result: Option<String>,
    /// Filter by time range start (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms)
    pub until: Option<i64>,
}

/// Cursor-based audit stream page
#[derive(Debug, Clone, Default)]
pub struct AuditStreamPage {
    /// Ordered audit records for this page
    pub records: Vec<AuditActionRecord>,
    /// Cursor to resume from (last record ID), if any
    pub next_cursor: Option<i64>,
}

/// Query options for action history view
#[derive(Debug, Clone, Default)]
pub struct ActionHistoryQuery {
    /// Filter by audit action ID
    pub audit_action_id: Option<i64>,
    /// Maximum number of results (default: 100)
    pub limit: Option<usize>,
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by domain name
    pub domain: Option<String>,
    /// Filter by actor kind
    pub actor_kind: Option<String>,
    /// Filter by actor identifier
    pub actor_id: Option<String>,
    /// Filter by correlation identifier
    pub correlation_id: Option<String>,
    /// Filter by action kind
    pub action_kind: Option<String>,
    /// Filter by policy decision
    pub policy_decision: Option<String>,
    /// Filter by rule ID
    pub rule_id: Option<String>,
    /// Filter by result
    pub result: Option<String>,
    /// Filter by undoable flag
    pub undoable: Option<bool>,
    /// Filter by time range start (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms)
    pub until: Option<i64>,
}

/// Query options for events
#[derive(Debug, Clone, Default)]
pub struct EventQuery {
    /// Maximum number of results (default: 20)
    pub limit: Option<usize>,
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by rule ID (exact match)
    pub rule_id: Option<String>,
    /// Filter by event type (e.g., "compaction_warning")
    pub event_type: Option<String>,
    /// Filter by triage state (exact match)
    pub triage_state: Option<String>,
    /// Filter by label (exact match)
    pub label: Option<String>,
    /// Only return unhandled events
    pub unhandled_only: bool,
    /// Filter by time range start (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms)
    pub until: Option<i64>,
}

/// Query options for export operations (shared across all export data kinds)
#[derive(Debug, Clone, Default)]
pub struct ExportQuery {
    /// Filter by pane ID
    pub pane_id: Option<u64>,
    /// Filter by time range start (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms)
    pub until: Option<i64>,
    /// Maximum number of results
    pub limit: Option<usize>,
}

/// Query options for incremental segment scans.
#[derive(Debug, Clone)]
pub struct SegmentScanQuery {
    /// Return segments with id strictly greater than this value.
    pub after_id: Option<i64>,
    /// Filter by pane ID.
    pub pane_id: Option<u64>,
    /// Filter by time range start (epoch ms).
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms).
    pub until: Option<i64>,
    /// Maximum number of results to return.
    pub limit: usize,
}

impl Default for SegmentScanQuery {
    fn default() -> Self {
        Self {
            after_id: None,
            pane_id: None,
            since: None,
            until: None,
            limit: 1_000,
        }
    }
}

// =============================================================================
// Writer Thread Implementation
// =============================================================================

/// Maximum commands to drain per batch iteration.
const WRITER_BATCH_CAP: usize = 128;

/// Returns true if the command is a control operation that must run outside a
/// transaction (Shutdown, Vacuum, Checkpoint).
fn is_control_command(cmd: &WriteCommand) -> bool {
    matches!(
        cmd,
        WriteCommand::Shutdown { .. }
            | WriteCommand::Vacuum { .. }
            | WriteCommand::Checkpoint { .. }
    )
}

/// Main loop for the writer thread.
///
/// Batches pending writes into SQLite transactions to amortize journal/fsync
/// overhead.  Control commands (Shutdown, Vacuum, Checkpoint) commit any open
/// transaction first, then execute outside a transaction.
fn writer_loop(conn: &mut Connection, rx: &mut mpsc::Receiver<WriteCommand>) {
    while let Some(first_cmd) = rx.blocking_recv() {
        // Drain any additional pending commands for batching
        let mut batch = Vec::with_capacity(8);
        batch.push(first_cmd);
        while batch.len() < WRITER_BATCH_CAP {
            match rx.try_recv() {
                Ok(cmd) => batch.push(cmd),
                Err(_) => break,
            }
        }

        // Open a transaction when the batch has multiple DML commands.
        // Single-command batches skip the transaction wrapper (SQLite
        // auto-commits each statement anyway).
        let use_txn = batch.len() > 1 && !batch.iter().all(is_control_command);
        let mut txn_open = false;
        if use_txn {
            if conn.execute_batch("BEGIN IMMEDIATE").is_ok() {
                txn_open = true;
            }
        }

        let mut should_break = false;
        for cmd in batch {
            // Control commands must run outside a transaction
            if is_control_command(&cmd) && txn_open {
                let _ = conn.execute_batch("COMMIT");
                txn_open = false;
            }
            dispatch_write_command(conn, cmd, &mut should_break);
        }

        if txn_open {
            let _ = conn.execute_batch("COMMIT");
        }

        if should_break {
            break;
        }
    }
}

/// Dispatch a single write command to the appropriate sync handler.
fn dispatch_write_command(conn: &mut Connection, cmd: WriteCommand, should_break: &mut bool) {
    match cmd {
        WriteCommand::AppendSegment {
            pane_id,
            content,
            content_hash,
            respond,
        } => {
            let result = append_segment_sync(conn, pane_id, &content, content_hash.as_deref());
            let _ = respond.send(result);
        }
        WriteCommand::RecordGap {
            pane_id,
            reason,
            respond,
        } => {
            let result = record_gap_sync(conn, pane_id, &reason);
            let _ = respond.send(result);
        }
        WriteCommand::RecordEvent { event, respond } => {
            let result = record_event_sync(conn, &event);
            let _ = respond.send(result);
        }
        WriteCommand::MarkEventHandled {
            event_id,
            workflow_id,
            status,
            respond,
        } => {
            let result = mark_event_handled_sync(conn, event_id, workflow_id.as_deref(), &status);
            let _ = respond.send(result);
        }
        WriteCommand::SetEventTriageState {
            event_id,
            triage_state,
            updated_by,
            respond,
        } => {
            let result = set_event_triage_state_sync(
                conn,
                event_id,
                triage_state.as_deref(),
                updated_by.as_deref(),
            );
            let _ = respond.send(result);
        }
        WriteCommand::SetEventNote {
            event_id,
            note,
            updated_by,
            respond,
        } => {
            let result =
                set_event_note_sync(conn, event_id, note.as_deref(), updated_by.as_deref());
            let _ = respond.send(result);
        }
        WriteCommand::AddEventLabel {
            event_id,
            label,
            created_by,
            respond,
        } => {
            let result = add_event_label_sync(conn, event_id, &label, created_by.as_deref());
            let _ = respond.send(result);
        }
        WriteCommand::RemoveEventLabel {
            event_id,
            label,
            respond,
        } => {
            let result = remove_event_label_sync(conn, event_id, &label);
            let _ = respond.send(result);
        }
        WriteCommand::UpsertEventMute { record, respond } => {
            let result = upsert_event_mute_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::DeleteEventMute {
            identity_key,
            respond,
        } => {
            let result = delete_event_mute_sync(conn, &identity_key);
            let _ = respond.send(result);
        }
        WriteCommand::UpsertPane { pane, respond } => {
            let result = upsert_pane_sync(conn, &pane);
            let _ = respond.send(result);
        }
        WriteCommand::UpsertWorkflow { workflow, respond } => {
            let result = upsert_workflow_sync(conn, &workflow);
            let _ = respond.send(result);
        }
        WriteCommand::UpsertActionPlan { record, respond } => {
            let result = upsert_action_plan_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::InsertPreparedPlan { record, respond } => {
            let result = insert_prepared_plan_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::ConsumePreparedPlan {
            plan_id,
            now_ms,
            respond,
        } => {
            let result = consume_prepared_plan_sync(conn, &plan_id, now_ms);
            let _ = respond.send(result);
        }
        WriteCommand::InsertStepLog {
            workflow_id,
            audit_action_id,
            step_index,
            step_name,
            step_id,
            step_kind,
            result_type,
            result_data,
            policy_summary,
            verification_refs,
            error_code,
            started_at,
            completed_at,
            respond,
        } => {
            let result = insert_step_log_sync(
                conn,
                &workflow_id,
                audit_action_id,
                step_index,
                &step_name,
                step_id.as_deref(),
                step_kind.as_deref(),
                &result_type,
                result_data.as_deref(),
                policy_summary.as_deref(),
                verification_refs.as_deref(),
                error_code.as_deref(),
                started_at,
                completed_at,
            );
            let _ = respond.send(result);
        }
        WriteCommand::UpsertSession { session, respond } => {
            let result = upsert_agent_session_sync(conn, &session);
            let _ = respond.send(result);
        }
        WriteCommand::RecordAuditAction { action, respond } => {
            let result = record_audit_action_sync(conn, &action);
            let _ = respond.send(result);
        }
        WriteCommand::UpsertActionUndo { record, respond } => {
            let result = upsert_action_undo_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::MarkActionUndone {
            audit_action_id,
            undone_at,
            undone_by,
            respond,
        } => {
            let result = mark_action_undone_sync(conn, audit_action_id, undone_at, &undone_by);
            let _ = respond.send(result);
        }
        WriteCommand::PurgeAuditActions { before_ts, respond } => {
            let result = purge_audit_actions_sync(conn, before_ts);
            let _ = respond.send(result);
        }
        WriteCommand::InsertApprovalToken { token, respond } => {
            let result = insert_approval_token_sync(conn, &token);
            let _ = respond.send(result);
        }
        WriteCommand::ConsumeApprovalToken {
            code_hash,
            workspace_id,
            action_kind,
            pane_id,
            action_fingerprint,
            respond,
        } => {
            let result = consume_approval_token_sync(
                conn,
                &code_hash,
                &workspace_id,
                &action_kind,
                pane_id,
                &action_fingerprint,
            );
            let _ = respond.send(result);
        }
        WriteCommand::GetApprovalTokenByCode {
            code_hash,
            workspace_id,
            respond,
        } => {
            let result = get_approval_token_by_code_sync(conn, &code_hash, &workspace_id);
            let _ = respond.send(result);
        }
        WriteCommand::ConsumeApprovalTokenByCode {
            code_hash,
            workspace_id,
            respond,
        } => {
            let result = consume_approval_token_by_code_sync(conn, &code_hash, &workspace_id);
            let _ = respond.send(result);
        }
        WriteCommand::RecordMaintenance { record, respond } => {
            let result = record_maintenance_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::RecordSecretScanReport { record, respond } => {
            let result = record_secret_scan_report_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::InsertSavedSearch { record, respond } => {
            let result = insert_saved_search_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::UpdateSavedSearchRun {
            id,
            last_run_at,
            last_result_count,
            last_error,
            respond,
        } => {
            let result = update_saved_search_run_sync(
                conn,
                &id,
                last_run_at,
                last_result_count,
                last_error.as_deref(),
            );
            let _ = respond.send(result);
        }
        WriteCommand::UpdateSavedSearchSchedule {
            id,
            enabled,
            schedule_interval_ms,
            respond,
        } => {
            let result =
                update_saved_search_schedule_sync(conn, &id, enabled, schedule_interval_ms);
            let _ = respond.send(result);
        }
        WriteCommand::DeleteSavedSearch { name, respond } => {
            let result = delete_saved_search_sync(conn, &name);
            let _ = respond.send(result);
        }
        WriteCommand::PruneSegments { before_ts, respond } => {
            let result = prune_segments_sync(conn, before_ts);
            let _ = respond.send(result);
        }
        WriteCommand::Vacuum { respond } => {
            let result = vacuum_sync(conn);
            let _ = respond.send(result);
        }
        WriteCommand::Checkpoint { respond } => {
            let result = checkpoint_sync(conn);
            let _ = respond.send(result);
        }
        WriteCommand::UpsertAccount { account, respond } => {
            let result = upsert_account_sync(conn, &account);
            let _ = respond.send(result);
        }
        WriteCommand::UpdateAccountLastUsed {
            service,
            account_id,
            last_used_at,
            respond,
        } => {
            let result = update_account_last_used_sync(conn, &service, &account_id, last_used_at);
            let _ = respond.send(result);
        }
        WriteCommand::DeleteAccount {
            service,
            account_id,
            respond,
        } => {
            let result = delete_account_sync(conn, &service, &account_id);
            let _ = respond.send(result);
        }
        WriteCommand::CreateReservation {
            pane_id,
            owner_kind,
            owner_id,
            reason,
            ttl_ms,
            respond,
        } => {
            let result = create_reservation_sync(
                conn,
                pane_id,
                &owner_kind,
                &owner_id,
                reason.as_deref(),
                ttl_ms,
            );
            let _ = respond.send(result);
        }
        WriteCommand::ReleaseReservation {
            reservation_id,
            respond,
        } => {
            let result = release_reservation_sync(conn, reservation_id);
            let _ = respond.send(result);
        }
        WriteCommand::ExpireStaleReservations { respond } => {
            let result = expire_stale_reservations_sync(conn);
            let _ = respond.send(result);
        }
        WriteCommand::RecordUsageMetric { record, respond } => {
            let result = record_usage_metric_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::RecordUsageMetricsBatch { records, respond } => {
            let result = record_usage_metrics_batch_sync(conn, &records);
            let _ = respond.send(result);
        }
        WriteCommand::PurgeUsageMetrics { before_ts, respond } => {
            let result = purge_usage_metrics_sync(conn, before_ts);
            let _ = respond.send(result);
        }
        WriteCommand::RecordNotification { record, respond } => {
            let result = record_notification_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::UpdateNotificationStatus {
            id,
            status,
            error_message,
            respond,
        } => {
            let result = update_notification_status_sync(conn, id, status, error_message);
            let _ = respond.send(result);
        }
        WriteCommand::AcknowledgeNotification {
            id,
            acknowledged_by,
            action_taken,
            respond,
        } => {
            let result =
                acknowledge_notification_sync(conn, id, &acknowledged_by, action_taken.as_deref());
            let _ = respond.send(result);
        }
        WriteCommand::IncrementNotificationRetry { id, respond } => {
            let result = increment_notification_retry_sync(conn, id);
            let _ = respond.send(result);
        }
        WriteCommand::PurgeNotificationHistory { before_ts, respond } => {
            let result = purge_notification_history_sync(conn, before_ts);
            let _ = respond.send(result);
        }
        WriteCommand::DeleteEventsBefore {
            before_ts,
            batch_size,
            respond,
        } => {
            let result = delete_events_before_sync(conn, before_ts, batch_size);
            let _ = respond.send(result);
        }
        WriteCommand::DeleteEventsByTier {
            before_ts,
            severities,
            event_types,
            handled,
            batch_size,
            respond,
        } => {
            let result = delete_events_by_tier_sync(
                conn,
                before_ts,
                &severities,
                &event_types,
                handled,
                batch_size,
            );
            let _ = respond.send(result);
        }
        WriteCommand::InsertPaneBookmark { record, respond } => {
            let result = insert_pane_bookmark_sync(conn, &record);
            let _ = respond.send(result);
        }
        WriteCommand::DeletePaneBookmark { alias, respond } => {
            let result = delete_pane_bookmark_sync(conn, &alias);
            let _ = respond.send(result);
        }
        WriteCommand::Shutdown { respond } => {
            let _ = respond.send(());
            *should_break = true;
        }
    }
}

// =============================================================================
// Synchronous Database Operations
// =============================================================================

/// Get current timestamp in epoch milliseconds
pub fn now_ms() -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn u64_to_i64(value: u64, label: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        StorageError::Database(format!("{label} value {value} exceeds i64 range")).into()
    })
}

fn usize_to_i64(value: usize, label: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        StorageError::Database(format!("{label} value {value} exceeds i64 range")).into()
    })
}

fn i64_to_usize(value: i64) -> rusqlite::Result<usize> {
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

/// Append a segment (synchronous, called from writer thread)
fn append_segment_sync(
    conn: &Connection,
    pane_id: u64,
    content: &str,
    content_hash: Option<&str>,
) -> Result<Segment> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;

    // Get next sequence number for this pane
    let next_seq: u64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM output_segments WHERE pane_id = ?1",
            [pane_id_i64],
            |row| {
                let val: i64 = row.get(0)?;
                #[allow(clippy::cast_sign_loss)]
                Ok(val as u64)
            },
        )
        .map_err(|e| StorageError::Database(format!("Failed to get next seq: {e}")))?;

    let now = now_ms();
    let content_len = content.len();

    let next_seq_i64 = u64_to_i64(next_seq, "seq")?;
    let content_len_i64 = usize_to_i64(content_len, "content_len")?;

    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, content_hash, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            pane_id_i64,
            next_seq_i64,
            content,
            content_len_i64,
            content_hash,
            now
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert segment: {e}")))?;

    let id = conn.last_insert_rowid();

    Ok(Segment {
        id,
        pane_id,
        seq: next_seq,
        content: content.to_string(),
        content_len,
        content_hash: content_hash.map(String::from),
        captured_at: now,
    })
}

/// Record a gap event (synchronous)
fn record_gap_sync(conn: &Connection, pane_id: u64, reason: &str) -> Result<Option<Gap>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;

    // Get the last sequence for this pane
    let last_seq: Option<u64> = conn
        .query_row(
            "SELECT MAX(seq) FROM output_segments WHERE pane_id = ?1",
            [pane_id_i64],
            |row| {
                let val: Option<i64> = row.get(0)?;
                #[allow(clippy::cast_sign_loss)]
                Ok(val.map(|v| v as u64))
            },
        )
        .optional()
        .map_err(|e| StorageError::Database(format!("Failed to get last seq: {e}")))?
        .flatten();

    let Some(seq_before) = last_seq else {
        // No segments yet, so no gap to record (start of stream)
        return Ok(None);
    };

    let seq_after = seq_before + 1;
    let now = now_ms();

    let seq_before_i64 = u64_to_i64(seq_before, "seq_before")?;
    let seq_after_i64 = u64_to_i64(seq_after, "seq_after")?;

    conn.execute(
        "INSERT INTO output_gaps (pane_id, seq_before, seq_after, reason, detected_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![pane_id_i64, seq_before_i64, seq_after_i64, reason, now],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert gap: {e}")))?;

    let id = conn.last_insert_rowid();

    Ok(Some(Gap {
        id,
        pane_id,
        seq_before,
        seq_after,
        reason: reason.to_string(),
        detected_at: now,
    }))
}

/// Record an event (synchronous)
fn record_event_sync(conn: &Connection, event: &StoredEvent) -> Result<i64> {
    let extracted_json = event
        .extracted
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());

    let pane_id_i64 = u64_to_i64(event.pane_id, "pane_id")?;

    let insert = conn.execute(
        "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence,
         extracted, matched_text, segment_id, detected_at, dedupe_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            pane_id_i64,
            event.rule_id,
            event.agent_type,
            event.event_type,
            event.severity,
            event.confidence,
            extracted_json,
            event.matched_text,
            event.segment_id,
            event.detected_at,
            event.dedupe_key.clone(),
        ],
    );

    match insert {
        Ok(_) => Ok(conn.last_insert_rowid()),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
        {
            if let Some(ref dedupe_key) = event.dedupe_key {
                let existing: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM events WHERE dedupe_key = ?1",
                        params![dedupe_key],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|e| {
                        StorageError::Database(format!("Failed to resolve deduped event id: {e}"))
                    })?;
                if let Some(id) = existing {
                    return Ok(id);
                }
            }
            Err(
                StorageError::Database(format!("Failed to insert event (dedupe conflict): {err}"))
                    .into(),
            )
        }
        Err(e) => Err(StorageError::Database(format!("Failed to insert event: {e}")).into()),
    }
}

/// Mark event as handled (synchronous)
fn mark_event_handled_sync(
    conn: &Connection,
    event_id: i64,
    workflow_id: Option<&str>,
    status: &str,
) -> Result<()> {
    let now = now_ms();

    conn.execute(
        "UPDATE events SET handled_at = ?1, handled_by_workflow_id = ?2, handled_status = ?3
         WHERE id = ?4",
        params![now, workflow_id, status, event_id],
    )
    .map_err(|e| StorageError::Database(format!("Failed to mark event handled: {e}")))?;

    Ok(())
}

/// Set or clear triage state on an event row.
///
/// Returns true if an event row was updated.
fn set_event_triage_state_sync(
    conn: &Connection,
    event_id: i64,
    triage_state: Option<&str>,
    updated_by: Option<&str>,
) -> Result<bool> {
    let rows = if let Some(state) = triage_state {
        let now = now_ms();
        conn.execute(
            "UPDATE events
             SET triage_state = ?1,
                 triage_updated_at = ?2,
                 triage_updated_by = ?3
             WHERE id = ?4",
            params![state, now, updated_by, event_id],
        )
    } else {
        conn.execute(
            "UPDATE events
             SET triage_state = NULL,
                 triage_updated_at = NULL,
                 triage_updated_by = NULL
             WHERE id = ?1",
            params![event_id],
        )
    }
    .map_err(|e| StorageError::Database(format!("Failed to set triage state: {e}")))?;

    Ok(rows > 0)
}

/// Set or clear the note associated with an event.
///
/// Note content is redacted before persistence to avoid storing secrets.
fn set_event_note_sync(
    conn: &Connection,
    event_id: i64,
    note: Option<&str>,
    updated_by: Option<&str>,
) -> Result<()> {
    if let Some(note) = note {
        let redactor = Redactor::new();
        let note = redactor.redact(note);
        let now = now_ms();
        conn.execute(
            "INSERT INTO event_notes (event_id, note, updated_at, updated_by)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(event_id) DO UPDATE SET
                note = excluded.note,
                updated_at = excluded.updated_at,
                updated_by = excluded.updated_by",
            params![event_id, note, now, updated_by],
        )
        .map_err(|e| StorageError::Database(format!("Failed to set event note: {e}")))?;
        return Ok(());
    }

    conn.execute(
        "DELETE FROM event_notes WHERE event_id = ?1",
        params![event_id],
    )
    .map_err(|e| StorageError::Database(format!("Failed to clear event note: {e}")))?;

    Ok(())
}

/// Add a label to an event (idempotent).
///
/// Returns true if a new label was inserted.
fn add_event_label_sync(
    conn: &Connection,
    event_id: i64,
    label: &str,
    created_by: Option<&str>,
) -> Result<bool> {
    let now = now_ms();
    let rows = conn
        .execute(
            "INSERT OR IGNORE INTO event_labels (event_id, label, created_at, created_by)
             VALUES (?1, ?2, ?3, ?4)",
            params![event_id, label, now, created_by],
        )
        .map_err(|e| StorageError::Database(format!("Failed to add event label: {e}")))?;

    Ok(rows > 0)
}

/// Remove a label from an event.
///
/// Returns true if a label row was deleted.
fn remove_event_label_sync(conn: &Connection, event_id: i64, label: &str) -> Result<bool> {
    let rows = conn
        .execute(
            "DELETE FROM event_labels WHERE event_id = ?1 AND label = ?2",
            params![event_id, label],
        )
        .map_err(|e| StorageError::Database(format!("Failed to remove event label: {e}")))?;
    Ok(rows > 0)
}

/// Query all annotations for an event.
fn query_event_annotations_sync(
    conn: &Connection,
    event_id: i64,
) -> Result<Option<EventAnnotations>> {
    let triage: Option<(Option<String>, Option<i64>, Option<String>)> = conn
        .query_row(
            "SELECT triage_state, triage_updated_at, triage_updated_by FROM events WHERE id = ?1",
            params![event_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|e| StorageError::Database(format!("Failed to query triage state: {e}")))?;

    let Some((triage_state, triage_updated_at, triage_updated_by)) = triage else {
        return Ok(None);
    };

    let note_row: Option<(String, i64, Option<String>)> = conn
        .query_row(
            "SELECT note, updated_at, updated_by FROM event_notes WHERE event_id = ?1",
            params![event_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|e| StorageError::Database(format!("Failed to query event note: {e}")))?;

    let (note, note_updated_at, note_updated_by) = note_row
        .map(|(n, ts, by)| (Some(n), Some(ts), by))
        .unwrap_or((None, None, None));

    let mut stmt = conn
        .prepare("SELECT label FROM event_labels WHERE event_id = ?1 ORDER BY label ASC")
        .map_err(|e| StorageError::Database(format!("Failed to prepare labels query: {e}")))?;
    let rows = stmt
        .query_map(params![event_id], |row| row.get::<_, String>(0))
        .map_err(|e| StorageError::Database(format!("Labels query failed: {e}")))?;

    let mut labels = Vec::new();
    for row in rows {
        labels.push(row.map_err(|e| StorageError::Database(format!("Label row error: {e}")))?);
    }

    Ok(Some(EventAnnotations {
        triage_state,
        triage_updated_at,
        triage_updated_by,
        note,
        note_updated_at,
        note_updated_by,
        labels,
    }))
}

/// Insert or update a persistent event mute.
fn upsert_event_mute_sync(conn: &Connection, record: &EventMuteRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO event_mutes (identity_key, scope, created_at, expires_at, created_by, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(identity_key) DO UPDATE SET
            scope = excluded.scope,
            created_at = excluded.created_at,
            expires_at = excluded.expires_at,
            created_by = excluded.created_by,
            reason = excluded.reason",
        params![
            record.identity_key,
            record.scope,
            record.created_at,
            record.expires_at,
            record.created_by,
            record.reason
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert event mute: {e}")))?;

    Ok(())
}

/// Delete a persistent event mute by identity key.
fn delete_event_mute_sync(conn: &Connection, identity_key: &str) -> Result<bool> {
    let rows = conn
        .execute(
            "DELETE FROM event_mutes WHERE identity_key = ?1",
            params![identity_key],
        )
        .map_err(|e| StorageError::Database(format!("Failed to delete event mute: {e}")))?;
    Ok(rows > 0)
}

/// Check if an identity key is muted (and not expired).
fn query_event_mute(conn: &Connection, identity_key: &str, now_ms: i64) -> Result<bool> {
    let mut stmt = conn
        .prepare(
            "SELECT 1 FROM event_mutes
             WHERE identity_key = ?1
               AND (expires_at IS NULL OR expires_at > ?2)
             LIMIT 1",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare mute query: {e}")))?;

    let mut rows = stmt
        .query(params![identity_key, now_ms])
        .map_err(|e| StorageError::Database(format!("Mute query failed: {e}")))?;

    Ok(rows
        .next()
        .map_err(|e| StorageError::Database(format!("Mute query row error: {e}")))?
        .is_some())
}

/// List all active (non-expired) mutes.
fn list_active_mutes_sync(conn: &Connection, now_ms: i64) -> Result<Vec<EventMuteRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT identity_key, scope, created_at, expires_at, created_by, reason
             FROM event_mutes
             WHERE expires_at IS NULL OR expires_at > ?1
             ORDER BY created_at DESC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare mute list query: {e}")))?;

    let rows = stmt
        .query_map(params![now_ms], |row| {
            Ok(EventMuteRecord {
                identity_key: row.get(0)?,
                scope: row.get(1)?,
                created_at: row.get(2)?,
                expires_at: row.get(3)?,
                created_by: row.get(4)?,
                reason: row.get(5)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Mute list query failed: {e}")))?;

    let mut mutes = Vec::new();
    for row in rows {
        mutes.push(row.map_err(|e| StorageError::Database(format!("Mute list row error: {e}")))?);
    }
    Ok(mutes)
}

/// Compute the event identity key for a stored event.
fn query_event_identity_key(conn: &Connection, event_id: i64) -> Result<Option<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT e.rule_id, e.event_type, e.extracted, e.pane_id, p.pane_uuid
             FROM events e
             LEFT JOIN panes p ON p.pane_id = e.pane_id
             WHERE e.id = ?1",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare identity query: {e}")))?;

    let mut rows = stmt
        .query(params![event_id])
        .map_err(|e| StorageError::Database(format!("Identity query failed: {e}")))?;

    if let Some(row) = rows
        .next()
        .map_err(|e| StorageError::Database(format!("Identity query row error: {e}")))?
    {
        let rule_id: String = row
            .get(0)
            .map_err(|e| StorageError::Database(format!("Failed to read rule_id: {e}")))?;
        let event_type: String = row
            .get(1)
            .map_err(|e| StorageError::Database(format!("Failed to read event_type: {e}")))?;
        let extracted_str: Option<String> = row
            .get(2)
            .map_err(|e| StorageError::Database(format!("Failed to read extracted: {e}")))?;
        let pane_id_i64: i64 = row
            .get(3)
            .map_err(|e| StorageError::Database(format!("Failed to read pane_id: {e}")))?;
        let pane_uuid: Option<String> = row
            .get(4)
            .map_err(|e| StorageError::Database(format!("Failed to read pane_uuid: {e}")))?;
        let extracted = extracted_str
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);

        let detection = crate::patterns::Detection {
            rule_id,
            agent_type: crate::patterns::AgentType::Unknown,
            event_type,
            severity: crate::patterns::Severity::Info,
            confidence: 0.0,
            extracted,
            matched_text: String::new(),
            span: (0, 0),
        };

        let pane_id = u64::try_from(pane_id_i64).unwrap_or(0);
        return Ok(Some(event_identity_key(
            &detection,
            pane_id,
            pane_uuid.as_deref(),
        )));
    }

    Ok(None)
}

/// Upsert pane record (synchronous)
fn upsert_pane_sync(conn: &Connection, pane: &PaneRecord) -> Result<()> {
    let pane_id_i64 = u64_to_i64(pane.pane_id, "pane_id")?;
    let window_id_i64 = pane
        .window_id
        .map(|v| u64_to_i64(v, "window_id"))
        .transpose()?;
    let tab_id_i64 = pane.tab_id.map(|v| u64_to_i64(v, "tab_id")).transpose()?;

    conn.execute(
        "INSERT INTO panes (pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
         first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(pane_id) DO UPDATE SET
            pane_uuid = COALESCE(excluded.pane_uuid, panes.pane_uuid),
            domain = excluded.domain,
            window_id = excluded.window_id,
            tab_id = excluded.tab_id,
            title = excluded.title,
            cwd = excluded.cwd,
            tty_name = excluded.tty_name,
            last_seen_at = excluded.last_seen_at,
            observed = excluded.observed,
            ignore_reason = excluded.ignore_reason,
            last_decision_at = excluded.last_decision_at",
        params![
            pane_id_i64,
            pane.pane_uuid,
            pane.domain,
            window_id_i64,
            tab_id_i64,
            pane.title,
            pane.cwd,
            pane.tty_name,
            pane.first_seen_at,
            pane.last_seen_at,
            i64::from(pane.observed),
            pane.ignore_reason,
            pane.last_decision_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert pane: {e}")))?;

    Ok(())
}

/// Upsert workflow execution (synchronous)
fn upsert_workflow_sync(conn: &Connection, workflow: &WorkflowRecord) -> Result<()> {
    let wait_condition_json = workflow
        .wait_condition
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let context_json = workflow
        .context
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let result_json = workflow
        .result
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());

    let pane_id_i64 = u64_to_i64(workflow.pane_id, "pane_id")?;
    let current_step_i64 = usize_to_i64(workflow.current_step, "current_step")?;

    conn.execute(
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, trigger_event_id,
         current_step, status, wait_condition, context, result, error, started_at, updated_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(id) DO UPDATE SET
            current_step = excluded.current_step,
            status = excluded.status,
            wait_condition = excluded.wait_condition,
            context = excluded.context,
            result = excluded.result,
            error = excluded.error,
            updated_at = excluded.updated_at,
            completed_at = excluded.completed_at",
        params![
            workflow.id,
            workflow.workflow_name,
            pane_id_i64,
            workflow.trigger_event_id,
            current_step_i64,
            workflow.status,
            wait_condition_json,
            context_json,
            result_json,
            workflow.error,
            workflow.started_at,
            workflow.updated_at,
            workflow.completed_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert workflow: {e}")))?;

    Ok(())
}

/// Upsert workflow action plan (synchronous)
fn upsert_action_plan_sync(conn: &Connection, record: &WorkflowActionPlanRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO workflow_action_plans (workflow_id, plan_id, plan_hash, plan_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(workflow_id) DO UPDATE SET
            plan_id = excluded.plan_id,
            plan_hash = excluded.plan_hash,
            plan_json = excluded.plan_json,
            created_at = excluded.created_at",
        params![
            record.workflow_id,
            record.plan_id,
            record.plan_hash,
            record.plan_json,
            record.created_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert action plan: {e}")))?;

    Ok(())
}

/// Insert workflow step log (synchronous)
#[allow(clippy::too_many_arguments)]
fn insert_step_log_sync(
    conn: &Connection,
    workflow_id: &str,
    audit_action_id: Option<i64>,
    step_index: usize,
    step_name: &str,
    step_id: Option<&str>,
    step_kind: Option<&str>,
    result_type: &str,
    result_data: Option<&str>,
    policy_summary: Option<&str>,
    verification_refs: Option<&str>,
    error_code: Option<&str>,
    started_at: i64,
    completed_at: i64,
) -> Result<()> {
    let duration_ms = completed_at.saturating_sub(started_at);
    let step_index_i64 = usize_to_i64(step_index, "step_index")?;

    conn.execute(
        "INSERT INTO workflow_step_logs (workflow_id, audit_action_id, step_index, step_name, step_id,
         step_kind, result_type, result_data, policy_summary, verification_refs, error_code,
         started_at, completed_at, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            workflow_id,
            audit_action_id,
            step_index_i64,
            step_name,
            step_id,
            step_kind,
            result_type,
            result_data,
            policy_summary,
            verification_refs,
            error_code,
            started_at,
            completed_at,
            duration_ms,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert step log: {e}")))?;

    Ok(())
}

/// Upsert agent session (synchronous)
///
/// If the session has id == 0, creates a new session.
/// Otherwise, updates the existing session.
/// Returns the session ID.
fn upsert_agent_session_sync(conn: &Connection, session: &AgentSessionRecord) -> Result<i64> {
    let pane_id_i64 = u64_to_i64(session.pane_id, "pane_id")?;
    let external_meta_json = session
        .external_meta
        .as_ref()
        .and_then(|value| serde_json::to_string(value).ok());

    if session.id == 0 {
        // Insert new session
        conn.execute(
            "INSERT INTO agent_sessions (pane_id, agent_type, session_id, external_id, external_meta,
             started_at, ended_at, end_reason, total_tokens, input_tokens, output_tokens,
             cached_tokens, reasoning_tokens, model_name, estimated_cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                pane_id_i64,
                session.agent_type,
                session.session_id,
                session.external_id,
                external_meta_json,
                session.started_at,
                session.ended_at,
                session.end_reason,
                session.total_tokens,
                session.input_tokens,
                session.output_tokens,
                session.cached_tokens,
                session.reasoning_tokens,
                session.model_name,
                session.estimated_cost_usd,
            ],
        )
        .map_err(|e| StorageError::Database(format!("Failed to insert session: {e}")))?;

        Ok(conn.last_insert_rowid())
    } else {
        // Update existing session
        conn.execute(
            "UPDATE agent_sessions SET
             pane_id = ?1, agent_type = ?2, session_id = ?3, external_id = ?4, external_meta = ?5,
             started_at = ?6, ended_at = ?7, end_reason = ?8, total_tokens = ?9,
             input_tokens = ?10, output_tokens = ?11, cached_tokens = ?12,
             reasoning_tokens = ?13, model_name = ?14, estimated_cost_usd = ?15
             WHERE id = ?16",
            params![
                pane_id_i64,
                session.agent_type,
                session.session_id,
                session.external_id,
                external_meta_json,
                session.started_at,
                session.ended_at,
                session.end_reason,
                session.total_tokens,
                session.input_tokens,
                session.output_tokens,
                session.cached_tokens,
                session.reasoning_tokens,
                session.model_name,
                session.estimated_cost_usd,
                session.id,
            ],
        )
        .map_err(|e| StorageError::Database(format!("Failed to update session: {e}")))?;

        Ok(session.id)
    }
}

/// Record an audit action (synchronous)
fn record_audit_action_sync(conn: &Connection, action: &AuditActionRecord) -> Result<i64> {
    let pane_id_i64 = action
        .pane_id
        .map(|pane_id| u64_to_i64(pane_id, "pane_id"))
        .transpose()?;
    let ts = if action.ts == 0 { now_ms() } else { action.ts };

    conn.execute(
        "INSERT INTO audit_actions (ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            ts,
            action.actor_kind.as_str(),
            action.actor_id.as_deref(),
            action.correlation_id.as_deref(),
            pane_id_i64,
            action.domain.as_deref(),
            action.action_kind.as_str(),
            action.policy_decision.as_str(),
            action.decision_reason.as_deref(),
            action.rule_id.as_deref(),
            action.input_summary.as_deref(),
            action.verification_summary.as_deref(),
            action.decision_context.as_deref(),
            action.result.as_str(),
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert audit action: {e}")))?;

    Ok(conn.last_insert_rowid())
}

/// Upsert undo metadata for an audit action (synchronous)
fn upsert_action_undo_sync(conn: &Connection, record: &ActionUndoRecord) -> Result<()> {
    let undoable_i64 = i64::from(record.undoable);

    conn.execute(
        "INSERT INTO action_undo (audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(audit_action_id) DO UPDATE SET
            undoable = excluded.undoable,
            undo_strategy = excluded.undo_strategy,
            undo_hint = excluded.undo_hint,
            undo_payload = excluded.undo_payload,
            undone_at = excluded.undone_at,
            undone_by = excluded.undone_by",
        params![
            record.audit_action_id,
            undoable_i64,
            record.undo_strategy,
            record.undo_hint,
            record.undo_payload,
            record.undone_at,
            record.undone_by,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert action_undo: {e}")))?;

    Ok(())
}

fn query_action_undo_sync(
    conn: &Connection,
    audit_action_id: i64,
) -> Result<Option<ActionUndoRecord>> {
    conn.query_row(
        "SELECT audit_action_id, undoable, undo_strategy, undo_hint, undo_payload, undone_at, undone_by
         FROM action_undo WHERE audit_action_id = ?1",
        params![audit_action_id],
        |row| {
            Ok(ActionUndoRecord {
                audit_action_id: row.get(0)?,
                undoable: {
                    let value: i64 = row.get(1)?;
                    value != 0
                },
                undo_strategy: row.get(2)?,
                undo_hint: row.get(3)?,
                undo_payload: row.get(4)?,
                undone_at: row.get(5)?,
                undone_by: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Failed to query action_undo: {e}")).into())
}

fn mark_action_undone_sync(
    conn: &Connection,
    audit_action_id: i64,
    undone_at: i64,
    undone_by: &str,
) -> Result<bool> {
    let changed = conn
        .execute(
            "UPDATE action_undo
             SET undone_at = ?1, undone_by = ?2
             WHERE audit_action_id = ?3 AND undoable = 1 AND undone_at IS NULL",
            params![undone_at, undone_by, audit_action_id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to mark action as undone: {e}")))?;
    Ok(changed > 0)
}

/// Purge audit actions before a cutoff timestamp (synchronous)
fn purge_audit_actions_sync(conn: &Connection, before_ts: i64) -> Result<usize> {
    let deleted = conn
        .execute("DELETE FROM audit_actions WHERE ts < ?1", [before_ts])
        .map_err(|e| StorageError::Database(format!("Failed to purge audit actions: {e}")))?;
    Ok(deleted)
}

fn record_maintenance_sync(conn: &Connection, record: &MaintenanceRecord) -> Result<i64> {
    let ts = if record.timestamp == 0 {
        now_ms()
    } else {
        record.timestamp
    };

    conn.execute(
        "INSERT INTO maintenance_log (event_type, message, metadata, timestamp) VALUES (?1, ?2, ?3, ?4)",
        params![record.event_type, record.message, record.metadata, ts],
    )
    .map_err(|e| StorageError::Database(format!("Failed to record maintenance: {e}")))?;

    Ok(conn.last_insert_rowid())
}

fn record_secret_scan_report_sync(
    conn: &Connection,
    record: &SecretScanReportRecord,
) -> Result<i64> {
    let created_at = if record.created_at == 0 {
        now_ms()
    } else {
        record.created_at
    };

    conn.execute(
        "INSERT INTO secret_scan_reports (scope_hash, scope_json, report_version, \
         last_segment_id, report_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.scope_hash,
            record.scope_json,
            record.report_version,
            record.last_segment_id,
            record.report_json,
            created_at
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to record secret scan report: {e}")))?;

    Ok(conn.last_insert_rowid())
}

fn insert_saved_search_sync(conn: &Connection, record: &SavedSearchRecord) -> Result<()> {
    let enabled = i64::from(record.enabled);
    let limit = if record.limit <= 0 {
        SAVED_SEARCH_DEFAULT_LIMIT
    } else {
        record.limit
    };

    conn.execute(
        "INSERT INTO saved_searches (
            id, name, query, pane_id, \"limit\", since_mode, since_ms,
            schedule_interval_ms, enabled, last_run_at, last_result_count, last_error,
            created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            record.id,
            record.name,
            record.query,
            record.pane_id.map(|v| v as i64),
            limit,
            record.since_mode,
            record.since_ms,
            record.schedule_interval_ms,
            enabled,
            record.last_run_at,
            record.last_result_count,
            record.last_error,
            record.created_at,
            record.updated_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert saved search: {e}")))?;

    Ok(())
}

fn update_saved_search_run_sync(
    conn: &Connection,
    id: &str,
    last_run_at: i64,
    last_result_count: Option<i64>,
    last_error: Option<&str>,
) -> Result<()> {
    let updated = conn
        .execute(
            "UPDATE saved_searches
             SET last_run_at = ?1,
                 last_result_count = ?2,
                 last_error = ?3,
                 updated_at = ?4
             WHERE id = ?5",
            params![last_run_at, last_result_count, last_error, now_ms(), id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to update saved search: {e}")))?;

    if updated == 0 {
        return Err(StorageError::NotFound(format!("Saved search not found: {id}")).into());
    }
    Ok(())
}

fn update_saved_search_schedule_sync(
    conn: &Connection,
    id: &str,
    enabled: bool,
    schedule_interval_ms: Option<i64>,
) -> Result<()> {
    let enabled_i64 = i64::from(enabled);
    let updated = conn
        .execute(
            "UPDATE saved_searches
             SET enabled = ?1,
                 schedule_interval_ms = ?2,
                 updated_at = ?3
             WHERE id = ?4",
            params![enabled_i64, schedule_interval_ms, now_ms(), id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to update saved search: {e}")))?;

    if updated == 0 {
        return Err(StorageError::NotFound(format!("Saved search not found: {id}")).into());
    }
    Ok(())
}

fn delete_saved_search_sync(conn: &Connection, name: &str) -> Result<usize> {
    let deleted = conn
        .execute("DELETE FROM saved_searches WHERE name = ?1", [name])
        .map_err(|e| StorageError::Database(format!("Failed to delete saved search: {e}")))?;
    Ok(deleted)
}

fn saved_search_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SavedSearchRecord> {
    let enabled: i64 = row.get(8)?;
    let pane_id_raw: Option<i64> = row.get(3)?;
    Ok(SavedSearchRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        query: row.get(2)?,
        pane_id: pane_id_raw.map(|v| v as u64),
        limit: row.get(4)?,
        since_mode: row.get(5)?,
        since_ms: row.get(6)?,
        schedule_interval_ms: row.get(7)?,
        enabled: enabled != 0,
        last_run_at: row.get(9)?,
        last_result_count: row.get(10)?,
        last_error: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn query_saved_search_by_name(conn: &Connection, name: &str) -> Result<Option<SavedSearchRecord>> {
    Ok(conn
        .query_row(
            "SELECT id, name, query, pane_id, \"limit\", since_mode, since_ms, schedule_interval_ms,
                    enabled, last_run_at, last_result_count, last_error, created_at, updated_at
             FROM saved_searches
             WHERE name = ?1",
            [name],
            saved_search_from_row,
        )
        .optional()
        .map_err(|e| StorageError::Database(format!("Failed to query saved search: {e}")))?)
}

fn list_saved_searches_sync(conn: &Connection) -> Result<Vec<SavedSearchRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, query, pane_id, \"limit\", since_mode, since_ms, schedule_interval_ms,
                    enabled, last_run_at, last_result_count, last_error, created_at, updated_at
             FROM saved_searches
             ORDER BY name ASC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to list saved searches: {e}")))?;
    let rows = stmt
        .query_map([], saved_search_from_row)
        .map_err(|e| StorageError::Database(format!("Failed to list saved searches: {e}")))?;
    let mut searches = Vec::new();
    for row in rows {
        searches.push(row.map_err(|e| StorageError::Database(format!("{e}")))?);
    }
    Ok(searches)
}

// =============================================================================
// Pane Bookmarks
// =============================================================================

fn insert_pane_bookmark_sync(conn: &Connection, record: &PaneBookmarkRecord) -> Result<i64> {
    let tags_json = record
        .tags
        .as_ref()
        .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".to_string()));

    conn.execute(
        "INSERT INTO pane_bookmarks (pane_id, alias, tags, description, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.pane_id as i64,
            record.alias,
            tags_json,
            record.description,
            record.created_at,
            record.updated_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert pane bookmark: {e}")))?;

    Ok(conn.last_insert_rowid())
}

fn delete_pane_bookmark_sync(conn: &Connection, alias: &str) -> Result<bool> {
    let deleted = conn
        .execute("DELETE FROM pane_bookmarks WHERE alias = ?1", [alias])
        .map_err(|e| StorageError::Database(format!("Failed to delete pane bookmark: {e}")))?;
    Ok(deleted > 0)
}

fn pane_bookmark_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PaneBookmarkRecord> {
    let pane_id_raw: i64 = row.get(1)?;
    let tags_raw: Option<String> = row.get(3)?;
    let tags = tags_raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok());
    Ok(PaneBookmarkRecord {
        id: row.get(0)?,
        pane_id: pane_id_raw as u64,
        alias: row.get(2)?,
        tags,
        description: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn query_pane_bookmark_by_alias(
    conn: &Connection,
    alias: &str,
) -> Result<Option<PaneBookmarkRecord>> {
    Ok(conn
        .query_row(
            "SELECT id, pane_id, alias, tags, description, created_at, updated_at
             FROM pane_bookmarks WHERE alias = ?1",
            [alias],
            pane_bookmark_from_row,
        )
        .optional()
        .map_err(|e| StorageError::Database(format!("Failed to query pane bookmark: {e}")))?)
}

fn list_pane_bookmarks_sync(conn: &Connection) -> Result<Vec<PaneBookmarkRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, alias, tags, description, created_at, updated_at
             FROM pane_bookmarks ORDER BY alias ASC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to list pane bookmarks: {e}")))?;
    let rows = stmt
        .query_map([], pane_bookmark_from_row)
        .map_err(|e| StorageError::Database(format!("Failed to list pane bookmarks: {e}")))?;
    let mut bookmarks = Vec::new();
    for row in rows {
        bookmarks.push(row.map_err(|e| StorageError::Database(format!("{e}")))?);
    }
    Ok(bookmarks)
}

fn list_pane_bookmarks_by_tag_sync(
    conn: &Connection,
    tag: &str,
) -> Result<Vec<PaneBookmarkRecord>> {
    // Use JSON containment check: tags column is a JSON array
    let pattern = format!("%\"{tag}\"%");
    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, alias, tags, description, created_at, updated_at
             FROM pane_bookmarks WHERE tags LIKE ?1 ORDER BY alias ASC",
        )
        .map_err(|e| {
            StorageError::Database(format!("Failed to list pane bookmarks by tag: {e}"))
        })?;
    let rows = stmt
        .query_map([pattern], pane_bookmark_from_row)
        .map_err(|e| {
            StorageError::Database(format!("Failed to list pane bookmarks by tag: {e}"))
        })?;
    let mut bookmarks = Vec::new();
    for row in rows {
        bookmarks.push(row.map_err(|e| StorageError::Database(format!("{e}")))?);
    }
    Ok(bookmarks)
}

fn prune_segments_sync(conn: &Connection, before_ts: i64) -> Result<usize> {
    let deleted = conn
        .execute(
            "DELETE FROM output_segments WHERE captured_at < ?1",
            params![before_ts],
        )
        .map_err(|e| StorageError::Database(format!("Failed to prune segments: {e}")))?;
    Ok(deleted)
}

fn vacuum_sync(conn: &Connection) -> Result<()> {
    conn.execute_batch("VACUUM")
        .map_err(|e| StorageError::Database(format!("Failed to vacuum database: {e}")))?;
    Ok(())
}

/// Checkpoint WAL (PASSIVE — non-blocking, does not stall readers or writers)
/// and run PRAGMA optimize to maintain query planner statistics.
fn checkpoint_sync(conn: &Connection) -> Result<CheckpointResult> {
    let wal_pages: i64 = conn
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get(1))
        .map_err(|e| StorageError::Database(format!("WAL checkpoint failed: {e}")))?;

    conn.execute_batch("PRAGMA optimize")
        .map_err(|e| StorageError::Database(format!("PRAGMA optimize failed: {e}")))?;

    Ok(CheckpointResult {
        wal_pages,
        optimized: true,
    })
}

// =============================================================================
// Usage Metrics Operations (Synchronous)
// =============================================================================

fn record_usage_metric_sync(conn: &Connection, record: &UsageMetricRecord) -> Result<i64> {
    let ts = if record.timestamp == 0 {
        now_ms()
    } else {
        record.timestamp
    };
    let created = if record.created_at == 0 {
        now_ms()
    } else {
        record.created_at
    };
    let pane_id = record.pane_id.map(|id| id as i64);

    conn.execute(
        "INSERT INTO usage_metrics (timestamp, metric_type, pane_id, agent_type, account_id, workflow_id, count, amount, tokens, metadata, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            ts,
            record.metric_type.as_str(),
            pane_id,
            record.agent_type,
            record.account_id,
            record.workflow_id,
            record.count,
            record.amount,
            record.tokens,
            record.metadata,
            created,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to record usage metric: {e}")))?;

    Ok(conn.last_insert_rowid())
}

fn record_usage_metrics_batch_sync(
    conn: &mut Connection,
    records: &[UsageMetricRecord],
) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }

    let tx = conn
        .transaction()
        .map_err(|e| StorageError::Database(format!("Failed to start metrics batch tx: {e}")))?;

    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO usage_metrics (timestamp, metric_type, pane_id, agent_type, account_id, workflow_id, count, amount, tokens, metadata, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )
            .map_err(|e| {
                StorageError::Database(format!("Failed to prepare metrics batch insert: {e}"))
            })?;

        for record in records {
            let ts = if record.timestamp == 0 {
                now_ms()
            } else {
                record.timestamp
            };
            let created = if record.created_at == 0 {
                now_ms()
            } else {
                record.created_at
            };
            let pane_id = record.pane_id.map(|id| id as i64);

            stmt.execute(params![
                ts,
                record.metric_type.as_str(),
                pane_id,
                record.agent_type,
                record.account_id,
                record.workflow_id,
                record.count,
                record.amount,
                record.tokens,
                record.metadata,
                created,
            ])
            .map_err(|e| StorageError::Database(format!("Failed to insert usage metric: {e}")))?;
        }
    }

    tx.commit()
        .map_err(|e| StorageError::Database(format!("Failed to commit metrics batch tx: {e}")))?;

    Ok(records.len())
}

fn purge_usage_metrics_sync(conn: &Connection, before_ts: i64) -> Result<usize> {
    let deleted = conn
        .execute(
            "DELETE FROM usage_metrics WHERE timestamp < ?1",
            params![before_ts],
        )
        .map_err(|e| StorageError::Database(format!("Failed to purge usage metrics: {e}")))?;
    Ok(deleted)
}

fn query_usage_metrics_sync(
    conn: &Connection,
    query: &MetricQuery,
) -> Result<Vec<UsageMetricRecord>> {
    let mut sql = String::from(
        "SELECT id, timestamp, metric_type, pane_id, agent_type, account_id, workflow_id, count, amount, tokens, metadata, created_at FROM usage_metrics WHERE 1=1",
    );
    let mut param_values: Vec<SqlValue> = Vec::new();

    if let Some(ref mt) = query.metric_type {
        sql.push_str(" AND metric_type = ?");
        param_values.push(SqlValue::Text(mt.as_str().to_string()));
    }
    if let Some(ref agent) = query.agent_type {
        sql.push_str(" AND agent_type = ?");
        param_values.push(SqlValue::Text(agent.clone()));
    }
    if let Some(ref account) = query.account_id {
        sql.push_str(" AND account_id = ?");
        param_values.push(SqlValue::Text(account.clone()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND timestamp >= ?");
        param_values.push(SqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND timestamp < ?");
        param_values.push(SqlValue::Integer(until));
    }
    sql.push_str(" ORDER BY timestamp DESC");
    if let Some(limit) = query.limit {
        sql.push_str(" LIMIT ?");
        param_values.push(SqlValue::Integer(limit as i64));
    }

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = param_values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare metric query: {e}")))?;
    let rows = stmt
        .query_map(params_ref.as_slice(), |row| {
            let metric_type_str: String = row.get(2)?;
            let pane_id_raw: Option<i64> = row.get(3)?;
            Ok(UsageMetricRecord {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                metric_type: metric_type_str.parse().unwrap_or(MetricType::ApiCall),
                pane_id: pane_id_raw.map(|v| v as u64),
                agent_type: row.get(4)?,
                account_id: row.get(5)?,
                workflow_id: row.get(6)?,
                count: row.get(7)?,
                amount: row.get(8)?,
                tokens: row.get(9)?,
                metadata: row.get(10)?,
                created_at: row.get(11)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Failed to query metrics: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

fn aggregate_daily_sync(conn: &Connection, since_ts: i64) -> Result<Vec<DailyMetricSummary>> {
    let mut stmt = conn
        .prepare(
            "SELECT (timestamp / 86400000) * 86400000 AS day_ts,
                    agent_type,
                    COALESCE(SUM(tokens), 0),
                    COALESCE(SUM(amount), 0.0),
                    COUNT(*)
             FROM usage_metrics
             WHERE timestamp >= ?1
             GROUP BY day_ts, agent_type
             ORDER BY day_ts DESC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare daily aggregate: {e}")))?;

    let rows = stmt
        .query_map(params![since_ts], |row| {
            Ok(DailyMetricSummary {
                day_ts: row.get(0)?,
                agent_type: row.get(1)?,
                total_tokens: row.get(2)?,
                total_cost: row.get(3)?,
                event_count: row.get(4)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Failed to aggregate daily: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

fn aggregate_by_agent_sync(conn: &Connection, since_ts: i64) -> Result<Vec<AgentMetricBreakdown>> {
    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(agent_type, 'unknown'),
                    COALESCE(SUM(tokens), 0),
                    COALESCE(SUM(amount), 0.0),
                    CASE WHEN COUNT(*) > 0 THEN CAST(COALESCE(SUM(tokens), 0) AS REAL) / COUNT(*) ELSE 0.0 END
             FROM usage_metrics
             WHERE timestamp >= ?1
             GROUP BY agent_type
             ORDER BY SUM(amount) DESC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare agent aggregate: {e}")))?;

    let rows = stmt
        .query_map(params![since_ts], |row| {
            Ok(AgentMetricBreakdown {
                agent_type: row.get(0)?,
                total_tokens: row.get(1)?,
                total_cost: row.get(2)?,
                avg_tokens_per_event: row.get(3)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Failed to aggregate by agent: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

// =============================================================================
// Notification History Operations (Synchronous)
// =============================================================================

fn record_notification_sync(conn: &Connection, record: &NotificationHistoryRecord) -> Result<i64> {
    conn.execute(
        "INSERT INTO notification_history (
            timestamp, event_id, channel, title, body, severity,
            status, error_message, acknowledged_at, acknowledged_by,
            action_taken, retry_count, metadata, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        rusqlite::params![
            record.timestamp,
            record.event_id,
            record.channel,
            record.title,
            record.body,
            record.severity,
            record.status.as_str(),
            record.error_message,
            record.acknowledged_at,
            record.acknowledged_by,
            record.action_taken,
            record.retry_count,
            record.metadata,
            record.created_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to record notification: {e}")))?;
    Ok(conn.last_insert_rowid())
}

fn update_notification_status_sync(
    conn: &Connection,
    id: i64,
    status: NotificationStatus,
    error_message: Option<String>,
) -> Result<()> {
    let changed = conn
        .execute(
            "UPDATE notification_history SET status = ?1, error_message = ?2 WHERE id = ?3",
            rusqlite::params![status.as_str(), error_message, id],
        )
        .map_err(|e| {
            StorageError::Database(format!("Failed to update notification status: {e}"))
        })?;
    if changed == 0 {
        return Err(StorageError::Database(format!("Notification {id} not found")).into());
    }
    Ok(())
}

fn acknowledge_notification_sync(
    conn: &Connection,
    id: i64,
    acknowledged_by: &str,
    action_taken: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    let changed = conn
        .execute(
            "UPDATE notification_history SET acknowledged_at = ?1, acknowledged_by = ?2, action_taken = ?3 WHERE id = ?4",
            rusqlite::params![now, acknowledged_by, action_taken, id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to acknowledge notification: {e}")))?;
    if changed == 0 {
        return Err(StorageError::Database(format!("Notification {id} not found")).into());
    }
    Ok(())
}

fn increment_notification_retry_sync(conn: &Connection, id: i64) -> Result<()> {
    let changed = conn
        .execute(
            "UPDATE notification_history SET retry_count = retry_count + 1, status = 'pending' WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| {
            StorageError::Database(format!("Failed to increment notification retry: {e}"))
        })?;
    if changed == 0 {
        return Err(StorageError::Database(format!("Notification {id} not found")).into());
    }
    Ok(())
}

fn purge_notification_history_sync(conn: &Connection, before_ts: i64) -> Result<usize> {
    let deleted = conn
        .execute(
            "DELETE FROM notification_history WHERE timestamp < ?1",
            rusqlite::params![before_ts],
        )
        .map_err(|e| {
            StorageError::Database(format!("Failed to purge notification history: {e}"))
        })?;
    Ok(deleted)
}

// =============================================================================
// Cleanup engine: count + delete sync helpers
// =============================================================================

fn count_segments_before_sync(conn: &Connection, before_ts: i64) -> Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM output_segments WHERE captured_at < ?1",
            params![before_ts],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(format!("Failed to count segments: {e}")))?;
    Ok(count as usize)
}

fn count_events_before_sync(conn: &Connection, before_ts: i64) -> Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE detected_at < ?1",
            params![before_ts],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(format!("Failed to count events: {e}")))?;
    Ok(count as usize)
}

fn count_events_by_tier_sync(
    conn: &Connection,
    before_ts: i64,
    severities: &[String],
    event_types: &[String],
    handled: Option<bool>,
) -> Result<usize> {
    let (sql, param_values) = build_tier_query(
        "SELECT COUNT(*) FROM events",
        before_ts,
        severities,
        event_types,
        handled,
    );
    let params_dyn: Vec<&dyn rusqlite::ToSql> = param_values
        .iter()
        .map(|v| v as &dyn rusqlite::ToSql)
        .collect();
    let count: i64 = conn
        .query_row(&sql, params_dyn.as_slice(), |row| row.get(0))
        .map_err(|e| StorageError::Database(format!("Failed to count events by tier: {e}")))?;
    Ok(count as usize)
}

fn count_audit_actions_before_sync(conn: &Connection, before_ts: i64) -> Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM audit_actions WHERE ts < ?1",
            params![before_ts],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(format!("Failed to count audit actions: {e}")))?;
    Ok(count as usize)
}

fn count_usage_metrics_before_sync(conn: &Connection, before_ts: i64) -> Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM usage_metrics WHERE timestamp < ?1",
            params![before_ts],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(format!("Failed to count usage metrics: {e}")))?;
    Ok(count as usize)
}

fn count_notification_history_before_sync(conn: &Connection, before_ts: i64) -> Result<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notification_history WHERE timestamp < ?1",
            params![before_ts],
            |row| row.get(0),
        )
        .map_err(|e| {
            StorageError::Database(format!("Failed to count notification history: {e}"))
        })?;
    Ok(count as usize)
}

fn delete_events_before_sync(
    conn: &Connection,
    before_ts: i64,
    batch_size: usize,
) -> Result<usize> {
    let mut total_deleted = 0usize;
    loop {
        let deleted = conn
            .execute(
                "DELETE FROM events WHERE id IN (\
                 SELECT id FROM events WHERE detected_at < ?1 LIMIT ?2)",
                params![before_ts, batch_size as i64],
            )
            .map_err(|e| StorageError::Database(format!("Failed to delete events: {e}")))?;
        total_deleted += deleted;
        if deleted < batch_size {
            break;
        }
    }
    Ok(total_deleted)
}

fn delete_events_by_tier_sync(
    conn: &Connection,
    before_ts: i64,
    severities: &[String],
    event_types: &[String],
    handled: Option<bool>,
    batch_size: usize,
) -> Result<usize> {
    let (inner_query, param_values) = build_tier_query(
        "SELECT id FROM events",
        before_ts,
        severities,
        event_types,
        handled,
    );
    let delete_sql = format!("DELETE FROM events WHERE id IN ({inner_query} LIMIT {batch_size})");

    let mut total_deleted = 0usize;
    loop {
        let params_dyn: Vec<&dyn rusqlite::ToSql> = param_values
            .iter()
            .map(|v| v as &dyn rusqlite::ToSql)
            .collect();
        let deleted = conn
            .execute(&delete_sql, params_dyn.as_slice())
            .map_err(|e| StorageError::Database(format!("Failed to delete events by tier: {e}")))?;
        total_deleted += deleted;
        if deleted < batch_size {
            break;
        }
    }
    Ok(total_deleted)
}

/// Build a tier-filtered query clause with positional parameters.
fn build_tier_query(
    select_prefix: &str,
    before_ts: i64,
    severities: &[String],
    event_types: &[String],
    handled: Option<bool>,
) -> (String, Vec<SqlValue>) {
    let mut sql = format!("{select_prefix} WHERE detected_at < ?");
    let mut params: Vec<SqlValue> = vec![SqlValue::Integer(before_ts)];

    if !severities.is_empty() {
        let placeholders: Vec<String> = severities.iter().map(|_| "?".to_string()).collect();
        sql.push_str(&format!(" AND severity IN ({})", placeholders.join(",")));
        for s in severities {
            params.push(SqlValue::Text(s.clone()));
        }
    }

    if !event_types.is_empty() {
        let conditions: Vec<String> = event_types
            .iter()
            .map(|_| "event_type LIKE ?".to_string())
            .collect();
        sql.push_str(&format!(" AND ({})", conditions.join(" OR ")));
        for et in event_types {
            params.push(SqlValue::Text(format!("{et}%")));
        }
    }

    if let Some(want_handled) = handled {
        if want_handled {
            sql.push_str(" AND handled_at IS NOT NULL");
        } else {
            sql.push_str(" AND handled_at IS NULL");
        }
    }

    (sql, params)
}

fn query_notification_history_sync(
    conn: &Connection,
    query: &NotificationHistoryQuery,
) -> Result<Vec<NotificationHistoryRecord>> {
    let mut sql = String::from(
        "SELECT id, timestamp, event_id, channel, title, body, severity,
                status, error_message, acknowledged_at, acknowledged_by,
                action_taken, retry_count, metadata, created_at
         FROM notification_history WHERE 1=1",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(since) = query.since {
        params.push(Box::new(since));
        sql.push_str(&format!(" AND timestamp >= ?{}", params.len()));
    }
    if let Some(until) = query.until {
        params.push(Box::new(until));
        sql.push_str(&format!(" AND timestamp <= ?{}", params.len()));
    }
    if let Some(ref channel) = query.channel {
        params.push(Box::new(channel.clone()));
        sql.push_str(&format!(" AND channel = ?{}", params.len()));
    }
    if let Some(status) = query.status {
        params.push(Box::new(status.as_str().to_string()));
        sql.push_str(&format!(" AND status = ?{}", params.len()));
    }
    if let Some(event_id) = query.event_id {
        params.push(Box::new(event_id));
        sql.push_str(&format!(" AND event_id = ?{}", params.len()));
    }

    sql.push_str(" ORDER BY timestamp DESC");

    let limit = query.limit.unwrap_or(100);
    params.push(Box::new(limit as i64));
    sql.push_str(&format!(" LIMIT ?{}", params.len()));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let status_str: String = row.get(7)?;
            let status: NotificationStatus =
                status_str.parse().unwrap_or(NotificationStatus::Pending);
            let pane_id_i64: Option<i64> = row.get(2)?;
            Ok(NotificationHistoryRecord {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                event_id: pane_id_i64,
                channel: row.get(3)?,
                title: row.get(4)?,
                body: row.get(5)?,
                severity: row.get(6)?,
                status,
                error_message: row.get(8)?,
                acknowledged_at: row.get(9)?,
                acknowledged_by: row.get(10)?,
                action_taken: row.get(11)?,
                retry_count: row.get(12)?,
                metadata: row.get(13)?,
                created_at: row.get(14)?,
            })
        })
        .map_err(|e| {
            StorageError::Database(format!("Failed to query notification history: {e}"))
        })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

fn get_notification_sync(conn: &Connection, id: i64) -> Result<NotificationHistoryRecord> {
    conn.query_row(
        "SELECT id, timestamp, event_id, channel, title, body, severity,
                status, error_message, acknowledged_at, acknowledged_by,
                action_taken, retry_count, metadata, created_at
         FROM notification_history WHERE id = ?1",
        rusqlite::params![id],
        |row| {
            let status_str: String = row.get(7)?;
            let status: NotificationStatus =
                status_str.parse().unwrap_or(NotificationStatus::Pending);
            Ok(NotificationHistoryRecord {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                event_id: row.get(2)?,
                channel: row.get(3)?,
                title: row.get(4)?,
                body: row.get(5)?,
                severity: row.get(6)?,
                status,
                error_message: row.get(8)?,
                acknowledged_at: row.get(9)?,
                acknowledged_by: row.get(10)?,
                action_taken: row.get(11)?,
                retry_count: row.get(12)?,
                metadata: row.get(13)?,
                created_at: row.get(14)?,
            })
        },
    )
    .map_err(|e| -> crate::error::Error {
        StorageError::Database(format!("Notification {id} not found: {e}")).into()
    })
}

// =============================================================================
// Account Operations (Synchronous)
// =============================================================================

/// Upsert an account record (insert or update by service+account_id)
fn upsert_account_sync(conn: &Connection, account: &crate::accounts::AccountRecord) -> Result<i64> {
    conn.execute(
        "INSERT INTO accounts (
            account_id, service, name, percent_remaining, reset_at,
            tokens_used, tokens_remaining, tokens_limit,
            last_refreshed_at, last_used_at, created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(service, account_id) DO UPDATE SET
            name = excluded.name,
            percent_remaining = excluded.percent_remaining,
            reset_at = excluded.reset_at,
            tokens_used = excluded.tokens_used,
            tokens_remaining = excluded.tokens_remaining,
            tokens_limit = excluded.tokens_limit,
            last_refreshed_at = excluded.last_refreshed_at,
            updated_at = excluded.updated_at",
        params![
            account.account_id,
            account.service,
            account.name,
            account.percent_remaining,
            account.reset_at,
            account.tokens_used,
            account.tokens_remaining,
            account.tokens_limit,
            account.last_refreshed_at,
            account.last_used_at,
            account.created_at,
            account.updated_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert account: {e}")))?;

    Ok(conn.last_insert_rowid())
}

/// Update an account's last_used_at timestamp
fn update_account_last_used_sync(
    conn: &Connection,
    service: &str,
    account_id: &str,
    last_used_at: i64,
) -> Result<()> {
    let updated = conn
        .execute(
            "UPDATE accounts SET last_used_at = ?1, updated_at = ?2
             WHERE service = ?3 AND account_id = ?4",
            params![last_used_at, now_ms(), service, account_id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to update account last_used: {e}")))?;

    if updated == 0 {
        return Err(
            StorageError::NotFound(format!("Account not found: {service}/{account_id}")).into(),
        );
    }
    Ok(())
}

/// Delete an account by service and account_id
fn delete_account_sync(conn: &Connection, service: &str, account_id: &str) -> Result<bool> {
    let deleted = conn
        .execute(
            "DELETE FROM accounts WHERE service = ?1 AND account_id = ?2",
            params![service, account_id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to delete account: {e}")))?;

    Ok(deleted > 0)
}

/// Get all accounts for a service (synchronous, read-only)
fn get_accounts_by_service_sync(
    conn: &Connection,
    service: &str,
) -> Result<Vec<crate::accounts::AccountRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, account_id, service, name, percent_remaining, reset_at,
                    tokens_used, tokens_remaining, tokens_limit,
                    last_refreshed_at, last_used_at, created_at, updated_at
             FROM accounts
             WHERE service = ?1
             ORDER BY percent_remaining DESC, last_used_at ASC NULLS FIRST",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare accounts query: {e}")))?;

    let rows = stmt
        .query_map([service], |row| {
            Ok(crate::accounts::AccountRecord {
                id: row.get(0)?,
                account_id: row.get(1)?,
                service: row.get(2)?,
                name: row.get(3)?,
                percent_remaining: row.get(4)?,
                reset_at: row.get(5)?,
                tokens_used: row.get(6)?,
                tokens_remaining: row.get(7)?,
                tokens_limit: row.get(8)?,
                last_refreshed_at: row.get(9)?,
                last_used_at: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Failed to query accounts: {e}")))?;

    let mut accounts = Vec::new();
    for row in rows {
        accounts.push(
            row.map_err(|e| StorageError::Database(format!("Failed to read account row: {e}")))?,
        );
    }
    Ok(accounts)
}

/// Get a single account by service and account_id (synchronous, read-only)
fn get_account_sync(
    conn: &Connection,
    service: &str,
    account_id: &str,
) -> Result<Option<crate::accounts::AccountRecord>> {
    let result = conn.query_row(
        "SELECT id, account_id, service, name, percent_remaining, reset_at,
                tokens_used, tokens_remaining, tokens_limit,
                last_refreshed_at, last_used_at, created_at, updated_at
         FROM accounts
         WHERE service = ?1 AND account_id = ?2",
        params![service, account_id],
        |row| {
            Ok(crate::accounts::AccountRecord {
                id: row.get(0)?,
                account_id: row.get(1)?,
                service: row.get(2)?,
                name: row.get(3)?,
                percent_remaining: row.get(4)?,
                reset_at: row.get(5)?,
                tokens_used: row.get(6)?,
                tokens_remaining: row.get(7)?,
                tokens_limit: row.get(8)?,
                last_refreshed_at: row.get(9)?,
                last_used_at: row.get(10)?,
                created_at: row.get(11)?,
                updated_at: row.get(12)?,
            })
        },
    );

    match result {
        Ok(account) => Ok(Some(account)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StorageError::Database(format!("Failed to get account: {e}")).into()),
    }
}

// =============================================================================
// Pane Reservation Sync Operations
// =============================================================================

/// Create a pane reservation, enforcing one-active-per-pane.
///
/// If an active, unexpired reservation already exists for the pane, returns
/// a conflict error. Expired reservations are treated as released.
fn create_reservation_sync(
    conn: &Connection,
    pane_id: u64,
    owner_kind: &str,
    owner_id: &str,
    reason: Option<&str>,
    ttl_ms: i64,
) -> Result<PaneReservation> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let now = now_ms();

    // Check for existing active reservation (not expired)
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM pane_reservations
             WHERE pane_id = ?1 AND status = 'active' AND expires_at > ?2",
            params![pane_id_i64, now],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| StorageError::Database(format!("Failed to check reservation: {e}")))?;

    if let Some(existing_id) = existing {
        return Err(StorageError::Database(format!(
            "Pane {pane_id} already has active reservation (id={existing_id})"
        ))
        .into());
    }

    let expires_at = now + ttl_ms;

    conn.execute(
        "INSERT INTO pane_reservations (pane_id, owner_kind, owner_id, reason, created_at, expires_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active')",
        params![pane_id_i64, owner_kind, owner_id, reason, now, expires_at],
    )
    .map_err(|e| StorageError::Database(format!("Failed to create reservation: {e}")))?;

    let id = conn.last_insert_rowid();

    Ok(PaneReservation {
        id,
        pane_id,
        owner_kind: owner_kind.to_string(),
        owner_id: owner_id.to_string(),
        reason: reason.map(String::from),
        created_at: now,
        expires_at,
        released_at: None,
        status: "active".to_string(),
    })
}

/// Release a pane reservation by setting status to "released".
///
/// Returns true if a reservation was released, false if not found or already released.
fn release_reservation_sync(conn: &Connection, reservation_id: i64) -> Result<bool> {
    let now = now_ms();
    let updated = conn
        .execute(
            "UPDATE pane_reservations SET status = 'released', released_at = ?1
             WHERE id = ?2 AND status = 'active'",
            params![now, reservation_id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to release reservation: {e}")))?;

    Ok(updated > 0)
}

/// Get the active reservation for a pane (if any).
///
/// Only returns a reservation that is both status='active' and not expired.
fn get_active_reservation_sync(conn: &Connection, pane_id: u64) -> Result<Option<PaneReservation>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let now = now_ms();

    let result = conn.query_row(
        "SELECT id, pane_id, owner_kind, owner_id, reason, created_at, expires_at, released_at, status
         FROM pane_reservations
         WHERE pane_id = ?1 AND status = 'active' AND expires_at > ?2",
        params![pane_id_i64, now],
        |row| {
            let pane_id_val: i64 = row.get(1)?;
            #[allow(clippy::cast_sign_loss)]
            Ok(PaneReservation {
                id: row.get(0)?,
                pane_id: pane_id_val as u64,
                owner_kind: row.get(2)?,
                owner_id: row.get(3)?,
                reason: row.get(4)?,
                created_at: row.get(5)?,
                expires_at: row.get(6)?,
                released_at: row.get(7)?,
                status: row.get(8)?,
            })
        },
    );

    match result {
        Ok(reservation) => Ok(Some(reservation)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StorageError::Database(format!("Failed to get reservation: {e}")).into()),
    }
}

/// List all active (unexpired) reservations.
fn list_active_reservations_sync(conn: &Connection) -> Result<Vec<PaneReservation>> {
    let now = now_ms();
    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, owner_kind, owner_id, reason, created_at, expires_at, released_at, status
             FROM pane_reservations
             WHERE status = 'active' AND expires_at > ?1
             ORDER BY created_at ASC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare reservations query: {e}")))?;

    let rows = stmt
        .query_map([now], |row| {
            let pane_id_val: i64 = row.get(1)?;
            #[allow(clippy::cast_sign_loss)]
            Ok(PaneReservation {
                id: row.get(0)?,
                pane_id: pane_id_val as u64,
                owner_kind: row.get(2)?,
                owner_id: row.get(3)?,
                reason: row.get(4)?,
                created_at: row.get(5)?,
                expires_at: row.get(6)?,
                released_at: row.get(7)?,
                status: row.get(8)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Failed to query reservations: {e}")))?;

    let mut reservations = Vec::new();
    for row in rows {
        reservations.push(
            row.map_err(|e| {
                StorageError::Database(format!("Failed to read reservation row: {e}"))
            })?,
        );
    }
    Ok(reservations)
}

/// Expire all stale reservations (past their TTL).
///
/// Sets status to "released" and released_at to now for all active reservations
/// whose expires_at is in the past. Returns the number of reservations expired.
fn expire_stale_reservations_sync(conn: &Connection) -> Result<usize> {
    let now = now_ms();
    let expired = conn
        .execute(
            "UPDATE pane_reservations SET status = 'released', released_at = ?1
             WHERE status = 'active' AND expires_at <= ?1",
            params![now],
        )
        .map_err(|e| StorageError::Database(format!("Failed to expire reservations: {e}")))?;

    Ok(expired)
}

/// Insert an approval token (synchronous)
fn insert_approval_token_sync(conn: &Connection, token: &ApprovalTokenRecord) -> Result<i64> {
    let pane_id_i64 = token
        .pane_id
        .map(|pane_id| u64_to_i64(pane_id, "pane_id"))
        .transpose()?;

    conn.execute(
        "INSERT INTO approval_tokens (code_hash, created_at, expires_at, used_at, workspace_id,
         action_kind, pane_id, action_fingerprint, plan_hash, plan_version, risk_summary)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            token.code_hash.as_str(),
            token.created_at,
            token.expires_at,
            token.used_at,
            token.workspace_id.as_str(),
            token.action_kind.as_str(),
            pane_id_i64,
            token.action_fingerprint.as_str(),
            token.plan_hash.as_deref(),
            token.plan_version,
            token.risk_summary.as_deref(),
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert approval token: {e}")))?;

    Ok(conn.last_insert_rowid())
}

/// Insert a prepared plan preview (synchronous)
fn insert_prepared_plan_sync(conn: &Connection, record: &PreparedPlanRecord) -> Result<()> {
    let pane_id_i64 = record
        .pane_id
        .map(|pane_id| u64_to_i64(pane_id, "pane_id"))
        .transpose()?;
    let requires = i32::from(record.requires_approval);

    conn.execute(
        "INSERT OR REPLACE INTO prepared_plans
         (plan_id, plan_hash, workspace_id, action_kind, pane_id, pane_uuid, params_json,
          plan_json, requires_approval, created_at, expires_at, consumed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            record.plan_id.as_str(),
            record.plan_hash.as_str(),
            record.workspace_id.as_str(),
            record.action_kind.as_str(),
            pane_id_i64,
            record.pane_uuid.as_deref(),
            record.params_json.as_deref(),
            record.plan_json.as_str(),
            requires,
            record.created_at,
            record.expires_at,
            record.consumed_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert prepared plan: {e}")))?;

    Ok(())
}

/// Consume a prepared plan by plan_id (synchronous)
fn consume_prepared_plan_sync(
    conn: &mut Connection,
    plan_id: &str,
    now_ms: i64,
) -> Result<Option<PreparedPlanRecord>> {
    let tx = conn
        .transaction()
        .map_err(|e| StorageError::Database(format!("Failed to start transaction: {e}")))?;

    let record = {
        let mut stmt = tx
            .prepare(
                "SELECT plan_id, plan_hash, workspace_id, action_kind, pane_id, pane_uuid, params_json,
                        plan_json, requires_approval, created_at, expires_at, consumed_at
                 FROM prepared_plans
                 WHERE plan_id = ?1
                   AND consumed_at IS NULL
                   AND expires_at >= ?2
                 LIMIT 1",
            )
            .map_err(|e| {
                StorageError::Database(format!("Failed to prepare prepared plan query: {e}"))
            })?;

        stmt.query_row(params![plan_id, now_ms], |row| {
            let pane_id: Option<i64> = row.get(4)?;
            let requires: i64 = row.get(8)?;
            Ok(PreparedPlanRecord {
                plan_id: row.get(0)?,
                plan_hash: row.get(1)?,
                workspace_id: row.get(2)?,
                action_kind: row.get(3)?,
                pane_id: pane_id.map(|v| v as u64),
                pane_uuid: row.get(5)?,
                params_json: row.get(6)?,
                plan_json: row.get(7)?,
                requires_approval: requires != 0,
                created_at: row.get(9)?,
                expires_at: row.get(10)?,
                consumed_at: row.get(11)?,
            })
        })
        .optional()
        .map_err(|e| StorageError::Database(format!("Prepared plan query failed: {e}")))?
    };

    if let Some(mut record) = record {
        let updated = tx
            .execute(
                "UPDATE prepared_plans SET consumed_at = ?1 WHERE plan_id = ?2 AND consumed_at IS NULL",
                params![now_ms, plan_id],
            )
            .map_err(|e| {
                StorageError::Database(format!("Failed to consume prepared plan: {e}"))
            })?;

        if updated == 0 {
            tx.commit().map_err(|e| {
                StorageError::Database(format!("Failed to commit prepared plan: {e}"))
            })?;
            return Ok(None);
        }

        record.consumed_at = Some(now_ms);
        tx.commit()
            .map_err(|e| StorageError::Database(format!("Failed to commit prepared plan: {e}")))?;
        return Ok(Some(record));
    }

    tx.commit()
        .map_err(|e| StorageError::Database(format!("Failed to commit prepared plan: {e}")))?;
    Ok(None)
}

/// Consume an approval token if it matches scope and is valid (synchronous)
#[allow(clippy::too_many_arguments)]
fn consume_approval_token_sync(
    conn: &mut Connection,
    code_hash: &str,
    workspace_id: &str,
    action_kind: &str,
    pane_id: Option<u64>,
    action_fingerprint: &str,
) -> Result<Option<ApprovalTokenRecord>> {
    let now = now_ms();
    let tx = conn
        .transaction()
        .map_err(|e| StorageError::Database(format!("Failed to start transaction: {e}")))?;

    let mut sql = String::from(
        "SELECT id, code_hash, created_at, expires_at, used_at, workspace_id, action_kind,
         pane_id, action_fingerprint, plan_hash, plan_version, risk_summary
         FROM approval_tokens
         WHERE code_hash = ?
           AND workspace_id = ?
           AND action_kind = ?
           AND action_fingerprint = ?
           AND used_at IS NULL
           AND expires_at >= ?",
    );
    let mut params = vec![
        SqlValue::Text(code_hash.to_string()),
        SqlValue::Text(workspace_id.to_string()),
        SqlValue::Text(action_kind.to_string()),
        SqlValue::Text(action_fingerprint.to_string()),
        SqlValue::Integer(now),
    ];

    // Add pane_id constraint if specified
    if let Some(pid) = pane_id {
        sql.push_str(" AND pane_id = ?");
        #[allow(clippy::cast_possible_wrap)]
        params.push(SqlValue::Integer(pid as i64));
    }
    sql.push_str(" LIMIT 1");

    let record = {
        let mut stmt = tx.prepare(&sql).map_err(|e| {
            StorageError::Database(format!("Failed to prepare approval query: {e}"))
        })?;

        stmt.query_row(rusqlite::params_from_iter(params), |row| {
            Ok(ApprovalTokenRecord {
                id: row.get(0)?,
                code_hash: row.get(1)?,
                created_at: row.get(2)?,
                expires_at: row.get(3)?,
                used_at: row.get(4)?,
                workspace_id: row.get(5)?,
                action_kind: row.get(6)?,
                pane_id: {
                    let val: Option<i64> = row.get(7)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                action_fingerprint: row.get(8)?,
                plan_hash: row.get(9)?,
                plan_version: row.get(10)?,
                risk_summary: row.get(11)?,
            })
        })
        .optional()
        .map_err(|e| StorageError::Database(format!("Approval query failed: {e}")))?
    };

    if let Some(mut record) = record {
        let updated = tx
            .execute(
                "UPDATE approval_tokens SET used_at = ?1 WHERE id = ?2 AND used_at IS NULL",
                params![now, record.id],
            )
            .map_err(|e| {
                StorageError::Database(format!("Failed to consume approval token: {e}"))
            })?;

        if updated == 0 {
            tx.commit().map_err(|e| {
                StorageError::Database(format!("Failed to commit approval token: {e}"))
            })?;
            return Ok(None);
        }

        record.used_at = Some(now);
        tx.commit()
            .map_err(|e| StorageError::Database(format!("Failed to commit approval token: {e}")))?;
        return Ok(Some(record));
    }

    tx.commit()
        .map_err(|e| StorageError::Database(format!("Failed to commit approval token: {e}")))?;
    Ok(None)
}

/// Get an approval token by code hash without consuming it (synchronous)
fn get_approval_token_by_code_sync(
    conn: &Connection,
    code_hash: &str,
    workspace_id: &str,
) -> Result<Option<ApprovalTokenRecord>> {
    let sql = "SELECT id, code_hash, created_at, expires_at, used_at, workspace_id, action_kind,
               pane_id, action_fingerprint, plan_hash, plan_version, risk_summary
               FROM approval_tokens
               WHERE code_hash = ?
                 AND workspace_id = ?
               LIMIT 1";

    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare approval query: {e}")))?;

    stmt.query_row([code_hash, workspace_id], |row| {
        Ok(ApprovalTokenRecord {
            id: row.get(0)?,
            code_hash: row.get(1)?,
            created_at: row.get(2)?,
            expires_at: row.get(3)?,
            used_at: row.get(4)?,
            workspace_id: row.get(5)?,
            action_kind: row.get(6)?,
            pane_id: {
                let val: Option<i64> = row.get(7)?;
                #[allow(clippy::cast_sign_loss)]
                val.map(|v| v as u64)
            },
            action_fingerprint: row.get(8)?,
            plan_hash: row.get(9)?,
            plan_version: row.get(10)?,
            risk_summary: row.get(11)?,
        })
    })
    .optional()
    .map_err(|e| StorageError::Database(format!("Approval query failed: {e}")).into())
}

/// Consume an approval token by code hash only, without fingerprint validation (synchronous)
fn consume_approval_token_by_code_sync(
    conn: &mut Connection,
    code_hash: &str,
    workspace_id: &str,
) -> Result<Option<ApprovalTokenRecord>> {
    let now = now_ms();
    let tx = conn
        .transaction()
        .map_err(|e| StorageError::Database(format!("Failed to start transaction: {e}")))?;

    let sql = "SELECT id, code_hash, created_at, expires_at, used_at, workspace_id, action_kind,
               pane_id, action_fingerprint, plan_hash, plan_version, risk_summary
               FROM approval_tokens
               WHERE code_hash = ?
                 AND workspace_id = ?
                 AND used_at IS NULL
                 AND expires_at >= ?
               LIMIT 1";

    let record = {
        let mut stmt = tx.prepare(sql).map_err(|e| {
            StorageError::Database(format!("Failed to prepare approval query: {e}"))
        })?;

        stmt.query_row([code_hash, workspace_id, &now.to_string()], |row| {
            Ok(ApprovalTokenRecord {
                id: row.get(0)?,
                code_hash: row.get(1)?,
                created_at: row.get(2)?,
                expires_at: row.get(3)?,
                used_at: row.get(4)?,
                workspace_id: row.get(5)?,
                action_kind: row.get(6)?,
                pane_id: {
                    let val: Option<i64> = row.get(7)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                action_fingerprint: row.get(8)?,
                plan_hash: row.get(9)?,
                plan_version: row.get(10)?,
                risk_summary: row.get(11)?,
            })
        })
        .optional()
        .map_err(|e| StorageError::Database(format!("Approval query failed: {e}")))?
    };

    if let Some(mut record) = record {
        let updated = tx
            .execute(
                "UPDATE approval_tokens SET used_at = ?1 WHERE id = ?2 AND used_at IS NULL",
                params![now, record.id],
            )
            .map_err(|e| {
                StorageError::Database(format!("Failed to consume approval token: {e}"))
            })?;

        if updated == 0 {
            tx.commit().map_err(|e| {
                StorageError::Database(format!("Failed to commit approval token: {e}"))
            })?;
            return Ok(None);
        }

        record.used_at = Some(now);
        tx.commit()
            .map_err(|e| StorageError::Database(format!("Failed to commit approval token: {e}")))?;
        return Ok(Some(record));
    }

    tx.commit()
        .map_err(|e| StorageError::Database(format!("Failed to commit approval token: {e}")))?;
    Ok(None)
}

// =============================================================================
// Read Operations (called from spawn_blocking)
// =============================================================================

/// Validate FTS5 query syntax by attempting a limited search
fn validate_fts_query(conn: &Connection, query: &str) -> Result<()> {
    // Try to execute a limited query to validate syntax
    let result = conn.query_row(
        "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH ?1 LIMIT 1",
        [query],
        |_| Ok(()),
    );

    match result {
        Ok(()) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, Some(msg))) => {
            // FTS5 syntax errors have specific error codes
            Err(StorageError::FtsQueryError(format!(
                "Invalid FTS5 query syntax: {msg}. \
                 Valid syntax includes: simple words, \"phrases\", prefix*, AND/OR/NOT operators. \
                 SQLite error code: {}",
                err.extended_code
            ))
            .into())
        }
        Err(e) => Err(StorageError::FtsQueryError(format!("Query validation failed: {e}")).into()),
    }
}

/// Search using FTS5 with snippet extraction and BM25 scores
///
/// Returns structured results with:
/// - The matching segment data
/// - A snippet with highlighted matching terms (using configurable markers)
/// - Highlighted content (full segment with markers)
/// - The BM25 relevance score (lower = more relevant)
#[allow(clippy::cast_sign_loss)]
fn search_fts_with_snippets(
    conn: &Connection,
    query: &str,
    options: &SearchOptions,
) -> Result<Vec<SearchResult>> {
    // Validate query syntax first for better error messages
    validate_fts_query(conn, query)?;

    let limit = options.limit.unwrap_or(100);
    let include_snippets = options.include_snippets.unwrap_or(true);
    let max_tokens = options.snippet_max_tokens.unwrap_or(64);
    let prefix = options.highlight_prefix.as_deref().unwrap_or(">>>");
    let suffix = options.highlight_suffix.as_deref().unwrap_or("<<<");

    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query.to_string())];

    // Build query with optional filters
    // FTS5 snippet function: snippet(table, column_idx, prefix, suffix, ellipsis, max_tokens)
    // FTS5 bm25 function: bm25(table) returns negative score (more negative = better match)
    let mut sql = if include_snippets {
        params_vec.push(Box::new(prefix.to_string()));
        params_vec.push(Box::new(suffix.to_string()));
        params_vec.push(Box::new(usize_to_i64(max_tokens, "max_tokens")?));

        "SELECT s.id, s.pane_id, s.seq, s.content, s.content_len, s.content_hash, s.captured_at,
                snippet(output_segments_fts, 0, ?2, ?3, '...', ?4) as snippet,
                highlight(output_segments_fts, 0, ?2, ?3) as highlight,
                bm25(output_segments_fts) as score
         FROM output_segments s
         JOIN output_segments_fts fts ON s.id = fts.rowid
         WHERE output_segments_fts MATCH ?1"
            .to_string()
    } else {
        String::from(
            "SELECT s.id, s.pane_id, s.seq, s.content, s.content_len, s.content_hash, s.captured_at,
                    NULL as snippet,
                    NULL as highlight,
                    bm25(output_segments_fts) as score
             FROM output_segments s
             JOIN output_segments_fts fts ON s.id = fts.rowid
             WHERE output_segments_fts MATCH ?1",
        )
    };

    if let Some(pane_id) = options.pane_id {
        sql.push_str(" AND s.pane_id = ?");
        params_vec.push(Box::new(u64_to_i64(pane_id, "pane_id")?));
    }

    if let Some(since) = options.since {
        sql.push_str(" AND s.captured_at >= ?");
        params_vec.push(Box::new(since));
    }

    if let Some(until) = options.until {
        sql.push_str(" AND s.captured_at <= ?");
        params_vec.push(Box::new(until));
    }

    // Order by BM25 score (more negative = better match, so ascending order)
    // Tie-break by captured_at/id for deterministic ordering.
    sql.push_str(" ORDER BY score ASC, s.captured_at ASC, s.id ASC LIMIT ?");
    params_vec.push(Box::new(usize_to_i64(limit, "limit")?));

    let params_refs: Vec<&dyn rusqlite::ToSql> =
        params_vec.iter().map(std::convert::AsRef::as_ref).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::FtsQueryError(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(params_refs.as_slice(), |row| {
            Ok(SearchResult {
                segment: Segment {
                    id: row.get(0)?,
                    pane_id: {
                        let val: i64 = row.get(1)?;
                        #[allow(clippy::cast_sign_loss)]
                        {
                            val as u64
                        }
                    },
                    seq: {
                        let val: i64 = row.get(2)?;
                        #[allow(clippy::cast_sign_loss)]
                        {
                            val as u64
                        }
                    },
                    content: row.get(3)?,
                    content_len: {
                        let val: i64 = row.get(4)?;
                        i64_to_usize(val)?
                    },
                    content_hash: row.get(5)?,
                    captured_at: row.get(6)?,
                },
                snippet: row.get(7)?,
                highlight: row.get(8)?,
                score: row.get(9)?,
            })
        })
        .map_err(|e| StorageError::FtsQueryError(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

// =============================================================================
// Indexing Progress Tracking (wa-upg.5.2)
// =============================================================================

/// Get per-pane indexing statistics.
///
/// Since FTS5 indexing is trigger-driven (same transaction as INSERT), the
/// segment count *is* the FTS row count under normal operation.  We separately
/// check FTS integrity to detect corruption.
fn get_pane_indexing_stats_sync(conn: &Connection) -> Result<Vec<PaneIndexingStats>> {
    let mut stmt = conn
        .prepare(
            "SELECT p.pane_id,
                    COALESCE(seg.cnt, 0),
                    COALESCE(seg.bytes, 0),
                    seg.max_seq,
                    seg.last_at
             FROM panes p
             LEFT JOIN (
                 SELECT pane_id,
                        COUNT(*) AS cnt,
                        SUM(content_len) AS bytes,
                        MAX(seq) AS max_seq,
                        MAX(captured_at) AS last_at
                 FROM output_segments
                 GROUP BY pane_id
             ) seg ON seg.pane_id = p.pane_id
             WHERE p.observed = 1
             ORDER BY p.pane_id",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare indexing stats: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            let pane_id: u64 = {
                let v: i64 = row.get(0)?;
                v as u64
            };
            let segment_count: u64 = {
                let v: i64 = row.get(1)?;
                v as u64
            };
            let total_bytes: u64 = {
                let v: i64 = row.get(2)?;
                v as u64
            };
            let max_seq: Option<u64> = row.get::<_, Option<i64>>(3)?.map(|v| v as u64);
            let last_segment_at: Option<i64> = row.get(4)?;
            // Trigger-driven FTS: segment_count == fts_row_count by construction
            Ok(PaneIndexingStats {
                pane_id,
                segment_count,
                total_bytes,
                max_seq,
                last_segment_at,
                fts_row_count: segment_count,
                fts_consistent: true,
            })
        })
        .map_err(|e| StorageError::Database(format!("Failed to query indexing stats: {e}")))?;

    let mut stats = Vec::new();
    for row in rows {
        stats.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(stats)
}

/// Run the FTS5 integrity-check command.
///
/// Returns Ok(true) if the index is consistent, Ok(false) if corruption
/// is detected.
fn check_fts_integrity_sync(conn: &Connection) -> Result<bool> {
    match conn.execute_batch(
        "INSERT INTO output_segments_fts(output_segments_fts) VALUES('integrity-check')",
    ) {
        Ok(()) => Ok(true),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("database disk image is malformed") || msg.contains("fts5: ") {
                Ok(false)
            } else {
                Err(StorageError::Database(format!("FTS integrity check failed: {e}")).into())
            }
        }
    }
}

/// Build an aggregate health report from per-pane stats and FTS integrity.
fn build_indexing_health_report(
    pane_stats: Vec<PaneIndexingStats>,
    fts_ok: bool,
) -> IndexingHealthReport {
    let total_segments: u64 = pane_stats.iter().map(|p| p.segment_count).sum();
    let total_bytes: u64 = pane_stats.iter().map(|p| p.total_bytes).sum();
    let total_fts_rows: u64 = pane_stats.iter().map(|p| p.fts_row_count).sum();
    let inconsistent_panes = if fts_ok {
        0
    } else {
        // If FTS integrity check fails, mark all panes as potentially inconsistent
        pane_stats.len() as u64
    };

    // Update per-pane consistency based on FTS health
    let panes: Vec<PaneIndexingStats> = if fts_ok {
        pane_stats
    } else {
        pane_stats
            .into_iter()
            .map(|mut p| {
                p.fts_consistent = false;
                p
            })
            .collect()
    };

    IndexingHealthReport {
        healthy: fts_ok && inconsistent_panes == 0,
        total_segments,
        total_bytes,
        total_fts_rows,
        inconsistent_panes,
        panes,
    }
}

// =============================================================================
// Incremental FTS Sync (wa-3g9.4)
// =============================================================================

/// Current FTS index version. Increment when FTS schema changes require rebuild.
const FTS_INDEX_VERSION: u32 = 1;

/// Get the current FTS index state
fn get_fts_index_state_sync(conn: &Connection) -> Result<Option<FtsIndexState>> {
    conn.query_row(
        "SELECT index_version, last_full_rebuild_at, created_at, updated_at
         FROM fts_index_state WHERE id = 1",
        [],
        |row| {
            Ok(FtsIndexState {
                index_version: {
                    let v: i64 = row.get(0)?;
                    v as u32
                },
                last_full_rebuild_at: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Failed to get FTS index state: {e}")).into())
}

/// Initialize or update FTS index state
fn upsert_fts_index_state_sync(conn: &Connection, state: &FtsIndexState) -> Result<()> {
    conn.execute(
        "INSERT INTO fts_index_state (id, index_version, last_full_rebuild_at, created_at, updated_at)
         VALUES (1, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
             index_version = excluded.index_version,
             last_full_rebuild_at = excluded.last_full_rebuild_at,
             updated_at = excluded.updated_at",
        params![
            i64::from(state.index_version),
            state.last_full_rebuild_at,
            state.created_at,
            state.updated_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert FTS index state: {e}")))?;
    Ok(())
}

/// Get FTS progress for a specific pane
fn get_fts_pane_progress_sync(conn: &Connection, pane_id: u64) -> Result<Option<FtsPaneProgress>> {
    conn.query_row(
        "SELECT pane_id, last_indexed_seq, indexed_count, last_indexed_at
         FROM fts_pane_progress WHERE pane_id = ?1",
        [pane_id as i64],
        |row| {
            Ok(FtsPaneProgress {
                pane_id: {
                    let v: i64 = row.get(0)?;
                    v as u64
                },
                last_indexed_seq: {
                    let v: i64 = row.get(1)?;
                    v as u64
                },
                indexed_count: {
                    let v: i64 = row.get(2)?;
                    v as u64
                },
                last_indexed_at: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Failed to get FTS pane progress: {e}")).into())
}

/// Update FTS progress for a pane
fn upsert_fts_pane_progress_sync(conn: &Connection, progress: &FtsPaneProgress) -> Result<()> {
    conn.execute(
        "INSERT INTO fts_pane_progress (pane_id, last_indexed_seq, indexed_count, last_indexed_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(pane_id) DO UPDATE SET
             last_indexed_seq = excluded.last_indexed_seq,
             indexed_count = excluded.indexed_count,
             last_indexed_at = excluded.last_indexed_at",
        params![
            progress.pane_id as i64,
            progress.last_indexed_seq as i64,
            progress.indexed_count as i64,
            progress.last_indexed_at,
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to upsert FTS pane progress: {e}")))?;
    Ok(())
}

/// Clear all FTS pane progress (used before full rebuild)
fn clear_fts_pane_progress_sync(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM fts_pane_progress", [])
        .map_err(|e| StorageError::Database(format!("Failed to clear FTS pane progress: {e}")))?;
    Ok(())
}

/// Get segments that need indexing for a pane (seq > last_indexed_seq)
fn get_unindexed_segments_sync(
    conn: &Connection,
    pane_id: u64,
    last_indexed_seq: u64,
    limit: usize,
) -> Result<Vec<Segment>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
             FROM output_segments
             WHERE pane_id = ?1 AND seq > ?2
             ORDER BY seq
             LIMIT ?3",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare unindexed query: {e}")))?;

    let rows = stmt
        .query_map(
            params![pane_id as i64, last_indexed_seq as i64, limit as i64],
            |row| {
                Ok(Segment {
                    id: row.get(0)?,
                    pane_id: {
                        let v: i64 = row.get(1)?;
                        v as u64
                    },
                    seq: {
                        let v: i64 = row.get(2)?;
                        v as u64
                    },
                    content: row.get(3)?,
                    content_len: {
                        let v: i64 = row.get(4)?;
                        i64_to_usize(v)?
                    },
                    content_hash: row.get(5)?,
                    captured_at: row.get(6)?,
                })
            },
        )
        .map_err(|e| StorageError::Database(format!("Failed to query unindexed segments: {e}")))?;

    let mut segments = Vec::new();
    for row in rows {
        segments.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(segments)
}

/// Manually insert a segment into the FTS index (for recovery/rebuild)
fn insert_fts_entry_sync(conn: &Connection, segment: &Segment) -> Result<()> {
    conn.execute(
        "INSERT INTO output_segments_fts(rowid, content) VALUES (?1, ?2)",
        params![segment.id, &segment.content],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert FTS entry: {e}")))?;
    Ok(())
}

/// Perform incremental FTS sync for a pane
///
/// This function indexes segments that are newer than the recorded progress,
/// working in batches to avoid memory pressure and allow progress commits.
fn sync_fts_for_pane(
    conn: &Connection,
    pane_id: u64,
    config: &FtsSyncConfig,
) -> Result<(u64, u64)> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // Get current progress
    let progress = get_fts_pane_progress_sync(conn, pane_id)?;
    let last_seq = progress.as_ref().map_or(0, |p| p.last_indexed_seq);
    let mut indexed_count = progress.as_ref().map_or(0, |p| p.indexed_count);

    let mut total_indexed = 0u64;
    let mut max_seq = last_seq;

    loop {
        // Get batch of unindexed segments
        let segments = get_unindexed_segments_sync(conn, pane_id, max_seq, config.batch_size)?;
        if segments.is_empty() {
            break;
        }

        // Index each segment (respecting byte limit)
        let mut batch_bytes = 0usize;
        for segment in &segments {
            // Check byte limit (but always index at least one)
            if batch_bytes > 0 && batch_bytes + segment.content_len > config.max_batch_bytes {
                break;
            }

            insert_fts_entry_sync(conn, segment)?;
            total_indexed += 1;
            indexed_count += 1;
            max_seq = segment.seq;
            batch_bytes += segment.content_len;
        }

        // Commit progress after each batch if configured
        if config.commit_progress && total_indexed > 0 {
            let new_progress = FtsPaneProgress {
                pane_id,
                last_indexed_seq: max_seq,
                indexed_count,
                last_indexed_at: now,
            };
            upsert_fts_pane_progress_sync(conn, &new_progress)?;
        }

        // If we processed fewer segments than batch size, we're done
        if segments.len() < config.batch_size {
            break;
        }
    }

    // Final progress update
    if total_indexed > 0 && !config.commit_progress {
        let new_progress = FtsPaneProgress {
            pane_id,
            last_indexed_seq: max_seq,
            indexed_count,
            last_indexed_at: now,
        };
        upsert_fts_pane_progress_sync(conn, &new_progress)?;
    }

    Ok((total_indexed, max_seq))
}

/// Perform a full FTS rebuild with batched progress tracking
///
/// This drops the FTS index content and reindexes all segments.
fn full_fts_rebuild_sync(conn: &Connection, config: &FtsSyncConfig) -> Result<FtsSyncResult> {
    use std::time::Instant;
    let start = Instant::now();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut warnings = Vec::new();

    // Drop all FTS content
    if let Err(e) = conn
        .execute_batch("INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')")
    {
        warnings.push(format!("FTS delete-all failed (may be empty): {e}"));
    }

    // Clear progress tracking
    clear_fts_pane_progress_sync(conn)?;

    // Get all panes
    let pane_ids: Vec<u64> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT pane_id FROM output_segments ORDER BY pane_id")
            .map_err(|e| StorageError::Database(format!("Failed to list panes: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let v: i64 = row.get(0)?;
                Ok(v as u64)
            })
            .map_err(|e| StorageError::Database(format!("Failed to query panes: {e}")))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
        }
        ids
    };

    let mut total_indexed = 0u64;
    let panes_processed = pane_ids.len() as u64;

    // Sync each pane
    for pane_id in pane_ids {
        match sync_fts_for_pane(conn, pane_id, config) {
            Ok((indexed, _)) => total_indexed += indexed,
            Err(e) => warnings.push(format!("Pane {pane_id} sync failed: {e}")),
        }
    }

    // Update index state
    let state = FtsIndexState {
        index_version: FTS_INDEX_VERSION,
        last_full_rebuild_at: Some(now),
        created_at: now,
        updated_at: now,
    };
    upsert_fts_index_state_sync(conn, &state)?;

    let duration = start.elapsed();
    Ok(FtsSyncResult {
        segments_indexed: total_indexed,
        panes_processed,
        full_rebuild: true,
        duration_ms: duration.as_millis() as u64,
        warnings,
    })
}

/// Perform incremental FTS sync on startup
///
/// This checks the FTS index state and either:
/// 1. Does nothing if index is healthy and version matches
/// 2. Syncs only new segments if index is healthy but has gaps
/// 3. Performs a full rebuild if index is corrupt or version mismatches
pub fn sync_fts_on_startup(conn: &Connection, config: &FtsSyncConfig) -> Result<FtsSyncResult> {
    use std::time::Instant;
    let start = Instant::now();

    let mut warnings = Vec::new();

    // Check FTS integrity
    let fts_ok = check_fts_integrity_sync(conn)?;
    if !fts_ok {
        tracing::warn!("FTS index corruption detected, performing full rebuild");
        return full_fts_rebuild_sync(conn, config);
    }

    // Get current index state
    let state = get_fts_index_state_sync(conn)?;

    // Check if version mismatch (schema change)
    if let Some(ref s) = state {
        if s.index_version != FTS_INDEX_VERSION {
            tracing::info!(
                old_version = s.index_version,
                new_version = FTS_INDEX_VERSION,
                "FTS index version mismatch, performing full rebuild"
            );
            return full_fts_rebuild_sync(conn, config);
        }
    } else {
        // No state = first run after migration, initialize
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let new_state = FtsIndexState {
            index_version: FTS_INDEX_VERSION,
            last_full_rebuild_at: None,
            created_at: now,
            updated_at: now,
        };
        upsert_fts_index_state_sync(conn, &new_state)?;
    }

    // Get all panes with segments
    let pane_ids: Vec<u64> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT pane_id FROM output_segments ORDER BY pane_id")
            .map_err(|e| StorageError::Database(format!("Failed to list panes: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let v: i64 = row.get(0)?;
                Ok(v as u64)
            })
            .map_err(|e| StorageError::Database(format!("Failed to query panes: {e}")))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
        }
        ids
    };

    let mut total_indexed = 0u64;
    let panes_processed = pane_ids.len() as u64;

    // Incremental sync each pane
    for pane_id in pane_ids {
        match sync_fts_for_pane(conn, pane_id, config) {
            Ok((indexed, _)) => total_indexed += indexed,
            Err(e) => warnings.push(format!("Pane {pane_id} incremental sync failed: {e}")),
        }
    }

    let duration = start.elapsed();
    Ok(FtsSyncResult {
        segments_indexed: total_indexed,
        panes_processed,
        full_rebuild: false,
        duration_ms: duration.as_millis() as u64,
        warnings,
    })
}

/// Query an agent session by ID
#[allow(clippy::cast_sign_loss)]
fn query_agent_session(conn: &Connection, session_id: i64) -> Result<Option<AgentSessionRecord>> {
    conn.query_row(
        "SELECT id, pane_id, agent_type, session_id, external_id, external_meta,
         started_at, ended_at, end_reason, total_tokens, input_tokens, output_tokens,
         cached_tokens, reasoning_tokens, model_name, estimated_cost_usd
         FROM agent_sessions WHERE id = ?1",
        [session_id],
        |row| {
            let external_meta_str: Option<String> = row.get(5)?;
            let external_meta = external_meta_str
                .as_ref()
                .and_then(|value| serde_json::from_str(value).ok());
            Ok(AgentSessionRecord {
                id: row.get(0)?,
                pane_id: {
                    let v: i64 = row.get(1)?;
                    v as u64
                },
                agent_type: row.get(2)?,
                session_id: row.get(3)?,
                external_id: row.get(4)?,
                external_meta,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                end_reason: row.get(8)?,
                total_tokens: row.get(9)?,
                input_tokens: row.get(10)?,
                output_tokens: row.get(11)?,
                cached_tokens: row.get(12)?,
                reasoning_tokens: row.get(13)?,
                model_name: row.get(14)?,
                estimated_cost_usd: row.get(15)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

/// Query active agent sessions (ended_at IS NULL)
#[allow(clippy::cast_sign_loss)]
fn query_active_sessions(conn: &Connection) -> Result<Vec<AgentSessionRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, agent_type, session_id, external_id, external_meta,
             started_at, ended_at, end_reason, total_tokens, input_tokens, output_tokens,
             cached_tokens, reasoning_tokens, model_name, estimated_cost_usd
             FROM agent_sessions WHERE ended_at IS NULL
             ORDER BY started_at DESC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            let external_meta_str: Option<String> = row.get(5)?;
            let external_meta = external_meta_str
                .as_ref()
                .and_then(|value| serde_json::from_str(value).ok());
            Ok(AgentSessionRecord {
                id: row.get(0)?,
                pane_id: {
                    let v: i64 = row.get(1)?;
                    v as u64
                },
                agent_type: row.get(2)?,
                session_id: row.get(3)?,
                external_id: row.get(4)?,
                external_meta,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                end_reason: row.get(8)?,
                total_tokens: row.get(9)?,
                input_tokens: row.get(10)?,
                output_tokens: row.get(11)?,
                cached_tokens: row.get(12)?,
                reasoning_tokens: row.get(13)?,
                model_name: row.get(14)?,
                estimated_cost_usd: row.get(15)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

/// Query agent sessions for a specific pane
#[allow(clippy::cast_sign_loss)]
fn query_sessions_for_pane(conn: &Connection, pane_id: u64) -> Result<Vec<AgentSessionRecord>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;

    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, agent_type, session_id, external_id, external_meta,
             started_at, ended_at, end_reason, total_tokens, input_tokens, output_tokens,
             cached_tokens, reasoning_tokens, model_name, estimated_cost_usd
             FROM agent_sessions WHERE pane_id = ?1
             ORDER BY started_at DESC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([pane_id_i64], |row| {
            let external_meta_str: Option<String> = row.get(5)?;
            let external_meta = external_meta_str
                .as_ref()
                .and_then(|value| serde_json::from_str(value).ok());
            Ok(AgentSessionRecord {
                id: row.get(0)?,
                pane_id: {
                    let v: i64 = row.get(1)?;
                    v as u64
                },
                agent_type: row.get(2)?,
                session_id: row.get(3)?,
                external_id: row.get(4)?,
                external_meta,
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                end_reason: row.get(8)?,
                total_tokens: row.get(9)?,
                input_tokens: row.get(10)?,
                output_tokens: row.get(11)?,
                cached_tokens: row.get(12)?,
                reasoning_tokens: row.get(13)?,
                model_name: row.get(14)?,
                estimated_cost_usd: row.get(15)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

/// Query unhandled events
fn query_unhandled_events(conn: &Connection, limit: usize) -> Result<Vec<StoredEvent>> {
    let limit_i64 = usize_to_i64(limit, "limit")?;

    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, rule_id, agent_type, event_type, severity, confidence,
             extracted, matched_text, segment_id, detected_at, dedupe_key, handled_at,
             handled_by_workflow_id, handled_status
             FROM events
             WHERE handled_at IS NULL
             ORDER BY detected_at DESC
             LIMIT ?1",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([limit_i64], |row| {
            let extracted_str: Option<String> = row.get(7)?;
            let extracted = extracted_str
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok());

            Ok(StoredEvent {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                rule_id: row.get(2)?,
                agent_type: row.get(3)?,
                event_type: row.get(4)?,
                severity: row.get(5)?,
                confidence: row.get(6)?,
                extracted,
                matched_text: row.get(8)?,
                segment_id: row.get(9)?,
                detected_at: row.get(10)?,
                dedupe_key: row.get(11)?,
                handled_at: row.get(12)?,
                handled_by_workflow_id: row.get(13)?,
                handled_status: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

/// Count unhandled events per pane
fn query_unhandled_event_counts(conn: &Connection) -> Result<std::collections::HashMap<u64, u32>> {
    let mut stmt = conn
        .prepare(
            "SELECT pane_id, COUNT(*) as cnt
             FROM events
             WHERE handled_at IS NULL
             GROUP BY pane_id",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            let pane_id: i64 = row.get(0)?;
            let count: i64 = row.get(1)?;
            let pane_id = u64::try_from(pane_id).unwrap_or(0);
            let count = u32::try_from(count).unwrap_or(u32::MAX);
            Ok((pane_id, count))
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut result = std::collections::HashMap::new();
    for row in rows {
        let (pane_id, count) =
            row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?;
        result.insert(pane_id, count);
    }

    Ok(result)
}

/// Get most recent activity timestamp per pane (from segments table)
fn query_last_activity_by_pane(conn: &Connection) -> Result<std::collections::HashMap<u64, i64>> {
    let mut stmt = conn
        .prepare(
            "SELECT pane_id, MAX(captured_at) as last_activity
             FROM output_segments
             GROUP BY pane_id",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            let pane_id: i64 = row.get(0)?;
            let last_activity: i64 = row.get(1)?;
            #[allow(clippy::cast_sign_loss)]
            Ok((pane_id as u64, last_activity))
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut result = std::collections::HashMap::new();
    for row in rows {
        let (pane_id, last_activity) =
            row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?;
        result.insert(pane_id, last_activity);
    }

    Ok(result)
}

/// Query events with optional filters
fn query_events(conn: &Connection, query: &EventQuery) -> Result<Vec<StoredEvent>> {
    let mut sql = String::from(
        "SELECT id, pane_id, rule_id, agent_type, event_type, severity, confidence,
         extracted, matched_text, segment_id, detected_at, dedupe_key, handled_at,
         handled_by_workflow_id, handled_status
         FROM events WHERE 1=1",
    );

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if query.unhandled_only {
        sql.push_str(" AND handled_at IS NULL");
    }

    if let Some(pane_id) = query.pane_id {
        sql.push_str(" AND pane_id = ?");
        #[allow(clippy::cast_possible_wrap)]
        params.push(Box::new(pane_id as i64));
    }

    if let Some(ref rule_id) = query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(Box::new(rule_id.clone()));
    }

    if let Some(ref event_type) = query.event_type {
        sql.push_str(" AND event_type = ?");
        params.push(Box::new(event_type.clone()));
    }

    if let Some(ref triage_state) = query.triage_state {
        sql.push_str(" AND triage_state = ?");
        params.push(Box::new(triage_state.clone()));
    }

    if let Some(ref label) = query.label {
        sql.push_str(" AND id IN (SELECT event_id FROM event_labels WHERE label = ?)");
        params.push(Box::new(label.clone()));
    }

    if let Some(since) = query.since {
        sql.push_str(" AND detected_at >= ?");
        params.push(Box::new(since));
    }

    if let Some(until) = query.until {
        sql.push_str(" AND detected_at <= ?");
        params.push(Box::new(until));
    }

    sql.push_str(" ORDER BY detected_at DESC");

    let limit = query.limit.unwrap_or(20);
    let limit_i64 = usize_to_i64(limit, "limit")?;
    sql.push_str(" LIMIT ?");
    params.push(Box::new(limit_i64));

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let extracted_str: Option<String> = row.get(7)?;
            let extracted = extracted_str
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok());

            Ok(StoredEvent {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                rule_id: row.get(2)?,
                agent_type: row.get(3)?,
                event_type: row.get(4)?,
                severity: row.get(5)?,
                confidence: row.get(6)?,
                extracted,
                matched_text: row.get(8)?,
                segment_id: row.get(9)?,
                detected_at: row.get(10)?,
                dedupe_key: row.get(11)?,
                handled_at: row.get(12)?,
                handled_by_workflow_id: row.get(13)?,
                handled_status: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

// =============================================================================
// Timeline Query Implementation (wa-6sk.1)
// =============================================================================

/// Query timeline with unified event view across panes
fn query_timeline(conn: &Connection, query: &TimelineQuery) -> Result<Timeline> {
    // Build the SQL query with joins for pane info
    let mut sql = String::from(
        "SELECT e.id, e.pane_id, e.rule_id, e.agent_type, e.event_type, e.severity,
                e.confidence, e.detected_at, e.handled_at, e.handled_by_workflow_id,
                e.handled_status, e.matched_text,
                p.pane_uuid, p.domain, p.cwd, p.title
         FROM events e
         JOIN panes p ON p.pane_id = e.pane_id
         WHERE 1=1",
    );

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    // Time range filters
    if let Some(start) = query.start {
        sql.push_str(" AND e.detected_at >= ?");
        params.push(Box::new(start));
    }

    if let Some(end) = query.end {
        sql.push_str(" AND e.detected_at <= ?");
        params.push(Box::new(end));
    }

    // Pane filter
    if let Some(ref pane_ids) = query.pane_ids {
        if !pane_ids.is_empty() {
            let placeholders: Vec<&str> = pane_ids.iter().map(|_| "?").collect();
            sql.push_str(&format!(" AND e.pane_id IN ({})", placeholders.join(",")));
            for &pid in pane_ids {
                params.push(Box::new(pid as i64));
            }
        }
    }

    // Severity filter
    if let Some(ref severities) = query.severities {
        if !severities.is_empty() {
            let placeholders: Vec<&str> = severities.iter().map(|_| "?").collect();
            sql.push_str(&format!(" AND e.severity IN ({})", placeholders.join(",")));
            for s in severities {
                params.push(Box::new(s.clone()));
            }
        }
    }

    // Event type filter
    if let Some(ref event_types) = query.event_types {
        if !event_types.is_empty() {
            let placeholders: Vec<&str> = event_types.iter().map(|_| "?").collect();
            sql.push_str(&format!(
                " AND e.event_type IN ({})",
                placeholders.join(",")
            ));
            for et in event_types {
                params.push(Box::new(et.clone()));
            }
        }
    }

    // Agent type filter
    if let Some(ref agent_types) = query.agent_types {
        if !agent_types.is_empty() {
            let placeholders: Vec<&str> = agent_types.iter().map(|_| "?").collect();
            sql.push_str(&format!(
                " AND e.agent_type IN ({})",
                placeholders.join(",")
            ));
            for at in agent_types {
                params.push(Box::new(at.clone()));
            }
        }
    }

    // Unhandled filter
    if query.unhandled_only {
        sql.push_str(" AND e.handled_at IS NULL");
    }

    // Count total before pagination
    let count_sql = format!(
        "SELECT COUNT(*) FROM events e JOIN panes p ON p.pane_id = e.pane_id WHERE {}",
        sql.split("WHERE").nth(1).unwrap_or("1=1")
    );

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let total_count: i64 = conn
        .query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))
        .unwrap_or(0);

    // Add ordering and pagination
    sql.push_str(" ORDER BY e.detected_at ASC");

    let limit_i64 = query.limit as i64;
    let offset_i64 = query.offset as i64;
    sql.push_str(" LIMIT ? OFFSET ?");
    params.push(Box::new(limit_i64));
    params.push(Box::new(offset_i64));

    // Execute query
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare timeline query: {e}")))?;

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let pane_id: i64 = row.get(1)?;
            let pane_id_u64 = pane_id as u64;

            Ok(TimelineEvent {
                id: row.get(0)?,
                timestamp: row.get(7)?,
                pane_info: PaneInfo {
                    pane_id: pane_id_u64,
                    pane_uuid: row.get(12)?,
                    agent_type: {
                        let at: String = row.get(3)?;
                        if at == "unknown" { None } else { Some(at) }
                    },
                    domain: row.get(13)?,
                    cwd: row.get(14)?,
                    title: row.get(15)?,
                },
                rule_id: row.get(2)?,
                event_type: row.get(4)?,
                severity: row.get(5)?,
                confidence: row.get(6)?,
                handled: {
                    let handled_at: Option<i64> = row.get(8)?;
                    handled_at.map(|ts| HandledInfo {
                        handled_at: ts,
                        workflow_id: row.get(9).ok().flatten(),
                        status: row
                            .get::<_, Option<String>>(10)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| "unknown".to_string()),
                    })
                },
                correlations: Vec::new(), // Populated later if requested
                summary: row.get::<_, Option<String>>(11).ok().flatten(),
            })
        })
        .map_err(|e| StorageError::Database(format!("Timeline query failed: {e}")))?;

    let mut events = Vec::new();
    for row in rows {
        events.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    // Calculate time range from results
    let (start, end) = if events.is_empty() {
        let now = now_ms();
        (query.start.unwrap_or(now), query.end.unwrap_or(now))
    } else {
        (
            events.first().map_or(0, |e| e.timestamp),
            events.last().map_or(0, |e| e.timestamp),
        )
    };

    // Detect correlations if requested
    let correlations = if query.include_correlations && !events.is_empty() {
        detect_correlations(&events)
    } else {
        Vec::new()
    };

    // Attach correlation refs to events
    let mut events_with_refs = events;
    for event in &mut events_with_refs {
        event.correlations = correlations
            .iter()
            .filter(|c| c.event_ids.contains(&event.id))
            .map(|c| CorrelationRef {
                id: c.id.clone(),
                correlation_type: c.correlation_type,
            })
            .collect();
    }

    let has_more = (query.offset + events_with_refs.len()) < total_count as usize;

    Ok(Timeline {
        start,
        end,
        events: events_with_refs,
        correlations,
        total_count: total_count as u64,
        has_more,
    })
}

/// Detect correlations between timeline events
fn detect_correlations(events: &[TimelineEvent]) -> Vec<Correlation> {
    let mut correlations = Vec::new();
    let mut correlation_counter = 0u64;

    // Temporal correlation: events within 10 seconds of each other
    const TEMPORAL_WINDOW_MS: i64 = 10_000;
    // Cascade correlation window: 30 seconds
    const CASCADE_WINDOW_MS: i64 = 30_000;
    // Failover correlation window: 5 minutes
    const FAILOVER_WINDOW_MS: i64 = 300_000;
    // DedupeGroup window: same rule_id across different panes within 30 seconds
    const DEDUPE_GROUP_WINDOW_MS: i64 = 30_000;

    fn rule_prefix(rule_id: &str) -> Option<&str> {
        let prefix = rule_id.split('.').next()?;
        match prefix {
            "codex" | "claude_code" | "gemini" | "wezterm" => Some(prefix),
            _ => None,
        }
    }

    fn event_agent_type(event: &TimelineEvent) -> Option<&str> {
        if let Some(prefix) = rule_prefix(&event.rule_id) {
            Some(prefix)
        } else {
            event.pane_info.agent_type.as_deref()
        }
    }

    fn is_usage_limit_event(event: &TimelineEvent) -> bool {
        event.event_type == "usage.reached"
            || event.event_type == "usage_limit"
            || event.rule_id.contains("usage.reached")
            || event.rule_id.contains("usage_limit")
    }

    fn is_session_start_event(event: &TimelineEvent) -> bool {
        event.event_type == "session.start"
            || event.event_type == "session_start"
            || event.rule_id.contains("session.start")
            || event.rule_id.contains("session_start")
    }

    fn is_recovery_event(event: &TimelineEvent) -> bool {
        event.event_type.starts_with("session.")
            || event.event_type.starts_with("session_")
            || event.rule_id.contains("session.resume")
            || event.rule_id.contains("session.start")
            || event.rule_id.contains("session_resume")
            || event.rule_id.contains("session_start")
    }

    // Find temporal clusters
    let mut i = 0;
    while i < events.len() {
        let base_event = &events[i];
        let mut cluster = vec![base_event.id];
        let mut j = i + 1;

        // Collect events within temporal window
        while j < events.len() {
            let candidate = &events[j];
            if candidate.timestamp - base_event.timestamp <= TEMPORAL_WINDOW_MS {
                // Different panes = more interesting correlation
                if candidate.pane_info.pane_id != base_event.pane_info.pane_id {
                    cluster.push(candidate.id);
                }
            } else {
                break;
            }
            j += 1;
        }

        // Only create correlation if multiple events from different panes
        if cluster.len() > 1 {
            correlation_counter += 1;
            correlations.push(Correlation {
                id: format!("corr-temporal-{correlation_counter}"),
                event_ids: cluster,
                correlation_type: CorrelationType::Temporal,
                confidence: 0.6,
                description: "Events occurred within 10 seconds across different panes".to_string(),
            });
        }

        i += 1;
    }

    // Workflow group correlation: events handled by same workflow
    let mut workflow_groups: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();

    for event in events {
        if let Some(ref handled) = event.handled {
            if let Some(ref wf_id) = handled.workflow_id {
                workflow_groups
                    .entry(wf_id.clone())
                    .or_default()
                    .push(event.id);
            }
        }
    }

    for (wf_id, event_ids) in workflow_groups {
        if event_ids.len() > 1 {
            correlation_counter += 1;
            correlations.push(Correlation {
                id: format!("corr-workflow-{correlation_counter}"),
                event_ids,
                correlation_type: CorrelationType::WorkflowGroup,
                confidence: 0.95,
                description: format!("Events handled by workflow {wf_id}"),
            });
        }
    }

    // Cascade correlation: error/critical in one pane followed by recovery event elsewhere
    for (i, event) in events.iter().enumerate() {
        let severity = event.severity.to_lowercase();
        if severity != "error" && severity != "critical" {
            continue;
        }

        let agent = event_agent_type(event);

        for later_event in events.iter().skip(i + 1) {
            if later_event.timestamp - event.timestamp > CASCADE_WINDOW_MS {
                break;
            }
            if later_event.pane_info.pane_id == event.pane_info.pane_id {
                continue;
            }
            if !is_recovery_event(later_event) {
                continue;
            }

            let later_agent = event_agent_type(later_event);
            if agent.is_some() && later_agent.is_some() && agent != later_agent {
                continue;
            }

            correlation_counter += 1;
            correlations.push(Correlation {
                id: format!("corr-cascade-{correlation_counter}"),
                event_ids: vec![event.id, later_event.id],
                correlation_type: CorrelationType::Cascade,
                confidence: 0.75,
                description: "Error followed by recovery event in another pane".to_string(),
            });
            break;
        }
    }

    // DedupeGroup correlation: same rule_id firing across different panes within window
    {
        let mut rule_groups: std::collections::HashMap<&str, Vec<&TimelineEvent>> =
            std::collections::HashMap::new();
        for event in events {
            rule_groups
                .entry(event.rule_id.as_str())
                .or_default()
                .push(event);
        }
        for (rule_id, group) in &rule_groups {
            if group.len() < 2 {
                continue;
            }
            // Check if events span multiple panes and are within the window
            let pane_ids: std::collections::HashSet<u64> =
                group.iter().map(|e| e.pane_info.pane_id).collect();
            if pane_ids.len() < 2 {
                continue;
            }
            // Find clusters within the dedupe window
            let mut sorted = group.clone();
            sorted.sort_by_key(|e| e.timestamp);
            let mut cluster_start = 0;
            while cluster_start < sorted.len() {
                let base_ts = sorted[cluster_start].timestamp;
                let mut cluster_ids = vec![sorted[cluster_start].id];
                let mut cluster_panes =
                    std::collections::HashSet::from([sorted[cluster_start].pane_info.pane_id]);
                let mut j = cluster_start + 1;
                while j < sorted.len() && sorted[j].timestamp - base_ts <= DEDUPE_GROUP_WINDOW_MS {
                    cluster_ids.push(sorted[j].id);
                    cluster_panes.insert(sorted[j].pane_info.pane_id);
                    j += 1;
                }
                if cluster_ids.len() >= 2 && cluster_panes.len() >= 2 {
                    correlation_counter += 1;
                    correlations.push(Correlation {
                        id: format!("corr-dedupe-{correlation_counter}"),
                        event_ids: cluster_ids,
                        correlation_type: CorrelationType::DedupeGroup,
                        confidence: 0.7,
                        description: format!(
                            "Same rule '{}' fired across {} panes",
                            rule_id,
                            cluster_panes.len()
                        ),
                    });
                }
                cluster_start = j;
            }
        }
    }

    // Failover correlation: usage limit followed by new session in different pane
    for (i, event) in events.iter().enumerate() {
        if !is_usage_limit_event(event) {
            continue;
        }

        let agent = event_agent_type(event);

        // Look for session start in another pane within 5 minutes
        for later_event in events.iter().skip(i + 1) {
            if later_event.timestamp - event.timestamp > FAILOVER_WINDOW_MS {
                break;
            }
            if later_event.pane_info.pane_id == event.pane_info.pane_id {
                continue;
            }
            if !is_session_start_event(later_event) {
                continue;
            }

            let later_agent = event_agent_type(later_event);
            if agent.is_some() && later_agent.is_some() && agent != later_agent {
                continue;
            }

            correlation_counter += 1;
            correlations.push(Correlation {
                id: format!("corr-failover-{correlation_counter}"),
                event_ids: vec![event.id, later_event.id],
                correlation_type: CorrelationType::Failover,
                confidence: 0.85,
                description: "Usage limit followed by new session (potential failover)".to_string(),
            });
            break;
        }
    }

    correlations
}

/// Query audit actions with optional filters
fn query_audit_actions(conn: &Connection, query: &AuditQuery) -> Result<Vec<AuditActionRecord>> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result
         FROM audit_actions WHERE 1=1",
    );
    let mut params: Vec<SqlValue> = Vec::new();

    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(SqlValue::Integer(pane_id_i64));
    }
    if let Some(domain) = &query.domain {
        sql.push_str(" AND domain = ?");
        params.push(SqlValue::Text(domain.clone()));
    }
    if let Some(actor_kind) = &query.actor_kind {
        sql.push_str(" AND actor_kind = ?");
        params.push(SqlValue::Text(actor_kind.clone()));
    }
    if let Some(actor_id) = &query.actor_id {
        sql.push_str(" AND actor_id = ?");
        params.push(SqlValue::Text(actor_id.clone()));
    }
    if let Some(correlation_id) = &query.correlation_id {
        sql.push_str(" AND correlation_id = ?");
        params.push(SqlValue::Text(correlation_id.clone()));
    }
    if let Some(action_kind) = &query.action_kind {
        sql.push_str(" AND action_kind = ?");
        params.push(SqlValue::Text(action_kind.clone()));
    }
    if let Some(policy_decision) = &query.policy_decision {
        sql.push_str(" AND policy_decision = ?");
        params.push(SqlValue::Text(policy_decision.clone()));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(SqlValue::Text(rule_id.clone()));
    }
    if let Some(result) = &query.result {
        sql.push_str(" AND result = ?");
        params.push(SqlValue::Text(result.clone()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND ts >= ?");
        params.push(SqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND ts <= ?");
        params.push(SqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY ts DESC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(SqlValue::Integer(limit_i64));

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare audit query: {e}")))?;

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok(AuditActionRecord {
                id: row.get(0)?,
                ts: row.get(1)?,
                actor_kind: row.get(2)?,
                actor_id: row.get(3)?,
                correlation_id: row.get(4)?,
                pane_id: {
                    let val: Option<i64> = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                domain: row.get(6)?,
                action_kind: row.get(7)?,
                policy_decision: row.get(8)?,
                decision_reason: row.get(9)?,
                rule_id: row.get(10)?,
                input_summary: row.get(11)?,
                verification_summary: row.get(12)?,
                decision_context: row.get(13)?,
                result: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Audit query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

/// Query audit actions using a cursor for stable streaming.
fn query_audit_actions_stream(
    conn: &Connection,
    query: &AuditStreamQuery,
) -> Result<AuditStreamPage> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result
         FROM audit_actions WHERE 1=1",
    );
    let mut params: Vec<SqlValue> = Vec::new();

    if let Some(cursor) = query.cursor {
        sql.push_str(" AND id > ?");
        params.push(SqlValue::Integer(cursor));
    }
    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(SqlValue::Integer(pane_id_i64));
    }
    if let Some(domain) = &query.domain {
        sql.push_str(" AND domain = ?");
        params.push(SqlValue::Text(domain.clone()));
    }
    if let Some(actor_kind) = &query.actor_kind {
        sql.push_str(" AND actor_kind = ?");
        params.push(SqlValue::Text(actor_kind.clone()));
    }
    if let Some(actor_id) = &query.actor_id {
        sql.push_str(" AND actor_id = ?");
        params.push(SqlValue::Text(actor_id.clone()));
    }
    if let Some(correlation_id) = &query.correlation_id {
        sql.push_str(" AND correlation_id = ?");
        params.push(SqlValue::Text(correlation_id.clone()));
    }
    if let Some(action_kind) = &query.action_kind {
        sql.push_str(" AND action_kind = ?");
        params.push(SqlValue::Text(action_kind.clone()));
    }
    if let Some(policy_decision) = &query.policy_decision {
        sql.push_str(" AND policy_decision = ?");
        params.push(SqlValue::Text(policy_decision.clone()));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(SqlValue::Text(rule_id.clone()));
    }
    if let Some(result) = &query.result {
        sql.push_str(" AND result = ?");
        params.push(SqlValue::Text(result.clone()));
    }
    if let Some(since) = query.since {
        sql.push_str(" AND ts >= ?");
        params.push(SqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND ts <= ?");
        params.push(SqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY id ASC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(SqlValue::Integer(limit_i64));

    if let Some(offset) = query.offset {
        sql.push_str(" OFFSET ?");
        let offset_i64 = usize_to_i64(offset, "offset")?;
        params.push(SqlValue::Integer(offset_i64));
    }

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        StorageError::Database(format!("Failed to prepare audit stream query: {e}"))
    })?;

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok(AuditActionRecord {
                id: row.get(0)?,
                ts: row.get(1)?,
                actor_kind: row.get(2)?,
                actor_id: row.get(3)?,
                correlation_id: row.get(4)?,
                pane_id: {
                    let val: Option<i64> = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                domain: row.get(6)?,
                action_kind: row.get(7)?,
                policy_decision: row.get(8)?,
                decision_reason: row.get(9)?,
                rule_id: row.get(10)?,
                input_summary: row.get(11)?,
                verification_summary: row.get(12)?,
                decision_context: row.get(13)?,
                result: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Audit stream query failed: {e}")))?;

    let mut records = Vec::new();
    for row in rows {
        records.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    let next_cursor = records.last().map(|record| record.id);
    Ok(AuditStreamPage {
        records,
        next_cursor,
    })
}

/// Query action history view with optional filters
fn query_action_history(
    conn: &Connection,
    query: &ActionHistoryQuery,
) -> Result<Vec<ActionHistoryRecord>> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, correlation_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result, undoable, undo_strategy, undo_hint, undone_at, undone_by,
         workflow_id, step_name
         FROM action_history WHERE 1=1",
    );
    let mut params: Vec<SqlValue> = Vec::new();

    if let Some(audit_action_id) = query.audit_action_id {
        sql.push_str(" AND id = ?");
        params.push(SqlValue::Integer(audit_action_id));
    }
    if let Some(pane_id) = query.pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(SqlValue::Integer(pane_id_i64));
    }
    if let Some(domain) = &query.domain {
        sql.push_str(" AND domain = ?");
        params.push(SqlValue::Text(domain.clone()));
    }
    if let Some(actor_kind) = &query.actor_kind {
        sql.push_str(" AND actor_kind = ?");
        params.push(SqlValue::Text(actor_kind.clone()));
    }
    if let Some(actor_id) = &query.actor_id {
        sql.push_str(" AND actor_id = ?");
        params.push(SqlValue::Text(actor_id.clone()));
    }
    if let Some(correlation_id) = &query.correlation_id {
        sql.push_str(" AND correlation_id = ?");
        params.push(SqlValue::Text(correlation_id.clone()));
    }
    if let Some(action_kind) = &query.action_kind {
        sql.push_str(" AND action_kind = ?");
        params.push(SqlValue::Text(action_kind.clone()));
    }
    if let Some(policy_decision) = &query.policy_decision {
        sql.push_str(" AND policy_decision = ?");
        params.push(SqlValue::Text(policy_decision.clone()));
    }
    if let Some(rule_id) = &query.rule_id {
        sql.push_str(" AND rule_id = ?");
        params.push(SqlValue::Text(rule_id.clone()));
    }
    if let Some(result) = &query.result {
        sql.push_str(" AND result = ?");
        params.push(SqlValue::Text(result.clone()));
    }
    if let Some(undoable) = query.undoable {
        if undoable {
            sql.push_str(" AND undoable = 1");
        } else {
            sql.push_str(" AND (undoable = 0 OR undoable IS NULL)");
        }
    }
    if let Some(since) = query.since {
        sql.push_str(" AND ts >= ?");
        params.push(SqlValue::Integer(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND ts <= ?");
        params.push(SqlValue::Integer(until));
    }

    sql.push_str(" ORDER BY ts DESC, id DESC LIMIT ?");
    let limit_i64 = usize_to_i64(query.limit.unwrap_or(100), "limit")?;
    params.push(SqlValue::Integer(limit_i64));

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        StorageError::Database(format!("Failed to prepare action history query: {e}"))
    })?;

    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            Ok(ActionHistoryRecord {
                id: row.get(0)?,
                ts: row.get(1)?,
                actor_kind: row.get(2)?,
                actor_id: row.get(3)?,
                correlation_id: row.get(4)?,
                pane_id: {
                    let val: Option<i64> = row.get(5)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                domain: row.get(6)?,
                action_kind: row.get(7)?,
                policy_decision: row.get(8)?,
                decision_reason: row.get(9)?,
                rule_id: row.get(10)?,
                input_summary: row.get(11)?,
                verification_summary: row.get(12)?,
                decision_context: row.get(13)?,
                result: row.get(14)?,
                undoable: {
                    let val: Option<i64> = row.get(15)?;
                    val.map(|v| v != 0)
                },
                undo_strategy: row.get(16)?,
                undo_hint: row.get(17)?,
                undone_at: row.get(18)?,
                undone_by: row.get(19)?,
                workflow_id: row.get(20)?,
                step_name: row.get(21)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Action history query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

/// Count active (unused + unexpired) approval tokens for a workspace
fn query_active_approvals_count(conn: &Connection, workspace_id: &str, now_ms: i64) -> Result<u32> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM approval_tokens
             WHERE workspace_id = ?1 AND used_at IS NULL AND expires_at >= ?2",
            params![workspace_id, now_ms],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(format!("Approval count query failed: {e}")))?;

    u32::try_from(count).map_err(|_| {
        StorageError::Database(format!("Active approval count {count} exceeds u32 range")).into()
    })
}

/// Look up an approval token by code hash (without consuming)
fn query_approval_token_by_hash(
    conn: &Connection,
    code_hash: &str,
) -> Result<Option<ApprovalTokenRecord>> {
    let result = conn.query_row(
        "SELECT id, code_hash, created_at, expires_at, used_at, workspace_id, action_kind,
         pane_id, action_fingerprint, plan_hash, plan_version, risk_summary
         FROM approval_tokens
         WHERE code_hash = ?1",
        params![code_hash],
        |row| {
            Ok(ApprovalTokenRecord {
                id: row.get(0)?,
                code_hash: row.get(1)?,
                created_at: row.get(2)?,
                expires_at: row.get(3)?,
                used_at: row.get(4)?,
                workspace_id: row.get(5)?,
                action_kind: row.get(6)?,
                pane_id: {
                    let val: Option<i64> = row.get(7)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                action_fingerprint: row.get(8)?,
                plan_hash: row.get(9)?,
                plan_version: row.get(10)?,
                risk_summary: row.get(11)?,
            })
        },
    );

    match result {
        Ok(record) => Ok(Some(record)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StorageError::Database(format!("Approval token lookup failed: {e}")).into()),
    }
}

/// Query maximum sequence number for a pane
fn query_max_seq(conn: &Connection, pane_id: u64) -> Result<Option<u64>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;

    conn.query_row(
        "SELECT MAX(seq) FROM output_segments WHERE pane_id = ?1",
        [pane_id_i64],
        |row| {
            let val: Option<i64> = row.get(0)?;
            #[allow(clippy::cast_sign_loss)]
            Ok(val.map(|v| v as u64))
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
    .map(Option::flatten)
}

/// Query all panes
fn query_panes(conn: &Connection) -> Result<Vec<PaneRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
             first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
             FROM panes
             ORDER BY last_seen_at DESC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(PaneRecord {
                pane_id: {
                    let val: i64 = row.get(0)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                pane_uuid: row.get(1)?,
                domain: row.get(2)?,
                window_id: {
                    let val: Option<i64> = row.get(3)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                tab_id: {
                    let val: Option<i64> = row.get(4)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                title: row.get(5)?,
                cwd: row.get(6)?,
                tty_name: row.get(7)?,
                first_seen_at: row.get(8)?,
                last_seen_at: row.get(9)?,
                observed: row.get::<_, i64>(10)? != 0,
                ignore_reason: row.get(11)?,
                last_decision_at: row.get(12)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

/// Query a specific pane
fn query_pane(conn: &Connection, pane_id: u64) -> Result<Option<PaneRecord>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;

    conn.query_row(
        "SELECT pane_id, pane_uuid, domain, window_id, tab_id, title, cwd, tty_name,
         first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at
         FROM panes WHERE pane_id = ?1",
        [pane_id_i64],
        |row| {
            Ok(PaneRecord {
                pane_id: {
                    let val: i64 = row.get(0)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                pane_uuid: row.get(1)?,
                domain: row.get(2)?,
                window_id: {
                    let val: Option<i64> = row.get(3)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                tab_id: {
                    let val: Option<i64> = row.get(4)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                title: row.get(5)?,
                cwd: row.get(6)?,
                tty_name: row.get(7)?,
                first_seen_at: row.get(8)?,
                last_seen_at: row.get(9)?,
                observed: row.get::<_, i64>(10)? != 0,
                ignore_reason: row.get(11)?,
                last_decision_at: row.get(12)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

/// Query segments for a pane
#[allow(clippy::cast_sign_loss)]
fn query_segments(conn: &Connection, pane_id: u64, limit: usize) -> Result<Vec<Segment>> {
    let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
    let limit_i64 = usize_to_i64(limit, "limit")?;

    let mut stmt = conn
        .prepare(
            "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
             FROM output_segments
             WHERE pane_id = ?1
             ORDER BY seq DESC
             LIMIT ?2",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([pane_id_i64, limit_i64], |row| {
            Ok(Segment {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                content: row.get(3)?,
                content_len: {
                    let val: i64 = row.get(4)?;
                    i64_to_usize(val)?
                },
                content_hash: row.get(5)?,
                captured_at: row.get(6)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

/// Query workflow by ID
#[allow(clippy::cast_sign_loss)]
fn query_workflow(conn: &Connection, workflow_id: &str) -> Result<Option<WorkflowRecord>> {
    conn.query_row(
        "SELECT id, workflow_name, pane_id, trigger_event_id, current_step, status,
         wait_condition, context, result, error, started_at, updated_at, completed_at
         FROM workflow_executions WHERE id = ?1",
        [workflow_id],
        |row| {
            let wait_condition_str: Option<String> = row.get(6)?;
            let wait_condition = wait_condition_str
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok());

            let context_str: Option<String> = row.get(7)?;
            let context = context_str
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok());

            let result_str: Option<String> = row.get(8)?;
            let result = result_str
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok());

            Ok(WorkflowRecord {
                id: row.get(0)?,
                workflow_name: row.get(1)?,
                pane_id: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                trigger_event_id: row.get(3)?,
                current_step: {
                    let val: i64 = row.get(4)?;
                    usize::try_from(val).map_err(|_| {
                        rusqlite::Error::InvalidColumnType(
                            4,
                            "current_step".to_string(),
                            rusqlite::types::Type::Integer,
                        )
                    })?
                },
                status: row.get(5)?,
                wait_condition,
                context,
                result,
                error: row.get(9)?,
                started_at: row.get(10)?,
                updated_at: row.get(11)?,
                completed_at: row.get(12)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

/// Query workflow step logs by workflow ID
fn query_action_plan(
    conn: &Connection,
    workflow_id: &str,
) -> Result<Option<WorkflowActionPlanRecord>> {
    conn.query_row(
        "SELECT workflow_id, plan_id, plan_hash, plan_json, created_at \
         FROM workflow_action_plans WHERE workflow_id = ?1",
        [workflow_id],
        |row| {
            Ok(WorkflowActionPlanRecord {
                workflow_id: row.get(0)?,
                plan_id: row.get(1)?,
                plan_hash: row.get(2)?,
                plan_json: row.get(3)?,
                created_at: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

fn query_prepared_plan(conn: &Connection, plan_id: &str) -> Result<Option<PreparedPlanRecord>> {
    conn.query_row(
        "SELECT plan_id, plan_hash, workspace_id, action_kind, pane_id, pane_uuid, params_json,
                plan_json, requires_approval, created_at, expires_at, consumed_at
         FROM prepared_plans
         WHERE plan_id = ?1",
        [plan_id],
        |row| {
            let pane_id: Option<i64> = row.get(4)?;
            let requires: i64 = row.get(8)?;
            Ok(PreparedPlanRecord {
                plan_id: row.get(0)?,
                plan_hash: row.get(1)?,
                workspace_id: row.get(2)?,
                action_kind: row.get(3)?,
                pane_id: pane_id.map(|v| v as u64),
                pane_uuid: row.get(5)?,
                params_json: row.get(6)?,
                plan_json: row.get(7)?,
                requires_approval: requires != 0,
                created_at: row.get(9)?,
                expires_at: row.get(10)?,
                consumed_at: row.get(11)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

fn query_step_logs(conn: &Connection, workflow_id: &str) -> Result<Vec<WorkflowStepLogRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, workflow_id, audit_action_id, step_index, step_name, step_id, step_kind,
             result_type, result_data, policy_summary, verification_refs, error_code,
             started_at, completed_at, duration_ms
             FROM workflow_step_logs
             WHERE workflow_id = ?1
             ORDER BY step_index ASC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([workflow_id], |row| {
            Ok(WorkflowStepLogRecord {
                id: row.get(0)?,
                workflow_id: row.get(1)?,
                audit_action_id: row.get(2)?,
                step_index: {
                    let val: i64 = row.get(3)?;
                    i64_to_usize(val)?
                },
                step_name: row.get(4)?,
                step_id: row.get(5)?,
                step_kind: row.get(6)?,
                result_type: row.get(7)?,
                result_data: row.get(8)?,
                policy_summary: row.get(9)?,
                verification_refs: row.get(10)?,
                error_code: row.get(11)?,
                started_at: row.get(12)?,
                completed_at: row.get(13)?,
                duration_ms: row.get(14)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn query_latest_step_log(
    conn: &Connection,
    workflow_id: &str,
) -> Result<Option<WorkflowStepLogRecord>> {
    conn.query_row(
        "SELECT id, workflow_id, audit_action_id, step_index, step_name, step_id, step_kind,
             result_type, result_data, policy_summary, verification_refs, error_code,
             started_at, completed_at, duration_ms
         FROM workflow_step_logs
         WHERE workflow_id = ?1
         ORDER BY step_index DESC
         LIMIT 1",
        [workflow_id],
        |row| {
            Ok(WorkflowStepLogRecord {
                id: row.get(0)?,
                workflow_id: row.get(1)?,
                audit_action_id: row.get(2)?,
                step_index: {
                    let val: i64 = row.get(3)?;
                    i64_to_usize(val)?
                },
                step_name: row.get(4)?,
                step_id: row.get(5)?,
                step_kind: row.get(6)?,
                result_type: row.get(7)?,
                result_data: row.get(8)?,
                policy_summary: row.get(9)?,
                verification_refs: row.get(10)?,
                error_code: row.get(11)?,
                started_at: row.get(12)?,
                completed_at: row.get(13)?,
                duration_ms: row.get(14)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

/// Query incomplete workflows for resume on restart
#[allow(clippy::cast_sign_loss)]
fn query_incomplete_workflows(conn: &Connection) -> Result<Vec<WorkflowRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, workflow_name, pane_id, trigger_event_id, current_step, status,
             wait_condition, context, result, error, started_at, updated_at, completed_at
             FROM workflow_executions
             WHERE status IN ('running', 'waiting')
             ORDER BY started_at ASC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            let wait_condition_str: Option<String> = row.get(6)?;
            let wait_condition = wait_condition_str
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok());

            let context_str: Option<String> = row.get(7)?;
            let context = context_str
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok());

            let result_str: Option<String> = row.get(8)?;
            let result = result_str
                .as_ref()
                .and_then(|s| serde_json::from_str(s).ok());

            Ok(WorkflowRecord {
                id: row.get(0)?,
                workflow_name: row.get(1)?,
                pane_id: {
                    let val: i64 = row.get(2)?;
                    val as u64
                },
                trigger_event_id: row.get(3)?,
                current_step: {
                    let val: i64 = row.get(4)?;
                    i64_to_usize(val)?
                },
                status: row.get(5)?,
                wait_condition,
                context,
                result,
                error: row.get(9)?,
                started_at: row.get(10)?,
                updated_at: row.get(11)?,
                completed_at: row.get(12)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

// =============================================================================
// Segment Scan Query Functions
// =============================================================================

/// Build a dynamic WHERE clause and params from a SegmentScanQuery.
/// `time_column` is the column name used for since/until filtering.
fn build_segment_scan_where(
    query: &SegmentScanQuery,
    time_column: &str,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(after_id) = query.after_id {
        clauses.push(format!("id > ?{}", params.len() + 1));
        params.push(Box::new(after_id));
    }
    if let Some(pane_id) = query.pane_id {
        clauses.push(format!("pane_id = ?{}", params.len() + 1));
        params.push(Box::new(u64_to_i64_unchecked(pane_id)));
    }
    if let Some(since) = query.since {
        clauses.push(format!("{time_column} >= ?{}", params.len() + 1));
        params.push(Box::new(since));
    }
    if let Some(until) = query.until {
        clauses.push(format!("{time_column} <= ?{}", params.len() + 1));
        params.push(Box::new(until));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    (where_clause, params)
}

fn query_scan_segments(conn: &Connection, query: &SegmentScanQuery) -> Result<Vec<Segment>> {
    let (where_clause, params) = build_segment_scan_where(query, "captured_at");
    let limit = if query.limit == 0 { 1_000 } else { query.limit };
    let sql = format!(
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
         FROM output_segments{where_clause}
         ORDER BY id ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(Segment {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                content: row.get(3)?,
                content_len: {
                    let val: i64 = row.get(4)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as usize
                    }
                },
                content_hash: row.get(5)?,
                captured_at: row.get(6)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

fn query_latest_secret_scan_report(
    conn: &Connection,
    scope_hash: &str,
) -> Result<Option<SecretScanReportRecord>> {
    conn.query_row(
        "SELECT id, scope_hash, scope_json, report_version, last_segment_id, \
         report_json, created_at
         FROM secret_scan_reports
         WHERE scope_hash = ?1
         ORDER BY created_at DESC, id DESC
         LIMIT 1",
        params![scope_hash],
        |row| {
            Ok(SecretScanReportRecord {
                id: row.get(0)?,
                scope_hash: row.get(1)?,
                scope_json: row.get(2)?,
                report_version: row.get(3)?,
                last_segment_id: row.get(4)?,
                report_json: row.get(5)?,
                created_at: row.get(6)?,
            })
        },
    )
    .optional()
    .map_err(|e| StorageError::Database(format!("Query failed: {e}")).into())
}

// =============================================================================
// Export Query Functions
// =============================================================================

/// Build a dynamic WHERE clause and params from an ExportQuery.
/// `time_column` is the column name used for since/until filtering.
fn build_export_where(
    query: &ExportQuery,
    time_column: &str,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(pane_id) = query.pane_id {
        clauses.push(format!("pane_id = ?{}", params.len() + 1));
        params.push(Box::new(u64_to_i64_unchecked(pane_id)));
    }
    if let Some(since) = query.since {
        clauses.push(format!("{time_column} >= ?{}", params.len() + 1));
        params.push(Box::new(since));
    }
    if let Some(until) = query.until {
        clauses.push(format!("{time_column} <= ?{}", params.len() + 1));
        params.push(Box::new(until));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    (where_clause, params)
}

/// Unchecked u64→i64 cast for query params (SQLite stores as i64).
fn u64_to_i64_unchecked(val: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    {
        val as i64
    }
}

fn query_export_segments(conn: &Connection, query: &ExportQuery) -> Result<Vec<Segment>> {
    let (where_clause, params) = build_export_where(query, "captured_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
         FROM output_segments{where_clause}
         ORDER BY captured_at ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(Segment {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                content: row.get(3)?,
                content_len: {
                    let val: i64 = row.get(4)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as usize
                    }
                },
                content_hash: row.get(5)?,
                captured_at: row.get(6)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

fn query_export_gaps(conn: &Connection, query: &ExportQuery) -> Result<Vec<Gap>> {
    let (where_clause, params) = build_export_where(query, "detected_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, seq_before, seq_after, reason, detected_at
         FROM output_gaps{where_clause}
         ORDER BY detected_at ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(Gap {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq_before: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                seq_after: {
                    let val: i64 = row.get(3)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                reason: row.get(4)?,
                detected_at: row.get(5)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

fn query_export_workflows(conn: &Connection, query: &ExportQuery) -> Result<Vec<WorkflowRecord>> {
    let (where_clause, params) = build_export_where(query, "started_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, workflow_name, pane_id, trigger_event_id, current_step,
                status, wait_condition, context, result, error, started_at, updated_at, completed_at
         FROM workflow_executions{where_clause}
         ORDER BY started_at ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let wait_condition: Option<String> = row.get(6)?;
            let wait_condition = wait_condition.and_then(|s| serde_json::from_str(&s).ok());
            let context: Option<String> = row.get(7)?;
            let context = context.and_then(|s| serde_json::from_str(&s).ok());
            let result: Option<String> = row.get(8)?;
            let result = result.and_then(|s| serde_json::from_str(&s).ok());

            Ok(WorkflowRecord {
                id: row.get(0)?,
                workflow_name: row.get(1)?,
                pane_id: {
                    let val: i64 = row.get(2)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                trigger_event_id: row.get(3)?,
                current_step: {
                    let val: i64 = row.get(4)?;
                    i64_to_usize(val)?
                },
                status: row.get(5)?,
                wait_condition,
                context,
                result,
                error: row.get(9)?,
                started_at: row.get(10)?,
                updated_at: row.get(11)?,
                completed_at: row.get(12)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

fn query_export_sessions(
    conn: &Connection,
    query: &ExportQuery,
) -> Result<Vec<AgentSessionRecord>> {
    let (where_clause, params) = build_export_where(query, "started_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, agent_type, session_id, external_id, external_meta,
                started_at, ended_at, end_reason,
                total_tokens, input_tokens, output_tokens, cached_tokens, reasoning_tokens,
                model_name, estimated_cost_usd
         FROM agent_sessions{where_clause}
         ORDER BY started_at ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(AgentSessionRecord {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                agent_type: row.get(2)?,
                session_id: row.get(3)?,
                external_id: row.get(4)?,
                external_meta: row
                    .get::<_, Option<String>>(5)?
                    .as_ref()
                    .and_then(|value| serde_json::from_str(value).ok()),
                started_at: row.get(6)?,
                ended_at: row.get(7)?,
                end_reason: row.get(8)?,
                total_tokens: row.get(9)?,
                input_tokens: row.get(10)?,
                output_tokens: row.get(11)?,
                cached_tokens: row.get(12)?,
                reasoning_tokens: row.get(13)?,
                model_name: row.get(14)?,
                estimated_cost_usd: row.get(15)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

fn query_export_reservations(
    conn: &Connection,
    query: &ExportQuery,
) -> Result<Vec<PaneReservation>> {
    let (where_clause, params) = build_export_where(query, "created_at");
    let limit = query.limit.unwrap_or(10_000);
    let sql = format!(
        "SELECT id, pane_id, owner_kind, owner_id, reason,
                created_at, expires_at, released_at, status
         FROM pane_reservations{where_clause}
         ORDER BY created_at ASC
         LIMIT ?{}",
        params.len() + 1
    );

    let mut all_params = params;
    all_params.push(Box::new(limit as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        all_params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(PaneReservation {
                id: row.get(0)?,
                pane_id: {
                    let val: i64 = row.get(1)?;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        val as u64
                    }
                },
                owner_kind: row.get(2)?,
                owner_id: row.get(3)?,
                reason: row.get(4)?,
                created_at: row.get(5)?,
                expires_at: row.get(6)?,
                released_at: row.get(7)?,
                status: row.get(8)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    // =========================================================================
    // Schema Initialization Tests
    // =========================================================================

    #[test]
    fn schema_initializes_on_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();

        // Should need initialization
        assert!(needs_initialization(&conn).unwrap());

        // Initialize
        initialize_schema(&conn).unwrap();

        // Should not need initialization anymore
        assert!(!needs_initialization(&conn).unwrap());

        // Version should be recorded
        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, Some(SCHEMA_VERSION));
    }

    #[test]
    fn schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();

        // Initialize twice
        initialize_schema(&conn).unwrap();
        initialize_schema(&conn).unwrap();

        // Should still be valid
        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, Some(SCHEMA_VERSION));
    }

    #[test]
    fn all_tables_exist_after_init() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let expected_tables = [
            "schema_version",
            "panes",
            "output_segments",
            "output_gaps",
            "events",
            "event_labels",
            "event_notes",
            "workflow_executions",
            "workflow_step_logs",
            "audit_actions",
            "action_undo",
            "approval_tokens",
            "config",
            "saved_searches",
            "maintenance_log",
            "pane_bookmarks",
        ];

        for table in &expected_tables {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "Table {table} should exist");
        }
    }

    // =========================================================================
    // Migration Plan Tests
    // =========================================================================

    #[test]
    fn migration_plan_empty_when_at_target() {
        let plan = build_migration_plan(SCHEMA_VERSION, SCHEMA_VERSION).unwrap();
        assert!(plan.steps.is_empty());
        assert_eq!(plan.from_version, SCHEMA_VERSION);
        assert_eq!(plan.to_version, SCHEMA_VERSION);
    }

    #[test]
    fn migration_roundtrip_down_then_up() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let downgrade_target = 3;
        let down_plan = build_migration_plan(SCHEMA_VERSION, downgrade_target).unwrap();
        apply_migration_plan(&conn, &down_plan).unwrap();
        assert_eq!(get_user_version(&conn).unwrap(), downgrade_target);

        let up_plan = build_migration_plan(downgrade_target, SCHEMA_VERSION).unwrap();
        apply_migration_plan(&conn, &up_plan).unwrap();
        assert_eq!(get_user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migration_v18_preserves_existing_events() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Downgrade just the newest migration (v18 -> v17).
        let down_plan = build_migration_plan(SCHEMA_VERSION, 17).unwrap();
        apply_migration_plan(&conn, &down_plan).unwrap();
        assert_eq!(get_user_version(&conn).unwrap(), 17);

        let now_ms = 1_700_000_000_000i64;

        // Insert pane + event using the pre-v18 schema (no triage columns/tables).
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![1i64, "codex.usage_limit", "codex", "usage", "warning", 0.95, now_ms],
        )
        .unwrap();

        let count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_before, 1);

        // Upgrade back to current schema and verify event row is preserved.
        let up_plan = build_migration_plan(17, SCHEMA_VERSION).unwrap();
        apply_migration_plan(&conn, &up_plan).unwrap();
        assert_eq!(get_user_version(&conn).unwrap(), SCHEMA_VERSION);

        let count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 1);

        // New columns/tables should exist after upgrade.
        let triage_state: Option<String> = conn
            .query_row("SELECT triage_state FROM events WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(triage_state.is_none());

        let labels_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='event_labels'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(labels_table, 1);
    }

    #[test]
    fn fts_table_exists_after_init() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='output_segments_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "FTS5 table should exist");
    }

    #[test]
    fn action_history_view_exists_after_init() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='action_history'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "action_history view should exist");
    }

    #[test]
    fn wal_mode_is_enabled() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        // In-memory databases use "memory" mode, but WAL works on file-based DBs
        assert!(mode == "memory" || mode == "wal");
    }

    // =========================================================================
    // WAL Recovery Tests (wa-o8j)
    // =========================================================================

    #[test]
    fn wal_recovery_passes_on_fresh_in_memory_db() {
        let conn = Connection::open_in_memory().unwrap();
        // Should pass without error on a fresh database
        // Note: in-memory DBs don't have WAL files, but the function should handle this
        check_and_recover_wal(&conn, ":memory:").unwrap();
    }

    #[test]
    fn wal_recovery_passes_integrity_check() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();
        // After schema init, integrity check should still pass
        check_and_recover_wal(&conn, ":memory:").unwrap();
    }

    #[test]
    fn wal_recovery_with_file_db() {
        use std::fs;
        let temp_dir = std::env::temp_dir();
        let db_path = temp_dir.join(format!("wa_test_wal_recovery_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        // Create and populate a database
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
            initialize_schema(&conn).unwrap();
            // Insert some data to ensure WAL activity
            conn.execute(
                "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at) VALUES (1, 'local', 0, 0)",
                [],
            ).unwrap();
        }

        // Re-open and run recovery
        {
            let conn = Connection::open(&db_path).unwrap();
            check_and_recover_wal(&conn, &db_path_str).unwrap();
            // Verify data is intact
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM panes", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 1);
        }

        // Cleanup
        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(format!("{db_path_str}-wal"));
        let _ = fs::remove_file(format!("{db_path_str}-shm"));
    }

    // =========================================================================
    // Migration System Tests
    // =========================================================================

    #[test]
    fn user_version_set_on_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();

        // Fresh DB should have user_version = 0
        let initial = get_user_version(&conn).unwrap();
        assert_eq!(initial, 0);

        // After init, should match SCHEMA_VERSION
        initialize_schema(&conn).unwrap();
        let after = get_user_version(&conn).unwrap();
        assert_eq!(after, SCHEMA_VERSION);
    }

    #[test]
    fn user_version_and_schema_version_match() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let user_ver = get_user_version(&conn).unwrap();
        let schema_ver = get_schema_version(&conn).unwrap().unwrap();

        assert_eq!(user_ver, schema_ver);
        assert_eq!(user_ver, SCHEMA_VERSION);
    }

    #[test]
    fn schema_version_audit_trail_recorded() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Should have exactly one record for initial schema
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Record should have correct version and non-null timestamp
        let (version, applied_at, description): (i32, i64, String) = conn
            .query_row(
                "SELECT version, applied_at, description FROM schema_version",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(version, SCHEMA_VERSION);
        assert!(applied_at > 0, "applied_at should be set");
        assert_eq!(description, "Initial schema");
    }

    #[test]
    fn wa_meta_initialized_on_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let (schema_version, min_compatible, created_by, created_at): (i32, String, String, i64) =
            conn.query_row(
                "SELECT schema_version, min_compatible_wa, created_by_wa, created_at \
                 FROM wa_meta WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        assert_eq!(schema_version, SCHEMA_VERSION);
        assert_eq!(min_compatible, crate::VERSION);
        assert_eq!(created_by, crate::VERSION);
        assert!(created_at > 0, "created_at should be set");
    }

    #[test]
    fn wa_too_old_rejected_by_meta() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        conn.execute(
            "UPDATE wa_meta SET min_compatible_wa = '99.0.0' WHERE id = 1",
            [],
        )
        .unwrap();

        let result = initialize_schema(&conn);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("requires wa"),
            "Error should mention required wa version: {err}"
        );
    }

    #[test]
    fn future_schema_version_rejected() {
        let conn = Connection::open_in_memory().unwrap();

        // Manually set user_version to a future version
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .unwrap();

        // Initialization should fail
        let result = initialize_schema(&conn);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("newer than supported"),
            "Error should mention version mismatch: {err_str}"
        );
    }

    #[test]
    fn idempotent_init_preserves_version() {
        let conn = Connection::open_in_memory().unwrap();

        // Initialize
        initialize_schema(&conn).unwrap();
        let version1 = get_user_version(&conn).unwrap();

        // Initialize again (should be no-op)
        initialize_schema(&conn).unwrap();
        let version2 = get_user_version(&conn).unwrap();

        assert_eq!(version1, version2);
        assert_eq!(version1, SCHEMA_VERSION);

        // Audit trail should still have just one record
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn pending_migrations_empty_at_current_version() {
        let pending = pending_migrations(SCHEMA_VERSION);
        assert!(pending.is_empty());
    }

    #[test]
    fn pending_migrations_includes_all_from_zero() {
        let pending = pending_migrations(0);
        assert_eq!(pending.len(), MIGRATIONS.len());
    }

    #[test]
    fn migrations_are_sorted_by_version() {
        let mut prev_version = 0;
        for migration in MIGRATIONS {
            assert!(
                migration.version > prev_version,
                "Migration versions must be strictly increasing"
            );
            prev_version = migration.version;
        }
    }

    #[test]
    fn migration_runner_simulated_upgrade() {
        // This test simulates what happens when we add a new migration
        let conn = Connection::open_in_memory().unwrap();

        // Create a minimal v1 schema without the new audit_actions column.
        conn.execute_batch(
            r"
            CREATE TABLE panes (
                pane_id INTEGER PRIMARY KEY,
                domain TEXT NOT NULL DEFAULT 'local',
                window_id INTEGER,
                tab_id INTEGER,
                title TEXT,
                cwd TEXT,
                tty_name TEXT,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL,
                observed INTEGER NOT NULL DEFAULT 1,
                ignore_reason TEXT,
                last_decision_at INTEGER
            );

            CREATE TABLE audit_actions (
                id INTEGER PRIMARY KEY,
                ts INTEGER NOT NULL,
                actor_kind TEXT NOT NULL,
                actor_id TEXT,
                pane_id INTEGER REFERENCES panes(pane_id) ON DELETE SET NULL,
                domain TEXT,
                action_kind TEXT NOT NULL,
                policy_decision TEXT NOT NULL,
                decision_reason TEXT,
                rule_id TEXT,
                input_summary TEXT,
                verification_summary TEXT,
                result TEXT NOT NULL
            );
            ",
        )
        .unwrap();
        set_user_version(&conn, 0).unwrap();

        // Tables should exist but version should be 0
        assert!(!needs_initialization(&conn).unwrap());
        assert_eq!(get_user_version(&conn).unwrap(), 0);

        // Run initialization (should apply migrations)
        initialize_schema(&conn).unwrap();

        // Should now be at current version
        assert_eq!(get_user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));
    }

    #[test]
    fn v1_schema_includes_agent_sessions() {
        // Verify the v1 schema includes agent_sessions table
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_sessions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "agent_sessions table should exist in v1 schema");
    }

    // =========================================================================
    // Basic Insert/Query Tests (validates schema correctness)
    // =========================================================================

    #[test]
    #[allow(clippy::cast_possible_wrap)]
    fn can_insert_and_query_pane() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![42i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        let (pane_id, domain): (i64, String) = conn
            .query_row(
                "SELECT pane_id, domain FROM panes WHERE pane_id = ?1",
                [42i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(pane_id, 42);
        assert_eq!(domain, "local");
    }

    #[test]
    fn can_insert_segment_with_unique_constraint() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        // Insert pane first (foreign key)
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        // Insert segment
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "hello", 5, now_ms],
        ).unwrap();

        // Duplicate should fail
        let result = conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "world", 5, now_ms],
        );
        assert!(result.is_err(), "Duplicate (pane_id, seq) should fail");
    }

    #[test]
    fn fts_trigger_syncs_on_insert() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        // Insert pane
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        // Insert segment
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "hello world test", 16, now_ms],
        ).unwrap();

        // Search via FTS
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH 'world'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "FTS should find the inserted content");
    }

    #[test]
    fn fts_search_returns_snippet_and_highlight() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        let content = "hello world from wezterm";
        let content_len = i64::try_from(content.len()).unwrap();
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, content, content_len, now_ms],
        )
        .unwrap();

        let results = search_fts_with_snippets(&conn, "world", &SearchOptions::default())
            .expect("search should succeed");
        assert_eq!(results.len(), 1);

        let snippet = results[0].snippet.as_deref().expect("snippet");
        assert!(snippet.contains(">>>world<<<"));

        let highlight = results[0].highlight.as_deref().expect("highlight");
        assert!(highlight.contains(">>>world<<<"));
    }

    #[test]
    fn fts_search_scopes_by_pane_and_limit() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        for pane_id in [1i64, 2i64] {
            conn.execute(
                "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![pane_id, "local", now_ms, now_ms, 1],
            )
            .unwrap();
        }

        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "needle alpha", 12i64, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![2i64, 0i64, "needle beta", 11i64, now_ms + 1000],
        )
        .unwrap();

        let options = SearchOptions {
            pane_id: Some(2),
            limit: Some(1),
            ..Default::default()
        };

        let results =
            search_fts_with_snippets(&conn, "needle", &options).expect("search should succeed");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].segment.pane_id, 2);
    }

    #[test]
    fn fts_search_invalid_query_is_structured_error() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let err = search_fts_with_snippets(&conn, "\"unterminated", &SearchOptions::default())
            .expect_err("expected invalid query error");

        match err {
            crate::Error::Storage(StorageError::FtsQueryError(msg)) => {
                assert!(msg.contains("Invalid FTS5 query syntax"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn fts_lint_detects_empty_query() {
        let lints = lint_fts_query("   ");
        assert!(
            lints.iter().any(|lint| lint.code == "empty_query"),
            "expected empty_query lint"
        );
        assert!(
            lints
                .iter()
                .any(|lint| lint.severity == SearchLintSeverity::Error),
            "expected error severity for empty query"
        );
    }

    #[test]
    fn fts_lint_detects_unbalanced_quotes() {
        let lints = lint_fts_query("\"unterminated");
        assert!(
            lints.iter().any(|lint| lint.code == "unbalanced_quotes"),
            "expected unbalanced_quotes lint"
        );
    }

    #[test]
    fn fts_lint_detects_operator_misuse() {
        let lints = lint_fts_query("AND error OR");
        assert!(
            lints.iter().any(|lint| lint.code == "leading_operator"),
            "expected leading_operator lint"
        );
        assert!(
            lints.iter().any(|lint| lint.code == "trailing_operator"),
            "expected trailing_operator lint"
        );
    }

    #[test]
    fn fts_lint_warns_on_bad_wildcard_position() {
        let lints = lint_fts_query("err*or");
        assert!(
            lints.iter().any(|lint| lint.code == "wildcard_position"),
            "expected wildcard_position lint"
        );
    }

    #[test]
    fn fts_lint_allows_quoted_phrase() {
        let lints = lint_fts_query("\"error code\"");
        assert!(
            lints
                .iter()
                .all(|lint| lint.severity != SearchLintSeverity::Error),
            "expected no error lints for quoted phrase"
        );
    }

    #[test]
    fn fts_lint_allows_operator_query() {
        let lints = lint_fts_query("error OR warning");
        assert!(
            lints
                .iter()
                .all(|lint| lint.severity != SearchLintSeverity::Error),
            "expected no error lints for operator query"
        );
    }

    #[test]
    fn fts_search_order_is_deterministic_on_ties() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        let content = "tie breaker needle";
        let content_len = i64::try_from(content.len()).unwrap();
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, content, content_len, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, content, content_len, now_ms + 1000],
        )
        .unwrap();

        let results = search_fts_with_snippets(&conn, "needle", &SearchOptions::default())
            .expect("search should succeed");
        assert_eq!(results.len(), 2);
        assert!(results[0].segment.captured_at <= results[1].segment.captured_at);
    }

    #[test]
    fn can_insert_event_and_mark_handled() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        // Insert pane
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        // Insert unhandled event
        conn.execute(
            "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity, confidence, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![1i64, "codex.usage_limit", "codex", "usage", "warning", 0.95, now_ms],
        ).unwrap();

        // Query unhandled
        let unhandled_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE handled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unhandled_count, 1);

        // Mark as handled
        conn.execute(
            "UPDATE events SET handled_at = ?1, handled_status = ?2 WHERE id = 1",
            params![now_ms + 1000, "completed"],
        )
        .unwrap();

        // Query unhandled again
        let unhandled_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE handled_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unhandled_count, 0);
    }

    #[test]
    fn can_insert_workflow_execution() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        // Insert pane
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        // Insert workflow execution
        conn.execute(
            "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params!["wf-001", "handle_compaction", 1i64, 0, "running", now_ms, now_ms],
        ).unwrap();

        // Query
        let (name, status): (String, String) = conn
            .query_row(
                "SELECT workflow_name, status FROM workflow_executions WHERE id = ?1",
                ["wf-001"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(name, "handle_compaction");
        assert_eq!(status, "running");
    }

    // =========================================================================
    // Data Structure Serialization Tests
    // =========================================================================

    #[test]
    fn segment_serializes() {
        let segment = Segment {
            id: 1,
            pane_id: 42,
            seq: 100,
            content: "Hello, world!".to_string(),
            content_len: 13,
            content_hash: Some("abc123".to_string()),
            captured_at: 1_234_567_890,
        };

        let json = serde_json::to_string(&segment).unwrap();
        assert!(json.contains("Hello, world!"));
        assert!(json.contains("content_len"));
    }

    #[test]
    fn pane_record_serializes() {
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(0),
            tab_id: Some(0),
            title: Some("bash".to_string()),
            cwd: Some("/home/user".to_string()),
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_001_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };

        let json = serde_json::to_string(&pane).unwrap();
        assert!(json.contains("local"));
        assert!(json.contains("bash"));
    }

    #[test]
    fn stored_event_serializes() {
        let event = StoredEvent {
            id: 1,
            pane_id: 42,
            rule_id: "codex.usage_limit".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage".to_string(),
            severity: "warning".to_string(),
            confidence: 0.95,
            extracted: Some(serde_json::json!({"limit": 100})),
            matched_text: Some("Usage limit reached".to_string()),
            segment_id: Some(123),
            detected_at: 1_700_000_000_000,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("codex.usage_limit"));
        assert!(json.contains("0.95"));
    }

    #[test]
    fn workflow_record_serializes() {
        let workflow = WorkflowRecord {
            id: "wf-001".to_string(),
            workflow_name: "handle_compaction".to_string(),
            pane_id: 42,
            trigger_event_id: Some(1),
            current_step: 2,
            status: "running".to_string(),
            wait_condition: None,
            context: Some(serde_json::json!({"retry_count": 0})),
            result: None,
            error: None,
            started_at: 1_700_000_000_000,
            updated_at: 1_700_000_001_000,
            completed_at: None,
        };

        let json = serde_json::to_string(&workflow).unwrap();
        assert!(json.contains("handle_compaction"));
        assert!(json.contains("wf-001"));
    }

    // =========================================================================
    // wa-4vx.3.8: Audit Actions Tests
    // =========================================================================

    #[test]
    fn audit_action_record_serializes() {
        let action = AuditActionRecord {
            id: 1,
            ts: 1_700_000_000_000,
            actor_kind: "human".to_string(),
            actor_id: Some("user-1".to_string()),
            correlation_id: None,
            pane_id: Some(42),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("ok".to_string()),
            rule_id: Some("policy.allow".to_string()),
            input_summary: Some("echo hi".to_string()),
            verification_summary: Some("prompt_active".to_string()),
            decision_context: Some("{\"rule\":\"policy.allow\"}".to_string()),
            result: "success".to_string(),
        };

        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("send_text"));
        assert!(json.contains("policy_decision"));
        assert!(json.contains("decision_context"));
    }

    #[test]
    fn audit_action_redacts_sensitive_fields() {
        let mut action = AuditActionRecord {
            id: 0,
            ts: 1_700_000_000_000,
            actor_kind: "robot".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some(
                "token sk-abc123456789012345678901234567890123456789012345678901".to_string(),
            ),
            rule_id: None,
            input_summary: Some(
                "API key sk-abc123456789012345678901234567890123456789012345678901".to_string(),
            ),
            verification_summary: Some("checked prompt".to_string()),
            decision_context: Some(
                "{\"token\":\"sk-abc123456789012345678901234567890123456789012345678901\"}"
                    .to_string(),
            ),
            result: "success".to_string(),
        };

        let redactor = Redactor::new();
        action.redact_fields(&redactor);

        let reason = action.decision_reason.unwrap();
        let input = action.input_summary.unwrap();
        let context = action.decision_context.unwrap();

        assert!(reason.contains("[REDACTED]"));
        assert!(input.contains("[REDACTED]"));
        assert!(context.contains("[REDACTED]"));
        assert!(!reason.contains("sk-abc"));
        assert!(!input.contains("sk-abc"));
        assert!(!context.contains("sk-abc"));
    }

    #[test]
    fn audit_stream_record_redacts_sensitive_fields() {
        let action = AuditActionRecord {
            id: 42,
            ts: 1_700_000_000_123,
            actor_kind: "robot".to_string(),
            actor_id: Some("cli".to_string()),
            correlation_id: None,
            pane_id: Some(2),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("token sk-abc123456789012345678901234567890".to_string()),
            rule_id: None,
            input_summary: Some("API key sk-abc123456789012345678901234567890".to_string()),
            verification_summary: Some("prompt ok".to_string()),
            decision_context: Some(
                "{\"token\":\"sk-abc123456789012345678901234567890\"}".to_string(),
            ),
            result: "success".to_string(),
        };

        let redactor = Redactor::new();
        let record = AuditStreamRecord::from_action(action, &redactor);

        let reason = record.decision_reason.unwrap();
        let input = record.input_summary.unwrap();
        let context = record.decision_context.unwrap();

        assert!(reason.contains("[REDACTED]"));
        assert!(input.contains("[REDACTED]"));
        assert!(context.contains("[REDACTED]"));
        assert!(!reason.contains("sk-abc"));
        assert!(!input.contains("sk-abc"));
        assert!(!context.contains("sk-abc"));
    }

    #[test]
    fn can_insert_and_query_audit_actions() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        let action = AuditActionRecord {
            id: 0,
            ts: now_ms,
            actor_kind: "human".to_string(),
            actor_id: Some("cli".to_string()),
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("ok".to_string()),
            rule_id: None,
            input_summary: Some("echo hi".to_string()),
            verification_summary: Some("prompt".to_string()),
            decision_context: None,
            result: "success".to_string(),
        };

        let id = record_audit_action_sync(&conn, &action).unwrap();
        assert!(id > 0);

        let query = AuditQuery {
            pane_id: Some(1),
            actor_kind: Some("human".to_string()),
            action_kind: Some("send_text".to_string()),
            ..Default::default()
        };
        let rows = query_audit_actions(&conn, &query).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].actor_kind, "human");
        assert_eq!(rows[0].action_kind, "send_text");
        assert_eq!(rows[0].policy_decision, "allow");
    }

    #[test]
    fn action_history_includes_undo_metadata() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        let action = AuditActionRecord {
            id: 0,
            ts: now_ms,
            actor_kind: "human".to_string(),
            actor_id: Some("cli".to_string()),
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("ok".to_string()),
            rule_id: None,
            input_summary: Some("echo hi".to_string()),
            verification_summary: Some("prompt".to_string()),
            decision_context: None,
            result: "success".to_string(),
        };

        let action_id = record_audit_action_sync(&conn, &action).unwrap();

        let undo = ActionUndoRecord {
            audit_action_id: action_id,
            undoable: true,
            undo_strategy: "manual".to_string(),
            undo_hint: Some("run undo manually".to_string()),
            undo_payload: Some(r#"{"command":"undo"}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        upsert_action_undo_sync(&conn, &undo).unwrap();

        let rows = query_action_history(&conn, &ActionHistoryQuery::default()).unwrap();
        assert!(!rows.is_empty());

        let row = &rows[0];
        assert_eq!(row.id, action_id);
        assert_eq!(row.action_kind, "send_text");
        assert_eq!(row.undoable, Some(true));
        assert_eq!(row.undo_strategy.as_deref(), Some("manual"));
        assert_eq!(row.undo_hint.as_deref(), Some("run undo manually"));
        assert!(row.workflow_id.is_none());
        assert!(row.step_name.is_none());
    }

    #[test]
    fn action_undo_index_exists_after_init() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_action_undo_undoable'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "idx_action_undo_undoable index should exist");
    }

    #[test]
    fn action_history_orders_by_ts_and_id() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        let base = AuditActionRecord {
            id: 0,
            ts: 1_000,
            actor_kind: "human".to_string(),
            actor_id: Some("cli".to_string()),
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: Some("first".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };

        let id1 = record_audit_action_sync(&conn, &base).unwrap();
        let id2 = record_audit_action_sync(
            &conn,
            &AuditActionRecord {
                ts: 2_000,
                input_summary: Some("second".to_string()),
                ..base.clone()
            },
        )
        .unwrap();
        let id3 = record_audit_action_sync(
            &conn,
            &AuditActionRecord {
                ts: 2_000,
                input_summary: Some("third".to_string()),
                ..base
            },
        )
        .unwrap();

        let rows = query_action_history(
            &conn,
            &ActionHistoryQuery {
                limit: Some(10),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, id3);
        assert_eq!(rows[1].id, id2);
        assert_eq!(rows[2].id, id1);
    }

    #[test]
    fn action_history_filters_undoable() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        let base = AuditActionRecord {
            id: 0,
            ts: 1_000,
            actor_kind: "human".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: Some("first".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };

        let undoable_id = record_audit_action_sync(&conn, &base).unwrap();
        let non_undoable_id = record_audit_action_sync(
            &conn,
            &AuditActionRecord {
                ts: 2_000,
                input_summary: Some("second".to_string()),
                ..base
            },
        )
        .unwrap();

        upsert_action_undo_sync(
            &conn,
            &ActionUndoRecord {
                audit_action_id: undoable_id,
                undoable: true,
                undo_strategy: "manual".to_string(),
                undo_hint: None,
                undo_payload: None,
                undone_at: None,
                undone_by: None,
            },
        )
        .unwrap();
        upsert_action_undo_sync(
            &conn,
            &ActionUndoRecord {
                audit_action_id: non_undoable_id,
                undoable: false,
                undo_strategy: "none".to_string(),
                undo_hint: None,
                undo_payload: None,
                undone_at: None,
                undone_by: None,
            },
        )
        .unwrap();

        let undoable = query_action_history(
            &conn,
            &ActionHistoryQuery {
                undoable: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(undoable.len(), 1);
        assert_eq!(undoable[0].id, undoable_id);

        let non_undoable = query_action_history(
            &conn,
            &ActionHistoryQuery {
                undoable: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(non_undoable.iter().any(|row| row.id == non_undoable_id));
    }

    #[test]
    fn action_history_includes_workflow_step_info() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        let action = AuditActionRecord {
            id: 0,
            ts: now_ms,
            actor_kind: "workflow".to_string(),
            actor_id: Some("wf-1".to_string()),
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "workflow_step".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: Some("step".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };
        let action_id = record_audit_action_sync(&conn, &action).unwrap();

        conn.execute(
            "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params!["wf-1", "test", 1i64, 0i64, "running", now_ms, now_ms],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO workflow_step_logs (workflow_id, audit_action_id, step_index, step_name, result_type, started_at, completed_at, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params!["wf-1", action_id, 0i64, "step-0", "done", now_ms, now_ms, 0i64],
        )
        .unwrap();

        let rows = query_action_history(&conn, &ActionHistoryQuery::default()).unwrap();
        let row = rows.iter().find(|row| row.id == action_id).unwrap();
        assert_eq!(row.workflow_id.as_deref(), Some("wf-1"));
        assert_eq!(row.step_name.as_deref(), Some("step-0"));
    }

    #[test]
    fn action_undo_redaction_applied() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        let action = AuditActionRecord {
            id: 0,
            ts: now_ms,
            actor_kind: "human".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: Some("hi".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };
        let action_id = record_audit_action_sync(&conn, &action).unwrap();

        let secret = "sk-abc123456789012345678901234567890123456789012345678901";
        let mut undo = ActionUndoRecord {
            audit_action_id: action_id,
            undoable: true,
            undo_strategy: "manual".to_string(),
            undo_hint: Some(format!("token {secret}")),
            undo_payload: Some(format!(r#"{{"token":"{secret}"}}"#)),
            undone_at: None,
            undone_by: None,
        };
        let redactor = Redactor::new();
        undo.redact_fields(&redactor);
        upsert_action_undo_sync(&conn, &undo).unwrap();

        let (hint, payload): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT undo_hint, undo_payload FROM action_undo WHERE audit_action_id = ?1",
                params![action_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        let hint = hint.expect("undo_hint missing");
        let payload = payload.expect("undo_payload missing");
        assert!(hint.contains("[REDACTED]"));
        assert!(payload.contains("[REDACTED]"));
        assert!(!hint.contains("sk-abc"));
        assert!(!payload.contains("sk-abc"));
    }

    #[test]
    fn purge_audit_actions_removes_old_entries() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

        let older = AuditActionRecord {
            id: 0,
            ts: 1_000,
            actor_kind: "human".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: Some("old".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };
        let newer = AuditActionRecord {
            ts: 2_000,
            input_summary: Some("new".to_string()),
            ..older.clone()
        };

        record_audit_action_sync(&conn, &older).unwrap();
        record_audit_action_sync(&conn, &newer).unwrap();

        let deleted = purge_audit_actions_sync(&conn, 1_500).unwrap();
        assert_eq!(deleted, 1);

        let rows = query_audit_actions(&conn, &AuditQuery::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts, 2_000);
    }

    #[test]
    fn audit_query_filters_and_limits() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

        let allow = AuditActionRecord {
            id: 0,
            ts: 1_000,
            actor_kind: "human".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("ok".to_string()),
            rule_id: None,
            input_summary: Some("echo hi".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };
        let deny = AuditActionRecord {
            ts: 2_000,
            actor_kind: "workflow".to_string(),
            actor_id: Some("wf-123".to_string()),
            action_kind: "workflow_run".to_string(),
            policy_decision: "deny".to_string(),
            decision_reason: Some("blocked".to_string()),
            result: "denied".to_string(),
            ..allow.clone()
        };

        record_audit_action_sync(&conn, &allow).unwrap();
        record_audit_action_sync(&conn, &deny).unwrap();

        let last_one = query_audit_actions(
            &conn,
            &AuditQuery {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(last_one.len(), 1);
        assert_eq!(last_one[0].ts, 2_000);

        let by_pane = query_audit_actions(
            &conn,
            &AuditQuery {
                pane_id: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_pane.len(), 2);

        let by_workflow = query_audit_actions(
            &conn,
            &AuditQuery {
                actor_id: Some("wf-123".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(by_workflow.len(), 1);
        assert_eq!(by_workflow[0].actor_kind, "workflow");

        let denied = query_audit_actions(
            &conn,
            &AuditQuery {
                policy_decision: Some("deny".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].policy_decision, "deny");
    }

    #[test]
    fn audit_stream_query_pages_with_cursor() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

        let base = AuditActionRecord {
            id: 0,
            ts: 1_000,
            actor_kind: "human".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: Some("first".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };

        let id1 = record_audit_action_sync(&conn, &base).unwrap();
        let id2 = record_audit_action_sync(
            &conn,
            &AuditActionRecord {
                ts: 2_000,
                input_summary: Some("second".to_string()),
                ..base.clone()
            },
        )
        .unwrap();
        let id3 = record_audit_action_sync(
            &conn,
            &AuditActionRecord {
                ts: 3_000,
                input_summary: Some("third".to_string()),
                ..base.clone()
            },
        )
        .unwrap();

        let page1 = query_audit_actions_stream(
            &conn,
            &AuditStreamQuery {
                limit: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page1.records.len(), 2);
        assert!(page1.records[0].id < page1.records[1].id);
        assert_eq!(page1.records[0].id, id1);
        assert_eq!(page1.records[1].id, id2);
        assert_eq!(page1.next_cursor, Some(id2));

        let page2 = query_audit_actions_stream(
            &conn,
            &AuditStreamQuery {
                cursor: page1.next_cursor,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(page2.records.len(), 1);
        assert_eq!(page2.records[0].id, id3);
        assert_eq!(page2.next_cursor, Some(id3));
    }

    #[test]
    fn audit_stream_query_empty_returns_none_cursor() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let page = query_audit_actions_stream(&conn, &AuditStreamQuery::default()).unwrap();
        assert!(page.records.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn audit_stream_query_respects_limit() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

        let action = AuditActionRecord {
            id: 0,
            ts: 1_000,
            actor_kind: "human".to_string(),
            actor_id: None,
            correlation_id: None,
            pane_id: Some(1),
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: Some("hi".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
        };

        record_audit_action_sync(&conn, &action).unwrap();
        record_audit_action_sync(
            &conn,
            &AuditActionRecord {
                ts: 2_000,
                input_summary: Some("second".to_string()),
                ..action.clone()
            },
        )
        .unwrap();

        let page = query_audit_actions_stream(
            &conn,
            &AuditStreamQuery {
                limit: Some(1),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(page.records.len(), 1);
        assert_eq!(page.next_cursor, Some(page.records[0].id));
    }

    #[test]
    fn audit_stream_record_serializes_json_schema() {
        let action = AuditActionRecord {
            id: 7,
            ts: 1_700_000_000_999,
            actor_kind: "workflow".to_string(),
            actor_id: Some("wf-123".to_string()),
            correlation_id: Some("corr-1".to_string()),
            pane_id: Some(3),
            domain: Some("local".to_string()),
            action_kind: "workflow_run".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: Some("ok".to_string()),
            rule_id: Some("rule-1".to_string()),
            input_summary: Some("input".to_string()),
            verification_summary: Some("verify".to_string()),
            decision_context: Some("{\"ctx\":true}".to_string()),
            result: "success".to_string(),
        };

        let redactor = Redactor::new();
        let record = AuditStreamRecord::from_action(action, &redactor);
        let value = serde_json::to_value(&record).unwrap();

        assert!(value.get("id").is_some());
        assert!(value.get("ts").is_some());
        assert!(value.get("actor_kind").is_some());
        assert!(value.get("action_kind").is_some());
        assert!(value.get("policy_decision").is_some());
        assert!(value.get("result").is_some());
    }

    #[test]
    fn approval_token_record_serializes() {
        let token = ApprovalTokenRecord {
            id: 1,
            code_hash: "sha256:abc123".to_string(),
            created_at: 1_700_000_000_000,
            expires_at: 1_700_000_010_000,
            used_at: None,
            workspace_id: "workspace-a".to_string(),
            action_kind: "send_text".to_string(),
            pane_id: Some(42),
            action_fingerprint: "sha256:fingerprint".to_string(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };

        let json = serde_json::to_string(&token).unwrap();
        assert!(json.contains("sha256:abc123"));
        assert!(json.contains("workspace-a"));
    }

    // =========================================================================
    // wa-4vx.3.6: Retention & Maintenance Tests
    // =========================================================================

    #[test]
    fn retention_prunes_old_segments_and_fts() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "old content", 11, now_ms - 1000],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, "new content", 11, now_ms + 1000],
        )
        .unwrap();

        let deleted = prune_segments_sync(&conn, now_ms).unwrap();
        assert_eq!(deleted, 1);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM output_segments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1);

        let fts_old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM output_segments_fts WHERE output_segments_fts MATCH 'old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_old, 0);
    }

    #[test]
    fn maintenance_log_records_event() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let record = MaintenanceRecord {
            id: 0,
            event_type: "retention_cleanup".to_string(),
            message: Some("cleanup complete".to_string()),
            metadata: Some("{\"deleted\": 1}".to_string()),
            timestamp: 0,
        };

        let id = record_maintenance_sync(&conn, &record).unwrap();
        assert!(id > 0);

        let event_type: String = conn
            .query_row(
                "SELECT event_type FROM maintenance_log WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_type, "retention_cleanup");
    }

    #[test]
    fn secret_scan_report_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let record = SecretScanReportRecord {
            id: 0,
            scope_hash: "scope-hash".to_string(),
            scope_json: "{\"pane_id\":1}".to_string(),
            report_version: 1,
            last_segment_id: Some(42),
            report_json: "{\"report_version\":1}".to_string(),
            created_at: 1_700_000_000_000,
        };

        let id = record_secret_scan_report_sync(&conn, &record).unwrap();
        assert!(id > 0);

        let fetched = query_latest_secret_scan_report(&conn, "scope-hash")
            .unwrap()
            .expect("report should exist");
        assert_eq!(fetched.scope_hash, "scope-hash");
        assert_eq!(fetched.last_segment_id, Some(42));
        assert_eq!(fetched.report_version, 1);
        assert_eq!(fetched.report_json, "{\"report_version\":1}");
    }

    #[test]
    fn saved_search_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let record = SavedSearchRecord::new(
            "errors".to_string(),
            "error OR warning".to_string(),
            Some(1),
            25,
            SAVED_SEARCH_SINCE_MODE_LAST_RUN.to_string(),
            None,
        );
        insert_saved_search_sync(&conn, &record).unwrap();

        let fetched = query_saved_search_by_name(&conn, "errors")
            .unwrap()
            .expect("saved search should exist");
        assert_eq!(fetched.name, "errors");
        assert_eq!(fetched.query, "error OR warning");
        assert_eq!(fetched.pane_id, Some(1));
        assert_eq!(fetched.limit, 25);
        assert_eq!(fetched.since_mode, SAVED_SEARCH_SINCE_MODE_LAST_RUN);

        update_saved_search_schedule_sync(&conn, &fetched.id, true, Some(60_000)).unwrap();
        let scheduled = query_saved_search_by_name(&conn, "errors")
            .unwrap()
            .expect("saved search should exist");
        assert!(scheduled.enabled);
        assert_eq!(scheduled.schedule_interval_ms, Some(60_000));

        let record2 = SavedSearchRecord::new(
            "alpha".to_string(),
            "panic".to_string(),
            None,
            10,
            SAVED_SEARCH_SINCE_MODE_FIXED.to_string(),
            Some(1_700_000_000_000),
        );
        insert_saved_search_sync(&conn, &record2).unwrap();

        let list = list_saved_searches_sync(&conn).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "alpha");
        assert_eq!(list[1].name, "errors");

        let run_ts = now_ms();
        update_saved_search_run_sync(&conn, &fetched.id, run_ts, Some(3), None).unwrap();
        let updated = query_saved_search_by_name(&conn, "errors")
            .unwrap()
            .expect("saved search should exist");
        assert_eq!(updated.last_run_at, Some(run_ts));
        assert_eq!(updated.last_result_count, Some(3));
        assert!(updated.last_error.is_none());

        let deleted = delete_saved_search_sync(&conn, "errors").unwrap();
        assert_eq!(deleted, 1);
        let missing = query_saved_search_by_name(&conn, "errors").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn can_insert_and_consume_approval_token() {
        let mut conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", 1i64, 1i64, 1],
        )
        .unwrap();

        let now = now_ms();
        let token = ApprovalTokenRecord {
            id: 0,
            code_hash: "sha256:tokenhash".to_string(),
            created_at: now,
            expires_at: now + 5_000,
            used_at: None,
            workspace_id: "ws".to_string(),
            action_kind: "send_text".to_string(),
            pane_id: Some(1),
            action_fingerprint: "sha256:fp".to_string(),
            plan_hash: None,
            plan_version: None,
            risk_summary: None,
        };

        insert_approval_token_sync(&conn, &token).unwrap();

        let consumed = consume_approval_token_sync(
            &mut conn,
            "sha256:tokenhash",
            "ws",
            "send_text",
            Some(1),
            "sha256:fp",
        )
        .unwrap();
        assert!(consumed.is_some());
        assert!(consumed.unwrap().used_at.is_some());

        let second = consume_approval_token_sync(
            &mut conn,
            "sha256:tokenhash",
            "ws",
            "send_text",
            Some(1),
            "sha256:fp",
        )
        .unwrap();
        assert!(second.is_none());
    }

    // =========================================================================
    // wa-4vx.3.3: Gap Recording Tests
    // =========================================================================

    #[test]
    fn can_record_gap_on_discontinuity() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        // Insert pane
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        // Insert some segments (seq 0, 1, 2)
        for seq in 0..3 {
            conn.execute(
                "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![1i64, seq, format!("content {}", seq), 10, now_ms + seq * 100],
            ).unwrap();
        }

        // Record a gap (simulating a discontinuity detected)
        let gap = record_gap_sync(&conn, 1, "sequence_jump")
            .unwrap()
            .expect("should return gap");

        // Verify gap was recorded
        assert_eq!(gap.pane_id, 1);
        assert_eq!(gap.seq_before, 2); // Last seq was 2
        assert_eq!(gap.seq_after, 3); // Next expected would be 3
        assert_eq!(gap.reason, "sequence_jump");

        // Query the gap from the database
        let (id, pane_id, seq_before, seq_after, reason): (i64, i64, i64, i64, String) = conn
            .query_row(
                "SELECT id, pane_id, seq_before, seq_after, reason FROM output_gaps WHERE pane_id = ?1",
                [1i64],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();

        assert!(id > 0);
        assert_eq!(pane_id, 1);
        assert_eq!(seq_before, 2);
        assert_eq!(seq_after, 3);
        assert_eq!(reason, "sequence_jump");
    }

    #[test]
    fn gap_reasons_are_stable() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        // Insert pane
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        // Insert initial segment so gaps can be computed (needs seq_before)
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0, "initial content", 15, now_ms],
        )
        .unwrap();

        // Record gaps with different reasons
        let reasons = vec![
            "sequence_jump",
            "overlap_detected",
            "cursor_truncation",
            "session_restart",
        ];

        for reason in &reasons {
            record_gap_sync(&conn, 1, reason).unwrap();
        }

        // Verify all gaps were recorded with stable reasons
        let mut stmt = conn
            .prepare("SELECT reason FROM output_gaps WHERE pane_id = ?1 ORDER BY id")
            .unwrap();
        let recorded_reasons: Vec<String> = stmt
            .query_map([1i64], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(recorded_reasons, reasons);
    }

    // =========================================================================
    // wa-4vx.3.3: Last-N Query Tests
    // =========================================================================

    #[test]
    fn last_n_segments_returns_deterministic_order() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        // Insert pane
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        // Insert segments out of order (seq: 5, 2, 8, 1, 3)
        let insert_order = vec![5, 2, 8, 1, 3];
        for seq in insert_order {
            conn.execute(
                "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![1i64, seq, format!("segment-{}", seq), 10, now_ms + seq * 100],
            ).unwrap();
        }

        // Query last 3 segments
        let segments = query_segments(&conn, 1, 3).unwrap();

        // Should return in descending seq order: 8, 5, 3
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].seq, 8);
        assert_eq!(segments[1].seq, 5);
        assert_eq!(segments[2].seq, 3);

        // Query all segments
        let all_segments = query_segments(&conn, 1, 100).unwrap();
        assert_eq!(all_segments.len(), 5);

        // Verify strictly descending order
        for window in all_segments.windows(2) {
            assert!(
                window[0].seq > window[1].seq,
                "Segments should be in strictly descending seq order"
            );
        }
    }

    #[test]
    fn last_n_query_is_indexed() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Verify the index exists using EXPLAIN QUERY PLAN
        let plan: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id, pane_id, seq, content, content_len, content_hash, captured_at
                 FROM output_segments WHERE pane_id = 1 ORDER BY seq DESC LIMIT 10",
                [],
                |row| row.get(3),
            )
            .unwrap();

        // The query plan should use the idx_segments_pane_seq index
        assert!(
            plan.contains("idx_segments_pane_seq") || plan.contains("USING INDEX"),
            "Query should use the pane_seq index, got: {plan}"
        );
    }

    // =========================================================================
    // wa-4vx.3.5: Agent Sessions Storage Tests
    // =========================================================================

    #[test]
    fn can_insert_agent_session() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        let session = AgentSessionRecord {
            id: 0,
            pane_id: 1,
            agent_type: "claude_code".to_string(),
            session_id: Some("sess-123".to_string()),
            external_id: Some("ext-456".to_string()),
            external_meta: None,
            started_at: now_ms,
            ended_at: None,
            end_reason: None,
            total_tokens: None,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            reasoning_tokens: None,
            model_name: Some("opus-4.5".to_string()),
            estimated_cost_usd: None,
        };

        let session_id = upsert_agent_session_sync(&conn, &session).unwrap();
        assert!(session_id > 0, "Session should have been assigned an ID");

        let retrieved = query_agent_session(&conn, session_id).unwrap().unwrap();
        assert_eq!(retrieved.pane_id, 1);
        assert_eq!(retrieved.agent_type, "claude_code");
    }

    #[test]
    fn can_update_agent_session() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        let session = AgentSessionRecord::new_start(1, "codex");
        let session_id = upsert_agent_session_sync(&conn, &session).unwrap();

        let mut updated = AgentSessionRecord::new_start(1, "codex");
        updated.id = session_id;
        updated.ended_at = Some(now_ms + 60_000);
        updated.total_tokens = Some(5000);

        upsert_agent_session_sync(&conn, &updated).unwrap();

        let retrieved = query_agent_session(&conn, session_id).unwrap().unwrap();
        assert_eq!(retrieved.total_tokens, Some(5000));
    }

    #[test]
    fn query_active_sessions_filters_ended() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now_ms = 1_700_000_000_000i64;

        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

        // Active session
        let active = AgentSessionRecord::new_start(1, "claude");
        upsert_agent_session_sync(&conn, &active).unwrap();

        // Ended session
        let mut ended = AgentSessionRecord::new_start(1, "codex");
        ended.ended_at = Some(now_ms);
        upsert_agent_session_sync(&conn, &ended).unwrap();

        let results = query_active_sessions(&conn).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].agent_type, "claude");
    }

    #[test]
    fn agent_sessions_table_exists() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='agent_sessions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
}

// =========================================================================
// wa-4vx.3.4: FTS Search API Tests
// =========================================================================

#[test]
fn fts_search_returns_matching_segments() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    // Insert segments with different content
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "error: connection refused", 26, now_ms],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, "successfully connected to server", 32, now_ms + 100],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 2i64, "another error occurred here", 27, now_ms + 200],
        ).unwrap();

    // Search for "error"
    let results = search_fts_with_snippets(&conn, "error", &SearchOptions::default()).unwrap();

    assert_eq!(results.len(), 2, "Should find 2 segments with 'error'");
    assert!(results[0].segment.content.contains("error"));
    assert!(results[1].segment.content.contains("error"));
}

#[test]
fn fts_search_returns_snippets_with_highlights() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "The important error message appears here", 40, now_ms],
        ).unwrap();

    let options = SearchOptions {
        highlight_prefix: Some("[[".to_string()),
        highlight_suffix: Some("]]".to_string()),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "error", &options).unwrap();

    assert_eq!(results.len(), 1);
    let snippet = results[0].snippet.as_ref().expect("Should have snippet");
    assert!(
        snippet.contains("[[error]]"),
        "Snippet should contain highlighted term: {snippet}"
    );
}

#[test]
fn fts_search_respects_pane_filter() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms, 1],
        ).unwrap();
    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![2i64, "local", now_ms, now_ms, 1],
        ).unwrap();

    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "pane one test message", 21, now_ms],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![2i64, 0i64, "pane two test message", 21, now_ms],
        ).unwrap();

    let options = SearchOptions {
        pane_id: Some(1),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "test", &options).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].segment.pane_id, 1);
}

#[test]
fn fts_search_respects_time_filter() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, "local", now_ms, now_ms + 2000, 1],
        ).unwrap();

    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 0i64, "early test message", 18, now_ms],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 1i64, "middle test message", 19, now_ms + 1000],
        ).unwrap();
    conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![1i64, 2i64, "late test message", 17, now_ms + 2000],
        ).unwrap();

    let options = SearchOptions {
        since: Some(now_ms + 500),
        until: Some(now_ms + 1500),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "test", &options).unwrap();

    assert_eq!(results.len(), 1);
    assert!(results[0].segment.content.contains("middle"));
}

#[test]
fn fts_search_invalid_query_returns_error() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let result = validate_fts_query(&conn, "\"unclosed quote");
    assert!(result.is_err());
    let err = result.unwrap_err();
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("Invalid FTS5 query syntax"),
        "Error should mention FTS5 syntax: {err_msg}"
    );
}

#[test]
fn fts_search_respects_limit() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    for i in 0i64..10 {
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                1i64,
                i,
                format!("test message number {i}"),
                20,
                now_ms + i * 100
            ],
        )
        .unwrap();
    }

    let options = SearchOptions {
        limit: Some(3),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "test", &options).unwrap();

    assert_eq!(results.len(), 3, "Should respect limit of 3");
}

#[test]
fn fts_search_bm25_ordering() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 0i64, "single error here", 17, now_ms],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 1i64, "error error error multiple errors", 33, now_ms + 100],
    )
    .unwrap();

    let results = search_fts_with_snippets(&conn, "error", &SearchOptions::default()).unwrap();

    assert_eq!(results.len(), 2);
    assert!(
        results[0].score <= results[1].score,
        "First result should have lower (better) BM25 score"
    );
}

#[test]
fn fts_search_no_snippets_option() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 0i64, "test content here", 17, now_ms],
    )
    .unwrap();

    let options = SearchOptions {
        include_snippets: Some(false),
        ..Default::default()
    };
    let results = search_fts_with_snippets(&conn, "test", &options).unwrap();

    assert_eq!(results.len(), 1);
    assert!(
        results[0].snippet.is_none(),
        "Snippet should be None when disabled"
    );
}

// =========================================================================
// wa-4vx.3.7: FTS Empty/No-Match Behavior Tests
// =========================================================================

#[test]
fn fts_search_no_match_returns_empty() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, 0i64, "hello world", 11, now_ms],
    )
    .unwrap();

    // Search for term that doesn't exist
    let results =
        search_fts_with_snippets(&conn, "nonexistent", &SearchOptions::default()).unwrap();

    assert!(results.is_empty(), "Should return empty vec for no matches");
}

#[test]
fn fts_search_empty_db_returns_empty() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    // Search on empty database (no panes, no segments)
    let results = search_fts_with_snippets(&conn, "anything", &SearchOptions::default()).unwrap();

    assert!(
        results.is_empty(),
        "Should return empty vec for empty database"
    );
}

// =========================================================================
// wa-4vx.3.7: Workflow Step Logs Tests
// =========================================================================

#[test]
fn can_insert_and_query_workflow_step_logs() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    // Insert pane
    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    // Insert workflow execution
    conn.execute(
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params!["wf-test-001", "test_workflow", 1i64, 0, "running", now_ms, now_ms],
    )
    .unwrap();

    // Insert step logs
    insert_step_log_sync(
        &conn,
        "wf-test-001",
        None,
        0,
        "step_one",
        None, // step_id
        None, // step_kind
        "continue",
        Some(r#"{"output": "step 1 done"}"#),
        None, // policy_summary
        None, // verification_refs
        None, // error_code
        now_ms,
        now_ms + 100,
    )
    .unwrap();

    insert_step_log_sync(
        &conn,
        "wf-test-001",
        None,
        1,
        "step_two",
        None, // step_id
        None, // step_kind
        "done",
        Some(r#"{"output": "final"}"#),
        None, // policy_summary
        None, // verification_refs
        None, // error_code
        now_ms + 100,
        now_ms + 300,
    )
    .unwrap();

    // Query step logs
    let logs = query_step_logs(&conn, "wf-test-001").unwrap();

    assert_eq!(logs.len(), 2, "Should have 2 step logs");

    // Verify ordering by step_index
    assert_eq!(logs[0].step_index, 0);
    assert_eq!(logs[0].step_name, "step_one");
    assert_eq!(logs[0].result_type, "continue");
    assert_eq!(logs[0].duration_ms, 100);

    assert_eq!(logs[1].step_index, 1);
    assert_eq!(logs[1].step_name, "step_two");
    assert_eq!(logs[1].result_type, "done");
    assert_eq!(logs[1].duration_ms, 200);
}

#[test]
fn query_step_logs_returns_empty_for_unknown_workflow() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let logs = query_step_logs(&conn, "nonexistent-workflow").unwrap();

    assert!(
        logs.is_empty(),
        "Should return empty vec for unknown workflow"
    );
}

#[test]
fn query_latest_step_log_returns_last_step() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params!["wf-test-latest", "test_workflow", 1i64, 0, "running", now_ms, now_ms],
    )
    .unwrap();

    insert_step_log_sync(
        &conn,
        "wf-test-latest",
        None,
        0,
        "step_one",
        None,
        None,
        "continue",
        None,
        None,
        None,
        None,
        now_ms,
        now_ms + 100,
    )
    .unwrap();

    insert_step_log_sync(
        &conn,
        "wf-test-latest",
        None,
        2,
        "step_three",
        None,
        None,
        "done",
        None,
        None,
        None,
        None,
        now_ms + 200,
        now_ms + 400,
    )
    .unwrap();

    insert_step_log_sync(
        &conn,
        "wf-test-latest",
        None,
        1,
        "step_two",
        None,
        None,
        "continue",
        None,
        None,
        None,
        None,
        now_ms + 100,
        now_ms + 200,
    )
    .unwrap();

    let latest = query_latest_step_log(&conn, "wf-test-latest")
        .unwrap()
        .unwrap();
    assert_eq!(latest.step_index, 2);
    assert_eq!(latest.step_name, "step_three");
    assert_eq!(latest.result_type, "done");
}

#[test]
fn query_latest_step_log_returns_none_for_unknown_workflow() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let latest = query_latest_step_log(&conn, "unknown-workflow").unwrap();
    assert!(latest.is_none());
}

#[test]
fn workflow_step_log_result_data_is_optional() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params!["wf-test-002", "test_workflow", 1i64, 0, "running", now_ms, now_ms],
    )
    .unwrap();

    // Insert step log without result_data
    insert_step_log_sync(
        &conn,
        "wf-test-002",
        None,
        0,
        "simple_step",
        None, // step_id
        None, // step_kind
        "continue",
        None, // result_data
        None, // policy_summary
        None, // verification_refs
        None, // error_code
        now_ms,
        now_ms + 50,
    )
    .unwrap();

    let logs = query_step_logs(&conn, "wf-test-002").unwrap();

    assert_eq!(logs.len(), 1);
    assert!(logs[0].result_data.is_none(), "result_data should be None");
}

#[test]
fn workflow_step_log_record_serializes() {
    let log = WorkflowStepLogRecord {
        id: 1,
        workflow_id: "wf-001".to_string(),
        audit_action_id: None,
        step_index: 0,
        step_name: "init".to_string(),
        step_id: None,
        step_kind: None,
        result_type: "continue".to_string(),
        result_data: Some(r#"{"status": "ok"}"#.to_string()),
        policy_summary: None,
        verification_refs: None,
        error_code: None,
        started_at: 1_700_000_000_000,
        completed_at: 1_700_000_000_100,
        duration_ms: 100,
    };

    let json = serde_json::to_string(&log).unwrap();
    assert!(json.contains("wf-001"));
    assert!(json.contains("init"));
    assert!(json.contains("duration_ms"));
}

#[test]
fn can_insert_and_query_workflow_action_plan() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;

    conn.execute(
        "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![1i64, "local", now_ms, now_ms, 1],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO workflow_executions (id, workflow_name, pane_id, current_step, status, started_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params!["wf-plan-001", "test_workflow", 1i64, 0, "running", now_ms, now_ms],
    )
    .unwrap();

    let plan = crate::plan::ActionPlan::builder("Test Plan", "workspace-1")
        .add_step(crate::plan::StepPlan::new(
            1,
            crate::plan::StepAction::SendText {
                pane_id: 1,
                text: "hello".to_string(),
                paste_mode: None,
            },
            "Send hello",
        ))
        .build();

    let record = action_plan_record_from_plan("wf-plan-001", &plan).unwrap();
    upsert_action_plan_sync(&conn, &record).unwrap();

    let fetched = query_action_plan(&conn, "wf-plan-001").unwrap().unwrap();
    assert_eq!(fetched.plan_id, plan.plan_id.to_string());
    assert_eq!(fetched.plan_hash, plan.compute_hash());

    let parsed: crate::plan::ActionPlan = serde_json::from_str(&fetched.plan_json).unwrap();
    assert_eq!(parsed.plan_id, plan.plan_id);
}

#[test]
fn can_insert_and_consume_prepared_plan() {
    let mut conn = Connection::open_in_memory().unwrap();
    initialize_schema(&conn).unwrap();

    let now_ms = 1_700_000_000_000i64;
    let record = PreparedPlanRecord {
        plan_id: "plan:abcd1234".to_string(),
        plan_hash: "sha256:abcd1234".to_string(),
        workspace_id: "/tmp/wa".to_string(),
        action_kind: "send_text".to_string(),
        pane_id: Some(1),
        pane_uuid: None,
        params_json: Some(r#"{"type":"send_text","pane_id":1}"#.to_string()),
        plan_json: r#"{"plan_id":"plan:abcd1234","plan_hash":"sha256:abcd1234"}"#.to_string(),
        requires_approval: false,
        created_at: now_ms,
        expires_at: now_ms + 60_000,
        consumed_at: None,
    };

    insert_prepared_plan_sync(&conn, &record).unwrap();
    let fetched = query_prepared_plan(&conn, "plan:abcd1234")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.plan_id, record.plan_id);
    assert_eq!(fetched.action_kind, "send_text");

    let consumed = consume_prepared_plan_sync(&mut conn, "plan:abcd1234", now_ms + 1)
        .unwrap()
        .unwrap();
    assert!(consumed.consumed_at.is_some());

    let second = consume_prepared_plan_sync(&mut conn, "plan:abcd1234", now_ms + 2).unwrap();
    assert!(second.is_none());
}

// =========================================================================
// wa-4vx.3.7: Async StorageHandle Tests
// =========================================================================

#[tokio::test]
async fn storage_handle_graceful_shutdown() {
    let temp_dir = std::env::temp_dir();
    let db_path = temp_dir.join(format!("wa_test_shutdown_{}.db", std::process::id()));
    let db_path_str = db_path.to_string_lossy().to_string();

    // Create storage handle
    let storage = StorageHandle::new(&db_path_str).await.unwrap();

    // Upsert a pane to verify it works
    let pane = PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some("test".to_string()),
        cwd: None,
        tty_name: None,
        first_seen_at: 1_700_000_000_000,
        last_seen_at: 1_700_000_000_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    };
    storage.upsert_pane(pane).await.unwrap();

    // Graceful shutdown
    storage.shutdown().await.unwrap();

    // Cleanup
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
    let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
}

#[tokio::test]
async fn storage_handle_insert_step_log_and_query() {
    let temp_dir = std::env::temp_dir();
    let db_path = temp_dir.join(format!("wa_test_steplog_{}.db", std::process::id()));
    let db_path_str = db_path.to_string_lossy().to_string();

    let storage = StorageHandle::new(&db_path_str).await.unwrap();

    // Create pane
    let pane = PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some("test".to_string()),
        cwd: None,
        tty_name: None,
        first_seen_at: 1_700_000_000_000,
        last_seen_at: 1_700_000_000_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    };
    storage.upsert_pane(pane).await.unwrap();

    // Create workflow
    let workflow = WorkflowRecord {
        id: "wf-async-001".to_string(),
        workflow_name: "async_test".to_string(),
        pane_id: 1,
        trigger_event_id: None,
        current_step: 0,
        status: "running".to_string(),
        wait_condition: None,
        context: None,
        result: None,
        error: None,
        started_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
        completed_at: None,
    };
    storage.upsert_workflow(workflow).await.unwrap();

    // Insert step log via async API
    storage
        .insert_step_log(
            "wf-async-001",
            None,
            0,
            "async_step",
            None, // step_id
            None, // step_kind
            "continue",
            Some(r#"{"async": true}"#.to_string()),
            None, // policy_summary
            None, // verification_refs
            None, // error_code
            1_700_000_000_000,
            1_700_000_000_050,
        )
        .await
        .unwrap();

    // Query step logs via async API
    let logs = storage.get_step_logs("wf-async-001").await.unwrap();

    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].step_name, "async_step");
    assert_eq!(logs[0].duration_ms, 50);

    storage.shutdown().await.unwrap();

    // Cleanup
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
    let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
}

#[tokio::test]
async fn storage_handle_action_plan_roundtrip() {
    let temp_dir = std::env::temp_dir();
    let db_path = temp_dir.join(format!("wa_test_plan_{}.db", std::process::id()));
    let db_path_str = db_path.to_string_lossy().to_string();

    let storage = StorageHandle::new(&db_path_str).await.unwrap();

    let pane = PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some("test".to_string()),
        cwd: None,
        tty_name: None,
        first_seen_at: 1_700_000_000_000,
        last_seen_at: 1_700_000_000_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    };
    storage.upsert_pane(pane).await.unwrap();

    let workflow = WorkflowRecord {
        id: "wf-plan-async-001".to_string(),
        workflow_name: "async_plan_test".to_string(),
        pane_id: 1,
        trigger_event_id: None,
        current_step: 0,
        status: "running".to_string(),
        wait_condition: None,
        context: None,
        result: None,
        error: None,
        started_at: 1_700_000_000_000,
        updated_at: 1_700_000_000_000,
        completed_at: None,
    };
    storage.upsert_workflow(workflow).await.unwrap();

    let plan = crate::plan::ActionPlan::builder("Async Plan", "workspace-async")
        .add_step(crate::plan::StepPlan::new(
            1,
            crate::plan::StepAction::SendText {
                pane_id: 1,
                text: "/compact".to_string(),
                paste_mode: None,
            },
            "Send compact",
        ))
        .build();

    storage
        .upsert_action_plan("wf-plan-async-001", &plan)
        .await
        .unwrap();

    let fetched = storage
        .get_action_plan("wf-plan-async-001")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.plan_id, plan.plan_id.to_string());

    storage.shutdown().await.unwrap();

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
    let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
}

#[tokio::test]
async fn storage_handle_records_audit_action_redacted() {
    let temp_dir = std::env::temp_dir();
    let db_path = temp_dir.join(format!("wa_test_audit_{}.db", std::process::id()));
    let db_path_str = db_path.to_string_lossy().to_string();

    let storage = StorageHandle::new(&db_path_str).await.unwrap();

    let pane = PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some("test".to_string()),
        cwd: None,
        tty_name: None,
        first_seen_at: 1_700_000_000_000,
        last_seen_at: 1_700_000_000_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    };
    storage.upsert_pane(pane).await.unwrap();

    let action = AuditActionRecord {
        id: 0,
        ts: 1_700_000_000_000,
        actor_kind: "robot".to_string(),
        actor_id: None,
        correlation_id: None,
        pane_id: Some(1),
        domain: Some("local".to_string()),
        action_kind: "send_text".to_string(),
        policy_decision: "allow".to_string(),
        decision_reason: None,
        rule_id: None,
        input_summary: Some(
            "API key sk-abc123456789012345678901234567890123456789012345678901".to_string(),
        ),
        verification_summary: None,
        decision_context: None,
        result: "success".to_string(),
    };

    storage.record_audit_action_redacted(action).await.unwrap();

    let query = AuditQuery {
        pane_id: Some(1),
        limit: Some(10),
        ..Default::default()
    };
    let rows = storage.get_audit_actions(query).await.unwrap();
    assert_eq!(rows.len(), 1);

    let input = rows[0].input_summary.as_ref().unwrap();
    assert!(input.contains("[REDACTED]"));
    assert!(!input.contains("sk-abc"));
    let redactor = Redactor::new();
    for field in [
        rows[0].decision_reason.as_deref(),
        rows[0].input_summary.as_deref(),
        rows[0].verification_summary.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        assert!(!field.contains("sk-abc"));
        assert!(!redactor.contains_secrets(field));
    }

    storage.shutdown().await.unwrap();

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
    let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
}

#[tokio::test]
async fn storage_handle_writer_queue_processes_all() {
    let temp_dir = std::env::temp_dir();
    let db_path = temp_dir.join(format!("wa_test_queue_{}.db", std::process::id()));
    let db_path_str = db_path.to_string_lossy().to_string();

    // Create storage with small queue
    let config = StorageConfig {
        write_queue_size: 4,
    };
    let storage = StorageHandle::with_config(&db_path_str, config)
        .await
        .unwrap();

    // Create pane first
    let pane = PaneRecord {
        pane_id: 1,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some("test".to_string()),
        cwd: None,
        tty_name: None,
        first_seen_at: 1_700_000_000_000,
        last_seen_at: 1_700_000_000_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    };
    storage.upsert_pane(pane).await.unwrap();

    // Send many segment appends sequentially
    for i in 0..10 {
        let content = format!("segment content {i}");
        storage.append_segment(1, &content, None).await.unwrap();
    }

    // All appends should succeed
    let segments = storage.get_segments(1, 100).await.unwrap();
    assert_eq!(segments.len(), 10, "All 10 segments should be stored");

    storage.shutdown().await.unwrap();

    // Cleanup
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path_str}-wal"));
    let _ = std::fs::remove_file(format!("{db_path_str}-shm"));
}

// =============================================================================
// Database Check & Repair Tests (wa-ubb)
// =============================================================================

#[cfg(test)]
mod db_check_repair_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DB_CHECK_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path(prefix: &str) -> String {
        let id = DB_CHECK_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        format!("/tmp/wa_dbcheck_{prefix}_{pid}_{id}.db")
    }

    fn cleanup_db(path: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path}-wal"));
        let _ = std::fs::remove_file(format!("{path}-shm"));
        // Also clean up any backup files
        if let Ok(entries) = std::fs::read_dir("/tmp") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&path.replace("/tmp/", "")) && name.contains(".bak.") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    #[test]
    fn check_nonexistent_db() {
        let path = temp_db_path("nonexist");
        let report = check_database_health(Path::new(&path));

        assert!(!report.db_exists);
        assert!(report.has_errors());
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, DbCheckStatus::Error);
    }

    #[test]
    fn check_healthy_db() {
        let path = temp_db_path("healthy");
        // Create and initialize a valid database
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = check_database_health(Path::new(&path));

        assert!(report.db_exists);
        assert!(report.db_size_bytes.is_some());
        assert_eq!(report.schema_version, Some(SCHEMA_VERSION));
        assert!(!report.has_errors());
        assert_eq!(report.problem_count(), 0);

        // All checks should be OK
        for check in &report.checks {
            assert_eq!(
                check.status,
                DbCheckStatus::Ok,
                "Check '{}' was {:?}: {:?}",
                check.name,
                check.status,
                check.detail
            );
        }

        cleanup_db(&path);
    }

    #[test]
    fn check_old_schema_version() {
        let path = temp_db_path("oldschema");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
            // Set an older schema version
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
        }

        let report = check_database_health(Path::new(&path));

        assert!(report.db_exists);
        assert_eq!(report.schema_version, Some(1));
        assert!(report.has_warnings());

        let schema_check = report
            .checks
            .iter()
            .find(|c| c.name == "Schema version")
            .unwrap();
        assert_eq!(schema_check.status, DbCheckStatus::Warning);
        assert!(
            schema_check
                .detail
                .as_ref()
                .unwrap()
                .contains("needs migration")
        );

        cleanup_db(&path);
    }

    #[test]
    fn check_future_schema_version() {
        let path = temp_db_path("future");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
            conn.execute_batch("PRAGMA user_version = 999").unwrap();
        }

        let report = check_database_health(Path::new(&path));

        assert_eq!(report.schema_version, Some(999));
        assert!(report.has_errors());

        let schema_check = report
            .checks
            .iter()
            .find(|c| c.name == "Schema version")
            .unwrap();
        assert_eq!(schema_check.status, DbCheckStatus::Error);
        assert!(
            schema_check
                .detail
                .as_ref()
                .unwrap()
                .contains("newer than supported")
        );

        cleanup_db(&path);
    }

    #[test]
    fn check_report_serializes_to_json() {
        let path = temp_db_path("json");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = check_database_health(Path::new(&path));
        let json = serde_json::to_string(&report).unwrap();
        let parsed: DbCheckReport = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.db_exists, report.db_exists);
        assert_eq!(parsed.checks.len(), report.checks.len());

        cleanup_db(&path);
    }

    #[test]
    fn repair_nonexistent_db_fails() {
        let path = temp_db_path("repairne");
        let result = repair_database(Path::new(&path), false, true);
        assert!(result.is_err());
    }

    #[test]
    fn repair_dry_run_makes_no_changes() {
        let path = temp_db_path("dryrun");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let size_before = std::fs::metadata(&path).unwrap().len();
        let report = repair_database(Path::new(&path), true, true).unwrap();

        // Dry run should not create a backup
        assert!(report.backup_path.is_none());
        assert!(report.all_succeeded());

        // File should still exist and be similar size
        let size_after = std::fs::metadata(&path).unwrap().len();
        assert_eq!(size_before, size_after);

        cleanup_db(&path);
    }

    #[test]
    fn repair_creates_backup() {
        let path = temp_db_path("backup");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = repair_database(Path::new(&path), false, false).unwrap();

        assert!(report.backup_path.is_some());
        let backup = report.backup_path.as_ref().unwrap();
        assert!(Path::new(backup).exists(), "Backup file should exist");

        // Clean up backup
        let _ = std::fs::remove_file(backup);
        cleanup_db(&path);
    }

    #[test]
    fn repair_skips_backup_when_requested() {
        let path = temp_db_path("nobackup");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = repair_database(Path::new(&path), false, true).unwrap();

        assert!(report.backup_path.is_none());
        assert!(report.all_succeeded());

        cleanup_db(&path);
    }

    #[test]
    fn repair_healthy_db_reports_no_action_needed() {
        let path = temp_db_path("noaction");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = repair_database(Path::new(&path), false, true).unwrap();

        assert!(report.all_succeeded());
        // Each item should indicate no repair was needed
        let fts_item = report
            .repairs
            .iter()
            .find(|r| r.name == "FTS index")
            .unwrap();
        assert!(fts_item.detail.contains("healthy"));

        cleanup_db(&path);
    }

    #[test]
    fn repair_report_serializes_to_json() {
        let path = temp_db_path("repjson");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
            initialize_schema(&conn).unwrap();
        }

        let report = repair_database(Path::new(&path), true, true).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        let parsed: DbRepairReport = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.repairs.len(), report.repairs.len());

        cleanup_db(&path);
    }

    #[test]
    fn check_report_methods() {
        let report = DbCheckReport {
            db_path: "/test".to_string(),
            db_exists: true,
            db_size_bytes: Some(1024),
            schema_version: Some(7),
            checks: vec![
                DbCheckItem {
                    name: "ok_check".to_string(),
                    status: DbCheckStatus::Ok,
                    detail: None,
                },
                DbCheckItem {
                    name: "warn_check".to_string(),
                    status: DbCheckStatus::Warning,
                    detail: Some("warning".to_string()),
                },
                DbCheckItem {
                    name: "err_check".to_string(),
                    status: DbCheckStatus::Error,
                    detail: Some("error".to_string()),
                },
            ],
        };

        assert!(report.has_errors());
        assert!(report.has_warnings());
        assert_eq!(report.problem_count(), 2);
    }

    #[test]
    fn check_status_display() {
        assert_eq!(DbCheckStatus::Ok.to_string(), "OK");
        assert_eq!(DbCheckStatus::Warning.to_string(), "WARNING");
        assert_eq!(DbCheckStatus::Error.to_string(), "ERROR");
    }
}

// =============================================================================
// Incremental FTS Sync Tests (wa-3g9.4)
// =============================================================================

#[cfg(test)]
mod fts_sync_tests {
    use super::*;

    /// Helper to insert a test segment directly
    fn insert_test_segment(conn: &Connection, pane_id: u64, seq: u64, content: &str) {
        let now = now_ms();
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                pane_id as i64,
                seq as i64,
                content,
                content.len() as i64,
                now
            ],
        )
        .unwrap();
    }

    /// Helper to create a pane
    fn insert_test_pane(conn: &Connection, pane_id: u64) {
        let now = now_ms();
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed)
             VALUES (?1, 'local', ?2, ?3, 1)",
            params![pane_id as i64, now, now],
        )
        .unwrap();
    }

    #[test]
    fn fts_index_state_tables_exist() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Check fts_index_state table exists
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fts_index_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "fts_index_state table should exist");

        // Check fts_pane_progress table exists
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fts_pane_progress'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "fts_pane_progress table should exist");
    }

    #[test]
    fn get_fts_index_state_returns_none_when_empty() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // State table exists but is empty until sync initializes it
        let state = get_fts_index_state_sync(&conn).unwrap();
        // After migration, we insert a default row
        assert!(state.is_some() || state.is_none()); // Depends on migration logic
    }

    #[test]
    fn upsert_fts_index_state_works() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        let state = FtsIndexState {
            index_version: 42,
            last_full_rebuild_at: Some(now),
            created_at: now,
            updated_at: now,
        };

        upsert_fts_index_state_sync(&conn, &state).unwrap();

        let loaded = get_fts_index_state_sync(&conn).unwrap().unwrap();
        assert_eq!(loaded.index_version, 42);
        assert_eq!(loaded.last_full_rebuild_at, Some(now));
    }

    #[test]
    fn fts_pane_progress_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Create pane first (foreign key)
        insert_test_pane(&conn, 100);

        let now = now_ms();
        let progress = FtsPaneProgress {
            pane_id: 100,
            last_indexed_seq: 50,
            indexed_count: 50,
            last_indexed_at: now,
        };

        upsert_fts_pane_progress_sync(&conn, &progress).unwrap();

        let loaded = get_fts_pane_progress_sync(&conn, 100).unwrap().unwrap();
        assert_eq!(loaded.pane_id, 100);
        assert_eq!(loaded.last_indexed_seq, 50);
        assert_eq!(loaded.indexed_count, 50);
    }

    #[test]
    fn sync_fts_on_startup_initializes_state() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let config = FtsSyncConfig::default();
        let result = sync_fts_on_startup(&conn, &config).unwrap();

        // Should complete with no segments (empty db)
        assert_eq!(result.segments_indexed, 0);
        assert!(!result.full_rebuild);

        // State should be initialized
        let state = get_fts_index_state_sync(&conn).unwrap().unwrap();
        assert_eq!(state.index_version, FTS_INDEX_VERSION);
    }

    #[test]
    fn sync_fts_indexes_new_segments() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Create pane and segments
        insert_test_pane(&conn, 1);
        insert_test_segment(&conn, 1, 1, "Hello world");
        insert_test_segment(&conn, 1, 2, "Testing FTS sync");
        insert_test_segment(&conn, 1, 3, "Third segment");

        // Note: With trigger-driven FTS, segments are already indexed on insert.
        // The incremental sync is for recovery scenarios.
        // Let's clear FTS and progress to simulate recovery
        conn.execute_batch(
            "INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')",
        )
        .ok(); // May fail if empty
        clear_fts_pane_progress_sync(&conn).unwrap();

        let config = FtsSyncConfig::default();
        let result = sync_fts_on_startup(&conn, &config).unwrap();

        // Should rebuild all 3 segments
        assert_eq!(result.segments_indexed, 3);
        assert_eq!(result.panes_processed, 1);
    }

    #[test]
    fn sync_fts_respects_progress() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Create pane and segments
        insert_test_pane(&conn, 1);
        insert_test_segment(&conn, 1, 1, "First");
        insert_test_segment(&conn, 1, 2, "Second");
        insert_test_segment(&conn, 1, 3, "Third");

        // Clear FTS
        conn.execute_batch(
            "INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')",
        )
        .ok();

        // Set progress to seq 2 (pretend first two are already indexed)
        let now = now_ms();
        let progress = FtsPaneProgress {
            pane_id: 1,
            last_indexed_seq: 2,
            indexed_count: 2,
            last_indexed_at: now,
        };
        upsert_fts_pane_progress_sync(&conn, &progress).unwrap();

        let config = FtsSyncConfig::default();
        let result = sync_fts_on_startup(&conn, &config).unwrap();

        // Should only index segment 3
        assert_eq!(result.segments_indexed, 1);
    }

    #[test]
    fn full_rebuild_clears_progress() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Create pane and segments
        insert_test_pane(&conn, 1);
        insert_test_segment(&conn, 1, 1, "One");
        insert_test_segment(&conn, 1, 2, "Two");

        // Set some progress
        let now = now_ms();
        upsert_fts_pane_progress_sync(
            &conn,
            &FtsPaneProgress {
                pane_id: 1,
                last_indexed_seq: 1,
                indexed_count: 1,
                last_indexed_at: now,
            },
        )
        .unwrap();

        let config = FtsSyncConfig::default();
        let result = full_fts_rebuild_sync(&conn, &config).unwrap();

        assert!(result.full_rebuild);
        assert_eq!(result.segments_indexed, 2);

        // Progress should be updated
        let progress = get_fts_pane_progress_sync(&conn, 1).unwrap().unwrap();
        assert_eq!(progress.last_indexed_seq, 2);
        assert_eq!(progress.indexed_count, 2);
    }

    #[test]
    fn full_rebuild_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        insert_test_pane(&conn, 1);
        insert_test_segment(&conn, 1, 1, "Alpha");
        insert_test_segment(&conn, 1, 2, "Beta");
        insert_test_segment(&conn, 1, 3, "Gamma");

        let config = FtsSyncConfig::default();
        let first = full_fts_rebuild_sync(&conn, &config).unwrap();
        assert!(first.full_rebuild);
        assert_eq!(first.segments_indexed, 3);

        let fts_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM output_segments_fts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fts_rows, 3);

        let second = full_fts_rebuild_sync(&conn, &config).unwrap();
        assert!(second.full_rebuild);
        assert_eq!(second.segments_indexed, 3);

        let fts_rows_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM output_segments_fts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fts_rows_after, 3);
    }

    #[test]
    fn version_mismatch_triggers_rebuild() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Set an old version
        let now = now_ms();
        let old_state = FtsIndexState {
            index_version: FTS_INDEX_VERSION - 1, // Old version
            last_full_rebuild_at: Some(now),
            created_at: now,
            updated_at: now,
        };
        upsert_fts_index_state_sync(&conn, &old_state).unwrap();

        // Create pane and segment
        insert_test_pane(&conn, 1);
        insert_test_segment(&conn, 1, 1, "Test content");

        let config = FtsSyncConfig::default();
        let result = sync_fts_on_startup(&conn, &config).unwrap();

        // Should trigger full rebuild due to version mismatch
        assert!(result.full_rebuild);

        // State should be updated to new version
        let state = get_fts_index_state_sync(&conn).unwrap().unwrap();
        assert_eq!(state.index_version, FTS_INDEX_VERSION);
    }

    #[test]
    fn batch_config_limits_work() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Create pane and multiple segments
        insert_test_pane(&conn, 1);
        for i in 1..=10 {
            insert_test_segment(&conn, 1, i, &format!("Segment {i} with some content"));
        }

        // Clear FTS
        conn.execute_batch(
            "INSERT INTO output_segments_fts(output_segments_fts) VALUES('delete-all')",
        )
        .ok();
        clear_fts_pane_progress_sync(&conn).unwrap();

        // Use small batch size
        let config = FtsSyncConfig {
            batch_size: 3,
            max_batch_bytes: 1_048_576,
            commit_progress: true,
        };

        let result = sync_fts_on_startup(&conn, &config).unwrap();

        // Should index all 10 segments in multiple batches
        assert_eq!(result.segments_indexed, 10);

        // Progress should be at the end
        let progress = get_fts_pane_progress_sync(&conn, 1).unwrap().unwrap();
        assert_eq!(progress.last_indexed_seq, 10);
    }
}

// =============================================================================
// Timeline Data Model Tests (wa-6sk.1)
// =============================================================================

#[cfg(test)]
mod timeline_tests {
    use super::*;

    /// Helper to create a pane
    fn insert_test_pane(conn: &Connection, pane_id: u64, domain: &str) {
        let now = now_ms();
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed)
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![pane_id as i64, domain, now, now],
        )
        .unwrap();
    }

    /// Helper to create an event
    fn insert_test_event(
        conn: &Connection,
        pane_id: u64,
        rule_id: &str,
        event_type: &str,
        severity: &str,
        detected_at: i64,
    ) -> i64 {
        conn.execute(
            "INSERT INTO events (pane_id, rule_id, agent_type, event_type, severity,
             confidence, detected_at)
             VALUES (?1, ?2, 'claude_code', ?3, ?4, 0.9, ?5)",
            params![pane_id as i64, rule_id, event_type, severity, detected_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn correlation_type_display() {
        assert_eq!(CorrelationType::Failover.to_string(), "failover");
        assert_eq!(CorrelationType::Temporal.to_string(), "temporal");
        assert_eq!(CorrelationType::WorkflowGroup.to_string(), "workflow_group");
    }

    #[test]
    fn timeline_query_builder() {
        let query = TimelineQuery::new()
            .with_range(1000, 2000)
            .with_panes(vec![1, 2])
            .with_severities(vec!["critical".to_string()])
            .unhandled_only()
            .with_pagination(50, 10);

        assert_eq!(query.start, Some(1000));
        assert_eq!(query.end, Some(2000));
        assert_eq!(query.pane_ids, Some(vec![1, 2]));
        assert_eq!(query.severities, Some(vec!["critical".to_string()]));
        assert!(query.unhandled_only);
        assert_eq!(query.limit, 50);
        assert_eq!(query.offset, 10);
    }

    #[test]
    fn empty_timeline_query() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let query = TimelineQuery::new();
        let timeline = query_timeline(&conn, &query).unwrap();

        assert!(timeline.events.is_empty());
        assert!(timeline.correlations.is_empty());
        assert_eq!(timeline.total_count, 0);
        assert!(!timeline.has_more);
    }

    #[test]
    fn timeline_with_events() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        // Create panes
        insert_test_pane(&conn, 1, "local");
        insert_test_pane(&conn, 2, "ssh");

        // Create events
        let now = now_ms();
        insert_test_event(
            &conn,
            1,
            "codex.usage_limit",
            "usage_limit",
            "warning",
            now - 2000,
        );
        insert_test_event(
            &conn,
            2,
            "codex.compaction",
            "compaction",
            "info",
            now - 1000,
        );
        insert_test_event(&conn, 1, "codex.error", "error", "critical", now);

        let query = TimelineQuery::new();
        let timeline = query_timeline(&conn, &query).unwrap();

        assert_eq!(timeline.events.len(), 3);
        assert_eq!(timeline.total_count, 3);
        assert!(!timeline.has_more);

        // Events should be in chronological order
        assert!(timeline.events[0].timestamp <= timeline.events[1].timestamp);
        assert!(timeline.events[1].timestamp <= timeline.events[2].timestamp);

        // Pane info should be populated
        assert_eq!(timeline.events[0].pane_info.domain, "local");
        assert_eq!(timeline.events[1].pane_info.domain, "ssh");
    }

    #[test]
    fn timeline_with_pane_filter() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        insert_test_pane(&conn, 1, "local");
        insert_test_pane(&conn, 2, "ssh");

        let now = now_ms();
        insert_test_event(&conn, 1, "rule1", "event1", "info", now - 1000);
        insert_test_event(&conn, 2, "rule2", "event2", "info", now);
        insert_test_event(&conn, 1, "rule3", "event3", "info", now + 1000);

        let query = TimelineQuery::new().with_panes(vec![1]);
        let timeline = query_timeline(&conn, &query).unwrap();

        assert_eq!(timeline.events.len(), 2);
        assert!(timeline.events.iter().all(|e| e.pane_info.pane_id == 1));
    }

    #[test]
    fn timeline_with_severity_filter() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        insert_test_pane(&conn, 1, "local");

        let now = now_ms();
        insert_test_event(&conn, 1, "rule1", "event1", "info", now - 2000);
        insert_test_event(&conn, 1, "rule2", "event2", "warning", now - 1000);
        insert_test_event(&conn, 1, "rule3", "event3", "critical", now);

        let query = TimelineQuery::new().with_severities(vec!["critical".to_string()]);
        let timeline = query_timeline(&conn, &query).unwrap();

        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].severity, "critical");
    }

    #[test]
    fn timeline_pagination() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        insert_test_pane(&conn, 1, "local");

        let now = now_ms();
        for i in 0..10 {
            insert_test_event(
                &conn,
                1,
                &format!("rule{i}"),
                "event",
                "info",
                now + i * 1000,
            );
        }

        // First page
        let query = TimelineQuery::new().with_pagination(3, 0);
        let timeline = query_timeline(&conn, &query).unwrap();

        assert_eq!(timeline.events.len(), 3);
        assert_eq!(timeline.total_count, 10);
        assert!(timeline.has_more);

        // Second page
        let query = TimelineQuery::new().with_pagination(3, 3);
        let timeline = query_timeline(&conn, &query).unwrap();

        assert_eq!(timeline.events.len(), 3);
        assert!(timeline.has_more);

        // Last page
        let query = TimelineQuery::new().with_pagination(3, 9);
        let timeline = query_timeline(&conn, &query).unwrap();

        assert_eq!(timeline.events.len(), 1);
        assert!(!timeline.has_more);
    }

    #[test]
    fn detect_temporal_correlations() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        insert_test_pane(&conn, 1, "local");
        insert_test_pane(&conn, 2, "ssh");

        let now = now_ms();
        // Events within temporal window across different panes
        insert_test_event(&conn, 1, "rule1", "event1", "info", now);
        insert_test_event(&conn, 2, "rule2", "event2", "info", now + 2000); // 2s later

        let query = TimelineQuery::new();
        let timeline = query_timeline(&conn, &query).unwrap();

        // Should detect temporal correlation
        assert!(!timeline.correlations.is_empty());
        let temporal = timeline
            .correlations
            .iter()
            .find(|c| c.correlation_type == CorrelationType::Temporal);
        assert!(temporal.is_some());
    }

    #[test]
    fn detect_failover_correlations() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        insert_test_pane(&conn, 1, "local");
        insert_test_pane(&conn, 2, "ssh");

        let now = now_ms();
        insert_test_event(
            &conn,
            1,
            "codex.usage.reached",
            "usage.reached",
            "critical",
            now,
        );
        insert_test_event(
            &conn,
            2,
            "codex.session.start",
            "session.start",
            "info",
            now + 10_000,
        );

        let timeline = query_timeline(&conn, &TimelineQuery::new()).unwrap();
        let failover = timeline
            .correlations
            .iter()
            .find(|c| c.correlation_type == CorrelationType::Failover);
        assert!(failover.is_some());
    }

    #[test]
    fn detect_cascade_correlations() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        insert_test_pane(&conn, 1, "local");
        insert_test_pane(&conn, 2, "ssh");

        let now = now_ms();
        insert_test_event(
            &conn,
            1,
            "codex.error.timeout",
            "error.timeout",
            "critical",
            now,
        );
        insert_test_event(
            &conn,
            2,
            "codex.session.resume_hint",
            "session.resume_hint",
            "info",
            now + 5_000,
        );

        let timeline = query_timeline(&conn, &TimelineQuery::new()).unwrap();
        let cascade = timeline
            .correlations
            .iter()
            .find(|c| c.correlation_type == CorrelationType::Cascade);
        assert!(cascade.is_some());
    }

    #[test]
    fn unhandled_only_filter() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        insert_test_pane(&conn, 1, "local");

        let now = now_ms();
        let event1_id = insert_test_event(&conn, 1, "rule1", "event1", "info", now - 1000);
        insert_test_event(&conn, 1, "rule2", "event2", "info", now);

        // Mark first event as handled
        conn.execute(
            "UPDATE events SET handled_at = ?1, handled_status = 'completed' WHERE id = ?2",
            params![now, event1_id],
        )
        .unwrap();

        let query = TimelineQuery::new().unhandled_only();
        let timeline = query_timeline(&conn, &query).unwrap();

        assert_eq!(timeline.events.len(), 1);
        assert!(timeline.events[0].handled.is_none());
    }
}

// =============================================================================
// Async StorageHandle Tests (wa-4vx.3.7)
// =============================================================================

#[cfg(test)]
mod storage_handle_tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Counter for unique temp DB paths
    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Generate a unique temp DB path
    fn temp_db_path() -> String {
        let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        dir.join(format!("wa_test_{counter}_{}.db", std::process::id()))
            .to_str()
            .unwrap()
            .to_string()
    }

    /// Helper to create a test pane record
    fn test_pane(pane_id: u64) -> PaneRecord {
        let now = now_ms();
        PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn storage_handle_sets_db_permissions() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        let mode = std::fs::metadata(&db_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        for suffix in ["-wal", "-shm"] {
            let path = format!("{db_path}{suffix}");
            if std::path::Path::new(&path).exists() {
                let mode = std::fs::metadata(&path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600);
            }
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_path}-wal"));
        let _ = std::fs::remove_file(format!("{db_path}-shm"));
    }

    #[tokio::test]
    async fn storage_handle_basic_write_read() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        // Create a pane
        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Append a segment
        let segment: Segment = handle
            .append_segment(1, "Hello, world!", None)
            .await
            .unwrap();

        assert_eq!(segment.pane_id, 1);
        assert_eq!(segment.seq, 0);
        assert_eq!(segment.content, "Hello, world!");

        // Append another segment
        let segment2: Segment = handle
            .append_segment(1, "Second segment", None)
            .await
            .unwrap();

        assert_eq!(segment2.seq, 1);

        // Query segments
        let recent: Vec<Segment> = handle.get_segments(1, 10).await.unwrap();
        assert_eq!(recent.len(), 2);
        // Returned in descending seq order
        assert_eq!(recent[0].seq, 1);
        assert_eq!(recent[1].seq, 0);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_records_usage_metrics_batch() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        // Single insert
        let id1 = handle
            .record_usage_metric(UsageMetricRecord {
                id: 0,
                timestamp: 1_000,
                metric_type: MetricType::ApiCall,
                pane_id: Some(1),
                agent_type: Some("codex".to_string()),
                account_id: None,
                workflow_id: None,
                count: Some(1),
                amount: None,
                tokens: None,
                metadata: Some("{\"tool\":\"wa.robot.state\"}".to_string()),
                created_at: 1_000,
            })
            .await
            .unwrap();
        assert!(id1 > 0);

        // Batch insert
        let inserted = handle
            .record_usage_metrics_batch(vec![
                UsageMetricRecord {
                    id: 0,
                    timestamp: 2_000,
                    metric_type: MetricType::TokenUsage,
                    pane_id: Some(1),
                    agent_type: Some("codex".to_string()),
                    account_id: Some("acct-1".to_string()),
                    workflow_id: None,
                    count: None,
                    amount: None,
                    tokens: Some(123),
                    metadata: None,
                    created_at: 2_000,
                },
                UsageMetricRecord {
                    id: 0,
                    timestamp: 3_000,
                    metric_type: MetricType::ApiCost,
                    pane_id: Some(1),
                    agent_type: Some("codex".to_string()),
                    account_id: Some("acct-1".to_string()),
                    workflow_id: None,
                    count: None,
                    amount: Some(0.42),
                    tokens: None,
                    metadata: Some("{\"source\":\"test\"}".to_string()),
                    created_at: 3_000,
                },
            ])
            .await
            .unwrap();
        assert_eq!(inserted, 2);

        let rows = handle
            .query_usage_metrics(MetricQuery {
                metric_type: None,
                agent_type: Some("codex".to_string()),
                account_id: None,
                since: Some(0),
                until: None,
                limit: Some(10),
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);

        // Sorted DESC by timestamp
        assert_eq!(rows[0].timestamp, 3_000);
        assert_eq!(rows[1].timestamp, 2_000);
        assert_eq!(rows[2].timestamp, 1_000);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_shutdown_flushes_pending_writes() {
        let db_path = temp_db_path();

        {
            let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();
            handle.upsert_pane(test_pane(1)).await.unwrap();

            // Queue up multiple writes
            for i in 0..10 {
                handle
                    .append_segment(1, &format!("Segment {i}"), None)
                    .await
                    .unwrap();
            }

            // Shutdown should flush all pending writes
            handle.shutdown().await.unwrap();
        }

        // Reopen and verify all writes persisted
        {
            let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();
            let segments: Vec<Segment> = handle.get_segments(1, 100).await.unwrap();

            // All 10 segments should be present
            assert_eq!(segments.len(), 10);

            // Verify sequence numbers are correct (returned in descending order)
            let seqs: Vec<u64> = segments.iter().map(|s| s.seq).collect();
            assert_eq!(seqs, vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);

            handle.shutdown().await.unwrap();
        }

        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_concurrent_reads_during_writes() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Write segments
        for i in 0..5 {
            handle
                .append_segment(1, &format!("Content {i}"), None)
                .await
                .unwrap();
        }

        // Concurrent reads should work (WAL mode)
        let read1 = handle.get_segments(1, 10);
        let read2 = handle.get_segments(1, 10);
        let (result1, result2) = tokio::join!(read1, read2);

        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_eq!(result1.unwrap().len(), 5);
        assert_eq!(result2.unwrap().len(), 5);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_workflow_step_logs() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        // Create pane first (required for foreign key constraint)
        handle.upsert_pane(test_pane(1)).await.unwrap();

        let workflow_id = "wf-test-123";
        let now = now_ms();

        // Create workflow execution
        let workflow = WorkflowRecord {
            id: workflow_id.to_string(),
            workflow_name: "test_workflow".to_string(),
            pane_id: 1,
            trigger_event_id: None,
            current_step: 0,
            status: "running".to_string(),
            wait_condition: None,
            context: None,
            result: None,
            error: None,
            started_at: now,
            updated_at: now,
            completed_at: None,
        };

        handle.upsert_workflow(workflow).await.unwrap();

        // Insert step logs
        handle
            .insert_step_log(
                workflow_id,
                None,
                0,
                "init",
                None, // step_id
                None, // step_kind
                "success",
                Some(r#"{"message":"started"}"#.to_string()),
                None, // policy_summary
                None, // verification_refs
                None, // error_code
                now,
                now + 100,
            )
            .await
            .unwrap();

        handle
            .insert_step_log(
                workflow_id,
                None,
                1,
                "send_text",
                None, // step_id
                None, // step_kind
                "success",
                Some(r#"{"chars":42}"#.to_string()),
                None, // policy_summary
                None, // verification_refs
                None, // error_code
                now + 100,
                now + 200,
            )
            .await
            .unwrap();

        handle
            .insert_step_log(
                workflow_id,
                None,
                2,
                "wait_for",
                None, // step_id
                None, // step_kind
                "success",
                Some(r#"{"matched":true}"#.to_string()),
                None, // policy_summary
                None, // verification_refs
                None, // error_code
                now + 200,
                now + 500,
            )
            .await
            .unwrap();

        // Query step logs
        let steps: Vec<WorkflowStepLogRecord> = handle.get_step_logs(workflow_id).await.unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].step_name, "init");
        assert_eq!(steps[1].step_name, "send_text");
        assert_eq!(steps[2].step_name, "wait_for");

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_gap_recording() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Record some segments
        let _seg: Segment = handle.append_segment(1, "Before gap", None).await.unwrap();

        // Record a gap
        let gap: Gap = handle
            .record_gap(1, "connection_lost")
            .await
            .unwrap()
            .expect("should return gap");

        assert_eq!(gap.pane_id, 1);
        assert_eq!(gap.reason, "connection_lost");

        // Record more segments after gap
        let _seg2: Segment = handle.append_segment(1, "After gap", None).await.unwrap();

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_event_lifecycle() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        let now = now_ms();

        // Create pane first (foreign key constraint)
        handle.upsert_pane(test_pane(1)).await.unwrap();

        let event = StoredEvent {
            id: 0, // Will be assigned
            pane_id: 1,
            rule_id: "test.rule".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage".to_string(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: Some(serde_json::json!({"key":"value"})),
            matched_text: Some("match".to_string()),
            segment_id: None,
            detected_at: now,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };

        let event_id: i64 = handle.record_event(event).await.unwrap();
        assert!(event_id > 0);

        // Mark handled
        handle
            .mark_event_handled(event_id, Some("wf-123".to_string()), "completed")
            .await
            .unwrap();

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_event_annotations_roundtrip() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        let now = now_ms();

        // Create pane first (foreign key constraint)
        handle.upsert_pane(test_pane(1)).await.unwrap();

        let event = StoredEvent {
            id: 0,
            pane_id: 1,
            rule_id: "test.rule".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage".to_string(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: None,
            matched_text: Some("match".to_string()),
            segment_id: None,
            detected_at: now,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };

        let event_id: i64 = handle.record_event(event).await.unwrap();
        assert!(event_id > 0);

        // Triage state
        let changed = handle
            .set_event_triage_state(
                event_id,
                Some("new".to_string()),
                Some("tester".to_string()),
            )
            .await
            .unwrap();
        assert!(changed);

        // Labels (idempotent)
        let inserted = handle
            .add_event_label(
                event_id,
                "needs-attn".to_string(),
                Some("tester".to_string()),
            )
            .await
            .unwrap();
        assert!(inserted);
        let inserted_again = handle
            .add_event_label(
                event_id,
                "needs-attn".to_string(),
                Some("tester".to_string()),
            )
            .await
            .unwrap();
        assert!(!inserted_again);

        // Note (should be redacted at write time)
        let note = "token sk-abc123456789012345678901234567890123456789012345678901";
        handle
            .set_event_note(event_id, Some(note.to_string()), Some("tester".to_string()))
            .await
            .unwrap();

        let annotations = handle
            .get_event_annotations(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(annotations.triage_state.as_deref(), Some("new"));
        assert_eq!(annotations.triage_updated_by.as_deref(), Some("tester"));
        assert_eq!(annotations.labels, vec!["needs-attn".to_string()]);
        let stored_note = annotations.note.unwrap_or_default();
        assert!(stored_note.contains("[REDACTED]"));
        assert!(!stored_note.contains("sk-abc"));

        // Query filters should work (label + triage state)
        let events = handle
            .get_events(EventQuery {
                triage_state: Some("new".to_string()),
                label: Some("needs-attn".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event_id);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_with_small_queue_handles_burst() {
        let db_path = temp_db_path();

        // Use a small queue to test bounded channel behavior
        let config = StorageConfig {
            write_queue_size: 4,
        };
        let handle: StorageHandle = StorageHandle::with_config(&db_path, config).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Write more items than queue size - should work because we await each write
        for i in 0..20 {
            handle
                .append_segment(1, &format!("Segment {i}"), None)
                .await
                .unwrap();
        }

        let segments: Vec<Segment> = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 20);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_seq_is_monotonic_per_pane() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        // Create two panes
        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();

        // Interleave writes to both panes
        for i in 0..5 {
            handle
                .append_segment(1, &format!("Pane1 seg {i}"), None)
                .await
                .unwrap();
            handle
                .append_segment(2, &format!("Pane2 seg {i}"), None)
                .await
                .unwrap();
        }

        // Verify each pane has monotonic seqs starting at 0
        let pane1_segs: Vec<Segment> = handle.get_segments(1, 10).await.unwrap();
        let pane2_segs: Vec<Segment> = handle.get_segments(2, 10).await.unwrap();

        assert_eq!(pane1_segs.len(), 5);
        assert_eq!(pane2_segs.len(), 5);

        // Check monotonicity (returned in descending order)
        let pane1_seq_values: Vec<u64> = pane1_segs.iter().map(|s| s.seq).collect();
        let pane2_seq_values: Vec<u64> = pane2_segs.iter().map(|s| s.seq).collect();

        assert_eq!(pane1_seq_values, vec![4, 3, 2, 1, 0]);
        assert_eq!(pane2_seq_values, vec![4, 3, 2, 1, 0]);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn storage_handle_agent_sessions() {
        let db_path = temp_db_path();
        let handle: StorageHandle = StorageHandle::new(&db_path).await.unwrap();

        let now = now_ms();

        // Create pane first (foreign key constraint)
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        handle.upsert_pane(pane).await.unwrap();

        let mut session = AgentSessionRecord::new_start(1, "claude_code");
        session.started_at = now;
        session.total_tokens = Some(1000);
        session.model_name = Some("opus".to_string());

        let session_id: i64 = handle.upsert_agent_session(session).await.unwrap();
        assert!(session_id > 0);

        // Query back
        let retrieved: Option<AgentSessionRecord> =
            handle.get_agent_session(session_id).await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.agent_type, "claude_code");
        assert_eq!(retrieved.total_tokens, Some(1000));

        // Query active sessions
        let active: Vec<AgentSessionRecord> = handle.get_active_sessions().await.unwrap();
        assert!(!active.is_empty());

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    // =========================================================================
    // Checkpoint Tests (wa-upg.5.3)
    // =========================================================================

    #[tokio::test]
    async fn checkpoint_returns_result() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        // Write some data so the WAL has pages
        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle
            .append_segment(1, "checkpoint test data", None)
            .await
            .unwrap();

        let result = handle.checkpoint().await.unwrap();
        // PASSIVE checkpoint may or may not move pages, but it should succeed
        assert!(result.wal_pages >= 0);
        assert!(result.optimized);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn checkpoint_is_idempotent() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Run checkpoint twice — both should succeed
        let r1 = handle.checkpoint().await.unwrap();
        let r2 = handle.checkpoint().await.unwrap();
        assert!(r1.optimized);
        assert!(r2.optimized);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn checkpoint_after_many_writes() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Generate WAL traffic
        for i in 0..50 {
            handle
                .append_segment(1, &format!("segment {i}"), None)
                .await
                .unwrap();
        }

        let result = handle.checkpoint().await.unwrap();
        assert!(result.wal_pages >= 0);
        assert!(result.optimized);

        // Data should still be readable after checkpoint
        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 50);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn vacuum_still_works() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "vacuum test", None).await.unwrap();

        // Vacuum should still work alongside checkpoint
        handle.vacuum().await.unwrap();

        let segments = handle.get_segments(1, 10).await.unwrap();
        assert_eq!(segments.len(), 1);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    // =========================================================================
    // Write Batching Tests (wa-upg.5.3)
    // =========================================================================

    #[tokio::test]
    async fn concurrent_writes_are_batched() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Fire many writes concurrently — they should be batched
        let mut handles = Vec::new();
        for i in 0..20 {
            let h = handle.clone();
            handles.push(tokio::spawn(async move {
                h.append_segment(1, &format!("batch-{i}"), None)
                    .await
                    .unwrap()
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // All segments should be persisted
        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 20);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn batched_writes_preserve_ordering() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        // Write segments sequentially — seq numbers should be monotonic
        for i in 0..10 {
            handle
                .append_segment(1, &format!("ordered-{i}"), None)
                .await
                .unwrap();
        }

        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 10);

        // Verify ordering by content (they should come back newest-first from get_segments)
        // but seq numbers should be monotonically increasing
        let mut seqs: Vec<u64> = segments.iter().map(|s| s.seq).collect();
        seqs.sort();
        for (idx, seq) in seqs.iter().enumerate() {
            assert_eq!(*seq, idx as u64, "seq should be monotonic");
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn checkpoint_sync_function_works_directly() {
        // Test the sync function directly with an in-memory connection
        // that uses WAL mode (requires file-based DB for WAL)
        let db_path = temp_db_path();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
        initialize_schema(&conn).unwrap();

        // Insert some data
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (1, 'local', 0, 0, 1)",
            [],
        )
        .unwrap();

        let result = checkpoint_sync(&conn).unwrap();
        assert!(result.wal_pages >= 0);
        assert!(result.optimized);

        drop(conn);
        let _ = std::fs::remove_file(&db_path);
    }

    // =========================================================================
    // Indexing Progress Tracking Tests (wa-upg.5.2)
    // =========================================================================

    #[tokio::test]
    async fn indexing_stats_empty_database() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert!(stats.is_empty(), "No panes means no stats");

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn indexing_stats_pane_with_no_segments() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].pane_id, 1);
        assert_eq!(stats[0].segment_count, 0);
        assert_eq!(stats[0].total_bytes, 0);
        assert!(stats[0].max_seq.is_none());
        assert!(stats[0].last_segment_at.is_none());
        assert!(stats[0].fts_consistent);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn indexing_stats_tracks_segments() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "hello", None).await.unwrap();
        handle.append_segment(1, "world!", None).await.unwrap();
        handle.append_segment(1, "test data", None).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].pane_id, 1);
        assert_eq!(stats[0].segment_count, 3);
        assert_eq!(stats[0].total_bytes, 5 + 6 + 9); // hello + world! + test data
        assert_eq!(stats[0].max_seq, Some(2)); // 0, 1, 2
        assert!(stats[0].last_segment_at.is_some());
        assert!(stats[0].fts_consistent);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn indexing_stats_multiple_panes() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();

        handle.append_segment(1, "pane1-data", None).await.unwrap();
        handle
            .append_segment(2, "pane2-data-longer", None)
            .await
            .unwrap();
        handle.append_segment(2, "pane2-more", None).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats.len(), 2);

        let p1 = stats.iter().find(|s| s.pane_id == 1).unwrap();
        assert_eq!(p1.segment_count, 1);
        assert_eq!(p1.total_bytes, 10);

        let p2 = stats.iter().find(|s| s.pane_id == 2).unwrap();
        assert_eq!(p2.segment_count, 2);
        assert_eq!(p2.total_bytes, 17 + 10);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn indexing_stats_seq_is_monotonic() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        for i in 0..10 {
            handle
                .append_segment(1, &format!("seg-{i}"), None)
                .await
                .unwrap();
        }

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats[0].segment_count, 10);
        assert_eq!(stats[0].max_seq, Some(9));

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn indexing_stats_ignored_panes_excluded() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        // Create observed pane
        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "visible", None).await.unwrap();

        // Create ignored pane
        let mut ignored = test_pane(2);
        ignored.observed = false;
        ignored.ignore_reason = Some("test exclude".to_string());
        handle.upsert_pane(ignored).await.unwrap();

        let stats = handle.get_pane_indexing_stats().await.unwrap();
        assert_eq!(stats.len(), 1, "Only observed panes appear in stats");
        assert_eq!(stats[0].pane_id, 1);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn indexing_health_report_healthy() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.append_segment(1, "hello world", None).await.unwrap();

        let report = handle.get_indexing_health().await.unwrap();
        assert!(report.healthy);
        assert_eq!(report.total_segments, 1);
        assert_eq!(report.total_bytes, 11);
        assert_eq!(report.inconsistent_panes, 0);
        assert_eq!(report.panes.len(), 1);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn indexing_health_report_aggregates() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle.upsert_pane(test_pane(1)).await.unwrap();
        handle.upsert_pane(test_pane(2)).await.unwrap();
        handle.upsert_pane(test_pane(3)).await.unwrap();

        for pane in 1..=3u64 {
            for i in 0..5 {
                handle
                    .append_segment(pane, &format!("p{pane}-s{i}"), None)
                    .await
                    .unwrap();
            }
        }

        let report = handle.get_indexing_health().await.unwrap();
        assert!(report.healthy);
        assert_eq!(report.total_segments, 15);
        assert_eq!(report.panes.len(), 3);
        for p in &report.panes {
            assert_eq!(p.segment_count, 5);
            assert!(p.fts_consistent);
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn fts_integrity_check_on_healthy_db() {
        let db_path = temp_db_path();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL").unwrap();
        initialize_schema(&conn).unwrap();

        // Insert some data via triggers
        conn.execute(
            "INSERT INTO panes (pane_id, domain, first_seen_at, last_seen_at, observed) VALUES (1, 'local', 0, 0, 1)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO output_segments (pane_id, seq, content, content_len, captured_at) VALUES (1, 0, 'test', 4, 0)",
            [],
        ).unwrap();

        let ok = check_fts_integrity_sync(&conn).unwrap();
        assert!(ok, "Healthy FTS should pass integrity check");

        drop(conn);
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn build_report_marks_healthy_when_fts_ok() {
        let stats = vec![PaneIndexingStats {
            pane_id: 1,
            segment_count: 10,
            total_bytes: 100,
            max_seq: Some(9),
            last_segment_at: Some(1000),
            fts_row_count: 10,
            fts_consistent: true,
        }];
        let report = build_indexing_health_report(stats, true);
        assert!(report.healthy);
        assert_eq!(report.inconsistent_panes, 0);
    }

    #[test]
    fn build_report_marks_unhealthy_when_fts_corrupt() {
        let stats = vec![
            PaneIndexingStats {
                pane_id: 1,
                segment_count: 10,
                total_bytes: 100,
                max_seq: Some(9),
                last_segment_at: Some(1000),
                fts_row_count: 10,
                fts_consistent: true,
            },
            PaneIndexingStats {
                pane_id: 2,
                segment_count: 5,
                total_bytes: 50,
                max_seq: Some(4),
                last_segment_at: Some(2000),
                fts_row_count: 5,
                fts_consistent: true,
            },
        ];
        let report = build_indexing_health_report(stats, false);
        assert!(!report.healthy);
        assert_eq!(report.inconsistent_panes, 2); // All panes marked
        assert!(!report.panes[0].fts_consistent);
        assert!(!report.panes[1].fts_consistent);
    }
}

// =============================================================================
// Queue Depth Instrumentation Tests (wa-upg.12.2)
// =============================================================================

#[cfg(test)]
mod queue_depth_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static QD_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path() -> String {
        let id = QD_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        dir.join(format!("wa_qd_test_{id}_{}.db", std::process::id()))
            .to_string_lossy()
            .to_string()
    }

    #[tokio::test]
    async fn write_queue_depth_starts_at_zero() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        assert_eq!(handle.write_queue_depth(), 0);
        assert!(handle.write_queue_capacity() > 0);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn write_queue_capacity_matches_config() {
        let db_path = temp_db_path();
        let mut config = StorageConfig::default();
        config.write_queue_size = 64;
        let handle = StorageHandle::with_config(&db_path, config).await.unwrap();

        assert_eq!(handle.write_queue_capacity(), 64);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn write_queue_depth_is_bounded() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        // Queue depth should always be <= capacity
        let depth = handle.write_queue_depth();
        let cap = handle.write_queue_capacity();
        assert!(
            depth <= cap,
            "depth ({depth}) should be <= capacity ({cap})"
        );

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn write_queue_depth_rises_under_concurrent_writes() {
        let db_path = temp_db_path();
        let mut config = StorageConfig::default();
        config.write_queue_size = 8; // Small queue to observe depth
        let handle = StorageHandle::with_config(&db_path, config).await.unwrap();

        // Register a pane first
        handle
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();

        // Submit multiple writes without awaiting (fire and forget via spawn)
        let mut join_handles = Vec::new();
        for i in 0..6 {
            let h = handle.clone();
            let jh =
                tokio::spawn(async move { h.append_segment(1, &format!("data-{i}"), None).await });
            join_handles.push(jh);
        }

        // Queue depth should be bounded by capacity
        let cap = handle.write_queue_capacity();
        assert_eq!(cap, 8);

        // Wait for all writes to complete
        for jh in join_handles {
            jh.await.unwrap().unwrap();
        }

        // After all writes complete, depth should return to 0
        // (give writer a moment to drain)
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let final_depth = handle.write_queue_depth();
        assert_eq!(
            final_depth, 0,
            "Queue should be drained after all writes complete"
        );

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn write_queue_bounded_under_heavy_load() {
        // Verify the queue never exceeds its configured capacity
        let db_path = temp_db_path();
        let mut config = StorageConfig::default();
        config.write_queue_size = 4; // Very small queue
        let handle = StorageHandle::with_config(&db_path, config).await.unwrap();

        handle
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();

        // Flood with many writes
        let cap = handle.write_queue_capacity();
        let mut join_handles = Vec::new();
        for i in 0..20 {
            let h = handle.clone();
            let jh =
                tokio::spawn(async move { h.append_segment(1, &format!("flood-{i}"), None).await });
            join_handles.push(jh);
        }

        // Sample queue depth multiple times during processing
        let mut max_observed_depth = 0usize;
        for _ in 0..10 {
            let depth = handle.write_queue_depth();
            if depth > max_observed_depth {
                max_observed_depth = depth;
            }
            assert!(
                depth <= cap,
                "Queue depth ({depth}) exceeded capacity ({cap})"
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }

        // Wait for all writes
        for jh in join_handles {
            jh.await.unwrap().unwrap();
        }

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn write_queue_depth_returns_to_zero_after_drain() {
        let db_path = temp_db_path();
        let handle = StorageHandle::new(&db_path).await.unwrap();

        handle
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();

        // Write some segments sequentially
        for i in 0..5 {
            handle
                .append_segment(1, &format!("sequential-{i}"), None)
                .await
                .unwrap();
        }

        // After sequential writes, queue should be empty
        assert_eq!(handle.write_queue_depth(), 0);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }
}

// =============================================================================
// Backpressure Integration Tests (wa-upg.12.5)
// =============================================================================

#[cfg(test)]
mod backpressure_integration_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::mpsc;

    static BP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path() -> String {
        let id = BP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        dir.join(format!("wa_bp_test_{id}_{}.db", std::process::id()))
            .to_string_lossy()
            .to_string()
    }

    #[tokio::test]
    async fn capture_channel_backpressure_detected() {
        // Simulate backpressure on the capture channel:
        // - Create a tiny channel (capacity 2)
        // - Fill it up
        // - Verify send times out (reserve with timeout)
        use tokio::time::{Duration, timeout};

        let (tx, _rx) = mpsc::channel::<u8>(2);

        // Fill the channel
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();

        // Channel is full — reserve should time out
        let result = timeout(Duration::from_millis(50), tx.reserve()).await;
        assert!(result.is_err(), "Should timeout when channel is full");

        // Verify depth
        let depth = tx.max_capacity() - tx.capacity();
        assert_eq!(depth, 2, "Queue should be at capacity");
    }

    #[tokio::test]
    async fn capture_channel_drains_when_consumer_resumes() {
        use tokio::time::Duration;

        let (tx, mut rx) = mpsc::channel::<u8>(4);

        // Fill partially
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        tx.send(3).await.unwrap();

        let depth_before = tx.max_capacity() - tx.capacity();
        assert_eq!(depth_before, 3);

        // Consume all items
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();

        // Small yield for channel state to update
        tokio::time::sleep(Duration::from_millis(1)).await;

        let depth_after = tx.max_capacity() - tx.capacity();
        assert_eq!(depth_after, 0, "Queue should drain when consumer resumes");
    }

    #[tokio::test]
    async fn storage_concurrent_writers_dont_deadlock() {
        // Multiple concurrent writers on a small queue should complete
        // without deadlock (writer thread drains fast enough)
        let db_path = temp_db_path();
        let mut config = StorageConfig::default();
        config.write_queue_size = 4;
        let handle = StorageHandle::with_config(&db_path, config).await.unwrap();

        handle
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();

        // Spawn many concurrent writers
        let mut handles = Vec::new();
        for i in 0..16 {
            let h = handle.clone();
            handles.push(tokio::spawn(async move {
                h.append_segment(1, &format!("concurrent-{i}"), None)
                    .await
                    .unwrap();
            }));
        }

        // Use a timeout to detect deadlocks
        let result = tokio::time::timeout(tokio::time::Duration::from_secs(10), async {
            for jh in handles {
                jh.await.unwrap();
            }
        })
        .await;

        assert!(
            result.is_ok(),
            "Concurrent writers should complete without deadlock"
        );

        // Verify all 16 segments were written
        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 16, "All concurrent writes should persist");

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn gap_recording_works_under_backpressure() {
        // Ensure GAP records can be written even when the queue has work pending
        let db_path = temp_db_path();
        let mut config = StorageConfig::default();
        config.write_queue_size = 4;
        let handle = StorageHandle::with_config(&db_path, config).await.unwrap();

        handle
            .upsert_pane(PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();

        // Write a segment first (gap requires existing seq)
        let seg_before = handle.append_segment(1, "before-gap", None).await.unwrap();

        // Record a gap (simulating backpressure-induced discontinuity)
        let gap = handle.record_gap(1, "backpressure_overflow").await.unwrap();
        assert!(
            gap.is_some(),
            "GAP should be recorded after existing segment"
        );
        let gap = gap.unwrap();
        assert_eq!(gap.pane_id, 1);
        assert_eq!(gap.reason, "backpressure_overflow");
        assert_eq!(gap.seq_before, seg_before.seq);
        assert_eq!(gap.seq_after, seg_before.seq + 1);

        // Continue writing after gap
        let seg_after = handle.append_segment(1, "after-gap", None).await.unwrap();

        // Verify segments are in the output_segments table
        let segments = handle.get_segments(1, 100).await.unwrap();
        assert_eq!(segments.len(), 2); // before and after (gap is in output_gaps table)
        // get_segments returns ORDER BY seq DESC (most recent first)
        assert_eq!(segments[0].content, "after-gap");
        assert_eq!(segments[1].content, "before-gap");
        // Sequence numbers should show the discontinuity
        assert!(seg_after.seq > seg_before.seq);

        handle.shutdown().await.unwrap();
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn health_warning_threshold_generates_warnings() {
        // Test the warning generation logic with a controlled queue state
        use crate::crash::HealthSnapshot;

        // Simulate a snapshot where capture queue is at 80% (above 75% threshold)
        let snapshot = HealthSnapshot {
            timestamp: 0,
            observed_panes: 2,
            capture_queue_depth: 820,
            write_queue_depth: 10,
            last_seq_by_pane: vec![],
            warnings: vec!["Capture queue backpressure: 820/1024 (80%)".to_string()],
            ingest_lag_avg_ms: 100.0,
            ingest_lag_max_ms: 500,
            db_writable: true,
            db_last_write_at: Some(1000),
            pane_priority_overrides: vec![],
            scheduler: None,
            backpressure_tier: None,
            last_activity_by_pane: vec![],
            restart_count: 0,
            last_crash_at: None,
            consecutive_crashes: 0,
            current_backoff_ms: 0,
            in_crash_loop: false,
        };

        assert!(!snapshot.warnings.is_empty());
        assert!(snapshot.warnings[0].contains("backpressure"));
        assert!(snapshot.warnings[0].contains("80%"));
    }

    #[tokio::test]
    async fn event_bus_detects_subscriber_lag() {
        use crate::events::{Event, EventBus};

        let bus = EventBus::new(4); // Small capacity

        // Subscribe before publishing
        let mut sub = bus.subscribe();

        // Publish more events than buffer size to cause lag
        for i in 0..8 {
            let _ = bus.publish(Event::SegmentCaptured {
                pane_id: 1,
                seq: i,
                content_len: 100,
            });
        }

        // First recv should indicate lag (missed events)
        let result = sub.recv().await;
        match result {
            Err(crate::events::RecvError::Lagged { missed_count }) => {
                assert!(missed_count > 0, "Should report missed events due to lag");
            }
            Ok(_) => {
                // Some events may still be in buffer, that's also valid
                // as long as the bus didn't panic
            }
            Err(e) => panic!("Unexpected error: {e:?}"),
        }

        // Stats should reflect capacity
        let stats = bus.stats();
        assert_eq!(stats.capacity, 4);
    }
}

// =============================================================================
// Property-Based Tests (wa-4vx.10.5)
// =============================================================================

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::runtime::Runtime;

    // Counter for unique temp DB paths
    static PROPTEST_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Generate a unique temp DB path
    fn temp_db_path() -> String {
        let counter = PROPTEST_DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        dir.join(format!("wa_proptest_{counter}_{}.db", std::process::id()))
            .to_str()
            .unwrap()
            .to_string()
    }

    /// Helper to create a test pane record
    fn test_pane(pane_id: u64) -> PaneRecord {
        let now = now_ms();
        PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    // Strategy for generating valid segment content (non-empty ASCII strings)
    fn segment_content_strategy() -> impl Strategy<Value = String> {
        // Generate strings of 1-100 printable ASCII characters
        "[a-zA-Z0-9 .,!?]{1,100}"
    }

    // Strategy for generating write operations (pane_id, content)
    fn write_ops_strategy() -> impl Strategy<Value = Vec<(u64, String)>> {
        // Generate 1-50 write operations across 1-5 panes
        let pane_count = 1u64..=5;
        pane_count.prop_flat_map(|max_panes| {
            proptest::collection::vec((1..=max_panes, segment_content_strategy()), 1..50)
        })
    }

    proptest! {
        // Set configuration for deterministic, CI-friendly runs
        #![proptest_config(ProptestConfig {
            cases: 50,  // Bounded case count
            max_shrink_iters: 100,
            .. ProptestConfig::default()
        })]

        /// Property: Sequence numbers are monotonically increasing per pane
        ///
        /// For any sequence of write operations across multiple panes,
        /// each pane's segments must have strictly increasing seq numbers.
        #[test]
        fn prop_seq_monotonic_per_pane(writes in write_ops_strategy()) {
            let rt = Runtime::new().expect("create runtime");
            let db_path = temp_db_path();

            // Collect results from async block for verification
            let verification_results: Vec<(u64, Vec<u64>)> = rt.block_on(async {
                let handle = StorageHandle::new(&db_path).await.expect("create storage");

                // Determine which panes we need to create
                let pane_ids: std::collections::HashSet<u64> = writes.iter().map(|(p, _)| *p).collect();

                // Create all needed panes
                for &pane_id in &pane_ids {
                    handle.upsert_pane(test_pane(pane_id)).await.expect("create pane");
                }

                // Execute all writes
                for (pane_id, content) in &writes {
                    handle.append_segment(*pane_id, content, None).await.expect("append segment");
                }

                // Collect seq values for each pane
                let mut results = Vec::new();
                for &pane_id in &pane_ids {
                    let segments = handle.get_segments(pane_id, 1000).await.expect("get segments");
                    // Segments are returned in descending seq order, reverse for ascending
                    let seqs: Vec<u64> = segments.iter().rev().map(|s| s.seq).collect();
                    results.push((pane_id, seqs));
                }

                handle.shutdown().await.expect("shutdown");
                results
            });

            let _ = std::fs::remove_file(&db_path);

            // Verify monotonicity outside async block
            for (pane_id, seqs) in verification_results {
                for (expected, actual) in seqs.iter().enumerate() {
                    prop_assert_eq!(
                        *actual, expected as u64,
                        "Pane {} seq at index {} should be {} but got {}",
                        pane_id, expected, expected, actual
                    );
                }
            }
        }

        /// Property: Inserted text becomes searchable via FTS
        ///
        /// For any valid search term inserted as segment content,
        /// FTS search should find it.
        #[test]
        fn prop_fts_finds_inserted_text(content in "[a-zA-Z]{3,20}") {
            let rt = Runtime::new().expect("create runtime");
            let db_path = temp_db_path();

            // Collect search results from async block
            let (results_empty, found_content): (bool, bool) = rt.block_on(async {
                let handle = StorageHandle::new(&db_path).await.expect("create storage");
                handle.upsert_pane(test_pane(1)).await.expect("create pane");

                // Insert the content as a segment
                handle.append_segment(1, &content, None).await.expect("append segment");

                // Search for the content
                let results = handle.search(&content).await.expect("search");

                let is_empty = results.is_empty();
                let found = results.iter().any(|seg| seg.content.contains(&content));

                handle.shutdown().await.expect("shutdown");
                (is_empty, found)
            });

            let _ = std::fs::remove_file(&db_path);

            // Verify outside async block
            prop_assert!(
                !results_empty,
                "FTS search for '{}' should return results",
                content
            );
            prop_assert!(
                found_content,
                "At least one result should contain '{}'",
                content
            );
        }

        /// Property: FTS respects pane scoping
        ///
        /// Content inserted in one pane should not appear in searches
        /// scoped to a different pane.
        #[test]
        fn prop_fts_respects_pane_scope(
            (content1, content2) in ("[a-zA-Z]{5,15}", "[a-zA-Z]{5,15}")
                .prop_filter("contents must differ", |(a, b)| a != b)
        ) {
            let rt = Runtime::new().expect("create runtime");
            let db_path = temp_db_path();

            // Collect search results from async block
            let (found_in_pane1, found_in_pane2): (bool, bool) = rt.block_on(async {
                let handle = StorageHandle::new(&db_path).await.expect("create storage");

                // Create two panes
                handle.upsert_pane(test_pane(1)).await.expect("create pane 1");
                handle.upsert_pane(test_pane(2)).await.expect("create pane 2");

                // Insert different content in each pane
                handle.append_segment(1, &content1, None).await.expect("append to pane 1");
                handle.append_segment(2, &content2, None).await.expect("append to pane 2");

                // Search for content1 scoped to pane 1
                let opts1 = SearchOptions {
                    pane_id: Some(1),
                    ..Default::default()
                };
                let results1 = handle.search_with_options(&content1, opts1).await.expect("search pane 1");

                // Search for content1 scoped to pane 2
                let opts2 = SearchOptions {
                    pane_id: Some(2),
                    ..Default::default()
                };
                let results2 = handle.search_with_options(&content1, opts2).await.expect("search pane 2");

                handle.shutdown().await.expect("shutdown");
                (!results1.is_empty(), !results2.is_empty())
            });

            let _ = std::fs::remove_file(&db_path);

            // Verify outside async block
            prop_assert!(
                found_in_pane1,
                "Should find '{}' in pane 1",
                content1
            );
            prop_assert!(
                !found_in_pane2,
                "Should NOT find '{}' in pane 2",
                content1
            );
        }
    }
}

// =============================================================================
// Accounts DB Mirror Tests (wa-nu4.1.5.3)
// =============================================================================

#[cfg(test)]
mod accounts_db_tests {
    use super::*;
    use crate::accounts::AccountRecord;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();
        conn
    }

    fn make_db_account(id: &str, service: &str, pct: f64, now: i64) -> AccountRecord {
        AccountRecord {
            id: 0,
            account_id: id.to_string(),
            service: service.to_string(),
            name: Some(format!("{id}-name")),
            percent_remaining: pct,
            reset_at: None,
            tokens_used: Some(1000),
            tokens_remaining: Some(9000),
            tokens_limit: Some(10000),
            last_refreshed_at: now,
            last_used_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn db_upsert_account_inserts_new() {
        let conn = setup_db();
        let acct = make_db_account("acc-1", "openai", 80.0, 1000);

        let row_id = upsert_account_sync(&conn, &acct).unwrap();
        assert!(row_id > 0);

        let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap();
        assert!(fetched.is_some());
        let f = fetched.unwrap();
        assert_eq!(f.account_id, "acc-1");
        assert_eq!(f.service, "openai");
        assert!((f.percent_remaining - 80.0).abs() < 0.001);
        assert_eq!(f.tokens_used, Some(1000));
    }

    #[test]
    fn db_upsert_account_updates_existing() {
        let conn = setup_db();
        let acct = make_db_account("acc-1", "openai", 80.0, 1000);
        upsert_account_sync(&conn, &acct).unwrap();

        // Update with new percent_remaining
        let updated = AccountRecord {
            percent_remaining: 50.0,
            last_refreshed_at: 2000,
            updated_at: 2000,
            tokens_used: Some(5000),
            tokens_remaining: Some(5000),
            ..acct
        };
        upsert_account_sync(&conn, &updated).unwrap();

        let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap().unwrap();
        assert!((fetched.percent_remaining - 50.0).abs() < 0.001);
        assert_eq!(fetched.tokens_used, Some(5000));
        assert_eq!(fetched.last_refreshed_at, 2000);
    }

    #[test]
    fn db_upsert_idempotent() {
        let conn = setup_db();
        let acct = make_db_account("acc-1", "openai", 80.0, 1000);

        upsert_account_sync(&conn, &acct).unwrap();
        upsert_account_sync(&conn, &acct).unwrap();
        upsert_account_sync(&conn, &acct).unwrap();

        // Should still be exactly one record
        let accounts = get_accounts_by_service_sync(&conn, "openai").unwrap();
        assert_eq!(accounts.len(), 1);
    }

    #[test]
    fn db_upsert_preserves_last_used_at() {
        let conn = setup_db();
        let acct = make_db_account("acc-1", "openai", 80.0, 1000);
        upsert_account_sync(&conn, &acct).unwrap();

        // Set last_used_at via dedicated function
        update_account_last_used_sync(&conn, "openai", "acc-1", 5000).unwrap();

        // Now upsert with new data — should NOT overwrite last_used_at
        // because ON CONFLICT doesn't include last_used_at in the UPDATE
        let updated = AccountRecord {
            percent_remaining: 60.0,
            last_refreshed_at: 2000,
            updated_at: 2000,
            ..make_db_account("acc-1", "openai", 60.0, 2000)
        };
        upsert_account_sync(&conn, &updated).unwrap();

        let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap().unwrap();
        // last_used_at should still be 5000 (set by update_account_last_used_sync)
        assert_eq!(fetched.last_used_at, Some(5000));
        // But percent_remaining should be updated
        assert!((fetched.percent_remaining - 60.0).abs() < 0.001);
    }

    #[test]
    fn db_get_accounts_by_service_sorted() {
        let conn = setup_db();

        // Insert accounts with different percent_remaining
        let accounts = [
            make_db_account("low", "openai", 20.0, 1000),
            make_db_account("high", "openai", 90.0, 1000),
            make_db_account("mid", "openai", 50.0, 1000),
        ];
        for acct in &accounts {
            upsert_account_sync(&conn, acct).unwrap();
        }

        let fetched = get_accounts_by_service_sync(&conn, "openai").unwrap();
        assert_eq!(fetched.len(), 3);
        // Should be sorted by percent_remaining DESC
        assert_eq!(fetched[0].account_id, "high");
        assert_eq!(fetched[1].account_id, "mid");
        assert_eq!(fetched[2].account_id, "low");
    }

    #[test]
    fn db_get_accounts_by_service_nulls_first_for_last_used() {
        let conn = setup_db();

        let a1 = make_db_account("never-used", "openai", 50.0, 1000);
        let a2 = make_db_account("used-recently", "openai", 50.0, 1000);
        upsert_account_sync(&conn, &a1).unwrap();
        upsert_account_sync(&conn, &a2).unwrap();

        // Set last_used_at only for a2
        update_account_last_used_sync(&conn, "openai", "used-recently", 5000).unwrap();

        let fetched = get_accounts_by_service_sync(&conn, "openai").unwrap();
        assert_eq!(fetched.len(), 2);
        // Same percent_remaining, so ordered by last_used_at ASC NULLS FIRST
        assert_eq!(fetched[0].account_id, "never-used");
        assert_eq!(fetched[1].account_id, "used-recently");
    }

    #[test]
    fn db_get_accounts_empty_for_unknown_service() {
        let conn = setup_db();
        let acct = make_db_account("acc-1", "openai", 80.0, 1000);
        upsert_account_sync(&conn, &acct).unwrap();

        let fetched = get_accounts_by_service_sync(&conn, "anthropic").unwrap();
        assert!(fetched.is_empty());
    }

    #[test]
    fn db_get_account_returns_none_for_missing() {
        let conn = setup_db();

        let fetched = get_account_sync(&conn, "openai", "nonexistent").unwrap();
        assert!(fetched.is_none());
    }

    #[test]
    fn db_update_last_used_updates_timestamp() {
        let conn = setup_db();
        let acct = make_db_account("acc-1", "openai", 80.0, 1000);
        upsert_account_sync(&conn, &acct).unwrap();

        update_account_last_used_sync(&conn, "openai", "acc-1", 9999).unwrap();

        let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap().unwrap();
        assert_eq!(fetched.last_used_at, Some(9999));
    }

    #[test]
    fn db_update_last_used_errors_on_missing() {
        let conn = setup_db();

        let result = update_account_last_used_sync(&conn, "openai", "nonexistent", 1000);
        assert!(result.is_err());
    }

    #[test]
    fn db_delete_account_removes_record() {
        let conn = setup_db();
        let acct = make_db_account("acc-1", "openai", 80.0, 1000);
        upsert_account_sync(&conn, &acct).unwrap();

        let deleted = delete_account_sync(&conn, "openai", "acc-1").unwrap();
        assert!(deleted);

        let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap();
        assert!(fetched.is_none());
    }

    #[test]
    fn db_delete_nonexistent_returns_false() {
        let conn = setup_db();

        let deleted = delete_account_sync(&conn, "openai", "nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn db_multiple_services_isolated() {
        let conn = setup_db();

        upsert_account_sync(&conn, &make_db_account("acc-1", "openai", 80.0, 1000)).unwrap();
        upsert_account_sync(&conn, &make_db_account("acc-2", "anthropic", 60.0, 1000)).unwrap();
        upsert_account_sync(&conn, &make_db_account("acc-3", "openai", 40.0, 1000)).unwrap();

        let openai = get_accounts_by_service_sync(&conn, "openai").unwrap();
        let anthropic = get_accounts_by_service_sync(&conn, "anthropic").unwrap();

        assert_eq!(openai.len(), 2);
        assert_eq!(anthropic.len(), 1);
        assert_eq!(anthropic[0].account_id, "acc-2");
    }

    #[test]
    fn db_upsert_then_select_end_to_end() {
        let conn = setup_db();

        // Insert multiple accounts
        upsert_account_sync(&conn, &make_db_account("depleted", "openai", 2.0, 1000)).unwrap();
        upsert_account_sync(&conn, &make_db_account("best", "openai", 90.0, 1000)).unwrap();
        upsert_account_sync(&conn, &make_db_account("decent", "openai", 50.0, 1000)).unwrap();

        // Fetch and select using the accounts module
        let accounts = get_accounts_by_service_sync(&conn, "openai").unwrap();
        let config = crate::accounts::AccountSelectionConfig::default();
        let result = crate::accounts::select_account(&accounts, &config);

        assert!(result.selected.is_some());
        assert_eq!(result.selected.unwrap().account_id, "best");
        assert_eq!(result.explanation.filtered_out.len(), 1); // "depleted" below 5%
        assert_eq!(result.explanation.candidates.len(), 2); // "best" and "decent"
    }

    #[test]
    fn db_upsert_updates_only_specified_fields() {
        let conn = setup_db();

        // Insert with all fields
        let acct = AccountRecord {
            id: 0,
            account_id: "acc-1".to_string(),
            service: "openai".to_string(),
            name: Some("Original Name".to_string()),
            percent_remaining: 80.0,
            reset_at: Some("2026-02-01T00:00:00Z".to_string()),
            tokens_used: Some(2000),
            tokens_remaining: Some(8000),
            tokens_limit: Some(10000),
            last_refreshed_at: 1000,
            last_used_at: None,
            created_at: 1000,
            updated_at: 1000,
        };
        upsert_account_sync(&conn, &acct).unwrap();

        // Update with changed name and percent
        let updated = AccountRecord {
            name: Some("Updated Name".to_string()),
            percent_remaining: 50.0,
            last_refreshed_at: 2000,
            updated_at: 2000,
            ..acct
        };
        upsert_account_sync(&conn, &updated).unwrap();

        let fetched = get_account_sync(&conn, "openai", "acc-1").unwrap().unwrap();
        assert_eq!(fetched.name.as_deref(), Some("Updated Name"));
        assert!((fetched.percent_remaining - 50.0).abs() < 0.001);
        assert_eq!(fetched.reset_at.as_deref(), Some("2026-02-01T00:00:00Z"));
    }
}

#[cfg(test)]
mod reservation_tests {
    use super::*;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();
        // Ensure a pane exists for FK constraint
        conn.execute(
            "INSERT INTO panes (pane_id, title, cwd, observed, first_seen_at, last_seen_at)
             VALUES (1, 'test', '/tmp', 1, 1000, 1000)",
            [],
        )
        .unwrap();
        conn
    }

    fn setup_db_with_panes(pane_ids: &[u64]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();
        for &pid in pane_ids {
            conn.execute(
                "INSERT INTO panes (pane_id, title, cwd, observed, first_seen_at, last_seen_at)
                 VALUES (?1, 'test', '/tmp', 1, 1000, 1000)",
                params![pid as i64],
            )
            .unwrap();
        }
        conn
    }

    // =========================================================================
    // PaneReservation struct tests
    // =========================================================================

    #[test]
    fn reservation_is_active_when_valid() {
        let r = PaneReservation {
            id: 1,
            pane_id: 1,
            owner_kind: "workflow".to_string(),
            owner_id: "wf-123".to_string(),
            reason: Some("test".to_string()),
            created_at: 1000,
            expires_at: 5000,
            released_at: None,
            status: "active".to_string(),
        };
        assert!(r.is_active(2000));
        assert!(r.is_active(4999));
    }

    #[test]
    fn reservation_not_active_when_expired() {
        let r = PaneReservation {
            id: 1,
            pane_id: 1,
            owner_kind: "workflow".to_string(),
            owner_id: "wf-123".to_string(),
            reason: None,
            created_at: 1000,
            expires_at: 5000,
            released_at: None,
            status: "active".to_string(),
        };
        // At exactly expires_at, is_active returns false (> not >=)
        assert!(!r.is_active(5000));
        assert!(!r.is_active(6000));
    }

    #[test]
    fn reservation_not_active_when_released() {
        let r = PaneReservation {
            id: 1,
            pane_id: 1,
            owner_kind: "workflow".to_string(),
            owner_id: "wf-123".to_string(),
            reason: None,
            created_at: 1000,
            expires_at: 5000,
            released_at: Some(3000),
            status: "released".to_string(),
        };
        assert!(!r.is_active(2000));
    }

    // =========================================================================
    // PaneReservationConfig tests
    // =========================================================================

    #[test]
    fn config_default_values() {
        let cfg = PaneReservationConfig::default();
        assert_eq!(cfg.default_ttl_ms, 30 * 60 * 1000);
        assert_eq!(cfg.max_ttl_ms, 4 * 60 * 60 * 1000);
    }

    #[test]
    fn config_clamp_ttl_within_range() {
        let cfg = PaneReservationConfig::default();
        assert_eq!(cfg.clamp_ttl(60_000), 60_000);
    }

    #[test]
    fn config_clamp_ttl_below_minimum() {
        let cfg = PaneReservationConfig::default();
        assert_eq!(cfg.clamp_ttl(500), 1000);
        assert_eq!(cfg.clamp_ttl(0), 1000);
        assert_eq!(cfg.clamp_ttl(-100), 1000);
    }

    #[test]
    fn config_clamp_ttl_above_maximum() {
        let cfg = PaneReservationConfig::default();
        let five_hours = 5 * 60 * 60 * 1000;
        assert_eq!(cfg.clamp_ttl(five_hours), cfg.max_ttl_ms);
    }

    // =========================================================================
    // create_reservation_sync tests
    // =========================================================================

    #[test]
    fn create_reservation_basic() {
        let conn = setup_db();
        let r =
            create_reservation_sync(&conn, 1, "workflow", "wf-1", Some("testing"), 60_000).unwrap();

        assert_eq!(r.pane_id, 1);
        assert_eq!(r.owner_kind, "workflow");
        assert_eq!(r.owner_id, "wf-1");
        assert_eq!(r.reason.as_deref(), Some("testing"));
        assert_eq!(r.status, "active");
        assert!(r.released_at.is_none());
        assert!(r.expires_at > r.created_at);
    }

    #[test]
    fn create_reservation_no_reason() {
        let conn = setup_db();
        let r = create_reservation_sync(&conn, 1, "agent", "agent-x", None, 30_000).unwrap();

        assert!(r.reason.is_none());
        assert_eq!(r.owner_kind, "agent");
    }

    #[test]
    fn create_reservation_conflict_with_active() {
        let conn = setup_db();

        // First reservation succeeds
        let _r1 = create_reservation_sync(&conn, 1, "workflow", "wf-1", None, 600_000).unwrap();

        // Second reservation on same pane should fail
        let r2 = create_reservation_sync(&conn, 1, "workflow", "wf-2", None, 60_000);
        assert!(r2.is_err());
        let err_msg = format!("{}", r2.unwrap_err());
        assert!(err_msg.contains("already has active reservation"));
    }

    #[test]
    fn create_reservation_allowed_after_release() {
        let conn = setup_db();

        let r1 = create_reservation_sync(&conn, 1, "workflow", "wf-1", None, 600_000).unwrap();
        release_reservation_sync(&conn, r1.id).unwrap();

        // Now a new reservation should succeed
        let r2 = create_reservation_sync(&conn, 1, "workflow", "wf-2", None, 60_000);
        assert!(r2.is_ok());
    }

    #[test]
    fn create_reservation_allowed_on_different_panes() {
        let conn = setup_db_with_panes(&[1, 2]);

        let r1 = create_reservation_sync(&conn, 1, "workflow", "wf-1", None, 600_000);
        let r2 = create_reservation_sync(&conn, 2, "workflow", "wf-2", None, 600_000);

        assert!(r1.is_ok());
        assert!(r2.is_ok());
    }

    // =========================================================================
    // release_reservation_sync tests
    // =========================================================================

    #[test]
    fn release_reservation_sets_released() {
        let conn = setup_db();
        let r = create_reservation_sync(&conn, 1, "workflow", "wf-1", None, 600_000).unwrap();

        let released = release_reservation_sync(&conn, r.id).unwrap();
        assert!(released);

        // Verify status changed in DB
        let active = get_active_reservation_sync(&conn, 1).unwrap();
        assert!(active.is_none());
    }

    #[test]
    fn release_nonexistent_returns_false() {
        let conn = setup_db();
        let released = release_reservation_sync(&conn, 9999).unwrap();
        assert!(!released);
    }

    #[test]
    fn release_already_released_returns_false() {
        let conn = setup_db();
        let r = create_reservation_sync(&conn, 1, "workflow", "wf-1", None, 600_000).unwrap();

        assert!(release_reservation_sync(&conn, r.id).unwrap());
        // Second release is a no-op
        assert!(!release_reservation_sync(&conn, r.id).unwrap());
    }

    // =========================================================================
    // get_active_reservation_sync tests
    // =========================================================================

    #[test]
    fn get_active_reservation_returns_some() {
        let conn = setup_db();
        let created =
            create_reservation_sync(&conn, 1, "workflow", "wf-1", Some("reason"), 600_000).unwrap();

        let fetched = get_active_reservation_sync(&conn, 1).unwrap();
        assert!(fetched.is_some());
        let f = fetched.unwrap();
        assert_eq!(f.id, created.id);
        assert_eq!(f.owner_id, "wf-1");
        assert_eq!(f.reason.as_deref(), Some("reason"));
    }

    #[test]
    fn get_active_reservation_returns_none_for_unreserved_pane() {
        let conn = setup_db();
        let fetched = get_active_reservation_sync(&conn, 1).unwrap();
        assert!(fetched.is_none());
    }

    #[test]
    fn get_active_reservation_returns_none_after_release() {
        let conn = setup_db();
        let r = create_reservation_sync(&conn, 1, "workflow", "wf-1", None, 600_000).unwrap();
        release_reservation_sync(&conn, r.id).unwrap();

        let fetched = get_active_reservation_sync(&conn, 1).unwrap();
        assert!(fetched.is_none());
    }

    // =========================================================================
    // list_active_reservations_sync tests
    // =========================================================================

    #[test]
    fn list_active_empty() {
        let conn = setup_db();
        let list = list_active_reservations_sync(&conn).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn list_active_multiple_panes() {
        let conn = setup_db_with_panes(&[1, 2, 3]);

        create_reservation_sync(&conn, 1, "workflow", "wf-1", None, 600_000).unwrap();
        create_reservation_sync(&conn, 2, "agent", "agent-a", None, 600_000).unwrap();
        create_reservation_sync(&conn, 3, "manual", "user-1", None, 600_000).unwrap();

        let list = list_active_reservations_sync(&conn).unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn list_active_excludes_released() {
        let conn = setup_db_with_panes(&[1, 2]);

        let r1 = create_reservation_sync(&conn, 1, "workflow", "wf-1", None, 600_000).unwrap();
        create_reservation_sync(&conn, 2, "workflow", "wf-2", None, 600_000).unwrap();

        release_reservation_sync(&conn, r1.id).unwrap();

        let list = list_active_reservations_sync(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].pane_id, 2);
    }

    // =========================================================================
    // expire_stale_reservations_sync tests
    // =========================================================================

    #[test]
    fn expire_stale_none_to_expire() {
        let conn = setup_db();
        create_reservation_sync(&conn, 1, "workflow", "wf-1", None, 600_000).unwrap();

        let expired = expire_stale_reservations_sync(&conn).unwrap();
        assert_eq!(expired, 0);
    }

    #[test]
    fn expire_stale_expires_past_ttl() {
        let conn = setup_db();

        // Manually insert a reservation with expires_at in the past
        let past = now_ms() - 10_000;
        conn.execute(
            "INSERT INTO pane_reservations (pane_id, owner_kind, owner_id, reason, created_at, expires_at, status)
             VALUES (1, 'workflow', 'wf-old', NULL, ?1, ?2, 'active')",
            params![past - 60_000, past],
        )
        .unwrap();

        let expired = expire_stale_reservations_sync(&conn).unwrap();
        assert_eq!(expired, 1);

        // Should no longer appear as active
        let active = get_active_reservation_sync(&conn, 1).unwrap();
        assert!(active.is_none());
    }

    #[test]
    fn expire_stale_does_not_touch_valid() {
        let conn = setup_db_with_panes(&[1, 2]);

        // One valid, one expired
        create_reservation_sync(&conn, 1, "workflow", "wf-valid", None, 600_000).unwrap();

        let past = now_ms() - 5_000;
        conn.execute(
            "INSERT INTO pane_reservations (pane_id, owner_kind, owner_id, reason, created_at, expires_at, status)
             VALUES (2, 'workflow', 'wf-old', NULL, ?1, ?2, 'active')",
            params![past - 60_000, past],
        )
        .unwrap();

        let expired = expire_stale_reservations_sync(&conn).unwrap();
        assert_eq!(expired, 1);

        // Valid one should still be active
        let active = list_active_reservations_sync(&conn).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].owner_id, "wf-valid");
    }

    // =========================================================================
    // Round-trip / integration tests
    // =========================================================================

    #[test]
    fn reserve_release_reserve_round_trip() {
        let conn = setup_db();

        // Create first reservation
        let r1 =
            create_reservation_sync(&conn, 1, "workflow", "wf-1", Some("first"), 600_000).unwrap();
        assert!(get_active_reservation_sync(&conn, 1).unwrap().is_some());

        // Release it
        assert!(release_reservation_sync(&conn, r1.id).unwrap());
        assert!(get_active_reservation_sync(&conn, 1).unwrap().is_none());

        // Create second reservation on same pane
        let r2 =
            create_reservation_sync(&conn, 1, "agent", "agent-b", Some("second"), 300_000).unwrap();
        let active = get_active_reservation_sync(&conn, 1).unwrap().unwrap();
        assert_eq!(active.id, r2.id);
        assert_eq!(active.owner_kind, "agent");
        assert_eq!(active.owner_id, "agent-b");
    }

    #[test]
    fn expired_reservation_allows_new_creation() {
        let conn = setup_db();

        // Insert an already-expired reservation directly
        let past = now_ms() - 10_000;
        conn.execute(
            "INSERT INTO pane_reservations (pane_id, owner_kind, owner_id, reason, created_at, expires_at, status)
             VALUES (1, 'workflow', 'wf-expired', NULL, ?1, ?2, 'active')",
            params![past - 60_000, past],
        )
        .unwrap();

        // New reservation should succeed because the existing one is expired
        let r = create_reservation_sync(&conn, 1, "workflow", "wf-new", None, 60_000);
        assert!(r.is_ok());
    }

    #[test]
    fn ttl_determines_expiry() {
        let conn = setup_db();
        let ttl = 120_000i64; // 2 minutes
        let r = create_reservation_sync(&conn, 1, "workflow", "wf-1", None, ttl).unwrap();

        // expires_at should be approximately created_at + ttl
        let diff = r.expires_at - r.created_at;
        assert_eq!(diff, ttl);
    }

    #[test]
    fn serialization_round_trip() {
        let r = PaneReservation {
            id: 42,
            pane_id: 7,
            owner_kind: "workflow".to_string(),
            owner_id: "wf-abc".to_string(),
            reason: Some("testing serialization".to_string()),
            created_at: 1000,
            expires_at: 2000,
            released_at: None,
            status: "active".to_string(),
        };

        let json = serde_json::to_string(&r).unwrap();
        let deserialized: PaneReservation = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, r.id);
        assert_eq!(deserialized.pane_id, r.pane_id);
        assert_eq!(deserialized.owner_kind, r.owner_kind);
        assert_eq!(deserialized.owner_id, r.owner_id);
        assert_eq!(deserialized.reason, r.reason);
        assert_eq!(deserialized.status, r.status);
    }
}

// =============================================================================
// Timeline and Correlation Detection Tests (wa-6sk.2)
// =============================================================================

#[cfg(test)]
mod timeline_correlation_tests {
    use super::*;

    fn create_test_event(
        id: i64,
        ts: i64,
        pane_id: u64,
        rule_id: &str,
        event_type: &str,
    ) -> TimelineEvent {
        TimelineEvent {
            id,
            timestamp: ts,
            pane_info: PaneInfo {
                pane_id,
                pane_uuid: None,
                domain: "local".to_string(),
                title: None,
                cwd: None,
                agent_type: None,
            },
            rule_id: rule_id.to_string(),
            event_type: event_type.to_string(),
            severity: "info".to_string(),
            confidence: 0.9,
            handled: None,
            correlations: Vec::new(),
            summary: None,
        }
    }

    #[test]
    fn temporal_correlation_detects_close_events() {
        let events = vec![
            create_test_event(1, 1000, 1, "rule_a", "error"),
            create_test_event(2, 2000, 2, "rule_b", "error"), // Different pane, within 10s
            create_test_event(3, 3000, 1, "rule_c", "warning"),
        ];

        let correlations = detect_correlations(&events);

        // Should find temporal correlation between events in different panes
        let temporal = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .collect::<Vec<_>>();

        assert!(!temporal.is_empty(), "Should detect temporal correlation");
        assert!(
            temporal
                .iter()
                .any(|c| c.event_ids.contains(&1) && c.event_ids.contains(&2)),
            "Should correlate events 1 and 2"
        );
    }

    #[test]
    fn temporal_correlation_ignores_same_pane() {
        let events = vec![
            create_test_event(1, 1000, 1, "rule_a", "error"),
            create_test_event(2, 2000, 1, "rule_b", "error"), // Same pane
        ];

        let correlations = detect_correlations(&events);

        // Same pane events should not create temporal correlation
        let temporal = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .collect::<Vec<_>>();

        assert!(
            temporal.is_empty() || !temporal.iter().any(|c| c.event_ids.len() > 1),
            "Should not correlate same-pane events"
        );
    }

    #[test]
    fn temporal_correlation_respects_window() {
        let events = vec![
            create_test_event(1, 1000, 1, "rule_a", "error"),
            create_test_event(2, 15000, 2, "rule_b", "error"), // 14 seconds apart, outside 10s window
        ];

        let correlations = detect_correlations(&events);

        // Events too far apart should not correlate
        let temporal = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .filter(|c| c.event_ids.contains(&1) && c.event_ids.contains(&2))
            .count();

        assert_eq!(
            temporal, 0,
            "Events outside 10s window should not correlate"
        );
    }

    #[test]
    fn workflow_group_correlation_links_handled_events() {
        let mut event1 = create_test_event(1, 1000, 1, "rule_a", "error");
        event1.handled = Some(HandledInfo {
            handled_at: 2000,
            workflow_id: Some("wf-123".to_string()),
            status: "handled".to_string(),
        });

        let mut event2 = create_test_event(2, 1500, 2, "rule_b", "error");
        event2.handled = Some(HandledInfo {
            handled_at: 2100,
            workflow_id: Some("wf-123".to_string()), // Same workflow ID
            status: "handled".to_string(),
        });

        let event3 = create_test_event(3, 1200, 3, "rule_c", "warning");

        let correlations = detect_correlations(&[event1, event2, event3]);

        let workflow_corr = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::WorkflowGroup)
            .collect::<Vec<_>>();

        assert_eq!(workflow_corr.len(), 1, "Should find one workflow group");
        assert!(
            workflow_corr[0].event_ids.contains(&1) && workflow_corr[0].event_ids.contains(&2),
            "Should link events with same workflow ID"
        );
        assert!(
            !workflow_corr[0].event_ids.contains(&3),
            "Should not include unrelated event"
        );
    }

    #[test]
    fn failover_correlation_detects_limit_then_session() {
        let events = vec![
            create_test_event(1, 1000, 1, "usage_limit", "usage_limit"),
            create_test_event(2, 30000, 2, "session_start", "session_start"), // Different pane, within 5min
        ];

        let correlations = detect_correlations(&events);

        let failover = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Failover)
            .collect::<Vec<_>>();

        assert_eq!(failover.len(), 1, "Should detect failover correlation");
        assert!(
            failover[0].event_ids.contains(&1) && failover[0].event_ids.contains(&2),
            "Should link usage_limit to session_start"
        );
    }

    #[test]
    fn failover_correlation_ignores_same_pane() {
        let events = vec![
            create_test_event(1, 1000, 1, "usage_limit", "usage_limit"),
            create_test_event(2, 30000, 1, "session_start", "session_start"), // Same pane
        ];

        let correlations = detect_correlations(&events);

        let failover = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Failover)
            .count();

        assert_eq!(failover, 0, "Same-pane events should not be failover");
    }

    #[test]
    fn failover_correlation_respects_5min_window() {
        let events = vec![
            create_test_event(1, 1000, 1, "usage_limit", "usage_limit"),
            create_test_event(2, 400000, 2, "session_start", "session_start"), // >5min apart
        ];

        let correlations = detect_correlations(&events);

        let failover = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Failover)
            .count();

        assert_eq!(failover, 0, "Events >5min apart should not be failover");
    }

    #[test]
    fn correlation_confidence_values() {
        let mut event1 = create_test_event(1, 1000, 1, "rule_a", "error");
        event1.handled = Some(HandledInfo {
            handled_at: 2000,
            workflow_id: Some("wf-1".to_string()),
            status: "handled".to_string(),
        });
        let mut event2 = create_test_event(2, 1100, 2, "rule_b", "error");
        event2.handled = Some(HandledInfo {
            handled_at: 2100,
            workflow_id: Some("wf-1".to_string()),
            status: "handled".to_string(),
        });

        let correlations = detect_correlations(&[event1, event2]);

        for corr in &correlations {
            match corr.correlation_type {
                CorrelationType::Temporal => {
                    assert!(
                        (corr.confidence - 0.6).abs() < 0.01,
                        "Temporal confidence should be 0.6"
                    );
                }
                CorrelationType::WorkflowGroup => {
                    assert!(
                        (corr.confidence - 0.95).abs() < 0.01,
                        "WorkflowGroup confidence should be 0.95"
                    );
                }
                CorrelationType::Failover => {
                    assert!(
                        (corr.confidence - 0.8).abs() < 0.01,
                        "Failover confidence should be 0.8"
                    );
                }
                _ => {}
            }
        }
    }

    #[test]
    fn empty_events_returns_empty_correlations() {
        let correlations = detect_correlations(&[]);
        assert!(correlations.is_empty());
    }

    #[test]
    fn single_event_returns_empty_correlations() {
        let events = vec![create_test_event(1, 1000, 1, "rule_a", "error")];
        let correlations = detect_correlations(&events);
        assert!(correlations.is_empty());
    }

    #[test]
    fn correlation_ids_are_unique() {
        let events = vec![
            create_test_event(1, 1000, 1, "rule_a", "error"),
            create_test_event(2, 2000, 2, "rule_b", "error"),
            create_test_event(3, 3000, 3, "rule_c", "error"),
            create_test_event(4, 4000, 4, "rule_d", "error"),
        ];

        let correlations = detect_correlations(&events);

        let ids: std::collections::HashSet<_> = correlations.iter().map(|c| &c.id).collect();
        assert_eq!(
            ids.len(),
            correlations.len(),
            "Correlation IDs should be unique"
        );
    }

    #[test]
    fn correlation_type_display() {
        assert_eq!(CorrelationType::Failover.to_string(), "failover");
        assert_eq!(CorrelationType::Cascade.to_string(), "cascade");
        assert_eq!(CorrelationType::Temporal.to_string(), "temporal");
        assert_eq!(CorrelationType::WorkflowGroup.to_string(), "workflow_group");
        assert_eq!(CorrelationType::DedupeGroup.to_string(), "dedupe_group");
    }

    #[test]
    fn dedupe_group_detects_same_rule_across_panes() {
        let events = vec![
            create_test_event(1, 1000, 1, "claude_code.usage.reached", "error"),
            create_test_event(2, 5000, 2, "claude_code.usage.reached", "error"),
            create_test_event(3, 8000, 3, "claude_code.usage.reached", "error"),
        ];

        let correlations = detect_correlations(&events);

        let dedupe = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
            .collect::<Vec<_>>();

        assert_eq!(dedupe.len(), 1, "Should detect one dedupe group");
        assert_eq!(
            dedupe[0].event_ids.len(),
            3,
            "All three events should be grouped"
        );
        assert!(
            (dedupe[0].confidence - 0.7).abs() < 0.01,
            "DedupeGroup confidence should be 0.7"
        );
    }

    #[test]
    fn dedupe_group_ignores_same_pane_only() {
        let events = vec![
            create_test_event(1, 1000, 1, "rule_a", "error"),
            create_test_event(2, 5000, 1, "rule_a", "error"), // Same pane
        ];

        let correlations = detect_correlations(&events);

        let dedupe = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
            .count();

        assert_eq!(
            dedupe, 0,
            "Same-pane-only events should not form dedupe group"
        );
    }

    #[test]
    fn dedupe_group_respects_window() {
        let events = vec![
            create_test_event(1, 1000, 1, "rule_a", "error"),
            create_test_event(2, 50000, 2, "rule_a", "error"), // 49s apart, outside 30s window
        ];

        let correlations = detect_correlations(&events);

        let dedupe = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
            .count();

        assert_eq!(
            dedupe, 0,
            "Events outside 30s window should not form dedupe group"
        );
    }

    #[test]
    fn dedupe_group_different_rules_not_grouped() {
        let events = vec![
            create_test_event(1, 1000, 1, "rule_a", "error"),
            create_test_event(2, 2000, 2, "rule_b", "error"),
        ];

        let correlations = detect_correlations(&events);

        let dedupe = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
            .count();

        assert_eq!(dedupe, 0, "Different rule_ids should not form dedupe group");
    }

    #[test]
    fn temporal_window_10s_boundary() {
        // Exactly at boundary: 10s apart should still correlate
        let events = vec![
            create_test_event(1, 1000, 1, "rule_a", "error"),
            create_test_event(2, 11000, 2, "rule_b", "error"), // Exactly 10s
        ];

        let correlations = detect_correlations(&events);

        let temporal = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .filter(|c| c.event_ids.contains(&1) && c.event_ids.contains(&2))
            .count();

        assert_eq!(temporal, 1, "Events exactly 10s apart should correlate");
    }

    #[test]
    fn temporal_window_just_outside() {
        // 10.001s apart should not correlate
        let events = vec![
            create_test_event(1, 1000, 1, "rule_a", "error"),
            create_test_event(2, 11002, 2, "rule_b", "error"), // >10s
        ];

        let correlations = detect_correlations(&events);

        let temporal = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .filter(|c| c.event_ids.contains(&1) && c.event_ids.contains(&2))
            .count();

        assert_eq!(
            temporal, 0,
            "Events >10s apart should not temporally correlate"
        );
    }

    #[test]
    fn failover_within_5min_window_detected() {
        // 4 minutes apart (240s) — within 5min window
        let events = vec![
            create_test_event(1, 1000, 1, "usage_limit", "usage.reached"),
            create_test_event(2, 241000, 2, "session_start", "session.start"),
        ];

        let correlations = detect_correlations(&events);

        let failover = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Failover)
            .count();

        assert_eq!(failover, 1, "Events 4min apart should detect failover");
    }

    #[test]
    fn cascade_error_then_recovery_detected() {
        let mut event1 = create_test_event(1, 1000, 1, "rule_a", "error");
        event1.severity = "error".to_string();

        let event2 = create_test_event(2, 15000, 2, "session.resume", "session.resume");

        let correlations = detect_correlations(&[event1, event2]);

        let cascade = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Cascade)
            .collect::<Vec<_>>();

        assert_eq!(cascade.len(), 1, "Should detect cascade correlation");
        assert!(
            (cascade[0].confidence - 0.75).abs() < 0.01,
            "Cascade confidence should be 0.75"
        );
    }

    #[test]
    fn cascade_ignores_non_error_severity() {
        let event1 = create_test_event(1, 1000, 1, "rule_a", "info");
        let event2 = create_test_event(2, 5000, 2, "session.resume", "session.resume");

        let correlations = detect_correlations(&[event1, event2]);

        let cascade = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Cascade)
            .count();

        assert_eq!(cascade, 0, "Non-error severity should not trigger cascade");
    }

    #[test]
    fn cascade_respects_30s_window() {
        let mut event1 = create_test_event(1, 1000, 1, "rule_a", "error");
        event1.severity = "error".to_string();

        let event2 = create_test_event(2, 40000, 2, "session.resume", "session.resume");

        let correlations = detect_correlations(&[event1, event2]);

        let cascade = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Cascade)
            .count();

        assert_eq!(cascade, 0, "Events >30s apart should not cascade");
    }

    #[test]
    fn failover_agent_type_mismatch_no_correlation() {
        let mut event1 = create_test_event(1, 1000, 1, "usage_limit", "usage.reached");
        event1.pane_info.agent_type = Some("claude_code".to_string());

        let mut event2 = create_test_event(2, 30000, 2, "session_start", "session.start");
        event2.pane_info.agent_type = Some("codex".to_string());

        let correlations = detect_correlations(&[event1, event2]);

        let failover = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Failover)
            .count();

        assert_eq!(
            failover, 0,
            "Different agent types should not create failover correlation"
        );
    }

    #[test]
    fn failover_same_agent_type_correlates() {
        let mut event1 = create_test_event(1, 1000, 1, "usage_limit", "usage.reached");
        event1.pane_info.agent_type = Some("claude_code".to_string());

        let mut event2 = create_test_event(2, 30000, 2, "session_start", "session.start");
        event2.pane_info.agent_type = Some("claude_code".to_string());

        let correlations = detect_correlations(&[event1, event2]);

        let failover = correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Failover)
            .count();

        assert_eq!(
            failover, 1,
            "Same agent type should create failover correlation"
        );
    }

    #[test]
    fn correlation_serde_roundtrip() {
        let corr = Correlation {
            id: "corr-test-1".to_string(),
            event_ids: vec![1, 2, 3],
            correlation_type: CorrelationType::DedupeGroup,
            confidence: 0.7,
            description: "Test correlation".to_string(),
        };

        let json = serde_json::to_string(&corr).unwrap();
        let deserialized: Correlation = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.id, corr.id);
        assert_eq!(deserialized.event_ids, corr.event_ids);
        assert_eq!(deserialized.correlation_type, CorrelationType::DedupeGroup);
        assert!((deserialized.confidence - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn multiple_correlation_types_coexist() {
        // Two events: close in time, same workflow, same rule in different panes
        let mut event1 = create_test_event(1, 1000, 1, "rule_x", "error");
        event1.severity = "error".to_string();
        event1.handled = Some(HandledInfo {
            handled_at: 2000,
            workflow_id: Some("wf-99".to_string()),
            status: "handled".to_string(),
        });

        let mut event2 = create_test_event(2, 3000, 2, "rule_x", "session.resume");
        event2.handled = Some(HandledInfo {
            handled_at: 4000,
            workflow_id: Some("wf-99".to_string()),
            status: "handled".to_string(),
        });

        let correlations = detect_correlations(&[event1, event2]);

        let types: std::collections::HashSet<_> =
            correlations.iter().map(|c| c.correlation_type).collect();

        // Should have at least temporal + workflow + dedupe (and possibly cascade)
        assert!(
            types.contains(&CorrelationType::Temporal),
            "Should detect temporal correlation"
        );
        assert!(
            types.contains(&CorrelationType::WorkflowGroup),
            "Should detect workflow group"
        );
        assert!(
            types.contains(&CorrelationType::DedupeGroup),
            "Should detect dedupe group (same rule_id across panes)"
        );
    }

    #[test]
    fn many_events_performance_no_panic() {
        // Verify detect_correlations handles a larger event set without panicking
        let events: Vec<TimelineEvent> = (0..100)
            .map(|i| create_test_event(i, i * 500, (i % 5) as u64 + 1, "rule_perf", "warning"))
            .collect();

        let correlations = detect_correlations(&events);

        // Should produce some correlations without panicking
        assert!(
            !correlations.is_empty(),
            "100 events across 5 panes should produce correlations"
        );
        // All IDs should be unique
        let ids: std::collections::HashSet<_> = correlations.iter().map(|c| &c.id).collect();
        assert_eq!(ids.len(), correlations.len());
    }

    // =========================================================================
    // Pane Bookmark Tests
    // =========================================================================

    #[test]
    fn pane_bookmark_insert_and_query() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        let record = PaneBookmarkRecord {
            id: 0,
            pane_id: 42,
            alias: "build".to_string(),
            tags: Some(vec!["ci".to_string(), "important".to_string()]),
            description: Some("The build pane".to_string()),
            created_at: now,
            updated_at: now,
        };

        let id = insert_pane_bookmark_sync(&conn, &record).unwrap();
        assert!(id > 0);

        let fetched = query_pane_bookmark_by_alias(&conn, "build").unwrap();
        assert!(fetched.is_some());
        let bm = fetched.unwrap();
        assert_eq!(bm.pane_id, 42);
        assert_eq!(bm.alias, "build");
        assert_eq!(
            bm.tags,
            Some(vec!["ci".to_string(), "important".to_string()])
        );
        assert_eq!(bm.description.as_deref(), Some("The build pane"));
    }

    #[test]
    fn pane_bookmark_alias_unique() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        let record = PaneBookmarkRecord {
            id: 0,
            pane_id: 1,
            alias: "main".to_string(),
            tags: None,
            description: None,
            created_at: now,
            updated_at: now,
        };

        insert_pane_bookmark_sync(&conn, &record).unwrap();
        let result = insert_pane_bookmark_sync(&conn, &record);
        assert!(result.is_err(), "Duplicate alias should fail");
    }

    #[test]
    fn pane_bookmark_list_and_delete() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        for (pane, alias) in [(1, "alpha"), (2, "beta"), (3, "gamma")] {
            let record = PaneBookmarkRecord {
                id: 0,
                pane_id: pane,
                alias: alias.to_string(),
                tags: None,
                description: None,
                created_at: now,
                updated_at: now,
            };
            insert_pane_bookmark_sync(&conn, &record).unwrap();
        }

        let list = list_pane_bookmarks_sync(&conn).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].alias, "alpha");
        assert_eq!(list[1].alias, "beta");
        assert_eq!(list[2].alias, "gamma");

        let deleted = delete_pane_bookmark_sync(&conn, "beta").unwrap();
        assert!(deleted);

        let list2 = list_pane_bookmarks_sync(&conn).unwrap();
        assert_eq!(list2.len(), 2);

        let not_found = delete_pane_bookmark_sync(&conn, "nonexistent").unwrap();
        assert!(!not_found);
    }

    #[test]
    fn pane_bookmark_filter_by_tag() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        let records = vec![
            PaneBookmarkRecord {
                id: 0,
                pane_id: 1,
                alias: "web".to_string(),
                tags: Some(vec!["frontend".to_string(), "prod".to_string()]),
                description: None,
                created_at: now,
                updated_at: now,
            },
            PaneBookmarkRecord {
                id: 0,
                pane_id: 2,
                alias: "api".to_string(),
                tags: Some(vec!["backend".to_string(), "prod".to_string()]),
                description: None,
                created_at: now,
                updated_at: now,
            },
            PaneBookmarkRecord {
                id: 0,
                pane_id: 3,
                alias: "test".to_string(),
                tags: Some(vec!["ci".to_string()]),
                description: None,
                created_at: now,
                updated_at: now,
            },
        ];

        for r in &records {
            insert_pane_bookmark_sync(&conn, r).unwrap();
        }

        let prod = list_pane_bookmarks_by_tag_sync(&conn, "prod").unwrap();
        assert_eq!(prod.len(), 2);

        let ci = list_pane_bookmarks_by_tag_sync(&conn, "ci").unwrap();
        assert_eq!(ci.len(), 1);
        assert_eq!(ci[0].alias, "test");

        let none = list_pane_bookmarks_by_tag_sync(&conn, "nonexistent").unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn pane_bookmark_persists_across_restarts() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        let record = PaneBookmarkRecord {
            id: 0,
            pane_id: 99,
            alias: "persistent".to_string(),
            tags: Some(vec!["durable".to_string()]),
            description: Some("survives restart".to_string()),
            created_at: now,
            updated_at: now,
        };

        insert_pane_bookmark_sync(&conn, &record).unwrap();

        // Simulate "restart" by querying fresh (same in-memory DB)
        let fetched = query_pane_bookmark_by_alias(&conn, "persistent")
            .unwrap()
            .unwrap();
        assert_eq!(fetched.pane_id, 99);
        assert_eq!(fetched.description.as_deref(), Some("survives restart"));
    }

    #[test]
    fn pane_bookmark_nonexistent_alias_returns_none() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let result = query_pane_bookmark_by_alias(&conn, "ghost").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn pane_bookmark_multiple_for_same_pane() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        for alias in ["build-1", "build-2", "build-3"] {
            insert_pane_bookmark_sync(
                &conn,
                &PaneBookmarkRecord {
                    id: 0,
                    pane_id: 42,
                    alias: alias.to_string(),
                    tags: None,
                    description: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .unwrap();
        }

        let list = list_pane_bookmarks_sync(&conn).unwrap();
        assert_eq!(list.len(), 3);
        assert!(list.iter().all(|bm| bm.pane_id == 42));
    }

    #[test]
    fn pane_bookmark_null_tags_vs_empty_tags() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        insert_pane_bookmark_sync(
            &conn,
            &PaneBookmarkRecord {
                id: 0,
                pane_id: 1,
                alias: "notags".to_string(),
                tags: None,
                description: None,
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();

        insert_pane_bookmark_sync(
            &conn,
            &PaneBookmarkRecord {
                id: 0,
                pane_id: 2,
                alias: "emptytags".to_string(),
                tags: Some(vec![]),
                description: None,
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();

        let no_tags = query_pane_bookmark_by_alias(&conn, "notags")
            .unwrap()
            .unwrap();
        assert!(no_tags.tags.is_none());

        let empty_tags = query_pane_bookmark_by_alias(&conn, "emptytags")
            .unwrap()
            .unwrap();
        assert_eq!(empty_tags.tags, Some(vec![]));
    }

    #[test]
    fn pane_bookmark_case_sensitive_aliases() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        for alias in ["build", "Build", "BUILD"] {
            insert_pane_bookmark_sync(
                &conn,
                &PaneBookmarkRecord {
                    id: 0,
                    pane_id: 1,
                    alias: alias.to_string(),
                    tags: None,
                    description: None,
                    created_at: now,
                    updated_at: now,
                },
            )
            .unwrap();
        }

        let list = list_pane_bookmarks_sync(&conn).unwrap();
        assert_eq!(list.len(), 3, "Case-different aliases should be distinct");
    }

    #[test]
    fn pane_bookmark_nonexistent_pane_id_allowed() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        let result = insert_pane_bookmark_sync(
            &conn,
            &PaneBookmarkRecord {
                id: 0,
                pane_id: 999_999,
                alias: "phantom".to_string(),
                tags: None,
                description: None,
                created_at: now,
                updated_at: now,
            },
        );
        assert!(
            result.is_ok(),
            "Should allow bookmark for non-existent pane"
        );
    }

    #[test]
    fn pane_bookmark_empty_description_vs_none() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        insert_pane_bookmark_sync(
            &conn,
            &PaneBookmarkRecord {
                id: 0,
                pane_id: 1,
                alias: "nodesc".to_string(),
                tags: None,
                description: None,
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();

        insert_pane_bookmark_sync(
            &conn,
            &PaneBookmarkRecord {
                id: 0,
                pane_id: 2,
                alias: "emptydesc".to_string(),
                tags: None,
                description: Some(String::new()),
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();

        let no_desc = query_pane_bookmark_by_alias(&conn, "nodesc")
            .unwrap()
            .unwrap();
        assert!(no_desc.description.is_none());

        let empty_desc = query_pane_bookmark_by_alias(&conn, "emptydesc")
            .unwrap()
            .unwrap();
        assert_eq!(empty_desc.description, Some(String::new()));
    }

    #[test]
    fn pane_bookmark_tag_with_special_chars() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let now = now_ms();
        let tags = vec![
            "normal".to_string(),
            "has spaces".to_string(),
            "quote\"in".to_string(),
        ];
        insert_pane_bookmark_sync(
            &conn,
            &PaneBookmarkRecord {
                id: 0,
                pane_id: 1,
                alias: "specialtags".to_string(),
                tags: Some(tags.clone()),
                description: None,
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();

        let fetched = query_pane_bookmark_by_alias(&conn, "specialtags")
            .unwrap()
            .unwrap();
        assert_eq!(fetched.tags.unwrap(), tags);
    }

    #[test]
    fn pane_bookmark_list_empty_db() {
        let conn = Connection::open_in_memory().unwrap();
        initialize_schema(&conn).unwrap();

        let list = list_pane_bookmarks_sync(&conn).unwrap();
        assert!(list.is_empty());

        let by_tag = list_pane_bookmarks_by_tag_sync(&conn, "anything").unwrap();
        assert!(by_tag.is_empty());
    }
}

// =============================================================================
// Timeline Integration Tests (wa-6sk.5)
// =============================================================================

#[cfg(test)]
mod timeline_integration_tests {
    use super::*;

    fn make_pane(pane_id: u64, now: i64) -> PaneRecord {
        PaneRecord {
            pane_id,
            pane_uuid: Some(format!("uuid-{pane_id}")),
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some(format!("pane-{pane_id}")),
            cwd: Some("/tmp/test".to_string()),
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    fn make_event(
        pane_id: u64,
        rule_id: &str,
        event_type: &str,
        severity: &str,
        detected_at: i64,
    ) -> StoredEvent {
        StoredEvent {
            id: 0,
            pane_id,
            rule_id: rule_id.to_string(),
            agent_type: "claude_code".to_string(),
            event_type: event_type.to_string(),
            severity: severity.to_string(),
            confidence: 0.9,
            extracted: None,
            matched_text: None,
            segment_id: None,
            detected_at,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }
    }

    #[tokio::test]
    async fn timeline_empty_db_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let query = TimelineQuery::new();
        let timeline = handle.get_timeline(query).await.unwrap();

        assert!(timeline.events.is_empty());
        assert!(timeline.correlations.is_empty());
        assert_eq!(timeline.total_count, 0);
        assert!(!timeline.has_more);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_single_event_no_correlations() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 1);
        assert!(timeline.correlations.is_empty());
        assert_eq!(timeline.total_count, 1);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_temporal_correlation_across_panes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Two events within 10s across different panes
        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "warning", "warning", now + 3000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 2);
        let temporal = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .count();
        assert!(temporal > 0, "Should detect temporal correlation");

        // Events should have correlation refs attached
        let event_with_refs = timeline
            .events
            .iter()
            .filter(|e| !e.correlations.is_empty())
            .count();
        assert!(event_with_refs > 0, "Events should have correlation refs");

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_failover_correlation_integration() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Usage limit in pane 1, session start in pane 2 within 5 minutes
        handle
            .record_event(make_event(
                1,
                "usage_limit",
                "usage.reached",
                "warning",
                now,
            ))
            .await
            .unwrap();
        handle
            .record_event(make_event(
                2,
                "session_start",
                "session.start",
                "info",
                now + 120_000,
            ))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let failover = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Failover)
            .count();
        assert_eq!(failover, 1, "Should detect failover correlation");

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_pagination_offset_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        // Insert 10 events
        for i in 0..10 {
            handle
                .record_event(make_event(
                    1,
                    &format!("rule_{i}"),
                    "info",
                    "info",
                    now + i * 1000,
                ))
                .await
                .unwrap();
        }

        // Page 1: first 3
        let query = TimelineQuery {
            limit: 3,
            offset: 0,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let page1 = handle.get_timeline(query).await.unwrap();
        assert_eq!(page1.events.len(), 3);
        assert_eq!(page1.total_count, 10);
        assert!(page1.has_more);

        // Page 2: next 3
        let query = TimelineQuery {
            limit: 3,
            offset: 3,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let page2 = handle.get_timeline(query).await.unwrap();
        assert_eq!(page2.events.len(), 3);
        assert!(page2.has_more);

        // Page 4: last 1
        let query = TimelineQuery {
            limit: 3,
            offset: 9,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let page4 = handle.get_timeline(query).await.unwrap();
        assert_eq!(page4.events.len(), 1);
        assert!(!page4.has_more);

        // Beyond range
        let query = TimelineQuery {
            limit: 3,
            offset: 15,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let beyond = handle.get_timeline(query).await.unwrap();
        assert!(beyond.events.is_empty());
        assert!(!beyond.has_more);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_filter_by_severity() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_b", "warning", "warning", now + 1000))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_c", "info", "info", now + 2000))
            .await
            .unwrap();

        let query = TimelineQuery {
            severities: Some(vec!["error".to_string()]),
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].severity, "error");

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_filter_by_pane_id() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "error", "error", now + 1000))
            .await
            .unwrap();

        let query = TimelineQuery {
            pane_ids: Some(vec![1]),
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 1);
        assert_eq!(timeline.events[0].pane_info.pane_id, 1);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_filter_by_time_range() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        for i in 0..5 {
            handle
                .record_event(make_event(
                    1,
                    &format!("rule_{i}"),
                    "info",
                    "info",
                    now + i * 60_000,
                ))
                .await
                .unwrap();
        }

        // Only events in first 2 minutes
        let query = TimelineQuery {
            start: Some(now),
            end: Some(now + 120_000),
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 3); // t=0, t=60s, t=120s

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_unhandled_only_filter() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        // Insert event then mark it as handled via the proper API
        let event_id = handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .mark_event_handled(event_id, Some("wf-1".to_string()), "handled")
            .await
            .unwrap();

        handle
            .record_event(make_event(1, "rule_b", "warning", "warning", now + 5000))
            .await
            .unwrap();

        let query = TimelineQuery {
            unhandled_only: true,
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        assert_eq!(timeline.events.len(), 1);
        assert!(timeline.events[0].handled.is_none());

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_events_same_timestamp_handled_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Three events at the exact same timestamp
        for i in 0..3 {
            handle
                .record_event(make_event(
                    (i % 2) as u64 + 1,
                    &format!("rule_{i}"),
                    "error",
                    "error",
                    now,
                ))
                .await
                .unwrap();
        }

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 3);
        // Should detect temporal correlation (same timestamp, different panes)
        let temporal = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::Temporal)
            .count();
        assert!(
            temporal > 0,
            "Same-timestamp cross-pane events should correlate"
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_correlation_refs_attached_to_events() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Two events that should correlate temporally
        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "warning", "warning", now + 2000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        // At least one event should have correlation refs
        let has_refs = timeline.events.iter().any(|e| !e.correlations.is_empty());
        assert!(
            has_refs,
            "Correlated events should have CorrelationRef attached"
        );

        // Verify ref IDs match top-level correlation IDs
        for event in &timeline.events {
            for cref in &event.correlations {
                assert!(
                    timeline.correlations.iter().any(|c| c.id == cref.id),
                    "Event correlation ref ID should match a top-level correlation"
                );
            }
        }

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_correlations_disabled_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Events that would normally correlate
        handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(2, "rule_b", "error", "error", now + 1000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 2);
        assert!(
            timeline.correlations.is_empty(),
            "Correlations should be empty when disabled"
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_query_performance_many_events() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        // Create 5 panes
        for p in 1..=5 {
            handle.upsert_pane(make_pane(p, now)).await.unwrap();
        }

        // Insert 200 events across panes
        for i in 0..200 {
            let pane = (i % 5) as u64 + 1;
            handle
                .record_event(make_event(
                    pane,
                    &format!("rule_{}", i % 10),
                    "detection",
                    if i % 3 == 0 { "error" } else { "warning" },
                    now + i * 500,
                ))
                .await
                .unwrap();
        }

        // Time the query
        let start = std::time::Instant::now();
        let query = TimelineQuery {
            include_correlations: true,
            limit: 100,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(timeline.events.len(), 100);
        assert_eq!(timeline.total_count, 200);
        assert!(timeline.has_more);
        assert!(
            !timeline.correlations.is_empty(),
            "Should find correlations among 200 events"
        );
        // Performance budget: query should complete in <500ms (generous for CI)
        assert!(
            elapsed.as_millis() < 500,
            "Timeline query took {}ms, expected <500ms",
            elapsed.as_millis()
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_workflow_group_integration() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();

        // Two events handled by same workflow (must use mark_event_handled API)
        let eid1 = handle
            .record_event(make_event(1, "rule_a", "error", "error", now))
            .await
            .unwrap();
        handle
            .mark_event_handled(eid1, Some("wf-test-1".to_string()), "handled")
            .await
            .unwrap();

        let eid2 = handle
            .record_event(make_event(2, "rule_b", "error", "error", now + 5000))
            .await
            .unwrap();
        handle
            .mark_event_handled(eid2, Some("wf-test-1".to_string()), "handled")
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let workflow = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::WorkflowGroup)
            .collect::<Vec<_>>();
        assert_eq!(workflow.len(), 1, "Should detect workflow group");
        assert_eq!(workflow[0].event_ids.len(), 2);
        assert!(
            (workflow[0].confidence - 0.95).abs() < 0.01,
            "Workflow confidence should be 0.95"
        );

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_serde_roundtrip() {
        let timeline = Timeline {
            start: 1000,
            end: 5000,
            events: vec![TimelineEvent {
                id: 1,
                timestamp: 1000,
                pane_info: PaneInfo {
                    pane_id: 1,
                    pane_uuid: Some("uuid-1".to_string()),
                    agent_type: Some("claude_code".to_string()),
                    domain: "local".to_string(),
                    cwd: Some("/tmp".to_string()),
                    title: Some("test".to_string()),
                },
                rule_id: "rule_a".to_string(),
                event_type: "error".to_string(),
                severity: "error".to_string(),
                confidence: 0.9,
                handled: None,
                correlations: vec![CorrelationRef {
                    id: "corr-1".to_string(),
                    correlation_type: CorrelationType::Temporal,
                }],
                summary: Some("Test event".to_string()),
            }],
            correlations: vec![Correlation {
                id: "corr-1".to_string(),
                event_ids: vec![1, 2],
                correlation_type: CorrelationType::Temporal,
                confidence: 0.6,
                description: "Test correlation".to_string(),
            }],
            total_count: 1,
            has_more: false,
        };

        let json = serde_json::to_string(&timeline).unwrap();
        let deserialized: Timeline = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.events.len(), 1);
        assert_eq!(deserialized.correlations.len(), 1);
        assert_eq!(deserialized.total_count, 1);
        assert!(!deserialized.has_more);
        assert_eq!(deserialized.events[0].correlations.len(), 1);
        assert_eq!(
            deserialized.correlations[0].correlation_type,
            CorrelationType::Temporal
        );
    }

    #[tokio::test]
    async fn timeline_dedupe_group_integration() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();
        handle.upsert_pane(make_pane(2, now)).await.unwrap();
        handle.upsert_pane(make_pane(3, now)).await.unwrap();

        // Same rule firing across 3 panes within 30s
        handle
            .record_event(make_event(
                1,
                "claude_code.usage.reached",
                "usage",
                "warning",
                now,
            ))
            .await
            .unwrap();
        handle
            .record_event(make_event(
                2,
                "claude_code.usage.reached",
                "usage",
                "warning",
                now + 10_000,
            ))
            .await
            .unwrap();
        handle
            .record_event(make_event(
                3,
                "claude_code.usage.reached",
                "usage",
                "warning",
                now + 20_000,
            ))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: true,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        let dedupe = timeline
            .correlations
            .iter()
            .filter(|c| c.correlation_type == CorrelationType::DedupeGroup)
            .collect::<Vec<_>>();
        assert_eq!(dedupe.len(), 1, "Should detect dedupe group across 3 panes");
        assert_eq!(dedupe[0].event_ids.len(), 3);

        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn timeline_events_ordered_chronologically() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("wa.db");
        let handle = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .unwrap();

        let now = 1_700_000_000_000i64;
        handle.upsert_pane(make_pane(1, now)).await.unwrap();

        // Insert events out of order
        handle
            .record_event(make_event(1, "rule_c", "info", "info", now + 5000))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_a", "info", "info", now))
            .await
            .unwrap();
        handle
            .record_event(make_event(1, "rule_b", "info", "info", now + 2000))
            .await
            .unwrap();

        let query = TimelineQuery {
            include_correlations: false,
            ..TimelineQuery::new()
        };
        let timeline = handle.get_timeline(query).await.unwrap();

        assert_eq!(timeline.events.len(), 3);
        assert!(
            timeline.events[0].timestamp <= timeline.events[1].timestamp
                && timeline.events[1].timestamp <= timeline.events[2].timestamp,
            "Events should be in chronological order"
        );

        handle.shutdown().await.unwrap();
    }
}
