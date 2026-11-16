//! M9 nominal-model proxy and fail-closed contract: PID fleet-memory de-escalation
//! ([`frankenterm_core::fleet_memory_controller`], gate `memory.dampening=pid`).
//!
//! The M9 gauntlet experiment replaces the fixed per-tier eviction fractions in
//! `FleetScrollbackOrchestrator::plan_eviction` with a discrete-time anti-windup
//! PID that governs the fleet-wide reclaim MAGNITUDE toward an RSS-headroom
//! setpoint. De-escalation is smoothed; escalation stays bang-bang (a monotone
//! floor at Critical/Emergency keeps reclaim ≥ legacy); and the controller fails
//! closed to the legacy fractions on invalid configuration or a
//! missing/non-finite/out-of-range/stalled RSS sample.
//!
//! This harness checks four deterministic contracts the keep-gate demands:
//!
//! 1. **Nominal-model proxy** — verify the synthetic first-order headroom model
//!    is recovered from its noiseless step response, calculate every bracketed
//!    crossover plus the exact Nyquist boundary, and check the shipped default
//!    controller has **gain margin ≥ 6 dB, phase margin ≥ 30°, bounded overshoot,
//!    and bounded steady-state error** on that model. This is not empirical
//!    production-host evidence.
//! 2. **Golden hysteresis-mode unchanged** — the default (`Hysteresis`) path
//!    produces the exact legacy `EvictionPlan` (hand-computed) for a fixed
//!    corpus across all three pressure tiers.
//! 3. **Fail-closed equivalence** — PID mode with invalid configuration or a
//!    missing/non-finite/out-of-range/stalled RSS sample yields field-equivalent
//!    plans to the legacy path; `Hysteresis` delegates exactly to
//!    `plan_eviction`.
//! 4. **Requested-target floor + smooth de-escalation** — sampled
//!    Critical/Emergency inputs never request a higher retained-byte target
//!    than legacy; at Elevated, a rising-headroom trajectory decreases a
//!    non-zero PID output before reaching zero reclaim.
//!
//! Domain: fleet-memory controller — M9 anti-windup PID de-escalation.

use frankenterm_core::fleet_memory_controller::{
    EvictionPlan, EvictionTarget, FleetPressureTier, FleetScrollbackOrchestrator, MemoryDampening,
    PaneScrollbackInfo, PidDampeningConfig, PidReclaimController,
};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

// ── Minimal complex arithmetic for the frequency-response proxy ──────────

type Complex = (f64, f64);

fn complex_add(lhs: Complex, rhs: Complex) -> Complex {
    (lhs.0 + rhs.0, lhs.1 + rhs.1)
}

fn complex_subtract(lhs: Complex, rhs: Complex) -> Complex {
    (lhs.0 - rhs.0, lhs.1 - rhs.1)
}

fn complex_multiply(lhs: Complex, rhs: Complex) -> Complex {
    (
        lhs.1.mul_add(-rhs.1, lhs.0 * rhs.0),
        lhs.1.mul_add(rhs.0, lhs.0 * rhs.1),
    )
}

fn complex_divide(lhs: Complex, rhs: Complex) -> Complex {
    let denominator = rhs.1.mul_add(rhs.1, rhs.0 * rhs.0);
    (
        lhs.1.mul_add(rhs.1, lhs.0 * rhs.0) / denominator,
        lhs.0.mul_add(-rhs.1, lhs.1 * rhs.0) / denominator,
    )
}

fn complex_magnitude(value: Complex) -> f64 {
    value.0.hypot(value.1)
}

fn principal_phase_degrees(value: Complex) -> f64 {
    value.1.atan2(value.0).to_degrees()
}

fn phase_near(reference_degrees: f64, principal_degrees: f64) -> f64 {
    let turns = ((reference_degrees - principal_degrees) / 360.0).round();
    turns.mul_add(360.0, principal_degrees)
}

fn refine_bracketed_root(mut lower: f64, mut upper: f64, value_at: impl Fn(f64) -> f64) -> f64 {
    let mut lower_value = value_at(lower);
    let upper_value = value_at(upper);
    assert!(lower_value.is_finite() && upper_value.is_finite());
    if lower_value.abs() <= f64::EPSILON {
        return lower;
    }
    if upper_value.abs() <= f64::EPSILON {
        return upper;
    }
    assert_ne!(
        lower_value.is_sign_negative(),
        upper_value.is_sign_negative(),
        "root refinement requires a sign-changing bracket"
    );

    for _ in 0..96 {
        let midpoint = 0.5_f64.mul_add(upper - lower, lower);
        let midpoint_value = value_at(midpoint);
        assert!(midpoint_value.is_finite());
        if midpoint_value.abs() <= f64::EPSILON {
            return midpoint;
        }
        if midpoint_value.is_sign_negative() == lower_value.is_sign_negative() {
            lower = midpoint;
            lower_value = midpoint_value;
        } else {
            upper = midpoint;
        }
    }
    0.5_f64.mul_add(upper - lower, lower)
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

fn required_plan<'plan>(
    plan: Option<&'plan EvictionPlan>,
    label: &str,
) -> Result<&'plan EvictionPlan, TestCaseError> {
    plan.ok_or_else(|| TestCaseError::fail(format!("{label} unexpectedly produced no plan")))
}

// =============================================================================
// 1. Nominal-model proxy (GM ≥ 6 dB, PM ≥ 30°, bounded response error)
// =============================================================================

#[test]
fn pid_nominal_model_margin_and_closed_loop_proxy() {
    // This is a synthetic nominal model, not an empirical production-host
    // plant. A reclaim fraction raises headroom; the DC gain is one.
    let (plant_pole, plant_input_gain) = (0.7_f64, 0.3_f64);

    // Recover the known parameters from a noiseless unit-step response. This
    // checks the model algebra only; it does not identify a live system.
    let mut headroom_response = 0.0_f64;
    let mut step_response = Vec::new();
    for _ in 0..6 {
        headroom_response = plant_pole.mul_add(headroom_response, plant_input_gain);
        step_response.push(headroom_response);
    }
    let estimated_input_gain = step_response[0];
    let estimated_pole = (step_response[1] - step_response[0]) / step_response[0];
    assert!(
        (estimated_input_gain - plant_input_gain).abs() < 1e-9
            && (estimated_pole - plant_pole).abs() < 1e-9,
        "nominal-model recovery failed: input gain {estimated_input_gain} (expected \
         {plant_input_gain}), pole {estimated_pole} (expected {plant_pole})"
    );

    // Bind the proxy to the complete shipped default. Any intentional default
    // change must update both the design derivation and its retained margins.
    let cfg = PidDampeningConfig::default();
    assert_eq!(
        cfg,
        PidDampeningConfig {
            dampening: MemoryDampening::Hysteresis,
            setpoint_headroom: 0.25,
            kp: 49.0 / 60.0,
            ki: 0.35,
            kd: 0.0,
            out_min: 0.0,
            out_max: 1.0,
            stall_threshold: 8,
        },
        "shipped PID defaults changed without updating the nominal-model proxy"
    );
    assert!(cfg.is_valid());
    let (kp, ki, kd, setpoint) = (cfg.kp, cfg.ki, cfg.kd, cfg.setpoint_headroom);

    // Open-loop L(z) = C(z)·P(z) on the identified model.
    //   C(z) = Kp + Ki·z/(z-1) + Kd·(z-1)/z   (discrete PI[D], dt=1)
    //   P(z) = b_hat/(z - p_hat)
    let open_loop_at_point = |unit_circle_point: Complex| -> Complex {
        let point_minus_one = complex_subtract(unit_circle_point, (1.0, 0.0));
        let point_minus_pole = complex_subtract(unit_circle_point, (estimated_pole, 0.0));
        let integral_term = complex_multiply(
            (ki, 0.0),
            complex_divide(unit_circle_point, point_minus_one),
        );
        let derivative_term = complex_multiply(
            (kd, 0.0),
            complex_divide(point_minus_one, unit_circle_point),
        );
        let controller_response =
            complex_add(complex_add((kp, 0.0), integral_term), derivative_term);
        let plant_response = complex_divide((estimated_input_gain, 0.0), point_minus_pole);
        complex_multiply(controller_response, plant_response)
    };
    let open_loop_response = |theta: f64| -> Complex {
        if (std::f64::consts::PI - theta).abs() <= f64::EPSILON {
            open_loop_at_point((-1.0, 0.0))
        } else {
            open_loop_at_point((theta.cos(), theta.sin()))
        }
    };

    // Sweep θ ∈ (0, π], refine every bracketed unity-gain and negative-real-axis
    // crossover, and take the worst margin. The exact z=-1 endpoint matters:
    // principal atan2 phase cannot cross below -180°, which made the previous
    // phase-based detector unreachable and falsely reported infinite GM.
    let frequency_steps = 400_000usize;
    let theta_increment = std::f64::consts::PI / frequency_steps as f64;
    let mut previous_theta = theta_increment;
    let mut previous_response = open_loop_response(previous_theta);
    let mut previous_gain_residual = complex_magnitude(previous_response) - 1.0;
    let mut previous_unwrapped_phase = principal_phase_degrees(previous_response);
    let mut unity_crossing_margins = Vec::new();
    let mut real_axis_crossing_margins = Vec::new();

    for frequency_step in 2..=frequency_steps {
        let theta = if frequency_step == frequency_steps {
            std::f64::consts::PI
        } else {
            theta_increment * frequency_step as f64
        };
        let loop_response = open_loop_response(theta);
        let gain_residual = complex_magnitude(loop_response) - 1.0;
        let unwrapped_phase = phase_near(
            previous_unwrapped_phase,
            principal_phase_degrees(loop_response),
        );

        let gain_crossover_theta = if gain_residual.abs() <= f64::EPSILON {
            Some(theta)
        } else if gain_residual.is_sign_negative() != previous_gain_residual.is_sign_negative() {
            Some(refine_bracketed_root(
                previous_theta,
                theta,
                |candidate_theta| complex_magnitude(open_loop_response(candidate_theta)) - 1.0,
            ))
        } else {
            None
        };
        if let Some(crossover_theta) = gain_crossover_theta {
            let crossover_response = open_loop_response(crossover_theta);
            let interval_fraction = (crossover_theta - previous_theta) / (theta - previous_theta);
            let reference_phase = interval_fraction.mul_add(
                unwrapped_phase - previous_unwrapped_phase,
                previous_unwrapped_phase,
            );
            let crossover_phase =
                phase_near(reference_phase, principal_phase_degrees(crossover_response));
            unity_crossing_margins.push(180.0 + crossover_phase);
        }

        let crosses_real_axis = loop_response.1.abs() <= f64::EPSILON
            || loop_response.1.is_sign_negative() != previous_response.1.is_sign_negative();
        if crosses_real_axis {
            let crossover_theta = if loop_response.1.abs() <= f64::EPSILON {
                theta
            } else {
                refine_bracketed_root(previous_theta, theta, |candidate_theta| {
                    open_loop_response(candidate_theta).1
                })
            };
            let crossover_response = open_loop_response(crossover_theta);
            if crossover_response.0 < 0.0 {
                let crossover_magnitude = complex_magnitude(crossover_response);
                assert!(crossover_magnitude > 0.0);
                real_axis_crossing_margins.push(-20.0 * crossover_magnitude.log10());
            }
        }

        previous_theta = theta;
        previous_response = loop_response;
        previous_gain_residual = gain_residual;
        previous_unwrapped_phase = unwrapped_phase;
    }

    let phase_margin_degrees = unity_crossing_margins
        .into_iter()
        .reduce(f64::min)
        .expect("the fixed nominal design must have a unity-gain crossover");
    let gain_margin_db = real_axis_crossing_margins
        .into_iter()
        .reduce(f64::min)
        .expect("the fixed nominal design must have a finite phase crossover");

    // Independent retained values for the fixed default design. In particular,
    // L(-1) = -119/680, so GM is finite: 20·log10(680/119).
    let nyquist_response = open_loop_at_point((-1.0, 0.0));
    assert!(nyquist_response.1.abs() <= f64::EPSILON);
    assert!((nyquist_response.0 + 119.0 / 680.0).abs() < 1e-12);
    let expected_gain_margin_db = 20.0 * (680.0_f64 / 119.0).log10();
    let expected_phase_margin_degrees = 79.921_341_892_212_34_f64;
    assert!(
        (gain_margin_db - expected_gain_margin_db).abs() < 1e-9,
        "gain-margin solver drifted: {gain_margin_db:.12} dB vs expected \
         {expected_gain_margin_db:.12} dB"
    );
    assert!(
        (phase_margin_degrees - expected_phase_margin_degrees).abs() < 1e-9,
        "phase-margin solver drifted: {phase_margin_degrees:.12}° vs expected \
         {expected_phase_margin_degrees:.12}°"
    );
    assert!(
        phase_margin_degrees >= 30.0,
        "phase margin {phase_margin_degrees:.3}° must be ≥ 30° \
         (gains kp={kp}, ki={ki}, kd={kd})"
    );
    assert!(
        gain_margin_db >= 6.0,
        "gain margin {gain_margin_db:.3} dB must be ≥ 6 dB \
         (gains kp={kp}, ki={ki}, kd={kd})"
    );

    // Exercise the shipped controller directly against the nominal model. Only
    // the stall threshold is disabled so convergence to an identical synthetic
    // sample does not masquerade as a sensor fault in this model-only loop.
    let mut closed_loop_config = cfg;
    closed_loop_config.dampening = MemoryDampening::Pid;
    closed_loop_config.stall_threshold = u32::MAX;
    let mut closed_loop_controller = PidReclaimController::new();
    let mut simulated_headroom = 0.0_f64;
    let mut peak = f64::MIN;
    for _ in 0..400 {
        let control_output = closed_loop_controller
            .reclaim_fraction(Some(simulated_headroom), &closed_loop_config)
            .expect("valid nominal-model sample with disabled stall threshold");
        assert!(
            control_output > closed_loop_config.out_min
                && control_output < closed_loop_config.out_max,
            "nominal response unexpectedly left the linear operating region"
        );
        simulated_headroom =
            plant_pole.mul_add(simulated_headroom, plant_input_gain * control_output);
        peak = peak.max(simulated_headroom);
    }
    assert!(
        peak <= setpoint * (1.0 + 1e-6),
        "nominal step-response peak {peak:.9} exceeds the bounded setpoint \
         tolerance around {setpoint}"
    );
    assert!(
        (simulated_headroom - setpoint).abs() < 1e-6,
        "nominal step-response error: headroom={simulated_headroom:.9}, \
         setpoint={setpoint}"
    );
}

// =============================================================================
// 2. Golden: hysteresis (default) mode is field-equivalent to the legacy plan
// =============================================================================

#[test]
fn golden_hysteresis_mode_matches_legacy_fractions() {
    // Two idle panes (no prior activity → delta 0), warm 1000B/10pg & 2000B/20pg.
    let panes = [pane(1, 0, 1000, 10), pane(2, 0, 2000, 20)];

    // Expected plans hand-computed from the legacy integer ratios:
    //   Elevated  retain 3/4 → target 2250, evict 750  → pane2: 8 pages
    //   Critical  retain 1/4 → target 750,  evict 2250 → pane2: 20, pane1: 3
    //   Emergency retain 0/1 → target 0,    evict 3000 → pane2: 20, pane1: 10
    let expected: [(FleetPressureTier, EvictionPlan); 3] = [
        (
            FleetPressureTier::Elevated,
            EvictionPlan {
                targets: vec![EvictionTarget {
                    pane_id: 2,
                    pages_to_evict: 8,
                }],
                trigger_tier: FleetPressureTier::Elevated,
                fleet_warm_bytes_before: 3000,
                fleet_warm_bytes_target: 2250,
            },
        ),
        (
            FleetPressureTier::Critical,
            EvictionPlan {
                targets: vec![
                    EvictionTarget {
                        pane_id: 2,
                        pages_to_evict: 20,
                    },
                    EvictionTarget {
                        pane_id: 1,
                        pages_to_evict: 3,
                    },
                ],
                trigger_tier: FleetPressureTier::Critical,
                fleet_warm_bytes_before: 3000,
                fleet_warm_bytes_target: 750,
            },
        ),
        (
            FleetPressureTier::Emergency,
            EvictionPlan {
                targets: vec![
                    EvictionTarget {
                        pane_id: 2,
                        pages_to_evict: 20,
                    },
                    EvictionTarget {
                        pane_id: 1,
                        pages_to_evict: 10,
                    },
                ],
                trigger_tier: FleetPressureTier::Emergency,
                fleet_warm_bytes_before: 3000,
                fleet_warm_bytes_target: 0,
            },
        ),
    ];

    for (tier, expected_plan) in expected {
        // Legacy default path.
        let mut legacy_orchestrator = FleetScrollbackOrchestrator::new();
        let legacy = legacy_orchestrator.plan_eviction(tier, &panes);
        assert_eq!(
            legacy,
            Some(expected_plan.clone()),
            "legacy plan_eviction golden mismatch at {tier:?}"
        );

        // Default (Hysteresis) damped path must be field-equivalent.
        let mut damped_orchestrator = FleetScrollbackOrchestrator::new();
        let mut controller = PidReclaimController::new();
        let cfg = PidDampeningConfig::default(); // dampening = Hysteresis
        let damped = damped_orchestrator.plan_eviction_damped(
            tier,
            &panes,
            Some(0.10),
            &mut controller,
            &cfg,
        );
        assert_eq!(
            damped,
            Some(expected_plan),
            "Hysteresis-mode plan_eviction_damped diverged from legacy at {tier:?}"
        );
    }

    // Normal tier yields no plan in either mode.
    let mut legacy_orchestrator = FleetScrollbackOrchestrator::new();
    assert!(
        legacy_orchestrator
            .plan_eviction(FleetPressureTier::Normal, &panes)
            .is_none()
    );
    let mut damped_orchestrator = FleetScrollbackOrchestrator::new();
    let mut controller = PidReclaimController::new();
    assert!(
        damped_orchestrator
            .plan_eviction_damped(
                FleetPressureTier::Normal,
                &panes,
                Some(0.1),
                &mut controller,
                &pid_cfg(),
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
    let tier = FleetPressureTier::Elevated;
    let cfg = pid_cfg();
    let stalled_sample = 0.12_f64;

    // The controller remains live before the exact repeated-sample boundary,
    // latches failure at the boundary, and stays failed for the same sample.
    let mut direct_controller = PidReclaimController::new();
    for repeat_index in 0..cfg.stall_threshold {
        assert!(
            direct_controller
                .reclaim_fraction(Some(stalled_sample), &cfg)
                .is_some(),
            "controller failed before repeat boundary at index {repeat_index}"
        );
    }
    assert!(
        direct_controller
            .reclaim_fraction(Some(stalled_sample), &cfg)
            .is_none(),
        "controller did not fail at the exact repeat boundary"
    );
    assert!(
        direct_controller
            .reclaim_fraction(Some(stalled_sample), &cfg)
            .is_none(),
        "stalled controller did not remain latched for the same sample"
    );

    // A changed valid sample recovers cold rather than applying stale integral
    // or derivative state.
    let recovered = direct_controller
        .reclaim_fraction(Some(0.13), &cfg)
        .expect("changed valid sample must recover from a stall");
    let mut fresh_controller = PidReclaimController::new();
    let fresh = fresh_controller
        .reclaim_fraction(Some(0.13), &cfg)
        .expect("fresh valid sample");
    assert_eq!(
        recovered.to_bits(),
        fresh.to_bits(),
        "stall recovery did not restart from cold controller state"
    );

    // An intervening sensor fault breaks a consecutive-equality run.
    let interrupt_cfg = PidDampeningConfig {
        stall_threshold: 1,
        ..cfg
    };
    let mut fault_interrupted_controller = PidReclaimController::new();
    assert!(
        fault_interrupted_controller
            .reclaim_fraction(Some(stalled_sample), &interrupt_cfg)
            .is_some()
    );
    assert!(
        fault_interrupted_controller
            .reclaim_fraction(None, &interrupt_cfg)
            .is_none()
    );
    assert!(
        fault_interrupted_controller
            .reclaim_fraction(Some(stalled_sample), &interrupt_cfg)
            .is_some(),
        "missing sample did not break the consecutive-sample run"
    );

    // Exercise the orchestration wiring at Elevated, where the live PID plan is
    // known to differ from legacy before the stall boundary.
    let mut legacy_orchestrator = FleetScrollbackOrchestrator::new();
    let legacy = legacy_orchestrator.plan_eviction(tier, &panes);
    let mut damped_orchestrator = FleetScrollbackOrchestrator::new();
    let mut wired_controller = PidReclaimController::new();
    for repeat_index in 0..cfg.stall_threshold {
        let damped = damped_orchestrator.plan_eviction_damped(
            tier,
            &panes,
            Some(stalled_sample),
            &mut wired_controller,
            &cfg,
        );
        if repeat_index == 0 {
            assert_ne!(
                damped, legacy,
                "pre-stall PID plan unexpectedly matched legacy at Elevated"
            );
        }
    }
    let failed_closed = damped_orchestrator.plan_eviction_damped(
        tier,
        &panes,
        Some(stalled_sample),
        &mut wired_controller,
        &cfg,
    );
    assert_eq!(
        failed_closed, legacy,
        "stall boundary did not route to the field-equivalent legacy plan"
    );
}

#[test]
fn pid_gate_and_empty_fleet_transitions_reset_controller_state() {
    let panes = [pane(1, 0, 4000, 40), pane(2, 0, 9000, 30)];
    let pid_config = pid_cfg();
    let hysteresis_config = PidDampeningConfig::default();

    let mut gate_controller = PidReclaimController::new();
    assert!(
        gate_controller
            .reclaim_fraction(Some(0.10), &pid_config)
            .is_some()
    );
    let mut gate_orchestrator = FleetScrollbackOrchestrator::new();
    let _legacy_plan = gate_orchestrator.plan_eviction_damped(
        FleetPressureTier::Elevated,
        &panes,
        Some(0.10),
        &mut gate_controller,
        &hysteresis_config,
    );
    let gate_reenabled = gate_controller
        .reclaim_fraction(Some(0.13), &pid_config)
        .expect("re-enabled PID sample");
    let mut fresh_controller = PidReclaimController::new();
    let fresh = fresh_controller
        .reclaim_fraction(Some(0.13), &pid_config)
        .expect("fresh PID sample");
    assert_eq!(
        gate_reenabled.to_bits(),
        fresh.to_bits(),
        "PID gate transition preserved stale controller state"
    );

    let mut empty_fleet_controller = PidReclaimController::new();
    assert!(
        empty_fleet_controller
            .reclaim_fraction(Some(0.10), &pid_config)
            .is_some()
    );
    let mut empty_fleet_orchestrator = FleetScrollbackOrchestrator::new();
    assert!(
        empty_fleet_orchestrator
            .plan_eviction_damped(
                FleetPressureTier::Elevated,
                &[],
                Some(0.10),
                &mut empty_fleet_controller,
                &pid_config,
            )
            .is_none()
    );
    let after_empty_fleet = empty_fleet_controller
        .reclaim_fraction(Some(0.13), &pid_config)
        .expect("PID sample after empty fleet");
    assert_eq!(
        after_empty_fleet.to_bits(),
        fresh.to_bits(),
        "empty fleet preserved stale controller state"
    );
}

#[test]
fn pid_invalid_configuration_and_sensor_inputs_fail_closed() {
    let valid = pid_cfg();
    assert!(valid.is_valid());

    let invalid_cases = [
        (
            "NaN setpoint",
            PidDampeningConfig {
                setpoint_headroom: f64::NAN,
                ..valid
            },
        ),
        (
            "setpoint below zero",
            PidDampeningConfig {
                setpoint_headroom: -0.1,
                ..valid
            },
        ),
        (
            "setpoint above one",
            PidDampeningConfig {
                setpoint_headroom: 1.1,
                ..valid
            },
        ),
        (
            "infinite proportional gain",
            PidDampeningConfig {
                kp: f64::INFINITY,
                ..valid
            },
        ),
        (
            "negative proportional gain",
            PidDampeningConfig { kp: -0.1, ..valid },
        ),
        (
            "NaN integral gain",
            PidDampeningConfig {
                ki: f64::NAN,
                ..valid
            },
        ),
        (
            "negative integral gain",
            PidDampeningConfig { ki: -0.1, ..valid },
        ),
        (
            "infinite derivative gain",
            PidDampeningConfig {
                kd: f64::NEG_INFINITY,
                ..valid
            },
        ),
        (
            "negative derivative gain",
            PidDampeningConfig { kd: -0.1, ..valid },
        ),
        (
            "output minimum below zero",
            PidDampeningConfig {
                out_min: -0.1,
                ..valid
            },
        ),
        (
            "output maximum above one",
            PidDampeningConfig {
                out_max: 1.1,
                ..valid
            },
        ),
        (
            "reversed output bounds",
            PidDampeningConfig {
                out_min: 0.8,
                out_max: 0.2,
                ..valid
            },
        ),
        (
            "zero stall threshold",
            PidDampeningConfig {
                stall_threshold: 0,
                ..valid
            },
        ),
    ];

    for (label, invalid) in invalid_cases {
        assert!(!invalid.is_valid(), "{label} was accepted");
        let mut controller = PidReclaimController::new();
        assert!(
            controller.reclaim_fraction(Some(0.1), &invalid).is_none(),
            "{label} did not fail closed"
        );
    }

    let panes = [pane(1, 0, 4000, 40), pane(2, 0, 9000, 30)];
    let tier = FleetPressureTier::Elevated;
    let mut legacy_orchestrator = FleetScrollbackOrchestrator::new();
    let legacy = legacy_orchestrator.plan_eviction(tier, &panes);
    let invalid_for_wiring = PidDampeningConfig {
        out_min: 0.8,
        out_max: 0.2,
        ..valid
    };
    let mut damped_orchestrator = FleetScrollbackOrchestrator::new();
    let mut controller = PidReclaimController::new();
    let failed_closed = damped_orchestrator.plan_eviction_damped(
        tier,
        &panes,
        Some(0.1),
        &mut controller,
        &invalid_for_wiring,
    );
    assert_eq!(
        failed_closed, legacy,
        "invalid PID configuration did not route to the legacy plan"
    );

    for invalid_sample in [
        None,
        Some(f64::NAN),
        Some(f64::INFINITY),
        Some(f64::NEG_INFINITY),
        Some(-0.1),
        Some(1.1),
    ] {
        let mut controller = PidReclaimController::new();
        assert!(
            controller
                .reclaim_fraction(invalid_sample, &valid)
                .is_none()
        );
    }

    assert!(
        serde_json::from_str::<PidDampeningConfig>(r#"{"unknown_parameter": 1}"#).is_err(),
        "unknown PID configuration fields must be rejected"
    );

    // Finite-but-extreme gains are schema-valid, but any non-finite arithmetic
    // they produce must still fail closed rather than returning NaN or panicking.
    let extreme = PidDampeningConfig {
        kp: f64::MAX,
        ki: f64::MAX,
        kd: f64::MAX,
        ..valid
    };
    assert!(extreme.is_valid());
    let mut controller = PidReclaimController::new();
    assert!(controller.reclaim_fraction(Some(0.0), &extreme).is_some());
    assert!(controller.reclaim_fraction(Some(1.0), &extreme).is_none());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Invalid RSS samples and Hysteresis mode produce the field-equivalent
    /// legacy plan across sampled non-Normal tiers and bounded pane corpora.
    #[test]
    fn pid_mode_fails_closed_equals_legacy(
        pane_shapes in prop::collection::vec((1usize..50_000, 1usize..400), 1..8),
        tier_selector in 0u8..3,
        fault_case in 0u8..6,
    ) {
        let tier = match tier_selector {
            0 => FleetPressureTier::Elevated,
            1 => FleetPressureTier::Critical,
            _ => FleetPressureTier::Emergency,
        };
        let panes: Vec<PaneScrollbackInfo> = pane_shapes
            .iter()
            .enumerate()
            .map(|(pane_index, (warm_bytes, warm_pages))| {
                pane(pane_index as u64 + 1, 0, *warm_bytes, *warm_pages)
            })
            .collect();

        let mut legacy_orchestrator = FleetScrollbackOrchestrator::new();
        let legacy = legacy_orchestrator.plan_eviction(tier, &panes);

        // (a) PID mode, missing/non-finite/out-of-range RSS → fail closed.
        let invalid_sample = match fault_case {
            0 => None,
            1 => Some(f64::NAN),
            2 => Some(f64::INFINITY),
            3 => Some(f64::NEG_INFINITY),
            4 => Some(-0.1),
            _ => Some(1.1),
        };
        let mut pid_orchestrator = FleetScrollbackOrchestrator::new();
        let mut pid_controller = PidReclaimController::new();
        let failed_closed = pid_orchestrator.plan_eviction_damped(
            tier,
            &panes,
            invalid_sample,
            &mut pid_controller,
            &pid_cfg(),
        );
        prop_assert_eq!(
            &failed_closed,
            &legacy,
            "PID fail-closed plan must be field-equivalent to legacy"
        );

        // (b) Hysteresis mode delegates exactly to plan_eviction.
        let mut hysteresis_orchestrator = FleetScrollbackOrchestrator::new();
        let mut hysteresis_controller = PidReclaimController::new();
        let hysteresis_config = PidDampeningConfig::default();
        let hysteresis_plan = hysteresis_orchestrator.plan_eviction_damped(
            tier,
            &panes,
            Some(0.2),
            &mut hysteresis_controller,
            &hysteresis_config,
        );
        prop_assert_eq!(
            &hysteresis_plan,
            &legacy,
            "Hysteresis mode must be field-equivalent to legacy"
        );
    }

    /// Requested-target floor: for sampled valid headroom and non-empty bounded
    /// pane corpora, Critical/Emergency always produce plans and the PID retained
    /// target is no higher than the legacy target.
    #[test]
    fn pid_retained_target_floor_at_dangerous_tiers(
        headroom in prop_oneof![Just(0.0_f64), Just(1.0_f64), 0.0_f64..1.0_f64],
        pane_shapes in prop::collection::vec((1usize..50_000, 1usize..400), 1..8),
        emergency in any::<bool>(),
    ) {
        let tier = if emergency {
            FleetPressureTier::Emergency
        } else {
            FleetPressureTier::Critical
        };
        let panes: Vec<PaneScrollbackInfo> = pane_shapes
            .iter()
            .enumerate()
            .map(|(pane_index, (warm_bytes, warm_pages))| {
                pane(pane_index as u64 + 1, 0, *warm_bytes, *warm_pages)
            })
            .collect();

        let mut legacy_orchestrator = FleetScrollbackOrchestrator::new();
        let legacy = legacy_orchestrator.plan_eviction(tier, &panes);

        let mut pid_orchestrator = FleetScrollbackOrchestrator::new();
        let mut pid_controller = PidReclaimController::new();
        let damped = pid_orchestrator.plan_eviction_damped(
            tier,
            &panes,
            Some(headroom),
            &mut pid_controller,
            &pid_cfg(),
        );

        let legacy_plan = required_plan(legacy.as_ref(), "dangerous-tier legacy path")?;
        let damped_plan = required_plan(damped.as_ref(), "dangerous-tier PID path")?;
        let legacy_target = legacy_plan.fleet_warm_bytes_target;
        let damped_target = damped_plan.fleet_warm_bytes_target;
        prop_assert!(
            damped_target <= legacy_target,
            "requested-target floor violated at {:?}: damped target {} > legacy target {}",
            tier, damped_target, legacy_target
        );
    }
}

// =============================================================================
// 4. Smooth de-escalation: rising headroom reduces requested reclaim
// =============================================================================

#[test]
fn pid_smooth_deescalation_reduces_eviction_at_elevated() {
    let panes = [pane(1, 0, 8000, 80), pane(2, 0, 8000, 80)];
    let tier = FleetPressureTier::Elevated;
    let cfg = pid_cfg();

    // Exercise a single stateful controller through a rising-headroom
    // trajectory. This prevents an inert always-zero PID from satisfying the
    // orchestration comparison below.
    let mut trajectory_controller = PidReclaimController::new();
    let control_outputs: Vec<f64> = [0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40]
        .into_iter()
        .map(|headroom| {
            trajectory_controller
                .reclaim_fraction(Some(headroom), &cfg)
                .expect("finite rising-headroom sample")
        })
        .collect();
    assert!(
        control_outputs[0] > 0.0 && control_outputs[4] > 0.0,
        "trajectory never exercised non-zero PID reclaim"
    );
    assert!(
        control_outputs.windows(2).all(|pair| pair[1] < pair[0]),
        "PID output did not decrease strictly with the retained trajectory: \
         {control_outputs:?}"
    );
    assert!(
        control_outputs.last().copied().unwrap_or(1.0) <= f64::EPSILON,
        "rising-headroom trajectory did not reach zero reclaim"
    );

    let mut legacy_orchestrator = FleetScrollbackOrchestrator::new();
    let legacy = legacy_orchestrator
        .plan_eviction(tier, &panes)
        .expect("non-empty Elevated corpus must produce a legacy plan");
    let legacy_requested_reclaim = legacy.fleet_warm_bytes_before - legacy.fleet_warm_bytes_target;
    assert!(
        legacy_requested_reclaim > 0,
        "legacy must request reclaim at Elevated"
    );

    // A fresh intermediate sample must produce a non-zero request below the
    // legacy request, proving that the PID path is wired rather than inert.
    let mut intermediate_orchestrator = FleetScrollbackOrchestrator::new();
    let mut intermediate_controller = PidReclaimController::new();
    let intermediate = intermediate_orchestrator
        .plan_eviction_damped(tier, &panes, Some(0.20), &mut intermediate_controller, &cfg)
        .expect("intermediate headroom must request non-zero reclaim");
    let intermediate_requested_reclaim =
        intermediate.fleet_warm_bytes_before - intermediate.fleet_warm_bytes_target;
    assert!(
        intermediate_requested_reclaim > 0
            && intermediate_requested_reclaim < legacy_requested_reclaim,
        "intermediate PID request {intermediate_requested_reclaim} was not \
         strictly between zero and legacy {legacy_requested_reclaim}"
    );

    // Headroom well above the setpoint requests zero reclaim.
    let mut ample_orchestrator = FleetScrollbackOrchestrator::new();
    let mut ample_controller = PidReclaimController::new();
    let ample = ample_orchestrator.plan_eviction_damped(
        tier,
        &panes,
        Some(0.95),
        &mut ample_controller,
        &cfg,
    );
    let ample_requested_reclaim = ample.as_ref().map_or(0, |plan| {
        plan.fleet_warm_bytes_before - plan.fleet_warm_bytes_target
    });

    assert!(ample.is_none(), "ample headroom must request zero reclaim");
    assert_eq!(ample_requested_reclaim, 0);
    assert!(
        ample_requested_reclaim < intermediate_requested_reclaim,
        "ample-headroom request {ample_requested_reclaim} was not below the \
         intermediate request {intermediate_requested_reclaim}"
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
    // fires on bit-identical repeats) never trips during this probe.
    let bound = (cfg.out_max - cfg.out_min) / cfg.ki.abs();
    for cycle in 0..200 {
        let headroom = (cycle as f64) * 1e-5;
        let control_output = pid
            .reclaim_fraction(Some(headroom), &cfg)
            .expect("finite sample");
        assert!((cfg.out_min..=cfg.out_max).contains(&control_output));
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
        let headroom = (cycle as f64).mul_add(1e-3, 0.90);
        let control_output = pid
            .reclaim_fraction(Some(headroom), &cfg)
            .expect("finite sample");
        if control_output <= 0.05 {
            recovered = Some(cycle);
            break;
        }
    }
    assert!(
        recovered.is_some(),
        "anti-windup recovery failed: output did not de-escalate within 12 cycles"
    );
}
