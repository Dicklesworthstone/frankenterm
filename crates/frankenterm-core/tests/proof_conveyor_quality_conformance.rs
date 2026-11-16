//! W8.T net-new conformance: deferred-proof quality classifier taxonomy
//! (ft-7h5da.9.6).
//!
//! The proof-quality classifier (W8.3, closed) turns a ProofIntent + one
//! retained replay attempt into a typed, fail-closed receipt. This is the
//! W8.T UNIT item "the classifier assigns every outcome to the taxonomy" plus
//! "ProofIntent serde + source-hash capture", exercised comprehensively through
//! the public API:
//!   * ONLY a confirmed remote pass (current source, valid artifact) is a valid
//!     remote proof — every one of the other nine attempt outcomes fails closed
//!     (local-proof substitution is impossible);
//!   * a stale source hash overrides even a remote pass (stale-hash flagged,
//!     not validated);
//!   * a local fallback or unconfirmed remote on a "passed" attempt never
//!     validates and is flagged;
//!   * an invalid/missing expected artifact on a pass never validates;
//!   * the release-impact and package/workspace scope axes classify correctly;
//!   * ProofIntent + ProofQualityReceipt serde round-trip and the receipt is
//!     content-addressed.
//!
//! Zero-RCH, public API only, standalone file, no source edits. The gated W8.T
//! remainder (envelope admission emitting a DEFERRED receipt with NO local
//! fallback, replay-only-when-remote-admitted, the admission explainer's
//! predicted-with-timestamp verdict, and no-bead-closeout-on-non-valid-proof)
//! rides the runtime/queue sub-beads .9.2/.9.4/.9.5 (blocked) and is out of
//! scope for this classifier slice.

use frankenterm_core::deferred_proof_replay::{
    DEFERRED_PROOF_REPLAY_ATTEMPT_CONTRACT_ID, DEFERRED_PROOF_REPLAY_SCHEMA_VERSION,
    DeferredProofReplayAttemptOutcome as Outcome, DeferredProofReplayAttemptRecord,
};
use frankenterm_core::proof_intent::{ProofIntent, ProofKind, ProofRedactionPolicy, ProofScope};
use frankenterm_core::proof_quality::{
    PROOF_QUALITY_CONTRACT_ID, ProofArtifactObservation, ProofExecutionVenue, ProofQualityInput,
    ProofQualityReceipt, ProofQualityScope, ProofReleaseImpact, ProofSourceState,
    ProofTerminalClass, classify_proof_quality,
};

const SRC: &str = "sha256:current-tree";

const ALL_OUTCOMES: [Outcome; 10] = [
    Outcome::RemoteProofPassed,
    Outcome::RemoteProofFailed,
    Outcome::BlockedLocalFallback,
    Outcome::BlockedWorkerNull,
    Outcome::BlockedNoAdmissibleWorkers,
    Outcome::BlockedExit143,
    Outcome::BlockedRemoteTimeout,
    Outcome::BlockedStuckDetectorCancelled,
    Outcome::BlockedTopologyPreflight,
    Outcome::BlockedRemoteNotConfirmed,
];

fn intent(
    scope: ProofScope,
    expected_artifact: Option<String>,
    attestation: Option<String>,
) -> ProofIntent {
    ProofIntent::new(
        "RCH_REQUIRE_REMOTE=1 rch exec -- cargo test -p frankenterm-core",
        scope,
        ProofKind::Test,
        SRC,
        expected_artifact,
        true, // required_remote
        Some("ft-7h5da.9.6".to_string()),
        attestation,
        ProofRedactionPolicy::Standard,
        1_704_000_000_000,
    )
}

fn pkg_intent() -> ProofIntent {
    intent(
        ProofScope::Package {
            package: "frankenterm-core".to_string(),
        },
        None,
        None,
    )
}

fn attempt(
    outcome: Outcome,
    remote_reached: bool,
    local_fallback: bool,
    exit: Option<i32>,
) -> DeferredProofReplayAttemptRecord {
    DeferredProofReplayAttemptRecord {
        schema_version: DEFERRED_PROOF_REPLAY_SCHEMA_VERSION,
        contract_id: DEFERRED_PROOF_REPLAY_ATTEMPT_CONTRACT_ID.to_string(),
        bead_id: "ft-7h5da.9.6".to_string(),
        receipt_id: "receipt-1".to_string(),
        source_receipt_digest: "sha256:digest".to_string(),
        source_text_sha256: None,
        command_argv: vec!["rch".to_string(), "exec".to_string()],
        command_env: Vec::new(),
        target_dir: Some("/tmp/ft-proof".to_string()),
        selected_worker: remote_reached.then(|| "vmi-test".to_string()),
        rch_job_id: remote_reached.then(|| "j-1".to_string()),
        remote_exit_status: exit,
        outcome,
        blockers: Vec::new(),
        remote_cargo_reached: remote_reached,
        local_fallback_detected: local_fallback,
        stdout_path: None,
        stderr_path: None,
        attempt_record_path: None,
        note: "conformance attempt".to_string(),
    }
}

/// CORE FAIL-CLOSED: only a confirmed remote pass validates. Under an otherwise
/// favorable setup (current source, valid artifact), every one of the ten
/// attempt outcomes maps to a typed terminal class, and ONLY RemoteProofPassed
/// is a valid remote proof — local-proof substitution is impossible.
#[test]
fn only_confirmed_remote_pass_validates_every_other_outcome_fails_closed() {
    let intent = pkg_intent(); // no expected artifact -> artifact NotExpected
    for outcome in ALL_OUTCOMES {
        let is_pass = outcome == Outcome::RemoteProofPassed;
        let remote_reached = matches!(
            outcome,
            Outcome::RemoteProofPassed | Outcome::RemoteProofFailed
        );
        let local_fallback = outcome == Outcome::BlockedLocalFallback;
        let exit = Some(i32::from(!is_pass));
        let attempt = attempt(outcome, remote_reached, local_fallback, exit);

        let receipt = classify_proof_quality(ProofQualityInput {
            intent: &intent,
            live_source_hash: SRC, // current
            attempt: &attempt,
            artifact_observation: ProofArtifactObservation::PresentValid,
            release_impact: None,
        });

        assert_eq!(
            receipt.valid_remote_proof, is_pass,
            "only RemoteProofPassed may validate; {outcome:?} must fail closed"
        );
        if is_pass {
            assert_eq!(receipt.terminal_class, ProofTerminalClass::ValidRemoteProof);
            assert_eq!(receipt.execution_venue, ProofExecutionVenue::RemoteWorker);
            assert!(
                receipt.blockers.is_empty(),
                "valid proof carries no blockers"
            );
        } else {
            assert_ne!(
                receipt.terminal_class,
                ProofTerminalClass::ValidRemoteProof,
                "{outcome:?} must not be classed ValidRemoteProof"
            );
            assert!(
                !receipt.blockers.is_empty(),
                "{outcome:?} must record at least one typed blocker"
            );
        }
    }
}

/// A stale source hash overrides even a confirmed remote pass — the proof was
/// minted against a different tree, so it is flagged stale, not validated.
#[test]
fn stale_source_overrides_even_a_remote_pass() {
    let intent = pkg_intent();
    let attempt = attempt(Outcome::RemoteProofPassed, true, false, Some(0));
    let receipt = classify_proof_quality(ProofQualityInput {
        intent: &intent,
        live_source_hash: "sha256:drifted-tree", // != intent source hash
        attempt: &attempt,
        artifact_observation: ProofArtifactObservation::PresentValid,
        release_impact: None,
    });
    assert_eq!(receipt.source_state, ProofSourceState::Stale);
    assert_eq!(receipt.terminal_class, ProofTerminalClass::StaleSource);
    assert!(!receipt.valid_remote_proof);
    assert!(receipt.blockers.iter().any(|b| b == "source.stale"));
}

/// A "passed" attempt that actually fell back to local execution never
/// validates and the local fallback is flagged.
#[test]
fn local_fallback_on_a_pass_never_validates() {
    let intent = pkg_intent();
    let attempt = attempt(Outcome::RemoteProofPassed, false, true, Some(0));
    let receipt = classify_proof_quality(ProofQualityInput {
        intent: &intent,
        live_source_hash: SRC,
        attempt: &attempt,
        artifact_observation: ProofArtifactObservation::PresentValid,
        release_impact: None,
    });
    assert_eq!(receipt.execution_venue, ProofExecutionVenue::LocalFallback);
    assert!(!receipt.valid_remote_proof);
    assert!(receipt.local_fallback_detected);
    assert!(receipt.blockers.iter().any(|b| b == "rch.local_fallback"));
}

/// A remote pass with an expected artifact that is missing never validates.
#[test]
fn missing_expected_artifact_on_a_pass_is_invalid() {
    let intent = intent(
        ProofScope::Workspace,
        Some("target/proof.json".to_string()),
        None,
    );
    let attempt = attempt(Outcome::RemoteProofPassed, true, false, Some(0));
    let receipt = classify_proof_quality(ProofQualityInput {
        intent: &intent,
        live_source_hash: SRC,
        attempt: &attempt,
        artifact_observation: ProofArtifactObservation::Missing,
        release_impact: None,
    });
    assert_eq!(receipt.terminal_class, ProofTerminalClass::InvalidArtifact);
    assert!(!receipt.valid_remote_proof);
    assert!(receipt.blockers.iter().any(|b| b == "artifact.missing"));
}

/// The release-impact and package/workspace scope axes classify from the
/// intent: an attestation slot makes a proof release-blocking, its absence
/// supplemental; the scope mirrors the intent's scope.
#[test]
fn release_impact_and_scope_axes_classify_from_intent() {
    let attempt = attempt(Outcome::RemoteProofPassed, true, false, Some(0));

    let supplemental = classify_proof_quality(ProofQualityInput {
        intent: &pkg_intent(), // no attestation slot, package scope
        live_source_hash: SRC,
        attempt: &attempt,
        artifact_observation: ProofArtifactObservation::PresentValid,
        release_impact: None,
    });
    assert_eq!(
        supplemental.release_impact,
        ProofReleaseImpact::Supplemental
    );
    assert_eq!(
        supplemental.proof_scope,
        ProofQualityScope::Package {
            package: "frankenterm-core".to_string()
        }
    );

    let release_blocking_intent = intent(
        ProofScope::Workspace,
        None,
        Some("v1.0.0/core-tests".to_string()),
    );
    let release_blocking = classify_proof_quality(ProofQualityInput {
        intent: &release_blocking_intent,
        live_source_hash: SRC,
        attempt: &attempt,
        artifact_observation: ProofArtifactObservation::PresentValid,
        release_impact: None,
    });
    assert_eq!(
        release_blocking.release_impact,
        ProofReleaseImpact::ReleaseBlocking
    );
    assert_eq!(release_blocking.proof_scope, ProofQualityScope::Workspace);
}

/// W8.T item 1: ProofIntent serde round-trips and its source-hash drives
/// `is_stale` (current vs drifted).
#[test]
fn proof_intent_serde_round_trips_and_source_hash_drives_is_stale() {
    let intent = pkg_intent();
    let json = serde_json::to_string(&intent).expect("serialize intent");
    let back: ProofIntent = serde_json::from_str(&json).expect("deserialize intent");
    assert_eq!(
        serde_json::to_value(&intent).unwrap(),
        serde_json::to_value(&back).unwrap(),
        "ProofIntent must serde round-trip losslessly"
    );
    assert!(!intent.is_stale(SRC), "matching source hash is not stale");
    assert!(
        intent.is_stale("sha256:something-else"),
        "a drifted source hash must be stale"
    );
}

/// The receipt is content-addressed and serde round-trips losslessly.
#[test]
fn proof_quality_receipt_is_content_addressed_and_round_trips() {
    let intent = pkg_intent();
    let attempt = attempt(Outcome::RemoteProofPassed, true, false, Some(0));
    let receipt = classify_proof_quality(ProofQualityInput {
        intent: &intent,
        live_source_hash: SRC,
        attempt: &attempt,
        artifact_observation: ProofArtifactObservation::PresentValid,
        release_impact: None,
    });
    assert_eq!(receipt.contract_id, PROOF_QUALITY_CONTRACT_ID);
    assert!(
        receipt.receipt_id.starts_with("proof-quality:"),
        "receipt id must be content-addressed: {}",
        receipt.receipt_id
    );
    let json = serde_json::to_string(&receipt).expect("serialize receipt");
    let back: ProofQualityReceipt = serde_json::from_str(&json).expect("deserialize receipt");
    assert_eq!(receipt, back, "ProofQualityReceipt must serde round-trip");
}
