//! Property-based tests for config_profiles module.
//!
//! Verifies config profile management invariants:
//! - canonicalize_profile_name: idempotent, lowercase, trims whitespace
//! - is_valid_profile_name (via canonicalize): character set [a-z0-9_-]{1,32}
//! - ConfigProfileManifest: serde roundtrip, default values
//! - ConfigProfileManifestEntry: serde roundtrip, defaults
//! - touch_last_applied: updates correct entry, creates if missing

use proptest::prelude::*;

use frankenterm_core::config_profiles::{
    ConfigProfileManifest, ConfigProfileManifestEntry, canonicalize_profile_name,
    touch_last_applied,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

/// Valid profile name: [a-z0-9_-]{1,32}
fn arb_valid_name() -> impl Strategy<Value = String> {
    "[a-z0-9_-]{1,32}"
}

/// Invalid names: empty, too long, bad chars
fn arb_invalid_name() -> impl Strategy<Value = String> {
    prop_oneof![
        // Empty after trim
        Just(String::new()),
        Just("   ".to_string()),
        // Too long
        "[a-z]{33,40}",
        // Invalid chars (uppercase kept to trigger lowercase+validation path)
        "[A-Z!@#$%^&*()]{1,10}".prop_filter("must have invalid chars after lowercase", |s| {
            let lower = s.trim().to_lowercase();
            !lower.is_empty()
                && !lower
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        }),
    ]
}

fn arb_manifest_entry() -> impl Strategy<Value = ConfigProfileManifestEntry> {
    (
        arb_valid_name(),
        "[a-z0-9_-]{1,20}\\.toml",
        prop::option::of("[a-z ]{5,30}"),
        prop::option::of(0u64..=10_000_000_000),
        prop::option::of(0u64..=10_000_000_000),
        prop::option::of(0u64..=10_000_000_000),
    )
        .prop_map(
            |(name, path, description, created_at, updated_at, last_applied_at)| {
                ConfigProfileManifestEntry {
                    name,
                    path,
                    description,
                    created_at,
                    updated_at,
                    last_applied_at,
                }
            },
        )
}

fn arb_manifest() -> impl Strategy<Value = ConfigProfileManifest> {
    (
        prop::collection::vec(arb_manifest_entry(), 0..=5),
        prop::option::of(arb_valid_name()),
        prop::option::of(0u64..=10_000_000_000),
    )
        .prop_map(
            |(profiles, last_applied_profile, last_applied_at)| ConfigProfileManifest {
                version: 1,
                profiles,
                last_applied_profile,
                last_applied_at,
            },
        )
}

// ────────────────────────────────────────────────────────────────────
// canonicalize_profile_name: idempotence, lowercase, trim
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Canonicalization is idempotent.
    #[test]
    fn prop_canonicalize_idempotent(name in arb_valid_name()) {
        let first = canonicalize_profile_name(&name).unwrap();
        let second = canonicalize_profile_name(&first).unwrap();
        prop_assert_eq!(&first, &second);
    }

    /// Result is always lowercase.
    #[test]
    fn prop_canonicalize_lowercase(name in arb_valid_name()) {
        let result = canonicalize_profile_name(&name).unwrap();
        let lower = result.to_lowercase();
        prop_assert!(result == lower, "expected lowercase, got '{}'", result);
    }

    /// Valid names succeed.
    #[test]
    fn prop_valid_names_succeed(name in arb_valid_name()) {
        prop_assert!(canonicalize_profile_name(&name).is_ok());
    }

    /// Result contains only [a-z0-9_-].
    #[test]
    fn prop_canonicalize_char_set(name in arb_valid_name()) {
        let result = canonicalize_profile_name(&name).unwrap();
        prop_assert!(
            result.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'),
            "result '{}' has invalid chars", result
        );
    }

    /// Result length is 1..=32.
    #[test]
    fn prop_canonicalize_length(name in arb_valid_name()) {
        let result = canonicalize_profile_name(&name).unwrap();
        prop_assert!(!result.is_empty() && result.len() <= 32,
            "length {} out of range", result.len());
    }

    /// Uppercase input is lowered.
    #[test]
    fn prop_canonicalize_uppercased(name in arb_valid_name()) {
        let upper = name.to_uppercase();
        // Only test if the uppercased version would be valid after lowering
        if let Ok(result) = canonicalize_profile_name(&upper) {
            prop_assert_eq!(result, name.to_lowercase());
        }
    }

    /// Whitespace-padded input is trimmed.
    #[test]
    fn prop_canonicalize_trims(name in arb_valid_name()) {
        let padded = format!("  {}  ", name);
        let result = canonicalize_profile_name(&padded).unwrap();
        prop_assert_eq!(result, name.to_lowercase());
    }
}

// ────────────────────────────────────────────────────────────────────
// canonicalize_profile_name: rejection
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Invalid names are rejected.
    #[test]
    fn prop_invalid_names_rejected(name in arb_invalid_name()) {
        prop_assert!(canonicalize_profile_name(&name).is_err(),
            "expected '{}' to be rejected", name);
    }
}

// ────────────────────────────────────────────────────────────────────
// ConfigProfileManifest: serde roundtrip, defaults
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Manifest serde roundtrip preserves all fields.
    #[test]
    fn prop_manifest_serde_roundtrip(m in arb_manifest()) {
        let json = serde_json::to_string(&m).unwrap();
        let back: ConfigProfileManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.version, m.version);
        prop_assert_eq!(back.profiles.len(), m.profiles.len());
        prop_assert_eq!(back.last_applied_profile, m.last_applied_profile);
        prop_assert_eq!(back.last_applied_at, m.last_applied_at);
    }

    /// Default manifest has version 1 and empty profiles.
    #[test]
    fn prop_manifest_default_valid(_dummy in 0..1u32) {
        let m = ConfigProfileManifest::default();
        prop_assert_eq!(m.version, 1);
        prop_assert!(m.profiles.is_empty());
        prop_assert!(m.last_applied_profile.is_none());
        prop_assert!(m.last_applied_at.is_none());
    }

    /// Empty JSON deserializes with defaults.
    #[test]
    fn prop_manifest_empty_json_defaults(_dummy in 0..1u32) {
        let m: ConfigProfileManifest = serde_json::from_str("{}").unwrap();
        prop_assert_eq!(m.version, 1);
        prop_assert!(m.profiles.is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// ConfigProfileManifestEntry: serde roundtrip, defaults
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Entry serde roundtrip preserves all fields.
    #[test]
    fn prop_entry_serde_roundtrip(e in arb_manifest_entry()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: ConfigProfileManifestEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &e.name);
        prop_assert_eq!(&back.path, &e.path);
        prop_assert_eq!(back.description, e.description);
        prop_assert_eq!(back.created_at, e.created_at);
        prop_assert_eq!(back.updated_at, e.updated_at);
        prop_assert_eq!(back.last_applied_at, e.last_applied_at);
    }

    /// Default entry has empty name and path.
    #[test]
    fn prop_entry_default_empty(_dummy in 0..1u32) {
        let e = ConfigProfileManifestEntry::default();
        prop_assert!(e.name.is_empty());
        prop_assert!(e.path.is_empty());
        prop_assert!(e.description.is_none());
        prop_assert!(e.created_at.is_none());
    }
}

// ────────────────────────────────────────────────────────────────────
// touch_last_applied: updates and creates
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// touch_last_applied sets manifest-level fields.
    #[test]
    fn prop_touch_sets_manifest_fields(
        name in arb_valid_name(),
        ts in 0u64..=10_000_000_000,
    ) {
        let path = format!("{}.toml", name);
        let mut m = ConfigProfileManifest::default();
        touch_last_applied(&mut m, &name, &path, ts);
        prop_assert_eq!(m.last_applied_profile.as_deref(), Some(name.as_str()));
        prop_assert_eq!(m.last_applied_at, Some(ts));
    }

    /// touch_last_applied creates new entry when missing.
    #[test]
    fn prop_touch_creates_entry(
        name in arb_valid_name(),
        ts in 0u64..=10_000_000_000,
    ) {
        let path = format!("{}.toml", name);
        let mut m = ConfigProfileManifest::default();
        touch_last_applied(&mut m, &name, &path, ts);
        prop_assert_eq!(m.profiles.len(), 1);
        prop_assert_eq!(&m.profiles[0].name, &name);
        prop_assert_eq!(&m.profiles[0].path, &path);
        prop_assert_eq!(m.profiles[0].created_at, Some(ts));
        prop_assert_eq!(m.profiles[0].last_applied_at, Some(ts));
    }

    /// touch_last_applied updates existing entry (not duplicating).
    #[test]
    fn prop_touch_updates_existing(
        name in arb_valid_name(),
        ts1 in 0u64..=5_000_000_000u64,
        ts2 in 5_000_000_001u64..=10_000_000_000,
    ) {
        let path = format!("{}.toml", name);
        let mut m = ConfigProfileManifest::default();
        touch_last_applied(&mut m, &name, &path, ts1);
        touch_last_applied(&mut m, &name, &path, ts2);
        // Should still be exactly 1 entry (updated, not duplicated)
        prop_assert_eq!(m.profiles.len(), 1);
        prop_assert_eq!(m.profiles[0].last_applied_at, Some(ts2));
        prop_assert_eq!(m.profiles[0].updated_at, Some(ts2));
        // created_at preserved from first touch
        prop_assert_eq!(m.profiles[0].created_at, Some(ts1));
    }

    /// touch_last_applied with multiple profiles updates only the target.
    #[test]
    fn prop_touch_targets_correct_entry(
        name1 in arb_valid_name(),
        name2 in arb_valid_name().prop_filter("must differ", |n| n.len() > 1),
        ts in 0u64..=10_000_000_000,
    ) {
        // Ensure names differ
        prop_assume!(name1 != name2);

        let mut m = ConfigProfileManifest::default();
        touch_last_applied(&mut m, &name1, &format!("{}.toml", name1), 100);
        touch_last_applied(&mut m, &name2, &format!("{}.toml", name2), 200);

        // Now update name2
        touch_last_applied(&mut m, &name2, &format!("{}.toml", name2), ts);

        // name1 is untouched
        let e1 = m.profiles.iter().find(|e| e.name == name1).unwrap();
        prop_assert_eq!(e1.last_applied_at, Some(100));

        // name2 is updated
        let e2 = m.profiles.iter().find(|e| e.name == name2).unwrap();
        prop_assert_eq!(e2.last_applied_at, Some(ts));
    }
}

// ────────────────────────────────────────────────────────────────────
// ConfigProfileManifest: Clone and Debug
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Manifest Clone preserves all fields.
    #[test]
    fn prop_manifest_clone_preserves(m in arb_manifest()) {
        let cloned = m.clone();
        prop_assert_eq!(cloned.version, m.version);
        prop_assert_eq!(cloned.profiles.len(), m.profiles.len());
        prop_assert_eq!(cloned.last_applied_profile, m.last_applied_profile);
        prop_assert_eq!(cloned.last_applied_at, m.last_applied_at);
        for (a, b) in cloned.profiles.iter().zip(m.profiles.iter()) {
            prop_assert_eq!(&a.name, &b.name);
            prop_assert_eq!(&a.path, &b.path);
        }
    }

    /// Manifest Debug is non-empty and contains type name.
    #[test]
    fn prop_manifest_debug_contains_type(m in arb_manifest()) {
        let debug = format!("{:?}", m);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("ConfigProfileManifest"));
    }

    /// Manifest JSON has expected top-level keys.
    #[test]
    fn prop_manifest_json_structure(m in arb_manifest()) {
        let json = serde_json::to_string(&m).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(obj.contains_key("version"));
        prop_assert!(obj.contains_key("profiles"));
        prop_assert_eq!(obj["version"].as_u64(), Some(1));
    }
}

// ────────────────────────────────────────────────────────────────────
// ConfigProfileManifestEntry: Clone and Debug
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Entry Clone preserves all fields.
    #[test]
    fn prop_entry_clone_preserves(e in arb_manifest_entry()) {
        let cloned = e.clone();
        prop_assert_eq!(&cloned.name, &e.name);
        prop_assert_eq!(&cloned.path, &e.path);
        prop_assert_eq!(cloned.description, e.description);
        prop_assert_eq!(cloned.created_at, e.created_at);
        prop_assert_eq!(cloned.updated_at, e.updated_at);
        prop_assert_eq!(cloned.last_applied_at, e.last_applied_at);
    }

    /// Entry Debug is non-empty and contains the entry name.
    #[test]
    fn prop_entry_debug_contains_name(e in arb_manifest_entry()) {
        let debug = format!("{:?}", e);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains(&e.name),
            "debug '{}' should contain name '{}'", debug, e.name);
    }

    /// Entry JSON has expected field names.
    #[test]
    fn prop_entry_json_fields(e in arb_manifest_entry()) {
        let json = serde_json::to_string(&e).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(obj.contains_key("name"));
        prop_assert!(obj.contains_key("path"));
    }

    /// Entry default has all optional fields as None.
    #[test]
    fn prop_entry_default_all_none(_dummy in 0..1u8) {
        let e = ConfigProfileManifestEntry::default();
        prop_assert!(e.description.is_none());
        prop_assert!(e.created_at.is_none());
        prop_assert!(e.updated_at.is_none());
        prop_assert!(e.last_applied_at.is_none());
    }
}

// ────────────────────────────────────────────────────────────────────
// canonicalize_profile_name: additional properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Canonicalization is deterministic: same input yields same output.
    #[test]
    fn prop_canonicalize_deterministic(name in arb_valid_name()) {
        let r1 = canonicalize_profile_name(&name).unwrap();
        let r2 = canonicalize_profile_name(&name).unwrap();
        prop_assert_eq!(r1, r2);
    }

    /// Canonicalized result length <= input trimmed length (no expansion).
    #[test]
    fn prop_canonicalize_no_expansion(name in arb_valid_name()) {
        let result = canonicalize_profile_name(&name).unwrap();
        prop_assert!(result.len() <= name.trim().len(),
            "result '{}' longer than input '{}'", result, name.trim());
    }

    /// Mixed-case valid chars are properly lowered.
    #[test]
    fn prop_canonicalize_mixed_case(name in "[a-z0-9_-]{1,16}") {
        // Construct mixed case: alternate upper/lower
        let mixed: String = name.chars().enumerate()
            .map(|(i, c)| if i % 2 == 0 { c.to_uppercase().next().unwrap() } else { c })
            .collect();
        let result = canonicalize_profile_name(&mixed).unwrap();
        prop_assert_eq!(result, name.to_lowercase());
    }
}

// ────────────────────────────────────────────────────────────────────
// Manifest serde: skip_serializing_if behavior
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// When last_applied_profile is None, JSON doesn't contain that key.
    #[test]
    fn prop_manifest_skip_none_fields(_dummy in 0..1u8) {
        let m = ConfigProfileManifest::default();
        let json = serde_json::to_string(&m).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(!obj.contains_key("last_applied_profile"),
            "None field should be skipped: {}", json);
        prop_assert!(!obj.contains_key("last_applied_at"),
            "None field should be skipped: {}", json);
    }

    /// When last_applied_profile is Some, JSON contains it.
    #[test]
    fn prop_manifest_includes_some_fields(
        name in arb_valid_name(),
        ts in 0u64..=10_000_000_000,
    ) {
        let m = ConfigProfileManifest {
            version: 1,
            profiles: vec![],
            last_applied_profile: Some(name.clone()),
            last_applied_at: Some(ts),
        };
        let json = serde_json::to_string(&m).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(obj.contains_key("last_applied_profile"));
        prop_assert!(obj.contains_key("last_applied_at"));
        prop_assert_eq!(obj["last_applied_profile"].as_str(), Some(name.as_str()));
    }
}

// ────────────────────────────────────────────────────────────────────
// touch_last_applied: additional edge cases
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Double-touch with same timestamp is idempotent.
    #[test]
    fn prop_touch_same_ts_idempotent(
        name in arb_valid_name(),
        ts in 0u64..=10_000_000_000,
    ) {
        let path = format!("{}.toml", name);
        let mut m = ConfigProfileManifest::default();
        touch_last_applied(&mut m, &name, &path, ts);
        touch_last_applied(&mut m, &name, &path, ts);
        prop_assert_eq!(m.profiles.len(), 1);
        prop_assert_eq!(m.profiles[0].last_applied_at, Some(ts));
        prop_assert_eq!(m.profiles[0].created_at, Some(ts));
    }

    /// Touch preserves description of existing entry.
    #[test]
    fn prop_touch_preserves_description(
        name in arb_valid_name(),
        desc in "[a-z ]{5,20}",
        ts1 in 0u64..=5_000_000_000u64,
        ts2 in 5_000_000_001u64..=10_000_000_000,
    ) {
        let path = format!("{}.toml", name);
        let mut m = ConfigProfileManifest {
            version: 1,
            profiles: vec![ConfigProfileManifestEntry {
                name: name.clone(),
                path: path.clone(),
                description: Some(desc.clone()),
                created_at: Some(ts1),
                updated_at: Some(ts1),
                last_applied_at: Some(ts1),
            }],
            last_applied_profile: None,
            last_applied_at: None,
        };
        touch_last_applied(&mut m, &name, &path, ts2);
        prop_assert_eq!(m.profiles[0].description.as_deref(), Some(desc.as_str()));
    }

    /// Three distinct touches create three entries, update selectively.
    #[test]
    fn prop_touch_three_entries(
        n1 in "[a-e]{3,5}",
        n2 in "[f-j]{3,5}",
        n3 in "[k-o]{3,5}",
    ) {
        prop_assume!(n1 != n2 && n2 != n3 && n1 != n3);
        let mut m = ConfigProfileManifest::default();
        touch_last_applied(&mut m, &n1, &format!("{}.toml", n1), 100);
        touch_last_applied(&mut m, &n2, &format!("{}.toml", n2), 200);
        touch_last_applied(&mut m, &n3, &format!("{}.toml", n3), 300);
        prop_assert_eq!(m.profiles.len(), 3);
        // Last touch updates manifest-level fields
        prop_assert_eq!(m.last_applied_profile.as_deref(), Some(n3.as_str()));
        prop_assert_eq!(m.last_applied_at, Some(300));
    }
}
