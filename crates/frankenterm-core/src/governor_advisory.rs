//! ft-7h5da.7.8: a display-only `GovernorAdvisory` render schema.
//!
//! Ratified under ft-7h5da.12.2: the Adaptive Governor Mesh is *advisory
//! visibility*, NOT a unified `BudgetVerdict` decision chokepoint. This module
//! defines a single rendering/explanation record that each existing governor can
//! PRODUCE from its own native verdict, so an operator-facing surface (the W6
//! Attention Console) can show every governor's would-be posture in one shape.
//!
//! CRITICAL anti-coupling invariant: this is a DISPLAY type only. No governor
//! consumes another governor's advisory, and no decision logic reads one. The
//! producers below only READ each governor's native verdict and render it; they
//! introduce zero cross-governor decision dependency. (This is the hazard
//! flagged during the ft-7h5da.12.2 duel — a shared decision type would couple
//! the governors' control flow; a shared *display* type does not.)

use serde::{Deserialize, Serialize};

/// Coarse severity bucket shared across governors for at-a-glance rendering.
///
/// This is an explanation grouping, NOT a decision: a governor's real action is
/// always its own native verdict. Two governors mapping to the same severity
/// does not mean they agree on anything actionable — it only co-locates them in
/// a console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorySeverity {
    /// The governor would let the action proceed now.
    Allow,
    /// The governor would slow / defer / reduce the action (non-fatal).
    Throttle,
    /// The governor would refuse the action (reject / block class).
    RejectClass,
}

impl AdvisorySeverity {
    /// Stable snake_case token (matches the serde representation).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Throttle => "throttle",
            Self::RejectClass => "reject_class",
        }
    }
}

/// A display-only, per-governor advisory: what one governor's native verdict
/// would mean, rendered into a common shape for an operator console.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernorAdvisory {
    /// Stable identifier of the producing governor (e.g. `connector_governor`).
    pub governor: String,
    /// The governor's native verdict rendered as a stable snake_case token.
    pub verdict: String,
    /// Stable snake_case reason-code token explaining the verdict.
    pub reason_code: String,
    /// Short human-readable recommendation for the operator.
    pub recommendation: String,
    /// Coarse cross-governor severity bucket (display grouping only).
    pub severity: AdvisorySeverity,
}

impl GovernorAdvisory {
    fn new(
        governor: &'static str,
        verdict: impl Into<String>,
        reason_code: impl Into<String>,
        recommendation: impl Into<String>,
        severity: AdvisorySeverity,
    ) -> Self {
        Self {
            governor: governor.to_string(),
            verdict: verdict.into(),
            reason_code: reason_code.into(),
            recommendation: recommendation.into(),
            severity,
        }
    }
}

// ===========================================================================
// Per-governor producers. Each READS the governor's native verdict and renders
// it; none reads another governor's advisory or decision.
// ===========================================================================

/// Render a `connector_governor` decision as an advisory.
#[must_use]
pub fn connector_governor_advisory(
    decision: &crate::connector_governor::GovernorDecision,
) -> GovernorAdvisory {
    use crate::connector_governor::GovernorVerdict;
    let severity = match decision.verdict {
        GovernorVerdict::Allow => AdvisorySeverity::Allow,
        GovernorVerdict::Throttle => AdvisorySeverity::Throttle,
        GovernorVerdict::Reject => AdvisorySeverity::RejectClass,
    };
    let recommendation = match decision.verdict {
        GovernorVerdict::Allow => "dispatch now".to_string(),
        GovernorVerdict::Throttle => {
            format!("delay ~{}ms before dispatch", decision.delay_ms)
        }
        GovernorVerdict::Reject => "hold this action until limits clear".to_string(),
    };
    GovernorAdvisory::new(
        "connector_governor",
        decision.verdict.to_string(),
        decision.reason.to_string(),
        recommendation,
        severity,
    )
}

/// Render a `capacity_governor` decision as an advisory.
#[must_use]
pub fn capacity_governor_advisory(
    decision: &crate::capacity_governor::GovernorDecision,
) -> GovernorAdvisory {
    use crate::capacity_governor::GovernorDecision as D;
    let (verdict, severity) = match decision {
        D::Allow { .. } => ("allow", AdvisorySeverity::Allow),
        D::Throttle { .. } => ("throttle", AdvisorySeverity::Throttle),
        D::Offload { .. } => ("offload", AdvisorySeverity::Throttle),
        D::Block { .. } => ("block", AdvisorySeverity::RejectClass),
        // An operator override permits the workload regardless of pressure;
        // render it as an allow but keep the override visible as the verdict.
        D::Override { .. } => ("override", AdvisorySeverity::Allow),
    };
    GovernorAdvisory::new(
        "capacity_governor",
        verdict,
        verdict,
        decision.reason().to_string(),
        severity,
    )
}

/// Render a `fleet_memory_controller` recommended action as an advisory.
#[must_use]
pub fn fleet_memory_advisory(
    action: crate::fleet_memory_controller::FleetMemoryAction,
) -> GovernorAdvisory {
    use crate::fleet_memory_controller::FleetMemoryAction as A;
    let (verdict, recommendation, severity) = match action {
        A::None => (
            "none",
            "no memory intervention needed",
            AdvisorySeverity::Allow,
        ),
        A::ThrottlePolling => (
            "throttle_polling",
            "raise idle pane poll intervals to relieve memory",
            AdvisorySeverity::Throttle,
        ),
        A::EvictWarmScrollback => (
            "evict_warm_scrollback",
            "evict warm scrollback pages on idle panes",
            AdvisorySeverity::Throttle,
        ),
        A::PauseIdlePanes => (
            "pause_idle_panes",
            "pause output capture on lowest-priority panes",
            AdvisorySeverity::RejectClass,
        ),
        A::EmergencyCleanup => (
            "emergency_cleanup",
            "emergency: evict warm, pause most panes, trigger GC",
            AdvisorySeverity::RejectClass,
        ),
    };
    GovernorAdvisory::new(
        "fleet_memory_controller",
        verdict,
        verdict,
        recommendation,
        severity,
    )
}

/// The advisories of the governors the watcher runtime evaluates every health
/// tick: capture/write backpressure (its `GREEN`..`BLACK` tier), the fleet
/// memory controller (its most severe recommended action) and swarm admission.
/// Published in the health snapshot for read-only operator surfaces; no
/// decision path reads them (ft-7h5da.7.9).
#[must_use]
pub fn runtime_governor_advisories(
    backpressure_tier: Option<&str>,
    fleet_memory_actions: &[crate::fleet_memory_controller::FleetMemoryAction],
    admission: &crate::swarm_scheduler::ResourceAdmissionDecisionSummary,
) -> Vec<GovernorAdvisory> {
    let mut advisories = Vec::with_capacity(3);
    advisories.extend(backpressure_tier.and_then(backpressure_tier_advisory));
    advisories.push(fleet_memory_advisory(
        fleet_memory_actions
            .iter()
            .copied()
            .max()
            .unwrap_or(crate::fleet_memory_controller::FleetMemoryAction::None),
    ));
    advisories.push(swarm_admission_advisory(admission));
    advisories
}

/// Render the runtime's capture/write backpressure tier (`GREEN`, `YELLOW`,
/// `RED`, `BLACK`) as an advisory; `None` for an unrecognized tier.
#[must_use]
pub fn backpressure_tier_advisory(tier: &str) -> Option<GovernorAdvisory> {
    use crate::policy::PolicyRecommendationResourcePressure as P;
    let pressure = match tier.to_ascii_uppercase().as_str() {
        "GREEN" => P::Nominal,
        "YELLOW" => P::Elevated,
        "RED" | "BLACK" => P::Critical,
        _ => return None,
    };
    Some(backpressure_advisory(pressure))
}

/// The stable snake_case serde token of a unit enum variant.
fn serde_token<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(token)) => token,
        _ => "unknown".to_string(),
    }
}

/// Render the swarm admission controller's decision (the verdict fleet spawns
/// consult) as an advisory.
#[must_use]
pub fn swarm_admission_advisory(
    decision: &crate::swarm_scheduler::ResourceAdmissionDecisionSummary,
) -> GovernorAdvisory {
    use crate::swarm_scheduler::AdmissionAction as A;
    let (recommendation, severity) = match decision.action {
        A::Admit => ("admit new agent spawns", AdvisorySeverity::Allow),
        A::Defer => (
            "defer new spawns until pressure drops",
            AdvisorySeverity::Throttle,
        ),
        A::Degrade => (
            "spawn only in a reduced-quality mode",
            AdvisorySeverity::Throttle,
        ),
        A::Shed => (
            "refuse new spawns; the host is saturated",
            AdvisorySeverity::RejectClass,
        ),
    };
    let verdict = serde_token(&decision.action);
    let reason_code = decision
        .reason_codes
        .first()
        .map_or_else(|| verdict.clone(), serde_token);
    GovernorAdvisory::new(
        "swarm_admission",
        verdict,
        reason_code,
        recommendation,
        severity,
    )
}

/// Render a `backpressure` resource-pressure signal as an advisory.
#[must_use]
pub fn backpressure_advisory(
    pressure: crate::policy::PolicyRecommendationResourcePressure,
) -> GovernorAdvisory {
    use crate::policy::PolicyRecommendationResourcePressure as P;
    let (recommendation, severity) = match pressure {
        P::Nominal => ("proceed; no backpressure", AdvisorySeverity::Allow),
        P::Elevated => (
            "proceed with caution under elevated pressure",
            AdvisorySeverity::Throttle,
        ),
        P::Critical => (
            "delay new work under critical pressure",
            AdvisorySeverity::RejectClass,
        ),
    };
    GovernorAdvisory::new(
        "backpressure",
        pressure.as_str(),
        pressure.as_str(),
        recommendation,
        severity,
    )
}

/// Render an `operating_envelope` admission outcome as an advisory.
///
/// Takes the decision's native `outcome` (its verdict) plus its `reason_codes`
/// directly — callers pass `decision.outcome` and `&decision.reason_codes` —
/// since the rest of `OperatingEnvelopeDecision` (tiers, parallelism caps) is
/// not part of the rendered advisory.
#[must_use]
pub fn operating_envelope_advisory(
    outcome: crate::operating_envelope::OperatingEnvelopeOutcome,
    reason_codes: &[String],
) -> GovernorAdvisory {
    use crate::operating_envelope::OperatingEnvelopeOutcome as O;
    let (verdict, recommendation, severity) = match outcome {
        O::Admit => (
            "admit",
            "admit at the current envelope",
            AdvisorySeverity::Allow,
        ),
        O::Defer => (
            "defer",
            "defer until conditions improve",
            AdvisorySeverity::Throttle,
        ),
        O::Degrade => (
            "degrade",
            "proceed at a reduced envelope",
            AdvisorySeverity::Throttle,
        ),
        O::Shed => (
            "shed",
            "shed load to relieve pressure",
            AdvisorySeverity::Throttle,
        ),
        O::Wait => (
            "wait",
            "wait for an admission window",
            AdvisorySeverity::Throttle,
        ),
        O::Block => (
            "block",
            "block until the envelope recovers",
            AdvisorySeverity::RejectClass,
        ),
    };
    // Reason: the first native reason code, else the outcome token.
    let reason_code = reason_codes
        .first()
        .cloned()
        .unwrap_or_else(|| verdict.to_string());
    GovernorAdvisory::new(
        "operating_envelope",
        verdict,
        reason_code,
        recommendation,
        severity,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail_closed_admission() -> crate::swarm_scheduler::ResourceAdmissionDecisionSummary {
        use crate::swarm_scheduler::{
            AdmissionRequest, SwarmAdmissionController, SwarmAdmissionTelemetry,
        };
        SwarmAdmissionController::default().evaluate(
            &AdmissionRequest::standard(2, 1),
            &SwarmAdmissionTelemetry {
                queue_pressure: None,
                fleet_pressure: None,
                memory_tier_budget: None,
                latency_stage_pressures: None,
                herd_wave_pressure: None,
            },
        )
    }

    #[test]
    fn swarm_admission_advisory_mirrors_the_native_verdict() {
        use crate::swarm_scheduler::AdmissionAction;
        let decision = fail_closed_admission();
        let advisory = swarm_admission_advisory(&decision);
        assert_eq!(advisory.governor, "swarm_admission");
        assert_eq!(advisory.verdict, serde_token(&decision.action));
        assert_eq!(
            advisory.reason_code,
            decision
                .reason_codes
                .first()
                .map_or_else(|| advisory.verdict.clone(), serde_token)
        );
        let expected = match decision.action {
            AdmissionAction::Admit => AdvisorySeverity::Allow,
            AdmissionAction::Defer | AdmissionAction::Degrade => AdvisorySeverity::Throttle,
            AdmissionAction::Shed => AdvisorySeverity::RejectClass,
        };
        assert_eq!(advisory.severity, expected);
    }

    #[test]
    fn runtime_advisories_cover_each_live_governor_and_its_worst_action() {
        use crate::fleet_memory_controller::FleetMemoryAction;
        let admission = fail_closed_admission();

        let quiet = runtime_governor_advisories(Some("GREEN"), &[], &admission);
        let governors: Vec<&str> = quiet.iter().map(|a| a.governor.as_str()).collect();
        assert_eq!(
            governors,
            ["backpressure", "fleet_memory_controller", "swarm_admission"]
        );
        assert_eq!(quiet[0].severity, AdvisorySeverity::Allow);
        assert_eq!(quiet[1].verdict, "none");

        let pressed = runtime_governor_advisories(
            Some("BLACK"),
            &[
                FleetMemoryAction::ThrottlePolling,
                FleetMemoryAction::PauseIdlePanes,
                FleetMemoryAction::EvictWarmScrollback,
            ],
            &admission,
        );
        assert_eq!(pressed[0].severity, AdvisorySeverity::RejectClass);
        assert_eq!(pressed[1].verdict, "pause_idle_panes");
        assert_eq!(pressed[1].severity, AdvisorySeverity::RejectClass);

        // No queue capacities, no backpressure tier: that governor is absent.
        let untiered = runtime_governor_advisories(None, &[], &admission);
        assert_eq!(untiered.len(), 2);
        assert_eq!(untiered[0].governor, "fleet_memory_controller");
    }

    #[test]
    fn severity_tokens_are_stable_snake_case() {
        assert_eq!(AdvisorySeverity::Allow.as_str(), "allow");
        assert_eq!(AdvisorySeverity::Throttle.as_str(), "throttle");
        assert_eq!(AdvisorySeverity::RejectClass.as_str(), "reject_class");
        // serde representation matches the stable token.
        assert_eq!(
            serde_json::to_string(&AdvisorySeverity::RejectClass).unwrap(),
            "\"reject_class\""
        );
    }

    #[test]
    fn advisory_round_trips_through_serde() {
        let a = GovernorAdvisory::new(
            "connector_governor",
            "throttle",
            "global_rate_limit",
            "delay ~50ms before dispatch",
            AdvisorySeverity::Throttle,
        );
        let json = serde_json::to_string(&a).unwrap();
        let back: GovernorAdvisory = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
        assert!(json.contains("\"severity\":\"throttle\""));
    }

    #[test]
    fn connector_governor_allow_and_reject_render() {
        use crate::connector_governor::{GovernorDecision, GovernorReason, GovernorVerdict};

        let allow = GovernorDecision::allow("github", "fetch", 1_000);
        let a = connector_governor_advisory(&allow);
        assert_eq!(a.governor, "connector_governor");
        assert_eq!(a.verdict, "allow");
        assert_eq!(a.reason_code, "clear");
        assert_eq!(a.severity, AdvisorySeverity::Allow);

        let reject = GovernorDecision {
            verdict: GovernorVerdict::Reject,
            reason: GovernorReason::BudgetExceeded,
            delay_ms: 0,
            connector_id: "github".to_string(),
            action_kind: "fetch".to_string(),
            decided_at_ms: 1_000,
        };
        let r = connector_governor_advisory(&reject);
        assert_eq!(r.verdict, "reject");
        assert_eq!(r.reason_code, "budget_exceeded");
        assert_eq!(r.severity, AdvisorySeverity::RejectClass);
    }

    #[test]
    fn capacity_governor_variants_map_to_severity() {
        use crate::capacity_governor::GovernorDecision as D;

        let throttle = capacity_governor_advisory(&D::Throttle {
            delay_ms: 250,
            reason: "queue depth high".to_string(),
        });
        assert_eq!(throttle.governor, "capacity_governor");
        assert_eq!(throttle.verdict, "throttle");
        assert_eq!(throttle.recommendation, "queue depth high");
        assert_eq!(throttle.severity, AdvisorySeverity::Throttle);

        let block = capacity_governor_advisory(&D::Block {
            reason: "no slots".to_string(),
        });
        assert_eq!(block.verdict, "block");
        assert_eq!(block.severity, AdvisorySeverity::RejectClass);

        let over = capacity_governor_advisory(&D::Override {
            operator: "jeff".to_string(),
            reason: "manual override".to_string(),
            original_decision: Box::new(D::Block {
                reason: "no slots".to_string(),
            }),
        });
        assert_eq!(over.verdict, "override");
        assert_eq!(over.severity, AdvisorySeverity::Allow);
    }

    #[test]
    fn fleet_memory_actions_map_to_severity() {
        use crate::fleet_memory_controller::FleetMemoryAction as A;
        assert_eq!(
            fleet_memory_advisory(A::None).severity,
            AdvisorySeverity::Allow
        );
        let throttle = fleet_memory_advisory(A::ThrottlePolling);
        assert_eq!(throttle.verdict, "throttle_polling");
        assert_eq!(throttle.severity, AdvisorySeverity::Throttle);
        assert_eq!(
            fleet_memory_advisory(A::EmergencyCleanup).severity,
            AdvisorySeverity::RejectClass
        );
    }

    #[test]
    fn backpressure_pressure_maps_to_severity() {
        use crate::policy::PolicyRecommendationResourcePressure as P;
        assert_eq!(
            backpressure_advisory(P::Nominal).severity,
            AdvisorySeverity::Allow
        );
        let critical = backpressure_advisory(P::Critical);
        assert_eq!(critical.governor, "backpressure");
        assert_eq!(critical.verdict, "critical");
        assert_eq!(critical.severity, AdvisorySeverity::RejectClass);
    }

    #[test]
    fn operating_envelope_outcomes_map_to_severity() {
        use crate::operating_envelope::OperatingEnvelopeOutcome as O;

        let admit = operating_envelope_advisory(O::Admit, &[]);
        assert_eq!(admit.governor, "operating_envelope");
        assert_eq!(admit.verdict, "admit");
        // No native reason code => the outcome token is used.
        assert_eq!(admit.reason_code, "admit");
        assert_eq!(admit.severity, AdvisorySeverity::Allow);

        let reasons = vec!["fail_closed.lower_missing".to_string()];
        let block = operating_envelope_advisory(O::Block, &reasons);
        assert_eq!(block.verdict, "block");
        assert_eq!(block.reason_code, "fail_closed.lower_missing");
        assert_eq!(block.severity, AdvisorySeverity::RejectClass);

        for outcome in [O::Defer, O::Degrade, O::Shed, O::Wait] {
            assert_eq!(
                operating_envelope_advisory(outcome, &[]).severity,
                AdvisorySeverity::Throttle
            );
        }
    }
}
