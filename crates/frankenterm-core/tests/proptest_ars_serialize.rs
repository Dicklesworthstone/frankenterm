//! Property-based tests for ARS Disk Serialization & Auto-Pruning.
//!
//! Verifies manifest invariants, store consistency, prune correctness,
//! blacklist integrity, and serde roundtrips.

use proptest::prelude::*;
// std collections used transitively by strategies

use frankenterm_core::ars_blast_radius::MaturityTier;
use frankenterm_core::ars_drift::EValueConfig;
use frankenterm_core::ars_evidence::EvidenceVerdict;
use frankenterm_core::ars_evolve::VersionStatus;
use frankenterm_core::ars_serialize::{
    BlacklistEntry, BlacklistReason, DriftSnapshot, EvidenceSummary, ManifestEntry, PruneConfig,
    PruneEngine, PruneResult, PruneStats, ReflexManifest, ReflexRecord, ReflexStore,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_maturity() -> impl Strategy<Value = MaturityTier> {
    prop_oneof![
        Just(MaturityTier::Incubating),
        Just(MaturityTier::Graduated),
        Just(MaturityTier::Veteran),
    ]
}

fn arb_status() -> impl Strategy<Value = VersionStatus> {
    prop_oneof![
        Just(VersionStatus::Active),
        Just(VersionStatus::Incubating),
        Just(VersionStatus::Deprecated),
        Just(VersionStatus::Disabled),
    ]
}

fn arb_verdict() -> impl Strategy<Value = EvidenceVerdict> {
    prop_oneof![
        Just(EvidenceVerdict::Support),
        Just(EvidenceVerdict::Neutral),
        Just(EvidenceVerdict::Reject),
    ]
}

fn arb_drift_snapshot() -> impl Strategy<Value = DriftSnapshot> {
    (
        0.0..100.0f64,
        0.0..1.0f64,
        0..1000usize,
        0..500usize,
        0..500usize,
        0..10usize,
        any::<bool>(),
    )
        .prop_map(
            |(e_value, null_rate, total_obs, post_succ, post_obs, drift_count, calibrated)| {
                DriftSnapshot {
                    e_value,
                    null_rate,
                    total_observations: total_obs,
                    post_cal_successes: post_succ,
                    post_cal_observations: post_obs,
                    drift_count,
                    calibrated,
                    config: EValueConfig::default(),
                }
            },
        )
}

fn arb_evidence_summary() -> impl Strategy<Value = EvidenceSummary> {
    (
        0..50usize,
        any::<bool>(),
        arb_verdict(),
        "[a-f0-9]{8,16}",
    )
        .prop_map(|(entry_count, is_complete, verdict, root_hash)| EvidenceSummary {
            entry_count,
            is_complete,
            overall_verdict: verdict,
            root_hash,
            categories: vec!["ChangeDetection".to_string()],
        })
}

fn arb_record(id: u64) -> impl Strategy<Value = ReflexRecord> {
    (
        arb_maturity(),
        arb_status(),
        arb_drift_snapshot(),
        arb_evidence_summary(),
        0..100u64,
        0..50u64,
        0..20u64,
        "[a-z]{3,8}",
    )
        .prop_map(
            move |(tier, status, drift, evidence, succ, fail, consec, cluster)| ReflexRecord {
                reflex_id: id,
                cluster_id: cluster,
                version: 1,
                trigger_key: vec![1, 2, 3],
                commands: vec!["cmd".to_string()],
                status,
                tier,
                successes: succ,
                failures: fail,
                consecutive_failures: consec,
                drift_state: drift,
                evidence_summary: evidence,
                parent_reflex_id: None,
                parent_version: None,
                created_at_ms: 0,
                updated_at_ms: 1000,
            },
        )
}

fn arb_manifest_entry(id: u64) -> impl Strategy<Value = ManifestEntry> {
    (arb_maturity(), arb_status(), arb_verdict(), 0.0..100.0f64).prop_map(
        move |(tier, status, verdict, e_value)| ManifestEntry {
            reflex_id: id,
            cluster_id: "c1".to_string(),
            version: 1,
            status,
            tier,
            successes: 10,
            failures: 0,
            e_value,
            is_drifted: false,
            evidence_verdict: verdict,
            created_at_ms: 0,
            updated_at_ms: 1000,
        },
    )
}

// =============================================================================
// Manifest invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn manifest_len_matches_entries(n in 1..20u64) {
        let mut m = ReflexManifest::new();
        for i in 0..n {
            m.upsert(ManifestEntry {
                reflex_id: i,
                cluster_id: "c".to_string(),
                version: 1,
                status: VersionStatus::Active,
                tier: MaturityTier::Incubating,
                successes: 0, failures: 0, e_value: 1.0,
                is_drifted: false,
                evidence_verdict: EvidenceVerdict::Neutral,
                created_at_ms: 0, updated_at_ms: 0,
            });
        }
        prop_assert_eq!(m.len(), n as usize);
        prop_assert_eq!(m.ids().len(), n as usize);
    }

    #[test]
    fn manifest_upsert_overwrites(entry in arb_manifest_entry(42)) {
        let mut m = ReflexManifest::new();
        m.upsert(entry.clone());
        m.upsert(entry);
        prop_assert_eq!(m.len(), 1);
    }

    #[test]
    fn manifest_remove_reduces_len(n in 2..10u64) {
        let mut m = ReflexManifest::new();
        for i in 0..n {
            m.upsert(ManifestEntry {
                reflex_id: i,
                cluster_id: "c".to_string(),
                version: 1,
                status: VersionStatus::Active,
                tier: MaturityTier::Incubating,
                successes: 0, failures: 0, e_value: 1.0,
                is_drifted: false,
                evidence_verdict: EvidenceVerdict::Neutral,
                created_at_ms: 0, updated_at_ms: 0,
            });
        }
        m.remove(0);
        prop_assert_eq!(m.len(), (n - 1) as usize);
        prop_assert!(m.get(0).is_none());
    }

    #[test]
    fn manifest_serde_roundtrip(n in 1..10u64) {
        let mut m = ReflexManifest::new();
        for i in 0..n {
            m.upsert(ManifestEntry {
                reflex_id: i,
                cluster_id: format!("c{}", i),
                version: 1,
                status: VersionStatus::Active,
                tier: MaturityTier::Incubating,
                successes: i, failures: 0, e_value: 1.0,
                is_drifted: false,
                evidence_verdict: EvidenceVerdict::Neutral,
                created_at_ms: 0, updated_at_ms: 0,
            });
        }
        let json = serde_json::to_string(&m).unwrap();
        let decoded: ReflexManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.len(), m.len());
        for i in 0..n {
            prop_assert!(decoded.get(i).is_some());
        }
    }
}

// =============================================================================
// Store invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn store_upsert_consistent(record in arb_record(1)) {
        let mut store = ReflexStore::new();
        store.upsert(record.clone());
        prop_assert_eq!(store.len(), 1);
        let got = store.get(1).unwrap();
        prop_assert_eq!(got.reflex_id, 1);
        let entry = store.get_entry(1).unwrap();
        prop_assert_eq!(entry.reflex_id, 1);
    }

    #[test]
    fn store_remove_consistent(record in arb_record(7)) {
        let mut store = ReflexStore::new();
        store.upsert(record);
        prop_assert_eq!(store.len(), 1);
        let removed = store.remove(7);
        prop_assert!(removed.is_some());
        prop_assert_eq!(store.len(), 0);
        prop_assert!(store.get(7).is_none());
        prop_assert!(store.get_entry(7).is_none());
    }

    #[test]
    fn store_blacklist_removes_and_blocks(record in arb_record(3)) {
        let mut store = ReflexStore::new();
        store.upsert(record);
        store.blacklist(BlacklistEntry {
            reflex_id: 3,
            cluster_id: "c".to_string(),
            reason: BlacklistReason::OperatorBan { note: "test".to_string() },
            blacklisted_at_ms: 5000,
            final_e_value: 0.0,
            final_failures: 0,
        });
        prop_assert!(store.is_blacklisted(3));
        prop_assert_eq!(store.len(), 0);
        prop_assert!(store.get(3).is_none());
    }

    #[test]
    fn store_dirty_after_upsert(record in arb_record(1)) {
        let mut store = ReflexStore::new();
        prop_assert!(!store.is_dirty());
        store.upsert(record);
        prop_assert!(store.is_dirty());
        store.mark_clean();
        prop_assert!(!store.is_dirty());
    }
}

// =============================================================================
// Prune invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prune_never_removes_healthy(
        n in 1..5u64,
    ) {
        let mut store = ReflexStore::new();
        for i in 0..n {
            let r = ReflexRecord {
                reflex_id: i,
                cluster_id: "c".to_string(),
                version: 1,
                trigger_key: vec![1],
                commands: vec!["cmd".into()],
                status: VersionStatus::Active,
                tier: MaturityTier::Graduated,
                successes: 100,
                failures: 0,
                consecutive_failures: 0,
                drift_state: DriftSnapshot {
                    e_value: 50.0,
                    null_rate: 0.9,
                    total_observations: 100,
                    post_cal_successes: 90,
                    post_cal_observations: 100,
                    drift_count: 0,
                    calibrated: true,
                    config: EValueConfig::default(),
                },
                evidence_summary: EvidenceSummary {
                    entry_count: 3, is_complete: true,
                    overall_verdict: EvidenceVerdict::Support,
                    root_hash: "h".to_string(), categories: vec![],
                },
                parent_reflex_id: None, parent_version: None,
                created_at_ms: 0, updated_at_ms: 1000,
            };
            store.upsert(r);
        }
        let mut engine = PruneEngine::with_defaults();
        let result = store.prune(&mut engine, 10_000_000);
        prop_assert!(result.is_empty());
        prop_assert_eq!(store.len(), n as usize);
    }

    #[test]
    fn prune_e_value_collapse_detected(
        e_val in 0.0..0.009f64,
    ) {
        let mut store = ReflexStore::new();
        let r = ReflexRecord {
            reflex_id: 1,
            cluster_id: "c".to_string(),
            version: 1,
            trigger_key: vec![1],
            commands: vec!["cmd".into()],
            status: VersionStatus::Active,
            tier: MaturityTier::Incubating,
            successes: 10, failures: 5, consecutive_failures: 0,
            drift_state: DriftSnapshot {
                e_value: e_val, null_rate: 0.5,
                total_observations: 50, post_cal_successes: 20,
                post_cal_observations: 50, drift_count: 1, calibrated: true,
                config: EValueConfig::default(),
            },
            evidence_summary: EvidenceSummary {
                entry_count: 1, is_complete: true,
                overall_verdict: EvidenceVerdict::Reject,
                root_hash: "h".to_string(), categories: vec![],
            },
            parent_reflex_id: None, parent_version: None,
            created_at_ms: 0, updated_at_ms: 1000,
        };
        store.upsert(r);
        let mut engine = PruneEngine::with_defaults();
        let result = store.prune(&mut engine, 10_000_000);
        prop_assert_eq!(result.e_value_pruned.len(), 1);
        prop_assert!(store.is_blacklisted(1));
    }

    #[test]
    fn prune_consecutive_failures_detected(
        failures in 10..50u64,
    ) {
        let mut store = ReflexStore::new();
        let r = ReflexRecord {
            reflex_id: 1,
            cluster_id: "c".to_string(),
            version: 1,
            trigger_key: vec![1],
            commands: vec!["cmd".into()],
            status: VersionStatus::Active,
            tier: MaturityTier::Incubating,
            successes: 0, failures, consecutive_failures: failures,
            drift_state: DriftSnapshot {
                e_value: 5.0, null_rate: 0.5,
                total_observations: 50, post_cal_successes: 20,
                post_cal_observations: 50, drift_count: 0, calibrated: true,
                config: EValueConfig::default(),
            },
            evidence_summary: EvidenceSummary {
                entry_count: 1, is_complete: true,
                overall_verdict: EvidenceVerdict::Neutral,
                root_hash: "h".to_string(), categories: vec![],
            },
            parent_reflex_id: None, parent_version: None,
            created_at_ms: 0, updated_at_ms: 1000,
        };
        store.upsert(r);
        let mut engine = PruneEngine::with_defaults();
        let result = store.prune(&mut engine, 10_000_000);
        prop_assert_eq!(result.failure_pruned.len(), 1);
    }

    #[test]
    fn prune_total_is_sum_of_categories(
        e_val_n in 0..3u64,
        fail_n in 0..3u64,
    ) {
        let result = PruneResult {
            e_value_pruned: (0..e_val_n).collect(),
            failure_pruned: (100..100 + fail_n).collect(),
            deprecated_pruned: vec![],
            blacklisted: vec![],
        };
        prop_assert_eq!(
            result.total_pruned(),
            (e_val_n + fail_n) as usize
        );
    }

    #[test]
    fn prune_stats_runs_increment(n in 1..5u32) {
        let mut store = ReflexStore::new();
        // Add healthy reflexes.
        store.upsert(ReflexRecord {
            reflex_id: 1, cluster_id: "c".to_string(), version: 1,
            trigger_key: vec![1], commands: vec!["cmd".into()],
            status: VersionStatus::Active, tier: MaturityTier::Graduated,
            successes: 100, failures: 0, consecutive_failures: 0,
            drift_state: DriftSnapshot {
                e_value: 50.0, null_rate: 0.9, total_observations: 100,
                post_cal_successes: 90, post_cal_observations: 100,
                drift_count: 0, calibrated: true, config: EValueConfig::default(),
            },
            evidence_summary: EvidenceSummary {
                entry_count: 3, is_complete: true,
                overall_verdict: EvidenceVerdict::Support,
                root_hash: "h".to_string(), categories: vec![],
            },
            parent_reflex_id: None, parent_version: None,
            created_at_ms: 0, updated_at_ms: 1000,
        });
        let mut engine = PruneEngine::with_defaults();
        for _ in 0..n {
            store.prune(&mut engine, 10_000_000);
        }
        prop_assert_eq!(engine.stats().total_prune_runs, n as u64);
    }
}

// =============================================================================
// Blacklist invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn blacklist_entry_serde_roundtrip(
        id in 0..1000u64,
        cluster in "[a-z]{3,8}",
        ts in 0..u64::MAX,
        e_val in 0.0..100.0f64,
        fails in 0..100u64,
    ) {
        let entry = BlacklistEntry {
            reflex_id: id,
            cluster_id: cluster,
            reason: BlacklistReason::EValueCollapse { final_e_value: e_val },
            blacklisted_at_ms: ts,
            final_e_value: e_val,
            final_failures: fails,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: BlacklistEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.reflex_id, entry.reflex_id);
        prop_assert_eq!(decoded.final_failures, entry.final_failures);
    }

    #[test]
    fn record_serde_roundtrip(record in arb_record(42)) {
        let json = serde_json::to_string(&record).unwrap();
        let decoded: ReflexRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.reflex_id, 42);
        prop_assert_eq!(decoded.version, record.version);
        prop_assert_eq!(decoded.successes, record.successes);
    }

    #[test]
    fn prune_config_serde_roundtrip(
        threshold in 0.001..0.1f64,
        max_fail in 5..50u64,
    ) {
        let config = PruneConfig {
            e_value_collapse_threshold: threshold,
            max_consecutive_failures: max_fail,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: PruneConfig = serde_json::from_str(&json).unwrap();
        let diff = (decoded.e_value_collapse_threshold - threshold).abs();
        prop_assert!(diff < 1e-10);
        prop_assert_eq!(decoded.max_consecutive_failures, max_fail);
    }

    #[test]
    fn prune_stats_serde_roundtrip(
        runs in 0..1000u64,
        pruned in 0..500u64,
        bl in 0..100u64,
    ) {
        let stats = PruneStats {
            total_prune_runs: runs,
            total_pruned: pruned,
            total_blacklisted: bl,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: PruneStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, stats);
    }
}
