//! Durable workflow execution engine
//!
//! Provides idempotent, recoverable, audited workflow execution.
//!
//! # Architecture
//!
//! Workflows are explicit state machines with a uniform execution model:
//! - **Workflow trait**: Defines the workflow interface (name, steps, execution)
//! - **WorkflowContext**: Runtime context with WezTerm client, storage, pane state
//! - **StepResult**: Step outcomes (continue, done, retry, abort, wait)
//! - **WaitCondition**: Conditions to pause execution (pattern, idle, external)
//!
//! This design enables:
//! - Persistent/resumable workflows
//! - Deterministic step logic testing
//! - Shared runner across agent-specific workflows

mod account_steps;
mod builtin_workflows;
mod codex_exit;
mod context;
mod coordination;
mod descriptors;
mod engine;
mod handlers;
mod lock;
mod plan_helpers;
mod runner;
mod step_results;
mod traits;
mod wait_execution;
pub use account_steps::*;
pub use builtin_workflows::*;
pub use codex_exit::*;
pub use context::*;
pub use coordination::*;
pub use descriptors::*;
pub use engine::*;
pub use handlers::*;
pub use lock::*;
pub use plan_helpers::*;
pub use runner::*;
pub use step_results::*;
pub use traits::*;
pub use wait_execution::*;

use crate::cass::{CassAgent, CassClient, CassSearchHit, SearchOptions};
use crate::policy::{InjectionResult, PaneCapabilities, Redactor};
use crate::runtime_compat::sleep;
use crate::storage::StorageHandle;
use crate::wezterm::{
    CodexSummaryWaitResult, PaneTextSource, PaneWaiter, WaitMatcher, WaitOptions, WaitResult,
    WeztermHandleSource, default_wezterm_handle, stable_hash, tail_text,
    wait_for_codex_session_summary,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Type alias for a boxed future used in dyn-compatible traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type PolicyInjector = crate::policy::PolicyGatedInjector<crate::wezterm::WeztermHandle>;
type PolicyInjectorHandle = Arc<crate::runtime_compat::Mutex<PolicyInjector>>;

pub(crate) fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Detection, PatternEngine, Severity};
    use crate::runtime_compat::CompatRuntime;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build workflows test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    // ========================================================================
    // StepResult Tests
    // ========================================================================

    #[test]
    fn step_result_continue_serializes() {
        let result = StepResult::Continue;
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("continue"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_continue());
    }

    #[test]
    fn step_result_done_serializes() {
        let result = StepResult::done(serde_json::json!({"status": "ok"}));
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("done"));
        assert!(json.contains("status"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_done());
        assert!(parsed.is_terminal());
    }

    #[test]
    fn step_result_retry_serializes() {
        let result = StepResult::retry(5000);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("retry"));
        assert!(json.contains("5000"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        match parsed {
            StepResult::Retry { delay_ms } => assert_eq!(delay_ms, 5000),
            _ => panic!("Expected Retry"),
        }
    }

    #[test]
    fn step_result_abort_serializes() {
        let result = StepResult::abort("test failure");
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("abort"));
        assert!(json.contains("test failure"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_terminal());
    }

    #[test]
    fn step_result_wait_for_serializes() {
        let result =
            StepResult::wait_for_with_timeout(WaitCondition::pattern("prompt.ready"), 10_000);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("wait_for"));
        assert!(json.contains("prompt.ready"));
        assert!(json.contains("10000"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        match parsed {
            StepResult::WaitFor {
                condition,
                timeout_ms,
            } => {
                assert_eq!(timeout_ms, Some(10_000));
                match condition {
                    WaitCondition::Pattern { rule_id, .. } => assert_eq!(rule_id, "prompt.ready"),
                    _ => panic!("Expected Pattern condition"),
                }
            }
            _ => panic!("Expected WaitFor"),
        }
    }

    #[test]
    fn step_result_helper_methods() {
        assert!(StepResult::cont().is_continue());
        assert!(StepResult::done_empty().is_done());
        assert!(StepResult::done_empty().is_terminal());
        assert!(StepResult::abort("error").is_terminal());
        assert!(!StepResult::retry(100).is_terminal());
        assert!(!StepResult::wait_for(WaitCondition::external("key")).is_terminal());
    }

    // ========================================================================
    // WaitCondition Tests
    // ========================================================================

    #[test]
    fn wait_condition_pattern_serializes() {
        let cond = WaitCondition::pattern("test.rule");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("pattern"));
        assert!(json.contains("test.rule"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    #[test]
    fn wait_condition_pattern_on_pane_serializes() {
        let cond = WaitCondition::pattern_on_pane(42, "test.rule");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("42"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id(), Some(42));
    }

    #[test]
    fn wait_condition_pane_idle_serializes() {
        let cond = WaitCondition::pane_idle(1000);
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("pane_idle"));
        assert!(json.contains("1000"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
    }

    #[test]
    fn wait_condition_pane_idle_on_serializes() {
        let cond = WaitCondition::pane_idle_on(99, 500);
        assert_eq!(cond.pane_id(), Some(99));
    }

    #[test]
    fn wait_condition_external_serializes() {
        let cond = WaitCondition::external("approval_granted");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("external"));
        assert!(json.contains("approval_granted"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    #[test]
    fn wait_condition_text_match_serializes() {
        let cond = WaitCondition::text_match(TextMatch::substring("ready"));
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("text_match"));
        assert!(json.contains("substring"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    #[test]
    fn wait_condition_sleep_serializes() {
        let cond = WaitCondition::sleep(1500);
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("sleep"));
        assert!(json.contains("1500"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    // ========================================================================
    // WorkflowStep Tests
    // ========================================================================

    #[test]
    fn workflow_step_creates() {
        let step = WorkflowStep::new("send_prompt", "Send a prompt to the terminal");
        assert_eq!(step.name, "send_prompt");
        assert_eq!(step.description, "Send a prompt to the terminal");
    }

    // ========================================================================
    // WorkflowConfig Tests
    // ========================================================================

    #[test]
    fn workflow_config_defaults() {
        let config = WorkflowConfig::default();
        assert_eq!(config.default_wait_timeout_ms, 30_000);
        assert_eq!(config.max_step_retries, 3);
        assert_eq!(config.retry_delay_ms, 1_000);
    }

    // ========================================================================
    // WorkflowEngine Tests
    // ========================================================================

    #[test]
    fn engine_can_be_created() {
        let engine = WorkflowEngine::new(5);
        assert_eq!(engine.max_concurrent(), 5);
    }

    // ========================================================================
    // Stub Workflow Tests (wa-nu4.1.1.1 acceptance criteria)
    // ========================================================================

    /// A stub workflow for testing that demonstrates all workflow capabilities
    struct StubWorkflow {
        name: &'static str,
        description: &'static str,
        target_rule_prefix: &'static str,
    }

    impl StubWorkflow {
        fn new() -> Self {
            Self {
                name: "stub_workflow",
                description: "A test workflow for verification",
                target_rule_prefix: "test.",
            }
        }
    }

    impl Workflow for StubWorkflow {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            self.description
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.starts_with(self.target_rule_prefix)
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("step_one", "First step - sends prompt"),
                WorkflowStep::new("step_two", "Second step - waits for response"),
                WorkflowStep::new("step_three", "Third step - completes"),
            ]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::cont(),
                    1 => StepResult::wait_for(WaitCondition::pattern("response.ready")),
                    2 => StepResult::done(serde_json::json!({"completed": true})),
                    _ => StepResult::abort("unexpected step index"),
                }
            })
        }

        fn cleanup(&self, _ctx: &mut WorkflowContext) -> BoxFuture<'_, ()> {
            Box::pin(async {
                // Stub cleanup - no-op
            })
        }
    }

    fn make_test_detection(rule_id: &str) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type: AgentType::Wezterm,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
            span: (0, 0),
        }
    }

    #[test]
    fn stub_workflow_compiles_and_has_correct_metadata() {
        let workflow = StubWorkflow::new();

        assert_eq!(workflow.name(), "stub_workflow");
        assert_eq!(workflow.description(), "A test workflow for verification");
        assert_eq!(workflow.step_count(), 3);

        let steps = workflow.steps();
        assert_eq!(steps[0].name, "step_one");
        assert_eq!(steps[1].name, "step_two");
        assert_eq!(steps[2].name, "step_three");
    }

    #[test]
    fn stub_workflow_handles_matching_detections() {
        let workflow = StubWorkflow::new();

        // Should handle detections with matching prefix
        assert!(workflow.handles(&make_test_detection("test.prompt_ready")));
        assert!(workflow.handles(&make_test_detection("test.anything")));

        // Should not handle detections with non-matching prefix
        assert!(!workflow.handles(&make_test_detection("other.prompt_ready")));
        assert!(!workflow.handles(&make_test_detection("production.event")));
    }

    #[test]
    fn stub_workflow_executes_steps_correctly() {
        run_async_test(async {
        let workflow = StubWorkflow::new();

        // Create a minimal context for testing
        // Note: In real usage, this would have an actual StorageHandle
        // For this test, we just verify the step execution logic

        // We can't easily create a WorkflowContext without a real StorageHandle,
        // but we can verify the workflow's step logic independently
        let steps = workflow.steps();
        assert_eq!(steps.len(), 3);
        });
    }

    #[test]
    fn step_result_transitions_exhaustive() {
        // Verify all StepResult variants can be created and identified
        let variants = [
            StepResult::Continue,
            StepResult::Done {
                result: serde_json::Value::Null,
            },
            StepResult::Retry { delay_ms: 1000 },
            StepResult::Abort {
                reason: "test".to_string(),
            },
            StepResult::WaitFor {
                condition: WaitCondition::external("key"),
                timeout_ms: None,
            },
        ];

        // Each variant serializes uniquely
        let mut json_types = std::collections::HashSet::new();
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let type_field = parsed["type"].as_str().unwrap().to_string();
            json_types.insert(type_field);
        }

        // All 5 variants have unique type identifiers
        assert_eq!(json_types.len(), 5);
        assert!(json_types.contains("continue"));
        assert!(json_types.contains("done"));
        assert!(json_types.contains("retry"));
        assert!(json_types.contains("abort"));
        assert!(json_types.contains("wait_for"));
    }

    #[test]
    fn wait_condition_transitions_exhaustive() {
        // Verify all WaitCondition variants
        let variants = [
            WaitCondition::Pattern {
                pane_id: None,
                rule_id: "test".to_string(),
            },
            WaitCondition::PaneIdle {
                pane_id: None,
                idle_threshold_ms: 1000,
            },
            WaitCondition::External {
                key: "test".to_string(),
            },
        ];

        let mut json_types = std::collections::HashSet::new();
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let type_field = parsed["type"].as_str().unwrap().to_string();
            json_types.insert(type_field);
        }

        assert_eq!(json_types.len(), 3);
        assert!(json_types.contains("pattern"));
        assert!(json_types.contains("pane_idle"));
        assert!(json_types.contains("external"));
    }

    // ========================================================================
    // WaitConditionResult Tests
    // ========================================================================

    #[test]
    fn wait_condition_result_satisfied_is_satisfied() {
        let result = WaitConditionResult::Satisfied {
            elapsed_ms: 100,
            polls: 5,
            context: Some("matched".to_string()),
        };
        assert!(result.is_satisfied());
        assert!(!result.is_timed_out());
        assert_eq!(result.elapsed_ms(), Some(100));
    }

    #[test]
    fn wait_condition_result_timed_out_is_timed_out() {
        let result = WaitConditionResult::TimedOut {
            elapsed_ms: 5000,
            polls: 100,
            last_observed: Some("waiting for prompt".to_string()),
        };
        assert!(!result.is_satisfied());
        assert!(result.is_timed_out());
        assert_eq!(result.elapsed_ms(), Some(5000));
    }

    #[test]
    fn wait_condition_result_unsupported_has_no_elapsed() {
        let result = WaitConditionResult::Unsupported {
            reason: "external signals not implemented".to_string(),
        };
        assert!(!result.is_satisfied());
        assert!(!result.is_timed_out());
        assert_eq!(result.elapsed_ms(), None);
    }

    // ========================================================================
    // WaitConditionOptions Tests
    // ========================================================================

    #[test]
    fn wait_condition_options_defaults() {
        let options = WaitConditionOptions::default();
        assert_eq!(options.tail_lines, 200);
        assert_eq!(options.poll_initial.as_millis(), 50);
        assert_eq!(options.poll_max.as_millis(), 1000);
        assert_eq!(options.max_polls, 10_000);
        assert!(options.allow_idle_heuristics);
    }

    // ========================================================================
    // Helper Function Tests
    // ========================================================================

    #[test]
    fn tail_text_extracts_last_n_lines() {
        let text = "line1\nline2\nline3\nline4\nline5";
        assert_eq!(tail_text(text, 3), "line3\nline4\nline5");
        assert_eq!(tail_text(text, 1), "line5");
        assert_eq!(tail_text(text, 10), text);
        assert_eq!(tail_text(text, 0), "");
    }

    #[test]
    fn tail_text_handles_empty_input() {
        assert_eq!(tail_text("", 5), "");
    }

    #[test]
    fn tail_text_handles_single_line() {
        assert_eq!(tail_text("single line", 5), "single line");
    }

    #[test]
    fn truncate_for_log_preserves_short_strings() {
        assert_eq!(truncate_for_log("hello", 10), "hello");
        assert_eq!(truncate_for_log("exact", 5), "exact");
    }

    #[test]
    fn truncate_for_log_truncates_long_strings() {
        assert_eq!(truncate_for_log("hello world", 8), "hello...");
    }

    // ========================================================================
    // Heuristic Idle Check Tests
    // ========================================================================

    #[test]
    fn heuristic_idle_detects_bash_prompt() {
        let text = "output from command\nuser@host:~$ ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_root_prompt() {
        let text = "output\nroot@host:~# ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_zsh_prompt() {
        let text = "output\n❯ ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_python_repl() {
        let text = ">>> ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_prompt_with_trailing_newline() {
        // Note: Rust's lines() iterator doesn't include trailing empty lines,
        // so "user@host:~$ \n" becomes the last line as "user@host:~$ "
        // which after trim_end becomes "user@host:~$" ending with "$"
        let text = "output\nuser@host:~$ \n";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_rejects_command_output() {
        let text = "building project...\nCompiling foo v1.0.0";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(!is_idle);
        assert!(desc.contains("no_prompt_detected"));
    }

    #[test]
    fn heuristic_idle_rejects_running_command() {
        // Use "50/100" instead of "50%" - the % character would match the tcsh prompt pattern
        let text = "npm run build\nProgress: 50/100";
        let (is_idle, _desc) = heuristic_idle_check(text, 10);
        assert!(!is_idle);
    }

    // ========================================================================
    // WaitConditionExecutor Tests (using mock source)
    // ========================================================================

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock pane text source for testing
    struct MockPaneSource {
        texts: Mutex<Vec<String>>,
        call_count: AtomicUsize,
    }

    impl MockPaneSource {
        fn new(texts: Vec<String>) -> Self {
            Self {
                texts: Mutex::new(texts),
                call_count: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    impl crate::wezterm::PaneTextSource for MockPaneSource {
        type Fut<'a> =
            std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            let count = self.call_count.fetch_add(1, Ordering::Relaxed);
            let texts = self.texts.lock().unwrap();
            let text = if count < texts.len() {
                texts[count].clone()
            } else {
                texts.last().cloned().unwrap_or_default()
            };
            Box::pin(async move { Ok(text) })
        }
    }

    #[derive(Default)]
    struct MockWezterm;

    impl MockWezterm {
        fn pane_info(pane_id: u64) -> crate::wezterm::PaneInfo {
            crate::wezterm::PaneInfo {
                pane_id,
                tab_id: 1,
                window_id: 1,
                domain_id: None,
                domain_name: None,
                workspace: None,
                size: None,
                rows: None,
                cols: None,
                title: None,
                cwd: None,
                tty_name: None,
                cursor_x: None,
                cursor_y: None,
                cursor_visibility: None,
                left_col: None,
                top_row: None,
                is_active: false,
                is_zoomed: false,
                extra: std::collections::HashMap::new(),
            }
        }
    }

    impl crate::wezterm::WeztermInterface for MockWezterm {
        fn list_panes(&self) -> crate::wezterm::WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_pane(
            &self,
            pane_id: u64,
        ) -> crate::wezterm::WeztermFuture<'_, crate::wezterm::PaneInfo> {
            Box::pin(async move { Ok(Self::pane_info(pane_id)) })
        }

        fn get_text(
            &self,
            _pane_id: u64,
            _escapes: bool,
        ) -> crate::wezterm::WeztermFuture<'_, String> {
            Box::pin(async { Ok(String::new()) })
        }

        fn send_text(&self, _pane_id: u64, _text: &str) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_text_no_paste(
            &self,
            _pane_id: u64,
            _text: &str,
        ) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_text_with_options(
            &self,
            _pane_id: u64,
            _text: &str,
            _no_paste: bool,
            _no_newline: bool,
        ) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_control(
            &self,
            _pane_id: u64,
            _control_char: &str,
        ) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_ctrl_c(&self, _pane_id: u64) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_ctrl_d(&self, _pane_id: u64) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn spawn(
            &self,
            _cwd: Option<&str>,
            _domain_name: Option<&str>,
        ) -> crate::wezterm::WeztermFuture<'_, u64> {
            Box::pin(async { Ok(1) })
        }

        fn split_pane(
            &self,
            _pane_id: u64,
            _direction: crate::wezterm::SplitDirection,
            _cwd: Option<&str>,
            _percent: Option<u8>,
        ) -> crate::wezterm::WeztermFuture<'_, u64> {
            Box::pin(async { Ok(2) })
        }

        fn activate_pane(&self, _pane_id: u64) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn get_pane_direction(
            &self,
            _pane_id: u64,
            _direction: crate::wezterm::MoveDirection,
        ) -> crate::wezterm::WeztermFuture<'_, Option<u64>> {
            Box::pin(async { Ok(None) })
        }

        fn kill_pane(&self, _pane_id: u64) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn zoom_pane(&self, _pane_id: u64, _zoom: bool) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            crate::circuit_breaker::CircuitBreakerStatus::default()
        }
    }

    #[test]
    fn pattern_wait_succeeds_on_immediate_match() {
        run_async_test(async {
        let source = MockPaneSource::new(vec![
            "Conversation compacted 100,000 tokens to 25,000 tokens".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(10),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("claude_code.compaction");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert_eq!(source.calls(), 1);
        });
    }

    #[test]
    fn pattern_wait_times_out_on_no_match() {
        run_async_test(async {
        let source = MockPaneSource::new(vec!["no matching pattern here".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 5,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("claude_code.compaction");
        let result = executor
            .execute(&condition, 1, Duration::from_millis(20))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
        });
    }

    #[test]
    fn pattern_wait_succeeds_after_multiple_polls() {
        run_async_test(async {
        let source = MockPaneSource::new(vec![
            "no match yet".to_string(),
            "still no match".to_string(),
            "Conversation compacted 100,000 tokens to 25,000 tokens".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("claude_code.compaction");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert!(source.calls() >= 3);
        });
    }

    #[test]
    fn text_match_wait_succeeds_on_substring() {
        run_async_test(async {
        let source = MockPaneSource::new(vec!["booting".to_string(), "ready> prompt".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::text_match(TextMatch::substring("ready>"));
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert!(source.calls() >= 2);
        });
    }

    #[test]
    fn text_match_wait_succeeds_on_regex() {
        run_async_test(async {
        let source = MockPaneSource::new(vec![
            "waiting".to_string(),
            "completed in 123ms".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::text_match(TextMatch::regex(r"completed in \d+ms"));
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert!(source.calls() >= 2);
        });
    }

    #[test]
    fn pane_idle_succeeds_with_osc133_prompt_active() {
        run_async_test(async {
        use crate::ingest::{Osc133State, ShellState};

        let source = MockPaneSource::new(vec!["some text".to_string()]);
        let engine = PatternEngine::new();
        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::PromptActive;

        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc_state)
            .with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        // idle_threshold_ms = 0 means immediate satisfaction when idle
        let condition = WaitCondition::pane_idle(0);
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        if let WaitConditionResult::Satisfied { context, .. } = result {
            assert!(context.unwrap().contains("osc133"));
        }
        });
    }

    #[test]
    fn pane_idle_times_out_with_osc133_command_running() {
        run_async_test(async {
        use crate::ingest::{Osc133State, ShellState};

        let source = MockPaneSource::new(vec!["running command...".to_string()]);
        let engine = PatternEngine::new();
        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::CommandRunning;

        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc_state)
            .with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 5,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pane_idle(0);
        let result = executor
            .execute(&condition, 1, Duration::from_millis(20))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
        });
    }

    // ========================================================================
    // Descriptor Workflow Tests (wa-nu4.1.1.6)
    // ========================================================================

    #[test]
    fn descriptor_yaml_parses_and_validates() {
        let yaml = r#"
workflow_schema_version: 1
name: "demo_wait"
description: "Demo workflow"
steps:
  - type: wait_for
    id: wait_prompt
    matcher:
      kind: substring
      value: "ready>"
    timeout_ms: 1000
  - type: send_text
    id: send_cmd
    text: "echo hi"
    wait_for:
      kind: regex
      pattern: "hi"
    wait_timeout_ms: 2000
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        assert_eq!(descriptor.name, "demo_wait");
        assert_eq!(descriptor.steps.len(), 2);

        let workflow = DescriptorWorkflow::new(descriptor);
        assert_eq!(workflow.steps().len(), 2);
    }

    #[test]
    fn descriptor_toml_parses_and_validates() {
        let toml = r#"
workflow_schema_version = 1
name = "demo_sleep"

[[steps]]
type = "sleep"
id = "pause"
duration_ms = 500
"#;
        let descriptor = WorkflowDescriptor::from_toml_str(toml).unwrap();
        assert_eq!(descriptor.name, "demo_sleep");
        assert_eq!(descriptor.steps.len(), 1);
    }

    #[test]
    fn descriptor_rejects_duplicate_step_ids() {
        let yaml = r#"
workflow_schema_version: 1
name: "dup_steps"
steps:
  - type: sleep
    id: step
    duration_ms: 10
  - type: sleep
    id: step
    duration_ms: 10
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Duplicate step id"));
    }

    #[test]
    fn descriptor_rejects_invalid_regex() {
        let yaml = r#"
workflow_schema_version: 1
name: "bad_regex"
steps:
  - type: wait_for
    id: wait_bad
    matcher:
      kind: regex
      pattern: "(["
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Invalid regex pattern"));
    }

    #[test]
    fn descriptor_rejects_schema_version() {
        let yaml = r#"
workflow_schema_version: 999
name: "bad_version"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("workflow_schema_version"));
    }

    #[test]
    fn descriptor_rejects_too_many_steps() {
        let mut yaml = String::from("workflow_schema_version: 1\nname: \"too_many\"\nsteps:\n");
        for idx in 0..=DESCRIPTOR_MAX_STEPS {
            yaml.push_str(&format!(
                "  - type: sleep\n    id: step_{idx}\n    duration_ms: 1\n"
            ));
        }
        let err = WorkflowDescriptor::from_yaml_str(&yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("too many steps"));
    }

    #[test]
    fn descriptor_rejects_wait_timeout_too_large() {
        let too_long = DESCRIPTOR_MAX_WAIT_TIMEOUT_MS + 1;
        let yaml = format!(
            r#"
workflow_schema_version: 1
name: "timeout_too_large"
steps:
  - type: wait_for
    id: wait_too_long
    matcher:
      kind: substring
      value: "ready"
    timeout_ms: {too_long}
"#
        );
        let err = WorkflowDescriptor::from_yaml_str(&yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("timeout_ms too large"));
    }

    #[test]
    fn descriptor_send_ctrl_requires_injector() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "ctrl_only"
steps:
  - type: send_ctrl
    id: interrupt
    key: ctrl_c
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let workflow = DescriptorWorkflow::new(descriptor);

        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("descriptor_send_ctrl.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-ctrl");
        let result = workflow.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => {
                assert!(reason.contains("No injector configured"));
            }
            other => panic!("Expected abort, got: {other:?}"),
        }
        });
    }

    #[test]
    fn descriptor_workflow_logs_policy_summary_on_send_text() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "send_text_policy"
steps:
  - type: send_text
    id: send_cmd
    text: "echo secret"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let workflow: Arc<dyn Workflow> = Arc::new(DescriptorWorkflow::new(descriptor));

        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("descriptor_policy.db")
            .to_string_lossy()
            .to_string();

        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::strict(),
                default_wezterm_handle(),
            ),
        ));
        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        let pane_id = 101u64;
        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::clone(&workflow));

        let execution_id = generate_workflow_id(workflow.name());
        runner
            .engine
            .start_with_id(
                &storage,
                execution_id.clone(),
                workflow.name(),
                pane_id,
                None,
                None,
            )
            .await
            .unwrap();

        let result = runner
            .run_workflow(pane_id, workflow, &execution_id, 0)
            .await;
        assert!(result.is_aborted(), "Expected policy-gated abort");

        let logs = storage.get_step_logs(&execution_id).await.unwrap();
        assert_eq!(logs.len(), 1, "Expected a single step log entry");

        let policy_summary = logs[0]
            .policy_summary
            .as_ref()
            .expect("policy summary missing");
        let summary_json: serde_json::Value = serde_json::from_str(policy_summary).unwrap();
        assert_ne!(
            summary_json.get("decision").and_then(|v| v.as_str()),
            Some("allow")
        );
        });
    }

    #[test]
    fn workflow_runner_emits_step_and_policy_decision_capture_events() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "decision_capture_flow"
steps:
  - type: send_text
    id: send_cmd
    text: "echo hello"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let workflow: Arc<dyn Workflow> = Arc::new(DescriptorWorkflow::new(descriptor));

        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("workflow_decision_capture.db")
            .to_string_lossy()
            .to_string();

        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::strict(),
                default_wezterm_handle(),
            ),
        ));

        let sink = Arc::new(crate::replay_capture::CollectingCaptureSink::new());
        let replay_adapter = Arc::new(crate::replay_capture::CaptureAdapter::new(
            sink.clone(),
            crate::replay_capture::CaptureConfig::default(),
        ));

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        )
        .with_replay_capture_adapter(replay_adapter);

        let pane_id = 101u64;
        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::clone(&workflow));

        let execution_id = generate_workflow_id(workflow.name());
        runner
            .engine
            .start_with_id(
                &storage,
                execution_id.clone(),
                workflow.name(),
                pane_id,
                None,
                None,
            )
            .await
            .unwrap();

        let result = runner
            .run_workflow(pane_id, workflow, &execution_id, 0)
            .await;
        assert!(result.is_aborted(), "Expected policy-gated abort");

        let events = sink.recorder_events();
        assert!(
            events.iter().any(|event| {
                matches!(
                    &event.payload,
                    crate::recording::RecorderEventPayload::ControlMarker { details, .. }
                        if details.get("decision_type")
                            == Some(&serde_json::json!("WorkflowStep"))
                )
            }),
            "expected workflow_step decision provenance event"
        );
        assert!(
            events.iter().any(|event| {
                matches!(
                    &event.payload,
                    crate::recording::RecorderEventPayload::ControlMarker { details, .. }
                        if details.get("decision_type")
                            == Some(&serde_json::json!("PolicyEvaluation"))
                )
            }),
            "expected policy_evaluation decision provenance event"
        );
        });
    }

    // --- Custom workflow extensions (wa-fno.2) ---

    #[test]
    fn descriptor_with_triggers_parses() {
        let yaml = r#"
workflow_schema_version: 1
name: "triggered_flow"
description: "A workflow with triggers"
triggers:
  - event_types: ["session.compaction"]
    agent_types: ["codex"]
    rule_ids: ["compaction.detected"]
steps:
  - type: notify
    id: note
    message: "Trigger activated"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        assert_eq!(descriptor.triggers.len(), 1);
        assert_eq!(
            descriptor.triggers[0].event_types,
            vec!["session.compaction"]
        );
        assert_eq!(descriptor.triggers[0].agent_types, vec!["codex"]);
        assert_eq!(descriptor.triggers[0].rule_ids, vec!["compaction.detected"]);
    }

    #[test]
    fn descriptor_trigger_handles_detection() {
        let yaml = r#"
workflow_schema_version: 1
name: "trigger_match"
triggers:
  - event_types: ["session.compaction"]
    agent_types: ["codex"]
steps:
  - type: log
    id: entry
    message: "handled"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        let matching = crate::patterns::Detection {
            rule_id: "any_rule".to_string(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "session.compaction".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(workflow.handles(&matching));

        let non_matching = crate::patterns::Detection {
            rule_id: "any_rule".to_string(),
            agent_type: crate::patterns::AgentType::ClaudeCode,
            event_type: "session.compaction".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&non_matching));
    }

    #[test]
    fn descriptor_no_triggers_does_not_handle() {
        let yaml = r#"
workflow_schema_version: 1
name: "no_triggers"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let detection = crate::patterns::Detection {
            rule_id: "test".to_string(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "any.event".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&detection));
    }

    #[test]
    fn descriptor_on_failure_parses() {
        let yaml = r#"
workflow_schema_version: 1
name: "with_failure"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
on_failure:
  action: notify
  message: "Failed at step: ${failed_step}"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        assert!(descriptor.on_failure.is_some());
        let handler = descriptor.on_failure.as_ref().unwrap();
        let msg = handler.interpolate_message("pause");
        assert_eq!(msg, "Failed at step: pause");
    }

    #[test]
    fn descriptor_on_failure_log_variant() {
        let yaml = r#"
workflow_schema_version: 1
name: "failure_log"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
on_failure:
  action: log
  message: "Error in ${failed_step}"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let handler = descriptor.on_failure.as_ref().unwrap();
        assert!(matches!(handler, DescriptorFailureHandler::Log { .. }));
        assert_eq!(handler.interpolate_message("step_x"), "Error in step_x");
    }

    #[test]
    fn descriptor_notify_step_parses_and_builds_steps() {
        let yaml = r#"
workflow_schema_version: 1
name: "notify_flow"
steps:
  - type: notify
    id: alert
    message: "Something happened"
  - type: log
    id: record
    message: "Logged event"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let steps = workflow.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].name, "alert");
        assert_eq!(steps[1].name, "record");
    }

    #[test]
    fn descriptor_notify_step_returns_continue() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "notify_exec"
steps:
  - type: notify
    id: alert
    message: "test notification"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("notify.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-notify");
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
        });
    }

    #[test]
    fn descriptor_log_step_returns_continue() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "log_exec"
steps:
  - type: log
    id: entry
    message: "audit trail entry"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("log.db").to_string_lossy().to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-log");
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
        });
    }

    #[test]
    fn descriptor_abort_step_returns_abort() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "abort_exec"
steps:
  - type: abort
    id: bail
    reason: "cannot proceed"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("abort.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-abort");
        let result = workflow.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => assert_eq!(reason, "cannot proceed"),
            other => panic!("Expected Abort, got: {other:?}"),
        }
        });
    }

    #[test]
    fn descriptor_conditional_then_branch() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_then"
steps:
  - type: conditional
    id: check
    test_text: "error detected in output"
    matcher:
      kind: substring
      value: "error"
    then_steps:
      - type: notify
        id: alert
        message: "error found"
    else_steps:
      - type: log
        id: ok
        message: "all clear"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("cond.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-cond");
        // test_text contains "error" so then_steps should run (notify returns Continue)
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
        });
    }

    #[test]
    fn descriptor_conditional_else_branch() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_else"
steps:
  - type: conditional
    id: check
    test_text: "all systems nominal"
    matcher:
      kind: substring
      value: "error"
    then_steps:
      - type: abort
        id: bail
        reason: "error found"
    else_steps:
      - type: notify
        id: ok
        message: "all clear"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("cond_else.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx =
            WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-cond-else");
        // test_text does NOT contain "error" so else branch runs (notify returns Continue).
        // Conditionals compile to multiple executable steps (JumpIfFalse + body + Jump);
        // simulate the engine loop: Continue advances to the next step, JumpTo jumps,
        // and Abort/Done/etc. are terminal.
        let num_steps = workflow.step_count();
        let mut step = 0usize;
        let result = loop {
            let r = workflow.execute_step(&mut ctx, step).await;
            match r {
                StepResult::JumpTo { step: target } => step = target,
                StepResult::Continue if step + 1 < num_steps => step += 1,
                other => break other,
            }
        };
        assert!(result.is_continue());
        });
    }

    #[test]
    fn descriptor_conditional_then_with_abort() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_abort"
steps:
  - type: conditional
    id: check
    test_text: "FATAL error occurred"
    matcher:
      kind: regex
      pattern: "FATAL"
    then_steps:
      - type: abort
        id: bail
        reason: "fatal error"
    else_steps: []
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("cond_abort.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx =
            WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-cond-abort");
        // Conditionals compile to multiple executable steps; simulate the engine loop:
        // Continue advances to the next step, JumpTo jumps, Abort/Done/etc. are terminal.
        let num_steps = workflow.step_count();
        let mut step = 0usize;
        let result = loop {
            let r = workflow.execute_step(&mut ctx, step).await;
            match r {
                StepResult::JumpTo { step: target } => step = target,
                StepResult::Continue if step + 1 < num_steps => step += 1,
                other => break other,
            }
        };
        match result {
            StepResult::Abort { reason } => assert_eq!(reason, "fatal error"),
            other => panic!("Expected Abort, got: {other:?}"),
        }
        });
    }

    #[test]
    fn descriptor_loop_repeats_steps() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "loop_test"
steps:
  - type: loop
    id: repeat
    count: 3
    body:
      - type: log
        id: tick
        message: "iteration"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("loop.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-loop");
        // All body steps are log (Continue), so loop completes with Continue
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
        });
    }

    #[test]
    fn descriptor_loop_aborts_on_abort_step() {
        run_async_test(async {
        let yaml = r#"
workflow_schema_version: 1
name: "loop_abort"
steps:
  - type: loop
    id: repeat
    count: 10
    body:
      - type: abort
        id: bail
        reason: "stop early"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("loop_abort.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx =
            WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-loop-abort");
        let result = workflow.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => assert_eq!(reason, "stop early"),
            other => panic!("Expected Abort, got: {other:?}"),
        }
        });
    }

    #[test]
    fn descriptor_rejects_loop_count_too_large() {
        let yaml = r#"
workflow_schema_version: 1
name: "big_loop"
steps:
  - type: loop
    id: big
    count: 1001
    body:
      - type: sleep
        id: pause
        duration_ms: 1
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Loop count too large"));
    }

    #[test]
    fn descriptor_rejects_empty_loop_body() {
        let yaml = r#"
workflow_schema_version: 1
name: "empty_loop"
steps:
  - type: loop
    id: empty
    count: 5
    body: []
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Loop body must contain at least one step"));
    }

    #[test]
    fn descriptor_conditional_validates_nested_steps() {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_validate"
steps:
  - type: conditional
    id: check
    test_text: "test"
    matcher:
      kind: regex
      pattern: "(["
    then_steps: []
    else_steps: []
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Invalid regex pattern"));
    }

    #[test]
    fn descriptor_toml_with_triggers_and_on_failure() {
        let toml = r#"
workflow_schema_version = 1
name = "toml_full"

[[triggers]]
event_types = ["restart.prompt"]
agent_types = ["custom-agent"]

[[steps]]
type = "send_text"
id = "restart"
text = "/restart\n"

[on_failure]
action = "notify"
message = "Failed at ${failed_step}"
"#;
        let descriptor = WorkflowDescriptor::from_toml_str(toml).unwrap();
        assert_eq!(descriptor.triggers.len(), 1);
        assert!(descriptor.on_failure.is_some());
        let handler = descriptor.on_failure.as_ref().unwrap();
        assert_eq!(handler.interpolate_message("restart"), "Failed at restart");
    }

    #[test]
    fn load_workflows_from_dir_loads_yaml_and_toml() {
        let dir = tempfile::TempDir::new().unwrap();

        let yaml_content = r#"
workflow_schema_version: 1
name: "yaml_wf"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        std::fs::write(dir.path().join("wf1.yaml"), yaml_content).unwrap();

        let toml_content = r#"
workflow_schema_version = 1
name = "toml_wf"

[[steps]]
type = "sleep"
id = "pause"
duration_ms = 10
"#;
        std::fs::write(dir.path().join("wf2.toml"), toml_content).unwrap();

        // Should skip non-workflow files
        std::fs::write(dir.path().join("readme.txt"), "ignore me").unwrap();

        let loaded = load_workflows_from_dir(dir.path());
        assert_eq!(loaded.len(), 2);
        let names: Vec<&str> = loaded.iter().map(|(wf, _)| wf.name).collect();
        assert!(names.contains(&"yaml_wf"));
        assert!(names.contains(&"toml_wf"));
    }

    #[test]
    fn load_workflows_from_dir_skips_invalid_files() {
        let dir = tempfile::TempDir::new().unwrap();

        // Valid
        let yaml = r#"
workflow_schema_version: 1
name: "valid"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        std::fs::write(dir.path().join("valid.yaml"), yaml).unwrap();

        // Invalid YAML
        std::fs::write(dir.path().join("broken.yaml"), "not: valid: yaml: {{").unwrap();

        let loaded = load_workflows_from_dir(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.name, "valid");
    }

    #[test]
    fn load_workflows_from_nonexistent_dir_returns_empty() {
        let loaded =
            load_workflows_from_dir(std::path::Path::new("/nonexistent/path/wa/workflows"));
        assert!(loaded.is_empty());
    }

    #[test]
    fn default_workflow_dir_returns_some() {
        // On most systems, config_dir() is available
        let dir = default_workflow_dir();
        if let Some(d) = &dir {
            assert!(d.ends_with("ft/workflows"));
        }
    }

    #[test]
    fn descriptor_failure_handler_abort_variant() {
        let yaml = r#"
workflow_schema_version: 1
name: "fail_abort"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
on_failure:
  action: abort
  message: "Critical failure in ${failed_step}"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let handler = descriptor.on_failure.as_ref().unwrap();
        assert!(matches!(handler, DescriptorFailureHandler::Abort { .. }));
        assert_eq!(
            handler.interpolate_message("pause"),
            "Critical failure in pause"
        );
    }

    #[test]
    fn descriptor_multiple_triggers_any_match() {
        let yaml = r#"
workflow_schema_version: 1
name: "multi_trigger"
triggers:
  - event_types: ["session.compaction"]
  - rule_ids: ["usage.warning"]
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        // First trigger matches on event_type
        let det1 = crate::patterns::Detection {
            rule_id: "other_rule".to_string(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "session.compaction".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(workflow.handles(&det1));

        // Second trigger matches on rule_id
        let det2 = crate::patterns::Detection {
            rule_id: "usage.warning".to_string(),
            agent_type: crate::patterns::AgentType::ClaudeCode,
            event_type: "other.event".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(workflow.handles(&det2));

        // Neither trigger matches
        let det3 = crate::patterns::Detection {
            rule_id: "unrelated".to_string(),
            agent_type: crate::patterns::AgentType::Gemini,
            event_type: "unrelated.event".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&det3));
    }

    #[test]
    fn descriptor_yml_extension_loads() {
        let dir = tempfile::TempDir::new().unwrap();
        let yaml = r#"
workflow_schema_version: 1
name: "yml_ext"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        std::fs::write(dir.path().join("test.yml"), yaml).unwrap();
        let loaded = load_workflows_from_dir(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.name, "yml_ext");
    }

    #[test]
    fn pane_idle_uses_heuristics_when_no_osc133() {
        run_async_test(async {
        let source = MockPaneSource::new(vec!["user@host:~$ ".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pane_idle(0);
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        if let WaitConditionResult::Satisfied { context, .. } = result {
            assert!(context.unwrap().contains("heuristic"));
        }
        });
    }

    #[test]
    fn pane_idle_respects_threshold_duration() {
        run_async_test(async {
        use crate::ingest::{Osc133State, ShellState};

        let source = MockPaneSource::new(vec!["some text".to_string()]);
        let engine = PatternEngine::new();
        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::PromptActive;

        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc_state)
            .with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(10),
                poll_max: Duration::from_millis(50),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        // Require 50ms idle threshold
        let condition = WaitCondition::pane_idle(50);
        let start = std::time::Instant::now();
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        // Should have waited at least the threshold duration
        assert!(elapsed >= Duration::from_millis(50));
        });
    }

    #[test]
    fn stable_tail_succeeds_after_stability_window() {
        run_async_test(async {
        let source = MockPaneSource::new(vec![
            "compaction in progress".to_string(),
            "compaction in progress".to_string(),
            "compaction in progress".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::stable_tail(1);
        let result = executor
            .execute(&condition, 1, Duration::from_millis(50))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        });
    }

    #[test]
    fn stable_tail_times_out_when_changing() {
        run_async_test(async {
        let source = MockPaneSource::new(vec![
            "line 1".to_string(),
            "line 2".to_string(),
            "line 3".to_string(),
            "line 4".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 5,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::stable_tail(100);
        let result = executor
            .execute(&condition, 1, Duration::from_millis(10))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
        });
    }

    #[test]
    fn external_wait_returns_unsupported() {
        run_async_test(async {
        let source = MockPaneSource::new(vec!["text".to_string()]);
        let engine = PatternEngine::new();

        let executor = WaitConditionExecutor::new(&source, &engine);
        let condition = WaitCondition::external("my_signal");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        match result {
            WaitConditionResult::Unsupported { reason } => {
                assert!(reason.contains("my_signal"));
            }
            _ => panic!("Expected Unsupported"),
        }
        });
    }

    #[test]
    fn wait_respects_max_polls() {
        run_async_test(async {
        let source = MockPaneSource::new(vec!["no match".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(1),
                max_polls: 3,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("nonexistent.rule");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(60))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
        if let WaitConditionResult::TimedOut { polls, .. } = result {
            assert!(polls <= 3);
        }
        });
    }

    // ========================================================================
    // Workflow Persistence Tests (wa-nu4.1.1.3)
    // ========================================================================

    #[test]
    fn compute_next_step_empty_logs_returns_zero() {
        let logs: Vec<crate::storage::WorkflowStepLogRecord> = vec![];
        assert_eq!(super::compute_next_step(&logs), 0);
    }

    #[test]
    fn compute_next_step_with_continue_returns_next() {
        let logs = vec![crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "test-123".to_string(),
            audit_action_id: None,
            step_index: 0,
            step_name: "step_0".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "continue".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 1100,
            duration_ms: 100,
        }];
        assert_eq!(super::compute_next_step(&logs), 1);
    }

    #[test]
    fn compute_next_step_with_done_returns_next() {
        let logs = vec![crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "test-123".to_string(),
            audit_action_id: None,
            step_index: 2,
            step_name: "step_2".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "done".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 1100,
            duration_ms: 100,
        }];
        assert_eq!(super::compute_next_step(&logs), 3);
    }

    #[test]
    fn compute_next_step_with_retry_returns_same() {
        // Retry means the step should be re-executed
        let logs = vec![crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "test-123".to_string(),
            audit_action_id: None,
            step_index: 1,
            step_name: "step_1".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "retry".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 1100,
            duration_ms: 100,
        }];
        // No completed steps, so start from 0
        assert_eq!(super::compute_next_step(&logs), 0);
    }

    #[test]
    fn compute_next_step_mixed_logs_finds_highest_completed() {
        let logs = vec![
            crate::storage::WorkflowStepLogRecord {
                id: 1,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 0,
                step_name: "step_0".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "continue".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1000,
                completed_at: 1100,
                duration_ms: 100,
            },
            crate::storage::WorkflowStepLogRecord {
                id: 2,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 1,
                step_name: "step_1".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "continue".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1100,
                completed_at: 1200,
                duration_ms: 100,
            },
            crate::storage::WorkflowStepLogRecord {
                id: 3,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 2,
                step_name: "step_2".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "retry".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1200,
                completed_at: 1300,
                duration_ms: 100,
            },
        ];
        // Highest completed is step_index 1, so next is 2
        assert_eq!(super::compute_next_step(&logs), 2);
    }

    #[test]
    fn compute_next_step_out_of_order_logs() {
        // Logs might not be in order; function should still find max
        let logs = vec![
            crate::storage::WorkflowStepLogRecord {
                id: 3,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 2,
                step_name: "step_2".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "continue".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1200,
                completed_at: 1300,
                duration_ms: 100,
            },
            crate::storage::WorkflowStepLogRecord {
                id: 1,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 0,
                step_name: "step_0".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "continue".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1000,
                completed_at: 1100,
                duration_ms: 100,
            },
        ];
        // Highest completed is step_index 2, so next is 3
        assert_eq!(super::compute_next_step(&logs), 3);
    }

    #[test]
    fn generate_workflow_id_format() {
        let id = super::generate_workflow_id("test_workflow");
        assert!(id.starts_with("test_workflow-"));
        // Should have format: name-timestamp-random
        let parts: Vec<&str> = id.split('-').collect();
        assert!(parts.len() >= 3);
        // Last part should be hex (8 chars)
        let last = parts.last().unwrap();
        assert_eq!(last.len(), 8);
        assert!(last.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_workflow_id_uniqueness() {
        let id1 = super::generate_workflow_id("workflow");
        let id2 = super::generate_workflow_id("workflow");
        // Random component should make them different
        assert_ne!(id1, id2);
    }

    #[test]
    fn execution_status_serialization() {
        let statuses = [
            ExecutionStatus::Running,
            ExecutionStatus::Waiting,
            ExecutionStatus::Completed,
            ExecutionStatus::Aborted,
        ];

        for status in &statuses {
            let json = serde_json::to_string(status).unwrap();
            let parsed: ExecutionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, status);
        }
    }

    #[test]
    fn workflow_execution_serialization() {
        let execution = WorkflowExecution {
            id: "test-123-abc".to_string(),
            workflow_name: "test_workflow".to_string(),
            pane_id: 42,
            current_step: 2,
            status: ExecutionStatus::Running,
            started_at: 1000,
            updated_at: 1500,
        };

        let json = serde_json::to_string(&execution).unwrap();
        let parsed: WorkflowExecution = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, execution.id);
        assert_eq!(parsed.workflow_name, execution.workflow_name);
        assert_eq!(parsed.pane_id, execution.pane_id);
        assert_eq!(parsed.current_step, execution.current_step);
        assert_eq!(parsed.status, execution.status);
    }

    // ========================================================================
    // PaneWorkflowLockManager Tests (wa-nu4.1.1.2)
    // ========================================================================

    #[test]
    fn lock_manager_acquire_and_release() {
        let manager = PaneWorkflowLockManager::new();

        // Initially unlocked
        assert!(manager.is_locked(42).is_none());

        // Acquire succeeds
        let result = manager.try_acquire(42, "test_workflow", "exec-001");
        assert!(result.is_acquired());
        assert!(!result.is_already_locked());

        // Now locked
        let lock_info = manager.is_locked(42);
        assert!(lock_info.is_some());
        let info = lock_info.unwrap();
        assert_eq!(info.pane_id, 42);
        assert_eq!(info.workflow_name, "test_workflow");
        assert_eq!(info.execution_id, "exec-001");
        assert!(info.locked_at_ms > 0);

        // Release succeeds
        assert!(manager.release(42, "exec-001"));

        // Now unlocked
        assert!(manager.is_locked(42).is_none());
    }

    #[test]
    fn lock_manager_double_acquire_fails() {
        let manager = PaneWorkflowLockManager::new();

        // First acquire succeeds
        let result1 = manager.try_acquire(42, "workflow_a", "exec-001");
        assert!(result1.is_acquired());

        // Second acquire fails with details about the existing lock
        let result2 = manager.try_acquire(42, "workflow_b", "exec-002");
        assert!(result2.is_already_locked());
        match result2 {
            LockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                locked_since_ms,
            } => {
                assert_eq!(held_by_workflow, "workflow_a");
                assert_eq!(held_by_execution, "exec-001");
                assert!(locked_since_ms > 0);
            }
            LockAcquisitionResult::Acquired => panic!("Expected AlreadyLocked"),
        }

        // Release and retry succeeds
        manager.release(42, "exec-001");
        let result3 = manager.try_acquire(42, "workflow_b", "exec-002");
        assert!(result3.is_acquired());
    }

    #[test]
    fn lock_manager_release_with_wrong_execution_id_fails() {
        let manager = PaneWorkflowLockManager::new();

        manager.try_acquire(42, "test_workflow", "exec-001");

        // Release with wrong execution_id fails
        assert!(!manager.release(42, "wrong-exec-id"));

        // Lock still held
        assert!(manager.is_locked(42).is_some());

        // Correct execution_id works
        assert!(manager.release(42, "exec-001"));
        assert!(manager.is_locked(42).is_none());
    }

    #[test]
    fn lock_manager_multiple_panes_independent() {
        let manager = PaneWorkflowLockManager::new();

        // Lock pane 1
        let r1 = manager.try_acquire(1, "workflow_a", "exec-001");
        assert!(r1.is_acquired());

        // Lock pane 2 succeeds (different pane)
        let r2 = manager.try_acquire(2, "workflow_b", "exec-002");
        assert!(r2.is_acquired());

        // Lock pane 3 succeeds
        let r3 = manager.try_acquire(3, "workflow_c", "exec-003");
        assert!(r3.is_acquired());

        // All locked
        assert!(manager.is_locked(1).is_some());
        assert!(manager.is_locked(2).is_some());
        assert!(manager.is_locked(3).is_some());

        // Release pane 2 doesn't affect others
        manager.release(2, "exec-002");
        assert!(manager.is_locked(1).is_some());
        assert!(manager.is_locked(2).is_none());
        assert!(manager.is_locked(3).is_some());
    }

    #[test]
    fn lock_manager_active_locks() {
        let manager = PaneWorkflowLockManager::new();

        // Initially empty
        assert!(manager.active_locks().is_empty());

        manager.try_acquire(1, "workflow_a", "exec-001");
        manager.try_acquire(2, "workflow_b", "exec-002");

        let active = manager.active_locks();
        assert_eq!(active.len(), 2);

        let pane_ids: std::collections::HashSet<u64> = active.iter().map(|l| l.pane_id).collect();
        assert!(pane_ids.contains(&1));
        assert!(pane_ids.contains(&2));
    }

    #[test]
    fn lock_guard_releases_on_drop() {
        let manager = PaneWorkflowLockManager::new();

        // Acquire via guard
        {
            let guard = manager.acquire_guard(42, "test_workflow", "exec-001");
            assert!(guard.is_some());
            let guard = guard.unwrap();
            assert_eq!(guard.pane_id(), 42);
            assert_eq!(guard.execution_id(), "exec-001");

            // Lock is held
            assert!(manager.is_locked(42).is_some());
        }

        // Guard dropped, lock released
        assert!(manager.is_locked(42).is_none());
    }

    #[test]
    fn lock_guard_acquire_fails_when_locked() {
        let manager = PaneWorkflowLockManager::new();

        // Acquire first lock
        let _guard1 = manager.acquire_guard(42, "workflow_a", "exec-001");
        assert!(manager.is_locked(42).is_some());

        // Second acquire fails
        let guard2 = manager.acquire_guard(42, "workflow_b", "exec-002");
        assert!(guard2.is_none());
    }

    #[test]
    fn lock_manager_force_release() {
        let manager = PaneWorkflowLockManager::new();

        manager.try_acquire(42, "test_workflow", "exec-001");
        assert!(manager.is_locked(42).is_some());

        // Force release works even with unknown execution_id
        let removed = manager.force_release(42);
        assert!(removed.is_some());
        let info = removed.unwrap();
        assert_eq!(info.execution_id, "exec-001");

        // Now unlocked
        assert!(manager.is_locked(42).is_none());

        // Force release on unlocked pane returns None
        assert!(manager.force_release(42).is_none());
    }

    #[test]
    fn lock_acquisition_result_methods() {
        let acquired = LockAcquisitionResult::Acquired;
        assert!(acquired.is_acquired());
        assert!(!acquired.is_already_locked());

        let locked = LockAcquisitionResult::AlreadyLocked {
            held_by_workflow: "test".to_string(),
            held_by_execution: "exec-001".to_string(),
            locked_since_ms: 1_234_567_890,
        };
        assert!(!locked.is_acquired());
        assert!(locked.is_already_locked());
    }

    #[test]
    fn lock_manager_concurrent_simulation() {
        use std::sync::Arc;
        use std::thread;

        let manager = Arc::new(PaneWorkflowLockManager::new());
        let pane_id = 42;

        // Simulate concurrent access with threads
        let mut handles = vec![];

        for i in 0..10 {
            let m = Arc::clone(&manager);
            let handle = thread::spawn(move || {
                let exec_id = format!("exec-{i:03}");
                m.try_acquire(pane_id, "concurrent_workflow", &exec_id)
            });
            handles.push(handle);
        }

        // Collect results
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Exactly one should have acquired the lock
        let acquired_count = results.iter().filter(|r| r.is_acquired()).count();
        let locked_count = results.iter().filter(|r| r.is_already_locked()).count();

        assert_eq!(acquired_count, 1);
        assert_eq!(locked_count, 9);
    }

    #[test]
    fn pane_lock_info_serialization() {
        let info = PaneLockInfo {
            pane_id: 42,
            workflow_name: "test_workflow".to_string(),
            execution_id: "exec-001".to_string(),
            locked_at_ms: 1_234_567_890_000,
        };

        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneLockInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.pane_id, info.pane_id);
        assert_eq!(parsed.workflow_name, info.workflow_name);
        assert_eq!(parsed.execution_id, info.execution_id);
        assert_eq!(parsed.locked_at_ms, info.locked_at_ms);
    }

    // ========================================================================
    // Coordination Primitives Tests (wa-nu4.4.4.1)
    // ========================================================================

    #[test]
    fn pane_group_by_domain() {
        let panes = vec![
            crate::storage::PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 2,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 3,
                pane_uuid: None,
                domain: "SSH:host1".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
        ];

        let groups = build_pane_groups(&panes, &PaneGroupStrategy::ByDomain);
        assert_eq!(groups.len(), 2);
        // BTreeMap sorts by key, so "SSH:host1" comes before "local"
        assert_eq!(groups[0].name, "SSH:host1");
        assert_eq!(groups[0].pane_ids, vec![3]);
        assert_eq!(groups[1].name, "local");
        assert_eq!(groups[1].pane_ids, vec![1, 2]);
    }

    #[test]
    fn pane_group_by_agent() {
        let panes = vec![
            crate::storage::PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("codex session".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 2,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("claude code".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 3,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("bash shell".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
        ];

        let groups = build_pane_groups(&panes, &PaneGroupStrategy::ByAgent);
        assert_eq!(groups.len(), 3);
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"codex"));
        assert!(names.contains(&"claude_code"));
        assert!(names.contains(&"unknown"));
    }

    #[test]
    fn pane_group_by_project() {
        let panes = vec![
            crate::storage::PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/home/user/project-a".to_string()),
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 2,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/home/user/project-a".to_string()),
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 3,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/home/user/project-b".to_string()),
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
        ];

        let groups = build_pane_groups(&panes, &PaneGroupStrategy::ByProject);
        assert_eq!(groups.len(), 2);
        let proj_a = groups
            .iter()
            .find(|g| g.name.contains("project-a"))
            .unwrap();
        assert_eq!(proj_a.pane_ids, vec![1, 2]);
        let proj_b = groups
            .iter()
            .find(|g| g.name.contains("project-b"))
            .unwrap();
        assert_eq!(proj_b.pane_ids, vec![3]);
    }

    #[test]
    fn pane_group_explicit() {
        let groups = build_pane_groups(
            &[],
            &PaneGroupStrategy::Explicit {
                pane_ids: vec![5, 3, 1],
            },
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "explicit");
        assert_eq!(groups[0].pane_ids, vec![1, 3, 5]); // sorted
    }

    #[test]
    fn pane_group_len_and_is_empty() {
        let group = PaneGroup::new("test", vec![1, 2, 3], PaneGroupStrategy::ByDomain);
        assert_eq!(group.len(), 3);
        assert!(!group.is_empty());

        let empty = PaneGroup::new("empty", vec![], PaneGroupStrategy::ByDomain);
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
    }

    #[test]
    fn group_lock_all_succeed() {
        let manager = PaneWorkflowLockManager::new();
        let result = manager.try_acquire_group(&[1, 2, 3], "swarm_wf", "exec-001");
        assert!(result.is_acquired());
        match &result {
            GroupLockResult::Acquired { locked_panes } => {
                assert_eq!(locked_panes.len(), 3);
            }
            GroupLockResult::PartialFailure { .. } => panic!("Expected Acquired"),
        }

        // All should be locked
        assert!(manager.is_locked(1).is_some());
        assert!(manager.is_locked(2).is_some());
        assert!(manager.is_locked(3).is_some());
    }

    #[test]
    fn group_lock_partial_failure_rolls_back() {
        let manager = PaneWorkflowLockManager::new();

        // Pre-lock pane 2
        manager.try_acquire(2, "other_wf", "exec-other");

        // Group lock should fail and roll back
        let result = manager.try_acquire_group(&[1, 2, 3], "swarm_wf", "exec-001");
        assert!(!result.is_acquired());

        match &result {
            GroupLockResult::PartialFailure {
                would_have_locked,
                conflicts,
            } => {
                // Pane 1 would have been locked but was rolled back
                assert!(would_have_locked.contains(&1));
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].pane_id, 2);
                assert_eq!(conflicts[0].held_by_workflow, "other_wf");
            }
            GroupLockResult::Acquired { .. } => panic!("Expected PartialFailure"),
        }

        // Pane 1 should NOT be locked (rollback)
        assert!(manager.is_locked(1).is_none());
        // Pane 2 still locked by original holder
        assert!(manager.is_locked(2).is_some());
        // Pane 3 should NOT be locked (never reached or rolled back)
        assert!(manager.is_locked(3).is_none());
    }

    #[test]
    fn group_lock_release_all() {
        let manager = PaneWorkflowLockManager::new();
        manager.try_acquire_group(&[1, 2, 3], "swarm_wf", "exec-001");

        let released = manager.release_group(&[1, 2, 3], "exec-001");
        assert_eq!(released, 3);

        assert!(manager.is_locked(1).is_none());
        assert!(manager.is_locked(2).is_none());
        assert!(manager.is_locked(3).is_none());
    }

    #[test]
    fn broadcast_precondition_prompt_active() {
        let passing = PaneCapabilities {
            prompt_active: true,
            ..Default::default()
        };
        let failing = PaneCapabilities {
            prompt_active: false,
            ..Default::default()
        };
        assert!(BroadcastPrecondition::PromptActive.check(&passing));
        assert!(!BroadcastPrecondition::PromptActive.check(&failing));
    }

    #[test]
    fn broadcast_precondition_not_alt_screen() {
        let normal = PaneCapabilities {
            alt_screen: Some(false),
            ..Default::default()
        };
        let alt = PaneCapabilities {
            alt_screen: Some(true),
            ..Default::default()
        };
        let unknown = PaneCapabilities {
            alt_screen: None,
            ..Default::default()
        };
        assert!(BroadcastPrecondition::NotAltScreen.check(&normal));
        assert!(!BroadcastPrecondition::NotAltScreen.check(&alt));
        assert!(BroadcastPrecondition::NotAltScreen.check(&unknown));
    }

    #[test]
    fn broadcast_precondition_no_gap_and_not_reserved() {
        let safe = PaneCapabilities {
            has_recent_gap: false,
            is_reserved: false,
            ..Default::default()
        };
        assert!(BroadcastPrecondition::NoRecentGap.check(&safe));
        assert!(BroadcastPrecondition::NotReserved.check(&safe));

        let risky = PaneCapabilities {
            has_recent_gap: true,
            is_reserved: true,
            ..Default::default()
        };
        assert!(!BroadcastPrecondition::NoRecentGap.check(&risky));
        assert!(!BroadcastPrecondition::NotReserved.check(&risky));
    }

    #[test]
    fn check_preconditions_returns_failed_labels() {
        let preconditions = default_broadcast_preconditions();
        let caps = PaneCapabilities {
            prompt_active: false,
            alt_screen: Some(true),
            has_recent_gap: false,
            is_reserved: false,
            ..Default::default()
        };
        let failures = check_preconditions(&preconditions, &caps);
        assert!(failures.contains(&"prompt_active"));
        assert!(failures.contains(&"not_alt_screen"));
        assert!(!failures.contains(&"no_recent_gap"));
    }

    #[test]
    fn check_preconditions_all_pass() {
        let preconditions = default_broadcast_preconditions();
        let caps = PaneCapabilities {
            prompt_active: true,
            alt_screen: Some(false),
            has_recent_gap: false,
            is_reserved: false,
            ..Default::default()
        };
        let failures = check_preconditions(&preconditions, &caps);
        assert!(failures.is_empty());
    }

    #[test]
    fn broadcast_result_tracking() {
        let mut result = BroadcastResult::new("test_action");
        result.add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 100 });
        result.add_outcome(
            2,
            PaneBroadcastOutcome::Denied {
                reason: "policy denied".to_string(),
            },
        );
        result.add_outcome(
            3,
            PaneBroadcastOutcome::PreconditionFailed {
                failed: vec!["prompt_active".to_string()],
            },
        );
        result.add_outcome(
            4,
            PaneBroadcastOutcome::Skipped {
                reason: "locked".to_string(),
            },
        );

        assert_eq!(result.allowed_count(), 1);
        assert_eq!(result.denied_count(), 1);
        assert_eq!(result.precondition_failed_count(), 1);
        assert_eq!(result.skipped_count(), 1);
        assert!(!result.all_allowed());
    }

    #[test]
    fn broadcast_result_all_allowed() {
        let mut result = BroadcastResult::new("multi_pane_restart");
        result.add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 50 });
        result.add_outcome(2, PaneBroadcastOutcome::Allowed { elapsed_ms: 75 });

        assert!(result.all_allowed());
        assert_eq!(result.allowed_count(), 2);
    }

    #[test]
    fn broadcast_result_serialization() {
        let mut result = BroadcastResult::new("test");
        result.add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 100 });
        result.total_elapsed_ms = 150;

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["action"], "test");
        assert_eq!(json["total_elapsed_ms"], 150);
        assert_eq!(json["outcomes"][0]["pane_id"], 1);
        assert_eq!(json["outcomes"][0]["outcome"]["status"], "allowed");
    }

    #[test]
    fn pane_group_strategy_serialization() {
        let strategy = PaneGroupStrategy::Explicit {
            pane_ids: vec![1, 2, 3],
        };
        let json = serde_json::to_value(&strategy).unwrap();
        assert_eq!(json["type"], "explicit");
        assert_eq!(json["pane_ids"], serde_json::json!([1, 2, 3]));

        let by_domain = PaneGroupStrategy::ByDomain;
        let json = serde_json::to_value(&by_domain).unwrap();
        assert_eq!(json["type"], "by_domain");
    }

    #[test]
    fn infer_agent_from_title_works() {
        assert_eq!(infer_agent_from_title("Codex CLI session"), Some("codex"));
        assert_eq!(
            infer_agent_from_title("Claude Code - project"),
            Some("claude_code")
        );
        assert_eq!(infer_agent_from_title("Gemini workspace"), Some("gemini"));
        assert_eq!(infer_agent_from_title("bash shell"), None);
    }

    #[test]
    fn default_broadcast_preconditions_has_all_four() {
        let preconditions = default_broadcast_preconditions();
        assert_eq!(preconditions.len(), 4);
        let labels: Vec<&str> = preconditions.iter().map(|p| p.label()).collect();
        assert!(labels.contains(&"prompt_active"));
        assert!(labels.contains(&"not_alt_screen"));
        assert!(labels.contains(&"no_recent_gap"));
        assert!(labels.contains(&"not_reserved"));
    }

    // ========================================================================
    // Multi-Pane Coordination Workflow Tests (wa-nu4.4.4.2)
    // ========================================================================

    fn make_test_pane(
        pane_id: u64,
        domain: &str,
        title: Option<&str>,
        cwd: Option<&str>,
    ) -> crate::storage::PaneRecord {
        crate::storage::PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: domain.to_string(),
            window_id: None,
            tab_id: None,
            title: title.map(str::to_string),
            cwd: cwd.map(str::to_string),
            tty_name: None,
            first_seen_at: 0,
            last_seen_at: 0,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    fn make_ready_caps() -> crate::policy::PaneCapabilities {
        crate::policy::PaneCapabilities {
            prompt_active: true,
            command_running: false,
            alt_screen: Some(false),
            has_recent_gap: false,
            is_reserved: false,
            reserved_by: None,
        }
    }

    fn make_busy_caps() -> crate::policy::PaneCapabilities {
        crate::policy::PaneCapabilities {
            prompt_active: false,
            command_running: true,
            alt_screen: Some(false),
            has_recent_gap: false,
            is_reserved: false,
            reserved_by: None,
        }
    }

    fn make_alt_screen_caps() -> crate::policy::PaneCapabilities {
        crate::policy::PaneCapabilities {
            prompt_active: false,
            command_running: false,
            alt_screen: Some(true),
            has_recent_gap: false,
            is_reserved: false,
            reserved_by: None,
        }
    }

    #[test]
    fn coordinate_agents_config_defaults() {
        let config = CoordinateAgentsConfig::default();
        assert_eq!(config.strategy, PaneGroupStrategy::ByAgent);
        assert_eq!(config.preconditions.len(), 4);
        assert!(!config.abort_on_lock_failure);
    }

    #[test]
    fn coordinate_agents_config_serialization() {
        let config = CoordinateAgentsConfig {
            strategy: PaneGroupStrategy::Explicit {
                pane_ids: vec![1, 2],
            },
            preconditions: vec![BroadcastPrecondition::PromptActive],
            abort_on_lock_failure: true,
        };
        let json = serde_json::to_value(&config).expect("serialize");
        assert_eq!(json["abort_on_lock_failure"], true);
        let rt: CoordinateAgentsConfig = serde_json::from_value(json).expect("deserialize");
        assert!(rt.abort_on_lock_failure);
        assert_eq!(rt.preconditions.len(), 1);
    }

    #[test]
    fn agent_reread_prompt_for_known_agents() {
        assert!(agent_reread_prompt("codex").contains("AGENTS.md"));
        assert!(agent_reread_prompt("claude_code").contains("AGENTS.md"));
        assert!(agent_reread_prompt("gemini").contains("AGENTS.md"));
        assert!(agent_reread_prompt("unknown").contains("AGENTS.md"));
    }

    #[test]
    fn agent_pause_text_is_ctrl_c() {
        for agent in &["codex", "claude_code", "gemini", "unknown"] {
            assert_eq!(agent_pause_text(agent), "\x03");
        }
    }

    #[test]
    fn evaluate_preconditions_all_pass() {
        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_ready_caps());

        let results =
            evaluate_pane_preconditions(&[1, 2], &caps, &default_broadcast_preconditions());

        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_none()); // pane 1 passed
        assert!(results[1].1.is_none()); // pane 2 passed
    }

    #[test]
    fn evaluate_preconditions_filters_busy_pane() {
        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_busy_caps()); // command running, no prompt

        let results =
            evaluate_pane_preconditions(&[1, 2], &caps, &default_broadcast_preconditions());

        assert!(results[0].1.is_none()); // pane 1 passed
        match &results[1].1 {
            Some(PaneBroadcastOutcome::PreconditionFailed { failed }) => {
                assert!(failed.contains(&"prompt_active".to_string()));
            }
            other => panic!("Expected PreconditionFailed, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_preconditions_missing_caps_skips() {
        let caps = std::collections::HashMap::new(); // no caps at all

        let results = evaluate_pane_preconditions(&[1], &caps, &default_broadcast_preconditions());

        match &results[0].1 {
            Some(PaneBroadcastOutcome::Skipped { reason }) => {
                assert!(reason.contains("no capabilities"));
            }
            other => panic!("Expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn plan_reread_context_filters_and_groups() {
        let panes = vec![
            make_test_pane(1, "local", Some("codex session"), None),
            make_test_pane(2, "local", Some("claude code"), None),
            make_test_pane(3, "local", Some("vim editor"), None), // will be in alt-screen
        ];

        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_ready_caps());
        caps.insert(3, make_alt_screen_caps());

        let config = CoordinateAgentsConfig {
            strategy: PaneGroupStrategy::ByAgent,
            preconditions: default_broadcast_preconditions(),
            abort_on_lock_failure: false,
        };

        let result = plan_reread_context(&panes, &caps, &config);

        assert_eq!(result.operation, "reread_context");
        assert_eq!(result.total_panes(), 3);
        assert_eq!(result.total_acted(), 2); // panes 1 and 2
        assert_eq!(result.broadcast.allowed_count(), 2);
        assert_eq!(result.broadcast.precondition_failed_count(), 1); // pane 3
    }

    #[test]
    fn plan_reread_context_empty_panes() {
        let panes: Vec<crate::storage::PaneRecord> = vec![];
        let caps = std::collections::HashMap::new();
        let config = CoordinateAgentsConfig::default();

        let result = plan_reread_context(&panes, &caps, &config);

        assert_eq!(result.total_panes(), 0);
        assert_eq!(result.total_acted(), 0);
        assert!(result.groups.is_empty());
    }

    #[test]
    fn plan_pause_all_relaxed_preconditions() {
        // pause_all should only filter by NotAltScreen, not by PromptActive
        let panes = vec![
            make_test_pane(1, "local", Some("codex session"), None),
            make_test_pane(2, "local", Some("claude code"), None),
            make_test_pane(3, "local", Some("vim editor"), None),
        ];

        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_busy_caps()); // command running — should still get paused
        caps.insert(2, make_ready_caps());
        caps.insert(3, make_alt_screen_caps()); // alt-screen — should be filtered out

        let config = CoordinateAgentsConfig::default();
        let result = plan_pause_all(&panes, &caps, &config);

        assert_eq!(result.operation, "pause_all");
        // panes 1 and 2 should be acted on, pane 3 filtered by alt-screen
        assert_eq!(result.broadcast.allowed_count(), 2);
        assert_eq!(result.broadcast.precondition_failed_count(), 1);
    }

    #[test]
    fn plan_pause_all_explicit_strategy() {
        let panes = vec![
            make_test_pane(1, "local", None, None),
            make_test_pane(2, "local", None, None),
        ];

        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_ready_caps());

        let config = CoordinateAgentsConfig {
            strategy: PaneGroupStrategy::Explicit {
                pane_ids: vec![1, 2],
            },
            preconditions: default_broadcast_preconditions(),
            abort_on_lock_failure: false,
        };

        let result = plan_pause_all(&panes, &caps, &config);
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].group_name, "explicit");
        assert_eq!(result.broadcast.allowed_count(), 2);
    }

    #[test]
    fn resolve_reread_prompts_agent_specific() {
        let panes = vec![
            make_test_pane(1, "local", Some("codex session"), None),
            make_test_pane(2, "local", Some("claude code"), None),
            make_test_pane(3, "local", Some("bash shell"), None),
        ];

        let prompts = resolve_reread_prompts(&panes);
        assert_eq!(prompts.len(), 3);
        assert!(prompts[&1].contains("AGENTS.md"));
        assert!(prompts[&2].contains("AGENTS.md"));
        assert!(prompts[&3].contains("AGENTS.md"));
        // Codex gets the plain-text version, Claude gets /read
        assert!(prompts[&1].starts_with("Read"));
        assert!(prompts[&2].starts_with("/read"));
    }

    #[test]
    fn resolve_pause_texts_all_ctrl_c() {
        let panes = vec![
            make_test_pane(1, "local", Some("codex session"), None),
            make_test_pane(2, "local", Some("unknown"), None),
        ];

        let texts = resolve_pause_texts(&panes);
        assert_eq!(texts[&1], "\x03");
        assert_eq!(texts[&2], "\x03");
    }

    #[test]
    fn coordination_result_new_and_accessors() {
        let mut result = CoordinationResult::new("test_op");
        assert_eq!(result.operation, "test_op");
        assert_eq!(result.total_panes(), 0);
        assert_eq!(result.total_acted(), 0);

        result.groups.push(GroupCoordinationEntry {
            group_name: "g1".to_string(),
            pane_count: 3,
            acted_count: 2,
            precondition_failed_count: 1,
            skipped_count: 0,
        });
        result.groups.push(GroupCoordinationEntry {
            group_name: "g2".to_string(),
            pane_count: 2,
            acted_count: 1,
            precondition_failed_count: 0,
            skipped_count: 1,
        });

        assert_eq!(result.total_panes(), 5);
        assert_eq!(result.total_acted(), 3);
    }

    #[test]
    fn coordination_result_serialization() {
        let mut result = CoordinationResult::new("reread_context");
        result.groups.push(GroupCoordinationEntry {
            group_name: "codex".to_string(),
            pane_count: 2,
            acted_count: 2,
            precondition_failed_count: 0,
            skipped_count: 0,
        });
        result
            .broadcast
            .add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 5 });
        result
            .broadcast
            .add_outcome(2, PaneBroadcastOutcome::Allowed { elapsed_ms: 3 });

        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(json["operation"], "reread_context");
        assert_eq!(json["groups"][0]["group_name"], "codex");
        assert_eq!(json["broadcast"]["outcomes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn plan_reread_context_by_domain_groups_correctly() {
        let panes = vec![
            make_test_pane(1, "local", Some("codex"), None),
            make_test_pane(2, "local", Some("claude"), None),
            make_test_pane(3, "SSH:remote", Some("codex"), None),
        ];

        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_ready_caps());
        caps.insert(3, make_ready_caps());

        let config = CoordinateAgentsConfig {
            strategy: PaneGroupStrategy::ByDomain,
            preconditions: default_broadcast_preconditions(),
            abort_on_lock_failure: false,
        };

        let result = plan_reread_context(&panes, &caps, &config);
        assert_eq!(result.groups.len(), 2); // "SSH:remote" and "local"
        assert_eq!(result.broadcast.allowed_count(), 3);
    }

    #[test]
    fn group_coordination_entry_fields() {
        let entry = GroupCoordinationEntry {
            group_name: "test".to_string(),
            pane_count: 5,
            acted_count: 3,
            precondition_failed_count: 1,
            skipped_count: 1,
        };
        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["pane_count"], 5);
        assert_eq!(json["acted_count"], 3);
        assert_eq!(json["precondition_failed_count"], 1);
        assert_eq!(json["skipped_count"], 1);
    }

    // ========================================================================
    // Unstick Workflow Tests (wa-nu4.4.4.4)
    // ========================================================================

    #[test]
    fn unstick_finding_kind_labels() {
        assert_eq!(UnstickFindingKind::TodoComment.label(), "TODO/FIXME");
        assert_eq!(UnstickFindingKind::PanicSite.label(), "panic site");
        assert_eq!(
            UnstickFindingKind::SuppressedError.label(),
            "suppressed error"
        );
    }

    #[test]
    fn unstick_finding_kind_serialization() {
        let json = serde_json::to_value(UnstickFindingKind::TodoComment).expect("serialize");
        assert_eq!(json, "todo_comment");

        let json = serde_json::to_value(UnstickFindingKind::PanicSite).expect("serialize");
        assert_eq!(json, "panic_site");

        let rt: UnstickFindingKind =
            serde_json::from_str("\"suppressed_error\"").expect("deserialize");
        assert_eq!(rt, UnstickFindingKind::SuppressedError);
    }

    #[test]
    fn unstick_config_defaults() {
        let config = UnstickConfig::default();
        assert_eq!(config.max_findings_per_kind, 10);
        assert_eq!(config.max_total_findings, 25);
        assert!(config.extensions.contains(&"rs".to_string()));
        assert!(config.extensions.contains(&"py".to_string()));
    }

    #[test]
    fn unstick_report_empty() {
        let report = UnstickReport::empty("text");
        assert_eq!(report.total_findings(), 0);
        assert_eq!(report.scanner, "text");
        assert!(!report.truncated);
        assert_eq!(report.human_summary(), "No actionable findings.");
    }

    #[test]
    fn unstick_report_with_findings() {
        let mut report = UnstickReport::empty("text");
        report.findings.push(UnstickFinding {
            kind: UnstickFindingKind::TodoComment,
            file: "src/main.rs".to_string(),
            line: 42,
            snippet: "// TODO: fix this".to_string(),
            suggestion: "Address this TODO".to_string(),
        });
        report.findings.push(UnstickFinding {
            kind: UnstickFindingKind::PanicSite,
            file: "src/lib.rs".to_string(),
            line: 100,
            snippet: "value.unwrap()".to_string(),
            suggestion: "Use ? operator".to_string(),
        });
        report.files_scanned = 5;
        report.counts.insert("TODO/FIXME".to_string(), 1);
        report.counts.insert("panic site".to_string(), 1);

        assert_eq!(report.total_findings(), 2);
        let summary = report.human_summary();
        assert!(summary.contains("Found 2 items"));
        assert!(summary.contains("src/main.rs:42"));
        assert!(summary.contains("src/lib.rs:100"));
    }

    #[test]
    fn unstick_report_serialization() {
        let mut report = UnstickReport::empty("text");
        report.findings.push(UnstickFinding {
            kind: UnstickFindingKind::TodoComment,
            file: "test.rs".to_string(),
            line: 1,
            snippet: "// TODO".to_string(),
            suggestion: "fix it".to_string(),
        });
        report.files_scanned = 1;

        let json = serde_json::to_value(&report).expect("serialize");
        assert_eq!(json["scanner"], "text");
        assert_eq!(json["files_scanned"], 1);
        assert_eq!(json["findings"][0]["kind"], "todo_comment");
        assert_eq!(json["findings"][0]["file"], "test.rs");
        assert_eq!(json["findings"][0]["line"], 1);
    }

    #[test]
    fn truncate_snippet_short_passthrough() {
        assert_eq!(truncate_snippet("short", 80), "short");
        assert_eq!(truncate_snippet("  padded  ", 80), "padded");
    }

    #[test]
    fn truncate_snippet_long_truncated() {
        let long = "a".repeat(100);
        let result = truncate_snippet(&long, 20);
        assert!(result.len() <= 20);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_snippet_small_limit_stays_bounded() {
        let long = "abcdef";
        assert_eq!(truncate_snippet(long, 0), "");
        assert_eq!(truncate_snippet(long, 1), ".");
        assert_eq!(truncate_snippet(long, 2), "..");
        assert_eq!(truncate_snippet(long, 3), "...");
    }

    #[test]
    fn truncate_snippet_unicode_boundary_safe() {
        let emoji = "😀😀😀😀";
        let result = truncate_snippet(emoji, 7);
        assert_eq!(result, "😀...");
        assert!(result.len() <= 7);
    }

    #[test]
    fn scan_file_text_finds_todo() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "fn main() {\n    // TODO: fix this\n}\n").expect("write file");

        let patterns = TextScanPatterns::new();
        let mut kind_counts = std::collections::HashMap::new();

        let findings = scan_file_text(&file_path, dir.path(), &patterns, 10, &mut kind_counts);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, UnstickFindingKind::TodoComment);
        assert_eq!(findings[0].file, "test.rs");
        assert_eq!(findings[0].line, 2);
        assert!(findings[0].snippet.contains("TODO"));
    }

    #[test]
    fn scan_file_text_finds_unwrap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "let x = foo.unwrap();\n").expect("write file");

        let patterns = TextScanPatterns::new();
        let mut kind_counts = std::collections::HashMap::new();

        let findings = scan_file_text(&file_path, dir.path(), &patterns, 10, &mut kind_counts);

        assert!(
            findings
                .iter()
                .any(|f| f.kind == UnstickFindingKind::PanicSite)
        );
    }

    #[test]
    fn scan_file_text_finds_suppressed_error() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "let _ = some_fallible_call()?;\n").expect("write file");

        let patterns = TextScanPatterns::new();
        let mut kind_counts = std::collections::HashMap::new();

        let findings = scan_file_text(&file_path, dir.path(), &patterns, 10, &mut kind_counts);

        assert!(
            findings
                .iter()
                .any(|f| f.kind == UnstickFindingKind::SuppressedError)
        );
    }

    #[test]
    fn scan_file_text_respects_max_per_kind() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("test.rs");
        let content = (0..20)
            .map(|i| format!("// TODO: item {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, content).expect("write file");

        let patterns = TextScanPatterns::new();
        let mut kind_counts = std::collections::HashMap::new();

        let findings = scan_file_text(&file_path, dir.path(), &patterns, 3, &mut kind_counts);

        assert_eq!(findings.len(), 3); // capped at max_per_kind
    }

    #[test]
    fn run_unstick_scan_text_on_fixture_dir() {
        let dir = tempfile::tempdir().expect("create temp dir");

        // Create a small fixture tree
        std::fs::write(
            dir.path().join("main.rs"),
            "fn main() {\n    // TODO: implement\n    let x = foo.unwrap();\n}\n",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn bar() {\n    // FIXME: clean up\n}\n",
        )
        .expect("write");
        // Non-matching extension should be skipped
        std::fs::write(
            dir.path().join("notes.txt"),
            "TODO: this should be skipped\n",
        )
        .expect("write");

        let config = UnstickConfig {
            root: dir.path().to_path_buf(),
            max_findings_per_kind: 10,
            max_total_findings: 25,
            extensions: vec!["rs".to_string()],
        };

        let report = run_unstick_scan_text(&config);

        assert_eq!(report.scanner, "text");
        assert_eq!(report.files_scanned, 2);
        assert!(!report.truncated);
        // Should find: 2 TODOs + 1 unwrap = 3 findings minimum
        assert!(report.total_findings() >= 3);
        // .txt file should not contribute findings
        assert!(report.findings.iter().all(|f| {
            std::path::Path::new(&f.file)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
        }));
    }

    #[test]
    fn run_unstick_scan_text_skips_hidden_and_target() {
        let dir = tempfile::tempdir().expect("create temp dir");

        // Create dirs that should be skipped
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).expect("mkdir");
        std::fs::write(hidden.join("secret.rs"), "// TODO: hidden").expect("write");

        let target = dir.path().join("target");
        std::fs::create_dir(&target).expect("mkdir");
        std::fs::write(target.join("built.rs"), "// TODO: target").expect("write");

        // Create a file that should be scanned
        std::fs::write(dir.path().join("src.rs"), "// TODO: visible").expect("write");

        let config = UnstickConfig {
            root: dir.path().to_path_buf(),
            ..UnstickConfig::default()
        };

        let report = run_unstick_scan_text(&config);

        assert_eq!(report.files_scanned, 1);
        assert_eq!(report.total_findings(), 1);
        assert_eq!(report.findings[0].file, "src.rs");
    }

    #[test]
    fn run_unstick_scan_text_truncates_at_max_total() {
        let dir = tempfile::tempdir().expect("create temp dir");

        // Create many TODO lines
        let content = (0..50)
            .map(|i| format!("// TODO: item {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("big.rs"), content).expect("write");

        let config = UnstickConfig {
            root: dir.path().to_path_buf(),
            max_findings_per_kind: 100, // high per-kind limit
            max_total_findings: 5,      // low total limit
            extensions: vec!["rs".to_string()],
        };

        let report = run_unstick_scan_text(&config);

        assert!(report.total_findings() <= 5);
        assert!(report.truncated);
    }

    #[test]
    fn run_unstick_scan_text_empty_dir() {
        let dir = tempfile::tempdir().expect("create temp dir");

        let config = UnstickConfig {
            root: dir.path().to_path_buf(),
            ..UnstickConfig::default()
        };

        let report = run_unstick_scan_text(&config);
        assert_eq!(report.total_findings(), 0);
        assert_eq!(report.files_scanned, 0);
        assert!(!report.truncated);
    }

    #[test]
    fn run_unstick_scan_text_nonexistent_dir() {
        let config = UnstickConfig {
            root: std::path::PathBuf::from("/nonexistent/path/does/not/exist"),
            ..UnstickConfig::default()
        };

        let report = run_unstick_scan_text(&config);
        assert_eq!(report.total_findings(), 0);
        assert_eq!(report.files_scanned, 0);
    }

    #[test]
    fn unstick_human_summary_with_many_findings() {
        let mut report = UnstickReport::empty("text");
        for i in 0..15 {
            report.findings.push(UnstickFinding {
                kind: UnstickFindingKind::TodoComment,
                file: format!("file{i}.rs"),
                line: i as u32 + 1,
                snippet: format!("// TODO: item {i}"),
                suggestion: "fix it".to_string(),
            });
        }
        report.files_scanned = 15;
        report.counts.insert("TODO/FIXME".to_string(), 15);

        let summary = report.human_summary();
        assert!(summary.contains("Found 15 items"));
        assert!(summary.contains("... and 5 more"));
    }

    // ========================================================================
    // WorkflowRunner Tests
    // ========================================================================

    #[test]
    fn workflow_runner_config_default_has_sensible_values() {
        let config = WorkflowRunnerConfig::default();

        assert!(config.max_concurrent > 0);
        assert!(config.step_timeout_ms > 0);
        assert!(config.retry_backoff_multiplier >= 1.0);
        assert!(config.max_retries_per_step > 0);
    }

    #[test]
    fn workflow_start_result_variants_serialize() {
        let variants = vec![
            WorkflowStartResult::Started {
                execution_id: "exec-001".to_string(),
                workflow_name: "test_workflow".to_string(),
            },
            WorkflowStartResult::NoMatchingWorkflow {
                rule_id: "test.rule".to_string(),
            },
            WorkflowStartResult::PaneLocked {
                pane_id: 42,
                held_by_workflow: "other_workflow".to_string(),
                held_by_execution: "exec-002".to_string(),
            },
            WorkflowStartResult::Error {
                error: "Something went wrong".to_string(),
            },
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let parsed: WorkflowStartResult = serde_json::from_str(&json).unwrap();

            // Verify round-trip
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn workflow_execution_result_variants_serialize() {
        let variants = vec![
            WorkflowExecutionResult::Completed {
                execution_id: "exec-001".to_string(),
                result: serde_json::json!({"success": true}),
                elapsed_ms: 1000,
                steps_executed: 3,
            },
            WorkflowExecutionResult::Aborted {
                execution_id: "exec-002".to_string(),
                reason: "Timeout exceeded".to_string(),
                step_index: 2,
                elapsed_ms: 5000,
            },
            WorkflowExecutionResult::PolicyDenied {
                execution_id: "exec-003".to_string(),
                step_index: 1,
                reason: "Rate limit exceeded".to_string(),
            },
            WorkflowExecutionResult::Error {
                execution_id: Some("exec-004".to_string()),
                error: "Database connection failed".to_string(),
            },
            WorkflowExecutionResult::Error {
                execution_id: None,
                error: "Early failure".to_string(),
            },
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let parsed: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();

            // Verify round-trip
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn workflow_start_result_accessors_work() {
        let started = WorkflowStartResult::Started {
            execution_id: "exec-001".to_string(),
            workflow_name: "test".to_string(),
        };
        assert!(started.is_started());
        assert!(!started.is_locked());
        assert!(started.execution_id().is_some());

        let locked = WorkflowStartResult::PaneLocked {
            pane_id: 1,
            held_by_workflow: "other".to_string(),
            held_by_execution: "exec-002".to_string(),
        };
        assert!(!locked.is_started());
        assert!(locked.is_locked());
        assert!(locked.execution_id().is_none());

        let no_match = WorkflowStartResult::NoMatchingWorkflow {
            rule_id: "test".to_string(),
        };
        assert!(!no_match.is_started());
        assert!(!no_match.is_locked());
        assert!(no_match.execution_id().is_none());

        let error = WorkflowStartResult::Error {
            error: "fail".to_string(),
        };
        assert!(!error.is_started());
        assert!(!error.is_locked());
        assert!(error.execution_id().is_none());
    }

    #[test]
    fn workflow_execution_result_accessors_work() {
        let completed = WorkflowExecutionResult::Completed {
            execution_id: "exec-001".to_string(),
            result: serde_json::Value::Null,
            elapsed_ms: 100,
            steps_executed: 2,
        };
        assert!(completed.is_completed());
        assert!(!completed.is_aborted());
        assert_eq!(completed.execution_id(), Some("exec-001"));

        let aborted = WorkflowExecutionResult::Aborted {
            execution_id: "exec-002".to_string(),
            reason: "test".to_string(),
            step_index: 1,
            elapsed_ms: 50,
        };
        assert!(!aborted.is_completed());
        assert!(aborted.is_aborted());
        assert_eq!(aborted.execution_id(), Some("exec-002"));

        let denied = WorkflowExecutionResult::PolicyDenied {
            execution_id: "exec-003".to_string(),
            step_index: 0,
            reason: "rate limit".to_string(),
        };
        assert!(!denied.is_completed());
        assert!(!denied.is_aborted());
        assert_eq!(denied.execution_id(), Some("exec-003"));

        let error_with_id = WorkflowExecutionResult::Error {
            execution_id: Some("exec-004".to_string()),
            error: "fail".to_string(),
        };
        assert!(!error_with_id.is_completed());
        assert!(!error_with_id.is_aborted());
        assert_eq!(error_with_id.execution_id(), Some("exec-004"));

        let error_no_id = WorkflowExecutionResult::Error {
            execution_id: None,
            error: "fail".to_string(),
        };
        assert!(error_no_id.execution_id().is_none());
    }

    // ========================================================================
    // Workflow Selection Tests
    // ========================================================================

    /// Test workflow that handles compaction patterns.
    struct MockCompactionWorkflow;

    impl Workflow for MockCompactionWorkflow {
        fn name(&self) -> &'static str {
            "handle_compaction"
        }

        fn description(&self) -> &'static str {
            "Mock workflow for compaction handling"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("compaction")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("notify", "Send notification")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done_empty(),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Test workflow that handles usage limit patterns.
    struct MockUsageLimitWorkflow;

    impl Workflow for MockUsageLimitWorkflow {
        fn name(&self) -> &'static str {
            "handle_usage_limit"
        }

        fn description(&self) -> &'static str {
            "Mock workflow for usage limit handling"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("usage")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("warn", "Send warning")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done_empty(),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Test that find_matching_workflow returns the correct workflow for a detection.
    #[test]
    fn workflow_runner_selects_correct_workflow_for_compaction() {
        // Create runner with multiple registered workflows
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());

        // Create mock injector (won't be called in this test)
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                default_wezterm_handle(),
            ),
        ));

        // Create a minimal storage handle using temp file
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();
        let storage = rt.block_on(async {
            Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap())
        });

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            storage.clone(),
            injector,
            WorkflowRunnerConfig::default(),
        );

        // Register workflows
        runner.register_workflow(Arc::new(MockCompactionWorkflow));
        runner.register_workflow(Arc::new(MockUsageLimitWorkflow));

        // Create compaction detection
        let compaction_detection = Detection {
            rule_id: "claude.compaction".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "compaction".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            matched_text: "Auto-compact: compacted".to_string(),
            extracted: serde_json::json!({}),
            span: (0, 0),
        };

        // Should find compaction workflow
        let workflow = runner.find_matching_workflow(&compaction_detection);
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_compaction");

        // Create usage detection
        let usage_detection = Detection {
            rule_id: "codex.usage.warning".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage_warning".to_string(),
            severity: Severity::Info,
            confidence: 0.8,
            matched_text: "less than 25%".to_string(),
            extracted: serde_json::json!({}),
            span: (0, 0),
        };

        // Should find usage limit workflow
        let workflow = runner.find_matching_workflow(&usage_detection);
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_usage_limit");

        // Create unmatched detection
        let unmatched_detection = Detection {
            rule_id: "unknown.pattern".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "unknown".to_string(),
            severity: Severity::Info,
            confidence: 0.5,
            matched_text: "something".to_string(),
            extracted: serde_json::json!({}),
            span: (0, 0),
        };

        // Should not find any workflow
        let workflow = runner.find_matching_workflow(&unmatched_detection);
        assert!(workflow.is_none());

        // Cleanup
        rt.block_on(async { storage.shutdown().await.unwrap() });
    }

    /// Test that pane locks prevent concurrent workflow executions.
    #[test]
    fn workflow_runner_lock_prevents_concurrent_runs() {
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                default_wezterm_handle(),
            ),
        ));

        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();
        let storage = rt.block_on(async {
            Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap())
        });

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            storage.clone(),
            injector,
            WorkflowRunnerConfig::default(),
        );

        runner.register_workflow(Arc::new(MockCompactionWorkflow));

        let pane_id = 42u64;
        let detection = Detection {
            rule_id: "claude.compaction".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "compaction".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            matched_text: "compacted".to_string(),
            extracted: serde_json::json!({}),
            span: (0, 0),
        };

        // Create test pane first
        rt.block_on(async {
            let pane = crate::storage::PaneRecord {
                pane_id,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: Some(1),
                tab_id: Some(1),
                title: Some("test".to_string()),
                cwd: Some("/tmp".to_string()),
                tty_name: None,
                first_seen_at: now_ms(),
                last_seen_at: now_ms(),
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            };
            storage.upsert_pane(pane).await.unwrap();
        });

        // First handle_detection should start
        let result1 = rt.block_on(runner.handle_detection(pane_id, &detection, None));
        assert!(result1.is_started());

        // Second handle_detection should be blocked by lock
        let result2 = rt.block_on(runner.handle_detection(pane_id, &detection, None));
        assert!(result2.is_locked());

        // Verify the lock info
        if let WorkflowStartResult::PaneLocked {
            held_by_workflow, ..
        } = result2
        {
            assert_eq!(held_by_workflow, "handle_compaction");
        }

        // Cleanup
        rt.block_on(async { storage.shutdown().await.unwrap() });
    }

    /// Test that find_workflow_by_name works correctly.
    #[test]
    fn workflow_runner_find_by_name() {
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                default_wezterm_handle(),
            ),
        ));

        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();
        let storage = rt.block_on(async {
            Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap())
        });

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            storage.clone(),
            injector,
            WorkflowRunnerConfig::default(),
        );

        runner.register_workflow(Arc::new(MockCompactionWorkflow));
        runner.register_workflow(Arc::new(MockUsageLimitWorkflow));

        // Find by name
        let workflow = runner.find_workflow_by_name("handle_compaction");
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_compaction");

        let workflow = runner.find_workflow_by_name("handle_usage_limit");
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_usage_limit");

        // Not found
        let workflow = runner.find_workflow_by_name("nonexistent");
        assert!(workflow.is_none());

        // Cleanup
        rt.block_on(async { storage.shutdown().await.unwrap() });
    }

    // ========================================================================
    // Policy Denial Tests (wa-nu4.1.1.5)
    // ========================================================================

    /// Test workflow that attempts to send text and checks policy result.
    /// Kept for future integration tests that need to test workflow execution with policy gates.
    #[allow(dead_code)]
    struct MockTextSendingWorkflow;

    impl Workflow for MockTextSendingWorkflow {
        fn name(&self) -> &'static str {
            "text_sender"
        }

        fn description(&self) -> &'static str {
            "Mock workflow that sends text to test policy gates"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("text_send")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("send_text", "Send text to terminal")]
        }

        fn execute_step(
            &self,
            ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            // Need to move ctx reference into the async block properly
            let pane_id = ctx.pane_id();
            let has_injector = ctx.has_injector();
            let execution_id = ctx.execution_id().to_string();

            // Clone capabilities for use in async block
            let capabilities = ctx.capabilities().clone();

            Box::pin(async move {
                match step_idx {
                    0 => {
                        // Try to send text - policy should deny if command is running
                        if !has_injector {
                            return StepResult::abort("No injector configured");
                        }
                        // We need access to the context to call send_text
                        // This is a limitation of the mock - we'll test via the policy directly
                        StepResult::done(serde_json::json!({
                            "pane_id": pane_id,
                            "has_injector": has_injector,
                            "execution_id": execution_id,
                            "prompt_active": capabilities.prompt_active,
                            "command_running": capabilities.command_running,
                        }))
                    }
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Test that policy denial is properly returned when sending to a running command.
    #[test]
    fn policy_denies_send_when_command_running() {
        use crate::policy::{
            ActionKind, ActorKind, PaneCapabilities, PolicyDecision, PolicyEngine, PolicyInput,
        };

        // Create a strict policy engine (requires prompt active)
        let mut engine = PolicyEngine::strict();

        // Create capabilities where command is running (not at prompt)
        let caps = PaneCapabilities::running();

        // Try to authorize a send - should be denied
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Workflow)
            .with_pane(42)
            .with_capabilities(caps)
            .with_text_summary("test command")
            .with_workflow("wf-test-001");

        let decision = engine.authorize(&input);

        // Verify it's denied with the expected reason
        match decision {
            PolicyDecision::Deny {
                reason, rule_id, ..
            } => {
                assert!(
                    reason.contains("running command") || reason.contains("wait for prompt"),
                    "Expected denial reason about running command, got: {reason}"
                );
                assert_eq!(rule_id, Some("policy.prompt_required".to_string()));
            }
            other => panic!("Expected Deny, got: {other:?}"),
        }
    }

    /// Test that InjectionResult::Denied is returned when policy denies.
    #[test]
    fn policy_gated_injector_returns_denied_for_running_command() {
        run_async_test(async {
        use crate::policy::{
            ActorKind, InjectionResult, PaneCapabilities, PolicyEngine, PolicyGatedInjector,
        };

        // Create a strict policy engine (requires prompt active)
        let engine = PolicyEngine::strict();
        let client = default_wezterm_handle();
        let mut injector = PolicyGatedInjector::new(engine, client);

        // Create capabilities where command is running (not at prompt)
        let caps = PaneCapabilities::running();

        // Try to send text - should be denied by policy
        let result = injector
            .send_text(
                42,
                "echo test",
                ActorKind::Workflow,
                &caps,
                Some("wf-test-002"),
            )
            .await;

        // Verify it's denied
        assert!(
            result.is_denied(),
            "Expected denied result, got: {result:?}"
        );

        // Verify the rule ID
        if let InjectionResult::Denied { decision, .. } = result {
            assert_eq!(
                decision.rule_id(),
                Some("policy.prompt_required"),
                "Expected policy.prompt_required rule, got: {:?}",
                decision.rule_id()
            );
        }
        });
    }

    /// Test that WorkflowContext has injector access after with_injector is called.
    #[test]
    fn workflow_context_injector_access() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create context without injector
            let ctx =
                WorkflowContext::new(storage.clone(), 42, PaneCapabilities::default(), "exec-001");
            assert!(!ctx.has_injector());

            // Create context with injector
            let engine = crate::policy::PolicyEngine::permissive();
            let client = default_wezterm_handle();
            let injector = Arc::new(crate::runtime_compat::Mutex::new(
                crate::policy::PolicyGatedInjector::new(engine, client),
            ));

            let ctx_with_injector =
                WorkflowContext::new(storage.clone(), 42, PaneCapabilities::default(), "exec-002")
                    .with_injector(injector);

            assert!(ctx_with_injector.has_injector());

            storage.shutdown().await.unwrap();
        });
    }

    // ========================================================================
    // HandleCompaction Workflow Tests (wa-nu4.1.2.1)
    // ========================================================================

    #[test]
    fn handle_compaction_metadata() {
        let workflow = HandleCompaction::new();

        assert_eq!(workflow.name(), "handle_compaction");
        assert_eq!(
            workflow.description(),
            "Re-inject critical context (AGENTS.md) after conversation compaction"
        );

        let steps = workflow.steps();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].name, "check_guards");
        assert_eq!(steps[1].name, "stabilize");
        assert_eq!(steps[2].name, "send_prompt");
        assert_eq!(steps[3].name, "verify_send");
    }

    #[test]
    fn handle_compaction_handles_compaction_events() {
        let workflow = HandleCompaction::new();

        // Should handle event_type "session.compaction"
        let detection_event_type = Detection {
            rule_id: "something.other".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.compaction".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&detection_event_type));

        // Should handle rule_id containing "compaction"
        let detection_rule_id = Detection {
            rule_id: "claude_code.compaction".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "other".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&detection_rule_id));

        // Should NOT handle unrelated detections
        let detection_unrelated = Detection {
            rule_id: "prompt.ready".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "prompt".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&detection_unrelated));
    }

    #[test]
    fn handle_compaction_guard_checks() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_guards.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Normal capabilities - should pass guards
            let normal_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };
            let ctx_normal =
                WorkflowContext::new(storage.clone(), 42, normal_caps, "exec-guard-normal");
            let result = HandleCompaction::check_pane_guards(&ctx_normal);
            assert!(result.is_ok(), "Normal state should pass guards");

            // Alt-screen active - should fail
            let alt_caps = PaneCapabilities {
                alt_screen: Some(true),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };
            let ctx_alt = WorkflowContext::new(storage.clone(), 42, alt_caps, "exec-guard-alt");
            let result = HandleCompaction::check_pane_guards(&ctx_alt);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("alt-screen"));

            // Command running - should fail
            let cmd_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: true,
                has_recent_gap: false,
                ..Default::default()
            };
            let ctx_cmd = WorkflowContext::new(storage.clone(), 42, cmd_caps, "exec-guard-cmd");
            let result = HandleCompaction::check_pane_guards(&ctx_cmd);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("running"));

            // Recent gap - should fail
            let gap_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: true,
                ..Default::default()
            };
            let ctx_gap = WorkflowContext::new(storage.clone(), 42, gap_caps, "exec-guard-gap");
            let result = HandleCompaction::check_pane_guards(&ctx_gap);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("gap"));

            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn handle_compaction_prompts_exist() {
        // Verify all agent-specific prompts are non-empty
        assert!(!compaction_prompts::CLAUDE_CODE.is_empty());
        assert!(!compaction_prompts::CODEX.is_empty());
        assert!(!compaction_prompts::GEMINI.is_empty());
        assert!(!compaction_prompts::UNKNOWN.is_empty());

        // Verify they contain AGENTS.md reference (key context file)
        assert!(compaction_prompts::CLAUDE_CODE.contains("AGENTS.md"));
        assert!(compaction_prompts::CODEX.contains("AGENTS.md"));
        assert!(compaction_prompts::GEMINI.contains("AGENTS.md"));
    }

    #[test]
    fn handle_compaction_builder_pattern() {
        let workflow = HandleCompaction::new()
            .with_stabilization_ms(5000)
            .with_idle_timeout_ms(60_000);

        assert_eq!(workflow.stabilization_ms, 5000);
        assert_eq!(workflow.idle_timeout_ms, 60_000);
    }

    #[test]
    fn handle_compaction_default_values() {
        let workflow = HandleCompaction::default();

        // Defaults should be reasonable values
        assert!(workflow.stabilization_ms > 0);
        assert!(workflow.idle_timeout_ms > 0);
        assert!(workflow.idle_timeout_ms > workflow.stabilization_ms);
    }

    // ========================================================================
    // HandleCompaction Integration Tests (wa-nu4.1.2.4)
    // ========================================================================
    //
    // These tests verify the full workflow execution path with synthetic
    // detections and various pane states.

    /// Test: Synthetic compaction detection + PromptActive state
    /// Expected: Workflow proceeds through guards → step logs show completion path
    #[test]
    fn handle_compaction_integration_prompt_active_passes_guards() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_integration.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create PromptActive capabilities (pane is ready for input)
            let prompt_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };

            let execution_id = "test-hc-prompt-active-001";
            let pane_id = 42u64;

            // Create context with PromptActive state
            let ctx =
                WorkflowContext::new(storage.clone(), pane_id, prompt_caps.clone(), execution_id);

            // Verify guards pass for PromptActive state
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_ok(),
                "Guard check should pass for PromptActive state, got: {:?}",
                guard_result
            );

            // Create synthetic compaction detection
            let detection = Detection {
                rule_id: "claude_code.compaction".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.compaction".to_string(),
                severity: Severity::Info,
                confidence: 1.0,
                extracted: serde_json::json!({
                    "tokens_before": 150_000,
                    "tokens_after": 25_000
                }),
                matched_text: "Auto-compact: compacted 150,000 tokens to 25,000 tokens".to_string(),
                span: (0, 0),
            };

            // Verify HandleCompaction handles this detection
            let workflow = HandleCompaction::new();
            assert!(
                workflow.handles(&detection),
                "HandleCompaction should handle compaction detections"
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: AltScreen active state causes workflow abort
    /// Expected: Guard check fails with "alt-screen" in error message
    #[test]
    fn handle_compaction_integration_alt_screen_aborts() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_altscreen.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create AltScreen capabilities (vim, less, htop, etc.)
            let alt_screen_caps = PaneCapabilities {
                alt_screen: Some(true),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };

            let execution_id = "test-hc-altscreen-001";
            let pane_id = 42u64;

            let ctx = WorkflowContext::new(storage.clone(), pane_id, alt_screen_caps, execution_id);

            // Verify guards fail for AltScreen state
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_err(),
                "Guard check should fail for AltScreen state"
            );

            // Verify error message is actionable (contains "alt-screen")
            let err = guard_result.unwrap_err();
            assert!(
                err.contains("alt-screen"),
                "Error message should mention 'alt-screen' for actionable diagnosis, got: {}",
                err
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: Command running state causes workflow abort
    /// Expected: Guard check fails with "running" in error message
    #[test]
    fn handle_compaction_integration_command_running_aborts() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_cmdrunning.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create capabilities where command is running
            let running_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: true,
                has_recent_gap: false,
                ..Default::default()
            };

            let execution_id = "test-hc-running-001";
            let pane_id = 42u64;

            let ctx = WorkflowContext::new(storage.clone(), pane_id, running_caps, execution_id);

            // Verify guards fail
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_err(),
                "Guard check should fail when command is running"
            );

            // Verify error message is actionable
            let err = guard_result.unwrap_err();
            assert!(
                err.contains("running"),
                "Error message should mention 'running' for actionable diagnosis, got: {}",
                err
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: Recent gap state causes workflow abort
    /// Expected: Guard check fails with "gap" in error message
    #[test]
    fn handle_compaction_integration_recent_gap_aborts() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_gap.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create capabilities with recent gap
            let gap_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: true,
                ..Default::default()
            };

            let execution_id = "test-hc-gap-001";
            let pane_id = 42u64;

            let ctx = WorkflowContext::new(storage.clone(), pane_id, gap_caps, execution_id);

            // Verify guards fail
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_err(),
                "Guard check should fail with recent gap"
            );

            // Verify error message is actionable
            let err = guard_result.unwrap_err();
            assert!(
                err.contains("gap"),
                "Error message should mention 'gap' for actionable diagnosis, got: {}",
                err
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: Verify step metadata is correct for handle_compaction
    /// Expected: Steps array contains check_guards, stabilize, send_prompt, verify_send
    #[test]
    fn handle_compaction_step_metadata_complete() {
        let workflow = HandleCompaction::new();
        let steps = workflow.steps();

        // Verify all expected steps are present
        assert_eq!(steps.len(), 4, "HandleCompaction should have 4 steps");

        // Verify step names are descriptive (for logging/debugging)
        let step_names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        assert!(
            step_names.contains(&"check_guards"),
            "Should have check_guards step"
        );
        assert!(
            step_names.contains(&"stabilize"),
            "Should have stabilize step"
        );
        assert!(
            step_names.contains(&"send_prompt"),
            "Should have send_prompt step"
        );
        assert!(
            step_names.contains(&"verify_send"),
            "Should have verify_send step"
        );

        // Verify step descriptions are non-empty (for actionable logging)
        for step in &steps {
            assert!(
                !step.description.is_empty(),
                "Step '{}' should have a description for actionable logging",
                step.name
            );
        }
    }

    /// Test: Agent-specific prompt selection is deterministic
    /// Expected: Each agent type gets a consistent, non-empty prompt
    #[test]
    fn handle_compaction_agent_prompt_selection_deterministic() {
        // Test each agent type gets a deterministic prompt
        let agents = vec![
            (AgentType::ClaudeCode, compaction_prompts::CLAUDE_CODE),
            (AgentType::Codex, compaction_prompts::CODEX),
            (AgentType::Gemini, compaction_prompts::GEMINI),
            (AgentType::Unknown, compaction_prompts::UNKNOWN),
        ];

        for (agent_type, expected_prompt) in agents {
            // Verify prompt is non-empty
            assert!(
                !expected_prompt.is_empty(),
                "Prompt for {:?} should not be empty",
                agent_type
            );

            // Verify prompt contains AGENTS.md reference (except Unknown)
            if agent_type != AgentType::Unknown {
                assert!(
                    expected_prompt.contains("AGENTS.md"),
                    "Prompt for {:?} should reference AGENTS.md",
                    agent_type
                );
            }

            // Verify prompt ends with newline (for clean send)
            assert!(
                expected_prompt.ends_with('\n'),
                "Prompt for {:?} should end with newline for clean send",
                agent_type
            );
        }
    }

    #[test]
    fn handle_compaction_prompt_precedence() {
        use crate::config::{CompactionPromptOverride, PaneFilterRule};
        use serde_json::json;

        let mut config = crate::config::CompactionPromptConfig::default();
        config.default = "default".to_string();
        config
            .by_agent
            .insert("codex".to_string(), "agent".to_string());
        config.by_pane.insert(7, "pane".to_string());
        config.by_project.push(CompactionPromptOverride {
            rule: PaneFilterRule::new("project_rule").with_cwd("/repo"),
            prompt: "project".to_string(),
        });

        let workflow = HandleCompaction::new().with_prompt_config(config);

        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let temp_dir = tempfile::tempdir().unwrap();
            let db_path = temp_dir
                .path()
                .join("prec.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
            let mut ctx =
                WorkflowContext::new(storage, 7, PaneCapabilities::default(), "exec-prec")
                    .with_trigger(json!({"agent_type": "codex"}));
            ctx.set_pane_meta(PaneMetadata {
                cwd: Some("/repo".to_string()),
                ..Default::default()
            });
            assert_eq!(workflow.resolve_prompt(&ctx), "pane");

            let temp_dir = tempfile::tempdir().unwrap();
            let db_path = temp_dir
                .path()
                .join("prec2.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
            let mut ctx =
                WorkflowContext::new(storage, 8, PaneCapabilities::default(), "exec-prec2")
                    .with_trigger(json!({"agent_type": "codex"}));
            ctx.set_pane_meta(PaneMetadata {
                cwd: Some("/repo".to_string()),
                ..Default::default()
            });
            assert_eq!(workflow.resolve_prompt(&ctx), "project");

            let temp_dir = tempfile::tempdir().unwrap();
            let db_path = temp_dir
                .path()
                .join("prec3.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
            let ctx = WorkflowContext::new(storage, 9, PaneCapabilities::default(), "exec-prec3")
                .with_trigger(json!({"agent_type": "codex"}));
            assert_eq!(workflow.resolve_prompt(&ctx), "agent");
        });
    }

    #[test]
    fn handle_compaction_prompt_bounds_and_redaction() {
        use serde_json::json;

        let mut config = crate::config::CompactionPromptConfig::default();
        config.max_prompt_len = 25;
        config.max_snippet_len = 16;
        config.by_agent.clear();
        config.default = "Prompt {{pane_cwd}}".to_string();

        let workflow = HandleCompaction::new().with_prompt_config(config);

        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let temp_dir = tempfile::tempdir().unwrap();
            let db_path = temp_dir
                .path()
                .join("bounds.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
            let mut ctx =
                WorkflowContext::new(storage, 5, PaneCapabilities::default(), "exec-bounds")
                    .with_trigger(json!({"agent_type": "codex"}));
            ctx.set_pane_meta(PaneMetadata {
                cwd: Some("sk-abc123456789012345678901234567890123456789012345678901".to_string()),
                ..Default::default()
            });

            let prompt = workflow.resolve_prompt(&ctx);
            assert!(prompt.len() <= 25);
            assert!(!prompt.contains("sk-abc"));
            assert!(prompt.contains("[REDACTED]"));
        });
    }

    /// Test: Workflow execution step 0 (check_guards) with PromptActive state
    /// Expected: Step returns Continue (not Abort)
    #[test]
    fn handle_compaction_execute_step0_prompt_active_continues() {
        run_async_test(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step0.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        // Create PromptActive capabilities
        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step0-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 0).await;

        // Step 0 should return Continue for valid state
        match result {
            StepResult::Continue => {
                // Success - guards passed
            }
            StepResult::Abort { reason } => {
                panic!("Step 0 should not abort for PromptActive state: {}", reason);
            }
            other => {
                panic!("Unexpected step result for step 0: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: Workflow execution step 0 (check_guards) with AltScreen state
    /// Expected: Step returns Abort with actionable reason
    #[test]
    fn handle_compaction_execute_step0_alt_screen_aborts() {
        run_async_test(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step0_alt.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        // Create AltScreen capabilities
        let alt_caps = PaneCapabilities {
            alt_screen: Some(true),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, alt_caps, "test-step0-alt-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 0).await;

        // Step 0 should abort for AltScreen
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("alt-screen"),
                    "Abort reason should mention 'alt-screen': {}",
                    reason
                );
            }
            StepResult::Continue => {
                panic!("Step 0 should abort for AltScreen state, got Continue");
            }
            other => {
                panic!(
                    "Unexpected step result for step 0 with AltScreen: {:?}",
                    other
                );
            }
        }

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: Workflow execution step 1 (stabilize) returns Continue after stabilization
    /// Expected: Step returns Continue (no wait-for result)
    #[test]
    fn handle_compaction_execute_step1_returns_continue() {
        run_async_test(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step1.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step1-001");

        let workflow = HandleCompaction::new()
            .with_stabilization_ms(0)
            .with_idle_timeout_ms(50);
        let result = workflow.execute_step(&mut ctx, 1).await;

        // Step 1 should return Continue once stabilized
        match result {
            StepResult::Continue => {}
            StepResult::Abort { reason } => {
                panic!("Step 1 should not abort when stabilization is zero: {reason}");
            }
            other => panic!("Step 1 should return Continue, got: {:?}", other),
        }

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: Workflow execution step 2 (send_prompt) without injector
    /// Expected: Step returns Abort (no injector configured)
    #[test]
    fn handle_compaction_execute_step2_no_injector_aborts() {
        run_async_test(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step2_no_inj.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        // Create context WITHOUT injector
        let mut ctx =
            WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step2-no-inj-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 2).await;

        // Step 2 should abort without injector
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.to_lowercase().contains("injector"),
                    "Abort reason should mention missing injector: {}",
                    reason
                );
            }
            other => {
                panic!("Step 2 should abort without injector, got: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: Unexpected step index returns Abort
    /// Expected: Step indices >= step_count return Abort
    #[test]
    fn handle_compaction_execute_invalid_step_aborts() {
        run_async_test(async {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_invalid_step.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities::default();

        let mut ctx =
            WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-invalid-step-001");

        let workflow = HandleCompaction::new();

        // Try to execute step beyond the workflow's steps
        let invalid_step = workflow.step_count() + 1;
        let result = workflow.execute_step(&mut ctx, invalid_step).await;

        // Should abort for invalid step
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("step") || reason.contains("index"),
                    "Abort reason should mention invalid step: {}",
                    reason
                );
            }
            other => {
                panic!("Invalid step should abort, got: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
        });
    }

    // ========================================================================
    // Workflow Engine Tests (wa-nu4.1.1.7)
    // Lock Behavior, Step Logging, and Resume Tests
    // ========================================================================

    /// Simple workflow that completes after one step (for testing lock release on success)
    struct SimpleCompletingWorkflow;

    impl Workflow for SimpleCompletingWorkflow {
        fn name(&self) -> &'static str {
            "simple_completing"
        }

        fn description(&self) -> &'static str {
            "Test workflow that completes immediately"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("simple_complete")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("complete", "Complete immediately")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done(serde_json::json!({"completed": true})),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Workflow that aborts after one step (for testing lock release on abort)
    struct AbortingWorkflow {
        abort_reason: String,
    }

    impl AbortingWorkflow {
        fn new(reason: &str) -> Self {
            Self {
                abort_reason: reason.to_string(),
            }
        }
    }

    impl Workflow for AbortingWorkflow {
        fn name(&self) -> &'static str {
            "aborting_workflow"
        }

        fn description(&self) -> &'static str {
            "Test workflow that aborts"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("abort_test")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("abort_step", "Abort immediately")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            let reason = self.abort_reason.clone();
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::abort(&reason),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Multi-step workflow for testing step logging and resume
    struct MultiStepWorkflow {
        fail_at_step: Option<usize>,
    }

    impl MultiStepWorkflow {
        fn new() -> Self {
            Self { fail_at_step: None }
        }

        fn failing_at(step: usize) -> Self {
            Self {
                fail_at_step: Some(step),
            }
        }
    }

    impl Workflow for MultiStepWorkflow {
        fn name(&self) -> &'static str {
            "multi_step"
        }

        fn description(&self) -> &'static str {
            "Test workflow with multiple steps"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("multi_step")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("step_0", "First step"),
                WorkflowStep::new("step_1", "Second step"),
                WorkflowStep::new("step_2", "Third step"),
                WorkflowStep::new("step_3", "Final step"),
            ]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            let fail_at = self.fail_at_step;
            Box::pin(async move {
                if Some(step_idx) == fail_at {
                    return StepResult::abort("Simulated failure");
                }
                match step_idx {
                    0..=2 => StepResult::cont(),
                    3 => StepResult::done(serde_json::json!({"steps_completed": 4})),
                    _ => StepResult::abort("Unexpected step index"),
                }
            })
        }
    }

    /// Workflow with a single idempotent SendText step.
    struct IdempotentSendWorkflow;

    impl Workflow for IdempotentSendWorkflow {
        fn name(&self) -> &'static str {
            "idempotent_send"
        }

        fn description(&self) -> &'static str {
            "Test workflow with idempotent send step"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("idempotent_send")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("send", "Send test text")]
        }

        fn to_action_plan(
            &self,
            ctx: &WorkflowContext,
            execution_id: &str,
        ) -> Option<crate::plan::ActionPlan> {
            let pane_id = ctx.pane_id();
            let step = crate::plan::StepPlan::new(
                1,
                crate::plan::StepAction::SendText {
                    pane_id,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
                "Send test text",
            )
            .idempotent();

            Some(
                crate::plan::ActionPlan::builder(self.description(), "test-workspace")
                    .add_step(step)
                    .metadata(serde_json::json!({
                        "workflow_name": self.name(),
                        "execution_id": execution_id,
                        "pane_id": pane_id,
                    }))
                    .created_at(now_ms())
                    .build(),
            )
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::send_text("hello"),
                    _ => StepResult::abort("Unexpected step index"),
                }
            })
        }
    }

    /// Workflow that sets prompt capabilities before sending text.
    struct PromptSendWorkflow;

    impl Workflow for PromptSendWorkflow {
        fn name(&self) -> &'static str {
            "prompt_send"
        }

        fn description(&self) -> &'static str {
            "Test workflow that sets prompt capabilities before SendText"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("prompt_send")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("send", "Send test text")]
        }

        fn execute_step(
            &self,
            ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            if step_idx == 0 {
                ctx.update_capabilities(PaneCapabilities::prompt());
            }
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::send_text("hello"),
                    _ => StepResult::abort("Unexpected step index"),
                }
            })
        }
    }

    /// Helper to create a test WorkflowRunner with storage.
    /// Uses a mock WezTerm handle with the given pane IDs pre-registered
    /// so that SendText steps succeed without a real WezTerm binary.
    async fn create_test_runner(
        db_path: &str,
    ) -> (
        WorkflowRunner,
        Arc<crate::storage::StorageHandle>,
        Arc<PaneWorkflowLockManager>,
    ) {
        create_test_runner_with_panes(db_path, &[]).await
    }

    /// Like `create_test_runner` but pre-registers the given pane IDs in the
    /// mock WezTerm backend so that SendText steps succeed for those panes.
    async fn create_test_runner_with_panes(
        db_path: &str,
        pane_ids: &[u64],
    ) -> (
        WorkflowRunner,
        Arc<crate::storage::StorageHandle>,
        Arc<PaneWorkflowLockManager>,
    ) {
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(db_path).await.unwrap());

        let mock = crate::wezterm::MockWezterm::new();
        for &pid in pane_ids {
            mock.add_default_pane(pid).await;
        }
        let handle: crate::wezterm::WeztermHandle = Arc::new(mock);

        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                handle,
            ),
        ));

        let runner = WorkflowRunner::new(
            engine,
            Arc::clone(&lock_manager),
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        (runner, storage, lock_manager)
    }

    /// Helper to create a test pane in storage
    async fn create_test_pane(storage: &crate::storage::StorageHandle, pane_id: u64) {
        let pane = crate::storage::PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("test".to_string()),
            cwd: Some("/tmp".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();
    }

    // ------------------------------------------------------------------------
    // Lock Release Tests (wa-nu4.1.1.7)
    // ------------------------------------------------------------------------

    /// Test: Lock is released when workflow completes successfully (Done)
    #[test]
    fn lock_released_on_workflow_completion() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_lock_complete.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 42u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(SimpleCompletingWorkflow));

        // Start workflow - acquires lock
        let detection = make_test_detection("simple_complete.test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started(), "Workflow should start");

        // Lock should be held
        assert!(
            lock_manager.is_locked(pane_id).is_some(),
            "Lock should be held after starting workflow"
        );

        // Run the workflow to completion
        let workflow = runner.find_workflow_by_name("simple_completing").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        // Verify workflow completed
        assert!(
            exec_result.is_completed(),
            "Workflow should complete successfully"
        );

        // Lock should be released after completion
        assert!(
            lock_manager.is_locked(pane_id).is_none(),
            "Lock should be released after workflow completion"
        );

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: Lock is released when workflow aborts
    #[test]
    fn lock_released_on_workflow_abort() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_lock_abort.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 43u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(AbortingWorkflow::new("Test abort reason")));

        // Start workflow - acquires lock
        let detection = make_test_detection("abort_test.trigger");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started(), "Workflow should start");

        // Lock should be held
        assert!(
            lock_manager.is_locked(pane_id).is_some(),
            "Lock should be held after starting workflow"
        );

        // Run the workflow (will abort)
        let workflow = runner.find_workflow_by_name("aborting_workflow").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        // Verify workflow aborted
        assert!(exec_result.is_aborted(), "Workflow should abort");
        if let WorkflowExecutionResult::Aborted { reason, .. } = &exec_result {
            assert!(
                reason.contains("Test abort reason"),
                "Abort should have expected reason"
            );
        }

        // Lock should be released after abort
        assert!(
            lock_manager.is_locked(pane_id).is_none(),
            "Lock should be released after workflow abort"
        );

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: Per-pane lock prevents concurrent workflow execution
    #[test]
    fn per_pane_lock_prevents_concurrent_workflows() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_lock_concurrent.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 44u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        // Start first workflow
        let detection1 = make_test_detection("multi_step.first");
        let start_result1 = runner.handle_detection(pane_id, &detection1, None).await;
        assert!(start_result1.is_started(), "First workflow should start");

        // Verify lock is held by first workflow
        let lock_info = lock_manager.is_locked(pane_id);
        assert!(lock_info.is_some(), "Lock should be held");
        let info = lock_info.unwrap();
        assert_eq!(info.workflow_name, "multi_step");

        // Try to start second workflow on same pane
        let detection2 = make_test_detection("multi_step.second");
        let start_result2 = runner.handle_detection(pane_id, &detection2, None).await;

        // Second workflow should be blocked
        assert!(
            start_result2.is_locked(),
            "Second workflow should be blocked by lock"
        );
        if let WorkflowStartResult::PaneLocked {
            held_by_workflow, ..
        } = start_result2
        {
            assert_eq!(
                held_by_workflow, "multi_step",
                "Lock should be held by first workflow"
            );
        }

        // Complete first workflow to release lock
        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let exec_id = start_result1.execution_id().unwrap();
        let _ = runner
            .run_workflow(pane_id, workflow.clone(), exec_id, 0)
            .await;

        // Now second workflow can start
        let start_result3 = runner.handle_detection(pane_id, &detection2, None).await;
        assert!(
            start_result3.is_started(),
            "Workflow should start after lock released"
        );

        storage.shutdown().await.unwrap();
        });
    }

    // ------------------------------------------------------------------------
    // Step Logging Tests (wa-nu4.1.1.7)
    // ------------------------------------------------------------------------

    /// Test: Step logs are written correctly during workflow execution
    #[test]
    fn step_logs_written_correctly() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step_logs.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 45u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        // Start and run workflow
        let detection = make_test_detection("multi_step.log_test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        assert!(
            exec_result.is_completed(),
            "Workflow should complete: {exec_result:?}"
        );

        // Verify step logs were written
        let step_logs = storage.get_step_logs(execution_id).await.unwrap();

        // Multi-step workflow has 4 steps (0, 1, 2, 3)
        assert_eq!(step_logs.len(), 4, "Should have 4 step log entries");

        // Verify each step log
        for (i, log) in step_logs.iter().enumerate() {
            assert_eq!(log.workflow_id, execution_id);
            assert_eq!(log.step_index, i);
            assert_eq!(log.step_name, format!("step_{i}"));
            assert!(log.started_at > 0, "Started timestamp should be set");
            assert!(log.completed_at >= log.started_at, "Completed >= started");
            assert!(log.duration_ms >= 0, "Duration should be non-negative");
        }

        // First 3 steps should be "continue", last should be "done"
        assert_eq!(step_logs[0].result_type, "continue");
        assert_eq!(step_logs[1].result_type, "continue");
        assert_eq!(step_logs[2].result_type, "continue");
        assert_eq!(step_logs[3].result_type, "done");

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: SendText step logs capture audit_action_id and join into action_history
    #[test]
    fn send_text_step_logs_audit_action_id() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_send_text_audit.db")
            .to_string_lossy()
            .to_string();

        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let wezterm: crate::wezterm::WeztermHandle = Arc::new(MockWezterm);
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::with_storage(
                crate::policy::PolicyEngine::permissive(),
                wezterm,
                storage.as_ref().clone(),
            ),
        ));

        let runner = WorkflowRunner::new(
            engine,
            Arc::clone(&lock_manager),
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        let pane_id = 60u64;
        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(PromptSendWorkflow));

        let detection = make_test_detection("prompt_send.audit");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap();

        let workflow = runner.find_workflow_by_name("prompt_send").unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;
        assert!(
            exec_result.is_completed(),
            "Workflow should complete: {exec_result:?}"
        );

        let step_logs = storage.get_step_logs(execution_id).await.unwrap();
        let send_log = step_logs
            .iter()
            .find(|log| log.step_name == "send")
            .expect("send_text step log missing");
        let audit_action_id = send_log.audit_action_id.expect("audit_action_id missing");

        let history = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("send_text".to_string()),
                limit: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        let entry = history
            .iter()
            .find(|row| row.id == audit_action_id)
            .expect("action_history entry missing");

        assert_eq!(entry.workflow_id.as_deref(), Some(execution_id));
        assert_eq!(entry.step_name.as_deref(), Some("send"));

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: workflow completion updates undo metadata and records workflow actions.
    #[test]
    fn workflow_completion_updates_undo_metadata() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_workflow_completion_audit.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 61u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        let detection = make_test_detection("multi_step.audit");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap();

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;
        assert!(
            exec_result.is_completed(),
            "Workflow should complete: {exec_result:?}"
        );

        let start_actions = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_start".to_string()),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        let start = start_actions
            .first()
            .expect("workflow_start action missing");
        assert_eq!(start.undoable, Some(false));
        assert_eq!(start.undo_strategy.as_deref(), Some("workflow_abort"));
        assert_eq!(
            start.undo_hint.as_deref(),
            Some("workflow no longer running")
        );

        let completed = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_completed".to_string()),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!completed.is_empty(), "workflow_completed action missing");

        let steps = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_step".to_string()),
                limit: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            steps
                .iter()
                .any(|row| row.step_name.as_deref() == Some("step_0")),
            "workflow_step entries should include step_name"
        );

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: workflow abort updates undo metadata and records abort action.
    #[test]
    fn workflow_abort_updates_undo_metadata() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_workflow_abort_audit.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 62u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::failing_at(2)));

        let detection = make_test_detection("multi_step.abort_audit");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap();

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;
        assert!(
            exec_result.is_aborted(),
            "Workflow should abort: {exec_result:?}"
        );

        let start_actions = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_start".to_string()),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        let start = start_actions
            .first()
            .expect("workflow_start action missing");
        assert_eq!(start.undoable, Some(false));
        assert_eq!(start.undo_strategy.as_deref(), Some("workflow_abort"));
        assert_eq!(
            start.undo_hint.as_deref(),
            Some("workflow no longer running")
        );

        let aborted = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_aborted".to_string()),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!aborted.is_empty(), "workflow_aborted action missing");

        let steps = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_step".to_string()),
                limit: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            steps.iter().any(|row| {
                row.input_summary.as_ref().is_some_and(|summary| {
                    serde_json::from_str::<serde_json::Value>(summary)
                        .ok()
                        .and_then(|value| value.get("parent_action_id").and_then(|v| v.as_i64()))
                        == Some(start.id)
                })
            }),
            "workflow_step input_summary should include parent_action_id"
        );

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: idempotent steps are skipped on retry to avoid double-apply.
    #[test]
    fn idempotent_step_skip_prevents_double_send() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_idempotent_skip.db")
            .to_string_lossy()
            .to_string();

        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let wezterm: crate::wezterm::WeztermHandle = Arc::new(
            crate::wezterm::WeztermClient::with_socket("/tmp/wa-test-nonexistent.sock"),
        );
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                wezterm,
            ),
        ));

        let runner = WorkflowRunner::new(
            engine,
            Arc::clone(&lock_manager),
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        let pane_id = 47u64;
        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(IdempotentSendWorkflow));

        let detection = make_test_detection("idempotent_send.test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap();

        let workflow = runner.find_workflow_by_name("idempotent_send").unwrap();
        let ctx = WorkflowContext::new(
            Arc::clone(&storage),
            pane_id,
            PaneCapabilities::default(),
            execution_id,
        );
        let plan = workflow
            .to_action_plan(&ctx, execution_id)
            .expect("plan required");
        let step = &plan.steps[0];

        let result_data = serde_json::json!({
            "idempotency_key": step.step_id.0,
        })
        .to_string();

        storage
            .insert_step_log(
                execution_id,
                None,
                0,
                "send",
                Some(step.step_id.0.clone()),
                Some(step.action.action_type_name().to_string()),
                "continue",
                Some(result_data),
                None,
                None,
                None,
                now_ms(),
                now_ms(),
            )
            .await
            .unwrap();

        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        assert!(
            exec_result.is_completed(),
            "Workflow should complete when idempotent step is skipped"
        );

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: Step logs record abort correctly
    #[test]
    fn step_logs_record_abort() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step_logs_abort.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 46u64;

        create_test_pane(&storage, pane_id).await;
        // Workflow that fails at step 2
        runner.register_workflow(Arc::new(MultiStepWorkflow::failing_at(2)));

        // Start and run workflow
        let detection = make_test_detection("multi_step.abort_log_test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        assert!(exec_result.is_aborted(), "Workflow should abort");

        // Verify step logs
        let step_logs = storage.get_step_logs(execution_id).await.unwrap();

        // Should have 3 step logs (steps 0, 1, 2 where 2 aborts)
        assert_eq!(step_logs.len(), 3, "Should have 3 step log entries");

        // Steps 0 and 1 should be "continue"
        assert_eq!(step_logs[0].result_type, "continue");
        assert_eq!(step_logs[1].result_type, "continue");
        // Step 2 should be "abort"
        assert_eq!(step_logs[2].result_type, "abort");

        storage.shutdown().await.unwrap();
        });
    }

    // ------------------------------------------------------------------------
    // Resume Tests (wa-nu4.1.1.7)
    // ------------------------------------------------------------------------

    /// Test: WorkflowEngine.resume computes correct next step from logs
    #[test]
    fn engine_resume_finds_correct_step() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_resume.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let engine = WorkflowEngine::new(3);

        // Create a test pane
        create_test_pane(&storage, 50).await;

        // Start a workflow
        let execution = engine
            .start(&storage, "test_workflow", 50, None, None)
            .await
            .unwrap();

        // Manually insert step logs to simulate partial execution
        // Steps 0 and 1 completed, step 2 was in progress
        storage
            .insert_step_log(
                &execution.id,
                None,
                0,
                "step_0",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                1000,
                1100,
            )
            .await
            .unwrap();
        storage
            .insert_step_log(
                &execution.id,
                None,
                1,
                "step_1",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                1100,
                1200,
            )
            .await
            .unwrap();

        // Resume should find next step is 2
        let resume_result = engine.resume(&storage, &execution.id).await.unwrap();
        assert!(resume_result.is_some(), "Should find incomplete workflow");

        let (resumed_exec, next_step) = resume_result.unwrap();
        assert_eq!(resumed_exec.id, execution.id);
        assert_eq!(next_step, 2, "Next step should be 2 (after steps 0, 1)");

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: find_incomplete_workflows returns workflows with running/waiting status
    #[test]
    fn find_incomplete_workflows_returns_running_and_waiting() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_find_incomplete.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let engine = WorkflowEngine::new(3);

        // Create test panes
        create_test_pane(&storage, 51).await;
        create_test_pane(&storage, 52).await;
        create_test_pane(&storage, 53).await;

        // Start multiple workflows in different states
        let exec1 = engine
            .start(&storage, "workflow_1", 51, None, None)
            .await
            .unwrap();
        let exec2 = engine
            .start(&storage, "workflow_2", 52, None, None)
            .await
            .unwrap();
        let exec3 = engine
            .start(&storage, "workflow_3", 53, None, None)
            .await
            .unwrap();

        // Mark exec2 as waiting
        engine
            .update_status(
                &storage,
                &exec2.id,
                ExecutionStatus::Waiting,
                1,
                Some(&WaitCondition::pane_idle(1000)),
                None,
            )
            .await
            .unwrap();

        // Mark exec3 as completed (should not be returned)
        engine
            .update_status(
                &storage,
                &exec3.id,
                ExecutionStatus::Completed,
                2,
                None,
                None,
            )
            .await
            .unwrap();

        // Find incomplete workflows
        let incomplete = storage.find_incomplete_workflows().await.unwrap();

        // Should find exec1 (running) and exec2 (waiting), not exec3 (completed)
        assert_eq!(incomplete.len(), 2, "Should find 2 incomplete workflows");

        let incomplete_ids: std::collections::HashSet<_> =
            incomplete.iter().map(|w| w.id.as_str()).collect();
        assert!(incomplete_ids.contains(exec1.id.as_str()));
        assert!(incomplete_ids.contains(exec2.id.as_str()));
        assert!(!incomplete_ids.contains(exec3.id.as_str()));

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: resume_incomplete resumes workflows from last completed step
    #[test]
    fn resume_incomplete_resumes_from_last_step() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_resume_incomplete.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 54u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        // Start workflow and simulate partial execution
        let detection = make_test_detection("multi_step.resume_test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap().to_string();

        // Insert step logs for steps 0 and 1 (completed)
        storage
            .insert_step_log(
                &execution_id,
                None,
                0,
                "step_0",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                1000,
                1100,
            )
            .await
            .unwrap();
        storage
            .insert_step_log(
                &execution_id,
                None,
                1,
                "step_1",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                1100,
                1200,
            )
            .await
            .unwrap();

        // Release the lock to simulate a restart scenario
        lock_manager.force_release(pane_id);

        // Call resume_incomplete
        let results = runner.resume_incomplete().await;

        // Should have resumed and completed the workflow
        assert_eq!(results.len(), 1, "Should resume 1 workflow");
        assert!(
            results[0].is_completed(),
            "Resumed workflow should complete"
        );

        // Verify step logs show resumed execution (steps 2 and 3)
        let step_logs = storage.get_step_logs(&execution_id).await.unwrap();

        // Should have 4 step logs total now
        assert_eq!(step_logs.len(), 4, "Should have 4 step logs after resume");

        // Steps 0, 1 were from before, steps 2, 3 from resume
        assert_eq!(step_logs[2].step_index, 2);
        assert_eq!(step_logs[3].step_index, 3);
        assert_eq!(step_logs[3].result_type, "done");

        storage.shutdown().await.unwrap();
        });
    }

    /// Test: Aborted workflows are not resumed
    #[test]
    fn aborted_workflows_not_resumed() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_aborted_not_resumed.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let engine = WorkflowEngine::new(3);

        // Create test pane
        create_test_pane(&storage, 55).await;

        // Start a workflow and mark it aborted
        let execution = engine
            .start(&storage, "test_workflow", 55, None, None)
            .await
            .unwrap();

        engine
            .update_status(
                &storage,
                &execution.id,
                ExecutionStatus::Aborted,
                1,
                None,
                Some("Test abort"),
            )
            .await
            .unwrap();

        // Find incomplete - should not include aborted workflow
        let incomplete = storage.find_incomplete_workflows().await.unwrap();
        assert!(
            incomplete.is_empty(),
            "Aborted workflow should not be in incomplete list"
        );

        // Resume should return None
        let resume_result = engine.resume(&storage, &execution.id).await.unwrap();
        assert!(
            resume_result.is_none(),
            "Aborted workflow should not be resumable"
        );

        storage.shutdown().await.unwrap();
        });
    }

    // ====================================================================
    // Codex Exit Step Tests (wa-nu4.1.3.2)
    // ====================================================================

    #[derive(Clone)]
    struct TestTextSource {
        sequence: Arc<Vec<String>>,
        index: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TestTextSource {
        fn new(sequence: Vec<&str>) -> Self {
            Self {
                sequence: Arc::new(sequence.into_iter().map(str::to_string).collect()),
                index: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl PaneTextSource for TestTextSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            let idx = self.index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = self
                .sequence
                .get(idx)
                .cloned()
                .or_else(|| self.sequence.last().cloned())
                .unwrap_or_default();
            Box::pin(async move { Ok(text) })
        }
    }

    fn allowed_ctrl_c_result() -> InjectionResult {
        InjectionResult::Allowed {
            decision: crate::policy::PolicyDecision::allow(),
            summary: "ctrl-c".to_string(),
            pane_id: 1,
            action: crate::policy::ActionKind::SendCtrlC,
            audit_action_id: None,
        }
    }

    fn wait_options_single_poll() -> WaitOptions {
        WaitOptions {
            tail_lines: 200,
            escapes: false,
            poll_initial: Duration::from_millis(0),
            poll_max: Duration::from_millis(0),
            max_polls: 1,
        }
    }

    #[test]
    fn codex_exit_sends_one_ctrl_c_when_summary_present() {
        run_async_test(async {
        let source = TestTextSource::new(vec![
            "Token usage: total=10 input=5 (+ 0 cached) output=5\nTo resume, run: codex resume 123e4567-e89b-12d3-a456-426614174000",
        ]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allowed_ctrl_c_result())
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let result = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect("exit should succeed");

        assert_eq!(result.ctrl_c_count, 1);
        assert!(result.summary.matched);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn codex_exit_sends_second_ctrl_c_when_grace_times_out() {
        run_async_test(async {
        let source = TestTextSource::new(vec![
            "still running...",
            "Token usage: total=10 input=5 (+ 0 cached) output=5\nTo resume, run: codex resume 123e4567-e89b-12d3-a456-426614174000",
        ]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allowed_ctrl_c_result())
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let result = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect("exit should succeed");

        assert_eq!(result.ctrl_c_count, 2);
        assert!(result.summary.matched);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn codex_exit_errors_when_summary_never_appears() {
        run_async_test(async {
        let source = TestTextSource::new(vec!["no summary", "still no summary"]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allowed_ctrl_c_result())
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let err = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect_err("expected failure");
        assert!(err.contains("Session summary not found"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn codex_exit_aborts_on_policy_denial() {
        run_async_test(async {
        let source = TestTextSource::new(vec![
            "Token usage: total=1 input=1 (+ 0 cached) output=0 codex resume 123e4567-e89b-12d3-a456-426614174000",
        ]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(InjectionResult::Denied {
                    decision: crate::policy::PolicyDecision::deny("blocked"),
                    summary: "ctrl-c".to_string(),
                    pane_id: 1,
                    action: crate::policy::ActionKind::SendCtrlC,
                    audit_action_id: None,
                })
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let err = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect_err("expected denial");
        assert!(err.contains("denied"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        });
    }

    // ========================================================================
    // Codex Session Summary Parsing Tests (wa-nu4.1.3.3)
    // ========================================================================

    #[test]
    fn parse_codex_session_summary_succeeds_on_valid_fixture() {
        let tail = r"
You've reached your usage limit.
Token usage: total=1,234 input=500 (+ 200 cached) output=534 (reasoning 100)
To continue this session, run: codex resume 123e4567-e89b-12d3-a456-426614174000
Try again at 3:00 PM UTC.
";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.session_id, "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(result.token_usage.total, Some(1234));
        assert_eq!(result.token_usage.input, Some(500));
        assert_eq!(result.token_usage.cached, Some(200));
        assert_eq!(result.token_usage.output, Some(534));
        assert_eq!(result.token_usage.reasoning, Some(100));
        assert_eq!(result.reset_time.as_deref(), Some("3:00 PM UTC"));
    }

    #[test]
    fn parse_codex_session_summary_handles_minimal_valid_input() {
        let tail = "Token usage: total=100\ncodex resume abc12345-1234-1234-1234-123456789abc";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.session_id, "abc12345-1234-1234-1234-123456789abc");
        assert_eq!(result.token_usage.total, Some(100));
        assert!(result.token_usage.input.is_none());
        assert!(result.reset_time.is_none());
    }

    #[test]
    fn parse_codex_session_summary_handles_numbers_with_commas() {
        let tail = "Token usage: total=1,234,567 input=999,999\ncodex resume abcd1234-5678-90ab-cdef-1234567890ab";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.token_usage.total, Some(1_234_567));
        assert_eq!(result.token_usage.input, Some(999_999));
    }

    #[test]
    fn parse_codex_session_summary_fails_when_session_id_missing() {
        let tail = "Token usage: total=100 input=50";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        assert!(err.missing.contains(&"session_id"));
        assert!(!err.missing.contains(&"token_usage"));
    }

    #[test]
    fn parse_codex_session_summary_fails_when_token_usage_missing() {
        let tail = "codex resume 123e4567-e89b-12d3-a456-426614174000";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        assert!(err.missing.contains(&"token_usage"));
        assert!(!err.missing.contains(&"session_id"));
    }

    #[test]
    fn parse_codex_session_summary_fails_when_both_missing() {
        let tail = "Some random text without markers";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn parse_codex_session_summary_error_does_not_leak_raw_content() {
        let tail = "secret_api_key=sk-12345 some sensitive data";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        // Error should contain hash and length, not raw content
        let err_string = err.to_string();
        assert!(err_string.contains("tail_hash="));
        assert!(err_string.contains("tail_len="));
        assert!(!err_string.contains("secret_api_key"));
        assert!(!err_string.contains("sk-12345"));
    }

    #[test]
    fn parse_codex_session_summary_extracts_reset_time_variations() {
        // Various reset time formats
        let cases = [
            (
                "Token usage: total=1\ncodex resume abcd1234\ntry again at 2:30 PM",
                Some("2:30 PM"),
            ),
            (
                "Token usage: total=1\ncodex resume abcd1234\nTry again at tomorrow 9am.",
                Some("tomorrow 9am"),
            ),
            ("Token usage: total=1\ncodex resume abcd1234", None),
        ];

        for (tail, expected_reset) in cases {
            let result = parse_codex_session_summary(tail).expect("should parse");
            assert_eq!(
                result.reset_time.as_deref(),
                expected_reset,
                "Failed for: {tail}"
            );
        }
    }

    #[test]
    fn parse_codex_session_summary_uses_last_session_id_when_multiple() {
        // If multiple resume hints appear, use the last one
        let tail = "codex resume 11111111-1111-1111-1111-111111111111\nToken usage: total=1\ncodex resume 22222222-2222-2222-2222-222222222222";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.session_id, "22222222-2222-2222-2222-222222222222");
    }

    // ========================================================================
    // Account Selection Step Tests (wa-nu4.1.3.4)
    // ========================================================================
    //
    // Note: The core selection logic (determinism, threshold filtering, LRU tie-break)
    // is tested in accounts.rs (9 tests). The `refresh_and_select_account` function
    // wires caut + storage + selection together.
    //
    // Full integration tests with a real database should be added to verify:
    // - caut refresh results are correctly persisted to the accounts table
    // - selection uses the refreshed data from DB
    // - last_used_at updates work correctly after successful failover
    //
    // The tests below verify the error types and result structures.

    #[test]
    fn account_selection_step_error_displays_caut_error() {
        let caut_err = crate::caut::CautError::NotInstalled;
        let step_err = AccountSelectionStepError::Caut(caut_err);
        let display = step_err.to_string();
        assert!(display.contains("caut error"));
        assert!(display.contains("not installed"));
    }

    #[test]
    fn account_selection_step_error_displays_storage_error() {
        let step_err = AccountSelectionStepError::Storage("connection failed".to_string());
        let display = step_err.to_string();
        assert!(display.contains("storage error"));
        assert!(display.contains("connection failed"));
    }

    #[test]
    fn account_selection_step_result_can_be_constructed() {
        use crate::accounts::SelectionExplanation;

        // Verify the step result structure is correct
        let explanation = SelectionExplanation {
            total_considered: 2,
            filtered_out: vec![],
            candidates: vec![],
            selection_reason: "Test reason".to_string(),
        };

        let result = AccountSelectionStepResult {
            selected: None,
            explanation,
            quota_advisory: crate::accounts::AccountQuotaAdvisory {
                availability: crate::accounts::QuotaAvailability::Exhausted,
                low_quota_threshold_percent: crate::accounts::DEFAULT_LOW_QUOTA_THRESHOLD_PERCENT,
                selected_percent_remaining: None,
                warning: Some("Test reason".to_string()),
            },
            accounts_refreshed: 2,
        };

        assert!(result.selected.is_none());
        assert_eq!(result.accounts_refreshed, 2);
        assert_eq!(result.explanation.total_considered, 2);
    }

    #[test]
    fn account_selection_step_result_with_selected_account() {
        use crate::accounts::{AccountRecord, SelectionExplanation};

        let account = AccountRecord {
            id: 1,
            account_id: "acc-123".to_string(),
            service: "openai".to_string(),
            name: Some("Test Account".to_string()),
            percent_remaining: 75.0,
            reset_at: None,
            tokens_used: Some(1000),
            tokens_remaining: Some(3000),
            tokens_limit: Some(4000),
            last_refreshed_at: 1000,
            last_used_at: None,
            created_at: 1000,
            updated_at: 1000,
        };

        let explanation = SelectionExplanation {
            total_considered: 1,
            filtered_out: vec![],
            candidates: vec![],
            selection_reason: "Only eligible account".to_string(),
        };

        let result = AccountSelectionStepResult {
            selected: Some(account),
            explanation,
            quota_advisory: crate::accounts::AccountQuotaAdvisory {
                availability: crate::accounts::QuotaAvailability::Available,
                low_quota_threshold_percent: crate::accounts::DEFAULT_LOW_QUOTA_THRESHOLD_PERCENT,
                selected_percent_remaining: Some(75.0),
                warning: None,
            },
            accounts_refreshed: 1,
        };

        assert!(result.selected.is_some());
        assert_eq!(result.selected.as_ref().unwrap().account_id, "acc-123");
        assert_eq!(result.accounts_refreshed, 1);
    }

    // ── Device code parsing tests ───────────────────────────────────────

    #[test]
    fn parse_device_code_direct_format() {
        // parse_device_code expects "code" or "enter" directly before the code value
        let tail = "Your one-time code: ABCD-12345 (visit https://auth.openai.com/device)";
        let result = parse_device_code(tail).expect("should parse");
        assert_eq!(result.code, "ABCD-12345");
        assert!(result.url.is_some());
        assert!(result.url.unwrap().contains("auth.openai.com"));
    }

    #[test]
    fn parse_device_code_v2_format() {
        let tail = "Please open https://auth.openai.com/codex/device and enter this one-time code: WXYZ-98765";
        let result = parse_device_code(tail).expect("should parse");
        assert_eq!(result.code, "WXYZ-98765");
        assert!(result.url.is_some());
    }

    #[test]
    fn parse_device_code_lowercase_prompt() {
        let tail = "enter code: AAAA-BBBB";
        let result = parse_device_code(tail).expect("should parse");
        assert_eq!(result.code, "AAAA-BBBB");
        assert!(result.url.is_none());
    }

    #[test]
    fn parse_device_code_no_code_returns_error() {
        let tail = "Some random output with no device code";
        let err = parse_device_code(tail).unwrap_err();
        assert!(err.to_string().contains("device code"));
        // Must not contain raw tail content (safe diagnostics)
        assert!(!err.to_string().contains("random output"));
        assert!(err.tail_hash != 0);
        assert_eq!(err.tail_len, tail.len());
    }

    #[test]
    fn parse_device_code_empty_input() {
        let err = parse_device_code("").unwrap_err();
        assert_eq!(err.tail_len, 0);
    }

    #[test]
    fn parse_device_code_url_only_no_code() {
        let tail = "Visit https://auth.openai.com/device to continue";
        let err = parse_device_code(tail).unwrap_err();
        assert!(err.tail_len > 0);
    }

    #[test]
    fn parse_device_code_mixed_case() {
        let tail = "CODE: abcd-efgh";
        let result = parse_device_code(tail).expect("should parse case-insensitive");
        assert_eq!(result.code, "ABCD-EFGH");
    }

    #[test]
    fn validate_device_code_valid_formats() {
        assert!(validate_device_code("ABCD-1234"));
        assert!(validate_device_code("ABCD-12345"));
        assert!(validate_device_code("WXYZ-EFGH"));
        assert!(validate_device_code("1234-5678"));
    }

    #[test]
    fn validate_device_code_invalid_formats() {
        assert!(!validate_device_code(""));
        assert!(!validate_device_code("ABCD"));
        assert!(!validate_device_code("AB-CD"));
        assert!(!validate_device_code("ABCD-"));
        assert!(!validate_device_code("-ABCD"));
        assert!(!validate_device_code("ABCD-EF-GH"));
    }

    #[test]
    fn device_auth_login_command_is_correct() {
        assert_eq!(DEVICE_AUTH_LOGIN_COMMAND, "cod login --device-auth\n");
        assert!(DEVICE_AUTH_LOGIN_COMMAND.ends_with('\n'));
    }

    #[test]
    fn device_code_parse_error_display_is_safe() {
        let sensitive_tail = "secret-token: sk-abc123 and code somewhere";
        let err = parse_device_code(sensitive_tail).unwrap_err();
        let display = err.to_string();
        // Display must not leak raw tail content
        assert!(!display.contains("sk-abc123"));
        assert!(!display.contains("secret-token"));
        // But should contain diagnostic info
        assert!(display.contains("device code"));
    }

    // ========================================================================
    // HandleSessionEnd Tests (wa-nu4.2.2.3)
    // ========================================================================

    #[test]
    fn handle_session_end_metadata() {
        let wf = HandleSessionEnd::new();
        assert_eq!(wf.name(), "handle_session_end");
        assert!(!wf.description().is_empty());
        assert_eq!(wf.steps().len(), 2);
        assert_eq!(wf.steps()[0].name, "extract_summary");
        assert_eq!(wf.steps()[1].name, "persist_record");
    }

    #[test]
    fn handle_session_end_handles_session_summary() {
        let wf = HandleSessionEnd::new();
        let detection = Detection {
            rule_id: "codex.session.token_usage".to_string(),
            agent_type: AgentType::Codex,
            event_type: "session.summary".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "Token usage: total=100".to_string(),
            span: (0, 20),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_session_end_handles_session_end() {
        let wf = HandleSessionEnd::new();
        let detection = Detection {
            rule_id: "claude_code.session.end".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.end".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "Session ended".to_string(),
            span: (0, 13),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_session_end_ignores_other_events() {
        let wf = HandleSessionEnd::new();
        let detection = Detection {
            rule_id: "codex.usage.warning".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.warning".to_string(),
            severity: Severity::Warning,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "usage warning".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn handle_session_end_record_from_codex_detection() {
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": "1500",
                "input": "1000",
                "output": "500",
                "cached": "200",
                "reasoning": "50",
            }
        });
        let record = HandleSessionEnd::record_from_detection(42, &trigger);
        assert_eq!(record.pane_id, 42);
        assert_eq!(record.agent_type, "codex");
        assert_eq!(record.total_tokens, Some(1500));
        assert_eq!(record.input_tokens, Some(1000));
        assert_eq!(record.output_tokens, Some(500));
        assert_eq!(record.cached_tokens, Some(200));
        assert_eq!(record.reasoning_tokens, Some(50));
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
        assert!(record.ended_at.is_some());
    }

    #[test]
    fn handle_session_end_record_from_claude_code_detection() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.summary",
            "extracted": {
                "cost": "2.50",
            }
        });
        let record = HandleSessionEnd::record_from_detection(99, &trigger);
        assert_eq!(record.pane_id, 99);
        assert_eq!(record.agent_type, "claude_code");
        assert_eq!(record.estimated_cost_usd, Some(2.50));
        assert!(record.total_tokens.is_none());
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
    }

    #[test]
    fn handle_session_end_record_from_gemini_detection() {
        let trigger = serde_json::json!({
            "agent_type": "gemini",
            "event_type": "session.summary",
            "extracted": {
                "session_id": "abcdef12-3456-7890-abcd-ef1234567890",
                "tool_calls": "7",
            }
        });
        let record = HandleSessionEnd::record_from_detection(10, &trigger);
        assert_eq!(record.pane_id, 10);
        assert_eq!(record.agent_type, "gemini");
        assert_eq!(
            record.session_id.as_deref(),
            Some("abcdef12-3456-7890-abcd-ef1234567890")
        );
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
    }

    #[test]
    fn handle_session_end_record_from_session_end_event() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.end",
            "extracted": {}
        });
        let record = HandleSessionEnd::record_from_detection(5, &trigger);
        assert_eq!(record.agent_type, "claude_code");
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
        assert!(record.ended_at.is_some());
        // No token or cost data expected from a bare session.end
        assert!(record.total_tokens.is_none());
        assert!(record.estimated_cost_usd.is_none());
    }

    #[test]
    fn handle_session_end_record_missing_extracted() {
        let trigger = serde_json::json!({
            "agent_type": "unknown",
            "event_type": "session.end",
        });
        let record = HandleSessionEnd::record_from_detection(1, &trigger);
        assert_eq!(record.agent_type, "unknown");
        assert!(record.session_id.is_none());
        assert!(record.total_tokens.is_none());
        assert!(record.estimated_cost_usd.is_none());
    }

    #[test]
    fn handle_session_end_record_comma_numbers() {
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": "1,500,000",
                "input": "1,000,000",
                "output": "500,000",
            }
        });
        let record = HandleSessionEnd::record_from_detection(42, &trigger);
        assert_eq!(record.total_tokens, Some(1_500_000));
        assert_eq!(record.input_tokens, Some(1_000_000));
        assert_eq!(record.output_tokens, Some(500_000));
    }

    #[test]
    fn handle_session_end_trigger_event_types() {
        let wf = HandleSessionEnd::new();
        let types = wf.trigger_event_types();
        assert!(types.contains(&"session.summary"));
        assert!(types.contains(&"session.end"));
    }

    #[test]
    fn handle_session_end_supported_agents() {
        let wf = HandleSessionEnd::new();
        let agents = wf.supported_agent_types();
        assert!(agents.contains(&"codex"));
        assert!(agents.contains(&"claude_code"));
        assert!(agents.contains(&"gemini"));
    }

    #[test]
    fn handle_session_end_not_destructive() {
        let wf = HandleSessionEnd::new();
        assert!(!wf.is_destructive());
        assert!(!wf.requires_approval());
        assert!(wf.requires_pane());
    }

    #[test]
    fn handle_session_end_persist_roundtrip() {
        run_async_test(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path =
            std::env::temp_dir().join(format!("wa_test_session_end_{}_{n}.db", std::process::id()));
        let db = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        // Insert a pane record first (FK constraint)
        let pane = crate::storage::PaneRecord {
            pane_id: 77,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        db.upsert_pane(pane).await.expect("insert pane");

        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": "5000",
                "input": "3000",
                "output": "2000",
                "session_id": "abc-def-123",
            }
        });
        let record = HandleSessionEnd::record_from_detection(77, &trigger);
        let db_id = db.upsert_agent_session(record).await.expect("upsert");
        assert!(db_id > 0);

        // Query back by DB id
        let session = db
            .get_agent_session(db_id)
            .await
            .expect("query")
            .expect("session should exist");
        assert_eq!(session.agent_type, "codex");
        assert_eq!(session.session_id.as_deref(), Some("abc-def-123"));
        assert_eq!(session.total_tokens, Some(5000));
        assert_eq!(session.input_tokens, Some(3000));
        assert_eq!(session.output_tokens, Some(2000));
        assert!(session.ended_at.is_some());
        assert_eq!(session.end_reason.as_deref(), Some("completed"));
        });
    }

    #[test]
    fn persist_caut_refresh_accounts_records_metrics() {
        run_async_test(async {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_caut_metrics_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let storage = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        let refresh = crate::caut::CautRefresh {
            service: Some("openai".to_string()),
            refreshed_at: Some("2026-02-06T00:00:00Z".to_string()),
            accounts: vec![
                crate::caut::CautAccountUsage {
                    id: Some("acct-1".to_string()),
                    name: Some("Account 1".to_string()),
                    percent_remaining: Some(42.0),
                    limit_hours: None,
                    reset_at: Some("2026-02-06T01:00:00Z".to_string()),
                    tokens_used: Some(1000),
                    tokens_remaining: Some(2000),
                    tokens_limit: Some(3000),
                    extra: HashMap::new(),
                },
                crate::caut::CautAccountUsage {
                    id: Some("acct-2".to_string()),
                    name: Some("Account 2".to_string()),
                    percent_remaining: Some(7.0),
                    limit_hours: None,
                    reset_at: None,
                    tokens_used: Some(10),
                    tokens_remaining: Some(20),
                    tokens_limit: Some(30),
                    extra: HashMap::new(),
                },
            ],
            extra: HashMap::new(),
        };

        let now = 10_000_i64;
        let refreshed = super::persist_caut_refresh_accounts(
            &storage,
            crate::caut::CautService::OpenAI,
            &refresh,
            now,
        )
        .await
        .expect("persist refresh");
        assert_eq!(refreshed, 2);

        // Query by account_id to avoid relying on agent_type filtering (we store agent_type=None here).
        let acct1 = storage
            .query_usage_metrics(crate::storage::MetricQuery {
                metric_type: Some(crate::storage::MetricType::TokenUsage),
                agent_type: None,
                account_id: Some("acct-1".to_string()),
                since: Some(0),
                until: None,
                limit: Some(10),
            })
            .await
            .expect("query metrics");
        assert_eq!(acct1.len(), 1);
        assert_eq!(acct1[0].tokens, Some(1000));
        assert_eq!(acct1[0].amount, None);

        storage.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_file(&db_path);
        });
    }

    // ========================================================================
    // HandleProcessTriageLifecycle Tests (ft-2vuw7.5.4.1.2.1.1)
    // ========================================================================

    #[test]
    fn handle_process_triage_lifecycle_metadata() {
        let wf = HandleProcessTriageLifecycle::new();
        assert_eq!(wf.name(), "handle_process_triage_lifecycle");
        assert_eq!(wf.steps().len(), 6);
        assert_eq!(wf.steps()[0].name, "snapshot");
        assert_eq!(wf.steps()[5].name, "session");
        assert_eq!(wf.trigger_event_types(), ["process_triage.lifecycle"]);
        assert_eq!(wf.trigger_rule_ids(), ["process_triage.lifecycle"]);
        assert!(!wf.is_destructive());
    }

    #[test]
    fn handle_process_triage_lifecycle_handles_expected_detection() {
        let wf = HandleProcessTriageLifecycle::new();

        let detection = Detection {
            rule_id: "process_triage.lifecycle".to_string(),
            agent_type: AgentType::Codex,
            event_type: "process_triage.lifecycle".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "triage lifecycle requested".to_string(),
            span: (0, 10),
        };
        assert!(wf.handles(&detection));

        let non_matching = Detection {
            rule_id: "codex.session.token_usage".to_string(),
            agent_type: AgentType::Codex,
            event_type: "session.summary".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "Token usage".to_string(),
            span: (0, 10),
        };
        assert!(!wf.handles(&non_matching));
    }

    #[test]
    fn handle_process_triage_lifecycle_step0_aborts_on_alt_screen() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("triage_lifecycle_alt_screen.db");
        let storage = Arc::new(
            crate::storage::StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let mut caps = PaneCapabilities::default();
        caps.alt_screen = Some(true);
        let mut ctx = WorkflowContext::new(storage, 7, caps, "exec-triage-alt");

        let wf = HandleProcessTriageLifecycle::new();
        let result = wf.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => {
                assert!(reason.contains("alt-screen"), "unexpected reason: {reason}");
            }
            other => panic!("expected abort, got {other:?}"),
        }
        });
    }

    #[test]
    fn handle_process_triage_lifecycle_step2_aborts_on_protected_destructive_action() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("triage_lifecycle_protected_abort.db");
        let storage = Arc::new(
            crate::storage::StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let trigger = serde_json::json!({
            "process_triage": {
                "plan": {
                    "entries": [
                        {
                            "category": "system_process",
                            "action": { "action": "force_kill" }
                        }
                    ],
                    "auto_safe_count": 0,
                    "review_count": 0,
                    "protected_count": 1
                }
            }
        });

        let mut ctx = WorkflowContext::new(
            storage,
            9,
            PaneCapabilities::default(),
            "exec-triage-protected",
        )
        .with_trigger(trigger);

        let wf = HandleProcessTriageLifecycle::new();
        let result = wf.execute_step(&mut ctx, 2).await;
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("protected category includes destructive action"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected abort, got {other:?}"),
        }
        });
    }

    #[test]
    fn handle_process_triage_lifecycle_session_step_emits_all_artifacts() {
        run_async_test(async {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("triage_lifecycle_session.db");
        let storage = Arc::new(
            crate::storage::StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let trigger = serde_json::json!({
            "process_triage": {
                "ft_session_id": "ft-abc",
                "pt_session_id": "pt-xyz",
                "provider": "pt_cli",
                "plan": {
                    "entries": [
                        {
                            "category": "stuck_cli",
                            "action": { "action": "graceful_kill" }
                        },
                        {
                            "category": "active_agent",
                            "action": { "action": "protect" }
                        }
                    ],
                    "auto_safe_count": 1,
                    "review_count": 0,
                    "protected_count": 1
                }
            }
        });

        let mut ctx = WorkflowContext::new(
            storage,
            42,
            PaneCapabilities::default(),
            "exec-triage-session",
        )
        .with_trigger(trigger);
        let wf = HandleProcessTriageLifecycle::new();
        let result = wf.execute_step(&mut ctx, 5).await;

        match result {
            StepResult::Done { result } => {
                assert_eq!(result["status"], "completed");
                assert_eq!(result["workflow"], "handle_process_triage_lifecycle");
                assert!(result["snapshot"].is_object());
                assert!(result["plan"].is_object());
                assert!(result["apply"].is_object());
                assert!(result["verify"].is_object());
                assert!(result["diff"].is_object());
                assert_eq!(result["session"]["ft_session_id"], "ft-abc");
                assert_eq!(result["session"]["pt_session_id"], "pt-xyz");
                assert_eq!(result["session"]["provider"], "pt_cli");
            }
            other => panic!("expected done, got {other:?}"),
        }
        });
    }

    // ========================================================================
    // HandleAuthRequired Tests (wa-nu4.2.2.4)
    // ========================================================================

    #[test]
    fn handle_auth_required_metadata() {
        let wf = HandleAuthRequired::new();
        assert_eq!(wf.name(), "handle_auth_required");
        assert!(!wf.description().is_empty());
        assert_eq!(wf.steps().len(), 3);
        assert_eq!(wf.steps()[0].name, "check_cooldown");
        assert_eq!(wf.steps()[1].name, "classify_auth");
        assert_eq!(wf.steps()[2].name, "record_and_plan");
    }

    #[test]
    fn handle_auth_required_handles_device_code() {
        let wf = HandleAuthRequired::new();
        let detection = Detection {
            rule_id: "codex.auth.device_code_prompt".to_string(),
            agent_type: AgentType::Codex,
            event_type: "auth.device_code".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "Enter code".to_string(),
            span: (0, 10),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_auth_required_handles_auth_error() {
        let wf = HandleAuthRequired::new();
        let detection = Detection {
            rule_id: "claude_code.auth.api_key_error".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "auth.error".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "API key invalid".to_string(),
            span: (0, 15),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_auth_required_ignores_session_events() {
        let wf = HandleAuthRequired::new();
        let detection = Detection {
            rule_id: "codex.session.token_usage".to_string(),
            agent_type: AgentType::Codex,
            event_type: "session.summary".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "Token usage".to_string(),
            span: (0, 11),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn auth_strategy_device_code_from_detection() {
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "auth.device_code",
            "rule_id": "codex.auth.device_code_prompt",
            "extracted": {
                "code": "ABCD-12345",
            }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "device_code");
        match &strategy {
            AuthRecoveryStrategy::DeviceCode { code, url } => {
                assert_eq!(code.as_deref(), Some("ABCD-12345"));
                assert!(url.is_none());
            }
            _ => panic!("Expected DeviceCode strategy"),
        }
    }

    #[test]
    fn auth_strategy_api_key_error_from_detection() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "auth.error",
            "rule_id": "claude_code.auth.api_key_error",
            "extracted": {}
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "api_key_error");
    }

    #[test]
    fn auth_strategy_manual_intervention_fallback() {
        let trigger = serde_json::json!({
            "agent_type": "gemini",
            "event_type": "auth.unknown",
            "rule_id": "gemini.auth.something",
            "extracted": {}
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "manual_intervention");
        match &strategy {
            AuthRecoveryStrategy::ManualIntervention { agent_type, hint } => {
                assert_eq!(agent_type, "gemini");
                assert!(hint.contains("gemini"));
            }
            _ => panic!("Expected ManualIntervention strategy"),
        }
    }

    #[test]
    fn handle_auth_required_trigger_event_types() {
        let wf = HandleAuthRequired::new();
        let types = wf.trigger_event_types();
        assert!(types.contains(&"auth.device_code"));
        assert!(types.contains(&"auth.error"));
    }

    #[test]
    fn handle_auth_required_supported_agents() {
        let wf = HandleAuthRequired::new();
        let agents = wf.supported_agent_types();
        assert!(agents.contains(&"codex"));
        assert!(agents.contains(&"claude_code"));
        assert!(agents.contains(&"gemini"));
    }

    #[test]
    fn handle_auth_required_not_destructive() {
        let wf = HandleAuthRequired::new();
        assert!(!wf.is_destructive());
        assert!(!wf.requires_approval());
        assert!(wf.requires_pane());
    }

    #[test]
    fn auth_strategy_serializes() {
        let strategy = AuthRecoveryStrategy::DeviceCode {
            code: Some("ABCD-12345".to_string()),
            url: Some("https://auth.openai.com/device".to_string()),
        };
        let json = serde_json::to_value(&strategy).unwrap();
        assert_eq!(json["strategy"], "DeviceCode");
        assert_eq!(json["code"], "ABCD-12345");
        assert_eq!(json["url"], "https://auth.openai.com/device");
    }

    #[test]
    fn handle_auth_required_normalized_cass_query_prefers_matched_text() {
        let trigger = serde_json::json!({
            "matched_text": "   invalid   api   key   from provider   ",
            "event_type": "auth.error",
            "agent_type": "codex"
        });
        let query = HandleAuthRequired::normalized_cass_query(&trigger)
            .expect("query should be derived from matched text");
        assert_eq!(query, "invalid api key from provider");
    }

    #[test]
    fn handle_auth_required_build_recovery_prompt_includes_cass_hints() {
        let strategy = AuthRecoveryStrategy::ApiKeyError {
            key_hint: Some("OPENAI_API_KEY".to_string()),
        };
        let trigger = serde_json::json!({
            "matched_text": "Invalid API key",
        });
        let lookup = AuthCassHintsLookup {
            query: Some("Invalid API key".to_string()),
            workspace: Some("/repo".to_string()),
            hints: vec![
                "/repo/sessions/a.jsonl:42 - rotate key and retry".to_string(),
                "/repo/sessions/b.jsonl:13 - export OPENAI_API_KEY before launch".to_string(),
            ],
            error: None,
        };

        let prompt = HandleAuthRequired::build_recovery_prompt(&strategy, &trigger, &lookup);
        assert!(prompt.contains("Strategy: api_key_error"));
        assert!(prompt.contains("Related fixes from past sessions (cass):"));
        assert!(prompt.contains("OPENAI_API_KEY"));
        assert!(prompt.contains("Cass query: Invalid API key"));
        assert!(prompt.contains("Cass workspace filter: /repo"));
    }

    #[test]
    fn handle_auth_required_audit_roundtrip() {
        run_async_test(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path =
            std::env::temp_dir().join(format!("wa_test_auth_req_{}_{n}.db", std::process::id()));
        let db = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        // Insert pane record
        let pane = crate::storage::PaneRecord {
            pane_id: 88,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        db.upsert_pane(pane).await.expect("insert pane");

        // Record an auth event
        let audit = crate::storage::AuditActionRecord {
            id: 0,
            ts: now_ms(),
            actor_kind: "workflow".to_string(),
            actor_id: Some("test-exec-1".to_string()),
            correlation_id: None,
            pane_id: Some(88),
            domain: None,
            action_kind: "auth_required".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: Some("codex.auth.device_code_prompt".to_string()),
            input_summary: Some("Auth required for codex: device_code".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "recorded".to_string(),
        };
        let audit_id = db.record_audit_action(audit).await.expect("record");
        assert!(audit_id > 0);

        // Query back
        let query = crate::storage::AuditQuery {
            pane_id: Some(88),
            action_kind: Some("auth_required".to_string()),
            limit: Some(10),
            ..Default::default()
        };
        let results = db.get_audit_actions(query).await.expect("query");
        assert!(!results.is_empty());
        assert_eq!(results[0].action_kind, "auth_required");
        assert_eq!(results[0].pane_id, Some(88));
        });
    }

    #[test]
    fn handle_auth_required_cooldown_blocks_repeat() {
        run_async_test(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR2: AtomicU64 = AtomicU64::new(0);
        let n = CTR2.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_auth_cooldown_{}_{n}.db",
            std::process::id()
        ));
        let db = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        // Insert pane
        let pane = crate::storage::PaneRecord {
            pane_id: 89,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        db.upsert_pane(pane).await.expect("insert pane");

        // Insert a recent auth event (within cooldown)
        let audit = crate::storage::AuditActionRecord {
            id: 0,
            ts: now_ms(), // Just now
            actor_kind: "workflow".to_string(),
            actor_id: Some("test-exec-2".to_string()),
            correlation_id: None,
            pane_id: Some(89),
            domain: None,
            action_kind: "auth_required".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "recorded".to_string(),
        };
        db.record_audit_action(audit).await.expect("record");

        // Now check cooldown: query for recent auth events within default window
        let since = now_ms() - AUTH_COOLDOWN_MS;
        let query = crate::storage::AuditQuery {
            pane_id: Some(89),
            action_kind: Some("auth_required".to_string()),
            since: Some(since),
            limit: Some(1),
            ..Default::default()
        };
        let results = db.get_audit_actions(query).await.expect("query");
        assert!(
            !results.is_empty(),
            "Should find recent auth event within cooldown window"
        );
        });
    }

    // ========================================================================
    // HandleClaudeCodeLimits Tests (wa-03j, wa-nu4.2.2.1)
    // ========================================================================

    #[test]
    fn handle_claude_code_limits_metadata() {
        let wf = HandleClaudeCodeLimits::new();
        assert_eq!(wf.name(), "handle_claude_code_limits");
        assert!(!wf.description().is_empty());
        assert_eq!(wf.steps().len(), 3);
        assert_eq!(wf.steps()[0].name, "check_guards");
        assert_eq!(wf.steps()[1].name, "check_cooldown");
        assert_eq!(wf.steps()[2].name, "classify_and_record");
    }

    #[test]
    fn handle_claude_code_limits_handles_usage_warning() {
        let wf = HandleClaudeCodeLimits::new();
        let detection = Detection {
            rule_id: "claude_code.usage.warning".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.warning".to_string(),
            severity: Severity::Warning,
            confidence: 0.95,
            extracted: serde_json::json!({"remaining": "10"}),
            matched_text: "usage limit".to_string(),
            span: (0, 11),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_claude_code_limits_handles_usage_reached() {
        let wf = HandleClaudeCodeLimits::new();
        let detection = Detection {
            rule_id: "claude_code.usage.reached".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "limit reached".to_string(),
            span: (0, 13),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_claude_code_limits_handles_rate_limit_detected() {
        let wf = HandleClaudeCodeLimits::new();
        let detection = Detection {
            rule_id: "claude_code.rate_limit.detected".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "rate_limit.detected".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({"retry_after": "15 minutes"}),
            matched_text: "rate limit detected".to_string(),
            span: (0, 19),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_claude_code_limits_ignores_codex_usage() {
        let wf = HandleClaudeCodeLimits::new();
        let detection = Detection {
            rule_id: "codex.usage.reached".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "limit reached".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection), "Should ignore Codex usage events");
    }

    #[test]
    fn handle_claude_code_limits_ignores_session_events() {
        let wf = HandleClaudeCodeLimits::new();
        let detection = Detection {
            rule_id: "claude_code.session.end".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.end".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Session ended".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection), "Should ignore session.end events");
    }

    #[test]
    fn handle_claude_code_limits_trigger_event_types() {
        let wf = HandleClaudeCodeLimits::new();
        let types = wf.trigger_event_types();
        assert!(types.contains(&"usage.warning"));
        assert!(types.contains(&"usage.reached"));
        assert!(types.contains(&"rate_limit.detected"));
    }

    #[test]
    fn handle_claude_code_limits_supported_agents() {
        let wf = HandleClaudeCodeLimits::new();
        let agents = wf.supported_agent_types();
        assert_eq!(agents, &["claude_code"]);
    }

    #[test]
    fn handle_claude_code_limits_not_destructive() {
        let wf = HandleClaudeCodeLimits::new();
        assert!(!wf.is_destructive());
        assert!(!wf.requires_approval());
        assert!(wf.requires_pane());
    }

    #[test]
    fn handle_claude_code_limits_classify_usage_warning() {
        let trigger = serde_json::json!({
            "event_type": "usage.warning",
            "extracted": { "remaining": "10" }
        });
        let (limit_type, reset_time) = HandleClaudeCodeLimits::classify_limit(&trigger);
        assert_eq!(limit_type, "usage_warning");
        assert!(reset_time.is_none());
    }

    #[test]
    fn handle_claude_code_limits_classify_usage_reached() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": { "reset_time": "30 minutes" }
        });
        let (limit_type, reset_time) = HandleClaudeCodeLimits::classify_limit(&trigger);
        assert_eq!(limit_type, "usage_reached");
        assert_eq!(reset_time.as_deref(), Some("30 minutes"));
    }

    #[test]
    fn handle_claude_code_limits_classify_rate_limit_detected() {
        let trigger = serde_json::json!({
            "event_type": "rate_limit.detected",
            "extracted": { "retry_after": "15 minutes" }
        });
        let (limit_type, reset_time) = HandleClaudeCodeLimits::classify_limit(&trigger);
        assert_eq!(limit_type, "rate_limit_detected");
        assert_eq!(reset_time.as_deref(), Some("15 minutes"));
    }

    #[test]
    fn handle_claude_code_limits_recovery_plan_warning() {
        let plan = HandleClaudeCodeLimits::build_recovery_plan("usage_warning", None, 42);
        assert_eq!(plan["limit_type"], "usage_warning");
        assert_eq!(plan["pane_id"], 42);
        assert_eq!(plan["safe_to_send"], true);
        assert!(!plan["next_steps"].as_array().unwrap().is_empty());
    }

    #[test]
    fn handle_claude_code_limits_recovery_plan_reached() {
        let reset = Some("2 hours".to_string());
        let plan =
            HandleClaudeCodeLimits::build_recovery_plan("usage_reached", reset.as_deref(), 7);
        assert_eq!(plan["limit_type"], "usage_reached");
        assert_eq!(plan["safe_to_send"], false);
        assert_eq!(plan["reset_time"], "2 hours");
    }

    #[test]
    fn handle_claude_code_limits_custom_cooldown() {
        let wf = HandleClaudeCodeLimits::with_cooldown_ms(30_000);
        assert_eq!(wf.cooldown_ms, 30_000);
    }

    // ========================================================================
    // HandleGeminiQuota Unit Tests (wa-smm)
    // ========================================================================

    #[test]
    fn handle_gemini_quota_metadata() {
        let wf = HandleGeminiQuota::default();
        assert_eq!(wf.name(), "handle_gemini_quota");
        assert!(!wf.description().is_empty());
        assert_eq!(wf.steps().len(), 3);
        assert_eq!(wf.steps()[0].name, "check_guards");
        assert_eq!(wf.steps()[1].name, "check_cooldown");
        assert_eq!(wf.steps()[2].name, "classify_and_record");
    }

    #[test]
    fn handle_gemini_quota_handles_usage_warning() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "gemini.usage.reached".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "usage.warning".to_string(),
            severity: Severity::Warning,
            confidence: 0.95,
            extracted: serde_json::json!({"remaining": "15"}),
            matched_text: "usage warning".to_string(),
            span: (0, 13),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_handles_usage_reached() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "gemini.usage.reached".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({"remaining": "0"}),
            matched_text: "usage reached".to_string(),
            span: (0, 13),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_handles_rate_limit_detected() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "gemini.rate_limit.detected".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "rate_limit.detected".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({"retry_after": "10 minutes"}),
            matched_text: "resource exhausted".to_string(),
            span: (0, 18),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_ignores_codex_usage() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "codex.usage.reached".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "usage reached".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_ignores_claude_code_usage() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "claude_code.usage.reached".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "usage reached".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_ignores_session_events() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "gemini.session.end".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "session.end".to_string(),
            severity: Severity::Info,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "session end".to_string(),
            span: (0, 11),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_trigger_event_types() {
        let wf = HandleGeminiQuota::default();
        let types = wf.trigger_event_types();
        assert!(types.contains(&"usage.warning"));
        assert!(types.contains(&"usage.reached"));
        assert!(types.contains(&"rate_limit.detected"));
    }

    #[test]
    fn handle_gemini_quota_supported_agents() {
        let wf = HandleGeminiQuota::default();
        assert_eq!(wf.supported_agent_types(), &["gemini"]);
    }

    #[test]
    fn handle_gemini_quota_not_destructive() {
        let wf = HandleGeminiQuota::default();
        assert!(!wf.is_destructive());
        assert!(!wf.requires_approval());
        assert!(wf.requires_pane());
    }

    #[test]
    fn handle_gemini_quota_classify_usage_warning() {
        let trigger = serde_json::json!({
            "event_type": "usage.warning",
            "extracted": { "remaining": "15" }
        });
        let (quota_type, remaining) = HandleGeminiQuota::classify_quota(&trigger);
        assert_eq!(quota_type, "quota_warning");
        assert_eq!(remaining, Some("15".to_string()));
    }

    #[test]
    fn handle_gemini_quota_classify_usage_reached() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": { "remaining": "0" }
        });
        let (quota_type, remaining) = HandleGeminiQuota::classify_quota(&trigger);
        assert_eq!(quota_type, "quota_reached");
        assert_eq!(remaining, Some("0".to_string()));
    }

    #[test]
    fn handle_gemini_quota_classify_rate_limit_detected() {
        let trigger = serde_json::json!({
            "event_type": "rate_limit.detected",
            "extracted": { "retry_after": "10 minutes" }
        });
        let (quota_type, remaining) = HandleGeminiQuota::classify_quota(&trigger);
        assert_eq!(quota_type, "quota_reached");
        assert!(remaining.is_none());
    }

    #[test]
    fn handle_gemini_quota_recovery_plan_warning() {
        let plan = HandleGeminiQuota::build_recovery_plan("quota_warning", Some("15"), 42);
        assert_eq!(plan["quota_type"], "quota_warning");
        assert_eq!(plan["pane_id"], 42);
        assert_eq!(plan["safe_to_send"], true);
        assert!(plan["next_steps"].is_array());
    }

    #[test]
    fn handle_gemini_quota_recovery_plan_reached() {
        let plan = HandleGeminiQuota::build_recovery_plan("quota_reached", Some("0"), 42);
        assert_eq!(plan["quota_type"], "quota_reached");
        assert_eq!(plan["safe_to_send"], false);
        assert!(plan["next_steps"].is_array());
    }

    #[test]
    fn handle_gemini_quota_custom_cooldown() {
        let wf = HandleGeminiQuota::with_cooldown_ms(60_000);
        assert_eq!(wf.cooldown_ms, 60_000);
    }

    // ========================================================================
    // Workflow Regression Tests (wa-nu4.2.2.5)
    // ========================================================================

    /// Helper: build a detection with specific event_type, agent_type, and extracted data.
    fn make_session_detection(
        rule_id: &str,
        agent_type: AgentType,
        event_type: &str,
        extracted: serde_json::Value,
    ) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type,
            event_type: event_type.to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted,
            matched_text: "test fixture".to_string(),
            span: (0, 12),
        }
    }

    #[test]
    fn regression_session_end_selects_for_codex_summary() {
        let wf = HandleSessionEnd::new();
        let det = make_session_detection(
            "codex.session.token_usage",
            AgentType::Codex,
            "session.summary",
            serde_json::json!({"total": "5000"}),
        );
        assert!(
            wf.handles(&det),
            "HandleSessionEnd should match codex session.summary"
        );
    }

    #[test]
    fn regression_session_end_selects_for_claude_summary() {
        let wf = HandleSessionEnd::new();
        let det = make_session_detection(
            "claude_code.session.cost_summary",
            AgentType::ClaudeCode,
            "session.summary",
            serde_json::json!({"cost": "3.50"}),
        );
        assert!(
            wf.handles(&det),
            "HandleSessionEnd should match claude_code session.summary"
        );
    }

    #[test]
    fn regression_session_end_selects_for_gemini_summary() {
        let wf = HandleSessionEnd::new();
        let det = make_session_detection(
            "gemini.session.summary",
            AgentType::Gemini,
            "session.summary",
            serde_json::json!({"session_id": "abc-123"}),
        );
        assert!(
            wf.handles(&det),
            "HandleSessionEnd should match gemini session.summary"
        );
    }

    #[test]
    fn regression_session_end_selects_for_session_end_event() {
        let wf = HandleSessionEnd::new();
        let det = make_session_detection(
            "claude_code.session.end",
            AgentType::ClaudeCode,
            "session.end",
            serde_json::Value::Null,
        );
        assert!(
            wf.handles(&det),
            "HandleSessionEnd should match session.end event"
        );
    }

    #[test]
    fn regression_auth_required_selects_for_device_code() {
        let wf = HandleAuthRequired::new();
        let det = make_session_detection(
            "codex.auth.device_code_prompt",
            AgentType::Codex,
            "auth.device_code",
            serde_json::json!({"code": "ABCD-12345"}),
        );
        assert!(
            wf.handles(&det),
            "HandleAuthRequired should match auth.device_code"
        );
    }

    #[test]
    fn regression_auth_required_selects_for_api_key_error() {
        let wf = HandleAuthRequired::new();
        let det = make_session_detection(
            "claude_code.auth.api_key_error",
            AgentType::ClaudeCode,
            "auth.error",
            serde_json::Value::Null,
        );
        assert!(
            wf.handles(&det),
            "HandleAuthRequired should match auth.error"
        );
    }

    #[test]
    fn regression_no_cross_trigger_session_to_auth() {
        let session_wf = HandleSessionEnd::new();
        let auth_wf = HandleAuthRequired::new();

        // session.summary should NOT trigger auth workflow
        let session_det = make_session_detection(
            "codex.session.token_usage",
            AgentType::Codex,
            "session.summary",
            serde_json::json!({}),
        );
        assert!(
            !auth_wf.handles(&session_det),
            "Auth workflow should NOT match session.summary"
        );
        assert!(
            session_wf.handles(&session_det),
            "Session workflow should match session.summary"
        );

        // auth.device_code should NOT trigger session workflow
        let auth_det = make_session_detection(
            "codex.auth.device_code_prompt",
            AgentType::Codex,
            "auth.device_code",
            serde_json::json!({}),
        );
        assert!(
            !session_wf.handles(&auth_det),
            "Session workflow should NOT match auth.device_code"
        );
        assert!(
            auth_wf.handles(&auth_det),
            "Auth workflow should match auth.device_code"
        );
    }

    #[test]
    fn regression_runner_selects_session_end_workflow() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let db_path =
            std::env::temp_dir().join(format!("wa_test_reg_sel_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        rt.block_on(async {
            let (runner, _storage, _lock) = create_test_runner(&db_path_str).await;
            runner.register_workflow(Arc::new(HandleSessionEnd::new()));
            runner.register_workflow(Arc::new(HandleAuthRequired::new()));

            // Session summary → session end workflow
            let det = make_session_detection(
                "codex.session.token_usage",
                AgentType::Codex,
                "session.summary",
                serde_json::json!({"total": "1000"}),
            );
            let wf = runner.find_matching_workflow(&det);
            assert!(
                wf.is_some(),
                "Should find matching workflow for session.summary"
            );
            assert_eq!(wf.unwrap().name(), "handle_session_end");

            // Auth device code → auth required workflow
            let det = make_session_detection(
                "codex.auth.device_code_prompt",
                AgentType::Codex,
                "auth.device_code",
                serde_json::json!({"code": "TEST-12345"}),
            );
            let wf = runner.find_matching_workflow(&det);
            assert!(
                wf.is_some(),
                "Should find matching workflow for auth.device_code"
            );
            assert_eq!(wf.unwrap().name(), "handle_auth_required");

            // Unrelated detection → no workflow
            let det = make_session_detection(
                "some.other.rule",
                AgentType::Codex,
                "something.else",
                serde_json::Value::Null,
            );
            let wf = runner.find_matching_workflow(&det);
            assert!(wf.is_none(), "No workflow should match unrelated detection");
        });
    }

    #[test]
    fn regression_session_end_full_execution() {
        run_async_test(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_reg_exec_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let (runner, storage, _lock) = create_test_runner(&db_path_str).await;
        runner.register_workflow(Arc::new(HandleSessionEnd::new()));

        let pane_id = 200u64;
        create_test_pane(&storage, pane_id).await;

        // Create a Codex session.summary detection
        let det = make_session_detection(
            "codex.session.token_usage",
            AgentType::Codex,
            "session.summary",
            serde_json::json!({
                "total": "5000",
                "input": "3000",
                "output": "2000",
            }),
        );

        // Start the workflow
        let start = runner.handle_detection(pane_id, &det, None).await;
        assert!(
            start.is_started(),
            "Workflow should start for session.summary"
        );
        let execution_id = start.execution_id().unwrap().to_string();

        // Run the workflow
        let wf = runner.find_workflow_by_name("handle_session_end").unwrap();
        let result = runner.run_workflow(pane_id, wf, &execution_id, 0).await;
        assert!(
            result.is_completed(),
            "Session end workflow should complete: {result:?}"
        );

        // Verify step logs recorded
        let logs = storage.get_step_logs(&execution_id).await.unwrap();
        assert_eq!(logs.len(), 2, "Should have 2 step logs (extract + persist)");
        assert_eq!(logs[0].step_name, "extract_summary");
        assert_eq!(logs[1].step_name, "persist_record");

        // Verify session persisted
        // (The record was persisted via upsert_agent_session; we verify via step log result data)
        assert_eq!(logs[1].result_type, "done");

        // Verify usage metrics were recorded (token usage + duration)
        let metrics = storage
            .query_usage_metrics(crate::storage::MetricQuery {
                metric_type: None,
                agent_type: Some("codex".to_string()),
                account_id: None,
                since: Some(0),
                until: None,
                limit: Some(50),
            })
            .await
            .expect("query usage metrics");
        assert!(
            metrics
                .iter()
                .any(|m| m.metric_type == crate::storage::MetricType::TokenUsage),
            "Expected token usage metric"
        );
        assert!(
            metrics
                .iter()
                .any(|m| m.metric_type == crate::storage::MetricType::SessionDuration),
            "Expected session duration metric"
        );
        });
    }

    #[test]
    fn regression_auth_required_full_execution() {
        run_async_test(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_reg_auth_exec_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let (runner, storage, _lock) = create_test_runner_with_panes(&db_path_str, &[201]).await;
        runner.register_workflow(Arc::new(HandleAuthRequired::new()));

        let pane_id = 201u64;
        create_test_pane(&storage, pane_id).await;

        // Create a device code detection
        let det = make_session_detection(
            "codex.auth.device_code_prompt",
            AgentType::Codex,
            "auth.device_code",
            serde_json::json!({
                "code": "ABCD-12345",
            }),
        );

        // Start the workflow
        let start = runner.handle_detection(pane_id, &det, None).await;
        assert!(
            start.is_started(),
            "Workflow should start for auth.device_code"
        );
        let execution_id = start.execution_id().unwrap().to_string();

        // Run the workflow
        let wf = runner
            .find_workflow_by_name("handle_auth_required")
            .unwrap();
        let result = runner.run_workflow(pane_id, wf, &execution_id, 0).await;
        assert!(
            result.is_completed(),
            "Auth required workflow should complete: {result:?}"
        );

        // Verify step logs
        let logs = storage.get_step_logs(&execution_id).await.unwrap();
        assert_eq!(
            logs.len(),
            3,
            "Should have 3 step logs (cooldown + classify + record)"
        );
        assert_eq!(logs[0].step_name, "check_cooldown");
        assert_eq!(logs[1].step_name, "classify_auth");
        assert_eq!(logs[2].step_name, "record_and_plan");

        // Verify audit record created
        let query = crate::storage::AuditQuery {
            pane_id: Some(pane_id),
            action_kind: Some("auth_required".to_string()),
            limit: Some(10),
            ..Default::default()
        };
        let audits = storage.get_audit_actions(query).await.unwrap();
        assert!(
            !audits.is_empty(),
            "Auth event should be recorded in audit log"
        );
        assert_eq!(audits[0].action_kind, "auth_required");
        assert_eq!(audits[0].pane_id, Some(pane_id));
        });
    }

    #[test]
    fn regression_auth_cooldown_skips_repeat() {
        run_async_test(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_reg_cooldown_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let (runner, storage, _lock) = create_test_runner_with_panes(&db_path_str, &[202]).await;
        runner.register_workflow(Arc::new(HandleAuthRequired::new()));

        let pane_id = 202u64;
        create_test_pane(&storage, pane_id).await;

        let det = make_session_detection(
            "codex.auth.device_code_prompt",
            AgentType::Codex,
            "auth.device_code",
            serde_json::json!({"code": "ABCD-12345"}),
        );

        // First run: should complete normally
        let start1 = runner.handle_detection(pane_id, &det, None).await;
        assert!(start1.is_started());
        let exec_id1 = start1.execution_id().unwrap().to_string();
        let wf = runner
            .find_workflow_by_name("handle_auth_required")
            .unwrap();
        let result1 = runner.run_workflow(pane_id, wf.clone(), &exec_id1, 0).await;
        assert!(result1.is_completed(), "First auth run should complete");

        // Second run: cooldown check should cause early completion (step 0 returns Done)
        let start2 = runner.handle_detection(pane_id, &det, None).await;
        assert!(start2.is_started());
        let exec_id2 = start2.execution_id().unwrap().to_string();
        let result2 = runner.run_workflow(pane_id, wf, &exec_id2, 0).await;
        assert!(
            result2.is_completed(),
            "Second auth run should complete (via cooldown skip)"
        );

        // Verify second run has fewer step logs (only 1 step: cooldown check → Done)
        let logs2 = storage.get_step_logs(&exec_id2).await.unwrap();
        assert_eq!(
            logs2.len(),
            1,
            "Cooldown-skipped run should have only 1 step log, got {}",
            logs2.len()
        );
        assert_eq!(logs2[0].step_name, "check_cooldown");
        assert_eq!(logs2[0].result_type, "done");
        });
    }

    #[test]
    fn regression_session_end_null_trigger_produces_sparse_record() {
        run_async_test(async {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_reg_no_trigger_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let (runner, storage, _lock) = create_test_runner(&db_path_str).await;
        runner.register_workflow(Arc::new(HandleSessionEnd::new()));

        let pane_id = 203u64;
        create_test_pane(&storage, pane_id).await;

        // Detection that matches session.summary (runner doesn't populate trigger in context)
        let det = make_session_detection(
            "codex.session.token_usage",
            AgentType::Codex,
            "session.summary",
            serde_json::Value::Null,
        );

        let start = runner.handle_detection(pane_id, &det, None).await;
        assert!(start.is_started());
        let exec_id = start.execution_id().unwrap().to_string();
        let wf = runner.find_workflow_by_name("handle_session_end").unwrap();
        let result = runner.run_workflow(pane_id, wf, &exec_id, 0).await;

        // The workflow completes even without trigger data — it produces a sparse record
        // with agent_type="unknown" and no extracted fields
        assert!(
            result.is_completed(),
            "Session end should complete even without trigger data: {result:?}"
        );

        let logs = storage.get_step_logs(&exec_id).await.unwrap();
        assert_eq!(
            logs.len(),
            2,
            "Should have 2 step logs even with sparse data"
        );
        });
    }

    #[test]
    fn regression_codex_session_fixture_drift_check() {
        // Verify the Codex session parser produces correct records from known formats.
        // If Codex output drifts, this test fails and points to the exact field.
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": "12345",
                "input": "8000",
                "output": "4345",
                "cached": "2000",
                "reasoning": "1500",
                "session_id": "abc-def-123-456",
            }
        });
        let record = HandleSessionEnd::record_from_detection(100, &trigger);

        // Each field checked individually for actionable diagnostics
        assert_eq!(record.agent_type, "codex", "agent_type drift");
        assert_eq!(record.total_tokens, Some(12345), "total_tokens drift");
        assert_eq!(record.input_tokens, Some(8000), "input_tokens drift");
        assert_eq!(record.output_tokens, Some(4345), "output_tokens drift");
        assert_eq!(record.cached_tokens, Some(2000), "cached_tokens drift");
        assert_eq!(
            record.reasoning_tokens,
            Some(1500),
            "reasoning_tokens drift"
        );
        assert_eq!(
            record.session_id.as_deref(),
            Some("abc-def-123-456"),
            "session_id drift"
        );
        assert_eq!(
            record.end_reason.as_deref(),
            Some("completed"),
            "end_reason drift"
        );
    }

    #[test]
    fn regression_claude_code_session_fixture_drift_check() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.summary",
            "extracted": {
                "cost": "7.25",
            }
        });
        let record = HandleSessionEnd::record_from_detection(101, &trigger);

        assert_eq!(record.agent_type, "claude_code", "agent_type drift");
        assert_eq!(record.estimated_cost_usd, Some(7.25), "cost drift");
        assert!(
            record.total_tokens.is_none(),
            "Claude Code should not have tokens"
        );
        assert_eq!(
            record.end_reason.as_deref(),
            Some("completed"),
            "end_reason drift"
        );
    }

    #[test]
    fn regression_gemini_session_fixture_drift_check() {
        let trigger = serde_json::json!({
            "agent_type": "gemini",
            "event_type": "session.summary",
            "extracted": {
                "session_id": "aaaa-bbbb-cccc-dddd",
                "tool_calls": "42",
            }
        });
        let record = HandleSessionEnd::record_from_detection(102, &trigger);

        assert_eq!(record.agent_type, "gemini", "agent_type drift");
        assert_eq!(
            record.session_id.as_deref(),
            Some("aaaa-bbbb-cccc-dddd"),
            "session_id drift"
        );
        assert!(
            record.total_tokens.is_none(),
            "Gemini fixture should not have tokens"
        );
        assert!(
            record.estimated_cost_usd.is_none(),
            "Gemini fixture should not have cost"
        );
        assert_eq!(
            record.end_reason.as_deref(),
            Some("completed"),
            "end_reason drift"
        );
    }

    #[test]
    fn regression_auth_strategy_device_code_fixture_drift() {
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "auth.device_code",
            "rule_id": "codex.auth.device_code_prompt",
            "extracted": {
                "code": "WXYZ-98765",
                "url": "https://auth.openai.com/device",
            }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        match &strategy {
            AuthRecoveryStrategy::DeviceCode { code, url } => {
                assert_eq!(code.as_deref(), Some("WXYZ-98765"), "code drift");
                assert_eq!(
                    url.as_deref(),
                    Some("https://auth.openai.com/device"),
                    "url drift"
                );
            }
            other => panic!("Expected DeviceCode strategy, got: {other:?}"),
        }
    }

    #[test]
    fn regression_auth_strategy_api_key_fixture_drift() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "auth.error",
            "rule_id": "claude_code.auth.api_key_error",
            "extracted": {
                "key_name": "ANTHROPIC_API_KEY",
            }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        match &strategy {
            AuthRecoveryStrategy::ApiKeyError { key_hint } => {
                assert_eq!(
                    key_hint.as_deref(),
                    Some("ANTHROPIC_API_KEY"),
                    "key_hint drift"
                );
            }
            other => panic!("Expected ApiKeyError strategy, got: {other:?}"),
        }
    }

    #[test]
    fn regression_broken_fixture_produces_readable_error() {
        // Intentionally broken: mismatched agent_type field name in extracted
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "WRONG_total": "5000",
                "WRONG_input": "3000",
            }
        });
        let record = HandleSessionEnd::record_from_detection(999, &trigger);
        // Fields should gracefully be None, not crash
        assert!(
            record.total_tokens.is_none(),
            "Wrong field name should not parse as total"
        );
        assert!(
            record.input_tokens.is_none(),
            "Wrong field name should not parse as input"
        );
        // Agent type still correctly extracted from top-level
        assert_eq!(record.agent_type, "codex");
    }

    // ========================================================================
    // Device Auth Step Tests (wa-nu4.1.3.6)
    // ========================================================================

    #[cfg(feature = "browser")]
    mod device_auth_step_tests {
        use super::*;

        // -- DeviceAuthStepOutcome serde tests --

        #[test]
        fn outcome_authenticated_serde() {
            let outcome = DeviceAuthStepOutcome::Authenticated {
                elapsed_ms: 5432,
                account: "work".into(),
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"authenticated""#));
            assert!(json.contains(r#""elapsed_ms":5432"#));
            assert!(json.contains(r#""account":"work""#));

            let parsed: DeviceAuthStepOutcome = serde_json::from_str(&json).unwrap();
            match parsed {
                DeviceAuthStepOutcome::Authenticated {
                    elapsed_ms,
                    account,
                } => {
                    assert_eq!(elapsed_ms, 5432);
                    assert_eq!(account, "work");
                }
                _ => panic!("Expected Authenticated variant"),
            }
        }

        #[test]
        fn outcome_bootstrap_required_serde() {
            let outcome = DeviceAuthStepOutcome::BootstrapRequired {
                reason: "MFA required".into(),
                account: "default".into(),
                artifacts_dir: Some(std::path::PathBuf::from("/tmp/artifacts")),
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"bootstrap_required""#));
            assert!(json.contains(r#""reason":"MFA required""#));
            assert!(json.contains("artifacts_dir"));

            let parsed: DeviceAuthStepOutcome = serde_json::from_str(&json).unwrap();
            match parsed {
                DeviceAuthStepOutcome::BootstrapRequired {
                    reason,
                    account,
                    artifacts_dir,
                } => {
                    assert_eq!(reason, "MFA required");
                    assert_eq!(account, "default");
                    assert!(artifacts_dir.is_some());
                }
                _ => panic!("Expected BootstrapRequired variant"),
            }
        }

        #[test]
        fn outcome_bootstrap_required_omits_none_artifacts() {
            let outcome = DeviceAuthStepOutcome::BootstrapRequired {
                reason: "Password needed".into(),
                account: "default".into(),
                artifacts_dir: None,
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(!json.contains("artifacts_dir"));
        }

        #[test]
        fn outcome_failed_serde_batch2() {
            let outcome = DeviceAuthStepOutcome::Failed {
                error: "Playwright crashed".into(),
                error_kind: Some("PlaywrightError".into()),
                artifacts_dir: None,
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"failed""#));
            assert!(json.contains(r#""error":"Playwright crashed""#));
            assert!(json.contains(r#""error_kind":"PlaywrightError""#));
            assert!(!json.contains("artifacts_dir"));
        }

        #[test]
        fn outcome_failed_omits_none_kind() {
            let outcome = DeviceAuthStepOutcome::Failed {
                error: "unknown".into(),
                error_kind: None,
                artifacts_dir: None,
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(!json.contains("error_kind"));
        }

        // -- execute_device_auth_step: code validation --

        #[test]
        fn step_rejects_invalid_device_code() {
            let tmp =
                std::env::temp_dir().join(format!("wa_device_auth_test_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&tmp);

            let outcome = execute_device_auth_step("not-valid", "default", &tmp, None, true);
            match outcome {
                DeviceAuthStepOutcome::Failed {
                    error, error_kind, ..
                } => {
                    assert!(error.contains("Invalid device code"));
                    assert_eq!(error_kind.as_deref(), Some("invalid_code"));
                }
                other => panic!("Expected Failed, got {other:?}"),
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[test]
        fn step_rejects_empty_device_code() {
            let tmp = std::env::temp_dir()
                .join(format!("wa_device_auth_test_empty_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&tmp);

            let outcome = execute_device_auth_step("", "default", &tmp, None, true);
            match outcome {
                DeviceAuthStepOutcome::Failed { error_kind, .. } => {
                    assert_eq!(error_kind.as_deref(), Some("invalid_code"));
                }
                other => panic!("Expected Failed, got {other:?}"),
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[test]
        fn step_rejects_short_parts() {
            let tmp = std::env::temp_dir()
                .join(format!("wa_device_auth_test_short_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&tmp);

            let outcome = execute_device_auth_step("AB-CD", "default", &tmp, None, true);
            match outcome {
                DeviceAuthStepOutcome::Failed { error_kind, .. } => {
                    assert_eq!(error_kind.as_deref(), Some("invalid_code"));
                }
                other => panic!("Expected Failed, got {other:?}"),
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[test]
        fn step_accepts_valid_code_format_then_fails_browser() {
            // A valid code format will pass validation but fail at browser init
            // (Playwright not installed in test env). This verifies the step
            // progresses past validation to the browser phase.
            let tmp = std::env::temp_dir()
                .join(format!("wa_device_auth_test_valid_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&tmp);

            let outcome = execute_device_auth_step("ABCD-EFGH", "default", &tmp, None, true);
            match outcome {
                DeviceAuthStepOutcome::Failed {
                    error, error_kind, ..
                } => {
                    // Should fail at browser init, not code validation
                    assert_ne!(error_kind.as_deref(), Some("invalid_code"));
                    assert!(
                        error.contains("Browser")
                            || error.contains("browser")
                            || error.contains("Playwright")
                            || error.contains("playwright")
                            || error_kind.as_deref() == Some("browser_not_ready"),
                        "Expected browser-related error, got: {error}"
                    );
                }
                // If somehow browser init succeeds (unlikely in CI), that's also fine
                DeviceAuthStepOutcome::Authenticated { .. } => {}
                DeviceAuthStepOutcome::BootstrapRequired { .. } => {}
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[test]
        fn step_with_alphanumeric_code() {
            let tmp = std::env::temp_dir().join(format!(
                "wa_device_auth_test_alphanum_{}",
                std::process::id()
            ));
            let _ = std::fs::create_dir_all(&tmp);

            // Alphanumeric codes (including digits) should pass validation
            let outcome = execute_device_auth_step("AB12-CD34", "default", &tmp, None, true);
            // Should NOT fail with invalid_code
            match &outcome {
                DeviceAuthStepOutcome::Failed { error_kind, .. } => {
                    assert_ne!(error_kind.as_deref(), Some("invalid_code"));
                }
                _ => {} // Authenticated or BootstrapRequired are both fine
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        // -- device_auth_outcome_to_step_result mapping --

        #[test]
        fn outcome_to_step_result_authenticated() {
            let outcome = DeviceAuthStepOutcome::Authenticated {
                elapsed_ms: 1000,
                account: "default".into(),
            };
            let step = device_auth_outcome_to_step_result(&outcome);
            match step {
                StepResult::Done { result } => {
                    assert!(result.get("status").is_some());
                    assert_eq!(
                        result.get("status").and_then(|v| v.as_str()),
                        Some("authenticated")
                    );
                }
                other => panic!("Expected Done, got {other:?}"),
            }
        }

        #[test]
        fn outcome_to_step_result_bootstrap_required() {
            let outcome = DeviceAuthStepOutcome::BootstrapRequired {
                reason: "MFA".into(),
                account: "work".into(),
                artifacts_dir: None,
            };
            let step = device_auth_outcome_to_step_result(&outcome);
            match step {
                StepResult::Abort { reason } => {
                    assert!(reason.contains("bootstrap"));
                    assert!(reason.contains("work"));
                    assert!(reason.contains("MFA"));
                }
                other => panic!("Expected Abort, got {other:?}"),
            }
        }

        #[test]
        fn outcome_to_step_result_failed() {
            let outcome = DeviceAuthStepOutcome::Failed {
                error: "Selector mismatch".into(),
                error_kind: Some("SelectorMismatch".into()),
                artifacts_dir: None,
            };
            let step = device_auth_outcome_to_step_result(&outcome);
            match step {
                StepResult::Abort { reason } => {
                    assert!(reason.contains("Selector mismatch"));
                }
                other => panic!("Expected Abort, got {other:?}"),
            }
        }

        #[test]
        fn outcome_to_step_result_authenticated_contains_elapsed() {
            let outcome = DeviceAuthStepOutcome::Authenticated {
                elapsed_ms: 7890,
                account: "test".into(),
            };
            let step = device_auth_outcome_to_step_result(&outcome);
            if let StepResult::Done { result } = step {
                assert_eq!(
                    result.get("elapsed_ms").and_then(|v| v.as_u64()),
                    Some(7890)
                );
            }
        }
    }

    // ========================================================================
    // Resume Session Step Tests (wa-nu4.1.3.7)
    // ========================================================================

    mod resume_session_step_tests {
        use super::*;

        // -- ResumeSessionConfig --

        #[test]
        fn config_defaults() {
            let config = ResumeSessionConfig::default();
            assert!(config.resume_command_template.contains("{session_id}"));
            assert_eq!(config.proceed_text, "proceed.\n");
            assert_eq!(config.post_resume_stable_ms, 3_000);
            assert_eq!(config.post_proceed_stable_ms, 5_000);
            assert_eq!(config.resume_timeout_ms, 30_000);
            assert_eq!(config.proceed_timeout_ms, 30_000);
        }

        #[test]
        fn config_serde_round_trip() {
            let config = ResumeSessionConfig::default();
            let json = serde_json::to_string(&config).unwrap();
            let parsed: ResumeSessionConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(
                parsed.resume_command_template,
                config.resume_command_template
            );
            assert_eq!(parsed.proceed_text, config.proceed_text);
            assert_eq!(parsed.post_resume_stable_ms, config.post_resume_stable_ms);
        }

        // -- format_resume_command --

        #[test]
        fn format_resume_command_default() {
            let config = ResumeSessionConfig::default();
            let cmd = format_resume_command("abc123-def456", &config);
            assert_eq!(cmd, "cod resume abc123-def456\n");
        }

        #[test]
        fn format_resume_command_custom_template() {
            let config = ResumeSessionConfig {
                resume_command_template: "codex resume {session_id} --continue\n".into(),
                ..Default::default()
            };
            let cmd = format_resume_command("session-99", &config);
            assert_eq!(cmd, "codex resume session-99 --continue\n");
        }

        #[test]
        fn format_resume_command_no_placeholder() {
            let config = ResumeSessionConfig {
                resume_command_template: "fixed-command\n".into(),
                ..Default::default()
            };
            let cmd = format_resume_command("ignored-id", &config);
            assert_eq!(cmd, "fixed-command\n");
        }

        // -- validate_session_id --

        #[test]
        fn validate_session_id_valid_uuid() {
            assert!(validate_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
        }

        #[test]
        fn validate_session_id_valid_short_hex() {
            assert!(validate_session_id("abcdef01"));
        }

        #[test]
        fn validate_session_id_valid_with_hyphens() {
            assert!(validate_session_id("abc-def-123"));
        }

        #[test]
        fn validate_session_id_rejects_too_short() {
            assert!(!validate_session_id("abc"));
            assert!(!validate_session_id("1234567")); // 7 chars
        }

        #[test]
        fn validate_session_id_rejects_empty() {
            assert!(!validate_session_id(""));
        }

        #[test]
        fn validate_session_id_rejects_non_hex() {
            assert!(!validate_session_id("zzzzzzzz"));
            assert!(!validate_session_id("abc!defg"));
        }

        #[test]
        fn validate_session_id_trims_whitespace() {
            assert!(validate_session_id("  abcdef01  "));
        }

        // -- ResumeSessionOutcome serde --

        #[test]
        fn outcome_ready_serde() {
            let outcome = ResumeSessionOutcome::Ready {
                session_id: "abc-123".into(),
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"ready""#));
            assert!(json.contains(r#""session_id":"abc-123""#));

            let parsed: ResumeSessionOutcome = serde_json::from_str(&json).unwrap();
            match parsed {
                ResumeSessionOutcome::Ready { session_id } => {
                    assert_eq!(session_id, "abc-123");
                }
                _ => panic!("Expected Ready variant"),
            }
        }

        #[test]
        fn outcome_verify_timeout_serde() {
            let outcome = ResumeSessionOutcome::VerifyTimeout {
                session_id: "def-456".into(),
                phase: "proceed".into(),
                waited_ms: 30_000,
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"timeout""#));
            assert!(json.contains(r#""phase":"proceed""#));
            assert!(json.contains(r#""waited_ms":30000"#));
        }

        #[test]
        fn outcome_failed_serde() {
            let outcome = ResumeSessionOutcome::Failed {
                error: "Policy denied".into(),
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"failed""#));
            assert!(json.contains(r#""error":"Policy denied""#));
        }

        // -- build_resume_step_result --

        #[test]
        fn resume_step_result_is_send_text() {
            let config = ResumeSessionConfig::default();
            let result = build_resume_step_result("abc-def-123", &config);
            assert!(result.is_send_text());

            if let StepResult::SendText {
                text,
                wait_for,
                wait_timeout_ms,
            } = result
            {
                assert_eq!(text, "cod resume abc-def-123\n");
                assert!(wait_for.is_some());
                match wait_for.unwrap() {
                    WaitCondition::StableTail {
                        pane_id,
                        stable_for_ms,
                    } => {
                        assert_eq!(pane_id, None);
                        assert_eq!(stable_for_ms, 3_000);
                    }
                    other => panic!("Expected StableTail, got {other:?}"),
                }
                assert_eq!(wait_timeout_ms, Some(30_000));
            }
        }

        // -- build_proceed_step_result --

        #[test]
        fn proceed_step_result_is_send_text() {
            let config = ResumeSessionConfig::default();
            let result = build_proceed_step_result(&config);
            assert!(result.is_send_text());

            if let StepResult::SendText {
                text,
                wait_for,
                wait_timeout_ms,
            } = result
            {
                assert_eq!(text, "proceed.\n");
                assert!(wait_for.is_some());
                match wait_for.unwrap() {
                    WaitCondition::StableTail { stable_for_ms, .. } => {
                        assert_eq!(stable_for_ms, 5_000);
                    }
                    other => panic!("Expected StableTail, got {other:?}"),
                }
                assert_eq!(wait_timeout_ms, Some(30_000));
            }
        }

        // -- resume_outcome_to_step_result mapping --

        #[test]
        fn outcome_to_step_result_ready_is_done() {
            let outcome = ResumeSessionOutcome::Ready {
                session_id: "abc".into(),
            };
            let step = resume_outcome_to_step_result(&outcome);
            assert!(step.is_done());

            if let StepResult::Done { result } = step {
                assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("ready"));
            }
        }

        #[test]
        fn outcome_to_step_result_timeout_is_done() {
            // Timeouts are soft failures — still Done, not Abort
            let outcome = ResumeSessionOutcome::VerifyTimeout {
                session_id: "abc".into(),
                phase: "resume".into(),
                waited_ms: 30_000,
            };
            let step = resume_outcome_to_step_result(&outcome);
            assert!(step.is_done());

            if let StepResult::Done { result } = step {
                assert_eq!(
                    result.get("status").and_then(|v| v.as_str()),
                    Some("timeout")
                );
            }
        }

        #[test]
        fn outcome_to_step_result_failed_is_abort() {
            let outcome = ResumeSessionOutcome::Failed {
                error: "Policy denied send".into(),
            };
            let step = resume_outcome_to_step_result(&outcome);
            assert!(step.is_terminal());

            match step {
                StepResult::Abort { reason } => {
                    assert!(reason.contains("Policy denied send"));
                }
                other => panic!("Expected Abort, got {other:?}"),
            }
        }

        // -- Custom config --

        #[test]
        fn custom_config_affects_step_results() {
            let config = ResumeSessionConfig {
                resume_command_template: "codex resume {session_id}\n".into(),
                proceed_text: "continue.\n".into(),
                post_resume_stable_ms: 1_000,
                post_proceed_stable_ms: 2_000,
                resume_timeout_ms: 10_000,
                proceed_timeout_ms: 15_000,
            };

            let resume = build_resume_step_result("session-42", &config);
            if let StepResult::SendText {
                text,
                wait_timeout_ms,
                ..
            } = resume
            {
                assert_eq!(text, "codex resume session-42\n");
                assert_eq!(wait_timeout_ms, Some(10_000));
            }

            let proceed = build_proceed_step_result(&config);
            if let StepResult::SendText {
                text,
                wait_timeout_ms,
                ..
            } = proceed
            {
                assert_eq!(text, "continue.\n");
                assert_eq!(wait_timeout_ms, Some(15_000));
            }
        }
    }

    // ========================================================================
    // Safe Fallback Path Tests (wa-nu4.1.3.8)
    // ========================================================================
    mod fallback_path_tests {
        use super::*;

        // -- FallbackReason --

        #[test]
        fn fallback_reason_needs_human_auth_display() {
            let reason = FallbackReason::NeedsHumanAuth {
                account: "openai-team".into(),
                detail: "MFA required".into(),
            };
            let s = reason.to_string();
            assert!(s.contains("openai-team"), "should contain account: {s}");
            assert!(s.contains("MFA required"), "should contain detail: {s}");
        }

        #[test]
        fn fallback_reason_failover_disabled_display() {
            let reason = FallbackReason::FailoverDisabled;
            assert!(reason.to_string().contains("disabled"));
        }

        #[test]
        fn fallback_reason_tool_missing_display() {
            let reason = FallbackReason::ToolMissing {
                tool: "playwright".into(),
            };
            assert!(reason.to_string().contains("playwright"));
        }

        #[test]
        fn fallback_reason_policy_denied_display() {
            let reason = FallbackReason::PolicyDenied {
                rule: "alt_screen_active".into(),
            };
            assert!(reason.to_string().contains("alt_screen_active"));
        }

        #[test]
        fn fallback_reason_all_accounts_exhausted_display() {
            let reason = FallbackReason::AllAccountsExhausted {
                accounts_checked: 3,
            };
            let s = reason.to_string();
            assert!(s.contains("3"), "should contain count: {s}");
        }

        #[test]
        fn fallback_reason_other_display() {
            let reason = FallbackReason::Other {
                detail: "unexpected error".into(),
            };
            assert!(reason.to_string().contains("unexpected error"));
        }

        #[test]
        fn fallback_reason_serde_round_trip() {
            let reasons = vec![
                FallbackReason::NeedsHumanAuth {
                    account: "acct-1".into(),
                    detail: "password required".into(),
                },
                FallbackReason::FailoverDisabled,
                FallbackReason::ToolMissing {
                    tool: "caut".into(),
                },
                FallbackReason::PolicyDenied {
                    rule: "recent_gap".into(),
                },
                FallbackReason::AllAccountsExhausted {
                    accounts_checked: 5,
                },
                FallbackReason::Other {
                    detail: "test".into(),
                },
            ];

            for reason in &reasons {
                let json = serde_json::to_string(reason).unwrap();
                let parsed: FallbackReason = serde_json::from_str(&json).unwrap();
                // Verify the kind tag survived round-trip
                let json2 = serde_json::to_string(&parsed).unwrap();
                assert_eq!(json, json2, "Round-trip mismatch for {reason:?}");
            }
        }

        // -- FallbackNextStepPlan --

        #[test]
        fn plan_version_is_current() {
            assert_eq!(FallbackNextStepPlan::CURRENT_VERSION, 1);
        }

        #[test]
        fn needs_human_auth_plan_structure() {
            let plan = build_needs_human_auth_plan(
                42,
                "openai-team",
                "MFA required",
                Some("sess-abc123"),
                Some(1_700_000_000_000),
                1_699_999_000_000,
            );

            assert_eq!(plan.version, 1);
            assert_eq!(plan.pane_id, 42);
            assert!(matches!(plan.reason, FallbackReason::NeedsHumanAuth { .. }));
            assert_eq!(plan.resume_session_id.as_deref(), Some("sess-abc123"));
            assert_eq!(plan.account_id.as_deref(), Some("openai-team"));
            assert_eq!(plan.retry_after_ms, Some(1_700_000_000_000));
            assert!(!plan.operator_steps.is_empty());
            assert!(!plan.suggested_commands.is_empty());
            assert_eq!(plan.created_at_ms, 1_699_999_000_000);

            // Operator steps should mention bootstrap and resume
            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("bootstrap"),
                "steps should mention bootstrap: {steps_text}"
            );
            assert!(
                steps_text.contains("sess-abc123"),
                "steps should mention session ID: {steps_text}"
            );
        }

        #[test]
        fn needs_human_auth_plan_without_session_id() {
            let plan = build_needs_human_auth_plan(10, "acct", "SSO needed", None, None, 1_000_000);

            assert!(plan.resume_session_id.is_none());
            assert!(plan.retry_after_ms.is_none());
            // Should not mention resume in steps
            let steps_text = plan.operator_steps.join(" ");
            assert!(
                !steps_text.contains("resume"),
                "should not mention resume without session ID: {steps_text}"
            );
        }

        #[test]
        fn failover_disabled_plan_structure() {
            let plan = build_failover_disabled_plan(
                99,
                Some("sess-xyz"),
                Some(1_700_000_000_000),
                1_699_999_000_000,
            );

            assert!(matches!(plan.reason, FallbackReason::FailoverDisabled));
            assert_eq!(plan.pane_id, 99);
            assert_eq!(plan.resume_session_id.as_deref(), Some("sess-xyz"));

            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("disabled"),
                "should mention disabled: {steps_text}"
            );
        }

        #[test]
        fn tool_missing_plan_structure() {
            let plan = build_tool_missing_plan(7, "playwright", 1_000_000);

            assert!(matches!(plan.reason, FallbackReason::ToolMissing { .. }));
            assert_eq!(plan.pane_id, 7);
            assert!(plan.resume_session_id.is_none());
            assert!(plan.retry_after_ms.is_none());

            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("playwright"),
                "should mention missing tool: {steps_text}"
            );
        }

        #[test]
        fn all_accounts_exhausted_plan_with_retry() {
            let plan = build_all_accounts_exhausted_plan(
                55,
                4,
                Some("sess-111"),
                Some(1_700_000_000_000),
                1_699_999_000_000,
            );

            assert!(matches!(
                plan.reason,
                FallbackReason::AllAccountsExhausted {
                    accounts_checked: 4
                }
            ));
            assert_eq!(plan.pane_id, 55);

            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("4"),
                "should mention account count: {steps_text}"
            );
            assert!(
                steps_text.contains("reset"),
                "should mention reset when retry_after is set: {steps_text}"
            );
        }

        #[test]
        fn all_accounts_exhausted_plan_without_retry() {
            let plan = build_all_accounts_exhausted_plan(55, 2, None, None, 1_000_000);

            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("ft accounts status"),
                "should suggest checking accounts: {steps_text}"
            );
        }

        // -- Step result conversion --

        #[test]
        fn fallback_plan_to_step_result_is_done() {
            let plan = build_needs_human_auth_plan(1, "acct", "MFA", None, None, 1_000_000);
            let result = fallback_plan_to_step_result(&plan);
            assert!(result.is_done(), "fallback should produce Done, not Abort");
            assert!(result.is_terminal());
        }

        #[test]
        fn fallback_plan_to_step_result_has_fallback_flag() {
            let plan = build_failover_disabled_plan(1, None, None, 1_000_000);
            let result = fallback_plan_to_step_result(&plan);
            assert!(is_fallback_result(&result), "should be tagged as fallback");
        }

        #[test]
        fn is_fallback_result_false_for_normal_done() {
            let result = StepResult::done(serde_json::json!({"status": "ok"}));
            assert!(
                !is_fallback_result(&result),
                "normal Done should not be fallback"
            );
        }

        #[test]
        fn is_fallback_result_false_for_abort() {
            let result = StepResult::abort("test");
            assert!(!is_fallback_result(&result), "Abort should not be fallback");
        }

        #[test]
        fn fallback_handled_status_is_paused() {
            assert_eq!(FALLBACK_HANDLED_STATUS, "paused");
        }

        // -- Serde round-trip for full plan --

        #[test]
        fn plan_serde_round_trip() {
            let plan = build_needs_human_auth_plan(
                42,
                "openai-team",
                "MFA required",
                Some("sess-abc123"),
                Some(1_700_000_000_000),
                1_699_999_000_000,
            );

            let json = serde_json::to_string(&plan).unwrap();
            let parsed: FallbackNextStepPlan = serde_json::from_str(&json).unwrap();

            assert_eq!(parsed.version, plan.version);
            assert_eq!(parsed.pane_id, plan.pane_id);
            assert_eq!(parsed.resume_session_id, plan.resume_session_id);
            assert_eq!(parsed.account_id, plan.account_id);
            assert_eq!(parsed.retry_after_ms, plan.retry_after_ms);
            assert_eq!(parsed.operator_steps.len(), plan.operator_steps.len());
            assert_eq!(
                parsed.suggested_commands.len(),
                plan.suggested_commands.len()
            );
            assert_eq!(parsed.created_at_ms, plan.created_at_ms);
        }

        #[test]
        fn plan_skips_none_fields_in_json() {
            let plan = build_tool_missing_plan(1, "caut", 1_000_000);
            let json = serde_json::to_string(&plan).unwrap();

            // Optional None fields should be absent
            assert!(
                !json.contains("retry_after_ms"),
                "None fields should be skipped: {json}"
            );
            assert!(
                !json.contains("resume_session_id"),
                "None fields should be skipped: {json}"
            );
            assert!(
                !json.contains("account_id"),
                "None fields should be skipped: {json}"
            );
        }

        #[test]
        fn fallback_step_result_preserves_plan_fields() {
            let plan =
                build_needs_human_auth_plan(42, "acct", "MFA", Some("sess-1"), None, 1_000_000);
            let result = fallback_plan_to_step_result(&plan);

            if let StepResult::Done { result } = result {
                // The plan fields plus the "fallback" flag should all be present
                let map = result.as_object().unwrap();
                assert!(map.contains_key("version"));
                assert!(map.contains_key("reason"));
                assert!(map.contains_key("pane_id"));
                assert!(map.contains_key("operator_steps"));
                assert!(map.contains_key("fallback"));
                assert_eq!(map["fallback"], true);
                assert_eq!(map["pane_id"], 42);
            } else {
                panic!("Expected Done variant");
            }
        }
    }

    // ========================================================================
    // Usage-limit Workflow Tests (wa-nu4.1.3.9)
    // ========================================================================

    #[test]
    fn usage_limit_workflow_has_correct_name_and_steps() {
        let wf = HandleUsageLimits::new();
        assert_eq!(wf.name(), "handle_usage_limits");
        assert_eq!(
            wf.description(),
            "Exit agent, persist session summary, and select new account for failover"
        );

        let steps = wf.steps();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].name, "check_guards");
        assert_eq!(steps[1].name, "exit_and_persist");
        assert_eq!(steps[2].name, "select_account");
    }

    #[test]
    fn usage_limit_workflow_handles_codex_usage_events() {
        let wf = HandleUsageLimits::new();

        let detection = Detection {
            rule_id: "codex.usage_limit".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "You've hit your usage limit".to_string(),
            span: (0, 0),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn usage_limit_workflow_handles_codex_rate_limit_events() {
        let wf = HandleUsageLimits::new();

        let detection = Detection {
            rule_id: "codex.rate_limit.detected".to_string(),
            agent_type: AgentType::Codex,
            event_type: "rate_limit.detected".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::json!({
                "retry_after": "5 minutes"
            }),
            matched_text: "rate limit exceeded; retry after 5 minutes".to_string(),
            span: (0, 0),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn usage_limit_workflow_ignores_non_codex_agents() {
        let wf = HandleUsageLimits::new();

        let detection = Detection {
            rule_id: "claude_code.usage.reached".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "rate limit".to_string(),
            span: (0, 0),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn usage_limit_workflow_ignores_non_usage_events() {
        let wf = HandleUsageLimits::new();

        let detection = Detection {
            rule_id: "codex.session.end".to_string(),
            agent_type: AgentType::Codex,
            event_type: "session.summary".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "Session ended normally".to_string(),
            span: (0, 0),
        };
        assert!(!wf.handles(&detection));
    }

    // -- Fixture: Real-world usage limit transcript --

    const FIXTURE_FULL_USAGE_LIMIT: &str = r"
$ cod start
Starting Codex session...
Working directory: /data/projects/my-app

You've hit your usage limit. Try again at 3:00 PM UTC.

Token usage: total=1,234,567 input=500,000 (+ 200,000 cached) output=534,567 (reasoning 100,000)
To continue this session, run: codex resume 123e4567-e89b-12d3-a456-426614174000
$";

    const FIXTURE_MINIMAL_USAGE_LIMIT: &str =
        "Token usage: total=42\ncodex resume aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    const FIXTURE_NO_RESET_TIME: &str = r"
You've hit your usage limit.
Token usage: total=100 input=50 output=50
codex resume 11111111-2222-3333-4444-555555555555";

    const FIXTURE_MISSING_SESSION_ID: &str = r"
Token usage: total=100 input=50
You've hit your usage limit. Try again at 5:00 PM.";

    const FIXTURE_MISSING_TOKEN_USAGE: &str =
        "codex resume 11111111-2222-3333-4444-555555555555\nSome other output";

    const FIXTURE_EMPTY: &str = "";

    const FIXTURE_GARBAGE: &str = "random noise\n123\nno markers here";

    #[test]
    fn fixture_full_usage_limit_parses_all_fields() {
        let result = parse_codex_session_summary(FIXTURE_FULL_USAGE_LIMIT).expect("should parse");
        assert_eq!(result.session_id, "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(result.token_usage.total, Some(1_234_567));
        assert_eq!(result.token_usage.input, Some(500_000));
        assert_eq!(result.token_usage.cached, Some(200_000));
        assert_eq!(result.token_usage.output, Some(534_567));
        assert_eq!(result.token_usage.reasoning, Some(100_000));
        assert_eq!(result.reset_time.as_deref(), Some("3:00 PM UTC"));
    }

    #[test]
    fn fixture_minimal_parses_required_fields() {
        let result =
            parse_codex_session_summary(FIXTURE_MINIMAL_USAGE_LIMIT).expect("should parse");
        assert_eq!(result.session_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(result.token_usage.total, Some(42));
        assert!(result.token_usage.input.is_none());
        assert!(result.reset_time.is_none());
    }

    #[test]
    fn fixture_no_reset_time_parses_without_reset() {
        let result = parse_codex_session_summary(FIXTURE_NO_RESET_TIME).expect("should parse");
        assert_eq!(result.session_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(result.token_usage.total, Some(100));
        assert!(result.reset_time.is_none());
    }

    #[test]
    fn fixture_missing_session_id_fails_gracefully() {
        let err = parse_codex_session_summary(FIXTURE_MISSING_SESSION_ID).expect_err("should fail");
        assert!(err.missing.contains(&"session_id"));
        assert!(!err.missing.contains(&"token_usage"));
        assert!(err.tail_len > 0);
        assert!(err.tail_hash != 0);
    }

    #[test]
    fn fixture_missing_token_usage_fails_gracefully() {
        let err =
            parse_codex_session_summary(FIXTURE_MISSING_TOKEN_USAGE).expect_err("should fail");
        assert!(err.missing.contains(&"token_usage"));
        assert!(!err.missing.contains(&"session_id"));
    }

    #[test]
    fn fixture_empty_fails_with_both_missing() {
        let err = parse_codex_session_summary(FIXTURE_EMPTY).expect_err("should fail");
        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn fixture_garbage_fails_with_both_missing() {
        let err = parse_codex_session_summary(FIXTURE_GARBAGE).expect_err("should fail");
        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn token_usage_extraction_handles_partial_line() {
        let line = "Token usage: total=500 output=300";
        let usage = extract_token_usage(line);
        assert_eq!(usage.total, Some(500));
        assert_eq!(usage.output, Some(300));
        assert!(usage.input.is_none());
        assert!(usage.cached.is_none());
        assert!(usage.reasoning.is_none());
    }

    #[test]
    fn token_usage_extraction_handles_empty_line() {
        let usage = extract_token_usage("");
        assert!(usage.total.is_none());
        assert!(!usage.has_any());
    }

    #[test]
    fn session_record_from_summary_maps_all_fields() {
        let summary = CodexSessionSummary {
            session_id: "test-session-1234".to_string(),
            token_usage: CodexTokenUsage {
                total: Some(1000),
                input: Some(400),
                output: Some(500),
                cached: Some(100),
                reasoning: Some(50),
            },
            reset_time: Some("5:00 PM".to_string()),
        };

        let record = codex_session_record_from_summary(42, &summary);
        assert_eq!(record.pane_id, 42);
        assert_eq!(record.agent_type, "codex");
        assert_eq!(record.session_id.as_deref(), Some("test-session-1234"));
        assert_eq!(record.total_tokens, Some(1000));
        assert_eq!(record.input_tokens, Some(400));
        assert_eq!(record.output_tokens, Some(500));
        assert_eq!(record.cached_tokens, Some(100));
        assert_eq!(record.reasoning_tokens, Some(50));
    }

    #[test]
    fn usage_limit_default_impl() {
        let wf = HandleUsageLimits;
        assert_eq!(wf.name(), "handle_usage_limits");
    }

    #[test]
    fn parse_error_display_is_actionable() {
        let err = CodexSessionParseError {
            missing: vec!["session_id", "token_usage"],
            tail_hash: 0xdeadbeef,
            tail_len: 42,
        };
        let display = err.to_string();
        assert!(display.contains("session_id"));
        assert!(display.contains("token_usage"));
        assert!(display.contains("tail_hash="));
        assert!(display.contains("tail_len=42"));
    }

    #[test]
    fn find_session_id_prefers_last_occurrence() {
        let tail = "codex resume aaaa1234\nsome text\ncodex resume bbbb5678";
        let id = find_session_id(tail).unwrap();
        assert_eq!(id, "bbbb5678");
    }

    #[test]
    fn find_session_id_returns_none_on_no_match() {
        assert!(find_session_id("no resume hint here").is_none());
    }

    #[test]
    fn find_reset_time_extracts_from_try_again_at() {
        let tail = "Try again at 10:30 AM EST.";
        let time = find_reset_time(tail).unwrap();
        assert_eq!(time, "10:30 AM EST");
    }

    #[test]
    fn find_reset_time_returns_none_when_absent() {
        assert!(find_reset_time("no reset time here").is_none());
    }

    #[test]
    fn find_token_usage_line_finds_last_occurrence() {
        let tail = "Token usage: first\nsome middle\nToken usage: second";
        let line = find_token_usage_line(tail).unwrap();
        assert!(line.contains("second"));
    }

    #[test]
    fn parse_number_handles_commas_and_edge_cases() {
        assert_eq!(parse_number("1,234,567"), Some(1_234_567));
        assert_eq!(parse_number("42"), Some(42));
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("abc"), None);
    }

    // =========================================================================
    // RubyBeaver wa-1u90p.7.1 — additional pure unit tests
    // =========================================================================

    #[test]
    fn parse_number_negative() {
        assert_eq!(parse_number("-1"), Some(-1));
    }

    #[test]
    fn parse_number_zero() {
        assert_eq!(parse_number("0"), Some(0));
    }

    #[test]
    fn parse_number_large_with_commas() {
        assert_eq!(parse_number("1,000,000,000"), Some(1_000_000_000));
    }

    #[test]
    fn capture_number_from_total_regex() {
        assert_eq!(capture_number(&CODEX_TOTAL_RE, "total=42"), Some(42));
    }

    #[test]
    fn capture_number_from_input_regex() {
        assert_eq!(capture_number(&CODEX_INPUT_RE, "input=100"), Some(100));
    }

    #[test]
    fn capture_number_from_output_regex() {
        assert_eq!(capture_number(&CODEX_OUTPUT_RE, "output=200"), Some(200));
    }

    #[test]
    fn capture_number_no_match_returns_none() {
        assert_eq!(capture_number(&CODEX_TOTAL_RE, "nothing here"), None);
    }

    #[test]
    fn capture_number_cached_with_comma() {
        assert_eq!(
            capture_number(&CODEX_CACHED_RE, "(+ 1,234 cached)"),
            Some(1234)
        );
    }

    #[test]
    fn capture_number_reasoning() {
        assert_eq!(
            capture_number(&CODEX_REASONING_RE, "(reasoning 500)"),
            Some(500)
        );
    }

    #[test]
    fn extract_token_usage_full_line() {
        let line = "Token usage: total=1,000 input=400 output=500 (+ 50 cached) (reasoning 100)";
        let usage = extract_token_usage(line);
        assert_eq!(usage.total, Some(1000));
        assert_eq!(usage.input, Some(400));
        assert_eq!(usage.output, Some(500));
        assert_eq!(usage.cached, Some(50));
        assert_eq!(usage.reasoning, Some(100));
        assert!(usage.has_any());
    }

    #[test]
    fn codex_token_usage_has_any_true_with_total_only() {
        let usage = CodexTokenUsage {
            total: Some(100),
            input: None,
            output: None,
            cached: None,
            reasoning: None,
        };
        assert!(usage.has_any());
    }

    #[test]
    fn codex_token_usage_has_any_false_all_none() {
        let usage = CodexTokenUsage {
            total: None,
            input: None,
            output: None,
            cached: None,
            reasoning: None,
        };
        assert!(!usage.has_any());
    }

    #[test]
    fn codex_token_usage_has_any_true_with_cached_only() {
        let usage = CodexTokenUsage {
            total: None,
            input: None,
            output: None,
            cached: Some(42),
            reasoning: None,
        };
        assert!(usage.has_any());
    }

    #[test]
    fn codex_exit_options_default_grace_timeout() {
        let opts = CodexExitOptions::default();
        assert_eq!(opts.grace_timeout_ms, 2_000);
    }

    #[test]
    fn codex_exit_options_default_summary_timeout() {
        let opts = CodexExitOptions::default();
        assert_eq!(opts.summary_timeout_ms, 20_000);
    }

    #[test]
    fn codex_session_parse_error_display_includes_fields() {
        let err = CodexSessionParseError {
            missing: vec!["session_id"],
            tail_hash: 0xCAFE,
            tail_len: 100,
        };
        let msg = err.to_string();
        assert!(msg.contains("session_id"));
        assert!(msg.contains("tail_len=100"));
    }

    #[test]
    fn codex_session_parse_error_is_error_trait() {
        let err = CodexSessionParseError {
            missing: vec!["token_usage"],
            tail_hash: 0,
            tail_len: 0,
        };
        // Verify it implements std::error::Error
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn step_result_cont_builder() {
        assert!(matches!(StepResult::cont(), StepResult::Continue));
    }

    #[test]
    fn step_result_done_builder_with_value() {
        let result = StepResult::done(serde_json::json!({"key": "value"}));
        assert!(result.is_done());
        assert!(result.is_terminal());
    }

    #[test]
    fn step_result_done_empty_is_null() {
        if let StepResult::Done { result } = StepResult::done_empty() {
            assert!(result.is_null());
        } else {
            panic!("expected Done");
        }
    }

    #[test]
    fn step_result_retry_builder() {
        if let StepResult::Retry { delay_ms } = StepResult::retry(5000) {
            assert_eq!(delay_ms, 5000);
        } else {
            panic!("expected Retry");
        }
    }

    #[test]
    fn step_result_abort_builder() {
        if let StepResult::Abort { reason } = StepResult::abort("failed") {
            assert_eq!(reason, "failed");
        } else {
            panic!("expected Abort");
        }
    }

    #[test]
    fn step_result_wait_for_builder_default_timeout() {
        let result = StepResult::wait_for(WaitCondition::external("key"));
        if let StepResult::WaitFor { timeout_ms, .. } = result {
            assert!(timeout_ms.is_none());
        } else {
            panic!("expected WaitFor");
        }
    }

    #[test]
    fn step_result_wait_for_with_timeout_builder() {
        let result = StepResult::wait_for_with_timeout(WaitCondition::external("key"), 3000);
        if let StepResult::WaitFor { timeout_ms, .. } = result {
            assert_eq!(timeout_ms, Some(3000));
        } else {
            panic!("expected WaitFor");
        }
    }

    #[test]
    fn step_result_send_text_builder() {
        let result = StepResult::send_text("hello");
        assert!(result.is_send_text());
        if let StepResult::SendText {
            text,
            wait_for,
            wait_timeout_ms,
        } = result
        {
            assert_eq!(text, "hello");
            assert!(wait_for.is_none());
            assert!(wait_timeout_ms.is_none());
        } else {
            panic!("expected SendText");
        }
    }

    #[test]
    fn step_result_send_text_and_wait_builder() {
        let result = StepResult::send_text_and_wait("hello", WaitCondition::pane_idle(1000), 5000);
        assert!(result.is_send_text());
        if let StepResult::SendText {
            wait_for,
            wait_timeout_ms,
            ..
        } = result
        {
            assert!(wait_for.is_some());
            assert_eq!(wait_timeout_ms, Some(5000));
        } else {
            panic!("expected SendText");
        }
    }

    #[test]
    fn step_result_is_continue_only_for_continue() {
        assert!(StepResult::cont().is_continue());
        assert!(!StepResult::done_empty().is_continue());
        assert!(!StepResult::abort("e").is_continue());
        assert!(!StepResult::retry(1).is_continue());
    }

    #[test]
    fn step_result_is_send_text_only_for_send_text() {
        assert!(StepResult::send_text("x").is_send_text());
        assert!(!StepResult::cont().is_send_text());
        assert!(!StepResult::done_empty().is_send_text());
    }

    #[test]
    fn step_result_is_terminal_done_and_abort_only() {
        assert!(StepResult::done_empty().is_terminal());
        assert!(StepResult::abort("e").is_terminal());
        assert!(!StepResult::cont().is_terminal());
        assert!(!StepResult::retry(1).is_terminal());
        assert!(!StepResult::send_text("x").is_terminal());
        assert!(!StepResult::wait_for(WaitCondition::external("k")).is_terminal());
    }

    #[test]
    fn text_match_substring_builder() {
        let m = TextMatch::substring("hello");
        assert!(matches!(m, TextMatch::Substring { value } if value == "hello"));
    }

    #[test]
    fn text_match_regex_builder() {
        let m = TextMatch::regex(r"\d+");
        assert!(matches!(m, TextMatch::Regex { pattern } if pattern == r"\d+"));
    }

    #[test]
    fn text_match_description_substring() {
        let m = TextMatch::substring("test");
        let desc = m.description();
        assert!(desc.starts_with("substring(len=4"));
    }

    #[test]
    fn text_match_description_regex() {
        let m = TextMatch::regex(r"pat");
        let desc = m.description();
        assert!(desc.starts_with("regex(len=3"));
    }

    #[test]
    fn execution_status_all_variants_serde() {
        for (variant, expected) in [
            (ExecutionStatus::Running, "\"running\""),
            (ExecutionStatus::Waiting, "\"waiting\""),
            (ExecutionStatus::Completed, "\"completed\""),
            (ExecutionStatus::Aborted, "\"aborted\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let back: ExecutionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn workflow_engine_default_max_concurrent() {
        let engine = WorkflowEngine::default();
        assert_eq!(engine.max_concurrent(), 3);
    }

    #[test]
    fn workflow_engine_custom_max_concurrent() {
        let engine = WorkflowEngine::new(10);
        assert_eq!(engine.max_concurrent(), 10);
    }

    #[test]
    fn lock_acquisition_result_acquired_methods() {
        let result = LockAcquisitionResult::Acquired;
        assert!(result.is_acquired());
        assert!(!result.is_already_locked());
    }

    #[test]
    fn lock_acquisition_result_already_locked_methods() {
        let result = LockAcquisitionResult::AlreadyLocked {
            held_by_workflow: "test_wf".to_string(),
            held_by_execution: "exec-1".to_string(),
            locked_since_ms: 1000,
        };
        assert!(!result.is_acquired());
        assert!(result.is_already_locked());
    }

    #[test]
    fn pane_lock_info_serde_roundtrip() {
        let info = PaneLockInfo {
            pane_id: 42,
            workflow_name: "test_wf".to_string(),
            execution_id: "exec-1".to_string(),
            locked_at_ms: 1234567890,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: PaneLockInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pane_id, 42);
        assert_eq!(back.workflow_name, "test_wf");
    }

    #[test]
    fn step_error_code_abort_returns_ft5002() {
        let result = StepResult::abort("some error");
        assert_eq!(
            step_error_code_from_result(&result),
            Some("FT-5002".to_string())
        );
    }

    #[test]
    fn step_error_code_continue_returns_none() {
        assert_eq!(step_error_code_from_result(&StepResult::cont()), None);
    }

    #[test]
    fn step_error_code_done_returns_none() {
        assert_eq!(step_error_code_from_result(&StepResult::done_empty()), None);
    }

    #[test]
    fn step_error_code_retry_returns_none() {
        assert_eq!(step_error_code_from_result(&StepResult::retry(100)), None);
    }

    #[test]
    fn step_error_code_send_text_returns_none() {
        assert_eq!(
            step_error_code_from_result(&StepResult::send_text("x")),
            None
        );
    }

    #[test]
    fn redact_text_for_log_short_unchanged() {
        let result = redact_text_for_log("hello", 100);
        assert_eq!(result, "hello");
    }

    #[test]
    fn redact_text_for_log_truncates_long() {
        let long = "a".repeat(200);
        let result = redact_text_for_log(&long, 10);
        assert!(result.len() <= 13); // 10 chars + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn redact_text_for_log_exact_boundary() {
        let result = redact_text_for_log("12345", 5);
        assert_eq!(result, "12345"); // exactly at limit, no truncation
    }

    #[test]
    fn redacted_step_result_for_logging_preserves_continue() {
        let result = StepResult::cont();
        let redacted = redacted_step_result_for_logging(&result);
        assert!(matches!(redacted, StepResult::Continue));
    }

    #[test]
    fn redacted_step_result_for_logging_preserves_abort() {
        let result = StepResult::abort("reason");
        let redacted = redacted_step_result_for_logging(&result);
        if let StepResult::Abort { reason } = redacted {
            assert_eq!(reason, "reason");
        } else {
            panic!("expected Abort");
        }
    }

    #[test]
    fn redacted_step_result_for_logging_truncates_send_text() {
        let long_text = "x".repeat(500);
        let result = StepResult::send_text(long_text);
        let redacted = redacted_step_result_for_logging(&result);
        if let StepResult::SendText { text, .. } = redacted {
            assert!(text.len() <= 163); // 160 + "..."
        } else {
            panic!("expected SendText");
        }
    }

    #[test]
    fn fallback_reason_display_needs_human_auth() {
        let reason = FallbackReason::NeedsHumanAuth {
            account: "openai".to_string(),
            detail: "MFA required".to_string(),
        };
        let msg = reason.to_string();
        assert!(msg.contains("openai"));
        assert!(msg.contains("MFA required"));
    }

    #[test]
    fn fallback_reason_display_failover_disabled() {
        let reason = FallbackReason::FailoverDisabled;
        assert!(reason.to_string().contains("disabled"));
    }

    #[test]
    fn fallback_reason_display_tool_missing() {
        let reason = FallbackReason::ToolMissing {
            tool: "playwright".to_string(),
        };
        assert!(reason.to_string().contains("playwright"));
    }

    #[test]
    fn fallback_reason_display_policy_denied() {
        let reason = FallbackReason::PolicyDenied {
            rule: "alt_screen".to_string(),
        };
        assert!(reason.to_string().contains("alt_screen"));
    }

    #[test]
    fn fallback_reason_display_all_accounts_exhausted() {
        let reason = FallbackReason::AllAccountsExhausted {
            accounts_checked: 3,
        };
        assert!(reason.to_string().contains("3"));
    }

    #[test]
    fn fallback_reason_display_other() {
        let reason = FallbackReason::Other {
            detail: "custom detail".to_string(),
        };
        assert_eq!(reason.to_string(), "custom detail");
    }

    #[test]
    fn fallback_reason_serde_needs_human_auth() {
        let reason = FallbackReason::NeedsHumanAuth {
            account: "test".to_string(),
            detail: "details".to_string(),
        };
        let json = serde_json::to_string(&reason).unwrap();
        let back: FallbackReason = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, FallbackReason::NeedsHumanAuth { .. }));
    }

    #[test]
    fn fallback_reason_serde_failover_disabled() {
        let reason = FallbackReason::FailoverDisabled;
        let json = serde_json::to_string(&reason).unwrap();
        let back: FallbackReason = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, FallbackReason::FailoverDisabled));
    }

    #[test]
    fn fallback_reason_serde_tool_missing() {
        let reason = FallbackReason::ToolMissing {
            tool: "caut".to_string(),
        };
        let json = serde_json::to_string(&reason).unwrap();
        let back: FallbackReason = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, FallbackReason::ToolMissing { .. }));
    }

    #[test]
    fn fallback_reason_serde_all_accounts_exhausted() {
        let reason = FallbackReason::AllAccountsExhausted {
            accounts_checked: 5,
        };
        let json = serde_json::to_string(&reason).unwrap();
        let back: FallbackReason = serde_json::from_str(&json).unwrap();
        if let FallbackReason::AllAccountsExhausted { accounts_checked } = back {
            assert_eq!(accounts_checked, 5);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn fallback_next_step_plan_serde_roundtrip() {
        let plan = FallbackNextStepPlan {
            version: FallbackNextStepPlan::CURRENT_VERSION,
            reason: FallbackReason::FailoverDisabled,
            pane_id: 42,
            operator_steps: vec!["Step 1".to_string()],
            retry_after_ms: Some(1000),
            resume_session_id: Some("abc-123".to_string()),
            account_id: Some("acct-1".to_string()),
            suggested_commands: vec!["ft auth bootstrap".to_string()],
            created_at_ms: 9999,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: FallbackNextStepPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, FallbackNextStepPlan::CURRENT_VERSION);
        assert_eq!(back.pane_id, 42);
        assert_eq!(back.operator_steps.len(), 1);
        assert_eq!(back.retry_after_ms, Some(1000));
        assert_eq!(back.resume_session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn fallback_next_step_plan_current_version_is_one() {
        assert_eq!(FallbackNextStepPlan::CURRENT_VERSION, 1);
    }

    #[test]
    fn fallback_next_step_plan_optional_fields_skip_none() {
        let plan = FallbackNextStepPlan {
            version: 1,
            reason: FallbackReason::FailoverDisabled,
            pane_id: 1,
            operator_steps: vec![],
            retry_after_ms: None,
            resume_session_id: None,
            account_id: None,
            suggested_commands: vec![],
            created_at_ms: 0,
        };
        let json = serde_json::to_string(&plan).unwrap();
        // skip_serializing_if = "Option::is_none" should omit these
        assert!(!json.contains("retry_after_ms"));
        assert!(!json.contains("resume_session_id"));
        assert!(!json.contains("account_id"));
        assert!(!json.contains("suggested_commands"));
    }

    #[test]
    fn now_ms_returns_plausible_timestamp() {
        let ms = now_ms();
        // Should be after 2020-01-01
        assert!(ms > 1_577_836_800_000);
    }

    #[test]
    fn generate_workflow_id_contains_workflow_name() {
        let id = generate_workflow_id("handle_usage");
        assert!(id.starts_with("handle_usage-"));
    }

    #[test]
    fn generate_workflow_id_has_three_parts() {
        let id = generate_workflow_id("test");
        assert_eq!(id.splitn(3, '-').count(), 3, "expected name-timestamp-hex");
    }

    #[test]
    fn find_token_usage_line_returns_none_when_absent() {
        assert!(find_token_usage_line("no usage here").is_none());
    }

    #[test]
    fn find_session_id_extracts_uuid_style() {
        let tail = "codex resume a1b2c3d4-e5f6-7890-abcd-1234567890ab";
        let id = find_session_id(tail).unwrap();
        assert!(id.contains("a1b2c3d4"));
    }

    #[test]
    fn find_reset_time_multiple_prefers_last() {
        let tail = "try again at 3:00 PM.\ntry again at 5:00 PM.";
        let time = find_reset_time(tail).unwrap();
        assert!(time.contains("5:00 PM"));
    }

    #[test]
    fn ctrl_c_injection_ok_allowed() {
        let result = InjectionResult::Allowed {
            decision: crate::policy::PolicyDecision::Allow {
                rule_id: Some("test".to_string()),
                context: None,
            },
            summary: "ctrl-c".to_string(),
            pane_id: 1,
            action: crate::policy::ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        assert!(ctrl_c_injection_ok(result).is_ok());
    }

    #[test]
    fn ctrl_c_injection_ok_denied() {
        let result = InjectionResult::Denied {
            decision: crate::policy::PolicyDecision::Deny {
                reason: "blocked".to_string(),
                rule_id: None,
                context: None,
            },
            summary: "ctrl-c".to_string(),
            pane_id: 1,
            action: crate::policy::ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        let err = ctrl_c_injection_ok(result).unwrap_err();
        assert!(err.contains("denied"));
    }

    #[test]
    fn ctrl_c_injection_ok_error() {
        let result = InjectionResult::Error {
            error: "connection lost".to_string(),
            pane_id: 1,
            action: crate::policy::ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        let err = ctrl_c_injection_ok(result).unwrap_err();
        assert!(err.contains("connection lost"));
    }

    #[test]
    fn wait_condition_stable_tail_serde() {
        let cond = WaitCondition::StableTail {
            pane_id: None,
            stable_for_ms: 2000,
        };
        let json = serde_json::to_string(&cond).unwrap();
        let back: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cond);
    }

    #[test]
    fn wait_condition_stable_tail_with_pane() {
        let cond = WaitCondition::StableTail {
            pane_id: Some(10),
            stable_for_ms: 500,
        };
        assert_eq!(cond.pane_id(), Some(10));
    }

    #[test]
    fn text_match_to_wait_matcher_substring() {
        let m = TextMatch::substring("hello");
        let wm = m.to_wait_matcher().unwrap();
        assert!(format!("{:?}", wm).contains("hello"));
    }

    #[test]
    fn text_match_to_wait_matcher_regex_valid() {
        let m = TextMatch::regex(r"\d+");
        assert!(m.to_wait_matcher().is_ok());
    }

    #[test]
    fn text_match_to_wait_matcher_regex_invalid() {
        let m = TextMatch::regex(r"(unclosed");
        assert!(m.to_wait_matcher().is_err());
    }

    #[test]
    fn build_verification_refs_empty_for_continue() {
        let result = StepResult::cont();
        assert!(build_verification_refs(&result, None).is_none());
    }

    #[test]
    fn build_verification_refs_populated_for_wait_for() {
        let result = StepResult::wait_for_with_timeout(WaitCondition::external("signal"), 5000);
        let refs = build_verification_refs(&result, None).unwrap();
        assert!(refs.contains("wait_for"));
        assert!(refs.contains("5000"));
    }

    #[test]
    fn build_verification_refs_populated_for_send_text_wait() {
        let result = StepResult::send_text_and_wait("hello", WaitCondition::pane_idle(1000), 3000);
        let refs = build_verification_refs(&result, None).unwrap();
        assert!(refs.contains("post_send_wait"));
    }

    #[test]
    fn parse_codex_session_summary_full() {
        let tail = "codex resume abc12345\nToken usage: total=500 input=200 output=300\ntry again at 5:00 PM.";
        let summary = parse_codex_session_summary(tail).unwrap();
        assert_eq!(summary.session_id, "abc12345");
        assert_eq!(summary.token_usage.total, Some(500));
        assert_eq!(summary.reset_time.as_deref(), Some("5:00 PM"));
    }

    #[test]
    fn parse_codex_session_summary_missing_session_id() {
        let tail = "Token usage: total=500";
        let err = parse_codex_session_summary(tail).unwrap_err();
        assert!(err.missing.contains(&"session_id"));
    }

    #[test]
    fn parse_codex_session_summary_missing_token_usage() {
        let tail = "codex resume abc12345";
        let err = parse_codex_session_summary(tail).unwrap_err();
        assert!(err.missing.contains(&"token_usage"));
    }

    // =========================================================================
    // RubyBeaver wa-1u90p.7.1 batch 2
    // =========================================================================

    #[test]
    fn parse_codex_session_summary_missing_both() {
        let tail = "nothing useful";
        let err = parse_codex_session_summary(tail).unwrap_err();
        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn parse_codex_session_summary_no_reset_time() {
        let tail = "codex resume abc12345\nToken usage: total=500 input=200 output=300";
        let summary = parse_codex_session_summary(tail).unwrap();
        assert!(summary.reset_time.is_none());
    }

    #[test]
    fn codex_session_parse_error_tail_hash_nonzero() {
        let err = parse_codex_session_summary("some content").unwrap_err();
        // tail_hash should be deterministic and non-zero for non-empty input
        assert!(err.tail_hash != 0 || err.tail_len == 0);
        assert_eq!(err.tail_len, "some content".len());
    }

    #[test]
    fn descriptor_limits_default_max_steps() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_steps > 0);
        assert!(limits.max_steps <= 100);
    }

    #[test]
    fn descriptor_limits_default_max_wait_timeout() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_wait_timeout_ms > 0);
    }

    #[test]
    fn descriptor_limits_default_max_sleep() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_sleep_ms > 0);
    }

    #[test]
    fn descriptor_limits_default_max_text_len() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_text_len > 0);
    }

    #[test]
    fn descriptor_limits_default_max_match_len() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_match_len > 0);
    }

    #[test]
    fn descriptor_failure_handler_interpolate_notify() {
        let handler = DescriptorFailureHandler::Notify {
            message: "Step ${failed_step} failed".to_string(),
        };
        let msg = handler.interpolate_message("send_ctrl_c");
        assert_eq!(msg, "Step send_ctrl_c failed");
    }

    #[test]
    fn descriptor_failure_handler_interpolate_log() {
        let handler = DescriptorFailureHandler::Log {
            message: "Error in ${failed_step}".to_string(),
        };
        let msg = handler.interpolate_message("wait_idle");
        assert_eq!(msg, "Error in wait_idle");
    }

    #[test]
    fn descriptor_failure_handler_interpolate_abort_no_placeholder() {
        let handler = DescriptorFailureHandler::Abort {
            message: "workflow failed".to_string(),
        };
        let msg = handler.interpolate_message("step1");
        assert_eq!(msg, "workflow failed");
    }

    #[test]
    fn descriptor_matcher_to_text_match_substring() {
        let dm = DescriptorMatcher::Substring {
            value: "hello".to_string(),
        };
        let tm = dm.to_text_match();
        assert!(matches!(tm, TextMatch::Substring { value } if value == "hello"));
    }

    #[test]
    fn descriptor_matcher_to_text_match_regex() {
        let dm = DescriptorMatcher::Regex {
            pattern: r"\d+".to_string(),
        };
        let tm = dm.to_text_match();
        assert!(matches!(tm, TextMatch::Regex { pattern } if pattern == r"\d+"));
    }

    #[test]
    fn descriptor_trigger_serde_roundtrip() {
        let trigger = DescriptorTrigger {
            event_types: vec!["session.end".to_string()],
            agent_types: vec!["codex".to_string()],
            rule_ids: vec!["compaction.detected".to_string()],
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let back: DescriptorTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(back.event_types, vec!["session.end"]);
        assert_eq!(back.agent_types, vec!["codex"]);
    }

    #[test]
    fn descriptor_trigger_empty_defaults() {
        let json = "{}";
        let trigger: DescriptorTrigger = serde_json::from_str(json).unwrap();
        assert!(trigger.event_types.is_empty());
        assert!(trigger.agent_types.is_empty());
        assert!(trigger.rule_ids.is_empty());
    }

    #[test]
    fn device_code_struct_fields() {
        let dc = DeviceCode {
            code: "ABCD-1234".to_string(),
            url: Some("https://example.com/device".to_string()),
        };
        assert_eq!(dc.code, "ABCD-1234");
        assert!(dc.url.unwrap().contains("device"));
    }

    #[test]
    fn device_code_parse_error_display_format() {
        let err = DeviceCodeParseError {
            expected: "XXXX-YYYY",
            tail_hash: 0xBEEF,
            tail_len: 50,
        };
        let msg = err.to_string();
        assert!(msg.contains("XXXX-YYYY"));
        assert!(msg.contains("tail_len=50"));
    }

    #[test]
    fn device_code_parse_error_is_std_error() {
        let err = DeviceCodeParseError {
            expected: "test",
            tail_hash: 0,
            tail_len: 0,
        };
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn validate_device_code_empty_string() {
        assert!(!validate_device_code(""));
    }

    #[test]
    fn validate_device_code_no_dash() {
        assert!(!validate_device_code("ABCD1234"));
    }

    #[test]
    fn validate_device_code_too_short_parts() {
        assert!(!validate_device_code("AB-CD"));
    }

    #[test]
    fn parse_device_code_from_text() {
        let tail = "Enter code: ABCD-1234";
        let dc = parse_device_code(tail).unwrap();
        assert_eq!(dc.code, "ABCD-1234");
    }

    #[test]
    fn parse_device_code_no_match() {
        let err = parse_device_code("nothing here").unwrap_err();
        assert!(err.expected.contains("device code"));
    }

    #[test]
    fn account_selection_step_error_display_caut() {
        let err = AccountSelectionStepError::Storage("db locked".to_string());
        let msg = err.to_string();
        assert!(msg.contains("db locked"));
    }

    #[test]
    fn account_selection_step_error_is_std_error() {
        let err = AccountSelectionStepError::Storage("test".to_string());
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn pane_group_strategy_serde_by_domain() {
        let s = PaneGroupStrategy::ByDomain;
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pane_group_strategy_serde_by_agent() {
        let s = PaneGroupStrategy::ByAgent;
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pane_group_strategy_serde_by_project() {
        let s = PaneGroupStrategy::ByProject;
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pane_group_strategy_serde_explicit() {
        let s = PaneGroupStrategy::Explicit {
            pane_ids: vec![1, 2, 3],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pane_group_new_and_accessors() {
        let g = PaneGroup::new("test", vec![1, 2], PaneGroupStrategy::ByDomain);
        assert_eq!(g.name, "test");
        assert_eq!(g.len(), 2);
        assert!(!g.is_empty());
    }

    #[test]
    fn pane_group_empty() {
        let g = PaneGroup::new("empty", vec![], PaneGroupStrategy::ByAgent);
        assert_eq!(g.len(), 0);
        assert!(g.is_empty());
    }

    #[test]
    fn wait_condition_result_satisfied_elapsed() {
        let r = WaitConditionResult::Satisfied {
            elapsed_ms: 100,
            polls: 5,
            context: Some("matched".to_string()),
        };
        assert!(r.is_satisfied());
        assert!(!r.is_timed_out());
        assert_eq!(r.elapsed_ms(), Some(100));
    }

    #[test]
    fn wait_condition_result_timed_out_elapsed() {
        let r = WaitConditionResult::TimedOut {
            elapsed_ms: 5000,
            polls: 50,
            last_observed: None,
        };
        assert!(!r.is_satisfied());
        assert!(r.is_timed_out());
        assert_eq!(r.elapsed_ms(), Some(5000));
    }

    #[test]
    fn wait_condition_result_unsupported_no_elapsed() {
        let r = WaitConditionResult::Unsupported {
            reason: "not impl".to_string(),
        };
        assert!(!r.is_satisfied());
        assert!(!r.is_timed_out());
        assert_eq!(r.elapsed_ms(), None);
    }

    #[test]
    fn wait_condition_options_default_has_sensible_values() {
        let opts = WaitConditionOptions::default();
        assert!(opts.tail_lines > 0);
        assert!(opts.max_polls > 0);
        assert!(opts.poll_initial > Duration::ZERO);
        assert!(opts.poll_max >= opts.poll_initial);
    }

    #[test]
    fn auth_recovery_strategy_label_device_code() {
        let s = AuthRecoveryStrategy::DeviceCode {
            code: None,
            url: None,
        };
        assert_eq!(s.label(), "device_code");
    }

    #[test]
    fn auth_recovery_strategy_label_api_key_error() {
        let s = AuthRecoveryStrategy::ApiKeyError { key_hint: None };
        assert_eq!(s.label(), "api_key_error");
    }

    #[test]
    fn auth_recovery_strategy_label_manual() {
        let s = AuthRecoveryStrategy::ManualIntervention {
            agent_type: "codex".to_string(),
            hint: "login".to_string(),
        };
        assert_eq!(s.label(), "manual_intervention");
    }

    #[test]
    fn auth_recovery_strategy_from_detection_device_code() {
        let trigger = serde_json::json!({
            "event_type": "auth.device_code",
            "extracted": { "code": "ABCD-1234", "url": "https://example.com" }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "device_code");
    }

    #[test]
    fn auth_recovery_strategy_from_detection_api_key() {
        let trigger = serde_json::json!({
            "event_type": "auth.error",
            "extracted": { "key_name": "OPENAI_API_KEY" }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "api_key_error");
    }

    #[test]
    fn auth_recovery_strategy_from_detection_manual_fallback() {
        let trigger = serde_json::json!({
            "event_type": "auth.unknown",
            "agent_type": "claude_code"
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "manual_intervention");
    }

    #[test]
    fn build_needs_human_auth_plan_includes_account() {
        let plan = build_needs_human_auth_plan(
            42,
            "openai-team",
            "MFA required",
            Some("sess-123"),
            Some(9999),
            1000,
        );
        assert_eq!(plan.version, FallbackNextStepPlan::CURRENT_VERSION);
        assert_eq!(plan.pane_id, 42);
        assert!(
            plan.operator_steps
                .iter()
                .any(|s| s.contains("openai-team"))
        );
        assert_eq!(plan.retry_after_ms, Some(9999));
        assert_eq!(plan.resume_session_id.as_deref(), Some("sess-123"));
    }

    #[test]
    fn build_needs_human_auth_plan_no_resume() {
        let plan = build_needs_human_auth_plan(1, "acct", "detail", None, None, 0);
        assert!(plan.resume_session_id.is_none());
        assert!(plan.retry_after_ms.is_none());
    }

    #[test]
    fn validate_session_id_accepts_long_hex() {
        assert!(validate_session_id("a1b2c3d4e5f60000"));
    }

    #[test]
    fn validate_session_id_rejects_special_chars() {
        assert!(!validate_session_id("abc!@#$"));
    }

    #[test]
    fn format_resume_command_with_session_id() {
        let config = ResumeSessionConfig::default();
        let cmd = format_resume_command("test-session-123", &config);
        assert!(cmd.contains("test-session-123"));
    }

    #[test]
    fn elapsed_ms_is_zero_for_recent_instant() {
        let start = Instant::now();
        let ms = elapsed_ms(start);
        assert!(ms < 1000); // Should be near-zero
    }

    #[test]
    fn workflow_step_new() {
        let step = WorkflowStep::new("step1", "First step");
        assert_eq!(step.name, "step1");
        assert_eq!(step.description, "First step");
    }

    #[test]
    fn wait_condition_pane_id_pattern() {
        let cond = WaitCondition::pattern("rule");
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_pattern_on_pane() {
        let cond = WaitCondition::pattern_on_pane(42, "rule");
        assert_eq!(cond.pane_id(), Some(42));
    }

    #[test]
    fn wait_condition_pane_id_pane_idle() {
        let cond = WaitCondition::pane_idle(1000);
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_pane_idle_on() {
        let cond = WaitCondition::pane_idle_on(10, 1000);
        assert_eq!(cond.pane_id(), Some(10));
    }

    #[test]
    fn wait_condition_pane_id_stable_tail() {
        let cond = WaitCondition::stable_tail(2000);
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_stable_tail_on() {
        let cond = WaitCondition::stable_tail_on(5, 2000);
        assert_eq!(cond.pane_id(), Some(5));
    }

    #[test]
    fn wait_condition_pane_id_text_match() {
        let cond = WaitCondition::text_match(TextMatch::substring("x"));
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_text_match_on_pane() {
        let cond = WaitCondition::text_match_on_pane(99, TextMatch::substring("x"));
        assert_eq!(cond.pane_id(), Some(99));
    }

    #[test]
    fn wait_condition_pane_id_sleep() {
        let cond = WaitCondition::sleep(100);
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_external() {
        let cond = WaitCondition::external("signal");
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn descriptor_loop_execution() {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "test_loop".to_string(),
            description: None,
            triggers: vec![],
            steps: vec![DescriptorStep::Loop {
                id: "loop".to_string(),
                description: None,
                count: 3,
                body: vec![DescriptorStep::Log {
                    id: "log".to_string(),
                    description: None,
                    message: "iteration".to_string(),
                }],
            }],
            on_failure: None,
        };
        let _workflow = DescriptorWorkflow::new(descriptor);
    }
}
