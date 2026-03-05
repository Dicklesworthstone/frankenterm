//! DPOR-style distributed merge tests for aggregator semantics.
//!
//! These tests exercise merge behavior under many interleavings with LabRuntime.

#![cfg(all(feature = "distributed", feature = "asupersync-runtime"))]

mod common;

use asupersync::runtime::yield_now;
use asupersync::{Budget, LabRuntime, TaskId};
use common::lab::{ExplorationTestConfig, run_exploration_test};
use frankenterm_core::wire_protocol::{
    Aggregator, IngestResult, PaneDelta, WireEnvelope, WirePayload,
};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

fn schedule_task(runtime: &mut LabRuntime, task_id: TaskId) {
    runtime.scheduler.lock().schedule(task_id, 0);
}

fn pane_delta(pane_id: u64, seq: u64, content: String) -> PaneDelta {
    PaneDelta {
        pane_id,
        seq,
        content_len: content.len(),
        content,
        captured_at_ms: 0,
    }
}

#[test]
fn dpor_distributed_merge_interleavings_preserve_accept_set() {
    let config = ExplorationTestConfig::new("distributed_merge_accept_set", 16)
        .base_seed(19)
        .worker_count(3)
        .max_steps_per_run(120_000);

    let report = run_exploration_test(config, |runtime| {
        let aggregator = Arc::new(Mutex::new(Aggregator::new(16)));
        let accepted: Arc<Mutex<Vec<(String, u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let region = runtime.state.create_root_region(Budget::INFINITE);

        let mut task_ids = Vec::new();
        for sender in ["agent-a", "agent-b"] {
            let sender_for_task = sender.to_string();
            let aggregator = Arc::clone(&aggregator);
            let accepted = Arc::clone(&accepted);
            let (task_id, _) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    for seq in 1..=8_u64 {
                        let envelope = WireEnvelope::new(
                            seq,
                            &sender_for_task,
                            WirePayload::PaneDelta(pane_delta(
                                42,
                                seq,
                                format!("DPOR_MERGE_MARKER sender={sender_for_task} seq={seq}"),
                            )),
                        );
                        let result = {
                            let mut guard = aggregator.lock().expect("lock aggregator");
                            guard.ingest_envelope(envelope).expect("ingest")
                        };
                        if let IngestResult::Accepted(WirePayload::PaneDelta(delta)) = result {
                            accepted.lock().expect("lock accepted").push((
                                sender_for_task.clone(),
                                seq,
                                delta.seq,
                            ));
                        }
                        yield_now().await;
                    }
                })
                .expect("create producer task");
            task_ids.push(task_id);
        }

        for task_id in task_ids {
            schedule_task(runtime, task_id);
        }
        runtime.run_until_quiescent();

        let got: BTreeSet<_> = accepted
            .lock()
            .expect("lock accepted")
            .iter()
            .cloned()
            .collect();
        let mut expected = BTreeSet::new();
        for sender in ["agent-a", "agent-b"] {
            for seq in 1..=8_u64 {
                expected.insert((sender.to_string(), seq, seq));
            }
        }
        assert_eq!(
            got, expected,
            "accepted merge set should be schedule-invariant for monotonic sender streams"
        );
    });

    assert!(report.passed());
    assert!(report.total_runs >= 8);
}

#[test]
fn dpor_distributed_ingest_and_query_snapshot_consistent() {
    let config = ExplorationTestConfig::new("distributed_ingest_query_snapshot_consistent", 12)
        .base_seed(31)
        .worker_count(3)
        .max_steps_per_run(140_000);

    let report = run_exploration_test(config, |runtime| {
        let aggregator = Arc::new(Mutex::new(Aggregator::new(8)));
        let persisted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let producer_done = Arc::new(AtomicBool::new(false));
        let region = runtime.state.create_root_region(Budget::INFINITE);

        let agg_for_producer = Arc::clone(&aggregator);
        let persisted_for_producer = Arc::clone(&persisted);
        let done_for_producer = Arc::clone(&producer_done);
        let (producer_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                for seq in 1..=24_u64 {
                    let payload = format!("QUERY_MARKER seq={seq}");
                    let envelope = WireEnvelope::new(
                        seq,
                        "agent-query",
                        WirePayload::PaneDelta(pane_delta(9, seq, payload.clone())),
                    );
                    let result = {
                        let mut guard = agg_for_producer.lock().expect("lock aggregator");
                        guard.ingest_envelope(envelope).expect("ingest")
                    };
                    if let IngestResult::Accepted(WirePayload::PaneDelta(delta)) = result {
                        persisted_for_producer
                            .lock()
                            .expect("lock persisted")
                            .push(delta.content);
                    }
                    yield_now().await;
                }
                done_for_producer.store(true, Ordering::SeqCst);
            })
            .expect("create producer");

        let persisted_for_query = Arc::clone(&persisted);
        let done_for_query = Arc::clone(&producer_done);
        let (query_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let mut last_seen = 0_usize;
                loop {
                    let snapshot = persisted_for_query.lock().expect("lock persisted").clone();
                    assert!(
                        snapshot.iter().all(|entry| entry.contains("QUERY_MARKER")),
                        "query snapshot must never observe torn/non-marker rows"
                    );
                    assert!(
                        snapshot.len() >= last_seen,
                        "query snapshots must be monotonic under append-only ingest"
                    );
                    last_seen = snapshot.len();

                    if done_for_query.load(Ordering::SeqCst) && snapshot.len() == 24 {
                        break;
                    }
                    yield_now().await;
                }
            })
            .expect("create query task");

        schedule_task(runtime, producer_id);
        schedule_task(runtime, query_id);
        runtime.run_until_quiescent();

        assert_eq!(
            persisted.lock().expect("lock persisted").len(),
            24,
            "all accepted deltas should be query-visible after ingest completes"
        );
    });

    assert!(report.passed());
}

#[test]
fn dpor_distributed_disconnect_yields_contiguous_prefix() {
    let config = ExplorationTestConfig::new("distributed_disconnect_contiguous_prefix", 12)
        .base_seed(47)
        .worker_count(3)
        .max_steps_per_run(140_000);

    let report = run_exploration_test(config, |runtime| {
        let aggregator = Arc::new(Mutex::new(Aggregator::new(8)));
        let persisted: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let disconnect = Arc::new(AtomicBool::new(false));
        let emitted = Arc::new(AtomicU64::new(0));
        let region = runtime.state.create_root_region(Budget::INFINITE);

        let agg_for_producer = Arc::clone(&aggregator);
        let persisted_for_producer = Arc::clone(&persisted);
        let disconnect_for_producer = Arc::clone(&disconnect);
        let emitted_for_producer = Arc::clone(&emitted);
        let (producer_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                for seq in 1..=32_u64 {
                    if disconnect_for_producer.load(Ordering::SeqCst) {
                        break;
                    }
                    let envelope = WireEnvelope::new(
                        seq,
                        "agent-disconnect",
                        WirePayload::PaneDelta(pane_delta(
                            11,
                            seq,
                            format!("DISCONNECT_MARKER seq={seq}"),
                        )),
                    );
                    let result = {
                        let mut guard = agg_for_producer.lock().expect("lock aggregator");
                        guard.ingest_envelope(envelope).expect("ingest")
                    };
                    if let IngestResult::Accepted(WirePayload::PaneDelta(delta)) = result {
                        persisted_for_producer
                            .lock()
                            .expect("lock persisted")
                            .push(delta.seq);
                        emitted_for_producer.fetch_add(1, Ordering::SeqCst);
                    }
                    yield_now().await;
                }
            })
            .expect("create producer");

        let disconnect_for_task = Arc::clone(&disconnect);
        let emitted_for_task = Arc::clone(&emitted);
        let (disconnect_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                while emitted_for_task.load(Ordering::SeqCst) < 7 {
                    yield_now().await;
                }
                disconnect_for_task.store(true, Ordering::SeqCst);
            })
            .expect("create disconnect task");

        schedule_task(runtime, producer_id);
        schedule_task(runtime, disconnect_id);
        runtime.run_until_quiescent();

        let mut seqs = persisted.lock().expect("lock persisted").clone();
        seqs.sort_unstable();
        seqs.dedup();

        let expected: Vec<u64> = (1..=(seqs.len() as u64)).collect();
        assert_eq!(
            seqs, expected,
            "disconnect must leave a clean contiguous committed prefix (no holes/partials)"
        );
        assert!(
            !seqs.is_empty(),
            "test should commit at least one message before disconnect"
        );
    });

    assert!(report.passed());
}

#[test]
fn dpor_distributed_reconnect_replay_preserves_contiguous_sequence() {
    let config = ExplorationTestConfig::new("distributed_reconnect_replay_contiguous", 12)
        .base_seed(89)
        .worker_count(4)
        .max_steps_per_run(160_000);

    let report = run_exploration_test(config, |runtime| {
        let aggregator = Arc::new(Mutex::new(Aggregator::new(8)));
        let accepted: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let disconnect = Arc::new(AtomicBool::new(false));
        let emitted = Arc::new(AtomicU64::new(0));
        let region = runtime.state.create_root_region(Budget::INFINITE);

        let agg_for_primary = Arc::clone(&aggregator);
        let accepted_for_primary = Arc::clone(&accepted);
        let disconnect_for_primary = Arc::clone(&disconnect);
        let emitted_for_primary = Arc::clone(&emitted);
        let (primary_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                for seq in 1..=12_u64 {
                    if disconnect_for_primary.load(Ordering::SeqCst) {
                        break;
                    }
                    let envelope = WireEnvelope::new(
                        seq,
                        "agent-reconnect",
                        WirePayload::PaneDelta(pane_delta(
                            21,
                            seq,
                            format!("RECONNECT_PRIMARY seq={seq}"),
                        )),
                    );
                    let result = {
                        let mut guard = agg_for_primary.lock().expect("lock aggregator");
                        guard.ingest_envelope(envelope).expect("ingest")
                    };
                    if let IngestResult::Accepted(WirePayload::PaneDelta(delta)) = result {
                        accepted_for_primary
                            .lock()
                            .expect("lock accepted")
                            .push(delta.seq);
                        emitted_for_primary.fetch_add(1, Ordering::SeqCst);
                    }
                    yield_now().await;
                }
            })
            .expect("create primary producer");

        let disconnect_for_task = Arc::clone(&disconnect);
        let emitted_for_task = Arc::clone(&emitted);
        let (disconnect_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                while emitted_for_task.load(Ordering::SeqCst) < 6 {
                    yield_now().await;
                }
                disconnect_for_task.store(true, Ordering::SeqCst);
            })
            .expect("create disconnect task");

        let agg_for_reconnect = Arc::clone(&aggregator);
        let accepted_for_reconnect = Arc::clone(&accepted);
        let disconnect_for_reconnect = Arc::clone(&disconnect);
        let (reconnect_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                while !disconnect_for_reconnect.load(Ordering::SeqCst) {
                    yield_now().await;
                }
                for seq in 4..=12_u64 {
                    let envelope = WireEnvelope::new(
                        seq,
                        "agent-reconnect",
                        WirePayload::PaneDelta(pane_delta(
                            21,
                            seq,
                            format!("RECONNECT_RESUME seq={seq}"),
                        )),
                    );
                    let result = {
                        let mut guard = agg_for_reconnect.lock().expect("lock aggregator");
                        guard.ingest_envelope(envelope).expect("ingest")
                    };
                    if let IngestResult::Accepted(WirePayload::PaneDelta(delta)) = result {
                        accepted_for_reconnect
                            .lock()
                            .expect("lock accepted")
                            .push(delta.seq);
                    }
                    yield_now().await;
                }
            })
            .expect("create reconnect producer");

        schedule_task(runtime, primary_id);
        schedule_task(runtime, disconnect_id);
        schedule_task(runtime, reconnect_id);
        runtime.run_until_quiescent();

        let mut seqs = accepted.lock().expect("lock accepted").clone();
        seqs.sort_unstable();
        seqs.dedup();
        let expected: Vec<u64> = (1..=12_u64).collect();
        assert_eq!(
            seqs, expected,
            "reconnect replay should fill gaps and avoid duplicates, yielding contiguous accepted sequence"
        );
    });

    assert!(report.passed());
    assert!(report.total_runs >= 8);
}
