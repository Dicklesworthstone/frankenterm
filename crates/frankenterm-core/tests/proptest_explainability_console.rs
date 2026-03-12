//! Property tests for explainability_console module (ft-3681t.9.7).
//!
//! Covers serde roundtrips, TraceSeverity ordering, CausalRelationship serde,
//! TraceSource serde, console ingestion invariants, capacity eviction bounds,
//! query filter correctness, telemetry counter consistency, trace ID monotonicity,
//! correlation grouping, and causal link mechanics.

use frankenterm_core::explainability_console::*;
use frankenterm_core::policy::{ActionKind, ActorKind, PolicySurface};
use frankenterm_core::policy_decision_log::DecisionOutcome;
use proptest::prelude::*;
use std::collections::HashMap;

// =============================================================================
// Strategies
// =============================================================================

fn arb_trace_source() -> impl Strategy<Value = TraceSource> {
    prop_oneof![
        Just(TraceSource::Policy),
        Just(TraceSource::Audit),
        Just(TraceSource::Connector),
        Just(TraceSource::Workflow),
        Just(TraceSource::CommandGuard),
        Just(TraceSource::RateLimiter),
        Just(TraceSource::Quarantine),
    ]
}

fn arb_trace_severity() -> impl Strategy<Value = TraceSeverity> {
    prop_oneof![
        Just(TraceSeverity::Info),
        Just(TraceSeverity::Warning),
        Just(TraceSeverity::Denied),
        Just(TraceSeverity::Critical),
    ]
}

fn arb_causal_relationship() -> impl Strategy<Value = CausalRelationship> {
    prop_oneof![
        Just(CausalRelationship::TriggeredBy),
        Just(CausalRelationship::Triggered),
        Just(CausalRelationship::RetryOf),
        Just(CausalRelationship::Overrides),
        Just(CausalRelationship::CompensationOf),
        Just(CausalRelationship::Correlated),
    ]
}

fn arb_decision_outcome() -> impl Strategy<Value = DecisionOutcome> {
    prop_oneof![
        Just(DecisionOutcome::Allow),
        Just(DecisionOutcome::Deny),
        Just(DecisionOutcome::RequireApproval),
    ]
}

fn make_test_trace(
    outcome: DecisionOutcome,
    source: TraceSource,
    pane_id: Option<u64>,
    timestamp_ms: u64,
) -> DecisionTrace {
    DecisionTrace {
        trace_id: 0,
        timestamp_ms,
        action: ActionKind::SendText,
        actor: ActorKind::Robot,
        surface: PolicySurface::Robot,
        pane_id,
        outcome,
        rule_id: None,
        reason: "test".to_string(),
        rules_evaluated: 1,
        explanation_id: None,
        context: HashMap::new(),
        causal_links: Vec::new(),
        correlation_id: None,
        source,
        severity: TraceSeverity::Info,
    }
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_trace_source(source in arb_trace_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let back: TraceSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(source, back);
    }

    #[test]
    fn serde_roundtrip_trace_severity(sev in arb_trace_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: TraceSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, back);
    }

    #[test]
    fn serde_roundtrip_causal_relationship(rel in arb_causal_relationship()) {
        let json = serde_json::to_string(&rel).unwrap();
        let back: CausalRelationship = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rel, back);
    }

    #[test]
    fn serde_roundtrip_causal_link(
        trace_id in 1..1000u64,
        rel in arb_causal_relationship(),
    ) {
        let link = CausalLink {
            related_trace_id: trace_id,
            relationship: rel,
            description: Some("test link".into()),
        };
        let json = serde_json::to_string(&link).unwrap();
        let back: CausalLink = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.related_trace_id, trace_id);
        prop_assert_eq!(back.relationship, rel);
    }

    #[test]
    fn serde_roundtrip_decision_trace(
        outcome in arb_decision_outcome(),
        source in arb_trace_source(),
        ts in 0..1_000_000u64,
    ) {
        let mut trace = make_test_trace(outcome, source, Some(42), ts);
        trace.trace_id = 7;
        trace.rule_id = Some("test.rule".into());
        let json = serde_json::to_string(&trace).unwrap();
        let back: DecisionTrace = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.trace_id, 7);
        prop_assert_eq!(back.outcome, outcome);
        prop_assert_eq!(back.source, source);
        prop_assert_eq!(back.timestamp_ms, ts);
    }

    #[test]
    fn serde_roundtrip_trace_query(_dummy in 0..1u32) {
        let query = TraceQuery::denials(50);
        let json = serde_json::to_string(&query).unwrap();
        let back: TraceQuery = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.limit, 50);
    }

    #[test]
    fn serde_roundtrip_console_telemetry(_dummy in 0..1u32) {
        let telem = ConsoleTelemetry::default();
        let json = serde_json::to_string(&telem).unwrap();
        let back: ConsoleTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.traces_ingested, 0);
        prop_assert_eq!(back.queries_executed, 0);
    }
}

// =============================================================================
// TraceSeverity ordering
// =============================================================================

proptest! {
    #[test]
    fn severity_total_order(a in arb_trace_severity(), b in arb_trace_severity()) {
        prop_assert!(a <= b || a > b);
    }

    #[test]
    fn info_is_minimum(sev in arb_trace_severity()) {
        prop_assert!(sev >= TraceSeverity::Info);
    }

    #[test]
    fn critical_is_maximum(sev in arb_trace_severity()) {
        prop_assert!(sev <= TraceSeverity::Critical);
    }
}

// =============================================================================
// Console ingestion invariants
// =============================================================================

proptest! {
    #[test]
    fn trace_ids_are_monotonic(n in 2..20usize) {
        let mut console = ExplainabilityConsole::new(100);
        let mut ids = Vec::new();
        for i in 0..n {
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, i as u64 * 100);
            ids.push(console.ingest(trace));
        }
        for window in ids.windows(2) {
            prop_assert!(window[0] < window[1]);
        }
    }

    #[test]
    fn ingested_trace_is_retrievable(
        outcome in arb_decision_outcome(),
        source in arb_trace_source(),
        pane_id in 0..100u64,
    ) {
        let mut console = ExplainabilityConsole::new(100);
        let trace = make_test_trace(outcome, source, Some(pane_id), 1000);
        let id = console.ingest(trace);
        let found = console.get_trace(id);
        prop_assert!(found.is_some());
        prop_assert_eq!(found.unwrap().pane_id, Some(pane_id));
        prop_assert_eq!(found.unwrap().source, source);
    }

    #[test]
    fn len_tracks_ingestion(n in 1..20usize) {
        let mut console = ExplainabilityConsole::new(100);
        for i in 0..n {
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, i as u64);
            console.ingest(trace);
        }
        prop_assert_eq!(console.len(), n);
        prop_assert!(!console.is_empty());
    }
}

// =============================================================================
// Capacity eviction
// =============================================================================

proptest! {
    #[test]
    fn capacity_bounds_trace_count(
        capacity in 2..15usize,
        inserts in 5..30usize,
    ) {
        let mut console = ExplainabilityConsole::new(capacity);
        for i in 0..inserts {
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, i as u64);
            console.ingest(trace);
        }
        prop_assert!(console.len() <= capacity);
    }

    #[test]
    fn eviction_telemetry_consistent(
        capacity in 2..10usize,
        inserts in 5..20usize,
    ) {
        let mut console = ExplainabilityConsole::new(capacity);
        for i in 0..inserts {
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, i as u64);
            console.ingest(trace);
        }
        prop_assert_eq!(console.telemetry().traces_ingested, inserts as u64);
        let expected_evictions = if inserts > capacity { inserts - capacity } else { 0 };
        prop_assert_eq!(console.telemetry().traces_evicted, expected_evictions as u64);
    }
}

// =============================================================================
// Query filter correctness
// =============================================================================

proptest! {
    #[test]
    fn query_all_returns_all_traces(n in 1..15usize) {
        let mut console = ExplainabilityConsole::new(100);
        for i in 0..n {
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, i as u64);
            console.ingest(trace);
        }
        let result = console.query(&TraceQuery::all(100));
        prop_assert_eq!(result.total_count, n);
        prop_assert_eq!(result.traces.len(), n);
    }

    #[test]
    fn query_by_pane_filters_correctly(
        target_pane in 0..10u64,
        n_match in 1..5usize,
        n_other in 1..5usize,
    ) {
        let mut console = ExplainabilityConsole::new(100);
        for i in 0..n_match {
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, Some(target_pane), i as u64 * 100);
            console.ingest(trace);
        }
        for i in 0..n_other {
            let other_pane = target_pane + 1;
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, Some(other_pane), (n_match + i) as u64 * 100);
            console.ingest(trace);
        }
        let result = console.query(&TraceQuery::for_pane(target_pane, 100));
        prop_assert_eq!(result.total_count, n_match);
    }

    #[test]
    fn query_denials_only_returns_denials(
        n_allow in 1..5usize,
        n_deny in 1..5usize,
    ) {
        let mut console = ExplainabilityConsole::new(100);
        for i in 0..n_allow {
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, i as u64);
            console.ingest(trace);
        }
        for i in 0..n_deny {
            let trace = make_test_trace(DecisionOutcome::Deny, TraceSource::Policy, None, (n_allow + i) as u64);
            console.ingest(trace);
        }
        let result = console.query(&TraceQuery::denials(100));
        prop_assert_eq!(result.total_count, n_deny);
        for t in &result.traces {
            prop_assert_eq!(t.outcome, DecisionOutcome::Deny);
        }
    }

    #[test]
    fn query_by_source_filters_correctly(
        target in arb_trace_source(),
    ) {
        let mut console = ExplainabilityConsole::new(100);
        // Add one of each source
        for (i, src) in [TraceSource::Policy, TraceSource::Connector, TraceSource::Workflow].iter().enumerate() {
            let trace = make_test_trace(DecisionOutcome::Allow, *src, None, i as u64);
            console.ingest(trace);
        }
        let query = TraceQuery {
            source: Some(target),
            limit: 100,
            ..Default::default()
        };
        let result = console.query(&query);
        for t in &result.traces {
            prop_assert_eq!(t.source, target);
        }
    }

    #[test]
    fn query_pagination_respects_limit_and_offset(
        limit in 1..5usize,
        offset in 0..3usize,
    ) {
        let mut console = ExplainabilityConsole::new(100);
        let total = 10;
        for i in 0..total {
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, i as u64);
            console.ingest(trace);
        }
        let query = TraceQuery {
            limit,
            offset,
            ..Default::default()
        };
        let result = console.query(&query);
        prop_assert_eq!(result.total_count, total);
        let expected_len = limit.min(total.saturating_sub(offset));
        prop_assert_eq!(result.traces.len(), expected_len);
    }
}

// =============================================================================
// Telemetry consistency
// =============================================================================

proptest! {
    #[test]
    fn telemetry_ingestion_count(n in 1..20usize) {
        let mut console = ExplainabilityConsole::new(100);
        for i in 0..n {
            let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, i as u64);
            console.ingest(trace);
        }
        prop_assert_eq!(console.telemetry().traces_ingested, n as u64);
    }

    #[test]
    fn telemetry_query_count(n_queries in 1..10usize) {
        let mut console = ExplainabilityConsole::new(100);
        let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, 1000);
        console.ingest(trace);
        for _ in 0..n_queries {
            let _ = console.query(&TraceQuery::all(10));
        }
        prop_assert_eq!(console.telemetry().queries_executed, n_queries as u64);
    }

    #[test]
    fn new_console_has_zero_telemetry(_dummy in 0..1u32) {
        let console = ExplainabilityConsole::new(100);
        let t = console.telemetry();
        prop_assert_eq!(t.traces_ingested, 0);
        prop_assert_eq!(t.traces_evicted, 0);
        prop_assert_eq!(t.queries_executed, 0);
        prop_assert_eq!(t.traces_matched, 0);
    }
}

// =============================================================================
// Correlation grouping
// =============================================================================

proptest! {
    #[test]
    fn correlated_traces_grouped(n in 2..6usize) {
        let mut console = ExplainabilityConsole::new(100);
        let mut first_id = 0;
        for i in 0..n {
            let mut trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, i as u64);
            trace.correlation_id = Some("op-test".to_string());
            let id = console.ingest(trace);
            if i == 0 { first_id = id; }
        }
        // Also add unrelated trace
        console.ingest(make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, 999));

        let correlated = console.get_correlated(first_id);
        prop_assert_eq!(correlated.len(), n - 1); // excludes self
    }

    #[test]
    fn uncorrelated_trace_returns_empty(_dummy in 0..1u32) {
        let mut console = ExplainabilityConsole::new(100);
        let trace = make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, 1000);
        let id = console.ingest(trace); // no correlation_id
        let correlated = console.get_correlated(id);
        prop_assert!(correlated.is_empty());
    }
}

// =============================================================================
// Causal link mechanics
// =============================================================================

proptest! {
    #[test]
    fn link_traces_succeeds_for_existing(rel in arb_causal_relationship()) {
        let mut console = ExplainabilityConsole::new(100);
        let id1 = console.ingest(make_test_trace(DecisionOutcome::Deny, TraceSource::Policy, None, 1000));
        let id2 = console.ingest(make_test_trace(DecisionOutcome::Allow, TraceSource::Policy, None, 1001));
        let linked = console.link_traces(id2, id1, rel, Some("test".into()));
        prop_assert!(linked);
        let trace = console.get_trace(id2).unwrap();
        prop_assert_eq!(trace.causal_links.len(), 1);
        prop_assert_eq!(trace.causal_links[0].relationship, rel);
    }

    #[test]
    fn link_nonexistent_fails(id in 1000..2000u64) {
        let mut console = ExplainabilityConsole::new(100);
        prop_assert!(!console.link_traces(id, 1, CausalRelationship::TriggeredBy, None));
    }
}

// =============================================================================
// Policy decision ingestion
// =============================================================================

proptest! {
    #[test]
    fn policy_deny_has_denied_severity(_dummy in 0..1u32) {
        let mut console = ExplainabilityConsole::new(100);
        let id = console.ingest_policy_decision(
            ActionKind::SendText,
            ActorKind::Robot,
            PolicySurface::Robot,
            Some(42),
            DecisionOutcome::Deny,
            None,
            "test".into(),
            1,
            1000,
            None,
        );
        let trace = console.get_trace(id).unwrap();
        prop_assert_eq!(trace.severity, TraceSeverity::Denied);
        prop_assert_eq!(trace.source, TraceSource::Policy);
    }

    #[test]
    fn policy_allow_has_info_severity(_dummy in 0..1u32) {
        let mut console = ExplainabilityConsole::new(100);
        let id = console.ingest_policy_decision(
            ActionKind::SendText,
            ActorKind::Robot,
            PolicySurface::Robot,
            None,
            DecisionOutcome::Allow,
            None,
            "ok".into(),
            1,
            1000,
            None,
        );
        let trace = console.get_trace(id).unwrap();
        prop_assert_eq!(trace.severity, TraceSeverity::Info);
    }

    #[test]
    fn policy_require_approval_has_warning_severity(_dummy in 0..1u32) {
        let mut console = ExplainabilityConsole::new(100);
        let id = console.ingest_policy_decision(
            ActionKind::SendText,
            ActorKind::Robot,
            PolicySurface::Robot,
            None,
            DecisionOutcome::RequireApproval,
            None,
            "needs approval".into(),
            1,
            1000,
            None,
        );
        let trace = console.get_trace(id).unwrap();
        prop_assert_eq!(trace.severity, TraceSeverity::Warning);
    }
}

// =============================================================================
// Render trace
// =============================================================================

proptest! {
    #[test]
    fn render_trace_not_empty(
        outcome in arb_decision_outcome(),
        source in arb_trace_source(),
    ) {
        let mut trace = make_test_trace(outcome, source, Some(1), 1000);
        trace.trace_id = 42;
        let rendered = ExplainabilityConsole::render_trace(&trace);
        prop_assert!(!rendered.is_empty());
        prop_assert!(rendered.contains("#42"));
    }
}

// =============================================================================
// Standard factory invariants
// =============================================================================

#[test]
fn new_console_is_empty() {
    let console = ExplainabilityConsole::new(100);
    assert!(console.is_empty());
    assert_eq!(console.len(), 0);
}

#[test]
fn get_trace_nonexistent_returns_none() {
    let console = ExplainabilityConsole::new(100);
    assert!(console.get_trace(999).is_none());
}

#[test]
fn severity_ordering_correct() {
    assert!(TraceSeverity::Info < TraceSeverity::Warning);
    assert!(TraceSeverity::Warning < TraceSeverity::Denied);
    assert!(TraceSeverity::Denied < TraceSeverity::Critical);
}
