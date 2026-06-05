//! Deterministic replay adapter for redacted mission-twin snapshots.
//!
//! The replay core is deliberately narrow: it validates retained
//! `MissionTwinSnapshotEnvelope` inputs, translates their redacted source facts
//! into `MissionObjectivePlannerInput`, and delegates ranking to the existing
//! side-effect-free mission objective planner.

use std::cmp::Ordering;

use crate::mission_objective_plan::{
    MissionObjectiveCandidateReadiness, MissionObjectiveCandidateWork,
    MissionObjectiveCapacityPosture, MissionObjectiveDirtyPath, MissionObjectiveEvidenceCategory,
    MissionObjectiveEvidenceItem, MissionObjectiveFreshnessState, MissionObjectivePlan,
    MissionObjectivePlanSurfaceData, MissionObjectivePlannerInput,
    MissionObjectiveProofAvailability, MissionObjectiveRedactionPosture,
    MissionObjectiveSourceKind, MissionObjectiveSourceSnapshot, MissionObjectiveSourceState,
    build_mission_objective_plan_surface_data, plan_mission_objective,
};
use crate::mission_twin_snapshot::{
    AgentMailAvailabilityState, AgentMailMissionTwinSnapshot, BeadsMissionTwinSnapshot,
    DirtyPathSummary, FreshnessState, GitMissionTwinSnapshot, MissionTwinSnapshotEnvelope,
    MissionTwinSnapshotError, OperatingEnvelopeMissionTwinSnapshot, OperatingEnvelopeVerdict,
    OwnerState, RchAdmissionState, RchMissionTwinSnapshot, ReservationsMissionTwinSnapshot,
    SourceEvidence, SourceStatus, StaleOwnerCandidate,
};

pub const MISSION_TWIN_REPLAY_SOURCE_BEAD: &str = "ft-u7r37.2";
pub const MISSION_TWIN_REPLAY_SOURCE: &str = "mission_twin.replay.ft-u7r37.2";
pub const MISSION_TWIN_REPLAY_OBJECTIVE: &str =
    "Replay redacted mission-twin snapshots into a side-effect-free current-state plan.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionTwinReplayError {
    EmptySnapshotSet,
    InvalidSnapshot {
        snapshot_id: String,
        error: MissionTwinSnapshotError,
    },
}

impl std::fmt::Display for MissionTwinReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySnapshotSet => {
                write!(f, "mission twin replay requires at least one snapshot")
            }
            Self::InvalidSnapshot { snapshot_id, error } => {
                write!(f, "invalid mission twin snapshot {snapshot_id}: {error}")
            }
        }
    }
}

impl std::error::Error for MissionTwinReplayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EmptySnapshotSet => None,
            Self::InvalidSnapshot { error, .. } => Some(error),
        }
    }
}

#[must_use]
pub fn mission_twin_replay_source_id(snapshot_id: &str, source_name: &str) -> String {
    format!("mission_twin.{snapshot_id}.{source_name}")
}

pub fn build_mission_twin_replay_planner_input(
    snapshots: &[MissionTwinSnapshotEnvelope],
) -> Result<MissionObjectivePlannerInput, MissionTwinReplayError> {
    let ordered = ordered_validated_snapshots(snapshots)?;
    let generated_at_ms = ordered
        .iter()
        .map(|snapshot| snapshot.generated_at_ms)
        .max()
        .ok_or(MissionTwinReplayError::EmptySnapshotSet)?;

    let mut input = MissionObjectivePlannerInput::new(
        generated_at_ms,
        MISSION_TWIN_REPLAY_SOURCE,
        MISSION_TWIN_REPLAY_OBJECTIVE,
    );

    for snapshot in ordered {
        append_source_snapshots(&mut input, snapshot);
        append_dirty_paths(&mut input, snapshot);
        append_candidates(&mut input, snapshot);
    }

    input
        .source_snapshots
        .sort_by(|left, right| left.source_id.cmp(&right.source_id));
    input.dirty_paths.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.status.cmp(&right.status))
    });
    input
        .candidates
        .sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));

    Ok(input)
}

pub fn replay_mission_twin_snapshots(
    snapshots: &[MissionTwinSnapshotEnvelope],
) -> Result<MissionObjectivePlan, MissionTwinReplayError> {
    let input = build_mission_twin_replay_planner_input(snapshots)?;
    Ok(plan_mission_objective(&input))
}

pub fn build_mission_twin_replay_surface_data(
    snapshots: &[MissionTwinSnapshotEnvelope],
    explain_step: Option<&str>,
    explain_reason: Option<&str>,
) -> Result<MissionObjectivePlanSurfaceData, MissionTwinReplayError> {
    let plan = replay_mission_twin_snapshots(snapshots)?;
    Ok(build_mission_objective_plan_surface_data(
        plan,
        explain_step,
        explain_reason,
    ))
}

fn ordered_validated_snapshots(
    snapshots: &[MissionTwinSnapshotEnvelope],
) -> Result<Vec<&MissionTwinSnapshotEnvelope>, MissionTwinReplayError> {
    if snapshots.is_empty() {
        return Err(MissionTwinReplayError::EmptySnapshotSet);
    }

    let mut ordered = snapshots.iter().collect::<Vec<_>>();
    ordered.sort_by(compare_snapshots);

    for snapshot in &ordered {
        snapshot
            .validate()
            .map_err(|error| MissionTwinReplayError::InvalidSnapshot {
                snapshot_id: display_snapshot_id(snapshot),
                error,
            })?;
    }

    Ok(ordered)
}

fn compare_snapshots(
    left: &&MissionTwinSnapshotEnvelope,
    right: &&MissionTwinSnapshotEnvelope,
) -> Ordering {
    left.snapshot_id
        .cmp(&right.snapshot_id)
        .then(left.generated_at_ms.cmp(&right.generated_at_ms))
}

fn display_snapshot_id(snapshot: &MissionTwinSnapshotEnvelope) -> String {
    if snapshot.snapshot_id.trim().is_empty() {
        "<missing>".to_string()
    } else {
        snapshot.snapshot_id.clone()
    }
}

fn append_source_snapshots(
    input: &mut MissionObjectivePlannerInput,
    snapshot: &MissionTwinSnapshotEnvelope,
) {
    input.source_snapshots.push(beads_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.beads,
    ));
    input.source_snapshots.push(rch_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.rch,
    ));
    input.source_snapshots.push(agent_mail_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.agent_mail,
    ));
    input.source_snapshots.push(git_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.git,
    ));
    input.source_snapshots.push(reservations_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.reservations,
    ));
    input
        .source_snapshots
        .push(operating_envelope_source_snapshot(
            &snapshot.snapshot_id,
            &snapshot.sources.operating_envelope,
        ));
}

fn append_dirty_paths(
    input: &mut MissionObjectivePlannerInput,
    snapshot: &MissionTwinSnapshotEnvelope,
) {
    input.dirty_paths.extend(
        snapshot
            .sources
            .git
            .dirty_paths
            .iter()
            .map(dirty_path_for_planner),
    );
}

fn append_candidates(
    input: &mut MissionObjectivePlannerInput,
    snapshot: &MissionTwinSnapshotEnvelope,
) {
    input.candidates.extend(replay_candidates(snapshot));
}

fn replay_candidates(snapshot: &MissionTwinSnapshotEnvelope) -> Vec<MissionObjectiveCandidateWork> {
    if !snapshot.sources.beads.owner_states.is_empty() {
        return snapshot
            .sources
            .beads
            .owner_states
            .iter()
            .map(|owner| {
                let mut candidate = base_candidate(
                    snapshot,
                    &format!("owner.{}", owner.bead_id),
                    MissionObjectiveCandidateReadiness::ActiveSameDomain,
                    format!(
                        "Wait for active owner {} on {}",
                        owner.assignee, owner.bead_id
                    ),
                )
                .target_bead_id(&owner.bead_id)
                .active_owner(&owner.assignee, owner.age_seconds)
                .with_reason_code("mission_twin.owner_handoff_required");

                for reason_code in &owner.reason_codes {
                    candidate = candidate.with_reason_code(reason_code);
                }

                candidate
            })
            .collect();
    }

    if !snapshot.sources.beads.stale_owner_candidates.is_empty() {
        return snapshot
            .sources
            .beads
            .stale_owner_candidates
            .iter()
            .map(|owner| stale_owner_candidate(snapshot, owner))
            .collect();
    }

    if snapshot.sources.beads.ready_count == 0 {
        return Vec::new();
    }

    let mut candidate = base_candidate(
        snapshot,
        "ready-work",
        MissionObjectiveCandidateReadiness::ReadyBead,
        format!(
            "Replay {} Beads-ready candidate(s) from mission-twin snapshot {}",
            snapshot.sources.beads.ready_count, snapshot.snapshot_id
        ),
    )
    .with_reason_code("mission_twin.replay.ready_work_available");

    for path in ready_candidate_owned_paths(snapshot) {
        candidate = candidate.with_owned_path(path);
    }

    vec![candidate]
}

fn stale_owner_candidate(
    snapshot: &MissionTwinSnapshotEnvelope,
    owner: &StaleOwnerCandidate,
) -> MissionObjectiveCandidateWork {
    let mut candidate = base_candidate(
        snapshot,
        &format!("stale-owner.{}", owner.bead_id),
        MissionObjectiveCandidateReadiness::StaleReopenCandidate,
        format!(
            "Status-check stale owner {} on {}",
            owner.assignee, owner.bead_id
        ),
    )
    .target_bead_id(&owner.bead_id)
    .active_owner(&owner.assignee, owner.age_seconds)
    .stale_after_seconds(60)
    .with_reason_code("mission_twin.stale_owner_candidate");

    for reason_code in &owner.reason_codes {
        candidate = candidate.with_reason_code(reason_code);
    }

    candidate
}

fn base_candidate(
    snapshot: &MissionTwinSnapshotEnvelope,
    suffix: &str,
    readiness: MissionObjectiveCandidateReadiness,
    title: String,
) -> MissionObjectiveCandidateWork {
    let mut candidate = MissionObjectiveCandidateWork::new(
        replay_candidate_id(&snapshot.snapshot_id, suffix),
        readiness,
    )
    .title(title)
    .priority(candidate_priority(snapshot))
    .proof_availability(snapshot_proof_availability(snapshot))
    .capacity_posture(snapshot_capacity_posture(snapshot));

    for reason_code in all_snapshot_reason_codes(snapshot) {
        candidate = candidate.with_reason_code(reason_code);
    }

    candidate
}

fn replay_candidate_id(snapshot_id: &str, suffix: &str) -> String {
    format!("mission-twin.{snapshot_id}.{suffix}")
}

fn candidate_priority(snapshot: &MissionTwinSnapshotEnvelope) -> u8 {
    if snapshot.sources.rch.critical_pressure_count > 0
        || snapshot.sources.rch.admission_state != RchAdmissionState::Ready
    {
        1
    } else {
        2
    }
}

fn snapshot_proof_availability(
    snapshot: &MissionTwinSnapshotEnvelope,
) -> MissionObjectiveProofAvailability {
    if snapshot.sources.rch.admission_state != RchAdmissionState::Ready
        || snapshot.sources.rch.critical_pressure_count > 0
        || !snapshot.sources.rch.blocked_proof_lanes.is_empty()
    {
        MissionObjectiveProofAvailability::Blocked
    } else {
        MissionObjectiveProofAvailability::NotRequired
    }
}

fn snapshot_capacity_posture(
    snapshot: &MissionTwinSnapshotEnvelope,
) -> MissionObjectiveCapacityPosture {
    match snapshot.sources.operating_envelope.verdict {
        OperatingEnvelopeVerdict::Admit => MissionObjectiveCapacityPosture::Admit,
        OperatingEnvelopeVerdict::Deny => MissionObjectiveCapacityPosture::Defer,
        OperatingEnvelopeVerdict::Shed => MissionObjectiveCapacityPosture::Pause,
        OperatingEnvelopeVerdict::Unknown => MissionObjectiveCapacityPosture::Unknown,
    }
}

fn ready_candidate_owned_paths(snapshot: &MissionTwinSnapshotEnvelope) -> Vec<String> {
    let mut paths = Vec::new();
    extend_reasonless_paths(&mut paths, &snapshot.sources.git.overlap_paths);

    for dirty_path in &snapshot.sources.git.dirty_paths {
        if dirty_path.overlaps_owned_path {
            push_unique(&mut paths, dirty_path.path.clone());
        }
    }

    paths.sort();
    paths
}

fn dirty_path_for_planner(path: &DirtyPathSummary) -> MissionObjectiveDirtyPath {
    let category = if path.overlaps_owned_path {
        "owned_path_overlap"
    } else {
        "dirty_tree"
    };

    MissionObjectiveDirtyPath::new(&path.path, &path.status).category(category)
}

fn beads_source_snapshot(
    snapshot_id: &str,
    source: &BeadsMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "beads"),
        MissionObjectiveSourceKind::Beads,
        &source.evidence,
        MissionObjectiveEvidenceCategory::BeadsReadyQueue,
        format!(
            "ready={} blocked={} in_progress={}",
            source.ready_count, source.blocked_count, source.in_progress_count
        ),
        beads_reason_codes(source),
    )
}

fn rch_source_snapshot(
    snapshot_id: &str,
    source: &RchMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    let mut snapshot = source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "rch"),
        MissionObjectiveSourceKind::Rch,
        &source.evidence,
        MissionObjectiveEvidenceCategory::RchWorkerSelection,
        format!(
            "admission={} healthy_workers={}/{} critical_pressure={}",
            rch_admission_state_name(source.admission_state),
            source.healthy_workers,
            source.total_workers,
            source.critical_pressure_count
        ),
        rch_reason_codes(source),
    );

    if source.admission_state == RchAdmissionState::Unavailable {
        snapshot.state = MissionObjectiveSourceState::Unavailable;
    } else if source.admission_state != RchAdmissionState::Ready
        || source.critical_pressure_count > 0
        || !source.blocked_proof_lanes.is_empty()
    {
        snapshot.state = MissionObjectiveSourceState::Blocked;
    }

    snapshot
}

fn agent_mail_source_snapshot(
    snapshot_id: &str,
    source: &AgentMailMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    let mut snapshot = source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "agent_mail"),
        MissionObjectiveSourceKind::AgentMail,
        &source.evidence,
        MissionObjectiveEvidenceCategory::AgentMailAvailability,
        format!(
            "availability={} active_agents={}",
            agent_mail_availability_name(source.availability_state),
            source.active_agents.len()
        ),
        agent_mail_reason_codes(source),
    );

    snapshot.state = match source.availability_state {
        AgentMailAvailabilityState::Healthy => snapshot.state,
        AgentMailAvailabilityState::Red => MissionObjectiveSourceState::Unavailable,
        AgentMailAvailabilityState::Fallback | AgentMailAvailabilityState::Unknown => {
            MissionObjectiveSourceState::Degraded
        }
    };

    snapshot
}

fn git_source_snapshot(
    snapshot_id: &str,
    source: &GitMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "git"),
        MissionObjectiveSourceKind::Git,
        &source.evidence,
        MissionObjectiveEvidenceCategory::GitDirtyTree,
        format!(
            "branch={} dirty_paths={} overlap_paths={} deletion_paths_present={}",
            source.branch,
            source.dirty_paths.len(),
            source.overlap_paths.len(),
            source.deletion_paths_present
        ),
        git_reason_codes(source),
    )
}

fn reservations_source_snapshot(
    snapshot_id: &str,
    source: &ReservationsMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "reservations"),
        MissionObjectiveSourceKind::BlockerRadar,
        &source.evidence,
        MissionObjectiveEvidenceCategory::ActiveAssigneeOverlap,
        format!("active_reservations={}", source.active_reservations.len()),
        reservations_reason_codes(source),
    )
}

fn operating_envelope_source_snapshot(
    snapshot_id: &str,
    source: &OperatingEnvelopeMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    let mut snapshot = source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "operating_envelope"),
        MissionObjectiveSourceKind::ResourceCockpit,
        &source.evidence,
        MissionObjectiveEvidenceCategory::CapacityPressure,
        format!(
            "operating_envelope_verdict={}",
            operating_envelope_verdict_name(source.verdict)
        ),
        operating_envelope_reason_codes(source),
    );

    snapshot.state = match source.verdict {
        OperatingEnvelopeVerdict::Admit => snapshot.state,
        OperatingEnvelopeVerdict::Deny | OperatingEnvelopeVerdict::Shed => {
            MissionObjectiveSourceState::Degraded
        }
        OperatingEnvelopeVerdict::Unknown => MissionObjectiveSourceState::Degraded,
    };

    snapshot
}

fn source_snapshot_from_evidence(
    source_id: String,
    kind: MissionObjectiveSourceKind,
    evidence: &SourceEvidence,
    category: MissionObjectiveEvidenceCategory,
    summary: String,
    reason_codes: Vec<String>,
) -> MissionObjectiveSourceSnapshot {
    let reason_codes = sorted_reason_codes(reason_codes);
    let mut evidence_item = MissionObjectiveEvidenceItem::new(category, summary);
    evidence_item.reason_codes = reason_codes.clone();

    let mut snapshot = MissionObjectiveSourceSnapshot::new(source_id, kind);
    snapshot.state = source_state(evidence.status);
    snapshot.freshness_state = freshness_state(evidence.freshness_state);
    snapshot.redaction_posture = redaction_posture(evidence);
    snapshot.reason_codes = reason_codes;
    snapshot.evidence.push(evidence_item);
    snapshot
}

fn source_state(status: SourceStatus) -> MissionObjectiveSourceState {
    match status {
        SourceStatus::Available => MissionObjectiveSourceState::Available,
        SourceStatus::Degraded => MissionObjectiveSourceState::Degraded,
        SourceStatus::Unavailable => MissionObjectiveSourceState::Unavailable,
        SourceStatus::Blocked => MissionObjectiveSourceState::Blocked,
    }
}

fn freshness_state(state: FreshnessState) -> MissionObjectiveFreshnessState {
    match state {
        FreshnessState::Fresh => MissionObjectiveFreshnessState::Fresh,
        FreshnessState::Stale => MissionObjectiveFreshnessState::Stale,
        FreshnessState::Unknown => MissionObjectiveFreshnessState::Unknown,
        FreshnessState::NotCollected => MissionObjectiveFreshnessState::NotCollected,
    }
}

fn redaction_posture(evidence: &SourceEvidence) -> MissionObjectiveRedactionPosture {
    if evidence.raw_pane_content_stored {
        MissionObjectiveRedactionPosture::RawPaneContent
    } else if evidence.redacted {
        MissionObjectiveRedactionPosture::RedactedSummaryOnly
    } else {
        MissionObjectiveRedactionPosture::Unredacted
    }
}

fn all_snapshot_reason_codes(snapshot: &MissionTwinSnapshotEnvelope) -> Vec<String> {
    let mut reason_codes = Vec::new();
    extend_reason_codes(
        &mut reason_codes,
        beads_reason_codes(&snapshot.sources.beads),
    );
    extend_reason_codes(&mut reason_codes, rch_reason_codes(&snapshot.sources.rch));
    extend_reason_codes(
        &mut reason_codes,
        agent_mail_reason_codes(&snapshot.sources.agent_mail),
    );
    extend_reason_codes(&mut reason_codes, git_reason_codes(&snapshot.sources.git));
    extend_reason_codes(
        &mut reason_codes,
        reservations_reason_codes(&snapshot.sources.reservations),
    );
    extend_reason_codes(
        &mut reason_codes,
        operating_envelope_reason_codes(&snapshot.sources.operating_envelope),
    );

    for rejected in &snapshot.validation.rejected_inputs {
        push_unique(&mut reason_codes, rejected.reason_code.clone());
    }

    sorted_reason_codes(reason_codes)
}

fn beads_reason_codes(source: &BeadsMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();

    if source.ready_count == 0 {
        push_unique(&mut reason_codes, "mission_twin.no_ready_work");
    }

    for blocker in &source.dependency_blockers {
        push_unique(&mut reason_codes, "beads.dependency_blocked");
        extend_reason_codes(&mut reason_codes, blocker.reason_codes.clone());
    }

    for owner in &source.owner_states {
        match owner.owner_state {
            OwnerState::Active => push_unique(&mut reason_codes, "beads.owner_active"),
            OwnerState::StaleCandidate => {
                push_unique(&mut reason_codes, "beads.stale_reopen_candidate");
            }
            OwnerState::Unknown => push_unique(&mut reason_codes, "beads.owner_unknown"),
        }
        extend_reason_codes(&mut reason_codes, owner.reason_codes.clone());
    }

    for owner in &source.stale_owner_candidates {
        push_unique(&mut reason_codes, "beads.stale_reopen_candidate");
        extend_reason_codes(&mut reason_codes, owner.reason_codes.clone());
    }

    sorted_reason_codes(reason_codes)
}

fn rch_reason_codes(source: &RchMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();
    extend_reason_codes(&mut reason_codes, source.admission_reasons.clone());

    match source.admission_state {
        RchAdmissionState::Ready => {}
        RchAdmissionState::NotReady => push_unique(&mut reason_codes, "rch.admission.not_ready"),
        RchAdmissionState::Unavailable => push_unique(&mut reason_codes, "rch.unavailable"),
    }

    if source.critical_pressure_count > 0 {
        push_unique(&mut reason_codes, "rch.critical_pressure");
    }
    if source.healthy_workers == 0 {
        push_unique(&mut reason_codes, "rch.no_workers_passed_health");
    }

    for lane in &source.blocked_proof_lanes {
        push_unique(&mut reason_codes, "rch.proof_substrate_blocked");
        extend_reason_codes(&mut reason_codes, lane.reason_codes.clone());
    }

    sorted_reason_codes(reason_codes)
}

fn agent_mail_reason_codes(source: &AgentMailMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();
    extend_reason_codes(&mut reason_codes, source.fallback_reason_codes.clone());

    match source.availability_state {
        AgentMailAvailabilityState::Healthy => {}
        AgentMailAvailabilityState::Red => push_unique(&mut reason_codes, "agent_mail.red"),
        AgentMailAvailabilityState::Fallback => {
            push_unique(&mut reason_codes, "agent_mail.fallback_required");
        }
        AgentMailAvailabilityState::Unknown => {
            push_unique(&mut reason_codes, "agent_mail.unknown");
        }
    }

    sorted_reason_codes(reason_codes)
}

fn git_reason_codes(source: &GitMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();

    if !source.dirty_paths.is_empty() {
        push_unique(&mut reason_codes, "git.dirty_paths_present");
    }
    if source
        .dirty_paths
        .iter()
        .any(|dirty_path| dirty_path.overlaps_owned_path)
        || !source.overlap_paths.is_empty()
    {
        push_unique(&mut reason_codes, "mission_twin.dirty_overlap");
        push_unique(&mut reason_codes, "dirty_overlap.owned_surface_blocked");
    }
    if source.deletion_paths_present {
        push_unique(&mut reason_codes, "git.deletion_paths_present");
    }

    sorted_reason_codes(reason_codes)
}

fn reservations_reason_codes(source: &ReservationsMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();

    if source
        .active_reservations
        .iter()
        .any(|reservation| reservation.exclusive)
    {
        push_unique(&mut reason_codes, "reservations.exclusive_active");
    }

    sorted_reason_codes(reason_codes)
}

fn operating_envelope_reason_codes(source: &OperatingEnvelopeMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.reason_codes.clone();
    extend_reason_codes(&mut reason_codes, source.evidence.reason_codes.clone());

    match source.verdict {
        OperatingEnvelopeVerdict::Admit => {}
        OperatingEnvelopeVerdict::Deny => {
            push_unique(&mut reason_codes, "operating_envelope.deny");
        }
        OperatingEnvelopeVerdict::Shed => {
            push_unique(&mut reason_codes, "operating_envelope.shed");
        }
        OperatingEnvelopeVerdict::Unknown => {
            push_unique(&mut reason_codes, "operating_envelope.unknown");
        }
    }

    sorted_reason_codes(reason_codes)
}

fn sorted_reason_codes(mut reason_codes: Vec<String>) -> Vec<String> {
    reason_codes.retain(|reason_code| !reason_code.trim().is_empty());
    reason_codes.sort();
    reason_codes.dedup();
    reason_codes
}

fn extend_reason_codes(target: &mut Vec<String>, reason_codes: Vec<String>) {
    for reason_code in reason_codes {
        push_unique(target, reason_code);
    }
}

fn extend_reasonless_paths(target: &mut Vec<String>, paths: &[String]) {
    for path in paths {
        push_unique(target, path.clone());
    }
}

fn push_unique(target: &mut Vec<String>, value: impl Into<String>) {
    let value = value.into();
    if !value.trim().is_empty() && !target.contains(&value) {
        target.push(value);
    }
}

fn rch_admission_state_name(state: RchAdmissionState) -> &'static str {
    match state {
        RchAdmissionState::Ready => "ready",
        RchAdmissionState::NotReady => "not_ready",
        RchAdmissionState::Unavailable => "unavailable",
    }
}

fn agent_mail_availability_name(state: AgentMailAvailabilityState) -> &'static str {
    match state {
        AgentMailAvailabilityState::Healthy => "healthy",
        AgentMailAvailabilityState::Red => "red",
        AgentMailAvailabilityState::Fallback => "fallback",
        AgentMailAvailabilityState::Unknown => "unknown",
    }
}

fn operating_envelope_verdict_name(verdict: OperatingEnvelopeVerdict) -> &'static str {
    match verdict {
        OperatingEnvelopeVerdict::Admit => "admit",
        OperatingEnvelopeVerdict::Deny => "deny",
        OperatingEnvelopeVerdict::Shed => "shed",
        OperatingEnvelopeVerdict::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission_objective_plan::MissionObjectivePlanStatus;
    use crate::mission_twin_snapshot::{
        ActiveAgentSummary, AgentMailMissionTwinSnapshot, BeadsMissionTwinSnapshot,
        BlockedProofLane, DependencyBlocker, DirtyPathSummary, EvidenceLevel,
        GitMissionTwinSnapshot, MissionTwinForbiddenAction, MissionTwinSources,
        MissionTwinValidationSummary, OperatingEnvelopeMissionTwinSnapshot, RejectedInputSummary,
        RemoteHead, ReservationSummary, ReservationsMissionTwinSnapshot, SourceEvidence,
        ValidationState,
    };

    #[test]
    fn replay_rejects_empty_snapshot_sets() {
        assert_eq!(
            build_mission_twin_replay_planner_input(&[]),
            Err(MissionTwinReplayError::EmptySnapshotSet)
        );
    }

    #[test]
    fn replay_input_order_is_deterministic_for_equivalent_snapshots() {
        let left = valid_snapshot("b-snapshot");
        let right = valid_snapshot("a-snapshot");

        let first =
            build_mission_twin_replay_surface_data(&[left.clone(), right.clone()], None, None)
                .expect("first replay succeeds");
        let second = build_mission_twin_replay_surface_data(&[right, left], None, None)
            .expect("second replay succeeds");

        let first_json = serde_json::to_string(&first).expect("first replay serializes");
        let second_json = serde_json::to_string(&second).expect("second replay serializes");

        assert_eq!(
            first_json, second_json,
            "snapshot ordering must not perturb replay output"
        );
    }

    #[test]
    fn replay_preserves_source_reason_codes_in_plan() {
        let mut snapshot = valid_snapshot("reason-preservation");
        snapshot.sources.rch.admission_state = RchAdmissionState::NotReady;
        snapshot.sources.rch.critical_pressure_count = 5;
        snapshot
            .sources
            .rch
            .blocked_proof_lanes
            .push(BlockedProofLane {
                bead_id: "ft-proof".to_string(),
                command_family: "cargo".to_string(),
                reason_codes: vec![
                    "rch.admission.not_ready".to_string(),
                    "rch.critical_pressure".to_string(),
                ],
            });
        snapshot.sources.agent_mail.availability_state = AgentMailAvailabilityState::Red;
        snapshot
            .sources
            .agent_mail
            .fallback_reason_codes
            .push("swarm_tick_fallback".to_string());
        snapshot
            .sources
            .beads
            .dependency_blockers
            .push(DependencyBlocker {
                blocked_bead_id: "ft-dependent".to_string(),
                blocking_bead_id: "ft-blocker".to_string(),
                blocker_status: "in_progress".to_string(),
                reason_codes: vec!["mission_twin.snapshot_contract_required".to_string()],
            });
        snapshot.sources.git.dirty_paths.push(DirtyPathSummary {
            path: "crates/frankenterm-core/src/mission_twin_snapshot.rs".to_string(),
            status: "M".to_string(),
            overlaps_owned_path: true,
        });
        snapshot
            .sources
            .git
            .overlap_paths
            .push("crates/frankenterm-core/src/mission_twin_snapshot.rs".to_string());
        snapshot
            .sources
            .operating_envelope
            .reason_codes
            .push("mission_twin.dirty_overlap".to_string());

        let plan = replay_mission_twin_snapshots(&[snapshot]).expect("replay succeeds");

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::DirtyOverlap);
        for reason_code in [
            "agent_mail.red",
            "swarm_tick_fallback",
            "rch.critical_pressure",
            "rch.proof_substrate_blocked",
            "mission_twin.snapshot_contract_required",
            "mission_twin.dirty_overlap",
            "dirty_overlap.owned_surface_blocked",
        ] {
            assert!(
                plan.reason_codes.iter().any(|code| code == reason_code),
                "missing replay reason code {reason_code}"
            );
        }
    }

    fn valid_snapshot(snapshot_id: &str) -> MissionTwinSnapshotEnvelope {
        MissionTwinSnapshotEnvelope {
            schema_version: crate::mission_twin_snapshot::MISSION_TWIN_SNAPSHOT_SCHEMA_VERSION,
            contract_id: crate::mission_twin_snapshot::MISSION_TWIN_SNAPSHOT_CONTRACT_ID
                .to_string(),
            source_bead: crate::mission_twin_snapshot::MISSION_TWIN_SNAPSHOT_SOURCE_BEAD
                .to_string(),
            snapshot_id: snapshot_id.to_string(),
            generated_at_ms: 1_771_376_400_000,
            raw_pane_content_stored: false,
            forbidden_actions: MissionTwinForbiddenAction::required_set().to_vec(),
            artifact_paths: vec!["fixtures/mission-twin/snapshot/valid/healthy.json".to_string()],
            sources: MissionTwinSources {
                beads: BeadsMissionTwinSnapshot {
                    evidence: evidence("beads"),
                    ready_count: 1,
                    blocked_count: 0,
                    in_progress_count: 0,
                    dependency_blockers: Vec::new(),
                    owner_states: Vec::new(),
                    stale_owner_candidates: Vec::new(),
                },
                rch: RchMissionTwinSnapshot {
                    evidence: evidence("rch"),
                    admission_state: RchAdmissionState::Ready,
                    healthy_workers: 8,
                    total_workers: 8,
                    critical_pressure_count: 0,
                    admission_reasons: Vec::new(),
                    blocked_proof_lanes: Vec::new(),
                },
                agent_mail: AgentMailMissionTwinSnapshot {
                    evidence: evidence("agent_mail"),
                    availability_state: AgentMailAvailabilityState::Healthy,
                    active_agents: Vec::<ActiveAgentSummary>::new(),
                    fallback_reason_codes: Vec::new(),
                },
                git: GitMissionTwinSnapshot {
                    evidence: evidence("git"),
                    branch: "main".to_string(),
                    head: "0123456789abcdef0123456789abcdef01234567".to_string(),
                    remote_heads: Vec::<RemoteHead>::new(),
                    dirty_paths: Vec::new(),
                    overlap_paths: Vec::new(),
                    deletion_paths_present: false,
                },
                reservations: ReservationsMissionTwinSnapshot {
                    evidence: evidence("reservations"),
                    active_reservations: Vec::<ReservationSummary>::new(),
                },
                operating_envelope: OperatingEnvelopeMissionTwinSnapshot {
                    evidence: evidence("operating_envelope"),
                    verdict: OperatingEnvelopeVerdict::Admit,
                    reason_codes: Vec::new(),
                    source_snapshot_artifact_paths: Vec::new(),
                },
            },
            validation: MissionTwinValidationSummary {
                validation_state: ValidationState::Accepted,
                rejected_inputs: Vec::<RejectedInputSummary>::new(),
                destructive_action_hints: Vec::new(),
                ambiguous_timestamps_rejected: true,
            },
        }
    }

    fn evidence(source_id: &str) -> SourceEvidence {
        SourceEvidence {
            source_id: source_id.to_string(),
            status: SourceStatus::Available,
            freshness_state: FreshnessState::Fresh,
            collected_at_ms: Some(1_771_376_400_000),
            freshness_ms: Some(1),
            evidence_level: EvidenceLevel::Fixture,
            redacted: true,
            raw_pane_content_stored: false,
            reason_codes: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }
}
