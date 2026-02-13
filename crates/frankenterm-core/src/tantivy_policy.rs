//! Tantivy commit/merge policy for sustained recorder ingest and query latency.
//!
//! This module defines configuration types, adaptive merge strategies, and
//! segment health monitoring for the Tantivy lexical index used by the flight
//! recorder. The policies are tunable at runtime without code changes.
//!
//! Key design principles:
//! - Merge policy adapts to load regime (idle/steady/burst/overload)
//! - Commit frequency balances freshness vs write amplification
//! - Segment count stays bounded to keep query latency predictable
//! - All thresholds are configurable with safe defaults
//!
//! Bead: wa-oegrb.4.3

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Load regime classification
// ---------------------------------------------------------------------------

/// Observed load regime for adaptive policy selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadRegime {
    /// < 10 events/sec, infrequent commits acceptable
    Idle,
    /// 10–500 events/sec, normal terminal workload
    Steady,
    /// 500–5000 events/sec, swarm burst or replay
    Burst,
    /// > 5000 events/sec, backpressure should engage
    Overload,
}

impl LoadRegime {
    /// Classify an observed event rate (events/sec) into a load regime.
    #[must_use]
    pub fn classify(events_per_sec: f64) -> Self {
        if events_per_sec < 10.0 {
            Self::Idle
        } else if events_per_sec < 500.0 {
            Self::Steady
        } else if events_per_sec < 5000.0 {
            Self::Burst
        } else {
            Self::Overload
        }
    }
}

// ---------------------------------------------------------------------------
// Merge strategy
// ---------------------------------------------------------------------------

/// Segment merge strategy selector.
///
/// Controls how Tantivy segments are merged to balance write amplification
/// against query-time segment fan-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// Log-structured: merge small segments into larger tiers.
    /// Best for sustained ingest with periodic query bursts.
    LogMerge,
    /// Aggressive: merge eagerly to minimize segment count.
    /// Best for query-heavy workloads with lower ingest rate.
    Aggressive,
    /// Conservative: merge only when segment count exceeds threshold.
    /// Best for write-heavy workloads where query latency is less critical.
    Conservative,
    /// No automatic merging. Manual control only.
    /// Use during bulk reindex or when external tooling manages merges.
    NoMerge,
}

// ---------------------------------------------------------------------------
// Commit policy
// ---------------------------------------------------------------------------

/// Commit trigger policy for the Tantivy index writer.
///
/// A commit makes recently indexed documents visible to readers. Commits
/// are expensive (fsync + segment finalization), so the policy must balance
/// document freshness against write amplification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitPolicy {
    /// Maximum documents to buffer before forcing a commit.
    /// Lower values = fresher results, higher write amplification.
    pub max_docs_before_commit: u64,

    /// Maximum bytes of heap to buffer before forcing a commit.
    /// Prevents unbounded memory growth during burst ingest.
    pub max_bytes_before_commit: u64,

    /// Maximum wall-clock interval between commits.
    /// Ensures documents become searchable within this window.
    pub max_interval: Duration,

    /// Minimum interval between commits to prevent thrashing.
    /// Protects against commit storms during steady-state trickle.
    pub min_interval: Duration,
}

impl Default for CommitPolicy {
    fn default() -> Self {
        Self {
            max_docs_before_commit: 10_000,
            max_bytes_before_commit: 64 * 1024 * 1024, // 64 MiB
            max_interval: Duration::from_secs(5),
            min_interval: Duration::from_millis(500),
        }
    }
}

impl CommitPolicy {
    /// Returns a policy tuned for the given load regime.
    #[must_use]
    pub fn for_regime(regime: LoadRegime) -> Self {
        match regime {
            LoadRegime::Idle => Self {
                max_docs_before_commit: 1_000,
                max_bytes_before_commit: 8 * 1024 * 1024,
                max_interval: Duration::from_secs(30),
                min_interval: Duration::from_secs(2),
            },
            LoadRegime::Steady => Self::default(),
            LoadRegime::Burst => Self {
                max_docs_before_commit: 50_000,
                max_bytes_before_commit: 128 * 1024 * 1024,
                max_interval: Duration::from_secs(10),
                min_interval: Duration::from_secs(1),
            },
            LoadRegime::Overload => Self {
                max_docs_before_commit: 100_000,
                max_bytes_before_commit: 256 * 1024 * 1024,
                max_interval: Duration::from_secs(30),
                min_interval: Duration::from_secs(5),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Merge policy configuration
// ---------------------------------------------------------------------------

/// Configuration for the segment merge policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePolicyConfig {
    /// Active merge strategy.
    pub strategy: MergeStrategy,

    /// Maximum number of segments before triggering a forced merge.
    /// When segment count exceeds this, merge is triggered regardless of strategy.
    pub max_segment_count: u32,

    /// Target segment count after a forced merge operation.
    /// The merge operation will try to reduce segments to this count.
    pub target_segment_count: u32,

    /// Minimum segment size (bytes) to consider for merging.
    /// Segments below this size are always eligible for merge.
    pub min_segment_size_bytes: u64,

    /// Maximum segment size (bytes) after which no further merging.
    /// Prevents creating extremely large segments that are expensive to merge.
    pub max_segment_size_bytes: u64,

    /// Maximum number of segments to merge in a single operation.
    /// Bounds the I/O cost of a single merge.
    pub max_merge_factor: u32,

    /// Budget for concurrent merge operations.
    /// Higher values allow more parallelism but consume more I/O bandwidth.
    pub max_concurrent_merges: u32,
}

impl Default for MergePolicyConfig {
    fn default() -> Self {
        Self {
            strategy: MergeStrategy::LogMerge,
            max_segment_count: 30,
            target_segment_count: 8,
            min_segment_size_bytes: 256 * 1024, // 256 KiB
            max_segment_size_bytes: 2 * 1024 * 1024 * 1024, // 2 GiB
            max_merge_factor: 10,
            max_concurrent_merges: 2,
        }
    }
}

impl MergePolicyConfig {
    /// Returns a merge policy tuned for the given load regime.
    #[must_use]
    pub fn for_regime(regime: LoadRegime) -> Self {
        match regime {
            LoadRegime::Idle => Self {
                strategy: MergeStrategy::Aggressive,
                max_segment_count: 15,
                target_segment_count: 4,
                max_concurrent_merges: 1,
                ..Self::default()
            },
            LoadRegime::Steady => Self::default(),
            LoadRegime::Burst => Self {
                strategy: MergeStrategy::Conservative,
                max_segment_count: 50,
                target_segment_count: 12,
                max_merge_factor: 15,
                max_concurrent_merges: 3,
                ..Self::default()
            },
            LoadRegime::Overload => Self {
                strategy: MergeStrategy::NoMerge,
                max_segment_count: 100,
                target_segment_count: 20,
                max_concurrent_merges: 1,
                ..Self::default()
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level index tuning configuration
// ---------------------------------------------------------------------------

/// Complete index tuning configuration combining commit and merge policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexTuningConfig {
    /// Commit trigger policy.
    pub commit: CommitPolicy,

    /// Segment merge policy.
    pub merge: MergePolicyConfig,

    /// Writer heap budget in bytes.
    /// Controls how much memory the Tantivy IndexWriter uses for buffering.
    pub writer_heap_bytes: u64,

    /// Number of indexing threads.
    /// 0 means auto-detect (typically num_cpus / 2).
    pub indexing_threads: u32,

    /// Whether to enable adaptive regime switching.
    /// When true, the controller will adjust policies based on observed load.
    pub adaptive: bool,

    /// Window size for event rate estimation (in seconds).
    pub rate_window_secs: u32,
}

impl Default for IndexTuningConfig {
    fn default() -> Self {
        Self {
            commit: CommitPolicy::default(),
            merge: MergePolicyConfig::default(),
            writer_heap_bytes: 128 * 1024 * 1024, // 128 MiB
            indexing_threads: 0,
            adaptive: true,
            rate_window_secs: 30,
        }
    }
}

// ---------------------------------------------------------------------------
// Segment health snapshot
// ---------------------------------------------------------------------------

/// Point-in-time snapshot of Tantivy index segment health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentHealthSnapshot {
    /// Current number of live segments.
    pub segment_count: u32,
    /// Total size of all segments in bytes.
    pub total_bytes: u64,
    /// Size of the largest segment in bytes.
    pub largest_segment_bytes: u64,
    /// Size of the smallest segment in bytes.
    pub smallest_segment_bytes: u64,
    /// Number of documents across all segments.
    pub total_docs: u64,
    /// Number of deleted (tombstoned) documents pending purge.
    pub deleted_docs: u64,
    /// Number of merge operations currently in progress.
    pub merges_in_progress: u32,
    /// True if segment count exceeds the configured maximum.
    pub needs_merge: bool,
}

impl SegmentHealthSnapshot {
    /// Returns the ratio of deleted docs to total docs (0.0 if no docs).
    #[must_use]
    pub fn deleted_ratio(&self) -> f64 {
        if self.total_docs == 0 {
            0.0
        } else {
            self.deleted_docs as f64 / self.total_docs as f64
        }
    }

    /// Returns the size ratio between largest and smallest segment.
    /// A high ratio suggests unbalanced segments that could benefit from merging.
    #[must_use]
    pub fn size_skew_ratio(&self) -> f64 {
        if self.smallest_segment_bytes == 0 {
            0.0
        } else {
            self.largest_segment_bytes as f64 / self.smallest_segment_bytes as f64
        }
    }
}

// ---------------------------------------------------------------------------
// Commit decision
// ---------------------------------------------------------------------------

/// Decision output from the commit policy evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitDecision {
    /// No commit needed yet.
    Hold,
    /// Commit should be triggered.
    Commit,
    /// Commit is forced due to exceeding a hard limit.
    ForceCommit,
}

/// Tracks state needed for commit policy decisions.
pub struct CommitTracker {
    docs_since_commit: u64,
    bytes_since_commit: u64,
    last_commit: Instant,
    policy: CommitPolicy,
}

impl CommitTracker {
    /// Create a new commit tracker with the given policy.
    #[must_use]
    pub fn new(policy: CommitPolicy) -> Self {
        Self {
            docs_since_commit: 0,
            bytes_since_commit: 0,
            last_commit: Instant::now(),
            policy,
        }
    }

    /// Record that a document was indexed.
    pub fn record_doc(&mut self, size_bytes: u64) {
        self.docs_since_commit += 1;
        self.bytes_since_commit += size_bytes;
    }

    /// Evaluate whether a commit should happen now.
    #[must_use]
    pub fn should_commit(&self) -> CommitDecision {
        let elapsed = self.last_commit.elapsed();

        // Hard limits → ForceCommit
        if self.docs_since_commit >= self.policy.max_docs_before_commit {
            return CommitDecision::ForceCommit;
        }
        if self.bytes_since_commit >= self.policy.max_bytes_before_commit {
            return CommitDecision::ForceCommit;
        }

        // Minimum interval not reached → Hold
        if elapsed < self.policy.min_interval {
            return CommitDecision::Hold;
        }

        // Maximum interval exceeded → Commit
        if elapsed >= self.policy.max_interval {
            if self.docs_since_commit > 0 {
                return CommitDecision::Commit;
            }
            // No docs to commit, just reset timer
            return CommitDecision::Hold;
        }

        CommitDecision::Hold
    }

    /// Reset counters after a commit is performed.
    pub fn mark_committed(&mut self) {
        self.docs_since_commit = 0;
        self.bytes_since_commit = 0;
        self.last_commit = Instant::now();
    }

    /// Returns the current number of buffered documents.
    #[must_use]
    pub fn buffered_docs(&self) -> u64 {
        self.docs_since_commit
    }

    /// Returns the current number of buffered bytes.
    #[must_use]
    pub fn buffered_bytes(&self) -> u64 {
        self.bytes_since_commit
    }

    /// Update the commit policy (e.g. when load regime changes).
    pub fn set_policy(&mut self, policy: CommitPolicy) {
        self.policy = policy;
    }
}

// ---------------------------------------------------------------------------
// Merge decision
// ---------------------------------------------------------------------------

/// Decision output from the merge policy evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeDecision {
    /// No merge needed.
    None,
    /// Merge should be scheduled opportunistically.
    Opportunistic,
    /// Merge is needed due to segment count exceeding threshold.
    Required,
    /// Merge is suppressed (NoMerge strategy or at concurrency limit).
    Suppressed,
}

/// Evaluates merge decisions based on segment health and policy.
pub struct MergePolicyEvaluator {
    config: MergePolicyConfig,
}

impl MergePolicyEvaluator {
    /// Create a new evaluator with the given configuration.
    #[must_use]
    pub fn new(config: MergePolicyConfig) -> Self {
        Self { config }
    }

    /// Evaluate whether merging is needed given current segment health.
    #[must_use]
    pub fn evaluate(&self, health: &SegmentHealthSnapshot) -> MergeDecision {
        if self.config.strategy == MergeStrategy::NoMerge {
            return MergeDecision::Suppressed;
        }

        if health.merges_in_progress >= self.config.max_concurrent_merges {
            return MergeDecision::Suppressed;
        }

        // Hard threshold: too many segments
        if health.segment_count > self.config.max_segment_count {
            return MergeDecision::Required;
        }

        match self.config.strategy {
            MergeStrategy::Aggressive => {
                // Merge when above target
                if health.segment_count > self.config.target_segment_count {
                    MergeDecision::Opportunistic
                } else if health.deleted_ratio() > 0.2 {
                    // Also merge to reclaim space from deleted docs
                    MergeDecision::Opportunistic
                } else {
                    MergeDecision::None
                }
            }
            MergeStrategy::LogMerge => {
                // Merge when significantly above target (50% buffer)
                let threshold =
                    self.config.target_segment_count + self.config.target_segment_count / 2;
                if health.segment_count > threshold {
                    MergeDecision::Opportunistic
                } else if health.size_skew_ratio() > 100.0 && health.segment_count > 3 {
                    // Highly skewed segments — merge small ones
                    MergeDecision::Opportunistic
                } else {
                    MergeDecision::None
                }
            }
            MergeStrategy::Conservative => {
                // Only merge when nearing hard limit
                let threshold = self.config.max_segment_count - self.config.max_segment_count / 4;
                if health.segment_count > threshold {
                    MergeDecision::Opportunistic
                } else {
                    MergeDecision::None
                }
            }
            MergeStrategy::NoMerge => MergeDecision::Suppressed,
        }
    }

    /// Update the merge policy configuration.
    pub fn set_config(&mut self, config: MergePolicyConfig) {
        self.config = config;
    }

    /// Returns the current configuration.
    #[must_use]
    pub fn config(&self) -> &MergePolicyConfig {
        &self.config
    }
}

// ---------------------------------------------------------------------------
// Adaptive policy controller
// ---------------------------------------------------------------------------

/// Lock-free event rate counter for load classification.
pub struct EventRateCounter {
    /// Total events observed in current window.
    count: AtomicU64,
    /// Window start timestamp as epoch millis.
    window_start_ms: AtomicU64,
    /// Window duration in milliseconds.
    window_ms: u64,
}

impl EventRateCounter {
    /// Create a new counter with the given window size.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            count: AtomicU64::new(0),
            window_start_ms: AtomicU64::new(now_ms),
            window_ms: window.as_millis() as u64,
        }
    }

    /// Record one event.
    pub fn record(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record multiple events.
    pub fn record_n(&self, n: u64) {
        self.count.fetch_add(n, Ordering::Relaxed);
    }

    /// Compute the current event rate (events/sec) and optionally rotate the window.
    ///
    /// Returns (rate, was_rotated).
    pub fn rate_and_rotate(&self, now_ms: u64) -> (f64, bool) {
        let start = self.window_start_ms.load(Ordering::Relaxed);
        let elapsed = now_ms.saturating_sub(start);

        if elapsed >= self.window_ms {
            let count = self.count.swap(0, Ordering::Relaxed);
            self.window_start_ms.store(now_ms, Ordering::Relaxed);
            let secs = elapsed as f64 / 1000.0;
            if secs > 0.0 {
                (count as f64 / secs, true)
            } else {
                (0.0, true)
            }
        } else {
            let count = self.count.load(Ordering::Relaxed);
            let secs = elapsed as f64 / 1000.0;
            if secs > 0.0 {
                (count as f64 / secs, false)
            } else {
                (0.0, false)
            }
        }
    }

    /// Get current count without rotation.
    #[must_use]
    pub fn current_count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// Adaptive policy controller that adjusts commit/merge policy based on load.
pub struct AdaptivePolicyController {
    config: IndexTuningConfig,
    current_regime: LoadRegime,
    rate_counter: EventRateCounter,
    regime_change_count: u64,
}

impl AdaptivePolicyController {
    /// Create a new adaptive controller with the given base configuration.
    #[must_use]
    pub fn new(config: IndexTuningConfig) -> Self {
        let window = Duration::from_secs(config.rate_window_secs as u64);
        Self {
            current_regime: LoadRegime::Steady,
            rate_counter: EventRateCounter::new(window),
            regime_change_count: 0,
            config,
        }
    }

    /// Record that events were ingested.
    pub fn record_events(&self, count: u64) {
        self.rate_counter.record_n(count);
    }

    /// Re-evaluate the load regime and return updated policies if changed.
    ///
    /// Returns `Some((commit, merge))` if the regime changed, `None` otherwise.
    pub fn evaluate(&mut self, now_ms: u64) -> Option<(CommitPolicy, MergePolicyConfig)> {
        if !self.config.adaptive {
            return None;
        }

        let (rate, _rotated) = self.rate_counter.rate_and_rotate(now_ms);
        let new_regime = LoadRegime::classify(rate);

        if new_regime != self.current_regime {
            self.current_regime = new_regime;
            self.regime_change_count += 1;
            Some((
                CommitPolicy::for_regime(new_regime),
                MergePolicyConfig::for_regime(new_regime),
            ))
        } else {
            None
        }
    }

    /// Returns the current detected load regime.
    #[must_use]
    pub fn current_regime(&self) -> LoadRegime {
        self.current_regime
    }

    /// Returns how many times the regime has changed.
    #[must_use]
    pub fn regime_change_count(&self) -> u64 {
        self.regime_change_count
    }

    /// Returns the base tuning configuration.
    #[must_use]
    pub fn config(&self) -> &IndexTuningConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- LoadRegime tests --

    #[test]
    fn regime_classify_idle() {
        assert_eq!(LoadRegime::classify(0.0), LoadRegime::Idle);
        assert_eq!(LoadRegime::classify(5.0), LoadRegime::Idle);
        assert_eq!(LoadRegime::classify(9.99), LoadRegime::Idle);
    }

    #[test]
    fn regime_classify_steady() {
        assert_eq!(LoadRegime::classify(10.0), LoadRegime::Steady);
        assert_eq!(LoadRegime::classify(250.0), LoadRegime::Steady);
        assert_eq!(LoadRegime::classify(499.0), LoadRegime::Steady);
    }

    #[test]
    fn regime_classify_burst() {
        assert_eq!(LoadRegime::classify(500.0), LoadRegime::Burst);
        assert_eq!(LoadRegime::classify(2500.0), LoadRegime::Burst);
        assert_eq!(LoadRegime::classify(4999.0), LoadRegime::Burst);
    }

    #[test]
    fn regime_classify_overload() {
        assert_eq!(LoadRegime::classify(5000.0), LoadRegime::Overload);
        assert_eq!(LoadRegime::classify(50000.0), LoadRegime::Overload);
    }

    #[test]
    fn regime_serde_roundtrip() {
        for regime in [
            LoadRegime::Idle,
            LoadRegime::Steady,
            LoadRegime::Burst,
            LoadRegime::Overload,
        ] {
            let json = serde_json::to_string(&regime).unwrap();
            let back: LoadRegime = serde_json::from_str(&json).unwrap();
            assert_eq!(regime, back);
        }
    }

    // -- CommitPolicy tests --

    #[test]
    fn commit_policy_default_is_steady() {
        let default = CommitPolicy::default();
        let steady = CommitPolicy::for_regime(LoadRegime::Steady);
        assert_eq!(default, steady);
    }

    #[test]
    fn commit_policy_idle_has_larger_interval() {
        let idle = CommitPolicy::for_regime(LoadRegime::Idle);
        let steady = CommitPolicy::for_regime(LoadRegime::Steady);
        assert!(idle.max_interval > steady.max_interval);
    }

    #[test]
    fn commit_policy_burst_has_larger_buffer() {
        let burst = CommitPolicy::for_regime(LoadRegime::Burst);
        let steady = CommitPolicy::for_regime(LoadRegime::Steady);
        assert!(burst.max_docs_before_commit > steady.max_docs_before_commit);
    }

    #[test]
    fn commit_policy_overload_max_buffer() {
        let overload = CommitPolicy::for_regime(LoadRegime::Overload);
        let burst = CommitPolicy::for_regime(LoadRegime::Burst);
        assert!(overload.max_docs_before_commit > burst.max_docs_before_commit);
    }

    #[test]
    fn commit_policy_serde_roundtrip() {
        let policy = CommitPolicy::default();
        let json = serde_json::to_string(&policy).unwrap();
        let back: CommitPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, back);
    }

    // -- MergePolicyConfig tests --

    #[test]
    fn merge_config_default_is_log_merge() {
        let cfg = MergePolicyConfig::default();
        assert_eq!(cfg.strategy, MergeStrategy::LogMerge);
    }

    #[test]
    fn merge_config_idle_is_aggressive() {
        let cfg = MergePolicyConfig::for_regime(LoadRegime::Idle);
        assert_eq!(cfg.strategy, MergeStrategy::Aggressive);
    }

    #[test]
    fn merge_config_burst_is_conservative() {
        let cfg = MergePolicyConfig::for_regime(LoadRegime::Burst);
        assert_eq!(cfg.strategy, MergeStrategy::Conservative);
    }

    #[test]
    fn merge_config_overload_disables_merge() {
        let cfg = MergePolicyConfig::for_regime(LoadRegime::Overload);
        assert_eq!(cfg.strategy, MergeStrategy::NoMerge);
    }

    #[test]
    fn merge_config_serde_roundtrip() {
        let cfg = MergePolicyConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: MergePolicyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn merge_config_target_less_than_max() {
        for regime in [
            LoadRegime::Idle,
            LoadRegime::Steady,
            LoadRegime::Burst,
            LoadRegime::Overload,
        ] {
            let cfg = MergePolicyConfig::for_regime(regime);
            assert!(
                cfg.target_segment_count < cfg.max_segment_count,
                "target must be less than max for {:?}",
                regime
            );
        }
    }

    // -- IndexTuningConfig tests --

    #[test]
    fn index_tuning_default_is_adaptive() {
        let cfg = IndexTuningConfig::default();
        assert!(cfg.adaptive);
        assert!(cfg.writer_heap_bytes > 0);
    }

    #[test]
    fn index_tuning_serde_roundtrip() {
        let cfg = IndexTuningConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: IndexTuningConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    // -- SegmentHealthSnapshot tests --

    #[test]
    fn segment_health_deleted_ratio_empty() {
        let h = SegmentHealthSnapshot {
            segment_count: 0,
            total_bytes: 0,
            largest_segment_bytes: 0,
            smallest_segment_bytes: 0,
            total_docs: 0,
            deleted_docs: 0,
            merges_in_progress: 0,
            needs_merge: false,
        };
        assert!(h.deleted_ratio().abs() < f64::EPSILON);
    }

    #[test]
    fn segment_health_deleted_ratio() {
        let h = SegmentHealthSnapshot {
            segment_count: 5,
            total_bytes: 1_000_000,
            largest_segment_bytes: 500_000,
            smallest_segment_bytes: 50_000,
            total_docs: 10_000,
            deleted_docs: 2_000,
            merges_in_progress: 0,
            needs_merge: false,
        };
        assert!((h.deleted_ratio() - 0.2).abs() < 0.001);
    }

    #[test]
    fn segment_health_size_skew() {
        let h = SegmentHealthSnapshot {
            segment_count: 5,
            total_bytes: 1_000_000,
            largest_segment_bytes: 500_000,
            smallest_segment_bytes: 5_000,
            total_docs: 10_000,
            deleted_docs: 0,
            merges_in_progress: 0,
            needs_merge: false,
        };
        assert!((h.size_skew_ratio() - 100.0).abs() < 0.001);
    }

    #[test]
    fn segment_health_size_skew_zero_smallest() {
        let h = SegmentHealthSnapshot {
            segment_count: 1,
            total_bytes: 100,
            largest_segment_bytes: 100,
            smallest_segment_bytes: 0,
            total_docs: 1,
            deleted_docs: 0,
            merges_in_progress: 0,
            needs_merge: false,
        };
        assert!(h.size_skew_ratio().abs() < f64::EPSILON);
    }

    // -- CommitTracker tests --

    #[test]
    fn commit_tracker_initial_hold() {
        let tracker = CommitTracker::new(CommitPolicy::default());
        assert_eq!(tracker.should_commit(), CommitDecision::Hold);
        assert_eq!(tracker.buffered_docs(), 0);
    }

    #[test]
    fn commit_tracker_force_on_doc_limit() {
        let policy = CommitPolicy {
            max_docs_before_commit: 5,
            ..CommitPolicy::default()
        };
        let mut tracker = CommitTracker::new(policy);
        for _ in 0..5 {
            tracker.record_doc(100);
        }
        assert_eq!(tracker.should_commit(), CommitDecision::ForceCommit);
    }

    #[test]
    fn commit_tracker_force_on_byte_limit() {
        let policy = CommitPolicy {
            max_bytes_before_commit: 1000,
            ..CommitPolicy::default()
        };
        let mut tracker = CommitTracker::new(policy);
        tracker.record_doc(1001);
        assert_eq!(tracker.should_commit(), CommitDecision::ForceCommit);
    }

    #[test]
    fn commit_tracker_reset_on_committed() {
        let policy = CommitPolicy {
            max_docs_before_commit: 5,
            ..CommitPolicy::default()
        };
        let mut tracker = CommitTracker::new(policy);
        for _ in 0..5 {
            tracker.record_doc(100);
        }
        assert_eq!(tracker.should_commit(), CommitDecision::ForceCommit);
        tracker.mark_committed();
        assert_eq!(tracker.buffered_docs(), 0);
        assert_eq!(tracker.buffered_bytes(), 0);
        assert_eq!(tracker.should_commit(), CommitDecision::Hold);
    }

    #[test]
    fn commit_tracker_holds_during_min_interval() {
        let policy = CommitPolicy {
            min_interval: Duration::from_secs(60),
            max_interval: Duration::from_secs(120),
            max_docs_before_commit: u64::MAX,
            max_bytes_before_commit: u64::MAX,
        };
        let mut tracker = CommitTracker::new(policy);
        tracker.record_doc(100);
        // min_interval is 60s so should hold
        assert_eq!(tracker.should_commit(), CommitDecision::Hold);
    }

    // -- MergePolicyEvaluator tests --

    fn healthy_snapshot(segment_count: u32) -> SegmentHealthSnapshot {
        SegmentHealthSnapshot {
            segment_count,
            total_bytes: segment_count as u64 * 100_000,
            largest_segment_bytes: 100_000,
            smallest_segment_bytes: 100_000,
            total_docs: segment_count as u64 * 1000,
            deleted_docs: 0,
            merges_in_progress: 0,
            needs_merge: false,
        }
    }

    #[test]
    fn merge_no_merge_suppresses() {
        let eval = MergePolicyEvaluator::new(MergePolicyConfig {
            strategy: MergeStrategy::NoMerge,
            ..Default::default()
        });
        assert_eq!(
            eval.evaluate(&healthy_snapshot(50)),
            MergeDecision::Suppressed
        );
    }

    #[test]
    fn merge_suppressed_at_concurrency_limit() {
        let eval = MergePolicyEvaluator::new(MergePolicyConfig {
            max_concurrent_merges: 2,
            ..Default::default()
        });
        let mut health = healthy_snapshot(50);
        health.merges_in_progress = 2;
        assert_eq!(eval.evaluate(&health), MergeDecision::Suppressed);
    }

    #[test]
    fn merge_required_over_max_segments() {
        let eval = MergePolicyEvaluator::new(MergePolicyConfig {
            max_segment_count: 30,
            ..Default::default()
        });
        assert_eq!(
            eval.evaluate(&healthy_snapshot(31)),
            MergeDecision::Required
        );
    }

    #[test]
    fn merge_aggressive_triggers_above_target() {
        let eval = MergePolicyEvaluator::new(MergePolicyConfig {
            strategy: MergeStrategy::Aggressive,
            target_segment_count: 8,
            max_segment_count: 30,
            ..Default::default()
        });
        assert_eq!(
            eval.evaluate(&healthy_snapshot(10)),
            MergeDecision::Opportunistic
        );
    }

    #[test]
    fn merge_aggressive_ok_below_target() {
        let eval = MergePolicyEvaluator::new(MergePolicyConfig {
            strategy: MergeStrategy::Aggressive,
            target_segment_count: 8,
            max_segment_count: 30,
            ..Default::default()
        });
        assert_eq!(eval.evaluate(&healthy_snapshot(5)), MergeDecision::None);
    }

    #[test]
    fn merge_aggressive_triggers_on_deleted_ratio() {
        let eval = MergePolicyEvaluator::new(MergePolicyConfig {
            strategy: MergeStrategy::Aggressive,
            target_segment_count: 20,
            max_segment_count: 30,
            ..Default::default()
        });
        let mut health = healthy_snapshot(5);
        health.total_docs = 10_000;
        health.deleted_docs = 3_000; // 30% deleted
        assert_eq!(eval.evaluate(&health), MergeDecision::Opportunistic);
    }

    #[test]
    fn merge_log_triggers_above_target_plus_buffer() {
        let eval = MergePolicyEvaluator::new(MergePolicyConfig {
            strategy: MergeStrategy::LogMerge,
            target_segment_count: 8,
            max_segment_count: 30,
            ..Default::default()
        });
        // target + 50% = 12, so 13 should trigger
        assert_eq!(
            eval.evaluate(&healthy_snapshot(13)),
            MergeDecision::Opportunistic
        );
        // 11 should not trigger
        assert_eq!(eval.evaluate(&healthy_snapshot(11)), MergeDecision::None);
    }

    #[test]
    fn merge_log_triggers_on_size_skew() {
        let eval = MergePolicyEvaluator::new(MergePolicyConfig {
            strategy: MergeStrategy::LogMerge,
            target_segment_count: 8,
            max_segment_count: 30,
            ..Default::default()
        });
        let mut health = healthy_snapshot(5);
        health.largest_segment_bytes = 10_000_000;
        health.smallest_segment_bytes = 1_000; // 10,000x skew
        assert_eq!(eval.evaluate(&health), MergeDecision::Opportunistic);
    }

    #[test]
    fn merge_conservative_only_near_max() {
        let eval = MergePolicyEvaluator::new(MergePolicyConfig {
            strategy: MergeStrategy::Conservative,
            target_segment_count: 8,
            max_segment_count: 30,
            ..Default::default()
        });
        // threshold = 30 - 30/4 = 23
        assert_eq!(eval.evaluate(&healthy_snapshot(20)), MergeDecision::None);
        assert_eq!(
            eval.evaluate(&healthy_snapshot(24)),
            MergeDecision::Opportunistic
        );
    }

    // -- EventRateCounter tests --

    #[test]
    fn rate_counter_initial_zero() {
        let counter = EventRateCounter::new(Duration::from_secs(30));
        assert_eq!(counter.current_count(), 0);
    }

    #[test]
    fn rate_counter_records() {
        let counter = EventRateCounter::new(Duration::from_secs(30));
        counter.record();
        counter.record();
        counter.record_n(3);
        assert_eq!(counter.current_count(), 5);
    }

    #[test]
    fn rate_counter_rotates_on_window_expiry() {
        let counter = EventRateCounter::new(Duration::from_secs(1));
        counter.record_n(100);

        let start = counter.window_start_ms.load(Ordering::Relaxed);
        // Simulate window expiry
        let (rate, rotated) = counter.rate_and_rotate(start + 2000);
        assert!(rotated);
        assert!(rate > 0.0);
        // After rotation, count should be reset
        assert_eq!(counter.current_count(), 0);
    }

    #[test]
    fn rate_counter_no_rotate_within_window() {
        let counter = EventRateCounter::new(Duration::from_secs(30));
        counter.record_n(100);

        let start = counter.window_start_ms.load(Ordering::Relaxed);
        let (_rate, rotated) = counter.rate_and_rotate(start + 1000);
        assert!(!rotated);
        // Count should remain
        assert_eq!(counter.current_count(), 100);
    }

    // -- AdaptivePolicyController tests --

    #[test]
    fn adaptive_controller_starts_steady() {
        let ctrl = AdaptivePolicyController::new(IndexTuningConfig::default());
        assert_eq!(ctrl.current_regime(), LoadRegime::Steady);
        assert_eq!(ctrl.regime_change_count(), 0);
    }

    #[test]
    fn adaptive_controller_no_change_when_disabled() {
        let config = IndexTuningConfig {
            adaptive: false,
            ..Default::default()
        };
        let mut ctrl = AdaptivePolicyController::new(config);
        ctrl.record_events(50_000);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60_000;
        assert!(ctrl.evaluate(now).is_none());
    }

    #[test]
    fn adaptive_controller_transitions_on_load() {
        let config = IndexTuningConfig {
            rate_window_secs: 1,
            ..Default::default()
        };
        let mut ctrl = AdaptivePolicyController::new(config);

        // Record enough events for overload in 1-sec window
        ctrl.record_events(10_000);

        let start = ctrl.rate_counter.window_start_ms.load(Ordering::Relaxed);
        // Evaluate after window expires (simulate 1 second)
        let result = ctrl.evaluate(start + 1001);
        assert!(result.is_some());
        assert_eq!(ctrl.current_regime(), LoadRegime::Overload);
        assert_eq!(ctrl.regime_change_count(), 1);
    }

    #[test]
    fn adaptive_controller_returns_regime_specific_policies() {
        let config = IndexTuningConfig {
            rate_window_secs: 1,
            ..Default::default()
        };
        let mut ctrl = AdaptivePolicyController::new(config);

        // Trigger overload
        ctrl.record_events(10_000);
        let start = ctrl.rate_counter.window_start_ms.load(Ordering::Relaxed);
        let result = ctrl.evaluate(start + 1001);

        if let Some((commit, merge)) = result {
            assert_eq!(merge.strategy, MergeStrategy::NoMerge);
            assert!(commit.max_docs_before_commit > CommitPolicy::default().max_docs_before_commit);
        } else {
            panic!("expected regime change");
        }
    }

    #[test]
    fn adaptive_controller_no_change_on_same_regime() {
        let config = IndexTuningConfig {
            rate_window_secs: 1,
            ..Default::default()
        };
        let mut ctrl = AdaptivePolicyController::new(config);

        // Record steady-state events (100 events in 1 sec = 100 eps)
        ctrl.record_events(100);
        let start = ctrl.rate_counter.window_start_ms.load(Ordering::Relaxed);
        // Steady is the default, so no change
        let result = ctrl.evaluate(start + 1001);
        assert!(result.is_none());
        assert_eq!(ctrl.regime_change_count(), 0);
    }
}
