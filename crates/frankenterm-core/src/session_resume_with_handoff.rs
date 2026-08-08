//! Handoff-capsule-aware session resume planner
//! (ft-1650n.5 slice 3).
//!
//! Wraps [`crate::session_resume::SessionResumer`] with a three-state
//! commit shape that integrates [`crate::handoff_capsule`]:
//!
//!   PRE-RESUME    plan.preflight_validate(...)
//!     → integrity check + destination-compatibility validation
//!     → consumes the unvalidated plan and returns validated authority
//!     → on integrity mismatch / version mismatch → bail BEFORE any
//!       casr subprocess runs (no side effects from a tampered or
//!       incompatible capsule)
//!
//!   RESUME         validated.invoke_resume(&SessionResumer)
//!     → delegates to SessionResumer::resume_session
//!     → consumes validated authority and returns completion authority
//!
//!   POST-RESUME   completed.apply_to_resumed_session(outcomes)
//!     → produces an AppliedHandoffSummary recording which Allowed
//!       sections were applied, with per-section apply outcomes
//!       (Applied / SkippedByPolicy / Errored)
//!
//! # Why three states?
//!
//! Resume invokes a casr subprocess that ultimately mutates pane
//! state. We MUST NOT spend that side-effect on a capsule that's
//! later going to skip every section anyway. Pre-validation gates
//! the work; if every guarded section would skip and the operator
//! wanted full inheritance, they can fix the destination's
//! capability passport before paying the resume cost.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::capability_passport::CapabilityPassport;
use crate::casr_types::CasrResumeOutput;
use crate::handoff_capsule::{
    CapsuleSection, CapsuleValidationError, HandoffCapsule, ValidationOutcome,
};
use crate::session_resume::{
    AgentProvider, MAX_SESSION_RESUME_ID_BYTES, SessionResumeError, SessionResumer,
    validate_provider_slug,
};
use crate::session_topology::LifecycleEntityKind;

/// Plan for resuming a session with handoff-capsule integration.
///
/// Construct from a capsule + destination passport + resume target;
/// call [`HandoffAwareResumePlan::preflight_validate`] to check
/// compatibility BEFORE the casr subprocess fires. Only the returned
/// [`ValidatedHandoffResumePlan`] can invoke the resume, and only the resulting
/// [`CompletedHandoffResume`] can record apply outcomes. Callers therefore
/// cannot bypass either preflight or the corresponding resume invocation.
#[derive(Debug)]
pub struct HandoffAwareResumePlan<'a> {
    capsule: &'a HandoffCapsule,
    destination_passport: Option<&'a CapabilityPassport>,
    session_id: String,
    target_provider: AgentProvider,
}

impl<'a> HandoffAwareResumePlan<'a> {
    #[must_use]
    pub fn new(
        capsule: &'a HandoffCapsule,
        destination_passport: Option<&'a CapabilityPassport>,
        session_id: impl Into<String>,
        target_provider: AgentProvider,
    ) -> Self {
        Self {
            capsule,
            destination_passport,
            session_id: session_id.into(),
            target_provider,
        }
    }

    /// Phase 1 — preflight validation. Runs BEFORE any resume work.
    /// Consumes the unvalidated plan and returns the sole authority capable of
    /// invoking resume. The authority carries the exact accepted/skipped
    /// section decision used by post-resume accounting. Returns
    /// `Err(CapsuleValidationError)` on integrity or version
    /// mismatch without exposing any invocation method.
    pub fn preflight_validate(
        self,
    ) -> Result<ValidatedHandoffResumePlan<'a>, CapsuleValidationError> {
        let validation = self
            .capsule
            .validate_for_destination(self.destination_passport)?;
        Ok(ValidatedHandoffResumePlan {
            capsule: self.capsule,
            session_id: self.session_id,
            target_provider: self.target_provider,
            validation,
        })
    }
}

/// Preflight-validated authority for the mutating resume and its post-resume
/// accounting. This type cannot be constructed outside this module.
#[derive(Debug)]
pub struct ValidatedHandoffResumePlan<'a> {
    capsule: &'a HandoffCapsule,
    session_id: String,
    target_provider: AgentProvider,
    validation: ValidationOutcome,
}

impl<'a> ValidatedHandoffResumePlan<'a> {
    /// Exact preflight decision carried by this authority.
    #[must_use]
    pub fn validation(&self) -> &ValidationOutcome {
        &self.validation
    }

    /// Phase 2 — invoke the underlying SessionResumer. The type system proves
    /// that capsule integrity and destination compatibility were checked. The
    /// authority is single-use and becomes [`CompletedHandoffResume`].
    pub fn invoke_resume(
        self,
        resumer: &SessionResumer,
    ) -> Result<CompletedHandoffResume<'a>, SessionResumeError> {
        let resume_output = resumer.resume_session(&self.session_id, &self.target_provider)?;
        Ok(CompletedHandoffResume {
            validated: self,
            resume_output,
        })
    }

    /// Cx-first Phase 2. Caller cancellation reaches the bounded subprocess
    /// supervisor through [`SessionResumer::resume_session_with_cx`].
    pub async fn invoke_resume_with_cx(
        self,
        cx: &crate::cx::Cx,
        resumer: &SessionResumer,
    ) -> Result<CompletedHandoffResume<'a>, SessionResumeError> {
        let resume_output = resumer
            .resume_session_with_cx(cx, &self.session_id, &self.target_provider)
            .await?;
        Ok(CompletedHandoffResume {
            validated: self,
            resume_output,
        })
    }
}

/// Single-use authority proving that preflight succeeded and the corresponding
/// resume invocation returned. Only this state can record apply outcomes.
#[derive(Debug)]
pub struct CompletedHandoffResume<'a> {
    validated: ValidatedHandoffResumePlan<'a>,
    resume_output: CasrResumeOutput,
}

impl CompletedHandoffResume<'_> {
    /// Inspect the invocation result before deciding whether to apply accepted
    /// capsule sections.
    #[must_use]
    pub fn resume_output(&self) -> &CasrResumeOutput {
        &self.resume_output
    }

    /// Consume the completion authority without applying handoff sections.
    #[must_use]
    pub fn into_resume_output(self) -> CasrResumeOutput {
        self.resume_output
    }

    /// Phase 3 — record per-section apply outcomes. Pure-function:
    /// takes the carried post-resume output + validation outcome and produces
    /// an [`AppliedHandoffSummary`]. Does not
    /// itself mutate the resumed session — the caller is responsible
    /// for actually applying each accepted section to whatever store
    /// receives the inheritance (passport store, pending-approval
    /// queue, etc). The summary records the operator-supplied apply
    /// outcomes per section. The exact accepted-index set and canonical section
    /// labels are enforced; duplicates, omissions, and unaccepted outcomes are
    /// rejected rather than producing a misleading clean summary.
    pub fn apply_to_resumed_session(
        self,
        per_section_outcomes: Vec<SectionApplyOutcome>,
    ) -> Result<AppliedHandoffSummary, HandoffApplySummaryError> {
        if !self.resume_output.ok {
            return Err(HandoffApplySummaryError::ResumeReportedFailure);
        }
        if self.resume_output.dry_run {
            return Err(HandoffApplySummaryError::ResumeWasDryRun);
        }
        if self.resume_output.source_session_id.as_deref()
            != Some(self.validated.session_id.as_str())
        {
            return Err(HandoffApplySummaryError::SourceSessionMismatch);
        }
        let Some(output_target_provider) = self.resume_output.target_provider.as_deref() else {
            return Err(HandoffApplySummaryError::TargetProviderMismatch);
        };
        if validate_provider_slug(output_target_provider).is_err()
            || AgentProvider::from_slug(output_target_provider) != self.validated.target_provider
        {
            return Err(HandoffApplySummaryError::TargetProviderMismatch);
        }
        let Some(resumed_session_id) = self.resume_output.target_session_id.as_deref() else {
            return Err(HandoffApplySummaryError::MissingResumedSessionId);
        };
        if resumed_session_id.is_empty()
            || resumed_session_id.len() > MAX_SESSION_RESUME_ID_BYTES
            || resumed_session_id.trim() != resumed_session_id
            || resumed_session_id.chars().any(char::is_control)
        {
            return Err(HandoffApplySummaryError::InvalidResumedSessionId {
                identifier_bytes: resumed_session_id.len(),
            });
        }
        let resumed_session_id = resumed_session_id.to_string();
        validate_apply_outcomes(
            self.validated.capsule,
            &self.validated.validation,
            &per_section_outcomes,
        )?;
        let ValidatedHandoffResumePlan {
            capsule,
            session_id,
            target_provider,
            validation,
        } = self.validated;
        Ok(AppliedHandoffSummary {
            session_id,
            target_provider_slug: target_provider.slug().to_string(),
            resumed_session_id,
            capsule_version: capsule.version,
            accepted_indices: validation.accepted,
            skipped_count: validation.skipped.len(),
            applied: per_section_outcomes,
        })
    }
}

/// Structural rejection from post-resume apply accounting. These errors carry
/// only finite indices/counts, never capsule content or operator error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffApplySummaryError {
    ResumeReportedFailure,
    ResumeWasDryRun,
    SourceSessionMismatch,
    TargetProviderMismatch,
    MissingResumedSessionId,
    InvalidResumedSessionId { identifier_bytes: usize },
    InvalidAcceptedSectionIndex { section_index: usize },
    DuplicateAcceptedSectionIndex { section_index: usize },
    DuplicateSectionOutcome { section_index: usize },
    UnacceptedSectionOutcome { section_index: usize },
    MissingAcceptedSectionOutcome { section_index: usize },
    SectionLabelMismatch { section_index: usize },
}

impl std::fmt::Display for HandoffApplySummaryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, section_index) = match self {
            Self::ResumeReportedFailure => {
                return formatter.write_str("cannot apply handoff after failed resume");
            }
            Self::ResumeWasDryRun => {
                return formatter.write_str("cannot apply handoff after dry-run resume");
            }
            Self::SourceSessionMismatch => {
                return formatter
                    .write_str("resume output did not match the requested source session");
            }
            Self::TargetProviderMismatch => {
                return formatter
                    .write_str("resume output did not match the requested target provider");
            }
            Self::MissingResumedSessionId => {
                return formatter.write_str("resume output omitted the resumed session identity");
            }
            Self::InvalidResumedSessionId { identifier_bytes } => {
                return write!(
                    formatter,
                    "resume output carried invalid session identity (identifier_bytes={identifier_bytes})"
                );
            }
            Self::InvalidAcceptedSectionIndex { section_index } => {
                ("invalid_accepted_index", section_index)
            }
            Self::DuplicateAcceptedSectionIndex { section_index } => {
                ("duplicate_accepted_index", section_index)
            }
            Self::DuplicateSectionOutcome { section_index } => ("duplicate_outcome", section_index),
            Self::UnacceptedSectionOutcome { section_index } => {
                ("unaccepted_outcome", section_index)
            }
            Self::MissingAcceptedSectionOutcome { section_index } => {
                ("missing_outcome", section_index)
            }
            Self::SectionLabelMismatch { section_index } => {
                ("section_label_mismatch", section_index)
            }
        };
        write!(
            formatter,
            "invalid handoff apply summary (kind={kind}, section_index={section_index})"
        )
    }
}

impl std::error::Error for HandoffApplySummaryError {}

fn validate_apply_outcomes(
    capsule: &HandoffCapsule,
    validation: &ValidationOutcome,
    outcomes: &[SectionApplyOutcome],
) -> Result<(), HandoffApplySummaryError> {
    let mut accepted = HashSet::with_capacity(validation.accepted.len());
    for &section_index in &validation.accepted {
        if section_index >= capsule.sections.len() {
            return Err(HandoffApplySummaryError::InvalidAcceptedSectionIndex { section_index });
        }
        if !accepted.insert(section_index) {
            return Err(HandoffApplySummaryError::DuplicateAcceptedSectionIndex { section_index });
        }
    }

    let mut observed = HashSet::with_capacity(outcomes.len());
    for outcome in outcomes {
        if !observed.insert(outcome.section_index) {
            return Err(HandoffApplySummaryError::DuplicateSectionOutcome {
                section_index: outcome.section_index,
            });
        }
        if !accepted.contains(&outcome.section_index) {
            return Err(HandoffApplySummaryError::UnacceptedSectionOutcome {
                section_index: outcome.section_index,
            });
        }
        if capsule.sections[outcome.section_index].label() != outcome.section_label.as_str() {
            return Err(HandoffApplySummaryError::SectionLabelMismatch {
                section_index: outcome.section_index,
            });
        }
    }
    for &section_index in &validation.accepted {
        if !observed.contains(&section_index) {
            return Err(HandoffApplySummaryError::MissingAcceptedSectionOutcome { section_index });
        }
    }
    Ok(())
}

/// Recorded outcome for one capsule section's post-resume apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionApplyOutcome {
    pub section_index: usize,
    pub section_label: String,
    pub disposition: SectionDisposition,
}

/// What happened when the operator-side apply step ran a section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SectionDisposition {
    /// Section was applied to its target store / queue successfully.
    Applied,
    /// Section was eligible per preflight but the operator's apply
    /// step decided to skip (e.g. operator-supplied filter, or the
    /// section was redundant with destination state).
    SkippedByPolicy { reason: String },
    /// Apply attempt failed with the carried operator-readable
    /// error. Distinguishes "tried and failed" from "preflight said
    /// no" for triage.
    Errored { error: String },
}

/// Summary of a complete handoff-aware resume cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedHandoffSummary {
    pub session_id: String,
    pub target_provider_slug: String,
    /// Session ID returned by casr post-resume (may equal
    /// `session_id` for in-place resume; may differ on fork).
    pub resumed_session_id: String,
    pub capsule_version: u32,
    /// Indices into capsule.sections that preflight allowed.
    pub accepted_indices: Vec<usize>,
    /// Number of sections preflight skipped.
    pub skipped_count: usize,
    /// Per-section apply outcomes for the accepted indices, in
    /// caller-supplied order.
    pub applied: Vec<SectionApplyOutcome>,
}

impl AppliedHandoffSummary {
    /// True iff every accepted section was either Applied or
    /// SkippedByPolicy (no Errored). Useful for operator dashboards
    /// that want a single "did the handoff land cleanly" boolean.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.has_complete_outcome_coverage()
            && self
                .applied
                .iter()
                .all(|o| !matches!(o.disposition, SectionDisposition::Errored { .. }))
    }

    /// True when serialized or manually assembled summaries still contain one
    /// and only one outcome for every accepted section and no others.
    #[must_use]
    pub fn has_complete_outcome_coverage(&self) -> bool {
        if self.applied.len() != self.accepted_indices.len() {
            return false;
        }
        let accepted = self
            .accepted_indices
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let observed = self
            .applied
            .iter()
            .map(|outcome| outcome.section_index)
            .collect::<HashSet<_>>();
        accepted.len() == self.accepted_indices.len()
            && observed.len() == self.applied.len()
            && observed == accepted
    }

    /// Count of sections that landed at each disposition.
    #[must_use]
    pub fn disposition_counts(&self) -> DispositionCounts {
        let mut counts = DispositionCounts::default();
        for o in &self.applied {
            match &o.disposition {
                SectionDisposition::Applied => counts.applied += 1,
                SectionDisposition::SkippedByPolicy { .. } => counts.skipped_by_policy += 1,
                SectionDisposition::Errored { .. } => counts.errored += 1,
            }
        }
        counts
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispositionCounts {
    pub applied: usize,
    pub skipped_by_policy: usize,
    pub errored: usize,
}

/// Reasons the operator might cite when classifying a section as
/// safe to skip even though preflight allowed it. Stable strings so
/// dashboards can filter / count consistently.
pub mod skip_by_policy_reasons {
    pub const REDUNDANT_WITH_DESTINATION: &str = "redundant_with_destination";
    pub const OPERATOR_OPTED_OUT: &str = "operator_opted_out";
    pub const INHERITANCE_DISABLED_FOR_KIND: &str = "inheritance_disabled_for_kind";
}

/// True iff the capsule references a section kind that the
/// destination's lifecycle entity kind even can consume. E.g.
/// applying a PassportExcerpt requires the destination to be a
/// pane (not a session-level entity). Pure-function predicate
/// callers may consult before invoking the planner.
#[must_use]
pub fn section_kind_compatible_with_destination(
    section: &CapsuleSection,
    destination_kind: LifecycleEntityKind,
) -> bool {
    match section {
        CapsuleSection::PassportExcerpt { .. } => {
            // PassportExcerpts apply only at pane scope.
            matches!(destination_kind, LifecycleEntityKind::Pane)
        }
        // All other section kinds are entity-kind-agnostic.
        CapsuleSection::ContextSummary { .. }
        | CapsuleSection::VerificationChecklist { .. }
        | CapsuleSection::MissionState { .. }
        | CapsuleSection::CausalSummary { .. }
        | CapsuleSection::PendingApprovals { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_passport::{
        CapabilityClass, CapabilityEntry, CapabilityPassport, CapabilityVerification, RedactedProof,
    };
    use crate::handoff_capsule::{CapsuleEndpoint, CapsuleSection, HandoffCapsule};

    fn dest_passport(verified: &[&str]) -> CapabilityPassport {
        let capabilities = verified
            .iter()
            .map(|s| CapabilityEntry {
                class: CapabilityClass::SafetyConstraint((*s).to_string()),
                verification: CapabilityVerification::Verified,
                last_observed_at_ms: Some(1_000_000),
                proof: RedactedProof::from_value(s.as_bytes()),
            })
            .collect();
        CapabilityPassport {
            agent_id: "dest-agent".into(),
            pane_id: Some(2),
            capabilities,
            generation: 1,
            signed_at_ms: 1_000_000,
        }
    }

    fn sample_capsule() -> HandoffCapsule {
        HandoffCapsule::build(
            CapsuleEndpoint {
                agent_id: "source-agent".into(),
                pane_id: Some(1),
                label: None,
            },
            CapsuleEndpoint {
                agent_id: "dest-agent".into(),
                pane_id: Some(2),
                label: None,
            },
            vec![
                CapsuleSection::ContextSummary {
                    text: "context".into(),
                },
                CapsuleSection::MissionState {
                    payload: serde_json::json!({"k": 1}),
                },
                CapsuleSection::PendingApprovals {
                    count: 1,
                    summaries: vec!["approve git push".into()],
                },
            ],
            1_500_000,
        )
    }

    fn fake_resume_output() -> CasrResumeOutput {
        CasrResumeOutput {
            ok: true,
            source_provider: Some("claude-code".into()),
            target_provider: Some("claude-code".into()),
            source_session_id: Some("src-session".into()),
            target_session_id: Some("casr-resumed-session-id".into()),
            written_paths: None,
            resume_command: None,
            dry_run: false,
            warnings: Vec::new(),
            extra: std::collections::HashMap::new(),
        }
    }

    fn completed_resume<'a>(
        capsule: &'a HandoffCapsule,
        destination_passport: Option<&'a CapabilityPassport>,
        resume_output: CasrResumeOutput,
    ) -> CompletedHandoffResume<'a> {
        let validated = HandoffAwareResumePlan::new(
            capsule,
            destination_passport,
            "src-session",
            AgentProvider::ClaudeCode,
        )
        .preflight_validate()
        .expect("validate ok");
        CompletedHandoffResume {
            validated,
            resume_output,
        }
    }

    #[test]
    fn preflight_validate_returns_full_acceptance_when_destination_satisfies_all() {
        let capsule = sample_capsule();
        let dest = dest_passport(&["inherit_mission_state", "inherit_pending_approvals"]);
        let provider = AgentProvider::ClaudeCode;
        let plan = HandoffAwareResumePlan::new(&capsule, Some(&dest), "src-session", provider);
        let validated = plan.preflight_validate().expect("validate ok");
        let outcome = validated.validation();
        assert!(outcome.is_fully_accepted());
        // 3 sections, all accepted.
        assert_eq!(outcome.accepted, vec![0, 1, 2]);
    }

    #[test]
    fn preflight_validate_skips_sections_destination_cannot_satisfy() {
        let capsule = sample_capsule();
        // Destination has only inherit_mission_state Verified — so
        // ContextSummary (unguarded) + MissingState accepted; the
        // PendingApprovals section is skipped.
        let dest = dest_passport(&["inherit_mission_state"]);
        let plan = HandoffAwareResumePlan::new(
            &capsule,
            Some(&dest),
            "src-session",
            AgentProvider::ClaudeCode,
        );
        let validated = plan.preflight_validate().expect("validate ok");
        let outcome = validated.validation();
        assert!(!outcome.is_fully_accepted());
        assert_eq!(outcome.accepted, vec![0, 1]);
        assert_eq!(outcome.skipped.len(), 1);
        assert_eq!(outcome.skipped[0].section_label, "pending_approvals");
    }

    #[test]
    fn preflight_validate_short_circuits_on_integrity_mismatch_before_resume() {
        let mut capsule = sample_capsule();
        // Tamper a section after build.
        match &mut capsule.sections[0] {
            CapsuleSection::ContextSummary { text } => text.push_str(" — TAMPERED"),
            _ => unreachable!(),
        }
        let dest = dest_passport(&["inherit_mission_state", "inherit_pending_approvals"]);
        let plan = HandoffAwareResumePlan::new(
            &capsule,
            Some(&dest),
            "src-session",
            AgentProvider::ClaudeCode,
        );
        let err = plan.preflight_validate().unwrap_err();
        assert!(matches!(
            err,
            CapsuleValidationError::IntegrityMismatch { .. }
        ));
    }

    #[test]
    fn preflight_validate_no_destination_passport_skips_every_guarded_section() {
        let capsule = sample_capsule();
        let plan =
            HandoffAwareResumePlan::new(&capsule, None, "src-session", AgentProvider::ClaudeCode);
        let validated = plan.preflight_validate().expect("validate ok");
        let outcome = validated.validation();
        // Only ContextSummary (unguarded) accepted; Mission +
        // Approvals skipped.
        assert_eq!(outcome.accepted, vec![0]);
        assert_eq!(outcome.skipped.len(), 2);
    }

    #[test]
    fn completed_authority_carries_the_exact_resume_output() {
        let capsule = sample_capsule();
        let expected = fake_resume_output();
        let completed = completed_resume(&capsule, None, expected.clone());
        assert_eq!(completed.resume_output(), &expected);
        assert_eq!(completed.into_resume_output(), expected);
    }

    #[test]
    fn handoff_typestate_surface_is_single_use_and_ordered() {
        let source = include_str!("session_resume_with_handoff.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix");
        let unvalidated_start = production
            .find("impl<'a> HandoffAwareResumePlan<'a> {")
            .expect("unvalidated plan implementation");
        let validated_start = production
            .find("impl<'a> ValidatedHandoffResumePlan<'a> {")
            .expect("validated plan implementation");
        let completed_start = production
            .find("impl CompletedHandoffResume<'_> {")
            .expect("completed authority implementation");
        let error_start = production
            .find("pub enum HandoffApplySummaryError {")
            .expect("completed authority boundary");

        let unvalidated = &production[unvalidated_start..validated_start];
        let validated = &production[validated_start..completed_start];
        let completed = &production[completed_start..error_start];
        assert!(!unvalidated.contains("pub fn invoke_resume("));
        assert!(!unvalidated.contains("apply_to_resumed_session"));
        assert!(validated.contains("pub fn invoke_resume(\n        self,"));
        assert!(validated.contains("pub async fn invoke_resume_with_cx(\n        self,"));
        assert!(!validated.contains("apply_to_resumed_session"));
        assert!(completed.contains("pub fn apply_to_resumed_session(\n        self,"));

        let authority_start = production
            .find("pub struct CompletedHandoffResume<'a> {")
            .expect("completed authority declaration");
        let authority = &production[authority_start..completed_start];
        assert!(authority.contains("    validated: ValidatedHandoffResumePlan<'a>,"));
        assert!(authority.contains("    resume_output: CasrResumeOutput,"));
        assert!(!authority.contains("pub validated:"));
        assert!(!authority.contains("pub resume_output:"));
    }

    #[test]
    fn apply_to_resumed_session_records_per_section_dispositions() {
        let capsule = sample_capsule();
        let dest = dest_passport(&["inherit_mission_state", "inherit_pending_approvals"]);
        let completed = completed_resume(&capsule, Some(&dest), fake_resume_output());
        let summary = completed
            .apply_to_resumed_session(vec![
                SectionApplyOutcome {
                    section_index: 0,
                    section_label: "context_summary".into(),
                    disposition: SectionDisposition::Applied,
                },
                SectionApplyOutcome {
                    section_index: 1,
                    section_label: "mission_state".into(),
                    disposition: SectionDisposition::SkippedByPolicy {
                        reason: skip_by_policy_reasons::OPERATOR_OPTED_OUT.into(),
                    },
                },
                SectionApplyOutcome {
                    section_index: 2,
                    section_label: "pending_approvals".into(),
                    disposition: SectionDisposition::Errored {
                        error: "approval queue full".into(),
                    },
                },
            ])
            .expect("one canonical outcome per accepted section");
        assert_eq!(summary.session_id, "src-session");
        assert_eq!(summary.target_provider_slug, "claude-code");
        assert_eq!(summary.resumed_session_id, "casr-resumed-session-id");
        assert_eq!(summary.accepted_indices, vec![0, 1, 2]);
        assert_eq!(summary.skipped_count, 0);
        assert_eq!(summary.applied.len(), 3);
        // is_clean returns false because one section Errored.
        assert!(!summary.is_clean());
        let counts = summary.disposition_counts();
        assert_eq!(counts.applied, 1);
        assert_eq!(counts.skipped_by_policy, 1);
        assert_eq!(counts.errored, 1);
    }

    #[test]
    fn applied_summary_is_clean_when_no_errored() {
        let summary = AppliedHandoffSummary {
            session_id: "x".into(),
            target_provider_slug: "claude-code".into(),
            resumed_session_id: "x".into(),
            capsule_version: 1,
            accepted_indices: vec![0, 1],
            skipped_count: 0,
            applied: vec![
                SectionApplyOutcome {
                    section_index: 0,
                    section_label: "a".into(),
                    disposition: SectionDisposition::Applied,
                },
                SectionApplyOutcome {
                    section_index: 1,
                    section_label: "b".into(),
                    disposition: SectionDisposition::SkippedByPolicy {
                        reason: "redundant".into(),
                    },
                },
            ],
        };
        assert!(summary.is_clean());
    }

    #[test]
    fn apply_summary_rejects_missing_duplicate_unaccepted_and_mislabeled_outcomes() {
        let capsule = sample_capsule();
        let dest = dest_passport(&["inherit_mission_state", "inherit_pending_approvals"]);
        let completed = || completed_resume(&capsule, Some(&dest), fake_resume_output());

        let missing = completed()
            .apply_to_resumed_session(vec![
                SectionApplyOutcome {
                    section_index: 0,
                    section_label: "context_summary".into(),
                    disposition: SectionDisposition::Applied,
                },
                SectionApplyOutcome {
                    section_index: 1,
                    section_label: "mission_state".into(),
                    disposition: SectionDisposition::Applied,
                },
            ])
            .expect_err("an accepted section cannot be omitted");
        assert_eq!(
            missing,
            HandoffApplySummaryError::MissingAcceptedSectionOutcome { section_index: 2 }
        );

        let duplicate = completed()
            .apply_to_resumed_session(vec![
                SectionApplyOutcome {
                    section_index: 0,
                    section_label: "context_summary".into(),
                    disposition: SectionDisposition::Applied,
                },
                SectionApplyOutcome {
                    section_index: 0,
                    section_label: "context_summary".into(),
                    disposition: SectionDisposition::Applied,
                },
            ])
            .expect_err("duplicate accounting cannot satisfy coverage");
        assert_eq!(
            duplicate,
            HandoffApplySummaryError::DuplicateSectionOutcome { section_index: 0 }
        );

        let unaccepted = completed()
            .apply_to_resumed_session(vec![SectionApplyOutcome {
                section_index: 99,
                section_label: "context_summary".into(),
                disposition: SectionDisposition::Applied,
            }])
            .expect_err("unaccepted section cannot be recorded");
        assert_eq!(
            unaccepted,
            HandoffApplySummaryError::UnacceptedSectionOutcome { section_index: 99 }
        );

        let mislabeled = completed()
            .apply_to_resumed_session(vec![
                SectionApplyOutcome {
                    section_index: 0,
                    section_label: "mission_state".into(),
                    disposition: SectionDisposition::Applied,
                },
                SectionApplyOutcome {
                    section_index: 1,
                    section_label: "mission_state".into(),
                    disposition: SectionDisposition::Applied,
                },
                SectionApplyOutcome {
                    section_index: 2,
                    section_label: "pending_approvals".into(),
                    disposition: SectionDisposition::Applied,
                },
            ])
            .expect_err("section labels must match the capsule authority");
        assert_eq!(
            mislabeled,
            HandoffApplySummaryError::SectionLabelMismatch { section_index: 0 }
        );
    }

    #[test]
    fn apply_summary_requires_successful_non_dry_resume_with_bounded_identity() {
        let capsule = sample_capsule();
        let dest = dest_passport(&["inherit_mission_state", "inherit_pending_approvals"]);
        let outcomes = || {
            vec![
                SectionApplyOutcome {
                    section_index: 0,
                    section_label: "context_summary".into(),
                    disposition: SectionDisposition::Applied,
                },
                SectionApplyOutcome {
                    section_index: 1,
                    section_label: "mission_state".into(),
                    disposition: SectionDisposition::Applied,
                },
                SectionApplyOutcome {
                    section_index: 2,
                    section_label: "pending_approvals".into(),
                    disposition: SectionDisposition::Applied,
                },
            ]
        };

        let mut output = fake_resume_output();
        output.ok = false;
        assert_eq!(
            completed_resume(&capsule, Some(&dest), output.clone())
                .apply_to_resumed_session(outcomes()),
            Err(HandoffApplySummaryError::ResumeReportedFailure)
        );

        output.ok = true;
        output.dry_run = true;
        assert_eq!(
            completed_resume(&capsule, Some(&dest), output.clone())
                .apply_to_resumed_session(outcomes()),
            Err(HandoffApplySummaryError::ResumeWasDryRun)
        );

        output.dry_run = false;
        output.source_session_id = Some("different-source-session-canary".into());
        let source_error = completed_resume(&capsule, Some(&dest), output.clone())
            .apply_to_resumed_session(outcomes())
            .expect_err("resume output must bind to the requested source session");
        assert_eq!(
            source_error,
            HandoffApplySummaryError::SourceSessionMismatch
        );
        assert!(
            !source_error
                .to_string()
                .contains("different-source-session-canary")
        );

        output.source_session_id = Some("src-session".into());
        output.target_provider = Some("../different-provider-canary".into());
        let provider_error = completed_resume(&capsule, Some(&dest), output.clone())
            .apply_to_resumed_session(outcomes())
            .expect_err("resume output must bind to the requested target provider");
        assert_eq!(
            provider_error,
            HandoffApplySummaryError::TargetProviderMismatch
        );
        assert!(
            !provider_error
                .to_string()
                .contains("different-provider-canary")
        );

        output.target_provider = Some("claude-code".into());
        output.target_session_id = None;
        assert_eq!(
            completed_resume(&capsule, Some(&dest), output.clone())
                .apply_to_resumed_session(outcomes()),
            Err(HandoffApplySummaryError::MissingResumedSessionId)
        );

        output.target_session_id = Some("bad\nidentity".into());
        assert_eq!(
            completed_resume(&capsule, Some(&dest), output).apply_to_resumed_session(outcomes()),
            Err(HandoffApplySummaryError::InvalidResumedSessionId {
                identifier_bytes: "bad\nidentity".len(),
            })
        );
    }

    #[test]
    fn is_clean_rejects_structurally_incomplete_deserialized_summaries() {
        let summary = AppliedHandoffSummary {
            session_id: "x".into(),
            target_provider_slug: "claude-code".into(),
            resumed_session_id: "x".into(),
            capsule_version: 1,
            accepted_indices: vec![0, 1],
            skipped_count: 0,
            applied: vec![SectionApplyOutcome {
                section_index: 0,
                section_label: "context_summary".into(),
                disposition: SectionDisposition::Applied,
            }],
        };
        assert!(!summary.has_complete_outcome_coverage());
        assert!(!summary.is_clean());
    }

    #[test]
    fn apply_outcome_validator_rejects_corrupt_validation_authority() {
        let capsule = sample_capsule();
        let no_outcomes = Vec::new();
        let invalid = ValidationOutcome {
            accepted: vec![capsule.sections.len()],
            skipped: Vec::new(),
        };
        assert_eq!(
            validate_apply_outcomes(&capsule, &invalid, &no_outcomes),
            Err(HandoffApplySummaryError::InvalidAcceptedSectionIndex {
                section_index: capsule.sections.len(),
            })
        );

        let duplicate = ValidationOutcome {
            accepted: vec![0, 0],
            skipped: Vec::new(),
        };
        assert_eq!(
            validate_apply_outcomes(&capsule, &duplicate, &no_outcomes),
            Err(HandoffApplySummaryError::DuplicateAcceptedSectionIndex { section_index: 0 })
        );
    }

    #[test]
    fn section_kind_compatible_with_destination_passport_excerpt_requires_pane() {
        let excerpt = CapsuleSection::PassportExcerpt {
            passport: CapabilityPassport {
                agent_id: "x".into(),
                pane_id: None,
                capabilities: vec![],
                generation: 0,
                signed_at_ms: 0,
            },
        };
        assert!(section_kind_compatible_with_destination(
            &excerpt,
            LifecycleEntityKind::Pane
        ));
        assert!(!section_kind_compatible_with_destination(
            &excerpt,
            LifecycleEntityKind::Session
        ));
        assert!(!section_kind_compatible_with_destination(
            &excerpt,
            LifecycleEntityKind::Window
        ));
    }

    #[test]
    fn section_kind_compatible_unguarded_kinds_apply_anywhere() {
        let context = CapsuleSection::ContextSummary { text: "x".into() };
        for kind in [
            LifecycleEntityKind::Pane,
            LifecycleEntityKind::Session,
            LifecycleEntityKind::Window,
        ] {
            assert!(section_kind_compatible_with_destination(&context, kind));
        }
    }

    #[test]
    fn applied_summary_serde_roundtrip() {
        let summary = AppliedHandoffSummary {
            session_id: "src".into(),
            target_provider_slug: "claude-code".into(),
            resumed_session_id: "dst".into(),
            capsule_version: 1,
            accepted_indices: vec![0, 1, 2],
            skipped_count: 1,
            applied: vec![SectionApplyOutcome {
                section_index: 0,
                section_label: "context_summary".into(),
                disposition: SectionDisposition::Applied,
            }],
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: AppliedHandoffSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back, summary);
    }
}
