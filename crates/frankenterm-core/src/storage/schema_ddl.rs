//! Schema Definition (DDL strings & version constant)
//!
//! [ft-6qkx1 / ft-dn2tu Phase 2] Extracted from `storage.rs` so the
//! initial DDL surface lives in a focused, append-only file instead of
//! a 30k-line mega-module. Re-exported via `pub use schema_ddl::{...}`
//! in `storage.rs` so existing call sites in
//! `frankenterm_core::storage::SCHEMA_VERSION` / `SCHEMA_SQL` and the
//! sibling modules need no edits.
//!
//! Constants exported:
//! - [`SCHEMA_VERSION`] — current target schema version (PRAGMA user_version).
//! - [`SCHEMA_SQL`] — full DDL bundle applied for fresh DB initialization.
//! - [`FTS_TRIGGER_RECREATE_SQL`] — `pub(crate)` idempotent FTS-trigger
//!   re-creation, used by `defer_fts_triggers: false` open paths in
//!   `storage.rs` and test scaffolding.
//!
//! Pure data: no impls, no helpers — moves are mechanical.

// =============================================================================
// Schema Definition
// =============================================================================

/// Current schema version for migration tracking.
///
/// This is the target version that new databases will be initialized to,
/// and existing databases will be migrated to.
/// Uses SQLite's PRAGMA user_version for atomic version tracking.
///
/// Per ft-4yr9i: bumped 24 → 25 to gate the agent_profiles table
/// + role index from agent_profiles.rs (ft-df3cz substrate). The
/// migration entry sits at MIGRATIONS[24] in
/// storage/migrations.rs.
///
/// Per br-ft-4iz0q substrate-pass: bumped 25 → 26 to gate the
/// `profiles_applied_log` table the daemon-side
/// `RobotProfile.apply` handler writes idempotency receipts
/// into. The migration entry sits at MIGRATIONS[25] in
/// storage/migrations.rs; the receipt schema mirrors the
/// `ApplyReceipt` substrate type at
/// crates/frankenterm-core/src/robot_profile_handler.rs.
pub const SCHEMA_VERSION: i32 = 26;

/// Schema initialization SQL
///
/// Convention notes:
/// - Timestamps: epoch milliseconds (i64) for hot-path queries
/// - JSON columns: TEXT containing JSON (v0 simplicity)
/// - All tables use INTEGER PRIMARY KEY for rowid aliasing

/// [ft-ih4tm] Idempotent re-creation of the three `output_segments` FTS
/// triggers. Called when a database is opened with
/// `defer_fts_triggers: false` so the flag is truly reversible — without
/// this, a DB opened once with `true` stays in deferred mode forever
/// because `initialize_schema` short-circuits for up-to-date schemas and
/// `CREATE TRIGGER IF NOT EXISTS` from the main `SCHEMA_SQL` never re-runs.
///
/// KEEP IN SYNC with the `CREATE TRIGGER IF NOT EXISTS output_segments_*`
/// block inside `SCHEMA_SQL` below — if a trigger body changes here,
/// change it there too and vice versa.
pub(crate) const FTS_TRIGGER_RECREATE_SQL: &str = r#"
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
"#;

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

-- ft metadata: version compatibility + provenance
CREATE TABLE IF NOT EXISTS ft_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version INTEGER NOT NULL,
    min_compatible_ft TEXT NOT NULL,
    created_by_ft TEXT NOT NULL,
    created_at INTEGER NOT NULL  -- epoch ms
);

-- Panes: metadata and observation decisions
-- Supports: ft status, ft robot state, privacy/perf filtering
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

-- Segment embeddings for semantic search
CREATE TABLE IF NOT EXISTS segment_embeddings (
    segment_id INTEGER NOT NULL REFERENCES output_segments(id) ON DELETE CASCADE,
    embedder_id TEXT NOT NULL,
    dimension INTEGER NOT NULL,
    vector BLOB NOT NULL,
    embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    PRIMARY KEY (segment_id, embedder_id)
);

CREATE INDEX IF NOT EXISTS idx_segment_embeddings_embedder
    ON segment_embeddings(embedder_id);

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
    created_at INTEGER NOT NULL          -- epoch ms
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

-- Mux sessions: top-level session tracking (one per watcher invocation)
CREATE TABLE IF NOT EXISTS mux_sessions (
    session_id TEXT PRIMARY KEY,           -- UUID v7 for time-ordering
    created_at INTEGER NOT NULL,           -- epoch ms
    last_checkpoint_at INTEGER,            -- epoch ms
    shutdown_clean INTEGER NOT NULL DEFAULT 0,  -- 1 = graceful, 0 = crash/power loss
    topology_json TEXT NOT NULL,           -- serialized tab/split tree
    window_metadata_json TEXT,             -- window size, title, position
    ft_version TEXT NOT NULL,              -- binary version at creation
    host_id TEXT                           -- hostname + boot_id for multi-host disambiguation
);

-- Session checkpoints: individual checkpoint snapshots (many per session)
CREATE TABLE IF NOT EXISTS session_checkpoints (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
    checkpoint_at INTEGER NOT NULL,        -- epoch ms
    checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
    state_hash TEXT NOT NULL,              -- [ft-ybtyg] SipHash-24 (16-hex-char u64) over the serialized state/inputs; used for dedup-skip + restore-path state witness. Not a cryptographic integrity hash.
    pane_count INTEGER NOT NULL,
    total_bytes INTEGER NOT NULL,          -- serialized size for budget tracking
    metadata_json TEXT                     -- trigger reason for 'event' type
);

CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);

-- Mux pane state: per-pane state snapshot, linked to a checkpoint
CREATE TABLE IF NOT EXISTS mux_pane_state (
    id INTEGER PRIMARY KEY,
    checkpoint_id INTEGER NOT NULL REFERENCES session_checkpoints(id) ON DELETE CASCADE,
    pane_id INTEGER NOT NULL,              -- WezTerm pane ID at capture time
    cwd TEXT,
    command TEXT,                           -- best-effort process name
    env_json TEXT,                          -- selected env vars (redacted)
    terminal_state_json TEXT NOT NULL,      -- cursor pos, attributes, alt-screen, scrollback ref
    agent_metadata_json TEXT,               -- agent type, session ID, state
    scrollback_checkpoint_seq INTEGER,      -- links to output_segments.seq for replay
    last_output_at INTEGER                 -- epoch ms of last captured output
);

CREATE INDEX IF NOT EXISTS idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
CREATE INDEX IF NOT EXISTS idx_pane_state_pane ON mux_pane_state(pane_id);

-- Action history view (audit + undo + workflow step info)
CREATE VIEW IF NOT EXISTS action_history AS
SELECT a.*,
       u.undoable, u.undo_strategy, u.undo_hint, u.undone_at, u.undone_by,
       w.workflow_id, w.step_name
FROM audit_actions a
LEFT JOIN action_undo u ON u.audit_action_id = a.id
LEFT JOIN workflow_step_logs w ON w.audit_action_id = a.id;
"#;
