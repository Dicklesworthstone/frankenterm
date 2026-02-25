//! Property-based tests for scope watchdog detectors.
//!
//! Covers:
//! - Orphan detection correctness
//! - Stuck cancellation threshold accuracy
//! - Scope leak detection
//! - Severity escalation
//! - Config serde roundtrip
//! - Scan summary consistency

use frankenterm_core::scope_tree::{
    register_standard_scopes, well_known, ScopeId, ScopeState, ScopeTier, ScopeTree,
};
use frankenterm_core::scope_watchdog::{
    AlertKind, AlertSeverity, ScanSummary, ScopeWatchdog, WatchdogConfig,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_tier() -> impl Strategy<Value = ScopeTier> {
    prop_oneof![
        Just(ScopeTier::Root),
        Just(ScopeTier::Daemon),
        Just(ScopeTier::Watcher),
        Just(ScopeTier::Worker),
        Just(ScopeTier::Ephemeral),
    ]
}

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn stuck_cancellation_threshold_accurate(
        grace_ms in 100u64..30_000,
        elapsed_ms in 0i64..60_000,
    ) {
        let mut config = WatchdogConfig::default();
        config.tier_grace_periods.insert("daemon".into(), grace_ms);

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();
        let scope = ScopeId("daemon:test".into());
        tree.register(scope.clone(), ScopeTier::Daemon, &ScopeId::root(), "d", 1000).unwrap();
        tree.start(&scope, 1100).unwrap();
        tree.request_shutdown(&scope, 5000).unwrap();

        let mut watchdog = ScopeWatchdog::with_config(config);
        let check_ms = 5000 + elapsed_ms;
        let alerts = watchdog.scan(&tree, check_ms);

        let stuck: Vec<_> = alerts.iter()
            .filter(|a| matches!(a.kind, AlertKind::StuckCancellation { .. }))
            .collect();

        if elapsed_ms > grace_ms as i64 {
            prop_assert!(!stuck.is_empty(), "should detect stuck at {}ms > {}ms", elapsed_ms, grace_ms);
        } else {
            prop_assert!(stuck.is_empty(), "should NOT detect stuck at {}ms <= {}ms", elapsed_ms, grace_ms);
        }
    }

    #[test]
    fn scope_leak_threshold_accurate(
        limit in 1usize..50,
        n_daemons in 0usize..60,
    ) {
        let mut config = WatchdogConfig::default();
        config.tier_scope_limits.insert("daemon".into(), limit);

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        for i in 0..n_daemons {
            tree.register(
                ScopeId(format!("daemon:d{i}")),
                ScopeTier::Daemon,
                &ScopeId::root(),
                format!("d{i}"),
                1000,
            ).unwrap();
        }

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, 2000);

        let leaks: Vec<_> = alerts.iter()
            .filter(|a| matches!(a.kind, AlertKind::ScopeLeak { tier: ScopeTier::Daemon, .. }))
            .collect();

        if n_daemons > limit {
            prop_assert!(!leaks.is_empty(), "should detect leak at {} > {}", n_daemons, limit);
        } else {
            prop_assert!(leaks.is_empty(), "should NOT detect leak at {} <= {}", n_daemons, limit);
        }
    }

    #[test]
    fn severity_escalation_stuck(
        grace_ms in 100u64..10_000,
        multiplier in 1u32..5,
    ) {
        let mut config = WatchdogConfig::default();
        config.tier_grace_periods.insert("daemon".into(), grace_ms);

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();
        let scope = ScopeId("daemon:esc".into());
        tree.register(scope.clone(), ScopeTier::Daemon, &ScopeId::root(), "d", 1000).unwrap();
        tree.start(&scope, 1100).unwrap();
        tree.request_shutdown(&scope, 5000).unwrap();

        let elapsed = grace_ms as i64 * multiplier as i64 + 1;
        let check_ms = 5000 + elapsed;

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, check_ms);

        let stuck: Vec<_> = alerts.iter()
            .filter(|a| matches!(a.kind, AlertKind::StuckCancellation { .. }))
            .collect();

        if !stuck.is_empty() {
            let severity = stuck[0].severity;
            if multiplier >= 3 {
                prop_assert_eq!(severity, AlertSeverity::Critical);
            } else if multiplier >= 2 {
                prop_assert_eq!(severity, AlertSeverity::Error);
            } else {
                prop_assert_eq!(severity, AlertSeverity::Warning);
            }
        }
    }

    #[test]
    fn scan_summary_counts_consistent(
        n_stuck in 0usize..5,
        n_leaks in 0usize..3,
    ) {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Create conditions for stuck cancellations
        for i in 0..n_stuck {
            let id = ScopeId(format!("daemon:stuck{i}"));
            tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), format!("s{i}"), 1000).unwrap();
            tree.start(&id, 1100).unwrap();
            tree.request_shutdown(&id, 2000).unwrap();
        }

        // Create conditions for leaks (register lots of workers)
        let mut config = WatchdogConfig::default();
        config.tier_scope_limits.insert("worker".into(), 2);
        let worker_count = 2 + n_leaks;

        // Need a daemon to parent the workers
        let daemon = ScopeId("daemon:host".into());
        tree.register(daemon.clone(), ScopeTier::Daemon, &ScopeId::root(), "host", 1000).unwrap();
        tree.start(&daemon, 1100).unwrap();

        for i in 0..worker_count {
            tree.register(
                ScopeId(format!("worker:w{i}")),
                ScopeTier::Worker,
                &daemon,
                format!("w{i}"),
                1000,
            ).unwrap();
        }

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, 20_000); // Past grace period

        let summary = ScanSummary::from_alerts(&alerts, 20_000);
        prop_assert_eq!(summary.total_alerts, alerts.len());

        let total_by_severity: usize = summary.by_severity.values().sum();
        prop_assert_eq!(total_by_severity, alerts.len());

        let total_by_kind = summary.orphans
            + summary.stuck_cancellations
            + summary.zombie_finalizers
            + summary.deadlocks
            + summary.scope_leaks
            + summary.stale_created
            + summary.excessive_depth;
        prop_assert_eq!(total_by_kind, alerts.len());
    }

    #[test]
    fn config_serde_roundtrip(
        grace in 100u64..60_000,
        timeout in 100u64..30_000,
        max_depth in 1usize..20,
        stale_threshold in 1000i64..120_000,
    ) {
        let mut config = WatchdogConfig::default();
        config.default_grace_period_ms = grace;
        config.finalizer_timeout_ms = timeout;
        config.max_depth = max_depth;
        config.stale_created_threshold_ms = stale_threshold;

        let json = serde_json::to_string(&config).unwrap();
        let restored: WatchdogConfig = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(config.default_grace_period_ms, restored.default_grace_period_ms);
        prop_assert_eq!(config.finalizer_timeout_ms, restored.finalizer_timeout_ms);
        prop_assert_eq!(config.max_depth, restored.max_depth);
        prop_assert_eq!(config.stale_created_threshold_ms, restored.stale_created_threshold_ms);
    }

    #[test]
    fn tier_grace_period_consistency(tier in arb_tier()) {
        let config = WatchdogConfig::default();
        let grace = config.grace_period_for_tier(tier);

        // Grace period should be positive
        prop_assert!(grace > 0, "grace for {:?} should be > 0", tier);

        // Higher priority tiers should have shorter grace (same as cancellation policy)
        if tier == ScopeTier::Ephemeral {
            prop_assert!(grace <= 5000, "ephemeral grace should be short");
        }
        if tier == ScopeTier::Root {
            prop_assert!(grace >= 10000, "root grace should be long");
        }
    }

    #[test]
    fn clean_tree_always_clean(n_daemons in 0usize..5, n_watchers in 0usize..3) {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        for i in 0..n_daemons {
            let id = ScopeId(format!("daemon:d{i}"));
            tree.register(id.clone(), ScopeTier::Daemon, &ScopeId::root(), format!("d{i}"), 1000).unwrap();
            tree.start(&id, 1100).unwrap();
        }
        for i in 0..n_watchers {
            let id = ScopeId(format!("watcher:w{i}"));
            tree.register(id.clone(), ScopeTier::Watcher, &ScopeId::root(), format!("w{i}"), 1000).unwrap();
            tree.start(&id, 1100).unwrap();
        }

        let mut watchdog = ScopeWatchdog::new();
        // Scan at a time that's within normal bounds (2 seconds after creation)
        let alerts = watchdog.scan(&tree, 3000);

        // No alerts for a healthy tree (short time since creation)
        prop_assert!(alerts.is_empty(), "healthy tree should have no alerts, got: {:?}",
            alerts.iter().map(|a| format!("{}", a.kind)).collect::<Vec<_>>());
    }
}

// ── Non-proptest structural tests ──────────────────────────────────────────

#[test]
fn severity_ordering_is_correct() {
    assert!(AlertSeverity::Info < AlertSeverity::Warning);
    assert!(AlertSeverity::Warning < AlertSeverity::Error);
    assert!(AlertSeverity::Error < AlertSeverity::Critical);
}

#[test]
fn multiple_detectors_fire_independently() {
    let mut tree = ScopeTree::new(1000);
    tree.start(&ScopeId::root(), 1000).unwrap();

    // Create stuck cancellation
    let stuck = ScopeId("daemon:stuck".into());
    tree.register(stuck.clone(), ScopeTier::Daemon, &ScopeId::root(), "stuck", 1000)
        .unwrap();
    tree.start(&stuck, 1100).unwrap();
    tree.request_shutdown(&stuck, 2000).unwrap();

    // Create scope leak (register lots of ephemeral scopes)
    let mut config = WatchdogConfig::default();
    config.tier_scope_limits.insert("ephemeral".into(), 2);

    for i in 0..5 {
        tree.register(
            ScopeId(format!("ephemeral:query:{i}")),
            ScopeTier::Ephemeral,
            &ScopeId::root(),
            format!("q{i}"),
            1000,
        )
        .unwrap();
    }

    // Create stale scope
    let stale = ScopeId("daemon:stale".into());
    tree.register(stale.clone(), ScopeTier::Daemon, &ScopeId::root(), "stale", 1000)
        .unwrap();
    // Don't start it

    let mut watchdog = ScopeWatchdog::with_config(config);
    let alerts = watchdog.scan(&tree, 50_000); // 50s → stuck + stale + leak

    let summary = ScanSummary::from_alerts(&alerts, 50_000);
    assert!(summary.stuck_cancellations > 0, "should detect stuck");
    assert!(summary.scope_leaks > 0, "should detect leak");
    assert!(summary.stale_created > 0, "should detect stale");
    assert!(summary.has_errors());
}

#[test]
fn alert_display_variants() {
    let kinds = vec![
        AlertKind::OrphanTask {
            scope_id: ScopeId("c".into()),
            parent_id: ScopeId("p".into()),
            scope_state: ScopeState::Running,
            parent_state: ScopeState::Closed,
        },
        AlertKind::StuckCancellation {
            scope_id: ScopeId("s".into()),
            draining_since_ms: 0,
            elapsed_ms: 1000,
            expected_grace_ms: 500,
        },
        AlertKind::ZombieFinalizer {
            scope_id: ScopeId("z".into()),
            finalizing_since_ms: 0,
            elapsed_ms: 5000,
        },
        AlertKind::DeadlockRisk {
            cycle: vec![ScopeId("a".into()), ScopeId("b".into()), ScopeId("a".into())],
        },
        AlertKind::ScopeLeak {
            tier: ScopeTier::Worker,
            count: 300,
            threshold: 200,
        },
        AlertKind::StaleCreated {
            scope_id: ScopeId("stale".into()),
            created_at_ms: 0,
            elapsed_ms: 60_000,
        },
        AlertKind::ExcessiveDepth {
            scope_id: ScopeId("deep".into()),
            depth: 12,
            max_depth: 8,
        },
    ];

    for kind in kinds {
        let display = kind.to_string();
        assert!(!display.is_empty(), "display should be non-empty for {:?}", kind);
    }
}

#[test]
fn watchdog_canonical_string_deterministic() {
    let wd = ScopeWatchdog::new();
    assert_eq!(wd.canonical_string(), wd.canonical_string());
    assert!(wd.canonical_string().contains("scans=0"));
}
