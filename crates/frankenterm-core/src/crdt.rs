//! Conflict-Free Replicated Data Types (CRDTs) for distributed agent state.
//!
//! Provides eventually-consistent data structures that converge without
//! coordination. Each CRDT satisfies the Strong Eventual Consistency (SEC)
//! properties: all replicas that have received the same set of updates
//! (in any order) have equivalent state.
//!
//! # Mathematical Guarantees
//!
//! Every CRDT in this module forms a join-semilattice under its merge operation:
//! - **Commutativity**: merge(a, b) = merge(b, a)
//! - **Associativity**: merge(merge(a, b), c) = merge(a, merge(b, c))
//! - **Idempotency**: merge(a, a) = a
//!
//! # Types
//!
//! | Type | Description | Operations |
//! |------|-------------|------------|
//! | [`GCounter`] | Grow-only counter | increment, value, merge |
//! | [`PnCounter`] | Positive-negative counter | increment, decrement, value, merge |
//! | [`GSet`] | Grow-only set | insert, contains, merge |
//! | [`OrSet`] | Observed-remove set | insert, remove, contains, merge |
//! | [`LwwRegister`] | Last-writer-wins register | set, get, merge |
//! | [`MvRegister`] | Multi-value register | set, get_all, merge |
//!
//! # Use Cases
//!
//! - Distributed pane state synchronization across ft instances
//! - Multi-agent event counters without coordination
//! - Replicated configuration across distributed mode nodes
//! - Conflict-free agent status tracking in swarm operations
//!
//! Bead: ft-283h4.24

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::Hash;

// ─── Replica ID ──────────────────────────────────────────────────────────

/// A unique identifier for a replica (node/agent).
pub type ReplicaId = String;

// ─── G-Counter ───────────────────────────────────────────────────────────

/// Grow-only counter. Each replica maintains its own count; the global
/// value is the sum across all replicas.
///
/// # Lattice Properties
/// - State: map from ReplicaId → u64
/// - Merge: pointwise max
/// - Value: sum of all entries
///
/// # Example
/// ```
/// use frankenterm_core::crdt::GCounter;
///
/// let mut c1 = GCounter::new("node-1");
/// c1.increment();
/// c1.increment();
///
/// let mut c2 = GCounter::new("node-2");
/// c2.increment();
///
/// c1.merge(&c2);
/// assert_eq!(c1.value(), 3);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GCounter {
    replica_id: ReplicaId,
    counts: BTreeMap<ReplicaId, u64>,
}

impl GCounter {
    /// Create a new G-Counter for the given replica.
    pub fn new(replica_id: impl Into<ReplicaId>) -> Self {
        Self {
            replica_id: replica_id.into(),
            counts: BTreeMap::new(),
        }
    }

    /// Increment this replica's count by 1.
    pub fn increment(&mut self) {
        self.increment_by(1);
    }

    /// Increment this replica's count by `n`.
    pub fn increment_by(&mut self, n: u64) {
        let entry = self.counts.entry(self.replica_id.clone()).or_insert(0);
        *entry = entry.saturating_add(n);
    }

    /// Get the global counter value (sum of all replicas).
    pub fn value(&self) -> u64 {
        self.counts.values().fold(0u64, |acc, v| acc.saturating_add(*v))
    }

    /// Get this replica's local count.
    pub fn local_value(&self) -> u64 {
        self.counts.get(&self.replica_id).copied().unwrap_or(0)
    }

    /// Merge another G-Counter into this one (pointwise max).
    pub fn merge(&mut self, other: &GCounter) {
        for (id, &count) in &other.counts {
            let entry = self.counts.entry(id.clone()).or_insert(0);
            *entry = (*entry).max(count);
        }
    }

    /// Number of replicas that have contributed.
    pub fn replica_count(&self) -> usize {
        self.counts.len()
    }

    /// The replica ID of this counter.
    pub fn replica_id(&self) -> &str {
        &self.replica_id
    }
}

// ─── PN-Counter ──────────────────────────────────────────────────────────

/// Positive-negative counter. Combines two G-Counters: one for increments,
/// one for decrements. The value is P - N.
///
/// # Example
/// ```
/// use frankenterm_core::crdt::PnCounter;
///
/// let mut c = PnCounter::new("node-1");
/// c.increment();
/// c.increment();
/// c.decrement();
/// assert_eq!(c.value(), 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PnCounter {
    positive: GCounter,
    negative: GCounter,
}

impl PnCounter {
    /// Create a new PN-Counter for the given replica.
    pub fn new(replica_id: impl Into<ReplicaId>) -> Self {
        let id: ReplicaId = replica_id.into();
        Self {
            positive: GCounter::new(id.clone()),
            negative: GCounter::new(id),
        }
    }

    /// Increment by 1.
    pub fn increment(&mut self) {
        self.positive.increment();
    }

    /// Increment by `n`.
    pub fn increment_by(&mut self, n: u64) {
        self.positive.increment_by(n);
    }

    /// Decrement by 1.
    pub fn decrement(&mut self) {
        self.negative.increment();
    }

    /// Decrement by `n`.
    pub fn decrement_by(&mut self, n: u64) {
        self.negative.increment_by(n);
    }

    /// Get the counter value (positive - negative). May be negative.
    pub fn value(&self) -> i128 {
        i128::from(self.positive.value()) - i128::from(self.negative.value())
    }

    /// Merge another PN-Counter.
    pub fn merge(&mut self, other: &PnCounter) {
        self.positive.merge(&other.positive);
        self.negative.merge(&other.negative);
    }

    /// The replica ID.
    pub fn replica_id(&self) -> &str {
        self.positive.replica_id()
    }
}

// ─── G-Set ───────────────────────────────────────────────────────────────

/// Grow-only set. Elements can be added but never removed.
/// Merge is set union.
///
/// # Example
/// ```
/// use frankenterm_core::crdt::GSet;
///
/// let mut s1 = GSet::<String>::new();
/// s1.insert("a".to_string());
///
/// let mut s2 = GSet::<String>::new();
/// s2.insert("b".to_string());
///
/// s1.merge(&s2);
/// assert!(s1.contains(&"a".to_string()));
/// assert!(s1.contains(&"b".to_string()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GSet<T: Ord + Clone> {
    elements: BTreeSet<T>,
}

impl<T: Ord + Clone> GSet<T> {
    /// Create an empty G-Set.
    pub fn new() -> Self {
        Self {
            elements: BTreeSet::new(),
        }
    }

    /// Insert an element.
    pub fn insert(&mut self, item: T) {
        self.elements.insert(item);
    }

    /// Check membership.
    pub fn contains(&self, item: &T) -> bool {
        self.elements.contains(item)
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Iterate over elements.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.elements.iter()
    }

    /// Merge another G-Set (set union).
    pub fn merge(&mut self, other: &GSet<T>) {
        for item in &other.elements {
            self.elements.insert(item.clone());
        }
    }
}

impl<T: Ord + Clone> Default for GSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ─── OR-Set ──────────────────────────────────────────────────────────────

/// Observed-Remove Set. Elements can be added and removed. Concurrent
/// adds and removes of the same element are resolved in favor of add
/// (add-wins semantics).
///
/// Uses unique tags (replica_id + sequence number) to track additions.
/// A remove only removes tags that have been observed locally.
///
/// # Example
/// ```
/// use frankenterm_core::crdt::OrSet;
///
/// let mut s = OrSet::<String>::new("node-1");
/// s.insert("x".to_string());
/// assert!(s.contains(&"x".to_string()));
/// s.remove(&"x".to_string());
/// assert!(!s.contains(&"x".to_string()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrSet<T: Ord + Clone + Hash> {
    replica_id: ReplicaId,
    seq: u64,
    /// Map from element → set of (replica_id, seq) tags
    entries: BTreeMap<T, BTreeSet<(ReplicaId, u64)>>,
}

impl<T: Ord + Clone + Hash> OrSet<T> {
    /// Create a new OR-Set for the given replica.
    pub fn new(replica_id: impl Into<ReplicaId>) -> Self {
        Self {
            replica_id: replica_id.into(),
            seq: 0,
            entries: BTreeMap::new(),
        }
    }

    /// Insert an element with a fresh unique tag.
    pub fn insert(&mut self, item: T) {
        self.seq += 1;
        let tag = (self.replica_id.clone(), self.seq);
        self.entries.entry(item).or_default().insert(tag);
    }

    /// Remove an element by removing all currently-observed tags.
    pub fn remove(&mut self, item: &T) {
        self.entries.remove(item);
    }

    /// Check if element is in the set (has at least one tag).
    pub fn contains(&self, item: &T) -> bool {
        self.entries
            .get(item)
            .is_some_and(|tags| !tags.is_empty())
    }

    /// Number of distinct elements.
    pub fn len(&self) -> usize {
        self.entries
            .iter()
            .filter(|(_, tags)| !tags.is_empty())
            .count()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get all elements currently in the set.
    pub fn elements(&self) -> Vec<&T> {
        self.entries
            .iter()
            .filter(|(_, tags)| !tags.is_empty())
            .map(|(k, _)| k)
            .collect()
    }

    /// Merge another OR-Set. Add-wins: tags from both sides are unioned.
    /// Only tags that were explicitly removed on one side (and not re-added)
    /// disappear.
    pub fn merge(&mut self, other: &OrSet<T>) {
        for (item, other_tags) in &other.entries {
            let local_tags = self.entries.entry(item.clone()).or_default();
            for tag in other_tags {
                local_tags.insert(tag.clone());
            }
        }
        // Update our seq to be at least as high as any observed
        for tags in self.entries.values() {
            for (rid, s) in tags {
                if rid == &self.replica_id {
                    self.seq = self.seq.max(*s);
                }
            }
        }
    }

    /// The replica ID.
    pub fn replica_id(&self) -> &str {
        &self.replica_id
    }
}

// ─── LWW-Register ────────────────────────────────────────────────────────

/// Last-Writer-Wins Register. Stores a single value with a timestamp.
/// On merge, the value with the highest timestamp wins. Ties broken by
/// replica ID lexicographic order.
///
/// # Example
/// ```
/// use frankenterm_core::crdt::LwwRegister;
///
/// let mut r1 = LwwRegister::new("node-1", "initial".to_string(), 0);
/// let mut r2 = LwwRegister::new("node-2", "updated".to_string(), 1);
///
/// r1.merge(&r2);
/// assert_eq!(r1.get(), &"updated".to_string());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LwwRegister<T: Clone + Eq> {
    replica_id: ReplicaId,
    value: T,
    timestamp: u64,
    writer_id: ReplicaId,
}

impl<T: Clone + Eq> LwwRegister<T> {
    /// Create a new LWW-Register with an initial value and timestamp.
    pub fn new(replica_id: impl Into<ReplicaId>, value: T, timestamp: u64) -> Self {
        let rid: ReplicaId = replica_id.into();
        Self {
            writer_id: rid.clone(),
            replica_id: rid,
            value,
            timestamp,
        }
    }

    /// Set a new value with a timestamp. Only takes effect if the timestamp
    /// is greater than (or equal with higher replica ID) the current one.
    pub fn set(&mut self, value: T, timestamp: u64) {
        if timestamp > self.timestamp
            || (timestamp == self.timestamp && self.replica_id >= self.writer_id)
        {
            self.value = value;
            self.timestamp = timestamp;
            self.writer_id = self.replica_id.clone();
        }
    }

    /// Get the current value.
    pub fn get(&self) -> &T {
        &self.value
    }

    /// Get the current timestamp.
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }

    /// Get the writer ID of the current value.
    pub fn writer_id(&self) -> &str {
        &self.writer_id
    }

    /// Merge another register. Higher timestamp wins; ties broken by
    /// lexicographically greater writer ID.
    pub fn merge(&mut self, other: &LwwRegister<T>) {
        if other.timestamp > self.timestamp
            || (other.timestamp == self.timestamp && other.writer_id > self.writer_id)
        {
            self.value = other.value.clone();
            self.timestamp = other.timestamp;
            self.writer_id = other.writer_id.clone();
        }
    }

    /// The replica ID.
    pub fn replica_id(&self) -> &str {
        &self.replica_id
    }
}

// ─── MV-Register ─────────────────────────────────────────────────────────

/// Multi-Value Register. When concurrent writes happen, ALL concurrent
/// values are preserved (no data loss). The application resolves conflicts
/// by examining all values.
///
/// Uses a version vector to track causality. Concurrent updates produce
/// multiple values; causally-dominated updates are discarded.
///
/// # Example
/// ```
/// use frankenterm_core::crdt::MvRegister;
///
/// let mut r1 = MvRegister::<String>::new("node-1");
/// r1.set("hello".to_string());
///
/// let mut r2 = MvRegister::<String>::new("node-2");
/// r2.set("world".to_string());
///
/// r1.merge(&r2);
/// let values = r1.get_all();
/// assert!(values.len() == 2); // concurrent writes → both preserved
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MvRegister<T: Clone + Eq + Ord> {
    replica_id: ReplicaId,
    /// Each entry: (value, version_vector_at_write)
    entries: BTreeSet<MvEntry<T>>,
    /// Current version vector
    version_vector: BTreeMap<ReplicaId, u64>,
}

/// An entry in the MV-Register: value + the version vector at write time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct MvEntry<T: Clone + Eq + Ord> {
    value: T,
    vv: BTreeMap<ReplicaId, u64>,
}

impl<T: Clone + Eq + Ord> MvRegister<T> {
    /// Create a new empty MV-Register for the given replica.
    pub fn new(replica_id: impl Into<ReplicaId>) -> Self {
        Self {
            replica_id: replica_id.into(),
            entries: BTreeSet::new(),
            version_vector: BTreeMap::new(),
        }
    }

    /// Set a new value. Increments this replica's version and replaces all
    /// locally-known entries (since this write causally dominates them).
    pub fn set(&mut self, value: T) {
        // Increment our version
        let v = self
            .version_vector
            .entry(self.replica_id.clone())
            .or_insert(0);
        *v += 1;

        // This write dominates all current entries
        self.entries.clear();
        self.entries.insert(MvEntry {
            value,
            vv: self.version_vector.clone(),
        });
    }

    /// Get all concurrent values. If there's exactly one, there are no
    /// conflicts. Multiple values indicate concurrent writes.
    pub fn get_all(&self) -> Vec<&T> {
        self.entries.iter().map(|e| &e.value).collect()
    }

    /// Get a single value if there are no conflicts, None if empty or conflicted.
    pub fn get(&self) -> Option<&T> {
        if self.entries.len() == 1 {
            self.entries.iter().next().map(|e| &e.value)
        } else {
            None
        }
    }

    /// Number of concurrent values (1 = no conflict, >1 = conflict).
    pub fn conflict_count(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are concurrent conflicting values.
    pub fn has_conflict(&self) -> bool {
        self.entries.len() > 1
    }

    /// Merge another MV-Register. Keeps entries that are not dominated
    /// by any entry in the other register.
    pub fn merge(&mut self, other: &MvRegister<T>) {
        // Merge version vectors (pointwise max)
        for (id, &v) in &other.version_vector {
            let entry = self.version_vector.entry(id.clone()).or_insert(0);
            *entry = (*entry).max(v);
        }

        // Collect all entries from both sides
        let all_entries: Vec<MvEntry<T>> = self
            .entries
            .iter()
            .chain(other.entries.iter())
            .cloned()
            .collect();

        // Remove dominated entries: entry A dominates entry B if A's VV ≥ B's VV
        // (componentwise) and A ≠ B
        let mut kept = Vec::new();
        for (i, entry) in all_entries.iter().enumerate() {
            let dominated = all_entries.iter().enumerate().any(|(j, other_entry)| {
                i != j && vv_dominates(&other_entry.vv, &entry.vv)
            });
            if !dominated {
                kept.push(entry.clone());
            }
        }

        self.entries = kept.into_iter().collect();
    }

    /// The replica ID.
    pub fn replica_id(&self) -> &str {
        &self.replica_id
    }
}

/// Check if version vector `a` dominates (≥ componentwise and >) version vector `b`.
fn vv_dominates(a: &BTreeMap<ReplicaId, u64>, b: &BTreeMap<ReplicaId, u64>) -> bool {
    // a dominates b if: for all keys in b, a[key] >= b[key], AND a ≠ b
    if a == b {
        return false;
    }
    for (key, &b_val) in b {
        let a_val = a.get(key).copied().unwrap_or(0);
        if a_val < b_val {
            return false;
        }
    }
    true
}

// ─── Version Vector Utilities ────────────────────────────────────────────

/// A version vector for tracking causality across replicas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionVector {
    entries: BTreeMap<ReplicaId, u64>,
}

impl VersionVector {
    /// Create an empty version vector.
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Increment the version for a replica.
    pub fn increment(&mut self, replica_id: &str) {
        let v = self.entries.entry(replica_id.to_string()).or_insert(0);
        *v += 1;
    }

    /// Get the version for a replica.
    pub fn get(&self, replica_id: &str) -> u64 {
        self.entries.get(replica_id).copied().unwrap_or(0)
    }

    /// Merge with another version vector (pointwise max).
    pub fn merge(&mut self, other: &VersionVector) {
        for (id, &v) in &other.entries {
            let entry = self.entries.entry(id.clone()).or_insert(0);
            *entry = (*entry).max(v);
        }
    }

    /// Check if this VV dominates another (causally after).
    pub fn dominates(&self, other: &VersionVector) -> bool {
        vv_dominates(&self.entries, &other.entries)
    }

    /// Check if this VV is concurrent with another (neither dominates).
    pub fn concurrent_with(&self, other: &VersionVector) -> bool {
        !self.dominates(other) && !other.dominates(self)
    }

    /// Number of replicas tracked.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for VersionVector {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Convergence Hash ────────────────────────────────────────────────────

/// Compute a deterministic hash of a CRDT state for convergence checking.
/// Two replicas with the same convergence hash have identical state.
pub fn convergence_hash(data: &[u8]) -> u64 {
    // FNV-1a hash for deterministic fingerprinting
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── G-Counter Tests ─────────────────────────────────────────────

    #[test]
    fn gcounter_empty() {
        let c = GCounter::new("r1");
        assert_eq!(c.value(), 0);
        assert_eq!(c.local_value(), 0);
        assert_eq!(c.replica_count(), 0);
    }

    #[test]
    fn gcounter_increment() {
        let mut c = GCounter::new("r1");
        c.increment();
        c.increment();
        assert_eq!(c.value(), 2);
        assert_eq!(c.local_value(), 2);
    }

    #[test]
    fn gcounter_increment_by() {
        let mut c = GCounter::new("r1");
        c.increment_by(10);
        assert_eq!(c.value(), 10);
    }

    #[test]
    fn gcounter_merge_disjoint() {
        let mut c1 = GCounter::new("r1");
        let mut c2 = GCounter::new("r2");
        c1.increment_by(5);
        c2.increment_by(3);
        c1.merge(&c2);
        assert_eq!(c1.value(), 8);
        assert_eq!(c1.replica_count(), 2);
    }

    #[test]
    fn gcounter_merge_overlapping() {
        let mut c1 = GCounter::new("r1");
        let mut c2 = GCounter::new("r1");
        c1.increment_by(5);
        c2.increment_by(3);
        c1.merge(&c2);
        // Same replica ID: max(5, 3) = 5
        assert_eq!(c1.value(), 5);
    }

    #[test]
    fn gcounter_merge_idempotent() {
        let mut c1 = GCounter::new("r1");
        c1.increment_by(5);
        let snapshot = c1.clone();
        c1.merge(&snapshot);
        assert_eq!(c1.value(), 5);
    }

    #[test]
    fn gcounter_merge_commutative() {
        let mut c1 = GCounter::new("r1");
        let mut c2 = GCounter::new("r2");
        c1.increment_by(5);
        c2.increment_by(3);

        let mut a = c1.clone();
        a.merge(&c2);

        let mut b = c2.clone();
        b.merge(&c1);

        assert_eq!(a.value(), b.value());
    }

    #[test]
    fn gcounter_saturation() {
        let mut c = GCounter::new("r1");
        c.increment_by(u64::MAX - 1);
        c.increment_by(10);
        assert_eq!(c.local_value(), u64::MAX);
    }

    #[test]
    fn gcounter_serde_roundtrip() {
        let mut c = GCounter::new("r1");
        c.increment_by(42);
        let json = serde_json::to_string(&c).unwrap();
        let back: GCounter = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    // ─── PN-Counter Tests ────────────────────────────────────────────

    #[test]
    fn pncounter_empty() {
        let c = PnCounter::new("r1");
        assert_eq!(c.value(), 0);
    }

    #[test]
    fn pncounter_increment_decrement() {
        let mut c = PnCounter::new("r1");
        c.increment();
        c.increment();
        c.decrement();
        assert_eq!(c.value(), 1);
    }

    #[test]
    fn pncounter_negative() {
        let mut c = PnCounter::new("r1");
        c.decrement();
        c.decrement();
        assert_eq!(c.value(), -2);
    }

    #[test]
    fn pncounter_merge() {
        let mut c1 = PnCounter::new("r1");
        let mut c2 = PnCounter::new("r2");
        c1.increment_by(10);
        c1.decrement_by(3);
        c2.increment_by(5);
        c2.decrement_by(1);
        c1.merge(&c2);
        assert_eq!(c1.value(), (10 - 3 + 5 - 1) as i128);
    }

    #[test]
    fn pncounter_merge_commutative() {
        let mut c1 = PnCounter::new("r1");
        let mut c2 = PnCounter::new("r2");
        c1.increment_by(10);
        c2.decrement_by(3);

        let mut a = c1.clone();
        a.merge(&c2);

        let mut b = c2.clone();
        b.merge(&c1);

        assert_eq!(a.value(), b.value());
    }

    #[test]
    fn pncounter_serde_roundtrip() {
        let mut c = PnCounter::new("r1");
        c.increment_by(10);
        c.decrement_by(3);
        let json = serde_json::to_string(&c).unwrap();
        let back: PnCounter = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    // ─── G-Set Tests ─────────────────────────────────────────────────

    #[test]
    fn gset_empty() {
        let s = GSet::<String>::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn gset_insert_contains() {
        let mut s = GSet::new();
        s.insert("hello".to_string());
        assert!(s.contains(&"hello".to_string()));
        assert!(!s.contains(&"world".to_string()));
    }

    #[test]
    fn gset_merge() {
        let mut s1 = GSet::new();
        let mut s2 = GSet::new();
        s1.insert("a".to_string());
        s2.insert("b".to_string());
        s1.merge(&s2);
        assert!(s1.contains(&"a".to_string()));
        assert!(s1.contains(&"b".to_string()));
        assert_eq!(s1.len(), 2);
    }

    #[test]
    fn gset_merge_idempotent() {
        let mut s = GSet::new();
        s.insert("a".to_string());
        let snapshot = s.clone();
        s.merge(&snapshot);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn gset_merge_commutative() {
        let mut s1 = GSet::new();
        let mut s2 = GSet::new();
        s1.insert("a".to_string());
        s2.insert("b".to_string());

        let mut a = s1.clone();
        a.merge(&s2);

        let mut b = s2.clone();
        b.merge(&s1);

        assert_eq!(a, b);
    }

    #[test]
    fn gset_serde_roundtrip() {
        let mut s = GSet::new();
        s.insert("x".to_string());
        s.insert("y".to_string());
        let json = serde_json::to_string(&s).unwrap();
        let back: GSet<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    // ─── OR-Set Tests ────────────────────────────────────────────────

    #[test]
    fn orset_empty() {
        let s = OrSet::<String>::new("r1");
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn orset_insert_remove() {
        let mut s = OrSet::new("r1");
        s.insert("x".to_string());
        assert!(s.contains(&"x".to_string()));
        s.remove(&"x".to_string());
        assert!(!s.contains(&"x".to_string()));
    }

    #[test]
    fn orset_concurrent_add_wins() {
        // Simulate concurrent add and remove
        let mut s1 = OrSet::new("r1");
        s1.insert("x".to_string());

        let mut s2 = s1.clone();
        // s1 removes x
        s1.remove(&"x".to_string());
        // s2 adds x concurrently (fresh tag)
        s2.insert("x".to_string());

        s1.merge(&s2);
        // Add wins: the concurrent add's tag survives
        assert!(s1.contains(&"x".to_string()));
    }

    #[test]
    fn orset_merge_disjoint() {
        let mut s1 = OrSet::new("r1");
        let mut s2 = OrSet::new("r2");
        s1.insert("a".to_string());
        s2.insert("b".to_string());
        s1.merge(&s2);
        assert!(s1.contains(&"a".to_string()));
        assert!(s1.contains(&"b".to_string()));
    }

    #[test]
    fn orset_elements() {
        let mut s = OrSet::new("r1");
        s.insert("a".to_string());
        s.insert("b".to_string());
        let elems = s.elements();
        assert_eq!(elems.len(), 2);
    }

    #[test]
    fn orset_serde_roundtrip() {
        let mut s = OrSet::new("r1");
        s.insert("x".to_string());
        let json = serde_json::to_string(&s).unwrap();
        let back: OrSet<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    // ─── LWW-Register Tests ──────────────────────────────────────────

    #[test]
    fn lww_basic() {
        let r = LwwRegister::new("r1", "initial".to_string(), 0);
        assert_eq!(r.get(), &"initial".to_string());
        assert_eq!(r.timestamp(), 0);
    }

    #[test]
    fn lww_set_higher_ts() {
        let mut r = LwwRegister::new("r1", "old".to_string(), 0);
        r.set("new".to_string(), 1);
        assert_eq!(r.get(), &"new".to_string());
    }

    #[test]
    fn lww_merge_higher_wins() {
        let mut r1 = LwwRegister::new("r1", "old".to_string(), 0);
        let r2 = LwwRegister::new("r2", "new".to_string(), 1);
        r1.merge(&r2);
        assert_eq!(r1.get(), &"new".to_string());
    }

    #[test]
    fn lww_merge_lower_ignored() {
        let mut r1 = LwwRegister::new("r1", "new".to_string(), 2);
        let r2 = LwwRegister::new("r2", "old".to_string(), 1);
        r1.merge(&r2);
        assert_eq!(r1.get(), &"new".to_string());
    }

    #[test]
    fn lww_merge_tie_broken_by_replica() {
        let mut r1 = LwwRegister::new("aaa", "val_a".to_string(), 5);
        let r2 = LwwRegister::new("zzz", "val_z".to_string(), 5);
        r1.merge(&r2);
        // Higher replica ID wins on tie
        assert_eq!(r1.get(), &"val_z".to_string());
    }

    #[test]
    fn lww_merge_commutative() {
        let r1 = LwwRegister::new("r1", "a".to_string(), 1);
        let r2 = LwwRegister::new("r2", "b".to_string(), 2);

        let mut a = r1.clone();
        a.merge(&r2);

        let mut b = r2.clone();
        b.merge(&r1);

        assert_eq!(a.get(), b.get());
    }

    #[test]
    fn lww_merge_idempotent() {
        let mut r = LwwRegister::new("r1", "val".to_string(), 1);
        let snapshot = r.clone();
        r.merge(&snapshot);
        assert_eq!(r.get(), &"val".to_string());
    }

    #[test]
    fn lww_serde_roundtrip() {
        let r = LwwRegister::new("r1", "hello".to_string(), 42);
        let json = serde_json::to_string(&r).unwrap();
        let back: LwwRegister<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    // ─── MV-Register Tests ───────────────────────────────────────────

    #[test]
    fn mvregister_empty() {
        let r = MvRegister::<String>::new("r1");
        assert_eq!(r.get_all().len(), 0);
        assert_eq!(r.conflict_count(), 0);
        assert!(!r.has_conflict());
    }

    #[test]
    fn mvregister_single_set() {
        let mut r = MvRegister::new("r1");
        r.set("hello".to_string());
        assert_eq!(r.get(), Some(&"hello".to_string()));
        assert_eq!(r.conflict_count(), 1);
        assert!(!r.has_conflict());
    }

    #[test]
    fn mvregister_sequential_overwrites() {
        let mut r = MvRegister::new("r1");
        r.set("first".to_string());
        r.set("second".to_string());
        assert_eq!(r.get(), Some(&"second".to_string()));
        assert_eq!(r.conflict_count(), 1);
    }

    #[test]
    fn mvregister_concurrent_writes_produce_conflict() {
        let mut r1 = MvRegister::new("r1");
        r1.set("from_r1".to_string());

        let mut r2 = MvRegister::new("r2");
        r2.set("from_r2".to_string());

        r1.merge(&r2);
        assert!(r1.has_conflict());
        assert_eq!(r1.conflict_count(), 2);
        let values = r1.get_all();
        assert!(values.contains(&&"from_r1".to_string()));
        assert!(values.contains(&&"from_r2".to_string()));
    }

    #[test]
    fn mvregister_conflict_resolution_by_subsequent_write() {
        let mut r1 = MvRegister::new("r1");
        r1.set("from_r1".to_string());

        let mut r2 = MvRegister::new("r2");
        r2.set("from_r2".to_string());

        r1.merge(&r2);
        assert!(r1.has_conflict());

        // Resolve by writing a new value
        r1.set("resolved".to_string());
        assert_eq!(r1.get(), Some(&"resolved".to_string()));
        assert!(!r1.has_conflict());
    }

    #[test]
    fn mvregister_causal_dominance() {
        let mut r1 = MvRegister::new("r1");
        r1.set("v1".to_string());

        // r2 observes r1's state and then writes
        let mut r2 = r1.clone();
        r2.set("v2".to_string());

        // r1 merges r2's state — v2 causally dominates v1
        r1.merge(&r2);
        assert_eq!(r1.get(), Some(&"v2".to_string()));
        assert!(!r1.has_conflict());
    }

    #[test]
    fn mvregister_serde_roundtrip() {
        let mut r = MvRegister::new("r1");
        r.set("hello".to_string());
        let json = serde_json::to_string(&r).unwrap();
        let back: MvRegister<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    // ─── Version Vector Tests ────────────────────────────────────────

    #[test]
    fn vv_empty() {
        let vv = VersionVector::new();
        assert!(vv.is_empty());
        assert_eq!(vv.get("r1"), 0);
    }

    #[test]
    fn vv_increment() {
        let mut vv = VersionVector::new();
        vv.increment("r1");
        vv.increment("r1");
        assert_eq!(vv.get("r1"), 2);
    }

    #[test]
    fn vv_merge() {
        let mut vv1 = VersionVector::new();
        let mut vv2 = VersionVector::new();
        vv1.increment("r1");
        vv1.increment("r1");
        vv2.increment("r1");
        vv2.increment("r2");
        vv1.merge(&vv2);
        assert_eq!(vv1.get("r1"), 2); // max(2, 1) = 2
        assert_eq!(vv1.get("r2"), 1);
    }

    #[test]
    fn vv_dominates() {
        let mut vv1 = VersionVector::new();
        let mut vv2 = VersionVector::new();
        vv1.increment("r1");
        vv1.increment("r1");
        vv2.increment("r1");
        assert!(vv1.dominates(&vv2));
        assert!(!vv2.dominates(&vv1));
    }

    #[test]
    fn vv_not_dominates_self() {
        let mut vv = VersionVector::new();
        vv.increment("r1");
        assert!(!vv.dominates(&vv));
    }

    // ─── Convergence Hash Tests ──────────────────────────────────────

    #[test]
    fn convergence_hash_deterministic() {
        let data = b"test data";
        let h1 = convergence_hash(data);
        let h2 = convergence_hash(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn convergence_hash_different_input() {
        let h1 = convergence_hash(b"hello");
        let h2 = convergence_hash(b"world");
        assert_ne!(h1, h2);
    }

    // ─── Three-Replica Convergence Scenarios ─────────────────────────

    #[test]
    fn three_replica_gcounter_convergence() {
        let mut c1 = GCounter::new("r1");
        let mut c2 = GCounter::new("r2");
        let mut c3 = GCounter::new("r3");

        c1.increment_by(5);
        c2.increment_by(3);
        c3.increment_by(7);

        // Merge in different orders
        let mut a = c1.clone();
        a.merge(&c2);
        a.merge(&c3);

        let mut b = c3.clone();
        b.merge(&c1);
        b.merge(&c2);

        let mut c = c2.clone();
        c.merge(&c3);
        c.merge(&c1);

        assert_eq!(a.value(), 15);
        assert_eq!(b.value(), 15);
        assert_eq!(c.value(), 15);
    }

    #[test]
    fn three_replica_orset_convergence() {
        let mut s1 = OrSet::new("r1");
        let mut s2 = OrSet::new("r2");
        let mut s3 = OrSet::new("r3");

        s1.insert("a".to_string());
        s2.insert("b".to_string());
        s3.insert("a".to_string());
        s3.insert("c".to_string());

        let mut a = s1.clone();
        a.merge(&s2);
        a.merge(&s3);

        let mut b = s3.clone();
        b.merge(&s1);
        b.merge(&s2);

        // Both should contain a, b, c
        assert_eq!(a.len(), 3);
        assert_eq!(b.len(), 3);
        assert!(a.contains(&"a".to_string()));
        assert!(a.contains(&"b".to_string()));
        assert!(a.contains(&"c".to_string()));
    }

    #[test]
    fn pncounter_large_values() {
        let mut c = PnCounter::new("r1");
        c.increment_by(u64::MAX / 2);
        c.decrement_by(u64::MAX / 4);
        let expected = i128::from(u64::MAX / 2) - i128::from(u64::MAX / 4);
        assert_eq!(c.value(), expected);
    }

    #[test]
    fn gset_large_merge() {
        let mut s1 = GSet::new();
        let mut s2 = GSet::new();
        for i in 0..100 {
            s1.insert(format!("item-{}", i));
        }
        for i in 50..150 {
            s2.insert(format!("item-{}", i));
        }
        s1.merge(&s2);
        assert_eq!(s1.len(), 150);
    }

    #[test]
    fn orset_remove_readd() {
        let mut s = OrSet::new("r1");
        s.insert("x".to_string());
        s.remove(&"x".to_string());
        s.insert("x".to_string());
        assert!(s.contains(&"x".to_string()));
    }

    #[test]
    fn lww_writer_tracking() {
        let r1 = LwwRegister::new("r1", "val".to_string(), 5);
        assert_eq!(r1.writer_id(), "r1");

        let mut r2 = LwwRegister::new("r2", "other".to_string(), 10);
        r2.merge(&r1);
        // r2 has higher ts, keeps its own value
        assert_eq!(r2.writer_id(), "r2");
    }
}
