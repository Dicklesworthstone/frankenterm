#![allow(clippy::module_name_repetitions)]

//! Operator handoff templates derived from proof-doctor verdicts.
//!
//! This module is intentionally pure. It formats Beads comments and optional
//! Agent Mail messages from the existing proof-doctor verdict schema so a proof
//! blocker can be handed off without inventing a parallel classifier.

use serde::{Deserialize, Serialize};

use crate::proof_doctor::{
    ProofDoctorBlocker, ProofDoctorOwner, ProofDoctorPhase, ProofDoctorStatus,
    ProofDoctorToolVersionState, ProofDoctorVerdict,
};

/// Handoff template schema version implemented by this module.
pub const PROOF_HANDOFF_SCHEMA_VERSION: u32 = 1;

/// Rendered Agent Mail message for a targeted proof handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofAgentMailHandoff {
    /// Target agent names. Empty is never used; no mail is emitted instead.
    pub to: Vec<String>,
    /// Concise subject for the owning agent.
    pub subject: String,
    /// Markdown body.
    pub body_md: String,
    /// Suggested Agent Mail importance.
    pub importance: String,
}

/// Rendered handoff package for Beads plus optional Agent Mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofHandoffPackage {
    /// Handoff schema version.
    pub schema_version: u32,
    /// Source proof-doctor verdict id.
    pub verdict_id: String,
    /// Optional owning Bead id.
    pub bead_id: Option<String>,
    /// Optional parent Bead id.
    pub parent_bead_id: Option<String>,
    /// Source proof-doctor status.
    pub status: ProofDoctorStatus,
    /// Source proof-doctor phase.
    pub phase: ProofDoctorPhase,
    /// Stable reason code selected from the verdict.
    pub reason_code: String,
    /// Best owner signal from the primary blocker, when known.
    pub owner: Option<ProofDoctorOwner>,
    /// Beads comment text.
    pub beads_comment: String,
    /// Agent Mail message when a specific non-current owner is known.
    pub agent_mail: Option<ProofAgentMailHandoff>,
    /// Whether the verdict can support closeout.
    pub safe_to_close: bool,
}

/// Build Beads and optional Agent Mail handoff text from a verdict.
#[must_use]
pub fn build_proof_handoff(verdict: &ProofDoctorVerdict) -> ProofHandoffPackage {
    let primary_blocker = verdict.blockers.first();
    let owner = primary_blocker.and_then(|blocker| blocker.owner.clone());
    let reason_code = verdict_reason_code(verdict, primary_blocker);
    let safe_to_close = verdict
        .ledger_projection
        .as_ref()
        .is_some_and(|projection| projection.safe_to_close);
    let beads_comment = render_beads_comment(
        verdict,
        primary_blocker,
        owner.as_ref(),
        &reason_code,
        safe_to_close,
    );
    let agent_mail = owner
        .as_ref()
        .and_then(|owner| target_agent(owner, &verdict.agent_name))
        .map(|agent| render_agent_mail(verdict, primary_blocker, &reason_code, &agent));

    ProofHandoffPackage {
        schema_version: PROOF_HANDOFF_SCHEMA_VERSION,
        verdict_id: verdict.verdict_id.clone(),
        bead_id: verdict.bead_id.clone(),
        parent_bead_id: verdict.parent_bead_id.clone(),
        status: verdict.status,
        phase: verdict.phase,
        reason_code,
        owner,
        beads_comment,
        agent_mail,
        safe_to_close,
    }
}

fn render_beads_comment(
    verdict: &ProofDoctorVerdict,
    blocker: Option<&ProofDoctorBlocker>,
    owner: Option<&ProofDoctorOwner>,
    reason_code: &str,
    safe_to_close: bool,
) -> String {
    let bead = verdict.bead_id.as_deref().unwrap_or("unassigned-bead");
    let status = status_label(verdict.status);
    let phase = phase_label(verdict.phase);
    let command = command_display(&verdict.intended_command);
    let remote = remote_cargo_label(verdict.evidence.remote_cargo_reached);
    let tool = tool_state_label(verdict.evidence.tool_version_state);
    let closeout = if safe_to_close {
        "safe to close from this verdict"
    } else {
        "closeout blocked by this verdict"
    };
    let owner_text = owner
        .map(owner_label)
        .unwrap_or_else(|| "no specific owner target".to_string());
    let affected_paths = affected_paths_label(blocker);
    let summary = status_summary(verdict, blocker);
    let next_action = &verdict.next_action.message;

    format!(
        "Proof-doctor handoff for {bead}: {status}. Verdict {verdict_id}; phase {phase}; reason {reason_code}; {remote}; RCH tool state {tool}; owner {owner_text}; {closeout}. Command: `{command}`. {affected_paths} Summary: {summary}. Next action: {next_action}",
        verdict_id = verdict.verdict_id,
    )
}

fn render_agent_mail(
    verdict: &ProofDoctorVerdict,
    blocker: Option<&ProofDoctorBlocker>,
    reason_code: &str,
    target_agent: &str,
) -> ProofAgentMailHandoff {
    let bead = verdict.bead_id.as_deref().unwrap_or("unassigned-bead");
    let status = status_label(verdict.status);
    let command = command_display(&verdict.intended_command);
    let remote = remote_cargo_label(verdict.evidence.remote_cargo_reached);
    let tool = tool_state_label(verdict.evidence.tool_version_state);
    let affected_paths = affected_paths_label(blocker);
    let summary = status_summary(verdict, blocker);
    let next_action = &verdict.next_action.message;
    let subject = format!("{bead} proof-doctor {status}: {reason_code}");
    let body_md = format!(
        "Proof-doctor produced a targeted handoff for `{bead}`.\n\n- Verdict: `{verdict_id}`\n- Status: `{status}`\n- Reason: `{reason_code}`\n- Remote Cargo: `{remote}`\n- RCH tool state: `{tool}`\n- Command: `{command}`\n- {affected_paths}\n\nSummary: {summary}\n\nNext action: {next_action}",
        verdict_id = verdict.verdict_id,
    );

    ProofAgentMailHandoff {
        to: vec![target_agent.to_string()],
        subject,
        body_md,
        importance: mail_importance(verdict.status).to_string(),
    }
}

fn verdict_reason_code(
    verdict: &ProofDoctorVerdict,
    blocker: Option<&ProofDoctorBlocker>,
) -> String {
    blocker.map_or_else(
        || {
            verdict.ledger_projection.as_ref().map_or_else(
                || "proof.no_blocker".to_string(),
                |projection| projection.reason_code.clone(),
            )
        },
        |blocker| blocker.reason_code.clone(),
    )
}

fn target_agent(owner: &ProofDoctorOwner, current_agent: &str) -> Option<String> {
    match owner {
        ProofDoctorOwner::OtherAgent { agent_name, .. }
        | ProofDoctorOwner::Reservation { agent_name, .. }
            if agent_name != current_agent =>
        {
            Some(agent_name.clone())
        }
        ProofDoctorOwner::Bead {
            assignee: Some(agent_name),
            ..
        } if agent_name != current_agent => Some(agent_name.clone()),
        ProofDoctorOwner::CurrentAgent { .. }
        | ProofDoctorOwner::OtherAgent { .. }
        | ProofDoctorOwner::Bead { .. }
        | ProofDoctorOwner::Reservation { .. }
        | ProofDoctorOwner::Unknown => None,
    }
}

fn status_summary(verdict: &ProofDoctorVerdict, blocker: Option<&ProofDoctorBlocker>) -> String {
    match verdict.status {
        ProofDoctorStatus::Passed => {
            "Remote proof passed with retained evidence; attach pass evidence before closeout."
                .to_string()
        }
        ProofDoctorStatus::InfraBlocked => blocker.map_or_else(
            || "Infrastructure blocked proof before a source verdict was available.".to_string(),
            |blocker| blocker.message.clone(),
        ),
        ProofDoctorStatus::SourceBlocked => blocker.map_or_else(
            || "Remote Cargo or rustc reported a source-owned failure.".to_string(),
            |blocker| blocker.message.clone(),
        ),
        ProofDoctorStatus::TestBlocked => blocker.map_or_else(
            || "Remote assertion execution failed.".to_string(),
            |blocker| blocker.message.clone(),
        ),
        ProofDoctorStatus::DirtyTreeBlocked | ProofDoctorStatus::OwnershipBlocked => blocker
            .map_or_else(
                || "Active ownership or dirty-tree state blocks proof attribution.".to_string(),
                |blocker| blocker.message.clone(),
            ),
        ProofDoctorStatus::Invalid => blocker.map_or_else(
            || "Command shape or policy is invalid for this proof lane.".to_string(),
            |blocker| blocker.message.clone(),
        ),
        ProofDoctorStatus::SkippedNotProven => blocker.map_or_else(
            || "Required predicate is absent; this lane is skipped, not proven.".to_string(),
            |blocker| blocker.message.clone(),
        ),
        ProofDoctorStatus::Inconclusive => blocker.map_or_else(
            || "Evidence is incomplete; this lane is not green proof.".to_string(),
            |blocker| blocker.message.clone(),
        ),
        ProofDoctorStatus::Runnable => verdict.operator_summary.clone(),
    }
}

fn affected_paths_label(blocker: Option<&ProofDoctorBlocker>) -> String {
    let Some(blocker) = blocker else {
        return "Affected paths: none.".to_string();
    };

    if blocker.affected_paths.is_empty() {
        "Affected paths: none.".to_string()
    } else {
        format!("Affected paths: {}.", blocker.affected_paths.join(", "))
    }
}

fn owner_label(owner: &ProofDoctorOwner) -> String {
    match owner {
        ProofDoctorOwner::CurrentAgent {
            agent_name,
            bead_id,
        } => bead_id.as_ref().map_or_else(
            || format!("current agent {agent_name}"),
            |bead_id| format!("current agent {agent_name} on {bead_id}"),
        ),
        ProofDoctorOwner::OtherAgent {
            agent_name,
            bead_id,
        } => bead_id.as_ref().map_or_else(
            || format!("agent {agent_name}"),
            |bead_id| format!("agent {agent_name} on {bead_id}"),
        ),
        ProofDoctorOwner::Bead { bead_id, assignee } => assignee.as_ref().map_or_else(
            || format!("Bead {bead_id}"),
            |assignee| format!("Bead {bead_id} assigned to {assignee}"),
        ),
        ProofDoctorOwner::Reservation {
            agent_name,
            path_pattern,
        } => format!("reservation by {agent_name} for {path_pattern}"),
        ProofDoctorOwner::Unknown => "unknown owner".to_string(),
    }
}

fn command_display(command: &[String]) -> String {
    if command.is_empty() {
        "(no command recorded)".to_string()
    } else {
        command.join(" ")
    }
}

const fn status_label(status: ProofDoctorStatus) -> &'static str {
    match status {
        ProofDoctorStatus::Runnable => "runnable",
        ProofDoctorStatus::Passed => "passed",
        ProofDoctorStatus::SourceBlocked => "source_blocked",
        ProofDoctorStatus::TestBlocked => "test_blocked",
        ProofDoctorStatus::InfraBlocked => "infra_blocked",
        ProofDoctorStatus::DirtyTreeBlocked => "dirty_tree_blocked",
        ProofDoctorStatus::OwnershipBlocked => "ownership_blocked",
        ProofDoctorStatus::Invalid => "invalid",
        ProofDoctorStatus::SkippedNotProven => "skipped_not_proven",
        ProofDoctorStatus::Inconclusive => "inconclusive",
    }
}

const fn phase_label(phase: ProofDoctorPhase) -> &'static str {
    match phase {
        ProofDoctorPhase::Preflight => "preflight",
        ProofDoctorPhase::LaunchObserved => "launch_observed",
        ProofDoctorPhase::RemoteCargoObserved => "remote_cargo_observed",
        ProofDoctorPhase::TerminalClassified => "terminal_classified",
        ProofDoctorPhase::EvidenceGap => "evidence_gap",
    }
}

const fn remote_cargo_label(remote_cargo_reached: bool) -> &'static str {
    if remote_cargo_reached {
        "remote Cargo reached"
    } else {
        "remote Cargo not reached"
    }
}

const fn tool_state_label(state: ProofDoctorToolVersionState) -> &'static str {
    match state {
        ProofDoctorToolVersionState::Unknown => "unknown",
        ProofDoctorToolVersionState::InstalledCurrent => "installed_current",
        ProofDoctorToolVersionState::InstalledStale => "installed_stale",
        ProofDoctorToolVersionState::PatchedLocal => "patched_local",
        ProofDoctorToolVersionState::Mixed => "mixed",
    }
}

const fn mail_importance(status: ProofDoctorStatus) -> &'static str {
    match status {
        ProofDoctorStatus::Passed | ProofDoctorStatus::Runnable => "normal",
        ProofDoctorStatus::Inconclusive | ProofDoctorStatus::SkippedNotProven => "normal",
        ProofDoctorStatus::SourceBlocked
        | ProofDoctorStatus::TestBlocked
        | ProofDoctorStatus::InfraBlocked
        | ProofDoctorStatus::DirtyTreeBlocked
        | ProofDoctorStatus::OwnershipBlocked
        | ProofDoctorStatus::Invalid => "high",
    }
}

#[cfg(test)]
mod tests {
    use crate::proof_doctor::{
        ProofDoctorDirtyPath, ProofDoctorEvidence, ProofDoctorOwner, ProofDoctorPhase,
        ProofDoctorPreflightInput, ProofDoctorToolVersionState, classify_proof_doctor,
    };
    use crate::proof_lane::{ArtifactRetrievalStatus, ProofBackend, ProofScope};

    use super::build_proof_handoff;

    fn base_input() -> ProofDoctorPreflightInput {
        ProofDoctorPreflightInput {
            bead_id: Some("ft-wik9p.5".to_string()),
            parent_bead_id: Some("ft-wik9p".to_string()),
            agent_name: "MagentaFalcon".to_string(),
            repo_path: "/Users/jemanuel/projects/frankenterm".to_string(),
            git_head: "adac4acd5".to_string(),
            branch: "main".to_string(),
            generated_at_utc: "2026-05-05T13:40:00Z".to_string(),
            intended_command: vec![
                "rch".to_string(),
                "exec".to_string(),
                "--".to_string(),
                "env".to_string(),
                "CARGO_TARGET_DIR=/tmp/ft-wik9p5-target".to_string(),
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "frankenterm-core-audit-types".to_string(),
                "proof_handoff".to_string(),
            ],
            intended_target_dir: Some("/tmp/ft-wik9p5-target".to_string()),
            intended_scope: ProofScope::CargoTest,
            required_backend: ProofBackend::Rch,
            phase: ProofDoctorPhase::TerminalClassified,
            proof_path_prefixes: vec!["crates/frankenterm-core-audit-types".to_string()],
            evidence: ProofDoctorEvidence::default(),
        }
    }

    #[test]
    fn rch_pre_cargo_failure_handoff_is_infra_not_source() {
        let mut input = base_input();
        input.evidence.selected_worker = Some("vmi1149989".to_string());
        input.evidence.sync_duration_ms = Some(139_961);
        input.evidence.wrapper_exit_code = Some(127);

        let verdict = classify_proof_doctor(&input);
        let handoff = build_proof_handoff(&verdict);

        assert_eq!(
            handoff.reason_code,
            "proof.rch.pre_cargo_timeout_exec_missing"
        );
        assert!(handoff.beads_comment.contains("infra_blocked"));
        assert!(handoff.beads_comment.contains("remote Cargo not reached"));
        assert!(!handoff.beads_comment.contains("source_blocked"));
        assert!(handoff.agent_mail.is_none());
    }

    #[test]
    fn remote_compile_failure_handoff_keeps_path_and_remote_cargo_evidence() {
        let mut input = base_input();
        input.evidence.remote_cargo_reached = true;
        input.evidence.rustc_reached = true;
        input.evidence.remote_exit_code = Some(101);
        input.evidence.diagnostic_paths =
            vec!["crates/frankenterm-core/src/resource_pressure.rs".to_string()];
        input.evidence.diagnostic_summary =
            Some("Remote rustc reported missing field `worker_id`.".to_string());

        let verdict = classify_proof_doctor(&input);
        let handoff = build_proof_handoff(&verdict);

        assert!(handoff.beads_comment.contains("source_blocked"));
        assert!(handoff.beads_comment.contains("remote Cargo reached"));
        assert!(
            handoff
                .beads_comment
                .contains("crates/frankenterm-core/src/resource_pressure.rs")
        );
        assert!(handoff.beads_comment.contains("missing field `worker_id`"));
    }

    #[test]
    fn dirty_owned_path_targets_single_known_owner() {
        let mut input = base_input();
        input.evidence.dirty_paths.push(ProofDoctorDirtyPath {
            path: "crates/frankenterm-core-audit-types/src/proof_doctor.rs".to_string(),
            status: "M".to_string(),
            affects_proof: true,
            owner: Some(ProofDoctorOwner::Bead {
                bead_id: "ft-wik9p.6".to_string(),
                assignee: Some("OliveChapel".to_string()),
            }),
        });

        let verdict = classify_proof_doctor(&input);
        let handoff = build_proof_handoff(&verdict);
        let mail = handoff
            .agent_mail
            .expect("known assignee gets targeted mail");

        assert_eq!(mail.to, vec!["OliveChapel"]);
        assert!(mail.subject.contains("ft-wik9p.5"));
        assert!(mail.body_md.contains("dirty_tree_blocked"));
        assert!(
            mail.body_md
                .contains("crates/frankenterm-core-audit-types/src/proof_doctor.rs")
        );
    }

    #[test]
    fn stale_installed_tooling_handoff_distinguishes_tool_state() {
        let mut input = base_input();
        input.evidence.rch_binary_path = Some("/Users/jemanuel/.local/bin/rch".to_string());
        input.evidence.rch_external_timeout_enabled = Some(false);
        input.evidence.stale_external_timeout_observed = true;
        input.evidence.tool_version_state = ProofDoctorToolVersionState::InstalledStale;

        let verdict = classify_proof_doctor(&input);
        let handoff = build_proof_handoff(&verdict);

        assert!(handoff.beads_comment.contains("infra_blocked"));
        assert!(handoff.beads_comment.contains("installed_stale"));
        assert!(
            handoff
                .beads_comment
                .contains("Effective RCH config disables the external timeout wrapper")
        );
    }

    #[test]
    fn inconclusive_sync_handoff_does_not_promote_green_proof() {
        let mut input = base_input();
        input.phase = ProofDoctorPhase::LaunchObserved;
        input.evidence.selected_worker = Some("vmi1264463".to_string());
        input.evidence.sync_duration_ms = Some(180_611);

        let verdict = classify_proof_doctor(&input);
        let handoff = build_proof_handoff(&verdict);

        assert!(handoff.beads_comment.contains("inconclusive"));
        assert!(handoff.beads_comment.contains("remote Cargo not reached"));
        assert!(!handoff.safe_to_close);
        assert!(
            !handoff
                .beads_comment
                .contains("safe to close from this verdict")
        );
    }

    #[test]
    fn passed_handoff_allows_closeout_without_broadcast_mail() {
        let mut input = base_input();
        input.evidence.remote_cargo_reached = true;
        input.evidence.rustc_reached = true;
        input.evidence.test_binary_started = true;
        input.evidence.remote_exit_code = Some(0);
        input.evidence.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        input.evidence.tool_version_state = ProofDoctorToolVersionState::PatchedLocal;

        let verdict = classify_proof_doctor(&input);
        let handoff = build_proof_handoff(&verdict);

        assert!(handoff.safe_to_close);
        assert!(handoff.beads_comment.contains("passed"));
        assert!(handoff.beads_comment.contains("patched_local"));
        assert!(handoff.agent_mail.is_none());
    }
}
