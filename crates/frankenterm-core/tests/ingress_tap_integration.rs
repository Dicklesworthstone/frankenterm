//! Integration tests for ingress tap points (ft-oegrb.2.2).
//!
//! Verifies that the `IngressTap` fires correctly when integrated
//! with `PolicyGatedInjector` for all injection outcomes.

use std::sync::Arc;

use frankenterm_core::policy::{
    ActorKind, InjectionResult, PaneCapabilities, PolicyEngine, PolicyGatedInjector,
};
use frankenterm_core::recording::{
    IngressEvent, IngressOutcome, IngressTap, NoopTap, RecorderEventSource, RecorderIngressKind,
    SharedIngressTap,
};
use frankenterm_core::wezterm::{MockWezterm, WeztermHandle};

/// Test-local collecting tap (the library's CollectingTap is #[cfg(test)]
/// which is not visible to integration tests).
#[derive(Debug, Default)]
struct TestTap {
    events: std::sync::Mutex<Vec<IngressEvent>>,
}

impl TestTap {
    fn new() -> Self {
        Self::default()
    }

    fn events(&self) -> Vec<IngressEvent> {
        self.events.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

impl IngressTap for TestTap {
    fn on_ingress(&self, event: IngressEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// Capabilities that pass all policy checks (prompt active, alt-screen off).
fn safe_caps() -> PaneCapabilities {
    PaneCapabilities {
        prompt_active: true,
        alt_screen: Some(false),
        ..PaneCapabilities::default()
    }
}

#[tokio::test]
async fn tap_fires_on_allowed_send_text() {
    let mock = MockWezterm::new();
    mock.add_default_pane(1).await;
    let handle: WeztermHandle = Arc::new(mock);

    let tap = Arc::new(TestTap::new());
    let mut injector = PolicyGatedInjector::new(PolicyEngine::permissive(), handle);
    injector.set_ingress_tap(tap.clone());

    let caps = safe_caps();
    let result = injector
        .send_text(1, "ls -la\n", ActorKind::Robot, &caps, None)
        .await;
    assert!(
        matches!(result, InjectionResult::Allowed { .. }),
        "expected Allowed, got: {result:?}"
    );

    assert_eq!(tap.len(), 1);
    let event = &tap.events()[0];
    assert_eq!(event.pane_id, 1);
    assert_eq!(event.source, RecorderEventSource::RobotMode);
    assert_eq!(event.ingress_kind, RecorderIngressKind::SendText);
    assert_eq!(event.outcome, IngressOutcome::Allowed);
    assert!(event.workflow_id.is_none());
    assert!(event.occurred_at_ms > 0);
}

#[tokio::test]
async fn tap_fires_on_denied_alt_screen() {
    let mock = MockWezterm::new();
    mock.add_default_pane(1).await;
    let handle: WeztermHandle = Arc::new(mock);

    let tap = Arc::new(TestTap::new());
    let engine = PolicyEngine::strict();
    let mut injector = PolicyGatedInjector::new(engine, handle);
    injector.set_ingress_tap(tap.clone());

    let mut caps = safe_caps();
    caps.alt_screen = Some(true);
    let result = injector
        .send_text(1, "rm -rf /", ActorKind::Robot, &caps, None)
        .await;
    assert!(matches!(result, InjectionResult::Denied { .. }));

    assert_eq!(tap.len(), 1);
    assert!(matches!(
        tap.events()[0].outcome,
        IngressOutcome::Denied { .. }
    ));
}

#[tokio::test]
async fn tap_fires_with_workflow_id_and_correct_ingress_kind() {
    let mock = MockWezterm::new();
    mock.add_default_pane(1).await;
    let handle: WeztermHandle = Arc::new(mock);

    let tap = Arc::new(TestTap::new());
    let mut injector = PolicyGatedInjector::new(PolicyEngine::permissive(), handle);
    injector.set_ingress_tap(tap.clone());

    let caps = safe_caps();
    let result = injector
        .send_text(1, "echo hi", ActorKind::Workflow, &caps, Some("wf-42"))
        .await;
    assert!(
        matches!(result, InjectionResult::Allowed { .. }),
        "expected Allowed, got: {result:?}"
    );

    let event = &tap.events()[0];
    assert_eq!(event.ingress_kind, RecorderIngressKind::WorkflowAction);
    assert_eq!(event.workflow_id, Some("wf-42".into()));
    assert_eq!(event.outcome, IngressOutcome::Allowed);
    assert_eq!(event.source, RecorderEventSource::WorkflowEngine);
}

#[tokio::test]
async fn tap_fires_for_ctrl_c_requires_approval() {
    let mock = MockWezterm::new();
    mock.add_default_pane(1).await;
    let handle: WeztermHandle = Arc::new(mock);

    let tap = Arc::new(TestTap::new());
    let mut injector = PolicyGatedInjector::new(PolicyEngine::permissive(), handle);
    injector.set_ingress_tap(tap.clone());

    let caps = safe_caps();
    // Ctrl-C is considered destructive → requires approval
    let result = injector.send_ctrl_c(1, ActorKind::Robot, &caps, None).await;
    assert!(
        matches!(result, InjectionResult::RequiresApproval { .. }),
        "expected RequiresApproval, got: {result:?}"
    );

    assert_eq!(tap.len(), 1);
    assert_eq!(tap.events()[0].ingress_kind, RecorderIngressKind::SendText);
    assert_eq!(tap.events()[0].outcome, IngressOutcome::RequiresApproval);
}

#[tokio::test]
async fn no_tap_set_still_works() {
    let mock = MockWezterm::new();
    mock.add_default_pane(1).await;
    let handle: WeztermHandle = Arc::new(mock);

    // No tap set — should work without panicking
    let mut injector = PolicyGatedInjector::new(PolicyEngine::permissive(), handle);
    let caps = safe_caps();
    let result = injector
        .send_text(1, "hello", ActorKind::Human, &caps, None)
        .await;
    assert!(
        matches!(result, InjectionResult::Allowed { .. }),
        "expected Allowed, got: {result:?}"
    );
}

#[tokio::test]
async fn tap_records_human_actor_as_operator_action() {
    let mock = MockWezterm::new();
    mock.add_default_pane(1).await;
    let handle: WeztermHandle = Arc::new(mock);

    let tap = Arc::new(TestTap::new());
    let mut injector = PolicyGatedInjector::new(PolicyEngine::permissive(), handle);
    injector.set_ingress_tap(tap.clone());

    let caps = safe_caps();
    let _ = injector
        .send_text(1, "whoami", ActorKind::Human, &caps, None)
        .await;

    assert_eq!(tap.len(), 1);
    assert_eq!(tap.events()[0].source, RecorderEventSource::OperatorAction);
}

#[tokio::test]
async fn noop_tap_compiles_as_shared() {
    let _tap: SharedIngressTap = Arc::new(NoopTap);
    // Verifies NoopTap satisfies SharedIngressTap bounds
}

#[tokio::test]
async fn multiple_injections_accumulate_in_tap() {
    let mock = MockWezterm::new();
    mock.add_default_pane(1).await;
    let handle: WeztermHandle = Arc::new(mock);

    let tap = Arc::new(TestTap::new());
    let mut injector = PolicyGatedInjector::new(PolicyEngine::permissive(), handle);
    injector.set_ingress_tap(tap.clone());

    let caps = safe_caps();
    for i in 0..5 {
        let _ = injector
            .send_text(1, &format!("cmd-{i}"), ActorKind::Robot, &caps, None)
            .await;
    }

    assert_eq!(tap.len(), 5);
    for (i, event) in tap.events().iter().enumerate() {
        assert!(event.text.contains(&format!("cmd-{i}")));
    }
}
