//! Property-based tests for ARS Evidence Card renderer.
//!
//! Verifies serde roundtrips, builder invariants, renderer structural
//! properties, and section construction from verdicts/assessments.

use proptest::prelude::*;

use frankenterm_core::ars_blast_radius::{BlastDecision, DenyReason, MaturityTier};
use frankenterm_core::ars_drift::DriftVerdict;
use frankenterm_core::ars_evidence::EvidenceVerdict;
use frankenterm_core::ars_explain::{
    BlastSection, DriftSection, EvidenceCardBuilder, EvidenceSection, ReplaySection, TimelineEvent,
    TimelineKind, render_card, render_summary,
};
use frankenterm_core::ars_replay::ReplayAssessment;

// =============================================================================
// Strategies
// =============================================================================

fn arb_maturity() -> impl Strategy<Value = MaturityTier> {
    prop_oneof![
        Just(MaturityTier::Incubating),
        Just(MaturityTier::Graduated),
        Just(MaturityTier::Veteran),
    ]
}

fn arb_timeline_kind() -> impl Strategy<Value = TimelineKind> {
    prop_oneof![
        Just(TimelineKind::Calibrated),
        Just(TimelineKind::Promoted),
        Just(TimelineKind::Executed),
        Just(TimelineKind::Drifted),
        Just(TimelineKind::Evolved),
        Just(TimelineKind::Deprecated),
    ]
}

fn arb_evidence_verdict() -> impl Strategy<Value = EvidenceVerdict> {
    prop_oneof![
        Just(EvidenceVerdict::Support),
        Just(EvidenceVerdict::Neutral),
        Just(EvidenceVerdict::Reject),
    ]
}

fn arb_drift_section() -> impl Strategy<Value = DriftSection> {
    (
        0.0..1000.0f64,
        1.0..100.0f64,
        any::<bool>(),
        0.0..1.0f64,
        0.0..1.0f64,
        0..1000usize,
    )
        .prop_map(
            |(e_value, threshold, is_drifted, null_rate, observed_rate, observations)| {
                DriftSection {
                    e_value,
                    threshold,
                    is_drifted,
                    null_rate,
                    observed_rate,
                    observations,
                }
            },
        )
}

fn arb_replay_section() -> impl Strategy<Value = ReplaySection> {
    (0.0..1.0f64, 0..100usize, any::<bool>(), "[a-z]{0,10}").prop_map(
        |(pass_rate, incidents, validated, note)| ReplaySection {
            pass_rate,
            incidents,
            validated,
            note,
        },
    )
}

fn arb_blast_section() -> impl Strategy<Value = BlastSection> {
    (
        any::<bool>(),
        arb_maturity(),
        proptest::option::of("[a-z]{3,8}"),
    )
        .prop_map(|(allowed, tier, deny_reason)| BlastSection {
            allowed,
            tier,
            deny_reason,
        })
}

fn arb_evidence_section() -> impl Strategy<Value = EvidenceSection> {
    (
        0..100usize,
        any::<bool>(),
        arb_evidence_verdict(),
        proptest::collection::vec("[a-z]{3,8}", 0..5),
        "[a-f0-9]{8,16}",
    )
        .prop_map(
            |(entry_count, is_complete, verdict, categories, root_hash)| EvidenceSection {
                entry_count,
                is_complete,
                verdict,
                categories,
                root_hash,
            },
        )
}

// =============================================================================
// Serde roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn drift_section_serde_roundtrip(section in arb_drift_section()) {
        let json = serde_json::to_string(&section).unwrap();
        let decoded: DriftSection = serde_json::from_str(&json).unwrap();
        let diff_e = (decoded.e_value - section.e_value).abs();
        let diff_t = (decoded.threshold - section.threshold).abs();
        prop_assert!(diff_e < 1e-10);
        prop_assert!(diff_t < 1e-10);
        prop_assert_eq!(decoded.is_drifted, section.is_drifted);
        prop_assert_eq!(decoded.observations, section.observations);
    }

    #[test]
    fn replay_section_serde_roundtrip(section in arb_replay_section()) {
        let json = serde_json::to_string(&section).unwrap();
        let decoded: ReplaySection = serde_json::from_str(&json).unwrap();
        let diff = (decoded.pass_rate - section.pass_rate).abs();
        prop_assert!(diff < 1e-10);
        prop_assert_eq!(decoded.incidents, section.incidents);
        prop_assert_eq!(decoded.validated, section.validated);
    }

    #[test]
    fn blast_section_serde_roundtrip(section in arb_blast_section()) {
        let json = serde_json::to_string(&section).unwrap();
        let decoded: BlastSection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.allowed, section.allowed);
        prop_assert_eq!(decoded.deny_reason, section.deny_reason);
    }

    #[test]
    fn evidence_section_serde_roundtrip(section in arb_evidence_section()) {
        let json = serde_json::to_string(&section).unwrap();
        let decoded: EvidenceSection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.entry_count, section.entry_count);
        prop_assert_eq!(decoded.is_complete, section.is_complete);
        prop_assert_eq!(decoded.root_hash, section.root_hash);
    }

    #[test]
    fn timeline_event_serde_roundtrip(
        ts in 0..u64::MAX,
        label in "[a-z]{3,10}",
        kind in arb_timeline_kind(),
    ) {
        let event = TimelineEvent {
            timestamp_ms: ts,
            label,
            kind,
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: TimelineEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, event);
    }
}

// =============================================================================
// Card builder invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn builder_preserves_reflex_id(id in 0..u64::MAX) {
        let card = EvidenceCardBuilder::new(id, "test").build();
        prop_assert_eq!(card.reflex_id, id);
    }

    #[test]
    fn builder_preserves_name(name in "[a-z]{3,20}") {
        let card = EvidenceCardBuilder::new(1, &name).build();
        prop_assert_eq!(card.reflex_name, name);
    }

    #[test]
    fn builder_preserves_version(v in 1..1000u32) {
        let card = EvidenceCardBuilder::new(1, "test").version(v).build();
        prop_assert_eq!(card.version, v);
    }

    #[test]
    fn builder_preserves_maturity(tier in arb_maturity()) {
        let card = EvidenceCardBuilder::new(1, "test").maturity(tier).build();
        prop_assert_eq!(card.maturity, tier);
    }

    #[test]
    fn builder_preserves_executions(s in 0..1000u64, f in 0..100u64) {
        let card = EvidenceCardBuilder::new(1, "test").executions(s, f).build();
        prop_assert_eq!(card.successes, s);
        prop_assert_eq!(card.failures, f);
    }

    #[test]
    fn builder_preserves_timeline_count(n in 0..10usize) {
        let mut b = EvidenceCardBuilder::new(1, "test");
        for i in 0..n {
            b = b.timeline_event(i as u64, "evt", TimelineKind::Executed);
        }
        let card = b.build();
        prop_assert_eq!(card.timeline.len(), n);
    }
}

// =============================================================================
// Renderer structural invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn card_starts_with_top_border(
        id in 0..1000u64,
        name in "[a-z]{3,15}",
    ) {
        let card = EvidenceCardBuilder::new(id, &name).build();
        let text = render_card(&card);
        prop_assert!(text.starts_with('┌'));
        prop_assert!(text.ends_with('┘'));
    }

    #[test]
    fn card_contains_reflex_name(name in "[a-z]{3,15}") {
        let card = EvidenceCardBuilder::new(1, &name).build();
        let text = render_card(&card);
        prop_assert!(text.contains(&name));
    }

    #[test]
    fn summary_contains_reflex_name(name in "[a-z]{3,15}") {
        let card = EvidenceCardBuilder::new(1, &name).build();
        let summary = render_summary(&card);
        prop_assert!(summary.contains(&name));
    }

    #[test]
    fn summary_is_single_line(name in "[a-z]{3,10}") {
        let card = EvidenceCardBuilder::new(1, &name).build();
        let summary = render_summary(&card);
        prop_assert!(!summary.contains('\n'));
    }

    #[test]
    fn card_lines_are_bordered(name in "[a-z]{3,10}") {
        let card = EvidenceCardBuilder::new(1, &name)
            .timeline_event(1, "test", TimelineKind::Executed)
            .build();
        let text = render_card(&card);
        for line in text.lines() {
            let first = line.chars().next().unwrap();
            let last = line.chars().last().unwrap();
            let is_valid_border = matches!(
                (first, last),
                ('┌', '┐') | ('│', '│') | ('├', '┤') | ('└', '┘')
            );
            prop_assert!(is_valid_border, "bad border: {:?}", line);
        }
    }
}

// =============================================================================
// Section construction invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn drift_status_label_is_one_of_three(section in arb_drift_section()) {
        let label = section.status_label();
        let is_valid = label == "safe" || label == "DRIFTED" || label == "calibrating";
        prop_assert!(is_valid, "unexpected label: {}", label);
    }

    #[test]
    fn replay_status_label_is_pass_or_fail(section in arb_replay_section()) {
        let label = section.status_label();
        let is_valid = label == "PASS" || label == "FAIL";
        prop_assert!(is_valid);
    }

    #[test]
    fn blast_status_label_is_allow_or_deny(section in arb_blast_section()) {
        let label = section.status_label();
        let is_valid = label == "Allow" || label == "Deny";
        prop_assert!(is_valid);
    }

    #[test]
    fn evidence_status_label_is_valid(section in arb_evidence_section()) {
        let label = section.status_label();
        let is_valid = label == "Support" || label == "Neutral" || label == "Reject";
        prop_assert!(is_valid, "unexpected label: {}", label);
    }

    #[test]
    fn drift_drifted_implies_label(
        e_value in 0.0..1000.0f64,
        threshold in 1.0..100.0f64,
        null_rate in 0.01..0.99f64,
        obs_rate in 0.01..0.99f64,
        obs in 1..100usize,
    ) {
        let section = DriftSection {
            e_value,
            threshold,
            is_drifted: true,
            null_rate,
            observed_rate: obs_rate,
            observations: obs,
        };
        prop_assert_eq!(section.status_label(), "DRIFTED");
    }

    #[test]
    fn replay_validated_implies_pass(
        pass_rate in 0.0..1.0f64,
        incidents in 1..100usize,
    ) {
        let section = ReplaySection {
            pass_rate,
            incidents,
            validated: true,
            note: String::new(),
        };
        prop_assert_eq!(section.status_label(), "PASS");
    }

    #[test]
    fn blast_allowed_implies_allow(tier in arb_maturity()) {
        let section = BlastSection {
            allowed: true,
            tier,
            deny_reason: None,
        };
        prop_assert_eq!(section.status_label(), "Allow");
    }

    #[test]
    fn drift_from_verdict_preserves_drifted(
        e_val in 1.0..100.0f64,
        null_rate in 0.01..0.99f64,
        obs_rate in 0.01..0.99f64,
        obs in 1..100usize,
        threshold in 1.0..100.0f64,
    ) {
        let verdict = DriftVerdict::Drifted {
            e_value: e_val,
            null_rate,
            observed_rate: obs_rate,
            observations: obs,
        };
        let section = DriftSection::from_verdict(&verdict, threshold);
        prop_assert!(section.is_drifted);
        let diff = (section.e_value - e_val).abs();
        prop_assert!(diff < 1e-10);
    }

    #[test]
    fn replay_from_validated_preserves(
        pass_rate in 0.5..1.0f64,
        incidents in 1..100usize,
    ) {
        let assessment = ReplayAssessment::Validated { pass_rate, incidents };
        let section = ReplaySection::from_assessment(&assessment);
        prop_assert!(section.validated);
        prop_assert_eq!(section.incidents, incidents);
    }

    #[test]
    fn replay_from_rejected_preserves(
        pass_rate in 0.0..0.5f64,
        incidents in 1..100usize,
        reason in "[a-z]{3,15}",
    ) {
        let assessment = ReplayAssessment::Rejected { pass_rate, incidents, reason: reason.clone() };
        let section = ReplaySection::from_assessment(&assessment);
        prop_assert!(!section.validated);
        prop_assert_eq!(section.note, reason);
    }

    #[test]
    fn blast_from_allow_preserves_tier(tier in arb_maturity()) {
        let decision = BlastDecision::Allow { tier };
        let section = BlastSection::from_decision(&decision);
        prop_assert!(section.allowed);
        prop_assert_eq!(section.tier, tier);
    }

    #[test]
    fn blast_from_deny_preserves(tier in arb_maturity()) {
        let decision = BlastDecision::Deny {
            reason: DenyReason::SwarmLimit,
            tier,
        };
        let section = BlastSection::from_decision(&decision);
        prop_assert!(!section.allowed);
        prop_assert!(section.deny_reason.is_some());
    }
}

// =============================================================================
// Full card serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn full_card_serde_roundtrip(
        id in 0..1000u64,
        name in "[a-z]{3,10}",
        version in 1..100u32,
        tier in arb_maturity(),
        s in 0..1000u64,
        f in 0..100u64,
    ) {
        let card = EvidenceCardBuilder::new(id, &name)
            .version(version)
            .maturity(tier)
            .executions(s, f)
            .build();
        let json = serde_json::to_string(&card).unwrap();
        let decoded: frankenterm_core::ars_explain::EvidenceCard =
            serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.reflex_id, id);
        prop_assert_eq!(decoded.reflex_name, name);
        prop_assert_eq!(decoded.version, version);
        prop_assert_eq!(decoded.successes, s);
        prop_assert_eq!(decoded.failures, f);
    }
}
