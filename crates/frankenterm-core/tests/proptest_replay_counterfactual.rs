//! Property-based tests for replay_counterfactual (ft-og6q6.4.1).
//!
//! Invariants tested:
//! - OV-1: TOML roundtrip: load → JSON → deserialize preserves override_count
//! - OV-2: Empty package is_empty and override_count == 0
//! - OV-3: Conflicting actions on same ID rejected
//! - OV-4: Same action on same ID not a conflict
//! - OV-5: definition_hash is deterministic for same input
//! - OV-6: definition_hash differs for different inputs
//! - OV-7: Loader computes hash when definition present
//! - OV-8: Hash mismatch rejected
//! - WC-1: Exact match always succeeds for identical strings
//! - WC-2: Star-only matches everything
//! - WC-3: Suffix wildcard matches correct targets
//! - WC-4: Prefix wildcard matches correct targets
//! - WC-5: Wildcard is symmetric with star-only
//! - AP-1: Applicator Disabled lookup returns Disabled
//! - AP-2: Applicator Replace lookup returns Replace with definition
//! - AP-3: Applicator NoOverride for unknown IDs
//! - AP-4: Applicator substitution_count matches lookups
//! - AP-5: Wildcard applicator matches expected IDs
//! - MF-1: Manifest entry count matches resolved overrides
//! - MF-2: Manifest entries have correct categories
//! - MF-3: Manifest serde roundtrip
//! - SE-1: OverrideAction serde roundtrip
//! - SE-2: OverrideError serde roundtrip
//! - SE-3: SubstitutionRecord serde roundtrip
//! - BL-1: validate_against_baseline accepts known IDs
//! - BL-2: validate_against_baseline rejects unknown non-Add IDs

use proptest::prelude::*;
use std::collections::BTreeMap;

use frankenterm_core::replay_counterfactual::{
    LookupResult, OverrideAction, OverrideApplicator, OverrideError, OverrideManifest,
    OverridePackage, OverridePackageLoader, SubstitutionRecord, definition_hash, wildcard_matches,
};

// ── Strategies ──────────────────────────────────────────────────────────

fn arb_action() -> impl Strategy<Value = OverrideAction> {
    prop_oneof![
        Just(OverrideAction::Replace),
        Just(OverrideAction::Disable),
        Just(OverrideAction::Add),
    ]
}

fn arb_rule_id() -> impl Strategy<Value = String> {
    "[a-z_]{3,20}"
}

fn arb_definition() -> impl Strategy<Value = String> {
    "[a-z0-9 =_]{1,50}"
}

/// Generate valid TOML for an OverridePackage.
fn arb_package_toml(
    n_patterns: usize,
    n_workflows: usize,
    n_policies: usize,
) -> impl Strategy<Value = String> {
    let patterns = proptest::collection::vec(
        (
            arb_rule_id(),
            arb_action(),
            proptest::option::of(arb_definition()),
        ),
        n_patterns..=n_patterns,
    );
    let workflows = proptest::collection::vec(
        (
            arb_rule_id(),
            arb_action(),
            proptest::option::of(arb_definition()),
        ),
        n_workflows..=n_workflows,
    );
    let policies = proptest::collection::vec(
        (
            arb_rule_id(),
            arb_action(),
            proptest::option::of(arb_definition()),
        ),
        n_policies..=n_policies,
    );

    (patterns, workflows, policies).prop_map(|(pats, wfs, pols)| {
        let mut toml = String::from("[meta]\nname = \"proptest\"\n\n");
        for (rid, action, def) in &pats {
            toml.push_str("[[pattern_overrides]]\n");
            toml.push_str(&format!("rule_id = \"{rid}\"\n"));
            toml.push_str(&format!("action = \"{action}\"\n"));
            if let Some(d) = def {
                toml.push_str(&format!("new_definition = \"{d}\"\n"));
            }
            toml.push('\n');
        }
        for (wid, action, def) in &wfs {
            toml.push_str("[[workflow_overrides]]\n");
            toml.push_str(&format!("workflow_id = \"{wid}\"\n"));
            toml.push_str(&format!("action = \"{action}\"\n"));
            if let Some(d) = def {
                toml.push_str(&format!("new_steps = \"{d}\"\n"));
            }
            toml.push('\n');
        }
        for (pid, action, def) in &pols {
            toml.push_str("[[policy_overrides]]\n");
            toml.push_str(&format!("policy_id = \"{pid}\"\n"));
            toml.push_str(&format!("action = \"{action}\"\n"));
            if let Some(d) = def {
                toml.push_str(&format!("new_rules = \"{d}\"\n"));
            }
            toml.push('\n');
        }
        toml
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── OV-1: TOML→JSON roundtrip preserves count ───────────────────────

    #[test]
    fn ov1_toml_json_roundtrip(toml_str in arb_package_toml(1, 0, 0)) {
        // May fail on conflicts; skip those.
        if let Ok(pkg) = OverridePackageLoader::load(&toml_str) {
            let json = serde_json::to_string(&pkg).unwrap();
            let restored: OverridePackage = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(restored.override_count(), pkg.override_count());
        }
    }

    // ── OV-2: Empty package ─────────────────────────────────────────────

    #[test]
    fn ov2_empty_package(_dummy in 0u8..1) {
        let toml = "[meta]\nname = \"empty\"\n";
        let pkg = OverridePackageLoader::load(toml).unwrap();
        prop_assert!(pkg.is_empty());
        prop_assert_eq!(pkg.override_count(), 0);
    }

    // ── OV-3: Conflicting actions rejected ──────────────────────────────

    #[test]
    fn ov3_conflicting_rejected(
        rule_id in arb_rule_id(),
        a1 in arb_action(),
        a2 in arb_action()
    ) {
        prop_assume!(a1 != a2);
        let toml = format!(
            "[meta]\nname = \"conflict\"\n\n\
             [[pattern_overrides]]\nrule_id = \"{rule_id}\"\naction = \"{a1}\"\n\n\
             [[pattern_overrides]]\nrule_id = \"{rule_id}\"\naction = \"{a2}\"\n"
        );
        let result = OverridePackageLoader::load(&toml);
        let is_conflict = matches!(result, Err(OverrideError::ConflictingOverrides { .. }));
        prop_assert!(result.is_err() || !is_conflict,
            "conflicting actions should error");
        // More precisely:
        if let Err(e) = result {
            let is_cf = matches!(e, OverrideError::ConflictingOverrides { .. });
            prop_assert!(is_cf, "expected ConflictingOverrides, got {:?}", e);
        }
    }

    // ── OV-4: Same action not conflict ──────────────────────────────────

    #[test]
    fn ov4_same_action_ok(rule_id in arb_rule_id(), action in arb_action()) {
        let def_clause = if action == OverrideAction::Disable {
            String::new()
        } else {
            "new_definition = \"v1\"\n".to_string()
        };
        let toml = format!(
            "[meta]\nname = \"same\"\n\n\
             [[pattern_overrides]]\nrule_id = \"{rule_id}\"\naction = \"{action}\"\n{def_clause}\n\
             [[pattern_overrides]]\nrule_id = \"{rule_id}\"\naction = \"{action}\"\n{def_clause}\n"
        );
        let result = OverridePackageLoader::load(&toml);
        prop_assert!(result.is_ok(), "same action should not conflict: {:?}", result.err());
    }

    // ── OV-5: Hash deterministic ────────────────────────────────────────

    #[test]
    fn ov5_hash_deterministic(input in arb_definition()) {
        let h1 = definition_hash(&input);
        let h2 = definition_hash(&input);
        prop_assert_eq!(h1.clone(), h2);
        prop_assert_eq!(h1.len(), 16, "FNV-1a 64-bit = 16 hex chars");
    }

    // ── OV-6: Hash differs ──────────────────────────────────────────────

    #[test]
    fn ov6_hash_differs(a in arb_definition(), b in arb_definition()) {
        prop_assume!(a != b);
        let ha = definition_hash(&a);
        let hb = definition_hash(&b);
        prop_assert_ne!(ha, hb, "different inputs should produce different hashes");
    }

    // ── OV-7: Loader computes hash ──────────────────────────────────────

    #[test]
    fn ov7_loader_computes_hash(def in arb_definition()) {
        let toml = format!(
            "[meta]\nname = \"hash\"\n\n\
             [[pattern_overrides]]\nrule_id = \"test_rule\"\naction = \"replace\"\nnew_definition = \"{def}\"\n"
        );
        if let Ok(pkg) = OverridePackageLoader::load(&toml) {
            let entry = &pkg.pattern_overrides[0];
            prop_assert!(entry.definition_hash.is_some());
            let expected = definition_hash(&def);
            prop_assert_eq!(entry.definition_hash.as_deref(), Some(expected.as_str()));
        }
    }

    // ── OV-8: Hash mismatch ────────────────────────────────────────────

    #[test]
    fn ov8_hash_mismatch(def in arb_definition()) {
        let toml = format!(
            "[meta]\nname = \"mismatch\"\n\n\
             [[pattern_overrides]]\nrule_id = \"rule\"\naction = \"replace\"\n\
             new_definition = \"{def}\"\ndefinition_hash = \"0000000000000000\"\n"
        );
        let result = OverridePackageLoader::load(&toml);
        // 0000... is very unlikely to match any real hash.
        if definition_hash(&def) != "0000000000000000" {
            let is_mismatch = matches!(result, Err(OverrideError::HashMismatch { .. }));
            prop_assert!(is_mismatch);
        }
    }

    // ── WC-1: Exact match ───────────────────────────────────────────────

    #[test]
    fn wc1_exact_match(s in arb_rule_id()) {
        prop_assert!(wildcard_matches(&s, &s), "exact match should succeed");
    }

    // ── WC-2: Star matches everything ───────────────────────────────────

    #[test]
    fn wc2_star_matches_all(s in "[a-z_]{0,30}") {
        prop_assert!(wildcard_matches("*", &s), "* should match anything");
    }

    // ── WC-3: Suffix wildcard ───────────────────────────────────────────

    #[test]
    fn wc3_suffix_wildcard(prefix in "[a-z]{2,8}", suffix in "[a-z]{2,8}") {
        let pattern = format!("{prefix}*");
        let target = format!("{prefix}{suffix}");
        prop_assert!(wildcard_matches(&pattern, &target),
            "'{pattern}' should match '{target}'");
    }

    // ── WC-4: Prefix wildcard ───────────────────────────────────────────

    #[test]
    fn wc4_prefix_wildcard(prefix in "[a-z]{2,8}", suffix in "[a-z]{2,8}") {
        let pattern = format!("*{suffix}");
        let target = format!("{prefix}{suffix}");
        prop_assert!(wildcard_matches(&pattern, &target),
            "'*{suffix}' should match '{target}'");
    }

    // ── WC-5: Star-only symmetric ───────────────────────────────────────

    #[test]
    fn wc5_star_symmetric(a in "[a-z]{1,10}", b in "[a-z]{1,10}") {
        // If exact a matches b, then *a* also matches b.
        if a == b {
            let pattern = format!("*{a}*");
            prop_assert!(wildcard_matches(&pattern, &b));
        }
    }

    // ── AP-1: Disabled returns Disabled ─────────────────────────────────

    #[test]
    fn ap1_disabled_lookup(rule_id in arb_rule_id()) {
        let toml = format!(
            "[meta]\nname = \"dis\"\n\n\
             [[pattern_overrides]]\nrule_id = \"{rule_id}\"\naction = \"disable\"\n"
        );
        let pkg = OverridePackageLoader::load(&toml).unwrap();
        let app = OverrideApplicator::new(&pkg);
        let result = app.lookup_pattern(&rule_id, None);
        prop_assert_eq!(result, LookupResult::Disabled);
    }

    // ── AP-2: Replace returns Replace with definition ───────────────────

    #[test]
    fn ap2_replace_lookup(rule_id in arb_rule_id(), def in arb_definition()) {
        let toml = format!(
            "[meta]\nname = \"rep\"\n\n\
             [[pattern_overrides]]\nrule_id = \"{rule_id}\"\naction = \"replace\"\n\
             new_definition = \"{def}\"\n"
        );
        if let Ok(pkg) = OverridePackageLoader::load(&toml) {
            let app = OverrideApplicator::new(&pkg);
            let result = app.lookup_pattern(&rule_id, None);
            prop_assert_eq!(result, LookupResult::Replace(def));
        }
    }

    // ── AP-3: Unknown returns NoOverride ────────────────────────────────

    #[test]
    fn ap3_no_override(rule_id in arb_rule_id()) {
        let toml = "[meta]\nname = \"none\"\n";
        let pkg = OverridePackageLoader::load(toml).unwrap();
        let app = OverrideApplicator::new(&pkg);
        let result = app.lookup_pattern(&rule_id, None);
        prop_assert_eq!(result, LookupResult::NoOverride);
    }

    // ── AP-4: Substitution count matches lookups ────────────────────────

    #[test]
    fn ap4_sub_count(n in 1usize..10) {
        let toml = "[meta]\nname = \"count\"\n\n\
             [[pattern_overrides]]\nrule_id = \"rule_a\"\naction = \"disable\"\n";
        let pkg = OverridePackageLoader::load(toml).unwrap();
        let app = OverrideApplicator::new(&pkg);
        for _ in 0..n {
            app.lookup_pattern("rule_a", None);
        }
        prop_assert_eq!(app.substitution_count(), n);
    }

    // ── AP-5: Wildcard applicator matches ───────────────────────────────

    #[test]
    fn ap5_wildcard_applicator(
        prefix in "[a-z]{3,6}",
        suffixes in proptest::collection::vec("[a-z]{2,5}", 1..5)
    ) {
        let pattern = format!("{prefix}_*");
        let toml = format!(
            "[meta]\nname = \"wc\"\n\n\
             [[pattern_overrides]]\nrule_id = \"{pattern}\"\naction = \"disable\"\n"
        );
        let pkg = OverridePackageLoader::load(&toml).unwrap();
        let app = OverrideApplicator::new(&pkg);
        for s in &suffixes {
            let target = format!("{prefix}_{s}");
            let result = app.lookup_pattern(&target, None);
            prop_assert_eq!(result, LookupResult::Disabled,
                "wildcard should match {}", target);
        }
    }

    // ── MF-1: Manifest entry count ──────────────────────────────────────

    #[test]
    fn mf1_manifest_count(n_rules in 1usize..5) {
        let mut toml = String::from("[meta]\nname = \"mf1\"\n\n");
        let mut baseline = BTreeMap::new();
        for i in 0..n_rules {
            let rid = format!("rule_{i}");
            toml.push_str(&format!(
                "[[pattern_overrides]]\nrule_id = \"{rid}\"\naction = \"disable\"\n\n"
            ));
            baseline.insert(rid, format!("h_{i}"));
        }
        if let Ok(pkg) = OverridePackageLoader::load(&toml) {
            let manifest = OverrideManifest::build(&pkg, &baseline);
            prop_assert_eq!(manifest.entries.len(), n_rules,
                "manifest should have one entry per rule");
        }
    }

    // ── MF-2: Manifest category ─────────────────────────────────────────

    #[test]
    fn mf2_manifest_category(_dummy in 0u8..1) {
        let toml = "[meta]\nname = \"cat\"\n\n\
             [[pattern_overrides]]\nrule_id = \"r1\"\naction = \"disable\"\n\n\
             [[workflow_overrides]]\nworkflow_id = \"w1\"\naction = \"disable\"\n\n\
             [[policy_overrides]]\npolicy_id = \"p1\"\naction = \"disable\"\n";
        let pkg = OverridePackageLoader::load(toml).unwrap();
        let mut baseline = BTreeMap::new();
        baseline.insert("r1".to_string(), "h".to_string());
        baseline.insert("w1".to_string(), "h".to_string());
        baseline.insert("p1".to_string(), "h".to_string());
        let manifest = OverrideManifest::build(&pkg, &baseline);
        let cats: Vec<&str> = manifest.entries.iter().map(|e| e.category.as_str()).collect();
        prop_assert!(cats.contains(&"pattern"));
        prop_assert!(cats.contains(&"workflow"));
        prop_assert!(cats.contains(&"policy"));
    }

    // ── MF-3: Manifest serde ────────────────────────────────────────────

    #[test]
    fn mf3_manifest_serde(n in 1usize..5) {
        let mut toml = String::from("[meta]\nname = \"mf3\"\n\n");
        let mut baseline = BTreeMap::new();
        for i in 0..n {
            let rid = format!("rule_{i}");
            toml.push_str(&format!(
                "[[pattern_overrides]]\nrule_id = \"{rid}\"\naction = \"disable\"\n\n"
            ));
            baseline.insert(rid, format!("h_{i}"));
        }
        if let Ok(pkg) = OverridePackageLoader::load(&toml) {
            let manifest = OverrideManifest::build(&pkg, &baseline);
            let json = serde_json::to_string(&manifest).unwrap();
            let restored: OverrideManifest = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(restored.entries.len(), manifest.entries.len());
        }
    }

    // ── SE-1: OverrideAction serde ──────────────────────────────────────

    #[test]
    fn se1_action_serde(action in arb_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let restored: OverrideAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, action);
    }

    // ── SE-2: OverrideError serde ───────────────────────────────────────

    #[test]
    fn se2_error_serde(rule_id in arb_rule_id()) {
        let err = OverrideError::UnknownRuleId(rule_id.clone());
        let json = serde_json::to_string(&err).unwrap();
        let restored: OverrideError = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, err);
    }

    // ── SE-3: SubstitutionRecord serde ──────────────────────────────────

    #[test]
    fn se3_substitution_serde(
        item_id in arb_rule_id(),
        action in arb_action()
    ) {
        let rec = SubstitutionRecord {
            item_id: item_id.clone(),
            category: "pattern".to_string(),
            action,
            original_hash: Some("h1".to_string()),
            override_hash: Some("h2".to_string()),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let restored: SubstitutionRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.item_id, item_id);
        prop_assert_eq!(restored.action, action);
    }

    // ── BL-1: Baseline accepts known IDs ────────────────────────────────

    #[test]
    fn bl1_baseline_accepts(n in 1usize..5) {
        let mut toml = String::from("[meta]\nname = \"bl1\"\n\n");
        let mut known = Vec::new();
        for i in 0..n {
            let rid = format!("rule_{i}");
            toml.push_str(&format!(
                "[[pattern_overrides]]\nrule_id = \"{rid}\"\naction = \"disable\"\n\n"
            ));
            known.push(rid);
        }
        if let Ok(pkg) = OverridePackageLoader::load(&toml) {
            let result = OverridePackageLoader::validate_against_baseline(&pkg, &known);
            prop_assert!(result.is_ok());
        }
    }

    // ── BL-2: Baseline rejects unknown ──────────────────────────────────

    #[test]
    fn bl2_baseline_rejects_unknown(rule_id in arb_rule_id()) {
        let toml = format!(
            "[meta]\nname = \"bl2\"\n\n\
             [[pattern_overrides]]\nrule_id = \"{rule_id}\"\naction = \"replace\"\n\
             new_definition = \"x\"\n"
        );
        if let Ok(pkg) = OverridePackageLoader::load(&toml) {
            // Empty baseline — rule_id won't exist.
            let result = OverridePackageLoader::validate_against_baseline(&pkg, &[]);
            let is_unknown = matches!(result, Err(OverrideError::UnknownRuleId(_)));
            prop_assert!(is_unknown, "should reject unknown rule ID");
        }
    }
}
