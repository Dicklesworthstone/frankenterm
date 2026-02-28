//! Property-based tests for command guard telemetry counters (ft-3kxe.27).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. evaluations tracks evaluate() calls
//! 3. allowed + blocked + warned = evaluations
//! 4. quick_rejects <= allowed
//! 5. audit_clears tracks clear_audit_log() calls
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::command_guard::{
    CommandGuard, CommandGuardTelemetrySnapshot, GuardPolicy, PaneGuardConfig, TrustLevel,
};

// =============================================================================
// Helpers
// =============================================================================

fn strict_policy() -> GuardPolicy {
    GuardPolicy {
        default_trust: TrustLevel::Strict,
        ..GuardPolicy::default()
    }
}

fn permissive_policy() -> GuardPolicy {
    GuardPolicy {
        default_trust: TrustLevel::Permissive,
        ..GuardPolicy::default()
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let guard = CommandGuard::with_defaults();
    let snap = guard.telemetry().snapshot();

    assert_eq!(snap.evaluations, 0);
    assert_eq!(snap.allowed, 0);
    assert_eq!(snap.blocked, 0);
    assert_eq!(snap.warned, 0);
    assert_eq!(snap.quick_rejects, 0);
    assert_eq!(snap.audit_clears, 0);
}

#[test]
fn safe_command_allowed() {
    let mut guard = CommandGuard::new(strict_policy());
    let decision = guard.evaluate("ls -la", 1);

    assert!(!decision.is_blocked());
    let snap = guard.telemetry().snapshot();
    assert_eq!(snap.evaluations, 1);
    assert_eq!(snap.allowed, 1);
    assert_eq!(snap.blocked, 0);
}

#[test]
fn destructive_command_blocked_strict() {
    let mut guard = CommandGuard::new(strict_policy());
    let decision = guard.evaluate("rm -rf /", 1);

    assert!(decision.is_blocked());
    let snap = guard.telemetry().snapshot();
    assert_eq!(snap.evaluations, 1);
    assert_eq!(snap.blocked, 1);
    assert_eq!(snap.allowed, 0);
}

#[test]
fn destructive_command_warned_permissive() {
    let mut guard = CommandGuard::new(permissive_policy());
    let decision = guard.evaluate("rm -rf /", 1);

    assert!(decision.is_warning());
    let snap = guard.telemetry().snapshot();
    assert_eq!(snap.evaluations, 1);
    assert_eq!(snap.warned, 1);
    assert_eq!(snap.blocked, 0);
}

#[test]
fn readonly_pane_always_allows() {
    let mut guard = CommandGuard::new(strict_policy());
    guard.set_pane_config(
        1,
        PaneGuardConfig {
            trust_level: TrustLevel::ReadOnly,
            ..PaneGuardConfig::default()
        },
    );

    guard.evaluate("rm -rf /", 1);

    let snap = guard.telemetry().snapshot();
    assert_eq!(snap.evaluations, 1);
    assert_eq!(snap.allowed, 1);
    assert_eq!(snap.blocked, 0);
}

#[test]
fn quick_reject_counted() {
    let mut guard = CommandGuard::new(strict_policy());
    // A benign command with no keywords should quick-reject
    guard.evaluate("echo hello world", 1);

    let snap = guard.telemetry().snapshot();
    assert_eq!(snap.quick_rejects, 1);
    assert_eq!(snap.allowed, 1);
}

#[test]
fn audit_clears_tracked() {
    let mut guard = CommandGuard::with_defaults();
    guard.evaluate("ls", 1);
    guard.clear_audit_log();
    guard.clear_audit_log();

    let snap = guard.telemetry().snapshot();
    assert_eq!(snap.audit_clears, 2);
}

#[test]
fn multiple_evaluations_sum() {
    let mut guard = CommandGuard::new(strict_policy());

    guard.evaluate("ls", 1);
    guard.evaluate("echo hi", 1);
    guard.evaluate("rm -rf /", 1);
    guard.evaluate("git status", 1);

    let snap = guard.telemetry().snapshot();
    assert_eq!(snap.evaluations, 4);
    assert_eq!(
        snap.allowed + snap.blocked + snap.warned,
        snap.evaluations,
        "allowed ({}) + blocked ({}) + warned ({}) != evaluations ({})",
        snap.allowed,
        snap.blocked,
        snap.warned,
        snap.evaluations,
    );
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = CommandGuardTelemetrySnapshot {
        evaluations: 1000,
        allowed: 800,
        blocked: 150,
        warned: 50,
        quick_rejects: 600,
        audit_clears: 10,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: CommandGuardTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn evaluations_equals_call_count(
        count in 1usize..30,
    ) {
        let mut guard = CommandGuard::new(strict_policy());
        for _ in 0..count {
            guard.evaluate("ls -la", 1);
        }
        let snap = guard.telemetry().snapshot();
        prop_assert_eq!(snap.evaluations, count as u64);
    }

    #[test]
    fn allowed_plus_blocked_plus_warned_equals_evaluations(
        n_safe in 0usize..10,
        n_destructive in 0usize..10,
    ) {
        let mut guard = CommandGuard::new(strict_policy());

        for _ in 0..n_safe {
            guard.evaluate("echo hello", 1);
        }
        for _ in 0..n_destructive {
            guard.evaluate("rm -rf /tmp/test", 1);
        }

        let snap = guard.telemetry().snapshot();
        prop_assert_eq!(
            snap.allowed + snap.blocked + snap.warned,
            snap.evaluations,
            "allowed ({}) + blocked ({}) + warned ({}) != evaluations ({})",
            snap.allowed, snap.blocked, snap.warned, snap.evaluations,
        );
    }

    #[test]
    fn quick_rejects_bounded_by_allowed(
        count in 1usize..30,
    ) {
        let mut guard = CommandGuard::new(strict_policy());
        for _ in 0..count {
            guard.evaluate("echo benign", 1);
        }
        let snap = guard.telemetry().snapshot();
        prop_assert!(
            snap.quick_rejects <= snap.allowed,
            "quick_rejects ({}) > allowed ({})",
            snap.quick_rejects, snap.allowed,
        );
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..30),
    ) {
        let mut guard = CommandGuard::new(strict_policy());
        let mut prev = guard.telemetry().snapshot();

        let commands = ["ls", "echo hi", "rm -rf /", "git push --force"];

        for op in &ops {
            match op {
                0 | 1 => {
                    guard.evaluate(commands[*op as usize], 1);
                }
                2 => {
                    guard.evaluate(commands[2], 1);
                }
                3 => {
                    guard.clear_audit_log();
                }
                _ => unreachable!(),
            }

            let snap = guard.telemetry().snapshot();
            prop_assert!(snap.evaluations >= prev.evaluations,
                "evaluations decreased: {} -> {}",
                prev.evaluations, snap.evaluations);
            prop_assert!(snap.allowed >= prev.allowed,
                "allowed decreased: {} -> {}",
                prev.allowed, snap.allowed);
            prop_assert!(snap.blocked >= prev.blocked,
                "blocked decreased: {} -> {}",
                prev.blocked, snap.blocked);
            prop_assert!(snap.warned >= prev.warned,
                "warned decreased: {} -> {}",
                prev.warned, snap.warned);
            prop_assert!(snap.quick_rejects >= prev.quick_rejects,
                "quick_rejects decreased: {} -> {}",
                prev.quick_rejects, snap.quick_rejects);
            prop_assert!(snap.audit_clears >= prev.audit_clears,
                "audit_clears decreased: {} -> {}",
                prev.audit_clears, snap.audit_clears);

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        evaluations in 0u64..100000,
        allowed in 0u64..50000,
        blocked in 0u64..50000,
        warned in 0u64..50000,
        quick_rejects in 0u64..50000,
        audit_clears in 0u64..10000,
    ) {
        let snap = CommandGuardTelemetrySnapshot {
            evaluations,
            allowed,
            blocked,
            warned,
            quick_rejects,
            audit_clears,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: CommandGuardTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
