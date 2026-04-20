//! Property-based tests for `workflows::traits` public metadata helpers.

use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::plan::StepAction;
use frankenterm_core::workflows::{
    BoxFuture, StepResult, Workflow, WorkflowContext, WorkflowInfo, WorkflowStep,
};
use proptest::prelude::*;

fn arb_step() -> impl Strategy<Value = WorkflowStep> {
    (
        "[A-Za-z0-9_.-]{1,24}",
        "[A-Za-z0-9 _,.:/-]{1,64}",
    )
        .prop_map(|(name, description)| WorkflowStep::new(name, description))
}

struct StaticWorkflow {
    steps: Vec<WorkflowStep>,
    requires_pane: bool,
    requires_approval: bool,
    can_abort: bool,
    destructive: bool,
}

impl Workflow for StaticWorkflow {
    fn name(&self) -> &'static str {
        "static_workflow"
    }

    fn description(&self) -> &'static str {
        "Synthetic workflow for property tests"
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

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["session.compaction", "usage.limits"]
    }

    fn trigger_rule_ids(&self) -> &'static [&'static str] {
        &["codex.usage.reached", "codex.compaction.prompt"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["codex", "claude_code"]
    }

    fn requires_pane(&self) -> bool {
        self.requires_pane
    }

    fn requires_approval(&self) -> bool {
        self.requires_approval
    }

    fn can_abort(&self) -> bool {
        self.can_abort
    }

    fn is_destructive(&self) -> bool {
        self.destructive
    }

    fn dependencies(&self) -> &'static [&'static str] {
        &["handle_auth_required", "handle_usage_limits"]
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn workflow_step_json_roundtrip_preserves_fields(step in arb_step()) {
        let value = serde_json::to_value(&step).unwrap();
        let decoded: WorkflowStep = serde_json::from_value(value).unwrap();

        prop_assert_eq!(decoded.name, step.name);
        prop_assert_eq!(decoded.description, step.description);
    }

    #[test]
    fn workflow_info_from_workflow_reflects_trait_metadata(
        steps in prop::collection::vec(arb_step(), 1..5),
        requires_pane in any::<bool>(),
        requires_approval in any::<bool>(),
        can_abort in any::<bool>(),
        destructive in any::<bool>(),
    ) {
        let workflow = StaticWorkflow {
            steps: steps.clone(),
            requires_pane,
            requires_approval,
            can_abort,
            destructive,
        };

        let info = WorkflowInfo::from_workflow(&workflow);

        prop_assert_eq!(info.name, "static_workflow");
        prop_assert_eq!(info.description, "Synthetic workflow for property tests");
        prop_assert_eq!(info.enabled, true);
        prop_assert_eq!(info.step_count, steps.len());
        prop_assert_eq!(info.requires_pane, requires_pane);
        prop_assert_eq!(info.requires_approval, requires_approval);
        prop_assert_eq!(info.can_abort, can_abort);
        prop_assert_eq!(info.destructive, destructive);
        prop_assert_eq!(info.trigger_event_types, vec!["session.compaction", "usage.limits"]);
        prop_assert_eq!(info.trigger_rule_ids, vec!["codex.usage.reached", "codex.compaction.prompt"]);
        prop_assert_eq!(info.agent_types, vec!["codex", "claude_code"]);
        prop_assert_eq!(info.dependencies, vec!["handle_auth_required", "handle_usage_limits"]);
    }

    #[test]
    fn workflow_steps_to_plans_preserves_step_order_and_payload(
        steps in prop::collection::vec(arb_step(), 1..5),
        pane_id in 1u64..10_000,
    ) {
        let workflow = StaticWorkflow {
            steps: steps.clone(),
            requires_pane: true,
            requires_approval: false,
            can_abort: true,
            destructive: false,
        };

        let plans = workflow.steps_to_plans(pane_id);

        prop_assert_eq!(plans.len(), steps.len());

        for (idx, (step, plan)) in steps.iter().zip(plans.iter()).enumerate() {
            prop_assert_eq!(plan.step_number, (idx + 1) as u32);
            prop_assert_eq!(&plan.description, &step.description);

            match &plan.action {
                StepAction::Custom { action_type, payload } => {
                    prop_assert_eq!(action_type, &format!("workflow_step:{}", step.name));
                    prop_assert_eq!(&payload["workflow"], "static_workflow");
                    prop_assert_eq!(&payload["step_name"], &step.name);
                    prop_assert_eq!(&payload["description"], &step.description);
                    prop_assert_eq!(&payload["pane_id"], pane_id);
                }
                other => prop_assert!(false, "expected custom step action, got {:?}", other),
            }
        }
    }
}

#[test]
fn static_workflow_handle_predicate_is_codex_only() {
    let workflow = StaticWorkflow {
        steps: vec![WorkflowStep::new("step", "desc")],
        requires_pane: true,
        requires_approval: false,
        can_abort: true,
        destructive: false,
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
        rule_id: "codex.usage.reached".into(),
        agent_type: AgentType::ClaudeCode,
        event_type: "usage.limit".into(),
        severity: Severity::Warning,
        confidence: 0.9,
        extracted: serde_json::json!({}),
        matched_text: "usage reached".into(),
        span: (0, 13),
    };

    assert!(workflow.handles(&codex));
    assert!(!workflow.handles(&claude));
}
