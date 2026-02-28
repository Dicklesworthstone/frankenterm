//! Workflow context types for runtime execution.
//!
//! Provides `WorkflowContext` (runtime context with storage, pane state, capabilities,
//! and policy-gated injection), `WorkflowConfig`, and `PaneMetadata`.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

// ============================================================================
// Workflow Context
// ============================================================================

/// Cached pane metadata for workflow execution.
#[derive(Debug, Clone, Default)]
pub struct PaneMetadata {
    /// Domain name (e.g., local, SSH:host)
    pub domain: Option<String>,
    /// Pane title
    pub title: Option<String>,
    /// Current working directory
    pub cwd: Option<String>,
}

impl PaneMetadata {
    pub(crate) fn from_record(record: &crate::storage::PaneRecord) -> Self {
        Self {
            domain: Some(record.domain.clone()),
            title: record.title.clone(),
            cwd: record.cwd.clone(),
        }
    }
}

/// Configuration for a workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowConfig {
    /// Default timeout for wait conditions (milliseconds)
    pub default_wait_timeout_ms: u64,
    /// Maximum number of retries per step
    pub max_step_retries: u32,
    /// Delay between retry attempts (milliseconds)
    pub retry_delay_ms: u64,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            default_wait_timeout_ms: 30_000, // 30 seconds
            max_step_retries: 3,
            retry_delay_ms: 1_000, // 1 second
        }
    }
}

/// Runtime context for workflow execution.
///
/// Provides access to:
/// - WezTerm client for sending commands
/// - Storage handle for persistence
/// - Current pane state and capabilities
/// - Triggering event/detection
/// - Workflow configuration
#[derive(Clone)]
pub struct WorkflowContext {
    /// Storage handle for persistence operations
    storage: Arc<StorageHandle>,
    /// Target pane ID for this workflow
    pane_id: u64,
    /// Cached pane metadata for deterministic prompt selection
    pane_meta: PaneMetadata,
    /// Current pane capabilities snapshot
    capabilities: PaneCapabilities,
    /// The event/detection that triggered this workflow (JSON)
    trigger: Option<serde_json::Value>,
    /// Workflow configuration
    config: WorkflowConfig,
    /// Workflow execution ID
    execution_id: String,
    /// Policy-gated injector for terminal actions (optional)
    injector: Option<PolicyInjectorHandle>,
    /// The action plan for this workflow execution (plan-first mode)
    action_plan: Option<crate::plan::ActionPlan>,
}

impl WorkflowContext {
    /// Create a new workflow context
    #[must_use]
    pub fn new(
        storage: Arc<StorageHandle>,
        pane_id: u64,
        capabilities: PaneCapabilities,
        execution_id: impl Into<String>,
    ) -> Self {
        Self {
            storage,
            pane_id,
            pane_meta: PaneMetadata::default(),
            capabilities,
            trigger: None,
            config: WorkflowConfig::default(),
            execution_id: execution_id.into(),
            injector: None,
            action_plan: None,
        }
    }

    /// Set the policy-gated injector for terminal actions
    #[must_use]
    pub fn with_injector(mut self, injector: PolicyInjectorHandle) -> Self {
        self.injector = Some(injector);
        self
    }

    /// Set the triggering event/detection
    #[must_use]
    pub fn with_trigger(mut self, trigger: serde_json::Value) -> Self {
        self.trigger = Some(trigger);
        self
    }

    /// Set custom workflow configuration
    #[must_use]
    pub fn with_config(mut self, config: WorkflowConfig) -> Self {
        self.config = config;
        self
    }

    /// Set cached pane metadata for prompt selection.
    pub fn set_pane_meta(&mut self, meta: PaneMetadata) {
        self.pane_meta = meta;
    }

    /// Get cached pane metadata.
    #[must_use]
    pub fn pane_meta(&self) -> &PaneMetadata {
        &self.pane_meta
    }

    /// Get the storage handle
    #[must_use]
    pub fn storage(&self) -> &Arc<StorageHandle> {
        &self.storage
    }

    /// Get the target pane ID
    #[must_use]
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// Get the current pane capabilities
    #[must_use]
    pub fn capabilities(&self) -> &PaneCapabilities {
        &self.capabilities
    }

    /// Update the pane capabilities snapshot
    pub fn update_capabilities(&mut self, capabilities: PaneCapabilities) {
        self.capabilities = capabilities;
    }

    /// Get the triggering event/detection, if any
    #[must_use]
    pub fn trigger(&self) -> Option<&serde_json::Value> {
        self.trigger.as_ref()
    }

    /// Get the workflow configuration
    #[must_use]
    pub fn config(&self) -> &WorkflowConfig {
        &self.config
    }

    /// Get the execution ID
    #[must_use]
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// Get the default wait timeout from config
    #[must_use]
    pub fn default_wait_timeout_ms(&self) -> u64 {
        self.config.default_wait_timeout_ms
    }

    /// Check if an injector is available for actions
    #[must_use]
    pub fn has_injector(&self) -> bool {
        self.injector.is_some()
    }

    /// Send text to the target pane via policy-gated injection.
    ///
    /// Returns `Ok(InjectionResult)` on success, `Err` if no injector is configured.
    ///
    /// The injection is performed through the `PolicyGatedInjector` which:
    /// - Checks policy authorization
    /// - Emits audit entries
    /// - Only sends if allowed
    ///
    /// Note: The injector lock (tokio::sync::Mutex) is intentionally held across
    /// the `.await` because `inject()` requires `&mut self` for the entire
    /// policy-check-then-send operation. This is safe because:
    /// 1. `inject()` does not re-acquire this lock (no re-entrant locking)
    /// 2. tokio::sync::Mutex is designed for async contexts
    /// 3. The lock ensures atomicity of policy evaluation + send
    pub async fn send_text(
        &mut self,
        text: &str,
    ) -> Result<crate::policy::InjectionResult, &'static str> {
        let injector = self.injector.as_ref().ok_or("No injector configured")?;
        let result = {
            let mut guard = injector.lock().await;
            guard
                .send_text(
                    self.pane_id,
                    text,
                    crate::policy::ActorKind::Workflow,
                    &self.capabilities,
                    Some(&self.execution_id),
                )
                .await
        };
        Ok(result)
    }

    /// Send Ctrl-C (interrupt) to the target pane via policy-gated injection.
    ///
    /// See [`send_text`](Self::send_text) for lock safety rationale.
    pub async fn send_ctrl_c(&mut self) -> Result<crate::policy::InjectionResult, &'static str> {
        let injector = self.injector.as_ref().ok_or("No injector configured")?;
        let result = {
            let mut guard = injector.lock().await;
            guard
                .send_ctrl_c(
                    self.pane_id,
                    crate::policy::ActorKind::Workflow,
                    &self.capabilities,
                    Some(&self.execution_id),
                )
                .await
        };
        Ok(result)
    }

    /// Send Ctrl-D (EOF) to the target pane via policy-gated injection.
    ///
    /// See [`send_text`](Self::send_text) for lock safety rationale.
    pub async fn send_ctrl_d(&mut self) -> Result<crate::policy::InjectionResult, &'static str> {
        let injector = self.injector.as_ref().ok_or("No injector configured")?;
        let result = {
            let mut guard = injector.lock().await;
            guard
                .send_ctrl_d(
                    self.pane_id,
                    crate::policy::ActorKind::Workflow,
                    &self.capabilities,
                    Some(&self.execution_id),
                )
                .await
        };
        Ok(result)
    }

    /// Send Ctrl-Z (suspend) to the target pane via policy-gated injection.
    ///
    /// See [`send_text`](Self::send_text) for lock safety rationale.
    pub async fn send_ctrl_z(&mut self) -> Result<crate::policy::InjectionResult, &'static str> {
        let injector = self.injector.as_ref().ok_or("No injector configured")?;
        let result = {
            let mut guard = injector.lock().await;
            guard
                .send_ctrl_z(
                    self.pane_id,
                    crate::policy::ActorKind::Workflow,
                    &self.capabilities,
                    Some(&self.execution_id),
                )
                .await
        };
        Ok(result)
    }

    // ========================================================================
    // Plan-first execution support (wa-upg.2.3)
    // ========================================================================

    /// Set the action plan for this workflow execution.
    pub fn set_action_plan(&mut self, plan: crate::plan::ActionPlan) {
        self.action_plan = Some(plan);
    }

    /// Get the action plan for this workflow execution, if any.
    #[must_use]
    pub fn action_plan(&self) -> Option<&crate::plan::ActionPlan> {
        self.action_plan.as_ref()
    }

    /// Check if this context is executing in plan-first mode.
    #[must_use]
    pub fn has_action_plan(&self) -> bool {
        self.action_plan.is_some()
    }

    /// Get the step plan for a given step index, if executing in plan-first mode.
    #[must_use]
    pub fn get_step_plan(&self, step_idx: usize) -> Option<&crate::plan::StepPlan> {
        self.action_plan
            .as_ref()
            .and_then(|plan| plan.steps.get(step_idx))
    }

    /// Get the idempotency key for a step, if executing in plan-first mode.
    #[must_use]
    pub fn get_step_idempotency_key(
        &self,
        step_idx: usize,
    ) -> Option<&crate::plan::IdempotencyKey> {
        self.get_step_plan(step_idx).map(|step| &step.step_id)
    }

    /// Get the workspace ID from the action plan.
    #[must_use]
    pub fn workspace_id(&self) -> Option<&str> {
        self.action_plan.as_ref().map(|p| p.workspace_id.as_str())
    }
}
