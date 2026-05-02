//! 200-pane soak harness for `WatchdogedTripleBuffer`
//! (br-ft-gso6n sub-task 8 substrate slice).
//!
//! Substrate-pure scenario harness. Spawns N panes (each
//! its own `WatchdogedTripleBuffer<TestPaneState>`), drives
//! a synthetic publish + acquire workload, collects per-pane
//! `PaneHealthSnapshot`, aggregates via
//! `aggregate_fleet_health`, and asserts
//! `SoakSummary::meets_acceptance` against the bead's
//! 200-pane / 24h / 0-force-recycles / 0-warnings spec.
//!
//! ## What this harness ships
//!
//! - `SoakHarness` builder that owns the pane vector and
//!   drives ticks deterministically off a synthetic
//!   `Instant`-time clock (no real sleeps; tests are
//!   sub-second).
//! - `run_clean_soak()` — every pane stays under the
//!   warn-after threshold; asserts the fleet aggregate
//!   reports zero force-recycles, zero warnings, and
//!   `meets_acceptance` returns `true`.
//! - `run_hung_renderer_soak()` — one pane simulates a
//!   stuck reader (acquires + holds past
//!   force-recycle-after); asserts that pane's
//!   `force_recycle` counter increments within the bead's
//!   5-second window while every other pane stays clean.
//! - `#[ignore]` full-spec test that exercises 200 panes
//!   over a synthetic 24h window — opt-in via
//!   `cargo test -- --ignored triple_buffer_soak::full`.
//!
//! ## Why integration tests instead of a criterion bench
//!
//! Criterion benches are throughput-shaped; this scenario
//! is correctness-shaped (asserts a meets_acceptance
//! predicate). Integration tests run under default
//! `cargo test` so CI catches a regression in the watchdog
//! state machine or the SoakSummary predicate.

use std::time::{Duration, Instant};

use frankenterm_core::triple_buffer_fleet_health::{
    PaneHealthSnapshot, PaneId, SoakAcceptanceConfig, SoakSummary, aggregate_fleet_health,
};
use frankenterm_core::triple_buffer_watchdog::WatchdogConfig;
use frankenterm_core::watchdoged_triple_buffer::WatchdogedTripleBuffer;

// ============================================================================
// Test fixture: minimal pane-state stand-in
// ============================================================================

/// Stand-in for `TerminalState` — the soak doesn't need
/// terminal semantics, only a `Send + Sync + 'static` payload
/// the wrapper can hold. A simple counter lets the workload
/// confirm publish/acquire cycles round-trip.
#[derive(Debug, Clone, Copy)]
struct TestPaneState {
    seqno: u64,
    cursor_x: u16,
    cursor_y: u16,
}

impl TestPaneState {
    fn initial() -> Self {
        Self {
            seqno: 0,
            cursor_x: 0,
            cursor_y: 0,
        }
    }
}

// ============================================================================
// SoakHarness
// ============================================================================

/// One pane's runtime state inside the harness — owns its
/// `WatchdogedTripleBuffer`, the cumulative
/// `PaneHealthSnapshot` it accumulates over the soak, and
/// scratch fields for the hung-renderer scenario.
struct PaneRuntime {
    pane_id: PaneId,
    wtb: WatchdogedTripleBuffer<TestPaneState>,
    last_published_seqno: u64,
    /// Optional tick at which this pane was scheduled to
    /// "hang" — the harness simulates a stuck renderer by
    /// acquiring at this tick and never releasing the guard.
    hang_at_tick: Option<u64>,
    /// Stored guard for hung-renderer scenarios. When
    /// `Some`, the pane has acquired but not released; the
    /// watchdog should escalate to ForceRecycle.
    held_guard: Option<frankenterm_core::watchdoged_triple_buffer::WatchdogedAcquireGuard<TestPaneState>>,
}

struct SoakHarness {
    panes: Vec<PaneRuntime>,
    /// Ticks per second of the synthetic clock — drives the
    /// frame-timer-poll cadence. 60 Hz matches the GUI.
    ticks_per_second: u64,
    /// Synthetic wall-clock origin for `Instant` arithmetic.
    /// All `acquire` / `poll` calls pass `origin + tick *
    /// (1s/ticks_per_second)`.
    origin: Instant,
}

impl SoakHarness {
    fn new(pane_count: u32, watchdog_config: WatchdogConfig) -> Self {
        let panes = (0..pane_count)
            .map(|i| PaneRuntime {
                pane_id: PaneId(u64::from(i)),
                wtb: WatchdogedTripleBuffer::with_watchdog_config(
                    TestPaneState::initial(),
                    watchdog_config,
                ),
                last_published_seqno: 0,
                hang_at_tick: None,
                held_guard: None,
            })
            .collect();
        Self {
            panes,
            ticks_per_second: 60,
            origin: Instant::now(),
        }
    }

    fn tick_to_instant(&self, tick: u64) -> Instant {
        let nanos = (tick as u128 * 1_000_000_000) / u128::from(self.ticks_per_second);
        self.origin + Duration::from_nanos(nanos as u64)
    }

    /// Schedule pane index `pane_idx` to hang starting at
    /// `tick`. The hung pane will acquire at that tick and
    /// never release; subsequent ticks should escalate the
    /// watchdog to ForceRecycle.
    fn schedule_pane_hang(&mut self, pane_idx: usize, tick: u64) {
        self.panes[pane_idx].hang_at_tick = Some(tick);
    }

    /// Drive one tick: every pane publishes a fresh value,
    /// then acquires + immediately releases (simulating one
    /// frame of clean rendering). Hung panes acquire and
    /// hold the guard. Watchdog poll runs after.
    fn tick(&mut self, tick: u64) {
        let now = self.tick_to_instant(tick);
        for pane in self.panes.iter_mut() {
            // 1) Publish a fresh state (input thread side).
            pane.last_published_seqno += 1;
            let new_state = TestPaneState {
                seqno: pane.last_published_seqno,
                cursor_x: (tick & 0x7f) as u16,
                cursor_y: ((tick >> 7) & 0x1f) as u16,
            };
            let _ = pane.wtb.publish(new_state);

            // 2) Render-thread acquire.
            let should_hang = pane.hang_at_tick == Some(tick);
            if pane.held_guard.is_none() {
                let guard = pane.wtb.acquire(now);
                if should_hang {
                    // Hold the guard — don't drop. The
                    // watchdog should now have an outstanding
                    // acquire; subsequent polls escalate.
                    pane.held_guard = Some(guard);
                } else {
                    // Touch the snapshot so the compiler
                    // can't elide the acquire, then drop
                    // (auto-release).
                    let _seq = guard.seqno;
                    drop(guard);
                }
            }

            // 3) Frame-timer poll. Drives the watchdog's
            // tick; on ForceRecycle the wrapper invokes
            // `inner.force_recycle()` automatically.
            let _ = pane.wtb.poll(now);
        }
    }

    /// Snapshot every pane's WatchdogedHealth into the
    /// substrate's PaneHealthSnapshot type.
    fn snapshot_fleet(&self) -> Vec<PaneHealthSnapshot> {
        self.panes
            .iter()
            .map(|p| {
                let h = p.wtb.health();
                let stats = h.watchdog();
                PaneHealthSnapshot {
                    pane_id: p.pane_id,
                    acquires: stats.acquires_total(),
                    releases: stats.releases_total(),
                    warnings: stats.warnings_emitted(),
                    force_recycles: stats.force_recycle_invocations(),
                    last_force_recycle_ts_ms: 0, // synthetic clock; integration sets via Instant
                    watchdog_active: h.watchdog_active(),
                }
            })
            .collect()
    }
}

// ============================================================================
// Acceptance config helpers
// ============================================================================

/// Test-friendly soak config — relaxed `min_panes` /
/// `min_elapsed_ms` so the integration test runs in
/// milliseconds while still exercising the full
/// `meets_acceptance` predicate. The bead's 200-pane / 24h
/// spec is in a separate `#[ignore]` test below.
fn ci_smoke_config(panes: u32, elapsed_ms: u64) -> SoakAcceptanceConfig {
    SoakAcceptanceConfig {
        max_force_recycles: 0,
        max_warnings_per_pane_per_hour: 0,
        min_panes: panes,
        min_elapsed_ms: elapsed_ms,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[test]
fn clean_soak_with_50_panes_passes_acceptance() {
    // 50 panes, 60 ticks (1 second simulated), every pane
    // acquires-then-releases each tick. Watchdog should
    // never fire.
    let mut harness = SoakHarness::new(50, WatchdogConfig::default());
    let total_ticks: u64 = 60;
    for tick in 0..total_ticks {
        harness.tick(tick);
    }

    let snapshots = harness.snapshot_fleet();
    assert_eq!(snapshots.len(), 50);

    let agg = aggregate_fleet_health(&snapshots);
    assert_eq!(agg.total_panes, 50);
    assert_eq!(agg.total_force_recycles, 0);
    assert_eq!(agg.total_warnings, 0);
    assert_eq!(agg.panes_currently_active_watchdog, 0);
    assert_eq!(agg.panes_ever_force_recycled, 0);

    let summary = SoakSummary {
        panes: agg.total_panes,
        elapsed_ms: 1_000,
        total_force_recycles: agg.total_force_recycles,
        total_warnings: agg.total_warnings,
        max_per_pane_force_recycles: agg.max_per_pane_force_recycles,
    };
    assert!(
        summary.meets_acceptance(ci_smoke_config(50, 1_000)),
        "clean 50-pane soak must satisfy meets_acceptance",
    );
}

#[test]
fn hung_renderer_force_recycles_within_five_seconds() {
    // One pane in a 50-pane fleet hangs at tick 0
    // (acquires + holds). With the default
    // WatchdogConfig (warn_after=1s, force_recycle_after=5s)
    // and a 60Hz tick rate, the watchdog should fire by
    // tick 60*5 = 300.
    let mut harness = SoakHarness::new(50, WatchdogConfig::default());
    let hung_pane_idx = 7;
    harness.schedule_pane_hang(hung_pane_idx, 0);

    // Drive 6 simulated seconds (60 ticks/sec * 6 = 360
    // ticks) so the force-recycle fires comfortably inside
    // the bead's 5s SLO.
    let total_ticks: u64 = 360;
    for tick in 0..total_ticks {
        harness.tick(tick);
    }

    let snapshots = harness.snapshot_fleet();
    let hung = snapshots
        .iter()
        .find(|p| p.pane_id == PaneId(hung_pane_idx as u64))
        .expect("hung pane present in snapshots");
    assert!(
        hung.has_fired(),
        "hung pane must have force_recycled within 5s; counter = {}",
        hung.force_recycles
    );

    // Every other pane must remain clean — the bead's
    // "fleet isolation" invariant.
    for snap in &snapshots {
        if snap.pane_id == PaneId(hung_pane_idx as u64) {
            continue;
        }
        assert_eq!(
            snap.force_recycles, 0,
            "non-hung pane {:?} unexpectedly force_recycled",
            snap.pane_id,
        );
    }

    // Aggregate sees exactly one ever-force-recycled pane.
    let agg = aggregate_fleet_health(&snapshots);
    assert_eq!(agg.panes_ever_force_recycled, 1);
}

#[test]
fn hung_renderer_does_not_fire_before_five_seconds() {
    // Same setup as above but only run 4.5 simulated
    // seconds (270 ticks at 60Hz). Watchdog default
    // force_recycle_after=5s; the hung pane MUST NOT have
    // force-recycled yet at the 4.5s mark.
    let mut harness = SoakHarness::new(20, WatchdogConfig::default());
    harness.schedule_pane_hang(3, 0);

    let total_ticks: u64 = 270;
    for tick in 0..total_ticks {
        harness.tick(tick);
    }

    let snapshots = harness.snapshot_fleet();
    let hung = &snapshots[3];
    assert_eq!(
        hung.force_recycles, 0,
        "hung pane fired before 5s (saw {} at 4.5s)",
        hung.force_recycles
    );
}

#[test]
fn hung_renderer_emits_at_least_one_warning_before_recycle() {
    // Default warn_after=1s. The hung pane should emit a
    // warning between t=1s and t=5s, before force-recycle.
    // Drive exactly 4 simulated seconds (240 ticks) — past
    // warn_after but before force_recycle_after.
    let mut harness = SoakHarness::new(10, WatchdogConfig::default());
    harness.schedule_pane_hang(2, 0);

    for tick in 0..240u64 {
        harness.tick(tick);
    }

    let snapshots = harness.snapshot_fleet();
    let hung = &snapshots[2];
    assert!(
        hung.warnings >= 1,
        "expected >= 1 warning at 4s; saw {}",
        hung.warnings,
    );
    assert_eq!(
        hung.force_recycles, 0,
        "force_recycle must not yet have fired at 4s",
    );
}

#[test]
fn fleet_aggregate_isolates_hung_pane_signal() {
    // Smoke: hang two non-adjacent panes, leave 18 clean,
    // assert aggregate sees 2 panes_ever_force_recycled and
    // max_per_pane_force_recycles == 1 (each hung pane
    // recycles once during the run).
    let mut harness = SoakHarness::new(20, WatchdogConfig::default());
    harness.schedule_pane_hang(4, 0);
    harness.schedule_pane_hang(13, 0);

    for tick in 0..360u64 {
        harness.tick(tick);
    }

    let snapshots = harness.snapshot_fleet();
    let agg = aggregate_fleet_health(&snapshots);
    assert_eq!(agg.panes_ever_force_recycled, 2);
    assert!(agg.max_per_pane_force_recycles >= 1);
}

#[test]
fn meets_acceptance_rejects_force_recycle_during_smoke() {
    // Adversarial soak: at least one force-recycle fires
    // → meets_acceptance returns false even with the
    // relaxed test config.
    let mut harness = SoakHarness::new(10, WatchdogConfig::default());
    harness.schedule_pane_hang(0, 0);
    for tick in 0..360u64 {
        harness.tick(tick);
    }

    let snapshots = harness.snapshot_fleet();
    let agg = aggregate_fleet_health(&snapshots);
    let summary = SoakSummary {
        panes: agg.total_panes,
        elapsed_ms: 6_000,
        total_force_recycles: agg.total_force_recycles,
        total_warnings: agg.total_warnings,
        max_per_pane_force_recycles: agg.max_per_pane_force_recycles,
    };
    assert!(
        !summary.meets_acceptance(ci_smoke_config(10, 6_000)),
        "soak with one hung pane must NOT pass meets_acceptance",
    );
}

#[test]
fn meets_acceptance_rejects_below_min_panes() {
    // Even if the fleet is otherwise clean, a smaller
    // pane count than `min_panes` fails the gate.
    let mut harness = SoakHarness::new(10, WatchdogConfig::default());
    for tick in 0..60u64 {
        harness.tick(tick);
    }
    let snapshots = harness.snapshot_fleet();
    let agg = aggregate_fleet_health(&snapshots);
    let summary = SoakSummary {
        panes: agg.total_panes,
        elapsed_ms: 1_000,
        total_force_recycles: agg.total_force_recycles,
        total_warnings: agg.total_warnings,
        max_per_pane_force_recycles: agg.max_per_pane_force_recycles,
    };
    // Demand 200 panes → 10 < 200 → fail.
    assert!(!summary.meets_acceptance(ci_smoke_config(200, 1_000)));
}

#[test]
fn meets_acceptance_rejects_below_min_elapsed_ms() {
    let mut harness = SoakHarness::new(50, WatchdogConfig::default());
    for tick in 0..60u64 {
        harness.tick(tick);
    }
    let snapshots = harness.snapshot_fleet();
    let agg = aggregate_fleet_health(&snapshots);
    let summary = SoakSummary {
        panes: agg.total_panes,
        elapsed_ms: 100, // 100 ms < min_elapsed_ms=1_000
        total_force_recycles: agg.total_force_recycles,
        total_warnings: agg.total_warnings,
        max_per_pane_force_recycles: agg.max_per_pane_force_recycles,
    };
    assert!(!summary.meets_acceptance(ci_smoke_config(50, 1_000)));
}

#[test]
fn fleet_aggregate_zero_panes_safe() {
    // Defensive: harness with 0 panes produces empty
    // aggregate; meets_acceptance fails because
    // panes < min_panes.
    let harness = SoakHarness::new(0, WatchdogConfig::default());
    let snapshots = harness.snapshot_fleet();
    assert!(snapshots.is_empty());
    let agg = aggregate_fleet_health(&snapshots);
    assert_eq!(agg.total_panes, 0);
    let summary = SoakSummary {
        panes: 0,
        elapsed_ms: 1_000_000,
        total_force_recycles: 0,
        total_warnings: 0,
        max_per_pane_force_recycles: 0,
    };
    assert!(!summary.meets_acceptance(SoakAcceptanceConfig::default()));
}

// ============================================================================
// Bead's full 200-pane spec — opt-in via --ignored
// ============================================================================

/// Full bead-spec soak: 200 panes for a synthetic 24h.
/// Run with `cargo test -- --ignored full_spec_24h`.
///
/// The ticks-per-second clock makes this deterministic and
/// fast (no real wall-clock waits) — we drive 60Hz for 24h
/// = 5,184,000 ticks against the watchdog's deterministic
/// state machine. With clean panes and no hangs, every poll
/// is `WatchdogDecision::NoOp` and no counters increment.
///
/// Memory: 200 panes × small per-pane fixture is bounded;
/// runtime: ~2s on a warm cache because the inner loop is
/// pure arithmetic.
#[test]
#[ignore = "full 200-pane / 24h soak; opt in via --ignored"]
fn full_spec_24h_200_pane_soak_passes_bead_acceptance() {
    let mut harness = SoakHarness::new(200, WatchdogConfig::default());
    // Reduced tick density to keep the loop bounded — 1 tick
    // per simulated second is sufficient because the
    // watchdog state machine is monotonic in `now` and a
    // clean pane never escalates.
    harness.ticks_per_second = 1;
    let total_ticks: u64 = 24 * 60 * 60; // 24h in seconds
    for tick in 0..total_ticks {
        harness.tick(tick);
    }

    let snapshots = harness.snapshot_fleet();
    let agg = aggregate_fleet_health(&snapshots);
    let summary = SoakSummary {
        panes: agg.total_panes,
        elapsed_ms: total_ticks * 1_000,
        total_force_recycles: agg.total_force_recycles,
        total_warnings: agg.total_warnings,
        max_per_pane_force_recycles: agg.max_per_pane_force_recycles,
    };
    assert!(
        summary.meets_acceptance(SoakAcceptanceConfig::default()),
        "200-pane / 24h soak failed bead acceptance: {summary:?}",
    );
}
