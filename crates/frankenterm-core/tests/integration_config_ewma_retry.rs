//! Integration test: config filtering → EWMA rate tracking → retry policy.
//!
//! Exercises the cross-module flow where configuration drives pane selection,
//! EWMA smoothes their output metrics, and retry policies adapt to pane
//! priority and observed anomalies:
//!
//!   PaneFilterConfig.check_pane(domain, title, cwd)
//!     → PanePriorityConfig.priority_for_pane(...)   (assign priority)
//!       → Ewma/EwmaWithVariance.observe(rate, time)  (track output)
//!         → RateEstimator.tick(time)                  (estimate event rate)
//!           → RetryPolicy.delay_for_attempt(n)        (schedule retries)
//!
//! This mirrors a real ingestion loop: config filters unwanted panes, assigns
//! priority to the rest, EWMA tracks output health, and retry backoff adapts
//! to each pane's importance (high-priority panes get more aggressive retry).

use std::time::Duration;

use frankenterm_core::config::{
    PaneFilterConfig, PaneFilterRule, PanePriorityConfig, PanePriorityRule,
};
use frankenterm_core::ewma::{EwmaWithVariance, RateEstimator};
use frankenterm_core::retry::RetryPolicy;

// ── Helpers ─────────────────────────────────────────────────────────────

/// Build a filter config that excludes panes in /tmp and SSH wildcard.
fn test_filter_config() -> PaneFilterConfig {
    PaneFilterConfig {
        include: vec![],
        exclude: vec![
            PaneFilterRule::new("exclude-tmp").with_cwd("/tmp/*"),
            PaneFilterRule::new("exclude-ssh").with_domain("SSH:*"),
        ],
    }
}

/// Build a priority config: "vim" title → priority 1 (highest),
/// /home/user/projects → priority 5, default → 10.
fn test_priority_config() -> PanePriorityConfig {
    PanePriorityConfig {
        default_priority: 10,
        rules: vec![
            PanePriorityRule {
                matcher: PaneFilterRule::new("editor").with_title("vim"),
                priority: 1,
            },
            PanePriorityRule {
                matcher: PaneFilterRule::new("projects").with_cwd("/home/user/projects"),
                priority: 5,
            },
        ],
    }
}

/// Choose a retry policy based on pane priority (lower = more important).
fn retry_for_priority(priority: u32) -> RetryPolicy {
    match priority {
        0..=2 => RetryPolicy {
            initial_delay: Duration::from_millis(50),
            max_delay: Duration::from_secs(2),
            backoff_factor: 1.5,
            jitter_percent: 0.0, // deterministic for testing
            max_attempts: Some(5),
        },
        3..=7 => RetryPolicy {
            initial_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: Some(3),
        },
        _ => RetryPolicy {
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            backoff_factor: 2.0,
            jitter_percent: 0.0,
            max_attempts: Some(2),
        },
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

/// Filter config correctly excludes panes, and only non-excluded panes
/// proceed to priority assignment and EWMA tracking.
#[test]
fn filter_gates_priority_and_tracking() {
    let filter = test_filter_config();
    let priority_cfg = test_priority_config();

    // These should be filtered out.
    assert!(filter.check_pane("local", "bash", "/tmp/scratch").is_some());
    assert!(
        filter
            .check_pane("SSH:remote", "zsh", "/home/user")
            .is_some()
    );

    // These pass the filter.
    assert!(
        filter
            .check_pane("local", "vim", "/home/user/projects")
            .is_none()
    );
    assert!(filter.check_pane("local", "bash", "/home/user").is_none());

    // Priority only makes sense for non-excluded panes.
    let vim_priority = priority_cfg.priority_for_pane("local", "vim", "/home/user/projects");
    assert_eq!(vim_priority, 1, "vim should get highest priority");

    let bash_priority = priority_cfg.priority_for_pane("local", "bash", "/home/user");
    assert_eq!(bash_priority, 10, "unmatched pane gets default priority");

    let projects_priority = priority_cfg.priority_for_pane("local", "bash", "/home/user/projects");
    assert_eq!(
        projects_priority, 5,
        "projects cwd matches mid-priority rule"
    );
}

/// Priority drives retry aggressiveness: high-priority panes get faster
/// retries with more attempts.
#[test]
fn priority_drives_retry_aggressiveness() {
    let priority_cfg = test_priority_config();

    // High priority (vim editor) → aggressive retry.
    let vim_priority = priority_cfg.priority_for_pane("local", "vim", "/home/user/projects");
    let vim_retry = retry_for_priority(vim_priority);
    assert_eq!(vim_retry.max_attempts, Some(5));
    assert_eq!(vim_retry.initial_delay, Duration::from_millis(50));

    // Medium priority (projects dir).
    let proj_priority = priority_cfg.priority_for_pane("local", "bash", "/home/user/projects");
    let proj_retry = retry_for_priority(proj_priority);
    assert_eq!(proj_retry.max_attempts, Some(3));
    assert_eq!(proj_retry.initial_delay, Duration::from_millis(200));

    // Low priority (default).
    let default_priority = priority_cfg.priority_for_pane("local", "bash", "/var/log");
    let default_retry = retry_for_priority(default_priority);
    assert_eq!(default_retry.max_attempts, Some(2));
    assert_eq!(default_retry.initial_delay, Duration::from_millis(500));

    // Verify backoff progression for each tier.
    // High priority: 50ms, 75ms, 112ms, 168ms, 253ms (×1.5 each).
    let d0 = vim_retry.delay_for_attempt(0);
    let d1 = vim_retry.delay_for_attempt(1);
    assert_eq!(d0, Duration::from_millis(50));
    assert_eq!(d1, Duration::from_millis(75));

    // Low priority: 500ms, 1000ms (×2.0).
    let d0 = default_retry.delay_for_attempt(0);
    let d1 = default_retry.delay_for_attempt(1);
    assert_eq!(d0, Duration::from_millis(500));
    assert_eq!(d1, Duration::from_secs(1));
}

/// EWMA tracks output rates for panes that pass the filter, and the
/// smoothed rate reflects the actual throughput pattern.
#[test]
fn ewma_tracks_output_rate_for_filtered_panes() {
    let filter = test_filter_config();

    // Simulate 3 panes: 2 pass filter, 1 excluded.
    struct PaneState {
        domain: &'static str,
        title: &'static str,
        cwd: &'static str,
        rate_estimator: Option<RateEstimator>,
    }

    let mut panes = vec![
        PaneState {
            domain: "local",
            title: "vim",
            cwd: "/home/user/projects",
            rate_estimator: None,
        },
        PaneState {
            domain: "local",
            title: "bash",
            cwd: "/home/user",
            rate_estimator: None,
        },
        PaneState {
            domain: "SSH:remote",
            title: "zsh",
            cwd: "/root",
            rate_estimator: None,
        },
    ];

    // Initialize rate estimators only for panes that pass filter.
    for pane in &mut panes {
        if filter
            .check_pane(pane.domain, pane.title, pane.cwd)
            .is_none()
        {
            pane.rate_estimator = Some(RateEstimator::with_half_life_ms(5000.0));
        }
    }

    // Excluded pane should have no estimator.
    assert!(panes[0].rate_estimator.is_some(), "vim should pass filter");
    assert!(panes[1].rate_estimator.is_some(), "bash should pass filter");
    assert!(
        panes[2].rate_estimator.is_none(),
        "SSH pane should be excluded"
    );

    // Simulate events at 10 events/sec for vim (every 100ms).
    for i in 0..20 {
        if let Some(ref mut est) = panes[0].rate_estimator {
            est.tick(1000 + i * 100);
        }
    }

    // Simulate events at 2 events/sec for bash (every 500ms).
    for i in 0..10 {
        if let Some(ref mut est) = panes[1].rate_estimator {
            est.tick(1000 + i * 500);
        }
    }

    // Check estimated rates.
    let vim_rate = panes[0].rate_estimator.as_ref().unwrap().rate_per_sec();
    let bash_rate = panes[1].rate_estimator.as_ref().unwrap().rate_per_sec();

    assert!(
        (vim_rate - 10.0).abs() < 2.0,
        "vim rate should be ~10/sec, got {vim_rate}"
    );
    assert!(
        (bash_rate - 2.0).abs() < 0.5,
        "bash rate should be ~2/sec, got {bash_rate}"
    );

    // Higher rate pane should have more total events.
    assert!(
        panes[0].rate_estimator.as_ref().unwrap().total_events()
            > panes[1].rate_estimator.as_ref().unwrap().total_events()
    );
}

/// EwmaWithVariance detects anomalous output spikes, which could trigger
/// more aggressive retry when combined with priority.
#[test]
fn anomaly_detection_with_priority_escalation() {
    let priority_cfg = test_priority_config();

    // Track output rate variance for a bash pane.
    let mut tracker = EwmaWithVariance::with_half_life_ms(2000.0);

    // Feed stable output with natural variation: ~50 bytes ± small jitter.
    for i in 0..30 {
        // Add small variation so variance builds up (range: 45-55).
        let value = ((i % 5) as f64 - 2.0).mul_add(2.5, 50.0);
        tracker.observe(value, i * 100);
    }

    // The mean should be close to 50.
    let mean = tracker.mean();
    assert!((mean - 50.0).abs() < 5.0, "mean should be ~50, got {mean}");

    // Normal value: not anomalous.
    assert!(
        !tracker.is_anomaly(55.0, 2.0),
        "55 should not be anomalous at 2-sigma"
    );

    // Spike: 500 bytes is far from mean.
    assert!(
        tracker.is_anomaly(500.0, 2.0),
        "500 should be anomalous at 2-sigma"
    );

    // When an anomaly is detected, we might escalate retry priority.
    // Start with default priority for this pane.
    let base_priority = priority_cfg.priority_for_pane("local", "bash", "/home/user");
    assert_eq!(base_priority, 10);

    // Escalate: if anomaly detected, temporarily boost priority.
    let escalated_priority = if tracker.is_anomaly(500.0, 2.0) {
        (base_priority / 2).max(1) // halve priority value (lower = higher)
    } else {
        base_priority
    };
    assert_eq!(escalated_priority, 5);

    // Escalated priority gets more aggressive retry.
    let base_retry = retry_for_priority(base_priority);
    let escalated_retry = retry_for_priority(escalated_priority);
    assert!(
        escalated_retry.max_attempts.unwrap() > base_retry.max_attempts.unwrap(),
        "escalated should have more retry attempts"
    );
    assert!(
        escalated_retry.initial_delay < base_retry.initial_delay,
        "escalated should have shorter initial delay"
    );
}

/// Retry delay progression respects max_delay cap across all priority tiers.
#[test]
fn retry_delay_capped_at_max_for_all_tiers() {
    for priority in [1, 5, 10] {
        let policy = retry_for_priority(priority);
        let max = policy.max_delay;

        // Even at high attempt numbers, delay should never exceed max.
        for attempt in 0..20 {
            let delay = policy.delay_for_attempt(attempt);
            assert!(
                delay <= max,
                "priority {priority}, attempt {attempt}: delay {delay:?} > max {max:?}"
            );
        }
    }
}

/// Full pipeline: filter → priority → EWMA tracking → anomaly-aware retry.
#[test]
fn full_pipeline_filter_priority_ewma_retry() {
    let filter = test_filter_config();
    let priority_cfg = test_priority_config();

    // Simulate a set of panes with different characteristics.
    let panes = [
        ("local", "vim", "/home/user/projects/rust"),
        ("local", "bash", "/home/user"),
        ("local", "htop", "/home/user"),
        ("SSH:staging", "deploy", "/opt/app"), // excluded
        ("local", "cargo", "/tmp/build"),      // excluded
    ];

    let mut active_panes: Vec<(usize, u32, RateEstimator, EwmaWithVariance)> = Vec::new();

    for (idx, &(domain, title, cwd)) in panes.iter().enumerate() {
        // Step 1: Filter.
        if filter.check_pane(domain, title, cwd).is_some() {
            continue; // excluded
        }

        // Step 2: Assign priority.
        let priority = priority_cfg.priority_for_pane(domain, title, cwd);

        // Step 3: Initialize tracking.
        let rate_est = RateEstimator::with_half_life_ms(5000.0);
        let ewma_var = EwmaWithVariance::with_half_life_ms(2000.0);

        active_panes.push((idx, priority, rate_est, ewma_var));
    }

    // Should have 3 active panes (vim, bash, htop).
    assert_eq!(active_panes.len(), 3);

    // Verify priorities.
    assert_eq!(active_panes[0].1, 1); // vim → highest
    assert_eq!(active_panes[1].1, 10); // bash → default
    assert_eq!(active_panes[2].1, 10); // htop → default

    // Simulate output: vim gets high rate, bash medium, htop low.
    let rates = [100u64, 500, 2000]; // ms between events
    for (i, (_, _, rate_est, ewma_var)) in active_panes.iter_mut().enumerate() {
        let interval = rates[i];
        for j in 0..20u64 {
            let t = 1000 + j * interval;
            rate_est.tick(t);
            ewma_var.observe(50.0 + (j % 3) as f64, t);
        }
    }

    // vim has highest rate (~10/s), htop lowest (~0.5/s).
    let vim_rate = active_panes[0].2.rate_per_sec();
    let htop_rate = active_panes[2].2.rate_per_sec();
    assert!(
        vim_rate > htop_rate,
        "vim ({vim_rate:.1}/s) should be faster than htop ({htop_rate:.1}/s)"
    );

    // Step 4: Choose retry policy based on priority.
    for (_, priority, _, _) in &active_panes {
        let policy = retry_for_priority(*priority);
        // All policies should have valid configurations.
        assert!(policy.max_attempts.unwrap() >= 2);
        assert!(policy.initial_delay >= Duration::from_millis(50));
        assert!(policy.max_delay >= policy.initial_delay);
    }

    // Inject anomaly on the htop pane and verify escalation would change retry.
    let (_, htop_priority, _, ref htop_tracker) = active_panes[2];
    let normal_retry = retry_for_priority(htop_priority);

    // Simulate anomalous value.
    let is_anomalous = htop_tracker.is_anomaly(1000.0, 2.0);
    assert!(is_anomalous, "1000 should be anomalous against ~50 mean");

    let escalated_priority = if is_anomalous {
        (htop_priority / 2).max(1)
    } else {
        htop_priority
    };
    let escalated_retry = retry_for_priority(escalated_priority);

    assert!(
        escalated_retry.max_attempts.unwrap() >= normal_retry.max_attempts.unwrap(),
        "anomaly escalation should not reduce retry attempts"
    );
}
