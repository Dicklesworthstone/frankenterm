//! Property-based tests for scope watchdog detectors.
//!
//! Covers:
//! - Orphan detection correctness
//! - Stuck cancellation threshold accuracy
//! - Scope leak detection
//! - Severity escalation
//! - Config serde roundtrip
//! - Scan summary consistency

use frankenterm_core::scope_tree::{ScopeId, ScopeState, ScopeTier, ScopeTree};
use frankenterm_core::scope_watchdog::{
    AlertKind, AlertSeverity, ScanSummary, ScopeWatchdog, WatchdogAlert, WatchdogConfig,
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

    // ── Orphan detection ────────────────────────────────────────────────

    #[test]
    fn orphan_detected_when_parent_closed(
        n_orphans in 1usize..5,
    ) {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Create a daemon, start it
        let parent = ScopeId("daemon:parent".into());
        tree.register(parent.clone(), ScopeTier::Daemon, &ScopeId::root(), "p", 1000).unwrap();
        tree.start(&parent, 1100).unwrap();

        // Register children under the parent
        for i in 0..n_orphans {
            let child = ScopeId(format!("worker:child{i}"));
            tree.register(child.clone(), ScopeTier::Worker, &parent, format!("c{i}"), 1200).unwrap();
            tree.start(&child, 1300).unwrap();
        }

        // Force parent to Closed state via get_mut (can't do via API with live children)
        tree.get_mut(&parent).unwrap().state = ScopeState::Closed;

        let mut watchdog = ScopeWatchdog::new();
        let alerts = watchdog.scan(&tree, 4000);

        let orphans: Vec<_> = alerts.iter()
            .filter(|a| matches!(a.kind, AlertKind::OrphanTask { .. }))
            .collect();

        prop_assert_eq!(orphans.len(), n_orphans, "each running child with closed parent is an orphan");
        for alert in &orphans {
            prop_assert_eq!(alert.severity, AlertSeverity::Error);
        }
    }

    // ── Zombie finalizer detection ─────────────────────────────────────

    #[test]
    fn zombie_finalizer_detected(
        timeout_ms in 1000u64..20_000,
        elapsed_ms in 0i64..40_000,
    ) {
        let mut config = WatchdogConfig::default();
        config.finalizer_timeout_ms = timeout_ms;

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let scope = ScopeId("daemon:zombie".into());
        tree.register(scope.clone(), ScopeTier::Daemon, &ScopeId::root(), "z", 1000).unwrap();
        tree.start(&scope, 1100).unwrap();
        tree.request_shutdown(&scope, 2000).unwrap();
        tree.finalize(&scope).unwrap();

        // Check at shutdown_time + elapsed
        let check_ms = 2000 + elapsed_ms;

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, check_ms);

        let zombies: Vec<_> = alerts.iter()
            .filter(|a| matches!(a.kind, AlertKind::ZombieFinalizer { .. }))
            .collect();

        if elapsed_ms > timeout_ms as i64 {
            prop_assert!(!zombies.is_empty(), "should detect zombie at {}ms > {}ms", elapsed_ms, timeout_ms);
        } else {
            prop_assert!(zombies.is_empty(), "should NOT detect zombie at {}ms <= {}ms", elapsed_ms, timeout_ms);
        }
    }

    // ── Stale Created detection ────────────────────────────────────────

    #[test]
    fn stale_created_detected(
        threshold_ms in 5000i64..60_000,
        elapsed_ms in 0i64..120_000,
    ) {
        let mut config = WatchdogConfig::default();
        config.stale_created_threshold_ms = threshold_ms;

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Register but don't start
        let scope = ScopeId("daemon:stale".into());
        tree.register(scope.clone(), ScopeTier::Daemon, &ScopeId::root(), "s", 1000).unwrap();

        let check_ms = 1000 + elapsed_ms;

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, check_ms);

        let stale: Vec<_> = alerts.iter()
            .filter(|a| matches!(a.kind, AlertKind::StaleCreated { .. }))
            .collect();

        if elapsed_ms > threshold_ms {
            prop_assert!(!stale.is_empty(), "should detect stale at {}ms > {}ms", elapsed_ms, threshold_ms);
        } else {
            prop_assert!(stale.is_empty(), "should NOT detect stale at {}ms <= {}ms", elapsed_ms, threshold_ms);
        }
    }

    // ── Excessive depth detection ──────────────────────────────────────

    #[test]
    fn excessive_depth_detected(
        max_depth in 2usize..6,
        actual_depth in 2usize..10,
    ) {
        let mut config = WatchdogConfig::default();
        config.max_depth = max_depth;
        // Don't trigger scope leak alerts for daemon tier
        config.tier_scope_limits.insert("daemon".into(), 100);

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Build a chain of daemon scopes to desired depth
        let mut parent = ScopeId::root();
        for d in 0..actual_depth {
            let child = ScopeId(format!("daemon:depth{d}"));
            tree.register(child.clone(), ScopeTier::Daemon, &parent, format!("d{d}"), 1000).unwrap();
            tree.start(&child, 1100).unwrap();
            parent = child;
        }

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, 2000);

        let depth_alerts: Vec<_> = alerts.iter()
            .filter(|a| matches!(a.kind, AlertKind::ExcessiveDepth { .. }))
            .collect();

        if actual_depth > max_depth {
            prop_assert!(!depth_alerts.is_empty(), "should detect excessive depth {} > {}", actual_depth, max_depth);
        } else {
            prop_assert!(depth_alerts.is_empty(), "should NOT detect excessive depth {} <= {}", actual_depth, max_depth);
        }
    }

    // ── Scope leak severity escalation ─────────────────────────────────

    #[test]
    fn scope_leak_severity_escalation(
        limit in 2usize..10,
        over_factor in 1usize..4,
    ) {
        let mut config = WatchdogConfig::default();
        config.tier_scope_limits.insert("worker".into(), limit);

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let daemon = ScopeId("daemon:host".into());
        tree.register(daemon.clone(), ScopeTier::Daemon, &ScopeId::root(), "host", 1000).unwrap();
        tree.start(&daemon, 1100).unwrap();

        let n_workers = limit * over_factor + 1;
        for i in 0..n_workers {
            tree.register(
                ScopeId(format!("worker:w{i}")),
                ScopeTier::Worker,
                &daemon,
                format!("w{i}"),
                1000,
            ).unwrap();
        }

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, 2000);

        let leaks: Vec<_> = alerts.iter()
            .filter(|a| matches!(a.kind, AlertKind::ScopeLeak { tier: ScopeTier::Worker, .. }))
            .collect();

        prop_assert!(!leaks.is_empty());
        let severity = leaks[0].severity;
        if n_workers > limit * 2 {
            prop_assert_eq!(severity, AlertSeverity::Critical, "severe overcount should be Critical");
        } else {
            prop_assert_eq!(severity, AlertSeverity::Warning, "mild overcount should be Warning");
        }
    }

    // ── Scan count and total_alerts tracking ───────────────────────────

    #[test]
    fn scan_count_increments(n_scans in 1usize..10) {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let mut watchdog = ScopeWatchdog::new();
        prop_assert_eq!(watchdog.scan_count(), 0);

        for i in 0..n_scans {
            let _ = watchdog.scan(&tree, 1000 * (i as i64 + 1));
        }
        prop_assert_eq!(watchdog.scan_count(), n_scans as u64);
    }

    // ── Total alerts accumulates across scans ──────────────────────────

    #[test]
    fn total_alerts_accumulates(n_scans in 1usize..5) {
        let mut config = WatchdogConfig::default();
        config.tier_scope_limits.insert("ephemeral".into(), 1);

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Create a leak that fires every scan
        for i in 0..3 {
            tree.register(
                ScopeId(format!("ephemeral:e{i}")),
                ScopeTier::Ephemeral,
                &ScopeId::root(),
                format!("e{i}"),
                1000,
            ).unwrap();
        }

        let mut watchdog = ScopeWatchdog::with_config(config);
        let mut running_total = 0u64;
        for i in 0..n_scans {
            let alerts = watchdog.scan(&tree, 2000 + i as i64);
            running_total += alerts.len() as u64;
        }

        prop_assert_eq!(watchdog.total_alerts(), running_total);
        prop_assert_eq!(watchdog.scan_count(), n_scans as u64);
    }

    // ── Deadlock detection disabled ────────────────────────────────────

    #[test]
    fn deadlock_detection_respects_config_flag(_dummy in 0u8..1) {
        let mut config = WatchdogConfig::default();
        config.detect_deadlocks = false;

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        // Draining parent with non-closed child (could be wait-for)
        let parent = ScopeId("daemon:p".into());
        tree.register(parent.clone(), ScopeTier::Daemon, &ScopeId::root(), "p", 1000).unwrap();
        tree.start(&parent, 1100).unwrap();
        let child = ScopeId("worker:c".into());
        tree.register(child.clone(), ScopeTier::Worker, &parent, "c", 1000).unwrap();
        tree.start(&child, 1100).unwrap();
        tree.request_shutdown(&parent, 2000).unwrap();

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, 50_000);

        let has_deadlock = alerts.iter()
            .any(|a| matches!(a.kind, AlertKind::DeadlockRisk { .. }));
        prop_assert!(!has_deadlock, "deadlock detection should be disabled");
    }

    // ── WatchdogAlert serde roundtrip ──────────────────────────────────

    #[test]
    fn alert_serde_roundtrip(
        grace_ms in 100u64..10_000,
    ) {
        let mut config = WatchdogConfig::default();
        config.tier_grace_periods.insert("daemon".into(), grace_ms);

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let scope = ScopeId("daemon:serde".into());
        tree.register(scope.clone(), ScopeTier::Daemon, &ScopeId::root(), "s", 1000).unwrap();
        tree.start(&scope, 1100).unwrap();
        tree.request_shutdown(&scope, 2000).unwrap();

        let check_ms = 2000 + grace_ms as i64 * 2 + 1;
        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, check_ms);

        for alert in &alerts {
            let json = serde_json::to_string(alert).unwrap();
            let restored: WatchdogAlert = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(restored.severity, alert.severity);
            prop_assert_eq!(restored.timestamp_ms, alert.timestamp_ms);
            prop_assert_eq!(&restored.message, &alert.message);
        }
    }

    // ── ScanSummary serde roundtrip ────────────────────────────────────

    #[test]
    fn scan_summary_serde_roundtrip(n_scopes in 0usize..5) {
        let mut config = WatchdogConfig::default();
        config.tier_scope_limits.insert("worker".into(), 1);

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let daemon = ScopeId("daemon:host".into());
        tree.register(daemon.clone(), ScopeTier::Daemon, &ScopeId::root(), "host", 1000).unwrap();
        tree.start(&daemon, 1100).unwrap();

        for i in 0..n_scopes {
            tree.register(
                ScopeId(format!("worker:w{i}")),
                ScopeTier::Worker,
                &daemon,
                format!("w{i}"),
                1000,
            ).unwrap();
        }

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, 2000);
        let summary = ScanSummary::from_alerts(&alerts, 2000);

        let json = serde_json::to_string(&summary).unwrap();
        let restored: ScanSummary = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.total_alerts, summary.total_alerts);
        prop_assert_eq!(restored.orphans, summary.orphans);
        prop_assert_eq!(restored.stuck_cancellations, summary.stuck_cancellations);
        prop_assert_eq!(restored.zombie_finalizers, summary.zombie_finalizers);
        prop_assert_eq!(restored.scope_leaks, summary.scope_leaks);
        prop_assert_eq!(restored.stale_created, summary.stale_created);
        prop_assert_eq!(restored.excessive_depth, summary.excessive_depth);
    }

    // ── AlertKind serde roundtrip ──────────────────────────────────────

    #[test]
    fn alert_kind_serde_roundtrip(tier in arb_tier()) {
        let kinds = vec![
            AlertKind::OrphanTask {
                scope_id: ScopeId("child".into()),
                parent_id: ScopeId("parent".into()),
                scope_state: ScopeState::Running,
                parent_state: ScopeState::Closed,
            },
            AlertKind::StuckCancellation {
                scope_id: ScopeId("stuck".into()),
                draining_since_ms: 1000,
                elapsed_ms: 5000,
                expected_grace_ms: 3000,
            },
            AlertKind::ZombieFinalizer {
                scope_id: ScopeId("zombie".into()),
                finalizing_since_ms: 2000,
                elapsed_ms: 8000,
            },
            AlertKind::DeadlockRisk {
                cycle: vec![ScopeId("a".into()), ScopeId("b".into()), ScopeId("a".into())],
            },
            AlertKind::ScopeLeak {
                tier,
                count: 100,
                threshold: 50,
            },
            AlertKind::StaleCreated {
                scope_id: ScopeId("stale".into()),
                created_at_ms: 0,
                elapsed_ms: 30_000,
            },
            AlertKind::ExcessiveDepth {
                scope_id: ScopeId("deep".into()),
                depth: 10,
                max_depth: 5,
            },
        ];

        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let restored: AlertKind = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(kind, &restored);
        }
    }

    // ── AlertKind Display is non-empty ─────────────────────────────────

    #[test]
    fn alert_kind_display_non_empty(tier in arb_tier()) {
        let kinds = vec![
            AlertKind::OrphanTask {
                scope_id: ScopeId("c".into()),
                parent_id: ScopeId("p".into()),
                scope_state: ScopeState::Running,
                parent_state: ScopeState::Closed,
            },
            AlertKind::ScopeLeak { tier, count: 10, threshold: 5 },
            AlertKind::ExcessiveDepth {
                scope_id: ScopeId("d".into()),
                depth: 10,
                max_depth: 5,
            },
        ];
        for kind in &kinds {
            let display = kind.to_string();
            prop_assert!(!display.is_empty());
        }
    }

    // ── AlertSeverity serde roundtrip ──────────────────────────────────

    #[test]
    fn alert_severity_serde_roundtrip(
        idx in 0usize..4,
    ) {
        let sevs = [AlertSeverity::Info, AlertSeverity::Warning, AlertSeverity::Error, AlertSeverity::Critical];
        let sev = sevs[idx];
        let json = serde_json::to_string(&sev).unwrap();
        let restored: AlertSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, restored);
    }

    // ── AlertSeverity ordering ─────────────────────────────────────────

    #[test]
    fn alert_severity_ordering(a in 0usize..4, b in 0usize..4) {
        let sevs = [AlertSeverity::Info, AlertSeverity::Warning, AlertSeverity::Error, AlertSeverity::Critical];
        let sa = sevs[a];
        let sb = sevs[b];
        match a.cmp(&b) {
            std::cmp::Ordering::Less => prop_assert!(sa < sb),
            std::cmp::Ordering::Greater => prop_assert!(sa > sb),
            std::cmp::Ordering::Equal => prop_assert_eq!(sa, sb),
        }
    }

    // ── ScanSummary has_errors accuracy ────────────────────────────────

    #[test]
    fn scan_summary_has_errors_accuracy(
        n_workers in 0usize..10,
    ) {
        let mut config = WatchdogConfig::default();
        config.tier_scope_limits.insert("worker".into(), 3);

        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let daemon = ScopeId("daemon:host".into());
        tree.register(daemon.clone(), ScopeTier::Daemon, &ScopeId::root(), "host", 1000).unwrap();
        tree.start(&daemon, 1100).unwrap();

        for i in 0..n_workers {
            tree.register(
                ScopeId(format!("worker:w{i}")),
                ScopeTier::Worker,
                &daemon,
                format!("w{i}"),
                1000,
            ).unwrap();
        }

        let mut watchdog = ScopeWatchdog::with_config(config);
        let alerts = watchdog.scan(&tree, 2000);
        let summary = ScanSummary::from_alerts(&alerts, 2000);

        let any_error_or_crit = alerts.iter().any(|a|
            a.severity == AlertSeverity::Error || a.severity == AlertSeverity::Critical
        );
        prop_assert_eq!(summary.has_errors(), any_error_or_crit);
    }

    // ── scope_limit_for_tier returns MAX for unconfigured ──────────────

    #[test]
    fn unconfigured_tier_limit_is_max(_dummy in 0u8..1) {
        let mut config = WatchdogConfig::default();
        config.tier_scope_limits.clear();

        let limit = config.scope_limit_for_tier(ScopeTier::Worker);
        prop_assert_eq!(limit, usize::MAX);
    }

    // ── grace_period_for_tier falls back to default ────────────────────

    #[test]
    fn grace_period_fallback_to_default(
        default_ms in 1000u64..60_000,
    ) {
        let mut config = WatchdogConfig::default();
        config.tier_grace_periods.clear();
        config.default_grace_period_ms = default_ms;

        let grace = config.grace_period_for_tier(ScopeTier::Daemon);
        prop_assert_eq!(grace, default_ms);
    }

    // ── Canonical string changes after scan ────────────────────────────

    #[test]
    fn canonical_string_updates_after_scan(_dummy in 0u8..1) {
        let mut tree = ScopeTree::new(1000);
        tree.start(&ScopeId::root(), 1000).unwrap();

        let mut watchdog = ScopeWatchdog::new();
        let before = watchdog.canonical_string();
        let _ = watchdog.scan(&tree, 2000);
        let after = watchdog.canonical_string();

        let check_scans = after.contains("scans=1");
        prop_assert_ne!(before, after, "canonical string should change after scan");
        prop_assert!(check_scans, "should show scans=1 after one scan");
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
    tree.register(
        stuck.clone(),
        ScopeTier::Daemon,
        &ScopeId::root(),
        "stuck",
        1000,
    )
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
    tree.register(
        stale.clone(),
        ScopeTier::Daemon,
        &ScopeId::root(),
        "stale",
        1000,
    )
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
            cycle: vec![
                ScopeId("a".into()),
                ScopeId("b".into()),
                ScopeId("a".into()),
            ],
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
        assert!(
            !display.is_empty(),
            "display should be non-empty for {:?}",
            kind
        );
    }
}

#[test]
fn watchdog_canonical_string_deterministic() {
    let wd = ScopeWatchdog::new();
    assert_eq!(wd.canonical_string(), wd.canonical_string());
    assert!(wd.canonical_string().contains("scans=0"));
}
