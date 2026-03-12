//! Property tests for unified_telemetry module (ft-3681t.7.1).
//!
//! Covers serde roundtrips for standalone types, trace context construction,
//! correlation ID semantics, redaction label ordering and restriction logic,
//! health status worst-case aggregation, and causality link serialization.

use frankenterm_core::unified_telemetry::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_telemetry_source() -> impl Strategy<Value = TelemetrySource> {
    prop_oneof![
        Just(TelemetrySource::Cli),
        Just(TelemetrySource::Mcp),
        Just(TelemetrySource::Web),
        Just(TelemetrySource::Internal),
    ]
}

fn arb_redaction_label() -> impl Strategy<Value = RedactionLabel> {
    prop_oneof![
        Just(RedactionLabel::Public),
        Just(RedactionLabel::Internal),
        Just(RedactionLabel::Sensitive),
        Just(RedactionLabel::Pii),
    ]
}

fn arb_health_status() -> impl Strategy<Value = HealthStatus> {
    prop_oneof![
        Just(HealthStatus::Healthy),
        Just(HealthStatus::Degraded),
        Just(HealthStatus::Unhealthy),
        Just(HealthStatus::Unknown),
    ]
}

fn arb_subsystem_layer() -> impl Strategy<Value = SubsystemLayer> {
    prop_oneof![
        Just(SubsystemLayer::Policy),
        Just(SubsystemLayer::Connector),
        Just(SubsystemLayer::Swarm),
        Just(SubsystemLayer::Mux),
        Just(SubsystemLayer::Storage),
        Just(SubsystemLayer::Runtime),
        Just(SubsystemLayer::Ingest),
    ]
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_telemetry_source(source in arb_telemetry_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let back: TelemetrySource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(source, back);
    }

    #[test]
    fn serde_roundtrip_redaction_label(label in arb_redaction_label()) {
        let json = serde_json::to_string(&label).unwrap();
        let back: RedactionLabel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(label, back);
    }

    #[test]
    fn serde_roundtrip_health_status(status in arb_health_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: HealthStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn serde_roundtrip_subsystem_layer(layer in arb_subsystem_layer()) {
        let json = serde_json::to_string(&layer).unwrap();
        let back: SubsystemLayer = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(layer, back);
    }

    #[test]
    fn serde_roundtrip_trace_context(
        trace_id in "[a-f0-9]{32}",
        span_id in "[a-f0-9]{16}",
        has_parent in any::<bool>(),
    ) {
        let mut ctx = TraceContext::new(trace_id, span_id);
        if has_parent {
            ctx = ctx.with_parent("parent-span");
        }
        let json = serde_json::to_string(&ctx).unwrap();
        let back: TraceContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ctx, back);
    }

    #[test]
    fn serde_roundtrip_correlation_id(id in "[a-z0-9-]{1,32}") {
        let cid = CorrelationId::new(&id);
        let json = serde_json::to_string(&cid).unwrap();
        let back: CorrelationId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cid, back);
    }

    #[test]
    fn serde_roundtrip_causality_link(
        cause_corr in "[a-z0-9]{1,8}",
        cause_span in "[a-z0-9]{1,8}",
        effect_corr in "[a-z0-9]{1,8}",
        effect_span in "[a-z0-9]{1,8}",
        latency in proptest::option::of(0..1_000_000u64),
    ) {
        let link = CausalityLink {
            cause_correlation_id: cause_corr,
            cause_span_id: cause_span,
            effect_correlation_id: effect_corr,
            effect_span_id: effect_span,
            latency_us: latency,
            label: Some("test-edge".into()),
        };
        let json = serde_json::to_string(&link).unwrap();
        let back: CausalityLink = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(link, back);
    }
}

// =============================================================================
// TraceContext semantics
// =============================================================================

proptest! {
    #[test]
    fn empty_trace_context_is_empty(_dummy in 0..1u32) {
        let ctx = TraceContext::default();
        prop_assert!(ctx.is_empty());
        prop_assert!(ctx.parent_span_id.is_none());
    }

    #[test]
    fn non_empty_trace_context(trace_id in "[a-f0-9]{32}") {
        let ctx = TraceContext::new(&trace_id, "span1");
        prop_assert!(!ctx.is_empty());
    }

    #[test]
    fn with_parent_sets_parent(parent in "[a-z0-9]{1,16}") {
        let ctx = TraceContext::new("t1", "s1").with_parent(&parent);
        prop_assert_eq!(ctx.parent_span_id.as_deref(), Some(parent.as_str()));
    }
}

// =============================================================================
// CorrelationId semantics
// =============================================================================

proptest! {
    #[test]
    fn correlation_id_display_matches_inner(id in "[a-z0-9-]{1,32}") {
        let cid = CorrelationId::new(&id);
        prop_assert_eq!(cid.to_string(), id);
        prop_assert!(!cid.is_empty());
    }

    #[test]
    fn empty_correlation_is_empty(_dummy in 0..1u32) {
        let cid = CorrelationId::default();
        prop_assert!(cid.is_empty());
    }
}

// =============================================================================
// RedactionLabel ordering and restriction logic
// =============================================================================

proptest! {
    #[test]
    fn redaction_label_total_order(a in arb_redaction_label(), b in arb_redaction_label()) {
        prop_assert!(a <= b || a > b);
    }

    #[test]
    fn public_is_minimum(label in arb_redaction_label()) {
        prop_assert!(label >= RedactionLabel::Public);
    }

    #[test]
    fn pii_is_maximum(label in arb_redaction_label()) {
        prop_assert!(label <= RedactionLabel::Pii);
    }

    #[test]
    fn max_restriction_is_commutative(a in arb_redaction_label(), b in arb_redaction_label()) {
        prop_assert_eq!(a.max_restriction(b), b.max_restriction(a));
    }

    #[test]
    fn max_restriction_idempotent(a in arb_redaction_label()) {
        prop_assert_eq!(a.max_restriction(a), a);
    }

    #[test]
    fn max_restriction_with_public_is_identity(label in arb_redaction_label()) {
        prop_assert_eq!(label.max_restriction(RedactionLabel::Public), label);
    }

    #[test]
    fn max_restriction_with_pii_is_pii(label in arb_redaction_label()) {
        prop_assert_eq!(label.max_restriction(RedactionLabel::Pii), RedactionLabel::Pii);
    }

    #[test]
    fn requires_scrub_only_sensitive_and_pii(label in arb_redaction_label()) {
        let expected = matches!(label, RedactionLabel::Sensitive | RedactionLabel::Pii);
        prop_assert_eq!(label.requires_scrub(), expected);
    }
}

// =============================================================================
// HealthStatus worst-case aggregation
// =============================================================================

proptest! {
    #[test]
    fn worst_is_commutative(a in arb_health_status(), b in arb_health_status()) {
        prop_assert_eq!(a.worst(b), b.worst(a));
    }

    #[test]
    fn worst_idempotent(a in arb_health_status()) {
        prop_assert_eq!(a.worst(a), a);
    }

    #[test]
    fn worst_with_healthy_is_identity(status in arb_health_status()) {
        // Healthy is the best, so worst(x, Healthy) = x (except Unknown)
        let result = status.worst(HealthStatus::Healthy);
        match status {
            HealthStatus::Unhealthy => prop_assert_eq!(result, HealthStatus::Unhealthy),
            HealthStatus::Unknown => prop_assert_eq!(result, HealthStatus::Unknown),
            HealthStatus::Degraded => prop_assert_eq!(result, HealthStatus::Degraded),
            HealthStatus::Healthy => prop_assert_eq!(result, HealthStatus::Healthy),
        }
    }

    #[test]
    fn worst_with_unhealthy_always_unhealthy(status in arb_health_status()) {
        prop_assert_eq!(status.worst(HealthStatus::Unhealthy), HealthStatus::Unhealthy);
    }

    #[test]
    fn unknown_dominates_degraded_and_healthy(status in arb_health_status()) {
        if status != HealthStatus::Unhealthy {
            let result = status.worst(HealthStatus::Unknown);
            prop_assert_eq!(result, HealthStatus::Unknown);
        }
    }
}

// =============================================================================
// Default values
// =============================================================================

#[test]
fn default_telemetry_source_is_internal() {
    assert_eq!(TelemetrySource::default(), TelemetrySource::Internal);
}

#[test]
fn default_redaction_label_is_internal() {
    assert_eq!(RedactionLabel::default(), RedactionLabel::Internal);
}

#[test]
fn default_health_status_is_unknown() {
    assert_eq!(HealthStatus::default(), HealthStatus::Unknown);
}

#[test]
fn schema_version_not_empty() {
    assert!(!SCHEMA_VERSION.is_empty());
}
