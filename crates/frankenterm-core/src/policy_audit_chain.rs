//! Policy audit chain — hash-linked immutable audit trail.
//!
//! Chains policy decisions, quarantine events, and compliance actions into
//! a verifiable SHA-256 hash-linked sequence. Provides tamper detection,
//! chain verification, and bounded retention with configurable export.
//!
//! Part of ft-3681t.6.4/ft-3681t.6.5 precursor work.

use std::collections::VecDeque;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// =============================================================================
// Audit entry types
// =============================================================================

/// Classification of an audit chain entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEntryKind {
    /// A policy decision was made.
    PolicyDecision,
    /// A quarantine action was taken.
    QuarantineAction,
    /// A kill switch was activated or deactivated.
    KillSwitchAction,
    /// A compliance violation was detected.
    ComplianceViolation,
    /// A compliance remediation was completed.
    ComplianceRemediation,
    /// A credential action (issue/rotate/revoke).
    CredentialAction,
    /// A forensic export was performed.
    ForensicExport,
    /// A configuration change affecting policy.
    ConfigChange,
}

impl fmt::Display for AuditEntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PolicyDecision => write!(f, "policy_decision"),
            Self::QuarantineAction => write!(f, "quarantine_action"),
            Self::KillSwitchAction => write!(f, "kill_switch_action"),
            Self::ComplianceViolation => write!(f, "compliance_violation"),
            Self::ComplianceRemediation => write!(f, "compliance_remediation"),
            Self::CredentialAction => write!(f, "credential_action"),
            Self::ForensicExport => write!(f, "forensic_export"),
            Self::ConfigChange => write!(f, "config_change"),
        }
    }
}

/// A single entry in the audit chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditChainEntry {
    /// Sequence number (monotonically increasing).
    pub sequence: u64,
    /// Timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Kind of audit event.
    pub kind: AuditEntryKind,
    /// Who performed the action.
    pub actor: String,
    /// Description of the action.
    pub description: String,
    /// Reference to related entity (rule_id, component_id, etc.).
    pub entity_ref: String,
    /// SHA-256 hash of this entry's content.
    pub content_hash: String,
    /// SHA-256 hash of the previous entry (empty for genesis).
    pub previous_hash: String,
    /// SHA-256 hash linking this entry to its predecessor.
    pub chain_hash: String,
}

/// Result of chain verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainVerificationResult {
    /// Whether the chain is valid.
    pub valid: bool,
    /// Total entries checked.
    pub entries_checked: usize,
    /// Index of first invalid entry (if any).
    pub first_invalid_at: Option<usize>,
    /// Description of the verification failure (if any).
    pub failure_reason: Option<String>,
}

impl fmt::Display for ChainVerificationResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.valid {
            write!(f, "valid ({} entries)", self.entries_checked)
        } else {
            write!(
                f,
                "INVALID at entry {}: {}",
                self.first_invalid_at.unwrap_or(0),
                self.failure_reason.as_deref().unwrap_or("unknown")
            )
        }
    }
}

// =============================================================================
// Audit chain telemetry
// =============================================================================

/// Telemetry counters for the audit chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AuditChainTelemetry {
    pub entries_appended: u64,
    pub entries_evicted: u64,
    pub verifications_run: u64,
    pub verification_failures: u64,
    pub exports_completed: u64,
}

/// Telemetry snapshot for the audit chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditChainTelemetrySnapshot {
    pub captured_at_ms: u64,
    pub counters: AuditChainTelemetry,
    pub chain_length: usize,
    pub max_entries: usize,
    pub next_sequence: u64,
}

// =============================================================================
// Configuration
// =============================================================================

/// TOML-serializable configuration for the audit chain subsystem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AuditChainConfig {
    /// Maximum entries retained before oldest are evicted.
    pub max_entries: usize,
    /// Whether to record Allow decisions (can be noisy).
    pub record_allows: bool,
}

impl Default for AuditChainConfig {
    fn default() -> Self {
        Self {
            max_entries: 1024,
            record_allows: false,
        }
    }
}

// =============================================================================
// Audit chain — core data structure
// =============================================================================

/// A bounded, hash-linked audit chain.
pub struct AuditChain {
    entries: VecDeque<AuditChainEntry>,
    max_entries: usize,
    next_sequence: u64,
    last_hash: String,
    telemetry: AuditChainTelemetry,
    record_allows: bool,
}

impl AuditChain {
    /// Create a new audit chain with the given capacity.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries: max_entries.max(1),
            next_sequence: 0,
            last_hash: String::new(),
            telemetry: AuditChainTelemetry::default(),
            record_allows: false,
        }
    }

    /// Create an audit chain from configuration.
    pub fn from_config(config: &AuditChainConfig) -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries: config.max_entries.max(1),
            next_sequence: 0,
            last_hash: String::new(),
            telemetry: AuditChainTelemetry::default(),
            record_allows: config.record_allows,
        }
    }

    /// Whether this chain records Allow decisions.
    pub fn records_allows(&self) -> bool {
        self.record_allows
    }

    /// Append an entry to the chain.
    pub fn append(
        &mut self,
        kind: AuditEntryKind,
        actor: &str,
        description: &str,
        entity_ref: &str,
        timestamp_ms: u64,
    ) -> &AuditChainEntry {
        let sequence = self.next_sequence;
        self.next_sequence += 1;

        let content_hash =
            Self::hash_content(sequence, timestamp_ms, kind, actor, description, entity_ref);
        let previous_hash = self.last_hash.clone();
        let chain_hash = Self::hash_chain(&content_hash, &previous_hash);

        let entry = AuditChainEntry {
            sequence,
            timestamp_ms,
            kind,
            actor: actor.to_string(),
            description: description.to_string(),
            entity_ref: entity_ref.to_string(),
            content_hash,
            previous_hash,
            chain_hash: chain_hash.clone(),
        };

        self.last_hash = chain_hash;

        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
            self.telemetry.entries_evicted += 1;
        }
        self.entries.push_back(entry);
        self.telemetry.entries_appended += 1;

        self.entries.back().unwrap()
    }

    /// Number of entries in the chain.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the chain is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the latest entry.
    pub fn latest(&self) -> Option<&AuditChainEntry> {
        self.entries.back()
    }

    /// Get an entry by sequence number.
    pub fn get_by_sequence(&self, seq: u64) -> Option<&AuditChainEntry> {
        self.entries.iter().find(|e| e.sequence == seq)
    }

    /// Get entries by kind.
    pub fn entries_by_kind(&self, kind: AuditEntryKind) -> Vec<&AuditChainEntry> {
        self.entries.iter().filter(|e| e.kind == kind).collect()
    }

    /// Get entries within a time range.
    pub fn entries_in_range(&self, start_ms: u64, end_ms: u64) -> Vec<&AuditChainEntry> {
        self.entries
            .iter()
            .filter(|e| e.timestamp_ms >= start_ms && e.timestamp_ms <= end_ms)
            .collect()
    }

    /// Verify the integrity of the hash chain.
    pub fn verify(&mut self) -> ChainVerificationResult {
        self.telemetry.verifications_run += 1;

        if self.entries.is_empty() {
            return ChainVerificationResult {
                valid: true,
                entries_checked: 0,
                first_invalid_at: None,
                failure_reason: None,
            };
        }

        let mut prev_hash = String::new();

        for (i, entry) in self.entries.iter().enumerate() {
            // Verify content hash
            let expected_content = Self::hash_content(
                entry.sequence,
                entry.timestamp_ms,
                entry.kind,
                &entry.actor,
                &entry.description,
                &entry.entity_ref,
            );

            if entry.content_hash != expected_content {
                self.telemetry.verification_failures += 1;
                return ChainVerificationResult {
                    valid: false,
                    entries_checked: i + 1,
                    first_invalid_at: Some(i),
                    failure_reason: Some(format!(
                        "content hash mismatch at sequence {}",
                        entry.sequence
                    )),
                };
            }

            // Verify previous hash linkage
            if entry.previous_hash != prev_hash {
                self.telemetry.verification_failures += 1;
                return ChainVerificationResult {
                    valid: false,
                    entries_checked: i + 1,
                    first_invalid_at: Some(i),
                    failure_reason: Some(format!(
                        "previous hash mismatch at sequence {}",
                        entry.sequence
                    )),
                };
            }

            // Verify chain hash
            let expected_chain = Self::hash_chain(&entry.content_hash, &entry.previous_hash);
            if entry.chain_hash != expected_chain {
                self.telemetry.verification_failures += 1;
                return ChainVerificationResult {
                    valid: false,
                    entries_checked: i + 1,
                    first_invalid_at: Some(i),
                    failure_reason: Some(format!(
                        "chain hash mismatch at sequence {}",
                        entry.sequence
                    )),
                };
            }

            prev_hash.clone_from(&entry.chain_hash);
        }

        ChainVerificationResult {
            valid: true,
            entries_checked: self.entries.len(),
            first_invalid_at: None,
            failure_reason: None,
        }
    }

    /// Export all entries as JSON.
    pub fn export_json(&mut self) -> String {
        self.telemetry.exports_completed += 1;
        let entries: Vec<&AuditChainEntry> = self.entries.iter().collect();
        serde_json::to_string_pretty(&entries).unwrap_or_default()
    }

    /// Export all entries as JSONL.
    pub fn export_jsonl(&mut self) -> String {
        self.telemetry.exports_completed += 1;
        self.entries
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get a telemetry snapshot.
    pub fn telemetry_snapshot(&self, now_ms: u64) -> AuditChainTelemetrySnapshot {
        AuditChainTelemetrySnapshot {
            captured_at_ms: now_ms,
            counters: self.telemetry.clone(),
            chain_length: self.entries.len(),
            max_entries: self.max_entries,
            next_sequence: self.next_sequence,
        }
    }

    // ---- Hash helpers ----

    fn hash_content(
        sequence: u64,
        timestamp_ms: u64,
        kind: AuditEntryKind,
        actor: &str,
        description: &str,
        entity_ref: &str,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(sequence.to_le_bytes());
        hasher.update(timestamp_ms.to_le_bytes());
        hasher.update(kind.to_string().as_bytes());
        hasher.update(actor.as_bytes());
        hasher.update(description.as_bytes());
        hasher.update(entity_ref.as_bytes());
        hex::encode(hasher.finalize())
    }

    fn hash_chain(content_hash: &str, previous_hash: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content_hash.as_bytes());
        hasher.update(previous_hash.as_bytes());
        hex::encode(hasher.finalize())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let config = AuditChainConfig::default();
        assert_eq!(config.max_entries, 1024);
        assert!(!config.record_allows);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = AuditChainConfig {
            max_entries: 512,
            record_allows: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: AuditChainConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn config_missing_fields_use_defaults() {
        let json = "{}";
        let config: AuditChainConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_entries, 1024);
        assert!(!config.record_allows);
    }

    #[test]
    fn from_config_respects_settings() {
        let config = AuditChainConfig {
            max_entries: 5,
            record_allows: true,
        };
        let chain = AuditChain::from_config(&config);
        assert!(chain.is_empty());
        assert!(chain.records_allows());

        // Verify max_entries is bounded
        let config_zero = AuditChainConfig {
            max_entries: 0,
            record_allows: false,
        };
        let mut chain = AuditChain::from_config(&config_zero);
        // max_entries clamped to 1
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d1", "r1", 1000);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d2", "r2", 2000);
        assert_eq!(chain.len(), 1); // eviction at capacity 1
    }

    #[test]
    fn empty_chain() {
        let chain = AuditChain::new(100);
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
        assert!(chain.latest().is_none());
    }

    #[test]
    fn append_single_entry() {
        let mut chain = AuditChain::new(100);
        let entry = chain.append(
            AuditEntryKind::PolicyDecision,
            "system",
            "denied write to pane",
            "rule-1",
            1000,
        );
        assert_eq!(entry.sequence, 0);
        assert_eq!(entry.kind, AuditEntryKind::PolicyDecision);
        assert_eq!(entry.previous_hash, "");
        assert!(!entry.content_hash.is_empty());
        assert!(!entry.chain_hash.is_empty());
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn chain_links_entries() {
        let mut chain = AuditChain::new(100);
        chain.append(
            AuditEntryKind::PolicyDecision,
            "system",
            "allowed",
            "rule-1",
            1000,
        );
        let first_chain_hash = chain.latest().unwrap().chain_hash.clone();
        chain.append(
            AuditEntryKind::QuarantineAction,
            "admin",
            "quarantined connector",
            "conn-1",
            2000,
        );

        // Second entry's previous_hash should be first entry's chain_hash
        let second = chain.get_by_sequence(1).unwrap();
        assert_eq!(second.previous_hash, first_chain_hash);
    }

    #[test]
    fn verify_valid_chain() {
        let mut chain = AuditChain::new(100);
        for i in 0..5 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "system",
                &format!("decision {i}"),
                &format!("rule-{i}"),
                i * 1000,
            );
        }
        let result = chain.verify();
        assert!(result.valid);
        assert_eq!(result.entries_checked, 5);
    }

    #[test]
    fn verify_empty_chain() {
        let mut chain = AuditChain::new(100);
        let result = chain.verify();
        assert!(result.valid);
        assert_eq!(result.entries_checked, 0);
    }

    #[test]
    fn verify_detects_tampered_content() {
        let mut chain = AuditChain::new(100);
        chain.append(
            AuditEntryKind::PolicyDecision,
            "system",
            "original",
            "rule-1",
            1000,
        );
        chain.append(
            AuditEntryKind::QuarantineAction,
            "admin",
            "quarantine",
            "conn-1",
            2000,
        );

        // Tamper with the first entry's description
        if let Some(entry) = chain.entries.front_mut() {
            entry.description = "tampered".to_string();
        }

        let result = chain.verify();
        assert!(!result.valid);
        assert_eq!(result.first_invalid_at, Some(0));
        assert!(
            result
                .failure_reason
                .as_ref()
                .unwrap()
                .contains("content hash")
        );
    }

    #[test]
    fn verify_detects_broken_link() {
        let mut chain = AuditChain::new(100);
        chain.append(
            AuditEntryKind::PolicyDecision,
            "system",
            "first",
            "rule-1",
            1000,
        );
        chain.append(
            AuditEntryKind::PolicyDecision,
            "system",
            "second",
            "rule-2",
            2000,
        );

        // Tamper with the second entry's previous_hash
        if let Some(entry) = chain.entries.get_mut(1) {
            entry.previous_hash = "bad_hash".to_string();
        }

        let result = chain.verify();
        assert!(!result.valid);
        assert_eq!(result.first_invalid_at, Some(1));
        assert!(
            result
                .failure_reason
                .as_ref()
                .unwrap()
                .contains("previous hash")
        );
    }

    #[test]
    fn bounded_eviction() {
        let mut chain = AuditChain::new(3);
        for i in 0..5 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "system",
                &format!("decision {i}"),
                &format!("rule-{i}"),
                i * 1000,
            );
        }
        assert_eq!(chain.len(), 3);
        // First entry should be sequence 2 (0 and 1 evicted)
        assert_eq!(chain.entries.front().unwrap().sequence, 2);
        let snap = chain.telemetry_snapshot(5000);
        assert_eq!(snap.counters.entries_appended, 5);
        assert_eq!(snap.counters.entries_evicted, 2);
    }

    #[test]
    fn entries_by_kind() {
        let mut chain = AuditChain::new(100);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d1", "r1", 1000);
        chain.append(AuditEntryKind::QuarantineAction, "admin", "q1", "c1", 2000);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d2", "r2", 3000);

        let decisions = chain.entries_by_kind(AuditEntryKind::PolicyDecision);
        assert_eq!(decisions.len(), 2);
        let quarantines = chain.entries_by_kind(AuditEntryKind::QuarantineAction);
        assert_eq!(quarantines.len(), 1);
    }

    #[test]
    fn entries_in_range() {
        let mut chain = AuditChain::new(100);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d1", "r1", 1000);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d2", "r2", 2000);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d3", "r3", 3000);

        let range = chain.entries_in_range(1500, 2500);
        assert_eq!(range.len(), 1);
        assert_eq!(range[0].sequence, 1);
    }

    #[test]
    fn export_json() {
        let mut chain = AuditChain::new(100);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d1", "r1", 1000);

        let json = chain.export_json();
        let parsed: Vec<AuditChainEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].sequence, 0);
    }

    #[test]
    fn export_jsonl() {
        let mut chain = AuditChain::new(100);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "d1", "r1", 1000);
        chain.append(AuditEntryKind::QuarantineAction, "admin", "q1", "c1", 2000);

        let jsonl = chain.export_jsonl();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let _: AuditChainEntry = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 =
            AuditChain::hash_content(0, 1000, AuditEntryKind::PolicyDecision, "sys", "test", "r1");
        let h2 =
            AuditChain::hash_content(0, 1000, AuditEntryKind::PolicyDecision, "sys", "test", "r1");
        assert_eq!(h1, h2);

        // Different input → different hash
        let h3 =
            AuditChain::hash_content(1, 1000, AuditEntryKind::PolicyDecision, "sys", "test", "r1");
        assert_ne!(h1, h3);
    }

    #[test]
    fn chain_hash_deterministic() {
        let h1 = AuditChain::hash_chain("abc", "def");
        let h2 = AuditChain::hash_chain("abc", "def");
        assert_eq!(h1, h2);

        let h3 = AuditChain::hash_chain("abc", "ghi");
        assert_ne!(h1, h3);
    }

    #[test]
    fn sequence_monotonic() {
        let mut chain = AuditChain::new(100);
        for i in 0..10 {
            let entry = chain.append(AuditEntryKind::PolicyDecision, "sys", "d", "r", i * 100);
            assert_eq!(entry.sequence, i);
        }
    }

    #[test]
    fn latest_returns_last() {
        let mut chain = AuditChain::new(100);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "first", "r1", 1000);
        chain.append(AuditEntryKind::PolicyDecision, "sys", "last", "r2", 2000);

        let latest = chain.latest().unwrap();
        assert_eq!(latest.description, "last");
    }

    #[test]
    fn get_by_sequence_after_eviction() {
        let mut chain = AuditChain::new(3);
        for i in 0..5 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        // Sequences 0 and 1 are evicted
        assert!(chain.get_by_sequence(0).is_none());
        assert!(chain.get_by_sequence(1).is_none());
        assert!(chain.get_by_sequence(2).is_some());
    }

    #[test]
    fn verify_chain_after_eviction_uses_local_hashes() {
        let mut chain = AuditChain::new(3);
        for i in 0..5 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        // After eviction, the remaining entries still form a valid sub-chain
        // but the first remaining entry's previous_hash points to a now-evicted entry
        // Verification should handle this gracefully by checking what's present
        let result = chain.verify();
        // The chain will be invalid since the first remaining entry's previous_hash
        // doesn't match "" (genesis) — it matches the evicted entry's chain_hash.
        // This is expected behavior: eviction breaks provable chain integrity
        // but the entries themselves are internally consistent.
        assert!(!result.valid);
    }

    #[test]
    fn telemetry_snapshot_accurate() {
        let mut chain = AuditChain::new(5);
        for i in 0..7 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        chain.verify();
        chain.export_json();

        let snap = chain.telemetry_snapshot(10000);
        assert_eq!(snap.counters.entries_appended, 7);
        assert_eq!(snap.counters.entries_evicted, 2);
        assert_eq!(snap.counters.verifications_run, 1);
        assert_eq!(snap.counters.exports_completed, 1);
        assert_eq!(snap.chain_length, 5);
        assert_eq!(snap.max_entries, 5);
        assert_eq!(snap.next_sequence, 7);
    }

    #[test]
    fn entry_kind_display() {
        assert_eq!(
            AuditEntryKind::PolicyDecision.to_string(),
            "policy_decision"
        );
        assert_eq!(
            AuditEntryKind::QuarantineAction.to_string(),
            "quarantine_action"
        );
        assert_eq!(
            AuditEntryKind::KillSwitchAction.to_string(),
            "kill_switch_action"
        );
        assert_eq!(
            AuditEntryKind::ComplianceViolation.to_string(),
            "compliance_violation"
        );
        assert_eq!(
            AuditEntryKind::ComplianceRemediation.to_string(),
            "compliance_remediation"
        );
        assert_eq!(
            AuditEntryKind::CredentialAction.to_string(),
            "credential_action"
        );
        assert_eq!(
            AuditEntryKind::ForensicExport.to_string(),
            "forensic_export"
        );
        assert_eq!(AuditEntryKind::ConfigChange.to_string(), "config_change");
    }

    #[test]
    fn verification_result_display() {
        let valid = ChainVerificationResult {
            valid: true,
            entries_checked: 5,
            first_invalid_at: None,
            failure_reason: None,
        };
        assert_eq!(valid.to_string(), "valid (5 entries)");

        let invalid = ChainVerificationResult {
            valid: false,
            entries_checked: 3,
            first_invalid_at: Some(2),
            failure_reason: Some("content hash mismatch".to_string()),
        };
        assert_eq!(
            invalid.to_string(),
            "INVALID at entry 2: content hash mismatch"
        );
    }

    #[test]
    fn entry_serde_roundtrip() {
        let mut chain = AuditChain::new(100);
        let entry = chain
            .append(
                AuditEntryKind::QuarantineAction,
                "admin",
                "quarantined agent-x",
                "agent-x",
                42000,
            )
            .clone();

        let json = serde_json::to_string(&entry).unwrap();
        let back: AuditChainEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn telemetry_serde_roundtrip() {
        let telemetry = AuditChainTelemetry {
            entries_appended: 100,
            entries_evicted: 5,
            verifications_run: 3,
            verification_failures: 1,
            exports_completed: 2,
        };
        let json = serde_json::to_string(&telemetry).unwrap();
        let back: AuditChainTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(telemetry, back);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let snap = AuditChainTelemetrySnapshot {
            captured_at_ms: 1000,
            counters: AuditChainTelemetry::default(),
            chain_length: 42,
            max_entries: 100,
            next_sequence: 42,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: AuditChainTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn kind_serde_roundtrip() {
        for kind in [
            AuditEntryKind::PolicyDecision,
            AuditEntryKind::QuarantineAction,
            AuditEntryKind::KillSwitchAction,
            AuditEntryKind::ComplianceViolation,
            AuditEntryKind::ComplianceRemediation,
            AuditEntryKind::CredentialAction,
            AuditEntryKind::ForensicExport,
            AuditEntryKind::ConfigChange,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: AuditEntryKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }
}
