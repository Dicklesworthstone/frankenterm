//! Property-based tests for replay_shadow_rollout (ft-og6q6.7.5).
//!
//! Invariants tested:
//! - SR-1: RolloutStage str roundtrip
//! - SR-2: Shadow mode never blocks PRs
//! - SR-3: Full enforcement blocks failing gates
//! - SR-4: Kill switch always prevents blocking
//! - SR-5: Partial enforces G1+G2, shadows G3
//! - SR-6: from_elapsed_days monotonic staging
//! - SR-7: Flaky rate 0 runs → 0 rate
//! - SR-8: Flaky rate critical threshold triggers Critical
//! - SR-9: Flaky rate below warning → Normal
//! - SR-10: Rollback trigger: flaky > critical
//! - SR-11: Rollback trigger: duration > 2x budget
//! - SR-12: No triggers → not triggered
//! - SR-13: Drill success requires all steps pass
//! - SR-14: EnforcementDecision serde roundtrip
//! - SR-15: RolloutConfig serde roundtrip
//! - SR-16: FlakyRateMetrics serde roundtrip
//! - SR-17: RollbackTrigger serde roundtrip
//! - SR-18: DrillResult serde roundtrip
//! - SR-19: RolloutMetrics serde roundtrip
//! - SR-20: Weekly digest contains stage name

use proptest::prelude::*;
use std::collections::BTreeMap;

use frankenterm_core::replay_ci_gate::{
    GateCheck, GateId, GateReport, GateStatus, ALL_GATES,
};
use frankenterm_core::replay_shadow_rollout::{
    RolloutStage, RolloutConfig, EnforcementMode,
    evaluate_enforcement, calculate_flaky_rate, evaluate_rollback_triggers,
    evaluate_rollback_drill, weekly_digest,
    AlertLevel, RolloutMetrics, DrillResult, FlakyRateMetrics,
    RollbackTrigger, EnforcementDecision,
};

fn pass_report(gate: GateId) -> GateReport {
    GateReport::new(gate, vec![GateCheck {
        name: "ok".into(), passed: true, message: "p".into(),
        duration_ms: None, artifact_path: None,
    }], 100, "2026-01-01T00:00:00Z".into())
}

fn fail_report(gate: GateId) -> GateReport {
    GateReport::new(gate, vec![GateCheck {
        name: "bad".into(), passed: false, message: "f".into(),
        duration_ms: None, artifact_path: None,
    }], 200, "2026-01-01T00:00:00Z".into())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── SR-1: Stage str roundtrip ────────────────────────────────────────

    #[test]
    fn sr01_stage_roundtrip(idx in 0usize..3) {
        let stages = [RolloutStage::Shadow, RolloutStage::Partial, RolloutStage::Full];
        let stage = stages[idx];
        let s = stage.as_str();
        let parsed = RolloutStage::from_str_stage(s);
        prop_assert_eq!(parsed, Some(stage));
    }

    // ── SR-2: Shadow never blocks ────────────────────────────────────────

    #[test]
    fn sr02_shadow_never_blocks(idx in 0usize..3) {
        let config = RolloutConfig { stage: RolloutStage::Shadow, ..Default::default() };
        let report = fail_report(ALL_GATES[idx]);
        let decision = evaluate_enforcement(&config, &report, None);
        let is_blocked = decision.pr_blocked;
        prop_assert!(!is_blocked);
        prop_assert_eq!(decision.mode, EnforcementMode::Shadow);
    }

    // ── SR-3: Full enforcement blocks failures ───────────────────────────

    #[test]
    fn sr03_full_blocks_failures(idx in 0usize..3) {
        let config = RolloutConfig { stage: RolloutStage::Full, ..Default::default() };
        let report = fail_report(ALL_GATES[idx]);
        let decision = evaluate_enforcement(&config, &report, None);
        prop_assert!(decision.pr_blocked);
        prop_assert!(decision.enforced);
    }

    // ── SR-4: Kill switch prevents blocking ──────────────────────────────

    #[test]
    fn sr04_kill_switch(idx in 0usize..3) {
        let config = RolloutConfig {
            stage: RolloutStage::Full,
            kill_switch: true,
            ..Default::default()
        };
        let report = fail_report(ALL_GATES[idx]);
        let decision = evaluate_enforcement(&config, &report, None);
        let is_blocked = decision.pr_blocked;
        prop_assert!(!is_blocked);
        prop_assert_eq!(decision.mode, EnforcementMode::KillSwitch);
    }

    // ── SR-5: Partial enforces G1+G2, shadows G3 ────────────────────────

    #[test]
    fn sr05_partial_enforcement(idx in 0usize..3) {
        let config = RolloutConfig { stage: RolloutStage::Partial, ..Default::default() };
        let gate = ALL_GATES[idx];
        let report = fail_report(gate);
        let decision = evaluate_enforcement(&config, &report, None);
        match gate {
            GateId::Smoke | GateId::TestSuite => {
                prop_assert!(decision.pr_blocked);
                prop_assert!(decision.enforced);
            }
            GateId::Regression => {
                let is_blocked = decision.pr_blocked;
                prop_assert!(!is_blocked);
                let is_enforced = decision.enforced;
                prop_assert!(!is_enforced);
            }
        }
    }

    // ── SR-6: from_elapsed_days monotonic ────────────────────────────────

    #[test]
    fn sr06_stage_monotonic(d1 in 0u64..14, d2 in 14u64..28, d3 in 28u64..365) {
        let s1 = RolloutStage::from_elapsed_days(d1);
        let s2 = RolloutStage::from_elapsed_days(d2);
        let s3 = RolloutStage::from_elapsed_days(d3);
        prop_assert_eq!(s1, RolloutStage::Shadow);
        prop_assert_eq!(s2, RolloutStage::Partial);
        prop_assert_eq!(s3, RolloutStage::Full);
    }

    // ── SR-7: Zero runs → 0 rate ────────────────────────────────────────

    #[test]
    fn sr07_zero_runs(_dummy in 0u8..1) {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(0, 0, &config, vec![]);
        prop_assert!((metrics.flaky_rate - 0.0).abs() < f64::EPSILON);
        prop_assert_eq!(metrics.alert_level, AlertLevel::Normal);
    }

    // ── SR-8: Critical flaky threshold ───────────────────────────────────

    #[test]
    fn sr08_flaky_critical(flaky in 6u64..100) {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(100, flaky, &config, vec![]);
        prop_assert_eq!(metrics.alert_level, AlertLevel::Critical);
    }

    // ── SR-9: Below warning → Normal ─────────────────────────────────────

    #[test]
    fn sr09_below_warning(flaky in 0u64..3) {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(100, flaky, &config, vec![]);
        prop_assert_eq!(metrics.alert_level, AlertLevel::Normal);
    }

    // ── SR-10: Rollback flaky trigger ────────────────────────────────────

    #[test]
    fn sr10_rollback_flaky(flaky in 6u64..50) {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(100, flaky, &config, vec![]);
        let trigger = evaluate_rollback_triggers(&metrics, &BTreeMap::new(), &config);
        prop_assert!(trigger.triggered);
    }

    // ── SR-11: Rollback duration trigger ─────────────────────────────────

    #[test]
    fn sr11_rollback_duration(actual in 61u64..1000, budget in 1u64..30) {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(100, 0, &config, vec![]);
        let durations = BTreeMap::from([("smoke".to_string(), (actual, budget))]);
        let trigger = evaluate_rollback_triggers(&metrics, &durations, &config);
        prop_assert!(trigger.triggered);
    }

    // ── SR-12: No triggers ───────────────────────────────────────────────

    #[test]
    fn sr12_no_triggers(flaky in 0u64..3) {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(100, flaky, &config, vec![]);
        let durations = BTreeMap::from([("smoke".to_string(), (25u64, 30u64))]);
        let trigger = evaluate_rollback_triggers(&metrics, &durations, &config);
        let is_triggered = trigger.triggered;
        prop_assert!(!is_triggered);
    }

    // ── SR-13: Drill success requires all steps ──────────────────────────

    #[test]
    fn sr13_drill_partial_fail(step_idx in 0usize..3) {
        let d = step_idx == 0;
        let n = step_idx == 1;
        let r = step_idx == 2;
        let result = evaluate_rollback_drill(!d, !n, !r, 1000, "now");
        // One false step means not all pass, so success = false
        let is_success = result.success;
        prop_assert!(!is_success);
    }

    // ── SR-14: EnforcementDecision serde ─────────────────────────────────

    #[test]
    fn sr14_enforcement_serde(idx in 0usize..3) {
        let config = RolloutConfig { stage: RolloutStage::Full, ..Default::default() };
        let report = fail_report(ALL_GATES[idx]);
        let decision = evaluate_enforcement(&config, &report, None);
        let json = serde_json::to_string(&decision).unwrap();
        let restored: EnforcementDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, decision);
    }

    // ── SR-15: RolloutConfig serde ───────────────────────────────────────

    #[test]
    fn sr15_config_serde(days in 1u64..365) {
        let config = RolloutConfig {
            shadow_days: days,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: RolloutConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, config);
    }

    // ── SR-16: FlakyRateMetrics serde ────────────────────────────────────

    #[test]
    fn sr16_flaky_serde(total in 1u64..1000, flaky in 0u64..100) {
        let config = RolloutConfig::default();
        let metrics = calculate_flaky_rate(total, flaky.min(total), &config, vec![]);
        let json = serde_json::to_string(&metrics).unwrap();
        let restored: FlakyRateMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.total_runs, metrics.total_runs);
        prop_assert_eq!(restored.flaky_runs, metrics.flaky_runs);
        prop_assert_eq!(restored.alert_level, metrics.alert_level);
        // f64 loses precision through JSON serde
        prop_assert!((restored.flaky_rate - metrics.flaky_rate).abs() < 1e-10);
    }

    // ── SR-17: RollbackTrigger serde ─────────────────────────────────────

    #[test]
    fn sr17_trigger_serde(count in 0usize..5) {
        let reasons: Vec<String> = (0..count).map(|i| format!("reason {}", i)).collect();
        let trigger = RollbackTrigger {
            triggered: !reasons.is_empty(),
            reasons,
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let restored: RollbackTrigger = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, trigger);
    }

    // ── SR-18: DrillResult serde ─────────────────────────────────────────

    #[test]
    fn sr18_drill_serde(dur in 100u64..100000) {
        let result = evaluate_rollback_drill(true, true, true, dur, "2026-01-01T00:00:00Z");
        let json = serde_json::to_string(&result).unwrap();
        let restored: DrillResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, result);
    }

    // ── SR-19: RolloutMetrics serde ──────────────────────────────────────

    #[test]
    fn sr19_metrics_serde(artifacts in 0u64..10000) {
        let metrics = RolloutMetrics {
            stage: RolloutStage::Shadow,
            gate_pass_rate: BTreeMap::from([("smoke".to_string(), 0.95)]),
            gate_duration_trend: BTreeMap::new(),
            flaky_rate: 0.02,
            artifact_count: artifacts,
            days_in_stage: 7,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let restored: RolloutMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, metrics);
    }

    // ── SR-20: Weekly digest contains stage ──────────────────────────────

    #[test]
    fn sr20_digest_stage(idx in 0usize..3) {
        let stages = [RolloutStage::Shadow, RolloutStage::Partial, RolloutStage::Full];
        let stage = stages[idx];
        let metrics = RolloutMetrics {
            stage,
            gate_pass_rate: BTreeMap::new(),
            gate_duration_trend: BTreeMap::new(),
            flaky_rate: 0.0,
            artifact_count: 0,
            days_in_stage: 1,
        };
        let digest = weekly_digest(&metrics);
        prop_assert!(digest.contains(stage.as_str()));
    }
}
