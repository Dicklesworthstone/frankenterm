//! Property tests for ntm_importer module (ft-3681t.8.3).

use frankenterm_core::ntm_importer::*;
use proptest::prelude::*;
use std::collections::HashMap;

// =============================================================================
// Arbitrary strategies
// =============================================================================

fn arb_ntm_pane() -> impl Strategy<Value = NtmPane> {
    (
        proptest::option::of(any::<String>().prop_map(|s| s.chars().take(20).collect::<String>())),
        proptest::option::of("[a-z]{3,10}".prop_map(String::from)),
        prop::collection::vec("[a-z]{1,5}".prop_map(String::from), 0..3),
        proptest::option::of(0.1f64..0.9),
        any::<bool>(),
    )
        .prop_map(|(role, command, args, split_ratio, is_focus)| NtmPane {
            role,
            command,
            args,
            environment: HashMap::new(),
            cwd: None,
            split_direction: None,
            split_ratio,
            is_focus,
        })
}

fn arb_ntm_window() -> impl Strategy<Value = NtmWindow> {
    (
        "[a-z]{3,10}".prop_map(String::from),
        proptest::option::of(prop::sample::select(vec![
            "vertical".to_string(),
            "horizontal".to_string(),
            "grid".to_string(),
            "tiled".to_string(),
        ])),
        prop::collection::vec(arb_ntm_pane(), 0..5),
    )
        .prop_map(|(name, layout, panes)| NtmWindow {
            name,
            layout,
            panes,
            focus_index: None,
        })
}

fn arb_ntm_session() -> impl Strategy<Value = NtmSession> {
    (
        "[a-z]{3,15}".prop_map(String::from),
        proptest::option::of("/tmp/[a-z]{5}".prop_map(String::from)),
        prop::collection::vec(arb_ntm_window(), 0..4),
        proptest::option::of(prop::sample::select(vec![
            "swarm".to_string(),
            "interactive".to_string(),
            "headless".to_string(),
        ])),
        any::<bool>(),
    )
        .prop_map(|(name, workspace, windows, coordinator_mode, auto_start)| {
            NtmSession {
                name,
                workspace,
                windows,
                environment: HashMap::new(),
                coordinator_mode,
                auto_start,
                safety_overrides: HashMap::new(),
                extra: HashMap::new(),
            }
        })
}

fn arb_ntm_workflow_trigger() -> impl Strategy<Value = NtmWorkflowTrigger> {
    (
        prop::sample::select(vec![
            "pattern".to_string(),
            "event".to_string(),
            "manual".to_string(),
        ]),
        "[a-z.]{3,20}".prop_map(String::from),
    )
        .prop_map(|(kind, value)| NtmWorkflowTrigger {
            kind,
            value,
            pane_filter: None,
        })
}

fn arb_ntm_workflow_step() -> impl Strategy<Value = NtmWorkflowStep> {
    (
        "[a-z_]{3,15}".prop_map(String::from),
        prop::sample::select(vec![
            "send_text".to_string(),
            "wait_for".to_string(),
            "assert".to_string(),
            "sleep".to_string(),
        ]),
        proptest::option::of(1u64..120),
    )
        .prop_map(|(name, action, timeout_secs)| NtmWorkflowStep {
            name,
            action,
            params: HashMap::from([
                ("text".to_string(), serde_json::json!("test")),
                ("pattern".to_string(), serde_json::json!(".*")),
                ("condition".to_string(), serde_json::json!("true")),
                ("seconds".to_string(), serde_json::json!(1)),
            ]),
            conditions: Vec::new(),
            timeout_secs,
        })
}

fn arb_ntm_workflow() -> impl Strategy<Value = NtmWorkflow> {
    (
        "[a-z-]{3,20}".prop_map(String::from),
        proptest::option::of("[A-Za-z ]{5,30}".prop_map(String::from)),
        prop::collection::vec(arb_ntm_workflow_trigger(), 0..3),
        prop::collection::vec(arb_ntm_workflow_step(), 1..5),
        any::<bool>(),
        any::<bool>(),
        0u64..300,
    )
        .prop_map(
            |(name, description, triggers, steps, enabled, allow_parallel, timeout_secs)| {
                NtmWorkflow {
                    name,
                    description,
                    triggers,
                    steps,
                    enabled,
                    allow_parallel,
                    timeout_secs,
                    safety_class: Some("review".to_string()),
                }
            },
        )
}

fn arb_ntm_config() -> impl Strategy<Value = NtmConfig> {
    (
        proptest::option::of(prop::sample::select(vec![
            "debug".to_string(),
            "info".to_string(),
            "warn".to_string(),
        ])),
        proptest::option::of(100u64..5000),
        prop::collection::vec(
            prop::sample::select(vec![
                "default".to_string(),
                "security".to_string(),
                "devops".to_string(),
            ]),
            0..3,
        ),
        any::<bool>(),
        any::<bool>(),
        0u32..200,
    )
        .prop_map(
            |(log_level, poll_interval_ms, pattern_packs, gate, approval, rate)| NtmConfig {
                log_level,
                workspace: None,
                poll_interval_ms,
                pattern_packs,
                safety: NtmSafetyConfig {
                    command_safety_gate: gate,
                    require_approval_destructive: approval,
                    rate_limit_per_minute: rate,
                    allowlist: Vec::new(),
                    denylist: Vec::new(),
                },
                robot: NtmRobotConfig::default(),
                hooks: Vec::new(),
                extra: HashMap::new(),
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn ntm_session_serde_roundtrip(session in arb_ntm_session()) {
        let json = serde_json::to_string(&session).unwrap();
        let back: NtmSession = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(session.name, back.name);
        prop_assert_eq!(session.workspace, back.workspace);
        prop_assert_eq!(session.windows.len(), back.windows.len());
        prop_assert_eq!(session.coordinator_mode, back.coordinator_mode);
        prop_assert_eq!(session.auto_start, back.auto_start);
    }

    #[test]
    fn ntm_workflow_serde_roundtrip(workflow in arb_ntm_workflow()) {
        let json = serde_json::to_string(&workflow).unwrap();
        let back: NtmWorkflow = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(workflow.name, back.name);
        prop_assert_eq!(workflow.steps.len(), back.steps.len());
        prop_assert_eq!(workflow.enabled, back.enabled);
        prop_assert_eq!(workflow.timeout_secs, back.timeout_secs);
    }

    #[test]
    fn ntm_config_serde_roundtrip(config in arb_ntm_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: NtmConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.log_level, back.log_level);
        prop_assert_eq!(config.poll_interval_ms, back.poll_interval_ms);
        prop_assert_eq!(config.safety.rate_limit_per_minute, back.safety.rate_limit_per_minute);
    }

    #[test]
    fn import_finding_serde_roundtrip(
        severity in prop::sample::select(vec![ImportSeverity::Info, ImportSeverity::Warning, ImportSeverity::Error]),
        code in "[A-Z_]{5,20}".prop_map(String::from),
        message in "[a-z ]{10,40}".prop_map(String::from),
    ) {
        let finding = ImportFinding {
            severity,
            source_path: "test:source".to_string(),
            code: code.clone(),
            message: message.clone(),
            remediation: Some("fix it".to_string()),
        };
        let json = serde_json::to_string(&finding).unwrap();
        let back: ImportFinding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(finding.severity, back.severity);
        prop_assert_eq!(finding.code, back.code);
        prop_assert_eq!(finding.message, back.message);
    }

    #[test]
    fn translated_workflow_serde_roundtrip(
        name in "[a-z-]{3,15}".prop_map(String::from),
        enabled in any::<bool>(),
        max_concurrent in 0u32..10,
        timeout_ms in 0u64..120_000,
    ) {
        let tw = TranslatedWorkflow {
            name: name.clone(),
            description: "test".to_string(),
            trigger_rule_ids: vec!["trigger.test".to_string()],
            steps: vec![TranslatedWorkflowStep {
                name: "step1".to_string(),
                description: "test step".to_string(),
                step_type: TranslatedStepType::SendText {
                    text: "hello".to_string(),
                    pane_filter: None,
                },
            }],
            enabled,
            safety_class: "safe".to_string(),
            max_concurrent,
            timeout_ms,
        };
        let json = serde_json::to_string(&tw).unwrap();
        let back: TranslatedWorkflow = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(name, back.name);
        prop_assert_eq!(enabled, back.enabled);
        prop_assert_eq!(max_concurrent, back.max_concurrent);
        prop_assert_eq!(timeout_ms, back.timeout_ms);
    }
}

// =============================================================================
// Behavioral property tests
// =============================================================================

proptest! {
    #[test]
    fn session_import_produces_at_least_one_profile(session in arb_ntm_session()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[session], &[], None);
        prop_assert!(bundle.session_profiles.len() >= 1);
        prop_assert_eq!(bundle.report.total_items, 1);
    }

    #[test]
    fn session_import_layout_count_matches_window_count(session in arb_ntm_session()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[session.clone()], &[], None);
        prop_assert_eq!(bundle.layout_templates.len(), session.windows.len());
    }

    #[test]
    fn workflow_import_preserves_step_count(workflow in arb_ntm_workflow()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[], &[workflow.clone()], None);
        prop_assert_eq!(bundle.workflows.len(), 1);
        prop_assert_eq!(bundle.workflows[0].steps.len(), workflow.steps.len());
    }

    #[test]
    fn workflow_timeout_converts_to_ms(workflow in arb_ntm_workflow()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[], &[workflow.clone()], None);
        let tw = &bundle.workflows[0];
        prop_assert_eq!(tw.timeout_ms, workflow.timeout_secs * 1000);
    }

    #[test]
    fn config_import_preserves_safety_settings(config in arb_ntm_config()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[], &[], Some(&config));
        let tc = bundle.config.as_ref().unwrap();
        prop_assert_eq!(tc.safety.command_safety_gate, config.safety.command_safety_gate);
        prop_assert_eq!(tc.safety.require_approval_destructive, config.safety.require_approval_destructive);
        prop_assert_eq!(tc.safety.rate_limit_per_minute, config.safety.rate_limit_per_minute);
    }

    #[test]
    fn report_counts_are_consistent(
        session in arb_ntm_session(),
        workflow in arb_ntm_workflow(),
        config in arb_ntm_config(),
    ) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[session], &[workflow], Some(&config));
        let report = &bundle.report;
        prop_assert_eq!(
            report.total_items,
            report.success_count + report.failure_count
        );
        prop_assert_eq!(report.total_items, report.items.len());
    }

    #[test]
    fn all_profiles_have_imported_tag(session in arb_ntm_session()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[session], &[], None);
        for profile in &bundle.session_profiles {
            let has_import_tag = profile.tags.iter().any(|t| t.starts_with("imported:"));
            prop_assert!(has_import_tag, "Profile {} missing imported: tag", profile.name);
        }
    }

    #[test]
    fn layout_template_min_panes_matches_actual(session in arb_ntm_session()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[session.clone()], &[], None);
        for (i, template) in bundle.layout_templates.iter().enumerate() {
            if i < session.windows.len() {
                let expected = session.windows[i].panes.len().max(1) as u32;
                prop_assert_eq!(
                    template.min_panes, expected,
                    "Template {} min_panes mismatch", template.name
                );
            }
        }
    }
}

// =============================================================================
// Role translation property tests
// =============================================================================

proptest! {
    #[test]
    fn role_translation_never_panics(role in "[a-zA-Z_-]{1,30}".prop_map(String::from)) {
        // The function should handle any input without panicking.
        let _ = NtmImporter::new();
        // translate_ntm_role is not pub, so we test via import
        let session = NtmSession {
            name: "test".to_string(),
            workspace: None,
            windows: vec![NtmWindow {
                name: "w".to_string(),
                layout: None,
                panes: vec![NtmPane {
                    role: Some(role),
                    command: None,
                    args: Vec::new(),
                    environment: HashMap::new(),
                    cwd: None,
                    split_direction: None,
                    split_ratio: None,
                    is_focus: false,
                }],
                focus_index: None,
            }],
            environment: HashMap::new(),
            coordinator_mode: None,
            auto_start: false,
            safety_overrides: HashMap::new(),
            extra: HashMap::new(),
        };
        let importer = NtmImporter::new();
        let bundle = importer.import(&[session], &[], None);
        prop_assert!(!bundle.session_profiles.is_empty());
    }

    #[test]
    fn unsupported_coordinator_mode_always_errors(
        mode in prop::sample::select(vec!["cluster".to_string(), "distributed".to_string()]),
    ) {
        let session = NtmSession {
            name: "test".to_string(),
            workspace: None,
            windows: Vec::new(),
            environment: HashMap::new(),
            coordinator_mode: Some(mode),
            auto_start: false,
            safety_overrides: HashMap::new(),
            extra: HashMap::new(),
        };
        let importer = NtmImporter::new();
        let bundle = importer.import(&[session], &[], None);
        let has_error = bundle.report.items.iter().any(|item| {
            item.findings.iter().any(|f| f.code == "UNSUPPORTED_COORDINATOR_MODE")
        });
        prop_assert!(has_error);
    }
}

// =============================================================================
// Import bundle completeness tests
// =============================================================================

proptest! {
    #[test]
    fn full_import_bundle_serde_roundtrip(
        session in arb_ntm_session(),
        workflow in arb_ntm_workflow(),
        config in arb_ntm_config(),
    ) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[session], &[workflow], Some(&config));
        let json = serde_json::to_string(&bundle).unwrap();
        let back: NtmImportBundle = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(bundle.session_profiles.len(), back.session_profiles.len());
        prop_assert_eq!(bundle.workflows.len(), back.workflows.len());
        prop_assert_eq!(bundle.report.total_items, back.report.total_items);
    }

    #[test]
    fn empty_import_produces_empty_bundle(dummy in 0u8..1) {
        let _ = dummy;
        let importer = NtmImporter::new();
        let bundle = importer.import(&[], &[], None);
        prop_assert!(bundle.session_profiles.is_empty());
        prop_assert!(bundle.layout_templates.is_empty());
        prop_assert!(bundle.workflows.is_empty());
        prop_assert!(bundle.config.is_none());
        prop_assert_eq!(bundle.report.total_items, 0);
    }
}
