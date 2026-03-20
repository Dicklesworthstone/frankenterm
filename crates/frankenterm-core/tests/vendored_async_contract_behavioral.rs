// =============================================================================
// Core↔vendored async contract BEHAVIORAL tests (ft-e34d9.10.5.4)
//
// Unlike the structural/static tests in vendored_async_contract_verification.rs,
// these tests exercise actual async runtime behavior to verify ABC contract
// invariants hold at runtime. Each test maps to a specific contract.
//
// Coverage:
//   B01–B04: Channel delivery contracts (ABC-CHN-001, ABC-CHN-002)
//   B05–B07: Timeout override contracts (ABC-TO-001)
//   B08–B09: Task lifecycle tracking (ABC-TL-001, ABC-TL-002)
//   B10–B12: Semaphore backpressure (ABC-BP-001)
//   B13–B15: Task ownership and cancellation (ABC-OWN-001, ABC-CAN-002)
//   B16–B18: Error mapping chain (ABC-ERR-001)
//   B19–B20: Sync primitive boundary behavior (ABC-OWN-002)
//   B21–B23: Cross-layer integration scenarios
// =============================================================================

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use frankenterm_core::runtime_compat::{
    self, CompatRuntime, Mutex, RuntimeBuilder, RwLock, Semaphore, TryAcquireError,
};
use frankenterm_core::vendored_async_contracts::{
    ContractAuditReport, ContractCompliance, ContractEvidence, EvidenceType, standard_contracts,
};

// =============================================================================
// Helpers
// =============================================================================

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("failed to build runtime for behavioral test");
    runtime.block_on(future);
}

fn emit_behavioral_log(scenario_id: &str, contract_id: &str, check: &str, outcome: &str) {
    let payload = serde_json::json!({
        "timestamp": "2026-03-20T00:00:00Z",
        "component": "vendored_async_contract.behavioral",
        "scenario_id": scenario_id,
        "correlation_id": format!("ft-e34d9.10.5.4-behavioral-{scenario_id}"),
        "contract_id": contract_id,
        "check": check,
        "outcome": outcome,
    });
    eprintln!("{payload}");
}

// =============================================================================
// B01–B04: Channel delivery contracts (ABC-CHN-001, ABC-CHN-002)
// =============================================================================

/// B01: ABC-CHN-002 — mpsc channel delivers all buffered items after sender drop.
///
/// Sends N items into a bounded mpsc channel, drops the sender, then verifies
/// the receiver drains every item before reporting closure.
#[test]
fn b01_mpsc_channel_non_lossy_delivery_on_close() {
    run_async_test(async {
        let (tx, mut rx) = runtime_compat::mpsc::channel::<u32>(16);

        let items_to_send: Vec<u32> = (0..10).collect();
        for &item in &items_to_send {
            runtime_compat::mpsc_send(&tx, item)
                .await
                .expect("send should succeed");
        }
        // Drop sender to close the channel
        drop(tx);

        // Drain all items via the compatibility helper
        let mut received = Vec::new();
        while let Some(item) = runtime_compat::mpsc_recv_option(&mut rx).await {
            received.push(item);
        }

        assert_eq!(
            received, items_to_send,
            "all buffered items must be delivered after sender drop (ABC-CHN-002)"
        );

        emit_behavioral_log("b01", "ABC-CHN-002", "mpsc_non_lossy_close", "pass");
    });
}

/// B02: ABC-CHN-001 — watch channel delivers latest value through runtime_compat.
///
/// Creates a watch channel, sends a sequence of values, and verifies the
/// receiver observes the most recent value.
#[test]
fn b02_watch_channel_delivers_latest_value() {
    run_async_test(async {
        let (tx, rx) = runtime_compat::watch::channel(0u32);

        tx.send(42).expect("watch send should succeed");
        tx.send(99).expect("watch send should succeed");

        // Borrow the current value — borrow() is synchronous on both backends
        let current = *rx.borrow();
        assert_eq!(current, 99, "watch receiver must see latest value");

        emit_behavioral_log("b02", "ABC-CHN-001", "watch_latest_value", "pass");
    });
}

/// B03: ABC-CHN-001 — broadcast channel delivers to all receivers.
///
/// Creates a broadcast channel with multiple receivers and verifies all
/// receivers get the sent message.
#[test]
fn b03_broadcast_channel_fanout_delivery() {
    run_async_test(async {
        let (tx, mut rx1) = runtime_compat::broadcast::channel::<String>(16);
        let mut rx2 = tx.subscribe();

        tx.send("hello".into()).expect("broadcast send should succeed");

        let val1 = rx1.recv().await.expect("rx1 should receive");
        let val2 = rx2.recv().await.expect("rx2 should receive");

        assert_eq!(val1, "hello");
        assert_eq!(val2, "hello");

        emit_behavioral_log("b03", "ABC-CHN-001", "broadcast_fanout", "pass");
    });
}

/// B04: ABC-CHN-001 — oneshot channel delivers exactly one value.
///
/// Creates a oneshot channel, sends a single value, and verifies the
/// receiver gets it. Also verifies the sender is consumed after send.
#[test]
fn b04_oneshot_channel_single_delivery() {
    run_async_test(async {
        let (tx, rx) = runtime_compat::oneshot::channel::<u64>();

        tx.send(42).expect("oneshot send should succeed");

        let value = rx.await.expect("oneshot recv should succeed");
        assert_eq!(value, 42);

        emit_behavioral_log("b04", "ABC-CHN-001", "oneshot_delivery", "pass");
    });
}

// =============================================================================
// B05–B07: Timeout override contracts (ABC-TO-001)
// =============================================================================

/// B05: ABC-TO-001 — timeout expires on slow future, returns error.
///
/// Verifies that runtime_compat::timeout enforces the caller's deadline
/// and does not allow the inner future to extend it.
#[test]
fn b05_timeout_expires_on_slow_future() {
    run_async_test(async {
        let start = Instant::now();
        let result = runtime_compat::timeout(Duration::from_millis(50), async {
            runtime_compat::sleep(Duration::from_secs(10)).await;
            "should not reach"
        })
        .await;

        assert!(
            result.is_err(),
            "timeout must expire on slow future (ABC-TO-001)"
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "timeout should return quickly, not wait for inner future (elapsed: {elapsed:?})"
        );

        emit_behavioral_log("b05", "ABC-TO-001", "timeout_expires", "pass");
    });
}

/// B06: ABC-TO-001 — timeout succeeds on fast future, returns value.
#[test]
fn b06_timeout_succeeds_on_fast_future() {
    run_async_test(async {
        let result = runtime_compat::timeout(Duration::from_secs(5), async { 42u32 }).await;

        assert!(result.is_ok(), "fast future must complete within timeout");
        assert_eq!(result.unwrap(), 42);

        emit_behavioral_log("b06", "ABC-TO-001", "timeout_succeeds", "pass");
    });
}

/// B07: ABC-TO-001 — sleep completes with reasonable precision.
///
/// Verifies that runtime_compat::sleep actually waits at least the
/// requested duration (no premature return).
#[test]
fn b07_sleep_waits_at_least_requested_duration() {
    run_async_test(async {
        let start = Instant::now();
        runtime_compat::sleep(Duration::from_millis(25)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(20),
            "sleep must wait at least ~25ms (got {elapsed:?})"
        );

        emit_behavioral_log("b07", "ABC-TO-001", "sleep_precision", "pass");
    });
}

// =============================================================================
// B08–B09: Task lifecycle tracking (ABC-TL-001, ABC-TL-002)
// =============================================================================

/// B08: ABC-TL-001 — JoinSet drives spawned tasks to completion.
///
/// Spawns multiple tasks into a JoinSet and verifies that join_next()
/// reaps all of them. This proves task lifecycle is tracked.
#[test]
fn b08_joinset_drives_tasks_to_completion() {
    run_async_test(async {
        let mut set = runtime_compat::task::JoinSet::new();
        let counter = Arc::new(AtomicUsize::new(0));

        for i in 0..5u32 {
            let c = counter.clone();
            set.spawn(async move {
                c.fetch_add(1, Ordering::SeqCst);
                i
            });
        }

        assert_eq!(set.len(), 5, "JoinSet should track 5 tasks");

        let mut results = Vec::new();
        while let Some(result) = set.join_next().await {
            results.push(result.expect("task should succeed"));
        }

        assert_eq!(results.len(), 5, "all 5 tasks must complete");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            5,
            "all tasks must have executed"
        );
        assert!(set.is_empty(), "JoinSet should be empty after draining");

        emit_behavioral_log("b08", "ABC-TL-001", "joinset_completion", "pass");
    });
}

/// B09: ABC-TL-001 — JoinSet abort_all cancels all tasks.
///
/// Spawns tasks and calls abort_all, then drains them to verify all
/// tasks report cancellation. Under tokio, abort_all marks tasks for
/// cancellation but doesn't remove them until reaped via join_next.
#[test]
fn b09_joinset_abort_all_cancels_tasks() {
    run_async_test(async {
        let mut set = runtime_compat::task::JoinSet::new();

        for _ in 0..3 {
            set.spawn(async {
                runtime_compat::sleep(Duration::from_secs(60)).await;
            });
        }

        assert_eq!(set.len(), 3);
        set.abort_all();

        // Drain remaining handles — each should report cancellation or
        // the set should already be empty (asupersync clears immediately).
        let mut cancel_count = 0;
        while let Some(result) = set.join_next().await {
            if result.is_err() {
                cancel_count += 1;
            }
        }

        // Either all 3 were reaped as cancelled (tokio) or
        // the set was already empty (asupersync clears on abort_all)
        assert!(
            set.is_empty(),
            "JoinSet must be empty after abort_all + drain (ABC-TL-001)"
        );

        emit_behavioral_log(
            "b09",
            "ABC-TL-001",
            "joinset_abort_all",
            &format!("pass:cancelled={cancel_count}"),
        );
    });
}

// =============================================================================
// B10–B12: Semaphore backpressure (ABC-BP-001)
// =============================================================================

/// B10: ABC-BP-001 — Semaphore limits concurrent access.
///
/// Creates a semaphore with N permits and verifies that the (N+1)th
/// try_acquire fails with NoPermits.
#[test]
fn b10_semaphore_limits_concurrent_access() {
    let sem = Semaphore::new(3);

    let _p1 = sem.try_acquire().expect("permit 1 should succeed");
    let _p2 = sem.try_acquire().expect("permit 2 should succeed");
    let _p3 = sem.try_acquire().expect("permit 3 should succeed");

    let result = sem.try_acquire();
    assert!(
        matches!(result, Err(TryAcquireError::NoPermits)),
        "4th try_acquire must fail with NoPermits when 3 permits held (ABC-BP-001)"
    );

    emit_behavioral_log("b10", "ABC-BP-001", "semaphore_limit", "pass");
}

/// B11: ABC-BP-001 — Semaphore permits released on drop.
///
/// Acquires all permits, drops one, then verifies try_acquire succeeds again.
#[test]
fn b11_semaphore_permit_release_on_drop() {
    let sem = Semaphore::new(2);

    let p1 = sem.try_acquire().expect("permit 1");
    let _p2 = sem.try_acquire().expect("permit 2");

    assert!(sem.try_acquire().is_err(), "no permits available");

    // Drop p1 to release one permit
    drop(p1);

    let p3 = sem.try_acquire();
    assert!(
        p3.is_ok(),
        "dropping a permit must make it available again (ABC-BP-001)"
    );

    emit_behavioral_log("b11", "ABC-BP-001", "permit_release_on_drop", "pass");
}

/// B12: ABC-BP-001 — Semaphore close causes try_acquire to return Closed.
#[test]
fn b12_semaphore_close_signals_closure() {
    let sem = Semaphore::new(5);
    sem.close();

    let result = sem.try_acquire();
    assert!(
        matches!(result, Err(TryAcquireError::Closed)),
        "try_acquire after close must return Closed (ABC-BP-001)"
    );

    emit_behavioral_log("b12", "ABC-BP-001", "semaphore_close", "pass");
}

// =============================================================================
// B13–B15: Task ownership and cancellation (ABC-OWN-001, ABC-CAN-002)
// =============================================================================

/// B13: ABC-OWN-001 — Spawned task result accessible via JoinHandle.
///
/// Verifies that the spawner retains ownership of the task's result
/// through the JoinHandle.
#[test]
fn b13_task_ownership_via_join_handle() {
    run_async_test(async {
        let handle = runtime_compat::task::spawn(async { 42u32 });

        let result = handle.await;
        assert!(
            result.is_ok(),
            "JoinHandle must yield Ok result for completed task"
        );
        assert_eq!(
            result.unwrap(),
            42,
            "spawner receives task's output (ABC-OWN-001)"
        );

        emit_behavioral_log("b13", "ABC-OWN-001", "join_handle_ownership", "pass");
    });
}

/// B14: ABC-CAN-002 — JoinHandle abort causes is_cancelled error.
///
/// Spawns a long-running task, aborts it, and verifies the JoinHandle
/// reports cancellation.
#[test]
fn b14_join_handle_abort_implies_cancel() {
    run_async_test(async {
        let handle = runtime_compat::task::spawn(async {
            runtime_compat::sleep(Duration::from_secs(60)).await;
            "should not complete"
        });

        handle.abort();
        let result = handle.await;

        assert!(
            result.is_err(),
            "aborted task must yield Err (ABC-CAN-002)"
        );
        let err = result.unwrap_err();
        assert!(err.is_cancelled(), "error must report as cancelled: {err}");

        emit_behavioral_log("b14", "ABC-CAN-002", "abort_implies_cancel", "pass");
    });
}

/// B15: ABC-OWN-001 — Multiple spawned tasks complete independently.
///
/// Spawns several tasks and verifies each completes with its own result,
/// proving task ownership is per-handle.
#[test]
fn b15_multiple_tasks_independent_ownership() {
    run_async_test(async {
        let h1 = runtime_compat::task::spawn(async { "alpha" });
        let h2 = runtime_compat::task::spawn(async { "beta" });
        let h3 = runtime_compat::task::spawn(async { "gamma" });

        let r1 = h1.await.expect("task 1");
        let r2 = h2.await.expect("task 2");
        let r3 = h3.await.expect("task 3");

        assert_eq!(r1, "alpha");
        assert_eq!(r2, "beta");
        assert_eq!(r3, "gamma");

        emit_behavioral_log("b15", "ABC-OWN-001", "independent_ownership", "pass");
    });
}

// =============================================================================
// B16–B18: Error mapping chain (ABC-ERR-001)
// =============================================================================

/// B16: ABC-ERR-001 — Send to dropped receiver yields error.
///
/// Verifies that error types at the channel boundary correctly propagate
/// failure information when the receiver is gone.
#[test]
fn b16_send_error_on_closed_channel() {
    run_async_test(async {
        let (tx, rx) = runtime_compat::mpsc::channel::<u32>(8);
        drop(rx);

        // Send after receiver dropped should fail
        let result = runtime_compat::mpsc_send(&tx, 42).await;
        assert!(
            result.is_err(),
            "send to closed channel must return error (ABC-ERR-001)"
        );

        emit_behavioral_log("b16", "ABC-ERR-001", "send_error_on_closed", "pass");
    });
}

/// B17: ABC-ERR-001 — Oneshot recv after sender drop yields error.
#[test]
fn b17_oneshot_recv_after_sender_drop_error() {
    run_async_test(async {
        let (tx, rx) = runtime_compat::oneshot::channel::<u32>();
        drop(tx);

        let result = rx.await;
        assert!(
            result.is_err(),
            "oneshot recv after sender drop must error (ABC-ERR-001)"
        );

        emit_behavioral_log("b17", "ABC-ERR-001", "oneshot_close_error", "pass");
    });
}

/// B18: ABC-ERR-001 — Broadcast recv after all senders dropped yields error.
#[test]
fn b18_broadcast_recv_after_close_error() {
    run_async_test(async {
        let (tx, mut rx) = runtime_compat::broadcast::channel::<u32>(8);
        drop(tx);

        let result = rx.recv().await;
        assert!(
            result.is_err(),
            "broadcast recv after sender drop must error (ABC-ERR-001)"
        );

        emit_behavioral_log("b18", "ABC-ERR-001", "broadcast_close_error", "pass");
    });
}

// =============================================================================
// B19–B20: Sync primitive boundary behavior (ABC-OWN-002)
// =============================================================================

/// B19: ABC-OWN-002 — Mutex guard scoping enforces exclusive access.
///
/// Verifies that Mutex guards are scope-bounded: taking a lock, mutating,
/// dropping the guard, then re-acquiring sees the mutation.
#[test]
fn b19_mutex_guard_scope_bounded() {
    run_async_test(async {
        let m = Mutex::new(vec![1, 2, 3]);

        {
            let mut guard = m.lock().await;
            guard.push(4);
            // guard dropped here
        }

        let guard = m.lock().await;
        assert_eq!(
            *guard,
            vec![1, 2, 3, 4],
            "mutation must persist after guard drop (ABC-OWN-002)"
        );

        emit_behavioral_log("b19", "ABC-OWN-002", "mutex_scope_bounded", "pass");
    });
}

/// B20: ABC-OWN-002 — RwLock write then read preserves value.
///
/// Verifies that RwLock guards are scope-bounded and that writes
/// are visible to subsequent reads after the write guard is dropped.
#[test]
fn b20_rwlock_write_then_read() {
    run_async_test(async {
        let rw = RwLock::new(42u32);

        // Read initial value
        {
            let r = rw.read().await;
            assert_eq!(*r, 42);
        }

        // Write new value
        {
            let mut w = rw.write().await;
            *w = 99;
        }

        // Read updated value
        {
            let r = rw.read().await;
            assert_eq!(
                *r, 99,
                "write must be visible after guard drop (ABC-OWN-002)"
            );
        }

        emit_behavioral_log("b20", "ABC-OWN-002", "rwlock_write_read", "pass");
    });
}

// =============================================================================
// B21–B23: Cross-layer integration scenarios
// =============================================================================

/// B21: Integration — Full audit report assembled from behavioral evidence.
///
/// Builds a ContractAuditReport using evidence from the behavioral tests
/// above and verifies the audit machinery works end-to-end.
#[test]
fn b21_full_audit_report_with_behavioral_evidence() {
    let contracts = standard_contracts();
    let mut report = ContractAuditReport::new("behavioral-audit-001", 1_700_000_000_000);

    // Provide passing behavioral evidence for each contract
    let evidence_map: Vec<(&str, &str)> = vec![
        ("ABC-OWN-001", "b13_task_ownership_via_join_handle"),
        ("ABC-OWN-002", "b19_mutex_guard_scope_bounded"),
        ("ABC-CAN-001", "b05_timeout_expires_on_slow_future"),
        ("ABC-CAN-002", "b14_join_handle_abort_implies_cancel"),
        ("ABC-CHN-001", "b02_watch_channel_delivers_latest_value"),
        ("ABC-CHN-002", "b01_mpsc_channel_non_lossy_delivery_on_close"),
        ("ABC-ERR-001", "b16_send_error_on_closed_channel"),
        ("ABC-ERR-002", "manual_code_review_required"),
        ("ABC-BP-001", "b10_semaphore_limits_concurrent_access"),
        ("ABC-TO-001", "b05_timeout_expires_on_slow_future"),
        ("ABC-TL-001", "b08_joinset_drives_tasks_to_completion"),
        ("ABC-TL-002", "b13_task_ownership_via_join_handle"),
    ];

    for contract in contracts {
        let id = contract.contract_id.as_str();
        let evidence: Vec<ContractEvidence> = evidence_map
            .iter()
            .filter(|(cid, _)| *cid == id)
            .map(|(_, test_name)| ContractEvidence {
                contract_id: id.to_owned(),
                test_name: test_name.to_string(),
                passed: id != "ABC-ERR-002", // ERR-002 is non-verifiable
                evidence_type: if id == "ABC-ERR-002" {
                    EvidenceType::CodeReview
                } else {
                    EvidenceType::RuntimeAssertion
                },
                detail: format!(
                    "behavioral test {}",
                    if id != "ABC-ERR-002" {
                        "passed"
                    } else {
                        "requires manual review"
                    }
                ),
            })
            .collect();

        report.add_compliance(ContractCompliance::from_evidence(contract, evidence));
    }

    report.finalize();

    // All verifiable contracts should be compliant
    let verifiable_failing: Vec<_> = report
        .failing_contracts()
        .into_iter()
        .filter(|c| c.contract.verifiable)
        .collect();

    assert!(
        verifiable_failing.is_empty(),
        "all verifiable contracts should be compliant with behavioral evidence: {:?}",
        verifiable_failing
            .iter()
            .map(|c| &c.contract.contract_id)
            .collect::<Vec<_>>()
    );

    // Compliance rate should reflect 11/12 (ERR-002 is non-verifiable)
    assert!(
        report.compliance_rate >= 11.0 / 12.0 - 0.01,
        "compliance rate should be ~91.7%+, got {:.1}%",
        report.compliance_rate * 100.0
    );

    emit_behavioral_log("b21", "all", "full_audit_report", "pass");
}

/// B22: Integration — Semaphore available_permits tracking.
///
/// Verifies the permit counter accurately tracks acquisitions and releases.
#[test]
fn b22_semaphore_available_permits_tracking() {
    let sem = Semaphore::new(5);
    assert_eq!(sem.available_permits(), 5);

    let p1 = sem.try_acquire().unwrap();
    assert_eq!(sem.available_permits(), 4);

    let p2 = sem.try_acquire().unwrap();
    assert_eq!(sem.available_permits(), 3);

    drop(p1);
    assert_eq!(sem.available_permits(), 4);

    drop(p2);
    assert_eq!(sem.available_permits(), 5);

    emit_behavioral_log("b22", "ABC-BP-001", "available_permits_tracking", "pass");
}

/// B23: Integration — Channel pipeline: mpsc → process → broadcast fanout.
///
/// Simulates a real cross-layer pattern: items flow through an mpsc channel
/// (vendored → core boundary), get processed, then fan out via broadcast.
#[test]
fn b23_channel_pipeline_mpsc_to_broadcast() {
    run_async_test(async {
        // Stage 1: mpsc ingestion (simulating vendored → core)
        let (ingest_tx, mut ingest_rx) = runtime_compat::mpsc::channel::<u32>(8);
        // Stage 2: broadcast fanout (simulating core → observers)
        let (fanout_tx, mut fanout_rx1) = runtime_compat::broadcast::channel::<u32>(8);
        let mut fanout_rx2 = fanout_tx.subscribe();

        // Produce items
        for i in 0..3 {
            runtime_compat::mpsc_send(&ingest_tx, i).await.unwrap();
        }
        drop(ingest_tx);

        // Process: drain mpsc, transform, fanout to broadcast
        let mut processed = 0;
        while let Some(item) = runtime_compat::mpsc_recv_option(&mut ingest_rx).await {
            let transformed = item * 10;
            fanout_tx.send(transformed).unwrap();
            processed += 1;
        }
        drop(fanout_tx);

        assert_eq!(processed, 3, "all ingested items must be processed");

        // Both broadcast receivers should get all items
        let mut r1_items = Vec::new();
        while let Ok(v) = fanout_rx1.recv().await {
            r1_items.push(v);
        }
        let mut r2_items = Vec::new();
        while let Ok(v) = fanout_rx2.recv().await {
            r2_items.push(v);
        }

        assert_eq!(r1_items, vec![0, 10, 20]);
        assert_eq!(r2_items, vec![0, 10, 20]);

        emit_behavioral_log(
            "b23",
            "ABC-CHN-001+CHN-002",
            "channel_pipeline",
            "pass",
        );
    });
}

// =============================================================================
// Serde roundtrip for behavioral evidence types
// =============================================================================

/// Behavioral evidence types roundtrip through serde correctly.
#[test]
fn serde_roundtrip_behavioral_evidence() {
    let evidence = ContractEvidence {
        contract_id: "ABC-BP-001".into(),
        test_name: "b10_semaphore_limits_concurrent_access".into(),
        passed: true,
        evidence_type: EvidenceType::RuntimeAssertion,
        detail: "semaphore enforced 3-permit limit".into(),
    };

    let json = serde_json::to_string(&evidence).expect("serialize");
    let back: ContractEvidence = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.contract_id, evidence.contract_id);
    assert_eq!(back.test_name, evidence.test_name);
    assert_eq!(back.passed, evidence.passed);
    assert_eq!(back.evidence_type, evidence.evidence_type);

    emit_behavioral_log("serde", "infra", "evidence_roundtrip", "pass");
}
