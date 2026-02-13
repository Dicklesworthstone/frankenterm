//! Property-based tests for the environment auto-configuration engine.
//!
//! Validates:
//! 1. poll_interval_ms is always >= 100 (base interval)
//! 2. min_poll_interval_ms is always one of {25, 50, 100}
//! 3. max_concurrent_captures is always in [4, 32]
//! 4. pattern_packs always contains "builtin:core"
//! 5. rate_limit_per_pane is always > 0
//! 6. Production hostnames enable strict_safety and rate <= 10
//! 7. Remote panes enable strict_safety
//! 8. High per-core load (> 2.0) sets poll_interval_ms >= 500
//! 9. Moderate per-core load (> 1.0) sets poll_interval_ms >= 200
//! 10. Low memory (< 2048 MB) sets poll_interval_ms >= 300
//! 11. ConfigRecommendation fields are always non-empty
//! 12. ConfigSource serde roundtrip
//! 13. ConnectionType serde roundtrip
//! 14. AutoConfig is JSON-serializable for any valid input
//! 15. Pattern packs have no duplicates

use proptest::prelude::*;

use frankenterm_core::environment::{
    AutoConfig, ConfigSource, ConnectionType, DetectedAgent, DetectedEnvironment, RemoteHost,
    ShellInfo, SystemInfo, WeztermCapabilities, WeztermInfo,
};
use frankenterm_core::patterns::AgentType;

// =============================================================================
// Strategies
// =============================================================================

fn arb_cpu_count() -> impl Strategy<Value = usize> {
    1_usize..=128
}

fn arb_memory_mb() -> impl Strategy<Value = Option<u64>> {
    prop_oneof![Just(None), (256_u64..=131072).prop_map(Some),]
}

fn arb_load_average() -> impl Strategy<Value = Option<f64>> {
    prop_oneof![Just(None), (0.0_f64..100.0).prop_map(Some),]
}

fn arb_agent_type() -> impl Strategy<Value = AgentType> {
    prop_oneof![
        Just(AgentType::Codex),
        Just(AgentType::ClaudeCode),
        Just(AgentType::Gemini),
    ]
}

fn arb_detected_agent() -> impl Strategy<Value = DetectedAgent> {
    (arb_agent_type(), 0_u64..1000, 0.0_f32..1.0).prop_map(|(agent_type, pane_id, confidence)| {
        DetectedAgent {
            agent_type,
            pane_id,
            confidence,
            indicators: vec![format!("title:{:?}", agent_type).to_lowercase()],
        }
    })
}

fn arb_connection_type() -> impl Strategy<Value = ConnectionType> {
    prop_oneof![
        Just(ConnectionType::Ssh),
        Just(ConnectionType::Wsl),
        Just(ConnectionType::Docker),
        Just(ConnectionType::Unknown),
    ]
}

fn arb_hostname() -> impl Strategy<Value = String> {
    prop_oneof![
        // Normal hostnames
        "[a-z]{3,10}-[0-9]{1,3}",
        // Production-looking hostnames
        Just("web-prod-01".to_string()),
        Just("api-production".to_string()),
        Just("live-server".to_string()),
        // Staging/dev
        Just("staging-01".to_string()),
        Just("dev-server".to_string()),
    ]
}

fn arb_remote_host() -> impl Strategy<Value = RemoteHost> {
    (
        arb_hostname(),
        arb_connection_type(),
        proptest::collection::vec(0_u64..1000, 1..5),
    )
        .prop_map(|(hostname, connection_type, pane_ids)| RemoteHost {
            hostname,
            connection_type,
            pane_ids,
        })
}

fn arb_detected_environment() -> impl Strategy<Value = DetectedEnvironment> {
    (
        arb_cpu_count(),
        arb_memory_mb(),
        arb_load_average(),
        proptest::collection::vec(arb_detected_agent(), 0..5),
        proptest::collection::vec(arb_remote_host(), 0..4),
    )
        .prop_map(|(cpu_count, memory_mb, load_average, agents, remotes)| {
            make_env(cpu_count, memory_mb, load_average, agents, remotes)
        })
}

/// Helper: construct a DetectedEnvironment from parts.
fn make_env(
    cpu_count: usize,
    memory_mb: Option<u64>,
    load_average: Option<f64>,
    agents: Vec<DetectedAgent>,
    remotes: Vec<RemoteHost>,
) -> DetectedEnvironment {
    DetectedEnvironment {
        wezterm: WeztermInfo {
            version: None,
            socket_path: None,
            is_running: false,
            capabilities: WeztermCapabilities::default(),
        },
        shell: ShellInfo {
            shell_path: Some("/bin/zsh".into()),
            shell_type: Some("zsh".into()),
            version: None,
            config_file: None,
            osc_133_enabled: false,
        },
        agents,
        remotes,
        system: SystemInfo {
            os: "linux".into(),
            arch: "x86_64".into(),
            cpu_count,
            memory_mb,
            load_average,
            detected_at_epoch_ms: 0,
        },
        detected_at: chrono::Utc::now(),
    }
}

// =============================================================================
// Property: poll_interval_ms is always >= 100
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn poll_interval_at_least_100(env in arb_detected_environment()) {
        let auto = AutoConfig::from_environment(&env);
        prop_assert!(
            auto.poll_interval_ms >= 100,
            "poll_interval_ms={} is below base of 100",
            auto.poll_interval_ms,
        );
    }
}

// =============================================================================
// Property: min_poll_interval_ms is always one of {25, 50, 100}
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn min_poll_interval_valid_tier(env in arb_detected_environment()) {
        let auto = AutoConfig::from_environment(&env);
        prop_assert!(
            [25, 50, 100].contains(&auto.min_poll_interval_ms),
            "min_poll_interval_ms={} not in {{25, 50, 100}}",
            auto.min_poll_interval_ms,
        );
    }
}

// =============================================================================
// Property: max_concurrent_captures is always in [4, 32]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn concurrent_captures_bounded(env in arb_detected_environment()) {
        let auto = AutoConfig::from_environment(&env);
        prop_assert!(
            auto.max_concurrent_captures >= 4,
            "max_concurrent_captures={} below floor of 4",
            auto.max_concurrent_captures,
        );
        prop_assert!(
            auto.max_concurrent_captures <= 32,
            "max_concurrent_captures={} above cap of 32",
            auto.max_concurrent_captures,
        );
    }
}

// =============================================================================
// Property: pattern_packs always contains "builtin:core"
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn pattern_packs_always_has_core(env in arb_detected_environment()) {
        let auto = AutoConfig::from_environment(&env);
        prop_assert!(
            auto.pattern_packs.contains(&"builtin:core".to_string()),
            "pattern_packs {:?} must contain 'builtin:core'",
            auto.pattern_packs,
        );
    }
}

// =============================================================================
// Property: pattern_packs has no duplicates
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn pattern_packs_no_duplicates(env in arb_detected_environment()) {
        let auto = AutoConfig::from_environment(&env);
        let mut seen = std::collections::HashSet::new();
        for pack in &auto.pattern_packs {
            prop_assert!(
                seen.insert(pack),
                "duplicate pattern pack: {:?} in {:?}",
                pack, auto.pattern_packs,
            );
        }
    }
}

// =============================================================================
// Property: rate_limit_per_pane is always > 0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn rate_limit_positive(env in arb_detected_environment()) {
        let auto = AutoConfig::from_environment(&env);
        prop_assert!(
            auto.rate_limit_per_pane > 0,
            "rate_limit_per_pane={} must be positive",
            auto.rate_limit_per_pane,
        );
    }
}

// =============================================================================
// Property: Production hostnames enable strict_safety and rate <= 10
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn production_hostname_enables_strict(
        cpu_count in arb_cpu_count(),
        memory_mb in arb_memory_mb(),
        load_average in arb_load_average(),
    ) {
        let remotes = vec![RemoteHost {
            hostname: "web-prod-01".to_string(),
            connection_type: ConnectionType::Ssh,
            pane_ids: vec![1],
        }];
        let env = make_env(cpu_count, memory_mb, load_average, vec![], remotes);
        let auto = AutoConfig::from_environment(&env);

        prop_assert!(auto.strict_safety, "production hostname should enable strict safety");
        prop_assert!(
            auto.rate_limit_per_pane <= 10,
            "production rate_limit={} should be <= 10",
            auto.rate_limit_per_pane,
        );
    }
}

// =============================================================================
// Property: Remote panes enable strict_safety
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn remote_panes_enable_strict(
        cpu_count in arb_cpu_count(),
        memory_mb in arb_memory_mb(),
        load_average in arb_load_average(),
        remote in arb_remote_host(),
    ) {
        let env = make_env(cpu_count, memory_mb, load_average, vec![], vec![remote]);
        let auto = AutoConfig::from_environment(&env);

        prop_assert!(
            auto.strict_safety,
            "any remote host should enable strict safety",
        );
    }
}

// =============================================================================
// Property: No remotes → not strict (unless other factors)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn no_remotes_not_strict(
        cpu_count in arb_cpu_count(),
        memory_mb in arb_memory_mb(),
        load_average in arb_load_average(),
        agents in proptest::collection::vec(arb_detected_agent(), 0..3),
    ) {
        let env = make_env(cpu_count, memory_mb, load_average, agents, vec![]);
        let auto = AutoConfig::from_environment(&env);

        prop_assert!(
            !auto.strict_safety,
            "no remote panes should mean no strict safety",
        );
        prop_assert_eq!(
            auto.rate_limit_per_pane, 30,
            "no remotes should use default rate limit of 30",
        );
    }
}

// =============================================================================
// Property: High per-core load (> 2.0) sets poll_interval_ms >= 500
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn high_load_throttles_to_500(
        cpu_count in 1_usize..=32,
        per_core_load in 2.01_f64..50.0,
    ) {
        let total_load = per_core_load * cpu_count as f64;
        let env = make_env(cpu_count, Some(16384), Some(total_load), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);

        prop_assert!(
            auto.poll_interval_ms >= 500,
            "per-core load {:.1} > 2.0 should set poll >= 500, got {}",
            per_core_load, auto.poll_interval_ms,
        );
    }
}

// =============================================================================
// Property: Moderate per-core load (> 1.0, <= 2.0) sets poll >= 200
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn moderate_load_throttles_to_200(
        cpu_count in 1_usize..=32,
        per_core_load in 1.01_f64..2.0,
    ) {
        let total_load = per_core_load * cpu_count as f64;
        let env = make_env(cpu_count, Some(16384), Some(total_load), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);

        prop_assert!(
            auto.poll_interval_ms >= 200,
            "per-core load {:.1} > 1.0 should set poll >= 200, got {}",
            per_core_load, auto.poll_interval_ms,
        );
    }
}

// =============================================================================
// Property: Low memory (< 2048 MB) sets poll_interval_ms >= 300
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn low_memory_throttles_to_300(
        cpu_count in arb_cpu_count(),
        memory_mb in 256_u64..2048,
    ) {
        let env = make_env(cpu_count, Some(memory_mb), Some(0.1), vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);

        prop_assert!(
            auto.poll_interval_ms >= 300,
            "memory {}MB < 2048 should set poll >= 300, got {}",
            memory_mb, auto.poll_interval_ms,
        );
    }
}

// =============================================================================
// Property: min_poll_interval scales correctly with CPU count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn min_poll_scales_with_cpus(cpu_count in 1_usize..=128) {
        let env = make_env(cpu_count, Some(16384), None, vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);

        let expected = if cpu_count >= 8 {
            25
        } else if cpu_count >= 4 {
            50
        } else {
            100
        };
        prop_assert_eq!(
            auto.min_poll_interval_ms, expected,
            "cpu_count={} should give min_poll={}",
            cpu_count, expected,
        );
    }
}

// =============================================================================
// Property: concurrent_captures = clamp((cpu_count * 2), 4, 32)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn concurrent_captures_formula(cpu_count in 1_usize..=128) {
        let env = make_env(cpu_count, Some(16384), None, vec![], vec![]);
        let auto = AutoConfig::from_environment(&env);

        let expected = ((cpu_count * 2).min(32) as u32).max(4);
        prop_assert_eq!(
            auto.max_concurrent_captures, expected,
            "cpu_count={} should give captures={}",
            cpu_count, expected,
        );
    }
}

// =============================================================================
// Property: AutoConfig is always JSON-serializable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn auto_config_always_serializable(env in arb_detected_environment()) {
        let auto = AutoConfig::from_environment(&env);
        let json = serde_json::to_string(&auto);
        prop_assert!(json.is_ok(), "AutoConfig should always serialize");
    }
}

// =============================================================================
// Property: ConfigRecommendation fields are non-empty when present
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn recommendations_have_nonempty_fields(env in arb_detected_environment()) {
        let auto = AutoConfig::from_environment(&env);
        for rec in &auto.recommendations {
            prop_assert!(!rec.key.is_empty(), "recommendation key is empty");
            prop_assert!(!rec.value.is_empty(), "recommendation value is empty");
            prop_assert!(!rec.reason.is_empty(), "recommendation reason is empty");
        }
    }
}

// =============================================================================
// Property: ConfigSource serializes to snake_case
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_source_serializes_to_snake_case(
        source in prop_oneof![
            Just(ConfigSource::Default),
            Just(ConfigSource::AutoDetected),
            Just(ConfigSource::ConfigFile),
        ]
    ) {
        let json = serde_json::to_string(&source).unwrap();
        // Should be a quoted snake_case string
        prop_assert!(json.starts_with('"'), "expected quoted string, got: {}", json);
        let inner = &json[1..json.len()-1];
        prop_assert!(
            inner.chars().all(|c: char| c.is_ascii_lowercase() || c == '_'),
            "expected snake_case, got: {}",
            inner,
        );
    }
}

// =============================================================================
// Property: ConnectionType serializes to snake_case
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn connection_type_serializes_to_snake_case(ct in arb_connection_type()) {
        let json = serde_json::to_string(&ct).unwrap();
        prop_assert!(json.starts_with('"'), "expected quoted string, got: {}", json);
        let inner = &json[1..json.len()-1];
        prop_assert!(
            inner.chars().all(|c: char| c.is_ascii_lowercase() || c == '_'),
            "expected snake_case, got: {}",
            inner,
        );
    }
}

// =============================================================================
// Unit tests for edge cases
// =============================================================================

#[test]
fn empty_environment_uses_safe_defaults() {
    let env = make_env(1, None, None, vec![], vec![]);
    let auto = AutoConfig::from_environment(&env);
    assert_eq!(auto.poll_interval_ms, 100);
    assert!(!auto.strict_safety);
    assert_eq!(auto.rate_limit_per_pane, 30);
    assert_eq!(auto.pattern_packs, vec!["builtin:core"]);
    assert_eq!(auto.max_concurrent_captures, 4);
    assert_eq!(auto.min_poll_interval_ms, 100);
}

#[test]
fn boundary_load_exactly_1_0_not_moderate() {
    // per-core load 1.0 exactly → NOT moderate (condition is > 1.0)
    let env = make_env(4, Some(8192), Some(4.0), vec![], vec![]);
    let auto = AutoConfig::from_environment(&env);
    assert_eq!(auto.poll_interval_ms, 100);
}

#[test]
fn boundary_load_exactly_2_0_is_moderate_not_high() {
    // per-core load 2.0 exactly → moderate but NOT high (condition for high is > 2.0)
    let env = make_env(4, Some(8192), Some(8.0), vec![], vec![]);
    let auto = AutoConfig::from_environment(&env);
    assert_eq!(auto.poll_interval_ms, 200);
}

#[test]
fn boundary_memory_exactly_2048_not_low() {
    // 2048 MB is NOT low (condition is < 2048)
    let env = make_env(4, Some(2048), Some(0.1), vec![], vec![]);
    let auto = AutoConfig::from_environment(&env);
    assert_eq!(auto.poll_interval_ms, 100);
}

#[test]
fn all_agent_types_get_packs() {
    let agents = vec![
        DetectedAgent {
            agent_type: AgentType::Codex,
            pane_id: 1,
            confidence: 0.9,
            indicators: vec!["title:codex".into()],
        },
        DetectedAgent {
            agent_type: AgentType::ClaudeCode,
            pane_id: 2,
            confidence: 0.8,
            indicators: vec!["title:claude".into()],
        },
        DetectedAgent {
            agent_type: AgentType::Gemini,
            pane_id: 3,
            confidence: 0.7,
            indicators: vec!["title:gemini".into()],
        },
    ];
    let env = make_env(4, Some(8192), None, agents, vec![]);
    let auto = AutoConfig::from_environment(&env);
    assert!(auto.pattern_packs.contains(&"builtin:codex".to_string()));
    assert!(
        auto.pattern_packs
            .contains(&"builtin:claude_code".to_string())
    );
    assert!(auto.pattern_packs.contains(&"builtin:gemini".to_string()));
}
