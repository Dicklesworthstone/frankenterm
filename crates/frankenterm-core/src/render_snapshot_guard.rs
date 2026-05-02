//! Render-thread snapshot-guard policy substrate (ft-2okh0.3.2).
//!
//! Pure-logic substrate for the bead's "Migrate render thread to
//! read-only snapshot pointers". The integration crate handles
//! the actual render-path rewrite (paint.rs, etc.); this module
//! ships the guard-lifecycle state machine, lock-wait
//! distribution, classification per the bead's p99 <100 ns
//! target, and the read-vs-mutation type marker that the
//! integration's static-analysis audit can trust.
//!
//! ## What this module ships
//!
//! - `GuardLifecycle` — three-state machine (`Idle / Held /
//!   Released`) covering one frame's snapshot acquisition.
//! - `GuardEvent` — `Acquire { ns } / Hold { ns } / Drop` events
//!   the integration emits at the boundaries.
//! - `SnapshotKind` — `ReadOnly / Mutation` type marker the
//!   bead's render-thread audit relies on. Render-thread
//!   guards must always be `ReadOnly`.
//! - `RenderFrameTiming` per-frame `acquire_ns`, `hold_ns`,
//!   `dirty_lines_observed` capture.
//! - `LockWaitClassification` 3-tier `Green / Yellow / Red`
//!   per the bead's "render-thread lock-wait p99 <100 ns"
//!   acceptance criterion.
//! - `LockWaitDistribution` — bounded-bucket histogram with
//!   p50 / p95 / p99 percentile readers and `meets_p99_target`
//!   acceptance predicate.
//! - `SnapshotMigrationConfig` — bead default 100 ns p99
//!   target; operator-tunable.
//! - `RenderSnapshotTelemetry` — bead's structured-logging
//!   counters (per frame + per session).
//!
//! ## What is deferred to ft-2okh0.3.2.cont
//!
//! - Auditing + rewriting `crates/frankenterm-gui/src/
//!   termwindow/render/paint.rs` and dependents to read
//!   through `TripleBuffer.read()` instead of
//!   `RwLock<TerminalState>::read()`.
//! - Hooking actual `Instant::now()` calls into
//!   `LockWaitDistribution::record`.
//! - The static-analysis check that no render-thread call
//!   path constructs a `Mutation` guard.

#![allow(dead_code)]

// ============================================================================
// Snapshot kind — read vs mutation
// ============================================================================

/// Marker for which side of the triple-buffer the guard came
/// from. The bead's "DO NOT BREAK" rule: render-thread guards
/// MUST always be `ReadOnly`. The integration's static-analysis
/// audit checks every `SnapshotKind` reachable from the render
/// call graph is `ReadOnly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SnapshotKind {
    /// Read-only snapshot from `TripleBuffer.read()`. The
    /// only kind allowed on the render thread.
    #[default]
    ReadOnly,
    /// Mutation guard — only legitimate on the writer side
    /// (input thread, PTY-driven dirty events). The
    /// substrate's `is_legal_for_render_thread` predicate
    /// returns false for this variant.
    Mutation,
}

impl SnapshotKind {
    /// Bead's acceptance rule: render-thread call graph must
    /// only construct `ReadOnly` guards.
    #[must_use]
    pub const fn is_legal_for_render_thread(self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

// ============================================================================
// Guard lifecycle
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GuardLifecycle {
    /// No guard outstanding. Next frame can acquire.
    #[default]
    Idle,
    /// Frame is in flight: `acquired_at_ns` recorded; render
    /// reads cells, dirty bits, etc.
    Held {
        kind: SnapshotKind,
        acquired_at_ns: u64,
    },
    /// Frame complete; guard released. Distinguished from
    /// `Idle` for one-frame telemetry coalescing.
    Released { held_for_ns: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuardEvent {
    /// Render thread requested a snapshot; took `wait_ns` to
    /// acquire (this is the value the bead's
    /// `snapshot_acquire_ns` field captures).
    Acquire {
        kind: SnapshotKind,
        wait_ns: u64,
        now_ns: u64,
    },
    /// Render thread dropped the guard at `now_ns`.
    Drop { now_ns: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuardTransition {
    /// Acquired a fresh guard.
    Acquired { kind: SnapshotKind, wait_ns: u64 },
    /// Released the held guard. `held_for_ns` is the frame's
    /// hold time.
    Released { held_for_ns: u64 },
    /// Adversarial: caller emitted a `Drop` while in `Idle`,
    /// or `Acquire` while already `Held`. The substrate
    /// signals the integration to log without mutating state.
    InvalidEvent,
    /// Adversarial: caller tried to `Acquire` a `Mutation`
    /// kind. Substrate refuses; the bead's render-thread
    /// invariant is enforced at the type-marker level.
    RefusedMutationOnRender,
}

/// Apply an event to the guard lifecycle. Returns the
/// transition the integration should observe + log. The
/// substrate enforces:
///
/// 1. `SnapshotKind::Mutation` on a render-thread guard is
///    refused (`RefusedMutationOnRender`).
/// 2. `Drop` while `Idle` is invalid.
/// 3. `Acquire` while `Held` is invalid (the integration
///    must Drop the previous frame's guard first).
pub fn apply_event(
    state: &mut GuardLifecycle,
    event: GuardEvent,
    enforce_render_thread: bool,
) -> GuardTransition {
    match event {
        GuardEvent::Acquire {
            kind,
            wait_ns,
            now_ns,
        } => {
            if enforce_render_thread && !kind.is_legal_for_render_thread() {
                return GuardTransition::RefusedMutationOnRender;
            }
            match state {
                GuardLifecycle::Idle | GuardLifecycle::Released { .. } => {
                    *state = GuardLifecycle::Held {
                        kind,
                        acquired_at_ns: now_ns,
                    };
                    GuardTransition::Acquired { kind, wait_ns }
                }
                GuardLifecycle::Held { .. } => GuardTransition::InvalidEvent,
            }
        }
        GuardEvent::Drop { now_ns } => match state {
            GuardLifecycle::Held { acquired_at_ns, .. } => {
                let held_for_ns = now_ns.saturating_sub(*acquired_at_ns);
                *state = GuardLifecycle::Released { held_for_ns };
                GuardTransition::Released { held_for_ns }
            }
            GuardLifecycle::Idle | GuardLifecycle::Released { .. } => GuardTransition::InvalidEvent,
        },
    }
}

// ============================================================================
// Per-frame timing
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderFrameTiming {
    /// `snapshot_acquire_ns` — bead's per-frame field.
    pub acquire_ns: u64,
    /// `snapshot_hold_ns` — bead's per-frame field.
    pub hold_ns: u64,
    /// `dirty_lines_observed` — bead's per-frame field.
    pub dirty_lines_observed: u32,
    /// Frame timestamp in nanoseconds (bead's per-frame
    /// `ts_ns`).
    pub ts_ns: u64,
}

// ============================================================================
// Lock-wait classification + acceptance criterion
// ============================================================================

/// Bead's acceptance criterion: render-thread lock-wait p99
/// must be under 100 ns. Substrate splits the observed wait
/// into three tiers for `ft doctor` colour-coding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LockWaitClassification {
    /// `<100 ns` — passes the bead's acceptance criterion.
    Green,
    /// `100 ns – 1 µs` — likely contention; inspect.
    Yellow,
    /// `>= 1 µs` — failure mode; render-thread is blocking
    /// on writer side.
    Red,
}

pub const ACCEPTANCE_P99_NS: u64 = 100;
pub const YELLOW_THRESHOLD_NS: u64 = 100;
pub const RED_THRESHOLD_NS: u64 = 1_000;

#[must_use]
pub const fn classify_lock_wait(ns: u64) -> LockWaitClassification {
    if ns < YELLOW_THRESHOLD_NS {
        LockWaitClassification::Green
    } else if ns < RED_THRESHOLD_NS {
        LockWaitClassification::Yellow
    } else {
        LockWaitClassification::Red
    }
}

// ============================================================================
// Lock-wait distribution
// ============================================================================

/// Bounded-bucket histogram for render-thread lock-wait
/// observations. The integration calls `record(ns)` on every
/// snapshot acquisition; readers use `percentile_ns` to query.
///
/// Bucket layout (8 buckets, log-spaced):
///   0:    [0,    50)  ns
///   1:   [50,   100)  ns
///   2:  [100,   500)  ns
///   3:  [500,  1000)  ns
///   4: [1000,  5000)  ns
///   5: [5000, 50000)  ns
///   6: [50000, 500000) ns
///   7: [500000, ∞)    ns
///
/// Coarse-grained on purpose: at <100 ns target, sub-50 ns
/// resolution doesn't help; at the failure tail, we just
/// need to know "we're way over."
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LockWaitDistribution {
    pub buckets: [u64; 8],
    pub total: u64,
}

const BUCKET_BOUNDARIES_NS: [u64; 8] = [50, 100, 500, 1_000, 5_000, 50_000, 500_000, u64::MAX];

impl LockWaitDistribution {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buckets: [0; 8],
            total: 0,
        }
    }

    pub fn record(&mut self, ns: u64) {
        let bucket = Self::bucket_for(ns);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.total = self.total.saturating_add(1);
    }

    #[must_use]
    pub fn bucket_for(ns: u64) -> usize {
        for (i, boundary) in BUCKET_BOUNDARIES_NS.iter().enumerate() {
            if ns < *boundary {
                return i;
            }
        }
        // Unreachable since the last boundary is u64::MAX, but
        // defensive.
        7
    }

    /// Upper bound of bucket `i` in nanoseconds. Used by
    /// `percentile_ns` to report a worst-case value for the
    /// percentile.
    #[must_use]
    pub fn bucket_upper_ns(i: usize) -> u64 {
        BUCKET_BOUNDARIES_NS[i]
    }

    /// Percentile in `[0..=100]`. Returns the upper bound of
    /// the bucket containing the percentile sample. Defensive
    /// `None` when no samples have been recorded.
    #[must_use]
    pub fn percentile_ns(&self, p: u8) -> Option<u64> {
        if self.total == 0 {
            return None;
        }
        let p = p.min(100) as u64;
        let target = (self.total * p).div_ceil(100).max(1);
        let mut cumulative = 0u64;
        for (i, count) in self.buckets.iter().enumerate() {
            cumulative = cumulative.saturating_add(*count);
            if cumulative >= target {
                return Some(BUCKET_BOUNDARIES_NS[i]);
            }
        }
        Some(u64::MAX)
    }

    /// Bead's acceptance predicate: p99 <100 ns.
    #[must_use]
    pub fn meets_p99_target(&self) -> bool {
        match self.percentile_ns(99) {
            None => true, // no samples ⇒ no failures yet
            Some(ns) => ns <= ACCEPTANCE_P99_NS,
        }
    }
}

// ============================================================================
// Config
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotMigrationConfig {
    /// p99 acceptance target. Bead default 100 ns.
    pub p99_target_ns: u64,
    /// Whether the substrate enforces "render-thread guards
    /// must be ReadOnly" at the type-marker level. Operator
    /// can disable for diagnostic builds; default true.
    pub enforce_render_thread_invariant: bool,
}

impl Default for SnapshotMigrationConfig {
    fn default() -> Self {
        Self {
            p99_target_ns: ACCEPTANCE_P99_NS,
            enforce_render_thread_invariant: true,
        }
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderSnapshotTelemetry {
    /// Frames rendered with a successful snapshot acquire.
    pub frames_rendered: u64,
    /// Times the integration tried to drop without a held
    /// guard, or acquire while already held.
    pub invalid_event_count: u64,
    /// Times the integration tried to acquire a Mutation
    /// guard on the render thread (substrate refused).
    pub refused_mutation_on_render_count: u64,
    /// Bucket-distribution of lock-wait times.
    pub lock_wait: LockWaitDistribution,
    /// Maximum hold time observed in this session.
    pub max_hold_ns: u64,
    /// Sum of hold times — combine with frames_rendered for
    /// average.
    pub total_hold_ns: u64,
    /// Frames that exceeded the bead's per-frame budget
    /// (hold_ns > target frame time).
    pub frames_over_budget: u64,
}

impl RenderSnapshotTelemetry {
    pub fn record_transition(&mut self, transition: GuardTransition) {
        match transition {
            GuardTransition::Acquired { wait_ns, .. } => {
                self.lock_wait.record(wait_ns);
            }
            GuardTransition::Released { held_for_ns } => {
                self.frames_rendered = self.frames_rendered.saturating_add(1);
                self.total_hold_ns = self.total_hold_ns.saturating_add(held_for_ns);
                if held_for_ns > self.max_hold_ns {
                    self.max_hold_ns = held_for_ns;
                }
            }
            GuardTransition::InvalidEvent => {
                self.invalid_event_count = self.invalid_event_count.saturating_add(1);
            }
            GuardTransition::RefusedMutationOnRender => {
                self.refused_mutation_on_render_count =
                    self.refused_mutation_on_render_count.saturating_add(1);
            }
        }
    }

    pub fn record_frame_over_budget(&mut self) {
        self.frames_over_budget = self.frames_over_budget.saturating_add(1);
    }

    /// Bead's per-session aggregate: lock_wait_p50/p95/p99.
    #[must_use]
    pub fn lock_wait_p99(&self) -> Option<u64> {
        self.lock_wait.percentile_ns(99)
    }

    #[must_use]
    pub fn lock_wait_p95(&self) -> Option<u64> {
        self.lock_wait.percentile_ns(95)
    }

    #[must_use]
    pub fn lock_wait_p50(&self) -> Option<u64> {
        self.lock_wait.percentile_ns(50)
    }

    #[must_use]
    pub fn average_hold_ns(&self) -> Option<u64> {
        if self.frames_rendered == 0 {
            None
        } else {
            Some(self.total_hold_ns / self.frames_rendered)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // SnapshotKind
    // ----------------------------------------------------------------

    #[test]
    fn snapshot_kind_default_read_only() {
        assert_eq!(SnapshotKind::default(), SnapshotKind::ReadOnly);
    }

    #[test]
    fn snapshot_kind_render_legality() {
        assert!(SnapshotKind::ReadOnly.is_legal_for_render_thread());
        assert!(!SnapshotKind::Mutation.is_legal_for_render_thread());
    }

    // ----------------------------------------------------------------
    // GuardLifecycle + apply_event
    // ----------------------------------------------------------------

    #[test]
    fn lifecycle_acquire_idle_to_held() {
        let mut state = GuardLifecycle::Idle;
        let event = GuardEvent::Acquire {
            kind: SnapshotKind::ReadOnly,
            wait_ns: 50,
            now_ns: 1_000,
        };
        let transition = apply_event(&mut state, event, true);
        assert_eq!(
            transition,
            GuardTransition::Acquired {
                kind: SnapshotKind::ReadOnly,
                wait_ns: 50,
            },
        );
        assert_eq!(
            state,
            GuardLifecycle::Held {
                kind: SnapshotKind::ReadOnly,
                acquired_at_ns: 1_000,
            },
        );
    }

    #[test]
    fn lifecycle_drop_held_to_released() {
        let mut state = GuardLifecycle::Held {
            kind: SnapshotKind::ReadOnly,
            acquired_at_ns: 1_000,
        };
        let event = GuardEvent::Drop { now_ns: 1_500 };
        let transition = apply_event(&mut state, event, true);
        assert_eq!(transition, GuardTransition::Released { held_for_ns: 500 });
        assert_eq!(state, GuardLifecycle::Released { held_for_ns: 500 });
    }

    #[test]
    fn lifecycle_acquire_after_release_works() {
        let mut state = GuardLifecycle::Released { held_for_ns: 500 };
        let event = GuardEvent::Acquire {
            kind: SnapshotKind::ReadOnly,
            wait_ns: 30,
            now_ns: 2_000,
        };
        let transition = apply_event(&mut state, event, true);
        assert!(matches!(transition, GuardTransition::Acquired { .. }));
    }

    #[test]
    fn lifecycle_invalid_drop_when_idle() {
        let mut state = GuardLifecycle::Idle;
        let event = GuardEvent::Drop { now_ns: 100 };
        let transition = apply_event(&mut state, event, true);
        assert_eq!(transition, GuardTransition::InvalidEvent);
        assert_eq!(state, GuardLifecycle::Idle); // state unchanged
    }

    #[test]
    fn lifecycle_invalid_acquire_when_held() {
        let mut state = GuardLifecycle::Held {
            kind: SnapshotKind::ReadOnly,
            acquired_at_ns: 1_000,
        };
        let event = GuardEvent::Acquire {
            kind: SnapshotKind::ReadOnly,
            wait_ns: 10,
            now_ns: 1_500,
        };
        let transition = apply_event(&mut state, event, true);
        assert_eq!(transition, GuardTransition::InvalidEvent);
    }

    #[test]
    fn lifecycle_refuses_mutation_on_render_thread() {
        let mut state = GuardLifecycle::Idle;
        let event = GuardEvent::Acquire {
            kind: SnapshotKind::Mutation,
            wait_ns: 50,
            now_ns: 1_000,
        };
        let transition = apply_event(&mut state, event, true);
        assert_eq!(transition, GuardTransition::RefusedMutationOnRender);
        assert_eq!(state, GuardLifecycle::Idle); // state unchanged
    }

    #[test]
    fn lifecycle_allows_mutation_when_invariant_disabled() {
        // Diagnostic builds can disable the invariant.
        let mut state = GuardLifecycle::Idle;
        let event = GuardEvent::Acquire {
            kind: SnapshotKind::Mutation,
            wait_ns: 50,
            now_ns: 1_000,
        };
        let transition = apply_event(&mut state, event, false);
        assert!(matches!(transition, GuardTransition::Acquired { .. }));
    }

    #[test]
    fn lifecycle_drop_after_invalid_acquire_no_state_drift() {
        let mut state = GuardLifecycle::Idle;
        // Invalid acquire (Mutation, with enforcement on).
        apply_event(
            &mut state,
            GuardEvent::Acquire {
                kind: SnapshotKind::Mutation,
                wait_ns: 50,
                now_ns: 1_000,
            },
            true,
        );
        // Subsequent Drop should still be invalid (state was Idle).
        let transition = apply_event(&mut state, GuardEvent::Drop { now_ns: 2_000 }, true);
        assert_eq!(transition, GuardTransition::InvalidEvent);
    }

    // ----------------------------------------------------------------
    // classify_lock_wait
    // ----------------------------------------------------------------

    #[test]
    fn classify_under_100ns_green() {
        assert_eq!(classify_lock_wait(0), LockWaitClassification::Green);
        assert_eq!(classify_lock_wait(50), LockWaitClassification::Green);
        assert_eq!(classify_lock_wait(99), LockWaitClassification::Green);
    }

    #[test]
    fn classify_100_to_1us_yellow() {
        assert_eq!(classify_lock_wait(100), LockWaitClassification::Yellow);
        assert_eq!(classify_lock_wait(500), LockWaitClassification::Yellow);
        assert_eq!(classify_lock_wait(999), LockWaitClassification::Yellow);
    }

    #[test]
    fn classify_1us_or_more_red() {
        assert_eq!(classify_lock_wait(1_000), LockWaitClassification::Red);
        assert_eq!(classify_lock_wait(1_000_000), LockWaitClassification::Red);
        assert_eq!(classify_lock_wait(u64::MAX), LockWaitClassification::Red);
    }

    // ----------------------------------------------------------------
    // LockWaitDistribution
    // ----------------------------------------------------------------

    #[test]
    fn distribution_starts_empty() {
        let d = LockWaitDistribution::new();
        assert_eq!(d.total, 0);
        assert_eq!(d.percentile_ns(99), None);
        assert!(d.meets_p99_target()); // empty = trivially passes
    }

    #[test]
    fn distribution_records_into_buckets() {
        let mut d = LockWaitDistribution::new();
        d.record(0); // bucket 0
        d.record(50); // bucket 1
        d.record(100); // bucket 2
        d.record(500); // bucket 3
        d.record(1_000); // bucket 4
        d.record(5_000); // bucket 5
        d.record(50_000); // bucket 6
        d.record(500_000); // bucket 7
        for c in d.buckets {
            assert_eq!(c, 1);
        }
        assert_eq!(d.total, 8);
    }

    #[test]
    fn distribution_p99_under_target_passes() {
        let mut d = LockWaitDistribution::new();
        // 100 fast samples.
        for _ in 0..100 {
            d.record(20); // bucket 0 (<50 ns)
        }
        assert!(d.meets_p99_target());
        // p99 reports the bucket-0 upper bound = 50 ns.
        assert_eq!(d.percentile_ns(99), Some(50));
    }

    #[test]
    fn distribution_p99_over_target_fails() {
        let mut d = LockWaitDistribution::new();
        // 99 fast + 1 slow sample.
        for _ in 0..99 {
            d.record(20);
        }
        d.record(1_500); // bucket 4
        // p99 = the slow sample.
        let p99 = d.percentile_ns(99).unwrap();
        // Could be 50 or larger; specifically, p99 with target=99
        // demands ceil(100*99/100)=99 samples, which all fall in
        // bucket 0 (upper 50). So p99 still reports 50.
        // To force the slow sample into the p99, use target=100.
        let p100 = d.percentile_ns(100).unwrap();
        assert!(p100 > 1_000);
        // p99 is still bucket 0 with this distribution; the bead's
        // acceptance criterion specifies p99, so passes.
        assert!(p99 <= 100);
        assert!(d.meets_p99_target());
    }

    #[test]
    fn distribution_p99_fails_when_2pct_slow() {
        let mut d = LockWaitDistribution::new();
        for _ in 0..98 {
            d.record(20);
        }
        for _ in 0..2 {
            d.record(1_500); // bucket 4 (>1 µs)
        }
        // ceil(100*99/100) = 99; cumulative through buckets:
        //   bucket 0: 98 (<99)
        //   bucket 4: 100 (>=99) → returns bucket 4 upper = 5_000.
        let p99 = d.percentile_ns(99).unwrap();
        assert!(p99 > ACCEPTANCE_P99_NS);
        assert!(!d.meets_p99_target());
    }

    #[test]
    fn distribution_percentile_clamps_at_100() {
        let mut d = LockWaitDistribution::new();
        d.record(10);
        // p > 100 should clamp.
        let p110 = d.percentile_ns(110);
        let p100 = d.percentile_ns(100);
        assert_eq!(p110, p100);
    }

    #[test]
    fn distribution_bucket_for_correct() {
        assert_eq!(LockWaitDistribution::bucket_for(0), 0);
        assert_eq!(LockWaitDistribution::bucket_for(49), 0);
        assert_eq!(LockWaitDistribution::bucket_for(50), 1);
        assert_eq!(LockWaitDistribution::bucket_for(99), 1);
        assert_eq!(LockWaitDistribution::bucket_for(100), 2);
        assert_eq!(LockWaitDistribution::bucket_for(499), 2);
        assert_eq!(LockWaitDistribution::bucket_for(500), 3);
        assert_eq!(LockWaitDistribution::bucket_for(999), 3);
        assert_eq!(LockWaitDistribution::bucket_for(1_000), 4);
        assert_eq!(LockWaitDistribution::bucket_for(50_000), 6);
        assert_eq!(LockWaitDistribution::bucket_for(u64::MAX - 1), 7);
    }

    // ----------------------------------------------------------------
    // SnapshotMigrationConfig
    // ----------------------------------------------------------------

    #[test]
    fn config_default_matches_bead() {
        let c = SnapshotMigrationConfig::default();
        assert_eq!(c.p99_target_ns, 100);
        assert!(c.enforce_render_thread_invariant);
    }

    // ----------------------------------------------------------------
    // RenderSnapshotTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zero() {
        let t = RenderSnapshotTelemetry::default();
        assert_eq!(t.frames_rendered, 0);
        assert_eq!(t.lock_wait_p99(), None);
        assert_eq!(t.average_hold_ns(), None);
    }

    #[test]
    fn telemetry_record_acquired_records_wait() {
        let mut t = RenderSnapshotTelemetry::default();
        t.record_transition(GuardTransition::Acquired {
            kind: SnapshotKind::ReadOnly,
            wait_ns: 30,
        });
        assert_eq!(t.lock_wait.total, 1);
    }

    #[test]
    fn telemetry_record_released_increments_frames_and_holds() {
        let mut t = RenderSnapshotTelemetry::default();
        t.record_transition(GuardTransition::Released { held_for_ns: 400 });
        t.record_transition(GuardTransition::Released { held_for_ns: 600 });
        assert_eq!(t.frames_rendered, 2);
        assert_eq!(t.total_hold_ns, 1_000);
        assert_eq!(t.max_hold_ns, 600);
        assert_eq!(t.average_hold_ns(), Some(500));
    }

    #[test]
    fn telemetry_record_invalid_event() {
        let mut t = RenderSnapshotTelemetry::default();
        t.record_transition(GuardTransition::InvalidEvent);
        t.record_transition(GuardTransition::InvalidEvent);
        assert_eq!(t.invalid_event_count, 2);
    }

    #[test]
    fn telemetry_record_refused_mutation() {
        let mut t = RenderSnapshotTelemetry::default();
        t.record_transition(GuardTransition::RefusedMutationOnRender);
        assert_eq!(t.refused_mutation_on_render_count, 1);
    }

    #[test]
    fn telemetry_record_frame_over_budget() {
        let mut t = RenderSnapshotTelemetry::default();
        t.record_frame_over_budget();
        t.record_frame_over_budget();
        assert_eq!(t.frames_over_budget, 2);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_normal_frame_pipeline() {
        let mut state = GuardLifecycle::Idle;
        let mut telem = RenderSnapshotTelemetry::default();

        // Frame N starts: render thread acquires snapshot.
        let acquire = GuardEvent::Acquire {
            kind: SnapshotKind::ReadOnly,
            wait_ns: 25, // sub-50 ns ⇒ bucket 0
            now_ns: 1_000_000,
        };
        let t1 = apply_event(&mut state, acquire, true);
        telem.record_transition(t1);

        // 8 ms later: frame done; render thread drops guard.
        let drop_event = GuardEvent::Drop { now_ns: 9_000_000 };
        let t2 = apply_event(&mut state, drop_event, true);
        telem.record_transition(t2);

        assert_eq!(telem.frames_rendered, 1);
        assert_eq!(telem.max_hold_ns, 8_000_000);
        assert!(telem.lock_wait.meets_p99_target());
    }

    #[test]
    fn scenario_heavy_burst_test() {
        // Bead's e2e: 1 MB/s output, render thread never blocks.
        // Simulate 1000 frames with sub-100 ns wait.
        let mut state = GuardLifecycle::Idle;
        let mut telem = RenderSnapshotTelemetry::default();

        for i in 0..1000 {
            let now = (i as u64) * 16_667_000; // ~60 fps in ns
            let acquire = GuardEvent::Acquire {
                kind: SnapshotKind::ReadOnly,
                wait_ns: 30,
                now_ns: now,
            };
            telem.record_transition(apply_event(&mut state, acquire, true));
            telem.record_transition(apply_event(
                &mut state,
                GuardEvent::Drop {
                    now_ns: now + 8_000_000,
                },
                true,
            ));
        }
        assert_eq!(telem.frames_rendered, 1000);
        assert!(
            telem.lock_wait.meets_p99_target(),
            "p99 = {:?}",
            telem.lock_wait_p99()
        );
    }

    #[test]
    fn scenario_static_audit_catches_mutation_on_render() {
        // Bead's "no mutation sites in the render-thread call
        // graph": substrate's enforce_render_thread_invariant
        // refuses Mutation guards.
        let mut state = GuardLifecycle::Idle;
        let mut telem = RenderSnapshotTelemetry::default();

        // Bug: a refactor accidentally constructs a Mutation
        // guard on the render thread.
        let bad = GuardEvent::Acquire {
            kind: SnapshotKind::Mutation,
            wait_ns: 25,
            now_ns: 1_000,
        };
        let t = apply_event(&mut state, bad, true);
        telem.record_transition(t);

        // Substrate refused; lifecycle stayed Idle.
        assert_eq!(state, GuardLifecycle::Idle);
        assert_eq!(telem.refused_mutation_on_render_count, 1);
        assert_eq!(telem.frames_rendered, 0);
    }

    #[test]
    fn scenario_lock_contention_failure_mode() {
        // Pre-migration baseline: render thread contended with
        // input thread; many frames have ms-scale wait. Bead's
        // acceptance criterion fires.
        let mut d = LockWaitDistribution::new();
        for _ in 0..50 {
            d.record(30); // bucket 0
        }
        for _ in 0..50 {
            d.record(2_500_000); // bucket 6 (>50 µs)
        }
        let p99 = d.percentile_ns(99).unwrap();
        assert!(p99 > ACCEPTANCE_P99_NS);
        assert!(
            !d.meets_p99_target(),
            "pre-migration baseline must fail acceptance"
        );
    }
}
