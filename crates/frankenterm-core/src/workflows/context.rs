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
    /// Note: The injector lock (`runtime_compat::Mutex`) is intentionally held across
    /// the `.await` because `inject()` requires `&mut self` for the entire
    /// policy-check-then-send operation. This is safe because:
    /// 1. `inject()` does not re-acquire this lock (no re-entrant locking)
    /// 2. `runtime_compat::Mutex` is designed for async contexts
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_compat::{CompatRuntime, RuntimeBuilder};
    #[allow(unused_imports)]
    use crate::storage::PaneRecord;

    // ========================================================================
    // PaneMetadata tests
    // ========================================================================

    #[test]
    fn pane_metadata_default_all_none() {
        let meta = PaneMetadata::default();
        assert!(meta.domain.is_none());
        assert!(meta.title.is_none());
        assert!(meta.cwd.is_none());
    }

    #[test]
    fn pane_metadata_from_record() {
        let record = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(0),
            tab_id: Some(0),
            title: Some("vim main.rs".to_string()),
            cwd: Some("/home/user/project".to_string()),
            tty_name: None,
            first_seen_at: 0,
            last_seen_at: 0,
            observed: false,
            ignore_reason: None,
            last_decision_at: None,
        };
        let meta = PaneMetadata::from_record(&record);
        assert_eq!(meta.domain, Some("local".to_string()));
        assert_eq!(meta.title, Some("vim main.rs".to_string()));
        assert_eq!(meta.cwd, Some("/home/user/project".to_string()));
    }

    #[test]
    fn pane_metadata_from_record_with_none_fields() {
        let record = PaneRecord {
            pane_id: 2,
            pane_uuid: None,
            domain: "ssh:remote".to_string(),
            window_id: Some(0),
            tab_id: Some(0),
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: 0,
            last_seen_at: 0,
            observed: true,
            ignore_reason: None,
            last_decision_at: Some(12345),
        };
        let meta = PaneMetadata::from_record(&record);
        assert_eq!(meta.domain, Some("ssh:remote".to_string()));
        assert!(meta.title.is_none());
        assert!(meta.cwd.is_none());
    }

    #[test]
    fn pane_metadata_clone() {
        let meta = PaneMetadata {
            domain: Some("local".into()),
            title: Some("bash".into()),
            cwd: Some("/tmp".into()),
        };
        let cloned = meta.clone();
        assert_eq!(cloned.domain, meta.domain);
        assert_eq!(cloned.title, meta.title);
        assert_eq!(cloned.cwd, meta.cwd);
    }

    // ========================================================================
    // WorkflowConfig tests
    // ========================================================================

    #[test]
    fn workflow_config_default_values() {
        let config = WorkflowConfig::default();
        assert_eq!(config.default_wait_timeout_ms, 30_000);
        assert_eq!(config.max_step_retries, 3);
        assert_eq!(config.retry_delay_ms, 1_000);
    }

    #[test]
    fn workflow_config_serde_roundtrip() {
        let config = WorkflowConfig {
            default_wait_timeout_ms: 60_000,
            max_step_retries: 5,
            retry_delay_ms: 2_500,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: WorkflowConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.default_wait_timeout_ms, 60_000);
        assert_eq!(restored.max_step_retries, 5);
        assert_eq!(restored.retry_delay_ms, 2_500);
    }

    #[test]
    fn workflow_config_clone() {
        let config = WorkflowConfig {
            default_wait_timeout_ms: 10_000,
            max_step_retries: 1,
            retry_delay_ms: 500,
        };
        let cloned = config.clone();
        assert_eq!(
            cloned.default_wait_timeout_ms,
            config.default_wait_timeout_ms
        );
        assert_eq!(cloned.max_step_retries, config.max_step_retries);
        assert_eq!(cloned.retry_delay_ms, config.retry_delay_ms);
    }

    // ========================================================================
    // WorkflowContext tests (sync methods only)
    // ========================================================================

    fn make_storage() -> Arc<StorageHandle> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let rt = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let tmp =
                std::env::temp_dir().join(format!("ft_test_ctx_{}_{}.db", std::process::id(), id,));
            Arc::new(StorageHandle::new(tmp.to_str().unwrap()).await.unwrap())
        })
    }

    #[test]
    fn workflow_context_new_defaults() {
        let storage = make_storage();
        let ctx = WorkflowContext::new(storage, 42, PaneCapabilities::prompt(), "exec-001");
        assert_eq!(ctx.pane_id(), 42);
        assert_eq!(ctx.execution_id(), "exec-001");
        assert!(ctx.trigger().is_none());
        assert!(!ctx.has_injector());
        assert!(!ctx.has_action_plan());
        assert!(ctx.workspace_id().is_none());
        assert_eq!(ctx.default_wait_timeout_ms(), 30_000);
    }

    #[test]
    fn workflow_context_capabilities() {
        let storage = make_storage();
        let ctx = WorkflowContext::new(storage, 1, PaneCapabilities::running(), "exec-002");
        let caps = ctx.capabilities();
        assert!(caps.command_running);
    }

    #[test]
    fn workflow_context_update_capabilities() {
        let storage = make_storage();
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::unknown(), "exec-003");
        assert!(!ctx.capabilities().prompt_active);

        ctx.update_capabilities(PaneCapabilities::prompt());
        assert!(ctx.capabilities().prompt_active);
    }

    #[test]
    fn workflow_context_with_trigger() {
        let storage = make_storage();
        let ctx = WorkflowContext::new(storage, 1, PaneCapabilities::unknown(), "exec-004")
            .with_trigger(serde_json::json!({"rule_id": "test.detected"}));

        let trigger = ctx.trigger().unwrap();
        assert_eq!(trigger["rule_id"], "test.detected");
    }

    #[test]
    fn workflow_context_with_config() {
        let storage = make_storage();
        let custom_config = WorkflowConfig {
            default_wait_timeout_ms: 120_000,
            max_step_retries: 10,
            retry_delay_ms: 5_000,
        };
        let ctx = WorkflowContext::new(storage, 1, PaneCapabilities::unknown(), "exec-005")
            .with_config(custom_config);

        assert_eq!(ctx.config().default_wait_timeout_ms, 120_000);
        assert_eq!(ctx.config().max_step_retries, 10);
        assert_eq!(ctx.config().retry_delay_ms, 5_000);
        assert_eq!(ctx.default_wait_timeout_ms(), 120_000);
    }

    #[test]
    fn workflow_context_pane_meta_set_and_get() {
        let storage = make_storage();
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::unknown(), "exec-006");

        // Default metadata is empty
        assert!(ctx.pane_meta().domain.is_none());

        ctx.set_pane_meta(PaneMetadata {
            domain: Some("local".to_string()),
            title: Some("zsh".to_string()),
            cwd: Some("/usr/local".to_string()),
        });

        assert_eq!(ctx.pane_meta().domain.as_deref(), Some("local"));
        assert_eq!(ctx.pane_meta().title.as_deref(), Some("zsh"));
        assert_eq!(ctx.pane_meta().cwd.as_deref(), Some("/usr/local"));
    }

    #[test]
    fn workflow_context_storage_access() {
        let storage = make_storage();
        let expected_path = storage.db_path().to_string();
        let ctx = WorkflowContext::new(storage, 1, PaneCapabilities::unknown(), "exec-007");
        assert_eq!(ctx.storage().db_path(), expected_path);
    }

    #[test]
    fn workflow_context_clone() {
        let storage = make_storage();
        let ctx = WorkflowContext::new(storage, 99, PaneCapabilities::alt_screen(), "exec-008")
            .with_trigger(serde_json::json!({"key": "value"}));

        let cloned = ctx.clone();
        assert_eq!(cloned.pane_id(), 99);
        assert_eq!(cloned.execution_id(), "exec-008");
        assert!(cloned.trigger().is_some());
    }

    // ========================================================================
    // Plan-first execution support tests
    // ========================================================================

    #[test]
    fn workflow_context_set_action_plan() {
        let storage = make_storage();
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::unknown(), "exec-009");

        assert!(!ctx.has_action_plan());
        assert!(ctx.action_plan().is_none());
        assert!(ctx.get_step_plan(0).is_none());
        assert!(ctx.get_step_idempotency_key(0).is_none());

        let plan = crate::plan::ActionPlan::builder("Test Plan", "ws-1")
            .add_step(crate::plan::StepPlan::new(
                1,
                crate::plan::StepAction::Custom {
                    action_type: "test".to_string(),
                    payload: serde_json::json!({}),
                },
                "Test step",
            ))
            .build();

        ctx.set_action_plan(plan);

        assert!(ctx.has_action_plan());
        assert!(ctx.action_plan().is_some());
        assert_eq!(ctx.workspace_id(), Some("ws-1"));
    }

    #[test]
    fn workflow_context_get_step_plan() {
        let storage = make_storage();
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::unknown(), "exec-010");

        let plan = crate::plan::ActionPlan::builder("Plan", "ws-2")
            .add_step(crate::plan::StepPlan::new(
                1,
                crate::plan::StepAction::Custom {
                    action_type: "step_a".to_string(),
                    payload: serde_json::json!({}),
                },
                "Step A",
            ))
            .add_step(crate::plan::StepPlan::new(
                2,
                crate::plan::StepAction::Custom {
                    action_type: "step_b".to_string(),
                    payload: serde_json::json!({}),
                },
                "Step B",
            ))
            .build();

        ctx.set_action_plan(plan);

        assert!(ctx.get_step_plan(0).is_some());
        assert_eq!(ctx.get_step_plan(0).unwrap().description, "Step A");
        assert!(ctx.get_step_plan(1).is_some());
        assert_eq!(ctx.get_step_plan(1).unwrap().description, "Step B");
        assert!(ctx.get_step_plan(2).is_none()); // out of bounds
    }

    #[test]
    fn workflow_context_get_step_idempotency_key() {
        let storage = make_storage();
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::unknown(), "exec-011");

        let plan = crate::plan::ActionPlan::builder("Plan", "ws-3")
            .add_step(crate::plan::StepPlan::new(
                1,
                crate::plan::StepAction::Custom {
                    action_type: "test".to_string(),
                    payload: serde_json::json!({}),
                },
                "Test",
            ))
            .build();

        ctx.set_action_plan(plan);

        let key = ctx.get_step_idempotency_key(0).unwrap();
        assert!(key.0.starts_with("step:"));
        assert!(ctx.get_step_idempotency_key(1).is_none());
    }
}
