//! Galaxy Brain Evidence Ledger UI for `ft why`.
//!
//! Renders a beautiful terminal evidence card showing the mathematical
//! proof that a reflex execution was justified. Operators need to trust
//! the ARS subconscious — this module provides transparency.
//!
//! # Evidence Card Contents
//!
//! ```text
//! ┌─────────────────── ARS Evidence Card ───────────────────┐
//! │ Reflex: restart-app (v2) · Cluster: c-network           │
//! │ Maturity: Graduated · Executions: 47 ok / 2 fail        │
//! ├─────────────────────────────────────────────────────────┤
//! │ ⚡ E-Value Drift:  1.23 / 20.0 (safe)                   │
//! │ 📊 Replay:         94% pass (17/18 incidents)           │
//! │ 🔒 Blast Radius:   Allow (Graduated tier)               │
//! │ 📐 Evidence:       4 entries, complete, verdict=Accept   │
//! ├─────────────────────────────────────────────────────────┤
//! │ Timeline: calibrated → 47 successes → graduated → exec  │
//! └─────────────────────────────────────────────────────────┘
//! ```

use serde::{Deserialize, Serialize};

use crate::ars_blast_radius::{BlastDecision, MaturityTier};
use crate::ars_drift::DriftVerdict;
use crate::ars_evidence::{EvidenceVerdict, LedgerDigest};
use crate::ars_evolve::VersionStatus;
use crate::ars_fst::ReflexId;
use crate::ars_replay::ReplayAssessment;

// =============================================================================
// Evidence card
// =============================================================================

/// Complete evidence card for an ARS execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceCard {
    /// Reflex identity.
    pub reflex_id: ReflexId,
    /// Human-readable reflex name.
    pub reflex_name: String,
    /// Version number.
    pub version: u32,
    /// Cluster ID.
    pub cluster_id: String,
    /// Maturity tier.
    pub maturity: MaturityTier,
    /// Version status.
    pub status: VersionStatus,
    /// Success count.
    pub successes: u64,
    /// Failure count.
    pub failures: u64,
    /// E-value drift section.
    pub drift: DriftSection,
    /// Replay validation section.
    pub replay: ReplaySection,
    /// Blast radius section.
    pub blast_radius: BlastSection,
    /// Evidence ledger section.
    pub evidence: EvidenceSection,
    /// Timeline events.
    pub timeline: Vec<TimelineEvent>,
    /// Timestamp of card generation.
    pub generated_at_ms: u64,
}

/// Drift detection evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftSection {
    /// Current e-value.
    pub e_value: f64,
    /// Rejection threshold (1/α).
    pub threshold: f64,
    /// Whether drift is detected.
    pub is_drifted: bool,
    /// Null (calibrated) success rate.
    pub null_rate: f64,
    /// Observed success rate.
    pub observed_rate: f64,
    /// Total observations in the monitor.
    pub observations: usize,
}

impl DriftSection {
    /// Create from a DriftVerdict.
    pub fn from_verdict(verdict: &DriftVerdict, threshold: f64) -> Self {
        match verdict {
            DriftVerdict::NoDrift { e_value, null_rate } => Self {
                e_value: *e_value,
                threshold,
                is_drifted: false,
                null_rate: *null_rate,
                observed_rate: *null_rate,
                observations: 0,
            },
            DriftVerdict::Drifted {
                e_value,
                null_rate,
                observed_rate,
                observations,
            } => Self {
                e_value: *e_value,
                threshold,
                is_drifted: true,
                null_rate: *null_rate,
                observed_rate: *observed_rate,
                observations: *observations,
            },
            DriftVerdict::InsufficientData { observations, .. } => Self {
                e_value: 1.0,
                threshold,
                is_drifted: false,
                null_rate: 0.0,
                observed_rate: 0.0,
                observations: *observations,
            },
        }
    }

    /// Status label.
    pub fn status_label(&self) -> &'static str {
        if self.is_drifted {
            "DRIFTED"
        } else if self.observations == 0 {
            "calibrating"
        } else {
            "safe"
        }
    }
}

/// Replay validation evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaySection {
    /// Pass rate (0.0–1.0).
    pub pass_rate: f64,
    /// Incidents replayed.
    pub incidents: usize,
    /// Assessment.
    pub validated: bool,
    /// Human-readable reason (if rejected or insufficient).
    pub note: String,
}

impl ReplaySection {
    /// Create from a ReplayAssessment.
    pub fn from_assessment(assessment: &ReplayAssessment) -> Self {
        match assessment {
            ReplayAssessment::Validated {
                pass_rate,
                incidents,
            } => Self {
                pass_rate: *pass_rate,
                incidents: *incidents,
                validated: true,
                note: String::new(),
            },
            ReplayAssessment::Rejected {
                pass_rate,
                incidents,
                reason,
            } => Self {
                pass_rate: *pass_rate,
                incidents: *incidents,
                validated: false,
                note: reason.clone(),
            },
            ReplayAssessment::InsufficientData {
                available,
                required,
            } => Self {
                pass_rate: 0.0,
                incidents: *available,
                validated: false,
                note: format!("need {} incidents, have {}", required, available),
            },
        }
    }

    /// Status label.
    pub fn status_label(&self) -> &'static str {
        if self.validated { "PASS" } else { "FAIL" }
    }
}

/// Blast radius evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlastSection {
    /// Whether execution was allowed.
    pub allowed: bool,
    /// Tier at decision time.
    pub tier: MaturityTier,
    /// Deny reason (if denied).
    pub deny_reason: Option<String>,
}

impl BlastSection {
    /// Create from a BlastDecision.
    pub fn from_decision(decision: &BlastDecision) -> Self {
        match decision {
            BlastDecision::Allow { tier } => Self {
                allowed: true,
                tier: *tier,
                deny_reason: None,
            },
            BlastDecision::Deny { reason, tier } => Self {
                allowed: false,
                tier: *tier,
                deny_reason: Some(format!("{:?}", reason)),
            },
        }
    }

    /// Status label.
    pub fn status_label(&self) -> &'static str {
        if self.allowed { "Allow" } else { "Deny" }
    }
}

/// Evidence ledger summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceSection {
    /// Number of evidence entries.
    pub entry_count: usize,
    /// Whether the ledger is complete.
    pub is_complete: bool,
    /// Overall verdict.
    pub verdict: EvidenceVerdict,
    /// Categories present.
    pub categories: Vec<String>,
    /// Root hash (integrity).
    pub root_hash: String,
}

impl EvidenceSection {
    /// Create from a LedgerDigest.
    pub fn from_digest(digest: &LedgerDigest) -> Self {
        Self {
            entry_count: digest.entry_count,
            is_complete: digest.is_complete,
            verdict: digest.overall_verdict,
            categories: digest
                .categories_present
                .iter()
                .map(|c| format!("{:?}", c))
                .collect(),
            root_hash: digest.root_hash.clone(),
        }
    }

    /// Status label.
    pub fn status_label(&self) -> &'static str {
        match self.verdict {
            EvidenceVerdict::Support => "Support",
            EvidenceVerdict::Neutral => "Neutral",
            EvidenceVerdict::Reject => "Reject",
        }
    }
}

/// Timeline event for the card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// Timestamp (ms).
    pub timestamp_ms: u64,
    /// Event label.
    pub label: String,
    /// Event kind.
    pub kind: TimelineKind,
}

/// Kind of timeline event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimelineKind {
    Calibrated,
    Promoted,
    Executed,
    Drifted,
    Evolved,
    Deprecated,
}

// =============================================================================
// Card renderer
// =============================================================================

/// Render an evidence card as formatted text.
pub fn render_card(card: &EvidenceCard) -> String {
    let width = 60;
    let border = "─".repeat(width - 2);
    let mut lines = Vec::new();

    // Top border.
    lines.push(format!("┌{}┐", border));

    // Title.
    let title = format!(
        " Reflex: {} (v{}) · Cluster: {}",
        card.reflex_name, card.version, card.cluster_id
    );
    lines.push(format!("│{}│", pad_right(&title, width - 2)));

    let info = format!(
        " Maturity: {} · Execs: {} ok / {} fail",
        card.maturity.name(),
        card.successes,
        card.failures
    );
    lines.push(format!("│{}│", pad_right(&info, width - 2)));

    // Separator.
    lines.push(format!("├{}┤", border));

    // Drift.
    let drift_label = format!(
        " E-Value: {:.2} / {:.1} ({})",
        card.drift.e_value,
        card.drift.threshold,
        card.drift.status_label()
    );
    lines.push(format!("│{}│", pad_right(&drift_label, width - 2)));

    // Replay.
    let replay_label = format!(
        " Replay: {:.0}% {} ({} incidents)",
        card.replay.pass_rate * 100.0,
        card.replay.status_label(),
        card.replay.incidents
    );
    lines.push(format!("│{}│", pad_right(&replay_label, width - 2)));

    // Blast radius.
    let blast_label = format!(
        " Blast: {} ({} tier)",
        card.blast_radius.status_label(),
        card.blast_radius.tier.name()
    );
    lines.push(format!("│{}│", pad_right(&blast_label, width - 2)));

    // Evidence.
    let evidence_label = format!(
        " Evidence: {} entries, {}, verdict={}",
        card.evidence.entry_count,
        if card.evidence.is_complete {
            "complete"
        } else {
            "incomplete"
        },
        card.evidence.status_label()
    );
    lines.push(format!("│{}│", pad_right(&evidence_label, width - 2)));

    // Timeline.
    if !card.timeline.is_empty() {
        lines.push(format!("├{}┤", border));
        let events: Vec<String> = card.timeline.iter().map(|e| e.label.clone()).collect();
        let timeline_str = format!(" Timeline: {}", events.join(" -> "));
        // Truncate if too long (safe for multi-byte UTF-8).
        let truncated = if timeline_str.chars().count() > width - 2 {
            let safe_slice: String = timeline_str.chars().take(width - 5).collect();
            format!("{}...", safe_slice)
        } else {
            timeline_str
        };
        lines.push(format!("│{}│", pad_right(&truncated, width - 2)));
    }

    // Bottom border.
    lines.push(format!("└{}┘", border));

    lines.join("\n")
}

/// Pad a string to the right to fill `width` characters.
fn pad_right(s: &str, width: usize) -> String {
    let char_count = s.chars().count();
    if char_count >= width {
        s.chars().take(width).collect()
    } else {
        format!("{}{}", s, " ".repeat(width - char_count))
    }
}

/// Render a compact one-line summary.
pub fn render_summary(card: &EvidenceCard) -> String {
    format!(
        "[{}] {} v{} | drift={} replay={} blast={} evidence={}",
        card.maturity.name(),
        card.reflex_name,
        card.version,
        card.drift.status_label(),
        card.replay.status_label(),
        card.blast_radius.status_label(),
        card.evidence.status_label(),
    )
}

// =============================================================================
// Card builder
// =============================================================================

/// Builder for constructing evidence cards.
pub struct EvidenceCardBuilder {
    reflex_id: ReflexId,
    reflex_name: String,
    version: u32,
    cluster_id: String,
    maturity: MaturityTier,
    status: VersionStatus,
    successes: u64,
    failures: u64,
    drift: Option<DriftSection>,
    replay: Option<ReplaySection>,
    blast_radius: Option<BlastSection>,
    evidence: Option<EvidenceSection>,
    timeline: Vec<TimelineEvent>,
    generated_at_ms: u64,
}

impl EvidenceCardBuilder {
    /// Start building a card.
    pub fn new(reflex_id: ReflexId, reflex_name: &str) -> Self {
        Self {
            reflex_id,
            reflex_name: reflex_name.to_string(),
            version: 1,
            cluster_id: String::new(),
            maturity: MaturityTier::Incubating,
            status: VersionStatus::Active,
            successes: 0,
            failures: 0,
            drift: None,
            replay: None,
            blast_radius: None,
            evidence: None,
            timeline: Vec::new(),
            generated_at_ms: 0,
        }
    }

    pub fn version(mut self, v: u32) -> Self {
        self.version = v;
        self
    }

    pub fn cluster(mut self, c: &str) -> Self {
        self.cluster_id = c.to_string();
        self
    }

    pub fn maturity(mut self, m: MaturityTier) -> Self {
        self.maturity = m;
        self
    }

    pub fn status(mut self, s: VersionStatus) -> Self {
        self.status = s;
        self
    }

    pub fn executions(mut self, successes: u64, failures: u64) -> Self {
        self.successes = successes;
        self.failures = failures;
        self
    }

    pub fn drift(mut self, d: DriftSection) -> Self {
        self.drift = Some(d);
        self
    }

    pub fn replay(mut self, r: ReplaySection) -> Self {
        self.replay = Some(r);
        self
    }

    pub fn blast_radius(mut self, b: BlastSection) -> Self {
        self.blast_radius = Some(b);
        self
    }

    pub fn evidence(mut self, e: EvidenceSection) -> Self {
        self.evidence = Some(e);
        self
    }

    pub fn timeline_event(mut self, ts: u64, label: &str, kind: TimelineKind) -> Self {
        self.timeline.push(TimelineEvent {
            timestamp_ms: ts,
            label: label.to_string(),
            kind,
        });
        self
    }

    pub fn generated_at(mut self, ts: u64) -> Self {
        self.generated_at_ms = ts;
        self
    }

    /// Build the evidence card.
    pub fn build(self) -> EvidenceCard {
        EvidenceCard {
            reflex_id: self.reflex_id,
            reflex_name: self.reflex_name,
            version: self.version,
            cluster_id: self.cluster_id,
            maturity: self.maturity,
            status: self.status,
            successes: self.successes,
            failures: self.failures,
            drift: self.drift.unwrap_or(DriftSection {
                e_value: 1.0,
                threshold: 20.0,
                is_drifted: false,
                null_rate: 0.0,
                observed_rate: 0.0,
                observations: 0,
            }),
            replay: self.replay.unwrap_or(ReplaySection {
                pass_rate: 0.0,
                incidents: 0,
                validated: false,
                note: "not yet validated".to_string(),
            }),
            blast_radius: self.blast_radius.unwrap_or(BlastSection {
                allowed: false,
                tier: self.maturity,
                deny_reason: None,
            }),
            evidence: self.evidence.unwrap_or(EvidenceSection {
                entry_count: 0,
                is_complete: false,
                verdict: EvidenceVerdict::Neutral,
                categories: Vec::new(),
                root_hash: String::new(),
            }),
            timeline: self.timeline,
            generated_at_ms: self.generated_at_ms,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_card() -> EvidenceCard {
        EvidenceCardBuilder::new(1, "restart-app")
            .version(2)
            .cluster("c-network")
            .maturity(MaturityTier::Graduated)
            .executions(47, 2)
            .drift(DriftSection {
                e_value: 1.23,
                threshold: 20.0,
                is_drifted: false,
                null_rate: 0.9,
                observed_rate: 0.88,
                observations: 49,
            })
            .replay(ReplaySection {
                pass_rate: 0.94,
                incidents: 18,
                validated: true,
                note: String::new(),
            })
            .blast_radius(BlastSection {
                allowed: true,
                tier: MaturityTier::Graduated,
                deny_reason: None,
            })
            .evidence(EvidenceSection {
                entry_count: 4,
                is_complete: true,
                verdict: EvidenceVerdict::Support,
                categories: vec!["Generalize".to_string(), "Timeout".to_string()],
                root_hash: "12345".to_string(),
            })
            .timeline_event(1000, "calibrated", TimelineKind::Calibrated)
            .timeline_event(2000, "graduated", TimelineKind::Promoted)
            .timeline_event(3000, "executed", TimelineKind::Executed)
            .generated_at(3000)
            .build()
    }

    // ---- DriftSection ----

    #[test]
    fn drift_safe_label() {
        let d = DriftSection {
            e_value: 1.0,
            threshold: 20.0,
            is_drifted: false,
            null_rate: 0.8,
            observed_rate: 0.8,
            observations: 10,
        };
        assert_eq!(d.status_label(), "safe");
    }

    #[test]
    fn drift_drifted_label() {
        let d = DriftSection {
            e_value: 25.0,
            threshold: 20.0,
            is_drifted: true,
            null_rate: 0.8,
            observed_rate: 0.3,
            observations: 50,
        };
        assert_eq!(d.status_label(), "DRIFTED");
    }

    #[test]
    fn drift_calibrating_label() {
        let d = DriftSection {
            e_value: 1.0,
            threshold: 20.0,
            is_drifted: false,
            null_rate: 0.0,
            observed_rate: 0.0,
            observations: 0,
        };
        assert_eq!(d.status_label(), "calibrating");
    }

    #[test]
    fn drift_from_no_drift_verdict() {
        let verdict = DriftVerdict::NoDrift {
            e_value: 2.5,
            null_rate: 0.85,
        };
        let section = DriftSection::from_verdict(&verdict, 20.0);
        assert!(!section.is_drifted);
        let diff = (section.e_value - 2.5).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn drift_from_drifted_verdict() {
        let verdict = DriftVerdict::Drifted {
            e_value: 25.0,
            null_rate: 0.8,
            observed_rate: 0.3,
            observations: 50,
        };
        let section = DriftSection::from_verdict(&verdict, 20.0);
        assert!(section.is_drifted);
    }

    // ---- ReplaySection ----

    #[test]
    fn replay_pass_label() {
        let r = ReplaySection {
            pass_rate: 0.9,
            incidents: 10,
            validated: true,
            note: String::new(),
        };
        assert_eq!(r.status_label(), "PASS");
    }

    #[test]
    fn replay_fail_label() {
        let r = ReplaySection {
            pass_rate: 0.3,
            incidents: 10,
            validated: false,
            note: "too low".to_string(),
        };
        assert_eq!(r.status_label(), "FAIL");
    }

    #[test]
    fn replay_from_validated() {
        let a = ReplayAssessment::Validated {
            pass_rate: 0.95,
            incidents: 20,
        };
        let section = ReplaySection::from_assessment(&a);
        assert!(section.validated);
        assert_eq!(section.incidents, 20);
    }

    #[test]
    fn replay_from_rejected() {
        let a = ReplayAssessment::Rejected {
            pass_rate: 0.3,
            incidents: 10,
            reason: "low rate".to_string(),
        };
        let section = ReplaySection::from_assessment(&a);
        assert!(!section.validated);
    }

    // ---- BlastSection ----

    #[test]
    fn blast_allow_label() {
        let b = BlastSection {
            allowed: true,
            tier: MaturityTier::Graduated,
            deny_reason: None,
        };
        assert_eq!(b.status_label(), "Allow");
    }

    #[test]
    fn blast_deny_label() {
        let b = BlastSection {
            allowed: false,
            tier: MaturityTier::Incubating,
            deny_reason: Some("SwarmLimit".to_string()),
        };
        assert_eq!(b.status_label(), "Deny");
    }

    #[test]
    fn blast_from_allow_decision() {
        let d = BlastDecision::Allow {
            tier: MaturityTier::Veteran,
        };
        let section = BlastSection::from_decision(&d);
        assert!(section.allowed);
        assert_eq!(section.tier, MaturityTier::Veteran);
    }

    // ---- EvidenceSection ----

    #[test]
    fn evidence_support_label() {
        let e = EvidenceSection {
            entry_count: 3,
            is_complete: true,
            verdict: EvidenceVerdict::Support,
            categories: vec![],
            root_hash: String::new(),
        };
        assert_eq!(e.status_label(), "Support");
    }

    // ---- Card rendering ----

    #[test]
    fn card_renders_with_box() {
        let card = make_card();
        let text = render_card(&card);
        assert!(text.contains("restart-app"));
        assert!(text.contains("Graduated"));
        assert!(text.starts_with('┌'));
        assert!(text.ends_with('┘'));
    }

    #[test]
    fn summary_renders_compact() {
        let card = make_card();
        let summary = render_summary(&card);
        assert!(summary.contains("Graduated"));
        assert!(summary.contains("restart-app"));
        assert!(summary.contains("safe"));
    }

    // ---- Builder ----

    #[test]
    fn builder_defaults() {
        let card = EvidenceCardBuilder::new(1, "test").build();
        assert_eq!(card.reflex_id, 1);
        assert_eq!(card.version, 1);
        assert_eq!(card.maturity, MaturityTier::Incubating);
    }

    #[test]
    fn builder_chainable() {
        let card = EvidenceCardBuilder::new(1, "test")
            .version(3)
            .cluster("c1")
            .maturity(MaturityTier::Veteran)
            .executions(100, 5)
            .generated_at(9999)
            .build();
        assert_eq!(card.version, 3);
        assert_eq!(card.cluster_id, "c1");
        assert_eq!(card.maturity, MaturityTier::Veteran);
        assert_eq!(card.successes, 100);
    }

    // ---- Serde roundtrips ----

    #[test]
    fn card_serde_roundtrip() {
        let card = make_card();
        let json = serde_json::to_string(&card).unwrap();
        let decoded: EvidenceCard = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.reflex_id, card.reflex_id);
        assert_eq!(decoded.version, card.version);
    }

    #[test]
    fn drift_section_serde_roundtrip() {
        let d = DriftSection {
            e_value: 1.5,
            threshold: 20.0,
            is_drifted: false,
            null_rate: 0.8,
            observed_rate: 0.78,
            observations: 30,
        };
        let json = serde_json::to_string(&d).unwrap();
        let decoded: DriftSection = serde_json::from_str(&json).unwrap();
        let diff = (decoded.e_value - d.e_value).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn timeline_event_serde_roundtrip() {
        let e = TimelineEvent {
            timestamp_ms: 1000,
            label: "calibrated".to_string(),
            kind: TimelineKind::Calibrated,
        };
        let json = serde_json::to_string(&e).unwrap();
        let decoded: TimelineEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, e);
    }

    #[test]
    fn timeline_kind_serde_roundtrip() {
        let kinds = [
            TimelineKind::Calibrated,
            TimelineKind::Promoted,
            TimelineKind::Executed,
            TimelineKind::Drifted,
            TimelineKind::Evolved,
            TimelineKind::Deprecated,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let decoded: TimelineKind = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, kind);
        }
    }

    // ---- Evidence neutral/reject labels ----

    #[test]
    fn evidence_neutral_label() {
        let e = EvidenceSection {
            entry_count: 0,
            is_complete: false,
            verdict: EvidenceVerdict::Neutral,
            categories: vec![],
            root_hash: String::new(),
        };
        assert_eq!(e.status_label(), "Neutral");
    }

    #[test]
    fn evidence_reject_label() {
        let e = EvidenceSection {
            entry_count: 2,
            is_complete: true,
            verdict: EvidenceVerdict::Reject,
            categories: vec!["Timeout".to_string()],
            root_hash: "abc".to_string(),
        };
        assert_eq!(e.status_label(), "Reject");
    }

    // ---- From digest ----

    #[test]
    fn evidence_from_digest() {
        let digest = LedgerDigest {
            root_hash: "h123".to_string(),
            entry_count: 5,
            categories_present: vec![],
            is_complete: true,
            overall_verdict: EvidenceVerdict::Support,
            timestamp_range: (100, 200),
        };
        let section = EvidenceSection::from_digest(&digest);
        assert_eq!(section.entry_count, 5);
        assert!(section.is_complete);
        assert_eq!(section.root_hash, "h123");
    }

    // ---- Replay insufficient ----

    #[test]
    fn replay_from_insufficient() {
        let a = ReplayAssessment::InsufficientData {
            available: 1,
            required: 5,
        };
        let section = ReplaySection::from_assessment(&a);
        assert!(!section.validated);
        assert!(section.note.contains("need 5"));
    }

    // ---- Blast deny from decision ----

    #[test]
    fn blast_from_deny_decision() {
        let d = BlastDecision::Deny {
            reason: crate::ars_blast_radius::DenyReason::SwarmLimit,
            tier: MaturityTier::Incubating,
        };
        let section = BlastSection::from_decision(&d);
        assert!(!section.allowed);
        assert!(section.deny_reason.is_some());
    }

    // ---- Drift from insufficient ----

    #[test]
    fn drift_from_insufficient_verdict() {
        let verdict = DriftVerdict::InsufficientData {
            observations: 5,
            required: 20,
        };
        let section = DriftSection::from_verdict(&verdict, 20.0);
        assert!(!section.is_drifted);
        assert_eq!(section.observations, 5);
    }

    // ---- Card rendering details ----

    #[test]
    fn card_contains_drift_section() {
        let card = make_card();
        let text = render_card(&card);
        assert!(text.contains("E-Value"));
        assert!(text.contains("safe"));
    }

    #[test]
    fn card_contains_replay_section() {
        let card = make_card();
        let text = render_card(&card);
        assert!(text.contains("Replay"));
        assert!(text.contains("PASS"));
    }

    #[test]
    fn card_contains_blast_section() {
        let card = make_card();
        let text = render_card(&card);
        assert!(text.contains("Blast"));
        assert!(text.contains("Allow"));
    }

    #[test]
    fn card_contains_evidence_section() {
        let card = make_card();
        let text = render_card(&card);
        assert!(text.contains("Evidence"));
        assert!(text.contains("Support"));
    }

    #[test]
    fn card_contains_timeline() {
        let card = make_card();
        let text = render_card(&card);
        assert!(text.contains("Timeline"));
        assert!(text.contains("calibrated"));
    }

    #[test]
    fn summary_contains_all_verdicts() {
        let card = make_card();
        let summary = render_summary(&card);
        assert!(summary.contains("safe"));
        assert!(summary.contains("PASS"));
        assert!(summary.contains("Allow"));
        assert!(summary.contains("Support"));
    }

    #[test]
    fn empty_card_renders() {
        let card = EvidenceCardBuilder::new(1, "test").build();
        let text = render_card(&card);
        assert!(text.starts_with('┌'));
        assert!(text.ends_with('┘'));
    }
}
