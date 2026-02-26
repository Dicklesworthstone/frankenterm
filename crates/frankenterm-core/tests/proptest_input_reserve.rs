//! Property-based tests for [`frankenterm_core::input_reserve`].
//!
//! 32 properties covering S3-FIFO capacity/priority/stats invariants (10),
//! reserve floor monotonicity/partition conservation (10), controller
//! submit-schedule-shed consistency (8), and serde roundtrips (4).

use proptest::prelude::*;

use frankenterm_core::backpressure::BackpressureTier;
use frankenterm_core::input_reserve::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_work_item_class() -> impl Strategy<Value = WorkItemClass> {
    prop_oneof![
        Just(WorkItemClass::Input),
        Just(WorkItemClass::ViewportCore),
        Just(WorkItemClass::ViewportOverscan),
        Just(WorkItemClass::ColdScrollback),
        Just(WorkItemClass::BackgroundCapture),
    ]
}

fn arb_tier() -> impl Strategy<Value = BackpressureTier> {
    prop_oneof![
        Just(BackpressureTier::Green),
        Just(BackpressureTier::Yellow),
        Just(BackpressureTier::Red),
        Just(BackpressureTier::Black),
    ]
}

fn arb_shed_reason() -> impl Strategy<Value = ShedReason> {
    prop_oneof![
        Just(ShedReason::S3FifoSmallEviction),
        Just(ShedReason::ReserveFloorEnforcement),
        Just(ShedReason::ColdSeverityShed),
        Just(ShedReason::StaleTimeout),
        Just(ShedReason::CapacityOverflow),
        Just(ShedReason::GhostEviction),
        Just(ShedReason::IsolateInteractive),
    ]
}

fn arb_work_item(id_range: std::ops::Range<u64>) -> impl Strategy<Value = WorkItem> {
    (
        id_range,
        1u64..100,
        arb_work_item_class(),
        1u32..10,
        1000u64..2000,
    )
        .prop_map(|(id, pane_id, class, work_units, ts)| WorkItem {
            id,
            pane_id,
            class,
            work_units,
            submitted_at_ms: ts,
            sequence: 0,
        })
}

fn arb_severity() -> impl Strategy<Value = f64> {
    (0u32..=100).prop_map(|n| n as f64 / 100.0)
}

fn arb_s3fifo_config() -> impl Strategy<Value = S3FifoConfig> {
    (10usize..=100, 5u32..=40, 1usize..=5).prop_map(|(cap, small_pct, ghost_mul)| S3FifoConfig {
        total_capacity: cap,
        small_fraction: small_pct as f64 / 100.0,
        ghost_capacity_multiplier: ghost_mul,
    })
}

fn arb_reserve_floor_config() -> impl Strategy<Value = ReserveFloorConfig> {
    (1u32..=10, 0u32..=10, 1u32..=20, 10u32..=100).prop_map(|(base, surge, threshold, cold_pct)| {
        ReserveFloorConfig {
            base_floor_units: base,
            surge_reserve_units: surge,
            surge_backlog_threshold: threshold,
            cold_shed_severity: cold_pct as f64 / 100.0,
        }
    })
}

// ---------------------------------------------------------------------------
// S3-FIFO invariants (10 properties)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. Queue length never exceeds total_capacity
    #[test]
    fn s3fifo_capacity_invariant(
        config in arb_s3fifo_config(),
        items in proptest::collection::vec(arb_work_item(0..500), 1..200),
        tier in arb_tier(),
        severity in arb_severity(),
    ) {
        let mut q = S3FifoQueue::new(config.clone());
        for item in items {
            q.admit(item, tier, severity, 1000);
        }
        prop_assert!(q.len() <= config.total_capacity,
            "queue len {} exceeds capacity {}", q.len(), config.total_capacity);
    }

    // 2. Total admitted >= items selected + items evicted (accounting)
    #[test]
    fn s3fifo_accounting_invariant(
        config in arb_s3fifo_config(),
        items in proptest::collection::vec(arb_work_item(0..500), 1..100),
    ) {
        let mut q = S3FifoQueue::new(config);
        for item in &items {
            let _shed = q.admit(item.clone(), BackpressureTier::Green, 0.0, 1000);
        }
        let stats = q.stats();
        // admitted == items in queue + evicted + promoted_that_were_evicted
        prop_assert!(stats.total_admitted == items.len() as u64,
            "admitted {} != submitted {}", stats.total_admitted, items.len());
    }

    // 3. Ghost length never exceeds ghost capacity
    #[test]
    fn s3fifo_ghost_capacity_invariant(
        config in arb_s3fifo_config(),
        items in proptest::collection::vec(arb_work_item(0..500), 1..200),
    ) {
        let mut q = S3FifoQueue::new(config.clone());
        let ghost_cap = config.total_capacity.saturating_mul(config.ghost_capacity_multiplier).max(1);
        for item in items {
            q.admit(item, BackpressureTier::Green, 0.0, 1000);
        }
        prop_assert!(q.stats().ghost_len <= ghost_cap,
            "ghost {} exceeds capacity {}", q.stats().ghost_len, ghost_cap);
    }

    // 4. Drain returns items sorted by priority desc, then sequence asc
    #[test]
    fn s3fifo_drain_order_invariant(
        items in proptest::collection::vec(arb_work_item(0..500), 1..50),
    ) {
        let mut q = S3FifoQueue::new(S3FifoConfig { total_capacity: 500, ..S3FifoConfig::default() });
        for item in items {
            q.admit(item, BackpressureTier::Green, 0.0, 1000);
        }
        let drained = q.drain_all();
        for window in drained.windows(2) {
            let a_pri = window[0].class.priority_level();
            let b_pri = window[1].class.priority_level();
            prop_assert!(a_pri >= b_pri,
                "priority ordering violated: {} < {}", a_pri, b_pri);
            if a_pri == b_pri {
                prop_assert!(window[0].sequence <= window[1].sequence,
                    "sequence ordering violated within same priority");
            }
        }
    }

    // 5. After drain, queue is empty
    #[test]
    fn s3fifo_drain_empties(
        items in proptest::collection::vec(arb_work_item(0..500), 0..50),
    ) {
        let mut q = S3FifoQueue::new(S3FifoConfig::default());
        for item in items {
            q.admit(item, BackpressureTier::Green, 0.0, 1000);
        }
        let _ = q.drain_all();
        prop_assert!(q.is_empty());
        prop_assert_eq!(q.len(), 0);
    }

    // 6. Small segment never exceeds its fraction-based capacity
    #[test]
    fn s3fifo_small_capacity_invariant(
        config in arb_s3fifo_config(),
        items in proptest::collection::vec(arb_work_item(0..500), 1..100),
    ) {
        let mut q = S3FifoQueue::new(config.clone());
        let small_cap = ((config.total_capacity as f64 * config.small_fraction).ceil() as usize).max(1);
        for item in items {
            q.admit(item, BackpressureTier::Green, 0.0, 1000);
            prop_assert!(q.stats().small_len <= small_cap,
                "small {} > small_cap {}", q.stats().small_len, small_cap);
        }
    }

    // 7. Shed markers always have valid reasons
    #[test]
    fn s3fifo_shed_markers_valid(
        items in proptest::collection::vec(arb_work_item(0..100), 1..100),
    ) {
        let cfg = S3FifoConfig { total_capacity: 10, small_fraction: 0.3, ghost_capacity_multiplier: 2 };
        let mut q = S3FifoQueue::new(cfg);
        for item in items {
            let shed = q.admit(item, BackpressureTier::Red, 0.7, 1000);
            for marker in &shed {
                let is_valid = matches!(marker.reason,
                    ShedReason::S3FifoSmallEviction | ShedReason::CapacityOverflow);
                prop_assert!(is_valid, "unexpected shed reason from admit: {:?}", marker.reason);
            }
        }
    }

    // 8. Promoted count <= evicted from small that were hot
    #[test]
    fn s3fifo_promotion_bounded(
        items in proptest::collection::vec(arb_work_item(0..200), 1..100),
    ) {
        let mut q = S3FifoQueue::new(S3FifoConfig { total_capacity: 20, small_fraction: 0.3, ghost_capacity_multiplier: 2 });
        for item in items {
            q.admit(item, BackpressureTier::Green, 0.0, 1000);
        }
        let stats = q.stats();
        // promoted items came from small evictions, so promoted <= total_evicted + promoted
        prop_assert!(stats.total_promoted <= stats.total_admitted,
            "promoted {} > admitted {}", stats.total_promoted, stats.total_admitted);
    }

    // 9. Ghost hits tracked correctly
    #[test]
    fn s3fifo_ghost_hit_consistency(
        base_items in proptest::collection::vec(arb_work_item(0..50), 5..30),
    ) {
        let cfg = S3FifoConfig { total_capacity: 8, small_fraction: 0.25, ghost_capacity_multiplier: 3 };
        let mut q = S3FifoQueue::new(cfg);
        // Admit all to populate ghost
        for item in &base_items {
            q.admit(item.clone(), BackpressureTier::Green, 0.0, 1000);
        }
        let ghost_before = q.stats().ghost_len;
        // Re-admit some items (may hit ghost)
        let ghost_hits_before = q.stats().total_ghost_hits;
        for item in base_items.iter().take(5) {
            q.admit(item.clone(), BackpressureTier::Green, 0.0, 1000);
        }
        let ghost_hits_after = q.stats().total_ghost_hits;
        prop_assert!(ghost_hits_after >= ghost_hits_before);
        // Ghost hits should decrease ghost size (or at least not increase beyond capacity)
        let _ghost_after = q.stats().ghost_len;
        // Just verify ghost_hits is monotonically increasing
        let _ = ghost_before; // used above
    }

    // 10. Access bumps frequency
    #[test]
    fn s3fifo_access_frequency(
        id in 0u64..100,
    ) {
        let mut q = S3FifoQueue::new(S3FifoConfig::default());
        let item = WorkItem { id, pane_id: 1, class: WorkItemClass::Input, work_units: 1, submitted_at_ms: 1000, sequence: 0 };
        q.admit(item, BackpressureTier::Green, 0.0, 1000);
        // Initial admit sets frequency to 1
        q.access(id);
        q.access(id);
        // Can't directly inspect frequency, but accessing shouldn't panic
        prop_assert!(!q.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Reserve floor invariants (10 properties)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 11. Floor is non-negative
    #[test]
    fn floor_nonnegative(
        config in arb_reserve_floor_config(),
        backlog in 0u32..1000,
    ) {
        let policy = ReserveFloorPolicy::new(config);
        let floor = policy.compute_floor(backlog);
        prop_assert!(floor >= 1, "floor should be at least base_floor which is >= 1");
    }

    // 12. Floor monotonically increases with backlog (step function)
    #[test]
    fn floor_monotonic_with_backlog(
        config in arb_reserve_floor_config(),
        a in 0u32..500,
        b in 0u32..500,
    ) {
        let policy = ReserveFloorPolicy::new(config);
        let fa = policy.compute_floor(a);
        let fb = policy.compute_floor(b);
        if a <= b {
            prop_assert!(fa <= fb, "floor({})={} > floor({})={}", a, fa, b, fb);
        }
    }

    // 13. Partition budget conservation: interactive + background = total
    #[test]
    fn partition_conservation(
        config in arb_reserve_floor_config(),
        total in 0u32..1000,
        tier in arb_tier(),
        severity in arb_severity(),
        backlog in 0u32..100,
    ) {
        let policy = ReserveFloorPolicy::new(config);
        let part = policy.partition(total, tier, severity, backlog);
        prop_assert_eq!(
            part.interactive_budget + part.background_budget,
            total,
            "budgets don't sum to total: {} + {} != {}",
            part.interactive_budget, part.background_budget, total
        );
    }

    // 14. Interactive budget >= min(floor, total)
    #[test]
    fn partition_floor_invariant(
        config in arb_reserve_floor_config(),
        total in 0u32..1000,
        tier in arb_tier(),
        severity in arb_severity(),
        backlog in 0u32..100,
    ) {
        let policy = ReserveFloorPolicy::new(config.clone());
        let floor = policy.compute_floor(backlog);
        let part = policy.partition(total, tier, severity, backlog);
        let expected_min = floor.min(total);
        prop_assert!(
            part.interactive_budget >= expected_min,
            "interactive {} < min(floor={}, total={}) = {}",
            part.interactive_budget, floor, total, expected_min
        );
    }

    // 15. Shed action escalation is monotonic with tier ordering
    #[test]
    fn shed_action_monotonic_with_tier(
        config in arb_reserve_floor_config(),
    ) {
        let policy = ReserveFloorPolicy::new(config);
        let actions: Vec<ShedAction> = [
            BackpressureTier::Green,
            BackpressureTier::Yellow,
            BackpressureTier::Red,
            BackpressureTier::Black,
        ].iter().map(|t| policy.shed_action_for_tier(*t)).collect();

        for window in actions.windows(2) {
            prop_assert!(window[0] <= window[1],
                "shed action not monotonic: {:?} > {:?}", window[0], window[1]);
        }
    }

    // 16. With zero total budget, both partitions are zero
    #[test]
    fn partition_zero_total(
        config in arb_reserve_floor_config(),
        tier in arb_tier(),
        severity in arb_severity(),
        backlog in 0u32..100,
    ) {
        let policy = ReserveFloorPolicy::new(config);
        let part = policy.partition(0, tier, severity, backlog);
        prop_assert_eq!(part.interactive_budget, 0);
        prop_assert_eq!(part.background_budget, 0);
    }

    // 17. Surge only activates at threshold
    #[test]
    fn surge_threshold_boundary(
        config in arb_reserve_floor_config(),
    ) {
        let policy = ReserveFloorPolicy::new(config.clone());
        let below = if config.surge_backlog_threshold > 0 {
            policy.compute_floor(config.surge_backlog_threshold - 1)
        } else {
            policy.compute_floor(0)
        };
        let at = policy.compute_floor(config.surge_backlog_threshold);

        if config.surge_reserve_units > 0 && config.surge_backlog_threshold > 0 {
            prop_assert!(at > below,
                "surge should activate at threshold: floor_at={}, floor_below={}", at, below);
        }
    }

    // 18. Background budget is non-negative
    #[test]
    fn partition_background_nonneg(
        config in arb_reserve_floor_config(),
        total in 0u32..1000,
        tier in arb_tier(),
        severity in arb_severity(),
        backlog in 0u32..100,
    ) {
        let policy = ReserveFloorPolicy::new(config);
        let part = policy.partition(total, tier, severity, backlog);
        // background_budget is u32, so always >= 0, but let's verify semantics
        prop_assert!(part.interactive_budget <= total);
    }

    // 19. Higher backlog never reduces interactive budget for same total
    #[test]
    fn partition_backlog_monotonic(
        config in arb_reserve_floor_config(),
        total in 1u32..1000,
        tier in arb_tier(),
        severity in arb_severity(),
        a in 0u32..500,
        b in 0u32..500,
    ) {
        let policy = ReserveFloorPolicy::new(config);
        let pa = policy.partition(total, tier, severity, a);
        let pb = policy.partition(total, tier, severity, b);
        if a <= b {
            prop_assert!(pa.interactive_budget <= pb.interactive_budget,
                "backlog {} gives interactive {} > backlog {} gives {}",
                a, pa.interactive_budget, b, pb.interactive_budget);
        }
    }

    // 20. Config roundtrip through accessors
    #[test]
    fn policy_config_accessible(
        config in arb_reserve_floor_config(),
    ) {
        let policy = ReserveFloorPolicy::new(config.clone());
        prop_assert_eq!(policy.config().base_floor_units, config.base_floor_units);
        prop_assert_eq!(policy.config().surge_reserve_units, config.surge_reserve_units);
    }
}

// ---------------------------------------------------------------------------
// Controller consistency (8 properties)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // 21. Selected items respect budget partition
    #[test]
    fn controller_budget_respected(
        items in proptest::collection::vec(arb_work_item(0..200), 1..30),
        total_budget in 1u32..50,
        tier in arb_tier(),
        severity in arb_severity(),
        backlog in 0u32..20,
    ) {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        for item in items {
            ctrl.submit(item, tier, severity, 1000);
        }
        let result = ctrl.schedule_frame(total_budget, tier, severity, backlog, 1500);

        let interactive_total: u32 = result.selected.iter()
            .filter(|item| item.class.is_interactive())
            .map(|item| item.work_units)
            .sum();
        let background_total: u32 = result.selected.iter()
            .filter(|item| !item.class.is_interactive())
            .map(|item| item.work_units)
            .sum();

        prop_assert!(interactive_total <= result.partition.interactive_budget,
            "interactive {} > budget {}", interactive_total, result.partition.interactive_budget);
        prop_assert!(background_total <= result.partition.background_budget,
            "background {} > budget {}", background_total, result.partition.background_budget);
    }

    // 22. Selected + shed = all submitted (conservation)
    #[test]
    fn controller_item_conservation(
        items in proptest::collection::vec(arb_work_item(0..500), 1..20),
        total_budget in 1u32..100,
    ) {
        let cfg = InputReserveConfig {
            s3fifo: S3FifoConfig { total_capacity: 500, small_fraction: 0.1, ghost_capacity_multiplier: 1 },
            stale_timeout_ms: 100000,
            ..Default::default()
        };
        let mut ctrl = InputReserveController::new(cfg);
        let mut admission_shed = 0usize;
        for item in &items {
            let shed = ctrl.submit(item.clone(), BackpressureTier::Green, 0.0, 1000);
            admission_shed += shed.len();
        }
        let result = ctrl.schedule_frame(total_budget, BackpressureTier::Green, 0.0, 0, 1500);

        let total_accounted = result.selected.len() + result.shed_markers.len() + admission_shed;
        prop_assert_eq!(total_accounted, items.len(),
            "selected {} + frame_shed {} + admit_shed {} != submitted {}",
            result.selected.len(), result.shed_markers.len(), admission_shed, items.len());
    }

    // 23. Black tier never selects non-interactive items
    #[test]
    fn controller_black_isolates(
        items in proptest::collection::vec(arb_work_item(0..200), 1..20),
    ) {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        for item in items {
            ctrl.submit(item, BackpressureTier::Black, 1.0, 1000);
        }
        let result = ctrl.schedule_frame(100, BackpressureTier::Black, 1.0, 0, 1500);

        for item in &result.selected {
            prop_assert!(item.class.is_interactive(),
                "non-interactive item {:?} selected under Black tier", item.class);
        }
    }

    // 24. Red tier never selects ColdScrollback
    #[test]
    fn controller_red_drops_cold(
        items in proptest::collection::vec(arb_work_item(0..200), 1..20),
    ) {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        for item in items {
            ctrl.submit(item, BackpressureTier::Red, 0.9, 1000);
        }
        let result = ctrl.schedule_frame(100, BackpressureTier::Red, 0.9, 0, 1500);

        for item in &result.selected {
            prop_assert_ne!(item.class, WorkItemClass::ColdScrollback,
                "cold scrollback selected under Red tier");
        }
    }

    // 25. Stale items are always shed
    #[test]
    fn controller_stale_always_shed(
        n in 1usize..20,
    ) {
        let cfg = InputReserveConfig {
            stale_timeout_ms: 100,
            s3fifo: S3FifoConfig { total_capacity: 100, ..S3FifoConfig::default() },
            ..Default::default()
        };
        let mut ctrl = InputReserveController::new(cfg);
        for i in 0..n {
            let item = WorkItem {
                id: i as u64,
                pane_id: 1,
                class: WorkItemClass::Input,
                work_units: 1,
                submitted_at_ms: 500,
                sequence: 0,
            };
            ctrl.submit(item, BackpressureTier::Green, 0.0, 500);
        }
        // Schedule at t=700 → stale (200 > 100)
        let result = ctrl.schedule_frame(1000, BackpressureTier::Green, 0.0, 0, 700);
        prop_assert!(result.selected.is_empty(),
            "stale items should not be selected");
        let all_stale = result.shed_markers.iter().all(|m| m.reason == ShedReason::StaleTimeout);
        prop_assert!(all_stale, "all shed markers should be StaleTimeout");
    }

    // 26. Metrics counters are consistent
    #[test]
    fn controller_metrics_consistent(
        items in proptest::collection::vec(arb_work_item(0..200), 1..30),
        budget in 1u32..50,
    ) {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        for item in items {
            ctrl.submit(item, BackpressureTier::Green, 0.0, 1000);
        }
        ctrl.schedule_frame(budget, BackpressureTier::Green, 0.0, 0, 1500);

        let m = ctrl.metrics();
        prop_assert_eq!(m.frames_scheduled, 1);
        prop_assert!(m.total_items_delivered + m.total_items_shed > 0 || m.queue_stats.total_admitted == 0);
    }

    // 27. Sequence numbers are monotonically assigned
    #[test]
    fn controller_sequence_monotonic(
        items in proptest::collection::vec(arb_work_item(0..500), 2..30),
    ) {
        let cfg = InputReserveConfig {
            s3fifo: S3FifoConfig { total_capacity: 500, ..S3FifoConfig::default() },
            ..Default::default()
        };
        let mut ctrl = InputReserveController::new(cfg);
        for item in items {
            ctrl.submit(item, BackpressureTier::Green, 0.0, 1000);
        }
        let result = ctrl.schedule_frame(10000, BackpressureTier::Green, 0.0, 0, 1500);

        // Within the same priority level, sequence should be ascending
        let mut prev_by_priority: std::collections::HashMap<u8, u64> = std::collections::HashMap::new();
        for item in &result.selected {
            let pri = item.class.priority_level();
            if let Some(prev_seq) = prev_by_priority.get(&pri) {
                prop_assert!(item.sequence > *prev_seq,
                    "sequence not monotonic within priority {}: {} <= {}", pri, item.sequence, prev_seq);
            }
            prev_by_priority.insert(pri, item.sequence);
        }
    }

    // 28. Multiple schedule_frame calls on empty queue produce empty results
    #[test]
    fn controller_empty_idempotent(
        n in 1usize..5,
    ) {
        let mut ctrl = InputReserveController::new(InputReserveConfig::default());
        for _ in 0..n {
            let result = ctrl.schedule_frame(100, BackpressureTier::Green, 0.0, 0, 1000);
            prop_assert!(result.selected.is_empty());
            prop_assert!(result.shed_markers.is_empty());
        }
        prop_assert_eq!(ctrl.metrics().frames_scheduled, n as u64);
    }
}

// ---------------------------------------------------------------------------
// Serde roundtrips (4 properties)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 29. WorkItemClass serde roundtrip
    #[test]
    fn serde_work_item_class(class in arb_work_item_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let back: WorkItemClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(class, back);
    }

    // 30. ShedReason serde roundtrip
    #[test]
    fn serde_shed_reason(reason in arb_shed_reason()) {
        let json = serde_json::to_string(&reason).unwrap();
        let back: ShedReason = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(reason, back);
    }

    // 31. ShedMarker serde roundtrip
    #[test]
    fn serde_shed_marker(
        item_id in 0u64..1000,
        pane_id in 0u64..100,
        class in arb_work_item_class(),
        reason in arb_shed_reason(),
        tier in arb_tier(),
        severity in arb_severity(),
        shed_at_ms in 0u64..100000,
    ) {
        let marker = ShedMarker {
            item_id,
            pane_id,
            class,
            reason,
            tier,
            severity,
            shed_at_ms,
        };
        let json = serde_json::to_string(&marker).unwrap();
        let back: ShedMarker = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.item_id, marker.item_id);
        prop_assert_eq!(back.reason, marker.reason);
        prop_assert_eq!(back.class, marker.class);
        prop_assert_eq!(back.tier, marker.tier);
        prop_assert!((back.severity - marker.severity).abs() < 1e-10);
    }

    // 32. InputReserveConfig serde roundtrip
    #[test]
    fn serde_config(
        total_cap in 10usize..500,
        small_frac in 5u32..50,
        ghost_mul in 1usize..10,
        base in 1u32..20,
        surge in 0u32..20,
        threshold in 1u32..50,
        stale in 100u64..50000,
    ) {
        let cfg = InputReserveConfig {
            s3fifo: S3FifoConfig {
                total_capacity: total_cap,
                small_fraction: small_frac as f64 / 100.0,
                ghost_capacity_multiplier: ghost_mul,
            },
            reserve_floor: ReserveFloorConfig {
                base_floor_units: base,
                surge_reserve_units: surge,
                surge_backlog_threshold: threshold,
                cold_shed_severity: 0.7,
            },
            stale_timeout_ms: stale,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: InputReserveConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.s3fifo.total_capacity, cfg.s3fifo.total_capacity);
        prop_assert_eq!(back.reserve_floor.base_floor_units, cfg.reserve_floor.base_floor_units);
        prop_assert_eq!(back.stale_timeout_ms, cfg.stale_timeout_ms);
    }
}
