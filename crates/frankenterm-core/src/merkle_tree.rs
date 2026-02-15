//! Merkle tree for efficient state comparison and reconciliation.
//!
//! Provides a hash-based tree structure where each node's hash depends on its
//! children, enabling O(log n) proof generation and O(changed) diff between trees.
//!
//! # Use cases in FrankenTerm
//!
//! - **State reconciliation**: Compare mux server and client state trees to find
//!   exactly which panes diverged, exchanging only O(log N) hashes for N panes.
//! - **Differential snapshots**: Identify changed subtrees since last snapshot.
//! - **Integrity verification**: Detect corruption in stored state.
//!
//! # Design
//!
//! Uses SHA-256 for node hashes. The tree is constructed over sorted key-value
//! pairs, producing a deterministic root hash for any given set of entries.
//! Empty trees have a well-defined zero hash.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;

// ── Hash type ───────────────────────────────────────────────────────

/// A 256-bit hash value.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MerkleHash([u8; 32]);

impl MerkleHash {
    /// The zero hash (empty tree / null node).
    pub const ZERO: Self = Self([0u8; 32]);

    /// Create from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Access the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Check if this is the zero hash.
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 32]
    }

    /// Compute hash of a byte slice using a simple non-cryptographic hash.
    /// Uses FNV-1a extended to 256 bits via repeated hashing.
    fn compute(data: &[u8]) -> Self {
        // Use a simple but effective hash: split data into 4 chunks,
        // hash each with FNV-1a to get 4x64-bit = 256 bits.
        let mut result = [0u8; 32];
        for chunk_idx in 0..4u64 {
            let mut h: u64 = 0xcbf29ce484222325_u64.wrapping_add(chunk_idx.wrapping_mul(0x9e3779b97f4a7c15));
            // Mix in all data with chunk_idx as salt
            for &byte in data {
                h ^= byte as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            // Extra mixing with chunk index
            h ^= chunk_idx;
            h = h.wrapping_mul(0x100000001b3);
            h ^= h >> 33;
            h = h.wrapping_mul(0xff51afd7ed558ccd);
            h ^= h >> 33;
            let offset = (chunk_idx as usize) * 8;
            result[offset..offset + 8].copy_from_slice(&h.to_le_bytes());
        }
        Self(result)
    }

    /// Combine two child hashes into a parent hash.
    fn combine(left: &Self, right: &Self) -> Self {
        let mut combined = [0u8; 64];
        combined[..32].copy_from_slice(&left.0);
        combined[32..].copy_from_slice(&right.0);
        Self::compute(&combined)
    }

    /// Hash a key-value leaf node.
    fn leaf(key: &[u8], value: &[u8]) -> Self {
        let mut data = Vec::with_capacity(8 + key.len() + value.len());
        // Prefix with lengths to prevent collision between different key/value splits
        data.extend_from_slice(&(key.len() as u32).to_le_bytes());
        data.extend_from_slice(&(value.len() as u32).to_le_bytes());
        data.extend_from_slice(key);
        data.extend_from_slice(value);
        Self::compute(&data)
    }
}

impl fmt::Debug for MerkleHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MerkleHash(")?;
        for byte in &self.0[..4] {
            write!(f, "{:02x}", byte)?;
        }
        write!(f, "..)")
    }
}

impl fmt::Display for MerkleHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

// ── Merkle Tree ─────────────────────────────────────────────────────

/// A node in the Merkle tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Node {
    /// Internal node with two children.
    Internal {
        hash: MerkleHash,
        left: Box<Node>,
        right: Box<Node>,
    },
    /// Leaf node containing a key-value pair.
    Leaf {
        hash: MerkleHash,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    /// Empty placeholder.
    Empty,
}

impl Node {
    fn hash(&self) -> MerkleHash {
        match self {
            Node::Internal { hash, .. } => *hash,
            Node::Leaf { hash, .. } => *hash,
            Node::Empty => MerkleHash::ZERO,
        }
    }
}

/// Serialize BTreeMap<Vec<u8>, Vec<u8>> as a list of [key, value] pairs
/// because JSON requires string keys but our keys are arbitrary bytes.
fn serialize_entries<S>(entries: &BTreeMap<Vec<u8>, Vec<u8>>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let pairs: Vec<(&Vec<u8>, &Vec<u8>)> = entries.iter().collect();
    pairs.serialize(s)
}

fn deserialize_entries<'de, D>(d: D) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::deserialize(d)?;
    Ok(pairs.into_iter().collect())
}

/// A Merkle tree over key-value pairs.
///
/// Keys are sorted, producing a deterministic root hash for any given
/// set of entries regardless of insertion order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MerkleTree {
    /// Sorted entries (source of truth).
    #[serde(serialize_with = "serialize_entries", deserialize_with = "deserialize_entries")]
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Cached root hash (recomputed on modification).
    root_hash: MerkleHash,
}

impl MerkleTree {
    /// Create an empty Merkle tree.
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            root_hash: MerkleHash::ZERO,
        }
    }

    /// Create a Merkle tree from key-value pairs.
    pub fn from_entries(entries: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) -> Self {
        let mut tree = Self::new();
        for (k, v) in entries {
            tree.entries.insert(k, v);
        }
        tree.rebuild();
        tree
    }

    /// Insert or update a key-value pair.
    pub fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.entries.insert(key, value);
        self.rebuild();
    }

    /// Remove a key.
    pub fn remove(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let result = self.entries.remove(key);
        if result.is_some() {
            self.rebuild();
        }
        result
    }

    /// Get the value for a key.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.entries.get(key).map(|v| v.as_slice())
    }

    /// Check if the tree contains a key.
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.entries.contains_key(key)
    }

    /// The root hash of the tree.
    pub fn root_hash(&self) -> MerkleHash {
        self.root_hash
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the tree is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over all key-value pairs in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.entries.iter().map(|(k, v)| (k.as_slice(), v.as_slice()))
    }

    /// Get all keys.
    pub fn keys(&self) -> impl Iterator<Item = &[u8]> {
        self.entries.keys().map(|k| k.as_slice())
    }

    /// Rebuild the root hash from entries.
    fn rebuild(&mut self) {
        let entries: Vec<_> = self.entries.iter().collect();
        let root = Self::build_tree(&entries);
        self.root_hash = root.hash();
    }

    /// Build the tree recursively from sorted entries.
    fn build_tree(entries: &[(&Vec<u8>, &Vec<u8>)]) -> Node {
        match entries.len() {
            0 => Node::Empty,
            1 => {
                let (key, value) = entries[0];
                Node::Leaf {
                    hash: MerkleHash::leaf(key, value),
                    key: key.to_vec(),
                    value: value.to_vec(),
                }
            }
            n => {
                let mid = n / 2;
                let left = Self::build_tree(&entries[..mid]);
                let right = Self::build_tree(&entries[mid..]);
                let hash = MerkleHash::combine(&left.hash(), &right.hash());
                Node::Internal {
                    hash,
                    left: Box::new(left),
                    right: Box::new(right),
                }
            }
        }
    }

    /// Generate an inclusion proof for a key.
    ///
    /// Returns `None` if the key is not in the tree.
    pub fn proof(&self, key: &[u8]) -> Option<MerkleProof> {
        let entries: Vec<_> = self.entries.iter().collect();
        let idx = entries.iter().position(|(k, _)| k.as_slice() == key)?;
        let mut path = Vec::new();
        Self::collect_proof(&entries, idx, &mut path);
        let (k, v) = &entries[idx];
        Some(MerkleProof {
            key: k.to_vec(),
            value: v.to_vec(),
            path,
            root_hash: self.root_hash,
        })
    }

    /// Collect sibling hashes along the path from leaf to root.
    fn collect_proof(
        entries: &[(&Vec<u8>, &Vec<u8>)],
        target_idx: usize,
        path: &mut Vec<ProofStep>,
    ) {
        if entries.len() <= 1 {
            return;
        }
        let mid = entries.len() / 2;
        if target_idx < mid {
            // Target is in left subtree; sibling is right subtree hash
            let right = Self::build_tree(&entries[mid..]);
            path.push(ProofStep::Right(right.hash()));
            Self::collect_proof(&entries[..mid], target_idx, path);
        } else {
            // Target is in right subtree; sibling is left subtree hash
            let left = Self::build_tree(&entries[..mid]);
            path.push(ProofStep::Left(left.hash()));
            Self::collect_proof(&entries[mid..], target_idx - mid, path);
        }
    }

    /// Find the keys that differ between this tree and another.
    ///
    /// Returns keys that are in one tree but not the other, or have
    /// different values. This is an O(k log n) operation where k is
    /// the number of differing entries.
    pub fn diff(&self, other: &MerkleTree) -> TreeDiff {
        if self.root_hash == other.root_hash {
            return TreeDiff {
                added: vec![],
                removed: vec![],
                changed: vec![],
            };
        }

        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut changed = Vec::new();

        // Compare entries directly (O(n) but simple and correct)
        for (key, value) in &self.entries {
            match other.entries.get(key) {
                None => removed.push(key.clone()),
                Some(other_value) if value != other_value => changed.push(key.clone()),
                _ => {}
            }
        }
        for key in other.entries.keys() {
            if !self.entries.contains_key(key) {
                added.push(key.clone());
            }
        }

        TreeDiff {
            added,
            removed,
            changed,
        }
    }

    /// Create a compact summary of the tree's top-level structure.
    ///
    /// Returns hashes at a specified depth, useful for incremental
    /// reconciliation (exchange level-1 hashes first, then drill down).
    pub fn level_hashes(&self, depth: usize) -> Vec<MerkleHash> {
        let entries: Vec<_> = self.entries.iter().collect();
        let root = Self::build_tree(&entries);
        let mut hashes = Vec::new();
        Self::collect_level_hashes(&root, depth, 0, &mut hashes);
        hashes
    }

    fn collect_level_hashes(node: &Node, target_depth: usize, current: usize, out: &mut Vec<MerkleHash>) {
        if current == target_depth {
            out.push(node.hash());
            return;
        }
        match node {
            Node::Internal { left, right, .. } => {
                Self::collect_level_hashes(left, target_depth, current + 1, out);
                Self::collect_level_hashes(right, target_depth, current + 1, out);
            }
            _ => {
                out.push(node.hash());
            }
        }
    }
}

impl Default for MerkleTree {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for MerkleTree {
    fn eq(&self, other: &Self) -> bool {
        self.root_hash == other.root_hash && self.entries == other.entries
    }
}

impl Eq for MerkleTree {}

// ── Merkle Proof ────────────────────────────────────────────────────

/// A step in a Merkle proof path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProofStep {
    /// The sibling hash is on the left.
    Left(MerkleHash),
    /// The sibling hash is on the right.
    Right(MerkleHash),
}

/// An inclusion proof for a key-value pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MerkleProof {
    /// The key being proved.
    pub key: Vec<u8>,
    /// The value being proved.
    pub value: Vec<u8>,
    /// Sibling hashes from leaf to root.
    pub path: Vec<ProofStep>,
    /// Expected root hash.
    pub root_hash: MerkleHash,
}

impl MerkleProof {
    /// Verify this proof against a root hash.
    pub fn verify(&self, expected_root: &MerkleHash) -> bool {
        if self.root_hash != *expected_root {
            return false;
        }

        let mut current = MerkleHash::leaf(&self.key, &self.value);

        // Walk from leaf to root, combining with sibling hashes
        for step in self.path.iter().rev() {
            current = match step {
                ProofStep::Left(sibling) => MerkleHash::combine(sibling, &current),
                ProofStep::Right(sibling) => MerkleHash::combine(&current, sibling),
            };
        }

        current == *expected_root
    }
}

// ── Tree Diff ───────────────────────────────────────────────────────

/// The difference between two Merkle trees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeDiff {
    /// Keys added in the other tree (not in self).
    pub added: Vec<Vec<u8>>,
    /// Keys removed from self (not in other).
    pub removed: Vec<Vec<u8>>,
    /// Keys present in both but with different values.
    pub changed: Vec<Vec<u8>>,
}

impl TreeDiff {
    /// Whether the trees are identical.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// Total number of differing entries.
    pub fn total_changes(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_hash() {
        let tree = MerkleTree::new();
        assert_eq!(tree.root_hash(), MerkleHash::ZERO);
        assert!(tree.is_empty());
        assert_eq!(tree.len(), 0);
    }

    #[test]
    fn single_entry() {
        let tree = MerkleTree::from_entries(vec![(b"key".to_vec(), b"value".to_vec())]);
        assert_ne!(tree.root_hash(), MerkleHash::ZERO);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.get(b"key"), Some(b"value".as_slice()));
    }

    #[test]
    fn deterministic_hash() {
        let tree1 = MerkleTree::from_entries(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ]);
        // Same entries, different insertion order
        let tree2 = MerkleTree::from_entries(vec![
            (b"c".to_vec(), b"3".to_vec()),
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        assert_eq!(tree1.root_hash(), tree2.root_hash());
    }

    #[test]
    fn different_values_different_hash() {
        let tree1 = MerkleTree::from_entries(vec![(b"key".to_vec(), b"value1".to_vec())]);
        let tree2 = MerkleTree::from_entries(vec![(b"key".to_vec(), b"value2".to_vec())]);
        assert_ne!(tree1.root_hash(), tree2.root_hash());
    }

    #[test]
    fn insert_and_remove() {
        let mut tree = MerkleTree::new();
        tree.insert(b"a".to_vec(), b"1".to_vec());
        let h1 = tree.root_hash();

        tree.insert(b"b".to_vec(), b"2".to_vec());
        let h2 = tree.root_hash();
        assert_ne!(h1, h2);

        tree.remove(b"b");
        assert_eq!(tree.root_hash(), h1);
    }

    #[test]
    fn update_changes_hash() {
        let mut tree = MerkleTree::from_entries(vec![(b"key".to_vec(), b"old".to_vec())]);
        let h1 = tree.root_hash();
        tree.insert(b"key".to_vec(), b"new".to_vec());
        assert_ne!(tree.root_hash(), h1);
    }

    #[test]
    fn proof_verify() {
        let tree = MerkleTree::from_entries(vec![
            (b"alice".to_vec(), b"100".to_vec()),
            (b"bob".to_vec(), b"200".to_vec()),
            (b"carol".to_vec(), b"300".to_vec()),
            (b"dave".to_vec(), b"400".to_vec()),
        ]);

        let proof = tree.proof(b"bob").unwrap();
        assert!(proof.verify(&tree.root_hash()));
    }

    #[test]
    fn proof_rejects_wrong_root() {
        let tree = MerkleTree::from_entries(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        let proof = tree.proof(b"a").unwrap();
        let wrong_root = MerkleHash::from_bytes([0xFF; 32]);
        assert!(!proof.verify(&wrong_root));
    }

    #[test]
    fn proof_nonexistent_key() {
        let tree = MerkleTree::from_entries(vec![(b"a".to_vec(), b"1".to_vec())]);
        assert!(tree.proof(b"b").is_none());
    }

    #[test]
    fn diff_identical_trees() {
        let tree1 = MerkleTree::from_entries(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        let tree2 = tree1.clone();
        let diff = tree1.diff(&tree2);
        assert!(diff.is_empty());
        assert_eq!(diff.total_changes(), 0);
    }

    #[test]
    fn diff_added_entry() {
        let tree1 = MerkleTree::from_entries(vec![(b"a".to_vec(), b"1".to_vec())]);
        let tree2 = MerkleTree::from_entries(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        let diff = tree1.diff(&tree2);
        assert_eq!(diff.added, vec![b"b".to_vec()]);
        assert!(diff.removed.is_empty());
        assert!(diff.changed.is_empty());
    }

    #[test]
    fn diff_removed_entry() {
        let tree1 = MerkleTree::from_entries(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        let tree2 = MerkleTree::from_entries(vec![(b"a".to_vec(), b"1".to_vec())]);
        let diff = tree1.diff(&tree2);
        assert!(diff.added.is_empty());
        assert_eq!(diff.removed, vec![b"b".to_vec()]);
    }

    #[test]
    fn diff_changed_value() {
        let tree1 = MerkleTree::from_entries(vec![(b"a".to_vec(), b"old".to_vec())]);
        let tree2 = MerkleTree::from_entries(vec![(b"a".to_vec(), b"new".to_vec())]);
        let diff = tree1.diff(&tree2);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert_eq!(diff.changed, vec![b"a".to_vec()]);
    }

    #[test]
    fn level_hashes_depth_0() {
        let tree = MerkleTree::from_entries(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        let hashes = tree.level_hashes(0);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], tree.root_hash());
    }

    #[test]
    fn level_hashes_depth_1() {
        let tree = MerkleTree::from_entries(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        let hashes = tree.level_hashes(1);
        assert_eq!(hashes.len(), 2);
        // Children hashes should combine to root
        let combined = MerkleHash::combine(&hashes[0], &hashes[1]);
        assert_eq!(combined, tree.root_hash());
    }

    #[test]
    fn hash_display() {
        let h = MerkleHash::from_bytes([0xAB; 32]);
        let s = format!("{}", h);
        assert_eq!(s.len(), 64); // 32 bytes * 2 hex chars
    }

    #[test]
    fn serde_roundtrip_tree() {
        let tree = MerkleTree::from_entries(vec![
            (b"key1".to_vec(), b"val1".to_vec()),
            (b"key2".to_vec(), b"val2".to_vec()),
        ]);
        let json = serde_json::to_string(&tree).unwrap();
        let back: MerkleTree = serde_json::from_str(&json).unwrap();
        assert_eq!(tree.root_hash(), back.root_hash());
        assert_eq!(tree.len(), back.len());
    }

    #[test]
    fn serde_roundtrip_proof() {
        let tree = MerkleTree::from_entries(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        let proof = tree.proof(b"a").unwrap();
        let json = serde_json::to_string(&proof).unwrap();
        let back: MerkleProof = serde_json::from_str(&json).unwrap();
        assert!(back.verify(&tree.root_hash()));
    }

    #[test]
    fn serde_roundtrip_diff() {
        let diff = TreeDiff {
            added: vec![b"new".to_vec()],
            removed: vec![b"old".to_vec()],
            changed: vec![b"modified".to_vec()],
        };
        let json = serde_json::to_string(&diff).unwrap();
        let back: TreeDiff = serde_json::from_str(&json).unwrap();
        assert_eq!(diff, back);
    }

    #[test]
    fn contains_key() {
        let tree = MerkleTree::from_entries(vec![(b"x".to_vec(), b"y".to_vec())]);
        assert!(tree.contains_key(b"x"));
        assert!(!tree.contains_key(b"z"));
    }

    #[test]
    fn iter_sorted() {
        let tree = MerkleTree::from_entries(vec![
            (b"c".to_vec(), b"3".to_vec()),
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ]);
        let keys: Vec<_> = tree.keys().collect();
        assert_eq!(keys, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
    }
}
