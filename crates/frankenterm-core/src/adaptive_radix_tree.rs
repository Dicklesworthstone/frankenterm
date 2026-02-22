//! Adaptive Radix Tree (ART) — space-efficient sorted key-value trie.
//!
//! An adaptive radix tree stores byte-string keys with values, using node
//! sizes that adapt to the actual branching factor at each level. This
//! gives both the cache efficiency of a sorted array for sparse branches
//! and the O(1) lookup of a 256-entry array for dense branches.
//!
//! # Design
//!
//! ```text
//!                  [Node16: "com"]
//!                 /       |        \
//!          [Node4: "m"]  "p"  [Node4: "s"]
//!          /     \              |
//!        "and"  "it"          "earch"
//!         =v1    =v2            =v3
//! ```
//!
//! Four node types, chosen by child count:
//! - **Node4**: up to 4 children (linear scan)
//! - **Node16**: 5–16 children (sorted keys)
//! - **Node48**: 17–48 children (key index → child slot)
//! - **Node256**: 49–256 children (direct byte-indexed array)
//!
//! Path compression collapses chains of single-child nodes into a
//! compressed prefix stored at the parent.
//!
//! # Use Cases in FrankenTerm
//!
//! - **Command history prefix search**: Find all commands starting with "git".
//! - **Event deduplication**: Quickly check if an event key prefix exists.
//! - **Session index**: Sorted iteration over session IDs by prefix.
//! - **Auto-complete**: Efficient prefix enumeration for terminal UIs.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Constants ──────────────────────────────────────────────────────────

const NODE4_MAX: usize = 4;
const NODE16_MAX: usize = 16;
const NODE48_MAX: usize = 48;

// ── Inner Node Types ───────────────────────────────────────────────────

/// A node in the adaptive radix tree (arena-allocated).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtNode<V> {
    /// Compressed path prefix (path compression / lazy expansion).
    prefix: Vec<u8>,
    /// Optional value stored at this node.
    value: Option<V>,
    /// Child storage, adapts to branching factor.
    inner: InnerNode,
}

/// Adaptive inner node storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum InnerNode {
    /// Leaf node with no children.
    Empty,
    /// Up to 4 children: sorted (key, child_index) pairs.
    Node4 { keys: Vec<u8>, children: Vec<usize> },
    /// 5–16 children: sorted (key, child_index) pairs.
    Node16 { keys: Vec<u8>, children: Vec<usize> },
    /// 17–48 children: index[byte] → slot, slots[slot] → child_index.
    /// index entries are 0xFF for unused bytes.
    Node48 {
        index: Vec<u8>,
        children: Vec<Option<usize>>,
        count: usize,
    },
    /// 49–256 children: direct byte-indexed array.
    Node256 { children: Vec<Option<usize>> },
}

impl InnerNode {
    fn child_count(&self) -> usize {
        match self {
            InnerNode::Empty => 0,
            InnerNode::Node4 { keys, .. } => keys.len(),
            InnerNode::Node16 { keys, .. } => keys.len(),
            InnerNode::Node48 { count, .. } => *count,
            InnerNode::Node256 { children } => children.iter().filter(|c| c.is_some()).count(),
        }
    }

    fn find_child(&self, byte: u8) -> Option<usize> {
        match self {
            InnerNode::Empty => None,
            InnerNode::Node4 { keys, children } | InnerNode::Node16 { keys, children } => keys
                .iter()
                .position(|&k| k == byte)
                .map(|pos| children[pos]),
            InnerNode::Node48 {
                index, children, ..
            } => {
                let slot = index[byte as usize];
                if slot == 0xFF {
                    None
                } else {
                    children[slot as usize]
                }
            }
            InnerNode::Node256 { children } => children[byte as usize],
        }
    }

    fn insert_child(&mut self, byte: u8, child_idx: usize) {
        match self {
            InnerNode::Empty => {
                *self = InnerNode::Node4 {
                    keys: vec![byte],
                    children: vec![child_idx],
                };
            }
            InnerNode::Node4 { keys, children } => {
                let pos = keys.iter().position(|&k| k >= byte).unwrap_or(keys.len());
                keys.insert(pos, byte);
                children.insert(pos, child_idx);
            }
            InnerNode::Node16 { keys, children } => {
                let pos = keys.iter().position(|&k| k >= byte).unwrap_or(keys.len());
                keys.insert(pos, byte);
                children.insert(pos, child_idx);
            }
            InnerNode::Node48 {
                index,
                children,
                count,
            } => {
                // Find first empty slot
                let slot = children.iter().position(|c| c.is_none()).unwrap();
                index[byte as usize] = slot as u8;
                children[slot] = Some(child_idx);
                *count += 1;
            }
            InnerNode::Node256 { children } => {
                children[byte as usize] = Some(child_idx);
            }
        }
    }

    fn remove_child(&mut self, byte: u8) {
        match self {
            InnerNode::Empty => {}
            InnerNode::Node4 { keys, children } | InnerNode::Node16 { keys, children } => {
                if let Some(pos) = keys.iter().position(|&k| k == byte) {
                    keys.remove(pos);
                    children.remove(pos);
                }
            }
            InnerNode::Node48 {
                index,
                children,
                count,
            } => {
                let slot = index[byte as usize];
                if slot != 0xFF {
                    children[slot as usize] = None;
                    index[byte as usize] = 0xFF;
                    *count -= 1;
                }
            }
            InnerNode::Node256 { children } => {
                children[byte as usize] = None;
            }
        }
    }

    /// Grow to the next node size if at capacity.
    /// Call BEFORE insert_child when node is full.
    fn maybe_grow(&mut self) {
        match self {
            InnerNode::Node4 { keys, children } if keys.len() >= NODE4_MAX => {
                *self = InnerNode::Node16 {
                    keys: std::mem::take(keys),
                    children: std::mem::take(children),
                };
            }
            InnerNode::Node16 { keys, children } if keys.len() >= NODE16_MAX => {
                let mut index = vec![0xFFu8; 256];
                let mut new_children = vec![None; NODE48_MAX];
                for (slot, (&k, &c)) in keys.iter().zip(children.iter()).enumerate() {
                    index[k as usize] = slot as u8;
                    new_children[slot] = Some(c);
                }
                let count = keys.len();
                *self = InnerNode::Node48 {
                    index,
                    children: new_children,
                    count,
                };
            }
            InnerNode::Node48 {
                index,
                children,
                count,
            } if *count >= NODE48_MAX => {
                let mut new_children = vec![None; 256];
                for (byte, &slot) in index.iter().enumerate() {
                    if slot != 0xFF {
                        new_children[byte] = children[slot as usize];
                    }
                }
                *self = InnerNode::Node256 {
                    children: new_children,
                };
            }
            _ => {}
        }
    }

    /// Shrink to a smaller node size if under-utilized.
    fn maybe_shrink(&mut self) {
        match self {
            InnerNode::Node256 { children } => {
                let count = children.iter().filter(|c| c.is_some()).count();
                if count <= NODE48_MAX {
                    let mut index = vec![0xFFu8; 256];
                    let mut new_children = vec![None; NODE48_MAX];
                    let mut slot = 0;
                    for (byte, child) in children.iter().enumerate() {
                        if let Some(c) = child {
                            index[byte] = slot as u8;
                            new_children[slot] = Some(*c);
                            slot += 1;
                        }
                    }
                    *self = InnerNode::Node48 {
                        index,
                        children: new_children,
                        count,
                    };
                }
            }
            InnerNode::Node48 {
                index,
                children,
                count,
            } => {
                if *count <= NODE16_MAX {
                    let mut keys = Vec::with_capacity(*count);
                    let mut new_children = Vec::with_capacity(*count);
                    for (byte, &slot) in index.iter().enumerate() {
                        if slot != 0xFF {
                            if let Some(c) = children[slot as usize] {
                                keys.push(byte as u8);
                                new_children.push(c);
                            }
                        }
                    }
                    *self = InnerNode::Node16 {
                        keys,
                        children: new_children,
                    };
                }
            }
            InnerNode::Node16 { keys, children } => {
                if keys.len() <= NODE4_MAX {
                    *self = InnerNode::Node4 {
                        keys: std::mem::take(keys),
                        children: std::mem::take(children),
                    };
                }
            }
            InnerNode::Node4 { keys, .. } => {
                if keys.is_empty() {
                    *self = InnerNode::Empty;
                }
            }
            InnerNode::Empty => {}
        }
    }

    /// Iterate over (key_byte, child_index) pairs in sorted order.
    fn children_sorted(&self) -> Vec<(u8, usize)> {
        match self {
            InnerNode::Empty => Vec::new(),
            InnerNode::Node4 { keys, children } | InnerNode::Node16 { keys, children } => keys
                .iter()
                .zip(children.iter())
                .map(|(&k, &c)| (k, c))
                .collect(),
            InnerNode::Node48 {
                index, children, ..
            } => {
                let mut result = Vec::new();
                for (byte, &slot) in index.iter().enumerate() {
                    if slot != 0xFF {
                        if let Some(c) = children[slot as usize] {
                            result.push((byte as u8, c));
                        }
                    }
                }
                result
            }
            InnerNode::Node256 { children } => children
                .iter()
                .enumerate()
                .filter_map(|(byte, c)| c.map(|idx| (byte as u8, idx)))
                .collect(),
        }
    }
}

// ── Adaptive Radix Tree ────────────────────────────────────────────────

/// An adaptive radix tree mapping byte-string keys to values.
///
/// Provides O(k) search, insert, and delete operations where k is the
/// key length. Node sizes adapt to actual branching factor for memory
/// efficiency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveRadixTree<V> {
    nodes: Vec<ArtNode<V>>,
    root: Option<usize>,
    len: usize,
    free: Vec<usize>,
}

impl<V> Default for AdaptiveRadixTree<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> AdaptiveRadixTree<V> {
    /// Create an empty ART.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
            len: 0,
            free: Vec::new(),
        }
    }

    /// Return the number of key-value pairs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Check if the tree is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Insert a key-value pair. Returns the previous value if the key existed.
    pub fn insert(&mut self, key: &[u8], value: V) -> Option<V> {
        if let Some(root) = self.root {
            let result = self.insert_recursive(root, key, 0, value);
            if result.is_none() {
                self.len += 1;
            }
            result
        } else {
            let idx = self.alloc_node(key.to_vec(), Some(value));
            self.root = Some(idx);
            self.len += 1;
            None
        }
    }

    /// Look up a value by key.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&V> {
        let mut current = self.root?;
        let mut depth = 0;

        loop {
            let node = &self.nodes[current];

            // Check prefix match
            let prefix_len = node.prefix.len();
            if depth + prefix_len > key.len() {
                return None;
            }
            if node.prefix[..] != key[depth..depth + prefix_len] {
                return None;
            }
            depth += prefix_len;

            if depth == key.len() {
                return node.value.as_ref();
            }

            // Follow child
            let byte = key[depth];
            match node.inner.find_child(byte) {
                Some(child) => {
                    current = child;
                    depth += 1;
                }
                None => return None,
            }
        }
    }

    /// Check if a key exists.
    #[must_use]
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    /// Remove a key-value pair. Returns the value if the key existed.
    pub fn remove(&mut self, key: &[u8]) -> Option<V> {
        let root = self.root?;
        let (new_root, removed) = self.remove_recursive(root, key, 0);
        if new_root != Some(root) {
            self.free_node(root);
        }
        self.root = new_root;
        if removed.is_some() {
            self.len -= 1;
        }
        removed
    }

    /// Find all key-value pairs whose keys start with the given prefix.
    ///
    /// Returns (key, &value) pairs in sorted order.
    #[must_use]
    pub fn prefix_search(&self, prefix: &[u8]) -> Vec<(Vec<u8>, &V)> {
        let mut results = Vec::new();
        if let Some(root) = self.root {
            self.prefix_search_recursive(root, prefix, 0, Vec::new(), &mut results);
        }
        results
    }

    /// Iterate over all key-value pairs in lexicographic order.
    #[must_use]
    #[allow(clippy::iter_not_returning_iterator)]
    pub fn iter(&self) -> Vec<(Vec<u8>, &V)> {
        let mut results = Vec::new();
        if let Some(root) = self.root {
            self.collect_all(root, Vec::new(), &mut results);
        }
        results
    }

    /// Return the number of nodes in the tree (for diagnostics).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    // ── Internal: Node allocation ──────────────────────────────────

    fn alloc_node(&mut self, prefix: Vec<u8>, value: Option<V>) -> usize {
        let new_node = ArtNode {
            prefix,
            value,
            inner: InnerNode::Empty,
        };
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = new_node;
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(new_node);
            idx
        }
    }

    fn free_node(&mut self, idx: usize) {
        self.nodes[idx].value = None;
        self.nodes[idx].prefix.clear();
        self.nodes[idx].inner = InnerNode::Empty;
        self.free.push(idx);
    }

    // ── Internal: Recursive insert ─────────────────────────────────

    fn insert_recursive(
        &mut self,
        node_idx: usize,
        key: &[u8],
        depth: usize,
        value: V,
    ) -> Option<V> {
        let prefix_len = self.nodes[node_idx].prefix.len();

        // Find mismatch point in prefix
        let mismatch = self.prefix_mismatch(node_idx, key, depth);

        if mismatch < prefix_len {
            // Split this node at the mismatch point
            self.split_node(node_idx, mismatch, key, depth, value);
            return None;
        }

        let new_depth = depth + prefix_len;

        if new_depth == key.len() {
            // Key matches this node exactly — update value
            let old = self.nodes[node_idx].value.take();
            self.nodes[node_idx].value = Some(value);
            return old;
        }

        let byte = key[new_depth];

        if let Some(child) = self.nodes[node_idx].inner.find_child(byte) {
            // Recurse into existing child
            self.insert_recursive(child, key, new_depth + 1, value)
        } else {
            // Create new child — grow first if at capacity
            let remaining = key[new_depth + 1..].to_vec();
            let child_idx = self.alloc_node(remaining, Some(value));
            self.nodes[node_idx].inner.maybe_grow();
            self.nodes[node_idx].inner.insert_child(byte, child_idx);
            None
        }
    }

    fn prefix_mismatch(&self, node_idx: usize, key: &[u8], depth: usize) -> usize {
        let prefix = &self.nodes[node_idx].prefix;
        let key_remaining = &key[depth..];
        let max_check = prefix.len().min(key_remaining.len());
        for i in 0..max_check {
            if prefix[i] != key_remaining[i] {
                return i;
            }
        }
        max_check.min(prefix.len())
    }

    fn split_node(&mut self, node_idx: usize, mismatch: usize, key: &[u8], depth: usize, value: V) {
        let old_prefix = self.nodes[node_idx].prefix.clone();
        let old_suffix = old_prefix[mismatch + 1..].to_vec();
        let old_byte = old_prefix[mismatch];

        // Extract value before alloc_node to avoid double borrow
        let old_value = self.nodes[node_idx].value.take();
        let old_child_idx = self.alloc_node(old_suffix, old_value);
        // Move children from original node to old child
        let old_inner = std::mem::replace(&mut self.nodes[node_idx].inner, InnerNode::Empty);
        self.nodes[old_child_idx].inner = old_inner;

        // Update the current node: truncate prefix to mismatch point
        self.nodes[node_idx].prefix = old_prefix[..mismatch].to_vec();
        self.nodes[node_idx]
            .inner
            .insert_child(old_byte, old_child_idx);

        let new_depth = depth + mismatch;
        if new_depth == key.len() {
            // The new key ends at the split point
            self.nodes[node_idx].value = Some(value);
        } else {
            let new_byte = key[new_depth];
            let new_suffix = key[new_depth + 1..].to_vec();
            let new_child_idx = self.alloc_node(new_suffix, Some(value));
            self.nodes[node_idx]
                .inner
                .insert_child(new_byte, new_child_idx);
            self.nodes[node_idx].inner.maybe_grow();
        }
    }

    // ── Internal: Recursive remove ─────────────────────────────────

    fn remove_recursive(
        &mut self,
        node_idx: usize,
        key: &[u8],
        depth: usize,
    ) -> (Option<usize>, Option<V>) {
        let prefix_len = self.nodes[node_idx].prefix.len();

        // Check prefix match
        if depth + prefix_len > key.len() {
            return (Some(node_idx), None);
        }
        if self.nodes[node_idx].prefix[..] != key[depth..depth + prefix_len] {
            return (Some(node_idx), None);
        }

        let new_depth = depth + prefix_len;

        if new_depth == key.len() {
            // Found the node — remove its value
            let removed = self.nodes[node_idx].value.take();
            if removed.is_none() {
                return (Some(node_idx), None);
            }

            // If no children, this node can be removed
            if self.nodes[node_idx].inner.child_count() == 0 {
                return (None, removed);
            }

            // If exactly one child, merge with it
            if self.nodes[node_idx].inner.child_count() == 1 {
                let (byte, child) = self.nodes[node_idx].inner.children_sorted()[0];
                let mut merged_prefix = self.nodes[node_idx].prefix.clone();
                merged_prefix.push(byte);
                merged_prefix.extend_from_slice(&self.nodes[child].prefix);
                self.nodes[child].prefix = merged_prefix;
                return (Some(child), removed);
            }

            return (Some(node_idx), removed);
        }

        let byte = key[new_depth];
        let child = match self.nodes[node_idx].inner.find_child(byte) {
            Some(c) => c,
            None => return (Some(node_idx), None),
        };

        let (new_child, removed) = self.remove_recursive(child, key, new_depth + 1);

        match new_child {
            Some(new_c) => {
                if new_c != child {
                    // Child was replaced (merged)
                    self.nodes[node_idx].inner.remove_child(byte);
                    // Re-derive the byte from new child's prefix
                    // The new child already has the merged prefix
                    self.nodes[node_idx].inner.insert_child(byte, new_c);
                    self.free_node(child);
                }
            }
            None => {
                // Child was removed
                self.nodes[node_idx].inner.remove_child(byte);
                self.nodes[node_idx].inner.maybe_shrink();
                self.free_node(child);

                // If this node has no value and exactly one child, merge
                if self.nodes[node_idx].value.is_none()
                    && self.nodes[node_idx].inner.child_count() == 1
                {
                    let (remaining_byte, remaining_child) =
                        self.nodes[node_idx].inner.children_sorted()[0];
                    let mut merged_prefix = self.nodes[node_idx].prefix.clone();
                    merged_prefix.push(remaining_byte);
                    merged_prefix.extend_from_slice(&self.nodes[remaining_child].prefix);
                    self.nodes[remaining_child].prefix = merged_prefix;
                    return (Some(remaining_child), removed);
                }

                // If no children and no value, remove this node too
                if self.nodes[node_idx].value.is_none()
                    && self.nodes[node_idx].inner.child_count() == 0
                {
                    return (None, removed);
                }
            }
        }

        (Some(node_idx), removed)
    }

    // ── Internal: Prefix search ────────────────────────────────────

    fn prefix_search_recursive<'a>(
        &'a self,
        node_idx: usize,
        prefix: &[u8],
        depth: usize,
        key_so_far: Vec<u8>,
        results: &mut Vec<(Vec<u8>, &'a V)>,
    ) {
        let node = &self.nodes[node_idx];
        let prefix_remaining = &prefix[depth..];

        // Match as much of the node prefix as possible
        let match_len = node.prefix.len().min(prefix_remaining.len());
        if node.prefix[..match_len] != prefix_remaining[..match_len] {
            return; // Mismatch
        }

        if prefix_remaining.len() <= node.prefix.len() {
            // The search prefix is fully consumed — collect everything under this node.
            // collect_all extends key_so_far with node.prefix, so don't extend here.
            self.collect_all(node_idx, key_so_far, results);
            return;
        }

        // Prefix extends beyond this node — follow the child
        let mut key_so_far = key_so_far;
        key_so_far.extend_from_slice(&node.prefix);
        let next_depth = depth + node.prefix.len();
        let byte = prefix[next_depth];
        key_so_far.push(byte);

        if let Some(child) = node.inner.find_child(byte) {
            self.prefix_search_recursive(child, prefix, next_depth + 1, key_so_far, results);
        }
    }

    fn collect_all<'a>(
        &'a self,
        node_idx: usize,
        key_so_far: Vec<u8>,
        results: &mut Vec<(Vec<u8>, &'a V)>,
    ) {
        let node = &self.nodes[node_idx];
        let mut current_key = key_so_far;
        current_key.extend_from_slice(&node.prefix);

        if let Some(ref val) = node.value {
            results.push((current_key.clone(), val));
        }

        for (byte, child_idx) in node.inner.children_sorted() {
            let mut child_key = current_key.clone();
            child_key.push(byte);
            self.collect_all(child_idx, child_key, results);
        }
    }
}

// ── Convenience ────────────────────────────────────────────────────────

impl<V> AdaptiveRadixTree<V> {
    /// Insert a string key.
    pub fn insert_str(&mut self, key: &str, value: V) -> Option<V> {
        self.insert(key.as_bytes(), value)
    }

    /// Look up by string key.
    #[must_use]
    pub fn get_str(&self, key: &str) -> Option<&V> {
        self.get(key.as_bytes())
    }

    /// Check if a string key exists.
    #[must_use]
    pub fn contains_str(&self, key: &str) -> bool {
        self.contains_key(key.as_bytes())
    }

    /// Remove by string key.
    pub fn remove_str(&mut self, key: &str) -> Option<V> {
        self.remove(key.as_bytes())
    }

    /// Prefix search with string prefix.
    #[must_use]
    pub fn prefix_search_str(&self, prefix: &str) -> Vec<(Vec<u8>, &V)> {
        self.prefix_search(prefix.as_bytes())
    }
}

// ── FromIterator ───────────────────────────────────────────────────────

impl<V> FromIterator<(Vec<u8>, V)> for AdaptiveRadixTree<V> {
    fn from_iter<I: IntoIterator<Item = (Vec<u8>, V)>>(iter: I) -> Self {
        let mut tree = Self::new();
        for (key, value) in iter {
            tree.insert(&key, value);
        }
        tree
    }
}

// ── Display ────────────────────────────────────────────────────────────

impl<V> fmt::Display for AdaptiveRadixTree<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "AdaptiveRadixTree({} entries, {} nodes)",
            self.len,
            self.nodes.len()
        )
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree() {
        let tree: AdaptiveRadixTree<i32> = AdaptiveRadixTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
        assert!(tree.get(b"hello").is_none());
        assert!(!tree.contains_key(b"hello"));
    }

    #[test]
    fn single_insert_and_get() {
        let mut tree = AdaptiveRadixTree::new();
        assert!(tree.insert(b"hello", 42).is_none());
        assert_eq!(tree.len(), 1);
        assert_eq!(*tree.get(b"hello").unwrap(), 42);
        assert!(tree.get(b"hell").is_none());
        assert!(tree.get(b"hello!").is_none());
    }

    #[test]
    fn multiple_inserts() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"apple", 1);
        tree.insert(b"app", 2);
        tree.insert(b"application", 3);
        tree.insert(b"banana", 4);

        assert_eq!(tree.len(), 4);
        assert_eq!(*tree.get(b"apple").unwrap(), 1);
        assert_eq!(*tree.get(b"app").unwrap(), 2);
        assert_eq!(*tree.get(b"application").unwrap(), 3);
        assert_eq!(*tree.get(b"banana").unwrap(), 4);
    }

    #[test]
    fn overwrite_value() {
        let mut tree = AdaptiveRadixTree::new();
        assert!(tree.insert(b"key", 1).is_none());
        assert_eq!(tree.insert(b"key", 2), Some(1));
        assert_eq!(tree.len(), 1);
        assert_eq!(*tree.get(b"key").unwrap(), 2);
    }

    #[test]
    fn prefix_search_basic() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"git status", 1);
        tree.insert(b"git commit", 2);
        tree.insert(b"git push", 3);
        tree.insert(b"grep foo", 4);
        tree.insert(b"ls -la", 5);

        let git_results = tree.prefix_search(b"git");
        assert_eq!(git_results.len(), 3);

        let grep_results = tree.prefix_search(b"grep");
        assert_eq!(grep_results.len(), 1);

        let all_results = tree.prefix_search(b"");
        assert_eq!(all_results.len(), 5);
    }

    #[test]
    fn prefix_search_exact_match() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"hello", 1);
        tree.insert(b"hello world", 2);

        let results = tree.prefix_search(b"hello");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn remove_basic() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"hello", 1);
        tree.insert(b"world", 2);

        assert_eq!(tree.remove(b"hello"), Some(1));
        assert_eq!(tree.len(), 1);
        assert!(tree.get(b"hello").is_none());
        assert_eq!(*tree.get(b"world").unwrap(), 2);
    }

    #[test]
    fn remove_nonexistent() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"hello", 1);
        assert!(tree.remove(b"world").is_none());
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn remove_prefix_key() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"app", 1);
        tree.insert(b"apple", 2);
        tree.insert(b"application", 3);

        assert_eq!(tree.remove(b"app"), Some(1));
        assert_eq!(tree.len(), 2);
        assert!(tree.get(b"app").is_none());
        assert_eq!(*tree.get(b"apple").unwrap(), 2);
        assert_eq!(*tree.get(b"application").unwrap(), 3);
    }

    #[test]
    fn empty_key() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"", 0);
        tree.insert(b"a", 1);

        assert_eq!(tree.len(), 2);
        assert_eq!(*tree.get(b"").unwrap(), 0);
        assert_eq!(*tree.get(b"a").unwrap(), 1);
    }

    #[test]
    fn string_helpers() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert_str("hello", 1);
        assert_eq!(*tree.get_str("hello").unwrap(), 1);
        assert!(tree.contains_str("hello"));
        assert_eq!(tree.remove_str("hello"), Some(1));
        assert!(!tree.contains_str("hello"));
    }

    #[test]
    fn iterator_sorted_order() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"charlie", 3);
        tree.insert(b"alpha", 1);
        tree.insert(b"bravo", 2);

        let items = tree.iter();
        assert_eq!(items.len(), 3);

        // Should be in lexicographic order
        for w in items.windows(2) {
            assert!(w[0].0 <= w[1].0);
        }
    }

    #[test]
    fn from_iterator() {
        let tree: AdaptiveRadixTree<i32> = vec![
            (b"apple".to_vec(), 1),
            (b"banana".to_vec(), 2),
            (b"cherry".to_vec(), 3),
        ]
        .into_iter()
        .collect();

        assert_eq!(tree.len(), 3);
        assert_eq!(*tree.get(b"banana").unwrap(), 2);
    }

    #[test]
    fn node_growth_to_node16() {
        let mut tree = AdaptiveRadixTree::new();
        // Insert keys that diverge at position 0 to fill a single node
        for b in 0..10u8 {
            tree.insert(&[b], b as i32);
        }
        assert_eq!(tree.len(), 10);
        for b in 0..10u8 {
            assert_eq!(*tree.get(&[b]).unwrap(), b as i32);
        }
    }

    #[test]
    fn node_growth_to_node48() {
        let mut tree = AdaptiveRadixTree::new();
        for b in 0..30u8 {
            tree.insert(&[b], b as i32);
        }
        assert_eq!(tree.len(), 30);
        for b in 0..30u8 {
            assert_eq!(*tree.get(&[b]).unwrap(), b as i32);
        }
    }

    #[test]
    fn node_growth_to_node256() {
        let mut tree = AdaptiveRadixTree::new();
        for b in 0..=255u8 {
            tree.insert(&[b], b as i32);
        }
        assert_eq!(tree.len(), 256);
        for b in 0..=255u8 {
            assert_eq!(*tree.get(&[b]).unwrap(), b as i32);
        }
    }

    #[test]
    fn shared_prefix_splitting() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"abcdef", 1);
        tree.insert(b"abcxyz", 2);

        assert_eq!(*tree.get(b"abcdef").unwrap(), 1);
        assert_eq!(*tree.get(b"abcxyz").unwrap(), 2);
        assert!(tree.get(b"abc").is_none());
    }

    #[test]
    fn serde_roundtrip() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"hello", 1);
        tree.insert(b"world", 2);
        tree.insert(b"help", 3);

        let json = serde_json::to_string(&tree).unwrap();
        let restored: AdaptiveRadixTree<i32> = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 3);
        assert_eq!(*restored.get(b"hello").unwrap(), 1);
        assert_eq!(*restored.get(b"world").unwrap(), 2);
        assert_eq!(*restored.get(b"help").unwrap(), 3);
    }

    #[test]
    fn display_format() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"a", 1);
        tree.insert(b"b", 2);
        let s = format!("{}", tree);
        assert!(s.contains("2 entries"));
    }

    #[test]
    fn default_is_empty() {
        let tree: AdaptiveRadixTree<()> = AdaptiveRadixTree::default();
        assert!(tree.is_empty());
    }

    #[test]
    fn long_keys() {
        let mut tree = AdaptiveRadixTree::new();
        let long_key = vec![42u8; 1000];
        tree.insert(&long_key, 1);
        assert_eq!(*tree.get(&long_key).unwrap(), 1);

        let mut similar = long_key.clone();
        similar[999] = 43;
        tree.insert(&similar, 2);
        assert_eq!(*tree.get(&similar).unwrap(), 2);
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn binary_keys() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(&[0, 0, 0], 1);
        tree.insert(&[0, 0, 1], 2);
        tree.insert(&[255, 255, 255], 3);

        assert_eq!(tree.len(), 3);
        assert_eq!(*tree.get(&[0, 0, 0]).unwrap(), 1);
        assert_eq!(*tree.get(&[255, 255, 255]).unwrap(), 3);
    }

    // ── Additional tests ──────────────────────────────────────────────

    #[test]
    fn get_nonexistent_from_empty() {
        let tree: AdaptiveRadixTree<i32> = AdaptiveRadixTree::new();
        assert!(tree.get(b"anything").is_none());
    }

    #[test]
    fn contains_key_basic() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"hello", 1);
        assert!(tree.contains_key(b"hello"));
        assert!(!tree.contains_key(b"hell"));
        assert!(!tree.contains_key(b"helloo"));
    }

    #[test]
    fn remove_from_empty() {
        let mut tree: AdaptiveRadixTree<i32> = AdaptiveRadixTree::new();
        assert!(tree.remove(b"anything").is_none());
    }

    #[test]
    fn remove_all_keys() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"a", 1);
        tree.insert(b"b", 2);
        tree.insert(b"c", 3);
        assert_eq!(tree.remove(b"a"), Some(1));
        assert_eq!(tree.remove(b"b"), Some(2));
        assert_eq!(tree.remove(b"c"), Some(3));
        assert!(tree.is_empty());
    }

    #[test]
    fn remove_then_reinsert() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"hello", 1);
        tree.remove(b"hello");
        assert!(tree.get(b"hello").is_none());
        tree.insert(b"hello", 2);
        assert_eq!(*tree.get(b"hello").unwrap(), 2);
    }

    #[test]
    fn insert_returns_old_on_overwrite() {
        let mut tree = AdaptiveRadixTree::new();
        assert!(tree.insert(b"key", 1).is_none());
        assert_eq!(tree.insert(b"key", 2), Some(1));
        assert_eq!(tree.insert(b"key", 3), Some(2));
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn prefix_search_empty_tree() {
        let tree: AdaptiveRadixTree<i32> = AdaptiveRadixTree::new();
        assert!(tree.prefix_search(b"hello").is_empty());
    }

    #[test]
    fn prefix_search_no_match() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"hello", 1);
        assert!(tree.prefix_search(b"world").is_empty());
    }

    #[test]
    fn prefix_search_empty_prefix_returns_all() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"a", 1);
        tree.insert(b"b", 2);
        tree.insert(b"c", 3);
        let results = tree.prefix_search(b"");
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn prefix_search_str_helper() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert_str("git commit", 1);
        tree.insert_str("git push", 2);
        tree.insert_str("grep", 3);
        let results = tree.prefix_search_str("git");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn iter_empty() {
        let tree: AdaptiveRadixTree<i32> = AdaptiveRadixTree::new();
        assert!(tree.iter().is_empty());
    }

    #[test]
    fn iter_single() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"only", 42);
        let items = tree.iter();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, b"only");
        assert_eq!(*items[0].1, 42);
    }

    #[test]
    fn iter_sorted_with_shared_prefix() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"abc", 1);
        tree.insert(b"abd", 2);
        tree.insert(b"abe", 3);
        let items = tree.iter();
        assert_eq!(items.len(), 3);
        // Verify sorted
        for w in items.windows(2) {
            assert!(w[0].0 <= w[1].0);
        }
    }

    #[test]
    fn node_count_grows() {
        let mut tree = AdaptiveRadixTree::new();
        assert_eq!(tree.node_count(), 0);
        tree.insert(b"a", 1);
        assert!(tree.node_count() > 0);
        tree.insert(b"b", 2);
        assert!(tree.node_count() >= 2);
    }

    #[test]
    fn remove_leaf_node_cleans_up() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"abc", 1);
        tree.insert(b"abd", 2);
        assert_eq!(tree.remove(b"abc"), Some(1));
        assert!(tree.get(b"abc").is_none());
        assert_eq!(*tree.get(b"abd").unwrap(), 2);
        assert_eq!(tree.len(), 1);
    }

    #[test]
    fn remove_middle_node() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"a", 1);
        tree.insert(b"ab", 2);
        tree.insert(b"abc", 3);
        assert_eq!(tree.remove(b"ab"), Some(2));
        assert_eq!(*tree.get(b"a").unwrap(), 1);
        assert_eq!(*tree.get(b"abc").unwrap(), 3);
    }

    #[test]
    fn serde_roundtrip_empty() {
        let tree: AdaptiveRadixTree<i32> = AdaptiveRadixTree::new();
        let json = serde_json::to_string(&tree).unwrap();
        let restored: AdaptiveRadixTree<i32> = serde_json::from_str(&json).unwrap();
        assert!(restored.is_empty());
    }

    #[test]
    fn display_empty() {
        let tree: AdaptiveRadixTree<i32> = AdaptiveRadixTree::new();
        let s = format!("{}", tree);
        assert!(s.contains("0 entries"));
    }

    #[test]
    fn clone_independence() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert(b"hello", 1);
        let mut clone = tree.clone();
        clone.insert(b"world", 2);
        assert_eq!(tree.len(), 1);
        assert_eq!(clone.len(), 2);
    }

    #[test]
    fn many_keys_with_shared_prefix() {
        let mut tree = AdaptiveRadixTree::new();
        for i in 0..50u8 {
            let key = format!("prefix_{}", i);
            tree.insert(key.as_bytes(), i as i32);
        }
        assert_eq!(tree.len(), 50);
        for i in 0..50u8 {
            let key = format!("prefix_{}", i);
            assert_eq!(*tree.get(key.as_bytes()).unwrap(), i as i32);
        }
    }

    #[test]
    fn stress_insert_remove() {
        let mut tree = AdaptiveRadixTree::new();
        for i in 0..100u32 {
            tree.insert(&i.to_be_bytes(), i);
        }
        assert_eq!(tree.len(), 100);
        for i in 0..50u32 {
            assert_eq!(tree.remove(&i.to_be_bytes()), Some(i));
        }
        assert_eq!(tree.len(), 50);
        for i in 50..100u32 {
            assert_eq!(*tree.get(&i.to_be_bytes()).unwrap(), i);
        }
    }

    #[test]
    fn contains_str_helper() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert_str("hello", 1);
        assert!(tree.contains_str("hello"));
        assert!(!tree.contains_str("world"));
    }

    #[test]
    fn remove_str_helper() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert_str("hello", 1);
        assert_eq!(tree.remove_str("hello"), Some(1));
        assert!(tree.is_empty());
    }

    #[test]
    fn get_str_helper() {
        let mut tree = AdaptiveRadixTree::new();
        tree.insert_str("key", 42);
        assert_eq!(*tree.get_str("key").unwrap(), 42);
        assert!(tree.get_str("other").is_none());
    }

    #[test]
    fn single_byte_keys_all_256() {
        let mut tree = AdaptiveRadixTree::new();
        for b in 0..=255u8 {
            tree.insert(&[b], b as i32);
        }
        assert_eq!(tree.len(), 256);
        // Verify all retrievable
        for b in 0..=255u8 {
            assert_eq!(*tree.get(&[b]).unwrap(), b as i32);
        }
        // Remove half
        for b in 0..128u8 {
            assert_eq!(tree.remove(&[b]), Some(b as i32));
        }
        assert_eq!(tree.len(), 128);
    }
}
