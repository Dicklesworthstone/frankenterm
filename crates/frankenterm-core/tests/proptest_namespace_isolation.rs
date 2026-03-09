//! Property-based tests for namespace_isolation module serde roundtrips.

use frankenterm_core::namespace_isolation::*;
use proptest::prelude::*;
use std::collections::{BTreeSet, HashMap};

// =============================================================================
// Strategies
// =============================================================================

fn arb_namespace_segment() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z][a-z0-9_-]{0,15}")
        .unwrap()
        .prop_filter("non-empty segment", |s| !s.is_empty())
}

fn arb_namespace_name() -> impl Strategy<Value = String> {
    prop::collection::vec(arb_namespace_segment(), 1..=4)
        .prop_map(|segments| segments.join("."))
}

fn arb_tenant_namespace() -> impl Strategy<Value = TenantNamespace> {
    arb_namespace_name().prop_filter_map("valid namespace", |name| TenantNamespace::new(name))
}

fn arb_resource_kind() -> impl Strategy<Value = NamespacedResourceKind> {
    prop_oneof![
        Just(NamespacedResourceKind::Pane),
        Just(NamespacedResourceKind::Session),
        Just(NamespacedResourceKind::Workflow),
        Just(NamespacedResourceKind::Connector),
        Just(NamespacedResourceKind::Agent),
        Just(NamespacedResourceKind::Credential),
    ]
}

fn arb_cross_tenant_decision() -> impl Strategy<Value = CrossTenantDecision> {
    prop_oneof![
        Just(CrossTenantDecision::Deny),
        Just(CrossTenantDecision::AllowWithAudit),
        Just(CrossTenantDecision::Allow),
    ]
}

fn arb_namespace_binding() -> impl Strategy<Value = NamespaceBinding> {
    (arb_tenant_namespace(), arb_resource_kind(), "[a-z0-9]{1,20}")
        .prop_map(|(ns, kind, id)| NamespaceBinding::new(ns, kind, id))
}

fn arb_cross_tenant_rule() -> impl Strategy<Value = CrossTenantRule> {
    (
        arb_tenant_namespace(),
        arb_tenant_namespace(),
        prop::collection::btree_set(arb_resource_kind(), 0..=3),
        arb_cross_tenant_decision(),
        prop::option::of("[a-z ]{1,30}"),
    )
        .prop_map(|(src, tgt, kinds, decision, reason)| CrossTenantRule {
            source: src,
            target: tgt,
            resource_kinds: kinds,
            decision,
            reason,
        })
}

fn arb_cross_tenant_policy() -> impl Strategy<Value = CrossTenantPolicy> {
    (
        arb_cross_tenant_decision(),
        any::<bool>(),
        any::<bool>(),
        prop::collection::vec(arb_cross_tenant_rule(), 0..=3),
    )
        .prop_map(|(default_decision, allow_hierarchical, system_bypass, rules)| {
            CrossTenantPolicy {
                default_decision,
                allow_hierarchical,
                system_bypass,
                rules,
            }
        })
}

fn arb_boundary_check_result() -> impl Strategy<Value = BoundaryCheckResult> {
    (
        any::<bool>(),
        arb_tenant_namespace(),
        arb_tenant_namespace(),
        arb_cross_tenant_decision(),
        prop::option::of("[a-z_]{1,20}"),
        any::<bool>(),
    )
        .prop_map(
            |(crosses, src, tgt, decision, rule, hier)| BoundaryCheckResult {
                crosses_boundary: crosses,
                source_namespace: src,
                target_namespace: tgt,
                decision,
                matched_rule: rule,
                hierarchical_match: hier,
            },
        )
}

fn arb_boundary_audit_entry() -> impl Strategy<Value = BoundaryAuditEntry> {
    (
        any::<u64>(),
        arb_tenant_namespace(),
        arb_tenant_namespace(),
        arb_resource_kind(),
        "[a-z0-9]{1,10}",
        arb_cross_tenant_decision(),
        prop::option::of("[a-z ]{1,20}"),
    )
        .prop_map(
            |(ts, src, tgt, kind, id, decision, reason)| BoundaryAuditEntry {
                timestamp_ms: ts,
                source_namespace: src,
                target_namespace: tgt,
                resource_kind: kind.as_str().to_owned(),
                resource_id: id,
                decision,
                reason,
            },
        )
}

fn arb_registry_snapshot() -> impl Strategy<Value = NamespaceRegistrySnapshot> {
    (
        any::<usize>(),
        any::<usize>(),
        prop::collection::hash_map("[a-z]{1,10}", any::<usize>(), 0..=4),
        any::<usize>(),
        any::<usize>(),
        arb_cross_tenant_decision(),
    )
        .prop_map(
            |(bindings, active, counts, audit, rules, decision)| NamespaceRegistrySnapshot {
                total_bindings: bindings,
                active_namespaces: active,
                namespace_counts: counts,
                audit_log_size: audit,
                policy_rule_count: rules,
                default_decision: decision,
            },
        )
}

fn arb_telemetry_snapshot() -> impl Strategy<Value = NamespaceIsolationTelemetrySnapshot> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(total, cross, denied, audited, hier, bypass)| {
                NamespaceIsolationTelemetrySnapshot {
                    checks_total: total,
                    cross_boundary_total: cross,
                    cross_boundary_denied: denied,
                    cross_boundary_audited: audited,
                    hierarchical_grants: hier,
                    system_bypass_grants: bypass,
                }
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn tenant_namespace_json_roundtrip(ns in arb_tenant_namespace()) {
        let json = serde_json::to_string(&ns).unwrap();
        let back: TenantNamespace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ns, back);
    }

    #[test]
    fn cross_tenant_decision_json_roundtrip(d in arb_cross_tenant_decision()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: CrossTenantDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, back);
    }

    #[test]
    fn namespace_binding_json_roundtrip(b in arb_namespace_binding()) {
        let json = serde_json::to_string(&b).unwrap();
        let back: NamespaceBinding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(b, back);
    }

    #[test]
    fn cross_tenant_rule_json_roundtrip(r in arb_cross_tenant_rule()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: CrossTenantRule = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r, back);
    }

    #[test]
    fn cross_tenant_policy_json_roundtrip(p in arb_cross_tenant_policy()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: CrossTenantPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(p, back);
    }

    #[test]
    fn boundary_check_result_json_roundtrip(r in arb_boundary_check_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: BoundaryCheckResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r, back);
    }

    #[test]
    fn boundary_audit_entry_json_roundtrip(e in arb_boundary_audit_entry()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: BoundaryAuditEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(e, back);
    }

    #[test]
    fn registry_snapshot_json_roundtrip(s in arb_registry_snapshot()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: NamespaceRegistrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn telemetry_snapshot_json_roundtrip(s in arb_telemetry_snapshot()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: NamespaceIsolationTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    // ---- Behavioral property tests ----

    #[test]
    fn same_namespace_boundary_never_crosses(ns in arb_tenant_namespace(), kind in arb_resource_kind()) {
        let reg = NamespaceRegistry::new();
        let result = reg.check_boundary(&ns, &ns, kind);
        prop_assert!(!result.crosses_boundary);
        prop_assert!(result.is_allowed());
    }

    #[test]
    fn different_namespace_boundary_always_crosses(
        a in arb_tenant_namespace(),
        b in arb_tenant_namespace(),
        kind in arb_resource_kind()
    ) {
        prop_assume!(a != b);
        let reg = NamespaceRegistry::new();
        let result = reg.check_boundary(&a, &b, kind);
        prop_assert!(result.crosses_boundary);
    }

    #[test]
    fn default_policy_denies_cross_tenant(
        a in arb_tenant_namespace(),
        b in arb_tenant_namespace(),
        kind in arb_resource_kind()
    ) {
        prop_assume!(a != b && !a.is_system());
        let policy = CrossTenantPolicy {
            system_bypass: false,
            allow_hierarchical: false,
            ..CrossTenantPolicy::default()
        };
        let reg = NamespaceRegistry::with_policy(policy);
        let result = reg.check_boundary(&a, &b, kind);
        prop_assert!(!result.is_allowed());
    }

    #[test]
    fn bind_lookup_identity(ns in arb_tenant_namespace(), kind in arb_resource_kind(), id in "[a-z0-9]{1,10}") {
        let mut reg = NamespaceRegistry::new();
        reg.bind(kind, &id, ns.clone());
        prop_assert_eq!(reg.lookup(kind, &id), ns);
        prop_assert!(reg.is_bound(kind, &id));
    }

    #[test]
    fn unbind_restores_default(ns in arb_tenant_namespace(), kind in arb_resource_kind(), id in "[a-z0-9]{1,10}") {
        let mut reg = NamespaceRegistry::new();
        reg.bind(kind, &id, ns);
        reg.unbind(kind, &id);
        prop_assert_eq!(reg.lookup(kind, &id), TenantNamespace::default());
        prop_assert!(!reg.is_bound(kind, &id));
    }

    #[test]
    fn namespace_depth_equals_segment_count(ns in arb_tenant_namespace()) {
        let expected = ns.as_str().split('.').count();
        prop_assert_eq!(ns.depth(), expected);
    }

    #[test]
    fn parent_has_fewer_segments(ns in arb_tenant_namespace()) {
        if let Some(parent) = ns.parent() {
            prop_assert!(parent.depth() < ns.depth());
            prop_assert!(ns.is_descendant_of(&parent));
        }
    }

    #[test]
    fn not_descendant_of_self(ns in arb_tenant_namespace()) {
        prop_assert!(!ns.is_descendant_of(&ns));
    }

    #[test]
    fn ancestor_descendant_asymmetric(
        parent in arb_tenant_namespace(),
        child in arb_tenant_namespace()
    ) {
        if child.is_descendant_of(&parent) {
            prop_assert!(!parent.is_descendant_of(&child));
            prop_assert!(parent.is_ancestor_of(&child));
        }
    }

    #[test]
    fn telemetry_record_increments_total(result in arb_boundary_check_result()) {
        let mut telem = NamespaceIsolationTelemetry::default();
        let before = telem.checks_total;
        telem.record(&result);
        prop_assert_eq!(telem.checks_total, before + 1);
    }
}
