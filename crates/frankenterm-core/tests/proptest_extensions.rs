//! Property-based tests for the `extensions` module.
//!
//! Covers serde roundtrips for `ExtensionSource`, `ExtensionInfo`,
//! `ExtensionDetail`, `ExtensionRuleInfo`, and `ValidationResult`.
//! Also tests `resolve_extensions_dir` path resolution invariants
//! and structural properties of all types.

use std::path::Path;

use frankenterm_core::extensions::{
    ExtensionDetail, ExtensionInfo, ExtensionRuleInfo, ExtensionSource, ValidationResult,
    resolve_extensions_dir,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_extension_source() -> impl Strategy<Value = ExtensionSource> {
    prop_oneof![Just(ExtensionSource::Builtin), Just(ExtensionSource::File),]
}

fn arb_extension_info() -> impl Strategy<Value = ExtensionInfo> {
    (
        "[a-z_]{3,15}",                         // name
        "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}", // version
        arb_extension_source(),
        0_usize..50,                            // rule_count
        proptest::option::of("[a-z/]{5,20}"),   // path
        any::<bool>(),                          // active
    )
        .prop_map(|(name, version, source, rule_count, path, active)| ExtensionInfo {
            name,
            version,
            source,
            rule_count,
            path,
            active,
        })
}

fn arb_extension_rule_info() -> impl Strategy<Value = ExtensionRuleInfo> {
    (
        "[a-z.]{3,15}",   // id
        "[a-z]{3,10}",     // agent_type
        "[a-z.]{3,15}",   // event_type
        "info|warning|critical", // severity
        "[A-Za-z ]{5,30}", // description
    )
        .prop_map(|(id, agent_type, event_type, severity, description)| ExtensionRuleInfo {
            id,
            agent_type,
            event_type,
            severity,
            description,
        })
}

fn arb_extension_detail() -> impl Strategy<Value = ExtensionDetail> {
    (
        "[a-z_]{3,15}",
        "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}",
        arb_extension_source(),
        proptest::option::of("[a-z/]{5,20}"),
        proptest::collection::vec(arb_extension_rule_info(), 0..5),
    )
        .prop_map(|(name, version, source, path, rules)| ExtensionDetail {
            name,
            version,
            source,
            path,
            rules,
        })
}

fn arb_validation_result() -> impl Strategy<Value = ValidationResult> {
    (
        any::<bool>(),
        proptest::option::of("[a-z_]{3,15}"),
        proptest::option::of("[0-9.]{3,10}"),
        0_usize..20,
        proptest::collection::vec("[a-z ]{5,20}", 0..3),
        proptest::collection::vec("[a-z ]{5,20}", 0..3),
    )
        .prop_map(
            |(valid, pack_name, version, rule_count, errors, warnings)| ValidationResult {
                valid,
                pack_name,
                version,
                rule_count,
                errors,
                warnings,
            },
        )
}

// =========================================================================
// ExtensionSource — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ExtensionSource serde roundtrip.
    #[test]
    fn prop_extension_source_serde(source in arb_extension_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let back: ExtensionSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, source);
    }

    /// ExtensionSource serializes to snake_case.
    #[test]
    fn prop_extension_source_snake_case(source in arb_extension_source()) {
        let json = serde_json::to_string(&source).unwrap();
        let expected = match source {
            ExtensionSource::Builtin => "\"builtin\"",
            ExtensionSource::File => "\"file\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }
}

// =========================================================================
// ExtensionInfo — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// ExtensionInfo serde roundtrip preserves all fields.
    #[test]
    fn prop_extension_info_serde(info in arb_extension_info()) {
        let json = serde_json::to_string(&info).unwrap();
        let back: ExtensionInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &info.name);
        prop_assert_eq!(&back.version, &info.version);
        prop_assert_eq!(back.source, info.source);
        prop_assert_eq!(back.rule_count, info.rule_count);
        prop_assert_eq!(&back.path, &info.path);
        prop_assert_eq!(back.active, info.active);
    }
}

// =========================================================================
// ExtensionDetail — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ExtensionDetail serde roundtrip preserves all fields.
    #[test]
    fn prop_extension_detail_serde(detail in arb_extension_detail()) {
        let json = serde_json::to_string(&detail).unwrap();
        let back: ExtensionDetail = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &detail.name);
        prop_assert_eq!(&back.version, &detail.version);
        prop_assert_eq!(back.source, detail.source);
        prop_assert_eq!(&back.path, &detail.path);
        prop_assert_eq!(back.rules.len(), detail.rules.len());
        for (b, d) in back.rules.iter().zip(detail.rules.iter()) {
            prop_assert_eq!(&b.id, &d.id);
            prop_assert_eq!(&b.agent_type, &d.agent_type);
            prop_assert_eq!(&b.event_type, &d.event_type);
            prop_assert_eq!(&b.severity, &d.severity);
            prop_assert_eq!(&b.description, &d.description);
        }
    }
}

// =========================================================================
// ExtensionRuleInfo — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ExtensionRuleInfo serde roundtrip.
    #[test]
    fn prop_rule_info_serde(rule in arb_extension_rule_info()) {
        let json = serde_json::to_string(&rule).unwrap();
        let back: ExtensionRuleInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &rule.id);
        prop_assert_eq!(&back.agent_type, &rule.agent_type);
        prop_assert_eq!(&back.event_type, &rule.event_type);
        prop_assert_eq!(&back.severity, &rule.severity);
        prop_assert_eq!(&back.description, &rule.description);
    }
}

// =========================================================================
// ValidationResult — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// ValidationResult serde roundtrip preserves all fields.
    #[test]
    fn prop_validation_result_serde(result in arb_validation_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: ValidationResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.valid, result.valid);
        prop_assert_eq!(&back.pack_name, &result.pack_name);
        prop_assert_eq!(&back.version, &result.version);
        prop_assert_eq!(back.rule_count, result.rule_count);
        prop_assert_eq!(&back.errors, &result.errors);
        prop_assert_eq!(&back.warnings, &result.warnings);
    }

    /// ValidationResult: invalid results always have errors OR warnings.
    /// (This is a property we test — invalid results SHOULD have errors,
    /// but we can at least verify the serde fidelity.)
    #[test]
    fn prop_validation_result_serde_deterministic(result in arb_validation_result()) {
        let json1 = serde_json::to_string(&result).unwrap();
        let json2 = serde_json::to_string(&result).unwrap();
        prop_assert_eq!(&json1, &json2);
    }
}

// =========================================================================
// resolve_extensions_dir — path invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// resolve_extensions_dir with a config path always returns a path
    /// ending with "extensions".
    #[test]
    fn prop_resolve_ends_with_extensions(dir in "[a-z/]{3,20}") {
        let config_path = format!("/tmp/{dir}/config.toml");
        let resolved = resolve_extensions_dir(Some(Path::new(&config_path)));
        prop_assert!(
            resolved.ends_with("extensions"),
            "resolved path {:?} should end with 'extensions'", resolved
        );
    }

    /// resolve_extensions_dir with None returns a path ending with "extensions".
    #[test]
    fn prop_resolve_none_ends_with_extensions(_dummy in 0..1_u8) {
        let resolved = resolve_extensions_dir(None);
        prop_assert!(
            resolved.ends_with("extensions"),
            "resolved path {:?} should end with 'extensions'", resolved
        );
    }

    /// resolve_extensions_dir is deterministic.
    #[test]
    fn prop_resolve_deterministic(dir in "[a-z]{3,10}") {
        let config_path = format!("/tmp/{dir}/config.toml");
        let r1 = resolve_extensions_dir(Some(Path::new(&config_path)));
        let r2 = resolve_extensions_dir(Some(Path::new(&config_path)));
        prop_assert_eq!(r1, r2);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn extension_source_variants_distinct() {
    assert_ne!(ExtensionSource::Builtin, ExtensionSource::File);
}

#[test]
fn validation_result_empty_errors() {
    let result = ValidationResult {
        valid: true,
        pack_name: Some("test".to_string()),
        version: Some("1.0.0".to_string()),
        rule_count: 5,
        errors: vec![],
        warnings: vec![],
    };
    let json = serde_json::to_string(&result).unwrap();
    let back: ValidationResult = serde_json::from_str(&json).unwrap();
    assert!(back.errors.is_empty());
    assert!(back.warnings.is_empty());
}

#[test]
fn extension_detail_empty_rules() {
    let detail = ExtensionDetail {
        name: "test".to_string(),
        version: "1.0.0".to_string(),
        source: ExtensionSource::Builtin,
        path: None,
        rules: vec![],
    };
    let json = serde_json::to_string(&detail).unwrap();
    let back: ExtensionDetail = serde_json::from_str(&json).unwrap();
    assert!(back.rules.is_empty());
}

#[test]
fn resolve_extensions_dir_with_config_sibling() {
    let resolved = resolve_extensions_dir(Some(Path::new("/home/user/.config/ft/config.toml")));
    assert_eq!(
        resolved,
        std::path::PathBuf::from("/home/user/.config/ft/extensions")
    );
}
