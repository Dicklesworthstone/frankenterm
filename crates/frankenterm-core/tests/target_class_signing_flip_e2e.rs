//! Perfect end-to-end test for the target-class signing → flip flow (ft-d7fow,
//! W9.4 family ft-7h5da.10.4.x). NO MOCKS: real on-disk bench-lane artifacts in a
//! temp repo root, the real scenario input builder, the real on-disk artifact
//! reader, the real planner, and the real surface serializer — wired exactly the
//! way the production `ft swarm envelope current` command wires them.
//!
//! ## What this covers that the existing tests do not
//!
//! - `tests/target_class_signing_flip_conformance.rs` (cc_1, ft-1t0za) stops at
//!   `operating_envelope_target_class_from_artifact` returning a `TargetClass`; it
//!   never feeds the result into `plan_operating_envelope` nor builds the
//!   production surface payload.
//! - The in-source `signing_artifact_flips_target_class_unproven_gate` test injects
//!   a *synthetic* `TargetClass` directly (`.proof_state(SkippedNotProven)`), so it
//!   never exercises the disk → reader → planner chain.
//!
//! This test reproduces `crates/frankenterm/src/main.rs::build_operating_envelope_payload`
//! verbatim for the `Current` scenario (the only scenario that resolves the target
//! class from the signed artifact) and asserts the FULL flip lifecycle on the
//! production plan + serialized surface:
//!
//!   1. FAIL-CLOSED-UNTIL-SIGNED — pristine repo, a `skipped_not_proven` artifact,
//!      and every partially-signed artifact all hold the envelope at
//!      `developer_laptop` (the high-scale CPU/memory envelope is never claimed).
//!   2. THE FLIP — a genuinely bench-lane-signed artifact (the exact schema
//!      `scripts/run-target-class-cockpit.sh` emits) flips the live plan to
//!      `target_64_core_256g` / `Measured`, citing the signed-artifact provenance.
//!   3. NO SYNTHETIC FLIP — breaking any single signing condition (or forging only
//!      cosmetic fields) drops the plan back to `developer_laptop`; the flip is also
//!      REVERSIBLE: reverting the artifact to `skipped_not_proven` re-closes the gate.
//!   4. STRUCTURAL FAIL-CLOSED INVARIANT — the production resolver never yields a
//!      "claiming-high-scale-but-unproven" class, so `capacity.target_class_unproven`
//!      can never appear in the production plan's decision reason codes.
//!
//! ## Regression guard (the bug this test caught)
//!
//! The bench-lane signer writes `high_scale_claim_allowed` under `evidence`, but
//! the reader originally read it from `/hardware_predicate/high_scale_claim_allowed`
//! — a path the real artifact never populates. `unwrap_or(false)` on the absent
//! field mapped a genuinely-signed artifact to `Unavailable`, so the gate could
//! never flip through the real harness (the campaign's entire payoff was
//! unreachable). `real_bench_lane_signed_artifact_flips_gate` pins the real schema
//! and would fail against the pre-ft-d7fow reader.

use std::fs;
use std::path::Path;

use frankenterm_core::operating_envelope::{
    OperatingEnvelopePlan, OperatingEnvelopeProofState, OperatingEnvelopeScenario,
    OperatingEnvelopeSurface, OperatingEnvelopeTargetProfile, TARGET_CLASS_ARTIFACT_POINTER,
    build_operating_envelope_surface_data, operating_envelope_input_for_scenario,
    operating_envelope_target_class_from_artifact, plan_operating_envelope,
};
use serde_json::{Value, json};

const NOW_MS: u64 = 1_781_000_000_000;
/// Where the bench-lane signer writes each run's `summary.json` (the
/// `artifact_pattern` recorded in the committed pointer artifact).
const SUMMARY_REL: &str =
    "tests/e2e/artifacts/target-class/linux-x86_64-high-core/20260616T120000Z/summary.json";

// ───────────────────────── production wiring (no mocks) ──────────────────────

/// Mirror of `build_operating_envelope_payload` for the `Current` scenario: the
/// real scenario input builder, then the live target class resolved from the
/// on-disk artifact rooted at `repo_root`, then the real planner. This is the
/// exact production path — only `repo_root` is redirected to a temp dir so the
/// test owns the artifact bytes.
fn production_plan(repo_root: &Path) -> OperatingEnvelopePlan {
    let input = operating_envelope_input_for_scenario(
        NOW_MS,
        "operating-envelope-current",
        "swarm.scale_up",
        OperatingEnvelopeScenario::Current,
    )
    .target_class(operating_envelope_target_class_from_artifact(repo_root));
    plan_operating_envelope(input)
}

/// The real surface payload `ft swarm envelope current` serializes for the CLI/MCP.
fn production_payload(plan: &OperatingEnvelopePlan) -> Value {
    build_operating_envelope_surface_data(
        plan,
        OperatingEnvelopeSurface::Status,
        "ft swarm envelope current",
        None,
    )
}

fn log_stage(stage: &str, plan: &OperatingEnvelopePlan) {
    eprintln!(
        "[e2e target-class flip] stage={stage} profile={:?} proof_state={:?} \
         cpu_cores={} memory_gib={} agents={} target_reasons={:?} decision_reasons={:?}",
        plan.target_class.profile,
        plan.target_class.proof_state,
        plan.target_class.requested_cpu_cores,
        plan.target_class.requested_memory_gib,
        plan.target_class.requested_agent_count,
        plan.target_class.reason_codes,
        plan.decision.reason_codes,
    );
}

// ───────────────────────── on-disk bench-lane artifacts ──────────────────────

/// Write a realistic bench-lane pointer + `summary.json` into a temp repo root,
/// matching the schema `scripts/run-target-class-cockpit.sh` actually emits.
fn place_artifact(repo: &Path, pointer_status: &str, summary: &Value) {
    let pointer = repo.join(TARGET_CLASS_ARTIFACT_POINTER);
    fs::create_dir_all(pointer.parent().expect("pointer parent")).expect("mkdir pointer");
    fs::write(
        &pointer,
        serde_json::to_vec_pretty(&json!({
            "schema_version": "1.0.0",
            "kind": "resource-cockpit-target-class-gate",
            "status": pointer_status,
            "current_artifact": { "path": SUMMARY_REL, "status": pointer_status },
        }))
        .expect("ser pointer"),
    )
    .expect("write pointer");

    let summary_path = repo.join(SUMMARY_REL);
    fs::create_dir_all(summary_path.parent().expect("summary parent")).expect("mkdir summary");
    fs::write(
        &summary_path,
        serde_json::to_vec_pretty(summary).expect("ser summary"),
    )
    .expect("write summary");
}

/// A genuinely-signed bench-lane summary: the host met the 64-CPU/256-GiB SKU, all
/// gates passed, the artifact is `ready_to_sign`. NOTE: `high_scale_claim_allowed`
/// lives under `evidence` (where the real signer writes it) and is intentionally
/// ABSENT from `hardware_predicate` — this is the exact shape that exposed the
/// reader path bug.
fn signed_summary() -> Value {
    json!({
        "schema_version": "1.0.0",
        "bead_id": "ft-tf6g3.14",
        "run_id": "20260616T120000Z",
        "status": "passed",
        "exit_code": 0,
        "ready_to_sign": true,
        "sku": { "id": "linux-x86_64-high-core", "high_scale_sku": true },
        "observed_host": { "logical_cpus": 64, "memory_gib": 256 },
        "hardware_predicate": {
            "required_logical_cpus": 64,
            "required_memory_gib": 256,
            "observed_logical_cpus": 64,
            "observed_memory_gib": 256,
            "target_class": true,
            "proof_status": "proven_predicate_met",
            "failure_reasons": []
        },
        "evidence": {
            "target_hardware": "target_hardware",
            "skipped_not_proven": false,
            "high_scale_claim_allowed": true,
            "conformance": "passed",
            "bench_budget": "passed",
            "high_scale_rehearsal": "passed"
        }
    })
}

/// The committed-in-repo state today: the host is too small, so the run is
/// retained as `skipped_not_proven` and is not signable.
fn skipped_summary() -> Value {
    json!({
        "schema_version": "1.0.0",
        "status": "skipped",
        "exit_code": 0,
        "ready_to_sign": false,
        "sku": { "id": "linux-x86_64-high-core", "high_scale_sku": true },
        "observed_host": { "logical_cpus": 14, "memory_gib": 64 },
        "hardware_predicate": {
            "required_logical_cpus": 64,
            "required_memory_gib": 256,
            "observed_logical_cpus": 14,
            "observed_memory_gib": 64,
            "target_class": false,
            "proof_status": "skipped_not_proven",
            "failure_reasons": [
                "target_class.cpu_below_sku_floor",
                "target_class.memory_below_sku_floor"
            ]
        },
        "evidence": {
            "target_hardware": "skipped_not_proven",
            "skipped_not_proven": true,
            "high_scale_claim_allowed": false
        }
    })
}

// ───────────────────────────── assertion helpers ─────────────────────────────

fn assert_developer_laptop(plan: &OperatingEnvelopePlan, ctx: &str) {
    assert_eq!(
        plan.target_class.profile,
        OperatingEnvelopeTargetProfile::DeveloperLaptop,
        "{ctx}: the high-scale class must be held back"
    );
    assert_ne!(
        plan.target_class.proof_state,
        OperatingEnvelopeProofState::Measured,
        "{ctx}: a held-back class must never report Measured"
    );
    assert_eq!(
        plan.target_class.requested_cpu_cores, 8,
        "{ctx}: the high-scale 64-core envelope must not be claimed"
    );
    assert_eq!(
        plan.target_class.requested_memory_gib, 32,
        "{ctx}: the high-scale 256-GiB envelope must not be claimed"
    );
    assert!(
        !plan
            .target_class
            .reason_codes
            .iter()
            .any(|c| c == "target_hardware.proven_from_signed_artifact"),
        "{ctx}: a held-back class must not cite signed-artifact provenance: {:?}",
        plan.target_class.reason_codes
    );
}

fn assert_high_scale_measured(plan: &OperatingEnvelopePlan, ctx: &str) {
    assert_eq!(
        plan.target_class.profile,
        OperatingEnvelopeTargetProfile::Target64Core256g,
        "{ctx}: the signed artifact must flip to the high-scale class"
    );
    assert_eq!(
        plan.target_class.proof_state,
        OperatingEnvelopeProofState::Measured,
        "{ctx}: a signed high-scale class must report Measured"
    );
    assert_eq!(
        plan.target_class.requested_cpu_cores, 64,
        "{ctx}: the flipped envelope must claim 64 cores"
    );
    assert_eq!(
        plan.target_class.requested_memory_gib, 256,
        "{ctx}: the flipped envelope must claim 256 GiB"
    );
    assert!(
        plan.target_class
            .reason_codes
            .iter()
            .any(|c| c == "target_hardware.proven_from_signed_artifact"),
        "{ctx}: a Measured class must cite the signed-artifact provenance: {:?}",
        plan.target_class.reason_codes
    );
}

/// The structural fail-closed invariant: the production resolver only ever yields
/// `developer_laptop` (NotRequired) or `target_64_core_256g` (Measured) — never a
/// high-scale class with an unproven state — so the planner's
/// "claiming-but-unproven" reason is unreachable on the production path.
fn assert_no_unproven_gate(plan: &OperatingEnvelopePlan, ctx: &str) {
    assert!(
        !plan
            .decision
            .reason_codes
            .iter()
            .any(|c| c == "capacity.target_class_unproven"),
        "{ctx}: the production resolver must never surface a claiming-but-unproven \
         high-scale class: {:?}",
        plan.decision.reason_codes
    );
}

// ─────────────────────────────────── tests ───────────────────────────────────

/// Headline: drive the whole signing → flip → revocation lifecycle through the
/// exact production wiring, asserting the live plan at every stage.
#[test]
fn full_flip_lifecycle_through_production_wiring() {
    // Stage 0 — pristine repo, no artifact at all: fail-closed to developer_laptop.
    let repo = tempfile::tempdir().expect("tempdir");
    let plan = production_plan(repo.path());
    log_stage("0-pristine", &plan);
    assert_developer_laptop(&plan, "stage 0 (no artifact)");
    assert_no_unproven_gate(&plan, "stage 0 (no artifact)");

    // Stage 1 — the host is too small; bench lane retained `skipped_not_proven`.
    place_artifact(repo.path(), "skipped_not_proven", &skipped_summary());
    let plan = production_plan(repo.path());
    log_stage("1-skipped", &plan);
    assert_developer_laptop(&plan, "stage 1 (skipped_not_proven)");
    assert_no_unproven_gate(&plan, "stage 1 (skipped_not_proven)");

    // Stage 2 — predicate proven but a downstream gate failed, so NOT ready_to_sign.
    let mut partial = signed_summary();
    partial["status"] = json!("failed");
    partial["ready_to_sign"] = json!(false);
    partial["evidence"]["bench_budget"] = json!("failed");
    place_artifact(repo.path(), "proven_predicate_met", &partial);
    let plan = production_plan(repo.path());
    log_stage("2-proven-but-unsigned", &plan);
    assert_developer_laptop(&plan, "stage 2 (proven but not ready_to_sign)");
    assert_no_unproven_gate(&plan, "stage 2 (proven but not ready_to_sign)");

    // Stage 3 — fully bench-lane signed: THE FLIP to high-scale Measured.
    place_artifact(repo.path(), "proven_predicate_met", &signed_summary());
    let plan = production_plan(repo.path());
    log_stage("3-signed", &plan);
    assert_high_scale_measured(&plan, "stage 3 (signed)");
    assert_no_unproven_gate(&plan, "stage 3 (signed)");
    // The full production surface must still serialize and stay dry-run/no-side-effect.
    let payload = production_payload(&plan);
    assert_eq!(
        payload["dry_run"],
        json!(true),
        "signed surface stays dry-run"
    );
    assert_eq!(
        payload["live_mutation_allowed"],
        json!(false),
        "signed surface never allows live mutation"
    );
    assert_eq!(
        payload["side_effects_executed"],
        json!(false),
        "planning the signed envelope executes no side effects"
    );
    assert!(
        !payload["summary"]["blocked_reason_codes"]
            .as_array()
            .expect("blocked_reason_codes array")
            .iter()
            .any(|c| c.as_str() == Some("capacity.target_class_unproven")),
        "signed surface must not carry the unproven gate reason: {}",
        payload["summary"]["blocked_reason_codes"]
    );

    // Stage 4 — REVOCATION: revert the artifact to skipped. The gate re-closes;
    // the flip is not sticky.
    place_artifact(repo.path(), "skipped_not_proven", &skipped_summary());
    let plan = production_plan(repo.path());
    log_stage("4-revoked", &plan);
    assert_developer_laptop(&plan, "stage 4 (reverted to skipped)");
    assert_no_unproven_gate(&plan, "stage 4 (reverted to skipped)");
}

/// Regression guard for the schema-location bug (ft-d7fow): the real bench-lane
/// signer writes `high_scale_claim_allowed` under `evidence`, NOT under
/// `hardware_predicate`. The pre-fix reader looked only at `hardware_predicate`,
/// so this exact (genuinely-signed) artifact mapped to `Unavailable` and the gate
/// never flipped. This test pins the real schema end-to-end.
#[test]
fn real_bench_lane_signed_artifact_flips_gate() {
    let signed = signed_summary();
    // Pin the property the bug hinged on: the field is ONLY under `evidence`.
    assert_eq!(
        signed["evidence"]["high_scale_claim_allowed"],
        json!(true),
        "the real signer writes the claim flag under `evidence`"
    );
    assert!(
        signed["hardware_predicate"]
            .get("high_scale_claim_allowed")
            .is_none(),
        "the real signer does NOT mirror the claim flag into `hardware_predicate`"
    );

    let repo = tempfile::tempdir().expect("tempdir");
    place_artifact(repo.path(), "proven_predicate_met", &signed);
    let plan = production_plan(repo.path());
    log_stage("real-signed", &plan);
    assert_high_scale_measured(&plan, "real bench-lane signed artifact");
    assert_no_unproven_gate(&plan, "real bench-lane signed artifact");
}

/// No synthetic flip: every artifact that is not fully signed must keep the
/// production envelope at `developer_laptop`. Each case mutates exactly one
/// signing condition off the genuinely-signed base.
#[test]
fn fail_closed_until_signed_rejects_every_partial_artifact() {
    // (label, pointer status, summary) — each must NOT flip.
    let mut not_ready = signed_summary();
    not_ready["ready_to_sign"] = json!(false);

    let mut claim_vetoed = signed_summary();
    claim_vetoed["evidence"]["high_scale_claim_allowed"] = json!(false);

    let mut not_proven = signed_summary();
    not_proven["hardware_predicate"]["proof_status"] = json!("skipped_not_proven");

    let mut proof_status_missing = signed_summary();
    proof_status_missing["hardware_predicate"]
        .as_object_mut()
        .expect("hardware_predicate object")
        .remove("proof_status");

    // Cosmetic-only forgery: top-level status/ready_to_sign say "signed" but there
    // is no hardware-predicate evidence at all.
    let cosmetic_forgery = json!({
        "status": "passed",
        "ready_to_sign": true,
        "evidence": { "high_scale_claim_allowed": true }
    });

    let cases: [(&str, &str, Value); 6] = [
        (
            "skipped_not_proven",
            "skipped_not_proven",
            skipped_summary(),
        ),
        ("not ready_to_sign", "proven_predicate_met", not_ready),
        ("claim vetoed", "proven_predicate_met", claim_vetoed),
        ("proof downgraded", "skipped_not_proven", not_proven),
        (
            "proof_status removed",
            "proven_predicate_met",
            proof_status_missing,
        ),
        ("cosmetic forgery", "proven_predicate_met", cosmetic_forgery),
    ];

    for (label, pointer_status, summary) in cases {
        let repo = tempfile::tempdir().expect("tempdir");
        place_artifact(repo.path(), pointer_status, &summary);
        let plan = production_plan(repo.path());
        log_stage(label, &plan);
        assert_developer_laptop(&plan, label);
        assert_no_unproven_gate(&plan, label);
    }
}

/// A corrupt or truncated on-disk artifact must fail closed through the whole
/// production path — never a panic, never a synthetic high-scale flip.
#[test]
fn corrupt_artifact_fails_closed_through_production_path() {
    // Corrupt (non-JSON) pointer.
    let corrupt = tempfile::tempdir().expect("tempdir");
    let pointer = corrupt.path().join(TARGET_CLASS_ARTIFACT_POINTER);
    fs::create_dir_all(pointer.parent().expect("parent")).expect("mkdir");
    fs::write(&pointer, b"{ this is not valid json").expect("write");
    let plan = production_plan(corrupt.path());
    log_stage("corrupt-pointer", &plan);
    assert_developer_laptop(&plan, "corrupt pointer");
    assert_no_unproven_gate(&plan, "corrupt pointer");

    // Valid pointer, truncated summary → falls back to the pointer status, never flips.
    let truncated = tempfile::tempdir().expect("tempdir");
    let pointer = truncated.path().join(TARGET_CLASS_ARTIFACT_POINTER);
    fs::create_dir_all(pointer.parent().expect("parent")).expect("mkdir");
    fs::write(
        &pointer,
        serde_json::to_vec(&json!({
            "status": "skipped_not_proven",
            "current_artifact": { "path": SUMMARY_REL, "status": "skipped_not_proven" }
        }))
        .expect("ser"),
    )
    .expect("write pointer");
    let summary_path = truncated.path().join(SUMMARY_REL);
    fs::create_dir_all(summary_path.parent().expect("parent")).expect("mkdir");
    fs::write(&summary_path, b"{\"ready_to_sign\": tr").expect("write truncated");
    let plan = production_plan(truncated.path());
    log_stage("truncated-summary", &plan);
    assert_developer_laptop(&plan, "truncated summary");
    assert_no_unproven_gate(&plan, "truncated summary");
}
