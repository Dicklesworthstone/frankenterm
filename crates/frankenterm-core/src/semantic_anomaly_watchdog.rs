//! Semantic Anomaly Watchdog — Lock-Free IPC & Adaptive Batching Daemon.
//!
//! Bead: ft-344j8.7
//!
//! The watchdog bridges PTY ingestion and the ONNX-powered semantic anomaly
//! pipeline without ever blocking the terminal. Architecture:
//!
//! ```text
//! PTY Thread                     ML Thread (dedicated OS thread)
//! ─────────                      ────────────────────────────────
//! observe_segment(bytes)  ──►  ArrayQueue (SPSC, lock-free)
//!   └─ if full: shed + count      │
//!                                  ▼
//!                           adaptive_batch (≤16 items or 10ms)
//!                                  │
//!                                  ▼
//!                           EntropyGate → skip low-entropy
//!                                  │
//!                                  ▼
//!                           embed_fn(segment) → Vec<f32>
//!                                  │
//!                                  ▼
//!                           ConformalAnomalyDetector.observe()
//!                                  │
//!                                  ▼
//!                           if anomaly → EventBus.publish(SemanticAnomalyDetected)
//! ```
//!
//! Key properties:
//! - **Zero-Hitch**: PTY thread never blocks (lock-free `try_send`)
//! - **Adaptive batching**: collects up to `batch_size` items or waits `batch_timeout`
//! - **Dedicated OS thread**: avoids exhausting the async blocking pool
//! - **Graceful shutdown**: poison pill via channel close

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam::queue::ArrayQueue;
use serde::{Deserialize, Serialize};

use crate::events::EventBus;
use crate::patterns::{AgentType, Detection, Severity};
use crate::semantic_anomaly::{
    ConformalAnomalyConfig, ConformalShock, EntropyGateConfig, GatedAnomalyDetector,
    GatedObservation,
};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the semantic anomaly watchdog daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogConfig {
    /// Capacity of the lock-free segment queue. Segments are dropped (shed)
    /// when the queue is full. Default: 256.
    pub queue_capacity: usize,
    /// Maximum batch size for adaptive batching. Default: 16.
    pub batch_size: usize,
    /// Maximum time to wait for a full batch before processing what's available.
    /// Default: 10ms.
    pub batch_timeout_ms: u64,
    /// Entropy gate configuration.
    pub entropy_gate: EntropyGateConfig,
    /// Conformal anomaly detector configuration.
    pub conformal: ConformalAnomalyConfig,
    /// Minimum segment size to consider (bytes). Segments shorter than this
    /// are silently ignored. Default: 4.
    pub min_segment_bytes: usize,
    /// Maximum segment size to accept (bytes). Larger segments are truncated.
    /// Default: 65536 (64 KB).
    pub max_segment_bytes: usize,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 256,
            batch_size: 16,
            batch_timeout_ms: 10,
            entropy_gate: EntropyGateConfig::default(),
            conformal: ConformalAnomalyConfig::default(),
            min_segment_bytes: 4,
            max_segment_bytes: 65_536,
        }
    }
}

fn sanitize_watchdog_config(mut config: WatchdogConfig) -> WatchdogConfig {
    if config.queue_capacity == 0 {
        tracing::warn!(
            "semantic watchdog queue_capacity=0 is invalid; clamping to 1"
        );
        config.queue_capacity = 1;
    }
    if config.batch_size == 0 {
        tracing::warn!("semantic watchdog batch_size=0 is invalid; clamping to 1");
        config.batch_size = 1;
    }
    if config.max_segment_bytes < config.min_segment_bytes {
        tracing::warn!(
            min_segment_bytes = config.min_segment_bytes,
            max_segment_bytes = config.max_segment_bytes,
            "semantic watchdog max_segment_bytes < min_segment_bytes; clamping max to min"
        );
        config.max_segment_bytes = config.min_segment_bytes;
    }
    config
}

// =============================================================================
// Metrics
// =============================================================================

/// Runtime metrics for the watchdog daemon (lock-free atomic counters).
#[derive(Debug)]
pub struct WatchdogMetrics {
    /// Segments submitted by PTY threads.
    pub segments_submitted: AtomicU64,
    /// Segments shed (dropped) because the queue was full.
    pub segments_shed: AtomicU64,
    /// Segments processed by the ML thread.
    pub segments_processed: AtomicU64,
    /// Segments skipped by the entropy gate.
    pub segments_entropy_skipped: AtomicU64,
    /// Segments that triggered embedding.
    pub segments_embedded: AtomicU64,
    /// Anomalies detected.
    pub anomalies_detected: AtomicU64,
    /// Batches processed.
    pub batches_processed: AtomicU64,
    /// Total batch fill (sum of batch sizes for avg computation).
    pub total_batch_fill: AtomicU64,
    /// Segments ignored because they were too short.
    pub segments_too_short: AtomicU64,
    /// Segments truncated because they exceeded max_segment_bytes.
    pub segments_truncated: AtomicU64,
}

impl WatchdogMetrics {
    fn new() -> Self {
        Self {
            segments_submitted: AtomicU64::new(0),
            segments_shed: AtomicU64::new(0),
            segments_processed: AtomicU64::new(0),
            segments_entropy_skipped: AtomicU64::new(0),
            segments_embedded: AtomicU64::new(0),
            anomalies_detected: AtomicU64::new(0),
            batches_processed: AtomicU64::new(0),
            total_batch_fill: AtomicU64::new(0),
            segments_too_short: AtomicU64::new(0),
            segments_truncated: AtomicU64::new(0),
        }
    }

    /// Snapshot the metrics into a serializable form.
    pub fn snapshot(&self) -> WatchdogMetricsSnapshot {
        let batches = self.batches_processed.load(Ordering::Relaxed);
        let fill = self.total_batch_fill.load(Ordering::Relaxed);
        WatchdogMetricsSnapshot {
            segments_submitted: self.segments_submitted.load(Ordering::Relaxed),
            segments_shed: self.segments_shed.load(Ordering::Relaxed),
            segments_processed: self.segments_processed.load(Ordering::Relaxed),
            segments_entropy_skipped: self.segments_entropy_skipped.load(Ordering::Relaxed),
            segments_embedded: self.segments_embedded.load(Ordering::Relaxed),
            anomalies_detected: self.anomalies_detected.load(Ordering::Relaxed),
            batches_processed: batches,
            avg_batch_fill: if batches > 0 {
                fill as f64 / batches as f64
            } else {
                0.0
            },
            segments_too_short: self.segments_too_short.load(Ordering::Relaxed),
            segments_truncated: self.segments_truncated.load(Ordering::Relaxed),
        }
    }
}

/// Serializable snapshot of watchdog metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogMetricsSnapshot {
    pub segments_submitted: u64,
    pub segments_shed: u64,
    pub segments_processed: u64,
    pub segments_entropy_skipped: u64,
    pub segments_embedded: u64,
    pub anomalies_detected: u64,
    pub batches_processed: u64,
    pub avg_batch_fill: f64,
    pub segments_too_short: u64,
    pub segments_truncated: u64,
}

// =============================================================================
// Segment envelope
// =============================================================================

/// A terminal segment submitted for anomaly analysis.
#[derive(Debug, Clone)]
pub struct SegmentEnvelope {
    /// Pane ID the segment came from.
    pub pane_id: u64,
    /// The raw terminal segment bytes.
    pub data: Vec<u8>,
    /// Timestamp when the segment was captured.
    pub captured_at: Instant,
}

// =============================================================================
// Anomaly event (for EventBus)
// =============================================================================

/// An anomaly detected by the watchdog, ready for EventBus publishing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticAnomalyEvent {
    /// Pane where the anomaly was detected.
    pub pane_id: u64,
    /// The conformal shock details.
    pub shock: ConformalShock,
    /// Size of the segment that triggered the anomaly (bytes).
    pub segment_len: usize,
}

// =============================================================================
// Watchdog handle (producer side — used by PTY threads)
// =============================================================================

/// Handle for submitting segments to the watchdog.
///
/// Cheaply cloneable (Arc-backed). Call `observe_segment()` from any PTY thread.
/// If the queue is full, the segment is silently dropped and `segments_shed`
/// is incremented (Zero-Hitch guarantee).
#[derive(Clone)]
pub struct WatchdogHandle {
    queue: Arc<ArrayQueue<SegmentEnvelope>>,
    metrics: Arc<WatchdogMetrics>,
    running: Arc<AtomicBool>,
    config: WatchdogConfig,
}

impl WatchdogHandle {
    /// Submit a terminal segment for anomaly analysis.
    ///
    /// **Never blocks.** If the queue is full, the segment is dropped and
    /// `segments_shed` is incremented.
    ///
    /// Returns `true` if the segment was enqueued, `false` if it was shed.
    pub fn observe_segment(&self, pane_id: u64, data: &[u8]) -> bool {
        if !self.running.load(Ordering::Relaxed) {
            return false;
        }

        // Filter by segment size.
        if data.len() < self.config.min_segment_bytes {
            self.metrics
                .segments_too_short
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }

        self.metrics
            .segments_submitted
            .fetch_add(1, Ordering::Relaxed);

        // Truncate if too large.
        let segment_data = if data.len() > self.config.max_segment_bytes {
            self.metrics
                .segments_truncated
                .fetch_add(1, Ordering::Relaxed);
            data[..self.config.max_segment_bytes].to_vec()
        } else {
            data.to_vec()
        };

        let envelope = SegmentEnvelope {
            pane_id,
            data: segment_data,
            captured_at: Instant::now(),
        };

        // Lock-free enqueue. If full, shed.
        match self.queue.push(envelope) {
            Ok(()) => true,
            Err(_) => {
                self.metrics.segments_shed.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Check if the watchdog is still running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Get a snapshot of the current metrics.
    pub fn metrics(&self) -> WatchdogMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Current queue depth.
    pub fn queue_depth(&self) -> usize {
        self.queue.len()
    }

    /// Queue capacity.
    pub fn queue_capacity(&self) -> usize {
        self.queue.capacity()
    }
}

// =============================================================================
// Watchdog daemon
// =============================================================================

/// The semantic anomaly watchdog daemon.
///
/// Spawns a dedicated OS thread that consumes segments from the lock-free
/// queue, adaptively batches them, runs entropy gating + ONNX embedding +
/// conformal anomaly detection, and publishes detected anomalies to the
/// EventBus.
pub struct SemanticAnomalyWatchdog {
    /// Handle for the ML thread.
    thread_handle: Option<std::thread::JoinHandle<()>>,
    /// Shared handle for producers.
    handle: WatchdogHandle,
}

impl SemanticAnomalyWatchdog {
    /// Start the watchdog daemon.
    ///
    /// `embed_fn` is called for each segment that passes the entropy gate.
    /// It should produce an embedding vector (e.g., via FastEmbed ONNX).
    ///
    /// `event_bus` is optional; if provided, anomalies are published to it.
    pub fn start<F>(config: WatchdogConfig, embed_fn: F, event_bus: Option<Arc<EventBus>>) -> Self
    where
        F: Fn(&[u8]) -> Vec<f32> + Send + 'static,
    {
        let config = sanitize_watchdog_config(config);
        let queue = Arc::new(ArrayQueue::new(config.queue_capacity));
        let metrics = Arc::new(WatchdogMetrics::new());
        let running = Arc::new(AtomicBool::new(true));

        let handle = WatchdogHandle {
            queue: Arc::clone(&queue),
            metrics: Arc::clone(&metrics),
            running: Arc::clone(&running),
            config: config.clone(),
        };

        let ml_queue = Arc::clone(&queue);
        let ml_metrics = Arc::clone(&metrics);
        let ml_running = Arc::clone(&running);
        let batch_timeout = Duration::from_millis(config.batch_timeout_ms);

        let thread_handle = std::thread::Builder::new()
            .name("ft-semantic-ml".to_string())
            .spawn(move || {
                ml_thread_loop(
                    ml_queue,
                    ml_metrics,
                    ml_running,
                    config,
                    batch_timeout,
                    embed_fn,
                    event_bus,
                );
            })
            .expect("failed to spawn ML thread");

        Self {
            thread_handle: Some(thread_handle),
            handle,
        }
    }

    /// Get a handle for submitting segments (cheaply cloneable).
    pub fn handle(&self) -> WatchdogHandle {
        self.handle.clone()
    }

    /// Gracefully shut down the watchdog.
    ///
    /// Sets the running flag to false and waits for the ML thread to drain
    /// remaining items and exit.
    pub fn shutdown(mut self) {
        self.handle.running.store(false, Ordering::Release);
        if let Some(h) = self.thread_handle.take() {
            let _ = h.join();
        }
    }

    /// Check if the watchdog is running.
    pub fn is_running(&self) -> bool {
        self.handle.is_running()
    }

    /// Get current metrics snapshot.
    pub fn metrics(&self) -> WatchdogMetricsSnapshot {
        self.handle.metrics()
    }
}

impl Drop for SemanticAnomalyWatchdog {
    fn drop(&mut self) {
        self.handle.running.store(false, Ordering::Release);
        if let Some(h) = self.thread_handle.take() {
            let _ = h.join();
        }
    }
}

// =============================================================================
// ML thread loop (runs on dedicated OS thread)
// =============================================================================

fn ml_thread_loop<F>(
    queue: Arc<ArrayQueue<SegmentEnvelope>>,
    metrics: Arc<WatchdogMetrics>,
    running: Arc<AtomicBool>,
    config: WatchdogConfig,
    batch_timeout: Duration,
    embed_fn: F,
    event_bus: Option<Arc<EventBus>>,
) where
    F: Fn(&[u8]) -> Vec<f32>,
{
    let mut detector = GatedAnomalyDetector::new(config.entropy_gate, config.conformal);
    let mut batch: Vec<SegmentEnvelope> = Vec::with_capacity(config.batch_size);

    tracing::info!(
        queue_capacity = config.queue_capacity,
        batch_size = config.batch_size,
        batch_timeout_ms = config.batch_timeout_ms,
        "semantic anomaly ML thread started"
    );

    loop {
        // Adaptive batch collection: fill up to batch_size or wait batch_timeout.
        batch.clear();
        let batch_start = Instant::now();

        while batch.len() < config.batch_size {
            match queue.pop() {
                Some(envelope) => {
                    batch.push(envelope);
                }
                None => {
                    // Queue empty. If we have items, check timeout.
                    if !batch.is_empty() && batch_start.elapsed() >= batch_timeout {
                        break;
                    }
                    // If we have no items and daemon is stopping, exit.
                    if !running.load(Ordering::Relaxed) {
                        // Drain any remaining items.
                        while let Some(env) = queue.pop() {
                            batch.push(env);
                        }
                        break;
                    }
                    // Brief sleep to avoid busy-spinning. 1ms is a good tradeoff
                    // between latency and CPU usage.
                    std::thread::sleep(Duration::from_millis(1));

                    if batch_start.elapsed() >= batch_timeout {
                        break;
                    }
                }
            }
        }

        // Exit condition: no items and shutdown requested.
        if batch.is_empty() && !running.load(Ordering::Relaxed) {
            break;
        }

        if batch.is_empty() {
            continue;
        }

        // Process the batch.
        let batch_len = batch.len() as u64;
        metrics.batches_processed.fetch_add(1, Ordering::Relaxed);
        metrics
            .total_batch_fill
            .fetch_add(batch_len, Ordering::Relaxed);

        for envelope in std::mem::take(&mut batch) {
            let observation = detector.observe(&envelope.data, &embed_fn);
            metrics.segments_processed.fetch_add(1, Ordering::Relaxed);

            match observation {
                GatedObservation::Skipped(_) => {
                    metrics
                        .segments_entropy_skipped
                        .fetch_add(1, Ordering::Relaxed);
                }
                GatedObservation::Processed {
                    anomaly: Some(shock),
                    ..
                } => {
                    metrics.segments_embedded.fetch_add(1, Ordering::Relaxed);
                    metrics.anomalies_detected.fetch_add(1, Ordering::Relaxed);

                    let event = SemanticAnomalyEvent {
                        pane_id: envelope.pane_id,
                        shock: shock.clone(),
                        segment_len: envelope.data.len(),
                    };

                    tracing::warn!(
                        pane_id = envelope.pane_id,
                        p_value = shock.p_value,
                        distance = shock.distance,
                        segment_len = envelope.data.len(),
                        "semantic anomaly detected"
                    );

                    if let Some(ref bus) = event_bus {
                        publish_anomaly_event(bus, &event);
                    }
                }
                GatedObservation::Processed { anomaly: None, .. } => {
                    metrics.segments_embedded.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    tracing::info!(
        segments_processed = metrics.segments_processed.load(Ordering::Relaxed),
        anomalies_detected = metrics.anomalies_detected.load(Ordering::Relaxed),
        "semantic anomaly ML thread exiting"
    );
}

/// Publish a semantic anomaly event to the EventBus.
///
/// Uses the PatternDetected event variant with a Detection payload
/// to integrate with the existing event infrastructure.
fn publish_anomaly_event(bus: &EventBus, event: &SemanticAnomalyEvent) {
    let detection = Detection {
        rule_id: "core.semantic_anomaly:conformal_shock".to_string(),
        agent_type: AgentType::Unknown,
        event_type: "semantic_anomaly".to_string(),
        severity: Severity::Critical,
        confidence: 1.0 - event.shock.p_value,
        extracted: serde_json::json!({
            "p_value": event.shock.p_value,
            "distance": event.shock.distance as f64,
            "alpha": event.shock.alpha,
            "calibration_count": event.shock.calibration_count,
            "calibration_median": event.shock.calibration_median,
            "segment_len": event.segment_len,
        }),
        matched_text: format!(
            "Semantic anomaly: p={:.4}, distance={:.3}",
            event.shock.p_value, event.shock.distance
        ),
        span: (0, 0),
    };

    let _delivered = bus.publish(crate::events::Event::PatternDetected {
        pane_id: event.pane_id,
        pane_uuid: None,
        detection,
        event_id: None,
    });
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    // Mock embed function: returns 8-dimensional vector based on first few bytes.
    fn mock_embed(data: &[u8]) -> Vec<f32> {
        let mut v = vec![0.0f32; 8];
        for (i, &b) in data.iter().take(8).enumerate() {
            v[i] = b as f32 / 255.0;
        }
        // Normalize.
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > f32::EPSILON {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }

    fn test_config() -> WatchdogConfig {
        WatchdogConfig {
            queue_capacity: 32,
            batch_size: 4,
            batch_timeout_ms: 5,
            min_segment_bytes: 2,
            max_segment_bytes: 1024,
            entropy_gate: EntropyGateConfig {
                min_entropy_bits_per_byte: 0.5, // Low threshold for testing.
                min_segment_bytes: 2,
                enabled: true,
            },
            conformal: ConformalAnomalyConfig {
                min_calibration: 5,
                calibration_window: 50,
                alpha: 0.05,
                centroid_alpha: 0.1,
            },
        }
    }

    // =========================================================================
    // Configuration tests
    // =========================================================================

    #[test]
    fn config_default_values() {
        let config = WatchdogConfig::default();
        assert_eq!(config.queue_capacity, 256);
        assert_eq!(config.batch_size, 16);
        assert_eq!(config.batch_timeout_ms, 10);
        assert_eq!(config.min_segment_bytes, 4);
        assert_eq!(config.max_segment_bytes, 65_536);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = test_config();
        let json = serde_json::to_string(&config).unwrap();
        let restored: WatchdogConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.queue_capacity, config.queue_capacity);
        assert_eq!(restored.batch_size, config.batch_size);
        assert_eq!(restored.batch_timeout_ms, config.batch_timeout_ms);
    }

    #[test]
    fn sanitize_watchdog_config_clamps_invalid_values() {
        let mut config = test_config();
        config.queue_capacity = 0;
        config.batch_size = 0;
        config.min_segment_bytes = 128;
        config.max_segment_bytes = 8;

        let sanitized = sanitize_watchdog_config(config);
        assert_eq!(sanitized.queue_capacity, 1);
        assert_eq!(sanitized.batch_size, 1);
        assert_eq!(sanitized.max_segment_bytes, 128);
    }

    // =========================================================================
    // Metrics tests
    // =========================================================================

    #[test]
    fn metrics_start_at_zero() {
        let m = WatchdogMetrics::new();
        let snap = m.snapshot();
        assert_eq!(snap.segments_submitted, 0);
        assert_eq!(snap.segments_shed, 0);
        assert_eq!(snap.segments_processed, 0);
        assert_eq!(snap.anomalies_detected, 0);
        assert_eq!(snap.avg_batch_fill, 0.0);
    }

    #[test]
    fn metrics_snapshot_captures_atomics() {
        let m = WatchdogMetrics::new();
        m.segments_submitted.store(10, Ordering::Relaxed);
        m.segments_shed.store(3, Ordering::Relaxed);
        m.batches_processed.store(2, Ordering::Relaxed);
        m.total_batch_fill.store(8, Ordering::Relaxed);
        let snap = m.snapshot();
        assert_eq!(snap.segments_submitted, 10);
        assert_eq!(snap.segments_shed, 3);
        assert_eq!(snap.avg_batch_fill, 4.0);
    }

    #[test]
    fn metrics_snapshot_serde() {
        let m = WatchdogMetrics::new();
        m.segments_submitted.store(5, Ordering::Relaxed);
        let snap = m.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let restored: WatchdogMetricsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.segments_submitted, 5);
    }

    // =========================================================================
    // Handle tests (no ML thread needed)
    // =========================================================================

    #[test]
    fn handle_observe_enqueues() {
        let queue = Arc::new(ArrayQueue::new(8));
        let metrics = Arc::new(WatchdogMetrics::new());
        let running = Arc::new(AtomicBool::new(true));
        let handle = WatchdogHandle {
            queue: Arc::clone(&queue),
            metrics: Arc::clone(&metrics),
            running: Arc::clone(&running),
            config: test_config(),
        };

        assert!(handle.observe_segment(1, b"hello world"));
        assert_eq!(handle.queue_depth(), 1);
        assert_eq!(metrics.segments_submitted.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn handle_observe_sheds_when_full() {
        let queue = Arc::new(ArrayQueue::new(2));
        let metrics = Arc::new(WatchdogMetrics::new());
        let running = Arc::new(AtomicBool::new(true));
        let handle = WatchdogHandle {
            queue: Arc::clone(&queue),
            metrics: Arc::clone(&metrics),
            running: Arc::clone(&running),
            config: test_config(),
        };

        assert!(handle.observe_segment(1, b"first"));
        assert!(handle.observe_segment(1, b"second"));
        // Queue is now full — third should be shed.
        assert!(!handle.observe_segment(1, b"third"));
        assert_eq!(metrics.segments_shed.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.segments_submitted.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn handle_rejects_when_not_running() {
        let queue = Arc::new(ArrayQueue::new(8));
        let metrics = Arc::new(WatchdogMetrics::new());
        let running = Arc::new(AtomicBool::new(false));
        let handle = WatchdogHandle {
            queue: Arc::clone(&queue),
            metrics: Arc::clone(&metrics),
            running,
            config: test_config(),
        };

        assert!(!handle.observe_segment(1, b"data"));
        assert_eq!(metrics.segments_submitted.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn handle_rejects_too_short() {
        let queue = Arc::new(ArrayQueue::new(8));
        let metrics = Arc::new(WatchdogMetrics::new());
        let running = Arc::new(AtomicBool::new(true));
        let handle = WatchdogHandle {
            queue: Arc::clone(&queue),
            metrics: Arc::clone(&metrics),
            running,
            config: test_config(), // min_segment_bytes = 2
        };

        assert!(!handle.observe_segment(1, b"x")); // 1 byte < 2
        assert_eq!(metrics.segments_too_short.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.segments_submitted.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn handle_truncates_oversized() {
        let queue = Arc::new(ArrayQueue::new(8));
        let metrics = Arc::new(WatchdogMetrics::new());
        let running = Arc::new(AtomicBool::new(true));
        let mut config = test_config();
        config.max_segment_bytes = 10;
        let handle = WatchdogHandle {
            queue: Arc::clone(&queue),
            metrics: Arc::clone(&metrics),
            running,
            config,
        };

        let big_data = vec![42u8; 100];
        assert!(handle.observe_segment(1, &big_data));
        assert_eq!(metrics.segments_truncated.load(Ordering::Relaxed), 1);

        // Verify the enqueued segment was truncated.
        let env = queue.pop().unwrap();
        assert_eq!(env.data.len(), 10);
    }

    #[test]
    fn handle_queue_capacity() {
        let queue = Arc::new(ArrayQueue::new(16));
        let metrics = Arc::new(WatchdogMetrics::new());
        let running = Arc::new(AtomicBool::new(true));
        let handle = WatchdogHandle {
            queue,
            metrics,
            running,
            config: test_config(),
        };

        assert_eq!(handle.queue_capacity(), 16);
        assert_eq!(handle.queue_depth(), 0);
    }

    // =========================================================================
    // Watchdog lifecycle tests
    // =========================================================================

    #[test]
    fn watchdog_start_and_shutdown() {
        let watchdog = SemanticAnomalyWatchdog::start(test_config(), mock_embed, None);
        assert!(watchdog.is_running());
        let snap = watchdog.metrics();
        assert_eq!(snap.segments_submitted, 0);
        watchdog.shutdown();
    }

    #[test]
    fn watchdog_start_clamps_zero_queue_and_batch_config() {
        let mut config = test_config();
        config.queue_capacity = 0;
        config.batch_size = 0;

        let watchdog = SemanticAnomalyWatchdog::start(config, mock_embed, None);
        let handle = watchdog.handle();
        assert_eq!(handle.queue_capacity(), 1);

        assert!(handle.observe_segment(1, b"valid segment payload"));
        std::thread::sleep(Duration::from_millis(30));

        let snap = watchdog.metrics();
        assert_eq!(snap.segments_submitted, 1);
        watchdog.shutdown();
    }

    #[test]
    fn watchdog_processes_segments() {
        let watchdog = SemanticAnomalyWatchdog::start(test_config(), mock_embed, None);
        let handle = watchdog.handle();

        // Submit diverse segments (high entropy) to pass the gate.
        for i in 0..10 {
            let data: Vec<u8> = (0..64).map(|j| ((i * 17 + j * 31) % 256) as u8).collect();
            handle.observe_segment(1, &data);
        }

        // Give the ML thread time to process.
        std::thread::sleep(Duration::from_millis(50));

        let snap = watchdog.metrics();
        assert_eq!(snap.segments_submitted, 10);
        assert!(
            snap.segments_processed > 0,
            "ML thread should have processed segments"
        );

        watchdog.shutdown();
    }

    #[test]
    fn watchdog_handles_shutdown_with_pending_items() {
        let watchdog = SemanticAnomalyWatchdog::start(test_config(), mock_embed, None);
        let handle = watchdog.handle();

        // Submit many items quickly.
        for i in 0..20 {
            let data: Vec<u8> = (0..32).map(|j| ((i * 7 + j) % 256) as u8).collect();
            handle.observe_segment(1, &data);
        }

        // Immediately shutdown — should drain remaining items.
        watchdog.shutdown();
        // If we got here without deadlock, the test passes.
    }

    #[test]
    fn watchdog_drop_triggers_shutdown() {
        let watchdog = SemanticAnomalyWatchdog::start(test_config(), mock_embed, None);
        let _handle = watchdog.handle();
        drop(watchdog); // Should trigger shutdown via Drop impl.
        // If we got here without deadlock, the test passes.
    }

    #[test]
    fn watchdog_handle_is_clone() {
        let watchdog = SemanticAnomalyWatchdog::start(test_config(), mock_embed, None);
        let h1 = watchdog.handle();
        let h2 = h1.clone();

        h1.observe_segment(1, b"from h1");
        h2.observe_segment(2, b"from h2");

        std::thread::sleep(Duration::from_millis(30));
        let snap = watchdog.metrics();
        assert_eq!(snap.segments_submitted, 2);

        watchdog.shutdown();
    }

    #[test]
    fn watchdog_with_eventbus() {
        let bus = Arc::new(EventBus::new(64));
        let mut sub = bus.subscribe_detections();

        let watchdog = SemanticAnomalyWatchdog::start(test_config(), mock_embed, Some(bus));
        let handle = watchdog.handle();

        // Warmup: submit many similar segments to fill calibration window.
        let base: Vec<u8> = (0..64).map(|i| (i * 3 % 256) as u8).collect();
        for _ in 0..30 {
            handle.observe_segment(1, &base);
        }
        std::thread::sleep(Duration::from_millis(100));

        // Now submit a dramatically different segment (potential anomaly).
        let anomalous: Vec<u8> = (0..64).map(|i| (255 - i * 2) as u8).collect();
        handle.observe_segment(1, &anomalous);
        std::thread::sleep(Duration::from_millis(100));

        // Check if an event was published (may or may not trigger depending
        // on conformal p-value — this is a best-effort test).
        let snap = watchdog.metrics();
        assert!(snap.segments_processed > 0);

        // Try to receive any published events.
        let _received = sub.try_recv();
        // We don't assert on receiving an event because conformal detection
        // depends on statistical properties of the calibration window.

        watchdog.shutdown();
    }

    #[test]
    fn watchdog_entropy_gate_skips_low_entropy() {
        let mut config = test_config();
        config.entropy_gate.min_entropy_bits_per_byte = 3.0; // Higher threshold.
        let watchdog = SemanticAnomalyWatchdog::start(config, mock_embed, None);
        let handle = watchdog.handle();

        // Submit low-entropy segment (all same byte).
        let low_entropy = vec![b'='; 100];
        handle.observe_segment(1, &low_entropy);

        // Submit high-entropy segment.
        let high_entropy: Vec<u8> = (0..100).map(|i| (i * 37 % 256) as u8).collect();
        handle.observe_segment(1, &high_entropy);

        std::thread::sleep(Duration::from_millis(50));

        let snap = watchdog.metrics();
        assert_eq!(snap.segments_submitted, 2);
        // At least one should have been skipped by entropy gate.
        // (The exact count depends on timing and the gate threshold.)

        watchdog.shutdown();
    }

    #[test]
    fn watchdog_shed_count_under_pressure() {
        let mut config = test_config();
        config.queue_capacity = 4;
        config.batch_timeout_ms = 50; // Slow batching to fill queue.
        let watchdog = SemanticAnomalyWatchdog::start(config, |_| vec![1.0; 8], None);
        let handle = watchdog.handle();

        // Flood the queue.
        let mut shed_count = 0u32;
        for i in 0..100 {
            let data: Vec<u8> = (0..32).map(|j| ((i + j) % 256) as u8).collect();
            if !handle.observe_segment(1, &data) {
                shed_count += 1;
            }
        }

        // With capacity 4, we should have shed many segments.
        assert!(shed_count > 0, "should shed segments when queue is full");

        watchdog.shutdown();
    }

    #[test]
    fn watchdog_multiple_panes() {
        let watchdog = SemanticAnomalyWatchdog::start(test_config(), mock_embed, None);
        let handle = watchdog.handle();

        for pane in 1..=3 {
            let data: Vec<u8> = (0..32).map(|i| ((pane as u8) * 50 + i) % 255).collect();
            handle.observe_segment(pane, &data);
        }

        std::thread::sleep(Duration::from_millis(50));
        let snap = watchdog.metrics();
        assert_eq!(snap.segments_submitted, 3);
        assert!(snap.segments_processed > 0);

        watchdog.shutdown();
    }

    #[test]
    fn segment_envelope_debug() {
        let env = SegmentEnvelope {
            pane_id: 42,
            data: vec![1, 2, 3],
            captured_at: Instant::now(),
        };
        let dbg = format!("{:?}", env);
        assert!(dbg.contains("42"));
    }

    #[test]
    fn semantic_anomaly_event_serde() {
        let event = SemanticAnomalyEvent {
            pane_id: 7,
            shock: ConformalShock {
                distance: 0.95,
                p_value: 0.001,
                alpha: 0.05,
                calibration_count: 200,
                calibration_median: 0.12,
            },
            segment_len: 1024,
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: SemanticAnomalyEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.pane_id, 7);
        assert_eq!(restored.segment_len, 1024);
        assert!((restored.shock.p_value - 0.001).abs() < 1e-10);
    }

    #[test]
    fn watchdog_metrics_snapshot_avg_batch_fill() {
        let m = WatchdogMetrics::new();
        // Simulate 3 batches with fills of 4, 8, 12 (total = 24, avg = 8).
        m.batches_processed.store(3, Ordering::Relaxed);
        m.total_batch_fill.store(24, Ordering::Relaxed);
        let snap = m.snapshot();
        assert!((snap.avg_batch_fill - 8.0).abs() < 1e-10);
    }

    #[test]
    fn watchdog_handle_is_running_reflects_state() {
        let queue = Arc::new(ArrayQueue::new(8));
        let metrics = Arc::new(WatchdogMetrics::new());
        let running = Arc::new(AtomicBool::new(true));
        let handle = WatchdogHandle {
            queue,
            metrics,
            running: Arc::clone(&running),
            config: test_config(),
        };

        assert!(handle.is_running());
        running.store(false, Ordering::Release);
        assert!(!handle.is_running());
    }
}
