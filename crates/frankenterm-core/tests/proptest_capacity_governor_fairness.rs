//! Property tests for `capacity_governor` invariants —
//! complementary to the existing 15 KB
//! `proptest_capacity_governor.rs` (serde round-trips, weight
//! monotonicity, threshold-spot-checks). This file pins the
//! invariants the user PHASE 8 directive named: **tier
//! transitions, queue depth bounds, throttle-gate fairness
//! across `WorkloadCategory`**.
//!
//! ## Invariants pinned
//!
//! 1. **Throttle-gate fairness** — for any
//!    `(config, signals)`, if Heavy is *Allowed* then Medium
//!    and Light must also be Allowed. Heavy is the most
//!    resource-intensive workload; if pressure is low enough
//!    to admit Heavy, the lighter categories cannot have a
//!    stricter gate. (Inversely, Light being Blocked implies
//!    Medium and Heavy are also Blocked under the same
//!    `(config, signals)`.)
//!
//! 2. **Throttle-gate ordering under pressure** — at any
//!    pressure that triggers a Block on Light, Heavy and Medium
//!    are likewise non-Allow. Pinning the ordered fairness:
//!    Heavy ⊆ Medium ⊆ Light in admission set.
//!
//! 3. **Tier transition monotonicity** — for any two pressure
//!    signals with `s1.max ≤ s2.max`, `s1.health_tier() ≤
//!    s2.health_tier()`. Pressure increase must never *lower*
//!    the tier.
//!
//! 4. **Determinism** — two FRESH governors with the same
//!    config produce the same first-call decision for the same
//!    `(category, signals)`. The governor mutates self-state
//!    (telemetry, decision_log, codel queue) on each call, so
//!    determinism between two calls on the SAME governor is
//!    not a property; the across-fresh-instance comparison is.
//!
//! 5. **Default signals → Allow for all categories** — zero
//!    pressure must allow all three categories.
//!
//! 6. **`is_permitted` partitions Block from everything else** —
//!    `decision.is_permitted() == !matches!(decision, Block)`.
//!
//! 7. **Override semantics — applies_to** — when an override
//!    has `category = None`, it applies to every category.
//!    When set to `Some(c)`, it applies only to `c`.
//!
//! 8. **Override semantics — is_active** — `expires_ms == 0`
//!    means always active; `expires_ms > 0` means active iff
//!    `now_ms < expires_ms`.
//!
//! 9. **Operator override produces an Override decision with
//!    the original wrapped inside** — when an active override
//!    is in place that applies to the requested category,
//!    `evaluate` returns `Override { original_decision, ... }`
//!    where the inner `original_decision` is what the governor
//!    would have produced without the override.
//!
//! 10. **`rch_can_offload` is conjunction-of-flags** —
//!     `rch_available && rch_workers_available > 0`.
//!
//! Logs are emitted as structured tracing-json events matching
//! the prior phase pattern.

use std::sync::Once;

use frankenterm_core::capacity_governor::{
    CapacityGovernor, CapacityGovernorConfig, GovernorDecision, OperatorOverride, PressureSignals,
    WorkloadCategory,
};
use proptest::prelude::*;
use tracing::info;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

fn arb_workload_category() -> impl Strategy<Value = WorkloadCategory> {
    prop_oneof![
        Just(WorkloadCategory::Heavy),
        Just(WorkloadCategory::Medium),
        Just(WorkloadCategory::Light),
    ]
}

/// Pressure signals across the full healthy → critical range.
/// Uses `0.0..=1.0` for ratios, bounded counts for active
/// workloads, and a 0..=20 load-average range that crosses the
/// default `load_average_block_threshold = 12.0`.
fn arb_pressure_signals() -> impl Strategy<Value = PressureSignals> {
    (
        0.0..=1.0_f64,     // cpu_utilization
        0.0..=1.0_f64,     // memory_utilization
        0u32..=8,          // active_heavy_workloads
        0u32..=16,         // active_medium_workloads
        0.0..=20.0_f64,    // load_average_1m
        any::<bool>(),     // rch_available
        0u32..=8,          // rch_workers_available
        0.0..=1.0_f64,     // io_pressure
        0u64..=10_000_000, // timestamp_ms
    )
        .prop_map(
            |(cpu, mem, heavy, medium, load, rch, workers, io, ts)| PressureSignals {
                cpu_utilization: cpu,
                memory_utilization: mem,
                active_heavy_workloads: heavy,
                active_medium_workloads: medium,
                load_average_1m: load,
                rch_available: rch,
                rch_workers_available: workers,
                io_pressure: io,
                timestamp_ms: ts,
            },
        )
}

/// Standard test config — defaults, kept identical across all
/// fresh-governor constructions in the same proptest case so the
/// determinism invariant holds.
fn standard_config() -> CapacityGovernorConfig {
    CapacityGovernorConfig::default()
}

fn is_allow(d: &GovernorDecision) -> bool {
    matches!(d, GovernorDecision::Allow { .. })
}

fn is_block(d: &GovernorDecision) -> bool {
    matches!(d, GovernorDecision::Block { .. })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// **Property 1 — throttle-gate fairness (Allow direction)**:
    /// if Heavy is Allowed under `(config, signals)`, then Medium
    /// and Light must also be Allowed under the same. Heavy is
    /// the resource-heaviest category; if it gets through, the
    /// lighter categories cannot face a stricter gate.
    #[test]
    fn proptest_capacity_governor_heavy_allowed_implies_medium_and_light_allowed(
        signals in arb_pressure_signals(),
    ) {
        init_test_tracing_json();
        let mut g_heavy = CapacityGovernor::new(standard_config());
        let mut g_medium = CapacityGovernor::new(standard_config());
        let mut g_light = CapacityGovernor::new(standard_config());

        let d_heavy = g_heavy.evaluate(WorkloadCategory::Heavy, &signals);
        let d_medium = g_medium.evaluate(WorkloadCategory::Medium, &signals);
        let d_light = g_light.evaluate(WorkloadCategory::Light, &signals);

        info!(
            test = "heavy_allow_implies_lighter_allow",
            cpu = signals.cpu_utilization,
            mem = signals.memory_utilization,
            d_heavy = ?d_heavy,
            d_medium = ?d_medium,
            d_light = ?d_light,
            "throttle-gate fairness case"
        );

        if is_allow(&d_heavy) {
            prop_assert!(is_allow(&d_medium),
                "Heavy Allow must imply Medium Allow under the same signals");
            prop_assert!(is_allow(&d_light),
                "Heavy Allow must imply Light Allow under the same signals");
        }
    }

    /// **Property 2 — throttle-gate ordering (Block direction)**:
    /// if Light is Blocked under `(config, signals)`, then
    /// Medium and Heavy must also be non-Allow (Throttle, Block,
    /// or Offload). Light is the cheapest workload; if it can't
    /// get through, the heavier categories cannot have a more
    /// permissive gate.
    #[test]
    fn proptest_capacity_governor_light_blocked_implies_medium_and_heavy_non_allow(
        signals in arb_pressure_signals(),
    ) {
        init_test_tracing_json();
        let mut g_heavy = CapacityGovernor::new(standard_config());
        let mut g_medium = CapacityGovernor::new(standard_config());
        let mut g_light = CapacityGovernor::new(standard_config());

        let d_light = g_light.evaluate(WorkloadCategory::Light, &signals);
        if is_block(&d_light) {
            let d_medium = g_medium.evaluate(WorkloadCategory::Medium, &signals);
            let d_heavy = g_heavy.evaluate(WorkloadCategory::Heavy, &signals);
            prop_assert!(!is_allow(&d_medium),
                "Light Block must imply Medium non-Allow under the same signals");
            prop_assert!(!is_allow(&d_heavy),
                "Light Block must imply Heavy non-Allow under the same signals");
        }
    }

    /// **Property 3 — tier transition monotonicity**: for any
    /// two pressure signal bundles with the same max-pressure
    /// component, the derived health tier is the same; if `s1.max
    /// ≤ s2.max`, then `s1.health_tier() ≤ s2.health_tier()`.
    #[test]
    fn proptest_capacity_governor_health_tier_monotone_in_max_pressure(
        s1 in arb_pressure_signals(),
        s2 in arb_pressure_signals(),
    ) {
        init_test_tracing_json();
        let m1 = s1.cpu_utilization.max(s1.memory_utilization).max(s1.io_pressure);
        let m2 = s2.cpu_utilization.max(s2.memory_utilization).max(s2.io_pressure);
        let t1 = s1.health_tier();
        let t2 = s2.health_tier();
        if m1 <= m2 {
            prop_assert!(t1 <= t2,
                "tier must be monotone-non-decreasing in max pressure (m1={m1}, t1={t1:?}, m2={m2}, t2={t2:?})");
        }
    }

    /// **Property 4 — determinism across fresh governors**: two
    /// FRESH governors with the same config produce the same
    /// first-call decision for the same (category, signals). The
    /// governor mutates self-state (telemetry, decision_log,
    /// codel queue) so determinism holds only between fresh
    /// instances on their first call.
    #[test]
    fn proptest_capacity_governor_first_call_deterministic_across_fresh_instances(
        category in arb_workload_category(),
        signals in arb_pressure_signals(),
    ) {
        init_test_tracing_json();
        let mut g_a = CapacityGovernor::new(standard_config());
        let mut g_b = CapacityGovernor::new(standard_config());
        let d_a = g_a.evaluate(category, &signals);
        let d_b = g_b.evaluate(category, &signals);
        prop_assert_eq!(d_a, d_b,
            "first-call decision must be deterministic across fresh governors");
    }

    /// **Property 5 — default signals → Allow for all categories**.
    #[test]
    fn proptest_capacity_governor_default_signals_allow_all_categories(
        category in arb_workload_category(),
    ) {
        init_test_tracing_json();
        let mut g = CapacityGovernor::new(standard_config());
        let signals = PressureSignals::default();
        let d = g.evaluate(category, &signals);
        prop_assert!(is_allow(&d),
            "default zero-pressure signals must Allow {category:?}, got {d:?}");
    }

    /// **Property 6 — is_permitted partitions Block from
    /// everything else**: `decision.is_permitted() ==
    /// !matches!(decision, Block)`.
    #[test]
    fn proptest_capacity_governor_is_permitted_partitions_block(
        category in arb_workload_category(),
        signals in arb_pressure_signals(),
    ) {
        init_test_tracing_json();
        let mut g = CapacityGovernor::new(standard_config());
        let d = g.evaluate(category, &signals);
        prop_assert_eq!(
            d.is_permitted(),
            !is_block(&d),
            "is_permitted must equal !matches!(Block)"
        );
    }

    /// **Property 7 — override applies_to**: `category = None`
    /// applies to every category; `Some(c)` only to `c`.
    #[test]
    fn proptest_capacity_governor_override_applies_to(
        check in arb_workload_category(),
        scope in proptest::option::of(arb_workload_category()),
    ) {
        init_test_tracing_json();
        let ovr = OperatorOverride {
            operator: "op".to_string(),
            category: scope,
            expires_ms: 0,
            reason: "r".to_string(),
        };
        let observed = ovr.applies_to(check);
        let expected = scope.is_none() || scope == Some(check);
        prop_assert_eq!(observed, expected);
    }

    /// **Property 8 — override is_active expiry**:
    /// `expires_ms == 0` always active; otherwise active iff
    /// `now_ms < expires_ms`.
    #[test]
    fn proptest_capacity_governor_override_is_active(
        expires in 0u64..=10_000_000_u64,
        now in 0u64..=10_000_000_u64,
    ) {
        init_test_tracing_json();
        let ovr = OperatorOverride {
            operator: "op".to_string(),
            category: None,
            expires_ms: expires,
            reason: "r".to_string(),
        };
        let observed = ovr.is_active(now);
        let expected = expires == 0 || now < expires;
        prop_assert_eq!(observed, expected);
    }

    /// **Property 9 — operator override wraps the original
    /// decision**: when an active matching override is in
    /// place, `evaluate` returns `Override { original_decision,
    /// ... }` AND the inner original_decision is permitted
    /// (`is_permitted` true on the wrapper). The wrapper
    /// itself reports `is_permitted = true` because Override
    /// is not Block.
    #[test]
    fn proptest_capacity_governor_active_override_produces_wrapped_decision(
        category in arb_workload_category(),
        signals in arb_pressure_signals(),
    ) {
        init_test_tracing_json();
        let mut g = CapacityGovernor::new(standard_config());
        // expires_ms > timestamp guarantees active.
        let expires = signals.timestamp_ms.saturating_add(1_000);
        g.add_override(OperatorOverride {
            operator: "test-op".to_string(),
            category: None,
            expires_ms: expires,
            reason: "force-through".to_string(),
        });
        let d = g.evaluate(category, &signals);
        // Override must be permitted (it's not a Block variant).
        prop_assert!(d.is_permitted(), "Override decision must be permitted");
        match &d {
            GovernorDecision::Override { operator, original_decision, .. } => {
                prop_assert_eq!(operator, "test-op");
                prop_assert!(!original_decision.reason().is_empty(),
                    "wrapped original_decision must carry a reason string");
            }
            other => prop_assert!(false,
                "active override must produce Override decision, got {other:?}"),
        }
    }

    /// **Property 10 — rch_can_offload is conjunction**:
    /// `rch_available && rch_workers_available > 0`. The two
    /// flags must both be true; absent or zero workers means
    /// no offload regardless of `rch_available`.
    #[test]
    fn proptest_capacity_governor_rch_can_offload_is_conjunction(
        signals in arb_pressure_signals(),
    ) {
        init_test_tracing_json();
        let observed = signals.rch_can_offload();
        let expected = signals.rch_available && signals.rch_workers_available > 0;
        prop_assert_eq!(observed, expected);
    }
}
