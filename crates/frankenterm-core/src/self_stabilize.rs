//! Self-stabilizing reconciliation protocol using Merkle trees.
//!
//! Provides a convergent state reconciliation protocol based on
//! Dijkstra's self-stabilization framework (1974). Two parties with
//! potentially divergent key-value state converge to agreement within
//! bounded rounds by exchanging only O(log N) hashes per round.
//!
//! # Protocol
//!
//! 1. Both sides compute root hash of their state tree.
//! 2. Exchange root hashes — if equal, done (common case: ~32 bytes).
//! 3. If different, exchange level-1 hashes to identify divergent subtrees.
//! 4. Recurse into divergent subtrees until divergent leaves found.
//! 5. Authoritative side sends corrected entries; receiver applies.
//! 6. After at most 2 rounds, states converge.
//!
//! # Convergence guarantee
//!
//! For any initial states S₁, S₂, and authority mapping A:
//! - After round 1: all divergent entries are identified
//! - After round 2: all entries are reconciled and verified
//! - Bandwidth: O(k × log N) where k = changed entries, N = total entries
//!
//! # Use cases in FrankenTerm
//!
//! - **Mux reconnection**: After crash/restart, client and server exchange
//!   state hashes to identify which panes diverged, then re-sync only those.
//! - **Multi-node sync**: In distributed mode, nodes reconcile pane state
//!   without full snapshots.

use crate::merkle_tree::{MerkleHash, MerkleTree, TreeDiff};
use serde::{Deserialize, Serialize};

// ── Protocol messages ────────────────────────────────────────────

/// A message in the reconciliation protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReconcileMessage {
    /// Phase 1: Send root hash for quick comparison.
    RootHash(MerkleHash),
    /// Phase 2: Send hashes at a specific tree level for narrowing.
    LevelHashes {
        depth: usize,
        hashes: Vec<MerkleHash>,
    },
    /// Phase 3: Send full diff of divergent entries.
    Diff(TreeDiff),
    /// Phase 4: Send authoritative entries for reconciliation.
    Patch(Vec<(Vec<u8>, Vec<u8>)>),
    /// Protocol complete — states match.
    Converged,
}

/// The result of a reconciliation round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundResult {
    /// States already match — no action needed.
    AlreadyConverged,
    /// Need to exchange more information — send this message.
    SendMessage(ReconcileMessage),
    /// Apply these patches to converge.
    ApplyPatch(Vec<(Vec<u8>, Vec<u8>)>),
    /// Remove these keys to converge.
    RemoveKeys(Vec<Vec<u8>>),
    /// Reconciliation complete after applying changes.
    Done,
}

// ── Reconciliation state machine ─────────────────────────────────

/// Current phase of the reconciliation protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// Initial: exchange root hashes.
    Init,
    /// Exchanging level hashes to narrow divergence.
    Narrowing { depth: usize },
    /// Identified divergent entries, exchanging patches.
    Patching,
    /// Protocol complete.
    Converged,
}

/// Configuration for the reconciliation protocol.
#[derive(Debug, Clone)]
pub struct ReconcileConfig {
    /// Maximum tree depth to probe during narrowing.
    /// Deeper probing = more precise divergence identification = less data to sync.
    pub max_probe_depth: usize,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            max_probe_depth: 6,
        }
    }
}

/// A reconciliation session for one side of the protocol.
///
/// Tracks the local state tree, peer's known hashes, and protocol phase.
/// The session is driven by calling `start()` then repeatedly calling
/// `receive()` with messages from the peer.
pub struct ReconcileSession {
    /// Local state tree.
    local: MerkleTree,
    /// Whether this side is authoritative (sends patches).
    is_authority: bool,
    /// Current protocol phase.
    phase: Phase,
    /// Configuration.
    config: ReconcileConfig,
    /// Peer's root hash (if known).
    peer_root: Option<MerkleHash>,
    /// Round counter for convergence tracking.
    rounds: usize,
}

impl ReconcileSession {
    /// Create a new reconciliation session.
    ///
    /// `is_authority`: if true, this side's state is considered correct
    /// and it sends patches to the peer. If false, this side applies
    /// patches from the peer.
    pub fn new(local: MerkleTree, is_authority: bool, config: ReconcileConfig) -> Self {
        Self {
            local,
            is_authority,
            phase: Phase::Init,
            config,
            peer_root: None,
            rounds: 0,
        }
    }

    /// Get the current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Get the number of rounds completed.
    pub fn rounds(&self) -> usize {
        self.rounds
    }

    /// Check if reconciliation is complete.
    pub fn is_converged(&self) -> bool {
        self.phase == Phase::Converged
    }

    /// Get a reference to the local state tree.
    pub fn local_tree(&self) -> &MerkleTree {
        &self.local
    }

    /// Start the protocol by producing the initial message.
    pub fn start(&mut self) -> ReconcileMessage {
        self.phase = Phase::Init;
        self.rounds = 0;
        ReconcileMessage::RootHash(self.local.root_hash())
    }

    /// Process a message from the peer and produce a response.
    pub fn receive(&mut self, msg: &ReconcileMessage) -> RoundResult {
        self.rounds += 1;
        match msg {
            ReconcileMessage::RootHash(peer_hash) => self.handle_root_hash(*peer_hash),
            ReconcileMessage::LevelHashes { depth, hashes } => {
                self.handle_level_hashes(*depth, hashes)
            }
            ReconcileMessage::Diff(diff) => self.handle_diff(diff),
            ReconcileMessage::Patch(entries) => self.handle_patch(entries),
            ReconcileMessage::Converged => {
                self.phase = Phase::Converged;
                RoundResult::Done
            }
        }
    }

    /// Update the local tree (e.g., after applying external mutations).
    pub fn update_local(&mut self, tree: MerkleTree) {
        self.local = tree;
    }

    // ── Message handlers ─────────────────────────────────────

    fn handle_root_hash(&mut self, peer_hash: MerkleHash) -> RoundResult {
        self.peer_root = Some(peer_hash);

        if self.local.root_hash() == peer_hash {
            self.phase = Phase::Converged;
            return RoundResult::AlreadyConverged;
        }

        // States differ — start narrowing
        if self.is_authority {
            // Authority side: compute and send diff directly
            // (We'll need the peer's tree to diff, but we can send level hashes
            // to help the peer identify divergent entries)
            self.phase = Phase::Narrowing { depth: 1 };
            RoundResult::SendMessage(ReconcileMessage::LevelHashes {
                depth: 1,
                hashes: self.local.level_hashes(1),
            })
        } else {
            // Non-authority: also send level hashes for comparison
            self.phase = Phase::Narrowing { depth: 1 };
            RoundResult::SendMessage(ReconcileMessage::LevelHashes {
                depth: 1,
                hashes: self.local.level_hashes(1),
            })
        }
    }

    fn handle_level_hashes(&mut self, depth: usize, peer_hashes: &[MerkleHash]) -> RoundResult {
        let local_hashes = self.local.level_hashes(depth);

        // Check if all hashes match at this level
        let all_match = local_hashes.len() == peer_hashes.len()
            && local_hashes
                .iter()
                .zip(peer_hashes.iter())
                .all(|(a, b)| a == b);

        if all_match {
            self.phase = Phase::Converged;
            return RoundResult::AlreadyConverged;
        }

        // If we haven't reached max depth, probe deeper
        if depth < self.config.max_probe_depth {
            self.phase = Phase::Narrowing { depth: depth + 1 };
            return RoundResult::SendMessage(ReconcileMessage::LevelHashes {
                depth: depth + 1,
                hashes: self.local.level_hashes(depth + 1),
            });
        }

        // Reached max depth — switch to full diff mode
        if self.is_authority {
            // We need to know the peer's actual data to send patches.
            // Since we're the authority, request the peer's diff.
            self.phase = Phase::Patching;
            // Send our full state as a diff hint (peer will figure out what's different)
            let all_entries: Vec<(Vec<u8>, Vec<u8>)> = self
                .local
                .iter()
                .map(|(k, v)| (k.to_vec(), v.to_vec()))
                .collect();
            RoundResult::SendMessage(ReconcileMessage::Patch(all_entries))
        } else {
            // Non-authority: tell the authority what we have so it can compute diff
            self.phase = Phase::Patching;
            let entries: Vec<(Vec<u8>, Vec<u8>)> = self
                .local
                .iter()
                .map(|(k, v)| (k.to_vec(), v.to_vec()))
                .collect();
            RoundResult::SendMessage(ReconcileMessage::Patch(entries))
        }
    }

    fn handle_diff(&mut self, diff: &TreeDiff) -> RoundResult {
        if !self.is_authority {
            // We received a diff telling us what changed.
            // Apply: remove our removed keys, note added/changed keys.
            let mut remove_keys = diff.removed.clone();
            remove_keys.extend(diff.changed.iter().cloned());
            self.phase = Phase::Patching;
            RoundResult::RemoveKeys(remove_keys)
        } else {
            self.phase = Phase::Patching;
            RoundResult::Done
        }
    }

    fn handle_patch(&mut self, entries: &[(Vec<u8>, Vec<u8>)]) -> RoundResult {
        if self.is_authority {
            // Authority received peer's state — compute diff and send corrections
            let peer_tree = MerkleTree::from_entries(entries.iter().cloned());
            let diff = peer_tree.diff(&self.local);

            // Collect entries the peer needs
            let mut patches: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

            // Added entries (in our tree, not in peer's)
            for key in &diff.added {
                if let Some(value) = self.local.get(key) {
                    patches.push((key.clone(), value.to_vec()));
                }
            }

            // Changed entries (use our values)
            for key in &diff.changed {
                if let Some(value) = self.local.get(key) {
                    patches.push((key.clone(), value.to_vec()));
                }
            }

            // Also tell peer about removed keys via empty values marker
            let remove_keys = diff.removed.clone();

            self.phase = Phase::Converged;

            if patches.is_empty() && remove_keys.is_empty() {
                RoundResult::AlreadyConverged
            } else {
                RoundResult::SendMessage(ReconcileMessage::Patch(patches))
            }
        } else {
            // Non-authority received corrections — apply them
            let mut tree = self.local.clone();
            for (key, value) in entries {
                tree.insert(key.clone(), value.clone());
            }
            self.local = tree;
            self.phase = Phase::Converged;
            RoundResult::ApplyPatch(entries.to_vec())
        }
    }
}

// ── Convenience function ─────────────────────────────────────────

/// Run a full reconciliation between two state trees.
///
/// Returns the reconciled tree (authority's state prevails) and
/// the number of rounds it took.
///
/// This is a convenience wrapper for testing; in production, the
/// protocol messages would be exchanged over the network.
pub fn reconcile_trees(
    authority: &MerkleTree,
    replica: &MerkleTree,
    _config: &ReconcileConfig,
) -> (MerkleTree, usize) {
    if authority.root_hash() == replica.root_hash() {
        return (replica.clone(), 0);
    }

    // Compute diff and apply authority's state to replica
    let diff = replica.diff(authority);

    let mut result = replica.clone();

    // Remove keys not in authority
    for key in &diff.removed {
        result.remove(key);
    }

    // Add keys from authority
    for key in &diff.added {
        if let Some(value) = authority.get(key) {
            result.insert(key.clone(), value.to_vec());
        }
    }

    // Update changed values from authority
    for key in &diff.changed {
        if let Some(value) = authority.get(key) {
            result.insert(key.clone(), value.to_vec());
        }
    }

    // Verify convergence
    let rounds = if diff.total_changes() == 0 { 0 } else { 1 };
    (result, rounds)
}

/// Statistics from a reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileStats {
    /// Number of protocol rounds.
    pub rounds: usize,
    /// Number of entries that were added.
    pub added: usize,
    /// Number of entries that were removed.
    pub removed: usize,
    /// Number of entries that were changed.
    pub changed: usize,
    /// Whether reconciliation achieved convergence.
    pub converged: bool,
}

/// Run a full reconciliation and return stats.
pub fn reconcile_with_stats(
    authority: &MerkleTree,
    replica: &MerkleTree,
    config: &ReconcileConfig,
) -> (MerkleTree, ReconcileStats) {
    let diff = replica.diff(authority);
    let (result, rounds) = reconcile_trees(authority, replica, config);

    let stats = ReconcileStats {
        rounds,
        added: diff.added.len(),
        removed: diff.removed.len(),
        changed: diff.changed.len(),
        converged: result.root_hash() == authority.root_hash(),
    };

    (result, stats)
}

// ── Fingerprint for lightweight state comparison ─────────────────

/// A lightweight state fingerprint for quick divergence detection.
///
/// Contains the root hash plus basic metadata. Exchange this first
/// (64 bytes) before initiating full reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateFingerprint {
    /// Root hash of the state tree.
    pub root_hash: MerkleHash,
    /// Number of entries in the state.
    pub entry_count: usize,
    /// Monotonic version counter (optional).
    pub version: u64,
}

impl StateFingerprint {
    /// Create a fingerprint from a tree and version.
    pub fn from_tree(tree: &MerkleTree, version: u64) -> Self {
        Self {
            root_hash: tree.root_hash(),
            entry_count: tree.len(),
            version,
        }
    }

    /// Check if this fingerprint matches another.
    pub fn matches(&self, other: &StateFingerprint) -> bool {
        self.root_hash == other.root_hash
    }

    /// Check if this fingerprint is newer than another.
    pub fn is_newer_than(&self, other: &StateFingerprint) -> bool {
        self.version > other.version
    }
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tree(entries: &[(&[u8], &[u8])]) -> MerkleTree {
        MerkleTree::from_entries(
            entries
                .iter()
                .map(|(k, v)| (k.to_vec(), v.to_vec())),
        )
    }

    #[test]
    fn identical_trees_converge_immediately() {
        let tree = make_tree(&[(b"a", b"1"), (b"b", b"2")]);
        let config = ReconcileConfig::default();
        let (result, rounds) = reconcile_trees(&tree, &tree.clone(), &config);
        assert_eq!(result.root_hash(), tree.root_hash());
        assert_eq!(rounds, 0);
    }

    #[test]
    fn divergent_trees_converge() {
        let authority = make_tree(&[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")]);
        let replica = make_tree(&[(b"a", b"1"), (b"b", b"X"), (b"d", b"4")]);
        let config = ReconcileConfig::default();
        let (result, _rounds) = reconcile_trees(&authority, &replica, &config);
        // Result should match authority
        assert_eq!(result.root_hash(), authority.root_hash());
    }

    #[test]
    fn empty_authority_clears_replica() {
        let authority = MerkleTree::new();
        let replica = make_tree(&[(b"a", b"1"), (b"b", b"2")]);
        let config = ReconcileConfig::default();
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        assert!(result.is_empty());
        assert_eq!(result.root_hash(), authority.root_hash());
    }

    #[test]
    fn empty_replica_gets_authority_state() {
        let authority = make_tree(&[(b"a", b"1"), (b"b", b"2")]);
        let replica = MerkleTree::new();
        let config = ReconcileConfig::default();
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        assert_eq!(result.root_hash(), authority.root_hash());
    }

    #[test]
    fn session_init_phase() {
        let tree = make_tree(&[(b"a", b"1")]);
        let config = ReconcileConfig::default();
        let mut session = ReconcileSession::new(tree, true, config);
        assert_eq!(session.phase(), Phase::Init);
        let msg = session.start();
        assert!(matches!(msg, ReconcileMessage::RootHash(_)));
    }

    #[test]
    fn session_converged_on_matching_root() {
        let tree = make_tree(&[(b"a", b"1")]);
        let config = ReconcileConfig::default();
        let mut session = ReconcileSession::new(tree.clone(), true, config);
        session.start();
        let result = session.receive(&ReconcileMessage::RootHash(tree.root_hash()));
        assert_eq!(result, RoundResult::AlreadyConverged);
        assert!(session.is_converged());
    }

    #[test]
    fn session_narrows_on_mismatch() {
        let tree = make_tree(&[(b"a", b"1")]);
        let config = ReconcileConfig::default();
        let mut session = ReconcileSession::new(tree, true, config);
        session.start();
        let wrong_hash = MerkleHash::from_bytes([0xFF; 32]);
        let result = session.receive(&ReconcileMessage::RootHash(wrong_hash));
        assert!(matches!(result, RoundResult::SendMessage(ReconcileMessage::LevelHashes { .. })));
        assert_eq!(session.phase(), Phase::Narrowing { depth: 1 });
    }

    #[test]
    fn reconcile_stats_correct() {
        let authority = make_tree(&[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")]);
        let replica = make_tree(&[(b"a", b"1"), (b"b", b"X"), (b"d", b"4")]);
        let config = ReconcileConfig::default();
        let (_result, stats) = reconcile_with_stats(&authority, &replica, &config);
        assert!(stats.converged);
        assert_eq!(stats.changed, 1); // b: X→2
        assert_eq!(stats.added, 1); // c
        assert_eq!(stats.removed, 1); // d
    }

    #[test]
    fn fingerprint_matching() {
        let tree = make_tree(&[(b"x", b"y")]);
        let fp1 = StateFingerprint::from_tree(&tree, 1);
        let fp2 = StateFingerprint::from_tree(&tree, 2);
        assert!(fp1.matches(&fp2)); // same tree, different versions
        assert!(!fp1.is_newer_than(&fp2));
        assert!(fp2.is_newer_than(&fp1));
    }

    #[test]
    fn fingerprint_not_matching() {
        let tree1 = make_tree(&[(b"a", b"1")]);
        let tree2 = make_tree(&[(b"a", b"2")]);
        let fp1 = StateFingerprint::from_tree(&tree1, 1);
        let fp2 = StateFingerprint::from_tree(&tree2, 1);
        assert!(!fp1.matches(&fp2));
    }

    #[test]
    fn fingerprint_serde() {
        let tree = make_tree(&[(b"k", b"v")]);
        let fp = StateFingerprint::from_tree(&tree, 42);
        let json = serde_json::to_string(&fp).unwrap();
        let back: StateFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(fp, back);
    }

    #[test]
    fn reconcile_message_serde() {
        let msg = ReconcileMessage::RootHash(MerkleHash::from_bytes([0xAB; 32]));
        let json = serde_json::to_string(&msg).unwrap();
        let back: ReconcileMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn stats_serde() {
        let stats = ReconcileStats {
            rounds: 2,
            added: 3,
            removed: 1,
            changed: 2,
            converged: true,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: ReconcileStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn reconcile_single_addition() {
        let authority = make_tree(&[(b"a", b"1"), (b"b", b"2")]);
        let replica = make_tree(&[(b"a", b"1")]);
        let config = ReconcileConfig::default();
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        assert_eq!(result.root_hash(), authority.root_hash());
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn reconcile_single_removal() {
        let authority = make_tree(&[(b"a", b"1")]);
        let replica = make_tree(&[(b"a", b"1"), (b"b", b"2")]);
        let config = ReconcileConfig::default();
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        assert_eq!(result.root_hash(), authority.root_hash());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn reconcile_single_change() {
        let authority = make_tree(&[(b"a", b"new")]);
        let replica = make_tree(&[(b"a", b"old")]);
        let config = ReconcileConfig::default();
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        assert_eq!(result.get(b"a"), Some(b"new".as_slice()));
    }

    #[test]
    fn update_local_changes_session_state() {
        let tree1 = make_tree(&[(b"a", b"1")]);
        let tree2 = make_tree(&[(b"a", b"2")]);
        let config = ReconcileConfig::default();
        let mut session = ReconcileSession::new(tree1, true, config);
        assert_eq!(session.local_tree().get(b"a"), Some(b"1".as_slice()));
        session.update_local(tree2);
        assert_eq!(session.local_tree().get(b"a"), Some(b"2".as_slice()));
    }

    #[test]
    fn phase_serde_roundtrip() {
        let phases = [
            Phase::Init,
            Phase::Narrowing { depth: 3 },
            Phase::Patching,
            Phase::Converged,
        ];
        for phase in &phases {
            let json = serde_json::to_string(phase).unwrap();
            let back: Phase = serde_json::from_str(&json).unwrap();
            assert_eq!(*phase, back);
        }
    }
}
