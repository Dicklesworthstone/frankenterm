//! Persistent-rope ↔ TripleBuffer composition contracts
//! ([BR-TERM-EMULATOR-UPLIFT-2.3.3] / `ft-2okh0.3.3`).
//!
//! `TripleBuffer<TerminalState>` holds 3 copies of state.
//! With a persistent (immutable, structurally-shared)
//! rope-backed grid, the 3 copies share structure —
//! typical 3× memory overhead drops to ~1.1×.
//!
//! The persistent rope substrate already exists at
//! `persistent_rope_grid.rs` and the triple-buffer
//! substrate at `triple_buffer.rs` /
//! `watchdoged_triple_buffer.rs`. This module ships the
//! **composition contracts** that govern when (and
//! whether) to migrate `TerminalState` to a rope-backed
//! grid:
//!
//! 1. **Memory-overhead measurement contract** —
//!    [`MemoryOverheadSample`] +
//!    [`MemoryOverheadAggregate`] capture the bench-time
//!    overhead the decision rubric reads.
//! 2. **Decision rubric** — [`decide_rope_adoption`] is
//!    the bead's "ship rope-backed-triple-buffer iff
//!    memory ≤1.5×, render unchanged, mutation
//!    unchanged" decision tree.
//! 3. **Shared-bytes estimator** — [`SharedBytesEstimator`]
//!    is the rope ref-count → bytes-shared projection
//!    contract for the structured log.
//! 4. **Structured-log row contract** —
//!    [`SnapshotLogRow`] mirrors the bead's
//!    `tests/rope_triple_buffer/logs/<scenario>.jsonl`
//!    requirement (per-snapshot:
//!    `ts_ns, total_bytes, shared_bytes`; per-session:
//!    `peak_memory, average_sharing_pct`).
//! 5. **Old-snapshot retention contract** —
//!    [`SnapshotRetentionPolicy`] models the bead's
//!    "Hold a snapshot from 60s ago; assert memory
//!    stable" requirement.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Memory-overhead measurement
// ============================================================================

/// One bench-time sample. The bench at
/// `tests/rope_triple_buffer/` populates a `Vec` of these
/// per scenario; the decision rubric reads the aggregate.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryOverheadSample {
    /// Scenario tag — `idle_60s`, `200_pane_fleet`,
    /// `mutation_burst`, etc.
    pub scenario: String,
    /// Memory used by 3 flat-grid copies (the baseline
    /// the bead's decision rubric compares against).
    pub flat_three_copy_bytes: u64,
    /// Memory used by 3 rope-root copies (with
    /// structural sharing).
    pub rope_three_root_bytes: u64,
}

impl MemoryOverheadSample {
    /// Overhead ratio: rope-bytes / flat-baseline-bytes.
    /// 1.0 = perfect sharing; 3.0 = no sharing (worst).
    /// Returns `None` if the baseline is zero.
    #[must_use]
    pub fn overhead_ratio(&self) -> Option<f64> {
        if self.flat_three_copy_bytes == 0 {
            return None;
        }
        // Flat 3-copy baseline normalized to 1× single
        // copy → divide by 3 to get the per-copy
        // baseline; ratio is rope-3-roots / flat-1-copy.
        let single_copy_baseline = self.flat_three_copy_bytes as f64 / 3.0;
        Some(self.rope_three_root_bytes as f64 / single_copy_baseline)
    }
}

/// Aggregate of N samples — used by the decision rubric.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MemoryOverheadAggregate {
    pub samples: Vec<MemoryOverheadSample>,
}

impl MemoryOverheadAggregate {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, sample: MemoryOverheadSample) {
        self.samples.push(sample);
    }

    /// Worst-case overhead ratio across all samples.
    #[must_use]
    pub fn max_overhead_ratio(&self) -> Option<f64> {
        self.samples
            .iter()
            .filter_map(|s| s.overhead_ratio())
            .fold(None, |acc, r| match acc {
                None => Some(r),
                Some(prev) if r > prev => Some(r),
                Some(prev) => Some(prev),
            })
    }

    /// Mean overhead ratio across all samples.
    #[must_use]
    pub fn mean_overhead_ratio(&self) -> Option<f64> {
        let ratios: Vec<f64> = self
            .samples
            .iter()
            .filter_map(|s| s.overhead_ratio())
            .collect();
        if ratios.is_empty() {
            return None;
        }
        Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
    }
}

// ============================================================================
// Decision rubric
// ============================================================================

/// Performance-comparison input for the decision rubric.
/// Bench measures these as p50/p99 timings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PerformanceComparison {
    /// Render-frame time, flat-grid baseline (ns).
    pub render_p99_ns_flat: u64,
    /// Render-frame time, rope-backed (ns).
    pub render_p99_ns_rope: u64,
    /// Mutation (insert/delete) p99 time, flat (ns).
    pub mutation_p99_ns_flat: u64,
    /// Mutation p99 time, rope-backed (ns).
    pub mutation_p99_ns_rope: u64,
}

impl PerformanceComparison {
    /// Render regression ratio: rope/flat. >1.0 means rope
    /// is slower.
    #[must_use]
    pub fn render_regression_ratio(&self) -> Option<f64> {
        if self.render_p99_ns_flat == 0 {
            return None;
        }
        Some(self.render_p99_ns_rope as f64 / self.render_p99_ns_flat as f64)
    }

    #[must_use]
    pub fn mutation_regression_ratio(&self) -> Option<f64> {
        if self.mutation_p99_ns_flat == 0 {
            return None;
        }
        Some(self.mutation_p99_ns_rope as f64 / self.mutation_p99_ns_flat as f64)
    }
}

/// Decision the rubric emits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RopeAdoptionDecision {
    /// Ship rope-backed triple-buffer.
    Adopt,
    /// Stay on flat-grid triple-buffer (3× memory
    /// acceptable per the bead's "If decision says 'don't
    /// ship rope': triple-buffer still ships with flat
    /// grid").
    StayFlat { reason: AdoptionRejectionReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdoptionRejectionReason {
    /// Memory ratio exceeded 1.5× threshold.
    MemoryOverheadTooHigh,
    /// Render p99 regression exceeded 5%.
    RenderRegression,
    /// Mutation p99 regression exceeded 10% (mutation is
    /// O(log n) on rope vs O(1) on flat — bead allows
    /// "should be fine").
    MutationRegression,
    /// Bench data missing — no decision possible.
    InsufficientData,
}

/// Decision rubric per the bead:
///
/// > Ship rope-backed-triple-buffer if AND ONLY IF:
/// > - Memory overhead with rope ≤1.5× (vs 3× without)
/// > - Render performance unchanged
/// > - Mutation performance unchanged (rope insert/delete
/// >   is O(log n) — should be fine)
///
/// Encoded thresholds:
/// - Memory: ≤1.5× (the bead's stated number).
/// - Render p99: ≤1.05× (5% slack — "unchanged" with
///   measurement noise floor).
/// - Mutation p99: ≤1.10× (10% slack — "should be fine"
///   for O(log n)).
pub const MEMORY_OVERHEAD_THRESHOLD: f64 = 1.5;
pub const RENDER_REGRESSION_THRESHOLD: f64 = 1.05;
pub const MUTATION_REGRESSION_THRESHOLD: f64 = 1.10;

#[must_use]
pub fn decide_rope_adoption(
    memory: &MemoryOverheadAggregate,
    perf: &PerformanceComparison,
) -> RopeAdoptionDecision {
    let Some(max_mem_ratio) = memory.max_overhead_ratio() else {
        return RopeAdoptionDecision::StayFlat {
            reason: AdoptionRejectionReason::InsufficientData,
        };
    };

    if max_mem_ratio > MEMORY_OVERHEAD_THRESHOLD {
        return RopeAdoptionDecision::StayFlat {
            reason: AdoptionRejectionReason::MemoryOverheadTooHigh,
        };
    }

    let Some(render_ratio) = perf.render_regression_ratio() else {
        return RopeAdoptionDecision::StayFlat {
            reason: AdoptionRejectionReason::InsufficientData,
        };
    };
    if render_ratio > RENDER_REGRESSION_THRESHOLD {
        return RopeAdoptionDecision::StayFlat {
            reason: AdoptionRejectionReason::RenderRegression,
        };
    }

    let Some(mutation_ratio) = perf.mutation_regression_ratio() else {
        return RopeAdoptionDecision::StayFlat {
            reason: AdoptionRejectionReason::InsufficientData,
        };
    };
    if mutation_ratio > MUTATION_REGRESSION_THRESHOLD {
        return RopeAdoptionDecision::StayFlat {
            reason: AdoptionRejectionReason::MutationRegression,
        };
    }

    RopeAdoptionDecision::Adopt
}

// ============================================================================
// Shared-bytes estimator
// ============================================================================

/// One rope chunk's ref-count + size — the rope crate
/// exposes this via its leaf-iterator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkRefCount {
    /// Stable hash of the chunk's content (rope's content-
    /// addressed leaf id).
    pub chunk_id: u64,
    /// Bytes in the chunk.
    pub bytes: u32,
    /// How many roots (snapshots) hold a reference.
    pub ref_count: u32,
}

/// Estimator for `total_bytes` + `shared_bytes` per the
/// bead's structured-log requirement.
#[derive(Debug, Clone, Default)]
pub struct SharedBytesEstimator;

impl SharedBytesEstimator {
    /// Total bytes across all chunks.
    #[must_use]
    pub fn total_bytes(&self, chunks: &[ChunkRefCount]) -> u64 {
        chunks.iter().map(|c| c.bytes as u64).sum()
    }

    /// Shared bytes — chunks with `ref_count > 1`. The
    /// bytes are counted *once* (the rope shares them
    /// across snapshots, so the storage cost is single).
    #[must_use]
    pub fn shared_bytes(&self, chunks: &[ChunkRefCount]) -> u64 {
        chunks
            .iter()
            .filter(|c| c.ref_count > 1)
            .map(|c| c.bytes as u64)
            .sum()
    }

    /// Average sharing percentage — `shared_bytes /
    /// total_bytes * 100`. Returns 0 if total is zero.
    #[must_use]
    pub fn average_sharing_pct(&self, chunks: &[ChunkRefCount]) -> f64 {
        let total = self.total_bytes(chunks);
        if total == 0 {
            return 0.0;
        }
        let shared = self.shared_bytes(chunks);
        (shared as f64 / total as f64) * 100.0
    }
}

// ============================================================================
// Structured log row
// ============================================================================

/// One JSONL row at
/// `tests/rope_triple_buffer/logs/<scenario>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotLogRow {
    /// Per-snapshot row: `ts_ns, total_bytes, shared_bytes`.
    Snapshot {
        ts_ns: u64,
        total_bytes: u64,
        shared_bytes: u64,
    },
    /// Per-session summary: `peak_memory, average_sharing_pct`.
    SessionSummary {
        peak_memory_bytes: u64,
        average_sharing_pct_x10000: u32, // bps × 100 (one-decimal precision)
    },
}

#[must_use]
pub fn render_log_jsonl(rows: &[SnapshotLogRow]) -> String {
    let mut out = String::new();
    for r in rows {
        let line = serde_json::to_string(r).expect("SnapshotLogRow always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_log_jsonl(jsonl: &str) -> Result<Vec<SnapshotLogRow>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

// ============================================================================
// Old-snapshot retention contract
// ============================================================================

/// Bead's "Old-snapshot retention" requirement: hold a
/// snapshot from 60s ago; assert memory stable.
///
/// This is a pure-logic policy that consumes the per-
/// snapshot log and answers: "did total memory grow
/// monotonically, or did the rope's structural sharing
/// keep it bounded?"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRetentionPolicy {
    /// Allowed memory growth ratio over the retention
    /// window. 1.10 means memory may grow 10% beyond
    /// baseline — anything more flags as a leak.
    pub growth_ratio_threshold: u32, // basis points (1.10 → 11000)
}

impl Default for SnapshotRetentionPolicy {
    fn default() -> Self {
        // Bead: "assert memory stable" → 10% slack for
        // measurement noise.
        Self {
            growth_ratio_threshold: 11_000, // 1.10×
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionVerdict {
    /// Memory stayed within the threshold; rope sharing
    /// is working.
    Stable,
    /// Memory exceeded the threshold; sharing failed.
    Unstable,
    /// Insufficient data (fewer than 2 snapshots).
    InsufficientData,
}

impl SnapshotRetentionPolicy {
    /// Evaluate a retention window: take the first +
    /// last total-bytes snapshot, compute growth ratio,
    /// compare to threshold.
    #[must_use]
    pub fn evaluate(&self, rows: &[SnapshotLogRow]) -> RetentionVerdict {
        let snapshots: Vec<u64> = rows
            .iter()
            .filter_map(|r| match r {
                SnapshotLogRow::Snapshot { total_bytes, .. } => Some(*total_bytes),
                _ => None,
            })
            .collect();
        if snapshots.len() < 2 {
            return RetentionVerdict::InsufficientData;
        }
        let first = snapshots[0];
        let last = *snapshots.last().expect("len >= 2 checked");
        if first == 0 {
            return RetentionVerdict::InsufficientData;
        }
        let growth_bps = ((last as u128 * 10_000) / first as u128) as u32;
        if growth_bps <= self.growth_ratio_threshold {
            RetentionVerdict::Stable
        } else {
            RetentionVerdict::Unstable
        }
    }
}

// ============================================================================
// Health snapshot
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RopeTripleBufferHealth {
    pub adoption_decision: Option<String>, // "adopt" / "stay_flat:<reason>"
    pub max_observed_overhead_ratio_bps: u32, // basis points
    pub mean_observed_overhead_ratio_bps: u32,
    pub retention_verdict: Option<String>,
    pub snapshots_observed: u32,
    pub events_by_decision: BTreeMap<String, u64>,
}

impl RopeTripleBufferHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self::default()
    }

    pub fn record_decision(&mut self, decision: &RopeAdoptionDecision) {
        let slug = match decision {
            RopeAdoptionDecision::Adopt => "adopt".to_string(),
            RopeAdoptionDecision::StayFlat { reason } => {
                format!(
                    "stay_flat:{}",
                    match reason {
                        AdoptionRejectionReason::MemoryOverheadTooHigh => "memory",
                        AdoptionRejectionReason::RenderRegression => "render",
                        AdoptionRejectionReason::MutationRegression => "mutation",
                        AdoptionRejectionReason::InsufficientData => "no_data",
                    }
                )
            }
        };
        self.adoption_decision = Some(slug.clone());
        *self.events_by_decision.entry(slug).or_insert(0) += 1;
    }

    /// True iff the most recent decision is `Adopt` or
    /// rejection reason is data-driven (not insufficient-
    /// data).
    #[must_use]
    pub fn is_safe(&self) -> bool {
        match &self.adoption_decision {
            None => true, // no decision yet — vacuously safe
            Some(s) if s.starts_with("adopt") => true,
            Some(s) if s == "stay_flat:no_data" => false,
            Some(_) => true, // a real rejection reason is fine
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_baseline_3copy(single_copy_bytes: u64) -> u64 {
        single_copy_bytes * 3
    }

    // ------------------------------------------------------------------------
    // Memory-overhead measurement
    // ------------------------------------------------------------------------

    #[test]
    fn perfect_sharing_yields_1x_overhead() {
        let s = MemoryOverheadSample {
            scenario: "ideal".to_string(),
            flat_three_copy_bytes: flat_baseline_3copy(1_000),
            rope_three_root_bytes: 1_000, // perfect sharing — 1× single copy
        };
        assert_eq!(s.overhead_ratio(), Some(1.0));
    }

    #[test]
    fn no_sharing_yields_3x_overhead() {
        let s = MemoryOverheadSample {
            scenario: "worst_case".to_string(),
            flat_three_copy_bytes: flat_baseline_3copy(1_000),
            rope_three_root_bytes: 3_000, // no sharing
        };
        assert_eq!(s.overhead_ratio(), Some(3.0));
    }

    #[test]
    fn typical_overhead_in_bead_target_range() {
        // Bead claims "~1.1×" with rope. Test that the
        // measurement function correctly reports it.
        let s = MemoryOverheadSample {
            scenario: "typical".to_string(),
            flat_three_copy_bytes: flat_baseline_3copy(1_000),
            rope_three_root_bytes: 1_100,
        };
        assert_eq!(s.overhead_ratio(), Some(1.1));
    }

    #[test]
    fn aggregate_max_picks_worst_scenario() {
        let mut a = MemoryOverheadAggregate::new();
        a.add(MemoryOverheadSample {
            scenario: "a".to_string(),
            flat_three_copy_bytes: 3_000,
            rope_three_root_bytes: 1_100,
        });
        a.add(MemoryOverheadSample {
            scenario: "b".to_string(),
            flat_three_copy_bytes: 3_000,
            rope_three_root_bytes: 1_400,
        });
        assert_eq!(a.max_overhead_ratio(), Some(1.4));
    }

    #[test]
    fn empty_aggregate_returns_none() {
        let a = MemoryOverheadAggregate::new();
        assert_eq!(a.max_overhead_ratio(), None);
        assert_eq!(a.mean_overhead_ratio(), None);
    }

    #[test]
    fn zero_baseline_returns_none() {
        let s = MemoryOverheadSample {
            scenario: "x".to_string(),
            flat_three_copy_bytes: 0,
            rope_three_root_bytes: 100,
        };
        assert_eq!(s.overhead_ratio(), None);
    }

    // ------------------------------------------------------------------------
    // Decision rubric
    // ------------------------------------------------------------------------

    fn good_perf() -> PerformanceComparison {
        PerformanceComparison {
            render_p99_ns_flat: 1_000_000,
            render_p99_ns_rope: 1_010_000, // +1%
            mutation_p99_ns_flat: 100_000,
            mutation_p99_ns_rope: 102_000, // +2%
        }
    }

    fn typical_memory(rope_per_root_bytes: u64) -> MemoryOverheadAggregate {
        let mut a = MemoryOverheadAggregate::new();
        a.add(MemoryOverheadSample {
            scenario: "idle_60s".to_string(),
            flat_three_copy_bytes: 3_000,
            rope_three_root_bytes: rope_per_root_bytes,
        });
        a
    }

    #[test]
    fn decision_adopt_when_all_thresholds_met() {
        let mem = typical_memory(1_100); // 1.1×
        let decision = decide_rope_adoption(&mem, &good_perf());
        assert_eq!(decision, RopeAdoptionDecision::Adopt);
    }

    #[test]
    fn decision_rejects_when_memory_exceeds_15x() {
        let mem = typical_memory(1_600); // 1.6× > 1.5
        let decision = decide_rope_adoption(&mem, &good_perf());
        assert_eq!(
            decision,
            RopeAdoptionDecision::StayFlat {
                reason: AdoptionRejectionReason::MemoryOverheadTooHigh
            }
        );
    }

    #[test]
    fn decision_rejects_render_regression() {
        let mem = typical_memory(1_100);
        let perf = PerformanceComparison {
            render_p99_ns_flat: 1_000_000,
            render_p99_ns_rope: 1_100_000, // +10%
            mutation_p99_ns_flat: 100_000,
            mutation_p99_ns_rope: 100_000,
        };
        let decision = decide_rope_adoption(&mem, &perf);
        assert_eq!(
            decision,
            RopeAdoptionDecision::StayFlat {
                reason: AdoptionRejectionReason::RenderRegression
            }
        );
    }

    #[test]
    fn decision_rejects_mutation_regression() {
        let mem = typical_memory(1_100);
        let perf = PerformanceComparison {
            render_p99_ns_flat: 1_000_000,
            render_p99_ns_rope: 1_010_000,
            mutation_p99_ns_flat: 100_000,
            mutation_p99_ns_rope: 130_000, // +30% > 10%
        };
        let decision = decide_rope_adoption(&mem, &perf);
        assert_eq!(
            decision,
            RopeAdoptionDecision::StayFlat {
                reason: AdoptionRejectionReason::MutationRegression
            }
        );
    }

    #[test]
    fn decision_insufficient_data_when_no_samples() {
        let mem = MemoryOverheadAggregate::new();
        let decision = decide_rope_adoption(&mem, &good_perf());
        assert_eq!(
            decision,
            RopeAdoptionDecision::StayFlat {
                reason: AdoptionRejectionReason::InsufficientData
            }
        );
    }

    #[test]
    fn decision_at_15x_boundary_adopts() {
        let mem = typical_memory(1_500); // exactly 1.5×
        let decision = decide_rope_adoption(&mem, &good_perf());
        assert_eq!(decision, RopeAdoptionDecision::Adopt);
    }

    // ------------------------------------------------------------------------
    // Shared-bytes estimator
    // ------------------------------------------------------------------------

    #[test]
    fn no_sharing_when_all_refcount_1() {
        let chunks = vec![
            ChunkRefCount {
                chunk_id: 1,
                bytes: 100,
                ref_count: 1,
            },
            ChunkRefCount {
                chunk_id: 2,
                bytes: 200,
                ref_count: 1,
            },
        ];
        let est = SharedBytesEstimator;
        assert_eq!(est.total_bytes(&chunks), 300);
        assert_eq!(est.shared_bytes(&chunks), 0);
        assert_eq!(est.average_sharing_pct(&chunks), 0.0);
    }

    #[test]
    fn full_sharing_when_all_refcount_3() {
        let chunks = vec![
            ChunkRefCount {
                chunk_id: 1,
                bytes: 100,
                ref_count: 3,
            },
            ChunkRefCount {
                chunk_id: 2,
                bytes: 200,
                ref_count: 3,
            },
        ];
        let est = SharedBytesEstimator;
        assert_eq!(est.total_bytes(&chunks), 300);
        assert_eq!(est.shared_bytes(&chunks), 300);
        assert_eq!(est.average_sharing_pct(&chunks), 100.0);
    }

    #[test]
    fn partial_sharing() {
        let chunks = vec![
            ChunkRefCount {
                chunk_id: 1,
                bytes: 100,
                ref_count: 3, // shared
            },
            ChunkRefCount {
                chunk_id: 2,
                bytes: 200,
                ref_count: 1, // not shared
            },
        ];
        let est = SharedBytesEstimator;
        assert_eq!(est.total_bytes(&chunks), 300);
        assert_eq!(est.shared_bytes(&chunks), 100);
        let pct = est.average_sharing_pct(&chunks);
        assert!((pct - 33.333).abs() < 0.01);
    }

    #[test]
    fn empty_chunks_safe() {
        let est = SharedBytesEstimator;
        assert_eq!(est.total_bytes(&[]), 0);
        assert_eq!(est.shared_bytes(&[]), 0);
        assert_eq!(est.average_sharing_pct(&[]), 0.0);
    }

    // ------------------------------------------------------------------------
    // Structured log
    // ------------------------------------------------------------------------

    #[test]
    fn structured_log_jsonl_roundtrip() {
        let rows = vec![
            SnapshotLogRow::Snapshot {
                ts_ns: 1_000,
                total_bytes: 10_000,
                shared_bytes: 8_500,
            },
            SnapshotLogRow::SessionSummary {
                peak_memory_bytes: 12_000,
                average_sharing_pct_x10000: 8_500_000,
            },
        ];
        let jsonl = render_log_jsonl(&rows);
        let parsed = parse_log_jsonl(&jsonl).unwrap();
        assert_eq!(parsed, rows);
    }

    // ------------------------------------------------------------------------
    // Retention policy
    // ------------------------------------------------------------------------

    #[test]
    fn retention_stable_when_memory_flat() {
        let rows = vec![
            SnapshotLogRow::Snapshot {
                ts_ns: 0,
                total_bytes: 10_000,
                shared_bytes: 9_000,
            },
            SnapshotLogRow::Snapshot {
                ts_ns: 60_000_000_000,
                total_bytes: 10_100, // +1%
                shared_bytes: 9_050,
            },
        ];
        let p = SnapshotRetentionPolicy::default();
        assert_eq!(p.evaluate(&rows), RetentionVerdict::Stable);
    }

    #[test]
    fn retention_unstable_when_memory_grows_50pct() {
        let rows = vec![
            SnapshotLogRow::Snapshot {
                ts_ns: 0,
                total_bytes: 10_000,
                shared_bytes: 9_000,
            },
            SnapshotLogRow::Snapshot {
                ts_ns: 60_000_000_000,
                total_bytes: 15_000, // +50%
                shared_bytes: 1_000,
            },
        ];
        let p = SnapshotRetentionPolicy::default();
        assert_eq!(p.evaluate(&rows), RetentionVerdict::Unstable);
    }

    #[test]
    fn retention_insufficient_data_with_one_snapshot() {
        let rows = vec![SnapshotLogRow::Snapshot {
            ts_ns: 0,
            total_bytes: 10_000,
            shared_bytes: 9_000,
        }];
        let p = SnapshotRetentionPolicy::default();
        assert_eq!(p.evaluate(&rows), RetentionVerdict::InsufficientData);
    }

    // ------------------------------------------------------------------------
    // Health snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn health_baseline_safe() {
        assert!(RopeTripleBufferHealth::baseline().is_safe());
    }

    #[test]
    fn health_records_decision() {
        let mut h = RopeTripleBufferHealth::baseline();
        h.record_decision(&RopeAdoptionDecision::Adopt);
        assert_eq!(h.adoption_decision.as_deref(), Some("adopt"));
        assert!(h.is_safe());
    }

    #[test]
    fn health_unsafe_on_no_data_rejection() {
        let mut h = RopeTripleBufferHealth::baseline();
        h.record_decision(&RopeAdoptionDecision::StayFlat {
            reason: AdoptionRejectionReason::InsufficientData,
        });
        assert!(!h.is_safe()); // no data is unsafe — bench wasn't run
    }

    #[test]
    fn health_safe_on_real_rejection_reason() {
        let mut h = RopeTripleBufferHealth::baseline();
        h.record_decision(&RopeAdoptionDecision::StayFlat {
            reason: AdoptionRejectionReason::MemoryOverheadTooHigh,
        });
        assert!(h.is_safe()); // real rejection — flat is the right call
    }

    // ------------------------------------------------------------------------
    // Headline scenario
    // ------------------------------------------------------------------------

    #[test]
    fn bead_target_11x_passes_decision() {
        // Bead claims rope drops 3× to "~1.1×". Verify the
        // decision rubric accepts that scenario.
        let mut mem = MemoryOverheadAggregate::new();
        mem.add(MemoryOverheadSample {
            scenario: "200_pane_fleet".to_string(),
            flat_three_copy_bytes: 30_000,
            rope_three_root_bytes: 11_000, // 1.1× single-copy
        });
        let perf = PerformanceComparison {
            render_p99_ns_flat: 1_000_000,
            render_p99_ns_rope: 1_010_000,
            mutation_p99_ns_flat: 100_000,
            mutation_p99_ns_rope: 105_000,
        };
        assert_eq!(
            decide_rope_adoption(&mem, &perf),
            RopeAdoptionDecision::Adopt
        );
    }

    #[test]
    fn old_snapshot_60s_retention_scenario() {
        // Bead's "Hold a snapshot from 60s ago; assert
        // memory stable" requirement.
        let rows = vec![
            SnapshotLogRow::Snapshot {
                ts_ns: 0,
                total_bytes: 100_000,
                shared_bytes: 90_000,
            },
            SnapshotLogRow::Snapshot {
                ts_ns: 30_000_000_000,
                total_bytes: 102_000, // +2%
                shared_bytes: 92_000,
            },
            SnapshotLogRow::Snapshot {
                ts_ns: 60_000_000_000,
                total_bytes: 105_000, // +5%
                shared_bytes: 95_000,
            },
        ];
        let p = SnapshotRetentionPolicy::default();
        assert_eq!(p.evaluate(&rows), RetentionVerdict::Stable);
    }
}
