//! Property-based tests for trauma guard telemetry counters (ft-3kxe.41).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. commands_recorded tracks record_command_result() calls
//! 3. interventions tracks only intervention decisions
//! 4. mutations_recorded tracks all record_mutation() calls
//! 5. functional_mutations tracks only functional mutation paths
//! 6. history_trims tracks when history exceeds limit
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::trauma_guard::{TraumaConfig, TraumaState, TraumaTelemetrySnapshot};

// =============================================================================
// Helpers
// =============================================================================

fn test_state() -> TraumaState {
    TraumaState::with_config(TraumaConfig {
        history_limit: 10,
        loop_threshold: 3,
        ..TraumaConfig::default()
    })
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let state = test_state();
    let snap = state.telemetry().snapshot();

    assert_eq!(snap.commands_recorded, 0);
    assert_eq!(snap.interventions, 0);
    assert_eq!(snap.mutations_recorded, 0);
    assert_eq!(snap.functional_mutations, 0);
    assert_eq!(snap.history_trims, 0);
}

#[test]
fn commands_recorded_tracked() {
    let mut state = test_state();
    let empty: Vec<String> = vec![];
    state.record_command_result(1000, "cargo test", &empty);
    state.record_command_result(2000, "cargo build", &empty);

    let snap = state.telemetry().snapshot();
    assert_eq!(snap.commands_recorded, 2);
}

#[test]
fn mutations_counted_both_functional_and_non() {
    let mut state = test_state();
    state.record_mutation(1000, "src/main.rs"); // functional
    state.record_mutation(2000, "AGENT_TODO.md"); // non-functional
    state.record_mutation(3000, "src/lib.rs"); // functional

    let snap = state.telemetry().snapshot();
    assert_eq!(snap.mutations_recorded, 3);
    assert_eq!(snap.functional_mutations, 2);
}

#[test]
fn history_trims_tracked() {
    let mut state = TraumaState::with_config(TraumaConfig {
        history_limit: 3,
        ..TraumaConfig::default()
    });
    let empty: Vec<String> = vec![];

    for i in 0..5 {
        state.record_command_result(i * 1000, "cargo test", &empty);
    }

    let snap = state.telemetry().snapshot();
    assert_eq!(snap.commands_recorded, 5);
    // After the 4th command, history exceeds limit of 3, triggering trim
    assert!(snap.history_trims >= 1, "expected at least 1 history trim");
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = TraumaTelemetrySnapshot {
        commands_recorded: 1000,
        interventions: 50,
        mutations_recorded: 200,
        functional_mutations: 150,
        history_trims: 10,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: TraumaTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn commands_equal_call_count(
        count in 1usize..20,
    ) {
        let mut state = test_state();
        let empty: Vec<String> = vec![];
        for i in 0..count {
            state.record_command_result((i * 1000) as u64, "cargo test", &empty);
        }
        let snap = state.telemetry().snapshot();
        prop_assert_eq!(snap.commands_recorded, count as u64);
    }

    #[test]
    fn mutations_equal_call_count(
        paths in prop::collection::vec(
            prop::sample::select(vec![
                "src/main.rs",
                "src/lib.rs",
                "AGENT_TODO.md",
                "README.md",
                "tests/test.rs",
                ".beads/issues.jsonl",
            ]),
            1..15,
        ),
    ) {
        let mut state = test_state();
        for (i, path) in paths.iter().enumerate() {
            state.record_mutation((i * 1000) as u64, path);
        }
        let snap = state.telemetry().snapshot();
        prop_assert_eq!(snap.mutations_recorded, paths.len() as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..3, 1..20),
    ) {
        let mut state = test_state();
        let empty: Vec<String> = vec![];
        let mut prev = state.telemetry().snapshot();

        for (i, op) in ops.iter().enumerate() {
            match op {
                0 => { state.record_command_result((i * 1000) as u64, "cargo test", &empty); }
                1 => { state.record_mutation((i * 1000) as u64, "src/main.rs"); }
                2 => { state.record_mutation((i * 1000) as u64, "README.md"); }
                _ => unreachable!(),
            }

            let snap = state.telemetry().snapshot();
            prop_assert!(snap.commands_recorded >= prev.commands_recorded,
                "commands_recorded decreased");
            prop_assert!(snap.interventions >= prev.interventions,
                "interventions decreased");
            prop_assert!(snap.mutations_recorded >= prev.mutations_recorded,
                "mutations_recorded decreased");
            prop_assert!(snap.functional_mutations >= prev.functional_mutations,
                "functional_mutations decreased");
            prop_assert!(snap.history_trims >= prev.history_trims,
                "history_trims decreased");

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        commands_recorded in 0u64..100000,
        interventions in 0u64..10000,
        mutations_recorded in 0u64..10000,
        functional_mutations in 0u64..10000,
        history_trims in 0u64..1000,
    ) {
        let snap = TraumaTelemetrySnapshot {
            commands_recorded,
            interventions,
            mutations_recorded,
            functional_mutations,
            history_trims,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: TraumaTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
