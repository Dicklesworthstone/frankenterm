//! Property-based tests for replay_guardrails_gate (ft-og6q6.5.5).
//!
//! Invariants tested:
//! - GG-1: RegressionBudget serde roundtrip
//! - GG-2: RegressionBudget TOML roundtrip
//! - GG-3: GateResult serde roundtrip (Pass)
//! - GG-4: GateResult serde roundtrip (Fail)
//! - GG-5: GateResult serde roundtrip (Warn)
//! - GG-6: Empty report always passes any budget
//! - GG-7: Critical count ≤ max_critical → no critical violation
//! - GG-8: Critical count > max_critical → critical violation present
//! - GG-9: High count ≤ max_high → no high violation
//! - GG-10: High count > max_high → high violation present
//! - GG-11: Medium count ≤ max_medium → no medium violation
//! - GG-12: Medium count > max_medium → medium violation present
//! - GG-13: Valid annotation excludes matching position
//! - GG-14: Invalid annotation (empty pr_reference) doesn't exclude
//! - GG-15: Skip budget violation when percent exceeded
//! - GG-16: Time budget violation when ms exceeded
//! - GG-17: Violation excess = actual - limit for severity counts
//! - GG-18: GateResult::is_pass/is_fail consistency
//! - GG-19: ExpectedDivergenceAnnotation serde roundtrip
//! - GG-20: Low/Info divergences never cause violations

use proptest::prelude::*;

use frankenterm_core::replay_guardrails_gate::{
    EvaluationContext, ExpectedDivergenceAnnotation, GateEvaluator, GateResult, RegressionBudget,
    Violation, Warning,
};
use frankenterm_core::replay_report::{JsonDivergence, JsonReport, JsonRiskSummary};

// ── Strategies ────────────────────────────────────────────────────────────

fn arb_budget() -> impl Strategy<Value = RegressionBudget> {
    (0u32..10, 0u32..10, 0u32..20, 0.0f64..50.0, 0u64..10_000_000).prop_map(
        |(mc, mh, mm, skip, time)| RegressionBudget {
            max_critical: mc,
            max_high: mh,
            max_medium: mm,
            skip_budget_percent: skip,
            time_budget_ms: time,
        },
    )
}

fn make_div(severity: &str, rule_id: &str, position: u64) -> JsonDivergence {
    JsonDivergence {
        position,
        divergence_type: "Modified".into(),
        severity: severity.into(),
        rule_id: rule_id.into(),
        root_cause: "Unknown".into(),
        baseline_output: "o1".into(),
        candidate_output: "o2".into(),
    }
}

fn empty_report() -> JsonReport {
    JsonReport {
        replay_run_id: "test".into(),
        artifact_path: "a.replay".into(),
        override_path: String::new(),
        equivalence_level: "L2".into(),
        pass: true,
        recommendation: "Pass".into(),
        divergences: vec![],
        risk_summary: JsonRiskSummary {
            max_severity: "Info".into(),
            total_risk_score: 0,
            critical_count: 0,
            high_count: 0,
            medium_count: 0,
            low_count: 0,
            info_count: 0,
        },
        timestamp: "2026-02-24T00:00:00Z".into(),
    }
}

fn report_with_divs(divs: Vec<JsonDivergence>) -> JsonReport {
    let mut r = empty_report();
    r.divergences = divs;
    r
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── GG-1: Budget serde roundtrip ──────────────────────────────────

    #[test]
    fn gg1_budget_serde(budget in arb_budget()) {
        let json = serde_json::to_string(&budget).unwrap();
        let restored: RegressionBudget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.max_critical, budget.max_critical);
        prop_assert_eq!(restored.max_high, budget.max_high);
        prop_assert_eq!(restored.max_medium, budget.max_medium);
        prop_assert_eq!(restored.time_budget_ms, budget.time_budget_ms);
    }

    // ── GG-2: Budget TOML roundtrip ───────────────────────────────────

    #[test]
    fn gg2_budget_toml(budget in arb_budget()) {
        let toml_str = budget.to_toml().unwrap();
        let restored = RegressionBudget::from_toml(&toml_str).unwrap();
        prop_assert_eq!(restored.max_critical, budget.max_critical);
        prop_assert_eq!(restored.max_high, budget.max_high);
        prop_assert_eq!(restored.max_medium, budget.max_medium);
    }

    // ── GG-3: GateResult serde (Pass) ─────────────────────────────────

    #[test]
    fn gg3_pass_serde(_dummy in 0u8..1) {
        let r = GateResult::Pass;
        let json = serde_json::to_string(&r).unwrap();
        let restored: GateResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, r);
    }

    // ── GG-4: GateResult serde (Fail) ─────────────────────────────────

    #[test]
    fn gg4_fail_serde(n in 1usize..4) {
        let violations: Vec<Violation> = (0..n)
            .map(|i| Violation {
                budget_dimension: format!("dim_{}", i),
                limit: "0".into(),
                actual: "1".into(),
                excess: "1".into(),
                contributing_rule_ids: vec![format!("r_{}", i)],
            })
            .collect();
        let r = GateResult::Fail(violations);
        let json = serde_json::to_string(&r).unwrap();
        let restored: GateResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, r);
    }

    // ── GG-5: GateResult serde (Warn) ─────────────────────────────────

    #[test]
    fn gg5_warn_serde(n in 1usize..4) {
        let warnings: Vec<Warning> = (0..n)
            .map(|i| Warning {
                message: format!("warn_{}", i),
                budget_dimension: format!("dim_{}", i),
                usage_percent: 80.0 + i as f64,
            })
            .collect();
        let r = GateResult::Warn(warnings);
        let json = serde_json::to_string(&r).unwrap();
        let restored: GateResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, r);
    }

    // ── GG-6: Empty report always passes ──────────────────────────────

    #[test]
    fn gg6_empty_passes(budget in arb_budget()) {
        let eval = GateEvaluator::new(budget);
        let result = eval.evaluate_simple(&empty_report());
        prop_assert!(result.is_pass());
    }

    // ── GG-7: Critical ≤ max → no critical violation ──────────────────

    #[test]
    fn gg7_critical_within(max_crit in 1u32..5, n_crit in 0u32..5) {
        prop_assume!(n_crit <= max_crit);
        let budget = RegressionBudget {
            max_critical: max_crit,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        let divs: Vec<_> = (0..n_crit)
            .map(|i| make_div("Critical", &format!("r_{}", i), i as u64))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        let has_critical_violation = result.violations().iter().any(|v| v.budget_dimension == "max_critical");
        prop_assert!(!has_critical_violation);
    }

    // ── GG-8: Critical > max → critical violation ─────────────────────

    #[test]
    fn gg8_critical_exceeds(max_crit in 0u32..3, extra in 1u32..4) {
        let n_crit = max_crit + extra;
        let budget = RegressionBudget {
            max_critical: max_crit,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        let divs: Vec<_> = (0..n_crit)
            .map(|i| make_div("Critical", &format!("r_{}", i), i as u64))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        prop_assert!(result.is_fail());
        let has_critical = result.violations().iter().any(|v| v.budget_dimension == "max_critical");
        prop_assert!(has_critical);
    }

    // ── GG-9: High ≤ max → no high violation ──────────────────────────

    #[test]
    fn gg9_high_within(max_high in 1u32..5, n_high in 0u32..5) {
        prop_assume!(n_high <= max_high);
        let budget = RegressionBudget {
            max_high,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        let divs: Vec<_> = (0..n_high)
            .map(|i| make_div("High", &format!("r_{}", i), i as u64))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        let has_high_violation = result.violations().iter().any(|v| v.budget_dimension == "max_high");
        prop_assert!(!has_high_violation);
    }

    // ── GG-10: High > max → high violation ────────────────────────────

    #[test]
    fn gg10_high_exceeds(max_high in 0u32..3, extra in 1u32..4) {
        let n_high = max_high + extra;
        let budget = RegressionBudget {
            max_high,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        let divs: Vec<_> = (0..n_high)
            .map(|i| make_div("High", &format!("r_{}", i), i as u64))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        prop_assert!(result.is_fail());
        let has_high = result.violations().iter().any(|v| v.budget_dimension == "max_high");
        prop_assert!(has_high);
    }

    // ── GG-11: Medium ≤ max → no medium violation ─────────────────────

    #[test]
    fn gg11_medium_within(max_med in 1u32..10, n_med in 0u32..10) {
        prop_assume!(n_med <= max_med);
        let budget = RegressionBudget {
            max_medium: max_med,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        let divs: Vec<_> = (0..n_med)
            .map(|i| make_div("Medium", &format!("r_{}", i), i as u64))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        let has_med_violation = result.violations().iter().any(|v| v.budget_dimension == "max_medium");
        prop_assert!(!has_med_violation);
    }

    // ── GG-12: Medium > max → medium violation ────────────────────────

    #[test]
    fn gg12_medium_exceeds(max_med in 0u32..5, extra in 1u32..4) {
        let n_med = max_med + extra;
        let budget = RegressionBudget {
            max_medium: max_med,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        let divs: Vec<_> = (0..n_med)
            .map(|i| make_div("Medium", &format!("r_{}", i), i as u64))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        prop_assert!(result.is_fail());
        let has_med = result.violations().iter().any(|v| v.budget_dimension == "max_medium");
        prop_assert!(has_med);
    }

    // ── GG-13: Valid annotation excludes ──────────────────────────────

    #[test]
    fn gg13_annotation_excludes(n_crit in 1u32..4) {
        let eval = GateEvaluator::with_defaults();
        let divs: Vec<_> = (0..n_crit)
            .map(|i| make_div("Critical", &format!("r_{}", i), i as u64))
            .collect();
        let report = report_with_divs(divs);
        // Annotate all positions.
        let annotations: Vec<_> = (0..n_crit)
            .map(|i| ExpectedDivergenceAnnotation {
                position: i as u64,
                reason: "intentional".into(),
                pr_reference: format!("PR-{}", i),
                definition_change_hash: "hash".into(),
            })
            .collect();
        let ctx = EvaluationContext {
            annotations,
            ..Default::default()
        };
        let result = eval.evaluate(&report, &ctx);
        prop_assert!(result.is_pass());
    }

    // ── GG-14: Invalid annotation doesn't exclude ─────────────────────

    #[test]
    fn gg14_invalid_annotation(n_crit in 1u32..3) {
        let eval = GateEvaluator::with_defaults();
        let divs: Vec<_> = (0..n_crit)
            .map(|i| make_div("Critical", &format!("r_{}", i), i as u64))
            .collect();
        let report = report_with_divs(divs);
        // Annotations with empty pr_reference.
        let annotations: Vec<_> = (0..n_crit)
            .map(|i| ExpectedDivergenceAnnotation {
                position: i as u64,
                reason: "reason".into(),
                pr_reference: String::new(), // invalid!
                definition_change_hash: String::new(),
            })
            .collect();
        let ctx = EvaluationContext {
            annotations,
            ..Default::default()
        };
        let result = eval.evaluate(&report, &ctx);
        prop_assert!(result.is_fail());
    }

    // ── GG-15: Skip budget violation ──────────────────────────────────

    #[test]
    fn gg15_skip_violation(total in 10u64..100, skip_pct in 11.0f64..50.0) {
        let skipped = ((total as f64 * skip_pct / 100.0).ceil()) as u64;
        prop_assume!(skipped > 0);
        let eval = GateEvaluator::with_defaults(); // 10% default
        let ctx = EvaluationContext {
            total_artifacts: total,
            skipped_artifacts: skipped,
            ..Default::default()
        };
        let result = eval.evaluate(&empty_report(), &ctx);
        let has_skip = result.violations().iter().any(|v| v.budget_dimension == "skip_budget_percent");
        prop_assert!(has_skip);
    }

    // ── GG-16: Time budget violation ──────────────────────────────────

    #[test]
    fn gg16_time_violation(budget_ms in 1000u64..2_000_000, extra in 1u64..100_000) {
        let budget = RegressionBudget {
            time_budget_ms: budget_ms,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        let ctx = EvaluationContext {
            replay_duration_ms: budget_ms + extra,
            ..Default::default()
        };
        let result = eval.evaluate(&empty_report(), &ctx);
        let has_time = result.violations().iter().any(|v| v.budget_dimension == "time_budget_ms");
        prop_assert!(has_time);
    }

    // ── GG-17: Excess = actual - limit ────────────────────────────────

    #[test]
    fn gg17_excess_correct(max_crit in 0u32..3, extra in 1u32..5) {
        let n_crit = max_crit + extra;
        let budget = RegressionBudget {
            max_critical: max_crit,
            ..Default::default()
        };
        let eval = GateEvaluator::new(budget);
        let divs: Vec<_> = (0..n_crit)
            .map(|i| make_div("Critical", &format!("r_{}", i), i as u64))
            .collect();
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        let violation = result.violations().iter().find(|v| v.budget_dimension == "max_critical").unwrap();
        let actual: u32 = violation.actual.parse().unwrap();
        let limit: u32 = violation.limit.parse().unwrap();
        let excess: u32 = violation.excess.parse().unwrap();
        prop_assert_eq!(excess, actual - limit);
    }

    // ── GG-18: is_pass/is_fail consistency ────────────────────────────

    #[test]
    fn gg18_pass_fail_exclusive(
        budget in arb_budget(),
        n_crit in 0u32..3,
        n_high in 0u32..3,
    ) {
        let eval = GateEvaluator::new(budget);
        let mut divs = Vec::new();
        for i in 0..n_crit {
            divs.push(make_div("Critical", &format!("cr_{}", i), i as u64));
        }
        for i in 0..n_high {
            divs.push(make_div("High", &format!("hi_{}", i), (n_crit + i) as u64));
        }
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        // is_pass and is_fail should be mutually exclusive.
        prop_assert!(result.is_pass() != result.is_fail());
    }

    // ── GG-19: Annotation serde roundtrip ─────────────────────────────

    #[test]
    fn gg19_annotation_serde(pos in 0u64..1000, reason in "[a-z]{3,10}", pr in "PR-[0-9]{1,4}") {
        let ann = ExpectedDivergenceAnnotation {
            position: pos,
            reason: reason.clone(),
            pr_reference: pr.clone(),
            definition_change_hash: "hash".into(),
        };
        let json = serde_json::to_string(&ann).unwrap();
        let restored: ExpectedDivergenceAnnotation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.position, pos);
        prop_assert_eq!(&restored.reason, &reason);
        prop_assert_eq!(&restored.pr_reference, &pr);
    }

    // ── GG-20: Low/Info never cause violations ────────────────────────

    #[test]
    fn gg20_low_info_safe(n_low in 0u32..10, n_info in 0u32..10) {
        let budget = RegressionBudget::default();
        let eval = GateEvaluator::new(budget);
        let mut divs = Vec::new();
        for i in 0..n_low {
            divs.push(make_div("Low", &format!("lo_{}", i), i as u64));
        }
        for i in 0..n_info {
            divs.push(make_div("Info", &format!("inf_{}", i), (n_low + i) as u64));
        }
        let report = report_with_divs(divs);
        let result = eval.evaluate_simple(&report);
        prop_assert!(result.is_pass());
    }
}
