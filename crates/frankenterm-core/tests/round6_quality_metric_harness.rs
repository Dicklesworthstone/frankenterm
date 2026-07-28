//! Round-6 A5 — deterministic quality-metric adjudication harness
//! (bead `ft-round5-gauntlet-lw0s7.20`).
//!
//! ## Why this exists
//!
//! Two round-5 candidates were measured on the *wrong axis* and stayed
//! un-adjudicated (see `docs/perf-ledger/round5-negative-results.md`):
//!
//! * **S3-FIFO eviction** (`cache.eviction=s3fifo`) — the round-5 criterion
//!   bench timed per-op compute (S3-FIFO is ~2× LFU there) but its *purpose* is
//!   **scan-resistant hit-rate at equal capacity**, which compute timing never
//!   captures.
//! * **M9 PID fleet-memory dampening** (`memory.dampening=pid`) — timed compute
//!   (PID −10%) but its purpose is **fewer evicted bytes and less
//!   reclaim-target oscillation** under a memory-pressure replay.
//!
//! The key property both share: the quality metric (hit-rate, evicted-bytes,
//! oscillation) is a **deterministic** function of the workload trace and the
//! policy. It needs **no quiet host** — running it once yields the exact,
//! host-independent verdict. That is what makes these candidates *adjudicable*,
//! and what the noisy compute benches could never deliver.
//!
//! This harness drives both arms of both candidates over quality-stress traces,
//! computes the exact metric per arm, renders a **verdict scorecard** (win /
//! tie / regression with margin), and prints it for the ledger. The `#[test]`
//! bodies are fail-closed proofs of the *harness* (the metric is well-formed and
//! both arms ran); the win/loss verdict itself is the emitted data the
//! orchestrator adjudicates — it is exact, so the scorecard is the final word.
//!
//! ```text
//! cargo test -p frankenterm-core --test round6_quality_metric_harness -- --nocapture
//! ```
//!
//! Extensible: Q4 (lazy-captures suppression-rate) and M2 (succinct-attr RSS)
//! adjudicate via the same deterministic-scorecard shape — add a trace + an
//! arm pair and emit through [`Scorecard`].

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use config::ConfigHandle;
use lfucache::{CacheEvictionPolicy, LfuCacheU64};

use frankenterm_core::fleet_memory_controller::{
    EvictionPlan, FleetPressureTier, FleetScrollbackOrchestrator, MemoryDampening,
    PaneScrollbackInfo, PidDampeningConfig, PidReclaimController,
};

/// Relative margin below which a delta is "tie" (not a meaningful win/loss).
const MEANINGFUL_MARGIN: f64 = 0.01;

/// A single adjudicated quality comparison: baseline arm vs candidate arm on
/// one deterministic metric.
struct Scorecard {
    candidate: &'static str,
    metric: &'static str,
    higher_is_better: bool,
    baseline_arm: &'static str,
    baseline_value: f64,
    candidate_arm: &'static str,
    candidate_value: f64,
}

impl Scorecard {
    fn rel_delta(&self) -> f64 {
        if self.baseline_value == 0.0 {
            if self.candidate_value == 0.0 {
                0.0
            } else {
                f64::INFINITY
            }
        } else {
            (self.candidate_value - self.baseline_value) / self.baseline_value
        }
    }

    fn verdict(&self) -> &'static str {
        let rel = self.rel_delta();
        if !rel.is_finite() || rel.abs() < MEANINGFUL_MARGIN {
            return "TIE";
        }
        let improved = if self.higher_is_better {
            self.candidate_value > self.baseline_value
        } else {
            self.candidate_value < self.baseline_value
        };
        if improved { "WIN" } else { "REGRESSION" }
    }

    fn print(&self) {
        println!(
            "{:<22} {:<26} {:<12}={:>14.4}  {:<12}={:>14.4}  rel={:>+8.2}%  -> {}",
            self.candidate,
            self.metric,
            self.baseline_arm,
            self.baseline_value,
            self.candidate_arm,
            self.candidate_value,
            self.rel_delta() * 100.0,
            self.verdict(),
        );
    }

    fn to_json(&self) -> String {
        format!(
            "{{\"candidate\":\"{}\",\"metric\":\"{}\",\"higher_is_better\":{},\
             \"baseline_arm\":\"{}\",\"baseline\":{:.4},\"candidate_arm\":\"{}\",\
             \"candidate\":{:.4},\"rel_delta\":{:.6},\"verdict\":\"{}\"}}",
            self.candidate,
            self.metric,
            self.higher_is_better,
            self.baseline_arm,
            self.baseline_value,
            self.candidate_arm,
            self.candidate_value,
            self.rel_delta(),
            self.verdict(),
        )
    }
}

// ── S3-FIFO scan-heavy hit-rate ─────────────────────────────────────────────

const CACHE_CAPACITY: usize = 128;
const HOT_KEYS: u64 = 32;
const SCAN_KEYS_PER_ROUND: u64 = 384;
const SCAN_ROUNDS: u64 = 24;
const PHASE_SET: u64 = 96;
const PHASE_ACCESSES: u64 = 600;
const PHASES: u64 = 6;

#[derive(Clone, Copy)]
struct HitRate {
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl HitRate {
    fn rate(self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }
}

fn fixed_capacity(_: &ConfigHandle) -> usize {
    CACHE_CAPACITY
}

/// One-hit-wonder scan trace: a stable hot working set repeatedly hit, flooded
/// each round by a fresh batch of single-use scan keys that would evict the hot
/// set under a recency-only policy. Classic scan-resistance stress.
fn scan_resistance_trace() -> Vec<u64> {
    let mut trace = Vec::new();
    for _ in 0..8 {
        trace.extend(0..HOT_KEYS);
    }
    for round in 0..SCAN_ROUNDS {
        trace.extend(0..HOT_KEYS);
        let scan_start = 1_000_000 + round * SCAN_KEYS_PER_ROUND;
        trace.extend(scan_start..scan_start + SCAN_KEYS_PER_ROUND);
        trace.extend(0..HOT_KEYS);
    }
    trace
}

/// Phase-shift trace: the hot set migrates between disjoint regions over time.
/// LFU clings to historically-frequent-but-now-stale keys (frequency
/// pollution); a scan-resistant/recency-aware policy adapts. Separates S3-FIFO
/// from pure LFU where the one-hit-wonder trace may not.
fn phase_shift_trace() -> Vec<u64> {
    let mut trace = Vec::new();
    for phase in 0..PHASES {
        let base = phase * 10_000;
        for i in 0..PHASE_ACCESSES {
            trace.push(base + (i % PHASE_SET));
        }
    }
    trace
}

fn run_cache(policy: CacheEvictionPolicy, trace: &[u64]) -> HitRate {
    let config = ConfigHandle::default_config();
    let mut cache = LfuCacheU64::new_with_eviction_policy(
        "round6_quality_hit",
        "round6_quality_miss",
        fixed_capacity,
        &config,
        policy,
    );
    let mut hr = HitRate {
        hits: 0,
        misses: 0,
        evictions: 0,
    };
    for &key in trace {
        if cache.get(&key).is_some() {
            hr.hits += 1;
        } else {
            hr.misses += 1;
            hr.evictions +=
                u64::try_from(cache.put_capturing_evictions(key, key).len()).unwrap_or(u64::MAX);
        }
    }
    hr
}

// ── M9 PID dampening: evicted-bytes + reclaim oscillation ───────────────────

const PANE_COUNT: usize = 192;

/// A sawtooth/varied memory-pressure replay long enough to expose oscillation:
/// repeated elevated cycles with rising-then-falling headroom plus critical
/// spikes. A band-hysteresis controller flip-flops the reclaim target near the
/// band edges; PID damps it. Both quantities below are deterministic.
fn pressure_replay() -> Vec<(FleetPressureTier, Option<f64>)> {
    let mut cycles = Vec::new();
    for cycle in 0..4 {
        // rising headroom (pressure easing)
        for step in 0..6 {
            let headroom = 0.06 + f64::from(step) * 0.04;
            cycles.push((FleetPressureTier::Elevated, Some(headroom)));
        }
        // critical spike
        cycles.push((
            FleetPressureTier::Critical,
            Some(0.05 + f64::from(cycle) * 0.01),
        ));
        // falling headroom (pressure building) — the flip side of the sawtooth
        for step in (0..6).rev() {
            let headroom = 0.06 + f64::from(step) * 0.04;
            cycles.push((FleetPressureTier::Elevated, Some(headroom)));
        }
    }
    cycles
}

struct ReplayMetric {
    total_evicted_bytes: usize,
    direction_changes: usize,
}

fn build_panes() -> Vec<PaneScrollbackInfo> {
    (0..PANE_COUNT)
        .map(|pane| {
            let warm_pages = 32 + (pane % 48);
            let bytes_per_page = 10_240 + (pane % 7) * 2_048;
            let warm_bytes = warm_pages * bytes_per_page;
            PaneScrollbackInfo {
                pane_id: pane as u64,
                activity_counter: u64::from(pane % 5 == 0),
                warm_bytes,
                warm_pages,
                estimated_memory_bytes: warm_bytes + 128 * 256,
            }
        })
        .collect()
}

fn apply_eviction_plan(panes: &mut [PaneScrollbackInfo], plan: &EvictionPlan) -> usize {
    let mut evicted = 0usize;
    for target in &plan.targets {
        if let Some(pane) = panes.iter_mut().find(|p| p.pane_id == target.pane_id) {
            if pane.warm_pages == 0 || pane.warm_bytes == 0 {
                continue;
            }
            let pages = target.pages_to_evict.min(pane.warm_pages);
            let bytes = pane
                .warm_bytes
                .saturating_mul(pages)
                .checked_div(pane.warm_pages)
                .unwrap_or(0)
                .min(pane.warm_bytes);
            pane.warm_pages = pane.warm_pages.saturating_sub(pages);
            pane.warm_bytes = pane.warm_bytes.saturating_sub(bytes);
            pane.estimated_memory_bytes = pane.estimated_memory_bytes.saturating_sub(bytes);
            evicted = evicted.saturating_add(bytes);
        }
    }
    evicted
}

fn run_replay(dampening: MemoryDampening) -> ReplayMetric {
    let mut panes = build_panes();
    let mut orchestrator = FleetScrollbackOrchestrator::new();
    let mut pid = PidReclaimController::new();
    let cfg = PidDampeningConfig {
        dampening,
        ..PidDampeningConfig::default()
    };
    let mut total_evicted_bytes = 0usize;
    let mut previous_target: Option<usize> = None;
    let mut previous_direction = 0isize;
    let mut direction_changes = 0usize;

    for (tier, headroom) in pressure_replay() {
        if let Some(plan) =
            orchestrator.plan_eviction_damped(tier, &panes, headroom, &mut pid, &cfg)
        {
            if let Some(prev) = previous_target {
                let direction = match plan.fleet_warm_bytes_target.cmp(&prev) {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                };
                if direction != 0 && previous_direction != 0 && direction != previous_direction {
                    direction_changes += 1;
                }
                if direction != 0 {
                    previous_direction = direction;
                }
            }
            previous_target = Some(plan.fleet_warm_bytes_target);
            total_evicted_bytes =
                total_evicted_bytes.saturating_add(apply_eviction_plan(&mut panes, &plan));
        }
    }

    ReplayMetric {
        total_evicted_bytes,
        direction_changes,
    }
}

// ── Tests (fail-closed harness proofs) + scorecard emission ─────────────────

#[test]
fn s3fifo_scan_heavy_hit_rate_adjudicated() {
    let scan_lfu = run_cache(CacheEvictionPolicy::Lfu, &scan_resistance_trace());
    let scan_s3 = run_cache(CacheEvictionPolicy::S3Fifo, &scan_resistance_trace());
    let phase_lfu = run_cache(CacheEvictionPolicy::Lfu, &phase_shift_trace());
    let phase_s3 = run_cache(CacheEvictionPolicy::S3Fifo, &phase_shift_trace());

    let cards = [
        Scorecard {
            candidate: "S3-FIFO",
            metric: "hit_rate@scan_resistance",
            higher_is_better: true,
            baseline_arm: "lfu",
            baseline_value: scan_lfu.rate(),
            candidate_arm: "s3fifo",
            candidate_value: scan_s3.rate(),
        },
        Scorecard {
            candidate: "S3-FIFO",
            metric: "hit_rate@phase_shift",
            higher_is_better: true,
            baseline_arm: "lfu",
            baseline_value: phase_lfu.rate(),
            candidate_arm: "s3fifo",
            candidate_value: phase_s3.rate(),
        },
    ];

    println!("\n=== ROUND-6 A5 quality scorecard: S3-FIFO hit-rate (deterministic) ===");
    println!(
        "capacity={CACHE_CAPACITY} hot={HOT_KEYS} scan/round={SCAN_KEYS_PER_ROUND} rounds={SCAN_ROUNDS} \
         | phase_set={PHASE_SET} phases={PHASES}"
    );
    for c in &cards {
        c.print();
        println!("ROUND6_A5_JSON {}", c.to_json());
    }
    println!(
        "  scan: lfu {}h/{}m/{}ev  s3fifo {}h/{}m/{}ev",
        scan_lfu.hits,
        scan_lfu.misses,
        scan_lfu.evictions,
        scan_s3.hits,
        scan_s3.misses,
        scan_s3.evictions
    );

    // Fail-closed harness invariants (NOT the verdict — that is the emitted data).
    for c in &cards {
        assert!(
            (0.0..=1.0).contains(&c.baseline_value) && (0.0..=1.0).contains(&c.candidate_value),
            "hit-rate out of [0,1] for metric {}",
            c.metric
        );
    }
    assert!(
        scan_lfu.hits + scan_lfu.misses > 0,
        "scan trace did not run"
    );
    assert!(
        scan_s3.evictions > 0,
        "scan trace evicted nothing — capacity too large"
    );
    assert!(
        phase_lfu.hits + phase_lfu.misses > 0,
        "phase trace did not run"
    );
}

#[test]
fn m9_pid_dampening_evicted_bytes_and_oscillation_adjudicated() {
    let hyst = run_replay(MemoryDampening::Hysteresis);
    let pid = run_replay(MemoryDampening::Pid);

    let cards = [
        Scorecard {
            candidate: "M9-PID",
            metric: "evicted_bytes@pressure",
            higher_is_better: false,
            baseline_arm: "hysteresis",
            baseline_value: hyst.total_evicted_bytes as f64,
            candidate_arm: "pid",
            candidate_value: pid.total_evicted_bytes as f64,
        },
        Scorecard {
            candidate: "M9-PID",
            metric: "reclaim_oscillation",
            higher_is_better: false,
            baseline_arm: "hysteresis",
            baseline_value: hyst.direction_changes as f64,
            candidate_arm: "pid",
            candidate_value: pid.direction_changes as f64,
        },
    ];

    println!("\n=== ROUND-6 A5 quality scorecard: M9 PID dampening (deterministic) ===");
    println!(
        "panes={PANE_COUNT} replay_cycles={}",
        pressure_replay().len()
    );
    for c in &cards {
        c.print();
        println!("ROUND6_A5_JSON {}", c.to_json());
    }

    // Fail-closed harness invariants.
    assert!(
        hyst.total_evicted_bytes > 0 || pid.total_evicted_bytes > 0,
        "replay evicted nothing — pressure model is inert"
    );
}
