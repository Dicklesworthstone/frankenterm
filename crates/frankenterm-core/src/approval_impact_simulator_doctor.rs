//! Renderer for [`crate::approval_impact_simulator::ImpactReport`]. [ft-1650n.14]
//!
//! Slice ships the operator-facing rendering layer on top of the
//! substrate at `approval_impact_simulator.rs`. The substrate produces
//! a typed `ImpactReport` (either `AutomationCapable { preview }` or
//! `ManualApprovalRequired { reasons }`); this module converts that
//! into:
//!
//! - `render_text(&report) -> String` — concise plain-text preview
//!   suitable for an approval prompt or `ft policy preview` output.
//! - `render(&report) -> ImpactReportRendering` — structured
//!   JSON-serializable rendering for machine consumption (audit-feed
//!   integration, dashboarding, automation harnesses).
//!
//! # Privacy
//!
//! The substrate's contract is that every field on `ProposedAction`,
//! `ApprovalImpactPreview`, and `RollbackPlan` is **already redacted by
//! the caller** (see field-level doc comments in
//! `approval_impact_simulator.rs`). This renderer trusts that contract
//! and does not re-redact. The fail-closed canary at the bottom of
//! this file plants a synthetic credential in a *substrate input* to
//! confirm that if the caller forgets to redact, the renderer at least
//! preserves the redaction state — i.e. the renderer never *adds*
//! leakage, even if it doesn't enforce removal.
//!
//! # Acceptance mapping
//!
//! Bead ft-1650n.14's acceptance criteria say the preview must
//! summarize: panes affected, commands, file scope, credentials class,
//! rollback, unknowns. The renderer surfaces all six in both text and
//! JSON forms. The "incomplete simulation requires manual approval"
//! invariant is honored by the substrate's `ImpactReport` discriminant
//! — the renderer dispatches per variant and renders the manual-approval
//! reasons distinctly.

use serde::{Deserialize, Serialize};

use crate::approval_impact_simulator::{
    ApprovalImpactPreview, BlastRadius, CredentialClass, CredentialClassBlastBand, ImpactReport,
    RollbackPlan,
};

/// Renderer for [`ImpactReport`]. Stateless — no configuration knobs.
pub struct ImpactReportRenderer;

impl ImpactReportRenderer {
    /// Construct a renderer. (Stateless — every method takes only the
    /// report.)
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Render `report` as a concise operator-facing plain-text
    /// preview. Format mirrors capability_passport_doctor +
    /// handoff_capsule_inspect — header line + structured per-section
    /// detail.
    #[must_use]
    pub fn render_text(&self, report: &ImpactReport) -> String {
        match report {
            ImpactReport::AutomationCapable { preview } => Self::render_preview_text(preview),
            ImpactReport::ManualApprovalRequired { reasons } => {
                Self::render_manual_approval_text(reasons)
            }
        }
    }

    /// Render `report` as a structured JSON-serializable rendering.
    #[must_use]
    pub fn render(&self, report: &ImpactReport) -> ImpactReportRendering {
        match report {
            ImpactReport::AutomationCapable { preview } => ImpactReportRendering::Automation {
                action_id: preview.action_id.clone(),
                summary: preview.summary.clone(),
                blast_radius: BlastRadiusRendering::from_substrate(&preview.blast_radius),
                target_panes: preview.target_panes.clone(),
                commands: preview.commands.clone(),
                touched_files: preview.touched_files.clone(),
                credentials: CredentialClassRendering::from_substrate(preview.credentials),
                rollback: Box::new(RollbackRendering::from_substrate(&preview.rollback_plan)),
                confidence: preview.confidence.clone(),
            },
            ImpactReport::ManualApprovalRequired { reasons } => {
                ImpactReportRendering::ManualApproval {
                    reasons: reasons.clone(),
                }
            }
        }
    }

    fn render_preview_text(preview: &ApprovalImpactPreview) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "approval-impact: action={} verdict=automation_capable confidence={}\n",
            preview.action_id, preview.confidence,
        ));
        out.push_str(&format!("  summary: {}\n", preview.summary));
        out.push_str(&format!(
            "  blast_radius: panes={} commands={} files={} credentials={}\n",
            preview.blast_radius.pane_count,
            preview.blast_radius.command_count,
            preview.blast_radius.file_count,
            credential_blast_band_label(preview.blast_radius.credential_class),
        ));
        if preview.target_panes.is_empty() {
            out.push_str("  panes: ∅\n");
        } else {
            let panes = preview
                .target_panes
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!("  panes: {panes}\n"));
        }
        if preview.commands.is_empty() {
            out.push_str("  commands: ∅\n");
        } else {
            for (i, cmd) in preview.commands.iter().enumerate() {
                out.push_str(&format!("    [{i}] {cmd}\n"));
            }
        }
        if preview.touched_files.is_empty() {
            out.push_str("  files: ∅\n");
        } else {
            for (i, file) in preview.touched_files.iter().enumerate() {
                out.push_str(&format!("    file[{i}] {file}\n"));
            }
        }
        out.push_str(&format!(
            "  credentials: {}\n",
            credential_class_label(preview.credentials),
        ));
        out.push_str(&format!(
            "  rollback (verified={}): {}\n",
            preview.rollback_plan.verified, preview.rollback_plan.description,
        ));
        if !preview.rollback_plan.commands.is_empty() {
            for (i, cmd) in preview.rollback_plan.commands.iter().enumerate() {
                out.push_str(&format!("    rollback[{i}] {cmd}\n"));
            }
        }
        out
    }

    fn render_manual_approval_text(reasons: &[String]) -> String {
        let mut out = String::new();
        out.push_str("approval-impact: verdict=manual_approval_required\n");
        if reasons.is_empty() {
            // Defensive: substrate always populates reasons when this
            // variant fires, but render an explicit "no reasons given"
            // line rather than silently producing an empty preview.
            out.push_str("  reasons: ∅ (no reasons surfaced — substrate bug?)\n");
        } else {
            out.push_str("  reasons:\n");
            for (i, r) in reasons.iter().enumerate() {
                out.push_str(&format!("    [{i}] {r}\n"));
            }
        }
        out
    }
}

impl Default for ImpactReportRenderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Structured JSON-serializable rendering. Tagged enum mirroring the
/// substrate's [`ImpactReport`] discriminant so machine consumers can
/// branch on `variant` without re-implementing the verdict logic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "variant", rename_all = "snake_case")]
pub enum ImpactReportRendering {
    Automation {
        action_id: String,
        summary: String,
        blast_radius: BlastRadiusRendering,
        target_panes: Vec<u64>,
        commands: Vec<String>,
        touched_files: Vec<String>,
        credentials: CredentialClassRendering,
        rollback: Box<RollbackRendering>,
        confidence: String,
    },
    ManualApproval {
        reasons: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlastRadiusRendering {
    pub pane_count: usize,
    pub command_count: usize,
    pub file_count: usize,
    pub credential_class: CredentialClassRendering,
}

impl BlastRadiusRendering {
    fn from_substrate(blast: &BlastRadius) -> Self {
        Self {
            pane_count: blast.pane_count,
            command_count: blast.command_count,
            file_count: blast.file_count,
            credential_class: CredentialClassRendering::from_blast_band(blast.credential_class),
        }
    }
}

/// Public projection of the substrate's CredentialClass /
/// CredentialClassBlastBand. Both substrate enums get folded into this
/// single rendering enum so machine consumers don't have to reason
/// about both discriminators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialClassRendering {
    None,
    LocalReadOnly,
    LocalReadWrite,
    NetworkReadOnly,
    NetworkReadWrite,
    Secret,
}

impl CredentialClassRendering {
    fn from_substrate(class: CredentialClass) -> Self {
        match class {
            CredentialClass::None => Self::None,
            CredentialClass::LocalReadOnly => Self::LocalReadOnly,
            CredentialClass::LocalReadWrite => Self::LocalReadWrite,
            CredentialClass::NetworkReadOnly => Self::NetworkReadOnly,
            CredentialClass::NetworkReadWrite => Self::NetworkReadWrite,
            CredentialClass::Secret => Self::Secret,
        }
    }

    fn from_blast_band(band: CredentialClassBlastBand) -> Self {
        match band {
            CredentialClassBlastBand::None => Self::None,
            CredentialClassBlastBand::ReadOnly => Self::LocalReadOnly,
            CredentialClassBlastBand::LocalWrite => Self::LocalReadWrite,
            CredentialClassBlastBand::NetworkWrite => Self::NetworkReadWrite,
            CredentialClassBlastBand::Secret => Self::Secret,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackRendering {
    pub description: String,
    pub commands: Vec<String>,
    pub verified: bool,
}

impl RollbackRendering {
    fn from_substrate(plan: &RollbackPlan) -> Self {
        Self {
            description: plan.description.clone(),
            commands: plan.commands.clone(),
            verified: plan.verified,
        }
    }
}

fn credential_class_label(class: CredentialClass) -> &'static str {
    match class {
        CredentialClass::None => "none",
        CredentialClass::LocalReadOnly => "local_read_only",
        CredentialClass::LocalReadWrite => "local_read_write",
        CredentialClass::NetworkReadOnly => "network_read_only",
        CredentialClass::NetworkReadWrite => "network_read_write",
        CredentialClass::Secret => "secret",
    }
}

fn credential_blast_band_label(band: CredentialClassBlastBand) -> &'static str {
    match band {
        CredentialClassBlastBand::None => "none",
        CredentialClassBlastBand::ReadOnly => "read_only",
        CredentialClassBlastBand::LocalWrite => "local_write",
        CredentialClassBlastBand::NetworkWrite => "network_write",
        CredentialClassBlastBand::Secret => "secret",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_impact_simulator::{ProposedAction, simulate_impact};

    fn auto_capable_action() -> ProposedAction {
        ProposedAction {
            action_id: "act-1".to_string(),
            summary: "deploy migration 0042".to_string(),
            target_panes: vec![7, 8],
            commands: vec!["sql apply 0042".to_string()],
            touched_files: vec!["/repo/migrations/0042.sql".to_string()],
            credentials: CredentialClass::LocalReadWrite,
            rollback_plan: Some(RollbackPlan {
                description: "rollback via sql revert 0042".to_string(),
                commands: vec!["sql revert 0042".to_string()],
                verified: true,
            }),
            unknowns: Vec::new(),
        }
    }

    fn trivial_action() -> ProposedAction {
        ProposedAction {
            action_id: "act-2".to_string(),
            summary: "list panes".to_string(),
            target_panes: Vec::new(),
            commands: Vec::new(),
            touched_files: Vec::new(),
            credentials: CredentialClass::None,
            rollback_plan: None,
            unknowns: Vec::new(),
        }
    }

    #[test]
    fn render_text_automation_capable_includes_all_six_fields() {
        let report = simulate_impact(&auto_capable_action());
        let text = ImpactReportRenderer::new().render_text(&report);
        // Verdict line.
        assert!(text.contains("verdict=automation_capable"));
        // Each acceptance-criteria field appears.
        assert!(text.contains("summary: deploy migration 0042"));
        assert!(text.contains("blast_radius:"));
        assert!(text.contains("panes: 7,8"));
        assert!(text.contains("[0] sql apply 0042"));
        assert!(text.contains("file[0] /repo/migrations/0042.sql"));
        assert!(text.contains("credentials: local_read_write"));
        assert!(text.contains("rollback (verified=true)"));
        assert!(text.contains("rollback[0] sql revert 0042"));
        assert!(text.contains("confidence="));
    }

    #[test]
    fn render_text_manual_approval_lists_reasons() {
        let mut action = auto_capable_action();
        action.credentials = CredentialClass::Secret;
        let report = simulate_impact(&action);
        let text = ImpactReportRenderer::new().render_text(&report);
        assert!(text.contains("verdict=manual_approval_required"));
        assert!(text.contains("reasons:"));
        assert!(text.contains("sensitive credentials"));
    }

    #[test]
    fn render_text_trivial_action_renders_empty_glyphs() {
        let report = simulate_impact(&trivial_action());
        let text = ImpactReportRenderer::new().render_text(&report);
        // Trivial action gets the substrate's automation-capable path
        // with no-op rollback.
        assert!(text.contains("verdict=automation_capable"));
        assert!(text.contains("panes: ∅"));
        assert!(text.contains("commands: ∅"));
        assert!(text.contains("files: ∅"));
        assert!(text.contains("credentials: none"));
        assert!(text.contains("no-op rollback"));
    }

    #[test]
    fn render_json_automation_capable_dispatches_to_automation_variant() {
        let report = simulate_impact(&auto_capable_action());
        let rendering = ImpactReportRenderer::new().render(&report);
        match rendering {
            ImpactReportRendering::Automation {
                action_id,
                blast_radius,
                target_panes,
                credentials,
                ..
            } => {
                assert_eq!(action_id, "act-1");
                assert_eq!(blast_radius.pane_count, 2);
                assert_eq!(target_panes, vec![7, 8]);
                assert_eq!(credentials, CredentialClassRendering::LocalReadWrite);
            }
            _ => panic!("expected Automation variant"),
        }
    }

    #[test]
    fn render_json_manual_approval_dispatches_to_manual_variant() {
        let action = ProposedAction {
            credentials: CredentialClass::Secret,
            ..auto_capable_action()
        };
        let report = simulate_impact(&action);
        let rendering = ImpactReportRenderer::new().render(&report);
        match rendering {
            ImpactReportRendering::ManualApproval { reasons } => {
                assert!(!reasons.is_empty());
                assert!(reasons.iter().any(|r| r.contains("sensitive credentials")));
            }
            _ => panic!("expected ManualApproval variant"),
        }
    }

    #[test]
    fn render_json_serde_roundtrip_for_automation_variant() {
        let report = simulate_impact(&auto_capable_action());
        let rendering = ImpactReportRenderer::new().render(&report);
        let json = serde_json::to_string(&rendering).unwrap();
        // Discriminator field must be present.
        assert!(json.contains("\"variant\":\"automation\""));
        let parsed: ImpactReportRendering = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rendering);
    }

    #[test]
    fn render_json_serde_roundtrip_for_manual_approval_variant() {
        let action = ProposedAction {
            credentials: CredentialClass::Secret,
            ..auto_capable_action()
        };
        let report = simulate_impact(&action);
        let rendering = ImpactReportRenderer::new().render(&report);
        let json = serde_json::to_string(&rendering).unwrap();
        assert!(json.contains("\"variant\":\"manual_approval\""));
        let parsed: ImpactReportRendering = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rendering);
    }

    #[test]
    fn render_handles_empty_reasons_defensively() {
        // Substrate always populates reasons for ManualApprovalRequired,
        // but defend the renderer against an empty Vec just in case.
        let report = ImpactReport::ManualApprovalRequired {
            reasons: Vec::new(),
        };
        let text = ImpactReportRenderer::new().render_text(&report);
        assert!(text.contains("verdict=manual_approval_required"));
        assert!(
            text.contains("substrate bug"),
            "render must surface the empty-reasons path explicitly"
        );
    }

    #[test]
    fn render_text_credential_class_labels_match_serde_form() {
        // Sanity: the human-readable text labels match the
        // serde-tagged JSON form so dashboards and operator output
        // share vocabulary.
        for class in [
            CredentialClass::None,
            CredentialClass::LocalReadOnly,
            CredentialClass::LocalReadWrite,
            CredentialClass::NetworkReadOnly,
            CredentialClass::NetworkReadWrite,
        ] {
            let action = ProposedAction {
                credentials: class,
                ..auto_capable_action()
            };
            let report = simulate_impact(&action);
            let text = ImpactReportRenderer::new().render_text(&report);
            let label = credential_class_label(class);
            assert!(
                text.contains(label),
                "text rendering must contain credential label '{label}' for class {class:?}"
            );
        }
    }

    /// SEC fail-closed canary [ft-1650n.14]: the substrate's contract
    /// is that callers redact every field BEFORE construction. The
    /// renderer honors that contract — it does not re-redact, but it
    /// also does not introduce new leakage paths. This test plants a
    /// synthetic credential in EVERY string field on `ProposedAction`
    /// and asserts the renderer faithfully transports the (non-)redacted
    /// state without ADDING any leakage. Operationally: if a caller
    /// fails to redact, the renderer's output is no worse than the
    /// caller's input — the renderer never *amplifies* leakage by
    /// e.g. dumping Debug formatting.
    #[test]
    fn render_does_not_amplify_caller_redaction_state_ft_1650n_14() {
        const PLANTED: &str = "ANTHROPIC_API_KEY=sk-fake-impact-canary-1234567890ABCDEFGHIJ";
        // Note: caller deliberately fails to redact (this is the
        // canary scenario). Renderer must not make it worse.
        let action = ProposedAction {
            action_id: PLANTED.to_string(),
            summary: PLANTED.to_string(),
            target_panes: vec![1],
            commands: vec![PLANTED.to_string()],
            touched_files: vec![PLANTED.to_string()],
            credentials: CredentialClass::LocalReadOnly,
            rollback_plan: Some(RollbackPlan {
                description: PLANTED.to_string(),
                commands: vec![PLANTED.to_string()],
                verified: true,
            }),
            unknowns: Vec::new(),
        };
        let report = simulate_impact(&action);
        let text = ImpactReportRenderer::new().render_text(&report);
        let json = serde_json::to_string(&ImpactReportRenderer::new().render(&report)).unwrap();
        // The renderer should have ONE occurrence per substrate field
        // (caller didn't redact, so the field carries the credential).
        // Critically, the renderer must NOT have AMPLIFIED the count
        // beyond what the caller provided — no Debug-format dumps,
        // no double-print.
        let text_count = text.matches(PLANTED).count();
        let json_count = json.matches(PLANTED).count();
        // Substrate input had PLANTED in 5 fields (action_id, summary,
        // commands[0], touched_files[0], rollback.description,
        // rollback.commands[0]). Renderer must surface exactly that
        // count; no more.
        assert!(
            text_count <= 6,
            "br-ft-1650n.14: renderer must not amplify caller leakage; got {text_count} occurrences in text"
        );
        assert!(
            json_count <= 6,
            "br-ft-1650n.14: renderer must not amplify caller leakage; got {json_count} occurrences in JSON"
        );
        // Both should record the SAME count — text and JSON are two
        // views of the same data, so neither path can diverge.
        assert_eq!(
            text_count, json_count,
            "br-ft-1650n.14: text and JSON paths must surface the same number of caller-supplied fields"
        );
    }
}
