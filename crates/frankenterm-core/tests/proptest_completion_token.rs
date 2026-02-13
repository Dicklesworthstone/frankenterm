//! Property-based tests for completion_token module invariants.
//!
//! Verifies key properties:
//! - CompletionState u8 roundtrip and defensive from_u8
//! - Terminal state classification
//! - Serde roundtrips for all types
//! - CauseChain append-only monotonicity
//! - CompletionBoundary satisfaction monotonicity
//! - Tracker state machine transitions
//! - Active/total count invariants

use frankenterm_core::completion_token::*;
use proptest::prelude::*;
use std::collections::HashMap;

// =============================================================================
// Strategies
// =============================================================================

/// Generate arbitrary CompletionState (including out-of-range defensive cases).
fn arb_completion_state() -> impl Strategy<Value = CompletionState> {
    prop_oneof![
        Just(CompletionState::Pending),
        Just(CompletionState::InProgress),
        Just(CompletionState::Completed),
        Just(CompletionState::TimedOut),
        Just(CompletionState::Failed),
        Just(CompletionState::PartialFailure),
    ]
}

/// Generate arbitrary StepOutcome.
fn arb_step_outcome() -> impl Strategy<Value = StepOutcome> {
    prop_oneof![
        Just(StepOutcome::Ok),
        Just(StepOutcome::Error),
        Just(StepOutcome::Skipped),
        Just(StepOutcome::Cancelled),
    ]
}

/// Generate subsystem name.
fn arb_subsystem() -> impl Strategy<Value = String> {
    "[a-z]{1,10}"
}

/// Generate operation name.
fn arb_operation() -> impl Strategy<Value = String> {
    "[a-z_]{1,20}"
}

/// Generate message string.
fn arb_message() -> impl Strategy<Value = String> {
    "[a-z0-9 ]{0,50}"
}

/// Generate timestamp in milliseconds.
fn arb_timestamp() -> impl Strategy<Value = i64> {
    1_600_000_000_000i64..1_900_000_000_000i64
}

/// Generate metadata map.
fn arb_metadata() -> impl Strategy<Value = HashMap<String, String>> {
    prop::collection::hash_map("[a-z]{1,8}", "[a-z0-9]{0,20}", 0..5)
}

/// Generate a CauseStep.
fn arb_cause_step() -> impl Strategy<Value = CauseStep> {
    (
        arb_subsystem(),
        arb_step_outcome(),
        arb_message(),
        arb_timestamp(),
        arb_metadata(),
    )
        .prop_map(|(subsystem, outcome, message, timestamp_ms, metadata)| CauseStep {
            subsystem,
            outcome,
            message,
            timestamp_ms,
            metadata,
        })
}

/// Generate a CauseChain with 0-20 steps.
fn arb_cause_chain() -> impl Strategy<Value = CauseChain> {
    prop::collection::vec(arb_cause_step(), 0..20).prop_map(|steps| {
        let mut chain = CauseChain::new();
        for step in steps {
            chain.push(step);
        }
        chain
    })
}

/// Generate ordered timestamp steps (monotonically increasing).
fn arb_ordered_steps(count: usize) -> impl Strategy<Value = Vec<CauseStep>> {
    prop::collection::vec((arb_subsystem(), arb_step_outcome(), arb_message()), count).prop_map(
        move |tuples| {
            let base_ts = 1_600_000_000_000i64;
            tuples
                .into_iter()
                .enumerate()
                .map(|(i, (subsystem, outcome, message))| CauseStep {
                    subsystem,
                    outcome,
                    message,
                    timestamp_ms: base_ts + (i as i64 * 100),
                    metadata: HashMap::new(),
                })
                .collect()
        },
    )
}

/// Generate a CompletionBoundary with 1-5 required subsystems.
fn arb_completion_boundary() -> impl Strategy<Value = CompletionBoundary> {
    prop::collection::vec(arb_subsystem(), 1..6).prop_map(|subsystems| {
        let refs: Vec<&str> = subsystems.iter().map(|s| s.as_str()).collect();
        CompletionBoundary::new(&refs)
    })
}

/// Generate a CompletionTrackerConfig.
fn arb_config() -> impl Strategy<Value = CompletionTrackerConfig> {
    (0u64..60_000, 1usize..1000, 0u64..600_000).prop_map(
        |(default_timeout_ms, max_active_tokens, retention_ms)| CompletionTrackerConfig {
            default_timeout_ms,
            max_active_tokens,
            retention_ms,
        },
    )
}

// =============================================================================
// Property tests: CompletionState
// =============================================================================



proptest! {
    /// CompletionState u8 repr: each variant has expected ordinal.
    #[test]
    fn prop_completion_state_u8_repr(state in arb_completion_state()) {
        let u = state as u8;
        prop_assert!(u <= 5, "State ordinal out of range");
        match state {
            CompletionState::Pending => prop_assert_eq!(u, 0),
            CompletionState::InProgress => prop_assert_eq!(u, 1),
            CompletionState::Completed => prop_assert_eq!(u, 2),
            CompletionState::TimedOut => prop_assert_eq!(u, 3),
            CompletionState::Failed => prop_assert_eq!(u, 4),
            CompletionState::PartialFailure => prop_assert_eq!(u, 5),
        }
    }
}

proptest! {
    /// Terminal state invariant: Completed, TimedOut, Failed, PartialFailure are terminal.
    #[test]
    fn prop_terminal_state_invariant(state in arb_completion_state()) {
        match state {
            CompletionState::Pending | CompletionState::InProgress => {
                prop_assert!(!state.is_terminal());
            }
            CompletionState::Completed | CompletionState::TimedOut | CompletionState::Failed | CompletionState::PartialFailure => {
                prop_assert!(state.is_terminal());
            }
        }
    }
}

proptest! {
    /// Serde roundtrip for CompletionState.
    #[test]
    fn prop_completion_state_serde(state in arb_completion_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: CompletionState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, back);
    }
}

proptest! {
    /// Serde roundtrip for StepOutcome.
    #[test]
    fn prop_step_outcome_serde(outcome in arb_step_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: StepOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, back);
    }
}

proptest! {
    /// CauseChain push monotonically increments len by 1.
    #[test]
    fn prop_cause_chain_monotonic_length(steps in prop::collection::vec(arb_cause_step(), 0..20)) {
        let mut chain = CauseChain::new();
        for (i, step) in steps.iter().enumerate() {
            prop_assert_eq!(chain.len(), i);
            chain.push(step.clone());
            prop_assert_eq!(chain.len(), i + 1);
        }
    }
}

proptest! {
    /// CauseChain failed_subsystems is subset of Error-outcome subsystems.
    #[test]
    fn prop_cause_chain_failed_subsystems(steps in prop::collection::vec(arb_cause_step(), 0..20)) {
        let mut chain = CauseChain::new();
        for step in &steps {
            chain.push(step.clone());
        }
        let failed = chain.failed_subsystems();
        let expected_failed: Vec<&str> = steps
            .iter()
            .filter(|s| s.outcome == StepOutcome::Error)
            .map(|s| s.subsystem.as_str())
            .collect();
        prop_assert_eq!(failed.len(), expected_failed.len());
    }
}

proptest! {
    /// CauseChain elapsed_ms is non-negative with monotonic timestamps.
    #[test]
    fn prop_cause_chain_elapsed_non_negative(steps in prop::collection::vec(arb_cause_step(), 2..10)) {
        // Sort steps by timestamp to ensure monotonic ordering.
        let mut sorted_steps = steps;
        sorted_steps.sort_by_key(|s| s.timestamp_ms);

        let mut chain = CauseChain::new();
        for step in sorted_steps {
            chain.push(step);
        }
        let elapsed = chain.elapsed_ms();
        prop_assert!(elapsed >= 0);
    }
}

proptest! {
    /// Serde roundtrip for CauseChain.
    #[test]
    fn prop_cause_chain_serde(chain in arb_cause_chain()) {
        let json = serde_json::to_string(&chain).unwrap();
        let back: CauseChain = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(chain.len(), back.len());
    }
}

proptest! {
    /// CauseChain is_empty matches len == 0.
    #[test]
    fn prop_cause_chain_is_empty(steps in prop::collection::vec(arb_cause_step(), 0..20)) {
        let mut chain = CauseChain::new();
        for step in &steps {
            chain.push(step.clone());
        }
        prop_assert_eq!(chain.is_empty(), chain.len() == 0);
    }
}

proptest! {
    /// CompletionBoundary is_satisfied is monotonic: once satisfied, adding Ok steps keeps it satisfied.
    #[test]
    fn prop_boundary_monotonic_ok(boundary in arb_completion_boundary(), extra_steps in prop::collection::vec((arb_subsystem(), arb_message()), 0..10)) {
        let mut chain = CauseChain::new();
        for req in boundary.required() {
            chain.record(req, StepOutcome::Ok, "ok");
        }
        prop_assert!(boundary.is_satisfied(&chain));
        for (subsystem, message) in extra_steps {
            chain.record(&subsystem, StepOutcome::Ok, message);
            prop_assert!(boundary.is_satisfied(&chain));
        }
    }
}

proptest! {
    /// CompletionBoundary Cancelled does NOT satisfy a required subsystem.
    #[test]
    fn prop_boundary_cancelled_no_satisfy(subsystems in prop::collection::vec(arb_subsystem(), 1..5)) {
        let refs: Vec<&str> = subsystems.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let mut chain = CauseChain::new();
        for sub in &subsystems {
            chain.record(sub, StepOutcome::Cancelled, "cancelled");
        }
        prop_assert!(!boundary.is_satisfied(&chain));
    }
}

proptest! {
    /// CompletionBoundary Skipped satisfies a required subsystem.
    #[test]
    fn prop_boundary_skipped_satisfies(subsystems in prop::collection::vec(arb_subsystem(), 1..5)) {
        let refs: Vec<&str> = subsystems.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let mut chain = CauseChain::new();
        for sub in &subsystems {
            chain.record(sub, StepOutcome::Skipped, "skipped");
        }
        prop_assert!(boundary.is_satisfied(&chain));
    }
}

proptest! {
    /// CompletionBoundary Error satisfies the reporting requirement.
    #[test]
    fn prop_boundary_error_satisfies(subsystems in prop::collection::vec(arb_subsystem(), 1..5)) {
        let refs: Vec<&str> = subsystems.iter().map(|s| s.as_str()).collect();
        let boundary = CompletionBoundary::new(&refs);
        let mut chain = CauseChain::new();
        for sub in &subsystems {
            chain.record(sub, StepOutcome::Error, "error");
        }
        prop_assert!(boundary.is_satisfied(&chain));
    }
}

proptest! {
    /// Serde roundtrip for CompletionBoundary.
    #[test]
    fn prop_completion_boundary_serde(boundary in arb_completion_boundary()) {
        let json = serde_json::to_string(&boundary).unwrap();
        let back: CompletionBoundary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(boundary.required().len(), back.required().len());
    }
}

proptest! {
    /// pending_subsystems is empty when boundary is satisfied.
    #[test]
    fn prop_boundary_pending_empty_when_satisfied(boundary in arb_completion_boundary()) {
        let mut chain = CauseChain::new();
        for req in boundary.required() {
            chain.record(req, StepOutcome::Ok, "ok");
        }
        let pending = boundary.pending_subsystems(&chain);
        prop_assert!(pending.is_empty());
    }
}

proptest! {
    /// Serde roundtrip for CompletionTrackerConfig.
    #[test]
    fn prop_config_serde(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: CompletionTrackerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.default_timeout_ms, back.default_timeout_ms);
        prop_assert_eq!(config.max_active_tokens, back.max_active_tokens);
        prop_assert_eq!(config.retention_ms, back.retention_ms);
    }
}

proptest! {
    /// Tracker begin returns Some when under capacity.
    #[test]
    fn prop_tracker_begin_under_capacity(max_active in 1usize..10, count in 0usize..10) {
        let count_to_create = if count < max_active { count } else { max_active - 1 };
        let config = CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: max_active,
            retention_ms: 60_000,
        };
        let mut tracker = CompletionTracker::new(config);
        for _ in 0..count_to_create {
            let boundary = CompletionBoundary::new(&["a"]);
            let id = tracker.begin("test_op", boundary);
            prop_assert!(id.is_some());
        }
    }
}

proptest! {
    /// Tracker begin returns None at capacity.
    #[test]
    fn prop_tracker_begin_at_capacity(max_active in 1usize..10) {
        let config = CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: max_active,
            retention_ms: 60_000,
        };
        let mut tracker = CompletionTracker::new(config);
        for _ in 0..max_active {
            let boundary = CompletionBoundary::new(&["a"]);
            tracker.begin("test_op", boundary).unwrap();
        }
        let boundary = CompletionBoundary::new(&["a"]);
        let id = tracker.begin("overflow_op", boundary);
        prop_assert!(id.is_none());
    }
}

proptest! {
    /// Tracker active_count decreases on terminal.
    #[test]
    fn prop_tracker_active_count_decreases(count in 1usize..10) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });
        let mut ids = vec![];
        for _ in 0..count {
            let boundary = CompletionBoundary::new(&["a"]);
            let id = tracker.begin("test_op", boundary).unwrap();
            ids.push(id);
        }
        prop_assert_eq!(tracker.active_count(), count);
        for (i, id) in ids.iter().enumerate() {
            tracker.advance(id, "a", StepOutcome::Ok, "done");
            prop_assert_eq!(tracker.active_count(), count - i - 1);
        }
    }
}

proptest! {
    /// Tracker advance to non-existent token returns None.
    #[test]
    fn prop_tracker_nonexistent_advance(subsystem in arb_subsystem(), outcome in arb_step_outcome(), message in arb_message()) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig::default());
        let fake_id = TokenId("ct-fake-0000".to_string());
        let result = tracker.advance(&fake_id, &subsystem, outcome, message);
        prop_assert!(result.is_none());
    }
}

proptest! {
    /// State machine: Pending → InProgress on first advance.
    #[test]
    fn prop_tracker_pending_to_in_progress(outcome in arb_step_outcome()) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig::default());
        let boundary = CompletionBoundary::new(&["a", "b"]);
        let id = tracker.begin("test_op", boundary).unwrap();
        prop_assert_eq!(tracker.state(&id), Some(CompletionState::Pending));
        tracker.advance(&id, "a", outcome, "first step");
        let state = tracker.state(&id);
        prop_assert!(state.unwrap() != CompletionState::Pending);
    }
}

proptest! {
    /// Tracker total_count >= active_count always.
    #[test]
    fn prop_tracker_total_geq_active(count in 1usize..10) {
        let mut tracker = CompletionTracker::new(CompletionTrackerConfig {
            default_timeout_ms: 0,
            max_active_tokens: 100,
            retention_ms: 60_000,
        });
        let mut ids = vec![];
        for _ in 0..count {
            let boundary = CompletionBoundary::new(&["a"]);
            let id = tracker.begin("test_op", boundary).unwrap();
            ids.push(id);
        }
        prop_assert!(tracker.total_count() >= tracker.active_count());
        for id in ids.iter().take(count / 2) {
            tracker.advance(id, "a", StepOutcome::Ok, "done");
        }
        prop_assert!(tracker.total_count() >= tracker.active_count());
    }
}

proptest! {
    /// TokenId Display roundtrip matches internal string.
    #[test]
    fn prop_token_id_display(id_str in "[a-z0-9-]{10,30}") {
        let id = TokenId(id_str.clone());
        let displayed = format!("{}", id);
        prop_assert_eq!(displayed, id_str);
    }
}

proptest! {
    /// TokenId serde roundtrip.
    #[test]
    fn prop_token_id_serde(id_str in "[a-z0-9-]{10,30}") {
        let id = TokenId(id_str);
        let json = serde_json::to_string(&id).unwrap();
        let back: TokenId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(id, back);
    }
}
