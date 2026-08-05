//! Property-based tests for shard assignment, id encoding, and health reporting.

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::circuit_breaker::CircuitBreakerStatus;
use frankenterm_core::config::{Config, VendoredShardingConfig};
use frankenterm_core::patterns::AgentType;
use frankenterm_core::sharding::{
    AssignmentStrategy, LOCAL_PANE_ID_BITS, LOCAL_PANE_ID_MASK, MAX_CONFIGURED_SHARDS,
    MAX_GLOBAL_PANE_ID, MAX_SHARD_ID, ShardBackendErrorClass, ShardHealthEntry,
    ShardHealthProbeOutcome, ShardHealthReport, ShardHealthReportOutcome, ShardId,
    assign_pane_with_strategy, decode_sharded_pane_id, encode_sharded_pane_id,
    is_sharded_pane_id, try_decode_sharded_pane_id, try_encode_sharded_pane_id,
};
use frankenterm_core::watchdog::HealthStatus;

// =========================================================================
// Strategies
// =========================================================================

fn arb_shard_count() -> impl Strategy<Value = usize> {
    1usize..=16
}

fn arb_shards() -> impl Strategy<Value = Vec<ShardId>> {
    arb_shard_count().prop_map(|count| (0..count).map(ShardId).collect())
}

fn arb_assignment_strategy_for_shard_count(
    shard_count: usize,
) -> impl Strategy<Value = AssignmentStrategy> {
    prop_oneof![
        Just(AssignmentStrategy::RoundRobin),
        (1u32..256).prop_map(|virtual_nodes| AssignmentStrategy::ConsistentHash { virtual_nodes }),
        (
            prop::collection::vec(("[a-z]{1,8}", 0usize..shard_count), 0..10),
            prop::option::of(0usize..shard_count),
        )
            .prop_map(
                |(domain_pairs, default_shard)| AssignmentStrategy::ByDomain {
                    domain_to_shard: domain_pairs
                        .into_iter()
                        .map(|(domain, shard)| (domain, ShardId(shard)))
                        .collect(),
                    default_shard: default_shard.map(ShardId),
                }
            ),
        (
            prop::collection::vec((arb_agent_type(), 0usize..shard_count), 0..10),
            prop::option::of(0usize..shard_count),
        )
            .prop_map(
                |(agent_pairs, default_shard)| AssignmentStrategy::ByAgentType {
                    agent_to_shard: agent_pairs
                        .into_iter()
                        .map(|(agent, shard)| (agent, ShardId(shard)))
                        .collect(),
                    default_shard: default_shard.map(ShardId),
                }
            ),
        (
            prop::collection::vec((any::<u64>(), 0usize..shard_count), 0..12),
            prop::option::of(0usize..shard_count),
        )
            .prop_map(|(manual_pairs, default_shard)| AssignmentStrategy::Manual {
                pane_to_shard: manual_pairs
                    .into_iter()
                    .map(|(pane_id, shard)| (pane_id, ShardId(shard)))
                    .collect(),
                default_shard: default_shard.map(ShardId),
            }),
    ]
}

fn arb_agent_type() -> impl Strategy<Value = AgentType> {
    prop_oneof![
        Just(AgentType::Codex),
        Just(AgentType::ClaudeCode),
        Just(AgentType::Gemini),
        Just(AgentType::Wezterm),
        Just(AgentType::Unknown),
    ]
}

fn arb_health_status() -> impl Strategy<Value = HealthStatus> {
    prop_oneof![
        Just(HealthStatus::Healthy),
        Just(HealthStatus::Degraded),
        Just(HealthStatus::Critical),
        Just(HealthStatus::Hung),
    ]
}

fn arb_circuit_state_kind()
-> impl Strategy<Value = frankenterm_core::circuit_breaker::CircuitStateKind> {
    prop_oneof![
        Just(frankenterm_core::circuit_breaker::CircuitStateKind::Closed),
        Just(frankenterm_core::circuit_breaker::CircuitStateKind::Open),
        Just(frankenterm_core::circuit_breaker::CircuitStateKind::HalfOpen),
    ]
}

fn arb_circuit_breaker_status() -> impl Strategy<Value = CircuitBreakerStatus> {
    (
        arb_circuit_state_kind(),
        0u32..100,                          // consecutive_failures
        1u32..20,                           // failure_threshold
        1u32..10,                           // success_threshold
        1000u64..60_000,                    // open_cooldown_ms
        proptest::option::of(0u64..60_000), // open_for_ms
        proptest::option::of(0u64..60_000), // cooldown_remaining_ms
        proptest::option::of(0u32..10),     // half_open_successes
    )
        .prop_map(
            |(state, cf, ft, st, ocms, ofms, crms, hos)| CircuitBreakerStatus {
                state,
                consecutive_failures: cf,
                failure_threshold: ft,
                success_threshold: st,
                open_cooldown_ms: ocms,
                open_for_ms: ofms,
                cooldown_remaining_ms: crms,
                half_open_successes: hos,
            },
        )
}

fn arb_backend_error_class() -> impl Strategy<Value = ShardBackendErrorClass> {
    prop_oneof![
        Just(ShardBackendErrorClass::Unavailable),
        Just(ShardBackendErrorClass::PaneNotFound),
        Just(ShardBackendErrorClass::CommandFailed),
        Just(ShardBackendErrorClass::InvalidResponse),
        Just(ShardBackendErrorClass::OutputTooLarge),
        Just(ShardBackendErrorClass::TimedOut),
        Just(ShardBackendErrorClass::CircuitOpen),
        Just(ShardBackendErrorClass::Cancelled),
        Just(ShardBackendErrorClass::Panicked),
        Just(ShardBackendErrorClass::Io),
        Just(ShardBackendErrorClass::Other),
    ]
}

fn arb_probe_outcome() -> impl Strategy<Value = ShardHealthProbeOutcome> {
    prop_oneof![
        Just(ShardHealthProbeOutcome::Complete),
        arb_backend_error_class().prop_map(ShardHealthProbeOutcome::Failed),
        Just(ShardHealthProbeOutcome::Cancelled),
        Just(ShardHealthProbeOutcome::NotStarted),
    ]
}

fn arb_shard_health_entry() -> impl Strategy<Value = ShardHealthEntry> {
    (
        0usize..100, // shard_id
        arb_health_status(),
        proptest::option::of(0usize..1000), // pane_count
        arb_circuit_breaker_status(),
        arb_probe_outcome(),
    )
        .prop_map(
            |(shard_id, mut status, mut pane_count, circuit, probe_outcome)| {
                if probe_outcome != ShardHealthProbeOutcome::Complete
                    && status == HealthStatus::Healthy
                {
                    status = HealthStatus::Degraded;
                }
                if matches!(
                    probe_outcome,
                    ShardHealthProbeOutcome::Failed(_) | ShardHealthProbeOutcome::NotStarted
                ) {
                    pane_count = None;
                } else if probe_outcome == ShardHealthProbeOutcome::Complete
                    && pane_count.is_none()
                {
                    pane_count = Some(0);
                }
                ShardHealthEntry {
                    shard_id: ShardId(shard_id),
                    status,
                    pane_count,
                    circuit,
                    probe_outcome,
                }
            },
        )
}

fn arb_shard_health_report() -> impl Strategy<Value = ShardHealthReport> {
    (
        0u64..2_000_000_000,
        prop::collection::vec(arb_shard_health_entry(), 0..80),
    )
        .prop_map(|(timestamp_ms, mut shards)| {
            for (index, entry) in shards.iter_mut().enumerate() {
                entry.shard_id = ShardId(index);
            }
            let overall = shards
                .iter()
                .fold(HealthStatus::Healthy, |worst, entry| {
                    worst.max(entry.status)
                });
            ShardHealthReport {
                timestamp_ms,
                overall,
                shards,
            }
        })
}

// =========================================================================
// Encode/decode roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn prop_encode_decode_roundtrip(
        shard in 0usize..=MAX_SHARD_ID,
        local in 0u64..=LOCAL_PANE_ID_MASK,
    ) {
        let encoded = encode_sharded_pane_id(ShardId(shard), local);
        let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
        prop_assert_eq!(decoded_shard, ShardId(shard));
        prop_assert_eq!(decoded_local, local);
        prop_assert!(encoded <= MAX_GLOBAL_PANE_ID);
        prop_assert!(i64::try_from(encoded).is_ok());
    }

    /// is_sharded_pane_id is consistent with encode for non-zero shards.
    #[test]
    fn prop_is_sharded_consistent_with_encode(
        shard in 1usize..=MAX_SHARD_ID,
        local in 0u64..=LOCAL_PANE_ID_MASK,
    ) {
        let encoded = encode_sharded_pane_id(ShardId(shard), local);
        prop_assert!(
            is_sharded_pane_id(encoded),
            "encoded pane with shard {} should be detected as sharded",
            shard,
        );
    }

    /// Shard 0 encodes produce non-sharded IDs (shard bits are zero).
    #[test]
    fn prop_shard_zero_not_sharded(local in 0u64..=LOCAL_PANE_ID_MASK) {
        let encoded = encode_sharded_pane_id(ShardId(0), local);
        prop_assert!(
            !is_sharded_pane_id(encoded),
            "encoded pane with shard 0 should not be detected as sharded",
        );
    }

    /// Different shard+local pairs produce different encoded values.
    #[test]
    fn prop_encode_unique(
        shard1 in 0usize..=MAX_SHARD_ID,
        shard2 in 0usize..=MAX_SHARD_ID,
        local1 in 0u64..=LOCAL_PANE_ID_MASK,
        local2 in 0u64..=LOCAL_PANE_ID_MASK,
    ) {
        prop_assume!(shard1 != shard2 || local1 != local2);
        let enc1 = encode_sharded_pane_id(ShardId(shard1), local1);
        let enc2 = encode_sharded_pane_id(ShardId(shard2), local2);
        prop_assert_ne!(enc1, enc2, "distinct shard+local should produce distinct encoded IDs");
    }
}

#[test]
fn health_probe_wire_uses_typed_finite_outcomes_only() {
    let cases = [
        ShardHealthProbeOutcome::Complete,
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Unavailable),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::PaneNotFound),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::CommandFailed),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::InvalidResponse),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::OutputTooLarge),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::TimedOut),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::CircuitOpen),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Cancelled),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Panicked),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Io),
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other),
        ShardHealthProbeOutcome::Cancelled,
        ShardHealthProbeOutcome::NotStarted,
    ];

    for (index, expected) in cases.into_iter().enumerate() {
        let entry = ShardHealthEntry {
            shard_id: ShardId(index),
            status: match expected {
                ShardHealthProbeOutcome::Complete => HealthStatus::Healthy,
                ShardHealthProbeOutcome::Failed(_) => HealthStatus::Hung,
                ShardHealthProbeOutcome::Cancelled | ShardHealthProbeOutcome::NotStarted => {
                    HealthStatus::Degraded
                }
            },
            pane_count: (expected == ShardHealthProbeOutcome::Complete).then_some(0),
            circuit: CircuitBreakerStatus::default(),
            probe_outcome: expected,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("\"label\""));
        assert!(!json.contains("\"error\""));
        let back: ShardHealthEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.probe_outcome, expected);
    }
}

#[test]
fn configured_shard_capacity_matches_the_encoded_id_domain() {
    assert_eq!(MAX_CONFIGURED_SHARDS, MAX_SHARD_ID + 1);
}

// =========================================================================
// Assignment completeness
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(220))]

    #[test]
    fn prop_assignment_completeness(
        shards in arb_shards(),
        pane_ids in prop::collection::vec(any::<u64>(), 1..200),
        domain_pairs in prop::collection::vec(("[a-z]{1,8}", 0usize..20), 0..20),
        manual_pairs in prop::collection::vec((any::<u64>(), 0usize..20), 0..20),
        default in prop::option::of(0usize..20),
    ) {
        let pane_to_shard = manual_pairs
            .into_iter()
            .map(|(pane_id, raw)| (pane_id, ShardId(raw)))
            .collect::<HashMap<_, _>>();

        let strategy = AssignmentStrategy::Manual {
            pane_to_shard,
            default_shard: default.map(ShardId),
        };

        for pane_id in pane_ids {
            let domain = domain_pairs.first().map(|(d, _)| d.as_str());
            let shard = assign_pane_with_strategy(
                &strategy,
                &shards,
                pane_id,
                domain,
                Some(AgentType::Unknown),
            );
            prop_assert!(
                shards.contains(&shard),
                "assigned shard {:?} not in available set {:?}",
                shard,
                shards
            );
        }
    }
}

// =========================================================================
// Consistent hash properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(180))]

    #[test]
    fn prop_consistent_hash_minimal_disruption(
        pane_ids in prop::collection::vec(any::<u64>(), 120..800),
        base_nodes in 2usize..10,
        virtual_nodes in 16u32..256,
    ) {
        let base = (0..base_nodes).map(ShardId).collect::<Vec<_>>();
        let expanded = (0..=base_nodes).map(ShardId).collect::<Vec<_>>();

        let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes };

        let mut remapped = 0usize;
        for pane_id in &pane_ids {
            let old = assign_pane_with_strategy(&strategy, &base, *pane_id, None, None);
            let new = assign_pane_with_strategy(&strategy, &expanded, *pane_id, None, None);
            if old != new {
                remapped += 1;
            }
        }

        // Adding one node should remap at most ~2/N keys where N is original
        // shard count. The theoretical bound is 1/N for perfect consistent
        // hashing, but with finite virtual_nodes (as low as 16) the ring
        // distribution is imperfect, so we allow 2x headroom.
        let max_allowed = 2 * pane_ids.len().div_ceil(base_nodes);
        prop_assert!(
            remapped <= max_allowed,
            "remapped {remapped}/{} keys (max allowed {max_allowed}) when expanding {base_nodes} -> {} shards",
            pane_ids.len(),
            base_nodes + 1
        );
    }

    /// Consistent hash is deterministic: same inputs → same shard.
    #[test]
    fn prop_consistent_hash_deterministic(
        pane_id in any::<u64>(),
        shard_count in 2usize..10,
        virtual_nodes in 16u32..256,
    ) {
        let shards: Vec<ShardId> = (0..shard_count).map(ShardId).collect();
        let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes };
        let s1 = assign_pane_with_strategy(&strategy, &shards, pane_id, None, None);
        let s2 = assign_pane_with_strategy(&strategy, &shards, pane_id, None, None);
        prop_assert_eq!(s1, s2, "consistent hash should be deterministic");
    }
}

// =========================================================================
// Sharding config roundtrip and validation
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(120))]

    #[test]
    fn prop_vendored_sharding_config_roundtrip_and_validate(
        input in (2usize..16).prop_flat_map(|shard_count| {
            (
                Just(shard_count),
                arb_assignment_strategy_for_shard_count(shard_count),
            )
        }),
    ) {
        let (shard_count, assignment) = input;
        let socket_paths = (0..shard_count)
            .map(|idx| format!("/tmp/ft-prop-shard-{idx}.sock"))
            .collect::<Vec<_>>();

        let sharding = VendoredShardingConfig {
            enabled: true,
            socket_paths: socket_paths.clone(),
            assignment: assignment.clone(),
        };

        let encoded = serde_json::to_string(&sharding).unwrap();
        let decoded: VendoredShardingConfig = serde_json::from_str(&encoded).unwrap();

        prop_assert!(decoded.enabled);
        prop_assert_eq!(&decoded.socket_paths, &socket_paths);
        prop_assert_eq!(&decoded.assignment, &assignment);

        let mut config = Config::default();
        config.vendored.sharding = decoded;
        prop_assert!(config.validate().is_ok());
    }
}

// =========================================================================
// RoundRobin / ByDomain / ByAgentType assignment
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// RoundRobin always assigns to a valid shard.
    #[test]
    fn prop_round_robin_valid(
        shards in arb_shards(),
        pane_id in any::<u64>(),
    ) {
        let strategy = AssignmentStrategy::RoundRobin;
        let shard = assign_pane_with_strategy(&strategy, &shards, pane_id, None, None);
        prop_assert!(shards.contains(&shard));
    }

    /// ByDomain always assigns to a valid shard.
    #[test]
    fn prop_by_domain_valid(
        shards in arb_shards(),
        pane_id in any::<u64>(),
        domain in "[a-z]{3,8}",
    ) {
        let mut domain_to_shard = HashMap::new();
        if let Some(first) = shards.first() {
            domain_to_shard.insert(domain.clone(), *first);
        }
        let strategy = AssignmentStrategy::ByDomain {
            domain_to_shard,
            default_shard: shards.first().copied(),
        };
        let shard = assign_pane_with_strategy(
            &strategy, &shards, pane_id, Some(&domain), None,
        );
        prop_assert!(shards.contains(&shard));
    }

    /// ByAgentType always assigns to a valid shard.
    #[test]
    fn prop_by_agent_type_valid(
        shards in arb_shards(),
        pane_id in any::<u64>(),
        agent in arb_agent_type(),
    ) {
        let mut agent_to_shard = HashMap::new();
        if let Some(first) = shards.first() {
            agent_to_shard.insert(agent, *first);
        }
        let strategy = AssignmentStrategy::ByAgentType {
            agent_to_shard,
            default_shard: shards.first().copied(),
        };
        let shard = assign_pane_with_strategy(
            &strategy, &shards, pane_id, None, Some(agent),
        );
        prop_assert!(shards.contains(&shard));
    }
}

// =========================================================================
// Strategy serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn prop_strategy_roundtrip_serialization(
        domain_pairs in prop::collection::vec(("[a-z]{1,8}", 0usize..8), 0..12),
        agent_pairs in prop::collection::vec((arb_agent_type(), 0usize..8), 0..8),
        manual_pairs in prop::collection::vec((any::<u64>(), 0usize..8), 0..12),
        default_domain in prop::option::of(0usize..8),
        default_agent in prop::option::of(0usize..8),
        default_manual in prop::option::of(0usize..8),
        vnodes in 1u32..200,
    ) {
        let by_domain = AssignmentStrategy::ByDomain {
            domain_to_shard: domain_pairs
                .iter()
                .map(|(domain, shard)| (domain.clone(), ShardId(*shard)))
                .collect(),
            default_shard: default_domain.map(ShardId),
        };
        let by_agent = AssignmentStrategy::ByAgentType {
            agent_to_shard: agent_pairs
                .iter()
                .map(|(agent, shard)| (*agent, ShardId(*shard)))
                .collect(),
            default_shard: default_agent.map(ShardId),
        };
        let manual = AssignmentStrategy::Manual {
            pane_to_shard: manual_pairs
                .iter()
                .map(|(pane_id, shard)| (*pane_id, ShardId(*shard)))
                .collect(),
            default_shard: default_manual.map(ShardId),
        };
        let consistent = AssignmentStrategy::ConsistentHash {
            virtual_nodes: vnodes,
        };

        let strategies = vec![
            by_domain,
            by_agent,
            manual,
            consistent,
            AssignmentStrategy::RoundRobin,
        ];
        for strategy in strategies {
            let encoded = serde_json::to_string(&strategy).unwrap();
            let decoded: AssignmentStrategy = serde_json::from_str(&encoded).unwrap();
            prop_assert_eq!(decoded, strategy);
        }
    }
}

// =========================================================================
// Health report serde and invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// ShardHealthEntry serde preserves its sole typed, finite authority.
    #[test]
    fn prop_health_entry_serde_roundtrip(entry in arb_shard_health_entry()) {
        let expected_outcome = entry.probe_outcome;
        let json = serde_json::to_string(&entry).unwrap();
        let projection: serde_json::Value = serde_json::from_str(&json).unwrap();
        let back: ShardHealthEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.shard_id, entry.shard_id);
        prop_assert_eq!(back.status, entry.status);
        prop_assert_eq!(back.pane_count, entry.pane_count);
        prop_assert_eq!(back.circuit.state, entry.circuit.state);
        prop_assert_eq!(back.probe_outcome, expected_outcome);
        prop_assert!(projection.get("label").is_none());
        prop_assert!(projection.get("error").is_none());
        prop_assert!(projection.get("probe_outcome").is_some());
    }

    /// ShardHealthReport serde roundtrip preserves its validated typed outcome
    /// and deterministic shard ordering.
    #[test]
    fn prop_health_report_serde_roundtrip(report in arb_shard_health_report()) {
        let expected_outcome = report.outcome();
        let json = serde_json::to_string(&report).unwrap();
        let projection: serde_json::Value = serde_json::from_str(&json).unwrap();
        let back: ShardHealthReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_ms, report.timestamp_ms);
        prop_assert_eq!(back.overall, report.overall);
        prop_assert_eq!(back.outcome(), expected_outcome);
        prop_assert_eq!(back.shards.len(), report.shards.len());
        for (b, r) in back.shards.iter().zip(report.shards.iter()) {
            prop_assert_eq!(b.shard_id, r.shard_id);
            prop_assert_eq!(b.status, r.status);
            prop_assert_eq!(b.probe_outcome, r.probe_outcome);
        }
        let expected_outcome_name = match expected_outcome {
            ShardHealthReportOutcome::Complete => "complete",
            ShardHealthReportOutcome::Cancelled => "cancelled",
        };
        prop_assert_eq!(projection["outcome"].as_str(), Some(expected_outcome_name));
    }

    /// unhealthy_shards returns only non-Healthy entries.
    #[test]
    fn prop_unhealthy_shards_filter(report in arb_shard_health_report()) {
        let unhealthy = report.unhealthy_shards();
        for entry in &unhealthy {
            prop_assert_ne!(entry.status, HealthStatus::Healthy,
                "unhealthy_shards should not include Healthy entries");
        }
        // Count manually
        let expected = report.shards.iter().filter(|e| e.status != HealthStatus::Healthy).count();
        prop_assert_eq!(unhealthy.len(), expected);
    }

    /// watchdog_warnings admits at most 64 entries plus one exact omission
    /// summary for larger unhealthy sets.
    #[test]
    fn prop_watchdog_warnings_count(report in arb_shard_health_report()) {
        const WARNING_LIMIT: usize = 64;
        let warnings = report.watchdog_warnings();
        let unhealthy = report.unhealthy_shards();
        let expected = unhealthy.len().min(WARNING_LIMIT)
            + usize::from(unhealthy.len() > WARNING_LIMIT);
        prop_assert_eq!(warnings.len(), expected);
        prop_assert!(warnings.iter().all(|warning| warning.len() < 256));
        if unhealthy.len() > WARNING_LIMIT {
            let omitted = unhealthy.len() - WARNING_LIMIT;
            let expected_suffix = format!(
                "omitted {omitted} additional unhealthy shard(s) after bounded limit \
                 {WARNING_LIMIT}"
            );
            prop_assert!(warnings
                .last()
                .is_some_and(|warning| warning.contains(&expected_suffix)));
        }
    }
}

// =========================================================================
// ShardId: Clone, Debug, Display, Ord
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// ShardId Clone produces identical value.
    #[test]
    fn prop_shard_id_clone(id in 0usize..10000) {
        let s = ShardId(id);
        let cloned = s;
        prop_assert_eq!(s, cloned);
    }

    /// ShardId Debug is non-empty.
    #[test]
    fn prop_shard_id_debug(id in 0usize..10000) {
        let s = ShardId(id);
        let debug = format!("{:?}", s);
        prop_assert!(!debug.is_empty());
    }

    /// ShardId Display contains the inner value.
    #[test]
    fn prop_shard_id_display(id in 0usize..10000) {
        let s = ShardId(id);
        let display = s.to_string();
        prop_assert!(display.contains(&id.to_string()));
    }

    /// ShardId ordering is consistent with inner usize.
    #[test]
    fn prop_shard_id_ordering(a in 0usize..10000, b in 0usize..10000) {
        let sa = ShardId(a);
        let sb = ShardId(b);
        prop_assert_eq!(sa.cmp(&sb), a.cmp(&b));
    }

    /// ShardId serde roundtrip preserves value.
    #[test]
    fn prop_shard_id_serde(id in 0usize..10000) {
        let s = ShardId(id);
        let json = serde_json::to_string(&s).unwrap();
        let back: ShardId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }
}

// =========================================================================
// Encode/decode additional properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Every in-domain pair encodes to a persistence-safe value.
    #[test]
    fn prop_encode_always_produces(
        shard in 0usize..=MAX_SHARD_ID,
        local in 0u64..=LOCAL_PANE_ID_MASK,
    ) {
        let encoded = try_encode_sharded_pane_id(ShardId(shard), local).unwrap();
        let (dec_shard, dec_local) = try_decode_sharded_pane_id(encoded).unwrap();
        prop_assert_eq!(dec_shard, ShardId(shard));
        prop_assert_eq!(dec_local, local);
        prop_assert!(encoded <= MAX_GLOBAL_PANE_ID);
    }

    /// Oversized local ids are rejected instead of aliases being created.
    #[test]
    fn prop_local_overflow_rejected(
        shard in 0usize..=MAX_SHARD_ID,
        overflow in 1u64..=(u64::MAX - LOCAL_PANE_ID_MASK),
    ) {
        let local = LOCAL_PANE_ID_MASK + overflow;
        prop_assert!(try_encode_sharded_pane_id(ShardId(shard), local).is_err());
    }

    /// The reserved sign-bit shard range is rejected at both codec boundaries.
    #[test]
    fn prop_reserved_sign_bit_rejected(local in 0u64..=LOCAL_PANE_ID_MASK) {
        let invalid_shard = MAX_SHARD_ID + 1;
        prop_assert!(try_encode_sharded_pane_id(ShardId(invalid_shard), local).is_err());
        let invalid_global = (invalid_shard as u64) << LOCAL_PANE_ID_BITS | local;
        prop_assert!(invalid_global > MAX_GLOBAL_PANE_ID);
        prop_assert!(try_decode_sharded_pane_id(invalid_global).is_err());
    }
}

// =========================================================================
// AssignmentStrategy: Default and Clone
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Default strategy is RoundRobin.
    #[test]
    fn prop_default_strategy_is_round_robin(_dummy in 0..1u8) {
        let strategy = AssignmentStrategy::default();
        prop_assert_eq!(strategy, AssignmentStrategy::RoundRobin);
    }

    /// RoundRobin serde roundtrip.
    #[test]
    fn prop_round_robin_serde(_dummy in 0..1u8) {
        let strategy = AssignmentStrategy::RoundRobin;
        let json = serde_json::to_string(&strategy).unwrap();
        let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(strategy, back);
    }

    /// ConsistentHash serde roundtrip.
    #[test]
    fn prop_consistent_hash_serde(vnodes in 1u32..1000) {
        let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: vnodes };
        let json = serde_json::to_string(&strategy).unwrap();
        let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(strategy, back);
    }
}

// =========================================================================
// Assignment: RoundRobin distributes across shards
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// RoundRobin with multiple panes touches multiple shards.
    #[test]
    fn prop_round_robin_distribution(
        shard_count in 2usize..8,
        pane_ids in prop::collection::vec(any::<u64>(), 20..100),
    ) {
        let shards: Vec<ShardId> = (0..shard_count).map(ShardId).collect();
        let strategy = AssignmentStrategy::RoundRobin;
        let mut seen = std::collections::HashSet::new();
        for pid in &pane_ids {
            let s = assign_pane_with_strategy(&strategy, &shards, *pid, None, None);
            seen.insert(s);
        }
        // With enough panes, should see more than 1 shard
        prop_assert!(seen.len() > 1 || shard_count == 1,
            "expected multiple shards used, got {} out of {}", seen.len(), shard_count);
    }

    /// ConsistentHash assigns to valid shards.
    #[test]
    fn prop_consistent_hash_valid(
        shard_count in 2usize..10,
        pane_id in any::<u64>(),
        vnodes in 16u32..256,
    ) {
        let shards: Vec<ShardId> = (0..shard_count).map(ShardId).collect();
        let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: vnodes };
        let s = assign_pane_with_strategy(&strategy, &shards, pane_id, None, None);
        prop_assert!(shards.contains(&s));
    }
}

// =========================================================================
// Health report: Clone, Debug, empty report
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Health report Clone preserves all fields.
    #[test]
    fn prop_health_report_clone(report in arb_shard_health_report()) {
        let cloned = report.clone();
        prop_assert_eq!(cloned.timestamp_ms, report.timestamp_ms);
        prop_assert_eq!(cloned.overall, report.overall);
        prop_assert_eq!(cloned.shards.len(), report.shards.len());
    }

    /// Health report Debug is non-empty and cardinality-bounded.
    #[test]
    fn prop_health_report_debug(report in arb_shard_health_report()) {
        let debug = format!("{:?}", report);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.len() < 16 * 1_024);
        if report.shards.len() > 16 {
            let omitted = report.shards.len() - 16;
            // Keep every formatting invocation outside `prop_assert!`: the
            // macro's diagnostic expansion passes the condition tokens
            // through `concat!`, which interprets braces nested anywhere in
            // that expression as its own positional placeholders.
            let expected = format!("omitted_shards: {omitted}");
            prop_assert!(debug.contains(&expected));
        }
    }

    /// Empty shards report has zero unhealthy and zero warnings.
    #[test]
    fn prop_empty_report_no_unhealthy(_dummy in 0..1u8) {
        let report = ShardHealthReport {
            timestamp_ms: 0,
            overall: HealthStatus::Healthy,
            shards: vec![],
        };
        prop_assert_eq!(report.unhealthy_shards().len(), 0);
        prop_assert_eq!(report.watchdog_warnings().len(), 0);
    }

    /// All-healthy report has zero unhealthy shards.
    #[test]
    fn prop_all_healthy_no_warnings(
        count in 1usize..8,
    ) {
        let shards: Vec<ShardHealthEntry> = (0..count).map(|i| ShardHealthEntry {
            shard_id: ShardId(i),
            status: HealthStatus::Healthy,
            pane_count: Some(10),
            circuit: CircuitBreakerStatus {
                state: frankenterm_core::circuit_breaker::CircuitStateKind::Closed,
                consecutive_failures: 0,
                failure_threshold: 5,
                success_threshold: 3,
                open_cooldown_ms: 30000,
                open_for_ms: None,
                cooldown_remaining_ms: None,
                half_open_successes: None,
            },
            probe_outcome: ShardHealthProbeOutcome::Complete,
        }).collect();
        let report = ShardHealthReport {
            timestamp_ms: 100,
            overall: HealthStatus::Healthy,
            shards,
        };
        prop_assert_eq!(report.unhealthy_shards().len(), 0);
        prop_assert_eq!(report.watchdog_warnings().len(), 0);
    }

    /// watchdog_warnings returns non-empty strings.
    #[test]
    fn prop_watchdog_warnings_non_empty_strings(report in arb_shard_health_report()) {
        let warnings = report.watchdog_warnings();
        for w in &warnings {
            prop_assert!(!w.is_empty(), "warning string should be non-empty");
        }
    }
}
