//! Property-based tests for the `restore_process` module.
//!
//! Covers `LaunchConfig` serde roundtrips and defaults, `LaunchAction` tagged
//! enum serde roundtrips, `ProcessPlan`/`LaunchResult`/`LaunchReport` serde
//! roundtrips, and `LaunchReport` default values.

use std::collections::HashMap;
use std::path::PathBuf;

use frankenterm_core::restore_process::{
    LaunchAction, LaunchConfig, LaunchReport, LaunchResult, ProcessPlan,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_pathbuf() -> impl Strategy<Value = PathBuf> {
    "/[a-z]{3,10}(/[a-z]{3,10}){0,3}".prop_map(PathBuf::from)
}

fn arb_launch_action() -> impl Strategy<Value = LaunchAction> {
    prop_oneof![
        (
            "[a-z]{3,10}",
            arb_pathbuf(),
        )
            .prop_map(|(shell, cwd)| LaunchAction::LaunchShell { shell, cwd }),
        (
            "[a-z ]{5,30}",
            arb_pathbuf(),
            "[a-z_]{3,15}",
        )
            .prop_map(|(command, cwd, agent_type)| LaunchAction::LaunchAgent {
                command,
                cwd,
                agent_type,
            }),
        "[a-z ]{5,30}".prop_map(|reason| LaunchAction::Skip { reason }),
        (
            "[A-Za-z ]{5,30}",
            "[a-z]{3,10}",
        )
            .prop_map(|(hint, original_process)| LaunchAction::Manual {
                hint,
                original_process,
            }),
    ]
}

fn arb_launch_config() -> impl Strategy<Value = LaunchConfig> {
    (
        any::<bool>(),
        any::<bool>(),
        0_u64..5000,
    )
        .prop_map(|(launch_shells, launch_agents, launch_delay_ms)| LaunchConfig {
            launch_shells,
            launch_agents,
            launch_delay_ms,
            agent_commands: HashMap::new(),
        })
}

fn arb_process_plan() -> impl Strategy<Value = ProcessPlan> {
    (
        0_u64..1000,
        0_u64..1000,
        arb_launch_action(),
        proptest::option::of("[A-Za-z ]{5,40}"),
    )
        .prop_map(|(old_pane_id, new_pane_id, action, state_warning)| ProcessPlan {
            old_pane_id,
            new_pane_id,
            action,
            state_warning,
        })
}

fn arb_launch_result() -> impl Strategy<Value = LaunchResult> {
    (
        0_u64..1000,
        0_u64..1000,
        arb_launch_action(),
        any::<bool>(),
        proptest::option::of("[a-z ]{3,20}"),
    )
        .prop_map(|(old_pane_id, new_pane_id, action, success, error)| LaunchResult {
            old_pane_id,
            new_pane_id,
            action,
            success,
            error,
        })
}

// =========================================================================
// LaunchConfig — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// LaunchConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde(config in arb_launch_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: LaunchConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.launch_shells, config.launch_shells);
        prop_assert_eq!(back.launch_agents, config.launch_agents);
        prop_assert_eq!(back.launch_delay_ms, config.launch_delay_ms);
    }

    /// Default LaunchConfig has expected values.
    #[test]
    fn prop_config_defaults(_dummy in 0..1_u8) {
        let config = LaunchConfig::default();
        prop_assert!(config.launch_shells);
        prop_assert!(!config.launch_agents);
        prop_assert_eq!(config.launch_delay_ms, 500);
        prop_assert!(config.agent_commands.is_empty());
    }

    /// LaunchConfig deserializes from empty JSON with defaults.
    #[test]
    fn prop_config_from_empty_json(_dummy in 0..1_u8) {
        let back: LaunchConfig = serde_json::from_str("{}").unwrap();
        prop_assert!(back.launch_shells);
        prop_assert!(!back.launch_agents);
        prop_assert_eq!(back.launch_delay_ms, 500);
    }

    /// LaunchConfig with agent_commands roundtrips.
    #[test]
    fn prop_config_with_commands(
        agent_type in "[a-z_]{3,15}",
        cmd in "[a-z /{}.]{5,30}",
    ) {
        let mut config = LaunchConfig::default();
        config.agent_commands.insert(agent_type.clone(), cmd.clone());
        let json = serde_json::to_string(&config).unwrap();
        let back: LaunchConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.agent_commands.get(&agent_type), Some(&cmd));
    }

    /// LaunchConfig serde is deterministic.
    #[test]
    fn prop_config_deterministic(config in arb_launch_config()) {
        let j1 = serde_json::to_string(&config).unwrap();
        let j2 = serde_json::to_string(&config).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// LaunchAction — tagged enum serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// LaunchAction serde roundtrip preserves variant and fields.
    #[test]
    fn prop_action_serde(action in arb_launch_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: LaunchAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, action);
    }

    /// LaunchAction JSON contains the "action" tag.
    #[test]
    fn prop_action_has_tag(action in arb_launch_action()) {
        let json = serde_json::to_string(&action).unwrap();
        prop_assert!(json.contains("\"action\""));
    }

    /// LaunchAction tag is snake_case.
    #[test]
    fn prop_action_tag_snake_case(action in arb_launch_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let expected_tag = match &action {
            LaunchAction::LaunchShell { .. } => "launch_shell",
            LaunchAction::LaunchAgent { .. } => "launch_agent",
            LaunchAction::Skip { .. } => "skip",
            LaunchAction::Manual { .. } => "manual",
        };
        prop_assert!(
            json.contains(expected_tag),
            "expected tag '{}' in JSON: {}",
            expected_tag, json
        );
    }

    /// LaunchAction serde is deterministic.
    #[test]
    fn prop_action_deterministic(action in arb_launch_action()) {
        let j1 = serde_json::to_string(&action).unwrap();
        let j2 = serde_json::to_string(&action).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// ProcessPlan — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ProcessPlan serde roundtrip preserves all fields.
    #[test]
    fn prop_plan_serde(plan in arb_process_plan()) {
        let json = serde_json::to_string(&plan).unwrap();
        let back: ProcessPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.old_pane_id, plan.old_pane_id);
        prop_assert_eq!(back.new_pane_id, plan.new_pane_id);
        prop_assert_eq!(back.action, plan.action);
        prop_assert_eq!(&back.state_warning, &plan.state_warning);
    }
}

// =========================================================================
// LaunchResult — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// LaunchResult serde roundtrip preserves all fields.
    #[test]
    fn prop_result_serde(result in arb_launch_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: LaunchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.old_pane_id, result.old_pane_id);
        prop_assert_eq!(back.new_pane_id, result.new_pane_id);
        prop_assert_eq!(back.action, result.action);
        prop_assert_eq!(back.success, result.success);
        prop_assert_eq!(&back.error, &result.error);
    }
}

// =========================================================================
// LaunchReport — serde roundtrip and defaults
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Default LaunchReport has all zero counts.
    #[test]
    fn prop_report_defaults(_dummy in 0..1_u8) {
        let report = LaunchReport::default();
        prop_assert!(report.results.is_empty());
        prop_assert_eq!(report.shells_launched, 0);
        prop_assert_eq!(report.agents_launched, 0);
        prop_assert_eq!(report.skipped, 0);
        prop_assert_eq!(report.manual, 0);
        prop_assert_eq!(report.failed, 0);
    }

    /// LaunchReport serde roundtrip preserves counts.
    #[test]
    fn prop_report_serde(
        shells in 0_usize..10,
        agents in 0_usize..10,
        skipped in 0_usize..10,
        manual in 0_usize..10,
        failed in 0_usize..10,
    ) {
        let report = LaunchReport {
            results: vec![],
            shells_launched: shells,
            agents_launched: agents,
            skipped,
            manual,
            failed,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: LaunchReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.shells_launched, shells);
        prop_assert_eq!(back.agents_launched, agents);
        prop_assert_eq!(back.skipped, skipped);
        prop_assert_eq!(back.manual, manual);
        prop_assert_eq!(back.failed, failed);
    }

    /// LaunchReport with results roundtrips.
    #[test]
    fn prop_report_with_results(result in arb_launch_result()) {
        let report = LaunchReport {
            results: vec![result.clone()],
            shells_launched: 1,
            ..Default::default()
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: LaunchReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.results.len(), 1);
        prop_assert_eq!(back.results[0].old_pane_id, result.old_pane_id);
        prop_assert_eq!(&back.results[0].action, &result.action);
    }

    /// LaunchReport serde is deterministic.
    #[test]
    fn prop_report_deterministic(
        shells in 0_usize..10,
        agents in 0_usize..10,
    ) {
        let report = LaunchReport {
            results: vec![],
            shells_launched: shells,
            agents_launched: agents,
            ..Default::default()
        };
        let j1 = serde_json::to_string(&report).unwrap();
        let j2 = serde_json::to_string(&report).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn launch_action_variants_distinct() {
    let shell = LaunchAction::LaunchShell {
        shell: "bash".to_string(),
        cwd: PathBuf::from("/home"),
    };
    let agent = LaunchAction::LaunchAgent {
        command: "claude".to_string(),
        cwd: PathBuf::from("/project"),
        agent_type: "claude_code".to_string(),
    };
    let skip = LaunchAction::Skip {
        reason: "disabled".to_string(),
    };
    let manual = LaunchAction::Manual {
        hint: "restart vim".to_string(),
        original_process: "vim".to_string(),
    };
    assert_ne!(shell, agent);
    assert_ne!(shell, skip);
    assert_ne!(shell, manual);
    assert_ne!(agent, skip);
}

#[test]
fn config_partial_json_fills_defaults() {
    let json = r#"{"launch_agents": true}"#;
    let config: LaunchConfig = serde_json::from_str(json).unwrap();
    assert!(config.launch_agents);
    // Defaults for missing fields
    assert!(config.launch_shells);
    assert_eq!(config.launch_delay_ms, 500);
}

#[test]
fn report_empty_roundtrip() {
    let report = LaunchReport::default();
    let json = serde_json::to_string(&report).unwrap();
    let back: LaunchReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.shells_launched, 0);
    assert!(back.results.is_empty());
}
