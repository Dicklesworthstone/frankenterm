//! Deterministic context-horizon risk prediction.
//!
//! This module is intentionally read-only. It consumes already-redacted,
//! structured context evidence and produces the v1 horizon DTO shape without
//! reading pane text or storing prompt/transcript content.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const CONTEXT_HORIZON_CONTRACT_ID: &str = "ft.context_horizon.v1";
pub const CONTEXT_HORIZON_SCHEMA_VERSION: u16 = 1;
const DEFAULT_HORIZON_WINDOW_MS: u64 = 15 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonEvidenceState {
    Measured,
    Inferred,
    Simulated,
    Stale,
    Unavailable,
    Mixed,
}

impl ContextHorizonEvidenceState {
    fn combine(states: impl IntoIterator<Item = Self>) -> Self {
        let states = states.into_iter().collect::<BTreeSet<_>>();
        match states.len() {
            0 => Self::Unavailable,
            1 => *states.iter().next().expect("state set has one entry"),
            _ => Self::Mixed,
        }
    }

    fn penalty(self) -> f64 {
        match self {
            Self::Measured => 0.0,
            Self::Inferred => 0.1,
            Self::Simulated => 0.25,
            Self::Stale => 0.35,
            Self::Unavailable => 0.5,
            Self::Mixed => 0.2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonRiskTier {
    Green,
    Yellow,
    Red,
    Black,
}

impl ContextHorizonRiskTier {
    fn from_pressure_tier(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "green" => Some(Self::Green),
            "yellow" => Some(Self::Yellow),
            "red" => Some(Self::Red),
            "black" => Some(Self::Black),
            _ => None,
        }
    }

    fn from_utilization(utilization: f64) -> Self {
        if utilization >= 0.98 {
            Self::Black
        } else if utilization >= 0.90 {
            Self::Red
        } else if utilization >= 0.75 {
            Self::Yellow
        } else {
            Self::Green
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonFailureClass {
    SourceRegression,
    PrivacyViolation,
    EnvironmentBlocked,
    UnavailableEvidence,
    TargetHardwareSkipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonActionKind {
    Monitor,
    RefreshEvidence,
    PrepareHandoff,
    DryRunCompact,
    RebalanceWorkload,
    PauseNewWork,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonUnavailableDomain {
    pub domain: String,
    pub evidence_state: ContextHorizonEvidenceState,
    pub failure_class: ContextHorizonFailureClass,
    pub reason: String,
}

impl ContextHorizonUnavailableDomain {
    #[must_use]
    pub fn unavailable(domain: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            evidence_state: ContextHorizonEvidenceState::Unavailable,
            failure_class: ContextHorizonFailureClass::UnavailableEvidence,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonCitation {
    pub source: String,
    pub pane_id: Option<u64>,
    pub evidence_state: ContextHorizonEvidenceState,
    pub generated_at_ms: u64,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHorizonPaneEvidence {
    pub pane_id: u64,
    pub active_context_present: bool,
    pub token_budget: Option<i64>,
    pub tokens_consumed: Option<i64>,
    pub pressure_tier: Option<String>,
    pub compaction_count: Option<i64>,
    pub last_rotated_at_ms: Option<i64>,
    pub last_activity_at_ms: Option<i64>,
    pub previous_utilization: Option<f64>,
    pub recent_rate_limit_events: u32,
    pub recent_compaction_events: u32,
    pub evidence_state: ContextHorizonEvidenceState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHorizonInput {
    pub generated_at_ms: u64,
    pub horizon_window_ms: u64,
    pub panes: Vec<ContextHorizonPaneEvidence>,
    pub unavailable_domains: Vec<ContextHorizonUnavailableDomain>,
    pub artifact_paths: Vec<String>,
}

impl ContextHorizonInput {
    #[must_use]
    pub fn new(generated_at_ms: u64) -> Self {
        Self {
            generated_at_ms,
            horizon_window_ms: DEFAULT_HORIZON_WINDOW_MS,
            panes: Vec::new(),
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHorizonPaneRisk {
    pub pane_id: u64,
    pub risk_tier: ContextHorizonRiskTier,
    pub evidence_state: ContextHorizonEvidenceState,
    pub utilization: f64,
    pub utilization_trend: f64,
    pub active_context_present: bool,
    pub token_budget: Option<u64>,
    pub tokens_consumed: Option<u64>,
    pub compaction_count: u32,
    pub last_rotation_age_ms: Option<u64>,
    pub last_activity_age_ms: Option<u64>,
    pub recent_rate_limit_events: u32,
    pub recent_compaction_events: u32,
    pub confidence: f64,
    pub reasons: Vec<String>,
    pub citations: Vec<ContextHorizonCitation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonFleetSummary {
    pub total_panes: usize,
    pub green_count: usize,
    pub yellow_count: usize,
    pub red_count: usize,
    pub black_count: usize,
    pub measured_count: usize,
    pub stale_count: usize,
    pub unavailable_count: usize,
    pub highest_risk_tier: ContextHorizonRiskTier,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonRecommendation {
    pub action_kind: ContextHorizonActionKind,
    pub pane_id: Option<u64>,
    pub priority: u8,
    pub reason: String,
    pub evidence_state: ContextHorizonEvidenceState,
    pub mutation_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonRedactionPolicy {
    pub raw_transcript_allowed: bool,
    pub raw_prompt_allowed: bool,
    pub bounded_citations_only: bool,
    pub secret_redaction_required: bool,
}

impl Default for ContextHorizonRedactionPolicy {
    fn default() -> Self {
        Self {
            raw_transcript_allowed: false,
            raw_prompt_allowed: false,
            bounded_citations_only: true,
            secret_redaction_required: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHorizonReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub source: String,
    pub evidence_state: ContextHorizonEvidenceState,
    pub horizon_window_ms: u64,
    pub fleet_summary: ContextHorizonFleetSummary,
    pub pane_risks: Vec<ContextHorizonPaneRisk>,
    pub recommendations: Vec<ContextHorizonRecommendation>,
    pub citations: Vec<ContextHorizonCitation>,
    pub unavailable_domains: Vec<ContextHorizonUnavailableDomain>,
    pub redaction_policy: ContextHorizonRedactionPolicy,
    pub artifact_paths: Vec<String>,
    pub raw_context_content_stored: bool,
}

#[must_use]
pub fn predict_context_horizon(input: &ContextHorizonInput) -> ContextHorizonReport {
    let mut unavailable_domains = input.unavailable_domains.clone();
    if input.panes.is_empty() && unavailable_domains.is_empty() {
        unavailable_domains.push(ContextHorizonUnavailableDomain::unavailable(
            "pane_contexts",
            "no tracked panes were available to score",
        ));
    }

    let pane_risks = input
        .panes
        .iter()
        .map(|pane| score_pane(input.generated_at_ms, input.horizon_window_ms, pane))
        .collect::<Vec<_>>();
    let fleet_summary = summarize_fleet(&pane_risks);
    let mut citations = pane_risks
        .iter()
        .flat_map(|pane| pane.citations.iter().cloned())
        .collect::<Vec<_>>();
    citations.extend(
        unavailable_domains
            .iter()
            .map(|domain| ContextHorizonCitation {
                source: domain.domain.clone(),
                pane_id: None,
                evidence_state: domain.evidence_state,
                generated_at_ms: input.generated_at_ms,
                fields: vec!["availability".to_string(), "reason".to_string()],
            }),
    );

    let recommendations = build_recommendations(&pane_risks, &unavailable_domains);
    let evidence_state = ContextHorizonEvidenceState::combine(
        pane_risks.iter().map(|risk| risk.evidence_state).chain(
            unavailable_domains
                .iter()
                .map(|domain| domain.evidence_state),
        ),
    );

    ContextHorizonReport {
        schema_version: CONTEXT_HORIZON_SCHEMA_VERSION,
        contract_id: CONTEXT_HORIZON_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        source: "native_context_horizon_predictor".to_string(),
        evidence_state,
        horizon_window_ms: input.horizon_window_ms,
        fleet_summary,
        pane_risks,
        recommendations,
        citations,
        unavailable_domains,
        redaction_policy: ContextHorizonRedactionPolicy::default(),
        artifact_paths: input.artifact_paths.clone(),
        raw_context_content_stored: false,
    }
}

fn score_pane(
    generated_at_ms: u64,
    horizon_window_ms: u64,
    pane: &ContextHorizonPaneEvidence,
) -> ContextHorizonPaneRisk {
    let mut reasons = Vec::new();
    let mut states = vec![pane.evidence_state];

    let token_budget = sanitize_positive_u64(pane.token_budget, "token_budget", &mut reasons);
    let tokens_consumed =
        sanitize_nonnegative_u64(pane.tokens_consumed, "tokens_consumed", &mut reasons);
    if token_budget.is_none() || tokens_consumed.is_none() {
        states.push(ContextHorizonEvidenceState::Unavailable);
    }

    let utilization = match (tokens_consumed, token_budget) {
        (Some(consumed), Some(budget)) if budget > 0 => {
            let raw = consumed as f64 / budget as f64;
            if raw > 1.0 {
                reasons
                    .push("tokens_consumed exceeds token_budget; utilization clamped".to_string());
            }
            raw.clamp(0.0, 1.0)
        }
        (Some(consumed), None) if consumed > 0 => {
            reasons.push("token_budget unavailable while tokens are consumed".to_string());
            1.0
        }
        _ => 0.0,
    };

    let mut risk_tier = ContextHorizonRiskTier::from_utilization(utilization);
    if let Some(pressure_tier) = pane
        .pressure_tier
        .as_deref()
        .and_then(ContextHorizonRiskTier::from_pressure_tier)
    {
        risk_tier = risk_tier.max(pressure_tier);
    } else if pane.pressure_tier.is_some() {
        reasons.push("pressure_tier was not recognized".to_string());
        states.push(ContextHorizonEvidenceState::Unavailable);
    }

    if !pane.active_context_present {
        reasons.push("active context row is unavailable".to_string());
        risk_tier = risk_tier.max(ContextHorizonRiskTier::Yellow);
        states.push(ContextHorizonEvidenceState::Inferred);
    }
    if pane.recent_rate_limit_events > 0 {
        reasons.push("recent rate-limit evidence is present".to_string());
        risk_tier = risk_tier.max(if pane.recent_rate_limit_events > 1 {
            ContextHorizonRiskTier::Red
        } else {
            ContextHorizonRiskTier::Yellow
        });
    }
    if pane.recent_compaction_events > 1 {
        reasons.push("repeated recent compaction evidence is present".to_string());
        risk_tier = risk_tier.max(ContextHorizonRiskTier::Red);
    }

    let last_rotation_age_ms = age_ms(generated_at_ms, pane.last_rotated_at_ms, &mut reasons);
    let last_activity_age_ms = age_ms(generated_at_ms, pane.last_activity_at_ms, &mut reasons);
    if last_activity_age_ms.is_some_and(|age| age > horizon_window_ms) {
        reasons.push("last activity evidence is stale for the horizon window".to_string());
        states.push(ContextHorizonEvidenceState::Stale);
    }
    if last_rotation_age_ms.is_some_and(|age| age > horizon_window_ms.saturating_mul(4)) {
        reasons.push("last rotation evidence is stale for the horizon window".to_string());
        states.push(ContextHorizonEvidenceState::Stale);
    }

    let previous_utilization = pane
        .previous_utilization
        .unwrap_or(utilization)
        .clamp(0.0, 1.0);
    let utilization_trend = (utilization - previous_utilization).clamp(-1.0, 1.0);
    if utilization_trend >= 0.15 {
        reasons.push("utilization is rising quickly".to_string());
        risk_tier = risk_tier.max(ContextHorizonRiskTier::Yellow);
    }

    let evidence_state = ContextHorizonEvidenceState::combine(states);
    let confidence =
        (1.0 - evidence_state.penalty() - reason_penalty(reasons.len())).clamp(0.05, 1.0);
    let compaction_count = sanitize_compaction_count(pane.compaction_count, &mut reasons);
    let citations = vec![ContextHorizonCitation {
        source: "robot.context.status".to_string(),
        pane_id: Some(pane.pane_id),
        evidence_state,
        generated_at_ms,
        fields: vec![
            "active_context_present".to_string(),
            "token_budget".to_string(),
            "tokens_consumed".to_string(),
            "pressure_tier".to_string(),
            "compaction_count".to_string(),
        ],
    }];

    ContextHorizonPaneRisk {
        pane_id: pane.pane_id,
        risk_tier,
        evidence_state,
        utilization,
        utilization_trend,
        active_context_present: pane.active_context_present,
        token_budget,
        tokens_consumed,
        compaction_count,
        last_rotation_age_ms,
        last_activity_age_ms,
        recent_rate_limit_events: pane.recent_rate_limit_events,
        recent_compaction_events: pane.recent_compaction_events,
        confidence,
        reasons,
        citations,
    }
}

fn sanitize_positive_u64(
    value: Option<i64>,
    field: &str,
    reasons: &mut Vec<String>,
) -> Option<u64> {
    match value {
        Some(value) if value > 0 => u64::try_from(value).ok(),
        Some(_) => {
            reasons.push(format!("{field} must be positive"));
            None
        }
        None => {
            reasons.push(format!("{field} is unavailable"));
            None
        }
    }
}

fn sanitize_nonnegative_u64(
    value: Option<i64>,
    field: &str,
    reasons: &mut Vec<String>,
) -> Option<u64> {
    match value {
        Some(value) if value >= 0 => u64::try_from(value).ok(),
        Some(_) => {
            reasons.push(format!("{field} must be non-negative"));
            None
        }
        None => {
            reasons.push(format!("{field} is unavailable"));
            None
        }
    }
}

fn sanitize_compaction_count(value: Option<i64>, reasons: &mut Vec<String>) -> u32 {
    match value {
        Some(value) if value >= 0 => u32::try_from(value).unwrap_or(u32::MAX),
        Some(_) => {
            reasons.push("compaction_count must be non-negative".to_string());
            0
        }
        None => {
            reasons.push("compaction_count is unavailable".to_string());
            0
        }
    }
}

fn age_ms(
    generated_at_ms: u64,
    timestamp_ms: Option<i64>,
    reasons: &mut Vec<String>,
) -> Option<u64> {
    match timestamp_ms {
        Some(value) if value >= 0 => u64::try_from(value)
            .ok()
            .map(|timestamp| generated_at_ms.saturating_sub(timestamp)),
        Some(_) => {
            reasons.push("timestamp evidence must be non-negative".to_string());
            None
        }
        None => None,
    }
}

fn reason_penalty(reason_count: usize) -> f64 {
    (reason_count.min(6) as f64) * 0.05
}

fn summarize_fleet(pane_risks: &[ContextHorizonPaneRisk]) -> ContextHorizonFleetSummary {
    let mut summary = ContextHorizonFleetSummary {
        total_panes: pane_risks.len(),
        green_count: 0,
        yellow_count: 0,
        red_count: 0,
        black_count: 0,
        measured_count: 0,
        stale_count: 0,
        unavailable_count: 0,
        highest_risk_tier: ContextHorizonRiskTier::Green,
    };

    for risk in pane_risks {
        summary.highest_risk_tier = summary.highest_risk_tier.max(risk.risk_tier);
        match risk.risk_tier {
            ContextHorizonRiskTier::Green => summary.green_count += 1,
            ContextHorizonRiskTier::Yellow => summary.yellow_count += 1,
            ContextHorizonRiskTier::Red => summary.red_count += 1,
            ContextHorizonRiskTier::Black => summary.black_count += 1,
        }
        match risk.evidence_state {
            ContextHorizonEvidenceState::Measured => summary.measured_count += 1,
            ContextHorizonEvidenceState::Stale => summary.stale_count += 1,
            ContextHorizonEvidenceState::Unavailable => summary.unavailable_count += 1,
            ContextHorizonEvidenceState::Mixed => {
                summary.stale_count +=
                    usize::from(risk.reasons.iter().any(|reason| reason.contains("stale")));
                summary.unavailable_count += usize::from(
                    risk.reasons
                        .iter()
                        .any(|reason| reason.contains("unavailable")),
                );
            }
            ContextHorizonEvidenceState::Inferred | ContextHorizonEvidenceState::Simulated => {}
        }
    }

    summary
}

fn build_recommendations(
    pane_risks: &[ContextHorizonPaneRisk],
    unavailable_domains: &[ContextHorizonUnavailableDomain],
) -> Vec<ContextHorizonRecommendation> {
    let mut recommendations = Vec::new();
    if !unavailable_domains.is_empty() {
        recommendations.push(ContextHorizonRecommendation {
            action_kind: ContextHorizonActionKind::RefreshEvidence,
            pane_id: None,
            priority: 1,
            reason: "one or more evidence domains are unavailable".to_string(),
            evidence_state: ContextHorizonEvidenceState::Unavailable,
            mutation_allowed: false,
        });
    }

    for risk in pane_risks {
        let (action_kind, priority, reason) = match risk.risk_tier {
            ContextHorizonRiskTier::Black => (
                ContextHorizonActionKind::PauseNewWork,
                1,
                "context horizon is critical; pause new work and prepare handoff",
            ),
            ContextHorizonRiskTier::Red => (
                ContextHorizonActionKind::PrepareHandoff,
                2,
                "context horizon is high risk; prepare handoff materials",
            ),
            ContextHorizonRiskTier::Yellow => (
                ContextHorizonActionKind::DryRunCompact,
                3,
                "context horizon is elevated; run a dry-run compaction advisor",
            ),
            ContextHorizonRiskTier::Green => (
                ContextHorizonActionKind::Monitor,
                4,
                "context horizon is currently normal",
            ),
        };
        if risk.risk_tier > ContextHorizonRiskTier::Green
            || matches!(
                risk.evidence_state,
                ContextHorizonEvidenceState::Stale
                    | ContextHorizonEvidenceState::Unavailable
                    | ContextHorizonEvidenceState::Mixed
            )
        {
            recommendations.push(ContextHorizonRecommendation {
                action_kind,
                pane_id: Some(risk.pane_id),
                priority,
                reason: reason.to_string(),
                evidence_state: risk.evidence_state,
                mutation_allowed: false,
            });
        }
    }

    recommendations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(pane_id: u64, consumed: i64, budget: i64) -> ContextHorizonPaneEvidence {
        ContextHorizonPaneEvidence {
            pane_id,
            active_context_present: true,
            token_budget: Some(budget),
            tokens_consumed: Some(consumed),
            pressure_tier: Some("green".to_string()),
            compaction_count: Some(0),
            last_rotated_at_ms: Some(1_000),
            last_activity_at_ms: Some(9_000),
            previous_utilization: None,
            recent_rate_limit_events: 0,
            recent_compaction_events: 0,
            evidence_state: ContextHorizonEvidenceState::Measured,
        }
    }

    #[test]
    fn context_horizon_predictor_classifies_thresholds_deterministically() {
        let input = ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![
                pane(1, 100, 1_000),
                pane(2, 750, 1_000),
                pane(3, 900, 1_000),
                pane(4, 990, 1_000),
            ],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        };

        let report = predict_context_horizon(&input);
        let tiers = report
            .pane_risks
            .iter()
            .map(|risk| risk.risk_tier)
            .collect::<Vec<_>>();

        assert_eq!(
            tiers,
            vec![
                ContextHorizonRiskTier::Green,
                ContextHorizonRiskTier::Yellow,
                ContextHorizonRiskTier::Red,
                ContextHorizonRiskTier::Black,
            ]
        );
        assert_eq!(report.fleet_summary.green_count, 1);
        assert_eq!(report.fleet_summary.yellow_count, 1);
        assert_eq!(report.fleet_summary.red_count, 1);
        assert_eq!(report.fleet_summary.black_count, 1);
        assert_eq!(report.evidence_state, ContextHorizonEvidenceState::Measured);
        assert!(!report.raw_context_content_stored);
    }

    #[test]
    fn context_horizon_empty_storage_fails_closed_as_unavailable() {
        let report = predict_context_horizon(&ContextHorizonInput::new(42));

        assert_eq!(
            report.evidence_state,
            ContextHorizonEvidenceState::Unavailable
        );
        assert_eq!(report.fleet_summary.total_panes, 0);
        assert_eq!(
            report.fleet_summary.highest_risk_tier,
            ContextHorizonRiskTier::Green
        );
        assert_eq!(report.unavailable_domains.len(), 1);
        assert_eq!(report.unavailable_domains[0].domain, "pane_contexts");
        assert!(
            report
                .recommendations
                .iter()
                .any(|rec| rec.action_kind == ContextHorizonActionKind::RefreshEvidence)
        );
    }

    #[test]
    fn context_horizon_malformed_counters_do_not_panic() {
        let input = ContextHorizonInput {
            generated_at_ms: 20_000,
            horizon_window_ms: 1_000,
            panes: vec![ContextHorizonPaneEvidence {
                pane_id: 7,
                active_context_present: false,
                token_budget: Some(-1),
                tokens_consumed: Some(-5),
                pressure_tier: Some("not-a-tier".to_string()),
                compaction_count: Some(-2),
                last_rotated_at_ms: Some(-1),
                last_activity_at_ms: None,
                previous_utilization: Some(2.0),
                recent_rate_limit_events: 1,
                recent_compaction_events: 0,
                evidence_state: ContextHorizonEvidenceState::Measured,
            }],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        };

        let report = predict_context_horizon(&input);
        let risk = &report.pane_risks[0];

        assert_eq!(risk.evidence_state, ContextHorizonEvidenceState::Mixed);
        assert!(risk.risk_tier >= ContextHorizonRiskTier::Yellow);
        assert_eq!(risk.token_budget, None);
        assert_eq!(risk.tokens_consumed, None);
        assert_eq!(risk.compaction_count, 0);
        assert!(risk.confidence < 1.0);
        assert!(
            risk.reasons
                .iter()
                .any(|reason| reason.contains("token_budget"))
        );
    }

    #[test]
    fn context_horizon_stale_activity_marks_stale_evidence() {
        let mut stale = pane(9, 300, 1_000);
        stale.last_activity_at_ms = Some(1_000);
        let input = ContextHorizonInput {
            generated_at_ms: 60_000,
            horizon_window_ms: 10_000,
            panes: vec![stale],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        };

        let report = predict_context_horizon(&input);
        let risk = &report.pane_risks[0];

        assert_eq!(risk.evidence_state, ContextHorizonEvidenceState::Mixed);
        assert_eq!(report.evidence_state, ContextHorizonEvidenceState::Mixed);
        assert!(risk.reasons.iter().any(|reason| reason.contains("stale")));
    }

    #[test]
    fn context_horizon_serialization_contains_no_raw_prompt_or_transcript_fields() {
        let report = predict_context_horizon(&ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![pane(1, 200, 1_000)],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        });
        let value = serde_json::to_value(&report).expect("report serializes");
        let mut disallowed = Vec::new();
        collect_disallowed_keys(&value, &mut disallowed);

        assert!(disallowed.is_empty(), "{disallowed:?}");
        assert_eq!(
            value["raw_context_content_stored"],
            serde_json::json!(false)
        );
        assert_eq!(
            value["redaction_policy"]["raw_prompt_allowed"],
            serde_json::json!(false)
        );
        assert_eq!(
            value["redaction_policy"]["raw_transcript_allowed"],
            serde_json::json!(false)
        );
    }

    fn collect_disallowed_keys(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if matches!(
                        key.as_str(),
                        "raw_prompt"
                            | "raw_transcript"
                            | "prompt_body"
                            | "pane_text"
                            | "raw_text"
                            | "transcript"
                    ) {
                        out.push(key.clone());
                    }
                    collect_disallowed_keys(value, out);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    collect_disallowed_keys(value, out);
                }
            }
            _ => {}
        }
    }
}
