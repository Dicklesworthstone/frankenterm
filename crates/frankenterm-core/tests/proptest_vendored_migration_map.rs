//! Property-based tests for vendored_migration_map.
//!
//! Covers:
//! - Inventory structural invariants
//! - Migration wave ordering and dependency consistency
//! - Serde roundtrip for all types
//! - Query API correctness
//! - Canonical string determinism
//! - Feature gate consistency
//! - Dependency graph acyclicity

use frankenterm_core::vendored_migration_map::{
    build_canonical_map, Criticality, MigrationDifficulty, MigrationWave, RuntimePrimitive,
    VendoredCrateId, VendoredMigrationMap,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_difficulty() -> impl Strategy<Value = MigrationDifficulty> {
    prop_oneof![
        Just(MigrationDifficulty::AlreadyCompat),
        Just(MigrationDifficulty::Low),
        Just(MigrationDifficulty::Medium),
        Just(MigrationDifficulty::High),
    ]
}

fn arb_criticality() -> impl Strategy<Value = Criticality> {
    prop_oneof![
        Just(Criticality::None),
        Just(Criticality::Low),
        Just(Criticality::Medium),
        Just(Criticality::High),
    ]
}

fn arb_wave() -> impl Strategy<Value = MigrationWave> {
    prop_oneof![
        Just(MigrationWave::Wave0AlreadyCompat),
        Just(MigrationWave::Wave1Codec),
        Just(MigrationWave::Wave2Ssh),
        Just(MigrationWave::Wave3ConfigScripting),
        Just(MigrationWave::Wave4Mux),
        Just(MigrationWave::Wave5DevOnly),
        Just(MigrationWave::NotApplicable),
    ]
}

fn arb_runtime_primitive() -> impl Strategy<Value = RuntimePrimitive> {
    prop_oneof![
        Just(RuntimePrimitive::Smol),
        Just(RuntimePrimitive::Asupersync),
        Just(RuntimePrimitive::AsyncIo),
        Just(RuntimePrimitive::AsyncExecutor),
        Just(RuntimePrimitive::FuturesTraits),
        Just(RuntimePrimitive::Flume),
    ]
}

fn arb_crate_id() -> impl Strategy<Value = VendoredCrateId> {
    "[a-z][a-z0-9_-]{1,15}".prop_map(VendoredCrateId::new)
}

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn difficulty_score_preserves_order(a in arb_difficulty(), b in arb_difficulty()) {
        if a < b {
            prop_assert!(a.score() < b.score());
        } else if a == b {
            prop_assert_eq!(a.score(), b.score());
        }
    }

    #[test]
    fn criticality_score_preserves_order(a in arb_criticality(), b in arb_criticality()) {
        if a < b {
            prop_assert!(a.score() < b.score());
        } else if a == b {
            prop_assert_eq!(a.score(), b.score());
        }
    }

    #[test]
    fn wave_ordinal_preserves_order(a in arb_wave(), b in arb_wave()) {
        if a < b {
            prop_assert!(a.ordinal() < b.ordinal());
        } else if a == b {
            prop_assert_eq!(a.ordinal(), b.ordinal());
        }
    }

    #[test]
    fn difficulty_serde_roundtrip(d in arb_difficulty()) {
        let json = serde_json::to_string(&d).unwrap();
        let restored: MigrationDifficulty = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, restored);
    }

    #[test]
    fn criticality_serde_roundtrip(c in arb_criticality()) {
        let json = serde_json::to_string(&c).unwrap();
        let restored: Criticality = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c, restored);
    }

    #[test]
    fn wave_serde_roundtrip(w in arb_wave()) {
        let json = serde_json::to_string(&w).unwrap();
        let restored: MigrationWave = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(w, restored);
    }

    #[test]
    fn runtime_primitive_serde_roundtrip(p in arb_runtime_primitive()) {
        let json = serde_json::to_string(&p).unwrap();
        let restored: RuntimePrimitive = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(p, restored);
    }

    #[test]
    fn crate_id_display_roundtrip(name in "[a-z][a-z0-9_-]{1,15}") {
        let id = VendoredCrateId::new(&name);
        prop_assert_eq!(id.to_string(), name.clone());
        prop_assert_eq!(id.as_str(), name.as_str());
    }

    #[test]
    fn canonical_map_wave_coverage(wave in arb_wave()) {
        let map = build_canonical_map();
        let crates = map.wave_crates(wave);
        // All returned crates must be in the requested wave
        for entry in &crates {
            prop_assert_eq!(entry.wave, wave);
        }
    }

    #[test]
    fn canonical_map_entry_invariants(idx in 0usize..9) {
        let map = build_canonical_map();
        let entries: Vec<_> = map.entries.values().collect();
        if idx < entries.len() {
            let entry = entries[idx];
            // total_async_refs >= smol + asupersync
            prop_assert!(
                entry.total_async_refs >= entry.total_smol_refs + entry.total_asupersync_refs,
                "{}: total {} < smol {} + asupersync {}",
                entry.crate_id,
                entry.total_async_refs,
                entry.total_smol_refs,
                entry.total_asupersync_refs
            );
        }
    }

    #[test]
    fn canonical_map_serde_roundtrip(seed in 0u64..100) {
        // seed unused but forces proptest to run multiple times
        let _ = seed;
        let map = build_canonical_map();
        let json = serde_json::to_string_pretty(&map).unwrap();
        let restored: VendoredMigrationMap = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(map.total_vendored_crates, restored.total_vendored_crates);
        prop_assert_eq!(map.async_vendored_crates, restored.async_vendored_crates);
        prop_assert_eq!(map.global_smol_refs, restored.global_smol_refs);
        prop_assert_eq!(map.entries.len(), restored.entries.len());
    }

    #[test]
    fn canonical_string_deterministic(seed in 0u64..100) {
        let _ = seed;
        let map = build_canonical_map();
        let s1 = map.canonical_string();
        let s2 = map.canonical_string();
        prop_assert_eq!(s1, s2);
    }

    #[test]
    fn migration_order_is_sorted(seed in 0u64..100) {
        let _ = seed;
        let map = build_canonical_map();
        let order = map.migration_order();
        for pair in order.windows(2) {
            let a_key = (pair[0].wave.ordinal(), pair[0].difficulty.score());
            let b_key = (pair[1].wave.ordinal(), pair[1].difficulty.score());
            prop_assert!(
                a_key <= b_key,
                "{} {:?} should come before {} {:?}",
                pair[0].crate_id,
                a_key,
                pair[1].crate_id,
                b_key
            );
        }
    }
}

// ── Non-proptest structural tests ──────────────────────────────────────────

#[test]
fn wave0_all_have_asupersync_gate() {
    let map = build_canonical_map();
    for entry in map.wave_crates(MigrationWave::Wave0AlreadyCompat) {
        assert!(
            entry.feature_gates.has_async_asupersync,
            "{} in Wave0 must have async-asupersync",
            entry.crate_id
        );
    }
}

#[test]
fn no_entry_depends_on_itself() {
    let map = build_canonical_map();
    for entry in map.entries.values() {
        assert!(
            !entry.depends_on.contains(&entry.crate_id),
            "{} depends on itself",
            entry.crate_id
        );
    }
}

#[test]
fn all_dependencies_exist_in_map() {
    let map = build_canonical_map();
    for entry in map.entries.values() {
        for dep in &entry.depends_on {
            assert!(
                map.entries.contains_key(dep),
                "{} depends on {} which is not in the map",
                entry.crate_id,
                dep
            );
        }
    }
}

#[test]
fn dependency_wave_ordering() {
    // A crate's dependencies must be in the same or earlier wave
    let map = build_canonical_map();
    for entry in map.entries.values() {
        for dep in &entry.depends_on {
            if let Some(dep_entry) = map.entries.get(dep) {
                assert!(
                    dep_entry.wave.ordinal() <= entry.wave.ordinal(),
                    "{} (wave {}) depends on {} (wave {})",
                    entry.crate_id,
                    entry.wave.ordinal(),
                    dep,
                    dep_entry.wave.ordinal()
                );
            }
        }
    }
}

#[test]
fn ssh_depends_on_wave0_crates() {
    let map = build_canonical_map();
    let ssh = map.get("ssh").unwrap();
    assert!(ssh.depends_on.contains(&VendoredCrateId::new("async_ossl")));
    assert!(ssh.depends_on.contains(&VendoredCrateId::new("uds")));
    assert!(ssh.depends_on.contains(&VendoredCrateId::new("codec")));
}

#[test]
fn total_refs_match_inventory() {
    let map = build_canonical_map();
    // From asupersync-runtime-inventory.json: vendored smol = 68, asupersync = 7
    assert_eq!(map.global_smol_refs, 68);
    assert_eq!(map.global_asupersync_refs, 7);
}
