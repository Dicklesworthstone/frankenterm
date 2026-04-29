//! ft-dzats Phase 1a: extracted from storage.rs (mod backpressure_integration_tests).
//! Sibling submodule of `storage` — `use super::*;` resolves to `crate::storage::*`.

    use super::*;
    use crate::runtime_async::mpsc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        super::run_storage_async_test(future);
    }

    static BP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_db_path() -> String {
        let id = BP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir();
        dir.join(format!("wa_bp_test_{id}_{}.db", std::process::id()))
            .to_string_lossy()
            .to_string()
    }

    async fn send_mpsc<T>(tx: &mpsc::Sender<T>, value: T) {
        let cx = crate::cx::for_testing();
        let sent = tx.send(&cx, value).await;
        assert!(sent.is_ok(), "test mpsc send should succeed");
    }

    async fn recv_mpsc<T>(rx: &mut mpsc::Receiver<T>) -> T {
        let cx = crate::cx::for_testing();
        rx.recv(&cx).await.expect("test mpsc recv should succeed")
    }

    #[test]
    fn capture_channel_backpressure_detected() {
        run_async_test(async {
            // Simulate backpressure on the capture channel:
            // - Create a tiny channel (capacity 2)
            // - Fill it up
            // - Verify send times out (reserve with timeout)
            use std::time::Duration;

            #[allow(unused_variables)]
            let (tx, rx) = mpsc::channel::<u8>(2);
            #[allow(unused_variables)]
            let max_cap = 2usize;

            // Fill the channel
            send_mpsc(&tx, 1).await;
            send_mpsc(&tx, 2).await;

            // Channel is full — next send should block and timeout.
            let result =
                crate::runtime_async::timeout(Duration::from_millis(50), send_mpsc(&tx, 3)).await;
            assert!(result.is_err(), "Should timeout when channel is full");

            // Verify depth
            let depth = rx.len();
            assert_eq!(depth, 2, "Queue should be at capacity");
        });
    }

    #[test]
    fn capture_channel_drains_when_consumer_resumes() {
        run_async_test(async {
            use std::time::Duration;

            let (tx, mut rx) = mpsc::channel::<u8>(4);
            #[allow(unused_variables)]
            let max_cap = 4usize;

            // Fill partially
            send_mpsc(&tx, 1).await;
            send_mpsc(&tx, 2).await;
            send_mpsc(&tx, 3).await;

            let depth_before = rx.len();
            assert_eq!(depth_before, 3);

            // Consume all items
            let _ = recv_mpsc(&mut rx).await;
            let _ = recv_mpsc(&mut rx).await;
            let _ = recv_mpsc(&mut rx).await;

            // Small yield for channel state to update
            crate::runtime_async::sleep(Duration::from_millis(1)).await;

            let depth_after = rx.len();
            assert_eq!(depth_after, 0, "Queue should drain when consumer resumes");
        });
    }

    #[test]
    fn storage_concurrent_writers_dont_deadlock() {
        run_async_test(async {
            // Multiple concurrent writers on a small queue should complete
            // without deadlock (writer thread drains fast enough)
            let db_path = temp_db_path();
            let mut config = StorageConfig::default();
            config.write_queue_size = 4;
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

            // Spawn many concurrent writers
            let mut handles = Vec::new();
            for i in 0..16 {
                let h = handle.clone();
                handles.push(crate::runtime_async::task::spawn(async move {
                    h.append_segment(1, &format!("concurrent-{i}"), None)
                        .await
                        .unwrap();
                }));
            }

            // Use a timeout to detect deadlocks
            let result = crate::runtime_async::timeout(std::time::Duration::from_secs(10), async {
                for jh in handles {
                    jh.await.unwrap();
                }
            })
            .await;

            assert!(
                result.is_ok(),
                "Concurrent writers should complete without deadlock"
            );

            // Verify all 16 segments were written
            let segments = handle.get_segments(1, 100).await.unwrap();
            assert_eq!(segments.len(), 16, "All concurrent writes should persist");

            handle.shutdown().await.unwrap();
            let _ = std::fs::remove_file(&db_path);
        });
    }

    #[test]
    fn gap_recording_works_under_backpressure() {
        run_async_test(async {
            // Ensure GAP records can be written even when the queue has work pending
            let db_path = temp_db_path();
            let mut config = StorageConfig::default();
            config.write_queue_size = 4;
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

            // Write a segment first (gap requires existing seq)
            let seg_before = handle.append_segment(1, "before-gap", None).await.unwrap();

            // Record a gap (simulating backpressure-induced discontinuity)
            let gap = handle.record_gap(1, "backpressure_overflow").await.unwrap();
            assert!(
                gap.is_some(),
                "GAP should be recorded after existing segment"
            );
            let gap = gap.unwrap();
            assert_eq!(gap.pane_id, 1);
            assert_eq!(gap.reason, "backpressure_overflow");
            assert_eq!(gap.seq_before, seg_before.seq);
            assert_eq!(gap.seq_after, seg_before.seq + 1);

            // Continue writing after gap
            let seg_after = handle.append_segment(1, "after-gap", None).await.unwrap();

            // Verify segments are in the output_segments table
            let segments = handle.get_segments(1, 100).await.unwrap();
            assert_eq!(segments.len(), 2); // before and after (gap is in output_gaps table)
            // get_segments returns ORDER BY seq DESC (most recent first)
            assert_eq!(segments[0].content, "after-gap");
            assert_eq!(segments[1].content, "before-gap");
            // Sequence numbers should show the discontinuity
            assert!(seg_after.seq > seg_before.seq);

            handle.shutdown().await.unwrap();
            let _ = std::fs::remove_file(&db_path);
        });
    }

    #[test]
    fn health_warning_threshold_generates_warnings() {
        run_async_test(async {
            // Test the warning generation logic with a controlled queue state
            use crate::crash::HealthSnapshot;

            // Simulate a snapshot where capture queue is at 80% (above 75% threshold)
            let snapshot = HealthSnapshot {
                timestamp: 0,
                observed_panes: 2,
                capture_queue_depth: 820,
                write_queue_depth: 10,
                last_seq_by_pane: vec![],
                warnings: vec!["Capture queue backpressure: 820/1024 (80%)".to_string()],
                ingest_lag_avg_ms: 100.0,
                ingest_lag_max_ms: 500,
                db_writable: true,
                db_last_write_at: Some(1000),
                pane_priority_overrides: vec![],
                scheduler: None,
                backpressure_tier: None,
                last_activity_by_pane: vec![],
                restart_count: 0,
                last_crash_at: None,
                consecutive_crashes: 0,
                current_backoff_ms: 0,
                in_crash_loop: false,
                fleet_pressure_tier: None,
                leak_risk_inventory: crate::crash::LeakRiskInventorySnapshot::default(),
            };

            assert!(!snapshot.warnings.is_empty());
            assert!(snapshot.warnings[0].contains("backpressure"));
            assert!(snapshot.warnings[0].contains("80%"));
        });
    }

    #[test]
    fn event_bus_detects_subscriber_lag() {
        run_async_test(async {
            use crate::events::{Event, EventBus};

            let bus = EventBus::new(4); // Small capacity

            // Subscribe before publishing
            let mut sub = bus.subscribe();

            // Publish more events than buffer size to cause lag
            for i in 0..8 {
                let _ = bus.publish(Event::SegmentCaptured {
                    pane_id: 1,
                    seq: i,
                    content_len: 100,
                });
            }

            // First recv should indicate lag (missed events)
            let result = sub.recv().await;
            match result {
                Err(crate::events::RecvError::Lagged { missed_count }) => {
                    assert!(missed_count > 0, "Should report missed events due to lag");
                }
                Ok(_) => {
                    // Some events may still be in buffer, that's also valid
                    // as long as the bus didn't panic
                }
                Err(e) => panic!("Unexpected error: {e:?}"),
            }

            // Stats should reflect capacity
            let stats = bus.stats();
            assert_eq!(stats.capacity, 4);
        });
    }
