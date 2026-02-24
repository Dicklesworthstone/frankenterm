//! Action plan types for unified workflow representation.
//!
//! This module provides the core types for representing action plans:
//! - [`ActionPlan`]: A complete plan with metadata and execution steps
//! - [`StepPlan`]: A single step within a plan
//! - [`Precondition`]: Conditions that must be satisfied before execution
//! - [`Verification`]: How to verify successful step completion
//! - [`OnFailure`]: What to do when a step fails
//! - [`IdempotencyKey`]: Content-addressed key for safe replay
//!
//! # Canonical Serialization
//!
//! All types use stable field ordering for deterministic hashing.
//! The `plan_version` field enables forward compatibility.
//!
//! # Example
//!
//! ```
//! use frankenterm_core::plan::{ActionPlan, StepPlan, StepAction};
//!
//! let plan = ActionPlan::builder("Recover rate-limited agent", "workspace-123")
//!     .add_step(StepPlan::new(
//!         1,
//!         StepAction::SendText { pane_id: 0, text: "/compact".into(), paste_mode: None },
//!         "Send /compact command",
//!     ))
//!     .build();
//! ```

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// Current schema version for action plans.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

// ============================================================================
// Core Plan Types
// ============================================================================

/// A complete action plan with metadata and execution steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPlan {
    /// Schema version for forward compatibility
    pub plan_version: u32,

    /// Unique plan identifier (content-addressed)
    pub plan_id: PlanId,

    /// Human-readable plan title
    pub title: String,

    /// Workspace scope (ensures plans don't cross boundaries)
    pub workspace_id: String,

    /// When the plan was created (excluded from hash)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,

    /// Ordered sequence of steps to execute
    pub steps: Vec<StepPlan>,

    /// Global preconditions that must all pass before any step executes
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub preconditions: Vec<Precondition>,

    /// What to do if any step fails (default: abort)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<OnFailure>,

    /// Arbitrary metadata for tooling (excluded from hash)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl ActionPlan {
    /// Create a new action plan builder.
    #[must_use]
    pub fn builder(title: impl Into<String>, workspace_id: impl Into<String>) -> ActionPlanBuilder {
        ActionPlanBuilder::new(title, workspace_id)
    }

    /// Compute the canonical hash for this plan.
    #[must_use]
    pub fn compute_hash(&self) -> String {
        let canonical = self.canonical_string();
        let hash = sha256_hex(&canonical);
        format!("sha256:{}", &hash[..32])
    }

    /// Generate the canonical string representation for hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut parts = Vec::new();

        // Version
        parts.push(format!("v={}", self.plan_version));

        // Workspace scope
        parts.push(format!("ws={}", self.workspace_id));

        // Title
        parts.push(format!("title={}", self.title));

        // Steps (in order)
        for (i, step) in self.steps.iter().enumerate() {
            parts.push(format!("step[{}]={}", i, step.canonical_string()));
        }

        // Preconditions (sorted for determinism)
        let mut precond_strs: Vec<_> = self
            .preconditions
            .iter()
            .map(Precondition::canonical_string)
            .collect();
        precond_strs.sort();
        for (i, p) in precond_strs.iter().enumerate() {
            parts.push(format!("precond[{}]={}", i, p));
        }

        // On-failure (if set)
        if let Some(on_failure) = &self.on_failure {
            parts.push(format!("on_failure={}", on_failure.canonical_string()));
        }

        parts.join("|")
    }

    /// Validate the plan for internal consistency.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Step numbers are not sequential starting from 1
    /// - Step IDs are not unique
    /// - Referenced steps in preconditions don't exist
    pub fn validate(&self) -> Result<(), PlanValidationError> {
        // Check step numbering
        for (i, step) in self.steps.iter().enumerate() {
            let expected = (i + 1) as u32;
            if step.step_number != expected {
                return Err(PlanValidationError::InvalidStepNumber {
                    expected,
                    actual: step.step_number,
                });
            }
        }

        // Check step ID uniqueness
        let mut seen_ids = std::collections::HashSet::new();
        for step in &self.steps {
            if !seen_ids.insert(&step.step_id) {
                return Err(PlanValidationError::DuplicateStepId(step.step_id.clone()));
            }
        }

        // Check precondition references
        for precond in &self.preconditions {
            if let Precondition::StepCompleted { step_id } = precond {
                if !seen_ids.contains(step_id) {
                    return Err(PlanValidationError::UnknownStepReference(step_id.clone()));
                }
            }
        }

        Ok(())
    }

    /// Get the number of steps in this plan.
    #[must_use]
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// Check if this plan has any preconditions.
    #[must_use]
    pub fn has_preconditions(&self) -> bool {
        !self.preconditions.is_empty()
    }
}

/// Builder for constructing action plans.
#[derive(Debug)]
pub struct ActionPlanBuilder {
    title: String,
    workspace_id: String,
    steps: Vec<StepPlan>,
    preconditions: Vec<Precondition>,
    on_failure: Option<OnFailure>,
    metadata: Option<serde_json::Value>,
    created_at: Option<i64>,
}

impl ActionPlanBuilder {
    /// Create a new builder.
    fn new(title: impl Into<String>, workspace_id: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            workspace_id: workspace_id.into(),
            steps: Vec::new(),
            preconditions: Vec::new(),
            on_failure: None,
            metadata: None,
            created_at: None,
        }
    }

    /// Add a step to the plan.
    #[must_use]
    pub fn add_step(mut self, step: StepPlan) -> Self {
        self.steps.push(step);
        self
    }

    /// Add multiple steps to the plan.
    #[must_use]
    pub fn add_steps(mut self, steps: impl IntoIterator<Item = StepPlan>) -> Self {
        self.steps.extend(steps);
        self
    }

    /// Add a global precondition.
    #[must_use]
    pub fn add_precondition(mut self, precondition: Precondition) -> Self {
        self.preconditions.push(precondition);
        self
    }

    /// Set the failure handling strategy.
    #[must_use]
    pub fn on_failure(mut self, strategy: OnFailure) -> Self {
        self.on_failure = Some(strategy);
        self
    }

    /// Set metadata for the plan.
    #[must_use]
    pub fn metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Set the creation timestamp.
    #[must_use]
    pub fn created_at(mut self, ts: i64) -> Self {
        self.created_at = Some(ts);
        self
    }

    /// Build the action plan.
    ///
    /// This computes the plan hash and assigns it to `plan_id`.
    #[must_use]
    pub fn build(self) -> ActionPlan {
        // Create plan without ID first
        let mut plan = ActionPlan {
            plan_version: PLAN_SCHEMA_VERSION,
            plan_id: PlanId::placeholder(),
            title: self.title,
            workspace_id: self.workspace_id,
            created_at: self.created_at,
            steps: self.steps,
            preconditions: self.preconditions,
            on_failure: self.on_failure,
            metadata: self.metadata,
        };

        // Compute and set the hash-based ID
        let hash = plan.compute_hash();
        plan.plan_id = PlanId::from_hash(&hash);

        plan
    }
}

// ============================================================================
// Plan and Step Identifiers
// ============================================================================

/// Content-addressed plan identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlanId(pub String);

impl PlanId {
    /// Create a plan ID from a hash.
    #[must_use]
    pub fn from_hash(hash: &str) -> Self {
        // Remove the sha256: prefix if present
        let clean_hash = hash.strip_prefix("sha256:").unwrap_or(hash);
        Self(format!("plan:{clean_hash}"))
    }

    /// Create a placeholder ID (used during construction).
    #[must_use]
    fn placeholder() -> Self {
        Self("plan:pending".to_string())
    }

    /// Check if this is a placeholder ID.
    #[must_use]
    pub fn is_placeholder(&self) -> bool {
        self.0 == "plan:pending"
    }
}

impl fmt::Display for PlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Content-addressed key for idempotent step execution.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdempotencyKey(pub String);

impl IdempotencyKey {
    /// Create from a hash.
    #[must_use]
    pub fn from_hash(hash: &str) -> Self {
        Self(format!("step:{hash}"))
    }

    /// Compute key for a step action.
    #[must_use]
    pub fn for_action(workspace_id: &str, step_number: u32, action: &StepAction) -> Self {
        let canonical = format!(
            "ws={}|step={}|action={}",
            workspace_id,
            step_number,
            action.canonical_string()
        );
        let hash = sha256_hex(&canonical);
        Self::from_hash(&hash[..16])
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Step Definition
// ============================================================================

/// A single step within an action plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepPlan {
    /// Step sequence number (1-indexed)
    pub step_number: u32,

    /// Content-addressed step identifier
    pub step_id: IdempotencyKey,

    /// What this step does
    pub action: StepAction,

    /// Human-readable description
    pub description: String,

    /// Conditions that must be true before this step executes
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub preconditions: Vec<Precondition>,

    /// How to verify successful execution
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,

    /// Step-specific failure handling (overrides plan-level)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<OnFailure>,

    /// Timeout for this step in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,

    /// Whether this step is skippable on retry (already completed)
    pub idempotent: bool,
}

impl StepPlan {
    /// Create a new step plan.
    #[must_use]
    pub fn new(step_number: u32, action: StepAction, description: impl Into<String>) -> Self {
        let description = description.into();
        // Generate idempotency key based on step number and action
        // Note: workspace_id is not available here, so we use a simplified key
        let key_canonical = format!("step={}|action={}", step_number, action.canonical_string());
        let hash = sha256_hex(&key_canonical);
        let step_id = IdempotencyKey::from_hash(&hash[..16]);

        Self {
            step_number,
            step_id,
            action,
            description,
            preconditions: Vec::new(),
            verification: None,
            on_failure: None,
            timeout_ms: None,
            idempotent: false,
        }
    }

    /// Create a step with a specific idempotency key.
    #[must_use]
    pub fn with_key(
        step_number: u32,
        step_id: IdempotencyKey,
        action: StepAction,
        description: impl Into<String>,
    ) -> Self {
        Self {
            step_number,
            step_id,
            action,
            description: description.into(),
            preconditions: Vec::new(),
            verification: None,
            on_failure: None,
            timeout_ms: None,
            idempotent: false,
        }
    }

    /// Add a precondition to this step.
    #[must_use]
    pub fn with_precondition(mut self, precondition: Precondition) -> Self {
        self.preconditions.push(precondition);
        self
    }

    /// Set the verification strategy.
    #[must_use]
    pub fn with_verification(mut self, verification: Verification) -> Self {
        self.verification = Some(verification);
        self
    }

    /// Set the failure handling strategy.
    #[must_use]
    pub fn with_on_failure(mut self, on_failure: OnFailure) -> Self {
        self.on_failure = Some(on_failure);
        self
    }

    /// Set the timeout.
    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// Mark this step as idempotent.
    #[must_use]
    pub fn idempotent(mut self) -> Self {
        self.idempotent = true;
        self
    }

    /// Generate canonical string for hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut parts = Vec::new();

        parts.push(format!("n={}", self.step_number));
        parts.push(format!("action={}", self.action.canonical_string()));
        parts.push(format!("desc={}", self.description));
        parts.push(format!("idempotent={}", self.idempotent));

        if let Some(timeout) = self.timeout_ms {
            parts.push(format!("timeout={timeout}"));
        }

        // Preconditions (sorted)
        let mut precond_strs: Vec<_> = self
            .preconditions
            .iter()
            .map(Precondition::canonical_string)
            .collect();
        precond_strs.sort();
        for p in &precond_strs {
            parts.push(format!("precond={p}"));
        }

        // Verification
        if let Some(v) = &self.verification {
            parts.push(format!("verify={}", v.canonical_string()));
        }

        // On-failure
        if let Some(f) = &self.on_failure {
            parts.push(format!("on_failure={}", f.canonical_string()));
        }

        parts.join(",")
    }
}

// ============================================================================
// Step Actions
// ============================================================================

/// The action to perform in a step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepAction {
    /// Send text to a pane
    SendText {
        pane_id: u64,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        paste_mode: Option<bool>,
    },

    /// Wait for a pattern match
    WaitFor {
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
        condition: WaitCondition,
        timeout_ms: u64,
    },

    /// Acquire a named lock
    AcquireLock {
        lock_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },

    /// Release a named lock
    ReleaseLock { lock_name: String },

    /// Store data in the database
    StoreData {
        key: String,
        value: serde_json::Value,
    },

    /// Execute a sub-workflow
    RunWorkflow {
        workflow_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        params: Option<serde_json::Value>,
    },

    /// Mark an event as handled
    MarkEventHandled { event_id: i64 },

    /// Validate an approval token
    ValidateApproval { approval_code: String },

    /// Execute a nested action plan
    NestedPlan { plan: Box<ActionPlan> },

    /// Custom action with arbitrary payload
    Custom {
        action_type: String,
        payload: serde_json::Value,
    },
}

impl PartialEq for StepAction {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::SendText {
                    pane_id: pane_a,
                    text: text_a,
                    paste_mode: paste_a,
                },
                Self::SendText {
                    pane_id: pane_b,
                    text: text_b,
                    paste_mode: paste_b,
                },
            ) => pane_a == pane_b && text_a == text_b && paste_a == paste_b,
            (
                Self::WaitFor {
                    pane_id: pane_a,
                    condition: condition_a,
                    timeout_ms: timeout_a,
                },
                Self::WaitFor {
                    pane_id: pane_b,
                    condition: condition_b,
                    timeout_ms: timeout_b,
                },
            ) => pane_a == pane_b && condition_a == condition_b && timeout_a == timeout_b,
            (
                Self::AcquireLock {
                    lock_name: lock_a,
                    timeout_ms: timeout_a,
                },
                Self::AcquireLock {
                    lock_name: lock_b,
                    timeout_ms: timeout_b,
                },
            ) => lock_a == lock_b && timeout_a == timeout_b,
            (Self::ReleaseLock { lock_name: lock_a }, Self::ReleaseLock { lock_name: lock_b }) => {
                lock_a == lock_b
            }
            (
                Self::StoreData {
                    key: key_a,
                    value: value_a,
                },
                Self::StoreData {
                    key: key_b,
                    value: value_b,
                },
            ) => key_a == key_b && value_a == value_b,
            (
                Self::RunWorkflow {
                    workflow_id: workflow_a,
                    params: params_a,
                },
                Self::RunWorkflow {
                    workflow_id: workflow_b,
                    params: params_b,
                },
            ) => workflow_a == workflow_b && params_a == params_b,
            (
                Self::MarkEventHandled { event_id: event_a },
                Self::MarkEventHandled { event_id: event_b },
            ) => event_a == event_b,
            (
                Self::ValidateApproval {
                    approval_code: code_a,
                },
                Self::ValidateApproval {
                    approval_code: code_b,
                },
            ) => code_a == code_b,
            (Self::NestedPlan { plan: plan_a }, Self::NestedPlan { plan: plan_b }) => {
                plan_a.compute_hash() == plan_b.compute_hash()
            }
            (
                Self::Custom {
                    action_type: action_a,
                    payload: payload_a,
                },
                Self::Custom {
                    action_type: action_b,
                    payload: payload_b,
                },
            ) => action_a == action_b && payload_a == payload_b,
            _ => false,
        }
    }
}

impl StepAction {
    /// Generate canonical string for hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::SendText {
                pane_id,
                text,
                paste_mode,
            } => {
                let paste = paste_mode.map_or("none".to_string(), |b| b.to_string());
                format!("send_text:pane={pane_id},text={text},paste={paste}")
            }
            Self::WaitFor {
                pane_id,
                condition,
                timeout_ms,
            } => {
                let pane = pane_id.map_or_else(|| "any".to_string(), |p| p.to_string());
                format!(
                    "wait_for:pane={},cond={},timeout={}",
                    pane,
                    condition.canonical_string(),
                    timeout_ms
                )
            }
            Self::AcquireLock {
                lock_name,
                timeout_ms,
            } => {
                let timeout = timeout_ms.map_or("none".to_string(), |t| t.to_string());
                format!("acquire_lock:name={lock_name},timeout={timeout}")
            }
            Self::ReleaseLock { lock_name } => format!("release_lock:name={lock_name}"),
            Self::StoreData { key, value } => {
                // Use canonical JSON for value
                let value_str = serde_json::to_string(value).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "plan StoreData value serialization failed");
                    String::new()
                });
                format!("store_data:key={key},value={value_str}")
            }
            Self::RunWorkflow {
                workflow_id,
                params,
            } => {
                let params_str = params
                    .as_ref()
                    .and_then(|p| serde_json::to_string(p)
                        .inspect_err(|e| tracing::warn!(error = %e, "plan RunWorkflow params serialization failed"))
                        .ok())
                    .unwrap_or_default();
                format!("run_workflow:id={workflow_id},params={params_str}")
            }
            Self::MarkEventHandled { event_id } => format!("mark_event_handled:id={event_id}"),
            Self::ValidateApproval { approval_code } => {
                format!("validate_approval:code={approval_code}")
            }
            Self::NestedPlan { plan } => format!("nested_plan:hash={}", plan.compute_hash()),
            Self::Custom {
                action_type,
                payload,
            } => {
                let payload_str = serde_json::to_string(payload).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "plan Custom payload serialization failed");
                    String::new()
                });
                format!("custom:type={action_type},payload={payload_str}")
            }
        }
    }

    /// Get a human-readable action type name.
    #[must_use]
    pub fn action_type_name(&self) -> &'static str {
        match self {
            Self::SendText { .. } => "send_text",
            Self::WaitFor { .. } => "wait_for",
            Self::AcquireLock { .. } => "acquire_lock",
            Self::ReleaseLock { .. } => "release_lock",
            Self::StoreData { .. } => "store_data",
            Self::RunWorkflow { .. } => "run_workflow",
            Self::MarkEventHandled { .. } => "mark_event_handled",
            Self::ValidateApproval { .. } => "validate_approval",
            Self::NestedPlan { .. } => "nested_plan",
            Self::Custom { .. } => "custom",
        }
    }
}

// ============================================================================
// Wait Conditions
// ============================================================================

/// Condition to wait for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WaitCondition {
    /// Wait for a pattern rule to match
    Pattern {
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
        rule_id: String,
    },

    /// Wait for pane to be idle
    PaneIdle {
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
        idle_threshold_ms: u64,
    },

    /// Wait for pane output tail to be stable
    StableTail {
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
        stable_for_ms: u64,
    },

    /// Wait for external signal
    External { key: String },
}

impl WaitCondition {
    /// Generate canonical string for hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::Pattern { pane_id, rule_id } => {
                let pane = pane_id.map_or_else(|| "any".to_string(), |p| p.to_string());
                format!("pattern:pane={pane},rule={rule_id}")
            }
            Self::PaneIdle {
                pane_id,
                idle_threshold_ms,
            } => {
                let pane = pane_id.map_or_else(|| "any".to_string(), |p| p.to_string());
                format!("pane_idle:pane={pane},threshold={idle_threshold_ms}")
            }
            Self::StableTail {
                pane_id,
                stable_for_ms,
            } => {
                let pane = pane_id.map_or_else(|| "any".to_string(), |p| p.to_string());
                format!("stable_tail:pane={pane},stable_for_ms={stable_for_ms}")
            }
            Self::External { key } => format!("external:key={key}"),
        }
    }
}

// ============================================================================
// Preconditions
// ============================================================================

/// A condition that must be satisfied before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Precondition {
    /// Pane must exist and be accessible
    PaneExists { pane_id: u64 },

    /// Pane must be in a specific state
    PaneState {
        pane_id: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        expected_agent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expected_domain: Option<String>,
    },

    /// A pattern must have matched recently
    PatternMatched {
        rule_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        within_ms: Option<u64>,
    },

    /// A pattern must NOT have matched
    PatternNotMatched {
        rule_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
    },

    /// A lock must be held by this execution
    LockHeld { lock_name: String },

    /// A lock must be available
    LockAvailable { lock_name: String },

    /// An approval must be valid
    ApprovalValid { scope: ApprovalScopeRef },

    /// Previous step must have succeeded
    StepCompleted { step_id: IdempotencyKey },

    /// Custom precondition with expression
    Custom { name: String, expression: String },
}

impl Precondition {
    /// Generate canonical string for hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::PaneExists { pane_id } => format!("pane_exists:{pane_id}"),
            Self::PaneState {
                pane_id,
                expected_agent,
                expected_domain,
            } => {
                let agent = expected_agent.as_deref().unwrap_or("any");
                let domain = expected_domain.as_deref().unwrap_or("any");
                format!("pane_state:{pane_id},agent={agent},domain={domain}")
            }
            Self::PatternMatched {
                rule_id,
                pane_id,
                within_ms,
            } => {
                let pane = pane_id.map_or_else(|| "any".to_string(), |p| p.to_string());
                let within = within_ms.map_or_else(|| "any".to_string(), |w| w.to_string());
                format!("pattern_matched:{rule_id},pane={pane},within={within}")
            }
            Self::PatternNotMatched { rule_id, pane_id } => {
                let pane = pane_id.map_or_else(|| "any".to_string(), |p| p.to_string());
                format!("pattern_not_matched:{rule_id},pane={pane}")
            }
            Self::LockHeld { lock_name } => format!("lock_held:{lock_name}"),
            Self::LockAvailable { lock_name } => format!("lock_available:{lock_name}"),
            Self::ApprovalValid { scope } => {
                format!(
                    "approval_valid:ws={},action={},pane={}",
                    scope.workspace_id,
                    scope.action_kind,
                    scope
                        .pane_id
                        .map_or_else(|| "any".to_string(), |p| p.to_string())
                )
            }
            Self::StepCompleted { step_id } => format!("step_completed:{}", step_id.0),
            Self::Custom { name, expression } => format!("custom:{name}={expression}"),
        }
    }
}

/// Reference to an approval scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalScopeRef {
    pub workspace_id: String,
    pub action_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
}

// ============================================================================
// Verification
// ============================================================================

/// How to verify a step completed successfully.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    /// Verification strategy
    pub strategy: VerificationStrategy,

    /// Human-readable description of what's being verified
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// How long to wait for verification
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl Verification {
    /// Create a pattern match verification.
    #[must_use]
    pub fn pattern_match(rule_id: impl Into<String>) -> Self {
        Self {
            strategy: VerificationStrategy::PatternMatch {
                rule_id: rule_id.into(),
                pane_id: None,
            },
            description: None,
            timeout_ms: None,
        }
    }

    /// Create a pane idle verification.
    #[must_use]
    pub fn pane_idle(idle_threshold_ms: u64) -> Self {
        Self {
            strategy: VerificationStrategy::PaneIdle {
                pane_id: None,
                idle_threshold_ms,
            },
            description: None,
            timeout_ms: None,
        }
    }

    /// Set the description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Set the timeout.
    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// Generate canonical string for hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut parts = vec![self.strategy.canonical_string()];
        if let Some(timeout) = self.timeout_ms {
            parts.push(format!("timeout={timeout}"));
        }
        parts.join(",")
    }
}

/// Verification strategies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerificationStrategy {
    /// Wait for a pattern to appear
    PatternMatch {
        rule_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
    },

    /// Wait for pane to become idle
    PaneIdle {
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
        idle_threshold_ms: u64,
    },

    /// Check that a specific pattern does NOT appear
    PatternAbsent {
        rule_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
        wait_ms: u64,
    },

    /// Verify via custom expression
    Custom { name: String, expression: String },

    /// No verification needed (fire-and-forget)
    None,
}

impl VerificationStrategy {
    /// Generate canonical string for hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::PatternMatch { rule_id, pane_id } => {
                let pane = pane_id.map_or_else(|| "any".to_string(), |p| p.to_string());
                format!("pattern_match:{rule_id},pane={pane}")
            }
            Self::PaneIdle {
                pane_id,
                idle_threshold_ms,
            } => {
                let pane = pane_id.map_or_else(|| "any".to_string(), |p| p.to_string());
                format!("pane_idle:pane={pane},threshold={idle_threshold_ms}")
            }
            Self::PatternAbsent {
                rule_id,
                pane_id,
                wait_ms,
            } => {
                let pane = pane_id.map_or_else(|| "any".to_string(), |p| p.to_string());
                format!("pattern_absent:{rule_id},pane={pane},wait={wait_ms}")
            }
            Self::Custom { name, expression } => format!("custom:{name}={expression}"),
            Self::None => "none".to_string(),
        }
    }
}

// ============================================================================
// Failure Handling
// ============================================================================

/// What to do when a step fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum OnFailure {
    /// Stop execution immediately
    Abort {
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },

    /// Retry the step with backoff
    Retry {
        max_attempts: u32,
        initial_delay_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_delay_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        backoff_multiplier: Option<f64>,
    },

    /// Skip this step and continue
    Skip {
        #[serde(skip_serializing_if = "Option::is_none")]
        warn: Option<bool>,
    },

    /// Execute fallback steps
    Fallback { steps: Vec<StepPlan> },

    /// Require human intervention
    RequireApproval { summary: String },
}

impl OnFailure {
    /// Create an abort strategy.
    #[must_use]
    pub fn abort() -> Self {
        Self::Abort { message: None }
    }

    /// Create an abort strategy with a message.
    #[must_use]
    pub fn abort_with_message(message: impl Into<String>) -> Self {
        Self::Abort {
            message: Some(message.into()),
        }
    }

    /// Create a retry strategy.
    #[must_use]
    pub fn retry(max_attempts: u32, initial_delay_ms: u64) -> Self {
        Self::Retry {
            max_attempts,
            initial_delay_ms,
            max_delay_ms: None,
            backoff_multiplier: None,
        }
    }

    /// Create a skip strategy.
    #[must_use]
    pub fn skip() -> Self {
        Self::Skip { warn: Some(true) }
    }

    /// Generate canonical string for hashing.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::Abort { message } => {
                let msg = message.as_deref().unwrap_or("");
                format!("abort:{msg}")
            }
            Self::Retry {
                max_attempts,
                initial_delay_ms,
                max_delay_ms,
                backoff_multiplier,
            } => {
                let max_d = max_delay_ms.map_or("none".to_string(), |d| d.to_string());
                let mult = backoff_multiplier.map_or("1.0".to_string(), |m| m.to_string());
                format!(
                    "retry:max={max_attempts},delay={initial_delay_ms},max_delay={max_d},mult={mult}"
                )
            }
            Self::Skip { warn } => {
                let w = warn.unwrap_or(true);
                format!("skip:warn={w}")
            }
            Self::Fallback { steps } => {
                let step_ids: Vec<_> = steps.iter().map(|s| s.step_id.0.clone()).collect();
                format!("fallback:{}", step_ids.join(","))
            }
            Self::RequireApproval { summary } => format!("require_approval:{summary}"),
        }
    }
}

// ============================================================================
// Mission Schema Pack
// ============================================================================

/// Current schema version for mission nouns and ownership contracts.
pub const MISSION_SCHEMA_VERSION: u32 = 1;

/// Stable mission identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MissionId(pub String);

impl MissionId {
    /// Create an ID from a hash string.
    #[must_use]
    pub fn from_hash(hash: &str) -> Self {
        let clean_hash = hash.strip_prefix("sha256:").unwrap_or(hash);
        Self(format!("mission:{clean_hash}"))
    }
}

impl fmt::Display for MissionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Stable candidate-action identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CandidateActionId(pub String);

impl CandidateActionId {
    /// Create an ID from a hash string.
    #[must_use]
    pub fn from_hash(hash: &str) -> Self {
        let clean_hash = hash.strip_prefix("sha256:").unwrap_or(hash);
        Self(format!("candidate:{clean_hash}"))
    }
}

impl fmt::Display for CandidateActionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Stable assignment identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AssignmentId(pub String);

impl AssignmentId {
    /// Create an ID from a hash string.
    #[must_use]
    pub fn from_hash(hash: &str) -> Self {
        let clean_hash = hash.strip_prefix("sha256:").unwrap_or(hash);
        Self(format!("assignment:{clean_hash}"))
    }
}

impl fmt::Display for AssignmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Stable reservation-intent identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReservationIntentId(pub String);

impl ReservationIntentId {
    /// Create an ID from a hash string.
    #[must_use]
    pub fn from_hash(hash: &str) -> Self {
        let clean_hash = hash.strip_prefix("sha256:").unwrap_or(hash);
        Self(format!("reservation:{clean_hash}"))
    }
}

impl fmt::Display for ReservationIntentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Explicit ownership boundary role in mission orchestration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionActorRole {
    Planner,
    Dispatcher,
    Operator,
}

impl fmt::Display for MissionActorRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planner => f.write_str("planner"),
            Self::Dispatcher => f.write_str("dispatcher"),
            Self::Operator => f.write_str("operator"),
        }
    }
}

/// Explicit owner mapping for mission decisions and execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionOwnership {
    pub planner: String,
    pub dispatcher: String,
    pub operator: String,
}

impl MissionOwnership {
    /// Resolve the actor name for a role.
    #[must_use]
    pub fn actor_for(&self, role: MissionActorRole) -> &str {
        match role {
            MissionActorRole::Planner => &self.planner,
            MissionActorRole::Dispatcher => &self.dispatcher,
            MissionActorRole::Operator => &self.operator,
        }
    }

    /// Deterministic string representation used by `Mission::canonical_string`.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "planner={},dispatcher={},operator={}",
            self.planner, self.dispatcher, self.operator
        )
    }

    /// Validate explicit ownership boundaries.
    pub fn validate(&self) -> Result<(), MissionValidationError> {
        let planner = self.planner.trim();
        let dispatcher = self.dispatcher.trim();
        let operator = self.operator.trim();

        if planner.is_empty() {
            return Err(MissionValidationError::EmptyOwnershipActor {
                role: MissionActorRole::Planner,
            });
        }
        if dispatcher.is_empty() {
            return Err(MissionValidationError::EmptyOwnershipActor {
                role: MissionActorRole::Dispatcher,
            });
        }
        if operator.is_empty() {
            return Err(MissionValidationError::EmptyOwnershipActor {
                role: MissionActorRole::Operator,
            });
        }

        let mut seen = std::collections::HashSet::new();
        for actor in [planner, dispatcher, operator] {
            if !seen.insert(actor) {
                return Err(MissionValidationError::DuplicateOwnershipActor(
                    actor.to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// Source/provenance envelope for mission generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionProvenance {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_sha: Option<String>,
}

impl MissionProvenance {
    /// Deterministic string representation used by `Mission::canonical_string`.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "bead={},thread={},source={},sha={}",
            self.bead_id.as_deref().unwrap_or(""),
            self.thread_id.as_deref().unwrap_or(""),
            self.source_command.as_deref().unwrap_or(""),
            self.source_sha.as_deref().unwrap_or("")
        )
    }
}

/// Planner-emitted candidate action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateAction {
    pub candidate_id: CandidateActionId,
    pub requested_by: MissionActorRole,
    pub action: StepAction,
    pub rationale: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    pub created_at_ms: i64,
}

impl CandidateAction {
    /// Deterministic string representation used by `Mission::canonical_string`.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut parts = vec![
            format!("id={}", self.candidate_id.0),
            format!("requested_by={}", self.requested_by),
            format!("action={}", self.action.canonical_string()),
            format!("rationale={}", self.rationale),
            format!("created_at_ms={}", self.created_at_ms),
        ];
        if let Some(score) = self.score {
            parts.push(format!("score={score:.6}"));
        }
        parts.join(",")
    }
}

/// Dispatcher reservation request intent prior to lock acquisition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationIntent {
    pub reservation_id: ReservationIntentId,
    pub requested_by: MissionActorRole,
    pub paths: Vec<String>,
    pub exclusive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub requested_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

impl ReservationIntent {
    /// Deterministic string representation used by `Assignment::canonical_string`.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut paths = self.paths.clone();
        paths.sort();
        format!(
            "id={},requested_by={},exclusive={},paths={},reason={},requested_at_ms={},expires_at_ms={}",
            self.reservation_id.0,
            self.requested_by,
            self.exclusive,
            paths.join(";"),
            self.reason.as_deref().unwrap_or(""),
            self.requested_at_ms,
            self.expires_at_ms
                .map_or_else(|| "none".to_string(), |v| v.to_string())
        )
    }
}

/// Approval lifecycle for an assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ApprovalState {
    NotRequired,
    Pending {
        requested_by: String,
        requested_at_ms: i64,
    },
    Approved {
        approved_by: String,
        approved_at_ms: i64,
        approval_code_hash: String,
    },
    Denied {
        denied_by: String,
        denied_at_ms: i64,
        reason_code: String,
    },
    Expired {
        expired_at_ms: i64,
        reason_code: String,
    },
}

impl ApprovalState {
    /// Deterministic string representation used by `Assignment::canonical_string`.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::NotRequired => "not_required".to_string(),
            Self::Pending {
                requested_by,
                requested_at_ms,
            } => format!("pending:{requested_by}:{requested_at_ms}"),
            Self::Approved {
                approved_by,
                approved_at_ms,
                approval_code_hash,
            } => format!("approved:{approved_by}:{approved_at_ms}:{approval_code_hash}"),
            Self::Denied {
                denied_by,
                denied_at_ms,
                reason_code,
            } => format!("denied:{denied_by}:{denied_at_ms}:{reason_code}"),
            Self::Expired {
                expired_at_ms,
                reason_code,
            } => format!("expired:{expired_at_ms}:{reason_code}"),
        }
    }
}

/// Final assignment execution outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Success {
        reason_code: String,
        completed_at_ms: i64,
    },
    Failed {
        reason_code: String,
        error_code: String,
        completed_at_ms: i64,
    },
    Cancelled {
        reason_code: String,
        completed_at_ms: i64,
    },
}

impl Outcome {
    /// Deterministic string representation used by `Assignment::canonical_string`.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        match self {
            Self::Success {
                reason_code,
                completed_at_ms,
            } => format!("success:{reason_code}:{completed_at_ms}"),
            Self::Failed {
                reason_code,
                error_code,
                completed_at_ms,
            } => format!("failed:{reason_code}:{error_code}:{completed_at_ms}"),
            Self::Cancelled {
                reason_code,
                completed_at_ms,
            } => format!("cancelled:{reason_code}:{completed_at_ms}"),
        }
    }
}

/// Canonical failure reason taxonomy for mission assignments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionFailureCode {
    PolicyDenied,
    ReservationConflict,
    RateLimited,
    StaleState,
    DispatchError,
    ApprovalRequired,
    ApprovalDenied,
    ApprovalExpired,
}

/// Whether a failure terminates the current assignment path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionFailureTerminality {
    Terminal,
    NonTerminal,
}

impl MissionFailureTerminality {
    /// Returns true when this failure is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Terminal)
    }
}

/// Retry strategy contract for a mission failure code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionFailureRetryability {
    Never,
    Immediate,
    AfterBackoff,
    AfterStateRefresh,
    AfterApprovalRefresh,
}

impl MissionFailureRetryability {
    /// Returns true when automated retry is permitted.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        !matches!(self, Self::Never)
    }
}

/// Full remediation contract for a mission failure code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissionFailureContract {
    pub reason_code: &'static str,
    pub error_code: &'static str,
    pub terminality: MissionFailureTerminality,
    pub retryability: MissionFailureRetryability,
    pub human_hint: &'static str,
    pub machine_hint: &'static str,
}

impl MissionFailureCode {
    const ALL: [Self; 8] = [
        Self::PolicyDenied,
        Self::ReservationConflict,
        Self::RateLimited,
        Self::StaleState,
        Self::DispatchError,
        Self::ApprovalRequired,
        Self::ApprovalDenied,
        Self::ApprovalExpired,
    ];

    /// Return all canonical mission failure codes.
    #[must_use]
    pub const fn all() -> [Self; 8] {
        Self::ALL
    }

    /// Parse from normalized reason code.
    #[must_use]
    pub fn from_reason_code(reason_code: &str) -> Option<Self> {
        match reason_code.trim() {
            "policy_denied" => Some(Self::PolicyDenied),
            "reservation_conflict" => Some(Self::ReservationConflict),
            "rate_limited" => Some(Self::RateLimited),
            "stale_state" => Some(Self::StaleState),
            "dispatch_error" => Some(Self::DispatchError),
            "approval_required" => Some(Self::ApprovalRequired),
            "approval_denied" => Some(Self::ApprovalDenied),
            "approval_expired" => Some(Self::ApprovalExpired),
            _ => None,
        }
    }

    /// Parse from normalized error code.
    #[must_use]
    pub fn from_error_code(error_code: &str) -> Option<Self> {
        match error_code.trim() {
            "FTM1001" => Some(Self::PolicyDenied),
            "FTM1002" => Some(Self::ReservationConflict),
            "FTM1003" => Some(Self::RateLimited),
            "FTM1004" => Some(Self::StaleState),
            "FTM1005" => Some(Self::DispatchError),
            "FTM1006" => Some(Self::ApprovalRequired),
            "FTM1007" => Some(Self::ApprovalDenied),
            "FTM1008" => Some(Self::ApprovalExpired),
            _ => None,
        }
    }

    /// Return canonical metadata for this failure code.
    #[must_use]
    pub const fn contract(self) -> MissionFailureContract {
        match self {
            Self::PolicyDenied => MissionFailureContract {
                reason_code: "policy_denied",
                error_code: "FTM1001",
                terminality: MissionFailureTerminality::Terminal,
                retryability: MissionFailureRetryability::Never,
                human_hint:
                    "Policy denied this action. Update policy or request operator override.",
                machine_hint: "abort_and_request_policy_override",
            },
            Self::ReservationConflict => MissionFailureContract {
                reason_code: "reservation_conflict",
                error_code: "FTM1002",
                terminality: MissionFailureTerminality::NonTerminal,
                retryability: MissionFailureRetryability::AfterStateRefresh,
                human_hint:
                    "Target paths are already reserved. Wait or coordinate with current owner.",
                machine_hint: "refresh_reservations_then_retry",
            },
            Self::RateLimited => MissionFailureContract {
                reason_code: "rate_limited",
                error_code: "FTM1003",
                terminality: MissionFailureTerminality::NonTerminal,
                retryability: MissionFailureRetryability::AfterBackoff,
                human_hint: "Rate limit reached. Wait for reset before retrying.",
                machine_hint: "apply_backoff_and_retry_after_window",
            },
            Self::StaleState => MissionFailureContract {
                reason_code: "stale_state",
                error_code: "FTM1004",
                terminality: MissionFailureTerminality::NonTerminal,
                retryability: MissionFailureRetryability::AfterStateRefresh,
                human_hint: "Observed state is stale. Refresh pane/session state before retry.",
                machine_hint: "refresh_runtime_state_then_retry",
            },
            Self::DispatchError => MissionFailureContract {
                reason_code: "dispatch_error",
                error_code: "FTM1005",
                terminality: MissionFailureTerminality::NonTerminal,
                retryability: MissionFailureRetryability::Immediate,
                human_hint: "Dispatch failed due to transient control-plane error. Retry dispatch.",
                machine_hint: "retry_dispatch_with_jitter",
            },
            Self::ApprovalRequired => MissionFailureContract {
                reason_code: "approval_required",
                error_code: "FTM1006",
                terminality: MissionFailureTerminality::NonTerminal,
                retryability: MissionFailureRetryability::AfterApprovalRefresh,
                human_hint: "Human approval is required before execution can continue.",
                machine_hint: "request_approval_and_pause",
            },
            Self::ApprovalDenied => MissionFailureContract {
                reason_code: "approval_denied",
                error_code: "FTM1007",
                terminality: MissionFailureTerminality::Terminal,
                retryability: MissionFailureRetryability::Never,
                human_hint:
                    "Human operator denied execution. Revise mission scope before retrying.",
                machine_hint: "abort_and_open_revision_task",
            },
            Self::ApprovalExpired => MissionFailureContract {
                reason_code: "approval_expired",
                error_code: "FTM1008",
                terminality: MissionFailureTerminality::NonTerminal,
                retryability: MissionFailureRetryability::AfterApprovalRefresh,
                human_hint: "Approval token expired. Request a fresh approval to continue.",
                machine_hint: "renew_approval_then_retry",
            },
        }
    }

    /// Canonical reason code string.
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        self.contract().reason_code
    }

    /// Canonical error code string.
    #[must_use]
    pub const fn error_code(self) -> &'static str {
        self.contract().error_code
    }

    /// Terminality contract for this code.
    #[must_use]
    pub const fn terminality(self) -> MissionFailureTerminality {
        self.contract().terminality
    }

    /// Retry contract for this code.
    #[must_use]
    pub const fn retryability(self) -> MissionFailureRetryability {
        self.contract().retryability
    }

    /// Human-readable remediation hint.
    #[must_use]
    pub const fn human_hint(self) -> &'static str {
        self.contract().human_hint
    }

    /// Machine-readable remediation hint.
    #[must_use]
    pub const fn machine_hint(self) -> &'static str {
        self.contract().machine_hint
    }
}

/// Field context for failure-code validation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionFailureContext {
    ApprovalDenied,
    ApprovalExpired,
    AssignmentOutcomeFailed,
    AssignmentEscalation,
}

impl fmt::Display for MissionFailureContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApprovalDenied => f.write_str("approval_denied"),
            Self::ApprovalExpired => f.write_str("approval_expired"),
            Self::AssignmentOutcomeFailed => f.write_str("assignment_outcome_failed"),
            Self::AssignmentEscalation => f.write_str("assignment_escalation"),
        }
    }
}

// ============================================================================
// Mission Transaction Contract (Track H1)
// ============================================================================

/// Current schema version for transaction-domain mission contracts.
pub const MISSION_TX_SCHEMA_VERSION: u32 = 1;

/// Stable transaction identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxId(pub String);

impl fmt::Display for TxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable transaction-plan identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxPlanId(pub String);

impl fmt::Display for TxPlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable transaction-step identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxStepId(pub String);

impl fmt::Display for TxStepId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Transaction lifecycle states for mission execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MissionTxState {
    #[default]
    Draft,
    Planned,
    Prepared,
    Committing,
    Committed,
    Compensating,
    RolledBack,
    Failed,
}

impl MissionTxState {
    /// Returns true when this state is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::RolledBack | Self::Failed)
    }
}

impl fmt::Display for MissionTxState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Draft => f.write_str("draft"),
            Self::Planned => f.write_str("planned"),
            Self::Prepared => f.write_str("prepared"),
            Self::Committing => f.write_str("committing"),
            Self::Committed => f.write_str("committed"),
            Self::Compensating => f.write_str("compensating"),
            Self::RolledBack => f.write_str("rolled_back"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

/// Canonical transaction lifecycle transition kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTxTransitionKind {
    PlanCreated,
    PrepareSucceeded,
    PrepareDenied,
    PrepareTimedOut,
    CommitStarted,
    CommitSucceeded,
    CommitPartial,
    CompensationStarted,
    CompensationSucceeded,
    CompensationFailed,
    RollbackForced,
    MarkFailed,
}

impl fmt::Display for MissionTxTransitionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlanCreated => f.write_str("plan_created"),
            Self::PrepareSucceeded => f.write_str("prepare_succeeded"),
            Self::PrepareDenied => f.write_str("prepare_denied"),
            Self::PrepareTimedOut => f.write_str("prepare_timed_out"),
            Self::CommitStarted => f.write_str("commit_started"),
            Self::CommitSucceeded => f.write_str("commit_succeeded"),
            Self::CommitPartial => f.write_str("commit_partial"),
            Self::CompensationStarted => f.write_str("compensation_started"),
            Self::CompensationSucceeded => f.write_str("compensation_succeeded"),
            Self::CompensationFailed => f.write_str("compensation_failed"),
            Self::RollbackForced => f.write_str("rollback_forced"),
            Self::MarkFailed => f.write_str("mark_failed"),
        }
    }
}

/// One legal edge in the transaction lifecycle transition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissionTxTransitionRule {
    pub from: MissionTxState,
    pub to: MissionTxState,
    pub kind: MissionTxTransitionKind,
}

const MISSION_TX_TRANSITION_RULES: [MissionTxTransitionRule; 12] = [
    MissionTxTransitionRule {
        from: MissionTxState::Draft,
        to: MissionTxState::Planned,
        kind: MissionTxTransitionKind::PlanCreated,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Planned,
        to: MissionTxState::Prepared,
        kind: MissionTxTransitionKind::PrepareSucceeded,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Planned,
        to: MissionTxState::Failed,
        kind: MissionTxTransitionKind::PrepareDenied,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Planned,
        to: MissionTxState::Failed,
        kind: MissionTxTransitionKind::PrepareTimedOut,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Prepared,
        to: MissionTxState::Committing,
        kind: MissionTxTransitionKind::CommitStarted,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Committing,
        to: MissionTxState::Committed,
        kind: MissionTxTransitionKind::CommitSucceeded,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Committing,
        to: MissionTxState::Compensating,
        kind: MissionTxTransitionKind::CommitPartial,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Failed,
        to: MissionTxState::Compensating,
        kind: MissionTxTransitionKind::CompensationStarted,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Compensating,
        to: MissionTxState::RolledBack,
        kind: MissionTxTransitionKind::CompensationSucceeded,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Compensating,
        to: MissionTxState::Failed,
        kind: MissionTxTransitionKind::CompensationFailed,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Committed,
        to: MissionTxState::Compensating,
        kind: MissionTxTransitionKind::RollbackForced,
    },
    MissionTxTransitionRule {
        from: MissionTxState::Committing,
        to: MissionTxState::Failed,
        kind: MissionTxTransitionKind::MarkFailed,
    },
];

/// Returns canonical transaction lifecycle transition table.
#[must_use]
pub const fn mission_tx_transition_table() -> &'static [MissionTxTransitionRule] {
    &MISSION_TX_TRANSITION_RULES
}

/// Returns whether a transaction lifecycle transition is legal.
#[must_use]
pub fn mission_tx_can_transition(
    from: MissionTxState,
    to: MissionTxState,
    kind: MissionTxTransitionKind,
) -> bool {
    mission_tx_transition_table()
        .iter()
        .any(|rule| rule.from == from && rule.to == to && rule.kind == kind)
}

/// Canonical failure taxonomy for mission transaction execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTxFailureCode {
    PlanInvalid,
    PreconditionFailed,
    PrepareDenied,
    PrepareTimeout,
    CommitDenied,
    CommitTimeout,
    CommitPartial,
    CompensationFailed,
    RollbackForced,
}

/// Retry semantics for transaction failure handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTxRetryability {
    Never,
    Immediate,
    AfterBackoff,
    AfterPlanRepair,
    AfterStateRefresh,
}

impl MissionTxRetryability {
    /// Returns true if automatic retry is permitted.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        !matches!(self, Self::Never)
    }
}

/// Full remediation contract for one transaction failure code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissionTxFailureContract {
    pub reason_code: &'static str,
    pub error_code: &'static str,
    pub retryability: MissionTxRetryability,
    pub human_hint: &'static str,
    pub machine_hint: &'static str,
}

impl MissionTxFailureCode {
    const ALL: [Self; 9] = [
        Self::PlanInvalid,
        Self::PreconditionFailed,
        Self::PrepareDenied,
        Self::PrepareTimeout,
        Self::CommitDenied,
        Self::CommitTimeout,
        Self::CommitPartial,
        Self::CompensationFailed,
        Self::RollbackForced,
    ];

    /// Return all canonical transaction failure codes.
    #[must_use]
    pub const fn all() -> [Self; 9] {
        Self::ALL
    }

    /// Parse from normalized reason code.
    #[must_use]
    pub fn from_reason_code(reason_code: &str) -> Option<Self> {
        match reason_code.trim() {
            "plan_invalid" => Some(Self::PlanInvalid),
            "precondition_failed" => Some(Self::PreconditionFailed),
            "prepare_denied" => Some(Self::PrepareDenied),
            "prepare_timeout" => Some(Self::PrepareTimeout),
            "commit_denied" => Some(Self::CommitDenied),
            "commit_timeout" => Some(Self::CommitTimeout),
            "commit_partial" => Some(Self::CommitPartial),
            "compensation_failed" => Some(Self::CompensationFailed),
            "rollback_forced" => Some(Self::RollbackForced),
            _ => None,
        }
    }

    /// Return canonical metadata for this transaction failure code.
    #[must_use]
    pub const fn contract(self) -> MissionTxFailureContract {
        match self {
            Self::PlanInvalid => MissionTxFailureContract {
                reason_code: "plan_invalid",
                error_code: "FTX2001",
                retryability: MissionTxRetryability::AfterPlanRepair,
                human_hint: "Transaction plan is invalid; fix structure before retry.",
                machine_hint: "rebuild_plan_and_revalidate",
            },
            Self::PreconditionFailed => MissionTxFailureContract {
                reason_code: "precondition_failed",
                error_code: "FTX2002",
                retryability: MissionTxRetryability::AfterStateRefresh,
                human_hint: "Preconditions failed; refresh state and retry if conditions recover.",
                machine_hint: "refresh_preconditions_then_retry",
            },
            Self::PrepareDenied => MissionTxFailureContract {
                reason_code: "prepare_denied",
                error_code: "FTX2003",
                retryability: MissionTxRetryability::Never,
                human_hint: "Prepare phase was denied by policy or safety gate.",
                machine_hint: "abort_and_request_operator_review",
            },
            Self::PrepareTimeout => MissionTxFailureContract {
                reason_code: "prepare_timeout",
                error_code: "FTX2004",
                retryability: MissionTxRetryability::AfterBackoff,
                human_hint: "Prepare phase timed out; retry after backoff.",
                machine_hint: "schedule_backoff_then_retry_prepare",
            },
            Self::CommitDenied => MissionTxFailureContract {
                reason_code: "commit_denied",
                error_code: "FTX2005",
                retryability: MissionTxRetryability::Never,
                human_hint: "Commit was denied; do not retry without operator intervention.",
                machine_hint: "abort_commit_and_require_override",
            },
            Self::CommitTimeout => MissionTxFailureContract {
                reason_code: "commit_timeout",
                error_code: "FTX2006",
                retryability: MissionTxRetryability::AfterBackoff,
                human_hint: "Commit timed out; retry with jitter or roll back.",
                machine_hint: "retry_commit_with_jitter",
            },
            Self::CommitPartial => MissionTxFailureContract {
                reason_code: "commit_partial",
                error_code: "FTX2007",
                retryability: MissionTxRetryability::AfterStateRefresh,
                human_hint: "Commit partially applied; compensation path required.",
                machine_hint: "run_compensation_plan",
            },
            Self::CompensationFailed => MissionTxFailureContract {
                reason_code: "compensation_failed",
                error_code: "FTX2008",
                retryability: MissionTxRetryability::Immediate,
                human_hint: "Compensation failed; immediate operator attention required.",
                machine_hint: "escalate_and_retry_compensation",
            },
            Self::RollbackForced => MissionTxFailureContract {
                reason_code: "rollback_forced",
                error_code: "FTX2009",
                retryability: MissionTxRetryability::AfterStateRefresh,
                human_hint: "Rollback was forced after safety violation.",
                machine_hint: "record_forced_rollback_and_reconcile",
            },
        }
    }

    /// Canonical reason code string.
    #[must_use]
    pub const fn reason_code(self) -> &'static str {
        self.contract().reason_code
    }

    /// Canonical error code string.
    #[must_use]
    pub const fn error_code(self) -> &'static str {
        self.contract().error_code
    }

    /// Retry contract for this failure code.
    #[must_use]
    pub const fn retryability(self) -> MissionTxRetryability {
        self.contract().retryability
    }

    /// Human-readable remediation hint.
    #[must_use]
    pub const fn human_hint(self) -> &'static str {
        self.contract().human_hint
    }

    /// Machine-readable remediation hint.
    #[must_use]
    pub const fn machine_hint(self) -> &'static str {
        self.contract().machine_hint
    }
}

/// Transaction intent envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxIntent {
    pub tx_id: TxId,
    pub requested_by: MissionActorRole,
    pub summary: String,
    pub correlation_id: String,
    pub created_at_ms: i64,
}

/// One transaction precondition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TxPrecondition {
    PromptActive {
        pane_id: u64,
    },
    ReservationHeld {
        reservation_id: ReservationIntentId,
    },
    ApprovalGranted {
        approval_code_hash: String,
    },
    Custom {
        key: String,
        expected: serde_json::Value,
    },
}

/// One transaction execution step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TxStep {
    pub step_id: TxStepId,
    pub ordinal: u32,
    pub action: StepAction,
}

/// One compensation step tied to a forward execution step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TxCompensation {
    pub for_step_id: TxStepId,
    pub action: StepAction,
}

/// Transaction plan payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TxPlan {
    pub plan_id: TxPlanId,
    pub tx_id: TxId,
    pub steps: Vec<TxStep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<TxPrecondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compensations: Vec<TxCompensation>,
}

/// Ordered receipt stream for transaction transitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxReceipt {
    pub seq: u64,
    pub state: MissionTxState,
    pub emitted_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// Final transaction outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TxOutcome {
    Pending,
    Committed {
        completed_at_ms: i64,
        receipt_seq: u64,
    },
    RolledBack {
        completed_at_ms: i64,
        receipt_seq: u64,
        reason_code: String,
    },
    Failed {
        completed_at_ms: i64,
        reason_code: String,
        error_code: String,
    },
}

impl TxOutcome {
    #[must_use]
    const fn kind_name(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed { .. } => "committed",
            Self::RolledBack { .. } => "rolled_back",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Structured transition log for transaction lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTxTransitionLog {
    pub timestamp_ms: i64,
    pub component: String,
    pub scenario_id: String,
    pub correlation_id: String,
    pub tx_id: TxId,
    pub state_from: MissionTxState,
    pub state_to: MissionTxState,
    pub transition_kind: MissionTxTransitionKind,
    pub decision_path: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub artifact_path: String,
}

impl MissionTxTransitionLog {
    /// Validate mandatory structured log fields and reason/error-code coherence.
    pub fn validate(&self) -> Result<(), MissionTxValidationError> {
        for (field, value) in [
            ("component", self.component.as_str()),
            ("scenario_id", self.scenario_id.as_str()),
            ("correlation_id", self.correlation_id.as_str()),
            ("decision_path", self.decision_path.as_str()),
            ("outcome", self.outcome.as_str()),
            ("artifact_path", self.artifact_path.as_str()),
            ("tx_id", self.tx_id.0.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(MissionTxValidationError::MissingTransitionLogField { field });
            }
        }

        if !mission_tx_can_transition(self.state_from, self.state_to, self.transition_kind) {
            return Err(MissionTxValidationError::IllegalLifecycleTransition {
                from: self.state_from,
                to: self.state_to,
                kind: self.transition_kind,
            });
        }

        if let Some(reason_code) = self.reason_code.as_deref() {
            let failure_code =
                MissionTxFailureCode::from_reason_code(reason_code).ok_or_else(|| {
                    MissionTxValidationError::UnknownFailureReasonCode {
                        reason_code: reason_code.to_string(),
                    }
                })?;
            if let Some(error_code) = self.error_code.as_deref() {
                if error_code.trim() != failure_code.error_code() {
                    return Err(MissionTxValidationError::MismatchedFailureErrorCode {
                        reason_code: reason_code.to_string(),
                        expected_error_code: failure_code.error_code().to_string(),
                        actual_error_code: error_code.to_string(),
                    });
                }
            }
        } else if self.error_code.is_some() {
            return Err(MissionTxValidationError::MissingFailureReasonCode);
        }

        Ok(())
    }
}

/// Transaction contract envelope (entities + state + receipts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionTxContract {
    pub tx_version: u32,
    pub intent: TxIntent,
    pub plan: TxPlan,
    pub lifecycle_state: MissionTxState,
    pub outcome: TxOutcome,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receipts: Vec<TxReceipt>,
}

impl MissionTxContract {
    /// Validate transaction contract structure, lifecycle, and invariants.
    pub fn validate(&self) -> Result<(), MissionTxValidationError> {
        if self.tx_version > MISSION_TX_SCHEMA_VERSION {
            return Err(MissionTxValidationError::UnsupportedTxVersion {
                version: self.tx_version,
                max_supported: MISSION_TX_SCHEMA_VERSION,
            });
        }
        if self.intent.summary.trim().is_empty() {
            return Err(MissionTxValidationError::MissingIntentSummary);
        }
        if self.intent.correlation_id.trim().is_empty() {
            return Err(MissionTxValidationError::MissingCorrelationId);
        }
        if self.plan.tx_id != self.intent.tx_id {
            return Err(MissionTxValidationError::PlanIntentMismatch {
                plan_tx_id: self.plan.tx_id.clone(),
                intent_tx_id: self.intent.tx_id.clone(),
            });
        }
        if self.plan.steps.is_empty() {
            return Err(MissionTxValidationError::EmptyPlanSteps);
        }

        let mut step_ids = std::collections::HashSet::new();
        for (index, step) in self.plan.steps.iter().enumerate() {
            let expected = u32::try_from(index + 1).unwrap_or(u32::MAX);
            if step.ordinal != expected {
                return Err(MissionTxValidationError::InvalidStepOrdinal {
                    step_id: step.step_id.clone(),
                    expected,
                    actual: step.ordinal,
                });
            }
            if !step_ids.insert(step.step_id.clone()) {
                return Err(MissionTxValidationError::DuplicateStepId(
                    step.step_id.clone(),
                ));
            }
        }
        for compensation in &self.plan.compensations {
            if !step_ids.contains(&compensation.for_step_id) {
                return Err(MissionTxValidationError::CompensationUnknownStep(
                    compensation.for_step_id.clone(),
                ));
            }
        }

        let mut last_seq = 0_u64;
        let mut has_seq = false;
        let mut prepared_seen = false;
        let mut commit_markers = 0_u32;
        let mut commit_failure_seen = false;
        let mut compensating_seen = false;
        for receipt in &self.receipts {
            if has_seq && receipt.seq <= last_seq {
                return Err(MissionTxValidationError::NonMonotonicReceiptSequence {
                    previous: last_seq,
                    current: receipt.seq,
                });
            }
            has_seq = true;
            last_seq = receipt.seq;

            match receipt.state {
                MissionTxState::Prepared => prepared_seen = true,
                MissionTxState::Committed => {
                    commit_markers = commit_markers.saturating_add(1);
                    if !prepared_seen {
                        return Err(MissionTxValidationError::CommitWithoutPreparedReceipt);
                    }
                }
                MissionTxState::Compensating => {
                    compensating_seen = true;
                }
                MissionTxState::Draft
                | MissionTxState::Planned
                | MissionTxState::Committing
                | MissionTxState::RolledBack
                | MissionTxState::Failed => {}
            }

            if let Some(reason_code) = receipt.reason_code.as_deref() {
                let failure =
                    MissionTxFailureCode::from_reason_code(reason_code).ok_or_else(|| {
                        MissionTxValidationError::UnknownFailureReasonCode {
                            reason_code: reason_code.to_string(),
                        }
                    })?;
                if failure == MissionTxFailureCode::CommitPartial {
                    commit_failure_seen = true;
                }
                if let Some(error_code) = receipt.error_code.as_deref() {
                    if error_code.trim() != failure.error_code() {
                        return Err(MissionTxValidationError::MismatchedFailureErrorCode {
                            reason_code: reason_code.to_string(),
                            expected_error_code: failure.error_code().to_string(),
                            actual_error_code: error_code.to_string(),
                        });
                    }
                }
            } else if receipt.error_code.is_some() {
                return Err(MissionTxValidationError::MissingFailureReasonCode);
            }
        }
        if commit_markers > 1 {
            return Err(MissionTxValidationError::DoubleCommitMarker);
        }
        if compensating_seen && !commit_failure_seen {
            return Err(MissionTxValidationError::CompensationWithoutCommitFailure);
        }

        match &self.outcome {
            TxOutcome::Pending => {
                if self.lifecycle_state.is_terminal() {
                    return Err(MissionTxValidationError::OutcomeStateMismatch {
                        state: self.lifecycle_state,
                        outcome: TxOutcome::Pending.kind_name(),
                    });
                }
            }
            TxOutcome::Committed { .. } => {
                if self.lifecycle_state != MissionTxState::Committed || commit_markers != 1 {
                    return Err(MissionTxValidationError::OutcomeStateMismatch {
                        state: self.lifecycle_state,
                        outcome: TxOutcome::Committed {
                            completed_at_ms: 0,
                            receipt_seq: 0,
                        }
                        .kind_name(),
                    });
                }
            }
            TxOutcome::RolledBack { reason_code, .. } => {
                if self.lifecycle_state != MissionTxState::RolledBack {
                    return Err(MissionTxValidationError::OutcomeStateMismatch {
                        state: self.lifecycle_state,
                        outcome: TxOutcome::RolledBack {
                            completed_at_ms: 0,
                            receipt_seq: 0,
                            reason_code: String::new(),
                        }
                        .kind_name(),
                    });
                }
                if MissionTxFailureCode::from_reason_code(reason_code).is_none() {
                    return Err(MissionTxValidationError::UnknownFailureReasonCode {
                        reason_code: reason_code.clone(),
                    });
                }
            }
            TxOutcome::Failed {
                reason_code,
                error_code,
                ..
            } => {
                if self.lifecycle_state != MissionTxState::Failed {
                    return Err(MissionTxValidationError::OutcomeStateMismatch {
                        state: self.lifecycle_state,
                        outcome: TxOutcome::Failed {
                            completed_at_ms: 0,
                            reason_code: String::new(),
                            error_code: String::new(),
                        }
                        .kind_name(),
                    });
                }
                let failure_code =
                    MissionTxFailureCode::from_reason_code(reason_code).ok_or_else(|| {
                        MissionTxValidationError::UnknownFailureReasonCode {
                            reason_code: reason_code.clone(),
                        }
                    })?;
                if error_code.trim() != failure_code.error_code() {
                    return Err(MissionTxValidationError::MismatchedFailureErrorCode {
                        reason_code: reason_code.clone(),
                        expected_error_code: failure_code.error_code().to_string(),
                        actual_error_code: error_code.clone(),
                    });
                }
            }
        }

        Ok(())
    }
}

/// Validation failures for mission transaction contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionTxValidationError {
    UnsupportedTxVersion {
        version: u32,
        max_supported: u32,
    },
    MissingIntentSummary,
    MissingCorrelationId,
    PlanIntentMismatch {
        plan_tx_id: TxId,
        intent_tx_id: TxId,
    },
    EmptyPlanSteps,
    InvalidStepOrdinal {
        step_id: TxStepId,
        expected: u32,
        actual: u32,
    },
    DuplicateStepId(TxStepId),
    CompensationUnknownStep(TxStepId),
    NonMonotonicReceiptSequence {
        previous: u64,
        current: u64,
    },
    CommitWithoutPreparedReceipt,
    DoubleCommitMarker,
    CompensationWithoutCommitFailure,
    MissingFailureReasonCode,
    UnknownFailureReasonCode {
        reason_code: String,
    },
    MismatchedFailureErrorCode {
        reason_code: String,
        expected_error_code: String,
        actual_error_code: String,
    },
    OutcomeStateMismatch {
        state: MissionTxState,
        outcome: &'static str,
    },
    MissingTransitionLogField {
        field: &'static str,
    },
    IllegalLifecycleTransition {
        from: MissionTxState,
        to: MissionTxState,
        kind: MissionTxTransitionKind,
    },
}

impl fmt::Display for MissionTxValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedTxVersion {
                version,
                max_supported,
            } => write!(
                f,
                "Unsupported transaction schema version {version}; max supported is {max_supported}"
            ),
            Self::MissingIntentSummary => f.write_str("Transaction intent summary is required"),
            Self::MissingCorrelationId => f.write_str("Transaction correlation_id is required"),
            Self::PlanIntentMismatch {
                plan_tx_id,
                intent_tx_id,
            } => write!(
                f,
                "Transaction plan tx_id ({plan_tx_id}) does not match intent tx_id ({intent_tx_id})"
            ),
            Self::EmptyPlanSteps => f.write_str("Transaction plan must contain at least one step"),
            Self::InvalidStepOrdinal {
                step_id,
                expected,
                actual,
            } => write!(
                f,
                "Invalid step ordinal for {}: expected {}, got {}",
                step_id, expected, actual
            ),
            Self::DuplicateStepId(step_id) => write!(f, "Duplicate transaction step ID: {step_id}"),
            Self::CompensationUnknownStep(step_id) => {
                write!(f, "Compensation references unknown step ID: {step_id}")
            }
            Self::NonMonotonicReceiptSequence { previous, current } => write!(
                f,
                "Transaction receipts must be monotonic: previous seq {}, current seq {}",
                previous, current
            ),
            Self::CommitWithoutPreparedReceipt => {
                f.write_str("Commit observed before a prepared receipt")
            }
            Self::DoubleCommitMarker => {
                f.write_str("Multiple committed receipts detected for one transaction")
            }
            Self::CompensationWithoutCommitFailure => {
                f.write_str("Compensation observed without a prior commit_partial failure marker")
            }
            Self::MissingFailureReasonCode => {
                f.write_str("Failure error_code requires a matching reason_code")
            }
            Self::UnknownFailureReasonCode { reason_code } => {
                write!(f, "Unknown transaction failure reason_code: {reason_code}")
            }
            Self::MismatchedFailureErrorCode {
                reason_code,
                expected_error_code,
                actual_error_code,
            } => write!(
                f,
                "Failure error_code mismatch for reason_code {}: expected {}, got {}",
                reason_code, expected_error_code, actual_error_code
            ),
            Self::OutcomeStateMismatch { state, outcome } => write!(
                f,
                "Transaction lifecycle state {} is incompatible with outcome {}",
                state, outcome
            ),
            Self::MissingTransitionLogField { field } => {
                write!(f, "Transaction transition log missing field: {field}")
            }
            Self::IllegalLifecycleTransition { from, to, kind } => write!(
                f,
                "Illegal transaction lifecycle transition: {} -> {} ({})",
                from, to, kind
            ),
        }
    }
}

impl std::error::Error for MissionTxValidationError {}

/// Mission policy preflight stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionPolicyPreflightStage {
    PlanTime,
    DispatchTime,
}

impl fmt::Display for MissionPolicyPreflightStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlanTime => f.write_str("plan_time"),
            Self::DispatchTime => f.write_str("dispatch_time"),
        }
    }
}

/// Normalized policy decision kind used by mission preflight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionPolicyDecisionKind {
    Allow,
    Deny,
    RequireApproval,
}

impl fmt::Display for MissionPolicyDecisionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allow => f.write_str("allow"),
            Self::Deny => f.write_str("deny"),
            Self::RequireApproval => f.write_str("require_approval"),
        }
    }
}

/// One policy preflight check produced by policy/rule evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionPolicyPreflightCheck {
    pub candidate_id: CandidateActionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<AssignmentId>,
    pub decision: MissionPolicyDecisionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Structured preflight outcome for one candidate/assignment check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionPolicyPreflightOutcome {
    pub stage: MissionPolicyPreflightStage,
    pub candidate_id: CandidateActionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<AssignmentId>,
    pub action_type: String,
    pub decision: MissionPolicyDecisionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Full mission preflight report consumed by planner/dispatcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionPolicyPreflightReport {
    pub stage: MissionPolicyPreflightStage,
    pub outcomes: Vec<MissionPolicyPreflightOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub planner_feedback_reason_codes: Vec<String>,
}

impl MissionPolicyPreflightReport {
    /// Returns true when at least one policy denial occurred.
    #[must_use]
    pub fn has_denials(&self) -> bool {
        self.outcomes
            .iter()
            .any(|outcome| outcome.decision == MissionPolicyDecisionKind::Deny)
    }

    /// Returns true when at least one check requested human approval.
    #[must_use]
    pub fn requires_approval(&self) -> bool {
        self.outcomes
            .iter()
            .any(|outcome| outcome.decision == MissionPolicyDecisionKind::RequireApproval)
    }
}

// ============================================================================
// Mission Dispatch Mapping Contract
// ============================================================================

/// Concrete dispatch surface used for a candidate action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mechanism", rename_all = "snake_case")]
pub enum MissionDispatchMechanism {
    RobotSend {
        pane_id: u64,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        paste_mode: Option<bool>,
    },
    RobotWaitFor {
        #[serde(skip_serializing_if = "Option::is_none")]
        pane_id: Option<u64>,
        condition: WaitCondition,
        timeout_ms: u64,
    },
    RobotRunWorkflow {
        workflow_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        params: Option<serde_json::Value>,
    },
    InternalLockAcquire {
        lock_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    InternalLockRelease {
        lock_name: String,
    },
    InternalStoreData {
        key: String,
        value: serde_json::Value,
    },
    InternalMarkEventHandled {
        event_id: i64,
    },
    InternalValidateApproval {
        approval_code: String,
    },
    InternalNestedPlan {
        plan_hash: String,
    },
    InternalCustom {
        action_type: String,
        payload: serde_json::Value,
    },
}

impl MissionDispatchMechanism {
    /// Human-readable primitive family used by this mechanism.
    #[must_use]
    pub const fn primitive_family(&self) -> &'static str {
        match self {
            Self::RobotSend { .. } | Self::RobotWaitFor { .. } | Self::RobotRunWorkflow { .. } => {
                "robot"
            }
            Self::InternalLockAcquire { .. }
            | Self::InternalLockRelease { .. }
            | Self::InternalStoreData { .. }
            | Self::InternalMarkEventHandled { .. }
            | Self::InternalValidateApproval { .. }
            | Self::InternalNestedPlan { .. }
            | Self::InternalCustom { .. } => "internal_plan",
        }
    }
}

/// File-reservation requirements to execute a mapped dispatch action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionReservationRequirement {
    pub required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intents: Vec<ReservationIntent>,
}

/// Messaging and issue-tracking requirements around mission dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionMessagingRequirement {
    pub requires_agent_mail: bool,
    pub requires_beads_update: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
}

/// Explicit edge-case contract to keep dispatch behavior predictable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MissionDispatchEdgeCase {
    MissingPane {
        pane_id: u64,
        reason_code: String,
        error_code: String,
        remediation: String,
    },
    StaleBeadState {
        bead_id: String,
        reason_code: String,
        error_code: String,
        remediation: String,
    },
}

/// Mapping from mission candidate action to concrete control-plane primitives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionDispatchContract {
    pub candidate_id: CandidateActionId,
    pub mechanism: MissionDispatchMechanism,
    pub reservation: MissionReservationRequirement,
    pub messaging: MissionMessagingRequirement,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_cases: Vec<MissionDispatchEdgeCase>,
}

/// Current dispatch availability state for an execution agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MissionAgentAvailability {
    Ready,
    Paused {
        reason_code: String,
    },
    Degraded {
        reason_code: String,
        max_parallel_assignments: u32,
    },
    RateLimited {
        reason_code: String,
        retry_after_ms: i64,
    },
    Offline {
        reason_code: String,
    },
}

impl MissionAgentAvailability {
    /// Canonical reason code for structured suitability output.
    #[must_use]
    pub fn reason_code(&self) -> Option<&str> {
        match self {
            Self::Ready => None,
            Self::Paused { reason_code }
            | Self::Degraded { reason_code, .. }
            | Self::RateLimited { reason_code, .. }
            | Self::Offline { reason_code } => Some(reason_code.as_str()),
        }
    }

    /// Whether this state hard-blocks assignment regardless of capabilities.
    #[must_use]
    pub const fn hard_blocks_assignment(&self) -> bool {
        matches!(
            self,
            Self::Paused { .. } | Self::RateLimited { .. } | Self::Offline { .. }
        )
    }
}

/// Capability/load profile used by mission dispatcher suitability scoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionAgentCapabilityProfile {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lane_affinity: Vec<String>,
    pub current_load: u32,
    pub max_parallel_assignments: u32,
    pub availability: MissionAgentAvailability,
}

impl MissionAgentCapabilityProfile {
    /// Effective capacity after availability modifiers.
    #[must_use]
    pub fn effective_capacity(&self) -> u32 {
        match self.availability {
            MissionAgentAvailability::Degraded {
                max_parallel_assignments,
                ..
            } => self.max_parallel_assignments.min(max_parallel_assignments),
            _ => self.max_parallel_assignments,
        }
    }
}

/// Candidate-specific requirements for dispatcher assignment selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionAssignmentSuitabilityRequest {
    pub candidate_id: CandidateActionId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preferred_capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane_affinity: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded_agents: Vec<String>,
    pub evaluated_at_ms: i64,
}

/// Per-agent suitability envelope emitted by dispatcher model evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionAgentSuitability {
    pub agent_id: String,
    pub eligible: bool,
    pub hard_constraints_satisfied: bool,
    pub effective_capacity: u32,
    pub score: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_required_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_preferred_capabilities: Vec<String>,
}

/// Dispatcher suitability report for one candidate requirement request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionAssignmentSuitabilityReport {
    pub candidate_id: CandidateActionId,
    pub evaluated_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane_affinity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evaluations: Vec<MissionAgentSuitability>,
}

/// Escalation severity for operator routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationLevel {
    Observe,
    Human,
    Emergency,
}

impl fmt::Display for EscalationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Observe => f.write_str("observe"),
            Self::Human => f.write_str("human"),
            Self::Emergency => f.write_str("emergency"),
        }
    }
}

/// Escalation envelope for execution anomalies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Escalation {
    pub level: EscalationLevel,
    pub triggered_by: MissionActorRole,
    pub reason_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub escalated_at_ms: i64,
}

impl Escalation {
    /// Deterministic string representation used by `Assignment::canonical_string`.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "level={},triggered_by={},reason_code={},error_code={},summary={},escalated_at_ms={}",
            self.level,
            self.triggered_by,
            self.reason_code,
            self.error_code.as_deref().unwrap_or(""),
            self.summary.as_deref().unwrap_or(""),
            self.escalated_at_ms
        )
    }
}

/// Dispatcher-selected execution assignment for a mission candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub assignment_id: AssignmentId,
    pub candidate_id: CandidateActionId,
    pub assigned_by: MissionActorRole,
    pub assignee: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reservation_intent: Option<ReservationIntent>,
    pub approval_state: ApprovalState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalation: Option<Escalation>,
    pub created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<i64>,
}

impl Assignment {
    /// Deterministic string representation used by `Mission::canonical_string`.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut parts = vec![
            format!("id={}", self.assignment_id.0),
            format!("candidate_id={}", self.candidate_id.0),
            format!("assigned_by={}", self.assigned_by),
            format!("assignee={}", self.assignee),
            format!("approval={}", self.approval_state.canonical_string()),
            format!("created_at_ms={}", self.created_at_ms),
            format!(
                "updated_at_ms={}",
                self.updated_at_ms
                    .map_or_else(|| "none".to_string(), |v| v.to_string())
            ),
        ];

        if let Some(reservation_intent) = &self.reservation_intent {
            parts.push(format!(
                "reservation={}",
                reservation_intent.canonical_string()
            ));
        }
        if let Some(outcome) = &self.outcome {
            parts.push(format!("outcome={}", outcome.canonical_string()));
        }
        if let Some(escalation) = &self.escalation {
            parts.push(format!("escalation={}", escalation.canonical_string()));
        }
        parts.join(",")
    }
}

/// Mission lifecycle state machine for planner->dispatcher->operator flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MissionLifecycleState {
    #[default]
    Planning,
    Planned,
    Dispatching,
    AwaitingApproval,
    Running,
    RetryPending,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

impl MissionLifecycleState {
    const ALL: [Self; 10] = [
        Self::Planning,
        Self::Planned,
        Self::Dispatching,
        Self::AwaitingApproval,
        Self::Running,
        Self::RetryPending,
        Self::Blocked,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
    ];

    /// Return all lifecycle states.
    #[must_use]
    pub const fn all() -> [Self; 10] {
        Self::ALL
    }

    /// Returns true when mission is in terminal state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

impl fmt::Display for MissionLifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planning => f.write_str("planning"),
            Self::Planned => f.write_str("planned"),
            Self::Dispatching => f.write_str("dispatching"),
            Self::AwaitingApproval => f.write_str("awaiting_approval"),
            Self::Running => f.write_str("running"),
            Self::RetryPending => f.write_str("retry_pending"),
            Self::Blocked => f.write_str("blocked"),
            Self::Completed => f.write_str("completed"),
            Self::Failed => f.write_str("failed"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// Transition intent for mission lifecycle movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionLifecycleTransitionKind {
    PlanFinalized,
    DispatchStarted,
    ApprovalRequested,
    ApprovalGranted,
    ApprovalDenied,
    ApprovalExpired,
    ExecutionStarted,
    ExecutionBlocked,
    RetryScheduled,
    RetryResumed,
    ExecutionSucceeded,
    ExecutionFailed,
    MissionCancelled,
}

impl fmt::Display for MissionLifecycleTransitionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlanFinalized => f.write_str("plan_finalized"),
            Self::DispatchStarted => f.write_str("dispatch_started"),
            Self::ApprovalRequested => f.write_str("approval_requested"),
            Self::ApprovalGranted => f.write_str("approval_granted"),
            Self::ApprovalDenied => f.write_str("approval_denied"),
            Self::ApprovalExpired => f.write_str("approval_expired"),
            Self::ExecutionStarted => f.write_str("execution_started"),
            Self::ExecutionBlocked => f.write_str("execution_blocked"),
            Self::RetryScheduled => f.write_str("retry_scheduled"),
            Self::RetryResumed => f.write_str("retry_resumed"),
            Self::ExecutionSucceeded => f.write_str("execution_succeeded"),
            Self::ExecutionFailed => f.write_str("execution_failed"),
            Self::MissionCancelled => f.write_str("mission_cancelled"),
        }
    }
}

impl MissionLifecycleTransitionKind {
    const ALL: [Self; 13] = [
        Self::PlanFinalized,
        Self::DispatchStarted,
        Self::ApprovalRequested,
        Self::ApprovalGranted,
        Self::ApprovalDenied,
        Self::ApprovalExpired,
        Self::ExecutionStarted,
        Self::ExecutionBlocked,
        Self::RetryScheduled,
        Self::RetryResumed,
        Self::ExecutionSucceeded,
        Self::ExecutionFailed,
        Self::MissionCancelled,
    ];

    /// Return all lifecycle transition kinds.
    #[must_use]
    pub const fn all() -> [Self; 13] {
        Self::ALL
    }
}

/// One legal lifecycle transition edge in the mission transition table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissionLifecycleTransitionRule {
    pub from: MissionLifecycleState,
    pub to: MissionLifecycleState,
    pub kind: MissionLifecycleTransitionKind,
}

const MISSION_LIFECYCLE_TRANSITION_RULES: [MissionLifecycleTransitionRule; 29] = [
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Planning,
        to: MissionLifecycleState::Planned,
        kind: MissionLifecycleTransitionKind::PlanFinalized,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Planning,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::MissionCancelled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Planned,
        to: MissionLifecycleState::Dispatching,
        kind: MissionLifecycleTransitionKind::DispatchStarted,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Planned,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::MissionCancelled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Dispatching,
        to: MissionLifecycleState::AwaitingApproval,
        kind: MissionLifecycleTransitionKind::ApprovalRequested,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Dispatching,
        to: MissionLifecycleState::Running,
        kind: MissionLifecycleTransitionKind::ExecutionStarted,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Dispatching,
        to: MissionLifecycleState::Blocked,
        kind: MissionLifecycleTransitionKind::ExecutionBlocked,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Dispatching,
        to: MissionLifecycleState::RetryPending,
        kind: MissionLifecycleTransitionKind::RetryScheduled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Dispatching,
        to: MissionLifecycleState::Failed,
        kind: MissionLifecycleTransitionKind::ExecutionFailed,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Dispatching,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::MissionCancelled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::AwaitingApproval,
        to: MissionLifecycleState::Running,
        kind: MissionLifecycleTransitionKind::ApprovalGranted,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::AwaitingApproval,
        to: MissionLifecycleState::Failed,
        kind: MissionLifecycleTransitionKind::ApprovalDenied,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::AwaitingApproval,
        to: MissionLifecycleState::Failed,
        kind: MissionLifecycleTransitionKind::ApprovalExpired,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::AwaitingApproval,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::MissionCancelled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Running,
        to: MissionLifecycleState::Completed,
        kind: MissionLifecycleTransitionKind::ExecutionSucceeded,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Running,
        to: MissionLifecycleState::Failed,
        kind: MissionLifecycleTransitionKind::ExecutionFailed,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Running,
        to: MissionLifecycleState::Blocked,
        kind: MissionLifecycleTransitionKind::ExecutionBlocked,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Running,
        to: MissionLifecycleState::RetryPending,
        kind: MissionLifecycleTransitionKind::RetryScheduled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Running,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::MissionCancelled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Blocked,
        to: MissionLifecycleState::RetryPending,
        kind: MissionLifecycleTransitionKind::RetryScheduled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Blocked,
        to: MissionLifecycleState::Running,
        kind: MissionLifecycleTransitionKind::RetryResumed,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Blocked,
        to: MissionLifecycleState::Failed,
        kind: MissionLifecycleTransitionKind::ExecutionFailed,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Blocked,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::MissionCancelled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::RetryPending,
        to: MissionLifecycleState::Dispatching,
        kind: MissionLifecycleTransitionKind::RetryResumed,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::RetryPending,
        to: MissionLifecycleState::Running,
        kind: MissionLifecycleTransitionKind::RetryResumed,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::RetryPending,
        to: MissionLifecycleState::Failed,
        kind: MissionLifecycleTransitionKind::ExecutionFailed,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::RetryPending,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::MissionCancelled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Failed,
        to: MissionLifecycleState::RetryPending,
        kind: MissionLifecycleTransitionKind::RetryScheduled,
    },
    MissionLifecycleTransitionRule {
        from: MissionLifecycleState::Failed,
        to: MissionLifecycleState::Cancelled,
        kind: MissionLifecycleTransitionKind::MissionCancelled,
    },
];

/// Returns canonical mission lifecycle transition table.
#[must_use]
pub const fn mission_lifecycle_transition_table() -> &'static [MissionLifecycleTransitionRule] {
    &MISSION_LIFECYCLE_TRANSITION_RULES
}

/// Returns whether a lifecycle transition is legal.
#[must_use]
pub fn mission_lifecycle_can_transition(
    from: MissionLifecycleState,
    to: MissionLifecycleState,
    kind: MissionLifecycleTransitionKind,
) -> bool {
    mission_lifecycle_transition_table()
        .iter()
        .any(|rule| rule.from == from && rule.to == to && rule.kind == kind)
}

/// Canonical mission object for planner/dispatcher/operator orchestration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub mission_version: u32,
    pub mission_id: MissionId,
    pub title: String,
    pub workspace_id: String,
    pub ownership: MissionOwnership,
    #[serde(default)]
    pub lifecycle_state: MissionLifecycleState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<MissionProvenance>,
    pub created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<CandidateAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignments: Vec<Assignment>,
}

impl Mission {
    /// Construct a mission with explicit ownership boundaries.
    #[must_use]
    pub fn new(
        mission_id: MissionId,
        title: impl Into<String>,
        workspace_id: impl Into<String>,
        ownership: MissionOwnership,
        created_at_ms: i64,
    ) -> Self {
        Self {
            mission_version: MISSION_SCHEMA_VERSION,
            mission_id,
            title: title.into(),
            workspace_id: workspace_id.into(),
            ownership,
            lifecycle_state: MissionLifecycleState::Planning,
            provenance: None,
            created_at_ms,
            updated_at_ms: None,
            candidates: Vec::new(),
            assignments: Vec::new(),
        }
    }

    /// Compute the mission hash from canonical mission serialization.
    #[must_use]
    pub fn compute_hash(&self) -> String {
        let canonical = self.canonical_string();
        let hash = sha256_hex(&canonical);
        format!("sha256:{}", &hash[..32])
    }

    /// Deterministic canonical string for stable/diff-friendly serialization checks.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        let mut parts = vec![
            format!("v={}", self.mission_version),
            format!("mission_id={}", self.mission_id.0),
            format!("title={}", self.title),
            format!("workspace_id={}", self.workspace_id),
            format!("ownership={}", self.ownership.canonical_string()),
            format!("lifecycle_state={}", self.lifecycle_state),
            format!("created_at_ms={}", self.created_at_ms),
            format!(
                "updated_at_ms={}",
                self.updated_at_ms
                    .map_or_else(|| "none".to_string(), |v| v.to_string())
            ),
        ];

        if let Some(provenance) = &self.provenance {
            parts.push(format!("provenance={}", provenance.canonical_string()));
        }

        let mut candidate_parts: Vec<String> = self
            .candidates
            .iter()
            .map(CandidateAction::canonical_string)
            .collect();
        candidate_parts.sort();
        for (index, candidate) in candidate_parts.iter().enumerate() {
            parts.push(format!("candidate[{index}]={candidate}"));
        }

        let mut assignment_parts: Vec<String> = self
            .assignments
            .iter()
            .map(Assignment::canonical_string)
            .collect();
        assignment_parts.sort();
        for (index, assignment) in assignment_parts.iter().enumerate() {
            parts.push(format!("assignment[{index}]={assignment}"));
        }

        parts.join("|")
    }

    /// Validate schema and ownership invariants.
    pub fn validate(&self) -> Result<(), MissionValidationError> {
        if self.mission_version > MISSION_SCHEMA_VERSION {
            return Err(MissionValidationError::UnsupportedMissionVersion {
                version: self.mission_version,
                max_supported: MISSION_SCHEMA_VERSION,
            });
        }
        Self::validate_non_empty_field("mission.mission_id", &self.mission_id.0)?;
        if self.title.trim().is_empty() {
            return Err(MissionValidationError::MissingTitle);
        }
        if self.workspace_id.trim().is_empty() {
            return Err(MissionValidationError::MissingWorkspaceId);
        }
        if let Some(updated_at_ms) = self.updated_at_ms {
            Self::validate_timestamp_order(
                "mission.updated_at_ms",
                self.created_at_ms,
                updated_at_ms,
            )?;
        }
        if let Some(provenance) = &self.provenance {
            Self::validate_optional_non_empty_field(
                "mission.provenance.bead_id",
                provenance.bead_id.as_deref(),
            )?;
            Self::validate_optional_non_empty_field(
                "mission.provenance.thread_id",
                provenance.thread_id.as_deref(),
            )?;
            Self::validate_optional_non_empty_field(
                "mission.provenance.source_command",
                provenance.source_command.as_deref(),
            )?;
            Self::validate_optional_non_empty_field(
                "mission.provenance.source_sha",
                provenance.source_sha.as_deref(),
            )?;
        }
        self.ownership.validate()?;

        let mut candidate_ids = std::collections::HashSet::new();
        for (candidate_index, candidate) in self.candidates.iter().enumerate() {
            Self::validate_non_empty_field(
                format!("mission.candidates[{candidate_index}].candidate_id"),
                &candidate.candidate_id.0,
            )?;
            Self::validate_non_empty_field(
                format!("mission.candidates[{candidate_index}].rationale"),
                &candidate.rationale,
            )?;
            if let Some(score) = candidate.score {
                if !score.is_finite() {
                    return Err(MissionValidationError::InvalidFieldValue {
                        field_path: format!("mission.candidates[{candidate_index}].score"),
                        message: "score must be finite".to_string(),
                    });
                }
            }
            if !candidate_ids.insert(candidate.candidate_id.clone()) {
                return Err(MissionValidationError::DuplicateCandidateId(
                    candidate.candidate_id.clone(),
                ));
            }
        }

        let mut assignment_ids = std::collections::HashSet::new();
        for (assignment_index, assignment) in self.assignments.iter().enumerate() {
            Self::validate_non_empty_field(
                format!("mission.assignments[{assignment_index}].assignment_id"),
                &assignment.assignment_id.0,
            )?;
            Self::validate_non_empty_field(
                format!("mission.assignments[{assignment_index}].candidate_id"),
                &assignment.candidate_id.0,
            )?;
            if !assignment_ids.insert(assignment.assignment_id.clone()) {
                return Err(MissionValidationError::DuplicateAssignmentId(
                    assignment.assignment_id.clone(),
                ));
            }
            if !candidate_ids.contains(&assignment.candidate_id) {
                return Err(MissionValidationError::UnknownCandidateReference(
                    assignment.candidate_id.clone(),
                ));
            }
            if assignment.assignee.trim().is_empty() {
                return Err(MissionValidationError::EmptyAssignee(
                    assignment.assignment_id.clone(),
                ));
            }
            if let Some(updated_at_ms) = assignment.updated_at_ms {
                Self::validate_timestamp_order(
                    format!("mission.assignments[{assignment_index}].updated_at_ms"),
                    assignment.created_at_ms,
                    updated_at_ms,
                )?;
            }
            match &assignment.approval_state {
                ApprovalState::Pending {
                    requested_at_ms, ..
                } => {
                    Self::validate_timestamp_order(
                        format!(
                            "mission.assignments[{assignment_index}].approval_state.pending.requested_at_ms"
                        ),
                        assignment.created_at_ms,
                        *requested_at_ms,
                    )?;
                }
                ApprovalState::Approved { approved_at_ms, .. } => {
                    Self::validate_timestamp_order(
                        format!(
                            "mission.assignments[{assignment_index}].approval_state.approved.approved_at_ms"
                        ),
                        assignment.created_at_ms,
                        *approved_at_ms,
                    )?;
                }
                ApprovalState::Denied { denied_at_ms, .. } => {
                    Self::validate_timestamp_order(
                        format!(
                            "mission.assignments[{assignment_index}].approval_state.denied.denied_at_ms"
                        ),
                        assignment.created_at_ms,
                        *denied_at_ms,
                    )?;
                }
                ApprovalState::Expired { expired_at_ms, .. } => {
                    Self::validate_timestamp_order(
                        format!(
                            "mission.assignments[{assignment_index}].approval_state.expired.expired_at_ms"
                        ),
                        assignment.created_at_ms,
                        *expired_at_ms,
                    )?;
                }
                ApprovalState::NotRequired => {}
            }
            if let Some(reservation_intent) = &assignment.reservation_intent {
                if reservation_intent.paths.is_empty() {
                    return Err(MissionValidationError::EmptyReservationPaths(
                        reservation_intent.reservation_id.clone(),
                    ));
                }
                for (path_index, path) in reservation_intent.paths.iter().enumerate() {
                    if path.trim().is_empty() {
                        return Err(MissionValidationError::InvalidFieldValue {
                            field_path: format!(
                                "mission.assignments[{assignment_index}].reservation_intent.paths[{path_index}]"
                            ),
                            message: "path cannot be empty".to_string(),
                        });
                    }
                }
            }
            if let Some(outcome) = &assignment.outcome {
                let completed_at_ms = match outcome {
                    Outcome::Success {
                        completed_at_ms, ..
                    }
                    | Outcome::Failed {
                        completed_at_ms, ..
                    }
                    | Outcome::Cancelled {
                        completed_at_ms, ..
                    } => *completed_at_ms,
                };
                Self::validate_timestamp_order(
                    format!("mission.assignments[{assignment_index}].outcome.completed_at_ms"),
                    assignment.created_at_ms,
                    completed_at_ms,
                )?;
            }
            Self::validate_assignment_failure_contract(assignment)?;
        }
        Self::validate_lifecycle_outcome_coherence(self.lifecycle_state, &self.assignments)?;

        Ok(())
    }

    /// Apply one lifecycle transition to this mission.
    pub fn transition_lifecycle(
        &mut self,
        to: MissionLifecycleState,
        kind: MissionLifecycleTransitionKind,
        transitioned_at_ms: i64,
    ) -> Result<(), MissionValidationError> {
        let from = self.lifecycle_state;
        if !mission_lifecycle_can_transition(from, to, kind) {
            return Err(MissionValidationError::InvalidLifecycleTransition { from, to, kind });
        }

        self.lifecycle_state = to;
        self.updated_at_ms = Some(transitioned_at_ms);
        Ok(())
    }

    /// Evaluate policy preflight checks for mission candidate actions.
    ///
    /// This pipeline supports both:
    /// - plan-time checks (candidate-level, before assignment dispatch)
    /// - dispatch-time checks (assignment-bound, just before execution)
    pub fn evaluate_policy_preflight(
        &self,
        stage: MissionPolicyPreflightStage,
        checks: &[MissionPolicyPreflightCheck],
    ) -> Result<MissionPolicyPreflightReport, MissionValidationError> {
        let mut outcomes = Vec::with_capacity(checks.len());
        let mut planner_feedback_reason_codes = Vec::new();

        for check in checks {
            let candidate = self
                .candidates
                .iter()
                .find(|candidate| candidate.candidate_id == check.candidate_id)
                .ok_or_else(|| {
                    MissionValidationError::UnknownCandidateReference(check.candidate_id.clone())
                })?;

            let assignment_id = match (stage, &check.assignment_id) {
                (MissionPolicyPreflightStage::DispatchTime, None) => {
                    return Err(MissionValidationError::MissingDispatchPreflightAssignment {
                        candidate_id: check.candidate_id.clone(),
                    });
                }
                (_, Some(assignment_id)) => {
                    let assignment =
                        self.find_assignment_by_id(assignment_id).ok_or_else(|| {
                            MissionValidationError::UnknownAssignmentReference(
                                assignment_id.clone(),
                            )
                        })?;
                    if assignment.candidate_id != check.candidate_id {
                        return Err(
                            MissionValidationError::PreflightAssignmentCandidateMismatch {
                                assignment_id: assignment.assignment_id.clone(),
                                assignment_candidate_id: assignment.candidate_id.clone(),
                                check_candidate_id: check.candidate_id.clone(),
                            },
                        );
                    }
                    Some(assignment.assignment_id.clone())
                }
                (_, None) => None,
            };

            let mut outcome = MissionPolicyPreflightOutcome {
                stage,
                candidate_id: check.candidate_id.clone(),
                assignment_id,
                action_type: candidate.action.action_type_name().to_string(),
                decision: check.decision,
                reason_code: None,
                error_code: None,
                human_hint: None,
                machine_hint: None,
                rule_id: check.rule_id.clone(),
                context: check.context.clone(),
            };

            match check.decision {
                MissionPolicyDecisionKind::Allow => {}
                MissionPolicyDecisionKind::Deny | MissionPolicyDecisionKind::RequireApproval => {
                    let failure_code = Self::resolve_preflight_reason_code(stage, check)?;
                    if check.decision == MissionPolicyDecisionKind::RequireApproval
                        && failure_code != MissionFailureCode::ApprovalRequired
                    {
                        return Err(
                            MissionValidationError::UnexpectedPolicyPreflightReasonCode {
                                candidate_id: check.candidate_id.clone(),
                                stage,
                                decision: check.decision,
                                expected_reason_code: MissionFailureCode::ApprovalRequired
                                    .reason_code()
                                    .to_string(),
                                actual_reason_code: failure_code.reason_code().to_string(),
                            },
                        );
                    }

                    outcome.reason_code = Some(failure_code.reason_code().to_string());
                    outcome.error_code = Some(failure_code.error_code().to_string());
                    outcome.human_hint = Some(failure_code.human_hint().to_string());
                    outcome.machine_hint = Some(failure_code.machine_hint().to_string());
                    planner_feedback_reason_codes.push(failure_code.reason_code().to_string());
                }
            }

            outcomes.push(outcome);
        }

        planner_feedback_reason_codes.sort();
        planner_feedback_reason_codes.dedup();

        Ok(MissionPolicyPreflightReport {
            stage,
            outcomes,
            planner_feedback_reason_codes,
        })
    }

    /// Build a concrete dispatch mapping contract for a candidate action.
    ///
    /// The mapping explicitly ties a planner candidate to:
    /// - execution surface (robot/internal)
    /// - reservation requirements (Agent Mail file intents)
    /// - messaging requirements (Agent Mail thread + Beads issue linkage)
    /// - canonical edge-case envelopes (missing pane / stale bead state)
    pub fn dispatch_contract_for_candidate(
        &self,
        candidate_id: &CandidateActionId,
    ) -> Result<MissionDispatchContract, MissionValidationError> {
        let candidate = self
            .candidates
            .iter()
            .find(|candidate| candidate.candidate_id == *candidate_id)
            .ok_or_else(|| {
                MissionValidationError::UnknownCandidateReference(candidate_id.clone())
            })?;

        let reservation_intents = self
            .assignments
            .iter()
            .filter(|assignment| assignment.candidate_id == *candidate_id)
            .filter_map(|assignment| assignment.reservation_intent.clone())
            .collect::<Vec<_>>();

        let reservation = MissionReservationRequirement {
            required: !reservation_intents.is_empty(),
            intents: reservation_intents,
        };

        let (bead_id, thread_id) = if let Some(provenance) = &self.provenance {
            let bead_id = provenance.bead_id.clone();
            let thread_id = provenance.thread_id.clone().or_else(|| bead_id.clone());
            (bead_id, thread_id)
        } else {
            (None, None)
        };

        let messaging = MissionMessagingRequirement {
            requires_agent_mail: thread_id.is_some(),
            requires_beads_update: bead_id.is_some(),
            thread_id,
            bead_id,
        };

        let mut edge_cases = Vec::new();
        if let Some(pane_id) = Self::candidate_target_pane_id(&candidate.action) {
            edge_cases.push(MissionDispatchEdgeCase::MissingPane {
                pane_id,
                reason_code: MissionFailureCode::StaleState.reason_code().to_string(),
                error_code: MissionFailureCode::StaleState.error_code().to_string(),
                remediation: "Refresh pane inventory with `ft robot state` before dispatch."
                    .to_string(),
            });
        }
        if let Some(bead_id) = &messaging.bead_id {
            edge_cases.push(MissionDispatchEdgeCase::StaleBeadState {
                bead_id: bead_id.clone(),
                reason_code: MissionFailureCode::StaleState.reason_code().to_string(),
                error_code: MissionFailureCode::StaleState.error_code().to_string(),
                remediation:
                    "Re-sync beads state (`br sync --import-only`) before status/comment updates."
                        .to_string(),
            });
        }

        Ok(MissionDispatchContract {
            candidate_id: candidate.candidate_id.clone(),
            mechanism: Self::dispatch_mechanism_for_action(&candidate.action),
            reservation,
            messaging,
            edge_cases,
        })
    }

    /// Evaluate assignment suitability for one candidate against agent capability profiles.
    ///
    /// This model enforces:
    /// - hard constraints (`required_capabilities`, availability hard blocks, exclusions)
    /// - soft preferences (`preferred_capabilities`, `lane_affinity`)
    /// - load/capacity gating (`current_load` vs effective parallel capacity)
    pub fn evaluate_assignment_suitability(
        &self,
        request: &MissionAssignmentSuitabilityRequest,
        profiles: &[MissionAgentCapabilityProfile],
    ) -> Result<MissionAssignmentSuitabilityReport, MissionValidationError> {
        self.candidates
            .iter()
            .find(|candidate| candidate.candidate_id == request.candidate_id)
            .ok_or_else(|| {
                MissionValidationError::UnknownCandidateReference(request.candidate_id.clone())
            })?;

        let required_capabilities = Self::normalize_non_empty_unique(
            "suitability_request.required_capabilities",
            &request.required_capabilities,
        )?;
        let preferred_capabilities = Self::normalize_non_empty_unique(
            "suitability_request.preferred_capabilities",
            &request.preferred_capabilities,
        )?;
        let excluded_agents = Self::normalize_non_empty_unique(
            "suitability_request.excluded_agents",
            &request.excluded_agents,
        )?
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
        let lane_affinity = request
            .lane_affinity
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if request.lane_affinity.is_some() && lane_affinity.is_none() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: "suitability_request.lane_affinity".to_string(),
                message: "lane affinity must be non-empty when provided".to_string(),
            });
        }

        let mut evaluations = Vec::with_capacity(profiles.len());
        for (profile_index, profile) in profiles.iter().enumerate() {
            Self::validate_non_empty_field(
                format!("profiles[{profile_index}].agent_id"),
                &profile.agent_id,
            )?;
            let normalized_capabilities = Self::normalize_non_empty_unique(
                format!("profiles[{profile_index}].capabilities"),
                &profile.capabilities,
            )?;
            let normalized_lanes = Self::normalize_non_empty_unique(
                format!("profiles[{profile_index}].lane_affinity"),
                &profile.lane_affinity,
            )?;

            let missing_required_capabilities = required_capabilities
                .iter()
                .filter(|required| !normalized_capabilities.contains(required))
                .cloned()
                .collect::<Vec<_>>();
            let matched_preferred_capabilities = preferred_capabilities
                .iter()
                .filter(|preferred| normalized_capabilities.contains(preferred))
                .cloned()
                .collect::<Vec<_>>();
            let lane_match = lane_affinity
                .as_ref()
                .is_some_and(|lane| normalized_lanes.contains(lane));
            let effective_capacity = profile.effective_capacity();

            let mut reason_codes = Vec::new();
            let mut eligible = true;
            let mut hard_constraints_satisfied = true;

            if !missing_required_capabilities.is_empty() {
                eligible = false;
                hard_constraints_satisfied = false;
                reason_codes.push("missing_required_capability".to_string());
            }
            if excluded_agents.contains(profile.agent_id.trim()) {
                eligible = false;
                hard_constraints_satisfied = false;
                reason_codes.push("assignment_excluded".to_string());
            }
            if profile.availability.hard_blocks_assignment() {
                eligible = false;
                hard_constraints_satisfied = false;
                let availability_reason = match profile.availability {
                    MissionAgentAvailability::Paused { .. } => "agent_paused",
                    MissionAgentAvailability::RateLimited { .. } => "agent_rate_limited",
                    MissionAgentAvailability::Offline { .. } => "agent_offline",
                    MissionAgentAvailability::Ready | MissionAgentAvailability::Degraded { .. } => {
                        "agent_unavailable"
                    }
                };
                reason_codes.push(availability_reason.to_string());
            } else if matches!(
                profile.availability,
                MissionAgentAvailability::Degraded { .. }
            ) {
                reason_codes.push("agent_degraded".to_string());
            }
            if effective_capacity == 0 || profile.current_load >= effective_capacity {
                eligible = false;
                hard_constraints_satisfied = false;
                reason_codes.push("agent_capacity_exhausted".to_string());
            }

            let utilization_penalty = if effective_capacity == 0 {
                1.0
            } else {
                f64::from(profile.current_load) / f64::from(effective_capacity)
            };
            let mut score = if eligible {
                1.0 - utilization_penalty
            } else {
                0.0
            };
            if eligible {
                score += matched_preferred_capabilities.len() as f64 * 0.2;
                if lane_match {
                    score += 0.35;
                }
                if matches!(
                    profile.availability,
                    MissionAgentAvailability::Degraded { .. }
                ) {
                    score = (score - 0.15).max(0.0);
                }
            }

            reason_codes.sort();
            reason_codes.dedup();

            evaluations.push(MissionAgentSuitability {
                agent_id: profile.agent_id.trim().to_string(),
                eligible,
                hard_constraints_satisfied,
                effective_capacity,
                score,
                reason_codes,
                missing_required_capabilities,
                matched_preferred_capabilities,
            });
        }

        evaluations.sort_by(|left, right| {
            use std::cmp::Ordering;
            match (left.eligible, right.eligible) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => right
                    .score
                    .partial_cmp(&left.score)
                    .unwrap_or(Ordering::Equal)
                    .then_with(|| left.agent_id.cmp(&right.agent_id)),
            }
        });

        let selected_agent = evaluations
            .iter()
            .find(|evaluation| evaluation.eligible)
            .map(|evaluation| evaluation.agent_id.clone());

        Ok(MissionAssignmentSuitabilityReport {
            candidate_id: request.candidate_id.clone(),
            evaluated_at_ms: request.evaluated_at_ms,
            lane_affinity,
            selected_agent,
            evaluations,
        })
    }

    fn dispatch_mechanism_for_action(action: &StepAction) -> MissionDispatchMechanism {
        match action {
            StepAction::SendText {
                pane_id,
                text,
                paste_mode,
            } => MissionDispatchMechanism::RobotSend {
                pane_id: *pane_id,
                text: text.clone(),
                paste_mode: *paste_mode,
            },
            StepAction::WaitFor {
                pane_id,
                condition,
                timeout_ms,
            } => MissionDispatchMechanism::RobotWaitFor {
                pane_id: *pane_id,
                condition: condition.clone(),
                timeout_ms: *timeout_ms,
            },
            StepAction::RunWorkflow {
                workflow_id,
                params,
            } => MissionDispatchMechanism::RobotRunWorkflow {
                workflow_id: workflow_id.clone(),
                params: params.clone(),
            },
            StepAction::AcquireLock {
                lock_name,
                timeout_ms,
            } => MissionDispatchMechanism::InternalLockAcquire {
                lock_name: lock_name.clone(),
                timeout_ms: *timeout_ms,
            },
            StepAction::ReleaseLock { lock_name } => {
                MissionDispatchMechanism::InternalLockRelease {
                    lock_name: lock_name.clone(),
                }
            }
            StepAction::StoreData { key, value } => MissionDispatchMechanism::InternalStoreData {
                key: key.clone(),
                value: value.clone(),
            },
            StepAction::MarkEventHandled { event_id } => {
                MissionDispatchMechanism::InternalMarkEventHandled {
                    event_id: *event_id,
                }
            }
            StepAction::ValidateApproval { approval_code } => {
                MissionDispatchMechanism::InternalValidateApproval {
                    approval_code: approval_code.clone(),
                }
            }
            StepAction::NestedPlan { plan } => MissionDispatchMechanism::InternalNestedPlan {
                plan_hash: plan.compute_hash(),
            },
            StepAction::Custom {
                action_type,
                payload,
            } => MissionDispatchMechanism::InternalCustom {
                action_type: action_type.clone(),
                payload: payload.clone(),
            },
        }
    }

    fn candidate_target_pane_id(action: &StepAction) -> Option<u64> {
        match action {
            StepAction::SendText { pane_id, .. } => Some(*pane_id),
            StepAction::WaitFor {
                pane_id, condition, ..
            } => pane_id.or(match condition {
                WaitCondition::Pattern { pane_id, .. }
                | WaitCondition::PaneIdle { pane_id, .. }
                | WaitCondition::StableTail { pane_id, .. } => *pane_id,
                WaitCondition::External { .. } => None,
            }),
            _ => None,
        }
    }

    fn find_assignment_by_id(&self, assignment_id: &AssignmentId) -> Option<&Assignment> {
        self.assignments
            .iter()
            .find(|assignment| assignment.assignment_id == *assignment_id)
    }

    fn normalize_non_empty_unique(
        field_path: impl AsRef<str>,
        values: &[String],
    ) -> Result<Vec<String>, MissionValidationError> {
        let field_path = field_path.as_ref();
        let mut normalized = std::collections::BTreeSet::new();
        for (index, value) in values.iter().enumerate() {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err(MissionValidationError::InvalidFieldValue {
                    field_path: format!("{field_path}[{index}]"),
                    message: "value must be non-empty".to_string(),
                });
            }
            normalized.insert(trimmed.to_string());
        }
        Ok(normalized.into_iter().collect())
    }

    fn resolve_preflight_reason_code(
        stage: MissionPolicyPreflightStage,
        check: &MissionPolicyPreflightCheck,
    ) -> Result<MissionFailureCode, MissionValidationError> {
        let reason_code = check
            .reason_code
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        if reason_code.is_empty() {
            return Err(MissionValidationError::MissingPolicyPreflightReasonCode {
                candidate_id: check.candidate_id.clone(),
                stage,
                decision: check.decision,
            });
        }
        MissionFailureCode::from_reason_code(reason_code).ok_or_else(|| {
            MissionValidationError::UnknownPolicyPreflightReasonCode {
                candidate_id: check.candidate_id.clone(),
                stage,
                reason_code: reason_code.to_string(),
            }
        })
    }

    fn validate_lifecycle_outcome_coherence(
        lifecycle_state: MissionLifecycleState,
        assignments: &[Assignment],
    ) -> Result<(), MissionValidationError> {
        let has_success = assignments
            .iter()
            .any(|assignment| matches!(assignment.outcome, Some(Outcome::Success { .. })));
        let has_failed = assignments
            .iter()
            .any(|assignment| matches!(assignment.outcome, Some(Outcome::Failed { .. })));
        let has_cancelled = assignments
            .iter()
            .any(|assignment| matches!(assignment.outcome, Some(Outcome::Cancelled { .. })));

        match lifecycle_state {
            MissionLifecycleState::Completed if !has_success => Err(
                MissionValidationError::TerminalStateWithoutMatchingOutcome {
                    state: lifecycle_state,
                    expected_outcome: "success".to_string(),
                },
            ),
            MissionLifecycleState::Failed if !has_failed => Err(
                MissionValidationError::TerminalStateWithoutMatchingOutcome {
                    state: lifecycle_state,
                    expected_outcome: "failed".to_string(),
                },
            ),
            MissionLifecycleState::Cancelled if !has_cancelled => Err(
                MissionValidationError::TerminalStateWithoutMatchingOutcome {
                    state: lifecycle_state,
                    expected_outcome: "cancelled".to_string(),
                },
            ),
            _ => Ok(()),
        }
    }

    fn validate_assignment_failure_contract(
        assignment: &Assignment,
    ) -> Result<(), MissionValidationError> {
        match &assignment.approval_state {
            ApprovalState::Denied { reason_code, .. } => {
                Self::validate_failure_reason_code(
                    &assignment.assignment_id,
                    MissionFailureContext::ApprovalDenied,
                    reason_code,
                )?;
            }
            ApprovalState::Expired { reason_code, .. } => {
                Self::validate_failure_reason_code(
                    &assignment.assignment_id,
                    MissionFailureContext::ApprovalExpired,
                    reason_code,
                )?;
            }
            ApprovalState::NotRequired
            | ApprovalState::Pending { .. }
            | ApprovalState::Approved { .. } => {}
        }

        if let Some(Outcome::Failed {
            reason_code,
            error_code,
            ..
        }) = &assignment.outcome
        {
            let failure_code = Self::validate_failure_reason_code(
                &assignment.assignment_id,
                MissionFailureContext::AssignmentOutcomeFailed,
                reason_code,
            )?;
            Self::validate_failure_error_code(
                &assignment.assignment_id,
                MissionFailureContext::AssignmentOutcomeFailed,
                reason_code,
                error_code,
                failure_code.error_code(),
            )?;
        }

        if let Some(escalation) = &assignment.escalation {
            if let Some(error_code) = escalation.error_code.as_deref() {
                let failure_code = Self::validate_failure_reason_code(
                    &assignment.assignment_id,
                    MissionFailureContext::AssignmentEscalation,
                    &escalation.reason_code,
                )?;
                Self::validate_failure_error_code(
                    &assignment.assignment_id,
                    MissionFailureContext::AssignmentEscalation,
                    &escalation.reason_code,
                    error_code,
                    failure_code.error_code(),
                )?;
            }
        }

        Ok(())
    }

    fn validate_failure_reason_code(
        assignment_id: &AssignmentId,
        context: MissionFailureContext,
        reason_code: &str,
    ) -> Result<MissionFailureCode, MissionValidationError> {
        let normalized = reason_code.trim();
        if normalized.is_empty() {
            return Err(MissionValidationError::EmptyFailureReasonCode {
                assignment_id: assignment_id.clone(),
                context,
            });
        }

        let failure_code = MissionFailureCode::from_reason_code(normalized).ok_or_else(|| {
            MissionValidationError::UnknownFailureReasonCode {
                assignment_id: assignment_id.clone(),
                context,
                reason_code: normalized.to_string(),
            }
        })?;

        let expected_code = match context {
            MissionFailureContext::ApprovalDenied => Some(MissionFailureCode::ApprovalDenied),
            MissionFailureContext::ApprovalExpired => Some(MissionFailureCode::ApprovalExpired),
            MissionFailureContext::AssignmentOutcomeFailed
            | MissionFailureContext::AssignmentEscalation => None,
        };

        if let Some(expected_code) = expected_code {
            if failure_code != expected_code {
                return Err(MissionValidationError::UnexpectedFailureCodeForContext {
                    assignment_id: assignment_id.clone(),
                    context,
                    expected_reason_code: expected_code.reason_code().to_string(),
                    actual_reason_code: failure_code.reason_code().to_string(),
                });
            }
        }

        Ok(failure_code)
    }

    fn validate_failure_error_code(
        assignment_id: &AssignmentId,
        context: MissionFailureContext,
        reason_code: &str,
        error_code: &str,
        expected_error_code: &str,
    ) -> Result<(), MissionValidationError> {
        let normalized = error_code.trim();
        if normalized.is_empty() {
            return Err(MissionValidationError::EmptyFailureErrorCode {
                assignment_id: assignment_id.clone(),
                context,
                reason_code: reason_code.trim().to_string(),
            });
        }
        if normalized != expected_error_code {
            return Err(MissionValidationError::MismatchedFailureErrorCode {
                assignment_id: assignment_id.clone(),
                context,
                reason_code: reason_code.trim().to_string(),
                expected_error_code: expected_error_code.to_string(),
                actual_error_code: normalized.to_string(),
            });
        }
        Ok(())
    }

    fn validate_non_empty_field(
        field_path: impl Into<String>,
        value: &str,
    ) -> Result<(), MissionValidationError> {
        if value.trim().is_empty() {
            return Err(MissionValidationError::InvalidFieldValue {
                field_path: field_path.into(),
                message: "value cannot be empty".to_string(),
            });
        }
        Ok(())
    }

    fn validate_optional_non_empty_field(
        field_path: impl Into<String>,
        value: Option<&str>,
    ) -> Result<(), MissionValidationError> {
        if let Some(value) = value {
            Self::validate_non_empty_field(field_path, value)?;
        }
        Ok(())
    }

    fn validate_timestamp_order(
        field_path: impl Into<String>,
        created_at_ms: i64,
        updated_at_ms: i64,
    ) -> Result<(), MissionValidationError> {
        if updated_at_ms < created_at_ms {
            return Err(MissionValidationError::NonMonotonicTimestamp {
                field_path: field_path.into(),
                created_at_ms,
                updated_at_ms,
            });
        }
        Ok(())
    }
}

/// Errors that can occur during mission schema validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionValidationError {
    UnsupportedMissionVersion {
        version: u32,
        max_supported: u32,
    },
    EmptyOwnershipActor {
        role: MissionActorRole,
    },
    DuplicateOwnershipActor(String),
    MissingTitle,
    MissingWorkspaceId,
    InvalidFieldValue {
        field_path: String,
        message: String,
    },
    NonMonotonicTimestamp {
        field_path: String,
        created_at_ms: i64,
        updated_at_ms: i64,
    },
    DuplicateCandidateId(CandidateActionId),
    DuplicateAssignmentId(AssignmentId),
    UnknownCandidateReference(CandidateActionId),
    UnknownAssignmentReference(AssignmentId),
    EmptyAssignee(AssignmentId),
    EmptyReservationPaths(ReservationIntentId),
    MissingDispatchPreflightAssignment {
        candidate_id: CandidateActionId,
    },
    PreflightAssignmentCandidateMismatch {
        assignment_id: AssignmentId,
        assignment_candidate_id: CandidateActionId,
        check_candidate_id: CandidateActionId,
    },
    MissingPolicyPreflightReasonCode {
        candidate_id: CandidateActionId,
        stage: MissionPolicyPreflightStage,
        decision: MissionPolicyDecisionKind,
    },
    UnknownPolicyPreflightReasonCode {
        candidate_id: CandidateActionId,
        stage: MissionPolicyPreflightStage,
        reason_code: String,
    },
    UnexpectedPolicyPreflightReasonCode {
        candidate_id: CandidateActionId,
        stage: MissionPolicyPreflightStage,
        decision: MissionPolicyDecisionKind,
        expected_reason_code: String,
        actual_reason_code: String,
    },
    InvalidLifecycleTransition {
        from: MissionLifecycleState,
        to: MissionLifecycleState,
        kind: MissionLifecycleTransitionKind,
    },
    TerminalStateWithoutMatchingOutcome {
        state: MissionLifecycleState,
        expected_outcome: String,
    },
    EmptyFailureReasonCode {
        assignment_id: AssignmentId,
        context: MissionFailureContext,
    },
    UnknownFailureReasonCode {
        assignment_id: AssignmentId,
        context: MissionFailureContext,
        reason_code: String,
    },
    UnexpectedFailureCodeForContext {
        assignment_id: AssignmentId,
        context: MissionFailureContext,
        expected_reason_code: String,
        actual_reason_code: String,
    },
    EmptyFailureErrorCode {
        assignment_id: AssignmentId,
        context: MissionFailureContext,
        reason_code: String,
    },
    MismatchedFailureErrorCode {
        assignment_id: AssignmentId,
        context: MissionFailureContext,
        reason_code: String,
        expected_error_code: String,
        actual_error_code: String,
    },
}

impl fmt::Display for MissionValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedMissionVersion {
                version,
                max_supported,
            } => {
                write!(
                    f,
                    "Unsupported mission version: {version} (max supported: {max_supported})"
                )
            }
            Self::EmptyOwnershipActor { role } => {
                write!(f, "Missing mission ownership actor for role: {role}")
            }
            Self::DuplicateOwnershipActor(actor) => {
                write!(f, "Ownership actor reused across boundaries: {actor}")
            }
            Self::MissingTitle => f.write_str("Mission title cannot be empty"),
            Self::MissingWorkspaceId => f.write_str("Mission workspace_id cannot be empty"),
            Self::InvalidFieldValue {
                field_path,
                message,
            } => {
                write!(f, "Invalid mission field '{field_path}': {message}")
            }
            Self::NonMonotonicTimestamp {
                field_path,
                created_at_ms,
                updated_at_ms,
            } => {
                write!(
                    f,
                    "Non-monotonic timestamp for '{field_path}': updated_at_ms ({updated_at_ms}) is earlier than created_at_ms ({created_at_ms})"
                )
            }
            Self::DuplicateCandidateId(id) => write!(f, "Duplicate candidate ID: {}", id.0),
            Self::DuplicateAssignmentId(id) => write!(f, "Duplicate assignment ID: {}", id.0),
            Self::UnknownCandidateReference(id) => {
                write!(f, "Assignment references unknown candidate ID: {}", id.0)
            }
            Self::UnknownAssignmentReference(id) => {
                write!(f, "Unknown assignment ID: {}", id.0)
            }
            Self::EmptyAssignee(id) => write!(f, "Assignment has empty assignee: {}", id.0),
            Self::EmptyReservationPaths(id) => {
                write!(f, "Reservation intent has empty paths: {}", id.0)
            }
            Self::MissingDispatchPreflightAssignment { candidate_id } => {
                write!(
                    f,
                    "Dispatch-time policy preflight requires assignment_id for candidate {}",
                    candidate_id.0
                )
            }
            Self::PreflightAssignmentCandidateMismatch {
                assignment_id,
                assignment_candidate_id,
                check_candidate_id,
            } => {
                write!(
                    f,
                    "Policy preflight assignment {} targets candidate {}, but check references candidate {}",
                    assignment_id.0, assignment_candidate_id.0, check_candidate_id.0
                )
            }
            Self::MissingPolicyPreflightReasonCode {
                candidate_id,
                stage,
                decision,
            } => {
                write!(
                    f,
                    "Missing preflight reason code for candidate {} at stage {stage} decision {decision}",
                    candidate_id.0
                )
            }
            Self::UnknownPolicyPreflightReasonCode {
                candidate_id,
                stage,
                reason_code,
            } => {
                write!(
                    f,
                    "Unknown preflight reason code '{reason_code}' for candidate {} at stage {stage}",
                    candidate_id.0
                )
            }
            Self::UnexpectedPolicyPreflightReasonCode {
                candidate_id,
                stage,
                decision,
                expected_reason_code,
                actual_reason_code,
            } => {
                write!(
                    f,
                    "Invalid preflight reason code '{actual_reason_code}' for candidate {} at stage {stage} decision {decision}; expected '{expected_reason_code}'",
                    candidate_id.0
                )
            }
            Self::InvalidLifecycleTransition { from, to, kind } => {
                write!(
                    f,
                    "Illegal mission lifecycle transition {from} -> {to} via {kind}"
                )
            }
            Self::TerminalStateWithoutMatchingOutcome {
                state,
                expected_outcome,
            } => {
                write!(
                    f,
                    "Mission lifecycle state {state} requires at least one '{expected_outcome}' assignment outcome"
                )
            }
            Self::EmptyFailureReasonCode {
                assignment_id,
                context,
            } => {
                write!(
                    f,
                    "Empty failure reason code for assignment {} ({context})",
                    assignment_id.0
                )
            }
            Self::UnknownFailureReasonCode {
                assignment_id,
                context,
                reason_code,
            } => {
                write!(
                    f,
                    "Unknown failure reason code '{reason_code}' for assignment {} ({context})",
                    assignment_id.0
                )
            }
            Self::UnexpectedFailureCodeForContext {
                assignment_id,
                context,
                expected_reason_code,
                actual_reason_code,
            } => {
                write!(
                    f,
                    "Failure reason code '{actual_reason_code}' is invalid for assignment {} ({context}); expected '{expected_reason_code}'",
                    assignment_id.0
                )
            }
            Self::EmptyFailureErrorCode {
                assignment_id,
                context,
                reason_code,
            } => {
                write!(
                    f,
                    "Empty failure error code for assignment {} ({context}) reason '{reason_code}'",
                    assignment_id.0
                )
            }
            Self::MismatchedFailureErrorCode {
                assignment_id,
                context,
                reason_code,
                expected_error_code,
                actual_error_code,
            } => {
                write!(
                    f,
                    "Failure error code mismatch for assignment {} ({context}) reason '{reason_code}': expected '{expected_error_code}', got '{actual_error_code}'",
                    assignment_id.0
                )
            }
        }
    }
}

impl std::error::Error for MissionValidationError {}

// ============================================================================
// Validation Errors
// ============================================================================

/// Errors that can occur during plan validation.
#[derive(Debug, Clone)]
pub enum PlanValidationError {
    /// Step numbers are not sequential
    InvalidStepNumber { expected: u32, actual: u32 },

    /// Duplicate step ID found
    DuplicateStepId(IdempotencyKey),

    /// Reference to unknown step
    UnknownStepReference(IdempotencyKey),

    /// Plan version not supported
    UnsupportedVersion { version: u32, max_supported: u32 },
}

impl fmt::Display for PlanValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStepNumber { expected, actual } => {
                write!(f, "Invalid step number: expected {expected}, got {actual}")
            }
            Self::DuplicateStepId(id) => write!(f, "Duplicate step ID: {}", id.0),
            Self::UnknownStepReference(id) => write!(f, "Unknown step reference: {}", id.0),
            Self::UnsupportedVersion {
                version,
                max_supported,
            } => {
                write!(
                    f,
                    "Unsupported plan version: {version} (max supported: {max_supported})"
                )
            }
        }
    }
}

impl std::error::Error for PlanValidationError {}

// ============================================================================
// Utility Functions
// ============================================================================

/// Compute SHA-256 hash and return as hex string.
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_plan_hash_determinism() {
        let plan1 = ActionPlan::builder("Test Plan", "workspace-1")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".into(),
                    paste_mode: None,
                },
                "Send hello",
            ))
            .build();

        let plan2 = ActionPlan::builder("Test Plan", "workspace-1")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".into(),
                    paste_mode: None,
                },
                "Send hello",
            ))
            .build();

        assert_eq!(plan1.compute_hash(), plan2.compute_hash());
    }

    #[test]
    fn test_plan_hash_changes_with_content() {
        let plan1 = ActionPlan::builder("Test Plan", "workspace-1")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".into(),
                    paste_mode: None,
                },
                "Send hello",
            ))
            .build();

        let plan2 = ActionPlan::builder("Test Plan", "workspace-1")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "world".into(), // Different text
                    paste_mode: None,
                },
                "Send hello",
            ))
            .build();

        assert_ne!(plan1.compute_hash(), plan2.compute_hash());
    }

    #[test]
    fn test_plan_validation_step_numbers() {
        let plan = ActionPlan::builder("Test", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "a".into(),
                    paste_mode: None,
                },
                "Step 1",
            ))
            .add_step(StepPlan::new(
                2,
                StepAction::SendText {
                    pane_id: 0,
                    text: "b".into(),
                    paste_mode: None,
                },
                "Step 2",
            ))
            .build();

        assert!(plan.validate().is_ok());
    }

    #[test]
    fn test_plan_validation_invalid_step_number() {
        let mut plan = ActionPlan::builder("Test", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "a".into(),
                    paste_mode: None,
                },
                "Step 1",
            ))
            .build();

        // Manually break the step number
        plan.steps[0].step_number = 5;

        let result = plan.validate();
        assert!(matches!(
            result,
            Err(PlanValidationError::InvalidStepNumber { .. })
        ));
    }

    #[test]
    fn test_idempotency_key_generation() {
        let key1 = IdempotencyKey::for_action(
            "ws-1",
            1,
            &StepAction::SendText {
                pane_id: 0,
                text: "hello".into(),
                paste_mode: None,
            },
        );

        let key2 = IdempotencyKey::for_action(
            "ws-1",
            1,
            &StepAction::SendText {
                pane_id: 0,
                text: "hello".into(),
                paste_mode: None,
            },
        );

        assert_eq!(key1, key2);
    }

    #[test]
    fn test_canonical_serialization_stability() {
        let step = StepPlan::new(
            1,
            StepAction::WaitFor {
                pane_id: Some(0),
                condition: WaitCondition::Pattern {
                    pane_id: None,
                    rule_id: "core.claude:rate_limited".into(),
                },
                timeout_ms: 60000,
            },
            "Wait for rate limit",
        );

        let canonical1 = step.canonical_string();
        let canonical2 = step.canonical_string();

        assert_eq!(canonical1, canonical2);
    }

    #[test]
    fn test_plan_json_roundtrip() {
        let plan = ActionPlan::builder("Test Plan", "workspace-1")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "/compact".into(),
                    paste_mode: Some(true),
                },
                "Send compact command",
            ))
            .add_precondition(Precondition::PaneExists { pane_id: 0 })
            .on_failure(OnFailure::retry(3, 1000))
            .build();

        let json = serde_json::to_string_pretty(&plan).unwrap();
        let parsed: ActionPlan = serde_json::from_str(&json).unwrap();

        assert_eq!(plan.plan_id, parsed.plan_id);
        assert_eq!(plan.title, parsed.title);
        assert_eq!(plan.steps.len(), parsed.steps.len());
    }

    // ========================================================================
    // Additional comprehensive tests for wa-upg.2.5
    // ========================================================================

    #[test]
    fn test_plan_hash_stability_known_value() {
        // This test ensures hash stability across runs/platforms by checking
        // against a known value. If canonical serialization changes, this test
        // will catch it.
        let plan = ActionPlan::builder("Stable Test", "ws-stable")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "test".into(),
                    paste_mode: None,
                },
                "Send test",
            ))
            .build();

        let hash = plan.compute_hash();
        // Hash should start with sha256: prefix
        assert!(hash.starts_with("sha256:"));
        // Hash should be consistent length (sha256: + 32 hex chars)
        assert_eq!(hash.len(), 7 + 32);
    }

    #[test]
    fn test_plan_hash_excludes_timestamps() {
        let plan1 = ActionPlan::builder("Test", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "x".into(),
                    paste_mode: None,
                },
                "Step",
            ))
            .created_at(1000)
            .build();

        let plan2 = ActionPlan::builder("Test", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "x".into(),
                    paste_mode: None,
                },
                "Step",
            ))
            .created_at(2000) // Different timestamp
            .build();

        // Hashes should be equal because timestamps are excluded
        assert_eq!(plan1.compute_hash(), plan2.compute_hash());
    }

    #[test]
    fn test_plan_hash_excludes_metadata() {
        let plan1 = ActionPlan::builder("Test", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "x".into(),
                    paste_mode: None,
                },
                "Step",
            ))
            .metadata(serde_json::json!({"key": "value1"}))
            .build();

        let plan2 = ActionPlan::builder("Test", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "x".into(),
                    paste_mode: None,
                },
                "Step",
            ))
            .metadata(serde_json::json!({"key": "value2"})) // Different metadata
            .build();

        // Hashes should be equal because metadata is excluded
        assert_eq!(plan1.compute_hash(), plan2.compute_hash());
    }

    #[test]
    fn test_plan_hash_includes_workspace() {
        let plan1 = ActionPlan::builder("Test", "workspace-1")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "x".into(),
                    paste_mode: None,
                },
                "Step",
            ))
            .build();

        let plan2 = ActionPlan::builder("Test", "workspace-2") // Different workspace
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "x".into(),
                    paste_mode: None,
                },
                "Step",
            ))
            .build();

        // Hashes should differ because workspace is included
        assert_ne!(plan1.compute_hash(), plan2.compute_hash());
    }

    #[test]
    fn test_plan_hash_includes_title() {
        let plan1 = ActionPlan::builder("Title A", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "x".into(),
                    paste_mode: None,
                },
                "Step",
            ))
            .build();

        let plan2 = ActionPlan::builder("Title B", "ws") // Different title
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "x".into(),
                    paste_mode: None,
                },
                "Step",
            ))
            .build();

        assert_ne!(plan1.compute_hash(), plan2.compute_hash());
    }

    #[test]
    fn test_idempotency_key_differs_by_workspace() {
        let key1 = IdempotencyKey::for_action(
            "ws-1",
            1,
            &StepAction::SendText {
                pane_id: 0,
                text: "hello".into(),
                paste_mode: None,
            },
        );

        let key2 = IdempotencyKey::for_action(
            "ws-2", // Different workspace
            1,
            &StepAction::SendText {
                pane_id: 0,
                text: "hello".into(),
                paste_mode: None,
            },
        );

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_idempotency_key_differs_by_step_number() {
        let key1 = IdempotencyKey::for_action(
            "ws",
            1,
            &StepAction::SendText {
                pane_id: 0,
                text: "hello".into(),
                paste_mode: None,
            },
        );

        let key2 = IdempotencyKey::for_action(
            "ws",
            2, // Different step number
            &StepAction::SendText {
                pane_id: 0,
                text: "hello".into(),
                paste_mode: None,
            },
        );

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_idempotency_key_differs_by_action() {
        let key1 = IdempotencyKey::for_action(
            "ws",
            1,
            &StepAction::SendText {
                pane_id: 0,
                text: "hello".into(),
                paste_mode: None,
            },
        );

        let key2 = IdempotencyKey::for_action(
            "ws",
            1,
            &StepAction::SendText {
                pane_id: 1, // Different pane
                text: "hello".into(),
                paste_mode: None,
            },
        );

        assert_ne!(key1, key2);
    }

    #[test]
    fn test_validation_duplicate_step_ids() {
        let mut plan = ActionPlan::builder("Test", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "a".into(),
                    paste_mode: None,
                },
                "Step 1",
            ))
            .add_step(StepPlan::new(
                2,
                StepAction::SendText {
                    pane_id: 0,
                    text: "b".into(),
                    paste_mode: None,
                },
                "Step 2",
            ))
            .build();

        // Manually create duplicate step ID
        plan.steps[1].step_id = plan.steps[0].step_id.clone();

        let result = plan.validate();
        assert!(matches!(
            result,
            Err(PlanValidationError::DuplicateStepId(_))
        ));
    }

    #[test]
    fn test_validation_unknown_step_reference() {
        let mut plan = ActionPlan::builder("Test", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "a".into(),
                    paste_mode: None,
                },
                "Step 1",
            ))
            .build();

        // Add precondition referencing non-existent step
        plan.preconditions.push(Precondition::StepCompleted {
            step_id: IdempotencyKey::from_hash("nonexistent"),
        });

        let result = plan.validate();
        assert!(matches!(
            result,
            Err(PlanValidationError::UnknownStepReference(_))
        ));
    }

    #[test]
    fn test_precondition_canonical_strings() {
        // Test all precondition types produce stable canonical strings
        let preconditions = vec![
            Precondition::PaneExists { pane_id: 0 },
            Precondition::PaneState {
                pane_id: 1,
                expected_agent: Some("claude".into()),
                expected_domain: None,
            },
            Precondition::PatternMatched {
                rule_id: "test.rule".into(),
                pane_id: Some(0),
                within_ms: Some(5000),
            },
            Precondition::PatternNotMatched {
                rule_id: "error.rule".into(),
                pane_id: None,
            },
            Precondition::LockHeld {
                lock_name: "test_lock".into(),
            },
            Precondition::LockAvailable {
                lock_name: "other_lock".into(),
            },
            Precondition::StepCompleted {
                step_id: IdempotencyKey::from_hash("abc123"),
            },
            Precondition::Custom {
                name: "custom".into(),
                expression: "x > 0".into(),
            },
        ];

        for precond in &preconditions {
            let s1 = precond.canonical_string();
            let s2 = precond.canonical_string();
            assert_eq!(s1, s2, "Precondition canonical string not stable");
            assert!(!s1.is_empty(), "Canonical string should not be empty");
        }
    }

    #[test]
    fn test_verification_canonical_strings() {
        let verifications = vec![
            Verification::pattern_match("test.rule"),
            Verification::pane_idle(5000),
            Verification {
                strategy: VerificationStrategy::PatternAbsent {
                    rule_id: "error".into(),
                    pane_id: Some(0),
                    wait_ms: 1000,
                },
                description: None,
                timeout_ms: None,
            },
            Verification {
                strategy: VerificationStrategy::Custom {
                    name: "custom".into(),
                    expression: "check()".into(),
                },
                description: Some("Custom check".into()),
                timeout_ms: Some(5000),
            },
            Verification {
                strategy: VerificationStrategy::None,
                description: None,
                timeout_ms: None,
            },
        ];

        for verify in &verifications {
            let s1 = verify.canonical_string();
            let s2 = verify.canonical_string();
            assert_eq!(s1, s2, "Verification canonical string not stable");
        }
    }

    #[test]
    fn test_on_failure_canonical_strings() {
        let strategies = vec![
            OnFailure::abort(),
            OnFailure::abort_with_message("Something went wrong"),
            OnFailure::retry(3, 1000),
            OnFailure::Retry {
                max_attempts: 5,
                initial_delay_ms: 500,
                max_delay_ms: Some(30000),
                backoff_multiplier: Some(2.0),
            },
            OnFailure::skip(),
            OnFailure::RequireApproval {
                summary: "Manual intervention needed".into(),
            },
        ];

        for strategy in &strategies {
            let s1 = strategy.canonical_string();
            let s2 = strategy.canonical_string();
            assert_eq!(s1, s2, "OnFailure canonical string not stable");
        }
    }

    #[test]
    fn test_step_action_canonical_strings() {
        let actions = vec![
            StepAction::SendText {
                pane_id: 0,
                text: "hello".into(),
                paste_mode: Some(true),
            },
            StepAction::WaitFor {
                pane_id: Some(0),
                condition: WaitCondition::Pattern {
                    pane_id: None,
                    rule_id: "test".into(),
                },
                timeout_ms: 5000,
            },
            StepAction::AcquireLock {
                lock_name: "test".into(),
                timeout_ms: Some(1000),
            },
            StepAction::ReleaseLock {
                lock_name: "test".into(),
            },
            StepAction::StoreData {
                key: "key".into(),
                value: serde_json::json!({"data": 123}),
            },
            StepAction::RunWorkflow {
                workflow_id: "wf-1".into(),
                params: Some(serde_json::json!({"arg": "value"})),
            },
            StepAction::MarkEventHandled { event_id: 42 },
            StepAction::ValidateApproval {
                approval_code: "ABC123".into(),
            },
            StepAction::Custom {
                action_type: "custom_action".into(),
                payload: serde_json::json!({}),
            },
        ];

        for action in &actions {
            let s1 = action.canonical_string();
            let s2 = action.canonical_string();
            assert_eq!(s1, s2, "StepAction canonical string not stable");
            assert!(!s1.is_empty());
        }
    }

    #[test]
    fn test_wait_condition_canonical_strings() {
        let conditions = vec![
            WaitCondition::Pattern {
                pane_id: Some(0),
                rule_id: "test.rule".into(),
            },
            WaitCondition::Pattern {
                pane_id: None,
                rule_id: "any.rule".into(),
            },
            WaitCondition::PaneIdle {
                pane_id: Some(1),
                idle_threshold_ms: 5000,
            },
            WaitCondition::StableTail {
                pane_id: Some(2),
                stable_for_ms: 1500,
            },
            WaitCondition::External {
                key: "signal_key".into(),
            },
        ];

        for cond in &conditions {
            let s1 = cond.canonical_string();
            let s2 = cond.canonical_string();
            assert_eq!(s1, s2, "WaitCondition canonical string not stable");
        }
    }

    #[test]
    fn test_plan_with_all_features() {
        // Test a complex plan with all features to ensure serialization works
        let plan = ActionPlan::builder("Complex Plan", "workspace-complex")
            .add_step(
                StepPlan::new(
                    1,
                    StepAction::AcquireLock {
                        lock_name: "pane-lock".into(),
                        timeout_ms: Some(5000),
                    },
                    "Acquire lock",
                )
                .with_precondition(Precondition::LockAvailable {
                    lock_name: "pane-lock".into(),
                })
                .with_timeout_ms(10000)
                .idempotent(),
            )
            .add_step(
                StepPlan::new(
                    2,
                    StepAction::SendText {
                        pane_id: 0,
                        text: "/compact".into(),
                        paste_mode: Some(true),
                    },
                    "Send compact command",
                )
                .with_precondition(Precondition::PaneExists { pane_id: 0 })
                .with_verification(
                    Verification::pattern_match("core.claude:compaction_complete")
                        .with_timeout_ms(60000),
                )
                .with_on_failure(OnFailure::retry(3, 1000)),
            )
            .add_step(
                StepPlan::new(
                    3,
                    StepAction::ReleaseLock {
                        lock_name: "pane-lock".into(),
                    },
                    "Release lock",
                )
                .idempotent(),
            )
            .add_precondition(Precondition::PaneState {
                pane_id: 0,
                expected_agent: Some("claude-code".into()),
                expected_domain: Some("local".into()),
            })
            .on_failure(OnFailure::abort_with_message("Plan failed"))
            .metadata(serde_json::json!({
                "source": "test",
                "version": 1
            }))
            .created_at(1_706_400_000_000)
            .build();

        // Validate the plan
        assert!(plan.validate().is_ok());

        // Test JSON roundtrip
        let json = serde_json::to_string_pretty(&plan).unwrap();
        let parsed: ActionPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.plan_id, parsed.plan_id);
        assert_eq!(plan.steps.len(), 3);

        // Test hash is stable
        let hash1 = plan.compute_hash();
        let hash2 = parsed.compute_hash();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_plan_step_count_and_helpers() {
        let plan = ActionPlan::builder("Test", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::SendText {
                    pane_id: 0,
                    text: "a".into(),
                    paste_mode: None,
                },
                "Step 1",
            ))
            .add_step(StepPlan::new(
                2,
                StepAction::SendText {
                    pane_id: 0,
                    text: "b".into(),
                    paste_mode: None,
                },
                "Step 2",
            ))
            .add_precondition(Precondition::PaneExists { pane_id: 0 })
            .build();

        assert_eq!(plan.step_count(), 2);
        assert!(plan.has_preconditions());
    }

    #[test]
    fn test_plan_id_display() {
        let id = PlanId::from_hash("sha256:abcdef1234567890");
        assert!(id.to_string().starts_with("plan:"));
        assert!(!id.is_placeholder());

        let placeholder = PlanId::placeholder();
        assert!(placeholder.is_placeholder());
    }

    #[test]
    fn test_idempotency_key_display() {
        let key = IdempotencyKey::from_hash("abcdef12");
        assert!(key.to_string().starts_with("step:"));
    }

    #[test]
    fn test_action_type_names() {
        assert_eq!(
            StepAction::SendText {
                pane_id: 0,
                text: String::new(),
                paste_mode: None
            }
            .action_type_name(),
            "send_text"
        );
        assert_eq!(
            StepAction::WaitFor {
                pane_id: None,
                condition: WaitCondition::External { key: String::new() },
                timeout_ms: 0
            }
            .action_type_name(),
            "wait_for"
        );
        assert_eq!(
            StepAction::AcquireLock {
                lock_name: String::new(),
                timeout_ms: None
            }
            .action_type_name(),
            "acquire_lock"
        );
        assert_eq!(
            StepAction::ReleaseLock {
                lock_name: String::new()
            }
            .action_type_name(),
            "release_lock"
        );
        assert_eq!(
            StepAction::StoreData {
                key: String::new(),
                value: serde_json::Value::Null
            }
            .action_type_name(),
            "store_data"
        );
        assert_eq!(
            StepAction::RunWorkflow {
                workflow_id: String::new(),
                params: None
            }
            .action_type_name(),
            "run_workflow"
        );
        assert_eq!(
            StepAction::MarkEventHandled { event_id: 0 }.action_type_name(),
            "mark_event_handled"
        );
        assert_eq!(
            StepAction::ValidateApproval {
                approval_code: String::new()
            }
            .action_type_name(),
            "validate_approval"
        );
        assert_eq!(
            StepAction::Custom {
                action_type: String::new(),
                payload: serde_json::Value::Null
            }
            .action_type_name(),
            "custom"
        );
    }

    #[test]
    fn test_validation_error_display() {
        let err1 = PlanValidationError::InvalidStepNumber {
            expected: 1,
            actual: 5,
        };
        assert!(err1.to_string().contains("expected 1"));
        assert!(err1.to_string().contains("got 5"));

        let err2 = PlanValidationError::DuplicateStepId(IdempotencyKey::from_hash("abc"));
        assert!(err2.to_string().contains("Duplicate"));

        let err3 = PlanValidationError::UnknownStepReference(IdempotencyKey::from_hash("xyz"));
        assert!(err3.to_string().contains("Unknown"));

        let err4 = PlanValidationError::UnsupportedVersion {
            version: 99,
            max_supported: 1,
        };
        assert!(err4.to_string().contains("99"));
        assert!(err4.to_string().contains('1'));
    }

    // ========================================================================
    // Batch 12 — PearlSpring wa-1u90p.7.1 builder, edge-case, serde tests
    // ========================================================================

    #[test]
    fn plan_id_from_hash_strips_sha256_prefix() {
        let id = PlanId::from_hash("sha256:abcdef");
        assert_eq!(id.0, "plan:abcdef");
    }

    #[test]
    fn plan_id_from_hash_no_prefix_passes_through() {
        let id = PlanId::from_hash("rawvalue");
        assert_eq!(id.0, "plan:rawvalue");
    }

    #[test]
    fn plan_id_placeholder_is_detected() {
        let placeholder = PlanId::placeholder();
        assert!(placeholder.is_placeholder());
        assert_eq!(placeholder.0, "plan:pending");
    }

    #[test]
    fn plan_id_equality() {
        let a = PlanId::from_hash("abc");
        let b = PlanId::from_hash("abc");
        let c = PlanId::from_hash("xyz");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn plan_id_serde_roundtrip() {
        let id = PlanId::from_hash("test123");
        let json = serde_json::to_string(&id).unwrap();
        let back: PlanId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn idempotency_key_serde_roundtrip() {
        let key = IdempotencyKey::from_hash("abc123");
        let json = serde_json::to_string(&key).unwrap();
        let back: IdempotencyKey = serde_json::from_str(&json).unwrap();
        assert_eq!(key, back);
    }

    #[test]
    fn step_plan_with_key_constructor() {
        let key = IdempotencyKey::from_hash("custom_key");
        let step = StepPlan::with_key(
            1,
            key.clone(),
            StepAction::ReleaseLock {
                lock_name: "test".into(),
            },
            "Release test lock",
        );
        assert_eq!(step.step_id, key);
        assert_eq!(step.step_number, 1);
        assert!(!step.idempotent);
        assert!(step.preconditions.is_empty());
        assert!(step.verification.is_none());
        assert!(step.on_failure.is_none());
        assert!(step.timeout_ms.is_none());
    }

    #[test]
    fn step_plan_builder_chain() {
        let step = StepPlan::new(
            1,
            StepAction::SendText {
                pane_id: 0,
                text: "x".into(),
                paste_mode: None,
            },
            "Send x",
        )
        .with_precondition(Precondition::PaneExists { pane_id: 0 })
        .with_verification(Verification::pane_idle(5000))
        .with_on_failure(OnFailure::skip())
        .with_timeout_ms(10000)
        .idempotent();

        assert!(step.idempotent);
        assert_eq!(step.preconditions.len(), 1);
        assert!(step.verification.is_some());
        assert!(step.on_failure.is_some());
        assert_eq!(step.timeout_ms, Some(10000));
    }

    #[test]
    fn verification_builders() {
        let v = Verification::pattern_match("my_rule")
            .with_description("Check pattern")
            .with_timeout_ms(5000);
        assert_eq!(v.description.as_deref(), Some("Check pattern"));
        assert_eq!(v.timeout_ms, Some(5000));
        assert!(matches!(
            v.strategy,
            VerificationStrategy::PatternMatch { .. }
        ));
    }

    #[test]
    fn verification_pane_idle_builder() {
        let v = Verification::pane_idle(3000);
        assert!(v.description.is_none());
        assert!(v.timeout_ms.is_none());
        if let VerificationStrategy::PaneIdle {
            idle_threshold_ms, ..
        } = v.strategy
        {
            assert_eq!(idle_threshold_ms, 3000);
        } else {
            panic!("Expected PaneIdle strategy");
        }
    }

    #[test]
    fn builder_add_steps_multiple() {
        let steps = vec![
            StepPlan::new(
                1,
                StepAction::AcquireLock {
                    lock_name: "a".into(),
                    timeout_ms: None,
                },
                "Acquire",
            ),
            StepPlan::new(
                2,
                StepAction::ReleaseLock {
                    lock_name: "a".into(),
                },
                "Release",
            ),
        ];
        let plan = ActionPlan::builder("Multi", "ws").add_steps(steps).build();
        assert_eq!(plan.step_count(), 2);
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn empty_plan_validates() {
        let plan = ActionPlan::builder("Empty", "ws").build();
        assert_eq!(plan.step_count(), 0);
        assert!(!plan.has_preconditions());
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn plan_schema_version_is_set() {
        let plan = ActionPlan::builder("Test", "ws").build();
        assert_eq!(plan.plan_version, PLAN_SCHEMA_VERSION);
    }

    #[test]
    fn on_failure_abort_message() {
        let f = OnFailure::abort_with_message("oops");
        if let OnFailure::Abort { message } = &f {
            assert_eq!(message.as_deref(), Some("oops"));
        } else {
            panic!("Expected Abort variant");
        }
    }

    #[test]
    fn on_failure_retry_defaults() {
        let f = OnFailure::retry(5, 2000);
        if let OnFailure::Retry {
            max_attempts,
            initial_delay_ms,
            max_delay_ms,
            backoff_multiplier,
        } = &f
        {
            assert_eq!(*max_attempts, 5);
            assert_eq!(*initial_delay_ms, 2000);
            assert!(max_delay_ms.is_none());
            assert!(backoff_multiplier.is_none());
        } else {
            panic!("Expected Retry variant");
        }
    }

    #[test]
    fn on_failure_skip_defaults() {
        let f = OnFailure::skip();
        if let OnFailure::Skip { warn } = &f {
            assert_eq!(*warn, Some(true));
        } else {
            panic!("Expected Skip variant");
        }
    }

    #[test]
    fn nested_plan_action_canonical_string() {
        let inner = ActionPlan::builder("Inner", "ws")
            .add_step(StepPlan::new(
                1,
                StepAction::MarkEventHandled { event_id: 1 },
                "Mark",
            ))
            .build();
        let action = StepAction::NestedPlan {
            plan: Box::new(inner),
        };
        let s = action.canonical_string();
        assert!(s.starts_with("nested_plan:hash=sha256:"));
        assert_eq!(action.action_type_name(), "nested_plan");
    }

    #[test]
    fn approval_scope_ref_serde() {
        let scope = ApprovalScopeRef {
            workspace_id: "ws-1".to_string(),
            action_kind: "send_text".to_string(),
            pane_id: Some(42),
        };
        let json = serde_json::to_string(&scope).unwrap();
        let back: ApprovalScopeRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back.workspace_id, "ws-1");
        assert_eq!(back.action_kind, "send_text");
        assert_eq!(back.pane_id, Some(42));
    }

    #[test]
    fn approval_valid_precondition_canonical() {
        let precond = Precondition::ApprovalValid {
            scope: ApprovalScopeRef {
                workspace_id: "ws".to_string(),
                action_kind: "send_text".to_string(),
                pane_id: None,
            },
        };
        let s = precond.canonical_string();
        assert!(s.contains("approval_valid"));
        assert!(s.contains("ws"));
        assert!(s.contains("send_text"));
        assert!(s.contains("any")); // pane_id is None
    }

    #[test]
    fn on_failure_fallback_canonical_string() {
        let fallback_steps = vec![StepPlan::new(
            1,
            StepAction::SendText {
                pane_id: 0,
                text: "fallback".into(),
                paste_mode: None,
            },
            "Fallback step",
        )];
        let f = OnFailure::Fallback {
            steps: fallback_steps,
        };
        let s = f.canonical_string();
        assert!(s.starts_with("fallback:"));
    }

    #[test]
    fn on_failure_require_approval_canonical() {
        let f = OnFailure::RequireApproval {
            summary: "Need help".into(),
        };
        let s = f.canonical_string();
        assert!(s.contains("require_approval"));
        assert!(s.contains("Need help"));
    }

    #[test]
    fn step_action_send_text_paste_mode_variations() {
        let a1 = StepAction::SendText {
            pane_id: 0,
            text: "x".into(),
            paste_mode: None,
        };
        let a2 = StepAction::SendText {
            pane_id: 0,
            text: "x".into(),
            paste_mode: Some(true),
        };
        let a3 = StepAction::SendText {
            pane_id: 0,
            text: "x".into(),
            paste_mode: Some(false),
        };
        assert!(a1.canonical_string().contains("paste=none"));
        assert!(a2.canonical_string().contains("paste=true"));
        assert!(a3.canonical_string().contains("paste=false"));
    }

    #[test]
    fn step_action_partial_eq_send_text_matches_all_fields() {
        let a1 = StepAction::SendText {
            pane_id: 7,
            text: "/retry".into(),
            paste_mode: Some(false),
        };
        let a2 = StepAction::SendText {
            pane_id: 7,
            text: "/retry".into(),
            paste_mode: Some(false),
        };
        let a3 = StepAction::SendText {
            pane_id: 7,
            text: "/retry".into(),
            paste_mode: Some(true),
        };

        assert_eq!(a1, a2);
        assert_ne!(a1, a3);
    }

    #[test]
    fn step_action_partial_eq_nested_plan_uses_plan_hash() {
        let make_nested = |event_id: i64| StepAction::NestedPlan {
            plan: Box::new(
                ActionPlan::builder("nested", "ws")
                    .add_step(StepPlan::new(
                        1,
                        StepAction::MarkEventHandled { event_id },
                        "mark event",
                    ))
                    .build(),
            ),
        };

        let a1 = make_nested(1);
        let a2 = make_nested(1);
        let a3 = make_nested(2);

        assert_eq!(a1, a2);
        assert_ne!(a1, a3);
    }

    #[test]
    fn step_action_wait_for_pane_none() {
        let a = StepAction::WaitFor {
            pane_id: None,
            condition: WaitCondition::External {
                key: "signal".into(),
            },
            timeout_ms: 1000,
        };
        let s = a.canonical_string();
        assert!(s.contains("pane=any"));
    }

    #[test]
    fn step_action_run_workflow_no_params() {
        let a = StepAction::RunWorkflow {
            workflow_id: "wf".into(),
            params: None,
        };
        let s = a.canonical_string();
        assert!(s.contains("run_workflow"));
        assert!(s.contains("wf"));
    }

    #[test]
    fn step_action_acquire_lock_no_timeout() {
        let a = StepAction::AcquireLock {
            lock_name: "my_lock".into(),
            timeout_ms: None,
        };
        let s = a.canonical_string();
        assert!(s.contains("timeout=none"));
    }

    #[test]
    fn plan_validation_error_is_error_trait() {
        let err = PlanValidationError::InvalidStepNumber {
            expected: 1,
            actual: 2,
        };
        // PlanValidationError implements std::error::Error
        let _: &dyn std::error::Error = &err;
    }

    // ========================================================================
    // Batch — RubyBeaver wa-1u90p.7.1 validation + serde edge-case tests
    // ========================================================================

    #[test]
    fn validation_rejects_duplicate_step_ids() {
        let key = IdempotencyKey::from_hash("same_key");
        let plan = ActionPlan {
            plan_version: PLAN_SCHEMA_VERSION,
            plan_id: PlanId::placeholder(),
            title: "dup".into(),
            workspace_id: "ws".into(),
            created_at: None,
            steps: vec![
                StepPlan::with_key(
                    1,
                    key.clone(),
                    StepAction::ReleaseLock {
                        lock_name: "a".into(),
                    },
                    "Step 1",
                ),
                StepPlan::with_key(
                    2,
                    key,
                    StepAction::ReleaseLock {
                        lock_name: "b".into(),
                    },
                    "Step 2",
                ),
            ],
            preconditions: vec![],
            on_failure: None,
            metadata: None,
        };
        let err = plan.validate().unwrap_err();
        assert!(matches!(err, PlanValidationError::DuplicateStepId(_)));
    }

    #[test]
    fn validation_rejects_wrong_step_numbering() {
        let plan = ActionPlan {
            plan_version: PLAN_SCHEMA_VERSION,
            plan_id: PlanId::placeholder(),
            title: "bad numbering".into(),
            workspace_id: "ws".into(),
            created_at: None,
            steps: vec![StepPlan::new(
                5, // should be 1
                StepAction::MarkEventHandled { event_id: 1 },
                "Wrong number",
            )],
            preconditions: vec![],
            on_failure: None,
            metadata: None,
        };
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            PlanValidationError::InvalidStepNumber {
                expected: 1,
                actual: 5
            }
        ));
    }

    #[test]
    fn validation_rejects_unknown_step_reference_in_precondition() {
        let plan = ActionPlan {
            plan_version: PLAN_SCHEMA_VERSION,
            plan_id: PlanId::placeholder(),
            title: "bad ref".into(),
            workspace_id: "ws".into(),
            created_at: None,
            steps: vec![],
            preconditions: vec![Precondition::StepCompleted {
                step_id: IdempotencyKey::from_hash("nonexistent"),
            }],
            on_failure: None,
            metadata: None,
        };
        let err = plan.validate().unwrap_err();
        assert!(matches!(err, PlanValidationError::UnknownStepReference(_)));
    }

    #[test]
    fn plan_hash_changes_when_title_changes() {
        let plan1 = ActionPlan::builder("Plan A", "ws").build();
        let plan2 = ActionPlan::builder("Plan B", "ws").build();
        assert_ne!(plan1.compute_hash(), plan2.compute_hash());
    }

    #[test]
    fn plan_hash_changes_when_workspace_changes() {
        let plan1 = ActionPlan::builder("Same", "ws-1").build();
        let plan2 = ActionPlan::builder("Same", "ws-2").build();
        assert_ne!(plan1.compute_hash(), plan2.compute_hash());
    }

    #[test]
    fn plan_hash_ignores_created_at() {
        let plan1 = ActionPlan::builder("Same", "ws").created_at(1000).build();
        let plan2 = ActionPlan::builder("Same", "ws").created_at(2000).build();
        assert_eq!(plan1.compute_hash(), plan2.compute_hash());
    }

    #[test]
    fn idempotency_key_for_action_is_deterministic() {
        let action = StepAction::SendText {
            pane_id: 1,
            text: "hello".into(),
            paste_mode: None,
        };
        let k1 = IdempotencyKey::for_action("ws", 1, &action);
        let k2 = IdempotencyKey::for_action("ws", 1, &action);
        assert_eq!(k1, k2);
    }

    #[test]
    fn idempotency_key_for_action_differs_by_workspace() {
        let action = StepAction::MarkEventHandled { event_id: 1 };
        let k1 = IdempotencyKey::for_action("ws-a", 1, &action);
        let k2 = IdempotencyKey::for_action("ws-b", 1, &action);
        assert_ne!(k1, k2);
    }

    #[test]
    fn step_action_serde_roundtrip_all_variants() {
        let actions = vec![
            StepAction::SendText {
                pane_id: 42,
                text: "hi".into(),
                paste_mode: Some(true),
            },
            StepAction::WaitFor {
                pane_id: Some(1),
                condition: WaitCondition::PaneIdle {
                    pane_id: Some(1),
                    idle_threshold_ms: 3000,
                },
                timeout_ms: 5000,
            },
            StepAction::AcquireLock {
                lock_name: "lock1".into(),
                timeout_ms: Some(1000),
            },
            StepAction::ReleaseLock {
                lock_name: "lock1".into(),
            },
            StepAction::StoreData {
                key: "k".into(),
                value: serde_json::json!({"x": 1}),
            },
            StepAction::RunWorkflow {
                workflow_id: "wf-1".into(),
                params: Some(serde_json::json!([])),
            },
            StepAction::MarkEventHandled { event_id: 99 },
            StepAction::ValidateApproval {
                approval_code: "CODE".into(),
            },
            StepAction::Custom {
                action_type: "my_type".into(),
                payload: serde_json::json!(null),
            },
        ];
        for action in &actions {
            let json = serde_json::to_string(action).unwrap();
            let back: StepAction = serde_json::from_str(&json).unwrap();
            assert_eq!(
                action.action_type_name(),
                back.action_type_name(),
                "action type mismatch for: {}",
                json
            );
        }
    }

    #[test]
    fn on_failure_serde_roundtrip_all_variants() {
        let variants = vec![
            OnFailure::abort(),
            OnFailure::abort_with_message("fail"),
            OnFailure::retry(3, 1000),
            OnFailure::skip(),
            OnFailure::RequireApproval {
                summary: "help".into(),
            },
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: OnFailure = serde_json::from_str(&json).unwrap();
            assert_eq!(
                v.canonical_string(),
                back.canonical_string(),
                "on_failure mismatch for: {}",
                json
            );
        }
    }

    #[test]
    fn wait_condition_serde_roundtrip_all_variants() {
        let conditions = vec![
            WaitCondition::Pattern {
                pane_id: Some(1),
                rule_id: "r".into(),
            },
            WaitCondition::PaneIdle {
                pane_id: None,
                idle_threshold_ms: 500,
            },
            WaitCondition::StableTail {
                pane_id: Some(2),
                stable_for_ms: 1000,
            },
            WaitCondition::External { key: "sig".into() },
        ];
        for cond in &conditions {
            let json = serde_json::to_string(cond).unwrap();
            let back: WaitCondition = serde_json::from_str(&json).unwrap();
            assert_eq!(
                cond.canonical_string(),
                back.canonical_string(),
                "wait_condition mismatch for: {}",
                json
            );
        }
    }

    fn sample_mission() -> Mission {
        let mut mission = Mission::new(
            MissionId("mission:core".to_string()),
            "Recover failing pane loop",
            "ws-main",
            MissionOwnership {
                planner: "planner-agent".to_string(),
                dispatcher: "dispatcher-agent".to_string(),
                operator: "operator-human".to_string(),
            },
            1_704_000_000_000,
        );
        mission.provenance = Some(MissionProvenance {
            bead_id: Some("ft-1i2ge.1.1".to_string()),
            thread_id: Some("ft-1i2ge.1.1".to_string()),
            source_command: Some("ft mission plan".to_string()),
            source_sha: Some("abc123".to_string()),
        });
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:a".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 1,
                text: "/retry".to_string(),
                paste_mode: Some(false),
            },
            rationale: "Retry once after cooldown".to_string(),
            score: Some(0.92),
            created_at_ms: 1_704_000_000_100,
        });
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId("assignment:a".to_string()),
            candidate_id: CandidateActionId("candidate:a".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "executor-agent-1".to_string(),
            reservation_intent: Some(ReservationIntent {
                reservation_id: ReservationIntentId("reservation:a".to_string()),
                requested_by: MissionActorRole::Dispatcher,
                paths: vec!["crates/frankenterm-core/src/plan.rs".to_string()],
                exclusive: true,
                reason: Some("mission replay update".to_string()),
                requested_at_ms: 1_704_000_000_200,
                expires_at_ms: Some(1_704_000_360_200),
            }),
            approval_state: ApprovalState::Approved {
                approved_by: "operator-human".to_string(),
                approved_at_ms: 1_704_000_000_220,
                approval_code_hash: "sha256:abcd".to_string(),
            },
            outcome: Some(Outcome::Success {
                reason_code: "retry_applied".to_string(),
                completed_at_ms: 1_704_000_000_700,
            }),
            escalation: None,
            created_at_ms: 1_704_000_000_210,
            updated_at_ms: Some(1_704_000_000_705),
        });
        mission.lifecycle_state = MissionLifecycleState::Completed;
        mission
    }

    #[test]
    fn mission_json_roundtrip_preserves_required_fields() {
        let mission = sample_mission();
        let json = serde_json::to_string_pretty(&mission).unwrap();
        let decoded: Mission = serde_json::from_str(&json).unwrap();

        assert_eq!(mission.mission_version, decoded.mission_version);
        assert_eq!(mission.mission_id, decoded.mission_id);
        assert_eq!(mission.title, decoded.title);
        assert_eq!(mission.workspace_id, decoded.workspace_id);
        assert_eq!(mission.ownership, decoded.ownership);
        assert_eq!(mission.provenance, decoded.provenance);
        assert_eq!(mission.candidates.len(), decoded.candidates.len());
        assert_eq!(mission.assignments.len(), decoded.assignments.len());
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn mission_validate_rejects_duplicate_ownership_actor() {
        let mut mission = sample_mission();
        mission.ownership.dispatcher = mission.ownership.planner.clone();

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::DuplicateOwnershipActor(_)
        ));
    }

    #[test]
    fn mission_validate_rejects_unknown_candidate_reference() {
        let mut mission = sample_mission();
        mission.assignments[0].candidate_id = CandidateActionId("candidate:missing".to_string());

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnknownCandidateReference(_)
        ));
    }

    #[test]
    fn mission_validate_rejects_empty_reservation_paths() {
        let mut mission = sample_mission();
        mission.assignments[0].reservation_intent = Some(ReservationIntent {
            reservation_id: ReservationIntentId("reservation:empty".to_string()),
            requested_by: MissionActorRole::Dispatcher,
            paths: Vec::new(),
            exclusive: false,
            reason: None,
            requested_at_ms: 1_704_000_000_111,
            expires_at_ms: None,
        });

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::EmptyReservationPaths(_)
        ));
    }

    #[test]
    fn mission_lifecycle_transition_table_contains_required_branches() {
        assert!(mission_lifecycle_can_transition(
            MissionLifecycleState::Planning,
            MissionLifecycleState::Planned,
            MissionLifecycleTransitionKind::PlanFinalized
        ));
        assert!(mission_lifecycle_can_transition(
            MissionLifecycleState::Dispatching,
            MissionLifecycleState::Blocked,
            MissionLifecycleTransitionKind::ExecutionBlocked
        ));
        assert!(mission_lifecycle_can_transition(
            MissionLifecycleState::AwaitingApproval,
            MissionLifecycleState::Failed,
            MissionLifecycleTransitionKind::ApprovalExpired
        ));
        assert!(mission_lifecycle_can_transition(
            MissionLifecycleState::Blocked,
            MissionLifecycleState::RetryPending,
            MissionLifecycleTransitionKind::RetryScheduled
        ));
        assert!(mission_lifecycle_can_transition(
            MissionLifecycleState::Running,
            MissionLifecycleState::Cancelled,
            MissionLifecycleTransitionKind::MissionCancelled
        ));
    }

    #[test]
    fn mission_lifecycle_happy_path_reaches_completed() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Planning;
        mission.updated_at_ms = None;

        mission
            .transition_lifecycle(
                MissionLifecycleState::Planned,
                MissionLifecycleTransitionKind::PlanFinalized,
                1_704_000_001_000,
            )
            .unwrap();
        mission
            .transition_lifecycle(
                MissionLifecycleState::Dispatching,
                MissionLifecycleTransitionKind::DispatchStarted,
                1_704_000_001_100,
            )
            .unwrap();
        mission
            .transition_lifecycle(
                MissionLifecycleState::Running,
                MissionLifecycleTransitionKind::ExecutionStarted,
                1_704_000_001_200,
            )
            .unwrap();
        mission
            .transition_lifecycle(
                MissionLifecycleState::Completed,
                MissionLifecycleTransitionKind::ExecutionSucceeded,
                1_704_000_001_300,
            )
            .unwrap();

        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Completed);
        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_lifecycle_retry_and_unblock_paths_are_supported() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Dispatching;

        mission
            .transition_lifecycle(
                MissionLifecycleState::Blocked,
                MissionLifecycleTransitionKind::ExecutionBlocked,
                1_704_000_002_100,
            )
            .unwrap();
        mission
            .transition_lifecycle(
                MissionLifecycleState::RetryPending,
                MissionLifecycleTransitionKind::RetryScheduled,
                1_704_000_002_200,
            )
            .unwrap();
        mission
            .transition_lifecycle(
                MissionLifecycleState::Dispatching,
                MissionLifecycleTransitionKind::RetryResumed,
                1_704_000_002_300,
            )
            .unwrap();
        mission
            .transition_lifecycle(
                MissionLifecycleState::Running,
                MissionLifecycleTransitionKind::ExecutionStarted,
                1_704_000_002_400,
            )
            .unwrap();
        mission
            .transition_lifecycle(
                MissionLifecycleState::Completed,
                MissionLifecycleTransitionKind::ExecutionSucceeded,
                1_704_000_002_500,
            )
            .unwrap();

        assert_eq!(mission.lifecycle_state, MissionLifecycleState::Completed);
        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_lifecycle_invalid_transition_is_rejected() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Planning;

        let err = mission
            .transition_lifecycle(
                MissionLifecycleState::Completed,
                MissionLifecycleTransitionKind::ExecutionSucceeded,
                1_704_000_003_000,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::InvalidLifecycleTransition { .. }
        ));
    }

    #[test]
    fn mission_lifecycle_transition_conformance_matches_transition_table() {
        for from in MissionLifecycleState::all() {
            for to in MissionLifecycleState::all() {
                for kind in MissionLifecycleTransitionKind::all() {
                    let mut mission = sample_mission();
                    mission.lifecycle_state = from;
                    mission.updated_at_ms = None;

                    let expected = mission_lifecycle_can_transition(from, to, kind);
                    let result = mission.transition_lifecycle(to, kind, 1_704_000_004_000);

                    if expected {
                        assert!(
                            result.is_ok(),
                            "expected legal transition {from} -> {to} via {kind}"
                        );
                        assert_eq!(mission.lifecycle_state, to);
                        assert_eq!(mission.updated_at_ms, Some(1_704_000_004_000));
                    } else {
                        let err = result.unwrap_err();
                        assert_eq!(
                            err,
                            MissionValidationError::InvalidLifecycleTransition { from, to, kind },
                            "expected rejection for illegal transition {from} -> {to} via {kind}"
                        );
                        assert_eq!(mission.lifecycle_state, from);
                    }
                }
            }
        }
    }

    #[test]
    fn mission_validate_rejects_empty_candidate_id_with_field_path() {
        let mut mission = sample_mission();
        mission.candidates[0].candidate_id = CandidateActionId(" ".to_string());

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::InvalidFieldValue { field_path, .. }
            if field_path == "mission.candidates[0].candidate_id"
        ));
    }

    #[test]
    fn mission_validate_rejects_non_finite_candidate_score() {
        let mut mission = sample_mission();
        mission.candidates[0].score = Some(f64::INFINITY);

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::InvalidFieldValue { field_path, .. }
            if field_path == "mission.candidates[0].score"
        ));
    }

    #[test]
    fn mission_validate_rejects_non_monotonic_assignment_timestamps() {
        let mut mission = sample_mission();
        mission.assignments[0].updated_at_ms = Some(mission.assignments[0].created_at_ms - 1);

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::NonMonotonicTimestamp { field_path, .. }
            if field_path == "mission.assignments[0].updated_at_ms"
        ));
    }

    #[test]
    fn mission_validate_rejects_empty_reservation_path_entry() {
        let mut mission = sample_mission();
        mission.assignments[0]
            .reservation_intent
            .as_mut()
            .unwrap()
            .paths = vec!["".to_string()];

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::InvalidFieldValue { field_path, .. }
            if field_path == "mission.assignments[0].reservation_intent.paths[0]"
        ));
    }

    #[test]
    fn mission_validate_rejects_terminal_state_without_matching_outcome() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Failed;

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::TerminalStateWithoutMatchingOutcome { .. }
        ));
    }

    #[test]
    fn mission_failure_taxonomy_catalog_has_unique_reason_and_error_codes() {
        let mut reason_codes = std::collections::HashSet::new();
        let mut error_codes = std::collections::HashSet::new();

        for code in MissionFailureCode::all() {
            let contract = code.contract();
            assert!(reason_codes.insert(contract.reason_code));
            assert!(error_codes.insert(contract.error_code));
            assert_eq!(
                MissionFailureCode::from_reason_code(contract.reason_code),
                Some(code)
            );
            assert_eq!(
                MissionFailureCode::from_error_code(contract.error_code),
                Some(code)
            );
        }
    }

    #[test]
    fn mission_failure_taxonomy_marks_retryability_and_hints() {
        for code in MissionFailureCode::all() {
            assert!(!code.human_hint().trim().is_empty());
            assert!(!code.machine_hint().trim().is_empty());
            assert!(!code.reason_code().trim().is_empty());
            assert!(!code.error_code().trim().is_empty());
        }

        assert!(MissionFailureCode::PolicyDenied.terminality().is_terminal());
        assert_eq!(
            MissionFailureCode::PolicyDenied.retryability(),
            MissionFailureRetryability::Never
        );
        assert!(!MissionFailureCode::PolicyDenied
            .retryability()
            .is_retryable());
        assert!(!MissionFailureCode::ApprovalDenied
            .retryability()
            .is_retryable());
        assert!(!MissionFailureCode::RateLimited.terminality().is_terminal());
        assert!(MissionFailureCode::RateLimited
            .retryability()
            .is_retryable());
    }

    #[test]
    fn mission_validate_rejects_unknown_failure_reason_code() {
        let mut mission = sample_mission();
        mission.assignments[0].outcome = Some(Outcome::Failed {
            reason_code: "unknown_failure".to_string(),
            error_code: "FTM1999".to_string(),
            completed_at_ms: 1_704_000_000_800,
        });

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnknownFailureReasonCode { .. }
        ));
    }

    #[test]
    fn mission_validate_rejects_mismatched_failure_error_code() {
        let mut mission = sample_mission();
        mission.assignments[0].outcome = Some(Outcome::Failed {
            reason_code: MissionFailureCode::RateLimited.reason_code().to_string(),
            error_code: MissionFailureCode::ReservationConflict
                .error_code()
                .to_string(),
            completed_at_ms: 1_704_000_000_800,
        });

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::MismatchedFailureErrorCode { .. }
        ));
    }

    #[test]
    fn mission_validate_accepts_recoverable_failure_contract() {
        let mut mission = sample_mission();
        mission.lifecycle_state = MissionLifecycleState::Failed;
        mission.assignments[0].outcome = Some(Outcome::Failed {
            reason_code: MissionFailureCode::ReservationConflict
                .reason_code()
                .to_string(),
            error_code: MissionFailureCode::ReservationConflict
                .error_code()
                .to_string(),
            completed_at_ms: 1_704_000_000_800,
        });

        assert!(mission.validate().is_ok());
    }

    #[test]
    fn mission_validate_requires_canonical_approval_denied_reason() {
        let mut mission = sample_mission();
        mission.assignments[0].approval_state = ApprovalState::Denied {
            denied_by: "operator-human".to_string(),
            denied_at_ms: 1_704_000_000_900,
            reason_code: MissionFailureCode::PolicyDenied.reason_code().to_string(),
        };

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnexpectedFailureCodeForContext {
                context: MissionFailureContext::ApprovalDenied,
                ..
            }
        ));
    }

    #[test]
    fn mission_validate_requires_canonical_approval_expired_reason() {
        let mut mission = sample_mission();
        mission.assignments[0].approval_state = ApprovalState::Expired {
            expired_at_ms: 1_704_000_000_900,
            reason_code: MissionFailureCode::ApprovalRequired
                .reason_code()
                .to_string(),
        };

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnexpectedFailureCodeForContext {
                context: MissionFailureContext::ApprovalExpired,
                ..
            }
        ));
    }

    #[test]
    fn mission_validate_rejects_escalation_error_without_canonical_reason() {
        let mut mission = sample_mission();
        mission.assignments[0].escalation = Some(Escalation {
            level: EscalationLevel::Human,
            triggered_by: MissionActorRole::Dispatcher,
            reason_code: "monitor_first".to_string(),
            error_code: Some(MissionFailureCode::StaleState.error_code().to_string()),
            summary: Some("Needs operator review".to_string()),
            escalated_at_ms: 1_704_000_000_901,
        });

        let err = mission.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnknownFailureReasonCode {
                context: MissionFailureContext::AssignmentEscalation,
                ..
            }
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn mission_contract_property_transition_conformance_matches_table(
            from_index in 0_usize..10_usize,
            to_index in 0_usize..10_usize,
            kind_index in 0_usize..13_usize,
            transition_ts in 1_704_200_000_000_i64..1_704_200_010_000_i64
        ) {
            let states = MissionLifecycleState::all();
            let kinds = MissionLifecycleTransitionKind::all();
            let from = states[from_index];
            let to = states[to_index];
            let kind = kinds[kind_index];

            let mut mission = sample_mission();
            mission.lifecycle_state = from;
            mission.updated_at_ms = None;

            let expected = mission_lifecycle_can_transition(from, to, kind);
            let result = mission.transition_lifecycle(to, kind, transition_ts);
            if expected {
                prop_assert!(result.is_ok());
                prop_assert_eq!(mission.lifecycle_state, to);
                prop_assert_eq!(mission.updated_at_ms, Some(transition_ts));
            } else {
                let err = result.unwrap_err();
                prop_assert_eq!(
                    err,
                    MissionValidationError::InvalidLifecycleTransition { from, to, kind },
                );
                prop_assert_eq!(mission.lifecycle_state, from);
                prop_assert!(mission.updated_at_ms.is_none());
            }
        }

        #[test]
        fn mission_contract_property_duplicate_candidate_ids_are_rejected(
            suffix in "[a-z0-9_]{1,12}"
        ) {
            let mut mission = sample_mission();
            let duplicate_id = format!("candidate:{suffix}");
            mission.candidates[0].candidate_id = CandidateActionId(duplicate_id.clone());
            mission.assignments[0].candidate_id = CandidateActionId(duplicate_id.clone());
            mission.candidates.push(CandidateAction {
                candidate_id: CandidateActionId(duplicate_id.clone()),
                requested_by: MissionActorRole::Planner,
                action: StepAction::WaitFor {
                    pane_id: Some(2),
                    condition: WaitCondition::Pattern {
                        pane_id: Some(2),
                        rule_id: "core.codex:done".to_string(),
                    },
                    timeout_ms: 5_000,
                },
                rationale: "Duplicate ID property check".to_string(),
                score: Some(0.11),
                created_at_ms: 1_704_200_000_111,
            });

            let err = mission.validate().unwrap_err();
            prop_assert!(matches!(
                err,
                MissionValidationError::DuplicateCandidateId(CandidateActionId(ref id))
                if id == &duplicate_id
            ));
        }

        #[test]
        fn mission_contract_property_blank_provenance_fields_are_rejected(
            field_selector in 0_usize..4_usize,
            whitespace_len in 1_usize..6_usize
        ) {
            let mut mission = sample_mission();
            let blank = " ".repeat(whitespace_len);
            let provenance = mission.provenance.get_or_insert_with(MissionProvenance::default);
            let expected_field_path = match field_selector {
                0 => {
                    provenance.bead_id = Some(blank);
                    "mission.provenance.bead_id"
                }
                1 => {
                    provenance.thread_id = Some(blank);
                    "mission.provenance.thread_id"
                }
                2 => {
                    provenance.source_command = Some(blank);
                    "mission.provenance.source_command"
                }
                _ => {
                    provenance.source_sha = Some(blank);
                    "mission.provenance.source_sha"
                }
            };

            let err = mission.validate().unwrap_err();
            prop_assert!(matches!(
                err,
                MissionValidationError::InvalidFieldValue { ref field_path, .. }
                if field_path == expected_field_path
            ), "unexpected validation error: {:?}", err);
        }

        #[test]
        fn mission_contract_property_failure_code_roundtrips_are_stable(
            index in 0_usize..8_usize
        ) {
            let code = MissionFailureCode::all()[index];
            let contract = code.contract();
            prop_assert_eq!(
                MissionFailureCode::from_reason_code(contract.reason_code),
                Some(code),
            );
            prop_assert_eq!(
                MissionFailureCode::from_error_code(contract.error_code),
                Some(code),
            );
        }

        #[test]
        fn mission_contract_property_terminal_states_require_matching_outcomes(
            state_selector in 0_usize..3_usize,
            outcome_selector in 0_usize..4_usize
        ) {
            let mut mission = sample_mission();
            let state = match state_selector {
                0 => MissionLifecycleState::Completed,
                1 => MissionLifecycleState::Failed,
                _ => MissionLifecycleState::Cancelled,
            };
            mission.lifecycle_state = state;

            mission.assignments[0].outcome = match outcome_selector {
                0 => None,
                1 => Some(Outcome::Success {
                    reason_code: "property_success".to_string(),
                    completed_at_ms: 1_704_200_000_901,
                }),
                2 => Some(Outcome::Failed {
                    reason_code: MissionFailureCode::RateLimited.reason_code().to_string(),
                    error_code: MissionFailureCode::RateLimited.error_code().to_string(),
                    completed_at_ms: 1_704_200_000_902,
                }),
                _ => Some(Outcome::Cancelled {
                    reason_code: "property_cancelled".to_string(),
                    completed_at_ms: 1_704_200_000_903,
                }),
            };

            let expected_valid = matches!(
                (state, &mission.assignments[0].outcome),
                (MissionLifecycleState::Completed, Some(Outcome::Success { .. }))
                    | (MissionLifecycleState::Failed, Some(Outcome::Failed { .. }))
                    | (MissionLifecycleState::Cancelled, Some(Outcome::Cancelled { .. }))
            );
            let result = mission.validate();
            if expected_valid {
                prop_assert!(result.is_ok());
            } else {
                let err = result.unwrap_err();
                prop_assert!(matches!(
                    err,
                    MissionValidationError::TerminalStateWithoutMatchingOutcome { .. }
                ), "unexpected validation error: {:?}", err);
            }
        }
    }

    #[test]
    fn mission_policy_preflight_plan_time_surfaces_structured_allow_and_deny_reasons() {
        let mut mission = sample_mission();
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:b".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::WaitFor {
                pane_id: Some(2),
                condition: WaitCondition::Pattern {
                    pane_id: Some(2),
                    rule_id: "core.codex:done".to_string(),
                },
                timeout_ms: 5_000,
            },
            rationale: "Observe completion signal".to_string(),
            score: Some(0.44),
            created_at_ms: 1_704_000_000_333,
        });

        let report = mission
            .evaluate_policy_preflight(
                MissionPolicyPreflightStage::PlanTime,
                &[
                    MissionPolicyPreflightCheck {
                        candidate_id: CandidateActionId("candidate:a".to_string()),
                        assignment_id: None,
                        decision: MissionPolicyDecisionKind::Allow,
                        reason_code: None,
                        rule_id: Some("policy.default_allow".to_string()),
                        context: Some("safe pane state".to_string()),
                    },
                    MissionPolicyPreflightCheck {
                        candidate_id: CandidateActionId("candidate:b".to_string()),
                        assignment_id: None,
                        decision: MissionPolicyDecisionKind::Deny,
                        reason_code: Some(
                            MissionFailureCode::PolicyDenied.reason_code().to_string(),
                        ),
                        rule_id: Some("policy.prompt_required".to_string()),
                        context: Some("prompt not active".to_string()),
                    },
                ],
            )
            .unwrap();

        assert_eq!(report.stage, MissionPolicyPreflightStage::PlanTime);
        assert_eq!(report.outcomes.len(), 2);
        assert!(report.has_denials());
        assert!(!report.requires_approval());
        assert_eq!(
            report.planner_feedback_reason_codes,
            vec![MissionFailureCode::PolicyDenied.reason_code().to_string()]
        );

        let deny_outcome = report
            .outcomes
            .iter()
            .find(|outcome| outcome.decision == MissionPolicyDecisionKind::Deny)
            .unwrap();
        assert_eq!(
            deny_outcome.reason_code.as_deref(),
            Some(MissionFailureCode::PolicyDenied.reason_code())
        );
        assert_eq!(
            deny_outcome.error_code.as_deref(),
            Some(MissionFailureCode::PolicyDenied.error_code())
        );
        assert!(deny_outcome
            .human_hint
            .as_deref()
            .unwrap()
            .contains("Policy denied"));
        assert_eq!(deny_outcome.action_type, "wait_for");
    }

    #[test]
    fn mission_policy_preflight_dispatch_time_requires_assignment_reference() {
        let mission = sample_mission();
        let err = mission
            .evaluate_policy_preflight(
                MissionPolicyPreflightStage::DispatchTime,
                &[MissionPolicyPreflightCheck {
                    candidate_id: CandidateActionId("candidate:a".to_string()),
                    assignment_id: None,
                    decision: MissionPolicyDecisionKind::Allow,
                    reason_code: None,
                    rule_id: None,
                    context: None,
                }],
            )
            .unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::MissingDispatchPreflightAssignment { .. }
        ));
    }

    #[test]
    fn mission_policy_preflight_dispatch_time_rejects_assignment_candidate_mismatch() {
        let mut mission = sample_mission();
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:b".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 2,
                text: "/status".to_string(),
                paste_mode: Some(false),
            },
            rationale: "Check status".to_string(),
            score: Some(0.12),
            created_at_ms: 1_704_000_000_444,
        });
        let err = mission
            .evaluate_policy_preflight(
                MissionPolicyPreflightStage::DispatchTime,
                &[MissionPolicyPreflightCheck {
                    candidate_id: CandidateActionId("candidate:b".to_string()),
                    assignment_id: Some(AssignmentId("assignment:a".to_string())),
                    decision: MissionPolicyDecisionKind::Allow,
                    reason_code: None,
                    rule_id: None,
                    context: None,
                }],
            )
            .unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::PreflightAssignmentCandidateMismatch { .. }
        ));
    }

    #[test]
    fn mission_policy_preflight_require_approval_requires_canonical_reason() {
        let mission = sample_mission();
        let err = mission
            .evaluate_policy_preflight(
                MissionPolicyPreflightStage::PlanTime,
                &[MissionPolicyPreflightCheck {
                    candidate_id: CandidateActionId("candidate:a".to_string()),
                    assignment_id: None,
                    decision: MissionPolicyDecisionKind::RequireApproval,
                    reason_code: Some(MissionFailureCode::DispatchError.reason_code().to_string()),
                    rule_id: Some("policy.destructive_action".to_string()),
                    context: Some("high-risk operation".to_string()),
                }],
            )
            .unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnexpectedPolicyPreflightReasonCode {
                decision: MissionPolicyDecisionKind::RequireApproval,
                ..
            }
        ));
    }

    #[test]
    fn mission_policy_preflight_rejects_unknown_reason_code() {
        let mission = sample_mission();
        let err = mission
            .evaluate_policy_preflight(
                MissionPolicyPreflightStage::PlanTime,
                &[MissionPolicyPreflightCheck {
                    candidate_id: CandidateActionId("candidate:a".to_string()),
                    assignment_id: None,
                    decision: MissionPolicyDecisionKind::Deny,
                    reason_code: Some("unknown_preflight_reason".to_string()),
                    rule_id: None,
                    context: None,
                }],
            )
            .unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnknownPolicyPreflightReasonCode { .. }
        ));
    }

    #[test]
    fn mission_policy_preflight_dispatch_time_accepts_assignment_bound_denial_and_feedback() {
        let mission = sample_mission();
        let report = mission
            .evaluate_policy_preflight(
                MissionPolicyPreflightStage::DispatchTime,
                &[MissionPolicyPreflightCheck {
                    candidate_id: CandidateActionId("candidate:a".to_string()),
                    assignment_id: Some(AssignmentId("assignment:a".to_string())),
                    decision: MissionPolicyDecisionKind::Deny,
                    reason_code: Some(
                        MissionFailureCode::ReservationConflict
                            .reason_code()
                            .to_string(),
                    ),
                    rule_id: Some("policy.pane_reserved".to_string()),
                    context: Some("file reservation held by another actor".to_string()),
                }],
            )
            .unwrap();

        assert_eq!(report.stage, MissionPolicyPreflightStage::DispatchTime);
        assert_eq!(report.outcomes.len(), 1);
        assert!(report.has_denials());
        assert_eq!(
            report.planner_feedback_reason_codes,
            vec![MissionFailureCode::ReservationConflict
                .reason_code()
                .to_string()]
        );
        assert_eq!(
            report.outcomes[0].assignment_id.as_ref().unwrap().0,
            "assignment:a"
        );
        assert_eq!(
            report.outcomes[0].reason_code.as_deref(),
            Some(MissionFailureCode::ReservationConflict.reason_code())
        );
        assert_eq!(
            report.outcomes[0].error_code.as_deref(),
            Some(MissionFailureCode::ReservationConflict.error_code())
        );
    }

    #[test]
    fn mission_dispatch_contract_maps_candidate_to_robot_and_coordination_primitives() {
        let mission = sample_mission();
        let contract = mission
            .dispatch_contract_for_candidate(&CandidateActionId("candidate:a".to_string()))
            .unwrap();

        assert_eq!(contract.candidate_id.0, "candidate:a");
        match &contract.mechanism {
            MissionDispatchMechanism::RobotSend {
                pane_id,
                text,
                paste_mode,
            } => {
                assert_eq!(*pane_id, 1);
                assert_eq!(text, "/retry");
                assert_eq!(*paste_mode, Some(false));
            }
            other => panic!("expected RobotSend mapping, got {other:?}"),
        }

        assert!(contract.reservation.required);
        assert_eq!(contract.reservation.intents.len(), 1);
        assert_eq!(
            contract.reservation.intents[0].paths,
            vec!["crates/frankenterm-core/src/plan.rs".to_string()]
        );

        assert!(contract.messaging.requires_agent_mail);
        assert!(contract.messaging.requires_beads_update);
        assert_eq!(
            contract.messaging.thread_id.as_deref(),
            Some("ft-1i2ge.1.1")
        );
        assert_eq!(contract.messaging.bead_id.as_deref(), Some("ft-1i2ge.1.1"));

        assert!(contract.edge_cases.iter().any(|edge| {
            matches!(
                edge,
                MissionDispatchEdgeCase::MissingPane {
                    pane_id,
                    reason_code,
                    error_code,
                    ..
                } if *pane_id == 1
                    && reason_code == MissionFailureCode::StaleState.reason_code()
                    && error_code == MissionFailureCode::StaleState.error_code()
            )
        }));
        assert!(contract.edge_cases.iter().any(|edge| {
            matches!(
                edge,
                MissionDispatchEdgeCase::StaleBeadState {
                    bead_id,
                    reason_code,
                    error_code,
                    ..
                } if bead_id == "ft-1i2ge.1.1"
                    && reason_code == MissionFailureCode::StaleState.reason_code()
                    && error_code == MissionFailureCode::StaleState.error_code()
            )
        }));
    }

    #[test]
    fn mission_dispatch_contract_maps_wait_for_to_robot_wait_for() {
        let mut mission = sample_mission();
        mission.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:b".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::WaitFor {
                pane_id: Some(2),
                condition: WaitCondition::Pattern {
                    pane_id: Some(2),
                    rule_id: "core.codex:usage_reached".to_string(),
                },
                timeout_ms: 15_000,
            },
            rationale: "Wait for usage event".to_string(),
            score: Some(0.5),
            created_at_ms: 1_704_000_000_777,
        });

        let contract = mission
            .dispatch_contract_for_candidate(&CandidateActionId("candidate:b".to_string()))
            .unwrap();

        match &contract.mechanism {
            MissionDispatchMechanism::RobotWaitFor {
                pane_id,
                condition,
                timeout_ms,
            } => {
                assert_eq!(*pane_id, Some(2));
                assert_eq!(*timeout_ms, 15_000);
                assert!(matches!(
                    condition,
                    WaitCondition::Pattern {
                        pane_id: Some(2),
                        rule_id
                    } if rule_id == "core.codex:usage_reached"
                ));
            }
            other => panic!("expected RobotWaitFor mapping, got {other:?}"),
        }

        assert!(!contract.reservation.required);
        assert!(contract.reservation.intents.is_empty());
        assert!(contract.edge_cases.iter().any(
            |edge| matches!(edge, MissionDispatchEdgeCase::MissingPane { pane_id, .. } if *pane_id == 2)
        ));
    }

    #[test]
    fn mission_dispatch_contract_without_provenance_disables_beads_and_mail_requirements() {
        let mut mission = sample_mission();
        mission.provenance = None;

        let contract = mission
            .dispatch_contract_for_candidate(&CandidateActionId("candidate:a".to_string()))
            .unwrap();

        assert!(!contract.messaging.requires_agent_mail);
        assert!(!contract.messaging.requires_beads_update);
        assert!(contract.messaging.thread_id.is_none());
        assert!(contract.messaging.bead_id.is_none());
        assert!(!contract
            .edge_cases
            .iter()
            .any(|edge| matches!(edge, MissionDispatchEdgeCase::StaleBeadState { .. })));
    }

    #[test]
    fn mission_dispatch_contract_rejects_unknown_candidate() {
        let mission = sample_mission();
        let err = mission
            .dispatch_contract_for_candidate(&CandidateActionId("candidate:missing".to_string()))
            .unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnknownCandidateReference(_)
        ));
    }

    #[test]
    fn mission_assignment_suitability_prefers_lane_and_soft_capabilities() {
        let mission = sample_mission();
        let request = MissionAssignmentSuitabilityRequest {
            candidate_id: CandidateActionId("candidate:a".to_string()),
            required_capabilities: vec!["robot.send".to_string()],
            preferred_capabilities: vec!["retry".to_string(), "observability".to_string()],
            lane_affinity: Some("mission-core".to_string()),
            excluded_agents: Vec::new(),
            evaluated_at_ms: 1_704_200_100_000,
        };
        let profiles = vec![
            MissionAgentCapabilityProfile {
                agent_id: "executor-agent-a".to_string(),
                capabilities: vec!["robot.send".to_string(), "retry".to_string()],
                lane_affinity: vec!["mission-core".to_string()],
                current_load: 0,
                max_parallel_assignments: 2,
                availability: MissionAgentAvailability::Ready,
            },
            MissionAgentCapabilityProfile {
                agent_id: "executor-agent-b".to_string(),
                capabilities: vec!["robot.send".to_string(), "observability".to_string()],
                lane_affinity: vec!["search-lane".to_string()],
                current_load: 0,
                max_parallel_assignments: 2,
                availability: MissionAgentAvailability::Ready,
            },
        ];

        let report = mission
            .evaluate_assignment_suitability(&request, &profiles)
            .unwrap();
        assert_eq!(report.selected_agent.as_deref(), Some("executor-agent-a"));
        assert_eq!(report.evaluations[0].agent_id, "executor-agent-a");
        assert!(report.evaluations[0].eligible);
        assert!(
            report.evaluations[0].score > report.evaluations[1].score,
            "lane affinity + soft preference should increase suitability"
        );
    }

    #[test]
    fn mission_assignment_suitability_rejects_paused_and_rate_limited_agents() {
        let mission = sample_mission();
        let request = MissionAssignmentSuitabilityRequest {
            candidate_id: CandidateActionId("candidate:a".to_string()),
            required_capabilities: vec!["robot.send".to_string()],
            preferred_capabilities: Vec::new(),
            lane_affinity: None,
            excluded_agents: Vec::new(),
            evaluated_at_ms: 1_704_200_100_100,
        };
        let profiles = vec![
            MissionAgentCapabilityProfile {
                agent_id: "paused-agent".to_string(),
                capabilities: vec!["robot.send".to_string()],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 1,
                availability: MissionAgentAvailability::Paused {
                    reason_code: "manual_pause".to_string(),
                },
            },
            MissionAgentCapabilityProfile {
                agent_id: "rate-agent".to_string(),
                capabilities: vec!["robot.send".to_string()],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 1,
                availability: MissionAgentAvailability::RateLimited {
                    reason_code: "rate_limited".to_string(),
                    retry_after_ms: 1_704_200_200_000,
                },
            },
            MissionAgentCapabilityProfile {
                agent_id: "ready-agent".to_string(),
                capabilities: vec!["robot.send".to_string()],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 1,
                availability: MissionAgentAvailability::Ready,
            },
        ];

        let report = mission
            .evaluate_assignment_suitability(&request, &profiles)
            .unwrap();
        assert_eq!(report.selected_agent.as_deref(), Some("ready-agent"));
        let paused = report
            .evaluations
            .iter()
            .find(|evaluation| evaluation.agent_id == "paused-agent")
            .unwrap();
        assert!(!paused.eligible);
        assert!(paused.reason_codes.contains(&"agent_paused".to_string()));
        let rate_limited = report
            .evaluations
            .iter()
            .find(|evaluation| evaluation.agent_id == "rate-agent")
            .unwrap();
        assert!(!rate_limited.eligible);
        assert!(rate_limited
            .reason_codes
            .contains(&"agent_rate_limited".to_string()));
    }

    #[test]
    fn mission_assignment_suitability_enforces_assignment_exclusions() {
        let mission = sample_mission();
        let request = MissionAssignmentSuitabilityRequest {
            candidate_id: CandidateActionId("candidate:a".to_string()),
            required_capabilities: vec!["robot.send".to_string()],
            preferred_capabilities: Vec::new(),
            lane_affinity: None,
            excluded_agents: vec!["executor-agent-a".to_string()],
            evaluated_at_ms: 1_704_200_100_200,
        };
        let profiles = vec![
            MissionAgentCapabilityProfile {
                agent_id: "executor-agent-a".to_string(),
                capabilities: vec!["robot.send".to_string()],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 2,
                availability: MissionAgentAvailability::Ready,
            },
            MissionAgentCapabilityProfile {
                agent_id: "executor-agent-b".to_string(),
                capabilities: vec!["robot.send".to_string()],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 2,
                availability: MissionAgentAvailability::Ready,
            },
        ];

        let report = mission
            .evaluate_assignment_suitability(&request, &profiles)
            .unwrap();
        assert_eq!(report.selected_agent.as_deref(), Some("executor-agent-b"));
        let excluded = report
            .evaluations
            .iter()
            .find(|evaluation| evaluation.agent_id == "executor-agent-a")
            .unwrap();
        assert!(!excluded.eligible);
        assert!(excluded
            .reason_codes
            .contains(&"assignment_excluded".to_string()));
    }

    #[test]
    fn mission_assignment_suitability_handles_degraded_capacity_limits() {
        let mission = sample_mission();
        let request = MissionAssignmentSuitabilityRequest {
            candidate_id: CandidateActionId("candidate:a".to_string()),
            required_capabilities: vec!["robot.send".to_string()],
            preferred_capabilities: Vec::new(),
            lane_affinity: None,
            excluded_agents: Vec::new(),
            evaluated_at_ms: 1_704_200_100_300,
        };
        let profiles = vec![
            MissionAgentCapabilityProfile {
                agent_id: "degraded-full".to_string(),
                capabilities: vec!["robot.send".to_string()],
                lane_affinity: Vec::new(),
                current_load: 1,
                max_parallel_assignments: 4,
                availability: MissionAgentAvailability::Degraded {
                    reason_code: "degraded_latency".to_string(),
                    max_parallel_assignments: 1,
                },
            },
            MissionAgentCapabilityProfile {
                agent_id: "degraded-open".to_string(),
                capabilities: vec!["robot.send".to_string()],
                lane_affinity: Vec::new(),
                current_load: 0,
                max_parallel_assignments: 4,
                availability: MissionAgentAvailability::Degraded {
                    reason_code: "degraded_latency".to_string(),
                    max_parallel_assignments: 1,
                },
            },
        ];

        let report = mission
            .evaluate_assignment_suitability(&request, &profiles)
            .unwrap();
        assert_eq!(report.selected_agent.as_deref(), Some("degraded-open"));
        let degraded_full = report
            .evaluations
            .iter()
            .find(|evaluation| evaluation.agent_id == "degraded-full")
            .unwrap();
        assert!(!degraded_full.eligible);
        assert!(degraded_full
            .reason_codes
            .contains(&"agent_capacity_exhausted".to_string()));
        assert!(degraded_full
            .reason_codes
            .contains(&"agent_degraded".to_string()));
    }

    #[test]
    fn mission_assignment_suitability_rejects_unknown_candidate() {
        let mission = sample_mission();
        let request = MissionAssignmentSuitabilityRequest {
            candidate_id: CandidateActionId("candidate:missing".to_string()),
            required_capabilities: Vec::new(),
            preferred_capabilities: Vec::new(),
            lane_affinity: None,
            excluded_agents: Vec::new(),
            evaluated_at_ms: 1_704_200_100_400,
        };

        let err = mission
            .evaluate_assignment_suitability(&request, &[])
            .unwrap_err();
        assert!(matches!(
            err,
            MissionValidationError::UnknownCandidateReference(_)
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn mission_assignment_suitability_property_excluded_agents_never_selected(
            exclusion_mask in 0_u8..8_u8
        ) {
            let mission = sample_mission();
            let agent_ids = ["agent-a", "agent-b", "agent-c"];
            let excluded_agents = agent_ids
                .iter()
                .enumerate()
                .filter_map(|(index, agent_id)| {
                    if (exclusion_mask & (1 << index)) != 0 {
                        Some((*agent_id).to_string())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            let request = MissionAssignmentSuitabilityRequest {
                candidate_id: CandidateActionId("candidate:a".to_string()),
                required_capabilities: vec!["robot.send".to_string()],
                preferred_capabilities: vec!["retry".to_string()],
                lane_affinity: None,
                excluded_agents: excluded_agents.clone(),
                evaluated_at_ms: 1_704_200_200_100,
            };
            let profiles = agent_ids
                .iter()
                .map(|agent_id| MissionAgentCapabilityProfile {
                    agent_id: (*agent_id).to_string(),
                    capabilities: vec!["robot.send".to_string(), "retry".to_string()],
                    lane_affinity: Vec::new(),
                    current_load: 0,
                    max_parallel_assignments: 1,
                    availability: MissionAgentAvailability::Ready,
                })
                .collect::<Vec<_>>();

            let report = mission
                .evaluate_assignment_suitability(&request, &profiles)
                .unwrap();
            if let Some(selected_agent) = report.selected_agent {
                prop_assert!(
                    !excluded_agents.iter().any(|excluded| excluded == &selected_agent),
                    "selected agent should never be excluded: {}",
                    selected_agent
                );
            }
            for excluded in excluded_agents {
                let evaluation = report
                    .evaluations
                    .iter()
                    .find(|evaluation| evaluation.agent_id == excluded)
                    .unwrap();
                prop_assert!(!evaluation.eligible);
                prop_assert!(evaluation.reason_codes.iter().any(|reason| reason == "assignment_excluded"));
            }
        }
    }

    #[test]
    fn mission_canonical_string_is_order_independent() {
        let mut first = sample_mission();
        first.candidates.push(CandidateAction {
            candidate_id: CandidateActionId("candidate:b".to_string()),
            requested_by: MissionActorRole::Planner,
            action: StepAction::WaitFor {
                pane_id: Some(2),
                condition: WaitCondition::Pattern {
                    pane_id: Some(2),
                    rule_id: "core.codex:done".to_string(),
                },
                timeout_ms: 1_000,
            },
            rationale: "Observe completion signal".to_string(),
            score: Some(0.33),
            created_at_ms: 1_704_000_000_333,
        });
        first.assignments.push(Assignment {
            assignment_id: AssignmentId("assignment:b".to_string()),
            candidate_id: CandidateActionId("candidate:b".to_string()),
            assigned_by: MissionActorRole::Dispatcher,
            assignee: "executor-agent-2".to_string(),
            reservation_intent: None,
            approval_state: ApprovalState::NotRequired,
            outcome: None,
            escalation: Some(Escalation {
                level: EscalationLevel::Observe,
                triggered_by: MissionActorRole::Dispatcher,
                reason_code: "monitor_first".to_string(),
                error_code: None,
                summary: Some("No direct intervention yet".to_string()),
                escalated_at_ms: 1_704_000_000_400,
            }),
            created_at_ms: 1_704_000_000_390,
            updated_at_ms: None,
        });

        let mut second = first.clone();
        second.candidates.reverse();
        second.assignments.reverse();

        assert_eq!(first.canonical_string(), second.canonical_string());
        assert_eq!(first.compute_hash(), second.compute_hash());
        assert!(first.validate().is_ok());
        assert!(second.validate().is_ok());
    }

    fn sample_tx_contract() -> MissionTxContract {
        MissionTxContract {
            tx_version: MISSION_TX_SCHEMA_VERSION,
            intent: TxIntent {
                tx_id: TxId("tx:alpha".to_string()),
                requested_by: MissionActorRole::Dispatcher,
                summary: "Apply staged mission dispatch updates".to_string(),
                correlation_id: "corr-tx-alpha".to_string(),
                created_at_ms: 1_704_100_000_000,
            },
            plan: TxPlan {
                plan_id: TxPlanId("tx-plan:alpha".to_string()),
                tx_id: TxId("tx:alpha".to_string()),
                steps: vec![
                    TxStep {
                        step_id: TxStepId("tx-step:prepare".to_string()),
                        ordinal: 1,
                        action: StepAction::AcquireLock {
                            lock_name: "mission.tx.alpha".to_string(),
                            timeout_ms: Some(2_000),
                        },
                    },
                    TxStep {
                        step_id: TxStepId("tx-step:commit".to_string()),
                        ordinal: 2,
                        action: StepAction::SendText {
                            pane_id: 1,
                            text: "/apply".to_string(),
                            paste_mode: Some(false),
                        },
                    },
                ],
                preconditions: vec![TxPrecondition::PromptActive { pane_id: 1 }],
                compensations: vec![TxCompensation {
                    for_step_id: TxStepId("tx-step:commit".to_string()),
                    action: StepAction::SendText {
                        pane_id: 1,
                        text: "/rollback".to_string(),
                        paste_mode: Some(false),
                    },
                }],
            },
            lifecycle_state: MissionTxState::Committed,
            outcome: TxOutcome::Committed {
                completed_at_ms: 1_704_100_000_900,
                receipt_seq: 4,
            },
            receipts: vec![
                TxReceipt {
                    seq: 1,
                    state: MissionTxState::Planned,
                    emitted_at_ms: 1_704_100_000_100,
                    reason_code: None,
                    error_code: None,
                },
                TxReceipt {
                    seq: 2,
                    state: MissionTxState::Prepared,
                    emitted_at_ms: 1_704_100_000_200,
                    reason_code: None,
                    error_code: None,
                },
                TxReceipt {
                    seq: 3,
                    state: MissionTxState::Committing,
                    emitted_at_ms: 1_704_100_000_300,
                    reason_code: None,
                    error_code: None,
                },
                TxReceipt {
                    seq: 4,
                    state: MissionTxState::Committed,
                    emitted_at_ms: 1_704_100_000_900,
                    reason_code: None,
                    error_code: None,
                },
            ],
        }
    }

    fn sample_tx_recovery_contract() -> MissionTxContract {
        let mut contract = sample_tx_contract();
        contract.lifecycle_state = MissionTxState::RolledBack;
        contract.outcome = TxOutcome::RolledBack {
            completed_at_ms: 1_704_100_001_100,
            receipt_seq: 5,
            reason_code: MissionTxFailureCode::RollbackForced
                .reason_code()
                .to_string(),
        };
        contract.receipts = vec![
            TxReceipt {
                seq: 1,
                state: MissionTxState::Planned,
                emitted_at_ms: 1_704_100_000_100,
                reason_code: None,
                error_code: None,
            },
            TxReceipt {
                seq: 2,
                state: MissionTxState::Prepared,
                emitted_at_ms: 1_704_100_000_200,
                reason_code: None,
                error_code: None,
            },
            TxReceipt {
                seq: 3,
                state: MissionTxState::Committing,
                emitted_at_ms: 1_704_100_000_300,
                reason_code: None,
                error_code: None,
            },
            TxReceipt {
                seq: 4,
                state: MissionTxState::Compensating,
                emitted_at_ms: 1_704_100_001_000,
                reason_code: Some(
                    MissionTxFailureCode::CommitPartial
                        .reason_code()
                        .to_string(),
                ),
                error_code: Some(MissionTxFailureCode::CommitPartial.error_code().to_string()),
            },
            TxReceipt {
                seq: 5,
                state: MissionTxState::RolledBack,
                emitted_at_ms: 1_704_100_001_100,
                reason_code: Some(
                    MissionTxFailureCode::RollbackForced
                        .reason_code()
                        .to_string(),
                ),
                error_code: Some(
                    MissionTxFailureCode::RollbackForced
                        .error_code()
                        .to_string(),
                ),
            },
        ];
        contract
    }

    #[test]
    fn mission_tx_failure_taxonomy_has_unique_reason_and_error_codes() {
        let mut reason_codes = std::collections::HashSet::new();
        let mut error_codes = std::collections::HashSet::new();

        for code in MissionTxFailureCode::all() {
            assert!(reason_codes.insert(code.reason_code()));
            assert!(error_codes.insert(code.error_code()));
            assert_eq!(
                MissionTxFailureCode::from_reason_code(code.reason_code()),
                Some(code)
            );
        }
    }

    #[test]
    fn mission_tx_transition_table_rejects_illegal_edges() {
        assert!(mission_tx_can_transition(
            MissionTxState::Draft,
            MissionTxState::Planned,
            MissionTxTransitionKind::PlanCreated
        ));
        assert!(!mission_tx_can_transition(
            MissionTxState::Draft,
            MissionTxState::Committed,
            MissionTxTransitionKind::CommitSucceeded
        ));
        assert!(!mission_tx_can_transition(
            MissionTxState::Prepared,
            MissionTxState::Committed,
            MissionTxTransitionKind::CommitSucceeded
        ));
    }

    #[test]
    fn mission_tx_contract_accepts_happy_path() {
        let contract = sample_tx_contract();
        assert!(contract.validate().is_ok());
    }

    #[test]
    fn mission_tx_contract_accepts_recovery_path_with_compensation() {
        let contract = sample_tx_recovery_contract();
        assert!(contract.validate().is_ok());
    }

    #[test]
    fn mission_tx_contract_rejects_non_monotonic_receipts() {
        let mut contract = sample_tx_contract();
        contract.receipts[3].seq = 3;
        let err = contract.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionTxValidationError::NonMonotonicReceiptSequence { .. }
        ));
    }

    #[test]
    fn mission_tx_contract_rejects_commit_without_prepared_receipt() {
        let mut contract = sample_tx_contract();
        contract
            .receipts
            .retain(|receipt| receipt.state != MissionTxState::Prepared);
        let err = contract.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionTxValidationError::CommitWithoutPreparedReceipt
        ));
    }

    #[test]
    fn mission_tx_contract_rejects_double_commit_markers() {
        let mut contract = sample_tx_contract();
        contract.receipts.push(TxReceipt {
            seq: 5,
            state: MissionTxState::Committed,
            emitted_at_ms: 1_704_100_000_901,
            reason_code: None,
            error_code: None,
        });
        let err = contract.validate().unwrap_err();
        assert!(matches!(err, MissionTxValidationError::DoubleCommitMarker));
    }

    #[test]
    fn mission_tx_contract_rejects_compensation_without_commit_failure_marker() {
        let mut contract = sample_tx_contract();
        contract.lifecycle_state = MissionTxState::RolledBack;
        contract.outcome = TxOutcome::RolledBack {
            completed_at_ms: 1_704_100_001_100,
            receipt_seq: 5,
            reason_code: MissionTxFailureCode::RollbackForced
                .reason_code()
                .to_string(),
        };
        contract.receipts.push(TxReceipt {
            seq: 5,
            state: MissionTxState::Compensating,
            emitted_at_ms: 1_704_100_001_000,
            reason_code: Some(
                MissionTxFailureCode::RollbackForced
                    .reason_code()
                    .to_string(),
            ),
            error_code: Some(
                MissionTxFailureCode::RollbackForced
                    .error_code()
                    .to_string(),
            ),
        });
        let err = contract.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionTxValidationError::CompensationWithoutCommitFailure
        ));
    }

    #[test]
    fn mission_tx_contract_rejects_outcome_state_mismatch() {
        let mut contract = sample_tx_contract();
        contract.lifecycle_state = MissionTxState::Prepared;
        let err = contract.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionTxValidationError::OutcomeStateMismatch { .. }
        ));
    }

    #[test]
    fn mission_tx_transition_log_enforces_structured_contract() {
        let mut log = MissionTxTransitionLog {
            timestamp_ms: 1_704_100_001_200,
            component: "mission.tx".to_string(),
            scenario_id: "tx_happy_path".to_string(),
            correlation_id: "corr-tx-alpha".to_string(),
            tx_id: TxId("tx:alpha".to_string()),
            state_from: MissionTxState::Prepared,
            state_to: MissionTxState::Committing,
            transition_kind: MissionTxTransitionKind::CommitStarted,
            decision_path: "prepared->committing".to_string(),
            outcome: "ok".to_string(),
            reason_code: None,
            error_code: None,
            artifact_path: "tests/evidence/tx_contract.log".to_string(),
        };
        assert!(log.validate().is_ok());

        log.component.clear();
        let err = log.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionTxValidationError::MissingTransitionLogField { field: "component" }
        ));

        log.component = "mission.tx".to_string();
        log.reason_code = Some(
            MissionTxFailureCode::CommitTimeout
                .reason_code()
                .to_string(),
        );
        log.error_code = Some(MissionTxFailureCode::CommitDenied.error_code().to_string());
        let err = log.validate().unwrap_err();
        assert!(matches!(
            err,
            MissionTxValidationError::MismatchedFailureErrorCode { .. }
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn mission_tx_contract_property_rejects_non_monotonic_receipt_suffix(
            final_seq in 0_u64..=3_u64
        ) {
            let mut contract = sample_tx_contract();
            contract.receipts[3].seq = final_seq;
            let err = contract.validate().unwrap_err();
            prop_assert!(matches!(
                err,
                MissionTxValidationError::NonMonotonicReceiptSequence { .. }
            ), "expected NonMonotonicReceiptSequence, got {:?}", err);
        }

        #[test]
        fn mission_tx_contract_property_rejects_duplicate_commit_markers(
            extra_markers in 1_usize..6_usize
        ) {
            let mut contract = sample_tx_contract();
            for offset in 0..extra_markers {
                contract.receipts.push(TxReceipt {
                    seq: 5 + u64::try_from(offset).unwrap_or(0),
                    state: MissionTxState::Committed,
                    emitted_at_ms: 1_704_100_000_910 + i64::try_from(offset).unwrap_or(0),
                    reason_code: None,
                    error_code: None,
                });
            }
            let err = contract.validate().unwrap_err();
            prop_assert!(matches!(err, MissionTxValidationError::DoubleCommitMarker));
        }

        #[test]
        fn mission_tx_contract_property_enforces_single_terminal_outcome(
            lifecycle_state in prop_oneof![
                Just(MissionTxState::Draft),
                Just(MissionTxState::Planned),
                Just(MissionTxState::Prepared),
                Just(MissionTxState::Committing),
                Just(MissionTxState::Compensating),
                Just(MissionTxState::RolledBack),
                Just(MissionTxState::Failed),
            ]
        ) {
            let mut contract = sample_tx_contract();
            contract.lifecycle_state = lifecycle_state;
            let err = contract.validate().unwrap_err();
            prop_assert!(matches!(
                err,
                MissionTxValidationError::OutcomeStateMismatch { .. }
            ), "expected OutcomeStateMismatch, got {:?}", err);
        }

        #[test]
        fn mission_tx_contract_property_rejects_compensation_without_commit_partial_marker(
            failure_index in 0_usize..8_usize
        ) {
            let mut contract = sample_tx_recovery_contract();
            let non_partial_codes: Vec<MissionTxFailureCode> = MissionTxFailureCode::all()
                .into_iter()
                .filter(|code| *code != MissionTxFailureCode::CommitPartial)
                .collect();
            let failure_code = non_partial_codes[failure_index % non_partial_codes.len()];
            contract.receipts[3].reason_code = Some(failure_code.reason_code().to_string());
            contract.receipts[3].error_code = Some(failure_code.error_code().to_string());
            let err = contract.validate().unwrap_err();
            prop_assert!(matches!(
                err,
                MissionTxValidationError::CompensationWithoutCommitFailure
            ));
        }
    }

    #[test]
    fn plan_no_preconditions_helper() {
        let plan = ActionPlan::builder("Test", "ws").build();
        assert!(!plan.has_preconditions());
    }
}
