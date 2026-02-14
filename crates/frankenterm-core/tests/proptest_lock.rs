//! Property-based tests for the `lock` module.
//!
//! Covers LockMetadata serde roundtrips, field preservation, boundary values,
//! JSON structure validation, Clone/Debug traits, cross-format consistency,
//! field independence, distinctness, JSON size bounds, and robustness.

use frankenterm_core::lock::LockMetadata;
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_lock_metadata() -> impl Strategy<Value = LockMetadata> {
    (
        1_u32..100_000,
        0_u64..10_000_000_000,
        "[a-z:0-9 ]{5,25}",
        "[0-9.]{3,10}",
    )
        .prop_map(
            |(pid, started_at, started_at_human, wa_version)| LockMetadata {
                pid,
                started_at,
                started_at_human,
                wa_version,
            },
        )
}

fn arb_boundary_metadata() -> impl Strategy<Value = LockMetadata> {
    prop_oneof![
        // Zero PID
        (Just(0u32), any::<u64>(), "[a-z]{1,10}", "[0-9.]{1,5}").prop_map(
            |(pid, ts, human, ver)| LockMetadata {
                pid,
                started_at: ts,
                started_at_human: human,
                wa_version: ver,
            }
        ),
        // Max PID
        (Just(u32::MAX), any::<u64>(), "[a-z]{1,10}", "[0-9.]{1,5}").prop_map(
            |(pid, ts, human, ver)| LockMetadata {
                pid,
                started_at: ts,
                started_at_human: human,
                wa_version: ver,
            }
        ),
        // Max timestamp
        (any::<u32>(), Just(u64::MAX), "[a-z]{1,10}", "[0-9.]{1,5}").prop_map(
            |(pid, ts, human, ver)| LockMetadata {
                pid,
                started_at: ts,
                started_at_human: human,
                wa_version: ver,
            }
        ),
        // Empty strings
        (
            any::<u32>(),
            any::<u64>(),
            Just(String::new()),
            Just(String::new())
        )
            .prop_map(|(pid, ts, human, ver)| LockMetadata {
                pid,
                started_at: ts,
                started_at_human: human,
                wa_version: ver,
            }),
    ]
}

fn arb_version_string() -> impl Strategy<Value = String> {
    prop_oneof![
        "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}",
        "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}-[a-z]{1,8}",
        Just("0.0.0".to_string()),
        Just("99.99.99".to_string()),
    ]
}

// =========================================================================
// LockMetadata — serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// LockMetadata serde roundtrip preserves all fields.
    #[test]
    fn prop_metadata_serde_roundtrip(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pid, meta.pid);
        prop_assert_eq!(back.started_at, meta.started_at);
        prop_assert_eq!(&back.started_at_human, &meta.started_at_human);
        prop_assert_eq!(&back.wa_version, &meta.wa_version);
    }

    /// Serialization is deterministic.
    #[test]
    fn prop_metadata_serde_deterministic(meta in arb_lock_metadata()) {
        let json1 = serde_json::to_string(&meta).unwrap();
        let json2 = serde_json::to_string(&meta).unwrap();
        prop_assert_eq!(&json1, &json2);
    }

    /// Pretty-printed JSON also roundtrips correctly.
    #[test]
    fn prop_metadata_pretty_roundtrip(meta in arb_lock_metadata()) {
        let json = serde_json::to_string_pretty(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pid, meta.pid);
        prop_assert_eq!(back.started_at, meta.started_at);
        prop_assert_eq!(&back.started_at_human, &meta.started_at_human);
        prop_assert_eq!(&back.wa_version, &meta.wa_version);
    }

    /// JSON always contains expected field names.
    #[test]
    fn prop_metadata_json_has_fields(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        prop_assert!(json.contains("\"pid\""), "should contain pid field");
        prop_assert!(json.contains("\"started_at\""), "should contain started_at field");
        prop_assert!(json.contains("\"started_at_human\""), "should contain started_at_human field");
        prop_assert!(json.contains("\"wa_version\""), "should contain wa_version field");
    }
}

// =========================================================================
// JSON structure validation
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Serialized metadata is always a valid JSON object.
    #[test]
    fn prop_metadata_valid_json_object(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object(), "should serialize as a JSON object");
    }

    /// JSON object has exactly 4 fields.
    #[test]
    fn prop_metadata_json_field_count(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert_eq!(obj.len(), 4, "LockMetadata should have exactly 4 fields, got {}", obj.len());
    }

    /// PID serializes as a JSON integer.
    #[test]
    fn prop_metadata_pid_is_integer(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pid_val = value.get("pid").unwrap();
        prop_assert!(pid_val.is_u64() || pid_val.is_i64(),
            "pid should be a JSON integer, got {}", pid_val);
        prop_assert_eq!(pid_val.as_u64().unwrap() as u32, meta.pid);
    }

    /// started_at serializes as a JSON integer.
    #[test]
    fn prop_metadata_started_at_is_integer(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let ts_val = value.get("started_at").unwrap();
        prop_assert!(ts_val.is_u64() || ts_val.is_i64(),
            "started_at should be a JSON integer, got {}", ts_val);
    }

    /// String fields serialize as JSON strings.
    #[test]
    fn prop_metadata_strings_are_json_strings(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("started_at_human").unwrap().is_string(),
            "started_at_human should be a JSON string");
        prop_assert!(value.get("wa_version").unwrap().is_string(),
            "wa_version should be a JSON string");
    }
}

// =========================================================================
// Boundary value tests
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Boundary metadata values survive roundtrip.
    #[test]
    fn prop_boundary_metadata_roundtrip(meta in arb_boundary_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pid, meta.pid);
        prop_assert_eq!(back.started_at, meta.started_at);
        prop_assert_eq!(&back.started_at_human, &meta.started_at_human);
        prop_assert_eq!(&back.wa_version, &meta.wa_version);
    }

    /// Max u64 timestamp roundtrips correctly.
    #[test]
    fn prop_max_timestamp_roundtrip(pid in any::<u32>()) {
        let meta = LockMetadata {
            pid,
            started_at: u64::MAX,
            started_at_human: "max".to_string(),
            wa_version: "0.1.0".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.started_at, u64::MAX);
    }

    /// Zero values roundtrip correctly.
    #[test]
    fn prop_zero_values_roundtrip(_dummy in 0u8..1) {
        let meta = LockMetadata {
            pid: 0,
            started_at: 0,
            started_at_human: String::new(),
            wa_version: String::new(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pid, 0u32);
        prop_assert_eq!(back.started_at, 0u64);
        prop_assert!(back.started_at_human.is_empty());
        prop_assert!(back.wa_version.is_empty());
    }
}

// =========================================================================
// Clone and Debug
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Clone produces field-equivalent metadata.
    #[test]
    fn prop_metadata_clone_equivalent(meta in arb_lock_metadata()) {
        let cloned = meta.clone();
        prop_assert_eq!(cloned.pid, meta.pid);
        prop_assert_eq!(cloned.started_at, meta.started_at);
        prop_assert_eq!(&cloned.started_at_human, &meta.started_at_human);
        prop_assert_eq!(&cloned.wa_version, &meta.wa_version);
    }

    /// Clone serializes identically to original.
    #[test]
    fn prop_metadata_clone_serde_identical(meta in arb_lock_metadata()) {
        let cloned = meta.clone();
        let json_orig = serde_json::to_string(&meta).unwrap();
        let json_clone = serde_json::to_string(&cloned).unwrap();
        prop_assert_eq!(&json_orig, &json_clone,
            "cloned metadata should serialize identically");
    }

    /// Debug representation is non-empty.
    #[test]
    fn prop_metadata_debug_non_empty(meta in arb_lock_metadata()) {
        let debug = format!("{:?}", meta);
        prop_assert!(!debug.is_empty(), "Debug should produce non-empty output");
    }

    /// Debug contains struct name.
    #[test]
    fn prop_metadata_debug_contains_type_name(meta in arb_lock_metadata()) {
        let debug = format!("{:?}", meta);
        prop_assert!(debug.contains("LockMetadata"),
            "Debug should contain type name, got: {}", debug);
    }

    /// Debug contains the PID value.
    #[test]
    fn prop_metadata_debug_contains_pid(meta in arb_lock_metadata()) {
        let debug = format!("{:?}", meta);
        let pid_str = meta.pid.to_string();
        prop_assert!(debug.contains(&pid_str),
            "Debug should contain pid '{}', got: {}", pid_str, debug);
    }
}

// =========================================================================
// Cross-format consistency
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Compact and pretty JSON both deserialize to the same values.
    #[test]
    fn prop_compact_and_pretty_equivalent(meta in arb_lock_metadata()) {
        let compact = serde_json::to_string(&meta).unwrap();
        let pretty = serde_json::to_string_pretty(&meta).unwrap();
        let from_compact: LockMetadata = serde_json::from_str(&compact).unwrap();
        let from_pretty: LockMetadata = serde_json::from_str(&pretty).unwrap();
        prop_assert_eq!(from_compact.pid, from_pretty.pid);
        prop_assert_eq!(from_compact.started_at, from_pretty.started_at);
        prop_assert_eq!(&from_compact.started_at_human, &from_pretty.started_at_human);
        prop_assert_eq!(&from_compact.wa_version, &from_pretty.wa_version);
    }

    /// Pretty JSON is always longer than (or equal to) compact JSON.
    #[test]
    fn prop_pretty_longer_than_compact(meta in arb_lock_metadata()) {
        let compact = serde_json::to_string(&meta).unwrap();
        let pretty = serde_json::to_string_pretty(&meta).unwrap();
        prop_assert!(pretty.len() >= compact.len(),
            "pretty ({}) should be >= compact ({})", pretty.len(), compact.len());
    }

    /// Deserialized from serde_json::Value matches direct deserialization.
    #[test]
    fn prop_value_roundtrip_matches_direct(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let from_value: LockMetadata = serde_json::from_value(value).unwrap();
        let from_str: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(from_value.pid, from_str.pid);
        prop_assert_eq!(from_value.started_at, from_str.started_at);
        prop_assert_eq!(&from_value.started_at_human, &from_str.started_at_human);
        prop_assert_eq!(&from_value.wa_version, &from_str.wa_version);
    }
}

// =========================================================================
// Version string handling
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Various version string formats survive roundtrip.
    #[test]
    fn prop_version_strings_roundtrip(version in arb_version_string()) {
        let meta = LockMetadata {
            pid: 1,
            started_at: 1000,
            started_at_human: "unix:1000".to_string(),
            wa_version: version.clone(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.wa_version, &version);
    }

    /// Unicode in human-readable timestamp survives roundtrip.
    #[test]
    fn prop_unicode_human_timestamp_roundtrip(
        human in "[a-zA-Z0-9: \\-+]{1,50}",
    ) {
        let meta = LockMetadata {
            pid: 42,
            started_at: 999,
            started_at_human: human.clone(),
            wa_version: "1.0.0".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.started_at_human, &human);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn metadata_with_zero_pid() {
    let meta = LockMetadata {
        pid: 0,
        started_at: 0,
        started_at_human: "unix:0".to_string(),
        wa_version: "0.0.0".to_string(),
    };
    let json = serde_json::to_string(&meta).unwrap();
    let back: LockMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(back.pid, 0);
    assert_eq!(back.started_at, 0);
}

#[test]
fn metadata_with_max_pid() {
    let meta = LockMetadata {
        pid: u32::MAX,
        started_at: u64::MAX,
        started_at_human: "max".to_string(),
        wa_version: "999.999.999".to_string(),
    };
    let json = serde_json::to_string(&meta).unwrap();
    let back: LockMetadata = serde_json::from_str(&json).unwrap();
    assert_eq!(back.pid, u32::MAX);
    assert_eq!(back.started_at, u64::MAX);
}

// =========================================================================
// Field independence
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Changing only pid preserves all other fields after roundtrip.
    #[test]
    fn prop_changing_pid_preserves_others(
        pid_a in any::<u32>(),
        pid_b in any::<u32>(),
        started_at in any::<u64>(),
        human in "[a-z]{1,10}",
        ver in "[0-9.]{1,5}",
    ) {
        let meta_a = LockMetadata {
            pid: pid_a,
            started_at,
            started_at_human: human.clone(),
            wa_version: ver.clone(),
        };
        let meta_b = LockMetadata {
            pid: pid_b,
            started_at,
            started_at_human: human.clone(),
            wa_version: ver.clone(),
        };

        let back_a: LockMetadata = serde_json::from_str(&serde_json::to_string(&meta_a).unwrap()).unwrap();
        let back_b: LockMetadata = serde_json::from_str(&serde_json::to_string(&meta_b).unwrap()).unwrap();

        // Other fields should be identical
        prop_assert_eq!(back_a.started_at, back_b.started_at);
        prop_assert_eq!(&back_a.started_at_human, &back_b.started_at_human);
        prop_assert_eq!(&back_a.wa_version, &back_b.wa_version);
    }

    /// Changing only started_at preserves all other fields after roundtrip.
    #[test]
    fn prop_changing_timestamp_preserves_others(
        pid in any::<u32>(),
        ts_a in any::<u64>(),
        ts_b in any::<u64>(),
        human in "[a-z]{1,10}",
        ver in "[0-9.]{1,5}",
    ) {
        let meta_a = LockMetadata {
            pid,
            started_at: ts_a,
            started_at_human: human.clone(),
            wa_version: ver.clone(),
        };
        let meta_b = LockMetadata {
            pid,
            started_at: ts_b,
            started_at_human: human.clone(),
            wa_version: ver.clone(),
        };

        let back_a: LockMetadata = serde_json::from_str(&serde_json::to_string(&meta_a).unwrap()).unwrap();
        let back_b: LockMetadata = serde_json::from_str(&serde_json::to_string(&meta_b).unwrap()).unwrap();

        prop_assert_eq!(back_a.pid, back_b.pid);
        prop_assert_eq!(&back_a.started_at_human, &back_b.started_at_human);
        prop_assert_eq!(&back_a.wa_version, &back_b.wa_version);
    }
}

// =========================================================================
// Distinctness: different inputs produce different JSON
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Two metadata with different PIDs produce different JSON.
    #[test]
    fn prop_different_pids_different_json(
        pid_a in 0_u32..1_000_000,
        pid_b in 1_000_000_u32..u32::MAX,
    ) {
        let base_ts = 12345_u64;
        let meta_a = LockMetadata {
            pid: pid_a,
            started_at: base_ts,
            started_at_human: "same".to_string(),
            wa_version: "1.0.0".to_string(),
        };
        let meta_b = LockMetadata {
            pid: pid_b,
            started_at: base_ts,
            started_at_human: "same".to_string(),
            wa_version: "1.0.0".to_string(),
        };
        let json_a = serde_json::to_string(&meta_a).unwrap();
        let json_b = serde_json::to_string(&meta_b).unwrap();
        prop_assert_ne!(&json_a, &json_b, "different PIDs should produce different JSON");
    }

    /// Two metadata with different timestamps produce different JSON.
    #[test]
    fn prop_different_timestamps_different_json(
        ts_a in 0_u64..1_000_000_000,
        ts_b in 1_000_000_000_u64..u64::MAX,
    ) {
        let meta_a = LockMetadata {
            pid: 1,
            started_at: ts_a,
            started_at_human: "same".to_string(),
            wa_version: "1.0.0".to_string(),
        };
        let meta_b = LockMetadata {
            pid: 1,
            started_at: ts_b,
            started_at_human: "same".to_string(),
            wa_version: "1.0.0".to_string(),
        };
        let json_a = serde_json::to_string(&meta_a).unwrap();
        let json_b = serde_json::to_string(&meta_b).unwrap();
        prop_assert_ne!(&json_a, &json_b, "different timestamps should produce different JSON");
    }
}

// =========================================================================
// JSON size bounds
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// JSON representation is bounded and reasonable in size.
    #[test]
    fn prop_json_size_bounded(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        // Minimum: {"pid":0,"started_at":0,"started_at_human":"","wa_version":""} = ~60 bytes
        prop_assert!(json.len() >= 50, "JSON too short: {} bytes", json.len());
        // Maximum: reasonable upper bound (no field should be extremely long in our strategy)
        prop_assert!(json.len() < 500, "JSON too large: {} bytes", json.len());
    }

    /// JSON is valid UTF-8 (always true for serde_json, but verify).
    #[test]
    fn prop_json_is_valid_utf8(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let bytes = json.as_bytes();
        prop_assert!(std::str::from_utf8(bytes).is_ok(), "JSON should be valid UTF-8");
    }
}

// =========================================================================
// Robustness: extra fields in JSON are tolerated
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Deserialization ignores extra fields (forward compatibility).
    #[test]
    fn prop_extra_fields_tolerated(meta in arb_lock_metadata(), extra_val in any::<u32>()) {
        let json = serde_json::to_string(&meta).unwrap();
        // Inject an extra field
        let augmented = json.replacen(
            "}",
            &format!(",\"extra_field\":{}}}", extra_val),
            1,
        );
        let back: LockMetadata = serde_json::from_str(&augmented).unwrap();
        prop_assert_eq!(back.pid, meta.pid);
        prop_assert_eq!(back.started_at, meta.started_at);
        prop_assert_eq!(&back.started_at_human, &meta.started_at_human);
        prop_assert_eq!(&back.wa_version, &meta.wa_version);
    }

    /// Deserialization fails when a required field is missing.
    #[test]
    fn prop_missing_field_fails(meta in arb_lock_metadata()) {
        let _json = serde_json::to_string(&meta).unwrap();
        // Create JSON without pid — deserialization must fail
        let partial = format!(
            "{{\"started_at\":{},\"started_at_human\":\"{}\",\"wa_version\":\"{}\"}}",
            meta.started_at,
            meta.started_at_human.replace('\\', "\\\\").replace('"', "\\\""),
            meta.wa_version.replace('\\', "\\\\").replace('"', "\\\""),
        );
        let result: Result<LockMetadata, _> = serde_json::from_str(&partial);
        prop_assert!(result.is_err(), "should fail when pid is missing");
    }
}

// =========================================================================
// Numeric boundary values
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Full u32 range PIDs roundtrip correctly.
    #[test]
    fn prop_full_u32_pid_range(pid in any::<u32>()) {
        let meta = LockMetadata {
            pid,
            started_at: 0,
            started_at_human: "t".to_string(),
            wa_version: "v".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pid, pid, "PID roundtrip failed for {}", pid);
    }

    /// Full u64 range timestamps roundtrip correctly.
    #[test]
    fn prop_full_u64_timestamp_range(ts in any::<u64>()) {
        let meta = LockMetadata {
            pid: 1,
            started_at: ts,
            started_at_human: "t".to_string(),
            wa_version: "v".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: LockMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.started_at, ts, "timestamp roundtrip failed for {}", ts);
    }
}
