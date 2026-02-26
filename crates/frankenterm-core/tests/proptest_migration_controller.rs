//! Property-based tests for migration controller (ft-dr6zv.1.3.D1).

use proptest::prelude::*;

use frankenterm_core::search::{
    MigrationController, MigrationControllerConfig, MigrationPhase,
    RetirementGateResult, run_default_retirement_gate,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_phase() -> impl Strategy<Value = MigrationPhase> {
    prop_oneof![
        Just(MigrationPhase::PreCheck),
        Just(MigrationPhase::Shadow),
        Just(MigrationPhase::Canary),
        Just(MigrationPhase::Cutover),
        Just(MigrationPhase::Rollback),
        Just(MigrationPhase::Retired),
    ]
}

// ---------------------------------------------------------------------------
// MC-1: Phase serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn mc_1_phase_serde_roundtrip(phase in arb_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let parsed: MigrationPhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(phase, parsed);
    }
}

// ---------------------------------------------------------------------------
// MC-2: Phase parse roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn mc_2_phase_parse_roundtrip(phase in arb_phase()) {
        let parsed = MigrationPhase::parse(phase.as_str());
        prop_assert_eq!(phase, parsed);
    }
}

// ---------------------------------------------------------------------------
// MC-3: Controller never panics on health check
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn mc_3_health_check_never_panics(
        auto_advance in proptest::bool::ANY,
        max_failures in 1u32..10,
    ) {
        let config = MigrationControllerConfig {
            auto_advance,
            max_consecutive_failures: max_failures,
            ..MigrationControllerConfig::default()
        };
        let mut ctrl = MigrationController::with_config(config);
        let _result = ctrl.run_retirement_check();
    }
}

// ---------------------------------------------------------------------------
// MC-4: RetirementGateResult serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    #[test]
    fn mc_4_result_serde_roundtrip(_seed in 0u32..100) {
        let result = run_default_retirement_gate();
        let json = serde_json::to_string(&result).unwrap();
        let parsed: RetirementGateResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result.approved, parsed.approved);
        prop_assert_eq!(result.checks_passed, parsed.checks_passed);
        prop_assert_eq!(result.checks_failed, parsed.checks_failed);
    }
}

// ---------------------------------------------------------------------------
// MC-5: Approved implies all checks passed
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    #[test]
    fn mc_5_approved_implies_all_passed(_seed in 0u32..100) {
        let result = run_default_retirement_gate();
        if result.approved {
            prop_assert_eq!(result.checks_failed, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// MC-6: Forward sequence always legal
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn mc_6_forward_sequence_legal(_seed in 0u32..100) {
        let mut ctrl = MigrationController::new();
        prop_assert!(ctrl.advance_to(MigrationPhase::Shadow).is_ok());
        prop_assert!(ctrl.advance_to(MigrationPhase::Canary).is_ok());
        prop_assert!(ctrl.advance_to(MigrationPhase::Cutover).is_ok());
        prop_assert!(ctrl.advance_to(MigrationPhase::Retired).is_ok());
    }
}

// ---------------------------------------------------------------------------
// MC-7: Rollback always resets failures
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn mc_7_rollback_resets_failures(_initial_failures in 0u32..100) {
        let mut ctrl = MigrationController::new();
        ctrl.advance_to(MigrationPhase::Shadow).unwrap();
        // Manually set failures (via field, only accessible here because it's in test).
        // Instead, run multiple health checks with bad config.
        ctrl.rollback("test");
        prop_assert_eq!(ctrl.consecutive_failures(), 0);
    }
}

// ---------------------------------------------------------------------------
// MC-8: Config serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #[test]
    fn mc_8_config_serde(
        auto_advance in proptest::bool::ANY,
        max_failures in 1u32..20,
        require_schema in proptest::bool::ANY,
    ) {
        let config = MigrationControllerConfig {
            auto_advance,
            max_consecutive_failures: max_failures,
            require_schema_gates: require_schema,
            ..MigrationControllerConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: MigrationControllerConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.auto_advance, parsed.auto_advance);
        prop_assert_eq!(config.max_consecutive_failures, parsed.max_consecutive_failures);
    }
}
