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
        .prop_map(
            |(name, workspace, windows, coordinator_mode, auto_start)| NtmSession {
                name,
                workspace,
                windows,
                environment: HashMap::new(),
                coordinator_mode,
                auto_start,
                safety_overrides: HashMap::new(),
                extra: HashMap::new(),
            },
        )
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
        prop_assert!(!bundle.session_profiles.is_empty());
        prop_assert_eq!(bundle.report.total_items, 1);
    }

    #[test]
    fn session_import_layout_count_matches_window_count(session in arb_ntm_session()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(std::slice::from_ref(&session), &[], None);
        prop_assert_eq!(bundle.layout_templates.len(), session.windows.len());
    }

    #[test]
    fn workflow_import_preserves_step_count(workflow in arb_ntm_workflow()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[], std::slice::from_ref(&workflow), None);
        prop_assert_eq!(bundle.workflows.len(), 1);
        prop_assert_eq!(bundle.workflows[0].steps.len(), workflow.steps.len());
    }

    #[test]
    fn workflow_timeout_converts_to_ms(workflow in arb_ntm_workflow()) {
        let importer = NtmImporter::new();
        let bundle = importer.import(&[], std::slice::from_ref(&workflow), None);
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
        let bundle = importer.import(std::slice::from_ref(&session), &[], None);
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

// =============================================================================
// Additional serde roundtrip tests for uncovered types
// =============================================================================

use std::collections::BTreeMap;

fn arb_ni_str() -> impl Strategy<Value = String> {
    "[a-z]{3,12}".prop_map(String::from)
}

fn arb_import_severity() -> impl Strategy<Value = ImportSeverity> {
    prop_oneof![
        Just(ImportSeverity::Info),
        Just(ImportSeverity::Warning),
        Just(ImportSeverity::Error),
    ]
}

fn arb_translated_step_type() -> impl Strategy<Value = TranslatedStepType> {
    prop_oneof![
        arb_ni_str().prop_map(|t| TranslatedStepType::SendText { text: t, pane_filter: None }),
        arb_ni_str().prop_map(|p| TranslatedStepType::WaitFor { pattern: p, timeout_ms: Some(5000) }),
        arb_ni_str().prop_map(|c| TranslatedStepType::Assert { condition: c }),
        (100u64..10_000).prop_map(|d| TranslatedStepType::Sleep { duration_ms: d }),
        arb_ni_str().prop_map(|a| TranslatedStepType::Unsupported {
            original_action: a, params: HashMap::new(),
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn ni_s01_ntm_window_serde(name in arb_ni_str()) {
        let win = NtmWindow {
            name: name.clone(), layout: Some("horizontal".to_string()),
            panes: vec![], focus_index: Some(0),
        };
        let json = serde_json::to_string(&win).unwrap();
        let back: NtmWindow = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
        prop_assert_eq!(back.focus_index, Some(0));
    }

    #[test]
    fn ni_s02_ntm_pane_serde(role in proptest::option::of(arb_ni_str()), is_focus in proptest::bool::ANY) {
        let pane = NtmPane {
            role: role.clone(), command: Some("bash".to_string()), args: vec!["-l".to_string()],
            environment: HashMap::new(), cwd: None, split_direction: None,
            split_ratio: Some(0.5), is_focus,
        };
        let json = serde_json::to_string(&pane).unwrap();
        let back: NtmPane = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.role, &role);
        prop_assert_eq!(back.is_focus, is_focus);
    }

    #[test]
    fn ni_s03_ntm_workflow_trigger_serde(kind in arb_ni_str(), value in arb_ni_str()) {
        let trigger = NtmWorkflowTrigger {
            kind: kind.clone(), value: value.clone(), pane_filter: None,
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let back: NtmWorkflowTrigger = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.kind, &kind);
        prop_assert_eq!(&back.value, &value);
    }

    #[test]
    fn ni_s04_ntm_workflow_step_serde(name in arb_ni_str(), action in arb_ni_str()) {
        let step = NtmWorkflowStep {
            name: name.clone(), action: action.clone(),
            params: HashMap::new(), conditions: vec![], timeout_secs: Some(30),
        };
        let json = serde_json::to_string(&step).unwrap();
        let back: NtmWorkflowStep = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
        prop_assert_eq!(&back.action, &action);
    }

    #[test]
    fn ni_s05_ntm_safety_config_serde(gate in proptest::bool::ANY, rate in 0u32..100) {
        let cfg = NtmSafetyConfig {
            command_safety_gate: gate, require_approval_destructive: true,
            rate_limit_per_minute: rate,
            allowlist: vec!["ls".to_string()], denylist: vec!["rm".to_string()],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: NtmSafetyConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.command_safety_gate, gate);
        prop_assert_eq!(back.rate_limit_per_minute, rate);
    }

    #[test]
    fn ni_s06_ntm_robot_config_serde(auth in proptest::bool::ANY, max_c in proptest::option::of(1u32..100)) {
        let cfg = NtmRobotConfig {
            bind_address: Some("127.0.0.1:9090".to_string()),
            require_auth: auth, max_concurrent: max_c,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: NtmRobotConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.require_auth, auth);
        prop_assert_eq!(back.max_concurrent, max_c);
    }

    #[test]
    fn ni_s07_ntm_hook_config_serde(event in arb_ni_str(), cmd in arb_ni_str()) {
        let hook = NtmHookConfig {
            event: event.clone(), command: cmd.clone(), timeout_secs: Some(10),
        };
        let json = serde_json::to_string(&hook).unwrap();
        let back: NtmHookConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.event, &event);
        prop_assert_eq!(&back.command, &cmd);
    }

    #[test]
    fn ni_s08_import_severity_serde(sev in arb_import_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: ImportSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, sev);
    }

    #[test]
    fn ni_s09_import_item_result_serde(sid in arb_ni_str(), success in proptest::bool::ANY) {
        let result = ImportItemResult {
            source_id: sid.clone(), target_type: "session_profile".to_string(),
            success, findings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ImportItemResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.source_id, &sid);
        prop_assert_eq!(back.success, success);
    }

    #[test]
    fn ni_s10_import_report_serde(total in 0usize..10, success_count in 0usize..10) {
        let report = ImportReport {
            schema_version: "1.0".to_string(), source_system: "ntm".to_string(),
            total_items: total, success_count, failure_count: 0, warning_count: 0,
            items: vec![], finding_summary: BTreeMap::new(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: ImportReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_items, total);
        prop_assert_eq!(back.success_count, success_count);
    }

    #[test]
    fn ni_s11_translated_session_profile_serde(name in arb_ni_str(), role in arb_ni_str()) {
        let profile = TranslatedSessionProfile {
            name: name.clone(), description: "test".to_string(), role: role.clone(),
            spawn_command: None, environment: HashMap::new(), working_directory: None,
            resource_hints: TranslatedResourceHints::default(),
            layout_template: None, bootstrap_commands: vec![], tags: vec![],
        };
        let json = serde_json::to_string(&profile).unwrap();
        let back: TranslatedSessionProfile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
        prop_assert_eq!(&back.role, &role);
    }

    #[test]
    fn ni_s12_translated_spawn_command_serde(cmd in arb_ni_str(), shell in proptest::bool::ANY) {
        let sc = TranslatedSpawnCommand {
            command: cmd.clone(), args: vec!["--flag".to_string()], use_shell: shell,
        };
        let json = serde_json::to_string(&sc).unwrap();
        let back: TranslatedSpawnCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.command, &cmd);
        prop_assert_eq!(back.use_shell, shell);
    }

    #[test]
    fn ni_s13_translated_resource_hints_serde(rows in 10u16..100, cols in 40u16..200) {
        let hints = TranslatedResourceHints {
            min_rows: rows, min_cols: cols, max_scrollback: 5000, priority_weight: 100,
        };
        let json = serde_json::to_string(&hints).unwrap();
        let back: TranslatedResourceHints = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.min_rows, rows);
        prop_assert_eq!(back.min_cols, cols);
    }

    #[test]
    fn ni_s14_translated_layout_template_serde(name in arb_ni_str(), panes in 1u32..10) {
        let tmpl = TranslatedLayoutTemplate {
            name: name.clone(), description: Some("test layout".to_string()),
            root: TranslatedLayoutNode::Slot { role: Some("main".to_string()), weight: 1.0 },
            min_panes: panes,
        };
        let json = serde_json::to_string(&tmpl).unwrap();
        let back: TranslatedLayoutTemplate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
        prop_assert_eq!(back.min_panes, panes);
    }

    #[test]
    fn ni_s15_translated_workflow_step_serde(name in arb_ni_str(), step_type in arb_translated_step_type()) {
        let step = TranslatedWorkflowStep {
            name: name.clone(), description: "test step".to_string(), step_type,
        };
        let json = serde_json::to_string(&step).unwrap();
        let back: TranslatedWorkflowStep = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
    }

    #[test]
    fn ni_s16_translated_config_serde(log_level in proptest::option::of(arb_ni_str())) {
        let cfg = TranslatedConfig {
            log_level: log_level.clone(), workspace: Some("/tmp/test".to_string()),
            poll_interval_ms: Some(500), pattern_packs: vec!["default".to_string()],
            safety: TranslatedSafetyConfig::default(),
            untranslated: HashMap::new(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TranslatedConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.log_level, &log_level);
    }

    #[test]
    fn ni_s17_translated_safety_config_serde(gate in proptest::bool::ANY, rate in 0u32..200) {
        let cfg = TranslatedSafetyConfig {
            command_safety_gate: gate, require_approval_destructive: true,
            rate_limit_per_minute: rate, allowlist: vec![], denylist: vec![],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TranslatedSafetyConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.command_safety_gate, gate);
        prop_assert_eq!(back.rate_limit_per_minute, rate);
    }

    #[test]
    fn ni_s18_ntm_import_bundle_serde(name in arb_ni_str()) {
        let bundle = NtmImportBundle {
            session_profiles: vec![TranslatedSessionProfile {
                name: name.clone(), description: "test".to_string(), role: "agent".to_string(),
                spawn_command: None, environment: HashMap::new(), working_directory: None,
                resource_hints: TranslatedResourceHints::default(),
                layout_template: None, bootstrap_commands: vec![], tags: vec![],
            }],
            layout_templates: vec![], workflows: vec![],
            config: None,
            report: ImportReport {
                schema_version: "1.0".to_string(), source_system: "ntm".to_string(),
                total_items: 1, success_count: 1, failure_count: 0, warning_count: 0,
                items: vec![], finding_summary: BTreeMap::new(),
            },
        };
        let json = serde_json::to_string(&bundle).unwrap();
        let back: NtmImportBundle = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.session_profiles[0].name, &name);
    }
}
