//! Idempotency and deduplication for mutating robot API actions (ft-3681t.4.6).
//!
//! Provides at-least-once safe mutation semantics so agent retries, network
//! failures, and orchestrator restarts never cause unintended repeated side
//! effects. Each mutating robot call carries an idempotency key; the guard
//! returns the cached outcome on duplicate submissions.
//!
//! # Architecture
//!
//! ```text
//! Agent ──► robot API ──► MutationGuard::check_or_execute()
//!                            │
//!                            ├─ New key  → execute action, record outcome
//!                            └─ Seen key → return cached MutationOutcome::Deduplicated
//! ```
//!
//! # Key types
//!
//! - [`MutationKey`]: Content-addressed key from action kind + parameters.
//! - [`MutationRecord`]: Cached outcome of a completed mutation.
//! - [`MutationGuard`]: Registry with TTL eviction and capacity bounds.
//! - [`MutationOutcome`]: Discriminates first execution from deduplicated replay.
//!
//! # Consistency with tx_idempotency
//!
//! Uses the same FNV-1a hash scheme as [`crate::tx_idempotency::IdempotencyKey`]
//! for deterministic key generation. The `rk:` prefix distinguishes robot keys
//! from `txk:` transaction keys.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

// ── Mutation Key ────────────────────────────────────────────────────────────

/// Content-addressed idempotency key for a single robot mutation.
///
/// Generated deterministically from action kind + fingerprint so that
/// replaying the same request produces the same key. Agents may also
/// supply an explicit client-generated key for cross-session stability.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MutationKey {
    /// The raw key string, format: `rk:{hash_hex}` or client-supplied.
    key: String,
    /// The action kind that produced this key (e.g., "send_text", "split_pane").
    action: String,
}

impl MutationKey {
    /// Derive a key from action kind and a fingerprint of the parameters.
    ///
    /// The fingerprint should capture all semantically significant parameters.
    /// For example, for `send_text`: `"{pane_id}|{text_hash}"`.
    #[must_use]
    pub fn derive(action: &str, fingerprint: &str) -> Self {
        let hash = fnv1a_hash(&format!("{action}|{fingerprint}"));
        Self {
            key: format!("rk:{hash:016x}"),
            action: action.to_string(),
        }
    }

    /// Create a key from an explicit client-supplied string.
    ///
    /// Use this when agents want cross-session stable keys (e.g., derived
    /// from their own task/step IDs).
    #[must_use]
    pub fn from_client(action: &str, client_key: &str) -> Self {
        Self {
            key: client_key.to_string(),
            action: action.to_string(),
        }
    }

    /// The string representation of this key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.key
    }

    /// The action kind this key belongs to.
    #[must_use]
    pub fn action(&self) -> &str {
        &self.action
    }
}

impl std::fmt::Display for MutationKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key)
    }
}

// ── Mutation Record ─────────────────────────────────────────────────────────

/// Cached outcome of a completed robot mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationRecord {
    /// The idempotency key that produced this record.
    pub key: MutationKey,
    /// Whether the original execution succeeded.
    pub success: bool,
    /// Cached response payload (JSON string).
    pub response_payload: Option<String>,
    /// Error message if the original execution failed.
    pub error_message: Option<String>,
    /// Wall-clock milliseconds the original execution took.
    pub elapsed_ms: u64,
    /// Unix timestamp (ms) when the record was created.
    pub created_at_ms: u64,
    /// How many times this key was submitted (1 = original, 2+ = deduped).
    pub submission_count: u64,
}

impl MutationRecord {
    /// Create a new record for a successful execution.
    #[must_use]
    pub fn success(key: MutationKey, response_payload: Option<String>, elapsed_ms: u64, now_ms: u64) -> Self {
        Self {
            key,
            success: true,
            response_payload,
            error_message: None,
            elapsed_ms,
            created_at_ms: now_ms,
            submission_count: 1,
        }
    }

    /// Create a new record for a failed execution.
    #[must_use]
    pub fn failure(key: MutationKey, error_message: String, elapsed_ms: u64, now_ms: u64) -> Self {
        Self {
            key,
            success: false,
            response_payload: None,
            error_message: Some(error_message),
            elapsed_ms,
            created_at_ms: now_ms,
            submission_count: 1,
        }
    }

    /// Whether this record has expired based on a TTL.
    #[must_use]
    pub fn is_expired(&self, now_ms: u64, ttl_ms: u64) -> bool {
        now_ms.saturating_sub(self.created_at_ms) > ttl_ms
    }
}

// ── Mutation Outcome ────────────────────────────────────────────────────────

/// Result of submitting a mutation through the guard.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
pub enum MutationOutcome {
    /// First execution — action was performed.
    Executed {
        /// The idempotency key used.
        key: String,
    },
    /// Duplicate submission — cached result returned.
    Deduplicated {
        /// The idempotency key that matched.
        key: String,
        /// How many times this key has now been submitted.
        submission_count: u64,
        /// The cached success status from the original execution.
        original_success: bool,
    },
}

impl MutationOutcome {
    /// Whether this was a first execution (not a dedup).
    #[must_use]
    pub fn is_first_execution(&self) -> bool {
        matches!(self, Self::Executed { .. })
    }

    /// Whether this was a deduplicated replay.
    #[must_use]
    pub fn is_deduplicated(&self) -> bool {
        matches!(self, Self::Deduplicated { .. })
    }

    /// The idempotency key string.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Executed { key } | Self::Deduplicated { key, .. } => key,
        }
    }
}

// ── Guard Configuration ─────────────────────────────────────────────────────

/// Configuration for the mutation deduplication guard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationGuardConfig {
    /// Maximum number of records to keep in memory.
    pub capacity: usize,
    /// Time-to-live for dedup records (milliseconds). Default: 1 hour.
    pub ttl_ms: u64,
    /// Whether failed mutations should be cached (allowing retry with same key).
    /// When false, a failed mutation's key is not recorded, so retrying with
    /// the same key will re-execute the action.
    pub cache_failures: bool,
}

impl Default for MutationGuardConfig {
    fn default() -> Self {
        Self {
            capacity: 10_000,
            ttl_ms: 3_600_000, // 1 hour
            cache_failures: false,
        }
    }
}

// ── Mutation Guard ──────────────────────────────────────────────────────────

/// In-memory deduplication guard for robot mutations.
///
/// Tracks recently executed mutations by idempotency key and returns
/// cached outcomes on duplicate submissions. Uses FIFO eviction when
/// the capacity limit is reached and TTL-based expiration.
///
/// Thread safety: wrap in `std::sync::Mutex` or `parking_lot::Mutex`
/// for concurrent access from multiple robot request handlers.
pub struct MutationGuard {
    config: MutationGuardConfig,
    /// Key → record mapping.
    records: HashMap<String, MutationRecord>,
    /// Insertion order for FIFO eviction.
    insertion_order: VecDeque<String>,
    /// Telemetry counters.
    telemetry: GuardTelemetry,
}

/// Telemetry snapshot for the mutation guard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GuardTelemetry {
    /// Total mutations recorded (first executions).
    pub mutations_recorded: u64,
    /// Total deduplications (cached returns).
    pub deduplications: u64,
    /// Total evictions (capacity or TTL).
    pub evictions: u64,
    /// Total failed mutations seen.
    pub failures_seen: u64,
    /// Current number of cached records.
    pub active_records: u64,
}

impl MutationGuard {
    /// Create a new guard with the given configuration.
    #[must_use]
    pub fn new(config: MutationGuardConfig) -> Self {
        Self {
            config,
            records: HashMap::new(),
            insertion_order: VecDeque::new(),
            telemetry: GuardTelemetry::default(),
        }
    }

    /// Create a guard with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(MutationGuardConfig::default())
    }

    /// Check if a mutation key has already been executed.
    ///
    /// Returns `Some(record)` if the key exists and hasn't expired,
    /// `None` if the key is new or expired.
    pub fn check(&mut self, key: &MutationKey, now_ms: u64) -> Option<&MutationRecord> {
        let key_str = key.as_str();

        // Check expiry first, remove if needed
        let expired = self
            .records
            .get(key_str)
            .is_some_and(|r| r.is_expired(now_ms, self.config.ttl_ms));

        if expired {
            self.remove_key(key_str);
            self.telemetry.evictions += 1;
            return None;
        }

        self.records.get(key_str)
    }

    /// Record a mutation outcome and return the disposition.
    ///
    /// If the key already exists (dedup hit), increments the submission count
    /// and returns `Deduplicated`. Otherwise records the new outcome and
    /// returns `Executed`.
    pub fn record(
        &mut self,
        key: MutationKey,
        success: bool,
        response_payload: Option<String>,
        error_message: Option<String>,
        elapsed_ms: u64,
        now_ms: u64,
    ) -> MutationOutcome {
        let key_str = key.as_str().to_string();

        // Check for existing non-expired record
        if let Some(existing) = self.records.get_mut(&key_str) {
            if !existing.is_expired(now_ms, self.config.ttl_ms) {
                existing.submission_count += 1;
                self.telemetry.deduplications += 1;
                return MutationOutcome::Deduplicated {
                    key: key_str,
                    submission_count: existing.submission_count,
                    original_success: existing.success,
                };
            }
            // Expired — remove and re-record
            self.remove_key(&key_str);
            self.telemetry.evictions += 1;
        }

        // Skip caching failures if configured
        if !success && !self.config.cache_failures {
            self.telemetry.failures_seen += 1;
            return MutationOutcome::Executed {
                key: key_str,
            };
        }

        if !success {
            self.telemetry.failures_seen += 1;
        }

        // Evict oldest if at capacity
        self.evict_if_full();

        // Record the new mutation
        let record = if success {
            MutationRecord::success(key, response_payload, elapsed_ms, now_ms)
        } else {
            MutationRecord::failure(
                key,
                error_message.unwrap_or_default(),
                elapsed_ms,
                now_ms,
            )
        };

        self.records.insert(key_str.clone(), record);
        self.insertion_order.push_back(key_str.clone());
        self.telemetry.mutations_recorded += 1;
        self.telemetry.active_records = self.records.len() as u64;

        MutationOutcome::Executed { key: key_str }
    }

    /// Evict all records older than the TTL.
    pub fn evict_expired(&mut self, now_ms: u64) {
        let ttl = self.config.ttl_ms;
        let before_len = self.records.len();

        self.records.retain(|_, record| !record.is_expired(now_ms, ttl));
        self.insertion_order
            .retain(|k| self.records.contains_key(k));

        let evicted = before_len - self.records.len();
        self.telemetry.evictions += evicted as u64;
        self.telemetry.active_records = self.records.len() as u64;
    }

    /// Get the current telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> &GuardTelemetry {
        &self.telemetry
    }

    /// Get the number of active records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the guard has no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Get the guard configuration.
    #[must_use]
    pub fn config(&self) -> &MutationGuardConfig {
        &self.config
    }

    /// Look up a record by key string (for diagnostics).
    #[must_use]
    pub fn get_record(&self, key_str: &str) -> Option<&MutationRecord> {
        self.records.get(key_str)
    }

    /// Remove a specific key.
    fn remove_key(&mut self, key_str: &str) {
        self.records.remove(key_str);
        self.insertion_order.retain(|k| k != key_str);
        self.telemetry.active_records = self.records.len() as u64;
    }

    /// Evict the oldest record if we're at capacity.
    fn evict_if_full(&mut self) {
        while self.records.len() >= self.config.capacity {
            if let Some(oldest_key) = self.insertion_order.pop_front() {
                self.records.remove(&oldest_key);
                self.telemetry.evictions += 1;
            } else {
                break;
            }
        }
        self.telemetry.active_records = self.records.len() as u64;
    }
}

// ── Batch Dedup ─────────────────────────────────────────────────────────────

/// Result of checking a batch of mutation keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchCheckResult {
    /// Keys that are new (not previously seen).
    pub new_keys: Vec<String>,
    /// Keys that matched existing records (deduplicated).
    pub dedup_keys: Vec<String>,
    /// Keys that matched but were expired (treated as new).
    pub expired_keys: Vec<String>,
}

impl MutationGuard {
    /// Check a batch of keys at once, classifying each.
    pub fn check_batch(&mut self, keys: &[MutationKey], now_ms: u64) -> BatchCheckResult {
        let mut result = BatchCheckResult {
            new_keys: Vec::new(),
            dedup_keys: Vec::new(),
            expired_keys: Vec::new(),
        };

        for key in keys {
            let key_str = key.as_str().to_string();
            match self.records.get(key.as_str()) {
                Some(record) if !record.is_expired(now_ms, self.config.ttl_ms) => {
                    result.dedup_keys.push(key_str);
                }
                Some(_) => {
                    // Expired
                    self.remove_key(&key_str);
                    self.telemetry.evictions += 1;
                    result.expired_keys.push(key_str);
                }
                None => {
                    result.new_keys.push(key_str);
                }
            }
        }

        result
    }
}

// ── Key Helpers ─────────────────────────────────────────────────────────────

/// Helper to derive a mutation key for `send_text` actions.
#[must_use]
pub fn send_text_key(pane_id: u64, text: &str) -> MutationKey {
    let text_hash = fnv1a_hash(text);
    MutationKey::derive("send_text", &format!("{pane_id}|{text_hash:016x}"))
}

/// Helper to derive a mutation key for `split_pane` actions.
#[must_use]
pub fn split_pane_key(pane_id: u64, direction: &str) -> MutationKey {
    MutationKey::derive("split_pane", &format!("{pane_id}|{direction}"))
}

/// Helper to derive a mutation key for `close_pane` actions.
#[must_use]
pub fn close_pane_key(pane_id: u64) -> MutationKey {
    MutationKey::derive("close_pane", &format!("{pane_id}"))
}

/// Helper to derive a mutation key for `event_annotate` actions.
#[must_use]
pub fn event_annotate_key(event_id: i64, annotation_hash: &str) -> MutationKey {
    MutationKey::derive("event_annotate", &format!("{event_id}|{annotation_hash}"))
}

/// Helper to derive a mutation key for `workflow_run` actions.
#[must_use]
pub fn workflow_run_key(workflow_id: &str, input_hash: &str) -> MutationKey {
    MutationKey::derive("workflow_run", &format!("{workflow_id}|{input_hash}"))
}

/// Helper to derive a mutation key for `agent_configure` actions.
#[must_use]
pub fn agent_configure_key(pane_id: u64, config_hash: &str) -> MutationKey {
    MutationKey::derive("agent_configure", &format!("{pane_id}|{config_hash}"))
}

// ── FNV-1a Hash ─────────────────────────────────────────────────────────────

/// FNV-1a 64-bit hash, consistent with `tx_idempotency` and `tx_plan_compiler`.
fn fnv1a_hash(data: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in data.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- MutationKey tests --

    #[test]
    fn key_derive_deterministic() {
        let k1 = MutationKey::derive("send_text", "42|abc");
        let k2 = MutationKey::derive("send_text", "42|abc");
        assert_eq!(k1, k2);
        assert!(k1.as_str().starts_with("rk:"));
    }

    #[test]
    fn key_derive_different_actions_differ() {
        let k1 = MutationKey::derive("send_text", "42|abc");
        let k2 = MutationKey::derive("split_pane", "42|abc");
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_derive_different_fingerprints_differ() {
        let k1 = MutationKey::derive("send_text", "42|abc");
        let k2 = MutationKey::derive("send_text", "42|xyz");
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_from_client_preserves_value() {
        let k = MutationKey::from_client("send_text", "my-custom-key-123");
        assert_eq!(k.as_str(), "my-custom-key-123");
        assert_eq!(k.action(), "send_text");
    }

    #[test]
    fn key_display_matches_as_str() {
        let k = MutationKey::derive("test", "data");
        assert_eq!(format!("{k}"), k.as_str());
    }

    #[test]
    fn key_serde_roundtrip() {
        let k = MutationKey::derive("send_text", "42|abc");
        let json = serde_json::to_string(&k).unwrap();
        let k2: MutationKey = serde_json::from_str(&json).unwrap();
        assert_eq!(k, k2);
    }

    // -- MutationRecord tests --

    #[test]
    fn record_success_construction() {
        let key = MutationKey::derive("send_text", "1|hello");
        let record = MutationRecord::success(key.clone(), Some("ok".into()), 5, 1000);
        assert!(record.success);
        assert_eq!(record.elapsed_ms, 5);
        assert_eq!(record.created_at_ms, 1000);
        assert_eq!(record.submission_count, 1);
        assert_eq!(record.response_payload, Some("ok".into()));
        assert!(record.error_message.is_none());
    }

    #[test]
    fn record_failure_construction() {
        let key = MutationKey::derive("send_text", "1|hello");
        let record = MutationRecord::failure(key, "pane not found".into(), 3, 2000);
        assert!(!record.success);
        assert_eq!(record.error_message.as_deref(), Some("pane not found"));
        assert!(record.response_payload.is_none());
    }

    #[test]
    fn record_expiry() {
        let key = MutationKey::derive("test", "data");
        let record = MutationRecord::success(key, None, 1, 1000);
        assert!(!record.is_expired(1500, 1000)); // 500ms < 1000ms TTL
        assert!(record.is_expired(2001, 1000));  // 1001ms > 1000ms TTL
        assert!(!record.is_expired(2000, 1000)); // exactly at boundary
    }

    #[test]
    fn record_serde_roundtrip() {
        let key = MutationKey::derive("send_text", "1|hello");
        let record = MutationRecord::success(key, Some("payload".into()), 10, 5000);
        let json = serde_json::to_string(&record).unwrap();
        let record2: MutationRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record.key, record2.key);
        assert_eq!(record.success, record2.success);
        assert_eq!(record.response_payload, record2.response_payload);
    }

    // -- MutationOutcome tests --

    #[test]
    fn outcome_executed() {
        let outcome = MutationOutcome::Executed {
            key: "rk:abc".into(),
        };
        assert!(outcome.is_first_execution());
        assert!(!outcome.is_deduplicated());
        assert_eq!(outcome.key(), "rk:abc");
    }

    #[test]
    fn outcome_deduplicated() {
        let outcome = MutationOutcome::Deduplicated {
            key: "rk:abc".into(),
            submission_count: 3,
            original_success: true,
        };
        assert!(!outcome.is_first_execution());
        assert!(outcome.is_deduplicated());
    }

    #[test]
    fn outcome_serde_roundtrip() {
        let outcome = MutationOutcome::Deduplicated {
            key: "rk:abc".into(),
            submission_count: 2,
            original_success: false,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"disposition\":\"deduplicated\""));
        let outcome2: MutationOutcome = serde_json::from_str(&json).unwrap();
        assert!(outcome2.is_deduplicated());
    }

    // -- MutationGuard tests --

    #[test]
    fn guard_new_key_returns_executed() {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("send_text", "1|hello");
        let outcome = guard.record(key, true, Some("ok".into()), None, 5, 1000);
        assert!(outcome.is_first_execution());
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn guard_duplicate_key_returns_deduplicated() {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("send_text", "1|hello");
        let _ = guard.record(key.clone(), true, Some("ok".into()), None, 5, 1000);
        let outcome = guard.record(key, true, Some("ok".into()), None, 5, 1001);
        assert!(outcome.is_deduplicated());
        if let MutationOutcome::Deduplicated { submission_count, .. } = outcome {
            assert_eq!(submission_count, 2);
        }
        assert_eq!(guard.len(), 1); // still 1 record
    }

    #[test]
    fn guard_triple_submission_count() {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("send_text", "1|hello");
        let _ = guard.record(key.clone(), true, None, None, 1, 1000);
        let _ = guard.record(key.clone(), true, None, None, 1, 1001);
        let outcome = guard.record(key, true, None, None, 1, 1002);
        if let MutationOutcome::Deduplicated { submission_count, .. } = outcome {
            assert_eq!(submission_count, 3);
        } else {
            panic!("expected Deduplicated");
        }
    }

    #[test]
    fn guard_check_returns_none_for_new_key() {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("send_text", "1|hello");
        assert!(guard.check(&key, 1000).is_none());
    }

    #[test]
    fn guard_check_returns_record_for_existing_key() {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("send_text", "1|hello");
        let _ = guard.record(key.clone(), true, Some("ok".into()), None, 5, 1000);
        let record = guard.check(&key, 1001).unwrap();
        assert!(record.success);
        assert_eq!(record.response_payload.as_deref(), Some("ok"));
    }

    #[test]
    fn guard_expired_key_treated_as_new() {
        let config = MutationGuardConfig {
            ttl_ms: 100,
            ..Default::default()
        };
        let mut guard = MutationGuard::new(config);
        let key = MutationKey::derive("send_text", "1|hello");
        let _ = guard.record(key.clone(), true, None, None, 1, 1000);
        // After TTL
        assert!(guard.check(&key, 1200).is_none());
        assert_eq!(guard.len(), 0); // evicted
    }

    #[test]
    fn guard_expired_key_re_executes() {
        let config = MutationGuardConfig {
            ttl_ms: 100,
            ..Default::default()
        };
        let mut guard = MutationGuard::new(config);
        let key = MutationKey::derive("send_text", "1|hello");
        let _ = guard.record(key.clone(), true, None, None, 1, 1000);
        // Re-record after expiry → should be Executed, not Deduplicated
        let outcome = guard.record(key, true, None, None, 1, 1200);
        assert!(outcome.is_first_execution());
    }

    #[test]
    fn guard_capacity_eviction() {
        let config = MutationGuardConfig {
            capacity: 3,
            ttl_ms: 60_000,
            cache_failures: false,
        };
        let mut guard = MutationGuard::new(config);

        for i in 0..5 {
            let key = MutationKey::derive("test", &format!("{i}"));
            let _ = guard.record(key, true, None, None, 1, 1000 + i);
        }

        // Capacity is 3, so oldest 2 should be evicted
        assert_eq!(guard.len(), 3);
        // Keys 0 and 1 should be gone
        assert!(guard.get_record(MutationKey::derive("test", "0").as_str()).is_none());
        assert!(guard.get_record(MutationKey::derive("test", "1").as_str()).is_none());
        // Keys 2, 3, 4 should remain
        assert!(guard.get_record(MutationKey::derive("test", "2").as_str()).is_some());
        assert!(guard.get_record(MutationKey::derive("test", "4").as_str()).is_some());
    }

    #[test]
    fn guard_failures_not_cached_by_default() {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("send_text", "1|fail");
        let outcome = guard.record(key.clone(), false, None, Some("error".into()), 1, 1000);
        assert!(outcome.is_first_execution());
        // Should NOT be cached
        assert_eq!(guard.len(), 0);
        // Retry with same key should re-execute
        let outcome2 = guard.record(key, true, Some("ok".into()), None, 1, 1001);
        assert!(outcome2.is_first_execution());
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn guard_failures_cached_when_configured() {
        let config = MutationGuardConfig {
            cache_failures: true,
            ..Default::default()
        };
        let mut guard = MutationGuard::new(config);
        let key = MutationKey::derive("send_text", "1|fail");
        let _ = guard.record(key.clone(), false, None, Some("error".into()), 1, 1000);
        assert_eq!(guard.len(), 1);
        // Retry with same key should be deduplicated
        let outcome = guard.record(key, true, None, None, 1, 1001);
        assert!(outcome.is_deduplicated());
        if let MutationOutcome::Deduplicated { original_success, .. } = outcome {
            assert!(!original_success); // original was failure
        }
    }

    #[test]
    fn guard_evict_expired_batch() {
        let config = MutationGuardConfig {
            ttl_ms: 100,
            ..Default::default()
        };
        let mut guard = MutationGuard::new(config);
        let _ = guard.record(MutationKey::derive("t", "1"), true, None, None, 1, 1000);
        let _ = guard.record(MutationKey::derive("t", "2"), true, None, None, 1, 1050);
        let _ = guard.record(MutationKey::derive("t", "3"), true, None, None, 1, 1200);
        assert_eq!(guard.len(), 3);

        guard.evict_expired(1150); // only "1" expired (1000 + 100 = 1100 < 1150)
        assert_eq!(guard.len(), 2);

        guard.evict_expired(1301); // "2" expired (1050 + 100 = 1150 < 1301), "3" still alive
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn guard_telemetry_tracks_counters() {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("test", "1");
        let _ = guard.record(key.clone(), true, None, None, 1, 1000);
        let _ = guard.record(key, true, None, None, 1, 1001);

        let t = guard.telemetry();
        assert_eq!(t.mutations_recorded, 1);
        assert_eq!(t.deduplications, 1);
        assert_eq!(t.active_records, 1);
    }

    #[test]
    fn guard_telemetry_tracks_failures() {
        let mut guard = MutationGuard::with_defaults();
        let key = MutationKey::derive("test", "1");
        let _ = guard.record(key, false, None, Some("err".into()), 1, 1000);
        assert_eq!(guard.telemetry().failures_seen, 1);
    }

    // -- Batch check tests --

    #[test]
    fn batch_check_classifies_keys() {
        let config = MutationGuardConfig {
            ttl_ms: 100,
            ..Default::default()
        };
        let mut guard = MutationGuard::new(config);

        let k1 = MutationKey::derive("t", "active");
        let k2 = MutationKey::derive("t", "expired");
        let k3 = MutationKey::derive("t", "new");

        let _ = guard.record(k1.clone(), true, None, None, 1, 1000);
        let _ = guard.record(k2.clone(), true, None, None, 1, 800);

        let result = guard.check_batch(&[k1, k2, k3], 1050);
        assert_eq!(result.dedup_keys.len(), 1);
        assert_eq!(result.expired_keys.len(), 1);
        assert_eq!(result.new_keys.len(), 1);
    }

    // -- Key helper tests --

    #[test]
    fn send_text_key_deterministic() {
        let k1 = send_text_key(42, "hello world");
        let k2 = send_text_key(42, "hello world");
        assert_eq!(k1, k2);
        assert_eq!(k1.action(), "send_text");
    }

    #[test]
    fn send_text_key_varies_by_pane() {
        let k1 = send_text_key(1, "hello");
        let k2 = send_text_key(2, "hello");
        assert_ne!(k1, k2);
    }

    #[test]
    fn send_text_key_varies_by_text() {
        let k1 = send_text_key(1, "hello");
        let k2 = send_text_key(1, "world");
        assert_ne!(k1, k2);
    }

    #[test]
    fn split_pane_key_works() {
        let k = split_pane_key(1, "horizontal");
        assert_eq!(k.action(), "split_pane");
    }

    #[test]
    fn close_pane_key_works() {
        let k = close_pane_key(42);
        assert_eq!(k.action(), "close_pane");
    }

    #[test]
    fn event_annotate_key_works() {
        let k = event_annotate_key(100, "abc123");
        assert_eq!(k.action(), "event_annotate");
    }

    #[test]
    fn workflow_run_key_works() {
        let k = workflow_run_key("my-wf", "input-hash");
        assert_eq!(k.action(), "workflow_run");
    }

    #[test]
    fn agent_configure_key_works() {
        let k = agent_configure_key(5, "config-hash");
        assert_eq!(k.action(), "agent_configure");
    }

    // -- FNV-1a consistency --

    #[test]
    fn fnv1a_deterministic() {
        assert_eq!(fnv1a_hash("test"), fnv1a_hash("test"));
    }

    #[test]
    fn fnv1a_different_inputs_differ() {
        assert_ne!(fnv1a_hash("test1"), fnv1a_hash("test2"));
    }

    #[test]
    fn fnv1a_empty_string() {
        // FNV-1a offset basis
        assert_eq!(fnv1a_hash(""), 0xcbf29ce484222325);
    }

    // -- Guard is_empty/len --

    #[test]
    fn guard_empty_initially() {
        let guard = MutationGuard::with_defaults();
        assert!(guard.is_empty());
        assert_eq!(guard.len(), 0);
    }

    // -- Edge case: same key, different action strings but same hash (collision) --

    #[test]
    fn guard_different_keys_independent() {
        let mut guard = MutationGuard::with_defaults();
        let k1 = MutationKey::derive("action_a", "data");
        let k2 = MutationKey::derive("action_b", "data");
        // These have different hashes (different action prefix)
        let _ = guard.record(k1.clone(), true, Some("a".into()), None, 1, 1000);
        let _ = guard.record(k2.clone(), true, Some("b".into()), None, 1, 1001);
        assert_eq!(guard.len(), 2);

        let r1 = guard.get_record(k1.as_str()).unwrap();
        assert_eq!(r1.response_payload.as_deref(), Some("a"));
        let r2 = guard.get_record(k2.as_str()).unwrap();
        assert_eq!(r2.response_payload.as_deref(), Some("b"));
    }
}
