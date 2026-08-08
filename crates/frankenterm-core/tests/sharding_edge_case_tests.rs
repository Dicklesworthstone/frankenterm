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
//! - Finite typed shard-health wire authority and cancellation topology
//! - Bounded shard-health serialization, diagnostics, and watchdog warnings
//! - infer_agent_type for all known agent patterns
//! - AssignmentStrategy serde roundtrip for all variants

use std::collections::HashMap;

use frankenterm_core::circuit_breaker::CircuitBreakerStatus;
use frankenterm_core::patterns::AgentType;
use frankenterm_core::sharding::{
    AssignmentStrategy, LOCAL_PANE_ID_BITS, LOCAL_PANE_ID_MASK, MAX_CONFIGURED_SHARDS,
    MAX_GLOBAL_PANE_ID, MAX_SHARD_ID, SHARD_ID_BITS, ShardBackendErrorClass, ShardHealthEntry,
    ShardHealthProbeOutcome, ShardHealthReport, ShardHealthReportOutcome, ShardId,
    assign_pane_with_strategy, decode_sharded_pane_id, encode_sharded_pane_id, infer_agent_type,
    is_sharded_pane_id, try_decode_sharded_pane_id, try_encode_sharded_pane_id,
};
use frankenterm_core::watchdog::HealthStatus;
use frankenterm_core::wezterm::PaneInfo;

// =============================================================================
// Constants validation
// =============================================================================

#[test]
fn pane_id_layout_reserves_the_sqlite_sign_bit() {
    assert_eq!(SHARD_ID_BITS, 15);
    assert_eq!(LOCAL_PANE_ID_BITS, 48);
    assert_eq!(SHARD_ID_BITS + LOCAL_PANE_ID_BITS, 63);
    assert_eq!(MAX_CONFIGURED_SHARDS, MAX_SHARD_ID + 1);
}

#[test]
fn local_pane_id_mask_has_correct_width() {
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
    let shard = ShardId(MAX_SHARD_ID);
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
    let shard = ShardId(MAX_SHARD_ID);
    let local = LOCAL_PANE_ID_MASK;
    let encoded = encode_sharded_pane_id(shard, local);
    assert_eq!(encoded, MAX_GLOBAL_PANE_ID);
    assert_eq!(i64::try_from(encoded).unwrap(), i64::MAX);
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
fn encode_local_pane_id_overflow_is_rejected_without_aliasing() {
    assert!(try_encode_sharded_pane_id(ShardId(1), LOCAL_PANE_ID_MASK + 1).is_err());
    assert!(try_encode_sharded_pane_id(ShardId(1), u64::MAX).is_err());
}

#[test]
fn encode_preserves_shard_id_in_high_bits() {
    let shard = ShardId(42);
    let local = 0u64;
    let encoded = encode_sharded_pane_id(shard, local);
    // Shard bits start immediately above the 48-bit local field.
    let high_bits = encoded >> LOCAL_PANE_ID_BITS;
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
    for shard_idx in [0, 1, 100, 1000, MAX_SHARD_ID] {
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
    // Shard 0 means all 15 shard bits are zero.
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
fn out_of_domain_value_is_bitwise_sharded_but_codec_rejects_it() {
    assert!(is_sharded_pane_id(u64::MAX));
    assert!(try_decode_sharded_pane_id(u64::MAX).is_err());
    assert!(std::panic::catch_unwind(|| decode_sharded_pane_id(u64::MAX)).is_err());
}

#[test]
fn just_one_shard_bit_set_is_sharded() {
    // Set only the lowest shard bit (bit 48)
    let pane_id = 1u64 << LOCAL_PANE_ID_BITS;
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
    assert_eq!(format!("{}", ShardId(MAX_SHARD_ID)), "32767");
}

#[test]
fn shard_id_serde_roundtrip() {
    for idx in [0, 1, 42, 1000, MAX_SHARD_ID] {
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
fn assignment_strategy_serde_manual_roundtrips_string_keys() {
    let strategy = AssignmentStrategy::Manual {
        pane_to_shard: HashMap::from([(42, ShardId(1)), (100, ShardId(0))]),
        default_shard: Some(ShardId(2)),
    };
    let json = serde_json::to_string(&strategy).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["strategy"], "manual");
    assert!(parsed["pane_to_shard"].is_object());
    assert_eq!(parsed["pane_to_shard"]["42"], 1);
    assert_eq!(parsed["pane_to_shard"]["100"], 0);
    assert_eq!(parsed["default_shard"], 2);
    let back: AssignmentStrategy = serde_json::from_str(&json).unwrap();
    assert_eq!(back, strategy);
}

#[test]
fn assignment_strategy_debug_omits_raw_domain_keys() {
    let secret = format!("domain-secret-sentinel-{}", "x".repeat(32 * 1_024));
    let strategy = AssignmentStrategy::ByDomain {
        domain_to_shard: HashMap::from([
            (secret.clone(), ShardId(0)),
            ("second-private-domain".to_owned(), ShardId(1)),
        ]),
        default_shard: Some(ShardId(1)),
    };

    let debug = format!("{strategy:?}");
    assert!(debug.contains("mapping_count: 2"));
    assert!(debug.contains("default_shard: Some(ShardId(1))"));
    assert!(!debug.contains("domain-secret-sentinel"));
    assert!(!debug.contains("second-private-domain"));
    assert!(debug.len() < 256);
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
    status: HealthStatus,
    probe_outcome: ShardHealthProbeOutcome,
) -> ShardHealthEntry {
    ShardHealthEntry {
        shard_id: ShardId(shard_id),
        status,
        pane_count: matches!(
            probe_outcome,
            ShardHealthProbeOutcome::Complete | ShardHealthProbeOutcome::Cancelled
        )
        .then_some(5),
        circuit: CircuitBreakerStatus::default(),
        probe_outcome,
    }
}

#[test]
fn health_report_unhealthy_shards_filters_correctly() {
    let report = ShardHealthReport {
        timestamp_ms: 1000,
        overall: HealthStatus::Critical,
        shards: vec![
            make_health_entry(0, HealthStatus::Healthy, ShardHealthProbeOutcome::Complete),
            make_health_entry(
                1,
                HealthStatus::Degraded,
                ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other),
            ),
            make_health_entry(
                2,
                HealthStatus::Critical,
                ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other),
            ),
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
            make_health_entry(0, HealthStatus::Healthy, ShardHealthProbeOutcome::Complete),
            make_health_entry(1, HealthStatus::Healthy, ShardHealthProbeOutcome::Complete),
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
            make_health_entry(0, HealthStatus::Healthy, ShardHealthProbeOutcome::Complete),
            make_health_entry(
                1,
                HealthStatus::Hung,
                ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other),
            ),
        ],
    };

    let warnings = report.watchdog_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Shard 1 unhealthy"));
    assert!(warnings[0].contains("status=hung"));
    assert!(warnings[0].contains("circuit=closed"));
    assert!(warnings[0].contains("probe=other"));
    assert!(warnings[0].len() < 256);
}

#[test]
fn health_report_watchdog_warnings_empty_when_all_healthy() {
    let report = ShardHealthReport {
        timestamp_ms: 1000,
        overall: HealthStatus::Healthy,
        shards: vec![make_health_entry(
            0,
            HealthStatus::Healthy,
            ShardHealthProbeOutcome::Complete,
        )],
    };

    assert!(report.watchdog_warnings().is_empty());
}

#[test]
fn health_report_complete_unhealthy_probe_has_complete_class() {
    let report = ShardHealthReport {
        timestamp_ms: 1000,
        overall: HealthStatus::Critical,
        shards: vec![make_health_entry(
            0,
            HealthStatus::Critical,
            ShardHealthProbeOutcome::Complete,
        )],
    };

    let warnings = report.watchdog_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("probe=complete"));
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
            make_health_entry(0, HealthStatus::Healthy, ShardHealthProbeOutcome::Complete),
            make_health_entry(
                1,
                HealthStatus::Degraded,
                ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other),
            ),
        ],
    };

    let json = serde_json::to_string(&report).unwrap();
    let back: ShardHealthReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.timestamp_ms, 12345);
    assert_eq!(back.overall, HealthStatus::Degraded);
    assert_eq!(back.outcome(), ShardHealthReportOutcome::Complete);
    assert_eq!(back.shards.len(), 2);
    assert_eq!(
        back.shards[0].probe_outcome,
        ShardHealthProbeOutcome::Complete
    );
    assert_eq!(
        back.shards[1].probe_outcome,
        ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other)
    );

    let projection: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(projection["outcome"], "complete");
    assert!(projection["shards"][0].get("label").is_none());
    assert!(projection["shards"][1].get("error").is_none());
    assert_eq!(projection["shards"][1]["probe_outcome"]["state"], "failed");
    assert_eq!(
        projection["shards"][1]["probe_outcome"]["error_class"],
        "other"
    );
}

#[test]
fn cancelled_health_report_preserves_stable_typed_topology() {
    let report = ShardHealthReport {
        timestamp_ms: 12_346,
        overall: HealthStatus::Degraded,
        shards: vec![
            ShardHealthEntry {
                shard_id: ShardId(0),
                status: HealthStatus::Healthy,
                pane_count: Some(4),
                circuit: CircuitBreakerStatus::default(),
                probe_outcome: ShardHealthProbeOutcome::Complete,
            },
            ShardHealthEntry {
                shard_id: ShardId(1),
                status: HealthStatus::Degraded,
                pane_count: Some(2),
                circuit: CircuitBreakerStatus::default(),
                probe_outcome: ShardHealthProbeOutcome::Cancelled,
            },
            ShardHealthEntry {
                shard_id: ShardId(2),
                status: HealthStatus::Degraded,
                pane_count: None,
                circuit: CircuitBreakerStatus::default(),
                probe_outcome: ShardHealthProbeOutcome::NotStarted,
            },
        ],
    };

    assert_eq!(report.outcome(), ShardHealthReportOutcome::Cancelled);
    assert_eq!(report.shards.len(), 3);
    assert_eq!(
        report.shards[0].probe_outcome,
        ShardHealthProbeOutcome::Complete
    );
    assert_eq!(
        report.shards[1].probe_outcome,
        ShardHealthProbeOutcome::Cancelled
    );
    assert_eq!(
        report.shards[2].probe_outcome,
        ShardHealthProbeOutcome::NotStarted
    );

    let warnings = report.watchdog_warnings();
    assert_eq!(warnings.len(), 2);
    assert!(
        warnings
            .iter()
            .all(|warning| warning.contains("health unknown"))
    );
    assert!(warnings[0].contains("probe=scan_cancelled"));
    assert!(warnings[0].contains("circuit=closed"));
    assert!(warnings[1].contains("probe=not_started"));
    assert!(warnings[1].contains("circuit=not_observed"));

    let json = serde_json::to_string(&report).unwrap();
    let projection: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(projection["outcome"], "cancelled");
    let back: ShardHealthReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.outcome(), ShardHealthReportOutcome::Cancelled);
    assert_eq!(back.shards.len(), 3);
}

#[test]
fn typed_health_report_projections_are_cardinality_bounded() {
    const SHARD_COUNT: usize = 70;
    const WARNING_LIMIT: usize = 64;
    let shards = (0..SHARD_COUNT)
        .map(|index| ShardHealthEntry {
            shard_id: ShardId(index),
            status: HealthStatus::Critical,
            pane_count: None,
            circuit: CircuitBreakerStatus::default(),
            probe_outcome: ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other),
        })
        .collect();
    let report = ShardHealthReport {
        timestamp_ms: 12_347,
        overall: HealthStatus::Critical,
        shards,
    };

    let warnings = report.watchdog_warnings();
    assert_eq!(warnings.len(), WARNING_LIMIT + 1);
    assert!(
        warnings[..WARNING_LIMIT]
            .iter()
            .all(|warning| warning.len() < 256 && warning.contains("probe=other"))
    );
    assert_eq!(
        warnings.last().map(String::as_str),
        Some("Shard watchdog omitted 6 additional unhealthy shard(s) after bounded limit 64")
    );
    let debug = format!("{report:?}");
    assert!(debug.contains("shard_count: 70"));
    assert!(debug.contains("omitted_shards: 54"));
    assert!(debug.len() < 16 * 1_024);

    let json = serde_json::to_string(&report).unwrap();
    assert!(json.len() < 128 * 1_024);
    let back: ShardHealthReport = serde_json::from_str(&json).unwrap();
    assert!(back.shards.iter().all(|entry| {
        entry.probe_outcome == ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::Other)
    }));
}

#[test]
fn health_report_wire_rejects_conflicting_or_noncanonical_authority() {
    let valid = ShardHealthReport {
        timestamp_ms: 12_348,
        overall: HealthStatus::Degraded,
        shards: vec![ShardHealthEntry {
            shard_id: ShardId(0),
            status: HealthStatus::Degraded,
            pane_count: None,
            circuit: CircuitBreakerStatus::default(),
            probe_outcome: ShardHealthProbeOutcome::NotStarted,
        }],
    };
    let valid_json = serde_json::to_string(&valid).unwrap();
    let valid_value: serde_json::Value = serde_json::from_str(&valid_json).unwrap();

    let mut conflicting_outcome = valid_value.clone();
    conflicting_outcome["outcome"] = serde_json::Value::String("complete".to_owned());
    assert!(serde_json::from_value::<ShardHealthReport>(conflicting_outcome).is_err());

    let mut conflicting_overall = valid_value.clone();
    conflicting_overall["overall"] = serde_json::Value::String("healthy".to_owned());
    assert!(serde_json::from_value::<ShardHealthReport>(conflicting_overall).is_err());

    let mut missing_report_outcome = valid_value.clone();
    missing_report_outcome
        .as_object_mut()
        .unwrap()
        .remove("outcome");
    assert!(serde_json::from_value::<ShardHealthReport>(missing_report_outcome).is_err());

    let mut raw_error_injection = valid_value.clone();
    raw_error_injection["shards"][0]["error"] = serde_json::Value::String("raw-secret".to_owned());
    assert!(serde_json::from_value::<ShardHealthReport>(raw_error_injection).is_err());

    let mut raw_label_injection = valid_value.clone();
    raw_label_injection["shards"][0]["label"] = serde_json::Value::String("raw-secret".to_owned());
    assert!(serde_json::from_value::<ShardHealthReport>(raw_label_injection).is_err());

    let mut missing_typed_outcome = valid_value.clone();
    missing_typed_outcome["shards"][0]
        .as_object_mut()
        .unwrap()
        .remove("probe_outcome");
    assert!(serde_json::from_value::<ShardHealthReport>(missing_typed_outcome).is_err());

    let mut impossible_not_started_count = valid_value.clone();
    impossible_not_started_count["shards"][0]["pane_count"] = serde_json::json!(5);
    assert!(serde_json::from_value::<ShardHealthReport>(impossible_not_started_count).is_err());

    let invalid_duplicate = ShardHealthReport {
        timestamp_ms: 12_349,
        overall: HealthStatus::Healthy,
        shards: vec![
            make_health_entry(0, HealthStatus::Healthy, ShardHealthProbeOutcome::Complete),
            make_health_entry(0, HealthStatus::Healthy, ShardHealthProbeOutcome::Complete),
        ],
    };
    assert!(serde_json::to_string(&invalid_duplicate).is_err());

    let invalid_healthy_failure = ShardHealthEntry {
        shard_id: ShardId(0),
        status: HealthStatus::Healthy,
        pane_count: None,
        circuit: CircuitBreakerStatus::default(),
        probe_outcome: ShardHealthProbeOutcome::Failed(ShardBackendErrorClass::CommandFailed),
    };
    assert!(serde_json::to_string(&invalid_healthy_failure).is_err());

    let invalid_complete_without_count = ShardHealthEntry {
        shard_id: ShardId(0),
        status: HealthStatus::Healthy,
        pane_count: None,
        circuit: CircuitBreakerStatus::default(),
        probe_outcome: ShardHealthProbeOutcome::Complete,
    };
    assert!(serde_json::to_string(&invalid_complete_without_count).is_err());

    let invalid_out_of_range_entry = ShardHealthEntry {
        shard_id: ShardId(MAX_SHARD_ID + 1),
        status: HealthStatus::Healthy,
        pane_count: Some(1),
        circuit: CircuitBreakerStatus::default(),
        probe_outcome: ShardHealthProbeOutcome::Complete,
    };
    assert!(serde_json::to_string(&invalid_out_of_range_entry).is_err());

    let oversized_report = ShardHealthReport {
        timestamp_ms: 12_350,
        overall: HealthStatus::Healthy,
        shards: (0..=MAX_CONFIGURED_SHARDS)
            .map(|shard_id| {
                make_health_entry(
                    shard_id,
                    HealthStatus::Healthy,
                    ShardHealthProbeOutcome::Complete,
                )
            })
            .collect(),
    };
    assert!(serde_json::to_string(&oversized_report).is_err());
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
    for shard_idx in [0, 1, 2, 255, 256, MAX_SHARD_ID - 1, MAX_SHARD_ID] {
        let shard = ShardId(shard_idx);
        let encoded = encode_sharded_pane_id(shard, local);
        let (ds, dl) = decode_sharded_pane_id(encoded);
        assert_eq!(ds, shard, "shard mismatch for shard_idx={shard_idx}");
        assert_eq!(dl, local, "local mismatch for shard_idx={shard_idx}");
    }

    assert!(try_encode_sharded_pane_id(ShardId(MAX_SHARD_ID + 1), local).is_err());
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
