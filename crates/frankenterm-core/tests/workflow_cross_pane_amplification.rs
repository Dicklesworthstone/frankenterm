//! Regression test for ft-j0ufc — workflow runner privilege amplification by
//! pattern injection.
//!
//! Threat: a low-trust pane A can produce output that matches a workflow's
//! detection pattern. The pre-fix runner did presence-only matching ("pattern
//! matched? then trigger"), so the workflow would acquire a lock on (and
//! drive actions against) whichever pane the matching detection arrived for —
//! even when the workflow's operator only intended high-trust panes to be
//! eligible to fire it.
//!
//! Fix: `Workflow::trigger_policy()` returns a `WorkflowTriggerPolicy` whose
//! optional `allowed_source_panes` allowlist is enforced inside
//! `WorkflowRunner::handle_detection` (and its cx-first sibling) BEFORE any
//! lock, audit row, or engine state is created. A refused trigger surfaces
//! the new `WorkflowStartResult::SourcePaneNotTrusted` variant.
//!
//! These tests prove:
//!   1. A workflow with no override (default `allow_all()`) still fires on
//!      every source pane (backwards compatibility — the pre-fix behavior).
//!   2. A workflow with an allowlist that excludes pane A REFUSES the
//!      trigger when pane A produces matching text, and emits
//!      `SourcePaneNotTrusted` carrying the source pane id, workflow name,
//!      and rule id (for audit/forensics).
//!   3. The same allowlist still PERMITS triggers from a pane that IS in the
//!      set, so the policy is a positive allowlist rather than a coarse
//!      kill-switch.
//!   4. The refused-trigger path takes NO lock — i.e. the lock manager is
//!      observably idle after the refusal — so an attacker cannot starve a
//!      legitimate operator's pane via this path.

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::fixtures::RuntimeFixture;
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::policy::{PolicyEngine, PolicyGatedInjector};
use frankenterm_core::storage::{PaneRecord, StorageHandle, now_ms};
use frankenterm_core::wezterm::{MockWezterm, WeztermHandle};
use frankenterm_core::workflows::{
    BoxFuture, CxPolicyInjector, PaneWorkflowLockManager, StepResult, Workflow, WorkflowContext,
    WorkflowEngine, WorkflowRunner, WorkflowRunnerConfig, WorkflowStartResult, WorkflowStep,
    WorkflowTriggerPolicy,
};

const TRIGGER_RULE: &str = "j0ufc.regression.compaction_continue";

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path(label: &str) -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir()
        .join(format!(
            "wf_xpane_amp_{label}_{counter}_{}.db",
            std::process::id()
        ))
        .to_str()
        .unwrap()
        .to_string()
}

/// Synthetic workflow used by the regression test. Optionally restricts the
/// set of source panes that may fire it.
struct TrustScopedWorkflow {
    policy: WorkflowTriggerPolicy,
}

impl TrustScopedWorkflow {
    fn allow_all() -> Self {
        Self {
            policy: WorkflowTriggerPolicy::allow_all(),
        }
    }

    fn allowlist(panes: &[u64]) -> Self {
        Self {
            policy: WorkflowTriggerPolicy::allowlist(panes.iter().copied()),
        }
    }
}

impl Workflow for TrustScopedWorkflow {
    fn name(&self) -> &'static str {
        "j0ufc_regression_workflow"
    }

    fn description(&self) -> &'static str {
        "ft-j0ufc regression: source-pane allowlist enforcement"
    }

    fn handles(&self, detection: &Detection) -> bool {
        detection.rule_id == TRIGGER_RULE
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![WorkflowStep::new("noop", "Marker step (never runs)")]
    }

    fn trigger_policy(&self) -> WorkflowTriggerPolicy {
        self.policy.clone()
    }

    fn execute_step(
        &self,
        _ctx: &mut WorkflowContext,
        _step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        Box::pin(async move { StepResult::done(serde_json::json!({"ok": true})) })
    }
}

fn make_detection() -> Detection {
    Detection {
        rule_id: TRIGGER_RULE.to_string(),
        agent_type: AgentType::ClaudeCode,
        event_type: "compaction".to_string(),
        severity: Severity::Info,
        confidence: 1.0,
        extracted: serde_json::Value::Null,
        matched_text: "Compacting...".to_string(),
        span: (0, 12),
    }
}

async fn upsert_test_pane(storage: &StorageHandle, pane_id: u64) {
    storage
        .upsert_pane(PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("test".to_string()),
            cwd: Some("/tmp".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        })
        .await
        .unwrap();
}

async fn build_runner(db_path: &str) -> (WorkflowRunner, Arc<PaneWorkflowLockManager>) {
    let engine = WorkflowEngine::default();
    let lock_manager = Arc::new(PaneWorkflowLockManager::new());
    let storage = Arc::new(StorageHandle::new(db_path).await.unwrap());

    let mock = MockWezterm::new();
    // Pre-register both attacker and victim pane ids so that any send_text
    // attempt would actually land — this isolates the test to the policy
    // gate rather than a spurious "pane not found" failure.
    for pid in [10u64, 20u64, 30u64] {
        mock.add_default_pane(pid).await;
        upsert_test_pane(&storage, pid).await;
    }
    let handle: WeztermHandle = Arc::new(mock);

    let injector =
        CxPolicyInjector::new(PolicyGatedInjector::new(PolicyEngine::permissive(), handle));

    let runner = WorkflowRunner::new(
        engine,
        Arc::clone(&lock_manager),
        storage,
        injector,
        WorkflowRunnerConfig::default(),
    );

    (runner, lock_manager)
}

#[test]
fn default_policy_allows_any_source_pane() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let db = temp_db_path("default_allow");
        let (runner, _lock_manager) = build_runner(&db).await;
        runner.register_workflow(Arc::new(TrustScopedWorkflow::allow_all()));

        // Untrusted pane id 10 should still fire under the legacy default.
        let detection = make_detection();
        let result = runner.handle_detection(10, &detection, None).await;
        assert!(
            result.is_started(),
            "default trigger_policy must remain backwards-compatible \
             (any source pane allowed); got {result:?}"
        );
    });
}

#[test]
fn allowlist_refuses_untrusted_source_pane() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let db = temp_db_path("refused");
        let (runner, lock_manager) = build_runner(&db).await;
        // Only pane 20 is trusted; pane 10 is the attacker.
        runner.register_workflow(Arc::new(TrustScopedWorkflow::allowlist(&[20])));

        let detection = make_detection();
        let result = runner.handle_detection(10, &detection, None).await;

        match result {
            WorkflowStartResult::SourcePaneNotTrusted {
                source_pane_id,
                workflow_name,
                rule_id,
            } => {
                assert_eq!(source_pane_id, 10);
                assert_eq!(workflow_name, "j0ufc_regression_workflow");
                assert_eq!(rule_id, TRIGGER_RULE);
            }
            other => panic!(
                "expected SourcePaneNotTrusted for non-allowlisted source pane; \
                 got {other:?}"
            ),
        }

        // No lock must have been acquired on either pane — the refusal
        // happens before lock acquisition, so an attacker cannot starve
        // a legitimate operator via this path.
        assert!(
            lock_manager.is_locked(10).is_none(),
            "refused trigger must not lock the source pane"
        );
        assert!(
            lock_manager.is_locked(20).is_none(),
            "refused trigger must not lock any other pane"
        );
    });
}

#[test]
fn allowlist_permits_trusted_source_pane() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let db = temp_db_path("permitted");
        let (runner, _lock_manager) = build_runner(&db).await;
        runner.register_workflow(Arc::new(TrustScopedWorkflow::allowlist(&[20])));

        // Pane 20 IS in the allowlist — trigger must proceed past the
        // ft-j0ufc gate (i.e. produce a Started result, not a refusal).
        let detection = make_detection();
        let result = runner.handle_detection(20, &detection, None).await;
        assert!(
            result.is_started(),
            "allowlisted source pane must still be permitted; got {result:?}"
        );
        assert!(
            !result.is_source_pane_not_trusted(),
            "allowlisted source pane must not be refused; got {result:?}"
        );
    });
}

#[test]
fn workflow_trigger_policy_predicate_unit() {
    // Pure predicate behavior — no runtime needed. Guards against drift
    // in `WorkflowTriggerPolicy::allows_source_pane` independently of the
    // runner integration above.
    let allow_all = WorkflowTriggerPolicy::allow_all();
    assert!(allow_all.allows_source_pane(0));
    assert!(allow_all.allows_source_pane(u64::MAX));

    let scoped = WorkflowTriggerPolicy::allowlist([1u64, 2, 3]);
    assert!(scoped.allows_source_pane(2));
    assert!(!scoped.allows_source_pane(4));
    assert!(!scoped.allows_source_pane(0));
}
