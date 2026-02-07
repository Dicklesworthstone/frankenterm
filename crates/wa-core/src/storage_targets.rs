//! Storage and indexing performance targets, metrics, and budgets.
//!
//! Defines measurable targets for the storage layer that can be enforced
//! by benchmarks, E2E tests, and runtime health checks.
//!
//! # Scale targets
//!
//! | Dimension         | Target     | Notes                                    |
//! |-------------------|------------|------------------------------------------|
//! | Observed panes    | 50+        | Concurrent capture without starvation    |
//! | Transcript size   | 2 GB+      | Per-workspace, accumulated over sessions |
//! | Ingest rate       | 1 MB/s     | Sustained across all panes combined      |
//! | Segment count     | 500K+      | Without query degradation                |
//! | DB file size      | 4 GB       | Before retention cleanup triggers        |
//!
//! # Latency budgets
//!
//! | Operation              | Budget (p95) | Measured at              |
//! |------------------------|--------------|--------------------------|
//! | Append single segment  | 2 ms         | 100K existing segments   |
//! | Batch append (128)     | 50 ms        | Amortized via writer     |
//! | FTS query (common)     | 15 ms        | 100K segments, warm      |
//! | FTS query (complex)    | 50 ms        | Phrase + filter           |
//! | Pane upsert            | 1 ms         | Metadata write            |
//! | Indexing lag ceiling    | 500 ms       | Time from capture to FTS  |
//! | Checkpoint (passive)   | 100 ms       | Non-blocking WAL compact  |
//!
//! # Health metrics
//!
//! | Metric                  | Green    | Yellow   | Red       |
//! |-------------------------|----------|----------|-----------|
//! | Writer queue depth      | < 50%   | 50–80%  | > 80%    |
//! | WAL size (frames)       | < 5K    | 5K–10K  | > 10K    |
//! | FTS consistency ratio   | 100%    | > 95%   | < 95%    |
//! | Indexing lag (ms)        | < 200   | 200–500 | > 500    |

use std::time::Duration;

use serde::{Deserialize, Serialize};

// ───────────────────────────────────────────────────────────────────────────
// Scale targets
// ───────────────────────────────────────────────────────────────────────────

/// Scale dimensions the storage layer must handle without degradation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleTargets {
    /// Minimum panes observed concurrently without starvation.
    pub min_concurrent_panes: usize,
    /// Minimum accumulated transcript bytes per workspace.
    pub min_transcript_bytes: u64,
    /// Minimum sustained ingest rate (bytes/sec across all panes).
    pub min_ingest_bytes_per_sec: u64,
    /// Minimum segment count before query degradation is acceptable.
    pub min_segments_before_degradation: u64,
    /// Maximum DB file size before retention cleanup should trigger.
    pub max_db_size_bytes: u64,
}

impl Default for ScaleTargets {
    fn default() -> Self {
        Self {
            min_concurrent_panes: 50,
            min_transcript_bytes: 2 * 1024 * 1024 * 1024, // 2 GB
            min_ingest_bytes_per_sec: 1_000_000,          // 1 MB/s
            min_segments_before_degradation: 500_000,
            max_db_size_bytes: 4 * 1024 * 1024 * 1024, // 4 GB
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Latency budgets
// ───────────────────────────────────────────────────────────────────────────

/// p95 latency budgets for storage operations.
///
/// These are measured against benchmark data in
/// `crates/wa-core/benches/storage_regression.rs` and enforced
/// as performance gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyBudgets {
    /// Single segment append (p95), measured at 100K existing segments.
    pub append_segment_p95: Duration,
    /// Batch append of `batch_size` segments (p95), amortized.
    pub batch_append_p95: Duration,
    /// Batch size used for `batch_append_p95`.
    pub batch_size: usize,
    /// FTS query for common patterns (p95), 100K segments warm cache.
    pub fts_query_common_p95: Duration,
    /// FTS query for complex patterns (p95), phrase + filter.
    pub fts_query_complex_p95: Duration,
    /// Pane metadata upsert (p95).
    pub pane_upsert_p95: Duration,
    /// Maximum time from capture to FTS-searchable.
    pub indexing_lag_ceiling: Duration,
    /// Passive WAL checkpoint (non-blocking).
    pub checkpoint_passive_p95: Duration,
}

impl Default for LatencyBudgets {
    fn default() -> Self {
        Self {
            append_segment_p95: Duration::from_millis(2),
            batch_append_p95: Duration::from_millis(50),
            batch_size: 128,
            fts_query_common_p95: Duration::from_millis(15),
            fts_query_complex_p95: Duration::from_millis(50),
            pane_upsert_p95: Duration::from_millis(1),
            indexing_lag_ceiling: Duration::from_millis(500),
            checkpoint_passive_p95: Duration::from_millis(100),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Throughput budgets
// ───────────────────────────────────────────────────────────────────────────

/// Sustained throughput targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThroughputBudgets {
    /// Minimum segments per second (sustained batch append).
    pub min_segments_per_sec: u64,
    /// Writer batch capacity (commands processed per transaction).
    pub writer_batch_cap: usize,
    /// FTS sync batch size (segments per indexing batch).
    pub fts_sync_batch_size: usize,
    /// FTS sync max batch bytes.
    pub fts_sync_max_batch_bytes: usize,
}

impl Default for ThroughputBudgets {
    fn default() -> Self {
        Self {
            min_segments_per_sec: 500,
            writer_batch_cap: 128,
            fts_sync_batch_size: 100,
            fts_sync_max_batch_bytes: 1_048_576, // 1 MiB
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Health thresholds
// ───────────────────────────────────────────────────────────────────────────

/// Health tier for a single metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthTier {
    /// Within normal operating range.
    Green,
    /// Approaching limits — investigate.
    Yellow,
    /// At or exceeding limits — immediate action needed.
    Red,
}

impl std::fmt::Display for HealthTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "green"),
            Self::Yellow => write!(f, "yellow"),
            Self::Red => write!(f, "red"),
        }
    }
}

/// Thresholds for writer queue depth health.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WriterQueueThresholds {
    /// Ratio at which yellow is triggered (0.0–1.0).
    pub yellow_ratio: f64,
    /// Ratio at which red is triggered (0.0–1.0).
    pub red_ratio: f64,
}

impl Default for WriterQueueThresholds {
    fn default() -> Self {
        Self {
            yellow_ratio: 0.50,
            red_ratio: 0.80,
        }
    }
}

impl WriterQueueThresholds {
    /// Classify writer queue depth health.
    #[must_use]
    pub fn classify(&self, depth: usize, capacity: usize) -> HealthTier {
        if capacity == 0 {
            return HealthTier::Red;
        }
        let ratio = depth as f64 / capacity as f64;
        if ratio >= self.red_ratio {
            HealthTier::Red
        } else if ratio >= self.yellow_ratio {
            HealthTier::Yellow
        } else {
            HealthTier::Green
        }
    }
}

/// Thresholds for WAL size health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalThresholds {
    /// Frame count at which yellow triggers.
    pub yellow_frames: u64,
    /// Frame count at which red triggers (checkpoint urgently).
    pub red_frames: u64,
}

impl Default for WalThresholds {
    fn default() -> Self {
        Self {
            yellow_frames: 5_000,
            red_frames: 10_000,
        }
    }
}

impl WalThresholds {
    /// Classify WAL size health.
    #[must_use]
    pub fn classify(&self, frames: u64) -> HealthTier {
        if frames >= self.red_frames {
            HealthTier::Red
        } else if frames >= self.yellow_frames {
            HealthTier::Yellow
        } else {
            HealthTier::Green
        }
    }
}

/// Thresholds for FTS index consistency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FtsConsistencyThresholds {
    /// Ratio below which yellow triggers.
    pub yellow_ratio: f64,
    /// Ratio below which red triggers.
    pub red_ratio: f64,
}

impl Default for FtsConsistencyThresholds {
    fn default() -> Self {
        Self {
            yellow_ratio: 0.95,
            red_ratio: 0.90,
        }
    }
}

impl FtsConsistencyThresholds {
    /// Classify FTS consistency health.
    ///
    /// `ratio` is `fts_rows / segment_count` (1.0 = perfectly consistent).
    #[must_use]
    pub fn classify(&self, ratio: f64) -> HealthTier {
        if ratio < self.red_ratio {
            HealthTier::Red
        } else if ratio < self.yellow_ratio {
            HealthTier::Yellow
        } else {
            HealthTier::Green
        }
    }
}

/// Thresholds for indexing lag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingLagThresholds {
    /// Lag at which yellow triggers.
    pub yellow: Duration,
    /// Lag at which red triggers.
    pub red: Duration,
}

impl Default for IndexingLagThresholds {
    fn default() -> Self {
        Self {
            yellow: Duration::from_millis(200),
            red: Duration::from_millis(500),
        }
    }
}

impl IndexingLagThresholds {
    /// Classify indexing lag health.
    #[must_use]
    pub fn classify(&self, lag: Duration) -> HealthTier {
        if lag >= self.red {
            HealthTier::Red
        } else if lag >= self.yellow {
            HealthTier::Yellow
        } else {
            HealthTier::Green
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Combined health thresholds
// ───────────────────────────────────────────────────────────────────────────

/// All storage health thresholds in one struct for configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageHealthThresholds {
    pub writer_queue: WriterQueueThresholds,
    pub wal: WalThresholds,
    pub fts_consistency: FtsConsistencyThresholds,
    pub indexing_lag: IndexingLagThresholds,
}

impl Default for StorageHealthThresholds {
    fn default() -> Self {
        Self {
            writer_queue: WriterQueueThresholds::default(),
            wal: WalThresholds::default(),
            fts_consistency: FtsConsistencyThresholds::default(),
            indexing_lag: IndexingLagThresholds::default(),
        }
    }
}

/// Point-in-time storage health assessment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageHealthSnapshot {
    /// Writer queue health.
    pub writer_queue: HealthTier,
    /// WAL size health.
    pub wal: HealthTier,
    /// FTS index consistency health.
    pub fts_consistency: HealthTier,
    /// Indexing lag health.
    pub indexing_lag: HealthTier,
    /// Worst tier across all metrics.
    pub overall: HealthTier,
}

impl StorageHealthSnapshot {
    /// Assess storage health from raw metrics.
    #[must_use]
    pub fn assess(metrics: &StorageMetrics, thresholds: &StorageHealthThresholds) -> Self {
        let writer_queue = thresholds
            .writer_queue
            .classify(metrics.writer_queue_depth, metrics.writer_queue_capacity);
        let wal = thresholds.wal.classify(metrics.wal_frames);
        let fts_consistency = thresholds
            .fts_consistency
            .classify(metrics.fts_consistency_ratio);
        let indexing_lag = thresholds.indexing_lag.classify(metrics.indexing_lag);

        let overall = worst_tier(&[writer_queue, wal, fts_consistency, indexing_lag]);

        Self {
            writer_queue,
            wal,
            fts_consistency,
            indexing_lag,
            overall,
        }
    }
}

/// Raw storage metrics used for health assessment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageMetrics {
    /// Current writer queue depth.
    pub writer_queue_depth: usize,
    /// Writer queue capacity.
    pub writer_queue_capacity: usize,
    /// Current WAL frame count.
    pub wal_frames: u64,
    /// FTS row count / segment count ratio (1.0 = consistent).
    pub fts_consistency_ratio: f64,
    /// Time from most recent capture to FTS availability.
    pub indexing_lag: Duration,
}

/// Return the worst (most severe) tier from a slice.
fn worst_tier(tiers: &[HealthTier]) -> HealthTier {
    if tiers.contains(&HealthTier::Red) {
        HealthTier::Red
    } else if tiers.contains(&HealthTier::Yellow) {
        HealthTier::Yellow
    } else {
        HealthTier::Green
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Combined performance profile
// ───────────────────────────────────────────────────────────────────────────

/// Complete storage performance profile: scale + latency + throughput + health.
///
/// Use [`StoragePerfProfile::default()`] for the standard targets.
/// Custom profiles can be used for resource-constrained or high-performance
/// environments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoragePerfProfile {
    pub scale: ScaleTargets,
    pub latency: LatencyBudgets,
    pub throughput: ThroughputBudgets,
    pub health: StorageHealthThresholds,
}

impl Default for StoragePerfProfile {
    fn default() -> Self {
        Self {
            scale: ScaleTargets::default(),
            latency: LatencyBudgets::default(),
            throughput: ThroughputBudgets::default(),
            health: StorageHealthThresholds::default(),
        }
    }
}

impl StoragePerfProfile {
    /// Profile for resource-constrained environments (e.g., CI, small VMs).
    #[must_use]
    pub fn constrained() -> Self {
        Self {
            scale: ScaleTargets {
                min_concurrent_panes: 10,
                min_transcript_bytes: 256 * 1024 * 1024, // 256 MB
                min_ingest_bytes_per_sec: 100_000,       // 100 KB/s
                min_segments_before_degradation: 50_000,
                max_db_size_bytes: 512 * 1024 * 1024, // 512 MB
            },
            latency: LatencyBudgets {
                append_segment_p95: Duration::from_millis(5),
                batch_append_p95: Duration::from_millis(100),
                batch_size: 64,
                fts_query_common_p95: Duration::from_millis(30),
                fts_query_complex_p95: Duration::from_millis(100),
                pane_upsert_p95: Duration::from_millis(2),
                indexing_lag_ceiling: Duration::from_secs(1),
                checkpoint_passive_p95: Duration::from_millis(200),
            },
            throughput: ThroughputBudgets {
                min_segments_per_sec: 200,
                writer_batch_cap: 64,
                fts_sync_batch_size: 50,
                fts_sync_max_batch_bytes: 512 * 1024,
            },
            health: StorageHealthThresholds::default(),
        }
    }

    /// Profile for high-performance environments (fast SSD, many cores).
    #[must_use]
    pub fn high_performance() -> Self {
        Self {
            scale: ScaleTargets {
                min_concurrent_panes: 200,
                min_transcript_bytes: 8 * 1024 * 1024 * 1024, // 8 GB
                min_ingest_bytes_per_sec: 5_000_000,          // 5 MB/s
                min_segments_before_degradation: 2_000_000,
                max_db_size_bytes: 16 * 1024 * 1024 * 1024, // 16 GB
            },
            latency: LatencyBudgets {
                append_segment_p95: Duration::from_micros(500),
                batch_append_p95: Duration::from_millis(20),
                batch_size: 256,
                fts_query_common_p95: Duration::from_millis(5),
                fts_query_complex_p95: Duration::from_millis(20),
                pane_upsert_p95: Duration::from_micros(500),
                indexing_lag_ceiling: Duration::from_millis(200),
                checkpoint_passive_p95: Duration::from_millis(50),
            },
            throughput: ThroughputBudgets {
                min_segments_per_sec: 2000,
                writer_batch_cap: 256,
                fts_sync_batch_size: 200,
                fts_sync_max_batch_bytes: 4 * 1024 * 1024,
            },
            health: StorageHealthThresholds::default(),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // --- Scale targets ---

    #[test]
    fn default_scale_targets() {
        let t = ScaleTargets::default();
        assert_eq!(t.min_concurrent_panes, 50);
        assert_eq!(t.min_transcript_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(t.min_ingest_bytes_per_sec, 1_000_000);
        assert_eq!(t.min_segments_before_degradation, 500_000);
    }

    #[test]
    fn scale_targets_roundtrip_serde() {
        let t = ScaleTargets::default();
        let json = serde_json::to_string(&t).unwrap();
        let parsed: ScaleTargets = serde_json::from_str(&json).unwrap();
        assert_eq!(t, parsed);
    }

    // --- Latency budgets ---

    #[test]
    fn default_latency_budgets() {
        let b = LatencyBudgets::default();
        assert_eq!(b.append_segment_p95, Duration::from_millis(2));
        assert_eq!(b.fts_query_common_p95, Duration::from_millis(15));
        assert_eq!(b.fts_query_complex_p95, Duration::from_millis(50));
        assert_eq!(b.pane_upsert_p95, Duration::from_millis(1));
        assert_eq!(b.indexing_lag_ceiling, Duration::from_millis(500));
    }

    #[test]
    fn constrained_latency_is_more_relaxed() {
        let def = LatencyBudgets::default();
        let con = StoragePerfProfile::constrained().latency;
        assert!(con.append_segment_p95 > def.append_segment_p95);
        assert!(con.fts_query_common_p95 > def.fts_query_common_p95);
        assert!(con.indexing_lag_ceiling > def.indexing_lag_ceiling);
    }

    #[test]
    fn high_perf_latency_is_tighter() {
        let def = LatencyBudgets::default();
        let hp = StoragePerfProfile::high_performance().latency;
        assert!(hp.append_segment_p95 < def.append_segment_p95);
        assert!(hp.fts_query_common_p95 < def.fts_query_common_p95);
        assert!(hp.indexing_lag_ceiling < def.indexing_lag_ceiling);
    }

    // --- Throughput budgets ---

    #[test]
    fn default_throughput_budgets() {
        let t = ThroughputBudgets::default();
        assert_eq!(t.min_segments_per_sec, 500);
        assert_eq!(t.writer_batch_cap, 128);
        assert_eq!(t.fts_sync_batch_size, 100);
    }

    // --- Writer queue health ---

    #[test]
    fn writer_queue_green() {
        let th = WriterQueueThresholds::default();
        assert_eq!(th.classify(100, 1024), HealthTier::Green);
        assert_eq!(th.classify(0, 1024), HealthTier::Green);
    }

    #[test]
    fn writer_queue_yellow() {
        let th = WriterQueueThresholds::default();
        assert_eq!(th.classify(512, 1024), HealthTier::Yellow);
        assert_eq!(th.classify(700, 1024), HealthTier::Yellow);
    }

    #[test]
    fn writer_queue_red() {
        let th = WriterQueueThresholds::default();
        assert_eq!(th.classify(820, 1024), HealthTier::Red);
        assert_eq!(th.classify(1024, 1024), HealthTier::Red);
    }

    #[test]
    fn writer_queue_zero_capacity_is_red() {
        let th = WriterQueueThresholds::default();
        assert_eq!(th.classify(0, 0), HealthTier::Red);
    }

    // --- WAL health ---

    #[test]
    fn wal_green() {
        let th = WalThresholds::default();
        assert_eq!(th.classify(0), HealthTier::Green);
        assert_eq!(th.classify(4999), HealthTier::Green);
    }

    #[test]
    fn wal_yellow() {
        let th = WalThresholds::default();
        assert_eq!(th.classify(5000), HealthTier::Yellow);
        assert_eq!(th.classify(9999), HealthTier::Yellow);
    }

    #[test]
    fn wal_red() {
        let th = WalThresholds::default();
        assert_eq!(th.classify(10000), HealthTier::Red);
        assert_eq!(th.classify(50000), HealthTier::Red);
    }

    // --- FTS consistency health ---

    #[test]
    fn fts_consistency_green() {
        let th = FtsConsistencyThresholds::default();
        assert_eq!(th.classify(1.0), HealthTier::Green);
        assert_eq!(th.classify(0.96), HealthTier::Green);
    }

    #[test]
    fn fts_consistency_yellow() {
        let th = FtsConsistencyThresholds::default();
        assert_eq!(th.classify(0.94), HealthTier::Yellow);
        assert_eq!(th.classify(0.91), HealthTier::Yellow);
    }

    #[test]
    fn fts_consistency_red() {
        let th = FtsConsistencyThresholds::default();
        assert_eq!(th.classify(0.89), HealthTier::Red);
        assert_eq!(th.classify(0.0), HealthTier::Red);
    }

    // --- Indexing lag health ---

    #[test]
    fn indexing_lag_green() {
        let th = IndexingLagThresholds::default();
        assert_eq!(th.classify(Duration::from_millis(0)), HealthTier::Green);
        assert_eq!(th.classify(Duration::from_millis(199)), HealthTier::Green);
    }

    #[test]
    fn indexing_lag_yellow() {
        let th = IndexingLagThresholds::default();
        assert_eq!(th.classify(Duration::from_millis(200)), HealthTier::Yellow);
        assert_eq!(th.classify(Duration::from_millis(499)), HealthTier::Yellow);
    }

    #[test]
    fn indexing_lag_red() {
        let th = IndexingLagThresholds::default();
        assert_eq!(th.classify(Duration::from_millis(500)), HealthTier::Red);
        assert_eq!(th.classify(Duration::from_secs(5)), HealthTier::Red);
    }

    // --- Storage health snapshot ---

    #[test]
    fn healthy_metrics_produce_green() {
        let metrics = StorageMetrics {
            writer_queue_depth: 50,
            writer_queue_capacity: 1024,
            wal_frames: 1000,
            fts_consistency_ratio: 1.0,
            indexing_lag: Duration::from_millis(50),
        };
        let snap = StorageHealthSnapshot::assess(&metrics, &StorageHealthThresholds::default());
        assert_eq!(snap.overall, HealthTier::Green);
        assert_eq!(snap.writer_queue, HealthTier::Green);
        assert_eq!(snap.wal, HealthTier::Green);
        assert_eq!(snap.fts_consistency, HealthTier::Green);
        assert_eq!(snap.indexing_lag, HealthTier::Green);
    }

    #[test]
    fn one_yellow_metric_makes_overall_yellow() {
        let metrics = StorageMetrics {
            writer_queue_depth: 600,
            writer_queue_capacity: 1024,
            wal_frames: 100,
            fts_consistency_ratio: 1.0,
            indexing_lag: Duration::from_millis(10),
        };
        let snap = StorageHealthSnapshot::assess(&metrics, &StorageHealthThresholds::default());
        assert_eq!(snap.writer_queue, HealthTier::Yellow);
        assert_eq!(snap.overall, HealthTier::Yellow);
    }

    #[test]
    fn one_red_metric_makes_overall_red() {
        let metrics = StorageMetrics {
            writer_queue_depth: 50,
            writer_queue_capacity: 1024,
            wal_frames: 15000,
            fts_consistency_ratio: 1.0,
            indexing_lag: Duration::from_millis(10),
        };
        let snap = StorageHealthSnapshot::assess(&metrics, &StorageHealthThresholds::default());
        assert_eq!(snap.wal, HealthTier::Red);
        assert_eq!(snap.overall, HealthTier::Red);
    }

    #[test]
    fn multiple_degraded_metrics() {
        let metrics = StorageMetrics {
            writer_queue_depth: 900,
            writer_queue_capacity: 1024,
            wal_frames: 8000,
            fts_consistency_ratio: 0.92,
            indexing_lag: Duration::from_millis(300),
        };
        let snap = StorageHealthSnapshot::assess(&metrics, &StorageHealthThresholds::default());
        assert_eq!(snap.writer_queue, HealthTier::Red);
        assert_eq!(snap.wal, HealthTier::Yellow);
        assert_eq!(snap.fts_consistency, HealthTier::Yellow);
        assert_eq!(snap.indexing_lag, HealthTier::Yellow);
        assert_eq!(snap.overall, HealthTier::Red);
    }

    // --- Health tier ---

    #[test]
    fn health_tier_display() {
        assert_eq!(HealthTier::Green.to_string(), "green");
        assert_eq!(HealthTier::Yellow.to_string(), "yellow");
        assert_eq!(HealthTier::Red.to_string(), "red");
    }

    #[test]
    fn health_tier_roundtrip_serde() {
        for tier in [HealthTier::Green, HealthTier::Yellow, HealthTier::Red] {
            let json = serde_json::to_string(&tier).unwrap();
            let parsed: HealthTier = serde_json::from_str(&json).unwrap();
            assert_eq!(tier, parsed);
        }
    }

    // --- Performance profiles ---

    #[test]
    fn default_profile_is_self_consistent() {
        let p = StoragePerfProfile::default();
        assert!(p.latency.batch_size <= p.throughput.writer_batch_cap * 2);
        assert!(p.latency.indexing_lag_ceiling >= p.health.indexing_lag.red);
    }

    #[test]
    fn constrained_profile_has_lower_scale() {
        let def = StoragePerfProfile::default();
        let con = StoragePerfProfile::constrained();
        assert!(con.scale.min_concurrent_panes < def.scale.min_concurrent_panes);
        assert!(con.scale.min_transcript_bytes < def.scale.min_transcript_bytes);
        assert!(con.throughput.min_segments_per_sec < def.throughput.min_segments_per_sec);
    }

    #[test]
    fn high_perf_profile_has_higher_scale() {
        let def = StoragePerfProfile::default();
        let hp = StoragePerfProfile::high_performance();
        assert!(hp.scale.min_concurrent_panes > def.scale.min_concurrent_panes);
        assert!(hp.scale.min_transcript_bytes > def.scale.min_transcript_bytes);
        assert!(hp.throughput.min_segments_per_sec > def.throughput.min_segments_per_sec);
    }

    #[test]
    fn profile_roundtrip_serde() {
        let p = StoragePerfProfile::default();
        let json = serde_json::to_string(&p).unwrap();
        let _parsed: StoragePerfProfile = serde_json::from_str(&json).unwrap();
    }

    // --- worst_tier helper ---

    #[test]
    fn worst_tier_all_green() {
        assert_eq!(
            worst_tier(&[HealthTier::Green, HealthTier::Green]),
            HealthTier::Green
        );
    }

    #[test]
    fn worst_tier_mixed() {
        assert_eq!(
            worst_tier(&[HealthTier::Green, HealthTier::Yellow]),
            HealthTier::Yellow
        );
        assert_eq!(
            worst_tier(&[HealthTier::Yellow, HealthTier::Red]),
            HealthTier::Red
        );
    }

    #[test]
    fn worst_tier_empty_is_green() {
        assert_eq!(worst_tier(&[]), HealthTier::Green);
    }
}
