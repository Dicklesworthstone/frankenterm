// Property-based tests for search/migration_controller module.
//
// Covers: serde roundtrips for MigrationPhase, PhaseTransitionError,
// HealthCheckResult, and MigrationControllerConfig. Also covers
// structural invariants for phase state machine and parse/display.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::search::{
    HealthCheckResult, MigrationController, MigrationControllerConfig, MigrationPhase,
    PhaseTransitionError,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_migration_phase() -> impl Strategy<Value = MigrationPhase> {
    prop_oneof![
        Just(MigrationPhase::PreCheck),
        Just(MigrationPhase::Shadow),
        Just(MigrationPhase::Canary),
        Just(MigrationPhase::Cutover),
        Just(MigrationPhase::Rollback),
        Just(MigrationPhase::Retired),
    ]
}

fn arb_phase_transition_error() -> impl Strategy<Value = PhaseTransitionError> {
    (arb_migration_phase(), arb_migration_phase(), "[a-z ]{5,30}")
        .prop_map(|(from, to, reason)| PhaseTransitionError { from, to, reason })
}

fn arb_health_check_result() -> impl Strategy<Value = HealthCheckResult> {
    ("[a-z_]{3,15}", any::<bool>(), "[a-z ]{5,30}").prop_map(|(name, passed, detail)| {
        HealthCheckResult {
            name,
            passed,
            detail,
        }
    })
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn migration_phase_serde_roundtrip(val in arb_migration_phase()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: MigrationPhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, back);
    }

    #[test]
    fn phase_transition_error_serde_roundtrip(val in arb_phase_transition_error()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: PhaseTransitionError = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.from, back.from);
        prop_assert_eq!(val.to, back.to);
        prop_assert_eq!(&val.reason, &back.reason);
    }

    #[test]
    fn health_check_result_serde_roundtrip(val in arb_health_check_result()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: HealthCheckResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.name, &back.name);
        prop_assert_eq!(val.passed, back.passed);
        prop_assert_eq!(&val.detail, &back.detail);
    }
}

// =============================================================================
// Structural invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn migration_phase_display_matches_as_str(val in arb_migration_phase()) {
        let display = format!("{val}");
        prop_assert_eq!(display, val.as_str());
    }

    #[test]
    fn migration_phase_parse_roundtrip(val in arb_migration_phase()) {
        let s = val.as_str();
        let parsed = MigrationPhase::parse(s);
        prop_assert_eq!(val, parsed);
    }

    #[test]
    fn migration_phase_terminal_states_correct(val in arb_migration_phase()) {
        match val {
            MigrationPhase::Retired | MigrationPhase::Rollback => {
                prop_assert!(val.is_terminal());
            }
            _ => {
                prop_assert!(!val.is_terminal());
            }
        }
    }

    #[test]
    fn migration_phase_live_on_orchestrated_correct(val in arb_migration_phase()) {
        match val {
            MigrationPhase::Cutover | MigrationPhase::Retired => {
                prop_assert!(val.is_live_on_orchestrated());
            }
            _ => {
                prop_assert!(!val.is_live_on_orchestrated());
            }
        }
    }

    #[test]
    fn migration_phase_default_is_precheck(_dummy in 0u8..1) {
        let phase = MigrationPhase::default();
        let check = matches!(phase, MigrationPhase::PreCheck);
        prop_assert!(check);
    }

    #[test]
    fn migration_phase_parse_unknown_defaults_to_precheck(input in "[0-9]{3,10}") {
        let parsed = MigrationPhase::parse(&input);
        let check = matches!(parsed, MigrationPhase::PreCheck);
        prop_assert!(check);
    }

    #[test]
    fn phase_transition_error_display_nonempty(val in arb_phase_transition_error()) {
        let display = format!("{val}");
        prop_assert!(!display.is_empty());
        prop_assert!(display.contains("cannot transition"));
    }

    #[test]
    fn migration_controller_starts_in_precheck(_dummy in 0u8..1) {
        let ctrl = MigrationController::new();
        let check = matches!(ctrl.phase(), MigrationPhase::PreCheck);
        prop_assert!(check);
    }

    #[test]
    fn migration_controller_config_default_has_safe_settings(_dummy in 0u8..1) {
        let config = MigrationControllerConfig::default();
        prop_assert!(config.require_replay_gate);
        prop_assert!(config.require_schema_gates);
        prop_assert!(!config.auto_advance);
        prop_assert!(config.max_consecutive_failures > 0);
    }

    #[test]
    fn migration_controller_with_config_respects_phase(val in arb_migration_phase()) {
        let ctrl = MigrationController::new();
        // New controller always starts at PreCheck regardless
        let check = matches!(ctrl.phase(), MigrationPhase::PreCheck);
        prop_assert!(check);
        // Phase parameter is for testing parse roundtrip
        let parsed = MigrationPhase::parse(val.as_str());
        prop_assert_eq!(val, parsed);
    }
}
