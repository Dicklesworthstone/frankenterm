//! Async wrapper integration tests for the `agent_profiles` CRUD
//! surface (br-ft-dngp2 / ft-43lpu.cont).
//!
//! Slice 1 (`agent_profiles_sql.rs`) covers the sync primitives.
//! These tests exercise the writer-loop dispatch + `StorageHandle`
//! async + Cx-first paths end-to-end against a real SQLite-backed
//! `StorageHandle`. Each test spins up its own DB so the writer
//! thread + migration v25 (which creates the `agent_profiles`
//! table) run for every case.

use std::collections::HashMap;

use crate::agent_profiles::AgentProfile;
use crate::storage::StorageHandle;

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use crate::runtime_async::CompatRuntime;
    let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("build agent_profiles handle test runtime");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(future);
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(runtime);
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::runtime_async::clear_runtime_handle();
    }));
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

async fn fresh_storage(suffix: &str) -> (StorageHandle, std::path::PathBuf) {
    let db_path = std::env::temp_dir().join(format!(
        "wa_test_agent_profiles_{}_{}.db",
        suffix,
        std::process::id()
    ));
    let db_str = db_path.to_string_lossy().to_string();
    let storage = StorageHandle::new(&db_str)
        .await
        .expect("StorageHandle::new for agent_profiles tests");
    (storage, db_path)
}

async fn cleanup(storage: StorageHandle, db_path: &std::path::Path) {
    let _ = storage.shutdown().await;
    let _ = std::fs::remove_file(db_path);
    let lossy = db_path.to_string_lossy();
    let _ = std::fs::remove_file(format!("{lossy}-wal"));
    let _ = std::fs::remove_file(format!("{lossy}-shm"));
}

fn synth_profile(name: &str, role: &str) -> AgentProfile {
    let mut env = HashMap::new();
    env.insert("EDITOR".to_string(), "vim".to_string());
    let mut metadata = HashMap::new();
    metadata.insert("team".to_string(), "platform".to_string());
    AgentProfile {
        name: name.to_string(),
        role: role.to_string(),
        tags: vec!["work".to_string(), "rust".to_string()],
        shell: "/bin/zsh".to_string(),
        command: Some("nvim".to_string()),
        env,
        metadata,
        created_at_ms: 1_700_000_000_000,
        updated_at_ms: 1_700_000_000_000,
    }
}

/// br-ft-dngp2: insert + get round-trips through the async
/// writer-thread dispatch identically to the sync slice-1
/// primitives. Every JSON-encoded TEXT column survives the
/// trip byte-for-byte.
#[test]
fn insert_and_get_async_roundtrip_preserves_every_field() {
    run_async_test(async {
        let (storage, db_path) = fresh_storage("roundtrip").await;
        let p = synth_profile("alice", "ops");

        let returned = storage
            .insert_agent_profile(p.clone())
            .await
            .expect("insert");
        assert_eq!(returned, "alice");

        let fetched = storage
            .get_agent_profile("alice")
            .await
            .expect("get")
            .expect("row exists");
        assert_eq!(p, fetched);

        cleanup(storage, &db_path).await;
    });
}

/// br-ft-dngp2: get on a missing name returns Ok(None), not
/// an error. Async surface mirrors the sync slice-1 contract.
#[test]
fn get_missing_name_returns_none() {
    run_async_test(async {
        let (storage, db_path) = fresh_storage("missing").await;
        let out = storage.get_agent_profile("nobody").await.unwrap();
        assert!(out.is_none());
        cleanup(storage, &db_path).await;
    });
}

/// br-ft-dngp2: list with `None` filter returns every row
/// ordered by name ASC; with `Some(role)` it restricts to
/// that role.
#[test]
fn list_filters_by_role_and_orders_by_name() {
    run_async_test(async {
        let (storage, db_path) = fresh_storage("list").await;

        // Insert deliberately out of name order with mixed roles.
        for (name, role) in [
            ("zebra", "ops"),
            ("alpha", "dev"),
            ("midline", "ops"),
        ] {
            storage
                .insert_agent_profile(synth_profile(name, role))
                .await
                .expect("insert");
        }

        let all = storage.list_agent_profiles(None).await.expect("list all");
        let names: Vec<_> = all.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["alpha", "midline", "zebra"]);

        let ops = storage
            .list_agent_profiles(Some("ops"))
            .await
            .expect("list ops");
        let ops_names: Vec<_> = ops.iter().map(|p| p.name.clone()).collect();
        assert_eq!(ops_names, vec!["midline", "zebra"]);

        cleanup(storage, &db_path).await;
    });
}

/// br-ft-dngp2: delete returns `true` when a row was removed,
/// `false` for an absent name. After delete, get returns None.
#[test]
fn delete_returns_hit_miss_and_removes_row() {
    run_async_test(async {
        let (storage, db_path) = fresh_storage("delete").await;

        storage
            .insert_agent_profile(synth_profile("bob", "dev"))
            .await
            .unwrap();

        let removed = storage.delete_agent_profile("bob").await.unwrap();
        assert!(removed, "delete must hit");

        let absent = storage.delete_agent_profile("bob").await.unwrap();
        assert!(!absent, "second delete must miss");

        let after = storage.get_agent_profile("bob").await.unwrap();
        assert!(after.is_none(), "row must be gone after delete");

        cleanup(storage, &db_path).await;
    });
}

/// br-ft-dngp2: validation runs before the insert touches
/// SQLite. An empty `name` (the simplest invariant violation)
/// surfaces as `StorageError::Database` — the async surface
/// collapses the typed `AgentProfileSqlError::Invalid` into the
/// catch-all variant with the operator-readable message
/// `"agent_profiles: ..."`.
#[test]
fn insert_validation_failure_is_reported_via_storage_error() {
    run_async_test(async {
        let (storage, db_path) = fresh_storage("validate").await;
        let mut bad = synth_profile("doesnt-matter", "ops");
        bad.name = String::new();
        let err = storage.insert_agent_profile(bad).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("agent_profiles"),
            "error message must namespace into agent_profiles: {msg}"
        );

        // No row landed.
        let listed = storage.list_agent_profiles(None).await.unwrap();
        assert!(listed.is_empty(), "validation rejects must not insert");

        cleanup(storage, &db_path).await;
    });
}

/// br-ft-dngp2: duplicate name surfaces as `StorageError::Database`
/// wrapping the SQLite UNIQUE constraint. Caller's "replace vs
/// insert" logic can match on the message.
#[test]
fn duplicate_insert_is_reported_via_storage_error() {
    run_async_test(async {
        let (storage, db_path) = fresh_storage("dupe").await;
        storage
            .insert_agent_profile(synth_profile("twin", "ops"))
            .await
            .expect("first insert");
        let err = storage
            .insert_agent_profile(synth_profile("twin", "ops"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("agent_profiles"),
            "duplicate error must namespace into agent_profiles: {msg}"
        );
        cleanup(storage, &db_path).await;
    });
}

/// br-ft-dngp2: Cx-first sibling exercises the same dispatch
/// path as the legacy entry point. Confirms the explicit
/// `cx.checkpoint()` pre-flight + `send_with_cx` ordering
/// matches the documented contract for the four new methods.
#[test]
fn cx_first_round_trip_matches_legacy() {
    run_async_test(async {
        let (storage, db_path) = fresh_storage("cx_first").await;
        let cx = crate::cx::for_request();

        let p = synth_profile("cx-charlie", "qa");
        let returned = storage
            .insert_agent_profile_with_cx(&cx, p.clone())
            .await
            .expect("insert_with_cx");
        assert_eq!(returned, "cx-charlie");

        let fetched = storage
            .get_agent_profile_with_cx(&cx, "cx-charlie")
            .await
            .expect("get_with_cx")
            .expect("row exists");
        assert_eq!(p, fetched);

        let listed = storage
            .list_agent_profiles_with_cx(&cx, Some("qa"))
            .await
            .expect("list_with_cx");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "cx-charlie");

        let removed = storage
            .delete_agent_profile_with_cx(&cx, "cx-charlie")
            .await
            .expect("delete_with_cx");
        assert!(removed);

        cleanup(storage, &db_path).await;
    });
}
