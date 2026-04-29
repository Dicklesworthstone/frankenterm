//! Recorder retention manager (operational engine).
//!
//! Bead: wa-oegrb.3.5
//!
//! Implements the operational half of the retention policy defined in
//! `recorder-governance-policy.md`. The DTOs (`RetentionConfig`,
//! `RetentionError`, `SensitivityTier`, `SegmentPhase`, `SegmentMeta`,
//! `RetentionStats`, `RetentionAuditEvent`, `RetentionAuditType`) live
//! in the leaf crate `frankenterm-core-audit-types`
//! (ft-xcsm0 / ft-8nqx0 Phase 4) so retention contracts can be
//! reviewed independently of the live deletion pipeline. Re-exported
//! below so existing `crate::recorder_retention::*` and
//! `frankenterm_core::recorder_retention::*` paths keep resolving.

pub use frankenterm_core_audit_types::recorder_retention_types::{
    RetentionAuditEvent, RetentionAuditType, RetentionConfig, RetentionError, RetentionStats,
    SegmentMeta, SegmentPhase, SensitivityTier,
};

use serde::{Deserialize, Serialize};

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
        self.segments
            .iter_mut()
            .find(|s| s.segment_id == segment_id)
    }

    /// List segments in a given phase.
    #[must_use]
    pub fn segments_in_phase(&self, phase: SegmentPhase) -> Vec<&SegmentMeta> {
        self.segments.iter().filter(|s| s.phase == phase).collect()
    }

    /// List segments by sensitivity tier.
    #[must_use]
    pub fn segments_by_tier(&self, tier: SensitivityTier) -> Vec<&SegmentMeta> {
        self.segments
            .iter()
            .filter(|s| s.sensitivity == tier)
            .collect()
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

        for idx in 0..self.segments.len() {
            loop {
                let target = {
                    let seg = &self.segments[idx];
                    seg.eligible_transition(&self.config, now_ms)
                };
                let Some(target) = target else { break };
                let seg_id = self.segments[idx].segment_id.clone();

                if target == SegmentPhase::Purged {
                    // Check checkpoint holds.
                    if let Some(holders) = checkpoint_holders.get(&seg_id) {
                        if !holders.is_empty() {
                            for h in holders {
                                result.held.push((seg_id.clone(), h.clone()));
                            }
                            result.purge_candidates.push(seg_id);
                            break;
                        }
                    }
                }

                let transitioned = {
                    let seg = &mut self.segments[idx];
                    seg.transition(target, now_ms).is_ok()
                };
                if !transitioned {
                    break;
                }

                match target {
                    SegmentPhase::Sealed => result.sealed.push(seg_id),
                    SegmentPhase::Archived => result.archived.push(seg_id),
                    SegmentPhase::Purged => result.purged.push(seg_id),
                    SegmentPhase::Active => {}
                }

                if target == SegmentPhase::Purged {
                    break;
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
            sealed_at_ms: if phase >= SegmentPhase::Sealed {
                Some(created_at_ms + ms(24))
            } else {
                None
            },
            archived_at_ms: if phase >= SegmentPhase::Archived {
                Some(created_at_ms + ms(24) + ms_days(7))
            } else {
                None
            },
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
        assert_eq!(
            cfg.retention_hours(SensitivityTier::T1Standard),
            24 + 7 * 24 + 60 * 24
        );
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
        let cfg = RetentionConfig {
            hot_hours: 24,
            ..Default::default()
        };
        let seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        assert_eq!(seg.eligible_transition(&cfg, ms(23)), None);
        assert_eq!(
            seg.eligible_transition(&cfg, ms(24)),
            Some(SegmentPhase::Sealed)
        );
    }

    #[test]
    fn segment_eligible_sealed_to_archived() {
        let cfg = RetentionConfig {
            warm_days: 7,
            ..Default::default()
        };
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
        let cfg = RetentionConfig {
            cold_days: 30,
            ..Default::default()
        };
        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        seg.transition(SegmentPhase::Sealed, ms(24)).unwrap();
        seg.transition(SegmentPhase::Archived, ms(24) + ms_days(7))
            .unwrap();

        let archive_time = ms(24) + ms_days(7);
        assert_eq!(
            seg.eligible_transition(&cfg, archive_time + ms_days(29)),
            None
        );
        assert_eq!(
            seg.eligible_transition(&cfg, archive_time + ms_days(30)),
            Some(SegmentPhase::Purged)
        );
    }

    #[test]
    fn segment_t3_accelerated_purge() {
        let cfg = RetentionConfig {
            t3_max_hours: 24,
            ..Default::default()
        };
        let seg = make_segment("s1", SensitivityTier::T3Restricted, SegmentPhase::Active, 0);
        // At 24 hours, T3 should be eligible for transition
        assert_eq!(
            seg.eligible_transition(&cfg, ms(24)),
            Some(SegmentPhase::Sealed)
        );
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
        seg.transition(SegmentPhase::Archived, ms(24) + ms_days(7))
            .unwrap();

        let archive_time = ms(24) + ms_days(7);
        // T1 should use extended retention (60 days)
        assert_eq!(
            seg.eligible_transition(&cfg, archive_time + ms_days(30)),
            None
        );
        assert_eq!(
            seg.eligible_transition(&cfg, archive_time + ms_days(59)),
            None
        );
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
        mgr.add_segment(make_segment(
            "s1",
            SensitivityTier::T1Standard,
            SegmentPhase::Active,
            0,
        ));
        mgr.add_segment(make_segment(
            "s2",
            SensitivityTier::T2Sensitive,
            SegmentPhase::Sealed,
            0,
        ));
        assert_eq!(mgr.segment_count(), 2);
    }

    #[test]
    fn manager_get_segment() {
        let mut mgr = RetentionManager::with_defaults();
        mgr.add_segment(make_segment(
            "s1",
            SensitivityTier::T1Standard,
            SegmentPhase::Active,
            0,
        ));
        assert!(mgr.get_segment("s1").is_some());
        assert!(mgr.get_segment("s99").is_none());
    }

    #[test]
    fn manager_segments_in_phase() {
        let mut mgr = RetentionManager::with_defaults();
        mgr.add_segment(make_segment(
            "s1",
            SensitivityTier::T1Standard,
            SegmentPhase::Active,
            0,
        ));
        mgr.add_segment(make_segment(
            "s2",
            SensitivityTier::T1Standard,
            SegmentPhase::Sealed,
            0,
        ));
        mgr.add_segment(make_segment(
            "s3",
            SensitivityTier::T2Sensitive,
            SegmentPhase::Sealed,
            0,
        ));

        assert_eq!(mgr.segments_in_phase(SegmentPhase::Active).len(), 1);
        assert_eq!(mgr.segments_in_phase(SegmentPhase::Sealed).len(), 2);
        assert_eq!(mgr.segments_in_phase(SegmentPhase::Archived).len(), 0);
    }

    #[test]
    fn manager_segments_by_tier() {
        let mut mgr = RetentionManager::with_defaults();
        mgr.add_segment(make_segment(
            "s1",
            SensitivityTier::T1Standard,
            SegmentPhase::Active,
            0,
        ));
        mgr.add_segment(make_segment(
            "s2",
            SensitivityTier::T2Sensitive,
            SegmentPhase::Active,
            0,
        ));
        mgr.add_segment(make_segment(
            "s3",
            SensitivityTier::T3Restricted,
            SegmentPhase::Active,
            0,
        ));

        assert_eq!(mgr.segments_by_tier(SensitivityTier::T1Standard).len(), 1);
        assert_eq!(mgr.segments_by_tier(SensitivityTier::T3Restricted).len(), 1);
    }

    #[test]
    fn manager_sweep_seals_old_active() {
        let mut mgr = RetentionManager::new(RetentionConfig {
            hot_hours: 24,
            ..Default::default()
        })
        .unwrap();

        mgr.add_segment(make_segment(
            "s1",
            SensitivityTier::T2Sensitive,
            SegmentPhase::Active,
            0,
        ));
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
        })
        .unwrap();

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
        })
        .unwrap();

        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        seg.transition(SegmentPhase::Sealed, ms(24)).unwrap();
        seg.transition(SegmentPhase::Archived, ms(24) + ms_days(7))
            .unwrap();
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
        })
        .unwrap();

        let mut seg = make_segment("s1", SensitivityTier::T2Sensitive, SegmentPhase::Active, 0);
        seg.transition(SegmentPhase::Sealed, ms(24)).unwrap();
        seg.transition(SegmentPhase::Archived, ms(24) + ms_days(1))
            .unwrap();
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
    fn manager_sweep_t3_converges_in_single_pass() {
        let mut mgr = RetentionManager::new(RetentionConfig {
            t3_max_hours: 24,
            ..Default::default()
        })
        .unwrap();

        mgr.add_segment(make_segment(
            "t3",
            SensitivityTier::T3Restricted,
            SegmentPhase::Active,
            0,
        ));

        let result = mgr.sweep(ms(24) + ms_days(1), &HashMap::new());
        assert_eq!(result.sealed, vec!["t3".to_string()]);
        assert_eq!(result.archived, vec!["t3".to_string()]);
        assert_eq!(result.purged, vec!["t3".to_string()]);
        assert_eq!(mgr.get_segment("t3").unwrap().phase, SegmentPhase::Purged);
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

    // Batch: DarkBadger wa-1u90p.7.1

    fn make_test_segment(id: &str, tier: SensitivityTier, created_at_ms: u64) -> SegmentMeta {
        make_segment(id, tier, SegmentPhase::Active, created_at_ms)
    }

    #[test]
    fn retention_config_debug_clone() {
        let c = RetentionConfig::default();
        let c2 = c.clone();
        assert_eq!(c.hot_hours, c2.hot_hours);
        assert_eq!(c.warm_days, c2.warm_days);
        let _ = format!("{:?}", c);
    }

    #[test]
    fn retention_config_default_values() {
        let c = RetentionConfig::default();
        assert_eq!(c.hot_hours, 24);
        assert_eq!(c.warm_days, 7);
        assert_eq!(c.cold_days, 30);
        assert_eq!(c.t3_max_hours, 24);
        assert_eq!(c.t1_extended_days, 30);
        assert_eq!(c.max_segment_bytes, 256 * 1024 * 1024);
        assert_eq!(c.max_segment_duration_secs, 3600);
    }

    #[test]
    fn retention_config_serde_roundtrip() {
        let c = RetentionConfig::default();
        let json = serde_json::to_string(&c).unwrap();
        let back: RetentionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hot_hours, c.hot_hours);
        assert_eq!(back.warm_days, c.warm_days);
        assert_eq!(back.cold_days, c.cold_days);
    }

    #[test]
    fn retention_config_validate_zero_warm_days() {
        let mut c = RetentionConfig::default();
        c.warm_days = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn retention_config_validate_zero_cold_days() {
        let mut c = RetentionConfig::default();
        c.cold_days = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn retention_config_validate_zero_segment_bytes() {
        let mut c = RetentionConfig::default();
        c.max_segment_bytes = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn retention_config_validate_zero_segment_duration() {
        let mut c = RetentionConfig::default();
        c.max_segment_duration_secs = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn retention_config_validate_t1_extended_days_limit() {
        let mut c = RetentionConfig::default();
        c.t1_extended_days = 91;
        assert!(c.validate().is_err());
        c.t1_extended_days = 90;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn sensitivity_tier_debug_clone_copy_eq() {
        let a = SensitivityTier::T1Standard;
        let b = a; // Copy
        assert_eq!(a, b);
        let c = a;
        assert_eq!(a, c);
        assert_ne!(SensitivityTier::T1Standard, SensitivityTier::T2Sensitive);
        let _ = format!("{:?}", a);
    }

    #[test]
    fn sensitivity_tier_ord_ordering() {
        assert!(SensitivityTier::T1Standard < SensitivityTier::T2Sensitive);
        assert!(SensitivityTier::T2Sensitive < SensitivityTier::T3Restricted);
    }

    #[test]
    fn sensitivity_tier_hash_in_set() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(SensitivityTier::T1Standard);
        set.insert(SensitivityTier::T3Restricted);
        set.insert(SensitivityTier::T1Standard); // dup
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn sensitivity_tier_serde_all_three() {
        let expected = [
            (SensitivityTier::T1Standard, "\"t1_standard\""),
            (SensitivityTier::T2Sensitive, "\"t2_sensitive\""),
            (SensitivityTier::T3Restricted, "\"t3_restricted\""),
        ];
        for (tier, json_str) in expected {
            let json = serde_json::to_string(&tier).unwrap();
            assert_eq!(json, json_str);
            let back: SensitivityTier = serde_json::from_str(&json).unwrap();
            assert_eq!(back, tier);
        }
    }

    #[test]
    fn sensitivity_tier_requires_accelerated_purge() {
        assert!(!SensitivityTier::T1Standard.requires_accelerated_purge());
        assert!(!SensitivityTier::T2Sensitive.requires_accelerated_purge());
        assert!(SensitivityTier::T3Restricted.requires_accelerated_purge());
    }

    #[test]
    fn segment_phase_debug_clone_copy_eq() {
        let a = SegmentPhase::Active;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(SegmentPhase::Active, SegmentPhase::Sealed);
        let _ = format!("{:?}", a);
    }

    #[test]
    fn segment_phase_ord_ordering() {
        assert!(SegmentPhase::Active < SegmentPhase::Sealed);
        assert!(SegmentPhase::Sealed < SegmentPhase::Archived);
        assert!(SegmentPhase::Archived < SegmentPhase::Purged);
    }

    #[test]
    fn segment_phase_hash_in_set() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        for p in [
            SegmentPhase::Active,
            SegmentPhase::Sealed,
            SegmentPhase::Archived,
            SegmentPhase::Purged,
        ] {
            set.insert(p);
        }
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn segment_phase_serde_all_four() {
        let expected = [
            (SegmentPhase::Active, "\"active\""),
            (SegmentPhase::Sealed, "\"sealed\""),
            (SegmentPhase::Archived, "\"archived\""),
            (SegmentPhase::Purged, "\"purged\""),
        ];
        for (phase, json_str) in expected {
            let json = serde_json::to_string(&phase).unwrap();
            assert_eq!(json, json_str);
            let back: SegmentPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(back, phase);
        }
    }

    #[test]
    fn segment_phase_is_writable() {
        assert!(SegmentPhase::Active.is_writable());
        assert!(!SegmentPhase::Sealed.is_writable());
        assert!(!SegmentPhase::Archived.is_writable());
        assert!(!SegmentPhase::Purged.is_writable());
    }

    #[test]
    fn segment_phase_is_queryable() {
        assert!(SegmentPhase::Active.is_queryable());
        assert!(SegmentPhase::Sealed.is_queryable());
        assert!(SegmentPhase::Archived.is_queryable());
        assert!(!SegmentPhase::Purged.is_queryable());
    }

    #[test]
    fn segment_phase_valid_transitions_all() {
        assert_eq!(
            SegmentPhase::Active.valid_transitions(),
            &[SegmentPhase::Sealed]
        );
        assert_eq!(
            SegmentPhase::Sealed.valid_transitions(),
            &[SegmentPhase::Archived]
        );
        assert_eq!(
            SegmentPhase::Archived.valid_transitions(),
            &[SegmentPhase::Purged]
        );
        assert!(SegmentPhase::Purged.valid_transitions().is_empty());
    }

    #[test]
    fn segment_phase_can_transition_to_invalid() {
        assert!(!SegmentPhase::Active.can_transition_to(SegmentPhase::Archived)); // skip
        assert!(!SegmentPhase::Sealed.can_transition_to(SegmentPhase::Active)); // backwards
        assert!(!SegmentPhase::Purged.can_transition_to(SegmentPhase::Active)); // from terminal
    }

    #[test]
    fn segment_meta_make_id_all_tiers() {
        assert_eq!(
            SegmentMeta::make_id(0, SensitivityTier::T1Standard, 1000),
            "0_t1_1000"
        );
        assert_eq!(
            SegmentMeta::make_id(5, SensitivityTier::T2Sensitive, 2000),
            "5_t2_2000"
        );
        assert_eq!(
            SegmentMeta::make_id(10, SensitivityTier::T3Restricted, 3000),
            "10_t3_3000"
        );
    }

    #[test]
    fn segment_meta_debug_clone_serde() {
        let m = make_test_segment("seg-1", SensitivityTier::T1Standard, 1000);
        let c = m.clone();
        assert_eq!(c.segment_id, "seg-1");
        let _ = format!("{:?}", m);
        let json = serde_json::to_string(&m).unwrap();
        let back: SegmentMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.segment_id, "seg-1");
        assert_eq!(back.sensitivity, SensitivityTier::T1Standard);
    }

    #[test]
    fn retention_sweep_result_default_empty() {
        let r = RetentionSweepResult::default();
        assert!(r.sealed.is_empty());
        assert!(r.archived.is_empty());
        assert!(r.purge_candidates.is_empty());
        assert!(r.purged.is_empty());
        assert!(r.held.is_empty());
        let _ = format!("{:?}", r);
    }

    #[test]
    fn retention_sweep_result_serde_roundtrip() {
        let mut r = RetentionSweepResult::default();
        r.sealed.push("seg-1".into());
        r.purged.push("seg-2".into());
        let json = serde_json::to_string(&r).unwrap();
        let back: RetentionSweepResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sealed, vec!["seg-1"]);
        assert_eq!(back.purged, vec!["seg-2"]);
    }

    #[test]
    fn retention_stats_debug_clone_serde() {
        let mut s = RetentionStats::default();
        s.active_count = 2;
        s.active_bytes = 1024;
        s.sealed_count = 1;
        s.sealed_bytes = 512;
        let c = s.clone();
        assert_eq!(c.live_count(), 3);
        assert_eq!(c.live_bytes(), 1536);
        let _ = format!("{:?}", s);
        let json = serde_json::to_string(&s).unwrap();
        let back: RetentionStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active_count, 2);
    }

    #[test]
    fn retention_audit_type_serde_all_six() {
        let expected = [
            (RetentionAuditType::SegmentSealed, "\"segment_sealed\""),
            (RetentionAuditType::SegmentArchived, "\"segment_archived\""),
            (RetentionAuditType::SegmentPurged, "\"segment_purged\""),
            (
                RetentionAuditType::AcceleratedPurge,
                "\"accelerated_purge\"",
            ),
            (RetentionAuditType::ManualPurge, "\"manual_purge\""),
            (RetentionAuditType::PolicyOverride, "\"policy_override\""),
        ];
        for (variant, json_str) in expected {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, json_str, "variant {:?}", variant);
            let back: RetentionAuditType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn retention_audit_event_debug_clone_serde() {
        let e = RetentionAuditEvent {
            audit_version: "v1".into(),
            event_type: RetentionAuditType::ManualPurge,
            segment_id: "seg-x".into(),
            ordinal_range: Some((0, 50)),
            sensitivity: SensitivityTier::T3Restricted,
            from_phase: Some(SegmentPhase::Archived),
            to_phase: SegmentPhase::Purged,
            timestamp_ms: 9000,
            justification: Some("security incident".into()),
        };
        let c = e.clone();
        assert_eq!(c.segment_id, "seg-x");
        let _ = format!("{:?}", e);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("manual_purge"));
        assert!(json.contains("security incident"));
    }

    #[test]
    fn retention_error_display_invalid_transition() {
        let err = RetentionError::InvalidTransition {
            segment_id: "s1".into(),
            from: SegmentPhase::Active,
            to: SegmentPhase::Purged,
        };
        let msg = err.to_string();
        assert!(msg.contains("s1"));
        assert!(msg.contains("Active"));
        assert!(msg.contains("Purged"));
    }

    #[test]
    fn retention_error_display_not_found() {
        let err = RetentionError::NotFound("missing-seg".into());
        assert!(err.to_string().contains("missing-seg"));
    }

    #[test]
    fn retention_error_std_error() {
        let err = RetentionError::InvalidConfig("test".into());
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn retention_manager_get_segment_mut() {
        let mut mgr = RetentionManager::with_defaults();
        mgr.add_segment(make_test_segment("s1", SensitivityTier::T1Standard, 1000));
        let seg = mgr.get_segment_mut("s1").unwrap();
        seg.size_bytes = 999;
        assert_eq!(mgr.get_segment("s1").unwrap().size_bytes, 999);
        assert!(mgr.get_segment_mut("nonexistent").is_none());
    }

    #[test]
    fn retention_manager_total_events() {
        let mut mgr = RetentionManager::with_defaults();
        let mut seg = make_test_segment("s1", SensitivityTier::T1Standard, 1000);
        seg.event_count = 100;
        mgr.add_segment(seg);
        let mut seg2 = make_test_segment("s2", SensitivityTier::T2Sensitive, 2000);
        seg2.event_count = 50;
        mgr.add_segment(seg2);
        assert_eq!(mgr.total_events(), 150);
    }

    #[test]
    fn retention_manager_config_accessor() {
        let mgr = RetentionManager::with_defaults();
        assert_eq!(mgr.config().hot_hours, 24);
    }

    #[test]
    fn retention_manager_segments_needing_roll() {
        let mut mgr = RetentionManager::with_defaults();
        let mut seg = make_test_segment("s1", SensitivityTier::T1Standard, 1000);
        seg.size_bytes = 256 * 1024 * 1024; // exactly at limit
        mgr.add_segment(seg);
        let needing = mgr.segments_needing_roll(1000);
        assert_eq!(needing.len(), 1);
    }
}
