//! Property-based tests for the connector data classification module.
//!
//! Tests cover sensitivity ordering/semantics, redaction strategy mapping,
//! classification rule field/content matching, policy connector matching,
//! ingestion decision semantics, telemetry arithmetic, serde roundtrips,
//! and classified event analysis helpers.

use proptest::prelude::*;

use frankenterm_core::connector_data_classification::{
    ClassificationError, ClassificationPolicy, ClassificationRule, ClassificationTelemetry,
    ClassifiedEvent, ClassifierConfig, ConnectorDataClassifier, DataSensitivity,
    FieldClassification, IngestionDecision, RedactionStrategy,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_sensitivity() -> impl Strategy<Value = DataSensitivity> {
    prop_oneof![
        Just(DataSensitivity::Public),
        Just(DataSensitivity::Internal),
        Just(DataSensitivity::Confidential),
        Just(DataSensitivity::Restricted),
        Just(DataSensitivity::Prohibited),
    ]
}

fn arb_redaction_strategy() -> impl Strategy<Value = RedactionStrategy> {
    prop_oneof![
        Just(RedactionStrategy::Mask),
        Just(RedactionStrategy::Hash),
        (1usize..=256).prop_map(|max_len| RedactionStrategy::Truncate { max_len }),
        Just(RedactionStrategy::Remove),
        "[a-z]{2,6}-".prop_map(|prefix| RedactionStrategy::Tokenize {
            token_prefix: prefix
        }),
        Just(RedactionStrategy::Passthrough),
    ]
}

fn arb_field_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("password".to_string()),
        Just("secret".to_string()),
        Just("api_key".to_string()),
        Just("access_token".to_string()),
        Just("credential_id".to_string()),
        Just("email".to_string()),
        Just("event_type".to_string()),
        Just("timestamp".to_string()),
        Just("connector_id".to_string()),
        Just("message".to_string()),
        Just("body".to_string()),
        Just("command_output".to_string()),
        "[a-z_]{3,15}".prop_map(|s| s),
    ]
}

fn arb_connector_id() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("github-main".to_string()),
        Just("slack-ops".to_string()),
        Just("aws-prod".to_string()),
        "conn-[a-z]{3,8}".prop_map(|s| s),
    ]
}

fn arb_telemetry() -> impl Strategy<Value = ClassificationTelemetry> {
    (
        0u64..=500,
        0u64..=300,
        0u64..=200,
        0u64..=100,
        0u64..=50,
        0u64..=2000,
        0u64..=1000,
        0u64..=500,
        0u64..=50,
        0u64..=20,
        0u64..=500,
        0u64..=100,
    )
        .prop_map(|(ec, ea, ear, er, eq, fc, fr, fre, sd, pt, pl, pm)| {
            ClassificationTelemetry {
                events_classified: ec,
                events_accepted: ea,
                events_accepted_redacted: ear,
                events_rejected: er,
                events_quarantined: eq,
                fields_classified: fc,
                fields_redacted: fr,
                fields_removed: fre,
                secrets_detected: sd,
                payload_truncations: pt,
                policy_lookups: pl,
                policy_misses: pm,
            }
        })
}

fn arb_field_classification() -> impl Strategy<Value = FieldClassification> {
    (
        arb_field_name(),
        arb_sensitivity(),
        arb_redaction_strategy(),
    )
        .prop_map(|(path, sensitivity, strategy)| FieldClassification {
            field_path: path,
            sensitivity,
            matched_rule: "test-rule".to_string(),
            strategy,
        })
}

// =============================================================================
// DataSensitivity property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Sensitivity ordering is total.
    #[test]
    fn sensitivity_total_order(a in arb_sensitivity(), b in arb_sensitivity()) {
        prop_assert!(a <= b || b <= a);
    }

    /// Sensitivity ordering is transitive.
    #[test]
    fn sensitivity_transitive(
        a in arb_sensitivity(),
        b in arb_sensitivity(),
        c in arb_sensitivity(),
    ) {
        if a <= b && b <= c {
            prop_assert!(a <= c);
        }
    }

    /// as_str produces non-empty snake_case string.
    #[test]
    fn sensitivity_as_str_valid(s in arb_sensitivity()) {
        let str_val = s.as_str();
        prop_assert!(!str_val.is_empty());
        prop_assert!(str_val.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    }

    /// Display matches as_str.
    #[test]
    fn sensitivity_display_matches_as_str(s in arb_sensitivity()) {
        prop_assert_eq!(s.to_string(), s.as_str());
    }

    /// requires_redaction is true for Confidential, Restricted, Prohibited.
    #[test]
    fn sensitivity_redaction_threshold(s in arb_sensitivity()) {
        let expected = s >= DataSensitivity::Confidential;
        prop_assert_eq!(s.requires_redaction(), expected);
    }

    /// must_remove is true only for Prohibited.
    #[test]
    fn sensitivity_must_remove_only_prohibited(s in arb_sensitivity()) {
        let expected = s == DataSensitivity::Prohibited;
        prop_assert_eq!(s.must_remove(), expected);
    }

    /// Serde roundtrip.
    #[test]
    fn sensitivity_serde_roundtrip(s in arb_sensitivity()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: DataSensitivity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }
}

// =============================================================================
// RedactionStrategy property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// for_sensitivity returns Passthrough for Public and Internal.
    #[test]
    fn strategy_passthrough_for_low_sensitivity(
        s in prop_oneof![Just(DataSensitivity::Public), Just(DataSensitivity::Internal)]
    ) {
        let strategy = RedactionStrategy::for_sensitivity(s);
        let is_passthrough = matches!(strategy, RedactionStrategy::Passthrough);
        prop_assert!(is_passthrough);
    }

    /// for_sensitivity returns non-Passthrough for Confidential+.
    #[test]
    fn strategy_active_for_high_sensitivity(
        s in prop_oneof![
            Just(DataSensitivity::Confidential),
            Just(DataSensitivity::Restricted),
            Just(DataSensitivity::Prohibited),
        ]
    ) {
        let strategy = RedactionStrategy::for_sensitivity(s);
        let is_passthrough = matches!(strategy, RedactionStrategy::Passthrough);
        prop_assert!(!is_passthrough);
    }

    /// for_sensitivity(Prohibited) returns Remove.
    #[test]
    fn strategy_prohibited_is_remove(_dummy in 0u8..1) {
        let strategy = RedactionStrategy::for_sensitivity(DataSensitivity::Prohibited);
        let is_remove = matches!(strategy, RedactionStrategy::Remove);
        prop_assert!(is_remove);
    }

    /// Display produces non-empty strings.
    #[test]
    fn strategy_display_nonempty(s in arb_redaction_strategy()) {
        prop_assert!(!s.to_string().is_empty());
    }

    /// Serde roundtrip.
    #[test]
    fn strategy_serde_roundtrip(s in arb_redaction_strategy()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: RedactionStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }
}

// =============================================================================
// ClassificationRule matching property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Exact field pattern matches exactly.
    #[test]
    fn rule_exact_field_match(field in arb_field_name()) {
        let rule = ClassificationRule::new(
            "r1",
            DataSensitivity::Restricted,
            vec![field.clone()],
        );
        prop_assert!(rule.matches_field(&field));
    }

    /// Wildcard field pattern matches prefix.
    #[test]
    fn rule_wildcard_field_matches_prefix(prefix in "[a-z]{2,8}", suffix in "[a-z]{1,5}") {
        let rule = ClassificationRule::new(
            "r1",
            DataSensitivity::Restricted,
            vec![format!("{prefix}*")],
        );
        let field_name = format!("{prefix}{suffix}");
        prop_assert!(rule.matches_field(&field_name));
    }

    /// Non-matching field pattern rejects.
    #[test]
    fn rule_non_matching_field_rejects(
        pattern in "[a-z]{4,8}",
        field in "[0-9]{4,8}",
    ) {
        let rule = ClassificationRule::new(
            "r1",
            DataSensitivity::Restricted,
            vec![pattern],
        );
        prop_assert!(!rule.matches_field(&field));
    }

    /// Empty content_patterns matches any content.
    #[test]
    fn rule_empty_content_matches_all(content in ".*") {
        let rule = ClassificationRule::new(
            "r1",
            DataSensitivity::Restricted,
            vec!["field".to_string()],
        );
        // No content_patterns = match all
        prop_assert!(rule.matches_content(&content));
    }

    /// Content pattern matches substring.
    #[test]
    fn rule_content_pattern_matches_substring(
        prefix in "[a-z]{2,5}",
        needle in "[a-z]{3,6}",
        suffix in "[a-z]{2,5}",
    ) {
        let mut rule = ClassificationRule::new(
            "r1",
            DataSensitivity::Restricted,
            vec!["field".to_string()],
        );
        rule.content_patterns = vec![needle.clone()];
        let content = format!("{prefix}{needle}{suffix}");
        prop_assert!(rule.matches_content(&content));
    }

    /// Content pattern rejects non-matching content.
    #[test]
    fn rule_content_pattern_rejects_missing(
        content in "[0-9]{10,20}",
    ) {
        let mut rule = ClassificationRule::new(
            "r1",
            DataSensitivity::Restricted,
            vec!["field".to_string()],
        );
        rule.content_patterns = vec!["zzz_never_match_zzz".to_string()];
        prop_assert!(!rule.matches_content(&content));
    }

    /// effective_strategy uses override when set.
    #[test]
    fn rule_effective_strategy_override(
        sensitivity in arb_sensitivity(),
        override_strategy in arb_redaction_strategy(),
    ) {
        let mut rule = ClassificationRule::new(
            "r1",
            sensitivity,
            vec!["field".to_string()],
        );
        rule.redaction_override = Some(override_strategy.clone());
        prop_assert_eq!(rule.effective_strategy(), override_strategy);
    }

    /// effective_strategy defaults to for_sensitivity when no override.
    #[test]
    fn rule_effective_strategy_default(sensitivity in arb_sensitivity()) {
        let rule = ClassificationRule::new(
            "r1",
            sensitivity,
            vec!["field".to_string()],
        );
        let expected = RedactionStrategy::for_sensitivity(sensitivity);
        prop_assert_eq!(rule.effective_strategy(), expected);
    }
}

// =============================================================================
// ClassificationPolicy connector matching property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Wildcard "*" policy matches any connector.
    #[test]
    fn wildcard_policy_matches_all(connector in arb_connector_id()) {
        let policy = ClassificationPolicy::default(); // connector_pattern = "*"
        prop_assert!(policy.matches_connector(&connector));
    }

    /// Exact connector pattern matches only that connector.
    #[test]
    fn exact_policy_matches_only_exact(target in arb_connector_id()) {
        let policy = ClassificationPolicy {
            connector_pattern: "github-main".to_string(),
            ..ClassificationPolicy::default()
        };
        let expected = target == "github-main";
        prop_assert_eq!(policy.matches_connector(&target), expected);
    }

    /// Prefix wildcard matches connectors starting with prefix.
    #[test]
    fn prefix_policy_matches_prefix(
        prefix in "[a-z]{3,6}",
        suffix in "[a-z]{1,5}",
    ) {
        let policy = ClassificationPolicy {
            connector_pattern: format!("{prefix}*"),
            ..ClassificationPolicy::default()
        };
        let connector = format!("{prefix}{suffix}");
        prop_assert!(policy.matches_connector(&connector));
    }

    /// Prefix wildcard rejects non-matching connectors.
    #[test]
    fn prefix_policy_rejects_non_match(
        prefix in "[a-z]{3,6}",
    ) {
        let policy = ClassificationPolicy {
            connector_pattern: format!("{prefix}*"),
            ..ClassificationPolicy::default()
        };
        prop_assert!(!policy.matches_connector("zzz-never-match"));
    }
}

// =============================================================================
// IngestionDecision property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Accept and AcceptRedacted are both accepted.
    #[test]
    fn accept_variants_accepted(
        variant in prop_oneof![
            Just(IngestionDecision::Accept),
            Just(IngestionDecision::AcceptRedacted),
        ]
    ) {
        prop_assert!(variant.is_accepted());
        prop_assert!(!variant.is_rejected());
    }

    /// Reject is rejected and not accepted.
    #[test]
    fn reject_not_accepted(reason in "[a-z ]{5,30}") {
        let decision = IngestionDecision::Reject { reason };
        prop_assert!(decision.is_rejected());
        prop_assert!(!decision.is_accepted());
    }

    /// Quarantine is neither accepted nor rejected.
    #[test]
    fn quarantine_neither(reason in "[a-z ]{5,30}") {
        let decision = IngestionDecision::Quarantine { reason };
        prop_assert!(!decision.is_accepted());
        prop_assert!(!decision.is_rejected());
    }

    /// Display produces non-empty string.
    #[test]
    fn ingestion_display_nonempty(
        variant in prop_oneof![
            Just(IngestionDecision::Accept),
            Just(IngestionDecision::AcceptRedacted),
            "[a-z ]{5,15}".prop_map(|r| IngestionDecision::Reject { reason: r }),
            "[a-z ]{5,15}".prop_map(|r| IngestionDecision::Quarantine { reason: r }),
        ]
    ) {
        prop_assert!(!variant.to_string().is_empty());
    }

    /// Serde roundtrip.
    #[test]
    fn ingestion_serde_roundtrip(
        variant in prop_oneof![
            Just(IngestionDecision::Accept),
            Just(IngestionDecision::AcceptRedacted),
            "[a-z]{5,15}".prop_map(|r| IngestionDecision::Reject { reason: r }),
            "[a-z]{5,15}".prop_map(|r| IngestionDecision::Quarantine { reason: r }),
        ]
    ) {
        let json = serde_json::to_string(&variant).unwrap();
        let back: IngestionDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(variant, back);
    }
}

// =============================================================================
// ClassificationTelemetry property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// total_events equals sum of accepted + accepted_redacted + rejected + quarantined.
    #[test]
    fn telemetry_total_events_correct(t in arb_telemetry()) {
        let expected = t.events_accepted + t.events_accepted_redacted + t.events_rejected + t.events_quarantined;
        prop_assert_eq!(t.total_events(), expected);
    }

    /// redaction_rate is 0 when fields_classified is 0.
    #[test]
    fn telemetry_redaction_rate_zero_classified(_dummy in 0u8..1) {
        let t = ClassificationTelemetry {
            fields_classified: 0,
            fields_redacted: 5,
            fields_removed: 3,
            ..ClassificationTelemetry::default()
        };
        let rate = t.redaction_rate();
        prop_assert!((rate - 0.0).abs() < f64::EPSILON);
    }

    /// redaction_rate is in [0.0, 1.0] when data is consistent.
    #[test]
    fn telemetry_redaction_rate_bounded(
        classified in 1u64..=1000,
        redacted in 0u64..=500,
        removed in 0u64..=500,
    ) {
        let total_redact = redacted + removed;
        // Ensure consistent: redacted+removed <= classified
        let classified = classified.max(total_redact);
        let t = ClassificationTelemetry {
            fields_classified: classified,
            fields_redacted: redacted,
            fields_removed: removed,
            ..ClassificationTelemetry::default()
        };
        let rate = t.redaction_rate();
        prop_assert!(rate >= 0.0);
        prop_assert!(rate <= 1.0 + f64::EPSILON);
    }

    /// Serde roundtrip.
    #[test]
    fn telemetry_serde_roundtrip(t in arb_telemetry()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: ClassificationTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, back);
    }
}

// =============================================================================
// ClassifiedEvent analysis helpers property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// has_prohibited is true iff any field is Prohibited.
    #[test]
    fn classified_event_has_prohibited(
        fields in prop::collection::vec(arb_field_classification(), 1..=10),
    ) {
        let expected = fields.iter().any(|f| f.sensitivity == DataSensitivity::Prohibited);
        let event = ClassifiedEvent {
            event_id: "e1".to_string(),
            connector_id: "c1".to_string(),
            overall_sensitivity: DataSensitivity::Public,
            field_classifications: fields,
            policy_id: "p1".to_string(),
            secrets_detected: false,
            classified_at_ms: 1000,
        };
        prop_assert_eq!(event.has_prohibited(), expected);
    }

    /// requires_redaction is true iff any field has Confidential+ sensitivity.
    #[test]
    fn classified_event_requires_redaction(
        fields in prop::collection::vec(arb_field_classification(), 1..=10),
    ) {
        let expected = fields.iter().any(|f| f.sensitivity.requires_redaction());
        let event = ClassifiedEvent {
            event_id: "e1".to_string(),
            connector_id: "c1".to_string(),
            overall_sensitivity: DataSensitivity::Public,
            field_classifications: fields,
            policy_id: "p1".to_string(),
            secrets_detected: false,
            classified_at_ms: 1000,
        };
        prop_assert_eq!(event.requires_redaction(), expected);
    }

    /// sensitivity_histogram counts match field list.
    #[test]
    fn classified_event_histogram_accurate(
        fields in prop::collection::vec(arb_field_classification(), 1..=15),
    ) {
        let event = ClassifiedEvent {
            event_id: "e1".to_string(),
            connector_id: "c1".to_string(),
            overall_sensitivity: DataSensitivity::Public,
            field_classifications: fields.clone(),
            policy_id: "p1".to_string(),
            secrets_detected: false,
            classified_at_ms: 1000,
        };
        let hist = event.sensitivity_histogram();
        let total: usize = hist.values().sum();
        prop_assert_eq!(total, fields.len());
    }
}

// =============================================================================
// Classifier policy management property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Registering N policies yields N policy_count.
    #[test]
    fn classifier_register_n_policies(n in 1usize..=10) {
        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        for i in 0..n {
            let policy = ClassificationPolicy {
                policy_id: format!("p{i}"),
                connector_pattern: format!("conn-{i}"),
                ..ClassificationPolicy::default()
            };
            classifier.register_policy(policy);
        }
        prop_assert_eq!(classifier.policy_count(), n);
    }

    /// Exact connector policies are found before wildcard.
    #[test]
    fn classifier_exact_before_wildcard(connector in arb_connector_id()) {
        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        // Register wildcard first
        classifier.register_policy(ClassificationPolicy {
            policy_id: "wildcard".to_string(),
            connector_pattern: "*".to_string(),
            ..ClassificationPolicy::default()
        });
        // Register exact match
        classifier.register_policy(ClassificationPolicy {
            policy_id: "exact".to_string(),
            connector_pattern: connector.clone(),
            ..ClassificationPolicy::default()
        });
        let found = classifier.find_policy(&connector);
        let found_ok = found.is_some();
        prop_assert!(found_ok);
        // Exact match should win (inserted before wildcard)
        prop_assert_eq!(&found.unwrap().policy_id, "exact");
    }

    /// Wildcard policy is found when no exact match.
    #[test]
    fn classifier_wildcard_fallback(_dummy in 0u8..1) {
        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        classifier.register_policy(ClassificationPolicy::default());
        let found = classifier.find_policy("anything-at-all");
        let found_ok = found.is_some();
        prop_assert!(found_ok);
    }

    /// No policy registered means find_policy returns None.
    #[test]
    fn classifier_no_policy_returns_none(connector in arb_connector_id()) {
        let classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        prop_assert!(classifier.find_policy(&connector).is_none());
    }
}

// =============================================================================
// ClassifierConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// ClassifierConfig serde roundtrip.
    #[test]
    fn classifier_config_serde_roundtrip(
        max_entries in 100usize..=50000,
        marker in "[A-Z_]{5,20}",
        salt in "[a-z0-9]{5,15}",
        detailed in any::<bool>(),
    ) {
        let config = ClassifierConfig {
            max_audit_entries: max_entries,
            redaction_marker: marker,
            hash_salt: salt,
            detailed_audit: detailed,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ClassifierConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.max_audit_entries, back.max_audit_entries);
        prop_assert_eq!(config.redaction_marker, back.redaction_marker);
        prop_assert_eq!(config.hash_salt, back.hash_salt);
        prop_assert_eq!(config.detailed_audit, back.detailed_audit);
    }
}

// =============================================================================
// Error display property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// All error variants produce non-empty Display messages.
    #[test]
    fn error_display_nonempty(id in "[a-z]{3,10}", size in 1usize..=10000) {
        let errors = vec![
            ClassificationError::NoPolicyFound { connector_id: id.clone() },
            ClassificationError::PayloadTooLarge { size, max: size / 2 },
            ClassificationError::Rejected { reason: id.clone() },
            ClassificationError::Internal { reason: id },
        ];
        for e in &errors {
            let msg = e.to_string();
            prop_assert!(!msg.is_empty());
        }
    }
}

// =============================================================================
// Default policy has builtin rules
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default policy classifies known secret fields as Prohibited.
    #[test]
    fn default_policy_catches_secrets(
        field in prop_oneof![
            Just("password"),
            Just("secret"),
            Just("private_key"),
            Just("api_key"),
            Just("access_token"),
            Just("refresh_token"),
        ]
    ) {
        let policy = ClassificationPolicy::default();
        let found = policy.rules.iter().any(|r| {
            r.enabled && r.matches_field(field) && r.sensitivity == DataSensitivity::Prohibited
        });
        prop_assert!(found, "default policy should classify '{}' as Prohibited", field);
    }

    /// Default policy classifies structural fields as Public.
    #[test]
    fn default_policy_structural_public(
        field in prop_oneof![
            Just("event_type"),
            Just("event_id"),
            Just("connector_id"),
            Just("pane_id"),
            Just("severity"),
        ]
    ) {
        let policy = ClassificationPolicy::default();
        let found = policy.rules.iter().any(|r| {
            r.enabled && r.matches_field(field) && r.sensitivity == DataSensitivity::Public
        });
        prop_assert!(found, "default policy should classify '{}' as Public", field);
    }
}

// =============================================================================
// Ingestion decision from classifier
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Events with prohibited fields are rejected when allow_prohibited=false.
    #[test]
    fn ingestion_rejects_prohibited(_dummy in 0u8..1) {
        let classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        let event = ClassifiedEvent {
            event_id: "e1".to_string(),
            connector_id: "c1".to_string(),
            overall_sensitivity: DataSensitivity::Prohibited,
            field_classifications: vec![FieldClassification {
                field_path: "password".to_string(),
                sensitivity: DataSensitivity::Prohibited,
                matched_rule: "builtin-secrets".to_string(),
                strategy: RedactionStrategy::Remove,
            }],
            policy_id: "default".to_string(),
            secrets_detected: false,
            classified_at_ms: 1000,
        };
        let policy = ClassificationPolicy::default(); // allow_prohibited=false
        let decision = classifier.ingestion_decision(&event, &policy);
        prop_assert!(decision.is_rejected());
    }

    /// Events with prohibited fields are accepted when allow_prohibited=true.
    #[test]
    fn ingestion_accepts_prohibited_when_allowed(_dummy in 0u8..1) {
        let classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        let event = ClassifiedEvent {
            event_id: "e1".to_string(),
            connector_id: "c1".to_string(),
            overall_sensitivity: DataSensitivity::Prohibited,
            field_classifications: vec![FieldClassification {
                field_path: "password".to_string(),
                sensitivity: DataSensitivity::Prohibited,
                matched_rule: "builtin-secrets".to_string(),
                strategy: RedactionStrategy::Remove,
            }],
            policy_id: "default".to_string(),
            secrets_detected: false,
            classified_at_ms: 1000,
        };
        let mut policy = ClassificationPolicy::default();
        policy.allow_prohibited = true;
        let decision = classifier.ingestion_decision(&event, &policy);
        // Prohibited + allow_prohibited + requires_redaction → AcceptRedacted
        let is_accept_redacted = matches!(decision, IngestionDecision::AcceptRedacted);
        prop_assert!(is_accept_redacted);
    }

    /// All-public events are accepted without redaction.
    #[test]
    fn ingestion_accepts_public_events(n in 1usize..=5) {
        let classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        let fields: Vec<FieldClassification> = (0..n)
            .map(|i| FieldClassification {
                field_path: format!("field_{i}"),
                sensitivity: DataSensitivity::Public,
                matched_rule: "default".to_string(),
                strategy: RedactionStrategy::Passthrough,
            })
            .collect();
        let event = ClassifiedEvent {
            event_id: "e1".to_string(),
            connector_id: "c1".to_string(),
            overall_sensitivity: DataSensitivity::Public,
            field_classifications: fields,
            policy_id: "default".to_string(),
            secrets_detected: false,
            classified_at_ms: 1000,
        };
        let policy = ClassificationPolicy::default();
        let decision = classifier.ingestion_decision(&event, &policy);
        let is_accept = matches!(decision, IngestionDecision::Accept);
        prop_assert!(is_accept);
    }
}

// =============================================================================
// classify_event end-to-end property tests
// =============================================================================

use frankenterm_core::connector_event_model::{CanonicalConnectorEvent, EventDirection};

fn make_test_event(connector_id: &str, payload: serde_json::Value) -> CanonicalConnectorEvent {
    CanonicalConnectorEvent {
        connector_id: connector_id.to_string(),
        event_type: "test.event".to_string(),
        event_id: "evt-001".to_string(),
        correlation_id: "corr-001".to_string(),
        payload,
        metadata: std::collections::BTreeMap::new(),
        ..CanonicalConnectorEvent::new(
            EventDirection::Inbound,
            connector_id,
            "test.event",
            serde_json::Value::Null,
        )
    }
}

fn setup_classifier_with_default_policy() -> ConnectorDataClassifier {
    let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
    classifier.register_policy(ClassificationPolicy::default());
    classifier
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// classify_event classifies structural fields (event_type, event_id, etc.).
    #[test]
    fn classify_event_includes_structural_fields(
        connector in arb_connector_id(),
    ) {
        let mut classifier = setup_classifier_with_default_policy();
        let event = make_test_event(&connector, serde_json::json!({"status": "ok"}));
        let classified = classifier.classify_event(&event).unwrap();

        // Should have at least 4 structural fields + payload fields
        prop_assert!(classified.field_classifications.len() >= 4);
        // Should include event_type, event_id, connector_id, correlation_id
        let paths: Vec<&str> = classified.field_classifications.iter().map(|f| f.field_path.as_str()).collect();
        prop_assert!(paths.contains(&"event_type"), "should classify event_type");
        prop_assert!(paths.contains(&"event_id"), "should classify event_id");
        prop_assert!(paths.contains(&"connector_id"), "should classify connector_id");
        prop_assert!(paths.contains(&"correlation_id"), "should classify correlation_id");
    }

    /// classify_event overall_sensitivity is max of all field sensitivities.
    #[test]
    fn classify_event_overall_is_max_sensitivity(
        connector in arb_connector_id(),
    ) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"safe_field": "hello", "message": "world"});
        let event = make_test_event(&connector, payload);
        let classified = classifier.classify_event(&event).unwrap();

        let max_field_sensitivity = classified.field_classifications
            .iter()
            .map(|f| f.sensitivity)
            .max()
            .unwrap_or(DataSensitivity::Public);
        prop_assert_eq!(classified.overall_sensitivity, max_field_sensitivity);
    }

    /// classify_event increments telemetry.events_classified.
    #[test]
    fn classify_event_increments_telemetry(n in 1usize..=5) {
        let mut classifier = setup_classifier_with_default_policy();
        for i in 0..n {
            let event = make_test_event("conn-1", serde_json::json!({"step": i}));
            classifier.classify_event(&event).unwrap();
        }
        prop_assert_eq!(classifier.telemetry().events_classified, n as u64);
    }

    /// classify_event with no matching policy returns NoPolicyFound.
    #[test]
    fn classify_event_no_policy_error(connector in arb_connector_id()) {
        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        // No policy registered
        let event = make_test_event(&connector, serde_json::json!({}));
        let result = classifier.classify_event(&event);
        let is_err = result.is_err();
        prop_assert!(is_err);
        let is_no_policy = matches!(result.unwrap_err(), ClassificationError::NoPolicyFound { .. });
        prop_assert!(is_no_policy);
    }

    /// classify_event counts fields_classified in telemetry.
    #[test]
    fn classify_event_fields_classified_telemetry(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"field_a": "val_a", "field_b": "val_b"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();

        let total_fields = classified.field_classifications.len() as u64;
        prop_assert_eq!(classifier.telemetry().fields_classified, total_fields);
    }

    /// classify_event with payload containing secret field names classifies them as Prohibited.
    #[test]
    fn classify_event_secret_fields_prohibited(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"password": "super-secret-123", "username": "admin"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();

        let password_fc = classified.field_classifications.iter()
            .find(|f| f.field_path == "payload.password");
        let pw_found = password_fc.is_some();
        prop_assert!(pw_found, "password field should be classified");
        prop_assert_eq!(password_fc.unwrap().sensitivity, DataSensitivity::Prohibited);
    }

    /// classify_event payload truncation detection increments telemetry.
    #[test]
    fn classify_event_payload_truncation_detected(_dummy in 0u8..1) {
        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        let mut policy = ClassificationPolicy::default();
        policy.max_payload_bytes = 10; // Very small limit
        classifier.register_policy(policy);

        let payload = serde_json::json!({"big_field": "a very long string that exceeds the tiny limit"});
        let event = make_test_event("conn-1", payload);
        classifier.classify_event(&event).unwrap();

        prop_assert_eq!(classifier.telemetry().payload_truncations, 1);
    }

    /// classify_event with nested JSON classifies nested fields.
    #[test]
    fn classify_event_nested_payload(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({
            "outer": {
                "password": "nested-secret"
            }
        });
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();

        // Should have classified the nested password field
        let has_nested = classified.field_classifications.iter()
            .any(|f| f.field_path.contains("password"));
        prop_assert!(has_nested, "should classify nested password field");
    }
}

// =============================================================================
// redact_event property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// redact_event on all-public fields produces no redaction actions.
    #[test]
    fn redact_all_public_no_actions(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"event_type": "test", "status": "ok"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();

        // If overall is public, no redaction needed
        if classified.overall_sensitivity <= DataSensitivity::Internal {
            let redacted = classifier.redact_event(&event, &classified);
            prop_assert_eq!(redacted.redaction_actions.len(), 0);
        }
    }

    /// redact_event removes prohibited payload fields.
    #[test]
    fn redact_removes_prohibited_payload(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"password": "my-secret", "safe": "hello"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();
        let redacted = classifier.redact_event(&event, &classified);

        // password should be removed from the payload
        let result_payload = &redacted.event.payload;
        let pw_present = result_payload.get("password").is_some();
        prop_assert!(!pw_present, "password should be removed from redacted payload");
        // safe field should remain
        let safe_present = result_payload.get("safe").is_some();
        prop_assert!(safe_present, "safe field should remain");
    }

    /// redact_event creates audit entry.
    #[test]
    fn redact_creates_audit_entry(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"password": "secret123"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();
        let _redacted = classifier.redact_event(&event, &classified);

        prop_assert_eq!(classifier.audit_log().len(), 1);
        let entry = &classifier.audit_log()[0];
        prop_assert_eq!(&entry.event_id, "evt-001");
        prop_assert_eq!(&entry.connector_id, "conn-1");
    }

    /// redact_event telemetry: fields_removed and fields_redacted track actions.
    #[test]
    fn redact_telemetry_tracks_actions(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"password": "secret", "api_key": "key123"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();
        let _redacted = classifier.redact_event(&event, &classified);

        // At least some fields should be removed/redacted
        let total_actions = classifier.telemetry().fields_redacted + classifier.telemetry().fields_removed;
        prop_assert!(total_actions > 0, "should have tracked redaction actions");
    }

    /// RedactedEvent.fields_redacted + fields_removed == total redaction_actions.
    #[test]
    fn redacted_event_counts_match_actions(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"password": "s", "api_key": "k", "name": "safe"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();
        let redacted = classifier.redact_event(&event, &classified);

        let total = redacted.fields_redacted() + redacted.fields_removed();
        prop_assert_eq!(total, redacted.redaction_actions.len() as u32);
    }
}

// =============================================================================
// redact_event_with_decision property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// redact_event_with_decision tracks Accept in telemetry.
    #[test]
    fn redact_with_accept_increments_accepted(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"safe": "value"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();
        let _redacted = classifier.redact_event_with_decision(
            &event, &classified, IngestionDecision::Accept,
        );
        prop_assert_eq!(classifier.telemetry().events_accepted, 1);
    }

    /// redact_event_with_decision tracks AcceptRedacted in telemetry.
    #[test]
    fn redact_with_accept_redacted_increments(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"password": "secret"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();
        let _redacted = classifier.redact_event_with_decision(
            &event, &classified, IngestionDecision::AcceptRedacted,
        );
        prop_assert_eq!(classifier.telemetry().events_accepted_redacted, 1);
    }

    /// redact_event_with_decision tracks Reject in telemetry.
    #[test]
    fn redact_with_reject_increments_rejected(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"data": "test"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();
        let _redacted = classifier.redact_event_with_decision(
            &event, &classified,
            IngestionDecision::Reject { reason: "test reason".into() },
        );
        prop_assert_eq!(classifier.telemetry().events_rejected, 1);
    }

    /// redact_event_with_decision tracks Quarantine in telemetry.
    #[test]
    fn redact_with_quarantine_increments_quarantined(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let payload = serde_json::json!({"data": "test"});
        let event = make_test_event("conn-1", payload);
        let classified = classifier.classify_event(&event).unwrap();
        let _redacted = classifier.redact_event_with_decision(
            &event, &classified,
            IngestionDecision::Quarantine { reason: "suspicious".into() },
        );
        prop_assert_eq!(classifier.telemetry().events_quarantined, 1);
    }

    /// Multiple classify+redact cycles keep telemetry consistent.
    #[test]
    fn classify_redact_cycle_telemetry(n in 1usize..=6) {
        let mut classifier = setup_classifier_with_default_policy();
        for i in 0..n {
            let payload = serde_json::json!({"step": i, "safe": "data"});
            let event = make_test_event("conn-1", payload);
            let classified = classifier.classify_event(&event).unwrap();
            let _redacted = classifier.redact_event_with_decision(
                &event, &classified, IngestionDecision::Accept,
            );
        }
        prop_assert_eq!(classifier.telemetry().events_classified, n as u64);
        prop_assert_eq!(classifier.telemetry().events_accepted, n as u64);
        prop_assert_eq!(classifier.audit_log().len(), n);
    }
}

// =============================================================================
// Audit log lifecycle property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    /// Audit log is bounded by max_audit_entries config.
    #[test]
    fn audit_log_bounded_by_config(max_entries in 5usize..=20) {
        let config = ClassifierConfig {
            max_audit_entries: max_entries,
            ..ClassifierConfig::default()
        };
        let mut classifier = ConnectorDataClassifier::new(config);
        classifier.register_policy(ClassificationPolicy::default());

        // Generate more events than max_entries
        for i in 0..(max_entries + 10) {
            let payload = serde_json::json!({"step": i});
            let event = make_test_event("conn-1", payload);
            let classified = classifier.classify_event(&event).unwrap();
            classifier.redact_event_with_decision(
                &event, &classified, IngestionDecision::Accept,
            );
        }
        prop_assert!(classifier.audit_log().len() <= max_entries);
    }

    /// audit_log_json serializes without error.
    #[test]
    fn audit_log_json_serializes(n in 1usize..=5) {
        let mut classifier = setup_classifier_with_default_policy();
        for i in 0..n {
            let payload = serde_json::json!({"step": i});
            let event = make_test_event("conn-1", payload);
            let classified = classifier.classify_event(&event).unwrap();
            classifier.redact_event(&event, &classified);
        }
        let json_result = classifier.audit_log_json();
        let is_ok = json_result.is_ok();
        prop_assert!(is_ok, "audit_log_json should serialize successfully");
        let json_str = json_result.unwrap();
        prop_assert!(!json_str.is_empty());
    }
}

// =============================================================================
// Metadata field classification and redaction
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// classify_event classifies metadata fields.
    #[test]
    fn classify_event_with_metadata(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let mut event = make_test_event("conn-1", serde_json::json!({}));
        event.metadata.insert("safe_key".to_string(), "safe_value".to_string());
        event.metadata.insert("password".to_string(), "secret_in_metadata".to_string());

        let classified = classifier.classify_event(&event).unwrap();
        // Should have metadata.password classified
        let has_meta_pw = classified.field_classifications.iter()
            .any(|f| f.field_path == "metadata.password");
        prop_assert!(has_meta_pw, "should classify metadata.password");
    }

    /// redact_event redacts metadata fields with sensitive classification.
    #[test]
    fn redact_event_redacts_metadata(_dummy in 0u8..1) {
        let mut classifier = setup_classifier_with_default_policy();
        let mut event = make_test_event("conn-1", serde_json::json!({}));
        event.metadata.insert("password".to_string(), "meta_secret".to_string());

        let classified = classifier.classify_event(&event).unwrap();
        let redacted = classifier.redact_event(&event, &classified);

        // Password should be removed from metadata
        let pw_in_meta = redacted.event.metadata.get("password");
        // If classified as Prohibited with Remove strategy, it should be gone
        let has_pw_action = redacted.redaction_actions.iter()
            .any(|a| a.field_path == "metadata.password");
        if has_pw_action {
            // Either removed or replaced
            let action = redacted.redaction_actions.iter()
                .find(|a| a.field_path == "metadata.password").unwrap();
            if matches!(action.strategy, RedactionStrategy::Remove) {
                let is_none = pw_in_meta.is_none();
                prop_assert!(is_none, "removed metadata should not be present");
            }
        }
    }
}

// =============================================================================
// ClassificationAuditEntry serde roundtrip
// =============================================================================

use frankenterm_core::connector_data_classification::ClassificationAuditEntry;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// ClassificationAuditEntry survives serde roundtrip.
    #[test]
    fn audit_entry_serde_roundtrip(
        sensitivity in arb_sensitivity(),
        secrets in any::<bool>(),
        fields_r in 0u32..=20,
        fields_rem in 0u32..=20,
    ) {
        let entry = ClassificationAuditEntry {
            event_id: "e1".to_string(),
            connector_id: "c1".to_string(),
            policy_id: "p1".to_string(),
            sensitivity,
            decision: IngestionDecision::Accept,
            fields_redacted: fields_r,
            fields_removed: fields_rem,
            secrets_detected: secrets,
            timestamp_ms: 12345,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: ClassificationAuditEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(entry.event_id, back.event_id);
        prop_assert_eq!(entry.sensitivity, back.sensitivity);
        prop_assert_eq!(entry.fields_redacted, back.fields_redacted);
        prop_assert_eq!(entry.fields_removed, back.fields_removed);
        prop_assert_eq!(entry.secrets_detected, back.secrets_detected);
    }
}
