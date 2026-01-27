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
//! - `audit_actions`: Audit trail for policy decisions and outcomes
//! - `action_undo`: Undo metadata for audit actions
//! - `action_history`: View joining audit + undo + workflow step info
//! - `approval_tokens`: Allow-once approvals scoped to actions
//! - `config`: Key-value settings
//! - `maintenance_log`: System events and metrics
//!
//! FTS5 virtual table `output_segments_fts` enables full-text search.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use rusqlite::{
    Connection, OptionalExtension, params,
    types::{Type, Value as SqlValue},
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Result, StorageError};
use crate::policy::Redactor;

// =============================================================================
// Schema Definition
// =============================================================================

/// Current schema version for migration tracking.
///
/// This is the target version that new databases will be initialized to,
/// and existing databases will be migrated to.
/// Uses SQLite's PRAGMA user_version for atomic version tracking.
pub const SCHEMA_VERSION: i32 = 6;

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

    -- Idempotency: optional dedupe key (pane_id + rule_id + time_window)
    dedupe_key TEXT,                  -- computed key for duplicate prevention

    UNIQUE(dedupe_key)                -- prevents duplicate events when dedupe_key set
);

CREATE INDEX IF NOT EXISTS idx_events_pane ON events(pane_id);
CREATE INDEX IF NOT EXISTS idx_events_rule ON events(rule_id);
CREATE INDEX IF NOT EXISTS idx_events_unhandled ON events(handled_at) WHERE handled_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_events_detected ON events(detected_at);
CREATE INDEX IF NOT EXISTS idx_events_severity ON events(severity, detected_at);

-- Agent sessions: per-agent session timeline with token tracking
CREATE TABLE IF NOT EXISTS agent_sessions (
    id INTEGER PRIMARY KEY,
    pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    agent_type TEXT NOT NULL,         -- codex, claude_code, gemini, unknown
    session_id TEXT,                  -- Agent's internal session ID if available
    external_id TEXT,                 -- Correlation with cass, etc.
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
    result_type TEXT NOT NULL,        -- continue, done, retry, abort, wait_for
    result_data TEXT,                 -- JSON: result payload
    started_at INTEGER NOT NULL,      -- epoch ms
    completed_at INTEGER NOT NULL,    -- epoch ms
    duration_ms INTEGER NOT NULL      -- cached for stats
);

CREATE INDEX IF NOT EXISTS idx_step_logs_workflow ON workflow_step_logs(workflow_id, step_index);
CREATE INDEX IF NOT EXISTS idx_step_logs_audit_action ON workflow_step_logs(audit_action_id);

-- Audit actions: policy decisions and outcomes
CREATE TABLE IF NOT EXISTS audit_actions (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,               -- epoch ms
    actor_kind TEXT NOT NULL,          -- human, robot, mcp, workflow
    actor_id TEXT,                     -- optional (workflow execution id, MCP client id)
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
    action_fingerprint TEXT NOT NULL   -- normalized action fingerprint
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

-- Config: key-value settings
CREATE TABLE IF NOT EXISTS config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,              -- JSON value
    updated_at INTEGER NOT NULL       -- epoch ms
);

-- Maintenance log: system events and metrics
CREATE TABLE IF NOT EXISTS maintenance_log (
    id INTEGER PRIMARY KEY,
    event_type TEXT NOT NULL,         -- startup, shutdown, vacuum, retention_cleanup, error
    message TEXT,
    metadata TEXT,                    -- JSON: additional context
    timestamp INTEGER NOT NULL        -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_maintenance_timestamp ON maintenance_log(timestamp);

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
    /// When handled (epoch ms, None = unhandled)
    pub handled_at: Option<i64>,
    /// Workflow that handled this
    pub handled_by_workflow_id: Option<String>,
    /// Handling status
    pub handled_status: Option<String>,
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
    /// Result type (continue, done, retry, abort, wait_for)
    pub result_type: String,
    /// Result data (JSON)
    pub result_data: Option<String>,
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
}

impl ApprovalTokenRecord {
    /// Returns true if the token is unused and unexpired
    #[must_use]
    pub fn is_active(&self, now_ms: i64) -> bool {
        self.used_at.is_none() && self.expires_at >= now_ms
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
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

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

    // Existing database at version 0: ensure base schema exists, then migrate.
    if current == 0 {
        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| StorageError::MigrationFailed(format!("Schema init failed: {e}")))?;
    }

    // Apply pending migrations for existing databases
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
        let core = input
            .split(|c| c == '-' || c == '+')
            .next()
            .unwrap_or_default();
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

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get(0),
        )
        .map_err(|e| StorageError::Database(e.to_string()))?;
    Ok(exists > 0)
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
    let current_wa = crate::VERSION.to_string();

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
                    min_compatible = current_wa.clone();
                }
            } else if meta.min_compatible_wa != current_wa {
                min_compatible = current_wa.clone();
            }

            let created_by = if meta.created_by_wa.is_empty() {
                current_wa.clone()
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
        respond: oneshot::Sender<Result<Gap>>,
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
    /// Insert a workflow step log
    InsertStepLog {
        workflow_id: String,
        audit_action_id: Option<i64>,
        step_index: usize,
        step_name: String,
        result_type: String,
        result_data: Option<String>,
        started_at: i64,
        completed_at: i64,
        respond: oneshot::Sender<Result<()>>,
    },
    /// Upsert undo metadata for an audit action
    UpsertActionUndo {
        record: ActionUndoRecord,
        respond: oneshot::Sender<Result<()>>,
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
    pub async fn record_gap(&self, pane_id: u64, reason: &str) -> Result<Gap> {
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

    /// Insert a workflow step log entry
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_step_log(
        &self,
        workflow_id: &str,
        audit_action_id: Option<i64>,
        step_index: usize,
        step_name: &str,
        result_type: &str,
        result_data: Option<String>,
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
                result_type: result_type.to_string(),
                result_data,
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

/// Query options for action history view
#[derive(Debug, Clone, Default)]
pub struct ActionHistoryQuery {
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
    /// Only return unhandled events
    pub unhandled_only: bool,
    /// Filter by time range start (epoch ms)
    pub since: Option<i64>,
    /// Filter by time range end (epoch ms)
    pub until: Option<i64>,
}

// =============================================================================
// Writer Thread Implementation
// =============================================================================

/// Main loop for the writer thread
fn writer_loop(conn: &mut Connection, rx: &mut mpsc::Receiver<WriteCommand>) {
    // Use blocking_recv from sync context
    while let Some(cmd) = rx.blocking_recv() {
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
                let result =
                    mark_event_handled_sync(conn, event_id, workflow_id.as_deref(), &status);
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
            WriteCommand::InsertStepLog {
                workflow_id,
                audit_action_id,
                step_index,
                step_name,
                result_type,
                result_data,
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
                    &result_type,
                    result_data.as_deref(),
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
            WriteCommand::PruneSegments { before_ts, respond } => {
                let result = prune_segments_sync(conn, before_ts);
                let _ = respond.send(result);
            }
            WriteCommand::Vacuum { respond } => {
                let result = vacuum_sync(conn);
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
                let result =
                    update_account_last_used_sync(conn, &service, &account_id, last_used_at);
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
            WriteCommand::Shutdown { respond } => {
                // Acknowledge shutdown
                let _ = respond.send(());
                break;
            }
        }
    }
}

// =============================================================================
// Synchronous Database Operations
// =============================================================================

/// Get current timestamp in epoch milliseconds
fn now_ms() -> i64 {
    #[allow(clippy::cast_possible_truncation)]
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    usize::try_from(value).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0, // Column index - we don't have the actual column in this context
            Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("value {value} out of usize range"),
            )),
        )
    })
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
fn record_gap_sync(conn: &Connection, pane_id: u64, reason: &str) -> Result<Gap> {
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

    let seq_before = last_seq.unwrap_or(0);
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

    Ok(Gap {
        id,
        pane_id,
        seq_before,
        seq_after,
        reason: reason.to_string(),
        detected_at: now,
    })
}

/// Record an event (synchronous)
fn record_event_sync(conn: &Connection, event: &StoredEvent) -> Result<i64> {
    let extracted_json = event
        .extracted
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());

    let pane_id_i64 = u64_to_i64(event.pane_id, "pane_id")?;

    conn.execute(
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
            None::<String>, // dedupe_key - can be computed if needed
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert event: {e}")))?;

    Ok(conn.last_insert_rowid())
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

/// Insert workflow step log (synchronous)
#[allow(clippy::too_many_arguments)]
fn insert_step_log_sync(
    conn: &Connection,
    workflow_id: &str,
    audit_action_id: Option<i64>,
    step_index: usize,
    step_name: &str,
    result_type: &str,
    result_data: Option<&str>,
    started_at: i64,
    completed_at: i64,
) -> Result<()> {
    let duration_ms = completed_at.saturating_sub(started_at);
    let step_index_i64 = usize_to_i64(step_index, "step_index")?;

    conn.execute(
        "INSERT INTO workflow_step_logs (workflow_id, audit_action_id, step_index, step_name, result_type,
         result_data, started_at, completed_at, duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            workflow_id,
            audit_action_id,
            step_index_i64,
            step_name,
            result_type,
            result_data,
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

    if session.id == 0 {
        // Insert new session
        conn.execute(
            "INSERT INTO agent_sessions (pane_id, agent_type, session_id, external_id,
             started_at, ended_at, end_reason, total_tokens, input_tokens, output_tokens,
             cached_tokens, reasoning_tokens, model_name, estimated_cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                pane_id_i64,
                session.agent_type,
                session.session_id,
                session.external_id,
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
             pane_id = ?1, agent_type = ?2, session_id = ?3, external_id = ?4,
             started_at = ?5, ended_at = ?6, end_reason = ?7, total_tokens = ?8,
             input_tokens = ?9, output_tokens = ?10, cached_tokens = ?11,
             reasoning_tokens = ?12, model_name = ?13, estimated_cost_usd = ?14
             WHERE id = ?15",
            params![
                pane_id_i64,
                session.agent_type,
                session.session_id,
                session.external_id,
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
        "INSERT INTO audit_actions (ts, actor_kind, actor_id, pane_id, domain, action_kind,
         policy_decision, decision_reason, rule_id, input_summary, verification_summary,
         decision_context, result)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            ts,
            action.actor_kind.as_str(),
            action.actor_id.as_deref(),
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

/// Insert an approval token (synchronous)
fn insert_approval_token_sync(conn: &Connection, token: &ApprovalTokenRecord) -> Result<i64> {
    let pane_id_i64 = token
        .pane_id
        .map(|pane_id| u64_to_i64(pane_id, "pane_id"))
        .transpose()?;

    conn.execute(
        "INSERT INTO approval_tokens (code_hash, created_at, expires_at, used_at, workspace_id,
         action_kind, pane_id, action_fingerprint)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            token.code_hash.as_str(),
            token.created_at,
            token.expires_at,
            token.used_at,
            token.workspace_id.as_str(),
            token.action_kind.as_str(),
            pane_id_i64,
            token.action_fingerprint.as_str(),
        ],
    )
    .map_err(|e| StorageError::Database(format!("Failed to insert approval token: {e}")))?;

    Ok(conn.last_insert_rowid())
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
         pane_id, action_fingerprint
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

    if let Some(pane_id) = pane_id {
        let pane_id_i64 = u64_to_i64(pane_id, "pane_id")?;
        sql.push_str(" AND pane_id = ?");
        params.push(SqlValue::Integer(pane_id_i64));
    } else {
        sql.push_str(" AND pane_id IS NULL");
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
            })
        })
        .optional()
        .map_err(|e| StorageError::Database(format!("Approval query failed: {e}")))?
    };

    if let Some(mut record) = record {
        tx.execute(
            "UPDATE approval_tokens SET used_at = ?1 WHERE id = ?2",
            params![now, record.id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to consume approval token: {e}")))?;
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
               pane_id, action_fingerprint
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
               pane_id, action_fingerprint
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
            })
        })
        .optional()
        .map_err(|e| StorageError::Database(format!("Approval query failed: {e}")))?
    };

    if let Some(mut record) = record {
        tx.execute(
            "UPDATE approval_tokens SET used_at = ?1 WHERE id = ?2",
            params![now, record.id],
        )
        .map_err(|e| StorageError::Database(format!("Failed to consume approval token: {e}")))?;
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

/// Escape a string for safe inclusion in a SQL string literal.
fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
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
    let prefix = escape_sql_literal(options.highlight_prefix.as_deref().unwrap_or(">>>"));
    let suffix = escape_sql_literal(options.highlight_suffix.as_deref().unwrap_or("<<<"));

    // Build query with optional filters
    // FTS5 snippet function: snippet(table, column_idx, prefix, suffix, ellipsis, max_tokens)
    // FTS5 bm25 function: bm25(table) returns negative score (more negative = better match)
    let mut sql = if include_snippets {
        format!(
            "SELECT s.id, s.pane_id, s.seq, s.content, s.content_len, s.content_hash, s.captured_at,
                    snippet(output_segments_fts, 0, '{prefix}', '{suffix}', '...', {max_tokens}) as snippet,
                    highlight(output_segments_fts, 0, '{prefix}', '{suffix}') as highlight,
                    bm25(output_segments_fts) as score
             FROM output_segments s
             JOIN output_segments_fts fts ON s.id = fts.rowid
             WHERE output_segments_fts MATCH ?1"
        )
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

    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query.to_string())];

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

/// Query an agent session by ID
#[allow(clippy::cast_sign_loss)]
fn query_agent_session(conn: &Connection, session_id: i64) -> Result<Option<AgentSessionRecord>> {
    conn.query_row(
        "SELECT id, pane_id, agent_type, session_id, external_id, started_at, ended_at,
         end_reason, total_tokens, input_tokens, output_tokens, cached_tokens,
         reasoning_tokens, model_name, estimated_cost_usd
         FROM agent_sessions WHERE id = ?1",
        [session_id],
        |row| {
            Ok(AgentSessionRecord {
                id: row.get(0)?,
                pane_id: {
                    let v: i64 = row.get(1)?;
                    v as u64
                },
                agent_type: row.get(2)?,
                session_id: row.get(3)?,
                external_id: row.get(4)?,
                started_at: row.get(5)?,
                ended_at: row.get(6)?,
                end_reason: row.get(7)?,
                total_tokens: row.get(8)?,
                input_tokens: row.get(9)?,
                output_tokens: row.get(10)?,
                cached_tokens: row.get(11)?,
                reasoning_tokens: row.get(12)?,
                model_name: row.get(13)?,
                estimated_cost_usd: row.get(14)?,
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
            "SELECT id, pane_id, agent_type, session_id, external_id, started_at, ended_at,
             end_reason, total_tokens, input_tokens, output_tokens, cached_tokens,
             reasoning_tokens, model_name, estimated_cost_usd
             FROM agent_sessions WHERE ended_at IS NULL
             ORDER BY started_at DESC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(AgentSessionRecord {
                id: row.get(0)?,
                pane_id: {
                    let v: i64 = row.get(1)?;
                    v as u64
                },
                agent_type: row.get(2)?,
                session_id: row.get(3)?,
                external_id: row.get(4)?,
                started_at: row.get(5)?,
                ended_at: row.get(6)?,
                end_reason: row.get(7)?,
                total_tokens: row.get(8)?,
                input_tokens: row.get(9)?,
                output_tokens: row.get(10)?,
                cached_tokens: row.get(11)?,
                reasoning_tokens: row.get(12)?,
                model_name: row.get(13)?,
                estimated_cost_usd: row.get(14)?,
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
            "SELECT id, pane_id, agent_type, session_id, external_id, started_at, ended_at,
             end_reason, total_tokens, input_tokens, output_tokens, cached_tokens,
             reasoning_tokens, model_name, estimated_cost_usd
             FROM agent_sessions WHERE pane_id = ?1
             ORDER BY started_at DESC",
        )
        .map_err(|e| StorageError::Database(format!("Failed to prepare query: {e}")))?;

    let rows = stmt
        .query_map([pane_id_i64], |row| {
            Ok(AgentSessionRecord {
                id: row.get(0)?,
                pane_id: {
                    let v: i64 = row.get(1)?;
                    v as u64
                },
                agent_type: row.get(2)?,
                session_id: row.get(3)?,
                external_id: row.get(4)?,
                started_at: row.get(5)?,
                ended_at: row.get(6)?,
                end_reason: row.get(7)?,
                total_tokens: row.get(8)?,
                input_tokens: row.get(9)?,
                output_tokens: row.get(10)?,
                cached_tokens: row.get(11)?,
                reasoning_tokens: row.get(12)?,
                model_name: row.get(13)?,
                estimated_cost_usd: row.get(14)?,
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
             extracted, matched_text, segment_id, detected_at, handled_at,
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
                handled_at: row.get(11)?,
                handled_by_workflow_id: row.get(12)?,
                handled_status: row.get(13)?,
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
         extracted, matched_text, segment_id, detected_at, handled_at,
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
                handled_at: row.get(11)?,
                handled_by_workflow_id: row.get(12)?,
                handled_status: row.get(13)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

/// Query audit actions with optional filters
fn query_audit_actions(conn: &Connection, query: &AuditQuery) -> Result<Vec<AuditActionRecord>> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, pane_id, domain, action_kind, policy_decision,
         decision_reason, rule_id, input_summary, verification_summary, decision_context, result
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
                pane_id: {
                    let val: Option<i64> = row.get(4)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                domain: row.get(5)?,
                action_kind: row.get(6)?,
                policy_decision: row.get(7)?,
                decision_reason: row.get(8)?,
                rule_id: row.get(9)?,
                input_summary: row.get(10)?,
                verification_summary: row.get(11)?,
                decision_context: row.get(12)?,
                result: row.get(13)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Audit query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
}

/// Query action history view with optional filters
fn query_action_history(
    conn: &Connection,
    query: &ActionHistoryQuery,
) -> Result<Vec<ActionHistoryRecord>> {
    let mut sql = String::from(
        "SELECT id, ts, actor_kind, actor_id, pane_id, domain, action_kind, policy_decision,
         decision_reason, rule_id, input_summary, verification_summary, decision_context, result,
         undoable, undo_strategy, undo_hint, undone_at, undone_by, workflow_id, step_name
         FROM action_history WHERE 1=1",
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
                pane_id: {
                    let val: Option<i64> = row.get(4)?;
                    #[allow(clippy::cast_sign_loss)]
                    val.map(|v| v as u64)
                },
                domain: row.get(5)?,
                action_kind: row.get(6)?,
                policy_decision: row.get(7)?,
                decision_reason: row.get(8)?,
                rule_id: row.get(9)?,
                input_summary: row.get(10)?,
                verification_summary: row.get(11)?,
                decision_context: row.get(12)?,
                result: row.get(13)?,
                undoable: {
                    let val: Option<i64> = row.get(14)?;
                    val.map(|v| v != 0)
                },
                undo_strategy: row.get(15)?,
                undo_hint: row.get(16)?,
                undone_at: row.get(17)?,
                undone_by: row.get(18)?,
                workflow_id: row.get(19)?,
                step_name: row.get(20)?,
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
         pane_id, action_fingerprint
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
            })
        },
    );

    match result {
        Ok(record) => Ok(Some(record)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(StorageError::Database(format!("Approval token lookup failed: {e}")).into()),
    }
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
fn query_step_logs(conn: &Connection, workflow_id: &str) -> Result<Vec<WorkflowStepLogRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, workflow_id, audit_action_id, step_index, step_name, result_type, result_data,
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
                result_type: row.get(5)?,
                result_data: row.get(6)?,
                started_at: row.get(7)?,
                completed_at: row.get(8)?,
                duration_ms: row.get(9)?,
            })
        })
        .map_err(|e| StorageError::Database(format!("Query failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| StorageError::Database(format!("Row error: {e}")))?);
    }

    Ok(results)
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
            "workflow_executions",
            "workflow_step_logs",
            "audit_actions",
            "action_undo",
            "approval_tokens",
            "config",
            "maintenance_log",
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
        let gap = record_gap_sync(&conn, 1, "sequence_jump").unwrap();

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
        "continue",
        Some(r#"{"output": "step 1 done"}"#),
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
        "done",
        Some(r#"{"output": "final"}"#),
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
        "continue",
        None, // No result data
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
        result_type: "continue".to_string(),
        result_data: Some(r#"{"status": "ok"}"#.to_string()),
        started_at: 1_700_000_000_000,
        completed_at: 1_700_000_000_100,
        duration_ms: 100,
    };

    let json = serde_json::to_string(&log).unwrap();
    assert!(json.contains("wf-001"));
    assert!(json.contains("init"));
    assert!(json.contains("duration_ms"));
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
            "continue",
            Some(r#"{"async": true}"#.to_string()),
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
                "success",
                Some(r#"{"message":"started"}"#.to_string()),
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
                "success",
                Some(r#"{"chars":42}"#.to_string()),
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
                "success",
                Some(r#"{"matched":true}"#.to_string()),
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
        let gap: Gap = handle.record_gap(1, "connection_lost").await.unwrap();

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
