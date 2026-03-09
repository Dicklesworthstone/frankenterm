//! Append-only policy decision log for forensics and compliance.
//!
//! Records every policy decision with full context so operators can audit,
//! explain, and export decision history for compliance and disaster recovery.
//!
//! Part of ft-2shtw.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::policy::{ActionKind, ActorKind, PolicySurface};
use crate::policy_dsl::DslDecision;

// =============================================================================
// Decision entry
// =============================================================================

/// A single recorded policy decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionEntry {
    /// Monotonic sequence number within the log.
    pub seq: u64,
    /// Timestamp of the decision (epoch milliseconds).
    pub timestamp_ms: u64,
    /// Action that was evaluated.
    pub action: ActionKind,
    /// Actor who requested the action.
    pub actor: ActorKind,
    /// Surface where the request originated.
    pub surface: PolicySurface,
    /// Target pane ID (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// The decision rendered.
    pub decision: DecisionOutcome,
    /// ID of the rule that determined the outcome (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Human-readable reason for the decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Number of rules evaluated.
    pub rules_evaluated: u32,
}

/// The outcome of a policy decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOutcome {
    Allow,
    Deny,
    RequireApproval,
}

impl From<DslDecision> for DecisionOutcome {
    fn from(d: DslDecision) -> Self {
        match d {
            DslDecision::Allow => Self::Allow,
            DslDecision::Deny => Self::Deny,
            DslDecision::RequireApproval => Self::RequireApproval,
        }
    }
}

// =============================================================================
// Decision log
// =============================================================================

/// Configuration for the decision log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionLogConfig {
    /// Maximum number of entries to retain (oldest evicted first).
    pub max_entries: usize,
    /// Whether to record allow decisions (may be high-volume).
    pub record_allows: bool,
}

impl Default for DecisionLogConfig {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            record_allows: true,
        }
    }
}

/// Bounded append-only log of policy decisions.
///
/// Entries are stored in chronological order. When the log reaches
/// `max_entries`, the oldest entries are evicted.
#[derive(Debug)]
pub struct PolicyDecisionLog {
    config: DecisionLogConfig,
    entries: VecDeque<PolicyDecisionEntry>,
    next_seq: u64,
    total_recorded: u64,
    total_evicted: u64,
    deny_count: u64,
    allow_count: u64,
    require_approval_count: u64,
}

impl PolicyDecisionLog {
    /// Creates a new log with the given configuration.
    #[must_use]
    pub fn new(config: DecisionLogConfig) -> Self {
        Self {
            entries: VecDeque::with_capacity(config.max_entries.min(1024)),
            config,
            next_seq: 0,
            total_recorded: 0,
            total_evicted: 0,
            deny_count: 0,
            allow_count: 0,
            require_approval_count: 0,
        }
    }

    /// Creates a new log with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(DecisionLogConfig::default())
    }

    /// Records a new decision entry.
    ///
    /// Returns the sequence number assigned to the entry, or `None` if
    /// the entry was filtered out by configuration.
    pub fn record(
        &mut self,
        timestamp_ms: u64,
        action: ActionKind,
        actor: ActorKind,
        surface: PolicySurface,
        pane_id: Option<u64>,
        decision: DecisionOutcome,
        rule_id: Option<String>,
        reason: Option<String>,
        rules_evaluated: u32,
    ) -> Option<u64> {
        // Filter allows if configured
        if !self.config.record_allows && decision == DecisionOutcome::Allow {
            return None;
        }

        let seq = self.next_seq;
        self.next_seq += 1;

        let entry = PolicyDecisionEntry {
            seq,
            timestamp_ms,
            action,
            actor,
            surface,
            pane_id,
            decision,
            rule_id,
            reason,
            rules_evaluated,
        };

        // Evict oldest if at capacity
        if self.entries.len() >= self.config.max_entries {
            self.entries.pop_front();
            self.total_evicted += 1;
        }

        // Track counters
        match decision {
            DecisionOutcome::Allow => self.allow_count += 1,
            DecisionOutcome::Deny => self.deny_count += 1,
            DecisionOutcome::RequireApproval => self.require_approval_count += 1,
        }

        self.entries.push_back(entry);
        self.total_recorded += 1;

        Some(seq)
    }

    /// Returns the number of entries currently in the log.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the entry with the given sequence number.
    #[must_use]
    pub fn get(&self, seq: u64) -> Option<&PolicyDecisionEntry> {
        self.entries.iter().find(|e| e.seq == seq)
    }

    /// Returns all entries as a slice-like iterator.
    pub fn entries(&self) -> impl Iterator<Item = &PolicyDecisionEntry> {
        self.entries.iter()
    }

    /// Returns entries filtered by decision outcome.
    pub fn by_decision(&self, decision: DecisionOutcome) -> Vec<&PolicyDecisionEntry> {
        self.entries
            .iter()
            .filter(|e| e.decision == decision)
            .collect()
    }

    /// Returns entries filtered by actor kind.
    pub fn by_actor(&self, actor: ActorKind) -> Vec<&PolicyDecisionEntry> {
        self.entries.iter().filter(|e| e.actor == actor).collect()
    }

    /// Returns entries filtered by action kind.
    pub fn by_action(&self, action: ActionKind) -> Vec<&PolicyDecisionEntry> {
        self.entries.iter().filter(|e| e.action == action).collect()
    }

    /// Returns entries filtered by surface.
    pub fn by_surface(&self, surface: PolicySurface) -> Vec<&PolicyDecisionEntry> {
        self.entries
            .iter()
            .filter(|e| e.surface == surface)
            .collect()
    }

    /// Returns entries within a time range (inclusive).
    pub fn by_time_range(&self, start_ms: u64, end_ms: u64) -> Vec<&PolicyDecisionEntry> {
        self.entries
            .iter()
            .filter(|e| e.timestamp_ms >= start_ms && e.timestamp_ms <= end_ms)
            .collect()
    }

    /// Returns entries for a specific pane.
    pub fn by_pane(&self, pane_id: u64) -> Vec<&PolicyDecisionEntry> {
        self.entries
            .iter()
            .filter(|e| e.pane_id == Some(pane_id))
            .collect()
    }

    /// Exports all entries as a JSON array string.
    pub fn export_json(&self) -> Result<String, serde_json::Error> {
        let entries: Vec<&PolicyDecisionEntry> = self.entries.iter().collect();
        serde_json::to_string_pretty(&entries)
    }

    /// Exports entries matching a filter as JSON lines (one JSON object per line).
    pub fn export_jsonl<F>(&self, filter: F) -> Result<String, serde_json::Error>
    where
        F: Fn(&PolicyDecisionEntry) -> bool,
    {
        let mut lines = Vec::new();
        for entry in self.entries.iter().filter(|e| filter(e)) {
            lines.push(serde_json::to_string(entry)?);
        }
        Ok(lines.join("\n"))
    }

    /// Clears all entries (preserves counters and sequence numbers).
    pub fn clear(&mut self) {
        self.total_evicted += self.entries.len() as u64;
        self.entries.clear();
    }

    /// Returns a diagnostic snapshot.
    #[must_use]
    pub fn snapshot(&self) -> DecisionLogSnapshot {
        DecisionLogSnapshot {
            current_entries: self.entries.len(),
            max_entries: self.config.max_entries,
            total_recorded: self.total_recorded,
            total_evicted: self.total_evicted,
            next_seq: self.next_seq,
            deny_count: self.deny_count,
            allow_count: self.allow_count,
            require_approval_count: self.require_approval_count,
            record_allows: self.config.record_allows,
        }
    }
}

/// Diagnostic snapshot of the decision log state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionLogSnapshot {
    pub current_entries: usize,
    pub max_entries: usize,
    pub total_recorded: u64,
    pub total_evicted: u64,
    pub next_seq: u64,
    pub deny_count: u64,
    pub allow_count: u64,
    pub require_approval_count: u64,
    pub record_allows: bool,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(
        log: &mut PolicyDecisionLog,
        ts: u64,
        action: ActionKind,
        actor: ActorKind,
        decision: DecisionOutcome,
    ) -> u64 {
        log.record(
            ts,
            action,
            actor,
            PolicySurface::Mux,
            None,
            decision,
            None,
            None,
            1,
        )
        .unwrap()
    }

    #[test]
    fn record_and_get() {
        let mut log = PolicyDecisionLog::with_defaults();
        let seq = make_entry(
            &mut log,
            1000,
            ActionKind::Spawn,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );
        assert_eq!(seq, 0);
        assert_eq!(log.len(), 1);
        let entry = log.get(0).unwrap();
        assert_eq!(entry.action, ActionKind::Spawn);
        assert_eq!(entry.actor, ActorKind::Robot);
        assert_eq!(entry.decision, DecisionOutcome::Allow);
    }

    #[test]
    fn eviction_at_max() {
        let config = DecisionLogConfig {
            max_entries: 3,
            record_allows: true,
        };
        let mut log = PolicyDecisionLog::new(config);
        for i in 0..5 {
            make_entry(
                &mut log,
                i * 100,
                ActionKind::Spawn,
                ActorKind::Robot,
                DecisionOutcome::Allow,
            );
        }
        assert_eq!(log.len(), 3);
        assert_eq!(log.snapshot().total_evicted, 2);
        // Oldest entries (seq 0, 1) should be gone
        assert!(log.get(0).is_none());
        assert!(log.get(1).is_none());
        assert!(log.get(2).is_some());
    }

    #[test]
    fn filter_allows_disabled() {
        let config = DecisionLogConfig {
            max_entries: 100,
            record_allows: false,
        };
        let mut log = PolicyDecisionLog::new(config);
        let result = log.record(
            1000,
            ActionKind::Spawn,
            ActorKind::Robot,
            PolicySurface::Mux,
            None,
            DecisionOutcome::Allow,
            None,
            None,
            1,
        );
        assert!(result.is_none());
        assert!(log.is_empty());

        // Deny should still be recorded
        let result = log.record(
            1001,
            ActionKind::Spawn,
            ActorKind::Robot,
            PolicySurface::Mux,
            None,
            DecisionOutcome::Deny,
            None,
            None,
            1,
        );
        assert!(result.is_some());
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn by_decision_filter() {
        let mut log = PolicyDecisionLog::with_defaults();
        make_entry(
            &mut log,
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );
        make_entry(
            &mut log,
            200,
            ActionKind::Close,
            ActorKind::Robot,
            DecisionOutcome::Deny,
        );
        make_entry(
            &mut log,
            300,
            ActionKind::SendText,
            ActorKind::Human,
            DecisionOutcome::Allow,
        );

        let denies = log.by_decision(DecisionOutcome::Deny);
        assert_eq!(denies.len(), 1);
        assert_eq!(denies[0].action, ActionKind::Close);

        let allows = log.by_decision(DecisionOutcome::Allow);
        assert_eq!(allows.len(), 2);
    }

    #[test]
    fn by_actor_filter() {
        let mut log = PolicyDecisionLog::with_defaults();
        make_entry(
            &mut log,
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );
        make_entry(
            &mut log,
            200,
            ActionKind::Close,
            ActorKind::Human,
            DecisionOutcome::Deny,
        );

        let robots = log.by_actor(ActorKind::Robot);
        assert_eq!(robots.len(), 1);
    }

    #[test]
    fn by_action_filter() {
        let mut log = PolicyDecisionLog::with_defaults();
        make_entry(
            &mut log,
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );
        make_entry(
            &mut log,
            200,
            ActionKind::Spawn,
            ActorKind::Human,
            DecisionOutcome::Deny,
        );
        make_entry(
            &mut log,
            300,
            ActionKind::Close,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );

        let spawns = log.by_action(ActionKind::Spawn);
        assert_eq!(spawns.len(), 2);
    }

    #[test]
    fn by_surface_filter() {
        let mut log = PolicyDecisionLog::with_defaults();
        log.record(
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            PolicySurface::Mux,
            None,
            DecisionOutcome::Allow,
            None,
            None,
            1,
        );
        log.record(
            200,
            ActionKind::Spawn,
            ActorKind::Robot,
            PolicySurface::Connector,
            None,
            DecisionOutcome::Deny,
            None,
            None,
            1,
        );

        let mux = log.by_surface(PolicySurface::Mux);
        assert_eq!(mux.len(), 1);
        let conn = log.by_surface(PolicySurface::Connector);
        assert_eq!(conn.len(), 1);
    }

    #[test]
    fn by_time_range() {
        let mut log = PolicyDecisionLog::with_defaults();
        make_entry(
            &mut log,
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );
        make_entry(
            &mut log,
            200,
            ActionKind::Close,
            ActorKind::Robot,
            DecisionOutcome::Deny,
        );
        make_entry(
            &mut log,
            300,
            ActionKind::SendText,
            ActorKind::Human,
            DecisionOutcome::Allow,
        );

        let range = log.by_time_range(150, 250);
        assert_eq!(range.len(), 1);
        assert_eq!(range[0].timestamp_ms, 200);
    }

    #[test]
    fn by_pane_filter() {
        let mut log = PolicyDecisionLog::with_defaults();
        log.record(
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            PolicySurface::Mux,
            Some(42),
            DecisionOutcome::Allow,
            None,
            None,
            1,
        );
        log.record(
            200,
            ActionKind::Close,
            ActorKind::Robot,
            PolicySurface::Mux,
            Some(99),
            DecisionOutcome::Deny,
            None,
            None,
            1,
        );
        log.record(
            300,
            ActionKind::SendText,
            ActorKind::Human,
            PolicySurface::Mux,
            None,
            DecisionOutcome::Allow,
            None,
            None,
            1,
        );

        let pane42 = log.by_pane(42);
        assert_eq!(pane42.len(), 1);
    }

    #[test]
    fn export_json() {
        let mut log = PolicyDecisionLog::with_defaults();
        make_entry(
            &mut log,
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );
        let json = log.export_json().unwrap();
        assert!(json.contains("\"action\""));
        assert!(json.contains("\"decision\""));
    }

    #[test]
    fn export_jsonl_with_filter() {
        let mut log = PolicyDecisionLog::with_defaults();
        make_entry(
            &mut log,
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );
        make_entry(
            &mut log,
            200,
            ActionKind::Close,
            ActorKind::Robot,
            DecisionOutcome::Deny,
        );

        let jsonl = log
            .export_jsonl(|e| e.decision == DecisionOutcome::Deny)
            .unwrap();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"deny\""));
    }

    #[test]
    fn clear_preserves_counters() {
        let mut log = PolicyDecisionLog::with_defaults();
        make_entry(
            &mut log,
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );
        make_entry(
            &mut log,
            200,
            ActionKind::Close,
            ActorKind::Robot,
            DecisionOutcome::Deny,
        );
        log.clear();
        assert!(log.is_empty());
        let snap = log.snapshot();
        assert_eq!(snap.total_recorded, 2);
        assert_eq!(snap.total_evicted, 2);
        assert_eq!(snap.next_seq, 2);
    }

    #[test]
    fn snapshot_reflects_state() {
        let mut log = PolicyDecisionLog::with_defaults();
        make_entry(
            &mut log,
            100,
            ActionKind::Spawn,
            ActorKind::Robot,
            DecisionOutcome::Allow,
        );
        make_entry(
            &mut log,
            200,
            ActionKind::Close,
            ActorKind::Robot,
            DecisionOutcome::Deny,
        );
        make_entry(
            &mut log,
            300,
            ActionKind::SendText,
            ActorKind::Mcp,
            DecisionOutcome::RequireApproval,
        );

        let snap = log.snapshot();
        assert_eq!(snap.current_entries, 3);
        assert_eq!(snap.total_recorded, 3);
        assert_eq!(snap.deny_count, 1);
        assert_eq!(snap.allow_count, 1);
        assert_eq!(snap.require_approval_count, 1);
    }

    #[test]
    fn decision_outcome_from_dsl() {
        assert_eq!(
            DecisionOutcome::from(DslDecision::Allow),
            DecisionOutcome::Allow
        );
        assert_eq!(
            DecisionOutcome::from(DslDecision::Deny),
            DecisionOutcome::Deny
        );
        assert_eq!(
            DecisionOutcome::from(DslDecision::RequireApproval),
            DecisionOutcome::RequireApproval
        );
    }

    #[test]
    fn entry_serde_roundtrip() {
        let entry = PolicyDecisionEntry {
            seq: 42,
            timestamp_ms: 1234567890,
            action: ActionKind::Spawn,
            actor: ActorKind::Robot,
            surface: PolicySurface::Mux,
            pane_id: Some(99),
            decision: DecisionOutcome::Deny,
            rule_id: Some("deny-robot-spawn".to_owned()),
            reason: Some("Robots cannot spawn".to_owned()),
            rules_evaluated: 5,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: PolicyDecisionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = DecisionLogConfig {
            max_entries: 5000,
            record_allows: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: DecisionLogConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let snap = DecisionLogSnapshot {
            current_entries: 42,
            max_entries: 10000,
            total_recorded: 100,
            total_evicted: 58,
            next_seq: 100,
            deny_count: 20,
            allow_count: 70,
            require_approval_count: 10,
            record_allows: true,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: DecisionLogSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }
}
