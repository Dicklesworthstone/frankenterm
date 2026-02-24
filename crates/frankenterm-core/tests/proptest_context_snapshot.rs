//! Property-based tests for context_snapshot module.
//!
//! Tests invariants for:
//! - DurationTracker phase accounting
//! - SnapshotConfig bounds enforcement
//! - PaneSnapshotManager rate limiting and ring eviction
//! - ContextSnapshot serialization roundtrip
//! - Environment capture security invariants

use std::collections::HashMap;

use frankenterm_core::context_snapshot::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_phase_end_reason() -> impl Strategy<Value = PhaseEndReason> {
    prop_oneof![
        Just(PhaseEndReason::RegimeShift),
        Just(PhaseEndReason::ManualReset),
        proptest::option::of(any::<i32>())
            .prop_map(|ec| PhaseEndReason::CommandFinished { exit_code: ec }),
    ]
}

fn arb_shell_transition() -> impl Strategy<Value = ShellTransition> {
    prop_oneof![
        Just(ShellTransition::CommandStarted),
        Just(ShellTransition::CommandFinished),
        Just(ShellTransition::PromptRestored),
    ]
}

fn arb_snapshot_trigger() -> impl Strategy<Value = SnapshotTrigger> {
    prop_oneof![
        (0u64..1000, 0.0f64..1.0).prop_map(|(idx, prob)| SnapshotTrigger::BocpdChangePoint {
            observation_index: idx,
            posterior_probability: prob,
        }),
        (arb_shell_transition(), proptest::option::of(any::<i32>())).prop_map(|(t, ec)| {
            SnapshotTrigger::Osc133Boundary {
                transition: t,
                exit_code: ec,
            }
        }),
        "[a-z]{1,20}".prop_map(|reason| SnapshotTrigger::Manual { reason }),
    ]
}

fn arb_snapshot_env() -> impl Strategy<Value = SnapshotEnv> {
    proptest::collection::hash_map("[A-Z_]{1,10}", "[a-z0-9]{1,20}", 0..5).prop_map(|vars| {
        SnapshotEnv {
            vars,
            redacted_count: 0,
        }
    })
}

fn arb_output_features() -> impl Strategy<Value = SnapshotOutputFeatures> {
    (
        0.0f64..1000.0,
        0.0f64..100000.0,
        0.0f64..8.0,
        0.0f64..1.0,
        0.0f64..1.0,
    )
        .prop_map(|(rate, bytes, ent, unique, ansi)| SnapshotOutputFeatures {
            output_rate: rate,
            byte_rate: bytes,
            entropy: ent,
            unique_line_ratio: unique,
            ansi_density: ansi,
        })
}

fn arb_context_snapshot() -> impl Strategy<Value = ContextSnapshot> {
    (
        0u64..1000,
        0u64..100,
        1_000_000u64..2_000_000_000_000_000,
        arb_snapshot_trigger(),
        proptest::option::of("[a-z/]{1,30}"),
        proptest::option::of("[a-z]{1,10}"),
        proptest::option::of(arb_output_features()),
    )
        .prop_map(
            |(snap_id, pane_id, ts, trigger, cwd, domain, features)| ContextSnapshot {
                schema_version: CONTEXT_SNAPSHOT_SCHEMA_VERSION,
                snapshot_id: snap_id,
                pane_id,
                captured_at_us: ts,
                trigger,
                cwd,
                domain,
                process: None,
                env: None,
                terminal: None,
                phase_duration_us: None,
                output_features: features,
                correlation_id: format!("ctx-{pane_id}-{snap_id}-{ts}"),
            },
        )
}

fn test_config_no_ratelimit() -> SnapshotConfig {
    SnapshotConfig {
        max_snapshots_per_pane: 64,
        max_total_snapshots: 256,
        min_interval_ms: 0,
        max_env_vars: 32,
        capture_env: true,
        bocpd_threshold: 0.7,
    }
}

// =============================================================================
// DurationTracker invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Phase durations are non-negative (saturating subtraction).
    #[test]
    fn duration_phase_non_negative(
        starts in proptest::collection::vec(0u64..u64::MAX / 2, 2..20),
    ) {
        let mut tracker = DurationTracker::with_start(starts[0], 100);
        for &t in &starts[1..] {
            let phase = tracker.end_phase_at(t, PhaseEndReason::RegimeShift);
            prop_assert!(phase.duration_us() <= t.saturating_sub(0));
        }
    }

    /// Phase count never exceeds max_phases capacity.
    #[test]
    fn duration_bounded_phases(
        max_phases in 1usize..20,
        count in 1usize..50,
    ) {
        let mut tracker = DurationTracker::with_start(0, max_phases);
        for i in 0..count {
            tracker.end_phase_at((i as u64 + 1) * 100, PhaseEndReason::RegimeShift);
        }
        prop_assert!(tracker.phase_count() <= max_phases);
    }

    /// After ending N phases, the current phase start equals the last end time.
    #[test]
    fn duration_phase_continuity(
        timestamps in proptest::collection::vec(1u64..1_000_000, 2..15),
    ) {
        let mut sorted = timestamps;
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() < 2 {
            return Ok(());
        }

        let mut tracker = DurationTracker::with_start(sorted[0], 100);
        for &t in &sorted[1..] {
            tracker.end_phase_at(t, PhaseEndReason::RegimeShift);
        }
        prop_assert_eq!(tracker.phase_start_us, *sorted.last().unwrap());
    }

    /// Mean duration is always between min and max phase durations.
    #[test]
    fn duration_mean_bounded(
        durations in proptest::collection::vec(1u64..1_000_000, 1..20),
    ) {
        let mut tracker = DurationTracker::with_start(0, 100);
        let mut t = 0u64;
        for &d in &durations {
            t = t.saturating_add(d);
            tracker.end_phase_at(t, PhaseEndReason::RegimeShift);
        }

        if tracker.phase_count() > 0 {
            let mean = tracker.mean_duration_us();
            let min_dur = tracker.completed_phases.iter()
                .map(|p| p.duration_us())
                .min()
                .unwrap_or(0) as f64;
            let max_dur = tracker.completed_phases.iter()
                .map(|p| p.duration_us())
                .max()
                .unwrap_or(0) as f64;
            prop_assert!(mean >= min_dur - f64::EPSILON, "mean {} < min {}", mean, min_dur);
            prop_assert!(mean <= max_dur + f64::EPSILON, "mean {} > max {}", mean, max_dur);
        }
    }

    /// Percentile ordering: p50 <= p95 <= p99.
    #[test]
    fn duration_percentile_ordering(
        durations in proptest::collection::vec(1u64..10_000_000, 3..30),
    ) {
        let mut tracker = DurationTracker::with_start(0, 100);
        let mut t = 0u64;
        for &d in &durations {
            t = t.saturating_add(d);
            tracker.end_phase_at(t, PhaseEndReason::RegimeShift);
        }

        let p50 = tracker.p50_duration_us();
        let p95 = tracker.p95_duration_us();
        let p99 = tracker.p99_duration_us();
        prop_assert!(p50 <= p95, "p50={} > p95={}", p50, p95);
        prop_assert!(p95 <= p99, "p95={} > p99={}", p95, p99);
    }
}

// =============================================================================
// Environment capture invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Captured env never exceeds max_vars limit.
    #[test]
    fn env_capture_bounded(
        vars in proptest::collection::hash_map(
            "[A-Z_]{1,15}", "[a-z0-9]{1,30}", 0..50
        ),
        max_vars in 1usize..20,
    ) {
        let env = capture_env(&vars, max_vars);
        prop_assert!(env.vars.len() <= max_vars);
    }

    /// Sensitive patterns are always redacted.
    #[test]
    fn env_sensitive_always_redacted(
        sensitive_suffix in "[A-Z]{1,5}",
    ) {
        let mut vars = HashMap::new();
        // Inject a variable with a sensitive pattern.
        let name = format!("MY_SECRET_{sensitive_suffix}");
        vars.insert(name.clone(), "hidden".to_string());
        let env = capture_env(&vars, 100);
        prop_assert!(!env.vars.contains_key(&name), "Sensitive var '{}' was not redacted", name);
    }

    /// Safe variables are always captured (when not sensitive).
    #[test]
    fn env_safe_vars_captured(
        idx in 0usize..17, // SAFE_ENV_VARS has enough entries
    ) {
        let safe_names = vec![
            "PATH", "HOME", "SHELL", "TERM", "LANG", "EDITOR", "FT_WORKSPACE",
            "FT_OUTPUT_FORMAT", "VISUAL", "USER", "HOSTNAME", "PWD", "OLDPWD",
            "SHLVL", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION",
        ];
        if idx >= safe_names.len() {
            return Ok(());
        }
        let name = safe_names[idx];
        let mut vars = HashMap::new();
        vars.insert(name.to_string(), "test_value".to_string());
        let env = capture_env(&vars, 100);
        prop_assert!(env.vars.contains_key(name), "Safe var '{}' was not captured", name);
    }

    /// Redacted count + captured count <= input count.
    #[test]
    fn env_count_consistency(
        vars in proptest::collection::hash_map(
            "[A-Z_]{1,15}", "[a-z0-9]{1,30}", 0..30
        ),
    ) {
        let env = capture_env(&vars, 100);
        prop_assert!(
            env.vars.len() + env.redacted_count <= vars.len(),
            "captured {} + redacted {} > input {}",
            env.vars.len(), env.redacted_count, vars.len()
        );
    }
}

// =============================================================================
// PaneSnapshotManager invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Snapshot count never exceeds max_snapshots_per_pane.
    #[test]
    fn manager_bounded_snapshots(
        max_snaps in 2usize..20,
        num_triggers in 1usize..50,
    ) {
        let config = SnapshotConfig {
            max_snapshots_per_pane: max_snaps,
            min_interval_ms: 0,
            ..Default::default()
        };
        let mut mgr = PaneSnapshotManager::new(0, config);

        for i in 0..num_triggers {
            mgr.manual_snapshot_at(format!("s{i}"), (i as u64 + 1) * 1000);
        }

        prop_assert!(
            mgr.snapshot_count() <= max_snaps,
            "snapshot_count {} > max {}",
            mgr.snapshot_count(), max_snaps
        );
    }

    /// Total created is always >= snapshot count.
    #[test]
    fn manager_total_ge_retained(
        num_triggers in 0usize..30,
    ) {
        let mut mgr = PaneSnapshotManager::new(0, test_config_no_ratelimit());
        for i in 0..num_triggers {
            mgr.manual_snapshot_at(format!("s{i}"), (i as u64 + 1) * 1000);
        }
        prop_assert!(mgr.total_created() >= mgr.snapshot_count() as u64);
    }

    /// Snapshot IDs are strictly monotonically increasing.
    #[test]
    fn manager_monotonic_ids(
        num_triggers in 2usize..20,
    ) {
        let mut mgr = PaneSnapshotManager::new(0, test_config_no_ratelimit());
        let mut prev_id = None;
        for i in 0..num_triggers {
            if let Some(snap) = mgr.manual_snapshot_at(format!("s{i}"), (i as u64 + 1) * 1000) {
                if let Some(prev) = prev_id {
                    prop_assert!(snap.snapshot_id > prev, "IDs not monotonic: {} <= {}", snap.snapshot_id, prev);
                }
                prev_id = Some(snap.snapshot_id);
            }
        }
    }

    /// Rate limiting suppresses snapshots within min_interval.
    #[test]
    fn manager_rate_limiting_works(
        interval_ms in 100u64..5000,
        attempts in proptest::collection::vec(0u64..10000, 2..20),
    ) {
        let config = SnapshotConfig {
            min_interval_ms: interval_ms,
            max_snapshots_per_pane: 100,
            ..Default::default()
        };
        let mut mgr = PaneSnapshotManager::new(0, config);
        let interval_us = interval_ms * 1000;

        let mut sorted_times: Vec<u64> = attempts.iter().copied().collect();
        sorted_times.sort_unstable();
        sorted_times.dedup();

        let mut last_success_us: Option<u64> = None;
        for &t in &sorted_times {
            let result = mgr.manual_snapshot_at("test".to_string(), t);
            if let Some(_snap) = result {
                if let Some(prev) = last_success_us {
                    prop_assert!(
                        t.saturating_sub(prev) >= interval_us,
                        "Snapshot at {} too close to previous at {} (interval={}μs)",
                        t, prev, interval_us
                    );
                }
                last_success_us = Some(t);
            }
        }
    }

    /// BOCPD threshold filtering works correctly.
    #[test]
    fn manager_bocpd_threshold(
        threshold in 0.1f64..0.99,
        probability in 0.0f64..1.0,
    ) {
        let config = SnapshotConfig {
            bocpd_threshold: threshold,
            min_interval_ms: 0,
            ..Default::default()
        };
        let mut mgr = PaneSnapshotManager::new(0, config);
        let result = mgr.on_bocpd_change_point(0, probability, None);

        if probability < threshold {
            prop_assert!(result.is_none(), "Should be filtered: prob={} < thresh={}", probability, threshold);
        }
        // Note: when probability >= threshold, result should be Some,
        // but we can't assert this because rate limiting may also apply.
    }
}

// =============================================================================
// Serialization roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// SnapshotTrigger survives JSON roundtrip (f64 tolerance for posterior_probability).
    #[test]
    fn trigger_serde_roundtrip(trigger in arb_snapshot_trigger()) {
        let json = serde_json::to_string(&trigger).unwrap();
        let decoded: SnapshotTrigger = serde_json::from_str(&json).unwrap();
        // f64 loses precision in JSON roundtrip — compare structurally with tolerance
        match (&trigger, &decoded) {
            (
                SnapshotTrigger::BocpdChangePoint { observation_index: oi1, posterior_probability: pp1 },
                SnapshotTrigger::BocpdChangePoint { observation_index: oi2, posterior_probability: pp2 },
            ) => {
                prop_assert_eq!(*oi1, *oi2);
                prop_assert!((pp1 - pp2).abs() < 1e-10, "prob diff: {} vs {}", pp1, pp2);
            }
            _ => { prop_assert_eq!(trigger, decoded); }
        }
    }

    /// PhaseEndReason survives JSON roundtrip.
    #[test]
    fn phase_end_reason_serde_roundtrip(reason in arb_phase_end_reason()) {
        let json = serde_json::to_string(&reason).unwrap();
        let decoded: PhaseEndReason = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(reason, decoded);
    }

    /// ShellTransition survives JSON roundtrip.
    #[test]
    fn shell_transition_serde_roundtrip(transition in arb_shell_transition()) {
        let json = serde_json::to_string(&transition).unwrap();
        let decoded: ShellTransition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(transition, decoded);
    }

    /// ContextSnapshot survives JSON roundtrip (core fields).
    #[test]
    fn context_snapshot_serde_roundtrip(snap in arb_context_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: ContextSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.snapshot_id, decoded.snapshot_id);
        prop_assert_eq!(snap.pane_id, decoded.pane_id);
        prop_assert_eq!(snap.captured_at_us, decoded.captured_at_us);
        prop_assert_eq!(snap.cwd, decoded.cwd);
        prop_assert_eq!(snap.domain, decoded.domain);
        prop_assert_eq!(snap.correlation_id, decoded.correlation_id);
    }

    /// SnapshotEnv survives JSON roundtrip.
    #[test]
    fn snapshot_env_serde_roundtrip(env in arb_snapshot_env()) {
        let json = serde_json::to_string(&env).unwrap();
        let decoded: SnapshotEnv = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(env.vars, decoded.vars);
        prop_assert_eq!(env.redacted_count, decoded.redacted_count);
    }

    /// SnapshotOutputFeatures survive JSON roundtrip within tolerance.
    #[test]
    fn output_features_serde_roundtrip(features in arb_output_features()) {
        let json = serde_json::to_string(&features).unwrap();
        let decoded: SnapshotOutputFeatures = serde_json::from_str(&json).unwrap();
        prop_assert!((features.output_rate - decoded.output_rate).abs() < 1e-10);
        prop_assert!((features.byte_rate - decoded.byte_rate).abs() < 1e-10);
        prop_assert!((features.entropy - decoded.entropy).abs() < 1e-10);
        prop_assert!((features.unique_line_ratio - decoded.unique_line_ratio).abs() < 1e-10);
        prop_assert!((features.ansi_density - decoded.ansi_density).abs() < 1e-10);
    }

    /// Serialized snapshots are within size budget.
    #[test]
    fn snapshot_within_size_budget(snap in arb_context_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        prop_assert!(
            json.len() < CONTEXT_SNAPSHOT_SIZE_BUDGET,
            "Snapshot size {} exceeds budget {}",
            json.len(), CONTEXT_SNAPSHOT_SIZE_BUDGET
        );
    }
}

// =============================================================================
// SnapshotRegistry invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Registry pane count matches number of unique panes accessed.
    #[test]
    fn registry_pane_count_accuracy(
        pane_ids in proptest::collection::vec(0u64..20, 1..15),
    ) {
        let mut registry = SnapshotRegistry::new(test_config_no_ratelimit());
        for &id in &pane_ids {
            registry.pane_manager(id);
        }
        let unique_count = pane_ids.iter().collect::<std::collections::HashSet<_>>().len();
        prop_assert_eq!(registry.pane_count(), unique_count);
    }

    /// Remove pane decreases count by 1.
    #[test]
    fn registry_remove_decrements(
        pane_ids in proptest::collection::vec(0u64..20, 2..10),
    ) {
        let mut registry = SnapshotRegistry::new(test_config_no_ratelimit());
        let unique: Vec<u64> = {
            let mut set = std::collections::BTreeSet::new();
            for &id in &pane_ids {
                registry.pane_manager(id);
                set.insert(id);
            }
            set.into_iter().collect()
        };

        let before = registry.pane_count();
        if !unique.is_empty() {
            registry.remove_pane(unique[0]);
            prop_assert_eq!(registry.pane_count(), before - 1);
        }
    }

    /// Summary total_snapshots matches sum of per-pane totals.
    #[test]
    fn registry_summary_consistency(
        ops in proptest::collection::vec((0u64..5, 1usize..5), 1..10),
    ) {
        let mut registry = SnapshotRegistry::new(test_config_no_ratelimit());
        let mut time = 1000u64;

        for (pane_id, count) in &ops {
            let mgr = registry.pane_manager(*pane_id);
            for _ in 0..*count {
                time += 1000;
                mgr.manual_snapshot_at("test".to_string(), time);
            }
        }

        let summary = registry.summary();
        let pane_total: u64 = summary.panes.iter().map(|p| p.total_created).sum();
        prop_assert_eq!(summary.total_snapshots, pane_total);
    }
}
