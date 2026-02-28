//! Property-based tests for replay_fixture_harvest.rs.
//!
//! Covers serde roundtrips for HarvestSourceType, SensitivityTier,
//! RedactionMode, HarvestMetadata, ArtifactRegistryEntry, ArtifactRegistry,
//! HarvestEntryStatus, HarvestEntry, HarvestReport, ArtifactCompression,
//! ArtifactChunkEntry, ArtifactChunkManifest; ordering and label invariants
//! for SensitivityTier; ArtifactRegistry register/query/duplicate detection;
//! RetentionEnforcer tier mapping and tombstone logic; HarvestSource path
//! classification; HarvestSourceFilter include semantics; QualityFilters
//! and RetentionPolicy defaults; ArtifactCompression path detection.

use std::collections::HashSet;
use std::path::PathBuf;

use chrono::{Duration, Utc};
use frankenterm_core::replay_fixture_harvest::{
    ArtifactChunkEntry, ArtifactChunkManifest, ArtifactCompression, ArtifactRegistry,
    ArtifactRegistryEntry, HarvestEntry, HarvestEntryStatus, HarvestMetadata, HarvestReport,
    HarvestSource, HarvestSourceFilter, HarvestSourceType, QualityFilters, RedactionMode,
    RetentionEnforcer, RetentionPolicy, SensitivityTier,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_harvest_source_type() -> impl Strategy<Value = HarvestSourceType> {
    prop_oneof![
        Just(HarvestSourceType::IncidentRecording),
        Just(HarvestSourceType::SwarmCapture),
        Just(HarvestSourceType::ManualExport),
    ]
}

fn arb_sensitivity_tier() -> impl Strategy<Value = SensitivityTier> {
    prop_oneof![
        Just(SensitivityTier::T1),
        Just(SensitivityTier::T2),
        Just(SensitivityTier::T3),
    ]
}

fn arb_redaction_mode() -> impl Strategy<Value = RedactionMode> {
    prop_oneof![
        Just(RedactionMode::Mask),
        Just(RedactionMode::Hash),
        Just(RedactionMode::Drop),
    ]
}

fn arb_harvest_entry_status() -> impl Strategy<Value = HarvestEntryStatus> {
    prop_oneof![
        Just(HarvestEntryStatus::Harvested),
        Just(HarvestEntryStatus::Skipped),
        Just(HarvestEntryStatus::Error),
    ]
}

fn arb_compression() -> impl Strategy<Value = ArtifactCompression> {
    prop_oneof![
        Just(ArtifactCompression::None),
        Just(ArtifactCompression::Gzip),
        Just(ArtifactCompression::Zstd),
    ]
}

fn arb_harvest_metadata() -> impl Strategy<Value = HarvestMetadata> {
    (
        arb_harvest_source_type(),
        proptest::option::of("[A-Za-z0-9_-]{4,12}"),
        0..=10_000usize,
        0..=100usize,
        0..=500usize,
        arb_sensitivity_tier(),
        "[a-f0-9]{64}",
        prop::collection::vec("[a-z:_-]{3,20}", 0..=5),
        any::<bool>(),
        proptest::option::of(1..=365i64),
    )
        .prop_map(
            |(
                source_type,
                incident_id,
                event_count,
                pane_count,
                decision_count,
                sensitivity_tier,
                checksum_sha256,
                risk_tags,
                tombstoned,
                retention_tier_days,
            )| {
                HarvestMetadata {
                    harvest_date: Utc::now().to_rfc3339(),
                    source_type,
                    incident_id,
                    event_count,
                    pane_count,
                    decision_count,
                    sensitivity_tier,
                    checksum_sha256,
                    risk_tags,
                    tombstoned,
                    retention_tier_days,
                }
            },
        )
}

fn arb_registry_entry() -> impl Strategy<Value = ArtifactRegistryEntry> {
    (
        "[a-z_-]{5,20}",
        "[/a-z_.]{5,20}",
        "[/a-z_.]{5,20}",
        arb_harvest_source_type(),
        proptest::option::of("[A-Za-z0-9_-]{4,12}"),
        0..=10_000usize,
        0..=100usize,
        0..=500usize,
        arb_sensitivity_tier(),
        "[a-f0-9]{64}",
        prop::collection::vec("[a-z:_-]{3,20}", 0..=5),
        prop::collection::vec("[a-f0-9]{8,16}", 0..=5),
    )
        .prop_map(
            |(
                artifact_id,
                artifact_path,
                source_path,
                source_type,
                incident_id,
                event_count,
                pane_count,
                decision_count,
                sensitivity_tier,
                checksum_sha256,
                risk_tags,
                event_fingerprints,
            )| {
                ArtifactRegistryEntry {
                    artifact_id,
                    artifact_path,
                    source_path,
                    source_type,
                    incident_id,
                    event_count,
                    pane_count,
                    decision_count,
                    sensitivity_tier,
                    checksum_sha256,
                    risk_tags,
                    event_fingerprints,
                }
            },
        )
}

fn arb_harvest_entry() -> impl Strategy<Value = HarvestEntry> {
    (
        "[/a-z_.]{5,20}",
        arb_harvest_entry_status(),
        proptest::option::of("[/a-z_.]{5,20}"),
        proptest::option::of("[a-z_ ]{5,30}"),
        proptest::option::of(0.0f64..=1.0),
        0..=10_000usize,
    )
        .prop_map(
            |(source_path, status, artifact_path, reason, overlap_ratio, event_count)| {
                HarvestEntry {
                    source_path,
                    status,
                    artifact_path,
                    reason,
                    overlap_ratio,
                    event_count,
                }
            },
        )
}

fn arb_chunk_entry() -> impl Strategy<Value = ArtifactChunkEntry> {
    (
        0..=100usize,
        "[/a-z_.]{5,20}",
        0..=100_000usize,
        proptest::option::of("[a-f0-9]{8,16}"),
        proptest::option::of("[a-f0-9]{8,16}"),
        "[a-f0-9]{64}",
    )
        .prop_map(
            |(index, path, event_count, start_event_id, end_event_id, sha256)| ArtifactChunkEntry {
                index,
                path,
                event_count,
                start_event_id,
                end_event_id,
                sha256,
            },
        )
}

// ── HarvestSourceType ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 1. Serde roundtrip
    #[test]
    fn harvest_source_type_serde_roundtrip(st in arb_harvest_source_type()) {
        let json = serde_json::to_string(&st).unwrap();
        let restored: HarvestSourceType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, st);
    }

    // 2. as_str matches serde variant
    #[test]
    fn harvest_source_type_as_str_matches_serde(st in arb_harvest_source_type()) {
        let json = serde_json::to_string(&st).unwrap();
        let expected = format!("\"{}\"", st.as_str());
        prop_assert_eq!(json, expected);
    }

    // 3. All as_str values are unique
    #[test]
    fn harvest_source_type_as_str_unique(_seed in 0..10u32) {
        let types = [
            HarvestSourceType::IncidentRecording,
            HarvestSourceType::SwarmCapture,
            HarvestSourceType::ManualExport,
        ];
        let strs: Vec<_> = types.iter().map(|t| t.as_str()).collect();
        let mut unique = strs.clone();
        unique.sort();
        unique.dedup();
        prop_assert_eq!(unique.len(), strs.len());
    }
}

// ── SensitivityTier ─────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 4. Serde roundtrip
    #[test]
    fn sensitivity_tier_serde_roundtrip(tier in arb_sensitivity_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let restored: SensitivityTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, tier);
    }

    // 5. Total ordering: T1 < T2 < T3
    #[test]
    fn sensitivity_tier_ordering(a in arb_sensitivity_tier(), b in arb_sensitivity_tier()) {
        fn rank(t: SensitivityTier) -> u8 {
            match t {
                SensitivityTier::T1 => 0,
                SensitivityTier::T2 => 1,
                SensitivityTier::T3 => 2,
            }
        }
        prop_assert_eq!(a.cmp(&b), rank(a).cmp(&rank(b)));
    }

    // 6. as_risk_tag produces unique tags
    #[test]
    fn sensitivity_tier_risk_tags_unique(_seed in 0..10u32) {
        let tiers = [SensitivityTier::T1, SensitivityTier::T2, SensitivityTier::T3];
        let tags: Vec<_> = tiers.iter().map(|t| t.as_risk_tag()).collect();
        let mut unique = tags.clone();
        unique.sort();
        unique.dedup();
        prop_assert_eq!(unique.len(), tags.len());
    }

    // 7. as_risk_tag starts with "tier:"
    #[test]
    fn sensitivity_tier_risk_tag_prefix(tier in arb_sensitivity_tier()) {
        prop_assert!(tier.as_risk_tag().starts_with("tier:"));
    }

    // 8. as_ftreplay_label produces unique labels
    #[test]
    fn sensitivity_tier_ftreplay_labels_unique(_seed in 0..10u32) {
        let tiers = [SensitivityTier::T1, SensitivityTier::T2, SensitivityTier::T3];
        let labels: Vec<_> = tiers.iter().map(|t| t.as_ftreplay_label()).collect();
        let mut unique = labels.clone();
        unique.sort();
        unique.dedup();
        prop_assert_eq!(unique.len(), labels.len());
    }
}

// ── RedactionMode ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 9. Serde roundtrip
    #[test]
    fn redaction_mode_serde_roundtrip(mode in arb_redaction_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let restored: RedactionMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, mode);
    }

    // 10. All modes are distinct
    #[test]
    fn redaction_mode_distinct(_seed in 0..10u32) {
        let modes = [RedactionMode::Mask, RedactionMode::Hash, RedactionMode::Drop];
        for i in 0..modes.len() {
            for j in (i + 1)..modes.len() {
                prop_assert_ne!(modes[i], modes[j]);
            }
        }
    }
}

// ── HarvestMetadata ─────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 11. Serde roundtrip
    #[test]
    fn harvest_metadata_serde_roundtrip(meta in arb_harvest_metadata()) {
        let json = serde_json::to_string(&meta).unwrap();
        let restored: HarvestMetadata = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.event_count, meta.event_count);
        prop_assert_eq!(restored.pane_count, meta.pane_count);
        prop_assert_eq!(restored.decision_count, meta.decision_count);
        prop_assert_eq!(restored.sensitivity_tier, meta.sensitivity_tier);
        prop_assert_eq!(restored.source_type, meta.source_type);
        prop_assert_eq!(&restored.checksum_sha256, &meta.checksum_sha256);
        prop_assert_eq!(restored.tombstoned, meta.tombstoned);
        prop_assert_eq!(restored.retention_tier_days, meta.retention_tier_days);
    }
}

// ── ArtifactRegistryEntry ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 12. Serde roundtrip
    #[test]
    fn registry_entry_serde_roundtrip(entry in arb_registry_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ArtifactRegistryEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.artifact_id, &entry.artifact_id);
        prop_assert_eq!(restored.event_count, entry.event_count);
        prop_assert_eq!(restored.sensitivity_tier, entry.sensitivity_tier);
        prop_assert_eq!(&restored.checksum_sha256, &entry.checksum_sha256);
        prop_assert_eq!(restored.risk_tags.len(), entry.risk_tags.len());
        prop_assert_eq!(
            restored.event_fingerprints.len(),
            entry.event_fingerprints.len()
        );
    }
}

// ── ArtifactRegistry ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 13. Default registry has no artifacts
    #[test]
    fn registry_default_empty(_seed in 0..10u32) {
        let registry = ArtifactRegistry::default();
        prop_assert!(registry.artifacts.is_empty());
    }

    // 14. Register adds entry
    #[test]
    fn registry_register_adds(entry in arb_registry_entry()) {
        let mut registry = ArtifactRegistry::default();
        let id = entry.artifact_id.clone();
        registry.register(entry);
        prop_assert_eq!(registry.artifacts.len(), 1);
        prop_assert_eq!(&registry.artifacts[0].artifact_id, &id);
    }

    // 15. Register deduplicates by artifact_id
    #[test]
    fn registry_register_deduplicates_by_id(entry in arb_registry_entry()) {
        let mut registry = ArtifactRegistry::default();
        registry.register(entry.clone());
        // Register again with same ID — should replace, not add
        let mut entry2 = entry.clone();
        entry2.event_count = entry.event_count + 1;
        registry.register(entry2);
        prop_assert_eq!(registry.artifacts.len(), 1);
        prop_assert_eq!(registry.artifacts[0].event_count, entry.event_count + 1);
    }

    // 16. Register deduplicates by checksum
    #[test]
    fn registry_register_deduplicates_by_checksum(entry in arb_registry_entry()) {
        let mut registry = ArtifactRegistry::default();
        registry.register(entry.clone());
        // Register with different ID but same checksum
        let mut entry2 = entry.clone();
        entry2.artifact_id = format!("{}-v2", entry.artifact_id);
        registry.register(entry2);
        // Should have replaced the old entry
        prop_assert_eq!(registry.artifacts.len(), 1);
    }

    // 17. query_by_incident returns matching entries
    #[test]
    fn registry_query_by_incident(entry in arb_registry_entry()) {
        let mut registry = ArtifactRegistry::default();
        let incident_id = "test-incident-42";
        let mut entry_with_incident = entry.clone();
        entry_with_incident.incident_id = Some(incident_id.to_string());
        registry.register(entry_with_incident);

        let results = registry.query_by_incident(incident_id);
        prop_assert_eq!(results.len(), 1);

        let no_results = registry.query_by_incident("nonexistent");
        prop_assert!(no_results.is_empty());
    }

    // 18. query_by_risk_tag returns matching entries
    #[test]
    fn registry_query_by_risk_tag(entry in arb_registry_entry()) {
        let mut registry = ArtifactRegistry::default();
        let tag = "tier:t3";
        let mut entry_with_tag = entry.clone();
        entry_with_tag.risk_tags = vec![tag.to_string()];
        registry.register(entry_with_tag);

        let results = registry.query_by_risk_tag(tag);
        prop_assert_eq!(results.len(), 1);

        let no_results = registry.query_by_risk_tag("nonexistent");
        prop_assert!(no_results.is_empty());
    }

    // 19. find_duplicate returns highest overlap match
    #[test]
    fn registry_find_duplicate(_seed in 0..10u32) {
        let mut registry = ArtifactRegistry::default();
        let entry = ArtifactRegistryEntry {
            artifact_id: "art-1".to_string(),
            artifact_path: "/artifacts/art-1.ftreplay".to_string(),
            source_path: "/sources/src-1.jsonl".to_string(),
            source_type: HarvestSourceType::IncidentRecording,
            incident_id: None,
            event_count: 100,
            pane_count: 2,
            decision_count: 5,
            sensitivity_tier: SensitivityTier::T1,
            checksum_sha256: "a".repeat(64),
            risk_tags: vec!["tier:t1".to_string()],
            event_fingerprints: vec!["fp1".to_string(), "fp2".to_string(), "fp3".to_string()],
        };
        registry.register(entry);

        // 100% overlap
        let fps: HashSet<String> = ["fp1", "fp2", "fp3"].iter().map(|s| s.to_string()).collect();
        let dup = registry.find_duplicate(&fps, 0.9);
        prop_assert!(dup.is_some());
        let dup = dup.unwrap();
        prop_assert!((dup.overlap_ratio - 1.0).abs() < 1e-10);

        // Below threshold
        let fps2: HashSet<String> = ["fp1", "fpX", "fpY", "fpZ", "fpW"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let dup2 = registry.find_duplicate(&fps2, 0.9);
        prop_assert!(dup2.is_none()); // 1/5 = 0.2, below 0.9

        // Empty fingerprints
        let empty: HashSet<String> = HashSet::new();
        prop_assert!(registry.find_duplicate(&empty, 0.5).is_none());
    }

    // 20. Registry serde roundtrip
    #[test]
    fn registry_serde_roundtrip(entries in prop::collection::vec(arb_registry_entry(), 0..=3)) {
        let mut registry = ArtifactRegistry::default();
        for entry in &entries {
            registry.register(entry.clone());
        }
        let json = serde_json::to_string(&registry).unwrap();
        let restored: ArtifactRegistry = serde_json::from_str(&json).unwrap();
        // Size may differ due to deduplication, but should be <= entries.len()
        prop_assert!(restored.artifacts.len() <= entries.len());
    }
}

// ── HarvestEntryStatus ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 21. Serde roundtrip
    #[test]
    fn harvest_entry_status_serde_roundtrip(status in arb_harvest_entry_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let restored: HarvestEntryStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, status);
    }
}

// ── HarvestEntry ────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 22. Serde roundtrip
    #[test]
    fn harvest_entry_serde_roundtrip(entry in arb_harvest_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let restored: HarvestEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.source_path, &entry.source_path);
        prop_assert_eq!(restored.status, entry.status);
        prop_assert_eq!(restored.event_count, entry.event_count);
    }
}

// ── HarvestReport ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 23. Default report is all zeros
    #[test]
    fn harvest_report_default_empty(_seed in 0..10u32) {
        let report = HarvestReport::default();
        prop_assert_eq!(report.harvested, 0);
        prop_assert_eq!(report.skipped, 0);
        prop_assert_eq!(report.errors, 0);
        prop_assert_eq!(report.total_events, 0);
        prop_assert!(report.entries.is_empty());
    }

    // 24. Serde roundtrip
    #[test]
    fn harvest_report_serde_roundtrip(
        harvested in 0..=100usize,
        skipped in 0..=100usize,
        errors in 0..=100usize,
        total_events in 0..=10_000usize,
    ) {
        let report = HarvestReport {
            harvested,
            skipped,
            errors,
            total_events,
            entries: Vec::new(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let restored: HarvestReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.harvested, harvested);
        prop_assert_eq!(restored.skipped, skipped);
        prop_assert_eq!(restored.errors, errors);
        prop_assert_eq!(restored.total_events, total_events);
    }
}

// ── ArtifactCompression ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 25. Serde roundtrip
    #[test]
    fn compression_serde_roundtrip(comp in arb_compression()) {
        let json = serde_json::to_string(&comp).unwrap();
        let restored: ArtifactCompression = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, comp);
    }

    // 26. from_path detects .gz as Gzip
    #[test]
    fn compression_from_path_gzip(_seed in 0..10u32) {
        let path = PathBuf::from("/tmp/fixture.ftreplay.gz");
        prop_assert_eq!(ArtifactCompression::from_path(&path), ArtifactCompression::Gzip);
    }

    // 27. from_path detects .zst as Zstd
    #[test]
    fn compression_from_path_zstd(_seed in 0..10u32) {
        let path = PathBuf::from("/tmp/fixture.ftreplay.zst");
        prop_assert_eq!(ArtifactCompression::from_path(&path), ArtifactCompression::Zstd);
    }

    // 28. from_path returns None for other extensions
    #[test]
    fn compression_from_path_none(_seed in 0..10u32) {
        let path = PathBuf::from("/tmp/fixture.ftreplay");
        prop_assert_eq!(ArtifactCompression::from_path(&path), ArtifactCompression::None);
    }

    // 29. extension_suffix returns correct suffixes
    #[test]
    fn compression_extension_suffix(comp in arb_compression()) {
        let suffix = comp.extension_suffix();
        match comp {
            ArtifactCompression::None => prop_assert_eq!(suffix, ""),
            ArtifactCompression::Gzip => prop_assert_eq!(suffix, ".gz"),
            ArtifactCompression::Zstd => prop_assert_eq!(suffix, ".zst"),
        }
    }
}

// ── ArtifactChunkEntry ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 30. Serde roundtrip
    #[test]
    fn chunk_entry_serde_roundtrip(entry in arb_chunk_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ArtifactChunkEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.index, entry.index);
        prop_assert_eq!(&restored.path, &entry.path);
        prop_assert_eq!(restored.event_count, entry.event_count);
        prop_assert_eq!(&restored.sha256, &entry.sha256);
    }
}

// ── ArtifactChunkManifest ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 31. Serde roundtrip
    #[test]
    fn chunk_manifest_serde_roundtrip(
        comp in arb_compression(),
        chunks in prop::collection::vec(arb_chunk_entry(), 0..=3),
    ) {
        let manifest = ArtifactChunkManifest {
            schema_version: "ftreplay.chunk_manifest.v1".to_string(),
            artifact_id: "test-artifact-001".to_string(),
            total_event_count: chunks.iter().map(|c| c.event_count).sum(),
            chunk_size_events: 100_000,
            chunk_count: chunks.len(),
            compression: comp,
            chunks,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let restored: ArtifactChunkManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.artifact_id, &manifest.artifact_id);
        prop_assert_eq!(restored.chunk_count, manifest.chunk_count);
        prop_assert_eq!(restored.total_event_count, manifest.total_event_count);
        prop_assert_eq!(restored.compression, manifest.compression);
    }
}

// ── RetentionEnforcer ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 32. retention_days_for_tier maps correctly
    #[test]
    fn retention_days_for_tier(
        t1 in 1..=365i64,
        t2 in 1..=365i64,
        t3 in 1..=365i64,
        tier in arb_sensitivity_tier(),
    ) {
        let enforcer = RetentionEnforcer {
            policy: RetentionPolicy {
                t1_days: t1,
                t2_days: t2,
                t3_days: t3,
            },
        };
        let days = enforcer.retention_days_for_tier(tier);
        match tier {
            SensitivityTier::T1 => prop_assert_eq!(days, t1),
            SensitivityTier::T2 => prop_assert_eq!(days, t2),
            SensitivityTier::T3 => prop_assert_eq!(days, t3),
        }
    }

    // 33. should_tombstone: recent artifact is NOT tombstoned
    #[test]
    fn retention_recent_not_tombstoned(tier in arb_sensitivity_tier()) {
        let enforcer = RetentionEnforcer::default();
        let now = Utc::now();
        let harvest_date = now.to_rfc3339();
        prop_assert!(!enforcer.should_tombstone(&harvest_date, tier, now));
    }

    // 34. should_tombstone: very old artifact IS tombstoned
    #[test]
    fn retention_old_is_tombstoned(tier in arb_sensitivity_tier()) {
        let enforcer = RetentionEnforcer::default();
        let now = Utc::now();
        let old_date = (now - Duration::days(500)).to_rfc3339();
        prop_assert!(enforcer.should_tombstone(&old_date, tier, now));
    }

    // 35. should_tombstone with invalid date returns false
    #[test]
    fn retention_invalid_date_not_tombstoned(tier in arb_sensitivity_tier()) {
        let enforcer = RetentionEnforcer::default();
        prop_assert!(!enforcer.should_tombstone("not-a-date", tier, Utc::now()));
    }
}

// ── QualityFilters defaults ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 36. Default quality filters have positive minimums
    #[test]
    fn quality_filters_defaults(_seed in 0..10u32) {
        let filters = QualityFilters::default();
        prop_assert!(filters.min_event_count > 0);
        prop_assert!(filters.min_pane_count > 0);
        prop_assert!(filters.min_decision_count > 0);
        prop_assert!(filters.duplicate_overlap_threshold > 0.0);
        prop_assert!(filters.duplicate_overlap_threshold <= 1.0);
    }
}

// ── RetentionPolicy defaults ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 37. Default retention: T1 >= T2 >= T3
    #[test]
    fn retention_policy_defaults_ordered(_seed in 0..10u32) {
        let policy = RetentionPolicy::default();
        prop_assert!(policy.t1_days >= policy.t2_days);
        prop_assert!(policy.t2_days >= policy.t3_days);
        prop_assert!(policy.t3_days > 0);
    }
}

// ── HarvestSource ───────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 38. from_path classifies "incident" paths as IncidentRecording
    #[test]
    fn harvest_source_incident_classification(_seed in 0..10u32) {
        let source = HarvestSource::from_path(PathBuf::from("/data/incident-2024-01.jsonl"));
        prop_assert_eq!(source.source_type(), HarvestSourceType::IncidentRecording);
        prop_assert!(source.incident_id().is_some());
    }

    // 39. from_path classifies "swarm" paths as SwarmCapture
    #[test]
    fn harvest_source_swarm_classification(_seed in 0..10u32) {
        let source = HarvestSource::from_path(PathBuf::from("/data/swarm-run-42.jsonl"));
        prop_assert_eq!(source.source_type(), HarvestSourceType::SwarmCapture);
        prop_assert!(source.incident_id().is_none());
    }

    // 40. from_path classifies generic paths as ManualExport
    #[test]
    fn harvest_source_manual_classification(_seed in 0..10u32) {
        let source = HarvestSource::from_path(PathBuf::from("/data/export-2024.jsonl"));
        prop_assert_eq!(source.source_type(), HarvestSourceType::ManualExport);
        prop_assert!(source.incident_id().is_none());
    }

    // 41. path() returns the original path
    #[test]
    fn harvest_source_path_preserved(_seed in 0..10u32) {
        let path = PathBuf::from("/data/some-recording.jsonl");
        let source = HarvestSource::from_path(path.clone());
        prop_assert_eq!(source.path(), path.as_path());
    }
}

// ── HarvestSourceFilter ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 42. All filter includes everything
    #[test]
    fn harvest_filter_all_includes_all(_seed in 0..10u32) {
        let filter = HarvestSourceFilter::All;
        let incident = HarvestSource::from_path(PathBuf::from("/data/incident.jsonl"));
        let swarm = HarvestSource::from_path(PathBuf::from("/data/swarm.jsonl"));
        let manual = HarvestSource::from_path(PathBuf::from("/data/export.jsonl"));
        prop_assert!(filter.includes(&incident));
        prop_assert!(filter.includes(&swarm));
        prop_assert!(filter.includes(&manual));
    }

    // 43. IncidentOnly filter excludes non-incidents
    #[test]
    fn harvest_filter_incident_only(_seed in 0..10u32) {
        let filter = HarvestSourceFilter::IncidentOnly;
        let incident = HarvestSource::from_path(PathBuf::from("/data/incident.jsonl"));
        let manual = HarvestSource::from_path(PathBuf::from("/data/export.jsonl"));
        prop_assert!(filter.includes(&incident));
        prop_assert!(!filter.includes(&manual));
    }

    // 44. from_cli_flag parses valid flags
    #[test]
    fn harvest_filter_from_cli_valid(_seed in 0..10u32) {
        prop_assert_eq!(
            HarvestSourceFilter::from_cli_flag("all").unwrap(),
            HarvestSourceFilter::All
        );
        prop_assert_eq!(
            HarvestSourceFilter::from_cli_flag("incident-only").unwrap(),
            HarvestSourceFilter::IncidentOnly
        );
    }

    // 45. from_cli_flag rejects invalid flags
    #[test]
    fn harvest_filter_from_cli_invalid(_seed in 0..10u32) {
        prop_assert!(HarvestSourceFilter::from_cli_flag("unknown").is_err());
    }
}
