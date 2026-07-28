//! M9 proof: PID fleet-memory de-escalation
//! ([`frankenterm_core::fleet_memory_controller`], gate `memory.dampening=pid`).
//!
//! The M9 gauntlet experiment replaces the fixed per-tier eviction fractions in
//! `FleetScrollbackOrchestrator::plan_eviction` with a discrete-time anti-windup
//! PID that governs the fleet-wide reclaim MAGNITUDE toward an RSS-headroom
//! setpoint. De-escalation is smoothed; escalation stays bang-bang (a monotone
//! floor at Critical/Emergency keeps reclaim ≥ legacy); and the controller fails
//! closed to the legacy fractions on a missing/non-finite/stalled RSS sample.
//!
//! This harness proves four contracts the keep-gate demands:
//!
//! 1. **Plant-ID stability certificate** — identify the nominal first-order
//!    headroom plant from its step response, then certify the closed loop with
//!    the shipped default gains has **gain margin ≥ 6 dB, phase margin ≥ 30°, and
//!    zero overshoot** (with zero steady-state error). Binds the certificate to
//!    `PidDampeningConfig::default()`, so a silent gain edit breaks the proof.
//! 2. **Golden hysteresis-mode unchanged** — the default (`Hysteresis`) path
//!    produces the exact legacy `EvictionPlan` (hand-computed) for a fixed
//!    corpus across all three pressure tiers.
//! 3. **Fail-closed equivalence** — PID mode with a `None`/`NaN`/stalled RSS
//!    sample yields byte-identical plans to the legacy path; and `Hysteresis`
//!    mode delegates exactly to `plan_eviction`.
//! 4. **Monotone floor + smooth de-escalation** — at Critical/Emergency the PID
//!    never reclaims less than legacy (property, fuzzed); at Elevated with ample
//!    headroom it reclaims strictly less (the evicted-bytes win).
//!
//! Domain: fleet-memory controller — M9 anti-windup PID de-escalation.

use frankenterm_core::fleet_memory_controller::{
    EvictionPlan, FleetPressureTier, FleetScrollbackOrchestrator, MemoryDampening,
    PaneScrollbackInfo, PidDampeningConfig, PidReclaimController,
};
use proptest::prelude::*;

// ── Minimal complex arithmetic for the frequency-response certificate ────

type C = (f64, f64);
fn cadd(a: C, b: C) -> C {
    (a.0 + b.0, a.1 + b.1)
}
fn csub(a: C, b: C) -> C {
    (a.0 - b.0, a.1 - b.1)
}
fn cmul(a: C, b: C) -> C {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}
fn cdiv(a: C, b: C) -> C {
    let d = b.0 * b.0 + b.1 * b.1;
    ((a.0 * b.0 + a.1 * b.1) / d, (a.1 * b.0 - a.0 * b.1) / d)
}
fn cabs(a: C) -> f64 {
    a.0.hypot(a.1)
}
fn cphase_deg(a: C) -> f64 {
    a.1.atan2(a.0).to_degrees()
}

// ── Test fixtures ────────────────────────────────────────────────────────

fn pane(id: u64, activity: u64, warm_bytes: usize, warm_pages: usize) -> PaneScrollbackInfo {
    PaneScrollbackInfo {
        pane_id: id,
        activity_counter: activity,
        warm_bytes,
        warm_pages,
        estimated_memory_bytes: warm_bytes,
    }
}

fn pid_cfg() -> PidDampeningConfig {
    PidDampeningConfig {
        dampening: MemoryDampening::Pid,
        ..PidDampeningConfig::default()
    }
}

/// Canonicalize a plan into a fully-comparable tuple (EvictionPlan derives no
/// `PartialEq`), capturing every observable field.
fn canon_plan(plan: &Option<EvictionPlan>) -> Option<(u8, usize, usize, Vec<(u64, usize)>)> {
    plan.as_ref().map(|p| {
        let tier_tag = match p.trigger_tier {
            FleetPressureTier::Normal => 0u8,
            FleetPressureTier::Elevated => 1,
            FleetPressureTier::Critical => 2,
            FleetPressureTier::Emergency => 3,
        };
        let targets = p
            .targets
            .iter()
            .map(|t| (t.pane_id, t.pages_to_evict))
            .collect();
        (
            tier_tag,
            p.fleet_warm_bytes_before,
            p.fleet_warm_bytes_target,
            targets,
        )
    })
}

// =============================================================================
// 1. Plant-ID stability certificate (GM ≥ 6 dB, PM ≥ 30°, no overshoot)
// =============================================================================

#[test]
fn pid_plant_identification_stability_certificate() {
    // Nominal first-order headroom plant: h[k+1] = p·h[k] + b·u[k]
    // (reclaim fraction u raises headroom h; p is the refill/leak pole). DC gain
    // b/(1-p) = 1, so the setpoint is reachable with u ∈ [0,1].
    let (p_true, b_true) = (0.7_f64, 0.3_f64);

    // --- Plant identification from a unit-step response (noiseless) ---
    let mut y = 0.0_f64;
    let mut hist = Vec::new();
    for _ in 0..6 {
        y = p_true * y + b_true * 1.0;
        hist.push(y);
    }
    let b_hat = hist[0]; // y[1] = b
    let p_hat = (hist[1] - hist[0]) / hist[0]; // (y[2]-y[1])/y[1] = p
    assert!(
        (b_hat - b_true).abs() < 1e-9 && (p_hat - p_true).abs() < 1e-9,
        "plant-ID failed: b_hat={b_hat} (true {b_true}), p_hat={p_hat} (true {p_true})"
    );

    // --- Controller under test: the SHIPPED default gains ---
    let cfg = PidDampeningConfig::default();
    let (kp, ki, kd, setpoint) = (cfg.kp, cfg.ki, cfg.kd, cfg.setpoint_headroom);

    // Open-loop L(z) = C(z)·P(z) on the identified model.
    //   C(z) = Kp + Ki·z/(z-1) + Kd·(z-1)/z   (discrete PI[D], dt=1)
    //   P(z) = b_hat/(z - p_hat)
    let l_of = |theta: f64| -> C {
        let z = (theta.cos(), theta.sin());
        let z_minus_1 = csub(z, (1.0, 0.0));
        let z_minus_p = csub(z, (p_hat, 0.0));
        let term_i = cmul((ki, 0.0), cdiv(z, z_minus_1)); // Ki·z/(z-1)
        let term_d = cmul((kd, 0.0), cdiv(z_minus_1, z)); // Kd·(z-1)/z
        let c = cadd(cadd((kp, 0.0), term_i), term_d);
        let plant = cdiv((b_hat, 0.0), z_minus_p);
        cmul(c, plant)
    };

    // Sweep θ ∈ (0, π], find gain crossover (|L|=1 → PM) and phase crossover
    // (∠L = -180° → GM). No phase crossover ⇒ infinite gain margin.
    let n = 400_000usize;
    let mut last: Option<(f64, f64)> = None;
    let mut pm: Option<f64> = None;
    let mut gm_db: Option<f64> = None;
    for i in 1..=n {
        let theta = std::f64::consts::PI * (i as f64) / (n as f64);
        let val = l_of(theta);
        let mag = cabs(val);
        let ph = cphase_deg(val);
        if let Some((lm, lp)) = last {
            // Gain crossover: |L| crosses 1 (first down-crossing).
            if pm.is_none() && (lm - 1.0) * (mag - 1.0) < 0.0 {
                let t = (1.0 - lm) / (mag - lm);
                let ph_i = lp + t * (ph - lp);
                pm = Some(180.0 + ph_i);
            }
            // Phase crossover: ∠L crosses -180° (ignore ±180 wrap jumps).
            if (lp + 180.0) * (ph + 180.0) < 0.0 && (ph - lp).abs() < 180.0 {
                let t = (-180.0 - lp) / (ph - lp);
                let mag_i = lm + t * (mag - lm);
                if mag_i > 0.0 {
                    let g = -20.0 * mag_i.log10();
                    gm_db = Some(gm_db.map_or(g, |prev| prev.min(g)));
                }
            }
        }
        last = Some((mag, ph));
    }

    let pm = pm.expect("a gain crossover must exist (|L| falls below 1 at high freq)");
    let gm_db = gm_db.unwrap_or(f64::INFINITY); // no -180° crossing ⇒ infinite GM
    assert!(
        pm >= 30.0,
        "phase margin {pm:.3}° must be ≥ 30° (gains kp={kp}, ki={ki}, kd={kd})"
    );
    assert!(
        gm_db >= 6.0,
        "gain margin {gm_db:.3} dB must be ≥ 6 dB (gains kp={kp}, ki={ki}, kd={kd})"
    );

    // --- Closed-loop step response: no overshoot + zero steady-state error ---
    // Small-signal linear model (the controller is linear in its operating
    // region for this setpoint; see anti-windup test for the saturated case).
    let mut h = 0.0_f64;
    let mut integral = 0.0_f64;
    let mut prev_e = 0.0_f64;
    let mut have_prev = false;
    let mut peak = f64::MIN;
    for _ in 0..400 {
        let e = setpoint - h;
        integral += e; // dt = 1
        let d = if have_prev { e - prev_e } else { 0.0 };
        let u = kp * e + ki * integral + kd * d;
        h = p_true * h + b_true * u;
        peak = peak.max(h);
        prev_e = e;
        have_prev = true;
    }
    assert!(
        peak <= setpoint * (1.0 + 1e-6),
        "step response overshoot: peak {peak:.9} exceeds setpoint {setpoint}"
    );
    assert!(
        (h - setpoint).abs() < 1e-6,
        "step response steady-state error: h={h:.9} != setpoint {setpoint}"
    );
}

// =============================================================================
// 2. Golden: hysteresis (default) mode is byte-identical to the legacy plan
// =============================================================================

#[test]
fn golden_hysteresis_mode_matches_legacy_fractions() {
    // Two idle panes (no prior activity → delta 0), warm 1000B/10pg & 2000B/20pg.
    let panes = [pane(1, 0, 1000, 10), pane(2, 0, 2000, 20)];

    // Expected plans hand-computed from the legacy integer ratios:
    //   Elevated  retain 3/4 → target 2250, evict 750  → pane2: 8 pages
    //   Critical  retain 1/4 → target 750,  evict 2250 → pane2: 20, pane1: 3
    //   Emergency retain 0/1 → target 0,    evict 3000 → pane2: 20, pane1: 10
    let expected: [(FleetPressureTier, (u8, usize, usize, Vec<(u64, usize)>)); 3] = [
        (FleetPressureTier::Elevated, (1, 3000, 2250, vec![(2, 8)])),
        (
            FleetPressureTier::Critical,
            (2, 3000, 750, vec![(2, 20), (1, 3)]),
        ),
        (
            FleetPressureTier::Emergency,
            (3, 3000, 0, vec![(2, 20), (1, 10)]),
        ),
    ];

    for (tier, want) in expected {
        // Legacy default path.
        let mut o = FleetScrollbackOrchestrator::new();
        let legacy = o.plan_eviction(tier, &panes);
        assert_eq!(
            canon_plan(&legacy),
            Some(want.clone()),
            "legacy plan_eviction golden mismatch at {tier:?}"
        );

        // Default (Hysteresis) damped path must equal it byte-for-byte.
        let mut o2 = FleetScrollbackOrchestrator::new();
        let mut pid = PidReclaimController::new();
        let cfg = PidDampeningConfig::default(); // dampening = Hysteresis
        let damped = o2.plan_eviction_damped(tier, &panes, Some(0.10), &mut pid, &cfg);
        assert_eq!(
            canon_plan(&damped),
            Some(want),
            "Hysteresis-mode plan_eviction_damped diverged from legacy at {tier:?}"
        );
    }

    // Normal tier yields no plan in either mode.
    let mut o = FleetScrollbackOrchestrator::new();
    assert!(o.plan_eviction(FleetPressureTier::Normal, &panes).is_none());
    let mut o2 = FleetScrollbackOrchestrator::new();
    let mut pid = PidReclaimController::new();
    assert!(
        o2.plan_eviction_damped(
            FleetPressureTier::Normal,
            &panes,
            Some(0.1),
            &mut pid,
            &pid_cfg()
        )
        .is_none()
    );
}

// =============================================================================
// 3. Fail-closed: PID mode with bad/stalled RSS == legacy
// =============================================================================

#[test]
fn pid_mode_fails_closed_to_legacy_on_stalled_sensor() {
    let panes = [pane(1, 0, 4000, 40), pane(2, 0, 9000, 30)];
    let tier = FleetPressureTier::Critical;
    let cfg = pid_cfg();

    let mut legacy_orch = FleetScrollbackOrchestrator::new();
    let legacy = canon_plan(&legacy_orch.plan_eviction(tier, &panes));

    // Feed the SAME finite headroom until the sensor is declared stalled; once
    // stalled, the damped plan must equal the legacy plan exactly.
    let mut orch = FleetScrollbackOrchestrator::new();
    let mut pid = PidReclaimController::new();
    let stalled_sample = 0.12_f64;
    let mut saw_failclosed_equal = false;
    for cycle in 0..(cfg.stall_threshold as usize + 4) {
        let damped = canon_plan(&orch.plan_eviction_damped(
            tier,
            &panes,
            Some(stalled_sample),
            &mut pid,
            &cfg,
        ));
        if cycle as u32 >= cfg.stall_threshold {
            assert_eq!(
                damped, legacy,
                "after stall_threshold the controller must fail closed to legacy"
            );
            saw_failclosed_equal = true;
        }
    }
    assert!(saw_failclosed_equal);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// `None`/`NaN` RSS samples, and Hysteresis mode, all fail closed to the
    /// byte-identical legacy plan across every non-Normal tier and pane corpus.
    #[test]
    fn pid_mode_fails_closed_equals_legacy(
        warm in prop::collection::vec((1usize..50_000, 1usize..400), 1..8),
        tier_sel in 0u8..3,
        use_nan in any::<bool>(),
    ) {
        let tier = match tier_sel {
            0 => FleetPressureTier::Elevated,
            1 => FleetPressureTier::Critical,
            _ => FleetPressureTier::Emergency,
        };
        let panes: Vec<PaneScrollbackInfo> = warm
            .iter()
            .enumerate()
            .map(|(i, (b, pg))| pane(i as u64 + 1, 0, *b, *pg))
            .collect();

        let mut legacy_orch = FleetScrollbackOrchestrator::new();
        let legacy = canon_plan(&legacy_orch.plan_eviction(tier, &panes));

        // (a) PID mode, missing/non-finite RSS → fail closed.
        let bad_sample = if use_nan { Some(f64::NAN) } else { None };
        let mut o_a = FleetScrollbackOrchestrator::new();
        let mut pid_a = PidReclaimController::new();
        let damped_a = canon_plan(&o_a.plan_eviction_damped(tier, &panes, bad_sample, &mut pid_a, &pid_cfg()));
        prop_assert_eq!(&damped_a, &legacy, "PID fail-closed must equal legacy");

        // (b) Hysteresis mode delegates exactly to plan_eviction.
        let mut o_b = FleetScrollbackOrchestrator::new();
        let mut pid_b = PidReclaimController::new();
        let cfg_hyst = PidDampeningConfig::default();
        let damped_b = canon_plan(&o_b.plan_eviction_damped(tier, &panes, Some(0.2), &mut pid_b, &cfg_hyst));
        prop_assert_eq!(&damped_b, &legacy, "Hysteresis mode must equal legacy");
    }

    /// Monotone floor: at Critical/Emergency the PID never reclaims LESS than the
    /// legacy fraction, for ANY headroom (escalation stays bang-bang).
    #[test]
    fn pid_monotone_floor_at_dangerous_tiers(
        headroom in -0.5f64..1.5,
        warm in prop::collection::vec((1usize..50_000, 1usize..400), 1..8),
        emergency in any::<bool>(),
    ) {
        let tier = if emergency { FleetPressureTier::Emergency } else { FleetPressureTier::Critical };
        let panes: Vec<PaneScrollbackInfo> = warm
            .iter()
            .enumerate()
            .map(|(i, (b, pg))| pane(i as u64 + 1, 0, *b, *pg))
            .collect();

        let mut legacy_orch = FleetScrollbackOrchestrator::new();
        let legacy = legacy_orch.plan_eviction(tier, &panes);

        let mut pid_orch = FleetScrollbackOrchestrator::new();
        let mut pid = PidReclaimController::new();
        let damped = pid_orch.plan_eviction_damped(tier, &panes, Some(headroom), &mut pid, &pid_cfg());

        // Lower retained target ⇔ MORE reclaim. The PID target must be ≤ legacy.
        let legacy_target = legacy.as_ref().map_or(0, |p| p.fleet_warm_bytes_target);
        let damped_target = damped.as_ref().map_or(0, |p| p.fleet_warm_bytes_target);
        prop_assert!(
            damped_target <= legacy_target,
            "monotone floor violated at {:?}: damped target {} > legacy target {}",
            tier, damped_target, legacy_target
        );
    }
}

// =============================================================================
// 4. Smooth de-escalation: ample headroom at Elevated reclaims strictly less
// =============================================================================

#[test]
fn pid_smooth_deescalation_reduces_eviction_at_elevated() {
    let panes = [pane(1, 0, 8000, 80), pane(2, 0, 8000, 80)];
    let tier = FleetPressureTier::Elevated;

    let mut legacy_orch = FleetScrollbackOrchestrator::new();
    let legacy = legacy_orch.plan_eviction(tier, &panes);
    let legacy_reclaim = legacy
        .as_ref()
        .map_or(0, |p| p.fleet_warm_bytes_before - p.fleet_warm_bytes_target);
    assert!(legacy_reclaim > 0, "legacy must reclaim at Elevated");

    // Headroom well ABOVE the setpoint → error ≤ 0 → PID requests ~0 reclaim.
    let mut pid_orch = FleetScrollbackOrchestrator::new();
    let mut pid = PidReclaimController::new();
    let cfg = pid_cfg();
    let damped = pid_orch.plan_eviction_damped(tier, &panes, Some(0.95), &mut pid, &cfg);
    let damped_reclaim = damped
        .as_ref()
        .map_or(0, |p| p.fleet_warm_bytes_before - p.fleet_warm_bytes_target);

    assert!(
        damped_reclaim < legacy_reclaim,
        "smooth de-escalation must reclaim strictly less than legacy at Elevated \
         when headroom is ample: damped {damped_reclaim} vs legacy {legacy_reclaim}"
    );
}

// =============================================================================
// 5. Anti-windup: the integrator stays bounded under saturation and recovers
//    promptly (no windup lag) when the error reverses.
// =============================================================================

#[test]
fn pid_anti_windup_bounds_integral_and_recovers() {
    let cfg = pid_cfg();
    let mut pid = PidReclaimController::new();

    // Drive headroom far below setpoint for a long burst: output saturates at
    // out_max. Anti-windup must keep |Ki·integral| within the output span. The
    // sample is nudged microscopically each cycle so the stall detector (which
    // fires on byte-identical repeats) never trips during this windup probe.
    let bound = (cfg.out_max - cfg.out_min) / cfg.ki.abs();
    for cycle in 0..200 {
        let h = (cycle as f64) * 1e-5; // ≈0 headroom: large positive error
        let u = pid.reclaim_fraction(Some(h), &cfg).expect("finite sample");
        assert!((cfg.out_min..=cfg.out_max).contains(&u));
        assert!(
            pid.integral().abs() <= bound + 1e-9,
            "integral {} exceeded anti-windup bound {}",
            pid.integral(),
            bound
        );
    }
    // Saturated high near the top of the range.
    let saturated = pid
        .reclaim_fraction(Some(0.002), &cfg)
        .expect("finite sample");
    assert!(saturated > 0.9, "expected near-max reclaim under deficit");

    // Error reverses (headroom now well above setpoint). With anti-windup the
    // output must de-escalate to ~0 within a few cycles — not lag for the many
    // cycles a wound-up integrator would require.
    let mut recovered = None;
    for cycle in 0..12 {
        // Nudge the sample each cycle to avoid the stall detector tripping.
        let h = 0.90 + (cycle as f64) * 1e-3;
        let u = pid.reclaim_fraction(Some(h), &cfg).expect("finite sample");
        if u <= 0.05 {
            recovered = Some(cycle);
            break;
        }
    }
    assert!(
        recovered.is_some(),
        "anti-windup recovery failed: output did not de-escalate within 12 cycles"
    );
}
