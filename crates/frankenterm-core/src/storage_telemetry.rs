//! Storage-pipeline telemetry, diagnostics, and SLO-oriented metrics.
//!
//! Bead: wa-oegrb.3.6
//!
//! Instruments the recorder storage pipeline with:
//! - Per-operation latency histograms (append, flush, checkpoint)
//! - Throughput counters (events appended, bytes written, batches processed)
//! - Error class counters for triage
//! - Health tier classification (Green/Yellow/Red/Black)
//! - EWMA-smoothed append rate for trend detection
//! - Instrumented storage wrapper (`InstrumentedStorage<S>`) via decorator pattern
//! - Snapshot export for diagnostics and SLO dashboards

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::ewma::Ewma;
use crate::recorder_storage::{
    AppendRequest, AppendResponse, CheckpointCommitOutcome, CheckpointConsumerId, FlushMode,
    FlushStats, RecorderCheckpoint, RecorderStorageError, RecorderStorageErrorClass,
    RecorderStorageHealth, RecorderStorageLag,
};
use crate::telemetry::{HistogramSummary, MetricRegistry};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for storage-pipeline telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageTelemetryConfig {
    /// Maximum histogram samples retained for quantile estimation.
    pub histogram_max_samples: usize,

    /// Queue depth ratio thresholds for tier classification.
    /// [yellow_threshold, red_threshold, black_threshold] as fractions of capacity.
    pub tier_thresholds: [f64; 3],

    /// EWMA half-life for append rate smoothing (milliseconds).
    pub rate_ewma_half_life_ms: f64,

    /// SLO targets: append p95 latency ceiling (microseconds).
    pub slo_append_p95_us: f64,

    /// SLO targets: flush p95 latency ceiling (microseconds).
    pub slo_flush_p95_us: f64,
}

impl Default for StorageTelemetryConfig {
    fn default() -> Self {
        Self {
            histogram_max_samples: 1024,
            tier_thresholds: [0.5, 0.8, 0.95],
            rate_ewma_half_life_ms: 5000.0,
            slo_append_p95_us: 10_000.0,  // 10ms target
            slo_flush_p95_us: 100_000.0,  // 100ms target
        }
    }
}

// =============================================================================
// Health tier classification
// =============================================================================

/// Storage pipeline health tier, following the project-wide backpressure pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageHealthTier {
    /// Nominal operation. Queue depth < yellow threshold.
    Green = 0,
    /// Elevated load. Queue depth >= yellow but < red.
    Yellow = 1,
    /// High pressure. Queue depth >= red but < black.
    Red = 2,
    /// Critical overload or degraded backend.
    Black = 3,
}

impl StorageHealthTier {
    /// Classify health from queue depth ratio and degraded flag.
    #[must_use]
    pub fn classify(queue_ratio: f64, degraded: bool, thresholds: &[f64; 3]) -> Self {
        if degraded || queue_ratio >= thresholds[2] {
            StorageHealthTier::Black
        } else if queue_ratio >= thresholds[1] {
            StorageHealthTier::Red
        } else if queue_ratio >= thresholds[0] {
            StorageHealthTier::Yellow
        } else {
            StorageHealthTier::Green
        }
    }
}

// =============================================================================
// Metric names (constants for stable keys)
// =============================================================================

/// Histogram: append_batch latency in microseconds.
pub const METRIC_APPEND_LATENCY_US: &str = "storage.append_latency_us";
/// Histogram: flush latency in microseconds.
pub const METRIC_FLUSH_LATENCY_US: &str = "storage.flush_latency_us";
/// Histogram: checkpoint commit latency in microseconds.
pub const METRIC_CHECKPOINT_LATENCY_US: &str = "storage.checkpoint_latency_us";
/// Histogram: batch size (number of events per append).
pub const METRIC_BATCH_SIZE: &str = "storage.batch_size";

/// Counter: total events appended.
pub const COUNTER_EVENTS_APPENDED: &str = "storage.events_appended";
/// Counter: total batches processed.
pub const COUNTER_BATCHES_PROCESSED: &str = "storage.batches_processed";
/// Counter: total bytes written (estimated from event count × avg size).
pub const COUNTER_BYTES_WRITTEN: &str = "storage.bytes_written";
/// Counter: total flush operations.
pub const COUNTER_FLUSHES: &str = "storage.flushes";
/// Counter: total checkpoint commits.
pub const COUNTER_CHECKPOINTS: &str = "storage.checkpoints";
/// Counter: checkpoint commits that advanced.
pub const COUNTER_CHECKPOINT_ADVANCED: &str = "storage.checkpoint_advanced";
/// Counter: checkpoint no-ops (already advanced).
pub const COUNTER_CHECKPOINT_NOOP: &str = "storage.checkpoint_noop";
/// Counter: idempotent batch replays (duplicate batch_id).
pub const COUNTER_IDEMPOTENT_REPLAYS: &str = "storage.idempotent_replays";

/// Counter: total errors by class.
pub const COUNTER_ERROR_OVERLOAD: &str = "storage.errors.overload";
pub const COUNTER_ERROR_RETRYABLE: &str = "storage.errors.retryable";
pub const COUNTER_ERROR_TERMINAL_DATA: &str = "storage.errors.terminal_data";
pub const COUNTER_ERROR_CORRUPTION: &str = "storage.errors.corruption";

// =============================================================================
// SLO status
// =============================================================================

/// SLO compliance status for a single metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloStatus {
    /// Within SLO target.
    Met,
    /// Exceeding SLO target.
    Breached,
    /// Insufficient data to evaluate.
    Unknown,
}

// =============================================================================
// Pipeline snapshot
// =============================================================================

/// Point-in-time diagnostic snapshot of storage pipeline telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoragePipelineSnapshot {
    /// Timestamp when this snapshot was taken (ms since epoch).
    pub timestamp_ms: u64,

    /// Current health tier.
    pub health_tier: StorageHealthTier,

    /// Last observed health from the storage backend.
    pub health: Option<RecorderStorageHealth>,

    /// Last observed consumer lag.
    pub lag: Option<RecorderStorageLag>,

    /// Append latency summary (microseconds).
    pub append_latency: Option<HistogramSummary>,

    /// Flush latency summary (microseconds).
    pub flush_latency: Option<HistogramSummary>,

    /// Checkpoint commit latency summary (microseconds).
    pub checkpoint_latency: Option<HistogramSummary>,

    /// Batch size distribution.
    pub batch_size: Option<HistogramSummary>,

    /// Total events appended since start.
    pub total_events_appended: u64,

    /// Total batches processed since start.
    pub total_batches: u64,

    /// Total flush operations since start.
    pub total_flushes: u64,

    /// Total checkpoint operations since start.
    pub total_checkpoints: u64,

    /// EWMA-smoothed append rate (events per second).
    pub append_rate_ewma: f64,

    /// Error counts by class.
    pub errors: ErrorCounts,

    /// SLO compliance.
    pub slo_append_p95: SloStatus,
    pub slo_flush_p95: SloStatus,
}

/// Aggregated error counts by error class.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ErrorCounts {
    pub overload: u64,
    pub retryable: u64,
    pub terminal_data: u64,
    pub corruption: u64,
}

impl ErrorCounts {
    /// Total errors across all classes.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.overload + self.retryable + self.terminal_data + self.corruption
    }
}

// =============================================================================
// StorageTelemetry — core collector
// =============================================================================

/// Storage pipeline telemetry collector.
///
/// Thread-safe, designed for concurrent access from async tasks.
/// Uses [`MetricRegistry`] for histograms/counters and [`Ewma`] for rate smoothing.
pub struct StorageTelemetry {
    config: StorageTelemetryConfig,
    registry: Arc<MetricRegistry>,
    rate_ewma: std::sync::Mutex<Ewma>,
    last_health: std::sync::RwLock<Option<RecorderStorageHealth>>,
    last_lag: std::sync::RwLock<Option<RecorderStorageLag>>,
    created_at: Instant,
    /// Monotonic counter for estimating bytes written (from accepted_count × estimate).
    estimated_bytes: AtomicU64,
}

impl StorageTelemetry {
    /// Create a new telemetry collector with the given configuration.
    #[must_use]
    pub fn new(config: StorageTelemetryConfig) -> Self {
        let registry = Arc::new(MetricRegistry::new());

        // Register histograms
        let max = config.histogram_max_samples;
        registry.register_histogram(METRIC_APPEND_LATENCY_US, max);
        registry.register_histogram(METRIC_FLUSH_LATENCY_US, max);
        registry.register_histogram(METRIC_CHECKPOINT_LATENCY_US, max);
        registry.register_histogram(METRIC_BATCH_SIZE, max);

        let rate_ewma = std::sync::Mutex::new(Ewma::with_half_life_ms(config.rate_ewma_half_life_ms));

        Self {
            config,
            registry,
            rate_ewma,
            last_health: std::sync::RwLock::new(None),
            last_lag: std::sync::RwLock::new(None),
            created_at: Instant::now(),
            estimated_bytes: AtomicU64::new(0),
        }
    }

    /// Create with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(StorageTelemetryConfig::default())
    }

    /// Access the underlying metric registry (for external export).
    #[must_use]
    pub fn registry(&self) -> &Arc<MetricRegistry> {
        &self.registry
    }

    /// Record an append_batch operation.
    pub fn record_append(
        &self,
        elapsed_us: f64,
        event_count: usize,
        estimated_bytes: u64,
        was_idempotent_replay: bool,
    ) {
        self.registry.record_histogram(METRIC_APPEND_LATENCY_US, elapsed_us);
        self.registry.record_histogram(METRIC_BATCH_SIZE, event_count as f64);
        self.registry.add_counter(COUNTER_EVENTS_APPENDED, event_count as u64);
        self.registry.increment_counter(COUNTER_BATCHES_PROCESSED);
        self.estimated_bytes.fetch_add(estimated_bytes, Ordering::Relaxed);
        self.registry.add_counter(COUNTER_BYTES_WRITTEN, estimated_bytes);

        if was_idempotent_replay {
            self.registry.increment_counter(COUNTER_IDEMPOTENT_REPLAYS);
        }

        // Update rate EWMA
        let now_ms = self.created_at.elapsed().as_millis() as u64;
        if let Ok(mut ewma) = self.rate_ewma.lock() {
            ewma.observe(event_count as f64, now_ms);
        }
    }

    /// Record a flush operation.
    pub fn record_flush(&self, elapsed_us: f64) {
        self.registry.record_histogram(METRIC_FLUSH_LATENCY_US, elapsed_us);
        self.registry.increment_counter(COUNTER_FLUSHES);
    }

    /// Record a checkpoint commit operation.
    pub fn record_checkpoint(&self, elapsed_us: f64, outcome: CheckpointCommitOutcome) {
        self.registry.record_histogram(METRIC_CHECKPOINT_LATENCY_US, elapsed_us);
        self.registry.increment_counter(COUNTER_CHECKPOINTS);
        match outcome {
            CheckpointCommitOutcome::Advanced => {
                self.registry.increment_counter(COUNTER_CHECKPOINT_ADVANCED);
            }
            CheckpointCommitOutcome::NoopAlreadyAdvanced => {
                self.registry.increment_counter(COUNTER_CHECKPOINT_NOOP);
            }
            CheckpointCommitOutcome::RejectedOutOfOrder => {
                // Rejection is tracked by the error counter path
            }
        }
    }

    /// Record an error by its class.
    pub fn record_error(&self, class: RecorderStorageErrorClass) {
        match class {
            RecorderStorageErrorClass::Overload => {
                self.registry.increment_counter(COUNTER_ERROR_OVERLOAD);
            }
            RecorderStorageErrorClass::Retryable => {
                self.registry.increment_counter(COUNTER_ERROR_RETRYABLE);
            }
            RecorderStorageErrorClass::TerminalData | RecorderStorageErrorClass::TerminalConfig => {
                self.registry.increment_counter(COUNTER_ERROR_TERMINAL_DATA);
            }
            RecorderStorageErrorClass::Corruption => {
                self.registry.increment_counter(COUNTER_ERROR_CORRUPTION);
            }
            RecorderStorageErrorClass::DependencyUnavailable => {
                self.registry.increment_counter(COUNTER_ERROR_RETRYABLE);
            }
        }
    }

    /// Update the last-observed health snapshot.
    pub fn update_health(&self, health: RecorderStorageHealth) {
        if let Ok(mut w) = self.last_health.write() {
            *w = Some(health);
        }
    }

    /// Update the last-observed lag snapshot.
    pub fn update_lag(&self, lag: RecorderStorageLag) {
        if let Ok(mut w) = self.last_lag.write() {
            *w = Some(lag);
        }
    }

    /// Current health tier based on last-observed health.
    #[must_use]
    pub fn current_tier(&self) -> StorageHealthTier {
        let health = self.last_health.read().ok().and_then(|h| h.clone());
        match health {
            Some(h) => {
                let ratio = if h.queue_capacity > 0 {
                    h.queue_depth as f64 / h.queue_capacity as f64
                } else {
                    0.0
                };
                StorageHealthTier::classify(ratio, h.degraded, &self.config.tier_thresholds)
            }
            None => StorageHealthTier::Green, // no data yet
        }
    }

    /// Current EWMA-smoothed append rate (events per second).
    #[must_use]
    pub fn append_rate(&self) -> f64 {
        self.rate_ewma
            .lock()
            .ok()
            .map(|e| e.value())
            .unwrap_or(0.0)
    }

    /// Produce a full diagnostic snapshot.
    #[must_use]
    pub fn snapshot(&self) -> StoragePipelineSnapshot {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);

        let summaries = self.registry.histogram_summaries();
        let find_summary = |name: &str| -> Option<HistogramSummary> {
            summaries.iter().find(|s| s.name == name).cloned()
        };

        let append_summary = find_summary(METRIC_APPEND_LATENCY_US);
        let flush_summary = find_summary(METRIC_FLUSH_LATENCY_US);

        let slo_append_p95 = match &append_summary {
            Some(s) => match s.p95 {
                Some(p95) if p95 <= self.config.slo_append_p95_us => SloStatus::Met,
                Some(_) => SloStatus::Breached,
                None => SloStatus::Unknown,
            },
            None => SloStatus::Unknown,
        };

        let slo_flush_p95 = match &flush_summary {
            Some(s) => match s.p95 {
                Some(p95) if p95 <= self.config.slo_flush_p95_us => SloStatus::Met,
                Some(_) => SloStatus::Breached,
                None => SloStatus::Unknown,
            },
            None => SloStatus::Unknown,
        };

        let errors = ErrorCounts {
            overload: self.registry.counter_value(COUNTER_ERROR_OVERLOAD),
            retryable: self.registry.counter_value(COUNTER_ERROR_RETRYABLE),
            terminal_data: self.registry.counter_value(COUNTER_ERROR_TERMINAL_DATA),
            corruption: self.registry.counter_value(COUNTER_ERROR_CORRUPTION),
        };

        StoragePipelineSnapshot {
            timestamp_ms: now_ms,
            health_tier: self.current_tier(),
            health: self.last_health.read().ok().and_then(|h| h.clone()),
            lag: self.last_lag.read().ok().and_then(|l| l.clone()),
            append_latency: append_summary,
            flush_latency: flush_summary,
            checkpoint_latency: find_summary(METRIC_CHECKPOINT_LATENCY_US),
            batch_size: find_summary(METRIC_BATCH_SIZE),
            total_events_appended: self.registry.counter_value(COUNTER_EVENTS_APPENDED),
            total_batches: self.registry.counter_value(COUNTER_BATCHES_PROCESSED),
            total_flushes: self.registry.counter_value(COUNTER_FLUSHES),
            total_checkpoints: self.registry.counter_value(COUNTER_CHECKPOINTS),
            append_rate_ewma: self.append_rate(),
            errors,
            slo_append_p95,
            slo_flush_p95,
        }
    }

    /// Configuration reference.
    #[must_use]
    pub fn config(&self) -> &StorageTelemetryConfig {
        &self.config
    }
}

impl std::fmt::Debug for StorageTelemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageTelemetry")
            .field("config", &self.config)
            .field("tier", &self.current_tier())
            .finish()
    }
}

// =============================================================================
// InstrumentedStorage — decorator wrapper
// =============================================================================

/// Decorator that wraps any [`RecorderStorage`] implementation with automatic
/// telemetry recording. Each trait method is timed and its outcome is recorded
/// to the [`StorageTelemetry`] collector.
pub struct InstrumentedStorage<S> {
    inner: S,
    telemetry: Arc<StorageTelemetry>,
}

impl<S> InstrumentedStorage<S> {
    /// Wrap a storage backend with telemetry instrumentation.
    pub fn new(inner: S, telemetry: Arc<StorageTelemetry>) -> Self {
        Self { inner, telemetry }
    }

    /// Access the telemetry collector.
    #[must_use]
    pub fn telemetry(&self) -> &Arc<StorageTelemetry> {
        &self.telemetry
    }

    /// Unwrap, returning the inner storage and telemetry.
    pub fn into_parts(self) -> (S, Arc<StorageTelemetry>) {
        (self.inner, self.telemetry)
    }
}

// Note: The trait impl for RecorderStorage is not defined here because
// RecorderStorage is an async trait and the exact impl depends on the
// calling pattern. Callers should use InstrumentedStorage's helper methods:

impl<S> InstrumentedStorage<S> {
    /// Time an append_batch call and record telemetry.
    pub async fn append_batch_instrumented(
        &self,
        _req: AppendRequest,
        result: Result<AppendResponse, RecorderStorageError>,
        start: Instant,
    ) -> Result<AppendResponse, RecorderStorageError> {
        let elapsed_us = start.elapsed().as_nanos() as f64 / 1000.0;
        match &result {
            Ok(resp) => {
                let estimated_bytes = resp.accepted_count as u64 * 256; // heuristic
                self.telemetry.record_append(
                    elapsed_us,
                    resp.accepted_count,
                    estimated_bytes,
                    false,
                );
            }
            Err(e) => {
                self.telemetry.record_error(e.class());
            }
        }
        result
    }

    /// Time a flush call and record telemetry.
    pub fn flush_instrumented(
        &self,
        result: &Result<FlushStats, RecorderStorageError>,
        start: Instant,
    ) {
        let elapsed_us = start.elapsed().as_nanos() as f64 / 1000.0;
        match result {
            Ok(_) => self.telemetry.record_flush(elapsed_us),
            Err(e) => self.telemetry.record_error(e.class()),
        }
    }

    /// Time a checkpoint commit and record telemetry.
    pub fn checkpoint_instrumented(
        &self,
        result: &Result<CheckpointCommitOutcome, RecorderStorageError>,
        start: Instant,
    ) {
        let elapsed_us = start.elapsed().as_nanos() as f64 / 1000.0;
        match result {
            Ok(outcome) => self.telemetry.record_checkpoint(elapsed_us, *outcome),
            Err(e) => self.telemetry.record_error(e.class()),
        }
    }

    /// Update health from a health() call result.
    pub fn health_instrumented(&self, health: &RecorderStorageHealth) {
        self.telemetry.update_health(health.clone());
    }

    /// Update lag from a lag_metrics() call result.
    pub fn lag_instrumented(&self, lag: &RecorderStorageLag) {
        self.telemetry.update_lag(lag.clone());
    }
}

// =============================================================================
// Diagnostic helpers
// =============================================================================

/// Summary of storage health suitable for `wa doctor` or JSON health endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageDiagnosticSummary {
    /// Current tier.
    pub tier: StorageHealthTier,
    /// Human-readable status line.
    pub status: String,
    /// Recommended action (if any).
    pub recommendation: Option<String>,
    /// Error counts.
    pub errors: ErrorCounts,
    /// SLO compliance.
    pub slo_append: SloStatus,
    pub slo_flush: SloStatus,
}

/// Produce a diagnostic summary from a pipeline snapshot.
#[must_use]
pub fn diagnose(snapshot: &StoragePipelineSnapshot) -> StorageDiagnosticSummary {
    let status = match snapshot.health_tier {
        StorageHealthTier::Green => "Storage pipeline operating normally".to_string(),
        StorageHealthTier::Yellow => format!(
            "Elevated queue pressure ({} events queued)",
            snapshot.health.as_ref().map_or(0, |h| h.queue_depth)
        ),
        StorageHealthTier::Red => format!(
            "High queue pressure ({} events queued); consider throttling producers",
            snapshot.health.as_ref().map_or(0, |h| h.queue_depth)
        ),
        StorageHealthTier::Black => {
            let err = snapshot.health.as_ref().and_then(|h| h.last_error.clone());
            format!(
                "CRITICAL: Storage degraded{}",
                err.map_or(String::new(), |e| format!(": {}", e))
            )
        }
    };

    let recommendation = match snapshot.health_tier {
        StorageHealthTier::Green => None,
        StorageHealthTier::Yellow => Some("Monitor queue depth; may resolve on its own".to_string()),
        StorageHealthTier::Red => Some("Reduce ingest rate or increase flush frequency".to_string()),
        StorageHealthTier::Black => {
            Some("Investigate backend errors; may need manual intervention".to_string())
        }
    };

    StorageDiagnosticSummary {
        tier: snapshot.health_tier,
        status,
        recommendation,
        errors: snapshot.errors.clone(),
        slo_append: snapshot.slo_append_p95,
        slo_flush: snapshot.slo_flush_p95,
    }
}

/// Map a storage error to a recommended remediation action.
#[must_use]
pub fn remediation_for_error(class: RecorderStorageErrorClass) -> &'static str {
    match class {
        RecorderStorageErrorClass::Overload => {
            "Back off append rate; increase queue_capacity if sustained"
        }
        RecorderStorageErrorClass::Retryable => "Retry with exponential backoff; check disk I/O",
        RecorderStorageErrorClass::TerminalData => {
            "Fix producer data (invalid batch_id, checkpoint regression)"
        }
        RecorderStorageErrorClass::TerminalConfig => "Fix configuration and restart",
        RecorderStorageErrorClass::Corruption => {
            "Stop writes; run integrity check; consider reindex from backup"
        }
        RecorderStorageErrorClass::DependencyUnavailable => {
            "Check dependent service health; retry after availability"
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder_storage::{RecorderBackendKind, RecorderOffset};

    // -----------------------------------------------------------------------
    // StorageHealthTier
    // -----------------------------------------------------------------------

    #[test]
    fn tier_green_at_zero_depth() {
        let thresholds = [0.5, 0.8, 0.95];
        assert_eq!(
            StorageHealthTier::classify(0.0, false, &thresholds),
            StorageHealthTier::Green
        );
    }

    #[test]
    fn tier_yellow_above_threshold() {
        let thresholds = [0.5, 0.8, 0.95];
        assert_eq!(
            StorageHealthTier::classify(0.5, false, &thresholds),
            StorageHealthTier::Yellow
        );
        assert_eq!(
            StorageHealthTier::classify(0.79, false, &thresholds),
            StorageHealthTier::Yellow
        );
    }

    #[test]
    fn tier_red_above_threshold() {
        let thresholds = [0.5, 0.8, 0.95];
        assert_eq!(
            StorageHealthTier::classify(0.8, false, &thresholds),
            StorageHealthTier::Red
        );
        assert_eq!(
            StorageHealthTier::classify(0.94, false, &thresholds),
            StorageHealthTier::Red
        );
    }

    #[test]
    fn tier_black_above_threshold() {
        let thresholds = [0.5, 0.8, 0.95];
        assert_eq!(
            StorageHealthTier::classify(0.95, false, &thresholds),
            StorageHealthTier::Black
        );
        assert_eq!(
            StorageHealthTier::classify(1.0, false, &thresholds),
            StorageHealthTier::Black
        );
    }

    #[test]
    fn tier_black_when_degraded() {
        let thresholds = [0.5, 0.8, 0.95];
        assert_eq!(
            StorageHealthTier::classify(0.0, true, &thresholds),
            StorageHealthTier::Black
        );
    }

    #[test]
    fn tier_ordering() {
        assert!(StorageHealthTier::Green < StorageHealthTier::Yellow);
        assert!(StorageHealthTier::Yellow < StorageHealthTier::Red);
        assert!(StorageHealthTier::Red < StorageHealthTier::Black);
    }

    // -----------------------------------------------------------------------
    // StorageTelemetry — recording
    // -----------------------------------------------------------------------

    #[test]
    fn record_append_updates_counters_and_histograms() {
        let telem = StorageTelemetry::with_defaults();
        telem.record_append(500.0, 10, 2560, false);
        telem.record_append(800.0, 5, 1280, false);

        assert_eq!(telem.registry.counter_value(COUNTER_EVENTS_APPENDED), 15);
        assert_eq!(telem.registry.counter_value(COUNTER_BATCHES_PROCESSED), 2);
        assert_eq!(telem.registry.counter_value(COUNTER_BYTES_WRITTEN), 3840);
        assert_eq!(telem.registry.counter_value(COUNTER_IDEMPOTENT_REPLAYS), 0);
    }

    #[test]
    fn record_append_idempotent_replay() {
        let telem = StorageTelemetry::with_defaults();
        telem.record_append(100.0, 5, 1280, true);

        assert_eq!(telem.registry.counter_value(COUNTER_IDEMPOTENT_REPLAYS), 1);
    }

    #[test]
    fn record_flush_updates_counters() {
        let telem = StorageTelemetry::with_defaults();
        telem.record_flush(1500.0);
        telem.record_flush(2000.0);

        assert_eq!(telem.registry.counter_value(COUNTER_FLUSHES), 2);
    }

    #[test]
    fn record_checkpoint_advanced() {
        let telem = StorageTelemetry::with_defaults();
        telem.record_checkpoint(300.0, CheckpointCommitOutcome::Advanced);

        assert_eq!(telem.registry.counter_value(COUNTER_CHECKPOINTS), 1);
        assert_eq!(telem.registry.counter_value(COUNTER_CHECKPOINT_ADVANCED), 1);
        assert_eq!(telem.registry.counter_value(COUNTER_CHECKPOINT_NOOP), 0);
    }

    #[test]
    fn record_checkpoint_noop() {
        let telem = StorageTelemetry::with_defaults();
        telem.record_checkpoint(100.0, CheckpointCommitOutcome::NoopAlreadyAdvanced);

        assert_eq!(telem.registry.counter_value(COUNTER_CHECKPOINTS), 1);
        assert_eq!(telem.registry.counter_value(COUNTER_CHECKPOINT_NOOP), 1);
    }

    #[test]
    fn record_errors_by_class() {
        let telem = StorageTelemetry::with_defaults();
        telem.record_error(RecorderStorageErrorClass::Overload);
        telem.record_error(RecorderStorageErrorClass::Overload);
        telem.record_error(RecorderStorageErrorClass::Retryable);
        telem.record_error(RecorderStorageErrorClass::Corruption);

        assert_eq!(telem.registry.counter_value(COUNTER_ERROR_OVERLOAD), 2);
        assert_eq!(telem.registry.counter_value(COUNTER_ERROR_RETRYABLE), 1);
        assert_eq!(telem.registry.counter_value(COUNTER_ERROR_CORRUPTION), 1);
    }

    // -----------------------------------------------------------------------
    // Health tracking
    // -----------------------------------------------------------------------

    fn make_health(depth: usize, capacity: usize, degraded: bool) -> RecorderStorageHealth {
        RecorderStorageHealth {
            backend: RecorderBackendKind::AppendLog,
            degraded,
            queue_depth: depth,
            queue_capacity: capacity,
            latest_offset: None,
            last_error: if degraded {
                Some("test error".to_string())
            } else {
                None
            },
        }
    }

    #[test]
    fn update_health_changes_tier() {
        let telem = StorageTelemetry::with_defaults();
        assert_eq!(telem.current_tier(), StorageHealthTier::Green);

        telem.update_health(make_health(3, 4, false)); // 75% → Yellow
        assert_eq!(telem.current_tier(), StorageHealthTier::Yellow);

        telem.update_health(make_health(4, 4, false)); // 100% → Black
        assert_eq!(telem.current_tier(), StorageHealthTier::Black);
    }

    #[test]
    fn update_lag_preserved() {
        let telem = StorageTelemetry::with_defaults();
        let lag = RecorderStorageLag {
            latest_offset: Some(RecorderOffset {
                segment_id: 0,
                byte_offset: 1000,
                ordinal: 50,
            }),
            consumers: vec![],
        };
        telem.update_lag(lag.clone());

        let snap = telem.snapshot();
        assert!(snap.lag.is_some());
        assert_eq!(snap.lag.unwrap().latest_offset.unwrap().ordinal, 50);
    }

    // -----------------------------------------------------------------------
    // Snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_aggregates_all_metrics() {
        let telem = StorageTelemetry::with_defaults();

        // Record some operations
        telem.record_append(500.0, 10, 2560, false);
        telem.record_append(1500.0, 20, 5120, false);
        telem.record_flush(3000.0);
        telem.record_checkpoint(200.0, CheckpointCommitOutcome::Advanced);
        telem.record_error(RecorderStorageErrorClass::Overload);

        let snap = telem.snapshot();
        assert_eq!(snap.total_events_appended, 30);
        assert_eq!(snap.total_batches, 2);
        assert_eq!(snap.total_flushes, 1);
        assert_eq!(snap.total_checkpoints, 1);
        assert_eq!(snap.errors.overload, 1);
        assert!(snap.append_latency.is_some());
        assert!(snap.flush_latency.is_some());
    }

    #[test]
    fn snapshot_serializes_to_json() {
        let telem = StorageTelemetry::with_defaults();
        telem.record_append(500.0, 10, 2560, false);
        let snap = telem.snapshot();

        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("\"total_events_appended\":10"));
        assert!(json.contains("\"health_tier\":\"green\""));
    }

    // -----------------------------------------------------------------------
    // SLO evaluation
    // -----------------------------------------------------------------------

    #[test]
    fn slo_met_when_within_target() {
        let config = StorageTelemetryConfig {
            slo_append_p95_us: 10_000.0,
            ..Default::default()
        };
        let telem = StorageTelemetry::new(config);

        // Record many low-latency appends
        for _ in 0..100 {
            telem.record_append(500.0, 1, 256, false);
        }

        let snap = telem.snapshot();
        assert_eq!(snap.slo_append_p95, SloStatus::Met);
    }

    #[test]
    fn slo_breached_when_above_target() {
        let config = StorageTelemetryConfig {
            slo_append_p95_us: 100.0, // very tight SLO
            ..Default::default()
        };
        let telem = StorageTelemetry::new(config);

        // Record high-latency appends
        for _ in 0..100 {
            telem.record_append(5000.0, 1, 256, false);
        }

        let snap = telem.snapshot();
        assert_eq!(snap.slo_append_p95, SloStatus::Breached);
    }

    #[test]
    fn slo_unknown_with_no_data() {
        let telem = StorageTelemetry::with_defaults();
        let snap = telem.snapshot();
        assert_eq!(snap.slo_append_p95, SloStatus::Unknown);
        assert_eq!(snap.slo_flush_p95, SloStatus::Unknown);
    }

    // -----------------------------------------------------------------------
    // EWMA rate
    // -----------------------------------------------------------------------

    #[test]
    fn append_rate_updates_with_observations() {
        let telem = StorageTelemetry::with_defaults();
        // First observation initializes the EWMA
        telem.record_append(500.0, 100, 25600, false);
        let rate = telem.append_rate();
        assert!(rate > 0.0, "rate should be positive after observation");
    }

    #[test]
    fn append_rate_zero_initially() {
        let telem = StorageTelemetry::with_defaults();
        assert_eq!(telem.append_rate(), 0.0);
    }

    // -----------------------------------------------------------------------
    // Diagnostics
    // -----------------------------------------------------------------------

    #[test]
    fn diagnose_green_status() {
        let telem = StorageTelemetry::with_defaults();
        let snap = telem.snapshot();
        let diag = diagnose(&snap);

        assert_eq!(diag.tier, StorageHealthTier::Green);
        assert!(diag.status.contains("normally"));
        assert!(diag.recommendation.is_none());
    }

    #[test]
    fn diagnose_yellow_has_recommendation() {
        let telem = StorageTelemetry::with_defaults();
        telem.update_health(make_health(3, 4, false)); // 75% → Yellow
        let snap = telem.snapshot();
        let diag = diagnose(&snap);

        assert_eq!(diag.tier, StorageHealthTier::Yellow);
        assert!(diag.recommendation.is_some());
        assert!(diag.recommendation.unwrap().contains("Monitor"));
    }

    #[test]
    fn diagnose_red_recommends_throttle() {
        let telem = StorageTelemetry::with_defaults();
        telem.update_health(make_health(9, 10, false)); // 90% → Red
        let snap = telem.snapshot();
        let diag = diagnose(&snap);

        assert_eq!(diag.tier, StorageHealthTier::Red);
        assert!(diag.recommendation.unwrap().contains("Reduce ingest"));
    }

    #[test]
    fn diagnose_black_shows_error() {
        let telem = StorageTelemetry::with_defaults();
        telem.update_health(make_health(0, 4, true));
        let snap = telem.snapshot();
        let diag = diagnose(&snap);

        assert_eq!(diag.tier, StorageHealthTier::Black);
        assert!(diag.status.contains("CRITICAL"));
        assert!(diag.status.contains("test error"));
    }

    // -----------------------------------------------------------------------
    // ErrorCounts
    // -----------------------------------------------------------------------

    #[test]
    fn error_counts_total() {
        let ec = ErrorCounts {
            overload: 3,
            retryable: 2,
            terminal_data: 1,
            corruption: 1,
        };
        assert_eq!(ec.total(), 7);
    }

    #[test]
    fn error_counts_default_zero() {
        let ec = ErrorCounts::default();
        assert_eq!(ec.total(), 0);
    }

    // -----------------------------------------------------------------------
    // Remediation mapping
    // -----------------------------------------------------------------------

    #[test]
    fn remediation_covers_all_classes() {
        let classes = [
            RecorderStorageErrorClass::Overload,
            RecorderStorageErrorClass::Retryable,
            RecorderStorageErrorClass::TerminalData,
            RecorderStorageErrorClass::TerminalConfig,
            RecorderStorageErrorClass::Corruption,
            RecorderStorageErrorClass::DependencyUnavailable,
        ];
        for class in &classes {
            let msg = remediation_for_error(*class);
            assert!(!msg.is_empty(), "remediation should be non-empty for {:?}", class);
        }
    }

    // -----------------------------------------------------------------------
    // Config defaults
    // -----------------------------------------------------------------------

    #[test]
    fn default_config_has_sane_values() {
        let cfg = StorageTelemetryConfig::default();
        assert_eq!(cfg.histogram_max_samples, 1024);
        assert!(cfg.tier_thresholds[0] < cfg.tier_thresholds[1]);
        assert!(cfg.tier_thresholds[1] < cfg.tier_thresholds[2]);
        assert!(cfg.slo_append_p95_us > 0.0);
        assert!(cfg.slo_flush_p95_us > 0.0);
        assert!(cfg.rate_ewma_half_life_ms > 0.0);
    }

    #[test]
    fn config_serialization_roundtrip() {
        let cfg = StorageTelemetryConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: StorageTelemetryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.histogram_max_samples, cfg.histogram_max_samples);
    }

    // -----------------------------------------------------------------------
    // InstrumentedStorage helpers
    // -----------------------------------------------------------------------

    #[test]
    fn instrumented_health_updates_telemetry() {
        let telem = Arc::new(StorageTelemetry::with_defaults());
        let instrumented = InstrumentedStorage::new((), telem.clone());

        let health = make_health(2, 4, false); // 50% → Yellow
        instrumented.health_instrumented(&health);

        assert_eq!(telem.current_tier(), StorageHealthTier::Yellow);
    }

    #[test]
    fn instrumented_lag_updates_telemetry() {
        let telem = Arc::new(StorageTelemetry::with_defaults());
        let instrumented = InstrumentedStorage::new((), telem.clone());

        let lag = RecorderStorageLag {
            latest_offset: Some(RecorderOffset {
                segment_id: 0,
                byte_offset: 5000,
                ordinal: 100,
            }),
            consumers: vec![],
        };
        instrumented.lag_instrumented(&lag);

        let snap = telem.snapshot();
        assert!(snap.lag.is_some());
    }

    #[test]
    fn instrumented_flush_records_latency() {
        let telem = Arc::new(StorageTelemetry::with_defaults());
        let instrumented = InstrumentedStorage::new((), telem.clone());

        let start = Instant::now();
        let result: Result<FlushStats, RecorderStorageError> = Ok(FlushStats {
            backend: RecorderBackendKind::AppendLog,
            flushed_at_ms: 1000,
            latest_offset: None,
        });
        instrumented.flush_instrumented(&result, start);

        assert_eq!(telem.registry().counter_value(COUNTER_FLUSHES), 1);
    }

    #[test]
    fn instrumented_checkpoint_records_outcome() {
        let telem = Arc::new(StorageTelemetry::with_defaults());
        let instrumented = InstrumentedStorage::new((), telem.clone());

        let start = Instant::now();
        let result: Result<CheckpointCommitOutcome, RecorderStorageError> =
            Ok(CheckpointCommitOutcome::Advanced);
        instrumented.checkpoint_instrumented(&result, start);

        assert_eq!(telem.registry().counter_value(COUNTER_CHECKPOINTS), 1);
        assert_eq!(telem.registry().counter_value(COUNTER_CHECKPOINT_ADVANCED), 1);
    }

    #[test]
    fn instrumented_error_records_class() {
        let telem = Arc::new(StorageTelemetry::with_defaults());
        let instrumented = InstrumentedStorage::new((), telem.clone());

        let start = Instant::now();
        let result: Result<FlushStats, RecorderStorageError> =
            Err(RecorderStorageError::QueueFull { capacity: 4 });
        instrumented.flush_instrumented(&result, start);

        assert_eq!(telem.registry().counter_value(COUNTER_ERROR_OVERLOAD), 1);
    }

    #[test]
    fn into_parts_returns_inner_and_telemetry() {
        let telem = Arc::new(StorageTelemetry::with_defaults());
        let instrumented = InstrumentedStorage::new(42u32, telem.clone());

        let (inner, t) = instrumented.into_parts();
        assert_eq!(inner, 42);
        assert!(Arc::ptr_eq(&t, &telem));
    }
}
