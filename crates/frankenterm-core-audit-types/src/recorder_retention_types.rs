//! Recorder retention policy DTOs (ft-xcsm0 / ft-8nqx0 Phase 4).
//!
//! Lifted out of `frankenterm-core::recorder_retention` so retention
//! contracts can be reviewed independently from the operational
//! `RetentionManager` (which still owns segment-list state, sweep
//! orchestration, and the SQLite deletion paths in core).
//!
//! Types here:
//! - [`RetentionConfig`] / [`RetentionError`] — policy config + validation.
//! - [`SensitivityTier`] / [`SegmentPhase`] — classification + lifecycle.
//! - [`SegmentMeta`] — per-segment metadata + transition machinery.
//! - [`RetentionStats`] — aggregated retention counts/bytes.
//! - [`RetentionAuditEvent`] / [`RetentionAuditType`] — audit-trail DTO
//!   the manager emits on every transition.
//!
//! `RetentionSweepResult` stays in core alongside the manager since it
//! is the manager's per-call return shape; lifting it requires
//! touching the call sites that name the manager directly.
//!
//! Cross-crate dep: `SensitivityTier::classify` consumes
//! `RecorderRedactionLevel` from `frankenterm-core-replay-types`,
//! which is also a leaf-tier types crate, so the dep is leaf→leaf.

use serde::{Deserialize, Serialize};

use frankenterm_core_replay_types::recorder_metadata::RecorderRedactionLevel;

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
    /// Archived-phase retention days for T1 (metadata) data after the hot and
    /// warm lifecycle windows complete. Max 90.
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
            max_segment_duration_secs: 3600,      // 1 hour
        }
    }
}

impl RetentionConfig {
    /// Validate configuration constraints.
    pub fn validate(&self) -> Result<(), RetentionError> {
        if self.hot_hours == 0 {
            return Err(RetentionError::InvalidConfig(
                "hot_hours must be > 0".into(),
            ));
        }
        if self.warm_days == 0 {
            return Err(RetentionError::InvalidConfig(
                "warm_days must be > 0".into(),
            ));
        }
        if self.cold_days == 0 {
            return Err(RetentionError::InvalidConfig(
                "cold_days must be > 0".into(),
            ));
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

    /// Effective total retention window in hours for a given sensitivity tier.
    #[must_use]
    pub fn retention_hours(&self, tier: SensitivityTier) -> u64 {
        match tier {
            SensitivityTier::T1Standard => {
                (self.hot_hours as u64)
                    + (self.warm_days as u64) * 24
                    + (self.t1_extended_days as u64) * 24
            }
            SensitivityTier::T2Sensitive => {
                (self.hot_hours as u64)
                    + (self.warm_days as u64) * 24
                    + (self.cold_days as u64) * 24
            }
            SensitivityTier::T3Restricted => self.t3_max_hours as u64,
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
            Self::InvalidTransition {
                segment_id,
                from,
                to,
            } => {
                write!(
                    f,
                    "invalid transition for {}: {:?} -> {:?}",
                    segment_id, from, to
                )
            }
            Self::CheckpointHold {
                segment_id,
                consumer,
            } => {
                write!(
                    f,
                    "segment {} held by consumer checkpoint: {}",
                    segment_id, consumer
                )
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
        matches!(
            self,
            SegmentPhase::Active | SegmentPhase::Sealed | SegmentPhase::Archived
        )
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
        format!("{}_{}_{}", start_ordinal, tier_label, created_at_ms)
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
    pub fn eligible_transition(
        &self,
        config: &RetentionConfig,
        now_ms: u64,
    ) -> Option<SegmentPhase> {
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
                let sealed_age_days = self
                    .sealed_at_ms
                    .map(|ts| now_ms.saturating_sub(ts) / (1000 * 3600 * 24))
                    .unwrap_or(0);
                if sealed_age_days >= config.warm_days as u64 {
                    Some(SegmentPhase::Archived)
                } else {
                    None
                }
            }
            SegmentPhase::Archived => {
                let archived_age_days = self
                    .archived_at_ms
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
