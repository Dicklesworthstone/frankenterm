//! Property-based tests for ARS reflex compiler.
//!
//! Verifies compilation invariants: step structure, timeout clamping,
//! safety regex enforcement, and serde roundtrips.

use proptest::prelude::*;

use std::collections::HashMap;

use frankenterm_core::ars_compile::{
    ArsCompiler, CompileConfig, CompileError, CompileInput, CompileOutput,
};
use frankenterm_core::ars_evidence::{EvidenceCategory, EvidenceVerdict, LedgerDigest};
use frankenterm_core::ars_generalize::{GeneralizedCommand, ParamKind, TemplateVar};
use frankenterm_core::ars_timeout::{TimeoutDecision, TimeoutMethod};
use frankenterm_core::workflows::DescriptorStep;

// =============================================================================
// Strategies
// =============================================================================

fn arb_digest(verdict: EvidenceVerdict) -> LedgerDigest {
    LedgerDigest {
        entry_count: 3,
        overall_verdict: verdict,
        categories_present: vec![
            EvidenceCategory::ChangeDetection,
            EvidenceCategory::SafetyProof,
        ],
        is_complete: true,
        timestamp_range: (1000, 3000),
        root_hash: "a".repeat(64),
    }
}

fn arb_timeout(ms: u64) -> TimeoutDecision {
    TimeoutDecision {
        timeout_ms: ms,
        raw_optimal_ms: ms as f64,
        expected_loss: 0.5,
        method: TimeoutMethod::ExpectedLoss,
        stats: None,
        is_data_driven: true,
    }
}

fn arb_command(text: &str, idx: u32) -> GeneralizedCommand {
    GeneralizedCommand {
        original: text.to_string(),
        template: text.to_string(),
        variables: Vec::new(),
        block_index: idx,
    }
}

fn arb_command_text() -> impl Strategy<Value = String> {
    "[a-z]{3,10}( [a-z0-9_./-]{1,10}){0,3}"
}

fn arb_input_params() -> impl Strategy<Value = (Vec<String>, u64, EvidenceVerdict)> {
    (
        prop::collection::vec(arb_command_text(), 1..5),
        500..60000u64,
        prop_oneof![
            Just(EvidenceVerdict::Support),
            Just(EvidenceVerdict::Neutral),
        ],
    )
}

fn build_input(texts: &[String], timeout_ms: u64, verdict: EvidenceVerdict) -> CompileInput {
    let commands: Vec<GeneralizedCommand> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| arb_command(t, i as u32))
        .collect();
    CompileInput {
        cluster_id: "test-prop".to_string(),
        reflex_name: "proptest reflex".to_string(),
        description: Some("generated".to_string()),
        commands,
        timeout: arb_timeout(timeout_ms),
        evidence_digest: arb_digest(verdict),
        trigger: None,
        bindings: HashMap::new(),
        success_pattern: None,
    }
}

// =============================================================================
// Step structure invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn compiled_step_count_matches_formula(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        // default config: snapshot(1) + commands(n) + waitfor(1) + evidence(1) = n+3
        let expected = texts.len() + 3;
        prop_assert_eq!(output.step_count, expected);
    }

    #[test]
    fn first_step_is_always_log(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        let is_log = matches!(&output.descriptor.steps[0], DescriptorStep::Log { .. });
        prop_assert!(is_log, "first step should be a Log");
    }

    #[test]
    fn last_step_is_always_evidence_log(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        let last = output.descriptor.steps.last().unwrap();
        let is_evidence = matches!(last, DescriptorStep::Log { id, .. } if id.starts_with("ars_evidence"));
        prop_assert!(is_evidence, "last step should be evidence log");
    }

    #[test]
    fn send_text_count_matches_command_count(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        let send_count = output
            .descriptor
            .steps
            .iter()
            .filter(|s| matches!(s, DescriptorStep::SendText { .. }))
            .count();
        prop_assert_eq!(send_count, texts.len());
    }

    #[test]
    fn exactly_one_wait_for_step(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        let wait_count = output
            .descriptor
            .steps
            .iter()
            .filter(|s| matches!(s, DescriptorStep::WaitFor { .. }))
            .count();
        prop_assert_eq!(wait_count, 1);
    }

    #[test]
    fn all_step_ids_unique(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        let ids: Vec<String> = output
            .descriptor
            .steps
            .iter()
            .map(|s| match s {
                DescriptorStep::Log { id, .. }
                | DescriptorStep::SendText { id, .. }
                | DescriptorStep::WaitFor { id, .. }
                | DescriptorStep::Sleep { id, .. }
                | DescriptorStep::SendCtrl { id, .. }
                | DescriptorStep::Notify { id, .. }
                | DescriptorStep::Abort { id, .. }
                | DescriptorStep::Conditional { id, .. }
                | DescriptorStep::Loop { id, .. } => id.clone(),
            })
            .collect();
        let mut deduped = ids.clone();
        deduped.sort();
        deduped.dedup();
        prop_assert_eq!(ids.len(), deduped.len());
    }
}

// =============================================================================
// Timeout clamping invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn timeout_always_within_bounds(timeout_ms in 0..200_000u64) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&["echo ok".to_string()], timeout_ms, EvidenceVerdict::Support);
        let output = compiler.compile(&input).unwrap();
        let config = compiler.config();
        prop_assert!(output.effective_timeout_ms >= config.min_wait_timeout_ms);
        prop_assert!(output.effective_timeout_ms <= config.max_wait_timeout_ms);
    }

    #[test]
    fn timeout_in_range_preserved(timeout_ms in 500..120_000u64) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&["echo ok".to_string()], timeout_ms, EvidenceVerdict::Support);
        let output = compiler.compile(&input).unwrap();
        prop_assert_eq!(output.effective_timeout_ms, timeout_ms);
    }
}

// =============================================================================
// Evidence gate invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn reject_verdict_always_fails(
        texts in prop::collection::vec(arb_command_text(), 1..5),
        timeout_ms in 500..60000u64,
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, EvidenceVerdict::Reject);
        let result = compiler.compile(&input);
        let is_rejected = matches!(result, Err(CompileError::EvidenceRejected { .. }));
        prop_assert!(is_rejected, "Reject verdict should always fail compilation");
    }

    #[test]
    fn support_verdict_always_succeeds(
        texts in prop::collection::vec(arb_command_text(), 1..5),
        timeout_ms in 500..60000u64,
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, EvidenceVerdict::Support);
        let result = compiler.compile(&input);
        prop_assert!(result.is_ok(), "Support verdict should succeed");
    }

    #[test]
    fn neutral_verdict_always_succeeds(
        texts in prop::collection::vec(arb_command_text(), 1..5),
        timeout_ms in 500..60000u64,
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, EvidenceVerdict::Neutral);
        let result = compiler.compile(&input);
        prop_assert!(result.is_ok(), "Neutral verdict should succeed");
    }
}

// =============================================================================
// Safety regex enforcement
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn safe_bindings_accepted(
        filename in "[a-zA-Z0-9_./]{1,20}",
    ) {
        let cmd = GeneralizedCommand {
            original: format!("cargo build {filename}"),
            template: "cargo build {{cap.file_0}}".to_string(),
            variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: filename.clone(),
                kind: ParamKind::FilePath,
                safety_regex: "^[a-zA-Z0-9_./-]+$".to_string(),
            }],
            block_index: 0,
        };
        let mut input = build_input(&["dummy".to_string()], 5000, EvidenceVerdict::Support);
        input.commands = vec![cmd];
        input.bindings.insert("file_0".to_string(), filename);

        let compiler = ArsCompiler::with_defaults();
        let result = compiler.compile(&input);
        prop_assert!(result.is_ok());
    }

    #[test]
    fn unsafe_bindings_rejected(
        safe_part in "[a-zA-Z]{2,8}",
        inject in prop_oneof![
            Just(";".to_string()),
            Just("`".to_string()),
            Just("$(".to_string()),
            Just("|".to_string()),
        ],
    ) {
        let value = format!("{safe_part}{inject}");
        let cmd = GeneralizedCommand {
            original: format!("echo {value}"),
            template: "echo {{cap.file_0}}".to_string(),
            variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: "original".to_string(),
                kind: ParamKind::FilePath,
                safety_regex: "^[a-zA-Z0-9_./-]+$".to_string(),
            }],
            block_index: 0,
        };
        let mut input = build_input(&["dummy".to_string()], 5000, EvidenceVerdict::Support);
        input.commands = vec![cmd];
        input.bindings.insert("file_0".to_string(), value);

        let compiler = ArsCompiler::with_defaults();
        let result = compiler.compile(&input);
        let is_unsafe = matches!(result, Err(CompileError::UnsafeParameter { .. }));
        prop_assert!(is_unsafe, "injection chars should be rejected");
    }
}

// =============================================================================
// Serde roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_roundtrip(
        max_steps in 5..50usize,
        inject_snapshot in prop::bool::ANY,
        inject_wait in prop::bool::ANY,
    ) {
        let config = CompileConfig {
            max_steps,
            inject_snapshot,
            inject_wait_for: inject_wait,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: CompileConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.max_steps, config.max_steps);
        prop_assert_eq!(decoded.inject_snapshot, config.inject_snapshot);
        prop_assert_eq!(decoded.inject_wait_for, config.inject_wait_for);
    }

    #[test]
    fn compile_input_serde_roundtrip(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let input = build_input(&texts, timeout_ms, verdict);
        let json = serde_json::to_string(&input).unwrap();
        let decoded: CompileInput = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.cluster_id, input.cluster_id);
        prop_assert_eq!(decoded.commands.len(), input.commands.len());
        prop_assert_eq!(decoded.timeout.timeout_ms, input.timeout.timeout_ms);
    }

    #[test]
    fn compile_output_serde_roundtrip(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        let json = serde_json::to_string(&output).unwrap();
        let decoded: CompileOutput = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.step_count, output.step_count);
        prop_assert_eq!(decoded.effective_timeout_ms, output.effective_timeout_ms);
        prop_assert_eq!(decoded.is_instantiated, output.is_instantiated);
    }

    #[test]
    fn compile_error_serde_roundtrip(
        cluster in "[a-z]{3,10}",
    ) {
        let err = CompileError::OperatorLocked { cluster_id: cluster.clone() };
        let json = serde_json::to_string(&err).unwrap();
        let decoded: CompileError = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, err);
    }
}

// =============================================================================
// Descriptor name and schema
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn descriptor_name_contains_cluster_id(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        prop_assert!(output.descriptor.name.contains("test-prop"));
    }

    #[test]
    fn descriptor_schema_version_is_one(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        prop_assert_eq!(output.descriptor.workflow_schema_version, 1);
    }

    #[test]
    fn descriptor_has_failure_handler(
        (texts, timeout_ms, verdict) in arb_input_params(),
    ) {
        let compiler = ArsCompiler::with_defaults();
        let input = build_input(&texts, timeout_ms, verdict);
        let output = compiler.compile(&input).unwrap();
        prop_assert!(output.descriptor.on_failure.is_some());
    }
}

// =============================================================================
// Operator lock invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn operator_locked_always_rejects(
        cluster in "[a-z]{3,10}",
    ) {
        let config = CompileConfig {
            operator_locked: vec![cluster.clone()],
            ..Default::default()
        };
        let compiler = ArsCompiler::new(config);
        let mut input = build_input(&["echo ok".to_string()], 5000, EvidenceVerdict::Support);
        input.cluster_id = cluster;
        let result = compiler.compile(&input);
        let is_locked = matches!(result, Err(CompileError::OperatorLocked { .. }));
        prop_assert!(is_locked);
    }
}
