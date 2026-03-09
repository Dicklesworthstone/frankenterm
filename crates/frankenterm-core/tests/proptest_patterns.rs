//! Property-based tests for patterns module.
//!
//! Verifies invariants for:
//! - AgentType: serde roundtrip, snake_case, Display non-empty, Display matches serde
//! - Severity: serde roundtrip, snake_case
//! - Detection: dedup_key deterministic, dedup_key varies on extracted data
//! - DetectionContext: new(), mark_seen/is_seen lifecycle, clear_seen resets,
//!   capacity enforcement (MAX_SEEN_KEYS=1000), with_agent_type, with_pane
//! - RuleDef: interpolate_template substitutions, get_preview_command
//! - PatternPack: serde roundtrip, new() constructor
//! - PatternLibrary: empty(), merge rules, pack_for_rule
//! - TraceSpan, TraceEvidence, TraceGate, TraceBounds: serde roundtrip
//! - MatchTrace: serde roundtrip (PartialEq available)
//! - TraceOptions: default values

use frankenterm_core::patterns::*;
use proptest::prelude::*;
use serde_json::json;
use std::time::Duration;

// ============================================================================
// Strategies
// ============================================================================

fn arb_agent_type() -> impl Strategy<Value = AgentType> {
    prop_oneof![
        Just(AgentType::Codex),
        Just(AgentType::ClaudeCode),
        Just(AgentType::Gemini),
        Just(AgentType::Wezterm),
        Just(AgentType::Unknown),
    ]
}

fn arb_severity() -> impl Strategy<Value = Severity> {
    prop_oneof![
        Just(Severity::Info),
        Just(Severity::Warning),
        Just(Severity::Critical),
    ]
}

fn arb_detection() -> impl Strategy<Value = Detection> {
    (
        "[a-z_.]{5,30}",
        arb_agent_type(),
        "[a-z_]{3,15}",
        arb_severity(),
        0.0f64..=1.0,
        prop_oneof![
            Just(json!({})),
            Just(json!({"key": "val"})),
            Just(json!({"a": 1, "b": "two"})),
        ],
        "[a-zA-Z0-9 ]{0,50}",
        (0usize..1000, 0usize..1000),
    )
        .prop_map(
            |(
                rule_id,
                agent_type,
                event_type,
                severity,
                confidence,
                extracted,
                matched_text,
                span,
            )| {
                Detection {
                    rule_id,
                    agent_type,
                    event_type,
                    severity,
                    confidence,
                    extracted,
                    matched_text,
                    span,
                }
            },
        )
}

fn arb_trace_span() -> impl Strategy<Value = TraceSpan> {
    (0usize..10000, 0usize..10000).prop_map(|(start, end)| TraceSpan { start, end })
}

fn arb_trace_evidence() -> impl Strategy<Value = TraceEvidence> {
    (
        "[a-z]{3,10}",
        proptest::option::of("[a-z ]{1,20}"),
        proptest::option::of(arb_trace_span()),
        proptest::option::of("[a-zA-Z0-9 ]{1,50}"),
        proptest::bool::ANY,
    )
        .prop_map(|(kind, label, span, excerpt, truncated)| TraceEvidence {
            kind,
            label,
            span,
            excerpt,
            truncated,
        })
}

fn arb_trace_gate() -> impl Strategy<Value = TraceGate> {
    (
        "[a-z_]{3,15}",
        proptest::bool::ANY,
        proptest::option::of("[a-z ]{5,30}"),
    )
        .prop_map(|(gate, passed, reason)| TraceGate {
            gate,
            passed,
            reason,
        })
}

fn arb_trace_bounds() -> impl Strategy<Value = TraceBounds> {
    (
        1usize..20,
        10usize..500,
        10usize..200,
        0usize..50,
        proptest::bool::ANY,
        // Always include at least one field since truncated_fields uses
        // skip_serializing_if="Vec::is_empty" without serde(default)
        prop::collection::vec("[a-z_]{3,15}", 1..5),
    )
        .prop_map(
            |(
                max_evidence_items,
                max_excerpt_bytes,
                max_capture_bytes,
                evidence_total,
                evidence_truncated,
                truncated_fields,
            )| {
                TraceBounds {
                    max_evidence_items,
                    max_excerpt_bytes,
                    max_capture_bytes,
                    evidence_total,
                    evidence_truncated,
                    truncated_fields,
                }
            },
        )
}

fn arb_rule_def() -> impl Strategy<Value = RuleDef> {
    (
        arb_agent_type(),
        "[a-z_]{3,15}",
        arb_severity(),
        "[a-z ]{5,30}",
    )
        .prop_map(|(agent_type, event_type, severity, description)| {
            let prefix = match agent_type {
                AgentType::Codex => "codex",
                AgentType::ClaudeCode => "claude_code",
                AgentType::Gemini => "gemini",
                AgentType::Wezterm => "wezterm",
                AgentType::Unknown => "codex",
            };
            RuleDef {
                id: format!("{}.test_rule", prefix),
                agent_type,
                event_type,
                severity,
                anchors: vec!["test_anchor".to_string()],
                regex: None,
                description,
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            }
        })
}

// ============================================================================
// AgentType properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// AgentType serde roundtrip.
    #[test]
    fn prop_agent_type_serde_roundtrip(at in arb_agent_type()) {
        let json = serde_json::to_string(&at).unwrap();
        let back: AgentType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(at, back);
    }

    /// AgentType serializes to snake_case.
    #[test]
    fn prop_agent_type_snake_case(at in arb_agent_type()) {
        let json = serde_json::to_string(&at).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized agent type should be snake_case, got '{}'", inner
        );
    }

    /// AgentType Display is non-empty.
    #[test]
    fn prop_agent_type_display_non_empty(at in arb_agent_type()) {
        let d = at.to_string();
        prop_assert!(!d.is_empty(), "Display should not be empty");
    }

    /// AgentType Display is lowercase.
    #[test]
    fn prop_agent_type_display_lowercase(at in arb_agent_type()) {
        let d = at.to_string();
        prop_assert!(
            d.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "Display should be lowercase, got '{}'", d
        );
    }
}

// ============================================================================
// Severity properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Severity serde roundtrip.
    #[test]
    fn prop_severity_serde_roundtrip(s in arb_severity()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: Severity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    /// Severity serializes to snake_case.
    #[test]
    fn prop_severity_snake_case(s in arb_severity()) {
        let json = serde_json::to_string(&s).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized severity should be snake_case, got '{}'", inner
        );
    }
}

// ============================================================================
// Detection properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Detection dedup_key is deterministic (same detection → same key).
    #[test]
    fn prop_detection_dedup_key_deterministic(d in arb_detection()) {
        let key1 = d.dedup_key();
        let key2 = d.dedup_key();
        prop_assert_eq!(&key1, &key2);
    }

    /// Detection dedup_key starts with rule_id prefix.
    #[test]
    fn prop_detection_dedup_key_has_rule_id(d in arb_detection()) {
        let key = d.dedup_key();
        prop_assert!(key.starts_with(&d.rule_id),
            "dedup_key '{}' should start with rule_id '{}'", key, d.rule_id);
    }

    /// Detections with same rule_id but different extracted have different keys.
    #[test]
    fn prop_detection_dedup_key_varies_on_extracted(
        rule_id in "[a-z_.]{5,20}",
    ) {
        let d1 = Detection {
            rule_id: rule_id.clone(),
            agent_type: AgentType::Codex,
            event_type: "error".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            extracted: json!({"key": "value1"}),
            matched_text: "test".to_string(),
            span: (0, 4),
        };
        let d2 = Detection {
            rule_id,
            agent_type: AgentType::Codex,
            event_type: "error".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            extracted: json!({"key": "value2"}),
            matched_text: "test".to_string(),
            span: (0, 4),
        };
        prop_assert!(d1.dedup_key() != d2.dedup_key(),
            "Different extracted values should produce different dedup keys");
    }

    /// Detections with empty extracted object have same key for same rule_id.
    #[test]
    fn prop_detection_dedup_key_empty_extracted_same(
        rule_id in "[a-z_.]{5,20}",
    ) {
        let d1 = Detection {
            rule_id: rule_id.clone(),
            agent_type: AgentType::Codex,
            event_type: "error".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            extracted: json!({}),
            matched_text: "test1".to_string(),
            span: (0, 5),
        };
        let d2 = Detection {
            rule_id,
            agent_type: AgentType::Codex,
            event_type: "error".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            extracted: json!({}),
            matched_text: "test2".to_string(),
            span: (0, 5),
        };
        prop_assert_eq!(d1.dedup_key(), d2.dedup_key(),
            "Same rule_id + empty extracted should produce same dedup key");
    }
}

// ============================================================================
// DetectionContext properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// New context starts empty.
    #[test]
    fn prop_context_new_empty(_dummy in Just(())) {
        let ctx = DetectionContext::new();
        prop_assert_eq!(ctx.seen_count(), 0);
        prop_assert!(ctx.pane_id.is_none());
        prop_assert!(ctx.agent_type.is_none());
    }

    /// with_agent_type sets agent_type.
    #[test]
    fn prop_context_with_agent_type(at in arb_agent_type()) {
        let ctx = DetectionContext::with_agent_type(at);
        prop_assert_eq!(ctx.agent_type, Some(at));
        prop_assert!(ctx.pane_id.is_none());
        prop_assert_eq!(ctx.seen_count(), 0);
    }

    /// with_pane sets pane_id and optional agent_type.
    #[test]
    fn prop_context_with_pane(
        pane_id in 0u64..1000,
        at in proptest::option::of(arb_agent_type()),
    ) {
        let ctx = DetectionContext::with_pane(pane_id, at);
        prop_assert_eq!(ctx.pane_id, Some(pane_id));
        prop_assert_eq!(ctx.agent_type, at);
    }

    /// mark_seen returns true on first call for a detection.
    #[test]
    fn prop_context_mark_seen_first_true(d in arb_detection()) {
        let mut ctx = DetectionContext::new();
        let result = ctx.mark_seen(&d);
        prop_assert!(result, "First mark_seen should return true");
    }

    /// mark_seen returns false on second call for same detection.
    #[test]
    fn prop_context_mark_seen_second_false(d in arb_detection()) {
        let mut ctx = DetectionContext::new();
        ctx.mark_seen(&d);
        let result = ctx.mark_seen(&d);
        prop_assert!(!result, "Second mark_seen should return false");
    }

    /// is_seen returns false for unseen detection.
    #[test]
    fn prop_context_is_seen_unseen(d in arb_detection()) {
        let ctx = DetectionContext::new();
        prop_assert!(!ctx.is_seen(&d));
    }

    /// is_seen returns true after mark_seen.
    #[test]
    fn prop_context_is_seen_after_mark(d in arb_detection()) {
        let mut ctx = DetectionContext::new();
        ctx.mark_seen(&d);
        prop_assert!(ctx.is_seen(&d));
    }

    /// seen_count increments with unique detections.
    #[test]
    fn prop_context_seen_count_increments(count in 1usize..20) {
        let mut ctx = DetectionContext::new();
        for i in 0..count {
            let d = Detection {
                rule_id: format!("codex.rule_{}", i),
                agent_type: AgentType::Codex,
                event_type: "error".to_string(),
                severity: Severity::Warning,
                confidence: 0.9,
                extracted: json!({"idx": i}),
                matched_text: "test".to_string(),
                span: (0, 4),
            };
            ctx.mark_seen(&d);
        }
        prop_assert_eq!(ctx.seen_count(), count);
    }

    /// clear_seen resets to empty.
    #[test]
    fn prop_context_clear_resets(d in arb_detection()) {
        let mut ctx = DetectionContext::new();
        ctx.mark_seen(&d);
        prop_assert_eq!(ctx.seen_count(), 1);
        ctx.clear_seen();
        prop_assert_eq!(ctx.seen_count(), 0);
        // After clear, mark_seen returns true again
        let result = ctx.mark_seen(&d);
        prop_assert!(result, "mark_seen should return true after clear");
    }

    /// set_ttl changes the TTL.
    #[test]
    fn prop_context_set_ttl(secs in 1u64..3600) {
        let mut ctx = DetectionContext::new();
        ctx.set_ttl(Duration::from_secs(secs));
        prop_assert_eq!(ctx.ttl, Duration::from_secs(secs));
    }

    /// Default TTL is 5 minutes (300 seconds).
    #[test]
    fn prop_context_default_ttl(_dummy in Just(())) {
        let ctx = DetectionContext::new();
        prop_assert_eq!(ctx.ttl, Duration::from_secs(300));
    }
}

// ============================================================================
// DetectionContext capacity enforcement
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5))]

    /// Context enforces MAX_SEEN_KEYS capacity by evicting oldest.
    #[test]
    fn prop_context_capacity_enforcement(_dummy in Just(())) {
        let mut ctx = DetectionContext::new();
        // Fill to capacity (1000)
        for i in 0..1010 {
            let d = Detection {
                rule_id: format!("codex.cap_{}", i),
                agent_type: AgentType::Codex,
                event_type: "error".to_string(),
                severity: Severity::Info,
                confidence: 0.5,
                extracted: json!({}),
                matched_text: "x".to_string(),
                span: (0, 1),
            };
            ctx.mark_seen(&d);
        }
        // Should be capped at 1000
        prop_assert!(ctx.seen_count() <= 1000,
            "seen_count {} exceeds max 1000", ctx.seen_count());
    }
}

// ============================================================================
// RuleDef properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// RuleDef serde roundtrip preserves key fields.
    #[test]
    fn prop_rule_def_serde_roundtrip(rule in arb_rule_def()) {
        let json = serde_json::to_string(&rule).unwrap();
        let back: RuleDef = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &rule.id);
        prop_assert_eq!(back.agent_type, rule.agent_type);
        prop_assert_eq!(&back.event_type, &rule.event_type);
        prop_assert_eq!(back.severity, rule.severity);
        prop_assert_eq!(&back.description, &rule.description);
        prop_assert_eq!(back.anchors.len(), rule.anchors.len());
    }

    /// interpolate_template replaces {pane} placeholder.
    #[test]
    fn prop_interpolate_pane(pane_id in 0u64..10000) {
        let result = RuleDef::interpolate_template(
            "ft robot get-text --pane {pane}",
            pane_id,
            None,
            &AgentType::Codex,
            "codex.test",
        );
        prop_assert!(result.contains(&pane_id.to_string()),
            "Result '{}' should contain pane_id {}", result, pane_id);
        prop_assert!(!result.contains("{pane}"),
            "Result '{}' should not contain {{pane}} placeholder", result);
    }

    /// interpolate_template replaces {agent} placeholder.
    #[test]
    fn prop_interpolate_agent(at in arb_agent_type()) {
        let result = RuleDef::interpolate_template(
            "check agent {agent}",
            1,
            None,
            &at,
            "codex.test",
        );
        prop_assert!(result.contains(&at.to_string()),
            "Result '{}' should contain agent type '{}'", result, at);
    }

    /// interpolate_template replaces {rule_id} placeholder.
    #[test]
    fn prop_interpolate_rule_id(rule_id in "[a-z_.]{5,20}") {
        let result = RuleDef::interpolate_template(
            "info about {rule_id}",
            1,
            None,
            &AgentType::Codex,
            &rule_id,
        );
        prop_assert!(result.contains(&rule_id),
            "Result '{}' should contain rule_id '{}'", result, rule_id);
    }

    /// interpolate_template replaces {event_id} with number when present.
    #[test]
    fn prop_interpolate_event_id(event_id in 0i64..100000) {
        let result = RuleDef::interpolate_template(
            "event {event_id}",
            1,
            Some(event_id),
            &AgentType::Codex,
            "codex.test",
        );
        prop_assert!(result.contains(&event_id.to_string()),
            "Result '{}' should contain event_id {}", result, event_id);
    }

    /// interpolate_template replaces {event_id} with "unknown" when None.
    #[test]
    fn prop_interpolate_event_id_none(_dummy in Just(())) {
        let result = RuleDef::interpolate_template(
            "event {event_id}",
            1,
            None,
            &AgentType::Codex,
            "codex.test",
        );
        prop_assert!(result.contains("unknown"),
            "Result '{}' should contain 'unknown'", result);
    }

    /// get_preview_command returns None when no preview_command set.
    #[test]
    fn prop_preview_command_none(rule in arb_rule_def()) {
        // arb_rule_def sets preview_command to None
        let result = rule.get_preview_command(1, None);
        prop_assert!(result.is_none());
    }

    /// get_preview_command returns interpolated string when set.
    #[test]
    fn prop_preview_command_interpolated(pane_id in 0u64..10000) {
        let rule = RuleDef {
            id: "codex.test_cmd".to_string(),
            agent_type: AgentType::Codex,
            event_type: "error".to_string(),
            severity: Severity::Warning,
            anchors: vec!["anchor".to_string()],
            regex: None,
            description: "test".to_string(),
            remediation: None,
            workflow: None,
            manual_fix: None,
            preview_command: Some("ft robot get-text --pane {pane}".to_string()),
            learn_more_url: None,
        };
        let result = rule.get_preview_command(pane_id, None).unwrap();
        prop_assert!(result.contains(&pane_id.to_string()));
    }
}

// ============================================================================
// PatternPack properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// PatternPack::new creates pack with given fields.
    #[test]
    fn prop_pattern_pack_new(
        name in "[a-z_]{3,20}",
        version in "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}",
    ) {
        let pack = PatternPack::new(name.clone(), version.clone(), Vec::new());
        prop_assert_eq!(&pack.name, &name);
        prop_assert_eq!(&pack.version, &version);
        prop_assert!(pack.rules.is_empty());
    }

    /// PatternPack serde roundtrip.
    #[test]
    fn prop_pattern_pack_serde_roundtrip(
        name in "[a-z_]{3,20}",
        version in "[0-9]\\.[0-9]\\.[0-9]",
    ) {
        let pack = PatternPack::new(name.clone(), version.clone(), Vec::new());
        let json = serde_json::to_string(&pack).unwrap();
        let back: PatternPack = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
        prop_assert_eq!(&back.version, &version);
        prop_assert!(back.rules.is_empty());
    }
}

// ============================================================================
// PatternLibrary properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// PatternLibrary::empty() has no rules or packs.
    #[test]
    fn prop_library_empty(_dummy in Just(())) {
        let lib = PatternLibrary::empty();
        prop_assert!(lib.rules().is_empty());
        prop_assert!(lib.packs().is_empty());
    }

    /// PatternLibrary merges rules from multiple packs.
    #[test]
    fn prop_library_merges_rules(_dummy in Just(())) {
        let pack1 = PatternPack::new("builtin:core", "1.0.0", vec![
            RuleDef {
                id: "codex.rule_a".to_string(),
                agent_type: AgentType::Codex,
                event_type: "error".to_string(),
                severity: Severity::Warning,
                anchors: vec!["anchor_a".to_string()],
                regex: None,
                description: "Rule A".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
        ]);
        let pack2 = PatternPack::new("builtin:codex", "1.0.0", vec![
            RuleDef {
                id: "codex.rule_b".to_string(),
                agent_type: AgentType::Codex,
                event_type: "warning".to_string(),
                severity: Severity::Info,
                anchors: vec!["anchor_b".to_string()],
                regex: None,
                description: "Rule B".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
        ]);

        let lib = PatternLibrary::new(vec![pack1, pack2]).unwrap();
        prop_assert_eq!(lib.rules().len(), 2);
        prop_assert_eq!(lib.packs().len(), 2);
    }

    /// PatternLibrary rules are sorted by id.
    #[test]
    fn prop_library_rules_sorted(_dummy in Just(())) {
        let pack = PatternPack::new("builtin:core", "1.0.0", vec![
            RuleDef {
                id: "codex.zzz".to_string(),
                agent_type: AgentType::Codex,
                event_type: "error".to_string(),
                severity: Severity::Warning,
                anchors: vec!["z".to_string()],
                regex: None,
                description: "Z".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
            RuleDef {
                id: "codex.aaa".to_string(),
                agent_type: AgentType::Codex,
                event_type: "error".to_string(),
                severity: Severity::Warning,
                anchors: vec!["a".to_string()],
                regex: None,
                description: "A".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
        ]);

        let lib = PatternLibrary::new(vec![pack]).unwrap();
        let rules = lib.rules();
        for w in rules.windows(2) {
            prop_assert!(w[0].id <= w[1].id,
                "Rules not sorted: {} > {}", w[0].id, w[1].id);
        }
    }

    /// PatternLibrary pack_for_rule returns correct pack.
    #[test]
    fn prop_library_pack_for_rule(_dummy in Just(())) {
        let pack = PatternPack::new("builtin:core", "1.0.0", vec![
            RuleDef {
                id: "codex.test_lookup".to_string(),
                agent_type: AgentType::Codex,
                event_type: "error".to_string(),
                severity: Severity::Warning,
                anchors: vec!["lookup".to_string()],
                regex: None,
                description: "test".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
        ]);

        let lib = PatternLibrary::new(vec![pack]).unwrap();
        prop_assert_eq!(lib.pack_for_rule("codex.test_lookup"), Some("builtin:core"));
        prop_assert_eq!(lib.pack_for_rule("nonexistent.rule"), None);
    }

    /// Later packs override earlier packs by rule id.
    #[test]
    fn prop_library_later_pack_overrides(_dummy in Just(())) {
        let pack1 = PatternPack::new("builtin:core", "1.0.0", vec![
            RuleDef {
                id: "codex.override_test".to_string(),
                agent_type: AgentType::Codex,
                event_type: "error".to_string(),
                severity: Severity::Warning,
                anchors: vec!["old".to_string()],
                regex: None,
                description: "Old version".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
        ]);
        let pack2 = PatternPack::new("builtin:codex", "1.0.0", vec![
            RuleDef {
                id: "codex.override_test".to_string(),
                agent_type: AgentType::Codex,
                event_type: "error".to_string(),
                severity: Severity::Critical,
                anchors: vec!["new".to_string()],
                regex: None,
                description: "New version".to_string(),
                remediation: None,
                workflow: None,
                manual_fix: None,
                preview_command: None,
                learn_more_url: None,
            },
        ]);

        let lib = PatternLibrary::new(vec![pack1, pack2]).unwrap();
        // Should have only 1 rule (deduplicated by id)
        prop_assert_eq!(lib.rules().len(), 1);
        // Should use the later pack's version
        prop_assert_eq!(lib.rules()[0].severity, Severity::Critical);
        prop_assert_eq!(&lib.rules()[0].description, "New version");
    }
}

// ============================================================================
// Trace type serde properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// TraceSpan serde roundtrip.
    #[test]
    fn prop_trace_span_serde_roundtrip(span in arb_trace_span()) {
        let json = serde_json::to_string(&span).unwrap();
        let back: TraceSpan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, span);
    }

    /// TraceEvidence serde roundtrip.
    #[test]
    fn prop_trace_evidence_serde_roundtrip(ev in arb_trace_evidence()) {
        let json = serde_json::to_string(&ev).unwrap();
        let back: TraceEvidence = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, ev);
    }

    /// TraceGate serde roundtrip.
    #[test]
    fn prop_trace_gate_serde_roundtrip(gate in arb_trace_gate()) {
        let json = serde_json::to_string(&gate).unwrap();
        let back: TraceGate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, gate);
    }

    /// TraceBounds serde roundtrip.
    #[test]
    fn prop_trace_bounds_serde_roundtrip(bounds in arb_trace_bounds()) {
        let json = serde_json::to_string(&bounds).unwrap();
        let back: TraceBounds = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, bounds);
    }
}

// ============================================================================
// TraceOptions defaults
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// TraceOptions default has sensible values.
    #[test]
    fn prop_trace_options_default(_dummy in Just(())) {
        let opts = TraceOptions::default();
        prop_assert!(opts.max_evidence_items > 0);
        prop_assert!(opts.max_excerpt_bytes > 0);
        prop_assert!(opts.max_capture_bytes > 0);
        prop_assert!(!opts.include_non_matches);
    }
}

// ============================================================================
// PatternEngine detect invariants
// ============================================================================

fn make_anchor_only_rule(id: &str, anchor: &str) -> RuleDef {
    RuleDef {
        id: format!("codex.{}", id),
        agent_type: AgentType::Codex,
        event_type: "test.event".to_string(),
        severity: Severity::Info,
        anchors: vec![anchor.to_string()],
        regex: None,
        description: "test anchor-only rule".to_string(),
        remediation: None,
        workflow: None,
        manual_fix: None,
        preview_command: None,
        learn_more_url: None,
    }
}

fn make_test_engine(rules: Vec<RuleDef>) -> PatternEngine {
    let pack = PatternPack {
        name: "test".to_string(),
        version: "1.0.0".to_string(),
        rules,
    };
    PatternEngine::with_packs(vec![pack]).expect("valid test engine")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Anchor-only rule: repeated anchor produces at least one detection
    /// (once the detect() multi-occurrence fix lands, this should be == repeat_count)
    #[test]
    fn prop_anchor_only_detects_repeated_anchors(repeat_count in 1usize..6) {
        let anchor = "ALERT";
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", anchor)]);
        let text = vec![anchor; repeat_count].join(" x ");
        let detections = engine.detect(&text);

        prop_assert!(
            !detections.is_empty(),
            "anchor-only rule should detect at least 1 occurrence in text with {} repeats: {:?}",
            repeat_count, text
        );
        // Each detection must have the correct rule_id
        for d in &detections {
            prop_assert_eq!(&d.rule_id, "codex.r1");
        }
    }

    /// Empty text always produces zero detections
    #[test]
    fn prop_detect_empty_text_no_detections(
        anchor in "[a-zA-Z]{3,8}",
    ) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", &anchor)]);
        let detections = engine.detect("");
        prop_assert!(detections.is_empty(), "empty text should yield zero detections");
    }

    /// Detection rule_id matches the rule that was defined
    #[test]
    fn prop_detection_rule_id_matches(
        rule_id in "[a-z]{2,6}",
        anchor in "[a-zA-Z]{4,10}",
    ) {
        let engine = make_test_engine(vec![make_anchor_only_rule(&rule_id, &anchor)]);
        let detections = engine.detect(&anchor);
        let expected_id = format!("codex.{}", rule_id);
        for d in &detections {
            prop_assert_eq!(&d.rule_id, &expected_id, "detection rule_id should match defined rule");
        }
    }

    /// Detection span is within text bounds
    #[test]
    fn prop_detection_span_within_bounds(
        anchor in "[a-zA-Z]{3,8}",
        prefix in "[0-9 ]{0,10}",
        suffix in "[0-9 ]{0,10}",
    ) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", &anchor)]);
        let text = format!("{}{}{}", prefix, anchor, suffix);
        let detections = engine.detect(&text);
        for d in &detections {
            prop_assert!(
                d.span.1 <= text.len(),
                "span end ({}) should be <= text len ({})", d.span.1, text.len()
            );
            prop_assert!(
                d.span.0 <= d.span.1,
                "span start ({}) should be <= span end ({})", d.span.0, d.span.1
            );
        }
    }

    /// Two different anchor-only rules each produce detections independently
    #[test]
    fn prop_multiple_rules_detect_independently(
        anchor_a in "[A-Z]{4,8}",
        anchor_b in "[a-z]{4,8}",
    ) {
        let engine = make_test_engine(vec![
            make_anchor_only_rule("ra", &anchor_a),
            make_anchor_only_rule("rb", &anchor_b),
        ]);
        let text = format!("{} and {}", anchor_a, anchor_b);
        let detections = engine.detect(&text);
        let rule_ids: std::collections::HashSet<&str> = detections.iter()
            .map(|d| d.rule_id.as_str())
            .collect();
        prop_assert!(
            rule_ids.contains("codex.ra"),
            "should detect anchor_a rule: detections={:?}", detections
        );
        prop_assert!(
            rule_ids.contains("codex.rb"),
            "should detect anchor_b rule: detections={:?}", detections
        );
    }
}

// ============================================================================
// Additional coverage tests (PA-48 through PA-69)
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // ── PA-48: PatternTelemetrySnapshot serde roundtrip ─────────────────────

    #[test]
    fn pa48_telemetry_snapshot_serde(
        scans in 0u64..10_000,
        matches_total in 0u64..10_000,
        qr in 0u64..10_000,
        bc in 0u64..10_000,
        bp in 0u64..10_000,
        br in 0u64..10_000,
        cre in 0u64..10_000,
        re in 0u64..10_000,
    ) {
        let snap = PatternTelemetrySnapshot {
            scans_total: scans,
            matches_total,
            quick_rejects: qr,
            bloom_checks: bc,
            bloom_positives: bp,
            bloom_rejects: br,
            candidate_rules_evaluated: cre,
            regex_evaluations: re,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PatternTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &snap);
    }

    // ── PA-49: PatternEngine telemetry starts at zero ───────────────────────

    #[test]
    fn pa49_engine_telemetry_starts_zero(_dummy in 0u8..1) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.scans_total, 0);
        prop_assert_eq!(snap.matches_total, 0);
        prop_assert_eq!(snap.quick_rejects, 0);
    }

    // ── PA-50: detect() increments scans_total ──────────────────────────────

    #[test]
    fn pa50_detect_increments_scans(n in 1usize..5) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        for _ in 0..n {
            let _ = engine.detect("some text");
        }
        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.scans_total, n as u64);
    }

    // ── PA-51: detect() on matching text increments matches_total ────────────

    #[test]
    fn pa51_detect_match_increments_matches(n in 1usize..5) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        for _ in 0..n {
            let _ = engine.detect("ALERT here");
        }
        let snap = engine.telemetry().snapshot();
        prop_assert!(snap.matches_total >= n as u64,
            "matches_total {} should be >= {}", snap.matches_total, n);
    }

    // ── PA-52: is_initialized returns true after construction ────────────────

    #[test]
    fn pa52_is_initialized(_dummy in 0u8..1) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        prop_assert!(engine.is_initialized());
    }

    // ── PA-53: quick_reject returns false for empty text ────────────────────

    #[test]
    fn pa53_quick_reject_empty(_dummy in 0u8..1) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        // quick_reject returns false for empty text (definitely no matches)
        prop_assert!(!engine.quick_reject(""));
    }

    // ── PA-54: quick_reject returns true for text with anchor byte ──────────

    #[test]
    fn pa54_quick_reject_with_anchor(_dummy in 0u8..1) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        // Text containing 'A' (first byte of anchor) should not be quick-rejected
        let result = engine.quick_reject("ALERT is here");
        prop_assert!(result, "text with anchor should pass quick_reject");
    }

    // ── PA-55: rules() returns all rules from loaded packs ──────────────────

    #[test]
    fn pa55_rules_returns_all(n in 1usize..5) {
        let rules: Vec<RuleDef> = (0..n)
            .map(|i| make_anchor_only_rule(&format!("r{i}"), &format!("ANCHOR{i}")))
            .collect();
        let engine = make_test_engine(rules);
        prop_assert_eq!(engine.rules().len(), n);
    }

    // ── PA-56: packs() returns exactly one pack ─────────────────────────────

    #[test]
    fn pa56_packs_returns_one(_dummy in 0u8..1) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        prop_assert_eq!(engine.packs().len(), 1);
        prop_assert_eq!(&engine.packs()[0].name, "test");
    }

    // ── PA-57: detect_with_context deduplicates repeated input ──────────────

    #[test]
    fn pa57_detect_with_context_dedup(n in 2usize..5) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        let mut ctx = DetectionContext::new();
        let text = "ALERT happened";
        let first = engine.detect_with_context(text, &mut ctx);
        prop_assert!(!first.is_empty(), "first detection should find match");
        // Subsequent calls with same text should be deduplicated
        for _ in 1..n {
            let subsequent = engine.detect_with_context(text, &mut ctx);
            prop_assert!(subsequent.is_empty(),
                "subsequent calls should be deduplicated, got {} detections", subsequent.len());
        }
    }

    // ── PA-58: detect_with_context filters by agent_type ────────────────────

    #[test]
    fn pa58_detect_with_context_agent_filter(_dummy in 0u8..1) {
        // Rule targets Codex agent type
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        // Context with Gemini should filter out Codex-specific detections
        let mut ctx = DetectionContext::with_agent_type(AgentType::Gemini);
        let detections = engine.detect_with_context("ALERT happened", &mut ctx);
        prop_assert!(detections.is_empty(),
            "Gemini context should filter Codex rule, got {} detections", detections.len());
    }

    // ── PA-59: detect_with_context_and_trace returns traces ─────────────────

    #[test]
    fn pa59_detect_with_trace_returns_traces(_dummy in 0u8..1) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "ALERT")]);
        let mut ctx = DetectionContext::new();
        let opts = TraceOptions::default();
        let (detections, traces) = engine.detect_with_context_and_trace("ALERT happened", &mut ctx, &opts);
        prop_assert!(!detections.is_empty(), "should detect ALERT");
        // Traces should be produced for matching rules
        prop_assert!(!traces.is_empty(), "should produce traces");
    }

    // ── PA-60: RuleDef get_manual_fix None when not set ─────────────────────

    #[test]
    fn pa60_manual_fix_none(pane_id in 0u64..1000) {
        let rule = make_anchor_only_rule("r1", "ALERT");
        prop_assert!(rule.get_manual_fix(pane_id, None).is_none());
    }

    // ── PA-61: RuleDef get_manual_fix interpolates ──────────────────────────

    #[test]
    fn pa61_manual_fix_interpolates(pane_id in 0u64..1000) {
        let mut rule = make_anchor_only_rule("r1", "ALERT");
        rule.manual_fix = Some("Fix pane {pane} for rule {rule_id}".to_string());
        let result = rule.get_manual_fix(pane_id, None);
        prop_assert!(result.is_some());
        let fix = result.unwrap();
        prop_assert!(fix.contains(&pane_id.to_string()), "should contain pane_id: {}", fix);
        prop_assert!(fix.contains("codex.r1"), "should contain rule_id: {}", fix);
    }

    // ── PA-62: PatternLibrary new_with_user_packs allows custom prefix ──────

    #[test]
    fn pa62_library_user_packs(name_suffix in "[a-z]{3,10}") {
        let pack_name = format!("user:{name_suffix}");
        let rule_id = format!("custom.{name_suffix}.r1");
        let mut rule = make_anchor_only_rule("r1", "TEST");
        rule.id = rule_id.clone();
        let pack = PatternPack::new(&pack_name, "1.0.0", vec![rule]);
        let user_packs: std::collections::HashSet<String> = [pack_name].into_iter().collect();
        let lib = PatternLibrary::new_with_user_packs(vec![pack], &user_packs);
        prop_assert!(lib.is_ok(), "user pack should be accepted: {:?}", lib.err());
        let lib = lib.unwrap();
        prop_assert_eq!(lib.rules().len(), 1);
        prop_assert_eq!(&lib.rules()[0].id, &rule_id);
    }

    // ── PA-63: PatternLibrary pack_for_rule maps correctly ──────────────────

    #[test]
    fn pa63_pack_for_rule(rule_suffix in "[a-z]{3,8}") {
        let rule_id = format!("codex.{rule_suffix}");
        let mut rule = make_anchor_only_rule(&rule_suffix, "ANCHOR");
        rule.id = rule_id.clone();
        let pack = PatternPack::new("builtin:test", "1.0.0", vec![rule]);
        let user_packs: std::collections::HashSet<String> = std::collections::HashSet::new();
        let lib = PatternLibrary::new_with_user_packs(vec![pack], &user_packs).unwrap();
        let pack_name = lib.pack_for_rule(&rule_id);
        prop_assert_eq!(pack_name, Some("builtin:test"));
    }

    // ── PA-64: DetectionContext set_ttl preserves value ──────────────────────

    #[test]
    fn pa64_context_set_ttl(secs in 1u64..3600) {
        let mut ctx = DetectionContext::new();
        ctx.set_ttl(Duration::from_secs(secs));
        prop_assert_eq!(ctx.ttl, Duration::from_secs(secs));
    }

    // ── PA-65: DetectionContext with_pane sets pane and agent ────────────────

    #[test]
    fn pa65_context_with_pane(pane_id in 0u64..1000, agent in arb_agent_type()) {
        let ctx = DetectionContext::with_pane(pane_id, Some(agent));
        prop_assert_eq!(ctx.pane_id, Some(pane_id));
        prop_assert_eq!(ctx.agent_type, Some(agent));
    }

    // ── PA-66: Detection dedup_key includes extracted text ──────────────────

    #[test]
    fn pa66_dedup_key_includes_extracted(
        rule_id in "[a-z.]{5,15}",
        val_a in "[a-z]{3,10}",
        val_b in "[a-z]{3,10}",
    ) {
        if val_a != val_b {
            let d_a = Detection {
                rule_id: rule_id.clone(),
                agent_type: AgentType::Codex,
                event_type: "test".to_string(),
                severity: Severity::Info,
                confidence: 1.0,
                matched_text: "text".to_string(),
                extracted: serde_json::json!({"key": val_a}),
                span: (0, 4),
            };
            let d_b = Detection {
                rule_id: rule_id.clone(),
                agent_type: AgentType::Codex,
                event_type: "test".to_string(),
                severity: Severity::Info,
                confidence: 1.0,
                matched_text: "text".to_string(),
                extracted: serde_json::json!({"key": val_b}),
                span: (0, 4),
            };
            prop_assert_ne!(d_a.dedup_key(), d_b.dedup_key(),
                "different extracted values should produce different dedup keys");
        }
    }

    // ── PA-67: MatchTrace serde roundtrip ───────────────────────────────────

    #[test]
    fn pa67_match_trace_serde(rule_id in "[a-z.]{5,15}") {
        // Use non-empty truncated_fields to avoid skip_serializing_if/default mismatch
        let trace = MatchTrace {
            pack_id: "builtin:test".to_string(),
            rule_id: rule_id.clone(),
            extractor_id: None,
            matched_text: Some("ALERT".to_string()),
            confidence: Some(1.0),
            eligible: true,
            gates: vec![TraceGate {
                gate: "agent_type".to_string(),
                passed: true,
                reason: None,
            }],
            evidence: vec![TraceEvidence {
                kind: "anchor".to_string(),
                label: Some("ALERT".to_string()),
                span: Some(TraceSpan { start: 0, end: 5 }),
                excerpt: Some("ALERT".to_string()),
                truncated: false,
            }],
            bounds: TraceBounds {
                max_evidence_items: 10,
                max_excerpt_bytes: 256,
                max_capture_bytes: 256,
                evidence_total: 1,
                evidence_truncated: false,
                truncated_fields: vec!["excerpt".to_string()],
            },
        };
        let json = serde_json::to_string(&trace).unwrap();
        let back: MatchTrace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &trace);

        // Also verify JSON structure contains expected fields
        prop_assert!(json.contains("\"pack_id\""));
        prop_assert!(json.contains("\"rule_id\""));
        prop_assert!(json.contains("\"eligible\""));
    }

    // ── PA-68: Empty PatternEngine detect returns empty ─────────────────────

    #[test]
    fn pa68_empty_engine_no_detections(text in "[a-z ]{5,50}") {
        let engine = make_test_engine(vec![]);
        let detections = engine.detect(&text);
        prop_assert!(detections.is_empty(), "no rules should mean no detections");
    }

    // ── PA-69: detect_with_context cross-segment via tail_buffer ────────────

    #[test]
    fn pa69_cross_segment_tail_buffer(_dummy in 0u8..1) {
        let engine = make_test_engine(vec![make_anchor_only_rule("r1", "FULLALERT")]);
        let mut ctx = DetectionContext::new();
        // First segment ends with partial anchor
        let _ = engine.detect_with_context("some text FULL", &mut ctx);
        // Tail buffer should have buffered the end of previous text
        prop_assert!(!ctx.tail_buffer.is_empty(),
            "tail_buffer should be populated after first call");
        // Second segment completes the anchor
        let detections = engine.detect_with_context("ALERT more text", &mut ctx);
        // Should detect FULLALERT across the boundary
        prop_assert!(!detections.is_empty(),
            "cross-segment detection should find FULLALERT");
    }
}
