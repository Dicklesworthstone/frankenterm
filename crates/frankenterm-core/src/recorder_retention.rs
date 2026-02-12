//! Recorder data retention, partitioning, and archival lifecycle.
//!
//! Bead: wa-oegrb.3.5
//!
//! Implements the retention policy defined in `recorder-governance-policy.md`:
//! - Sensitivity tiers (T1/T2/T3) for data classification
//! - Retention classes (hot/warm/cold/purged) with configurable windows
//! - Segment lifecycle transitions with invariant enforcement
//! - Partition strategy (time, size, sensitivity boundaries)
//! - Purge operations with audit trail and consumer checkpoint safety

use serde::{Deserialize, Serialize};

use crate::recorder_storage::RecorderOffset;
use crate::recording::RecorderRedactionLevel;

// =============================================================================
// Configuration
// =============================================================================

/// Retention policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// Hours before active segments transition to sealed (warm).
    pub hot_hours: u32,
    /// Days before sealed segments transition to archived (cold).
    pub warm_days: u32,
    /// Days before archived segments are purged.
    pub cold_days: u32,
    /// Maximum hours for T3 (restricted/unredacted) data before mandatory purge.
    pub t3_max_hours: u32,
    /// Extended retention days for T1 (metadata) data. Max 90.
    pub t1_extended_days: u32,
    /// Maximum segment size in bytes before rolling to a new segment.
    pub max_segment_bytes: u64,
    /// Maximum segment duration in seconds before rolling.
    pub max_segment_duration_secs: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            hot_hours: 24,
            warm_days: 7,
            cold_days: 30,
            t3_max_hours: 24,
            t1_extended_days: 30,
            max_segment_bytes: 256 * 1024 * 1024, // 256 MB
            max_segment_duration_secs: 3600,       // 1 hour
        }
    }
}

impl RetentionConfig {
    /// Validate configuration constraints.
    pub fn validate(&self) -> Result<(), RetentionError> {
        if self.hot_hours == 0 {
            return Err(RetentionError::InvalidConfig("hot_hours must be > 0".into()));
        }
        if self.warm_days == 0 {
            return Err(RetentionError::InvalidConfig("warm_days must be > 0".into()));
        }
        if self.cold_days == 0 {
            return Err(RetentionError::InvalidConfig("cold_days must be > 0".into()));
        }
        if self.t1_extended_days > 90 {
            return Err(RetentionError::InvalidConfig(
                "t1_extended_days must be <= 90".into(),
            ));
        }
        if self.max_segment_bytes == 0 {
            return Err(RetentionError::InvalidConfig(
                "max_segment_bytes must be > 0".into(),
            ));
        }
        if self.max_segment_duration_secs == 0 {
            return Err(RetentionError::InvalidConfig(
                "max_segment_duration_secs must be > 0".into(),
            ));
        }
        Ok(())
    }

    /// Effective retention window in hours for a given sensitivity tier.
    #[must_use]
    pub fn retention_hours(&self, tier: SensitivityTier) -> u64 {
        match tier {
            SensitivityTier::T1Standard => {
                (self.t1_extended_days as u64) * 24
            }
            SensitivityTier::T2Sensitive => {
                (self.hot_hours as u64) + (self.warm_days as u64) * 24 + (self.cold_days as u64) * 24
            }
            SensitivityTier::T3Restricted => {
                self.t3_max_hours as u64
            }
        }
    }
}

// =============================================================================
// Errors
// =============================================================================

/// Retention operation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionError {
    /// Configuration validation failure.
    InvalidConfig(String),
    /// Segment cannot transition to the requested phase.
    InvalidTransition {
        segment_id: String,
        from: SegmentPhase,
        to: SegmentPhase,
    },
    /// Segment is referenced by a consumer checkpoint and cannot be purged.
    CheckpointHold {
        segment_id: String,
        consumer: String,
    },
    /// Segment not found.
    NotFound(String),
}

impl std::fmt::Display for RetentionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(msg) => write!(f, "invalid retention config: {}", msg),
            Self::InvalidTransition { segment_id, from, to } => {
                write!(f, "invalid transition for {}: {:?} -> {:?}", segment_id, from, to)
            }
            Self::CheckpointHold { segment_id, consumer } => {
                write!(f, "segment {} held by consumer checkpoint: {}", segment_id, consumer)
            }
            Self::NotFound(id) => write!(f, "segment not found: {}", id),
        }
    }
}

impl std::error::Error for RetentionError {}

// =============================================================================
// Sensitivity tiers
// =============================================================================

/// Data sensitivity classification per governance policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitivityTier {
    /// T1: Non-sensitive operational data (lifecycle markers, gaps, metadata).
    T1Standard = 1,
    /// T2: Contains or may contain PII/secrets after redaction.
    T2Sensitive = 2,
    /// T3: Contains known secrets or unredacted capture.
    T3Restricted = 3,
}

impl SensitivityTier {
    /// Classify an event's sensitivity from its redaction level and capture mode.
    #[must_use]
    pub fn classify(redaction: RecorderRedactionLevel, unredacted_capture: bool) -> Self {
        if unredacted_capture {
            SensitivityTier::T3Restricted
        } else {
            match redaction {
                RecorderRedactionLevel::None => SensitivityTier::T1Standard,
                RecorderRedactionLevel::Partial | RecorderRedactionLevel::Full => {
                    SensitivityTier::T2Sensitive
                }
            }
        }
    }

    /// Whether this tier's data requires accelerated purge.
    #[must_use]
    pub fn requires_accelerated_purge(&self) -> bool {
        matches!(self, SensitivityTier::T3Restricted)
    }
}

// =============================================================================
// Segment lifecycle
// =============================================================================

/// Segment lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentPhase {
    /// Actively being written to.
    Active = 0,
    /// No longer written to, still immediately queryable.
    Sealed = 1,
    /// Closed, optionally compressed, queryable with latency penalty.
    Archived = 2,
    /// Deleted.
    Purged = 3,
}

impl SegmentPhase {
    /// Whether this phase allows appending new events.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        matches!(self, SegmentPhase::Active)
    }

    /// Whether data in this phase is queryable.
    #[must_use]
    pub fn is_queryable(&self) -> bool {
        matches!(self, SegmentPhase::Active | SegmentPhase::Sealed | SegmentPhase::Archived)
    }

    /// Valid next phase.
    #[must_use]
    pub fn valid_transitions(&self) -> &[SegmentPhase] {
        match self {
            SegmentPhase::Active => &[SegmentPhase::Sealed],
            SegmentPhase::Sealed => &[SegmentPhase::Archived],
            SegmentPhase::Archived => &[SegmentPhase::Purged],
            SegmentPhase::Purged => &[],
        }
    }

    /// Check if a transition is valid.
    #[must_use]
    pub fn can_transition_to(&self, target: SegmentPhase) -> bool {
        self.valid_transitions().contains(&target)
    }
}

// =============================================================================
// Segment metadata
// =============================================================================

/// Metadata for a recorder log segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMeta {
    /// Unique segment identifier.
    pub segment_id: String,
    /// Sensitivity tier (determines retention rules).
    pub sensitivity: SensitivityTier,
    /// Current lifecycle phase.
    pub phase: SegmentPhase,
    /// First ordinal in this segment.
    pub start_ordinal: u64,
    /// Last ordinal in this segment (None if active and empty).
    pub end_ordinal: Option<u64>,
    /// Byte size of the segment data file.
    pub size_bytes: u64,
    /// Timestamp when segment was created (ms since epoch).
    pub created_at_ms: u64,
    /// Timestamp when segment was sealed (ms since epoch).
    pub sealed_at_ms: Option<u64>,
    /// Timestamp when segment was archived (ms since epoch).
    pub archived_at_ms: Option<u64>,
    /// Timestamp when segment was purged (ms since epoch).
    pub purged_at_ms: Option<u64>,
    /// Number of events in this segment.
    pub event_count: u64,
}

impl SegmentMeta {
    /// Generate the segment ID from its attributes.
    #[must_use]
    pub fn make_id(start_ordinal: u64, sensitivity: SensitivityTier, created_at_ms: u64) -> String {
        let tier_label = match sensitivity {
            SensitivityTier::T1Standard => "t1",
            SensitivityTier::T2Sensitive => "t2",
            SensitivityTier::T3Restricted => "t3",
        };
        format!("{}_{}_{}",start_ordinal, tier_label, created_at_ms)
    }

    /// Whether this segment needs to roll (create a new segment).
    #[must_use]
    pub fn should_roll(&self, config: &RetentionConfig, now_ms: u64) -> bool {
        if self.phase != SegmentPhase::Active {
            return false;
        }
        // Size boundary
        if self.size_bytes >= config.max_segment_bytes {
            return true;
        }
        // Time boundary
        let age_secs = now_ms.saturating_sub(self.created_at_ms) / 1000;
        if age_secs >= config.max_segment_duration_secs {
            return true;
        }
        false
    }

    /// Determine if this segment is eligible for the next lifecycle transition.
    #[must_use]
    pub fn eligible_transition(&self, config: &RetentionConfig, now_ms: u64) -> Option<SegmentPhase> {
        let age_hours = now_ms.saturating_sub(self.created_at_ms) / (1000 * 3600);

        // T3 accelerated purge: skip directly to Purged if past max hours
        if self.sensitivity == SensitivityTier::T3Restricted
            && age_hours >= config.t3_max_hours as u64
            && self.phase != SegmentPhase::Purged
        {
            // T3 can transition to next phase (or directly purge if already archived)
            return match self.phase {
                SegmentPhase::Active => Some(SegmentPhase::Sealed),
                SegmentPhase::Sealed => Some(SegmentPhase::Archived),
                SegmentPhase::Archived => Some(SegmentPhase::Purged),
                SegmentPhase::Purged => None,
            };
        }

        match self.phase {
            SegmentPhase::Active => {
                if age_hours >= config.hot_hours as u64 {
                    Some(SegmentPhase::Sealed)
                } else {
                    None
                }
            }
            SegmentPhase::Sealed => {
                let sealed_age_days = self.sealed_at_ms
                    .map(|ts| now_ms.saturating_sub(ts) / (1000 * 3600 * 24))
                    .unwrap_or(0);
                if sealed_age_days >= config.warm_days as u64 {
                    Some(SegmentPhase::Archived)
                } else {
                    None
                }
            }
            SegmentPhase::Archived => {
                let archived_age_days = self.archived_at_ms
                    .map(|ts| now_ms.saturating_sub(ts) / (1000 * 3600 * 24))
                    .unwrap_or(0);
                let cold_limit = if self.sensitivity == SensitivityTier::T1Standard {
                    config.t1_extended_days as u64
                } else {
                    config.cold_days as u64
                };
                if archived_age_days >= cold_limit {
                    Some(SegmentPhase::Purged)
                } else {
                    None
                }
            }
            SegmentPhase::Purged => None,
        }
    }

    /// Apply a phase transition, updating timestamps.
    pub fn transition(&mut self, target: SegmentPhase, now_ms: u64) -> Result<(), RetentionError> {
        if !self.phase.can_transition_to(target) {
            return Err(RetentionError::InvalidTransition {
                segment_id: self.segment_id.clone(),
                from: self.phase,
                to: target,
            });
        }
        match target {
            SegmentPhase::Sealed => self.sealed_at_ms = Some(now_ms),
            SegmentPhase::Archived => self.archived_at_ms = Some(now_ms),
            SegmentPhase::Purged => self.purged_at_ms = Some(now_ms),
            SegmentPhase::Active => {} // unreachable due to valid_transitions
        }
        self.phase = target;
        Ok(())
    }
}

// =============================================================================
// Retention manager
// =============================================================================

/// Manages segment lifecycle transitions and purge operations.
#[derive(Debug)]
pub struct RetentionManager {
    config: RetentionConfig,
    segments: Vec<SegmentMeta>,
}

/// Result of a retention sweep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetentionSweepResult {
    /// Segments transitioned to sealed.
    pub sealed: Vec<String>,
    /// Segments transitioned to archived.
    pub archived: Vec<String>,
    /// Segments eligible for purge (pending checkpoint check).
    pub purge_candidates: Vec<String>,
    /// Segments actually purged.
    pub purged: Vec<String>,
    /// Segments blocked from purge by consumer checkpoints.
    pub held: Vec<(String, String)>,
}

impl RetentionManager {
    /// Create a new retention manager.
    pub fn new(config: RetentionConfig) -> Result<Self, RetentionError> {
        config.validate()?;
        Ok(Self {
            config,
            segments: Vec::new(),
        })
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self {
            config: RetentionConfig::default(),
            segments: Vec::new(),
        }
    }

    /// Register a new segment.
    pub fn add_segment(&mut self, meta: SegmentMeta) {
        self.segments.push(meta);
    }

    /// Number of tracked segments.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Get a segment by ID.
    #[must_use]
    pub fn get_segment(&self, segment_id: &str) -> Option<&SegmentMeta> {
        self.segments.iter().find(|s| s.segment_id == segment_id)
    }

    /// Get a mutable reference to a segment by ID.
    pub fn get_segment_mut(&mut self, segment_id: &str) -> Option<&mut SegmentMeta> {
        self.segments.iter_mut().find(|s| s.segment_id == segment_id)
    }

    /// List segments in a given phase.
    #[must_use]
    pub fn segments_in_phase(&self, phase: SegmentPhase) -> Vec<&SegmentMeta> {
        self.segments.iter().filter(|s| s.phase == phase).collect()
    }

    /// List segments by sensitivity tier.
    #[must_use]
    pub fn segments_by_tier(&self, tier: SensitivityTier) -> Vec<&SegmentMeta> {
        self.segments.iter().filter(|s| s.sensitivity == tier).collect()
    }

    /// Determine which segments need to roll (active segment exceeded size/time).
    #[must_use]
    pub fn segments_needing_roll(&self, now_ms: u64) -> Vec<&SegmentMeta> {
        self.segments
            .iter()
            .filter(|s| s.should_roll(&self.config, now_ms))
            .collect()
    }

    /// Run a retention sweep: compute eligible transitions and purge candidates.
    ///
    /// `checkpoint_holders` maps segment IDs to consumer names that hold
    /// checkpoint references into that segment. Segments with active holders
    /// cannot be purged.
    pub fn sweep(
        &mut self,
        now_ms: u64,
        checkpoint_holders: &std::collections::HashMap<String, Vec<String>>,
    ) -> RetentionSweepResult {
        let mut result = RetentionSweepResult::default();

        // Collect transitions first to avoid borrow issues
        let transitions: Vec<(usize, SegmentPhase)> = self.segments
            .iter()
            .enumerate()
            .filter_map(|(i, seg)| {
                seg.eligible_transition(&self.config, now_ms)
                    .map(|target| (i, target))
            })
            .collect();

        for (idx, target) in transitions {
            let seg = &mut self.segments[idx];
            let seg_id = seg.segment_id.clone();

            if target == SegmentPhase::Purged {
                // Check checkpoint holds
                if let Some(holders) = checkpoint_holders.get(&seg_id) {
                    if !holders.is_empty() {
                        for h in holders {
                            result.held.push((seg_id.clone(), h.clone()));
                        }
                        result.purge_candidates.push(seg_id);
                        continue;
                    }
                }
                // Safe to purge
                if seg.transition(target, now_ms).is_ok() {
                    result.purged.push(seg_id);
                }
            } else {
                if seg.transition(target, now_ms).is_ok() {
                    match target {
                        SegmentPhase::Sealed => result.sealed.push(seg_id),
                        SegmentPhase::Archived => result.archived.push(seg_id),
                        _ => {}
                    }
                }
            }
        }

        result
    }

    /// Total data size across all non-purged segments.
    #[must_use]
    pub fn total_data_bytes(&self) -> u64 {
        self.segments
            .iter()
            .filter(|s| s.phase != SegmentPhase::Purged)
            .map(|s| s.size_bytes)
            .sum()
    }

    /// Total events across all non-purged segments.
    #[must_use]
    pub fn total_events(&self) -> u64 {
        self.segments
            .iter()
            .filter(|s| s.phase != SegmentPhase::Purged)
            .map(|s| s.event_count)
            .sum()
    }

    /// Statistics breakdown by phase.
    #[must_use]
    pub fn stats(&self) -> RetentionStats {
        let mut stats = RetentionStats::default();
        for seg in &self.segments {
            match seg.phase {
                SegmentPhase::Active => {
                    stats.active_count += 1;
                    stats.active_bytes += seg.size_bytes;
                }
                SegmentPhase::Sealed => {
                    stats.sealed_count += 1;
                    stats.sealed_bytes += seg.size_bytes;
                }
                SegmentPhase::Archived => {
                    stats.archived_count += 1;
                    stats.archived_bytes += seg.size_bytes;
                }
                SegmentPhase::Purged => {
                    stats.purged_count += 1;
                }
            }
        }
        stats
    }

    /// Configuration reference.
    #[must_use]
    pub fn config(&self) -> &RetentionConfig {
        &self.config
    }
}

/// Retention statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetentionStats {
    pub active_count: usize,
    pub active_bytes: u64,
    pub sealed_count: usize,
    pub sealed_bytes: u64,
    pub archived_count: usize,
    pub archived_bytes: u64,
    pub purged_count: usize,
}

impl RetentionStats {
    /// Total non-purged segments.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.active_count + self.sealed_count + self.archived_count
    }

    /// Total non-purged bytes.
    #[must_use]
    pub fn live_bytes(&self) -> u64 {
        self.active_bytes + self.sealed_bytes + self.archived_bytes
    }
}

// =============================================================================
// Audit event types
// =============================================================================

/// Audit event emitted during retention lifecycle operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionAuditEvent {
    /// Audit schema version.
    pub audit_version: String,
    /// Type of retention operation.
    pub event_type: RetentionAuditType,
    /// Segment affected.
    pub segment_id: String,
    /// Ordinal range of the segment.
    pub ordinal_range: Option<(u64, u64)>,
    /// Sensitivity tier of the segment.
    pub sensitivity: SensitivityTier,
    /// Phase before the operation.
    pub from_phase: Option<SegmentPhase>,
    /// Phase after the operation.
    pub to_phase: SegmentPhase,
    /// Timestamp (ms since epoch).
    pub timestamp_ms: u64,
    /// Optional justification for manual operations.
    pub justification: Option<String>,
}

/// Types of retention audit events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionAuditType {
    /// Segment sealed (hot → warm).
    SegmentSealed,
    /// Segment archived (warm → cold).
    SegmentArchived,
    /// Segment purged (cold → deleted).
    SegmentPurged,
    /// T3 accelerated purge.
    AcceleratedPurge,
    /// Manual purge by admin.
    ManualPurge,
    /// Retention policy override.
    PolicyOverride,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn ms(hours: u64) -> u64 {
        hours * 3600 * 1000
    }

    fn ms_days(days: u64) -> u64 {
        days * 24 * 3600 * 1000
    }

    fn make_segment(
        id: &str,
        tier: SensitivityTier,
        phase: SegmentPhase,
        created_at_ms: u64,
    ) -> SegmentMeta {
        SegmentMeta {
            segment_id: id.to_string(),
            sensitivity: tier,
            phase,
            start_ordinal: 0,
            end_ordinal: Some(100),
            size_bytes: 1024,
            event_count: 100,
            created_at_ms,
            sealed_at_ms: if phase >= SegmentPhase::Sealed { Some(created_at_ms + ms(24)) } else { None },
            archived_at_ms: if phase >= SegmentPhase::Archived { Some(created_at_ms + ms(24) + ms_days(7)) } else { None },
            purged_at_ms: None,
        }
    }

    // -----------------------------------------------------------------------
    // RetentionConfig
    // -----------------------------------------------------------------------

    #[test]
    fn config_default_valid() {
        let cfg = RetentionConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn config_rejects_zero_hot_hours() {
        let mut cfg = RetentionConfig::default();
        cfg.hot_hours = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_rejects_t1_extended_over_90() {
        let mut cfg = RetentionConfig::default();
        cfg.t1_extended_days = 91;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_retention_hours_by_tier() {
        let cfg = RetentionConfig {
            hot_hours: 24,
            warm_days: 7,
            cold_days: 30,
            t3_max_hours: 24,
            t1_extended_days: 60,
            ..Default::default()
        };
        assert_eq!(cfg.retention_hours(SensitivityTier::T1Standard), 60 * 24);
        assert_eq!(
            cfg.retention_hours(SensitivityTier::T2Sensitive),
            24 + 7 * 24 + 30 * 24
        );
        assert_eq!(cfg.retention_hours(SensitivityTier::T3Restricted), 24);
    }

    #[test]
    fn config_serialization_roundtrip() {
        let cfg = RetentionConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: RetentionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.hot_hours, cfg.hot_hours);
        assert_eq!(parsed.cold_days, cfg.cold_days);
    }

    // -----------------------------------------------------------------------
    // SensitivityTier
    // -----------------------------------------------------------------------

    #[test]
    fn tier_classify_none_is_t1() {
        assert_eq!(
            SensitivityTier::classify(RecorderRedactionLevel::None, false),
            SensitivityTier::T1Standard
        );
    }

    #[test]
    fn tier_classify_partial_is_t2() {
        assert_eq!(
            SensitivityTier::classify(RecorderRedactionLevel::Partial, false),
            SensitivityTier::T2Sensitive
        );
    }

    #[test]
    fn tier_classify_full_is_t2() {
        assert_eq!(
            SensitivityTier::classify(RecorderRedactionLevel::Full, false),
            SensitivityTier::T2Sensitive
        );
    }

    #[test]
    fn tier_classify_unredacted_is_t3() {
        assert_eq!(
            SensitivityTier::classify(RecorderRedactionLevel::None, true),
            SensitivityTier::T3Restricted
        );
    }

    #[test]
    fn tier_ordering() {
        assert!(SensitivityTier::T1Standard < SensitivityTier::T2Sensitive);
        assert!(SensitivityTier::T2Sensitive < SensitivityTier::T3Restricted);
    }

    #[test]
    fn tier_t3_requires_accelerated_purge() {
        assert!(SensitivityTier::T3Restricted.requires_accelerated_purge());
        assert!(!SensitivityTier::T2Sensitive.requires_accelerated_purge());
        assert!(!SensitivityTier::T1Standard.requires_accelerated_purge());
    }

    // -----------------------------------------------------------------------
    // SegmentPhase
    // -----------------------------------------------------------------------

    #[test]
    fn phase_active_is_writable() {
        assert!(SegmentPhase::Active.is_writable());
        assert!(!SegmentPhase::Sealed.is_writable());
    }

    #[test]
    fn phase_queryable() {
        assert!(SegmentPhase::Active.is_queryable());
        assert!(SegmentPhase::Sealed.is_queryable());
        assert!(SegmentPhase::Archived.is_queryable());
        assert!(!SegmentPhase::Purged.is_queryable());
    }

    #[test]
    fn phase_valid_transitions() {
        assert!(SegmentPhase::Active.can_transition_to(SegmentPhase::Sealed));
        assert!(!SegmentPhase::Active.can_transition_to(SegmentPhase::Archived));
        assert!(!SegmentPhase::Active.can_transition_to(SegmentPhase::Purged));
        assert!(SegmentPhase::Sealed.can_transition_to(SegmentPhase::Archived));
        assert!(SegmentPhase::Archived.can_transition_to(SegmentPhase::Purged));
        assert!(SegmentPhase::Purged.valid_transitions().is_empty());
    }

    #[test]
    fn phase_ordering() {
        assert!(SegmentPhase::Active < SegmentPhase::Sealed);
        assert!(SegmentPhase::Sealed < SegmentPhase::Archived);
        assert!(SegmentPhase::Archived < SegmentPhase::Purged);
    }

    // -----------------------------------------------------------------------
    // SegmentMeta
    // -----------------------------------------------------------------------

    #[test]
    fn segment_make_id() {
        let id = SegmentMeta::make_id(42, SensitivityTier::T2Sensitive, 1000);
        assert_eq!(id, "42_t2_1000");
    }

    #[test]
    fn segment_should_roll_by_size() {
        let cfg = RetentionConfig {
            max_segment_bytes: 100,
            ..Default::default()
        };
        let seg = SegmentMeta {
            segment_id: "test".into(),
            sensitivity: SensitivityTier::T1Standard,
            phase: SegmentPhase::Active,
            start_ordinal: 0,
            end_ordinal: None,
            size_bytes: 100,
            event_count: 10,
            created_at_ms: 0,
            sealed_at_ms: None,
            archived_at_ms: None,
            purged_at_ms: None,
        };
        assert!(seg.should_roll(&cfg, 1000));
    }

    #[test]
    fn segment_should_roll_by_time() {
        let cfg = RetentionConfig {
            max_segment_duration_secs: 60,
            ..Default::default()
        };
        let seg = SegmentMeta {
            segment_id: "test".into(),
            sensitivity: SensitivityTier::T1Standard,
            phase: SegmentPhase::Active,
            start_ordinal: 0,
            end_ordinal: None,
            size_bytes: 10,
            event_count: 1,
            created_at_ms: 0,
            sealed_at_ms: None,
            archived_at_ms: None,
            purged_at_ms: None,
        };
        assert!(seg.should_roll(&cfg, 61_000)); // 61 seconds
    }

    #[test]
    fn segment_no_roll_if_sealed() {
        let cfg = RetentionConfig::default();
        let seg = SegmentMeta {
            segment_id: "test".into(),
            sensitivity: SensitivityTier::T1Standard,
            phase: SegmentPhase::Sealed,
            start_ordinal: 0,
            end_ordinal: Some(100),
            size_bytes: 999_999_999,
            event_count: 100,
            created_at_ms: 0,
            sealed_at_ms: Some(1000),
            archived_at_ms: None,
            purged_at_ms: None,
        };
        assert!(!seg.should_roll(&cfg, ms(1000)));
    }

    #[test]
    fn segment_transition_valid() {
        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        assert!(seg.transition(SegmentPhase::Sealed, 1000).is_ok());
        assert_eq!(seg.phase, SegmentPhase::Sealed);
        assert_eq!(seg.sealed_at_ms, Some(1000));
    }

    #[test]
    fn segment_transition_invalid() {
        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        let result = seg.transition(SegmentPhase::Purged, 1000);
        assert!(result.is_err());
    }

    #[test]
    fn segment_eligible_hot_to_sealed() {
        let cfg = RetentionConfig { hot_hours: 24, ..Default::default() };
        let seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        assert_eq!(seg.eligible_transition(&cfg, ms(23)), None);
        assert_eq!(seg.eligible_transition(&cfg, ms(24)), Some(SegmentPhase::Sealed));
    }

    #[test]
    fn segment_eligible_sealed_to_archived() {
        let cfg = RetentionConfig { warm_days: 7, ..Default::default() };
        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        seg.transition(SegmentPhase::Sealed, ms(24)).unwrap();

        // 6 days after sealing — not eligible
        assert_eq!(seg.eligible_transition(&cfg, ms(24) + ms_days(6)), None);
        // 7 days after sealing — eligible
        assert_eq!(
            seg.eligible_transition(&cfg, ms(24) + ms_days(7)),
            Some(SegmentPhase::Archived)
        );
    }

    #[test]
    fn segment_eligible_archived_to_purged() {
        let cfg = RetentionConfig { cold_days: 30, ..Default::default() };
        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        seg.transition(SegmentPhase::Sealed, ms(24)).unwrap();
        seg.transition(SegmentPhase::Archived, ms(24) + ms_days(7)).unwrap();

        let archive_time = ms(24) + ms_days(7);
        assert_eq!(seg.eligible_transition(&cfg, archive_time + ms_days(29)), None);
        assert_eq!(
            seg.eligible_transition(&cfg, archive_time + ms_days(30)),
            Some(SegmentPhase::Purged)
        );
    }

    #[test]
    fn segment_t3_accelerated_purge() {
        let cfg = RetentionConfig { t3_max_hours: 24, ..Default::default() };
        let seg = make_segment("s1", SensitivityTier::T3Restricted, SegmentPhase::Active, 0);
        // At 24 hours, T3 should be eligible for transition
        assert_eq!(seg.eligible_transition(&cfg, ms(24)), Some(SegmentPhase::Sealed));
    }

    #[test]
    fn segment_t1_extended_retention() {
        let cfg = RetentionConfig {
            cold_days: 30,
            t1_extended_days: 60,
            ..Default::default()
        };
        let mut seg = make_segment("s1", SensitivityTier::T1Standard, SegmentPhase::Active, 0);
        seg.transition(SegmentPhase::Sealed, ms(24)).unwrap();
        seg.transition(SegmentPhase::Archived, ms(24) + ms_days(7)).unwrap();

        let archive_time = ms(24) + ms_days(7);
        // T1 should use extended retention (60 days)
        assert_eq!(seg.eligible_transition(&cfg, archive_time + ms_days(30)), None);
        assert_eq!(seg.eligible_transition(&cfg, archive_time + ms_days(59)), None);
        assert_eq!(
            seg.eligible_transition(&cfg, archive_time + ms_days(60)),
            Some(SegmentPhase::Purged)
        );
    }

    // -----------------------------------------------------------------------
    // RetentionManager
    // -----------------------------------------------------------------------

    #[test]
    fn manager_add_and_count_segments() {
        let mut mgr = RetentionManager::with_defaults();
        mgr.add_segment(make_segment("s1", SensitivityTier::T1Standard, SegmentPhase::Active, 0));
        mgr.add_segment(make_segment("s2", SensitivityTier::T2Sensitive, SegmentPhase::Sealed, 0));
        assert_eq!(mgr.segment_count(), 2);
    }

    #[test]
    fn manager_get_segment() {
        let mut mgr = RetentionManager::with_defaults();
        mgr.add_segment(make_segment("s1", SensitivityTier::T1Standard, SegmentPhase::Active, 0));
        assert!(mgr.get_segment("s1").is_some());
        assert!(mgr.get_segment("s99").is_none());
    }

    #[test]
    fn manager_segments_in_phase() {
        let mut mgr = RetentionManager::with_defaults();
        mgr.add_segment(make_segment("s1", SensitivityTier::T1Standard, SegmentPhase::Active, 0));
        mgr.add_segment(make_segment("s2", SensitivityTier::T1Standard, SegmentPhase::Sealed, 0));
        mgr.add_segment(make_segment("s3", SensitivityTier::T2Sensitive, SegmentPhase::Sealed, 0));

        assert_eq!(mgr.segments_in_phase(SegmentPhase::Active).len(), 1);
        assert_eq!(mgr.segments_in_phase(SegmentPhase::Sealed).len(), 2);
        assert_eq!(mgr.segments_in_phase(SegmentPhase::Archived).len(), 0);
    }

    #[test]
    fn manager_segments_by_tier() {
        let mut mgr = RetentionManager::with_defaults();
        mgr.add_segment(make_segment("s1", SensitivityTier::T1Standard, SegmentPhase::Active, 0));
        mgr.add_segment(make_segment("s2", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0));
        mgr.add_segment(make_segment("s3", SensitivityTier::T3Restricted, SegmentPhase::Active, 0));

        assert_eq!(mgr.segments_by_tier(SensitivityTier::T1Standard).len(), 1);
        assert_eq!(mgr.segments_by_tier(SensitivityTier::T3Restricted).len(), 1);
    }

    #[test]
    fn manager_sweep_seals_old_active() {
        let mut mgr = RetentionManager::new(RetentionConfig {
            hot_hours: 24,
            ..Default::default()
        }).unwrap();

        mgr.add_segment(make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0));
        let result = mgr.sweep(ms(25), &HashMap::new());
        assert_eq!(result.sealed, vec!["s1".to_string()]);
        assert_eq!(mgr.get_segment("s1").unwrap().phase, SegmentPhase::Sealed);
    }

    #[test]
    fn manager_sweep_archives_old_sealed() {
        let mut mgr = RetentionManager::new(RetentionConfig {
            hot_hours: 24,
            warm_days: 7,
            ..Default::default()
        }).unwrap();

        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        seg.transition(SegmentPhase::Sealed, ms(24)).unwrap();
        mgr.add_segment(seg);

        let now = ms(24) + ms_days(8);
        let result = mgr.sweep(now, &HashMap::new());
        assert_eq!(result.archived, vec!["s1".to_string()]);
    }

    #[test]
    fn manager_sweep_purges_old_archived() {
        let mut mgr = RetentionManager::new(RetentionConfig {
            cold_days: 30,
            ..Default::default()
        }).unwrap();

        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        seg.transition(SegmentPhase::Sealed, ms(24)).unwrap();
        seg.transition(SegmentPhase::Archived, ms(24) + ms_days(7)).unwrap();
        mgr.add_segment(seg);

        let now = ms(24) + ms_days(7) + ms_days(31);
        let result = mgr.sweep(now, &HashMap::new());
        assert_eq!(result.purged, vec!["s1".to_string()]);
    }

    #[test]
    fn manager_sweep_blocks_purge_on_checkpoint() {
        let mut mgr = RetentionManager::new(RetentionConfig {
            cold_days: 1,
            ..Default::default()
        }).unwrap();

        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        seg.transition(SegmentPhase::Sealed, ms(24)).unwrap();
        seg.transition(SegmentPhase::Archived, ms(24) + ms_days(1)).unwrap();
        mgr.add_segment(seg);

        let mut holders = HashMap::new();
        holders.insert("s1".to_string(), vec!["indexer".to_string()]);

        let now = ms(24) + ms_days(1) + ms_days(2);
        let result = mgr.sweep(now, &holders);
        assert!(result.purged.is_empty());
        assert_eq!(result.held.len(), 1);
        assert_eq!(result.held[0], ("s1".to_string(), "indexer".to_string()));
    }

    #[test]
    fn manager_stats() {
        let mut mgr = RetentionManager::with_defaults();
        let mut s1 = make_segment("s1", SensitivityTier::T1Standard, SegmentPhase::Active, 0);
        s1.size_bytes = 500;
        let mut s2 = make_segment("s2", SensitivityTier::T2Sensitive, SegmentPhase::Sealed, 0);
        s2.size_bytes = 300;
        mgr.add_segment(s1);
        mgr.add_segment(s2);

        let stats = mgr.stats();
        assert_eq!(stats.active_count, 1);
        assert_eq!(stats.active_bytes, 500);
        assert_eq!(stats.sealed_count, 1);
        assert_eq!(stats.sealed_bytes, 300);
        assert_eq!(stats.live_count(), 2);
        assert_eq!(stats.live_bytes(), 800);
    }

    #[test]
    fn manager_total_data_bytes() {
        let mut mgr = RetentionManager::with_defaults();
        let mut s1 = make_segment("s1", SensitivityTier::T1Standard, SegmentPhase::Active, 0);
        s1.size_bytes = 1000;
        mgr.add_segment(s1);
        assert_eq!(mgr.total_data_bytes(), 1000);
    }

    // -----------------------------------------------------------------------
    // RetentionAuditEvent
    // -----------------------------------------------------------------------

    #[test]
    fn audit_event_serializes() {
        let event = RetentionAuditEvent {
            audit_version: "ft.recorder.audit.v1".to_string(),
            event_type: RetentionAuditType::SegmentSealed,
            segment_id: "0_t2_1000".to_string(),
            ordinal_range: Some((0, 100)),
            sensitivity: SensitivityTier::T2Sensitive,
            from_phase: Some(SegmentPhase::Active),
            to_phase: SegmentPhase::Sealed,
            timestamp_ms: 5000,
            justification: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("segment_sealed"));
        assert!(json.contains("t2_sensitive"));
    }

    // -----------------------------------------------------------------------
    // ErrorCounts
    // -----------------------------------------------------------------------

    #[test]
    fn error_display() {
        let err = RetentionError::InvalidConfig("bad value".into());
        assert!(err.to_string().contains("bad value"));

        let err = RetentionError::CheckpointHold {
            segment_id: "s1".into(),
            consumer: "indexer".into(),
        };
        assert!(err.to_string().contains("indexer"));
    }

    #[test]
    fn retention_stats_default_zero() {
        let stats = RetentionStats::default();
        assert_eq!(stats.live_count(), 0);
        assert_eq!(stats.live_bytes(), 0);
    }
}
