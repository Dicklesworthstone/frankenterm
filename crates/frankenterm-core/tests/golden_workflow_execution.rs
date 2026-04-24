//! Golden-artifact tests for `WorkflowStartResult` / `WorkflowExecutionResult`.
//!
//! These enums are the JSON shapes emitted by the workflow runner back to
//! operators, MCP clients, and audit storage. Any wire-level drift — a renamed
//! tag, a flattened field, a missing optional — breaks ft robot CLI / MCP
//! clients / replay tooling, so the full variant matrix is pinned.
//!
//! Volatile fields (`execution_id` UUIDs, monotonic `elapsed_ms` counters)
//! are redacted through `insta` scrubbers so snapshots are stable across runs.

use frankenterm_core::workflows::runner::{WorkflowExecutionResult, WorkflowStartResult};
use insta::assert_json_snapshot;
use serde_json::json;

fn started_result() -> WorkflowStartResult {
    WorkflowStartResult::Started {
        execution_id: "exec-11111111-2222-3333-4444-555555555555".to_string(),
        workflow_name: "handle_usage_limits".to_string(),
    }
}

fn pane_locked_result() -> WorkflowStartResult {
    WorkflowStartResult::PaneLocked {
        pane_id: 7,
        held_by_workflow: "handle_compaction".to_string(),
        held_by_execution: "exec-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
    }
}

fn no_matching_workflow_result() -> WorkflowStartResult {
    WorkflowStartResult::NoMatchingWorkflow {
        rule_id: "codex.unknown_event".to_string(),
    }
}

fn concurrency_limit_result() -> WorkflowStartResult {
    WorkflowStartResult::ConcurrencyLimitReached {
        active: 8,
        limit: 8,
    }
}

fn start_error_result() -> WorkflowStartResult {
    WorkflowStartResult::Error {
        error: "workflow registry unavailable".to_string(),
    }
}

fn completed_result() -> WorkflowExecutionResult {
    WorkflowExecutionResult::Completed {
        execution_id: "exec-11111111-2222-3333-4444-555555555555".to_string(),
        result: json!({ "account_switched": true, "new_account": "alice@codex" }),
        elapsed_ms: 12345,
        steps_executed: 6,
    }
}

fn aborted_result() -> WorkflowExecutionResult {
    WorkflowExecutionResult::Aborted {
        execution_id: "exec-11111111-2222-3333-4444-555555555555".to_string(),
        reason: "user cancelled via Ctrl-C".to_string(),
        step_index: 3,
        elapsed_ms: 9000,
    }
}

fn policy_denied_result() -> WorkflowExecutionResult {
    WorkflowExecutionResult::PolicyDenied {
        execution_id: "exec-11111111-2222-3333-4444-555555555555".to_string(),
        step_index: 2,
        reason: "send blocked by send_text_policy".to_string(),
    }
}

fn execution_error_with_id() -> WorkflowExecutionResult {
    WorkflowExecutionResult::Error {
        execution_id: Some("exec-11111111-2222-3333-4444-555555555555".to_string()),
        error: "workflow handler panicked".to_string(),
    }
}

fn execution_error_without_id() -> WorkflowExecutionResult {
    WorkflowExecutionResult::Error {
        execution_id: None,
        error: "registry lookup failed before execution id was assigned".to_string(),
    }
}

// ── WorkflowStartResult variants ────────────────────────────────────────────

#[test]
fn workflow_start_started_matches_golden() {
    assert_json_snapshot!(
        "workflow_start_started",
        started_result(),
        { ".execution_id" => "[execution_id]" }
    );
}

#[test]
fn workflow_start_pane_locked_matches_golden() {
    assert_json_snapshot!(
        "workflow_start_pane_locked",
        pane_locked_result(),
        {
            ".held_by_execution" => "[execution_id]",
            ".pane_id" => "[pane_id]",
        }
    );
}

#[test]
fn workflow_start_no_matching_workflow_matches_golden() {
    assert_json_snapshot!("workflow_start_no_matching", no_matching_workflow_result());
}

#[test]
fn workflow_start_concurrency_limit_matches_golden() {
    assert_json_snapshot!(
        "workflow_start_concurrency_limit",
        concurrency_limit_result()
    );
}

#[test]
fn workflow_start_error_matches_golden() {
    assert_json_snapshot!("workflow_start_error", start_error_result());
}

// ── WorkflowExecutionResult variants ────────────────────────────────────────

#[test]
fn workflow_execution_completed_matches_golden() {
    assert_json_snapshot!(
        "workflow_execution_completed",
        completed_result(),
        {
            ".execution_id" => "[execution_id]",
            ".elapsed_ms" => "[elapsed_ms]",
        }
    );
}

#[test]
fn workflow_execution_aborted_matches_golden() {
    assert_json_snapshot!(
        "workflow_execution_aborted",
        aborted_result(),
        {
            ".execution_id" => "[execution_id]",
            ".elapsed_ms" => "[elapsed_ms]",
        }
    );
}

#[test]
fn workflow_execution_policy_denied_matches_golden() {
    assert_json_snapshot!(
        "workflow_execution_policy_denied",
        policy_denied_result(),
        { ".execution_id" => "[execution_id]" }
    );
}

#[test]
fn workflow_execution_error_with_id_matches_golden() {
    assert_json_snapshot!(
        "workflow_execution_error_with_id",
        execution_error_with_id(),
        { ".execution_id" => "[execution_id]" }
    );
}

#[test]
fn workflow_execution_error_without_id_matches_golden() {
    assert_json_snapshot!(
        "workflow_execution_error_without_id",
        execution_error_without_id()
    );
}

// ── Roundtrip invariants (non-snapshot but same harness) ────────────────────

#[test]
fn workflow_start_all_variants_roundtrip_through_json() {
    for original in [
        started_result(),
        pane_locked_result(),
        no_matching_workflow_result(),
        concurrency_limit_result(),
        start_error_result(),
    ] {
        let serialized = serde_json::to_string(&original).expect("serialize start result");
        let reparsed: WorkflowStartResult =
            serde_json::from_str(&serialized).expect("reparse start result");
        let reserialized = serde_json::to_string(&reparsed).expect("reserialize");
        assert_eq!(
            serialized, reserialized,
            "WorkflowStartResult roundtrip drift on {original:?}"
        );
    }
}

#[test]
fn workflow_execution_all_variants_roundtrip_through_json() {
    for original in [
        completed_result(),
        aborted_result(),
        policy_denied_result(),
        execution_error_with_id(),
        execution_error_without_id(),
    ] {
        let serialized = serde_json::to_string(&original).expect("serialize execution result");
        let reparsed: WorkflowExecutionResult =
            serde_json::from_str(&serialized).expect("reparse execution result");
        let reserialized = serde_json::to_string(&reparsed).expect("reserialize");
        assert_eq!(
            serialized, reserialized,
            "WorkflowExecutionResult roundtrip drift on {original:?}"
        );
    }
}
