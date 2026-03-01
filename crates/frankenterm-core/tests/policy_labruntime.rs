//! LabRuntime-ported policy tests for deterministic async testing.
//!
//! Ports `#[tokio::test]` functions from `policy.rs` to asupersync-based
//! `RuntimeFixture`, gaining seed-based reproducibility for PolicyGatedInjector
//! operations.
//!
//! Only `#[tokio::test]` async functions are ported here. Plain `#[test]`
//! functions remain in the inline `mod tests` inside `policy.rs`.
//!
//! Tests that access private fields or methods of `PolicyGatedInjector`
//! (e.g. `e2e_trauma_guard_deny_injects_synthetic_feedback` and
//! `e2e_non_trauma_deny_does_not_inject_synthetic_feedback`) cannot be
//! ported to integration tests because they rely on private `client` field
//! access and the private `maybe_inject_trauma_feedback` method.
//!
//! Bead: ft-22x4r

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::policy::{
    ActorKind, InjectionResult, PaneCapabilities, PolicyEngine, PolicyGatedInjector,
};
use frankenterm_core::recording::RecorderEventPayload;
use frankenterm_core::replay_capture::{CaptureAdapter, CaptureConfig, CollectingCaptureSink};
use std::sync::Arc;

// ===========================================================================
// Section 1: PolicyGatedInjector replay capture tests
// ===========================================================================

/// Ported from `injector_emits_policy_decision_to_replay_capture`.
///
/// Verifies that when a policy deny is emitted (alt_screen active), the
/// decision event is captured by the replay capture adapter with the
/// correct `decision_type` and `rule_id` fields.
#[test]
fn injector_emits_policy_decision_to_replay_capture() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let sink = Arc::new(CollectingCaptureSink::new());
        let adapter = Arc::new(CaptureAdapter::new(
            sink.clone(),
            CaptureConfig::default(),
        ));

        let mut injector = PolicyGatedInjector::new(
            PolicyEngine::strict(),
            frankenterm_core::wezterm::default_wezterm_handle(),
        );
        injector.set_decision_capture(adapter);

        let mut caps = PaneCapabilities::prompt();
        caps.alt_screen = Some(true);

        let result = injector
            .send_text(1, "echo hi", ActorKind::Robot, &caps, None)
            .await;
        assert!(
            matches!(result, InjectionResult::Denied { .. }),
            "expected deny when alt_screen is active"
        );

        let events = sink.recorder_events();
        assert_eq!(events.len(), 1);
        match &events[0].payload {
            RecorderEventPayload::ControlMarker { details, .. } => {
                assert_eq!(details["decision_type"], "PolicyEvaluation");
                assert_eq!(details["rule_id"], "policy.alt_screen");
            }
            other => panic!("expected control marker, got {other:?}"),
        }
    });
}
