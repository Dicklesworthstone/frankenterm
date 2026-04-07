//! Property-based tests for the mission_dispatch module.
//!
//! Covers serde roundtrips for DispatchResult, BatchDispatchResult, and
//! MissionDispatcherConfig, plus detection builder invariants.
//!
//! 8 property tests across 4 proptest! blocks.

#![cfg(feature = "subprocess-bridge")]

use proptest::prelude::*;

use frankenterm_core::mission_dispatch::{
    BatchDispatchResult, DispatchResult, MissionDispatcher, MissionDispatcherConfig,
};

// =============================================================================
// Strategies
// =============================================================================

/// Safe string: alphanumeric + underscore/hyphen, 1..30 chars.
fn arb_safe_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,30}"
}

/// Arbitrary DispatchResult.
fn arb_dispatch_result() -> impl Strategy<Value = DispatchResult> {
    (
        arb_safe_string(),
        arb_safe_string(),
        any::<bool>(),
        proptest::option::of(arb_safe_string()),
        proptest::option::of(arb_safe_string()),
        0u64..1_000_000,
    )
        .prop_map(
            |(assignment_id, target_agent, accepted, execution_id, reason, dispatch_ms)| {
                DispatchResult {
                    assignment_id,
                    target_agent,
                    accepted,
                    execution_id,
                    reason,
                    dispatch_ms,
                }
            },
        )
}

/// Arbitrary MissionDispatcherConfig.
fn arb_config() -> impl Strategy<Value = MissionDispatcherConfig> {
    (any::<bool>(), arb_safe_string(), arb_safe_string()).prop_map(
        |(sequential, workspace, track)| MissionDispatcherConfig {
            sequential,
            workspace,
            track,
        },
    )
}

/// Arbitrary BatchDispatchResult.
fn arb_batch_result() -> impl Strategy<Value = BatchDispatchResult> {
    (
        proptest::collection::vec(arb_dispatch_result(), 0..10),
        0u64..1_000_000,
        0u64..1000,
    )
        .prop_map(|(results, total_ms, cycle_id)| {
            let accepted_count = results.iter().filter(|r| r.accepted).count();
            let failed_count = results.len() - accepted_count;
            BatchDispatchResult {
                results,
                accepted_count,
                failed_count,
                total_ms,
                cycle_id,
            }
        })
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    // ── DispatchResult serde roundtrip ──────────────────────────────────

    #[test]
    fn dispatch_result_serde_roundtrip(result in arb_dispatch_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.assignment_id, result.assignment_id);
        prop_assert_eq!(restored.target_agent, result.target_agent);
        prop_assert_eq!(restored.accepted, result.accepted);
        prop_assert_eq!(restored.execution_id, result.execution_id);
        prop_assert_eq!(restored.reason, result.reason);
        prop_assert_eq!(restored.dispatch_ms, result.dispatch_ms);
    }

    #[test]
    fn dispatch_result_json_not_empty(result in arb_dispatch_result()) {
        let json = serde_json::to_string(&result).unwrap();
        prop_assert!(!json.is_empty());
        prop_assert!(json.contains("assignment_id"));
        prop_assert!(json.contains("accepted"));
    }

    // ── BatchDispatchResult serde roundtrip ─────────────────────────────

    #[test]
    fn batch_dispatch_result_serde_roundtrip(batch in arb_batch_result()) {
        let json = serde_json::to_string(&batch).unwrap();
        let restored: BatchDispatchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.results.len(), batch.results.len());
        prop_assert_eq!(restored.accepted_count, batch.accepted_count);
        prop_assert_eq!(restored.failed_count, batch.failed_count);
        prop_assert_eq!(restored.total_ms, batch.total_ms);
        prop_assert_eq!(restored.cycle_id, batch.cycle_id);
    }

    #[test]
    fn batch_dispatch_result_counts_consistent(batch in arb_batch_result()) {
        prop_assert_eq!(batch.accepted_count + batch.failed_count, batch.results.len());
        let actual_accepted = batch.results.iter().filter(|r| r.accepted).count();
        prop_assert_eq!(actual_accepted, batch.accepted_count);
    }

    // ── MissionDispatcherConfig serde roundtrip ─────────────────────────

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let restored: MissionDispatcherConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.sequential, config.sequential);
        prop_assert_eq!(restored.workspace, config.workspace);
        prop_assert_eq!(restored.track, config.track);
    }

    // ── Detection builder invariants ────────────────────────────────────

    #[test]
    fn dispatch_contract_fields_preserved(
        assignment_id in arb_safe_string(),
        target_agent in arb_safe_string(),
    ) {
        let contract = frankenterm_core::plan::MissionDispatchContract {
            assignment_id: assignment_id.clone(),
            target_agent: target_agent.clone(),
        };
        prop_assert_eq!(&contract.assignment_id, &assignment_id);
        prop_assert_eq!(&contract.target_agent, &target_agent);
    }

    #[test]
    fn dispatcher_config_default_is_valid(_dummy in 0u8..1) {
        let config = MissionDispatcherConfig::default();
        prop_assert!(!config.sequential);
        prop_assert!(!config.workspace.is_empty());
        prop_assert!(!config.track.is_empty());
        let dispatcher = MissionDispatcher::new(config);
        let _ = dispatcher; // Compiles and constructs successfully
    }
}
