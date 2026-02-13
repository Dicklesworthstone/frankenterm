//! Property-based tests for the `rulesets` module.
//!
//! Verifies `PatternsConfigPatch::apply_to` algebraic properties (identity,
//! override semantics, merge invariants), `touch_last_applied` mutation
//! correctness, and serde roundtrips for manifest types.

use std::collections::HashMap;

use frankenterm_core::config::{PackOverride, PatternsConfig};
use frankenterm_core::rulesets::{
    PatternsConfigPatch, RulesetManifest, RulesetManifestEntry, RulesetProfileFile,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_pack_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9-]{0,15}"
}

fn arb_rule_id() -> impl Strategy<Value = String> {
    "[A-Z]{2,4}-[0-9]{3}"
}

fn arb_pack_override() -> impl Strategy<Value = PackOverride> {
    (
        proptest::collection::vec(arb_rule_id(), 0..5),
        proptest::collection::hash_map(arb_rule_id(), "info|warn|error", 0..3),
    )
        .prop_map(|(disabled, severity)| PackOverride {
            disabled_rules: disabled,
            severity_overrides: severity,
            extra: HashMap::new(),
        })
}

fn arb_patterns_config() -> impl Strategy<Value = PatternsConfig> {
    (
        proptest::collection::vec(arb_pack_name(), 0..5),
        proptest::collection::hash_map(arb_pack_name(), arb_pack_override(), 0..3),
        any::<bool>(),
    )
        .prop_map(|(packs, overrides, quick_reject)| PatternsConfig {
            packs,
            pack_overrides: overrides,
            quick_reject_enabled: quick_reject,
            user_packs_enabled: false,
            user_packs_dir: None,
        })
}

fn arb_patterns_config_patch() -> impl Strategy<Value = PatternsConfigPatch> {
    (
        proptest::option::of(proptest::collection::vec(arb_pack_name(), 0..5)),
        proptest::option::of(proptest::collection::hash_map(
            arb_pack_name(),
            arb_pack_override(),
            0..3,
        )),
        proptest::option::of(any::<bool>()),
    )
        .prop_map(|(packs, overrides, quick_reject)| PatternsConfigPatch {
            packs,
            pack_overrides: overrides,
            quick_reject_enabled: quick_reject,
        })
}

fn arb_ruleset_manifest_entry() -> impl Strategy<Value = RulesetManifestEntry> {
    (
        "[a-z][a-z0-9_-]{0,15}",
        "[a-z][a-z0-9_-]{0,15}\\.toml",
        proptest::option::of("[A-Za-z ]{5,30}"),
        proptest::option::of(0u64..10_000_000_000u64),
        proptest::option::of(0u64..10_000_000_000u64),
        proptest::option::of(0u64..10_000_000_000u64),
    )
        .prop_map(
            |(name, path, desc, created, updated, applied)| RulesetManifestEntry {
                name,
                path,
                description: desc,
                created_at: created,
                updated_at: updated,
                last_applied_at: applied,
            },
        )
}

// =========================================================================
// PatternsConfigPatch::apply_to — identity
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty patch is identity: applying an empty patch preserves the base.
    #[test]
    fn prop_empty_patch_is_identity(base in arb_patterns_config()) {
        let empty = PatternsConfigPatch::default();
        let result = empty.apply_to(&base);
        prop_assert_eq!(&result.packs, &base.packs);
        prop_assert_eq!(result.quick_reject_enabled, base.quick_reject_enabled);
        // pack_overrides should also be preserved
        prop_assert_eq!(result.pack_overrides.len(), base.pack_overrides.len());
        for (key, val) in &base.pack_overrides {
            let result_val = result.pack_overrides.get(key);
            prop_assert!(result_val.is_some(), "missing key '{}'", key);
            prop_assert_eq!(&result_val.unwrap().disabled_rules, &val.disabled_rules);
            prop_assert_eq!(&result_val.unwrap().severity_overrides, &val.severity_overrides);
        }
    }
}

// =========================================================================
// PatternsConfigPatch::apply_to — override semantics
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Packs field: when patch has Some(packs), result.packs == patch.packs.
    #[test]
    fn prop_patch_packs_override(
        base in arb_patterns_config(),
        new_packs in proptest::collection::vec(arb_pack_name(), 0..5),
    ) {
        let patch = PatternsConfigPatch {
            packs: Some(new_packs.clone()),
            ..Default::default()
        };
        let result = patch.apply_to(&base);
        prop_assert_eq!(&result.packs, &new_packs);
    }

    /// Quick reject: when patch has Some(v), result.quick_reject_enabled == v.
    #[test]
    fn prop_patch_quick_reject_override(
        base in arb_patterns_config(),
        new_val in any::<bool>(),
    ) {
        let patch = PatternsConfigPatch {
            quick_reject_enabled: Some(new_val),
            ..Default::default()
        };
        let result = patch.apply_to(&base);
        prop_assert_eq!(result.quick_reject_enabled, new_val);
    }

    /// Pack overrides merge: base disabled_rules are preserved, overlay adds new.
    #[test]
    fn prop_pack_overrides_merge_preserves_base_rules(
        base_rules in proptest::collection::vec(arb_rule_id(), 1..4),
        overlay_rules in proptest::collection::vec(arb_rule_id(), 1..4),
        pack_name in arb_pack_name(),
    ) {
        let base = PatternsConfig {
            pack_overrides: HashMap::from([(
                pack_name.clone(),
                PackOverride {
                    disabled_rules: base_rules.clone(),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let overlay = HashMap::from([(
            pack_name.clone(),
            PackOverride {
                disabled_rules: overlay_rules.clone(),
                ..Default::default()
            },
        )]);
        let patch = PatternsConfigPatch {
            pack_overrides: Some(overlay),
            ..Default::default()
        };
        let result = patch.apply_to(&base);
        let merged = result.pack_overrides.get(&pack_name).unwrap();

        // All base rules should still be present
        for rule in &base_rules {
            prop_assert!(
                merged.disabled_rules.contains(rule),
                "base rule '{}' lost after merge", rule
            );
        }
        // All overlay rules should be present
        for rule in &overlay_rules {
            prop_assert!(
                merged.disabled_rules.contains(rule),
                "overlay rule '{}' missing after merge", rule
            );
        }
    }

    /// Pack overrides merge: new pack keys in overlay are added.
    #[test]
    fn prop_pack_overrides_merge_adds_new_keys(
        base in arb_patterns_config(),
        new_key in "[z][a-z]{5,10}",
        new_override in arb_pack_override(),
    ) {
        let overlay = HashMap::from([(new_key.clone(), new_override)]);
        let patch = PatternsConfigPatch {
            pack_overrides: Some(overlay),
            ..Default::default()
        };
        let result = patch.apply_to(&base);
        prop_assert!(
            result.pack_overrides.contains_key(&new_key),
            "new key '{}' should exist after merge", new_key
        );
    }

    /// Severity overrides in overlay replace those in base.
    #[test]
    fn prop_severity_overrides_replaced(
        pack_name in arb_pack_name(),
        rule_id in arb_rule_id(),
    ) {
        let base = PatternsConfig {
            pack_overrides: HashMap::from([(
                pack_name.clone(),
                PackOverride {
                    severity_overrides: HashMap::from([(rule_id.clone(), "info".to_string())]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let overlay = HashMap::from([(
            pack_name.clone(),
            PackOverride {
                severity_overrides: HashMap::from([(rule_id.clone(), "error".to_string())]),
                ..Default::default()
            },
        )]);
        let patch = PatternsConfigPatch {
            pack_overrides: Some(overlay),
            ..Default::default()
        };
        let result = patch.apply_to(&base);
        let merged = result.pack_overrides.get(&pack_name).unwrap();
        prop_assert_eq!(
            merged.severity_overrides.get(&rule_id).map(|s| s.as_str()),
            Some("error"),
        );
    }
}

// =========================================================================
// touch_last_applied
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Touching an existing entry updates timestamps but preserves created_at.
    #[test]
    fn prop_touch_existing_preserves_created(
        name in "[a-z]{3,10}",
        path in "[a-z]{3,10}\\.toml",
        created in 1u64..1_000_000,
        old_applied in 1u64..1_000_000,
        new_applied in 1_000_001u64..2_000_000,
    ) {
        let mut manifest = RulesetManifest {
            version: 1,
            rulesets: vec![RulesetManifestEntry {
                name: name.clone(),
                path: path.clone(),
                description: None,
                created_at: Some(created),
                updated_at: Some(old_applied),
                last_applied_at: Some(old_applied),
            }],
        };

        frankenterm_core::rulesets::touch_last_applied(&mut manifest, &name, &path, new_applied);

        prop_assert_eq!(manifest.rulesets.len(), 1);
        prop_assert_eq!(manifest.rulesets[0].created_at, Some(created));
        prop_assert_eq!(manifest.rulesets[0].last_applied_at, Some(new_applied));
        prop_assert_eq!(manifest.rulesets[0].updated_at, Some(new_applied));
    }

    /// Touching a missing entry creates a new one.
    #[test]
    fn prop_touch_missing_creates_entry(
        name in "[a-z]{3,10}",
        path in "[a-z]{3,10}\\.toml",
        applied in 1u64..2_000_000,
    ) {
        let mut manifest = RulesetManifest::default();

        frankenterm_core::rulesets::touch_last_applied(&mut manifest, &name, &path, applied);

        prop_assert_eq!(manifest.rulesets.len(), 1);
        prop_assert_eq!(&manifest.rulesets[0].name, &name);
        prop_assert_eq!(&manifest.rulesets[0].path, &path);
        prop_assert_eq!(manifest.rulesets[0].created_at, Some(applied));
        prop_assert_eq!(manifest.rulesets[0].last_applied_at, Some(applied));
    }

    /// Double touch: second call updates the same entry, doesn't create new.
    #[test]
    fn prop_touch_idempotent_entry_count(
        name in "[a-z]{3,10}",
        path in "[a-z]{3,10}\\.toml",
        t1 in 1u64..1_000_000,
        t2 in 1_000_001u64..2_000_000,
    ) {
        let mut manifest = RulesetManifest::default();
        frankenterm_core::rulesets::touch_last_applied(&mut manifest, &name, &path, t1);
        prop_assert_eq!(manifest.rulesets.len(), 1);

        frankenterm_core::rulesets::touch_last_applied(&mut manifest, &name, &path, t2);
        prop_assert_eq!(manifest.rulesets.len(), 1);
        prop_assert_eq!(manifest.rulesets[0].last_applied_at, Some(t2));
    }
}

// =========================================================================
// Serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_manifest_entry_serde_roundtrip(entry in arb_ruleset_manifest_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: RulesetManifestEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&entry.name, &parsed.name);
        prop_assert_eq!(&entry.path, &parsed.path);
        prop_assert_eq!(&entry.description, &parsed.description);
        prop_assert_eq!(entry.created_at, parsed.created_at);
        prop_assert_eq!(entry.updated_at, parsed.updated_at);
        prop_assert_eq!(entry.last_applied_at, parsed.last_applied_at);
    }

    #[test]
    fn prop_manifest_serde_roundtrip(
        entries in proptest::collection::vec(arb_ruleset_manifest_entry(), 0..5),
    ) {
        let manifest = RulesetManifest {
            version: 1,
            rulesets: entries,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: RulesetManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(manifest.version, parsed.version);
        prop_assert_eq!(manifest.rulesets.len(), parsed.rulesets.len());
    }

    #[test]
    fn prop_profile_file_serde_roundtrip(
        name in "[a-z]{3,10}",
        desc in proptest::option::of("[A-Za-z ]{5,30}"),
        inherits in proptest::option::of("[a-z]{3,10}"),
    ) {
        let profile = RulesetProfileFile {
            name: name.clone(),
            description: desc.clone(),
            inherits: inherits.clone(),
            patterns: PatternsConfigPatch::default(),
        };
        let json = serde_json::to_string(&profile).unwrap();
        let parsed: RulesetProfileFile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&profile.name, &parsed.name);
        prop_assert_eq!(&profile.description, &parsed.description);
        prop_assert_eq!(&profile.inherits, &parsed.inherits);
    }

    #[test]
    fn prop_patterns_config_patch_serde_roundtrip(patch in arb_patterns_config_patch()) {
        let json = serde_json::to_string(&patch).unwrap();
        let parsed: PatternsConfigPatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&patch.packs, &parsed.packs);
        prop_assert_eq!(patch.quick_reject_enabled, parsed.quick_reject_enabled);
        // pack_overrides: compare key sets
        match (&patch.pack_overrides, &parsed.pack_overrides) {
            (Some(a), Some(b)) => {
                prop_assert_eq!(a.len(), b.len());
                for key in a.keys() {
                    prop_assert!(b.contains_key(key), "missing key '{}'", key);
                }
            }
            (None, None) => {}
            _ => prop_assert!(false, "pack_overrides mismatch"),
        }
    }
}

// =========================================================================
// RulesetManifest default
// =========================================================================

#[test]
fn manifest_default_has_version_1() {
    let m = RulesetManifest::default();
    assert_eq!(m.version, 1);
    assert!(m.rulesets.is_empty());
}

#[test]
fn manifest_entry_default_has_empty_fields() {
    let e = RulesetManifestEntry::default();
    assert!(e.name.is_empty());
    assert!(e.path.is_empty());
    assert!(e.description.is_none());
    assert!(e.created_at.is_none());
}

#[test]
fn patterns_config_patch_default_is_all_none() {
    let p = PatternsConfigPatch::default();
    assert!(p.packs.is_none());
    assert!(p.pack_overrides.is_none());
    assert!(p.quick_reject_enabled.is_none());
}
