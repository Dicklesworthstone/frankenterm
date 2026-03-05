//! Property-based tests for the canonical connector event model.
//!
//! Tests cover schema versioning invariants, event builder completeness,
//! validation logic, compatibility checking, indexing contracts,
//! conversion functions, and serde roundtrips.

use proptest::prelude::*;
use std::collections::BTreeMap;

use frankenterm_core::connector_event_model::{
    CANONICAL_SCHEMA_VERSION, CanonicalConnectorEvent, CanonicalSeverity, CompatibilityReport,
    EventDirection, IndexingContract, SchemaEvolutionRegistry, SchemaFieldDef,
    SchemaValidationResult, SchemaVersion, check_compatibility, from_lifecycle_transition,
};
use frankenterm_core::connector_host_runtime::{
    ConnectorCapability, ConnectorFailureClass, ConnectorLifecyclePhase,
};
use frankenterm_core::connector_inbound_bridge::ConnectorSignalKind;
use frankenterm_core::connector_outbound_bridge::{
    ConnectorActionKind, OutboundEventSource, OutboundSeverity,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_direction() -> impl Strategy<Value = EventDirection> {
    prop_oneof![
        Just(EventDirection::Inbound),
        Just(EventDirection::Outbound),
        Just(EventDirection::Lifecycle),
    ]
}

fn arb_severity() -> impl Strategy<Value = CanonicalSeverity> {
    prop_oneof![
        Just(CanonicalSeverity::Info),
        Just(CanonicalSeverity::Warning),
        Just(CanonicalSeverity::Critical),
    ]
}

fn arb_outbound_severity() -> impl Strategy<Value = OutboundSeverity> {
    prop_oneof![
        Just(OutboundSeverity::Info),
        Just(OutboundSeverity::Warning),
        Just(OutboundSeverity::Critical),
    ]
}

fn arb_signal_kind() -> impl Strategy<Value = ConnectorSignalKind> {
    prop_oneof![
        Just(ConnectorSignalKind::Webhook),
        Just(ConnectorSignalKind::Stream),
        Just(ConnectorSignalKind::Poll),
        Just(ConnectorSignalKind::Lifecycle),
        Just(ConnectorSignalKind::HealthCheck),
        Just(ConnectorSignalKind::Failure),
        Just(ConnectorSignalKind::Custom),
    ]
}

fn arb_action_kind() -> impl Strategy<Value = ConnectorActionKind> {
    prop_oneof![
        Just(ConnectorActionKind::Notify),
        Just(ConnectorActionKind::Ticket),
        Just(ConnectorActionKind::TriggerWorkflow),
        Just(ConnectorActionKind::AuditLog),
        Just(ConnectorActionKind::Invoke),
        Just(ConnectorActionKind::CredentialAction),
    ]
}

fn arb_event_source() -> impl Strategy<Value = OutboundEventSource> {
    prop_oneof![
        Just(OutboundEventSource::PatternDetected),
        Just(OutboundEventSource::PaneLifecycle),
        Just(OutboundEventSource::WorkflowLifecycle),
        Just(OutboundEventSource::UserAction),
        Just(OutboundEventSource::PolicyDecision),
        Just(OutboundEventSource::HealthAlert),
        Just(OutboundEventSource::Custom),
    ]
}

fn arb_lifecycle_phase() -> impl Strategy<Value = ConnectorLifecyclePhase> {
    prop_oneof![
        Just(ConnectorLifecyclePhase::Stopped),
        Just(ConnectorLifecyclePhase::Starting),
        Just(ConnectorLifecyclePhase::Running),
        Just(ConnectorLifecyclePhase::Degraded),
        Just(ConnectorLifecyclePhase::Failed),
    ]
}

fn arb_failure_class() -> impl Strategy<Value = ConnectorFailureClass> {
    prop_oneof![
        Just(ConnectorFailureClass::Network),
        Just(ConnectorFailureClass::Auth),
        Just(ConnectorFailureClass::Quota),
        Just(ConnectorFailureClass::Timeout),
        Just(ConnectorFailureClass::Policy),
        Just(ConnectorFailureClass::Validation),
        Just(ConnectorFailureClass::Unknown),
    ]
}

fn arb_capability() -> impl Strategy<Value = ConnectorCapability> {
    prop_oneof![
        Just(ConnectorCapability::NetworkEgress),
        Just(ConnectorCapability::FilesystemRead),
        Just(ConnectorCapability::FilesystemWrite),
        Just(ConnectorCapability::Invoke),
        Just(ConnectorCapability::SecretBroker),
        Just(ConnectorCapability::ReadState),
        Just(ConnectorCapability::StreamEvents),
        Just(ConnectorCapability::ProcessExec),
    ]
}

fn arb_schema_version() -> impl Strategy<Value = SchemaVersion> {
    (1u32..5, 0u32..10).prop_map(|(major, minor)| SchemaVersion::new(major, minor))
}

fn arb_non_empty_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{1,20}".prop_map(String::from)
}

fn arb_metadata() -> impl Strategy<Value = BTreeMap<String, String>> {
    proptest::collection::btree_map(arb_non_empty_string(), arb_non_empty_string(), 0..5)
}

fn arb_canonical_event() -> impl Strategy<Value = CanonicalConnectorEvent> {
    (
        arb_direction(),
        arb_non_empty_string(),
        arb_non_empty_string(),
        arb_severity(),
        1u64..u64::MAX,
        arb_non_empty_string(),
        arb_non_empty_string(),
        arb_metadata(),
    )
        .prop_map(
            |(direction, connector_id, event_type, severity, ts, evt_id, corr_id, metadata)| {
                let mut event = CanonicalConnectorEvent::new(
                    direction,
                    connector_id,
                    event_type,
                    serde_json::json!({"test": true}),
                )
                .with_severity(severity)
                .with_timestamp_ms(ts)
                .with_event_id(evt_id)
                .with_correlation_id(corr_id);
                event.metadata = metadata;
                event
            },
        )
}

// =============================================================================
// Schema version properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn schema_version_reflexive_compatibility(
        major in 1u32..100,
        minor in 0u32..100,
    ) {
        let v = SchemaVersion::new(major, minor);
        prop_assert!(v.is_compatible_with(&v), "version should be compatible with itself");
    }

    #[test]
    fn schema_version_minor_forward_compat(
        major in 1u32..10,
        minor_low in 0u32..50,
        delta in 1u32..50,
    ) {
        let low = SchemaVersion::new(major, minor_low);
        let high = SchemaVersion::new(major, minor_low + delta);
        // Higher minor is compatible with lower (has all the fields)
        prop_assert!(high.is_compatible_with(&low));
        // Lower minor is NOT compatible with higher (missing new fields)
        prop_assert!(!low.is_compatible_with(&high));
    }

    #[test]
    fn schema_version_major_mismatch_incompatible(
        major_a in 1u32..10,
        major_b in 1u32..10,
        minor_a in 0u32..10,
        minor_b in 0u32..10,
    ) {
        prop_assume!(major_a != major_b);
        let a = SchemaVersion::new(major_a, minor_a);
        let b = SchemaVersion::new(major_b, minor_b);
        prop_assert!(!a.is_compatible_with(&b));
    }

    #[test]
    fn schema_version_display_format(
        major in 0u32..100,
        minor in 0u32..100,
    ) {
        let v = SchemaVersion::new(major, minor);
        let display = format!("{v}");
        prop_assert_eq!(display, format!("{major}.{minor}"));
    }

    #[test]
    fn schema_version_serde_roundtrip(
        major in 0u32..100,
        minor in 0u32..100,
    ) {
        let v = SchemaVersion::new(major, minor);
        let json = serde_json::to_string(&v).unwrap();
        let back: SchemaVersion = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, back);
    }
}

// =============================================================================
// Direction properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn direction_as_str_nonempty(dir in arb_direction()) {
        let s = dir.as_str();
        prop_assert!(!s.is_empty());
    }

    #[test]
    fn direction_display_matches_as_str(dir in arb_direction()) {
        prop_assert_eq!(format!("{dir}"), dir.as_str());
    }

    #[test]
    fn direction_serde_roundtrip(dir in arb_direction()) {
        let json = serde_json::to_string(&dir).unwrap();
        let back: EventDirection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dir, back);
    }
}

// =============================================================================
// Severity properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn severity_as_str_nonempty(sev in arb_severity()) {
        prop_assert!(!sev.as_str().is_empty());
    }

    #[test]
    fn severity_display_matches_as_str(sev in arb_severity()) {
        prop_assert_eq!(format!("{sev}"), sev.as_str());
    }

    #[test]
    fn severity_serde_roundtrip(sev in arb_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: CanonicalSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, back);
    }

    #[test]
    fn severity_from_outbound_preserves_level(sev in arb_outbound_severity()) {
        let canonical = CanonicalSeverity::from_outbound(sev);
        match sev {
            OutboundSeverity::Info => prop_assert_eq!(canonical, CanonicalSeverity::Info),
            OutboundSeverity::Warning => prop_assert_eq!(canonical, CanonicalSeverity::Warning),
            OutboundSeverity::Critical => prop_assert_eq!(canonical, CanonicalSeverity::Critical),
        }
    }
}

// =============================================================================
// Canonical event builder properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn event_new_sets_current_schema_version(
        dir in arb_direction(),
        connector in arb_non_empty_string(),
        event_type in arb_non_empty_string(),
    ) {
        let event = CanonicalConnectorEvent::new(
            dir, &connector, &event_type, serde_json::json!({}),
        );
        prop_assert_eq!(event.schema_version, SchemaVersion::current());
        prop_assert_eq!(event.direction, dir);
        prop_assert_eq!(&event.connector_id, &connector);
        prop_assert_eq!(&event.event_type, &event_type);
    }

    #[test]
    fn event_builder_with_methods_are_identity(
        connector in arb_non_empty_string(),
        ts in 1u64..u64::MAX,
        sev in arb_severity(),
        pane_id in 1u64..10000,
        wf_id in arb_non_empty_string(),
        zone_id in arb_non_empty_string(),
        cap in arb_capability(),
        name in arb_non_empty_string(),
        meta_key in arb_non_empty_string(),
        meta_val in arb_non_empty_string(),
    ) {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Outbound, &connector, "test", serde_json::json!({}),
        )
        .with_timestamp_ms(ts)
        .with_severity(sev)
        .with_pane_id(pane_id)
        .with_workflow_id(&wf_id)
        .with_sandbox(&zone_id, cap)
        .with_connector_name(&name)
        .with_metadata(&meta_key, &meta_val);

        prop_assert_eq!(event.timestamp_ms, ts);
        prop_assert_eq!(event.severity, sev);
        prop_assert_eq!(event.pane_id, Some(pane_id));
        prop_assert_eq!(event.workflow_id.as_deref(), Some(wf_id.as_str()));
        prop_assert_eq!(event.zone_id.as_deref(), Some(zone_id.as_str()));
        prop_assert_eq!(event.capability, Some(cap));
        prop_assert_eq!(event.connector_name.as_deref(), Some(name.as_str()));
        prop_assert_eq!(event.metadata.get(&meta_key).map(|s| s.as_str()), Some(meta_val.as_str()));
    }

    #[test]
    fn event_rule_id_format(
        dir in arb_direction(),
        connector in arb_non_empty_string(),
        event_type in arb_non_empty_string(),
    ) {
        let event = CanonicalConnectorEvent::new(
            dir, &connector, &event_type, serde_json::json!({}),
        );
        let rule_id = event.rule_id();
        let expected = format!("{}.{connector}.{event_type}", dir.as_str());
        prop_assert_eq!(rule_id, expected);
    }

    #[test]
    fn event_with_signal_sets_inbound_fields(
        kind in arb_signal_kind(),
        sub_type in proptest::option::of(arb_non_empty_string()),
    ) {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Inbound, "test", "test", serde_json::json!({}),
        )
        .with_signal(kind, sub_type.clone());

        prop_assert_eq!(event.signal_kind, Some(kind));
        prop_assert_eq!(event.signal_sub_type, sub_type);
    }

    #[test]
    fn event_with_action_sets_outbound_fields(
        source in arb_event_source(),
        kind in arb_action_kind(),
    ) {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Outbound, "test", "test", serde_json::json!({}),
        )
        .with_action(source, kind);

        prop_assert_eq!(event.event_source, Some(source));
        prop_assert_eq!(event.action_kind, Some(kind));
    }
}

// =============================================================================
// Failure detection properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn event_with_failure_class_is_failure(class in arb_failure_class()) {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Lifecycle, "test", "test", serde_json::json!({}),
        )
        .with_failure(class);
        prop_assert!(event.is_failure());
    }

    #[test]
    fn event_with_failed_phase_is_failure(_unused in 0..1u8) {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Lifecycle, "test", "test", serde_json::json!({}),
        )
        .with_lifecycle(ConnectorLifecyclePhase::Failed);
        prop_assert!(event.is_failure());
    }

    #[test]
    fn event_with_critical_severity_is_failure(
        dir in arb_direction(),
    ) {
        let event = CanonicalConnectorEvent::new(
            dir, "test", "test", serde_json::json!({}),
        )
        .with_severity(CanonicalSeverity::Critical);
        prop_assert!(event.is_failure());
    }

    #[test]
    fn event_info_no_failure_class_no_failed_phase_is_not_failure(
        phase in prop_oneof![
            Just(ConnectorLifecyclePhase::Stopped),
            Just(ConnectorLifecyclePhase::Starting),
            Just(ConnectorLifecyclePhase::Running),
        ],
    ) {
        let event = CanonicalConnectorEvent::new(
            EventDirection::Lifecycle, "test", "test", serde_json::json!({}),
        )
        .with_severity(CanonicalSeverity::Info)
        .with_lifecycle(phase);
        prop_assert!(!event.is_failure());
    }
}

// =============================================================================
// Serde roundtrip properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn canonical_event_serde_roundtrip(event in arb_canonical_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let back: CanonicalConnectorEvent = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(event.schema_version, back.schema_version);
        prop_assert_eq!(event.direction, back.direction);
        prop_assert_eq!(event.event_id, back.event_id);
        prop_assert_eq!(event.correlation_id, back.correlation_id);
        prop_assert_eq!(event.timestamp_ms, back.timestamp_ms);
        prop_assert_eq!(event.connector_id, back.connector_id);
        prop_assert_eq!(event.event_type, back.event_type);
        prop_assert_eq!(event.severity, back.severity);
        prop_assert_eq!(event.signal_kind, back.signal_kind);
        prop_assert_eq!(event.action_kind, back.action_kind);
        prop_assert_eq!(event.pane_id, back.pane_id);
        prop_assert_eq!(event.metadata, back.metadata);
    }

    #[test]
    fn schema_field_def_serde_roundtrip(
        name in arb_non_empty_string(),
        field_type in arb_non_empty_string(),
        required in proptest::bool::ANY,
        version in arb_schema_version(),
    ) {
        let field = SchemaFieldDef {
            name,
            field_type,
            required,
            introduced_in: version,
            deprecated_in: None,
            description: "test".to_string(),
        };
        let json = serde_json::to_string(&field).unwrap();
        let back: SchemaFieldDef = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(field, back);
    }

    #[test]
    fn validation_result_serde_roundtrip(
        valid in proptest::bool::ANY,
        n_errors in 0usize..5,
        n_warnings in 0usize..5,
    ) {
        let result = SchemaValidationResult {
            valid,
            errors: (0..n_errors).map(|i| format!("error-{i}")).collect(),
            warnings: (0..n_warnings).map(|i| format!("warning-{i}")).collect(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: SchemaValidationResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result.valid, back.valid);
        prop_assert_eq!(result.errors.len(), back.errors.len());
        prop_assert_eq!(result.warnings.len(), back.warnings.len());
    }
}

// =============================================================================
// Schema evolution registry properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn registry_v1_has_expected_required_count(_unused in 0..1u8) {
        let registry = SchemaEvolutionRegistry::new();
        let required = registry.required_fields_for(&SchemaVersion::current());
        // v1 has 9 required fields: schema_version, direction, event_id,
        // correlation_id, timestamp_ms, connector_id, event_type, severity, payload
        prop_assert_eq!(required.len(), 9);
    }

    #[test]
    fn registry_all_fields_includes_required_and_optional(_unused in 0..1u8) {
        let registry = SchemaEvolutionRegistry::new();
        let v = SchemaVersion::current();
        let all = registry.all_fields_for(&v);
        let required = registry.required_fields_for(&v);
        // All >= required
        prop_assert!(all.len() >= required.len());
        // Every required field is in all
        for r in &required {
            prop_assert!(all.iter().any(|a| a.name == r.name));
        }
    }

    #[test]
    fn registry_future_version_gets_same_or_more_fields(
        minor_delta in 0u32..5,
    ) {
        let registry = SchemaEvolutionRegistry::new();
        let current = SchemaVersion::current();
        let future = SchemaVersion::new(current.major, current.minor + minor_delta);
        let current_fields = registry.all_fields_for(&current);
        let future_fields = registry.all_fields_for(&future);
        // Same major, higher minor should get >= fields
        prop_assert!(future_fields.len() >= current_fields.len());
    }
}

// =============================================================================
// Validation properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn validation_valid_event_passes(
        connector in arb_non_empty_string(),
        event_type in arb_non_empty_string(),
        kind in arb_signal_kind(),
    ) {
        let registry = SchemaEvolutionRegistry::new();
        let event = CanonicalConnectorEvent::new(
            EventDirection::Inbound, &connector, &event_type, serde_json::json!({}),
        )
        .with_signal(kind, None);

        let result = registry.validate_event(&event);
        prop_assert!(result.valid, "well-formed event should pass validation: errors={:?}", result.errors);
    }

    #[test]
    fn validation_empty_required_fields_fail(
        dir in arb_direction(),
    ) {
        let registry = SchemaEvolutionRegistry::new();
        let mut event = CanonicalConnectorEvent::new(
            dir, "test", "test", serde_json::json!({}),
        );
        // Clear required fields
        event.event_id.clear();
        event.correlation_id.clear();
        event.connector_id.clear();
        event.event_type.clear();

        let result = registry.validate_event(&event);
        prop_assert!(!result.valid);
        prop_assert!(result.errors.len() >= 4, "should have at least 4 errors for 4 empty fields");
    }

    #[test]
    fn validation_incompatible_schema_version_fails(
        major in 2u32..100,
    ) {
        let registry = SchemaEvolutionRegistry::new();
        let mut event = CanonicalConnectorEvent::new(
            EventDirection::Inbound, "test", "test", serde_json::json!({}),
        );
        event.schema_version = SchemaVersion::new(major, 0);

        let result = registry.validate_event(&event);
        prop_assert!(!result.valid, "version {major}.0 should be incompatible with current");
    }

    #[test]
    fn validation_direction_specific_warnings(dir in arb_direction()) {
        let registry = SchemaEvolutionRegistry::new();
        // Create event without direction-specific fields
        let event = CanonicalConnectorEvent::new(
            dir, "test", "test", serde_json::json!({}),
        );
        let result = registry.validate_event(&event);
        // Should be valid (warnings don't fail)
        prop_assert!(result.valid);
        // Should have a direction-specific warning
        prop_assert!(!result.warnings.is_empty(),
            "event without direction-specific fields should have a warning for {:?}", dir);
    }
}

// =============================================================================
// Compatibility checking properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn compatibility_same_version_is_compatible(v in arb_schema_version()) {
        let registry = SchemaEvolutionRegistry::new();
        let report = check_compatibility(&registry, &v, &v);
        prop_assert!(report.compatible, "same version should be compatible");
        prop_assert!(report.missing_fields.is_empty());
    }

    #[test]
    fn compatibility_report_has_correct_versions(
        src in arb_schema_version(),
        tgt in arb_schema_version(),
    ) {
        let registry = SchemaEvolutionRegistry::new();
        let report = check_compatibility(&registry, &src, &tgt);
        prop_assert_eq!(report.source, src);
        prop_assert_eq!(report.target, tgt);
    }

    #[test]
    fn compatibility_report_serde_roundtrip(
        src in arb_schema_version(),
        tgt in arb_schema_version(),
    ) {
        let registry = SchemaEvolutionRegistry::new();
        let report = check_compatibility(&registry, &src, &tgt);
        let json = serde_json::to_string(&report).unwrap();
        let back: CompatibilityReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.compatible, back.compatible);
        prop_assert_eq!(report.missing_fields.len(), back.missing_fields.len());
        prop_assert_eq!(report.deprecated_fields.len(), back.deprecated_fields.len());
    }
}

// =============================================================================
// Indexing contract properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn indexing_contract_searchable_implies_found(idx in 0usize..6) {
        let contract = IndexingContract::default_contract();
        let field = &contract.searchable_fields[idx.min(contract.searchable_fields.len() - 1)];
        prop_assert!(contract.is_searchable(field));
    }

    #[test]
    fn indexing_contract_filterable_implies_found(idx in 0usize..11) {
        let contract = IndexingContract::default_contract();
        let field = &contract.filterable_fields[idx.min(contract.filterable_fields.len() - 1)];
        prop_assert!(contract.is_filterable(field));
    }

    #[test]
    fn indexing_contract_nonexistent_field_not_found(
        field in "[A-Z][a-z]{10,20}_nonexistent",
    ) {
        let contract = IndexingContract::default_contract();
        prop_assert!(!contract.is_searchable(&field));
        prop_assert!(!contract.is_filterable(&field));
    }

    #[test]
    fn indexing_contract_serde_roundtrip(_unused in 0..1u8) {
        let contract = IndexingContract::default_contract();
        let json = serde_json::to_string(&contract).unwrap();
        let back: IndexingContract = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(contract.searchable_fields, back.searchable_fields);
        prop_assert_eq!(contract.filterable_fields, back.filterable_fields);
        prop_assert_eq!(contract.sortable_fields, back.sortable_fields);
        prop_assert_eq!(contract.facet_fields, back.facet_fields);
    }
}

// =============================================================================
// Lifecycle transition conversion properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lifecycle_transition_direction_is_lifecycle(
        connector in arb_non_empty_string(),
        phase in arb_lifecycle_phase(),
        ts in 1u64..u64::MAX,
    ) {
        let event = from_lifecycle_transition(&connector, phase, ts);
        prop_assert_eq!(event.direction, EventDirection::Lifecycle);
        prop_assert_eq!(&event.connector_id, &connector);
        prop_assert_eq!(event.lifecycle_phase, Some(phase));
        prop_assert_eq!(event.timestamp_ms, ts);
    }

    #[test]
    fn lifecycle_transition_failed_is_critical(
        connector in arb_non_empty_string(),
        ts in 1u64..u64::MAX,
    ) {
        let event = from_lifecycle_transition(&connector, ConnectorLifecyclePhase::Failed, ts);
        prop_assert_eq!(event.severity, CanonicalSeverity::Critical);
        prop_assert!(event.is_failure());
    }

    #[test]
    fn lifecycle_transition_degraded_is_warning(
        connector in arb_non_empty_string(),
        ts in 1u64..u64::MAX,
    ) {
        let event = from_lifecycle_transition(&connector, ConnectorLifecyclePhase::Degraded, ts);
        prop_assert_eq!(event.severity, CanonicalSeverity::Warning);
    }

    #[test]
    fn lifecycle_transition_normal_phases_are_info(
        connector in arb_non_empty_string(),
        phase in prop_oneof![
            Just(ConnectorLifecyclePhase::Stopped),
            Just(ConnectorLifecyclePhase::Starting),
            Just(ConnectorLifecyclePhase::Running),
        ],
        ts in 1u64..u64::MAX,
    ) {
        let event = from_lifecycle_transition(&connector, phase, ts);
        prop_assert_eq!(event.severity, CanonicalSeverity::Info);
    }

    #[test]
    fn lifecycle_event_type_contains_phase(
        connector in arb_non_empty_string(),
        phase in arb_lifecycle_phase(),
        ts in 1u64..u64::MAX,
    ) {
        let event = from_lifecycle_transition(&connector, phase, ts);
        prop_assert!(event.event_type.starts_with("lifecycle."),
            "event_type should start with 'lifecycle.' but was: {}", event.event_type);
    }
}

// =============================================================================
// Cross-property: event constants
// =============================================================================

#[test]
fn canonical_schema_version_matches_current() {
    assert_eq!(SchemaVersion::current().major, CANONICAL_SCHEMA_VERSION);
    assert_eq!(SchemaVersion::current().minor, 0);
}

#[test]
fn default_schema_version_is_current() {
    assert_eq!(SchemaVersion::default(), SchemaVersion::current());
}

#[test]
fn default_severity_is_info() {
    assert_eq!(CanonicalSeverity::default(), CanonicalSeverity::Info);
}

#[test]
fn registry_default_equals_new() {
    let new = SchemaEvolutionRegistry::new();
    let default = SchemaEvolutionRegistry::default();
    assert_eq!(new.current_version, default.current_version);
    assert_eq!(new.fields.len(), default.fields.len());
}
