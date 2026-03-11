//! Property-based tests for `search::daemon_bridge` types.
//!
//! Covers serde roundtrips for all 8 public types:
//! EmbedPriority, DaemonBridgeConfig, DaemonBridgeMetrics,
//! SingleEmbedEntry, BatchEmbedRequest, SingleEmbedResult,
//! BatchEmbedResult, DaemonBridgeExplanation.
//!
//! Also tests behavioral invariants: priority mapping roundtrip,
//! config bridging identity, derived metric bounds, and explain_bridge
//! degradation classification.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::search::{
    BatchEmbedRequest, BatchEmbedResult, DaemonBridgeConfig, DaemonBridgeExplanation,
    DaemonBridgeMetrics, EmbedPriority, SingleEmbedEntry, SingleEmbedResult,
    compute_batch_utilization, compute_cache_hit_rate, compute_priority_skew, explain_bridge,
    from_coalescer_config, to_coalescer_config,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_embed_priority() -> impl Strategy<Value = EmbedPriority> {
    prop_oneof![
        Just(EmbedPriority::Interactive),
        Just(EmbedPriority::Background),
    ]
}

fn arb_daemon_bridge_config() -> impl Strategy<Value = DaemonBridgeConfig> {
    (
        1usize..256,   // max_batch_size
        1u64..1000,    // max_wait_ms
        1usize..64,    // min_batch_size
        any::<bool>(), // use_priority_lanes
        1usize..4096,  // cache_capacity
    )
        .prop_map(
            |(max_batch_size, max_wait_ms, min_batch_size, use_priority_lanes, cache_capacity)| {
                DaemonBridgeConfig {
                    max_batch_size,
                    max_wait_ms,
                    min_batch_size,
                    use_priority_lanes,
                    cache_capacity,
                }
            },
        )
}

fn arb_daemon_bridge_metrics() -> impl Strategy<Value = DaemonBridgeMetrics> {
    (
        any::<u64>(),       // total_submitted
        any::<u64>(),       // total_batches
        any::<u64>(),       // total_texts_batched
        any::<u64>(),       // interactive_submissions
        any::<u64>(),       // background_submissions
        0.0f64..10_000.0,   // avg_batch_size (realistic range)
        any::<u64>(),       // early_dispatches
        any::<u64>(),       // deadline_dispatches
        any::<u64>(),       // full_batch_dispatches
        any::<u64>(),       // timeout_dispatches
    )
        .prop_flat_map(|t| {
            (
                Just(t),
                any::<u64>(),   // cache_hits
                any::<u64>(),   // cache_misses
                any::<usize>(), // cache_entries
                any::<usize>(), // cache_capacity
            )
        })
        .prop_map(
            |(
                (
                    total_submitted,
                    total_batches,
                    total_texts_batched,
                    interactive_submissions,
                    background_submissions,
                    avg_batch_size,
                    early_dispatches,
                    deadline_dispatches,
                    full_batch_dispatches,
                    timeout_dispatches,
                ),
                cache_hits,
                cache_misses,
                cache_entries,
                cache_capacity,
            )| {
                DaemonBridgeMetrics {
                    total_submitted,
                    total_batches,
                    total_texts_batched,
                    interactive_submissions,
                    background_submissions,
                    avg_batch_size,
                    early_dispatches,
                    deadline_dispatches,
                    full_batch_dispatches,
                    timeout_dispatches,
                    cache_hits,
                    cache_misses,
                    cache_entries,
                    cache_capacity,
                }
            },
        )
}

fn arb_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_]{1,20}"
}

fn arb_single_embed_entry() -> impl Strategy<Value = SingleEmbedEntry> {
    (any::<u64>(), arb_string(), proptest::option::of(arb_string())).prop_map(
        |(id, text, model)| SingleEmbedEntry { id, text, model },
    )
}

fn arb_single_embed_result() -> impl Strategy<Value = SingleEmbedResult> {
    (
        any::<u64>(),
        proptest::collection::vec(-1.0f32..1.0, 0..16),
        arb_string(),
        any::<u64>(),
    )
        .prop_map(|(id, vector, model, elapsed_ms)| SingleEmbedResult {
            id,
            vector,
            model,
            elapsed_ms,
        })
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. EmbedPriority serde roundtrip
    #[test]
    fn prop_embed_priority_serde_roundtrip(p in arb_embed_priority()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: EmbedPriority = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(p, back);
    }

    // 2. DaemonBridgeConfig serde roundtrip
    #[test]
    fn prop_config_serde_roundtrip(cfg in arb_daemon_bridge_config()) {
        let json = serde_json::to_string(&cfg).unwrap();
        let back: DaemonBridgeConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cfg, back);
    }

    // 3. DaemonBridgeMetrics serde roundtrip
    #[test]
    fn prop_metrics_serde_roundtrip(m in arb_daemon_bridge_metrics()) {
        let json = serde_json::to_string(&m).unwrap();
        let back: DaemonBridgeMetrics = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(m.total_submitted, back.total_submitted);
        prop_assert_eq!(m.total_batches, back.total_batches);
        prop_assert_eq!(m.cache_hits, back.cache_hits);
        prop_assert_eq!(m.cache_misses, back.cache_misses);
        prop_assert_eq!(m.cache_entries, back.cache_entries);
        // f64 avg_batch_size: check approximate equality
        prop_assert!((m.avg_batch_size - back.avg_batch_size).abs() < 1e-10);
    }

    // 4. SingleEmbedEntry serde roundtrip
    #[test]
    fn prop_single_embed_entry_serde_roundtrip(e in arb_single_embed_entry()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: SingleEmbedEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(e.id, back.id);
        prop_assert_eq!(&e.text, &back.text);
        prop_assert_eq!(e.model, back.model);
    }

    // 5. BatchEmbedRequest serde roundtrip
    #[test]
    fn prop_batch_embed_request_serde_roundtrip(
        entries in proptest::collection::vec(arb_single_embed_entry(), 0..8),
        priority in arb_embed_priority(),
    ) {
        let req = BatchEmbedRequest { entries, priority };
        let json = serde_json::to_string(&req).unwrap();
        let back: BatchEmbedRequest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(req.len(), back.len());
        prop_assert_eq!(req.priority, back.priority);
        for (a, b) in req.entries.iter().zip(back.entries.iter()) {
            prop_assert_eq!(a.id, b.id);
            prop_assert_eq!(&a.text, &b.text);
        }
    }

    // 6. SingleEmbedResult serde roundtrip
    #[test]
    fn prop_single_embed_result_serde_roundtrip(r in arb_single_embed_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: SingleEmbedResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r.id, back.id);
        prop_assert_eq!(&r.model, &back.model);
        prop_assert_eq!(r.elapsed_ms, back.elapsed_ms);
        prop_assert_eq!(r.vector.len(), back.vector.len());
    }

    // 7. BatchEmbedResult serde roundtrip
    #[test]
    fn prop_batch_embed_result_serde_roundtrip(
        results in proptest::collection::vec(arb_single_embed_result(), 0..8),
        batch_size in any::<usize>(),
        coalesced in any::<bool>(),
    ) {
        let r = BatchEmbedResult { results, batch_size, coalesced };
        let json = serde_json::to_string(&r).unwrap();
        let back: BatchEmbedResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r.results.len(), back.results.len());
        prop_assert_eq!(r.batch_size, back.batch_size);
        prop_assert_eq!(r.coalesced, back.coalesced);
    }

    // 8. DaemonBridgeExplanation serde roundtrip
    #[test]
    fn prop_explanation_serde_roundtrip(
        cfg in arb_daemon_bridge_config(),
        metrics in arb_daemon_bridge_metrics(),
        cache_hit_rate in 0.0f64..=1.0,
        batch_utilization in 0.0f64..=1.0,
        priority_skew in -1.0f64..=1.0,
        is_degraded in any::<bool>(),
        degradation_reason in proptest::option::of("[a-z ]{5,30}"),
    ) {
        let expl = DaemonBridgeExplanation {
            config: cfg,
            metrics,
            cache_hit_rate,
            batch_utilization,
            priority_skew,
            is_degraded,
            degradation_reason: degradation_reason.clone(),
        };
        let json = serde_json::to_string(&expl).unwrap();
        let back: DaemonBridgeExplanation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.is_degraded, is_degraded);
        prop_assert_eq!(back.degradation_reason, degradation_reason);
        prop_assert!((back.cache_hit_rate - cache_hit_rate).abs() < 1e-10);
        prop_assert!((back.batch_utilization - batch_utilization).abs() < 1e-10);
    }
}

// =============================================================================
// Behavioral invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 9. EmbedPriority parse roundtrip for canonical strings
    #[test]
    fn prop_priority_as_str_parse_roundtrip(p in arb_embed_priority()) {
        let s = p.as_str();
        prop_assert_eq!(EmbedPriority::parse(s), p);
    }

    // 10. EmbedPriority Display matches as_str
    #[test]
    fn prop_priority_display_matches_as_str(p in arb_embed_priority()) {
        prop_assert_eq!(format!("{p}"), p.as_str());
    }

    // 11. EmbedPriority is_interactive correctness
    #[test]
    fn prop_priority_is_interactive_correctness(p in arb_embed_priority()) {
        prop_assert_eq!(p.is_interactive(), p == EmbedPriority::Interactive);
    }

    // 12. Config bridging identity: to_coalescer_config → from_coalescer_config preserves
    #[test]
    fn prop_config_bridge_identity(cfg in arb_daemon_bridge_config()) {
        let cc = to_coalescer_config(&cfg);
        let back = from_coalescer_config(&cc, cfg.cache_capacity);
        prop_assert_eq!(cfg, back);
    }

    // 13. DaemonBridgeConfig default serde: empty JSON yields defaults
    #[test]
    fn prop_config_default_from_empty_json(_dummy in 0u8..1) {
        let cfg: DaemonBridgeConfig = serde_json::from_str("{}").unwrap();
        let def = DaemonBridgeConfig::default();
        prop_assert_eq!(cfg, def);
    }

    // 14. cache_hit_rate in [0, 1]
    #[test]
    fn prop_cache_hit_rate_bounded(
        hits in 0u64..10_000,
        misses in 0u64..10_000,
    ) {
        let m = DaemonBridgeMetrics {
            cache_hits: hits,
            cache_misses: misses,
            ..Default::default()
        };
        let rate = compute_cache_hit_rate(&m);
        prop_assert!(rate >= 0.0);
        prop_assert!(rate <= 1.0);
    }

    // 15. cache_hit_rate is 0 when no requests
    #[test]
    fn prop_cache_hit_rate_zero_when_empty(_dummy in 0u8..1) {
        let m = DaemonBridgeMetrics::default();
        prop_assert!((compute_cache_hit_rate(&m) - 0.0).abs() < f64::EPSILON);
    }

    // 16. batch_utilization in [0, ∞) (non-negative)
    #[test]
    fn prop_batch_utilization_non_negative(
        batches in 0u64..1000,
        avg_size in 0.0f64..100.0,
        max_size in 1usize..256,
    ) {
        let m = DaemonBridgeMetrics {
            total_batches: batches,
            avg_batch_size: avg_size,
            ..Default::default()
        };
        let util = compute_batch_utilization(&m, max_size);
        prop_assert!(util >= 0.0);
    }

    // 17. batch_utilization zero with zero batches
    #[test]
    fn prop_batch_utilization_zero_when_empty(max_size in 1usize..256) {
        let m = DaemonBridgeMetrics::default();
        prop_assert!((compute_batch_utilization(&m, max_size) - 0.0).abs() < f64::EPSILON);
    }

    // 18. priority_skew in [-1, 1]
    #[test]
    fn prop_priority_skew_bounded(
        interactive in 0u64..10_000,
        background in 0u64..10_000,
    ) {
        let m = DaemonBridgeMetrics {
            interactive_submissions: interactive,
            background_submissions: background,
            ..Default::default()
        };
        let skew = compute_priority_skew(&m);
        prop_assert!(skew >= -1.0);
        prop_assert!(skew <= 1.0);
    }

    // 19. priority_skew is 0 when balanced
    #[test]
    fn prop_priority_skew_zero_when_balanced(n in 1u64..10_000) {
        let m = DaemonBridgeMetrics {
            interactive_submissions: n,
            background_submissions: n,
            ..Default::default()
        };
        prop_assert!((compute_priority_skew(&m) - 0.0).abs() < 1e-10);
    }

    // 20. priority_skew is 0 when no submissions
    #[test]
    fn prop_priority_skew_zero_when_empty(_dummy in 0u8..1) {
        let m = DaemonBridgeMetrics::default();
        prop_assert!((compute_priority_skew(&m) - 0.0).abs() < f64::EPSILON);
    }

    // 21. BatchEmbedRequest len matches entries
    #[test]
    fn prop_batch_request_len_matches(
        entries in proptest::collection::vec(arb_single_embed_entry(), 0..20),
    ) {
        let req = BatchEmbedRequest {
            entries: entries.clone(),
            priority: EmbedPriority::Background,
        };
        prop_assert_eq!(req.len(), entries.len());
        prop_assert_eq!(req.is_empty(), entries.is_empty());
    }

    // 22. BatchEmbedRequest texts preserves order
    #[test]
    fn prop_batch_request_texts_order(
        entries in proptest::collection::vec(arb_single_embed_entry(), 1..10),
    ) {
        let req = BatchEmbedRequest {
            entries: entries.clone(),
            priority: EmbedPriority::Interactive,
        };
        let texts = req.texts();
        prop_assert_eq!(texts.len(), entries.len());
        for (t, e) in texts.iter().zip(entries.iter()) {
            prop_assert_eq!(*t, e.text.as_str());
        }
    }

    // 23. explain_bridge not degraded with empty metrics
    #[test]
    fn prop_explain_bridge_empty_not_degraded(cfg in arb_daemon_bridge_config()) {
        let m = DaemonBridgeMetrics::default();
        let expl = explain_bridge(&cfg, &m);
        prop_assert!(!expl.is_degraded);
        prop_assert!(expl.degradation_reason.is_none());
    }

    // 24. explain_bridge cache_hit_rate matches compute_cache_hit_rate
    #[test]
    fn prop_explain_bridge_cache_rate_consistent(
        cfg in arb_daemon_bridge_config(),
        hits in 0u64..10_000,
        misses in 0u64..10_000,
    ) {
        let m = DaemonBridgeMetrics {
            cache_hits: hits,
            cache_misses: misses,
            ..Default::default()
        };
        let expl = explain_bridge(&cfg, &m);
        let expected = compute_cache_hit_rate(&m);
        prop_assert!((expl.cache_hit_rate - expected).abs() < 1e-10);
    }

    // 25. SingleEmbedEntry model skip_serializing_if None
    #[test]
    fn prop_embed_entry_model_none_omitted(id in any::<u64>(), text in arb_string()) {
        let entry = SingleEmbedEntry { id, text, model: None };
        let json = serde_json::to_string(&entry).unwrap();
        prop_assert!(!json.contains("\"model\""));
    }

    // 26. SingleEmbedEntry model Some is present
    #[test]
    fn prop_embed_entry_model_some_present(
        id in any::<u64>(),
        text in arb_string(),
        model in arb_string(),
    ) {
        let entry = SingleEmbedEntry { id, text, model: Some(model.clone()) };
        let json = serde_json::to_string(&entry).unwrap();
        prop_assert!(json.contains("\"model\""));
        let back: SingleEmbedEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.model.as_deref(), Some(model.as_str()));
    }

    // 27. DaemonBridgeExplanation degradation_reason absent when not degraded
    #[test]
    fn prop_explanation_no_reason_when_not_degraded(
        cfg in arb_daemon_bridge_config(),
    ) {
        let expl = DaemonBridgeExplanation {
            config: cfg,
            metrics: DaemonBridgeMetrics::default(),
            cache_hit_rate: 0.5,
            batch_utilization: 0.5,
            priority_skew: 0.0,
            is_degraded: false,
            degradation_reason: None,
        };
        let json = serde_json::to_string(&expl).unwrap();
        prop_assert!(!json.contains("\"degradation_reason\""));
    }
}
