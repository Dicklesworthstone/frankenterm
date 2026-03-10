//! Connector integration testbed — mock providers, chaos scenarios, and audit verifiers.
//!
//! Provides a deterministic test harness for connector subsystem integration:
//! mock provider simulation, sandbox escape attempts, delivery failure storms,
//! audit-chain integrity verification, and cross-cutting connector assertions.
//!
//! Part of ft-3681t.5.13.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::connector_bundles::{IngestionOutcome, IngestionPipeline, IngestionPipelineConfig};
use crate::connector_event_model::{
    CanonicalConnectorEvent, CanonicalSeverity, EventDirection, SchemaVersion,
};
use crate::connector_governor::{ConnectorGovernor, ConnectorGovernorConfig, GovernorVerdict};
use crate::connector_host_runtime::{ConnectorCapability, ConnectorCapabilityEnvelope};
use crate::connector_outbound_bridge::ConnectorAction;

// =============================================================================
// Mock provider
// =============================================================================

/// A mock external provider for testing connector behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MockProvider {
    /// Provider identifier.
    pub provider_id: String,
    /// Whether the provider is online.
    pub online: bool,
    /// Simulated latency in milliseconds.
    pub latency_ms: u64,
    /// Failure rate: 0..=100 (percentage of requests that fail).
    pub failure_rate_pct: u8,
    /// Rate limit: max requests per second (0 = unlimited).
    pub rate_limit_rps: u32,
    /// Requests received.
    pub requests_received: u64,
    /// Requests failed (simulated).
    pub requests_failed: u64,
    /// Request history for assertion.
    pub request_log: VecDeque<MockRequest>,
    /// Maximum request log entries.
    pub max_log_entries: usize,
}

/// A recorded mock request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MockRequest {
    /// Connector that sent the request.
    pub connector_id: String,
    /// Action kind.
    pub action_kind: String,
    /// Timestamp (epoch ms).
    pub at_ms: u64,
    /// Whether the request succeeded.
    pub success: bool,
    /// Failure reason (if any).
    pub failure_reason: Option<String>,
}

/// Outcome of sending a request to a mock provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockProviderOutcome {
    /// Request succeeded.
    Success,
    /// Provider is offline.
    Offline,
    /// Simulated failure (random based on failure_rate_pct).
    SimulatedFailure,
    /// Rate limited.
    RateLimited,
}

impl std::fmt::Display for MockProviderOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => f.write_str("success"),
            Self::Offline => f.write_str("offline"),
            Self::SimulatedFailure => f.write_str("simulated_failure"),
            Self::RateLimited => f.write_str("rate_limited"),
        }
    }
}

impl MockProvider {
    /// Create a new mock provider.
    pub fn new(provider_id: impl Into<String>) -> Self {
        Self {
            provider_id: provider_id.into(),
            online: true,
            latency_ms: 0,
            failure_rate_pct: 0,
            rate_limit_rps: 0,
            requests_received: 0,
            requests_failed: 0,
            request_log: VecDeque::new(),
            max_log_entries: 256,
        }
    }

    /// Set failure rate.
    #[must_use]
    pub fn with_failure_rate(mut self, pct: u8) -> Self {
        self.failure_rate_pct = pct.min(100);
        self
    }

    /// Set rate limit.
    #[must_use]
    pub fn with_rate_limit(mut self, rps: u32) -> Self {
        self.rate_limit_rps = rps;
        self
    }

    /// Take the provider offline.
    pub fn go_offline(&mut self) {
        self.online = false;
    }

    /// Bring the provider online.
    pub fn go_online(&mut self) {
        self.online = true;
    }

    /// Simulate receiving a request.
    ///
    /// Uses `seed` for deterministic failure simulation instead of randomness.
    pub fn receive_request(
        &mut self,
        connector_id: &str,
        action_kind: &str,
        at_ms: u64,
        seed: u64,
    ) -> MockProviderOutcome {
        self.requests_received += 1;

        let outcome = if !self.online {
            MockProviderOutcome::Offline
        } else if self.failure_rate_pct > 0 && (seed % 100) < u64::from(self.failure_rate_pct) {
            MockProviderOutcome::SimulatedFailure
        } else {
            MockProviderOutcome::Success
        };

        let success = matches!(outcome, MockProviderOutcome::Success);
        if !success {
            self.requests_failed += 1;
        }

        let request = MockRequest {
            connector_id: connector_id.to_string(),
            action_kind: action_kind.to_string(),
            at_ms,
            success,
            failure_reason: if success {
                None
            } else {
                Some(outcome.to_string())
            },
        };

        if self.request_log.len() >= self.max_log_entries {
            self.request_log.pop_front();
        }
        self.request_log.push_back(request);

        outcome
    }

    /// Number of successful requests.
    #[must_use]
    pub fn successful_requests(&self) -> u64 {
        self.requests_received - self.requests_failed
    }

    /// Failure ratio (0.0..=1.0).
    #[must_use]
    pub fn failure_ratio(&self) -> f64 {
        if self.requests_received == 0 {
            return 0.0;
        }
        self.requests_failed as f64 / self.requests_received as f64
    }
}

// =============================================================================
// Sandbox escape attempt
// =============================================================================

/// Describes a sandbox escape attempt for chaos testing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxEscapeAttempt {
    /// The capability being tested.
    pub capability: ConnectorCapability,
    /// Target path/host/command.
    pub target: String,
    /// Whether the sandbox should block this.
    pub expected_blocked: bool,
    /// Description of the test.
    pub description: String,
}

impl SandboxEscapeAttempt {
    /// Create a filesystem escape attempt.
    #[must_use]
    pub fn filesystem_read(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            capability: ConnectorCapability::FilesystemRead,
            target: path.clone(),
            expected_blocked: true,
            description: format!("Attempt to read outside sandbox: {path}"),
        }
    }

    /// Create a filesystem write escape attempt.
    #[must_use]
    pub fn filesystem_write(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            capability: ConnectorCapability::FilesystemWrite,
            target: path.clone(),
            expected_blocked: true,
            description: format!("Attempt to write outside sandbox: {path}"),
        }
    }

    /// Create a network egress escape attempt.
    #[must_use]
    pub fn network_egress(host: impl Into<String>) -> Self {
        let host = host.into();
        Self {
            capability: ConnectorCapability::NetworkEgress,
            target: host.clone(),
            expected_blocked: true,
            description: format!("Attempt to reach disallowed host: {host}"),
        }
    }

    /// Create a process execution escape attempt.
    #[must_use]
    pub fn process_exec(command: impl Into<String>) -> Self {
        let cmd = command.into();
        Self {
            capability: ConnectorCapability::ProcessExec,
            target: cmd.clone(),
            expected_blocked: true,
            description: format!("Attempt to execute disallowed command: {cmd}"),
        }
    }

    /// Evaluate this escape attempt against a capability envelope.
    #[must_use]
    pub fn evaluate(&self, envelope: &ConnectorCapabilityEnvelope) -> SandboxEscapeResult {
        let allowed = match self.capability {
            ConnectorCapability::FilesystemRead => {
                envelope.allows_target(ConnectorCapability::FilesystemRead, Some(&self.target))
            }
            ConnectorCapability::FilesystemWrite => {
                envelope.allows_target(ConnectorCapability::FilesystemWrite, Some(&self.target))
            }
            ConnectorCapability::NetworkEgress => {
                envelope.allows_target(ConnectorCapability::NetworkEgress, Some(&self.target))
            }
            ConnectorCapability::ProcessExec => {
                envelope.allows_target(ConnectorCapability::ProcessExec, Some(&self.target))
            }
            other => envelope.allows_capability(other),
        };

        let blocked = !allowed;
        SandboxEscapeResult {
            attempt: self.clone(),
            was_blocked: blocked,
            passed: blocked == self.expected_blocked,
        }
    }
}

/// Result of evaluating a sandbox escape attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxEscapeResult {
    /// The original attempt.
    pub attempt: SandboxEscapeAttempt,
    /// Whether the sandbox blocked the attempt.
    pub was_blocked: bool,
    /// Whether the test passed (blocked == expected_blocked).
    pub passed: bool,
}

// =============================================================================
// Chaos scenario
// =============================================================================

/// A chaos test scenario for connector resilience.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChaosScenario {
    /// Scenario identifier.
    pub scenario_id: String,
    /// Description.
    pub description: String,
    /// Scenario type.
    pub kind: ChaosScenarioKind,
    /// Duration of the chaos event in simulated milliseconds.
    pub duration_ms: u64,
    /// Intensity (0..=100).
    pub intensity_pct: u8,
}

/// Kind of chaos scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaosScenarioKind {
    /// Provider goes offline.
    ProviderOutage,
    /// Provider responds with errors at a given rate.
    ErrorStorm,
    /// Provider rate-limits all requests.
    RateLimitFlood,
    /// Latency spike from the provider.
    LatencySpike,
    /// Sandbox escape attempts.
    SandboxProbe,
    /// Credential rotation during active connections.
    CredentialRotation,
    /// Burst of events overwhelming the ingestion pipeline.
    IngestionFlood,
}

impl std::fmt::Display for ChaosScenarioKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProviderOutage => f.write_str("provider_outage"),
            Self::ErrorStorm => f.write_str("error_storm"),
            Self::RateLimitFlood => f.write_str("rate_limit_flood"),
            Self::LatencySpike => f.write_str("latency_spike"),
            Self::SandboxProbe => f.write_str("sandbox_probe"),
            Self::CredentialRotation => f.write_str("credential_rotation"),
            Self::IngestionFlood => f.write_str("ingestion_flood"),
        }
    }
}

impl ChaosScenario {
    /// Create a provider outage scenario.
    #[must_use]
    pub fn provider_outage(duration_ms: u64) -> Self {
        Self {
            scenario_id: "chaos-provider-outage".to_string(),
            description: "Provider goes completely offline".to_string(),
            kind: ChaosScenarioKind::ProviderOutage,
            duration_ms,
            intensity_pct: 100,
        }
    }

    /// Create an error storm scenario.
    #[must_use]
    pub fn error_storm(duration_ms: u64, failure_rate_pct: u8) -> Self {
        Self {
            scenario_id: "chaos-error-storm".to_string(),
            description: format!("Provider returns errors at {failure_rate_pct}% rate"),
            kind: ChaosScenarioKind::ErrorStorm,
            duration_ms,
            intensity_pct: failure_rate_pct.min(100),
        }
    }

    /// Create a sandbox probe scenario.
    #[must_use]
    pub fn sandbox_probe() -> Self {
        Self {
            scenario_id: "chaos-sandbox-probe".to_string(),
            description: "Attempt sandbox escape via filesystem/network/exec".to_string(),
            kind: ChaosScenarioKind::SandboxProbe,
            duration_ms: 0,
            intensity_pct: 100,
        }
    }

    /// Create an ingestion flood scenario.
    #[must_use]
    pub fn ingestion_flood(events_per_sec: u8) -> Self {
        Self {
            scenario_id: "chaos-ingestion-flood".to_string(),
            description: format!("Flood ingestion pipeline with {events_per_sec} events/sec"),
            kind: ChaosScenarioKind::IngestionFlood,
            duration_ms: 5000,
            intensity_pct: events_per_sec.min(100),
        }
    }
}

// =============================================================================
// Testbed configuration
// =============================================================================

/// Configuration for the integration testbed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TestbedConfig {
    /// Maximum mock providers.
    pub max_providers: usize,
    /// Maximum sandbox escape attempts to track.
    pub max_escape_results: usize,
    /// Maximum chaos scenario results to track.
    pub max_scenario_results: usize,
    /// Ingestion pipeline config.
    pub ingestion: IngestionPipelineConfig,
    /// Governor config.
    pub governor: ConnectorGovernorConfig,
}

impl Default for TestbedConfig {
    fn default() -> Self {
        Self {
            max_providers: 32,
            max_escape_results: 256,
            max_scenario_results: 128,
            ingestion: IngestionPipelineConfig::default(),
            governor: ConnectorGovernorConfig::default(),
        }
    }
}

// =============================================================================
// Testbed telemetry
// =============================================================================

/// Telemetry counters for the testbed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TestbedTelemetry {
    pub scenarios_run: u64,
    pub scenarios_passed: u64,
    pub scenarios_failed: u64,
    pub escape_attempts: u64,
    pub escapes_blocked: u64,
    pub escapes_allowed: u64,
    pub provider_requests: u64,
    pub provider_failures: u64,
    pub events_ingested: u64,
    pub governor_evaluations: u64,
    pub governor_rejects: u64,
}

/// Testbed snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestbedSnapshot {
    pub captured_at_ms: u64,
    pub counters: TestbedTelemetry,
    pub provider_count: usize,
    pub escape_result_count: usize,
    pub audit_chain_length: usize,
}

// =============================================================================
// Integration testbed
// =============================================================================

/// The connector integration testbed.
///
/// Orchestrates mock providers, sandbox probes, chaos scenarios, and audit
/// verification against the full connector subsystem stack.
pub struct ConnectorTestbed {
    config: TestbedConfig,
    providers: BTreeMap<String, MockProvider>,
    sandbox_envelope: ConnectorCapabilityEnvelope,
    ingestion: IngestionPipeline,
    governor: ConnectorGovernor,
    escape_results: VecDeque<SandboxEscapeResult>,
    telemetry: TestbedTelemetry,
}

impl ConnectorTestbed {
    /// Create a new testbed with default configuration.
    pub fn new(config: TestbedConfig) -> Self {
        let ingestion = IngestionPipeline::new(config.ingestion.clone());
        let governor = ConnectorGovernor::new(config.governor.clone());
        Self {
            config,
            providers: BTreeMap::new(),
            sandbox_envelope: ConnectorCapabilityEnvelope::default(),
            ingestion,
            governor,
            escape_results: VecDeque::new(),
            telemetry: TestbedTelemetry::default(),
        }
    }

    /// Set the sandbox capability envelope.
    pub fn set_sandbox(&mut self, envelope: ConnectorCapabilityEnvelope) {
        self.sandbox_envelope = envelope;
    }

    /// Register a mock provider.
    pub fn register_provider(&mut self, provider: MockProvider) {
        self.providers
            .insert(provider.provider_id.clone(), provider);
    }

    /// Get a mock provider.
    #[must_use]
    pub fn provider(&self, provider_id: &str) -> Option<&MockProvider> {
        self.providers.get(provider_id)
    }

    /// Get a mutable reference to a mock provider.
    pub fn provider_mut(&mut self, provider_id: &str) -> Option<&mut MockProvider> {
        self.providers.get_mut(provider_id)
    }

    /// Send a request to a mock provider through the governor.
    pub fn send_request(
        &mut self,
        provider_id: &str,
        connector_id: &str,
        action: &ConnectorAction,
        now_ms: u64,
        seed: u64,
    ) -> TestbedRequestResult {
        // Evaluate governor first.
        self.telemetry.governor_evaluations += 1;
        let decision = self.governor.evaluate(action, now_ms);

        if decision.verdict == GovernorVerdict::Reject {
            self.telemetry.governor_rejects += 1;
            return TestbedRequestResult {
                governor_verdict: decision.verdict,
                provider_outcome: None,
                ingestion_outcome: None,
            };
        }

        // Send to provider.
        self.telemetry.provider_requests += 1;
        let provider_outcome = if let Some(provider) = self.providers.get_mut(provider_id) {
            let outcome =
                provider.receive_request(connector_id, &action.action_kind.to_string(), now_ms, seed);
            if !matches!(outcome, MockProviderOutcome::Success) {
                self.telemetry.provider_failures += 1;
                self.governor.record_outcome(connector_id, false, now_ms);
            } else {
                self.governor.record_outcome(connector_id, true, now_ms);
            }
            Some(outcome)
        } else {
            None
        };

        // Create and ingest canonical event.
        let severity = match &provider_outcome {
            Some(MockProviderOutcome::Success) => CanonicalSeverity::Info,
            Some(MockProviderOutcome::Offline) => CanonicalSeverity::Critical,
            _ => CanonicalSeverity::Warning,
        };

        let event = CanonicalConnectorEvent {
            schema_version: SchemaVersion::current(),
            direction: EventDirection::Outbound,
            event_id: format!("test-{connector_id}-{now_ms}"),
            correlation_id: format!("corr-{now_ms}"),
            timestamp_ms: now_ms,
            connector_id: connector_id.to_string(),
            connector_name: Some(connector_id.to_string()),
            event_type: format!("request_{}", action.action_kind),
            severity,
            signal_kind: None,
            signal_sub_type: None,
            event_source: None,
            action_kind: Some(action.action_kind),
            lifecycle_phase: None,
            failure_class: None,
            pane_id: None,
            workflow_id: None,
            zone_id: None,
            capability: None,
            payload: serde_json::Value::Null,
            metadata: BTreeMap::new(),
        };

        self.telemetry.events_ingested += 1;
        let ingestion_outcome = Some(self.ingestion.ingest(&event, now_ms));

        TestbedRequestResult {
            governor_verdict: decision.verdict,
            provider_outcome,
            ingestion_outcome,
        }
    }

    /// Run a sandbox escape probe.
    pub fn run_sandbox_probe(&mut self, attempts: &[SandboxEscapeAttempt]) -> SandboxProbeReport {
        let mut results = Vec::new();
        let mut all_passed = true;

        for attempt in attempts {
            self.telemetry.escape_attempts += 1;
            let result = attempt.evaluate(&self.sandbox_envelope);

            if result.was_blocked {
                self.telemetry.escapes_blocked += 1;
            } else {
                self.telemetry.escapes_allowed += 1;
            }

            if !result.passed {
                all_passed = false;
            }

            if self.escape_results.len() >= self.config.max_escape_results {
                self.escape_results.pop_front();
            }
            self.escape_results.push_back(result.clone());
            results.push(result);
        }

        SandboxProbeReport {
            total_attempts: results.len(),
            all_passed,
            results,
        }
    }

    /// Run an ingestion flood test.
    pub fn run_ingestion_flood(
        &mut self,
        connector_id: &str,
        event_count: usize,
        start_ms: u64,
    ) -> IngestionFloodReport {
        let mut recorded = 0u64;
        let mut filtered = 0u64;
        let mut rejected = 0u64;

        for i in 0..event_count {
            let event = CanonicalConnectorEvent {
                schema_version: SchemaVersion::current(),
                direction: EventDirection::Inbound,
                event_id: format!("flood-{i}"),
                correlation_id: format!("flood-corr-{i}"),
                timestamp_ms: start_ms + (i as u64) * 10,
                connector_id: connector_id.to_string(),
                connector_name: Some(connector_id.to_string()),
                event_type: "flood_event".to_string(),
                severity: CanonicalSeverity::Info,
                signal_kind: None,
                signal_sub_type: None,
                event_source: None,
                action_kind: None,
                lifecycle_phase: None,
                failure_class: None,
                pane_id: None,
                workflow_id: None,
                zone_id: None,
                capability: None,
                payload: serde_json::Value::Null,
                metadata: BTreeMap::new(),
            };

            let now = start_ms + (i as u64) * 10;
            match self.ingestion.ingest(&event, now) {
                IngestionOutcome::Recorded => recorded += 1,
                IngestionOutcome::Filtered => filtered += 1,
                IngestionOutcome::Rejected { .. } => rejected += 1,
            }
        }

        let chain_valid = self.ingestion.verify_chain().valid;

        IngestionFloodReport {
            total_events: event_count as u64,
            recorded,
            filtered,
            rejected,
            chain_integrity_valid: chain_valid,
        }
    }

    /// Get a reference to the ingestion pipeline.
    #[must_use]
    pub fn ingestion(&self) -> &IngestionPipeline {
        &self.ingestion
    }

    /// Get a mutable reference to the ingestion pipeline.
    pub fn ingestion_mut(&mut self) -> &mut IngestionPipeline {
        &mut self.ingestion
    }

    /// Get a reference to the governor.
    #[must_use]
    pub fn governor(&self) -> &ConnectorGovernor {
        &self.governor
    }

    /// Get escape results.
    #[must_use]
    pub fn escape_results(&self) -> &VecDeque<SandboxEscapeResult> {
        &self.escape_results
    }

    /// Get telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &TestbedTelemetry {
        &self.telemetry
    }

    /// Capture a testbed snapshot.
    #[must_use]
    pub fn snapshot(&self, now_ms: u64) -> TestbedSnapshot {
        TestbedSnapshot {
            captured_at_ms: now_ms,
            counters: self.telemetry.clone(),
            provider_count: self.providers.len(),
            escape_result_count: self.escape_results.len(),
            audit_chain_length: self.ingestion.audit_chain().len(),
        }
    }
}

/// Result of sending a request through the testbed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestbedRequestResult {
    /// Governor verdict.
    pub governor_verdict: GovernorVerdict,
    /// Provider outcome (None if governor rejected or provider not found).
    pub provider_outcome: Option<MockProviderOutcome>,
    /// Ingestion outcome (None if governor rejected).
    pub ingestion_outcome: Option<IngestionOutcome>,
}

/// Report from a sandbox probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProbeReport {
    /// Total escape attempts.
    pub total_attempts: usize,
    /// Whether all escape attempts were correctly handled.
    pub all_passed: bool,
    /// Individual results.
    pub results: Vec<SandboxEscapeResult>,
}

/// Report from an ingestion flood test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestionFloodReport {
    /// Total events sent.
    pub total_events: u64,
    /// Events that were recorded.
    pub recorded: u64,
    /// Events that were filtered.
    pub filtered: u64,
    /// Events that were rejected.
    pub rejected: u64,
    /// Whether the audit chain integrity is valid after flood.
    pub chain_integrity_valid: bool,
}

// =============================================================================
// Standard escape attempt suite
// =============================================================================

/// Generate a standard set of sandbox escape attempts.
#[must_use]
pub fn standard_escape_attempts() -> Vec<SandboxEscapeAttempt> {
    vec![
        SandboxEscapeAttempt::filesystem_read("/etc/passwd"),
        SandboxEscapeAttempt::filesystem_read("/root/.ssh/id_rsa"),
        SandboxEscapeAttempt::filesystem_write("/etc/shadow"),
        SandboxEscapeAttempt::filesystem_write("/tmp/malicious.sh"),
        SandboxEscapeAttempt::network_egress("evil.example.com"),
        SandboxEscapeAttempt::network_egress("169.254.169.254"),
        SandboxEscapeAttempt::process_exec("rm -rf /"),
        SandboxEscapeAttempt::process_exec("curl http://evil.example.com"),
    ]
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_outbound_bridge::ConnectorActionKind;

    fn sample_action(connector_id: &str, kind: ConnectorActionKind) -> ConnectorAction {
        ConnectorAction {
            target_connector: connector_id.to_string(),
            action_kind: kind,
            correlation_id: "test-corr".to_string(),
            params: serde_json::Value::Null,
            created_at_ms: 1000,
        }
    }

    // -------------------------------------------------------------------------
    // MockProvider tests
    // -------------------------------------------------------------------------

    #[test]
    fn mock_provider_creation() {
        let p = MockProvider::new("github");
        assert_eq!(p.provider_id, "github");
        assert!(p.online);
        assert_eq!(p.requests_received, 0);
        assert_eq!(p.failure_ratio(), 0.0);
    }

    #[test]
    fn mock_provider_success() {
        let mut p = MockProvider::new("github");
        let outcome = p.receive_request("conn-1", "invoke", 1000, 50);
        assert_eq!(outcome, MockProviderOutcome::Success);
        assert_eq!(p.requests_received, 1);
        assert_eq!(p.successful_requests(), 1);
    }

    #[test]
    fn mock_provider_offline() {
        let mut p = MockProvider::new("github");
        p.go_offline();
        let outcome = p.receive_request("conn-1", "invoke", 1000, 50);
        assert_eq!(outcome, MockProviderOutcome::Offline);
        assert_eq!(p.requests_failed, 1);
        assert!(!p.online);
    }

    #[test]
    fn mock_provider_online_toggle() {
        let mut p = MockProvider::new("github");
        p.go_offline();
        assert!(!p.online);
        p.go_online();
        assert!(p.online);
    }

    #[test]
    fn mock_provider_failure_rate() {
        let mut p = MockProvider::new("github").with_failure_rate(100);
        let outcome = p.receive_request("conn-1", "invoke", 1000, 50);
        assert_eq!(outcome, MockProviderOutcome::SimulatedFailure);
        assert_eq!(p.failure_ratio(), 1.0);
    }

    #[test]
    fn mock_provider_deterministic_failure() {
        // With 50% failure rate, seed < 50 should fail, seed >= 50 should succeed
        let mut p = MockProvider::new("github").with_failure_rate(50);
        let out1 = p.receive_request("conn-1", "invoke", 1000, 30); // 30 < 50 → fail
        assert_eq!(out1, MockProviderOutcome::SimulatedFailure);
        let out2 = p.receive_request("conn-1", "invoke", 1001, 70); // 70 >= 50 → success
        assert_eq!(out2, MockProviderOutcome::Success);
    }

    #[test]
    fn mock_provider_request_log() {
        let mut p = MockProvider::new("github");
        p.max_log_entries = 2;
        p.receive_request("a", "invoke", 1000, 0);
        p.receive_request("b", "read", 2000, 0);
        p.receive_request("c", "stream", 3000, 0);
        // Bounded to 2 entries.
        assert_eq!(p.request_log.len(), 2);
        assert_eq!(p.request_log[0].connector_id, "b");
    }

    #[test]
    fn mock_provider_serde() {
        let p = MockProvider::new("github").with_failure_rate(25);
        let json = serde_json::to_string(&p).unwrap();
        let back: MockProvider = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    // -------------------------------------------------------------------------
    // SandboxEscapeAttempt tests
    // -------------------------------------------------------------------------

    #[test]
    fn escape_filesystem_read() {
        let attempt = SandboxEscapeAttempt::filesystem_read("/etc/passwd");
        assert_eq!(attempt.capability, ConnectorCapability::FilesystemRead);
        assert!(attempt.expected_blocked);

        let envelope = ConnectorCapabilityEnvelope::default();
        let result = attempt.evaluate(&envelope);
        assert!(result.was_blocked);
        assert!(result.passed);
    }

    #[test]
    fn escape_filesystem_write() {
        let attempt = SandboxEscapeAttempt::filesystem_write("/etc/shadow");
        let envelope = ConnectorCapabilityEnvelope::default();
        let result = attempt.evaluate(&envelope);
        assert!(result.was_blocked);
        assert!(result.passed);
    }

    #[test]
    fn escape_network_egress() {
        let attempt = SandboxEscapeAttempt::network_egress("evil.example.com");
        let envelope = ConnectorCapabilityEnvelope::default();
        let result = attempt.evaluate(&envelope);
        assert!(result.was_blocked);
        assert!(result.passed);
    }

    #[test]
    fn escape_process_exec() {
        let attempt = SandboxEscapeAttempt::process_exec("rm -rf /");
        let envelope = ConnectorCapabilityEnvelope::default();
        let result = attempt.evaluate(&envelope);
        assert!(result.was_blocked);
        assert!(result.passed);
    }

    #[test]
    fn standard_escapes_all_blocked_by_default() {
        let attempts = standard_escape_attempts();
        let envelope = ConnectorCapabilityEnvelope::default();
        for attempt in &attempts {
            let result = attempt.evaluate(&envelope);
            assert!(
                result.passed,
                "Expected escape blocked for: {}",
                attempt.description
            );
        }
    }

    // -------------------------------------------------------------------------
    // ChaosScenario tests
    // -------------------------------------------------------------------------

    #[test]
    fn chaos_provider_outage() {
        let s = ChaosScenario::provider_outage(5000);
        assert_eq!(s.kind, ChaosScenarioKind::ProviderOutage);
        assert_eq!(s.duration_ms, 5000);
        assert_eq!(s.intensity_pct, 100);
    }

    #[test]
    fn chaos_error_storm() {
        let s = ChaosScenario::error_storm(3000, 75);
        assert_eq!(s.kind, ChaosScenarioKind::ErrorStorm);
        assert_eq!(s.intensity_pct, 75);
    }

    #[test]
    fn chaos_sandbox_probe() {
        let s = ChaosScenario::sandbox_probe();
        assert_eq!(s.kind, ChaosScenarioKind::SandboxProbe);
    }

    #[test]
    fn chaos_scenario_kind_display() {
        assert_eq!(ChaosScenarioKind::ProviderOutage.to_string(), "provider_outage");
        assert_eq!(ChaosScenarioKind::ErrorStorm.to_string(), "error_storm");
        assert_eq!(ChaosScenarioKind::IngestionFlood.to_string(), "ingestion_flood");
    }

    #[test]
    fn chaos_scenario_serde() {
        let s = ChaosScenario::error_storm(3000, 50);
        let json = serde_json::to_string(&s).unwrap();
        let back: ChaosScenario = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    // -------------------------------------------------------------------------
    // ConnectorTestbed tests
    // -------------------------------------------------------------------------

    #[test]
    fn testbed_creation() {
        let tb = ConnectorTestbed::new(TestbedConfig::default());
        assert_eq!(tb.providers.len(), 0);
        assert_eq!(tb.telemetry().scenarios_run, 0);
    }

    #[test]
    fn testbed_register_provider() {
        let mut tb = ConnectorTestbed::new(TestbedConfig::default());
        tb.register_provider(MockProvider::new("github"));
        assert!(tb.provider("github").is_some());
        assert!(tb.provider("nope").is_none());
    }

    #[test]
    fn testbed_send_request_success() {
        let mut tb = ConnectorTestbed::new(TestbedConfig::default());
        tb.register_provider(MockProvider::new("github"));
        let action = sample_action("conn-1", ConnectorActionKind::Notify);
        let result = tb.send_request("github", "conn-1", &action, 1000, 50);
        assert_eq!(result.governor_verdict, GovernorVerdict::Allow);
        assert_eq!(result.provider_outcome, Some(MockProviderOutcome::Success));
        assert_eq!(result.ingestion_outcome, Some(IngestionOutcome::Recorded));
    }

    #[test]
    fn testbed_send_request_offline() {
        let mut tb = ConnectorTestbed::new(TestbedConfig::default());
        let mut provider = MockProvider::new("github");
        provider.go_offline();
        tb.register_provider(provider);
        let action = sample_action("conn-1", ConnectorActionKind::Notify);
        let result = tb.send_request("github", "conn-1", &action, 1000, 50);
        assert_eq!(result.provider_outcome, Some(MockProviderOutcome::Offline));
        assert_eq!(tb.telemetry().provider_failures, 1);
    }

    #[test]
    fn testbed_sandbox_probe() {
        let mut tb = ConnectorTestbed::new(TestbedConfig::default());
        let attempts = standard_escape_attempts();
        let report = tb.run_sandbox_probe(&attempts);
        assert!(report.all_passed);
        assert_eq!(report.total_attempts, 8);
        assert_eq!(tb.telemetry().escapes_blocked, 8);
        assert_eq!(tb.telemetry().escapes_allowed, 0);
    }

    #[test]
    fn testbed_ingestion_flood() {
        let config = TestbedConfig {
            ingestion: IngestionPipelineConfig {
                max_ingest_per_sec: 5,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut tb = ConnectorTestbed::new(config);
        let report = tb.run_ingestion_flood("conn-1", 20, 1000);
        assert_eq!(report.total_events, 20);
        assert!(report.chain_integrity_valid);
        // With rate limit of 5/sec and 10ms interval (20 events in 200ms),
        // some events should be rejected.
        assert!(report.rejected > 0);
        assert_eq!(report.recorded + report.filtered + report.rejected, 20);
    }

    #[test]
    fn testbed_ingestion_flood_chain_integrity() {
        let mut tb = ConnectorTestbed::new(TestbedConfig::default());
        let report = tb.run_ingestion_flood("conn-1", 100, 1000);
        assert!(report.chain_integrity_valid);
        assert_eq!(report.recorded, 100);
    }

    #[test]
    fn testbed_snapshot() {
        let mut tb = ConnectorTestbed::new(TestbedConfig::default());
        tb.register_provider(MockProvider::new("github"));
        let action = sample_action("conn-1", ConnectorActionKind::Notify);
        tb.send_request("github", "conn-1", &action, 1000, 50);
        let snap = tb.snapshot(2000);
        assert_eq!(snap.captured_at_ms, 2000);
        assert_eq!(snap.provider_count, 1);
        assert_eq!(snap.counters.provider_requests, 1);
        assert_eq!(snap.audit_chain_length, 1);
    }

    #[test]
    fn testbed_telemetry_counters() {
        let mut tb = ConnectorTestbed::new(TestbedConfig::default());
        tb.register_provider(MockProvider::new("github"));
        let action = sample_action("conn-1", ConnectorActionKind::Notify);
        tb.send_request("github", "conn-1", &action, 1000, 50);
        tb.send_request("github", "conn-1", &action, 2000, 50);
        let t = tb.telemetry();
        assert_eq!(t.governor_evaluations, 2);
        assert_eq!(t.provider_requests, 2);
        assert_eq!(t.events_ingested, 2);
    }

    // -------------------------------------------------------------------------
    // Report serde tests
    // -------------------------------------------------------------------------

    #[test]
    fn sandbox_probe_report_serde() {
        let report = SandboxProbeReport {
            total_attempts: 1,
            all_passed: true,
            results: vec![SandboxEscapeResult {
                attempt: SandboxEscapeAttempt::filesystem_read("/etc/passwd"),
                was_blocked: true,
                passed: true,
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: SandboxProbeReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }

    #[test]
    fn ingestion_flood_report_serde() {
        let report = IngestionFloodReport {
            total_events: 100,
            recorded: 90,
            filtered: 5,
            rejected: 5,
            chain_integrity_valid: true,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: IngestionFloodReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }

    #[test]
    fn testbed_telemetry_serde() {
        let t = TestbedTelemetry::default();
        let json = serde_json::to_string(&t).unwrap();
        let back: TestbedTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn testbed_snapshot_serde() {
        let snap = TestbedSnapshot {
            captured_at_ms: 1000,
            counters: TestbedTelemetry::default(),
            provider_count: 2,
            escape_result_count: 5,
            audit_chain_length: 10,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: TestbedSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn mock_provider_outcome_display() {
        assert_eq!(MockProviderOutcome::Success.to_string(), "success");
        assert_eq!(MockProviderOutcome::Offline.to_string(), "offline");
        assert_eq!(
            MockProviderOutcome::SimulatedFailure.to_string(),
            "simulated_failure"
        );
        assert_eq!(
            MockProviderOutcome::RateLimited.to_string(),
            "rate_limited"
        );
    }
}
