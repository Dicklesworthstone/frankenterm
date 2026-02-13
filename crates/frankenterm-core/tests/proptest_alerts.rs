//! Property-based tests for alerts module.
//!
//! Verifies alert system invariants:
//! - AlertPeriod: as_str/FromStr roundtrip, Display matches as_str, serde,
//!   duration_ms positive and strictly increasing
//! - AlertLevel: ordering, from_percent monotonic, as_str snake_case, serde
//! - AlertMetric: as_str/FromStr roundtrip, Display matches as_str, serde
//! - AlertRule: serde roundtrip, disabled rule never triggers, zero threshold
//!   never triggers, check monotonic in value for cost/token/ratelimit rules
//! - TriggeredAlert: serde roundtrip, summary non-empty
//! - AlertMonitor: add increases count, remove decreases count

use proptest::prelude::*;

use frankenterm_core::alerts::{
    AlertLevel, AlertMetric, AlertMonitor, AlertPeriod, AlertRule, TriggeredAlert,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_period() -> impl Strategy<Value = AlertPeriod> {
    prop_oneof![
        Just(AlertPeriod::Day),
        Just(AlertPeriod::Week),
        Just(AlertPeriod::Month),
    ]
}

fn arb_level() -> impl Strategy<Value = AlertLevel> {
    prop_oneof![
        Just(AlertLevel::Info),
        Just(AlertLevel::Warning),
        Just(AlertLevel::Critical),
        Just(AlertLevel::Exceeded),
    ]
}

fn arb_metric() -> impl Strategy<Value = AlertMetric> {
    prop_oneof![
        Just(AlertMetric::Cost),
        Just(AlertMetric::TokenUsage),
        Just(AlertMetric::RateLimitFrequency),
        Just(AlertMetric::AccountBalance),
    ]
}

fn arb_rule() -> impl Strategy<Value = AlertRule> {
    (
        "[a-z]{3,10}",                  // id
        arb_metric(),                   // metric
        0.01f64..=100_000.0,            // threshold
        arb_period(),                   // period
        prop::option::of("[a-z]{3,8}"), // agent_type
        prop::option::of("[a-z]{3,8}"), // account_id
        prop::option::of("[a-z]{3,8}"), // service
        prop::bool::ANY,                // enabled
    )
        .prop_map(
            |(id, metric, threshold, period, agent, account, service, enabled)| AlertRule {
                id,
                metric,
                threshold,
                period,
                agent_type: agent,
                account_id: account,
                service,
                enabled,
            },
        )
}

fn arb_triggered_alert() -> impl Strategy<Value = TriggeredAlert> {
    (
        "[a-z]{3,10}",       // rule_id
        arb_metric(),        // metric
        arb_level(),         // level
        0.0f64..=100_000.0,  // current_value
        0.01f64..=100_000.0, // threshold
        0.0f64..=2.0,        // percent_of_threshold
        arb_period(),        // period
        0i64..=i64::MAX / 2, // evaluated_at
    )
        .prop_map(
            |(rule_id, metric, level, current, threshold, pct, period, ts)| TriggeredAlert {
                rule_id,
                metric,
                level,
                current_value: current,
                threshold,
                percent_of_threshold: pct,
                period,
                evaluated_at: ts,
            },
        )
}

// ────────────────────────────────────────────────────────────────────
// AlertPeriod: as_str/FromStr roundtrip, Display, serde, duration_ms
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// as_str/FromStr roundtrip preserves value.
    #[test]
    fn prop_period_str_roundtrip(p in arb_period()) {
        let s = p.as_str();
        let back: AlertPeriod = s.parse().unwrap();
        prop_assert_eq!(back, p);
    }

    /// Display matches as_str.
    #[test]
    fn prop_period_display_matches_str(p in arb_period()) {
        let display = p.to_string();
        prop_assert_eq!(display, p.as_str());
    }

    /// Serde roundtrip.
    #[test]
    fn prop_period_serde_roundtrip(p in arb_period()) {
        let json = serde_json::to_string(&p).unwrap();
        let back: AlertPeriod = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, p);
    }

    /// duration_ms is positive.
    #[test]
    fn prop_period_duration_positive(p in arb_period()) {
        prop_assert!(p.duration_ms() > 0, "duration_ms should be positive");
    }

    /// duration_ms: Day < Week < Month.
    #[test]
    fn prop_period_duration_ordering(_dummy in 0..1u32) {
        prop_assert!(AlertPeriod::Day.duration_ms() < AlertPeriod::Week.duration_ms());
        prop_assert!(AlertPeriod::Week.duration_ms() < AlertPeriod::Month.duration_ms());
    }
}

// ────────────────────────────────────────────────────────────────────
// AlertLevel: ordering, from_percent, as_str, serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// AlertLevel ordering: Info < Warning < Critical < Exceeded.
    #[test]
    fn prop_level_ordering(l1 in arb_level(), l2 in arb_level()) {
        let vals = [AlertLevel::Info, AlertLevel::Warning, AlertLevel::Critical, AlertLevel::Exceeded];
        let idx1 = vals.iter().position(|&v| v == l1).unwrap();
        let idx2 = vals.iter().position(|&v| v == l2).unwrap();
        match idx1.cmp(&idx2) {
            std::cmp::Ordering::Less => prop_assert!(l1 < l2),
            std::cmp::Ordering::Equal => prop_assert_eq!(l1, l2),
            std::cmp::Ordering::Greater => prop_assert!(l1 > l2),
        }
    }

    /// from_percent is monotonically non-decreasing.
    #[test]
    fn prop_from_percent_monotonic(p1 in 0.0f64..=2.0, p2 in 0.0f64..=2.0) {
        if p1 <= p2 {
            let l1 = AlertLevel::from_percent(p1);
            let l2 = AlertLevel::from_percent(p2);
            // If l1 is Some, l2 must be Some and >= l1
            if let (Some(v1), Some(v2)) = (l1, l2) {
                prop_assert!(v1 <= v2, "from_percent({}) = {:?} > from_percent({}) = {:?}",
                    p1, v1, p2, v2);
            }
            // If l1 is Some but l2 is None, that's a violation
            if l1.is_some() {
                prop_assert!(l2.is_some(),
                    "from_percent({}) is Some but from_percent({}) is None", p1, p2);
            }
        }
    }

    /// from_percent(x) is None for x < 0.5.
    #[test]
    fn prop_from_percent_below_half_is_none(p in 0.0f64..0.5) {
        prop_assert!(AlertLevel::from_percent(p).is_none(),
            "from_percent({}) should be None", p);
    }

    /// from_percent(x) is Some for x >= 0.5.
    #[test]
    fn prop_from_percent_at_half_is_some(p in 0.5f64..=2.0) {
        prop_assert!(AlertLevel::from_percent(p).is_some(),
            "from_percent({}) should be Some", p);
    }

    /// from_percent(x) is Exceeded for x >= 1.0.
    #[test]
    fn prop_from_percent_exceeded(p in 1.0f64..=2.0) {
        prop_assert_eq!(
            AlertLevel::from_percent(p),
            Some(AlertLevel::Exceeded)
        );
    }

    /// as_str is non-empty and snake_case.
    #[test]
    fn prop_level_as_str_format(l in arb_level()) {
        let s = l.as_str();
        prop_assert!(!s.is_empty());
        prop_assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "as_str '{}' should be snake_case", s
        );
    }

    /// Display matches as_str.
    #[test]
    fn prop_level_display_matches_str(l in arb_level()) {
        let display = l.to_string();
        prop_assert_eq!(display, l.as_str());
    }

    /// Serde roundtrip.
    #[test]
    fn prop_level_serde_roundtrip(l in arb_level()) {
        let json = serde_json::to_string(&l).unwrap();
        let back: AlertLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, l);
    }
}

// ────────────────────────────────────────────────────────────────────
// AlertMetric: as_str/FromStr roundtrip, Display, serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// as_str/FromStr roundtrip.
    #[test]
    fn prop_metric_str_roundtrip(m in arb_metric()) {
        let s = m.as_str();
        let back: AlertMetric = s.parse().unwrap();
        prop_assert_eq!(back, m);
    }

    /// Display matches as_str.
    #[test]
    fn prop_metric_display_matches_str(m in arb_metric()) {
        let display = m.to_string();
        prop_assert_eq!(display, m.as_str());
    }

    /// Serde roundtrip.
    #[test]
    fn prop_metric_serde_roundtrip(m in arb_metric()) {
        let json = serde_json::to_string(&m).unwrap();
        let back: AlertMetric = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, m);
    }

    /// as_str is non-empty and snake_case.
    #[test]
    fn prop_metric_as_str_format(m in arb_metric()) {
        let s = m.as_str();
        prop_assert!(!s.is_empty());
        prop_assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "as_str '{}' should be snake_case", s
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// AlertRule: serde, disabled/zero, check monotonicity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// AlertRule serde roundtrip preserves key fields.
    #[test]
    fn prop_rule_serde_roundtrip(r in arb_rule()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: AlertRule = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.id, r.id);
        prop_assert_eq!(back.metric, r.metric);
        prop_assert!((back.threshold - r.threshold).abs() < 1e-9);
        prop_assert_eq!(back.period, r.period);
        prop_assert_eq!(back.enabled, r.enabled);
        prop_assert_eq!(back.agent_type, r.agent_type);
        prop_assert_eq!(back.account_id, r.account_id);
        prop_assert_eq!(back.service, r.service);
    }

    /// Disabled rule never triggers regardless of value.
    #[test]
    fn prop_rule_disabled_never_triggers(
        threshold in 0.01f64..=1000.0,
        value in 0.0f64..=10000.0,
    ) {
        let mut rule = AlertRule::cost("test", threshold, AlertPeriod::Day);
        rule.enabled = false;
        prop_assert!(rule.check(value).is_none(),
            "disabled rule should not trigger for value {}", value);
    }

    /// Zero threshold never triggers.
    #[test]
    fn prop_rule_zero_threshold_never_triggers(value in 0.0f64..=10000.0) {
        let rule = AlertRule::cost("test", 0.0, AlertPeriod::Day);
        prop_assert!(rule.check(value).is_none(),
            "zero threshold rule should not trigger for value {}", value);
    }

    /// For cost/token/ratelimit metrics, check() is monotonically non-decreasing in value.
    #[test]
    fn prop_rule_check_monotonic_cost(
        threshold in 0.01f64..=1000.0,
        v1 in 0.0f64..=2000.0,
        v2 in 0.0f64..=2000.0,
    ) {
        let rule = AlertRule::cost("test", threshold, AlertPeriod::Day);
        if v1 <= v2 {
            let l1 = rule.check(v1);
            let l2 = rule.check(v2);
            if let (Some(lv1), Some(lv2)) = (l1, l2) {
                prop_assert!(lv1 <= lv2,
                    "check({}) = {:?} > check({}) = {:?}", v1, lv1, v2, lv2);
            }
            if l1.is_some() {
                prop_assert!(l2.is_some(),
                    "check({}) is Some but check({}) is None", v1, v2);
            }
        }
    }

    /// Cost rule with value < 50% threshold returns None.
    #[test]
    fn prop_rule_below_half_is_none(
        threshold in 1.0f64..=1000.0,
        frac in 0.0f64..0.5,
    ) {
        let value = threshold * frac;
        let rule = AlertRule::cost("test", threshold, AlertPeriod::Day);
        // Value at frac < 50% should be None
        // (need frac * threshold / threshold = frac < 0.5)
        prop_assert!(rule.check(value).is_none(),
            "value {} ({}% of threshold {}) should not trigger",
            value, frac * 100.0, threshold);
    }
}

// ────────────────────────────────────────────────────────────────────
// TriggeredAlert: serde, summary
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// TriggeredAlert serde roundtrip.
    #[test]
    fn prop_triggered_serde_roundtrip(a in arb_triggered_alert()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: TriggeredAlert = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.rule_id, a.rule_id);
        prop_assert_eq!(back.metric, a.metric);
        prop_assert_eq!(back.level, a.level);
        prop_assert!((back.current_value - a.current_value).abs() < 1e-9);
        prop_assert!((back.threshold - a.threshold).abs() < 1e-9);
        prop_assert_eq!(back.period, a.period);
        prop_assert_eq!(back.evaluated_at, a.evaluated_at);
    }

    /// summary() is non-empty.
    #[test]
    fn prop_triggered_summary_nonempty(a in arb_triggered_alert()) {
        let s = a.summary();
        prop_assert!(!s.is_empty(), "summary should not be empty");
    }

    /// summary() contains the level string.
    #[test]
    fn prop_triggered_summary_contains_level(a in arb_triggered_alert()) {
        let s = a.summary();
        prop_assert!(s.contains(a.level.as_str()),
            "summary '{}' should contain level '{}'", s, a.level.as_str());
    }
}

// ────────────────────────────────────────────────────────────────────
// AlertMonitor: add/remove rules
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Adding N rules gives N rules.
    #[test]
    fn prop_monitor_add_count(n in 1usize..=15) {
        let rules: Vec<AlertRule> = (0..n)
            .map(|i| AlertRule::cost(format!("r{}", i), 50.0, AlertPeriod::Day))
            .collect();
        let monitor = AlertMonitor::new(rules);
        prop_assert_eq!(monitor.rules().len(), n);
    }

    /// Removing a rule decreases count by 1.
    #[test]
    fn prop_monitor_remove_decreases(n in 2usize..=10) {
        let rules: Vec<AlertRule> = (0..n)
            .map(|i| AlertRule::cost(format!("r{}", i), 50.0, AlertPeriod::Day))
            .collect();
        let mut monitor = AlertMonitor::new(rules);
        let removed = monitor.remove_rule("r0");
        prop_assert!(removed);
        prop_assert_eq!(monitor.rules().len(), n - 1);
    }

    /// Removing nonexistent rule returns false and doesn't change count.
    #[test]
    fn prop_monitor_remove_nonexistent(n in 1usize..=10) {
        let rules: Vec<AlertRule> = (0..n)
            .map(|i| AlertRule::cost(format!("r{}", i), 50.0, AlertPeriod::Day))
            .collect();
        let mut monitor = AlertMonitor::new(rules);
        let removed = monitor.remove_rule("nonexistent");
        prop_assert!(!removed);
        prop_assert_eq!(monitor.rules().len(), n);
    }

    /// add_rule increases count by 1.
    #[test]
    fn prop_monitor_add_rule_increments(n in 0usize..=10) {
        let rules: Vec<AlertRule> = (0..n)
            .map(|i| AlertRule::cost(format!("r{}", i), 50.0, AlertPeriod::Day))
            .collect();
        let mut monitor = AlertMonitor::new(rules);
        monitor.add_rule(AlertRule::cost("new", 100.0, AlertPeriod::Week));
        prop_assert_eq!(monitor.rules().len(), n + 1);
    }
}
