//! Persistent immutable data structures with structural sharing.
//!
//! Enables O(log n) copy-on-write mutations where old versions remain valid,
//! consuming only O(log n) additional space per mutation via shared subtrees.
//!
//! # Use cases in FrankenTerm
//!
//! - **Time-travel debugging**: Store versioned pane metadata, query any historical
//!   state in O(log n) without full snapshots.
//! - **Differential snapshots**: Efficient diff between versions by comparing
//!   only divergent tree paths.
//! - **Safe concurrent reads**: Old versions are immutable; no locks needed to
//!   read historical state while new versions are being created.
//!
//! # Data structures
//!
//! - [`PersistentVec<T>`]: Array-like structure with O(log₃₂ n) indexed access,
//!   append, and set. Based on a radix-32 trie (5-bit branching).
//! - [`PersistentMap<K, V>`]: Hash array mapped trie (HAMT) with O(log₃₂ n)
//!   insert, remove, and lookup. Uses structural sharing via `Arc`.
//! - [`VersionedStore<T>`]: Thin wrapper tracking version history with timestamps.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

// ── Constants ──────────────────────────────────────────────────────

/// Branching factor for trie nodes (2^BITS).
const BITS: usize = 5;
/// Number of children per internal node.
const WIDTH: usize = 1 << BITS; // 32
/// Bit mask for extracting child index.
const MASK: usize = WIDTH - 1;

// ── PersistentVec ─────────────────────────────────────────────────

/// Internal node for the persistent vector trie.
#[derive(Clone, Debug)]
enum VecNode<T: Clone> {
    /// Internal node with up to WIDTH children.
    Internal(Vec<Arc<VecNode<T>>>),
    /// Leaf node with up to WIDTH elements.
    Leaf(Vec<T>),
}

/// A persistent (immutable, structural-sharing) vector.
///
/// Supports O(log₃₂ n) get, set, and append operations.
/// Cloning is O(1) — it shares the entire tree.
/// Mutations produce a new version sharing most of the tree.
#[derive(Clone, Debug)]
pub struct PersistentVec<T: Clone> {
    root: Arc<VecNode<T>>,
    len: usize,
    /// Depth of the trie (0 = leaf only).
    depth: usize,
}

impl<T: Clone> PersistentVec<T> {
    /// Create an empty persistent vector.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: Arc::new(VecNode::Leaf(Vec::new())),
            len: 0,
            depth: 0,
        }
    }

    /// Number of elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the vector is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get element at index, or None if out of bounds.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len {
            return None;
        }
        get_node(&self.root, index, self.depth)
    }

    /// Return a new vector with the element at `index` replaced.
    ///
    /// Returns `None` if index is out of bounds.
    #[must_use]
    pub fn set(&self, index: usize, value: T) -> Option<Self> {
        if index >= self.len {
            return None;
        }
        let new_root = set_node(&self.root, index, value, self.depth);
        Some(Self {
            root: Arc::new(new_root),
            len: self.len,
            depth: self.depth,
        })
    }

    /// Return a new vector with `value` appended at the end.
    #[must_use]
    pub fn push(&self, value: T) -> Self {
        let new_len = self.len + 1;
        // Check if we need to grow the tree depth
        let capacity = capacity_at_depth(self.depth);
        if self.len < capacity {
            let new_root = push_node(&self.root, self.len, value, self.depth);
            Self {
                root: Arc::new(new_root),
                len: new_len,
                depth: self.depth,
            }
        } else {
            // Need a new root level
            let new_depth = self.depth + 1;
            let new_root = VecNode::Internal(vec![self.root.clone()]);
            let new_root = push_node(&Arc::new(new_root), self.len, value, new_depth);
            Self {
                root: Arc::new(new_root),
                len: new_len,
                depth: new_depth,
            }
        }
    }

    /// Return a new vector with the last element removed.
    ///
    /// Returns `None` if the vector is empty.
    #[must_use]
    pub fn pop(&self) -> Option<(Self, T)> {
        if self.len == 0 {
            return None;
        }
        let last = self.get(self.len - 1)?.clone();
        let new_len = self.len - 1;
        if new_len == 0 {
            return Some((Self::new(), last));
        }
        // Rebuild without the last element
        let new_root = pop_node(&self.root, self.len - 1, self.depth);
        // Check if we can shrink depth
        let (root, depth) = shrink_root(new_root, self.depth);
        Some((
            Self {
                root: Arc::new(root),
                len: new_len,
                depth,
            },
            last,
        ))
    }

    /// Iterate over all elements.
    #[allow(clippy::iter_without_into_iter)]
    pub fn iter(&self) -> PersistentVecIter<'_, T> {
        PersistentVecIter {
            vec: self,
            index: 0,
        }
    }

    /// Create from an iterator.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn from_iter(iter: impl IntoIterator<Item = T>) -> Self {
        let mut v = Self::new();
        for item in iter {
            v = v.push(item);
        }
        v
    }
}

impl<T: Clone + PartialEq> PartialEq for PersistentVec<T> {
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        for i in 0..self.len {
            if self.get(i) != other.get(i) {
                return false;
            }
        }
        true
    }
}

impl<T: Clone + Eq> Eq for PersistentVec<T> {}

impl<T: Clone> Default for PersistentVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Iterator over PersistentVec elements.
pub struct PersistentVecIter<'a, T: Clone> {
    vec: &'a PersistentVec<T>,
    index: usize,
}

impl<'a, T: Clone> Iterator for PersistentVecIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.vec.len {
            return None;
        }
        let item = self.vec.get(self.index);
        self.index += 1;
        item
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.vec.len - self.index;
        (remaining, Some(remaining))
    }
}

impl<T: Clone> ExactSizeIterator for PersistentVecIter<'_, T> {}

// ── PersistentVec internals ───────────────────────────────────────

fn capacity_at_depth(depth: usize) -> usize {
    WIDTH.pow(depth as u32 + 1)
}

fn get_node<T: Clone>(node: &VecNode<T>, index: usize, depth: usize) -> Option<&T> {
    match node {
        VecNode::Leaf(items) => items.get(index & MASK),
        VecNode::Internal(children) => {
            let shift = depth * BITS;
            let child_idx = (index >> shift) & MASK;
            children
                .get(child_idx)
                .and_then(|child| get_node(child, index, depth - 1))
        }
    }
}

fn set_node<T: Clone>(node: &VecNode<T>, index: usize, value: T, depth: usize) -> VecNode<T> {
    match node {
        VecNode::Leaf(items) => {
            let mut new_items = items.clone();
            new_items[index & MASK] = value;
            VecNode::Leaf(new_items)
        }
        VecNode::Internal(children) => {
            let shift = depth * BITS;
            let child_idx = (index >> shift) & MASK;
            let mut new_children = children.clone();
            if let Some(child) = children.get(child_idx) {
                new_children[child_idx] = Arc::new(set_node(child, index, value, depth - 1));
            }
            VecNode::Internal(new_children)
        }
    }
}

fn push_node<T: Clone>(node: &VecNode<T>, index: usize, value: T, depth: usize) -> VecNode<T> {
    match node {
        VecNode::Leaf(items) => {
            let mut new_items = items.clone();
            new_items.push(value);
            VecNode::Leaf(new_items)
        }
        VecNode::Internal(children) => {
            let shift = depth * BITS;
            let child_idx = (index >> shift) & MASK;
            let mut new_children = children.clone();
            if child_idx < children.len() {
                // Push into existing child
                let child = &children[child_idx];
                new_children[child_idx] = Arc::new(push_node(child, index, value, depth - 1));
            } else {
                // Create new child path
                let new_child = create_path(value, depth - 1);
                new_children.push(Arc::new(new_child));
            }
            VecNode::Internal(new_children)
        }
    }
}

fn create_path<T: Clone>(value: T, depth: usize) -> VecNode<T> {
    if depth == 0 {
        VecNode::Leaf(vec![value])
    } else {
        let child = create_path(value, depth - 1);
        VecNode::Internal(vec![Arc::new(child)])
    }
}

fn pop_node<T: Clone>(node: &VecNode<T>, last_idx: usize, depth: usize) -> VecNode<T> {
    match node {
        VecNode::Leaf(items) => {
            let mut new_items = items.clone();
            new_items.pop();
            VecNode::Leaf(new_items)
        }
        VecNode::Internal(children) => {
            let shift = depth * BITS;
            let child_idx = (last_idx >> shift) & MASK;
            let mut new_children = children.clone();
            let child = &children[child_idx];
            let new_child = pop_node(child, last_idx, depth - 1);
            // If child became empty, remove it
            if is_empty_node(&new_child) {
                new_children.pop();
            } else {
                new_children[child_idx] = Arc::new(new_child);
            }
            VecNode::Internal(new_children)
        }
    }
}

fn is_empty_node<T: Clone>(node: &VecNode<T>) -> bool {
    match node {
        VecNode::Leaf(items) => items.is_empty(),
        VecNode::Internal(children) => children.is_empty(),
    }
}

fn shrink_root<T: Clone>(node: VecNode<T>, depth: usize) -> (VecNode<T>, usize) {
    if depth == 0 {
        return (node, 0);
    }
    if let VecNode::Internal(ref children) = node {
        if children.len() == 1 {
            let child = (*children[0]).clone();
            return shrink_root(child, depth - 1);
        }
    }
    (node, depth)
}

// ── PersistentMap (HAMT) ──────────────────────────────────────────

/// Hash array mapped trie node.
#[derive(Clone, Debug)]
enum MapNode<K: Clone + Eq + Hash, V: Clone> {
    /// Empty node.
    Empty,
    /// Single key-value pair (collision leaf).
    Leaf(u64, K, V),
    /// Collision node (multiple entries with same hash prefix).
    Collision(u64, Vec<(K, V)>),
    /// Internal bitmap-indexed node.
    Bitmap {
        bitmap: u32,
        children: Vec<Arc<MapNode<K, V>>>,
    },
}

/// A persistent hash array mapped trie (HAMT).
///
/// Supports O(log₃₂ n) insert, remove, and lookup operations.
/// Mutations produce a new version; old versions remain valid
/// and share the unchanged portions of the trie.
#[derive(Clone, Debug)]
pub struct PersistentMap<K: Clone + Eq + Hash, V: Clone> {
    root: Arc<MapNode<K, V>>,
    len: usize,
}

impl<K: Clone + Eq + Hash, V: Clone> PersistentMap<K, V> {
    /// Create an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: Arc::new(MapNode::Empty),
            len: 0,
        }
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Get the value for a key.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<&V> {
        let hash = hash_key(key);
        map_get(&self.root, key, hash, 0)
    }

    /// Check if the map contains a key.
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// Return a new map with the key-value pair inserted (or updated).
    #[must_use]
    pub fn insert(&self, key: K, value: V) -> Self {
        let hash = hash_key(&key);
        let (new_root, added) = map_insert(&self.root, key, value, hash, 0);
        Self {
            root: Arc::new(new_root),
            len: if added { self.len + 1 } else { self.len },
        }
    }

    /// Return a new map with the key removed.
    #[must_use]
    pub fn remove(&self, key: &K) -> Self {
        let hash = hash_key(key);
        let (new_root, removed) = map_remove(&self.root, key, hash, 0);
        Self {
            root: Arc::new(new_root),
            len: if removed { self.len - 1 } else { self.len },
        }
    }

    /// Create from key-value pairs.
    #[must_use]
    pub fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut map = Self::new();
        for (k, v) in entries {
            map = map.insert(k, v);
        }
        map
    }

    /// Collect all entries into a Vec.
    ///
    /// The order is deterministic but not sorted.
    #[must_use]
    pub fn entries(&self) -> Vec<(&K, &V)> {
        let mut result = Vec::with_capacity(self.len);
        collect_entries(&self.root, &mut result);
        result
    }

    /// Collect all keys into a Vec.
    #[must_use]
    pub fn keys(&self) -> Vec<&K> {
        self.entries().into_iter().map(|(k, _)| k).collect()
    }
}

impl<K: Clone + Eq + Hash, V: Clone + PartialEq> PartialEq for PersistentMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        for (k, v) in self.entries() {
            match other.get(k) {
                Some(ov) if ov == v => {}
                _ => return false,
            }
        }
        true
    }
}

impl<K: Clone + Eq + Hash, V: Clone + Eq> Eq for PersistentMap<K, V> {}

impl<K: Clone + Eq + Hash, V: Clone> Default for PersistentMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Clone + Eq + Hash + fmt::Debug, V: Clone + fmt::Debug> fmt::Display
    for PersistentMap<K, V>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{")?;
        let entries = self.entries();
        for (i, (k, v)) in entries.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{:?}: {:?}", k, v)?;
        }
        write!(f, "}}")
    }
}

// ── HAMT internals ────────────────────────────────────────────────

/// FNV-1a hash (deterministic, not random).
fn hash_key<K: Hash>(key: &K) -> u64 {
    let mut hasher = FnvHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

/// Simple FNV-1a hasher.
struct FnvHasher(u64);

impl FnvHasher {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }
}

impl Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= byte as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

fn map_get<'a, K: Clone + Eq + Hash, V: Clone>(
    node: &'a MapNode<K, V>,
    key: &K,
    hash: u64,
    shift: usize,
) -> Option<&'a V> {
    match node {
        MapNode::Empty => None,
        MapNode::Leaf(_, k, v) => {
            if k == key {
                Some(v)
            } else {
                None
            }
        }
        MapNode::Collision(_, entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
        MapNode::Bitmap { bitmap, children } => {
            let idx = ((hash >> shift) & MASK as u64) as u32;
            let bit = 1u32 << idx;
            if bitmap & bit == 0 {
                return None;
            }
            let child_idx = (bitmap & (bit - 1)).count_ones() as usize;
            map_get(&children[child_idx], key, hash, shift + BITS)
        }
    }
}

fn map_insert<K: Clone + Eq + Hash, V: Clone>(
    node: &MapNode<K, V>,
    key: K,
    value: V,
    hash: u64,
    shift: usize,
) -> (MapNode<K, V>, bool) {
    match node {
        MapNode::Empty => (MapNode::Leaf(hash, key, value), true),
        MapNode::Leaf(existing_hash, ek, ev) => {
            if ek == &key {
                // Update existing
                (MapNode::Leaf(hash, key, value), false)
            } else if *existing_hash == hash {
                // Hash collision at this level
                let entries = vec![(ek.clone(), ev.clone()), (key, value)];
                (MapNode::Collision(hash, entries), true)
            } else {
                // Split into bitmap node
                let new_node = MapNode::Bitmap {
                    bitmap: 0,
                    children: Vec::new(),
                };
                // Re-insert existing leaf
                let (n, _) = map_insert(&new_node, ek.clone(), ev.clone(), *existing_hash, shift);
                // Insert new entry
                map_insert(&n, key, value, hash, shift)
            }
        }
        MapNode::Collision(chash, entries) => {
            if *chash == hash {
                // Same hash prefix — add to collision list
                let mut new_entries = entries.clone();
                for (i, (k, _)) in entries.iter().enumerate() {
                    if k == &key {
                        new_entries[i] = (key, value);
                        return (MapNode::Collision(*chash, new_entries), false);
                    }
                }
                new_entries.push((key, value));
                (MapNode::Collision(*chash, new_entries), true)
            } else {
                // Different hash — expand to bitmap node
                let idx = ((chash >> shift) & MASK as u64) as u32;
                let bit = 1u32 << idx;
                let collision = Arc::new(MapNode::Collision(*chash, entries.clone()));
                let new_node = MapNode::Bitmap {
                    bitmap: bit,
                    children: vec![collision],
                };
                map_insert(&new_node, key, value, hash, shift)
            }
        }
        MapNode::Bitmap { bitmap, children } => {
            let idx = ((hash >> shift) & MASK as u64) as u32;
            let bit = 1u32 << idx;
            let child_idx = (bitmap & (bit - 1)).count_ones() as usize;

            if bitmap & bit == 0 {
                // New slot
                let mut new_children = children.clone();
                let leaf = Arc::new(MapNode::Leaf(hash, key, value));
                new_children.insert(child_idx, leaf);
                (
                    MapNode::Bitmap {
                        bitmap: bitmap | bit,
                        children: new_children,
                    },
                    true,
                )
            } else {
                // Recurse into existing child
                let (new_child, added) =
                    map_insert(&children[child_idx], key, value, hash, shift + BITS);
                let mut new_children = children.clone();
                new_children[child_idx] = Arc::new(new_child);
                (
                    MapNode::Bitmap {
                        bitmap: *bitmap,
                        children: new_children,
                    },
                    added,
                )
            }
        }
    }
}

fn map_remove<K: Clone + Eq + Hash, V: Clone>(
    node: &MapNode<K, V>,
    key: &K,
    hash: u64,
    shift: usize,
) -> (MapNode<K, V>, bool) {
    match node {
        MapNode::Empty => (MapNode::Empty, false),
        MapNode::Leaf(_, k, _) => {
            if k == key {
                (MapNode::Empty, true)
            } else {
                (node.clone(), false)
            }
        }
        MapNode::Collision(chash, entries) => {
            let new_entries: Vec<_> = entries.iter().filter(|(k, _)| k != key).cloned().collect();
            if new_entries.len() == entries.len() {
                (node.clone(), false)
            } else if new_entries.len() == 1 {
                let (k, v) = new_entries.into_iter().next().unwrap();
                (MapNode::Leaf(hash_key(&k), k, v), true)
            } else {
                (MapNode::Collision(*chash, new_entries), true)
            }
        }
        MapNode::Bitmap { bitmap, children } => {
            let idx = ((hash >> shift) & MASK as u64) as u32;
            let bit = 1u32 << idx;
            if bitmap & bit == 0 {
                return (node.clone(), false);
            }
            let child_idx = (bitmap & (bit - 1)).count_ones() as usize;
            let (new_child, removed) = map_remove(&children[child_idx], key, hash, shift + BITS);
            if !removed {
                return (node.clone(), false);
            }
            match new_child {
                MapNode::Empty => {
                    let new_bitmap = bitmap & !bit;
                    if new_bitmap == 0 {
                        (MapNode::Empty, true)
                    } else {
                        let mut new_children = children.clone();
                        new_children.remove(child_idx);
                        // Collapse single-child bitmap to its child
                        if new_children.len() == 1 {
                            let only = &*new_children[0];
                            if matches!(only, MapNode::Leaf(..)) {
                                return (only.clone(), true);
                            }
                        }
                        (
                            MapNode::Bitmap {
                                bitmap: new_bitmap,
                                children: new_children,
                            },
                            true,
                        )
                    }
                }
                _ => {
                    let mut new_children = children.clone();
                    new_children[child_idx] = Arc::new(new_child);
                    (
                        MapNode::Bitmap {
                            bitmap: *bitmap,
                            children: new_children,
                        },
                        true,
                    )
                }
            }
        }
    }
}

fn collect_entries<'a, K: Clone + Eq + Hash, V: Clone>(
    node: &'a MapNode<K, V>,
    out: &mut Vec<(&'a K, &'a V)>,
) {
    match node {
        MapNode::Empty => {}
        MapNode::Leaf(_, k, v) => out.push((k, v)),
        MapNode::Collision(_, entries) => {
            for (k, v) in entries {
                out.push((k, v));
            }
        }
        MapNode::Bitmap { children, .. } => {
            for child in children {
                collect_entries(child, out);
            }
        }
    }
}

// ── VersionedStore ────────────────────────────────────────────────

/// A versioned container that tracks state over time.
///
/// Each version is an immutable snapshot. Creating a new version is
/// O(mutation cost), and old versions remain accessible.
#[derive(Clone, Debug)]
pub struct VersionedStore<T: Clone> {
    /// All versions, from oldest to newest.
    versions: Vec<(u64, T)>,
    /// Current version index.
    current: usize,
}

impl<T: Clone> VersionedStore<T> {
    /// Create a versioned store with an initial state.
    #[must_use]
    pub fn new(initial: T, timestamp_ms: u64) -> Self {
        Self {
            versions: vec![(timestamp_ms, initial)],
            current: 0,
        }
    }

    /// Get the current state.
    #[must_use]
    pub fn current(&self) -> &T {
        &self.versions[self.current].1
    }

    /// Get the current version number (0-indexed).
    #[must_use]
    pub fn version_number(&self) -> usize {
        self.current
    }

    /// Total number of versions stored.
    #[must_use]
    pub fn version_count(&self) -> usize {
        self.versions.len()
    }

    /// Push a new version.
    ///
    /// # Panics
    ///
    /// Debug-asserts that `timestamp_ms` is monotonically non-decreasing
    /// (required by [`at_timestamp`] binary search).
    pub fn push(&mut self, state: T, timestamp_ms: u64) {
        debug_assert!(
            self.versions
                .last()
                .map_or(true, |(ts, _)| timestamp_ms >= *ts),
            "VersionedStore::push requires monotonically non-decreasing timestamps"
        );
        self.versions.push((timestamp_ms, state));
        self.current = self.versions.len() - 1;
    }

    /// Get state at a specific version number.
    #[must_use]
    pub fn at_version(&self, version: usize) -> Option<&T> {
        self.versions.get(version).map(|(_, state)| state)
    }

    /// Get state at the version closest to the given timestamp.
    #[must_use]
    pub fn at_timestamp(&self, timestamp_ms: u64) -> Option<&T> {
        if self.versions.is_empty() {
            return None;
        }
        // Binary search for closest timestamp
        let idx = match self
            .versions
            .binary_search_by_key(&timestamp_ms, |(ts, _)| *ts)
        {
            Ok(i) => i,
            Err(i) => {
                if i == 0 {
                    0
                } else if i >= self.versions.len() {
                    self.versions.len() - 1
                } else {
                    // Pick closer of i-1 and i
                    let d1 = timestamp_ms.saturating_sub(self.versions[i - 1].0);
                    let d2 = self.versions[i].0.saturating_sub(timestamp_ms);
                    if d1 <= d2 { i - 1 } else { i }
                }
            }
        };
        Some(&self.versions[idx].1)
    }

    /// Get the timestamp for a version.
    #[must_use]
    pub fn timestamp_at(&self, version: usize) -> Option<u64> {
        self.versions.get(version).map(|(ts, _)| *ts)
    }

    /// Evict versions older than the given timestamp.
    ///
    /// Always keeps at least the current version.
    pub fn evict_before(&mut self, timestamp_ms: u64) {
        let keep_from = self
            .versions
            .iter()
            .position(|(ts, _)| *ts >= timestamp_ms)
            .unwrap_or_else(|| self.versions.len().saturating_sub(1));
        // Don't evict current version
        let keep_from = keep_from.min(self.current);
        if keep_from > 0 {
            self.versions.drain(0..keep_from);
            self.current -= keep_from;
        }
    }

    /// Iterate over all versions with their timestamps.
    pub fn iter_versions(&self) -> impl Iterator<Item = (u64, &T)> {
        self.versions.iter().map(|(ts, state)| (*ts, state))
    }
}

// ── Diff support ──────────────────────────────────────────────────

/// Changes between two PersistentMap versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapDiff<K: Eq + Hash, V> {
    /// Keys added in the newer version.
    pub added: Vec<(K, V)>,
    /// Keys removed from the older version.
    pub removed: Vec<K>,
    /// Keys with changed values (new value).
    pub changed: Vec<(K, V)>,
}

impl<K: Clone + Eq + Hash + Serialize, V: Clone + PartialEq + Serialize> PersistentMap<K, V> {
    /// Compute the diff from `self` (old) to `other` (new).
    #[must_use]
    pub fn diff(&self, other: &Self) -> MapDiff<K, V>
    where
        K: Serialize + for<'de> Deserialize<'de>,
        V: Serialize + for<'de> Deserialize<'de>,
    {
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut changed = Vec::new();

        // Find removed and changed
        for (k, v) in self.entries() {
            match other.get(k) {
                None => removed.push(k.clone()),
                Some(ov) if ov != v => changed.push((k.clone(), ov.clone())),
                _ => {}
            }
        }

        // Find added
        for (k, v) in other.entries() {
            if !self.contains_key(k) {
                added.push((k.clone(), v.clone()));
            }
        }

        MapDiff {
            added,
            removed,
            changed,
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── PersistentVec tests ───────────────────────────────────

    #[test]
    fn vec_empty() {
        let v: PersistentVec<i32> = PersistentVec::new();
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
        assert_eq!(v.get(0), None);
    }

    #[test]
    fn vec_push_and_get() {
        let v = PersistentVec::new().push(10).push(20).push(30);
        assert_eq!(v.len(), 3);
        assert_eq!(v.get(0), Some(&10));
        assert_eq!(v.get(1), Some(&20));
        assert_eq!(v.get(2), Some(&30));
        assert_eq!(v.get(3), None);
    }

    #[test]
    fn vec_structural_sharing() {
        let v1 = PersistentVec::new().push(1).push(2).push(3);
        let v2 = v1.push(4);
        // v1 is unchanged
        assert_eq!(v1.len(), 3);
        assert_eq!(v2.len(), 4);
        assert_eq!(v1.get(0), Some(&1));
        assert_eq!(v2.get(3), Some(&4));
    }

    #[test]
    fn vec_set() {
        let v1 = PersistentVec::new().push(1).push(2).push(3);
        let v2 = v1.set(1, 20).unwrap();
        assert_eq!(v1.get(1), Some(&2)); // original unchanged
        assert_eq!(v2.get(1), Some(&20)); // new version updated
    }

    #[test]
    fn vec_pop() {
        let v1 = PersistentVec::new().push(1).push(2).push(3);
        let (v2, last) = v1.pop().unwrap();
        assert_eq!(last, 3);
        assert_eq!(v2.len(), 2);
        assert_eq!(v1.len(), 3); // original unchanged
    }

    #[test]
    fn vec_iter() {
        let v = PersistentVec::new().push(10).push(20).push(30);
        let items: Vec<_> = v.iter().collect();
        assert_eq!(items, vec![&10, &20, &30]);
    }

    #[test]
    fn vec_large() {
        let mut v = PersistentVec::new();
        for i in 0..1000 {
            v = v.push(i);
        }
        assert_eq!(v.len(), 1000);
        for i in 0..1000 {
            assert_eq!(v.get(i), Some(&i));
        }
    }

    #[test]
    fn vec_equality() {
        let v1 = PersistentVec::new().push(1).push(2);
        let v2 = PersistentVec::new().push(1).push(2);
        let v3 = PersistentVec::new().push(1).push(3);
        assert_eq!(v1, v2);
        assert_ne!(v1, v3);
    }

    #[test]
    fn vec_set_out_of_bounds() {
        let v = PersistentVec::new().push(1);
        assert!(v.set(5, 10).is_none());
    }

    #[test]
    fn vec_pop_empty() {
        let v: PersistentVec<i32> = PersistentVec::new();
        assert!(v.pop().is_none());
    }

    // ── PersistentMap tests ───────────────────────────────────

    #[test]
    fn map_empty() {
        let m: PersistentMap<String, i32> = PersistentMap::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert_eq!(m.get(&"x".to_string()), None);
    }

    #[test]
    fn map_insert_and_get() {
        let m = PersistentMap::new()
            .insert("a".to_string(), 1)
            .insert("b".to_string(), 2)
            .insert("c".to_string(), 3);
        assert_eq!(m.len(), 3);
        assert_eq!(m.get(&"a".to_string()), Some(&1));
        assert_eq!(m.get(&"b".to_string()), Some(&2));
        assert_eq!(m.get(&"c".to_string()), Some(&3));
        assert_eq!(m.get(&"d".to_string()), None);
    }

    #[test]
    fn map_structural_sharing() {
        let m1 = PersistentMap::new()
            .insert("a".to_string(), 1)
            .insert("b".to_string(), 2);
        let m2 = m1.insert("c".to_string(), 3);
        // m1 unchanged
        assert_eq!(m1.len(), 2);
        assert_eq!(m2.len(), 3);
        assert_eq!(m1.get(&"c".to_string()), None);
        assert_eq!(m2.get(&"c".to_string()), Some(&3));
    }

    #[test]
    fn map_update() {
        let m1 = PersistentMap::new().insert("key".to_string(), 1);
        let m2 = m1.insert("key".to_string(), 2);
        assert_eq!(m1.get(&"key".to_string()), Some(&1)); // original
        assert_eq!(m2.get(&"key".to_string()), Some(&2)); // updated
        assert_eq!(m1.len(), 1);
        assert_eq!(m2.len(), 1);
    }

    #[test]
    fn map_remove() {
        let m1 = PersistentMap::new()
            .insert("a".to_string(), 1)
            .insert("b".to_string(), 2);
        let m2 = m1.remove(&"a".to_string());
        assert_eq!(m1.len(), 2); // original unchanged
        assert_eq!(m2.len(), 1);
        assert_eq!(m2.get(&"a".to_string()), None);
        assert_eq!(m2.get(&"b".to_string()), Some(&2));
    }

    #[test]
    fn map_remove_nonexistent() {
        let m = PersistentMap::new().insert("a".to_string(), 1);
        let m2 = m.remove(&"z".to_string());
        assert_eq!(m2.len(), 1);
    }

    #[test]
    fn map_large() {
        let mut m = PersistentMap::new();
        for i in 0..500 {
            m = m.insert(format!("key_{}", i), i);
        }
        assert_eq!(m.len(), 500);
        for i in 0..500 {
            assert_eq!(m.get(&format!("key_{}", i)), Some(&i));
        }
    }

    #[test]
    fn map_from_entries() {
        let entries = vec![("a".to_string(), 1), ("b".to_string(), 2)];
        let m = PersistentMap::from_entries(entries);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(&"a".to_string()), Some(&1));
    }

    #[test]
    fn map_contains_key() {
        let m = PersistentMap::new().insert("x".to_string(), 42);
        assert!(m.contains_key(&"x".to_string()));
        assert!(!m.contains_key(&"y".to_string()));
    }

    #[test]
    fn map_entries() {
        let m = PersistentMap::new()
            .insert("a".to_string(), 1)
            .insert("b".to_string(), 2);
        let entries = m.entries();
        assert_eq!(entries.len(), 2);
    }

    // ── VersionedStore tests ──────────────────────────────────

    #[test]
    fn versioned_basic() {
        let mut store = VersionedStore::new("v0".to_string(), 1000);
        assert_eq!(store.current(), "v0");
        assert_eq!(store.version_count(), 1);

        store.push("v1".to_string(), 2000);
        assert_eq!(store.current(), "v1");
        assert_eq!(store.version_count(), 2);

        assert_eq!(store.at_version(0), Some(&"v0".to_string()));
        assert_eq!(store.at_version(1), Some(&"v1".to_string()));
    }

    #[test]
    fn versioned_timestamp_lookup() {
        let mut store = VersionedStore::new(0, 1000);
        store.push(1, 2000);
        store.push(2, 3000);

        assert_eq!(store.at_timestamp(1000), Some(&0));
        assert_eq!(store.at_timestamp(2000), Some(&1));
        assert_eq!(store.at_timestamp(2400), Some(&1)); // closer to 2000
        assert_eq!(store.at_timestamp(2600), Some(&2)); // closer to 3000
    }

    #[test]
    fn versioned_eviction() {
        let mut store = VersionedStore::new(0, 1000);
        store.push(1, 2000);
        store.push(2, 3000);
        store.push(3, 4000);
        assert_eq!(store.version_count(), 4);

        store.evict_before(3000);
        assert_eq!(store.version_count(), 2); // versions at 3000 and 4000
        assert_eq!(store.current(), &3);
    }

    #[test]
    fn versioned_eviction_keeps_current() {
        let mut store = VersionedStore::new(0, 1000);
        store.push(1, 2000);
        // Current is version 1 (ts=2000)
        store.evict_before(5000); // try to evict everything
        assert!(store.version_count() >= 1); // must keep at least current
        assert_eq!(store.current(), &1);
    }

    #[test]
    fn versioned_iter() {
        let mut store = VersionedStore::new("a", 100);
        store.push("b", 200);
        let versions: Vec<_> = store.iter_versions().collect();
        assert_eq!(versions, vec![(100, &"a"), (200, &"b")]);
    }

    // ── MapDiff tests ─────────────────────────────────────────

    #[test]
    fn diff_identical() {
        let m = PersistentMap::new()
            .insert("a".to_string(), 1)
            .insert("b".to_string(), 2);
        let diff = m.diff(&m);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn diff_addition() {
        let m1 = PersistentMap::new().insert("a".to_string(), 1);
        let m2 = m1.insert("b".to_string(), 2);
        let diff = m1.diff(&m2);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0], ("b".to_string(), 2));
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_removal() {
        let m1 = PersistentMap::new()
            .insert("a".to_string(), 1)
            .insert("b".to_string(), 2);
        let m2 = m1.remove(&"a".to_string());
        let diff = m1.diff(&m2);
        assert_eq!(diff.removed, vec!["a".to_string()]);
        assert!(diff.added.is_empty());
    }

    #[test]
    fn diff_change() {
        let m1 = PersistentMap::new().insert("a".to_string(), 1);
        let m2 = m1.insert("a".to_string(), 99);
        let diff = m1.diff(&m2);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0], ("a".to_string(), 99));
    }

    #[test]
    fn map_integer_keys() {
        let m = PersistentMap::new()
            .insert(1u64, "one")
            .insert(2u64, "two")
            .insert(3u64, "three");
        assert_eq!(m.len(), 3);
        assert_eq!(m.get(&1), Some(&"one"));
        assert_eq!(m.get(&2), Some(&"two"));
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ──────────────────────

    // ── PersistentVec additional coverage ────────────────────

    #[test]
    fn vec_default_trait() {
        let v: PersistentVec<i32> = PersistentVec::default();
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
    }

    #[test]
    fn vec_debug_format() {
        let v = PersistentVec::new().push(1).push(2);
        let dbg = format!("{:?}", v);
        assert!(dbg.contains("PersistentVec"));
    }

    #[test]
    fn vec_from_iter_basic() {
        let v = PersistentVec::from_iter(vec![10, 20, 30]);
        assert_eq!(v.len(), 3);
        assert_eq!(v.get(0), Some(&10));
        assert_eq!(v.get(1), Some(&20));
        assert_eq!(v.get(2), Some(&30));
    }

    #[test]
    fn vec_from_iter_empty() {
        let v = PersistentVec::<i32>::from_iter(std::iter::empty());
        assert!(v.is_empty());
    }

    #[test]
    fn vec_exact_size_iterator() {
        let v = PersistentVec::from_iter(vec![1, 2, 3, 4, 5]);
        let mut iter = v.iter();
        assert_eq!(iter.len(), 5);
        iter.next();
        assert_eq!(iter.len(), 4);
        iter.next();
        iter.next();
        assert_eq!(iter.len(), 2);
    }

    #[test]
    fn vec_clone_is_independent() {
        let v1 = PersistentVec::new().push(1).push(2);
        let v2 = v1.clone();
        let v3 = v2.push(3);
        assert_eq!(v1.len(), 2);
        assert_eq!(v3.len(), 3);
    }

    #[test]
    fn vec_pop_to_empty() {
        let v = PersistentVec::new().push(42);
        let (empty, val) = v.pop().unwrap();
        assert_eq!(val, 42);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(empty.pop().is_none());
    }

    #[test]
    fn vec_set_preserves_others() {
        let v = PersistentVec::from_iter(vec![10, 20, 30, 40, 50]);
        let v2 = v.set(2, 99).unwrap();
        assert_eq!(v2.get(0), Some(&10));
        assert_eq!(v2.get(1), Some(&20));
        assert_eq!(v2.get(2), Some(&99));
        assert_eq!(v2.get(3), Some(&40));
        assert_eq!(v2.get(4), Some(&50));
    }

    #[test]
    fn vec_equality_different_lengths() {
        let v1 = PersistentVec::new().push(1).push(2);
        let v2 = PersistentVec::new().push(1).push(2).push(3);
        assert_ne!(v1, v2);
    }

    #[test]
    fn vec_iter_size_hint() {
        let v = PersistentVec::from_iter(vec![1, 2, 3]);
        let iter = v.iter();
        assert_eq!(iter.size_hint(), (3, Some(3)));
    }

    #[test]
    fn vec_multiple_pops() {
        let v = PersistentVec::from_iter(vec![1, 2, 3, 4]);
        let (v, _) = v.pop().unwrap();
        let (v, _) = v.pop().unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v.get(0), Some(&1));
        assert_eq!(v.get(1), Some(&2));
    }

    // ── PersistentMap additional coverage ────────────────────

    #[test]
    fn map_default_trait() {
        let m: PersistentMap<String, i32> = PersistentMap::default();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn map_debug_format() {
        let m = PersistentMap::new().insert("k".to_string(), 1);
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("PersistentMap"));
    }

    #[test]
    fn map_display_format() {
        let m = PersistentMap::new().insert("a".to_string(), 1);
        let disp = format!("{}", m);
        assert!(disp.starts_with('{'));
        assert!(disp.ends_with('}'));
        assert!(disp.contains("\"a\""));
    }

    #[test]
    fn map_display_empty() {
        let m: PersistentMap<String, i32> = PersistentMap::new();
        assert_eq!(format!("{}", m), "{}");
    }

    #[test]
    fn map_keys_method() {
        let m = PersistentMap::new()
            .insert("x".to_string(), 10)
            .insert("y".to_string(), 20);
        let keys = m.keys();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&&"x".to_string()));
        assert!(keys.contains(&&"y".to_string()));
    }

    #[test]
    fn map_equality_same_entries() {
        let m1 = PersistentMap::new()
            .insert("a".to_string(), 1)
            .insert("b".to_string(), 2);
        let m2 = PersistentMap::new()
            .insert("b".to_string(), 2)
            .insert("a".to_string(), 1);
        assert_eq!(m1, m2);
    }

    #[test]
    fn map_inequality_different_values() {
        let m1 = PersistentMap::new().insert("a".to_string(), 1);
        let m2 = PersistentMap::new().insert("a".to_string(), 2);
        assert_ne!(m1, m2);
    }

    #[test]
    fn map_inequality_different_lengths() {
        let m1 = PersistentMap::new().insert("a".to_string(), 1);
        let m2 = PersistentMap::new()
            .insert("a".to_string(), 1)
            .insert("b".to_string(), 2);
        assert_ne!(m1, m2);
    }

    #[test]
    fn map_from_entries_empty() {
        let m = PersistentMap::<String, i32>::from_entries(Vec::new());
        assert!(m.is_empty());
    }

    #[test]
    fn map_remove_all_entries() {
        let m = PersistentMap::new()
            .insert("a".to_string(), 1)
            .insert("b".to_string(), 2);
        let m = m.remove(&"a".to_string());
        let m = m.remove(&"b".to_string());
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn map_clone_is_independent() {
        let m1 = PersistentMap::new().insert("k".to_string(), 100);
        let m2 = m1.clone();
        let m3 = m2.insert("j".to_string(), 200);
        assert_eq!(m1.len(), 1);
        assert_eq!(m3.len(), 2);
    }

    // ── VersionedStore additional coverage ───────────────────

    #[test]
    fn versioned_at_version_out_of_bounds() {
        let store = VersionedStore::new(42, 1000);
        assert!(store.at_version(0).is_some());
        assert!(store.at_version(1).is_none());
        assert!(store.at_version(999).is_none());
    }

    #[test]
    fn versioned_version_number_after_push() {
        let mut store = VersionedStore::new("a", 100);
        assert_eq!(store.version_number(), 0);
        store.push("b", 200);
        assert_eq!(store.version_number(), 1);
        store.push("c", 300);
        assert_eq!(store.version_number(), 2);
    }

    #[test]
    fn versioned_timestamp_at() {
        let mut store = VersionedStore::new("v0", 1000);
        store.push("v1", 2000);
        store.push("v2", 3000);
        assert_eq!(store.timestamp_at(0), Some(1000));
        assert_eq!(store.timestamp_at(1), Some(2000));
        assert_eq!(store.timestamp_at(2), Some(3000));
        assert_eq!(store.timestamp_at(3), None);
    }

    #[test]
    fn versioned_at_timestamp_before_all() {
        let mut store = VersionedStore::new(0, 5000);
        store.push(1, 6000);
        assert_eq!(store.at_timestamp(1000), Some(&0));
    }

    #[test]
    fn versioned_at_timestamp_after_all() {
        let mut store = VersionedStore::new(0, 1000);
        store.push(1, 2000);
        assert_eq!(store.at_timestamp(9999), Some(&1));
    }

    #[test]
    fn versioned_at_timestamp_exact_match() {
        let mut store = VersionedStore::new(0, 1000);
        store.push(1, 2000);
        store.push(2, 3000);
        assert_eq!(store.at_timestamp(2000), Some(&1));
    }

    #[test]
    fn versioned_evict_before_no_op() {
        let mut store = VersionedStore::new(0, 5000);
        store.push(1, 6000);
        store.evict_before(1000);
        assert_eq!(store.version_count(), 2);
    }

    #[test]
    fn versioned_debug_format() {
        let store = VersionedStore::new(42i32, 1000);
        let dbg = format!("{:?}", store);
        assert!(dbg.contains("VersionedStore"));
    }

    #[test]
    fn versioned_clone_is_independent() {
        let mut store = VersionedStore::new(0, 1000);
        let store2 = store.clone();
        store.push(1, 2000);
        assert_eq!(store.version_count(), 2);
        assert_eq!(store2.version_count(), 1);
    }

    #[test]
    fn versioned_iter_versions_count() {
        let mut store = VersionedStore::new(0, 100);
        store.push(1, 200);
        store.push(2, 300);
        let versions: Vec<_> = store.iter_versions().collect();
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0], (100, &0));
        assert_eq!(versions[2], (300, &2));
    }

    // ── MapDiff additional coverage ─────────────────────────

    #[test]
    fn diff_debug_clone() {
        let m1 = PersistentMap::new().insert("a".to_string(), 1);
        let m2 = m1.insert("b".to_string(), 2);
        let diff = m1.diff(&m2);
        let dbg = format!("{:?}", diff);
        assert!(dbg.contains("MapDiff"));
        let diff2 = diff.clone();
        assert_eq!(diff, diff2);
    }

    #[test]
    fn diff_serde_roundtrip() {
        let m1 = PersistentMap::new().insert("a".to_string(), 1);
        let m2 = m1.remove(&"a".to_string()).insert("b".to_string(), 2);
        let diff = m1.diff(&m2);
        let json = serde_json::to_string(&diff).unwrap();
        let parsed: MapDiff<String, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(diff, parsed);
    }

    #[test]
    fn diff_combined_add_remove_change() {
        let m1 = PersistentMap::new()
            .insert("keep".to_string(), 1)
            .insert("remove_me".to_string(), 2)
            .insert("change_me".to_string(), 3);
        let m2 = PersistentMap::new()
            .insert("keep".to_string(), 1)
            .insert("change_me".to_string(), 99)
            .insert("added".to_string(), 4);
        let diff = m1.diff(&m2);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.changed.len(), 1);
        assert!(diff.added.iter().any(|(k, _)| k == "added"));
        assert!(diff.removed.contains(&"remove_me".to_string()));
        assert!(
            diff.changed
                .iter()
                .any(|(k, v)| k == "change_me" && *v == 99)
        );
    }

    #[test]
    fn diff_empty_maps() {
        let m1: PersistentMap<String, i32> = PersistentMap::new();
        let m2: PersistentMap<String, i32> = PersistentMap::new();
        let diff = m1.diff(&m2);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn diff_from_empty_to_populated() {
        let m1: PersistentMap<String, i32> = PersistentMap::new();
        let m2 = PersistentMap::new()
            .insert("x".to_string(), 10)
            .insert("y".to_string(), 20);
        let diff = m1.diff(&m2);
        assert_eq!(diff.added.len(), 2);
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn diff_from_populated_to_empty() {
        let m1 = PersistentMap::new()
            .insert("x".to_string(), 10)
            .insert("y".to_string(), 20);
        let m2: PersistentMap<String, i32> = PersistentMap::new();
        let diff = m1.diff(&m2);
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed.len(), 2);
        assert!(diff.changed.is_empty());
    }
}
