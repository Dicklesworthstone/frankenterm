//! LabRuntime port of all `#[tokio::test]` async tests from `restore_process.rs`.
//!
//! Feature-gated behind `asupersync-runtime`.
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::fixtures::RuntimeFixture;
use frankenterm_core::restore_process::{LaunchAction, LaunchConfig, ProcessLauncher, ProcessPlan};
use frankenterm_core::wezterm::{MockWezterm, WeztermHandle};

async fn mock_with_panes(pane_ids: &[u64]) -> WeztermHandle {
    let mock = MockWezterm::new();
    for &id in pane_ids {
        mock.add_default_pane(id).await;
    }
    Arc::new(mock) as WeztermHandle
}

// ===========================================================================
// 1. execute_shell_launch
// ===========================================================================

#[test]
fn execute_shell_launch() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let wez = mock_with_panes(&[100]).await;
        let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::LaunchShell {
                shell: "bash".into(),
                cwd: PathBuf::from("/home/user"),
            },
            state_warning: None,
        }];

        let report = launcher.execute(&plans).await;
        assert_eq!(report.shells_launched, 1);
        assert_eq!(report.failed, 0);
    });
}

// ===========================================================================
// 2. execute_mixed_plan
// ===========================================================================

#[test]
fn execute_mixed_plan() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let wez = mock_with_panes(&[100, 200, 300]).await;
        let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
        let plans = vec![
            ProcessPlan {
                old_pane_id: 1,
                new_pane_id: 100,
                action: LaunchAction::LaunchShell {
                    shell: "zsh".into(),
                    cwd: PathBuf::from("/project"),
                },
                state_warning: None,
            },
            ProcessPlan {
                old_pane_id: 2,
                new_pane_id: 200,
                action: LaunchAction::Skip {
                    reason: "no process info".into(),
                },
                state_warning: None,
            },
            ProcessPlan {
                old_pane_id: 3,
                new_pane_id: 300,
                action: LaunchAction::Manual {
                    hint: "Was running vim".into(),
                    original_process: "vim".into(),
                },
                state_warning: None,
            },
        ];

        let report = launcher.execute(&plans).await;
        assert_eq!(report.shells_launched, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.manual, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.results.len(), 3);
    });
}

// ===========================================================================
// 3. execute_empty_plans
// ===========================================================================

#[test]
fn execute_empty_plans() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let wez = mock_with_panes(&[]).await;
        let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
        let report = launcher.execute(&[]).await;
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
        let wez = mock_with_panes(&[]).await;
        let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
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
        let report = launcher.execute(&plans).await;
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
        let wez = mock_with_panes(&[]).await;
        let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::Manual {
                hint: "Restart vim manually".into(),
                original_process: "vim".into(),
            },
            state_warning: None,
        }];
        let report = launcher.execute(&plans).await;
        assert_eq!(report.manual, 1);
        assert_eq!(report.shells_launched, 0);
        assert!(report.results[0].success);
    });
}

// ===========================================================================
// 6. execute_agent_launch
// ===========================================================================

#[test]
fn execute_agent_launch() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let wez = mock_with_panes(&[100]).await;
        let launcher = ProcessLauncher::new(wez, LaunchConfig::default());
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 100,
            action: LaunchAction::LaunchAgent {
                command: "cd /proj && claude".into(),
                cwd: PathBuf::from("/proj"),
                agent_type: "claude_code".into(),
            },
            state_warning: Some("new session warning".into()),
        }];
        let report = launcher.execute(&plans).await;
        assert_eq!(report.agents_launched, 1);
        assert_eq!(report.failed, 0);
        assert!(report.results[0].success);
    });
}

// ===========================================================================
// 7. execute_report_result_order_preserved
// ===========================================================================

#[test]
fn execute_report_result_order_preserved() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let wez = mock_with_panes(&[100, 200]).await;
        let launcher = ProcessLauncher::new(
            wez,
            LaunchConfig {
                launch_delay_ms: 0,
                ..Default::default()
            },
        );
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
        let report = launcher.execute(&plans).await;
        assert_eq!(report.results[0].old_pane_id, 1);
        assert_eq!(report.results[1].old_pane_id, 2);
    });
}
