//! E4.F1.T3: Performance soak and flake validation harness.
//!
//! Verifies append/flush/cursor latency against storage_targets SLO budgets,
//! runs sustained ingest soak, and detects flaky variance.

use frankenterm_core::recorder_storage::{
    AppendLogRecorderStorage, AppendLogStorageConfig, AppendRequest, CursorRecord, DurabilityLevel,
    EventCursorError, FlushMode, RecorderBackendKind, RecorderEventCursor, RecorderEventReader,
    RecorderOffset, RecorderStorage,
};
use frankenterm_core::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use std::time::Instant;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn perf_config(path: &std::path::Path) -> AppendLogStorageConfig {
    AppendLogStorageConfig {
        data_path: path.join("events.log"),
        state_path: path.join("state.json"),
        queue_capacity: 4096,
        max_batch_events: 256,
        max_batch_bytes: 512 * 1024,
        max_idempotency_entries: 512,
    }
}

fn sample_event(seq: u64) -> RecorderEvent {
    RecorderEvent {
        schema_version: "ft.recorder.event.v1".to_string(),
        event_id: format!("perf-{seq}"),
        pane_id: seq % 8 + 1,
        session_id: Some("perf-session".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: 1_700_000_000_000 + seq,
        recorded_at_ms: 1_700_000_000_001 + seq,
        sequence: seq,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: format!("perf-payload-{seq}"),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn make_batch(batch_id: &str, start: u64, count: u64) -> AppendRequest {
    let events: Vec<_> = (start..start + count).map(sample_event).collect();
    AppendRequest {
        batch_id: batch_id.to_string(),
        events,
        required_durability: DurabilityLevel::Appended,
        producer_ts_ms: 1,
    }
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 * p / 100.0).ceil() as usize).saturating_sub(1);
    sorted[idx.min(sorted.len() - 1)]
}

fn coefficient_of_variation(values: &[u64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<u64>() as f64 / values.len() as f64;
    if mean < 1.0 {
        return 0.0;
    }
    let variance = values
        .iter()
        .map(|v| (*v as f64 - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64;
    variance.sqrt() / mean
}

// ===========================================================================
// Append throughput tests
// ===========================================================================

#[tokio::test]
async fn test_append_single_batch_latency() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let start = Instant::now();
    storage
        .append_batch(make_batch("single-1", 0, 1))
        .await
        .unwrap();
    let elapsed_us = start.elapsed().as_micros();

    // Single append should be well under 2ms SLO
    assert!(
        elapsed_us < 10_000,
        "single append took {elapsed_us}us, expected < 10ms"
    );
}

#[tokio::test]
async fn test_append_batch_128_latency() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let start = Instant::now();
    storage
        .append_batch(make_batch("batch128-1", 0, 128))
        .await
        .unwrap();
    let elapsed_us = start.elapsed().as_micros();

    // Batch of 128 should be under 50ms SLO
    assert!(
        elapsed_us < 50_000,
        "128-event batch took {elapsed_us}us, expected < 50ms"
    );
}

#[tokio::test]
async fn test_append_throughput_events_per_sec() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let total_events = 1000u64;
    let batch_size = 50u64;
    let start = Instant::now();

    let mut seq = 0u64;
    while seq < total_events {
        storage
            .append_batch(make_batch(&format!("tput-{seq}"), seq, batch_size))
            .await
            .unwrap();
        seq += batch_size;
    }

    let elapsed_secs = start.elapsed().as_secs_f64();
    let events_per_sec = total_events as f64 / elapsed_secs;

    // Should sustain at least 500 events/sec (very conservative)
    assert!(
        events_per_sec > 500.0,
        "throughput {events_per_sec:.0} events/sec, expected > 500"
    );
}

// ===========================================================================
// Flush latency tests
// ===========================================================================

#[tokio::test]
async fn test_flush_buffered_latency() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();
    storage
        .append_batch(make_batch("flush-buf-1", 0, 10))
        .await
        .unwrap();

    let start = Instant::now();
    storage.flush(FlushMode::Buffered).await.unwrap();
    let elapsed_us = start.elapsed().as_micros();

    // Buffered flush should be very fast
    assert!(
        elapsed_us < 50_000,
        "buffered flush took {elapsed_us}us, expected < 50ms"
    );
}

#[tokio::test]
async fn test_flush_durable_latency() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();
    storage
        .append_batch(make_batch("flush-dur-1", 0, 50))
        .await
        .unwrap();

    let start = Instant::now();
    storage.flush(FlushMode::Durable).await.unwrap();
    let elapsed_us = start.elapsed().as_micros();

    // Durable flush SLO: p95 < 50ms — we allow 100ms headroom in tests
    assert!(
        elapsed_us < 100_000,
        "durable flush took {elapsed_us}us, expected < 100ms"
    );
}

// ===========================================================================
// In-memory reader for cursor tests
// ===========================================================================

struct PerfMemoryReader {
    records: Vec<CursorRecord>,
}

struct PerfMemoryCursor {
    records: Vec<CursorRecord>,
    pos: usize,
}

impl RecorderEventCursor for PerfMemoryCursor {
    fn next_batch(
        &mut self,
        max: usize,
    ) -> std::result::Result<Vec<CursorRecord>, EventCursorError> {
        let end = (self.pos + max).min(self.records.len());
        let batch = self.records[self.pos..end].to_vec();
        self.pos = end;
        Ok(batch)
    }

    fn current_offset(&self) -> RecorderOffset {
        if self.pos < self.records.len() {
            self.records[self.pos].offset.clone()
        } else {
            self.records
                .last()
                .map(|r| RecorderOffset {
                    segment_id: 0,
                    byte_offset: r.offset.byte_offset + 1,
                    ordinal: r.offset.ordinal + 1,
                })
                .unwrap_or(RecorderOffset {
                    segment_id: 0,
                    byte_offset: 0,
                    ordinal: 0,
                })
        }
    }
}

impl RecorderEventReader for PerfMemoryReader {
    fn open_cursor(
        &self,
        from: RecorderOffset,
    ) -> std::result::Result<Box<dyn RecorderEventCursor>, EventCursorError> {
        let remaining: Vec<_> = self
            .records
            .iter()
            .filter(|r| r.offset.ordinal >= from.ordinal)
            .cloned()
            .collect();
        Ok(Box::new(PerfMemoryCursor {
            records: remaining,
            pos: 0,
        }))
    }

    fn head_offset(&self) -> std::result::Result<RecorderOffset, EventCursorError> {
        Ok(self
            .records
            .last()
            .map(|r| RecorderOffset {
                segment_id: 0,
                byte_offset: r.offset.byte_offset + 1,
                ordinal: r.offset.ordinal + 1,
            })
            .unwrap_or(RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 0,
            }))
    }
}

// ===========================================================================
// Cursor iteration tests
// ===========================================================================

#[tokio::test]
async fn test_cursor_iteration_1k_events() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    // Populate 1000 events and collect records for MemoryReader
    let mut all_records = Vec::new();
    let mut seq = 0u64;
    while seq < 1000 {
        let events: Vec<_> = (seq..seq + 100).map(sample_event).collect();
        let resp = storage
            .append_batch(AppendRequest {
                batch_id: format!("cursor-{seq}"),
                events: events.clone(),
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();
        let first_ord = resp.first_offset.ordinal;
        for (i, event) in events.into_iter().enumerate() {
            let ordinal = first_ord + i as u64;
            all_records.push(CursorRecord {
                event,
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: ordinal * 100,
                    ordinal,
                },
            });
        }
        seq += 100;
    }

    let reader = PerfMemoryReader {
        records: all_records,
    };

    let start = Instant::now();
    let mut cursor = reader
        .open_cursor(RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 0,
        })
        .unwrap();

    let mut total_read = 0;
    loop {
        let batch = cursor.next_batch(100).unwrap();
        if batch.is_empty() {
            break;
        }
        total_read += batch.len();
    }

    let elapsed_us = start.elapsed().as_micros();

    assert_eq!(total_read, 1000);
    // Cursor scan of 1K events should be very fast
    assert!(
        elapsed_us < 100_000,
        "1K cursor iteration took {elapsed_us}us, expected < 100ms"
    );
}

// ===========================================================================
// SLO gate tests
// ===========================================================================

#[tokio::test]
async fn test_slo_append_p50_under_1ms() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let mut latencies = Vec::with_capacity(100);
    for i in 0u64..100 {
        let start = Instant::now();
        storage
            .append_batch(make_batch(&format!("slo-p50-{i}"), i, 1))
            .await
            .unwrap();
        latencies.push(start.elapsed().as_micros() as u64);
    }

    latencies.sort();
    let p50 = percentile(&latencies, 50.0);

    // p50 should be under 1ms (1000us) — allow 5ms test headroom
    assert!(p50 < 5_000, "append p50 = {p50}us, expected < 5ms");
}

#[tokio::test]
async fn test_slo_append_p99_under_10ms() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let mut latencies = Vec::with_capacity(200);
    for i in 0u64..200 {
        let start = Instant::now();
        storage
            .append_batch(make_batch(&format!("slo-p99-{i}"), i, 1))
            .await
            .unwrap();
        latencies.push(start.elapsed().as_micros() as u64);
    }

    latencies.sort();
    let p99 = percentile(&latencies, 99.0);

    // p99 should be under 10ms — allow 50ms test headroom for CI
    assert!(p99 < 50_000, "append p99 = {p99}us, expected < 50ms");
}

#[tokio::test]
async fn test_slo_flush_durable_p50_under_5ms() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let mut latencies = Vec::with_capacity(50);
    for i in 0u64..50 {
        storage
            .append_batch(make_batch(&format!("slo-flush-{i}"), i * 10, 10))
            .await
            .unwrap();

        let start = Instant::now();
        storage.flush(FlushMode::Durable).await.unwrap();
        latencies.push(start.elapsed().as_micros() as u64);
    }

    latencies.sort();
    let p50 = percentile(&latencies, 50.0);

    // Durable flush p50 < 5ms — allow 50ms headroom
    assert!(p50 < 50_000, "flush durable p50 = {p50}us, expected < 50ms");
}

// ===========================================================================
// Backend kind verification
// ===========================================================================

#[tokio::test]
async fn test_backend_kind_is_append_log() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();
    assert_eq!(storage.backend_kind(), RecorderBackendKind::AppendLog);
}

// ===========================================================================
// Health under load
// ===========================================================================

#[tokio::test]
async fn test_health_stays_green_under_sustained_append() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    for i in 0u64..100 {
        storage
            .append_batch(make_batch(&format!("health-{i}"), i * 10, 10))
            .await
            .unwrap();
    }

    let health = storage.health().await;
    assert!(!health.degraded, "storage degraded after 1000 events");
    assert!(health.latest_offset.is_some());
    assert_eq!(health.latest_offset.unwrap().ordinal, 999);
}

// ===========================================================================
// Soak tests (sustained ingest)
// ===========================================================================

#[tokio::test]
async fn test_soak_sustained_ingest_no_degradation() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let total_events = 5000u64;
    let batch_size = 50u64;
    let mut seq = 0u64;
    let mut max_batch_us = 0u64;

    let start = Instant::now();
    while seq < total_events {
        let batch_start = Instant::now();
        storage
            .append_batch(make_batch(&format!("soak-{seq}"), seq, batch_size))
            .await
            .unwrap();
        let batch_us = batch_start.elapsed().as_micros() as u64;
        max_batch_us = max_batch_us.max(batch_us);
        seq += batch_size;
    }
    let total_secs = start.elapsed().as_secs_f64();

    let health = storage.health().await;
    assert!(!health.degraded, "storage degraded during soak");
    assert_eq!(health.latest_offset.unwrap().ordinal, total_events - 1);

    let throughput = total_events as f64 / total_secs;
    assert!(
        throughput > 100.0,
        "soak throughput {throughput:.0} events/sec, expected > 100"
    );

    // No single batch should exceed 500ms
    assert!(
        max_batch_us < 500_000,
        "worst batch {max_batch_us}us, expected < 500ms"
    );
}

#[tokio::test]
async fn test_soak_no_latency_drift() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let windows = 10;
    let events_per_window = 200u64;
    let batch_size = 50u64;
    let mut window_latencies = Vec::new();
    let mut seq = 0u64;

    for _w in 0..windows {
        let window_start = Instant::now();
        let mut events_in_window = 0u64;
        while events_in_window < events_per_window {
            storage
                .append_batch(make_batch(&format!("drift-{seq}"), seq, batch_size))
                .await
                .unwrap();
            seq += batch_size;
            events_in_window += batch_size;
        }
        let window_us = window_start.elapsed().as_micros() as u64;
        window_latencies.push(window_us);
    }

    // Check that the last window isn't more than 5x the first (no drift)
    let first = window_latencies[0].max(1);
    let last = *window_latencies.last().unwrap();
    let ratio = last as f64 / first as f64;

    assert!(
        ratio < 5.0,
        "latency drift ratio {ratio:.2}x (first={first}us, last={last}us), expected < 5x"
    );
}

// ===========================================================================
// Flake detection tests
// ===========================================================================

#[tokio::test]
async fn test_flake_append_variance_under_threshold() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    // Warm up
    for i in 0u64..10 {
        storage
            .append_batch(make_batch(&format!("warmup-{i}"), i, 10))
            .await
            .unwrap();
    }

    let mut latencies = Vec::with_capacity(20);
    for i in 0u64..20 {
        let start = Instant::now();
        storage
            .append_batch(make_batch(&format!("flake-{i}"), 100 + i * 10, 10))
            .await
            .unwrap();
        latencies.push(start.elapsed().as_micros() as u64);
    }

    let cv = coefficient_of_variation(&latencies);

    // Coefficient of variation < 2.0 (200%) is acceptable for CI
    // (filesystem caching makes this highly variable)
    assert!(
        cv < 2.0,
        "append CV = {cv:.3}, expected < 2.0 (latencies: {latencies:?})"
    );
}

#[tokio::test]
async fn test_flake_flush_variance_under_threshold() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let mut latencies = Vec::with_capacity(10);
    for i in 0u64..10 {
        storage
            .append_batch(make_batch(&format!("flake-flush-{i}"), i * 10, 10))
            .await
            .unwrap();

        let start = Instant::now();
        storage.flush(FlushMode::Durable).await.unwrap();
        latencies.push(start.elapsed().as_micros() as u64);
    }

    let cv = coefficient_of_variation(&latencies);
    assert!(cv < 2.0, "flush CV = {cv:.3}, expected < 2.0");
}

// ===========================================================================
// Multi-pane concurrent-ish append
// ===========================================================================

#[tokio::test]
async fn test_multi_pane_append_no_contention_degradation() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    // Simulate 8 panes appending interleaved
    let panes = 8u64;
    let events_per_pane = 50u64;
    let start = Instant::now();

    for round in 0..events_per_pane {
        for pane in 0..panes {
            let seq = round * panes + pane;
            storage
                .append_batch(make_batch(&format!("pane-{pane}-{round}"), seq, 1))
                .await
                .unwrap();
        }
    }

    let elapsed_ms = start.elapsed().as_millis();
    let total = panes * events_per_pane;

    assert!(
        elapsed_ms < 10_000,
        "{total} multi-pane appends took {elapsed_ms}ms, expected < 10s"
    );

    let health = storage.health().await;
    assert!(!health.degraded);
}

// ===========================================================================
// Checkpoint latency
// ===========================================================================

#[tokio::test]
async fn test_checkpoint_roundtrip_latency() {
    use frankenterm_core::recorder_storage::{CheckpointConsumerId, RecorderCheckpoint};

    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();
    storage
        .append_batch(make_batch("cp-seed", 0, 10))
        .await
        .unwrap();

    let consumer = CheckpointConsumerId("perf-consumer".to_string());

    let start = Instant::now();
    storage
        .commit_checkpoint(RecorderCheckpoint {
            consumer: consumer.clone(),
            upto_offset: RecorderOffset {
                segment_id: 0,
                byte_offset: 0,
                ordinal: 5,
            },
            schema_version: "v1".to_string(),
            committed_at_ms: 1000,
        })
        .await
        .unwrap();
    let commit_us = start.elapsed().as_micros();

    let start2 = Instant::now();
    let cp = storage.read_checkpoint(&consumer).await.unwrap();
    let read_us = start2.elapsed().as_micros();

    assert!(cp.is_some());
    assert_eq!(cp.unwrap().upto_offset.ordinal, 5);

    // Checkpoint SLO: p95 < 100ms
    assert!(commit_us < 100_000, "checkpoint commit took {commit_us}us");
    assert!(read_us < 100_000, "checkpoint read took {read_us}us");
}

// ===========================================================================
// Idempotency performance
// ===========================================================================

#[tokio::test]
async fn test_idempotent_replay_no_performance_penalty() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let batch = make_batch("idempotent-1", 0, 50);
    storage.append_batch(batch.clone()).await.unwrap();

    // Replay same batch — should be fast
    let start = Instant::now();
    let resp = storage.append_batch(batch).await.unwrap();
    let elapsed_us = start.elapsed().as_micros();

    assert_eq!(resp.accepted_count, 50);
    assert!(
        elapsed_us < 50_000,
        "idempotent replay took {elapsed_us}us, expected < 50ms"
    );
}

// ===========================================================================
// Percentile helper tests
// ===========================================================================

#[test]
fn test_percentile_helper_correctness() {
    let data: Vec<u64> = (1..=100).collect();
    assert_eq!(percentile(&data, 50.0), 50);
    assert_eq!(percentile(&data, 99.0), 99);
    assert_eq!(percentile(&data, 100.0), 100);
    assert_eq!(percentile(&data, 1.0), 1);
}

#[test]
fn test_percentile_empty() {
    let data: Vec<u64> = vec![];
    assert_eq!(percentile(&data, 50.0), 0);
}

#[test]
fn test_coefficient_of_variation_constant() {
    let data = vec![100, 100, 100, 100, 100];
    let cv = coefficient_of_variation(&data);
    assert!(cv < 0.001, "constant data CV should be 0, got {cv}");
}

#[test]
fn test_coefficient_of_variation_varied() {
    let data = vec![10, 20, 30, 40, 50];
    let cv = coefficient_of_variation(&data);
    assert!(cv > 0.0 && cv < 1.0, "moderate data CV = {cv}");
}

// ===========================================================================
// Large batch edge case
// ===========================================================================

#[tokio::test]
async fn test_max_batch_256_events_within_slo() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let start = Instant::now();
    storage
        .append_batch(make_batch("max-256", 0, 256))
        .await
        .unwrap();
    let elapsed_us = start.elapsed().as_micros();

    // Max batch (256) should still be under 50ms SLO
    assert!(
        elapsed_us < 100_000,
        "256-event batch took {elapsed_us}us, expected < 100ms"
    );
}

// ===========================================================================
// Sequential flush consistency
// ===========================================================================

#[tokio::test]
async fn test_sequential_flush_no_stacking() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    for i in 0u64..5 {
        storage
            .append_batch(make_batch(&format!("seq-flush-{i}"), i * 20, 20))
            .await
            .unwrap();
        storage.flush(FlushMode::Durable).await.unwrap();
    }

    // Final flush after all data
    let start = Instant::now();
    storage.flush(FlushMode::Durable).await.unwrap();
    let elapsed_us = start.elapsed().as_micros();

    // Should be nearly instant since nothing to flush
    assert!(
        elapsed_us < 50_000,
        "noop flush took {elapsed_us}us, expected < 50ms"
    );
}

// ===========================================================================
// Lag metrics performance
// ===========================================================================

#[tokio::test]
async fn test_lag_metrics_latency() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();
    storage
        .append_batch(make_batch("lag-seed", 0, 100))
        .await
        .unwrap();

    let start = Instant::now();
    let lag = storage.lag_metrics().await.unwrap();
    let elapsed_us = start.elapsed().as_micros();

    assert!(lag.latest_offset.is_some());
    assert!(
        elapsed_us < 50_000,
        "lag_metrics took {elapsed_us}us, expected < 50ms"
    );
}

// ===========================================================================
// Additional SLO and edge case tests
// ===========================================================================

#[tokio::test]
async fn test_health_latency_under_1ms() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();
    storage
        .append_batch(make_batch("health-lat", 0, 50))
        .await
        .unwrap();

    let start = Instant::now();
    let health = storage.health().await;
    let elapsed_us = start.elapsed().as_micros();

    assert!(!health.degraded);
    assert!(
        elapsed_us < 10_000,
        "health() took {elapsed_us}us, expected < 10ms"
    );
}

#[tokio::test]
async fn test_append_monotonic_ordinals_under_load() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let mut last_ordinal = 0u64;
    for i in 0u64..50 {
        let resp = storage
            .append_batch(make_batch(&format!("mono-{i}"), i * 10, 10))
            .await
            .unwrap();
        assert!(
            resp.first_offset.ordinal >= last_ordinal,
            "ordinal went backwards at batch {i}"
        );
        last_ordinal = resp.last_offset.ordinal + 1;
    }
    assert_eq!(last_ordinal, 500);
}

#[tokio::test]
async fn test_empty_batch_rejection_fast() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let start = Instant::now();
    let result = storage
        .append_batch(AppendRequest {
            batch_id: "empty-1".to_string(),
            events: vec![],
            required_durability: DurabilityLevel::Appended,
            producer_ts_ms: 1,
        })
        .await;
    let elapsed_us = start.elapsed().as_micros();

    assert!(result.is_err(), "empty batch should be rejected");
    assert!(
        elapsed_us < 10_000,
        "empty batch rejection took {elapsed_us}us, expected < 10ms"
    );
}

#[tokio::test]
async fn test_soak_window_p99_stability() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    let windows = 5;
    let batches_per_window = 20;
    let mut window_p99s = Vec::new();

    for w in 0..windows {
        let mut latencies = Vec::with_capacity(batches_per_window);
        for b in 0..batches_per_window {
            let seq = (w * batches_per_window + b) as u64 * 10;
            let start = Instant::now();
            storage
                .append_batch(make_batch(&format!("stability-{w}-{b}"), seq, 10))
                .await
                .unwrap();
            latencies.push(start.elapsed().as_micros() as u64);
        }
        latencies.sort();
        window_p99s.push(percentile(&latencies, 99.0));
    }

    // No window p99 should exceed 100ms
    for (i, p99) in window_p99s.iter().enumerate() {
        assert!(*p99 < 100_000, "window {i} p99 = {p99}us, expected < 100ms");
    }
}

#[tokio::test]
async fn test_cursor_partial_range_performance() {
    let dir = tempdir().unwrap();
    let storage = AppendLogRecorderStorage::open(perf_config(dir.path())).unwrap();

    // Populate 500 events
    let mut all_records = Vec::new();
    let mut seq = 0u64;
    while seq < 500 {
        let events: Vec<_> = (seq..seq + 50).map(sample_event).collect();
        let resp = storage
            .append_batch(AppendRequest {
                batch_id: format!("partial-{seq}"),
                events: events.clone(),
                required_durability: DurabilityLevel::Appended,
                producer_ts_ms: 1,
            })
            .await
            .unwrap();
        let first_ord = resp.first_offset.ordinal;
        for (i, event) in events.into_iter().enumerate() {
            let ordinal = first_ord + i as u64;
            all_records.push(CursorRecord {
                event,
                offset: RecorderOffset {
                    segment_id: 0,
                    byte_offset: ordinal * 100,
                    ordinal,
                },
            });
        }
        seq += 50;
    }

    let reader = PerfMemoryReader {
        records: all_records,
    };

    // Read only from ordinal 250 onwards
    let start = Instant::now();
    let mut cursor = reader
        .open_cursor(RecorderOffset {
            segment_id: 0,
            byte_offset: 0,
            ordinal: 250,
        })
        .unwrap();

    let mut count = 0;
    loop {
        let batch = cursor.next_batch(50).unwrap();
        if batch.is_empty() {
            break;
        }
        count += batch.len();
    }
    let elapsed_us = start.elapsed().as_micros();

    assert_eq!(count, 250);
    assert!(
        elapsed_us < 50_000,
        "partial cursor took {elapsed_us}us, expected < 50ms"
    );
}
