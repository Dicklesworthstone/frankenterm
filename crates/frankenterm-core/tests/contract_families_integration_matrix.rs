#![allow(clippy::bool_to_int_with_if)]
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use jsonschema::Validator;
use proptest::prelude::*;
use serde_json::{Value, json};

const STATES: [&str; 6] = [
    "measured",
    "inferred",
    "simulated",
    "stale",
    "unavailable",
    "mixed",
];

const FAMILIES: [FamilySpec; 5] = [
    FamilySpec {
        id: "context-horizon",
        schema: "ft-context-horizon.json",
    },
    FamilySpec {
        id: "capture-fairness",
        schema: "ft-capture-fairness.json",
    },
    FamilySpec {
        id: "herd-wave",
        schema: "ft-herd-wave.json",
    },
    FamilySpec {
        id: "blocker-radar",
        schema: "ft-blocker-radar.json",
    },
    FamilySpec {
        id: "resource-cockpit",
        schema: "ft-resource-pressure-cockpit.json",
    },
];

const INVARIANT_IDS: [&str; 7] = [
    "CF-001", "CF-002", "CF-003", "CF-004", "CF-005", "CF-006", "CF-007",
];

#[derive(Clone, Copy)]
struct FamilySpec {
    id: &'static str,
    schema: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct MatrixTuple {
    index: usize,
    context: &'static str,
    capture: &'static str,
    herd: &'static str,
    blocker: &'static str,
    resource: &'static str,
}

#[derive(Debug)]
struct ContractSet {
    context: Value,
    capture: Value,
    herd: Value,
    blocker: Value,
    resource: Value,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root exists")
        .to_path_buf()
}

fn load_schema_validator(schema_name: &str) -> Validator {
    let schema_path = workspace_root()
        .join("docs")
        .join("json-schema")
        .join(schema_name);
    let schema_text = fs::read_to_string(&schema_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", schema_path.display()));
    let schema: Value = serde_json::from_str(&schema_text)
        .unwrap_or_else(|err| panic!("parse {}: {err}", schema_path.display()));
    jsonschema::draft202012::options()
        .build(&schema)
        .unwrap_or_else(|err| panic!("compile {}: {err}", schema_path.display()))
}

fn validators() -> BTreeMap<&'static str, Validator> {
    FAMILIES
        .iter()
        .map(|family| (family.id, load_schema_validator(family.schema)))
        .collect()
}

fn assert_schema_accepts(label: &str, validator: &Validator, value: &Value) {
    if let Err(errors) = validator.validate(value) {
        let messages = errors
            .map(|error| format!("{}: {}", error.instance_path, error))
            .collect::<Vec<_>>()
            .join("\n");
        panic!("{label} failed schema validation:\n{messages}\n{value:#}");
    }
}

fn matrix_tuples() -> Vec<MatrixTuple> {
    let mut tuples = Vec::with_capacity(STATES.len().pow(5));
    let mut index = 0;
    for context in STATES {
        for capture in STATES {
            for herd in STATES {
                for blocker in STATES {
                    for resource in STATES {
                        tuples.push(MatrixTuple {
                            index,
                            context,
                            capture,
                            herd,
                            blocker,
                            resource,
                        });
                        index += 1;
                    }
                }
            }
        }
    }
    tuples
}

fn state_strategy() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("measured"),
        Just("inferred"),
        Just("simulated"),
        Just("stale"),
        Just("unavailable"),
        Just("mixed"),
    ]
}

fn generated_at(tuple: MatrixTuple) -> u64 {
    1_770_000_000_000 + u64::try_from(tuple.index).expect("tuple index fits u64")
}

fn reason_for(state: &str) -> String {
    format!("evidence.{state}")
}

fn context_risk(state: &str) -> &'static str {
    match state {
        "measured" => "green",
        "inferred" | "simulated" => "yellow",
        "stale" => "red",
        "unavailable" => "unknown",
        "mixed" => "black",
        _ => "unknown",
    }
}

fn handoff_readiness(risk: &str) -> &'static str {
    match risk {
        "green" => "not_needed",
        "yellow" => "prepare",
        "red" => "ready",
        "black" => "blocked",
        _ => "unknown",
    }
}

fn resource_pressure(state: &str) -> &'static str {
    match state {
        "measured" => "green",
        "inferred" | "simulated" => "yellow",
        "stale" | "mixed" => "red",
        "unavailable" => "unknown",
        _ => "unknown",
    }
}

fn resource_operator_status(pressure: &str) -> &'static str {
    match pressure {
        "green" => "ready",
        "yellow" => "watch",
        "red" | "black" => "violated",
        _ => "unavailable",
    }
}

fn resource_proof_gate(pressure: &str, state: &str) -> &'static str {
    if state == "unavailable" {
        "skipped_proof"
    } else if matches!(pressure, "red" | "black") {
        "pressured"
    } else if matches!(state, "stale" | "mixed") {
        "degraded"
    } else {
        "healthy"
    }
}

fn resource_schema_state(state: &str) -> &'static str {
    match state {
        "measured" => "measured",
        "inferred" => "measured",
        "simulated" => "simulated",
        "stale" => "stale",
        "unavailable" => "unavailable",
        "mixed" => "mixed",
        _ => "unavailable",
    }
}

fn capture_starvation_risk(tuple: MatrixTuple) -> bool {
    matches!(resource_pressure(tuple.resource), "red" | "black")
        || matches!(tuple.capture, "stale" | "unavailable" | "mixed")
}

fn blocker_state(state: &str) -> &'static str {
    match state {
        "measured" | "simulated" => "actionable",
        "inferred" => "waiting_external",
        "stale" => "stale_possible",
        "unavailable" => "mail_unavailable",
        "mixed" => "dirty_overlap",
        _ => "unknown",
    }
}

fn blocker_is_blocked(state: &str) -> bool {
    blocker_state(state) != "actionable"
}

fn herd_admission_action(tuple: MatrixTuple) -> &'static str {
    if tuple.herd == "unavailable" {
        "unavailable"
    } else if blocker_is_blocked(tuple.blocker) {
        "defer"
    } else if context_risk(tuple.context) == "black" {
        "shed"
    } else if matches!(resource_pressure(tuple.resource), "red" | "black")
        && capture_starvation_risk(tuple)
    {
        "degrade"
    } else {
        "admit"
    }
}

fn herd_next_action(tuple: MatrixTuple) -> &'static str {
    if blocker_is_blocked(tuple.blocker) {
        "pause_assignment"
    } else if herd_admission_action(tuple) == "admit" {
        "observe"
    } else {
        "reduce_fanout"
    }
}

fn herd_stagger_ms(tuple: MatrixTuple) -> u64 {
    if context_risk(tuple.context) == "black" {
        1_500
    } else if matches!(herd_admission_action(tuple), "defer" | "degrade" | "shed") {
        750
    } else {
        0
    }
}

fn herd_pressure_tier(tuple: MatrixTuple) -> &'static str {
    if tuple.herd == "unavailable" {
        "unknown"
    } else if context_risk(tuple.context) == "black" {
        "emergency"
    } else if matches!(resource_pressure(tuple.resource), "red" | "black") {
        "critical"
    } else if capture_starvation_risk(tuple) || blocker_is_blocked(tuple.blocker) {
        "elevated"
    } else {
        "normal"
    }
}

fn herd_overall_state(tuple: MatrixTuple) -> &'static str {
    match tuple.herd {
        "stale" => "stale_evidence",
        "unavailable" => "missing_telemetry",
        _ if context_risk(tuple.context) == "black" => "emergency",
        _ if matches!(resource_pressure(tuple.resource), "red" | "black") => "critical",
        _ if blocker_is_blocked(tuple.blocker) => "elevated",
        _ => "normal",
    }
}

fn stale_or_unavailable(state: &str) -> bool {
    matches!(state, "stale" | "unavailable")
}

fn unavailable_reason_marker(state: &str) -> Vec<String> {
    if stale_or_unavailable(state) {
        vec![reason_for(state)]
    } else {
        Vec::new()
    }
}

fn synthesize_context(tuple: MatrixTuple) -> Value {
    let risk = context_risk(tuple.context);
    let pane_at_red_or_black = matches!(risk, "red" | "black");
    let citation_id = format!("ctx-cite-{}", tuple.index);
    let reason = reason_for(tuple.context);
    let unavailable_domains = if stale_or_unavailable(tuple.context) {
        json!([{
            "domain": "context_registry",
            "evidence_state": tuple.context,
            "reason_codes": [reason],
            "failure_class": "unavailable_evidence"
        }])
    } else {
        json!([])
    };

    json!({
        "schema_version": 1,
        "contract_id": "ft.context_horizon.v1",
        "generated_at_ms": generated_at(tuple),
        "source": "contract_families.matrix.context_horizon",
        "evidence_state": tuple.context,
        "horizon_window_ms": 900000,
        "fleet_summary": {
            "total_panes": 1,
            "highest_risk_tier": risk,
            "panes_at_red_or_black": if pane_at_red_or_black { 1 } else { 0 },
            "top_operator_move": if pane_at_red_or_black { "pause assignment" } else { "observe" },
            "evidence_state": tuple.context
        },
        "pane_risks": [{
            "pane_id": 1,
            "risk_tier": risk,
            "evidence_state": tuple.context,
            "context_utilization": if risk == "unknown" { Value::Null } else { json!(0.99) },
            "tokens_consumed": if risk == "unknown" { Value::Null } else { json!(99000) },
            "token_budget": if risk == "unknown" { Value::Null } else { json!(100000) },
            "rotation_depth": 1,
            "ms_since_last_rotation": if tuple.context == "unavailable" { Value::Null } else { json!(60000) },
            "compaction_pressure": risk,
            "rate_limit_risk": if pane_at_red_or_black { risk } else { "green" },
            "handoff_readiness": handoff_readiness(risk),
            "reason_codes": [reason_for(tuple.context)],
            "citation_ids": [citation_id]
        }],
        "recommendations": [{
            "recommendation_id": format!("ctx-rec-{}", tuple.index),
            "scope": "pane",
            "pane_id": 1,
            "action_kind": if pane_at_red_or_black { "pause_assignment" } else { "none" },
            "mutation_allowed": false,
            "policy_state": if risk == "unknown" { "unavailable" } else { "allowed_dry_run" },
            "operator_summary": "deterministic context-horizon matrix recommendation",
            "suggested_command": "ft robot --format toon context horizon",
            "reason_codes": [reason_for(tuple.context)],
            "citation_ids": [citation_id]
        }],
        "citations": [{
            "citation_id": citation_id,
            "source": "contract-families-matrix",
            "evidence_state": tuple.context,
            "redacted": true,
            "summary": "bounded synthetic context counters"
        }],
        "unavailable_domains": unavailable_domains,
        "redaction_policy": {
            "raw_transcript_allowed": false,
            "raw_prompt_allowed": false,
            "bounded_citations_only": true,
            "secret_redaction_required": true
        },
        "raw_context_content_stored": false,
        "artifact_paths": ["target/test-logs/cross-family/context-horizon.json"]
    })
}

fn synthesize_capture(tuple: MatrixTuple) -> Value {
    let starvation = capture_starvation_risk(tuple);
    let reason = if starvation {
        "capture.starvation_risk"
    } else {
        "capture.scheduler_balanced"
    };

    json!({
        "schema_version": 1,
        "contract_id": "ft.capture_fairness.v1",
        "generated_at_ms": generated_at(tuple),
        "source": "contract_families.matrix.capture_fairness",
        "budget": {
            "max_captures_per_sec": 4,
            "max_bytes_per_sec": 4096
        },
        "ready_panes_total": 4,
        "available_permits": 2,
        "selected_panes": if starvation { json!([1]) } else { json!([1, 3]) },
        "scheduler_snapshot": {
            "budget_active": true,
            "max_captures_per_sec": 4,
            "max_bytes_per_sec": 4096,
            "captures_remaining": if starvation { 1 } else { 2 },
            "bytes_remaining": if starvation { 256 } else { 2048 },
            "total_rate_limited": if starvation { 1 } else { 0 },
            "total_byte_budget_exceeded": 0,
            "total_throttle_events": if starvation { 1 } else { 0 },
            "tracked_panes": 4,
            "pane_rows_total": 4,
            "pane_rows_truncated": false,
            "panes": [
                {"pane_id": 1, "canonical_evidence_state": tuple.capture, "reason_codes": [reason_for(tuple.capture)]}
            ],
            "tiers": [
                {
                    "tier": "low",
                    "canonical_evidence_state": tuple.capture,
                    "starvation_risk": starvation,
                    "reason_codes": [reason, reason_for(tuple.capture)]
                }
            ]
        },
        "pass_fail": {
            "selected_within_permits": true,
            "selected_within_ready_set": true,
            "no_raw_content": true
        },
        "redaction_policy": {
            "raw_pane_content_allowed": false,
            "bounded_counters_only": true,
            "secret_redaction_required": true
        },
        "raw_pane_content_stored": false,
        "artifact_paths": ["target/test-logs/cross-family/capture-fairness.json"]
    })
}

fn synthesize_herd(tuple: MatrixTuple) -> Value {
    let action = herd_admission_action(tuple);
    let next_action = herd_next_action(tuple);
    let stagger_ms = herd_stagger_ms(tuple);
    let pressure = herd_pressure_tier(tuple);
    let reason_codes = herd_reason_codes(tuple);

    json!({
        "schema_version": 1,
        "contract_id": "ft.herd_wave.v1",
        "generated_at_ms": generated_at(tuple),
        "source": "contract_families.matrix.herd_wave",
        "source_freshness": {
            "generated_at_ms": if tuple.herd == "unavailable" { Value::Null } else { json!(generated_at(tuple)) },
            "freshness_ms": if tuple.herd == "unavailable" { Value::Null } else { json!(0) },
            "max_age_ms": 60000,
            "evidence_state": tuple.herd,
            "reason_codes": [reason_for(tuple.herd)]
        },
        "evidence_state": tuple.herd,
        "overall_state": herd_overall_state(tuple),
        "dominant_kind": if action == "admit" { "none" } else { "workflow_fanout" },
        "event_count": if action == "admit" { 0 } else { 4 },
        "distinct_panes": if action == "admit" { 0 } else { 4 },
        "window_ms": 60000,
        "pressure_tier": pressure,
        "admission_action": action,
        "reason_codes": reason_codes,
        "recommended_stagger_ms": stagger_ms,
        "cohort_max_stagger_ms": stagger_ms.saturating_mul(4),
        "wave_summary": {
            "detected": action != "admit",
            "event_count": if action == "admit" { 0 } else { 4 },
            "distinct_panes": if action == "admit" { 0 } else { 4 },
            "window_ms": 60000,
            "first_seen_ms": if action == "admit" { Value::Null } else { json!(generated_at(tuple)) },
            "last_seen_ms": if action == "admit" { Value::Null } else { json!(generated_at(tuple) + 3000) },
            "dominant_kind": if action == "admit" { "none" } else { "workflow_fanout" },
            "dominant_kind_count": if action == "admit" { 0 } else { 4 },
            "pressure_tier": pressure,
            "recommended_stagger_ms": stagger_ms,
            "cohort_max_stagger_ms": stagger_ms.saturating_mul(4),
            "reason_codes": herd_reason_codes(tuple)
        },
        "priority_protection": {
            "protected": action == "admit",
            "protection_units": if action == "admit" { 1 } else { 0 },
            "pane_priority_tier": "medium",
            "work_priority": 5,
            "mission_critical": false,
            "effective_admission_action": action,
            "reason_codes": ["priority.matrix_fixture"]
        },
        "operator_override": {
            "active": false,
            "override_id": Value::Null,
            "scope": Value::Null,
            "approved_by": Value::Null,
            "reason_codes": []
        },
        "stagger_plan": [{
            "action_id": format!("herd-stagger-{}", tuple.index),
            "pane_id": 1,
            "cohort_rank": 0,
            "event_kind": if action == "admit" { "none" } else { "workflow_fanout" },
            "scheduled_after_ms": stagger_ms,
            "admission_action": action,
            "mutation_allowed": false,
            "reason_codes": herd_reason_codes(tuple),
            "citation_ids": ["herd-cite"]
        }],
        "citations": [{
            "citation_id": "herd-cite",
            "source": "contract-families-matrix",
            "evidence_state": tuple.herd,
            "subject_type": "fixture",
            "subject_id": format!("tuple-{}", tuple.index),
            "generated_at_ms": generated_at(tuple),
            "artifact_path": "target/test-logs/cross-family/herd-wave.json",
            "reason_codes": [reason_for(tuple.herd)]
        }],
        "next_actions": [{
            "action_id": format!("herd-next-{}", tuple.index),
            "action_kind": next_action,
            "operator_summary": "deterministic cross-family read-only action",
            "mutation_allowed": false,
            "requires_approval": false,
            "reason_codes": herd_reason_codes(tuple),
            "citation_ids": ["herd-cite"]
        }],
        "forbidden_actions": [
            "no_agent_mail_restart",
            "no_agent_mail_repair",
            "no_pane_mutation",
            "no_queue_mutation",
            "no_destructive_git_or_filesystem_operation",
            "no_raw_pane_content"
        ],
        "unavailable_sources": if stale_or_unavailable(tuple.herd) {
            json!([{
                "source": "herd_wave_telemetry",
                "evidence_state": tuple.herd,
                "freshness_ms": if tuple.herd == "stale" { json!(120000) } else { Value::Null },
                "max_age_ms": 60000,
                "reason_codes": [reason_for(tuple.herd)]
            }])
        } else {
            json!([])
        },
        "redaction_policy": {
            "raw_pane_content_allowed": false,
            "max_excerpt_chars": 0,
            "secret_redaction_required": true,
            "allowed_citation_subjects": ["fixture", "counter", "artifact"],
            "reason_codes": ["privacy.no_raw_pane_content"]
        },
        "raw_pane_content_stored": false,
        "target_class_hardware_proof": {
            "available": false,
            "cpu_cores": Value::Null,
            "memory_gib": Value::Null,
            "host_fingerprint": Value::Null,
            "rch_worker": Value::Null,
            "run_id": Value::Null,
            "artifact_path": Value::Null,
            "command": Value::Null,
            "exit_status": Value::Null,
            "measured_window_ms": Value::Null,
            "reason_codes": ["target_class.not_claimed"]
        },
        "artifact_paths": ["target/test-logs/cross-family/herd-wave.json"]
    })
}

fn herd_reason_codes(tuple: MatrixTuple) -> Vec<String> {
    let mut reasons = vec![reason_for(tuple.herd)];
    if context_risk(tuple.context) == "black" {
        reasons.push("cross_family.context_black".to_string());
    }
    if matches!(resource_pressure(tuple.resource), "red" | "black") {
        reasons.push("cross_family.resource_pressure".to_string());
    }
    if capture_starvation_risk(tuple) {
        reasons.push("cross_family.capture_starvation".to_string());
    }
    if blocker_is_blocked(tuple.blocker) {
        reasons.push("cross_family.blocker_not_actionable".to_string());
    }
    reasons
}

fn synthesize_blocker(tuple: MatrixTuple) -> Value {
    let state = blocker_state(tuple.blocker);
    let source_id = format!("blocker-source-{}", tuple.index);
    let blocker_id = format!("blocker-row-{}", tuple.index);
    let action_id = format!("blocker-next-{}", tuple.index);

    json!({
        "schema_version": 1,
        "contract_id": "ft.blocker_radar.v1",
        "generated_at_ms": generated_at(tuple),
        "source": "contract_families.matrix.blocker_radar",
        "overall_state": state,
        "sources": [{
            "source_id": source_id,
            "source_kind": "fixture",
            "evidence_state": state,
            "collected_at_ms": if tuple.blocker == "unavailable" { Value::Null } else { json!(generated_at(tuple)) },
            "freshness_ms": if tuple.blocker == "unavailable" { Value::Null } else { json!(0) },
            "command_or_api": "contract-family-fixture",
            "live": false,
            "redacted": true,
            "reason_codes": [reason_for(tuple.blocker)],
            "artifact_paths": ["target/test-logs/cross-family/blocker-radar.json"]
        }],
        "blockers": [{
            "blocker_id": blocker_id,
            "evidence_state": state,
            "severity": if state == "actionable" { "info" } else { "blocked" },
            "summary": "deterministic blocker-radar cross-family row",
            "source_ids": [source_id],
            "citation_ids": ["blocker-cite"],
            "dependency_ids": ["ft-tf6g3.46"],
            "next_action_ids": [action_id]
        }],
        "active_agents": [],
        "dirty_overlap": if state == "dirty_overlap" {
            json!([{
                "path": "crates/frankenterm-core/src/blocker_radar.rs",
                "status": "modified",
                "risk_level": "blocked",
                "expected_owner": "fixture-owner",
                "related_bead_ids": ["ft-tf6g3.46"],
                "recommendation": "avoid overlapping edit surface"
            }])
        } else {
            json!([])
        },
        "external_queues": if state == "waiting_external" {
            json!([{
                "queue_id": "fixture-rch",
                "substrate": "rch",
                "evidence_state": state,
                "run_id": "fixture-run",
                "url": Value::Null,
                "worker_id": "fixture-worker",
                "artifact_name": Value::Null,
                "source_ids": [source_id]
            }])
        } else {
            json!([])
        },
        "next_actions": [{
            "action_id": action_id,
            "action_kind": if state == "actionable" { "choose_ready_bead" } else { "wait_for_owner" },
            "mutation_allowed": false,
            "operator_summary": "deterministic read-only blocker action",
            "suggested_command": "bv --robot-triage",
            "reason_codes": [reason_for(tuple.blocker)],
            "citation_ids": ["blocker-cite"]
        }],
        "forbidden_actions": [
            {"command_pattern": "am service restart", "reason": "shared Agent Mail singleton"},
            {"command_pattern": "git reset --hard", "reason": "destructive repo operation"}
        ],
        "citations": [{
            "citation_id": "blocker-cite",
            "source_id": source_id,
            "summary": "redacted blocker fixture citation",
            "redacted": true
        }],
        "unavailable_sources": if state == "mail_unavailable" {
            json!([{
                "source_kind": "agent_mail",
                "evidence_state": state,
                "reason_codes": [reason_for(tuple.blocker)],
                "failure_class": "unavailable_evidence"
            }])
        } else {
            json!([])
        },
        "redaction_policy": {
            "raw_pane_content_allowed": false,
            "raw_prompt_allowed": false,
            "bounded_citations_only": true,
            "secret_redaction_required": true,
            "command_output_max_bytes": 4096
        },
        "raw_pane_content_stored": false,
        "artifact_paths": ["target/test-logs/cross-family/blocker-radar.json"]
    })
}

fn synthesize_resource(tuple: MatrixTuple) -> Value {
    let pressure = resource_pressure(tuple.resource);
    let state = resource_schema_state(tuple.resource);

    json!({
        "schema_version": 1,
        "contract_id": "ft.resource_pressure_cockpit.v1",
        "generated_at_ms": generated_at(tuple),
        "source": "contract_families.matrix.resource_cockpit",
        "status": resource_operator_status(pressure),
        "proof_gate": resource_proof_gate(pressure, tuple.resource),
        "evidence_state": state,
        "summary": "deterministic resource cockpit matrix row",
        "next_operator_move": if matches!(pressure, "red" | "black") { "reduce fanout" } else { "observe" },
        "run_identity": {
            "run_id": format!("cross-family-{}", tuple.index),
            "evidence_level": "local_reduced",
            "git_head": Value::Null,
            "repo_snapshot_head": Value::Null,
            "artifact_paths": ["target/test-logs/cross-family/resource-cockpit.json"],
            "hardware_predicate": {
                "logical_cpus": Value::Null,
                "memory_gib": Value::Null,
                "target_class": false,
                "proof_status": "skipped_not_proven"
            }
        },
        "domains": resource_domains(state, pressure),
        "residency_buckets": [],
        "queue_backpressure": [],
        "admission_decisions": [],
        "action_receipts": [],
        "artifact_paths": ["target/test-logs/cross-family/resource-cockpit.json"]
    })
}

fn resource_domains(state: &str, pressure: &str) -> Value {
    let domains = [
        "memory",
        "rss_residency",
        "pane_budget",
        "queue_backpressure",
        "storage_io",
        "worker_pool",
        "capacity_admission",
        "resource_admission",
        "action_receipts",
    ];
    let map = domains
        .into_iter()
        .map(|domain| {
            (
                domain.to_string(),
                json!({
                    "name": domain,
                    "evidence_state": state,
                    "pressure_tier": pressure,
                    "summary": format!("{domain} deterministic matrix summary"),
                    "operator_action": if matches!(pressure, "red" | "black") { "reduce fanout" } else { "observe" },
                    "reason_codes": [format!("resource.{domain}.{state}")]
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    Value::Object(map)
}

fn synthesize_contracts(tuple: MatrixTuple) -> ContractSet {
    ContractSet {
        context: synthesize_context(tuple),
        capture: synthesize_capture(tuple),
        herd: synthesize_herd(tuple),
        blocker: synthesize_blocker(tuple),
        resource: synthesize_resource(tuple),
    }
}

fn contract_by_family<'a>(contracts: &'a ContractSet, family: &str) -> &'a Value {
    match family {
        "context-horizon" => &contracts.context,
        "capture-fairness" => &contracts.capture,
        "herd-wave" => &contracts.herd,
        "blocker-radar" => &contracts.blocker,
        "resource-cockpit" => &contracts.resource,
        _ => panic!("unknown family {family}"),
    }
}

fn shape_signature(value: &Value) -> BTreeSet<String> {
    fn visit(value: &Value, path: &str, out: &mut BTreeSet<String>) {
        let kind = match value {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        out.insert(format!("{path}:{kind}"));
        match value {
            Value::Array(items) => {
                for item in items {
                    visit(item, &format!("{path}[]"), out);
                }
            }
            Value::Object(map) => {
                for (key, nested) in map {
                    let next = if path.is_empty() {
                        key.to_string()
                    } else {
                        format!("{path}.{key}")
                    };
                    visit(nested, &next, out);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    let mut out = BTreeSet::new();
    visit(value, "", &mut out);
    out
}

fn assert_toon_shape_parity(label: &str, value: &Value) {
    let toon = toon_rust::encode(value.clone(), None);
    let decoded = toon_rust::try_decode(&toon, None)
        .unwrap_or_else(|err| panic!("{label} TOON decode failed: {err}"));
    let decoded_json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let decoded_value: Value = serde_json::from_str(&decoded_json)
        .unwrap_or_else(|err| panic!("{label} decoded TOON JSON parse failed: {err}"));
    assert_eq!(
        shape_signature(value),
        shape_signature(&decoded_value),
        "{label} TOON shape drifted"
    );
}

fn bool_at(value: &Value, path: &[&str]) -> bool {
    let mut current = value;
    for segment in path {
        current = &current[*segment];
    }
    current
        .as_bool()
        .unwrap_or_else(|| panic!("{} must be bool in {value:#}", path.join(".")))
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut current = value;
    for segment in path {
        current = &current[*segment];
    }
    current
        .as_str()
        .unwrap_or_else(|| panic!("{} must be string in {value:#}", path.join(".")))
}

fn capture_starvation_from_contract(value: &Value) -> bool {
    value["scheduler_snapshot"]["tiers"]
        .as_array()
        .expect("capture tiers array")
        .iter()
        .any(|tier| tier["starvation_risk"].as_bool() == Some(true))
}

fn herd_has_next_action(value: &Value, expected: &[&str]) -> bool {
    value["next_actions"]
        .as_array()
        .expect("herd next_actions array")
        .iter()
        .any(|action| {
            action["action_kind"]
                .as_str()
                .is_some_and(|kind| expected.contains(&kind))
        })
}

fn has_stale_or_unavailable_reason(tuple: MatrixTuple, contracts: &ContractSet) -> bool {
    let context_ok = unavailable_reason_marker(tuple.context).is_empty()
        || !contracts.context["unavailable_domains"]
            .as_array()
            .expect("context unavailable domains")
            .is_empty();

    let capture_ok = unavailable_reason_marker(tuple.capture).is_empty()
        || contracts.capture["scheduler_snapshot"]["tiers"]
            .as_array()
            .expect("capture tiers array")
            .iter()
            .any(|tier| {
                tier["reason_codes"]
                    .as_array()
                    .expect("capture reason codes")
                    .iter()
                    .any(|reason| reason.as_str() == Some(&reason_for(tuple.capture)))
            });

    let herd_ok = unavailable_reason_marker(tuple.herd).is_empty()
        || !contracts.herd["unavailable_sources"]
            .as_array()
            .expect("herd unavailable sources")
            .is_empty();

    let blocker_ok = unavailable_reason_marker(tuple.blocker).is_empty()
        || !contracts.blocker["sources"]
            .as_array()
            .expect("blocker sources")
            .is_empty();

    let resource_ok = unavailable_reason_marker(tuple.resource).is_empty()
        || contracts.resource["domains"]["memory"]["reason_codes"]
            .as_array()
            .expect("resource reason codes")
            .iter()
            .any(|reason| {
                reason
                    .as_str()
                    .is_some_and(|text| text.ends_with(resource_schema_state(tuple.resource)))
            });

    context_ok && capture_ok && herd_ok && blocker_ok && resource_ok
}

fn raw_content_flags_are_false(contracts: &ContractSet) -> bool {
    !bool_at(&contracts.context, &["raw_context_content_stored"])
        && !bool_at(&contracts.capture, &["raw_pane_content_stored"])
        && !bool_at(&contracts.herd, &["raw_pane_content_stored"])
        && !bool_at(&contracts.blocker, &["raw_pane_content_stored"])
}

fn invariant_holds(id: &str, tuple: MatrixTuple, contracts: &ContractSet) -> bool {
    let resource_pressure = string_at(&contracts.resource, &["domains", "memory", "pressure_tier"]);
    let capture_starvation = capture_starvation_from_contract(&contracts.capture);
    let herd_action = string_at(&contracts.herd, &["admission_action"]);
    let context_risk = string_at(&contracts.context, &["fleet_summary", "highest_risk_tier"]);
    let blocker_state = string_at(&contracts.blocker, &["overall_state"]);
    let herd_stagger = contracts.herd["recommended_stagger_ms"]
        .as_u64()
        .expect("herd stagger is u64");

    match id {
        "CF-001" => !matches!(resource_pressure, "red" | "black") || capture_starvation,
        "CF-002" => {
            !(matches!(resource_pressure, "red" | "black") && capture_starvation)
                || matches!(herd_action, "defer" | "degrade" | "shed" | "unavailable")
        }
        "CF-003" => context_risk != "black" || (herd_stagger >= 1000 && herd_action != "admit"),
        "CF-004" => {
            blocker_state == "actionable"
                || (herd_action != "admit"
                    && herd_has_next_action(&contracts.herd, &["observe", "pause_assignment"]))
        }
        "CF-005" => has_stale_or_unavailable_reason(tuple, contracts),
        "CF-006" => raw_content_flags_are_false(contracts),
        "CF-007" => true,
        _ => panic!("unknown invariant {id}"),
    }
}

fn parsed_invariant_ids() -> BTreeSet<String> {
    let path = workspace_root().join("docs/contract-families-cross-invariants.md");
    let text =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    text.lines()
        .filter(|line| line.starts_with("| CF-"))
        .map(|line| {
            line.split('|')
                .nth(1)
                .expect("markdown table id column")
                .trim()
                .to_string()
        })
        .collect()
}

#[test]
fn cross_family_matrix_validates_schema_invariants_and_toon_shape() {
    let parsed_ids = parsed_invariant_ids();
    assert_eq!(
        parsed_ids,
        INVARIANT_IDS
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>(),
        "documented invariant IDs must match the executable matrix"
    );

    let validators = validators();
    let mut pass_counts = BTreeMap::new();
    for id in INVARIANT_IDS {
        pass_counts.insert(id, 0_usize);
    }

    let tuples = matrix_tuples();
    assert_eq!(tuples.len(), 7_776);

    for tuple in tuples {
        let contracts = synthesize_contracts(tuple);
        for family in FAMILIES {
            let value = contract_by_family(&contracts, family.id);
            assert_schema_accepts(
                &format!("tuple {} {}", tuple.index, family.id),
                validators.get(family.id).expect("validator present"),
                value,
            );
            assert_toon_shape_parity(&format!("tuple {} {}", tuple.index, family.id), value);
        }

        for id in INVARIANT_IDS {
            assert!(
                invariant_holds(id, tuple, &contracts),
                "tuple {tuple:?} violated {id}"
            );
            *pass_counts.get_mut(id).expect("pass count exists") += 1;
        }
    }

    for (id, count) in pass_counts {
        assert_eq!(count, 7_776, "{id} must pass every matrix tuple");
    }
}

#[test]
fn cross_family_negative_fixture_flags_resource_capture_violation() {
    let tuple = MatrixTuple {
        index: 99_999,
        context: "measured",
        capture: "measured",
        herd: "measured",
        blocker: "measured",
        resource: "mixed",
    };
    let mut contracts = synthesize_contracts(tuple);
    contracts.capture["scheduler_snapshot"]["tiers"][0]["starvation_risk"] = json!(false);

    assert!(
        !invariant_holds("CF-001", tuple, &contracts),
        "negative fixture must catch resource-red without capture starvation risk"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    #[test]
    fn random_cross_family_tuples_preserve_invariants(
        index in 0usize..1_000_000usize,
        context in state_strategy(),
        capture in state_strategy(),
        herd in state_strategy(),
        blocker in state_strategy(),
        resource in state_strategy(),
    ) {
        let tuple = MatrixTuple {
            index,
            context,
            capture,
            herd,
            blocker,
            resource,
        };
        let contracts = synthesize_contracts(tuple);

        for id in INVARIANT_IDS {
            prop_assert!(
                invariant_holds(id, tuple, &contracts),
                "tuple {:?} violated {}",
                tuple,
                id
            );
        }
    }
}
