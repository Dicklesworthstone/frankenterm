//! Generic recorder-audit hash-chain engine.
//!
//! This module owns bounded in-memory retention, ordinal assignment, and
//! hash-chain verification without depending on recorder storage or policy
//! internals from `frankenterm-core`.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Hash of the genesis entry (all zeros).
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Engine configuration for recorder audit logs.
#[derive(Debug, Clone)]
pub struct AuditLogEngineConfig {
    pub hash_chain_enabled: bool,
    pub max_memory_entries: usize,
    pub policy_version: String,
}

/// Context assigned by the engine when appending an entry.
#[derive(Debug, Clone)]
pub struct AuditAppendContext {
    pub ordinal: u64,
    pub policy_version: String,
    pub prev_entry_hash: String,
}

/// Record requirements for generic hash-chain verification.
pub trait RecorderAuditRecord {
    fn ordinal(&self) -> u64;
    fn prev_entry_hash(&self) -> &str;
    fn hash(&self) -> String;
}

/// Result of verifying the audit hash chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerification {
    /// Total entries verified.
    pub total_entries: u64,
    /// Whether the chain is intact.
    pub chain_intact: bool,
    /// First broken entry ordinal, if any.
    pub first_break_at: Option<u64>,
    /// Missing ordinals detected by gap detection.
    pub missing_ordinals: Vec<u64>,
    /// Expected ordinal range, first and last.
    pub ordinal_range: Option<(u64, u64)>,
}

struct AuditLogEngineInner<E> {
    entries: VecDeque<E>,
    next_ordinal: u64,
    last_hash: String,
    config: AuditLogEngineConfig,
    total_appended: u64,
}

/// Append-only recorder audit log engine.
pub struct AuditLogEngine<E> {
    inner: Mutex<AuditLogEngineInner<E>>,
}

impl<E> AuditLogEngine<E> {
    /// Create a new audit log engine.
    #[must_use]
    pub fn new(config: AuditLogEngineConfig) -> Self {
        Self {
            inner: Mutex::new(AuditLogEngineInner {
                entries: VecDeque::new(),
                next_ordinal: 0,
                last_hash: GENESIS_HASH.to_string(),
                config,
                total_appended: 0,
            }),
        }
    }

    /// Create a new audit log engine continuing from a known state.
    #[must_use]
    pub fn resume(config: AuditLogEngineConfig, next_ordinal: u64, last_hash: String) -> Self {
        Self {
            inner: Mutex::new(AuditLogEngineInner {
                entries: VecDeque::new(),
                next_ordinal,
                last_hash,
                config,
                total_appended: 0,
            }),
        }
    }

    /// Append a new audit entry built from the assigned context.
    pub fn append_with<F>(&self, build: F) -> E
    where
        E: Clone + RecorderAuditRecord,
        F: FnOnce(AuditAppendContext) -> E,
    {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let entry = build(AuditAppendContext {
            ordinal: inner.next_ordinal,
            policy_version: inner.config.policy_version.clone(),
            prev_entry_hash: inner.last_hash.clone(),
        });

        if inner.config.hash_chain_enabled {
            inner.last_hash = entry.hash();
        }

        inner.next_ordinal += 1;
        inner.total_appended += 1;

        if inner.entries.len() >= inner.config.max_memory_entries {
            inner.entries.pop_front();
        }

        inner.entries.push_back(entry.clone());
        entry
    }

    /// Number of in-memory entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .len()
    }

    /// Whether the log has no in-memory entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .is_empty()
    }

    /// Total entries ever appended, including entries evicted from memory.
    #[must_use]
    pub fn total_appended(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .total_appended
    }

    /// The next ordinal that will be assigned.
    #[must_use]
    pub fn next_ordinal(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .next_ordinal
    }

    /// The hash of the most recently appended entry.
    #[must_use]
    pub fn last_hash(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_hash
            .clone()
    }

    /// Return all in-memory entries.
    #[must_use]
    pub fn entries(&self) -> Vec<E>
    where
        E: Clone,
    {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.entries.iter().cloned().collect()
    }

    /// Return entries matching the given predicate.
    #[must_use]
    pub fn entries_where<F>(&self, filter: F) -> Vec<E>
    where
        E: Clone,
        F: Fn(&E) -> bool,
    {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.entries.iter().filter(|e| filter(e)).cloned().collect()
    }

    /// Drain all in-memory entries.
    pub fn drain(&self) -> Vec<E> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.entries.drain(..).collect()
    }
}

/// Verify the hash chain of the given entries.
#[must_use]
pub fn verify_chain<E: RecorderAuditRecord>(
    entries: &[E],
    expected_prev_hash: &str,
) -> ChainVerification {
    if entries.is_empty() {
        return ChainVerification {
            total_entries: 0,
            chain_intact: true,
            first_break_at: None,
            missing_ordinals: Vec::new(),
            ordinal_range: None,
        };
    }

    let mut chain_intact = true;
    let mut first_break_at = None;
    let mut missing_ordinals = Vec::new();
    let mut prev_hash = expected_prev_hash.to_string();

    let first_ordinal = entries[0].ordinal();
    let last_ordinal = entries[entries.len() - 1].ordinal();

    if entries[0].prev_entry_hash() != prev_hash && first_break_at.is_none() {
        chain_intact = false;
        first_break_at = Some(entries[0].ordinal());
    }

    prev_hash = entries[0].hash();

    for index in 1..entries.len() {
        let entry = &entries[index];
        let expected_ordinal = entries[index - 1].ordinal() + 1;
        if entry.ordinal() != expected_ordinal {
            for missing in expected_ordinal..entry.ordinal() {
                missing_ordinals.push(missing);
            }
        }

        if entry.prev_entry_hash() != prev_hash && first_break_at.is_none() {
            chain_intact = false;
            first_break_at = Some(entry.ordinal());
        }

        prev_hash = entry.hash();
    }

    ChainVerification {
        total_entries: entries.len() as u64,
        chain_intact,
        first_break_at,
        missing_ordinals,
        ordinal_range: Some((first_ordinal, last_ordinal)),
    }
}
