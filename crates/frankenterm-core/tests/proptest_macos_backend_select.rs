use proptest::prelude::*;

use frankenterm_core::macos_backend_select::{
    BackendFallbackReason, BackendOverride, BackendSelectionInputs, BackendSelectionResult,
    BackendStats, MacosArch, MacosBackend, MacosVersion, SWAP_CHAIN_SLOTS, SwapChainRotation,
    SwapChainSlot, select_macos_backend,
};

fn arb_arch() -> impl Strategy<Value = MacosArch> {
    prop_oneof![
        Just(MacosArch::AppleSilicon),
        Just(MacosArch::IntelX64),
        Just(MacosArch::Unknown),
    ]
}

fn arb_override() -> impl Strategy<Value = BackendOverride> {
    prop_oneof![
        Just(BackendOverride::Auto),
        Just(BackendOverride::Wgpu),
        Just(BackendOverride::MetalDirect),
    ]
}

fn expected_selection(inputs: BackendSelectionInputs) -> BackendSelectionResult {
    match inputs.override_ {
        BackendOverride::Wgpu => BackendSelectionResult {
            backend: MacosBackend::Wgpu,
            reason: BackendFallbackReason::OperatorOverrideWgpu,
        },
        BackendOverride::MetalDirect => {
            if inputs.arch == MacosArch::AppleSilicon && inputs.version.meets_baseline() {
                BackendSelectionResult {
                    backend: MacosBackend::MetalDirect,
                    reason: BackendFallbackReason::OperatorOverrideMetalDirect,
                }
            } else {
                BackendSelectionResult {
                    backend: MacosBackend::Wgpu,
                    reason: BackendFallbackReason::OperatorOverrideDowngraded,
                }
            }
        }
        BackendOverride::Auto => match inputs.arch {
            MacosArch::AppleSilicon if inputs.version.meets_baseline() => BackendSelectionResult {
                backend: MacosBackend::MetalDirect,
                reason: BackendFallbackReason::MetalDirectGranted,
            },
            MacosArch::AppleSilicon => BackendSelectionResult {
                backend: MacosBackend::Wgpu,
                reason: BackendFallbackReason::PreBaselineVersion,
            },
            MacosArch::IntelX64 => BackendSelectionResult {
                backend: MacosBackend::Wgpu,
                reason: BackendFallbackReason::IntelArch,
            },
            MacosArch::Unknown => BackendSelectionResult {
                backend: MacosBackend::Wgpu,
                reason: BackendFallbackReason::UnknownArch,
            },
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn proptest_macos_backend_version_baseline_matches_tuple_order(
        major in any::<u8>(),
        minor in any::<u8>(),
    ) {
        let version = MacosVersion::new(major, minor);
        prop_assert_eq!(version.major, major);
        prop_assert_eq!(version.minor, minor);
        prop_assert_eq!(version.meets_baseline(), major >= 13);
    }

    #[test]
    fn proptest_macos_backend_override_parser_is_trimmed_and_case_insensitive(
        prefix in "\\s{0,4}",
        suffix in "\\s{0,4}",
        spelling in prop_oneof![
            Just("auto"),
            Just("default"),
            Just("wgpu"),
            Just("metal-direct"),
            Just("metal_direct"),
            Just("metaldirect"),
            Just("metal"),
        ],
        upper in any::<bool>(),
    ) {
        let body = if upper {
            spelling.to_ascii_uppercase()
        } else {
            spelling.to_string()
        };
        let parsed = BackendOverride::from_env_str(&format!("{prefix}{body}{suffix}"));
        let expected = match spelling {
            "wgpu" => BackendOverride::Wgpu,
            "metal-direct" | "metal_direct" | "metaldirect" | "metal" => {
                BackendOverride::MetalDirect
            }
            _ => BackendOverride::Auto,
        };

        prop_assert_eq!(parsed, expected);
    }

    #[test]
    fn proptest_macos_backend_selector_matches_policy_table(
        arch in arb_arch(),
        major in any::<u8>(),
        minor in any::<u8>(),
        override_ in arb_override(),
    ) {
        let inputs = BackendSelectionInputs::new(arch, MacosVersion::new(major, minor), override_);
        let selected = select_macos_backend(inputs);
        let expected = expected_selection(inputs);

        prop_assert_eq!(selected, expected);
        prop_assert_eq!(selected.is_metal_direct(), selected.backend == MacosBackend::MetalDirect);
        prop_assert_eq!(
            selected.is_fallback(),
            selected.reason != BackendFallbackReason::MetalDirectGranted,
        );
    }

    #[test]
    fn proptest_macos_backend_swap_chain_slots_stay_in_three_slot_ring(
        idx in any::<u8>(),
        steps in 0usize..128,
    ) {
        let constructed = SwapChainSlot::try_new(idx);
        prop_assert_eq!(constructed.is_some(), idx < SWAP_CHAIN_SLOTS);

        if let Some(mut slot) = constructed {
            for _ in 0..steps {
                slot = slot.next();
                prop_assert!(slot.0 < SWAP_CHAIN_SLOTS);
            }
            prop_assert_eq!(slot.0, (idx as usize + steps).rem_euclid(SWAP_CHAIN_SLOTS as usize) as u8);
        }
    }

    #[test]
    fn proptest_macos_backend_rotation_advances_modulo_swap_chain_slots(
        steps in 0usize..256,
    ) {
        let mut rotation = SwapChainRotation::new();
        for _ in 0..steps {
            rotation.advance();
        }

        let expected = (steps % SWAP_CHAIN_SLOTS as usize) as u8;
        prop_assert_eq!(rotation.current_slot(), SwapChainSlot(expected));
    }

    #[test]
    fn proptest_macos_backend_stats_present_rate_is_bounded_integer_ratio(
        presented in 0u64..=1_000_000,
        skipped in 0u64..=1_000_000,
    ) {
        let stats = BackendStats {
            frames_presented: presented,
            frames_skipped: skipped,
            backend_switches: 0,
        };
        let total = presented + skipped;
        let expected = if total == 0 {
            0
        } else {
            ((presented * 100) / total).min(100) as u32
        };

        prop_assert_eq!(stats.present_rate_pct(), expected);
        prop_assert!(stats.present_rate_pct() <= 100);
    }
}
