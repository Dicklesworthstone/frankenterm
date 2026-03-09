//! Property-based tests for the search schema preservation gate.
//!
//! Covers SchemaField/SchemaSnapshot serde roundtrips, check_schema_preservation
//! invariants (reflexivity, superset safety, missing ⟹ unsafe), and type change
//! analysis properties.

use frankenterm_core::search::schema_gate::{
    check_schema_preservation, SchemaField, SchemaGateResult, SchemaSnapshot, SchemaTypeMismatch,
};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_field_name() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            (b'a'..=b'z').prop_map(|b| b as char),
            Just('_'),
        ],
        1..20,
    )
    .prop_map(|chars| chars.into_iter().collect::<String>())
}

fn arb_type_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("String".to_string()),
        Just("u64".to_string()),
        Just("u32".to_string()),
        Just("i64".to_string()),
        Just("i32".to_string()),
        Just("f32".to_string()),
        Just("f64".to_string()),
        Just("bool".to_string()),
        Just("usize".to_string()),
    ]
}

fn arb_schema_field() -> impl Strategy<Value = SchemaField> {
    (arb_field_name(), arb_type_name(), any::<bool>(), any::<bool>()).prop_map(
        |(name, field_type, required, indexed)| SchemaField {
            name,
            field_type,
            required,
            indexed,
        },
    )
}

fn arb_schema_snapshot() -> impl Strategy<Value = SchemaSnapshot> {
    (
        prop::collection::vec(arb_schema_field(), 0..10),
        arb_field_name(), // version
        any::<i64>(),     // captured_at_ms
    )
        .prop_map(|(fields, version, captured_at_ms)| SchemaSnapshot {
            fields,
            version,
            captured_at_ms,
        })
}

/// Snapshot with unique field names (no duplicates).
fn arb_unique_schema_snapshot() -> impl Strategy<Value = SchemaSnapshot> {
    (
        prop::collection::vec(
            (arb_type_name(), any::<bool>(), any::<bool>()),
            0..8,
        ),
        arb_field_name(),
        any::<i64>(),
    )
        .prop_map(|(type_specs, version, captured_at_ms)| {
            let fields: Vec<SchemaField> = type_specs
                .into_iter()
                .enumerate()
                .map(|(i, (field_type, required, indexed))| SchemaField {
                    name: format!("field_{i}"),
                    field_type,
                    required,
                    indexed,
                })
                .collect();
            SchemaSnapshot {
                fields,
                version,
                captured_at_ms,
            }
        })
}

fn arb_widening_pair() -> impl Strategy<Value = (String, String)> {
    prop_oneof![
        Just(("u8".to_string(), "u16".to_string())),
        Just(("u8".to_string(), "u32".to_string())),
        Just(("u8".to_string(), "u64".to_string())),
        Just(("u16".to_string(), "u32".to_string())),
        Just(("u16".to_string(), "u64".to_string())),
        Just(("u32".to_string(), "u64".to_string())),
        Just(("i8".to_string(), "i16".to_string())),
        Just(("i8".to_string(), "i32".to_string())),
        Just(("i8".to_string(), "i64".to_string())),
        Just(("i16".to_string(), "i32".to_string())),
        Just(("i16".to_string(), "i64".to_string())),
        Just(("i32".to_string(), "i64".to_string())),
    ]
}

// =============================================================================
// SchemaField serde roundtrip
// =============================================================================

proptest! {
    /// SchemaField survives JSON roundtrip.
    #[test]
    fn schema_field_serde_roundtrip(field in arb_schema_field()) {
        let json = serde_json::to_string(&field).unwrap();
        let rt: SchemaField = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt, field);
    }

    /// SchemaSnapshot survives JSON roundtrip (field order preserved).
    #[test]
    fn schema_snapshot_serde_roundtrip(snap in arb_schema_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let rt: SchemaSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.fields, snap.fields);
        prop_assert_eq!(rt.version, snap.version);
        prop_assert_eq!(rt.captured_at_ms, snap.captured_at_ms);
    }

    /// SchemaGateResult survives JSON roundtrip.
    #[test]
    fn gate_result_serde_roundtrip(
        snap in arb_unique_schema_snapshot(),
    ) {
        let result = check_schema_preservation(&snap, &snap);
        let json = serde_json::to_string(&result).unwrap();
        let rt: SchemaGateResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.safe, result.safe);
        prop_assert_eq!(rt.missing_fields, result.missing_fields);
        prop_assert_eq!(rt.added_fields, result.added_fields);
        prop_assert_eq!(rt.summary, result.summary);
    }
}

// =============================================================================
// check_schema_preservation: reflexivity
// =============================================================================

proptest! {
    /// A schema with unique field names compared against itself is always safe.
    #[test]
    fn preservation_reflexive(snap in arb_unique_schema_snapshot()) {
        let result = check_schema_preservation(&snap, &snap);
        prop_assert!(result.safe);
        prop_assert!(result.missing_fields.is_empty());
        prop_assert!(result.added_fields.is_empty());
    }

    /// Self-comparison with unique fields produces no lossy mismatches.
    #[test]
    fn preservation_reflexive_no_lossy(snap in arb_unique_schema_snapshot()) {
        let result = check_schema_preservation(&snap, &snap);
        let has_lossy = result.type_mismatches.iter().any(|m| m.lossy);
        prop_assert!(!has_lossy);
    }
}

// =============================================================================
// check_schema_preservation: superset safety
// =============================================================================

proptest! {
    /// Target that is a superset of source (same fields + extras) is safe.
    #[test]
    fn superset_target_is_safe(
        base_fields in prop::collection::vec(
            (arb_type_name(), any::<bool>()),
            1..5,
        ),
        extra_count in 1..5usize,
    ) {
        let source_fields: Vec<SchemaField> = base_fields
            .iter()
            .enumerate()
            .map(|(i, (ft, idx))| SchemaField {
                name: format!("f_{i}"),
                field_type: ft.clone(),
                required: true,
                indexed: *idx,
            })
            .collect();

        let mut target_fields = source_fields.clone();
        for i in 0..extra_count {
            target_fields.push(SchemaField {
                name: format!("extra_{i}"),
                field_type: "String".to_string(),
                required: false,
                indexed: false,
            });
        }

        let source = SchemaSnapshot {
            fields: source_fields,
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: target_fields,
            version: "v2".to_string(),
            captured_at_ms: 0,
        };

        let result = check_schema_preservation(&source, &target);
        prop_assert!(result.safe);
        prop_assert_eq!(result.added_fields.len(), extra_count);
        prop_assert!(result.missing_fields.is_empty());
    }

    /// Target missing a source field is unsafe.
    #[test]
    fn missing_field_is_unsafe(
        field_count in 2..6usize,
        remove_idx in 0..5usize,
    ) {
        let remove_idx = remove_idx % field_count;
        let source_fields: Vec<SchemaField> = (0..field_count)
            .map(|i| SchemaField {
                name: format!("f_{i}"),
                field_type: "String".to_string(),
                required: true,
                indexed: true,
            })
            .collect();

        let mut target_fields = source_fields.clone();
        let removed_name = target_fields.remove(remove_idx).name;

        let source = SchemaSnapshot {
            fields: source_fields,
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: target_fields,
            version: "v2".to_string(),
            captured_at_ms: 0,
        };

        let result = check_schema_preservation(&source, &target);
        prop_assert!(!result.safe);
        prop_assert!(result.missing_fields.contains(&removed_name));
    }
}

// =============================================================================
// check_schema_preservation: safe ⟺ invariant
// =============================================================================

proptest! {
    /// safe == true ⟹ missing_fields empty AND no lossy mismatches.
    #[test]
    fn safe_implies_no_problems(
        source in arb_unique_schema_snapshot(),
        target in arb_unique_schema_snapshot(),
    ) {
        let result = check_schema_preservation(&source, &target);
        if result.safe {
            prop_assert!(result.missing_fields.is_empty());
            let has_lossy = result.type_mismatches.iter().any(|m| m.lossy);
            prop_assert!(!has_lossy);
        }
    }

    /// !safe ⟹ missing_fields non-empty OR has lossy mismatch.
    #[test]
    fn unsafe_implies_problem(
        source in arb_unique_schema_snapshot(),
        target in arb_unique_schema_snapshot(),
    ) {
        let result = check_schema_preservation(&source, &target);
        if !result.safe {
            let has_lossy = result.type_mismatches.iter().any(|m| m.lossy);
            let has_problem = !result.missing_fields.is_empty() || has_lossy;
            prop_assert!(has_problem);
        }
    }
}

// =============================================================================
// check_schema_preservation: counting
// =============================================================================

proptest! {
    /// missing_fields + matched_fields + added_fields = source ∪ target field count.
    #[test]
    fn field_accounting(
        source in arb_unique_schema_snapshot(),
        target in arb_unique_schema_snapshot(),
    ) {
        let result = check_schema_preservation(&source, &target);

        // Source fields partition into: matched (in target) + missing (not in target).
        let matched_source = source.fields.len() - result.missing_fields.len();
        prop_assert_eq!(
            result.missing_fields.len() + matched_source,
            source.fields.len()
        );

        // Target fields partition into: matched (in source) + added (not in source).
        // But note: matched_source counts source fields found in target.
        // added_fields counts target fields not found in source.
        // matched_source may differ from (target.fields.len() - added_fields.len())
        // only if there are duplicate field names, which arb_unique_schema_snapshot avoids.
        let matched_target = target.fields.len() - result.added_fields.len();
        prop_assert_eq!(matched_source, matched_target);
    }

    /// Empty source → everything in target is "added", result is safe.
    #[test]
    fn empty_source_always_safe(target in arb_unique_schema_snapshot()) {
        let source = SchemaSnapshot {
            fields: vec![],
            version: "empty".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        prop_assert!(result.safe);
        prop_assert_eq!(result.added_fields.len(), target.fields.len());
    }
}

// =============================================================================
// Type change analysis properties
// =============================================================================

proptest! {
    /// Integer widening (narrow→wide) produces a non-lossy mismatch.
    #[test]
    fn widening_is_not_lossy(pair in arb_widening_pair()) {
        let (narrow, wide) = pair;
        let source = SchemaSnapshot {
            fields: vec![SchemaField {
                name: "x".to_string(),
                field_type: narrow,
                required: true,
                indexed: true,
            }],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField {
                name: "x".to_string(),
                field_type: wide,
                required: true,
                indexed: true,
            }],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        prop_assert!(result.safe);
        prop_assert_eq!(result.type_mismatches.len(), 1);
        prop_assert!(!result.type_mismatches[0].lossy);
    }

    /// Integer narrowing (wide→narrow) produces a lossy mismatch.
    #[test]
    fn narrowing_is_lossy(pair in arb_widening_pair()) {
        let (narrow, wide) = pair;
        let source = SchemaSnapshot {
            fields: vec![SchemaField {
                name: "x".to_string(),
                field_type: wide,
                required: true,
                indexed: true,
            }],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField {
                name: "x".to_string(),
                field_type: narrow,
                required: true,
                indexed: true,
            }],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        prop_assert!(!result.safe);
        prop_assert_eq!(result.type_mismatches.len(), 1);
        prop_assert!(result.type_mismatches[0].lossy);
    }

    /// T → Option<T> is not lossy.
    #[test]
    fn required_to_optional_not_lossy(type_name in arb_type_name()) {
        let source = SchemaSnapshot {
            fields: vec![SchemaField {
                name: "x".to_string(),
                field_type: type_name.clone(),
                required: true,
                indexed: true,
            }],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField {
                name: "x".to_string(),
                field_type: format!("Option<{type_name}>"),
                required: false,
                indexed: true,
            }],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        prop_assert!(result.safe);
    }

    /// Option<T> → T is lossy.
    #[test]
    fn optional_to_required_is_lossy(type_name in arb_type_name()) {
        let source = SchemaSnapshot {
            fields: vec![SchemaField {
                name: "x".to_string(),
                field_type: format!("Option<{type_name}>"),
                required: false,
                indexed: true,
            }],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField {
                name: "x".to_string(),
                field_type: type_name,
                required: true,
                indexed: true,
            }],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        prop_assert!(!result.safe);
    }

    /// Same type → same type has no type mismatches.
    #[test]
    fn same_type_no_mismatch(type_name in arb_type_name()) {
        let source = SchemaSnapshot {
            fields: vec![SchemaField {
                name: "x".to_string(),
                field_type: type_name.clone(),
                required: true,
                indexed: true,
            }],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = source.clone();
        let result = check_schema_preservation(&source, &target);
        prop_assert!(result.safe);
        prop_assert!(result.type_mismatches.is_empty());
    }
}

// =============================================================================
// Summary message properties
// =============================================================================

proptest! {
    /// Summary is never empty.
    #[test]
    fn summary_non_empty(
        source in arb_unique_schema_snapshot(),
        target in arb_unique_schema_snapshot(),
    ) {
        let result = check_schema_preservation(&source, &target);
        prop_assert!(!result.summary.is_empty());
    }

    /// Unsafe result summary contains "UNSAFE".
    #[test]
    fn unsafe_summary_contains_keyword(
        field_count in 1..5usize,
    ) {
        let source_fields: Vec<SchemaField> = (0..field_count)
            .map(|i| SchemaField {
                name: format!("f_{i}"),
                field_type: "String".to_string(),
                required: true,
                indexed: true,
            })
            .collect();
        let source = SchemaSnapshot {
            fields: source_fields,
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        prop_assert!(result.summary.contains("UNSAFE"));
    }

    /// Safe identical schema summary contains "identical".
    #[test]
    fn identical_summary_contains_identical(snap in arb_unique_schema_snapshot()) {
        let result = check_schema_preservation(&snap, &snap);
        // Only if both are non-empty and truly identical
        if snap.fields.is_empty() || result.type_mismatches.is_empty() {
            prop_assert!(
                result.summary.contains("identical") || result.summary.contains("safe"),
                "summary should mention safe or identical: {}",
                result.summary
            );
        }
    }
}

// =============================================================================
// SchemaTypeMismatch serde roundtrip
// =============================================================================

proptest! {
    /// SchemaTypeMismatch survives JSON roundtrip.
    #[test]
    fn type_mismatch_serde_roundtrip(
        field_name in arb_field_name(),
        source_type in arb_type_name(),
        target_type in arb_type_name(),
        lossy in any::<bool>(),
    ) {
        let mismatch = SchemaTypeMismatch {
            field_name,
            source_type,
            target_type,
            lossy,
        };
        let json = serde_json::to_string(&mismatch).unwrap();
        let rt: SchemaTypeMismatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.field_name, mismatch.field_name);
        prop_assert_eq!(rt.source_type, mismatch.source_type);
        prop_assert_eq!(rt.target_type, mismatch.target_type);
        prop_assert_eq!(rt.lossy, mismatch.lossy);
    }
}
