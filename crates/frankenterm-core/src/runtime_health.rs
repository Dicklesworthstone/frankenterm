//! Runtime health checks, doctor surfaces, and incident bundle enrichment (ft-e34d9.10.7.2).
//!
//! Extends FrankenTerm's diagnostic infrastructure with runtime-specific health
//! checks that integrate the unified telemetry schema from `runtime_telemetry`.
//!
//! # Components
//!
//! - [`RuntimeHealthCheck`]: Individual check with pass/warn/fail result and remediation
//! - [`RuntimeDoctorReport`]: Aggregated health report across all subsystems
//! - [`IncidentEnrichment`]: Runtime telemetry context for incident bundles
//! - [`HealthCheckRegistry`]: Extensible registry of named health checks
//!
//! # Usage
//!
//! ```ignore
//! let mut registry = HealthCheckRegistry::new();
//! registry.register("scope_tree", check_scope_tree_health(&tree));
//! registry.register("backpressure", check_backpressure_health(&snapshot));
//! let report = registry.run_all();
//! assert!(report.overall_healthy());
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::BuildHasher;

use crate::runtime_telemetry::{
    FailureClass, HealthTier, RuntimePhase, RuntimeTelemetryLog, UnifiedTelemetryRecord,
};

// =============================================================================
// Health check result
// =============================================================================

/// Outcome of a single health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Check passed — no issues detected.
    Pass,
    /// Check produced a warning — not critical but worth investigating.
    Warn,
    /// Check failed — requires attention.
    Fail,
    /// Check was skipped (precondition not met or not applicable).
    Skip,
}

impl CheckStatus {
    /// Whether this status represents a healthy state.
    #[must_use]
    pub fn is_healthy(self) -> bool {
        matches!(self, Self::Pass | Self::Skip)
    }

    /// Map to a health tier for aggregation.
    #[must_use]
    pub fn to_tier(self) -> HealthTier {
        match self {
            Self::Pass | Self::Skip => HealthTier::Green,
            Self::Warn => HealthTier::Yellow,
            Self::Fail => HealthTier::Red,
        }
    }
}

impl std::fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => f.write_str("PASS"),
            Self::Warn => f.write_str("WARN"),
            Self::Fail => f.write_str("FAIL"),
            Self::Skip => f.write_str("SKIP"),
        }
    }
}

// =============================================================================
// Remediation hint
// =============================================================================

/// An actionable remediation step for a health check finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationHint {
    /// Human-readable description of what to do.
    pub description: String,
    /// Optional CLI command to run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Optional documentation link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_link: Option<String>,
    /// Estimated effort (low/medium/high).
    pub effort: RemediationEffort,
}

/// Effort estimate for a remediation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemediationEffort {
    /// Quick fix — seconds to minutes.
    Low,
    /// Moderate investigation — minutes to an hour.
    Medium,
    /// Significant work — may require restart or config changes.
    High,
}

impl RemediationHint {
    /// Create a simple text-only hint.
    #[must_use]
    pub fn text(description: &str) -> Self {
        Self {
            description: description.to_string(),
            command: None,
            doc_link: None,
            effort: RemediationEffort::Low,
        }
    }

    /// Create a hint with a CLI command.
    #[must_use]
    pub fn with_command(description: &str, command: &str) -> Self {
        Self {
            description: description.to_string(),
            command: Some(command.to_string()),
            doc_link: None,
            effort: RemediationEffort::Low,
        }
    }

    /// Set the effort level.
    #[must_use]
    pub fn effort(mut self, effort: RemediationEffort) -> Self {
        self.effort = effort;
        self
    }

    /// Set a documentation link.
    #[must_use]
    pub fn doc(mut self, link: &str) -> Self {
        self.doc_link = Some(link.to_string());
        self
    }
}

// =============================================================================
// Runtime health check
// =============================================================================

/// Result of a single runtime health check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeHealthCheck {
    /// Machine-readable check identifier (e.g. `"scope_tree"`, `"backpressure"`).
    pub check_id: String,
    /// Human-readable check name.
    pub display_name: String,
    /// Check outcome.
    pub status: CheckStatus,
    /// Health tier derived from the check.
    pub tier: HealthTier,
    /// Human-readable summary of findings.
    pub summary: String,
    /// Detailed evidence supporting the finding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Actionable remediation steps (present for warn/fail).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remediation: Vec<RemediationHint>,
    /// Optional failure class (for failed checks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    /// Check duration in microseconds.
    pub duration_us: u64,
}

impl RuntimeHealthCheck {
    /// Create a passing health check.
    #[must_use]
    pub fn pass(check_id: &str, display_name: &str, summary: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
            display_name: display_name.to_string(),
            status: CheckStatus::Pass,
            tier: HealthTier::Green,
            summary: summary.to_string(),
            evidence: Vec::new(),
            remediation: Vec::new(),
            failure_class: None,
            duration_us: 0,
        }
    }

    /// Create a warning health check.
    #[must_use]
    pub fn warn(check_id: &str, display_name: &str, summary: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
            display_name: display_name.to_string(),
            status: CheckStatus::Warn,
            tier: HealthTier::Yellow,
            summary: summary.to_string(),
            evidence: Vec::new(),
            remediation: Vec::new(),
            failure_class: None,
            duration_us: 0,
        }
    }

    /// Create a failing health check.
    #[must_use]
    pub fn fail(check_id: &str, display_name: &str, summary: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
            display_name: display_name.to_string(),
            status: CheckStatus::Fail,
            tier: HealthTier::Red,
            summary: summary.to_string(),
            evidence: Vec::new(),
            remediation: Vec::new(),
            failure_class: None,
            duration_us: 0,
        }
    }

    /// Create a skipped health check.
    #[must_use]
    pub fn skip(check_id: &str, display_name: &str, reason: &str) -> Self {
        Self {
            check_id: check_id.to_string(),
            display_name: display_name.to_string(),
            status: CheckStatus::Skip,
            tier: HealthTier::Green,
            summary: reason.to_string(),
            evidence: Vec::new(),
            remediation: Vec::new(),
            failure_class: None,
            duration_us: 0,
        }
    }

    /// Add evidence line.
    #[must_use]
    pub fn with_evidence(mut self, line: &str) -> Self {
        self.evidence.push(line.to_string());
        self
    }

    /// Add a remediation hint.
    #[must_use]
    pub fn with_remediation(mut self, hint: RemediationHint) -> Self {
        self.remediation.push(hint);
        self
    }

    /// Set the failure class.
    #[must_use]
    pub fn with_failure_class(mut self, class: FailureClass) -> Self {
        self.failure_class = Some(class);
        self
    }

    /// Set the tier explicitly (overrides default from status).
    #[must_use]
    pub fn with_tier(mut self, tier: HealthTier) -> Self {
        self.tier = tier;
        self
    }

    /// Set the check duration.
    #[must_use]
    pub fn with_duration_us(mut self, us: u64) -> Self {
        self.duration_us = us;
        self
    }
}

// =============================================================================
// Doctor report (aggregated)
// =============================================================================

/// Aggregated doctor report across all health checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDoctorReport {
    /// Timestamp when the report was generated (epoch ms).
    pub timestamp_ms: u64,
    /// Overall health tier (worst of all checks).
    pub overall_tier: HealthTier,
    /// Overall runtime phase.
    pub phase: RuntimePhase,
    /// Individual check results.
    pub checks: Vec<RuntimeHealthCheck>,
    /// Count of checks by status.
    pub status_counts: StatusCounts,
    /// Total duration of all checks (microseconds).
    pub total_duration_us: u64,
    /// Telemetry log snapshot (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry_snapshot: Option<crate::runtime_telemetry::TelemetryLogSnapshot>,
}

/// Check status counts for the report summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusCounts {
    pub pass: u32,
    pub warn: u32,
    pub fail: u32,
    pub skip: u32,
}

impl StatusCounts {
    /// Total number of checks.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.pass + self.warn + self.fail + self.skip
    }
}

impl RuntimeDoctorReport {
    /// Whether the overall system is healthy (no failures).
    #[must_use]
    pub fn overall_healthy(&self) -> bool {
        self.status_counts.fail == 0
    }

    /// Whether there are any warnings.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        self.status_counts.warn > 0
    }

    /// Get all failing checks.
    #[must_use]
    pub fn failing_checks(&self) -> Vec<&RuntimeHealthCheck> {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Fail)
            .collect()
    }

    /// Get all checks with remediation hints.
    #[must_use]
    pub fn checks_with_remediation(&self) -> Vec<&RuntimeHealthCheck> {
        self.checks
            .iter()
            .filter(|c| !c.remediation.is_empty())
            .collect()
    }

    /// Format as a human-readable summary.
    #[must_use]
    pub fn format_summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "Runtime Health: {} ({} checks: {} pass, {} warn, {} fail, {} skip)",
            self.overall_tier,
            self.status_counts.total(),
            self.status_counts.pass,
            self.status_counts.warn,
            self.status_counts.fail,
            self.status_counts.skip,
        ));

        for check in &self.checks {
            let status_marker = match check.status {
                CheckStatus::Pass => "✓",
                CheckStatus::Warn => "⚠",
                CheckStatus::Fail => "✗",
                CheckStatus::Skip => "○",
            };
            lines.push(format!(
                "  {} {} — {}",
                status_marker, check.display_name, check.summary
            ));

            for hint in &check.remediation {
                lines.push(format!("    → {}", hint.description));
                if let Some(cmd) = &hint.command {
                    lines.push(format!("      $ {cmd}"));
                }
            }
        }

        lines.join("\n")
    }
}

// =============================================================================
// Health check registry
// =============================================================================

/// Registry of health checks that can be executed together.
pub struct HealthCheckRegistry {
    checks: Vec<RuntimeHealthCheck>,
    phase: RuntimePhase,
}

impl HealthCheckRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            checks: Vec::new(),
            phase: RuntimePhase::Running,
        }
    }

    /// Set the current runtime phase.
    #[must_use]
    pub fn with_phase(mut self, phase: RuntimePhase) -> Self {
        self.phase = phase;
        self
    }

    /// Register a pre-computed health check result.
    pub fn register(&mut self, check: RuntimeHealthCheck) {
        self.checks.push(check);
    }

    /// Build the aggregated doctor report.
    #[must_use]
    pub fn build_report(self) -> RuntimeDoctorReport {
        self.build_report_with_telemetry(None)
    }

    /// Build the aggregated doctor report with optional telemetry snapshot.
    #[must_use]
    pub fn build_report_with_telemetry(
        self,
        telemetry_log: Option<&RuntimeTelemetryLog>,
    ) -> RuntimeDoctorReport {
        let mut status_counts = StatusCounts::default();
        let mut worst_tier = HealthTier::Green;
        let mut total_duration_us = 0u64;

        for check in &self.checks {
            match check.status {
                CheckStatus::Pass => status_counts.pass += 1,
                CheckStatus::Warn => status_counts.warn += 1,
                CheckStatus::Fail => status_counts.fail += 1,
                CheckStatus::Skip => status_counts.skip += 1,
            }
            if check.tier > worst_tier {
                worst_tier = check.tier;
            }
            total_duration_us = total_duration_us.saturating_add(check.duration_us);
        }

        let telemetry_snapshot = telemetry_log.map(|log| log.snapshot());

        RuntimeDoctorReport {
            timestamp_ms: crate::runtime_telemetry::RuntimeTelemetryEvent::now_ms(),
            overall_tier: worst_tier,
            phase: self.phase,
            checks: self.checks,
            status_counts,
            total_duration_us,
            telemetry_snapshot,
        }
    }
}

impl Default for HealthCheckRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Incident bundle enrichment
// =============================================================================

/// Runtime telemetry context to include in incident bundles.
///
/// Enriches the existing incident bundle (from `incident_bundle.rs`) with
/// runtime-specific telemetry data for better root-cause analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentEnrichment {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// Health tier at the time of the incident.
    pub health_tier: HealthTier,
    /// Runtime phase at the time of the incident.
    pub phase: RuntimePhase,
    /// Doctor report snapshot (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doctor_report: Option<RuntimeDoctorReport>,
    /// Recent telemetry events leading up to the incident.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_events: Vec<crate::runtime_telemetry::RuntimeTelemetryEvent>,
    /// Recent telemetry records normalized to the shared cross-subsystem schema.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_records: Vec<UnifiedTelemetryRecord>,
    /// Tier transition history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tier_transitions: Vec<crate::runtime_telemetry::TierTransitionRecord>,
    /// Active failure classes at the time of incident.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_failures: Vec<ActiveFailure>,
    /// Scope states at the time of incident.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scope_states: HashMap<String, String>,
}

/// An active failure at the time of an incident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveFailure {
    /// Component experiencing the failure.
    pub component: String,
    /// Classification of the failure.
    pub failure_class: FailureClass,
    /// When the failure started (epoch ms).
    pub started_ms: u64,
    /// Number of occurrences.
    pub occurrence_count: u64,
    /// Last error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl IncidentEnrichment {
    /// Current schema version.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Create a new enrichment context.
    #[must_use]
    pub fn new(tier: HealthTier, phase: RuntimePhase) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            health_tier: tier,
            phase,
            doctor_report: None,
            recent_events: Vec::new(),
            recent_records: Vec::new(),
            tier_transitions: Vec::new(),
            active_failures: Vec::new(),
            scope_states: HashMap::new(),
        }
    }

    /// Populate from a telemetry log (takes the N most recent events).
    #[must_use]
    pub fn with_telemetry_log(mut self, log: &RuntimeTelemetryLog, max_events: usize) -> Self {
        let events = log.events();
        let start = events.len().saturating_sub(max_events);
        let recent_events = events[start..].to_vec();
        self.recent_records = recent_events
            .iter()
            .map(UnifiedTelemetryRecord::from)
            .collect();
        self.recent_events = recent_events;
        self
    }

    /// Attach a doctor report.
    #[must_use]
    pub fn with_doctor_report(mut self, report: RuntimeDoctorReport) -> Self {
        self.doctor_report = Some(report);
        self
    }

    /// Add a tier transition record.
    pub fn add_tier_transition(&mut self, record: crate::runtime_telemetry::TierTransitionRecord) {
        self.tier_transitions.push(record);
    }

    /// Add an active failure.
    pub fn add_active_failure(&mut self, failure: ActiveFailure) {
        self.active_failures.push(failure);
    }

    /// Record a scope state.
    pub fn add_scope_state(&mut self, scope_id: &str, state: &str) {
        self.scope_states
            .insert(scope_id.to_string(), state.to_string());
    }

    /// Export as JSON string.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Total event count in the enrichment.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.recent_events.len()
    }
}

// =============================================================================
// Built-in health checks
// =============================================================================

/// Check telemetry log health: eviction rate, error ratio, tier distribution.
#[must_use]
pub fn check_telemetry_log(log: &RuntimeTelemetryLog) -> RuntimeHealthCheck {
    let snap = log.snapshot();

    // Check eviction ratio
    if snap.total_emitted > 0 {
        let eviction_ratio = snap.total_evicted as f64 / snap.total_emitted as f64;
        if eviction_ratio > 0.5 {
            return RuntimeHealthCheck::warn(
                "telemetry_log",
                "Telemetry Log",
                &format!(
                    "High eviction rate: {:.1}% of events evicted ({}/{})",
                    eviction_ratio * 100.0,
                    snap.total_evicted,
                    snap.total_emitted
                ),
            )
            .with_evidence(&format!("Buffered: {}", snap.buffered_events))
            .with_evidence(&format!(
                "Tier distribution: G={} Y={} R={} B={}",
                snap.tier_counts[0], snap.tier_counts[1], snap.tier_counts[2], snap.tier_counts[3]
            ))
            .with_remediation(
                RemediationHint::text("Increase telemetry log max_events or reduce event volume")
                    .effort(RemediationEffort::Low),
            );
        }
    }

    // Check error event ratio
    let error_count = snap.category_counts.get("error").copied().unwrap_or(0);
    if snap.buffered_events > 0 {
        let error_ratio = error_count as f64 / snap.buffered_events as f64;
        if error_ratio > 0.2 {
            return RuntimeHealthCheck::warn(
                "telemetry_log",
                "Telemetry Log",
                &format!(
                    "High error ratio: {:.1}% of buffered events are errors ({}/{})",
                    error_ratio * 100.0,
                    error_count,
                    snap.buffered_events
                ),
            )
            .with_evidence(&format!(
                "Tier distribution: G={} Y={} R={} B={}",
                snap.tier_counts[0], snap.tier_counts[1], snap.tier_counts[2], snap.tier_counts[3]
            ))
            .with_remediation(RemediationHint::with_command(
                "Check recent error events",
                "ft debug dump-telemetry --filter error",
            ));
        }
    }

    // Check for Black tier events
    if snap.tier_counts[3] > 0 {
        return RuntimeHealthCheck::fail(
            "telemetry_log",
            "Telemetry Log",
            &format!(
                "Critical events detected: {} Black-tier events in buffer",
                snap.tier_counts[3]
            ),
        )
        .with_failure_class(FailureClass::Overload)
        .with_remediation(RemediationHint::with_command(
            "Investigate critical events",
            "ft debug dump-telemetry --filter tier=black",
        ));
    }

    RuntimeHealthCheck::pass(
        "telemetry_log",
        "Telemetry Log",
        &format!(
            "Healthy: {} events buffered, {} total emitted",
            snap.buffered_events, snap.total_emitted
        ),
    )
}

/// Check tier distribution health: are we spending too much time in degraded tiers?
#[must_use]
pub fn check_tier_distribution(log: &RuntimeTelemetryLog) -> RuntimeHealthCheck {
    let snap = log.snapshot();

    if snap.buffered_events == 0 {
        return RuntimeHealthCheck::skip(
            "tier_distribution",
            "Tier Distribution",
            "No telemetry events to analyze",
        );
    }

    let degraded = snap.tier_counts[1] + snap.tier_counts[2] + snap.tier_counts[3];
    let degraded_ratio = degraded as f64 / snap.buffered_events as f64;

    if degraded_ratio > 0.5 {
        let tier = if snap.tier_counts[3] > 0 {
            HealthTier::Black
        } else if snap.tier_counts[2] > 0 {
            HealthTier::Red
        } else {
            HealthTier::Yellow
        };

        return RuntimeHealthCheck::fail(
            "tier_distribution",
            "Tier Distribution",
            &format!(
                "Degraded: {:.1}% of events in non-green tiers (Y={} R={} B={})",
                degraded_ratio * 100.0,
                snap.tier_counts[1],
                snap.tier_counts[2],
                snap.tier_counts[3],
            ),
        )
        .with_tier(tier)
        .with_evidence(&format!("Total events: {}", snap.buffered_events))
        .with_remediation(
            RemediationHint::text("Investigate root cause of sustained degraded operation")
                .effort(RemediationEffort::High),
        );
    }

    if degraded_ratio > 0.1 {
        return RuntimeHealthCheck::warn(
            "tier_distribution",
            "Tier Distribution",
            &format!(
                "Elevated: {:.1}% of events in non-green tiers",
                degraded_ratio * 100.0,
            ),
        )
        .with_evidence(&format!(
            "G={} Y={} R={} B={}",
            snap.tier_counts[0], snap.tier_counts[1], snap.tier_counts[2], snap.tier_counts[3],
        ));
    }

    RuntimeHealthCheck::pass(
        "tier_distribution",
        "Tier Distribution",
        &format!(
            "Healthy: {:.1}% green ({}/{})",
            (1.0 - degraded_ratio) * 100.0,
            snap.tier_counts[0],
            snap.buffered_events,
        ),
    )
}

/// Check scope lifecycle health: are any scopes stuck in non-terminal states?
#[must_use]
pub fn check_scope_health(
    scope_states: &HashMap<String, String, impl BuildHasher>,
) -> RuntimeHealthCheck {
    if scope_states.is_empty() {
        return RuntimeHealthCheck::skip(
            "scope_health",
            "Scope Lifecycle",
            "No scope state data available",
        );
    }

    let mut draining_count = 0;
    let mut finalizing_count = 0;
    let mut stuck_scopes = Vec::new();

    for (scope_id, state) in scope_states {
        match state.as_str() {
            "draining" => {
                draining_count += 1;
                stuck_scopes.push(format!("{scope_id}={state}"));
            }
            "finalizing" => {
                finalizing_count += 1;
                stuck_scopes.push(format!("{scope_id}={state}"));
            }
            _ => {}
        }
    }

    if finalizing_count > 0 {
        return RuntimeHealthCheck::fail(
            "scope_health",
            "Scope Lifecycle",
            &format!("{finalizing_count} scope(s) stuck in finalizing state"),
        )
        .with_failure_class(FailureClass::Deadlock)
        .with_evidence(&stuck_scopes.join(", "))
        .with_remediation(
            RemediationHint::text("Investigate blocked finalizers; may need forced shutdown")
                .effort(RemediationEffort::High),
        );
    }

    if draining_count > 0 {
        return RuntimeHealthCheck::warn(
            "scope_health",
            "Scope Lifecycle",
            &format!("{draining_count} scope(s) in draining state"),
        )
        .with_evidence(&stuck_scopes.join(", "))
        .with_remediation(
            RemediationHint::text("Draining scopes may be waiting for in-flight work to complete")
                .effort(RemediationEffort::Medium),
        );
    }

    let total = scope_states.len();
    let running = scope_states
        .values()
        .filter(|s| s.as_str() == "running")
        .count();
    let closed = scope_states
        .values()
        .filter(|s| s.as_str() == "closed")
        .count();

    RuntimeHealthCheck::pass(
        "scope_health",
        "Scope Lifecycle",
        &format!("Healthy: {total} scopes ({running} running, {closed} closed)"),
    )
}

/// Check for recent failure patterns in the telemetry log.
#[must_use]
pub fn check_failure_patterns(log: &RuntimeTelemetryLog) -> RuntimeHealthCheck {
    let events = log.events();

    if events.is_empty() {
        return RuntimeHealthCheck::skip(
            "failure_patterns",
            "Failure Patterns",
            "No telemetry events to analyze",
        );
    }

    // Count failures by class
    let mut failure_counts: HashMap<String, u64> = HashMap::new();
    let mut panic_count = 0u64;
    let mut safety_count = 0u64;

    for event in events {
        if let Some(fc) = &event.failure_class {
            let key = fc.to_string();
            *failure_counts.entry(key).or_default() += 1;
            match fc {
                FailureClass::Panic => panic_count += 1,
                FailureClass::Safety => safety_count += 1,
                _ => {}
            }
        }
    }

    if panic_count > 0 {
        return RuntimeHealthCheck::fail(
            "failure_patterns",
            "Failure Patterns",
            &format!("{panic_count} panic(s) detected in telemetry"),
        )
        .with_failure_class(FailureClass::Panic)
        .with_tier(HealthTier::Black)
        .with_remediation(
            RemediationHint::with_command(
                "Review panic backtraces",
                "ft debug dump-telemetry --filter panic",
            )
            .effort(RemediationEffort::High),
        );
    }

    if safety_count > 0 {
        return RuntimeHealthCheck::fail(
            "failure_patterns",
            "Failure Patterns",
            &format!("{safety_count} safety violation(s) detected"),
        )
        .with_failure_class(FailureClass::Safety)
        .with_tier(HealthTier::Red)
        .with_remediation(
            RemediationHint::text("Investigate safety policy violations")
                .effort(RemediationEffort::High),
        );
    }

    let total_failures: u64 = failure_counts.values().sum();
    if total_failures > 0 {
        let detail: Vec<String> = failure_counts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();

        return RuntimeHealthCheck::warn(
            "failure_patterns",
            "Failure Patterns",
            &format!(
                "{total_failures} failure(s) detected: {}",
                detail.join(", ")
            ),
        );
    }

    RuntimeHealthCheck::pass(
        "failure_patterns",
        "Failure Patterns",
        "No failure patterns detected",
    )
}

/// Generate health checks from a policy metrics dashboard.
///
/// Produces one [`RuntimeHealthCheck`] per policy health indicator
/// (denial rate, quarantine density, compliance violations, audit chain
/// integrity, and kill switch), mapping each to the appropriate tier and
/// remediation hints.
pub fn checks_from_policy_dashboard(
    dashboard: &crate::policy_metrics::PolicyMetricsDashboard,
) -> Vec<RuntimeHealthCheck> {
    use crate::policy_metrics::HealthStatus;

    let mut checks = Vec::new();

    for indicator in &dashboard.indicators {
        let (status, tier, failure_class) = match indicator.status {
            HealthStatus::Healthy => (CheckStatus::Pass, HealthTier::Green, None),
            HealthStatus::Warning => (CheckStatus::Warn, HealthTier::Yellow, None),
            HealthStatus::Critical => (
                CheckStatus::Fail,
                HealthTier::Red,
                Some(FailureClass::Safety),
            ),
            HealthStatus::Unknown => (CheckStatus::Skip, HealthTier::Black, None),
        };

        let check_id = format!("policy.{}", indicator.name);
        let mut check = RuntimeHealthCheck {
            check_id,
            display_name: format!("Policy: {}", indicator.description),
            status,
            tier,
            summary: format!(
                "{}: {} (warn={}, crit={})",
                indicator.name,
                indicator.value,
                indicator.threshold_warning,
                indicator.threshold_critical,
            ),
            evidence: Vec::new(),
            remediation: Vec::new(),
            failure_class,
            duration_us: 0,
        };

        // Add remediation hints for non-healthy indicators
        if indicator.status == HealthStatus::Warning || indicator.status == HealthStatus::Critical {
            match indicator.name.as_str() {
                "denial_rate" => {
                    check.remediation.push(
                        RemediationHint::text("Review policy rules for over-restrictive patterns")
                            .effort(RemediationEffort::Medium),
                    );
                }
                "quarantine_density" => {
                    check.remediation.push(
                        RemediationHint::with_command(
                            "Review quarantined components",
                            "ft robot policy quarantine-list",
                        )
                        .effort(RemediationEffort::Medium),
                    );
                }
                "compliance_violations" => {
                    check.remediation.push(
                        RemediationHint::text(
                            "Investigate and remediate active compliance violations",
                        )
                        .effort(RemediationEffort::High),
                    );
                }
                "audit_chain_integrity" => {
                    check = check.with_failure_class(FailureClass::Corruption);
                    check.remediation.push(
                        RemediationHint::text("Audit chain tampered — investigate chain integrity and rebuild if necessary")
                            .effort(RemediationEffort::High),
                    );
                }
                "kill_switch" => {
                    check.remediation.push(
                        RemediationHint::with_command(
                            "Review and reset kill switch when safe",
                            "ft robot policy kill-switch reset",
                        )
                        .effort(RemediationEffort::Low),
                    );
                }
                _ => {}
            }
        }

        checks.push(check);
    }

    checks
}

// =============================================================================
// Robot types for health/doctor surfaces
// =============================================================================

/// Response data for `ft robot health` / `ft doctor --format json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckData {
    /// Overall health tier.
    pub overall_tier: String,
    /// Whether the system is healthy (no failures).
    pub healthy: bool,
    /// Whether there are warnings.
    pub has_warnings: bool,
    /// Current runtime phase.
    pub phase: String,
    /// Individual check results.
    pub checks: Vec<HealthCheckItem>,
    /// Summary counts.
    pub summary: HealthSummary,
}

/// Single health check item in robot output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckItem {
    /// Check identifier.
    pub check_id: String,
    /// Human-readable name.
    pub name: String,
    /// Status: pass/warn/fail/skip.
    pub status: String,
    /// Health tier for this check.
    pub tier: String,
    /// Summary message.
    pub summary: String,
    /// Evidence lines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Remediation hints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remediation: Vec<RemediationItem>,
}

/// Remediation hint in robot output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationItem {
    /// What to do.
    pub description: String,
    /// Optional command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Effort level.
    pub effort: String,
}

/// Summary counts in robot health output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSummary {
    pub total: u32,
    pub pass: u32,
    pub warn: u32,
    pub fail: u32,
    pub skip: u32,
}

impl From<&RuntimeDoctorReport> for HealthCheckData {
    fn from(report: &RuntimeDoctorReport) -> Self {
        Self {
            overall_tier: report.overall_tier.to_string(),
            healthy: report.overall_healthy(),
            has_warnings: report.has_warnings(),
            phase: report.phase.to_string(),
            checks: report
                .checks
                .iter()
                .map(|c| HealthCheckItem {
                    check_id: c.check_id.clone(),
                    name: c.display_name.clone(),
                    status: c.status.to_string().to_lowercase(),
                    tier: c.tier.to_string(),
                    summary: c.summary.clone(),
                    evidence: c.evidence.clone(),
                    remediation: c
                        .remediation
                        .iter()
                        .map(|r| RemediationItem {
                            description: r.description.clone(),
                            command: r.command.clone(),
                            effort: match r.effort {
                                RemediationEffort::Low => "low".to_string(),
                                RemediationEffort::Medium => "medium".to_string(),
                                RemediationEffort::High => "high".to_string(),
                            },
                        })
                        .collect(),
                })
                .collect(),
            summary: HealthSummary {
                total: report.status_counts.total(),
                pass: report.status_counts.pass,
                warn: report.status_counts.warn,
                fail: report.status_counts.fail,
                skip: report.status_counts.skip,
            },
        }
    }
}

/// Response data for `ft robot incident-bundle` enrichment info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentEnrichmentData {
    /// Schema version.
    pub schema_version: u32,
    /// Health tier at incident time.
    pub health_tier: String,
    /// Runtime phase at incident time.
    pub phase: String,
    /// Number of recent telemetry events included.
    pub recent_event_count: usize,
    /// Number of normalized telemetry records included.
    pub recent_record_count: usize,
    /// Number of tier transitions included.
    pub tier_transition_count: usize,
    /// Number of active failures.
    pub active_failure_count: usize,
    /// Number of scope states captured.
    pub scope_state_count: usize,
    /// Whether a doctor report is included.
    pub has_doctor_report: bool,
}

impl From<&IncidentEnrichment> for IncidentEnrichmentData {
    fn from(enrichment: &IncidentEnrichment) -> Self {
        Self {
            schema_version: enrichment.schema_version,
            health_tier: enrichment.health_tier.to_string(),
            phase: enrichment.phase.to_string(),
            recent_event_count: enrichment.recent_events.len(),
            recent_record_count: enrichment.recent_records.len(),
            tier_transition_count: enrichment.tier_transitions.len(),
            active_failure_count: enrichment.active_failures.len(),
            scope_state_count: enrichment.scope_states.len(),
            has_doctor_report: enrichment.doctor_report.is_some(),
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_telemetry::{
        RuntimeTelemetryEventBuilder, RuntimeTelemetryKind, RuntimeTelemetryLogConfig,
    };

    // ── CheckStatus ──

    #[test]
    fn check_status_healthy() {
        assert!(CheckStatus::Pass.is_healthy());
        assert!(CheckStatus::Skip.is_healthy());
        assert!(!CheckStatus::Warn.is_healthy());
        assert!(!CheckStatus::Fail.is_healthy());
    }

    #[test]
    fn check_status_to_tier() {
        assert_eq!(CheckStatus::Pass.to_tier(), HealthTier::Green);
        assert_eq!(CheckStatus::Skip.to_tier(), HealthTier::Green);
        assert_eq!(CheckStatus::Warn.to_tier(), HealthTier::Yellow);
        assert_eq!(CheckStatus::Fail.to_tier(), HealthTier::Red);
    }

    #[test]
    fn check_status_display() {
        assert_eq!(CheckStatus::Pass.to_string(), "PASS");
        assert_eq!(CheckStatus::Warn.to_string(), "WARN");
        assert_eq!(CheckStatus::Fail.to_string(), "FAIL");
        assert_eq!(CheckStatus::Skip.to_string(), "SKIP");
    }

    #[test]
    fn check_status_serde_roundtrip() {
        for status in [
            CheckStatus::Pass,
            CheckStatus::Warn,
            CheckStatus::Fail,
            CheckStatus::Skip,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let rt: CheckStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, status);
        }
    }

    // ── RemediationHint ──

    #[test]
    fn remediation_hint_text() {
        let hint = RemediationHint::text("do something");
        assert_eq!(hint.description, "do something");
        assert!(hint.command.is_none());
        assert!(hint.doc_link.is_none());
        assert_eq!(hint.effort, RemediationEffort::Low);
    }

    #[test]
    fn remediation_hint_with_command() {
        let hint = RemediationHint::with_command("check logs", "ft debug logs")
            .effort(RemediationEffort::Medium)
            .doc("https://docs.example.com");
        assert_eq!(hint.command, Some("ft debug logs".to_string()));
        assert_eq!(hint.effort, RemediationEffort::Medium);
        assert!(hint.doc_link.is_some());
    }

    #[test]
    fn remediation_effort_serde() {
        for effort in [
            RemediationEffort::Low,
            RemediationEffort::Medium,
            RemediationEffort::High,
        ] {
            let json = serde_json::to_string(&effort).unwrap();
            let rt: RemediationEffort = serde_json::from_str(&json).unwrap();
            assert_eq!(rt, effort);
        }
    }

    // ── RuntimeHealthCheck ──

    #[test]
    fn health_check_constructors() {
        let pass = RuntimeHealthCheck::pass("test", "Test Check", "All good");
        assert_eq!(pass.status, CheckStatus::Pass);
        assert_eq!(pass.tier, HealthTier::Green);

        let warn = RuntimeHealthCheck::warn("test", "Test Check", "Minor issue");
        assert_eq!(warn.status, CheckStatus::Warn);
        assert_eq!(warn.tier, HealthTier::Yellow);

        let fail = RuntimeHealthCheck::fail("test", "Test Check", "Critical");
        assert_eq!(fail.status, CheckStatus::Fail);
        assert_eq!(fail.tier, HealthTier::Red);

        let skip = RuntimeHealthCheck::skip("test", "Test Check", "N/A");
        assert_eq!(skip.status, CheckStatus::Skip);
        assert_eq!(skip.tier, HealthTier::Green);
    }

    #[test]
    fn health_check_builder_chain() {
        let check = RuntimeHealthCheck::fail("bp", "Backpressure", "Overloaded")
            .with_evidence("Queue depth: 95%")
            .with_evidence("Duration: 30s")
            .with_remediation(RemediationHint::text("Reduce load"))
            .with_failure_class(FailureClass::Overload)
            .with_tier(HealthTier::Black)
            .with_duration_us(1500);

        assert_eq!(check.evidence.len(), 2);
        assert_eq!(check.remediation.len(), 1);
        assert_eq!(check.failure_class, Some(FailureClass::Overload));
        assert_eq!(check.tier, HealthTier::Black);
        assert_eq!(check.duration_us, 1500);
    }

    #[test]
    fn health_check_serde_roundtrip() {
        let check = RuntimeHealthCheck::warn("test", "Test", "Warning issued")
            .with_evidence("evidence line")
            .with_remediation(RemediationHint::with_command("fix it", "ft fix"));

        let json = serde_json::to_string(&check).unwrap();
        let rt: RuntimeHealthCheck = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.check_id, check.check_id);
        assert_eq!(rt.status, check.status);
        assert_eq!(rt.evidence.len(), 1);
        assert_eq!(rt.remediation.len(), 1);
    }

    // ── HealthCheckRegistry + Report ──

    #[test]
    fn registry_builds_report() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("a", "Check A", "OK"));
        reg.register(RuntimeHealthCheck::warn("b", "Check B", "Warning"));
        reg.register(RuntimeHealthCheck::pass("c", "Check C", "OK"));

        let report = reg.build_report();
        assert_eq!(report.status_counts.pass, 2);
        assert_eq!(report.status_counts.warn, 1);
        assert_eq!(report.status_counts.fail, 0);
        assert_eq!(report.overall_tier, HealthTier::Yellow);
        assert!(report.overall_healthy());
        assert!(report.has_warnings());
    }

    #[test]
    fn report_overall_healthy() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("a", "A", "OK"));
        let report = reg.build_report();
        assert!(report.overall_healthy());
        assert!(!report.has_warnings());
    }

    #[test]
    fn report_overall_unhealthy() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::fail("a", "A", "Failed"));
        let report = reg.build_report();
        assert!(!report.overall_healthy());
        assert_eq!(report.failing_checks().len(), 1);
    }

    #[test]
    fn report_worst_tier_wins() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("a", "A", "OK"));
        reg.register(RuntimeHealthCheck::fail("b", "B", "Critical").with_tier(HealthTier::Black));
        reg.register(RuntimeHealthCheck::warn("c", "C", "Warn"));

        let report = reg.build_report();
        assert_eq!(report.overall_tier, HealthTier::Black);
    }

    #[test]
    fn report_format_summary() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("a", "Check A", "All good"));
        reg.register(
            RuntimeHealthCheck::warn("b", "Check B", "Minor issue")
                .with_remediation(RemediationHint::with_command("Fix it", "ft fix")),
        );

        let report = reg.build_report();
        let summary = report.format_summary();
        assert!(summary.contains("Check A"));
        assert!(summary.contains("Check B"));
        assert!(summary.contains("ft fix"));
    }

    #[test]
    fn report_with_telemetry() {
        let mut log = RuntimeTelemetryLog::with_defaults();
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                .reason("test"),
        );

        let reg = HealthCheckRegistry::new();
        let report = reg.build_report_with_telemetry(Some(&log));
        assert!(report.telemetry_snapshot.is_some());
        assert_eq!(report.telemetry_snapshot.unwrap().buffered_events, 1);
    }

    #[test]
    fn report_checks_with_remediation() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("a", "A", "OK"));
        reg.register(
            RuntimeHealthCheck::warn("b", "B", "Issue")
                .with_remediation(RemediationHint::text("do this")),
        );
        reg.register(RuntimeHealthCheck::pass("c", "C", "OK"));

        let report = reg.build_report();
        assert_eq!(report.checks_with_remediation().len(), 1);
    }

    // ── IncidentEnrichment ──

    #[test]
    fn incident_enrichment_basic() {
        let enrichment = IncidentEnrichment::new(HealthTier::Red, RuntimePhase::Running);
        assert_eq!(enrichment.schema_version, 1);
        assert_eq!(enrichment.health_tier, HealthTier::Red);
        assert_eq!(enrichment.phase, RuntimePhase::Running);
        assert!(enrichment.recent_events.is_empty());
        assert!(enrichment.recent_records.is_empty());
    }

    #[test]
    fn incident_enrichment_with_telemetry() {
        let mut log = RuntimeTelemetryLog::with_defaults();
        for i in 0..10 {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("ev_{i}")),
            );
        }

        let enrichment = IncidentEnrichment::new(HealthTier::Yellow, RuntimePhase::Running)
            .with_telemetry_log(&log, 5);
        assert_eq!(enrichment.event_count(), 5);
        // Should be the 5 most recent
        assert_eq!(enrichment.recent_events[0].reason_code, "ev_5");
        assert_eq!(enrichment.recent_events[4].reason_code, "ev_9");
        assert_eq!(enrichment.recent_records.len(), 5);
        assert_eq!(enrichment.recent_records[0].reason_code, "ev_5");
        assert_eq!(enrichment.recent_records[4].reason_code, "ev_9");
    }

    #[test]
    fn incident_enrichment_with_doctor_report() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("a", "A", "OK"));
        let report = reg.build_report();

        let enrichment = IncidentEnrichment::new(HealthTier::Green, RuntimePhase::Running)
            .with_doctor_report(report);
        assert!(enrichment.doctor_report.is_some());
    }

    #[test]
    fn incident_enrichment_active_failures() {
        let mut enrichment = IncidentEnrichment::new(HealthTier::Red, RuntimePhase::Running);
        enrichment.add_active_failure(ActiveFailure {
            component: "rt.storage".to_string(),
            failure_class: FailureClass::Timeout,
            started_ms: 1000,
            occurrence_count: 5,
            last_error: Some("write timeout".to_string()),
        });
        enrichment.add_scope_state("daemon:capture", "draining");

        assert_eq!(enrichment.active_failures.len(), 1);
        assert_eq!(enrichment.scope_states.len(), 1);
    }

    #[test]
    fn incident_enrichment_to_json() {
        let enrichment = IncidentEnrichment::new(HealthTier::Green, RuntimePhase::Running);
        let json = enrichment.to_json();
        assert!(json.contains("schema_version"));
        assert!(json.contains("\"green\""));
    }

    #[test]
    fn incident_enrichment_serde_roundtrip() {
        let mut enrichment = IncidentEnrichment::new(HealthTier::Yellow, RuntimePhase::Draining);
        enrichment.add_scope_state("root", "draining");
        enrichment.add_active_failure(ActiveFailure {
            component: "rt.test".to_string(),
            failure_class: FailureClass::Transient,
            started_ms: 500,
            occurrence_count: 3,
            last_error: None,
        });

        let json = serde_json::to_string(&enrichment).unwrap();
        let rt: IncidentEnrichment = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.health_tier, HealthTier::Yellow);
        assert_eq!(rt.phase, RuntimePhase::Draining);
        assert_eq!(rt.scope_states.len(), 1);
        assert_eq!(rt.active_failures.len(), 1);
    }

    // ── Built-in health checks ──

    #[test]
    fn check_telemetry_log_healthy() {
        let mut log = RuntimeTelemetryLog::with_defaults();
        for _ in 0..5 {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason("ok"),
            );
        }
        let check = check_telemetry_log(&log);
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn check_telemetry_log_high_eviction() {
        let mut log = RuntimeTelemetryLog::new(RuntimeTelemetryLogConfig {
            max_events: 3,
            enabled: true,
        });
        for i in 0..10 {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("ev_{i}")),
            );
        }
        let check = check_telemetry_log(&log);
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(!check.remediation.is_empty());
    }

    #[test]
    fn check_telemetry_log_black_tier() {
        let mut log = RuntimeTelemetryLog::with_defaults();
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::LoadShedding)
                .tier(HealthTier::Black)
                .reason("critical"),
        );
        let check = check_telemetry_log(&log);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.failure_class, Some(FailureClass::Overload));
    }

    #[test]
    fn check_tier_distribution_healthy() {
        let mut log = RuntimeTelemetryLog::with_defaults();
        for _ in 0..10 {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Green)
                    .reason("ok"),
            );
        }
        let check = check_tier_distribution(&log);
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn check_tier_distribution_degraded() {
        let mut log = RuntimeTelemetryLog::with_defaults();
        // 3 green + 7 yellow = 70% degraded
        for _ in 0..3 {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .tier(HealthTier::Green)
                    .reason("ok"),
            );
        }
        for _ in 0..7 {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::ThrottleApplied)
                    .tier(HealthTier::Yellow)
                    .reason("throttle"),
            );
        }
        let check = check_tier_distribution(&log);
        assert_eq!(check.status, CheckStatus::Fail);
    }

    #[test]
    fn check_tier_distribution_empty() {
        let log = RuntimeTelemetryLog::with_defaults();
        let check = check_tier_distribution(&log);
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn check_scope_health_pass() {
        let mut states = HashMap::new();
        states.insert("root".to_string(), "running".to_string());
        states.insert("daemon:capture".to_string(), "running".to_string());

        let check = check_scope_health(&states);
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn check_scope_health_draining() {
        let mut states = HashMap::new();
        states.insert("root".to_string(), "running".to_string());
        states.insert("daemon:capture".to_string(), "draining".to_string());

        let check = check_scope_health(&states);
        assert_eq!(check.status, CheckStatus::Warn);
    }

    #[test]
    fn check_scope_health_stuck_finalizing() {
        let mut states = HashMap::new();
        states.insert("daemon:capture".to_string(), "finalizing".to_string());

        let check = check_scope_health(&states);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.failure_class, Some(FailureClass::Deadlock));
    }

    #[test]
    fn check_scope_health_empty() {
        let states = HashMap::new();
        let check = check_scope_health(&states);
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn check_failure_patterns_clean() {
        let mut log = RuntimeTelemetryLog::with_defaults();
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                .reason("ok"),
        );
        let check = check_failure_patterns(&log);
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn check_failure_patterns_panic() {
        let mut log = RuntimeTelemetryLog::with_defaults();
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.error", RuntimeTelemetryKind::PanicCaptured)
                .failure(FailureClass::Panic)
                .reason("panic"),
        );
        let check = check_failure_patterns(&log);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.tier, HealthTier::Black);
    }

    #[test]
    fn check_failure_patterns_transient() {
        let mut log = RuntimeTelemetryLog::with_defaults();
        log.emit(
            RuntimeTelemetryEventBuilder::new("rt.net", RuntimeTelemetryKind::TransientError)
                .failure(FailureClass::Transient)
                .reason("timeout"),
        );
        let check = check_failure_patterns(&log);
        assert_eq!(check.status, CheckStatus::Warn);
    }

    // ── Robot types ──

    #[test]
    fn health_check_data_from_report() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("a", "Check A", "OK"));
        reg.register(
            RuntimeHealthCheck::warn("b", "Check B", "Warning")
                .with_remediation(RemediationHint::text("fix")),
        );
        let report = reg.build_report();

        let data = HealthCheckData::from(&report);
        assert_eq!(data.overall_tier, "yellow");
        assert!(data.healthy);
        assert!(data.has_warnings);
        assert_eq!(data.checks.len(), 2);
        assert_eq!(data.summary.pass, 1);
        assert_eq!(data.summary.warn, 1);
    }

    #[test]
    fn health_check_data_serde_roundtrip() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("test", "Test", "OK"));
        let report = reg.build_report();
        let data = HealthCheckData::from(&report);

        let json = serde_json::to_string(&data).unwrap();
        let rt: HealthCheckData = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.overall_tier, "green");
        assert!(rt.healthy);
    }

    #[test]
    fn incident_enrichment_data_from() {
        let enrichment = IncidentEnrichment::new(HealthTier::Red, RuntimePhase::Draining);
        let data = IncidentEnrichmentData::from(&enrichment);
        assert_eq!(data.health_tier, "red");
        assert_eq!(data.phase, "draining");
        assert_eq!(data.recent_event_count, 0);
        assert_eq!(data.recent_record_count, 0);
        assert!(!data.has_doctor_report);
    }

    // ── Status counts ──

    #[test]
    fn status_counts_total() {
        let counts = StatusCounts {
            pass: 3,
            warn: 1,
            fail: 2,
            skip: 1,
        };
        assert_eq!(counts.total(), 7);
    }

    // ── Policy dashboard health checks ──

    #[test]
    fn policy_checks_healthy_dashboard_all_pass() {
        use crate::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem(
            "test",
            PolicySubsystemInput {
                evaluations: 100,
                denials: 2,
                ..Default::default()
            },
        );
        let dash = collector.dashboard(1000);
        let checks = checks_from_policy_dashboard(&dash);
        assert_eq!(checks.len(), 5);
        for check in &checks {
            assert_eq!(
                check.status,
                CheckStatus::Pass,
                "check {} should pass",
                check.check_id
            );
        }
    }

    #[test]
    fn policy_checks_kill_switch_critical() {
        use crate::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_kill_switch(true);
        let dash = collector.dashboard(1000);
        let checks = checks_from_policy_dashboard(&dash);
        let ks = checks
            .iter()
            .find(|c| c.check_id == "policy.kill_switch")
            .unwrap();
        assert_eq!(ks.status, CheckStatus::Fail);
        assert_eq!(ks.tier, HealthTier::Red);
        assert!(!ks.remediation.is_empty());
    }

    #[test]
    fn policy_checks_high_denial_warning() {
        use crate::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_subsystem(
            "test",
            PolicySubsystemInput {
                evaluations: 100,
                denials: 15, // 15% > default warning threshold of 10%
                ..Default::default()
            },
        );
        let dash = collector.dashboard(1000);
        let checks = checks_from_policy_dashboard(&dash);
        let dr = checks
            .iter()
            .find(|c| c.check_id == "policy.denial_rate")
            .unwrap();
        assert_eq!(dr.status, CheckStatus::Warn);
        assert_eq!(dr.tier, HealthTier::Yellow);
    }

    #[test]
    fn policy_checks_invalid_chain_critical() {
        use crate::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        collector.update_audit_chain(50, false);
        let dash = collector.dashboard(1000);
        let checks = checks_from_policy_dashboard(&dash);
        let chain = checks
            .iter()
            .find(|c| c.check_id == "policy.audit_chain_integrity")
            .unwrap();
        assert_eq!(chain.status, CheckStatus::Fail);
        assert_eq!(chain.failure_class, Some(FailureClass::Corruption));
    }

    #[test]
    fn policy_checks_can_register_in_health_registry() {
        use crate::policy_metrics::*;
        let mut collector = PolicyMetricsCollector::new(PolicyMetricsThresholds::default());
        let dash = collector.dashboard(1000);
        let checks = checks_from_policy_dashboard(&dash);

        let mut registry = HealthCheckRegistry::new();
        for check in checks {
            registry.register(check);
        }
        let report = registry.build_report();
        assert!(report.overall_healthy());
        assert_eq!(report.checks.len(), 5);
    }
}
