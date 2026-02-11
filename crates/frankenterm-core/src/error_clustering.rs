//! Approximate nearest neighbor for novel error clustering via LSH.
//!
//! Uses MinHash locality-sensitive hashing to cluster similar errors across
//! panes in real-time without training data.
//!
//! # Method
//!
//! 1. Tokenize error text into character 5-grams (shingles)
//! 2. Compute MinHash signature: 128 hash functions
//! 3. LSH banding: 16 bands × 8 rows — similarity threshold ≈ 0.7
//! 4. Union-Find merges clusters when LSH detects similarity
//!
//! # Complexity
//!
//! - O(k) per error insertion where k = shingle count (typically < 100)
//! - O(1) amortized cluster lookup via LSH band index

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::debug;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for error clustering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusteringConfig {
    /// Number of MinHash hash functions.
    pub num_hashes: usize,
    /// Number of LSH bands (num_hashes must be divisible by this).
    pub num_bands: usize,
    /// Character n-gram size for shingling.
    pub shingle_size: usize,
    /// Maximum clusters to maintain (oldest evicted).
    pub max_clusters: usize,
    /// Maximum errors stored per cluster for display.
    pub max_samples_per_cluster: usize,
}

impl Default for ClusteringConfig {
    fn default() -> Self {
        Self {
            num_hashes: 128,
            num_bands: 16,
            shingle_size: 5,
            max_clusters: 1000,
            max_samples_per_cluster: 5,
        }
    }
}

// =============================================================================
// MinHash
// =============================================================================

/// Compute MinHash signature for a set of shingles.
///
/// Uses `num_hashes` independent hash functions, each producing the minimum
/// hash of all shingles. Hash functions are of the form `h(x) = (a*x + b) mod p`
/// where `p` is a large prime and `(a, b)` are per-function coefficients.
fn minhash_signature(shingles: &[u64], num_hashes: usize) -> Vec<u64> {
    const PRIME: u64 = 0xFFFF_FFFF_FFFF_FFC5; // large prime near u64::MAX
    let mut signature = vec![u64::MAX; num_hashes];

    for (i, sig) in signature.iter_mut().enumerate() {
        // Coefficients derived deterministically from hash index
        let a = (i as u64)
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let b = (i as u64)
            .wrapping_mul(1_442_695_040_888_963_407)
            .wrapping_add(7);

        for &shingle in shingles {
            let h = a.wrapping_mul(shingle).wrapping_add(b) % PRIME;
            if h < *sig {
                *sig = h;
            }
        }
    }

    signature
}

/// Extract character n-gram shingles from text, returning their hashes.
fn shingle(text: &str, n: usize) -> Vec<u64> {
    if text.len() < n {
        // For very short strings, use the whole string as one shingle
        return vec![hash_bytes(text.as_bytes())];
    }
    let bytes = text.as_bytes();
    let mut shingles = Vec::with_capacity(bytes.len().saturating_sub(n) + 1);
    for window in bytes.windows(n) {
        shingles.push(hash_bytes(window));
    }
    shingles
}

/// Simple FNV-1a hash for byte slices.
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

// =============================================================================
// LSH band index
// =============================================================================

/// LSH band index for fast approximate similarity lookup.
///
/// Divides MinHash signatures into bands; two signatures match if ANY
/// band is identical.
#[derive(Debug, Clone)]
struct BandIndex {
    rows_per_band: usize,
    /// band_idx → (band_hash → list of error IDs)
    tables: Vec<HashMap<u64, Vec<usize>>>,
}

impl BandIndex {
    fn new(num_hashes: usize, num_bands: usize) -> Self {
        assert!(
            num_hashes % num_bands == 0,
            "num_hashes must be divisible by num_bands"
        );
        let rows_per_band = num_hashes / num_bands;
        Self {
            rows_per_band,
            tables: (0..num_bands).map(|_| HashMap::new()).collect(),
        }
    }

    /// Insert a signature and return IDs of existing entries that match
    /// in at least one band.
    fn insert(&mut self, id: usize, signature: &[u64]) -> Vec<usize> {
        let mut candidates = Vec::new();
        for (band_idx, table) in self.tables.iter_mut().enumerate() {
            let start = band_idx * self.rows_per_band;
            let end = start + self.rows_per_band;
            let band_hash = hash_bytes(
                &signature[start..end]
                    .iter()
                    .flat_map(|h| h.to_le_bytes())
                    .collect::<Vec<u8>>(),
            );
            if let Some(existing) = table.get(&band_hash) {
                candidates.extend(existing.iter().copied());
            }
            table.entry(band_hash).or_default().push(id);
        }
        candidates.sort_unstable();
        candidates.dedup();
        candidates
    }
}

// =============================================================================
// Union-Find
// =============================================================================

/// Weighted union-find with path compression.
#[derive(Debug, Clone)]
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    fn union(&mut self, x: usize, y: usize) -> usize {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx == ry {
            return rx;
        }
        if self.rank[rx] < self.rank[ry] {
            self.parent[rx] = ry;
            ry
        } else if self.rank[rx] > self.rank[ry] {
            self.parent[ry] = rx;
            rx
        } else {
            self.parent[ry] = rx;
            self.rank[rx] += 1;
            rx
        }
    }

    fn extend(&mut self) -> usize {
        let id = self.parent.len();
        self.parent.push(id);
        self.rank.push(0);
        id
    }
}

// =============================================================================
// Error entry + cluster info
// =============================================================================

/// Summary of an error cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterInfo {
    /// Unique cluster ID.
    pub cluster_id: usize,
    /// Number of errors in this cluster.
    pub size: usize,
    /// Representative error text (first seen).
    pub representative: String,
    /// Sample error texts from this cluster.
    pub samples: Vec<String>,
    /// Pane IDs that contributed errors to this cluster.
    pub pane_ids: Vec<u64>,
    /// Timestamp of earliest error in cluster.
    pub first_seen_secs: u64,
    /// Timestamp of latest error in cluster.
    pub last_seen_secs: u64,
}

// =============================================================================
// Error clustering engine
// =============================================================================

/// Real-time error clustering engine using MinHash LSH.
#[derive(Debug)]
pub struct ErrorClusterer {
    config: ClusteringConfig,
    band_index: BandIndex,
    union_find: UnionFind,
    entry_count: usize,
    /// cluster_root → metadata
    cluster_meta: HashMap<usize, ClusterMeta>,
}

#[derive(Debug, Clone)]
struct ClusterMeta {
    size: usize,
    representative: String,
    samples: Vec<String>,
    pane_ids: Vec<u64>,
    first_seen_secs: u64,
    last_seen_secs: u64,
}

impl ErrorClusterer {
    /// Create a new clustering engine.
    pub fn new(config: ClusteringConfig) -> Self {
        Self {
            band_index: BandIndex::new(config.num_hashes, config.num_bands),
            union_find: UnionFind::new(0),
            entry_count: 0,
            cluster_meta: HashMap::new(),
            config,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ClusteringConfig::default())
    }

    /// Insert an error and return the cluster ID it was assigned to.
    pub fn insert(&mut self, text: &str, pane_id: Option<u64>, timestamp_secs: u64) -> usize {
        let shingles = shingle(text, self.config.shingle_size);
        let signature = minhash_signature(&shingles, self.config.num_hashes);

        let id = self.union_find.extend();
        let candidates = self.band_index.insert(id, &signature);
        self.entry_count += 1;

        // Merge with all candidates
        let mut root = self.union_find.find(id);
        for &candidate_id in &candidates {
            if candidate_id < self.entry_count {
                root = self.union_find.union(root, candidate_id);
            }
        }
        let root = self.union_find.find(root);

        // Update cluster metadata
        let meta = self
            .cluster_meta
            .entry(root)
            .or_insert_with(|| ClusterMeta {
                size: 0,
                representative: text.to_string(),
                samples: Vec::new(),
                pane_ids: Vec::new(),
                first_seen_secs: timestamp_secs,
                last_seen_secs: timestamp_secs,
            });
        meta.size += 1;
        meta.last_seen_secs = meta.last_seen_secs.max(timestamp_secs);
        meta.first_seen_secs = meta.first_seen_secs.min(timestamp_secs);
        if meta.samples.len() < self.config.max_samples_per_cluster {
            meta.samples.push(text.to_string());
        }
        if let Some(pid) = pane_id {
            if !meta.pane_ids.contains(&pid) {
                meta.pane_ids.push(pid);
            }
        }

        // Migrate metadata if root changed due to union
        if !candidates.is_empty() {
            self.reconcile_cluster_meta(root);
        }

        if let Some(meta) = self.cluster_meta.get(&root) {
            if meta.size > 1 {
                debug!(
                    cluster_id = root,
                    size = meta.size,
                    panes = meta.pane_ids.len(),
                    "Error clustered with existing group"
                );
            }
        }

        root
    }

    /// Reconcile cluster metadata after a union operation.
    fn reconcile_cluster_meta(&mut self, new_root: usize) {
        // Collect all roots that should merge into new_root
        let mut to_merge = Vec::new();
        for &old_root in self.cluster_meta.keys() {
            if old_root != new_root && self.union_find.find(old_root) == new_root {
                to_merge.push(old_root);
            }
        }

        for old_root in to_merge {
            if let Some(old_meta) = self.cluster_meta.remove(&old_root) {
                if let Some(new_meta) = self.cluster_meta.get_mut(&new_root) {
                    new_meta.size += old_meta.size;
                    new_meta.first_seen_secs =
                        new_meta.first_seen_secs.min(old_meta.first_seen_secs);
                    new_meta.last_seen_secs = new_meta.last_seen_secs.max(old_meta.last_seen_secs);
                    for pid in old_meta.pane_ids {
                        if !new_meta.pane_ids.contains(&pid) {
                            new_meta.pane_ids.push(pid);
                        }
                    }
                    let remaining = self
                        .config
                        .max_samples_per_cluster
                        .saturating_sub(new_meta.samples.len());
                    new_meta
                        .samples
                        .extend(old_meta.samples.into_iter().take(remaining));
                }
            }
        }
    }

    /// Get all current clusters.
    pub fn clusters(&mut self) -> Vec<ClusterInfo> {
        // Rebuild cluster roots
        let mut root_map: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..self.entry_count {
            let root = self.union_find.find(i);
            root_map.entry(root).or_default().push(i);
        }

        root_map
            .into_iter()
            .filter_map(|(root, _members)| {
                self.cluster_meta.get(&root).map(|meta| ClusterInfo {
                    cluster_id: root,
                    size: meta.size,
                    representative: meta.representative.clone(),
                    samples: meta.samples.clone(),
                    pane_ids: meta.pane_ids.clone(),
                    first_seen_secs: meta.first_seen_secs,
                    last_seen_secs: meta.last_seen_secs,
                })
            })
            .collect()
    }

    /// Get info for a specific cluster by root ID.
    pub fn cluster_info(&mut self, error_id: usize) -> Option<ClusterInfo> {
        let root = self.union_find.find(error_id);
        self.cluster_meta.get(&root).map(|meta| ClusterInfo {
            cluster_id: root,
            size: meta.size,
            representative: meta.representative.clone(),
            samples: meta.samples.clone(),
            pane_ids: meta.pane_ids.clone(),
            first_seen_secs: meta.first_seen_secs,
            last_seen_secs: meta.last_seen_secs,
        })
    }

    /// Number of errors ingested.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.entry_count
    }

    /// Number of distinct clusters.
    #[must_use]
    pub fn cluster_count(&self) -> usize {
        self.cluster_meta.len()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn hash_bytes_deterministic() {
        let h1 = hash_bytes(b"hello");
        let h2 = hash_bytes(b"hello");
        let h3 = hash_bytes(b"world");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
    }

    #[test]
    fn shingle_basic() {
        let s = shingle("hello world", 5);
        // "hello world" has 11 chars, 7 5-grams
        assert_eq!(s.len(), 7);
    }

    #[test]
    fn shingle_short_text() {
        let s = shingle("hi", 5);
        assert_eq!(s.len(), 1); // whole string as one shingle
    }

    #[test]
    fn minhash_identical_texts_match() {
        let s1 = shingle("ConnectionRefusedError: port 5432", 5);
        let s2 = shingle("ConnectionRefusedError: port 5432", 5);
        let sig1 = minhash_signature(&s1, 128);
        let sig2 = minhash_signature(&s2, 128);
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn minhash_similar_texts_high_overlap() {
        let s1 = shingle("ConnectionRefusedError: port 5432", 5);
        let s2 = shingle("ConnectionRefusedError: port 3306", 5);
        let sig1 = minhash_signature(&s1, 128);
        let sig2 = minhash_signature(&s2, 128);
        let matching = sig1.iter().zip(&sig2).filter(|(a, b)| a == b).count();
        let jaccard_est = matching as f64 / 128.0;
        assert!(
            jaccard_est > 0.5,
            "similar errors should have high Jaccard estimate: {jaccard_est}"
        );
    }

    #[test]
    fn minhash_different_texts_low_overlap() {
        let s1 = shingle("ConnectionRefusedError: port 5432", 5);
        let s2 = shingle("SyntaxError: unexpected token at line 42", 5);
        let sig1 = minhash_signature(&s1, 128);
        let sig2 = minhash_signature(&s2, 128);
        let matching = sig1.iter().zip(&sig2).filter(|(a, b)| a == b).count();
        let jaccard_est = matching as f64 / 128.0;
        assert!(
            jaccard_est < 0.3,
            "different errors should have low Jaccard estimate: {jaccard_est}"
        );
    }

    #[test]
    fn union_find_basic() {
        let mut uf = UnionFind::new(5);
        assert_ne!(uf.find(0), uf.find(1));
        uf.union(0, 1);
        assert_eq!(uf.find(0), uf.find(1));
        uf.union(2, 3);
        uf.union(1, 3);
        assert_eq!(uf.find(0), uf.find(3));
    }

    #[test]
    fn union_find_extend() {
        let mut uf = UnionFind::new(0);
        let a = uf.extend();
        let b = uf.extend();
        assert_ne!(uf.find(a), uf.find(b));
        uf.union(a, b);
        assert_eq!(uf.find(a), uf.find(b));
    }

    #[test]
    fn band_index_identical_sigs_match() {
        let mut idx = BandIndex::new(128, 16);
        let sig = vec![42u64; 128];
        idx.insert(0, &sig);
        let candidates = idx.insert(1, &sig);
        assert!(candidates.contains(&0));
    }

    #[test]
    fn band_index_different_sigs_no_match() {
        let mut idx = BandIndex::new(128, 16);
        let sig1: Vec<u64> = (0..128).collect();
        let sig2: Vec<u64> = (1000..1128).collect();
        idx.insert(0, &sig1);
        let candidates = idx.insert(1, &sig2);
        assert!(candidates.is_empty());
    }

    // -- ErrorClusterer --

    #[test]
    fn clusterer_groups_similar_errors() {
        let mut c = ErrorClusterer::with_defaults();
        let c1 = c.insert("ConnectionRefusedError: port 5432", Some(1), 100);
        let c2 = c.insert("ConnectionRefusedError: port 3306", Some(2), 101);
        let c3 = c.insert("ConnectionRefusedError: port 8080", Some(3), 102);

        // All connection refused errors should cluster together
        let root1 = c.union_find.find(c1);
        let root2 = c.union_find.find(c2);
        let root3 = c.union_find.find(c3);
        assert_eq!(root1, root2, "similar errors should cluster");
        assert_eq!(root2, root3, "similar errors should cluster");
    }

    #[test]
    fn clusterer_separates_different_errors() {
        let mut c = ErrorClusterer::with_defaults();
        c.insert("ConnectionRefusedError: port 5432", Some(1), 100);
        c.insert(
            "SyntaxError: unexpected token 'foo' at line 42",
            Some(2),
            101,
        );
        c.insert("PermissionDenied: /etc/shadow", Some(3), 102);

        let clusters = c.clusters();
        assert!(
            clusters.len() >= 2,
            "different errors should not all cluster: {} clusters",
            clusters.len()
        );
    }

    #[test]
    fn clusterer_tracks_panes() {
        let mut c = ErrorClusterer::with_defaults();
        c.insert("timeout after 30s", Some(1), 100);
        c.insert("timeout after 30s", Some(2), 101);
        c.insert("timeout after 30s", Some(3), 102);

        let clusters = c.clusters();
        let timeout_cluster = clusters
            .iter()
            .find(|c| c.representative.contains("timeout"));
        assert!(timeout_cluster.is_some());
        let tc = timeout_cluster.unwrap();
        assert_eq!(tc.pane_ids.len(), 3);
        assert_eq!(tc.size, 3);
    }

    #[test]
    fn clusterer_cluster_info() {
        let mut c = ErrorClusterer::with_defaults();
        let id = c.insert("test error", None, 100);
        let info = c.cluster_info(id);
        assert!(info.is_some());
        assert_eq!(info.unwrap().size, 1);
    }

    #[test]
    fn clusterer_counts() {
        let mut c = ErrorClusterer::with_defaults();
        assert_eq!(c.error_count(), 0);
        assert_eq!(c.cluster_count(), 0);
        c.insert("error one", None, 100);
        c.insert(
            "something completely different and unrelated to any other message",
            None,
            101,
        );
        assert_eq!(c.error_count(), 2);
        assert!(c.cluster_count() >= 1);
    }

    #[test]
    fn clusterer_timestamps() {
        let mut c = ErrorClusterer::with_defaults();
        c.insert("same error msg here", None, 100);
        c.insert("same error msg here", None, 200);
        c.insert("same error msg here", None, 50);

        let clusters = c.clusters();
        assert!(!clusters.is_empty());
        let cl = &clusters[0];
        assert_eq!(cl.first_seen_secs, 50);
        assert_eq!(cl.last_seen_secs, 200);
    }

    // -- Proptest --

    proptest! {
        #[test]
        fn proptest_minhash_signature_length(
            text in "[a-zA-Z0-9 ]{10,200}",
            num_hashes in (16usize..256).prop_filter("divisible by 8", |n| n % 8 == 0),
        ) {
            let shingles = shingle(&text, 5);
            let sig = minhash_signature(&shingles, num_hashes);
            prop_assert_eq!(sig.len(), num_hashes);
        }

        #[test]
        fn proptest_identical_texts_same_signature(
            text in "[a-zA-Z0-9 ]{10,200}"
        ) {
            let s1 = shingle(&text, 5);
            let s2 = shingle(&text, 5);
            let sig1 = minhash_signature(&s1, 128);
            let sig2 = minhash_signature(&s2, 128);
            prop_assert_eq!(sig1, sig2);
        }

        #[test]
        fn proptest_union_find_consistency(
            n in 5usize..50,
            merges in proptest::collection::vec((0usize..50, 0usize..50), 1..20),
        ) {
            let mut uf = UnionFind::new(n);
            for &(a, b) in &merges {
                if a < n && b < n {
                    uf.union(a, b);
                }
            }
            // Transitivity: if find(a) == find(b) and find(b) == find(c),
            // then find(a) == find(c)
            for i in 0..n {
                for j in 0..n {
                    if uf.find(i) == uf.find(j) {
                        for k in 0..n {
                            if uf.find(j) == uf.find(k) {
                                prop_assert_eq!(uf.find(i), uf.find(k));
                            }
                        }
                    }
                }
            }
        }

        #[test]
        fn proptest_shingle_nonempty(
            text in ".{1,200}"
        ) {
            let s = shingle(&text, 5);
            prop_assert!(!s.is_empty(), "shingles should never be empty");
        }
    }
}
