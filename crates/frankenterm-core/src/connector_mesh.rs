//! Multi-host connector mesh federation.
//!
//! Manages connector workloads across multiple hosts/zones while preserving
//! policy enforcement, observability, and failure containment. Provides host
//! registration, zone-aware routing, health tracking, and partition tolerance.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::connector_host_runtime::{
    ConnectorCapability, ConnectorFailureClass, ConnectorLifecyclePhase,
};

// =============================================================================
// Error types
// =============================================================================

/// Mesh-specific errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorMeshError {
    #[error("host not found: {host_id}")]
    HostNotFound { host_id: String },

    #[error("zone not found: {zone_id}")]
    ZoneNotFound { zone_id: String },

    #[error("duplicate host: {host_id}")]
    DuplicateHost { host_id: String },

    #[error("duplicate zone: {zone_id}")]
    DuplicateZone { zone_id: String },

    #[error("no healthy hosts available for zone {zone_id}")]
    NoHealthyHosts { zone_id: String },

    #[error("routing failed: {reason}")]
    RoutingFailed { reason: String },

    #[error("host capacity exceeded on {host_id}: {current}/{max} connectors")]
    CapacityExceeded {
        host_id: String,
        current: usize,
        max: usize,
    },

    #[error("partition detected: zone {zone_id} is isolated")]
    PartitionDetected { zone_id: String },

    #[error("invalid config: {reason}")]
    InvalidConfig { reason: String },
}

// =============================================================================
// Host / Zone types
// =============================================================================

/// Health status of a mesh host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HostHealth {
    Healthy,
    Degraded,
    Unreachable,
    Draining,
}

impl HostHealth {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unreachable => "unreachable",
            Self::Draining => "draining",
        }
    }

    /// Whether this host can accept new work.
    pub const fn accepts_work(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }
}

impl fmt::Display for HostHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A registered mesh host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeshHost {
    /// Unique host identifier.
    pub host_id: String,
    /// Zone this host belongs to.
    pub zone_id: String,
    /// Current health status.
    pub health: HostHealth,
    /// Capabilities this host supports.
    pub capabilities: Vec<ConnectorCapability>,
    /// Current number of active connectors.
    pub active_connectors: usize,
    /// Maximum connectors this host can support.
    pub max_connectors: usize,
    /// Last heartbeat timestamp (unix ms).
    pub last_heartbeat_ms: u64,
    /// Connector lifecycle phase (mirrors the host runtime phase).
    pub phase: ConnectorLifecyclePhase,
    /// Arbitrary host metadata.
    pub metadata: BTreeMap<String, String>,
}

impl MeshHost {
    /// Whether this host can accept a new connector.
    pub fn can_accept(&self) -> bool {
        self.health.accepts_work()
            && self.active_connectors < self.max_connectors
            && matches!(self.phase, ConnectorLifecyclePhase::Running)
    }

    /// Remaining capacity.
    pub fn remaining_capacity(&self) -> usize {
        self.max_connectors.saturating_sub(self.active_connectors)
    }

    /// Whether this host supports a given capability.
    pub fn supports(&self, cap: &ConnectorCapability) -> bool {
        self.capabilities.contains(cap)
    }
}

/// A mesh zone (failure domain / availability region).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeshZone {
    /// Unique zone identifier.
    pub zone_id: String,
    /// Human-readable label.
    pub label: String,
    /// Priority weight for routing (higher = preferred).
    pub priority: u32,
    /// Whether this zone is active.
    pub active: bool,
    /// Metadata.
    pub metadata: BTreeMap<String, String>,
}

impl MeshZone {
    pub fn new(zone_id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            zone_id: zone_id.into(),
            label: label.into(),
            priority: 100,
            active: true,
            metadata: BTreeMap::new(),
        }
    }
}

// =============================================================================
// Routing
// =============================================================================

/// Strategy for selecting hosts when routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingStrategy {
    /// Pick the host with the most remaining capacity.
    LeastLoaded,
    /// Pick within the preferred zone, fall back to others.
    ZoneAffinity,
    /// Round-robin across healthy hosts.
    RoundRobin,
}

impl Default for RoutingStrategy {
    fn default() -> Self {
        Self::LeastLoaded
    }
}

impl RoutingStrategy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LeastLoaded => "least_loaded",
            Self::ZoneAffinity => "zone_affinity",
            Self::RoundRobin => "round_robin",
        }
    }
}

impl fmt::Display for RoutingStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A routing request for connector placement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRequest {
    /// Connector ID to place.
    pub connector_id: String,
    /// Required capabilities.
    pub required_capabilities: Vec<ConnectorCapability>,
    /// Preferred zone (optional).
    pub preferred_zone: Option<String>,
    /// Routing strategy override.
    pub strategy: Option<RoutingStrategy>,
}

/// A routing decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutingDecision {
    pub connector_id: String,
    pub host_id: String,
    pub zone_id: String,
    pub strategy_used: RoutingStrategy,
    pub decided_at_ms: u64,
}

// =============================================================================
// Failure events
// =============================================================================

/// A mesh failure event for audit/observability.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MeshFailureEvent {
    pub host_id: String,
    pub zone_id: String,
    pub failure_class: ConnectorFailureClass,
    pub description: String,
    pub timestamp_ms: u64,
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for mesh operations.
#[derive(Debug, Clone, Default)]
pub struct MeshTelemetry {
    pub hosts_registered: u64,
    pub hosts_deregistered: u64,
    pub zones_created: u64,
    pub routing_requests: u64,
    pub routing_successes: u64,
    pub routing_failures: u64,
    pub health_updates: u64,
    pub failure_events: u64,
    pub heartbeats_received: u64,
}

/// Serializable telemetry snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshTelemetrySnapshot {
    pub hosts_registered: u64,
    pub hosts_deregistered: u64,
    pub zones_created: u64,
    pub routing_requests: u64,
    pub routing_successes: u64,
    pub routing_failures: u64,
    pub health_updates: u64,
    pub failure_events: u64,
    pub heartbeats_received: u64,
}

impl MeshTelemetry {
    pub fn snapshot(&self) -> MeshTelemetrySnapshot {
        MeshTelemetrySnapshot {
            hosts_registered: self.hosts_registered,
            hosts_deregistered: self.hosts_deregistered,
            zones_created: self.zones_created,
            routing_requests: self.routing_requests,
            routing_successes: self.routing_successes,
            routing_failures: self.routing_failures,
            health_updates: self.health_updates,
            failure_events: self.failure_events,
            heartbeats_received: self.heartbeats_received,
        }
    }
}

// =============================================================================
// Mesh config
// =============================================================================

/// Configuration for the connector mesh.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectorMeshConfig {
    /// Default routing strategy.
    pub default_strategy: RoutingStrategy,
    /// Heartbeat timeout (ms): host marked unreachable after this.
    pub heartbeat_timeout_ms: u64,
    /// Maximum failure events to retain.
    pub max_failure_history: usize,
    /// Maximum routing decisions to retain.
    pub max_routing_history: usize,
    /// Whether to allow cross-zone fallback.
    pub allow_cross_zone_fallback: bool,
}

impl Default for ConnectorMeshConfig {
    fn default() -> Self {
        Self {
            default_strategy: RoutingStrategy::LeastLoaded,
            heartbeat_timeout_ms: 30_000,
            max_failure_history: 500,
            max_routing_history: 1000,
            allow_cross_zone_fallback: true,
        }
    }
}

// =============================================================================
// Mesh federation engine
// =============================================================================

/// The connector mesh federation engine.
pub struct ConnectorMesh {
    config: ConnectorMeshConfig,
    hosts: HashMap<String, MeshHost>,
    zones: HashMap<String, MeshZone>,
    routing_history: VecDeque<RoutingDecision>,
    failure_history: VecDeque<MeshFailureEvent>,
    telemetry: MeshTelemetry,
    round_robin_counter: usize,
}

impl ConnectorMesh {
    /// Create a new mesh with the given config.
    pub fn new(config: ConnectorMeshConfig) -> Self {
        Self {
            config,
            hosts: HashMap::new(),
            zones: HashMap::new(),
            routing_history: VecDeque::new(),
            failure_history: VecDeque::new(),
            telemetry: MeshTelemetry::default(),
            round_robin_counter: 0,
        }
    }

    // ---- Zone management ----

    /// Register a new zone.
    pub fn register_zone(&mut self, zone: MeshZone) -> Result<(), ConnectorMeshError> {
        if self.zones.contains_key(&zone.zone_id) {
            return Err(ConnectorMeshError::DuplicateZone {
                zone_id: zone.zone_id,
            });
        }
        self.telemetry.zones_created += 1;
        self.zones.insert(zone.zone_id.clone(), zone);
        Ok(())
    }

    /// Get a zone by ID.
    pub fn get_zone(&self, zone_id: &str) -> Option<&MeshZone> {
        self.zones.get(zone_id)
    }

    /// List all zones.
    pub fn zones(&self) -> Vec<&MeshZone> {
        self.zones.values().collect()
    }

    // ---- Host management ----

    /// Register a new host.
    pub fn register_host(&mut self, host: MeshHost) -> Result<(), ConnectorMeshError> {
        if !self.zones.contains_key(&host.zone_id) {
            return Err(ConnectorMeshError::ZoneNotFound {
                zone_id: host.zone_id,
            });
        }
        if self.hosts.contains_key(&host.host_id) {
            return Err(ConnectorMeshError::DuplicateHost {
                host_id: host.host_id,
            });
        }
        self.telemetry.hosts_registered += 1;
        self.hosts.insert(host.host_id.clone(), host);
        Ok(())
    }

    /// Deregister a host.
    pub fn deregister_host(&mut self, host_id: &str) -> Result<MeshHost, ConnectorMeshError> {
        self.telemetry.hosts_deregistered += 1;
        self.hosts
            .remove(host_id)
            .ok_or_else(|| ConnectorMeshError::HostNotFound {
                host_id: host_id.to_string(),
            })
    }

    /// Get a host by ID.
    pub fn get_host(&self, host_id: &str) -> Option<&MeshHost> {
        self.hosts.get(host_id)
    }

    /// List all hosts.
    pub fn hosts(&self) -> Vec<&MeshHost> {
        self.hosts.values().collect()
    }

    /// List hosts in a specific zone.
    pub fn hosts_in_zone(&self, zone_id: &str) -> Vec<&MeshHost> {
        self.hosts
            .values()
            .filter(|h| h.zone_id == zone_id)
            .collect()
    }

    /// Update host health.
    pub fn update_health(
        &mut self,
        host_id: &str,
        health: HostHealth,
    ) -> Result<(), ConnectorMeshError> {
        let host = self
            .hosts
            .get_mut(host_id)
            .ok_or_else(|| ConnectorMeshError::HostNotFound {
                host_id: host_id.to_string(),
            })?;
        host.health = health;
        self.telemetry.health_updates += 1;
        Ok(())
    }

    /// Record a heartbeat from a host.
    pub fn record_heartbeat(
        &mut self,
        host_id: &str,
        now_ms: u64,
    ) -> Result<(), ConnectorMeshError> {
        let host = self
            .hosts
            .get_mut(host_id)
            .ok_or_else(|| ConnectorMeshError::HostNotFound {
                host_id: host_id.to_string(),
            })?;
        host.last_heartbeat_ms = now_ms;
        if host.health == HostHealth::Unreachable {
            host.health = HostHealth::Degraded;
        }
        self.telemetry.heartbeats_received += 1;
        Ok(())
    }

    /// Check for hosts that have missed heartbeats and mark unreachable.
    pub fn check_heartbeat_timeouts(&mut self, now_ms: u64) -> Vec<String> {
        let timeout = self.config.heartbeat_timeout_ms;
        let mut timed_out = Vec::new();
        for host in self.hosts.values_mut() {
            if host.health != HostHealth::Unreachable
                && host.health != HostHealth::Draining
                && now_ms.saturating_sub(host.last_heartbeat_ms) > timeout
            {
                host.health = HostHealth::Unreachable;
                timed_out.push(host.host_id.clone());
            }
        }
        timed_out
    }

    // ---- Routing ----

    /// Route a connector to an appropriate host.
    pub fn route(
        &mut self,
        request: &RoutingRequest,
        now_ms: u64,
    ) -> Result<RoutingDecision, ConnectorMeshError> {
        self.telemetry.routing_requests += 1;
        let strategy = request.strategy.unwrap_or(self.config.default_strategy);

        let candidates = self.find_candidates(request);

        if candidates.is_empty() {
            self.telemetry.routing_failures += 1;
            return Err(ConnectorMeshError::RoutingFailed {
                reason: "no eligible hosts found".to_string(),
            });
        }

        let selected = match strategy {
            RoutingStrategy::LeastLoaded => self.select_least_loaded(&candidates),
            RoutingStrategy::ZoneAffinity => {
                self.select_zone_affinity(&candidates, request.preferred_zone.as_deref())
            }
            RoutingStrategy::RoundRobin => self.select_round_robin(&candidates),
        };

        let Some(host_id) = selected else {
            self.telemetry.routing_failures += 1;
            return Err(ConnectorMeshError::RoutingFailed {
                reason: format!("strategy {} found no suitable host", strategy),
            });
        };

        let host = &self.hosts[&host_id];
        let decision = RoutingDecision {
            connector_id: request.connector_id.clone(),
            host_id: host_id.clone(),
            zone_id: host.zone_id.clone(),
            strategy_used: strategy,
            decided_at_ms: now_ms,
        };

        self.routing_history.push_back(decision.clone());
        while self.routing_history.len() > self.config.max_routing_history {
            self.routing_history.pop_front();
        }

        // Increment host load
        if let Some(h) = self.hosts.get_mut(&host_id) {
            h.active_connectors += 1;
        }

        self.telemetry.routing_successes += 1;
        Ok(decision)
    }

    /// Release a connector slot on a host.
    pub fn release_connector(&mut self, host_id: &str) -> Result<(), ConnectorMeshError> {
        let host = self
            .hosts
            .get_mut(host_id)
            .ok_or_else(|| ConnectorMeshError::HostNotFound {
                host_id: host_id.to_string(),
            })?;
        host.active_connectors = host.active_connectors.saturating_sub(1);
        Ok(())
    }

    /// Record a failure event.
    pub fn record_failure(&mut self, event: MeshFailureEvent) {
        self.telemetry.failure_events += 1;
        self.failure_history.push_back(event);
        while self.failure_history.len() > self.config.max_failure_history {
            self.failure_history.pop_front();
        }
    }

    // ---- Queries ----

    /// Get a mesh health snapshot.
    pub fn health_snapshot(&self) -> MeshHealthSnapshot {
        let total_hosts = self.hosts.len();
        let healthy = self
            .hosts
            .values()
            .filter(|h| h.health == HostHealth::Healthy)
            .count();
        let degraded = self
            .hosts
            .values()
            .filter(|h| h.health == HostHealth::Degraded)
            .count();
        let unreachable = self
            .hosts
            .values()
            .filter(|h| h.health == HostHealth::Unreachable)
            .count();
        let total_capacity: usize = self.hosts.values().map(|h| h.max_connectors).sum();
        let total_active: usize = self.hosts.values().map(|h| h.active_connectors).sum();

        MeshHealthSnapshot {
            total_hosts,
            healthy_hosts: healthy,
            degraded_hosts: degraded,
            unreachable_hosts: unreachable,
            total_zones: self.zones.len(),
            total_capacity,
            total_active,
        }
    }

    /// Telemetry.
    pub fn telemetry(&self) -> &MeshTelemetry {
        &self.telemetry
    }

    /// Routing history.
    pub fn routing_history(&self) -> &VecDeque<RoutingDecision> {
        &self.routing_history
    }

    /// Failure history.
    pub fn failure_history(&self) -> &VecDeque<MeshFailureEvent> {
        &self.failure_history
    }

    /// Config.
    pub fn config(&self) -> &ConnectorMeshConfig {
        &self.config
    }

    // ---- Private routing helpers ----

    fn find_candidates(&self, request: &RoutingRequest) -> Vec<String> {
        let mut candidates: Vec<String> = self
            .hosts
            .values()
            .filter(|h| {
                h.can_accept()
                    && request
                        .required_capabilities
                        .iter()
                        .all(|cap| h.supports(cap))
            })
            .map(|h| h.host_id.clone())
            .collect();
        candidates.sort_unstable();
        candidates
    }

    fn select_least_loaded(&self, candidates: &[String]) -> Option<String> {
        candidates
            .iter()
            .max_by_key(|id| self.hosts[id.as_str()].remaining_capacity())
            .cloned()
    }

    fn select_zone_affinity(
        &self,
        candidates: &[String],
        preferred_zone: Option<&str>,
    ) -> Option<String> {
        if let Some(zone) = preferred_zone {
            // Prefer hosts in the requested zone
            let zone_candidates: Vec<_> = candidates
                .iter()
                .filter(|id| self.hosts[id.as_str()].zone_id == zone)
                .collect();
            if !zone_candidates.is_empty() {
                return zone_candidates
                    .iter()
                    .max_by_key(|id| self.hosts[id.as_str()].remaining_capacity())
                    .map(|id| (*id).clone());
            }
            // Fall back to cross-zone if allowed
            if !self.config.allow_cross_zone_fallback {
                return None;
            }
        }
        self.select_least_loaded(candidates)
    }

    fn select_round_robin(&mut self, candidates: &[String]) -> Option<String> {
        if candidates.is_empty() {
            return None;
        }
        let idx = self.round_robin_counter % candidates.len();
        self.round_robin_counter = self.round_robin_counter.wrapping_add(1);
        Some(candidates[idx].clone())
    }
}

// =============================================================================
// Health snapshot
// =============================================================================

/// A snapshot of mesh health.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshHealthSnapshot {
    pub total_hosts: usize,
    pub healthy_hosts: usize,
    pub degraded_hosts: usize,
    pub unreachable_hosts: usize,
    pub total_zones: usize,
    pub total_capacity: usize,
    pub total_active: usize,
}

impl MeshHealthSnapshot {
    /// Utilization as a fraction (0.0-1.0).
    pub fn utilization(&self) -> f64 {
        if self.total_capacity == 0 {
            return 0.0;
        }
        self.total_active as f64 / self.total_capacity as f64
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Helpers ----

    fn default_mesh() -> ConnectorMesh {
        ConnectorMesh::new(ConnectorMeshConfig::default())
    }

    fn make_zone(id: &str) -> MeshZone {
        MeshZone::new(id, format!("Zone {id}"))
    }

    fn make_host(id: &str, zone: &str) -> MeshHost {
        MeshHost {
            host_id: id.to_string(),
            zone_id: zone.to_string(),
            health: HostHealth::Healthy,
            capabilities: vec![ConnectorCapability::Invoke, ConnectorCapability::ReadState],
            active_connectors: 0,
            max_connectors: 10,
            last_heartbeat_ms: 1000,
            phase: ConnectorLifecyclePhase::Running,
            metadata: BTreeMap::new(),
        }
    }

    fn make_request(id: &str) -> RoutingRequest {
        RoutingRequest {
            connector_id: id.to_string(),
            required_capabilities: vec![ConnectorCapability::Invoke],
            preferred_zone: None,
            strategy: None,
        }
    }

    // ========================================================================
    // Zone management
    // ========================================================================

    #[test]
    fn register_zone() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("us-east")).unwrap();
        assert_eq!(mesh.zones().len(), 1);
    }

    #[test]
    fn register_duplicate_zone() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        let err = mesh.register_zone(make_zone("z1")).unwrap_err();
        assert!(matches!(err, ConnectorMeshError::DuplicateZone { .. }));
    }

    #[test]
    fn get_zone() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        assert!(mesh.get_zone("z1").is_some());
        assert!(mesh.get_zone("z2").is_none());
    }

    // ========================================================================
    // Host management
    // ========================================================================

    #[test]
    fn register_host() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        assert_eq!(mesh.hosts().len(), 1);
    }

    #[test]
    fn register_host_unknown_zone() {
        let mut mesh = default_mesh();
        let err = mesh
            .register_host(make_host("h1", "z-missing"))
            .unwrap_err();
        assert!(matches!(err, ConnectorMeshError::ZoneNotFound { .. }));
    }

    #[test]
    fn register_duplicate_host() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        let err = mesh.register_host(make_host("h1", "z1")).unwrap_err();
        assert!(matches!(err, ConnectorMeshError::DuplicateHost { .. }));
    }

    #[test]
    fn deregister_host() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        let removed = mesh.deregister_host("h1").unwrap();
        assert_eq!(removed.host_id, "h1");
        assert!(mesh.hosts().is_empty());
    }

    #[test]
    fn deregister_host_missing() {
        let mut mesh = default_mesh();
        let err = mesh.deregister_host("nope").unwrap_err();
        assert!(matches!(err, ConnectorMeshError::HostNotFound { .. }));
    }

    #[test]
    fn hosts_in_zone() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_zone(make_zone("z2")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.register_host(make_host("h2", "z1")).unwrap();
        mesh.register_host(make_host("h3", "z2")).unwrap();

        assert_eq!(mesh.hosts_in_zone("z1").len(), 2);
        assert_eq!(mesh.hosts_in_zone("z2").len(), 1);
        assert!(mesh.hosts_in_zone("z3").is_empty());
    }

    // ========================================================================
    // Health + heartbeat
    // ========================================================================

    #[test]
    fn update_health() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.update_health("h1", HostHealth::Degraded).unwrap();
        assert_eq!(mesh.get_host("h1").unwrap().health, HostHealth::Degraded);
    }

    #[test]
    fn update_health_missing() {
        let mut mesh = default_mesh();
        let err = mesh.update_health("nope", HostHealth::Healthy).unwrap_err();
        assert!(matches!(err, ConnectorMeshError::HostNotFound { .. }));
    }

    #[test]
    fn heartbeat_updates_timestamp() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.record_heartbeat("h1", 5000).unwrap();
        assert_eq!(mesh.get_host("h1").unwrap().last_heartbeat_ms, 5000);
    }

    #[test]
    fn heartbeat_recovers_unreachable() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.update_health("h1", HostHealth::Unreachable).unwrap();
        mesh.record_heartbeat("h1", 5000).unwrap();
        assert_eq!(mesh.get_host("h1").unwrap().health, HostHealth::Degraded);
    }

    #[test]
    fn heartbeat_timeout_detection() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        // last heartbeat at 1000, timeout is 30000
        let timed_out = mesh.check_heartbeat_timeouts(50000);
        assert_eq!(timed_out, vec!["h1"]);
        assert_eq!(mesh.get_host("h1").unwrap().health, HostHealth::Unreachable);
    }

    #[test]
    fn heartbeat_timeout_ignores_recent() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.record_heartbeat("h1", 5000).unwrap();
        let timed_out = mesh.check_heartbeat_timeouts(6000);
        assert!(timed_out.is_empty());
    }

    #[test]
    fn heartbeat_timeout_ignores_draining() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.update_health("h1", HostHealth::Draining).unwrap();
        let timed_out = mesh.check_heartbeat_timeouts(999_999);
        assert!(timed_out.is_empty());
    }

    // ========================================================================
    // Routing
    // ========================================================================

    #[test]
    fn route_least_loaded() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        let mut h1 = make_host("h1", "z1");
        h1.active_connectors = 5;
        let h2 = make_host("h2", "z1");
        mesh.register_host(h1).unwrap();
        mesh.register_host(h2).unwrap();

        let decision = mesh.route(&make_request("c1"), 1000).unwrap();
        assert_eq!(decision.host_id, "h2"); // h2 has more capacity
        assert_eq!(decision.strategy_used, RoutingStrategy::LeastLoaded);
    }

    #[test]
    fn route_zone_affinity() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_zone(make_zone("z2")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.register_host(make_host("h2", "z2")).unwrap();

        let mut req = make_request("c1");
        req.preferred_zone = Some("z2".to_string());
        req.strategy = Some(RoutingStrategy::ZoneAffinity);

        let decision = mesh.route(&req, 1000).unwrap();
        assert_eq!(decision.host_id, "h2");
        assert_eq!(decision.zone_id, "z2");
    }

    #[test]
    fn route_zone_affinity_fallback() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_zone(make_zone("z2")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        // z2 has no hosts

        let mut req = make_request("c1");
        req.preferred_zone = Some("z2".to_string());
        req.strategy = Some(RoutingStrategy::ZoneAffinity);

        let decision = mesh.route(&req, 1000).unwrap();
        assert_eq!(decision.host_id, "h1"); // fallback to z1
    }

    #[test]
    fn route_zone_affinity_no_fallback() {
        let mut config = ConnectorMeshConfig::default();
        config.allow_cross_zone_fallback = false;
        let mut mesh = ConnectorMesh::new(config);
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_zone(make_zone("z2")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();

        let mut req = make_request("c1");
        req.preferred_zone = Some("z2".to_string());
        req.strategy = Some(RoutingStrategy::ZoneAffinity);

        let err = mesh.route(&req, 1000).unwrap_err();
        assert!(matches!(err, ConnectorMeshError::RoutingFailed { .. }));
    }

    #[test]
    fn route_round_robin() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.register_host(make_host("h2", "z1")).unwrap();

        let mut req = make_request("c1");
        req.strategy = Some(RoutingStrategy::RoundRobin);

        let d1 = mesh.route(&req, 1000).unwrap();
        req.connector_id = "c2".to_string();
        let d2 = mesh.route(&req, 1001).unwrap();

        // Should pick different hosts
        assert_ne!(d1.host_id, d2.host_id);
    }

    #[test]
    fn route_no_candidates() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        let mut h = make_host("h1", "z1");
        h.health = HostHealth::Unreachable;
        mesh.register_host(h).unwrap();

        let err = mesh.route(&make_request("c1"), 1000).unwrap_err();
        assert!(matches!(err, ConnectorMeshError::RoutingFailed { .. }));
    }

    #[test]
    fn route_capability_filter() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();

        let mut req = make_request("c1");
        req.required_capabilities = vec![ConnectorCapability::ProcessExec]; // h1 doesn't support this
        let err = mesh.route(&req, 1000).unwrap_err();
        assert!(matches!(err, ConnectorMeshError::RoutingFailed { .. }));
    }

    #[test]
    fn route_increments_load() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();

        mesh.route(&make_request("c1"), 1000).unwrap();
        assert_eq!(mesh.get_host("h1").unwrap().active_connectors, 1);

        mesh.route(&make_request("c2"), 1001).unwrap();
        assert_eq!(mesh.get_host("h1").unwrap().active_connectors, 2);
    }

    #[test]
    fn release_connector() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.route(&make_request("c1"), 1000).unwrap();

        mesh.release_connector("h1").unwrap();
        assert_eq!(mesh.get_host("h1").unwrap().active_connectors, 0);
    }

    #[test]
    fn release_connector_missing() {
        let mut mesh = default_mesh();
        let err = mesh.release_connector("nope").unwrap_err();
        assert!(matches!(err, ConnectorMeshError::HostNotFound { .. }));
    }

    // ========================================================================
    // Failure events
    // ========================================================================

    #[test]
    fn record_failure_event() {
        let mut mesh = default_mesh();
        mesh.record_failure(MeshFailureEvent {
            host_id: "h1".to_string(),
            zone_id: "z1".to_string(),
            failure_class: ConnectorFailureClass::Network,
            description: "connection refused".to_string(),
            timestamp_ms: 1000,
        });
        assert_eq!(mesh.failure_history().len(), 1);
        assert_eq!(mesh.telemetry().failure_events, 1);
    }

    #[test]
    fn failure_history_bounded() {
        let mut config = ConnectorMeshConfig::default();
        config.max_failure_history = 3;
        let mut mesh = ConnectorMesh::new(config);
        for i in 0..5 {
            mesh.record_failure(MeshFailureEvent {
                host_id: format!("h{i}"),
                zone_id: "z1".to_string(),
                failure_class: ConnectorFailureClass::Unknown,
                description: format!("fail {i}"),
                timestamp_ms: 1000 + i,
            });
        }
        assert_eq!(mesh.failure_history().len(), 3);
    }

    // ========================================================================
    // Health snapshot
    // ========================================================================

    #[test]
    fn health_snapshot_basic() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.register_host(make_host("h2", "z1")).unwrap();
        mesh.update_health("h2", HostHealth::Degraded).unwrap();

        let snap = mesh.health_snapshot();
        assert_eq!(snap.total_hosts, 2);
        assert_eq!(snap.healthy_hosts, 1);
        assert_eq!(snap.degraded_hosts, 1);
        assert_eq!(snap.total_zones, 1);
        assert_eq!(snap.total_capacity, 20);
        assert_eq!(snap.total_active, 0);
    }

    #[test]
    fn health_snapshot_utilization() {
        let snap = MeshHealthSnapshot {
            total_hosts: 2,
            healthy_hosts: 2,
            degraded_hosts: 0,
            unreachable_hosts: 0,
            total_zones: 1,
            total_capacity: 20,
            total_active: 10,
        };
        assert!((snap.utilization() - 0.5).abs() < 0.001);
    }

    #[test]
    fn health_snapshot_zero_capacity() {
        let snap = MeshHealthSnapshot {
            total_hosts: 0,
            healthy_hosts: 0,
            degraded_hosts: 0,
            unreachable_hosts: 0,
            total_zones: 0,
            total_capacity: 0,
            total_active: 0,
        };
        assert_eq!(snap.utilization(), 0.0);
    }

    // ========================================================================
    // Telemetry
    // ========================================================================

    #[test]
    fn telemetry_initial_zero() {
        let mesh = default_mesh();
        let snap = mesh.telemetry().snapshot();
        assert_eq!(snap.hosts_registered, 0);
        assert_eq!(snap.routing_requests, 0);
    }

    #[test]
    fn telemetry_accumulates() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();
        mesh.route(&make_request("c1"), 1000).unwrap();

        let snap = mesh.telemetry().snapshot();
        assert_eq!(snap.hosts_registered, 1);
        assert_eq!(snap.zones_created, 1);
        assert_eq!(snap.routing_requests, 1);
        assert_eq!(snap.routing_successes, 1);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let snap = MeshTelemetrySnapshot {
            hosts_registered: 5,
            hosts_deregistered: 1,
            zones_created: 2,
            routing_requests: 100,
            routing_successes: 95,
            routing_failures: 5,
            health_updates: 50,
            failure_events: 3,
            heartbeats_received: 200,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let s2: MeshTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, s2);
    }

    // ========================================================================
    // Serde roundtrips
    // ========================================================================

    #[test]
    fn config_serde_roundtrip() {
        let config = ConnectorMeshConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let c2: ConnectorMeshConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, c2);
    }

    #[test]
    fn mesh_host_serde_roundtrip() {
        let host = make_host("h1", "z1");
        let json = serde_json::to_string(&host).unwrap();
        let h2: MeshHost = serde_json::from_str(&json).unwrap();
        assert_eq!(host, h2);
    }

    #[test]
    fn mesh_zone_serde_roundtrip() {
        let zone = make_zone("z1");
        let json = serde_json::to_string(&zone).unwrap();
        let z2: MeshZone = serde_json::from_str(&json).unwrap();
        assert_eq!(zone, z2);
    }

    #[test]
    fn routing_decision_serde_roundtrip() {
        let dec = RoutingDecision {
            connector_id: "c1".to_string(),
            host_id: "h1".to_string(),
            zone_id: "z1".to_string(),
            strategy_used: RoutingStrategy::LeastLoaded,
            decided_at_ms: 1000,
        };
        let json = serde_json::to_string(&dec).unwrap();
        let d2: RoutingDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(dec, d2);
    }

    #[test]
    fn failure_event_serde_roundtrip() {
        let ev = MeshFailureEvent {
            host_id: "h1".to_string(),
            zone_id: "z1".to_string(),
            failure_class: ConnectorFailureClass::Network,
            description: "timeout".to_string(),
            timestamp_ms: 1000,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let e2: MeshFailureEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, e2);
    }

    #[test]
    fn health_snapshot_serde_roundtrip() {
        let snap = MeshHealthSnapshot {
            total_hosts: 3,
            healthy_hosts: 2,
            degraded_hosts: 1,
            unreachable_hosts: 0,
            total_zones: 2,
            total_capacity: 30,
            total_active: 10,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let s2: MeshHealthSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, s2);
    }

    // ========================================================================
    // Display impls
    // ========================================================================

    #[test]
    fn host_health_display() {
        assert_eq!(HostHealth::Healthy.to_string(), "healthy");
        assert_eq!(HostHealth::Degraded.to_string(), "degraded");
        assert_eq!(HostHealth::Unreachable.to_string(), "unreachable");
        assert_eq!(HostHealth::Draining.to_string(), "draining");
    }

    #[test]
    fn routing_strategy_display() {
        assert_eq!(RoutingStrategy::LeastLoaded.to_string(), "least_loaded");
        assert_eq!(RoutingStrategy::ZoneAffinity.to_string(), "zone_affinity");
        assert_eq!(RoutingStrategy::RoundRobin.to_string(), "round_robin");
    }

    #[test]
    fn host_health_accepts_work() {
        assert!(HostHealth::Healthy.accepts_work());
        assert!(HostHealth::Degraded.accepts_work());
        assert!(!HostHealth::Unreachable.accepts_work());
        assert!(!HostHealth::Draining.accepts_work());
    }

    // ========================================================================
    // MeshHost methods
    // ========================================================================

    #[test]
    fn mesh_host_can_accept() {
        let host = make_host("h1", "z1");
        assert!(host.can_accept());

        let mut full = make_host("h2", "z1");
        full.active_connectors = 10;
        assert!(!full.can_accept());

        let mut stopped = make_host("h3", "z1");
        stopped.phase = ConnectorLifecyclePhase::Stopped;
        assert!(!stopped.can_accept());
    }

    #[test]
    fn mesh_host_remaining_capacity() {
        let mut host = make_host("h1", "z1");
        assert_eq!(host.remaining_capacity(), 10);
        host.active_connectors = 7;
        assert_eq!(host.remaining_capacity(), 3);
    }

    #[test]
    fn mesh_host_supports_capability() {
        let host = make_host("h1", "z1");
        assert!(host.supports(&ConnectorCapability::Invoke));
        assert!(!host.supports(&ConnectorCapability::ProcessExec));
    }

    // ========================================================================
    // Routing history
    // ========================================================================

    #[test]
    fn routing_history_bounded() {
        let mut config = ConnectorMeshConfig::default();
        config.max_routing_history = 3;
        let mut mesh = ConnectorMesh::new(config);
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_host(make_host("h1", "z1")).unwrap();

        for i in 0..5 {
            let mut h = make_host(&format!("host-{i}"), "z1");
            if i > 0 {
                // first host already registered
                h.host_id = format!("host-extra-{i}");
                mesh.register_host(h).unwrap();
            }
        }

        for i in 0..5 {
            let _ = mesh.route(&make_request(&format!("c{i}")), 1000 + i as u64);
        }

        assert!(mesh.routing_history().len() <= 3);
    }

    // ========================================================================
    // Stress test
    // ========================================================================

    #[test]
    fn stress_many_hosts_and_routes() {
        let mut mesh = default_mesh();
        mesh.register_zone(make_zone("z1")).unwrap();
        mesh.register_zone(make_zone("z2")).unwrap();

        for i in 0..50 {
            let zone = if i % 2 == 0 { "z1" } else { "z2" };
            mesh.register_host(make_host(&format!("h{i}"), zone))
                .unwrap();
        }

        for i in 0..200 {
            let req = make_request(&format!("c{i}"));
            mesh.route(&req, 1000 + i as u64).unwrap();
        }

        let snap = mesh.health_snapshot();
        assert_eq!(snap.total_hosts, 50);
        assert_eq!(snap.total_active, 200);
        assert_eq!(mesh.telemetry().routing_successes, 200);
    }

    // ========================================================================
    // Error display
    // ========================================================================

    #[test]
    fn error_display_messages() {
        let err = ConnectorMeshError::HostNotFound {
            host_id: "h1".to_string(),
        };
        assert!(err.to_string().contains("h1"));

        let err = ConnectorMeshError::NoHealthyHosts {
            zone_id: "z1".to_string(),
        };
        assert!(err.to_string().contains("z1"));

        let err = ConnectorMeshError::CapacityExceeded {
            host_id: "h1".to_string(),
            current: 10,
            max: 10,
        };
        assert!(err.to_string().contains("10/10"));
    }
}
