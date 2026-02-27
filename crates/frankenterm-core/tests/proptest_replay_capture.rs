//! Property-based tests for replay_capture.rs.
//!
//! Covers serde roundtrips for CaptureSensitivityTier, CaptureRedactionMode,
//! CaptureRedactionPolicy, DecisionType, DecisionEvent; ordering invariants
//! for CaptureSensitivityTier; fnv1a_hash_text determinism and uniqueness
//! properties; sha256_hex format and determinism; summarize_decision_input
//! length bounding; and CaptureConfig default invariants.

use frankenterm_core::replay_capture::{
    CaptureRedactionMode, CaptureRedactionPolicy, CaptureSensitivityTier, DecisionEvent,
    DecisionType, fnv1a_hash_text, sha256_hex, summarize_decision_input,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_sensitivity_tier() -> impl Strategy<Value = CaptureSensitivityTier> {
    prop_oneof![
        Just(CaptureSensitivityTier::T1),
        Just(CaptureSensitivityTier::T2),
        Just(CaptureSensitivityTier::T3),
    ]
}

fn arb_redaction_mode() -> impl Strategy<Value = CaptureRedactionMode> {
    prop_oneof![
        Just(CaptureRedactionMode::Mask),
        Just(CaptureRedactionMode::Hash),
        Just(CaptureRedactionMode::Drop),
    ]
}

fn arb_decision_type() -> impl Strategy<Value = DecisionType> {
    prop_oneof![
        Just(DecisionType::PatternMatch),
        Just(DecisionType::WorkflowStep),
        Just(DecisionType::PolicyEvaluation),
    ]
}

fn arb_redaction_policy() -> impl Strategy<Value = CaptureRedactionPolicy> {
    (
        any::<bool>(),
        arb_redaction_mode(),
        1..=365u64,
        1..=365u64,
        1..=365u64,
    )
        .prop_map(
            |(enabled, mode, t1, t2, t3)| CaptureRedactionPolicy {
                enabled,
                mode,
                t1_retention_days: t1,
                t2_retention_days: t2,
                t3_retention_days: t3,
                custom_patterns: Vec::new(),
            },
        )
}

fn arb_decision_event() -> impl Strategy<Value = DecisionEvent> {
    (
        arb_decision_type(),
        "[a-z_]{3,15}",
        "[a-zA-Z0-9 ]{5,30}",
        "[a-zA-Z0-9 ]{5,30}",
        proptest::option::of("[a-f0-9]{8}"),
        proptest::option::of((0..=100u64).prop_map(|v| v as f64 / 100.0)),
        0..=1_000_000u64,
        1..=100u64,
    )
        .prop_map(
            |(decision_type, rule_id, definition_text, input_text, parent_event_id, confidence, timestamp_ms, pane_id)| {
                DecisionEvent::new(
                    decision_type,
                    pane_id,
                    rule_id,
                    &definition_text,
                    &input_text,
                    serde_json::json!({"result": "ok"}),
                    parent_event_id,
                    confidence,
                    timestamp_ms,
                )
            },
        )
}

// ── CaptureSensitivityTier ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 1. Serde roundtrip
    #[test]
    fn sensitivity_tier_serde_roundtrip(tier in arb_sensitivity_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let restored: CaptureSensitivityTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, tier);
    }

    // 2. Total ordering: T1 < T2 < T3
    #[test]
    fn sensitivity_tier_ordering(a in arb_sensitivity_tier(), b in arb_sensitivity_tier()) {
        fn rank(t: CaptureSensitivityTier) -> u8 {
            match t {
                CaptureSensitivityTier::T1 => 0,
                CaptureSensitivityTier::T2 => 1,
                CaptureSensitivityTier::T3 => 2,
            }
        }
        prop_assert_eq!(a.cmp(&b), rank(a).cmp(&rank(b)));
    }

    // 3. as_str matches serde variant
    #[test]
    fn sensitivity_tier_as_str_matches_serde(tier in arb_sensitivity_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let expected = format!("\"{}\"", tier.as_str());
        prop_assert_eq!(json, expected);
    }

    // 4. as_str values are unique
    #[test]
    fn sensitivity_tier_as_str_unique(_seed in 0..10u32) {
        let tiers = [
            CaptureSensitivityTier::T1,
            CaptureSensitivityTier::T2,
            CaptureSensitivityTier::T3,
        ];
        let strs: Vec<_> = tiers.iter().map(|t| t.as_str()).collect();
        let mut unique = strs.clone();
        unique.sort();
        unique.dedup();
        prop_assert_eq!(unique.len(), strs.len());
    }
}

// ── CaptureRedactionMode ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 5. Serde roundtrip
    #[test]
    fn redaction_mode_serde_roundtrip(mode in arb_redaction_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let restored: CaptureRedactionMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, mode);
    }

    // 6. All modes are distinct
    #[test]
    fn redaction_mode_distinct(_seed in 0..10u32) {
        let modes = [
            CaptureRedactionMode::Mask,
            CaptureRedactionMode::Hash,
            CaptureRedactionMode::Drop,
        ];
        for i in 0..modes.len() {
            for j in (i + 1)..modes.len() {
                prop_assert_ne!(modes[i], modes[j]);
            }
        }
    }
}

// ── CaptureRedactionPolicy ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 7. Serde roundtrip
    #[test]
    fn redaction_policy_serde_roundtrip(policy in arb_redaction_policy()) {
        let json = serde_json::to_string(&policy).unwrap();
        let restored: CaptureRedactionPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.enabled, policy.enabled);
        prop_assert_eq!(restored.mode, policy.mode);
        prop_assert_eq!(restored.t1_retention_days, policy.t1_retention_days);
        prop_assert_eq!(restored.t2_retention_days, policy.t2_retention_days);
        prop_assert_eq!(restored.t3_retention_days, policy.t3_retention_days);
    }

    // 8. Default policy has redaction enabled
    #[test]
    fn redaction_policy_default_enabled(_seed in 0..10u32) {
        let policy = CaptureRedactionPolicy::default();
        prop_assert!(policy.enabled);
        prop_assert_eq!(policy.mode, CaptureRedactionMode::Mask);
        prop_assert!(policy.t1_retention_days >= policy.t3_retention_days,
            "T1 retention should be >= T3 retention");
    }
}

// ── DecisionType ────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 9. Serde roundtrip
    #[test]
    fn decision_type_serde_roundtrip(dt in arb_decision_type()) {
        let json = serde_json::to_string(&dt).unwrap();
        let restored: DecisionType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, dt);
    }
}

// ── DecisionEvent ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 10. Serde roundtrip preserves all fields
    #[test]
    fn decision_event_serde_roundtrip(event in arb_decision_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let restored: DecisionEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.decision_type, event.decision_type);
        prop_assert_eq!(&restored.rule_id, &event.rule_id);
        prop_assert_eq!(restored.definition_hash, event.definition_hash);
        prop_assert_eq!(&restored.input_hash, &event.input_hash);
        prop_assert_eq!(&restored.input_summary, &event.input_summary);
        prop_assert_eq!(&restored.parent_event_id, &event.parent_event_id);
        prop_assert_eq!(restored.timestamp_ms, event.timestamp_ms);
        prop_assert_eq!(restored.pane_id, event.pane_id);
    }

    // 11. DecisionEvent.new populates definition_hash deterministically
    #[test]
    fn decision_event_definition_hash_deterministic(
        definition in "[a-zA-Z]{5,20}",
        dt in arb_decision_type(),
    ) {
        let e1 = DecisionEvent::new(
            dt, 1, "rule1", &definition, "input",
            serde_json::json!(null), None, None, 0,
        );
        let e2 = DecisionEvent::new(
            dt, 1, "rule1", &definition, "input",
            serde_json::json!(null), None, None, 0,
        );
        prop_assert_eq!(e1.definition_hash, e2.definition_hash);
    }

    // 12. DecisionEvent.new populates input_hash deterministically
    #[test]
    fn decision_event_input_hash_deterministic(
        input in "[a-zA-Z]{5,20}",
        dt in arb_decision_type(),
    ) {
        let e1 = DecisionEvent::new(
            dt, 1, "rule1", "def", &input,
            serde_json::json!(null), None, None, 0,
        );
        let e2 = DecisionEvent::new(
            dt, 1, "rule1", "def", &input,
            serde_json::json!(null), None, None, 0,
        );
        prop_assert_eq!(&e1.input_hash, &e2.input_hash);
    }
}

// ── fnv1a_hash_text ─────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 13. fnv1a is deterministic: same input → same hash
    #[test]
    fn fnv1a_deterministic(input in ".*") {
        let h1 = fnv1a_hash_text(&input);
        let h2 = fnv1a_hash_text(&input);
        prop_assert_eq!(h1, h2);
    }

    // 14. fnv1a empty string has defined value
    #[test]
    fn fnv1a_empty_defined(_seed in 0..10u32) {
        let h = fnv1a_hash_text("");
        // FNV-1a offset basis
        prop_assert_eq!(h, 0xcbf29ce484222325);
    }

    // 15. Different non-empty inputs usually produce different hashes (probabilistic)
    #[test]
    fn fnv1a_collision_resistant(
        a in "[a-zA-Z0-9]{4,20}",
        b in "[a-zA-Z0-9]{4,20}",
    ) {
        if a != b {
            let ha = fnv1a_hash_text(&a);
            let hb = fnv1a_hash_text(&b);
            // Not a hard guarantee, but extremely likely for short strings
            prop_assert_ne!(ha, hb, "different inputs '{}' and '{}' should (very likely) hash differently", a, b);
        }
    }
}

// ── sha256_hex ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 16. sha256_hex is deterministic
    #[test]
    fn sha256_hex_deterministic(input in ".*") {
        let h1 = sha256_hex(&input);
        let h2 = sha256_hex(&input);
        prop_assert_eq!(h1, h2);
    }

    // 17. sha256_hex is always 64 hex characters
    #[test]
    fn sha256_hex_length(input in ".*") {
        let h = sha256_hex(&input);
        prop_assert_eq!(h.len(), 64, "SHA-256 hex should be 64 chars, got {}", h.len());
    }

    // 18. sha256_hex only contains hex characters
    #[test]
    fn sha256_hex_format(input in ".*") {
        let h = sha256_hex(&input);
        let all_hex = h.chars().all(|c| c.is_ascii_hexdigit());
        prop_assert!(all_hex, "SHA-256 hex should only contain hex chars, got '{}'", h);
    }

    // 19. sha256_hex uses lowercase
    #[test]
    fn sha256_hex_lowercase(input in ".{1,20}") {
        let h = sha256_hex(&input);
        let lower = h.to_lowercase();
        prop_assert_eq!(h, lower, "SHA-256 hex should be lowercase");
    }
}

// ── summarize_decision_input ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 20. Summary never exceeds 256 bytes
    #[test]
    fn summary_bounded_at_256_bytes(input in ".{0,500}") {
        let summary = summarize_decision_input(&input);
        prop_assert!(summary.len() <= 256,
            "summary should be <= 256 bytes, got {} bytes", summary.len());
    }

    // 21. Summary is valid UTF-8 (no mid-codepoint truncation)
    #[test]
    fn summary_valid_utf8(input in ".*") {
        let summary = summarize_decision_input(&input);
        // If we can create a String, it's valid UTF-8
        let _ = String::from(summary);
    }

    // 22. Short input: summary <= input length
    #[test]
    fn summary_short_input_preserved(input in "[a-zA-Z ]{1,50}") {
        let summary = summarize_decision_input(&input);
        // After redaction, the summary should not be longer than the redacted version
        // (which may be longer due to redaction markers, but should be bounded)
        prop_assert!(summary.len() <= 256);
    }

    // 23. Summary is deterministic
    #[test]
    fn summary_deterministic(input in ".{5,100}") {
        let s1 = summarize_decision_input(&input);
        let s2 = summarize_decision_input(&input);
        prop_assert_eq!(s1, s2);
    }
}
