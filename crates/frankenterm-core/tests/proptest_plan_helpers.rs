//! Property-based tests for `workflows::plan_helpers` public helpers.

use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::workflows::{
    BoxFuture, IdempotencyCheckResult, StepResult, Workflow, WorkflowContext, WorkflowStep,
    workflow_to_action_plan,
};
use proptest::prelude::*;

fn arb_step() -> impl Strategy<Value = WorkflowStep> {
    (
        "[A-Za-z0-9_.-]{1,24}",
        "[A-Za-z0-9 _,.:/-]{1,64}",
    )
        .prop_map(|(name, description)| WorkflowStep::new(name, description))
}

struct PlanHelperWorkflow {
    steps: Vec<WorkflowStep>,
}

impl Workflow for PlanHelperWorkflow {
    fn name(&self) -> &'static str {
        "plan_helper_workflow"
    }

    fn description(&self) -> &'static str {
        "Plan helper workflow"
    }

    fn handles(&self, detection: &Detection) -> bool {
        detection.agent_type == AgentType::Codex
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        self.steps.clone()
    }

    fn execute_step(
        &self,
        _ctx: &mut WorkflowContext,
        _step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        Box::pin(async { StepResult::cont() })
    }
}

fn arb_idempotency_result() -> impl Strategy<Value = IdempotencyCheckResult> {
    prop_oneof![
        Just(IdempotencyCheckResult::NotExecuted),
        (0i64..=4_102_444_800_000i64, prop::option::of("[A-Za-z0-9 _.,:-]{0,40}"))
            .prop_map(|(completed_at, previous_result)| IdempotencyCheckResult::AlreadyCompleted {
                completed_at,
                previous_result,
            }),
        (0i64..=4_102_444_800_000i64)
            .prop_map(|started_at| IdempotencyCheckResult::PartiallyExecuted { started_at }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn workflow_to_action_plan_preserves_core_metadata(
        steps in prop::collection::vec(arb_step(), 1..5),
        workspace_id in "[A-Za-z0-9_.-]{1,24}",
        execution_id in "[A-Za-z0-9_.-]{1,24}",
        pane_id in 1u64..10_000,
    ) {
        let workflow = PlanHelperWorkflow { steps: steps.clone() };
        let plan = workflow_to_action_plan(&workflow, &workspace_id, pane_id, &execution_id);

        prop_assert_eq!(plan.title, "Plan helper workflow");
        prop_assert_eq!(plan.workspace_id, workspace_id);
        prop_assert_eq!(plan.steps.len(), steps.len());
        prop_assert_eq!(&plan.metadata.as_ref().unwrap()["workflow_name"], "plan_helper_workflow");
        prop_assert_eq!(plan.metadata.as_ref().unwrap()["execution_id"].as_str(), Some(execution_id.as_str()));
        prop_assert_eq!(&plan.metadata.as_ref().unwrap()["pane_id"], pane_id);
        prop_assert_eq!(&plan.metadata.as_ref().unwrap()["generated_by"], "workflow_to_action_plan");
    }

    #[test]
    fn workflow_to_action_plan_step_payloads_follow_workflow_steps(
        steps in prop::collection::vec(arb_step(), 1..5),
        pane_id in 1u64..10_000,
    ) {
        let workflow = PlanHelperWorkflow { steps: steps.clone() };
        let plan = workflow_to_action_plan(&workflow, "ws-proptest", pane_id, "exec-proptest");

        prop_assert_eq!(plan.steps.len(), steps.len());
        for (idx, (step, plan_step)) in steps.iter().zip(plan.steps.iter()).enumerate() {
            prop_assert_eq!(plan_step.step_number, (idx + 1) as u32);
            prop_assert_eq!(&plan_step.description, &step.description);
            prop_assert!(!plan_step.step_id.0.is_empty());
        }
    }

    #[test]
    fn idempotency_check_result_clone_preserves_variant_fields(result in arb_idempotency_result()) {
        let cloned = result.clone();
        let debug = format!("{:?}", result);

        match (result, cloned) {
            (IdempotencyCheckResult::NotExecuted, IdempotencyCheckResult::NotExecuted) => {
                prop_assert!(debug.contains("NotExecuted"));
            }
            (
                IdempotencyCheckResult::AlreadyCompleted { completed_at, previous_result },
                IdempotencyCheckResult::AlreadyCompleted { completed_at: cloned_at, previous_result: cloned_result },
            ) => {
                prop_assert_eq!(completed_at, cloned_at);
                prop_assert_eq!(previous_result, cloned_result);
                prop_assert!(debug.contains("AlreadyCompleted"));
            }
            (
                IdempotencyCheckResult::PartiallyExecuted { started_at },
                IdempotencyCheckResult::PartiallyExecuted { started_at: cloned_at },
            ) => {
                prop_assert_eq!(started_at, cloned_at);
                prop_assert!(debug.contains("PartiallyExecuted"));
            }
            _ => prop_assert!(false, "cloned result changed variants"),
        }
    }
}

#[test]
fn plan_helper_workflow_handles_codex_only() {
    let workflow = PlanHelperWorkflow {
        steps: vec![WorkflowStep::new("step", "desc")],
    };

    let codex = Detection {
        rule_id: "codex.usage.reached".into(),
        agent_type: AgentType::Codex,
        event_type: "usage.limit".into(),
        severity: Severity::Warning,
        confidence: 0.9,
        extracted: serde_json::json!({}),
        matched_text: "usage reached".into(),
        span: (0, 13),
    };
    let claude = Detection {
        agent_type: AgentType::ClaudeCode,
        ..codex.clone()
    };

    assert!(workflow.handles(&codex));
    assert!(!workflow.handles(&claude));
}
