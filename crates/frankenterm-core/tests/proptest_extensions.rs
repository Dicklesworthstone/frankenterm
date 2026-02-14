//! Property-based tests for the `extensions` module.
//!
//! Covers serde roundtrips for `ExtensionSource`, `ExtensionInfo`,
//! `ExtensionDetail`, `ExtensionRuleInfo`, `ValidationResult`,
//! `SandboxCapabilities`, `FileAccessScope`, `CapabilityLevel`,
//! `ExtensionManifest`, and `SandboxViolation`.
//! Also tests `resolve_extensions_dir` path resolution invariants,
//! `CapabilityLevel::to_capabilities` hierarchy properties, and
//! `ExtensionManifest::effective_capabilities` override logic.

use std::path::Path;

use frankenterm_core::extensions::{
    CapabilityLevel, ExtensionDetail, ExtensionInfo, ExtensionManifest, ExtensionRuleInfo,
    ExtensionSource, FileAccessScope, SandboxCapabilities, SandboxViolation, ValidationResult,
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
        0_usize..50,                          // rule_count
        proptest::option::of("[a-z/]{5,20}"), // path
        any::<bool>(),                        // active
    )
        .prop_map(
            |(name, version, source, rule_count, path, active)| ExtensionInfo {
                name,
                version,
                source,
                rule_count,
                path,
                active,
            },
        )
}

fn arb_extension_rule_info() -> impl Strategy<Value = ExtensionRuleInfo> {
    (
        "[a-z.]{3,15}",          // id
        "[a-z]{3,10}",           // agent_type
        "[a-z.]{3,15}",          // event_type
        "info|warning|critical", // severity
        "[A-Za-z ]{5,30}",       // description
    )
        .prop_map(
            |(id, agent_type, event_type, severity, description)| ExtensionRuleInfo {
                id,
                agent_type,
                event_type,
                severity,
                description,
            },
        )
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

fn arb_file_access_scope() -> impl Strategy<Value = FileAccessScope> {
    prop_oneof![
        Just(FileAccessScope::None),
        Just(FileAccessScope::OwnDataReadOnly),
        Just(FileAccessScope::OwnDataReadWrite),
        Just(FileAccessScope::ConfigReadOnly),
    ]
}

fn arb_capability_level() -> impl Strategy<Value = CapabilityLevel> {
    prop_oneof![
        Just(CapabilityLevel::ReadOnly),
        Just(CapabilityLevel::ReadNotify),
        Just(CapabilityLevel::Integration),
        Just(CapabilityLevel::Full),
    ]
}

fn arb_sandbox_capabilities() -> impl Strategy<Value = SandboxCapabilities> {
    (
        any::<bool>(),           // read_pane_output
        any::<bool>(),           // send_notifications
        any::<bool>(),           // http_requests
        arb_file_access_scope(),
        any::<bool>(),           // invoke_workflows
        any::<bool>(),           // send_text
    )
        .prop_map(
            |(rpo, sn, hr, fa, iw, st)| SandboxCapabilities {
                read_pane_output: rpo,
                send_notifications: sn,
                http_requests: hr,
                file_access: fa,
                invoke_workflows: iw,
                send_text: st,
            },
        )
}

fn arb_extension_manifest() -> impl Strategy<Value = ExtensionManifest> {
    (
        "[a-z_]{3,15}",
        "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}",
        arb_sandbox_capabilities(),
        proptest::option::of(arb_capability_level()),
        proptest::collection::vec("[a-z.]{3,20}", 0..5),
        1_u64..100_000_000,
        100_u64..60_000,
    )
        .prop_map(
            |(name, version, capabilities, capability_level, allowed_hosts, max_mem, max_exec)| {
                ExtensionManifest {
                    name,
                    version,
                    capabilities,
                    capability_level,
                    allowed_hosts,
                    max_memory_bytes: max_mem,
                    max_execution_ms: max_exec,
                }
            },
        )
}

fn arb_sandbox_violation() -> impl Strategy<Value = SandboxViolation> {
    (
        "[a-z_]{3,15}",
        "[a-z_]{5,20}",
        "[A-Za-z ]{10,40}",
    )
        .prop_map(|(extension_name, capability, message)| SandboxViolation {
            extension_name,
            capability,
            message,
        })
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

    /// ValidationResult serde is deterministic.
    #[test]
    fn prop_validation_result_serde_deterministic(result in arb_validation_result()) {
        let json1 = serde_json::to_string(&result).unwrap();
        let json2 = serde_json::to_string(&result).unwrap();
        prop_assert_eq!(&json1, &json2);
    }
}

// =========================================================================
// FileAccessScope — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// FileAccessScope serde roundtrip.
    #[test]
    fn prop_file_access_scope_serde(scope in arb_file_access_scope()) {
        let json = serde_json::to_string(&scope).unwrap();
        let back: FileAccessScope = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, scope);
    }

    /// FileAccessScope serializes to snake_case.
    #[test]
    fn prop_file_access_scope_snake_case(scope in arb_file_access_scope()) {
        let json = serde_json::to_string(&scope).unwrap();
        let expected = match scope {
            FileAccessScope::None => "\"none\"",
            FileAccessScope::OwnDataReadOnly => "\"own_data_read_only\"",
            FileAccessScope::OwnDataReadWrite => "\"own_data_read_write\"",
            FileAccessScope::ConfigReadOnly => "\"config_read_only\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }
}

// =========================================================================
// CapabilityLevel — serde roundtrip + hierarchy
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// CapabilityLevel serde roundtrip.
    #[test]
    fn prop_capability_level_serde(level in arb_capability_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let back: CapabilityLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, level);
    }

    /// CapabilityLevel serializes to snake_case.
    #[test]
    fn prop_capability_level_snake_case(level in arb_capability_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let expected = match level {
            CapabilityLevel::ReadOnly => "\"read_only\"",
            CapabilityLevel::ReadNotify => "\"read_notify\"",
            CapabilityLevel::Integration => "\"integration\"",
            CapabilityLevel::Full => "\"full\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// All capability levels include read_pane_output.
    #[test]
    fn prop_all_levels_read_pane(level in arb_capability_level()) {
        let caps = level.to_capabilities();
        prop_assert!(caps.read_pane_output,
            "all capability levels should include read_pane_output");
    }

    /// Higher levels are supersets: Full includes everything Integration does.
    #[test]
    fn prop_full_superset_of_integration(_dummy in 0..1_u8) {
        let full = CapabilityLevel::Full.to_capabilities();
        let integ = CapabilityLevel::Integration.to_capabilities();
        prop_assert!(full.read_pane_output >= integ.read_pane_output);
        prop_assert!(full.send_notifications >= integ.send_notifications);
        prop_assert!(full.http_requests >= integ.http_requests);
        prop_assert!(full.invoke_workflows >= integ.invoke_workflows);
    }

    /// ReadOnly has minimal capabilities: no notifications, no HTTP, no send_text.
    #[test]
    fn prop_read_only_minimal(_dummy in 0..1_u8) {
        let caps = CapabilityLevel::ReadOnly.to_capabilities();
        prop_assert!(caps.read_pane_output);
        prop_assert!(!caps.send_notifications);
        prop_assert!(!caps.http_requests);
        prop_assert!(!caps.invoke_workflows);
        prop_assert!(!caps.send_text);
    }
}

// =========================================================================
// SandboxCapabilities — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// SandboxCapabilities serde roundtrip preserves all fields.
    #[test]
    fn prop_sandbox_capabilities_serde(caps in arb_sandbox_capabilities()) {
        let json = serde_json::to_string(&caps).unwrap();
        let back: SandboxCapabilities = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, caps);
    }

    /// Default SandboxCapabilities has read_pane_output=true, rest false/None.
    #[test]
    fn prop_sandbox_capabilities_default(_dummy in 0..1_u8) {
        let caps = SandboxCapabilities::default();
        prop_assert!(caps.read_pane_output);
        prop_assert!(!caps.send_notifications);
        prop_assert!(!caps.http_requests);
        prop_assert_eq!(caps.file_access, FileAccessScope::None);
        prop_assert!(!caps.invoke_workflows);
        prop_assert!(!caps.send_text);
    }
}

// =========================================================================
// ExtensionManifest — serde roundtrip + effective_capabilities
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ExtensionManifest serde roundtrip preserves all fields.
    #[test]
    fn prop_manifest_serde(manifest in arb_extension_manifest()) {
        let json = serde_json::to_string(&manifest).unwrap();
        let back: ExtensionManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &manifest.name);
        prop_assert_eq!(&back.version, &manifest.version);
        prop_assert_eq!(back.capabilities, manifest.capabilities);
        prop_assert_eq!(back.capability_level, manifest.capability_level);
        prop_assert_eq!(&back.allowed_hosts, &manifest.allowed_hosts);
        prop_assert_eq!(back.max_memory_bytes, manifest.max_memory_bytes);
        prop_assert_eq!(back.max_execution_ms, manifest.max_execution_ms);
    }

    /// When capability_level is Some, effective_capabilities uses it.
    #[test]
    fn prop_manifest_level_overrides(
        level in arb_capability_level(),
        caps in arb_sandbox_capabilities(),
    ) {
        let manifest = ExtensionManifest {
            capability_level: Some(level),
            capabilities: caps,
            ..ExtensionManifest::default()
        };
        let effective = manifest.effective_capabilities();
        let expected = level.to_capabilities();
        prop_assert_eq!(effective, expected,
            "when capability_level is set, effective_capabilities should use it");
    }

    /// When capability_level is None, effective_capabilities uses capabilities field.
    #[test]
    fn prop_manifest_no_level_uses_caps(caps in arb_sandbox_capabilities()) {
        let manifest = ExtensionManifest {
            capability_level: None,
            capabilities: caps.clone(),
            ..ExtensionManifest::default()
        };
        let effective = manifest.effective_capabilities();
        prop_assert_eq!(effective, caps,
            "when capability_level is None, effective_capabilities should return capabilities");
    }
}

// =========================================================================
// SandboxViolation — serde roundtrip + Display
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// SandboxViolation serde roundtrip.
    #[test]
    fn prop_sandbox_violation_serde(violation in arb_sandbox_violation()) {
        let json = serde_json::to_string(&violation).unwrap();
        let back: SandboxViolation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, violation);
    }

    /// SandboxViolation Display contains extension_name, capability, and message.
    #[test]
    fn prop_sandbox_violation_display(violation in arb_sandbox_violation()) {
        let display = violation.to_string();
        prop_assert!(display.contains(&violation.extension_name),
            "Display should contain extension_name '{}': {}",
            violation.extension_name, display);
        prop_assert!(display.contains(&violation.capability),
            "Display should contain capability '{}': {}",
            violation.capability, display);
        prop_assert!(display.contains(&violation.message),
            "Display should contain message '{}': {}",
            violation.message, display);
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

#[test]
fn capability_level_hierarchy() {
    let ro = CapabilityLevel::ReadOnly.to_capabilities();
    let rn = CapabilityLevel::ReadNotify.to_capabilities();
    let int = CapabilityLevel::Integration.to_capabilities();
    let full = CapabilityLevel::Full.to_capabilities();

    // ReadOnly: only read
    assert!(ro.read_pane_output);
    assert!(!ro.send_notifications);

    // ReadNotify: read + notify
    assert!(rn.read_pane_output);
    assert!(rn.send_notifications);
    assert!(!rn.http_requests);

    // Integration: read + notify + http + workflows
    assert!(int.http_requests);
    assert!(int.invoke_workflows);
    assert!(!int.send_text);

    // Full: everything
    assert!(full.send_text);
}
