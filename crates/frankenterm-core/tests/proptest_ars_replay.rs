//! Property-based tests for ARS Historical Replay Validation Harness.
//!
//! Verifies replay verdicts, pass rate calculations, incident capping,
//! inconclusive handling, and serde roundtrips.

use proptest::prelude::*;

use frankenterm_core::ars_replay::{
    FailReason, HistoricalIncident, ReplayAssessment, ReplayConfig, ReplayHarness, ReplayStats,
    ReplayVerdict,
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
