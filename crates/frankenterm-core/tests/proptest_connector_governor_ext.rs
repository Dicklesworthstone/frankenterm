//! Extended property-based tests for connector_governor module.
//!
//! Supplements proptest_connector_governor.rs with coverage for:
//! - QueueBackpressure invariants (enqueue/dequeue, zones, peak tracking)
//! - TokenBucket edge cases (consume_n, zero-capacity, time_until_available)
//! - AdaptiveBackoff delay capping and exponent saturation
//! - GovernorDecision constructor invariants
//! - GovernorVerdict/GovernorReason Display stability
//! - Governor evaluation priority order
//! - Cost budget window GC and snapshot consistency
//! - Quota total_actions monotonicity

use frankenterm_core::connector_governor::*;
use frankenterm_core::connector_outbound_bridge::{ConnectorAction, ConnectorActionKind};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_action_kind() -> impl Strategy<Value = ConnectorActionKind> {
    prop_oneof![
        Just(ConnectorActionKind::Notify),
        Just(ConnectorActionKind::Ticket),
        Just(ConnectorActionKind::TriggerWorkflow),
        Just(ConnectorActionKind::AuditLog),
        Just(ConnectorActionKind::Invoke),
        Just(ConnectorActionKind::CredentialAction),
    ]
}

fn arb_verdict() -> impl Strategy<Value = GovernorVerdict> {
    prop_oneof![
        Just(GovernorVerdict::Allow),
        Just(GovernorVerdict::Throttle),
        Just(GovernorVerdict::Reject),
    ]
}

fn arb_reason() -> impl Strategy<Value = GovernorReason> {
    prop_oneof![
        Just(GovernorReason::Clear),
        Just(GovernorReason::ConnectorRateLimit),
        Just(GovernorReason::GlobalRateLimit),
        Just(GovernorReason::ConnectorQuotaExhausted),
        Just(GovernorReason::GlobalQuotaExhausted),
        Just(GovernorReason::BudgetExceeded),
        Just(GovernorReason::Backpressure),
        Just(GovernorReason::AdaptiveBackoff),
    ]
}

fn arb_connector_id() -> impl Strategy<Value = String> {
    "[a-z]{3,10}"
}

fn make_action(connector: &str, kind: ConnectorActionKind, ts: u64) -> ConnectorAction {
    ConnectorAction {
        target_connector: connector.to_string(),
        action_kind: kind,
        correlation_id: "test".to_string(),
        params: serde_json::json!({}),
        created_at_ms: ts,
    }
}

// =============================================================================
// QueueBackpressure Properties
// =============================================================================

proptest! {
    /// Enqueue always increases depth by 1
    #[test]
    fn queue_enqueue_increments_depth(n in 1usize..100) {
        let mut q = QueueBackpressure::new(QueueBackpressureConfig::default());
        for _ in 0..n {
            q.record_enqueue();
        }
        prop_assert_eq!(q.current_depth(), n);
    }

    /// Dequeue never underflows (saturates at 0)
    #[test]
    fn queue_dequeue_saturates_at_zero(
        enqueues in 0usize..20,
        dequeues in 0usize..40,
    ) {
        let mut q = QueueBackpressure::new(QueueBackpressureConfig::default());
        for _ in 0..enqueues {
            q.record_enqueue();
        }
        for _ in 0..dequeues {
            q.record_dequeue();
        }
        // depth can't go negative (it's usize with saturating_sub)
        let expected = enqueues.saturating_sub(dequeues);
        prop_assert_eq!(q.current_depth(), expected);
    }

    /// Peak depth is always >= current depth
    #[test]
    fn queue_peak_gte_current(
        ops in prop::collection::vec(prop::bool::ANY, 1..100),
    ) {
        let mut q = QueueBackpressure::new(QueueBackpressureConfig::default());
        for op in &ops {
            if *op {
                q.record_enqueue();
            } else {
                q.record_dequeue();
            }
        }
        prop_assert!(q.peak_depth() >= q.current_depth(),
            "peak {} < current {}", q.peak_depth(), q.current_depth());
    }

    /// depth_fraction is between 0.0 and some reasonable upper bound
    #[test]
    fn queue_depth_fraction_bounded(
        max_depth in 1usize..10_000,
        enqueues in 0usize..200,
    ) {
        let config = QueueBackpressureConfig {
            max_queue_depth: max_depth,
            throttle_threshold: 0.7,
            reject_threshold: 0.9,
        };
        let mut q = QueueBackpressure::new(config);
        for _ in 0..enqueues {
            q.record_enqueue();
        }
        let frac = q.depth_fraction();
        prop_assert!(frac >= 0.0, "depth_fraction negative: {}", frac);
    }

    /// Throttle zone implies depth >= throttle_threshold * max_depth
    #[test]
    fn queue_throttle_zone_consistent(
        max_depth in 10usize..1000,
        fill_pct in 0usize..120,
    ) {
        let config = QueueBackpressureConfig {
            max_queue_depth: max_depth,
            throttle_threshold: 0.7,
            reject_threshold: 0.9,
        };
        let mut q = QueueBackpressure::new(config);
        let target = max_depth * fill_pct / 100;
        for _ in 0..target {
            q.record_enqueue();
        }
        let frac = q.depth_fraction();
        if frac >= 0.7 {
            prop_assert!(q.should_throttle());
        }
        if frac < 0.7 {
            prop_assert!(!q.should_throttle());
        }
    }

    /// Reject zone implies should_throttle is also true
    #[test]
    fn queue_reject_implies_throttle(
        max_depth in 10usize..1000,
        fill_pct in 0usize..120,
    ) {
        let config = QueueBackpressureConfig {
            max_queue_depth: max_depth,
            throttle_threshold: 0.7,
            reject_threshold: 0.9,
        };
        let mut q = QueueBackpressure::new(config);
        let target = max_depth * fill_pct / 100;
        for _ in 0..target {
            q.record_enqueue();
        }
        if q.should_reject() {
            prop_assert!(q.should_throttle(),
                "reject zone should imply throttle zone");
        }
    }

    /// Snapshot preserves current and peak depth
    #[test]
    fn queue_snapshot_consistent(
        enqueues in 0usize..50,
        dequeues in 0usize..30,
    ) {
        let mut q = QueueBackpressure::new(QueueBackpressureConfig::default());
        for _ in 0..enqueues {
            q.record_enqueue();
        }
        for _ in 0..dequeues {
            q.record_dequeue();
        }
        let snap = q.snapshot();
        prop_assert_eq!(snap.current_depth, q.current_depth());
        prop_assert_eq!(snap.peak_depth, q.peak_depth());
        prop_assert_eq!(snap.total_enqueued, enqueues as u64);
    }
}

// =============================================================================
// TokenBucket Extended Properties
// =============================================================================

proptest! {
    /// consume_n with n > capacity always fails
    #[test]
    fn token_bucket_consume_n_over_capacity_fails(
        capacity in 1u64..500,
        rate in 1u64..50,
    ) {
        let config = TokenBucketConfig {
            capacity,
            refill_rate: rate,
            refill_interval_ms: 1000,
        };
        let mut bucket = TokenBucket::new(config);
        let result = bucket.try_consume_n(capacity + 1, 0);
        prop_assert!(!result, "consuming more than capacity should fail");
    }

    /// consume_n with n == capacity succeeds on a full bucket
    #[test]
    fn token_bucket_consume_n_exact_capacity(
        capacity in 1u64..500,
    ) {
        let config = TokenBucketConfig {
            capacity,
            refill_rate: 10,
            refill_interval_ms: 1000,
        };
        let mut bucket = TokenBucket::new(config);
        let result = bucket.try_consume_n(capacity, 0);
        prop_assert!(result, "consuming exact capacity from full bucket should succeed");
        prop_assert_eq!(bucket.available(0), 0);
    }

    /// with_initial clamps to capacity
    #[test]
    fn token_bucket_initial_clamped(
        capacity in 1u64..500,
        initial in 0u64..1000,
    ) {
        let config = TokenBucketConfig {
            capacity,
            refill_rate: 10,
            refill_interval_ms: 1000,
        };
        let mut bucket = TokenBucket::with_initial(config, initial, 0);
        let avail = bucket.available(0);
        prop_assert!(avail <= capacity,
            "initial {} clamped to capacity {}, got {}", initial, capacity, avail);
    }

    /// time_until_available returns 0 when tokens available
    #[test]
    fn token_bucket_time_until_available_zero_when_has_tokens(
        capacity in 1u64..500,
    ) {
        let config = TokenBucketConfig {
            capacity,
            refill_rate: 10,
            refill_interval_ms: 1000,
        };
        let mut bucket = TokenBucket::new(config);
        prop_assert_eq!(bucket.time_until_available(0), 0);
    }

    /// time_until_available > 0 when empty and refill_rate > 0
    #[test]
    fn token_bucket_time_until_available_positive_when_empty(
        refill_interval in 100u64..10_000,
    ) {
        let config = TokenBucketConfig {
            capacity: 10,
            refill_rate: 1,
            refill_interval_ms: refill_interval,
        };
        let mut bucket = TokenBucket::with_initial(config, 0, 0);
        let wait = bucket.time_until_available(0);
        prop_assert!(wait > 0, "should need to wait when empty, got 0");
        prop_assert!(wait <= refill_interval,
            "wait {} should be <= refill_interval {}", wait, refill_interval);
    }

    /// Capacity accessor matches config
    #[test]
    fn token_bucket_capacity_matches_config(
        capacity in 1u64..10_000,
    ) {
        let config = TokenBucketConfig {
            capacity,
            refill_rate: 10,
            refill_interval_ms: 1000,
        };
        let bucket = TokenBucket::new(config);
        prop_assert_eq!(bucket.capacity(), capacity);
    }
}

// =============================================================================
// AdaptiveBackoff Extended Properties
// =============================================================================

proptest! {
    /// Backoff delay is always <= max_delay_ms
    #[test]
    fn backoff_delay_capped_at_max(
        base_ms in 100u64..5000,
        max_ms in 5000u64..120_000,
        failures in 1u32..30,
    ) {
        let mut b = AdaptiveBackoff::new(base_ms, max_ms, 2.0);
        for i in 0..failures {
            b.record_failure(i as u64 * 1000);
        }
        let remaining = b.remaining_ms(failures as u64 * 1000);
        prop_assert!(remaining <= max_ms,
            "remaining {} > max {}", remaining, max_ms);
    }

    /// After success, not active regardless of time
    #[test]
    fn backoff_inactive_after_success(
        failures in 1u32..20,
        now_ms in 0u64..1_000_000,
    ) {
        let mut b = AdaptiveBackoff::connector_default();
        for i in 0..failures {
            b.record_failure(i as u64 * 1000);
        }
        b.record_success();
        prop_assert!(!b.is_active(now_ms));
        prop_assert_eq!(b.remaining_ms(now_ms), 0);
    }

    /// Consecutive failures counter tracks correctly
    #[test]
    fn backoff_failure_counter_accurate(failures in 0u32..50) {
        let mut b = AdaptiveBackoff::connector_default();
        for i in 0..failures {
            b.record_failure(i as u64 * 1000);
        }
        prop_assert_eq!(b.consecutive_failures(), failures);
    }

    /// Multiple success calls are idempotent
    #[test]
    fn backoff_success_idempotent(successes in 1u32..10) {
        let mut b = AdaptiveBackoff::connector_default();
        b.record_failure(0);
        for _ in 0..successes {
            b.record_success();
        }
        prop_assert_eq!(b.consecutive_failures(), 0);
    }
}

// =============================================================================
// GovernorDecision Invariants
// =============================================================================

proptest! {
    /// Allow decisions always have delay_ms = 0
    #[test]
    fn decision_allow_zero_delay(
        connector in arb_connector_id(),
        kind in arb_action_kind(),
        now_ms in 0u64..1_000_000,
    ) {
        let kind_str = match kind {
            ConnectorActionKind::Notify => "notify",
            ConnectorActionKind::Ticket => "ticket",
            ConnectorActionKind::TriggerWorkflow => "trigger_workflow",
            ConnectorActionKind::AuditLog => "audit_log",
            ConnectorActionKind::Invoke => "invoke",
            ConnectorActionKind::CredentialAction => "credential_action",
        };
        let d = GovernorDecision::allow(&connector, kind_str, now_ms);
        prop_assert_eq!(d.delay_ms, 0);
        prop_assert!(d.is_allowed());
        prop_assert!(!d.is_rejected());
        prop_assert_eq!(d.verdict, GovernorVerdict::Allow);
        prop_assert_eq!(d.reason, GovernorReason::Clear);
    }

    /// Reject decisions always have delay_ms = 0
    #[test]
    fn decision_reject_zero_delay(
        connector in arb_connector_id(),
        reason in arb_reason(),
        now_ms in 0u64..1_000_000,
    ) {
        let d = GovernorDecision::reject(&connector, "invoke", reason, now_ms);
        prop_assert_eq!(d.delay_ms, 0);
        prop_assert!(!d.is_allowed());
        prop_assert!(d.is_rejected());
    }

    /// Throttle decisions have is_allowed=true and is_rejected=false
    #[test]
    fn decision_throttle_is_allowed_not_rejected(
        connector in arb_connector_id(),
        reason in arb_reason(),
        delay in 1u64..100_000,
        now_ms in 0u64..1_000_000,
    ) {
        let d = GovernorDecision::throttle(&connector, "notify", reason, delay, now_ms);
        prop_assert!(d.is_allowed());
        prop_assert!(!d.is_rejected());
        prop_assert_eq!(d.delay_ms, delay);
    }

    /// Decision preserves connector_id
    #[test]
    fn decision_preserves_connector_id(
        connector in arb_connector_id(),
        now_ms in 0u64..1_000_000,
    ) {
        let d = GovernorDecision::allow(&connector, "notify", now_ms);
        prop_assert_eq!(d.connector_id, connector);
    }
}

// =============================================================================
// GovernorVerdict & GovernorReason Display Stability
// =============================================================================

proptest! {
    /// Verdict Display roundtrips (Display output is non-empty and stable)
    #[test]
    fn verdict_display_nonempty(v in arb_verdict()) {
        let s = v.to_string();
        prop_assert!(!s.is_empty());
        // Verify serde and display consistency
        let json = serde_json::to_string(&v).unwrap();
        let back: GovernorVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, back);
    }

    /// Reason Display roundtrips (Display output is non-empty and stable)
    #[test]
    fn reason_display_nonempty(r in arb_reason()) {
        let s = r.to_string();
        prop_assert!(!s.is_empty());
        let json = serde_json::to_string(&r).unwrap();
        let back: GovernorReason = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r, back);
    }
}

// =============================================================================
// Quota Tracker Extended Properties
// =============================================================================

proptest! {
    /// total_actions monotonically increases
    #[test]
    fn quota_total_actions_monotonic(
        n in 1usize..100,
    ) {
        let config = QuotaConfig {
            max_actions: 50,
            window_ms: 10_000,
            warning_threshold: 0.8,
        };
        let mut qt = QuotaTracker::new(config);
        let mut prev = 0;
        for i in 0..n {
            qt.record(i as u64 * 100);
            let total = qt.total_actions();
            prop_assert!(total > prev, "total should increase: {} -> {}", prev, total);
            prev = total;
        }
        prop_assert_eq!(qt.total_actions(), n as u64);
    }

    /// Warning threshold: is_warning is true iff usage >= threshold
    #[test]
    fn quota_warning_threshold_consistent(
        max_actions in 10u64..200,
        actions_pct in 0u64..120,
    ) {
        let threshold = 0.8;
        let config = QuotaConfig {
            max_actions,
            window_ms: 1_000_000,
            warning_threshold: threshold,
        };
        let mut qt = QuotaTracker::new(config);
        let target = (max_actions * actions_pct / 100).min(max_actions);
        for i in 0..target {
            qt.record(i * 10);
        }
        let frac = qt.usage_fraction(target * 10);
        if frac >= threshold {
            prop_assert!(qt.is_warning(target * 10));
        } else {
            prop_assert!(!qt.is_warning(target * 10));
        }
    }

    /// used + remaining == max in snapshot
    #[test]
    fn quota_snapshot_sum_invariant(
        max_actions in 1u64..500,
        n_actions in 0u64..100,
    ) {
        let config = QuotaConfig {
            max_actions,
            window_ms: 1_000_000,
            warning_threshold: 0.8,
        };
        let mut qt = QuotaTracker::new(config);
        for i in 0..n_actions {
            qt.record(i * 100);
        }
        let snap = qt.snapshot(n_actions * 100);
        prop_assert_eq!(snap.used + snap.remaining, snap.max,
            "used={} + remaining={} != max={}", snap.used, snap.remaining, snap.max);
    }
}

// =============================================================================
// Cost Budget Extended Properties
// =============================================================================

proptest! {
    /// Window GC frees old records
    #[test]
    fn cost_budget_gc_frees_old(
        max_cost in 100u64..10_000,
        window_ms in 1000u64..10_000,
    ) {
        let mut config = CostBudgetConfig::default();
        config.max_cost_cents = max_cost;
        config.window_ms = window_ms;
        let mut cb = CostBudget::new(config);

        // Record some actions at time 0
        for _ in 0..5 {
            cb.record(&ConnectorActionKind::Notify, 0);
        }
        let cost_before = cb.window_cost(0);
        prop_assert!(cost_before > 0);

        // After window passes, cost should be 0
        let cost_after = cb.window_cost(window_ms + 1);
        prop_assert_eq!(cost_after, 0, "window GC should clear old records");
    }

    /// Snapshot remaining + window_cost == max_cost
    #[test]
    fn cost_budget_snapshot_sum(
        max_cost in 10u64..10_000,
        n_actions in 0usize..20,
    ) {
        let mut config = CostBudgetConfig::default();
        config.max_cost_cents = max_cost;
        let mut cb = CostBudget::new(config);
        for i in 0..n_actions {
            cb.record(&ConnectorActionKind::AuditLog, i as u64 * 100);
        }
        let snap = cb.snapshot(n_actions as u64 * 100);
        // remaining = max - window_cost (via saturating_sub)
        let expected_remaining = snap.max_cost_cents.saturating_sub(snap.window_cost_cents);
        prop_assert_eq!(snap.remaining_cents, expected_remaining);
    }

    /// Total lifetime cost monotonically increases
    #[test]
    fn cost_budget_total_monotonic(n in 1usize..50) {
        let mut cb = CostBudget::new(CostBudgetConfig::default());
        let mut prev = 0;
        for i in 0..n {
            cb.record(&ConnectorActionKind::Ticket, i as u64 * 100);
            let total = cb.total_cost_cents();
            prop_assert!(total >= prev, "total cost decreased: {} -> {}", prev, total);
            prev = total;
        }
    }
}

// =============================================================================
// Governor Evaluation Priority Properties
// =============================================================================

proptest! {
    /// When queue is in reject zone, verdict is Reject with Backpressure reason
    #[test]
    fn governor_rejects_on_queue_backpressure(
        connector in arb_connector_id(),
        kind in arb_action_kind(),
    ) {
        let config = ConnectorGovernorConfig {
            queue_backpressure: QueueBackpressureConfig {
                max_queue_depth: 10,
                throttle_threshold: 0.7,
                reject_threshold: 0.9,
            },
            ..ConnectorGovernorConfig::default()
        };
        let mut gov = ConnectorGovernor::new(config);
        // Fill queue past reject threshold
        for _ in 0..10 {
            gov.record_enqueue();
        }
        let action = make_action(&connector, kind, 1000);
        let d = gov.evaluate(&action, 1000);
        prop_assert_eq!(d.verdict, GovernorVerdict::Reject);
        prop_assert_eq!(d.reason, GovernorReason::Backpressure);
    }

    /// When adaptive backoff is active, verdict is Throttle with AdaptiveBackoff reason
    #[test]
    fn governor_throttles_on_backoff(
        connector in arb_connector_id(),
    ) {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        // Trigger adaptive backoff
        gov.record_outcome(&connector, false, 1000);
        let action = make_action(&connector, ConnectorActionKind::Notify, 1000);
        let d = gov.evaluate(&action, 1000);
        prop_assert_eq!(d.verdict, GovernorVerdict::Throttle);
        prop_assert_eq!(d.reason, GovernorReason::AdaptiveBackoff);
    }

    /// Fresh governor with default config allows first action
    #[test]
    fn governor_allows_first_action(
        connector in arb_connector_id(),
        kind in arb_action_kind(),
    ) {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        let action = make_action(&connector, kind, 1000);
        let d = gov.evaluate(&action, 1000);
        prop_assert_eq!(d.verdict, GovernorVerdict::Allow);
        prop_assert_eq!(d.reason, GovernorReason::Clear);
    }

    /// Evaluation creates connector state lazily
    #[test]
    fn governor_creates_connector_on_evaluate(
        connector in arb_connector_id(),
    ) {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        prop_assert!(gov.get_connector(&connector).is_none());
        let action = make_action(&connector, ConnectorActionKind::Notify, 1000);
        gov.evaluate(&action, 1000);
        prop_assert!(gov.get_connector(&connector).is_some());
    }

    /// connector_ids returns all evaluated connectors
    #[test]
    fn governor_tracks_all_connectors(
        connectors in prop::collection::hash_set(arb_connector_id(), 1..5),
    ) {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        for c in &connectors {
            let action = make_action(c, ConnectorActionKind::Notify, 1000);
            gov.evaluate(&action, 1000);
        }
        let ids: std::collections::HashSet<&str> = gov.connector_ids().into_iter().collect();
        for c in &connectors {
            prop_assert!(ids.contains(c.as_str()),
                "missing connector: {}", c);
        }
    }
}

// =============================================================================
// Serde Roundtrip Extended Properties
// =============================================================================

proptest! {
    /// GovernorDecision serde roundtrip preserves all fields
    #[test]
    fn decision_serde_full_roundtrip(
        connector in arb_connector_id(),
        verdict in arb_verdict(),
        reason in arb_reason(),
        delay_ms in 0u64..100_000,
        now_ms in 0u64..1_000_000,
    ) {
        let d = GovernorDecision {
            verdict: verdict.clone(),
            reason: reason.clone(),
            delay_ms,
            connector_id: connector.clone(),
            action_kind: "notify".to_string(),
            decided_at_ms: now_ms,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: GovernorDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d.verdict, back.verdict);
        prop_assert_eq!(d.reason, back.reason);
        prop_assert_eq!(d.delay_ms, back.delay_ms);
        prop_assert_eq!(d.connector_id, back.connector_id);
        prop_assert_eq!(d.action_kind, back.action_kind);
        prop_assert_eq!(d.decided_at_ms, back.decided_at_ms);
    }

    /// QueueBackpressureSnapshot serde roundtrip
    #[test]
    fn queue_snapshot_serde_roundtrip(
        current in 0usize..10_000,
        max in 1usize..10_000,
        peak in 0usize..10_000,
        enqueued in 0u64..100_000,
        rejected in 0u64..100_000,
    ) {
        let snap = QueueBackpressureSnapshot {
            current_depth: current,
            max_depth: max,
            peak_depth: peak.max(current),
            depth_fraction: if max == 0 { 1.0 } else { current as f64 / max as f64 },
            total_enqueued: enqueued,
            total_rejected: rejected,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: QueueBackpressureSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.current_depth, back.current_depth);
        prop_assert_eq!(snap.max_depth, back.max_depth);
        prop_assert_eq!(snap.peak_depth, back.peak_depth);
        prop_assert_eq!(snap.total_enqueued, back.total_enqueued);
        prop_assert_eq!(snap.total_rejected, back.total_rejected);
        prop_assert!((snap.depth_fraction - back.depth_fraction).abs() < 1e-10);
    }

    /// CostBudgetSnapshot serde roundtrip
    #[test]
    fn cost_snapshot_serde_roundtrip(
        window_cost in 0u64..100_000,
        max_cost in 1u64..100_000,
        total in 0u64..1_000_000,
    ) {
        let remaining = max_cost.saturating_sub(window_cost);
        let snap = CostBudgetSnapshot {
            window_cost_cents: window_cost,
            max_cost_cents: max_cost,
            remaining_cents: remaining,
            usage_fraction: window_cost as f64 / max_cost as f64,
            total_lifetime_cents: total,
            window_ms: 3_600_000,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: CostBudgetSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.window_cost_cents, back.window_cost_cents);
        prop_assert_eq!(snap.max_cost_cents, back.max_cost_cents);
        prop_assert_eq!(snap.remaining_cents, back.remaining_cents);
        prop_assert_eq!(snap.total_lifetime_cents, back.total_lifetime_cents);
        prop_assert_eq!(snap.window_ms, back.window_ms);
        prop_assert!((snap.usage_fraction - back.usage_fraction).abs() < 1e-10);
    }
}
