//! Property-based tests for Replay Side-Effect Barrier (ft-og6q6.3.3).
//!
//! Verifies invariants of the SideEffectBarrier trait, ReplayBarrier,
//! LiveBarrier, CounterfactualBarrier, and SideEffectLog.

use frankenterm_core::policy::ActionKind;
use frankenterm_core::replay_side_effect_barrier::{
    CounterfactualBarrier, EffectRequest, EffectType, LiveBarrier, OverrideRule, ReplayBarrier,
    SideEffectBarrier, SideEffectEntry, SideEffectLog,
};
use proptest::prelude::*;
use std::collections::HashMap;

// ── Strategies ─────────────────────────────────────────────────────────

fn arb_effect_type() -> impl Strategy<Value = EffectType> {
    prop_oneof![
        Just(EffectType::SendKeys),
        Just(EffectType::SpawnProcess),
        Just(EffectType::ApiCall),
        Just(EffectType::FileWrite),
        Just(EffectType::EmitNotification),
        Just(EffectType::SendControl),
        Just(EffectType::ExecCommand),
        Just(EffectType::ClosePane),
    ]
}

fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::SendCtrlC),
        Just(ActionKind::SendCtrlD),
        Just(ActionKind::SendCtrlZ),
        Just(ActionKind::SendControl),
        Just(ActionKind::Spawn),
        Just(ActionKind::Split),
        Just(ActionKind::Activate),
        Just(ActionKind::Close),
        Just(ActionKind::BrowserAuth),
        Just(ActionKind::WorkflowRun),
        Just(ActionKind::ReservePane),
        Just(ActionKind::ReleasePane),
        Just(ActionKind::ReadOutput),
        Just(ActionKind::SearchOutput),
        Just(ActionKind::WriteFile),
        Just(ActionKind::DeleteFile),
        Just(ActionKind::ExecCommand),
    ]
}

fn arb_request() -> impl Strategy<Value = EffectRequest> {
    (
        0..100_000_u64,
        arb_effect_type(),
        prop::option::of(1..100_u64),
        "[a-zA-Z0-9 _/.-]{0,100}",
        "[a-zA-Z0-9::_]{1,50}",
        arb_action_kind(),
    )
        .prop_map(
            |(timestamp_ms, effect_type, pane_id, payload, caller, action_kind)| EffectRequest {
                timestamp_ms,
                effect_type,
                pane_id,
                payload,
                caller,
                action_kind,
                metadata: HashMap::new(),
            },
        )
}

fn arb_override_rule() -> impl Strategy<Value = OverrideRule> {
    (
        arb_effect_type(),
        prop::option::of(1..50_u64),
        prop::option::of("[a-z]{1,10}"),
        "[a-zA-Z0-9 ]{1,30}",
        "[a-zA-Z ]{1,30}",
    )
        .prop_map(
            |(effect_type, pane_id, payload_contains, replacement_payload, description)| {
                OverrideRule {
                    effect_type,
                    pane_id,
                    payload_contains,
                    replacement_payload,
                    description,
                }
            },
        )
}

// ── SideEffectLog Properties ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // LOG-1: Log length equals number of records
    #[test]
    fn log_length_matches_records(n in 0..50_usize) {
        let log = SideEffectLog::new();
        for i in 0..n {
            log.record(SideEffectEntry {
                index: 0,
                timestamp_ms: i as u64,
                effect_type: EffectType::SendKeys,
                pane_id: Some(1),
                payload_summary: "x".to_string(),
                caller_hint: "t".to_string(),
                action_kind: ActionKind::SendText,
                metadata: HashMap::new(),
            });
        }
        prop_assert_eq!(log.len(), n);
    }

    // LOG-2: Indices are monotonically increasing
    #[test]
    fn log_indices_monotone(n in 1..30_usize) {
        let log = SideEffectLog::new();
        for i in 0..n {
            log.record(SideEffectEntry {
                index: 999, // Will be overwritten
                timestamp_ms: i as u64 * 10,
                effect_type: EffectType::SendKeys,
                pane_id: Some(1),
                payload_summary: format!("msg_{i}"),
                caller_hint: "t".to_string(),
                action_kind: ActionKind::SendText,
                metadata: HashMap::new(),
            });
        }
        let entries = log.entries();
        for i in 0..entries.len() {
            prop_assert_eq!(entries[i].index, i);
        }
    }

    // LOG-3: Filter by type returns subset
    #[test]
    fn log_type_filter_subset(
        requests in prop::collection::vec(arb_request(), 1..30),
    ) {
        let log = SideEffectLog::new();
        for req in &requests {
            log.record(SideEffectEntry {
                index: 0,
                timestamp_ms: req.timestamp_ms,
                effect_type: req.effect_type,
                pane_id: req.pane_id,
                payload_summary: req.payload.clone(),
                caller_hint: req.caller.clone(),
                action_kind: req.action_kind,
                metadata: HashMap::new(),
            });
        }
        let total = log.len();
        let mut sum = 0;
        let types = [
            EffectType::SendKeys, EffectType::SpawnProcess, EffectType::ApiCall,
            EffectType::FileWrite, EffectType::EmitNotification, EffectType::SendControl,
            EffectType::ExecCommand, EffectType::ClosePane,
        ];
        for et in types {
            let filtered = log.effects_of_type(et);
            sum += filtered.len();
            for entry in &filtered {
                prop_assert_eq!(entry.effect_type, et);
            }
        }
        prop_assert_eq!(sum, total, "Union of all type filters must equal total");
    }

    // LOG-4: Filter by pane returns correct subset
    #[test]
    fn log_pane_filter_correct(
        pane_id in 1..10_u64,
        requests in prop::collection::vec(arb_request(), 1..30),
    ) {
        let log = SideEffectLog::new();
        let mut expected = 0;
        for req in &requests {
            if req.pane_id == Some(pane_id) {
                expected += 1;
            }
            log.record(SideEffectEntry {
                index: 0,
                timestamp_ms: req.timestamp_ms,
                effect_type: req.effect_type,
                pane_id: req.pane_id,
                payload_summary: req.payload.clone(),
                caller_hint: req.caller.clone(),
                action_kind: req.action_kind,
                metadata: HashMap::new(),
            });
        }
        let filtered = log.effects_for_pane(pane_id);
        prop_assert_eq!(filtered.len(), expected);
    }

    // LOG-5: JSON serde roundtrip preserves all entries
    #[test]
    fn log_json_roundtrip(n in 0..20_usize) {
        let log = SideEffectLog::new();
        for i in 0..n {
            log.record(SideEffectEntry {
                index: 0,
                timestamp_ms: i as u64 * 100,
                effect_type: EffectType::SendKeys,
                pane_id: Some(i as u64),
                payload_summary: format!("payload_{i}"),
                caller_hint: "test".to_string(),
                action_kind: ActionKind::SendText,
                metadata: HashMap::new(),
            });
        }
        let json = log.to_json();
        let restored = SideEffectLog::from_json(&json).unwrap();
        prop_assert_eq!(restored.len(), n);
        let orig = log.entries();
        let rest = restored.entries();
        for i in 0..n {
            prop_assert_eq!(orig[i].timestamp_ms, rest[i].timestamp_ms);
            prop_assert_eq!(orig[i].effect_type, rest[i].effect_type);
            prop_assert_eq!(orig[i].pane_id, rest[i].pane_id);
        }
    }

    // LOG-6: Clear resets to empty
    #[test]
    fn log_clear_resets(n in 1..20_usize) {
        let log = SideEffectLog::new();
        for i in 0..n {
            log.record(SideEffectEntry {
                index: 0,
                timestamp_ms: i as u64,
                effect_type: EffectType::SendKeys,
                pane_id: Some(1),
                payload_summary: "x".to_string(),
                caller_hint: "t".to_string(),
                action_kind: ActionKind::SendText,
                metadata: HashMap::new(),
            });
        }
        log.clear();
        prop_assert!(log.is_empty());
        prop_assert_eq!(log.len(), 0);
    }
}

// ── ReplayBarrier Properties ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // RB-1: ReplayBarrier never executes any effect
    #[test]
    fn replay_never_executes(request in arb_request()) {
        let barrier = ReplayBarrier::new();
        let outcome = barrier.process(&request);
        prop_assert!(!outcome.executed, "ReplayBarrier must never execute");
        prop_assert!(!outcome.overridden);
    }

    // RB-2: ReplayBarrier captures every effect in log
    #[test]
    fn replay_captures_all(requests in prop::collection::vec(arb_request(), 1..30)) {
        let barrier = ReplayBarrier::new();
        for req in &requests {
            barrier.process(req);
        }
        let log = barrier.log().unwrap();
        prop_assert_eq!(log.len(), requests.len());
    }

    // RB-3: ReplayBarrier preserves effect type
    #[test]
    fn replay_preserves_type(request in arb_request()) {
        let barrier = ReplayBarrier::new();
        barrier.process(&request);
        let entry = &barrier.log().unwrap().entries()[0];
        prop_assert_eq!(entry.effect_type, request.effect_type);
    }

    // RB-4: ReplayBarrier preserves pane_id
    #[test]
    fn replay_preserves_pane(request in arb_request()) {
        let barrier = ReplayBarrier::new();
        barrier.process(&request);
        let entry = &barrier.log().unwrap().entries()[0];
        prop_assert_eq!(entry.pane_id, request.pane_id);
    }
}

// ── LiveBarrier Properties ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // LB-1: LiveBarrier always marks executed
    #[test]
    fn live_always_executes(request in arb_request()) {
        let barrier = LiveBarrier::new();
        let outcome = barrier.process(&request);
        prop_assert!(outcome.executed, "LiveBarrier must always execute");
        prop_assert!(!outcome.overridden);
    }

    // LB-2: LiveBarrier records in log
    #[test]
    fn live_records_all(requests in prop::collection::vec(arb_request(), 1..30)) {
        let barrier = LiveBarrier::new();
        for req in &requests {
            barrier.process(req);
        }
        let log = barrier.log().unwrap();
        prop_assert_eq!(log.len(), requests.len());
    }
}

// ── CounterfactualBarrier Properties ───────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // CF-1: CounterfactualBarrier never executes
    #[test]
    fn counterfactual_never_executes(
        request in arb_request(),
        overrides in prop::collection::vec(arb_override_rule(), 0..5),
    ) {
        let barrier = CounterfactualBarrier::new(overrides);
        let outcome = barrier.process(&request);
        prop_assert!(!outcome.executed, "CounterfactualBarrier must never execute");
    }

    // CF-2: Override count <= total processed
    #[test]
    fn counterfactual_override_count_bounded(
        requests in prop::collection::vec(arb_request(), 1..20),
        overrides in prop::collection::vec(arb_override_rule(), 0..5),
    ) {
        let barrier = CounterfactualBarrier::new(overrides);
        for req in &requests {
            barrier.process(req);
        }
        prop_assert!(
            barrier.overrides_applied() <= requests.len(),
            "Overrides applied {} > requests {}",
            barrier.overrides_applied(), requests.len()
        );
    }

    // CF-3: All effects recorded in log
    #[test]
    fn counterfactual_log_complete(
        requests in prop::collection::vec(arb_request(), 1..20),
        overrides in prop::collection::vec(arb_override_rule(), 0..5),
    ) {
        let barrier = CounterfactualBarrier::new(overrides);
        for req in &requests {
            barrier.process(req);
        }
        let log = barrier.log().unwrap();
        prop_assert_eq!(log.len(), requests.len());
    }

    // CF-4: Override provenance in metadata when overridden
    #[test]
    fn counterfactual_provenance_present(
        pane_id in 1..10_u64,
        payload in "[a-z]{5,20}",
    ) {
        let rule = OverrideRule {
            effect_type: EffectType::SendKeys,
            pane_id: None,
            payload_contains: None,
            replacement_payload: "replaced".to_string(),
            description: "test".to_string(),
        };
        let barrier = CounterfactualBarrier::new(vec![rule]);
        let req = EffectRequest {
            timestamp_ms: 1000,
            effect_type: EffectType::SendKeys,
            pane_id: Some(pane_id),
            payload,
            caller: "test".to_string(),
            action_kind: ActionKind::SendText,
            metadata: HashMap::new(),
        };
        let outcome = barrier.process(&req);
        prop_assert!(outcome.overridden);
        let entry = &barrier.log().unwrap().entries()[0];
        prop_assert!(entry.metadata.contains_key("override_applied"));
        prop_assert!(entry.metadata.contains_key("original_payload"));
        prop_assert!(entry.metadata.contains_key("replacement_payload"));
    }
}

// ── EffectType Properties ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ET-1: ActionKind → EffectType mapping is total (no panics)
    #[test]
    fn action_kind_mapping_total(kind in arb_action_kind()) {
        let _ = EffectType::from_action_kind(kind);
    }

    // ET-2: EffectType serde roundtrip
    #[test]
    fn effect_type_serde(et in arb_effect_type()) {
        let json = serde_json::to_string(&et).unwrap();
        let back: EffectType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(et, back);
    }
}

// ── OverrideRule Properties ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // OR-1: OverrideRule serde roundtrip
    #[test]
    fn override_rule_serde(rule in arb_override_rule()) {
        let json = serde_json::to_string(&rule).unwrap();
        let back: OverrideRule = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rule.effect_type, back.effect_type);
        prop_assert_eq!(rule.pane_id, back.pane_id);
        prop_assert_eq!(rule.payload_contains, back.payload_contains);
        prop_assert_eq!(rule.replacement_payload, back.replacement_payload);
    }

    // OR-2: Mismatched effect type never matches
    #[test]
    fn override_wrong_type_never_matches(
        rule in arb_override_rule(),
        request in arb_request(),
    ) {
        if rule.effect_type != request.effect_type {
            let barrier = CounterfactualBarrier::new(vec![rule]);
            let outcome = barrier.process(&request);
            prop_assert!(!outcome.overridden, "Mismatched effect type should not override");
        }
    }
}

// ── Side-Effect Isolation Completeness (P-09) ──────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // P09: No real effects escape ReplayBarrier under any input sequence
    #[test]
    fn no_effects_escape_replay(
        requests in prop::collection::vec(arb_request(), 1..50),
    ) {
        let barrier = ReplayBarrier::new();
        for req in &requests {
            let outcome = barrier.process(req);
            prop_assert!(!outcome.executed, "Effect escaped ReplayBarrier: {:?}", req.effect_type);
        }
    }
}
