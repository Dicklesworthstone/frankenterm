use std::fmt;

use serde::{Deserialize, Serialize};

// C3: Tiered Scrollback Memory Hierarchy

/// Scrollback storage tier; data migrates Hot -> Warm -> Cold as it ages.
///
/// # Invariants
/// - Hot tier: O(1) random access, RAM-resident, bounded by `hot_max_bytes`.
/// - Warm tier: mmap-backed, O(1) page-fault access, bounded by `warm_max_bytes`.
/// - Cold tier: compressed (zstd-style length-prefix), sequential access only.
/// - Tier transitions are monotonic: once demoted, data never promotes back.
/// - Total bytes across all tiers = sum of segment sizes (conservation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScrollbackTier {
    /// RAM-resident, O(1) random access.
    Hot,
    /// mmap-backed file segments, page-fault access.
    Warm,
    /// Compressed segments, sequential decompression required.
    Cold,
}

impl fmt::Display for ScrollbackTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScrollbackTier::Hot => write!(f, "HOT"),
            ScrollbackTier::Warm => write!(f, "WARM"),
            ScrollbackTier::Cold => write!(f, "COLD"),
        }
    }
}

impl ScrollbackTier {
    /// Ordered tiers from fastest to slowest.
    pub const ALL: [ScrollbackTier; 3] = [
        ScrollbackTier::Hot,
        ScrollbackTier::Warm,
        ScrollbackTier::Cold,
    ];

    /// Numeric rank (0=Hot, 1=Warm, 2=Cold).
    pub fn rank(self) -> usize {
        match self {
            ScrollbackTier::Hot => 0,
            ScrollbackTier::Warm => 1,
            ScrollbackTier::Cold => 2,
        }
    }

    /// Next colder tier, if any.
    pub fn demote(self) -> Option<ScrollbackTier> {
        match self {
            ScrollbackTier::Hot => Some(ScrollbackTier::Warm),
            ScrollbackTier::Warm => Some(ScrollbackTier::Cold),
            ScrollbackTier::Cold => None,
        }
    }
}

/// Per-tier capacity and latency budget configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierConfig {
    pub tier: ScrollbackTier,
    /// Maximum bytes this tier may hold.
    pub max_bytes: u64,
    /// Target retrieval latency in microseconds (p99).
    pub target_latency_us: u64,
    /// Compression ratio estimate (1.0 = no compression, 0.25 = 4:1).
    pub compression_ratio: f64,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            tier: ScrollbackTier::Hot,
            max_bytes: 64 * 1024 * 1024, // 64 MiB
            target_latency_us: 10,       // 10 us
            compression_ratio: 1.0,
        }
    }
}

/// Migration policy governing tier transitions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierMigrationPolicy {
    /// Age threshold (in microseconds) before hot -> warm migration.
    pub hot_to_warm_age_us: u64,
    /// Age threshold (in microseconds) before warm -> cold migration.
    pub warm_to_cold_age_us: u64,
    /// Minimum segment size in bytes to be eligible for migration.
    pub min_segment_bytes: u64,
    /// High-water mark (0.0-1.0) triggering eager demotion.
    pub pressure_threshold: f64,
    /// Maximum concurrent migrations per epoch.
    pub max_concurrent_migrations: usize,
}

impl Default for TierMigrationPolicy {
    fn default() -> Self {
        Self {
            hot_to_warm_age_us: 60_000_000,   // 60 seconds
            warm_to_cold_age_us: 600_000_000, // 10 minutes
            min_segment_bytes: 4096,
            pressure_threshold: 0.85,
            max_concurrent_migrations: 4,
        }
    }
}

/// A contiguous segment of scrollback data tracked by the tier manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollbackSegment {
    pub segment_id: u64,
    pub pane_id: u64,
    pub tier: ScrollbackTier,
    pub byte_size: u64,
    pub line_count: u64,
    pub created_us: u64,
    pub last_accessed_us: u64,
    pub compressed: bool,
}

/// Migration event capturing a tier transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierMigrationEvent {
    pub segment_id: u64,
    pub from_tier: ScrollbackTier,
    pub to_tier: ScrollbackTier,
    pub bytes_migrated: u64,
    pub duration_us: u64,
    pub timestamp_us: u64,
}

/// Snapshot of the tiered scrollback manager state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TieredScrollbackSnapshot {
    pub hot_bytes: u64,
    pub warm_bytes: u64,
    pub cold_bytes: u64,
    pub hot_segments: usize,
    pub warm_segments: usize,
    pub cold_segments: usize,
    pub total_migrations: u64,
    pub total_bytes: u64,
    pub hot_utilization: f64,
    pub warm_utilization: f64,
}

/// Tiered scrollback manager: tracks segments across Hot/Warm/Cold tiers.
///
/// # Invariants
/// - Segment IDs are globally unique and monotonically increasing.
/// - `hot_bytes + warm_bytes + cold_bytes == sum(segment.byte_size)`.
/// - Tier transitions are monotonic (Hot -> Warm -> Cold, never reverse).
/// - Each segment belongs to exactly one tier at any time.
pub struct TieredScrollbackManager {
    hot_config: TierConfig,
    warm_config: TierConfig,
    cold_config: TierConfig,
    policy: TierMigrationPolicy,
    segments: Vec<ScrollbackSegment>,
    next_segment_id: u64,
    hot_bytes: u64,
    warm_bytes: u64,
    cold_bytes: u64,
    migration_events: Vec<TierMigrationEvent>,
    max_events: usize,
    total_migrations: u64,
}

impl TieredScrollbackManager {
    /// Create a new manager with explicit tier configs and migration policy.
    pub fn new(
        hot_config: TierConfig,
        warm_config: TierConfig,
        cold_config: TierConfig,
        policy: TierMigrationPolicy,
    ) -> Self {
        Self {
            hot_config,
            warm_config,
            cold_config,
            policy,
            segments: Vec::new(),
            next_segment_id: 0,
            hot_bytes: 0,
            warm_bytes: 0,
            cold_bytes: 0,
            migration_events: Vec::new(),
            max_events: 1024,
            total_migrations: 0,
        }
    }

    /// Create with sensible defaults (64 MiB hot, 256 MiB warm, 1 GiB cold).
    pub fn with_defaults() -> Self {
        let hot = TierConfig {
            tier: ScrollbackTier::Hot,
            max_bytes: 64 * 1024 * 1024,
            target_latency_us: 10,
            compression_ratio: 1.0,
        };
        let warm = TierConfig {
            tier: ScrollbackTier::Warm,
            max_bytes: 256 * 1024 * 1024,
            target_latency_us: 500,
            compression_ratio: 1.0,
        };
        let cold = TierConfig {
            tier: ScrollbackTier::Cold,
            max_bytes: 1024 * 1024 * 1024,
            target_latency_us: 10_000,
            compression_ratio: 0.25,
        };
        Self::new(hot, warm, cold, TierMigrationPolicy::default())
    }

    /// Ingest a new scrollback segment into the hot tier.
    /// Returns the assigned segment_id.
    pub fn ingest(&mut self, pane_id: u64, byte_size: u64, line_count: u64, now_us: u64) -> u64 {
        let segment_id = self.next_segment_id;
        self.next_segment_id += 1;
        let segment = ScrollbackSegment {
            segment_id,
            pane_id,
            tier: ScrollbackTier::Hot,
            byte_size,
            line_count,
            created_us: now_us,
            last_accessed_us: now_us,
            compressed: false,
        };
        self.segments.push(segment);
        self.hot_bytes += byte_size;
        segment_id
    }

    /// Record an access to a segment (updates last_accessed_us).
    pub fn touch(&mut self, segment_id: u64, now_us: u64) {
        if let Some(seg) = self
            .segments
            .iter_mut()
            .find(|s| s.segment_id == segment_id)
        {
            seg.last_accessed_us = now_us;
        }
    }

    /// Evaluate migration policy and demote eligible segments.
    /// Returns the number of segments migrated.
    pub fn migrate(&mut self, now_us: u64) -> usize {
        let mut migrations: Vec<(usize, ScrollbackTier)> = Vec::new();
        let mut count = 0;

        for (i, seg) in self.segments.iter().enumerate() {
            if count >= self.policy.max_concurrent_migrations {
                break;
            }
            if seg.byte_size < self.policy.min_segment_bytes {
                continue;
            }
            let age = now_us.saturating_sub(seg.last_accessed_us);
            match seg.tier {
                ScrollbackTier::Hot => {
                    let pressure = if self.hot_config.max_bytes > 0 {
                        self.hot_bytes as f64 / self.hot_config.max_bytes as f64
                    } else {
                        0.0
                    };
                    if age >= self.policy.hot_to_warm_age_us
                        || pressure >= self.policy.pressure_threshold
                    {
                        migrations.push((i, ScrollbackTier::Warm));
                        count += 1;
                    }
                }
                ScrollbackTier::Warm => {
                    let pressure = if self.warm_config.max_bytes > 0 {
                        self.warm_bytes as f64 / self.warm_config.max_bytes as f64
                    } else {
                        0.0
                    };
                    if age >= self.policy.warm_to_cold_age_us
                        || pressure >= self.policy.pressure_threshold
                    {
                        migrations.push((i, ScrollbackTier::Cold));
                        count += 1;
                    }
                }
                ScrollbackTier::Cold => {}
            }
        }

        // Apply migrations.
        for (idx, new_tier) in &migrations {
            let seg = &mut self.segments[*idx];
            let from_tier = seg.tier;
            let bytes = seg.byte_size;

            // Adjust tier byte counts.
            match from_tier {
                ScrollbackTier::Hot => self.hot_bytes = self.hot_bytes.saturating_sub(bytes),
                ScrollbackTier::Warm => self.warm_bytes = self.warm_bytes.saturating_sub(bytes),
                ScrollbackTier::Cold => {}
            }
            match new_tier {
                ScrollbackTier::Warm => self.warm_bytes += bytes,
                ScrollbackTier::Cold => {
                    // Apply compression ratio.
                    let compressed = (bytes as f64 * self.cold_config.compression_ratio) as u64;
                    seg.byte_size = compressed.max(1);
                    seg.compressed = true;
                    self.cold_bytes += seg.byte_size;
                }
                ScrollbackTier::Hot => {} // Never happens (monotonic).
            }

            let event = TierMigrationEvent {
                segment_id: seg.segment_id,
                from_tier,
                to_tier: *new_tier,
                bytes_migrated: bytes,
                duration_us: 0, // Simulated; real impl would measure.
                timestamp_us: now_us,
            };

            seg.tier = *new_tier;
            self.total_migrations += 1;

            if self.migration_events.len() < self.max_events {
                self.migration_events.push(event);
            }
        }

        migrations.len()
    }

    /// Lookup a segment by ID.
    pub fn segment(&self, segment_id: u64) -> Option<&ScrollbackSegment> {
        self.segments.iter().find(|s| s.segment_id == segment_id)
    }

    /// Total bytes across all tiers.
    pub fn total_bytes(&self) -> u64 {
        self.hot_bytes + self.warm_bytes + self.cold_bytes
    }

    /// Number of segments in a given tier.
    pub fn tier_segment_count(&self, tier: ScrollbackTier) -> usize {
        self.segments.iter().filter(|s| s.tier == tier).count()
    }

    /// Hot tier utilization (0.0-1.0).
    pub fn hot_utilization(&self) -> f64 {
        if self.hot_config.max_bytes == 0 {
            return 0.0;
        }
        self.hot_bytes as f64 / self.hot_config.max_bytes as f64
    }

    /// Warm tier utilization (0.0-1.0).
    pub fn warm_utilization(&self) -> f64 {
        if self.warm_config.max_bytes == 0 {
            return 0.0;
        }
        self.warm_bytes as f64 / self.warm_config.max_bytes as f64
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> TieredScrollbackSnapshot {
        TieredScrollbackSnapshot {
            hot_bytes: self.hot_bytes,
            warm_bytes: self.warm_bytes,
            cold_bytes: self.cold_bytes,
            hot_segments: self.tier_segment_count(ScrollbackTier::Hot),
            warm_segments: self.tier_segment_count(ScrollbackTier::Warm),
            cold_segments: self.tier_segment_count(ScrollbackTier::Cold),
            total_migrations: self.total_migrations,
            total_bytes: self.total_bytes(),
            hot_utilization: self.hot_utilization(),
            warm_utilization: self.warm_utilization(),
        }
    }

    /// One-line status summary.
    pub fn status_line(&self) -> String {
        format!(
            "scrollback hot={}/{} warm={}/{} cold={} migrations={}",
            self.hot_bytes,
            self.hot_config.max_bytes,
            self.warm_bytes,
            self.warm_config.max_bytes,
            self.cold_bytes,
            self.total_migrations,
        )
    }

    /// Number of segments total.
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Recent migration events.
    pub fn recent_migrations(&self) -> &[TierMigrationEvent] {
        &self.migration_events
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.segments.clear();
        self.migration_events.clear();
        self.next_segment_id = 0;
        self.hot_bytes = 0;
        self.warm_bytes = 0;
        self.cold_bytes = 0;
        self.total_migrations = 0;
    }

    /// Evict all segments for a given pane.
    pub fn evict_pane(&mut self, pane_id: u64) {
        self.segments.retain(|s| {
            if s.pane_id == pane_id {
                match s.tier {
                    ScrollbackTier::Hot => {
                        self.hot_bytes = self.hot_bytes.saturating_sub(s.byte_size);
                    }
                    ScrollbackTier::Warm => {
                        self.warm_bytes = self.warm_bytes.saturating_sub(s.byte_size);
                    }
                    ScrollbackTier::Cold => {
                        self.cold_bytes = self.cold_bytes.saturating_sub(s.byte_size);
                    }
                }
                false
            } else {
                true
            }
        });
    }

    /// Bulk ingest multiple segments. Returns assigned IDs.
    pub fn ingest_bulk(
        &mut self,
        items: &[(u64, u64, u64)], // (pane_id, byte_size, line_count)
        now_us: u64,
    ) -> Vec<u64> {
        items
            .iter()
            .map(|&(pane_id, byte_size, line_count)| {
                self.ingest(pane_id, byte_size, line_count, now_us)
            })
            .collect()
    }

    /// Segments for a given pane, ordered by creation time.
    pub fn segments_for_pane(&self, pane_id: u64) -> Vec<&ScrollbackSegment> {
        self.segments
            .iter()
            .filter(|s| s.pane_id == pane_id)
            .collect()
    }

    /// Tier-specific byte count.
    pub fn tier_bytes(&self, tier: ScrollbackTier) -> u64 {
        match tier {
            ScrollbackTier::Hot => self.hot_bytes,
            ScrollbackTier::Warm => self.warm_bytes,
            ScrollbackTier::Cold => self.cold_bytes,
        }
    }

    /// Total line count across all segments.
    pub fn total_lines(&self) -> u64 {
        self.segments.iter().map(|s| s.line_count).sum()
    }

    /// Evict the oldest hot-tier segments until hot utilization drops below the target ratio.
    /// Evicted segments are removed entirely (not migrated). Returns bytes freed.
    pub fn evict_hot_to_target(&mut self, target_utilization: f64) -> u64 {
        let target_bytes = (self.hot_config.max_bytes as f64 * target_utilization) as u64;
        let mut freed = 0u64;
        while self.hot_bytes > target_bytes {
            // Find the oldest hot segment by created_us.
            let oldest_idx = self
                .segments
                .iter()
                .enumerate()
                .filter(|(_, s)| s.tier == ScrollbackTier::Hot)
                .min_by_key(|(_, s)| s.created_us)
                .map(|(i, _)| i);
            match oldest_idx {
                Some(idx) => {
                    let removed = self.segments.remove(idx);
                    self.hot_bytes = self.hot_bytes.saturating_sub(removed.byte_size);
                    freed += removed.byte_size;
                }
                None => break,
            }
        }
        freed
    }

    /// Oldest segment in the hot tier, if any.
    pub fn oldest_hot_segment(&self) -> Option<&ScrollbackSegment> {
        self.segments
            .iter()
            .filter(|s| s.tier == ScrollbackTier::Hot)
            .min_by_key(|s| s.created_us)
    }

    /// Age of the oldest hot segment in microseconds, or 0 if none.
    pub fn oldest_hot_age_us(&self, now_us: u64) -> u64 {
        self.oldest_hot_segment()
            .map(|s| now_us.saturating_sub(s.last_accessed_us))
            .unwrap_or(0)
    }

    /// Distinct pane IDs with data in the manager.
    pub fn active_pane_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.segments.iter().map(|s| s.pane_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Cold tier utilization (0.0-1.0).
    pub fn cold_utilization(&self) -> f64 {
        if self.cold_config.max_bytes == 0 {
            return 0.0;
        }
        self.cold_bytes as f64 / self.cold_config.max_bytes as f64
    }
}

/// Degradation states for the tiered scrollback system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ScrollbackDegradation {
    Healthy,
    HotPressure {
        utilization: f64,
        threshold: f64,
    },
    WarmPressure {
        utilization: f64,
        threshold: f64,
    },
    MigrationBacklog {
        pending: usize,
        max_concurrent: usize,
    },
}

impl fmt::Display for ScrollbackDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScrollbackDegradation::Healthy => write!(f, "HEALTHY"),
            ScrollbackDegradation::HotPressure {
                utilization,
                threshold,
            } => {
                write!(
                    f,
                    "HOT_PRESSURE({:.1}%/{:.1}%)",
                    utilization * 100.0,
                    threshold * 100.0
                )
            }
            ScrollbackDegradation::WarmPressure {
                utilization,
                threshold,
            } => {
                write!(
                    f,
                    "WARM_PRESSURE({:.1}%/{:.1}%)",
                    utilization * 100.0,
                    threshold * 100.0
                )
            }
            ScrollbackDegradation::MigrationBacklog {
                pending,
                max_concurrent,
            } => {
                write!(f, "MIGRATION_BACKLOG({}/{})", pending, max_concurrent)
            }
        }
    }
}

/// Structured log entry for tiered scrollback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScrollbackLogEntry {
    pub hot_bytes: u64,
    pub warm_bytes: u64,
    pub cold_bytes: u64,
    pub total_segments: usize,
    pub total_migrations: u64,
    pub degradation: ScrollbackDegradation,
}

impl TieredScrollbackManager {
    /// Detect degradation state.
    pub fn detect_degradation(&self) -> ScrollbackDegradation {
        let hot_util = self.hot_utilization();
        if hot_util >= self.policy.pressure_threshold {
            return ScrollbackDegradation::HotPressure {
                utilization: hot_util,
                threshold: self.policy.pressure_threshold,
            };
        }
        let warm_util = self.warm_utilization();
        if warm_util >= self.policy.pressure_threshold {
            return ScrollbackDegradation::WarmPressure {
                utilization: warm_util,
                threshold: self.policy.pressure_threshold,
            };
        }
        // Check if hot tier has many segments ready to migrate.
        let pending = self
            .segments
            .iter()
            .filter(|s| {
                s.tier == ScrollbackTier::Hot && s.byte_size >= self.policy.min_segment_bytes
            })
            .count();
        if pending > self.policy.max_concurrent_migrations * 2 {
            return ScrollbackDegradation::MigrationBacklog {
                pending,
                max_concurrent: self.policy.max_concurrent_migrations,
            };
        }
        ScrollbackDegradation::Healthy
    }

    /// Create a structured log entry.
    pub fn log_entry(&self) -> ScrollbackLogEntry {
        ScrollbackLogEntry {
            hot_bytes: self.hot_bytes,
            warm_bytes: self.warm_bytes,
            cold_bytes: self.cold_bytes,
            total_segments: self.segments.len(),
            total_migrations: self.total_migrations,
            degradation: self.detect_degradation(),
        }
    }
}
