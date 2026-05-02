use proptest::prelude::*;

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
    ("idx_segment_embeddings_embedder", "segment_embeddings"),
    ("idx_gaps_pane", "output_gaps"),
    ("idx_gaps_detected", "output_gaps"),
    ("idx_events_pane", "events"),
    ("idx_events_rule", "events"),
    ("idx_events_unhandled", "events"),
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
        "audit_action_id INTEGER NOT NULL REFERENCES audit_actions(id) ON DELETE CASCADE",
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

fn occurrence_count(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

fn table_strategy() -> impl Strategy<Value = &'static str> {
    prop::sample::select(REQUIRED_TABLES.to_vec())
}

fn index_strategy() -> impl Strategy<Value = (&'static str, &'static str)> {
    prop::sample::select(REQUIRED_INDEXES.to_vec())
}

fn trigger_strategy() -> impl Strategy<Value = (&'static str, &'static str, &'static str)> {
    prop::sample::select(OUTPUT_SEGMENT_TRIGGERS.to_vec())
}

fn foreign_key_strategy() -> impl Strategy<Value = (&'static str, &'static str)> {
    prop::sample::select(REQUIRED_FOREIGN_KEYS.to_vec())
}

fn pragma_strategy() -> impl Strategy<Value = &'static str> {
    prop::sample::select(REQUIRED_PRAGMAS.to_vec())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_schema_ddl_required_tables_are_declared_once(table in table_strategy()) {
        let create_clause = format!("CREATE TABLE IF NOT EXISTS {table}");

        prop_assert_eq!(
            occurrence_count(SCHEMA_SQL, &create_clause),
            1,
            "table {table} should have exactly one CREATE TABLE clause",
        );
    }

    #[test]
    fn proptest_schema_ddl_required_indexes_target_expected_tables(
        (index_name, table_name) in index_strategy(),
    ) {
        let create_clause = format!("CREATE INDEX IF NOT EXISTS {index_name}");
        let on_clause = format!("ON {table_name}");

        prop_assert!(
            SCHEMA_SQL.contains(&create_clause),
            "missing index declaration for {index_name}",
        );
        prop_assert!(
            SCHEMA_SQL[SCHEMA_SQL.find(&create_clause).unwrap()..].contains(&on_clause),
            "index {index_name} should target table {table_name}",
        );
    }

    #[test]
    fn proptest_schema_ddl_output_segment_fts_triggers_keep_expected_actions(
        (trigger_name, trigger_timing, fts_action) in trigger_strategy(),
    ) {
        let create_clause = format!("CREATE TRIGGER IF NOT EXISTS {trigger_name}");
        let trigger_start = SCHEMA_SQL
            .find(&create_clause)
            .expect("trigger declaration should exist");
        let trigger_sql = &SCHEMA_SQL[trigger_start..];

        prop_assert_eq!(
            occurrence_count(SCHEMA_SQL, &create_clause),
            1,
            "trigger {trigger_name} should have exactly one SCHEMA_SQL declaration",
        );
        prop_assert!(trigger_sql.contains(trigger_timing));
        prop_assert!(trigger_sql.contains(fts_action));
    }

    #[test]
    fn proptest_schema_ddl_required_foreign_keys_stay_cascade_safe(
        (table_name, foreign_key_clause) in foreign_key_strategy(),
    ) {
        let create_clause = format!("CREATE TABLE IF NOT EXISTS {table_name}");
        let table_start = SCHEMA_SQL
            .find(&create_clause)
            .expect("table declaration should exist");
        let table_sql = &SCHEMA_SQL[table_start..];

        prop_assert!(
            table_sql.contains(foreign_key_clause),
            "table {table_name} should retain foreign key clause {foreign_key_clause}",
        );
        prop_assert!(foreign_key_clause.contains("ON DELETE CASCADE"));
    }

    #[test]
    fn proptest_schema_ddl_pragmas_and_version_metadata_are_stable(pragma in pragma_strategy()) {
        prop_assert!(SCHEMA_VERSION > 0);
        prop_assert!(SCHEMA_SQL.contains(pragma));
        prop_assert!(SCHEMA_SQL.contains("version INTEGER NOT NULL"));
        prop_assert!(SCHEMA_SQL.contains("schema_version INTEGER NOT NULL"));
        prop_assert!(SCHEMA_SQL.contains("min_compatible_ft TEXT NOT NULL"));
    }
}
