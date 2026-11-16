#![allow(clippy::too_many_lines)]

use frankenterm_core::config::SnapshotConfig;
use frankenterm_core::snapshot_engine::{
    SNAPSHOT_RECOVERY_CONTRACT_OWNER, SNAPSHOT_RECOVERY_CONTRACT_PROOF_MANIFEST,
    SnapshotRecoveryArtifactKind, SnapshotRecoveryCapability,
    SnapshotRecoveryCapabilityAvailability, SnapshotRecoveryClaimError,
    SnapshotRecoveryClientStateDisposition, SnapshotRecoveryCrossProductDisposition,
    SnapshotRecoveryDrillCurrency, SnapshotRecoveryDurabilityGrade, SnapshotRecoveryEvidence,
    SnapshotRecoveryFailureClass, SnapshotRecoveryFilesystemCapability, SnapshotRecoveryFreshness,
    SnapshotRecoveryKeyAvailability, SnapshotRecoveryPolicy, SnapshotRecoveryPolicyError,
    SnapshotRecoveryReadiness, SnapshotRecoveryRepairStatus, SnapshotRecoveryReplicaRank,
    SnapshotRecoveryScrubCoverage, SnapshotRecoverySemantics, SnapshotRecoveryVerdict,
    snapshot_recovery_contract_cell, snapshot_recovery_cross_product_cell,
    snapshot_recovery_failure_contract,
};
use std::collections::HashSet;

fn complete_recovery_evidence() -> SnapshotRecoveryEvidence {
    SnapshotRecoveryEvidence {
        artifact_kind: SnapshotRecoveryArtifactKind::WholeMuxRecoveryImage,
        artifact_validity: SnapshotRecoveryVerdict::Verified,
        repair_status: SnapshotRecoveryRepairStatus::NotRepaired,
        semantics: SnapshotRecoverySemantics::WholeMuxComplete,
        compatibility: SnapshotRecoveryVerdict::Verified,
        topology_authority: SnapshotRecoveryVerdict::Verified,
        guardian_census: SnapshotRecoveryVerdict::Verified,
        lease_replay_input_authority: SnapshotRecoveryVerdict::Verified,
        process_replacement_approval: SnapshotRecoveryVerdict::Verified,
        durability: SnapshotRecoveryDurabilityGrade::OffsiteVerified,
        freshness: SnapshotRecoveryFreshness::Verified,
        scrub_coverage: SnapshotRecoveryScrubCoverage::Current,
        drill_currency: SnapshotRecoveryDrillCurrency::Current,
        client_state: SnapshotRecoveryClientStateDisposition::PreservedAndVerified,
    }
}

#[test]
fn snapshot_recovery_policy_keeps_unproven_objectives_unset() {
    let policy = SnapshotRecoveryPolicy::default().validate().unwrap();
    assert_eq!(
        policy.periodic_interval_secs,
        SnapshotConfig::default().interval_seconds
    );
    assert_eq!(policy.target_local_rpo_secs, policy.periodic_interval_secs);
    assert_eq!(policy.target_replica_rpo_secs, None);
    assert_eq!(policy.max_full_anchor_age_secs, None);
    assert_eq!(policy.target_interactive_safe_rto_secs, None);
    assert_eq!(policy.target_complete_rto_secs, None);
    assert_eq!(policy.max_freshness_witness_age_secs, None);
    assert_eq!(policy.max_shallow_scrub_age_secs, None);
    assert_eq!(policy.max_deep_scrub_age_secs, None);
    assert_eq!(policy.max_disaster_drill_age_secs, None);

    let mut invalid = policy;
    invalid.target_local_rpo_secs = invalid.periodic_interval_secs - 1;
    assert_eq!(
        invalid.validate(),
        Err(SnapshotRecoveryPolicyError::LocalRpoBelowInterval)
    );
    invalid = policy;
    invalid.target_replica_rpo_secs = Some(invalid.target_local_rpo_secs - 1);
    assert_eq!(
        invalid.validate(),
        Err(SnapshotRecoveryPolicyError::ReplicaRpoBelowLocalRpo)
    );
    invalid = policy;
    invalid.target_interactive_safe_rto_secs = Some(60);
    invalid.target_complete_rto_secs = Some(59);
    assert_eq!(
        invalid.validate(),
        Err(SnapshotRecoveryPolicyError::CompleteRtoBelowInteractiveRto)
    );
}

#[test]
fn snapshot_recovery_contract_matrix_is_total_and_nonclaiming() {
    let mut cells = HashSet::new();
    for failure in SnapshotRecoveryFailureClass::ALL {
        let row = snapshot_recovery_failure_contract(failure);
        assert_eq!(row.failure, failure);
        assert!(!row.recoverable_point.is_empty());
        assert!(!row.rpo_scope.is_empty());
        assert!(!row.rto_scope.is_empty());
        assert!(!row.automation.is_empty());
        assert!(!row.mutation.is_empty());
        assert!(!row.required_evidence.is_empty());
        assert!(!row.terminal_outcome.is_empty());
        assert!(!row.nonclaim.is_empty());
        serde_json::to_value(row).expect("failure row must remain serializable");

        for capability in SnapshotRecoveryCapability::ALL {
            let cell = snapshot_recovery_contract_cell(failure, capability);
            assert_eq!(cell.failure, failure);
            assert_eq!(cell.capability, capability);
            assert!(!cell.nonclaim.is_empty());
            assert!(cells.insert((failure, capability)));
            serde_json::to_value(cell).expect("contract cell must remain serializable");
        }
    }
    assert_eq!(
        cells.len(),
        SnapshotRecoveryFailureClass::ALL.len() * SnapshotRecoveryCapability::ALL.len()
    );

    let mux_crash = snapshot_recovery_failure_contract(SnapshotRecoveryFailureClass::MuxCrash);
    let host_power_loss =
        snapshot_recovery_failure_contract(SnapshotRecoveryFailureClass::FullHostPowerLoss);
    assert_ne!(
        mux_crash.required_evidence,
        host_power_loss.required_evidence
    );
    assert!(mux_crash.nonclaim.contains("not host power loss"));
    assert!(host_power_loss.nonclaim.contains("do not execute"));
    assert_eq!(
        snapshot_recovery_contract_cell(
            SnapshotRecoveryFailureClass::FullHostPowerLoss,
            SnapshotRecoveryCapability::GuardianLiveProcessReattachment,
        )
        .availability,
        SnapshotRecoveryCapabilityAvailability::Forbidden
    );
    assert!(
        snapshot_recovery_failure_contract(SnapshotRecoveryFailureClass::LocalMediaLoss)
            .nonclaim
            .contains("repair symbols do not survive device loss")
    );
}

#[test]
fn snapshot_recovery_cross_product_dimensions_are_independent() {
    let mut fixture_count = 0_usize;
    for failure in SnapshotRecoveryFailureClass::ALL {
        for durability in [
            SnapshotRecoveryDurabilityGrade::Unverified,
            SnapshotRecoveryDurabilityGrade::LocalVerified,
            SnapshotRecoveryDurabilityGrade::ReplicatedVerified,
            SnapshotRecoveryDurabilityGrade::OffsiteVerified,
        ] {
            for key_availability in SnapshotRecoveryKeyAvailability::ALL {
                for replica_rank in SnapshotRecoveryReplicaRank::ALL {
                    for filesystem_capability in SnapshotRecoveryFilesystemCapability::ALL {
                        for phase in [
                            SnapshotRecoveryReadiness::Candidate,
                            SnapshotRecoveryReadiness::InteractiveSafe,
                            SnapshotRecoveryReadiness::Complete,
                        ] {
                            for operator_acknowledged in [false, true] {
                                let cell = snapshot_recovery_cross_product_cell(
                                    failure,
                                    durability,
                                    key_availability,
                                    replica_rank,
                                    filesystem_capability,
                                    phase,
                                    operator_acknowledged,
                                );
                                assert_eq!(cell.failure, failure);
                                assert_eq!(cell.durability, durability);
                                assert_eq!(cell.key_availability, key_availability);
                                assert_eq!(cell.replica_rank, replica_rank);
                                assert_eq!(cell.filesystem_capability, filesystem_capability);
                                assert_eq!(cell.phase, phase);
                                assert_eq!(cell.operator_acknowledged, operator_acknowledged);
                                assert!(!cell.terminal_nonclaim.is_empty());
                                serde_json::to_value(cell)
                                    .expect("cross-product cell must remain serializable");
                                fixture_count += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(fixture_count, 30 * 4 * 4 * 4 * 4 * 3 * 2);

    let eligible = snapshot_recovery_cross_product_cell(
        SnapshotRecoveryFailureClass::MuxCrash,
        SnapshotRecoveryDurabilityGrade::LocalVerified,
        SnapshotRecoveryKeyAvailability::PrimaryAvailable,
        SnapshotRecoveryReplicaRank::LocalPrimary,
        SnapshotRecoveryFilesystemCapability::PowerLossPublicationProven,
        SnapshotRecoveryReadiness::InteractiveSafe,
        false,
    );
    assert_eq!(
        eligible.disposition,
        SnapshotRecoveryCrossProductDisposition::EligibleForExactGuards
    );

    for (durability, replica_rank) in [
        (
            SnapshotRecoveryDurabilityGrade::LocalVerified,
            SnapshotRecoveryReplicaRank::None,
        ),
        (
            SnapshotRecoveryDurabilityGrade::ReplicatedVerified,
            SnapshotRecoveryReplicaRank::LocalPrimary,
        ),
        (
            SnapshotRecoveryDurabilityGrade::OffsiteVerified,
            SnapshotRecoveryReplicaRank::IndependentReplica,
        ),
    ] {
        let contradictory = snapshot_recovery_cross_product_cell(
            SnapshotRecoveryFailureClass::MuxCrash,
            durability,
            SnapshotRecoveryKeyAvailability::PrimaryAvailable,
            replica_rank,
            SnapshotRecoveryFilesystemCapability::PowerLossPublicationProven,
            SnapshotRecoveryReadiness::InteractiveSafe,
            false,
        );
        assert_eq!(
            contradictory.disposition,
            SnapshotRecoveryCrossProductDisposition::Forbidden
        );
        assert!(contradictory.terminal_nonclaim.contains("contradictory"));
    }

    let no_ack = snapshot_recovery_cross_product_cell(
        SnapshotRecoveryFailureClass::CompleteHostLoss,
        SnapshotRecoveryDurabilityGrade::ReplicatedVerified,
        SnapshotRecoveryKeyAvailability::IndependentWrapperAvailable,
        SnapshotRecoveryReplicaRank::IndependentReplica,
        SnapshotRecoveryFilesystemCapability::PowerLossPublicationProven,
        SnapshotRecoveryReadiness::InteractiveSafe,
        false,
    );
    assert_eq!(
        no_ack.disposition,
        SnapshotRecoveryCrossProductDisposition::Forbidden
    );
    assert!(no_ack.terminal_nonclaim.contains("acknowledgement"));

    for filesystem_capability in [
        SnapshotRecoveryFilesystemCapability::ReadOnlyRecoveryOnly,
        SnapshotRecoveryFilesystemCapability::Unsupported,
        SnapshotRecoveryFilesystemCapability::Unknown,
    ] {
        let read_only = snapshot_recovery_cross_product_cell(
            SnapshotRecoveryFailureClass::MuxCrash,
            SnapshotRecoveryDurabilityGrade::LocalVerified,
            SnapshotRecoveryKeyAvailability::PrimaryAvailable,
            SnapshotRecoveryReplicaRank::LocalPrimary,
            filesystem_capability,
            SnapshotRecoveryReadiness::InteractiveSafe,
            false,
        );
        assert_eq!(
            read_only.disposition,
            SnapshotRecoveryCrossProductDisposition::Forbidden
        );
    }
}

#[test]
fn snapshot_recovery_forensic_artifacts_cannot_be_promoted() {
    for evidence in [
        SnapshotRecoveryEvidence::verified_mux_forensic_dump(false),
        SnapshotRecoveryEvidence::verified_mux_forensic_dump(true),
        SnapshotRecoveryEvidence::verified_checkpoint_scrollback_export(false),
        SnapshotRecoveryEvidence::verified_checkpoint_scrollback_export(true),
    ] {
        let receipt = evidence
            .validate_claim(
                SnapshotRecoveryCapability::ForensicContentExport,
                SnapshotRecoveryReadiness::Candidate,
            )
            .unwrap();
        assert_eq!(
            receipt.capability(),
            SnapshotRecoveryCapability::ForensicContentExport
        );
        assert_eq!(receipt.readiness(), SnapshotRecoveryReadiness::Candidate);
        assert!(!receipt.mutation_permitted());

        for capability in [
            SnapshotRecoveryCapability::GuardianLiveProcessReattachment,
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            SnapshotRecoveryCapability::TopologyLayoutRecreation,
            SnapshotRecoveryCapability::PolicyGatedProcessReplacement,
        ] {
            assert_eq!(
                evidence.validate_claim(capability, SnapshotRecoveryReadiness::Candidate),
                Err(SnapshotRecoveryClaimError::ForensicPromotionForbidden)
            );
        }
        for readiness in [
            SnapshotRecoveryReadiness::InteractiveSafe,
            SnapshotRecoveryReadiness::Complete,
        ] {
            assert_eq!(
                evidence
                    .validate_claim(SnapshotRecoveryCapability::ForensicContentExport, readiness,),
                Err(SnapshotRecoveryClaimError::ForensicPromotionForbidden)
            );
        }
    }

    let whole_mux = complete_recovery_evidence();
    let receipt = whole_mux
        .validate_claim(
            SnapshotRecoveryCapability::ForensicContentExport,
            SnapshotRecoveryReadiness::Candidate,
        )
        .expect("a whole-mux image must retain read-only forensic export");
    assert!(!receipt.mutation_permitted());
    assert_eq!(
        whole_mux.validate_claim(
            SnapshotRecoveryCapability::ForensicContentExport,
            SnapshotRecoveryReadiness::InteractiveSafe,
        ),
        Err(SnapshotRecoveryClaimError::ForensicPromotionForbidden)
    );

    let mut repaired = SnapshotRecoveryEvidence::verified_mux_forensic_dump(true);
    repaired.repair_status = SnapshotRecoveryRepairStatus::RepairedUnverified;
    assert_eq!(
        repaired.validate_claim(
            SnapshotRecoveryCapability::ForensicContentExport,
            SnapshotRecoveryReadiness::Candidate,
        ),
        Err(SnapshotRecoveryClaimError::RepairNotReverified)
    );
}

#[test]
fn snapshot_recovery_claim_guard_rejects_each_missing_independent_fact() {
    let baseline = complete_recovery_evidence();
    let receipt = baseline
        .validate_release_claim(SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction)
        .unwrap();
    assert_eq!(receipt.readiness(), SnapshotRecoveryReadiness::Complete);
    assert!(receipt.mutation_permitted());

    let mut changed = baseline;
    changed.artifact_validity = SnapshotRecoveryVerdict::Unknown;
    assert_eq!(
        changed.validate_release_claim(
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
        ),
        Err(SnapshotRecoveryClaimError::ArtifactNotVerified)
    );
    changed = baseline;
    changed.repair_status = SnapshotRecoveryRepairStatus::RepairedUnverified;
    assert_eq!(
        changed.validate_release_claim(
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
        ),
        Err(SnapshotRecoveryClaimError::RepairNotReverified)
    );
    changed = baseline;
    changed.compatibility = SnapshotRecoveryVerdict::Unknown;
    assert_eq!(
        changed.validate_release_claim(
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
        ),
        Err(SnapshotRecoveryClaimError::CompatibilityNotVerified)
    );
    changed = baseline;
    changed.semantics = SnapshotRecoverySemantics::TerminalStateComplete;
    assert!(
        changed
            .validate_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
                SnapshotRecoveryReadiness::InteractiveSafe,
            )
            .is_ok()
    );
    assert_eq!(
        changed.validate_release_claim(
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
        ),
        Err(SnapshotRecoveryClaimError::WholeMuxSemanticsIncomplete)
    );
    changed = baseline;
    changed.durability = SnapshotRecoveryDurabilityGrade::Unverified;
    assert_eq!(
        changed.validate_release_claim(
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
        ),
        Err(SnapshotRecoveryClaimError::DurabilityNotVerified)
    );
    for freshness in [
        SnapshotRecoveryFreshness::Unknown,
        SnapshotRecoveryFreshness::Stale,
        SnapshotRecoveryFreshness::Conflict,
    ] {
        changed = baseline;
        changed.freshness = freshness;
        assert_eq!(
            changed.validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            ),
            Err(SnapshotRecoveryClaimError::FreshnessNotVerified)
        );
    }
    changed = baseline;
    changed.topology_authority = SnapshotRecoveryVerdict::Unknown;
    assert_eq!(
        changed.validate_release_claim(
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
        ),
        Err(SnapshotRecoveryClaimError::WholeMuxSemanticsIncomplete)
    );
    changed = baseline;
    changed.scrub_coverage = SnapshotRecoveryScrubCoverage::Overdue;
    assert_eq!(
        changed.validate_release_claim(
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
        ),
        Err(SnapshotRecoveryClaimError::ScrubCoverageNotCurrent)
    );
    changed = baseline;
    changed.drill_currency = SnapshotRecoveryDrillCurrency::Overdue;
    assert_eq!(
        changed.validate_release_claim(
            SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
        ),
        Err(SnapshotRecoveryClaimError::DisasterDrillNotCurrent)
    );

    changed = baseline;
    changed.guardian_census = SnapshotRecoveryVerdict::Unknown;
    assert_eq!(
        changed.validate_claim(
            SnapshotRecoveryCapability::GuardianLiveProcessReattachment,
            SnapshotRecoveryReadiness::InteractiveSafe,
        ),
        Err(SnapshotRecoveryClaimError::GuardianCensusNotVerified)
    );
    changed = baseline;
    changed.lease_replay_input_authority = SnapshotRecoveryVerdict::Unknown;
    assert_eq!(
        changed.validate_claim(
            SnapshotRecoveryCapability::GuardianLiveProcessReattachment,
            SnapshotRecoveryReadiness::InteractiveSafe,
        ),
        Err(SnapshotRecoveryClaimError::MutationAuthorityNotVerified)
    );
    changed = baseline;
    changed.process_replacement_approval = SnapshotRecoveryVerdict::Unknown;
    assert_eq!(
        changed.validate_claim(
            SnapshotRecoveryCapability::PolicyGatedProcessReplacement,
            SnapshotRecoveryReadiness::Complete,
        ),
        Err(SnapshotRecoveryClaimError::ProcessReplacementNotApproved)
    );

    changed = baseline;
    changed.client_state = SnapshotRecoveryClientStateDisposition::Conflict;
    let serialized = serde_json::to_value(changed).unwrap();
    assert_eq!(serialized["client_state"], "conflict");
    assert!(
        changed
            .validate_release_claim(
                SnapshotRecoveryCapability::ExactTerminalParserRenderReconstruction,
            )
            .is_ok()
    );
    assert_eq!(
        changed.client_state,
        SnapshotRecoveryClientStateDisposition::Conflict
    );
}

#[test]
fn snapshot_recovery_contract_proof_manifest_is_self_consistent() {
    let mut invariant_ids = HashSet::new();
    let mut filters = HashSet::new();
    for entry in SNAPSHOT_RECOVERY_CONTRACT_PROOF_MANIFEST {
        assert_eq!(entry.owner_bead, SNAPSHOT_RECOVERY_CONTRACT_OWNER);
        assert!(invariant_ids.insert(entry.invariant_id));
        assert!(filters.insert(entry.exact_filter_or_scenario));
        for field in [
            entry.fixture_or_oracle,
            entry.assertion,
            entry.package_or_script,
            entry.exact_filter_or_scenario,
            entry.test_layer,
            entry.platform,
            entry.required_artifacts,
            entry.causal_fault_or_mutation,
        ] {
            assert!(!field.is_empty());
        }
        serde_json::to_value(entry).expect("proof entry must remain serializable");
    }
    assert_eq!(SNAPSHOT_RECOVERY_CONTRACT_PROOF_MANIFEST.len(), 6);
    assert!(filters.contains("snapshot_recovery_cross_product_dimensions_are_independent"));
    assert!(filters.contains("snapshot_contract_clean_host_progressive_recovery"));
}
