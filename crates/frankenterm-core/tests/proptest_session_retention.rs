//! Property-based tests for session_retention module.
//!
//! Verifies cleanup invariants across arbitrary session configurations:
//!
//! - CleanupResult::total_sessions_deleted == age + count + size (always)
//! - CleanupResult::any_work_done iff total > 0 OR orphans > 0
//! - CleanupResult::default is all zeros and not any_work_done
//! - SessionRetentionConfig serde roundtrip (JSON and TOML)
//! - Active sessions (shutdown_clean=0) are NEVER deleted
//! - After age cleanup, remaining closed sessions are within age limit
//! - After count cleanup, closed session count <= max_closed_sessions
//! - Cleanup with all policies disabled deletes nothing
//! - Orphan cleanup removes all orphaned rows
//! - VACUUM only triggered when >= 10 sessions deleted
//! - Idempotency: second cleanup yields zero additional deletions
//! - Session deletion count never exceeds initial closed session count
//! - Size cleanup respects budget after completion

use proptest::prelude::*;
use rusqlite::{Connection, params};
use std::time::{SystemTime, UNIX_EPOCH};

use frankenterm_core::config::SessionRetentionConfig;
use frankenterm_core::session_retention::{CleanupResult, cleanup_sessions};
use frankenterm_core::storage::SCHEMA_SQL;

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn make_test_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    conn.execute_batch(SCHEMA_SQL).unwrap();
    conn
}

fn insert_session(conn: &Connection, id: &str, created_at: i64, shutdown_clean: bool) {
    conn.execute(
        "INSERT INTO mux_sessions (session_id, created_at, shutdown_clean, topology_json, ft_version)
         VALUES (?1, ?2, ?3, '{}', '0.1.0')",
        params![id, created_at, shutdown_clean as i64],
    )
    .unwrap();
}

fn insert_checkpoint(
    conn: &Connection,
    session_id: &str,
    checkpoint_at: i64,
    total_bytes: i64,
) -> i64 {
    conn.execute(
        "INSERT INTO session_checkpoints
         (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
         VALUES (?1, ?2, 'periodic', 'hash', 1, ?3)",
        params![session_id, checkpoint_at, total_bytes],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_pane_state(conn: &Connection, checkpoint_id: i64, pane_id: u64) {
    conn.execute(
        "INSERT INTO mux_pane_state
         (checkpoint_id, pane_id, terminal_state_json)
         VALUES (?1, ?2, '{}')",
        params![checkpoint_id, pane_id as i64],
    )
    .unwrap();
}

/// Insert an orphaned checkpoint (FK disabled temporarily).
fn insert_orphaned_checkpoint(conn: &Connection, fake_session_id: &str, checkpoint_at: i64) -> i64 {
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute(
        "INSERT INTO session_checkpoints
         (session_id, checkpoint_at, checkpoint_type, state_hash, pane_count, total_bytes)
         VALUES (?1, ?2, 'periodic', 'hash', 0, 0)",
        params![fake_session_id, checkpoint_at],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    id
}

/// Insert an orphaned pane_state (FK disabled temporarily).
fn insert_orphaned_pane_state(conn: &Connection, fake_checkpoint_id: i64, pane_id: u64) {
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute(
        "INSERT INTO mux_pane_state
         (checkpoint_id, pane_id, terminal_state_json)
         VALUES (?1, ?2, '{}')",
        params![fake_checkpoint_id, pane_id as i64],
    )
    .unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
}

fn count_sessions(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM mux_sessions", [], |row| row.get(0))
        .unwrap()
}

fn count_closed_sessions(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM mux_sessions WHERE shutdown_clean = 1",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn count_active_sessions(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM mux_sessions WHERE shutdown_clean = 0",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

fn count_checkpoints(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM session_checkpoints", [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn count_pane_states(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM mux_pane_state", [], |row| row.get(0))
        .unwrap()
}

fn total_checkpoint_bytes(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COALESCE(SUM(total_bytes), 0) FROM session_checkpoints",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_cleanup_result() -> impl Strategy<Value = CleanupResult> {
    (
        0..100usize,
        0..100usize,
        0..100usize,
        0..100usize,
        0..100usize,
        any::<bool>(),
    )
        .prop_map(
            |(age, count, size, orphan_cp, orphan_ps, vacuumed)| CleanupResult {
                deleted_by_age: age,
                deleted_by_count: count,
                deleted_by_size: size,
                orphaned_checkpoints: orphan_cp,
                orphaned_pane_states: orphan_ps,
                vacuumed,
            },
        )
}

fn arb_config() -> impl Strategy<Value = SessionRetentionConfig> {
    (0..365u64, 0..200usize, 0..2000u64, 0..168u64).prop_map(|(age, count, size, interval)| {
        SessionRetentionConfig {
            max_age_days: age,
            max_closed_sessions: count,
            max_total_size_mb: size,
            cleanup_interval_hours: interval,
        }
    })
}

/// Strategy for number of closed vs active sessions.
fn arb_session_mix() -> impl Strategy<Value = (usize, usize)> {
    (0..20usize, 0..10usize)
}

// ────────────────────────────────────────────────────────────────────
// 1. CleanupResult::total_sessions_deleted == age + count + size
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn total_sessions_deleted_is_sum_of_components(result in arb_cleanup_result()) {
        let expected = result.deleted_by_age + result.deleted_by_count + result.deleted_by_size;
        prop_assert_eq!(
            result.total_sessions_deleted(),
            expected,
            "total_sessions_deleted must equal age({}) + count({}) + size({})",
            result.deleted_by_age, result.deleted_by_count, result.deleted_by_size
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 2. CleanupResult::any_work_done iff total > 0 OR orphans > 0
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn any_work_done_iff_total_or_orphans_positive(result in arb_cleanup_result()) {
        let expected = result.total_sessions_deleted() > 0
            || result.orphaned_checkpoints > 0
            || result.orphaned_pane_states > 0;
        prop_assert_eq!(
            result.any_work_done(),
            expected,
            "any_work_done() should be {} for total={}, orphan_cp={}, orphan_ps={}",
            expected,
            result.total_sessions_deleted(),
            result.orphaned_checkpoints,
            result.orphaned_pane_states
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 3. CleanupResult::default is all zeros and not any_work_done
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn default_cleanup_result_is_zero(_unused in Just(())) {
        let result = CleanupResult::default();
        prop_assert_eq!(result.deleted_by_age, 0usize, "default age should be 0");
        prop_assert_eq!(result.deleted_by_count, 0usize, "default count should be 0");
        prop_assert_eq!(result.deleted_by_size, 0usize, "default size should be 0");
        prop_assert_eq!(result.orphaned_checkpoints, 0usize, "default orphan_cp should be 0");
        prop_assert_eq!(result.orphaned_pane_states, 0usize, "default orphan_ps should be 0");
        prop_assert!(!result.vacuumed, "default vacuumed should be false");
        prop_assert!(!result.any_work_done(), "default should report no work done");
        prop_assert_eq!(result.total_sessions_deleted(), 0usize, "default total should be 0");
    }
}

// ────────────────────────────────────────────────────────────────────
// 4. SessionRetentionConfig serde roundtrip (JSON)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_json_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let decoded: SessionRetentionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.max_age_days, config.max_age_days, "max_age_days roundtrip");
        prop_assert_eq!(decoded.max_closed_sessions, config.max_closed_sessions, "max_closed_sessions roundtrip");
        prop_assert_eq!(decoded.max_total_size_mb, config.max_total_size_mb, "max_total_size_mb roundtrip");
        prop_assert_eq!(decoded.cleanup_interval_hours, config.cleanup_interval_hours, "cleanup_interval_hours roundtrip");
    }
}

// ────────────────────────────────────────────────────────────────────
// 5. SessionRetentionConfig serde roundtrip (TOML)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_toml_roundtrip(config in arb_config()) {
        let toml_str = toml::to_string(&config).unwrap();
        let decoded: SessionRetentionConfig = toml::from_str(&toml_str).unwrap();
        prop_assert_eq!(decoded.max_age_days, config.max_age_days, "max_age_days toml roundtrip");
        prop_assert_eq!(decoded.max_closed_sessions, config.max_closed_sessions, "max_closed_sessions toml roundtrip");
        prop_assert_eq!(decoded.max_total_size_mb, config.max_total_size_mb, "max_total_size_mb toml roundtrip");
        prop_assert_eq!(decoded.cleanup_interval_hours, config.cleanup_interval_hours, "cleanup_interval_hours toml roundtrip");
    }
}

// ────────────────────────────────────────────────────────────────────
// 6. SessionRetentionConfig serde default fill-in
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn config_serde_default_fills_missing_fields(_unused in Just(())) {
        // Empty JSON object should yield default values thanks to #[serde(default)]
        let decoded: SessionRetentionConfig = serde_json::from_str("{}").unwrap();
        let defaults = SessionRetentionConfig::default();
        prop_assert_eq!(decoded.max_age_days, defaults.max_age_days, "default max_age_days from empty JSON");
        prop_assert_eq!(decoded.max_closed_sessions, defaults.max_closed_sessions, "default max_closed_sessions");
        prop_assert_eq!(decoded.max_total_size_mb, defaults.max_total_size_mb, "default max_total_size_mb");
        prop_assert_eq!(decoded.cleanup_interval_hours, defaults.cleanup_interval_hours, "default cleanup_interval_hours");
    }
}

// ────────────────────────────────────────────────────────────────────
// 7. Active sessions (shutdown_clean=0) are NEVER deleted
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn active_sessions_never_deleted(
        num_active in 1..15usize,
        num_closed in 0..10usize,
        max_age in 1..60u64,
        max_count in 1..20usize,
        max_size in 1..100u64,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Insert active sessions (some very old)
        for i in 0..num_active {
            let created = now - (i as i64 + 1) * 90 * 86_400_000; // 90+ days ago each
            insert_session(&conn, &format!("active-{}", i), created, false);
        }

        // Insert closed sessions
        for i in 0..num_closed {
            let created = now - (i as i64 + 1) * 90 * 86_400_000;
            insert_session(&conn, &format!("closed-{}", i), created, true);
        }

        let active_before = count_active_sessions(&conn);

        let config = SessionRetentionConfig {
            max_age_days: max_age,
            max_closed_sessions: max_count,
            max_total_size_mb: max_size,
            cleanup_interval_hours: 24,
        };

        let _result = cleanup_sessions(&conn, &config).unwrap();
        let active_after = count_active_sessions(&conn);

        prop_assert_eq!(
            active_after,
            active_before,
            "active sessions should never be deleted: before={}, after={}",
            active_before, active_after
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 8. After age cleanup, remaining closed sessions are within age limit
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn age_cleanup_leaves_only_recent_closed(
        num_old in 0..10usize,
        num_recent in 0..10usize,
        max_age in 1..90u64,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let cutoff = now - (max_age as i64 * 86_400_000);

        // Insert old closed sessions (before cutoff)
        for i in 0..num_old {
            let created = cutoff - (i as i64 + 1) * 86_400_000; // 1+ days before cutoff
            insert_session(&conn, &format!("old-{}", i), created, true);
        }

        // Insert recent closed sessions (after cutoff)
        for i in 0..num_recent {
            let created = cutoff + (i as i64 + 1) * 3_600_000; // 1+ hours after cutoff
            insert_session(&conn, &format!("recent-{}", i), created, true);
        }

        // Only age policy enabled
        let config = SessionRetentionConfig {
            max_age_days: max_age,
            max_closed_sessions: 0, // disabled
            max_total_size_mb: 0,   // disabled
            cleanup_interval_hours: 24,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();

        // All old closed sessions should be deleted
        prop_assert_eq!(
            result.deleted_by_age, num_old,
            "should delete {} old sessions, deleted {}",
            num_old, result.deleted_by_age
        );

        // Only recent sessions remain
        let remaining = count_closed_sessions(&conn);
        prop_assert_eq!(
            remaining as usize, num_recent,
            "should have {} recent sessions remaining, got {}",
            num_recent, remaining
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 9. After count cleanup, closed sessions <= max_closed_sessions
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn count_cleanup_respects_limit(
        num_closed in 0..30usize,
        max_count in 1..20usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..num_closed {
            let created = now - (i as i64) * 1000;
            insert_session(&conn, &format!("sess-{}", i), created, true);
        }

        // Only count policy enabled
        let config = SessionRetentionConfig {
            max_age_days: 0,        // disabled
            max_closed_sessions: max_count,
            max_total_size_mb: 0,   // disabled
            cleanup_interval_hours: 24,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();
        let remaining = count_closed_sessions(&conn) as usize;

        prop_assert!(
            remaining <= max_count,
            "remaining closed sessions ({}) should be <= max_count ({})",
            remaining, max_count
        );

        // Exact expected deletions
        let expected_deletions = num_closed.saturating_sub(max_count);
        prop_assert_eq!(
            result.deleted_by_count, expected_deletions,
            "expected {} deletions by count, got {}",
            expected_deletions, result.deleted_by_count
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 10. Cleanup with all policies disabled deletes nothing
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn disabled_policies_delete_nothing(
        (num_closed, num_active) in arb_session_mix(),
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..num_closed {
            let created = now - (i as i64 + 1) * 90 * 86_400_000;
            insert_session(&conn, &format!("closed-{}", i), created, true);
        }
        for i in 0..num_active {
            let created = now - (i as i64 + 1) * 90 * 86_400_000;
            insert_session(&conn, &format!("active-{}", i), created, false);
        }

        let total_before = count_sessions(&conn);

        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();

        prop_assert_eq!(
            result.deleted_by_age, 0usize,
            "disabled age policy should delete nothing"
        );
        prop_assert_eq!(
            result.deleted_by_count, 0usize,
            "disabled count policy should delete nothing"
        );
        prop_assert_eq!(
            result.deleted_by_size, 0usize,
            "disabled size policy should delete nothing"
        );

        let total_after = count_sessions(&conn);
        prop_assert_eq!(
            total_after, total_before,
            "total sessions unchanged with all policies disabled"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 11. Orphan cleanup removes all orphaned rows
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn orphan_cleanup_removes_all_orphans(
        num_orphan_cp in 0..8usize,
        num_orphan_ps in 0..8usize,
        num_valid_sessions in 0..5usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Insert valid sessions with checkpoints and pane states
        for i in 0..num_valid_sessions {
            let sid = format!("valid-{}", i);
            insert_session(&conn, &sid, now, true);
            let cp_id = insert_checkpoint(&conn, &sid, now, 1024);
            insert_pane_state(&conn, cp_id, i as u64);
        }

        // Insert orphaned checkpoints (referencing non-existent sessions)
        for i in 0..num_orphan_cp {
            insert_orphaned_checkpoint(&conn, &format!("ghost-sess-{}", i), now);
        }

        // Insert orphaned pane states (referencing non-existent checkpoints)
        for i in 0..num_orphan_ps {
            insert_orphaned_pane_state(&conn, 900_000 + i as i64, i as u64 + 1000);
        }

        // Use max settings so no sessions are deleted by age/count/size
        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();

        prop_assert_eq!(
            result.orphaned_checkpoints, num_orphan_cp,
            "should clean {} orphaned checkpoints, got {}",
            num_orphan_cp, result.orphaned_checkpoints
        );
        prop_assert_eq!(
            result.orphaned_pane_states, num_orphan_ps,
            "should clean {} orphaned pane states, got {}",
            num_orphan_ps, result.orphaned_pane_states
        );

        // After cleanup, no orphans should remain
        // Count checkpoints not belonging to any session
        let remaining_orphan_cp: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_checkpoints
                 WHERE session_id NOT IN (SELECT session_id FROM mux_sessions)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        prop_assert_eq!(remaining_orphan_cp, 0i64, "no orphaned checkpoints should remain");

        // Count pane states not belonging to any checkpoint
        let remaining_orphan_ps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mux_pane_state
                 WHERE checkpoint_id NOT IN (SELECT id FROM session_checkpoints)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        prop_assert_eq!(remaining_orphan_ps, 0i64, "no orphaned pane states should remain");
    }
}

// ────────────────────────────────────────────────────────────────────
// 12. VACUUM only triggered when >= 10 sessions deleted
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn vacuum_threshold_respected(
        num_old in 0..25usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Insert old closed sessions (all will be deleted by age policy)
        for i in 0..num_old {
            let created = now - (90 + i as i64) * 86_400_000; // 90+ days old
            insert_session(&conn, &format!("old-{}", i), created, true);
        }

        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 24,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();
        let total_deleted = result.total_sessions_deleted();

        if total_deleted >= 10 {
            prop_assert!(
                result.vacuumed,
                "VACUUM should run when {} >= 10 sessions deleted",
                total_deleted
            );
        } else {
            prop_assert!(
                !result.vacuumed,
                "VACUUM should NOT run when {} < 10 sessions deleted",
                total_deleted
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// 13. Idempotency: second cleanup yields zero additional deletions
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn cleanup_is_idempotent(
        num_closed in 0..15usize,
        num_active in 0..5usize,
        max_age in 1..60u64,
        max_count in 1..20usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Add 1-hour buffer to avoid boundary races: cleanup uses epoch_ms()
        // which advances slightly between the test's `now` and the call to
        // delete_sessions_by_age, so sessions exactly at the cutoff may
        // survive the first run and be caught on the second.
        let buffer_ms = 3_600_000_i64; // 1 hour

        for i in 0..num_closed {
            let created = now - (i as i64 + 1) * 5 * 86_400_000 - buffer_ms;
            insert_session(&conn, &format!("closed-{}", i), created, true);
        }
        for i in 0..num_active {
            let created = now - (i as i64 + 1) * 5 * 86_400_000 - buffer_ms;
            insert_session(&conn, &format!("active-{}", i), created, false);
        }

        let config = SessionRetentionConfig {
            max_age_days: max_age,
            max_closed_sessions: max_count,
            max_total_size_mb: 0,   // disabled to keep test focused
            cleanup_interval_hours: 24,
        };

        // First cleanup
        let _first = cleanup_sessions(&conn, &config).unwrap();

        // Second cleanup should find nothing to do
        let second = cleanup_sessions(&conn, &config).unwrap();

        prop_assert_eq!(
            second.deleted_by_age, 0usize,
            "second run should delete 0 by age, got {}",
            second.deleted_by_age
        );
        prop_assert_eq!(
            second.deleted_by_count, 0usize,
            "second run should delete 0 by count, got {}",
            second.deleted_by_count
        );
        prop_assert_eq!(
            second.deleted_by_size, 0usize,
            "second run should delete 0 by size, got {}",
            second.deleted_by_size
        );
        prop_assert_eq!(
            second.orphaned_checkpoints, 0usize,
            "second run should find 0 orphaned checkpoints"
        );
        prop_assert_eq!(
            second.orphaned_pane_states, 0usize,
            "second run should find 0 orphaned pane states"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 14. Total deleted never exceeds initial closed count
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn total_deleted_bounded_by_closed_count(
        num_closed in 0..20usize,
        num_active in 0..10usize,
        config in arb_config(),
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..num_closed {
            let created = now - (i as i64 + 1) * 10 * 86_400_000;
            let sid = format!("closed-{}", i);
            insert_session(&conn, &sid, created, true);
            insert_checkpoint(&conn, &sid, created, 10 * 1024 * 1024); // 10MB each
        }
        for i in 0..num_active {
            let created = now - (i as i64 + 1) * 10 * 86_400_000;
            insert_session(&conn, &format!("active-{}", i), created, false);
        }

        let result = cleanup_sessions(&conn, &config).unwrap();

        prop_assert!(
            result.total_sessions_deleted() <= num_closed,
            "total deleted ({}) should not exceed closed count ({})",
            result.total_sessions_deleted(), num_closed
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 15. Size cleanup respects budget
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn size_cleanup_stays_under_budget(
        num_closed in 1..15usize,
        bytes_per_session in 1..50u64,  // in MB
        max_size_mb in 1..200u64,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..num_closed {
            let sid = format!("sess-{}", i);
            let created = now - (i as i64) * 1000;
            insert_session(&conn, &sid, created, true);
            let bytes = (bytes_per_session as i64) * 1024 * 1024;
            insert_checkpoint(&conn, &sid, created, bytes);
        }

        let config = SessionRetentionConfig {
            max_age_days: 0,           // disabled
            max_closed_sessions: 0,    // disabled
            max_total_size_mb: max_size_mb,
            cleanup_interval_hours: 24,
        };

        let _result = cleanup_sessions(&conn, &config).unwrap();

        let remaining_bytes = total_checkpoint_bytes(&conn);
        let budget_bytes = (max_size_mb as i64) * 1024 * 1024;

        // After cleanup, remaining should be under budget
        // (or all closed sessions were exhausted)
        let remaining_closed = count_closed_sessions(&conn);
        if remaining_closed > 0 {
            prop_assert!(
                remaining_bytes <= budget_bytes,
                "remaining bytes ({}) should be <= budget ({}) when closed sessions remain ({})",
                remaining_bytes, budget_bytes, remaining_closed
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// 16. Cascade: session deletion cascades to checkpoints and pane states
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn cascade_deletes_child_rows(
        num_checkpoints in 1..5usize,
        num_panes_per_cp in 1..4usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;
        let old = now - 90 * 86_400_000; // 90 days old

        let sid = "old-cascading";
        insert_session(&conn, sid, old, true);

        for c in 0..num_checkpoints {
            let cp_id = insert_checkpoint(&conn, sid, old + c as i64, 1024);
            for p in 0..num_panes_per_cp {
                insert_pane_state(&conn, cp_id, (c * 100 + p) as u64);
            }
        }

        let cp_before = count_checkpoints(&conn);
        let ps_before = count_pane_states(&conn);
        prop_assert_eq!(cp_before, num_checkpoints as i64, "checkpoints inserted");
        prop_assert_eq!(ps_before, (num_checkpoints * num_panes_per_cp) as i64, "pane states inserted");

        let config = SessionRetentionConfig {
            max_age_days: 30,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 24,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();
        prop_assert_eq!(result.deleted_by_age, 1usize, "one session deleted by age");

        prop_assert_eq!(count_sessions(&conn), 0i64, "session deleted");
        prop_assert_eq!(count_checkpoints(&conn), 0i64, "checkpoints cascaded");
        prop_assert_eq!(count_pane_states(&conn), 0i64, "pane states cascaded");
    }
}

// ────────────────────────────────────────────────────────────────────
// 17. Count policy keeps the newest closed sessions
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn count_policy_preserves_newest(
        num_closed in 5..20usize,
        max_count in 1..5usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Insert sessions with increasing timestamps
        for i in 0..num_closed {
            let created = now - ((num_closed - i) as i64) * 86_400_000;
            insert_session(&conn, &format!("sess-{}", i), created, true);
        }

        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: max_count,
            max_total_size_mb: 0,
            cleanup_interval_hours: 24,
        };

        let _result = cleanup_sessions(&conn, &config).unwrap();

        // Remaining sessions should be the most recent ones
        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT session_id FROM mux_sessions ORDER BY created_at DESC")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };

        let effective_remaining = remaining.len().min(max_count);
        prop_assert!(
            remaining.len() <= max_count,
            "remaining ({}) should be <= max_count ({})",
            remaining.len(), max_count
        );

        // Verify they are the newest (highest index numbers)
        for sid in &remaining {
            let idx: usize = sid.strip_prefix("sess-").unwrap().parse().unwrap();
            prop_assert!(
                idx >= num_closed - effective_remaining,
                "session {} should be among the newest, expected idx >= {}",
                sid, num_closed - effective_remaining
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// 18. Size policy only deletes closed sessions
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn size_policy_only_targets_closed(
        num_active in 1..8usize,
        num_closed in 1..8usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // Active sessions with big checkpoints
        for i in 0..num_active {
            let sid = format!("active-{}", i);
            insert_session(&conn, &sid, now - (i as i64) * 1000, false);
            // We can't add checkpoints for active sessions easily because
            // we'd need the FK. We do add them to contribute to total_bytes.
            insert_checkpoint(&conn, &sid, now, 100 * 1024 * 1024); // 100MB each
        }

        // Closed sessions with big checkpoints
        for i in 0..num_closed {
            let sid = format!("closed-{}", i);
            insert_session(&conn, &sid, now - (i as i64) * 1000, true);
            insert_checkpoint(&conn, &sid, now, 100 * 1024 * 1024); // 100MB each
        }

        let active_before = count_active_sessions(&conn);

        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 1, // 1MB budget forces aggressive size cleanup
            cleanup_interval_hours: 24,
        };

        let _result = cleanup_sessions(&conn, &config).unwrap();
        let active_after = count_active_sessions(&conn);

        prop_assert_eq!(
            active_after, active_before,
            "size policy must not delete active sessions"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 19. any_work_done consistency with orphan-only cleanup
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn any_work_done_true_for_orphan_only(
        num_orphans in 1..10usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // No sessions, only orphaned checkpoints
        for i in 0..num_orphans {
            insert_orphaned_checkpoint(&conn, &format!("ghost-{}", i), now);
        }

        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();

        prop_assert_eq!(result.total_sessions_deleted(), 0usize, "no sessions deleted");
        prop_assert!(result.any_work_done(), "any_work_done should be true when orphans cleaned");
        prop_assert!(
            result.orphaned_checkpoints > 0,
            "should have orphaned checkpoints"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 20. Empty database cleanup is no-op
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn empty_db_cleanup_is_noop(config in arb_config()) {
        let conn = make_test_db();
        let result = cleanup_sessions(&conn, &config).unwrap();

        prop_assert_eq!(result.total_sessions_deleted(), 0usize, "empty db has nothing to delete");
        prop_assert!(!result.any_work_done(), "empty db should report no work done");
        prop_assert!(!result.vacuumed, "empty db should not vacuum");
    }
}

// ────────────────────────────────────────────────────────────────────
// 21. Combined policies: total deleted is consistent with DB state
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn combined_policies_consistent(
        num_closed in 0..15usize,
        num_active in 0..5usize,
        max_age in 1..60u64,
        max_count in 1..15usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..num_closed {
            let created = now - (i as i64 + 1) * 5 * 86_400_000;
            insert_session(&conn, &format!("closed-{}", i), created, true);
        }
        for i in 0..num_active {
            let created = now - (i as i64 + 1) * 5 * 86_400_000;
            insert_session(&conn, &format!("active-{}", i), created, false);
        }

        let initial_total = count_sessions(&conn);
        let initial_closed = count_closed_sessions(&conn) as usize;
        let initial_active = count_active_sessions(&conn) as usize;

        let config = SessionRetentionConfig {
            max_age_days: max_age,
            max_closed_sessions: max_count,
            max_total_size_mb: 0,
            cleanup_interval_hours: 24,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();
        let final_total = count_sessions(&conn);
        let final_active = count_active_sessions(&conn) as usize;

        // Sessions removed from DB match total_sessions_deleted
        let sessions_removed = (initial_total - final_total) as usize;
        prop_assert_eq!(
            sessions_removed,
            result.total_sessions_deleted(),
            "DB removal count should match result total"
        );

        // Active sessions unchanged
        prop_assert_eq!(final_active, initial_active, "active sessions unchanged");

        // Total deleted bounded by initial closed
        prop_assert!(
            result.total_sessions_deleted() <= initial_closed,
            "cannot delete more than initial closed"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 22. CleanupResult clone equality
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn cleanup_result_clone_preserves_fields(result in arb_cleanup_result()) {
        let cloned = result.clone();
        prop_assert_eq!(cloned.deleted_by_age, result.deleted_by_age, "age preserved");
        prop_assert_eq!(cloned.deleted_by_count, result.deleted_by_count, "count preserved");
        prop_assert_eq!(cloned.deleted_by_size, result.deleted_by_size, "size preserved");
        prop_assert_eq!(cloned.orphaned_checkpoints, result.orphaned_checkpoints, "orphan_cp preserved");
        prop_assert_eq!(cloned.orphaned_pane_states, result.orphaned_pane_states, "orphan_ps preserved");
        prop_assert_eq!(cloned.vacuumed, result.vacuumed, "vacuumed preserved");
        prop_assert_eq!(cloned.total_sessions_deleted(), result.total_sessions_deleted(), "total preserved");
        prop_assert_eq!(cloned.any_work_done(), result.any_work_done(), "any_work_done preserved");
    }
}

// ────────────────────────────────────────────────────────────────────
// 23. any_work_done false requires all zeros (except vacuumed)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn no_work_done_implies_all_counters_zero(result in arb_cleanup_result()) {
        if !result.any_work_done() {
            prop_assert_eq!(result.deleted_by_age, 0usize, "age must be 0 when no work");
            prop_assert_eq!(result.deleted_by_count, 0usize, "count must be 0 when no work");
            prop_assert_eq!(result.deleted_by_size, 0usize, "size must be 0 when no work");
            prop_assert_eq!(result.orphaned_checkpoints, 0usize, "orphan_cp must be 0 when no work");
            prop_assert_eq!(result.orphaned_pane_states, 0usize, "orphan_ps must be 0 when no work");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// 24. Age policy with max_age=0 is disabled (deletes nothing)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn zero_max_age_disables_age_policy(num_old in 1..10usize) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..num_old {
            let created = now - (i as i64 + 1) * 365 * 86_400_000; // very old
            insert_session(&conn, &format!("old-{}", i), created, true);
        }

        let config = SessionRetentionConfig {
            max_age_days: 0,         // disabled
            max_closed_sessions: 0,  // disabled
            max_total_size_mb: 0,    // disabled
            cleanup_interval_hours: 24,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();
        prop_assert_eq!(
            result.deleted_by_age, 0usize,
            "max_age_days=0 should disable age policy"
        );
        prop_assert_eq!(
            count_sessions(&conn), num_old as i64,
            "all sessions should remain"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 25. Size policy with max_total_size_mb=0 is disabled
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn zero_max_size_disables_size_policy(num_sessions in 1..10usize) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..num_sessions {
            let sid = format!("big-{}", i);
            insert_session(&conn, &sid, now - (i as i64) * 1000, true);
            insert_checkpoint(&conn, &sid, now, 1_000_000_000); // 1GB each
        }

        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0, // disabled
            cleanup_interval_hours: 24,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();
        prop_assert_eq!(
            result.deleted_by_size, 0usize,
            "max_total_size_mb=0 should disable size policy"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 26. Cleanup result components are non-overlapping
//     (a session deleted by age isn't counted again by count/size)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn deletion_categories_non_overlapping(
        num_closed in 0..20usize,
        max_age in 1..30u64,
        max_count in 1..10usize,
        max_size_mb in 1..50u64,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..num_closed {
            let sid = format!("sess-{}", i);
            let created = now - (i as i64 + 1) * 5 * 86_400_000;
            insert_session(&conn, &sid, created, true);
            insert_checkpoint(&conn, &sid, created, 10 * 1024 * 1024);
        }

        let initial_closed = count_closed_sessions(&conn) as usize;

        let config = SessionRetentionConfig {
            max_age_days: max_age,
            max_closed_sessions: max_count,
            max_total_size_mb: max_size_mb,
            cleanup_interval_hours: 24,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();

        // The sum of all categories must equal total_sessions_deleted
        let sum = result.deleted_by_age + result.deleted_by_count + result.deleted_by_size;
        prop_assert_eq!(
            sum,
            result.total_sessions_deleted(),
            "sum of categories must equal total"
        );

        // And the total must match what actually left the DB
        let final_closed = count_closed_sessions(&conn) as usize;
        let actual_deleted = initial_closed.saturating_sub(final_closed);
        prop_assert_eq!(
            result.total_sessions_deleted(),
            actual_deleted,
            "total_sessions_deleted must match actual DB removals"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// 27. Cleanup never errors on valid DB
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn cleanup_never_errors_on_valid_db(
        num_closed in 0..15usize,
        num_active in 0..5usize,
        config in arb_config(),
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        for i in 0..num_closed {
            let sid = format!("c-{}", i);
            let created = now - (i as i64 + 1) * 10 * 86_400_000;
            insert_session(&conn, &sid, created, true);
            insert_checkpoint(&conn, &sid, created, 1024 * 1024);
        }
        for i in 0..num_active {
            insert_session(
                &conn,
                &format!("a-{}", i),
                now - (i as i64) * 1000,
                false,
            );
        }

        let result = cleanup_sessions(&conn, &config);
        prop_assert!(result.is_ok(), "cleanup should not error on valid DB");
    }
}

// ────────────────────────────────────────────────────────────────────
// 28. Config defaults are sensible and stable
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn config_default_values_match_spec(_unused in Just(())) {
        let config = SessionRetentionConfig::default();
        prop_assert_eq!(config.max_age_days, 30u64, "default max_age_days");
        prop_assert_eq!(config.max_closed_sessions, 50usize, "default max_closed_sessions");
        prop_assert_eq!(config.max_total_size_mb, 500u64, "default max_total_size_mb");
        prop_assert_eq!(config.cleanup_interval_hours, 24u64, "default cleanup_interval_hours");
    }
}

// ────────────────────────────────────────────────────────────────────
// 29. Partial JSON config fills defaults for missing fields
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn partial_json_fills_defaults(max_age in 0..365u64) {
        let json = format!(r#"{{"max_age_days": {}}}"#, max_age);
        let decoded: SessionRetentionConfig = serde_json::from_str(&json).unwrap();
        let defaults = SessionRetentionConfig::default();

        prop_assert_eq!(decoded.max_age_days, max_age, "explicit field preserved");
        prop_assert_eq!(decoded.max_closed_sessions, defaults.max_closed_sessions, "missing field gets default");
        prop_assert_eq!(decoded.max_total_size_mb, defaults.max_total_size_mb, "missing field gets default");
        prop_assert_eq!(decoded.cleanup_interval_hours, defaults.cleanup_interval_hours, "missing field gets default");
    }
}

// ────────────────────────────────────────────────────────────────────
// 30. Mixed orphans: both checkpoint and pane_state orphans cleaned
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn mixed_orphan_cleanup(
        num_orphan_cp in 0..6usize,
        num_orphan_ps in 0..6usize,
    ) {
        let conn = make_test_db();
        let now = epoch_ms() as i64;

        // One valid session with checkpoint
        insert_session(&conn, "valid-1", now, true);
        let valid_cp = insert_checkpoint(&conn, "valid-1", now, 1024);
        insert_pane_state(&conn, valid_cp, 1);

        // Orphaned checkpoints
        for i in 0..num_orphan_cp {
            insert_orphaned_checkpoint(&conn, &format!("phantom-{}", i), now);
        }

        // Orphaned pane states
        for i in 0..num_orphan_ps {
            insert_orphaned_pane_state(&conn, 800_000 + i as i64, i as u64 + 500);
        }

        let config = SessionRetentionConfig {
            max_age_days: 0,
            max_closed_sessions: 0,
            max_total_size_mb: 0,
            cleanup_interval_hours: 0,
        };

        let result = cleanup_sessions(&conn, &config).unwrap();

        // Verify valid data preserved
        prop_assert_eq!(count_sessions(&conn), 1i64, "valid session preserved");
        prop_assert!(count_checkpoints(&conn) >= 1, "valid checkpoint preserved");
        prop_assert!(count_pane_states(&conn) >= 1, "valid pane state preserved");

        // Verify orphans cleaned
        prop_assert_eq!(
            result.orphaned_checkpoints, num_orphan_cp,
            "orphaned checkpoints cleaned"
        );
        prop_assert_eq!(
            result.orphaned_pane_states, num_orphan_ps,
            "orphaned pane states cleaned"
        );

        // Verify any_work_done consistency
        let expected_work = num_orphan_cp > 0 || num_orphan_ps > 0;
        prop_assert_eq!(
            result.any_work_done(), expected_work,
            "any_work_done should be {} for orphan_cp={}, orphan_ps={}",
            expected_work, num_orphan_cp, num_orphan_ps
        );
    }
}
