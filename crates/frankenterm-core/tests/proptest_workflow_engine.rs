//! Property-based tests for the workflows/engine module.
//!
//! Covers WorkflowStepPolicyDecision, WorkflowStepPolicySummary, and
//! policy_summary_decision_is_allow: serde roundtrip, parse consistency,
//! is_allowed invariant, and redact_text_for_log length guarantees.
//! Also covers WorkflowEngine durable state-machine transitions and
//! snapshot/restore-style resume across storage reopen when the asupersync
//! runtime feature is enabled.
//!
//! Complements proptest_workflows.rs (StepResult, WaitCondition, locks) and
//! proptest_workflows_expanded.rs (DescriptorStep, ExecutionStatus, UnstickReport).

#[cfg(feature = "asupersync-runtime")]
mod common;

#[cfg(feature = "asupersync-runtime")]
use std::collections::BTreeSet;
#[cfg(feature = "asupersync-runtime")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "asupersync-runtime")]
use common::fixtures::RuntimeFixture;
use frankenterm_core::policy::ActionKind;
#[cfg(feature = "asupersync-runtime")]
use frankenterm_core::storage::{PaneRecord, StorageHandle, now_ms};
use frankenterm_core::workflows::{
    ExecutionStatus, StepResult, WorkflowStepPolicyDecision, WorkflowStepPolicySummary,
    policy_summary_decision_is_allow, redact_text_for_log,
};
#[cfg(feature = "asupersync-runtime")]
use frankenterm_core::workflows::{WaitCondition, WorkflowEngine};
use proptest::prelude::*;

#[cfg(feature = "asupersync-runtime")]
static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_policy_decision() -> impl Strategy<Value = WorkflowStepPolicyDecision> {
    prop_oneof![
        Just(WorkflowStepPolicyDecision::Allow),
        Just(WorkflowStepPolicyDecision::Deny),
        Just(WorkflowStepPolicyDecision::RequireApproval),
        Just(WorkflowStepPolicyDecision::Error),
    ]
}

fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::SendCtrlC),
        Just(ActionKind::SendCtrlD),
        Just(ActionKind::SendCtrlZ),
        Just(ActionKind::SendControl),
        Just(ActionKind::Spawn),
        Just(ActionKind::Split),
        Just(ActionKind::Activate),
        Just(ActionKind::Close),
        Just(ActionKind::BrowserAuth),
        Just(ActionKind::WorkflowRun),
        Just(ActionKind::ReservePane),
        Just(ActionKind::ReleasePane),
        Just(ActionKind::ReadOutput),
        Just(ActionKind::SearchOutput),
        Just(ActionKind::WriteFile),
        Just(ActionKind::DeleteFile),
        Just(ActionKind::ExecCommand),
        Just(ActionKind::ConnectorNotify),
        Just(ActionKind::ConnectorTicket),
        Just(ActionKind::ConnectorTriggerWorkflow),
        Just(ActionKind::ConnectorAuditLog),
        Just(ActionKind::ConnectorInvoke),
        Just(ActionKind::ConnectorCredentialAction),
    ]
}

fn arb_policy_summary() -> impl Strategy<Value = WorkflowStepPolicySummary> {
    (
        arb_policy_decision(),
        proptest::option::of(arb_action_kind()),
        proptest::option::of("[a-z._]{1,32}"),
        proptest::option::of("[a-zA-Z0-9 ]{1,64}"),
        proptest::option::of("[a-zA-Z0-9 ]{1,64}"),
        proptest::option::of("[a-zA-Z0-9 ]{1,64}"),
    )
        .prop_map(|(decision, action, rule_id, reason, summary, error)| {
            WorkflowStepPolicySummary {
                decision,
                action,
                rule_id,
                reason,
                summary,
                error,
                decision_context: None,
            }
        })
}

#[cfg(feature = "asupersync-runtime")]
#[derive(Debug, Clone)]
struct EngineTransition {
    status: ExecutionStatus,
    current_step: usize,
    wait_rule: Option<String>,
    error: Option<String>,
}

#[cfg(feature = "asupersync-runtime")]
#[derive(Debug, Clone)]
enum RestoreLoggedStep {
    Continue,
    Done,
    Retry,
    WaitFor,
    SendText,
    Abort,
    JumpTo(usize),
}

#[cfg(feature = "asupersync-runtime")]
impl RestoreLoggedStep {
    fn result(&self) -> StepResult {
        match self {
            Self::Continue => StepResult::cont(),
            Self::Done => StepResult::done(serde_json::json!({"restore": true})),
            Self::Retry => StepResult::retry(25),
            Self::WaitFor => StepResult::wait_for_with_timeout(
                WaitCondition::pattern("property.restore.wait"),
                1_000,
            ),
            Self::SendText => StepResult::send_text_and_wait(
                "echo restored",
                WaitCondition::pattern("property.restore.sent"),
                1_000,
            ),
            Self::Abort => StepResult::abort("restore abort marker"),
            Self::JumpTo(step) => StepResult::jump_to(*step),
        }
    }

    fn expected_next_step(&self, step_index: usize) -> usize {
        match self {
            Self::Continue | Self::Done => step_index + 1,
            Self::JumpTo(step) => *step,
            Self::Retry | Self::WaitFor | Self::SendText | Self::Abort => step_index,
        }
    }

    fn can_use_persisted_progress(&self) -> bool {
        matches!(self, Self::WaitFor | Self::SendText)
    }
}

#[cfg(feature = "asupersync-runtime")]
fn arb_execution_status() -> impl Strategy<Value = ExecutionStatus> {
    prop_oneof![
        Just(ExecutionStatus::Running),
        Just(ExecutionStatus::Waiting),
        Just(ExecutionStatus::Completed),
        Just(ExecutionStatus::Aborted),
    ]
}

#[cfg(feature = "asupersync-runtime")]
fn arb_restore_logged_step() -> impl Strategy<Value = RestoreLoggedStep> {
    prop_oneof![
        Just(RestoreLoggedStep::Continue),
        Just(RestoreLoggedStep::Done),
        Just(RestoreLoggedStep::Retry),
        Just(RestoreLoggedStep::WaitFor),
        Just(RestoreLoggedStep::SendText),
        Just(RestoreLoggedStep::Abort),
        (0usize..16).prop_map(RestoreLoggedStep::JumpTo),
    ]
}

#[cfg(feature = "asupersync-runtime")]
fn arb_engine_transition() -> impl Strategy<Value = EngineTransition> {
    (
        arb_execution_status(),
        0usize..64,
        proptest::option::of("[a-z][a-z0-9_.]{2,20}"),
        proptest::option::of("[a-zA-Z0-9 _.-]{1,48}"),
    )
        .prop_map(
            |(status, current_step, wait_rule, error)| EngineTransition {
                status,
                current_step,
                wait_rule: matches!(status, ExecutionStatus::Waiting)
                    .then_some(wait_rule)
                    .flatten(),
                error: matches!(status, ExecutionStatus::Aborted)
                    .then_some(error)
                    .flatten(),
            },
        )
}

#[cfg(feature = "asupersync-runtime")]
fn temp_db_path(label: &str) -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir()
        .join(format!(
            "workflow_engine_property_{label}_{counter}_{}.sqlite3",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

#[cfg(feature = "asupersync-runtime")]
async fn storage_for_path(db_path: &str) -> StorageHandle {
    StorageHandle::new(db_path)
        .await
        .expect("create workflow engine property storage at explicit path")
}

#[cfg(feature = "asupersync-runtime")]
async fn storage_for(label: &str) -> StorageHandle {
    StorageHandle::new(&temp_db_path(label))
        .await
        .expect("create workflow engine property storage")
}

#[cfg(feature = "asupersync-runtime")]
fn incomplete_status(waiting: bool) -> ExecutionStatus {
    if waiting {
        ExecutionStatus::Waiting
    } else {
        ExecutionStatus::Running
    }
}

#[cfg(feature = "asupersync-runtime")]
async fn seed_pane(storage: &StorageHandle, pane_id: u64) {
    let now = now_ms();
    storage
        .upsert_pane(PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some(format!("workflow-engine-property-{pane_id}")),
            cwd: Some("/tmp/frankenterm-workflow-engine-property".to_string()),
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        })
        .await
        .expect("seed pane referenced by workflow engine property");
}

#[cfg(feature = "asupersync-runtime")]
fn status_storage_name(status: ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Running => "running",
        ExecutionStatus::Waiting => "waiting",
        ExecutionStatus::Completed => "completed",
        ExecutionStatus::Aborted => "aborted",
    }
}

#[cfg(feature = "asupersync-runtime")]
fn status_is_incomplete(status: ExecutionStatus) -> bool {
    matches!(status, ExecutionStatus::Running | ExecutionStatus::Waiting)
}

// ── WorkflowEngine durable state machine ────────────────────────────────────

#[cfg(feature = "asupersync-runtime")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    // Property 1: Generated status-transition sequences persist exactly one
    // durable final state, classify incomplete workflows consistently, and only
    // resume running/waiting executions.
    #[test]
    fn engine_state_machine_persists_resume_contract(
        pane_id in 1u64..10_000,
        transitions in proptest::collection::vec(arb_engine_transition(), 1..10),
    ) {
        let fixture = RuntimeFixture::current_thread();
        let result: Result<(), proptest::test_runner::TestCaseError> = fixture.block_on(async move {
            let storage = storage_for("state_machine").await;
            seed_pane(&storage, pane_id).await;
            let engine = WorkflowEngine::default();
            let cx = frankenterm_core::cx::for_request();
            let execution_id = format!(
                "property-engine-state-machine-{pane_id}-{}",
                transitions.len()
            );

            engine
                .start_with_id_cx(
                    &cx,
                    &storage,
                    execution_id.clone(),
                    "property_engine_state_machine",
                    pane_id,
                    None,
                    Some(serde_json::json!({"case": transitions.len()})),
                )
                .await
                .expect("start workflow engine property execution");

            for transition in &transitions {
                let wait_condition = transition
                    .wait_rule
                    .as_ref()
                    .map(|rule_id| WaitCondition::pattern(rule_id.clone()));
                engine
                    .update_status_cx(
                        &cx,
                        &storage,
                        &execution_id,
                        transition.status,
                        transition.current_step,
                        wait_condition.as_ref(),
                        transition.error.as_deref(),
                    )
                    .await
                    .expect("apply generated workflow engine transition");

                let record = storage
                    .get_workflow_with_cx(&cx, &execution_id)
                    .await
                    .expect("load workflow after generated transition")
                    .expect("workflow record remains durable");
                prop_assert_eq!(record.status.as_str(), status_storage_name(transition.status));
                prop_assert_eq!(record.current_step, transition.current_step);
                prop_assert_eq!(record.wait_condition, wait_condition.map(|condition| {
                    serde_json::to_value(condition).expect("serialize wait condition")
                }));
                prop_assert_eq!(record.error.as_deref(), transition.error.as_deref());
                prop_assert_eq!(
                    record.completed_at.is_some(),
                    !status_is_incomplete(transition.status),
                    "terminal states must set completed_at and incomplete states must clear it"
                );
            }

            let final_transition = transitions
                .last()
                .expect("proptest generated at least one transition");
            let incomplete_ids = engine
                .find_incomplete_cx(&cx, &storage)
                .await
                .expect("find incomplete workflows")
                .into_iter()
                .map(|record| record.id)
                .collect::<BTreeSet<_>>();
            prop_assert_eq!(
                incomplete_ids.contains(&execution_id),
                status_is_incomplete(final_transition.status),
                "find_incomplete must agree with the final durable state"
            );

            let resumed = engine
                .resume_cx(&cx, &storage, &execution_id)
                .await
                .expect("resume generated workflow");
            if status_is_incomplete(final_transition.status) {
                let (execution, next_step) =
                    resumed.expect("running/waiting workflow should resume");
                prop_assert_eq!(execution.status, final_transition.status);
                prop_assert_eq!(execution.current_step, final_transition.current_step);
                prop_assert_eq!(next_step, final_transition.current_step);
            } else {
                prop_assert!(
                    resumed.is_none(),
                    "completed/aborted workflow must not resume: {resumed:?}"
                );
            }

            Ok(())
        });

        result?;
    }
}

#[cfg(feature = "asupersync-runtime")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // Property 2: A workflow interrupted after durable step logs can be
    // restored by reopening storage and resuming from only the persisted
    // workflow record plus step-log snapshot.
    #[test]
    fn engine_snapshot_restore_reopens_storage_resume_contract(
        pane_id in 1u64..10_000,
        waiting in any::<bool>(),
        persisted_progress in any::<bool>(),
        logs in proptest::collection::vec(arb_restore_logged_step(), 1..8),
        current_step_seed in 0usize..32,
    ) {
        let fixture = RuntimeFixture::current_thread();
        let result: Result<(), proptest::test_runner::TestCaseError> = fixture.block_on(async move {
            let db_path = temp_db_path("snapshot_restore");
            let storage = storage_for_path(&db_path).await;
            seed_pane(&storage, pane_id).await;
            let engine = WorkflowEngine::default();
            let cx = frankenterm_core::cx::for_request();
            let execution_id = format!(
                "property-engine-restore-{pane_id}-{}",
                logs.len()
            );

            engine
                .start_with_id_cx(
                    &cx,
                    &storage,
                    execution_id.clone(),
                    "property_engine_snapshot_restore",
                    pane_id,
                    None,
                    Some(serde_json::json!({"restore_logs": logs.len()})),
                )
                .await
                .expect("start workflow engine restore property execution");

            for (step_index, logged_step) in logs.iter().enumerate() {
                engine
                    .log_step_cx(
                        &cx,
                        &storage,
                        &execution_id,
                        step_index,
                        "restore_step",
                        &logged_step.result(),
                        now_ms(),
                    )
                    .await
                    .expect("persist generated restore step log");
            }

            let final_status = incomplete_status(waiting);
            let last_index = logs.len() - 1;
            let last_step = logs
                .last()
                .expect("proptest generated at least one restore log");
            let current_step = if persisted_progress
                && final_status == ExecutionStatus::Running
                && last_step.can_use_persisted_progress()
            {
                last_index + 1
            } else {
                current_step_seed
            };
            let wait_condition = (final_status == ExecutionStatus::Waiting)
                .then(|| WaitCondition::pattern("property.restore.waiting"));

            engine
                .update_status_cx(
                    &cx,
                    &storage,
                    &execution_id,
                    final_status,
                    current_step,
                    wait_condition.as_ref(),
                    None,
                )
                .await
                .expect("persist generated restore status");
            let before_logs = storage
                .get_step_logs_with_cx(&cx, &execution_id)
                .await
                .expect("load step logs before restore");
            prop_assert_eq!(before_logs.len(), logs.len());

            storage.shutdown().await.expect("flush storage snapshot");

            let restored_storage = storage_for_path(&db_path).await;
            let restored_cx = frankenterm_core::cx::for_request();
            let restored_engine = WorkflowEngine::new(11);
            let restored_logs = restored_storage
                .get_step_logs_with_cx(&restored_cx, &execution_id)
                .await
                .expect("load step logs after storage restore");
            prop_assert_eq!(
                restored_logs.len(),
                before_logs.len(),
                "restore must preserve every durable step log"
            );
            prop_assert_eq!(
                restored_logs.last().map(|log| log.result_type.as_str()),
                before_logs.last().map(|log| log.result_type.as_str()),
                "restore must preserve the last step result type that drives resume"
            );

            let incomplete_ids = restored_engine
                .find_incomplete_cx(&restored_cx, &restored_storage)
                .await
                .expect("find incomplete after storage restore")
                .into_iter()
                .map(|record| record.id)
                .collect::<BTreeSet<_>>();
            prop_assert!(
                incomplete_ids.contains(&execution_id),
                "running/waiting workflow must be discoverable after restore"
            );

            let (execution, next_step) = restored_engine
                .resume_cx(&restored_cx, &restored_storage, &execution_id)
                .await
                .expect("resume after storage restore")
                .expect("incomplete workflow should resume after restore");
            let expected_next_step = if final_status == ExecutionStatus::Running
                && wait_condition.is_none()
                && last_step.can_use_persisted_progress()
                && current_step == last_index + 1
            {
                current_step
            } else {
                last_step.expected_next_step(last_index)
            };

            prop_assert_eq!(execution.id, execution_id);
            prop_assert_eq!(execution.workflow_name.as_str(), "property_engine_snapshot_restore");
            prop_assert_eq!(execution.pane_id, pane_id);
            prop_assert_eq!(execution.status, final_status);
            prop_assert_eq!(execution.current_step, expected_next_step);
            prop_assert_eq!(next_step, expected_next_step);

            restored_storage
                .shutdown()
                .await
                .expect("shutdown restored storage handle");
            Ok(())
        });

        result?;
    }
}

// ── WorkflowStepPolicyDecision ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Property 3: All WorkflowStepPolicyDecision variants survive serde roundtrip.
    #[test]
    fn policy_decision_serde_roundtrip(decision in arb_policy_decision()) {
        let json = serde_json::to_string(&decision).unwrap();
        let parsed: WorkflowStepPolicyDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decision, parsed);
    }

    // Property 2: is_allowed is true IFF decision is Allow.
    #[test]
    fn policy_decision_is_allowed_iff_allow(decision in arb_policy_decision()) {
        let expected = matches!(decision, WorkflowStepPolicyDecision::Allow);
        prop_assert_eq!(decision.is_allowed(), expected);
    }

    // Property 3: Decision JSON is always a quoted snake_case string.
    #[test]
    fn policy_decision_json_is_quoted_string(decision in arb_policy_decision()) {
        let json = serde_json::to_string(&decision).unwrap();
        prop_assert!(json.starts_with('"'));
        prop_assert!(json.ends_with('"'));
        let inner = &json[1..json.len()-1];
        let valid = ["allow", "deny", "require_approval", "error"];
        let check = valid.contains(&inner);
        prop_assert!(check, "unexpected decision JSON: {}", json);
    }
}

// ── WorkflowStepPolicySummary ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Property 4: WorkflowStepPolicySummary survives serde roundtrip.
    #[test]
    fn policy_summary_serde_roundtrip(summary in arb_policy_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: WorkflowStepPolicySummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(summary.decision, parsed.decision);
        prop_assert_eq!(summary.action, parsed.action);
        prop_assert_eq!(summary.rule_id, parsed.rule_id);
        prop_assert_eq!(summary.reason, parsed.reason);
        prop_assert_eq!(summary.summary, parsed.summary);
        prop_assert_eq!(summary.error, parsed.error);
    }

    // Property 5: WorkflowStepPolicySummary::parse inverts to_string.
    #[test]
    fn policy_summary_parse_inverts_serialize(summary in arb_policy_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let parsed = WorkflowStepPolicySummary::parse(&json);
        prop_assert!(parsed.is_some(), "parse failed on valid JSON: {}", json);
        let parsed = parsed.unwrap();
        prop_assert_eq!(summary.decision, parsed.decision);
        prop_assert_eq!(summary.action, parsed.action);
        prop_assert_eq!(summary.rule_id, parsed.rule_id);
    }

    // Property 6: is_allowed on summary matches decision.is_allowed.
    #[test]
    fn policy_summary_is_allowed_delegates_to_decision(summary in arb_policy_summary()) {
        prop_assert_eq!(summary.is_allowed(), summary.decision.is_allowed());
    }

    // Property 7: policy_summary_decision_is_allow agrees with typed parse.
    #[test]
    fn policy_summary_decision_fn_agrees_with_parse(summary in arb_policy_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let typed_result = summary.decision.is_allowed();
        let fn_result = policy_summary_decision_is_allow(&json);
        prop_assert_eq!(fn_result, Some(typed_result));
    }

    // Property 8: Optional fields serialize to absent keys (skip_serializing_if).
    #[test]
    fn policy_summary_none_fields_omitted(decision in arb_policy_decision()) {
        let summary = WorkflowStepPolicySummary {
            decision,
            action: None,
            rule_id: None,
            reason: None,
            summary: None,
            error: None,
            decision_context: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = parsed.as_object().unwrap();
        // Only the "decision" key should be present; all optional fields omitted.
        prop_assert!(!obj.contains_key("action"), "action should be omitted: {}", json);
        prop_assert!(!obj.contains_key("rule_id"), "rule_id should be omitted: {}", json);
        prop_assert!(!obj.contains_key("reason"), "reason should be omitted: {}", json);
        prop_assert!(!obj.contains_key("summary"), "summary should be omitted: {}", json);
        prop_assert!(!obj.contains_key("error"), "error should be omitted: {}", json);
        prop_assert!(!obj.contains_key("decision_context"), "decision_context should be omitted: {}", json);
        prop_assert_eq!(obj.len(), 1, "only 'decision' key expected: {}", json);
    }
}

// ── policy_summary_decision_is_allow ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Property 9: Legacy untyped JSON with "decision":"allow" returns Some(true).
    #[test]
    fn legacy_json_allow_returns_true(
        extra_key in "[a-z]{1,8}",
        extra_val in "[a-z]{1,8}",
    ) {
        let json = format!(r#"{{"decision":"allow","{}":"{}"}}"#, extra_key, extra_val);
        let result = policy_summary_decision_is_allow(&json);
        prop_assert_eq!(result, Some(true));
    }

    // Property 10: Legacy untyped JSON with "decision":"deny" returns Some(false).
    #[test]
    fn legacy_json_deny_returns_false(
        extra_key in "[a-z]{1,8}",
        extra_val in "[a-z]{1,8}",
    ) {
        let json = format!(r#"{{"decision":"deny","{}":"{}"}}"#, extra_key, extra_val);
        let result = policy_summary_decision_is_allow(&json);
        prop_assert_eq!(result, Some(false));
    }

    // Property 11: Non-JSON input returns None.
    #[test]
    fn non_json_returns_none(input in "[^{\"]*") {
        // Filter out accidental valid JSON
        if serde_json::from_str::<serde_json::Value>(&input).is_err() {
            let result = policy_summary_decision_is_allow(&input);
            prop_assert_eq!(result, None);
        }
    }
}

// ── redact_text_for_log ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // Property 12: Output char count never exceeds max_len + 3 (for "..." suffix).
    #[test]
    fn redact_output_length_bounded(
        text in ".{0,256}",
        max_len in 1usize..=256,
    ) {
        let result = redact_text_for_log(&text, max_len);
        let char_count = result.chars().count();
        // If truncated, output is max_len chars + "..."
        prop_assert!(
            char_count <= max_len + 3,
            "output too long: {} chars for max_len={}, output={:?}",
            char_count, max_len, result
        );
    }

    // Property 13: Short text passes through unmodified (after redaction).
    #[test]
    fn redact_short_text_no_truncation(
        text in "[a-zA-Z0-9 ]{0,20}",
        max_len in 20usize..=100,
    ) {
        let result = redact_text_for_log(&text, max_len);
        // The redactor won't find secrets in alphanumeric text,
        // so the output should match the input exactly.
        prop_assert_eq!(result, text);
    }

    // Property 14: Truncated output always ends with "...".
    #[test]
    fn redact_truncated_has_ellipsis(
        text in ".{10,256}",
        max_len in 1usize..=5,
    ) {
        let result = redact_text_for_log(&text, max_len);
        // If the redacted text was longer than max_len, it gets truncated
        if result.len() > max_len {
            prop_assert!(
                result.ends_with("..."),
                "truncated output should end with '...': {:?}", result
            );
        }
    }

    // Property 15: Empty text always returns empty regardless of max_len.
    #[test]
    fn redact_empty_always_empty(max_len in 0usize..=100) {
        let result = redact_text_for_log("", max_len);
        prop_assert_eq!(result, "");
    }

    // Property 16: Result is deterministic — same input gives same output.
    #[test]
    fn redact_deterministic(
        text in ".{0,128}",
        max_len in 1usize..=128,
    ) {
        let r1 = redact_text_for_log(&text, max_len);
        let r2 = redact_text_for_log(&text, max_len);
        prop_assert_eq!(r1, r2);
    }
}

// ── WorkflowStepPolicySummary + ActionKind serde cross-validation ───────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Property 17: ActionKind survives roundtrip through WorkflowStepPolicySummary serialization.
    #[test]
    fn action_kind_survives_summary_roundtrip(action in arb_action_kind()) {
        let summary = WorkflowStepPolicySummary {
            decision: WorkflowStepPolicyDecision::Allow,
            action: Some(action),
            rule_id: None,
            reason: None,
            summary: None,
            error: None,
            decision_context: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed = WorkflowStepPolicySummary::parse(&json).unwrap();
        prop_assert_eq!(parsed.action, Some(action));
    }

    // Property 18: Decision + action combination always serializes and parses.
    #[test]
    fn decision_action_combination_roundtrips(
        decision in arb_policy_decision(),
        action in arb_action_kind(),
        rule_id in proptest::option::of("[a-z.]{1,20}"),
    ) {
        let summary = WorkflowStepPolicySummary {
            decision,
            action: Some(action),
            rule_id: rule_id.clone(),
            reason: None,
            summary: None,
            error: None,
            decision_context: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed = WorkflowStepPolicySummary::parse(&json).unwrap();
        prop_assert_eq!(parsed.decision, decision);
        prop_assert_eq!(parsed.action, Some(action));
        prop_assert_eq!(parsed.rule_id, rule_id);
    }
}
