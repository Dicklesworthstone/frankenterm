//! Property-based tests for recorder query executor invariants.
//!
//! Bead: wa-v35a
//!
//! Validates critical security and correctness properties:
//! 1. Authorization: no actor ever sees data above their effective tier
//! 2. Redaction: sensitive text is always masked/omitted for insufficient tiers
//! 3. Pagination: iterating all pages yields exactly the full result set
//! 4. Audit completeness: every execute() call generates exactly one audit entry
//! 5. Sensitivity filtering: min/max tier filters are respected
//! 6. Elevation: grants raise effective tier; expiry restores base tier
//! 7. RecorderQueryRequest serde roundtrip
//! 8. TimeRange serde roundtrip + contains invariant
//! 9. QueryEventKind serde roundtrip + snake_case
//! 10. QueryStats serde roundtrip + default
//! 11. RecorderQueryRequest required_tier logic

use proptest::prelude::*;

use frankenterm_core::policy::ActorKind;
use frankenterm_core::recorder_audit::{
    AccessTier, ActorIdentity, AuditLog, AuditLogConfig, AuthzDecision,
};
use frankenterm_core::recorder_query::{
    InMemoryEventStore, QueryError, QueryEventKind, QueryStats, RecorderQueryExecutor,
    RecorderQueryRequest, TimeRange,
};
use frankenterm_core::recorder_retention::SensitivityTier;
use frankenterm_core::recording::{
    RecorderEvent, RecorderEventCausality, RecorderEventPayload, RecorderEventSource,
    RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
    RECORDER_EVENT_SCHEMA_VERSION_V1,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_actor_kind() -> impl Strategy<Value = ActorKind> {
    prop_oneof![
        Just(ActorKind::Human),
        Just(ActorKind::Robot),
        Just(ActorKind::Mcp),
        Just(ActorKind::Workflow),
    ]
}

fn arb_redaction_level() -> impl Strategy<Value = RecorderRedactionLevel> {
    prop_oneof![
        Just(RecorderRedactionLevel::None),
        Just(RecorderRedactionLevel::Partial),
        Just(RecorderRedactionLevel::Full),
    ]
}

fn arb_event_source() -> impl Strategy<Value = RecorderEventSource> {
    prop_oneof![
        Just(RecorderEventSource::WeztermMux),
        Just(RecorderEventSource::RobotMode),
        Just(RecorderEventSource::WorkflowEngine),
        Just(RecorderEventSource::OperatorAction),
        Just(RecorderEventSource::RecoveryFlow),
    ]
}

fn arb_access_tier() -> impl Strategy<Value = AccessTier> {
    prop_oneof![
        Just(AccessTier::A0PublicMetadata),
        Just(AccessTier::A1RedactedQuery),
        Just(AccessTier::A2FullQuery),
        Just(AccessTier::A3PrivilegedRaw),
        Just(AccessTier::A4Admin),
    ]
}

fn arb_query_event_kind() -> impl Strategy<Value = QueryEventKind> {
    prop_oneof![
        Just(QueryEventKind::IngressText),
        Just(QueryEventKind::EgressOutput),
        Just(QueryEventKind::ControlMarker),
        Just(QueryEventKind::LifecycleMarker),
    ]
}

fn arb_text(max_len: usize) -> impl Strategy<Value = String> {
    proptest::string::string_regex(&format!("[a-zA-Z0-9 _=-]{{1,{}}}", max_len)).unwrap()
}

fn arb_event(pane_id: u64, seq: u64, ts_ms: u64) -> impl Strategy<Value = RecorderEvent> {
    (arb_text(80), arb_redaction_level(), arb_event_source()).prop_map(
        move |(text, redaction, source)| RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: format!("evt-{}-{}", pane_id, seq),
            pane_id,
            session_id: Some("sess-prop".into()),
            workflow_id: None,
            correlation_id: None,
            source,
            occurred_at_ms: ts_ms,
            recorded_at_ms: ts_ms + 1,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text,
                encoding: RecorderTextEncoding::Utf8,
                redaction,
                ingress_kind: RecorderIngressKind::SendText,
            },
        },
    )
}

fn arb_event_set(count: usize) -> impl Strategy<Value = Vec<RecorderEvent>> {
    let strategies: Vec<_> = (0..count)
        .map(|i| {
            let pane_id = (i % 5) as u64 + 1;
            let seq = i as u64;
            let ts_ms = 1000 + (i as u64) * 100;
            arb_event(pane_id, seq, ts_ms)
        })
        .collect();
    strategies
}

fn arb_time_range() -> impl Strategy<Value = TimeRange> {
    (0_u64..1_000_000, 1_u64..1_000_000).prop_map(|(start, delta)| TimeRange {
        start_ms: start,
        end_ms: start + delta,
    })
}

fn arb_query_stats() -> impl Strategy<Value = QueryStats> {
    (
        0_usize..10_000,
        0_usize..10_000,
        0_usize..10_000,
        0_usize..10_000,
    )
        .prop_map(|(scanned, matched, redacted, excluded)| QueryStats {
            events_scanned: scanned,
            events_matched: matched,
            events_redacted: redacted,
            events_excluded: excluded,
            ..Default::default()
        })
}

fn make_executor(events: Vec<RecorderEvent>) -> RecorderQueryExecutor<InMemoryEventStore> {
    let store = InMemoryEventStore::new();
    store.insert(events);
    RecorderQueryExecutor::new(store, AuditLog::new(AuditLogConfig::default()))
}

const NOW: u64 = 1700000000000;

// =============================================================================
// Property: Authorization — no actor exceeds their effective tier
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn authz_base_tier_respected(
        actor_kind in arb_actor_kind(),
        events in arb_event_set(10),
        include_text in proptest::bool::ANY,
    ) {
        let exec = make_executor(events);
        let actor = ActorIdentity::new(actor_kind, "test-actor");
        let base_tier = AccessTier::default_for_actor(actor_kind);

        // Single-pane metadata query (A0) — always allowed.
        let req = RecorderQueryRequest::default().with_text(include_text).with_limit(100);
        let result = exec.execute(&actor, &req, NOW);

        // If the query succeeded, the effective tier must satisfy the required tier.
        if let Ok(resp) = result {
            prop_assert!(
                resp.effective_tier.satisfies(req.required_tier()),
                "effective tier {} should satisfy required tier {}",
                resp.effective_tier, req.required_tier()
            );
            // Effective tier without elevation should equal base tier.
            prop_assert_eq!(resp.effective_tier, base_tier,
                "without elevation, effective tier should be base tier");
        }
    }

    #[test]
    fn authz_cross_pane_requires_a2(
        actor_kind in arb_actor_kind(),
        events in arb_event_set(10),
    ) {
        let exec = make_executor(events);
        let actor = ActorIdentity::new(actor_kind, "test-actor");
        let base_tier = AccessTier::default_for_actor(actor_kind);

        // Cross-pane query requires A2.
        let req = RecorderQueryRequest::for_panes(vec![1, 2, 3]);
        let result = exec.execute(&actor, &req, NOW);

        if base_tier.satisfies(AccessTier::A2FullQuery) {
            // Human, Workflow: should succeed.
            prop_assert!(result.is_ok(),
                "actor {:?} with base tier {} should pass cross-pane check", actor_kind, base_tier);
        } else {
            // Robot, Mcp: should fail with ElevationRequired (not Deny).
            prop_assert!(result.is_err(),
                "actor {:?} with base tier {} should fail cross-pane check", actor_kind, base_tier);
            if let Err(QueryError::ElevationRequired { .. }) = result {
                // Robot/Mcp can elevate to A2 per governance policy.
            } else if let Err(QueryError::AccessDenied { .. }) = result {
                // This is also acceptable for types that can't elevate.
            } else {
                prop_assert!(false, "unexpected error variant: {:?}", result);
            }
        }
    }
}

// =============================================================================
// Property: Redaction — sensitive text never leaks to insufficient tiers
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn redaction_t2_masked_for_a1(
        text in arb_text(40),
    ) {
        // Create a T2 event (Partial redaction → T2Sensitive).
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "evt-1-0".into(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1000,
            recorded_at_ms: 1001,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: text.clone(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::Partial,
                ingress_kind: RecorderIngressKind::SendText,
            },
        };

        let exec = make_executor(vec![event]);
        let robot = ActorIdentity::new(ActorKind::Robot, "bot");

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&robot, &req, NOW).unwrap();

        // Robot (A1) must see masked text for T2 data.
        prop_assert_eq!(resp.events.len(), 1);
        let result_text = resp.events[0].text.as_deref().unwrap_or("");
        // The original text must NOT appear verbatim (unless very short).
        if text.len() > 8 {
            prop_assert_ne!(result_text, text.as_str(),
                "T2 text should be masked for A1 robot");
        }
        prop_assert!(resp.events[0].redacted, "event should be marked as redacted");
    }

    #[test]
    fn redaction_t1_visible_to_all(
        text in arb_text(40),
        actor_kind in arb_actor_kind(),
    ) {
        // T1 event (None redaction → T1Standard).
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "evt-1-0".into(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1000,
            recorded_at_ms: 1001,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: text.clone(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        };

        let exec = make_executor(vec![event]);
        let actor = ActorIdentity::new(actor_kind, "actor");

        // Single-pane query (A1 required).
        let req = RecorderQueryRequest::for_panes(vec![1]);
        let result = exec.execute(&actor, &req, NOW);

        // All actor kinds have at least A1, so single-pane should succeed.
        let resp = result.unwrap();
        prop_assert_eq!(resp.events.len(), 1);

        // T1 text should be visible to everyone.
        prop_assert_eq!(resp.events[0].text.as_deref(), Some(text.as_str()),
            "T1 text should be visible to {:?}", actor_kind);
        prop_assert!(!resp.events[0].redacted, "T1 should not be marked as redacted");
    }
}

// =============================================================================
// Property: Pagination — full iteration yields all results
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pagination_yields_all_results(
        events in arb_event_set(20),
        page_size in 1_usize..10,
    ) {
        let exec = make_executor(events);
        let actor = ActorIdentity::new(ActorKind::Human, "pager");

        // Collect all events by paginating.
        let mut all_ids = Vec::new();
        let mut offset = 0;
        loop {
            let req = RecorderQueryRequest::default()
                .with_limit(page_size)
                .with_offset(offset);

            let resp = exec.execute(&actor, &req, NOW).unwrap();
            let page_len = resp.events.len();

            for e in &resp.events {
                all_ids.push(e.event_id.clone());
            }

            if !resp.has_more || page_len == 0 {
                break;
            }
            offset += page_len;

            // Safety: prevent infinite loops.
            if offset > 1000 {
                break;
            }
        }

        // Verify: no duplicates.
        let mut sorted = all_ids.clone();
        sorted.sort();
        sorted.dedup();
        prop_assert_eq!(all_ids.len(), sorted.len(),
            "pagination should not produce duplicates");

        // Get total count from a single large query.
        let full_req = RecorderQueryRequest::default().with_limit(1000);
        let full_resp = exec.execute(&actor, &full_req, NOW).unwrap();

        prop_assert_eq!(all_ids.len(), full_resp.events.len(),
            "paginated total ({}) should match single query total ({})",
            all_ids.len(), full_resp.events.len());
    }
}

// =============================================================================
// Property: Audit completeness — every execute generates one audit entry
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn audit_entry_per_execute(
        actor_kind in arb_actor_kind(),
        events in arb_event_set(5),
        query_count in 1_u32..5,
    ) {
        let exec = make_executor(events);
        let actor = ActorIdentity::new(actor_kind, "audited");

        let mut expected_entries = 0;
        for _ in 0..query_count {
            // Single-pane query: always succeeds for any actor kind.
            let req = RecorderQueryRequest::for_panes(vec![1]);
            let _ = exec.execute(&actor, &req, NOW);
            expected_entries += 1;
        }

        let entries = exec.audit_log().entries();
        prop_assert_eq!(entries.len(), expected_entries,
            "expected {} audit entries, got {}", expected_entries, entries.len());

        // Each entry should reference the correct actor.
        for e in &entries {
            prop_assert_eq!(e.actor.kind, actor_kind,
                "audit entry actor kind should match");
        }
    }

    #[test]
    fn denied_queries_also_audited(
        events in arb_event_set(5),
    ) {
        let exec = make_executor(events);
        let robot = ActorIdentity::new(ActorKind::Robot, "restricted");

        // Cross-pane query: denied/elevation for robot.
        let req = RecorderQueryRequest::for_panes(vec![1, 2]);
        let _ = exec.execute(&robot, &req, NOW);

        let entries = exec.audit_log().entries();
        prop_assert_eq!(entries.len(), 1, "denied query should still produce audit entry");

        let decision = &entries[0].decision;
        prop_assert!(
            *decision == AuthzDecision::Deny || *decision == AuthzDecision::Elevate,
            "denied query audit should have Deny or Elevate decision, got {:?}", decision
        );
    }
}

// =============================================================================
// Property: Sensitivity filtering respected
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn sensitivity_filter_excludes_lower_tiers(
        events in arb_event_set(15),
    ) {
        let exec = make_executor(events);
        let actor = ActorIdentity::new(ActorKind::Human, "filter-test");

        // Only T2+ events.
        let mut req = RecorderQueryRequest::default().with_limit(100);
        req.min_sensitivity = Some(SensitivityTier::T2Sensitive);

        let resp = exec.execute(&actor, &req, NOW).unwrap();

        for e in &resp.events {
            prop_assert!(
                e.sensitivity >= SensitivityTier::T2Sensitive,
                "event with sensitivity {:?} should not pass T2+ filter", e.sensitivity
            );
        }
    }

    #[test]
    fn sensitivity_filter_excludes_higher_tiers(
        events in arb_event_set(15),
    ) {
        let exec = make_executor(events);
        let actor = ActorIdentity::new(ActorKind::Human, "filter-test");

        // Only T1 events.
        let mut req = RecorderQueryRequest::default().with_limit(100);
        req.max_sensitivity = Some(SensitivityTier::T1Standard);

        let resp = exec.execute(&actor, &req, NOW).unwrap();

        for e in &resp.events {
            prop_assert_eq!(
                e.sensitivity, SensitivityTier::T1Standard,
                "event with sensitivity {:?} should not pass T1-only filter", e.sensitivity
            );
        }
    }
}

// =============================================================================
// Property: Elevation raises effective tier
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn elevation_raises_effective_tier(
        target_tier in arb_access_tier(),
    ) {
        let events = vec![
            RecorderEvent {
                schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                event_id: "evt-1-0".into(),
                pane_id: 1,
                session_id: None,
                workflow_id: None,
                correlation_id: None,
                source: RecorderEventSource::WeztermMux,
                occurred_at_ms: 1000,
                recorded_at_ms: 1001,
                sequence: 0,
                causality: RecorderEventCausality {
                    parent_event_id: None,
                    trigger_event_id: None,
                    root_event_id: None,
                },
                payload: RecorderEventPayload::IngressText {
                    text: "test".to_string(),
                    encoding: RecorderTextEncoding::Utf8,
                    redaction: RecorderRedactionLevel::None,
                    ingress_kind: RecorderIngressKind::SendText,
                },
            },
        ];

        let exec = make_executor(events);
        let actor = ActorIdentity::new(ActorKind::Robot, "elevated-bot");

        // Grant elevation.
        exec.grant_elevation(
            actor.clone(),
            target_tier,
            "property test".into(),
            NOW,
            60_000,
        );

        // Simple query (A1 required).
        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&actor, &req, NOW).unwrap();

        let base_tier = AccessTier::default_for_actor(ActorKind::Robot);
        let expected = if target_tier > base_tier { target_tier } else { base_tier };
        prop_assert_eq!(resp.effective_tier, expected,
            "elevated tier should be max(base={}, grant={})", base_tier, target_tier);
    }

    #[test]
    fn elevation_expiry_restores_base(
        ttl_ms in 100_u64..10_000,
    ) {
        let exec = make_executor(vec![]);
        let actor = ActorIdentity::new(ActorKind::Robot, "expiring-bot");

        exec.grant_elevation(
            actor.clone(),
            AccessTier::A3PrivilegedRaw,
            "temp".into(),
            NOW,
            ttl_ms,
        );

        prop_assert_eq!(exec.active_grants(), 1);

        // Expire after TTL.
        let expired = exec.expire_grants(NOW + ttl_ms + 1);
        prop_assert_eq!(expired, 1, "grant should expire after TTL");
        prop_assert_eq!(exec.active_grants(), 0);
    }
}

// =============================================================================
// Property: Response stability — same query, same results
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn same_query_same_results(
        events in arb_event_set(10),
    ) {
        let exec = make_executor(events);
        let actor = ActorIdentity::new(ActorKind::Human, "stable");

        let req = RecorderQueryRequest::default().with_limit(100);

        let resp1 = exec.execute(&actor, &req, NOW).unwrap();
        let resp2 = exec.execute(&actor, &req, NOW).unwrap();

        prop_assert_eq!(resp1.events.len(), resp2.events.len(),
            "same query should return same number of events");
        prop_assert_eq!(resp1.total_count, resp2.total_count,
            "same query should report same total count");

        for (a, b) in resp1.events.iter().zip(resp2.events.iter()) {
            prop_assert_eq!(&a.event_id, &b.event_id,
                "same query should return events in same order");
        }
    }
}

// =============================================================================
// Property: Redaction mask preserves length
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mask_preserves_length(
        text in arb_text(100),
    ) {
        // Create T2 event.
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: "evt-1-0".into(),
            pane_id: 1,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1000,
            recorded_at_ms: 1001,
            sequence: 0,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: text.clone(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::Partial,
                ingress_kind: RecorderIngressKind::SendText,
            },
        };

        let exec = make_executor(vec![event]);
        let robot = ActorIdentity::new(ActorKind::Robot, "mask-test");

        let req = RecorderQueryRequest::for_panes(vec![1]);
        let resp = exec.execute(&robot, &req, NOW).unwrap();

        if let Some(masked) = &resp.events[0].text {
            // Masked text should be same length as original.
            prop_assert_eq!(masked.len(), text.len(),
                "masked text length ({}) should equal original ({})", masked.len(), text.len());
        }
    }
}

// =============================================================================
// Serde: RecorderQueryRequest roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn prop_query_request_serde_roundtrip(
        limit in 1_usize..1000,
        offset in 0_usize..500,
        include_text in any::<bool>(),
        pane_ids in prop::collection::vec(0_u64..1000, 0..5),
    ) {
        let req = RecorderQueryRequest {
            time_range: None,
            pane_ids,
            sources: vec![],
            text_pattern: None,
            limit,
            offset,
            include_text,
            min_sensitivity: None,
            max_sensitivity: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: RecorderQueryRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.limit, req.limit);
        prop_assert_eq!(back.offset, req.offset);
        prop_assert_eq!(back.include_text, req.include_text);
        prop_assert_eq!(back.pane_ids.len(), req.pane_ids.len());
    }

    #[test]
    fn prop_query_request_default_from_empty_json(_dummy in 0..1_u8) {
        let req: RecorderQueryRequest = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(req.limit, 100, "default limit should be 100");
        prop_assert!(req.include_text, "default include_text should be true");
        prop_assert!(req.pane_ids.is_empty());
        prop_assert!(req.time_range.is_none());
        prop_assert!(req.text_pattern.is_none());
    }
}

// =============================================================================
// Serde: TimeRange roundtrip + contains invariant
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn prop_time_range_serde_roundtrip(range in arb_time_range()) {
        let json = serde_json::to_string(&range).unwrap();
        let back: TimeRange = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.start_ms, range.start_ms);
        prop_assert_eq!(back.end_ms, range.end_ms);
    }

    #[test]
    fn prop_time_range_contains_endpoints(range in arb_time_range()) {
        // Start and end are both inclusive.
        prop_assert!(range.contains(range.start_ms), "range should contain start");
        prop_assert!(range.contains(range.end_ms), "range should contain end");
        // One before start should be excluded.
        if range.start_ms > 0 {
            prop_assert!(!range.contains(range.start_ms - 1), "range should exclude start-1");
        }
        // One after end should be excluded.
        if range.end_ms < u64::MAX {
            prop_assert!(!range.contains(range.end_ms + 1), "range should exclude end+1");
        }
    }
}

// =============================================================================
// Serde: QueryEventKind roundtrip + snake_case
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_query_event_kind_serde(kind in arb_query_event_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: QueryEventKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
    }

    #[test]
    fn prop_query_event_kind_snake_case(kind in arb_query_event_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let expected = match kind {
            QueryEventKind::IngressText => "\"ingress_text\"",
            QueryEventKind::EgressOutput => "\"egress_output\"",
            QueryEventKind::ControlMarker => "\"control_marker\"",
            QueryEventKind::LifecycleMarker => "\"lifecycle_marker\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }
}

// =============================================================================
// Serde: QueryStats roundtrip + default
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn prop_query_stats_serde_roundtrip(stats in arb_query_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let back: QueryStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.events_scanned, stats.events_scanned);
        prop_assert_eq!(back.events_matched, stats.events_matched);
        prop_assert_eq!(back.events_redacted, stats.events_redacted);
        prop_assert_eq!(back.events_excluded, stats.events_excluded);
    }

    #[test]
    fn prop_query_stats_default(_dummy in 0..1_u8) {
        let stats = QueryStats::default();
        prop_assert_eq!(stats.events_scanned, 0);
        prop_assert_eq!(stats.events_matched, 0);
        prop_assert_eq!(stats.events_redacted, 0);
        prop_assert_eq!(stats.events_excluded, 0);
        // Duration has #[serde(skip)], so shouldn't appear in JSON.
        let json = serde_json::to_string(&stats).unwrap();
        prop_assert!(!json.contains("duration"), "duration should be skipped in serde");
    }
}

// =============================================================================
// Serde: RecorderQueryRequest required_tier logic
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// Metadata-only queries (include_text=false) always require A0.
    #[test]
    fn prop_metadata_only_requires_a0(
        pane_ids in prop::collection::vec(0_u64..100, 0..5),
    ) {
        let req = RecorderQueryRequest {
            include_text: false,
            pane_ids,
            ..Default::default()
        };
        prop_assert_eq!(req.required_tier(), AccessTier::A0PublicMetadata);
    }

    /// Cross-pane text queries require A2.
    #[test]
    fn prop_cross_pane_requires_a2(
        pane_count in 2_usize..10,
    ) {
        let pane_ids: Vec<u64> = (0..pane_count as u64).collect();
        let req = RecorderQueryRequest::for_panes(pane_ids);
        prop_assert_eq!(req.required_tier(), AccessTier::A2FullQuery);
    }

    /// Text search queries require A2.
    #[test]
    fn prop_text_search_requires_a2(
        pattern in "[a-z]{3,20}",
    ) {
        let req = RecorderQueryRequest::text_search(pattern);
        prop_assert_eq!(req.required_tier(), AccessTier::A2FullQuery);
    }
}
