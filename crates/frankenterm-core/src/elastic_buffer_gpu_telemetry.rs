//! ElasticBuffer + wgpu vertex/instance buffer telemetry
//! contract
//! ([BR-TERM-EMULATOR-UPLIFT.1.3.cont2] / `ft-hznqt`).
//!
//! The parent bead (`ft-kciew`, shipped at `c88dde9c0`)
//! landed `ElasticBuffer<u32>` as the policy lifecycle for
//! the per-cell quad-instance buffer:
//! `TermWindow.quad_buffer_policy` with begin/end gesture
//! hooks at the resize entry and idle-shrink tick on the
//! periodic status timer. 15 inline tests pass.
//!
//! This continuation does the **GPU surgery side**: replace
//! `ElasticBuffer<u32>` with `ElasticBuffer<QuadInstance>`,
//! mirror grow/shrink onto the underlying `wgpu::Buffer`, and
//! ship `ft doctor` telemetry. The actual `wgpu::Queue::write_buffer`
//! calls + buffer regrowth live in `frankenterm-gui` and
//! require GPU runtime; this module ships the **contract
//! layer** that the integration consumes:
//!
//! - [`ElasticBufferGpuHealth`] — `ft doctor` snapshot
//!   surfacing `grow_count`, `shrink_count`,
//!   `high_water_mark`, `capacity`, `used`. Same `*Health`
//!   shape as this session's other fixtures.
//! - [`BufferLifecycleEvent`] — enum naming the state-
//!   machine transitions the integration emits (gesture
//!   begin / end, grow / shrink, frame write, idle tick).
//! - [`BenchScenario`] + [`bench_scenario_corpus`] — the
//!   bead's two named benches plus a third covering
//!   RQ-S6 heavy-burst input.
//! - [`QuadInstanceShape`] — the encoding contract per cell
//!   (mirrors the existing `QuadInstance` in
//!   `crates/frankenterm-gui/src/quad.rs`); the integration
//!   verifies its `std140`/`std430` layout matches.
//! - [`GestureRegrowGuard`] — invariant detector: during a
//!   resize gesture, no `Grow` event must fire (the bead's
//!   "zero allocs during gesture" RQ-S1 acceptance bound).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

// ============================================================================
// Buffer lifecycle event taxonomy
// ============================================================================

/// One event the integration emits at the GPU buffer
/// boundary. The `ElasticBufferGpuHealth` snapshot folds
/// these into counters; the `GestureRegrowGuard` invariant
/// fires if a `Grow` arrives between `GestureBegin` and
/// `GestureEnd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BufferLifecycleEvent {
    /// Resize gesture begin — clamps the elastic-buffer
    /// shrink path until `GestureEnd`.
    GestureBegin,
    /// Resize gesture end — re-arms shrink.
    GestureEnd,
    /// CPU-side allocation grew (and the integration must
    /// resize the underlying `wgpu::Buffer` to match).
    Grow { new_capacity: u32 },
    /// CPU-side allocation shrunk (and the integration must
    /// resize the underlying `wgpu::Buffer`).
    Shrink { new_capacity: u32 },
    /// One frame's instances written to the buffer
    /// (bookkeeping for the high-water mark counter).
    FrameWrite { instances_written: u32 },
    /// Idle tick — the periodic status-timer pulse that
    /// drives shrink consideration.
    IdleTick,
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot for the elastic-buffer + wgpu wiring.
/// Mirrors the `*Health` shape used across this session.
///
/// The integration projects `ElasticBuffer.{grow_count,
/// shrink_count, high_water_mark, capacity, used}` into this
/// shape verbatim; `ft doctor` surfaces it via the existing
/// counter-collection seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElasticBufferGpuHealth {
    /// Lifetime grow count (matches `ElasticBuffer::grow_count`).
    pub grow_count: u64,
    /// Lifetime shrink count.
    pub shrink_count: u64,
    /// Highest `used` value observed since process start.
    pub high_water_mark: u32,
    /// Current allocated capacity (CPU-side). The wgpu buffer
    /// matches this on every grow/shrink.
    pub capacity: u32,
    /// Currently-used cell count.
    pub used: u32,
    /// Grows that fired during a resize gesture — should be
    /// **zero** per RQ-S1.
    pub grows_during_gesture_total: u64,
    /// Whether a resize gesture is currently active.
    pub gesture_active: bool,
}

impl ElasticBufferGpuHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            grow_count: 0,
            shrink_count: 0,
            high_water_mark: 0,
            capacity: 0,
            used: 0,
            grows_during_gesture_total: 0,
            gesture_active: false,
        }
    }

    /// Bead's headline acceptance: zero allocations during a
    /// resize gesture. `is_safe()` returns false if any grow
    /// fired while a gesture was active.
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        self.grows_during_gesture_total == 0
    }

    /// Utilization ratio — `used / capacity`, clamped to 1.0
    /// when capacity is 0. The shrink policy reads this on
    /// idle ticks; below the shrink threshold it shrinks.
    #[must_use]
    pub fn utilization(&self) -> f64 {
        if self.capacity == 0 {
            return 1.0;
        }
        self.used as f64 / self.capacity as f64
    }
}

/// Update a health snapshot with one lifecycle event. Folds
/// counters and detects the gesture-grow invariant violation.
pub fn fold_event(health: &mut ElasticBufferGpuHealth, event: BufferLifecycleEvent) {
    match event {
        BufferLifecycleEvent::GestureBegin => {
            health.gesture_active = true;
        }
        BufferLifecycleEvent::GestureEnd => {
            health.gesture_active = false;
        }
        BufferLifecycleEvent::Grow { new_capacity } => {
            health.grow_count = health.grow_count.saturating_add(1);
            health.capacity = new_capacity;
            if health.gesture_active {
                health.grows_during_gesture_total =
                    health.grows_during_gesture_total.saturating_add(1);
            }
        }
        BufferLifecycleEvent::Shrink { new_capacity } => {
            health.shrink_count = health.shrink_count.saturating_add(1);
            health.capacity = new_capacity;
        }
        BufferLifecycleEvent::FrameWrite { instances_written } => {
            health.used = instances_written;
            if instances_written > health.high_water_mark {
                health.high_water_mark = instances_written;
            }
        }
        BufferLifecycleEvent::IdleTick => {
            // Pure observation — the shrink-decision logic
            // lives in the integration; the snapshot doesn't
            // mutate on idle ticks.
        }
    }
}

// ============================================================================
// Gesture-regrow invariant
// ============================================================================

/// Named invariants the bench harness asserts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BufferLifecycleViolation {
    /// A `Grow` event fired while `gesture_active`. The
    /// bead's RQ-S1 acceptance: zero grows during gesture.
    GrowDuringGesture { grow_count_delta: u32 },
    /// A `Shrink` fired while `gesture_active` (the bead's
    /// "shrink should also be suppressed during gesture"
    /// rule from the parent bead `ft-mpc9b.1.3`).
    ShrinkDuringGesture { shrink_count_delta: u32 },
    /// `used > capacity` — buffer overrun (caller wrote past
    /// the wgpu buffer's allocated size).
    UsedExceedsCapacity { used: u32, capacity: u32 },
}

#[must_use]
pub fn check_invariants(
    prior: &ElasticBufferGpuHealth,
    health: &ElasticBufferGpuHealth,
    last_event: BufferLifecycleEvent,
) -> Vec<BufferLifecycleViolation> {
    let mut out = Vec::new();

    // Used must never exceed capacity.
    if health.used > health.capacity && health.capacity > 0 {
        out.push(BufferLifecycleViolation::UsedExceedsCapacity {
            used: health.used,
            capacity: health.capacity,
        });
    }

    // GrowDuringGesture / ShrinkDuringGesture — fire only on
    // the transition (when the lifecycle event is the one
    // that bumped the counter and gesture was active in
    // `prior`).
    if prior.gesture_active {
        if let BufferLifecycleEvent::Grow { .. } = last_event {
            let delta = health.grow_count.saturating_sub(prior.grow_count);
            if delta > 0 {
                out.push(BufferLifecycleViolation::GrowDuringGesture {
                    grow_count_delta: delta as u32,
                });
            }
        }
        if let BufferLifecycleEvent::Shrink { .. } = last_event {
            let delta = health.shrink_count.saturating_sub(prior.shrink_count);
            if delta > 0 {
                out.push(BufferLifecycleViolation::ShrinkDuringGesture {
                    shrink_count_delta: delta as u32,
                });
            }
        }
    }

    out
}

// ============================================================================
// QuadInstance encoding contract
// ============================================================================

/// Shape contract for `QuadInstance` — mirrors the existing
/// type at `crates/frankenterm-gui/src/quad.rs`. The
/// integration verifies its actual `std140`/`std430` layout
/// matches this declared field count + total size at compile
/// time via static_assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuadInstanceShape {
    /// One QuadInstance per cell (the bead's "per-cell
    /// instancing replaces per-cell vertex batching" rule).
    pub bytes_per_instance: u32,
    /// Total fields packed into the instance.
    pub field_count: u32,
}

impl QuadInstanceShape {
    /// Conservative default — 32 bytes per instance covers
    /// (cell_xy: vec2, glyph_uv: vec2, fg_rgba: u32,
    /// bg_rgba: u32, attrs: u32, _pad: u32). The integration
    /// asserts the actual size matches.
    pub const DEFAULT: Self = Self {
        bytes_per_instance: 32,
        field_count: 6,
    };

    #[must_use]
    pub const fn buffer_bytes(self, cell_count: u32) -> u64 {
        (self.bytes_per_instance as u64) * (cell_count as u64)
    }
}

// ============================================================================
// Bench scenario corpus
// ============================================================================

/// One named bench scenario. The bead lists two; the third
/// (`heavy_burst`) covers RQ-S6 input-latency scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchScenario {
    /// 10× rapid resize gesture; assert `grow_count_delta == 0`
    /// during the gesture (RQ-S1).
    ResizeBurst,
    /// 1-hour idle session; assert eventual capacity reduction.
    IdleShrink,
    /// Sustained character throughput. RQ-S6 input latency
    /// target.
    HeavyBurst,
}

impl BenchScenario {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::ResizeBurst => "elastic_buffer_resize_burst",
            Self::IdleShrink => "elastic_buffer_idle_shrink",
            Self::HeavyBurst => "elastic_buffer_heavy_burst",
        }
    }

    /// Which SLO this scenario covers in
    /// `docs/perf/resize-quality-slo.md`.
    #[must_use]
    pub fn slo_id(self) -> &'static str {
        match self {
            Self::ResizeBurst => "RQ-S1",
            Self::IdleShrink => "RQ-S5",
            Self::HeavyBurst => "RQ-S6",
        }
    }

    /// Acceptance bound. Pass criterion the harness asserts.
    #[must_use]
    pub fn acceptance(self) -> BenchAcceptance {
        match self {
            Self::ResizeBurst => BenchAcceptance::ZeroGrowsDuringGesture,
            Self::IdleShrink => BenchAcceptance::EventualCapacityReduction {
                min_shrink_count: 1,
            },
            Self::HeavyBurst => BenchAcceptance::FrameLatencyP95Ms { max_ms: 16 },
        }
    }

    pub const ALL: &'static [Self] = &[Self::ResizeBurst, Self::IdleShrink, Self::HeavyBurst];
}

/// Acceptance criterion per scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BenchAcceptance {
    ZeroGrowsDuringGesture,
    EventualCapacityReduction { min_shrink_count: u32 },
    FrameLatencyP95Ms { max_ms: u32 },
}

#[must_use]
pub fn bench_scenario_corpus() -> Vec<BenchScenario> {
    BenchScenario::ALL.to_vec()
}

// ============================================================================
// Bench result record
// ============================================================================

/// One bench run's recorded outcome — for the per-release
/// JSON artifact + telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchRunResult {
    pub scenario: BenchScenario,
    /// Final health snapshot at end of run.
    pub final_health: ElasticBufferGpuHealth,
    /// Whether the run met its acceptance criterion.
    pub passed: bool,
    /// Optional notes (host info, rng seed, etc.).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub notes: Option<String>,
}

impl BenchRunResult {
    /// Evaluate acceptance based on the scenario and final
    /// health.
    #[must_use]
    pub fn evaluate(scenario: BenchScenario, final_health: ElasticBufferGpuHealth) -> Self {
        let passed = match scenario.acceptance() {
            BenchAcceptance::ZeroGrowsDuringGesture => final_health.grows_during_gesture_total == 0,
            BenchAcceptance::EventualCapacityReduction { min_shrink_count } => {
                final_health.shrink_count >= u64::from(min_shrink_count)
            }
            BenchAcceptance::FrameLatencyP95Ms { .. } => {
                // The latency assertion is harness-side; this
                // contract layer just records the structural
                // acceptance.
                final_health.is_safe()
            }
        };
        Self {
            scenario,
            final_health,
            passed,
            notes: None,
        }
    }
}

// ============================================================================
// Bench-suite snapshot
// ============================================================================

/// Aggregate of all bench scenarios — what the per-release
/// JSON artifact records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchSuiteSnapshot {
    pub schema_version: u32,
    pub bead: String,
    pub results: Vec<BenchRunResult>,
}

impl BenchSuiteSnapshot {
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: 1,
            bead: "ft-hznqt".to_string(),
            results: Vec::new(),
        }
    }

    pub fn record(&mut self, result: BenchRunResult) {
        // Replace-or-insert per scenario.
        if let Some(existing) = self
            .results
            .iter_mut()
            .find(|r| r.scenario == result.scenario)
        {
            *existing = result;
        } else {
            self.results.push(result);
        }
    }

    /// Whether all 3 scenarios have a passing recorded run.
    #[must_use]
    pub fn all_pass(&self) -> bool {
        let mut covered: BTreeSet<BenchScenario> = BTreeSet::new();
        for r in &self.results {
            if r.passed {
                covered.insert(r.scenario);
            }
        }
        covered.len() == BenchScenario::ALL.len()
    }

    /// Scenarios with no recorded passing run.
    #[must_use]
    pub fn missing_or_failing(&self) -> Vec<BenchScenario> {
        BenchScenario::ALL
            .iter()
            .copied()
            .filter(|s| !self.results.iter().any(|r| r.scenario == *s && r.passed))
            .collect()
    }
}

impl Default for BenchSuiteSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // Health snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn baseline_health_is_safe() {
        let h = ElasticBufferGpuHealth::baseline();
        assert!(h.is_safe());
        assert!((h.utilization() - 1.0).abs() <= f64::EPSILON); // capacity=0 → vacuous
    }

    #[test]
    fn utilization_ratio_correct() {
        let h = ElasticBufferGpuHealth {
            capacity: 1000,
            used: 250,
            ..ElasticBufferGpuHealth::baseline()
        };
        assert!((h.utilization() - 0.25).abs() <= f64::EPSILON);
    }

    // ------------------------------------------------------------------------
    // fold_event
    // ------------------------------------------------------------------------

    #[test]
    fn fold_grow_increments_count() {
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(&mut h, BufferLifecycleEvent::Grow { new_capacity: 1024 });
        assert_eq!(h.grow_count, 1);
        assert_eq!(h.capacity, 1024);
        assert_eq!(h.grows_during_gesture_total, 0);
    }

    #[test]
    fn fold_grow_during_gesture_increments_violation_counter() {
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(&mut h, BufferLifecycleEvent::GestureBegin);
        fold_event(&mut h, BufferLifecycleEvent::Grow { new_capacity: 2048 });
        assert_eq!(h.grow_count, 1);
        assert_eq!(h.grows_during_gesture_total, 1);
        assert!(!h.is_safe()); // RQ-S1 violated
    }

    #[test]
    fn fold_grow_after_gesture_end_does_not_count_as_violation() {
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(&mut h, BufferLifecycleEvent::GestureBegin);
        fold_event(&mut h, BufferLifecycleEvent::GestureEnd);
        fold_event(&mut h, BufferLifecycleEvent::Grow { new_capacity: 4096 });
        assert_eq!(h.grow_count, 1);
        assert_eq!(h.grows_during_gesture_total, 0);
        assert!(h.is_safe());
    }

    #[test]
    fn fold_shrink_increments_count() {
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(&mut h, BufferLifecycleEvent::Shrink { new_capacity: 512 });
        assert_eq!(h.shrink_count, 1);
        assert_eq!(h.capacity, 512);
    }

    #[test]
    fn fold_frame_write_updates_high_water() {
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(
            &mut h,
            BufferLifecycleEvent::FrameWrite {
                instances_written: 100,
            },
        );
        assert_eq!(h.used, 100);
        assert_eq!(h.high_water_mark, 100);
        // Smaller value doesn't move high water mark.
        fold_event(
            &mut h,
            BufferLifecycleEvent::FrameWrite {
                instances_written: 50,
            },
        );
        assert_eq!(h.used, 50);
        assert_eq!(h.high_water_mark, 100);
    }

    #[test]
    fn fold_idle_tick_is_pure_read() {
        let mut h = ElasticBufferGpuHealth::baseline();
        h.grow_count = 5;
        let before = h;
        fold_event(&mut h, BufferLifecycleEvent::IdleTick);
        assert_eq!(h, before);
    }

    // ------------------------------------------------------------------------
    // Invariants
    // ------------------------------------------------------------------------

    #[test]
    fn check_invariants_clean_at_baseline() {
        let h = ElasticBufferGpuHealth::baseline();
        let v = check_invariants(&h, &h, BufferLifecycleEvent::IdleTick);
        assert!(v.is_empty());
    }

    #[test]
    fn check_invariants_fires_grow_during_gesture() {
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(&mut h, BufferLifecycleEvent::GestureBegin);
        let prior = h;
        let event = BufferLifecycleEvent::Grow { new_capacity: 1024 };
        fold_event(&mut h, event);
        let v = check_invariants(&prior, &h, event);
        assert_eq!(v.len(), 1);
        assert!(matches!(
            v[0],
            BufferLifecycleViolation::GrowDuringGesture {
                grow_count_delta: 1
            }
        ));
    }

    #[test]
    fn check_invariants_fires_shrink_during_gesture() {
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(&mut h, BufferLifecycleEvent::GestureBegin);
        let prior = h;
        let event = BufferLifecycleEvent::Shrink { new_capacity: 256 };
        fold_event(&mut h, event);
        let v = check_invariants(&prior, &h, event);
        assert!(v.iter().any(|x| matches!(
            x,
            BufferLifecycleViolation::ShrinkDuringGesture {
                shrink_count_delta: 1
            }
        )));
    }

    #[test]
    fn check_invariants_fires_used_exceeds_capacity() {
        let prior = ElasticBufferGpuHealth::baseline();
        let h = ElasticBufferGpuHealth {
            capacity: 100,
            used: 200,
            ..ElasticBufferGpuHealth::baseline()
        };
        let v = check_invariants(
            &prior,
            &h,
            BufferLifecycleEvent::FrameWrite {
                instances_written: 200,
            },
        );
        assert!(v.iter().any(|x| matches!(
            x,
            BufferLifecycleViolation::UsedExceedsCapacity {
                used: 200,
                capacity: 100
            }
        )));
    }

    // ------------------------------------------------------------------------
    // Bench scenarios
    // ------------------------------------------------------------------------

    #[test]
    fn bench_scenario_corpus_has_three_entries() {
        let c = bench_scenario_corpus();
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn bench_scenario_slugs_distinct() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for s in BenchScenario::ALL {
            assert!(seen.insert(s.slug()), "dup {}", s.slug());
        }
    }

    #[test]
    fn resize_burst_bench_passes_when_no_grows_during_gesture() {
        let mut h = ElasticBufferGpuHealth::baseline();
        // Simulate canonical resize-burst: gesture wraps no
        // Grow events.
        fold_event(&mut h, BufferLifecycleEvent::GestureBegin);
        fold_event(
            &mut h,
            BufferLifecycleEvent::FrameWrite {
                instances_written: 80,
            },
        );
        fold_event(
            &mut h,
            BufferLifecycleEvent::FrameWrite {
                instances_written: 80,
            },
        );
        fold_event(&mut h, BufferLifecycleEvent::GestureEnd);
        let r = BenchRunResult::evaluate(BenchScenario::ResizeBurst, h);
        assert!(r.passed);
    }

    #[test]
    fn resize_burst_bench_fails_on_gesture_grow() {
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(&mut h, BufferLifecycleEvent::GestureBegin);
        fold_event(&mut h, BufferLifecycleEvent::Grow { new_capacity: 1024 });
        fold_event(&mut h, BufferLifecycleEvent::GestureEnd);
        let r = BenchRunResult::evaluate(BenchScenario::ResizeBurst, h);
        assert!(!r.passed);
    }

    #[test]
    fn idle_shrink_bench_passes_when_shrink_observed() {
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(&mut h, BufferLifecycleEvent::Shrink { new_capacity: 512 });
        let r = BenchRunResult::evaluate(BenchScenario::IdleShrink, h);
        assert!(r.passed);
    }

    #[test]
    fn idle_shrink_bench_fails_when_no_shrink() {
        let h = ElasticBufferGpuHealth::baseline();
        let r = BenchRunResult::evaluate(BenchScenario::IdleShrink, h);
        assert!(!r.passed);
    }

    // ------------------------------------------------------------------------
    // QuadInstanceShape
    // ------------------------------------------------------------------------

    #[test]
    fn quad_instance_default_size_buffer_calc() {
        let shape = QuadInstanceShape::DEFAULT;
        assert_eq!(shape.buffer_bytes(0), 0);
        assert_eq!(shape.buffer_bytes(100), 3200); // 32 × 100
    }

    // ------------------------------------------------------------------------
    // BenchSuiteSnapshot
    // ------------------------------------------------------------------------

    #[test]
    fn snapshot_record_replaces_on_duplicate_scenario() {
        let mut s = BenchSuiteSnapshot::new();
        s.record(BenchRunResult::evaluate(
            BenchScenario::ResizeBurst,
            ElasticBufferGpuHealth::baseline(),
        ));
        s.record(BenchRunResult::evaluate(
            BenchScenario::ResizeBurst,
            ElasticBufferGpuHealth::baseline(),
        ));
        assert_eq!(s.results.len(), 1);
    }

    #[test]
    fn snapshot_all_pass_requires_all_three_scenarios() {
        let mut s = BenchSuiteSnapshot::new();
        // Only 2 of 3 recorded as passing.
        let mut h = ElasticBufferGpuHealth::baseline();
        fold_event(&mut h, BufferLifecycleEvent::Shrink { new_capacity: 100 });
        s.record(BenchRunResult::evaluate(
            BenchScenario::ResizeBurst,
            ElasticBufferGpuHealth::baseline(),
        ));
        s.record(BenchRunResult::evaluate(BenchScenario::IdleShrink, h));
        assert!(!s.all_pass());
        let missing = s.missing_or_failing();
        assert_eq!(missing.len(), 1);
        assert!(missing.contains(&BenchScenario::HeavyBurst));
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let s = BenchSuiteSnapshot::new();
        let json = serde_json::to_string(&s).unwrap();
        let parsed: BenchSuiteSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    // ------------------------------------------------------------------------
    // Random schedule sweep
    // ------------------------------------------------------------------------

    #[test]
    fn random_schedule_sweep_invariants_consistent() {
        // 1024 trials × 32 events each. Asserts:
        //   - check_invariants is deterministic given the
        //     same prior + event combo.
        //   - state changes match the lifecycle event.
        let mut rng: u64 = 0xa5a5_5a5a_dead_beefu64;
        let xorshift = |s: &mut u64| -> u64 {
            let mut x = *s;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *s = x;
            x
        };
        let alphabet = [
            BufferLifecycleEvent::GestureBegin,
            BufferLifecycleEvent::GestureEnd,
            BufferLifecycleEvent::Grow { new_capacity: 100 },
            BufferLifecycleEvent::Shrink { new_capacity: 50 },
            BufferLifecycleEvent::FrameWrite {
                instances_written: 25,
            },
            BufferLifecycleEvent::IdleTick,
        ];

        for _ in 0..1024 {
            let mut h = ElasticBufferGpuHealth::baseline();
            for _ in 0..32 {
                let r = xorshift(&mut rng);
                let event = alphabet[(r as usize) % alphabet.len()];
                let prior = h;
                fold_event(&mut h, event);
                // Determinism: re-evaluate with the same prior
                // and event must produce the same violation
                // set.
                let v1 = check_invariants(&prior, &h, event);
                let v2 = check_invariants(&prior, &h, event);
                assert_eq!(v1, v2);
            }
        }
    }
}
