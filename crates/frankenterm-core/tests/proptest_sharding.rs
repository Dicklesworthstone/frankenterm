//! Property-based tests for shard assignment and id encoding.

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::patterns::AgentType;
use frankenterm_core::sharding::{
    AssignmentStrategy, ShardId, assign_pane_with_strategy, decode_sharded_pane_id,
    encode_sharded_pane_id,
};

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
}

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
        let domain_to_shard = domain_pairs
            .into_iter()
            .map(|(domain, raw)| (domain, ShardId(raw)))
            .collect::<HashMap<_, _>>();
        let pane_to_shard = manual_pairs
            .into_iter()
            .map(|(pane_id, raw)| (pane_id, ShardId(raw)))
            .collect::<HashMap<_, _>>();

        let strategy = AssignmentStrategy::Manual {
            pane_to_shard,
            default_shard: default.map(ShardId),
        };

        for pane_id in pane_ids {
            let domain = domain_to_shard.keys().next().map(String::as_str);
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
}

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

        for strategy in [by_domain, by_agent, manual, consistent, AssignmentStrategy::RoundRobin] {
            let encoded = serde_json::to_string(&strategy).unwrap();
            let decoded: AssignmentStrategy = serde_json::from_str(&encoded).unwrap();
            prop_assert_eq!(decoded, strategy);
        }
    }
}
