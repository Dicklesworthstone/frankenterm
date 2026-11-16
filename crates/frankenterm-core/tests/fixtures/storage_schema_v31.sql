-- Historical v31 upgrade fixture, frozen from commit
-- 7cd76e696633b8fe8b75fb82ee7d3600e8a197cb:
-- crates/frankenterm-core/src/storage/schema_ddl.rs SCHEMA_SQL and
-- crates/frankenterm-core/src/storage/migrations.rs migrations 1..31.
-- That runtime initialized SCHEMA_SQL, ran all migrations, then ensured ft_meta.
-- Whitespace/comments are condensed; migration-only tables 24..27 are included.
-- The sole schema variant is the seconds embedded_at default retained by older
-- v22/v23 upgrades: v30 normalized existing values but did not repair the default.
-- Do not regenerate this fixture from the current-head schema.
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;

CREATE TABLE schema_version (version INTEGER NOT NULL, applied_at INTEGER NOT NULL, description TEXT);
CREATE TABLE ft_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1), schema_version INTEGER NOT NULL,
    min_compatible_ft TEXT NOT NULL, created_by_ft TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE panes (
    pane_id INTEGER PRIMARY KEY, pane_uuid TEXT, domain TEXT NOT NULL DEFAULT 'local',
    window_id INTEGER, tab_id INTEGER, title TEXT, cwd TEXT, tty_name TEXT,
    first_seen_at INTEGER NOT NULL, last_seen_at INTEGER NOT NULL,
    observed INTEGER NOT NULL DEFAULT 1, ignore_reason TEXT, last_decision_at INTEGER
);
CREATE INDEX idx_panes_last_seen ON panes(last_seen_at);
CREATE INDEX idx_panes_observed ON panes(observed);
CREATE TABLE output_segments (
    id INTEGER PRIMARY KEY, pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    seq INTEGER NOT NULL, content TEXT NOT NULL, content_len INTEGER NOT NULL,
    content_hash TEXT, captured_at INTEGER NOT NULL, redaction_catalog_version TEXT,
    zone_type TEXT, UNIQUE(pane_id, seq)
);
CREATE INDEX idx_segments_pane_seq ON output_segments(pane_id, seq);
CREATE INDEX idx_segments_captured ON output_segments(captured_at);
CREATE INDEX idx_segments_zone_type ON output_segments(zone_type);
CREATE TABLE segment_embeddings (
    segment_id INTEGER NOT NULL REFERENCES output_segments(id) ON DELETE CASCADE,
    embedder_id TEXT NOT NULL, dimension INTEGER NOT NULL, vector BLOB NOT NULL,
    embedded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    PRIMARY KEY (segment_id, embedder_id)
);
CREATE INDEX idx_segment_embeddings_embedder ON segment_embeddings(embedder_id);
CREATE TABLE output_gaps (
    id INTEGER PRIMARY KEY, pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    seq_before INTEGER NOT NULL, seq_after INTEGER NOT NULL, reason TEXT NOT NULL,
    detected_at INTEGER NOT NULL
);
CREATE INDEX idx_gaps_pane ON output_gaps(pane_id);
CREATE INDEX idx_gaps_detected ON output_gaps(detected_at);
CREATE VIRTUAL TABLE output_segments_fts USING fts5(
    content, content='output_segments', content_rowid='id', tokenize='porter unicode61'
);
CREATE TRIGGER output_segments_ai AFTER INSERT ON output_segments BEGIN
    INSERT INTO output_segments_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TRIGGER output_segments_ad AFTER DELETE ON output_segments BEGIN
    INSERT INTO output_segments_fts(output_segments_fts, rowid, content) VALUES('delete', old.id, old.content);
END;
CREATE TRIGGER output_segments_au AFTER UPDATE ON output_segments BEGIN
    INSERT INTO output_segments_fts(output_segments_fts, rowid, content) VALUES('delete', old.id, old.content);
    INSERT INTO output_segments_fts(rowid, content) VALUES (new.id, new.content);
END;
CREATE TABLE events (
    id INTEGER PRIMARY KEY, pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    rule_id TEXT NOT NULL, agent_type TEXT NOT NULL, event_type TEXT NOT NULL,
    severity TEXT NOT NULL, confidence REAL NOT NULL, extracted TEXT, matched_text TEXT,
    segment_id INTEGER REFERENCES output_segments(id), detected_at INTEGER NOT NULL,
    handled_at INTEGER, handled_by_workflow_id TEXT, handled_status TEXT,
    triage_state TEXT, triage_updated_at INTEGER, triage_updated_by TEXT,
    dedupe_key TEXT, UNIQUE(dedupe_key)
);
CREATE INDEX idx_events_pane ON events(pane_id);
CREATE INDEX idx_events_rule ON events(rule_id);
CREATE INDEX idx_events_unhandled ON events(handled_at) WHERE handled_at IS NULL;
CREATE INDEX idx_events_detected ON events(detected_at);
CREATE INDEX idx_events_severity ON events(severity, detected_at);
CREATE INDEX idx_events_triage_state ON events(triage_state) WHERE triage_state IS NOT NULL;
CREATE TABLE event_labels (
    event_id INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    label TEXT NOT NULL, created_at INTEGER NOT NULL, created_by TEXT, PRIMARY KEY (event_id, label)
);
CREATE INDEX idx_event_labels_event ON event_labels(event_id);
CREATE INDEX idx_event_labels_label ON event_labels(label);
CREATE TABLE event_notes (
    event_id INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
    note TEXT NOT NULL, updated_at INTEGER NOT NULL, updated_by TEXT
);
CREATE INDEX idx_event_notes_updated_at ON event_notes(updated_at);
CREATE TABLE event_mutes (
    identity_key TEXT PRIMARY KEY, scope TEXT NOT NULL DEFAULT 'workspace',
    created_at INTEGER NOT NULL, expires_at INTEGER, created_by TEXT, reason TEXT
);
CREATE INDEX idx_event_mutes_expires ON event_mutes(expires_at) WHERE expires_at IS NOT NULL;
CREATE TABLE agent_sessions (
    id INTEGER PRIMARY KEY, pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    agent_type TEXT NOT NULL, session_id TEXT, external_id TEXT, external_meta TEXT,
    started_at INTEGER NOT NULL, ended_at INTEGER, end_reason TEXT, total_tokens INTEGER,
    input_tokens INTEGER, output_tokens INTEGER, cached_tokens INTEGER, reasoning_tokens INTEGER,
    model_name TEXT, estimated_cost_usd REAL
);
CREATE INDEX idx_sessions_pane ON agent_sessions(pane_id, started_at);
CREATE INDEX idx_sessions_external ON agent_sessions(external_id) WHERE external_id IS NOT NULL;
CREATE INDEX idx_sessions_active ON agent_sessions(ended_at) WHERE ended_at IS NULL;
CREATE TABLE workflow_executions (
    id TEXT PRIMARY KEY, workflow_name TEXT NOT NULL, pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
    trigger_event_id INTEGER REFERENCES events(id), current_step INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'running', wait_condition TEXT, context TEXT, result TEXT, error TEXT,
    started_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, completed_at INTEGER
);
CREATE INDEX idx_workflows_pane ON workflow_executions(pane_id);
CREATE INDEX idx_workflows_status ON workflow_executions(status);
CREATE INDEX idx_workflows_started ON workflow_executions(started_at);
CREATE TABLE workflow_step_logs (
    id INTEGER PRIMARY KEY, workflow_id TEXT NOT NULL REFERENCES workflow_executions(id) ON DELETE CASCADE,
    audit_action_id INTEGER REFERENCES audit_actions(id) ON DELETE SET NULL,
    step_index INTEGER NOT NULL, step_name TEXT NOT NULL, step_id TEXT, step_kind TEXT,
    result_type TEXT NOT NULL, result_data TEXT, policy_summary TEXT, verification_refs TEXT,
    error_code TEXT, started_at INTEGER NOT NULL, completed_at INTEGER NOT NULL, duration_ms INTEGER NOT NULL
);
CREATE INDEX idx_step_logs_workflow ON workflow_step_logs(workflow_id, step_index);
CREATE INDEX idx_step_logs_audit_action ON workflow_step_logs(audit_action_id);
CREATE TABLE workflow_action_plans (
    workflow_id TEXT PRIMARY KEY REFERENCES workflow_executions(id) ON DELETE CASCADE,
    plan_id TEXT NOT NULL, plan_hash TEXT NOT NULL, plan_json TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE INDEX idx_action_plans_hash ON workflow_action_plans(plan_hash);
CREATE TABLE prepared_plans (
    plan_id TEXT PRIMARY KEY, plan_hash TEXT NOT NULL, workspace_id TEXT NOT NULL,
    action_kind TEXT NOT NULL, pane_id INTEGER, pane_uuid TEXT, params_json TEXT,
    plan_json TEXT NOT NULL, requires_approval INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, consumed_at INTEGER
);
CREATE INDEX idx_prepared_plans_hash ON prepared_plans(plan_hash);
CREATE INDEX idx_prepared_plans_workspace ON prepared_plans(workspace_id);
CREATE INDEX idx_prepared_plans_expires ON prepared_plans(expires_at) WHERE consumed_at IS NULL;
CREATE TABLE audit_actions (
    id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, actor_kind TEXT NOT NULL, actor_id TEXT,
    correlation_id TEXT, pane_id INTEGER REFERENCES panes(pane_id) ON DELETE SET NULL,
    domain TEXT, action_kind TEXT NOT NULL, policy_decision TEXT NOT NULL, decision_reason TEXT,
    rule_id TEXT, input_summary TEXT, verification_summary TEXT, decision_context TEXT, result TEXT NOT NULL
);
CREATE INDEX idx_audit_actions_ts ON audit_actions(ts);
CREATE INDEX idx_audit_actions_pane ON audit_actions(pane_id, ts);
CREATE INDEX idx_audit_actions_actor ON audit_actions(actor_kind, ts);
CREATE INDEX idx_audit_actions_action ON audit_actions(action_kind, ts);
CREATE INDEX idx_audit_actions_decision ON audit_actions(policy_decision, ts);
CREATE INDEX idx_audit_actions_correlation ON audit_actions(correlation_id);
CREATE TABLE action_undo (
    audit_action_id INTEGER PRIMARY KEY REFERENCES audit_actions(id) ON DELETE CASCADE,
    undoable INTEGER NOT NULL DEFAULT 0, undo_strategy TEXT NOT NULL, undo_hint TEXT,
    undo_payload TEXT, undone_at INTEGER, undone_by TEXT
);
CREATE INDEX idx_action_undo_undoable ON action_undo(undoable) WHERE undoable = 1;
CREATE TABLE approval_tokens (
    id INTEGER PRIMARY KEY, code_hash TEXT NOT NULL, created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL, used_at INTEGER, workspace_id TEXT NOT NULL, action_kind TEXT NOT NULL,
    pane_id INTEGER REFERENCES panes(pane_id) ON DELETE SET NULL, action_fingerprint TEXT NOT NULL,
    plan_hash TEXT, plan_version INTEGER, risk_summary TEXT
);
CREATE UNIQUE INDEX idx_approval_tokens_hash ON approval_tokens(code_hash);
CREATE INDEX idx_approval_tokens_workspace ON approval_tokens(workspace_id, action_kind);
CREATE INDEX idx_approval_tokens_pane ON approval_tokens(pane_id);
CREATE INDEX idx_approval_tokens_expires ON approval_tokens(expires_at);
CREATE INDEX idx_approval_tokens_unused ON approval_tokens(used_at) WHERE used_at IS NULL;
CREATE INDEX idx_approval_tokens_fingerprint ON approval_tokens(action_fingerprint);
-- Migration v19 also ensures this partial index outside SCHEMA_SQL.
CREATE INDEX idx_approval_tokens_plan_hash ON approval_tokens(plan_hash) WHERE plan_hash IS NOT NULL;
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY, account_id TEXT NOT NULL, service TEXT NOT NULL, name TEXT,
    percent_remaining REAL NOT NULL, reset_at TEXT, tokens_used INTEGER, tokens_remaining INTEGER,
    tokens_limit INTEGER, last_refreshed_at INTEGER NOT NULL, last_used_at INTEGER,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_accounts_service_account ON accounts(service, account_id);
CREATE INDEX idx_accounts_service ON accounts(service);
CREATE INDEX idx_accounts_percent ON accounts(service, percent_remaining DESC);
CREATE INDEX idx_accounts_last_used ON accounts(service, last_used_at);
CREATE TABLE limit_windows (
    id INTEGER PRIMARY KEY, pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE,
    service TEXT NOT NULL, account_id TEXT NOT NULL, account_db_id INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    account_known INTEGER NOT NULL DEFAULT 0, agent_type TEXT, rule_id TEXT NOT NULL,
    event_type TEXT NOT NULL, limited_at INTEGER NOT NULL, reset_at INTEGER, reset_source TEXT NOT NULL,
    reset_text TEXT, conservative_ttl_ms INTEGER NOT NULL, last_seen_at INTEGER NOT NULL,
    seen_count INTEGER NOT NULL DEFAULT 1, metadata TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
    CHECK(account_known IN (0, 1)), CHECK(reset_source IN ('absolute', 'retry_after', 'unknown_ttl')),
    CHECK(seen_count >= 1), UNIQUE(pane_id, service, account_id)
);
CREATE INDEX idx_limit_windows_pane_account ON limit_windows(pane_id, service, account_id);
CREATE INDEX idx_limit_windows_service_reset ON limit_windows(service, reset_at);
CREATE INDEX idx_limit_windows_last_seen ON limit_windows(last_seen_at);
CREATE TABLE pane_reservations (
    id INTEGER PRIMARY KEY, pane_id INTEGER NOT NULL REFERENCES panes(pane_id),
    owner_kind TEXT NOT NULL, owner_id TEXT NOT NULL, reason TEXT, created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL, released_at INTEGER, status TEXT NOT NULL DEFAULT 'active'
);
CREATE INDEX idx_reservations_pane_status ON pane_reservations(pane_id, status);
CREATE INDEX idx_reservations_status ON pane_reservations(status);
CREATE INDEX idx_reservations_expires ON pane_reservations(expires_at) WHERE status = 'active';
CREATE TABLE fts_index_state (
    id INTEGER PRIMARY KEY CHECK (id = 1), index_version INTEGER NOT NULL DEFAULT 1,
    last_full_rebuild_at INTEGER, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE fts_pane_progress (
    pane_id INTEGER PRIMARY KEY REFERENCES panes(pane_id) ON DELETE CASCADE,
    last_indexed_seq INTEGER NOT NULL DEFAULT 0, indexed_count INTEGER NOT NULL DEFAULT 0, last_indexed_at INTEGER NOT NULL
);
CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE saved_searches (
    id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, query TEXT NOT NULL, pane_id INTEGER,
    "limit" INTEGER NOT NULL DEFAULT 50, since_mode TEXT NOT NULL DEFAULT 'last_run', since_ms INTEGER,
    schedule_interval_ms INTEGER, enabled INTEGER NOT NULL DEFAULT 0, last_run_at INTEGER,
    last_result_count INTEGER, last_error TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE INDEX idx_saved_searches_enabled ON saved_searches(enabled);
CREATE INDEX idx_saved_searches_last_run ON saved_searches(last_run_at);
CREATE TABLE maintenance_log (
    id INTEGER PRIMARY KEY, event_type TEXT NOT NULL, message TEXT, metadata TEXT, timestamp INTEGER NOT NULL
);
CREATE INDEX idx_maintenance_timestamp ON maintenance_log(timestamp);
CREATE TABLE secret_scan_reports (
    id INTEGER PRIMARY KEY, scope_hash TEXT NOT NULL, scope_json TEXT NOT NULL,
    report_version INTEGER NOT NULL, last_segment_id INTEGER, report_json TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE INDEX idx_secret_scan_reports_scope ON secret_scan_reports(scope_hash, created_at);
CREATE TABLE usage_metrics (
    id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, metric_type TEXT NOT NULL,
    pane_id INTEGER, agent_type TEXT, account_id TEXT, workflow_id TEXT, count INTEGER,
    amount REAL, tokens INTEGER, metadata TEXT, created_at INTEGER NOT NULL
);
CREATE INDEX idx_usage_metrics_timestamp ON usage_metrics(timestamp);
CREATE INDEX idx_usage_metrics_type_ts ON usage_metrics(metric_type, timestamp);
CREATE INDEX idx_usage_metrics_agent_ts ON usage_metrics(agent_type, timestamp);
CREATE INDEX idx_usage_metrics_account_ts ON usage_metrics(account_id, timestamp);
CREATE TABLE notification_history (
    id INTEGER PRIMARY KEY, timestamp INTEGER NOT NULL, event_id INTEGER, channel TEXT NOT NULL,
    title TEXT NOT NULL, body TEXT NOT NULL, severity TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending',
    error_message TEXT, acknowledged_at INTEGER, acknowledged_by TEXT, action_taken TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0, metadata TEXT, created_at INTEGER NOT NULL
);
CREATE INDEX idx_notification_history_timestamp ON notification_history(timestamp);
CREATE INDEX idx_notification_history_status ON notification_history(status);
CREATE INDEX idx_notification_history_event ON notification_history(event_id);
CREATE INDEX idx_notification_history_channel_ts ON notification_history(channel, timestamp);
CREATE TABLE pane_bookmarks (
    id INTEGER PRIMARY KEY, pane_id INTEGER NOT NULL, alias TEXT NOT NULL UNIQUE,
    tags TEXT, description TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE INDEX idx_pane_bookmarks_pane_id ON pane_bookmarks(pane_id);
CREATE INDEX idx_pane_bookmarks_alias ON pane_bookmarks(alias);
CREATE TABLE mux_sessions (
    session_id TEXT PRIMARY KEY, created_at INTEGER NOT NULL, last_checkpoint_at INTEGER,
    shutdown_clean INTEGER NOT NULL DEFAULT 0, topology_json TEXT NOT NULL,
    window_metadata_json TEXT, ft_version TEXT NOT NULL, host_id TEXT
);
CREATE TABLE session_checkpoints (
    id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE,
    checkpoint_at INTEGER NOT NULL,
    checkpoint_type TEXT NOT NULL CHECK(checkpoint_type IN ('periodic','event','shutdown','startup')),
    state_hash TEXT NOT NULL, pane_count INTEGER NOT NULL, total_bytes INTEGER NOT NULL, metadata_json TEXT
);
CREATE INDEX idx_checkpoints_session ON session_checkpoints(session_id, checkpoint_at);
CREATE TABLE mux_pane_state (
    id INTEGER PRIMARY KEY, checkpoint_id INTEGER NOT NULL REFERENCES session_checkpoints(id) ON DELETE CASCADE,
    pane_id INTEGER NOT NULL, cwd TEXT, command TEXT, env_json TEXT, terminal_state_json TEXT NOT NULL,
    agent_metadata_json TEXT, scrollback_checkpoint_seq INTEGER, last_output_at INTEGER
);
CREATE INDEX idx_pane_state_checkpoint ON mux_pane_state(checkpoint_id);
CREATE INDEX idx_pane_state_pane ON mux_pane_state(pane_id);
CREATE VIEW action_history AS
SELECT a.*, u.undoable, u.undo_strategy, u.undo_hint, u.undone_at, u.undone_by, w.workflow_id, w.step_name
FROM audit_actions a
LEFT JOIN action_undo u ON u.audit_action_id = a.id
LEFT JOIN workflow_step_logs w ON w.audit_action_id = a.id;

-- Migration-only additions absent from the historical baseline SCHEMA_SQL.
CREATE TABLE policy_denied_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT, ts_ms INTEGER NOT NULL, agent_id TEXT,
    tool_name TEXT NOT NULL, intent_hash TEXT, reason TEXT NOT NULL, reason_code TEXT NOT NULL,
    rule_id TEXT, decision TEXT NOT NULL
);
CREATE INDEX idx_policy_denied_audit_ts ON policy_denied_audit(ts_ms);
CREATE INDEX idx_policy_denied_audit_tool_ts ON policy_denied_audit(tool_name, ts_ms);
CREATE TABLE agent_profiles (
    name TEXT PRIMARY KEY NOT NULL, role TEXT NOT NULL DEFAULT '', tags TEXT NOT NULL DEFAULT '[]',
    shell TEXT NOT NULL DEFAULT '', command TEXT, env TEXT NOT NULL DEFAULT '{}', metadata TEXT NOT NULL DEFAULT '{}',
    created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
);
CREATE INDEX agent_profiles_role_idx ON agent_profiles(role);
CREATE TABLE profiles_applied_log (
    content_hash TEXT PRIMARY KEY NOT NULL, profile_name TEXT NOT NULL, profile_updated_at_ms INTEGER NOT NULL,
    count INTEGER NOT NULL, panes_spawned_json TEXT NOT NULL DEFAULT '[]', recorded_at_ms INTEGER NOT NULL
);
CREATE INDEX profiles_applied_log_profile_name_idx ON profiles_applied_log(profile_name);
CREATE TABLE fleet_mutation_receipts (
    idempotency_key TEXT PRIMARY KEY NOT NULL, payload_fingerprint TEXT NOT NULL,
    action TEXT NOT NULL, plan_id TEXT NOT NULL, dry_run INTEGER NOT NULL DEFAULT 0,
    receipt_json TEXT NOT NULL, recorded_at_ms INTEGER NOT NULL
);
CREATE INDEX fleet_mutation_receipts_action_time_idx ON fleet_mutation_receipts(action, recorded_at_ms DESC);

-- Stable test provenance/time; version authorities agree before upgrade.
INSERT INTO schema_version VALUES (31, 1700000000000, 'Historical v31 fixture');
INSERT INTO ft_meta VALUES (1, 31, '0.1.0', '0.1.0', 1700000000000);
PRAGMA user_version = 31;
