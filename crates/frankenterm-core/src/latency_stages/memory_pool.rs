use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::LatencyStage;

// AARSP Bead: ft-2p9cb.3.1 - Memory Ownership Graph & Pool

// AARSP Bead: ft-2p9cb.3.1.1

/// Memory ownership domain: identifies which subsystem owns an allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MemoryDomain {
    /// PTY capture buffers (hot path).
    PtyCapture,
    /// Delta extraction scratch space.
    DeltaExtraction,
    /// Storage write staging area.
    StorageWrite,
    /// Pattern detection working set.
    PatternDetection,
    /// Event bus message queues.
    EventBus,
    /// Workflow executor state.
    WorkflowEngine,
    /// Scrollback ring buffers.
    Scrollback,
    /// Shared/uncategorized.
    Shared,
}

impl MemoryDomain {
    /// All domains in canonical order.
    pub const ALL: [MemoryDomain; 8] = [
        MemoryDomain::PtyCapture,
        MemoryDomain::DeltaExtraction,
        MemoryDomain::StorageWrite,
        MemoryDomain::PatternDetection,
        MemoryDomain::EventBus,
        MemoryDomain::WorkflowEngine,
        MemoryDomain::Scrollback,
        MemoryDomain::Shared,
    ];
}

impl fmt::Display for MemoryDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PtyCapture => write!(f, "pty_capture"),
            Self::DeltaExtraction => write!(f, "delta_extract"),
            Self::StorageWrite => write!(f, "storage_write"),
            Self::PatternDetection => write!(f, "pattern_detect"),
            Self::EventBus => write!(f, "event_bus"),
            Self::WorkflowEngine => write!(f, "workflow"),
            Self::Scrollback => write!(f, "scrollback"),
            Self::Shared => write!(f, "shared"),
        }
    }
}

/// Maps pipeline stages to their primary memory domain.
pub fn stage_to_domain(stage: LatencyStage) -> MemoryDomain {
    match stage {
        LatencyStage::PtyCapture => MemoryDomain::PtyCapture,
        LatencyStage::DeltaExtraction => MemoryDomain::DeltaExtraction,
        LatencyStage::StorageWrite => MemoryDomain::StorageWrite,
        LatencyStage::PatternDetection => MemoryDomain::PatternDetection,
        LatencyStage::EventEmission => MemoryDomain::EventBus,
        LatencyStage::WorkflowDispatch | LatencyStage::ActionExecution => {
            MemoryDomain::WorkflowEngine
        }
        LatencyStage::ApiResponse
        | LatencyStage::EndToEndCapture
        | LatencyStage::EndToEndAction => MemoryDomain::Shared,
    }
}

/// Configuration for a memory pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolConfig {
    /// Domain this pool serves.
    pub domain: MemoryDomain,
    /// Fixed block size in bytes.
    pub block_size: usize,
    /// Initial number of blocks.
    pub initial_blocks: usize,
    /// Maximum blocks (hard cap).
    pub max_blocks: usize,
    /// High-water mark fraction for backpressure (0.0..1.0).
    pub high_water_mark: f64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            domain: MemoryDomain::Shared,
            block_size: 4096,
            initial_blocks: 64,
            max_blocks: 1024,
            high_water_mark: 0.85,
        }
    }
}

impl PoolConfig {
    fn normalized(mut self) -> Self {
        self.high_water_mark = normalize_high_water_mark(self.high_water_mark);
        self
    }
}

fn normalize_high_water_mark(high_water_mark: f64) -> f64 {
    if high_water_mark.is_finite() {
        high_water_mark.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Allocation result from a pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllocResult {
    /// Allocated from free list.
    FromFreeList { block_id: u64 },
    /// Allocated a new block (pool grew).
    Grown { block_id: u64 },
    /// Pool is at max capacity; allocation refused.
    PoolExhausted,
}

/// Per-pool diagnostic snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolSnapshot {
    /// Domain this pool serves.
    pub domain: MemoryDomain,
    /// Block size in bytes.
    pub block_size: usize,
    /// Total blocks allocated (in use + free list).
    pub total_blocks: usize,
    /// Blocks currently in use.
    pub in_use: usize,
    /// Blocks on the free list.
    pub free_count: usize,
    /// Maximum blocks allowed.
    pub max_blocks: usize,
    /// Total allocations since creation.
    pub total_allocs: u64,
    /// Total frees since creation.
    pub total_frees: u64,
    /// Total allocation failures (pool exhausted).
    pub total_exhausted: u64,
    /// Utilization fraction (0.0..1.0).
    pub utilization: f64,
}

/// Fixed-block memory pool. O(1) alloc/free via free list.
///
/// # Invariants
///
/// 1. `in_use + free_count == total_blocks` always.
/// 2. `total_blocks <= max_blocks` always.
/// 3. `total_allocs = total_frees + in_use` (exhausted are refused, not allocated).
/// 4. O(1) allocate and free.
/// 5. Deterministic: same sequence of ops -> same state.
#[derive(Debug, Clone)]
pub struct MemoryPool {
    config: PoolConfig,
    free_list: Vec<u64>,
    in_use_blocks: HashSet<u64>,
    next_block_id: u64,
    total_blocks: usize,
    in_use: usize,
    total_allocs: u64,
    total_frees: u64,
    total_exhausted: u64,
}

impl MemoryPool {
    /// Create a new pool.
    pub fn new(config: PoolConfig) -> Self {
        let config = config.normalized();
        let initial = config.initial_blocks.min(config.max_blocks);
        let free_list: Vec<u64> = (0..initial as u64).collect();
        Self {
            next_block_id: initial as u64,
            total_blocks: initial,
            in_use: 0,
            in_use_blocks: HashSet::new(),
            free_list,
            total_allocs: 0,
            total_frees: 0,
            total_exhausted: 0,
            config,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(PoolConfig::default())
    }

    /// Allocate a block.
    pub fn allocate(&mut self) -> AllocResult {
        // Try free list first.
        if let Some(block_id) = self.free_list.pop() {
            self.in_use += 1;
            self.in_use_blocks.insert(block_id);
            self.total_allocs += 1;
            return AllocResult::FromFreeList { block_id };
        }

        // Try growing.
        if self.total_blocks < self.config.max_blocks {
            let block_id = self.next_block_id;
            self.next_block_id += 1;
            self.total_blocks += 1;
            self.in_use += 1;
            self.in_use_blocks.insert(block_id);
            self.total_allocs += 1;
            return AllocResult::Grown { block_id };
        }

        self.total_exhausted += 1;
        AllocResult::PoolExhausted
    }

    /// Free a block (return to free list).
    ///
    /// Returns `true` when `block_id` was currently owned by the pool and in use.
    /// Duplicate frees and foreign block ids are rejected without mutating pool
    /// accounting.
    pub fn free(&mut self, block_id: u64) -> bool {
        if !self.in_use_blocks.remove(&block_id) {
            return false;
        }
        self.free_list.push(block_id);
        self.in_use -= 1;
        self.total_frees += 1;
        true
    }

    /// Current utilization (in_use / total_blocks).
    pub fn utilization(&self) -> f64 {
        if self.total_blocks == 0 {
            0.0
        } else {
            self.in_use as f64 / self.total_blocks as f64
        }
    }

    /// Whether pool is under pressure (above high-water mark).
    pub fn under_pressure(&self) -> bool {
        self.utilization() >= self.config.high_water_mark
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> PoolSnapshot {
        PoolSnapshot {
            domain: self.config.domain,
            block_size: self.config.block_size,
            total_blocks: self.total_blocks,
            in_use: self.in_use,
            free_count: self.free_list.len(),
            max_blocks: self.config.max_blocks,
            total_allocs: self.total_allocs,
            total_frees: self.total_frees,
            total_exhausted: self.total_exhausted,
            utilization: self.utilization(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        format!(
            "pool[{}] {}/{} util={:.1}% alloc={} free={} exhausted={}",
            self.config.domain,
            self.in_use,
            self.total_blocks,
            self.utilization() * 100.0,
            self.total_allocs,
            self.total_frees,
            self.total_exhausted,
        )
    }

    /// Domain this pool serves.
    pub fn domain(&self) -> MemoryDomain {
        self.config.domain
    }

    /// In-use count.
    pub fn in_use(&self) -> usize {
        self.in_use
    }

    /// Free count.
    pub fn free_count(&self) -> usize {
        self.free_list.len()
    }

    /// Total blocks allocated (in use + free).
    pub fn total_blocks(&self) -> usize {
        self.total_blocks
    }

    /// Shrink pool: return excess free blocks to reclaim memory.
    /// Returns number of blocks reclaimed.
    pub fn shrink(&mut self, target_free: usize) -> usize {
        let excess = self.free_list.len().saturating_sub(target_free);
        if excess > 0 {
            self.free_list.truncate(self.free_list.len() - excess);
            self.total_blocks -= excess;
        }
        excess
    }

    /// Reset pool to initial state.
    pub fn reset(&mut self) {
        let initial = self.config.initial_blocks.min(self.config.max_blocks);
        self.free_list = (0..initial as u64).collect();
        self.in_use_blocks.clear();
        self.next_block_id = initial as u64;
        self.total_blocks = initial;
        self.in_use = 0;
        self.total_allocs = 0;
        self.total_frees = 0;
        self.total_exhausted = 0;
    }
}

// AARSP Bead: ft-2p9cb.3.1.2

/// Degradation signal from the memory pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PoolDegradation {
    /// Pool is healthy.
    Healthy,
    /// Pool is under pressure (utilization above high-water mark).
    HighUtilization { utilization: f64, threshold: f64 },
    /// Pool is exhausted; allocations are failing.
    Exhausted { total_exhausted: u64 },
    /// Pool is fragmented: many blocks but high free count.
    Fragmented {
        total_blocks: usize,
        free_count: usize,
    },
}

impl fmt::Display for PoolDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::HighUtilization {
                utilization,
                threshold,
            } => write!(
                f,
                "HIGH_UTIL({:.1}%/thresh={:.1}%)",
                utilization * 100.0,
                threshold * 100.0
            ),
            Self::Exhausted { total_exhausted } => write!(f, "EXHAUSTED({})", total_exhausted),
            Self::Fragmented {
                total_blocks,
                free_count,
            } => write!(f, "FRAGMENTED({}/{}free)", total_blocks, free_count),
        }
    }
}

/// Structured log entry for pool health.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoolLogEntry {
    /// Domain.
    pub domain: MemoryDomain,
    /// Utilization.
    pub utilization: f64,
    /// In use.
    pub in_use: usize,
    /// Total blocks.
    pub total_blocks: usize,
    /// Degradation signal.
    pub degradation: PoolDegradation,
}

impl MemoryPool {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> PoolDegradation {
        if self.total_exhausted > 0 {
            return PoolDegradation::Exhausted {
                total_exhausted: self.total_exhausted,
            };
        }

        if self.under_pressure() {
            return PoolDegradation::HighUtilization {
                utilization: self.utilization(),
                threshold: self.config.high_water_mark,
            };
        }

        // Fragmentation: total blocks > 2x initial and > 50% free.
        if self.total_blocks > self.config.initial_blocks * 2
            && self.free_list.len() > self.total_blocks / 2
        {
            return PoolDegradation::Fragmented {
                total_blocks: self.total_blocks,
                free_count: self.free_list.len(),
            };
        }

        PoolDegradation::Healthy
    }

    /// Generate a structured log entry.
    pub fn log_entry(&self) -> PoolLogEntry {
        PoolLogEntry {
            domain: self.config.domain,
            utilization: self.utilization(),
            in_use: self.in_use,
            total_blocks: self.total_blocks,
            degradation: self.detect_degradation(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn free_rejects_double_free_and_foreign_block_ids_ft_nyvo1() {
        let config = PoolConfig {
            initial_blocks: 1,
            max_blocks: 1,
            ..Default::default()
        };
        let mut pool = MemoryPool::new(config);
        let block_id = match pool.allocate() {
            AllocResult::FromFreeList { block_id } => block_id,
            other => panic!("expected free-list allocation, got {other:?}"),
        };

        assert!(pool.free(block_id));
        let snapshot_after_valid_free = pool.snapshot();

        assert!(!pool.free(block_id));
        assert!(!pool.free(block_id + 10_000));
        assert_eq!(pool.snapshot(), snapshot_after_valid_free);
        assert_eq!(
            pool.snapshot().total_allocs,
            pool.snapshot().total_frees + pool.snapshot().in_use as u64
        );
    }

    #[test]
    fn non_finite_high_water_mark_fails_closed_to_pressure_ft_esr81() {
        let pool = MemoryPool::new(PoolConfig {
            initial_blocks: 1,
            max_blocks: 1,
            high_water_mark: f64::NAN,
            ..Default::default()
        });

        assert_eq!(pool.config.high_water_mark, 0.0);
        assert!(pool.under_pressure());
    }

    proptest! {
        #[test]
        fn proptest_high_water_mark_is_finite_and_bounded_ft_esr81(high_water_mark in any::<f64>()) {
            let pool = MemoryPool::new(PoolConfig {
                high_water_mark,
                ..Default::default()
            });

            prop_assert!(pool.config.high_water_mark.is_finite());
            prop_assert!((0.0..=1.0).contains(&pool.config.high_water_mark));

            let expected = normalize_high_water_mark(high_water_mark);
            prop_assert_eq!(pool.config.high_water_mark, expected);
            prop_assert_eq!(pool.under_pressure(), pool.utilization() >= expected);
        }
    }
}
