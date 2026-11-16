//! Generic bounded policy-decision log engine.
//!
//! This module owns append/evict/export/snapshot mechanics without depending
//! on `frankenterm-core` policy enums or redaction internals.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

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

/// Decision class used by the generic retention counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionClass {
    Allow,
    Deny,
    RequireApproval,
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

/// Entry requirements for generic filtering helpers.
pub trait DecisionLogRecord {
    fn seq(&self) -> u64;
    fn timestamp_ms(&self) -> u64;
    fn pane_id(&self) -> Option<u64>;
    fn decision_class(&self) -> DecisionClass;
}

/// Bounded append-only policy decision log engine.
#[derive(Debug)]
pub struct PolicyDecisionLogEngine<E> {
    config: DecisionLogConfig,
    entries: VecDeque<E>,
    next_seq: u64,
    total_recorded: u64,
    total_evicted: u64,
    deny_count: u64,
    allow_count: u64,
    require_approval_count: u64,
}

impl<E> PolicyDecisionLogEngine<E> {
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

    /// Records a new entry built with the assigned sequence number.
    pub fn record_with<F>(&mut self, decision: DecisionClass, build: F) -> Option<u64>
    where
        F: FnOnce(u64) -> E,
    {
        if !self.config.record_allows && decision == DecisionClass::Allow {
            return None;
        }

        let seq = self.next_seq;
        self.next_seq += 1;

        if self.entries.len() >= self.config.max_entries {
            self.entries.pop_front();
            self.total_evicted += 1;
        }

        match decision {
            DecisionClass::Allow => self.allow_count += 1,
            DecisionClass::Deny => self.deny_count += 1,
            DecisionClass::RequireApproval => self.require_approval_count += 1,
        }

        self.entries.push_back(build(seq));
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

    /// Returns all entries as an iterator.
    pub fn entries(&self) -> impl Iterator<Item = &E> {
        self.entries.iter()
    }

    /// Clears all entries while preserving counters and sequence numbers.
    pub fn clear(&mut self) {
        self.total_evicted += self.entries.len() as u64;
        self.entries.clear();
    }

    /// Exports entries matching a filter as JSON lines.
    pub fn export_jsonl<F>(&self, filter: F) -> Result<String, serde_json::Error>
    where
        E: Serialize,
        F: Fn(&E) -> bool,
    {
        let mut lines = Vec::new();
        for entry in self.entries.iter().filter(|e| filter(e)) {
            lines.push(serde_json::to_string(entry)?);
        }
        Ok(lines.join("\n"))
    }

    /// Exports all entries as a JSON array string.
    pub fn export_json(&self) -> Result<String, serde_json::Error>
    where
        E: Serialize,
    {
        let entries: Vec<&E> = self.entries.iter().collect();
        serde_json::to_string_pretty(&entries)
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

impl<E: DecisionLogRecord> PolicyDecisionLogEngine<E> {
    /// Returns the entry with the given sequence number.
    #[must_use]
    pub fn get(&self, seq: u64) -> Option<&E> {
        self.entries.iter().find(|e| e.seq() == seq)
    }

    /// Returns entries filtered by decision outcome.
    pub fn by_decision(&self, decision: DecisionClass) -> Vec<&E> {
        self.entries
            .iter()
            .filter(|e| e.decision_class() == decision)
            .collect()
    }

    /// Returns entries within a time range, inclusive.
    pub fn by_time_range(&self, start_ms: u64, end_ms: u64) -> Vec<&E> {
        self.entries
            .iter()
            .filter(|e| e.timestamp_ms() >= start_ms && e.timestamp_ms() <= end_ms)
            .collect()
    }

    /// Returns entries for a specific pane.
    pub fn by_pane(&self, pane_id: u64) -> Vec<&E> {
        self.entries
            .iter()
            .filter(|e| e.pane_id() == Some(pane_id))
            .collect()
    }
}
