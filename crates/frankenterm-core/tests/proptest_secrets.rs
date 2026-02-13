//! Property-based tests for secrets module
//!
//! Tests: SecretScanOptions (serde roundtrip, default values, field preservation),
//! SecretScanScope (serde roundtrip, default, from_options), scope_hash (determinism,
//! collision resistance, format), SecretScanSample (serde roundtrip, field preservation),
//! SecretScanReport (serde roundtrip, field integrity, BTreeMap ordering).

use frankenterm_core::secrets::{
    SECRET_SCAN_REPORT_VERSION, SecretScanOptions, SecretScanReport, SecretScanSample,
    SecretScanScope, scope_hash,
};
use proptest::prelude::*;
use std::collections::BTreeMap;

// ============================================================================
// Strategies
// ============================================================================

/// Generate arbitrary SecretScanOptions.
fn arb_scan_options() -> impl Strategy<Value = SecretScanOptions> {
    (
        proptest::option::of(0..1000u64),
        proptest::option::of(1_000_000_000_000i64..2_000_000_000_000i64),
        proptest::option::of(1_000_000_000_000i64..2_000_000_000_000i64),
        proptest::option::of(1..100000usize),
        1..10000usize,
        1..1000usize,
    )
        .prop_map(
            |(pane_id, since, until, max_segments, batch_size, sample_limit)| SecretScanOptions {
                pane_id,
                since,
                until,
                max_segments,
                batch_size,
                sample_limit,
            },
        )
}

/// Generate arbitrary SecretScanScope.
fn arb_scan_scope() -> impl Strategy<Value = SecretScanScope> {
    (
        proptest::option::of(0..1000u64),
        proptest::option::of(1_000_000_000_000i64..2_000_000_000_000i64),
        proptest::option::of(1_000_000_000_000i64..2_000_000_000_000i64),
    )
        .prop_map(|(pane_id, since, until)| SecretScanScope {
            pane_id,
            since,
            until,
        })
}

/// Generate a valid hex hash string (64 chars, lowercase hex).
fn arb_hex_hash() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just('0'),
            Just('1'),
            Just('2'),
            Just('3'),
            Just('4'),
            Just('5'),
            Just('6'),
            Just('7'),
            Just('8'),
            Just('9'),
            Just('a'),
            Just('b'),
            Just('c'),
            Just('d'),
            Just('e'),
            Just('f'),
        ],
        64,
    )
    .prop_map(|chars| chars.into_iter().collect::<String>())
}

/// Generate arbitrary SecretScanSample.
fn arb_scan_sample() -> impl Strategy<Value = SecretScanSample> {
    (
        "[a-z_]{3,30}",
        1..10000i64,
        0..1000u64,
        1_000_000_000_000i64..2_000_000_000_000i64,
        arb_hex_hash(),
        1..500usize,
    )
        .prop_map(
            |(pattern, segment_id, pane_id, captured_at, secret_hash, match_len)| {
                SecretScanSample {
                    pattern,
                    segment_id,
                    pane_id,
                    captured_at,
                    secret_hash,
                    match_len,
                }
            },
        )
}

/// Generate arbitrary matches_by_pattern BTreeMap.
fn arb_matches_by_pattern() -> impl Strategy<Value = BTreeMap<String, u64>> {
    proptest::collection::btree_map("[a-z_]{3,20}", 1..1000u64, 0..10)
}

/// Generate arbitrary SecretScanReport.
fn arb_scan_report() -> impl Strategy<Value = SecretScanReport> {
    (
        arb_scan_scope(),
        1_000_000_000_000i64..2_000_000_000_000i64,
        1_000_000_000_000i64..2_000_000_000_000i64,
        proptest::option::of(1..10000i64),
        proptest::option::of(1..10000i64),
        0..10000u64,
        0..1000000u64,
        0..1000u64,
        arb_matches_by_pattern(),
        proptest::collection::vec(arb_scan_sample(), 0..5),
    )
        .prop_map(
            |(
                scope,
                started_at,
                completed_at,
                resume_after_id,
                last_segment_id,
                scanned_segments,
                scanned_bytes,
                matches_total,
                matches_by_pattern,
                samples,
            )| {
                SecretScanReport {
                    report_version: SECRET_SCAN_REPORT_VERSION,
                    scope,
                    started_at,
                    completed_at,
                    resume_after_id,
                    last_segment_id,
                    scanned_segments,
                    scanned_bytes,
                    matches_total,
                    matches_by_pattern,
                    samples,
                }
            },
        )
}

// ============================================================================
// Property Tests: SecretScanOptions
// ============================================================================

proptest! {
    /// Property 1: SecretScanOptions serde roundtrip preserves all fields
    #[test]
    fn prop_scan_options_serde_roundtrip(opts in arb_scan_options()) {
        let json = serde_json::to_string(&opts).unwrap();
        let back: SecretScanOptions = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, opts.pane_id, "pane_id mismatch");
        prop_assert_eq!(back.since, opts.since, "since mismatch");
        prop_assert_eq!(back.until, opts.until, "until mismatch");
        prop_assert_eq!(back.max_segments, opts.max_segments, "max_segments mismatch");
        prop_assert_eq!(back.batch_size, opts.batch_size, "batch_size mismatch");
        prop_assert_eq!(back.sample_limit, opts.sample_limit, "sample_limit mismatch");
    }

    /// Property 2: SecretScanOptions default has expected values
    #[test]
    fn prop_scan_options_default_values(_dummy in Just(())) {
        let opts = SecretScanOptions::default();
        prop_assert!(opts.pane_id.is_none(), "Default pane_id should be None");
        prop_assert!(opts.since.is_none(), "Default since should be None");
        prop_assert!(opts.until.is_none(), "Default until should be None");
        prop_assert!(opts.max_segments.is_none(), "Default max_segments should be None");
        prop_assert_eq!(opts.batch_size, 1_000, "Default batch_size should be 1000");
        prop_assert_eq!(opts.sample_limit, 200, "Default sample_limit should be 200");
    }

    /// Property 3: SecretScanOptions serializes to valid JSON with expected fields
    #[test]
    fn prop_scan_options_json_structure(opts in arb_scan_options()) {
        let val: serde_json::Value = serde_json::to_value(&opts).unwrap();
        let map = val.as_object().unwrap();
        // batch_size and sample_limit are always present
        prop_assert!(map.contains_key("batch_size"), "batch_size should always be present");
        prop_assert!(map.contains_key("sample_limit"), "sample_limit should always be present");
    }

    // ========================================================================
    // Property Tests: SecretScanScope
    // ========================================================================

    /// Property 4: SecretScanScope serde roundtrip
    #[test]
    fn prop_scan_scope_serde_roundtrip(scope in arb_scan_scope()) {
        let json = serde_json::to_string(&scope).unwrap();
        let back: SecretScanScope = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, scope.pane_id, "pane_id mismatch");
        prop_assert_eq!(back.since, scope.since, "since mismatch");
        prop_assert_eq!(back.until, scope.until, "until mismatch");
    }

    /// Property 5: SecretScanScope default is all-None
    #[test]
    fn prop_scan_scope_default(_dummy in Just(())) {
        let scope = SecretScanScope::default();
        prop_assert!(scope.pane_id.is_none(), "Default pane_id should be None");
        prop_assert!(scope.since.is_none(), "Default since should be None");
        prop_assert!(scope.until.is_none(), "Default until should be None");
    }

    /// Property 6: SecretScanScope::from_options extracts correct fields
    #[test]
    fn prop_scan_scope_from_options(opts in arb_scan_options()) {
        let scope = SecretScanScope::from_options(&opts);
        prop_assert_eq!(scope.pane_id, opts.pane_id,
            "from_options should preserve pane_id");
        prop_assert_eq!(scope.since, opts.since,
            "from_options should preserve since");
        prop_assert_eq!(scope.until, opts.until,
            "from_options should preserve until");
    }

    /// Property 7: from_options ignores non-scope fields
    #[test]
    fn prop_scan_scope_from_options_ignores_non_scope(
        pane_id in proptest::option::of(0..100u64),
        since in proptest::option::of(1000i64..2000i64),
        until in proptest::option::of(2000i64..3000i64),
        max_segments1 in proptest::option::of(1..100usize),
        max_segments2 in proptest::option::of(100..200usize),
        batch_size1 in 1..100usize,
        batch_size2 in 100..200usize,
    ) {
        let opts1 = SecretScanOptions {
            pane_id,
            since,
            until,
            max_segments: max_segments1,
            batch_size: batch_size1,
            sample_limit: 50,
        };
        let opts2 = SecretScanOptions {
            pane_id,
            since,
            until,
            max_segments: max_segments2,
            batch_size: batch_size2,
            sample_limit: 100,
        };
        let scope1 = SecretScanScope::from_options(&opts1);
        let scope2 = SecretScanScope::from_options(&opts2);

        // Scopes should be identical when scope fields match
        prop_assert_eq!(scope1.pane_id, scope2.pane_id, "pane_id should match");
        prop_assert_eq!(scope1.since, scope2.since, "since should match");
        prop_assert_eq!(scope1.until, scope2.until, "until should match");
    }

    // ========================================================================
    // Property Tests: scope_hash
    // ========================================================================

    /// Property 8: scope_hash is deterministic
    #[test]
    fn prop_scope_hash_deterministic(opts in arb_scan_options()) {
        let h1 = scope_hash(&opts).unwrap();
        let h2 = scope_hash(&opts).unwrap();
        prop_assert_eq!(h1, h2, "scope_hash should be deterministic");
    }

    /// Property 9: scope_hash produces 64-char hex string (SHA-256)
    #[test]
    fn prop_scope_hash_format(opts in arb_scan_options()) {
        let h = scope_hash(&opts).unwrap();
        prop_assert_eq!(h.len(), 64,
            "scope_hash should be 64 hex chars, got {} chars", h.len());
        prop_assert!(h.chars().all(|c| c.is_ascii_hexdigit()),
            "scope_hash should be all hex digits: {}", h);
    }

    /// Property 10: scope_hash differs when scope fields differ
    #[test]
    fn prop_scope_hash_differs_on_pane_id(
        pane1 in 0..500u64,
        pane2 in 500..1000u64,
    ) {
        let opts1 = SecretScanOptions {
            pane_id: Some(pane1),
            ..Default::default()
        };
        let opts2 = SecretScanOptions {
            pane_id: Some(pane2),
            ..Default::default()
        };
        let h1 = scope_hash(&opts1).unwrap();
        let h2 = scope_hash(&opts2).unwrap();
        prop_assert_ne!(h1, h2,
            "Different pane_ids ({} vs {}) should produce different hashes", pane1, pane2);
    }

    /// Property 11: scope_hash same for same scope, different non-scope fields
    #[test]
    fn prop_scope_hash_same_scope_different_options(
        pane_id in proptest::option::of(0..100u64),
        since in proptest::option::of(1000i64..2000i64),
        until in proptest::option::of(2000i64..3000i64),
        batch1 in 1..500usize,
        batch2 in 500..1000usize,
    ) {
        let opts1 = SecretScanOptions {
            pane_id,
            since,
            until,
            max_segments: Some(10),
            batch_size: batch1,
            sample_limit: 50,
        };
        let opts2 = SecretScanOptions {
            pane_id,
            since,
            until,
            max_segments: Some(20),
            batch_size: batch2,
            sample_limit: 100,
        };
        let h1 = scope_hash(&opts1).unwrap();
        let h2 = scope_hash(&opts2).unwrap();
        prop_assert_eq!(h1, h2,
            "Same scope fields should produce same hash regardless of batch/limit");
    }

    /// Property 12: scope_hash differs when since differs
    #[test]
    fn prop_scope_hash_differs_on_since(
        since1 in 1_000_000_000_000i64..1_500_000_000_000i64,
        since2 in 1_500_000_000_000i64..2_000_000_000_000i64,
    ) {
        let opts1 = SecretScanOptions {
            since: Some(since1),
            ..Default::default()
        };
        let opts2 = SecretScanOptions {
            since: Some(since2),
            ..Default::default()
        };
        let h1 = scope_hash(&opts1).unwrap();
        let h2 = scope_hash(&opts2).unwrap();
        prop_assert_ne!(h1, h2,
            "Different since values should produce different hashes");
    }

    /// Property 13: scope_hash differs when Some vs None for pane_id
    #[test]
    fn prop_scope_hash_differs_some_vs_none(pane_id in 0..1000u64) {
        let opts_some = SecretScanOptions {
            pane_id: Some(pane_id),
            ..Default::default()
        };
        let opts_none = SecretScanOptions::default();
        let h_some = scope_hash(&opts_some).unwrap();
        let h_none = scope_hash(&opts_none).unwrap();
        prop_assert_ne!(h_some, h_none,
            "Some(pane_id) vs None should differ");
    }

    // ========================================================================
    // Property Tests: SecretScanSample
    // ========================================================================

    /// Property 14: SecretScanSample serde roundtrip
    #[test]
    fn prop_scan_sample_serde_roundtrip(sample in arb_scan_sample()) {
        let json = serde_json::to_string(&sample).unwrap();
        let back: SecretScanSample = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.pattern, &sample.pattern, "pattern mismatch");
        prop_assert_eq!(back.segment_id, sample.segment_id, "segment_id mismatch");
        prop_assert_eq!(back.pane_id, sample.pane_id, "pane_id mismatch");
        prop_assert_eq!(back.captured_at, sample.captured_at, "captured_at mismatch");
        prop_assert_eq!(&back.secret_hash, &sample.secret_hash, "secret_hash mismatch");
        prop_assert_eq!(back.match_len, sample.match_len, "match_len mismatch");
    }

    /// Property 15: SecretScanSample JSON always contains all fields
    #[test]
    fn prop_scan_sample_all_fields_present(sample in arb_scan_sample()) {
        let val: serde_json::Value = serde_json::to_value(&sample).unwrap();
        let map = val.as_object().unwrap();
        let expected_fields = ["pattern", "segment_id", "pane_id", "captured_at",
                              "secret_hash", "match_len"];
        for field in &expected_fields {
            prop_assert!(map.contains_key(*field),
                "Missing field '{}' in serialized SecretScanSample", field);
        }
        prop_assert_eq!(map.len(), expected_fields.len(),
            "SecretScanSample should have exactly {} fields, got {}",
            expected_fields.len(), map.len());
    }

    /// Property 16: SecretScanSample pattern is non-empty
    #[test]
    fn prop_scan_sample_pattern_nonempty(sample in arb_scan_sample()) {
        prop_assert!(!sample.pattern.is_empty(),
            "pattern should be non-empty");
    }

    /// Property 17: SecretScanSample secret_hash is 64-char hex
    #[test]
    fn prop_scan_sample_hash_format(sample in arb_scan_sample()) {
        prop_assert_eq!(sample.secret_hash.len(), 64,
            "secret_hash should be 64 chars");
        prop_assert!(sample.secret_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "secret_hash should be hex: {}", sample.secret_hash);
    }

    // ========================================================================
    // Property Tests: SecretScanReport
    // ========================================================================

    /// Property 18: SecretScanReport serde roundtrip preserves all scalar fields
    #[test]
    fn prop_scan_report_serde_roundtrip(report in arb_scan_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: SecretScanReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.report_version, report.report_version,
            "report_version mismatch");
        prop_assert_eq!(back.started_at, report.started_at,
            "started_at mismatch");
        prop_assert_eq!(back.completed_at, report.completed_at,
            "completed_at mismatch");
        prop_assert_eq!(back.resume_after_id, report.resume_after_id,
            "resume_after_id mismatch");
        prop_assert_eq!(back.last_segment_id, report.last_segment_id,
            "last_segment_id mismatch");
        prop_assert_eq!(back.scanned_segments, report.scanned_segments,
            "scanned_segments mismatch");
        prop_assert_eq!(back.scanned_bytes, report.scanned_bytes,
            "scanned_bytes mismatch");
        prop_assert_eq!(back.matches_total, report.matches_total,
            "matches_total mismatch");
    }

    /// Property 19: SecretScanReport serde preserves matches_by_pattern
    #[test]
    fn prop_scan_report_serde_preserves_pattern_map(report in arb_scan_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: SecretScanReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.matches_by_pattern.len(), report.matches_by_pattern.len(),
            "matches_by_pattern length mismatch");
        for (key, val) in &report.matches_by_pattern {
            let back_val = back.matches_by_pattern.get(key);
            prop_assert_eq!(back_val, Some(val),
                "Pattern '{}' count mismatch: expected {}, got {:?}", key, val, back_val);
        }
    }

    /// Property 20: SecretScanReport serde preserves samples count and order
    #[test]
    fn prop_scan_report_serde_preserves_samples(report in arb_scan_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: SecretScanReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.samples.len(), report.samples.len(),
            "samples count mismatch");
        for (i, (orig, deser)) in report.samples.iter().zip(back.samples.iter()).enumerate() {
            prop_assert_eq!(&deser.pattern, &orig.pattern,
                "Sample {} pattern mismatch", i);
            prop_assert_eq!(deser.segment_id, orig.segment_id,
                "Sample {} segment_id mismatch", i);
            prop_assert_eq!(&deser.secret_hash, &orig.secret_hash,
                "Sample {} secret_hash mismatch", i);
        }
    }

    /// Property 21: SecretScanReport serde preserves scope
    #[test]
    fn prop_scan_report_serde_preserves_scope(report in arb_scan_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: SecretScanReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.scope.pane_id, report.scope.pane_id,
            "scope.pane_id mismatch");
        prop_assert_eq!(back.scope.since, report.scope.since,
            "scope.since mismatch");
        prop_assert_eq!(back.scope.until, report.scope.until,
            "scope.until mismatch");
    }

    /// Property 22: Report version is always SECRET_SCAN_REPORT_VERSION
    #[test]
    fn prop_scan_report_version_constant(report in arb_scan_report()) {
        prop_assert_eq!(report.report_version, SECRET_SCAN_REPORT_VERSION,
            "report_version should be {}", SECRET_SCAN_REPORT_VERSION);
    }

    /// Property 23: SecretScanReport JSON contains all required top-level fields
    #[test]
    fn prop_scan_report_json_fields(report in arb_scan_report()) {
        let val: serde_json::Value = serde_json::to_value(&report).unwrap();
        let map = val.as_object().unwrap();
        let required = [
            "report_version", "scope", "started_at", "completed_at",
            "scanned_segments", "scanned_bytes", "matches_total",
            "matches_by_pattern", "samples",
        ];
        for field in &required {
            prop_assert!(map.contains_key(*field),
                "Required field '{}' missing from SecretScanReport JSON", field);
        }
    }

    /// Property 24: matches_by_pattern keys are sorted in JSON (BTreeMap)
    #[test]
    fn prop_scan_report_pattern_keys_sorted(report in arb_scan_report()) {
        let val: serde_json::Value = serde_json::to_value(&report).unwrap();
        let map_val = &val["matches_by_pattern"];
        if let Some(obj) = map_val.as_object() {
            let keys: Vec<&String> = obj.keys().collect();
            for window in keys.windows(2) {
                prop_assert!(window[0] <= window[1],
                    "BTreeMap keys should be sorted: '{}' > '{}'", window[0], window[1]);
            }
        }
    }

    /// Property 25: report with empty samples serializes samples as empty array
    #[test]
    fn prop_scan_report_empty_samples_array(scope in arb_scan_scope()) {
        let report = SecretScanReport {
            report_version: SECRET_SCAN_REPORT_VERSION,
            scope,
            started_at: 1_000_000_000_000,
            completed_at: 1_000_000_000_001,
            resume_after_id: None,
            last_segment_id: None,
            scanned_segments: 0,
            scanned_bytes: 0,
            matches_total: 0,
            matches_by_pattern: BTreeMap::new(),
            samples: Vec::new(),
        };
        let val: serde_json::Value = serde_json::to_value(&report).unwrap();
        let arr = val["samples"].as_array().unwrap();
        prop_assert!(arr.is_empty(), "Empty samples should serialize as empty array");
    }

    // ========================================================================
    // Property Tests: Cross-module consistency
    // ========================================================================

    /// Property 26: scope_hash output is consistent with scope serialization
    #[test]
    fn prop_scope_hash_based_on_scope_json(opts in arb_scan_options()) {
        let h = scope_hash(&opts).unwrap();
        // Hash should be 64 hex chars (SHA-256)
        prop_assert_eq!(h.len(), 64, "Hash length should be 64");

        // Same options should give same hash
        let h2 = scope_hash(&opts).unwrap();
        prop_assert_eq!(h, h2, "Same options should give same hash");
    }

    /// Property 27: SecretScanOptions with all None scope fields produces default scope hash
    #[test]
    fn prop_scope_hash_default_options(_dummy in Just(())) {
        let opts1 = SecretScanOptions::default();
        let opts2 = SecretScanOptions {
            pane_id: None,
            since: None,
            until: None,
            max_segments: Some(999),
            batch_size: 42,
            sample_limit: 1,
        };
        let h1 = scope_hash(&opts1).unwrap();
        let h2 = scope_hash(&opts2).unwrap();
        prop_assert_eq!(h1, h2,
            "Default and all-None scope should produce same hash");
    }

    /// Property 28: SECRET_SCAN_REPORT_VERSION is positive
    #[test]
    fn prop_report_version_positive(_dummy in Just(())) {
        prop_assert!(SECRET_SCAN_REPORT_VERSION > 0,
            "Report version should be positive");
    }

    /// Property 29: SecretScanReport JSON round-trip produces valid JSON
    #[test]
    fn prop_scan_report_valid_json(report in arb_scan_report()) {
        let json = serde_json::to_string(&report).unwrap();
        // Must be parseable
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.is_object(), "Report JSON should be an object");
    }

    /// Property 30: SecretScanReport JSON size is reasonable
    #[test]
    fn prop_scan_report_json_size_bounded(report in arb_scan_report()) {
        let json = serde_json::to_string(&report).unwrap();
        // With up to 5 samples and 10 pattern keys, JSON should be under 10KB
        prop_assert!(json.len() < 10_000,
            "Report JSON should be under 10KB, got {} bytes", json.len());
    }

    /// Property 31: SecretScanOptions batch_size and sample_limit are preserved
    #[test]
    fn prop_scan_options_batch_sample_preserved(
        batch_size in 1..10000usize,
        sample_limit in 1..1000usize,
    ) {
        let opts = SecretScanOptions {
            batch_size,
            sample_limit,
            ..Default::default()
        };
        let json = serde_json::to_string(&opts).unwrap();
        let back: SecretScanOptions = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.batch_size, batch_size, "batch_size should roundtrip");
        prop_assert_eq!(back.sample_limit, sample_limit, "sample_limit should roundtrip");
    }

    /// Property 32: SecretScanScope JSON has at most 3 fields
    #[test]
    fn prop_scan_scope_json_field_count(scope in arb_scan_scope()) {
        let val: serde_json::Value = serde_json::to_value(&scope).unwrap();
        let map = val.as_object().unwrap();
        prop_assert!(map.len() <= 3,
            "SecretScanScope should have at most 3 fields, got {}", map.len());
    }

    /// Property 33: SecretScanReport with samples never has negative match_len
    #[test]
    fn prop_scan_report_sample_match_len_positive(report in arb_scan_report()) {
        for (i, sample) in report.samples.iter().enumerate() {
            prop_assert!(sample.match_len > 0,
                "Sample {} match_len should be positive, got {}", i, sample.match_len);
        }
    }

    /// Property 34: scope_hash uses lowercase hex
    #[test]
    fn prop_scope_hash_lowercase(opts in arb_scan_options()) {
        let h = scope_hash(&opts).unwrap();
        let lower = h.to_lowercase();
        prop_assert_eq!(h, lower,
            "scope_hash should be lowercase hex");
    }

    /// Property 35: SecretScanSample with different patterns produces different JSON
    #[test]
    fn prop_scan_sample_pattern_in_json(
        pattern in "[a-z_]{3,20}",
        segment_id in 1..1000i64,
    ) {
        let sample = SecretScanSample {
            pattern: pattern.clone(),
            segment_id,
            pane_id: 1,
            captured_at: 1_000_000_000_000,
            secret_hash: "a".repeat(64),
            match_len: 10,
        };
        let json = serde_json::to_string(&sample).unwrap();
        prop_assert!(json.contains(&pattern),
            "JSON should contain pattern name '{}'", pattern);
    }
}
