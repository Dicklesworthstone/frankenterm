//! Property-based tests for ARS Historical Replay Validation Harness.
//!
//! Verifies replay verdicts, pass rate calculations, incident capping,
//! inconclusive handling, and serde roundtrips.

use proptest::prelude::*;

use frankenterm_core::ars_replay::{
    FailReason, HistoricalIncident, ReplayAssessment, ReplayConfig, ReplayHarness, ReplaySession,
    ReplayStats, ReplayVerdict,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_incident(success: bool) -> impl Strategy<Value = HistoricalIncident> {
    ("[a-z]{3,8}", 1..100u64).prop_map(move |(id, pane_id)| HistoricalIncident {
        incident_id: id,
        trigger_pattern: vec![1, 2, 3],
        output_before: "error: connection refused".to_string(),
        output_after: "connected successfully".to_string(),
        actual_commands: vec!["systemctl restart app".to_string()],
        timestamp_ms: 1000,
        pane_id,
        original_success: success,
    })
}

// =============================================================================
// Verdict invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn verdict_is_pass_xor_fail_xor_inconclusive(
        score in 0.0..1.0f64,
        id in "[a-z]{3,8}",
    ) {
        let pass = ReplayVerdict::Pass {
            incident_id: id.clone(),
            match_score: score,
        };
        prop_assert!(pass.is_pass());
        prop_assert!(!pass.is_fail());

        let fail = ReplayVerdict::Fail {
            incident_id: id.clone(),
            reason: FailReason::PatternMismatch,
        };
        prop_assert!(fail.is_fail());
        prop_assert!(!fail.is_pass());

        let inc = ReplayVerdict::Inconclusive {
            incident_id: id,
            reason: "test".to_string(),
        };
        prop_assert!(!inc.is_pass());
        prop_assert!(!inc.is_fail());
    }

    #[test]
    fn verdict_preserves_incident_id(id in "[a-z]{3,10}") {
        let v = ReplayVerdict::Pass {
            incident_id: id.clone(),
            match_score: 1.0,
        };
        prop_assert_eq!(v.incident_id(), &id);
    }
}

// =============================================================================
// Harness invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn insufficient_when_below_min(
        min in 3..10usize,
        available in 1..3usize,
    ) {
        let config = ReplayConfig {
            min_incidents: min,
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents: Vec<_> = (0..available)
            .map(|i| HistoricalIncident {
                incident_id: format!("inc-{i}"),
                trigger_pattern: vec![1],
                output_before: "err".to_string(),
                output_after: "ok".to_string(),
                actual_commands: vec!["cmd".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: true,
            })
            .collect();

        let session = harness.validate(1, &["cmd".into()], &incidents, 1000);
        let is_insuf = matches!(session.assessment, ReplayAssessment::InsufficientData { .. });
        prop_assert!(is_insuf);
    }

    #[test]
    fn max_incidents_caps_verdicts(
        max in 2..10usize,
        total in 10..20usize,
    ) {
        let config = ReplayConfig {
            max_incidents: max,
            min_incidents: 1,
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents: Vec<_> = (0..total)
            .map(|i| HistoricalIncident {
                incident_id: format!("inc-{i}"),
                trigger_pattern: vec![1],
                output_before: "err".to_string(),
                output_after: "ok".to_string(),
                actual_commands: vec!["systemctl restart app".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: true,
            })
            .collect();

        let session = harness.validate(
            1,
            &["systemctl restart app".into()],
            &incidents,
            1000,
        );
        prop_assert_eq!(session.verdicts.len(), max);
    }

    #[test]
    fn matching_commands_validate(
        n_incidents in 3..10usize,
    ) {
        let mut harness = ReplayHarness::with_defaults();
        let incidents: Vec<_> = (0..n_incidents)
            .map(|i| HistoricalIncident {
                incident_id: format!("inc-{i}"),
                trigger_pattern: vec![1],
                output_before: "error: connection refused".to_string(),
                output_after: "connected successfully".to_string(),
                actual_commands: vec!["systemctl restart app".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: true,
            })
            .collect();

        let session = harness.validate(
            1,
            &["systemctl restart app".into()],
            &incidents,
            1000,
        );
        prop_assert!(session.assessment.is_validated());
    }

    #[test]
    fn mismatching_commands_reject(
        n_incidents in 3..10usize,
    ) {
        let mut harness = ReplayHarness::with_defaults();
        let incidents: Vec<_> = (0..n_incidents)
            .map(|i| HistoricalIncident {
                incident_id: format!("inc-{i}"),
                trigger_pattern: vec![1],
                output_before: "error".to_string(),
                output_after: "ok".to_string(),
                actual_commands: vec!["systemctl restart app".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: true,
            })
            .collect();

        let session = harness.validate(
            1,
            &["completely different command xyz".into()],
            &incidents,
            1000,
        );
        let is_rejected = matches!(session.assessment, ReplayAssessment::Rejected { .. });
        prop_assert!(is_rejected);
    }

    #[test]
    fn failed_originals_are_inconclusive(
        n in 3..8usize,
    ) {
        let config = ReplayConfig {
            min_incidents: 1,
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents: Vec<_> = (0..n)
            .map(|i| HistoricalIncident {
                incident_id: format!("inc-{i}"),
                trigger_pattern: vec![1],
                output_before: "err".to_string(),
                output_after: "still err".to_string(),
                actual_commands: vec!["cmd".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: false,
            })
            .collect();

        let session = harness.validate(1, &["cmd".into()], &incidents, 1000);
        for v in &session.verdicts {
            let is_inconclusive = matches!(v, ReplayVerdict::Inconclusive { .. });
            prop_assert!(is_inconclusive);
        }
    }

    #[test]
    fn stats_sessions_increment(
        n_sessions in 1..5usize,
    ) {
        let config = ReplayConfig {
            min_incidents: 1,
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents = vec![HistoricalIncident {
            incident_id: "i1".to_string(),
            trigger_pattern: vec![1],
            output_before: "err".to_string(),
            output_after: "ok".to_string(),
            actual_commands: vec!["cmd".into()],
            timestamp_ms: 1000,
            pane_id: 1,
            original_success: true,
        }];

        for _ in 0..n_sessions {
            harness.validate(1, &["cmd".into()], &incidents, 1000);
        }

        let stats = harness.stats();
        prop_assert_eq!(stats.total_sessions, n_sessions as u64);
    }
}

// =============================================================================
// Serde roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_serde_roundtrip(
        min_inc in 1..20usize,
        max_inc in 20..200usize,
        min_rate in 0.5..1.0f64,
    ) {
        let config = ReplayConfig {
            min_incidents: min_inc,
            max_incidents: max_inc,
            min_pass_rate: min_rate,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: ReplayConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.min_incidents, config.min_incidents);
        prop_assert_eq!(decoded.max_incidents, config.max_incidents);
        let diff = (decoded.min_pass_rate - config.min_pass_rate).abs();
        prop_assert!(diff < 1e-10);
    }

    #[test]
    fn incident_serde_roundtrip(incident in arb_incident(true)) {
        let json = serde_json::to_string(&incident).unwrap();
        let decoded: HistoricalIncident = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, incident);
    }

    #[test]
    fn verdict_pass_serde_roundtrip(
        id in "[a-z]{3,8}",
        score in 0.0..1.0f64,
    ) {
        let v = ReplayVerdict::Pass {
            incident_id: id,
            match_score: score,
        };
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ReplayVerdict = serde_json::from_str(&json).unwrap();
        // f64 tolerance.
        if let ReplayVerdict::Pass { match_score, .. } = decoded {
            let diff = (match_score - score).abs();
            prop_assert!(diff < 1e-10);
        } else {
            prop_assert!(false, "wrong variant");
        }
    }

    #[test]
    fn assessment_serde_roundtrip(
        pass_rate in 0.0..1.0f64,
        incidents in 1..100usize,
    ) {
        let a = ReplayAssessment::Validated {
            pass_rate,
            incidents,
        };
        let json = serde_json::to_string(&a).unwrap();
        let decoded: ReplayAssessment = serde_json::from_str(&json).unwrap();
        if let ReplayAssessment::Validated {
            pass_rate: dp,
            incidents: di,
        } = decoded
        {
            let diff = (dp - pass_rate).abs();
            prop_assert!(diff < 1e-10);
            prop_assert_eq!(di, incidents);
        } else {
            prop_assert!(false, "wrong variant");
        }
    }

    #[test]
    fn stats_serde_roundtrip(
        sessions in 0..1000u64,
        validated in 0..500u64,
        rejected in 0..500u64,
    ) {
        let stats = ReplayStats {
            total_sessions: sessions,
            total_validated: validated,
            total_rejected: rejected,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: ReplayStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, stats);
    }
}

// =============================================================================
// Additional coverage tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// AR-15: Verdict::Fail serde roundtrip with CommandMismatch reason.
    #[test]
    fn ar15_verdict_fail_serde(
        id in "[a-z]{3,8}",
        expected in prop::collection::vec("[a-z ]{3,20}", 1..4),
        proposed in prop::collection::vec("[a-z ]{3,20}", 1..4),
    ) {
        let v = ReplayVerdict::Fail {
            incident_id: id,
            reason: FailReason::CommandMismatch { expected, proposed },
        };
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ReplayVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&v, &decoded);
    }

    /// AR-16: Verdict::Inconclusive serde roundtrip.
    #[test]
    fn ar16_verdict_inconclusive_serde(
        id in "[a-z]{3,8}",
        reason in "[a-zA-Z0-9 ]{5,40}",
    ) {
        let v = ReplayVerdict::Inconclusive {
            incident_id: id,
            reason,
        };
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ReplayVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&v, &decoded);
    }

    /// AR-17: All 5 FailReason variants survive serde roundtrip.
    #[test]
    fn ar17_fail_reason_serde_all_variants(
        variant in 0u8..5,
        sim in 0.0..1.0f64,
        thresh in 0.0..1.0f64,
        elapsed in 1..10000u64,
        max_ms in 1..10000u64,
    ) {
        let reason = match variant {
            0 => FailReason::PatternMismatch,
            1 => FailReason::CommandMismatch {
                expected: vec!["cmd-a".into()],
                proposed: vec!["cmd-b".into()],
            },
            2 => FailReason::OutputDivergence {
                similarity: sim,
                threshold: thresh,
            },
            3 => FailReason::Timeout { elapsed_ms: elapsed, max_ms },
            _ => FailReason::OriginalFailed,
        };
        let json = serde_json::to_string(&reason).unwrap();
        let decoded: FailReason = serde_json::from_str(&json).unwrap();
        // Use tolerance for f64-containing variants
        match (&reason, &decoded) {
            (
                FailReason::OutputDivergence { similarity: s1, threshold: t1 },
                FailReason::OutputDivergence { similarity: s2, threshold: t2 },
            ) => {
                prop_assert!((s1 - s2).abs() < 1e-10);
                prop_assert!((t1 - t2).abs() < 1e-10);
            }
            _ => {
                prop_assert_eq!(&reason, &decoded);
            }
        }
    }

    /// AR-18: Assessment::Rejected serde roundtrip.
    #[test]
    fn ar18_assessment_rejected_serde(
        pass_rate in 0.0..1.0f64,
        incidents in 1..100usize,
        reason in "[a-z ]{5,30}",
    ) {
        let a = ReplayAssessment::Rejected {
            pass_rate,
            incidents,
            reason,
        };
        let json = serde_json::to_string(&a).unwrap();
        let decoded: ReplayAssessment = serde_json::from_str(&json).unwrap();
        if let ReplayAssessment::Rejected {
            pass_rate: dp,
            incidents: di,
            reason: dr,
        } = &decoded
        {
            if let ReplayAssessment::Rejected { pass_rate: op, incidents: oi, reason: or_ } = &a {
                prop_assert!((dp - op).abs() < 1e-10);
                prop_assert_eq!(di, oi);
                prop_assert_eq!(dr, or_);
            }
        } else {
            prop_assert!(false, "wrong variant after roundtrip");
        }
    }

    /// AR-19: Assessment::InsufficientData serde roundtrip.
    #[test]
    fn ar19_assessment_insufficient_serde(
        available in 0..50usize,
        required in 1..50usize,
    ) {
        let a = ReplayAssessment::InsufficientData { available, required };
        let json = serde_json::to_string(&a).unwrap();
        let decoded: ReplayAssessment = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&a, &decoded);
    }

    /// AR-20: is_validated() returns false for Rejected.
    #[test]
    fn ar20_rejected_not_validated(
        pass_rate in 0.0..1.0f64,
        incidents in 1..100usize,
    ) {
        let a = ReplayAssessment::Rejected {
            pass_rate,
            incidents,
            reason: "test".to_string(),
        };
        prop_assert!(!a.is_validated());
    }

    /// AR-21: is_validated() returns false for InsufficientData.
    #[test]
    fn ar21_insufficient_not_validated(
        available in 0..10usize,
        required in 1..20usize,
    ) {
        let a = ReplayAssessment::InsufficientData { available, required };
        prop_assert!(!a.is_validated());
    }

    /// AR-22: incident_id() returns correct ID for all three verdict variants.
    #[test]
    fn ar22_incident_id_all_variants(id in "[a-z]{3,10}") {
        let pass = ReplayVerdict::Pass {
            incident_id: id.clone(),
            match_score: 0.9,
        };
        prop_assert_eq!(pass.incident_id(), &id);

        let fail = ReplayVerdict::Fail {
            incident_id: id.clone(),
            reason: FailReason::PatternMismatch,
        };
        prop_assert_eq!(fail.incident_id(), &id);

        let inc = ReplayVerdict::Inconclusive {
            incident_id: id.clone(),
            reason: "test".to_string(),
        };
        prop_assert_eq!(inc.incident_id(), &id);
    }

    /// AR-23: count_inconclusive_as_pass=true counts inconclusive as passes,
    /// boosting the pass rate above what it would be otherwise.
    #[test]
    fn ar23_inconclusive_as_pass_boosts_rate(
        n_success in 1..5usize,
        n_failed_orig in 1..5usize,
    ) {
        let min_incidents = 1;
        // Without inconclusive-as-pass
        let config_no = ReplayConfig {
            min_incidents,
            min_pass_rate: 0.0, // accept any rate
            count_inconclusive_as_pass: false,
            ..Default::default()
        };
        let mut harness_no = ReplayHarness::new(config_no);

        // With inconclusive-as-pass
        let config_yes = ReplayConfig {
            min_incidents,
            min_pass_rate: 0.0,
            count_inconclusive_as_pass: true,
            ..Default::default()
        };
        let mut harness_yes = ReplayHarness::new(config_yes);

        // Build incidents: some successful, some failed originals (-> inconclusive)
        let mut incidents = Vec::new();
        for i in 0..n_success {
            incidents.push(HistoricalIncident {
                incident_id: format!("ok-{i}"),
                trigger_pattern: vec![1],
                output_before: "error: connection refused".to_string(),
                output_after: "connected successfully".to_string(),
                actual_commands: vec!["systemctl restart app".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: true,
            });
        }
        for i in 0..n_failed_orig {
            incidents.push(HistoricalIncident {
                incident_id: format!("fail-{i}"),
                trigger_pattern: vec![1],
                output_before: "err".to_string(),
                output_after: "still err".to_string(),
                actual_commands: vec!["cmd".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: false,
            });
        }

        let cmds = vec!["systemctl restart app".into()];
        let session_no = harness_no.validate(1, &cmds, &incidents, 1000);
        let session_yes = harness_yes.validate(1, &cmds, &incidents, 1000);

        // With inconclusive-as-pass, the effective total is larger (includes inconclusives)
        // and passes include them too, so rate should be >= the without-inconclusive rate.
        let rate_no = match &session_no.assessment {
            ReplayAssessment::Validated { pass_rate, .. } => *pass_rate,
            ReplayAssessment::Rejected { pass_rate, .. } => *pass_rate,
            ReplayAssessment::InsufficientData { .. } => 0.0,
        };
        let rate_yes = match &session_yes.assessment {
            ReplayAssessment::Validated { pass_rate, .. } => *pass_rate,
            ReplayAssessment::Rejected { pass_rate, .. } => *pass_rate,
            ReplayAssessment::InsufficientData { .. } => 0.0,
        };
        prop_assert!(
            rate_yes >= rate_no - 1e-10,
            "inconclusive-as-pass rate {} should be >= normal rate {}",
            rate_yes, rate_no
        );
    }

    /// AR-24: stats.total_validated increments exactly once per validated session.
    #[test]
    fn ar24_stats_validated_increments(n in 1..6usize) {
        let config = ReplayConfig {
            min_incidents: 1,
            min_pass_rate: 0.0, // everything validates
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents = vec![HistoricalIncident {
            incident_id: "i1".to_string(),
            trigger_pattern: vec![1],
            output_before: "error: connection refused".to_string(),
            output_after: "connected successfully".to_string(),
            actual_commands: vec!["systemctl restart app".into()],
            timestamp_ms: 1000,
            pane_id: 1,
            original_success: true,
        }];

        for _ in 0..n {
            harness.validate(1, &["systemctl restart app".into()], &incidents, 1000);
        }

        let stats = harness.stats();
        prop_assert_eq!(stats.total_validated, n as u64);
        prop_assert_eq!(stats.total_rejected, 0);
        prop_assert_eq!(stats.total_sessions, n as u64);
    }

    /// AR-25: stats.total_rejected increments for each rejected session.
    #[test]
    fn ar25_stats_rejected_increments(n in 1..6usize) {
        let config = ReplayConfig {
            min_incidents: 1,
            min_pass_rate: 1.0, // very strict → rejects mismatches
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents = vec![HistoricalIncident {
            incident_id: "i1".to_string(),
            trigger_pattern: vec![1],
            output_before: "err".to_string(),
            output_after: "ok".to_string(),
            actual_commands: vec!["systemctl restart app".into()],
            timestamp_ms: 1000,
            pane_id: 1,
            original_success: true,
        }];

        for _ in 0..n {
            // Different command -> low similarity -> fail -> rejected
            harness.validate(1, &["totally unrelated xyz".into()], &incidents, 1000);
        }

        let stats = harness.stats();
        prop_assert_eq!(stats.total_rejected, n as u64);
        prop_assert_eq!(stats.total_validated, 0);
    }

    /// AR-26: ReplaySession survives serde roundtrip.
    #[test]
    fn ar26_session_serde_roundtrip(
        n_incidents in 3..8usize,
    ) {
        let mut harness = ReplayHarness::with_defaults();
        let incidents: Vec<_> = (0..n_incidents)
            .map(|i| HistoricalIncident {
                incident_id: format!("inc-{i}"),
                trigger_pattern: vec![1, 2],
                output_before: "error: connection refused".to_string(),
                output_after: "connected successfully".to_string(),
                actual_commands: vec!["systemctl restart app".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: true,
            })
            .collect();

        let session = harness.validate(1, &["systemctl restart app".into()], &incidents, 2000);
        let json = serde_json::to_string(&session).unwrap();
        let decoded: ReplaySession = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(session.reflex_id, decoded.reflex_id);
        prop_assert_eq!(&session.proposed_commands, &decoded.proposed_commands);
        prop_assert_eq!(session.verdicts.len(), decoded.verdicts.len());
        prop_assert_eq!(session.timestamp_ms, decoded.timestamp_ms);
        // Assessment variant should match
        let orig_validated = session.assessment.is_validated();
        let dec_validated = decoded.assessment.is_validated();
        prop_assert_eq!(orig_validated, dec_validated);
    }

    /// AR-27: Pass rate in assessment equals passes/applicable_total.
    #[test]
    fn ar27_pass_rate_accuracy(
        n_matching in 1..6usize,
        n_mismatching in 0..4usize,
    ) {
        let config = ReplayConfig {
            min_incidents: 1,
            min_pass_rate: 0.0, // accept any rate to avoid rejection hiding the rate
            count_inconclusive_as_pass: false,
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);

        let mut incidents = Vec::new();
        // Matching commands -> Pass
        for i in 0..n_matching {
            incidents.push(HistoricalIncident {
                incident_id: format!("match-{i}"),
                trigger_pattern: vec![1],
                output_before: "error: connection refused".to_string(),
                output_after: "connected successfully".to_string(),
                actual_commands: vec!["systemctl restart app".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: true,
            });
        }
        // Mismatching commands with different actual_commands -> Fail
        for i in 0..n_mismatching {
            incidents.push(HistoricalIncident {
                incident_id: format!("mismatch-{i}"),
                trigger_pattern: vec![1],
                output_before: "err".to_string(),
                output_after: "ok".to_string(),
                actual_commands: vec!["completely different unrelated operation".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: true,
            });
        }

        let session = harness.validate(
            1,
            &["systemctl restart app".into()],
            &incidents,
            1000,
        );

        // Count actual passes and fails in verdicts
        let passes = session.verdicts.iter().filter(|v| v.is_pass()).count();
        let fails = session.verdicts.iter().filter(|v| v.is_fail()).count();
        let total = passes + fails;

        if total > 0 {
            let expected_rate = passes as f64 / total as f64;
            let actual_rate = match &session.assessment {
                ReplayAssessment::Validated { pass_rate, .. } => *pass_rate,
                ReplayAssessment::Rejected { pass_rate, .. } => *pass_rate,
                ReplayAssessment::InsufficientData { .. } => -1.0,
            };
            prop_assert!(
                (actual_rate - expected_rate).abs() < 1e-10,
                "rate {} != expected {} (passes={}, fails={})",
                actual_rate, expected_rate, passes, fails
            );
        }
    }

    /// AR-28: InsufficientData stores correct available and required counts.
    #[test]
    fn ar28_insufficient_data_counts(
        min_required in 5..15usize,
        available in 1..5usize,
    ) {
        let config = ReplayConfig {
            min_incidents: min_required,
            ..Default::default()
        };
        let mut harness = ReplayHarness::new(config);
        let incidents: Vec<_> = (0..available)
            .map(|i| HistoricalIncident {
                incident_id: format!("inc-{i}"),
                trigger_pattern: vec![1],
                output_before: "err".to_string(),
                output_after: "ok".to_string(),
                actual_commands: vec!["cmd".into()],
                timestamp_ms: 1000,
                pane_id: 1,
                original_success: true,
            })
            .collect();

        let session = harness.validate(1, &["cmd".into()], &incidents, 1000);
        if let ReplayAssessment::InsufficientData {
            available: a,
            required: r,
        } = &session.assessment
        {
            prop_assert_eq!(*a, available);
            prop_assert_eq!(*r, min_required);
        } else {
            prop_assert!(false, "expected InsufficientData");
        }
    }
}
