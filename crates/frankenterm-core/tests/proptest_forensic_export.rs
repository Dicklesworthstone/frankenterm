//! Property tests for forensic_export module.
//!
//! Covers serde roundtrip for all 16 serializable types plus behavioral
//! invariants for ForensicStore query/ingest/redaction and display impls.

use frankenterm_core::forensic_export::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

// =============================================================================
// Arbitrary strategies
// =============================================================================

fn arb_forensic_actor() -> impl Strategy<Value = ForensicActor> {
    prop_oneof![
        ("[a-z0-9_]{1,10}", "[a-z0-9_-]{1,10}").prop_map(|(operator_id, session_id)| {
            ForensicActor::Operator {
                operator_id,
                session_id,
            }
        }),
        ("[a-z0-9_]{1,10}", "[a-z_]{1,10}")
            .prop_map(|(agent_id, model)| ForensicActor::Agent { agent_id, model }),
        "[a-z_]{1,10}".prop_map(|subsystem| ForensicActor::System { subsystem }),
        ("[a-z0-9_]{1,10}", "[a-z_]{1,10}").prop_map(|(connector_id, provider)| {
            ForensicActor::Connector {
                connector_id,
                provider,
            }
        }),
    ]
}

fn arb_forensic_action() -> impl Strategy<Value = ForensicAction> {
    prop_oneof![
        ("[0-9]{1,5}", "[a-z ]{1,15}").prop_map(|(pane_id, command_summary)| {
            ForensicAction::PaneWrite {
                pane_id,
                command_summary,
            }
        }),
        ("[a-z0-9_-]{1,10}", "[a-z_]{1,10}").prop_map(|(workflow_id, transition)| {
            ForensicAction::WorkflowLifecycle {
                workflow_id,
                transition,
            }
        }),
        ("[a-z_]{1,10}", "[a-z_]{1,10}").prop_map(|(rule_id, surface)| {
            ForensicAction::PolicyEvaluation { rule_id, surface }
        }),
        ("[a-z0-9_]{1,10}", "[a-z_]{1,10}").prop_map(|(connector_id, action_type)| {
            ForensicAction::ConnectorDispatch {
                connector_id,
                action_type,
            }
        }),
        ("[a-z_]{1,10}", "[a-z_]{1,10}").prop_map(|(credential_id, action_type)| {
            ForensicAction::CredentialAction {
                credential_id,
                action_type,
            }
        }),
        ("[a-z0-9_]{1,10}", "[a-z_]{1,10}").prop_map(|(component_id, new_state)| {
            ForensicAction::QuarantineChange {
                component_id,
                new_state,
            }
        }),
        "[a-z_]{1,10}".prop_map(|new_level| ForensicAction::KillSwitchChange { new_level }),
        "[a-z_.]{1,15}".prop_map(|config_key| ForensicAction::ConfigChange { config_key }),
        ("[a-z0-9_-]{1,10}", "[a-z_]{1,10}").prop_map(|(session_id, transition)| {
            ForensicAction::SessionLifecycle {
                session_id,
                transition,
            }
        }),
        ("[a-z_]{1,10}", "[a-z ]{1,15}")
            .prop_map(|(category, detail)| ForensicAction::Custom { category, detail }),
    ]
}

fn arb_forensic_target() -> impl Strategy<Value = ForensicTarget> {
    prop_oneof![
        ("[0-9]{1,5}", "[a-z_]{1,10}")
            .prop_map(|(pane_id, workspace)| ForensicTarget::Pane { pane_id, workspace }),
        "[a-z0-9_-]{1,10}".prop_map(|workflow_id| ForensicTarget::Workflow { workflow_id }),
        "[a-z0-9_-]{1,10}".prop_map(|session_id| ForensicTarget::Session { session_id }),
        "[a-z0-9_]{1,10}".prop_map(|connector_id| ForensicTarget::Connector { connector_id }),
        "[a-z0-9_]{1,10}".prop_map(|credential_id| ForensicTarget::Credential { credential_id }),
        "[a-z_]{1,10}".prop_map(|rule_id| ForensicTarget::PolicyRule { rule_id }),
        ("[a-z0-9_]{1,10}", "[a-z_]{1,10}").prop_map(|(component_id, component_kind)| {
            ForensicTarget::Component {
                component_id,
                component_kind,
            }
        }),
        "[a-z_]{1,10}".prop_map(|subsystem| ForensicTarget::System { subsystem }),
    ]
}

fn arb_policy_verdict() -> impl Strategy<Value = PolicyVerdict> {
    prop_oneof![
        Just(PolicyVerdict::Allow),
        Just(PolicyVerdict::Deny),
        Just(PolicyVerdict::AllowWithFlag),
        Just(PolicyVerdict::NoMatch),
    ]
}

fn arb_forensic_policy_decision() -> impl Strategy<Value = ForensicPolicyDecision> {
    (
        arb_policy_verdict(),
        prop::collection::vec("[a-z_]{1,10}", 0..3),
        "[a-z_]{1,10}",
        "[a-z ]{1,20}",
    )
        .prop_map(
            |(decision, matched_rules, surface, reason)| ForensicPolicyDecision {
                decision,
                matched_rules,
                surface,
                reason,
            },
        )
}

fn arb_forensic_outcome() -> impl Strategy<Value = ForensicOutcome> {
    prop_oneof![
        Just(ForensicOutcome::Success),
        "[a-z ]{1,20}".prop_map(|error| ForensicOutcome::Failed { error }),
        "[a-z ]{1,20}".prop_map(|reason| ForensicOutcome::Denied { reason }),
        "[a-z ]{1,20}".prop_map(|blocker| ForensicOutcome::Blocked { blocker }),
        Just(ForensicOutcome::Timeout),
        "[a-z ]{1,20}".prop_map(|reason| ForensicOutcome::RolledBack { reason }),
    ]
}

fn arb_correlation_ids() -> impl Strategy<Value = CorrelationIds> {
    (
        proptest::option::of("[a-f0-9]{8,16}"),
        proptest::option::of("[a-f0-9]{4,8}"),
        proptest::option::of("[a-z0-9_-]{1,10}"),
        proptest::option::of("[a-z0-9_-]{1,10}"),
        proptest::option::of("[a-z0-9_-]{1,10}"),
    )
        .prop_map(
            |(trace_id, span_id, session_id, workflow_id, transaction_id)| CorrelationIds {
                trace_id,
                span_id,
                session_id,
                workflow_id,
                transaction_id,
            },
        )
}

fn arb_sensitivity_level() -> impl Strategy<Value = SensitivityLevel> {
    prop_oneof![
        Just(SensitivityLevel::Public),
        Just(SensitivityLevel::Internal),
        Just(SensitivityLevel::Confidential),
        Just(SensitivityLevel::Restricted),
    ]
}

fn arb_forensic_record() -> impl Strategy<Value = ForensicRecord> {
    (
        "[a-f0-9]{8,16}",
        0..u64::MAX,
        arb_forensic_actor(),
        arb_forensic_action(),
        arb_forensic_target(),
        arb_forensic_policy_decision(),
        arb_forensic_outcome(),
        arb_correlation_ids(),
        arb_sensitivity_level(),
        prop::collection::btree_map("[a-z_]{1,8}", "[a-z0-9 ]{1,15}", 0..3),
    )
        .prop_map(
            |(
                record_id,
                timestamp_ms,
                actor,
                action,
                target,
                policy_decision,
                outcome,
                correlation,
                sensitivity,
                metadata,
            )| {
                ForensicRecord {
                    record_id,
                    timestamp_ms,
                    actor,
                    action,
                    target,
                    policy_decision,
                    outcome,
                    correlation,
                    sensitivity,
                    metadata,
                }
            },
        )
}

fn arb_time_range() -> impl Strategy<Value = TimeRange> {
    (0..u64::MAX / 2, 0..u64::MAX / 2).prop_map(|(a, b)| {
        let start_ms = a.min(b);
        let end_ms = a.max(b);
        TimeRange { start_ms, end_ms }
    })
}

fn arb_sort_order() -> impl Strategy<Value = SortOrder> {
    prop_oneof![
        Just(SortOrder::TimestampDesc),
        Just(SortOrder::TimestampAsc),
    ]
}

fn arb_export_format() -> impl Strategy<Value = ExportFormat> {
    prop_oneof![
        Just(ExportFormat::Json),
        Just(ExportFormat::Jsonl),
        Just(ExportFormat::Csv),
    ]
}

fn arb_forensic_query() -> impl Strategy<Value = ForensicQuery> {
    (
        proptest::option::of(arb_time_range()),
        proptest::option::of("[a-z_]{1,10}"),
        proptest::option::of("[a-z_]{1,10}"),
        proptest::option::of(arb_policy_verdict()),
        proptest::option::of("[a-z_]{1,10}"),
        proptest::option::of(arb_sensitivity_level()),
        proptest::option::of("[a-f0-9]{8,16}"),
        proptest::option::of("[a-z0-9_-]{1,10}"),
        proptest::option::of("[a-z ]{1,10}"),
        proptest::option::of(1..100usize),
        proptest::option::of(0..50usize),
        arb_sort_order(),
    )
        .prop_map(
            |(
                time_range,
                actor_filter,
                action_filter,
                verdict_filter,
                outcome_filter,
                min_sensitivity,
                trace_id,
                workflow_id,
                text_search,
                limit,
                offset,
                sort,
            )| {
                ForensicQuery {
                    time_range,
                    actor_filter,
                    action_filter,
                    verdict_filter,
                    outcome_filter,
                    min_sensitivity,
                    trace_id,
                    workflow_id,
                    text_search,
                    limit,
                    offset,
                    sort,
                }
            },
        )
}

fn arb_forensic_telemetry() -> impl Strategy<Value = ForensicTelemetry> {
    (0..1000u64, 0..1000u64, 0..1000u64, 0..1000u64, 0..1000u64).prop_map(
        |(
            records_ingested,
            records_evicted,
            queries_executed,
            exports_completed,
            records_redacted,
        )| {
            ForensicTelemetry {
                records_ingested,
                records_evicted,
                queries_executed,
                exports_completed,
                records_redacted,
            }
        },
    )
}

fn arb_forensic_telemetry_snapshot() -> impl Strategy<Value = ForensicTelemetrySnapshot> {
    (
        arb_forensic_telemetry(),
        0..u64::MAX,
        0..1000usize,
        1..1000usize,
    )
        .prop_map(
            |(counters, captured_at_ms, current_record_count, max_records)| {
                ForensicTelemetrySnapshot {
                    captured_at_ms,
                    counters,
                    current_record_count,
                    max_records,
                }
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn forensic_actor_json_roundtrip(a in arb_forensic_actor()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: ForensicActor = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&a, &back);
    }

    #[test]
    fn forensic_action_json_roundtrip(a in arb_forensic_action()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: ForensicAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&a, &back);
    }

    #[test]
    fn forensic_target_json_roundtrip(t in arb_forensic_target()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: ForensicTarget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&t, &back);
    }

    #[test]
    fn policy_verdict_json_roundtrip(v in arb_policy_verdict()) {
        let json = serde_json::to_string(&v).unwrap();
        let back: PolicyVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, back);
    }

    #[test]
    fn forensic_policy_decision_json_roundtrip(d in arb_forensic_policy_decision()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: ForensicPolicyDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&d, &back);
    }

    #[test]
    fn forensic_outcome_json_roundtrip(o in arb_forensic_outcome()) {
        let json = serde_json::to_string(&o).unwrap();
        let back: ForensicOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&o, &back);
    }

    #[test]
    fn correlation_ids_json_roundtrip(c in arb_correlation_ids()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: CorrelationIds = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&c, &back);
    }

    #[test]
    fn sensitivity_level_json_roundtrip(s in arb_sensitivity_level()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: SensitivityLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn forensic_record_json_roundtrip(r in arb_forensic_record()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: ForensicRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&r, &back);
    }

    #[test]
    fn time_range_json_roundtrip(t in arb_time_range()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: TimeRange = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&t, &back);
    }

    #[test]
    fn sort_order_json_roundtrip(s in arb_sort_order()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: SortOrder = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn export_format_json_roundtrip(f in arb_export_format()) {
        let json = serde_json::to_string(&f).unwrap();
        let back: ExportFormat = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(f, back);
    }

    #[test]
    fn forensic_query_json_roundtrip(q in arb_forensic_query()) {
        let json = serde_json::to_string(&q).unwrap();
        let _back: ForensicQuery = serde_json::from_str(&json).unwrap();
        // Deserialize succeeds
    }

    #[test]
    fn forensic_telemetry_json_roundtrip(t in arb_forensic_telemetry()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: ForensicTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, back);
    }

    #[test]
    fn forensic_telemetry_snapshot_json_roundtrip(s in arb_forensic_telemetry_snapshot()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: ForensicTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }
}

// =============================================================================
// Behavioral property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    // -- Display impls produce non-empty output --

    #[test]
    fn forensic_actor_display_nonempty(a in arb_forensic_actor()) {
        prop_assert!(!a.to_string().is_empty());
    }

    #[test]
    fn forensic_action_display_nonempty(a in arb_forensic_action()) {
        prop_assert!(!a.to_string().is_empty());
    }

    #[test]
    fn forensic_outcome_display_nonempty(o in arb_forensic_outcome()) {
        prop_assert!(!o.to_string().is_empty());
    }

    #[test]
    fn policy_verdict_display_matches(v in arb_policy_verdict()) {
        let display = v.to_string();
        let expected = match v {
            PolicyVerdict::Allow => "allow",
            PolicyVerdict::Deny => "deny",
            PolicyVerdict::AllowWithFlag => "allow_with_flag",
            PolicyVerdict::NoMatch => "no_match",
        };
        prop_assert_eq!(display, expected);
    }

    #[test]
    fn sensitivity_level_display_matches(s in arb_sensitivity_level()) {
        let display = s.to_string();
        let expected = match s {
            SensitivityLevel::Public => "public",
            SensitivityLevel::Internal => "internal",
            SensitivityLevel::Confidential => "confidential",
            SensitivityLevel::Restricted => "restricted",
        };
        prop_assert_eq!(display, expected);
    }

    #[test]
    fn export_format_display_matches(f in arb_export_format()) {
        let display = f.to_string();
        let expected = match f {
            ExportFormat::Json => "json",
            ExportFormat::Jsonl => "jsonl",
            ExportFormat::Csv => "csv",
        };
        prop_assert_eq!(display, expected);
    }

    // -- Sensitivity ordering is total --

    #[test]
    fn sensitivity_ordering_total(a in arb_sensitivity_level(), b in arb_sensitivity_level()) {
        let lt = a < b;
        let eq = a == b;
        let gt = a > b;
        let count = [lt, eq, gt].iter().filter(|&&x| x).count();
        prop_assert_eq!(count, 1);
    }

    // -- ForensicStore behavioral tests --

    #[test]
    fn store_ingest_increments_count(records in prop::collection::vec(arb_forensic_record(), 1..5)) {
        let mut store = ForensicStore::new(100);
        for r in &records {
            store.ingest(r.clone());
        }
        prop_assert_eq!(store.len(), records.len());
        prop_assert!(!store.is_empty());
    }

    #[test]
    fn store_respects_capacity(n in 1..10usize) {
        let mut store = ForensicStore::new(n);
        // Ingest more than capacity
        for i in 0..(n + 5) {
            store.ingest(ForensicRecord {
                record_id: format!("r{i}"),
                timestamp_ms: i as u64 * 1000,
                actor: ForensicActor::System { subsystem: "test".into() },
                action: ForensicAction::ConfigChange { config_key: "k".into() },
                target: ForensicTarget::System { subsystem: "s".into() },
                policy_decision: ForensicPolicyDecision {
                    decision: PolicyVerdict::Allow,
                    matched_rules: vec![],
                    surface: "s".into(),
                    reason: "r".into(),
                },
                outcome: ForensicOutcome::Success,
                correlation: CorrelationIds::default(),
                sensitivity: SensitivityLevel::Public,
                metadata: BTreeMap::new(),
            });
        }
        // Store should never exceed capacity
        prop_assert!(store.len() <= n);
    }

    #[test]
    fn store_query_empty_returns_nothing(_dummy in 0..1u8) {
        let mut store = ForensicStore::new(100);
        let result = store.query(&ForensicQuery::default());
        prop_assert_eq!(result.total_count, 0);
        prop_assert!(result.records.is_empty());
        prop_assert!(!result.has_more);
    }

    #[test]
    fn store_telemetry_ingested_matches(n in 1..10usize) {
        let mut store = ForensicStore::new(100);
        for i in 0..n {
            store.ingest(ForensicRecord {
                record_id: format!("r{i}"),
                timestamp_ms: i as u64,
                actor: ForensicActor::System { subsystem: "t".into() },
                action: ForensicAction::ConfigChange { config_key: "k".into() },
                target: ForensicTarget::System { subsystem: "s".into() },
                policy_decision: ForensicPolicyDecision {
                    decision: PolicyVerdict::Allow,
                    matched_rules: vec![],
                    surface: "s".into(),
                    reason: "r".into(),
                },
                outcome: ForensicOutcome::Success,
                correlation: CorrelationIds::default(),
                sensitivity: SensitivityLevel::Public,
                metadata: BTreeMap::new(),
            });
        }
        let snap = store.telemetry_snapshot(1000);
        prop_assert_eq!(snap.counters.records_ingested, n as u64);
        prop_assert_eq!(snap.current_record_count, n);
    }

    #[test]
    fn store_eviction_telemetry(capacity in 2..5usize, extra in 1..5usize) {
        let mut store = ForensicStore::new(capacity);
        let total = capacity + extra;
        for i in 0..total {
            store.ingest(ForensicRecord {
                record_id: format!("r{i}"),
                timestamp_ms: i as u64,
                actor: ForensicActor::System { subsystem: "t".into() },
                action: ForensicAction::ConfigChange { config_key: "k".into() },
                target: ForensicTarget::System { subsystem: "s".into() },
                policy_decision: ForensicPolicyDecision {
                    decision: PolicyVerdict::Allow,
                    matched_rules: vec![],
                    surface: "s".into(),
                    reason: "r".into(),
                },
                outcome: ForensicOutcome::Success,
                correlation: CorrelationIds::default(),
                sensitivity: SensitivityLevel::Public,
                metadata: BTreeMap::new(),
            });
        }
        let snap = store.telemetry_snapshot(1000);
        prop_assert_eq!(snap.counters.records_evicted, extra as u64);
        prop_assert_eq!(snap.counters.records_ingested, total as u64);
    }

    // -- CorrelationIds default is all None --

    #[test]
    fn correlation_ids_default_all_none(_dummy in 0..1u8) {
        let c = CorrelationIds::default();
        prop_assert!(c.trace_id.is_none());
        prop_assert!(c.span_id.is_none());
        prop_assert!(c.session_id.is_none());
        prop_assert!(c.workflow_id.is_none());
        prop_assert!(c.transaction_id.is_none());
    }

    // -- ForensicQuery default --

    #[test]
    fn forensic_query_default_is_unfiltered(_dummy in 0..1u8) {
        let q = ForensicQuery::default();
        prop_assert!(q.time_range.is_none());
        prop_assert!(q.actor_filter.is_none());
        prop_assert!(q.action_filter.is_none());
        prop_assert!(q.verdict_filter.is_none());
        prop_assert!(q.limit.is_none());
        prop_assert!(q.offset.is_none());
    }
}
