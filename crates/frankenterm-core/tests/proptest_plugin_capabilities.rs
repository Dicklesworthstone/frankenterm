use proptest::prelude::*;

use frankenterm_core::plugin_capabilities::{
    ApprovalToken, CapabilityClass, CapabilityGrantDecision, ManifestValidationError,
    PluginCapability, PluginLifecycleState, PluginLoaderPolicy, PluginManifest,
    PluginResourceBudget, PluginSignatureClass, decide_capability_grant, validate_manifest,
};

fn arb_capability() -> impl Strategy<Value = PluginCapability> {
    prop_oneof![
        Just(PluginCapability::ReadState),
        Just(PluginCapability::ReadEvents),
        Just(PluginCapability::RenderTile),
        Just(PluginCapability::RenderSidePanel),
        Just(PluginCapability::RenderOverlay),
        Just(PluginCapability::ReadOwnConfig),
        Just(PluginCapability::ReadPaneText),
        Just(PluginCapability::SendInput),
        Just(PluginCapability::SpawnPane),
        Just(PluginCapability::ClosePane),
        Just(PluginCapability::LaunchWorkflow),
        Just(PluginCapability::Network),
        Just(PluginCapability::FileSystemEscape),
        Just(PluginCapability::DirectPtyAccess),
        Just(PluginCapability::DirectMuxIpc),
        Just(PluginCapability::ArbitrarySubprocess),
    ]
}

fn arb_signature() -> impl Strategy<Value = PluginSignatureClass> {
    prop_oneof![
        Just(PluginSignatureClass::Unsigned),
        Just(PluginSignatureClass::SelfSigned),
        Just(PluginSignatureClass::Verified),
    ]
}

fn arb_lifecycle_state() -> impl Strategy<Value = PluginLifecycleState> {
    prop_oneof![
        Just(PluginLifecycleState::Loaded),
        Just(PluginLifecycleState::Initialised),
        Just(PluginLifecycleState::Running),
        Just(PluginLifecycleState::Reloading),
        Just(PluginLifecycleState::Stopped),
        Just(PluginLifecycleState::Errored),
    ]
}

fn signature_rank(signature: PluginSignatureClass) -> u8 {
    match signature {
        PluginSignatureClass::Unsigned => 0,
        PluginSignatureClass::SelfSigned => 1,
        PluginSignatureClass::Verified => 2,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn proptest_plugin_capabilities_class_helpers_match_classification(
        capability in arb_capability(),
    ) {
        prop_assert_eq!(capability.is_permissive(), capability.class() == CapabilityClass::Permissive);
        prop_assert_eq!(
            capability.requires_approval_token(),
            capability.class() == CapabilityClass::Restricted,
        );
        prop_assert_eq!(capability.is_forbidden(), capability.class() == CapabilityClass::Forbidden);

        let class_count = u8::from(capability.is_permissive())
            + u8::from(capability.requires_approval_token())
            + u8::from(capability.is_forbidden());
        prop_assert_eq!(class_count, 1);
    }

    #[test]
    fn proptest_plugin_capabilities_budget_clamp_caps_each_axis(
        render_deadline_ms in any::<u32>(),
        memory_cap_bytes in any::<u64>(),
        disk_cap_bytes in any::<u64>(),
    ) {
        let budget = PluginResourceBudget {
            render_deadline_ms,
            memory_cap_bytes,
            disk_cap_bytes,
        };
        let safe = budget.clamp_to_safe_bounds();

        prop_assert!(safe.budget.render_deadline_ms <= 50);
        prop_assert!(safe.budget.memory_cap_bytes <= 16 * 1024 * 1024);
        prop_assert!(safe.budget.disk_cap_bytes <= 100 * 1024 * 1024);
        prop_assert_eq!(safe.render_clamped, render_deadline_ms > 50);
        prop_assert_eq!(safe.mem_clamped, memory_cap_bytes > 16 * 1024 * 1024);
        prop_assert_eq!(safe.disk_clamped, disk_cap_bytes > 100 * 1024 * 1024);
        prop_assert_eq!(
            budget.any_clamp_engaged(),
            safe.render_clamped || safe.mem_clamped || safe.disk_clamped,
        );
    }

    #[test]
    fn proptest_plugin_capabilities_manifest_signature_policy_is_monotonic(
        actual in arb_signature(),
        minimum in arb_signature(),
    ) {
        let manifest = PluginManifest::new("plugin.test", "1.0.0")
            .with_signature(actual)
            .with_capabilities([PluginCapability::ReadState]);
        let policy = PluginLoaderPolicy {
            minimum_signature_class: minimum,
        };
        let validation = validate_manifest(&manifest, policy);

        if signature_rank(actual) < signature_rank(minimum) {
            prop_assert_eq!(
                validation,
                Err(ManifestValidationError::SignatureClassDisallowed {
                    plugin_id: "plugin.test".to_string(),
                    actual,
                    minimum,
                }),
            );
        } else {
            prop_assert_eq!(validation, Ok(()));
        }
    }

    #[test]
    fn proptest_plugin_capabilities_manifest_rejects_duplicate_or_forbidden_caps(
        capability in arb_capability(),
        duplicate in any::<bool>(),
    ) {
        let caps = if duplicate {
            vec![capability, capability]
        } else {
            vec![capability]
        };
        let manifest = PluginManifest::new("plugin.test", "1.0.0")
            .with_signature(PluginSignatureClass::Verified)
            .with_capabilities(caps);
        let validation = validate_manifest(&manifest, PluginLoaderPolicy::allow_unsigned());

        if capability.is_forbidden() {
            prop_assert_eq!(
                validation,
                Err(ManifestValidationError::ForbiddenCapabilityDeclared {
                    plugin_id: "plugin.test".to_string(),
                    capability,
                }),
            );
        } else if duplicate {
            prop_assert_eq!(
                validation,
                Err(ManifestValidationError::DuplicateCapability {
                    plugin_id: "plugin.test".to_string(),
                    capability,
                }),
            );
        } else {
            prop_assert_eq!(validation, Ok(()));
        }
    }

    #[test]
    fn proptest_plugin_capabilities_grant_decision_matches_manifest_and_tokens(
        requested in arb_capability(),
        declared in any::<bool>(),
        matching_token in any::<bool>(),
    ) {
        let manifest = if declared {
            PluginManifest::new("plugin.test", "1.0.0").with_capabilities([requested])
        } else {
            PluginManifest::new("plugin.test", "1.0.0")
        };
        let tokens = if matching_token {
            vec![ApprovalToken {
                plugin_id: "plugin.test".to_string(),
                capability: requested,
            }]
        } else {
            vec![ApprovalToken {
                plugin_id: "other.plugin".to_string(),
                capability: requested,
            }]
        };
        let decision = decide_capability_grant(&manifest, &tokens, requested);

        let expected = if requested.is_forbidden() {
            CapabilityGrantDecision::DeniedForbidden
        } else if !declared {
            CapabilityGrantDecision::DeniedNotInManifest
        } else if requested.is_permissive() || matching_token {
            CapabilityGrantDecision::Granted
        } else {
            CapabilityGrantDecision::DeniedNoApproval
        };

        prop_assert_eq!(decision, expected);
    }

    #[test]
    fn proptest_plugin_capabilities_lifecycle_transition_helpers_match_allowed_table(
        state in arb_lifecycle_state(),
        next in arb_lifecycle_state(),
    ) {
        prop_assert_eq!(state.can_transition_to(next), state.allowed_next().contains(&next));
        prop_assert_eq!(
            state.is_render_eligible(),
            matches!(state, PluginLifecycleState::Initialised | PluginLifecycleState::Running),
        );
    }
}
