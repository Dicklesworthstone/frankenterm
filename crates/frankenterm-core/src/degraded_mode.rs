//! Typed degraded-mode contracts for dependency outages.
//!
//! The contract is intentionally side-effect-free. Callers that already know a
//! dependency is unavailable can return one of these receipts instead of a bare
//! error, making the safe fallback set explicit.

use serde::{Deserialize, Serialize};

use crate::operating_envelope::{
    OperatingEnvelopeSourceKind, OperatingEnvelopeSourceSnapshot, OperatingEnvelopeSourceState,
};

pub const DEGRADED_MODE_CONTRACT_ID: &str = "ft.degraded_mode.v1";
pub const DEGRADED_MODE_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradedModeSurface {
    AgentMail,
    Rch,
    Mcp,
    Storage,
    SemanticPane,
    Beads,
}

impl DegradedModeSurface {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentMail => "agent_mail",
            Self::Rch => "rch",
            Self::Mcp => "mcp",
            Self::Storage => "storage",
            Self::SemanticPane => "semantic_pane",
            Self::Beads => "beads",
        }
    }

    #[must_use]
    pub const fn from_operating_envelope_source(kind: OperatingEnvelopeSourceKind) -> Option<Self> {
        match kind {
            OperatingEnvelopeSourceKind::AgentMail => Some(Self::AgentMail),
            OperatingEnvelopeSourceKind::Rch => Some(Self::Rch),
            OperatingEnvelopeSourceKind::Beads => Some(Self::Beads),
            OperatingEnvelopeSourceKind::RobotInventory => Some(Self::SemanticPane),
            OperatingEnvelopeSourceKind::CapacityResource
            | OperatingEnvelopeSourceKind::Git
            | OperatingEnvelopeSourceKind::BlockerRadar
            | OperatingEnvelopeSourceKind::Manual
            | OperatingEnvelopeSourceKind::Fixture => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradedModeState {
    Degraded,
    Unavailable,
    Blocked,
    Stale,
    NotCollected,
}

impl DegradedModeState {
    #[must_use]
    pub const fn from_source_state(state: OperatingEnvelopeSourceState) -> Option<Self> {
        match state {
            OperatingEnvelopeSourceState::Available => None,
            OperatingEnvelopeSourceState::Degraded => Some(Self::Degraded),
            OperatingEnvelopeSourceState::Unavailable => Some(Self::Unavailable),
            OperatingEnvelopeSourceState::Blocked => Some(Self::Blocked),
            OperatingEnvelopeSourceState::Stale => Some(Self::Stale),
            OperatingEnvelopeSourceState::NotCollected => Some(Self::NotCollected),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradedModeAction {
    AddBeadsComment,
    ClaimReadyBead,
    ContinueNonOverlappingCodeWork,
    CountLocalCargoAsProof,
    CreateBlockerBead,
    EditOverlappingDirtyPaths,
    EmitUnsupportedResourceReceipt,
    LocalHygieneCheck,
    NonAuthoritativeCoordinationIntent,
    RawRedactedGetText,
    RebuildDerivedStores,
    RecordDeferredProof,
    RepairAgentMail,
    RestartRchService,
    RobotCliEquivalent,
    RunRemoteProof,
    SearchClaims,
    SemanticZoneClaim,
    ServiceMutation,
    ServiceRestart,
    StorageMutation,
    StrictVerifiedSubmit,
    WorkerDrain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DegradedModeRestoreCondition {
    pub condition_id: String,
    pub summary: String,
}

impl DegradedModeRestoreCondition {
    #[must_use]
    pub fn new(condition_id: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            condition_id: condition_id.into(),
            summary: summary.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DegradedModeContract {
    pub contract_id: String,
    pub schema_version: u16,
    pub surface: DegradedModeSurface,
    pub state: DegradedModeState,
    pub reason_code: String,
    pub admitted_actions: Vec<DegradedModeAction>,
    pub forbidden_actions: Vec<DegradedModeAction>,
    pub restore_conditions: Vec<DegradedModeRestoreCondition>,
    pub notes: Vec<String>,
}

impl DegradedModeContract {
    #[must_use]
    pub fn new(
        surface: DegradedModeSurface,
        state: DegradedModeState,
        reason_code: impl Into<String>,
    ) -> Self {
        let reason_code = reason_code.into();
        let mut contract = Self {
            contract_id: DEGRADED_MODE_CONTRACT_ID.to_string(),
            schema_version: DEGRADED_MODE_SCHEMA_VERSION,
            surface,
            state,
            reason_code,
            admitted_actions: Vec::new(),
            forbidden_actions: Vec::new(),
            restore_conditions: Vec::new(),
            notes: Vec::new(),
        };
        apply_surface_defaults(&mut contract);
        contract
    }

    #[must_use]
    pub fn admits(&self, action: DegradedModeAction) -> bool {
        self.admitted_actions.contains(&action) && !self.forbidden_actions.contains(&action)
    }

    #[must_use]
    pub fn forbids(&self, action: DegradedModeAction) -> bool {
        self.forbidden_actions.contains(&action)
    }
}

#[must_use]
pub fn contract_for_surface(
    surface: DegradedModeSurface,
    state: DegradedModeState,
    reason_code: impl Into<String>,
) -> DegradedModeContract {
    DegradedModeContract::new(surface, state, reason_code)
}

#[must_use]
pub fn contract_for_source_snapshot(
    snapshot: &OperatingEnvelopeSourceSnapshot,
) -> Option<DegradedModeContract> {
    let state = DegradedModeState::from_source_state(snapshot.state)?;
    let surface = DegradedModeSurface::from_operating_envelope_source(snapshot.source_kind)?;
    let reason = snapshot
        .unavailable_reason
        .as_deref()
        .or(snapshot.degraded_reason.as_deref())
        .or_else(|| snapshot.reason_codes.first().map(String::as_str))
        .unwrap_or("dependency.degraded");
    Some(contract_for_surface(surface, state, reason))
}

fn apply_surface_defaults(contract: &mut DegradedModeContract) {
    match contract.surface {
        DegradedModeSurface::AgentMail => {
            admit(
                contract,
                &[
                    DegradedModeAction::ClaimReadyBead,
                    DegradedModeAction::AddBeadsComment,
                    DegradedModeAction::CreateBlockerBead,
                    DegradedModeAction::NonAuthoritativeCoordinationIntent,
                ],
            );
            forbid(
                contract,
                &[
                    DegradedModeAction::RepairAgentMail,
                    DegradedModeAction::ServiceRestart,
                    DegradedModeAction::EditOverlappingDirtyPaths,
                ],
            );
            restore(
                contract,
                "agent_mail.ack_round_trip",
                "agent-mail accepts a read/ack or send/receive round trip without repair commands",
            );
        }
        DegradedModeSurface::Rch => {
            admit(
                contract,
                &[
                    DegradedModeAction::LocalHygieneCheck,
                    DegradedModeAction::RecordDeferredProof,
                    DegradedModeAction::AddBeadsComment,
                    DegradedModeAction::ContinueNonOverlappingCodeWork,
                ],
            );
            forbid(
                contract,
                &[
                    DegradedModeAction::CountLocalCargoAsProof,
                    DegradedModeAction::RestartRchService,
                    DegradedModeAction::WorkerDrain,
                    DegradedModeAction::RunRemoteProof,
                ],
            );
            restore(
                contract,
                "rch.remote_worker_reached",
                "remote-required proof reaches a remote worker and Cargo starts remotely",
            );
        }
        DegradedModeSurface::Mcp => {
            admit(
                contract,
                &[
                    DegradedModeAction::RobotCliEquivalent,
                    DegradedModeAction::EmitUnsupportedResourceReceipt,
                    DegradedModeAction::AddBeadsComment,
                ],
            );
            forbid(
                contract,
                &[
                    DegradedModeAction::ServiceRestart,
                    DegradedModeAction::ServiceMutation,
                    DegradedModeAction::SemanticZoneClaim,
                ],
            );
            restore(
                contract,
                "mcp.resource_round_trip",
                "MCP resources and tools return typed envelopes without unsupported-resource receipts",
            );
        }
        DegradedModeSurface::Storage => {
            admit(contract, &[DegradedModeAction::RawRedactedGetText]);
            forbid(
                contract,
                &[
                    DegradedModeAction::StorageMutation,
                    DegradedModeAction::SearchClaims,
                    DegradedModeAction::RebuildDerivedStores,
                ],
            );
            restore(
                contract,
                "storage.read_write_health",
                "storage opens read-write, migrations are current, and search/index health checks pass",
            );
        }
        DegradedModeSurface::SemanticPane => {
            admit(contract, &[DegradedModeAction::RawRedactedGetText]);
            forbid(
                contract,
                &[
                    DegradedModeAction::SemanticZoneClaim,
                    DegradedModeAction::StrictVerifiedSubmit,
                ],
            );
            contract
                .notes
                .push(
                    "raw text is confidence-downgraded when semantic zones are unavailable".into(),
                );
            restore(
                contract,
                "semantic.zone_available",
                "pane exposes current semantic zones for the requested query level",
            );
        }
        DegradedModeSurface::Beads => {
            admit(
                contract,
                &[
                    DegradedModeAction::AddBeadsComment,
                    DegradedModeAction::CreateBlockerBead,
                    DegradedModeAction::NonAuthoritativeCoordinationIntent,
                ],
            );
            forbid(
                contract,
                &[
                    DegradedModeAction::ClaimReadyBead,
                    DegradedModeAction::EditOverlappingDirtyPaths,
                ],
            );
            restore(
                contract,
                "beads.ready_query_fresh",
                "br ready and issue update operations complete against fresh graph data",
            );
        }
    }
}

fn admit(contract: &mut DegradedModeContract, actions: &[DegradedModeAction]) {
    for action in actions {
        push_unique(&mut contract.admitted_actions, *action);
    }
}

fn forbid(contract: &mut DegradedModeContract, actions: &[DegradedModeAction]) {
    for action in actions {
        push_unique(&mut contract.forbidden_actions, *action);
    }
}

fn restore(contract: &mut DegradedModeContract, id: &str, summary: &str) {
    contract
        .restore_conditions
        .push(DegradedModeRestoreCondition::new(id, summary));
}

fn push_unique<T: PartialEq>(items: &mut Vec<T>, item: T) {
    if !items.contains(&item) {
        items.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operating_envelope::OperatingEnvelopeSourceSnapshot;

    #[test]
    fn rch_contract_defers_proof_and_forbids_local_proof_substitution() {
        let contract = contract_for_surface(
            DegradedModeSurface::Rch,
            DegradedModeState::Unavailable,
            "rch.no_admissible_workers",
        );

        assert!(contract.admits(DegradedModeAction::LocalHygieneCheck));
        assert!(contract.admits(DegradedModeAction::RecordDeferredProof));
        assert!(contract.forbids(DegradedModeAction::CountLocalCargoAsProof));
        assert!(contract.forbids(DegradedModeAction::RestartRchService));
        assert!(
            contract
                .restore_conditions
                .iter()
                .any(|condition| condition.condition_id == "rch.remote_worker_reached")
        );
    }

    #[test]
    fn agent_mail_contract_prefers_beads_and_forbids_repair() {
        let contract = contract_for_surface(
            DegradedModeSurface::AgentMail,
            DegradedModeState::Unavailable,
            "agent_mail.unreachable",
        );

        assert!(contract.admits(DegradedModeAction::ClaimReadyBead));
        assert!(contract.admits(DegradedModeAction::NonAuthoritativeCoordinationIntent));
        assert!(contract.forbids(DegradedModeAction::RepairAgentMail));
        assert!(contract.forbids(DegradedModeAction::ServiceRestart));
    }

    #[test]
    fn semantic_contract_downgrades_to_raw_redacted_text() {
        let contract = contract_for_surface(
            DegradedModeSurface::SemanticPane,
            DegradedModeState::Unavailable,
            "semantic.unavailable.alt_screen",
        );

        assert!(contract.admits(DegradedModeAction::RawRedactedGetText));
        assert!(contract.forbids(DegradedModeAction::SemanticZoneClaim));
        assert!(contract.forbids(DegradedModeAction::StrictVerifiedSubmit));
        assert!(
            contract
                .notes
                .iter()
                .any(|note| note.contains("confidence-downgraded"))
        );
    }

    #[test]
    fn operating_envelope_snapshot_maps_to_contract() {
        let snapshot =
            OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
                .unavailable("rch.remote_required_refused_local");

        let contract = contract_for_source_snapshot(&snapshot).expect("contract for rch snapshot");

        assert_eq!(contract.surface, DegradedModeSurface::Rch);
        assert_eq!(contract.state, DegradedModeState::Unavailable);
        assert_eq!(contract.reason_code, "rch.remote_required_refused_local");
        assert!(contract.forbids(DegradedModeAction::CountLocalCargoAsProof));
    }

    #[test]
    fn available_or_unmapped_snapshot_returns_no_contract() {
        let available = OperatingEnvelopeSourceSnapshot::new(
            "agent-mail",
            OperatingEnvelopeSourceKind::AgentMail,
        );
        assert!(contract_for_source_snapshot(&available).is_none());

        let git = OperatingEnvelopeSourceSnapshot::new("git", OperatingEnvelopeSourceKind::Git)
            .degraded("git.dirty");
        assert!(contract_for_source_snapshot(&git).is_none());
    }
}
