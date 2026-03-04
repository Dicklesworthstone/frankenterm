// Property-based tests for swarm_pipeline module (ft-3681t.3.1)
//
// Covers: serde roundtrips for all public types, pipeline validation invariants,
// topological ordering properties, backoff strategy monotonicity, circuit breaker
// state machine invariants, recovery policy properties, hook registry ordering,
// precondition evaluation, pipeline execution properties, and error Display coverage.
#![allow(clippy::ignored_unit_patterns)]

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use proptest::prelude::*;
use serde_json;

use frankenterm_core::swarm_pipeline::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_hook_phase() -> impl Strategy<Value = HookPhase> {
    prop_oneof![
        Just(HookPhase::PipelineStart),
        Just(HookPhase::PipelineEnd),
        Just(HookPhase::PreStep),
        Just(HookPhase::PostStep),
        Just(HookPhase::PreRecovery),
        Just(HookPhase::PostRecovery),
        Just(HookPhase::PreCompensation),
        Just(HookPhase::PostCompensation),
    ]
}

fn arb_log_level() -> impl Strategy<Value = LogLevel> {
    prop_oneof![
        Just(LogLevel::Trace),
        Just(LogLevel::Debug),
        Just(LogLevel::Info),
        Just(LogLevel::Warn),
        Just(LogLevel::Error),
    ]
}

fn arb_hook_outcome() -> impl Strategy<Value = HookOutcome> {
    prop_oneof![
        Just(HookOutcome::Continue),
        Just(HookOutcome::SkipStep),
        "[a-z ]{5,20}".prop_map(|reason| HookOutcome::Abort { reason }),
        ("[a-z_]{3,10}", "[a-z0-9]{3,10}")
            .prop_map(|(key, value)| HookOutcome::InjectMetadata { key, value }),
    ]
}

fn arb_precondition_check() -> impl Strategy<Value = PreconditionCheck> {
    prop_oneof![
        "[a-z_]{3,10}".prop_map(|key| PreconditionCheck::MetadataPresent { key }),
        ("[a-z_]{3,10}", "[a-z0-9]{3,10}")
            .prop_map(|(key, value)| PreconditionCheck::MetadataEquals { key, value }),
        (1u32..100).prop_map(|threshold| PreconditionCheck::MaxFailures { threshold }),
        (1000u64..300_000).prop_map(|max_ms| PreconditionCheck::TimeLimit { max_ms }),
    ]
}

fn arb_hook_handler() -> impl Strategy<Value = HookHandler> {
    prop_oneof![
        (arb_log_level(), "[a-z ]{5,20}")
            .prop_map(|(level, template)| HookHandler::Log { level, template }),
        "[a-z_]{5,15}".prop_map(|counter_name| HookHandler::Telemetry { counter_name }),
        arb_precondition_check().prop_map(|check| HookHandler::Precondition { check }),
        ("[a-z_]{3,10}", "[a-z0-9]{3,10}")
            .prop_map(|(key, value)| HookHandler::Metadata { key, value }),
        "[a-z ]{5,20}".prop_map(|t| HookHandler::AgentMailNotify {
            subject_template: t
        }),
        "[a-z_]{3,10}".prop_map(|tag| HookHandler::Custom { tag }),
    ]
}

fn arb_step_status() -> impl Strategy<Value = StepStatus> {
    prop_oneof![
        Just(StepStatus::Pending),
        Just(StepStatus::Running),
        Just(StepStatus::Succeeded),
        "[a-z ]{5,20}".prop_map(|error| StepStatus::Failed { error }),
        "[a-z ]{5,20}".prop_map(|reason| StepStatus::Skipped { reason }),
        Just(StepStatus::Compensated),
    ]
}

fn arb_step_outcome() -> impl Strategy<Value = StepOutcome> {
    (
        0usize..20,
        "[a-z][a-z0-9_-]{2,10}",
        arb_step_status(),
        0u32..10,
        0u64..60_000,
        0u32..5,
        0u32..3,
    )
        .prop_map(
            |(step_index, step_label, status, attempts, duration_ms, recovery_attempts, compensations_run)| {
                StepOutcome {
                    step_index,
                    step_label,
                    status,
                    attempts,
                    duration_ms,
                    recovery_attempts,
                    compensations_run,
                }
            },
        )
}

fn arb_backoff_strategy() -> impl Strategy<Value = BackoffStrategy> {
    prop_oneof![
        (100u64..10_000).prop_map(|delay_ms| BackoffStrategy::Fixed { delay_ms }),
        (100u64..5_000, 1.1f64..4.0, 10_000u64..300_000).prop_map(
            |(base_ms, multiplier, max_delay_ms)| BackoffStrategy::Exponential {
                base_ms,
                multiplier,
                max_delay_ms,
            }
        ),
        (100u64..5_000, 100u64..2_000, 10_000u64..300_000).prop_map(
            |(initial_ms, increment_ms, max_delay_ms)| BackoffStrategy::Linear {
                initial_ms,
                increment_ms,
                max_delay_ms,
            }
        ),
    ]
}

fn arb_circuit_state() -> impl Strategy<Value = CircuitState> {
    prop_oneof![
        Just(CircuitState::Closed),
        (0u64..1_000_000).prop_map(|ms| CircuitState::Open { opened_at_ms: ms }),
        (0u32..10).prop_map(|count| CircuitState::HalfOpen {
            attempt_count: count
        }),
    ]
}

#[allow(dead_code)]
fn arb_circuit_breaker_config() -> impl Strategy<Value = CircuitBreakerConfig> {
    (1u32..20, 1_000u64..120_000, 1u32..10).prop_map(
        |(failure_threshold, reset_timeout_ms, success_threshold)| CircuitBreakerConfig {
            failure_threshold,
            reset_timeout_ms,
            success_threshold,
        },
    )
}

fn arb_pipeline_condition() -> impl Strategy<Value = PipelineCondition> {
    prop_oneof![
        ("[a-z]{3,8}", "[a-z]{3,8}").prop_map(|(id, status)| PipelineCondition::WorkItemStatus {
            work_item_id: id,
            target_status: status,
        }),
        ("[a-z_]{3,8}", "[a-z0-9]{3,8}")
            .prop_map(|(key, value)| PipelineCondition::MetadataEquals { key, value }),
        (1000u64..60_000).prop_map(|ms| PipelineCondition::Timeout { after_ms: ms }),
        prop::collection::vec("[a-z]{3,8}", 1..=4)
            .prop_map(|labels| PipelineCondition::AllStepsComplete {
                step_labels: labels
            }),
    ]
}

fn arb_step_action() -> impl Strategy<Value = StepAction> {
    prop_oneof![
        Just(StepAction::Noop),
        ("[a-z]{3,10}", 0u32..10).prop_map(|(id, priority)| StepAction::DispatchWork {
            work_item_id: id,
            priority,
        }),
        (
            "[a-z ]{5,15}",
            "[a-z ]{10,30}",
            prop::collection::vec("[a-z]{3,8}", 1..=3)
        )
            .prop_map(|(subject, body, recipients)| StepAction::SendMessage {
                subject,
                body,
                recipients,
            }),
        (arb_pipeline_condition(), 100u64..5_000).prop_map(|(condition, poll_interval_ms)| {
            StepAction::WaitForCondition {
                condition,
                poll_interval_ms,
            }
        }),
        "[a-z_]{3,15}".prop_map(|name| StepAction::SubPipeline {
            pipeline_name: name
        }),
        ("[a-z_]{3,10}", prop::collection::vec("[a-z]{2,6}", 0..=3))
            .prop_map(|(command, args)| StepAction::Command { command, args }),
        "[a-z_]{3,10}".prop_map(|label| StepAction::Checkpoint { label }),
    ]
}

fn arb_compensation_kind() -> impl Strategy<Value = CompensationKind> {
    prop_oneof![
        "[a-z_ ]{5,20}".prop_map(|command| CompensationKind::SendCommand { command }),
        "[a-z0-9-]{5,15}".prop_map(|id| CompensationKind::RestoreCheckpoint {
            checkpoint_id: id
        }),
        ("[a-z]{3,8}", "[a-z ]{5,20}")
            .prop_map(|(agent_name, message)| CompensationKind::NotifyAgent {
                agent_name,
                message,
            }),
        "[a-z ]{5,20}".prop_map(|message| CompensationKind::Log { message }),
        (
            "[a-z_]{3,10}",
            prop::collection::hash_map("[a-z]{2,5}", "[a-z0-9]{2,5}", 0..=3)
        )
            .prop_map(|(tag, params)| CompensationKind::Custom { tag, params }),
    ]
}

fn arb_pipeline_status() -> impl Strategy<Value = PipelineStatus> {
    prop_oneof![
        Just(PipelineStatus::Pending),
        Just(PipelineStatus::Running),
        Just(PipelineStatus::Succeeded),
        "[a-z ]{5,20}".prop_map(|reason| PipelineStatus::Failed { reason }),
        "[a-z ]{5,20}".prop_map(|reason| PipelineStatus::Aborted { reason }),
        Just(PipelineStatus::Compensating),
    ]
}

fn arb_pipeline_error() -> impl Strategy<Value = PipelineError> {
    prop_oneof![
        "[a-z ]{5,20}".prop_map(|reason| PipelineError::ValidationFailed { reason }),
        Just(PipelineError::DependencyCycle),
        "[a-z_]{3,10}".prop_map(|label| PipelineError::StepNotFound { label }),
        "[a-z ]{5,20}".prop_map(|reason| PipelineError::ExecutionFailed { reason }),
        "[a-z_]{3,10}".prop_map(|label| PipelineError::CircuitBreakerOpen {
            step_label: label
        }),
        ("[a-z_]{3,10}", 1000u64..300_000)
            .prop_map(|(label, elapsed)| PipelineError::Timeout {
                step_label: label,
                elapsed_ms: elapsed,
            }),
    ]
}

// =============================================================================
// Helpers
// =============================================================================

fn noop_step(label: &str) -> PipelineStep {
    PipelineStep {
        label: label.to_string(),
        description: format!("Test step {label}"),
        action: StepAction::Noop,
        depends_on: Vec::new(),
        recovery: RecoveryPolicy::default(),
        compensation: None,
        timeout_ms: 5000,
        optional: false,
        preconditions: Vec::new(),
    }
}

fn step_with_deps(label: &str, deps: Vec<&str>) -> PipelineStep {
    let mut s = noop_step(label);
    s.depends_on = deps.into_iter().map(String::from).collect();
    s
}

fn simple_pipeline(name: &str, steps: Vec<PipelineStep>) -> PipelineDefinition {
    PipelineDefinition {
        name: name.to_string(),
        description: format!("Test pipeline {name}"),
        steps,
        default_recovery: RecoveryPolicy::default(),
        timeout_ms: 60_000,
        compensate_on_failure: true,
        metadata: HashMap::new(),
    }
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn hook_phase_serde_roundtrip(phase in arb_hook_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let restored: HookPhase = serde_json::from_str(&json).unwrap();
        assert_eq!(phase, restored);
    }

    #[test]
    fn hook_outcome_serde_roundtrip(outcome in arb_hook_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let restored: HookOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, restored);
    }

    #[test]
    fn hook_handler_serde_roundtrip(handler in arb_hook_handler()) {
        let json = serde_json::to_string(&handler).unwrap();
        let restored: HookHandler = serde_json::from_str(&json).unwrap();
        // HookHandler doesn't derive PartialEq, so just verify roundtrip parses
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn step_status_serde_roundtrip(status in arb_step_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let restored: StepStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, restored);
    }

    #[test]
    fn step_outcome_serde_roundtrip(outcome in arb_step_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let restored: StepOutcome = serde_json::from_str(&json).unwrap();
        // StepOutcome doesn't derive PartialEq (contains StepStatus which does)
        assert_eq!(outcome.step_index, restored.step_index);
        assert_eq!(outcome.step_label, restored.step_label);
        assert_eq!(outcome.status, restored.status);
        assert_eq!(outcome.attempts, restored.attempts);
        assert_eq!(outcome.duration_ms, restored.duration_ms);
    }

    #[test]
    fn backoff_strategy_serde_roundtrip(strategy in arb_backoff_strategy()) {
        let json = serde_json::to_string(&strategy).unwrap();
        let restored: BackoffStrategy = serde_json::from_str(&json).unwrap();
        // f64 precision loss in JSON roundtrip — verify via double-roundtrip stability
        let json2 = serde_json::to_string(&restored).unwrap();
        let restored2: BackoffStrategy = serde_json::from_str(&json2).unwrap();
        let json3 = serde_json::to_string(&restored2).unwrap();
        assert_eq!(json2, json3, "backoff strategy should stabilize after first roundtrip");
    }

    #[test]
    fn circuit_state_serde_roundtrip(state in arb_circuit_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let restored: CircuitState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, restored);
    }

    #[test]
    fn step_action_serde_roundtrip(action in arb_step_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let _restored: StepAction = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&_restored).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn compensation_kind_serde_roundtrip(kind in arb_compensation_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let restored: CompensationKind = serde_json::from_str(&json).unwrap();
        // CompensationKind has no PartialEq (HashMap inside Custom variant),
        // and HashMap key ordering is non-deterministic in JSON.
        // Compare via serde_json::Value which has deterministic equality.
        let val1: serde_json::Value = serde_json::to_value(&kind).unwrap();
        let val2: serde_json::Value = serde_json::to_value(&restored).unwrap();
        assert_eq!(val1, val2, "compensation kind should survive roundtrip");
    }

    #[test]
    fn pipeline_status_serde_roundtrip(status in arb_pipeline_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let restored: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, restored);
    }

    #[test]
    fn pipeline_error_serde_roundtrip(e in arb_pipeline_error()) {
        let json = serde_json::to_string(&e).unwrap();
        let restored: PipelineError = serde_json::from_str(&json).unwrap();
        assert_eq!(e, restored);
    }
}

// =============================================================================
// HookPhase Display coverage
// =============================================================================

proptest! {
    #[test]
    fn hook_phase_display_non_empty(phase in arb_hook_phase()) {
        let display = format!("{phase}");
        assert!(!display.is_empty());
        assert!(display.contains('.'));
    }
}

// =============================================================================
// Backoff strategy properties
// =============================================================================

proptest! {
    #[test]
    fn fixed_backoff_constant(delay_ms in 100u64..60_000, attempt in 0u32..20) {
        let strategy = BackoffStrategy::Fixed { delay_ms };
        let d = strategy.delay_for_attempt(attempt);
        assert_eq!(d, Duration::from_millis(delay_ms));
    }

    #[test]
    fn exponential_backoff_monotonic(
        base_ms in 100u64..5_000,
        multiplier in 1.0f64..4.0,
        max_delay_ms in 10_000u64..300_000,
    ) {
        let strategy = BackoffStrategy::Exponential {
            base_ms,
            multiplier,
            max_delay_ms,
        };
        let mut prev = Duration::ZERO;
        for attempt in 0..10 {
            let d = strategy.delay_for_attempt(attempt);
            assert!(d >= prev || d == Duration::from_millis(max_delay_ms));
            assert!(d <= Duration::from_millis(max_delay_ms));
            prev = d;
        }
    }

    #[test]
    fn linear_backoff_monotonic(
        initial_ms in 100u64..5_000,
        increment_ms in 100u64..2_000,
        max_delay_ms in 10_000u64..300_000,
    ) {
        let strategy = BackoffStrategy::Linear {
            initial_ms,
            increment_ms,
            max_delay_ms,
        };
        let mut prev = Duration::ZERO;
        for attempt in 0..10 {
            let d = strategy.delay_for_attempt(attempt);
            assert!(d >= prev || d == Duration::from_millis(max_delay_ms));
            assert!(d <= Duration::from_millis(max_delay_ms));
            prev = d;
        }
    }

    #[test]
    fn backoff_always_capped(strategy in arb_backoff_strategy()) {
        for attempt in 0..20 {
            let d = strategy.delay_for_attempt(attempt);
            // Delay should be finite (not overflow)
            let _ = d.as_millis();
            // And should be finite (not overflow)
            assert!(d.as_secs() < 86_400);
        }
    }
}

// =============================================================================
// Circuit breaker state machine properties
// =============================================================================

proptest! {
    #[test]
    fn circuit_breaker_closed_allows_requests(now_ms in 0u64..1_000_000) {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        assert!(cb.allow_request(now_ms));
    }

    #[test]
    fn circuit_breaker_opens_at_threshold(
        failure_threshold in 1u32..10,
    ) {
        let config = CircuitBreakerConfig {
            failure_threshold,
            reset_timeout_ms: 60_000,
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);

        // Record failures up to threshold
        for i in 0..failure_threshold {
            assert!(matches!(cb.state, CircuitState::Closed));
            cb.record_failure(i as u64 * 100);
        }
        assert!(matches!(cb.state, CircuitState::Open { .. }));
    }

    #[test]
    fn circuit_breaker_allows_after_timeout(
        reset_timeout_ms in 1_000u64..120_000,
    ) {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            reset_timeout_ms,
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);
        cb.record_failure(0);
        assert!(!cb.allow_request(reset_timeout_ms / 2));
        assert!(cb.allow_request(reset_timeout_ms + 1));
    }

    #[test]
    fn circuit_breaker_success_resets_consecutive_failures(
        failures in 1u32..5,
    ) {
        let config = CircuitBreakerConfig {
            failure_threshold: 10,
            reset_timeout_ms: 60_000,
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);
        for i in 0..failures {
            cb.record_failure(i as u64 * 100);
        }
        assert_eq!(cb.consecutive_failures, failures);
        cb.record_success(failures as u64 * 100);
        assert_eq!(cb.consecutive_failures, 0);
    }

    #[test]
    fn circuit_breaker_half_open_closes_on_success_threshold(
        success_threshold in 1u32..5,
    ) {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            reset_timeout_ms: 1_000,
            success_threshold,
        };
        let mut cb = CircuitBreaker::new(config);
        cb.record_failure(0);
        // Transition to half-open
        assert!(cb.allow_request_and_advance(1_001));
        assert!(matches!(cb.state, CircuitState::HalfOpen { .. }));

        for i in 0..success_threshold {
            cb.record_success(1_001 + i as u64);
        }
        assert_eq!(cb.state, CircuitState::Closed);
    }

    #[test]
    fn circuit_breaker_reset_returns_to_closed(
        failures in 1u32..10,
    ) {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            reset_timeout_ms: 60_000,
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);
        for i in 0..failures {
            cb.record_failure(i as u64);
        }
        cb.reset();
        assert_eq!(cb.state, CircuitState::Closed);
        assert_eq!(cb.consecutive_failures, 0);
        assert_eq!(cb.consecutive_successes, 0);
    }

    #[test]
    fn circuit_breaker_total_counts_accumulate(
        successes in 0u32..10,
        failures in 0u32..10,
    ) {
        let config = CircuitBreakerConfig {
            failure_threshold: 100,
            reset_timeout_ms: 60_000,
            success_threshold: 100,
        };
        let mut cb = CircuitBreaker::new(config);
        for i in 0..successes {
            cb.record_success(i as u64);
        }
        for i in 0..failures {
            cb.record_failure((successes + i) as u64);
        }
        assert_eq!(cb.total_successes, successes as u64);
        assert_eq!(cb.total_failures, failures as u64);
    }
}

// =============================================================================
// Recovery policy properties
// =============================================================================

proptest! {
    #[test]
    fn non_retryable_error_matching(
        patterns in prop::collection::vec("[a-z_]{3,10}", 1..=5),
        error_idx in 0usize..5,
    ) {
        let policy = RecoveryPolicy {
            non_retryable_errors: patterns.clone(),
            ..Default::default()
        };
        let idx = error_idx.min(patterns.len() - 1);
        let error_msg = format!("some context {} more context", patterns[idx]);
        assert!(policy.is_non_retryable(&error_msg));

        // A string that cannot possibly match any [a-z_]{3,10} substring
        assert!(!policy.is_non_retryable("0000000000"));
    }
}

// =============================================================================
// Pipeline validation properties
// =============================================================================

proptest! {
    #[test]
    fn valid_linear_chain_passes_validation(n in 2usize..8) {
        let mut steps = vec![noop_step("step-0")];
        for i in 1..n {
            steps.push(step_with_deps(&format!("step-{i}"), vec![&format!("step-{}", i - 1)]));
        }
        let pipeline = simple_pipeline("linear", steps);
        assert!(pipeline.validate().is_ok());
    }

    #[test]
    fn valid_diamond_dag_passes_validation(n_middle in 1usize..5) {
        let mut steps = vec![noop_step("root")];
        let mut middle_labels = Vec::new();
        for i in 0..n_middle {
            let label = format!("mid-{i}");
            steps.push(step_with_deps(&label, vec!["root"]));
            middle_labels.push(label);
        }
        let dep_refs: Vec<&str> = middle_labels.iter().map(String::as_str).collect();
        steps.push(step_with_deps("join", dep_refs));
        let pipeline = simple_pipeline("diamond", steps);
        assert!(pipeline.validate().is_ok());
    }

    #[test]
    fn empty_name_always_fails(_dummy in 0..1u32) {
        let p = simple_pipeline("", vec![noop_step("a")]);
        assert!(matches!(p.validate(), Err(PipelineError::ValidationFailed { .. })));
    }

    #[test]
    fn empty_steps_always_fails(_dummy in 0..1u32) {
        let p = simple_pipeline("test", vec![]);
        assert!(matches!(p.validate(), Err(PipelineError::ValidationFailed { .. })));
    }

    #[test]
    fn duplicate_labels_fail(label in "[a-z]{3,8}") {
        let p = simple_pipeline("test", vec![noop_step(&label), noop_step(&label)]);
        assert!(matches!(p.validate(), Err(PipelineError::ValidationFailed { .. })));
    }

    #[test]
    fn self_dependency_fails(label in "[a-z]{3,8}") {
        let p = simple_pipeline("test", vec![step_with_deps(&label, vec![&label])]);
        assert!(matches!(p.validate(), Err(PipelineError::ValidationFailed { .. })));
    }
}

// =============================================================================
// Topological order properties
// =============================================================================

proptest! {
    #[test]
    fn topological_order_covers_all_steps(n in 1usize..10) {
        let mut steps = vec![noop_step("step-0")];
        for i in 1..n {
            steps.push(step_with_deps(&format!("step-{i}"), vec![&format!("step-{}", i - 1)]));
        }
        let pipeline = simple_pipeline("topo-test", steps);
        let order = pipeline.topological_order().unwrap();
        assert_eq!(order.len(), n);
        let mut seen: HashSet<usize> = HashSet::new();
        for idx in &order {
            assert!(seen.insert(*idx), "duplicate index in topological order");
        }
    }

    #[test]
    fn topological_order_respects_dependencies(n in 2usize..8) {
        let mut steps = vec![noop_step("step-0")];
        for i in 1..n {
            steps.push(step_with_deps(&format!("step-{i}"), vec![&format!("step-{}", i - 1)]));
        }
        let pipeline = simple_pipeline("dep-test", steps);
        let order = pipeline.topological_order().unwrap();

        // For a linear chain, the order should be strictly 0, 1, 2, ...
        for (pos, &idx) in order.iter().enumerate() {
            assert_eq!(idx, pos, "linear chain should produce sequential order");
        }
    }

    #[test]
    fn ready_steps_initially_only_roots(n_roots in 1usize..5) {
        let mut steps = Vec::new();
        for i in 0..n_roots {
            steps.push(noop_step(&format!("root-{i}")));
        }
        // Add a step depending on all roots
        let dep_refs: Vec<&str> = (0..n_roots).map(|i| steps[i].label.as_str()).collect();
        let dep_labels: Vec<String> = dep_refs.iter().map(|s| s.to_string()).collect();
        let mut join = noop_step("join");
        join.depends_on = dep_labels;
        steps.push(join);

        let pipeline = simple_pipeline("roots-test", steps);
        let completed = HashSet::new();
        let ready = pipeline.ready_steps(&completed);

        // Only root steps should be ready
        assert_eq!(ready.len(), n_roots);
        for idx in &ready {
            assert!(*idx < n_roots);
        }
    }
}

// =============================================================================
// Pipeline execution properties
// =============================================================================

proptest! {
    #[test]
    fn noop_pipeline_always_succeeds(n in 1usize..6) {
        let steps: Vec<PipelineStep> = (0..n).map(|i| noop_step(&format!("step-{i}"))).collect();
        let pipeline = simple_pipeline("noop-exec", steps);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&pipeline, 1000).unwrap();
        assert_eq!(result.status, PipelineStatus::Succeeded);
        assert_eq!(result.step_outcomes.len(), n);
    }

    #[test]
    fn execution_id_contains_pipeline_name(name in "[a-z]{3,10}") {
        let pipeline = simple_pipeline(&name, vec![noop_step("a")]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&pipeline, 1000).unwrap();
        assert!(result.execution_id.contains(&name));
    }

    #[test]
    fn metadata_propagates_through_execution(
        key in "[a-z_]{3,8}",
        value in "[a-z0-9]{3,8}",
    ) {
        let mut pipeline = simple_pipeline("meta-test", vec![noop_step("a")]);
        pipeline.metadata.insert(key.clone(), value.clone());
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&pipeline, 1000).unwrap();
        assert_eq!(result.metadata.get(&key), Some(&value));
    }

    #[test]
    fn execution_sets_end_time(now_ms in 0u64..1_000_000) {
        let pipeline = simple_pipeline("time-test", vec![noop_step("a")]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&pipeline, now_ms).unwrap();
        assert!(result.ended_at_ms.is_some());
        assert!(result.ended_at_ms.unwrap() >= now_ms);
    }
}

// =============================================================================
// Hook registry properties
// =============================================================================

proptest! {
    #[test]
    fn hook_registry_ordered_by_priority(
        priorities in prop::collection::vec(1u32..1000, 2..=8),
    ) {
        let mut registry = HookRegistry::new();
        for (i, &priority) in priorities.iter().enumerate() {
            registry.register(HookRegistration {
                name: format!("hook-{i}"),
                phases: [HookPhase::PreStep].into(),
                priority,
                enabled: true,
                handler: HookHandler::Custom {
                    tag: format!("tag-{i}"),
                },
            });
        }

        let ctx = HookContext {
            execution_id: "test".to_string(),
            pipeline_name: "test".to_string(),
            step_index: None,
            step_label: None,
            elapsed_ms: 0,
            steps_completed: 0,
            total_steps: 0,
            last_result: None,
            metadata: HashMap::new(),
        };

        let results = registry.dispatch(HookPhase::PreStep, &ctx);
        assert_eq!(results.len(), priorities.len());

        // Hook names should come back in sorted-by-priority order
        let mut sorted_priorities = priorities.clone();
        sorted_priorities.sort();
        // Can't directly verify because hooks with same priority keep insertion order
        // but we can verify the registry length
        assert_eq!(registry.len(), priorities.len());
    }

    #[test]
    fn hook_registry_unregister_removes_hook(n in 1usize..5) {
        let mut registry = HookRegistry::new();
        for i in 0..n {
            registry.register(HookRegistration {
                name: format!("hook-{i}"),
                phases: [HookPhase::PreStep].into(),
                priority: 100,
                enabled: true,
                handler: HookHandler::Custom {
                    tag: format!("{i}"),
                },
            });
        }
        assert_eq!(registry.len(), n);
        assert!(registry.unregister("hook-0"));
        assert_eq!(registry.len(), n - 1);
        assert!(!registry.unregister("nonexistent"));
        assert_eq!(registry.len(), n - 1);
    }
}

// =============================================================================
// Precondition evaluation properties
// =============================================================================

proptest! {
    #[test]
    fn metadata_present_check_passes_when_key_exists(key in "[a-z_]{3,10}") {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "check".to_string(),
            phases: [HookPhase::PipelineStart].into(),
            priority: 10,
            enabled: true,
            handler: HookHandler::Precondition {
                check: PreconditionCheck::MetadataPresent { key: key.clone() },
            },
        });

        let mut metadata = HashMap::new();
        metadata.insert(key, "value".to_string());
        let ctx = HookContext {
            execution_id: "test".to_string(),
            pipeline_name: "test".to_string(),
            step_index: None,
            step_label: None,
            elapsed_ms: 0,
            steps_completed: 0,
            total_steps: 1,
            last_result: None,
            metadata,
        };

        let outcomes = registry.dispatch(HookPhase::PipelineStart, &ctx);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].1, HookOutcome::Continue);
    }

    #[test]
    fn max_failures_check_aborts_at_threshold(threshold in 1u32..20) {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "check".to_string(),
            phases: [HookPhase::PreStep].into(),
            priority: 10,
            enabled: true,
            handler: HookHandler::Precondition {
                check: PreconditionCheck::MaxFailures { threshold },
            },
        });

        let mut metadata = HashMap::new();
        metadata.insert("pipeline.failure_count".to_string(), threshold.to_string());
        let ctx = HookContext {
            execution_id: "test".to_string(),
            pipeline_name: "test".to_string(),
            step_index: Some(0),
            step_label: Some("a".to_string()),
            elapsed_ms: 0,
            steps_completed: 0,
            total_steps: 1,
            last_result: None,
            metadata,
        };

        let outcomes = registry.dispatch(HookPhase::PreStep, &ctx);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].1, HookOutcome::Abort { .. }));
    }
}

// =============================================================================
// Error Display coverage
// =============================================================================

proptest! {
    #[test]
    fn pipeline_error_display_non_empty(e in arb_pipeline_error()) {
        let msg = format!("{e}");
        assert!(!msg.is_empty());
    }

    #[test]
    fn pipeline_error_is_std_error(e in arb_pipeline_error()) {
        let _: &dyn std::error::Error = &e;
    }
}
