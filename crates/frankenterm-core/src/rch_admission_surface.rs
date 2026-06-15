//! Operator-facing surface rendering for the RCH admission diagnosis (ft-69gwh.5).
//!
//! The admission collector in [`crate::rch_admission`] produces an *advisory*
//! diagnosis: it can report that a proof command looks runnable or blocked, but
//! it is NEVER proof that Cargo, tests, clippy, benches, fuzzers, or release
//! gates passed. A dry-run, queue snapshot, worker-health report, or
//! schema-valid diagnostic must not be cited as compiled proof.
//!
//! This module is the single rendering layer the human (`ft doctor
//! rch-admission`), robot, and MCP surfaces share, so the advisory-vs-proof
//! distinction is expressed identically everywhere the diagnosis is exposed.
//! [`RchAdmissionSurface`] wraps a base [`RchAdmissionReport`] (or a
//! [`RchAdmissionPreflightReport`]) into a stable envelope whose `ok` field
//! means only "the read-only diagnosis was produced", and whose `proof_blocked`
//! field is the value a caller actually gates a proof lane on.

use serde::{Deserialize, Serialize};

use crate::rch_admission::{
    RchAdmissionPreflightReport, RchAdmissionPreflightVerdict, RchAdmissionProofStatus,
    RchAdmissionReasonCode, RchAdmissionReport,
};

/// Banner attached to every surface so the diagnosis is never mistaken for a
/// green build. Surfaced verbatim by the doctor, robot, and MCP renderings.
pub const RCH_ADMISSION_NOT_PROOF_BANNER: &str = "ADVISORY ONLY — RCH admission diagnosis, \
     not proof that Cargo, tests, clippy, benches, fuzzers, or release gates passed.";

/// Which operator surface emitted the diagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionSurfaceKind {
    /// `ft doctor rch-admission` human surface.
    Doctor,
    /// `ft robot` machine surface.
    Robot,
    /// MCP tool/resource surface.
    Mcp,
}

impl RchAdmissionSurfaceKind {
    /// Stable token for the surface kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Doctor => "doctor",
            Self::Robot => "robot",
            Self::Mcp => "mcp",
        }
    }
}

/// One reason-code row rendered for an operator surface, carrying the stable
/// recommendation plus the approval/blocking flags a caller routes on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionSurfaceReason {
    /// Stable blocker or advisory reason code.
    pub reason_code: RchAdmissionReasonCode,
    /// Safe next action tied to the reason code.
    pub recommendation: String,
    /// Whether clearing the reason requires operator-approved action.
    pub operator_approval_required: bool,
    /// Whether the reason blocks a remote-required proof lane.
    pub blocks_proof: bool,
}

impl RchAdmissionSurfaceReason {
    #[must_use]
    fn from_code(reason_code: RchAdmissionReasonCode) -> Self {
        Self {
            reason_code,
            recommendation: reason_code.recommendation().to_string(),
            operator_approval_required: reason_code.operator_approval_required(),
            blocks_proof: reason_code.blocks_proof(),
        }
    }
}

/// Stable robot/MCP envelope wrapping an RCH admission report.
///
/// `ok` is `true` whenever the read-only diagnosis was produced successfully —
/// it does NOT mean the proof command will pass. `proof_blocked` is the field
/// callers gate on. `advisory_only` and `not_proof_banner` are always set, so a
/// consumer can never cite this envelope as compiled proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionSurface {
    /// Surface that produced this rendering.
    pub surface: RchAdmissionSurfaceKind,
    /// Always `true` for a successfully produced diagnosis (read-only succeeded).
    pub ok: bool,
    /// Always `true`: the diagnosis is advisory and is not build proof.
    pub advisory_only: bool,
    /// Human-readable "not proof" banner, identical across surfaces.
    pub not_proof_banner: String,
    /// Proof status carried from the base report.
    pub proof_status: RchAdmissionProofStatus,
    /// Whether a remote-required proof lane is blocked and must not be claimed.
    pub proof_blocked: bool,
    /// Preflight verdict when the envelope was built from a preflight report.
    pub verdict: Option<RchAdmissionPreflightVerdict>,
    /// One-line operator-facing headline.
    pub headline: String,
    /// Reason rows with recommendations and approval/blocking flags.
    pub reasons: Vec<RchAdmissionSurfaceReason>,
    /// Count of retained read-only evidence citations on the base report.
    pub citation_count: usize,
    /// The full underlying advisory report.
    pub report: RchAdmissionReport,
}

impl RchAdmissionSurface {
    /// Build a surface envelope from a base admission report.
    #[must_use]
    pub fn from_report(surface: RchAdmissionSurfaceKind, report: RchAdmissionReport) -> Self {
        let proof_blocked = report.proof_status == RchAdmissionProofStatus::Blocked
            || report.reason_codes.iter().any(|code| code.blocks_proof());
        let reasons = report
            .reason_codes
            .iter()
            .copied()
            .map(RchAdmissionSurfaceReason::from_code)
            .collect::<Vec<_>>();
        let headline = surface_headline(report.proof_status, None, proof_blocked, reasons.len());
        let citation_count = report.citations.len();
        Self {
            surface,
            ok: true,
            advisory_only: true,
            not_proof_banner: RCH_ADMISSION_NOT_PROOF_BANNER.to_string(),
            proof_status: report.proof_status,
            proof_blocked,
            verdict: None,
            headline,
            reasons,
            citation_count,
            report,
        }
    }

    /// Build a surface envelope from a preflight report, carrying the verdict
    /// and preferring the preflight's verdict-aware summary as the headline.
    #[must_use]
    pub fn from_preflight(
        surface: RchAdmissionSurfaceKind,
        preflight: RchAdmissionPreflightReport,
    ) -> Self {
        let verdict = preflight.verdict;
        let summary = preflight.summary.clone();
        let mut envelope = Self::from_report(surface, preflight.admission_report);
        envelope.verdict = Some(verdict);
        envelope.proof_blocked =
            envelope.proof_blocked || verdict == RchAdmissionPreflightVerdict::Blocked;
        if summary.is_empty() {
            envelope.headline = surface_headline(
                envelope.proof_status,
                Some(verdict),
                envelope.proof_blocked,
                0,
            );
        } else {
            envelope.headline = summary;
        }
        envelope
    }

    /// Render the human-facing `ft doctor rch-admission` text block. The first
    /// line is always the not-proof banner.
    #[must_use]
    pub fn doctor_lines(&self) -> Vec<String> {
        let mut lines = vec![
            self.not_proof_banner.clone(),
            format!("surface: {}", self.surface.as_str()),
            format!("proof_status: {}", enum_token(&self.proof_status)),
        ];
        if let Some(verdict) = self.verdict {
            lines.push(format!("verdict: {}", enum_token(&verdict)));
        }
        lines.push(format!(
            "proof_blocked: {}",
            if self.proof_blocked { "yes" } else { "no" }
        ));
        lines.push(self.headline.clone());
        if self.reasons.is_empty() {
            lines.push("reasons: none (no blocking or advisory reason codes observed)".to_string());
        } else {
            lines.push(format!("reasons ({}):", self.reasons.len()));
            for reason in &self.reasons {
                let mut flags = Vec::new();
                if reason.blocks_proof {
                    flags.push("BLOCKS-PROOF");
                }
                if reason.operator_approval_required {
                    flags.push("OPERATOR-APPROVAL");
                }
                let flag_str = if flags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", flags.join("] ["))
                };
                lines.push(format!(
                    "  - {}{} -> {}",
                    enum_token(&reason.reason_code),
                    flag_str,
                    reason.recommendation
                ));
            }
        }
        lines.push(format!(
            "citations: {} read-only evidence row(s)",
            self.citation_count
        ));
        lines
    }
}

/// Render the stable snake_case token for a serde unit-enum value, reusing the
/// enum's `#[serde(rename_all = "snake_case")]` mapping as the single source of
/// truth (no exhaustive match to drift when a variant is added).
fn enum_token<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn surface_headline(
    proof_status: RchAdmissionProofStatus,
    verdict: Option<RchAdmissionPreflightVerdict>,
    proof_blocked: bool,
    reason_count: usize,
) -> String {
    if proof_blocked {
        let verdict_note = match verdict {
            Some(RchAdmissionPreflightVerdict::Invalid) => " (command cannot produce RCH proof)",
            _ => "",
        };
        return format!(
            "RCH proof lane BLOCKED{verdict_note} — {reason_count} advisory reason(s); \
             resolve before claiming remote Cargo evidence"
        );
    }
    match proof_status {
        RchAdmissionProofStatus::Runnable => {
            "RCH proof lane appears runnable (advisory) — a remote-required Cargo lane may be \
             attempted, but this diagnosis is not proof"
                .to_string()
        }
        RchAdmissionProofStatus::AdvisoryOnly
        | RchAdmissionProofStatus::Blocked
        | RchAdmissionProofStatus::Unknown => {
            "RCH admission inconclusive (advisory) — treat the proof lane as not-yet-runnable"
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rch_admission::{
        RCH_ADMISSION_CONTRACT_ID, RCH_ADMISSION_SCHEMA_VERSION, RchAdmissionAgentMailDiagnostic,
        RchAdmissionBeadsDiagnostic, RchAdmissionCollectorInput, RchAdmissionCommandDiagnostic,
        RchAdmissionLocalDiskDiagnostic, RchAdmissionQueueDiagnostic, RchAdmissionRecommendation,
        analyze_rch_admission_proof_command, build_rch_admission_preflight_report,
    };

    fn report_with(
        proof_status: RchAdmissionProofStatus,
        reason_codes: Vec<RchAdmissionReasonCode>,
    ) -> RchAdmissionReport {
        let recommendations = reason_codes
            .iter()
            .copied()
            .map(RchAdmissionRecommendation::from_reason)
            .collect();
        RchAdmissionReport {
            schema_version: RCH_ADMISSION_SCHEMA_VERSION,
            contract_id: RCH_ADMISSION_CONTRACT_ID.to_string(),
            generated_at_ms: 0,
            source: "unit-test".to_string(),
            advisory_only: true,
            proof_status,
            command: RchAdmissionCommandDiagnostic::new(
                "cargo test -p frankenterm-core --lib rch_admission_surface",
            ),
            local_disk: RchAdmissionLocalDiskDiagnostic::not_checked(),
            beads: RchAdmissionBeadsDiagnostic::unknown(),
            agent_mail: RchAdmissionAgentMailDiagnostic::unknown(),
            rch_queue: RchAdmissionQueueDiagnostic::unknown(),
            worker_rejections: Vec::new(),
            cargo_jobs: None,
            estimated_slots: Some(4),
            reason_codes,
            recommendations,
            forbidden_actions: Vec::new(),
            citations: Vec::new(),
        }
    }

    #[test]
    fn not_proof_banner_and_advisory_always_set() {
        for status in [
            RchAdmissionProofStatus::Runnable,
            RchAdmissionProofStatus::Blocked,
            RchAdmissionProofStatus::Unknown,
            RchAdmissionProofStatus::AdvisoryOnly,
        ] {
            let surface = RchAdmissionSurface::from_report(
                RchAdmissionSurfaceKind::Robot,
                report_with(status, Vec::new()),
            );
            assert!(surface.ok, "ok means diagnosis produced, not proof passed");
            assert!(surface.advisory_only);
            assert_eq!(surface.not_proof_banner, RCH_ADMISSION_NOT_PROOF_BANNER);
            // The banner is always the first doctor line.
            assert_eq!(surface.doctor_lines()[0], RCH_ADMISSION_NOT_PROOF_BANNER);
        }
    }

    #[test]
    fn blocking_reason_code_marks_proof_blocked() {
        let surface = RchAdmissionSurface::from_report(
            RchAdmissionSurfaceKind::Doctor,
            report_with(
                RchAdmissionProofStatus::Unknown,
                vec![RchAdmissionReasonCode::CriticalPressure],
            ),
        );
        assert!(
            surface.proof_blocked,
            "CriticalPressure blocks the proof lane"
        );
        let reason = &surface.reasons[0];
        assert!(reason.blocks_proof);
        assert!(reason.operator_approval_required);
        assert!(!reason.recommendation.is_empty());
        let text = surface.doctor_lines().join("\n");
        assert!(
            text.contains("BLOCKED"),
            "doctor text surfaces the block: {text}"
        );
        assert!(
            text.contains("critical_pressure"),
            "stable reason token rendered: {text}"
        );
        assert!(text.contains("BLOCKS-PROOF"));
    }

    #[test]
    fn explicit_blocked_status_blocks_even_without_reason_codes() {
        let surface = RchAdmissionSurface::from_report(
            RchAdmissionSurfaceKind::Mcp,
            report_with(RchAdmissionProofStatus::Blocked, Vec::new()),
        );
        assert!(surface.proof_blocked);
    }

    #[test]
    fn runnable_report_is_not_blocked() {
        let surface = RchAdmissionSurface::from_report(
            RchAdmissionSurfaceKind::Robot,
            report_with(RchAdmissionProofStatus::Runnable, Vec::new()),
        );
        assert!(!surface.proof_blocked);
        assert!(surface.reasons.is_empty());
        let text = surface.doctor_lines().join("\n");
        assert!(text.contains("runnable"), "{text}");
        assert!(
            text.contains("not proof"),
            "advisory wording retained: {text}"
        );
    }

    #[test]
    fn surface_serializes_with_advisory_and_block_fields() {
        let surface = RchAdmissionSurface::from_report(
            RchAdmissionSurfaceKind::Mcp,
            report_with(
                RchAdmissionProofStatus::Blocked,
                vec![RchAdmissionReasonCode::NoAdmissibleWorkers],
            ),
        );
        let value = serde_json::to_value(&surface).expect("surface serializes");
        assert_eq!(value["advisory_only"], serde_json::json!(true));
        assert_eq!(value["proof_blocked"], serde_json::json!(true));
        assert_eq!(value["surface"], serde_json::json!("mcp"));
        assert!(value["not_proof_banner"].as_str().is_some());
        // Round-trips through serde without loss (re-serialize equals the original
        // value, which avoids depending on exact struct equality of nested types).
        let back: RchAdmissionSurface =
            serde_json::from_value(value.clone()).expect("surface deserializes");
        assert_eq!(
            serde_json::to_value(&back).expect("re-serialize"),
            value,
            "surface round-trips through serde without loss"
        );
    }

    #[test]
    fn preflight_envelope_carries_verdict() {
        let input = RchAdmissionCollectorInput::new(
            7,
            "unit-test",
            RchAdmissionCommandDiagnostic::new("cargo test -p frankenterm-core --lib"),
        );
        let proof_command = analyze_rch_admission_proof_command(
            "RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- \
             env CARGO_TARGET_DIR=/tmp/ft-69gwh5-cc4-target cargo test -p frankenterm-core --lib",
            std::iter::empty::<(&str, &str)>(),
            Some(4),
        );
        let preflight = build_rch_admission_preflight_report(&input, proof_command);
        let surface =
            RchAdmissionSurface::from_preflight(RchAdmissionSurfaceKind::Doctor, preflight);
        assert!(surface.verdict.is_some());
        assert!(surface.advisory_only);
        assert_eq!(surface.not_proof_banner, RCH_ADMISSION_NOT_PROOF_BANNER);
        // The verdict is rendered into the doctor text.
        let text = surface.doctor_lines().join("\n");
        assert!(text.contains("verdict:"), "{text}");
    }
}
