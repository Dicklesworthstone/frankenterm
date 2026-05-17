//! Golden tests for JSON Schema validation and docs generation determinism.
//!
//! Validates that:
//! 1. All hand-authored JSON Schema files are valid JSON
//! 2. Schema files have required JSON Schema fields
//! 3. SchemaRegistry covers all on-disk schemas (no orphans)
//! 4. Every registry endpoint has a corresponding schema file
//! 5. Docs generation is deterministic (same input → same output)
//! 6. Generated reference has expected structural elements
//!
//! # Related Beads
//!
//! - wa-upg.10.5: Tests: schema validation + docs generation golden tests
//! - wa-upg.10.1: Schema-driven API strategy
//! - wa-upg.10.2: Schema-driven docs generator

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;

use frankenterm_core::api_schema::SchemaRegistry;
use frankenterm_core::auto_tune::{
    AUTO_TUNE_DECISION_RECORD_SCHEMA_VERSION, KnobGate, TunableKnobId, TuningConfidenceState,
    TuningDecisionKind, TuningDecisionRecord, TuningMetricWindowSummary, TuningMode,
};
use frankenterm_core::docs_gen::{
    DocGenConfig, EndpointCategory, categorize_endpoint, generate_endpoint_summary,
    generate_reference, parse_schema,
};
use frankenterm_core::fleet_memory_controller::{
    FleetMemoryTier, FleetMemoryTierReclamationAction, FleetPressureTier,
};
use frankenterm_core::memory_pressure::MacosResidencyBucket;
use frankenterm_core::runtime_telemetry::{
    SWARM_RESOURCE_COCKPIT_CONTRACT_ID, SWARM_RESOURCE_COCKPIT_SCHEMA_VERSION,
    SwarmCapacityAdmissionAction, SwarmCapacityCertificateStatus,
    SwarmCapacityOperatorDecisionSummary, SwarmCapacityOperatorStatus,
    SwarmCapacityOperatorSummary, SwarmCapacityStage, SwarmCapacityWorkClass,
    SwarmResourceCockpitActionReceipt, SwarmResourceCockpitAdmissionCounters,
    SwarmResourceCockpitAdmissionDecision, SwarmResourceCockpitDomainSummary,
    SwarmResourceCockpitDomains, SwarmResourceCockpitDrilldown,
    SwarmResourceCockpitEvidenceFreshness, SwarmResourceCockpitEvidenceState,
    SwarmResourceCockpitHardwarePredicate, SwarmResourceCockpitLatencyCohort,
    SwarmResourceCockpitMemoryTierSummary, SwarmResourceCockpitProofGate,
    SwarmResourceCockpitQueueBackpressureSummary, SwarmResourceCockpitResidencyBucket,
    SwarmResourceCockpitRunIdentity, SwarmResourceCockpitSnapshot, SwarmTailRiskStatus,
};
use frankenterm_core::storage::io_scheduler::{
    StorageIoClass, StorageIoDominantClassSummary, StorageIoOperatorSummary, StorageIoPressureTier,
};
use frankenterm_core::swarm_scheduler::{
    AdmissionAction, AdmissionDecisionCounters, AdmissionReasonCode,
    ResourceAdmissionDecisionSummary,
};
use jsonschema::Validator;
use serde_json::{Value, json};

/// Workspace root: two levels up from crate manifest dir.
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root exists")
        .to_path_buf()
}

/// Path to docs/json-schema/ directory.
fn schema_dir() -> PathBuf {
    workspace_root().join("docs").join("json-schema")
}

/// Path to docs/json-schema/PROVENANCE.md.
fn schema_provenance_path() -> PathBuf {
    schema_dir().join("PROVENANCE.md")
}

/// Load all .json files from docs/json-schema/.
fn load_all_schemas() -> Vec<(String, Value)> {
    let dir = schema_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut schemas: Vec<(String, Value)> = fs::read_dir(&dir)
        .expect("read schema dir")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                let content = fs::read_to_string(&path).ok()?;
                let value: Value = serde_json::from_str(&content).ok()?;
                Some((name, value))
            } else {
                None
            }
        })
        .collect();

    schemas.sort_by(|a, b| a.0.cmp(&b.0));
    schemas
}

fn load_schema(name: &str) -> Value {
    let path = schema_dir().join(name);
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read schema {}: {err}", path.display()));
    serde_json::from_str(&content)
        .unwrap_or_else(|err| panic!("failed to parse schema {}: {err}", path.display()))
}

fn compile_draft_2020_schema(schema: &Value) -> Validator {
    jsonschema::draft202012::options()
        .build(schema)
        .expect("resource cockpit schema compiles as Draft 2020-12")
}

fn assert_schema_accepts(label: &str, validator: &Validator, value: &Value) {
    if let Err(errors) = validator.validate(value) {
        let messages = errors
            .map(|error| format!("{}: {}", error.instance_path, error))
            .collect::<Vec<_>>()
            .join("\n");
        panic!("{label} did not match resource cockpit schema:\n{messages}");
    }
}

// ─────────────────────────────────────────────────────────────────────
// Schema file validation
// ─────────────────────────────────────────────────────────────────────

#[test]
fn all_schema_files_are_valid_json() {
    let dir = schema_dir();
    if !dir.exists() {
        return; // Skip if schemas dir doesn't exist (CI without full checkout)
    }

    let entries: Vec<_> = fs::read_dir(&dir)
        .expect("read schema dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .collect();

    assert!(!entries.is_empty(), "schema dir should not be empty");

    for entry in entries {
        let path = entry.path();
        let content = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("Failed to read {}: {e}", path.display());
        });
        let _: Value = serde_json::from_str(&content).unwrap_or_else(|e| {
            panic!(
                "Invalid JSON in {}: {e}",
                entry.file_name().to_string_lossy()
            );
        });
    }
}

#[test]
fn schema_files_have_required_fields() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        // Every schema should have a title
        assert!(
            schema.get("title").and_then(Value::as_str).is_some(),
            "{name} missing 'title'"
        );

        // Every schema should have a description
        assert!(
            schema.get("description").and_then(Value::as_str).is_some(),
            "{name} missing 'description'"
        );

        let schema_type = schema.get("type").and_then(Value::as_str);
        if name == "wa-robot-state.json" {
            assert_eq!(
                schema_type,
                Some("array"),
                "{name} should have type 'array'"
            );
        } else if name != "wa-robot-envelope.json" && name != "wa-mcp-envelope.json" {
            assert_eq!(
                schema_type,
                Some("object"),
                "{name} should have type 'object'"
            );
        }
    }
}

#[test]
fn schema_files_use_json_schema_draft() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        let draft = schema.get("$schema").and_then(Value::as_str);
        assert!(
            draft.is_some(),
            "{name} missing '$schema' (JSON Schema draft)"
        );
        assert!(
            draft.unwrap().contains("json-schema.org"),
            "{name} has unexpected $schema: {}",
            draft.unwrap()
        );
    }
}

#[test]
fn schema_files_have_id() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        let id = schema.get("$id").and_then(Value::as_str);
        assert!(id.is_some(), "{name} missing '$id'");
        let id = id.unwrap();
        let expected_domain = if name == "ft-config.json"
            || name == "ft-pattern-pack.json"
            || name == "ft-resource-pressure-cockpit.json"
            || name == "ft-swarm-capacity-signal-inventory.json"
        {
            "frankenterm.dev"
        } else {
            "wezterm-automata.dev"
        };
        assert!(
            id.contains(expected_domain),
            "{name} has unexpected $id domain: {id}"
        );
    }
}

#[test]
fn schema_provenance_covers_all_disk_schemas() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    let path = schema_provenance_path();
    let provenance = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "failed to read schema provenance at {}: {err}",
            path.display()
        )
    });

    let mut documented = HashSet::new();
    for line in provenance.lines() {
        let Some(rest) = line.trim().strip_prefix("| `") else {
            continue;
        };
        let Some((name, _)) = rest.split_once("` |") else {
            continue;
        };
        if std::path::Path::new(name)
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            documented.insert(name.to_string());
        }
    }

    let disk_names: HashSet<String> = schemas.iter().map(|(name, _)| name.clone()).collect();

    let mut missing: Vec<String> = disk_names
        .difference(&documented)
        .map(ToString::to_string)
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "schema files missing PROVENANCE.md entries: {missing:?}"
    );

    let mut stale: Vec<String> = documented
        .difference(&disk_names)
        .map(ToString::to_string)
        .collect();
    stale.sort();
    assert!(
        stale.is_empty(),
        "PROVENANCE.md entries without matching schema files: {stale:?}"
    );
}

#[test]
fn schema_files_no_additional_properties_leak() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    // The envelope schema uses conditional validation (if/then/else). The
    // operator config schema intentionally permits unknown subsection keys so
    // new runtime knobs do not make older config files invalid.
    let skip = ["wa-robot-envelope.json", "ft-config.json"];

    for (name, schema) in &schemas {
        if skip.contains(&name.as_str()) {
            continue;
        }
        if schema.get("properties").is_some() {
            let ap = schema.get("additionalProperties");
            assert!(
                ap.is_some(),
                "{name} has properties but no 'additionalProperties' field"
            );
            if let Some(ap_val) = ap {
                assert_eq!(
                    ap_val,
                    &Value::Bool(false),
                    "{name}: 'additionalProperties' should be false"
                );
            }
        }
    }
}

fn resource_cockpit_domain(
    name: &str,
    evidence_state: SwarmResourceCockpitEvidenceState,
    pressure_tier: &str,
    reason_code: &str,
) -> SwarmResourceCockpitDomainSummary {
    let freshness = (evidence_state != SwarmResourceCockpitEvidenceState::Measured).then(|| {
        SwarmResourceCockpitEvidenceFreshness {
            state: evidence_state,
            source: "schema_golden.resource_cockpit_fixture".to_string(),
            generated_at_ms: Some(1_700_000_050_000),
            freshness_ms: Some(0),
            max_age_ms: Some(60_000),
            reason_codes: vec![reason_code.to_string()],
        }
    });

    SwarmResourceCockpitDomainSummary {
        name: name.to_string(),
        evidence_state,
        pressure_tier: pressure_tier.to_string(),
        summary: format!("{name} fixture summary"),
        operator_action: if pressure_tier == "normal" || pressure_tier == "green" {
            "none".to_string()
        } else {
            format!("inspect_{name}")
        },
        reason_codes: vec![reason_code.to_string()],
        freshness,
        metrics: BTreeMap::from([("fixture".to_string(), json!(true))]),
    }
}

fn resource_cockpit_domains(
    evidence_state: SwarmResourceCockpitEvidenceState,
    pressure_tier: &str,
) -> SwarmResourceCockpitDomains {
    let reason_code = if evidence_state == SwarmResourceCockpitEvidenceState::Measured {
        "resource.proof.healthy"
    } else {
        "resource.telemetry.simulated"
    };
    SwarmResourceCockpitDomains {
        memory: resource_cockpit_domain("memory", evidence_state, pressure_tier, reason_code),
        rss_residency: resource_cockpit_domain(
            "rss_residency",
            evidence_state,
            pressure_tier,
            reason_code,
        ),
        pane_budget: resource_cockpit_domain(
            "pane_budget",
            evidence_state,
            pressure_tier,
            reason_code,
        ),
        queue_backpressure: resource_cockpit_domain(
            "queue_backpressure",
            evidence_state,
            pressure_tier,
            reason_code,
        ),
        storage_io: resource_cockpit_domain(
            "storage_io",
            evidence_state,
            pressure_tier,
            reason_code,
        ),
        worker_pool: resource_cockpit_domain(
            "worker_pool",
            evidence_state,
            pressure_tier,
            reason_code,
        ),
        capacity_admission: resource_cockpit_domain(
            "capacity_admission",
            evidence_state,
            pressure_tier,
            reason_code,
        ),
        resource_admission: resource_cockpit_domain(
            "resource_admission",
            evidence_state,
            pressure_tier,
            reason_code,
        ),
        action_receipts: resource_cockpit_domain(
            "action_receipts",
            evidence_state,
            pressure_tier,
            reason_code,
        ),
    }
}

fn resource_cockpit_mixed_domains() -> SwarmResourceCockpitDomains {
    SwarmResourceCockpitDomains {
        memory: resource_cockpit_domain(
            "memory",
            SwarmResourceCockpitEvidenceState::Measured,
            "elevated",
            "resource.memory.tier_pressure",
        ),
        rss_residency: resource_cockpit_domain(
            "rss_residency",
            SwarmResourceCockpitEvidenceState::Stale,
            "yellow",
            "resource.telemetry.stale",
        ),
        pane_budget: resource_cockpit_domain(
            "pane_budget",
            SwarmResourceCockpitEvidenceState::Unavailable,
            "unknown",
            "resource.telemetry.unavailable",
        ),
        queue_backpressure: resource_cockpit_domain(
            "queue_backpressure",
            SwarmResourceCockpitEvidenceState::Measured,
            "red",
            "queue_saturated",
        ),
        storage_io: resource_cockpit_domain(
            "storage_io",
            SwarmResourceCockpitEvidenceState::Measured,
            "red",
            "storage_io.write_error.io_error",
        ),
        worker_pool: resource_cockpit_domain(
            "worker_pool",
            SwarmResourceCockpitEvidenceState::Unavailable,
            "unknown",
            "worker_pool.stale_inventory",
        ),
        capacity_admission: resource_cockpit_domain(
            "capacity_admission",
            SwarmResourceCockpitEvidenceState::Measured,
            "elevated",
            "capacity.operator.watch",
        ),
        resource_admission: resource_cockpit_domain(
            "resource_admission",
            SwarmResourceCockpitEvidenceState::Measured,
            "critical",
            "memory_tier_pressure",
        ),
        action_receipts: resource_cockpit_domain(
            "action_receipts",
            SwarmResourceCockpitEvidenceState::Mixed,
            "yellow",
            "action_receipt.dry_run",
        ),
    }
}

fn resource_cockpit_capacity_decision(pressured: bool) -> SwarmCapacityOperatorDecisionSummary {
    SwarmCapacityOperatorDecisionSummary {
        stable_id_hash: "sha256:capacity-fixture".to_string(),
        work_class: SwarmCapacityWorkClass::Maintenance,
        action: if pressured {
            SwarmCapacityAdmissionAction::Defer
        } else {
            SwarmCapacityAdmissionAction::Admit
        },
        reason_code: if pressured {
            "capacity.operator.watch"
        } else {
            "resource.proof.healthy"
        }
        .to_string(),
        retry_after_secs: pressured.then_some(30),
        would_apply: false,
        audit_record_id: "audit-capacity-fixture".to_string(),
    }
}

fn resource_cockpit_resource_decision(pressured: bool) -> ResourceAdmissionDecisionSummary {
    ResourceAdmissionDecisionSummary {
        action: if pressured {
            AdmissionAction::Degrade
        } else {
            AdmissionAction::Admit
        },
        reason_codes: if pressured {
            vec![
                AdmissionReasonCode::MemoryTierPressure,
                AdmissionReasonCode::QueueSaturated,
            ]
        } else {
            vec![AdmissionReasonCode::Healthy]
        },
        counters: if pressured {
            AdmissionDecisionCounters {
                admitted: 0,
                deferred: 0,
                degraded: 1,
                shed: 0,
            }
        } else {
            AdmissionDecisionCounters {
                admitted: 1,
                deferred: 0,
                degraded: 0,
                shed: 0,
            }
        },
        raw_pressure_severity: if pressured { 3 } else { 0 },
        effective_pressure_severity: if pressured { 2 } else { 0 },
        priority_protection_units: u8::from(pressured),
        queue_utilization: Some(if pressured { 0.91 } else { 0.25 }),
        pending_items: Some(if pressured { 512 } else { 0 }),
        fleet_pressure: Some(if pressured {
            FleetPressureTier::Critical
        } else {
            FleetPressureTier::Normal
        }),
        memory_tier_pressure: Some(if pressured {
            FleetPressureTier::Emergency
        } else {
            FleetPressureTier::Normal
        }),
        max_latency_over_budget_ratio: Some(if pressured { 1.75 } else { 0.5 }),
        herd_wave_pressure: pressured.then_some(FleetPressureTier::Elevated),
        herd_wave_recommended_stagger_ms: pressured.then_some(250),
        herd_wave_cohort_max_stagger_ms: pressured.then_some(2_000),
    }
}

fn resource_cockpit_admission_counters(pressured: bool) -> SwarmResourceCockpitAdmissionCounters {
    if pressured {
        SwarmResourceCockpitAdmissionCounters {
            admitted: 0,
            deferred: 0,
            degraded: 1,
            shed: 0,
        }
    } else {
        SwarmResourceCockpitAdmissionCounters {
            admitted: 1,
            deferred: 0,
            degraded: 0,
            shed: 0,
        }
    }
}

fn resource_cockpit_full_fixture(
    label: &str,
    status: SwarmCapacityOperatorStatus,
    proof_gate: SwarmResourceCockpitProofGate,
    evidence_state: SwarmResourceCockpitEvidenceState,
    pressure_tier: &str,
) -> SwarmResourceCockpitSnapshot {
    let pressured = !matches!(pressure_tier, "normal" | "green");
    let capacity_decision = resource_cockpit_capacity_decision(pressured);
    let resource_decision = resource_cockpit_resource_decision(pressured);
    let domains = if evidence_state == SwarmResourceCockpitEvidenceState::Mixed {
        resource_cockpit_mixed_domains()
    } else {
        resource_cockpit_domains(evidence_state, pressure_tier)
    };
    let memory_reason_code = if pressured {
        "resource.memory.tier_pressure"
    } else {
        "resource.proof.healthy"
    };
    let queue_tier = if pressured { "red" } else { "green" };
    let admission_action = if pressured { "degrade" } else { "admit" };
    let storage_tier = if pressured {
        StorageIoPressureTier::Red
    } else {
        StorageIoPressureTier::Green
    };

    SwarmResourceCockpitSnapshot {
        schema_version: SWARM_RESOURCE_COCKPIT_SCHEMA_VERSION,
        contract_id: SWARM_RESOURCE_COCKPIT_CONTRACT_ID.to_string(),
        generated_at_ms: 1_700_000_050_000,
        source: format!("schema_golden.{label}"),
        status,
        proof_gate,
        evidence_state,
        summary: format!("{label} resource cockpit fixture"),
        next_operator_move: "retain schema, golden, and e2e proof artifacts".to_string(),
        run_identity: SwarmResourceCockpitRunIdentity {
            run_id: format!("ft-rz0eb-4-{label}"),
            evidence_level: "remote_reduced".to_string(),
            git_head: Some("test-head".to_string()),
            repo_snapshot_head: None,
            artifact_paths: vec![format!("tests/e2e/artifacts/{label}/summary.json")],
            hardware_predicate: SwarmResourceCockpitHardwarePredicate {
                logical_cpus: Some(16),
                memory_gib: Some(64),
                target_class: false,
                proof_status: "skipped_not_proven".to_string(),
            },
        },
        domains,
        memory_pressure: match pressure_tier {
            "normal" | "green" => Some(FleetPressureTier::Normal),
            "elevated" | "yellow" => Some(FleetPressureTier::Elevated),
            "critical" | "red" => Some(FleetPressureTier::Critical),
            "emergency" | "black" => Some(FleetPressureTier::Emergency),
            _ => None,
        },
        memory_tiers: vec![SwarmResourceCockpitMemoryTierSummary {
            tier: FleetMemoryTier::HotResident,
            tier_name: "hot_resident".to_string(),
            evidence_state: SwarmResourceCockpitEvidenceState::Measured,
            resident: true,
            budget_bytes: 2_048,
            actual_bytes: if pressured { 3_072 } else { 1_024 },
            over_budget_bytes: if pressured { 1_024 } else { 0 },
            remaining_budget_bytes: if pressured { 0 } else { 1_024 },
            reclaimable_bytes: if pressured { 512 } else { 0 },
            reclaimed_bytes: if pressured { 128 } else { 0 },
            evicted_bytes: if pressured { 64 } else { 0 },
            refused_bytes: if pressured { 32 } else { 0 },
            has_pressure: pressured,
            reclamation_action: Some(FleetMemoryTierReclamationAction::DemoteHotToWarm),
            reason_codes: vec![memory_reason_code.to_string()],
        }],
        residency_buckets: vec![SwarmResourceCockpitResidencyBucket {
            bucket: MacosResidencyBucket::RustHeap,
            bucket_name: "rust_heap".to_string(),
            evidence_state: SwarmResourceCockpitEvidenceState::Measured,
            bytes: Some(if pressured { 2_097_152 } else { 1_048_576 }),
            confidence: 92,
            dominant: true,
            reason_codes: vec![memory_reason_code.to_string()],
        }],
        slowest_latency_cohorts: vec![SwarmResourceCockpitLatencyCohort {
            stage: SwarmCapacityStage::StorageWrite,
            stage_name: "storage_write".to_string(),
            certificate_status: if pressured {
                SwarmCapacityCertificateStatus::Unsafe
            } else {
                SwarmCapacityCertificateStatus::Safe
            },
            tail_risk_status: Some(if pressured {
                SwarmTailRiskStatus::Watch
            } else {
                SwarmTailRiskStatus::Green
            }),
            reason_code: if pressured {
                "capacity.stage.storage_write_over_budget"
            } else {
                "resource.proof.healthy"
            }
            .to_string(),
            utilization: Some(if pressured { 0.88 } else { 0.25 }),
            observed_p99_ms: Some(if pressured { 18.0 } else { 5.0 }),
            modeled_p99_ms: Some(10.0),
            p99_over_model_ratio: Some(if pressured { 1.8 } else { 0.5 }),
        }],
        capacity_admission_decisions: vec![capacity_decision],
        resource_admission_decisions: vec![resource_decision],
        storage_io: Some(StorageIoOperatorSummary {
            schema_version: 1,
            pressure_domain: "storage_io".to_string(),
            io_pressure_tier: storage_tier,
            io_pressure_reason: if pressured {
                "storage_io.defer.search_freshness_lag"
            } else {
                "storage_io.within_budget"
            }
            .to_string(),
            operator_action: if pressured {
                "throttle_or_defer_io"
            } else {
                "none"
            }
            .to_string(),
            aggregate_queue_depth: if pressured { 8 } else { 0 },
            aggregate_bytes_pending: if pressured { 4_096 } else { 0 },
            oldest_queued_age_ms: pressured.then_some(250),
            durability_pending_total: if pressured { 2 } else { 0 },
            search_lag_segments: if pressured { 7 } else { 0 },
            hydration_lag_pages: u64::from(pressured),
            audit_fail_closed_total: 0,
            write_error_total: u64::from(pressured),
            dominant_class: Some(StorageIoDominantClassSummary {
                class: StorageIoClass::FtsIncremental,
                class_name: "fts_incremental".to_string(),
                queue_depth: if pressured { 8 } else { 0 },
                bytes_pending: if pressured { 4_096 } else { 0 },
                oldest_queued_age_ms: pressured.then_some(250),
                fail_closed_total: 0,
                write_error_total: u64::from(pressured),
                reason_code: pressured.then(|| "storage_io.write_error.io_error".to_string()),
            }),
        }),
        queue_backpressure: vec![SwarmResourceCockpitQueueBackpressureSummary {
            queue: "resource_admission".to_string(),
            evidence_state: SwarmResourceCockpitEvidenceState::Measured,
            tier: queue_tier.to_string(),
            depth: Some(if pressured { 512 } else { 0 }),
            capacity: Some(1_024),
            utilization: Some(if pressured { 0.91 } else { 0.25 }),
            oldest_queued_age_ms: pressured.then_some(750),
            operator_action: if pressured {
                "degrade_noncritical_work"
            } else {
                "none"
            }
            .to_string(),
            reason_codes: vec![if pressured {
                "queue_saturated".to_string()
            } else {
                "resource.proof.healthy".to_string()
            }],
        }],
        admission_decisions: vec![SwarmResourceCockpitAdmissionDecision {
            source: "resource_admission".to_string(),
            action: admission_action.to_string(),
            reason_codes: vec![if pressured {
                "memory_tier_pressure".to_string()
            } else {
                "resource.proof.healthy".to_string()
            }],
            counters: resource_cockpit_admission_counters(pressured),
            raw_pressure_severity: Some(if pressured { 3 } else { 0 }),
            effective_pressure_severity: Some(if pressured { 2 } else { 0 }),
            priority_protection_units: Some(u64::from(pressured)),
            queue_utilization: Some(if pressured { 0.91 } else { 0.25 }),
            pending_items: Some(if pressured { 512 } else { 0 }),
            fleet_pressure: Some(if pressured { "critical" } else { "normal" }.to_string()),
            memory_tier_pressure: Some(if pressured { "emergency" } else { "normal" }.to_string()),
            max_latency_over_budget_ratio: Some(if pressured { 1.75 } else { 0.5 }),
        }],
        action_receipts: vec![SwarmResourceCockpitActionReceipt {
            receipt_id: format!("{label}-receipt"),
            correlation_id: Some(format!("{label}-correlation")),
            action: "observe".to_string(),
            target_domain: "action_receipts".to_string(),
            requested_at_ms: 1_700_000_050_001,
            completed_at_ms: Some(1_700_000_050_002),
            status: "dry_run".to_string(),
            dry_run: true,
            policy_decision: "allow".to_string(),
            evidence_state: SwarmResourceCockpitEvidenceState::Measured,
            freshness: None,
            pane_id: Some(42),
            agent_name: Some("BluePike".to_string()),
            target_dir: Some("/tmp/ft-rz0eb-4".to_string()),
            queue_name: Some("resource_admission".to_string()),
            affected_bytes: Some(2_097_152),
            reason_codes: vec!["action_receipt.dry_run".to_string()],
            artifact_paths: vec![format!("artifacts/{label}/receipt.json")],
        }],
        auto_tune_decisions: vec![TuningDecisionRecord {
            schema_version: AUTO_TUNE_DECISION_RECORD_SCHEMA_VERSION,
            timestamp_ms: 1_700_000_050_003,
            profile: "high-core-canary".to_string(),
            correlation_id: format!("{label}-auto-tune"),
            kind: TuningDecisionKind::CandidateStarted,
            mode: TuningMode::Exploration,
            knob_id: Some(TunableKnobId::RuntimeOutputCoalesceWindowMs),
            knob_name: Some("runtime.output_coalesce_window_ms".to_string()),
            old_value: Some(50.0),
            new_value: Some(75.0),
            rollback_value: None,
            gate: Some(KnobGate::ObserveFirst),
            would_apply: true,
            live_mutation_allowed: false,
            reason_codes: vec!["auto_tune.candidate.runtime.output_coalesce_window_ms".to_string()],
            metric_window: Some(TuningMetricWindowSummary {
                warmup_complete: true,
                measurement_count: Some(30),
                minimum_measurements: Some(10),
                confidence: Some(0.95),
                minimum_confidence: Some(0.80),
                confidence_state: TuningConfidenceState::Acceptable,
            }),
            safety_checks: Vec::new(),
            active_explorations: Some(1),
            max_concurrent_explorations: Some(1),
        }],
        mitigation_history: vec![SwarmResourceCockpitDrilldown {
            subject: "memory".to_string(),
            reason_code: "resource.memory.tier_pressure".to_string(),
            detail: "hot resident tier exceeded fixture budget".to_string(),
        }],
        drilldowns: vec![SwarmResourceCockpitDrilldown {
            subject: "resource_admission".to_string(),
            reason_code: "memory_tier_pressure".to_string(),
            detail: "resource admission degraded noncritical work".to_string(),
        }],
        artifact_paths: vec![format!("tests/e2e/artifacts/{label}/summary.json")],
    }
}

#[test]
fn resource_pressure_cockpit_schema_accepts_generated_and_fixture_states_ft_rz0eb_4() {
    let schema = load_schema("ft-resource-pressure-cockpit.json");
    let validator = compile_draft_2020_schema(&schema);
    let unavailable = SwarmCapacityOperatorSummary::unavailable(
        1_700_000_050_000,
        2,
        "schema_golden.unavailable",
    );
    let unavailable_json = serde_json::to_value(
        unavailable
            .resource_cockpit
            .as_ref()
            .expect("unavailable level 2 summary includes cockpit"),
    )
    .expect("serialize generated unavailable cockpit");

    assert_schema_accepts(
        "generated unavailable/skipped cockpit",
        &validator,
        &unavailable_json,
    );
    assert_eq!(
        unavailable_json["run_identity"]["hardware_predicate"]["proof_status"],
        "skipped_not_proven"
    );
    for domain in [
        "memory",
        "rss_residency",
        "pane_budget",
        "queue_backpressure",
        "storage_io",
        "worker_pool",
        "capacity_admission",
        "resource_admission",
        "action_receipts",
    ] {
        assert_eq!(
            unavailable_json["domains"][domain]["evidence_state"], "unavailable",
            "missing telemetry must produce unavailable domain {domain}"
        );
    }
    assert!(
        unavailable_json.get("memory_tiers").is_none(),
        "empty optional memory tiers may be omitted"
    );

    let fixtures = [
        resource_cockpit_full_fixture(
            "healthy",
            SwarmCapacityOperatorStatus::Ready,
            SwarmResourceCockpitProofGate::Healthy,
            SwarmResourceCockpitEvidenceState::Measured,
            "normal",
        ),
        resource_cockpit_full_fixture(
            "pressured",
            SwarmCapacityOperatorStatus::Watch,
            SwarmResourceCockpitProofGate::Pressured,
            SwarmResourceCockpitEvidenceState::Measured,
            "elevated",
        ),
        resource_cockpit_full_fixture(
            "degraded",
            SwarmCapacityOperatorStatus::Violated,
            SwarmResourceCockpitProofGate::Degraded,
            SwarmResourceCockpitEvidenceState::Measured,
            "critical",
        ),
        resource_cockpit_full_fixture(
            "mixed",
            SwarmCapacityOperatorStatus::Watch,
            SwarmResourceCockpitProofGate::Pressured,
            SwarmResourceCockpitEvidenceState::Mixed,
            "elevated",
        ),
    ];

    for fixture in fixtures {
        let value = serde_json::to_value(&fixture).expect("serialize cockpit fixture");
        assert_schema_accepts(&fixture.source, &validator, &value);
        assert!(
            !value["run_identity"]["hardware_predicate"]["target_class"]
                .as_bool()
                .expect("target_class is boolean"),
            "{} fixture must not claim target hardware",
            fixture.source
        );
        assert_eq!(
            value["run_identity"]["hardware_predicate"]["proof_status"], "skipped_not_proven",
            "{} fixture must keep high-scale proof skipped",
            fixture.source
        );
    }
}

#[test]
fn resource_pressure_cockpit_schema_rejects_missing_required_domain_ft_rz0eb_4() {
    let schema = load_schema("ft-resource-pressure-cockpit.json");
    let validator = compile_draft_2020_schema(&schema);
    let mut value = serde_json::to_value(resource_cockpit_full_fixture(
        "missing-domain",
        SwarmCapacityOperatorStatus::Watch,
        SwarmResourceCockpitProofGate::Pressured,
        SwarmResourceCockpitEvidenceState::Mixed,
        "elevated",
    ))
    .expect("serialize cockpit fixture");

    value["domains"]
        .as_object_mut()
        .expect("domains object")
        .remove("action_receipts");

    assert!(
        !validator.is_valid(&value),
        "schema must reject omitted required cockpit domains"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Registry ↔ disk coverage
// ─────────────────────────────────────────────────────────────────────

#[test]
fn registry_covers_all_disk_schemas() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    let registry = SchemaRegistry::canonical();
    // Exclude non-endpoint schemas: the envelopes are response wrappers,
    // ft-config documents ft.toml, ft-pattern-pack documents extension files,
    // and the capacity contract files are nested operator artifacts rather
    // than standalone robot endpoints.
    let disk_names: Vec<String> = schemas
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| {
            name != "wa-robot-envelope.json"
                && name != "wa-mcp-envelope.json"
                && name != "ft-config.json"
                && name != "ft-pattern-pack.json"
                && name != "ft-resource-pressure-cockpit.json"
                && name != "ft-swarm-capacity-signal-inventory.json"
        })
        .collect();

    let uncovered = registry.uncovered_schemas(&disk_names);
    assert!(
        uncovered.is_empty(),
        "Schema files on disk not in registry: {uncovered:?}"
    );
}

#[test]
fn registry_schema_files_exist_on_disk() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    let disk_names: HashSet<String> = schemas.into_iter().map(|(name, _)| name).collect();
    let registry = SchemaRegistry::canonical();

    // Track which registry files are missing — these are expected gaps where
    // the schema file hasn't been authored yet.
    let mut missing = Vec::new();
    for file in registry.schema_files() {
        if !disk_names.contains(file) {
            missing.push(file.to_string());
        }
    }

    // Allow known gaps (schemas registered but not yet authored).
    // As schemas are authored, they should be removed from this list.
    let known_gaps: HashSet<&str> = ["wa-robot-rules-lint.json", "wa-robot-rules-show.json"]
        .into_iter()
        .collect();

    let unexpected: Vec<&String> = missing
        .iter()
        .filter(|f| !known_gaps.contains(f.as_str()))
        .collect();

    assert!(
        unexpected.is_empty(),
        "Registry references schema files not on disk (and not in known gaps): {unexpected:?}"
    );
}

#[test]
fn every_endpoint_has_schema_file() {
    let registry = SchemaRegistry::canonical();

    for ep in &registry.endpoints {
        assert!(
            !ep.schema_file.is_empty(),
            "Endpoint '{}' has empty schema_file",
            ep.id
        );
        assert!(
            std::path::Path::new(&ep.schema_file)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json")),
            "Endpoint '{}' schema_file should end with .json: {}",
            ep.id,
            ep.schema_file
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Schema parsing validation
// ─────────────────────────────────────────────────────────────────────

#[test]
fn all_schemas_parse_successfully() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        let doc = parse_schema(schema);
        // Every schema should have a non-empty title
        assert!(!doc.title.is_empty(), "{name} parsed with empty title");
    }
}

#[test]
fn parsed_schemas_have_properties() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        // Envelope and data schemas should have properties
        if schema.get("properties").is_some() {
            let doc = parse_schema(schema);
            assert!(
                !doc.properties.is_empty(),
                "{name} has 'properties' in JSON but parsed to empty"
            );
        }
    }
}

#[test]
fn send_schema_has_expected_fields() {
    let schemas = load_all_schemas();
    let send = schemas
        .iter()
        .find(|(name, _)| name == "wa-robot-send.json");

    if let Some((_, schema)) = send {
        let doc = parse_schema(schema);
        let names: Vec<&str> = doc.properties.iter().map(|p| p.name.as_str()).collect();

        assert!(names.contains(&"pane_id"), "send missing pane_id");
        assert!(names.contains(&"sent"), "send missing sent");
        assert!(
            names.contains(&"policy_decision"),
            "send missing policy_decision"
        );

        // Required fields should be marked required
        let pane_id = doc.properties.iter().find(|p| p.name == "pane_id").unwrap();
        assert!(pane_id.required, "pane_id should be required");

        let policy = doc
            .properties
            .iter()
            .find(|p| p.name == "policy_decision")
            .unwrap();
        assert!(
            !policy.enum_values.is_empty(),
            "policy_decision should have enum values"
        );
    }
}

#[test]
fn events_schema_has_defs() {
    let schemas = load_all_schemas();
    let events = schemas
        .iter()
        .find(|(name, _)| name == "wa-robot-events.json");

    if let Some((_, schema)) = events {
        let doc = parse_schema(schema);
        assert!(
            !doc.definitions.is_empty(),
            "events schema should have $defs"
        );

        assert!(
            doc.definitions.iter().any(|(n, _)| n == "event"),
            "events missing 'event' def"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Docs generation determinism
// ─────────────────────────────────────────────────────────────────────

#[test]
fn docs_generation_deterministic_without_schemas() {
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig::default();

    let pages1 = generate_reference(&registry, &[], &config);
    let pages2 = generate_reference(&registry, &[], &config);

    assert_eq!(pages1.len(), pages2.len());
    for (p1, p2) in pages1.iter().zip(pages2.iter()) {
        assert_eq!(
            p1.content, p2.content,
            "non-deterministic output for {}",
            p1.filename
        );
    }
}

#[test]
fn docs_generation_deterministic_with_schemas() {
    let schemas = load_all_schemas();
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig::default();

    let pages1 = generate_reference(&registry, &schemas, &config);
    let pages2 = generate_reference(&registry, &schemas, &config);

    assert_eq!(pages1.len(), pages2.len());
    for (p1, p2) in pages1.iter().zip(pages2.iter()) {
        assert_eq!(
            p1.content, p2.content,
            "non-deterministic output for {}",
            p1.filename
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Generated reference structure
// ─────────────────────────────────────────────────────────────────────

#[test]
fn reference_has_header_and_toc() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);

    assert!(!pages.is_empty(), "should produce at least one page");
    let content = &pages[0].content;

    assert!(content.contains("# wa API Reference"), "missing title");
    assert!(content.contains("## Table of Contents"), "missing TOC");
    assert!(
        content.contains(&registry.version),
        "missing version in header"
    );
}

#[test]
fn reference_has_all_categories() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    for cat in EndpointCategory::all() {
        assert!(
            content.contains(cat.title()),
            "missing category section: {}",
            cat.title()
        );
    }
}

#[test]
fn reference_has_envelope_section() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig {
        include_envelope: true,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    assert!(
        content.contains("## Response Envelope"),
        "missing envelope section"
    );
}

#[test]
fn reference_without_envelope() {
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig {
        include_envelope: false,
        include_error_codes: false,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &[], &config);
    let content = &pages[0].content;

    assert!(
        !content.contains("## Response Envelope"),
        "envelope should be excluded"
    );
}

#[test]
fn reference_has_error_codes() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig {
        include_error_codes: true,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    assert!(
        content.contains("## Error Codes"),
        "missing error codes section"
    );
    assert!(
        content.contains("robot.policy_denied"),
        "missing specific error code"
    );
    assert!(
        content.contains("robot.reservation_conflict"),
        "missing reservation conflict error code"
    );
    assert!(
        content.contains("robot.require_approval"),
        "missing approval-required error code"
    );
    assert!(
        content.contains("robot.approval_error"),
        "missing approval workflow error code"
    );
    assert!(
        content.contains("robot.wezterm_not_running"),
        "missing specific backend availability error code"
    );
}

#[test]
fn robot_envelope_schema_tracks_current_core_error_codes() {
    let schemas = load_all_schemas();
    let envelope = schemas
        .iter()
        .find(|(name, _)| name == "wa-robot-envelope.json")
        .map(|(_, schema)| schema)
        .expect("wa-robot-envelope.json should exist");

    let codes: HashSet<&str> = envelope
        .pointer("/$defs/error_codes/enum")
        .and_then(Value::as_array)
        .expect("wa-robot-envelope.json should define error_codes enum")
        .iter()
        .filter_map(Value::as_str)
        .collect();

    let expected: HashSet<&str> = [
        "robot.invalid_args",
        "robot.unknown_subcommand",
        "robot.config_error",
        "robot.feature_not_available",
        "robot.unsupported",
        "robot.wezterm_error",
        "robot.wezterm_not_found",
        "robot.wezterm_not_running",
        "robot.wezterm_socket_not_found",
        "robot.wezterm_command_failed",
        "robot.wezterm_parse_error",
        "robot.circuit_open",
        "robot.storage_error",
        "robot.fts_query_error",
        "robot.policy_denied",
        "robot.require_approval",
        "robot.approval_error",
        "robot.rate_limited",
        "robot.pane_not_found",
        "robot.reservation_conflict",
        "robot.event_not_found",
        "robot.rule_not_found",
        "robot.workflow_aborted",
        "robot.workflow_error",
        "robot.workflow_not_found",
        "robot.code_not_found",
        "robot.invalid_service",
        "robot.cass_not_installed",
        "robot.cass_timeout",
        "robot.cass_output_too_large",
        "robot.cass_invalid_json",
        "robot.cass_error",
        "robot.agent_detection_error",
        "robot.caut_error",
        "robot.mission_not_found",
        "robot.mission_read_failed",
        "robot.mission_invalid_json",
        "robot.mission_validation_failed",
        "robot.assignment_not_found",
        "robot.mission_error",
        "robot.tx_not_found",
        "robot.tx_read_failed",
        "robot.tx_invalid_json",
        "robot.tx_validation_failed",
        "robot.tx_execution_failed",
        "robot.tx_error",
        "robot.internal_error",
        "robot.timeout",
        "robot.profile.unknown_action",
        "robot.profile.bad_params",
        "robot.profile.not_found",
        "robot.profile.storage",
        "robot.profile.spawn_failed",
        "robot.profile.validation_failed",
        "robot.profile.policy_denied",
        "robot.profile.require_approval",
        "robot.profile.daemon_unavailable",
        "robot.profile.bootstrap_failed",
        "robot.profile.compensation_failed",
        "robot.profile.apply_log",
        "robot.profile.mutation_plan",
        "robot.fleet.inventory_unavailable",
        "robot.fleet.work_queue_unavailable",
        "robot.fleet.policy_denied",
        "robot.fleet.approval_required",
        "robot.fleet.daemon_unavailable",
        "robot.fleet.mutation_failed",
        "robot.fleet.no_eligible_targets",
        "robot.fleet.target_not_reached",
        "robot.fleet.validation_failed",
        "robot.fleet.work_item_not_found",
        "robot.fleet.work_item_completed",
        "robot.fleet.work_item_blocked",
        "robot.fleet.work_item_approval_blocked",
        "robot.fleet.work_item_reserved",
        "robot.fleet.reassign_conflict",
        "robot.fleet.reassign_failed",
        "robot.fleet.spawn_failed",
        "robot.fleet.launch_failed",
        "robot.fleet.stop_failed",
        "robot.fleet.plan_error",
    ]
    .into_iter()
    .collect();

    assert_eq!(
        codes, expected,
        "wa-robot-envelope.json drifted from the current robot error-code contract"
    );
}

#[test]
fn mcp_envelope_schema_tracks_current_core_error_codes() {
    let schemas = load_all_schemas();
    let envelope = schemas
        .iter()
        .find(|(name, _)| name == "wa-mcp-envelope.json")
        .map(|(_, schema)| schema)
        .expect("wa-mcp-envelope.json should exist");

    let codes: HashSet<&str> = envelope
        .pointer("/$defs/error_codes/enum")
        .and_then(Value::as_array)
        .expect("wa-mcp-envelope.json should define error_codes enum")
        .iter()
        .filter_map(Value::as_str)
        .collect();

    let expected: HashSet<&str> = [
        "FT-MCP-0001",
        "FT-MCP-0003",
        "FT-MCP-0004",
        "FT-MCP-0005",
        "FT-MCP-0006",
        "FT-MCP-0007",
        "FT-MCP-0008",
        "FT-MCP-0009",
        "FT-MCP-0010",
        "FT-MCP-0011",
        "FT-MCP-0012",
        "FT-MCP-0013",
        "FT-MCP-0014",
        "FT-MCP-0015",
        "FT-MCP-9000",
    ]
    .into_iter()
    .collect();

    assert_eq!(
        codes, expected,
        "wa-mcp-envelope.json drifted from the current MCP error-code contract"
    );
}

#[test]
fn reference_has_endpoint_sections() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    // Verify a few key endpoints are present
    assert!(content.contains("### Pane State"), "missing Pane State");
    assert!(content.contains("### Send Text"), "missing Send Text");
    assert!(content.contains("### Search"), "missing Search");
    assert!(content.contains("### Run Workflow"), "missing Run Workflow");
    assert!(content.contains("### List Rules"), "missing List Rules");
}

#[test]
fn reference_has_surface_info() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    // Dual-surface endpoints should show both robot and MCP
    assert!(
        content.contains("**Robot:** `ft robot state`"),
        "missing robot command for state"
    );
    assert!(
        content.contains("**MCP:** `wa.state`"),
        "missing MCP tool for state"
    );
}

#[test]
fn reference_marks_experimental() {
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig {
        include_experimental: true,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &[], &config);
    let content = &pages[0].content;

    // rules_show is experimental
    assert!(
        content.contains("Experimental"),
        "should mark experimental endpoints"
    );
}

#[test]
fn reference_excludes_experimental_when_configured() {
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig {
        include_experimental: false,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &[], &config);
    let content = &pages[0].content;

    // Show Rule is the only experimental endpoint
    assert!(
        !content.contains("### Show Rule"),
        "should exclude experimental endpoints"
    );
}

#[test]
fn reference_has_property_tables_for_loaded_schemas() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    // Should have property tables with headers
    assert!(
        content.contains("| Field | Type | Required | Description |"),
        "missing property table headers"
    );

    // Send endpoint should have its fields documented
    assert!(
        content.contains("| `pane_id`"),
        "missing pane_id in property table"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Endpoint categorization coverage
// ─────────────────────────────────────────────────────────────────────

#[test]
fn all_registry_endpoints_categorized() {
    let registry = SchemaRegistry::canonical();

    for ep in &registry.endpoints {
        let cat = categorize_endpoint(ep);
        // Verify it's a valid category (not just Meta for everything)
        let _ = cat.title(); // Should not panic
    }
}

#[test]
fn categorization_covers_expected_distribution() {
    let registry = SchemaRegistry::canonical();

    let mut counts: std::collections::HashMap<EndpointCategory, usize> =
        std::collections::HashMap::new();
    for ep in &registry.endpoints {
        *counts.entry(categorize_endpoint(ep)).or_default() += 1;
    }

    // Sanity check: should have endpoints in multiple categories
    assert!(counts.len() >= 5, "too few categories used: {counts:?}");

    // Pane operations should have 4 (state, get_text, send, wait_for)
    assert_eq!(
        counts
            .get(&EndpointCategory::PaneOperations)
            .copied()
            .unwrap_or(0),
        4,
        "expected 4 pane operations"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Summary generation
// ─────────────────────────────────────────────────────────────────────

#[test]
fn summary_table_includes_all_endpoints() {
    let registry = SchemaRegistry::canonical();
    let summary = generate_endpoint_summary(&registry);

    for ep in &registry.endpoints {
        assert!(
            summary.contains(&ep.title),
            "summary missing endpoint: {}",
            ep.title
        );
    }
}

#[test]
fn summary_table_has_correct_columns() {
    let registry = SchemaRegistry::canonical();
    let summary = generate_endpoint_summary(&registry);

    assert!(summary.contains("| Endpoint |"));
    assert!(summary.contains("| Robot Command |"));
    assert!(summary.contains("| MCP Tool |"));
    assert!(summary.contains("| Stable |"));
}
