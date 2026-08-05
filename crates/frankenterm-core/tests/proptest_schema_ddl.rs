use frankenterm_core::storage::{SCHEMA_SQL, SCHEMA_VERSION};

const REQUIRED_TABLES: &[&str] = &[
    "schema_version",
    "ft_meta",
    "panes",
    "output_segments",
    "segment_embeddings",
    "output_gaps",
    "events",
    "event_labels",
    "event_notes",
    "event_mutes",
    "agent_sessions",
    "workflow_executions",
    "workflow_step_logs",
    "workflow_action_plans",
    "prepared_plans",
    "audit_actions",
    "action_undo",
    "approval_tokens",
    "accounts",
    "pane_reservations",
    "fts_index_state",
    "fts_pane_progress",
    "config",
    "saved_searches",
    "maintenance_log",
    "secret_scan_reports",
    "usage_metrics",
    "notification_history",
    "pane_bookmarks",
    "mux_sessions",
    "session_checkpoints",
    "mux_pane_state",
];

const REQUIRED_INDEXES: &[(&str, &str)] = &[
    ("idx_panes_last_seen", "panes"),
    ("idx_panes_observed", "panes"),
    ("idx_segments_pane_seq", "output_segments"),
    ("idx_segments_captured", "output_segments"),
    ("idx_segments_pane_captured", "output_segments"),
    ("idx_segment_embeddings_embedder", "segment_embeddings"),
    ("idx_gaps_pane", "output_gaps"),
    ("idx_gaps_detected", "output_gaps"),
    ("idx_events_pane", "events"),
    ("idx_events_rule", "events"),
    ("idx_events_unhandled_detected", "events"),
    ("idx_events_unhandled_id", "events"),
    ("idx_events_unhandled_pane", "events"),
    ("idx_event_labels_event", "event_labels"),
    ("idx_event_labels_label", "event_labels"),
    ("idx_action_plans_hash", "workflow_action_plans"),
    ("idx_approval_tokens_unused", "approval_tokens"),
    ("idx_notification_history_event", "notification_history"),
    ("idx_pane_state_checkpoint", "mux_pane_state"),
];

const OUTPUT_SEGMENT_TRIGGERS: &[(&str, &str, &str)] = &[
    (
        "output_segments_ai",
        "AFTER INSERT",
        "INSERT INTO output_segments_fts(rowid, content)",
    ),
    (
        "output_segments_ad",
        "AFTER DELETE",
        "INSERT INTO output_segments_fts(output_segments_fts, rowid, content) VALUES('delete'",
    ),
    (
        "output_segments_au",
        "AFTER UPDATE",
        "INSERT INTO output_segments_fts(rowid, content) VALUES (new.id, new.content)",
    ),
];

const REQUIRED_FOREIGN_KEYS: &[(&str, &str)] = &[
    (
        "output_segments",
        "pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE",
    ),
    (
        "segment_embeddings",
        "segment_id INTEGER NOT NULL REFERENCES output_segments(id) ON DELETE CASCADE",
    ),
    (
        "events",
        "pane_id INTEGER NOT NULL REFERENCES panes(pane_id) ON DELETE CASCADE",
    ),
    (
        "workflow_step_logs",
        "workflow_id TEXT NOT NULL REFERENCES workflow_executions(id) ON DELETE CASCADE",
    ),
    (
        "action_undo",
        "audit_action_id INTEGER PRIMARY KEY REFERENCES audit_actions(id) ON DELETE CASCADE",
    ),
    (
        "session_checkpoints",
        "session_id TEXT NOT NULL REFERENCES mux_sessions(session_id) ON DELETE CASCADE",
    ),
];

const REQUIRED_PRAGMAS: &[&str] = &[
    "PRAGMA journal_mode = WAL;",
    "PRAGMA foreign_keys = ON;",
    "PRAGMA synchronous = NORMAL;",
];

fn exact_declaration_offsets(haystack: &str, declaration_prefix: &str) -> Vec<usize> {
    haystack
        .match_indices(declaration_prefix)
        .filter_map(|(offset, _)| {
            haystack
                .as_bytes()
                .get(offset + declaration_prefix.len())
                .is_some_and(|byte| byte.is_ascii_whitespace())
                .then_some(offset)
        })
        .collect()
}

fn unique_declaration_start(declaration_kind: &str, name: &str) -> usize {
    let declaration_prefix = format!("{declaration_kind}{name}");
    let offsets = exact_declaration_offsets(SCHEMA_SQL, &declaration_prefix);
    assert_eq!(
        offsets.len(),
        1,
        "{name} should have exactly one exact SCHEMA_SQL declaration"
    );
    offsets[0]
}

#[test]
fn schema_ddl_required_tables_are_declared_once() {
    for &table in REQUIRED_TABLES {
        let _ = unique_declaration_start("CREATE TABLE IF NOT EXISTS ", table);
    }
}

#[test]
fn schema_ddl_required_indexes_target_expected_tables() {
    for &(index_name, table_name) in REQUIRED_INDEXES {
        let declaration_start =
            unique_declaration_start("CREATE INDEX IF NOT EXISTS ", index_name);
        // Include the opening column-list delimiter so a similarly prefixed
        // table (for example `events_archive`) cannot satisfy an `events`
        // target assertion.
        let on_clause = format!("ON {table_name}(");
        let declaration_tail = &SCHEMA_SQL[declaration_start..];
        let declaration_end = declaration_tail
            .find(';')
            .expect("index declaration should end with a semicolon");
        let declaration = &declaration_tail[..=declaration_end];

        assert!(
            declaration.contains(&on_clause),
            "index {} should target table {}",
            index_name,
            table_name,
        );
    }
}

#[test]
fn schema_ddl_output_segment_fts_triggers_keep_expected_actions() {
    for &(trigger_name, trigger_timing, fts_action) in OUTPUT_SEGMENT_TRIGGERS {
        let trigger_start =
            unique_declaration_start("CREATE TRIGGER IF NOT EXISTS ", trigger_name);
        let trigger_tail = &SCHEMA_SQL[trigger_start..];
        let trigger_end = trigger_tail
            .find("\nEND;")
            .expect("trigger declaration should end with END;")
            + "\nEND;".len();
        let trigger_sql = &trigger_tail[..trigger_end];

        assert!(trigger_sql.contains(trigger_timing));
        assert!(trigger_sql.contains(fts_action));
    }
}

#[test]
fn schema_ddl_required_foreign_keys_stay_cascade_safe() {
    for &(table_name, foreign_key_clause) in REQUIRED_FOREIGN_KEYS {
        let table_start = unique_declaration_start("CREATE TABLE IF NOT EXISTS ", table_name);
        let table_tail = &SCHEMA_SQL[table_start..];
        let table_end = table_tail
            .find("\n);")
            .expect("table declaration should end with a closing line")
            + "\n);".len();
        let table_sql = &table_tail[..table_end];

        assert!(
            table_sql.contains(foreign_key_clause),
            "table {} should retain foreign key clause {}",
            table_name,
            foreign_key_clause,
        );
        assert!(foreign_key_clause.contains("ON DELETE CASCADE"));
    }
}

#[test]
fn schema_ddl_pragmas_and_version_metadata_are_stable() {
    assert!(SCHEMA_VERSION > 0);
    for &pragma in REQUIRED_PRAGMAS {
        assert!(SCHEMA_SQL.contains(pragma));
    }
    assert!(SCHEMA_SQL.contains("version INTEGER NOT NULL"));
    assert!(SCHEMA_SQL.contains("schema_version INTEGER NOT NULL"));
    assert!(SCHEMA_SQL.contains("min_compatible_ft TEXT NOT NULL"));
}
