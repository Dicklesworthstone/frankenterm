//! Property tests for session_workflow_explorer module.

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::session_workflow_explorer::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_event_category() -> impl Strategy<Value = EventCategory> {
    prop_oneof![
        Just(EventCategory::Output),
        Just(EventCategory::Input),
        Just(EventCategory::PolicyDecision),
        Just(EventCategory::WorkflowStep),
        Just(EventCategory::WorkflowLifecycle),
        Just(EventCategory::PatternMatch),
        Just(EventCategory::Snapshot),
        Just(EventCategory::Error),
        Just(EventCategory::Resize),
        Just(EventCategory::Marker),
        Just(EventCategory::StateChange),
        Just(EventCategory::Intervention),
    ]
}

fn arb_event_severity() -> impl Strategy<Value = EventSeverity> {
    prop_oneof![
        Just(EventSeverity::Trace),
        Just(EventSeverity::Info),
        Just(EventSeverity::Notice),
        Just(EventSeverity::Warning),
        Just(EventSeverity::Error),
        Just(EventSeverity::Critical),
    ]
}

fn arb_event_source() -> impl Strategy<Value = EventSource> {
    prop_oneof![
        Just(EventSource::Recording),
        Just(EventSource::Policy),
        Just(EventSource::Workflow),
        Just(EventSource::Patterns),
        Just(EventSource::ContextSnapshot),
        Just(EventSource::DiffSnapshot),
        Just(EventSource::Explainability),
        Just(EventSource::Robot),
        Just(EventSource::Operator),
        Just(EventSource::System),
    ]
}

fn arb_event_relationship() -> impl Strategy<Value = EventRelationship> {
    prop_oneof![
        Just(EventRelationship::Caused),
        Just(EventRelationship::CausedBy),
        Just(EventRelationship::RetryOf),
        Just(EventRelationship::CompensationOf),
        Just(EventRelationship::Correlated),
    ]
}

fn arb_step_outcome() -> impl Strategy<Value = StepOutcome> {
    prop_oneof![
        Just(StepOutcome::Completed),
        Just(StepOutcome::Running),
        Just(StepOutcome::Waiting),
        Just(StepOutcome::Retried),
        Just(StepOutcome::Aborted),
        Just(StepOutcome::Failed),
        Just(StepOutcome::Skipped),
    ]
}

fn arb_workflow_trace_status() -> impl Strategy<Value = WorkflowTraceStatus> {
    prop_oneof![
        Just(WorkflowTraceStatus::Running),
        Just(WorkflowTraceStatus::Completed),
        Just(WorkflowTraceStatus::Aborted),
        Just(WorkflowTraceStatus::Failed),
        Just(WorkflowTraceStatus::Waiting),
    ]
}

fn arb_pane_change_type() -> impl Strategy<Value = PaneChangeType> {
    prop_oneof![
        Just(PaneChangeType::Created),
        Just(PaneChangeType::Closed),
        Just(PaneChangeType::OutputChanged),
        Just(PaneChangeType::Resized),
        Just(PaneChangeType::TitleChanged),
    ]
}

fn arb_timeline_event() -> impl Strategy<Value = TimelineEvent> {
    (
        any::<u64>(),
        any::<u64>(),
        proptest::option::of(any::<u64>()),
        arb_event_category(),
        arb_event_severity(),
        arb_event_source(),
        any::<bool>(),
    )
        .prop_map(
            |(event_id, timestamp_ms, pane_id, category, severity, source, is_intervention)| {
                TimelineEvent {
                    event_id,
                    timestamp_ms,
                    pane_id,
                    category,
                    severity,
                    summary: format!("test event {}", event_id),
                    source,
                    details: HashMap::new(),
                    correlation_id: None,
                    related_events: Vec::new(),
                    workflow_step: None,
                    is_intervention_point: is_intervention,
                }
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_event_category(c in arb_event_category()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: EventCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c, back);
    }

    #[test]
    fn serde_roundtrip_event_severity(s in arb_event_severity()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: EventSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn serde_roundtrip_event_source(s in arb_event_source()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: EventSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn serde_roundtrip_event_relationship(r in arb_event_relationship()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: EventRelationship = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r, back);
    }

    #[test]
    fn serde_roundtrip_step_outcome(o in arb_step_outcome()) {
        let json = serde_json::to_string(&o).unwrap();
        let back: StepOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(o, back);
    }

    #[test]
    fn serde_roundtrip_workflow_trace_status(s in arb_workflow_trace_status()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: WorkflowTraceStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn serde_roundtrip_pane_change_type(p in arb_pane_change_type()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: PaneChangeType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(p, back);
    }

    #[test]
    fn serde_roundtrip_timeline_event(e in arb_timeline_event()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: TimelineEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(e.event_id, back.event_id);
        prop_assert_eq!(e.timestamp_ms, back.timestamp_ms);
        prop_assert_eq!(e.category, back.category);
        prop_assert_eq!(e.severity, back.severity);
        prop_assert_eq!(e.source, back.source);
    }

    #[test]
    fn serde_roundtrip_timeline_query(_dummy in Just(())) {
        let query = TimelineQuery::all(100);
        let json = serde_json::to_string(&query).unwrap();
        let back: TimelineQuery = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(query.limit, back.limit);
    }
}

// =============================================================================
// EventSeverity ordering tests
// =============================================================================

proptest! {
    #[test]
    fn severity_ordering_total(_dummy in Just(())) {
        prop_assert!(EventSeverity::Trace < EventSeverity::Info);
        prop_assert!(EventSeverity::Info < EventSeverity::Notice);
        prop_assert!(EventSeverity::Notice < EventSeverity::Warning);
        prop_assert!(EventSeverity::Warning < EventSeverity::Error);
        prop_assert!(EventSeverity::Error < EventSeverity::Critical);
    }

    #[test]
    fn severity_ordering_reflexive(s in arb_event_severity()) {
        prop_assert!(s <= s);
        prop_assert!(s >= s);
    }

    #[test]
    fn severity_ordering_transitive(
        a in arb_event_severity(),
        b in arb_event_severity(),
        c in arb_event_severity(),
    ) {
        if a <= b && b <= c {
            prop_assert!(a <= c);
        }
    }
}

// =============================================================================
// TimelineQuery tests
// =============================================================================

proptest! {
    #[test]
    fn query_all_has_limit(limit in 1usize..1000) {
        let q = TimelineQuery::all(limit);
        prop_assert_eq!(q.limit, limit);
        prop_assert!(q.pane_id.is_none());
        prop_assert!(q.category.is_none());
    }

    #[test]
    fn query_for_pane_filters_pane(pane_id in any::<u64>(), limit in 1usize..100) {
        let q = TimelineQuery::for_pane(pane_id, limit);
        prop_assert_eq!(q.pane_id, Some(pane_id));
        prop_assert_eq!(q.limit, limit);
    }

    #[test]
    fn query_errors_sets_min_severity(limit in 1usize..100) {
        let q = TimelineQuery::errors(limit);
        prop_assert_eq!(q.min_severity, Some(EventSeverity::Error));
    }

    #[test]
    fn query_interventions_sets_flag(limit in 1usize..100) {
        let q = TimelineQuery::interventions(limit);
        prop_assert!(q.interventions_only);
    }

    #[test]
    fn query_by_correlation(corr in "[a-z]{5,10}", limit in 1usize..100) {
        let q = TimelineQuery::by_correlation(&corr, limit);
        prop_assert_eq!(q.correlation_id, Some(corr));
    }

    #[test]
    fn query_time_range(since in 0u64..1000, until in 1000u64..2000, limit in 1usize..100) {
        let q = TimelineQuery::time_range(since, until, limit);
        prop_assert_eq!(q.since_ms, Some(since));
        prop_assert_eq!(q.until_ms, Some(until));
    }

    #[test]
    fn query_search(text in "[a-z]{3,10}", limit in 1usize..100) {
        let q = TimelineQuery::search(&text, limit);
        prop_assert_eq!(q.search_text, Some(text));
    }
}

// =============================================================================
// TimelineQuery::matches tests
// =============================================================================

fn make_event(
    event_id: u64,
    timestamp_ms: u64,
    pane_id: Option<u64>,
    category: EventCategory,
    severity: EventSeverity,
    source: EventSource,
    summary: &str,
    is_intervention: bool,
    correlation_id: Option<String>,
) -> TimelineEvent {
    TimelineEvent {
        event_id,
        timestamp_ms,
        pane_id,
        category,
        severity,
        summary: summary.to_string(),
        source,
        details: HashMap::new(),
        correlation_id,
        related_events: Vec::new(),
        workflow_step: None,
        is_intervention_point: is_intervention,
    }
}

proptest! {
    #[test]
    fn query_all_matches_everything(e in arb_timeline_event()) {
        let q = TimelineQuery::all(100);
        prop_assert!(q.matches(&e));
    }

    #[test]
    fn query_pane_id_filters(pane_id in any::<u64>(), other in any::<u64>()) {
        let e = make_event(1, 100, Some(pane_id), EventCategory::Output, EventSeverity::Info, EventSource::Recording, "test", false, None);
        let q = TimelineQuery::for_pane(pane_id, 100);
        prop_assert!(q.matches(&e));

        if other != pane_id {
            let q2 = TimelineQuery::for_pane(other, 100);
            prop_assert!(!q2.matches(&e));
        }
    }

    #[test]
    fn query_min_severity_filters(sev in arb_event_severity()) {
        let e = make_event(1, 100, None, EventCategory::Error, sev, EventSource::System, "test", false, None);
        let q_trace = TimelineQuery { min_severity: Some(EventSeverity::Trace), limit: 100, ..Default::default() };
        let q_critical = TimelineQuery { min_severity: Some(EventSeverity::Critical), limit: 100, ..Default::default() };
        // Everything matches trace
        prop_assert!(q_trace.matches(&e));
        // Only critical events match critical filter
        if sev < EventSeverity::Critical {
            prop_assert!(!q_critical.matches(&e));
        }
    }

    #[test]
    fn query_interventions_only_filters(is_intervention in any::<bool>()) {
        let e = make_event(1, 100, None, EventCategory::Intervention, EventSeverity::Info, EventSource::Robot, "test", is_intervention, None);
        let q = TimelineQuery::interventions(100);
        if is_intervention {
            prop_assert!(q.matches(&e));
        } else {
            prop_assert!(!q.matches(&e));
        }
    }

    #[test]
    fn query_time_range_filters(ts in 0u64..3000) {
        let e = make_event(1, ts, None, EventCategory::Output, EventSeverity::Info, EventSource::Recording, "test", false, None);
        let q = TimelineQuery::time_range(1000, 2000, 100);
        if ts >= 1000 && ts < 2000 {
            prop_assert!(q.matches(&e));
        } else {
            prop_assert!(!q.matches(&e));
        }
    }

    #[test]
    fn query_search_text_case_insensitive(_dummy in Just(())) {
        let e = make_event(1, 100, None, EventCategory::Output, EventSeverity::Info, EventSource::Recording, "Hello World Test", false, None);
        let q = TimelineQuery::search("hello world", 100);
        prop_assert!(q.matches(&e));
    }
}

// =============================================================================
// SessionWorkflowExplorer tests
// =============================================================================

proptest! {
    #[test]
    fn explorer_ingest_assigns_sequential_ids(count in 1usize..=20) {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let mut ids = Vec::new();
        for i in 0..count {
            let event = make_event(0, i as u64 * 100, None, EventCategory::Output, EventSeverity::Info, EventSource::Recording, &format!("event {}", i), false, None);
            ids.push(explorer.ingest(event));
        }
        // IDs should be sequential starting from 1
        for (i, id) in ids.iter().enumerate() {
            prop_assert_eq!(*id, (i + 1) as u64);
        }
    }

    #[test]
    fn explorer_capacity_eviction(capacity in 5usize..=20, extra in 1usize..=10) {
        let mut explorer = SessionWorkflowExplorer::new(capacity);
        let total = capacity + extra;
        for i in 0..total {
            let event = make_event(0, i as u64 * 100, None, EventCategory::Output, EventSeverity::Info, EventSource::Recording, &format!("event {}", i), false, None);
            explorer.ingest(event);
        }
        // Query all should return at most capacity events
        let result = explorer.query(&TimelineQuery::all(1000));
        prop_assert!(result.events.len() <= capacity);
    }

    #[test]
    fn explorer_query_respects_limit(count in 5usize..=20, limit in 1usize..=5) {
        let mut explorer = SessionWorkflowExplorer::new(100);
        for i in 0..count {
            let event = make_event(0, i as u64 * 100, None, EventCategory::Output, EventSeverity::Info, EventSource::Recording, &format!("event {}", i), false, None);
            explorer.ingest(event);
        }
        let result = explorer.query(&TimelineQuery::all(limit));
        prop_assert!(result.events.len() <= limit);
        prop_assert_eq!(result.total_count, count);
    }

    #[test]
    fn explorer_get_event_by_id(count in 1usize..=10) {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let mut ids = Vec::new();
        for i in 0..count {
            let event = make_event(0, i as u64 * 100, None, EventCategory::Output, EventSeverity::Info, EventSource::Recording, &format!("event {}", i), false, None);
            ids.push(explorer.ingest(event));
        }
        for id in &ids {
            let found = explorer.get_event(*id);
            prop_assert!(found.is_some());
            prop_assert_eq!(found.unwrap().event_id, *id);
        }
        // Non-existent ID
        prop_assert!(explorer.get_event(9999).is_none());
    }

    #[test]
    fn explorer_diff_counts(error_count in 0usize..=5, normal_count in 0usize..=5) {
        let mut explorer = SessionWorkflowExplorer::new(100);
        for i in 0..error_count {
            explorer.ingest_error(Some(0), &format!("error {}", i), EventSource::System, 500 + i as u64 * 10, None);
        }
        for i in 0..normal_count {
            let event = make_event(0, 500 + (error_count + i) as u64 * 10, Some(0), EventCategory::Output, EventSeverity::Info, EventSource::Recording, &format!("normal {}", i), false, None);
            explorer.ingest(event);
        }
        let diff = explorer.diff(0, 10000);
        prop_assert_eq!(diff.events_between, error_count + normal_count);
        prop_assert_eq!(diff.errors_between, error_count);
    }

    #[test]
    fn explorer_extract_separates_categories(_dummy in Just(())) {
        let mut explorer = SessionWorkflowExplorer::new(100);
        explorer.ingest_error(Some(0), "test error", EventSource::System, 100, None);
        explorer.ingest_intervention(Some(0), "test intervention", EventSource::Robot, 200, None);

        let extraction = explorer.extract(0, 1000);
        prop_assert_eq!(extraction.errors.len(), 1);
        prop_assert_eq!(extraction.interventions.len(), 1);
        prop_assert_eq!(extraction.total_examined, 2);
    }
}

// =============================================================================
// WorkflowTrace tests
// =============================================================================

proptest! {
    #[test]
    fn explorer_register_and_get_workflow_trace(exec_id in "[a-z]{5,10}") {
        let mut explorer = SessionWorkflowExplorer::new(100);
        let trace = WorkflowTrace {
            execution_id: exec_id.clone(),
            workflow_name: "test_wf".into(),
            pane_id: Some(0),
            steps: Vec::new(),
            status: WorkflowTraceStatus::Completed,
            started_ms: 100,
            completed_ms: Some(200),
            duration_ms: Some(100),
            correlation_id: None,
        };
        explorer.register_workflow_trace(trace);
        let found = explorer.get_workflow_trace(&exec_id);
        prop_assert!(found.is_some());
        prop_assert_eq!(&found.unwrap().execution_id, &exec_id);
    }

    #[test]
    fn explorer_list_workflow_traces_sorted(count in 1usize..=5) {
        let mut explorer = SessionWorkflowExplorer::new(100);
        for i in 0..count {
            let trace = WorkflowTrace {
                execution_id: format!("exec-{}", i),
                workflow_name: "wf".into(),
                pane_id: None,
                steps: Vec::new(),
                status: WorkflowTraceStatus::Completed,
                started_ms: (count - i) as u64 * 100, // reverse order
                completed_ms: None,
                duration_ms: None,
                correlation_id: None,
            };
            explorer.register_workflow_trace(trace);
        }
        let traces = explorer.list_workflow_traces();
        for i in 1..traces.len() {
            prop_assert!(traces[i].started_ms >= traces[i - 1].started_ms);
        }
    }
}

// =============================================================================
// Telemetry tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_explorer_telemetry(_dummy in Just(())) {
        let telemetry = ExplorerTelemetry {
            events_ingested: 42,
            events_evicted: 3,
            queries_executed: 10,
            extractions_performed: 2,
            diffs_computed: 1,
            workflow_traces_tracked: 5,
        };
        let json = serde_json::to_string(&telemetry).unwrap();
        let back: ExplorerTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(telemetry.events_ingested, back.events_ingested);
        prop_assert_eq!(telemetry.events_evicted, back.events_evicted);
    }
}

// =============================================================================
// ExtractedEvent From<TimelineEvent> test
// =============================================================================

proptest! {
    #[test]
    fn extracted_event_from_timeline_preserves_fields(e in arb_timeline_event()) {
        let extracted = ExtractedEvent::from(&e);
        prop_assert_eq!(extracted.event_id, e.event_id);
        prop_assert_eq!(extracted.timestamp_ms, e.timestamp_ms);
        prop_assert_eq!(extracted.pane_id, e.pane_id);
        prop_assert_eq!(extracted.category, e.category);
        prop_assert_eq!(extracted.severity, e.severity);
        prop_assert_eq!(extracted.summary, e.summary);
    }
}
