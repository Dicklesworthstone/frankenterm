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
