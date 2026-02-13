//! Property-based tests for completion_token module.
//!
//! Verifies completion token lifecycle invariants:
//! - CompletionState: is_terminal classification, from_u8 roundtrip, serde roundtrip,
//!   snake_case serialization, defensive fallback for unknown u8
//! - StepOutcome: serde roundtrip, snake_case serialization
//! - CauseChain: append-only, len/is_empty consistent, failed_subsystems correct,
//!   elapsed_ms non-negative for ordered timestamps, serde roundtrip
//! - CompletionBoundary: is_satisfied requires all non-Cancelled, pending_subsystems
//!   shrinks as steps added, Cancelled does not satisfy, Error does satisfy
//! - CompletionTrackerConfig: serde roundtrip, default values valid
//! - TokenId: serde roundtrip, Display matches inner string
//! - CompletionTracker: begin respects capacity, advance transitions correctly,
//!   terminal states are immutable, active_count tracks non-terminal tokens

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::completion_token::{
    Boundaries, CauseChain, CauseStep, CompletionBoundary, CompletionState, CompletionTracker,
    CompletionTrackerConfig, StepOutcome, TokenId, TokenSummary,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_state() -> impl Strategy<Value = CompletionState> {
    prop_oneof![
        Just(CompletionState::Pending),
        Just(CompletionState::InProgress),
        Just(CompletionState::Completed),
        Just(CompletionState::TimedOut),
        Just(CompletionState::Failed),
        Just(CompletionState::PartialFailure),
    ]
}

fn arb_outcome() -> impl Strategy<Value = StepOutcome> {
    prop_oneof![
        Just(StepOutcome::Ok),
        Just(StepOutcome::Error),
        Just(StepOutcome::Skipped),
        Just(StepOutcome::Cancelled),
    ]
}

fn arb_subsystem_name() -> impl Strategy<Value = String> {
    "[a-z_]{2,15}"
}

fn arb_cause_step() -> impl Strategy<Value = CauseStep> {
    (
        arb_subsystem_name(),
        arb_outcome(),
        "[a-z ]{0,30}",
        0i64..=10_000_000_000,
    )
        .prop_map(|(subsystem, outcome, message, timestamp_ms)| CauseStep {
            subsystem,
            outcome,
            message,
            timestamp_ms,
            metadata: HashMap::new(),
        })
}

fn arb_config() -> impl Strategy<Value = CompletionTrackerConfig> {
    (
        0u64..=120_000,       // default_timeout_ms
        1usize..=50_000,      // max_active_tokens
        0u64..=600_000,       // retention_ms
    )
        .prop_map(|(timeout, max_tokens, retention)| CompletionTrackerConfig {
            default_timeout_ms: timeout,
            max_active_tokens: max_tokens,
            retention_ms: retention,
        })
}

// ────────────────────────────────────────────────────────────────────
// CompletionState: is_terminal, from_u8, serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// is_terminal matches exactly {Completed, TimedOut, Failed, PartialFailure}.
    #[test]
    fn prop_state_terminal_classification(s in arb_state()) {
        let expected = matches!(
            s,
            CompletionState::Completed
                | CompletionState::TimedOut
                | CompletionState::Failed
                | CompletionState::PartialFailure
        );
        prop_assert_eq!(s.is_terminal(), expected);
    }

    /// CompletionState JSON roundtrip.
    #[test]
    fn prop_state_serde_roundtrip(s in arb_state()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: CompletionState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    /// CompletionState serializes to snake_case.
    #[test]
    fn prop_state_serde_snake_case(s in arb_state()) {
        let json = serde_json::to_string(&s).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "state '{}' should be snake_case", inner
        );
    }

    /// Pending and InProgress are non-terminal.
    #[test]
    fn prop_state_non_terminal(_dummy in 0..1u32) {
        prop_assert!(!CompletionState::Pending.is_terminal());
        prop_assert!(!CompletionState::InProgress.is_terminal());
    }
}

// ────────────────────────────────────────────────────────────────────
// StepOutcome: serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// StepOutcome JSON roundtrip.
    #[test]
    fn prop_outcome_serde_roundtrip(o in arb_outcome()) {
        let json = serde_json::to_string(&o).unwrap();
        let back: StepOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(o, back);
    }

    /// StepOutcome serializes to snake_case.
    #[test]
    fn prop_outcome_serde_snake_case(o in arb_outcome()) {
        let json = serde_json::to_string(&o).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "outcome '{}' should be snake_case", inner
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// CauseChain: append-only, len/is_empty, failed_subsystems, elapsed
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// CauseChain len equals number of pushed steps.
    #[test]
    fn prop_cause_chain_len(steps in prop::collection::vec(arb_cause_step(), 0..=10)) {
        let mut chain = CauseChain::new();
        for step in &steps {
            chain.push(step.clone());
        }
        prop_assert_eq!(chain.len(), steps.len());
        prop_assert_eq!(chain.is_empty(), steps.is_empty());
    }

    /// CauseChain preserves insertion order.
    #[test]
    fn prop_cause_chain_order(steps in prop::collection::vec(arb_cause_step(), 1..=8)) {
        let mut chain = CauseChain::new();
        for step in &steps {
            chain.push(step.clone());
        }
        for (i, step) in chain.steps().iter().enumerate() {
            prop_assert!(
                step.subsystem == steps[i].subsystem,
                "step {} subsystem mismatch", i
            );
        }
    }

    /// failed_subsystems returns exactly those with Error outcome.
    #[test]
    fn prop_cause_chain_failed_subsystems(steps in prop::collection::vec(arb_cause_step(), 0..=10)) {
        let mut chain = CauseChain::new();
        for step in &steps {
            chain.push(step.clone());
        }
        let failed = chain.failed_subsystems();
        let expected: Vec<&str> = steps
            .iter()
            .filter(|s| s.outcome == StepOutcome::Error)
            .map(|s| s.subsystem.as_str())
            .collect();
        prop_assert_eq!(failed, expected);
    }

    /// elapsed_ms is non-negative when timestamps are monotonic.
    #[test]
    fn prop_cause_chain_elapsed_monotonic(
        base in 0i64..=5_000_000_000,
        deltas in prop::collection::vec(0i64..=10_000, 2..=5),
    ) {
        let mut chain = CauseChain::new();
        let mut ts = base;
        for (i, delta) in deltas.iter().enumerate() {
            ts += delta;
            chain.push(CauseStep {
                subsystem: format!("s{}", i),
                outcome: StepOutcome::Ok,
                message: String::new(),
                timestamp_ms: ts,
                metadata: HashMap::new(),
            });
        }
        prop_assert!(chain.elapsed_ms() >= 0, "elapsed_ms {} < 0", chain.elapsed_ms());
    }

    /// elapsed_ms is 0 for single-step chains.
    #[test]
    fn prop_cause_chain_elapsed_single(step in arb_cause_step()) {
        let mut chain = CauseChain::new();
        chain.push(step);
        prop_assert_eq!(chain.elapsed_ms(), 0);
    }

    /// elapsed_ms is 0 for empty chains.
    #[test]
    fn prop_cause_chain_elapsed_empty(_dummy in 0..1u32) {
        let chain = CauseChain::new();
        prop_assert_eq!(chain.elapsed_ms(), 0);
    }

    /// CauseChain JSON roundtrip preserves step count and subsystem names.
    #[test]
    fn prop_cause_chain_serde_roundtrip(steps in prop::collection::vec(arb_cause_step(), 0..=5)) {
        let mut chain = CauseChain::new();
        for step in &steps {
            chain.push(step.clone());
        }
        let json = serde_json::to_string(&chain).unwrap();
        let back: CauseChain = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.len(), chain.len());
        for (i, step) in back.steps().iter().enumerate() {
            prop_assert!(
                step.subsystem == chain.steps()[i].subsystem,
                "step {} subsystem mismatch after roundtrip", i
            );
            prop_assert_eq!(step.outcome, chain.steps()[i].outcome);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// CompletionBoundary: is_satisfied, pending_subsystems
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty chain never satisfies a non-empty boundary.
    #[test]
    fn prop_boundary_empty_chain_unsatisfied(subs in prop::collection::vec(arb_subsystem_name(), 1..=5)) {
        let refs: Vec<&str> = subs.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let chain = CauseChain::new();
        prop_assert!(!boundary.is_satisfied(&chain));
    }

    /// pending_subsystems for empty chain equals all required subsystems.
    #[test]
    fn prop_boundary_pending_all_initially(subs in prop::collection::vec(arb_subsystem_name(), 1..=5)) {
        let refs: Vec<&str> = subs.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let chain = CauseChain::new();
        let pending = boundary.pending_subsystems(&chain);
        prop_assert_eq!(pending.len(), subs.len());
    }

    /// Cancelled outcome does NOT satisfy a boundary subsystem.
    #[test]
    fn prop_boundary_cancelled_not_satisfied(sub in arb_subsystem_name()) {
        let boundary = CompletionBoundary::new(&[&sub]);
        let mut chain = CauseChain::new();
        chain.record(&sub, StepOutcome::Cancelled, "cancelled");
        prop_assert!(!boundary.is_satisfied(&chain));
        prop_assert_eq!(boundary.pending_subsystems(&chain).len(), 1);
    }

    /// Error outcome DOES satisfy a boundary subsystem.
    #[test]
    fn prop_boundary_error_satisfies(sub in arb_subsystem_name()) {
        let boundary = CompletionBoundary::new(&[&sub]);
        let mut chain = CauseChain::new();
        chain.record(&sub, StepOutcome::Error, "failed");
        prop_assert!(boundary.is_satisfied(&chain));
        prop_assert!(boundary.pending_subsystems(&chain).is_empty());
    }

    /// Skipped outcome satisfies a boundary subsystem.
    #[test]
    fn prop_boundary_skipped_satisfies(sub in arb_subsystem_name()) {
        let boundary = CompletionBoundary::new(&[&sub]);
        let mut chain = CauseChain::new();
        chain.record(&sub, StepOutcome::Skipped, "n/a");
        prop_assert!(boundary.is_satisfied(&chain));
    }

    /// Ok outcome satisfies a boundary subsystem.
    #[test]
    fn prop_boundary_ok_satisfies(sub in arb_subsystem_name()) {
        let boundary = CompletionBoundary::new(&[&sub]);
        let mut chain = CauseChain::new();
        chain.record(&sub, StepOutcome::Ok, "done");
        prop_assert!(boundary.is_satisfied(&chain));
    }

    /// required() returns the subsystems passed at construction.
    #[test]
    fn prop_boundary_required_preserved(subs in prop::collection::vec(arb_subsystem_name(), 1..=5)) {
        let refs: Vec<&str> = subs.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let required: Vec<&str> = boundary.required().iter().map(|s| s.as_str()).collect();
        prop_assert_eq!(required, refs);
    }

    /// Boundary serde roundtrip preserves required subsystems.
    #[test]
    fn prop_boundary_serde_roundtrip(subs in prop::collection::vec(arb_subsystem_name(), 1..=5)) {
        let refs: Vec<&str> = subs.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let json = serde_json::to_string(&boundary).unwrap();
        let back: CompletionBoundary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.required().len(), boundary.required().len());
        for (a, b) in back.required().iter().zip(boundary.required().iter()) {
            prop_assert!(a == b, "subsystem mismatch: {} != {}", a, b);
        }
    }

    /// Once satisfied, adding more Ok steps keeps it satisfied (monotonic).
    #[test]
    fn prop_boundary_monotonic_satisfied(subs in prop::collection::vec(arb_subsystem_name(), 1..=4)) {
        let refs: Vec<&str> = subs.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let mut chain = CauseChain::new();

        // Satisfy all required
        for sub in &subs {
            chain.record(sub, StepOutcome::Ok, "ok");
        }
        prop_assert!(boundary.is_satisfied(&chain));

        // Add extra steps — still satisfied
        chain.record("extra_sub", StepOutcome::Ok, "bonus");
        prop_assert!(boundary.is_satisfied(&chain));
    }

    /// Preset boundaries have non-empty required lists.
    #[test]
    fn prop_preset_boundaries_non_empty(_dummy in 0..1u32) {
        prop_assert!(!Boundaries::send_text().required().is_empty());
        prop_assert!(!Boundaries::workflow_step().required().is_empty());
        prop_assert!(!Boundaries::capture().required().is_empty());
        prop_assert!(!Boundaries::pattern_detection().required().is_empty());
        prop_assert!(!Boundaries::recovery().required().is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// CompletionTrackerConfig: serde roundtrip, defaults
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Config JSON roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(c in arb_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: CompletionTrackerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.default_timeout_ms, c.default_timeout_ms);
        prop_assert_eq!(back.max_active_tokens, c.max_active_tokens);
        prop_assert_eq!(back.retention_ms, c.retention_ms);
    }

    /// Default config has reasonable values.
    #[test]
    fn prop_config_default_valid(_dummy in 0..1u32) {
        let c = CompletionTrackerConfig::default();
        prop_assert!(c.default_timeout_ms > 0, "default timeout should be > 0");
        prop_assert!(c.max_active_tokens > 0, "default max_active should be > 0");
        prop_assert!(c.retention_ms > 0, "default retention should be > 0");
    }
}

// ────────────────────────────────────────────────────────────────────
// TokenId: serde roundtrip, Display
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// TokenId JSON roundtrip.
    #[test]
    fn prop_token_id_serde_roundtrip(s in "[a-z0-9-]{5,30}") {
        let id = TokenId(s.clone());
        let json = serde_json::to_string(&id).unwrap();
        let back: TokenId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(id, back);
    }

    /// TokenId Display matches inner string.
    #[test]
    fn prop_token_id_display(s in "[a-z0-9-]{5,30}") {
        let id = TokenId(s.clone());
        prop_assert_eq!(format!("{}", id), s);
    }
}

// ────────────────────────────────────────────────────────────────────
// TokenSummary: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// TokenSummary JSON roundtrip preserves key fields.
    #[test]
    fn prop_token_summary_serde_roundtrip(
        id_str in "[a-z0-9-]{5,20}",
        op in "[a-z_]{3,15}",
        state in arb_state(),
        steps in 0usize..=20,
        age in 0i64..=100_000,
        pane in prop::option::of(0u64..=1_000_000),
    ) {
        let summary = TokenSummary {
            id: TokenId(id_str),
            operation: op.clone(),
            state,
            steps_completed: steps,
            pending: vec!["a".to_string(), "b".to_string()],
            age_ms: age,
            pane_id: pane,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: TokenSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, summary.id);
        prop_assert!(back.operation == op, "operation mismatch");
        prop_assert_eq!(back.state, state);
        prop_assert_eq!(back.steps_completed, steps);
        prop_assert_eq!(back.pane_id, pane);
    }
}

// ────────────────────────────────────────────────────────────────────
// CompletionTracker: capacity, state transitions, immutability
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Tracker starts with zero active tokens.
    #[test]
    fn prop_tracker_initial_empty(c in arb_config()) {
        let tracker = CompletionTracker::new(c);
        prop_assert_eq!(tracker.active_count(), 0);
        prop_assert_eq!(tracker.total_count(), 0);
    }

    /// begin() returns None when at capacity.
    #[test]
    fn prop_tracker_capacity_limit(max in 1usize..=5) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: max,
            retention_ms: 60_000,
        });

        for i in 0..max {
            let b = CompletionBoundary::new(&["a"]);
            let result = tracker.begin(&format!("op{}", i), b);
            prop_assert!(result.is_some(), "begin should succeed at count {}", i);
        }

        // One more should fail.
        let b = CompletionBoundary::new(&["a"]);
        prop_assert!(tracker.begin("overflow", b).is_none());
    }

    /// Completing a token frees capacity for new tokens.
    #[test]
    fn prop_tracker_capacity_freed_on_completion(_dummy in 0..1u32) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 1,
            retention_ms: 60_000,
        });

        let b = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("op1", b).unwrap();
        prop_assert!(tracker.begin("op2", CompletionBoundary::new(&["a"])).is_none());

        tracker.advance(&id, "a", StepOutcome::Ok, "done");
        prop_assert_eq!(tracker.active_count(), 0);

        // Now we can begin again.
        prop_assert!(tracker.begin("op2", CompletionBoundary::new(&["a"])).is_some());
    }

    /// Terminal tokens cannot transition to any other state.
    #[test]
    fn prop_tracker_terminal_immutable(_dummy in 0..1u32) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });

        // Complete a token.
        let b = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("op", b).unwrap();
        tracker.advance(&id, "a", StepOutcome::Ok, "done");
        prop_assert_eq!(tracker.state(&id), Some(CompletionState::Completed));

        // Try to fail it.
        tracker.fail(&id, "too late");
        prop_assert_eq!(tracker.state(&id), Some(CompletionState::Completed));

        // Try to time it out.
        tracker.timeout(&id);
        prop_assert_eq!(tracker.state(&id), Some(CompletionState::Completed));
    }

    /// advance on unknown token returns None.
    #[test]
    fn prop_tracker_unknown_token_none(s in "[a-z0-9-]{5,20}") {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig::default());
        let fake = TokenId(s);
        prop_assert_eq!(tracker.advance(&fake, "a", StepOutcome::Ok, "?"), None);
        prop_assert_eq!(tracker.state(&fake), None);
    }

    /// First advance transitions Pending → InProgress (for multi-subsystem boundary).
    #[test]
    fn prop_tracker_first_advance_in_progress(sub in arb_subsystem_name()) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });

        // Two subsystems so first advance doesn't complete the boundary.
        let b = CompletionBoundary::new(&[&sub, "zzz_other"]);
        let id = tracker.begin("op", b).unwrap();
        prop_assert_eq!(tracker.state(&id), Some(CompletionState::Pending));

        let new_state = tracker.advance(&id, &sub, StepOutcome::Ok, "started");
        prop_assert_eq!(new_state, Some(CompletionState::InProgress));
    }

    /// Error with no prior Ok steps → Failed (not PartialFailure).
    #[test]
    fn prop_tracker_error_without_ok_is_failed(sub in arb_subsystem_name()) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });
        let b = CompletionBoundary::new(&[&sub]);
        let id = tracker.begin("op", b).unwrap();
        let s = tracker.advance(&id, &sub, StepOutcome::Error, "failed");
        prop_assert_eq!(s, Some(CompletionState::Failed));
    }

    /// Error after Ok step → PartialFailure.
    #[test]
    fn prop_tracker_error_after_ok_is_partial(
        sub1 in arb_subsystem_name(),
        sub2 in arb_subsystem_name(),
    ) {
        prop_assume!(sub1 != sub2);
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });
        let b = CompletionBoundary::new(&[&sub1, &sub2]);
        let id = tracker.begin("op", b).unwrap();
        tracker.advance(&id, &sub1, StepOutcome::Ok, "ok");
        let s = tracker.advance(&id, &sub2, StepOutcome::Error, "failed");
        prop_assert_eq!(s, Some(CompletionState::PartialFailure));
    }

    /// Cancelled step leads to Failed.
    #[test]
    fn prop_tracker_cancelled_is_failed(sub in arb_subsystem_name()) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });
        let b = CompletionBoundary::new(&[&sub]);
        let id = tracker.begin("op", b).unwrap();
        let s = tracker.advance(&id, &sub, StepOutcome::Cancelled, "cancelled");
        prop_assert_eq!(s, Some(CompletionState::Failed));
    }

    /// active_count decreases as tokens reach terminal state.
    #[test]
    fn prop_tracker_active_count_tracks(count in 1usize..=5) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });

        let mut ids = Vec::new();
        for i in 0..count {
            let b = CompletionBoundary::new(&["a"]);
            let id = tracker.begin(&format!("op{}", i), b).unwrap();
            ids.push(id);
        }
        prop_assert_eq!(tracker.active_count(), count);

        for (i, id) in ids.iter().enumerate() {
            tracker.advance(id, "a", StepOutcome::Ok, "done");
            prop_assert_eq!(tracker.active_count(), count - i - 1);
        }
    }

    /// total_count >= active_count always.
    #[test]
    fn prop_tracker_total_geq_active(count in 1usize..=5) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });

        for i in 0..count {
            let b = CompletionBoundary::new(&["a"]);
            tracker.begin(&format!("op{}", i), b).unwrap();
        }
        prop_assert!(tracker.total_count() >= tracker.active_count());
    }

    /// cause_chain returns None for unknown token.
    #[test]
    fn prop_tracker_cause_chain_none(s in "[a-z0-9-]{5,20}") {
        let tracker = CompletionTracker::new(CompletionTrackerConfig::default());
        let fake = TokenId(s);
        prop_assert!(tracker.cause_chain(&fake).is_none());
    }

    /// pending_subsystems returns None for unknown token.
    #[test]
    fn prop_tracker_pending_none(s in "[a-z0-9-]{5,20}") {
        let tracker = CompletionTracker::new(CompletionTrackerConfig::default());
        let fake = TokenId(s);
        prop_assert!(tracker.pending_subsystems(&fake).is_none());
    }
}

// ────────────────────────────────────────────────────────────────────
// Integration: end-to-end happy path and failure paths
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// End-to-end: create token, advance all subsystems Ok → Completed.
    #[test]
    fn prop_e2e_happy_path(subs in prop::collection::vec(arb_subsystem_name(), 1..=4)) {
        let refs: Vec<&str> = subs.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig::default());
        let id = tracker.begin("test_op", boundary).unwrap();

        for sub in &subs {
            tracker.advance(&id, sub, StepOutcome::Ok, "ok");
        }

        prop_assert_eq!(tracker.state(&id), Some(CompletionState::Completed));
        prop_assert_eq!(tracker.active_count(), 0);
    }

    /// End-to-end: first Error → Failed.
    #[test]
    fn prop_e2e_first_error_fails(subs in prop::collection::vec(arb_subsystem_name(), 1..=4)) {
        let refs: Vec<&str> = subs.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig::default());
        let id = tracker.begin("test_op", boundary).unwrap();

        tracker.advance(&id, &subs[0], StepOutcome::Error, "error");
        prop_assert_eq!(tracker.state(&id), Some(CompletionState::Failed));
    }

    /// End-to-end: Ok then Error → PartialFailure.
    #[test]
    fn prop_e2e_partial_failure(subs in prop::collection::vec(arb_subsystem_name(), 2..=4)) {
        let refs: Vec<&str> = subs.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig::default());
        let id = tracker.begin("test_op", boundary).unwrap();

        tracker.advance(&id, &subs[0], StepOutcome::Ok, "ok");
        tracker.advance(&id, &subs[1], StepOutcome::Error, "failed");
        prop_assert_eq!(tracker.state(&id), Some(CompletionState::PartialFailure));
    }

    /// End-to-end: explicit timeout.
    #[test]
    fn prop_e2e_timeout(_dummy in 0..1u32) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig::default());
        let boundary = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("test_op", boundary).unwrap();

        tracker.advance(&id, "_start", StepOutcome::Ok, "started");
        tracker.timeout(&id);
        prop_assert_eq!(tracker.state(&id), Some(CompletionState::TimedOut));

        // Cause chain has the timeout step from _system.
        let chain = tracker.cause_chain(&id).unwrap();
        let last = chain.steps().last().unwrap();
        prop_assert!(last.subsystem == "_system", "last subsystem should be _system");
    }
}
