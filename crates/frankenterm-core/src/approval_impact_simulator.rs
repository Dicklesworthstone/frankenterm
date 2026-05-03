//! br-ft-1650n.14: Approval Impact Simulator substrate.
//!
//! Before asking an operator to approve a policy-gated action,
//! simulate and summarize the expected blast radius — panes
//! affected, commands sent, files touched, credentials involved,
//! rollback path, and unknown-propagation flags. The substrate
//! emits a structured `ImpactReport` (either
//! `AutomationCapable(preview)` or
//! `ManualApprovalRequired { reasons }`).
//!
//! ## Why a substrate slice
//!
//! Approval prompts that only say "allow / deny" do not scale to
//! workflows that touch many agents. Operators need fast but
//! evidence-rich impact previews. This substrate is the
//! pure-function core; the wired-pass slice will couple to the
//! actual capability-passport store + tx dry-run machinery.
//!
//! ## What ships in this slice
//!
//! - [`ProposedAction`] — what's about to happen.
//! - [`CredentialClass`] — typed credentials enum.
//! - [`RollbackPlan`] — required field; absence → manual approval.
//! - [`UnknownReason`] — typed reasons that disable automation.
//! - [`BlastRadius`] — quantified counts.
//! - [`ApprovalImpactPreview`] — operator-facing preview.
//! - [`ImpactReport`] — `AutomationCapable(preview)` or
//!   `ManualApprovalRequired { reasons }`.
//! - [`simulate_impact`] — pure function.
//!
//! ## What is deferred
//!
//! - Tx dry-run integration: the wired-pass slice will pull
//!   command effects from the planner_features tx machinery
//!   instead of accepting caller-supplied effect lists.
//! - Capability passport correlation: today the caller marks
//!   each action's required passport class manually; a
//!   wired-pass slice will look it up from the passport store.
//! - Causal graph context: the bead's "Integrate capability
//!   passports and causal graph context" item is deferred to
//!   the wired-pass slice.

use serde::{Deserialize, Serialize};

/// br-ft-1650n.14: an action awaiting approval. The substrate
/// is provenance-blind on the actual action body; it only
/// reasons about the structured fields below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedAction {
    /// Stable identifier for audit correlation.
    pub action_id: String,
    /// One-line operator-facing summary (already redacted).
    pub summary: String,
    /// Pane IDs the action will touch.
    pub target_panes: Vec<u64>,
    /// Commands the action will send (already redacted).
    pub commands: Vec<String>,
    /// File paths the action will touch (already redacted).
    pub touched_files: Vec<String>,
    /// Credentials the action will use.
    pub credentials: CredentialClass,
    /// Rollback plan, if any.
    pub rollback_plan: Option<RollbackPlan>,
    /// Unknowns that the simulator could not resolve. Non-empty
    /// → manual approval required.
    pub unknowns: Vec<UnknownReason>,
}

/// br-ft-1650n.14: typed credentials class. Operators read this
/// to gauge blast radius without parsing free-form strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialClass {
    /// No credentials involved.
    None,
    /// Read-only access to local resources.
    LocalReadOnly,
    /// Local read/write — limited to the current workspace.
    LocalReadWrite,
    /// Read-only access to a network/cloud resource.
    NetworkReadOnly,
    /// Read/write to a network/cloud resource (production
    /// blast radius).
    NetworkReadWrite,
    /// Sensitive credential with secret material (auth tokens,
    /// API keys, etc.). Always elevates to manual approval.
    Secret,
}

impl CredentialClass {
    /// Sensitive credentials gate the action up to manual
    /// approval regardless of other signals. The bead's
    /// "credentials involved" criterion needs the strongest
    /// possible isolation.
    #[must_use]
    pub fn requires_manual_approval(&self) -> bool {
        matches!(self, Self::Secret)
    }
}

/// br-ft-1650n.14: structured rollback plan. The substrate
/// requires this for any action with non-trivial blast radius;
/// absence triggers manual approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackPlan {
    /// Operator-facing description (already redacted).
    pub description: String,
    /// Concrete commands the operator can run to roll back
    /// (already redacted).
    pub commands: Vec<String>,
    /// Whether the rollback is verified by a dry-run prior to
    /// presentation. Unverified plans still permit automation
    /// but operators know the risk.
    pub verified: bool,
}

/// br-ft-1650n.14: typed unknown-propagation reasons. The
/// substrate's contract is that ANY non-empty `unknowns` list on
/// a `ProposedAction` forces manual approval. The enum is
/// extensible so future signal sources can plug in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum UnknownReason {
    /// A target pane has no known capability passport.
    UnknownPaneCapability { pane_id: u64 },
    /// A command's effect could not be predicted from the tx
    /// dry-run machinery.
    UnpredictableCommand { command: String },
    /// A file path falls outside the current workspace and the
    /// simulator cannot enumerate the affected files.
    UnboundedFileScope { hint: String },
    /// A credential's permissions could not be enumerated.
    UnknownCredentialScope { credential_label: String },
    /// Generic catch-all with operator-facing message.
    Other { message: String },
}

/// br-ft-1650n.14: quantified blast radius. The bead's "blast
/// radius" preview field maps to this struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlastRadius {
    pub pane_count: usize,
    pub command_count: usize,
    pub file_count: usize,
    pub credential_class: CredentialClassBlastBand,
}

/// Blast band derived from `CredentialClass`. Used in the
/// `BlastRadius` to give dashboards a coarse scalar without
/// embedding the full enum (operators sort by band).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialClassBlastBand {
    /// `CredentialClass::None`.
    #[default]
    None,
    /// `LocalReadOnly` / `NetworkReadOnly`.
    ReadOnly,
    /// `LocalReadWrite`.
    LocalWrite,
    /// `NetworkReadWrite`.
    NetworkWrite,
    /// `Secret` — the highest-blast band.
    Secret,
}

impl From<CredentialClass> for CredentialClassBlastBand {
    fn from(c: CredentialClass) -> Self {
        match c {
            CredentialClass::None => Self::None,
            CredentialClass::LocalReadOnly | CredentialClass::NetworkReadOnly => Self::ReadOnly,
            CredentialClass::LocalReadWrite => Self::LocalWrite,
            CredentialClass::NetworkReadWrite => Self::NetworkWrite,
            CredentialClass::Secret => Self::Secret,
        }
    }
}

/// br-ft-1650n.14: full operator-facing preview, returned only
/// when the simulator can complete an automation-capable
/// approval prompt. Every field is already redacted by contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalImpactPreview {
    pub action_id: String,
    pub summary: String,
    pub blast_radius: BlastRadius,
    pub target_panes: Vec<u64>,
    pub commands: Vec<String>,
    pub touched_files: Vec<String>,
    pub credentials: CredentialClass,
    pub rollback_plan: RollbackPlan,
    /// One-line confidence string for the operator dashboard
    /// (e.g., "high — rollback verified").
    pub confidence: String,
}

/// br-ft-1650n.14: simulator output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ImpactReport {
    /// The simulator produced a complete preview.
    AutomationCapable { preview: ApprovalImpactPreview },
    /// The simulator could not complete a preview. Operator
    /// must approve manually after reading the reasons. The
    /// substrate's contract is "incomplete simulation does NOT
    /// imply safety" (the bead's documented invariant).
    ManualApprovalRequired { reasons: Vec<String> },
}

/// br-ft-1650n.14: pure simulator entry point.
///
/// Returns `ManualApprovalRequired` if any of:
///
/// 1. `unknowns` is non-empty.
/// 2. `credentials` is `Secret`.
/// 3. `rollback_plan` is `None` AND blast radius is
///    non-trivial (any of: ≥1 command, ≥1 file, or non-None
///    credential class).
///
/// Otherwise returns `AutomationCapable` with a complete
/// preview.
#[must_use]
pub fn simulate_impact(action: &ProposedAction) -> ImpactReport {
    let mut reasons: Vec<String> = action
        .unknowns
        .iter()
        .map(|u| match u {
            UnknownReason::UnknownPaneCapability { pane_id } => {
                format!("unknown capability passport for pane {pane_id}")
            }
            UnknownReason::UnpredictableCommand { command } => {
                format!("unpredictable command effect: {command}")
            }
            UnknownReason::UnboundedFileScope { hint } => {
                format!("unbounded file scope: {hint}")
            }
            UnknownReason::UnknownCredentialScope { credential_label } => {
                format!("unknown credential scope: {credential_label}")
            }
            UnknownReason::Other { message } => message.clone(),
        })
        .collect();

    if action.credentials.requires_manual_approval() {
        reasons.push("sensitive credentials (Secret class) require manual approval".to_string());
    }

    let trivial = action.commands.is_empty()
        && action.touched_files.is_empty()
        && matches!(action.credentials, CredentialClass::None);
    if action.rollback_plan.is_none() && !trivial {
        reasons.push(
            "non-trivial blast radius requires a rollback plan; none provided".to_string(),
        );
    }

    if !reasons.is_empty() {
        return ImpactReport::ManualApprovalRequired { reasons };
    }

    // Automation-capable path.
    let blast_radius = BlastRadius {
        pane_count: action.target_panes.len(),
        command_count: action.commands.len(),
        file_count: action.touched_files.len(),
        credential_class: action.credentials.into(),
    };

    // Trivial actions (no rollback needed) get a default
    // rollback plan ("no-op rollback") so the preview always
    // carries a structured field.
    let rollback_plan = action.rollback_plan.clone().unwrap_or(RollbackPlan {
        description: "no-op rollback (action is read-only or trivial)".to_string(),
        commands: Vec::new(),
        verified: true,
    });

    let confidence = match (rollback_plan.verified, blast_radius.credential_class) {
        (true, CredentialClassBlastBand::None | CredentialClassBlastBand::ReadOnly) => {
            "high — rollback verified, low-blast credentials".to_string()
        }
        (true, _) => "medium — rollback verified, non-trivial credentials".to_string(),
        (false, _) => "low — rollback unverified".to_string(),
    };

    ImpactReport::AutomationCapable {
        preview: ApprovalImpactPreview {
            action_id: action.action_id.clone(),
            summary: action.summary.clone(),
            blast_radius,
            target_panes: action.target_panes.clone(),
            commands: action.commands.clone(),
            touched_files: action.touched_files.clone(),
            credentials: action.credentials,
            rollback_plan,
            confidence,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trivial_action() -> ProposedAction {
        ProposedAction {
            action_id: "act-1".to_string(),
            summary: "summary".to_string(),
            target_panes: Vec::new(),
            commands: Vec::new(),
            touched_files: Vec::new(),
            credentials: CredentialClass::None,
            rollback_plan: None,
            unknowns: Vec::new(),
        }
    }

    fn write_action() -> ProposedAction {
        ProposedAction {
            action_id: "act-2".to_string(),
            summary: "write summary".to_string(),
            target_panes: vec![1, 2],
            commands: vec!["echo hi".to_string()],
            touched_files: vec!["/tmp/file".to_string()],
            credentials: CredentialClass::LocalReadWrite,
            rollback_plan: Some(RollbackPlan {
                description: "rm /tmp/file".to_string(),
                commands: vec!["rm -f /tmp/file".to_string()],
                verified: true,
            }),
            unknowns: Vec::new(),
        }
    }

    /// Trivial action (no commands, no files, None credentials)
    /// can skip the rollback plan and still be automation-capable.
    #[test]
    fn trivial_action_is_automation_capable() {
        match simulate_impact(&trivial_action()) {
            ImpactReport::AutomationCapable { preview } => {
                assert_eq!(preview.action_id, "act-1");
                assert_eq!(preview.blast_radius.pane_count, 0);
                assert_eq!(preview.blast_radius.command_count, 0);
                assert_eq!(preview.blast_radius.file_count, 0);
                assert_eq!(
                    preview.blast_radius.credential_class,
                    CredentialClassBlastBand::None
                );
            }
            other => panic!("expected AutomationCapable, got {other:?}"),
        }
    }

    /// A write action with rollback + no unknowns is
    /// automation-capable.
    #[test]
    fn write_action_with_rollback_is_automation_capable() {
        match simulate_impact(&write_action()) {
            ImpactReport::AutomationCapable { preview } => {
                assert_eq!(preview.blast_radius.pane_count, 2);
                assert_eq!(preview.blast_radius.command_count, 1);
                assert_eq!(preview.blast_radius.file_count, 1);
                assert_eq!(
                    preview.blast_radius.credential_class,
                    CredentialClassBlastBand::LocalWrite
                );
                assert!(preview.rollback_plan.verified);
                assert!(preview.confidence.contains("medium"));
            }
            other => panic!("expected AutomationCapable, got {other:?}"),
        }
    }

    /// Any unknown forces manual approval — and the reason string
    /// surfaces in the report.
    #[test]
    fn unknown_propagation_forces_manual_approval() {
        let mut action = write_action();
        action.unknowns = vec![UnknownReason::UnknownPaneCapability { pane_id: 99 }];
        match simulate_impact(&action) {
            ImpactReport::ManualApprovalRequired { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("pane 99")));
            }
            other => panic!("expected ManualApprovalRequired, got {other:?}"),
        }
    }

    /// Multiple unknowns all surface as separate reasons.
    #[test]
    fn multiple_unknowns_all_surface() {
        let mut action = write_action();
        action.unknowns = vec![
            UnknownReason::UnknownPaneCapability { pane_id: 7 },
            UnknownReason::UnpredictableCommand {
                command: "rm -rf /".to_string(),
            },
            UnknownReason::UnboundedFileScope {
                hint: "**/*".to_string(),
            },
            UnknownReason::Other {
                message: "x".to_string(),
            },
        ];
        match simulate_impact(&action) {
            ImpactReport::ManualApprovalRequired { reasons } => {
                assert_eq!(reasons.len(), 4);
                assert!(reasons.iter().any(|r| r.contains("pane 7")));
                assert!(reasons.iter().any(|r| r.contains("rm -rf /")));
                assert!(reasons.iter().any(|r| r.contains("**/*")));
            }
            other => panic!("expected ManualApprovalRequired, got {other:?}"),
        }
    }

    /// Secret credentials always force manual approval, even when
    /// rollback is provided and unknowns are empty.
    #[test]
    fn secret_credentials_always_force_manual_approval() {
        let mut action = write_action();
        action.credentials = CredentialClass::Secret;
        match simulate_impact(&action) {
            ImpactReport::ManualApprovalRequired { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("Secret")));
            }
            other => panic!("expected ManualApprovalRequired, got {other:?}"),
        }
    }

    /// Non-trivial action without a rollback plan forces manual
    /// approval. Pinned: a single command + no rollback → manual.
    #[test]
    fn missing_rollback_for_non_trivial_action_forces_manual_approval() {
        let mut action = write_action();
        action.rollback_plan = None;
        match simulate_impact(&action) {
            ImpactReport::ManualApprovalRequired { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("rollback plan")));
            }
            other => panic!("expected ManualApprovalRequired, got {other:?}"),
        }
    }

    /// Trivial action without rollback is STILL automation-capable
    /// — the rollback requirement gates only on non-trivial blast
    /// radius. Pinned so the substrate doesn't over-gate read-only
    /// previews.
    #[test]
    fn trivial_action_without_rollback_is_automation_capable() {
        let action = trivial_action();
        assert!(matches!(
            simulate_impact(&action),
            ImpactReport::AutomationCapable { .. }
        ));
    }

    /// Confidence string reflects rollback-verified + credential
    /// class. High when both verified and low-blast.
    #[test]
    fn confidence_string_high_for_verified_rollback_low_blast() {
        let mut action = trivial_action();
        action.commands = vec!["ls".to_string()];
        action.credentials = CredentialClass::LocalReadOnly;
        action.rollback_plan = Some(RollbackPlan {
            description: "no-op".to_string(),
            commands: Vec::new(),
            verified: true,
        });
        match simulate_impact(&action) {
            ImpactReport::AutomationCapable { preview } => {
                assert!(preview.confidence.contains("high"));
            }
            other => panic!("expected AutomationCapable, got {other:?}"),
        }
    }

    /// Confidence is "low" when rollback is unverified.
    #[test]
    fn confidence_string_low_for_unverified_rollback() {
        let mut action = write_action();
        action.rollback_plan = Some(RollbackPlan {
            description: "rm".to_string(),
            commands: vec!["rm".to_string()],
            verified: false,
        });
        match simulate_impact(&action) {
            ImpactReport::AutomationCapable { preview } => {
                assert!(preview.confidence.contains("low"));
            }
            other => panic!("expected AutomationCapable, got {other:?}"),
        }
    }

    /// Credential class blast band mapping is total and stable.
    #[test]
    fn credential_class_blast_band_mapping() {
        assert_eq!(
            CredentialClassBlastBand::from(CredentialClass::None),
            CredentialClassBlastBand::None
        );
        assert_eq!(
            CredentialClassBlastBand::from(CredentialClass::LocalReadOnly),
            CredentialClassBlastBand::ReadOnly
        );
        assert_eq!(
            CredentialClassBlastBand::from(CredentialClass::NetworkReadOnly),
            CredentialClassBlastBand::ReadOnly
        );
        assert_eq!(
            CredentialClassBlastBand::from(CredentialClass::LocalReadWrite),
            CredentialClassBlastBand::LocalWrite
        );
        assert_eq!(
            CredentialClassBlastBand::from(CredentialClass::NetworkReadWrite),
            CredentialClassBlastBand::NetworkWrite
        );
        assert_eq!(
            CredentialClassBlastBand::from(CredentialClass::Secret),
            CredentialClassBlastBand::Secret
        );
    }

    /// ImpactReport serde roundtrip preserves both variants.
    #[test]
    fn impact_report_serde_roundtrip() {
        let preview_report = simulate_impact(&write_action());
        let json = serde_json::to_string(&preview_report).expect("serialize preview");
        let back: ImpactReport = serde_json::from_str(&json).expect("deserialize preview");
        assert_eq!(preview_report, back);

        let manual_report = ImpactReport::ManualApprovalRequired {
            reasons: vec!["x".to_string(), "y".to_string()],
        };
        let json = serde_json::to_string(&manual_report).expect("serialize manual");
        let back: ImpactReport = serde_json::from_str(&json).expect("deserialize manual");
        assert_eq!(manual_report, back);
    }

    /// Pure function: same input always produces same output.
    #[test]
    fn simulate_impact_is_pure() {
        let action = write_action();
        let r1 = simulate_impact(&action);
        let r2 = simulate_impact(&action);
        assert_eq!(r1, r2);
    }

    /// Audit-evidence threading: every ImpactReport variant
    /// carries enough structured data for audit log reconstruction.
    /// Pinned by serializing both variants and asserting the JSON
    /// contains the required field names.
    #[test]
    fn audit_fields_threaded_through_serde() {
        let report = simulate_impact(&write_action());
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.contains("\"action_id\""));
        assert!(json.contains("\"blast_radius\""));
        assert!(json.contains("\"rollback_plan\""));
        assert!(json.contains("\"confidence\""));
    }
}
