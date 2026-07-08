//! Config-honoring conformance (ft-m5hvl).
//!
//! Locks in the 5 reality-check "configured-but-ignored" fixes by asserting that
//! each config field, when set to a non-default value, ACTUALLY affects runtime
//! behavior — it is never silently ignored:
//!
//!   * safety        (ft-1t4o4) — `[safety.redaction|capabilities|reservations|
//!                                 semantic_shock]` + `approval.require_reapproval_on_failure`
//!   * retention     (ft-rrqhm) — `storage.retention_max_mb`
//!   * ingest        (ft-ta5as) — `ingest.backpressure_threshold|gap_detection|
//!                                 gap_detection_threshold_percent|max_segment_bytes`
//!   * snapshots     (ft-0yuxe) — `[snapshots.session_retention]`
//!   * search.daemon (ft-v46vj) — `search.daemon.socket_path|auto_spawn|
//!                                 worker_scan_interval_secs|worker_batch_size` (+ `enabled`)
//!
//! Two honoring mechanisms are exercised, both of which mean "not silently
//! ignored":
//!
//!   (A) BEHAVIORAL wiring — the field's value changes what the runtime DOES.
//!       Proven by running the real consumer (eviction / session cleanup) and
//!       observing different outcomes for different config values.
//!   (B) FAIL-CLOSED validation — a field with no production consumer, when set
//!       to a non-default value, makes `Config::validate()` FAIL LOUDLY (naming
//!       the key) instead of silently taking no effect. Proven by asserting the
//!       rejection.
//!
//! If a future change un-wires a behavioral fix, or drops a fail-closed
//! validate-reject, the corresponding assertion here fails.

use rusqlite::Connection;

use frankenterm_core::config::{Config, SessionRetentionConfig};

// ===========================================================================
// (B) Fail-closed validation matrix — pure `Config`, no DB / runtime needed.
// ===========================================================================

/// Apply `mutate` to a default config and assert `validate()` REJECTS it
/// fail-closed, naming `key` in the error (so the operator learns which setting
/// is unhonored rather than getting a silent no-op).
fn assert_rejects(area: &str, key: &str, mutate: impl FnOnce(&mut Config)) {
    let mut config = Config::default();
    mutate(&mut config);
    let err = config.validate().expect_err(&format!(
        "[{area}] setting `{key}` to a non-default value must be REJECTED fail-closed, not silently ignored"
    ));
    let msg = err.to_string();
    assert!(
        msg.contains(key),
        "[{area}] the fail-closed rejection must name `{key}` so the operator can act; got: {msg}"
    );
}

/// Apply `mutate` to a default config and assert `validate()` ACCEPTS it — used
/// for fields that ARE consumed in production, so they must NOT be fail-closed
/// (accepting a custom value is part of honoring it).
fn assert_accepts(label: &str, mutate: impl FnOnce(&mut Config)) {
    let mut config = Config::default();
    mutate(&mut config);
    let result = config.validate();
    assert!(
        result.is_ok(),
        "[{label}] a consumed/honored config field must be ACCEPTED by validate, got error: {:?}",
        result.err()
    );
}

/// Precondition for every `assert_accepts` below: the default config validates,
/// so any single-field acceptance failure is attributable to that field.
#[test]
fn config_default_validates() {
    Config::default()
        .validate()
        .expect("Config::default() must validate (precondition for the accept assertions)");
}

#[test]
fn safety_blocks_are_not_silently_ignored() {
    // ft-1t4o4: four [safety.*] blocks + the approval reapproval flag have no
    // behavior-affecting production consumer, so a customized value must fail
    // closed (a silent no-op here is a false sense of safety — e.g. dropped
    // custom redaction patterns can leak secrets).
    assert_rejects("safety", "safety.redaction", |c| {
        c.safety.redaction.enabled = !c.safety.redaction.enabled;
    });
    assert_rejects("safety", "safety.capabilities", |c| {
        c.safety.capabilities.allow_control_chars = !c.safety.capabilities.allow_control_chars;
    });
    assert_rejects("safety", "safety.reservations", |c| {
        // Flip a bound-free bool so `SafetyConfig::validate` (which runs before
        // the inline fail-closed reject) cannot fire a different cross-field
        // (e.g. ttl) error first.
        c.safety.reservations.auto_release_on_complete =
            !c.safety.reservations.auto_release_on_complete;
    });
    assert_rejects("safety", "safety.semantic_shock", |c| {
        c.safety.semantic_shock.enabled = !c.safety.semantic_shock.enabled;
    });
    assert_rejects(
        "safety",
        "safety.approval.require_reapproval_on_failure",
        |c| {
            c.safety.approval.require_reapproval_on_failure =
                !c.safety.approval.require_reapproval_on_failure;
        },
    );
}

#[test]
fn ingest_fields_are_not_silently_ignored() {
    // ft-ta5as: four [ingest] fields are parsed/documented but unconsumed
    // (backpressure uses a fixed tailer threshold, gap recording is
    // unconditional, no percentage heuristic, and the persist ceiling comes
    // from tuning.ingest.max_persist_segment_bytes).
    assert_rejects("ingest", "ingest.backpressure_threshold", |c| {
        c.ingest.backpressure_threshold = c.ingest.backpressure_threshold.wrapping_add(1);
    });
    assert_rejects("ingest", "ingest.gap_detection", |c| {
        c.ingest.gap_detection = !c.ingest.gap_detection;
    });
    assert_rejects("ingest", "ingest.gap_detection_threshold_percent", |c| {
        c.ingest.gap_detection_threshold_percent =
            c.ingest.gap_detection_threshold_percent.wrapping_add(1);
    });
    assert_rejects("ingest", "ingest.max_segment_bytes", |c| {
        c.ingest.max_segment_bytes = c.ingest.max_segment_bytes.wrapping_add(1);
    });
}

#[test]
fn workflows_audit_steps_is_not_silently_ignored() {
    // ft-7h5da.5.7: workflows.audit_steps is documented as "Enable step-level
    // audit logging" but no workflow engine/runner path reads it — workflow
    // execution recording is unconditional and there is no step-level audit
    // toggle. A customized value must fail closed instead of silently taking
    // no effect (an operator "disabling" audit logging must not believe it
    // was ever on in that form).
    assert_rejects("workflows", "workflows.audit_steps", |c| {
        c.workflows.audit_steps = !c.workflows.audit_steps;
    });
}

#[test]
fn search_daemon_unconsumed_fields_are_not_silently_ignored() {
    // ft-v46vj: only search.daemon.enabled is consumed (ipc flips
    // background_job_status). The four sibling fields have no running daemon to
    // honor them, so non-default values fail closed.
    assert_rejects("search.daemon", "search.daemon.socket_path", |c| {
        c.search.daemon.socket_path = "custom-conformance.sock".to_string();
    });
    assert_rejects("search.daemon", "search.daemon.auto_spawn", |c| {
        c.search.daemon.auto_spawn = !c.search.daemon.auto_spawn;
    });
    assert_rejects(
        "search.daemon",
        "search.daemon.worker_scan_interval_secs",
        |c| {
            c.search.daemon.worker_scan_interval_secs =
                c.search.daemon.worker_scan_interval_secs.wrapping_add(1);
        },
    );
    assert_rejects("search.daemon", "search.daemon.worker_batch_size", |c| {
        c.search.daemon.worker_batch_size = c.search.daemon.worker_batch_size.wrapping_add(1);
    });

    // enabled IS consumed, so it must be accepted (honored), not rejected.
    assert_accepts("search.daemon.enabled", |c| {
        c.search.daemon.enabled = true;
    });
}

#[test]
fn search_reranker_enabled_is_not_silently_ignored() {
    // ft-vyg9y: search.reranker_enabled is parsed but its only reader appends a
    // "cross-encoder" status-tier string — no production dispatch reranks (the
    // reranker subsystem is a test-only scaffold with an ONNX stub). Setting it
    // must fail closed, not silently enable nothing.
    assert_rejects("search.reranker", "search.reranker_enabled", |c| {
        c.search.reranker_enabled = true;
    });
}

#[test]
fn wired_retention_and_snapshot_fields_are_accepted_not_fail_closed() {
    // retention_max_mb (ft-rrqhm) and session_retention (ft-0yuxe) are genuinely
    // wired, so validate must NOT fail-close a custom value — they are honored,
    // not ignored. (The behavioral proofs that the VALUE changes runtime
    // behavior live in the tests below.)
    assert_accepts("storage.retention_max_mb", |c| {
        c.storage.retention_max_mb = 1000;
    });
    assert_accepts("snapshots.session_retention", |c| {
        c.snapshots.session_retention.cleanup_interval_hours = 12;
        c.snapshots.session_retention.max_age_days = 10;
        c.snapshots.session_retention.max_closed_sessions = 5;
        c.snapshots.session_retention.max_total_size_mb = 100;
    });
}

// ===========================================================================
// (A) Behavioral wiring — the configured VALUE changes what the runtime does.
// ===========================================================================

/// Run an async body on an ungated current-thread compat runtime (mirrors the
/// `tests/web.rs` harness; not gated behind `asupersync-runtime`).
fn run_async<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use frankenterm_core::runtime_async::CompatRuntime;
    let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future);
}

/// retention (ft-rrqhm): `storage.retention_max_mb` is honored behaviorally —
/// the configured cap value drives actual segment eviction. A `0` cap (the
/// "unset/no-limit" default) evicts nothing; a small positive cap evicts the
/// over-cap data and shrinks the live database size. Different config values =>
/// different runtime behavior, the inverse of a silent ignore.
#[test]
fn retention_max_mb_value_drives_eviction() {
    use frankenterm_core::storage::{PaneRecord, StorageConfig, StorageHandle};

    run_async(async {
        let db_path =
            std::env::temp_dir().join(format!("ft_m5hvl_retention_{}.db", std::process::id()));
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&db_path);

        let storage = StorageHandle::with_config(&db_str, StorageConfig::default())
            .await
            .expect("open storage");

        // Segments carry a foreign key to their pane row, so register the pane
        // before appending (matches the live capture path: discovery upserts
        // the pane, then captures append segments).
        storage
            .upsert_pane(PaneRecord {
                pane_id: 7,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("pane-7".to_string()),
                cwd: Some("/tmp/pane-7".to_string()),
                tty_name: None,
                first_seen_at: 1_710_000_000_000,
                last_seen_at: 1_710_000_000_000,
                observed: true,
                ignore_reason: None,
                last_decision_at: Some(1_710_000_000_000),
            })
            .await
            .expect("upsert pane 7");

        // ~4 MiB of low-cardinality content keeps the FTS index tiny so segment
        // content dominates the live size; comfortably over a 1 MiB cap.
        let big = "x".repeat(16 * 1024);
        for i in 0..256u64 {
            storage
                .append_segment(7, &format!("seg-{i}-{big}"), None)
                .await
                .expect("append segment");
        }
        storage.checkpoint().await.expect("checkpoint");

        // retention_max_mb = 0 (the default "no limit"): no eviction.
        let disabled = storage.enforce_size_limit(0).await.expect("enforce 0");
        assert_eq!(
            disabled.deleted_segments, 0,
            "retention_max_mb=0 must evict nothing"
        );

        // retention_max_mb = 1 MiB: the value drives real eviction.
        let capped = storage.enforce_size_limit(1).await.expect("enforce 1");
        assert!(
            capped.deleted_segments > 0,
            "retention_max_mb=1 must evict over-cap segments (the value is honored), got {capped:?}"
        );
        assert!(
            capped.used_bytes_after < capped.used_bytes_before,
            "live database size must shrink after size-cap eviction: {capped:?}"
        );

        storage.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));
    });
}

fn make_session_db() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .expect("enable fks");
    conn.execute_batch(frankenterm_core::storage::SCHEMA_SQL)
        .expect("apply schema");
    conn
}

fn insert_closed_session(conn: &Connection, id: &str, created_at: i64) {
    conn.execute(
        "INSERT INTO mux_sessions (session_id, created_at, shutdown_clean, topology_json, ft_version)
         VALUES (?1, ?2, 1, '{}', '0.1.0')",
        rusqlite::params![id, created_at],
    )
    .expect("insert session");
}

fn count_sessions(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
        .expect("count sessions")
}

/// snapshots (ft-0yuxe): `snapshots.session_retention.max_closed_sessions` is
/// honored behaviorally — the configured value drives how many closed sessions
/// survive cleanup. `0` (unlimited) deletes nothing; a positive cap evicts the
/// excess. Different config values => different deletion outcomes.
///
/// This exercises the cleanup engine that ft-0yuxe wired into the live snapshot
/// scheduler (previously it had no production caller, so the whole
/// `[snapshots.session_retention]` block was dead).
#[test]
fn session_retention_max_closed_sessions_drives_cleanup() {
    use frankenterm_core::session_retention::cleanup_sessions;

    let conn = make_session_db();
    for i in 0..5 {
        insert_closed_session(&conn, &format!("sess-{i}"), 1_000 + i64::from(i));
    }
    assert_eq!(
        count_sessions(&conn),
        5,
        "fixture: 5 closed sessions inserted"
    );

    // Only exercise the count policy: disable age + size so this assertion is
    // attributable solely to max_closed_sessions.
    let unlimited = SessionRetentionConfig {
        max_age_days: 0,
        max_closed_sessions: 0, // unlimited
        max_total_size_mb: 0,
        cleanup_interval_hours: 24,
    };
    let r0 = cleanup_sessions(&conn, &unlimited).expect("cleanup unlimited");
    assert_eq!(
        r0.deleted_by_count, 0,
        "max_closed_sessions=0 (unlimited) must delete nothing by count"
    );
    assert_eq!(
        count_sessions(&conn),
        5,
        "unlimited cleanup retains all sessions"
    );

    let capped = SessionRetentionConfig {
        max_age_days: 0,
        max_closed_sessions: 2,
        max_total_size_mb: 0,
        cleanup_interval_hours: 24,
    };
    let r1 = cleanup_sessions(&conn, &capped).expect("cleanup capped");
    assert_eq!(
        r1.deleted_by_count, 3,
        "max_closed_sessions=2 must evict the 3 excess closed sessions"
    );
    assert_eq!(
        count_sessions(&conn),
        2,
        "exactly max_closed_sessions=2 sessions must remain"
    );
}
