//! LabRuntime port of all `#[tokio::test]` async tests from `restore_process.rs`.
//!
//! Feature-gated behind `asupersync-runtime`.
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::fixtures::RuntimeFixture;
use frankenterm_core::restore_process::{LaunchAction, ProcessLauncher, ProcessPlan};
use frankenterm_core::wezterm::MockWezterm;

// ===========================================================================
// 1. execute_shell_launch_is_refused_without_pty_input
// ===========================================================================

#[test]
fn execute_shell_launch_is_refused_without_pty_input() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(100).await;
        let launcher = ProcessLauncher::new();
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::LaunchShell {
                shell: "bash".into(),
                cwd: PathBuf::from("/home/user"),
            },
            state_warning: None,
        }];

        let report = launcher.execute(&plans);
        assert_eq!(report.shells_launched, 0);
        assert_eq!(report.failed, 1);
        assert!(!report.results[0].success);
        assert_eq!(mock.pane_state(100).await.unwrap().content, "");
    });
}

// ===========================================================================
// 2. execute_mixed_plan
// ===========================================================================

#[test]
fn execute_mixed_plan() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let launcher = ProcessLauncher::new();
        let plans = vec![
            ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::LaunchShell {
                    shell: "secret-shell".into(),
                    cwd: PathBuf::from("/secret/project"),
                },
                state_warning: None,
            },
            ProcessPlan {
                old_pane_id: 2,
                new_pane_id: 200,
                action: LaunchAction::Skip {
                    reason: "secret-skip-reason".into(),
                },
                state_warning: None,
            },
            ProcessPlan {
                old_pane_id: 3,
                new_pane_id: 300,
                action: LaunchAction::Manual {
                    hint: "secret-manual-hint".into(),
                    original_process: "secret-process".into(),
                },
                state_warning: None,
            },
        ];

        let report = launcher.execute(&plans);
        assert_eq!(report.shells_launched, 0);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.manual, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.results.len(), 3);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret"));
    });
}

// ===========================================================================
// 3. execute_empty_plans
// ===========================================================================

#[test]
fn execute_empty_plans() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let launcher = ProcessLauncher::new();
        let report = launcher.execute(&[]);
        assert_eq!(report.results.len(), 0);
        assert_eq!(report.shells_launched, 0);
        assert_eq!(report.failed, 0);
    });
}

// ===========================================================================
// 4. execute_skip_only
// ===========================================================================

#[test]
fn execute_skip_only() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let launcher = ProcessLauncher::new();
        let plans = vec![
            ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::Skip {
                    reason: "disabled".into(),
                },
                state_warning: None,
            },
            ProcessPlan {
                old_pane_id: 2,
                new_pane_id: 200,
                action: LaunchAction::Skip {
                    reason: "no info".into(),
                },
                state_warning: None,
            },
        ];
        let report = launcher.execute(&plans);
        assert_eq!(report.skipped, 2);
        assert_eq!(report.shells_launched, 0);
        assert_eq!(report.agents_launched, 0);
        assert_eq!(report.results.len(), 2);
        assert!(report.results.iter().all(|r| r.success));
    });
}

// ===========================================================================
// 5. execute_manual_only
// ===========================================================================

#[test]
fn execute_manual_only() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let launcher = ProcessLauncher::new();
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::Manual {
                hint: "Restart vim manually".into(),
                original_process: "vim".into(),
            },
            state_warning: None,
        }];
        let report = launcher.execute(&plans);
        assert_eq!(report.manual, 1);
        assert_eq!(report.shells_launched, 0);
        assert!(report.results[0].success);
    });
}

// ===========================================================================
// 6. execute_legacy_agent_plan_is_refused
// ===========================================================================

#[test]
fn execute_legacy_agent_plan_is_refused() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(100).await;
        let launcher = ProcessLauncher::new();
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::LaunchAgent {
                command: "secret-agent-command".into(),
                cwd: PathBuf::from("/secret/agent/path"),
                agent_type: "secret-agent-type".into(),
            },
            state_warning: Some("secret-state-warning".into()),
        }];
        let report = launcher.execute(&plans);
        assert_eq!(report.agents_launched, 0);
        assert_eq!(report.failed, 1);
        assert!(!report.results[0].success);
        assert_eq!(mock.pane_state(100).await.unwrap().content, "");
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("secret"));
    });
}

// ===========================================================================
// 7. execute_report_result_order_preserved
// ===========================================================================

#[test]
fn execute_report_result_order_preserved() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let launcher = ProcessLauncher::new();
        let plans = vec![
            ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::LaunchShell {
                    shell: "bash".into(),
                    cwd: PathBuf::from("/a"),
                },
                state_warning: None,
            },
            ProcessPlan {
                old_pane_id: 2,
                new_pane_id: 200,
                action: LaunchAction::Skip {
                    reason: "skip".into(),
                },
                state_warning: None,
            },
        ];
        let report = launcher.execute(&plans);
        assert_eq!(report.results[0].old_pane_id, 1);
        assert_eq!(report.results[1].old_pane_id, 2);
    });
}
