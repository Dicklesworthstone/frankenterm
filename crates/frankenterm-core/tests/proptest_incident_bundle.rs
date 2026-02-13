//! Property-based tests for the incident_bundle module.
//!
//! Tests cover: BundleFormatVersion (serde roundtrip, check_compatibility reflexivity,
//! is_compatible_with reflexivity/symmetry, Display format, compatibility semantics),
//! PrivacyBudget (serde roundtrip, validate invariants, would_exceed_total saturation,
//! truncate_file_content UTF-8 safety and length bounds, truncate_excerpt character bounds),
//! BundleFile (serde roundtrip, filenames non-empty, required files apply to all kinds,
//! expected_for_kind superset of required), BundleReplayMode (serde roundtrip, contract
//! structural invariants), PrivacyBudgetSummary (tier detection for known budgets),
//! and generate_bundle_readme (output contains required sections).

use proptest::prelude::*;

use frankenterm_core::crash::IncidentKind;
use frankenterm_core::incident_bundle::{
    BundleFile, BundleFileEntry, BundleFormatVersion, BundleReplayMode, IncidentManifest,
    PrivacyBudget, PrivacyBudgetSummary, RedactionSummary, bundle_dirname, generate_bundle_readme,
};

// ============================================================================
// Strategies
// ============================================================================

/// Arbitrary BundleFormatVersion.
fn arb_format_version() -> impl Strategy<Value = BundleFormatVersion> {
    (any::<u16>(), any::<u16>()).prop_map(|(major, minor)| BundleFormatVersion { major, minor })
}

/// Arbitrary IncidentKind.
fn arb_incident_kind() -> impl Strategy<Value = IncidentKind> {
    prop_oneof![Just(IncidentKind::Crash), Just(IncidentKind::Manual),]
}

/// Arbitrary BundleFile variant.
fn arb_bundle_file() -> impl Strategy<Value = BundleFile> {
    prop_oneof![
        Just(BundleFile::Manifest),
        Just(BundleFile::Readme),
        Just(BundleFile::RedactionReport),
        Just(BundleFile::CrashReport),
        Just(BundleFile::CrashManifest),
        Just(BundleFile::HealthSnapshot),
        Just(BundleFile::ConfigSummary),
        Just(BundleFile::DbMetadata),
        Just(BundleFile::RecentEvents),
    ]
}

/// Arbitrary BundleReplayMode variant.
fn arb_replay_mode() -> impl Strategy<Value = BundleReplayMode> {
    prop_oneof![
        Just(BundleReplayMode::Policy),
        Just(BundleReplayMode::Rules),
        Just(BundleReplayMode::WorkflowTrace),
    ]
}

/// Arbitrary PrivacyBudget that is always valid (file <= total, all > 0).
fn arb_valid_budget() -> impl Strategy<Value = PrivacyBudget> {
    (
        1usize..=10_000_000, // max_total_bytes
        1usize..=10_000,     // max_lines_per_log
        1usize..=1_000,      // max_output_excerpt_len
        1usize..=1_000_000,  // max_backtrace_len
        any::<bool>(),       // include_db_metadata
        any::<bool>(),       // include_recent_events
        0usize..=1_000,      // max_recent_events
    )
        .prop_flat_map(
            |(total, lines, excerpt, backtrace, db_meta, events, max_events)| {
                // max_bytes_per_file must be 1..=total for validity
                (1usize..=total).prop_map(move |per_file| PrivacyBudget {
                    max_bytes_per_file: per_file,
                    max_total_bytes: total,
                    max_lines_per_log: lines,
                    max_output_excerpt_len: excerpt,
                    max_backtrace_len: backtrace,
                    include_db_metadata: db_meta,
                    include_recent_events: events,
                    max_recent_events: max_events,
                })
            },
        )
}

/// Arbitrary ASCII string for content testing.
fn arb_ascii_content() -> impl Strategy<Value = String> {
    prop::collection::vec(0x20u8..=0x7E, 0..500).prop_map(|bytes| String::from_utf8(bytes).unwrap())
}

/// Arbitrary multi-byte UTF-8 string (includes CJK, emoji, etc.).
fn arb_utf8_content() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            (0x20u32..=0x7E).prop_map(|c| char::from_u32(c).unwrap()),
            (0x4E00u32..=0x4E20).prop_map(|c| char::from_u32(c).unwrap()), // CJK
            Just('\u{1F600}'),                                             // emoji
        ],
        0..200,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

/// Arbitrary timestamp-like string for bundle_dirname.
fn arb_timestamp_str() -> impl Strategy<Value = String> {
    "[0-9]{8}_[0-9]{6}"
}

/// Arbitrary BundleFileEntry.
fn arb_file_entry() -> impl Strategy<Value = BundleFileEntry> {
    (
        "[a-z_]{1,20}\\.json".prop_map(String::from),
        any::<u64>(),
        any::<bool>(),
    )
        .prop_map(|(name, size_bytes, redacted)| BundleFileEntry {
            name,
            size_bytes,
            redacted,
        })
}

/// Arbitrary RedactionSummary.
fn arb_redaction_summary() -> impl Strategy<Value = RedactionSummary> {
    (any::<usize>(), any::<usize>()).prop_map(|(total, files)| RedactionSummary {
        total_redactions: total,
        files_with_redactions: files,
    })
}

/// Arbitrary IncidentManifest.
fn arb_manifest() -> impl Strategy<Value = IncidentManifest> {
    (
        arb_format_version(),
        "[a-z0-9.\\-]{1,20}".prop_map(String::from),
        arb_incident_kind(),
        "[0-9T:Z\\-]{10,30}".prop_map(String::from),
        prop::collection::vec(arb_file_entry(), 0..5),
        arb_valid_budget(),
        any::<u64>(),
        prop::option::of(arb_redaction_summary()),
    )
        .prop_map(
            |(version, wa_ver, kind, created, files, budget, total_bytes, redaction)| {
                IncidentManifest {
                    format_version: version,
                    wa_version: wa_ver,
                    kind,
                    created_at: created,
                    files,
                    privacy_budget: PrivacyBudgetSummary::from(&budget),
                    total_size_bytes: total_bytes,
                    redaction_summary: redaction,
                }
            },
        )
}

// ============================================================================
// BundleFormatVersion properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Serde roundtrip for BundleFormatVersion.
    #[test]
    fn prop_format_version_serde_roundtrip(v in arb_format_version()) {
        let json = serde_json::to_string(&v).unwrap();
        let parsed: BundleFormatVersion = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, v);
    }

    /// check_compatibility with self always succeeds.
    #[test]
    fn prop_format_version_check_compat_reflexive(v in arb_format_version()) {
        prop_assert!(
            v.check_compatibility(&v).is_ok(),
            "version {:?} should be compatible with itself",
            v
        );
    }

    /// is_compatible_with is reflexive.
    #[test]
    fn prop_format_version_is_compat_reflexive(v in arb_format_version()) {
        prop_assert!(v.is_compatible_with(&v));
    }

    /// is_compatible_with is symmetric.
    #[test]
    fn prop_format_version_is_compat_symmetric(
        a in arb_format_version(),
        b in arb_format_version(),
    ) {
        prop_assert_eq!(
            a.is_compatible_with(&b),
            b.is_compatible_with(&a),
            "is_compatible_with should be symmetric"
        );
    }

    /// is_compatible_with iff major versions match.
    #[test]
    fn prop_format_version_compat_iff_same_major(
        a in arb_format_version(),
        b in arb_format_version(),
    ) {
        prop_assert_eq!(
            a.is_compatible_with(&b),
            a.major == b.major,
            "is_compatible_with should match iff majors are equal"
        );
    }

    /// check_compatibility: same major, bundle minor <= reader minor → Ok.
    #[test]
    fn prop_format_version_compat_same_major_leq_minor(
        major in any::<u16>(),
        reader_minor in 0u16..=u16::MAX,
        bundle_minor in 0u16..=u16::MAX,
    ) {
        let reader = BundleFormatVersion { major, minor: reader_minor };
        let bundle = BundleFormatVersion { major, minor: bundle_minor };
        let result = reader.check_compatibility(&bundle);
        if bundle_minor <= reader_minor {
            prop_assert!(result.is_ok(), "should be Ok when bundle minor <= reader minor");
        } else {
            prop_assert!(
                matches!(result, Err(frankenterm_core::incident_bundle::BundleVersionError::NewerMinor { .. })),
                "should be NewerMinor when bundle minor > reader minor"
            );
        }
    }

    /// check_compatibility: different major → IncompatibleMajor.
    #[test]
    fn prop_format_version_compat_different_major(
        a_major in any::<u16>(),
        b_major in any::<u16>(),
        a_minor in any::<u16>(),
        b_minor in any::<u16>(),
    ) {
        prop_assume!(a_major != b_major);
        let reader = BundleFormatVersion { major: a_major, minor: a_minor };
        let bundle = BundleFormatVersion { major: b_major, minor: b_minor };
        let result = reader.check_compatibility(&bundle);
        prop_assert!(
            matches!(result, Err(frankenterm_core::incident_bundle::BundleVersionError::IncompatibleMajor { .. })),
            "different majors should yield IncompatibleMajor"
        );
    }

    /// Display format is "{major}.{minor}".
    #[test]
    fn prop_format_version_display(v in arb_format_version()) {
        let display = v.to_string();
        let expected = format!("{}.{}", v.major, v.minor);
        prop_assert_eq!(display, expected);
    }
}

// ============================================================================
// PrivacyBudget properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Serde roundtrip for PrivacyBudget.
    #[test]
    fn prop_budget_serde_roundtrip(budget in arb_valid_budget()) {
        let json = serde_json::to_string(&budget).unwrap();
        let parsed: PrivacyBudget = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, budget);
    }

    /// Valid budgets always pass validate().
    #[test]
    fn prop_budget_valid_passes_validate(budget in arb_valid_budget()) {
        prop_assert!(
            budget.validate().is_ok(),
            "valid budget should pass validate(): {:?}",
            budget
        );
    }

    /// validate fails when max_bytes_per_file > max_total_bytes (and both > 0).
    #[test]
    fn prop_budget_file_exceeds_total_fails(
        total in 1usize..=1_000_000,
        excess in 1usize..=1_000_000,
    ) {
        let budget = PrivacyBudget {
            max_bytes_per_file: total + excess,
            max_total_bytes: total,
            ..PrivacyBudget::default()
        };
        let result = budget.validate();
        prop_assert!(
            matches!(result, Err(frankenterm_core::incident_bundle::PrivacyBudgetError::FileExceedsTotal { .. })),
            "file > total should fail validation"
        );
    }

    /// validate fails when max_total_bytes is zero.
    #[test]
    fn prop_budget_zero_total_fails(per_file in 0usize..=0) {
        let budget = PrivacyBudget {
            max_bytes_per_file: per_file,
            max_total_bytes: 0,
            ..PrivacyBudget::default()
        };
        prop_assert!(budget.validate().is_err(), "zero total should fail");
    }

    /// would_exceed_total respects saturating addition.
    #[test]
    fn prop_budget_would_exceed_total(
        total in 1usize..=10_000_000,
        current in 0usize..=10_000_000,
        additional in 0usize..=10_000_000,
    ) {
        let budget = PrivacyBudget {
            max_total_bytes: total,
            ..PrivacyBudget::default()
        };
        let exceeds = budget.would_exceed_total(current, additional);
        let expected = current.saturating_add(additional) > total;
        prop_assert_eq!(exceeds, expected);
    }

    /// truncate_file_content preserves short content unchanged.
    #[test]
    fn prop_truncate_file_content_short_unchanged(
        content in arb_ascii_content(),
        limit in 500usize..=1_000_000,
    ) {
        let budget = PrivacyBudget {
            max_bytes_per_file: limit,
            max_total_bytes: limit + 1,
            ..PrivacyBudget::default()
        };
        if content.len() <= limit {
            let result = budget.truncate_file_content(&content);
            prop_assert_eq!(result, content, "short content should be unchanged");
        }
    }

    /// truncate_file_content output is valid UTF-8 (with multi-byte chars).
    #[test]
    fn prop_truncate_file_content_utf8_safe(content in arb_utf8_content()) {
        let budget = PrivacyBudget {
            max_bytes_per_file: 50,
            max_total_bytes: 100,
            ..PrivacyBudget::default()
        };
        let result = budget.truncate_file_content(&content);
        // If it compiles and doesn't panic, the output is valid UTF-8.
        prop_assert!(!result.is_empty() || content.is_empty());
    }

    /// truncate_file_content: when truncated, contains the marker.
    #[test]
    fn prop_truncate_file_content_marker(
        limit in 60usize..=200,
    ) {
        let budget = PrivacyBudget {
            max_bytes_per_file: limit,
            max_total_bytes: limit + 1,
            ..PrivacyBudget::default()
        };
        // Create content that's definitely longer than limit.
        let content: String = "x".repeat(limit + 100);
        let result = budget.truncate_file_content(&content);
        prop_assert!(
            result.contains("truncated"),
            "truncated output should contain marker"
        );
    }

    /// truncate_excerpt preserves short text unchanged.
    #[test]
    fn prop_truncate_excerpt_short_unchanged(
        text in "[a-z]{0,10}",
        limit in 10usize..=500,
    ) {
        let budget = PrivacyBudget {
            max_output_excerpt_len: limit,
            ..PrivacyBudget::default()
        };
        let char_count = text.chars().count();
        if char_count <= limit {
            let result = budget.truncate_excerpt(&text);
            prop_assert_eq!(result, text);
        }
    }

    /// truncate_excerpt output character count is bounded.
    #[test]
    fn prop_truncate_excerpt_char_bound(
        text in arb_utf8_content(),
        limit in 1usize..=100,
    ) {
        let budget = PrivacyBudget {
            max_output_excerpt_len: limit,
            ..PrivacyBudget::default()
        };
        let result = budget.truncate_excerpt(&text);
        let input_chars = text.chars().count();
        if input_chars <= limit {
            prop_assert_eq!(result.chars().count(), input_chars);
        } else {
            // limit chars + "..." (3 chars)
            prop_assert_eq!(result.chars().count(), limit + 3);
        }
    }
}

// ============================================================================
// BundleFile properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Serde roundtrip for BundleFile.
    #[test]
    fn prop_bundle_file_serde_roundtrip(file in arb_bundle_file()) {
        let json = serde_json::to_string(&file).unwrap();
        let parsed: BundleFile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, file);
    }

    /// Every BundleFile has a non-empty filename.
    #[test]
    fn prop_bundle_file_filename_nonempty(file in arb_bundle_file()) {
        prop_assert!(!file.filename().is_empty());
    }

    /// Required files apply to every incident kind.
    #[test]
    fn prop_required_files_apply_to_all_kinds(
        file in arb_bundle_file(),
        kind in arb_incident_kind(),
    ) {
        if file.is_required() {
            prop_assert!(
                file.applies_to_kind(kind),
                "required file {:?} should apply to {:?}",
                file,
                kind
            );
        }
    }

    /// expected_for_kind always includes all required files.
    #[test]
    fn prop_expected_includes_required(kind in arb_incident_kind()) {
        let expected = BundleFile::expected_for_kind(kind);
        for file in BundleFile::all() {
            if file.is_required() {
                prop_assert!(
                    expected.contains(file),
                    "expected_for_kind({:?}) missing required file {:?}",
                    kind,
                    file
                );
            }
        }
    }

    /// expected_for_kind is a subset of all().
    #[test]
    fn prop_expected_subset_of_all(kind in arb_incident_kind()) {
        let all = BundleFile::all();
        let expected = BundleFile::expected_for_kind(kind);
        for file in &expected {
            prop_assert!(
                all.contains(file),
                "{:?} in expected but not in all()",
                file
            );
        }
    }

    /// Crash-only files don't apply to Manual kind.
    #[test]
    fn prop_crash_files_not_in_manual(file in arb_bundle_file()) {
        if matches!(file, BundleFile::CrashReport | BundleFile::CrashManifest) {
            prop_assert!(
                file.applies_to_kind(IncidentKind::Crash),
                "crash file should apply to Crash kind"
            );
            prop_assert!(
                !file.applies_to_kind(IncidentKind::Manual),
                "crash file should not apply to Manual kind"
            );
        }
    }
}

// ============================================================================
// BundleReplayMode properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Serde roundtrip for BundleReplayMode.
    #[test]
    fn prop_replay_mode_serde_roundtrip(mode in arb_replay_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let parsed: BundleReplayMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, mode);
    }

    /// Every replay mode contract requires BundleFile::Manifest.
    #[test]
    fn prop_replay_contract_requires_manifest(mode in arb_replay_mode()) {
        let contract = mode.contract();
        prop_assert!(
            contract.required_files.contains(&BundleFile::Manifest),
            "{:?} contract should require Manifest",
            mode
        );
    }

    /// Every replay mode contract has "manifest_valid" check.
    #[test]
    fn prop_replay_contract_has_manifest_valid(mode in arb_replay_mode()) {
        let contract = mode.contract();
        prop_assert!(
            contract.checks.contains(&"manifest_valid"),
            "{:?} contract missing manifest_valid check",
            mode
        );
    }

    /// Every replay mode contract has "no_secrets_leaked" check.
    #[test]
    fn prop_replay_contract_has_no_secrets_leaked(mode in arb_replay_mode()) {
        let contract = mode.contract();
        prop_assert!(
            contract.checks.contains(&"no_secrets_leaked"),
            "{:?} contract missing no_secrets_leaked check",
            mode
        );
    }

    /// Every replay mode contract has "version_compatible" check.
    #[test]
    fn prop_replay_contract_has_version_compatible(mode in arb_replay_mode()) {
        let contract = mode.contract();
        prop_assert!(
            contract.checks.contains(&"version_compatible"),
            "{:?} contract missing version_compatible check",
            mode
        );
    }

    /// Contract mode field matches the mode it was created from.
    #[test]
    fn prop_replay_contract_mode_matches(mode in arb_replay_mode()) {
        let contract = mode.contract();
        prop_assert_eq!(contract.mode, mode);
    }

    /// Contract has non-empty checks and description.
    #[test]
    fn prop_replay_contract_nonempty(mode in arb_replay_mode()) {
        let contract = mode.contract();
        prop_assert!(!contract.checks.is_empty());
        prop_assert!(!contract.description.is_empty());
    }

    /// Display output is non-empty and lowercase.
    #[test]
    fn prop_replay_mode_display_nonempty_lowercase(mode in arb_replay_mode()) {
        let display = mode.to_string();
        prop_assert!(!display.is_empty());
        let lower = display.to_lowercase();
        prop_assert_eq!(display, lower, "Display should be lowercase");
    }
}

// ============================================================================
// PrivacyBudgetSummary properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// PrivacyBudgetSummary from default budget has tier "default".
    #[test]
    fn prop_budget_summary_default_tier(_dummy in 0u8..1) {
        let summary = PrivacyBudgetSummary::from(&PrivacyBudget::default());
        prop_assert_eq!(summary.tier.as_str(), "default");
    }

    /// PrivacyBudgetSummary from strict budget has tier "strict".
    #[test]
    fn prop_budget_summary_strict_tier(_dummy in 0u8..1) {
        let summary = PrivacyBudgetSummary::from(&PrivacyBudget::strict());
        prop_assert_eq!(summary.tier.as_str(), "strict");
    }

    /// PrivacyBudgetSummary from verbose budget has tier "verbose".
    #[test]
    fn prop_budget_summary_verbose_tier(_dummy in 0u8..1) {
        let summary = PrivacyBudgetSummary::from(&PrivacyBudget::verbose());
        prop_assert_eq!(summary.tier.as_str(), "verbose");
    }

    /// Custom budgets (not matching default/strict/verbose) get tier "custom".
    #[test]
    fn prop_budget_summary_custom_tier(budget in arb_valid_budget()) {
        let summary = PrivacyBudgetSummary::from(&budget);
        let is_default = budget == PrivacyBudget::default();
        let is_strict = budget == PrivacyBudget::strict();
        let is_verbose = budget == PrivacyBudget::verbose();
        if !is_default && !is_strict && !is_verbose {
            prop_assert_eq!(summary.tier.as_str(), "custom");
        }
    }

    /// Summary fields match budget fields.
    #[test]
    fn prop_budget_summary_fields_match(budget in arb_valid_budget()) {
        let summary = PrivacyBudgetSummary::from(&budget);
        prop_assert_eq!(summary.max_total_bytes, budget.max_total_bytes);
        prop_assert_eq!(summary.max_bytes_per_file, budget.max_bytes_per_file);
        prop_assert_eq!(summary.includes_events, budget.include_recent_events);
        prop_assert_eq!(summary.max_events, budget.max_recent_events);
    }
}

// ============================================================================
// bundle_dirname properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// bundle_dirname starts with "wa_incident_".
    #[test]
    fn prop_bundle_dirname_prefix(
        kind in arb_incident_kind(),
        ts in arb_timestamp_str(),
    ) {
        let name = bundle_dirname(kind, &ts);
        prop_assert!(name.starts_with("wa_incident_"));
    }

    /// bundle_dirname contains the kind and timestamp.
    #[test]
    fn prop_bundle_dirname_contains_parts(
        kind in arb_incident_kind(),
        ts in arb_timestamp_str(),
    ) {
        let name = bundle_dirname(kind, &ts);
        let kind_str = kind.to_string();
        prop_assert!(name.contains(&kind_str), "dirname should contain kind");
        prop_assert!(name.contains(&ts), "dirname should contain timestamp");
    }

    /// bundle_dirname format is exactly "wa_incident_{kind}_{ts}".
    #[test]
    fn prop_bundle_dirname_exact_format(
        kind in arb_incident_kind(),
        ts in arb_timestamp_str(),
    ) {
        let name = bundle_dirname(kind, &ts);
        let expected = format!("wa_incident_{}_{}", kind, ts);
        prop_assert_eq!(name, expected);
    }
}

// ============================================================================
// generate_bundle_readme properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// README contains the incident bundle header.
    #[test]
    fn prop_readme_contains_header(manifest in arb_manifest()) {
        let readme = generate_bundle_readme(&manifest);
        prop_assert!(readme.contains("ft Incident Bundle"));
    }

    /// README contains replay instructions.
    #[test]
    fn prop_readme_contains_replay(manifest in arb_manifest()) {
        let readme = generate_bundle_readme(&manifest);
        prop_assert!(readme.contains("ft reproduce"));
    }

    /// README contains the safety section.
    #[test]
    fn prop_readme_contains_safety(manifest in arb_manifest()) {
        let readme = generate_bundle_readme(&manifest);
        prop_assert!(readme.contains("Safety"));
    }

    /// README mentions redaction (either "No secrets" or "redacted").
    #[test]
    fn prop_readme_mentions_redaction(manifest in arb_manifest()) {
        let readme = generate_bundle_readme(&manifest);
        prop_assert!(
            readme.contains("redact") || readme.contains("No secrets"),
            "README should mention redaction"
        );
    }

    /// README contains the privacy budget tier.
    #[test]
    fn prop_readme_contains_budget_tier(manifest in arb_manifest()) {
        let readme = generate_bundle_readme(&manifest);
        prop_assert!(
            readme.contains(&manifest.privacy_budget.tier),
            "README should contain budget tier '{}'",
            manifest.privacy_budget.tier
        );
    }

    /// README lists all file entries from the manifest.
    #[test]
    fn prop_readme_lists_files(manifest in arb_manifest()) {
        let readme = generate_bundle_readme(&manifest);
        for entry in &manifest.files {
            prop_assert!(
                readme.contains(&entry.name),
                "README should list file '{}'",
                entry.name
            );
        }
    }
}

// ============================================================================
// IncidentManifest serde roundtrip
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// IncidentManifest survives JSON roundtrip with key fields preserved.
    #[test]
    fn prop_manifest_serde_roundtrip(manifest in arb_manifest()) {
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: IncidentManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.format_version, manifest.format_version);
        prop_assert_eq!(parsed.kind, manifest.kind);
        prop_assert_eq!(parsed.wa_version, manifest.wa_version);
        prop_assert_eq!(parsed.created_at, manifest.created_at);
        prop_assert_eq!(parsed.files.len(), manifest.files.len());
        prop_assert_eq!(parsed.total_size_bytes, manifest.total_size_bytes);
    }
}
