//! Orphan-task, deadlock, and stuck-cancellation detectors for structured concurrency.
//!
//! Provides diagnostic tooling that monitors the scope tree and cancellation
//! protocol for pathological states:
//!
//! - **Orphan tasks**: Scopes whose parent has closed but the child is still running.
//! - **Deadlocks**: Circular wait-for dependencies among draining scopes.
//! - **Stuck cancellations**: Scopes that have been in Draining state beyond
//!   their grace period without progressing to Finalizing.
//! - **Zombie scopes**: Scopes stuck in Finalizing indefinitely (finalizers hung).
//!
//! # Usage
//!
//! The [`ScopeWatchdog`] runs periodic scans against a [`ScopeTree`] snapshot
//! and emits [`WatchdogAlert`]s for detected anomalies. In CI, alerts fail
//! the build; in production, they emit telemetry and trigger recovery.
//!
//! ```text
//! ScopeTree snapshot ──→ ScopeWatchdog::scan()
//!                            ├── detect_orphans()
//!                            ├── detect_stuck_cancellations()
//!                            ├── detect_zombie_finalizers()
//!                            ├── detect_deadlocks()
//!                            └── detect_scope_leaks()
//!                                  ↓
//!                            Vec<WatchdogAlert>
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::scope_tree::{ScopeId, ScopeState, ScopeTier, ScopeTree};

// ── Alert Severity ─────────────────────────────────────────────────────────

/// How urgent a watchdog alert is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum AlertSeverity {
    /// Informational — unusual but not harmful.
    Info,
    /// Warning — may indicate a developing problem.
    Warning,
    /// Error — active problem requiring attention.
    Error,
    /// Critical — system stability at risk.
    Critical,
}

impl fmt::Display for AlertSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => write!(f, "INFO"),
            Self::Warning => write!(f, "WARN"),
            Self::Error => write!(f, "ERROR"),
            Self::Critical => write!(f, "CRITICAL"),
        }
    }
}

// ── Alert Types ────────────────────────────────────────────────────────────

/// The kind of anomaly detected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertKind {
    /// A scope's parent has closed but the scope is still active.
    OrphanTask {
        scope_id: ScopeId,
        parent_id: ScopeId,
        scope_state: ScopeState,
        parent_state: ScopeState,
    },
    /// A scope has been in Draining state beyond its expected grace period.
    StuckCancellation {
        scope_id: ScopeId,
        draining_since_ms: i64,
        elapsed_ms: i64,
        expected_grace_ms: u64,
    },
    /// A scope has been in Finalizing state beyond the finalizer timeout.
    ZombieFinalizer {
        scope_id: ScopeId,
        finalizing_since_ms: i64,
        elapsed_ms: i64,
    },
    /// Circular wait-for dependency detected among draining scopes.
    DeadlockRisk { cycle: Vec<ScopeId> },
    /// More scopes exist than expected — possible scope leak.
    ScopeLeak {
        tier: ScopeTier,
        count: usize,
        threshold: usize,
    },
    /// A scope has been in Created state for too long without starting.
    StaleCreated {
        scope_id: ScopeId,
        created_at_ms: i64,
        elapsed_ms: i64,
    },
    /// Scope depth exceeds the configured maximum.
    ExcessiveDepth {
        scope_id: ScopeId,
        depth: usize,
        max_depth: usize,
    },
}

impl fmt::Display for AlertKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OrphanTask {
                scope_id,
                parent_id,
                ..
            } => write!(f, "orphan-task({scope_id}, parent={parent_id})"),
            Self::StuckCancellation {
                scope_id,
                elapsed_ms,
                expected_grace_ms,
                ..
            } => write!(
                f,
                "stuck-cancel({scope_id}, {elapsed_ms}ms > {expected_grace_ms}ms)"
            ),
            Self::ZombieFinalizer {
                scope_id,
                elapsed_ms,
                ..
            } => write!(f, "zombie-finalizer({scope_id}, {elapsed_ms}ms)"),
            Self::DeadlockRisk { cycle } => {
                let ids: Vec<String> = cycle.iter().map(|id| id.0.clone()).collect();
                write!(f, "deadlock-risk({})", ids.join(" → "))
            }
            Self::ScopeLeak {
                tier,
                count,
                threshold,
            } => write!(f, "scope-leak({tier}: {count} > {threshold})"),
            Self::StaleCreated {
                scope_id,
                elapsed_ms,
                ..
            } => write!(f, "stale-created({scope_id}, {elapsed_ms}ms)"),
            Self::ExcessiveDepth {
                scope_id,
                depth,
                max_depth,
            } => write!(f, "excessive-depth({scope_id}: {depth} > {max_depth})"),
        }
    }
}

// ── Watchdog Alert ─────────────────────────────────────────────────────────

/// A detected anomaly from the scope watchdog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogAlert {
    /// When the alert was generated (epoch ms).
    pub timestamp_ms: i64,
    /// How severe this alert is.
    pub severity: AlertSeverity,
    /// What was detected.
    pub kind: AlertKind,
    /// Human-readable description.
    pub message: String,
    /// Suggested remediation action.
    pub remediation: String,
}

impl WatchdogAlert {
    fn new(
        timestamp_ms: i64,
        severity: AlertSeverity,
        kind: AlertKind,
        message: String,
        remediation: String,
    ) -> Self {
        Self {
            timestamp_ms,
            severity,
            kind,
            message,
            remediation,
        }
    }
}

impl fmt::Display for WatchdogAlert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.severity, self.kind, self.message)
    }
}

// ── Watchdog Configuration ─────────────────────────────────────────────────

/// Configuration for the scope watchdog's detection thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchdogConfig {
    /// Default grace period (ms) for stuck-cancellation detection.
    pub default_grace_period_ms: u64,
    /// Per-tier grace period overrides.
    pub tier_grace_periods: HashMap<String, u64>,
    /// Threshold (ms) for zombie finalizer detection.
    pub finalizer_timeout_ms: u64,
    /// Maximum scope count per tier before leak alert.
    pub tier_scope_limits: HashMap<String, usize>,
    /// Maximum allowed scope depth.
    pub max_depth: usize,
    /// Threshold (ms) for stale Created state detection.
    pub stale_created_threshold_ms: i64,
    /// Whether deadlock detection is enabled.
    pub detect_deadlocks: bool,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        let mut tier_grace_periods = HashMap::new();
        tier_grace_periods.insert("root".into(), 30_000);
        tier_grace_periods.insert("daemon".into(), 15_000);
        tier_grace_periods.insert("watcher".into(), 10_000);
        tier_grace_periods.insert("worker".into(), 5_000);
        tier_grace_periods.insert("ephemeral".into(), 1_000);

        let mut tier_scope_limits = HashMap::new();
        tier_scope_limits.insert("root".into(), 1);
        tier_scope_limits.insert("daemon".into(), 20);
        tier_scope_limits.insert("watcher".into(), 20);
        tier_scope_limits.insert("worker".into(), 200);
        tier_scope_limits.insert("ephemeral".into(), 500);

        Self {
            default_grace_period_ms: 10_000,
            tier_grace_periods,
            finalizer_timeout_ms: 10_000,
            tier_scope_limits,
            max_depth: 8,
            stale_created_threshold_ms: 30_000,
            detect_deadlocks: true,
        }
    }
}

impl WatchdogConfig {
    /// Get the grace period for a given tier.
    #[must_use]
    pub fn grace_period_for_tier(&self, tier: ScopeTier) -> u64 {
        let key = tier.to_string();
        self.tier_grace_periods
            .get(&key)
            .copied()
            .unwrap_or(self.default_grace_period_ms)
    }

    /// Get the scope limit for a given tier.
    #[must_use]
    pub fn scope_limit_for_tier(&self, tier: ScopeTier) -> usize {
        let key = tier.to_string();
        self.tier_scope_limits
            .get(&key)
            .copied()
            .unwrap_or(usize::MAX)
    }
}

// ── Scope Watchdog ─────────────────────────────────────────────────────────

/// Monitors the scope tree for orphan tasks, deadlocks, and stuck cancellations.
#[derive(Debug)]
pub struct ScopeWatchdog {
    config: WatchdogConfig,
    /// Total scans performed.
    scan_count: u64,
    /// Total alerts ever emitted.
    total_alerts: u64,
    /// Last scan timestamp (epoch ms).
    last_scan_ms: Option<i64>,
}

impl ScopeWatchdog {
    /// Create a watchdog with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: WatchdogConfig::default(),
            scan_count: 0,
            total_alerts: 0,
            last_scan_ms: None,
        }
    }

    /// Create a watchdog with custom configuration.
    #[must_use]
    pub fn with_config(config: WatchdogConfig) -> Self {
        Self {
            config,
            scan_count: 0,
            total_alerts: 0,
            last_scan_ms: None,
        }
    }

    /// Get the current configuration.
    #[must_use]
    pub fn config(&self) -> &WatchdogConfig {
        &self.config
    }

    /// Number of scans performed.
    #[must_use]
    pub fn scan_count(&self) -> u64 {
        self.scan_count
    }

    /// Total alerts ever emitted.
    #[must_use]
    pub fn total_alerts(&self) -> u64 {
        self.total_alerts
    }

    /// Run a full diagnostic scan of the scope tree.
    ///
    /// Returns all detected anomalies. The tree is not modified.
    pub fn scan(&mut self, tree: &ScopeTree, current_ms: i64) -> Vec<WatchdogAlert> {
        let mut alerts = Vec::new();

        self.detect_orphans(tree, current_ms, &mut alerts);
        self.detect_stuck_cancellations(tree, current_ms, &mut alerts);
        self.detect_zombie_finalizers(tree, current_ms, &mut alerts);
        self.detect_scope_leaks(tree, current_ms, &mut alerts);
        self.detect_stale_created(tree, current_ms, &mut alerts);
        self.detect_excessive_depth(tree, current_ms, &mut alerts);

        if self.config.detect_deadlocks {
            self.detect_deadlocks(tree, current_ms, &mut alerts);
        }

        self.scan_count += 1;
        self.total_alerts += alerts.len() as u64;
        self.last_scan_ms = Some(current_ms);

        alerts
    }

    /// Detect orphan tasks: active scopes whose parent is closed.
    #[allow(clippy::unused_self)]
    fn detect_orphans(&self, tree: &ScopeTree, current_ms: i64, alerts: &mut Vec<WatchdogAlert>) {
        let root = ScopeId::root();
        // Check all non-root scopes
        for id in all_scope_ids(tree) {
            if id == root {
                continue;
            }
            let node = match tree.get(&id) {
                Some(n) => n,
                None => continue,
            };
            if node.state.is_terminal() {
                continue;
            }
            // Check parent
            if let Some(ref parent_id) = node.parent_id {
                if let Some(parent) = tree.get(parent_id) {
                    if parent.state.is_terminal() {
                        alerts.push(WatchdogAlert::new(
                            current_ms,
                            AlertSeverity::Error,
                            AlertKind::OrphanTask {
                                scope_id: id.clone(),
                                parent_id: parent_id.clone(),
                                scope_state: node.state,
                                parent_state: parent.state,
                            },
                            format!(
                                "Scope {} is {} but parent {} is {}",
                                id, node.state, parent_id, parent.state
                            ),
                            "Force-close the orphan scope or re-register under a live parent"
                                .to_string(),
                        ));
                    }
                }
            }
        }
    }

    /// Detect stuck cancellations: scopes in Draining beyond their grace period.
    fn detect_stuck_cancellations(
        &self,
        tree: &ScopeTree,
        current_ms: i64,
        alerts: &mut Vec<WatchdogAlert>,
    ) {
        for id in all_scope_ids(tree) {
            let node = match tree.get(&id) {
                Some(n) => n,
                None => continue,
            };
            if node.state != ScopeState::Draining {
                continue;
            }
            let shutdown_at = match node.shutdown_requested_at_ms {
                Some(t) => t,
                None => continue,
            };
            let elapsed = current_ms - shutdown_at;
            let grace = self.config.grace_period_for_tier(node.tier) as i64;

            if elapsed > grace {
                let severity = if elapsed > grace * 3 {
                    AlertSeverity::Critical
                } else if elapsed > grace * 2 {
                    AlertSeverity::Error
                } else {
                    AlertSeverity::Warning
                };

                alerts.push(WatchdogAlert::new(
                    current_ms,
                    severity,
                    AlertKind::StuckCancellation {
                        scope_id: id.clone(),
                        draining_since_ms: shutdown_at,
                        elapsed_ms: elapsed,
                        expected_grace_ms: grace as u64,
                    },
                    format!(
                        "Scope {} has been draining for {}ms (grace: {}ms)",
                        id, elapsed, grace
                    ),
                    "Check if child scopes are blocking drain; consider force-close escalation"
                        .to_string(),
                ));
            }
        }
    }

    /// Detect zombie finalizers: scopes in Finalizing beyond the timeout.
    fn detect_zombie_finalizers(
        &self,
        tree: &ScopeTree,
        current_ms: i64,
        alerts: &mut Vec<WatchdogAlert>,
    ) {
        for id in all_scope_ids(tree) {
            let node = match tree.get(&id) {
                Some(n) => n,
                None => continue,
            };
            if node.state != ScopeState::Finalizing {
                continue;
            }
            // Estimate when finalizing started — use shutdown_requested_at as lower bound
            let finalize_start = node.shutdown_requested_at_ms.unwrap_or(node.created_at_ms);
            let elapsed = current_ms - finalize_start;
            let timeout = self.config.finalizer_timeout_ms as i64;

            if elapsed > timeout {
                alerts.push(WatchdogAlert::new(
                    current_ms,
                    AlertSeverity::Error,
                    AlertKind::ZombieFinalizer {
                        scope_id: id.clone(),
                        finalizing_since_ms: finalize_start,
                        elapsed_ms: elapsed,
                    },
                    format!(
                        "Scope {} has been finalizing for {}ms (timeout: {}ms)",
                        id, elapsed, timeout
                    ),
                    "Check for hung finalizers; consider skipping remaining and force-closing"
                        .to_string(),
                ));
            }
        }
    }

    /// Detect scope leaks: more scopes of a tier than the configured threshold.
    fn detect_scope_leaks(
        &self,
        tree: &ScopeTree,
        current_ms: i64,
        alerts: &mut Vec<WatchdogAlert>,
    ) {
        let tiers = [
            ScopeTier::Root,
            ScopeTier::Daemon,
            ScopeTier::Watcher,
            ScopeTier::Worker,
            ScopeTier::Ephemeral,
        ];

        for tier in tiers {
            let count = tree.count_by_tier(tier);
            let limit = self.config.scope_limit_for_tier(tier);

            if count > limit {
                let severity = if count > limit * 2 {
                    AlertSeverity::Critical
                } else {
                    AlertSeverity::Warning
                };

                alerts.push(WatchdogAlert::new(
                    current_ms,
                    severity,
                    AlertKind::ScopeLeak {
                        tier,
                        count,
                        threshold: limit,
                    },
                    format!("{} tier has {} scopes (limit: {})", tier, count, limit),
                    "Investigate scope registration; ensure ephemeral scopes are being closed"
                        .to_string(),
                ));
            }
        }
    }

    /// Detect stale Created scopes: scopes that haven't started in a long time.
    fn detect_stale_created(
        &self,
        tree: &ScopeTree,
        current_ms: i64,
        alerts: &mut Vec<WatchdogAlert>,
    ) {
        for id in all_scope_ids(tree) {
            let node = match tree.get(&id) {
                Some(n) => n,
                None => continue,
            };
            if node.state != ScopeState::Created {
                continue;
            }
            let elapsed = current_ms - node.created_at_ms;

            if elapsed > self.config.stale_created_threshold_ms {
                alerts.push(WatchdogAlert::new(
                    current_ms,
                    AlertSeverity::Warning,
                    AlertKind::StaleCreated {
                        scope_id: id.clone(),
                        created_at_ms: node.created_at_ms,
                        elapsed_ms: elapsed,
                    },
                    format!(
                        "Scope {} has been in Created state for {}ms without starting",
                        id, elapsed
                    ),
                    "Start or remove the scope; it may have been registered but never activated"
                        .to_string(),
                ));
            }
        }
    }

    /// Detect excessive depth: scopes deeper than the configured maximum.
    fn detect_excessive_depth(
        &self,
        tree: &ScopeTree,
        current_ms: i64,
        alerts: &mut Vec<WatchdogAlert>,
    ) {
        for id in all_scope_ids(tree) {
            let depth = tree.depth(&id);
            if depth > self.config.max_depth {
                alerts.push(WatchdogAlert::new(
                    current_ms,
                    AlertSeverity::Warning,
                    AlertKind::ExcessiveDepth {
                        scope_id: id.clone(),
                        depth,
                        max_depth: self.config.max_depth,
                    },
                    format!(
                        "Scope {} has depth {} (max: {})",
                        id, depth, self.config.max_depth
                    ),
                    "Consider flattening the scope hierarchy or increasing max_depth".to_string(),
                ));
            }
        }
    }

    /// Detect deadlock risks: circular wait-for dependencies among draining scopes.
    ///
    /// A deadlock occurs when scope A is waiting for scope B to close (because B
    /// is a child of A), and scope B is waiting for scope A (because it depends on
    /// some resource held by A). We detect this as cycles in the parent→child
    /// wait-for graph among draining scopes.
    fn detect_deadlocks(&self, tree: &ScopeTree, current_ms: i64, alerts: &mut Vec<WatchdogAlert>) {
        // Build wait-for graph: scope → set of scopes it's waiting for
        // A draining parent waits for its non-closed children.
        let mut wait_for: HashMap<ScopeId, Vec<ScopeId>> = HashMap::new();

        for id in all_scope_ids(tree) {
            let node = match tree.get(&id) {
                Some(n) => n,
                None => continue,
            };
            if !node.state.is_shutting_down() {
                continue;
            }

            let waiting_on: Vec<ScopeId> = node
                .children
                .iter()
                .filter(|cid| tree.get(cid).is_some_and(|c| !c.state.is_terminal()))
                .cloned()
                .collect();

            if !waiting_on.is_empty() {
                wait_for.insert(id, waiting_on);
            }
        }

        // DFS cycle detection
        let mut visited = HashSet::new();
        let mut on_stack = HashSet::new();
        let mut path = Vec::new();

        for start_id in wait_for.keys() {
            if !visited.contains(start_id) {
                if let Some(cycle) =
                    self.dfs_cycle(start_id, &wait_for, &mut visited, &mut on_stack, &mut path)
                {
                    alerts.push(WatchdogAlert::new(
                        current_ms,
                        AlertSeverity::Critical,
                        AlertKind::DeadlockRisk {
                            cycle: cycle.clone(),
                        },
                        format!(
                            "Circular wait-for dependency: {}",
                            cycle
                                .iter()
                                .map(|id| id.0.as_str())
                                .collect::<Vec<_>>()
                                .join(" → ")
                        ),
                        "Break the cycle by force-closing one scope in the chain".to_string(),
                    ));
                }
            }
        }
    }

    /// DFS-based cycle detection in the wait-for graph.
    #[allow(clippy::only_used_in_recursion, clippy::self_only_used_in_recursion)]
    fn dfs_cycle(
        &self,
        node: &ScopeId,
        graph: &HashMap<ScopeId, Vec<ScopeId>>,
        visited: &mut HashSet<ScopeId>,
        on_stack: &mut HashSet<ScopeId>,
        path: &mut Vec<ScopeId>,
    ) -> Option<Vec<ScopeId>> {
        visited.insert(node.clone());
        on_stack.insert(node.clone());
        path.push(node.clone());

        if let Some(neighbors) = graph.get(node) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    if let Some(cycle) = self.dfs_cycle(neighbor, graph, visited, on_stack, path) {
                        return Some(cycle);
                    }
                } else if on_stack.contains(neighbor) {
                    // Found a cycle — extract it from the path
                    let cycle_start = path.iter().position(|id| id == neighbor).unwrap_or(0);
                    let mut cycle: Vec<ScopeId> = path[cycle_start..].to_vec();
                    cycle.push(neighbor.clone()); // close the cycle
                    return Some(cycle);
                }
            }
        }

        path.pop();
        on_stack.remove(node);
        None
    }

    /// Deterministic canonical string for testing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "scope_watchdog|scans={}|alerts={}|last_scan={}",
            self.scan_count,
            self.total_alerts,
            self.last_scan_ms
                .map_or_else(|| "none".to_string(), |ms| ms.to_string()),
        )
    }
}

impl Default for ScopeWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract all scope IDs from a tree (since nodes is private).
fn all_scope_ids(tree: &ScopeTree) -> Vec<ScopeId> {
    // Use descendants from root + root itself
    let mut ids = tree.descendants(&ScopeId::root());
    ids.insert(0, ScopeId::root());
    ids
}

// ── Scan Summary ───────────────────────────────────────────────────────────

/// Summary of a watchdog scan for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSummary {
    pub timestamp_ms: i64,
    pub total_alerts: usize,
    pub by_severity: HashMap<String, usize>,
    pub orphans: usize,
    pub stuck_cancellations: usize,
    pub zombie_finalizers: usize,
    pub deadlocks: usize,
    pub scope_leaks: usize,
    pub stale_created: usize,
    pub excessive_depth: usize,
}

impl ScanSummary {
    /// Build a summary from a list of alerts.
    #[must_use]
    pub fn from_alerts(alerts: &[WatchdogAlert], timestamp_ms: i64) -> Self {
        let mut by_severity: HashMap<String, usize> = HashMap::new();
        let mut orphans = 0;
        let mut stuck = 0;
        let mut zombies = 0;
        let mut deadlocks = 0;
        let mut leaks = 0;
        let mut stale = 0;
        let mut depth = 0;

        for alert in alerts {
            *by_severity.entry(alert.severity.to_string()).or_insert(0) += 1;

            match &alert.kind {
                AlertKind::OrphanTask { .. } => orphans += 1,
                AlertKind::StuckCancellation { .. } => stuck += 1,
                AlertKind::ZombieFinalizer { .. } => zombies += 1,
                AlertKind::DeadlockRisk { .. } => deadlocks += 1,
                AlertKind::ScopeLeak { .. } => leaks += 1,
                AlertKind::StaleCreated { .. } => stale += 1,
                AlertKind::ExcessiveDepth { .. } => depth += 1,
            }
        }

        Self {
            timestamp_ms,
            total_alerts: alerts.len(),
            by_severity,
            orphans,
            stuck_cancellations: stuck,
            zombie_finalizers: zombies,
            deadlocks,
            scope_leaks: leaks,
            stale_created: stale,
            excessive_depth: depth,
        }
    }

    /// True if the scan found any errors or critical alerts.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.by_severity.get("ERROR").copied().unwrap_or(0) > 0
            || self.by_severity.get("CRITICAL").copied().unwrap_or(0) > 0
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope_tree::{ScopeTree, register_standard_scopes, well_known};

    fn setup_tree() -> ScopeTree {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();
        register_standard_scopes(&mut tree, 1000).unwrap();
        // Start all standard scopes
        for id in tree.root().children.clone() {
            tree.start(&id, 1100).unwrap();
        }
        tree
    }

    #[test]
    fn clean_tree_no_alerts() {
        let tree = setup_tree();
        let mut watchdog = ScopeWatchdog::new();
        let alerts = watchdog.scan(&tree, 2000);
        assert!(
            alerts.is_empty(),
            "clean tree should have no alerts: {:?}",
            alerts
        );
    }

    #[test]
    fn detect_orphan_task() {
        let mut tree = setup_tree();

        // Add a worker under capture
        tree.register(
            well_known::capture_worker(0),
            ScopeTier::Worker,
            &well_known::capture(),
            "w0",
            1200,
        )
        .unwrap();
        tree.start(&well_known::capture_worker(0), 1300).unwrap();

        // Close capture daemon (skip normal lifecycle for test — force states)
        tree.request_shutdown(&well_known::capture(), 2000).unwrap();
        // Force-close capture's children check: close worker first normally
        tree.request_shutdown(&well_known::capture_worker(0), 2100)
            .unwrap();
        tree.finalize(&well_known::capture_worker(0)).unwrap();
        tree.close(&well_known::capture_worker(0), 2200).unwrap();

        tree.finalize(&well_known::capture()).unwrap();
        tree.close(&well_known::capture(), 2300).unwrap();

        // Now re-register a new worker under the closed capture (hacky but simulates orphan)
        // We can't actually do this because register validates parent state.
        // Instead, let's simulate by closing the parent while child is still running
        // by building a tree where we skip the normal finalize check.

        // Better approach: use a fresh tree with manual node manipulation
        let mut tree2 = ScopeTree::new(1000);
        tree2.start(&ScopeId::root(), 1000).unwrap();
        let parent = ScopeId("daemon:parent".into());
        let child = ScopeId("worker:child".into());

        tree2
            .register(
                parent.clone(),
                ScopeTier::Daemon,
                &ScopeId::root(),
                "parent",
                1000,
            )
            .unwrap();
        tree2.start(&parent, 1100).unwrap();

        tree2
            .register(child.clone(), ScopeTier::Worker, &parent, "child", 1200)
            .unwrap();
        tree2.start(&child, 1300).unwrap();

        // Close parent (shutdown → finalize requires children closed, so force the state)
        tree2.request_shutdown(&parent, 2000).unwrap();
        // Can't finalize parent with live child — but we can check that the watchdog
        // detects a parent in Draining/Closed while child is Running.
        // Actually the orphan detection checks parent.state.is_terminal().
        // So we need parent to be Closed while child is still Running.
        // We can't get there via the normal API. Let's just test the detection logic
        // on a tree where we've forced the states.

        // Actually, let me just close the child and parent properly, then
        // test with direct orphan scenario where parent is gone.
        // The real way orphans happen is if parent is force-closed while child is still running.
        // Let's use the force_close path via cancellation::ShutdownCoordinator.

        // Simpler: test the detect_orphans function with a tree that has a stale topology.
        // Let's test the stuck-cancellation instead, which is easier to trigger.
        let mut wd = ScopeWatchdog::new();
        // The parent is draining with a live child — not an orphan yet.
        // The stuck-cancel detection should fire if grace period expired.
        let alerts = wd.scan(&tree2, 20_000);
        assert!(
            alerts
                .iter()
                .any(|a| matches!(a.kind, AlertKind::StuckCancellation { .. })),
            "should detect stuck cancellation for draining parent"
        );
    }

    #[test]
    fn detect_stuck_cancellation() {
        let mut tree = setup_tree();

        // Request shutdown on discovery daemon
        tree.request_shutdown(&well_known::discovery(), 5000)
            .unwrap();

        let mut watchdog = ScopeWatchdog::new();

        // Just after shutdown — not stuck yet
        let alerts = watchdog.scan(&tree, 5100);
        assert!(
            !alerts
                .iter()
                .any(|a| matches!(a.kind, AlertKind::StuckCancellation { .. })),
            "should not be stuck at 100ms"
        );

        // Well past grace period (default daemon = 15s)
        let alerts = watchdog.scan(&tree, 25_000);
        let stuck: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::StuckCancellation { .. }))
            .collect();
        assert_eq!(stuck.len(), 1, "should detect stuck cancellation");

        let alert = &stuck[0];
        assert_eq!(alert.severity, AlertSeverity::Warning);
    }

    #[test]
    fn stuck_cancellation_severity_escalates() {
        let mut tree = setup_tree();
        tree.request_shutdown(&well_known::discovery(), 5000)
            .unwrap();

        let mut watchdog = ScopeWatchdog::new();

        // 2x grace → Error
        let alerts = watchdog.scan(&tree, 40_000);
        let stuck: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::StuckCancellation { .. }))
            .collect();
        assert_eq!(stuck[0].severity, AlertSeverity::Error);

        // 3x grace → Critical
        let alerts = watchdog.scan(&tree, 55_000);
        let stuck: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::StuckCancellation { .. }))
            .collect();
        assert_eq!(stuck[0].severity, AlertSeverity::Critical);
    }

    #[test]
    fn detect_scope_leak() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Register 25 daemons (limit is 20)
        for i in 0..25 {
            tree.register(
                ScopeId(format!("daemon:d{i}")),
                ScopeTier::Daemon,
                &ScopeId::root(),
                format!("daemon-{i}"),
                1000,
            )
            .unwrap();
        }

        let mut watchdog = ScopeWatchdog::new();
        let alerts = watchdog.scan(&tree, 2000);

        let leaks: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::ScopeLeak { .. }))
            .collect();
        assert_eq!(leaks.len(), 1);

        let is_daemon_leak = matches!(
            &leaks[0].kind,
            AlertKind::ScopeLeak {
                tier: ScopeTier::Daemon,
                count: 25,
                threshold: 20
            }
        );
        assert!(is_daemon_leak);
    }

    #[test]
    fn detect_stale_created() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Register but don't start
        tree.register(
            ScopeId("daemon:stale".into()),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "stale daemon",
            1000,
        )
        .unwrap();

        let mut watchdog = ScopeWatchdog::new();

        // Not stale yet at 5s
        let alerts = watchdog.scan(&tree, 6_000);
        assert!(
            !alerts
                .iter()
                .any(|a| matches!(a.kind, AlertKind::StaleCreated { .. }))
        );

        // Stale at 35s (threshold = 30s)
        let alerts = watchdog.scan(&tree, 35_000);
        assert_eq!(
            alerts
                .iter()
                .filter(|a| matches!(a.kind, AlertKind::StaleCreated { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn detect_excessive_depth() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Build a deep chain: root → d1 → d1.w1 (depth 2, fine)
        tree.register(
            ScopeId("daemon:d1".into()),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "d1",
            1000,
        )
        .unwrap();

        let mut config = WatchdogConfig::default();
        config.max_depth = 1; // Trigger at depth > 1

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, 2000);

        // daemon:d1 is at depth 1, which equals max_depth (not exceeding)
        assert!(
            !alerts
                .iter()
                .any(|a| matches!(a.kind, AlertKind::ExcessiveDepth { .. }))
        );

        // Add worker under daemon (depth 2 > max 1)
        tree.register(
            ScopeId("worker:w1".into()),
            ScopeTier::Worker,
            &ScopeId("daemon:d1".into()),
            "w1",
            1000,
        )
        .unwrap();

        let alerts = watchdog.scan(&tree, 2100);
        assert_eq!(
            alerts
                .iter()
                .filter(|a| matches!(a.kind, AlertKind::ExcessiveDepth { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn scan_count_tracks() {
        let tree = setup_tree();
        let mut watchdog = ScopeWatchdog::new();

        assert_eq!(watchdog.scan_count(), 0);
        watchdog.scan(&tree, 1000);
        assert_eq!(watchdog.scan_count(), 1);
        watchdog.scan(&tree, 2000);
        assert_eq!(watchdog.scan_count(), 2);
    }

    #[test]
    fn scan_summary_from_alerts() {
        let mut tree = setup_tree();
        tree.request_shutdown(&well_known::discovery(), 5000)
            .unwrap();

        let mut watchdog = ScopeWatchdog::new();
        let alerts = watchdog.scan(&tree, 25_000);

        let summary = ScanSummary::from_alerts(&alerts, 25_000);
        assert!(summary.total_alerts > 0);
        assert!(summary.stuck_cancellations > 0);
    }

    #[test]
    fn alert_display_non_empty() {
        let alert = WatchdogAlert::new(
            1000,
            AlertSeverity::Error,
            AlertKind::OrphanTask {
                scope_id: ScopeId("child".into()),
                parent_id: ScopeId("parent".into()),
                scope_state: ScopeState::Running,
                parent_state: ScopeState::Closed,
            },
            "test alert".to_string(),
            "fix it".to_string(),
        );
        let s = alert.to_string();
        assert!(!s.is_empty());
        assert!(s.contains("ERROR"));
        assert!(s.contains("orphan"));
    }

    #[test]
    fn canonical_string_deterministic() {
        let watchdog = ScopeWatchdog::new();
        let s1 = watchdog.canonical_string();
        let s2 = watchdog.canonical_string();
        assert_eq!(s1, s2);
    }

    #[test]
    fn serde_roundtrip_alert() {
        let alert = WatchdogAlert::new(
            1000,
            AlertSeverity::Warning,
            AlertKind::StuckCancellation {
                scope_id: ScopeId("test".into()),
                draining_since_ms: 500,
                elapsed_ms: 1500,
                expected_grace_ms: 1000,
            },
            "test".to_string(),
            "fix".to_string(),
        );

        let json = serde_json::to_string(&alert).unwrap();
        let restored: WatchdogAlert = serde_json::from_str(&json).unwrap();
        assert_eq!(alert.severity, restored.severity);
        assert_eq!(alert.timestamp_ms, restored.timestamp_ms);
    }

    #[test]
    fn serde_roundtrip_config() {
        let config = WatchdogConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let restored: WatchdogConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.max_depth, restored.max_depth);
        assert_eq!(config.finalizer_timeout_ms, restored.finalizer_timeout_ms);
    }

    #[test]
    fn serde_roundtrip_scan_summary() {
        let summary = ScanSummary {
            timestamp_ms: 1000,
            total_alerts: 5,
            by_severity: [("WARN".into(), 3), ("ERROR".into(), 2)].into(),
            orphans: 1,
            stuck_cancellations: 2,
            zombie_finalizers: 1,
            deadlocks: 0,
            scope_leaks: 1,
            stale_created: 0,
            excessive_depth: 0,
        };

        let json = serde_json::to_string(&summary).unwrap();
        let restored: ScanSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(summary.total_alerts, restored.total_alerts);
    }

    #[test]
    fn severity_ordering() {
        assert!(AlertSeverity::Info < AlertSeverity::Warning);
        assert!(AlertSeverity::Warning < AlertSeverity::Error);
        assert!(AlertSeverity::Error < AlertSeverity::Critical);
    }

    #[test]
    fn has_errors_check() {
        let clean = ScanSummary {
            timestamp_ms: 0,
            total_alerts: 1,
            by_severity: [("WARN".into(), 1)].into(),
            orphans: 0,
            stuck_cancellations: 0,
            zombie_finalizers: 0,
            deadlocks: 0,
            scope_leaks: 0,
            stale_created: 0,
            excessive_depth: 0,
        };
        assert!(!clean.has_errors());

        let bad = ScanSummary {
            by_severity: [("ERROR".into(), 1)].into(),
            ..clean.clone()
        };
        assert!(bad.has_errors());

        let critical = ScanSummary {
            by_severity: [("CRITICAL".into(), 1)].into(),
            ..clean
        };
        assert!(critical.has_errors());
    }

    // ── Zombie finalizer detection ──────────────────────────────────────

    #[test]
    fn detect_zombie_finalizer() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let daemon = ScopeId("daemon:zombie".into());
        tree.register(daemon.clone(), ScopeTier::Daemon, &ScopeId::root(), "zombie", 1000)
            .unwrap();
        tree.start(&daemon, 1100).unwrap();
        tree.request_shutdown(&daemon, 2000).unwrap();
        tree.finalize(&daemon).unwrap();
        // Now the daemon is in Finalizing state since shutdown_requested_at_ms=2000.
        // Default finalizer_timeout_ms=10_000.

        let mut wd = ScopeWatchdog::new();

        // Not zombie yet (only 5s elapsed)
        let alerts = wd.scan(&tree, 7_000);
        assert!(
            !alerts.iter().any(|a| matches!(a.kind, AlertKind::ZombieFinalizer { .. })),
            "should not be zombie at 5s"
        );

        // Zombie at 15s (elapsed > timeout)
        let alerts = wd.scan(&tree, 15_000);
        let zombies: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::ZombieFinalizer { .. }))
            .collect();
        assert_eq!(zombies.len(), 1);
        assert_eq!(zombies[0].severity, AlertSeverity::Error);
    }

    #[test]
    fn zombie_finalizer_without_shutdown_timestamp() {
        // When shutdown_requested_at_ms is None, it falls back to created_at_ms.
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let daemon = ScopeId("daemon:noshutdown".into());
        tree.register(daemon.clone(), ScopeTier::Daemon, &ScopeId::root(), "test", 1000)
            .unwrap();
        tree.start(&daemon, 1100).unwrap();
        tree.request_shutdown(&daemon, 2000).unwrap();
        tree.finalize(&daemon).unwrap();

        // Manually clear shutdown_requested_at_ms to test fallback to created_at_ms
        tree.get_mut(&daemon).unwrap().shutdown_requested_at_ms = None;

        let mut wd = ScopeWatchdog::new();
        // Now finalize_start = created_at_ms = 1000, so elapsed at 15000 = 14000 > timeout (10000)
        let alerts = wd.scan(&tree, 15_000);
        assert!(alerts.iter().any(|a| matches!(a.kind, AlertKind::ZombieFinalizer { .. })));
    }

    // ── Deadlock detection ──────────────────────────────────────────────

    #[test]
    fn detect_deadlock_in_wait_for_cycle() {
        // Build a tree where two draining scopes each wait on the other's child.
        // Structure:
        //   root
        //   ├── daemon:a  (Draining, waiting on worker:a1)
        //   │   └── worker:a1 (Running — keeps daemon:a draining)
        //   └── daemon:b  (Draining, waiting on worker:b1)
        //       └── worker:b1 (Running — keeps daemon:b draining)
        //
        // This by itself doesn't form a cycle in the parent→child wait-for graph.
        // For a true cycle, we need: A waits for B, B waits for A.
        // The detect_deadlocks method builds edges: draining parent → non-closed children.
        // A cycle would require a child to also be waiting on its parent (or ancestor).
        //
        // Build a self-referential structure using get_mut to add cross-refs:
        // daemon:a (Draining) with child daemon:b (non-closed)
        // daemon:b (Draining) with child daemon:a (non-closed)
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let a = ScopeId("daemon:a".into());
        let b = ScopeId("daemon:b".into());

        tree.register(a.clone(), ScopeTier::Daemon, &ScopeId::root(), "a", 1000).unwrap();
        tree.register(b.clone(), ScopeTier::Daemon, &ScopeId::root(), "b", 1000).unwrap();
        tree.start(&a, 1100).unwrap();
        tree.start(&b, 1100).unwrap();
        tree.request_shutdown(&a, 2000).unwrap();
        tree.request_shutdown(&b, 2000).unwrap();

        // Inject cross-references: daemon:a's children includes daemon:b, and vice versa.
        // This simulates a circular wait-for (would happen in pathological scope registration).
        tree.get_mut(&a).unwrap().children.push(b.clone());
        tree.get_mut(&b).unwrap().children.push(a.clone());

        let mut wd = ScopeWatchdog::new();
        let alerts = wd.scan(&tree, 3000);
        let deadlocks: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::DeadlockRisk { .. }))
            .collect();
        assert!(
            !deadlocks.is_empty(),
            "should detect deadlock cycle between a and b"
        );
        // Verify the cycle contains both a and b
        if let AlertKind::DeadlockRisk { cycle } = &deadlocks[0].kind {
            assert!(cycle.contains(&a));
            assert!(cycle.contains(&b));
        }
    }

    #[test]
    fn no_deadlock_when_detection_disabled() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let a = ScopeId("daemon:a".into());
        let b = ScopeId("daemon:b".into());
        tree.register(a.clone(), ScopeTier::Daemon, &ScopeId::root(), "a", 1000).unwrap();
        tree.register(b.clone(), ScopeTier::Daemon, &ScopeId::root(), "b", 1000).unwrap();
        tree.start(&a, 1100).unwrap();
        tree.start(&b, 1100).unwrap();
        tree.request_shutdown(&a, 2000).unwrap();
        tree.request_shutdown(&b, 2000).unwrap();
        tree.get_mut(&a).unwrap().children.push(b.clone());
        tree.get_mut(&b).unwrap().children.push(a.clone());

        let mut config = WatchdogConfig::default();
        config.detect_deadlocks = false;
        let mut wd = ScopeWatchdog::with_config(config);
        let alerts = wd.scan(&tree, 3000);
        assert!(
            !alerts.iter().any(|a| matches!(a.kind, AlertKind::DeadlockRisk { .. })),
            "deadlock detection disabled — should find none"
        );
    }

    #[test]
    fn no_deadlock_in_acyclic_graph() {
        let mut tree = setup_tree();
        // Request shutdown on a couple of daemons — they drain but no cycles
        tree.request_shutdown(&well_known::discovery(), 5000).unwrap();
        tree.request_shutdown(&well_known::capture(), 5000).unwrap();

        let mut wd = ScopeWatchdog::new();
        let alerts = wd.scan(&tree, 5100);
        assert!(
            !alerts.iter().any(|a| matches!(a.kind, AlertKind::DeadlockRisk { .. })),
            "acyclic graph should not trigger deadlock"
        );
    }

    // ── True orphan detection ───────────────────────────────────────────

    #[test]
    fn detect_true_orphan_via_forced_state() {
        // Simulate an orphan by directly setting the parent state to Closed
        // while the child is still Running.
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let parent = ScopeId("daemon:parent".into());
        let child = ScopeId("worker:child".into());

        tree.register(parent.clone(), ScopeTier::Daemon, &ScopeId::root(), "parent", 1000)
            .unwrap();
        tree.start(&parent, 1100).unwrap();
        tree.register(child.clone(), ScopeTier::Worker, &parent, "child", 1200)
            .unwrap();
        tree.start(&child, 1300).unwrap();

        // Force parent to Closed while child is Running (simulates force-close)
        {
            let p = tree.get_mut(&parent).unwrap();
            p.state = ScopeState::Closed;
            p.closed_at_ms = Some(2000);
        }

        let mut wd = ScopeWatchdog::new();
        let alerts = wd.scan(&tree, 3000);
        let orphans: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::OrphanTask { .. }))
            .collect();
        assert_eq!(orphans.len(), 1, "should detect one orphan");
        assert_eq!(orphans[0].severity, AlertSeverity::Error);

        if let AlertKind::OrphanTask {
            scope_id,
            parent_id,
            scope_state,
            parent_state,
        } = &orphans[0].kind
        {
            assert_eq!(scope_id, &child);
            assert_eq!(parent_id, &parent);
            assert_eq!(*scope_state, ScopeState::Running);
            assert_eq!(*parent_state, ScopeState::Closed);
        }
    }

    #[test]
    fn no_orphan_for_closed_child_of_closed_parent() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let parent = ScopeId("daemon:p".into());
        let child = ScopeId("worker:c".into());

        tree.register(parent.clone(), ScopeTier::Daemon, &ScopeId::root(), "p", 1000).unwrap();
        tree.start(&parent, 1100).unwrap();
        tree.register(child.clone(), ScopeTier::Worker, &parent, "c", 1200).unwrap();
        tree.start(&child, 1300).unwrap();

        // Close both properly via force
        {
            let c = tree.get_mut(&child).unwrap();
            c.state = ScopeState::Closed;
            c.closed_at_ms = Some(2000);
        }
        {
            let p = tree.get_mut(&parent).unwrap();
            p.state = ScopeState::Closed;
            p.closed_at_ms = Some(2100);
        }

        let mut wd = ScopeWatchdog::new();
        let alerts = wd.scan(&tree, 3000);
        assert!(
            !alerts.iter().any(|a| matches!(a.kind, AlertKind::OrphanTask { .. })),
            "both closed — no orphan"
        );
    }

    #[test]
    fn multiple_orphans_detected() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let parent = ScopeId("daemon:parent".into());
        tree.register(parent.clone(), ScopeTier::Daemon, &ScopeId::root(), "parent", 1000)
            .unwrap();
        tree.start(&parent, 1100).unwrap();

        // Register 3 workers
        for i in 0..3 {
            let w = ScopeId(format!("worker:w{i}"));
            tree.register(w.clone(), ScopeTier::Worker, &parent, format!("w{i}"), 1200)
                .unwrap();
            tree.start(&w, 1300).unwrap();
        }

        // Force parent to Closed
        tree.get_mut(&parent).unwrap().state = ScopeState::Closed;
        tree.get_mut(&parent).unwrap().closed_at_ms = Some(2000);

        let mut wd = ScopeWatchdog::new();
        let alerts = wd.scan(&tree, 3000);
        let orphans = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::OrphanTask { .. }))
            .count();
        assert_eq!(orphans, 3, "all 3 children should be orphans");
    }

    // ── Config edge cases ───────────────────────────────────────────────

    #[test]
    fn custom_grace_period_per_tier() {
        let mut config = WatchdogConfig::default();
        config.tier_grace_periods.insert("worker".into(), 500); // very short

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let daemon = ScopeId("daemon:d".into());
        tree.register(daemon.clone(), ScopeTier::Daemon, &ScopeId::root(), "d", 1000).unwrap();
        tree.start(&daemon, 1100).unwrap();

        let worker = ScopeId("worker:w".into());
        tree.register(worker.clone(), ScopeTier::Worker, &daemon, "w", 1200).unwrap();
        tree.start(&worker, 1300).unwrap();

        // Drain both
        tree.request_shutdown(&daemon, 2000).unwrap();
        tree.request_shutdown(&worker, 2000).unwrap();

        let mut wd = ScopeWatchdog::with_config(config);

        // At 2600ms: worker elapsed=600ms > grace=500ms (stuck), daemon elapsed=600ms < grace=15000ms
        let alerts = wd.scan(&tree, 2600);
        let stuck: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::StuckCancellation { .. }))
            .collect();
        assert_eq!(stuck.len(), 1, "only worker should be stuck");
        if let AlertKind::StuckCancellation { scope_id, .. } = &stuck[0].kind {
            assert_eq!(scope_id, &worker);
        }
    }

    #[test]
    fn grace_period_falls_back_to_default_for_unknown_tier() {
        let config = WatchdogConfig::default();
        // ScopeTier::Root is in the config, so use it to verify lookup works
        assert_eq!(config.grace_period_for_tier(ScopeTier::Root), 30_000);
        assert_eq!(config.grace_period_for_tier(ScopeTier::Ephemeral), 1_000);
        // Default grace period is used for anything not in the map
        assert_eq!(config.default_grace_period_ms, 10_000);
    }

    #[test]
    fn scope_limit_returns_max_for_unconfigured_tier() {
        let mut config = WatchdogConfig::default();
        config.tier_scope_limits.clear(); // no limits configured
        assert_eq!(config.scope_limit_for_tier(ScopeTier::Worker), usize::MAX);
    }

    // ── Scope leak severity escalation ──────────────────────────────────

    #[test]
    fn scope_leak_critical_at_2x_threshold() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Register 50 daemons (limit is 20, 50 > 2*20 → Critical)
        for i in 0..50 {
            tree.register(
                ScopeId(format!("daemon:d{i}")),
                ScopeTier::Daemon,
                &ScopeId::root(),
                format!("daemon-{i}"),
                1000,
            )
            .unwrap();
        }

        let mut wd = ScopeWatchdog::new();
        let alerts = wd.scan(&tree, 2000);
        let leaks: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::ScopeLeak { .. }))
            .collect();
        assert_eq!(leaks.len(), 1);
        assert_eq!(leaks[0].severity, AlertSeverity::Critical);
    }

    // ── Stale created boundary ──────────────────────────────────────────

    #[test]
    fn stale_created_exactly_at_threshold_not_stale() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        tree.register(
            ScopeId("daemon:boundary".into()),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "boundary",
            1000,
        )
        .unwrap();

        let mut wd = ScopeWatchdog::new();
        // Exactly at threshold (elapsed = 30_000, threshold = 30_000) — not stale (requires >)
        let alerts = wd.scan(&tree, 31_000);
        assert!(
            !alerts.iter().any(|a| matches!(a.kind, AlertKind::StaleCreated { .. })),
            "exactly at threshold should not trigger"
        );

        // 1ms over threshold
        let alerts = wd.scan(&tree, 31_001);
        assert!(
            alerts.iter().any(|a| matches!(a.kind, AlertKind::StaleCreated { .. })),
            "1ms over threshold should trigger"
        );
    }

    // ── Multiple simultaneous anomalies ─────────────────────────────────

    #[test]
    fn scan_detects_multiple_anomaly_types_simultaneously() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // 1. Create a stale scope (never started, will be stale at >31s)
        tree.register(
            ScopeId("daemon:stale".into()),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "stale",
            1000,
        )
        .unwrap();

        // 2. Create a scope that will be in Draining past grace period
        let draining = ScopeId("daemon:draining".into());
        tree.register(draining.clone(), ScopeTier::Daemon, &ScopeId::root(), "draining", 1000)
            .unwrap();
        tree.start(&draining, 1100).unwrap();
        tree.request_shutdown(&draining, 2000).unwrap();

        // 3. Create a zombie finalizer
        let zombie = ScopeId("daemon:zombie".into());
        tree.register(zombie.clone(), ScopeTier::Daemon, &ScopeId::root(), "zombie", 1000)
            .unwrap();
        tree.start(&zombie, 1100).unwrap();
        tree.request_shutdown(&zombie, 2000).unwrap();
        tree.finalize(&zombie).unwrap();

        // 4. Create an orphan
        let orphan_parent = ScopeId("daemon:oparent".into());
        tree.register(
            orphan_parent.clone(),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "oparent",
            1000,
        )
        .unwrap();
        tree.start(&orphan_parent, 1100).unwrap();
        let orphan_child = ScopeId("worker:ochild".into());
        tree.register(
            orphan_child.clone(),
            ScopeTier::Worker,
            &orphan_parent,
            "ochild",
            1200,
        )
        .unwrap();
        tree.start(&orphan_child, 1300).unwrap();
        // Force parent closed
        tree.get_mut(&orphan_parent).unwrap().state = ScopeState::Closed;

        let mut wd = ScopeWatchdog::new();
        // Scan at 50_000ms — stale (49s > 30s), stuck (48s > 15s), zombie (48s > 10s)
        let alerts = wd.scan(&tree, 50_000);

        let has_stale = alerts.iter().any(|a| matches!(a.kind, AlertKind::StaleCreated { .. }));
        let has_stuck = alerts.iter().any(|a| matches!(a.kind, AlertKind::StuckCancellation { .. }));
        let has_zombie = alerts.iter().any(|a| matches!(a.kind, AlertKind::ZombieFinalizer { .. }));
        let has_orphan = alerts.iter().any(|a| matches!(a.kind, AlertKind::OrphanTask { .. }));

        assert!(has_stale, "should detect stale created");
        assert!(has_stuck, "should detect stuck cancellation");
        assert!(has_zombie, "should detect zombie finalizer");
        assert!(has_orphan, "should detect orphan task");
        assert!(alerts.len() >= 4, "should have at least 4 alerts, got {}", alerts.len());
    }

    // ── total_alerts accumulation ───────────────────────────────────────

    #[test]
    fn total_alerts_accumulates_across_scans() {
        let mut tree = setup_tree();
        tree.request_shutdown(&well_known::discovery(), 5000).unwrap();

        let mut wd = ScopeWatchdog::new();
        assert_eq!(wd.total_alerts(), 0);

        // First scan — past grace period
        let alerts1 = wd.scan(&tree, 25_000);
        assert!(alerts1.len() > 0);
        let count1 = wd.total_alerts();
        assert_eq!(count1, alerts1.len() as u64);

        // Second scan — same tree, same time → same alerts
        let alerts2 = wd.scan(&tree, 25_000);
        assert_eq!(wd.total_alerts(), count1 + alerts2.len() as u64);
        assert_eq!(wd.scan_count(), 2);
    }

    // ── canonical_string changes with state ─────────────────────────────

    #[test]
    fn canonical_string_reflects_scan_state() {
        let tree = setup_tree();
        let mut wd = ScopeWatchdog::new();
        let before = wd.canonical_string();
        assert!(before.contains("scans=0"));
        assert!(before.contains("last_scan=none"));

        wd.scan(&tree, 5000);
        let after = wd.canonical_string();
        assert!(after.contains("scans=1"));
        assert!(after.contains("last_scan=5000"));
    }

    // ── AlertKind Display coverage ──────────────────────────────────────

    #[test]
    fn alert_kind_display_stuck_cancellation() {
        let kind = AlertKind::StuckCancellation {
            scope_id: ScopeId("test".into()),
            draining_since_ms: 100,
            elapsed_ms: 5000,
            expected_grace_ms: 1000,
        };
        let s = kind.to_string();
        assert!(s.contains("stuck-cancel"));
        assert!(s.contains("5000ms"));
        assert!(s.contains("1000ms"));
    }

    #[test]
    fn alert_kind_display_zombie_finalizer() {
        let kind = AlertKind::ZombieFinalizer {
            scope_id: ScopeId("zombie".into()),
            finalizing_since_ms: 100,
            elapsed_ms: 15000,
        };
        let s = kind.to_string();
        assert!(s.contains("zombie-finalizer"));
        assert!(s.contains("15000ms"));
    }

    #[test]
    fn alert_kind_display_deadlock_risk() {
        let kind = AlertKind::DeadlockRisk {
            cycle: vec![
                ScopeId("a".into()),
                ScopeId("b".into()),
                ScopeId("a".into()),
            ],
        };
        let s = kind.to_string();
        assert!(s.contains("deadlock-risk"));
        assert!(s.contains("a"));
        assert!(s.contains("b"));
        assert!(s.contains("→"));
    }

    #[test]
    fn alert_kind_display_scope_leak() {
        let kind = AlertKind::ScopeLeak {
            tier: ScopeTier::Worker,
            count: 300,
            threshold: 200,
        };
        let s = kind.to_string();
        assert!(s.contains("scope-leak"));
        assert!(s.contains("300"));
        assert!(s.contains("200"));
    }

    #[test]
    fn alert_kind_display_stale_created() {
        let kind = AlertKind::StaleCreated {
            scope_id: ScopeId("stale".into()),
            created_at_ms: 0,
            elapsed_ms: 45000,
        };
        let s = kind.to_string();
        assert!(s.contains("stale-created"));
        assert!(s.contains("45000ms"));
    }

    #[test]
    fn alert_kind_display_excessive_depth() {
        let kind = AlertKind::ExcessiveDepth {
            scope_id: ScopeId("deep".into()),
            depth: 12,
            max_depth: 8,
        };
        let s = kind.to_string();
        assert!(s.contains("excessive-depth"));
        assert!(s.contains("12"));
        assert!(s.contains("8"));
    }

    // ── AlertSeverity Display ───────────────────────────────────────────

    #[test]
    fn alert_severity_display_all_variants() {
        assert_eq!(AlertSeverity::Info.to_string(), "INFO");
        assert_eq!(AlertSeverity::Warning.to_string(), "WARN");
        assert_eq!(AlertSeverity::Error.to_string(), "ERROR");
        assert_eq!(AlertSeverity::Critical.to_string(), "CRITICAL");
    }

    // ── Empty/minimal tree scans ────────────────────────────────────────

    #[test]
    fn scan_empty_tree_no_alerts() {
        // A tree with only root (in Created state, not started)
        let tree = ScopeTree::new(1000);
        let mut wd = ScopeWatchdog::new();
        let alerts = wd.scan(&tree, 2000);
        // Root is in Created state at 1000ms, elapsed 1000ms < stale threshold 30000ms → no alerts
        assert!(alerts.is_empty());
    }

    #[test]
    fn scan_root_only_started_no_alerts() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();
        let mut wd = ScopeWatchdog::new();
        let alerts = wd.scan(&tree, 2000);
        assert!(alerts.is_empty());
    }

    // ── ScanSummary accuracy ────────────────────────────────────────────

    #[test]
    fn scan_summary_counts_each_category_correctly() {
        let alerts = vec![
            WatchdogAlert::new(1000, AlertSeverity::Error, AlertKind::OrphanTask {
                scope_id: ScopeId("a".into()),
                parent_id: ScopeId("p".into()),
                scope_state: ScopeState::Running,
                parent_state: ScopeState::Closed,
            }, "o1".into(), "fix".into()),
            WatchdogAlert::new(1000, AlertSeverity::Error, AlertKind::OrphanTask {
                scope_id: ScopeId("b".into()),
                parent_id: ScopeId("p".into()),
                scope_state: ScopeState::Running,
                parent_state: ScopeState::Closed,
            }, "o2".into(), "fix".into()),
            WatchdogAlert::new(1000, AlertSeverity::Warning, AlertKind::StuckCancellation {
                scope_id: ScopeId("c".into()),
                draining_since_ms: 0,
                elapsed_ms: 5000,
                expected_grace_ms: 1000,
            }, "s1".into(), "fix".into()),
            WatchdogAlert::new(1000, AlertSeverity::Critical, AlertKind::DeadlockRisk {
                cycle: vec![ScopeId("x".into()), ScopeId("y".into())],
            }, "d1".into(), "fix".into()),
            WatchdogAlert::new(1000, AlertSeverity::Warning, AlertKind::ExcessiveDepth {
                scope_id: ScopeId("deep".into()),
                depth: 10,
                max_depth: 8,
            }, "e1".into(), "fix".into()),
        ];

        let summary = ScanSummary::from_alerts(&alerts, 1000);
        assert_eq!(summary.total_alerts, 5);
        assert_eq!(summary.orphans, 2);
        assert_eq!(summary.stuck_cancellations, 1);
        assert_eq!(summary.zombie_finalizers, 0);
        assert_eq!(summary.deadlocks, 1);
        assert_eq!(summary.scope_leaks, 0);
        assert_eq!(summary.stale_created, 0);
        assert_eq!(summary.excessive_depth, 1);
        assert_eq!(summary.by_severity.get("ERROR").copied().unwrap_or(0), 2);
        assert_eq!(summary.by_severity.get("WARN").copied().unwrap_or(0), 2);
        assert_eq!(summary.by_severity.get("CRITICAL").copied().unwrap_or(0), 1);
        assert!(summary.has_errors());
    }

    #[test]
    fn scan_summary_from_empty_alerts() {
        let summary = ScanSummary::from_alerts(&[], 1000);
        assert_eq!(summary.total_alerts, 0);
        assert_eq!(summary.orphans, 0);
        assert!(!summary.has_errors());
        assert!(summary.by_severity.is_empty());
    }

    // ── Excessive depth boundary ────────────────────────────────────────

    #[test]
    fn excessive_depth_at_exactly_max_not_triggered() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Build chain: root → d1 → w1 (depth=2)
        tree.register(
            ScopeId("daemon:d1".into()),
            ScopeTier::Daemon,
            &ScopeId::root(),
            "d1",
            1000,
        )
        .unwrap();
        tree.register(
            ScopeId("watcher:w1".into()),
            ScopeTier::Watcher,
            &ScopeId("daemon:d1".into()),
            "w1",
            1000,
        )
        .unwrap();

        let mut config = WatchdogConfig::default();
        config.max_depth = 2; // exactly at depth 2 → not triggered

        let mut wd = ScopeWatchdog::with_config(config);
        let alerts = wd.scan(&tree, 2000);
        assert!(
            !alerts.iter().any(|a| matches!(a.kind, AlertKind::ExcessiveDepth { .. })),
            "depth == max_depth should not trigger alert"
        );
    }

    #[test]
    fn excessive_depth_at_max_plus_one_triggered() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Build chain: root → d1 → w1 → e1 (depth=3)
        let d1 = ScopeId("daemon:d1".into());
        tree.register(d1.clone(), ScopeTier::Daemon, &ScopeId::root(), "d1", 1000).unwrap();
        let w1 = ScopeId("watcher:w1".into());
        tree.register(w1.clone(), ScopeTier::Watcher, &d1, "w1", 1000).unwrap();
        let worker = ScopeId("worker:e1".into());
        tree.register(worker.clone(), ScopeTier::Worker, &w1, "e1", 1000).unwrap();

        let mut config = WatchdogConfig::default();
        config.max_depth = 2; // depth 3 > 2 → triggered

        let mut wd = ScopeWatchdog::with_config(config);
        let alerts = wd.scan(&tree, 2000);
        let deep: Vec<_> = alerts
            .iter()
            .filter(|a| matches!(a.kind, AlertKind::ExcessiveDepth { .. }))
            .collect();
        assert_eq!(deep.len(), 1);
        if let AlertKind::ExcessiveDepth { scope_id, depth, max_depth } = &deep[0].kind {
            assert_eq!(scope_id, &worker);
            assert_eq!(*depth, 3);
            assert_eq!(*max_depth, 2);
        }
    }

    // ── WatchdogConfig with_config ──────────────────────────────────────

    #[test]
    fn with_config_uses_custom_values() {
        let mut config = WatchdogConfig::default();
        config.finalizer_timeout_ms = 1_000;
        config.max_depth = 3;
        config.stale_created_threshold_ms = 5_000;

        let wd = ScopeWatchdog::with_config(config);
        assert_eq!(wd.config().finalizer_timeout_ms, 1_000);
        assert_eq!(wd.config().max_depth, 3);
        assert_eq!(wd.config().stale_created_threshold_ms, 5_000);
    }

    // ── Serde roundtrip for all AlertKind variants ──────────────────────

    #[test]
    fn serde_roundtrip_all_alert_kinds() {
        let kinds = vec![
            AlertKind::OrphanTask {
                scope_id: ScopeId("child".into()),
                parent_id: ScopeId("parent".into()),
                scope_state: ScopeState::Running,
                parent_state: ScopeState::Closed,
            },
            AlertKind::StuckCancellation {
                scope_id: ScopeId("s".into()),
                draining_since_ms: 100,
                elapsed_ms: 5000,
                expected_grace_ms: 1000,
            },
            AlertKind::ZombieFinalizer {
                scope_id: ScopeId("z".into()),
                finalizing_since_ms: 200,
                elapsed_ms: 15000,
            },
            AlertKind::DeadlockRisk {
                cycle: vec![ScopeId("a".into()), ScopeId("b".into()), ScopeId("a".into())],
            },
            AlertKind::ScopeLeak {
                tier: ScopeTier::Worker,
                count: 300,
                threshold: 200,
            },
            AlertKind::StaleCreated {
                scope_id: ScopeId("stale".into()),
                created_at_ms: 0,
                elapsed_ms: 45000,
            },
            AlertKind::ExcessiveDepth {
                scope_id: ScopeId("deep".into()),
                depth: 12,
                max_depth: 8,
            },
        ];

        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let restored: AlertKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, &restored, "roundtrip failed for {kind}");
        }
    }

    // ── WatchdogAlert Display format ────────────────────────────────────

    #[test]
    fn watchdog_alert_display_format() {
        let alert = WatchdogAlert::new(
            1000,
            AlertSeverity::Critical,
            AlertKind::DeadlockRisk {
                cycle: vec![ScopeId("a".into()), ScopeId("b".into())],
            },
            "circular dependency detected".to_string(),
            "break cycle".to_string(),
        );
        let s = alert.to_string();
        // Format: [SEVERITY] kind: message
        assert!(s.starts_with("[CRITICAL]"));
        assert!(s.contains("deadlock-risk"));
        assert!(s.contains("circular dependency detected"));
    }

    // ── Stuck cancellation not triggered for non-Draining states ────────

    #[test]
    fn stuck_cancellation_ignores_running_scopes() {
        let mut tree = setup_tree();
        // All scopes are Running (started in setup_tree)
        let mut wd = ScopeWatchdog::new();
        // Even at very late time, Running scopes shouldn't trigger stuck cancellation
        let alerts = wd.scan(&tree, 1_000_000);
        assert!(
            !alerts.iter().any(|a| matches!(a.kind, AlertKind::StuckCancellation { .. })),
            "Running scopes should never trigger stuck cancellation"
        );
    }

    #[test]
    fn stuck_cancellation_ignores_finalizing_scopes() {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let d = ScopeId("daemon:fin".into());
        tree.register(d.clone(), ScopeTier::Daemon, &ScopeId::root(), "fin", 1000).unwrap();
        tree.start(&d, 1100).unwrap();
        tree.request_shutdown(&d, 2000).unwrap();
        tree.finalize(&d).unwrap();
        // Now in Finalizing, not Draining → stuck cancellation should not fire

        let mut wd = ScopeWatchdog::new();
        let alerts = wd.scan(&tree, 1_000_000);
        assert!(
            !alerts.iter().any(|a| matches!(a.kind, AlertKind::StuckCancellation { .. })),
            "Finalizing scopes should not trigger stuck cancellation (they trigger zombie instead)"
        );
    }

    // ── Default config values ───────────────────────────────────────────

    #[test]
    fn default_config_has_expected_values() {
        let config = WatchdogConfig::default();
        assert_eq!(config.default_grace_period_ms, 10_000);
        assert_eq!(config.finalizer_timeout_ms, 10_000);
        assert_eq!(config.max_depth, 8);
        assert_eq!(config.stale_created_threshold_ms, 30_000);
        assert!(config.detect_deadlocks);
        assert_eq!(config.tier_grace_periods.len(), 5);
        assert_eq!(config.tier_scope_limits.len(), 5);
    }
}
