//! Disk Serialization & Auto-Pruning Garbage Collector for ARS reflexes.
//!
//! Persists reflex definitions, evolution history, drift monitors, and
//! evidence ledgers to `~/.config/ft/reflexes/`. Includes a garbage
//! collector that prunes reflexes whose e-values collapse or which
//! repeatedly fail replay validation.
//!
//! # On-Disk Layout
//!
//! ```text
//! ~/.config/ft/reflexes/
//!   manifest.json          # ReflexManifest: all reflex metadata
//!   reflexes/
//!     <reflex_id>.json     # ReflexRecord: full reflex + evidence
//!   blacklist.json         # BlacklistEntry[]: permanently banned reflexes
//! ```

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::ars_blast_radius::MaturityTier;
use crate::ars_drift::EValueConfig;
use crate::ars_evidence::EvidenceVerdict;
use crate::ars_evolve::VersionStatus;
use crate::ars_fst::ReflexId;

// =============================================================================
// On-disk types
// =============================================================================

/// Manifest listing all known reflexes and their summaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflexManifest {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// Entries keyed by reflex ID.
    pub entries: BTreeMap<ReflexId, ManifestEntry>,
    /// Last save timestamp (ms).
    pub last_saved_ms: u64,
}

impl ReflexManifest {
    /// Create an empty manifest.
    pub fn new() -> Self {
        Self {
            schema_version: 1,
            entries: BTreeMap::new(),
            last_saved_ms: 0,
        }
    }

    /// Add or update an entry.
    pub fn upsert(&mut self, entry: ManifestEntry) {
        self.entries.insert(entry.reflex_id, entry);
    }

    /// Remove an entry by ID.
    pub fn remove(&mut self, id: ReflexId) -> Option<ManifestEntry> {
        self.entries.remove(&id)
    }

    /// Get entry by ID.
    pub fn get(&self, id: ReflexId) -> Option<&ManifestEntry> {
        self.entries.get(&id)
    }

    /// Count of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// All reflex IDs.
    pub fn ids(&self) -> Vec<ReflexId> {
        self.entries.keys().copied().collect()
    }

    /// Filter entries by status.
    pub fn by_status(&self, status: VersionStatus) -> Vec<&ManifestEntry> {
        self.entries
            .values()
            .filter(|e| e.status == status)
            .collect()
    }

    /// Filter entries by tier.
    pub fn by_tier(&self, tier: MaturityTier) -> Vec<&ManifestEntry> {
        self.entries.values().filter(|e| e.tier == tier).collect()
    }
}

impl Default for ReflexManifest {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary entry in the manifest (lightweight).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub reflex_id: ReflexId,
    pub cluster_id: String,
    pub version: u32,
    pub status: VersionStatus,
    pub tier: MaturityTier,
    pub successes: u64,
    pub failures: u64,
    pub e_value: f64,
    pub is_drifted: bool,
    pub evidence_verdict: EvidenceVerdict,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Full reflex record persisted to individual file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflexRecord {
    pub reflex_id: ReflexId,
    pub cluster_id: String,
    pub version: u32,
    pub trigger_key: Vec<u8>,
    pub commands: Vec<String>,
    pub status: VersionStatus,
    pub tier: MaturityTier,
    pub successes: u64,
    pub failures: u64,
    pub consecutive_failures: u64,
    /// Drift monitor state.
    pub drift_state: DriftSnapshot,
    /// Evidence summary.
    pub evidence_summary: EvidenceSummary,
    /// Parent lineage.
    pub parent_reflex_id: Option<ReflexId>,
    pub parent_version: Option<u32>,
    /// Timestamps.
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Snapshot of e-value drift monitor state for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftSnapshot {
    pub e_value: f64,
    pub null_rate: f64,
    pub total_observations: usize,
    pub post_cal_successes: usize,
    pub post_cal_observations: usize,
    pub drift_count: usize,
    pub calibrated: bool,
    pub config: EValueConfig,
}

/// Compact evidence summary for persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSummary {
    pub entry_count: usize,
    pub is_complete: bool,
    pub overall_verdict: EvidenceVerdict,
    pub root_hash: String,
    pub categories: Vec<String>,
}

/// Blacklisted reflex entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlacklistEntry {
    pub reflex_id: ReflexId,
    pub cluster_id: String,
    pub reason: BlacklistReason,
    pub blacklisted_at_ms: u64,
    pub final_e_value: f64,
    pub final_failures: u64,
}

/// Why a reflex was blacklisted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum BlacklistReason {
    /// E-value collapsed to near-zero (severe drift).
    EValueCollapse { final_e_value: f64 },
    /// Repeated replay validation failures.
    ReplayFailures { consecutive: u64 },
    /// Operator manual ban.
    OperatorBan { note: String },
    /// Exceeded maximum consecutive execution failures.
    ExecutionFailures { consecutive: u64 },
}

// =============================================================================
// Pruning engine
// =============================================================================

/// Configuration for auto-pruning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneConfig {
    /// E-value below this → candidate for pruning.
    pub e_value_collapse_threshold: f64,
    /// Consecutive failures above this → candidate for pruning.
    pub max_consecutive_failures: u64,
    /// Replay pass rate below this → candidate for pruning.
    pub min_replay_pass_rate: f64,
    /// Minimum age (ms) before eligible for pruning.
    pub min_age_ms: u64,
    /// Whether to auto-blacklist pruned reflexes.
    pub auto_blacklist: bool,
    /// Maximum number of deprecated reflexes to keep.
    pub max_deprecated_keep: usize,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            e_value_collapse_threshold: 0.01,
            max_consecutive_failures: 10,
            min_replay_pass_rate: 0.3,
            min_age_ms: 3_600_000, // 1 hour
            auto_blacklist: true,
            max_deprecated_keep: 100,
        }
    }
}

/// Result of a pruning operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneResult {
    /// IDs pruned due to e-value collapse.
    pub e_value_pruned: Vec<ReflexId>,
    /// IDs pruned due to consecutive failures.
    pub failure_pruned: Vec<ReflexId>,
    /// IDs pruned due to deprecated overflow.
    pub deprecated_pruned: Vec<ReflexId>,
    /// New blacklist entries created.
    pub blacklisted: Vec<BlacklistEntry>,
}

impl PruneResult {
    /// Total reflexes pruned.
    pub fn total_pruned(&self) -> usize {
        self.e_value_pruned.len() + self.failure_pruned.len() + self.deprecated_pruned.len()
    }

    /// Whether anything was pruned.
    pub fn is_empty(&self) -> bool {
        self.total_pruned() == 0
    }
}

/// Pruning engine that identifies and removes unhealthy reflexes.
pub struct PruneEngine {
    config: PruneConfig,
    total_prune_runs: u64,
    total_pruned: u64,
    total_blacklisted: u64,
}

impl PruneEngine {
    /// Create a new prune engine.
    pub fn new(config: PruneConfig) -> Self {
        Self {
            config,
            total_prune_runs: 0,
            total_pruned: 0,
            total_blacklisted: 0,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(PruneConfig::default())
    }

    /// Run pruning on a manifest, returning what should be removed.
    pub fn evaluate(
        &mut self,
        manifest: &ReflexManifest,
        records: &HashMap<ReflexId, ReflexRecord>,
        blacklist: &HashSet<ReflexId>,
        now_ms: u64,
    ) -> PruneResult {
        self.total_prune_runs += 1;

        let mut result = PruneResult {
            e_value_pruned: Vec::new(),
            failure_pruned: Vec::new(),
            deprecated_pruned: Vec::new(),
            blacklisted: Vec::new(),
        };

        for (id, entry) in &manifest.entries {
            // Skip already blacklisted.
            if blacklist.contains(id) {
                continue;
            }

            // Skip if too young.
            if now_ms.saturating_sub(entry.created_at_ms) < self.config.min_age_ms {
                continue;
            }

            // Skip disabled (already manually handled).
            if entry.status == VersionStatus::Disabled {
                continue;
            }

            // Check e-value collapse.
            if let Some(record) = records.get(id) {
                if record.drift_state.calibrated
                    && record.drift_state.e_value < self.config.e_value_collapse_threshold
                    && record.drift_state.total_observations > 0
                {
                    result.e_value_pruned.push(*id);
                    if self.config.auto_blacklist {
                        result.blacklisted.push(BlacklistEntry {
                            reflex_id: *id,
                            cluster_id: entry.cluster_id.clone(),
                            reason: BlacklistReason::EValueCollapse {
                                final_e_value: record.drift_state.e_value,
                            },
                            blacklisted_at_ms: now_ms,
                            final_e_value: record.drift_state.e_value,
                            final_failures: record.failures,
                        });
                    }
                    continue;
                }

                // Check consecutive failures.
                if record.consecutive_failures >= self.config.max_consecutive_failures {
                    result.failure_pruned.push(*id);
                    if self.config.auto_blacklist {
                        result.blacklisted.push(BlacklistEntry {
                            reflex_id: *id,
                            cluster_id: entry.cluster_id.clone(),
                            reason: BlacklistReason::ExecutionFailures {
                                consecutive: record.consecutive_failures,
                            },
                            blacklisted_at_ms: now_ms,
                            final_e_value: record.drift_state.e_value,
                            final_failures: record.failures,
                        });
                    }
                }
            }
        }

        // Prune excess deprecated reflexes (keep newest).
        let mut deprecated: Vec<(ReflexId, u64)> = manifest
            .entries
            .iter()
            .filter(|(id, e)| {
                e.status == VersionStatus::Deprecated
                    && !blacklist.contains(id)
                    && !result.e_value_pruned.contains(id)
                    && !result.failure_pruned.contains(id)
            })
            .map(|(id, e)| (*id, e.updated_at_ms))
            .collect();

        if deprecated.len() > self.config.max_deprecated_keep {
            // Sort by update time ascending (oldest first).
            deprecated.sort_by_key(|&(_, ts)| ts);
            let excess = deprecated.len() - self.config.max_deprecated_keep;
            for (id, _) in deprecated.iter().take(excess) {
                result.deprecated_pruned.push(*id);
            }
        }

        self.total_pruned += result.total_pruned() as u64;
        self.total_blacklisted += result.blacklisted.len() as u64;

        result
    }

    /// Apply a prune result to a manifest (mutates in place).
    pub fn apply(result: &PruneResult, manifest: &mut ReflexManifest) -> usize {
        let mut removed = 0;
        for id in &result.e_value_pruned {
            if manifest.remove(*id).is_some() {
                removed += 1;
            }
        }
        for id in &result.failure_pruned {
            if manifest.remove(*id).is_some() {
                removed += 1;
            }
        }
        for id in &result.deprecated_pruned {
            if manifest.remove(*id).is_some() {
                removed += 1;
            }
        }
        removed
    }

    /// Get prune statistics.
    pub fn stats(&self) -> PruneStats {
        PruneStats {
            total_prune_runs: self.total_prune_runs,
            total_pruned: self.total_pruned,
            total_blacklisted: self.total_blacklisted,
        }
    }

    /// Get config.
    pub fn config(&self) -> &PruneConfig {
        &self.config
    }
}

/// Pruning statistics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PruneStats {
    pub total_prune_runs: u64,
    pub total_pruned: u64,
    pub total_blacklisted: u64,
}

// =============================================================================
// Serialization store (in-memory model, I/O done by caller)
// =============================================================================

/// In-memory reflex store that can serialize to/from JSON.
pub struct ReflexStore {
    manifest: ReflexManifest,
    records: HashMap<ReflexId, ReflexRecord>,
    blacklist: Vec<BlacklistEntry>,
    blacklist_ids: HashSet<ReflexId>,
    dirty: bool,
}

impl ReflexStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            manifest: ReflexManifest::new(),
            records: HashMap::new(),
            blacklist: Vec::new(),
            blacklist_ids: HashSet::new(),
            dirty: false,
        }
    }

    /// Load from serialized parts.
    pub fn from_parts(
        manifest: ReflexManifest,
        records: HashMap<ReflexId, ReflexRecord>,
        blacklist: Vec<BlacklistEntry>,
    ) -> Self {
        let blacklist_ids: HashSet<ReflexId> = blacklist.iter().map(|b| b.reflex_id).collect();
        Self {
            manifest,
            records,
            blacklist,
            blacklist_ids,
            dirty: false,
        }
    }

    /// Insert or update a reflex record.
    pub fn upsert(&mut self, record: ReflexRecord) {
        let entry = ManifestEntry {
            reflex_id: record.reflex_id,
            cluster_id: record.cluster_id.clone(),
            version: record.version,
            status: record.status,
            tier: record.tier,
            successes: record.successes,
            failures: record.failures,
            e_value: record.drift_state.e_value,
            is_drifted: record.drift_state.drift_count > 0,
            evidence_verdict: record.evidence_summary.overall_verdict,
            created_at_ms: record.created_at_ms,
            updated_at_ms: record.updated_at_ms,
        };
        self.manifest.upsert(entry);
        self.records.insert(record.reflex_id, record);
        self.dirty = true;
    }

    /// Get a reflex record by ID.
    pub fn get(&self, id: ReflexId) -> Option<&ReflexRecord> {
        self.records.get(&id)
    }

    /// Get manifest entry by ID.
    pub fn get_entry(&self, id: ReflexId) -> Option<&ManifestEntry> {
        self.manifest.get(id)
    }

    /// Remove a reflex by ID.
    pub fn remove(&mut self, id: ReflexId) -> Option<ReflexRecord> {
        self.manifest.remove(id);
        self.dirty = true;
        self.records.remove(&id)
    }

    /// Add a blacklist entry.
    pub fn blacklist(&mut self, entry: BlacklistEntry) {
        self.blacklist_ids.insert(entry.reflex_id);
        self.remove(entry.reflex_id);
        self.blacklist.push(entry);
        self.dirty = true;
    }

    /// Check if a reflex is blacklisted.
    pub fn is_blacklisted(&self, id: ReflexId) -> bool {
        self.blacklist_ids.contains(&id)
    }

    /// Get all blacklist entries.
    pub fn blacklist_entries(&self) -> &[BlacklistEntry] {
        &self.blacklist
    }

    /// Get the manifest.
    pub fn manifest(&self) -> &ReflexManifest {
        &self.manifest
    }

    /// Get all records.
    pub fn records(&self) -> &HashMap<ReflexId, ReflexRecord> {
        &self.records
    }

    /// Count of active reflexes.
    pub fn len(&self) -> usize {
        self.manifest.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.manifest.is_empty()
    }

    /// Whether the store has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Mark the store as clean (after saving).
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    /// Run pruning and apply results.
    pub fn prune(&mut self, engine: &mut PruneEngine, now_ms: u64) -> PruneResult {
        let result = engine.evaluate(&self.manifest, &self.records, &self.blacklist_ids, now_ms);

        // Apply blacklisting first.
        for bl in &result.blacklisted {
            self.blacklist_ids.insert(bl.reflex_id);
            self.blacklist.push(bl.clone());
        }

        // Remove pruned records.
        PruneEngine::apply(&result, &mut self.manifest);
        for id in &result.e_value_pruned {
            self.records.remove(id);
        }
        for id in &result.failure_pruned {
            self.records.remove(id);
        }
        for id in &result.deprecated_pruned {
            self.records.remove(id);
        }

        if result.total_pruned() > 0 {
            self.dirty = true;
        }

        result
    }

    /// Serialize manifest to JSON string.
    pub fn serialize_manifest(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.manifest)
    }

    /// Serialize a single record to JSON string.
    pub fn serialize_record(&self, id: ReflexId) -> Option<Result<String, serde_json::Error>> {
        self.records.get(&id).map(serde_json::to_string_pretty)
    }

    /// Serialize blacklist to JSON string.
    pub fn serialize_blacklist(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&self.blacklist)
    }

    /// List reflexes matching filters.
    pub fn list(
        &self,
        status: Option<VersionStatus>,
        tier: Option<MaturityTier>,
    ) -> Vec<&ManifestEntry> {
        self.manifest
            .entries
            .values()
            .filter(|e| {
                status.as_ref().is_none_or(|s| e.status == *s) && tier.is_none_or(|t| e.tier == t)
            })
            .collect()
    }
}

impl Default for ReflexStore {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_drift_snapshot(e_value: f64, calibrated: bool) -> DriftSnapshot {
        DriftSnapshot {
            e_value,
            null_rate: 0.85,
            total_observations: 100,
            post_cal_successes: 80,
            post_cal_observations: 100,
            drift_count: 0,
            calibrated,
            config: EValueConfig::default(),
        }
    }

    fn make_evidence_summary() -> EvidenceSummary {
        EvidenceSummary {
            entry_count: 3,
            is_complete: true,
            overall_verdict: EvidenceVerdict::Support,
            root_hash: "abc123".to_string(),
            categories: vec!["ChangeDetection".to_string()],
        }
    }

    fn make_record(id: ReflexId, cluster: &str, tier: MaturityTier) -> ReflexRecord {
        ReflexRecord {
            reflex_id: id,
            cluster_id: cluster.to_string(),
            version: 1,
            trigger_key: vec![1, 2, 3],
            commands: vec!["cmd".to_string()],
            status: VersionStatus::Active,
            tier,
            successes: 10,
            failures: 0,
            consecutive_failures: 0,
            drift_state: make_drift_snapshot(5.0, true),
            evidence_summary: make_evidence_summary(),
            parent_reflex_id: None,
            parent_version: None,
            created_at_ms: 1000,
            updated_at_ms: 2000,
        }
    }

    // ---- Manifest ----

    #[test]
    fn manifest_new_is_empty() {
        let m = ReflexManifest::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn manifest_upsert_and_get() {
        let mut m = ReflexManifest::new();
        let entry = ManifestEntry {
            reflex_id: 1,
            cluster_id: "c1".to_string(),
            version: 1,
            status: VersionStatus::Active,
            tier: MaturityTier::Incubating,
            successes: 0,
            failures: 0,
            e_value: 1.0,
            is_drifted: false,
            evidence_verdict: EvidenceVerdict::Neutral,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        };
        m.upsert(entry);
        assert_eq!(m.len(), 1);
        assert!(m.get(1).is_some());
        assert!(m.get(2).is_none());
    }

    #[test]
    fn manifest_remove() {
        let mut m = ReflexManifest::new();
        let entry = ManifestEntry {
            reflex_id: 1,
            cluster_id: "c1".to_string(),
            version: 1,
            status: VersionStatus::Active,
            tier: MaturityTier::Graduated,
            successes: 10,
            failures: 0,
            e_value: 5.0,
            is_drifted: false,
            evidence_verdict: EvidenceVerdict::Support,
            created_at_ms: 1000,
            updated_at_ms: 2000,
        };
        m.upsert(entry);
        assert_eq!(m.len(), 1);
        m.remove(1);
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn manifest_ids() {
        let mut m = ReflexManifest::new();
        for i in 1..=3 {
            m.upsert(ManifestEntry {
                reflex_id: i,
                cluster_id: "c".to_string(),
                version: 1,
                status: VersionStatus::Active,
                tier: MaturityTier::Incubating,
                successes: 0,
                failures: 0,
                e_value: 1.0,
                is_drifted: false,
                evidence_verdict: EvidenceVerdict::Neutral,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            });
        }
        let ids = m.ids();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn manifest_by_status() {
        let mut m = ReflexManifest::new();
        for (i, status) in [
            VersionStatus::Active,
            VersionStatus::Deprecated,
            VersionStatus::Active,
        ]
        .iter()
        .enumerate()
        {
            m.upsert(ManifestEntry {
                reflex_id: i as u64,
                cluster_id: "c".to_string(),
                version: 1,
                status: status.clone(),
                tier: MaturityTier::Incubating,
                successes: 0,
                failures: 0,
                e_value: 1.0,
                is_drifted: false,
                evidence_verdict: EvidenceVerdict::Neutral,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            });
        }
        assert_eq!(m.by_status(VersionStatus::Active).len(), 2);
        assert_eq!(m.by_status(VersionStatus::Deprecated).len(), 1);
    }

    #[test]
    fn manifest_by_tier() {
        let mut m = ReflexManifest::new();
        for (i, tier) in [
            MaturityTier::Incubating,
            MaturityTier::Graduated,
            MaturityTier::Incubating,
        ]
        .iter()
        .enumerate()
        {
            m.upsert(ManifestEntry {
                reflex_id: i as u64,
                cluster_id: "c".to_string(),
                version: 1,
                status: VersionStatus::Active,
                tier: *tier,
                successes: 0,
                failures: 0,
                e_value: 1.0,
                is_drifted: false,
                evidence_verdict: EvidenceVerdict::Neutral,
                created_at_ms: 1000,
                updated_at_ms: 1000,
            });
        }
        assert_eq!(m.by_tier(MaturityTier::Incubating).len(), 2);
        assert_eq!(m.by_tier(MaturityTier::Graduated).len(), 1);
    }

    // ---- Store ----

    #[test]
    fn store_new_is_empty() {
        let store = ReflexStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(!store.is_dirty());
    }

    #[test]
    fn store_upsert_marks_dirty() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, "c1", MaturityTier::Incubating));
        assert!(store.is_dirty());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_get_record() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(42, "c1", MaturityTier::Graduated));
        let r = store.get(42).unwrap();
        assert_eq!(r.reflex_id, 42);
        assert_eq!(r.cluster_id, "c1");
    }

    #[test]
    fn store_remove_record() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, "c1", MaturityTier::Incubating));
        assert_eq!(store.len(), 1);
        let removed = store.remove(1);
        assert!(removed.is_some());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn store_blacklist_removes_from_manifest() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, "c1", MaturityTier::Incubating));
        assert_eq!(store.len(), 1);
        store.blacklist(BlacklistEntry {
            reflex_id: 1,
            cluster_id: "c1".to_string(),
            reason: BlacklistReason::OperatorBan {
                note: "test".to_string(),
            },
            blacklisted_at_ms: 5000,
            final_e_value: 0.001,
            final_failures: 5,
        });
        assert_eq!(store.len(), 0);
        assert!(store.is_blacklisted(1));
    }

    #[test]
    fn store_mark_clean() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, "c1", MaturityTier::Incubating));
        assert!(store.is_dirty());
        store.mark_clean();
        assert!(!store.is_dirty());
    }

    #[test]
    fn store_list_filter_by_status() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, "c1", MaturityTier::Incubating));
        let mut r2 = make_record(2, "c1", MaturityTier::Graduated);
        r2.status = VersionStatus::Deprecated;
        store.upsert(r2);
        let active = store.list(Some(VersionStatus::Active), None);
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn store_list_filter_by_tier() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, "c1", MaturityTier::Incubating));
        store.upsert(make_record(2, "c1", MaturityTier::Graduated));
        let grad = store.list(None, Some(MaturityTier::Graduated));
        assert_eq!(grad.len(), 1);
    }

    #[test]
    fn store_from_parts() {
        let mut manifest = ReflexManifest::new();
        manifest.upsert(ManifestEntry {
            reflex_id: 1,
            cluster_id: "c1".to_string(),
            version: 1,
            status: VersionStatus::Active,
            tier: MaturityTier::Incubating,
            successes: 0,
            failures: 0,
            e_value: 1.0,
            is_drifted: false,
            evidence_verdict: EvidenceVerdict::Neutral,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        });
        let mut records = HashMap::new();
        records.insert(1, make_record(1, "c1", MaturityTier::Incubating));
        let blacklist = vec![BlacklistEntry {
            reflex_id: 99,
            cluster_id: "c2".to_string(),
            reason: BlacklistReason::EValueCollapse { final_e_value: 0.0 },
            blacklisted_at_ms: 500,
            final_e_value: 0.0,
            final_failures: 20,
        }];
        let store = ReflexStore::from_parts(manifest, records, blacklist);
        assert_eq!(store.len(), 1);
        assert!(store.is_blacklisted(99));
        assert!(!store.is_dirty());
    }

    // ---- Serialization ----

    #[test]
    fn serialize_manifest_roundtrip() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, "c1", MaturityTier::Graduated));
        let json = store.serialize_manifest().unwrap();
        let decoded: ReflexManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.len(), 1);
        assert!(decoded.get(1).is_some());
    }

    #[test]
    fn serialize_record_roundtrip() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(42, "c1", MaturityTier::Veteran));
        let json = store.serialize_record(42).unwrap().unwrap();
        let decoded: ReflexRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.reflex_id, 42);
        assert_eq!(decoded.tier, MaturityTier::Veteran);
    }

    #[test]
    fn serialize_blacklist_roundtrip() {
        let mut store = ReflexStore::new();
        store.blacklist(BlacklistEntry {
            reflex_id: 7,
            cluster_id: "c1".to_string(),
            reason: BlacklistReason::ReplayFailures { consecutive: 5 },
            blacklisted_at_ms: 1000,
            final_e_value: 0.5,
            final_failures: 10,
        });
        let json = store.serialize_blacklist().unwrap();
        let decoded: Vec<BlacklistEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].reflex_id, 7);
    }

    // ---- Pruning ----

    #[test]
    fn prune_e_value_collapse() {
        let mut store = ReflexStore::new();
        let mut r = make_record(1, "c1", MaturityTier::Incubating);
        r.drift_state.e_value = 0.001;
        r.drift_state.calibrated = true;
        r.created_at_ms = 0;
        store.upsert(r);

        let mut engine = PruneEngine::with_defaults();
        let result = store.prune(&mut engine, 10_000_000);
        assert_eq!(result.e_value_pruned, vec![1]);
        assert_eq!(store.len(), 0);
        assert!(store.is_blacklisted(1));
    }

    #[test]
    fn prune_consecutive_failures() {
        let mut store = ReflexStore::new();
        let mut r = make_record(1, "c1", MaturityTier::Incubating);
        r.consecutive_failures = 15;
        r.created_at_ms = 0;
        store.upsert(r);

        let mut engine = PruneEngine::with_defaults();
        let result = store.prune(&mut engine, 10_000_000);
        assert_eq!(result.failure_pruned, vec![1]);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn prune_skips_young_reflexes() {
        let mut store = ReflexStore::new();
        let mut r = make_record(1, "c1", MaturityTier::Incubating);
        r.drift_state.e_value = 0.001;
        r.drift_state.calibrated = true;
        r.created_at_ms = 9_999_000; // Very recent.
        store.upsert(r);

        let mut engine = PruneEngine::with_defaults();
        let result = store.prune(&mut engine, 10_000_000);
        assert!(result.is_empty());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn prune_skips_disabled() {
        let mut store = ReflexStore::new();
        let mut r = make_record(1, "c1", MaturityTier::Incubating);
        r.drift_state.e_value = 0.001;
        r.drift_state.calibrated = true;
        r.status = VersionStatus::Disabled;
        r.created_at_ms = 0;
        store.upsert(r);

        let mut engine = PruneEngine::with_defaults();
        let result = store.prune(&mut engine, 10_000_000);
        assert!(result.is_empty());
    }

    #[test]
    fn prune_deprecated_overflow() {
        let mut store = ReflexStore::new();
        let config = PruneConfig {
            max_deprecated_keep: 2,
            ..Default::default()
        };

        for i in 0..5u64 {
            let mut r = make_record(i, "c1", MaturityTier::Graduated);
            r.status = VersionStatus::Deprecated;
            r.created_at_ms = 0;
            r.updated_at_ms = i * 1000; // Ascending timestamps.
            store.upsert(r);
        }

        let mut engine = PruneEngine::new(config);
        let result = store.prune(&mut engine, 10_000_000);
        assert_eq!(result.deprecated_pruned.len(), 3);
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn prune_healthy_reflexes_untouched() {
        let mut store = ReflexStore::new();
        store.upsert(make_record(1, "c1", MaturityTier::Graduated));
        store.upsert(make_record(2, "c2", MaturityTier::Veteran));

        let mut engine = PruneEngine::with_defaults();
        let result = store.prune(&mut engine, 10_000_000);
        assert!(result.is_empty());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn prune_stats_accumulate() {
        let mut store = ReflexStore::new();
        let mut r = make_record(1, "c1", MaturityTier::Incubating);
        r.drift_state.e_value = 0.001;
        r.drift_state.calibrated = true;
        r.created_at_ms = 0;
        store.upsert(r);

        let mut engine = PruneEngine::with_defaults();
        store.prune(&mut engine, 10_000_000);

        let stats = engine.stats();
        assert_eq!(stats.total_prune_runs, 1);
        assert_eq!(stats.total_pruned, 1);
        assert_eq!(stats.total_blacklisted, 1);
    }

    #[test]
    fn prune_without_auto_blacklist() {
        let config = PruneConfig {
            auto_blacklist: false,
            ..Default::default()
        };
        let mut store = ReflexStore::new();
        let mut r = make_record(1, "c1", MaturityTier::Incubating);
        r.drift_state.e_value = 0.001;
        r.drift_state.calibrated = true;
        r.created_at_ms = 0;
        store.upsert(r);

        let mut engine = PruneEngine::new(config);
        let result = store.prune(&mut engine, 10_000_000);
        assert_eq!(result.e_value_pruned.len(), 1);
        assert!(result.blacklisted.is_empty());
        assert!(!store.is_blacklisted(1));
    }

    // ---- Serde roundtrips ----

    #[test]
    fn record_serde_roundtrip() {
        let r = make_record(42, "net", MaturityTier::Veteran);
        let json = serde_json::to_string(&r).unwrap();
        let decoded: ReflexRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.reflex_id, 42);
        assert_eq!(decoded.cluster_id, "net");
    }

    #[test]
    fn blacklist_entry_serde_roundtrip() {
        let entry = BlacklistEntry {
            reflex_id: 5,
            cluster_id: "c1".to_string(),
            reason: BlacklistReason::EValueCollapse {
                final_e_value: 0.001,
            },
            blacklisted_at_ms: 1000,
            final_e_value: 0.001,
            final_failures: 7,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: BlacklistEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, entry);
    }

    #[test]
    fn blacklist_reason_variants_serde() {
        let reasons = [
            BlacklistReason::EValueCollapse { final_e_value: 0.0 },
            BlacklistReason::ReplayFailures { consecutive: 5 },
            BlacklistReason::OperatorBan {
                note: "test".to_string(),
            },
            BlacklistReason::ExecutionFailures { consecutive: 10 },
        ];
        for reason in &reasons {
            let json = serde_json::to_string(reason).unwrap();
            let decoded: BlacklistReason = serde_json::from_str(&json).unwrap();
            assert_eq!(&decoded, reason);
        }
    }

    #[test]
    fn prune_config_serde_roundtrip() {
        let config = PruneConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let decoded: PruneConfig = serde_json::from_str(&json).unwrap();
        let diff = (decoded.e_value_collapse_threshold - config.e_value_collapse_threshold).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn prune_result_total() {
        let result = PruneResult {
            e_value_pruned: vec![1, 2],
            failure_pruned: vec![3],
            deprecated_pruned: vec![4, 5, 6],
            blacklisted: vec![],
        };
        assert_eq!(result.total_pruned(), 6);
        assert!(!result.is_empty());
    }

    #[test]
    fn prune_result_empty() {
        let result = PruneResult {
            e_value_pruned: vec![],
            failure_pruned: vec![],
            deprecated_pruned: vec![],
            blacklisted: vec![],
        };
        assert_eq!(result.total_pruned(), 0);
        assert!(result.is_empty());
    }

    #[test]
    fn drift_snapshot_serde_roundtrip() {
        let snap = make_drift_snapshot(5.0, true);
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: DriftSnapshot = serde_json::from_str(&json).unwrap();
        let diff = (decoded.e_value - 5.0).abs();
        assert!(diff < 1e-10);
        assert!(decoded.calibrated);
    }

    #[test]
    fn evidence_summary_serde_roundtrip() {
        let s = make_evidence_summary();
        let json = serde_json::to_string(&s).unwrap();
        let decoded: EvidenceSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.entry_count, s.entry_count);
        assert_eq!(decoded.root_hash, s.root_hash);
    }

    #[test]
    fn manifest_serde_roundtrip() {
        let mut m = ReflexManifest::new();
        m.upsert(ManifestEntry {
            reflex_id: 1,
            cluster_id: "c1".to_string(),
            version: 2,
            status: VersionStatus::Active,
            tier: MaturityTier::Graduated,
            successes: 50,
            failures: 2,
            e_value: 3.5,
            is_drifted: false,
            evidence_verdict: EvidenceVerdict::Support,
            created_at_ms: 1000,
            updated_at_ms: 2000,
        });
        m.last_saved_ms = 3000;
        let json = serde_json::to_string(&m).unwrap();
        let decoded: ReflexManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded.schema_version, 1);
        assert_eq!(decoded.last_saved_ms, 3000);
    }
}
