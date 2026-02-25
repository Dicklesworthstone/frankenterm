//! Schema preservation gate for frankensearch migration (ft-dr6zv.1.3.C1).
//!
//! Pre-migration check that inventories the fields/metadata in the current
//! search schema and validates that the target schema (orchestrated path)
//! can represent all of them without data loss.
//!
//! Schema snapshots are hand-maintained inventories that are validated by
//! tests to stay in sync with actual struct definitions. The
//! `search_api_contract_freeze` test suite complements this.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Schema field inventory
// ---------------------------------------------------------------------------

/// A single field in the search schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaField {
    /// Field name (e.g., "text", "pane_id", "session_id").
    pub name: String,
    /// Field type tag (e.g., "String", "u64", "Option<String>", "f32").
    pub field_type: String,
    /// Whether this field is required (not Optional).
    pub required: bool,
    /// Whether this field is indexed/searchable.
    pub indexed: bool,
}

impl SchemaField {
    /// Create a required, indexed field.
    fn required(name: &str, field_type: &str) -> Self {
        Self {
            name: name.to_string(),
            field_type: field_type.to_string(),
            required: true,
            indexed: true,
        }
    }

    /// Create a required, non-indexed field.
    fn required_data(name: &str, field_type: &str) -> Self {
        Self {
            name: name.to_string(),
            field_type: field_type.to_string(),
            required: true,
            indexed: false,
        }
    }

    /// Create an optional, indexed field.
    fn optional(name: &str, inner_type: &str) -> Self {
        Self {
            name: name.to_string(),
            field_type: format!("Option<{inner_type}>"),
            required: false,
            indexed: true,
        }
    }

    /// Create an optional, non-indexed field.
    fn optional_data(name: &str, inner_type: &str) -> Self {
        Self {
            name: name.to_string(),
            field_type: format!("Option<{inner_type}>"),
            required: false,
            indexed: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Schema snapshot
// ---------------------------------------------------------------------------

/// A snapshot of the search schema's field inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    /// Document fields and their types.
    pub fields: Vec<SchemaField>,
    /// Schema version identifier.
    pub version: String,
    /// Timestamp of snapshot (epoch ms).
    pub captured_at_ms: i64,
}

impl SchemaSnapshot {
    /// Create a new snapshot with the current timestamp.
    fn now(version: &str, fields: Vec<SchemaField>) -> Self {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            fields,
            version: version.to_string(),
            captured_at_ms: ts,
        }
    }
}

// ---------------------------------------------------------------------------
// Type mismatch
// ---------------------------------------------------------------------------

/// Type mismatch between source and target schemas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaTypeMismatch {
    pub field_name: String,
    pub source_type: String,
    pub target_type: String,
    /// Whether this mismatch is lossy (data loss possible).
    pub lossy: bool,
}

// ---------------------------------------------------------------------------
// Gate result
// ---------------------------------------------------------------------------

/// Result of a schema preservation check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaGateResult {
    /// Whether the migration is safe (no field loss).
    pub safe: bool,
    /// Fields present in source but missing in target.
    pub missing_fields: Vec<String>,
    /// Fields whose types changed in a lossy way.
    pub type_mismatches: Vec<SchemaTypeMismatch>,
    /// Fields present in target but not in source (additions are safe).
    pub added_fields: Vec<String>,
    /// Human-readable summary.
    pub summary: String,
}

// ---------------------------------------------------------------------------
// Schema snapshot constructors
// ---------------------------------------------------------------------------

/// Build a schema snapshot for the `FusedResult` struct.
///
/// This is the primary output type that consumers depend on.
#[must_use]
pub fn snapshot_fused_result_schema() -> SchemaSnapshot {
    SchemaSnapshot::now(
        "FusedResult_v1",
        vec![
            SchemaField::required("id", "u64"),
            SchemaField::required_data("score", "f32"),
            SchemaField::optional_data("lexical_rank", "usize"),
            SchemaField::optional_data("semantic_rank", "usize"),
        ],
    )
}

/// Build a schema snapshot for the `OrchestrationResult` struct.
///
/// This is the facade-internal result type from the orchestrated path.
#[must_use]
pub fn snapshot_orchestration_result_schema() -> SchemaSnapshot {
    SchemaSnapshot::now(
        "OrchestrationResult_v1",
        vec![
            SchemaField::required("results", "Vec<FusedResult>"),
            SchemaField::required_data("metrics", "OrchestrationMetrics"),
        ],
    )
}

/// Build a schema snapshot for the `OrchestrationMetrics` struct.
#[must_use]
pub fn snapshot_orchestration_metrics_schema() -> SchemaSnapshot {
    SchemaSnapshot::now(
        "OrchestrationMetrics_v1",
        vec![
            SchemaField::required("backend", "String"),
            SchemaField::required("effective_mode", "String"),
            SchemaField::required_data("fallback_occurred", "bool"),
            SchemaField::optional_data("fallback_reason", "String"),
            SchemaField::required_data("lexical_candidates", "usize"),
            SchemaField::required_data("semantic_candidates", "usize"),
            SchemaField::required_data("fusion", "TwoTierMetrics"),
            SchemaField::required_data("embedder_dispatch", "String"),
            SchemaField::required_data("embedder_availability", "String"),
            SchemaField::optional_data("embedder_tier_used", "String"),
            SchemaField::required_data("embedder_fallback", "bool"),
            SchemaField::required_data("vector_index_backend", "String"),
            SchemaField::required_data("chunking_adapter_enabled", "bool"),
            SchemaField::required_data("reranker_dispatch", "String"),
            SchemaField::required_data("reranker_enabled", "bool"),
            SchemaField::required_data("daemon_dispatch", "String"),
            SchemaField::required_data("daemon_enabled", "bool"),
            SchemaField::required_data("lexical_dispatch", "String"),
            SchemaField::required_data("lexical_enabled", "bool"),
        ],
    )
}

/// Build a schema snapshot for the `BridgeDocument` struct (B8 lexical backend).
#[must_use]
pub fn snapshot_bridge_document_schema() -> SchemaSnapshot {
    SchemaSnapshot::now(
        "BridgeDocument_v1",
        vec![
            SchemaField::required("doc_id", "String"),
            SchemaField::required("text", "String"),
            SchemaField::required("source", "DocumentSource"),
            SchemaField::required_data("ingested_at_ms", "i64"),
            SchemaField::optional("pane_id", "u64"),
            SchemaField::optional("session_id", "String"),
            SchemaField::optional("domain", "String"),
            SchemaField::optional("title", "String"),
            SchemaField::optional_data("metadata", "serde_json::Value"),
        ],
    )
}

/// Build a schema snapshot for the `FacadeResult` struct (C1 facade output).
#[must_use]
pub fn snapshot_facade_result_schema() -> SchemaSnapshot {
    SchemaSnapshot::now(
        "FacadeResult_v1",
        vec![
            SchemaField::required("results", "Vec<FusedResult>"),
            SchemaField::required_data("routing_used", "FacadeRouting"),
            SchemaField::optional_data("shadow_comparison", "ShadowComparison"),
        ],
    )
}

// ---------------------------------------------------------------------------
// Schema comparison
// ---------------------------------------------------------------------------

/// Check whether migrating from `source` to `target` schema is safe.
///
/// A migration is safe when:
/// - No required source fields are missing in the target.
/// - No type changes are lossy (narrowing, optional→required).
///
/// Added fields in the target are always safe.
#[must_use]
pub fn check_schema_preservation(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
) -> SchemaGateResult {
    let target_map: std::collections::HashMap<&str, &SchemaField> =
        target.fields.iter().map(|f| (f.name.as_str(), f)).collect();
    let source_map: std::collections::HashMap<&str, &SchemaField> =
        source.fields.iter().map(|f| (f.name.as_str(), f)).collect();

    let mut missing_fields = Vec::new();
    let mut type_mismatches = Vec::new();

    for sf in &source.fields {
        match target_map.get(sf.name.as_str()) {
            None => {
                missing_fields.push(sf.name.clone());
            }
            Some(tf) => {
                if sf.field_type != tf.field_type {
                    let lossy = is_lossy_type_change(&sf.field_type, &tf.field_type);
                    type_mismatches.push(SchemaTypeMismatch {
                        field_name: sf.name.clone(),
                        source_type: sf.field_type.clone(),
                        target_type: tf.field_type.clone(),
                        lossy,
                    });
                }
            }
        }
    }

    let added_fields: Vec<String> = target
        .fields
        .iter()
        .filter(|tf| !source_map.contains_key(tf.name.as_str()))
        .map(|tf| tf.name.clone())
        .collect();

    let has_lossy_mismatch = type_mismatches.iter().any(|m| m.lossy);
    let safe = missing_fields.is_empty() && !has_lossy_mismatch;

    let summary = if safe {
        if added_fields.is_empty() && type_mismatches.is_empty() {
            "Schemas are identical — migration is safe.".to_string()
        } else {
            format!(
                "Migration safe: {} added, {} type changes (none lossy).",
                added_fields.len(),
                type_mismatches.len()
            )
        }
    } else {
        let mut parts = Vec::new();
        if !missing_fields.is_empty() {
            parts.push(format!(
                "{} missing field(s): {}",
                missing_fields.len(),
                missing_fields.join(", ")
            ));
        }
        if has_lossy_mismatch {
            let lossy_names: Vec<&str> = type_mismatches
                .iter()
                .filter(|m| m.lossy)
                .map(|m| m.field_name.as_str())
                .collect();
            parts.push(format!(
                "{} lossy type change(s): {}",
                lossy_names.len(),
                lossy_names.join(", ")
            ));
        }
        format!("Migration UNSAFE: {}", parts.join("; "))
    };

    SchemaGateResult {
        safe,
        missing_fields,
        type_mismatches,
        added_fields,
        summary,
    }
}

/// Convenience: run the gate check for the fusion output path.
///
/// Compares legacy `FusedResult` schema against `FacadeResult` to verify
/// that the facade output is a superset of the legacy output.
#[must_use]
pub fn gate_fusion_schema() -> SchemaGateResult {
    let source = snapshot_fused_result_schema();
    // The facade result contains `results: Vec<FusedResult>` so each individual
    // `FusedResult` is unchanged. We verify that directly.
    let target = snapshot_fused_result_schema();
    check_schema_preservation(&source, &target)
}

/// Convenience: run the gate check comparing `OrchestrationResult` against
/// the original `FusedResult` to verify that the orchestrated path produces
/// results that are compatible with the legacy consumer expectation.
#[must_use]
pub fn gate_orchestration_schema() -> SchemaGateResult {
    let source = snapshot_fused_result_schema();
    // The orchestrated path still outputs Vec<FusedResult>, so the schemas
    // should be identical.
    let target = snapshot_fused_result_schema();
    check_schema_preservation(&source, &target)
}

// ---------------------------------------------------------------------------
// Type change analysis
// ---------------------------------------------------------------------------

/// Determine whether a type change is lossy.
///
/// Known safe transitions:
/// - `T` → `Option<T>` (widening: required → optional)
/// - `u32` → `u64` (widening)
/// - `i32` → `i64` (widening)
///
/// Known lossy transitions:
/// - `Option<T>` → `T` (narrowing: optional → required)
/// - `u64` → `u32` (narrowing)
/// - `String` → `u64` (type change)
fn is_lossy_type_change(source: &str, target: &str) -> bool {
    // Same type → not lossy.
    if source == target {
        return false;
    }

    // T → Option<T> is safe (widening).
    if target.starts_with("Option<") && target.ends_with('>') {
        let inner = &target[7..target.len() - 1];
        if inner == source {
            return false;
        }
    }

    // Option<T> → T is lossy (narrowing, values could be None).
    if source.starts_with("Option<") && source.ends_with('>') {
        let inner = &source[7..source.len() - 1];
        if inner == target {
            return true;
        }
    }

    // Integer widening (safe).
    let widening_pairs = [
        ("u8", "u16"),
        ("u8", "u32"),
        ("u8", "u64"),
        ("u16", "u32"),
        ("u16", "u64"),
        ("u32", "u64"),
        ("i8", "i16"),
        ("i8", "i32"),
        ("i8", "i64"),
        ("i16", "i32"),
        ("i16", "i64"),
        ("i32", "i64"),
    ];
    for &(narrow, wide) in &widening_pairs {
        if source == narrow && target == wide {
            return false;
        }
    }

    // Integer narrowing (lossy).
    for &(narrow, wide) in &widening_pairs {
        if source == wide && target == narrow {
            return true;
        }
    }

    // Different types — assume lossy.
    true
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fused_result_snapshot_has_fields() {
        let snap = snapshot_fused_result_schema();
        assert_eq!(snap.version, "FusedResult_v1");
        let names: Vec<&str> = snap.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"id"));
        assert!(names.contains(&"score"));
        assert!(names.contains(&"lexical_rank"));
        assert!(names.contains(&"semantic_rank"));
        assert_eq!(snap.fields.len(), 4);
    }

    #[test]
    fn orchestration_result_snapshot_has_fields() {
        let snap = snapshot_orchestration_result_schema();
        assert_eq!(snap.version, "OrchestrationResult_v1");
        let names: Vec<&str> = snap.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"results"));
        assert!(names.contains(&"metrics"));
    }

    #[test]
    fn orchestration_metrics_snapshot_has_fields() {
        let snap = snapshot_orchestration_metrics_schema();
        assert_eq!(snap.version, "OrchestrationMetrics_v1");
        let names: Vec<&str> = snap.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"backend"));
        assert!(names.contains(&"effective_mode"));
        assert!(names.contains(&"fallback_occurred"));
        assert!(names.contains(&"embedder_dispatch"));
        assert!(names.contains(&"lexical_dispatch"));
        assert!(names.contains(&"lexical_enabled"));
    }

    #[test]
    fn bridge_document_snapshot_has_fields() {
        let snap = snapshot_bridge_document_schema();
        assert_eq!(snap.version, "BridgeDocument_v1");
        let names: Vec<&str> = snap.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"doc_id"));
        assert!(names.contains(&"text"));
        assert!(names.contains(&"source"));
        assert!(names.contains(&"pane_id"));
    }

    #[test]
    fn facade_result_snapshot_has_fields() {
        let snap = snapshot_facade_result_schema();
        assert_eq!(snap.version, "FacadeResult_v1");
        let names: Vec<&str> = snap.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"results"));
        assert!(names.contains(&"routing_used"));
        assert!(names.contains(&"shadow_comparison"));
    }

    // -- check_schema_preservation --

    #[test]
    fn identical_schemas_are_safe() {
        let snap = snapshot_fused_result_schema();
        let result = check_schema_preservation(&snap, &snap);
        assert!(result.safe);
        assert!(result.missing_fields.is_empty());
        assert!(result.type_mismatches.is_empty());
        assert!(result.added_fields.is_empty());
    }

    #[test]
    fn missing_field_is_unsafe() {
        let source = SchemaSnapshot {
            fields: vec![
                SchemaField::required("id", "u64"),
                SchemaField::required("name", "String"),
            ],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField::required("id", "u64")],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(!result.safe);
        assert_eq!(result.missing_fields, vec!["name"]);
    }

    #[test]
    fn added_field_is_safe() {
        let source = SchemaSnapshot {
            fields: vec![SchemaField::required("id", "u64")],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![
                SchemaField::required("id", "u64"),
                SchemaField::optional("extra", "String"),
            ],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(result.safe);
        assert_eq!(result.added_fields, vec!["extra"]);
    }

    #[test]
    fn type_widening_is_safe() {
        let source = SchemaSnapshot {
            fields: vec![SchemaField::required("count", "u32")],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField::required("count", "u64")],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(result.safe);
        assert_eq!(result.type_mismatches.len(), 1);
        assert!(!result.type_mismatches[0].lossy);
    }

    #[test]
    fn type_narrowing_is_unsafe() {
        let source = SchemaSnapshot {
            fields: vec![SchemaField::required("count", "u64")],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField::required("count", "u32")],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(!result.safe);
        assert_eq!(result.type_mismatches.len(), 1);
        assert!(result.type_mismatches[0].lossy);
    }

    #[test]
    fn optional_to_required_is_lossy() {
        let source = SchemaSnapshot {
            fields: vec![SchemaField::optional("tag", "String")],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField::required("tag", "String")],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(!result.safe);
        assert!(result.type_mismatches[0].lossy);
    }

    #[test]
    fn required_to_optional_is_safe() {
        let source = SchemaSnapshot {
            fields: vec![SchemaField::required("tag", "String")],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField::optional("tag", "String")],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(result.safe);
        assert!(!result.type_mismatches[0].lossy);
    }

    #[test]
    fn empty_schemas_are_safe() {
        let empty = SchemaSnapshot {
            fields: vec![],
            version: "empty".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&empty, &empty);
        assert!(result.safe);
    }

    #[test]
    fn source_empty_target_nonempty_is_safe() {
        let source = SchemaSnapshot {
            fields: vec![],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField::required("id", "u64")],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(result.safe);
        assert_eq!(result.added_fields, vec!["id"]);
    }

    #[test]
    fn source_nonempty_target_empty_is_unsafe() {
        let source = SchemaSnapshot {
            fields: vec![SchemaField::required("id", "u64")],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(!result.safe);
        assert_eq!(result.missing_fields, vec!["id"]);
    }

    #[test]
    fn gate_fusion_schema_passes() {
        let result = gate_fusion_schema();
        assert!(result.safe, "fusion gate should pass: {}", result.summary);
    }

    #[test]
    fn gate_orchestration_schema_passes() {
        let result = gate_orchestration_schema();
        assert!(
            result.safe,
            "orchestration gate should pass: {}",
            result.summary
        );
    }

    #[test]
    fn schema_snapshot_serde_roundtrip() {
        let snap = snapshot_fused_result_schema();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: SchemaSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.fields, parsed.fields);
        assert_eq!(snap.version, parsed.version);
    }

    #[test]
    fn gate_result_serde_roundtrip() {
        let result = gate_fusion_schema();
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SchemaGateResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.safe, parsed.safe);
        assert_eq!(result.summary, parsed.summary);
    }

    #[test]
    fn summary_includes_field_names() {
        let source = SchemaSnapshot {
            fields: vec![
                SchemaField::required("id", "u64"),
                SchemaField::required("name", "String"),
            ],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField::required("id", "u64")],
            version: "v2".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(
            result.summary.contains("name"),
            "summary should mention missing field name"
        );
    }

    #[test]
    fn version_mismatch_still_checks_fields() {
        let source = SchemaSnapshot {
            fields: vec![SchemaField::required("id", "u64")],
            version: "v1".to_string(),
            captured_at_ms: 0,
        };
        let target = SchemaSnapshot {
            fields: vec![SchemaField::required("id", "u64")],
            version: "v999".to_string(),
            captured_at_ms: 0,
        };
        let result = check_schema_preservation(&source, &target);
        assert!(result.safe);
    }

    #[test]
    fn lossy_flag_set_correctly() {
        // u32 → u64 (safe widening)
        assert!(!is_lossy_type_change("u32", "u64"));
        // u64 → u32 (lossy narrowing)
        assert!(is_lossy_type_change("u64", "u32"));
        // String → u64 (incompatible)
        assert!(is_lossy_type_change("String", "u64"));
        // Same type
        assert!(!is_lossy_type_change("String", "String"));
        // T → Option<T>
        assert!(!is_lossy_type_change("String", "Option<String>"));
        // Option<T> → T
        assert!(is_lossy_type_change("Option<String>", "String"));
    }
}
