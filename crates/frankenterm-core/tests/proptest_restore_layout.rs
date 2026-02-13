//! Property-based tests for the `restore_layout` module.
//!
//! Covers `RestoreConfig` serde roundtrips, default values, and boolean flag
//! combinations.

use frankenterm_core::restore_layout::RestoreConfig;
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_restore_config() -> impl Strategy<Value = RestoreConfig> {
    (any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
        |(restore_working_dirs, restore_split_ratios, continue_on_error)| RestoreConfig {
            restore_working_dirs,
            restore_split_ratios,
            continue_on_error,
        },
    )
}

// =========================================================================
// RestoreConfig — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// RestoreConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde(config in arb_restore_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.restore_working_dirs, config.restore_working_dirs);
        prop_assert_eq!(back.restore_split_ratios, config.restore_split_ratios);
        prop_assert_eq!(back.continue_on_error, config.continue_on_error);
    }

    /// Default RestoreConfig has all flags true.
    #[test]
    fn prop_default_config(_dummy in 0..1_u8) {
        let config = RestoreConfig::default();
        prop_assert!(config.restore_working_dirs);
        prop_assert!(config.restore_split_ratios);
        prop_assert!(config.continue_on_error);
    }

    /// RestoreConfig serde is deterministic.
    #[test]
    fn prop_config_deterministic(config in arb_restore_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// RestoreConfig deserializes from empty object with defaults (due to #[serde(default)]).
    #[test]
    fn prop_config_from_empty_json(_dummy in 0..1_u8) {
        let back: RestoreConfig = serde_json::from_str("{}").unwrap();
        prop_assert!(back.restore_working_dirs);
        prop_assert!(back.restore_split_ratios);
        prop_assert!(back.continue_on_error);
    }

    /// RestoreConfig deserializes from partial JSON with defaults for missing fields.
    #[test]
    fn prop_config_partial_json(val in any::<bool>()) {
        let json = format!("{{\"restore_working_dirs\":{}}}", val);
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.restore_working_dirs, val);
        // Missing fields should get defaults (true)
        prop_assert!(back.restore_split_ratios);
        prop_assert!(back.continue_on_error);
    }

    /// All 8 boolean combinations roundtrip correctly.
    #[test]
    fn prop_all_bool_combos(config in arb_restore_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: RestoreConfig = serde_json::from_str(&json).unwrap();
        // All three booleans survive
        prop_assert_eq!(
            (back.restore_working_dirs, back.restore_split_ratios, back.continue_on_error),
            (config.restore_working_dirs, config.restore_split_ratios, config.continue_on_error)
        );
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn config_default_values() {
    let config = RestoreConfig::default();
    assert!(config.restore_working_dirs);
    assert!(config.restore_split_ratios);
    assert!(config.continue_on_error);
}

#[test]
fn config_roundtrip_all_false() {
    let config = RestoreConfig {
        restore_working_dirs: false,
        restore_split_ratios: false,
        continue_on_error: false,
    };
    let json = serde_json::to_string(&config).unwrap();
    let back: RestoreConfig = serde_json::from_str(&json).unwrap();
    assert!(!back.restore_working_dirs);
    assert!(!back.restore_split_ratios);
    assert!(!back.continue_on_error);
}
