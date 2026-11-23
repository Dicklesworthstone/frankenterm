//! ft-u6fba Phase 1b: extracted from storage.rs (mod reservation_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to
//! `crate::storage::*`.

use super::*;
use rusqlite::{Connection, params};

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
        let pane_id = u64_to_i64(pid, "pane_id").unwrap();
        conn.execute(
            "INSERT INTO panes (pane_id, title, cwd, observed, first_seen_at, last_seen_at)
                 VALUES (?1, 'test', '/tmp', 1, 1000, 1000)",
            params![pane_id],
        )
        .unwrap();
    }
    conn
}

fn create_reservation_sync(
    conn: &mut Connection,
    pane_id: u64,
    owner_kind: &str,
    owner_id: &str,
    reason: Option<&str>,
    ttl_ms: i64,
) -> Result<PaneReservation> {
    with_test_storage_backend(conn, |backend| {
        create_reservation_backend(
            backend,
            pane_id,
            owner_kind,
            owner_id,
            reason,
            ttl_ms,
            &PaneReservationConfig::default(),
        )
    })
}

fn release_reservation_sync(conn: &mut Connection, reservation_id: i64) -> Result<bool> {
    with_test_storage_backend(conn, |backend| {
        release_reservation_backend(backend, reservation_id)
    })
}

fn get_active_reservation_sync(
    conn: &mut Connection,
    pane_id: u64,
) -> Result<Option<PaneReservation>> {
    with_test_storage_backend(conn, |backend| {
        get_active_reservation_backend(backend, pane_id)
    })
}

fn list_active_reservations_sync(conn: &mut Connection) -> Result<Vec<PaneReservation>> {
    with_test_storage_backend(conn, list_active_reservations_backend)
}

fn expire_stale_reservations_sync(conn: &mut Connection) -> Result<usize> {
    with_test_storage_backend(conn, expire_stale_reservations_backend)
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
    assert_eq!(cfg.default_ttl_ms, 5 * 60 * 1000);
    assert_eq!(cfg.max_ttl_ms, 60 * 60 * 1000);
    let parsed = crate::config::ReservationConfig::default();
    assert_eq!(
        cfg.default_ttl_ms,
        i64::try_from(parsed.default_ttl_secs * 1000).unwrap()
    );
    assert_eq!(
        cfg.max_ttl_ms,
        i64::try_from(parsed.max_ttl_secs * 1000).unwrap()
    );
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
    let mut conn = setup_db();
    let r =
        create_reservation_sync(&mut conn, 1, "workflow", "wf-1", Some("testing"), 60_000).unwrap();

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
    let mut conn = setup_db();
    let r = create_reservation_sync(&mut conn, 1, "agent", "agent-x", None, 30_000).unwrap();

    assert!(r.reason.is_none());
    assert_eq!(r.owner_kind, "agent");
}

#[test]
fn create_reservation_conflict_with_active() {
    let mut conn = setup_db();

    // First reservation succeeds
    let _r1 = create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, 600_000).unwrap();

    // Second reservation on same pane should fail
    let r2 = create_reservation_sync(&mut conn, 1, "workflow", "wf-2", None, 60_000);
    assert!(r2.is_err());
    let err = r2.unwrap_err();
    let err_msg = err.to_string();
    assert!(err_msg.contains("already has active reservation"));
    assert!(matches!(
        err,
        crate::Error::Storage(StorageError::ReservationConflict {
            pane_id: 1,
            existing_id: _
        })
    ));
}

#[test]
fn create_reservation_allowed_after_release() {
    let mut conn = setup_db();

    let r1 = create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, 600_000).unwrap();
    release_reservation_sync(&mut conn, r1.id).unwrap();

    // Now a new reservation should succeed
    let r2 = create_reservation_sync(&mut conn, 1, "workflow", "wf-2", None, 60_000);
    assert!(r2.is_ok());
}

#[test]
fn create_reservation_allowed_on_different_panes() {
    let mut conn = setup_db_with_panes(&[1, 2]);

    let r1 = create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, 600_000);
    let r2 = create_reservation_sync(&mut conn, 2, "workflow", "wf-2", None, 600_000);

    assert!(r1.is_ok());
    assert!(r2.is_ok());
}

// =========================================================================
// release_reservation_sync tests
// =========================================================================

#[test]
fn release_reservation_sets_released() {
    let mut conn = setup_db();
    let r = create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, 600_000).unwrap();

    let released = release_reservation_sync(&mut conn, r.id).unwrap();
    assert!(released);

    // Verify status changed in DB
    let active = get_active_reservation_sync(&mut conn, 1).unwrap();
    assert!(active.is_none());
}

#[test]
fn release_nonexistent_returns_false() {
    let mut conn = setup_db();
    let released = release_reservation_sync(&mut conn, 9999).unwrap();
    assert!(!released);
}

#[test]
fn release_already_released_returns_false() {
    let mut conn = setup_db();
    let r = create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, 600_000).unwrap();

    assert!(release_reservation_sync(&mut conn, r.id).unwrap());
    // Second release is a no-op
    assert!(!release_reservation_sync(&mut conn, r.id).unwrap());
}

// =========================================================================
// get_active_reservation_sync tests
// =========================================================================

#[test]
fn get_active_reservation_returns_some() {
    let mut conn = setup_db();
    let created =
        create_reservation_sync(&mut conn, 1, "workflow", "wf-1", Some("reason"), 600_000).unwrap();

    let fetched = get_active_reservation_sync(&mut conn, 1).unwrap();
    assert!(fetched.is_some());
    let f = fetched.unwrap();
    assert_eq!(f.id, created.id);
    assert_eq!(f.owner_id, "wf-1");
    assert_eq!(f.reason.as_deref(), Some("reason"));
}

#[test]
fn get_active_reservation_returns_none_for_unreserved_pane() {
    let mut conn = setup_db();
    let fetched = get_active_reservation_sync(&mut conn, 1).unwrap();
    assert!(fetched.is_none());
}

#[test]
fn get_active_reservation_returns_none_after_release() {
    let mut conn = setup_db();
    let r = create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, 600_000).unwrap();
    release_reservation_sync(&mut conn, r.id).unwrap();

    let fetched = get_active_reservation_sync(&mut conn, 1).unwrap();
    assert!(fetched.is_none());
}

// =========================================================================
// list_active_reservations_sync tests
// =========================================================================

#[test]
fn list_active_empty() {
    let mut conn = setup_db();
    let list = list_active_reservations_sync(&mut conn).unwrap();
    assert!(list.is_empty());
}

#[test]
fn list_active_multiple_panes() {
    let mut conn = setup_db_with_panes(&[1, 2, 3]);

    create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, 600_000).unwrap();
    create_reservation_sync(&mut conn, 2, "agent", "agent-a", None, 600_000).unwrap();
    create_reservation_sync(&mut conn, 3, "manual", "user-1", None, 600_000).unwrap();

    let list = list_active_reservations_sync(&mut conn).unwrap();
    assert_eq!(list.len(), 3);
}

#[test]
fn list_active_excludes_released() {
    let mut conn = setup_db_with_panes(&[1, 2]);

    let r1 = create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, 600_000).unwrap();
    create_reservation_sync(&mut conn, 2, "workflow", "wf-2", None, 600_000).unwrap();

    release_reservation_sync(&mut conn, r1.id).unwrap();

    let list = list_active_reservations_sync(&mut conn).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].pane_id, 2);
}

// =========================================================================
// expire_stale_reservations_sync tests
// =========================================================================

#[test]
fn expire_stale_none_to_expire() {
    let mut conn = setup_db();
    create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, 600_000).unwrap();

    let expired = expire_stale_reservations_sync(&mut conn).unwrap();
    assert_eq!(expired, 0);
}

#[test]
fn expire_stale_expires_past_ttl() {
    let mut conn = setup_db();

    // Manually insert a reservation with expires_at in the past
    let past = now_ms() - 10_000;
    conn.execute(
            "INSERT INTO pane_reservations (pane_id, owner_kind, owner_id, reason, created_at, expires_at, status)
             VALUES (1, 'workflow', 'wf-old', NULL, ?1, ?2, 'active')",
            params![past - 60_000, past],
        )
        .unwrap();

    let expired = expire_stale_reservations_sync(&mut conn).unwrap();
    assert_eq!(expired, 1);

    // Should no longer appear as active
    let active = get_active_reservation_sync(&mut conn, 1).unwrap();
    assert!(active.is_none());
}

#[test]
fn expire_stale_does_not_touch_valid() {
    let mut conn = setup_db_with_panes(&[1, 2]);

    // One valid, one expired
    create_reservation_sync(&mut conn, 1, "workflow", "wf-valid", None, 600_000).unwrap();

    let past = now_ms() - 5_000;
    conn.execute(
            "INSERT INTO pane_reservations (pane_id, owner_kind, owner_id, reason, created_at, expires_at, status)
             VALUES (2, 'workflow', 'wf-old', NULL, ?1, ?2, 'active')",
            params![past - 60_000, past],
        )
        .unwrap();

    let expired = expire_stale_reservations_sync(&mut conn).unwrap();
    assert_eq!(expired, 1);

    // Valid one should still be active
    let active = list_active_reservations_sync(&mut conn).unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].owner_id, "wf-valid");
}

// =========================================================================
// Round-trip / integration tests
// =========================================================================

#[test]
fn reserve_release_reserve_round_trip() {
    let mut conn = setup_db();

    // Create first reservation
    let r1 =
        create_reservation_sync(&mut conn, 1, "workflow", "wf-1", Some("first"), 600_000).unwrap();
    assert!(get_active_reservation_sync(&mut conn, 1).unwrap().is_some());

    // Release it
    assert!(release_reservation_sync(&mut conn, r1.id).unwrap());
    assert!(get_active_reservation_sync(&mut conn, 1).unwrap().is_none());

    // Create second reservation on same pane
    let r2 =
        create_reservation_sync(&mut conn, 1, "agent", "agent-b", Some("second"), 300_000).unwrap();
    let active = get_active_reservation_sync(&mut conn, 1).unwrap().unwrap();
    assert_eq!(active.id, r2.id);
    assert_eq!(active.owner_kind, "agent");
    assert_eq!(active.owner_id, "agent-b");
}

#[test]
fn expired_reservation_allows_new_creation() {
    let mut conn = setup_db();

    // Insert an already-expired reservation directly
    let past = now_ms() - 10_000;
    conn.execute(
            "INSERT INTO pane_reservations (pane_id, owner_kind, owner_id, reason, created_at, expires_at, status)
             VALUES (1, 'workflow', 'wf-expired', NULL, ?1, ?2, 'active')",
            params![past - 60_000, past],
        )
        .unwrap();

    // New reservation should succeed because the existing one is expired
    let r = create_reservation_sync(&mut conn, 1, "workflow", "wf-new", None, 60_000);
    assert!(r.is_ok());
}

#[test]
fn ttl_determines_expiry() {
    let mut conn = setup_db();
    let ttl = 120_000i64; // 2 minutes
    let r = create_reservation_sync(&mut conn, 1, "workflow", "wf-1", None, ttl).unwrap();

    // expires_at should be approximately created_at + ttl
    let diff = r.expires_at - r.created_at;
    assert_eq!(diff, ttl);
}

#[test]
fn create_reservation_clamps_unrepresentable_ttl_to_config_bounds() {
    let mut conn = setup_db_with_panes(&[1, 2]);
    let below_min = create_reservation_sync(&mut conn, 1, "workflow", "wf-min", None, -1).unwrap();
    let above_max =
        create_reservation_sync(&mut conn, 2, "workflow", "wf-max", None, i64::MAX).unwrap();

    assert_eq!(below_min.expires_at - below_min.created_at, 1_000);
    assert_eq!(
        above_max.expires_at - above_max.created_at,
        PaneReservationConfig::default().max_ttl_ms
    );
}

#[test]
fn configured_reservation_expiry_overflow_leaves_no_reservation() {
    let mut conn = setup_db();
    let policy = PaneReservationConfig {
        default_ttl_ms: 1_000,
        max_ttl_ms: i64::MAX,
    };
    let result = with_test_storage_backend(&mut conn, |backend| {
        create_reservation_backend(backend, 1, "agent", "overflow", None, i64::MAX, &policy)
    });
    let error = result.expect_err("an expiry past the timestamp range must be rejected");
    assert!(error.to_string().contains("overflow"));
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM pane_reservations", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0,
        "a rejected expiry must not reserve the pane"
    );
    assert!(create_reservation_sync(&mut conn, 1, "agent", "valid", None, 1_000).is_ok());
}

fn reservation_test_pane(pane_id: u64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: None,
        cwd: None,
        tty_name: None,
        first_seen_at: 1_000,
        last_seen_at: 1_000,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

#[test]
fn configured_handle_persists_default_explicit_and_clamped_reservation_ttls() {
    super::run_storage_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("configured-reservations.db");
        let app: crate::config::Config =
            toml::from_str("[safety.reservations]\ndefault_ttl_secs = 7\nmax_ttl_secs = 11\n")
                .unwrap();
        let cx = crate::cx::for_testing();
        let handle = StorageHandle::with_config_with_cx(
            &cx,
            db_path.to_str().unwrap(),
            StorageConfig::from(&app),
        )
        .await
        .unwrap();
        let cloned = handle.clone();
        assert_eq!(cloned.reservation_config().default_ttl_ms, 7_000);
        assert_eq!(cloned.reservation_config().max_ttl_ms, 11_000);

        let cases = [
            (1, cloned.reservation_config().default_ttl_ms, 7_000),
            (2, 9_000, 9_000),
            (3, i64::MAX, 11_000),
            (4, 0, 1_000),
        ];
        for (pane_id, requested, expected) in cases {
            handle
                .upsert_pane_with_cx(&cx, reservation_test_pane(pane_id))
                .await
                .unwrap();
            let reservation = cloned
                .create_reservation_with_cx(&cx, pane_id, "agent", "configured", None, requested)
                .await
                .unwrap();
            assert_eq!(reservation.expires_at - reservation.created_at, expected);
        }
        handle.shutdown_with_cx(&cx).await.unwrap();
        drop(cloned);
        drop(handle);

        // Read the committed database independently of the returned receipts.
        // In particular, this fails if only callers clamp but the writer still
        // constructs its former hard-coded four-hour policy.
        let conn = Connection::open(&db_path).unwrap();
        for (pane_id, _, expected) in cases {
            let stored_ttl: i64 = conn
                .query_row(
                    "SELECT expires_at - created_at FROM pane_reservations WHERE pane_id = ?1",
                    [i64::try_from(pane_id).unwrap()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(stored_ttl, expected, "persisted TTL for pane {pane_id}");
        }
    });
}

#[test]
fn reservation_policy_is_owned_by_each_storage_handle() {
    super::run_storage_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        let first_path = dir.path().join("first.db");
        let second_path = dir.path().join("second.db");
        let first = StorageHandle::with_config(
            first_path.to_str().unwrap(),
            StorageConfig {
                reservation_config: PaneReservationConfig {
                    default_ttl_ms: 5_000,
                    max_ttl_ms: 13_000,
                },
                ..StorageConfig::default()
            },
        )
        .await
        .unwrap();
        let second = StorageHandle::with_config(
            second_path.to_str().unwrap(),
            StorageConfig {
                reservation_config: PaneReservationConfig {
                    default_ttl_ms: 17_000,
                    max_ttl_ms: 29_000,
                },
                ..StorageConfig::default()
            },
        )
        .await
        .unwrap();

        for (handle, expected_default, expected_max) in
            [(&first, 5_000, 13_000), (&second, 17_000, 29_000)]
        {
            assert_eq!(handle.reservation_config().default_ttl_ms, expected_default);
            handle.upsert_pane(reservation_test_pane(1)).await.unwrap();
            let reservation = handle
                .create_reservation(1, "agent", "same-owner", None, 60_000)
                .await
                .unwrap();
            assert_eq!(
                reservation.expires_at - reservation.created_at,
                expected_max
            );
        }
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();
    });
}

#[test]
fn invalid_reservation_policy_is_rejected_before_opening_storage() {
    super::run_storage_async_test(async {
        let dir = tempfile::tempdir().unwrap();
        for (index, (default_ttl_ms, max_ttl_ms)) in [
            (0, 60_000),
            (999, 60_000),
            (-1, 60_000),
            (1_000, 999),
            (2_000, 1_000),
            (1_000, -1),
        ]
        .into_iter()
        .enumerate()
        {
            let parent = dir.path().join(format!("invalid-policy-{index}"));
            let db_path = parent.join("storage.db");
            let config = StorageConfig {
                reservation_config: PaneReservationConfig {
                    default_ttl_ms,
                    max_ttl_ms,
                },
                ..StorageConfig::default()
            };
            let cx = crate::cx::for_testing();
            let result = if index % 2 == 0 {
                StorageHandle::with_config(db_path.to_str().unwrap(), config).await
            } else {
                StorageHandle::with_config_with_cx(&cx, db_path.to_str().unwrap(), config).await
            };
            match result {
                Ok(handle) => {
                    handle.shutdown().await.unwrap();
                    panic!(
                        "invalid reservation policy was accepted: {default_ttl_ms}/{max_ttl_ms}"
                    );
                }
                Err(error) => assert!(
                    error.to_string().to_lowercase().contains("reservation"),
                    "invalid policy must report its reservation setting: {error}"
                ),
            }
            assert!(!db_path.exists(), "invalid policy created the database");
            assert!(
                !parent.exists(),
                "invalid policy created the parent directory"
            );
        }
    });
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
