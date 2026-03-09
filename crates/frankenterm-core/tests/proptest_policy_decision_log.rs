//! Property-based tests for the policy_decision_log module.
//!
//! Tests serde roundtrips for all public types and behavioral invariants
//! of the append-only decision log, including bounded retention, filtering,
//! counter consistency, and export.

use frankenterm_core::policy::{ActionKind, ActorKind, PolicySurface};
use frankenterm_core::policy_decision_log::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::SendCtrlC),
        Just(ActionKind::Spawn),
        Just(ActionKind::Split),
        Just(ActionKind::Close),
        Just(ActionKind::ReadOutput),
        Just(ActionKind::WriteFile),
        Just(ActionKind::DeleteFile),
        Just(ActionKind::ExecCommand),
    ]
}

fn arb_actor_kind() -> impl Strategy<Value = ActorKind> {
    prop_oneof![
        Just(ActorKind::Human),
        Just(ActorKind::Robot),
        Just(ActorKind::Mcp),
        Just(ActorKind::Workflow),
    ]
}

fn arb_policy_surface() -> impl Strategy<Value = PolicySurface> {
    prop_oneof![
        Just(PolicySurface::Unknown),
        Just(PolicySurface::Mux),
        Just(PolicySurface::Swarm),
        Just(PolicySurface::Robot),
        Just(PolicySurface::Connector),
        Just(PolicySurface::Workflow),
        Just(PolicySurface::Mcp),
        Just(PolicySurface::Ipc),
    ]
}

fn arb_decision_outcome() -> impl Strategy<Value = DecisionOutcome> {
    prop_oneof![
        Just(DecisionOutcome::Allow),
        Just(DecisionOutcome::Deny),
        Just(DecisionOutcome::RequireApproval),
    ]
}

fn arb_decision_entry() -> impl Strategy<Value = PolicyDecisionEntry> {
    (
        any::<u64>(),
        any::<u64>(),
        arb_action_kind(),
        arb_actor_kind(),
        arb_policy_surface(),
        prop::option::of(1..1000u64),
        arb_decision_outcome(),
        prop::option::of("[a-z-]{1,20}"),
        prop::option::of("[a-z ]{1,30}"),
        0..100u32,
    )
        .prop_map(
            |(seq, ts, action, actor, surface, pane_id, decision, rule_id, reason, rules)| {
                PolicyDecisionEntry {
                    seq,
                    timestamp_ms: ts,
                    action,
                    actor,
                    surface,
                    pane_id,
                    decision,
                    rule_id,
                    reason,
                    rules_evaluated: rules,
                }
            },
        )
}

fn arb_decision_log_config() -> impl Strategy<Value = DecisionLogConfig> {
    (1..10_000usize, any::<bool>()).prop_map(|(max_entries, record_allows)| DecisionLogConfig {
        max_entries,
        record_allows,
    })
}

fn arb_decision_log_snapshot() -> impl Strategy<Value = DecisionLogSnapshot> {
    (
        any::<usize>(),
        any::<usize>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<bool>(),
    )
        .prop_map(
            |(cur, max, recorded, evicted, next, deny, allow, req, rec_allows)| {
                DecisionLogSnapshot {
                    current_entries: cur,
                    max_entries: max,
                    total_recorded: recorded,
                    total_evicted: evicted,
                    next_seq: next,
                    deny_count: deny,
                    allow_count: allow,
                    require_approval_count: req,
                    record_allows: rec_allows,
                }
            },
        )
}

/// Parameters for a record call.
#[allow(clippy::type_complexity)]
fn arb_record_params() -> impl Strategy<Value = (u64, ActionKind, ActorKind, PolicySurface, Option<u64>, DecisionOutcome, Option<String>, Option<String>, u32)> {
    (
        any::<u64>(),
        arb_action_kind(),
        arb_actor_kind(),
        arb_policy_surface(),
        prop::option::of(1..1000u64),
        arb_decision_outcome(),
        prop::option::of("[a-z-]{1,15}"),
        prop::option::of("[a-z ]{1,20}"),
        0..50u32,
    )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn decision_entry_serde_roundtrip(entry in arb_decision_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let back: PolicyDecisionEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(entry, back);
    }

    #[test]
    fn decision_outcome_serde_roundtrip(outcome in arb_decision_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: DecisionOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, back);
    }

    #[test]
    fn decision_log_config_serde_roundtrip(config in arb_decision_log_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: DecisionLogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    #[test]
    fn decision_log_snapshot_serde_roundtrip(snap in arb_decision_log_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: DecisionLogSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    // =========================================================================
    // Behavioral property tests
    // =========================================================================

    // ---- Record returns monotonic sequence numbers ----

    #[test]
    fn seq_numbers_monotonic(
        params in prop::collection::vec(arb_record_params(), 2..=10)
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        let mut seqs = Vec::new();
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            if let Some(seq) = log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) {
                seqs.push(seq);
            }
        }
        for w in seqs.windows(2) {
            prop_assert!(w[0] < w[1], "seq {} should be < {}", w[0], w[1]);
        }
    }

    // ---- Log never exceeds max_entries ----

    #[test]
    fn log_never_exceeds_max(
        max_entries in 1..50usize,
        count in 1..100usize,
    ) {
        let config = DecisionLogConfig {
            max_entries,
            record_allows: true,
        };
        let mut log = PolicyDecisionLog::new(config);
        for i in 0..count {
            log.record(
                i as u64 * 100,
                ActionKind::Spawn,
                ActorKind::Robot,
                PolicySurface::Mux,
                None,
                DecisionOutcome::Allow,
                None,
                None,
                1,
            );
        }
        prop_assert!(log.len() <= max_entries);
    }

    // ---- total_recorded = allow_count + deny_count + require_approval_count ----

    #[test]
    fn counter_consistency(
        params in prop::collection::vec(arb_record_params(), 0..=20)
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let snap = log.snapshot();
        prop_assert_eq!(
            snap.total_recorded,
            snap.deny_count + snap.allow_count + snap.require_approval_count
        );
    }

    // ---- Eviction count + current entries = total_recorded ----

    #[test]
    fn eviction_plus_current_equals_total(
        max_entries in 1..20usize,
        params in prop::collection::vec(arb_record_params(), 0..=30)
    ) {
        let config = DecisionLogConfig {
            max_entries,
            record_allows: true,
        };
        let mut log = PolicyDecisionLog::new(config);
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let snap = log.snapshot();
        prop_assert_eq!(
            snap.total_recorded,
            snap.current_entries as u64 + snap.total_evicted
        );
    }

    // ---- Record allows filtering ----

    #[test]
    fn record_allows_false_skips_allows(
        params in prop::collection::vec(arb_record_params(), 0..=15)
    ) {
        let config = DecisionLogConfig {
            max_entries: 1000,
            record_allows: false,
        };
        let mut log = PolicyDecisionLog::new(config);
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let allows = log.by_decision(DecisionOutcome::Allow);
        prop_assert!(allows.is_empty(), "No allows should be recorded when record_allows=false");
    }

    // ---- by_decision filter completeness ----

    #[test]
    fn by_decision_partitions_all_entries(
        params in prop::collection::vec(arb_record_params(), 0..=15)
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let allows = log.by_decision(DecisionOutcome::Allow).len();
        let denies = log.by_decision(DecisionOutcome::Deny).len();
        let approvals = log.by_decision(DecisionOutcome::RequireApproval).len();
        prop_assert_eq!(allows + denies + approvals, log.len());
    }

    // ---- by_actor filter completeness ----

    #[test]
    fn by_actor_covers_all(
        params in prop::collection::vec(arb_record_params(), 0..=15)
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let total: usize = [ActorKind::Human, ActorKind::Robot, ActorKind::Mcp, ActorKind::Workflow]
            .iter()
            .map(|a| log.by_actor(*a).len())
            .sum();
        prop_assert_eq!(total, log.len());
    }

    // ---- Clear preserves counters ----

    #[test]
    fn clear_resets_entries_preserves_counters(
        params in prop::collection::vec(arb_record_params(), 1..=10)
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let snap_before = log.snapshot();
        let entries_before = snap_before.current_entries;
        let total_before = snap_before.total_recorded;
        log.clear();
        let snap_after = log.snapshot();
        prop_assert!(log.is_empty());
        prop_assert_eq!(snap_after.total_recorded, total_before);
        prop_assert_eq!(
            snap_after.total_evicted,
            snap_before.total_evicted + entries_before as u64
        );
    }

    // ---- get retrieves the correct entry ----

    #[test]
    fn get_returns_correct_entry(
        ts in any::<u64>(),
        action in arb_action_kind(),
        actor in arb_actor_kind(),
        decision in arb_decision_outcome()
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        let seq = log.record(
            ts, action, actor, PolicySurface::Mux, None, decision, None, None, 1
        ).unwrap();
        let entry = log.get(seq).unwrap();
        prop_assert_eq!(entry.seq, seq);
        prop_assert_eq!(entry.action, action);
        prop_assert_eq!(entry.actor, actor);
        prop_assert_eq!(entry.decision, decision);
    }

    // ---- export_json produces valid JSON ----

    #[test]
    fn export_json_is_valid(
        params in prop::collection::vec(arb_record_params(), 0..=5)
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let json = log.export_json().unwrap();
        let parsed: Vec<PolicyDecisionEntry> = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.len(), log.len());
    }

    // ---- export_jsonl line count matches filter ----

    #[test]
    fn export_jsonl_filter_correctness(
        params in prop::collection::vec(arb_record_params(), 0..=10)
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let deny_count = log.by_decision(DecisionOutcome::Deny).len();
        let jsonl = log.export_jsonl(|e| e.decision == DecisionOutcome::Deny).unwrap();
        let line_count = if jsonl.is_empty() { 0 } else { jsonl.lines().count() };
        prop_assert_eq!(line_count, deny_count);
    }

    // ---- next_seq equals total recorded (with allows enabled) ----

    #[test]
    fn next_seq_matches_total_with_allows(
        params in prop::collection::vec(arb_record_params(), 0..=15)
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let snap = log.snapshot();
        prop_assert_eq!(snap.next_seq, snap.total_recorded);
    }

    // ---- Snapshot is consistent with log state ----

    #[test]
    fn snapshot_reflects_current_state(
        max_entries in 1..30usize,
        params in prop::collection::vec(arb_record_params(), 0..=20)
    ) {
        let config = DecisionLogConfig {
            max_entries,
            record_allows: true,
        };
        let mut log = PolicyDecisionLog::new(config);
        for (ts, action, actor, surface, pane_id, decision, rule_id, reason, rules) in params {
            log.record(ts, action, actor, surface, pane_id, decision, rule_id, reason, rules);
        }
        let snap = log.snapshot();
        prop_assert_eq!(snap.current_entries, log.len());
        prop_assert_eq!(snap.max_entries, max_entries);
        prop_assert!(snap.current_entries <= snap.max_entries);
    }

    // ---- by_time_range is monotone ----

    #[test]
    fn time_range_subset(
        start in 0..500u64,
        end in 500..1000u64,
    ) {
        let mut log = PolicyDecisionLog::with_defaults();
        for ts in (0..1500).step_by(100) {
            log.record(
                ts, ActionKind::Spawn, ActorKind::Robot, PolicySurface::Mux,
                None, DecisionOutcome::Allow, None, None, 1,
            );
        }
        let range = log.by_time_range(start, end);
        for entry in &range {
            prop_assert!(entry.timestamp_ms >= start);
            prop_assert!(entry.timestamp_ms <= end);
        }
    }

    // ---- DecisionOutcome from DslDecision roundtrip ----

    #[test]
    fn decision_outcome_from_dsl_preserves_semantics(decision in arb_decision_outcome()) {
        use frankenterm_core::policy_dsl::DslDecision;
        let dsl = match decision {
            DecisionOutcome::Allow => DslDecision::Allow,
            DecisionOutcome::Deny => DslDecision::Deny,
            DecisionOutcome::RequireApproval => DslDecision::RequireApproval,
        };
        let roundtripped = DecisionOutcome::from(dsl);
        prop_assert_eq!(decision, roundtripped);
    }
}
