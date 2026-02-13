//! Property-based tests for the `lock` module.
//!
//! Covers `LockMetadata` serde roundtrips and field preservation.

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
        .prop_map(|(pid, started_at, started_at_human, wa_version)| LockMetadata {
            pid,
            started_at,
            started_at_human,
            wa_version,
        })
}

// =========================================================================
// LockMetadata — serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

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
    }

    /// JSON always contains expected field names.
    #[test]
    fn prop_metadata_json_has_fields(meta in arb_lock_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        prop_assert!(json.contains("\"pid\""), "should contain pid field");
        prop_assert!(json.contains("\"started_at\""), "should contain started_at field");
        prop_assert!(json.contains("\"wa_version\""), "should contain wa_version field");
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
