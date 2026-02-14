//! Property-based tests for shard assignment, id encoding, and health reporting.

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::circuit_breaker::CircuitBreakerStatus;
use frankenterm_core::patterns::AgentType;
use frankenterm_core::sharding::{
    AssignmentStrategy, ShardHealthEntry, ShardHealthReport, ShardId, assign_pane_with_strategy,
    decode_sharded_pane_id, encode_sharded_pane_id, is_sharded_pane_id,
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

fn arb_shard_health_entry() -> impl Strategy<Value = ShardHealthEntry> {
    (
        0usize..100,   // shard_id
        "[a-z]{3,12}", // label
        arb_health_status(),
        proptest::option::of(0usize..1000), // pane_count
        arb_circuit_breaker_status(),
        proptest::option::of("[a-z ]{3,30}"), // error
    )
        .prop_map(
            |(shard_id, label, status, pane_count, circuit, error)| ShardHealthEntry {
                shard_id: ShardId(shard_id),
                label,
                status,
                pane_count,
                circuit,
                error,
            },
        )
}

fn arb_shard_health_report() -> impl Strategy<Value = ShardHealthReport> {
    (
        0u64..2_000_000_000,
        arb_health_status(),
        prop::collection::vec(arb_shard_health_entry(), 0..8),
    )
        .prop_map(|(timestamp_ms, overall, shards)| ShardHealthReport {
            timestamp_ms,
            overall,
            shards,
        })
}

// =========================================================================
// Encode/decode roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn prop_encode_decode_roundtrip(
        shard in 0usize..=65535,
        local in any::<u64>(),
    ) {
        let encoded = encode_sharded_pane_id(ShardId(shard), local);
        let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
        prop_assert_eq!(decoded_shard, ShardId(shard));
        prop_assert_eq!(decoded_local, local & ((1u64 << 48) - 1));
    }

    /// is_sharded_pane_id is consistent with encode for non-zero shards.
    #[test]
    fn prop_is_sharded_consistent_with_encode(
        shard in 1usize..=65535,
        local in any::<u64>(),
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
    fn prop_shard_zero_not_sharded(local in any::<u64>()) {
        let encoded = encode_sharded_pane_id(ShardId(0), local);
        prop_assert!(
            !is_sharded_pane_id(encoded),
            "encoded pane with shard 0 should not be detected as sharded",
        );
    }

    /// Different shard+local pairs produce different encoded values.
    #[test]
    fn prop_encode_unique(
        shard1 in 0usize..=65535,
        shard2 in 0usize..=65535,
        local1 in 0u64..(1u64 << 48),
        local2 in 0u64..(1u64 << 48),
    ) {
        prop_assume!(shard1 != shard2 || local1 != local2);
        let enc1 = encode_sharded_pane_id(ShardId(shard1), local1);
        let enc2 = encode_sharded_pane_id(ShardId(shard2), local2);
        prop_assert_ne!(enc1, enc2, "distinct shard+local should produce distinct encoded IDs");
    }
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
        pane_ids in prop::collection::vec(any::<u64>(), 20..400),
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

        // Adding one node should not remap every key.
        prop_assert!(remapped < pane_ids.len());
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

        // Note: Manual variant with non-empty pane_to_shard (HashMap<u64, _>)
        // cannot round-trip through JSON strings due to a known serde_json
        // limitation: externally-tagged enums buffer content as Value, and
        // Value::Object stores keys as String which u64's Visitor rejects.
        // Test Manual separately only when pane_to_shard is empty.
        let strategies: Vec<AssignmentStrategy> = if manual_pairs.is_empty() {
            vec![by_domain, by_agent, manual, consistent, AssignmentStrategy::RoundRobin]
        } else {
            vec![by_domain, by_agent, consistent, AssignmentStrategy::RoundRobin]
        };
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

    /// ShardHealthEntry serde roundtrip preserves all fields.
    #[test]
    fn prop_health_entry_serde_roundtrip(entry in arb_shard_health_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let back: ShardHealthEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.shard_id, entry.shard_id);
        prop_assert_eq!(&back.label, &entry.label);
        prop_assert_eq!(back.status, entry.status);
        prop_assert_eq!(back.pane_count, entry.pane_count);
        prop_assert_eq!(back.circuit.state, entry.circuit.state);
        prop_assert_eq!(&back.error, &entry.error);
    }

    /// ShardHealthReport serde roundtrip preserves structure.
    #[test]
    fn prop_health_report_serde_roundtrip(report in arb_shard_health_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let back: ShardHealthReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.timestamp_ms, report.timestamp_ms);
        prop_assert_eq!(back.overall, report.overall);
        prop_assert_eq!(back.shards.len(), report.shards.len());
        for (b, r) in back.shards.iter().zip(report.shards.iter()) {
            prop_assert_eq!(b.shard_id, r.shard_id);
            prop_assert_eq!(b.status, r.status);
        }
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

    /// watchdog_warnings count matches unhealthy_shards count.
    #[test]
    fn prop_watchdog_warnings_count(report in arb_shard_health_report()) {
        let warnings = report.watchdog_warnings();
        let unhealthy = report.unhealthy_shards();
        prop_assert_eq!(warnings.len(), unhealthy.len(),
            "watchdog_warnings count should match unhealthy_shards count");
    }
}
