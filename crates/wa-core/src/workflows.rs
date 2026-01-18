//! Durable workflow execution engine
//!
//! Provides idempotent, recoverable, audited workflow execution.

use serde::{Deserialize, Serialize};

/// Result of a workflow step
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepResult {
    /// Proceed to next step
    Continue,
    /// Workflow completed successfully
    Done { result: serde_json::Value },
    /// Retry this step after delay
    Retry { delay_ms: u64 },
    /// Abort workflow with error
    Abort { reason: String },
    /// Wait for condition before proceeding
    WaitFor { condition: WaitCondition },
}

/// Conditions to wait for before proceeding
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WaitCondition {
    /// Wait for a pattern to appear
    Pattern { rule_id: String },
    /// Wait for pane to be idle
    PaneIdle { timeout_ms: u64 },
    /// Wait for external signal
    External { signal_name: String },
}

/// A step in a workflow
pub struct WorkflowStep {
    /// Step name
    pub name: String,
    /// Step description
    pub description: String,
}

/// Workflow execution state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowExecution {
    /// Unique execution ID
    pub id: String,
    /// Workflow name
    pub workflow_name: String,
    /// Pane being operated on
    pub pane_id: u64,
    /// Current step index
    pub current_step: usize,
    /// Status
    pub status: ExecutionStatus,
    /// Started at timestamp
    pub started_at: i64,
    /// Last updated timestamp
    pub updated_at: i64,
}

/// Workflow execution status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// Running
    Running,
    /// Waiting for condition
    Waiting,
    /// Completed successfully
    Completed,
    /// Aborted with error
    Aborted,
}

/// Workflow engine for managing executions
pub struct WorkflowEngine {
    /// Maximum concurrent workflows
    max_concurrent: usize,
}

impl Default for WorkflowEngine {
    fn default() -> Self {
        Self::new(3)
    }
}

impl WorkflowEngine {
    /// Create a new workflow engine
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        Self { max_concurrent }
    }

    /// Get the maximum concurrent workflows setting
    #[must_use]
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Start a workflow execution
    pub async fn start(
        &self,
        _workflow_name: &str,
        _pane_id: u64,
    ) -> crate::Result<WorkflowExecution> {
        // TODO: Implement workflow start
        todo!("Implement workflow start")
    }

    /// Resume a workflow execution
    pub async fn resume(&self, _execution_id: &str) -> crate::Result<WorkflowExecution> {
        // TODO: Implement workflow resume
        todo!("Implement workflow resume")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_result_serializes() {
        let result = StepResult::Continue;
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("continue"));
    }

    #[test]
    fn engine_can_be_created() {
        let engine = WorkflowEngine::new(5);
        assert_eq!(engine.max_concurrent(), 5);
    }
}
