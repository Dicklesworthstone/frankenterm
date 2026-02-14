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

// =========================================================================
// PatternsConfigPatch: Clone and Debug
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Clone preserves all fields.
    #[test]
    fn prop_patch_clone_preserves(patch in arb_patterns_config_patch()) {
        let cloned = patch.clone();
        prop_assert_eq!(&cloned.packs, &patch.packs);
        prop_assert_eq!(cloned.quick_reject_enabled, patch.quick_reject_enabled);
        match (&cloned.pack_overrides, &patch.pack_overrides) {
            (Some(a), Some(b)) => prop_assert_eq!(a.len(), b.len()),
            (None, None) => {}
            _ => prop_assert!(false, "pack_overrides mismatch after clone"),
        }
    }

    /// Debug is non-empty and contains type name.
    #[test]
    fn prop_patch_debug_non_empty(patch in arb_patterns_config_patch()) {
        let debug = format!("{:?}", patch);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("PatternsConfigPatch"));
    }

    /// PatternsConfig Clone preserves all fields.
    #[test]
    fn prop_patterns_config_clone(base in arb_patterns_config()) {
        let cloned = base.clone();
        prop_assert_eq!(&cloned.packs, &base.packs);
        prop_assert_eq!(cloned.quick_reject_enabled, base.quick_reject_enabled);
        prop_assert_eq!(cloned.user_packs_enabled, base.user_packs_enabled);
        prop_assert_eq!(&cloned.user_packs_dir, &base.user_packs_dir);
        prop_assert_eq!(cloned.pack_overrides.len(), base.pack_overrides.len());
    }
}

// =========================================================================
// PatternsConfigPatch::apply_to — preserves unrelated fields
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// apply_to always preserves user_packs_enabled and user_packs_dir.
    #[test]
    fn prop_apply_preserves_user_packs(
        base in arb_patterns_config(),
        patch in arb_patterns_config_patch(),
    ) {
        let result = patch.apply_to(&base);
        prop_assert_eq!(result.user_packs_enabled, base.user_packs_enabled);
        prop_assert_eq!(&result.user_packs_dir, &base.user_packs_dir);
    }

    /// apply_to with only packs set doesn't change quick_reject_enabled.
    #[test]
    fn prop_apply_packs_only_preserves_quick_reject(
        base in arb_patterns_config(),
        new_packs in proptest::collection::vec(arb_pack_name(), 0..5),
    ) {
        let patch = PatternsConfigPatch {
            packs: Some(new_packs),
            pack_overrides: None,
            quick_reject_enabled: None,
        };
        let result = patch.apply_to(&base);
        prop_assert_eq!(result.quick_reject_enabled, base.quick_reject_enabled);
    }

    /// apply_to with only quick_reject set doesn't change packs.
    #[test]
    fn prop_apply_qr_only_preserves_packs(
        base in arb_patterns_config(),
        qr in any::<bool>(),
    ) {
        let patch = PatternsConfigPatch {
            packs: None,
            pack_overrides: None,
            quick_reject_enabled: Some(qr),
        };
        let result = patch.apply_to(&base);
        prop_assert_eq!(&result.packs, &base.packs);
    }

    /// Applying a full patch (all Some) replaces packs, quick_reject, and merges overrides.
    #[test]
    fn prop_apply_full_patch(
        base in arb_patterns_config(),
        new_packs in proptest::collection::vec(arb_pack_name(), 0..5),
        new_overrides in proptest::collection::hash_map(arb_pack_name(), arb_pack_override(), 0..3),
        new_qr in any::<bool>(),
    ) {
        let patch = PatternsConfigPatch {
            packs: Some(new_packs.clone()),
            pack_overrides: Some(new_overrides.clone()),
            quick_reject_enabled: Some(new_qr),
        };
        let result = patch.apply_to(&base);
        prop_assert_eq!(&result.packs, &new_packs);
        prop_assert_eq!(result.quick_reject_enabled, new_qr);
        // All overlay keys should be present
        for key in new_overrides.keys() {
            prop_assert!(result.pack_overrides.contains_key(key),
                "overlay key '{}' missing", key);
        }
    }
}

// =========================================================================
// RulesetManifest: Clone and Debug
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// RulesetManifest Clone preserves version and entry count.
    #[test]
    fn prop_manifest_clone_preserves(
        entries in proptest::collection::vec(arb_ruleset_manifest_entry(), 0..5),
    ) {
        let manifest = RulesetManifest { version: 1, rulesets: entries };
        let cloned = manifest.clone();
        prop_assert_eq!(cloned.version, manifest.version);
        prop_assert_eq!(cloned.rulesets.len(), manifest.rulesets.len());
        for (a, b) in cloned.rulesets.iter().zip(manifest.rulesets.iter()) {
            prop_assert_eq!(&a.name, &b.name);
            prop_assert_eq!(&a.path, &b.path);
        }
    }

    /// RulesetManifest Debug is non-empty.
    #[test]
    fn prop_manifest_debug_non_empty(
        entries in proptest::collection::vec(arb_ruleset_manifest_entry(), 0..3),
    ) {
        let manifest = RulesetManifest { version: 1, rulesets: entries };
        let debug = format!("{:?}", manifest);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("RulesetManifest"));
    }

    /// RulesetManifest JSON has expected top-level keys.
    #[test]
    fn prop_manifest_json_structure(
        entries in proptest::collection::vec(arb_ruleset_manifest_entry(), 0..5),
    ) {
        let manifest = RulesetManifest { version: 1, rulesets: entries };
        let json = serde_json::to_string(&manifest).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(obj.contains_key("version"));
        prop_assert!(obj.contains_key("rulesets"));
        prop_assert_eq!(obj["version"].as_u64(), Some(1));
    }
}

// =========================================================================
// RulesetManifestEntry: Clone and Debug
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Entry Clone preserves all fields.
    #[test]
    fn prop_entry_clone_preserves(entry in arb_ruleset_manifest_entry()) {
        let cloned = entry.clone();
        prop_assert_eq!(&cloned.name, &entry.name);
        prop_assert_eq!(&cloned.path, &entry.path);
        prop_assert_eq!(cloned.description, entry.description);
        prop_assert_eq!(cloned.created_at, entry.created_at);
        prop_assert_eq!(cloned.updated_at, entry.updated_at);
        prop_assert_eq!(cloned.last_applied_at, entry.last_applied_at);
    }

    /// Entry Debug is non-empty and contains entry name.
    #[test]
    fn prop_entry_debug_contains_name(entry in arb_ruleset_manifest_entry()) {
        let debug = format!("{:?}", entry);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains(&entry.name),
            "debug '{}' should contain name '{}'", debug, entry.name);
    }

    /// Entry JSON has expected field names.
    #[test]
    fn prop_entry_json_fields(entry in arb_ruleset_manifest_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(obj.contains_key("name"));
        prop_assert!(obj.contains_key("path"));
    }
}

// =========================================================================
// RulesetProfileFile: Clone, Debug, defaults
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// RulesetProfileFile Clone preserves all fields.
    #[test]
    fn prop_profile_file_clone_preserves(
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
        let cloned = profile.clone();
        prop_assert_eq!(&cloned.name, &profile.name);
        prop_assert_eq!(&cloned.description, &profile.description);
        prop_assert_eq!(&cloned.inherits, &profile.inherits);
    }

    /// RulesetProfileFile Debug is non-empty.
    #[test]
    fn prop_profile_file_debug_non_empty(
        name in "[a-z]{3,10}",
    ) {
        let profile = RulesetProfileFile {
            name,
            ..Default::default()
        };
        let debug = format!("{:?}", profile);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("RulesetProfileFile"));
    }

    /// Default RulesetProfileFile has empty name and no inherits.
    #[test]
    fn prop_profile_file_default_empty(_dummy in 0..1u8) {
        let profile = RulesetProfileFile::default();
        prop_assert!(profile.name.is_empty());
        prop_assert!(profile.description.is_none());
        prop_assert!(profile.inherits.is_none());
        prop_assert!(profile.patterns.packs.is_none());
    }
}

// =========================================================================
// PackOverride: Clone, Debug, serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// PackOverride Clone preserves fields.
    #[test]
    fn prop_pack_override_clone(po in arb_pack_override()) {
        let cloned = po.clone();
        prop_assert_eq!(&cloned.disabled_rules, &po.disabled_rules);
        prop_assert_eq!(&cloned.severity_overrides, &po.severity_overrides);
    }

    /// PackOverride Debug is non-empty.
    #[test]
    fn prop_pack_override_debug(po in arb_pack_override()) {
        let debug = format!("{:?}", po);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("PackOverride"));
    }

    /// PackOverride serde roundtrip preserves disabled_rules.
    #[test]
    fn prop_pack_override_serde_roundtrip(po in arb_pack_override()) {
        let json = serde_json::to_string(&po).unwrap();
        let back: PackOverride = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.disabled_rules, &po.disabled_rules);
        prop_assert_eq!(&back.severity_overrides, &po.severity_overrides);
    }

    /// Default PackOverride has empty fields.
    #[test]
    fn prop_pack_override_default_empty(_dummy in 0..1u8) {
        let po = PackOverride::default();
        prop_assert!(po.disabled_rules.is_empty());
        prop_assert!(po.severity_overrides.is_empty());
        prop_assert!(po.extra.is_empty());
    }
}

// =========================================================================
// touch_last_applied: additional edge cases
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Touching with same timestamp twice doesn't create duplicates.
    #[test]
    fn prop_touch_same_ts_no_dup(
        name in "[a-z]{3,10}",
        path in "[a-z]{3,10}\\.toml",
        ts in 1u64..2_000_000,
    ) {
        let mut manifest = RulesetManifest::default();
        frankenterm_core::rulesets::touch_last_applied(&mut manifest, &name, &path, ts);
        frankenterm_core::rulesets::touch_last_applied(&mut manifest, &name, &path, ts);
        prop_assert_eq!(manifest.rulesets.len(), 1);
        prop_assert_eq!(manifest.rulesets[0].last_applied_at, Some(ts));
    }

    /// Touching multiple distinct entries preserves all.
    #[test]
    fn prop_touch_multiple_entries(
        n1 in "[a-z]{3,5}",
        n2 in "[f-z]{3,5}",
        n3 in "[a-e]{3,5}",
    ) {
        // Ensure distinct names
        prop_assume!(n1 != n2 && n2 != n3 && n1 != n3);
        let mut manifest = RulesetManifest::default();
        frankenterm_core::rulesets::touch_last_applied(&mut manifest, &n1, &format!("{}.toml", n1), 100);
        frankenterm_core::rulesets::touch_last_applied(&mut manifest, &n2, &format!("{}.toml", n2), 200);
        frankenterm_core::rulesets::touch_last_applied(&mut manifest, &n3, &format!("{}.toml", n3), 300);
        prop_assert_eq!(manifest.rulesets.len(), 3);
    }
}
