//! Property-based tests for replay_artifact_registry (ft-og6q6.6.4).
//!
//! Invariants tested:
//! - AR-1: ArtifactSensitivityTier serde roundtrip
//! - AR-2: ArtifactStatus serde roundtrip
//! - AR-3: ArtifactEntry serde roundtrip
//! - AR-4: ArtifactManifest TOML roundtrip
//! - AR-5: ArtifactSensitivityTier ordering (T1 < T2 < T3)
//! - AR-6: from_str_arg roundtrips as_str
//! - AR-7: SHA-256 deterministic (same input → same output)
//! - AR-8: SHA-256 length always 64
//! - AR-9: Empty manifest validates clean
//! - AR-10: Duplicate path detected by validate
//! - AR-11: Add then find succeeds
//! - AR-12: Add rejects duplicate
//! - AR-13: Retire sets status and reason
//! - AR-14: Prune removes only old retired
//! - AR-15: Prune dry_run preserves manifest
//! - AR-16: List filter by tier returns correct subset
//! - AR-17: List filter by status returns correct subset
//! - AR-18: Inspect missing → error
//! - AR-19: Inspect with matching checksum → integrity_ok
//! - AR-20: PruneResult serde roundtrip
//! - AR-21: ManifestValidationError serde roundtrip
//! - AR-22: Active count + retired count = total count
//! - AR-23: Prune bytes_freed = sum of pruned entry sizes
//! - AR-24: Add updates last_updated_ms
//! - AR-25: Retire updates last_updated_ms

use proptest::prelude::*;
use std::io::Write as _;
use std::path::Path;

use frankenterm_core_replay::replay_artifact_registry::{
    ArtifactEntry, ArtifactManifest, ArtifactRegistry, ArtifactSensitivityTier, ArtifactStatus,
    ListFilter, MANIFEST_SCHEMA_VERSION, ManifestValidationError, PruneOptions, PruneResult,
    sha256_bytes,
};

// ── Real FS fixture ──────────────────────────────────────────────────────

fn write_artifact_file(base_dir: &Path, relative: &str, content: &[u8]) {
    let path = base_dir.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create artifact parent directory");
    }
    let mut file = std::fs::File::create(&path).expect("create artifact file");
    file.write_all(content).expect("write artifact file");
    file.sync_all().expect("sync artifact file");
    let metadata = std::fs::metadata(&path).expect("stat artifact file");
    assert_eq!(metadata.len(), content.len() as u64);
}

// ── Strategies ───────────────────────────────────────────────────────────

fn arb_tier() -> impl Strategy<Value = ArtifactSensitivityTier> {
    prop_oneof![
        Just(ArtifactSensitivityTier::T1),
        Just(ArtifactSensitivityTier::T2),
        Just(ArtifactSensitivityTier::T3),
    ]
}

fn arb_status() -> impl Strategy<Value = ArtifactStatus> {
    prop_oneof![Just(ArtifactStatus::Active), Just(ArtifactStatus::Retired),]
}

fn arb_entry() -> impl Strategy<Value = ArtifactEntry> {
    (
        "[a-z]{3,8}\\.ftreplay",
        "[a-z_]{3,10}",
        arb_tier(),
        0u64..100_000,
        0u64..100,
        0u64..10,
        1000u64..999_999,
    )
        .prop_map(|(path, label, tier, size, events, decisions, created)| {
            let content = format!("content_{}", path);
            ArtifactEntry {
                path,
                label,
                sha256: sha256_bytes(content.as_bytes()),
                event_count: events,
                decision_count: decisions,
                created_at_ms: created,
                sensitivity_tier: tier,
                status: ArtifactStatus::Active,
                size_bytes: size,
                retire_reason: None,
                retired_at_ms: None,
            }
        })
}

fn make_entry(path: &str, content: &[u8]) -> ArtifactEntry {
    ArtifactEntry {
        path: path.to_string(),
        label: "test".to_string(),
        sha256: sha256_bytes(content),
        event_count: 0,
        decision_count: 0,
        created_at_ms: 1000,
        sensitivity_tier: ArtifactSensitivityTier::T1,
        status: ArtifactStatus::Active,
        size_bytes: content.len() as u64,
        retire_reason: None,
        retired_at_ms: None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── AR-1: Tier serde roundtrip ───────────────────────────────────────

    #[test]
    fn ar1_tier_serde(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let restored: ArtifactSensitivityTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, tier);
    }

    // ── AR-2: Status serde roundtrip ─────────────────────────────────────

    #[test]
    fn ar2_status_serde(status in arb_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let restored: ArtifactStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, status);
    }

    // ── AR-3: Entry serde roundtrip ──────────────────────────────────────

    #[test]
    fn ar3_entry_serde(entry in arb_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ArtifactEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.path, entry.path);
        prop_assert_eq!(restored.label, entry.label);
        prop_assert_eq!(restored.sensitivity_tier, entry.sensitivity_tier);
        prop_assert_eq!(restored.event_count, entry.event_count);
    }

    // ── AR-4: Manifest TOML roundtrip ────────────────────────────────────

    #[test]
    fn ar4_manifest_toml_roundtrip(n in 0usize..5) {
        let mut manifest = ArtifactManifest::new();
        manifest.last_updated_ms = 5000;
        for i in 0..n {
            let path = format!("artifact_{}.ftreplay", i);
            let content = format!("content_{}", i);
            manifest.artifacts.push(ArtifactEntry {
                path,
                label: format!("label_{}", i),
                sha256: sha256_bytes(content.as_bytes()),
                event_count: i as u64,
                decision_count: 1,
                created_at_ms: 1000 + i as u64,
                sensitivity_tier: ArtifactSensitivityTier::T1,
                status: ArtifactStatus::Active,
                size_bytes: 100,
                retire_reason: None,
                retired_at_ms: None,
            });
        }
        let toml_str = manifest.to_toml().unwrap();
        let restored = ArtifactManifest::from_toml(&toml_str).unwrap();
        prop_assert_eq!(restored.artifacts.len(), n);
        prop_assert_eq!(restored.schema_version, MANIFEST_SCHEMA_VERSION);
    }

    // ── AR-5: Tier ordering ──────────────────────────────────────────────

    #[test]
    fn ar5_tier_ordering(_dummy in 0u8..1) {
        prop_assert!(ArtifactSensitivityTier::T1 < ArtifactSensitivityTier::T2);
        prop_assert!(ArtifactSensitivityTier::T2 < ArtifactSensitivityTier::T3);
    }

    // ── AR-6: from_str_arg roundtrips as_str ─────────────────────────────

    #[test]
    fn ar6_str_roundtrip(tier in arb_tier()) {
        let s = tier.as_str();
        let restored = ArtifactSensitivityTier::from_str_arg(s);
        prop_assert_eq!(restored, Some(tier));
    }

    // ── AR-7: SHA-256 deterministic ──────────────────────────────────────

    #[test]
    fn ar7_sha256_deterministic(data in prop::collection::vec(any::<u8>(), 0..100)) {
        let a = sha256_bytes(&data);
        let b = sha256_bytes(&data);
        prop_assert_eq!(a, b);
    }

    // ── AR-8: SHA-256 length 64 ──────────────────────────────────────────

    #[test]
    fn ar8_sha256_length(data in prop::collection::vec(any::<u8>(), 0..100)) {
        let hash = sha256_bytes(&data);
        prop_assert_eq!(hash.len(), 64);
    }

    // ── AR-9: Empty manifest validates clean ─────────────────────────────

    #[test]
    fn ar9_empty_validates(_dummy in 0u8..1) {
        let m = ArtifactManifest::new();
        prop_assert!(m.validate().is_empty());
    }

    // ── AR-10: Duplicate path detected ───────────────────────────────────

    #[test]
    fn ar10_duplicate_detected(path in "[a-z]{3,8}\\.ftreplay") {
        let content = b"dup";
        let mut m = ArtifactManifest::new();
        m.artifacts.push(make_entry(&path, content));
        m.artifacts.push(make_entry(&path, content));
        let errors = m.validate();
        let has_dup = errors.iter().any(|e| matches!(e, ManifestValidationError::DuplicatePath { .. }));
        prop_assert!(has_dup);
    }

    // ── AR-11: Add then find succeeds ────────────────────────────────────

    #[test]
    fn ar11_add_find(path in "[a-z]{3,8}\\.ftreplay") {
        let content = b"find-me";
        let temp = tempfile::tempdir().expect("tempdir");
        write_artifact_file(temp.path(), &path, content);
        let manifest = ArtifactManifest::new();
        let mut reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        reg.add(&path, "lbl", ArtifactSensitivityTier::T1, 2000).unwrap();
        let found = reg.manifest().find(&path);
        prop_assert!(found.is_some());
        prop_assert_eq!(found.unwrap().label.as_str(), "lbl");
    }

    // ── AR-12: Add rejects duplicate ─────────────────────────────────────

    #[test]
    fn ar12_add_rejects_dup(path in "[a-z]{3,8}\\.ftreplay") {
        let content = b"first";
        let temp = tempfile::tempdir().expect("tempdir");
        write_artifact_file(temp.path(), &path, content);
        let manifest = ArtifactManifest::new();
        let mut reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        reg.add(&path, "lbl", ArtifactSensitivityTier::T1, 2000).unwrap();
        let result = reg.add(&path, "lbl2", ArtifactSensitivityTier::T1, 3000);
        prop_assert!(result.is_err());
    }

    // ── AR-13: Retire sets status and reason ─────────────────────────────

    #[test]
    fn ar13_retire_sets_fields(reason in "[a-z ]{5,20}") {
        let content = b"retire-me";
        let entry = make_entry("retire.ftreplay", content);
        let temp = tempfile::tempdir().expect("tempdir");
        write_artifact_file(temp.path(), "retire.ftreplay", content);
        let manifest = ArtifactManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.to_string(),
            last_updated_ms: 1000,
            artifacts: vec![entry],
        };
        let mut reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        reg.retire("retire.ftreplay", &reason, 5000).unwrap();
        let e = reg.manifest().find("retire.ftreplay").unwrap();
        prop_assert_eq!(e.status, ArtifactStatus::Retired);
        prop_assert_eq!(e.retire_reason.as_deref(), Some(reason.as_str()));
        prop_assert_eq!(e.retired_at_ms, Some(5000));
    }

    // ── AR-14: Prune removes only old retired ────────────────────────────

    #[test]
    fn ar14_prune_selective(n_active in 1usize..5, n_old_retired in 0usize..3) {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut manifest = ArtifactManifest::new();

        for i in 0..n_active {
            let path = format!("active_{}.ftreplay", i);
            let content = format!("a_{}", i);
            manifest.artifacts.push(ArtifactEntry {
                path: path.clone(),
                label: "active".into(),
                sha256: sha256_bytes(content.as_bytes()),
                event_count: 0,
                decision_count: 0,
                created_at_ms: 1000,
                sensitivity_tier: ArtifactSensitivityTier::T1,
                status: ArtifactStatus::Active,
                size_bytes: 10,
                retire_reason: None,
                retired_at_ms: None,
            });
            write_artifact_file(temp.path(), &path, content.as_bytes());
        }

        for i in 0..n_old_retired {
            let path = format!("old_{}.ftreplay", i);
            let content = format!("r_{}", i);
            manifest.artifacts.push(ArtifactEntry {
                path: path.clone(),
                label: "retired".into(),
                sha256: sha256_bytes(content.as_bytes()),
                event_count: 0,
                decision_count: 0,
                created_at_ms: 1000,
                sensitivity_tier: ArtifactSensitivityTier::T1,
                status: ArtifactStatus::Retired,
                retire_reason: Some("old".into()),
                retired_at_ms: Some(1000),
                size_bytes: 20,
            });
            write_artifact_file(temp.path(), &path, content.as_bytes());
        }

        let mut reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        let result = reg.prune(&PruneOptions {
            dry_run: false,
            max_age_days: 1,
            now_ms: 1000 + 2 * 24 * 60 * 60 * 1000,
        });

        prop_assert_eq!(result.pruned_count, n_old_retired as u64);
        prop_assert_eq!(reg.manifest().artifacts.len(), n_active);
        for path in result.pruned_paths {
            prop_assert!(!temp.path().join(path).exists());
        }
    }

    // ── AR-15: Prune dry_run preserves manifest ──────────────────────────

    #[test]
    fn ar15_prune_dry_run(_dummy in 0u8..1) {
        let mut manifest = ArtifactManifest::new();
        let mut entry = make_entry("dry.ftreplay", b"dry");
        entry.status = ArtifactStatus::Retired;
        entry.retired_at_ms = Some(1000);
        manifest.artifacts.push(entry);
        let temp = tempfile::tempdir().expect("tempdir");
        write_artifact_file(temp.path(), "dry.ftreplay", b"dry");
        let mut reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        let result = reg.prune(&PruneOptions {
            dry_run: true,
            max_age_days: 0,
            now_ms: 999_999_999,
        });
        prop_assert!(result.dry_run);
        prop_assert_eq!(reg.manifest().artifacts.len(), 1);
        prop_assert!(temp.path().join("dry.ftreplay").exists());
    }

    // ── AR-16: List filter by tier ───────────────────────────────────────

    #[test]
    fn ar16_list_tier_filter(n_t1 in 0usize..5, n_t2 in 0usize..5) {
        let mut manifest = ArtifactManifest::new();
        for i in 0..n_t1 {
            let mut e = make_entry(&format!("t1_{}.ftreplay", i), b"t1");
            e.sensitivity_tier = ArtifactSensitivityTier::T1;
            manifest.artifacts.push(e);
        }
        for i in 0..n_t2 {
            let mut e = make_entry(&format!("t2_{}.ftreplay", i), b"t2");
            e.sensitivity_tier = ArtifactSensitivityTier::T2;
            manifest.artifacts.push(e);
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        let filter = ListFilter {
            tier: Some(ArtifactSensitivityTier::T1),
            ..Default::default()
        };
        prop_assert_eq!(reg.list(&filter).len(), n_t1);
    }

    // ── AR-17: List filter by status ─────────────────────────────────────

    #[test]
    fn ar17_list_status_filter(n_active in 0usize..5, n_retired in 0usize..5) {
        let mut manifest = ArtifactManifest::new();
        for i in 0..n_active {
            manifest.artifacts.push(make_entry(&format!("a_{}.ftreplay", i), b"a"));
        }
        for i in 0..n_retired {
            let mut e = make_entry(&format!("r_{}.ftreplay", i), b"r");
            e.status = ArtifactStatus::Retired;
            manifest.artifacts.push(e);
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        let filter = ListFilter {
            status: Some(ArtifactStatus::Active),
            ..Default::default()
        };
        prop_assert_eq!(reg.list(&filter).len(), n_active);
    }

    // ── AR-18: Inspect missing → error ───────────────────────────────────

    #[test]
    fn ar18_inspect_missing(path in "[a-z]{3,8}\\.ftreplay") {
        let temp = tempfile::tempdir().expect("tempdir");
        let manifest = ArtifactManifest::new();
        let reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        let result = reg.inspect(&path);
        prop_assert!(result.is_err());
    }

    // ── AR-19: Inspect matching checksum → integrity_ok ──────────────────

    #[test]
    fn ar19_inspect_integrity(data in prop::collection::vec(any::<u8>(), 1..50)) {
        let entry = ArtifactEntry {
            path: "check.ftreplay".into(),
            label: "check".into(),
            sha256: sha256_bytes(&data),
            event_count: 0,
            decision_count: 0,
            created_at_ms: 1000,
            sensitivity_tier: ArtifactSensitivityTier::T1,
            status: ArtifactStatus::Active,
            size_bytes: data.len() as u64,
            retire_reason: None,
            retired_at_ms: None,
        };
        let temp = tempfile::tempdir().expect("tempdir");
        write_artifact_file(temp.path(), "check.ftreplay", &data);
        let manifest = ArtifactManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.to_string(),
            last_updated_ms: 1000,
            artifacts: vec![entry],
        };
        let reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        let detail = reg.inspect("check.ftreplay").unwrap();
        prop_assert!(detail.integrity_ok);
        prop_assert!(detail.file_exists);
    }

    // ── AR-20: PruneResult serde roundtrip ───────────────────────────────

    #[test]
    fn ar20_prune_result_serde(count in 0u64..10, freed in 0u64..10000, dry in proptest::bool::ANY) {
        let result = PruneResult {
            pruned_paths: (0..count).map(|i| format!("p_{}.ftreplay", i)).collect(),
            pruned_count: count,
            bytes_freed: freed,
            dry_run: dry,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: PruneResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.pruned_count, count);
        prop_assert_eq!(restored.bytes_freed, freed);
        prop_assert_eq!(restored.dry_run, dry);
    }

    // ── AR-21: ManifestValidationError serde roundtrip ───────────────────

    #[test]
    fn ar21_validation_error_serde(_dummy in 0u8..1) {
        let errors = vec![
            ManifestValidationError::DuplicatePath { path: "dup.ftreplay".into() },
            ManifestValidationError::InvalidChecksum { path: "bad.ftreplay".into(), reason: "short".into() },
            ManifestValidationError::EmptyPath,
            ManifestValidationError::MissingFile { path: "gone.ftreplay".into() },
            ManifestValidationError::ChecksumMismatch {
                path: "mismatch.ftreplay".into(),
                expected: "a".repeat(64),
                actual: "b".repeat(64),
            },
        ];
        for err in &errors {
            let json = serde_json::to_string(err).unwrap();
            let restored: ManifestValidationError = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&restored, err);
        }
    }

    // ── AR-22: Active + retired = total ──────────────────────────────────

    #[test]
    fn ar22_count_partition(n_active in 0usize..5, n_retired in 0usize..5) {
        let mut manifest = ArtifactManifest::new();
        for i in 0..n_active {
            manifest.artifacts.push(make_entry(&format!("a_{}.ftreplay", i), b"a"));
        }
        for i in 0..n_retired {
            let mut e = make_entry(&format!("r_{}.ftreplay", i), b"r");
            e.status = ArtifactStatus::Retired;
            manifest.artifacts.push(e);
        }
        let active = manifest.active_artifacts().len();
        let retired = manifest.retired_artifacts().len();
        prop_assert_eq!(active + retired, manifest.artifacts.len());
    }

    // ── AR-23: Prune bytes_freed = sum of pruned entry sizes ─────────────

    #[test]
    fn ar23_prune_bytes_sum(sizes in prop::collection::vec(10u64..1000, 1..5)) {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut manifest = ArtifactManifest::new();
        for (i, size) in sizes.iter().enumerate() {
            let content = format!("s_{}", i);
            let path = format!("s_{}.ftreplay", i);
            manifest.artifacts.push(ArtifactEntry {
                path: path.clone(),
                label: "sized".into(),
                sha256: sha256_bytes(content.as_bytes()),
                event_count: 0,
                decision_count: 0,
                created_at_ms: 1000,
                sensitivity_tier: ArtifactSensitivityTier::T1,
                status: ArtifactStatus::Retired,
                retire_reason: Some("test".into()),
                retired_at_ms: Some(1000),
                size_bytes: *size,
            });
            write_artifact_file(temp.path(), &path, content.as_bytes());
        }
        let mut reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        let result = reg.prune(&PruneOptions {
            dry_run: false,
            max_age_days: 0,
            now_ms: 1000 + 24 * 60 * 60 * 1000,
        });
        let expected_sum: u64 = sizes.iter().sum();
        prop_assert_eq!(result.bytes_freed, expected_sum);
    }

    // ── AR-24: Add updates last_updated_ms ───────────────────────────────

    #[test]
    fn ar24_add_updates_timestamp(now_ms in 1000u64..999_999) {
        let content = b"ts_test";
        let temp = tempfile::tempdir().expect("tempdir");
        write_artifact_file(temp.path(), "ts.ftreplay", content);
        let manifest = ArtifactManifest::new();
        let mut reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        reg.add("ts.ftreplay", "ts", ArtifactSensitivityTier::T1, now_ms).unwrap();
        prop_assert_eq!(reg.manifest().last_updated_ms, now_ms);
    }

    // ── AR-25: Retire updates last_updated_ms ────────────────────────────

    #[test]
    fn ar25_retire_updates_timestamp(now_ms in 2000u64..999_999) {
        let entry = make_entry("rt.ftreplay", b"retire_ts");
        let temp = tempfile::tempdir().expect("tempdir");
        write_artifact_file(temp.path(), "rt.ftreplay", b"retire_ts");
        let manifest = ArtifactManifest {
            schema_version: MANIFEST_SCHEMA_VERSION.to_string(),
            last_updated_ms: 1000,
            artifacts: vec![entry],
        };
        let mut reg = ArtifactRegistry::new(manifest, temp.path().to_path_buf());
        reg.retire("rt.ftreplay", "reason", now_ms).unwrap();
        prop_assert_eq!(reg.manifest().last_updated_ms, now_ms);
    }
}
