use std::collections::HashMap;

use frankenterm_core::swarm_pipeline::{
    BackoffStrategy, CompensatingAction, CompensationKind, HookHandler, HookPhase,
    HookRegistration, HookRegistry, PipelineDefinition, PipelineExecutor, PipelineStatus,
    PipelineStep, RecoveryPolicy, StepAction, StepStatus,
};

fn emit_structured_log(
    timestamp: &str,
    scenario_id: &str,
    correlation_id: &str,
    inputs: &str,
    decision: &str,
    outcome: &str,
    reason_code: &str,
    error_code: &str,
) {
    let payload = serde_json::json!({
        "timestamp": timestamp,
        "component": "swarm_pipeline.integration",
        "scenario_id": scenario_id,
        "correlation_id": correlation_id,
        "inputs": inputs,
        "decision": decision,
        "outcome": outcome,
        "reason_code": reason_code,
        "error_code": error_code
    });
    eprintln!("{payload}");
}

fn noop_step(label: &str) -> PipelineStep {
    PipelineStep {
        label: label.to_string(),
        description: format!("Step {label}"),
        action: StepAction::Noop,
        depends_on: Vec::new(),
        recovery: RecoveryPolicy::default(),
        compensation: None,
        timeout_ms: 5_000,
        optional: false,
        preconditions: Vec::new(),
    }
}

#[test]
fn swarm_pipeline_integration_failure_injection_triggers_recovery_and_compensation() {
    let scenario_id = "ft-3681t.3.5.integration.failure_recovery";
    let correlation_id = "ft-3681t.3.5-int-001";
    emit_structured_log(
        "2026-03-03T00:00:10Z",
        scenario_id,
        correlation_id,
        "pipeline=failure-injection-recovery",
        "start",
        "running",
        "none",
        "none",
    );

    let mut hooks = HookRegistry::new();
    hooks.register(HookRegistration {
        name: "integration-recovery-pre".to_string(),
        phases: [HookPhase::PreRecovery].into(),
        priority: 10,
        enabled: true,
        handler: HookHandler::Metadata {
            key: "integration.recovery.pre".to_string(),
            value: "seen".to_string(),
        },
    });
    hooks.register(HookRegistration {
        name: "integration-recovery-post".to_string(),
        phases: [HookPhase::PostRecovery].into(),
        priority: 20,
        enabled: true,
        handler: HookHandler::Metadata {
            key: "integration.recovery.post".to_string(),
            value: "seen".to_string(),
        },
    });
    hooks.register(HookRegistration {
        name: "integration-comp-pre".to_string(),
        phases: [HookPhase::PreCompensation].into(),
        priority: 30,
        enabled: true,
        handler: HookHandler::Metadata {
            key: "integration.compensation.pre".to_string(),
            value: "seen".to_string(),
        },
    });
    hooks.register(HookRegistration {
        name: "integration-comp-post".to_string(),
        phases: [HookPhase::PostCompensation].into(),
        priority: 40,
        enabled: true,
        handler: HookHandler::Metadata {
            key: "integration.compensation.post".to_string(),
            value: "seen".to_string(),
        },
    });

    let mut prepare = noop_step("prepare");
    prepare.compensation = Some(CompensatingAction {
        label: "undo-prepare".to_string(),
        compensates_step: "prepare".to_string(),
        action: CompensationKind::Log {
            message: "rollback prepare".to_string(),
        },
        timeout_ms: 5_000,
        required: true,
    });

    let mut inject_failure = noop_step("inject-failure");
    inject_failure.depends_on = vec!["prepare".to_string()];
    inject_failure.action = StepAction::Command {
        command: String::new(),
        args: Vec::new(),
    };
    inject_failure.recovery = RecoveryPolicy {
        max_retries: 1,
        backoff: BackoffStrategy::Fixed { delay_ms: 25 },
        ..Default::default()
    };

    let pipeline = PipelineDefinition {
        name: "failure-injection-recovery".to_string(),
        description: "Integration scenario for recovery + compensation".to_string(),
        steps: vec![prepare, inject_failure],
        default_recovery: RecoveryPolicy::default(),
        timeout_ms: 60_000,
        compensate_on_failure: true,
        metadata: HashMap::new(),
    };

    let mut executor = PipelineExecutor::with_hooks(hooks);
    let execution = executor
        .execute(&pipeline, 10_000)
        .expect("pipeline execution should complete");

    assert!(matches!(execution.status, PipelineStatus::Failed { .. }));
    assert_eq!(
        execution.metadata.get("integration.recovery.pre"),
        Some(&"seen".to_string())
    );
    assert_eq!(
        execution.metadata.get("integration.recovery.post"),
        Some(&"seen".to_string())
    );
    assert_eq!(
        execution.metadata.get("integration.compensation.pre"),
        Some(&"seen".to_string())
    );
    assert_eq!(
        execution.metadata.get("integration.compensation.post"),
        Some(&"seen".to_string())
    );
    assert!(
        execution
            .compensations_executed
            .contains(&"undo-prepare".to_string())
    );

    let prepare_outcome = execution
        .step_outcomes
        .get(&0)
        .expect("prepare step outcome should be present");
    assert_eq!(prepare_outcome.status, StepStatus::Compensated);

    let failure_outcome = execution
        .step_outcomes
        .get(&1)
        .expect("failure step outcome should be present");
    assert_eq!(failure_outcome.attempts, 2);
    assert_eq!(failure_outcome.recovery_attempts, 1);
    assert!(matches!(failure_outcome.status, StepStatus::Failed { .. }));

    emit_structured_log(
        "2026-03-03T00:00:11Z",
        scenario_id,
        correlation_id,
        "pipeline=failure-injection-recovery",
        "validate_recovery_and_compensation",
        "passed",
        "all_assertions_passed",
        "none",
    );
}

#[test]
fn swarm_pipeline_integration_optional_failure_blocks_required_dependent_step() {
    let scenario_id = "ft-3681t.3.5.integration.degraded_dependency_gate";
    let correlation_id = "ft-3681t.3.5-int-002";
    emit_structured_log(
        "2026-03-03T00:00:20Z",
        scenario_id,
        correlation_id,
        "pipeline=dependency-gate",
        "start",
        "running",
        "none",
        "none",
    );

    let mut upstream_optional = noop_step("upstream-optional");
    upstream_optional.optional = true;
    upstream_optional.action = StepAction::DispatchWork {
        work_item_id: String::new(),
        priority: 1,
    };
    upstream_optional.recovery.max_retries = 0;

    let mut required_downstream = noop_step("required-downstream");
    required_downstream.depends_on = vec!["upstream-optional".to_string()];

    let pipeline = PipelineDefinition {
        name: "dependency-gate".to_string(),
        description: "Dependency gate degraded-path integration scenario".to_string(),
        steps: vec![upstream_optional, required_downstream],
        default_recovery: RecoveryPolicy::default(),
        timeout_ms: 60_000,
        compensate_on_failure: false,
        metadata: HashMap::new(),
    };

    let mut executor = PipelineExecutor::new();
    let execution = executor
        .execute(&pipeline, 20_000)
        .expect("pipeline execution should complete");

    assert!(matches!(execution.status, PipelineStatus::Failed { .. }));
    let downstream = execution
        .step_outcomes
        .get(&1)
        .expect("dependent step outcome should be present");
    match &downstream.status {
        StepStatus::Failed { error } => {
            assert!(
                error.contains("unmet dependencies"),
                "expected unmet dependencies error, got: {error}"
            );
        }
        status => panic!("expected failed dependent step, got {status:?}"),
    }

    emit_structured_log(
        "2026-03-03T00:00:21Z",
        scenario_id,
        correlation_id,
        "pipeline=dependency-gate",
        "validate_dependency_gate",
        "passed",
        "all_assertions_passed",
        "none",
    );
}
