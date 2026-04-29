//! Append-only policy decision log for forensics and compliance.
//!
//! Records every policy decision with full context so operators can audit,
//! explain, and export decision history for compliance and disaster recovery.
//!
//! Part of ft-2shtw.

use std::sync::LazyLock;

pub use frankenterm_core_audit_types::policy_decision_log_engine::{
    DecisionLogConfig, DecisionLogSnapshot,
};
use frankenterm_core_audit_types::policy_decision_log_engine::{
    DecisionClass, DecisionLogRecord, PolicyDecisionLogEngine,
};
use serde::{Deserialize, Serialize};

use crate::policy::{ActionKind, ActorKind, PolicySurface};
use crate::policy_dsl::DslDecision;
use crate::redactor::Redactor;

/// ft-3se13: shared redactor used to strip secrets from decision-log
/// fields at the storage boundary. The decision log is part of the
/// audit-export surface, so a `Deny` reason like `"denied: text
/// contains sk-ant-api03-LEAK..."` would otherwise persist the
/// attacker-supplied secret in the persisted log + telemetry
/// snapshots. Performing the scrub inside `record()` makes the
/// invariant independent of every call site remembering — a
/// future caller cannot route around it without rewriting the
/// log itself.
static DECISION_LOG_REDACTOR: LazyLock<Redactor> = LazyLock::new(Redactor::new);

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

impl DecisionOutcome {
    fn class(self) -> DecisionClass {
        match self {
            Self::Allow => DecisionClass::Allow,
            Self::Deny => DecisionClass::Deny,
            Self::RequireApproval => DecisionClass::RequireApproval,
        }
    }
}

impl DecisionLogRecord for PolicyDecisionEntry {
    fn seq(&self) -> u64 {
        self.seq
    }

    fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    fn pane_id(&self) -> Option<u64> {
        self.pane_id
    }

    fn decision_class(&self) -> DecisionClass {
        self.decision.class()
    }
}

// =============================================================================
// Decision log
// =============================================================================

/// Bounded append-only log of policy decisions.
///
/// Entries are stored in chronological order. When the log reaches
/// `max_entries`, the oldest entries are evicted.
#[derive(Debug)]
pub struct PolicyDecisionLog {
    engine: PolicyDecisionLogEngine<PolicyDecisionEntry>,
}

impl PolicyDecisionLog {
    /// Creates a new log with the given configuration.
    #[must_use]
    pub fn new(config: DecisionLogConfig) -> Self {
        Self {
            engine: PolicyDecisionLogEngine::new(config),
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
        // ft-3se13: scrub secrets from the human-readable `reason`
        // field before it lands in the persisted log. Attacker-
        // controlled text (e.g. matched_pattern fragments referenced
        // by DSL rules, or PolicyInput.text_summary echoed into a
        // deny reason) would otherwise persist verbatim through
        // export, telemetry snapshots, and audit dumps. `rule_id` is
        // a stable static slug (`policy.deny.spawn_robot`-style) and
        // is left untouched.
        let redacted_reason = reason.map(|s| DECISION_LOG_REDACTOR.redact(&s));
        self.engine.record_with(decision.class(), |seq| PolicyDecisionEntry {
            seq,
            timestamp_ms,
            action,
            actor,
            surface,
            pane_id,
            decision,
            rule_id,
            reason: redacted_reason,
            rules_evaluated,
        })
    }

    /// Returns the number of entries currently in the log.
    #[must_use]
    pub fn len(&self) -> usize {
        self.engine.len()
    }

    /// Returns true if the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.engine.is_empty()
    }

    /// Returns the entry with the given sequence number.
    #[must_use]
    pub fn get(&self, seq: u64) -> Option<&PolicyDecisionEntry> {
        self.engine.get(seq)
    }

    /// Returns all entries as a slice-like iterator.
    pub fn entries(&self) -> impl Iterator<Item = &PolicyDecisionEntry> {
        self.engine.entries()
    }

    /// Returns entries filtered by decision outcome.
    pub fn by_decision(&self, decision: DecisionOutcome) -> Vec<&PolicyDecisionEntry> {
        self.engine.by_decision(decision.class())
    }

    /// Returns entries filtered by actor kind.
    pub fn by_actor(&self, actor: ActorKind) -> Vec<&PolicyDecisionEntry> {
        self.engine.entries().filter(|e| e.actor == actor).collect()
    }

    /// Returns entries filtered by action kind.
    pub fn by_action(&self, action: ActionKind) -> Vec<&PolicyDecisionEntry> {
        self.engine.entries().filter(|e| e.action == action).collect()
    }

    /// Returns entries filtered by surface.
    pub fn by_surface(&self, surface: PolicySurface) -> Vec<&PolicyDecisionEntry> {
        self.engine
            .entries()
            .filter(|e| e.surface == surface)
            .collect()
    }

    /// Returns entries within a time range (inclusive).
    pub fn by_time_range(&self, start_ms: u64, end_ms: u64) -> Vec<&PolicyDecisionEntry> {
        self.engine.by_time_range(start_ms, end_ms)
    }

    /// Returns entries for a specific pane.
    pub fn by_pane(&self, pane_id: u64) -> Vec<&PolicyDecisionEntry> {
        self.engine.by_pane(pane_id)
    }

    /// Exports all entries as a JSON array string.
    pub fn export_json(&self) -> Result<String, serde_json::Error> {
        self.engine.export_json()
    }

    /// Exports entries matching a filter as JSON lines (one JSON object per line).
    pub fn export_jsonl<F>(&self, filter: F) -> Result<String, serde_json::Error>
    where
        F: Fn(&PolicyDecisionEntry) -> bool,
    {
        self.engine.export_jsonl(filter)
    }

    /// Clears all entries (preserves counters and sequence numbers).
    pub fn clear(&mut self) {
        self.engine.clear();
    }

    /// Returns a diagnostic snapshot.
    #[must_use]
    pub fn snapshot(&self) -> DecisionLogSnapshot {
        self.engine.snapshot()
    }
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

    // ── ft-3se13: secret redaction at the storage boundary ────────────────
    //
    // Decision-log entries are exported via telemetry snapshots, audit
    // dumps, and operator queries — every consumer of `entry.reason`
    // gets whatever bytes the recorder placed there. A `Deny` reason
    // built from a DSL rule that names matched attacker text (or a
    // future caller that echoes PolicyInput.text_summary into the
    // reason) would otherwise persist secrets verbatim.
    //
    // Fixture: record a deny whose reason carries each of the
    // newly-covered ft-3xek9 token shapes; assert the persisted entry
    // contains no original secret bytes.

    fn record_with_reason<'a>(
        log: &'a mut PolicyDecisionLog,
        reason: &str,
    ) -> &'a PolicyDecisionEntry {
        let seq = log
            .record(
                1_777_200_000_000,
                ActionKind::SendText,
                ActorKind::Robot,
                PolicySurface::Mux,
                Some(7),
                DecisionOutcome::Deny,
                Some("policy.deny.contains_secret".to_owned()),
                Some(reason.to_owned()),
                3,
            )
            .expect("ft-3se13: deny must record");
        log.get(seq).expect("ft-3se13: recorded entry must be retrievable")
    }

    #[test]
    fn record_redacts_openai_secret_in_reason() {
        let mut log = PolicyDecisionLog::with_defaults();
        let raw = "denied: text contains sk-proj-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
        let entry = record_with_reason(&mut log, raw);
        let stored = entry.reason.as_deref().expect("reason recorded");
        assert!(
            !stored.contains("sk-proj-aBcDeFgHiJkLmNoPqRsTuVwXyZ"),
            "ft-3se13: OpenAI secret leaked into decision log: {stored:?}"
        );
        assert!(stored.contains("[REDACTED]"));
    }

    #[test]
    fn record_redacts_anthropic_secret_in_reason() {
        let mut log = PolicyDecisionLog::with_defaults();
        let raw =
            "denied by rule X: matched_pattern=sk-ant-api03-aBcDeFgHiJkLmNoPqRsTuVwXyZ_1234567890";
        let entry = record_with_reason(&mut log, raw);
        let stored = entry.reason.as_deref().expect("reason recorded");
        assert!(
            !stored.contains("sk-ant-api03-a"),
            "ft-3se13: Anthropic secret leaked into decision log: {stored:?}"
        );
        assert!(stored.contains("[REDACTED]"));
    }

    #[test]
    fn record_redacts_xai_secret_in_reason() {
        let mut log = PolicyDecisionLog::with_defaults();
        let raw = "deny: input contained xai-aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890aBcDeFgHiJkLmNoPqRsT01234567890";
        let entry = record_with_reason(&mut log, raw);
        let stored = entry.reason.as_deref().expect("reason recorded");
        assert!(
            !stored.contains("xai-a"),
            "ft-3se13: xAI secret leaked into decision log: {stored:?}"
        );
        assert!(stored.contains("[REDACTED]"));
    }

    #[test]
    fn record_redacts_github_pat_in_reason() {
        let mut log = PolicyDecisionLog::with_defaults();
        let raw =
            "denied: pasted token github_pat_11ABCDEFG0aBcDeFg_HiJkLmNoPqRsTuVwXyZ1234567890ABCDE";
        let entry = record_with_reason(&mut log, raw);
        let stored = entry.reason.as_deref().expect("reason recorded");
        assert!(
            !stored.contains("github_pat_11"),
            "ft-3se13: GitHub PAT leaked into decision log: {stored:?}"
        );
        assert!(stored.contains("[REDACTED]"));
    }

    #[test]
    fn record_preserves_clean_reason() {
        // Defensive: the redactor must not corrupt benign reasons
        // that share short fragments with secret prefixes.
        let mut log = PolicyDecisionLog::with_defaults();
        let raw = "denied: actor=Robot lacks send_text capability";
        let entry = record_with_reason(&mut log, raw);
        let stored = entry.reason.as_deref().expect("reason recorded");
        assert_eq!(
            stored, raw,
            "ft-3se13: benign reason must round-trip unchanged"
        );
    }

    #[test]
    fn record_redaction_visible_in_entries_iterator() {
        // Closes the export-surface loop: redaction must be visible
        // through the public iterator that telemetry / audit dumps use.
        let mut log = PolicyDecisionLog::with_defaults();
        record_with_reason(
            &mut log,
            "denied: text contains hf_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890",
        );
        let any_leaked = log
            .entries()
            .any(|e| e.reason.as_deref().is_some_and(|r| r.contains("hf_a")));
        assert!(
            !any_leaked,
            "ft-3se13: secret reachable through entries() iterator"
        );
    }

    #[test]
    fn record_skips_redaction_when_reason_is_none() {
        // A None reason must remain None (no spurious empty string).
        let mut log = PolicyDecisionLog::with_defaults();
        let seq = log
            .record(
                1_777_200_000_000,
                ActionKind::SendText,
                ActorKind::Robot,
                PolicySurface::Mux,
                Some(7),
                DecisionOutcome::Allow,
                None,
                None,
                0,
            )
            .expect("allow recorded");
        let entry = log.get(seq).expect("entry");
        assert!(entry.reason.is_none());
    }
}
