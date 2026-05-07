//! Evidence Ledger Generation for ARS reflexes.
//!
//! Every learned reflex must carry a verifiable proof of *why* it was
//! synthesized. The Evidence Ledger bundles:
//!
//! 1. **BOCPD Bayes Factors** — the change detection confidence
//! 2. **MDL extraction score** — minimal description length of the fix
//! 3. **Symbolic execution safety proof** — AST traversal/destruction verdict
//! 4. **Secret scan verdict** — no secrets leaked in the reflex
//! 5. **Parameter generalization bounds** — PAC-Bayesian risk bounds
//! 6. **Dynamic timeout** — expected-loss optimal timeout
//!
//! The ledger is cryptographically structured using SHA-256 hash chains,
//! making tampering detectable. This provides the data payload for the
//! "Galaxy Brain" UX (`ft why`).
//!
//! # Architecture
//!
//! ```text
//! Evidence entries → sorted by timestamp → hash-chained → LedgerDigest
//! ```

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for evidence ledger generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvidenceConfig {
    /// Minimum number of evidence entries required for a valid ledger.
    pub min_entries: usize,
    /// Maximum entries per ledger.
    pub max_entries: usize,
    /// Whether to compute hash chain (can be disabled for testing).
    pub hash_chain_enabled: bool,
    /// Required evidence categories for a complete ledger.
    pub required_categories: Vec<EvidenceCategory>,
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            min_entries: 1,
            max_entries: 100,
            hash_chain_enabled: true,
            required_categories: vec![
                EvidenceCategory::ChangeDetection,
                EvidenceCategory::SafetyProof,
            ],
        }
    }
}

// =============================================================================
// Evidence categories
// =============================================================================

/// Category of evidence in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EvidenceCategory {
    /// BOCPD change-point detection (Bayes Factor).
    ChangeDetection,
    /// MDL extraction score and compression ratio.
    MdlExtraction,
    /// Symbolic execution safety proof.
    SafetyProof,
    /// Secret/PII scan result.
    SecretScan,
    /// Parameter generalization PAC-Bayesian bounds.
    ParameterBounds,
    /// Dynamic timeout calculation.
    TimeoutCalc,
    /// Context snapshot metadata.
    ContextSnapshot,
    /// Custom evidence category.
    Custom,
}

impl fmt::Display for EvidenceCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChangeDetection => write!(f, "change_detection"),
            Self::MdlExtraction => write!(f, "mdl_extraction"),
            Self::SafetyProof => write!(f, "safety_proof"),
            Self::SecretScan => write!(f, "secret_scan"),
            Self::ParameterBounds => write!(f, "parameter_bounds"),
            Self::TimeoutCalc => write!(f, "timeout_calc"),
            Self::ContextSnapshot => write!(f, "context_snapshot"),
            Self::Custom => write!(f, "custom"),
        }
    }
}

// =============================================================================
// Evidence entry
// =============================================================================

/// A single evidence entry in the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    /// Unique sequence number within the ledger.
    pub seq: u64,
    /// Evidence category.
    pub category: EvidenceCategory,
    /// Timestamp (epoch microseconds).
    pub timestamp_us: u64,
    /// Human-readable summary of the evidence.
    pub summary: String,
    /// Structured evidence payload (JSON-compatible key-value pairs).
    pub payload: BTreeMap<String, EvidenceValue>,
    /// Verdict: did this evidence support or reject the reflex?
    pub verdict: EvidenceVerdict,
    /// SHA-256 hash of this entry (computed from category + timestamp + summary + payload).
    pub entry_hash: String,
    /// SHA-256 hash of the previous entry (chain link).
    pub prev_hash: String,
}

/// Evidence verdict — does this entry support or reject the reflex?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceVerdict {
    /// Evidence supports synthesizing the reflex.
    Support,
    /// Evidence is neutral (informational only).
    Neutral,
    /// Evidence rejects the reflex (safety violation, etc.).
    Reject,
}

/// A value in the evidence payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EvidenceValue {
    /// A string value.
    String(String),
    /// A numeric value.
    Number(f64),
    /// A boolean value.
    Bool(bool),
    /// A list of string values.
    StringList(Vec<String>),
}

impl From<&str> for EvidenceValue {
    fn from(s: &str) -> Self {
        Self::String(s.to_string())
    }
}

impl From<String> for EvidenceValue {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<f64> for EvidenceValue {
    fn from(n: f64) -> Self {
        Self::Number(n)
    }
}

impl From<bool> for EvidenceValue {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

// =============================================================================
// Ledger digest
// =============================================================================

/// Final digest of a complete evidence ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerDigest {
    /// SHA-256 hash of the final entry (root of the hash chain).
    pub root_hash: String,
    /// Total number of entries.
    pub entry_count: usize,
    /// Categories present in the ledger.
    pub categories_present: Vec<EvidenceCategory>,
    /// Whether all required categories are present.
    pub is_complete: bool,
    /// Overall verdict: all entries support, or any reject?
    pub overall_verdict: EvidenceVerdict,
    /// Timestamp range (earliest, latest) in epoch microseconds.
    pub timestamp_range: (u64, u64),
}

// =============================================================================
// Evidence Ledger
// =============================================================================

/// An immutable, hash-chained evidence ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceLedger {
    /// Ordered entries (by sequence number).
    entries: Vec<EvidenceEntry>,
    /// Configuration used to build this ledger.
    config: EvidenceConfig,
}

impl EvidenceLedger {
    /// Create a new empty ledger.
    #[must_use]
    pub fn new(config: EvidenceConfig) -> Self {
        Self {
            entries: Vec::new(),
            config,
        }
    }

    /// Create with default config.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(EvidenceConfig::default())
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ledger is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get all entries.
    #[must_use]
    pub fn entries(&self) -> &[EvidenceEntry] {
        &self.entries
    }

    /// Append an evidence entry to the ledger.
    ///
    /// Returns false if the ledger is at max capacity.
    pub fn append(
        &mut self,
        category: EvidenceCategory,
        timestamp_us: u64,
        summary: String,
        payload: BTreeMap<String, EvidenceValue>,
        verdict: EvidenceVerdict,
    ) -> bool {
        if self.entries.len() >= self.config.max_entries {
            return false;
        }

        let seq = self.entries.len() as u64;
        let prev_hash = self
            .entries
            .last()
            .map(|e| e.entry_hash.clone())
            .unwrap_or_else(|| "0".repeat(64)); // Genesis hash.

        let entry_hash = if self.config.hash_chain_enabled {
            compute_entry_hash(seq, category, timestamp_us, &summary, &payload, &prev_hash)
        } else {
            format!("unhashed-{}", seq)
        };

        self.entries.push(EvidenceEntry {
            seq,
            category,
            timestamp_us,
            summary,
            payload,
            verdict,
            entry_hash,
            prev_hash,
        });

        true
    }

    /// Verify the hash chain integrity.
    #[must_use]
    pub fn verify_chain(&self) -> ChainVerification {
        if self.entries.is_empty() {
            return ChainVerification {
                is_valid: true,
                entries_checked: 0,
                first_invalid_seq: None,
            };
        }

        if !self.config.hash_chain_enabled {
            return ChainVerification {
                is_valid: true,
                entries_checked: self.entries.len(),
                first_invalid_seq: None,
            };
        }

        let genesis = "0".repeat(64);

        for (i, entry) in self.entries.iter().enumerate() {
            // Check prev_hash links.
            let expected_prev = if i == 0 {
                &genesis
            } else {
                &self.entries[i - 1].entry_hash
            };

            if entry.prev_hash != *expected_prev {
                return ChainVerification {
                    is_valid: false,
                    entries_checked: i + 1,
                    first_invalid_seq: Some(entry.seq),
                };
            }

            // Check sequence number is monotonic.
            if entry.seq != i as u64 {
                return ChainVerification {
                    is_valid: false,
                    entries_checked: i + 1,
                    first_invalid_seq: Some(entry.seq),
                };
            }

            // Check entry hash is non-empty.
            if entry.entry_hash.is_empty() {
                return ChainVerification {
                    is_valid: false,
                    entries_checked: i + 1,
                    first_invalid_seq: Some(entry.seq),
                };
            }

            // Check that the entry hash actually matches the contents.
            let computed_hash = compute_entry_hash(
                entry.seq,
                entry.category,
                entry.timestamp_us,
                &entry.summary,
                &entry.payload,
                &entry.prev_hash,
            );
            if entry.entry_hash != computed_hash {
                return ChainVerification {
                    is_valid: false,
                    entries_checked: i + 1,
                    first_invalid_seq: Some(entry.seq),
                };
            }
        }

        ChainVerification {
            is_valid: true,
            entries_checked: self.entries.len(),
            first_invalid_seq: None,
        }
    }

    /// Compute the ledger digest.
    #[must_use]
    pub fn digest(&self) -> LedgerDigest {
        let root_hash = self
            .entries
            .last()
            .map(|e| e.entry_hash.clone())
            .unwrap_or_else(|| "0".repeat(64));

        let mut categories: Vec<EvidenceCategory> =
            self.entries.iter().map(|e| e.category).collect();
        categories.sort();
        categories.dedup();

        let is_complete = self
            .config
            .required_categories
            .iter()
            .all(|req| categories.contains(req));

        let overall_verdict = if self
            .entries
            .iter()
            .any(|e| e.verdict == EvidenceVerdict::Reject)
        {
            EvidenceVerdict::Reject
        } else if self
            .entries
            .iter()
            .all(|e| e.verdict == EvidenceVerdict::Support)
        {
            EvidenceVerdict::Support
        } else {
            EvidenceVerdict::Neutral
        };

        let ts_min = self
            .entries
            .iter()
            .map(|e| e.timestamp_us)
            .min()
            .unwrap_or(0);
        let ts_max = self
            .entries
            .iter()
            .map(|e| e.timestamp_us)
            .max()
            .unwrap_or(0);

        debug!(
            entry_count = self.entries.len(),
            is_complete,
            overall = ?overall_verdict,
            "Ledger digest computed"
        );

        LedgerDigest {
            root_hash,
            entry_count: self.entries.len(),
            categories_present: categories,
            is_complete,
            overall_verdict,
            timestamp_range: (ts_min, ts_max),
        }
    }

    /// Get entries by category.
    #[must_use]
    pub fn entries_by_category(&self, category: EvidenceCategory) -> Vec<&EvidenceEntry> {
        self.entries
            .iter()
            .filter(|e| e.category == category)
            .collect()
    }

    /// Check if a specific category is present.
    #[must_use]
    pub fn has_category(&self, category: EvidenceCategory) -> bool {
        self.entries.iter().any(|e| e.category == category)
    }

    /// Check if any entry has a Reject verdict.
    #[must_use]
    pub fn has_rejection(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.verdict == EvidenceVerdict::Reject)
    }
}

/// Result of chain verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainVerification {
    /// Whether the entire chain is valid.
    pub is_valid: bool,
    /// Number of entries checked.
    pub entries_checked: usize,
    /// First invalid entry sequence number (if any).
    pub first_invalid_seq: Option<u64>,
}

// =============================================================================
// Evidence builder (convenience)
// =============================================================================

/// Builder for creating evidence entries from ARS pipeline results.
pub struct EvidenceBuilder {
    ledger: EvidenceLedger,
}

impl EvidenceBuilder {
    /// Create a new builder with default config.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ledger: EvidenceLedger::with_defaults(),
        }
    }

    /// Create with specific config.
    #[must_use]
    pub fn with_config(config: EvidenceConfig) -> Self {
        Self {
            ledger: EvidenceLedger::new(config),
        }
    }

    /// Add a change detection evidence entry.
    pub fn add_change_detection(
        &mut self,
        timestamp_us: u64,
        bayes_factor: f64,
        run_length: u64,
        is_changepoint: bool,
    ) -> &mut Self {
        let mut payload = BTreeMap::new();
        payload.insert(
            "bayes_factor".to_string(),
            EvidenceValue::Number(bayes_factor),
        );
        payload.insert(
            "run_length".to_string(),
            EvidenceValue::Number(run_length as f64),
        );
        payload.insert(
            "is_changepoint".to_string(),
            EvidenceValue::Bool(is_changepoint),
        );

        let verdict = if is_changepoint {
            EvidenceVerdict::Support
        } else {
            EvidenceVerdict::Neutral
        };

        self.ledger.append(
            EvidenceCategory::ChangeDetection,
            timestamp_us,
            format!(
                "BOCPD change detection: BF={:.2}, run_length={}, changepoint={}",
                bayes_factor, run_length, is_changepoint
            ),
            payload,
            verdict,
        );
        self
    }

    /// Add an MDL extraction evidence entry.
    pub fn add_mdl_extraction(
        &mut self,
        timestamp_us: u64,
        compression_ratio: f64,
        commands_extracted: usize,
        confidence: f64,
    ) -> &mut Self {
        let mut payload = BTreeMap::new();
        payload.insert(
            "compression_ratio".to_string(),
            EvidenceValue::Number(compression_ratio),
        );
        payload.insert(
            "commands_extracted".to_string(),
            EvidenceValue::Number(commands_extracted as f64),
        );
        payload.insert("confidence".to_string(), EvidenceValue::Number(confidence));

        let verdict = if confidence >= 0.5 {
            EvidenceVerdict::Support
        } else {
            EvidenceVerdict::Neutral
        };

        self.ledger.append(
            EvidenceCategory::MdlExtraction,
            timestamp_us,
            format!(
                "MDL extraction: ratio={:.3}, commands={}, confidence={:.2}",
                compression_ratio, commands_extracted, confidence
            ),
            payload,
            verdict,
        );
        self
    }

    /// Add a safety proof evidence entry.
    pub fn add_safety_proof(
        &mut self,
        timestamp_us: u64,
        is_safe: bool,
        violations: Vec<String>,
    ) -> &mut Self {
        let mut payload = BTreeMap::new();
        payload.insert("is_safe".to_string(), EvidenceValue::Bool(is_safe));
        if !violations.is_empty() {
            payload.insert(
                "violations".to_string(),
                EvidenceValue::StringList(violations.clone()),
            );
        }

        let verdict = if is_safe {
            EvidenceVerdict::Support
        } else {
            EvidenceVerdict::Reject
        };

        self.ledger.append(
            EvidenceCategory::SafetyProof,
            timestamp_us,
            format!(
                "Symbolic execution safety: safe={}, violations={}",
                is_safe,
                violations.len()
            ),
            payload,
            verdict,
        );
        self
    }

    /// Add a secret scan evidence entry.
    pub fn add_secret_scan(
        &mut self,
        timestamp_us: u64,
        is_clean: bool,
        findings_count: usize,
    ) -> &mut Self {
        let mut payload = BTreeMap::new();
        payload.insert("is_clean".to_string(), EvidenceValue::Bool(is_clean));
        payload.insert(
            "findings_count".to_string(),
            EvidenceValue::Number(findings_count as f64),
        );

        let verdict = if is_clean {
            EvidenceVerdict::Support
        } else {
            EvidenceVerdict::Reject
        };

        self.ledger.append(
            EvidenceCategory::SecretScan,
            timestamp_us,
            format!(
                "Secret scan: clean={}, findings={}",
                is_clean, findings_count
            ),
            payload,
            verdict,
        );
        self
    }

    /// Add parameter bounds evidence.
    pub fn add_parameter_bounds(
        &mut self,
        timestamp_us: u64,
        risk_bound: f64,
        is_trustworthy: bool,
        params_count: usize,
    ) -> &mut Self {
        let mut payload = BTreeMap::new();
        payload.insert("risk_bound".to_string(), EvidenceValue::Number(risk_bound));
        payload.insert(
            "is_trustworthy".to_string(),
            EvidenceValue::Bool(is_trustworthy),
        );
        payload.insert(
            "params_count".to_string(),
            EvidenceValue::Number(params_count as f64),
        );

        let verdict = if is_trustworthy {
            EvidenceVerdict::Support
        } else {
            EvidenceVerdict::Neutral
        };

        self.ledger.append(
            EvidenceCategory::ParameterBounds,
            timestamp_us,
            format!(
                "PAC-Bayesian bounds: risk={:.3}, trustworthy={}, params={}",
                risk_bound, is_trustworthy, params_count
            ),
            payload,
            verdict,
        );
        self
    }

    /// Add timeout calculation evidence.
    pub fn add_timeout_calc(
        &mut self,
        timestamp_us: u64,
        timeout_ms: u64,
        expected_loss: f64,
        is_data_driven: bool,
    ) -> &mut Self {
        let mut payload = BTreeMap::new();
        payload.insert(
            "timeout_ms".to_string(),
            EvidenceValue::Number(timeout_ms as f64),
        );
        payload.insert(
            "expected_loss".to_string(),
            EvidenceValue::Number(expected_loss),
        );
        payload.insert(
            "is_data_driven".to_string(),
            EvidenceValue::Bool(is_data_driven),
        );

        self.ledger.append(
            EvidenceCategory::TimeoutCalc,
            timestamp_us,
            format!(
                "Dynamic timeout: {}ms, loss={:.3}",
                timeout_ms, expected_loss
            ),
            payload,
            EvidenceVerdict::Support,
        );
        self
    }

    /// Build and return the finalized ledger.
    #[must_use]
    pub fn build(self) -> EvidenceLedger {
        self.ledger
    }
}

impl Default for EvidenceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Hash computation
// =============================================================================

/// br-ft-dl088: Compute the SHA-256 entry hash for an evidence
/// ledger entry. Pre-fix this function used a 4-round FNV-1a
/// expansion that produced 64 hex chars (visual SHA-256 lookalike)
/// but had ZERO cryptographic strength — the file's module doc
/// (line 13-14) advertised "cryptographically structured using
/// SHA-256 hash chains, making tampering detectable", but the
/// actual implementation was a non-cryptographic checksum.
/// Operators relying on the ledger for tamper-detection got false
/// assurance.
///
/// Now: real SHA-256 over a deterministic byte stream. The byte
/// stream uses length-prefixed concatenation of every field so
/// distinct field values can never collide via boundary
/// ambiguity (e.g., (summary="ab", prev="cd") vs
/// (summary="abc", prev="d")). Output is the canonical
/// 64-character lowercase-hex SHA-256 digest.
fn compute_entry_hash(
    seq: u64,
    category: EvidenceCategory,
    timestamp_us: u64,
    summary: &str,
    payload: &BTreeMap<String, EvidenceValue>,
    prev_hash: &str,
) -> String {
    let mut hasher = Sha256::new();

    // br-ft-dl088: length-prefixed mix to prevent boundary-collision
    // attacks. Every variable-length input is preceded by an 8-byte
    // little-endian length so two distinct field assignments cannot
    // produce the same canonical byte stream (e.g., (a="xy", b="z")
    // vs (a="x", b="yz")). Fixed-length inputs (seq, timestamp_us,
    // category) are mixed without a length prefix since they have
    // no parsing ambiguity.
    let mix_field = |hasher: &mut Sha256, bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };

    hasher.update(seq.to_le_bytes());
    let category_str = format!("{}", category);
    mix_field(&mut hasher, category_str.as_bytes());
    hasher.update(timestamp_us.to_le_bytes());
    mix_field(&mut hasher, summary.as_bytes());

    // Hash payload using canonical JSON serialization for
    // determinism across serde roundtrips (avoids f64 precision
    // issues with raw bytes). BTreeMap ordering already gives us
    // a canonical key sequence.
    let payload_json = serde_json::to_string(payload).unwrap_or_default();
    mix_field(&mut hasher, payload_json.as_bytes());

    mix_field(&mut hasher, prev_hash.as_bytes());

    // SHA-256 produces a 32-byte digest → 64 lowercase hex chars,
    // matching the documented `entry_hash` shape (line 120 of this
    // file) AND the historical 4-round FNV output length (so
    // existing entry_hash field consumers don't need a width
    // change). Pre-fix the FNV output also matched this width but
    // had zero cryptographic strength.
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in &digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{:02x}", byte);
    }
    hex
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn now_us() -> u64 {
        1_000_000
    }

    // =========================================================================
    // EvidenceCategory tests
    // =========================================================================

    #[test]
    fn category_display() {
        assert_eq!(
            format!("{}", EvidenceCategory::ChangeDetection),
            "change_detection"
        );
        assert_eq!(format!("{}", EvidenceCategory::SafetyProof), "safety_proof");
    }

    #[test]
    fn category_serde_roundtrip() {
        let cats = [
            EvidenceCategory::ChangeDetection,
            EvidenceCategory::MdlExtraction,
            EvidenceCategory::SafetyProof,
            EvidenceCategory::SecretScan,
            EvidenceCategory::ParameterBounds,
            EvidenceCategory::TimeoutCalc,
            EvidenceCategory::ContextSnapshot,
            EvidenceCategory::Custom,
        ];
        for cat in &cats {
            let json = serde_json::to_string(cat).unwrap();
            let decoded: EvidenceCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(&decoded, cat);
        }
    }

    // =========================================================================
    // EvidenceValue tests
    // =========================================================================

    #[test]
    fn evidence_value_from_str() {
        let v: EvidenceValue = "hello".into();
        assert_eq!(v, EvidenceValue::String("hello".to_string()));
    }

    #[test]
    fn evidence_value_from_f64() {
        let v: EvidenceValue = std::f64::consts::PI.into();
        assert_eq!(v, EvidenceValue::Number(std::f64::consts::PI));
    }

    #[test]
    fn evidence_value_from_bool() {
        let v: EvidenceValue = true.into();
        assert_eq!(v, EvidenceValue::Bool(true));
    }

    #[test]
    fn evidence_value_serde_roundtrip() {
        let values = [
            EvidenceValue::String("test".to_string()),
            EvidenceValue::Number(42.0),
            EvidenceValue::Bool(false),
            EvidenceValue::StringList(vec!["a".to_string(), "b".to_string()]),
        ];
        for v in &values {
            let json = serde_json::to_string(v).unwrap();
            let decoded: EvidenceValue = serde_json::from_str(&json).unwrap();
            assert_eq!(&decoded, v);
        }
    }

    // =========================================================================
    // EvidenceLedger tests
    // =========================================================================

    #[test]
    fn ledger_starts_empty() {
        let ledger = EvidenceLedger::with_defaults();
        assert!(ledger.is_empty());
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn ledger_append_increments_count() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            now_us(),
            "test".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn ledger_append_respects_max() {
        let mut ledger = EvidenceLedger::new(EvidenceConfig {
            max_entries: 2,
            ..Default::default()
        });
        assert!(ledger.append(
            EvidenceCategory::ChangeDetection,
            1,
            "a".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        ));
        assert!(ledger.append(
            EvidenceCategory::SafetyProof,
            2,
            "b".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        ));
        assert!(!ledger.append(
            EvidenceCategory::SecretScan,
            3,
            "c".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        ));
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn ledger_hash_chain_valid() {
        let mut ledger = EvidenceLedger::with_defaults();
        for i in 0..5 {
            ledger.append(
                EvidenceCategory::ChangeDetection,
                i * 1000,
                format!("entry {}", i),
                BTreeMap::new(),
                EvidenceVerdict::Support,
            );
        }
        let verification = ledger.verify_chain();
        assert!(verification.is_valid);
        assert_eq!(verification.entries_checked, 5);
    }

    #[test]
    fn ledger_tampered_link_detected() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "entry 0".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        ledger.append(
            EvidenceCategory::SafetyProof,
            2000,
            "entry 1".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );

        // Tamper with entry 1's prev_hash (break the chain link).
        ledger.entries[1].prev_hash = "tampered".to_string();

        let verification = ledger.verify_chain();
        assert!(!verification.is_valid);
    }

    #[test]
    fn ledger_tampered_seq_detected() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "entry 0".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        ledger.append(
            EvidenceCategory::SafetyProof,
            2000,
            "entry 1".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );

        // Tamper with entry 1's sequence number.
        ledger.entries[1].seq = 99;

        let verification = ledger.verify_chain();
        assert!(!verification.is_valid);
    }

    #[test]
    fn ledger_tampered_payload_detected() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "entry 0".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );

        // Tamper with entry 0's payload without updating the hash.
        ledger.entries[0]
            .payload
            .insert("tampered".to_string(), EvidenceValue::Bool(true));

        let verification = ledger.verify_chain();
        assert!(!verification.is_valid);
    }

    #[test]
    fn ledger_entries_by_category() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "cd1".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        ledger.append(
            EvidenceCategory::SafetyProof,
            2000,
            "sp1".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        ledger.append(
            EvidenceCategory::ChangeDetection,
            3000,
            "cd2".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );

        let cd_entries = ledger.entries_by_category(EvidenceCategory::ChangeDetection);
        assert_eq!(cd_entries.len(), 2);
    }

    #[test]
    fn ledger_has_category() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "test".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        assert!(ledger.has_category(EvidenceCategory::ChangeDetection));
        assert!(!ledger.has_category(EvidenceCategory::SafetyProof));
    }

    #[test]
    fn ledger_has_rejection() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::SafetyProof,
            1000,
            "unsafe".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Reject,
        );
        assert!(ledger.has_rejection());
    }

    #[test]
    fn ledger_no_rejection() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "safe".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        assert!(!ledger.has_rejection());
    }

    // =========================================================================
    // LedgerDigest tests
    // =========================================================================

    #[test]
    fn digest_empty_ledger() {
        let ledger = EvidenceLedger::with_defaults();
        let digest = ledger.digest();
        assert_eq!(digest.entry_count, 0);
        assert!(!digest.is_complete);
    }

    #[test]
    fn digest_complete_when_required_present() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "cd".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        ledger.append(
            EvidenceCategory::SafetyProof,
            2000,
            "sp".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        let digest = ledger.digest();
        assert!(digest.is_complete);
    }

    #[test]
    fn digest_incomplete_when_missing_required() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "cd".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        // Missing SafetyProof.
        let digest = ledger.digest();
        assert!(!digest.is_complete);
    }

    #[test]
    fn digest_overall_reject_if_any_reject() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "good".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        ledger.append(
            EvidenceCategory::SafetyProof,
            2000,
            "bad".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Reject,
        );
        let digest = ledger.digest();
        assert_eq!(digest.overall_verdict, EvidenceVerdict::Reject);
    }

    #[test]
    fn digest_overall_support_if_all_support() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "good".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        ledger.append(
            EvidenceCategory::SafetyProof,
            2000,
            "good".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        let digest = ledger.digest();
        assert_eq!(digest.overall_verdict, EvidenceVerdict::Support);
    }

    #[test]
    fn digest_timestamp_range() {
        let mut ledger = EvidenceLedger::with_defaults();
        ledger.append(
            EvidenceCategory::ChangeDetection,
            5000,
            "a".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        ledger.append(
            EvidenceCategory::SafetyProof,
            1000,
            "b".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        ledger.append(
            EvidenceCategory::SecretScan,
            9000,
            "c".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        let digest = ledger.digest();
        assert_eq!(digest.timestamp_range, (1000, 9000));
    }

    // =========================================================================
    // EvidenceBuilder tests
    // =========================================================================

    #[test]
    fn builder_creates_complete_ledger() {
        let mut builder = EvidenceBuilder::new();
        builder
            .add_change_detection(1000, 10.0, 5, true)
            .add_safety_proof(2000, true, vec![])
            .add_secret_scan(3000, true, 0)
            .add_parameter_bounds(4000, 0.1, true, 2)
            .add_timeout_calc(5000, 5000, 2.3, true);

        let ledger = builder.build();
        assert_eq!(ledger.len(), 5);
        assert!(ledger.verify_chain().is_valid);
        let digest = ledger.digest();
        assert!(digest.is_complete);
        assert_eq!(digest.overall_verdict, EvidenceVerdict::Support);
    }

    #[test]
    fn builder_safety_reject_propagates() {
        let mut builder = EvidenceBuilder::new();
        builder
            .add_change_detection(1000, 5.0, 2, true)
            .add_safety_proof(2000, false, vec!["PathTraversal".to_string()]);

        let ledger = builder.build();
        assert!(ledger.has_rejection());
        let digest = ledger.digest();
        assert_eq!(digest.overall_verdict, EvidenceVerdict::Reject);
    }

    #[test]
    fn builder_secret_reject_propagates() {
        let mut builder = EvidenceBuilder::new();
        builder
            .add_change_detection(1000, 5.0, 2, true)
            .add_secret_scan(2000, false, 3);

        let ledger = builder.build();
        assert!(ledger.has_rejection());
    }

    // =========================================================================
    // Hash determinism tests
    // =========================================================================

    #[test]
    fn hash_is_deterministic() {
        let payload = BTreeMap::new();
        let h1 = compute_entry_hash(
            0,
            EvidenceCategory::ChangeDetection,
            1000,
            "test",
            &payload,
            "prev",
        );
        let h2 = compute_entry_hash(
            0,
            EvidenceCategory::ChangeDetection,
            1000,
            "test",
            &payload,
            "prev",
        );
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_changes_with_input() {
        let payload = BTreeMap::new();
        let h1 = compute_entry_hash(
            0,
            EvidenceCategory::ChangeDetection,
            1000,
            "test",
            &payload,
            "prev",
        );
        let h2 = compute_entry_hash(
            1,
            EvidenceCategory::ChangeDetection,
            1000,
            "test",
            &payload,
            "prev",
        );
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_length_is_64() {
        let h = compute_entry_hash(
            0,
            EvidenceCategory::ChangeDetection,
            1000,
            "test",
            &BTreeMap::new(),
            "prev",
        );
        assert_eq!(h.len(), 64);
    }

    // =========================================================================
    // Config tests
    // =========================================================================

    #[test]
    fn config_default_values() {
        let config = EvidenceConfig::default();
        assert_eq!(config.min_entries, 1);
        assert_eq!(config.max_entries, 100);
        assert!(config.hash_chain_enabled);
        assert_eq!(config.required_categories.len(), 2);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = EvidenceConfig {
            min_entries: 3,
            max_entries: 50,
            hash_chain_enabled: false,
            required_categories: vec![EvidenceCategory::MdlExtraction],
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: EvidenceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.min_entries, 3);
        assert_eq!(decoded.max_entries, 50);
        assert!(!decoded.hash_chain_enabled);
    }

    // =========================================================================
    // Ledger serde tests
    // =========================================================================

    #[test]
    fn ledger_serde_roundtrip() {
        let mut builder = EvidenceBuilder::new();
        builder
            .add_change_detection(1000, 10.0, 5, true)
            .add_safety_proof(2000, true, vec![]);
        let ledger = builder.build();

        let json = serde_json::to_string(&ledger).unwrap();
        let decoded: EvidenceLedger = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.len(), ledger.len());
        assert!(decoded.verify_chain().is_valid);
    }

    #[test]
    fn digest_serde_roundtrip() {
        let mut builder = EvidenceBuilder::new();
        builder
            .add_change_detection(1000, 10.0, 5, true)
            .add_safety_proof(2000, true, vec![]);
        let digest = builder.build().digest();

        let json = serde_json::to_string(&digest).unwrap();
        let decoded: LedgerDigest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.entry_count, digest.entry_count);
        assert_eq!(decoded.is_complete, digest.is_complete);
    }

    // =========================================================================
    // ChainVerification serde
    // =========================================================================

    #[test]
    fn chain_verification_serde_roundtrip() {
        let cv = ChainVerification {
            is_valid: true,
            entries_checked: 5,
            first_invalid_seq: None,
        };
        let json = serde_json::to_string(&cv).unwrap();
        let decoded: ChainVerification = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.is_valid, true);
        assert_eq!(decoded.entries_checked, 5);
    }

    // =========================================================================
    // Edge case: disabled hash chain
    // =========================================================================

    #[test]
    fn disabled_hash_chain_still_verifies() {
        let mut ledger = EvidenceLedger::new(EvidenceConfig {
            hash_chain_enabled: false,
            ..Default::default()
        });
        ledger.append(
            EvidenceCategory::ChangeDetection,
            1000,
            "test".to_string(),
            BTreeMap::new(),
            EvidenceVerdict::Support,
        );
        let verification = ledger.verify_chain();
        assert!(verification.is_valid);
    }

    // =========================================================================
    // br-ft-dl088: real SHA-256 entry hash (was a 4-round FNV-1a lookalike)
    // =========================================================================

    /// br-ft-dl088: the entry hash MUST be a real SHA-256 digest,
    /// not the pre-fix 4-round FNV-1a lookalike. Pin both the
    /// length (64 lowercase hex chars = 32-byte SHA-256 digest)
    /// and a known-answer test against `Sha256::digest` of the
    /// same canonical byte stream so a future refactor that
    /// regresses to a non-cryptographic hash trips here loudly.
    #[test]
    fn entry_hash_is_real_sha256_known_answer_ft_dl088() {
        // Compute the entry hash via the production helper.
        let computed = compute_entry_hash(
            42,
            EvidenceCategory::ChangeDetection,
            1_700_000_000_000_000,
            "ft-dl088 known-answer test",
            &BTreeMap::new(),
            "prev-hash-anchor",
        );

        // Canonical SHA-256 produces a 32-byte digest → 64 hex
        // characters lowercase. Pre-fix the 4-round FNV produced
        // the same width (visual lookalike) but with no
        // cryptographic strength — width alone isn't proof, so we
        // also recompute the expected digest from the same
        // canonical byte stream and assert byte-equality.
        assert_eq!(
            computed.len(),
            64,
            "ft-dl088: SHA-256 hex digest must be exactly 64 chars"
        );
        assert!(
            computed
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "ft-dl088: digest must be lowercase hex; got `{computed}`"
        );

        // Recompute via canonical byte-stream + Sha256::digest as
        // an external known-answer baseline. The byte stream MUST
        // match `compute_entry_hash`'s canonical-form construction
        // exactly (length-prefixed mix for variable-length fields).
        let mut hasher = Sha256::new();
        hasher.update(42_u64.to_le_bytes());
        let category_str = format!("{}", EvidenceCategory::ChangeDetection);
        hasher.update((category_str.as_bytes().len() as u64).to_le_bytes());
        hasher.update(category_str.as_bytes());
        hasher.update(1_700_000_000_000_000_u64.to_le_bytes());
        let summary = "ft-dl088 known-answer test";
        hasher.update((summary.as_bytes().len() as u64).to_le_bytes());
        hasher.update(summary.as_bytes());
        let payload_json = "{}";
        hasher.update((payload_json.as_bytes().len() as u64).to_le_bytes());
        hasher.update(payload_json.as_bytes());
        let prev = "prev-hash-anchor";
        hasher.update((prev.as_bytes().len() as u64).to_le_bytes());
        hasher.update(prev.as_bytes());
        let expected_digest = hasher.finalize();
        let mut expected_hex = String::with_capacity(64);
        for byte in expected_digest.iter() {
            use std::fmt::Write as _;
            let _ = write!(&mut expected_hex, "{:02x}", byte);
        }

        assert_eq!(
            computed, expected_hex,
            "ft-dl088: entry hash must equal SHA-256 of the canonical byte stream — \
             a future regression to FNV-1a (or any non-cryptographic hash) trips here"
        );
    }

    /// br-ft-dl088: SHA-256 cryptographic property — distinct
    /// inputs MUST produce distinct outputs (modulo astronomical
    /// collision probability). Pre-fix the FNV-1a expansion was a
    /// deterministic permutation of a single 64-bit hash; modifying
    /// any field perturbed the output, but the entire 256-bit space
    /// was structurally limited to ~2^64 distinct outputs (one per
    /// distinct FNV state). Real SHA-256 has the full 2^256
    /// preimage-resistance budget. This test proves that varying
    /// EACH input field changes the digest.
    #[test]
    fn entry_hash_changes_when_any_field_changes_ft_dl088() {
        let baseline = compute_entry_hash(
            1,
            EvidenceCategory::ChangeDetection,
            1000,
            "summary",
            &BTreeMap::new(),
            "prev",
        );
        // Vary `seq` — must produce distinct hash.
        let vary_seq = compute_entry_hash(
            2,
            EvidenceCategory::ChangeDetection,
            1000,
            "summary",
            &BTreeMap::new(),
            "prev",
        );
        assert_ne!(baseline, vary_seq, "ft-dl088: varying seq must change hash");

        // Vary `category`.
        let vary_category = compute_entry_hash(
            1,
            EvidenceCategory::SafetyProof,
            1000,
            "summary",
            &BTreeMap::new(),
            "prev",
        );
        assert_ne!(
            baseline, vary_category,
            "ft-dl088: varying category must change hash"
        );

        // Vary `timestamp_us`.
        let vary_ts = compute_entry_hash(
            1,
            EvidenceCategory::ChangeDetection,
            2000,
            "summary",
            &BTreeMap::new(),
            "prev",
        );
        assert_ne!(
            baseline, vary_ts,
            "ft-dl088: varying timestamp must change hash"
        );

        // Vary `summary`.
        let vary_summary = compute_entry_hash(
            1,
            EvidenceCategory::ChangeDetection,
            1000,
            "different",
            &BTreeMap::new(),
            "prev",
        );
        assert_ne!(
            baseline, vary_summary,
            "ft-dl088: varying summary must change hash"
        );

        // Vary `prev_hash` (chain-tampering vector).
        let vary_prev = compute_entry_hash(
            1,
            EvidenceCategory::ChangeDetection,
            1000,
            "summary",
            &BTreeMap::new(),
            "tampered-prev",
        );
        assert_ne!(
            baseline, vary_prev,
            "ft-dl088: varying prev_hash must change hash (chain-tamper detection)"
        );
    }

    /// br-ft-dl088: length-prefix boundary collision resistance.
    /// Pre-fix the FNV-1a mix concatenated raw bytes with no
    /// length prefix, so distinct field assignments could collide
    /// via boundary ambiguity. Concrete example pinned here:
    /// (summary="ab", prev="cd") vs (summary="abc", prev="d")
    /// MUST produce distinct hashes (which they do post-fix
    /// because the length-prefixed mix encodes each field's
    /// length explicitly).
    #[test]
    fn entry_hash_resists_boundary_collision_ft_dl088() {
        let h1 = compute_entry_hash(
            1,
            EvidenceCategory::ChangeDetection,
            1000,
            "ab",
            &BTreeMap::new(),
            "cd",
        );
        let h2 = compute_entry_hash(
            1,
            EvidenceCategory::ChangeDetection,
            1000,
            "abc",
            &BTreeMap::new(),
            "d",
        );
        assert_ne!(
            h1, h2,
            "ft-dl088: length-prefixed mix must prevent boundary collision \
             between (summary=`ab`, prev=`cd`) and (summary=`abc`, prev=`d`)"
        );
    }

    /// br-ft-dl088: SHA-256 KAT for the empty payload + zero seq +
    /// empty strings case. Pins the canonical-form byte stream
    /// shape against any future regression that re-orders or
    /// drops a length prefix. Computing the expected hash via an
    /// inline construction (rather than hardcoded hex) keeps the
    /// test maintainable when the canonical form intentionally
    /// changes — the test will then fail at the inline expected,
    /// not at a hardcoded literal.
    #[test]
    fn entry_hash_canonical_form_pinned_ft_dl088() {
        let computed = compute_entry_hash(
            0,
            EvidenceCategory::ChangeDetection,
            0,
            "",
            &BTreeMap::new(),
            "",
        );

        let mut hasher = Sha256::new();
        hasher.update(0_u64.to_le_bytes());
        let category_str = format!("{}", EvidenceCategory::ChangeDetection);
        hasher.update((category_str.as_bytes().len() as u64).to_le_bytes());
        hasher.update(category_str.as_bytes());
        hasher.update(0_u64.to_le_bytes());
        hasher.update(0_u64.to_le_bytes()); // empty summary len
        hasher.update(b""); // empty summary
        let payload_json = "{}";
        hasher.update((payload_json.as_bytes().len() as u64).to_le_bytes());
        hasher.update(payload_json.as_bytes());
        hasher.update(0_u64.to_le_bytes()); // empty prev len
        hasher.update(b""); // empty prev
        let expected = hasher.finalize();
        let mut expected_hex = String::with_capacity(64);
        for byte in expected.iter() {
            use std::fmt::Write as _;
            let _ = write!(&mut expected_hex, "{:02x}", byte);
        }

        assert_eq!(
            computed, expected_hex,
            "ft-dl088: empty-input canonical form must match the documented byte stream"
        );
    }
}
