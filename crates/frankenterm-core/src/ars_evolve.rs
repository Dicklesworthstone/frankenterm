//! Reflex Evolution Engine for ARS.
//!
//! When a reflex is demoted (via drift detection), the agent resolves the
//! error via LLM, producing a new command sequence. The evolution engine
//! recognizes this as a reflex update: it synthesizes version 2, maps it
//! to version 1 as its parent, and promotes the new version while
//! deprecating the old one.
//!
//! # Lifecycle
//!
//! ```text
//! v1 (Drifted) → LLM resolves → new sequence → EvolutionEngine
//!                                                  ↓
//!                              v2 (Incubating) ← synthesize + tag
//!                                  ↓
//!                   v1 → Deprecated, v2 → active
//! ```
//!
//! # Versioning
//!
//! Each reflex version carries a monotonic version number, a parent link,
//! and a deprecation status. The engine maintains the full version graph
//! for audit trails.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ars_fst::ReflexId;

// =============================================================================
// Reflex version
// =============================================================================

/// A versioned reflex record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReflexVersion {
    /// Unique reflex ID.
    pub reflex_id: ReflexId,
    /// Version number (monotonically increasing per lineage).
    pub version: u32,
    /// Cluster this reflex belongs to.
    pub cluster_id: String,
    /// Trigger key (command signature).
    pub trigger_key: Vec<u8>,
    /// Command sequence (the action to take).
    pub commands: Vec<String>,
    /// Parent version (None for original v1).
    pub parent_version: Option<u32>,
    /// Parent reflex ID (None for original).
    pub parent_reflex_id: Option<ReflexId>,
    /// Current status.
    pub status: VersionStatus,
    /// Reason for creation.
    pub creation_reason: CreationReason,
    /// Timestamp of creation (ms since epoch).
    pub created_at_ms: u64,
}

/// Status of a reflex version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VersionStatus {
    /// Active and being used.
    Active,
    /// Active but under probation (newly evolved).
    Incubating,
    /// No longer used — replaced by a newer version.
    Deprecated,
    /// Explicitly disabled by operator.
    Disabled,
}

impl VersionStatus {
    /// Whether this version is usable for execution.
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Active | Self::Incubating)
    }

    /// Display name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::Incubating => "Incubating",
            Self::Deprecated => "Deprecated",
            Self::Disabled => "Disabled",
        }
    }
}

/// Why a reflex version was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreationReason {
    /// Original discovery.
    Original,
    /// Evolved from a drifted parent.
    DriftEvolution { parent_reflex_id: ReflexId },
    /// Manual operator edit.
    OperatorEdit,
    /// Merged from multiple sources.
    Merge { source_ids: Vec<ReflexId> },
}

// =============================================================================
// Evolution request / result
// =============================================================================

/// Request to evolve a reflex.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionRequest {
    /// The drifted reflex to evolve from.
    pub parent_reflex_id: ReflexId,
    /// Cluster of the parent.
    pub cluster_id: String,
    /// New trigger key (may be same or different).
    pub new_trigger_key: Vec<u8>,
    /// New command sequence from LLM resolution.
    pub new_commands: Vec<String>,
    /// Timestamp of request.
    pub timestamp_ms: u64,
}

/// Result of an evolution attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EvolutionResult {
    /// Successfully evolved.
    Evolved {
        new_reflex_id: ReflexId,
        new_version: u32,
        deprecated_reflex_id: ReflexId,
    },
    /// Parent not found.
    ParentNotFound { reflex_id: ReflexId },
    /// Parent already deprecated (double evolution).
    AlreadyDeprecated { reflex_id: ReflexId },
    /// Commands are empty.
    EmptyCommands,
    /// Too many versions in lineage.
    LineageTooDeep { depth: u32, max_depth: u32 },
}

impl EvolutionResult {
    /// Whether evolution succeeded.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Evolved { .. })
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the evolution engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvolutionConfig {
    /// Maximum lineage depth before refusing further evolution.
    pub max_lineage_depth: u32,
    /// Whether to auto-deprecate parents on evolution.
    pub auto_deprecate_parent: bool,
    /// Whether new evolutions start as Incubating (true) or Active (false).
    pub incubate_evolutions: bool,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            max_lineage_depth: 20,
            auto_deprecate_parent: true,
            incubate_evolutions: true,
        }
    }
}

// =============================================================================
// Evolution engine
// =============================================================================

/// Manages reflex version evolution.
pub struct EvolutionEngine {
    config: EvolutionConfig,
    /// All reflex versions indexed by reflex ID.
    versions: HashMap<ReflexId, ReflexVersion>,
    /// Next reflex ID to assign.
    next_id: ReflexId,
    /// Lineage graph: child → parent.
    lineage: HashMap<ReflexId, ReflexId>,
    /// Total evolutions performed.
    total_evolutions: u64,
    /// Total deprecations.
    total_deprecations: u64,
}

impl EvolutionEngine {
    /// Create with configuration.
    pub fn new(config: EvolutionConfig) -> Self {
        Self {
            config,
            versions: HashMap::new(),
            next_id: 1,
            lineage: HashMap::new(),
            total_evolutions: 0,
            total_deprecations: 0,
        }
    }

    /// Create with defaults.
    pub fn with_defaults() -> Self {
        Self::new(EvolutionConfig::default())
    }

    /// Register an original (v1) reflex.
    pub fn register_original(
        &mut self,
        cluster_id: &str,
        trigger_key: Vec<u8>,
        commands: Vec<String>,
        timestamp_ms: u64,
    ) -> ReflexId {
        let id = self.next_id;
        self.next_id += 1;

        let version = ReflexVersion {
            reflex_id: id,
            version: 1,
            cluster_id: cluster_id.to_string(),
            trigger_key,
            commands,
            parent_version: None,
            parent_reflex_id: None,
            status: VersionStatus::Active,
            creation_reason: CreationReason::Original,
            created_at_ms: timestamp_ms,
        };

        self.versions.insert(id, version);
        debug!(reflex_id = id, "registered original reflex v1");
        id
    }

    /// Evolve a drifted reflex into a new version.
    pub fn evolve(&mut self, request: &EvolutionRequest) -> EvolutionResult {
        // Validate parent exists.
        let parent = match self.versions.get(&request.parent_reflex_id) {
            Some(p) => p.clone(),
            None => {
                return EvolutionResult::ParentNotFound {
                    reflex_id: request.parent_reflex_id,
                };
            }
        };

        // Check if already deprecated.
        if parent.status == VersionStatus::Deprecated {
            return EvolutionResult::AlreadyDeprecated {
                reflex_id: request.parent_reflex_id,
            };
        }

        // Validate commands.
        if request.new_commands.is_empty() {
            return EvolutionResult::EmptyCommands;
        }

        // Check lineage depth.
        let depth = self.lineage_depth(request.parent_reflex_id);
        if depth >= self.config.max_lineage_depth {
            return EvolutionResult::LineageTooDeep {
                depth,
                max_depth: self.config.max_lineage_depth,
            };
        }

        // Create new version.
        let new_id = self.next_id;
        self.next_id += 1;
        let new_version_num = parent.version + 1;

        let initial_status = if self.config.incubate_evolutions {
            VersionStatus::Incubating
        } else {
            VersionStatus::Active
        };

        let new_version = ReflexVersion {
            reflex_id: new_id,
            version: new_version_num,
            cluster_id: request.cluster_id.clone(),
            trigger_key: request.new_trigger_key.clone(),
            commands: request.new_commands.clone(),
            parent_version: Some(parent.version),
            parent_reflex_id: Some(request.parent_reflex_id),
            status: initial_status,
            creation_reason: CreationReason::DriftEvolution {
                parent_reflex_id: request.parent_reflex_id,
            },
            created_at_ms: request.timestamp_ms,
        };

        self.versions.insert(new_id, new_version);
        self.lineage.insert(new_id, request.parent_reflex_id);
        self.total_evolutions += 1;

        // Auto-deprecate parent.
        if self.config.auto_deprecate_parent {
            if let Some(p) = self.versions.get_mut(&request.parent_reflex_id) {
                p.status = VersionStatus::Deprecated;
                self.total_deprecations += 1;
                debug!(
                    reflex_id = request.parent_reflex_id,
                    "deprecated parent reflex"
                );
            }
        }

        debug!(
            new_id = new_id,
            new_version = new_version_num,
            parent_id = request.parent_reflex_id,
            "reflex evolved"
        );

        EvolutionResult::Evolved {
            new_reflex_id: new_id,
            new_version: new_version_num,
            deprecated_reflex_id: request.parent_reflex_id,
        }
    }

    /// Promote an incubating reflex to active.
    pub fn promote(&mut self, reflex_id: ReflexId) -> bool {
        if let Some(v) = self.versions.get_mut(&reflex_id) {
            if v.status == VersionStatus::Incubating {
                v.status = VersionStatus::Active;
                debug!(reflex_id, "reflex promoted to Active");
                return true;
            }
        }
        false
    }

    /// Manually deprecate a reflex.
    pub fn deprecate(&mut self, reflex_id: ReflexId) -> bool {
        if let Some(v) = self.versions.get_mut(&reflex_id) {
            if v.status.is_usable() {
                v.status = VersionStatus::Deprecated;
                self.total_deprecations += 1;
                warn!(reflex_id, "reflex manually deprecated");
                return true;
            }
        }
        false
    }

    /// Disable a reflex (operator override).
    pub fn disable(&mut self, reflex_id: ReflexId) -> bool {
        if let Some(v) = self.versions.get_mut(&reflex_id) {
            v.status = VersionStatus::Disabled;
            return true;
        }
        false
    }

    /// Get a reflex version.
    pub fn get_version(&self, reflex_id: ReflexId) -> Option<&ReflexVersion> {
        self.versions.get(&reflex_id)
    }

    /// Get the full lineage (ancestors) of a reflex.
    pub fn lineage(&self, reflex_id: ReflexId) -> Vec<ReflexId> {
        let mut chain = Vec::new();
        let mut current = reflex_id;
        while let Some(&parent) = self.lineage.get(&current) {
            chain.push(parent);
            current = parent;
        }
        chain
    }

    /// Get lineage depth (0 for originals).
    pub fn lineage_depth(&self, reflex_id: ReflexId) -> u32 {
        self.lineage(reflex_id).len() as u32
    }

    /// Find the latest (most recent) version in a lineage.
    pub fn latest_in_lineage(&self, reflex_id: ReflexId) -> ReflexId {
        // Find all descendants.
        let mut latest_id = reflex_id;
        let mut latest_version = self
            .versions
            .get(&reflex_id)
            .map(|v| v.version)
            .unwrap_or(0);

        for (id, parent_id) in &self.lineage {
            if self.is_ancestor(reflex_id, *id) || *parent_id == reflex_id {
                if let Some(v) = self.versions.get(id) {
                    if v.version > latest_version {
                        latest_version = v.version;
                        latest_id = *id;
                    }
                }
            }
        }
        latest_id
    }

    /// Check if `ancestor` is an ancestor of `descendant`.
    fn is_ancestor(&self, ancestor: ReflexId, descendant: ReflexId) -> bool {
        let chain = self.lineage(descendant);
        chain.contains(&ancestor)
    }

    /// Get all active (usable) reflexes.
    pub fn active_reflexes(&self) -> Vec<&ReflexVersion> {
        self.versions
            .values()
            .filter(|v| v.status.is_usable())
            .collect()
    }

    /// Get all deprecated reflexes.
    pub fn deprecated_reflexes(&self) -> Vec<&ReflexVersion> {
        self.versions
            .values()
            .filter(|v| v.status == VersionStatus::Deprecated)
            .collect()
    }

    /// Get statistics.
    pub fn stats(&self) -> EvolutionStats {
        let mut by_status: HashMap<String, usize> = HashMap::new();
        for v in self.versions.values() {
            *by_status.entry(v.status.name().to_string()).or_insert(0) += 1;
        }
        EvolutionStats {
            total_reflexes: self.versions.len(),
            total_evolutions: self.total_evolutions,
            total_deprecations: self.total_deprecations,
            by_status,
            max_lineage_depth: self
                .versions
                .keys()
                .map(|&id| self.lineage_depth(id))
                .max()
                .unwrap_or(0),
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &EvolutionConfig {
        &self.config
    }

    /// Total registered reflexes.
    pub fn reflex_count(&self) -> usize {
        self.versions.len()
    }
}

/// Evolution statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvolutionStats {
    pub total_reflexes: usize,
    pub total_evolutions: u64,
    pub total_deprecations: u64,
    pub by_status: HashMap<String, usize>,
    pub max_lineage_depth: u32,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine() -> EvolutionEngine {
        EvolutionEngine::with_defaults()
    }

    fn make_request(parent_id: ReflexId) -> EvolutionRequest {
        EvolutionRequest {
            parent_reflex_id: parent_id,
            cluster_id: "c1".to_string(),
            new_trigger_key: vec![1, 2, 3],
            new_commands: vec!["ls -la".to_string()],
            timestamp_ms: 1000,
        }
    }

    // ---- VersionStatus ----

    #[test]
    fn active_is_usable() {
        assert!(VersionStatus::Active.is_usable());
    }

    #[test]
    fn incubating_is_usable() {
        assert!(VersionStatus::Incubating.is_usable());
    }

    #[test]
    fn deprecated_not_usable() {
        assert!(!VersionStatus::Deprecated.is_usable());
    }

    #[test]
    fn disabled_not_usable() {
        assert!(!VersionStatus::Disabled.is_usable());
    }

    #[test]
    fn status_names() {
        assert_eq!(VersionStatus::Active.name(), "Active");
        assert_eq!(VersionStatus::Incubating.name(), "Incubating");
        assert_eq!(VersionStatus::Deprecated.name(), "Deprecated");
        assert_eq!(VersionStatus::Disabled.name(), "Disabled");
    }

    // ---- Register originals ----

    #[test]
    fn register_original_assigns_v1() {
        let mut engine = make_engine();
        let id = engine.register_original("c1", vec![1], vec!["cmd".into()], 1000);
        let v = engine.get_version(id).unwrap();
        assert_eq!(v.version, 1);
        assert_eq!(v.status, VersionStatus::Active);
        assert!(v.parent_reflex_id.is_none());
    }

    #[test]
    fn register_assigns_unique_ids() {
        let mut engine = make_engine();
        let id1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        let id2 = engine.register_original("c1", vec![2], vec!["b".into()], 1000);
        assert_ne!(id1, id2);
    }

    // ---- Evolution ----

    #[test]
    fn evolve_creates_v2() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["old".into()], 1000);
        let result = engine.evolve(&make_request(v1));
        assert!(result.is_success());

        if let EvolutionResult::Evolved {
            new_reflex_id,
            new_version,
            deprecated_reflex_id,
        } = result
        {
            assert_eq!(new_version, 2);
            assert_eq!(deprecated_reflex_id, v1);

            let new = engine.get_version(new_reflex_id).unwrap();
            assert_eq!(new.status, VersionStatus::Incubating);
            assert_eq!(new.parent_reflex_id, Some(v1));
        }
    }

    #[test]
    fn evolve_deprecates_parent() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["old".into()], 1000);
        engine.evolve(&make_request(v1));

        let old = engine.get_version(v1).unwrap();
        assert_eq!(old.status, VersionStatus::Deprecated);
    }

    #[test]
    fn evolve_parent_not_found() {
        let mut engine = make_engine();
        let result = engine.evolve(&make_request(999));
        let is_not_found = matches!(result, EvolutionResult::ParentNotFound { .. });
        assert!(is_not_found);
    }

    #[test]
    fn evolve_already_deprecated() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["old".into()], 1000);
        engine.evolve(&make_request(v1)); // Deprecates v1.

        let result = engine.evolve(&make_request(v1)); // Try again.
        let is_deprecated = matches!(result, EvolutionResult::AlreadyDeprecated { .. });
        assert!(is_deprecated);
    }

    #[test]
    fn evolve_empty_commands() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["old".into()], 1000);
        let req = EvolutionRequest {
            parent_reflex_id: v1,
            cluster_id: "c1".to_string(),
            new_trigger_key: vec![1],
            new_commands: vec![],
            timestamp_ms: 1000,
        };
        let result = engine.evolve(&req);
        let is_empty = matches!(result, EvolutionResult::EmptyCommands);
        assert!(is_empty);
    }

    #[test]
    fn evolve_lineage_too_deep() {
        let config = EvolutionConfig {
            max_lineage_depth: 2,
            ..Default::default()
        };
        let mut engine = EvolutionEngine::new(config);

        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        let r1 = engine.evolve(&make_request(v1));
        let v2_id = if let EvolutionResult::Evolved { new_reflex_id, .. } = r1 {
            new_reflex_id
        } else {
            panic!("should evolve");
        };

        // Promote v2 so it can be evolved.
        engine.promote(v2_id);

        let r2 = engine.evolve(&make_request(v2_id));
        let v3_id = if let EvolutionResult::Evolved { new_reflex_id, .. } = r2 {
            new_reflex_id
        } else {
            panic!("should evolve");
        };

        engine.promote(v3_id);

        let r3 = engine.evolve(&make_request(v3_id));
        let is_too_deep = matches!(r3, EvolutionResult::LineageTooDeep { .. });
        assert!(is_too_deep);
    }

    // ---- Lineage ----

    #[test]
    fn lineage_empty_for_original() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        assert!(engine.lineage(v1).is_empty());
        assert_eq!(engine.lineage_depth(v1), 0);
    }

    #[test]
    fn lineage_tracks_chain() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);

        let r = engine.evolve(&make_request(v1));
        let v2 = if let EvolutionResult::Evolved { new_reflex_id, .. } = r {
            new_reflex_id
        } else {
            panic!("should evolve");
        };

        let chain = engine.lineage(v2);
        assert_eq!(chain, vec![v1]);
        assert_eq!(engine.lineage_depth(v2), 1);
    }

    // ---- Promote / Deprecate ----

    #[test]
    fn promote_incubating_to_active() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        let r = engine.evolve(&make_request(v1));
        let v2 = if let EvolutionResult::Evolved { new_reflex_id, .. } = r {
            new_reflex_id
        } else {
            panic!();
        };

        assert_eq!(
            engine.get_version(v2).unwrap().status,
            VersionStatus::Incubating
        );
        assert!(engine.promote(v2));
        assert_eq!(
            engine.get_version(v2).unwrap().status,
            VersionStatus::Active
        );
    }

    #[test]
    fn promote_non_incubating_fails() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        // Already active.
        assert!(!engine.promote(v1));
    }

    #[test]
    fn deprecate_active_reflex() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        assert!(engine.deprecate(v1));
        assert_eq!(
            engine.get_version(v1).unwrap().status,
            VersionStatus::Deprecated
        );
    }

    #[test]
    fn disable_reflex() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        assert!(engine.disable(v1));
        assert_eq!(
            engine.get_version(v1).unwrap().status,
            VersionStatus::Disabled
        );
    }

    // ---- Active / deprecated queries ----

    #[test]
    fn active_reflexes_filters_correctly() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        engine.register_original("c1", vec![2], vec!["b".into()], 1000);
        engine.evolve(&make_request(v1)); // Deprecates v1.

        let active = engine.active_reflexes();
        // v2 (original) + v2_evolved (incubating) = 2 active.
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn deprecated_reflexes_list() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        engine.evolve(&make_request(v1));

        let deprecated = engine.deprecated_reflexes();
        assert_eq!(deprecated.len(), 1);
        assert_eq!(deprecated[0].reflex_id, v1);
    }

    // ---- Stats ----

    #[test]
    fn stats_track_correctly() {
        let mut engine = make_engine();
        let v1 = engine.register_original("c1", vec![1], vec!["a".into()], 1000);
        engine.evolve(&make_request(v1));

        let stats = engine.stats();
        assert_eq!(stats.total_reflexes, 2); // v1 + v2.
        assert_eq!(stats.total_evolutions, 1);
        assert_eq!(stats.total_deprecations, 1);
        assert_eq!(stats.max_lineage_depth, 1);
    }

    // ---- Serde roundtrips ----

    #[test]
    fn reflex_version_serde_roundtrip() {
        let v = ReflexVersion {
            reflex_id: 1,
            version: 2,
            cluster_id: "c1".to_string(),
            trigger_key: vec![1, 2, 3],
            commands: vec!["ls".to_string()],
            parent_version: Some(1),
            parent_reflex_id: Some(0),
            status: VersionStatus::Incubating,
            creation_reason: CreationReason::DriftEvolution {
                parent_reflex_id: 0,
            },
            created_at_ms: 1000,
        };
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ReflexVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn version_status_serde_roundtrip() {
        for status in [
            VersionStatus::Active,
            VersionStatus::Incubating,
            VersionStatus::Deprecated,
            VersionStatus::Disabled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let decoded: VersionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, status);
        }
    }

    #[test]
    fn creation_reason_serde_roundtrip() {
        let reasons = vec![
            CreationReason::Original,
            CreationReason::DriftEvolution {
                parent_reflex_id: 1,
            },
            CreationReason::OperatorEdit,
            CreationReason::Merge {
                source_ids: vec![1, 2],
            },
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let decoded: CreationReason = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, reason);
        }
    }

    #[test]
    fn evolution_result_serde_roundtrip() {
        let results = vec![
            EvolutionResult::Evolved {
                new_reflex_id: 2,
                new_version: 2,
                deprecated_reflex_id: 1,
            },
            EvolutionResult::ParentNotFound { reflex_id: 1 },
            EvolutionResult::AlreadyDeprecated { reflex_id: 1 },
            EvolutionResult::EmptyCommands,
            EvolutionResult::LineageTooDeep {
                depth: 5,
                max_depth: 3,
            },
        ];
        for result in results {
            let json = serde_json::to_string(&result).unwrap();
            let decoded: EvolutionResult = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, result);
        }
    }

    #[test]
    fn evolution_stats_serde_roundtrip() {
        let stats = EvolutionStats {
            total_reflexes: 10,
            total_evolutions: 5,
            total_deprecations: 3,
            by_status: HashMap::from([("Active".to_string(), 5), ("Deprecated".to_string(), 3)]),
            max_lineage_depth: 2,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: EvolutionStats = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, stats);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = EvolutionConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let decoded: EvolutionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.max_lineage_depth, config.max_lineage_depth);
    }
}
