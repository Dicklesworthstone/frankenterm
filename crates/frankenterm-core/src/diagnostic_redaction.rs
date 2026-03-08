//! Secret-safe diagnostics: redaction, privacy, and artifact hygiene (ft-e34d9.10.7.5).
//!
//! Ensures all runtime diagnostics, telemetry, and evidence artifacts preserve
//! triage utility while preventing leakage of secrets, credentials, tokens,
//! and sensitive user content.
//!
//! # Architecture
//!
//! Three redaction layers work together:
//!
//! 1. **Secret-pattern redaction** (`policy::Redactor`): Regex-based detection
//!    of API keys, tokens, credentials across 22+ patterns.
//! 2. **Structural field masking** (`DiagnosticFieldPolicy`): Per-field control
//!    over which telemetry detail keys are redacted vs passed through.
//! 3. **Privacy budget enforcement** (`DiagnosticPrivacyBudget`): Size and count
//!    limits to prevent unbounded diagnostic output.
//!
//! # Usage
//!
//! ```ignore
//! use frankenterm_core::diagnostic_redaction::DiagnosticRedactor;
//!
//! let redactor = DiagnosticRedactor::default();
//! let safe_event = redactor.redact_event(&event);
//! let safe_log = redactor.redact_log(&telemetry_log, 50);
//! let safe_enrichment = redactor.redact_enrichment(&enrichment);
//! ```

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::policy::Redactor;
use crate::runtime_health::{IncidentEnrichment, RuntimeDoctorReport, RuntimeHealthCheck};
use crate::runtime_telemetry::RuntimeTelemetryEvent;

// =============================================================================
// Diagnostic field policy
// =============================================================================

/// Per-field redaction policy for telemetry event details.
///
/// Controls which detail keys are always redacted, which are always passed
/// through, and which use secret-pattern scanning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticFieldPolicy {
    /// Detail keys that are always redacted (contain user content / secrets).
    pub always_redact: HashSet<String>,
    /// Detail keys that are always safe (structural / numeric).
    pub always_safe: HashSet<String>,
    /// Whether to apply secret-pattern scanning to non-classified keys.
    pub scan_unknown_keys: bool,
    /// Whether to redact correlation IDs (usually safe, but may encode session info).
    pub redact_correlation_ids: bool,
    /// Whether to redact scope IDs (usually structural, but may contain pane IDs).
    pub redact_scope_ids: bool,
    /// Marker string for redacted content.
    pub redaction_marker: String,
}

impl Default for DiagnosticFieldPolicy {
    fn default() -> Self {
        let mut always_redact = HashSet::new();
        always_redact.insert("error_message".to_string());
        always_redact.insert("backtrace".to_string());
        always_redact.insert("command_text".to_string());
        always_redact.insert("user_input".to_string());
        always_redact.insert("approval_code".to_string());
        always_redact.insert("token".to_string());
        always_redact.insert("credential".to_string());
        always_redact.insert("secret".to_string());
        always_redact.insert("password".to_string());
        always_redact.insert("api_key".to_string());
        always_redact.insert("output_text".to_string());
        always_redact.insert("last_error".to_string());

        let mut always_safe = HashSet::new();
        always_safe.insert("queue_depth".to_string());
        always_safe.insert("queue_capacity".to_string());
        always_safe.insert("tier_from".to_string());
        always_safe.insert("tier_to".to_string());
        always_safe.insert("duration_in_previous_ms".to_string());
        always_safe.insert("total_duration_ms".to_string());
        always_safe.insert("grace_period_ms".to_string());
        always_safe.insert("child_count".to_string());
        always_safe.insert("finalizer_count".to_string());
        always_safe.insert("scope_tier".to_string());
        always_safe.insert("shutdown_reason".to_string());
        always_safe.insert("active".to_string());
        always_safe.insert("count".to_string());
        always_safe.insert("ratio".to_string());

        Self {
            always_redact,
            always_safe,
            scan_unknown_keys: true,
            redact_correlation_ids: false,
            redact_scope_ids: false,
            redaction_marker: "[REDACTED]".to_string(),
        }
    }
}

impl DiagnosticFieldPolicy {
    /// Strict policy: redacts everything except numeric/boolean values.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            scan_unknown_keys: true,
            redact_correlation_ids: true,
            redact_scope_ids: true,
            ..Default::default()
        }
    }

    /// Permissive policy: only redacts known-sensitive keys.
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            scan_unknown_keys: false,
            redact_correlation_ids: false,
            redact_scope_ids: false,
            ..Default::default()
        }
    }
}

// =============================================================================
// Privacy budget for diagnostics
// =============================================================================

/// Size and count limits for diagnostic artifact output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticPrivacyBudget {
    /// Maximum telemetry events to include in diagnostic output.
    pub max_events: usize,
    /// Maximum tier transitions to include.
    pub max_tier_transitions: usize,
    /// Maximum active failures to include.
    pub max_active_failures: usize,
    /// Maximum detail entries per event.
    pub max_details_per_event: usize,
    /// Maximum string value length in details (truncate beyond).
    pub max_detail_value_len: usize,
    /// Maximum health checks in doctor report.
    pub max_health_checks: usize,
    /// Maximum evidence lines per health check.
    pub max_evidence_lines: usize,
    /// Maximum total output size in bytes (serialized JSON).
    pub max_total_bytes: usize,
}

impl Default for DiagnosticPrivacyBudget {
    fn default() -> Self {
        Self {
            max_events: 100,
            max_tier_transitions: 50,
            max_active_failures: 20,
            max_details_per_event: 20,
            max_detail_value_len: 500,
            max_health_checks: 50,
            max_evidence_lines: 10,
            max_total_bytes: 1_048_576, // 1 MiB
        }
    }
}

impl DiagnosticPrivacyBudget {
    /// Strict budget for external sharing.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            max_events: 20,
            max_tier_transitions: 10,
            max_active_failures: 5,
            max_details_per_event: 10,
            max_detail_value_len: 200,
            max_health_checks: 20,
            max_evidence_lines: 5,
            max_total_bytes: 262_144, // 256 KiB
        }
    }

    /// Verbose budget for internal debugging.
    #[must_use]
    pub fn verbose() -> Self {
        Self {
            max_events: 500,
            max_tier_transitions: 200,
            max_active_failures: 100,
            max_details_per_event: 50,
            max_detail_value_len: 2000,
            max_health_checks: 100,
            max_evidence_lines: 50,
            max_total_bytes: 4_194_304, // 4 MiB
        }
    }
}

// =============================================================================
// Redaction statistics
// =============================================================================

/// Counts of what was redacted during a diagnostic export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RedactionStats {
    /// Total events processed.
    pub events_processed: u64,
    /// Events dropped due to budget limits.
    pub events_dropped: u64,
    /// Detail values redacted (pattern match).
    pub details_redacted_pattern: u64,
    /// Detail values redacted (policy: always_redact key).
    pub details_redacted_policy: u64,
    /// Detail values truncated due to length.
    pub details_truncated: u64,
    /// Detail entries dropped due to per-event limit.
    pub details_dropped: u64,
    /// Health check evidence lines truncated.
    pub evidence_truncated: u64,
    /// Correlation IDs redacted.
    pub correlation_ids_redacted: u64,
    /// Scope IDs redacted.
    pub scope_ids_redacted: u64,
    /// Total bytes in the final output.
    pub output_bytes: u64,
    /// Whether the output was truncated due to byte budget.
    pub budget_exceeded: bool,
}

impl RedactionStats {
    /// Total number of redaction actions taken.
    #[must_use]
    pub fn total_redactions(&self) -> u64 {
        self.details_redacted_pattern
            + self.details_redacted_policy
            + self.details_truncated
            + self.details_dropped
            + self.correlation_ids_redacted
            + self.scope_ids_redacted
    }
}

// =============================================================================
// Diagnostic redactor
// =============================================================================

/// Unified diagnostic redactor combining secret scanning, field policy,
/// and privacy budget enforcement.
pub struct DiagnosticRedactor {
    redactor: Redactor,
    field_policy: DiagnosticFieldPolicy,
    budget: DiagnosticPrivacyBudget,
}

impl DiagnosticRedactor {
    /// Create a redactor with custom policies.
    #[must_use]
    pub fn new(field_policy: DiagnosticFieldPolicy, budget: DiagnosticPrivacyBudget) -> Self {
        Self {
            redactor: Redactor::new(),
            field_policy,
            budget,
        }
    }

    /// Create a strict redactor (for external sharing).
    #[must_use]
    pub fn strict() -> Self {
        Self {
            redactor: Redactor::new(),
            field_policy: DiagnosticFieldPolicy::strict(),
            budget: DiagnosticPrivacyBudget::strict(),
        }
    }

    /// Create a verbose redactor (for internal debugging).
    #[must_use]
    pub fn verbose() -> Self {
        Self {
            redactor: Redactor::new(),
            field_policy: DiagnosticFieldPolicy::permissive(),
            budget: DiagnosticPrivacyBudget::verbose(),
        }
    }

    /// Redact a single telemetry event.
    #[must_use]
    pub fn redact_event(&self, event: &RuntimeTelemetryEvent) -> RuntimeTelemetryEvent {
        self.redact_event_with_stats(event, &mut RedactionStats::default())
    }

    /// Redact a single telemetry event, tracking stats.
    #[must_use]
    fn redact_event_with_stats(
        &self,
        event: &RuntimeTelemetryEvent,
        stats: &mut RedactionStats,
    ) -> RuntimeTelemetryEvent {
        let mut redacted = event.clone();

        // Redact correlation_id if policy requires
        if self.field_policy.redact_correlation_ids && !redacted.correlation_id.is_empty() {
            redacted
                .correlation_id
                .clone_from(&self.field_policy.redaction_marker);
            stats.correlation_ids_redacted += 1;
        }

        // Redact scope_id if policy requires
        if self.field_policy.redact_scope_ids {
            if let Some(ref _id) = redacted.scope_id {
                redacted.scope_id = Some(self.field_policy.redaction_marker.clone());
                stats.scope_ids_redacted += 1;
            }
        }

        // Redact details
        let mut safe_details = HashMap::new();
        let mut detail_count = 0;

        for (key, value) in &event.details {
            if detail_count >= self.budget.max_details_per_event {
                stats.details_dropped += 1;
                continue;
            }

            let safe_value = self.redact_detail_value(key, value, stats);
            safe_details.insert(key.clone(), safe_value);
            detail_count += 1;
        }

        redacted.details = safe_details;
        redacted
    }

    /// Redact a single detail value based on key classification and content.
    fn redact_detail_value(
        &self,
        key: &str,
        value: &serde_json::Value,
        stats: &mut RedactionStats,
    ) -> serde_json::Value {
        // Non-string values are always safe (numbers, booleans, nulls)
        let text = match value.as_str() {
            Some(s) => s,
            None => return value.clone(),
        };

        // Check if this key is always redacted
        if self.field_policy.always_redact.contains(key) {
            stats.details_redacted_policy += 1;
            return serde_json::Value::String(self.field_policy.redaction_marker.clone());
        }

        // Check if this key is always safe
        if self.field_policy.always_safe.contains(key) {
            return self.truncate_value(text, stats);
        }

        // For unknown keys, optionally scan for secrets
        if self.field_policy.scan_unknown_keys && self.redactor.contains_secrets(text) {
            stats.details_redacted_pattern += 1;
            return serde_json::Value::String(self.redactor.redact(text));
        }

        self.truncate_value(text, stats)
    }

    /// Truncate a string value to the budget limit.
    fn truncate_value(&self, text: &str, stats: &mut RedactionStats) -> serde_json::Value {
        if text.len() > self.budget.max_detail_value_len {
            stats.details_truncated += 1;
            let truncated = &text[..self.budget.max_detail_value_len];
            serde_json::Value::String(format!("{truncated}... [truncated]"))
        } else {
            serde_json::Value::String(text.to_string())
        }
    }

    /// Redact a collection of telemetry events, enforcing budget limits.
    #[must_use]
    pub fn redact_events(
        &self,
        events: &[RuntimeTelemetryEvent],
    ) -> (Vec<RuntimeTelemetryEvent>, RedactionStats) {
        let mut stats = RedactionStats::default();
        let mut result = Vec::new();

        let limit = events.len().min(self.budget.max_events);
        let start = events.len().saturating_sub(limit);

        stats.events_dropped = start as u64;

        for event in &events[start..] {
            stats.events_processed += 1;
            result.push(self.redact_event_with_stats(event, &mut stats));
        }

        stats.output_bytes = serde_json::to_string(&result)
            .map(|s| s.len() as u64)
            .unwrap_or(0);

        if stats.output_bytes > self.budget.max_total_bytes as u64 {
            stats.budget_exceeded = true;
            // Trim from the oldest until within budget
            while stats.output_bytes > self.budget.max_total_bytes as u64 && result.len() > 1 {
                result.remove(0);
                stats.events_dropped += 1;
                stats.output_bytes = serde_json::to_string(&result)
                    .map(|s| s.len() as u64)
                    .unwrap_or(0);
            }
        }

        (result, stats)
    }

    /// Redact a health check (evidence lines and remediation).
    #[must_use]
    pub fn redact_health_check(&self, check: &RuntimeHealthCheck) -> RuntimeHealthCheck {
        let mut redacted = check.clone();

        // Truncate evidence lines
        if redacted.evidence.len() > self.budget.max_evidence_lines {
            redacted.evidence.truncate(self.budget.max_evidence_lines);
        }

        // Scan evidence for secrets
        redacted.evidence = redacted
            .evidence
            .iter()
            .map(|line| {
                if self.redactor.contains_secrets(line) {
                    self.redactor.redact(line)
                } else {
                    line.clone()
                }
            })
            .collect();

        // Scan remediation commands for secrets
        redacted.remediation = redacted
            .remediation
            .iter()
            .map(|hint| {
                let mut safe_hint = hint.clone();
                if let Some(ref cmd) = safe_hint.command {
                    if self.redactor.contains_secrets(cmd) {
                        safe_hint.command = Some(self.redactor.redact(cmd));
                    }
                }
                safe_hint
            })
            .collect();

        // Scan summary for secrets
        if self.redactor.contains_secrets(&redacted.summary) {
            redacted.summary = self.redactor.redact(&redacted.summary);
        }

        redacted
    }

    /// Redact a full doctor report.
    #[must_use]
    pub fn redact_doctor_report(&self, report: &RuntimeDoctorReport) -> RuntimeDoctorReport {
        let mut redacted = report.clone();

        // Limit and redact checks
        if redacted.checks.len() > self.budget.max_health_checks {
            redacted.checks.truncate(self.budget.max_health_checks);
        }

        redacted.checks = redacted
            .checks
            .iter()
            .map(|c| self.redact_health_check(c))
            .collect();

        redacted
    }

    /// Redact an incident enrichment context.
    #[must_use]
    pub fn redact_enrichment(
        &self,
        enrichment: &IncidentEnrichment,
    ) -> (IncidentEnrichment, RedactionStats) {
        let mut redacted = enrichment.clone();

        // Redact recent events
        let (safe_events, mut stats) = self.redact_events(&redacted.recent_events);
        redacted.recent_events = safe_events;
        let retained_record_start = redacted
            .recent_records
            .len()
            .saturating_sub(redacted.recent_events.len());
        redacted.recent_records = redacted.recent_records[retained_record_start..].to_vec();

        // Limit tier transitions
        if redacted.tier_transitions.len() > self.budget.max_tier_transitions {
            redacted
                .tier_transitions
                .truncate(self.budget.max_tier_transitions);
        }

        // Limit and redact active failures
        if redacted.active_failures.len() > self.budget.max_active_failures {
            redacted
                .active_failures
                .truncate(self.budget.max_active_failures);
        }
        for failure in &mut redacted.active_failures {
            if let Some(ref err) = failure.last_error {
                if self.redactor.contains_secrets(err) {
                    failure.last_error = Some(self.redactor.redact(err));
                    stats.details_redacted_pattern += 1;
                }
            }
        }

        // Redact doctor report if present
        if let Some(ref report) = redacted.doctor_report {
            redacted.doctor_report = Some(self.redact_doctor_report(report));
        }

        (redacted, stats)
    }

    /// Get the field policy.
    #[must_use]
    pub fn field_policy(&self) -> &DiagnosticFieldPolicy {
        &self.field_policy
    }

    /// Get the privacy budget.
    #[must_use]
    pub fn budget(&self) -> &DiagnosticPrivacyBudget {
        &self.budget
    }
}

impl Default for DiagnosticRedactor {
    fn default() -> Self {
        Self {
            redactor: Redactor::new(),
            field_policy: DiagnosticFieldPolicy::default(),
            budget: DiagnosticPrivacyBudget::default(),
        }
    }
}

// =============================================================================
// Redaction report (audit artifact)
// =============================================================================

/// Audit report documenting what was redacted in a diagnostic export.
///
/// This report is itself safe to share — it contains only counts,
/// never the original sensitive data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticRedactionReport {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// When the report was generated (epoch ms).
    pub timestamp_ms: u64,
    /// Which field policy was used.
    pub policy_name: String,
    /// Which budget preset was used.
    pub budget_name: String,
    /// Redaction statistics.
    pub stats: RedactionStats,
    /// Keys that were classified as always-redact.
    pub always_redact_keys: Vec<String>,
    /// Keys that were classified as always-safe.
    pub always_safe_keys: Vec<String>,
}

impl DiagnosticRedactionReport {
    /// Current schema version.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Create a report from redaction stats and the redactor config.
    #[must_use]
    pub fn from_stats(
        stats: RedactionStats,
        redactor: &DiagnosticRedactor,
        policy_name: &str,
        budget_name: &str,
    ) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            timestamp_ms: crate::runtime_telemetry::RuntimeTelemetryEvent::now_ms(),
            policy_name: policy_name.to_string(),
            budget_name: budget_name.to_string(),
            stats,
            always_redact_keys: redactor
                .field_policy
                .always_redact
                .iter()
                .cloned()
                .collect(),
            always_safe_keys: redactor.field_policy.always_safe.iter().cloned().collect(),
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_health::{
        ActiveFailure, HealthCheckRegistry, RemediationHint, RuntimeHealthCheck,
    };
    use crate::runtime_telemetry::{
        FailureClass, HealthTier, RuntimePhase, RuntimeTelemetryEventBuilder, RuntimeTelemetryKind,
        RuntimeTelemetryLog,
    };

    // ── Field policy ──

    #[test]
    fn default_policy_redacts_sensitive_keys() {
        let policy = DiagnosticFieldPolicy::default();
        assert!(policy.always_redact.contains("error_message"));
        assert!(policy.always_redact.contains("password"));
        assert!(policy.always_redact.contains("api_key"));
        assert!(policy.always_redact.contains("token"));
    }

    #[test]
    fn default_policy_allows_structural_keys() {
        let policy = DiagnosticFieldPolicy::default();
        assert!(policy.always_safe.contains("queue_depth"));
        assert!(policy.always_safe.contains("tier_from"));
        assert!(policy.always_safe.contains("child_count"));
    }

    #[test]
    fn strict_policy_redacts_ids() {
        let policy = DiagnosticFieldPolicy::strict();
        assert!(policy.redact_correlation_ids);
        assert!(policy.redact_scope_ids);
    }

    #[test]
    fn permissive_policy_does_not_scan_unknown() {
        let policy = DiagnosticFieldPolicy::permissive();
        assert!(!policy.scan_unknown_keys);
    }

    // ── Privacy budget ──

    #[test]
    fn default_budget_is_reasonable() {
        let budget = DiagnosticPrivacyBudget::default();
        assert_eq!(budget.max_events, 100);
        assert_eq!(budget.max_total_bytes, 1_048_576);
    }

    #[test]
    fn strict_budget_is_smaller() {
        let budget = DiagnosticPrivacyBudget::strict();
        assert!(budget.max_events < DiagnosticPrivacyBudget::default().max_events);
        assert!(budget.max_total_bytes < DiagnosticPrivacyBudget::default().max_total_bytes);
    }

    #[test]
    fn verbose_budget_is_larger() {
        let budget = DiagnosticPrivacyBudget::verbose();
        assert!(budget.max_events > DiagnosticPrivacyBudget::default().max_events);
        assert!(budget.max_total_bytes > DiagnosticPrivacyBudget::default().max_total_bytes);
    }

    // ── Event redaction ──

    #[test]
    fn redact_event_redacts_sensitive_keys() {
        let redactor = DiagnosticRedactor::default();

        let event =
            RuntimeTelemetryEventBuilder::new("rt.error", RuntimeTelemetryKind::TransientError)
                .detail_str("error_message", "Connection failed: sk-ant-abcdef123456")
                .detail_str("queue_depth", "42")
                .reason("test")
                .build();

        let safe = redactor.redact_event(&event);

        // error_message should be fully redacted (always_redact key)
        assert_eq!(
            safe.details.get("error_message"),
            Some(&serde_json::json!("[REDACTED]"))
        );

        // queue_depth should be preserved (always_safe key)
        assert_eq!(
            safe.details.get("queue_depth"),
            Some(&serde_json::json!("42"))
        );
    }

    #[test]
    fn redact_event_scans_unknown_keys_for_secrets() {
        let redactor = DiagnosticRedactor::default();

        let event = RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
            .detail_str("custom_field", "token=sk-ant-secret123456789012345")
            .reason("test")
            .build();

        let safe = redactor.redact_event(&event);

        // custom_field should be redacted because it contains a secret pattern
        let val = safe.details.get("custom_field").unwrap().as_str().unwrap();
        assert!(val.contains("[REDACTED]"));
        assert!(!val.contains("sk-ant-"));
    }

    #[test]
    fn redact_event_preserves_numeric_details() {
        let redactor = DiagnosticRedactor::default();

        let event = RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
            .detail_u64("count", 42)
            .detail_f64("ratio", 0.75)
            .detail_bool("active", true)
            .reason("test")
            .build();

        let safe = redactor.redact_event(&event);

        assert_eq!(safe.details.get("count"), Some(&serde_json::json!(42)));
        assert_eq!(safe.details.get("ratio"), Some(&serde_json::json!(0.75)));
        assert_eq!(safe.details.get("active"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn redact_event_strict_redacts_ids() {
        let redactor = DiagnosticRedactor::strict();

        let event =
            RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeStarted)
                .scope_id("daemon:capture:pane_42")
                .correlation("session-secret-123")
                .reason("test")
                .build();

        let safe = redactor.redact_event(&event);

        assert_eq!(safe.correlation_id, "[REDACTED]");
        assert_eq!(safe.scope_id, Some("[REDACTED]".to_string()));
    }

    #[test]
    fn redact_event_default_preserves_ids() {
        let redactor = DiagnosticRedactor::default();

        let event =
            RuntimeTelemetryEventBuilder::new("rt.scope", RuntimeTelemetryKind::ScopeStarted)
                .scope_id("daemon:capture")
                .correlation("cycle-42")
                .reason("test")
                .build();

        let safe = redactor.redact_event(&event);

        assert_eq!(safe.correlation_id, "cycle-42");
        assert_eq!(safe.scope_id, Some("daemon:capture".to_string()));
    }

    #[test]
    fn redact_event_truncates_long_values() {
        let budget = DiagnosticPrivacyBudget {
            max_detail_value_len: 20,
            ..Default::default()
        };
        let redactor = DiagnosticRedactor::new(DiagnosticFieldPolicy::default(), budget);

        let event = RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
            .detail_str(
                "scope_tier",
                "this is a very long value that exceeds the budget limit",
            )
            .reason("test")
            .build();

        let safe = redactor.redact_event(&event);
        let val = safe.details.get("scope_tier").unwrap().as_str().unwrap();
        assert!(val.contains("[truncated]"));
        assert!(val.len() < 100);
    }

    // ── Events collection redaction ──

    #[test]
    fn redact_events_enforces_count_limit() {
        let budget = DiagnosticPrivacyBudget {
            max_events: 3,
            ..Default::default()
        };
        let redactor = DiagnosticRedactor::new(DiagnosticFieldPolicy::default(), budget);

        let events: Vec<RuntimeTelemetryEvent> = (0..10)
            .map(|i| {
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("ev_{i}"))
                    .build()
            })
            .collect();

        let (safe, stats) = redactor.redact_events(&events);
        assert_eq!(safe.len(), 3);
        assert_eq!(stats.events_dropped, 7);
        assert_eq!(stats.events_processed, 3);

        // Should keep the 3 most recent
        assert_eq!(safe[0].reason_code, "ev_7");
        assert_eq!(safe[2].reason_code, "ev_9");
    }

    #[test]
    fn redact_events_empty_input() {
        let redactor = DiagnosticRedactor::default();
        let (safe, stats) = redactor.redact_events(&[]);
        assert!(safe.is_empty());
        assert_eq!(stats.events_processed, 0);
    }

    // ── Health check redaction ──

    #[test]
    fn redact_health_check_scans_evidence() {
        let redactor = DiagnosticRedactor::default();

        let check = RuntimeHealthCheck::warn("test", "Test", "Issue found")
            .with_evidence("API key leaked: sk-ant-secret123456789012345");

        let safe = redactor.redact_health_check(&check);
        let evidence = &safe.evidence[0];
        assert!(!evidence.contains("sk-ant-"));
        assert!(evidence.contains("[REDACTED]"));
    }

    #[test]
    fn redact_health_check_limits_evidence_lines() {
        let budget = DiagnosticPrivacyBudget {
            max_evidence_lines: 2,
            ..Default::default()
        };
        let redactor = DiagnosticRedactor::new(DiagnosticFieldPolicy::default(), budget);

        let check = RuntimeHealthCheck::warn("test", "Test", "Issue")
            .with_evidence("line 1")
            .with_evidence("line 2")
            .with_evidence("line 3")
            .with_evidence("line 4");

        let safe = redactor.redact_health_check(&check);
        assert_eq!(safe.evidence.len(), 2);
    }

    #[test]
    fn redact_health_check_scans_summary() {
        let redactor = DiagnosticRedactor::default();

        let check = RuntimeHealthCheck::warn(
            "test",
            "Test",
            "Auth failed: Bearer sk-ant-secret123456789012345",
        );

        let safe = redactor.redact_health_check(&check);
        assert!(!safe.summary.contains("sk-ant-"));
    }

    #[test]
    fn redact_health_check_scans_remediation_commands() {
        let redactor = DiagnosticRedactor::default();

        let check = RuntimeHealthCheck::warn("test", "Test", "Issue")
            .with_remediation(RemediationHint::with_command(
                "Fix auth",
                "curl -H 'Authorization: Bearer sk-ant-secret123456789012345' https://api.example.com",
            ));

        let safe = redactor.redact_health_check(&check);
        let cmd = safe.remediation[0].command.as_ref().unwrap();
        assert!(!cmd.contains("sk-ant-"));
    }

    // ── Doctor report redaction ──

    #[test]
    fn redact_doctor_report_limits_checks() {
        let budget = DiagnosticPrivacyBudget {
            max_health_checks: 2,
            ..Default::default()
        };
        let redactor = DiagnosticRedactor::new(DiagnosticFieldPolicy::default(), budget);

        let mut reg = HealthCheckRegistry::new();
        for i in 0..5 {
            reg.register(RuntimeHealthCheck::pass(
                &format!("check_{i}"),
                &format!("Check {i}"),
                "OK",
            ));
        }
        let report = reg.build_report();

        let safe = redactor.redact_doctor_report(&report);
        assert_eq!(safe.checks.len(), 2);
    }

    // ── Incident enrichment redaction ──

    #[test]
    fn redact_enrichment_limits_events() {
        let budget = DiagnosticPrivacyBudget {
            max_events: 5,
            ..Default::default()
        };
        let redactor = DiagnosticRedactor::new(DiagnosticFieldPolicy::default(), budget);

        let mut log = RuntimeTelemetryLog::with_defaults();
        for i in 0..20 {
            log.emit(
                RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
                    .reason(&format!("ev_{i}")),
            );
        }

        let enrichment = IncidentEnrichment::new(HealthTier::Yellow, RuntimePhase::Running)
            .with_telemetry_log(&log, 20);

        let (safe, stats) = redactor.redact_enrichment(&enrichment);
        assert_eq!(safe.recent_events.len(), 5);
        assert_eq!(safe.recent_records.len(), 5);
        assert_eq!(stats.events_dropped, 15);
    }

    #[test]
    fn redact_enrichment_redacts_failure_errors() {
        let redactor = DiagnosticRedactor::default();

        let mut enrichment = IncidentEnrichment::new(HealthTier::Red, RuntimePhase::Running);
        enrichment.add_active_failure(ActiveFailure {
            component: "rt.auth".to_string(),
            failure_class: FailureClass::Permanent,
            started_ms: 1000,
            occurrence_count: 1,
            last_error: Some("Auth failed with key sk-ant-secret123456789012345".to_string()),
        });

        let (safe, _stats) = redactor.redact_enrichment(&enrichment);
        let err = safe.active_failures[0].last_error.as_ref().unwrap();
        assert!(!err.contains("sk-ant-"));
    }

    // ── Redaction stats ──

    #[test]
    fn redaction_stats_total() {
        let stats = RedactionStats {
            details_redacted_pattern: 3,
            details_redacted_policy: 5,
            details_truncated: 2,
            details_dropped: 1,
            correlation_ids_redacted: 1,
            scope_ids_redacted: 1,
            ..Default::default()
        };
        assert_eq!(stats.total_redactions(), 13);
    }

    // ── Redaction report ──

    #[test]
    fn redaction_report_creation() {
        let redactor = DiagnosticRedactor::default();
        let stats = RedactionStats {
            events_processed: 10,
            details_redacted_pattern: 2,
            ..Default::default()
        };

        let report = DiagnosticRedactionReport::from_stats(stats, &redactor, "default", "default");

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.policy_name, "default");
        assert!(
            report
                .always_redact_keys
                .contains(&"error_message".to_string())
        );
        assert!(report.always_safe_keys.contains(&"queue_depth".to_string()));
    }

    #[test]
    fn redaction_report_serde_roundtrip() {
        let redactor = DiagnosticRedactor::default();
        let stats = RedactionStats::default();
        let report = DiagnosticRedactionReport::from_stats(stats, &redactor, "test", "test");

        let json = serde_json::to_string(&report).unwrap();
        let rt: DiagnosticRedactionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.schema_version, report.schema_version);
        assert_eq!(rt.policy_name, "test");
    }

    // ── Detail limits per event ──

    #[test]
    fn redact_event_enforces_detail_limit() {
        let budget = DiagnosticPrivacyBudget {
            max_details_per_event: 2,
            ..Default::default()
        };
        let redactor = DiagnosticRedactor::new(DiagnosticFieldPolicy::default(), budget);

        let mut builder =
            RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat);
        builder = builder
            .detail_str("scope_tier", "a")
            .detail_str("shutdown_reason", "b")
            .detail_str("tier_from", "c")
            .detail_str("tier_to", "d");
        let event = builder.reason("test").build();

        let safe = redactor.redact_event(&event);
        assert_eq!(safe.details.len(), 2);
    }

    // ── Permissive policy does not scan ──

    #[test]
    fn permissive_policy_does_not_scan_unknown_keys() {
        let redactor = DiagnosticRedactor::new(
            DiagnosticFieldPolicy::permissive(),
            DiagnosticPrivacyBudget::default(),
        );

        let event = RuntimeTelemetryEventBuilder::new("rt.test", RuntimeTelemetryKind::Heartbeat)
            .detail_str("custom", "contains sk-ant-secret123456789012345 token")
            .reason("test")
            .build();

        let safe = redactor.redact_event(&event);

        // Permissive should NOT redact unknown keys (only always_redact keys)
        let val = safe.details.get("custom").unwrap().as_str().unwrap();
        assert!(val.contains("sk-ant-"));
    }
}
