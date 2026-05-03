//! Policy audit chain — hash-linked immutable audit trail.
//!
//! Chains policy decisions, quarantine events, and compliance actions into
//! a verifiable SHA-256 hash-linked sequence. Provides tamper detection,
//! chain verification, and bounded retention with configurable export.
//!
//! Part of ft-3681t.6.4/ft-3681t.6.5 precursor work.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// br-ft-jyywz: silent-drop observability for audit-chain exports.
// export_json (unwrap_or_default) and export_jsonl
// (filter_map(.ok())) both silently absorb serde_json failures
// even though the audit chain's WHOLE POINT is tamper-evidence —
// a silently truncated export defeats the design.
//
// In practice serde_json on a Vec<&AuditChainEntry> never fails
// (the entry is composed of primitives + String + Vec<u8>), but
// the absorption pattern is a code smell and the observability
// gap means operators have no signal if it ever does. Bumps land
// on every dropped serialization plus on a fully-empty export
// from a non-empty chain (an export-disabled invariant).
static AUDIT_CHAIN_EXPORT_DROPPED_COUNT: AtomicU64 = AtomicU64::new(0);

/// Cumulative count of audit-chain entries dropped from a JSON or
/// JSONL export due to serde_json serialization failure since
/// process load. Exports always increment
/// `exports_completed` even on partial failure — this counter is
/// the operator's only signal that the export was incomplete.
#[must_use]
pub fn audit_chain_export_dropped_count() -> u64 {
    AUDIT_CHAIN_EXPORT_DROPPED_COUNT.load(Ordering::Relaxed)
}

/// Test helper: reset the counter so tests that simulate export
/// failures can assert post-increment values without state
/// leakage between tests.
#[cfg(test)]
pub fn reset_audit_chain_export_dropped_count_for_test() {
    AUDIT_CHAIN_EXPORT_DROPPED_COUNT.store(0, Ordering::Relaxed);
}

#[inline]
fn record_audit_chain_export_drop() {
    AUDIT_CHAIN_EXPORT_DROPPED_COUNT.fetch_add(1, Ordering::Relaxed);
}

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
    /// An OSC 52 clipboard write or read decision (allowed /
    /// denied / prompted) was made.
    ///
    /// br-ft-tkyr6 substrate-pass: variant added so the
    /// integration's `Osc52AuditEvent` payload (defined at
    /// `frankenterm_core::osc_protocol_omnibus`) has a
    /// dedicated kind in the chain instead of mis-classifying
    /// as `PolicyDecision`. The wired-pass cont-bead writes
    /// the event into the chain via `AuditChain::append_osc52`
    /// at every OSC 52 decision site (read + write paths in
    /// `osc_protocol_integration.rs`).
    Osc52Action,
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
            Self::Osc52Action => write!(f, "osc52_action"),
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
    /// br-ft-nucbz: tamper-evidence watermark. Sequence number of
    /// the OLDEST currently-retained entry; equals 0 before any
    /// eviction, advances by 1 per evicted entry. External
    /// auditors anchor the chain via
    /// `entries[0].sequence == first_retained_sequence`.
    #[serde(default)]
    pub first_retained_sequence: u64,
}

/// br-ft-nucbz: failure modes returned by
/// [`AuditChain::verify_chain_continuity`]. Distinguishes
/// legitimate eviction-truncated chains (anchor matches +
/// hash-links walk cleanly = `Ok`) from tamper-shaped states
/// (anchor mismatch = attacker-deleted entries; hash-link break
/// = attacker-modified entry; sequence gap = attacker-spliced
/// chain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditChainContinuityError {
    /// `entries[0].sequence` does not match
    /// `first_retained_sequence` — the chain has been truncated
    /// by something other than routine eviction (or the
    /// watermark itself was tampered with).
    AnchorMismatch { expected: u64, actual: u64 },
    /// An entry's `previous_hash` does not match its
    /// predecessor's `chain_hash` — the predecessor was
    /// modified after the entry was appended (tamper) or the
    /// entry's own hash field was overwritten.
    HashLinkBroken {
        sequence: u64,
        reason: &'static str,
    },
    /// Two adjacent entries' sequences are non-contiguous —
    /// the chain has been spliced (an entry was removed
    /// without recording the eviction in the watermark).
    SequenceGap { after: u64, before: u64 },
}

impl fmt::Display for AuditChainContinuityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnchorMismatch { expected, actual } => write!(
                f,
                "audit chain anchor mismatch: first_retained_sequence={expected} but \
                 entries[0].sequence={actual}"
            ),
            Self::HashLinkBroken { sequence, reason } => {
                write!(f, "audit chain hash-link broken at seq={sequence}: {reason}")
            }
            Self::SequenceGap { after, before } => write!(
                f,
                "audit chain sequence gap: entry seq={after} followed by entry seq={before} \
                 (expected {})",
                after.saturating_add(1)
            ),
        }
    }
}

impl std::error::Error for AuditChainContinuityError {}

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
    /// br-ft-nucbz: tamper-evidence watermark. Holds the sequence
    /// number of the OLDEST currently-retained entry. Initial 0;
    /// monotonically advances by 1 each time `append` evicts the
    /// front of the chain. An external auditor can verify
    /// `entries[0].sequence == first_retained_sequence` to anchor
    /// the chain — without the watermark, an attacker who deletes
    /// entries 0..K is indistinguishable from routine eviction.
    first_retained_sequence: u64,
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
            first_retained_sequence: 0,
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
            first_retained_sequence: 0,
        }
    }

    /// Whether this chain records Allow decisions.
    pub fn records_allows(&self) -> bool {
        self.record_allows
    }

    /// Sequence number of the oldest currently-retained entry
    /// (br-ft-nucbz). 0 before any eviction; advances by 1 per
    /// evicted entry. External auditors verifying chain
    /// continuity assert
    /// `entries[0].sequence == first_retained_sequence` to
    /// anchor the chain — without this anchor, attacker
    /// truncation is indistinguishable from routine eviction.
    pub fn first_retained_sequence(&self) -> u64 {
        self.first_retained_sequence
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
            // br-ft-nucbz: advance the tamper-evidence watermark
            // to one past the evicted sequence so an external
            // auditor can verify entries[0].sequence ==
            // first_retained_sequence after truncation.
            if let Some(evicted) = self.entries.pop_front() {
                self.first_retained_sequence = evicted.sequence.saturating_add(1);
            }
            self.telemetry.entries_evicted += 1;
        }
        self.entries.push_back(entry);
        self.telemetry.entries_appended += 1;

        self.entries.back().unwrap()
    }

    /// br-ft-nucbz: verify the chain's continuity within the
    /// retention window.
    ///
    /// Returns `Ok(())` when:
    /// - empty chain, OR
    /// - the oldest retained entry's sequence equals
    ///   `first_retained_sequence` (anchor matches), AND
    /// - every entry's `previous_hash` equals the predecessor's
    ///   `chain_hash` (or the empty string for the first
    ///   non-evicted entry IFF the chain has never evicted).
    ///
    /// Returns `Err(<reason>)` on any anchor mismatch or
    /// hash-link break — both are tamper-shapes.
    pub fn verify_chain_continuity(&self) -> Result<(), AuditChainContinuityError> {
        if self.entries.is_empty() {
            return Ok(());
        }
        let first = self.entries.front().expect("non-empty checked");
        if first.sequence != self.first_retained_sequence {
            return Err(AuditChainContinuityError::AnchorMismatch {
                expected: self.first_retained_sequence,
                actual: first.sequence,
            });
        }
        // Walk the previous_hash chain. The first retained entry's
        // previous_hash should be the empty string IFF nothing has
        // been evicted (first_retained_sequence == 0); otherwise it
        // points at the evicted predecessor's chain_hash and we
        // cannot verify it locally — the watermark is the trust
        // root for that case.
        if self.first_retained_sequence == 0 && !first.previous_hash.is_empty() {
            return Err(AuditChainContinuityError::HashLinkBroken {
                sequence: first.sequence,
                reason: "first entry of un-evicted chain has non-empty previous_hash",
            });
        }
        for window in self.entries.iter().zip(self.entries.iter().skip(1)) {
            let (prev, curr) = window;
            if curr.previous_hash != prev.chain_hash {
                return Err(AuditChainContinuityError::HashLinkBroken {
                    sequence: curr.sequence,
                    reason: "previous_hash does not match predecessor's chain_hash",
                });
            }
            if curr.sequence != prev.sequence.saturating_add(1) {
                return Err(AuditChainContinuityError::SequenceGap {
                    after: prev.sequence,
                    before: curr.sequence,
                });
            }
        }
        Ok(())
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

    /// Verify the integrity of the retained hash chain.
    ///
    /// When bounded retention has evicted older entries, the first retained entry
    /// is treated as an anchored head of the retained suffix. We can still verify
    /// its `content_hash` and `chain_hash` against its recorded `previous_hash`,
    /// and then verify that every later retained entry links to the previous
    /// retained entry's `chain_hash`. This proves the retained window is
    /// self-consistent without falsely flagging normal eviction as corruption.
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

        let mut prev_hash = self
            .entries
            .front()
            .map(|entry| entry.previous_hash.clone())
            .unwrap_or_default();

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

            // Verify previous hash linkage within the retained suffix. For the
            // first retained entry, `prev_hash` is seeded from its recorded
            // `previous_hash`, which may point to an evicted predecessor.
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
        match serde_json::to_string_pretty(&entries) {
            Ok(s) => s,
            Err(_) => {
                // br-ft-jyywz: tamper-evidence requires that a
                // silently-empty export be observable. Bump the
                // counter once per failed export so operators
                // can cross-reference exports_completed against
                // dropped count.
                record_audit_chain_export_drop();
                String::new()
            }
        }
    }

    /// Export all entries as JSONL.
    pub fn export_jsonl(&mut self) -> String {
        self.telemetry.exports_completed += 1;
        let mut serialized = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            match serde_json::to_string(entry) {
                Ok(line) => serialized.push(line),
                Err(_) => {
                    // br-ft-jyywz: per-entry serialize failure must
                    // be observable so operators can detect partial
                    // exports.
                    record_audit_chain_export_drop();
                }
            }
        }
        serialized.join("\n")
    }

    /// Get a telemetry snapshot.
    pub fn telemetry_snapshot(&self, now_ms: u64) -> AuditChainTelemetrySnapshot {
        AuditChainTelemetrySnapshot {
            captured_at_ms: now_ms,
            counters: self.telemetry.clone(),
            chain_length: self.entries.len(),
            max_entries: self.max_entries,
            next_sequence: self.next_sequence,
            first_retained_sequence: self.first_retained_sequence,
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
        // After eviction, the remaining entries still form a valid retained suffix
        // even though the first retained entry points at an evicted predecessor.
        let result = chain.verify();
        assert!(
            result.valid,
            "retained suffix should still verify: {result}"
        );
        assert_eq!(result.entries_checked, 3);
    }

    #[test]
    fn verify_chain_after_eviction_still_detects_retained_link_corruption() {
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
        // br-ft-nucbz: 2 entries evicted (seq 0 + 1), so the
        // tamper-evidence watermark advances to 2.
        assert_eq!(snap.first_retained_sequence, 2);
    }

    // ========================================================================
    // br-ft-nucbz: tamper-evidence watermark + verify_chain_continuity.
    // ========================================================================

    #[test]
    fn first_retained_sequence_starts_at_zero() {
        let chain = AuditChain::new(10);
        assert_eq!(chain.first_retained_sequence(), 0);
    }

    #[test]
    fn first_retained_sequence_unchanged_when_no_eviction() {
        let mut chain = AuditChain::new(10);
        for i in 0..5 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        // No eviction yet — watermark stays at 0.
        assert_eq!(chain.first_retained_sequence(), 0);
        // And the snapshot reflects the same value.
        assert_eq!(chain.telemetry_snapshot(0).first_retained_sequence, 0);
    }

    #[test]
    fn first_retained_sequence_advances_on_eviction() {
        let mut chain = AuditChain::new(2);
        // Fill to capacity, then append 3 more — evicts seq 0, 1, 2.
        for i in 0..5 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        // 3 entries evicted (seq 0, 1, 2), watermark = 3.
        assert_eq!(chain.first_retained_sequence(), 3);
        // Surviving entries[0].sequence == watermark (the anchor invariant).
        assert_eq!(chain.entries.front().unwrap().sequence, 3);
    }

    #[test]
    fn verify_chain_continuity_ok_on_empty_chain() {
        let chain = AuditChain::new(10);
        assert!(chain.verify_chain_continuity().is_ok());
    }

    #[test]
    fn verify_chain_continuity_ok_on_unevicted_chain() {
        let mut chain = AuditChain::new(10);
        for i in 0..5 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        assert!(chain.verify_chain_continuity().is_ok());
    }

    #[test]
    fn verify_chain_continuity_ok_on_truncated_by_eviction_chain() {
        let mut chain = AuditChain::new(3);
        for i in 0..7 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        // 4 entries evicted (seq 0..3); watermark = 4. Surviving
        // entries 4, 5, 6 — anchor matches + previous_hash links
        // walk cleanly within the retained window.
        assert!(chain.verify_chain_continuity().is_ok());
    }

    #[test]
    fn verify_chain_continuity_detects_anchor_mismatch_simulating_attacker_truncation() {
        // br-ft-nucbz: attacker scenario — entries[0] is removed
        // WITHOUT advancing the watermark. verify_chain_continuity
        // catches this.
        let mut chain = AuditChain::new(10);
        for i in 0..5 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        // Initial state is clean.
        assert!(chain.verify_chain_continuity().is_ok());
        // Simulate attacker removing entries[0] (seq=0) without
        // updating the watermark.
        chain.entries.pop_front();
        // entries[0].sequence is now 1, but
        // first_retained_sequence is still 0 — anchor mismatch.
        match chain.verify_chain_continuity() {
            Err(AuditChainContinuityError::AnchorMismatch { expected, actual }) => {
                assert_eq!(expected, 0);
                assert_eq!(actual, 1);
            }
            other => panic!("expected AnchorMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_chain_continuity_detects_sequence_gap_simulating_middle_splice() {
        // Attacker scenario: a middle entry is removed (splice).
        let mut chain = AuditChain::new(10);
        for i in 0..5 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        // Remove seq=2 (a middle entry).
        chain.entries.remove(2);
        // entries[0..2] have seq 0,1; entries[2..] now have seq 3,4
        // (was seq 3 at original index 3). Hash-link should ALSO
        // break because previous_hash of seq=3 was the chain_hash
        // of seq=2; but seq=2 is gone. The hash-link check fires
        // first.
        let err = chain.verify_chain_continuity().expect_err("tamper detected");
        match err {
            AuditChainContinuityError::HashLinkBroken { .. }
            | AuditChainContinuityError::SequenceGap { .. } => {}
            other => panic!("expected HashLinkBroken or SequenceGap, got {other:?}"),
        }
    }

    #[test]
    fn verify_chain_continuity_detects_hash_link_break_simulating_modify() {
        // Attacker scenario: an entry's content is modified after
        // append. previous_hash of the next entry no longer matches.
        let mut chain = AuditChain::new(10);
        for i in 0..3 {
            chain.append(
                AuditEntryKind::PolicyDecision,
                "sys",
                &format!("d{i}"),
                "r",
                i * 100,
            );
        }
        // Mutate entry[1]'s chain_hash directly (simulating
        // attacker who got write access to the entry without
        // recomputing the chain hash propagation).
        chain.entries[1].chain_hash = "attacker_overwritten_hash".to_string();
        match chain.verify_chain_continuity() {
            Err(AuditChainContinuityError::HashLinkBroken { sequence, .. }) => {
                // The break manifests at entry[2] (seq=2) whose
                // previous_hash points at entry[1]'s old chain_hash.
                assert_eq!(sequence, 2);
            }
            other => panic!("expected HashLinkBroken, got {other:?}"),
        }
    }

    #[test]
    fn audit_chain_continuity_error_displays() {
        let cases = [
            AuditChainContinuityError::AnchorMismatch {
                expected: 5,
                actual: 7,
            },
            AuditChainContinuityError::HashLinkBroken {
                sequence: 3,
                reason: "previous_hash does not match",
            },
            AuditChainContinuityError::SequenceGap {
                after: 4,
                before: 7,
            },
        ];
        for case in cases {
            let s = case.to_string();
            assert!(!s.is_empty());
            assert!(s.contains("audit chain"), "msg: {s}");
        }
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
        assert_eq!(AuditEntryKind::Osc52Action.to_string(), "osc52_action");
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
            first_retained_sequence: 0,
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

    // ─── br-ft-jyywz: audit-chain export drop counter ────────────────────

    fn export_drop_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn audit_chain_export_drop_counter_starts_at_zero_after_reset_ft_jyywz() {
        let _guard = export_drop_test_lock();
        super::reset_audit_chain_export_dropped_count_for_test();
        assert_eq!(super::audit_chain_export_dropped_count(), 0);
    }

    #[test]
    fn audit_chain_export_drop_counter_unchanged_on_clean_export_json_ft_jyywz() {
        // Pin the negative case: a chain whose entries DO serialize
        // cleanly must NOT bump the drop counter, even though the
        // counter is process-wide. Chains are composed of primitives
        // + String + Vec<u8>; serde_json never fails on this shape
        // in practice, so the chain is the realistic path operators
        // see and the counter must stay quiet.
        let _guard = export_drop_test_lock();
        super::reset_audit_chain_export_dropped_count_for_test();

        let mut chain = AuditChain::new(100);
        chain.append(
            AuditEntryKind::PolicyDecision,
            "system",
            "denied write",
            "rule-1",
            1000,
        );
        chain.append(
            AuditEntryKind::QuarantineAction,
            "operator",
            "pane-7 quarantined",
            "policy.quarantine",
            2000,
        );

        let json = chain.export_json();
        assert!(!json.is_empty(), "clean export must produce non-empty JSON");
        assert_eq!(
            super::audit_chain_export_dropped_count(),
            0,
            "br-ft-jyywz: clean export must not bump drop counter"
        );

        let jsonl = chain.export_jsonl();
        assert!(!jsonl.is_empty(), "clean export must produce non-empty JSONL");
        assert_eq!(
            super::audit_chain_export_dropped_count(),
            0,
            "br-ft-jyywz: clean JSONL export must not bump drop counter"
        );
    }

    #[test]
    fn audit_chain_export_jsonl_empty_chain_does_not_bump_ft_jyywz() {
        // An empty chain's export is the empty string — that is NOT
        // a drop, just a vacuous export. Pin that the counter stays
        // at zero so operators don't get false-positive alerts on
        // freshly-initialized engines.
        let _guard = export_drop_test_lock();
        super::reset_audit_chain_export_dropped_count_for_test();

        let mut chain = AuditChain::new(100);
        let json = chain.export_json();
        let jsonl = chain.export_jsonl();
        assert_eq!(json, "[]");
        assert_eq!(jsonl, "");
        assert_eq!(
            super::audit_chain_export_dropped_count(),
            0,
            "br-ft-jyywz: empty chain export must not bump drop counter"
        );
    }
}
