//! Write-ahead log engine for continuous zero-cost state persistence.
//!
//! Provides an append-only log with monotonic sequence numbers, O(1) checkpoint
//! markers, background compaction, and deterministic replay. State mutations are
//! recorded as structured entries; "snapshots" reduce to placing a checkpoint
//! marker (zero cost) rather than serializing full state.
//!
//! # Design
//!
//! ```text
//! Mutations ──→ WAL (append-only) ──→ [Checkpoint│Entry│Entry│Checkpoint│...]
//!                    │
//!       ┌────────────┼────────────┐
//!       │            │            │
//!  Checkpoint    Compaction    Replay
//!  O(1) cost    (background)  (restore)
//! ```
//!
//! # Use cases in FrankenTerm
//!
//! - **Crash recovery**: Replay WAL entries since last checkpoint to restore state.
//! - **Continuous persistence**: Every state change is logged; no periodic "save".
//! - **Audit trail**: WAL entries form an immutable record of all mutations.
//! - **Time-travel**: Replay entries up to a specific sequence to reconstruct
//!   historical state.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

// ── WAL Entry ────────────────────────────────────────────────────

/// A single entry in the write-ahead log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalEntry<T: Clone> {
    /// Monotonically increasing sequence number.
    pub seq: u64,
    /// Timestamp in milliseconds since epoch.
    pub timestamp_ms: u64,
    /// The payload.
    pub kind: EntryKind<T>,
}

/// The kind of WAL entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind<T: Clone> {
    /// A state mutation record.
    Mutation(T),
    /// A checkpoint marker (zero-cost snapshot point).
    Checkpoint,
    /// A compaction marker indicating entries before this seq were compacted.
    CompactionMarker { compacted_through: u64 },
}

// ── WAL Engine ───────────────────────────────────────────────────

/// Configuration for the WAL engine.
#[derive(Debug, Clone)]
pub struct WalConfig {
    /// Maximum number of entries before suggesting compaction.
    pub compaction_threshold: usize,
    /// Maximum entries to retain after compaction (keeps most recent).
    pub max_retained_entries: usize,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            compaction_threshold: 10_000,
            max_retained_entries: 1_000,
        }
    }
}

/// An in-memory write-ahead log engine.
///
/// Entries are stored in a `VecDeque` for efficient append and front-removal
/// during compaction. Sequence numbers are globally unique and monotonically
/// increasing.
#[derive(Debug, Clone)]
pub struct WalEngine<T: Clone> {
    /// The log entries.
    entries: VecDeque<WalEntry<T>>,
    /// Next sequence number to assign.
    next_seq: u64,
    /// Sequence of the last checkpoint.
    last_checkpoint_seq: Option<u64>,
    /// Sequence through which compaction has occurred.
    compacted_through: u64,
    /// Configuration.
    config: WalConfig,
}

impl<T: Clone> WalEngine<T> {
    /// Create a new empty WAL engine.
    pub fn new(config: WalConfig) -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 1,
            last_checkpoint_seq: None,
            compacted_through: 0,
            config,
        }
    }

    /// Append a mutation entry to the log.
    ///
    /// Returns the assigned sequence number.
    pub fn append(&mut self, mutation: T, timestamp_ms: u64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(WalEntry {
            seq,
            timestamp_ms,
            kind: EntryKind::Mutation(mutation),
        });
        seq
    }

    /// Place a checkpoint marker (O(1) "snapshot").
    ///
    /// Returns the checkpoint's sequence number.
    pub fn checkpoint(&mut self, timestamp_ms: u64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(WalEntry {
            seq,
            timestamp_ms,
            kind: EntryKind::Checkpoint,
        });
        self.last_checkpoint_seq = Some(seq);
        seq
    }

    /// Get the sequence number of the last checkpoint.
    pub fn last_checkpoint(&self) -> Option<u64> {
        self.last_checkpoint_seq
    }

    /// Get the next sequence number that will be assigned.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Total number of entries in the log (including checkpoints).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the sequence number through which compaction has occurred.
    pub fn compacted_through(&self) -> u64 {
        self.compacted_through
    }

    /// Check if compaction is recommended based on configuration.
    pub fn needs_compaction(&self) -> bool {
        self.entries.len() > self.config.compaction_threshold
    }

    /// Get an entry by sequence number.
    pub fn get(&self, seq: u64) -> Option<&WalEntry<T>> {
        if seq == 0 || seq < self.first_seq() {
            return None;
        }
        // Binary search since entries are sorted by seq
        self.entries.iter().find(|e| e.seq == seq)
    }

    /// Get the first (oldest) sequence number in the log.
    pub fn first_seq(&self) -> u64 {
        self.entries.front().map(|e| e.seq).unwrap_or(self.next_seq)
    }

    /// Get the last (newest) sequence number in the log.
    pub fn last_seq(&self) -> u64 {
        self.entries.back().map(|e| e.seq).unwrap_or(0)
    }

    /// Iterate over all entries.
    pub fn iter(&self) -> impl Iterator<Item = &WalEntry<T>> {
        self.entries.iter()
    }

    /// Iterate over entries since (exclusive) a given sequence number.
    pub fn since(&self, seq: u64) -> impl Iterator<Item = &WalEntry<T>> {
        self.entries.iter().filter(move |e| e.seq > seq)
    }

    /// Iterate over mutation entries only (skipping checkpoints).
    pub fn mutations(&self) -> impl Iterator<Item = &WalEntry<T>> {
        self.entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Mutation(_)))
    }

    /// Iterate over entries between two sequence numbers (inclusive).
    pub fn range(&self, from_seq: u64, to_seq: u64) -> impl Iterator<Item = &WalEntry<T>> {
        self.entries
            .iter()
            .filter(move |e| e.seq >= from_seq && e.seq <= to_seq)
    }

    /// Get entries since the last checkpoint.
    ///
    /// If no checkpoint exists, returns all entries.
    pub fn since_last_checkpoint(&self) -> impl Iterator<Item = &WalEntry<T>> {
        let cp_seq = self.last_checkpoint_seq.unwrap_or(0);
        self.entries.iter().filter(move |e| e.seq > cp_seq)
    }

    /// Compact the log by removing old entries.
    ///
    /// Retains entries after the most recent checkpoint (or the last
    /// `max_retained_entries` if no checkpoint exists). Returns the
    /// number of entries removed.
    pub fn compact(&mut self) -> usize {
        let retain_after = if let Some(cp_seq) = self.last_checkpoint_seq {
            // Keep entries after the last checkpoint
            cp_seq
        } else {
            // No checkpoint — keep last max_retained_entries
            let total = self.entries.len();
            if total <= self.config.max_retained_entries {
                return 0;
            }
            let keep_from_idx = total - self.config.max_retained_entries;
            self.entries
                .get(keep_from_idx)
                .map(|e| e.seq.saturating_sub(1))
                .unwrap_or(0)
        };

        let mut removed = 0;
        while let Some(front) = self.entries.front() {
            if front.seq <= retain_after && !matches!(front.kind, EntryKind::Checkpoint) {
                self.entries.pop_front();
                removed += 1;
            } else if front.seq < retain_after && matches!(front.kind, EntryKind::Checkpoint) {
                // Remove old checkpoints too (except the retain_after one)
                self.entries.pop_front();
                removed += 1;
            } else {
                break;
            }
        }

        if removed > 0 {
            self.compacted_through = retain_after;
            // Add compaction marker
            let seq = self.next_seq;
            self.next_seq += 1;
            self.entries.push_back(WalEntry {
                seq,
                timestamp_ms: 0,
                kind: EntryKind::CompactionMarker {
                    compacted_through: retain_after,
                },
            });
        }

        removed
    }

    /// Truncate the log to entries up to (inclusive) a given sequence number.
    ///
    /// Useful for "rewinding" to a known good state.
    pub fn truncate_after(&mut self, seq: u64) {
        while let Some(back) = self.entries.back() {
            if back.seq > seq {
                self.entries.pop_back();
            } else {
                break;
            }
        }
        self.next_seq = seq + 1;
        // Update last_checkpoint if it was truncated
        if let Some(cp) = self.last_checkpoint_seq {
            if cp > seq {
                self.last_checkpoint_seq = self
                    .entries
                    .iter()
                    .rev()
                    .find(|e| matches!(e.kind, EntryKind::Checkpoint))
                    .map(|e| e.seq);
            }
        }
    }

    /// Clear all entries. Resets the engine to empty state.
    ///
    /// Preserves the current sequence counter (entries are never reused).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.last_checkpoint_seq = None;
    }
}

// ── Replay support ───────────────────────────────────────────────

/// A replay cursor for replaying WAL entries.
///
/// Iterates over mutation entries in order, allowing the caller
/// to reconstruct state by applying each mutation.
pub struct ReplayCursor<'a, T: Clone> {
    entries: std::collections::vec_deque::Iter<'a, WalEntry<T>>,
    from_seq: u64,
    to_seq: u64,
}

impl<'a, T: Clone> ReplayCursor<'a, T> {
    /// Create a replay cursor over a range of sequences.
    pub fn new(wal: &'a WalEngine<T>, from_seq: u64, to_seq: u64) -> Self {
        Self {
            entries: wal.entries.iter(),
            from_seq,
            to_seq,
        }
    }
}

impl<'a, T: Clone> Iterator for ReplayCursor<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = self.entries.next()?;
            if entry.seq < self.from_seq {
                continue;
            }
            if entry.seq > self.to_seq {
                return None;
            }
            if let EntryKind::Mutation(ref data) = entry.kind {
                return Some(data);
            }
        }
    }
}

/// Replay mutations from a WAL engine within a sequence range.
///
/// Returns only the mutation payloads (not checkpoints or markers).
pub fn replay_mutations<T: Clone>(wal: &WalEngine<T>, from_seq: u64, to_seq: u64) -> Vec<&T> {
    ReplayCursor::new(wal, from_seq, to_seq).collect()
}

// ── WAL Statistics ───────────────────────────────────────────────

/// Statistics about the WAL engine state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalStats {
    /// Total entries in the log.
    pub total_entries: usize,
    /// Number of mutation entries.
    pub mutation_count: usize,
    /// Number of checkpoint entries.
    pub checkpoint_count: usize,
    /// First sequence number.
    pub first_seq: u64,
    /// Last sequence number.
    pub last_seq: u64,
    /// Sequence of last checkpoint.
    pub last_checkpoint_seq: Option<u64>,
    /// Compacted through sequence.
    pub compacted_through: u64,
    /// Whether compaction is recommended.
    pub needs_compaction: bool,
}

impl<T: Clone> WalEngine<T> {
    /// Get statistics about the current WAL state.
    pub fn stats(&self) -> WalStats {
        WalStats {
            total_entries: self.len(),
            mutation_count: self.mutations().count(),
            checkpoint_count: self
                .entries
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::Checkpoint))
                .count(),
            first_seq: self.first_seq(),
            last_seq: self.last_seq(),
            last_checkpoint_seq: self.last_checkpoint_seq,
            compacted_through: self.compacted_through,
            needs_compaction: self.needs_compaction(),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::needless_collect)]
mod tests {
    use super::*;

    fn default_wal() -> WalEngine<String> {
        WalEngine::new(WalConfig::default())
    }

    fn small_wal() -> WalEngine<String> {
        WalEngine::new(WalConfig {
            compaction_threshold: 5,
            max_retained_entries: 3,
        })
    }

    #[test]
    fn empty_wal() {
        let wal = default_wal();
        assert!(wal.is_empty());
        assert_eq!(wal.len(), 0);
        assert_eq!(wal.next_seq(), 1);
        assert_eq!(wal.last_checkpoint(), None);
    }

    #[test]
    fn append_mutation() {
        let mut wal = default_wal();
        let seq = wal.append("hello".to_string(), 1000);
        assert_eq!(seq, 1);
        assert_eq!(wal.len(), 1);
        assert_eq!(wal.next_seq(), 2);

        let entry = wal.get(1).unwrap();
        assert_eq!(entry.seq, 1);
        assert_eq!(entry.timestamp_ms, 1000);
        assert!(matches!(&entry.kind, EntryKind::Mutation(s) if s == "hello"));
    }

    #[test]
    fn append_multiple() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("entry_{}", i), i as u64 * 100);
        }
        assert_eq!(wal.len(), 5);
        assert_eq!(wal.first_seq(), 1);
        assert_eq!(wal.last_seq(), 5);
    }

    #[test]
    fn checkpoint() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        let cp = wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        assert_eq!(wal.last_checkpoint(), Some(cp));
        assert_eq!(wal.len(), 3);
    }

    #[test]
    fn since_iterator() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let since_3: Vec<_> = wal.since(3).collect();
        assert_eq!(since_3.len(), 2); // seqs 4 and 5
        assert_eq!(since_3[0].seq, 4);
        assert_eq!(since_3[1].seq, 5);
    }

    #[test]
    fn mutations_iterator() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        let muts: Vec<_> = wal.mutations().collect();
        assert_eq!(muts.len(), 2); // only mutations, no checkpoint
    }

    #[test]
    fn range_iterator() {
        let mut wal = default_wal();
        for i in 0..10 {
            wal.append(format!("e{}", i), i as u64);
        }
        let range: Vec<_> = wal.range(3, 7).collect();
        assert_eq!(range.len(), 5);
        assert_eq!(range[0].seq, 3);
        assert_eq!(range[4].seq, 7);
    }

    #[test]
    fn since_last_checkpoint() {
        let mut wal = default_wal();
        wal.append("before1".to_string(), 100);
        wal.append("before2".to_string(), 200);
        wal.checkpoint(300);
        wal.append("after1".to_string(), 400);
        wal.append("after2".to_string(), 500);

        let since_cp: Vec<_> = wal.since_last_checkpoint().collect();
        assert_eq!(since_cp.len(), 2);
        assert!(matches!(&since_cp[0].kind, EntryKind::Mutation(s) if s == "after1"));
    }

    #[test]
    fn compact_with_checkpoint() {
        let mut wal = small_wal();
        for i in 0..3 {
            wal.append(format!("old_{}", i), i as u64);
        }
        wal.checkpoint(300);
        for i in 0..3 {
            wal.append(format!("new_{}", i), 400 + i as u64);
        }

        let before = wal.len();
        let removed = wal.compact();
        assert!(removed > 0, "should compact old entries");
        assert!(wal.len() < before, "log should be smaller after compaction");

        // New entries should still be accessible
        let muts: Vec<_> = wal.mutations().collect();
        assert!(muts.len() >= 3, "new mutations should survive compaction");
    }

    #[test]
    fn compact_without_checkpoint() {
        let mut wal = WalEngine::new(WalConfig {
            compaction_threshold: 3,
            max_retained_entries: 2,
        });
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let removed = wal.compact();
        assert!(removed > 0);
        assert!(wal.len() <= 4); // at most 2 retained + compaction marker + maybe some
    }

    #[test]
    fn truncate_after() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        wal.truncate_after(3);
        assert_eq!(wal.len(), 3);
        assert_eq!(wal.last_seq(), 3);
        assert_eq!(wal.next_seq(), 4);
    }

    #[test]
    fn truncate_removes_checkpoint() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        wal.checkpoint(400);
        wal.append("c".to_string(), 500);

        wal.truncate_after(3); // keeps a, cp, b
        assert_eq!(wal.last_checkpoint(), Some(2)); // first checkpoint
    }

    #[test]
    fn clear() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let next = wal.next_seq();
        wal.clear();
        assert!(wal.is_empty());
        assert_eq!(wal.next_seq(), next); // sequence preserved
        assert_eq!(wal.last_checkpoint(), None);
    }

    #[test]
    fn replay_mutations() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        wal.append("c".to_string(), 400);

        let replayed: Vec<_> = super::replay_mutations(&wal, 1, 4).into_iter().collect();
        assert_eq!(replayed.len(), 3);
        assert_eq!(*replayed[0], "a");
        assert_eq!(*replayed[1], "b");
        assert_eq!(*replayed[2], "c");
    }

    #[test]
    fn replay_partial() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let replayed: Vec<_> = super::replay_mutations(&wal, 2, 4).into_iter().collect();
        assert_eq!(replayed.len(), 3);
        assert_eq!(*replayed[0], "e1"); // seq 2
    }

    #[test]
    fn get_nonexistent() {
        let wal = default_wal();
        assert!(wal.get(0).is_none());
        assert!(wal.get(99).is_none());
    }

    #[test]
    fn stats() {
        let mut wal = small_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);

        let stats = wal.stats();
        assert_eq!(stats.total_entries, 3);
        assert_eq!(stats.mutation_count, 2);
        assert_eq!(stats.checkpoint_count, 1);
        assert_eq!(stats.first_seq, 1);
        assert_eq!(stats.last_seq, 3);
        assert_eq!(stats.last_checkpoint_seq, Some(2));
    }

    #[test]
    fn stats_serde() {
        let stats = WalStats {
            total_entries: 10,
            mutation_count: 8,
            checkpoint_count: 2,
            first_seq: 1,
            last_seq: 10,
            last_checkpoint_seq: Some(5),
            compacted_through: 0,
            needs_compaction: false,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: WalStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn needs_compaction() {
        let mut wal = small_wal(); // threshold = 5
        for i in 0..4 {
            wal.append(format!("e{}", i), i as u64);
        }
        assert!(!wal.needs_compaction());
        wal.append("e4".to_string(), 4);
        wal.append("e5".to_string(), 5);
        assert!(wal.needs_compaction());
    }

    #[test]
    fn sequence_monotonic() {
        let mut wal = default_wal();
        let s1 = wal.append("a".to_string(), 0);
        let s2 = wal.checkpoint(1);
        let s3 = wal.append("b".to_string(), 2);
        assert!(s1 < s2);
        assert!(s2 < s3);
    }

    #[test]
    fn entry_kind_serde() {
        let kinds: Vec<EntryKind<String>> = vec![
            EntryKind::Mutation("test".to_string()),
            EntryKind::Checkpoint,
            EntryKind::CompactionMarker {
                compacted_through: 42,
            },
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: EntryKind<String> = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back);
        }
    }

    // -------------------------------------------------------------------
    // Batch: DarkBadger wa-1u90p.7.1
    // -------------------------------------------------------------------

    // -- WalConfig --

    #[test]
    fn wal_config_debug_clone() {
        let config = WalConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("WalConfig"), "got: {}", dbg);
        let cloned = config.clone();
        assert_eq!(cloned.compaction_threshold, config.compaction_threshold);
    }

    #[test]
    fn wal_config_default_values() {
        let config = WalConfig::default();
        assert_eq!(config.compaction_threshold, 10_000);
        assert_eq!(config.max_retained_entries, 1_000);
    }

    // -- WalEntry --

    #[test]
    fn wal_entry_debug() {
        let entry = WalEntry {
            seq: 1,
            timestamp_ms: 1000,
            kind: EntryKind::Mutation("test".to_string()),
        };
        let dbg = format!("{:?}", entry);
        assert!(dbg.contains("WalEntry"), "got: {}", dbg);
        assert!(dbg.contains("seq: 1"), "got: {}", dbg);
    }

    #[test]
    fn wal_entry_clone_eq() {
        let entry = WalEntry::<String> {
            seq: 5,
            timestamp_ms: 500,
            kind: EntryKind::Checkpoint,
        };
        let cloned = entry.clone();
        assert_eq!(entry, cloned);
    }

    #[test]
    fn wal_entry_serde_roundtrip() {
        let entry = WalEntry {
            seq: 42,
            timestamp_ms: 9999,
            kind: EntryKind::Mutation("hello".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: WalEntry<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    // -- EntryKind --

    #[test]
    fn entry_kind_debug() {
        let kind = EntryKind::CompactionMarker::<String> {
            compacted_through: 10,
        };
        let dbg = format!("{:?}", kind);
        assert!(dbg.contains("CompactionMarker"), "got: {}", dbg);
    }

    #[test]
    fn entry_kind_clone_eq() {
        let a = EntryKind::Mutation(42i32);
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(a, EntryKind::Checkpoint);
    }

    // -- WalEngine --

    #[test]
    fn wal_engine_debug() {
        let wal = default_wal();
        let dbg = format!("{:?}", wal);
        assert!(dbg.contains("WalEngine"), "got: {}", dbg);
    }

    #[test]
    fn wal_engine_clone_independence() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        let mut cloned = wal.clone();
        cloned.append("b".to_string(), 200);
        assert_eq!(wal.len(), 1);
        assert_eq!(cloned.len(), 2);
    }

    #[test]
    fn wal_first_seq_empty() {
        let wal = default_wal();
        // When empty, first_seq returns next_seq
        assert_eq!(wal.first_seq(), wal.next_seq());
    }

    #[test]
    fn wal_last_seq_empty() {
        let wal = default_wal();
        assert_eq!(wal.last_seq(), 0);
    }

    #[test]
    fn wal_compacted_through_initial() {
        let wal = default_wal();
        assert_eq!(wal.compacted_through(), 0);
    }

    #[test]
    fn wal_get_seq_zero() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        // Seq 0 is explicitly handled as None
        assert!(wal.get(0).is_none());
    }

    #[test]
    fn wal_get_below_first_seq() {
        let mut wal = small_wal();
        for i in 0..6 {
            wal.append(format!("e{}", i), i as u64);
        }
        wal.checkpoint(600);
        wal.compact();
        // After compaction, early sequences should be gone
        assert!(wal.get(1).is_none());
    }

    // -- Iterator edge cases --

    #[test]
    fn wal_since_high_seq() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        let result: Vec<_> = wal.since(999).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn wal_range_inverted() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        // from > to should return empty
        let result: Vec<_> = wal.range(5, 2).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn wal_range_beyond_bounds() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        let result: Vec<_> = wal.range(100, 200).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn wal_since_last_checkpoint_no_checkpoint() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.append("b".to_string(), 200);
        // Without checkpoint, returns all entries (since seq 0)
        let result: Vec<_> = wal.since_last_checkpoint().collect();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn wal_mutations_empty() {
        let wal = default_wal();
        assert_eq!(wal.mutations().count(), 0);
    }

    #[test]
    fn wal_iter_empty() {
        let wal = default_wal();
        assert_eq!(wal.iter().count(), 0);
    }

    // -- Compact edge cases --

    #[test]
    fn wal_compact_empty() {
        let mut wal = small_wal();
        let removed = wal.compact();
        assert_eq!(removed, 0);
    }

    #[test]
    fn wal_compact_below_threshold() {
        let mut wal = small_wal(); // threshold = 5
        wal.append("a".to_string(), 100);
        wal.append("b".to_string(), 200);
        // No checkpoint, below max_retained — nothing to compact
        let removed = wal.compact();
        assert_eq!(removed, 0);
    }

    #[test]
    fn wal_compact_updates_compacted_through() {
        let mut wal = small_wal();
        for i in 0..4 {
            wal.append(format!("e{}", i), i as u64);
        }
        wal.checkpoint(400);
        wal.append("after".to_string(), 500);
        assert_eq!(wal.compacted_through(), 0);
        let removed = wal.compact();
        if removed > 0 {
            assert!(wal.compacted_through() > 0);
        }
    }

    // -- Truncate edge cases --

    #[test]
    fn wal_truncate_empty() {
        let mut wal = default_wal();
        wal.truncate_after(0);
        assert!(wal.is_empty());
        assert_eq!(wal.next_seq(), 1);
    }

    #[test]
    fn wal_truncate_beyond_last() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.append("b".to_string(), 200);
        // Truncate beyond last seq is a no-op on entries
        wal.truncate_after(999);
        assert_eq!(wal.len(), 2);
    }

    // -- Clear --

    #[test]
    fn wal_clear_preserves_seq_counter() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let seq_before = wal.next_seq();
        wal.clear();
        assert_eq!(wal.next_seq(), seq_before);
    }

    // -- WalStats --

    #[test]
    fn wal_stats_debug_clone() {
        let stats = WalStats {
            total_entries: 5,
            mutation_count: 3,
            checkpoint_count: 2,
            first_seq: 1,
            last_seq: 5,
            last_checkpoint_seq: Some(4),
            compacted_through: 0,
            needs_compaction: false,
        };
        let dbg = format!("{:?}", stats);
        assert!(dbg.contains("WalStats"), "got: {}", dbg);
        let cloned = stats.clone();
        assert_eq!(stats, cloned);
    }

    #[test]
    fn wal_stats_empty() {
        let wal = default_wal();
        let stats = wal.stats();
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.mutation_count, 0);
        assert_eq!(stats.checkpoint_count, 0);
        assert_eq!(stats.last_checkpoint_seq, None);
        assert!(!stats.needs_compaction);
    }

    // -- ReplayCursor edge cases --

    #[test]
    fn replay_cursor_empty_wal() {
        let wal = default_wal();
        let result: Vec<_> = ReplayCursor::new(&wal, 1, 10).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn replay_cursor_from_exceeds_to() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.append("b".to_string(), 200);
        let result: Vec<_> = ReplayCursor::new(&wal, 10, 5).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn replay_cursor_skips_checkpoints() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        let result: Vec<_> = ReplayCursor::new(&wal, 1, 3).collect();
        assert_eq!(result.len(), 2);
        assert_eq!(*result[0], "a");
        assert_eq!(*result[1], "b");
    }

    // -- replay_mutations function --

    #[test]
    fn replay_mutations_empty_wal() {
        let wal = default_wal();
        let result = super::replay_mutations(&wal, 1, 10);
        assert!(result.is_empty());
    }
}
