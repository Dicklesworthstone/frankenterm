//! Process disposition engine for restored panes.
//!
//! Layout restoration already creates a default shell at the restored working
//! directory. This module classifies the captured foreground process and reports
//! whether operator follow-up is required.
//!
//! # Safety
//!
//! Shell switching and agent execution require an argv-isolated mux spawn API.
//! Until that API exists, the planner reports those cases as manual. Executable
//! launch plans are not representable by this module's public API.
//!
//! # Data flow
//!
//! ```text
//! PaneStateSnapshot (DB) → ProcessPlan → finite LaunchReport
//! ```

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::session_pane_state::PaneStateSnapshot;

// =============================================================================
// Plan types
// =============================================================================

/// Finite, content-free reason for a process disposition.
///
/// Captured commands, argv, working directories, agent metadata, and operator
/// hints are intentionally never copied into a plan or report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessDispositionReason {
    /// Layout restoration already created the pane's default shell.
    DefaultShellCreated,
    /// The capture surface supplied no trustworthy foreground-process or shell
    /// classification, so continuity cannot be inferred from absence.
    CapturedProcessStateUnavailableRequiresManualRecovery,
    /// A captured shell cannot be resumed or switched automatically.
    CapturedShellRequiresManualRecovery,
    /// A captured agent cannot be resumed automatically.
    CapturedAgentRequiresManualRecovery,
    /// A recognized interactive program cannot be resumed automatically.
    CapturedInteractiveProgramRequiresManualRecovery,
    /// Another captured foreground process cannot be resumed automatically.
    CapturedForegroundProcessRequiresManualRecovery,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_pane_state::{AgentMetadata, ProcessInfo, TerminalState};

    fn pane(pane_id: u64) -> PaneStateSnapshot {
        PaneStateSnapshot {
            schema_version: 1,
            pane_id,
            captured_at: 1,
            cwd: Some("/raw/cwd-canary".to_string()),
            foreground_process: None,
            shell: None,
            terminal: TerminalState {
                rows: 24,
                cols: 80,
                cursor_row: 0,
                cursor_col: 0,
                is_alt_screen: false,
                title: "raw-title-canary".to_string(),
            },
            scrollback_ref: None,
            agent: None,
            env: None,
        }
    }

    fn mapped(count: u64) -> HashMap<u64, u64> {
        (0..count)
            .map(|id| (id, id.saturating_add(10_000)))
            .collect()
    }

    #[test]
    fn classifications_are_finite_and_content_free() {
        let mut panes = vec![pane(0), pane(1), pane(2), pane(3), pane(4)];
        panes[0].agent = Some(AgentMetadata {
            agent_type: "raw-agent-canary".to_string(),
            session_id: Some("raw-session-canary".to_string()),
            state: Some("raw-state-canary".to_string()),
        });
        panes[1].foreground_process = Some(ProcessInfo {
            name: "zsh".to_string(),
            pid: Some(1),
            argv: Some(vec!["raw-shell-argv-canary".to_string()]),
        });
        panes[2].foreground_process = Some(ProcessInfo {
            name: "vim".to_string(),
            pid: Some(2),
            argv: Some(vec!["raw-editor-argv-canary".to_string()]),
        });
        panes[3].foreground_process = Some(ProcessInfo {
            name: "raw-unknown-process-canary".to_string(),
            pid: Some(3),
            argv: Some(vec!["raw-unknown-argv-canary".to_string()]),
        });

        let plans = ProcessLauncher::plan(&mapped(5), &panes);
        assert_eq!(plans.len(), 5);
        assert_eq!(
            plans[0].action,
            LaunchAction::Manual(ProcessDispositionReason::CapturedAgentRequiresManualRecovery)
        );
        assert_eq!(
            plans[1].action,
            LaunchAction::Manual(ProcessDispositionReason::CapturedShellRequiresManualRecovery)
        );
        assert_eq!(
            plans[2].action,
            LaunchAction::Manual(
                ProcessDispositionReason::CapturedInteractiveProgramRequiresManualRecovery
            )
        );
        assert_eq!(
            plans[3].action,
            LaunchAction::Manual(
                ProcessDispositionReason::CapturedForegroundProcessRequiresManualRecovery
            )
        );
        assert_eq!(
            plans[4].action,
            LaunchAction::Manual(
                ProcessDispositionReason::CapturedProcessStateUnavailableRequiresManualRecovery
            )
        );

        let diagnostic = format!("{plans:?}");
        for canary in ["raw-", "/raw/", "argv-canary", "state-canary"] {
            assert!(!diagnostic.contains(canary));
        }
    }

    #[test]
    fn captured_shell_field_requires_manual_recovery() {
        let mut state = pane(0);
        state.shell = Some("raw-shell-canary".to_string());
        let plans = ProcessLauncher::plan(&mapped(1), &[state]);
        assert_eq!(
            plans[0].action,
            LaunchAction::Manual(ProcessDispositionReason::CapturedShellRequiresManualRecovery)
        );
    }

    #[test]
    fn unmapped_panes_are_not_planned() {
        let plans = ProcessLauncher::plan(&HashMap::new(), &[pane(1)]);
        assert!(plans.is_empty());
    }

    #[test]
    fn pane_info_capture_without_process_authority_requires_manual_recovery() {
        let pane_info: crate::wezterm::PaneInfo = serde_json::from_value(serde_json::json!({
            "pane_id": 1,
            "tab_id": 2,
            "window_id": 3,
        }))
        .expect("minimal pane metadata");
        let captured = PaneStateSnapshot::from_pane_info(&pane_info, 1, false);
        assert!(captured.foreground_process.is_none());
        assert!(captured.shell.is_none());

        let plans = ProcessLauncher::plan(&HashMap::from([(1, 101)]), &[captured]);
        assert_eq!(
            plans[0].action,
            LaunchAction::Manual(
                ProcessDispositionReason::CapturedProcessStateUnavailableRequiresManualRecovery
            )
        );
    }

    #[test]
    fn process_classification_accepts_both_path_separator_styles() {
        assert!(is_shell("/usr/local/bin/zsh"));
        assert!(is_shell(r"C:\tools\fish"));
        assert!(is_agent_process("/opt/bin/codex"));
        assert!(is_agent_process(r"C:\tools\gemini-cli"));
        assert!(is_interactive_program(r"C:\tools\nvim"));
    }

    #[test]
    fn large_report_has_exact_totals_and_bounded_deterministic_sample() {
        let count = LAUNCH_RESULT_SAMPLE_CAP
            .saturating_mul(100)
            .saturating_add(7);
        let plans = (0..count)
            .map(|index| ProcessPlan {
                old_pane_id: u64::try_from(index).expect("test index fits u64"),
                new_pane_id: u64::try_from(index.saturating_add(50_000))
                    .expect("test index fits u64"),
                action: if index % 2 == 0 {
                    LaunchAction::Manual(
                        ProcessDispositionReason::CapturedAgentRequiresManualRecovery,
                    )
                } else {
                    LaunchAction::Skip(ProcessDispositionReason::DefaultShellCreated)
                },
            })
            .collect::<Vec<_>>();

        let report = ProcessLauncher::execute(&plans);
        assert_eq!(report.plans_total, count);
        assert_eq!(report.plans_settled, count);
        assert_eq!(report.manual.saturating_add(report.skipped), count);
        assert_eq!(report.result_sample.len(), LAUNCH_RESULT_SAMPLE_CAP);
        assert_eq!(report.result_sample[0].old_pane_id, 0);
        assert_eq!(
            report.result_sample[LAUNCH_RESULT_SAMPLE_CAP - 1].old_pane_id,
            u64::try_from(LAUNCH_RESULT_SAMPLE_CAP - 1).expect("sample index fits u64")
        );

        let second = ProcessLauncher::execute(&plans);
        assert_eq!(report.result_sample, second.result_sample);
        let diagnostic = format!("{report:?}");
        assert!(diagnostic.contains("sampled_results: 32"));
        assert!(diagnostic.len() < 512);
    }

    #[test]
    fn pre_cancel_retains_exact_zero_settled_count() {
        let cx = crate::cx::for_testing();
        cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("raw-cancel-detail-canary"),
        );
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 2,
            action: LaunchAction::Manual(
                ProcessDispositionReason::CapturedForegroundProcessRequiresManualRecovery,
            ),
        }];
        let report = ProcessLauncher::execute_cx(&cx, &plans);
        assert_eq!(report.plans_total, 1);
        assert_eq!(report.plans_settled, 0);
        assert!(report.result_sample.is_empty());
        assert_eq!(report.manual, 0);
        assert_eq!(
            report.interruption,
            Some(LaunchInterruption {
                plan_index: 0,
                phase: LaunchInterruptionPhase::BeforePlan,
                reason: LaunchInterruptionReason::Cancelled,
            })
        );
        assert!(!format!("{report:?}").contains("raw-cancel"));
    }

    #[test]
    fn interruption_preserves_deadline_poll_and_cost_budget_classes() {
        let plans = vec![ProcessPlan {
            old_pane_id: 1,
            new_pane_id: 2,
            action: LaunchAction::Skip(ProcessDispositionReason::DefaultShellCreated),
        }];
        for (budget, expected) in [
            (
                crate::cx::Budget::new().with_deadline(Default::default()),
                LaunchInterruptionReason::DeadlineExceeded,
            ),
            (
                crate::cx::Budget::new().with_poll_quota(0),
                LaunchInterruptionReason::PollQuotaExhausted,
            ),
            (
                crate::cx::Budget::new().with_cost_quota(0),
                LaunchInterruptionReason::CostQuotaExhausted,
            ),
        ] {
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            let report = ProcessLauncher::execute_cx(&cx, &plans);
            assert_eq!(
                report.interruption().map(|value| value.reason),
                Some(expected)
            );
            assert_eq!(report.plans_settled(), 0);
        }
    }

    #[test]
    fn interruption_preserves_cancellation_cleanup_timeout_class() {
        let cx = crate::cx::for_testing();
        let error = crate::runtime_async::ContextError::new(
            crate::runtime_async::ContextErrorKind::CancelTimeout,
        );
        assert_eq!(
            launch_interruption_reason(&cx, &error),
            LaunchInterruptionReason::CancellationCleanupTimedOut
        );
    }

    #[test]
    fn report_serialization_is_bounded_and_content_free() {
        let plans = (0..100)
            .map(|index| ProcessPlan {
                old_pane_id: index,
                new_pane_id: index.saturating_add(100),
                action: LaunchAction::Manual(
                    ProcessDispositionReason::CapturedAgentRequiresManualRecovery,
                ),
            })
            .collect::<Vec<_>>();
        let report = ProcessLauncher::execute(&plans);
        let encoded = serde_json::to_string(&report).expect("serialize finite report");
        assert_eq!(report.plans_total, 100);
        assert_eq!(report.result_sample.len(), LAUNCH_RESULT_SAMPLE_CAP);
        assert!(!encoded.contains("command"));
        assert!(!encoded.contains("argv"));
        assert!(!encoded.contains("cwd"));
        assert!(!encoded.contains("hint"));
        assert!(encoded.len() < 10_000);
    }
}

/// Finite action retained between classification and disposition settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchAction {
    /// Skip process follow-up for this pane.
    Skip(ProcessDispositionReason),
    /// Report that operator recovery is required for this pane.
    Manual(ProcessDispositionReason),
}

/// Finite, content-free disposition retained in execution reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchDisposition {
    Skip,
    Manual,
}

impl LaunchAction {
    const fn disposition(self) -> LaunchDisposition {
        match self {
            Self::Skip(_) => LaunchDisposition::Skip,
            Self::Manual(_) => LaunchDisposition::Manual,
        }
    }

    const fn reason(self) -> ProcessDispositionReason {
        match self {
            Self::Skip(reason) | Self::Manual(reason) => reason,
        }
    }
}

/// Process-restoration disposition plan for a single pane.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessPlan {
    /// Original pane ID from the snapshot.
    pub old_pane_id: u64,
    /// New pane ID after layout restoration.
    pub new_pane_id: u64,
    /// The action to take.
    pub action: LaunchAction,
}

/// Borrowed, finite process-classification input for one captured pane.
///
/// The referenced process name is inspected synchronously and never copied
/// into a plan, report, diagnostic, or serialized payload.
#[derive(Clone, Copy)]
pub struct ProcessDispositionInput<'a> {
    pub pane_id: u64,
    pub foreground_process_name: Option<&'a str>,
    pub shell_present: bool,
    pub agent_present: bool,
}

/// Result of settling a process plan on a single pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchResult {
    pub old_pane_id: u64,
    pub new_pane_id: u64,
    /// Content-free action category. Commands, paths, hints, and persisted
    /// process strings are deliberately not duplicated into reports.
    pub action: LaunchDisposition,
    pub reason: ProcessDispositionReason,
}

/// Maximum deterministic-prefix detail retained by a process report.
pub const LAUNCH_RESULT_SAMPLE_CAP: usize = 32;

/// Report after executing all process plans.
#[derive(Clone, Default, Serialize)]
pub struct LaunchReport {
    /// Bounded deterministic prefix of content-free per-pane results.
    result_sample: Vec<LaunchResult>,
    /// Exact number of plans presented to the evaluator.
    plans_total: usize,
    /// Exact number of plans settled before interruption.
    plans_settled: usize,
    skipped: usize,
    manual: usize,
    /// Structured reason that execution stopped before every plan settled.
    /// Cancellation stops the sequence immediately; callers use this field to
    /// leave the restore attempt unclean and require reconciliation rather than
    /// treating partial settlement as success.
    interruption: Option<LaunchInterruption>,
}

impl LaunchReport {
    pub fn result_sample(&self) -> &[LaunchResult] {
        &self.result_sample
    }

    pub const fn plans_total(&self) -> usize {
        self.plans_total
    }

    pub const fn plans_settled(&self) -> usize {
        self.plans_settled
    }

    pub const fn skipped_count(&self) -> usize {
        self.skipped
    }

    pub const fn manual_count(&self) -> usize {
        self.manual
    }

    pub const fn interruption(&self) -> Option<&LaunchInterruption> {
        self.interruption.as_ref()
    }
}

impl fmt::Debug for LaunchReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchReport")
            .field("sampled_results", &self.result_sample.len())
            .field("sample_cap", &LAUNCH_RESULT_SAMPLE_CAP)
            .field("plans_total", &self.plans_total)
            .field("plans_settled", &self.plans_settled)
            .field("skipped", &self.skipped)
            .field("manual", &self.manual)
            .field("interrupted", &self.interruption.is_some())
            .finish()
    }
}

/// Phase at which a process-disposition sequence stopped cooperatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchInterruptionPhase {
    BeforePlan,
}

/// Finite reason that disposition evaluation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchInterruptionReason {
    Cancelled,
    CancellationCleanupTimedOut,
    DeadlineExceeded,
    PollQuotaExhausted,
    CostQuotaExhausted,
    ContextFailure,
}

/// Typed partial-result marker for an interrupted process-disposition sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchInterruption {
    /// Zero-based plan index that did not settle.
    pub plan_index: usize,
    pub phase: LaunchInterruptionPhase,
    pub reason: LaunchInterruptionReason,
}

fn launch_interruption_reason(
    cx: &crate::cx::Cx,
    error: &crate::runtime_async::ContextError,
) -> LaunchInterruptionReason {
    use crate::outcome::CancelKind;
    use crate::runtime_async::ContextErrorKind;

    match error.kind() {
        ContextErrorKind::DeadlineExceeded => LaunchInterruptionReason::DeadlineExceeded,
        ContextErrorKind::PollQuotaExhausted => LaunchInterruptionReason::PollQuotaExhausted,
        ContextErrorKind::CostQuotaExhausted => LaunchInterruptionReason::CostQuotaExhausted,
        ContextErrorKind::CancelTimeout => LaunchInterruptionReason::CancellationCleanupTimedOut,
        ContextErrorKind::Cancelled => match cx.root_cancel_cause().map(|reason| reason.kind) {
            Some(CancelKind::Deadline | CancelKind::Timeout) => {
                LaunchInterruptionReason::DeadlineExceeded
            }
            Some(CancelKind::PollQuota) => LaunchInterruptionReason::PollQuotaExhausted,
            Some(CancelKind::CostBudget) => LaunchInterruptionReason::CostQuotaExhausted,
            Some(
                CancelKind::User
                | CancelKind::FailFast
                | CancelKind::RaceLost
                | CancelKind::ParentCancelled
                | CancelKind::ResourceUnavailable
                | CancelKind::Shutdown
                | CancelKind::LinkedExit,
            )
            | None => LaunchInterruptionReason::Cancelled,
        },
        _ => LaunchInterruptionReason::ContextFailure,
    }
}

// =============================================================================
// ProcessLauncher
// =============================================================================

/// Namespace for classifying and settling process-restoration dispositions.
#[derive(Debug, Clone, Copy)]
pub struct ProcessLauncher;

impl ProcessLauncher {
    /// Generate a process-disposition plan without executing anything.
    ///
    /// The plan maps each pane from the snapshot to a finite action based on
    /// captured process classification. It never retains captured text.
    pub fn plan(
        pane_id_map: &HashMap<u64, u64>,
        pane_states: &[PaneStateSnapshot],
    ) -> Vec<ProcessPlan> {
        Self::plan_inputs(
            pane_id_map,
            pane_states.iter().map(|state| ProcessDispositionInput {
                pane_id: state.pane_id,
                foreground_process_name: state
                    .foreground_process
                    .as_ref()
                    .map(|process| process.name.as_str()),
                shell_present: state.shell.is_some(),
                agent_present: state.agent.is_some(),
            }),
        )
    }

    /// Plan directly from borrowed finite capture fields without cloning
    /// terminal state, cwd, command, or agent metadata.
    pub fn plan_inputs<'a, I>(pane_id_map: &HashMap<u64, u64>, pane_states: I) -> Vec<ProcessPlan>
    where
        I: IntoIterator<Item = ProcessDispositionInput<'a>>,
        I::IntoIter: ExactSizeIterator,
    {
        let pane_states = pane_states.into_iter();
        let mut plans = Vec::with_capacity(pane_states.len().min(pane_id_map.len()));

        for state in pane_states {
            let new_pane_id = match pane_id_map.get(&state.pane_id) {
                Some(&id) => id,
                None => continue,
            };

            plans.push(ProcessPlan {
                old_pane_id: state.pane_id,
                new_pane_id,
                action: Self::resolve_input_action(state),
            });
        }

        plans
    }

    /// Settle a set of process plans without writing commands to panes.
    ///
    /// Plans are evaluated sequentially. Every generated action is a
    /// content-free `Skip` or `Manual` disposition; no process is launched and
    /// no PTY input is written.
    pub fn execute(plans: &[ProcessPlan]) -> LaunchReport {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        Self::execute_cx(&cx, plans)
    }

    /// Execute process plans under an explicit `&Cx` (ft-xbnl0.2.2 Cx-first
    /// API).
    ///
    /// Caller cancellation is checked before every disposition. Automatic
    /// process launch is intentionally absent because the mux boundary has no
    /// argv-isolated command spawn API.
    pub fn execute_cx(cx: &crate::cx::Cx, plans: &[ProcessPlan]) -> LaunchReport {
        let mut report = LaunchReport {
            result_sample: Vec::with_capacity(plans.len().min(LAUNCH_RESULT_SAMPLE_CAP)),
            plans_total: plans.len(),
            ..LaunchReport::default()
        };

        for (i, plan) in plans.iter().enumerate() {
            if let Err(error) = cx.checkpoint() {
                report.interruption = Some(LaunchInterruption {
                    plan_index: i,
                    phase: LaunchInterruptionPhase::BeforePlan,
                    reason: launch_interruption_reason(cx, &error),
                });
                break;
            }
            match plan.action {
                LaunchAction::Skip(_) => {
                    report.skipped = report.skipped.saturating_add(1);
                }
                LaunchAction::Manual(_) => {
                    report.manual = report.manual.saturating_add(1);
                }
            }
            report.plans_settled = report.plans_settled.saturating_add(1);
            if report.result_sample.len() < LAUNCH_RESULT_SAMPLE_CAP {
                report.result_sample.push(LaunchResult {
                    old_pane_id: plan.old_pane_id,
                    new_pane_id: plan.new_pane_id,
                    action: plan.action.disposition(),
                    reason: plan.action.reason(),
                });
            }
        }

        info!(
            plans_total = report.plans_total,
            plans_settled = report.plans_settled,
            sampled_results = report.result_sample.len(),
            skipped = report.skipped,
            manual = report.manual,
            interrupted = report.interruption.is_some(),
            "process restore dispositions settled"
        );

        report
    }

    // -------------------------------------------------------------------------
    // Internal: action resolution
    // -------------------------------------------------------------------------

    /// Determine what action to take for a pane based on its snapshot.
    fn resolve_input_action(state: ProcessDispositionInput<'_>) -> LaunchAction {
        // Explicit agent metadata is stronger than process-name heuristics.
        if state.agent_present {
            return LaunchAction::Manual(
                ProcessDispositionReason::CapturedAgentRequiresManualRecovery,
            );
        }

        // Classify by foreground process without retaining its name or argv.
        if let Some(process_name) = state.foreground_process_name {
            return Self::resolve_process_action(process_name);
        }

        // A captured shell is not the default shell instance created by the
        // layout mutation; resuming or switching it requires manual recovery.
        if state.shell_present {
            return LaunchAction::Manual(
                ProcessDispositionReason::CapturedShellRequiresManualRecovery,
            );
        }

        LaunchAction::Manual(
            ProcessDispositionReason::CapturedProcessStateUnavailableRequiresManualRecovery,
        )
    }

    /// Resolve an action from process classification alone.
    fn resolve_process_action(name: &str) -> LaunchAction {
        if is_agent_process(name) {
            return LaunchAction::Manual(
                ProcessDispositionReason::CapturedAgentRequiresManualRecovery,
            );
        }
        if is_shell(name) {
            return LaunchAction::Manual(
                ProcessDispositionReason::CapturedShellRequiresManualRecovery,
            );
        }
        if is_interactive_program(name) {
            return LaunchAction::Manual(
                ProcessDispositionReason::CapturedInteractiveProgramRequiresManualRecovery,
            );
        }
        LaunchAction::Manual(
            ProcessDispositionReason::CapturedForegroundProcessRequiresManualRecovery,
        )
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Check if a process name is a known shell.
fn is_shell(name: &str) -> bool {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or(name);
    matches!(
        basename,
        "bash" | "zsh" | "fish" | "sh" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "nushell"
    )
}

/// Check if a process name is a known interactive program that needs manual restart.
fn is_interactive_program(name: &str) -> bool {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or(name);
    matches!(
        basename,
        "vim"
            | "nvim"
            | "vi"
            | "nano"
            | "emacs"
            | "helix"
            | "hx"
            | "htop"
            | "btop"
            | "top"
            | "less"
            | "more"
            | "man"
            | "tmux"
            | "screen"
            | "python"
            | "python3"
            | "ipython"
            | "node"
            | "irb"
            | "ghci"
            | "psql"
            | "mysql"
            | "sqlite3"
    )
}

/// Check whether a process name belongs to a known agent CLI.
fn is_agent_process(name: &str) -> bool {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or(name);
    matches!(
        basename,
        "claude" | "claude-code" | "codex" | "codex-cli" | "gemini" | "gemini-cli"
    )
}
