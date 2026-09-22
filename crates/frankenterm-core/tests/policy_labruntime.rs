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
        let adapter = Arc::new(
            CaptureAdapter::new(sink.clone(), CaptureConfig::default())
                .expect("valid capture configuration"),
        );

        let mut injector = PolicyGatedInjector::new(
            PolicyEngine::strict(),
            frankenterm_core::wezterm::default_wezterm_handle(),
        );
        injector.set_decision_capture(adapter);

        let mut caps = PaneCapabilities::prompt();
        caps.is_reserved = Some(false);
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

#[test]
fn injector_refreshes_reservation_after_capability_snapshot() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let directory = tempfile::tempdir().expect("reservation fixture directory");
        let path = directory.path().join("reservation.db");
        let storage = frankenterm_core::storage::StorageHandle::new(path.to_str().unwrap())
            .await
            .expect("reservation fixture storage");
        let mock = Arc::new(frankenterm_core::wezterm::MockWezterm::new());
        let pane = mock.add_default_pane(42).await;
        let now = frankenterm_core::storage::now_ms();
        storage
            .upsert_pane(frankenterm_core::storage::PaneRecord {
                pane_id: 42,
                pane_uuid: None,
                domain: pane.domain,
                window_id: Some(pane.window_id),
                tab_id: Some(pane.tab_id),
                title: Some(pane.title),
                cwd: Some(pane.cwd),
                tty_name: None,
                first_seen_at: now,
                last_seen_at: now,
                observed: true,
                ignore_reason: None,
                last_decision_at: Some(now),
            })
            .await
            .expect("seed pane");
        assert!(storage.get_active_reservation(42).await.unwrap().is_none());
        let cached_free = PaneCapabilities {
            is_reserved: Some(false),
            ..PaneCapabilities::prompt()
        };
        let client: frankenterm_core::wezterm::WeztermHandle = mock.clone();
        let mut injector =
            PolicyGatedInjector::with_storage(PolicyEngine::permissive(), client, storage.clone());
        let reservation = storage
            .create_reservation(42, "workflow", "foreign-workflow", None, 60_000)
            .await
            .expect("reserve after capability snapshot");
        let before = mock.pane_state(42).await.unwrap().content;
        let blocked = injector
            .send_text(
                42,
                "echo blocked\n",
                ActorKind::Workflow,
                &cached_free,
                Some("caller"),
            )
            .await;
        match blocked {
            InjectionResult::Denied { decision, .. } => {
                assert_eq!(decision.rule_id(), Some("policy.pane_reserved"));
                let caps = &decision
                    .context()
                    .expect("reservation decision context")
                    .capabilities;
                assert_eq!(caps.is_reserved, Some(true));
                assert_eq!(caps.reserved_by.as_deref(), Some("foreign-workflow"));
            }
            other => panic!("new foreign reservation must block stale free capability: {other:?}"),
        }
        assert_eq!(mock.pane_state(42).await.unwrap().content, before);

        assert!(storage.release_reservation(reservation.id).await.unwrap());
        let allowed = injector
            .send_text(
                42,
                "echo released\n",
                ActorKind::Workflow,
                &cached_free,
                Some("caller"),
            )
            .await;
        assert!(
            matches!(allowed, InjectionResult::Allowed { .. }),
            "{allowed:?}"
        );
        let after = mock.pane_state(42).await.unwrap().content;
        assert!(after.contains("echo released"));

        // A cancelled reservation lookup must erase both a stale reserved bit
        // and a stale matching owner, even if another safety gate also denies.
        let cached_owner = PaneCapabilities {
            is_reserved: Some(true),
            reserved_by: Some("caller".to_string()),
            ..PaneCapabilities::prompt()
        };
        let cancelled = frankenterm_core::cx::for_testing();
        cancelled.cancel_with(
            frankenterm_core::outcome::CancelKind::User,
            Some("reservation refresh cancellation regression"),
        );
        assert!(
            storage
                .get_active_reservation_with_cx(&cancelled, 42)
                .await
                .is_err()
        );
        let unreadable = injector
            .send_text_with_cx(
                &cancelled,
                42,
                "echo unreadable\n",
                ActorKind::Workflow,
                &cached_owner,
                Some("caller"),
            )
            .await;
        match unreadable {
            InjectionResult::Denied { decision, .. } => {
                let caps = &decision
                    .context()
                    .expect("unreadable decision context")
                    .capabilities;
                assert_eq!(caps.is_reserved, None);
                assert_eq!(caps.reserved_by, None);
            }
            other => panic!("failed lookup must not reuse cached ownership: {other:?}"),
        }
        assert_eq!(mock.pane_state(42).await.unwrap().content, after);
        storage.shutdown().await.expect("close reservation storage");
    });
}
