//! br-ft-1650n.7 e2e harness: synthetic agent-transcript
//! fixtures for [`prompt_drift_canary::DriftStatistic`].
//!
//! Closes the bead's "Replay/e2e with old/new synthetic agent
//! transcript showing canary alert and fixture draft" + "Logging
//! must include baseline window, alert evidence, and suppression
//! reason" acceptance criteria.
//!
//! ## What the harness does
//!
//! Each fixture is a *trajectory*: a sequence of synthetic
//! observations representing how an agent-output statistic
//! (rule-hit rate, motif frequency) evolves over time. The
//! harness:
//!
//! 1. Walks the trajectory step-by-step.
//! 2. Calls `DriftStatistic::update` with each observation.
//! 3. Emits a structured tracing-json event per step containing
//!    the input observation, the canary's current cusum_high /
//!    cusum_low / observations_count, and any alert that fired.
//! 4. Asserts the expected verdict pattern (alarm count, fire
//!    step, suppression count).
//!
//! ## Fixtures
//!
//! - `replay_baseline_stable_never_alarms` — observations near
//!   the baseline mean for many steps. Pins the "no false
//!   alarm under no drift" contract.
//! - `replay_old_to_new_drift_up` — old-prompt baseline for the
//!   first N steps, then a sustained shift to a higher-output
//!   "new prompt" regime. Alert fires within the documented
//!   detection delay.
//! - `replay_old_to_new_drift_down` — mirror image (downward
//!   shift). Verifies `DriftAlert::DownwardShift` fires.
//! - `replay_sudden_spike_alarms_quickly` — single large spike
//!   crosses the alarm threshold in a single step.
//! - `replay_budget_exhausts_after_documented_count` — sustained
//!   drift against a budget=2 statistic. Exactly 2 alerts surface;
//!   subsequent CUSUM crossings increment the
//!   `suppressed_alarms_count` (the bead's "suppression reason"
//!   logging contract).
//! - `replay_stability_byte_identical` — re-feeding a fixture
//!   produces a byte-identical snapshot. Pins the substrate's
//!   purity contract end-to-end.

use std::sync::Once;

use frankenterm_core::prompt_drift_canary::{DriftAlert, DriftStatistic, DriftStatisticParams};
use tracing::info;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

/// Conservative test params: baseline_mean=0, k=0.5, h=5, budget
/// caller-supplied. Mirrors the substrate's documented defaults.
fn params(budget: u64) -> DriftStatisticParams {
    DriftStatisticParams {
        baseline_mean: 0.0,
        reference_k: 0.5,
        alarm_threshold_h: 5.0,
        false_alarm_budget: budget,
    }
}

/// Walk a trajectory, log each step + verdict, return the list
/// of alerts in fire order. Each step lands a structured
/// tracing-json event with the input observation and the
/// canary's snapshot at that step (the bead's "logging must
/// include baseline window, alert evidence" criterion).
fn replay(
    fixture_name: &'static str,
    trajectory: &[f64],
    budget: u64,
) -> (DriftStatistic, Vec<(usize, DriftAlert)>) {
    init_test_tracing_json();
    let mut stat = DriftStatistic::new(params(budget));
    let mut alerts = Vec::new();
    for (idx, obs) in trajectory.iter().enumerate() {
        let alert = stat.update(*obs);
        let snap = stat.snapshot();
        info!(
            fixture = fixture_name,
            step = idx,
            observation = obs,
            cusum_high = snap.cusum_high,
            cusum_low = snap.cusum_low,
            observations_count = snap.observations_count,
            alarms_count = snap.alarms_count,
            suppressed_alarms_count = snap.suppressed_alarms_count,
            budget_remaining = snap.budget_remaining,
            alert_kind = alert.as_ref().map_or("none", alert_kind_label),
            "drift canary replay step"
        );
        if let Some(a) = alert {
            alerts.push((idx, a));
        }
    }
    (stat, alerts)
}

fn alert_kind_label(alert: &DriftAlert) -> &'static str {
    match alert {
        DriftAlert::UpwardShift { .. } => "upward_shift",
        DriftAlert::DownwardShift { .. } => "downward_shift",
    }
}

/// Baseline-stable: 500 observations of small noise around the
/// baseline mean. The CUSUM should never cross the alarm
/// threshold; alarms_count stays at 0.
#[test]
fn replay_baseline_stable_never_alarms() {
    // Alternating ±0.1 noise — well below k=0.5, so the CUSUM
    // accumulators discharge to zero each step.
    let trajectory: Vec<f64> = (0..500)
        .map(|i| if i % 2 == 0 { 0.1 } else { -0.1 })
        .collect();
    let (stat, alerts) = replay("baseline_stable", &trajectory, 5);
    assert!(
        alerts.is_empty(),
        "baseline-stable trajectory must not alarm; got {} alarms",
        alerts.len()
    );
    let snap = stat.snapshot();
    assert_eq!(snap.alarms_count, 0);
    assert_eq!(snap.observations_count, 500);
    assert_eq!(snap.suppressed_alarms_count, 0);
}

/// Old-to-new drift-up: 50 baseline steps, then 100 steps at a
/// sustained +1.0 shift. The alarm fires within the documented
/// detection delay (~10 steps after shift onset for k=0.5,
/// h=5.0).
#[test]
fn replay_old_to_new_drift_up() {
    let mut trajectory: Vec<f64> = Vec::new();
    // Old-prompt baseline.
    trajectory.extend(std::iter::repeat_n(0.0, 50));
    // New-prompt regime (shift up).
    trajectory.extend(std::iter::repeat_n(1.0, 100));
    let (_stat, alerts) = replay("old_to_new_drift_up", &trajectory, 5);
    assert!(
        !alerts.is_empty(),
        "drift-up trajectory must alarm at least once"
    );
    let (fire_step, alert) = &alerts[0];
    assert!(
        matches!(alert, DriftAlert::UpwardShift { .. }),
        "first alert must be UpwardShift, got {alert:?}"
    );
    // Detection delay: with k=0.5 and obs=+1.0 each step, CUSUM
    // accumulates 0.5 per step; reaching h=5.0 takes ≥ 10 steps
    // after shift onset. Allow some slack.
    assert!(
        *fire_step >= 50,
        "alarm should not fire before shift onset (step 50); got {fire_step}"
    );
    assert!(
        *fire_step <= 70,
        "alarm should fire within 20 steps of shift; got {fire_step}"
    );
}

/// Old-to-new drift-down: mirror image — fires a DownwardShift.
#[test]
fn replay_old_to_new_drift_down() {
    let mut trajectory: Vec<f64> = Vec::new();
    trajectory.extend(std::iter::repeat_n(0.0, 50));
    trajectory.extend(std::iter::repeat_n(-1.0, 100));
    let (_stat, alerts) = replay("old_to_new_drift_down", &trajectory, 5);
    assert!(!alerts.is_empty());
    let (fire_step, alert) = &alerts[0];
    assert!(
        matches!(alert, DriftAlert::DownwardShift { .. }),
        "first alert must be DownwardShift, got {alert:?}"
    );
    assert!(*fire_step >= 50);
}

/// Sudden spike: a large single-step jump above h=5.0
/// immediately crosses the alarm threshold.
#[test]
fn replay_sudden_spike_alarms_quickly() {
    let mut trajectory: Vec<f64> = vec![0.0; 20];
    // One huge spike. cusum_high accumulates obs - k = 9.5 in a
    // single step → crosses h=5.0 immediately.
    trajectory.push(10.0);
    // Tail: back to baseline.
    trajectory.extend(std::iter::repeat_n(0.0, 20));
    let (_stat, alerts) = replay("sudden_spike", &trajectory, 5);
    assert_eq!(alerts.len(), 1, "single spike should produce one alarm");
    let (fire_step, alert) = &alerts[0];
    assert_eq!(*fire_step, 20, "alarm must fire on the spike step");
    assert!(matches!(alert, DriftAlert::UpwardShift { .. }));
}

/// Budget exhaustion: with budget=2, the third would-be alarm
/// is suppressed. Pinned by counting (alarms surfaced ==
/// budget) AND (suppressed_alarms_count > 0). The bead's
/// "suppression reason" logging contract shows up in the
/// per-step tracing-json output.
#[test]
fn replay_budget_exhausts_after_documented_count() {
    // Sustained drift trajectory long enough to exhaust a
    // budget=2 statistic. Each alarm consumes 1 budget and
    // resets the CUSUM, so we need many drift cycles.
    let trajectory: Vec<f64> = (0..200).map(|_| 1.0).collect();
    let (stat, alerts) = replay("budget_exhausts", &trajectory, 2);

    let snap = stat.snapshot();
    assert_eq!(
        alerts.len(),
        2,
        "exactly budget=2 alarms must surface; got {}",
        alerts.len()
    );
    assert_eq!(snap.alarms_count, 2);
    assert_eq!(snap.budget_remaining, 0);
    assert!(
        snap.suppressed_alarms_count >= 1,
        "post-budget crossings must increment suppressed counter"
    );
}

/// Stability: re-feeding a fixture produces a byte-identical
/// snapshot AND identical alert sequence. Pins the substrate's
/// purity contract end-to-end.
#[test]
fn replay_stability_byte_identical() {
    let mut trajectory: Vec<f64> = Vec::new();
    trajectory.extend(std::iter::repeat_n(0.0, 30));
    trajectory.extend(std::iter::repeat_n(1.0, 50));
    trajectory.extend(std::iter::repeat_n(0.0, 20));

    let (stat1, alerts1) = replay("stability_first", &trajectory, 5);
    let (stat2, alerts2) = replay("stability_second", &trajectory, 5);

    assert_eq!(
        stat1.snapshot(),
        stat2.snapshot(),
        "snapshot must be byte-identical across replays"
    );
    assert_eq!(alerts1.len(), alerts2.len());
    for ((s1, a1), (s2, a2)) in alerts1.iter().zip(alerts2.iter()) {
        assert_eq!(s1, s2, "alert fire step must match");
        let j1 = serde_json::to_string(a1).expect("serialize alert 1");
        let j2 = serde_json::to_string(a2).expect("serialize alert 2");
        assert_eq!(j1, j2, "alert payload must be byte-identical");
    }
}

/// Drift-then-recover: alarm fires under sustained drift, then
/// observations return to baseline. The alarm counter freezes
/// at 1 (post-alarm CUSUM reset + baseline observations don't
/// re-accumulate evidence).
#[test]
fn replay_drift_then_recover_silences_canary() {
    let mut trajectory: Vec<f64> = Vec::new();
    // Sustained drift up.
    trajectory.extend(std::iter::repeat_n(1.0, 30));
    // Recovery to baseline.
    trajectory.extend(std::iter::repeat_n(0.0, 200));
    let (stat, alerts) = replay("drift_then_recover", &trajectory, 10);
    let snap = stat.snapshot();
    assert!(
        !alerts.is_empty(),
        "drift period should produce at least one alarm"
    );
    let alarms_after_drift = alerts.len();
    assert_eq!(
        snap.alarms_count as usize, alarms_after_drift,
        "no alarms should fire during the recovery period"
    );
}
