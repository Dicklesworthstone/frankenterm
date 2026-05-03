use super::InvariantDomain;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;

/// Fault domain: an isolated region that can fail independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FaultDomain {
    /// Scheduler fault domain (lane management, admission control).
    Scheduler,
    /// Budget fault domain (percentile tracking, SLO enforcement).
    Budget,
    /// Recovery fault domain (mitigation, escalation, cooldown).
    Recovery,
    /// IO fault domain (PTY capture, event emission).
    Io,
    /// Storage fault domain (write pipeline, indexing).
    Storage,
}

impl FaultDomain {
    /// All fault domains.
    pub const ALL: &'static [Self] = &[
        Self::Scheduler,
        Self::Budget,
        Self::Recovery,
        Self::Io,
        Self::Storage,
    ];
}

impl fmt::Display for FaultDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scheduler => write!(f, "scheduler"),
            Self::Budget => write!(f, "budget"),
            Self::Recovery => write!(f, "recovery"),
            Self::Io => write!(f, "io"),
            Self::Storage => write!(f, "storage"),
        }
    }
}

/// Health state of a fault domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomainHealth {
    /// Operating normally.
    Healthy,
    /// Degraded but functional.
    Degraded,
    /// Crashed and awaiting restart.
    Crashed,
    /// Restarting (crash-only recovery in progress).
    Restarting,
    /// Isolated (quarantined to prevent blast-radius expansion).
    Isolated,
}

impl fmt::Display for DomainHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Degraded => write!(f, "degraded"),
            Self::Crashed => write!(f, "crashed"),
            Self::Restarting => write!(f, "restarting"),
            Self::Isolated => write!(f, "isolated"),
        }
    }
}

/// Crash-only service contract: specifies restart behavior for a domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashOnlyContract {
    /// Domain this contract governs.
    pub domain: FaultDomain,
    /// Maximum restart attempts before isolation.
    pub max_restarts: u32,
    /// Cooldown between restarts, in microseconds.
    pub restart_cooldown_us: u64,
    /// Whether to checkpoint state before crash restart.
    pub checkpoint_on_crash: bool,
    /// Timeout for restart completion, in microseconds. 0 means no timeout.
    pub restart_timeout_us: u64,
}

impl Default for CrashOnlyContract {
    fn default() -> Self {
        Self {
            domain: FaultDomain::Scheduler,
            max_restarts: 3,
            restart_cooldown_us: 100_000,
            checkpoint_on_crash: true,
            restart_timeout_us: 5_000_000,
        }
    }
}

/// A fault event recording a domain failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultEvent {
    /// Domain that faulted.
    pub domain: FaultDomain,
    /// Timestamp in epoch microseconds.
    pub timestamp_us: u64,
    /// Description of the fault.
    pub description: String,
    /// Whether recovery was attempted.
    pub recovery_attempted: bool,
    /// Whether recovery succeeded.
    pub recovery_succeeded: bool,
}

/// Snapshot of a fault domain's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultDomainState {
    /// The domain.
    pub domain: FaultDomain,
    /// Current health.
    pub health: DomainHealth,
    /// Total faults observed.
    pub total_faults: u64,
    /// Total restarts performed.
    pub total_restarts: u64,
    /// Consecutive failures (resets on success).
    pub consecutive_failures: u32,
    /// Timestamp of last fault (0 means never).
    pub last_fault_us: u64,
    /// Timestamp of last restart (0 means never).
    pub last_restart_us: u64,
}

/// Configuration for the fault isolation manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultIsolationConfig {
    /// Contracts for each domain.
    pub contracts: Vec<CrashOnlyContract>,
    /// Whether to auto-isolate domains that exceed max_restarts.
    pub auto_isolate: bool,
    /// Maximum fault history entries to retain.
    pub max_history: usize,
}

impl Default for FaultIsolationConfig {
    fn default() -> Self {
        Self {
            contracts: FaultDomain::ALL
                .iter()
                .map(|d| CrashOnlyContract {
                    domain: *d,
                    ..Default::default()
                })
                .collect(),
            auto_isolate: true,
            max_history: 1000,
        }
    }
}

/// Degradation state for the fault isolation manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaultIsolationDegradation {
    /// All domains healthy.
    Healthy,
    /// Some domains degraded.
    PartialDegradation { degraded_count: usize },
    /// Some domains isolated.
    DomainIsolated { isolated_domains: Vec<FaultDomain> },
}

impl fmt::Display for FaultIsolationDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::PartialDegradation { degraded_count } => {
                write!(f, "partial-degradation({degraded_count})")
            }
            Self::DomainIsolated { isolated_domains } => {
                let names: Vec<String> = isolated_domains.iter().map(|d| d.to_string()).collect();
                write!(f, "isolated({})", names.join(","))
            }
        }
    }
}

/// Log entry for fault isolation events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultIsolationLogEntry {
    /// Timestamp.
    pub timestamp_us: u64,
    /// Domain affected.
    pub domain: FaultDomain,
    /// Health transition.
    pub from_health: DomainHealth,
    /// New health state.
    pub to_health: DomainHealth,
    /// Description.
    pub description: String,
}

/// Snapshot of the entire fault isolation manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultIsolationSnapshot {
    /// Per-domain states.
    pub domains: Vec<FaultDomainState>,
    /// Total faults across all domains.
    pub total_faults: u64,
    /// Total restarts across all domains.
    pub total_restarts: u64,
    /// Configuration.
    pub config: FaultIsolationConfig,
}

/// Tracks fault-domain health, enforces crash-only contracts, and prevents
/// blast-radius expansion.
pub struct FaultIsolationManager {
    config: FaultIsolationConfig,
    states: HashMap<FaultDomain, FaultDomainState>,
    history: VecDeque<FaultEvent>,
}

impl FaultIsolationManager {
    /// Create a new manager with the given config.
    pub fn new(config: FaultIsolationConfig) -> Self {
        let mut states = HashMap::new();
        for domain in FaultDomain::ALL {
            states.insert(
                *domain,
                FaultDomainState {
                    domain: *domain,
                    health: DomainHealth::Healthy,
                    total_faults: 0,
                    total_restarts: 0,
                    consecutive_failures: 0,
                    last_fault_us: 0,
                    last_restart_us: 0,
                },
            );
        }
        Self {
            config,
            states,
            history: VecDeque::new(),
        }
    }

    /// Record a fault in a domain.
    pub fn record_fault(&mut self, domain: FaultDomain, description: String, timestamp_us: u64) {
        let contract = self.contract_for(domain);
        let max_restarts = contract.max_restarts;
        let auto_isolate = self.config.auto_isolate;

        let state = self.states.get_mut(&domain).unwrap();
        state.total_faults += 1;
        state.consecutive_failures += 1;
        state.last_fault_us = timestamp_us;

        if auto_isolate && state.consecutive_failures > max_restarts {
            state.health = DomainHealth::Isolated;
        } else {
            state.health = DomainHealth::Crashed;
        }

        let event = FaultEvent {
            domain,
            timestamp_us,
            description,
            recovery_attempted: false,
            recovery_succeeded: false,
        };

        if self.history.len() >= self.config.max_history {
            self.history.pop_front();
        }
        self.history.push_back(event);
    }

    /// Attempt restart of a crashed domain.
    pub fn attempt_restart(&mut self, domain: FaultDomain, timestamp_us: u64) -> bool {
        let contract = self.contract_for(domain);
        let state = self.states.get_mut(&domain).unwrap();
        match state.health {
            DomainHealth::Crashed => {
                if state.last_restart_us > 0
                    && timestamp_us.saturating_sub(state.last_restart_us)
                        < contract.restart_cooldown_us
                {
                    return false;
                }
                state.health = DomainHealth::Restarting;
                state.total_restarts += 1;
                state.last_restart_us = timestamp_us;
                true
            }
            DomainHealth::Isolated => false,
            _ => false,
        }
    }

    /// Mark restart as complete (success).
    pub fn restart_succeeded(&mut self, domain: FaultDomain) {
        let state = self.states.get_mut(&domain).unwrap();
        if state.health == DomainHealth::Restarting {
            state.health = DomainHealth::Healthy;
            state.consecutive_failures = 0;
        }
    }

    /// Mark restart as failed.
    pub fn restart_failed(&mut self, domain: FaultDomain, timestamp_us: u64) {
        let contract = self.contract_for(domain);
        let auto_isolate = self.config.auto_isolate;
        let state = self.states.get_mut(&domain).unwrap();
        if state.health == DomainHealth::Restarting {
            state.consecutive_failures += 1;
            if auto_isolate && state.consecutive_failures > contract.max_restarts {
                state.health = DomainHealth::Isolated;
            } else {
                state.health = DomainHealth::Crashed;
            }
            state.last_fault_us = timestamp_us;
        }
    }

    /// Manually mark a domain as degraded.
    pub fn mark_degraded(&mut self, domain: FaultDomain) {
        let state = self.states.get_mut(&domain).unwrap();
        if state.health == DomainHealth::Healthy {
            state.health = DomainHealth::Degraded;
        }
    }

    /// Manually un-isolate a domain (operator intervention).
    pub fn un_isolate(&mut self, domain: FaultDomain) {
        let state = self.states.get_mut(&domain).unwrap();
        if state.health == DomainHealth::Isolated {
            state.health = DomainHealth::Crashed;
            state.consecutive_failures = 0;
        }
    }

    /// Get the health of a domain.
    pub fn domain_health(&self, domain: FaultDomain) -> DomainHealth {
        self.states
            .get(&domain)
            .map_or(DomainHealth::Healthy, |s| s.health)
    }

    /// Get the state of a domain.
    pub fn domain_state(&self, domain: FaultDomain) -> Option<&FaultDomainState> {
        self.states.get(&domain)
    }

    /// Check if any domains are isolated.
    pub fn has_isolated_domains(&self) -> bool {
        self.states
            .values()
            .any(|s| s.health == DomainHealth::Isolated)
    }

    /// List isolated domains.
    pub fn isolated_domains(&self) -> Vec<FaultDomain> {
        self.states
            .values()
            .filter(|s| s.health == DomainHealth::Isolated)
            .map(|s| s.domain)
            .collect()
    }

    /// Get the crash-only contract for a domain.
    fn contract_for(&self, domain: FaultDomain) -> CrashOnlyContract {
        self.config
            .contracts
            .iter()
            .find(|c| c.domain == domain)
            .cloned()
            .unwrap_or_default()
    }

    /// Get a snapshot.
    pub fn snapshot(&self) -> FaultIsolationSnapshot {
        let domains: Vec<FaultDomainState> = FaultDomain::ALL
            .iter()
            .filter_map(|d| self.states.get(d).cloned())
            .collect();
        let total_faults = domains.iter().map(|d| d.total_faults).sum();
        let total_restarts = domains.iter().map(|d| d.total_restarts).sum();
        FaultIsolationSnapshot {
            domains,
            total_faults,
            total_restarts,
            config: self.config.clone(),
        }
    }

    /// Detect degradation.
    pub fn detect_degradation(&self) -> FaultIsolationDegradation {
        let isolated: Vec<FaultDomain> = self.isolated_domains();
        if !isolated.is_empty() {
            return FaultIsolationDegradation::DomainIsolated {
                isolated_domains: isolated,
            };
        }
        let degraded_count = self
            .states
            .values()
            .filter(|s| {
                s.health == DomainHealth::Degraded
                    || s.health == DomainHealth::Crashed
                    || s.health == DomainHealth::Restarting
            })
            .count();
        if degraded_count > 0 {
            return FaultIsolationDegradation::PartialDegradation { degraded_count };
        }
        FaultIsolationDegradation::Healthy
    }

    /// Create a log entry.
    pub fn log_entry(
        &self,
        domain: FaultDomain,
        from_health: DomainHealth,
        to_health: DomainHealth,
        description: String,
        timestamp_us: u64,
    ) -> FaultIsolationLogEntry {
        FaultIsolationLogEntry {
            timestamp_us,
            domain,
            from_health,
            to_health,
            description,
        }
    }

    /// Fault history.
    pub fn fault_history(&self) -> Vec<FaultEvent> {
        self.history.iter().cloned().collect()
    }

    /// Reset all domains to healthy.
    pub fn reset(&mut self) {
        for state in self.states.values_mut() {
            state.health = DomainHealth::Healthy;
            state.total_faults = 0;
            state.total_restarts = 0;
            state.consecutive_failures = 0;
            state.last_fault_us = 0;
            state.last_restart_us = 0;
        }
        self.history.clear();
    }

    /// Count of healthy domains.
    pub fn healthy_count(&self) -> usize {
        self.states
            .values()
            .filter(|s| s.health == DomainHealth::Healthy)
            .count()
    }

    /// Count of non-healthy domains.
    pub fn unhealthy_count(&self) -> usize {
        self.states
            .values()
            .filter(|s| s.health != DomainHealth::Healthy)
            .count()
    }

    /// Total faults across all domains.
    pub fn total_faults(&self) -> u64 {
        self.states.values().map(|s| s.total_faults).sum()
    }

    /// Total restarts across all domains.
    pub fn total_restarts(&self) -> u64 {
        self.states.values().map(|s| s.total_restarts).sum()
    }

    /// Faults for a specific domain.
    pub fn domain_faults(&self, domain: FaultDomain) -> u64 {
        self.states.get(&domain).map_or(0, |s| s.total_faults)
    }

    /// Restarts for a specific domain.
    pub fn domain_restarts(&self, domain: FaultDomain) -> u64 {
        self.states.get(&domain).map_or(0, |s| s.total_restarts)
    }

    /// Whether all domains are healthy.
    pub fn all_healthy(&self) -> bool {
        self.states
            .values()
            .all(|s| s.health == DomainHealth::Healthy)
    }

    /// Map `FaultDomain` to `InvariantDomain` for cross-system integration.
    pub fn to_invariant_domain(domain: FaultDomain) -> InvariantDomain {
        match domain {
            FaultDomain::Scheduler => InvariantDomain::Scheduler,
            FaultDomain::Budget => InvariantDomain::Budget,
            FaultDomain::Recovery => InvariantDomain::Recovery,
            FaultDomain::Io | FaultDomain::Storage => InvariantDomain::Composition,
        }
    }

    /// Access config.
    pub fn config(&self) -> &FaultIsolationConfig {
        &self.config
    }
}
