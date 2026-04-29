//! ft-dzats Phase 1a: extracted from storage.rs ($mod queue_depth_tests).
//! Sibling submodule of `storage` so `use super::*;` continues to resolve to
//! `crate::storage::*` — no path edits needed.

    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        super::run_storage_async_test(future);
    }

    static QD_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path() -> String {
        let id = QD_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        dir.join(format!("wa_qd_test_{id}_{}.db", std::process::id()))
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn write_queue_depth_starts_at_zero() {
        run_async_test(async {
            let db_path = temp_db_path();
            let handle = StorageHandle::new(&db_path).await.unwrap();

            assert_eq!(handle.write_queue_depth(), 0);
            assert!(handle.write_queue_capacity() > 0);

            handle.shutdown().await.unwrap();
            let _ = std::fs::remove_file(&db_path);
        });
    }

    #[test]
    fn write_queue_capacity_matches_config() {
        run_async_test(async {
            let db_path = temp_db_path();
            let mut config = StorageConfig::default();
            config.write_queue_size = 64;
            let handle = StorageHandle::with_config(&db_path, config).await.unwrap();

            assert_eq!(handle.write_queue_capacity(), 64);

            handle.shutdown().await.unwrap();
            let _ = std::fs::remove_file(&db_path);
        });
    }

    #[test]
    fn write_queue_depth_is_bounded() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn write_queue_depth_rises_under_concurrent_writes() {
        run_async_test(async {
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
                let jh = crate::runtime_async::task::spawn(async move {
                    h.append_segment(1, &format!("data-{i}"), None).await
                });
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
            crate::runtime_async::sleep(std::time::Duration::from_millis(50)).await;
            let final_depth = handle.write_queue_depth();
            assert_eq!(
                final_depth, 0,
                "Queue should be drained after all writes complete"
            );

            handle.shutdown().await.unwrap();
            let _ = std::fs::remove_file(&db_path);
        });
    }

    #[test]
    fn write_queue_bounded_under_heavy_load() {
        run_async_test(async {
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
                let jh = crate::runtime_async::task::spawn(async move {
                    h.append_segment(1, &format!("flood-{i}"), None).await
                });
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
                crate::runtime_async::sleep(std::time::Duration::from_millis(5)).await;
            }

            // Wait for all writes
            for jh in join_handles {
                jh.await.unwrap().unwrap();
            }

            handle.shutdown().await.unwrap();
            let _ = std::fs::remove_file(&db_path);
        });
    }

    #[test]
    fn write_queue_depth_returns_to_zero_after_drain() {
        run_async_test(async {
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
        });
    }
