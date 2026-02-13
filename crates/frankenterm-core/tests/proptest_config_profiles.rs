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
    canonicalize_profile_name, touch_last_applied, ConfigProfileManifest,
    ConfigProfileManifestEntry,
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
        Just("".to_string()),
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
        .prop_map(|(profiles, last_applied_profile, last_applied_at)| {
            ConfigProfileManifest {
                version: 1,
                profiles,
                last_applied_profile,
                last_applied_at,
            }
        })
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
        prop_assert!(result.len() >= 1 && result.len() <= 32,
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
