//! Edge case and deterministic tests for the sharding module.
//!
//! Bead: wa-1u90p.7.1 (test expansion)
//!
//! Validates:
//! - Encode/decode pane ID roundtrips across full bit-range
//! - is_sharded_pane_id boundary behavior
//! - AssignmentStrategy variants (RoundRobin, ByDomain, ByAgentType, Manual, ConsistentHash)
//! - assign_pane_with_strategy determinism and fallback behavior
//! - ShardId ordering, Display, serde
//! - ShardHealthReport unhealthy filtering and watchdog warnings
//! - infer_agent_type for all known agent patterns
//! - AssignmentStrategy serde roundtrip for all variants

use std::collections::HashMap;

use frankenterm_core::circuit_breaker::CircuitBreakerStatus;
use frankenterm_core::patterns::AgentType;
use frankenterm_core::sharding::{
    AssignmentStrategy, LOCAL_PANE_ID_MASK, SHARD_ID_BITS, ShardHealthEntry, ShardHealthReport,
    ShardId, assign_pane_with_strategy, decode_sharded_pane_id, encode_sharded_pane_id,
    infer_agent_type, is_sharded_pane_id,
};
use frankenterm_core::watchdog::HealthStatus;
use frankenterm_core::wezterm::PaneInfo;

// =============================================================================
// Constants validation
// =============================================================================

#[test]
fn shard_id_bits_is_16() {
    assert_eq!(SHARD_ID_BITS, 16);
}

#[test]
fn local_pane_id_mask_has_correct_width() {
    // With 16 shard bits, local pane ID should use 48 bits
    let expected_bits = 64 - SHARD_ID_BITS;
    assert_eq!(expected_bits, 48);
    assert_eq!(LOCAL_PANE_ID_MASK, (1u64 << 48) - 1);
    // Verify the mask has exactly 48 set bits
    assert_eq!(LOCAL_PANE_ID_MASK.count_ones(), 48);
}

// =============================================================================
// Encode/decode roundtrip
// =============================================================================

#[test]
fn encode_decode_roundtrip_shard_zero() {
    let shard = ShardId(0);
    let local = 42;
    let encoded = encode_sharded_pane_id(shard, local);
    let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
    assert_eq!(decoded_shard, shard);
    assert_eq!(decoded_local, local);
}

#[test]
fn encode_decode_roundtrip_max_shard() {
    // Max shard value that fits in 16 bits: 65535
    let shard = ShardId(0xFFFF);
    let local = 1;
    let encoded = encode_sharded_pane_id(shard, local);
    let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
    assert_eq!(decoded_shard, shard);
    assert_eq!(decoded_local, local);
}

#[test]
fn encode_decode_roundtrip_max_local_pane_id() {
    let shard = ShardId(1);
    let local = LOCAL_PANE_ID_MASK; // all 48 bits set
    let encoded = encode_sharded_pane_id(shard, local);
    let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
    assert_eq!(decoded_shard, shard);
    assert_eq!(decoded_local, local);
}

#[test]
fn encode_decode_roundtrip_both_max() {
    let shard = ShardId(0xFFFF);
    let local = LOCAL_PANE_ID_MASK;
    let encoded = encode_sharded_pane_id(shard, local);
    assert_eq!(encoded, u64::MAX); // all 64 bits set
    let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
    assert_eq!(decoded_shard, shard);
    assert_eq!(decoded_local, local);
}

#[test]
fn encode_decode_roundtrip_both_zero() {
    let shard = ShardId(0);
    let local = 0u64;
    let encoded = encode_sharded_pane_id(shard, local);
    assert_eq!(encoded, 0);
    let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
    assert_eq!(decoded_shard, shard);
    assert_eq!(decoded_local, local);
}

#[test]
fn encode_local_pane_id_overflow_is_masked() {
    // Local pane ID that exceeds 48 bits should be masked
    let shard = ShardId(1);
    let local = u64::MAX; // all 64 bits set
    let encoded = encode_sharded_pane_id(shard, local);
    let (decoded_shard, decoded_local) = decode_sharded_pane_id(encoded);
    assert_eq!(decoded_shard, shard);
    // High bits of local are masked off
    assert_eq!(decoded_local, LOCAL_PANE_ID_MASK);
}

#[test]
fn encode_preserves_shard_id_in_high_bits() {
    let shard = ShardId(42);
    let local = 0u64;
    let encoded = encode_sharded_pane_id(shard, local);
    // Shard should be in the top 16 bits
    let high_bits = encoded >> (64 - SHARD_ID_BITS);
    assert_eq!(high_bits, 42);
}

#[test]
fn decode_all_zero_gives_shard_zero() {
    let (shard, local) = decode_sharded_pane_id(0);
    assert_eq!(shard, ShardId(0));
    assert_eq!(local, 0);
}

#[test]
fn different_shards_different_encoded_ids() {
    let local = 100;
    let e0 = encode_sharded_pane_id(ShardId(0), local);
    let e1 = encode_sharded_pane_id(ShardId(1), local);
    let e2 = encode_sharded_pane_id(ShardId(2), local);
    assert_ne!(e0, e1);
    assert_ne!(e1, e2);
    assert_ne!(e0, e2);
}

#[test]
fn same_local_id_different_shards_decode_correctly() {
    for shard_idx in [0, 1, 100, 1000, 65535] {
        let shard = ShardId(shard_idx);
        let local = 7;
        let encoded = encode_sharded_pane_id(shard, local);
        let (ds, dl) = decode_sharded_pane_id(encoded);
        assert_eq!(ds, shard, "shard mismatch for shard_idx={shard_idx}");
        assert_eq!(dl, local, "local mismatch for shard_idx={shard_idx}");
    }
}

// =============================================================================
// is_sharded_pane_id
// =============================================================================

#[test]
fn shard_zero_local_nonzero_is_not_sharded() {
    // Shard 0 means the top 16 bits are zero
    let encoded = encode_sharded_pane_id(ShardId(0), 42);
    assert!(!is_sharded_pane_id(encoded));
}

#[test]
fn shard_nonzero_is_sharded() {
    let encoded = encode_sharded_pane_id(ShardId(1), 42);
    assert!(is_sharded_pane_id(encoded));
}

#[test]
fn raw_zero_is_not_sharded() {
    assert!(!is_sharded_pane_id(0));
}

#[test]
fn max_local_shard_zero_is_not_sharded() {
    let encoded = encode_sharded_pane_id(ShardId(0), LOCAL_PANE_ID_MASK);
    assert!(!is_sharded_pane_id(encoded));
}

#[test]
fn all_bits_set_is_sharded() {
    assert!(is_sharded_pane_id(u64::MAX));
}

#[test]
fn just_one_shard_bit_set_is_sharded() {
    // Set only the lowest shard bit (bit 48)
    let pane_id = 1u64 << (64 - SHARD_ID_BITS);
    assert!(is_sharded_pane_id(pane_id));
}

// =============================================================================
// ShardId basics
// =============================================================================

#[test]
fn shard_id_ordering() {
    assert!(ShardId(0) < ShardId(1));
    assert!(ShardId(1) < ShardId(100));
    assert_eq!(ShardId(42), ShardId(42));
}

#[test]
fn shard_id_display() {
    assert_eq!(format!("{}", ShardId(0)), "0");
    assert_eq!(format!("{}", ShardId(42)), "42");
    assert_eq!(format!("{}", ShardId(65535)), "65535");
}

#[test]
fn shard_id_serde_roundtrip() {
    for idx in [0, 1, 42, 1000, 65535] {
        let shard = ShardId(idx);
        let json = serde_json::to_string(&shard).unwrap();
        let back: ShardId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, shard, "serde roundtrip failed for ShardId({idx})");
    }
}

#[test]
fn shard_id_hash_distinct() {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    for i in 0..100 {
        assert!(
            set.insert(ShardId(i)),
            "ShardId({i}) should be unique in set"
        );
    }
    assert_eq!(set.len(), 100);
}

// =============================================================================
// AssignmentStrategy: RoundRobin
// =============================================================================

#[test]
fn round_robin_deterministic_fallback() {
    let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
    let strategy = AssignmentStrategy::RoundRobin;

    // RoundRobin returns None from strategy, so deterministic_fallback_shard is used
    // which hashes the pane_id. Same pane_id should always map to same shard.
    let a = assign_pane_with_strategy(&strategy, &shards, 100, None, None);
    let b = assign_pane_with_strategy(&strategy, &shards, 100, None, None);
    assert_eq!(a, b, "same pane_id should map to same shard");
    assert!(shards.contains(&a));
}

#[test]
fn round_robin_different_panes_may_differ() {
    let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
    let strategy = AssignmentStrategy::RoundRobin;

    // Test many pane IDs: at least some should differ
    let mut assigned: Vec<ShardId> = (0..100)
        .map(|pane_id| assign_pane_with_strategy(&strategy, &shards, pane_id, None, None))
        .collect();
    assigned.sort();
    assigned.dedup();
    assert!(
        assigned.len() > 1,
        "different pane IDs should spread across shards"
    );
}

// =============================================================================
// AssignmentStrategy: ByDomain
// =============================================================================

#[test]
fn by_domain_exact_match() {
    let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
    let strategy = AssignmentStrategy::ByDomain {
        domain_to_shard: HashMap::from([
            ("local".to_string(), ShardId(0)),
            ("ssh:dev-server".to_string(), ShardId(1)),
        ]),
        default_shard: Some(ShardId(2)),
    };

    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 1, Some("local"), None),
        ShardId(0)
    );
    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 1, Some("ssh:dev-server"), None),
        ShardId(1)
    );
}

#[test]
fn by_domain_falls_to_default() {
    let shards = vec![ShardId(0), ShardId(1)];
    let strategy = AssignmentStrategy::ByDomain {
        domain_to_shard: HashMap::from([("local".to_string(), ShardId(0))]),
        default_shard: Some(ShardId(1)),
    };

    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 1, Some("unknown-domain"), None),
        ShardId(1)
    );
}

#[test]
fn by_domain_no_hint_uses_default() {
    let shards = vec![ShardId(0), ShardId(1)];
    let strategy = AssignmentStrategy::ByDomain {
        domain_to_shard: HashMap::from([("local".to_string(), ShardId(0))]),
        default_shard: Some(ShardId(1)),
    };

    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 1, None, None),
        ShardId(1)
    );
}

// =============================================================================
// AssignmentStrategy: ByAgentType
// =============================================================================

#[test]
fn by_agent_type_routes_correctly() {
    let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
    let strategy = AssignmentStrategy::ByAgentType {
        agent_to_shard: HashMap::from([
            (AgentType::Codex, ShardId(0)),
            (AgentType::ClaudeCode, ShardId(1)),
            (AgentType::Gemini, ShardId(2)),
        ]),
        default_shard: None,
    };

    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 1, None, Some(AgentType::Codex)),
        ShardId(0)
    );
    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 1, None, Some(AgentType::ClaudeCode)),
        ShardId(1)
    );
    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 1, None, Some(AgentType::Gemini)),
        ShardId(2)
    );
}

#[test]
fn by_agent_type_unknown_falls_to_default() {
    let shards = vec![ShardId(0), ShardId(1)];
    let strategy = AssignmentStrategy::ByAgentType {
        agent_to_shard: HashMap::from([(AgentType::Codex, ShardId(0))]),
        default_shard: Some(ShardId(1)),
    };

    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 1, None, Some(AgentType::Unknown)),
        ShardId(1)
    );
}

#[test]
fn by_agent_type_no_hint_falls_to_default() {
    let shards = vec![ShardId(0), ShardId(1)];
    let strategy = AssignmentStrategy::ByAgentType {
        agent_to_shard: HashMap::from([(AgentType::Codex, ShardId(0))]),
        default_shard: Some(ShardId(1)),
    };

    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 1, None, None),
        ShardId(1)
    );
}

// =============================================================================
// AssignmentStrategy: Manual
// =============================================================================

#[test]
fn manual_exact_pane_mapping() {
    let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
    let strategy = AssignmentStrategy::Manual {
        pane_to_shard: HashMap::from([(42, ShardId(1)), (100, ShardId(2))]),
        default_shard: Some(ShardId(0)),
    };

    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 42, None, None),
        ShardId(1)
    );
    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 100, None, None),
        ShardId(2)
    );
}

#[test]
fn manual_unmapped_pane_uses_default() {
    let shards = vec![ShardId(0), ShardId(1)];
    let strategy = AssignmentStrategy::Manual {
        pane_to_shard: HashMap::from([(42, ShardId(1))]),
        default_shard: Some(ShardId(0)),
    };

    assert_eq!(
        assign_pane_with_strategy(&strategy, &shards, 999, None, None),
        ShardId(0)
    );
}

#[test]
fn manual_no_default_falls_to_hash() {
    let shards = vec![ShardId(0), ShardId(1)];
    let strategy = AssignmentStrategy::Manual {
        pane_to_shard: HashMap::from([(42, ShardId(1))]),
        default_shard: None,
    };

    // Unmapped pane with no default falls to deterministic hash fallback
    let result = assign_pane_with_strategy(&strategy, &shards, 999, None, None);
    assert!(shards.contains(&result));
}

// =============================================================================
// AssignmentStrategy: ConsistentHash
// =============================================================================

#[test]
fn consistent_hash_deterministic() {
    let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
    let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: 128 };

    let a = assign_pane_with_strategy(&strategy, &shards, 42, None, None);
    let b = assign_pane_with_strategy(&strategy, &shards, 42, None, None);
    assert_eq!(a, b, "consistent hash should be deterministic");
}

#[test]
fn consistent_hash_distributes_across_shards() {
    let shards = vec![ShardId(0), ShardId(1), ShardId(2)];
    let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: 128 };

    let mut assigned: Vec<ShardId> = (0..100)
        .map(|pane_id| assign_pane_with_strategy(&strategy, &shards, pane_id, None, None))
        .collect();
    assigned.sort();
    assigned.dedup();
    assert!(
        assigned.len() > 1,
        "consistent hash should distribute across multiple shards"
    );
}

#[test]
fn consistent_hash_ignores_domain_and_agent_hints() {
    let shards = vec![ShardId(0), ShardId(1)];
    let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: 64 };

    let without_hints = assign_pane_with_strategy(&strategy, &shards, 42, None, None);
    let with_domain = assign_pane_with_strategy(&strategy, &shards, 42, Some("local"), None);
    let with_agent =
        assign_pane_with_strategy(&strategy, &shards, 42, None, Some(AgentType::Codex));

    // Hints don't affect consistent hash assignment for pane routing
    assert_eq!(without_hints, with_domain);
    assert_eq!(without_hints, with_agent);
}

// =============================================================================
// assign_pane_with_strategy edge cases
// =============================================================================

#[test]
fn empty_shard_list_returns_shard_zero() {
    let shards: Vec<ShardId> = vec![];
    let strategy = AssignmentStrategy::RoundRobin;
    let result = assign_pane_with_strategy(&strategy, &shards, 42, None, None);
    assert_eq!(result, ShardId(0));
}

#[test]
fn single_shard_always_returns_it() {
    let shards = vec![ShardId(5)];
    let strategy = AssignmentStrategy::RoundRobin;

    for pane_id in 0..20 {
        let result = assign_pane_with_strategy(&strategy, &shards, pane_id, None, None);
        assert_eq!(result, ShardId(5), "single shard should always be selected");
    }
}

#[test]
fn strategy_referencing_invalid_shard_falls_to_hash() {
    // If strategy returns a shard not in the active list, fallback is used
    let shards = vec![ShardId(0), ShardId(1)];
    let strategy = AssignmentStrategy::Manual {
        pane_to_shard: HashMap::from([(42, ShardId(99))]), // ShardId(99) not in shards
        default_shard: None,
    };

    let result = assign_pane_with_strategy(&strategy, &shards, 42, None, None);
    // Should fall to deterministic hash since ShardId(99) isn't valid
    assert!(shards.contains(&result));
}

// =============================================================================
// AssignmentStrategy serde roundtrip
// =============================================================================

#[test]
fn assignment_strategy_serde_round_robin() {
    let strategy = AssignmentStrategy::RoundRobin;
    let json = serde_json::to_string(&strategy).unwrap();
    let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
    assert_eq!(back, AssignmentStrategy::RoundRobin);
}

#[test]
fn assignment_strategy_serde_by_domain() {
    let strategy = AssignmentStrategy::ByDomain {
        domain_to_shard: HashMap::from([
            ("local".to_string(), ShardId(0)),
            ("ssh:remote".to_string(), ShardId(1)),
        ]),
        default_shard: Some(ShardId(2)),
    };
    let json = serde_json::to_string(&strategy).unwrap();
    let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
    assert_eq!(back, strategy);
}

#[test]
fn assignment_strategy_serde_by_agent_type() {
    let strategy = AssignmentStrategy::ByAgentType {
        agent_to_shard: HashMap::from([
            (AgentType::Codex, ShardId(0)),
            (AgentType::ClaudeCode, ShardId(1)),
        ]),
        default_shard: None,
    };
    let json = serde_json::to_string(&strategy).unwrap();
    let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
    assert_eq!(back, strategy);
}

#[test]
fn assignment_strategy_serde_manual_serializes() {
    // Note: HashMap<u64, ShardId> keys become JSON strings ("42"), which is a
    // known serde_json limitation. We verify serialization produces valid JSON
    // and contains the expected structure.
    let strategy = AssignmentStrategy::Manual {
        pane_to_shard: HashMap::from([(42, ShardId(1)), (100, ShardId(0))]),
        default_shard: Some(ShardId(2)),
    };
    let json = serde_json::to_string(&strategy).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["strategy"], "manual");
    assert!(parsed["pane_to_shard"].is_object());
    assert_eq!(parsed["default_shard"], 2);
}

#[test]
fn assignment_strategy_serde_consistent_hash() {
    let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: 256 };
    let json = serde_json::to_string(&strategy).unwrap();
    let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
    assert_eq!(back, strategy);
}

#[test]
fn assignment_strategy_default_is_round_robin() {
    let strategy = AssignmentStrategy::default();
    assert_eq!(strategy, AssignmentStrategy::RoundRobin);
}

// =============================================================================
// infer_agent_type
// =============================================================================

fn make_pane_info(pane_id: u64, title: Option<&str>, domain: Option<&str>) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id: 0,
        window_id: 0,
        domain_id: None,
        domain_name: domain.map(String::from),
        workspace: None,
        size: None,
        rows: None,
        cols: None,
        title: title.map(String::from),
        cwd: None,
        tty_name: None,
        cursor_x: None,
        cursor_y: None,
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: false,
        is_zoomed: false,
        extra: std::collections::HashMap::new(),
    }
}

#[test]
fn infer_codex_from_title() {
    let pane = make_pane_info(1, Some("codex-session"), None);
    assert_eq!(infer_agent_type(&pane), AgentType::Codex);
}

#[test]
fn infer_codex_case_insensitive() {
    let pane = make_pane_info(1, Some("CODEX Session"), None);
    assert_eq!(infer_agent_type(&pane), AgentType::Codex);
}

#[test]
fn infer_claude_from_title() {
    let pane = make_pane_info(1, Some("claude-code running"), None);
    assert_eq!(infer_agent_type(&pane), AgentType::ClaudeCode);
}

#[test]
fn infer_gemini_from_title() {
    let pane = make_pane_info(1, Some("gemini-cli"), None);
    assert_eq!(infer_agent_type(&pane), AgentType::Gemini);
}

#[test]
fn infer_wezterm_from_title() {
    let pane = make_pane_info(1, Some("wezterm mux"), None);
    assert_eq!(infer_agent_type(&pane), AgentType::Wezterm);
}

#[test]
fn infer_unknown_from_unrecognized() {
    let pane = make_pane_info(1, Some("bash"), None);
    assert_eq!(infer_agent_type(&pane), AgentType::Unknown);
}

#[test]
fn infer_codex_from_domain() {
    let pane = make_pane_info(1, None, Some("codex-workspace"));
    assert_eq!(infer_agent_type(&pane), AgentType::Codex);
}

#[test]
fn infer_claude_from_domain() {
    let pane = make_pane_info(1, None, Some("claude-agent"));
    assert_eq!(infer_agent_type(&pane), AgentType::ClaudeCode);
}

#[test]
fn infer_empty_title_and_domain_is_unknown() {
    let pane = make_pane_info(1, None, None);
    assert_eq!(infer_agent_type(&pane), AgentType::Unknown);
}

#[test]
fn infer_priority_codex_over_claude() {
    // codex check comes before claude in the code
    let pane = make_pane_info(1, Some("codex claude gemini"), None);
    assert_eq!(infer_agent_type(&pane), AgentType::Codex);
}

// =============================================================================
// ShardHealthReport
// =============================================================================

fn make_health_entry(
    shard_id: usize,
    label: &str,
    status: HealthStatus,
    error: Option<&str>,
) -> ShardHealthEntry {
    ShardHealthEntry {
        shard_id: ShardId(shard_id),
        label: label.to_string(),
        status,
        pane_count: Some(5),
        circuit: CircuitBreakerStatus::default(),
        error: error.map(String::from),
    }
}

#[test]
fn health_report_unhealthy_shards_filters_correctly() {
    let report = ShardHealthReport {
        timestamp_ms: 1000,
        overall: HealthStatus::Degraded,
        shards: vec![
            make_health_entry(0, "healthy-shard", HealthStatus::Healthy, None),
            make_health_entry(1, "degraded-shard", HealthStatus::Degraded, Some("slow")),
            make_health_entry(2, "critical-shard", HealthStatus::Critical, Some("down")),
        ],
    };

    let unhealthy = report.unhealthy_shards();
    assert_eq!(unhealthy.len(), 2);
    assert!(unhealthy.iter().any(|e| e.shard_id == ShardId(1)));
    assert!(unhealthy.iter().any(|e| e.shard_id == ShardId(2)));
}

#[test]
fn health_report_all_healthy_no_unhealthy() {
    let report = ShardHealthReport {
        timestamp_ms: 1000,
        overall: HealthStatus::Healthy,
        shards: vec![
            make_health_entry(0, "s0", HealthStatus::Healthy, None),
            make_health_entry(1, "s1", HealthStatus::Healthy, None),
        ],
    };

    assert!(report.unhealthy_shards().is_empty());
}

#[test]
fn health_report_watchdog_warnings_format() {
    let report = ShardHealthReport {
        timestamp_ms: 1000,
        overall: HealthStatus::Hung,
        shards: vec![
            make_health_entry(0, "good", HealthStatus::Healthy, None),
            make_health_entry(1, "bad", HealthStatus::Hung, Some("connection refused")),
        ],
    };

    let warnings = report.watchdog_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Shard 1"));
    assert!(warnings[0].contains("bad"));
    assert!(warnings[0].contains("connection refused"));
}

#[test]
fn health_report_watchdog_warnings_empty_when_all_healthy() {
    let report = ShardHealthReport {
        timestamp_ms: 1000,
        overall: HealthStatus::Healthy,
        shards: vec![make_health_entry(0, "s0", HealthStatus::Healthy, None)],
    };

    assert!(report.watchdog_warnings().is_empty());
}

#[test]
fn health_report_no_error_detail_says_no_error_details() {
    let report = ShardHealthReport {
        timestamp_ms: 1000,
        overall: HealthStatus::Critical,
        shards: vec![make_health_entry(0, "s0", HealthStatus::Critical, None)],
    };

    let warnings = report.watchdog_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("no error details"));
}

#[test]
fn health_report_empty_shards() {
    let report = ShardHealthReport {
        timestamp_ms: 1000,
        overall: HealthStatus::Healthy,
        shards: vec![],
    };

    assert!(report.unhealthy_shards().is_empty());
    assert!(report.watchdog_warnings().is_empty());
}

#[test]
fn health_report_serde_roundtrip() {
    let report = ShardHealthReport {
        timestamp_ms: 12345,
        overall: HealthStatus::Degraded,
        shards: vec![
            make_health_entry(0, "alpha", HealthStatus::Healthy, None),
            make_health_entry(1, "beta", HealthStatus::Degraded, Some("slow")),
        ],
    };

    let json = serde_json::to_string(&report).unwrap();
    let back: ShardHealthReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.timestamp_ms, 12345);
    assert_eq!(back.overall, HealthStatus::Degraded);
    assert_eq!(back.shards.len(), 2);
    assert_eq!(back.shards[0].label, "alpha");
    assert_eq!(back.shards[1].error, Some("slow".to_string()));
}

// =============================================================================
// Scale test: many shards
// =============================================================================

#[test]
fn many_shards_assignment_covers_all() {
    let shards: Vec<ShardId> = (0..100).map(ShardId).collect();
    let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: 128 };

    let mut seen = std::collections::HashSet::new();
    // With enough pane IDs, we should hit many shards
    for pane_id in 0..10_000 {
        let assigned = assign_pane_with_strategy(&strategy, &shards, pane_id, None, None);
        seen.insert(assigned);
    }

    // Should hit a significant fraction of shards (at least 50%)
    assert!(
        seen.len() > 50,
        "consistent hash with 100 shards should distribute across >50, got {}",
        seen.len()
    );
}

#[test]
fn assignment_with_100_shards_is_deterministic() {
    let shards: Vec<ShardId> = (0..100).map(ShardId).collect();
    let strategy = AssignmentStrategy::ConsistentHash { virtual_nodes: 128 };

    for pane_id in [0, 1, 42, 999, 50_000, LOCAL_PANE_ID_MASK] {
        let a = assign_pane_with_strategy(&strategy, &shards, pane_id, None, None);
        let b = assign_pane_with_strategy(&strategy, &shards, pane_id, None, None);
        assert_eq!(a, b, "determinism failed for pane_id={pane_id}");
    }
}

// =============================================================================
// Bit manipulation boundary tests
// =============================================================================

#[test]
fn encode_decode_boundary_local_ids() {
    let shard = ShardId(1);
    // Test powers of 2 and boundaries near the mask
    for local in [
        0,
        1,
        2,
        255,
        256,
        65535,
        65536,
        LOCAL_PANE_ID_MASK - 1,
        LOCAL_PANE_ID_MASK,
    ] {
        let encoded = encode_sharded_pane_id(shard, local);
        let (ds, dl) = decode_sharded_pane_id(encoded);
        assert_eq!(ds, shard, "shard mismatch for local={local}");
        assert_eq!(dl, local, "local mismatch for local={local}");
    }
}

#[test]
fn encode_decode_boundary_shard_ids() {
    let local = 42u64;
    for shard_idx in [0, 1, 2, 255, 256, 32767, 32768, 65534, 65535] {
        let shard = ShardId(shard_idx);
        let encoded = encode_sharded_pane_id(shard, local);
        let (ds, dl) = decode_sharded_pane_id(encoded);
        assert_eq!(ds, shard, "shard mismatch for shard_idx={shard_idx}");
        assert_eq!(dl, local, "local mismatch for shard_idx={shard_idx}");
    }
}

#[test]
fn encoded_id_is_unique_across_shard_local_pairs() {
    use std::collections::HashSet;
    let mut seen = HashSet::new();

    // All combinations of a few shards and locals should produce unique encoded IDs
    for shard_idx in 0..10 {
        for local in 0..100 {
            let encoded = encode_sharded_pane_id(ShardId(shard_idx), local);
            assert!(
                seen.insert(encoded),
                "collision: shard={shard_idx}, local={local}, encoded={encoded}"
            );
        }
    }
    assert_eq!(seen.len(), 1000);
}
