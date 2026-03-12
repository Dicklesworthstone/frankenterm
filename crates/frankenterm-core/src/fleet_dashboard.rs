//! Fleet-level dashboards and alerting for multi-subsystem health observability.
//!
//! Consumes [`UnifiedFleetSnapshot`] from the unified telemetry schema and
//! produces actionable fleet health views, severity-routed alerts with runbook
//! linkage, and operator acknowledgement/resolution tracking.
//!
//! # Design
//!
//! - **FleetAlertPolicy**: Configurable alert rules evaluated against fleet snapshots.
//! - **FleetAlertManager**: Alert lifecycle (fire → ack → resolve) with deduplication.
//! - **FleetDashboardView**: Aggregated fleet health summary for operator dashboards.
//!
//! # Bead
//!
//! Implements ft-3681t.7.2 — real-time dashboards and alerting for fleet health.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::unified_telemetry::{
    HealthStatus, RedactionLabel, SubsystemLayer, UnifiedFleetSnapshot,
};

// ---------------------------------------------------------------------------
// Alert severity and routing
// ---------------------------------------------------------------------------

/// Severity level for fleet alerts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetAlertSeverity {
    /// Informational — no action required.
    Info,
    /// Warning — operator attention recommended.
    Warning,
    /// Critical — immediate operator attention required.
    Critical,
    /// Emergency — automated escalation triggered.
    Emergency,
}

/// Routing target for alert delivery.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertRoute {
    /// Log only (no active notification).
    Log,
    /// Dashboard badge / status bar indicator.
    Dashboard,
    /// Agent mail notification.
    AgentMail { recipient: String },
    /// Webhook POST.
    Webhook { url: String },
}

/// Runbook reference attached to an alert class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookRef {
    /// Runbook identifier (e.g. "RB-001").
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Link to documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_link: Option<String>,
    /// Remediation steps summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<String>,
}

// ---------------------------------------------------------------------------
// Alert policy
// ---------------------------------------------------------------------------

/// Alert class identifier (e.g. "fleet.health.degraded").
pub type AlertClassId = String;

/// A configurable alert policy rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetAlertPolicy {
    /// Unique alert class identifier.
    pub class_id: AlertClassId,
    /// Human-readable description of what this alert detects.
    pub description: String,
    /// Severity when this alert fires.
    pub severity: FleetAlertSeverity,
    /// Routing targets for this alert class.
    pub routes: Vec<AlertRoute>,
    /// The condition that triggers this alert.
    pub condition: AlertCondition,
    /// Optional runbook for operators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runbook: Option<RunbookRef>,
    /// Minimum seconds between re-fires of the same alert (dedup window).
    #[serde(default = "default_dedup_window")]
    pub dedup_window_secs: u64,
    /// Whether this policy is enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_dedup_window() -> u64 {
    300
}

fn default_enabled() -> bool {
    true
}

/// Conditions that can trigger a fleet alert.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AlertCondition {
    /// Fleet-wide health drops to or below this status.
    FleetHealthBelow { threshold: HealthStatus },
    /// A specific subsystem layer's health drops to or below this status.
    LayerHealthBelow {
        layer: SubsystemLayer,
        threshold: HealthStatus,
    },
    /// Redaction ceiling reaches this level or higher (data leak risk).
    RedactionCeilingAbove { threshold: RedactionLabel },
    /// Number of unhealthy envelopes exceeds this count.
    UnhealthyEnvelopeCount { max_count: usize },
    /// Number of degraded-or-worse envelopes exceeds this count.
    DegradedEnvelopeCount { max_count: usize },
}

impl AlertCondition {
    /// Evaluate this condition against a fleet snapshot.
    pub fn evaluate(&self, snapshot: &UnifiedFleetSnapshot) -> bool {
        match self {
            Self::FleetHealthBelow { threshold } => {
                health_at_or_below(snapshot.fleet_health, *threshold)
            }
            Self::LayerHealthBelow { layer, threshold } => {
                let key = serde_json::to_value(layer)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| format!("{layer:?}"));
                snapshot
                    .layer_health
                    .get(&key)
                    .map_or(false, |h| health_at_or_below(*h, *threshold))
            }
            Self::RedactionCeilingAbove { threshold } => {
                snapshot.redaction_ceiling >= *threshold
            }
            Self::UnhealthyEnvelopeCount { max_count } => {
                let count = snapshot
                    .envelopes
                    .iter()
                    .filter(|e| e.health == HealthStatus::Unhealthy)
                    .count();
                count > *max_count
            }
            Self::DegradedEnvelopeCount { max_count } => {
                let count = snapshot
                    .envelopes
                    .iter()
                    .filter(|e| {
                        matches!(e.health, HealthStatus::Degraded | HealthStatus::Unhealthy)
                    })
                    .count();
                count > *max_count
            }
        }
    }
}

/// Returns true when `status` is at or worse than `threshold`.
fn health_at_or_below(status: HealthStatus, threshold: HealthStatus) -> bool {
    // Ordering: Healthy > Degraded > Unknown > Unhealthy
    let rank = |h: HealthStatus| match h {
        HealthStatus::Healthy => 3,
        HealthStatus::Degraded => 2,
        HealthStatus::Unknown => 1,
        HealthStatus::Unhealthy => 0,
    };
    rank(status) <= rank(threshold)
}

// ---------------------------------------------------------------------------
// Fired alert
// ---------------------------------------------------------------------------

/// A fired alert instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetAlert {
    /// Monotonic alert instance ID within this manager.
    pub alert_id: u64,
    /// The policy class that triggered this alert.
    pub class_id: AlertClassId,
    /// Alert severity (copied from policy at fire time).
    pub severity: FleetAlertSeverity,
    /// Human-readable summary of what triggered the alert.
    pub summary: String,
    /// When the alert was fired (epoch ms).
    pub fired_at_ms: u64,
    /// When the alert was acknowledged (epoch ms), if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at_ms: Option<u64>,
    /// Who acknowledged the alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_by: Option<String>,
    /// When the alert was resolved (epoch ms), if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at_ms: Option<u64>,
    /// Resolution note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_note: Option<String>,
    /// Runbook reference from the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runbook: Option<RunbookRef>,
    /// Snapshot context at fire time (fleet health, layer health).
    pub context: AlertContext,
}

/// Contextual data captured when an alert fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertContext {
    pub fleet_health: HealthStatus,
    pub layer_health: HashMap<String, HealthStatus>,
    pub redaction_ceiling: RedactionLabel,
    pub envelope_count: usize,
}

impl FleetAlert {
    /// Whether this alert is still active (not resolved).
    pub fn is_active(&self) -> bool {
        self.resolved_at_ms.is_none()
    }

    /// Whether this alert has been acknowledged.
    pub fn is_acked(&self) -> bool {
        self.acked_at_ms.is_some()
    }
}

// ---------------------------------------------------------------------------
// Alert manager
// ---------------------------------------------------------------------------

/// Manages the fleet alert lifecycle: evaluate → fire → deduplicate → ack → resolve.
#[derive(Debug)]
pub struct FleetAlertManager {
    policies: Vec<FleetAlertPolicy>,
    alerts: Vec<FleetAlert>,
    next_id: u64,
    /// Last fire time per class_id for deduplication.
    last_fired: HashMap<AlertClassId, u64>,
}

impl FleetAlertManager {
    /// Create a new alert manager with the given policies.
    pub fn new(policies: Vec<FleetAlertPolicy>) -> Self {
        Self {
            policies,
            alerts: Vec::new(),
            next_id: 1,
            last_fired: HashMap::new(),
        }
    }

    /// Create a manager with built-in default policies.
    pub fn with_defaults() -> Self {
        Self::new(default_policies())
    }

    /// Evaluate all enabled policies against a fleet snapshot.
    /// Returns newly fired alerts (after dedup).
    pub fn evaluate(&mut self, snapshot: &UnifiedFleetSnapshot) -> Vec<&FleetAlert> {
        let now_ms = epoch_ms();
        let mut new_alerts = Vec::new();

        for policy in &self.policies {
            if !policy.enabled {
                continue;
            }
            if !policy.condition.evaluate(snapshot) {
                continue;
            }
            // Dedup check
            if let Some(&last) = self.last_fired.get(&policy.class_id) {
                if now_ms.saturating_sub(last) < policy.dedup_window_secs * 1000 {
                    continue;
                }
            }

            let alert = FleetAlert {
                alert_id: self.next_id,
                class_id: policy.class_id.clone(),
                severity: policy.severity,
                summary: format_alert_summary(policy, snapshot),
                fired_at_ms: now_ms,
                acked_at_ms: None,
                acked_by: None,
                resolved_at_ms: None,
                resolution_note: None,
                runbook: policy.runbook.clone(),
                context: AlertContext {
                    fleet_health: snapshot.fleet_health,
                    layer_health: snapshot.layer_health.clone(),
                    redaction_ceiling: snapshot.redaction_ceiling,
                    envelope_count: snapshot.envelope_count(),
                },
            };

            self.next_id += 1;
            self.last_fired.insert(policy.class_id.clone(), now_ms);
            self.alerts.push(alert);
            new_alerts.push(self.alerts.len() - 1);
        }

        new_alerts.iter().map(|&i| &self.alerts[i]).collect()
    }

    /// Acknowledge an alert by ID.
    pub fn acknowledge(&mut self, alert_id: u64, by: &str) -> bool {
        if let Some(alert) = self.alerts.iter_mut().find(|a| a.alert_id == alert_id) {
            if alert.acked_at_ms.is_none() {
                alert.acked_at_ms = Some(epoch_ms());
                alert.acked_by = Some(by.to_string());
                return true;
            }
        }
        false
    }

    /// Resolve an alert by ID.
    pub fn resolve(&mut self, alert_id: u64, note: &str) -> bool {
        if let Some(alert) = self.alerts.iter_mut().find(|a| a.alert_id == alert_id) {
            if alert.resolved_at_ms.is_none() {
                alert.resolved_at_ms = Some(epoch_ms());
                alert.resolution_note = Some(note.to_string());
                return true;
            }
        }
        false
    }

    /// All active (unresolved) alerts.
    pub fn active_alerts(&self) -> Vec<&FleetAlert> {
        self.alerts.iter().filter(|a| a.is_active()).collect()
    }

    /// All alerts (including resolved) for the audit trail.
    pub fn all_alerts(&self) -> &[FleetAlert] {
        &self.alerts
    }

    /// Count of active alerts by severity.
    pub fn active_counts_by_severity(&self) -> HashMap<FleetAlertSeverity, usize> {
        let mut counts = HashMap::new();
        for alert in self.alerts.iter().filter(|a| a.is_active()) {
            *counts.entry(alert.severity).or_insert(0) += 1;
        }
        counts
    }

    /// Reference to configured policies.
    pub fn policies(&self) -> &[FleetAlertPolicy] {
        &self.policies
    }

    /// Add a policy at runtime.
    pub fn add_policy(&mut self, policy: FleetAlertPolicy) {
        self.policies.push(policy);
    }
}

// ---------------------------------------------------------------------------
// Fleet dashboard view
// ---------------------------------------------------------------------------

/// Aggregated fleet dashboard view for operators.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetDashboardView {
    /// When this view was computed (epoch ms).
    pub computed_at_ms: u64,
    /// Overall fleet health (worst across all layers).
    pub fleet_health: HealthStatus,
    /// Per-layer health summary.
    pub layer_health: HashMap<String, HealthStatus>,
    /// Per-layer envelope counts.
    pub layer_envelope_counts: HashMap<String, usize>,
    /// Redaction ceiling.
    pub redaction_ceiling: RedactionLabel,
    /// Total envelope count.
    pub total_envelopes: usize,
    /// Count of causality links.
    pub causality_link_count: usize,
    /// Active alert summary (count by severity).
    pub active_alert_counts: HashMap<FleetAlertSeverity, usize>,
    /// Most critical active alert (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub most_critical_alert: Option<FleetAlertSummary>,
    /// Compact one-line status.
    pub summary_line: String,
}

/// Compact alert summary for dashboard display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetAlertSummary {
    pub alert_id: u64,
    pub class_id: String,
    pub severity: FleetAlertSeverity,
    pub summary: String,
    pub fired_at_ms: u64,
}

impl FleetDashboardView {
    /// Build a dashboard view from a fleet snapshot and alert manager state.
    pub fn from_snapshot(
        snapshot: &UnifiedFleetSnapshot,
        manager: &FleetAlertManager,
    ) -> Self {
        let now_ms = epoch_ms();

        let mut layer_envelope_counts: HashMap<String, usize> = HashMap::new();
        for env in &snapshot.envelopes {
            let key = serde_json::to_value(&env.layer)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| format!("{:?}", env.layer));
            *layer_envelope_counts.entry(key).or_insert(0) += 1;
        }

        let active_alert_counts = manager.active_counts_by_severity();

        let most_critical_alert = manager
            .active_alerts()
            .into_iter()
            .max_by_key(|a| a.severity)
            .map(|a| FleetAlertSummary {
                alert_id: a.alert_id,
                class_id: a.class_id.clone(),
                severity: a.severity,
                summary: a.summary.clone(),
                fired_at_ms: a.fired_at_ms,
            });

        let total_active: usize = active_alert_counts.values().sum();
        let summary_line = format!(
            "Fleet: {:?} | {} envelopes | {} active alerts | redaction: {:?}",
            snapshot.fleet_health,
            snapshot.envelope_count(),
            total_active,
            snapshot.redaction_ceiling,
        );

        Self {
            computed_at_ms: now_ms,
            fleet_health: snapshot.fleet_health,
            layer_health: snapshot.layer_health.clone(),
            layer_envelope_counts,
            redaction_ceiling: snapshot.redaction_ceiling,
            total_envelopes: snapshot.envelope_count(),
            causality_link_count: snapshot.causality.len(),
            active_alert_counts,
            most_critical_alert,
            summary_line,
        }
    }

    /// True when there are any critical or emergency alerts.
    pub fn has_critical_alerts(&self) -> bool {
        self.active_alert_counts
            .iter()
            .any(|(s, c)| *c > 0 && *s >= FleetAlertSeverity::Critical)
    }
}

// ---------------------------------------------------------------------------
// Default policies
// ---------------------------------------------------------------------------

/// Built-in alert policies covering common fleet failure modes.
pub fn default_policies() -> Vec<FleetAlertPolicy> {
    vec![
        FleetAlertPolicy {
            class_id: "fleet.health.unhealthy".into(),
            description: "Fleet overall health is unhealthy".into(),
            severity: FleetAlertSeverity::Critical,
            routes: vec![AlertRoute::Dashboard, AlertRoute::Log],
            condition: AlertCondition::FleetHealthBelow {
                threshold: HealthStatus::Unhealthy,
            },
            runbook: Some(RunbookRef {
                id: "RB-FLEET-001".into(),
                title: "Fleet Health Unhealthy".into(),
                doc_link: Some("docs/runbooks/fleet-unhealthy.md".into()),
                steps: vec![
                    "Check ft doctor --json for subsystem health".into(),
                    "Review active alerts for root cause".into(),
                    "Check mux pool connectivity".into(),
                    "Verify policy engine is not in kill-switch mode".into(),
                ],
            }),
            dedup_window_secs: 300,
            enabled: true,
        },
        FleetAlertPolicy {
            class_id: "fleet.health.degraded".into(),
            description: "Fleet overall health is degraded".into(),
            severity: FleetAlertSeverity::Warning,
            routes: vec![AlertRoute::Dashboard],
            condition: AlertCondition::FleetHealthBelow {
                threshold: HealthStatus::Degraded,
            },
            runbook: Some(RunbookRef {
                id: "RB-FLEET-002".into(),
                title: "Fleet Health Degraded".into(),
                doc_link: Some("docs/runbooks/fleet-degraded.md".into()),
                steps: vec![
                    "Identify which layer is degraded via layer_health".into(),
                    "Check connector reliability snapshots".into(),
                    "Monitor for escalation to unhealthy".into(),
                ],
            }),
            dedup_window_secs: 600,
            enabled: true,
        },
        FleetAlertPolicy {
            class_id: "fleet.policy.unhealthy".into(),
            description: "Policy engine subsystem is unhealthy".into(),
            severity: FleetAlertSeverity::Critical,
            routes: vec![AlertRoute::Dashboard, AlertRoute::Log],
            condition: AlertCondition::LayerHealthBelow {
                layer: SubsystemLayer::Policy,
                threshold: HealthStatus::Unhealthy,
            },
            runbook: Some(RunbookRef {
                id: "RB-POLICY-001".into(),
                title: "Policy Engine Unhealthy".into(),
                doc_link: Some("docs/runbooks/policy-unhealthy.md".into()),
                steps: vec![
                    "Check policy diagnostics via ft doctor".into(),
                    "Verify quarantine state".into(),
                    "Check kill-switch status".into(),
                ],
            }),
            dedup_window_secs: 300,
            enabled: true,
        },
        FleetAlertPolicy {
            class_id: "fleet.mux.unhealthy".into(),
            description: "Mux connection pool is unhealthy".into(),
            severity: FleetAlertSeverity::Critical,
            routes: vec![AlertRoute::Dashboard, AlertRoute::Log],
            condition: AlertCondition::LayerHealthBelow {
                layer: SubsystemLayer::Mux,
                threshold: HealthStatus::Unhealthy,
            },
            runbook: Some(RunbookRef {
                id: "RB-MUX-001".into(),
                title: "Mux Pool Unhealthy".into(),
                doc_link: Some("docs/runbooks/mux-unhealthy.md".into()),
                steps: vec![
                    "Check mux socket connectivity".into(),
                    "Verify WezTerm/backend process is running".into(),
                    "Review pool stats for connection failures".into(),
                ],
            }),
            dedup_window_secs: 300,
            enabled: true,
        },
        FleetAlertPolicy {
            class_id: "fleet.redaction.pii_leak_risk".into(),
            description: "Redaction ceiling at PII level — data leak risk".into(),
            severity: FleetAlertSeverity::Emergency,
            routes: vec![AlertRoute::Dashboard, AlertRoute::Log],
            condition: AlertCondition::RedactionCeilingAbove {
                threshold: RedactionLabel::Pii,
            },
            runbook: Some(RunbookRef {
                id: "RB-REDACT-001".into(),
                title: "PII Redaction Ceiling".into(),
                doc_link: Some("docs/runbooks/pii-leak-risk.md".into()),
                steps: vec![
                    "Identify which subsystem is emitting PII-classified data".into(),
                    "Check scrubbed_fields in envelope redaction metadata".into(),
                    "Verify credential broker is not leaking secrets".into(),
                ],
            }),
            dedup_window_secs: 60,
            enabled: true,
        },
        FleetAlertPolicy {
            class_id: "fleet.envelope.many_unhealthy".into(),
            description: "Multiple subsystem envelopes are unhealthy".into(),
            severity: FleetAlertSeverity::Warning,
            routes: vec![AlertRoute::Dashboard],
            condition: AlertCondition::UnhealthyEnvelopeCount { max_count: 2 },
            runbook: None,
            dedup_window_secs: 300,
            enabled: true,
        },
    ]
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_alert_summary(policy: &FleetAlertPolicy, snapshot: &UnifiedFleetSnapshot) -> String {
    format!(
        "{}: fleet_health={:?}, envelopes={}, redaction={:?}",
        policy.description,
        snapshot.fleet_health,
        snapshot.envelope_count(),
        snapshot.redaction_ceiling,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unified_telemetry::{
        EnvelopeBuilder, IngestPayload, SubsystemPayload,
    };

    fn sample_time() -> u64 {
        1_710_000_000_000
    }

    fn make_ingest_payload() -> SubsystemPayload {
        SubsystemPayload::Ingest(IngestPayload {
            snapshot: crate::tailer::SchedulerSnapshot {
                budget_active: false,
                max_captures_per_sec: 0,
                max_bytes_per_sec: 0,
                captures_remaining: 0,
                bytes_remaining: 0,
                total_rate_limited: 0,
                total_byte_budget_exceeded: 0,
                total_throttle_events: 0,
                tracked_panes: 0,
            },
        })
    }

    fn healthy_snapshot() -> UnifiedFleetSnapshot {
        let env = EnvelopeBuilder::new(SubsystemLayer::Policy, sample_time())
            .health(HealthStatus::Healthy)
            .build(make_ingest_payload());
        UnifiedFleetSnapshot::from_envelopes(sample_time(), vec![env], vec![])
    }

    fn degraded_snapshot() -> UnifiedFleetSnapshot {
        let e1 = EnvelopeBuilder::new(SubsystemLayer::Policy, sample_time())
            .health(HealthStatus::Healthy)
            .build(make_ingest_payload());
        let e2 = EnvelopeBuilder::new(SubsystemLayer::Mux, sample_time())
            .health(HealthStatus::Degraded)
            .build(make_ingest_payload());
        UnifiedFleetSnapshot::from_envelopes(sample_time(), vec![e1, e2], vec![])
    }

    fn unhealthy_snapshot() -> UnifiedFleetSnapshot {
        let e1 = EnvelopeBuilder::new(SubsystemLayer::Policy, sample_time())
            .health(HealthStatus::Unhealthy)
            .build(make_ingest_payload());
        let e2 = EnvelopeBuilder::new(SubsystemLayer::Mux, sample_time())
            .health(HealthStatus::Unhealthy)
            .build(make_ingest_payload());
        let e3 = EnvelopeBuilder::new(SubsystemLayer::Swarm, sample_time())
            .health(HealthStatus::Unhealthy)
            .build(make_ingest_payload());
        UnifiedFleetSnapshot::from_envelopes(sample_time(), vec![e1, e2, e3], vec![])
    }

    // -- AlertCondition evaluation --

    #[test]
    fn condition_fleet_health_below_matches_degraded() {
        let snap = degraded_snapshot();
        let cond = AlertCondition::FleetHealthBelow {
            threshold: HealthStatus::Degraded,
        };
        assert!(cond.evaluate(&snap));
    }

    #[test]
    fn condition_fleet_health_below_no_match_healthy() {
        let snap = healthy_snapshot();
        let cond = AlertCondition::FleetHealthBelow {
            threshold: HealthStatus::Degraded,
        };
        assert!(!cond.evaluate(&snap));
    }

    #[test]
    fn condition_layer_health_below_matches() {
        let snap = degraded_snapshot();
        let cond = AlertCondition::LayerHealthBelow {
            layer: SubsystemLayer::Mux,
            threshold: HealthStatus::Degraded,
        };
        assert!(cond.evaluate(&snap));
    }

    #[test]
    fn condition_layer_health_below_no_match() {
        let snap = degraded_snapshot();
        let cond = AlertCondition::LayerHealthBelow {
            layer: SubsystemLayer::Policy,
            threshold: HealthStatus::Degraded,
        };
        assert!(!cond.evaluate(&snap));
    }

    #[test]
    fn condition_redaction_ceiling_above_matches() {
        let mut snap = healthy_snapshot();
        snap.redaction_ceiling = RedactionLabel::Pii;
        let cond = AlertCondition::RedactionCeilingAbove {
            threshold: RedactionLabel::Sensitive,
        };
        assert!(cond.evaluate(&snap));
    }

    #[test]
    fn condition_redaction_ceiling_above_no_match() {
        let snap = healthy_snapshot();
        let cond = AlertCondition::RedactionCeilingAbove {
            threshold: RedactionLabel::Sensitive,
        };
        assert!(!cond.evaluate(&snap));
    }

    #[test]
    fn condition_unhealthy_envelope_count() {
        let snap = unhealthy_snapshot();
        let cond = AlertCondition::UnhealthyEnvelopeCount { max_count: 2 };
        assert!(cond.evaluate(&snap)); // 3 unhealthy > 2
    }

    #[test]
    fn condition_degraded_envelope_count() {
        let snap = degraded_snapshot();
        let cond = AlertCondition::DegradedEnvelopeCount { max_count: 0 };
        assert!(cond.evaluate(&snap)); // 1 degraded > 0
    }

    // -- FleetAlertManager --

    #[test]
    fn manager_fires_alert_on_degraded_fleet() {
        let mut manager = FleetAlertManager::with_defaults();
        let snap = degraded_snapshot();
        let fired = manager.evaluate(&snap);
        assert!(!fired.is_empty());
        let degraded_alert = fired
            .iter()
            .find(|a| a.class_id == "fleet.health.degraded");
        assert!(degraded_alert.is_some());
    }

    #[test]
    fn manager_no_alerts_on_healthy_fleet() {
        let mut manager = FleetAlertManager::with_defaults();
        let snap = healthy_snapshot();
        let fired = manager.evaluate(&snap);
        assert!(fired.is_empty());
    }

    #[test]
    fn manager_dedup_prevents_rapid_refire() {
        let mut manager = FleetAlertManager::new(vec![FleetAlertPolicy {
            class_id: "test.degraded".into(),
            description: "test".into(),
            severity: FleetAlertSeverity::Warning,
            routes: vec![AlertRoute::Log],
            condition: AlertCondition::FleetHealthBelow {
                threshold: HealthStatus::Degraded,
            },
            runbook: None,
            dedup_window_secs: 9999,
            enabled: true,
        }]);

        let snap = degraded_snapshot();
        let first = manager.evaluate(&snap);
        assert_eq!(first.len(), 1);

        // Second evaluation within dedup window should not fire
        let second = manager.evaluate(&snap);
        assert!(second.is_empty());
    }

    #[test]
    fn manager_acknowledge_and_resolve() {
        let mut manager = FleetAlertManager::with_defaults();
        let snap = degraded_snapshot();
        manager.evaluate(&snap);

        let active = manager.active_alerts();
        assert!(!active.is_empty());
        let alert_id = active[0].alert_id;

        // Acknowledge
        assert!(manager.acknowledge(alert_id, "operator-1"));
        let a = manager.all_alerts().iter().find(|a| a.alert_id == alert_id).unwrap();
        assert!(a.is_acked());
        assert!(a.is_active()); // Still active until resolved

        // Resolve
        assert!(manager.resolve(alert_id, "Root cause identified and fixed"));
        let a = manager.all_alerts().iter().find(|a| a.alert_id == alert_id).unwrap();
        assert!(!a.is_active());
        assert!(a.resolution_note.is_some());
    }

    #[test]
    fn manager_acknowledge_nonexistent_returns_false() {
        let mut manager = FleetAlertManager::with_defaults();
        assert!(!manager.acknowledge(999, "nobody"));
    }

    #[test]
    fn manager_active_counts_by_severity() {
        let mut manager = FleetAlertManager::with_defaults();
        let snap = unhealthy_snapshot();
        manager.evaluate(&snap);

        let counts = manager.active_counts_by_severity();
        let total: usize = counts.values().sum();
        assert!(total > 0);
    }

    #[test]
    fn disabled_policy_does_not_fire() {
        let mut manager = FleetAlertManager::new(vec![FleetAlertPolicy {
            class_id: "test.disabled".into(),
            description: "disabled rule".into(),
            severity: FleetAlertSeverity::Critical,
            routes: vec![],
            condition: AlertCondition::FleetHealthBelow {
                threshold: HealthStatus::Degraded,
            },
            runbook: None,
            dedup_window_secs: 0,
            enabled: false,
        }]);

        let snap = degraded_snapshot();
        let fired = manager.evaluate(&snap);
        assert!(fired.is_empty());
    }

    // -- FleetDashboardView --

    #[test]
    fn dashboard_view_from_healthy_snapshot() {
        let manager = FleetAlertManager::with_defaults();
        let snap = healthy_snapshot();
        let view = FleetDashboardView::from_snapshot(&snap, &manager);

        assert_eq!(view.fleet_health, HealthStatus::Healthy);
        assert_eq!(view.total_envelopes, 1);
        assert!(!view.has_critical_alerts());
        assert!(view.most_critical_alert.is_none());
        assert!(view.summary_line.contains("Healthy"));
    }

    #[test]
    fn dashboard_view_from_degraded_snapshot_with_alerts() {
        let mut manager = FleetAlertManager::with_defaults();
        let snap = degraded_snapshot();
        manager.evaluate(&snap);

        let view = FleetDashboardView::from_snapshot(&snap, &manager);
        assert_eq!(view.fleet_health, HealthStatus::Degraded);
        assert_eq!(view.total_envelopes, 2);
        assert!(view.most_critical_alert.is_some());
    }

    #[test]
    fn dashboard_view_has_critical_alerts_on_unhealthy() {
        let mut manager = FleetAlertManager::with_defaults();
        let snap = unhealthy_snapshot();
        manager.evaluate(&snap);

        let view = FleetDashboardView::from_snapshot(&snap, &manager);
        assert!(view.has_critical_alerts());
    }

    #[test]
    fn dashboard_view_layer_envelope_counts() {
        let snap = degraded_snapshot();
        let manager = FleetAlertManager::with_defaults();
        let view = FleetDashboardView::from_snapshot(&snap, &manager);

        assert_eq!(view.layer_envelope_counts.get("policy"), Some(&1));
        assert_eq!(view.layer_envelope_counts.get("mux"), Some(&1));
    }

    // -- Serde roundtrips --

    #[test]
    fn fleet_alert_policy_serde_roundtrip() {
        let policy = &default_policies()[0];
        let json = serde_json::to_string(policy).unwrap();
        let back: FleetAlertPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back.class_id, policy.class_id);
        assert_eq!(back.severity, policy.severity);
    }

    #[test]
    fn dashboard_view_serde_roundtrip() {
        let snap = healthy_snapshot();
        let manager = FleetAlertManager::with_defaults();
        let view = FleetDashboardView::from_snapshot(&snap, &manager);
        let json = serde_json::to_string(&view).unwrap();
        let back: FleetDashboardView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.fleet_health, view.fleet_health);
        assert_eq!(back.total_envelopes, view.total_envelopes);
    }

    #[test]
    fn alert_context_serde_roundtrip() {
        let ctx = AlertContext {
            fleet_health: HealthStatus::Degraded,
            layer_health: [("policy".to_string(), HealthStatus::Healthy)]
                .into_iter()
                .collect(),
            redaction_ceiling: RedactionLabel::Internal,
            envelope_count: 5,
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: AlertContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.fleet_health, ctx.fleet_health);
        assert_eq!(back.envelope_count, ctx.envelope_count);
    }

    // -- Default policies --

    #[test]
    fn default_policies_are_all_enabled() {
        let policies = default_policies();
        assert!(policies.len() >= 5);
        for p in &policies {
            assert!(p.enabled, "policy {} should be enabled", p.class_id);
        }
    }

    #[test]
    fn default_policies_have_unique_class_ids() {
        let policies = default_policies();
        let mut seen = std::collections::HashSet::new();
        for p in &policies {
            assert!(seen.insert(&p.class_id), "duplicate class_id: {}", p.class_id);
        }
    }

    #[test]
    fn runbook_serde_roundtrip() {
        let rb = RunbookRef {
            id: "RB-001".into(),
            title: "Test Runbook".into(),
            doc_link: Some("https://example.com/rb-001".into()),
            steps: vec!["Step 1".into(), "Step 2".into()],
        };
        let json = serde_json::to_string(&rb).unwrap();
        let back: RunbookRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, rb.id);
        assert_eq!(back.steps.len(), 2);
    }

    // -- Health ranking --

    #[test]
    fn health_at_or_below_ranking() {
        assert!(health_at_or_below(HealthStatus::Unhealthy, HealthStatus::Unhealthy));
        assert!(health_at_or_below(HealthStatus::Unhealthy, HealthStatus::Degraded));
        assert!(!health_at_or_below(HealthStatus::Healthy, HealthStatus::Degraded));
        assert!(health_at_or_below(HealthStatus::Degraded, HealthStatus::Degraded));
        assert!(health_at_or_below(HealthStatus::Unknown, HealthStatus::Unknown));
    }
}
