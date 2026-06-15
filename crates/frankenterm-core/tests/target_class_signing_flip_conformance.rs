//! Conformance + metamorphic tests for the target-class signing/flip gate
//! (W9.4 — ft-1t0za). Adversarial coverage of the production reader landed under
//! ft-7h5da.10.4.4 (`operating_envelope.rs`) + the signing markers from
//! ft-7h5da.10.4.3 / ft-7h5da.10.4.2.
//!
//! Invariants asserted (the campaign's whole safety story):
//!   1. FAIL-CLOSED-UNTIL-SIGNED-PROOF — `Measured` requires ALL of
//!      `proof_status == "proven_predicate_met"` AND `ready_to_sign == true` AND
//!      `hardware_predicate.high_scale_claim_allowed == true`. Anything else maps
//!      to `SkippedNotProven` or `Unavailable`, and the envelope target class
//!      stays `developer_laptop` (never `target_64_core_256g`).
//!   2. NO SYNTHETIC FLIP — breaking/removing ANY single required condition from
//!      a signed summary flips the state OFF `Measured`. Tampering only the
//!      cosmetic top-level `status` never yields `Measured`.
//!   3. SIGNATURE/MARKER REQUIRED + FAIL-CLOSED I/O — a missing `ready_to_sign`
//!      marker, or a corrupt/absent artifact, is `Unavailable` (never `Measured`).

use frankenterm_core::operating_envelope::{
    OperatingEnvelopeProofState, OperatingEnvelopeTargetProfile, TARGET_CLASS_ARTIFACT_POINTER,
    operating_envelope_target_class_from_artifact, read_target_class_proof_state,
    target_class_proof_state_from_summary,
};
use serde_json::{Value, json};

/// The one input shape that is allowed to flip the gate: signed + proven +
/// predicate-met. Every other test mutates exactly one field of this base.
fn signed_summary() -> Value {
    json!({
        "status": "passed",
        "ready_to_sign": true,
        "hardware_predicate": {
            "proof_status": "proven_predicate_met",
            "high_scale_claim_allowed": true
        }
    })
}

// ───────────────────────── Conformance: the mapping table ─────────────────────

#[test]
fn fully_signed_summary_maps_to_measured() {
    assert_eq!(
        target_class_proof_state_from_summary(&signed_summary()),
        OperatingEnvelopeProofState::Measured,
        "the all-conditions-true signed summary is the only Measured input"
    );
}

#[test]
fn skipped_not_proven_maps_to_skipped() {
    let summary = json!({
        "ready_to_sign": false,
        "hardware_predicate": { "proof_status": "skipped_not_proven" }
    });
    assert_eq!(
        target_class_proof_state_from_summary(&summary),
        OperatingEnvelopeProofState::SkippedNotProven
    );
}

#[test]
fn unknown_or_missing_proof_status_is_unavailable_fail_closed() {
    for summary in [
        json!({}),
        json!({ "ready_to_sign": true }),
        json!({ "hardware_predicate": { "proof_status": "garbage" } }),
        json!({ "hardware_predicate": { "proof_status": null } }),
        // proof_status nested at the wrong path is not consulted → Unavailable.
        json!({ "proof_status": "proven_predicate_met", "ready_to_sign": true }),
    ] {
        assert_eq!(
            target_class_proof_state_from_summary(&summary),
            OperatingEnvelopeProofState::Unavailable,
            "unknown/missing proof_status must fail closed: {summary}"
        );
    }
}

// ─────────────── Metamorphic: breaking ANY one gate drops Measured ────────────

/// The metamorphic relation: from a `Measured` summary, mutating exactly one of
/// the three required conditions must change the verdict away from `Measured`.
#[test]
fn breaking_any_single_required_condition_revokes_measured() {
    // Sanity: the base really is Measured before each mutation.
    assert_eq!(
        target_class_proof_state_from_summary(&signed_summary()),
        OperatingEnvelopeProofState::Measured
    );

    let mutations: Vec<(&str, Box<dyn Fn(&mut Value)>)> = vec![
        (
            "ready_to_sign=false",
            Box::new(|s: &mut Value| s["ready_to_sign"] = json!(false)),
        ),
        (
            "ready_to_sign removed",
            Box::new(|s: &mut Value| {
                s.as_object_mut().unwrap().remove("ready_to_sign");
            }),
        ),
        (
            "high_scale_claim_allowed=false",
            Box::new(|s: &mut Value| {
                s["hardware_predicate"]["high_scale_claim_allowed"] = json!(false);
            }),
        ),
        (
            "high_scale_claim_allowed removed",
            Box::new(|s: &mut Value| {
                s["hardware_predicate"]
                    .as_object_mut()
                    .unwrap()
                    .remove("high_scale_claim_allowed");
            }),
        ),
        (
            "proof_status removed",
            Box::new(|s: &mut Value| {
                s["hardware_predicate"]
                    .as_object_mut()
                    .unwrap()
                    .remove("proof_status");
            }),
        ),
    ];

    for (label, mutate) in mutations {
        let mut summary = signed_summary();
        mutate(&mut summary);
        assert_ne!(
            target_class_proof_state_from_summary(&summary),
            OperatingEnvelopeProofState::Measured,
            "breaking '{label}' must revoke Measured (no synthetic flip): {summary}"
        );
    }
}

#[test]
fn proof_status_downgrade_to_skipped_is_not_measured() {
    let mut summary = signed_summary();
    summary["hardware_predicate"]["proof_status"] = json!("skipped_not_proven");
    assert_eq!(
        target_class_proof_state_from_summary(&summary),
        OperatingEnvelopeProofState::SkippedNotProven,
        "even with ready_to_sign+high_scale true, a skipped proof is not Measured"
    );
}

/// Cosmetic `status` is not a gate: flipping it to "passed"/"proven" without the
/// hardware-predicate evidence must NOT synthesize a Measured verdict.
#[test]
fn cosmetic_status_field_cannot_synthesize_a_flip() {
    for forged in ["passed", "proven", "proven_predicate_met", "ok", "green"] {
        // hardware_predicate intentionally absent / unproven: only the cosmetic
        // top-level status is forged.
        let summary = json!({
            "status": forged,
            "ready_to_sign": true
        });
        assert_eq!(
            target_class_proof_state_from_summary(&summary),
            OperatingEnvelopeProofState::Unavailable,
            "forged top-level status='{forged}' must not flip the gate"
        );
    }
}

// ───────────── Signature/marker required + fail-closed disk I/O ───────────────

fn write_pointer_and_summary(repo: &std::path::Path, summary: Option<&Value>) {
    let pointer_path = repo.join(TARGET_CLASS_ARTIFACT_POINTER);
    std::fs::create_dir_all(pointer_path.parent().unwrap()).unwrap();
    let summary_rel = "tests/e2e/artifacts/target-class/sku/run/summary.json";
    std::fs::write(
        &pointer_path,
        serde_json::to_vec(&json!({
            "status": "skipped_not_proven",
            "current_artifact": { "path": summary_rel, "status": "skipped_not_proven" }
        }))
        .unwrap(),
    )
    .unwrap();
    if let Some(summary) = summary {
        let summary_path = repo.join(summary_rel);
        std::fs::create_dir_all(summary_path.parent().unwrap()).unwrap();
        std::fs::write(&summary_path, serde_json::to_vec(summary).unwrap()).unwrap();
    }
}

#[test]
fn signed_summary_on_disk_flips_gate_but_broken_one_does_not() {
    // Metamorphic over the full disk read path: the signed summary flips to
    // Measured; the same artifact with any single gate broken falls back.
    let signed = tempfile::tempdir().unwrap();
    write_pointer_and_summary(signed.path(), Some(&signed_summary()));
    assert_eq!(
        read_target_class_proof_state(signed.path()),
        OperatingEnvelopeProofState::Measured
    );

    let mut broken_summary = signed_summary();
    broken_summary["ready_to_sign"] = json!(false);
    let broken = tempfile::tempdir().unwrap();
    write_pointer_and_summary(broken.path(), Some(&broken_summary));
    assert_ne!(
        read_target_class_proof_state(broken.path()),
        OperatingEnvelopeProofState::Measured,
        "an unsigned (ready_to_sign=false) summary must not flip on the disk path"
    );
}

#[test]
fn corrupt_or_absent_artifacts_are_unavailable_never_measured() {
    // Absent pointer entirely.
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(
        read_target_class_proof_state(empty.path()),
        OperatingEnvelopeProofState::Unavailable
    );

    // Corrupt (non-JSON) pointer.
    let corrupt = tempfile::tempdir().unwrap();
    let ptr = corrupt.path().join(TARGET_CLASS_ARTIFACT_POINTER);
    std::fs::create_dir_all(ptr.parent().unwrap()).unwrap();
    std::fs::write(&ptr, b"{ this is not json").unwrap();
    assert_eq!(
        read_target_class_proof_state(corrupt.path()),
        OperatingEnvelopeProofState::Unavailable
    );

    // Truncated summary referenced by a valid pointer → fall back to pointer
    // status (skipped_not_proven), never Measured.
    let truncated = tempfile::tempdir().unwrap();
    let ptr = truncated.path().join(TARGET_CLASS_ARTIFACT_POINTER);
    std::fs::create_dir_all(ptr.parent().unwrap()).unwrap();
    let summary_rel = "tests/e2e/artifacts/target-class/sku/run/summary.json";
    std::fs::write(
        &ptr,
        serde_json::to_vec(&json!({
            "status": "skipped_not_proven",
            "current_artifact": { "path": summary_rel, "status": "skipped_not_proven" }
        }))
        .unwrap(),
    )
    .unwrap();
    let summary_path = truncated.path().join(summary_rel);
    std::fs::create_dir_all(summary_path.parent().unwrap()).unwrap();
    std::fs::write(&summary_path, b"{\"ready_to_sign\": tr").unwrap();
    assert_ne!(
        read_target_class_proof_state(truncated.path()),
        OperatingEnvelopeProofState::Measured,
        "a corrupt summary must never read as Measured"
    );
}

// ───────────── End-to-end: envelope target class fail-closed ──────────────────

#[test]
fn target_class_from_artifact_claims_high_scale_only_when_measured() {
    // Signed → target_64_core_256g + Measured + the proven reason code.
    let signed = tempfile::tempdir().unwrap();
    write_pointer_and_summary(signed.path(), Some(&signed_summary()));
    let claimed = operating_envelope_target_class_from_artifact(signed.path());
    assert_eq!(
        claimed.profile,
        OperatingEnvelopeTargetProfile::Target64Core256g
    );
    assert_eq!(claimed.proof_state, OperatingEnvelopeProofState::Measured);
    assert!(
        claimed
            .reason_codes
            .iter()
            .any(|c| c == "target_hardware.proven_from_signed_artifact"),
        "a measured claim must cite the signed-artifact provenance: {:?}",
        claimed.reason_codes
    );
}

#[test]
fn unproven_inputs_all_fall_back_to_developer_laptop() {
    // Metamorphic: every non-Measured artifact yields the developer-laptop
    // profile (NotRequired) — the default envelope is never silently upgraded.
    let mut broken_ready = signed_summary();
    broken_ready["ready_to_sign"] = json!(false);
    let mut broken_predicate = signed_summary();
    broken_predicate["hardware_predicate"]["high_scale_claim_allowed"] = json!(false);
    let skipped = json!({
        "ready_to_sign": false,
        "hardware_predicate": { "proof_status": "skipped_not_proven" }
    });

    for summary in [
        None,
        Some(broken_ready),
        Some(broken_predicate),
        Some(skipped),
    ] {
        let dir = tempfile::tempdir().unwrap();
        write_pointer_and_summary(dir.path(), summary.as_ref());
        let class = operating_envelope_target_class_from_artifact(dir.path());
        assert_eq!(
            class.profile,
            OperatingEnvelopeTargetProfile::DeveloperLaptop,
            "non-measured artifact must not claim high scale"
        );
        assert_ne!(
            class.proof_state,
            OperatingEnvelopeProofState::Measured,
            "non-measured artifact must not report Measured proof state"
        );
    }
}
