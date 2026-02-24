//! Property-based tests for ARS interceptor.
//!
//! Verifies context gate correctness, decision invariants, cooldown
//! behavior, concurrency limits, and serde roundtrips.

use proptest::prelude::*;

use std::collections::HashMap;

use frankenterm_core::ars_fst::FstMatch;
use frankenterm_core::ars_intercept::{
    ArsInterceptor, ContextBounds, ContextGateResult, FallbackReason, InterceptConfig,
    InterceptDecision, InterceptStats, PaneContext, evaluate_context_gate,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_cwd() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("/app/src".to_string()),
        Just("/home/user/project".to_string()),
        Just("/tmp".to_string()),
        Just("/var/log".to_string()),
    ]
}

fn arb_shell() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("bash".to_string()),
        Just("zsh".to_string()),
        Just("fish".to_string()),
    ]
}

fn arb_pane_context() -> impl Strategy<Value = PaneContext> {
    (arb_cwd(), arb_shell(), 40..200u16, 10..60u16, 1..100u64).prop_map(
        |(cwd, shell, cols, rows, pane_id)| PaneContext {
            cwd,
            env: HashMap::from([("TERM".to_string(), "xterm-256color".to_string())]),
            shell,
            cols,
            rows,
            pane_id,
        },
    )
}

fn arb_fst_match() -> impl Strategy<Value = FstMatch> {
    (0..1000u64, 0..50u32, 1..100usize, "[a-z]{2,6}").prop_map(
        |(reflex_id, priority, match_len, cluster)| FstMatch {
            reflex_id,
            priority,
            match_len,
            cluster_id: cluster,
        },
    )
}

// =============================================================================
// Context gate invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn empty_bounds_always_pass(ctx in arb_pane_context()) {
        let bounds = ContextBounds::default();
        let result = evaluate_context_gate(&bounds, &ctx);
        prop_assert!(result.passed());
    }

    #[test]
    fn cwd_prefix_match_passes(
        prefix_len in 1..4usize,
    ) {
        let cwd = "/app/src/main.rs";
        let prefix: String = cwd.chars().take(prefix_len).collect();
        let bounds = ContextBounds {
            cwd_prefix: prefix,
            ..Default::default()
        };
        let ctx = PaneContext {
            cwd: cwd.to_string(),
            env: HashMap::new(),
            shell: "bash".to_string(),
            cols: 80,
            rows: 24,
            pane_id: 1,
        };
        let result = evaluate_context_gate(&bounds, &ctx);
        prop_assert!(result.passed());
    }

    #[test]
    fn wrong_cwd_prefix_fails(ctx in arb_pane_context()) {
        let bounds = ContextBounds {
            cwd_prefix: "/nonexistent/path/xyz".to_string(),
            ..Default::default()
        };
        let result = evaluate_context_gate(&bounds, &ctx);
        let is_cwd_fail = matches!(result, ContextGateResult::CwdMismatch { .. });
        prop_assert!(is_cwd_fail);
    }

    #[test]
    fn shell_match_passes(shell in arb_shell()) {
        let bounds = ContextBounds {
            required_shell: shell.clone(),
            ..Default::default()
        };
        let ctx = PaneContext {
            cwd: "/tmp".to_string(),
            env: HashMap::new(),
            shell,
            cols: 80,
            rows: 24,
            pane_id: 1,
        };
        prop_assert!(evaluate_context_gate(&bounds, &ctx).passed());
    }

    #[test]
    fn dimension_check_passes_when_large_enough(
        min_cols in 10..80u16,
        min_rows in 5..24u16,
    ) {
        let bounds = ContextBounds {
            min_pane_cols: min_cols,
            min_pane_rows: min_rows,
            ..Default::default()
        };
        let ctx = PaneContext {
            cwd: "/tmp".to_string(),
            env: HashMap::new(),
            shell: "bash".to_string(),
            cols: 200,
            rows: 60,
            pane_id: 1,
        };
        prop_assert!(evaluate_context_gate(&bounds, &ctx).passed());
    }

    #[test]
    fn dimension_check_fails_when_too_small(
        min_cols in 100..200u16,
    ) {
        let bounds = ContextBounds {
            min_pane_cols: min_cols,
            ..Default::default()
        };
        let ctx = PaneContext {
            cwd: "/tmp".to_string(),
            env: HashMap::new(),
            shell: "bash".to_string(),
            cols: 80, // < min_cols
            rows: 24,
            pane_id: 1,
        };
        let result = evaluate_context_gate(&bounds, &ctx);
        let is_dim_fail = matches!(result, ContextGateResult::DimensionMismatch { .. });
        prop_assert!(is_dim_fail);
    }
}

// =============================================================================
// Interceptor decision invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn no_match_always_falls_back(ctx in arb_pane_context()) {
        let mut interceptor = ArsInterceptor::with_defaults();
        let decision = interceptor.decide(None, &ctx, 1000);
        let is_fallback = matches!(
            decision,
            InterceptDecision::FallbackToLlm { reason: FallbackReason::NoMatch }
        );
        prop_assert!(is_fallback);
    }

    #[test]
    fn disabled_always_returns_disabled(
        fst_match in arb_fst_match(),
        ctx in arb_pane_context(),
    ) {
        let config = InterceptConfig {
            enabled: false,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let decision = interceptor.decide(Some(&fst_match), &ctx, 1000);
        prop_assert_eq!(decision, InterceptDecision::Disabled);
    }

    #[test]
    fn priority_above_threshold_falls_back(
        threshold in 0..10u32,
        excess in 1..50u32,
        ctx in arb_pane_context(),
    ) {
        let config = InterceptConfig {
            max_priority_threshold: threshold,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = FstMatch {
            reflex_id: 1,
            priority: threshold + excess,
            match_len: 5,
            cluster_id: "c".to_string(),
        };
        let decision = interceptor.decide(Some(&m), &ctx, 1000);
        let is_priority_fail = matches!(
            decision,
            InterceptDecision::FallbackToLlm { reason: FallbackReason::PriorityTooLow { .. } }
        );
        prop_assert!(is_priority_fail);
    }

    #[test]
    fn priority_at_threshold_executes(
        threshold in 1..100u32,
        ctx in arb_pane_context(),
    ) {
        let config = InterceptConfig {
            max_priority_threshold: threshold,
            require_context_gate: false,
            cooldown_ms: 0,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = FstMatch {
            reflex_id: 1,
            priority: threshold, // exactly at threshold
            match_len: 5,
            cluster_id: "c".to_string(),
        };
        let decision = interceptor.decide(Some(&m), &ctx, 1000);
        let is_execute = matches!(decision, InterceptDecision::Execute { .. });
        prop_assert!(is_execute);
    }
}

// =============================================================================
// Cooldown invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn cooldown_blocks_within_window(
        cooldown_ms in 1000..10000u64,
        time_within in 0..999u64,
    ) {
        let config = InterceptConfig {
            cooldown_ms,
            require_context_gate: false,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = FstMatch {
            reflex_id: 1,
            priority: 0,
            match_len: 5,
            cluster_id: "c".to_string(),
        };
        let ctx = PaneContext {
            cwd: "/tmp".to_string(),
            env: HashMap::new(),
            shell: "bash".to_string(),
            cols: 80,
            rows: 24,
            pane_id: 42,
        };

        // First execution.
        let d1 = interceptor.decide(Some(&m), &ctx, 10000);
        let is_exec = matches!(d1, InterceptDecision::Execute { .. });
        prop_assert!(is_exec);
        interceptor.execution_completed();

        // Within cooldown.
        let d2 = interceptor.decide(Some(&m), &ctx, 10000 + time_within);
        let is_cooldown = matches!(
            d2,
            InterceptDecision::FallbackToLlm { reason: FallbackReason::Cooldown { .. } }
        );
        prop_assert!(is_cooldown);
    }

    #[test]
    fn cooldown_allows_after_window(
        cooldown_ms in 100..5000u64,
    ) {
        let config = InterceptConfig {
            cooldown_ms,
            require_context_gate: false,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = FstMatch {
            reflex_id: 1,
            priority: 0,
            match_len: 5,
            cluster_id: "c".to_string(),
        };
        let ctx = PaneContext {
            cwd: "/tmp".to_string(),
            env: HashMap::new(),
            shell: "bash".to_string(),
            cols: 80,
            rows: 24,
            pane_id: 42,
        };

        interceptor.decide(Some(&m), &ctx, 10000);
        interceptor.execution_completed();

        // After cooldown.
        let d = interceptor.decide(Some(&m), &ctx, 10000 + cooldown_ms + 1);
        let is_exec2 = matches!(d, InterceptDecision::Execute { .. });
        prop_assert!(is_exec2);
    }
}

// =============================================================================
// Stats invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn stats_attempts_equals_executions_plus_fallbacks(
        n_matches in 0..5usize,
        n_misses in 0..5usize,
    ) {
        let config = InterceptConfig {
            require_context_gate: false,
            cooldown_ms: 0,
            max_concurrent_executions: 100,
            ..Default::default()
        };
        let mut interceptor = ArsInterceptor::new(config);
        let m = FstMatch {
            reflex_id: 1,
            priority: 0,
            match_len: 5,
            cluster_id: "c".to_string(),
        };

        for i in 0..n_matches {
            let ctx = PaneContext {
                cwd: "/tmp".to_string(),
                env: HashMap::new(),
                shell: "bash".to_string(),
                cols: 80,
                rows: 24,
                pane_id: i as u64 + 1,
            };
            interceptor.decide(Some(&m), &ctx, (i as u64 + 1) * 1000);
            interceptor.execution_completed();
        }

        for i in 0..n_misses {
            let ctx = PaneContext {
                cwd: "/tmp".to_string(),
                env: HashMap::new(),
                shell: "bash".to_string(),
                cols: 80,
                rows: 24,
                pane_id: (n_matches + i) as u64 + 100,
            };
            interceptor.decide(None, &ctx, (i as u64 + 1) * 2000);
        }

        let stats = interceptor.stats();
        let total = n_matches + n_misses;
        prop_assert_eq!(stats.total_attempts, total as u64);
        prop_assert_eq!(
            stats.total_executions + stats.total_fallbacks,
            total as u64
        );
    }
}

// =============================================================================
// Serde roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_serde_roundtrip(
        enabled in prop::bool::ANY,
        cooldown in 0..60000u64,
        max_concurrent in 1..10u32,
    ) {
        let config = InterceptConfig {
            enabled,
            cooldown_ms: cooldown,
            max_concurrent_executions: max_concurrent,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: InterceptConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.enabled, config.enabled);
        prop_assert_eq!(decoded.cooldown_ms, config.cooldown_ms);
        prop_assert_eq!(decoded.max_concurrent_executions, config.max_concurrent_executions);
    }

    #[test]
    fn pane_context_serde_roundtrip(ctx in arb_pane_context()) {
        let json = serde_json::to_string(&ctx).unwrap();
        let decoded: PaneContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, ctx);
    }

    #[test]
    fn context_bounds_serde_roundtrip(
        cwd in arb_cwd(),
        shell in arb_shell(),
        min_cols in 0..200u16,
    ) {
        let bounds = ContextBounds {
            cwd_prefix: cwd,
            required_shell: shell,
            min_pane_cols: min_cols,
            ..Default::default()
        };
        let json = serde_json::to_string(&bounds).unwrap();
        let decoded: ContextBounds = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, bounds);
    }

    #[test]
    fn intercept_stats_serde_roundtrip(
        attempts in 0..1000u64,
        executions in 0..500u64,
        fallbacks in 0..500u64,
    ) {
        let stats = InterceptStats {
            total_attempts: attempts,
            total_executions: executions,
            total_fallbacks: fallbacks,
            active_executions: 0,
            is_paused: false,
            registered_bounds: 5,
            cooldown_entries: 3,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: InterceptStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, stats);
    }
}
